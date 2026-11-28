//! 接続先 (Linux) で SSH から起動される側。
//!
//! - 仮想マイク (null sink とその monitor source) を用意する。PipeWire で既定の録音デバイスにするときは、
//!   独立した source も作る（`DefaultMic`）
//! - 仮想マイクへの録音ストリームを監視し、有無の変化を START / STOP として client に伝える
//! - client から届いた PCM を仮想マイクへ流す
//!
//! PulseAudio とのやりとりは `pactl` / `pacat` の子プロセスで行う。libpulse にリンクしないので
//! ビルドに開発ヘッダが要らない。pipewire-pulse でも同じコマンドが使える。
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
    /// 既定のデバイスや sink の構成が変わったかもしれない。
    DefaultsChanged,
    PulseGone,
}

fn pactl(args: &[&str]) -> Result<String, String> {
    let out = Command::new("pactl")
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run pactl ({e}); install pulseaudio-utils"))?;
    if !out.status.success() {
        return Err(format!(
            "pactl {} failed: {}",
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

/// PipeWire で、既定の録音デバイスにするための独立した source。
///
/// PipeWire 1.6 系では、既定の録音デバイスを sink の monitor にしても、デバイスを指定しない録音は
/// 既定の出力先（捨て先）の monitor に繋がる。アプリの録音が仮想マイクに届かない。
/// そこで、monitor ではない source（`<sink>_mic`）を別に作って、そちらを既定にする。
/// 音は、録音セッションのあいだだけ、sink の monitor ポートからこの source の入力ポートへ
/// `pw-link` でつないで渡す。つなぎっぱなしにすると、どちらも休止できず、待機中も CPU を使う。
struct DefaultMic {
    name: String,
    index: u32,
    /// 自分で読み込んだ module。終了時に片付ける。
    module: Option<String>,
}

struct VirtualMic {
    sink: String,
    source_index: u32,
    /// PipeWire で `--set-default` のときだけ作る。
    default_mic: Option<DefaultMic>,
    /// 自分で読み込んだ module。終了時に片付ける。
    module: Option<String>,
    /// 差し替え前の既定 source。終了時に戻す。
    prev_default: Option<String>,
    /// 出力の捨て先として自分で読み込んだ module（`keep_out_of_default_sink`）。終了時に片付ける。
    discard_module: Option<String>,
    /// 捨て先を既定にする前の、既定の出力先。終了時に戻す。
    prev_default_sink: Option<String>,
}

/// 実体のある出力先が 1 つも無いときに、音声サーバーが代役として置く sink の名前
/// （PulseAudio の module-always-sink。pipewire-pulse も同じ名前を使う）。
const PLACEHOLDER_SINK: &str = "auto_null";

/// 既定の出力先。`Ok(None)` は「既定が無い」。確かめられなかった場合は `Err` で、区別する
/// （確かめられないのに書き換えると、実体のある出力先を既定から外してしまいかねない）。
fn default_sink() -> Result<Option<String>, String> {
    let name = pactl(&["get-default-sink"])?.trim().to_string();
    Ok(Some(name).filter(|s| !s.is_empty()))
}

/// `pactl info` の出力から、音声サーバーが PipeWire (pipewire-pulse) かどうかを見分ける。
/// pipewire-pulse は `Server Name: PulseAudio (on PipeWire 1.4.2)` のように名乗る。
fn is_pipewire(info: &str) -> bool {
    info.lines().any(|l| l.starts_with("Server Name:") && l.contains("PipeWire"))
}

/// `pw-link` を実行する。PipeWire に直接話しかける（pactl には、ポートをつなぐ手段が無い）。
fn pw_link(args: &[&str]) -> Result<(), String> {
    let out = Command::new("pw-link")
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run pw-link: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("pw-link {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()))
    }
}

impl VirtualMic {
    /// sink の monitor。名前を指定して録るアプリは、こちらを使う。
    fn monitor_name(&self) -> String {
        format!("{}.monitor", self.sink)
    }

    /// アプリに使ってもらう source の名前。既定にするための source を作ったときは、そちら。
    fn source_name(&self) -> String {
        match &self.default_mic {
            Some(d) => d.name.clone(),
            None => self.monitor_name(),
        }
    }

    /// 録音セッションのあいだだけ張るリンクの両端（sink の monitor ポート → 既定用の source の入力ポート）。
    /// 仮想マイクはモノラルで作るので、ポートの名前は決まっている。
    fn session_link(&self) -> Option<(String, String)> {
        self.default_mic.as_ref().map(|d| (format!("{}:monitor_MONO", self.sink), format!("{}:input_MONO", d.name)))
    }

    /// 録音セッションで張るリンクが、実際に張れることを起動時に確かめる。張れなければ、独立した source を
    /// やめて、monitor を既定にする動作に戻す。
    ///
    /// `pw-link` があっても、つなげるとは限らない。`pw-link` は `pactl` とは別の経路で音声サーバーに繋がるので、
    /// 片方だけが別の音声サーバーを指している環境では、ポートが見つからない。
    fn check_session_link(&mut self) {
        let Some((from, to)) = self.session_link() else { return };
        // 前の bridge が後片付けをせずに終わっていると、リンクが残っている。残っていると、仮想マイクが
        // 休止できず、待機中も CPU を使う。先に外す（無ければ失敗するが、構わない）
        let _ = pw_link(&["-d", "--", &from, &to]);
        // 作った直後は、ポートがまだ見えないことがある。少し待って試す
        let mut result = pw_link(&["--", &from, &to]);
        for _ in 0..10 {
            if result.is_ok() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
            result = pw_link(&["--", &from, &to]);
        }
        match result {
            // 張ったままにしない。録音のたびに張り直す
            Ok(()) => {
                let _ = pw_link(&["-d", "--", &from, &to]);
            }
            Err(e) => {
                log!("micmux bridge: cannot link the virtual mic to a separate source, using the sink monitor as the default device: {e}");
                if let Some(id) = self.default_mic.take().and_then(|d| d.module) {
                    let _ = pactl(&["unload-module", &id]);
                }
            }
        }
    }

    /// PipeWire で、既定の録音デバイスにするための source を作る。作れなければ `None`（monitor を既定にする）。
    fn create_default_mic(&self) -> Option<DefaultMic> {
        let name = format!("{}_mic", self.sink);
        let module = match find_index("sources", &name) {
            Ok(Some(_)) => None, // 前の bridge が残したもの。そのまま使う
            Ok(None) => {
                let loaded = pactl(&[
                    "load-module",
                    "module-null-sink",
                    "media.class=Audio/Source/Virtual",
                    &format!("sink_name={name}"),
                    "rate=16000",
                    "channels=1",
                    "format=s16le",
                    &format!("sink_properties=device.description={name}"),
                ]);
                match loaded {
                    Ok(id) => Some(id.trim().to_string()),
                    Err(e) => {
                        log!("micmux bridge: cannot create a separate source for the default device, using the sink monitor: {e}");
                        return None;
                    }
                }
            }
            Err(e) => {
                log!("micmux bridge: cannot look up sources, using the sink monitor as the default device: {e}");
                return None;
            }
        };
        match find_index("sources", &name) {
            Ok(Some(index)) => Some(DefaultMic { name, index, module }),
            other => {
                log!("micmux bridge: source '{name}' not found ({other:?}), using the sink monitor as the default device");
                if let Some(id) = module {
                    let _ = pactl(&["unload-module", &id]);
                }
                None
            }
        }
    }

    fn setup(opts: &Options) -> Result<VirtualMic, String> {
        let info = pactl(&["info"]).map_err(|e| {
            format!("cannot connect to the sound server; check that PulseAudio or pipewire-pulse is running ({e})")
        })?;
        let sink = opts.sink_name.clone();
        let mut mic = VirtualMic {
            sink: sink.clone(),
            source_index: 0,
            default_mic: None,
            module: None,
            prev_default: None,
            discard_module: None,
            prev_default_sink: None,
        };
        // 仮想マイクの sink を作る前に確かめる。作った後だと、既に鳴っている音が流れ込んでしまう
        match default_sink() {
            Ok(None) => mic.keep_out_of_default_sink(None),
            Ok(Some(d)) if d == PLACEHOLDER_SINK => mic.keep_out_of_default_sink(Some(d)),
            Ok(Some(_)) => {}
            Err(e) => log!(
                "micmux bridge: cannot check the default sink, leaving it unchanged; sounds played on this host may leak into the virtual mic: {e}"
            ),
        }
        let module = (|| -> Result<Option<String>, String> {
            if find_index("sinks", &sink)?.is_some() {
                log!("micmux bridge: using existing sink '{sink}'");
                return Ok(None);
            }
            let id = pactl(&[
                "load-module",
                "module-null-sink",
                &format!("sink_name={sink}"),
                "rate=16000",
                "channels=1",
                "format=s16le",
                &format!("sink_properties=device.description={sink}"),
            ])?;
            Ok(Some(id.trim().to_string()))
        })();
        mic.module = match module {
            Ok(m) => m,
            Err(e) => {
                mic.teardown(); // 先に作った捨て先を残さない
                return Err(e);
            }
        };
        // 上の確認をすり抜けた場合の保険（代役の sink の名前が違う音声サーバーなど）
        mic.guard_default_sink();
        match find_index("sources", &mic.monitor_name()) {
            Ok(Some(i)) => mic.source_index = i,
            other => {
                mic.teardown();
                return Err(format!("source '{}' not found ({other:?})", mic.monitor_name()));
            }
        }
        if opts.set_default {
            // 先に読む。独立した source を作ると、音声サーバーがそれを既定に選び直すことがある
            let prev = pactl(&["get-default-source"]).map(|s| s.trim().to_string()).ok();
            if is_pipewire(&info) {
                // pw-link が使えないなら、独立した source は作らない。作っても音を渡せず、既定から録るアプリに
                // 無音が届くだけになる。monitor を既定にする、これまでの動作にする
                match pw_link(&["--version"]) {
                    Ok(()) => mic.default_mic = mic.create_default_mic(),
                    Err(e) => log!("micmux bridge: pw-link is not usable, using the sink monitor as the default device: {e}"),
                }
                mic.check_session_link();
            }
            if prev.as_deref() != Some(&mic.source_name()) {
                if let Err(e) = pactl(&["set-default-source", &mic.source_name()]) {
                    log!("micmux bridge: cannot change the default source: {e}");
                } else {
                    mic.prev_default = prev;
                }
            }
        }
        Ok(mic)
    }

    /// 仮想マイクの sink が、既定の出力先にならないようにする。
    ///
    /// 仮想マイクは null sink とその monitor でできている。実体のある出力先が無い接続先
    /// （ヘッドレスのサーバー）では、この sink が唯一の出力先になり、音声サーバーが既定に選ぶ。
    /// すると、接続先で鳴った音がすべて仮想マイクに流れ込み、マイクの音としてアプリに届く。
    /// 出力の捨て先を別に用意し、そちらを既定にして防ぐ。
    /// 仮想マイクの sink が既定の出力先になっていたら、捨て先に替える。
    ///
    /// 起動時だけでなく、動いているあいだも呼ぶ。既定の出力先は後から変わる。たとえば、別の名前の
    /// 仮想マイクを使う bridge が先に終わって捨て先を消すと、残った sink はこちらの仮想マイクだけに
    /// なり、音声サーバーがそれを既定に選ぶ。
    fn guard_default_sink(&mut self) {
        // 確かめられなかったときは何もしない（setup でログを出している）
        if let Ok(Some(d)) = default_sink() {
            if d == self.sink {
                self.keep_out_of_default_sink(Some(d));
            }
        }
    }

    /// `prev` は、呼び出し側が確かめた、いまの既定の出力先。
    fn keep_out_of_default_sink(&mut self, prev: Option<String>) {
        let discard = format!("{}_discard", self.sink);
        if self.discard_module.is_none() && !matches!(find_index("sinks", &discard), Ok(Some(_))) {
            match pactl(&[
                "load-module",
                "module-null-sink",
                &format!("sink_name={discard}"),
                &format!("sink_properties=device.description={discard}"),
            ]) {
                Ok(id) => self.discard_module = Some(id.trim().to_string()),
                Err(e) => {
                    log!("micmux bridge: cannot create the discard sink; sounds played on this host may leak into the virtual mic: {e}");
                    return;
                }
            }
        }
        match pactl(&["set-default-sink", &discard]) {
            Ok(_) => {
                // 代役の sink と自分の sink は、戻す先にしない
                if self.prev_default_sink.is_none() {
                    self.prev_default_sink =
                        prev.filter(|p| p != PLACEHOLDER_SINK && *p != self.sink && *p != discard);
                }
                log!("micmux bridge: set the default sink to '{discard}' to keep other sounds out of the virtual mic");
            }
            Err(e) => log!("micmux bridge: cannot change the default sink; sounds played on this host may leak into the virtual mic: {e}"),
        }
    }

    fn active_recorders(&self) -> Result<usize, String> {
        let listing = pactl(&["list", "source-outputs"])?;
        let on_default = self.default_mic.as_ref().map_or(0, |d| count_active(&listing, d.index));
        Ok(count_active(&listing, self.source_index) + on_default)
    }

    fn teardown(&mut self) {
        if let Some(prev) = self.prev_default.take() {
            if !prev.is_empty() {
                let _ = pactl(&["set-default-source", &prev]);
            }
        }
        if let Some(id) = self.default_mic.take().and_then(|d| d.module) {
            let _ = pactl(&["unload-module", &id]);
        }
        if let Some(id) = self.module.take() {
            let _ = pactl(&["unload-module", &id]);
        }
        // 仮想マイクを消してから捨て先を消す。逆の順だと、そのあいだ仮想マイクが既定の出力先になる
        if let Some(prev) = self.prev_default_sink.take() {
            let _ = pactl(&["set-default-sink", &prev]);
        }
        if let Some(id) = self.discard_module.take() {
            let _ = pactl(&["unload-module", &id]);
        }
    }
}

/// 1 録音セッション分の、仮想マイクへの流し込み。
struct Playback {
    child: Child,
    stdin: Option<ChildStdin>,
    bytes: u64,
    /// このセッションのために張ったリンク（`VirtualMic::session_link`）。drop で外す。
    link: Option<(String, String)>,
}

impl Playback {
    fn start(mic: &VirtualMic, rate: u32, channels: u8, latency_ms: u32) -> io::Result<Playback> {
        let sink = &mic.sink;
        // 流し込む前につなぐ。先に流すと、冒頭が既定用の source に届かない
        let link = mic.session_link().filter(|(from, to)| match pw_link(&["--", from, to]) {
            Ok(()) => true,
            Err(e) => {
                log!("micmux bridge: cannot connect the virtual mic to the default source; apps recording from the default device get silence: {e}");
                false
            }
        });
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
        Ok(Playback { child, stdin, bytes: 0, link })
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
        // 張ったままにすると、仮想マイクが休止できない
        if let Some((from, to)) = self.link.take() {
            let _ = pw_link(&["-d", "--", &from, &to]);
        }
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
            // 既定のデバイスの変化は server のイベントとして届く。sink の増減は、それに先立って届く
            let event = if line.contains("source-output") || line.contains("on source #") {
                Event::PulseChanged
            } else if line.contains("on server") || line.contains("' on sink #") {
                Event::DefaultsChanged
            } else {
                continue;
            };
            if tx.send(event).is_err()
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
            log!("micmux bridge: cannot open {}, skipping the check for a previous bridge: {e}", path.display());
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
            log!("micmux bridge: stopping the previous bridge (pid {pid})");
            unsafe { libc::kill(pid, libc::SIGTERM) };
            termed = true;
        } else if pid > 0 && !killed && waited >= Duration::from_secs(3) {
            // 書き込みで止まっているなどで終われない。後片付けは残るが、壊されるよりよい
            log!("micmux bridge: previous bridge (pid {pid}) did not exit, killing it");
            unsafe { libc::kill(pid, libc::SIGKILL) };
            killed = true;
        } else if waited >= Duration::from_secs(6) {
            log!("micmux bridge: continuing without confirming that the previous bridge exited");
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
            let message = format!("protocol version mismatch (client {version}, bridge {VERSION})");
            let _ = send(&Msg::Error { message: message.clone() });
            return Err(message);
        }
        Ok(Event::Closed(e)) => return Err(format!("disconnected before HELLO ({e:?})")),
        Ok(_) => return Err("first message was not HELLO".into()),
        Err(_) => return Err("no HELLO received".into()),
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
            return Err(format!("cannot start pactl subscribe: {e}"));
        }
    };

    let result = serve(&opts, &mut mic, &rx);

    let _ = watcher.kill();
    let _ = watcher.wait();
    mic.teardown();
    result
}

/// 録音中の pacat (`playback`) はこの関数のローカルなので、どの経路で抜けても drop で止まる。
fn serve(opts: &Options, mic: &mut VirtualMic, rx: &Receiver<Event>) -> Result<(), String> {
    let io_err = |e: io::Error| format!("failed to send to client: {e}");
    send(&Msg::HelloAck { version: VERSION, source: mic.source_name() }).map_err(io_err)?;
    log!("micmux bridge: virtual mic '{}' is ready", mic.source_name());

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
                    log!("micmux bridge: recording requested → START (session {next_session})");
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
                        "micmux bridge: recording ended → STOP (session {session}, {:.1}s, received {bytes} bytes)",
                        started.elapsed().as_secs_f32()
                    );
                    send(&Msg::Stop { session }).map_err(io_err)?;
                }
                continue;
            }
        }
        if TERMINATE.load(Ordering::SeqCst) {
            log!("micmux bridge: received a termination signal");
            return Ok(());
        }
        if now.duration_since(last_rx) >= opts.idle_timeout {
            log!(
                "micmux bridge: nothing received from client for {} minutes, exiting",
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
            Event::DefaultsChanged => mic.guard_default_sink(),
            Event::PulseGone => {
                let message = "lost connection to the sound server".to_string();
                let _ = send(&Msg::Error { message: message.clone() });
                return Err(message);
            }
            Event::Closed(None) => {
                log!("micmux bridge: client disconnected");
                return Ok(());
            }
            Event::Closed(Some(e)) => return Err(format!("failed to receive from client: {e}")),
            Event::Msg(msg) => match msg {
                Msg::AudioBegin { session, rate, channels } => {
                    if active.map(|a| a.0) != Some(session) {
                        continue; // 終わったセッション宛て
                    }
                    if !(8_000..=192_000).contains(&rate) || !(1..=2).contains(&channels) {
                        log!("micmux bridge: unsupported audio format ({rate} Hz, {channels} ch)");
                        continue;
                    }
                    playback = None; // 前の pacat を止めてから起動し直す
                    match Playback::start(mic, rate, channels, opts.latency_ms) {
                        Ok(p) => playback = Some(p),
                        Err(e) => log!("micmux bridge: cannot start pacat: {e}"),
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
                                "micmux bridge: session {session}: first audio {} ms after START",
                                started.elapsed().as_millis()
                            );
                        }
                        if let Err(e) = p.write(&pcm) {
                            log!("micmux bridge: failed to write to the virtual mic: {e}");
                            playback = None;
                        }
                    }
                }
                Msg::AudioEnd { session, reason } => {
                    if active.map(|a| a.0) == Some(session) {
                        playback = None;
                        if reason != end_reason::STOPPED {
                            log!("micmux bridge: session {session}: client is not sending audio (reason {reason})");
                        }
                    }
                }
                Msg::Ping { nonce } => send(&Msg::Pong { nonce }).map_err(io_err)?,
                Msg::Error { message } => log!("micmux bridge: error from client: {message}"),
                other => log!("micmux bridge: unexpected message: {other:?}"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{count_active, is_pipewire};

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
    fn pipewire_を名乗る音声サーバーを見分ける() {
        assert!(is_pipewire("Server String: /run/user/1000/pulse/native\nServer Name: PulseAudio (on PipeWire 1.4.2)\n"));
        assert!(!is_pipewire("Server String: /run/user/1000/pulse/native\nServer Name: pulseaudio\n"));
        // ほかの行に出てくるだけでは、PipeWire とみなさない
        assert!(!is_pipewire("Server Name: pulseaudio\nDefault Sink: PipeWire-out\n"));
        assert!(!is_pipewire(""));
    }

    #[test]
    fn counts_only_uncorked_streams_on_our_source() {
        assert_eq!(count_active(LISTING, 0), 1);
        assert_eq!(count_active(LISTING, 2), 1);
        assert_eq!(count_active(LISTING, 7), 0);
        assert_eq!(count_active("", 0), 0);
    }
}
