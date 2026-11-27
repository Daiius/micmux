//! 接続先 (Linux) で SSH から起動される側。
//!
//! - 仮想マイク (null sink とその monitor source) を用意する
//! - 仮想マイクへの録音ストリームを監視し、有無の変化を START / STOP として client に伝える
//! - client から届いた PCM を仮想マイクへ流す
//!
//! PulseAudio とのやりとりは `pactl` / `pacat` の子プロセスで行う。libpulse にリンクしないので
//! ビルドに開発ヘッダが要らない。pipewire-pulse でも同じコマンドが使える（動作は未確認）。
//!
//! stdout はプロトコル専用。ログはすべて stderr に出す。

use crate::protocol::{self, end_reason, write_msg, Msg, VERSION};
use std::io::{self, BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

/// stderr へのログ。接続が切れると stderr も閉じるが、そこで panic すると後片付けを
/// 通らずに仮想マイクが残ってしまうので、書き込みの失敗は無視する（`eprintln!` は使わない）。
macro_rules! log {
    ($($arg:tt)*) => {{
        let _ = writeln!(io::stderr(), $($arg)*);
    }};
}

pub struct Options {
    pub sink_name: String,
    /// 仮想マイクを既定の録音デバイスにする（終了時に戻す）。
    pub set_default: bool,
    /// 録音ストリームが消えてから STOP を送るまでの猶予。
    /// アプリが「試し開き → 本番」と短時間で開き直すのを 1 セッションにまとめる。
    pub stop_holdoff: Duration,
    /// 仮想マイクへ流し込むストリームのバッファ長（ジッター吸収）。
    pub latency_ms: u32,
    /// client から何も届かないまま、これだけ経ったら後片付けをして終わる。
    /// 接続元がスリープやネットワークの切り替えで消えると TCP が半開きのまま残り、
    /// stdin の EOF が sshd の設定次第で何時間も来ないため。client は 10 秒ごとに Ping を送る。
    pub idle_timeout: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            sink_name: "micmux".into(),
            set_default: false,
            stop_holdoff: Duration::from_millis(300),
            latency_ms: 60,
            // 短くしすぎない。接続元が数分スリープしただけなら TCP は生きていることが多く、
            // 起きた client はそのまま使い続けられる。その前に bridge が終わってしまうと、
            // 端末の SSH は生きているのに音声だけ使えなくなる
            idle_timeout: Duration::from_secs(30 * 60),
        }
    }
}

enum Event {
    Msg(Msg),
    /// client 側が閉じた、または壊れた。
    Closed(Option<io::Error>),
    PulseChanged,
    PulseGone,
}

fn pactl(args: &[&str]) -> Result<String, String> {
    let out = Command::new("pactl")
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("pactl を実行できません ({e})。pulseaudio-utils を入れてください"))?;
    if !out.status.success() {
        return Err(format!(
            "pactl {} が失敗: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `pactl list short sinks|sources` から名前で index を引く。
fn find_index(kind: &str, name: &str) -> Result<Option<u32>, String> {
    Ok(pactl(&["list", "short", kind])?.lines().find_map(|l| {
        let mut f = l.split('\t');
        let idx = f.next()?.parse().ok()?;
        (f.next()? == name).then_some(idx)
    }))
}

/// `pactl list source-outputs` の出力から、`source` に繋がっていて cork されていない
/// 録音ストリームを数える。
pub fn count_active(listing: &str, source: u32) -> usize {
    let mut count = 0;
    let (mut on_source, mut corked, mut in_block) = (false, false, false);
    let mut flush = |in_block: bool, on_source: bool, corked: bool| {
        if in_block && on_source && !corked {
            count += 1;
        }
    };
    for line in listing.lines() {
        if line.starts_with("Source Output #") {
            flush(in_block, on_source, corked);
            (in_block, on_source, corked) = (true, false, false);
            continue;
        }
        // 項目はタブ 1 つ、プロパティはタブ 2 つで字下げされる。プロパティは見ない
        let Some(t) = line.strip_prefix('\t').filter(|t| !t.starts_with('\t')) else { continue };
        if let Some(v) = t.strip_prefix("Source: ") {
            on_source = v.trim().parse() == Ok(source);
        } else if let Some(v) = t.strip_prefix("Corked: ") {
            corked = v.trim() == "yes";
        }
    }
    flush(in_block, on_source, corked);
    count
}

struct VirtualMic {
    sink: String,
    source_index: u32,
    /// 自分で読み込んだ module。終了時に片付ける。
    module: Option<String>,
    /// 差し替え前の既定 source。終了時に戻す。
    prev_default: Option<String>,
}

impl VirtualMic {
    fn source_name(&self) -> String {
        format!("{}.monitor", self.sink)
    }

    fn setup(opts: &Options) -> Result<VirtualMic, String> {
        pactl(&["info"]).map_err(|e| {
            format!("サウンドサーバーに接続できません。PulseAudio か pipewire-pulse が動いているか確認してください ({e})")
        })?;
        let sink = opts.sink_name.clone();
        let module = if find_index("sinks", &sink)?.is_some() {
            log!("micmux bridge: 既存の sink '{sink}' を使います");
            None
        } else {
            let id = pactl(&[
                "load-module",
                "module-null-sink",
                &format!("sink_name={sink}"),
                "rate=16000",
                "channels=1",
                "format=s16le",
                &format!("sink_properties=device.description={sink}"),
            ])?;
            Some(id.trim().to_string())
        };
        let mut mic = VirtualMic { sink, source_index: 0, module, prev_default: None };
        match find_index("sources", &mic.source_name()) {
            Ok(Some(i)) => mic.source_index = i,
            other => {
                mic.teardown();
                return Err(format!("source '{}' が見つかりません ({other:?})", mic.source_name()));
            }
        }
        if opts.set_default {
            let prev = pactl(&["get-default-source"]).map(|s| s.trim().to_string()).ok();
            if prev.as_deref() != Some(&mic.source_name()) {
                if let Err(e) = pactl(&["set-default-source", &mic.source_name()]) {
                    log!("micmux bridge: 既定 source を変更できません: {e}");
                } else {
                    mic.prev_default = prev;
                }
            }
        }
        Ok(mic)
    }

    fn active_recorders(&self) -> Result<usize, String> {
        Ok(count_active(&pactl(&["list", "source-outputs"])?, self.source_index))
    }

    fn teardown(&mut self) {
        if let Some(prev) = self.prev_default.take() {
            if !prev.is_empty() {
                let _ = pactl(&["set-default-source", &prev]);
            }
        }
        if let Some(id) = self.module.take() {
            let _ = pactl(&["unload-module", &id]);
        }
    }
}

/// 1 録音セッション分の、仮想マイクへの流し込み。
struct Playback {
    child: Child,
    stdin: Option<ChildStdin>,
    bytes: u64,
}

impl Playback {
    fn start(sink: &str, rate: u32, channels: u8, latency_ms: u32) -> io::Result<Playback> {
        let mut child = Command::new("pacat")
            .args([
                "--playback",
                "--raw",
                "--format=s16le",
                &format!("--device={sink}"),
                &format!("--rate={rate}"),
                &format!("--channels={channels}"),
                &format!("--latency-msec={latency_ms}"),
                "--client-name=micmux",
                "--stream-name=micmux forwarded microphone",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?;
        let stdin = child.stdin.take();
        Ok(Playback { child, stdin, bytes: 0 })
    }

    fn write(&mut self, pcm: &[u8]) -> io::Result<()> {
        self.bytes += pcm.len() as u64;
        match self.stdin.as_mut() {
            Some(s) => s.write_all(pcm),
            None => Ok(()),
        }
    }
}

/// drop したら、残りを捨てて即座に止める。録音側はもう閉じているので、流し切っても誰も聞いていない。
/// エラーで serve を抜けた場合にも pacat を残さないよう、止めるのは drop に任せる。
impl Drop for Playback {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_reader(tx: SyncSender<Event>) {
    thread::spawn(move || {
        let closed = protocol::forward(&mut io::stdin().lock(), |m| tx.send(Event::Msg(m)).is_ok());
        let _ = tx.send(Event::Closed(closed));
    });
}

fn spawn_watcher(tx: SyncSender<Event>) -> io::Result<Child> {
    let mut child = Command::new("pactl")
        .arg("subscribe")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let out = child.stdout.take().expect("piped stdout");
    thread::spawn(move || {
        for line in BufReader::new(out).lines() {
            let Ok(line) = line else { break };
            // 録音ストリームの増減・cork の変化、および source 自体の作り直しを拾う
            if (line.contains("source-output") || line.contains("on source #"))
                && tx.send(Event::PulseChanged).is_err()
            {
                return;
            }
        }
        let _ = tx.send(Event::PulseGone);
    });
    Ok(child)
}

/// SIGTERM / SIGINT を受けたら立つ。メインループが見て、後片付けをしてから終わる。
static TERMINATE: AtomicBool = AtomicBool::new(false);

extern "C" fn on_terminate(_: libc::c_int) {
    TERMINATE.store(true, Ordering::SeqCst);
}

/// シグナルで即死すると仮想マイクと既定 source の変更が残ってしまうので、必ず後片付けを通す。
fn install_signal_handlers() {
    let handler = on_terminate as extern "C" fn(libc::c_int) as libc::sighandler_t;
    unsafe {
        // 接続が切れたときは stdin の EOF で気づけるので、SIGHUP では死なない
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGINT, handler);
    }
}

/// 同じ仮想マイクを使う bridge を 1 つに保つ。前の bridge が残っていれば終わらせ、
/// その後片付けが済むまで待つ。返ったファイルを閉じるまで（＝プロセスが終わるまで）排他が続く。
///
/// 前の bridge は、接続元が消えても無通信の検出まで残る。残ったまま新しい bridge が
/// 同じ sink を使い始めると、前の bridge が終わるときに module を unload して既定 source を戻し、
/// 新しいセッションの仮想マイクを壊してしまう。後から来た方を優先する。
///
/// 排他には fcntl のレコードロックを使う。持ち主の pid を OS から引けるので、
/// pid ファイルのように無関係なプロセスを止めてしまうことがない。
fn take_over(sink: &str) -> Option<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;
    let path = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => std::path::PathBuf::from(dir).join(format!("micmux-bridge-{sink}.lock")),
        None => format!("/tmp/micmux-bridge-{}-{sink}.lock", unsafe { libc::getuid() }).into(),
    };
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) => {
            log!("micmux bridge: {} を開けないので、前の bridge の確認を省きます: {e}", path.display());
            return None;
        }
    };
    let lock = |cmd: libc::c_int| -> (bool, libc::flock) {
        let mut fl: libc::flock = unsafe { std::mem::zeroed() };
        fl.l_type = libc::F_WRLCK as _;
        fl.l_whence = libc::SEEK_SET as _;
        let ok = unsafe { libc::fcntl(file.as_raw_fd(), cmd, &mut fl) } == 0;
        (ok, fl)
    };
    let started = Instant::now();
    let (mut termed, mut killed) = (false, false);
    loop {
        if lock(libc::F_SETLK).0 {
            return Some(file);
        }
        let (ok, fl) = lock(libc::F_GETLK);
        let pid = if ok && fl.l_type != libc::F_UNLCK as _ { fl.l_pid } else { 0 };
        let waited = started.elapsed();
        if pid > 0 && !termed {
            log!("micmux bridge: 前の bridge (pid {pid}) を終わらせます");
            unsafe { libc::kill(pid, libc::SIGTERM) };
            termed = true;
        } else if pid > 0 && !killed && waited >= Duration::from_secs(3) {
            // 書き込みで止まっているなどで終われない。後片付けは残るが、壊されるよりよい
            log!("micmux bridge: 前の bridge (pid {pid}) が終わらないので強制終了します");
            unsafe { libc::kill(pid, libc::SIGKILL) };
            killed = true;
        } else if waited >= Duration::from_secs(6) {
            log!("micmux bridge: 前の bridge の終了を確認できないまま続けます");
            return None;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn send(msg: &Msg) -> io::Result<()> {
    write_msg(&mut io::stdout().lock(), msg)
}

pub fn run(opts: Options) -> Result<(), String> {
    install_signal_handlers();
    let (tx, rx) = sync_channel::<Event>(256);
    spawn_reader(tx.clone());

    // HELLO を待ってから音声基盤に触る
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Event::Msg(Msg::Hello { version })) if version == VERSION => {}
        Ok(Event::Msg(Msg::Hello { version })) => {
            let message = format!("プロトコルのバージョンが違います (client {version}, bridge {VERSION})");
            let _ = send(&Msg::Error { message: message.clone() });
            return Err(message);
        }
        Ok(Event::Closed(e)) => return Err(format!("HELLO の前に切断されました ({e:?})")),
        Ok(_) => return Err("最初のメッセージが HELLO ではありません".into()),
        Err(_) => return Err("HELLO を受信できませんでした".into()),
    }

    let _exclusive = take_over(&opts.sink_name);
    let mut mic = match VirtualMic::setup(&opts) {
        Ok(m) => m,
        Err(e) => {
            let _ = send(&Msg::Error { message: e.clone() });
            return Err(e);
        }
    };
    let mut watcher = match spawn_watcher(tx.clone()) {
        Ok(w) => w,
        Err(e) => {
            mic.teardown();
            return Err(format!("pactl subscribe を起動できません: {e}"));
        }
    };

    let result = serve(&opts, &mic, &rx);

    let _ = watcher.kill();
    let _ = watcher.wait();
    mic.teardown();
    result
}

/// 録音中の pacat (`playback`) はこの関数のローカルなので、どの経路で抜けても drop で止まる。
fn serve(opts: &Options, mic: &VirtualMic, rx: &Receiver<Event>) -> Result<(), String> {
    let io_err = |e: io::Error| format!("client への送信に失敗: {e}");
    send(&Msg::HelloAck { version: VERSION, source: mic.source_name() }).map_err(io_err)?;
    log!("micmux bridge: 仮想マイク '{}' を用意しました", mic.source_name());

    let mut next_session: u32 = 0;
    // 現在の録音セッションと、その START を送った時刻
    let mut active: Option<(u32, Instant)> = None;
    let mut stop_at: Option<Instant> = None;
    let mut playback: Option<Playback> = None;
    let mut first_audio_logged = false;
    // イベントを取りこぼした場合の保険として、ときどき状態を取り直す
    let resync_every = Duration::from_secs(30);
    let mut next_resync = Instant::now();
    let mut check_demand = true; // 起動時点で既に録音中のアプリがあるかもしれない
    // client から最後に何かが届いた時刻。client は待機中も Ping を送り続ける
    let mut last_rx = Instant::now();

    loop {
        if check_demand {
            check_demand = false;
            next_resync = Instant::now() + resync_every;
            let n = mic.active_recorders()?;
            if n > 0 {
                stop_at = None;
                if active.is_none() {
                    next_session = next_session.wrapping_add(1);
                    active = Some((next_session, Instant::now()));
                    first_audio_logged = false;
                    log!("micmux bridge: 録音要求 → START (session {next_session})");
                    send(&Msg::Start { session: next_session }).map_err(io_err)?;
                }
            } else if active.is_some() && stop_at.is_none() {
                stop_at = Some(Instant::now() + opts.stop_holdoff);
            }
        }

        let now = Instant::now();
        if let Some(t) = stop_at {
            if now >= t {
                stop_at = None;
                if let Some((session, started)) = active.take() {
                    let bytes = playback.take().map_or(0, |p| p.bytes);
                    log!(
                        "micmux bridge: 録音終了 → STOP (session {session}, {:.1}s, 受信 {bytes} bytes)",
                        started.elapsed().as_secs_f32()
                    );
                    send(&Msg::Stop { session }).map_err(io_err)?;
                }
                continue;
            }
        }
        if TERMINATE.load(Ordering::SeqCst) {
            log!("micmux bridge: 終了シグナルを受けました");
            return Ok(());
        }
        if now.duration_since(last_rx) >= opts.idle_timeout {
            log!(
                "micmux bridge: client から {} 分間何も届かないので終わります",
                opts.idle_timeout.as_secs() / 60
            );
            return Ok(());
        }
        // シグナルと無通信に気づけるよう、長くても 1 秒で起きる
        let deadline = stop_at.map_or(next_resync, |t| t.min(next_resync)).min(now + Duration::from_secs(1));
        let ev = match rx.recv_timeout(deadline.saturating_duration_since(now)) {
            Ok(ev) => ev,
            Err(RecvTimeoutError::Timeout) => {
                if Instant::now() >= next_resync {
                    check_demand = true;
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        };

        if matches!(ev, Event::Msg(_)) {
            last_rx = Instant::now();
        }
        match ev {
            Event::PulseChanged => check_demand = true,
            Event::PulseGone => {
                let message = "サウンドサーバーとの接続が切れました".to_string();
                let _ = send(&Msg::Error { message: message.clone() });
                return Err(message);
            }
            Event::Closed(None) => {
                log!("micmux bridge: client が切断しました");
                return Ok(());
            }
            Event::Closed(Some(e)) => return Err(format!("client からの受信に失敗: {e}")),
            Event::Msg(msg) => match msg {
                Msg::AudioBegin { session, rate, channels } => {
                    if active.map(|a| a.0) != Some(session) {
                        continue; // 終わったセッション宛て
                    }
                    if !(8_000..=192_000).contains(&rate) || !(1..=2).contains(&channels) {
                        log!("micmux bridge: 扱えない音声形式 ({rate} Hz, {channels} ch)");
                        continue;
                    }
                    playback = None; // 前の pacat を止めてから起動し直す
                    match Playback::start(&mic.sink, rate, channels, opts.latency_ms) {
                        Ok(p) => playback = Some(p),
                        Err(e) => log!("micmux bridge: pacat を起動できません: {e}"),
                    }
                }
                Msg::Audio { session, pcm } => {
                    let Some((cur, started)) = active else { continue };
                    if cur != session {
                        continue; // 前のセッションの残りを次のセッションに混ぜない
                    }
                    if let Some(p) = playback.as_mut() {
                        if !first_audio_logged {
                            first_audio_logged = true;
                            log!(
                                "micmux bridge: session {session}: START から最初の音声まで {} ms",
                                started.elapsed().as_millis()
                            );
                        }
                        if let Err(e) = p.write(&pcm) {
                            log!("micmux bridge: 仮想マイクへの書き込みに失敗: {e}");
                            playback = None;
                        }
                    }
                }
                Msg::AudioEnd { session, reason } => {
                    if active.map(|a| a.0) == Some(session) {
                        playback = None;
                        if reason != end_reason::STOPPED {
                            log!("micmux bridge: session {session}: client が音声を送りません (reason {reason})");
                        }
                    }
                }
                Msg::Ping { nonce } => send(&Msg::Pong { nonce }).map_err(io_err)?,
                Msg::Error { message } => log!("micmux bridge: client からのエラー: {message}"),
                other => log!("micmux bridge: 想定外のメッセージ: {other:?}"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::count_active;

    const LISTING: &str = "\
Source Output #3
\tDriver: protocol-native.c
\tOwner Module: 1
\tClient: 9
\tSource: 0
\tSample Specification: float32le 1ch 48000Hz
\tCorked: no
\tProperties:
\t\tapplication.name = \"ALSA plug-in [2.1.332]\"
\t\tSource: 5 looks like a field but is a property

Source Output #4
\tSource: 0
\tCorked: yes

Source Output #5
\tSource: 2
\tCorked: no
";

    #[test]
    fn counts_only_uncorked_streams_on_our_source() {
        assert_eq!(count_active(LISTING, 0), 1);
        assert_eq!(count_active(LISTING, 2), 1);
        assert_eq!(count_active(LISTING, 7), 0);
        assert_eq!(count_active("", 0), 0);
    }
}
