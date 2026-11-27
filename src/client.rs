//! 接続元 (macOS など) で動かす側。
//!
//! SSH を子プロセスとして起動し、その標準入出力で bridge と話す。
//! 録音するかどうかを決めるのは常にこちら側:
//!
//! ```text
//! recording = armed && connected && requested
//! ```
//!
//! bridge からの START は「要求」でしかなく、armed でなければ断る。
//!
//! 操作用ソケットと起動の排他は `control`、ssh の組み立ては `ssh` にある。

use crate::capture::{Capture, Source, Writer};
use crate::control::{self, Cmd};
use crate::protocol::{self, end_reason, write_msg, Msg, VERSION};
use crate::ssh;
use std::io::{self, BufRead};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub struct Options {
    pub host: String,
    /// SSH の代わりに使う接続コマンド（`sh -c` で実行。docker exec などにも使える）。
    pub transport: Option<String>,
    /// 接続先での micmux の呼び出し方。
    pub remote_cmd: String,
    pub ssh_args: Vec<String>,
    pub bridge_args: Vec<String>,
    pub armed: bool,
    /// 連続録音時間の上限。0 なら上限なし。
    pub max_seconds: u64,
    pub source: Source,
    pub bell: bool,
    /// 端末から切り離して裏で動く（ssh の LocalCommand から起動する用）。
    pub background: bool,
    /// このプロセスが生きているあいだだけ動く。裏で動かすときの終了条件。
    pub watch_pid: Option<i32>,
    pub log_path: Option<PathBuf>,
    /// 端末用の SSH 接続に相乗りする（SSH の多重化）。認証が 1 回で済む。
    pub mux: bool,
}

pub enum Event {
    Msg(Msg),
    Closed(Option<io::Error>),
    Control(Cmd, Option<SyncSender<String>>),
    /// 別の ssh からの「自分が生きているあいだも動いていてほしい」という登録。
    Watch { pid: i32, host: String, reply: SyncSender<String> },
    /// 取得スレッドが自分から終わった。番号は取得の世代番号。
    CaptureExit(u64, Result<(), String>),
}

fn alive(pid: i32) -> bool {
    // シグナル 0 は存在確認だけ。権限が無くて失敗した場合もプロセスは存在している
    unsafe { libc::kill(pid, 0) == 0 || *errno_location() == libc::EPERM }
}

#[cfg(target_os = "macos")]
unsafe fn errno_location() -> *mut libc::c_int {
    libc::__error()
}

#[cfg(not(target_os = "macos"))]
unsafe fn errno_location() -> *mut libc::c_int {
    libc::__errno_location()
}

/// `cmd` を新しいセッションで起動するようにする（端末から切り離す）。
pub fn in_new_session(cmd: &mut Command) -> &mut Command {
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        })
    }
}

/// 切り離されたあとの自分であることを示す環境変数。
const DETACHED_ENV: &str = "MICMUX_DETACHED";

/// 端末から切り離す。自分自身を新しいセッションで起動し直し、親はすぐに戻る
/// （ssh の LocalCommand を待たせない）。戻り値が false なら親側で、そのまま終了してよい。
///
/// fork するだけでは済ませない。macOS では fork しただけの子プロセスから Core Audio などの
/// システムフレームワークを使うと動作が保証されないので、必ず exec し直す。
fn detach(log: &Path) -> Result<bool, String> {
    if std::env::var_os(DETACHED_ENV).is_some() {
        return Ok(true);
    }
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let open = || {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .map_err(|e| format!("{}: {e}", log.display()))
    };
    let exe = std::env::current_exe().map_err(|e| format!("自分の実行ファイルの場所がわかりません: {e}"))?;
    let mut cmd = Command::new(exe);
    cmd.args(std::env::args_os().skip(1))
        .env(DETACHED_ENV, "1")
        .stdin(Stdio::null())
        .stdout(open()?)
        .stderr(open()?);
    in_new_session(&mut cmd).spawn().map_err(|e| format!("裏での起動に失敗しました: {e}"))?;
    Ok(false)
}

pub fn default_log_path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into())).join(".local/state")
    });
    base.join("micmux").join("client.log")
}

/// 時刻つきでログに出す。裏で動いているときは stderr がログファイルになる。
fn say(text: &str) {
    eprintln!("{} micmux: {text}", clock());
}

/// ログ用の現地時刻 (HH:MM:SS)。
fn clock() -> String {
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
    }
}

/// bridge まで繋ぐ子プロセス。どの経路で抜けても drop で止める。
struct Transport(Child);

impl Drop for Transport {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// 終了の指示を受けた、または監視対象がいなくなった（どちらも正常終了）。
struct Done;

struct Client {
    opts: Options,
    writer: Writer,
    tx: Sender<Event>,
    /// bridge との HELLO の往復が済んだか。
    connected: bool,
    armed: bool,
    requested: Option<u32>,
    /// 取得、その開始時刻、世代番号。
    capture: Option<(Capture, Instant, u64)>,
    /// 取得を始めるたびに増やす。session 番号は同じ要求のあいだ使い回されるので、
    /// 取得の終了通知の照合には使えない（前の取得の通知で、始め直した取得を止めてしまう）。
    generation: u64,
    rtt_ms: Option<u128>,
    sessions: u64,
    /// 生きているあいだ動き続ける対象。すべて居なくなったら終了する。
    /// 自分の --watch-pid と、相乗りしてきた ssh。--watch-pid 無しで起動した場合は空のまま。
    watched: Vec<i32>,
}

impl Client {
    fn send(&self, msg: &Msg) -> io::Result<()> {
        write_msg(&mut *self.writer.lock().unwrap(), msg)
    }

    fn begin(&mut self, session: u32) {
        self.end("切り替え");
        self.generation += 1;
        let generation = self.generation;
        let tx = self.tx.clone();
        let capture = Capture::start(&self.opts.source, session, self.writer.clone(), move |r| {
            let _ = tx.send(Event::CaptureExit(generation, r));
        });
        self.capture = Some((capture, Instant::now(), generation));
        self.sessions += 1;
        say(&format!("● 録音中 → {} (session {session})", self.opts.host));
    }

    fn end(&mut self, why: &str) {
        let Some((capture, started, _)) = self.capture.take() else { return };
        let session = capture.session;
        let stats = capture.stop();
        let dropped = stats.dropped_samples.load(Ordering::Relaxed);
        say(&format!(
            "○ 録音停止 ({why}; session {session}, {:.1}s, {} KB, 最初の音声まで {} ms, 最大音量 {}{})",
            started.elapsed().as_secs_f32(),
            stats.bytes_sent.load(Ordering::Relaxed) / 1024,
            stats.first_audio_ms.load(Ordering::Relaxed),
            crate::capture::describe_peak(stats.peak.load(Ordering::Relaxed)),
            if dropped > 0 { format!(", 取りこぼし {dropped} サンプル") } else { String::new() },
        ));
    }

    fn deny(&mut self, session: u32) {
        let bell = if self.opts.bell { "\x07" } else { "" };
        say(&format!("{bell}録音要求を断りました (disarmed)。許可するには Enter か `micmux arm`"));
        let _ = self.send(&Msg::AudioEnd { session, reason: end_reason::DENIED });
    }

    /// 生存確認。bridge は何も届かない時間が続くと、接続元が消えたとみなして終わる。
    ///
    /// 録音中も送る（音声が止まっていても接続は生きていると伝えるため）。ただし音声の送信が
    /// 詰まっているときに待たされないよう、書き込み中なら見送る。そのときは音声が流れている。
    fn ping(&self, started: Instant) {
        if let Ok(mut w) = self.writer.try_lock() {
            let _ = write_msg(&mut *w, &Msg::Ping { nonce: started.elapsed().as_millis() as u64 });
        }
    }

    fn status(&self, json: bool) -> String {
        let recording = self.capture.is_some();
        if json {
            format!(
                "{{\"host\":\"{}\",\"connected\":{},\"armed\":{},\"requested\":{},\"recording\":{},\"sessions\":{},\"rtt_ms\":{}}}",
                self.opts.host.replace('\\', "\\\\").replace('"', "\\\""),
                self.connected,
                self.armed,
                self.requested.is_some(),
                recording,
                self.sessions,
                self.rtt_ms.map_or("null".into(), |r| r.to_string()),
            )
        } else {
            format!(
                "host={}{} armed={} requested={} recording={} sessions={} rtt={}",
                self.opts.host,
                if self.connected { "" } else { " (接続待ち)" },
                self.armed,
                self.requested.is_some(),
                recording,
                self.sessions,
                self.rtt_ms.map_or("-".into(), |r| format!("{r}ms")),
            )
        }
    }

    fn control(&mut self, cmd: Cmd) -> Option<String> {
        let cmd = match cmd {
            Cmd::Toggle if self.armed => Cmd::Disarm,
            Cmd::Toggle => Cmd::Arm,
            c => c,
        };
        match cmd {
            Cmd::Arm => {
                self.armed = true;
                say("armed: 録音要求を受け付けます");
                if let (Some(session), None) = (self.requested, &self.capture) {
                    self.begin(session);
                }
            }
            Cmd::Disarm => {
                self.armed = false;
                self.end("disarm");
                say("disarmed: 録音要求を断ります");
            }
            Cmd::Status => return Some(self.status(false)),
            Cmd::StatusJson => return Some(self.status(true)),
            Cmd::Quit => return None,
            Cmd::Toggle => unreachable!(),
        }
        Some(self.status(false))
    }

    /// 操作を処理して返事をする。終了の指示なら false。
    fn on_control(&mut self, cmd: Cmd, reply: Option<SyncSender<String>>) -> bool {
        match self.control(cmd) {
            Some(text) => {
                match reply {
                    Some(r) => drop(r.send(text)),
                    None if matches!(cmd, Cmd::Status | Cmd::StatusJson) => say(&text),
                    None => {}
                }
                true
            }
            None => {
                if let Some(r) = reply {
                    let _ = r.send("bye".into());
                }
                false
            }
        }
    }

    fn on_watch(&mut self, pid: i32, host: &str, reply: SyncSender<String>) {
        let text = if host != self.opts.host {
            format!("error: {} に接続中のため {host} には使えません", self.opts.host)
        } else {
            // --watch-pid 無しで起動された（端末から手で動かしている）なら、終了の条件にしない。
            // 相乗りしてきた ssh が終わっただけで、手で起動したものまで終わってしまわないように
            if self.opts.watch_pid.is_some() && !self.watched.contains(&pid) {
                self.watched.push(pid);
            }
            "ok".into()
        };
        let _ = reply.send(text);
    }

    /// 次のイベントを `timeout` まで待つ。操作・登録・取得の終了はここで片付け、
    /// bridge からのものだけを返す。接続の確立前でも同じように処理するので、
    /// 接続を待っているあいだに来た登録や操作も取りこぼさない。
    fn next(&mut self, rx: &Receiver<Event>, timeout: Duration) -> Result<Option<Event>, Done> {
        let ev = match rx.recv_timeout(timeout) {
            Ok(ev) => Some(ev),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return Err(Done),
        };
        let rest = match ev {
            Some(Event::Control(cmd, reply)) => {
                if !self.on_control(cmd, reply) {
                    self.end("終了");
                    return Err(Done);
                }
                None
            }
            Some(Event::Watch { pid, host, reply }) => {
                self.on_watch(pid, &host, reply);
                None
            }
            Some(Event::CaptureExit(generation, result)) => {
                if self.capture.as_ref().is_some_and(|c| c.2 == generation) {
                    self.end("取得が終了");
                    if let Err(e) = result {
                        say(&format!("マイクを取得できません: {e}"));
                    }
                }
                None
            }
            other => other,
        };
        // 登録された ssh がすべて終わったら、こちらも終わる
        if !self.watched.is_empty() {
            self.watched.retain(|&pid| alive(pid));
            if self.watched.is_empty() {
                self.end("終了");
                say("監視対象の ssh がすべて終了したので終わります");
                return Err(Done);
            }
        }
        Ok(rest)
    }

    fn on_msg(&mut self, msg: Msg, started: Instant) {
        match msg {
            Msg::Start { session } => {
                self.requested = Some(session);
                if self.armed {
                    self.begin(session);
                } else {
                    self.deny(session);
                }
            }
            Msg::Stop { session } => {
                if self.requested == Some(session) {
                    self.requested = None;
                }
                if self.capture.as_ref().is_some_and(|(c, ..)| c.session == session) {
                    self.end("要求終了");
                }
            }
            Msg::Pong { nonce } => {
                self.rtt_ms = Some(started.elapsed().as_millis().saturating_sub(nonce as u128));
            }
            Msg::Ping { nonce } => {
                let _ = self.send(&Msg::Pong { nonce });
            }
            Msg::Error { message } => say(&format!("bridge のエラー: {message}")),
            other => say(&format!("想定外のメッセージ: {other:?}")),
        }
    }
}

pub fn run(opts: Options) -> Result<(), String> {
    if std::env::var_os(ssh::INTERNAL_ENV).is_some() {
        return Ok(()); // micmux 自身の ssh から呼ばれた。何もしない
    }
    if control::another_instance_running() {
        // 同じホストへ 2 本目の ssh を張った場合。動いている方に相乗りして終わる
        return control::join_running_instance(&opts);
    }
    if opts.background {
        let log = opts.log_path.clone().unwrap_or_else(default_log_path);
        if !detach(&log)? {
            return Ok(());
        }
        say(&format!(
            "起動 (version {}, pid {}, host {}, watch-pid {:?}, mux {}, 取得 {:?})",
            env!("CARGO_PKG_VERSION"),
            std::process::id(),
            opts.host,
            opts.watch_pid,
            opts.mux,
            opts.source,
        ));
    }

    // ほぼ同時に起動した別の micmux に先を越された場合は、そちらに相乗りする
    let Some(_lock) = control::lock_instance()? else {
        return control::join_when_ready(&opts);
    };
    // 接続を待っているあいだに来る別の ssh からの登録も受けられるよう、先にソケットを開く。
    // 返った SocketFile が drop されるとき（この関数を抜けるとき）にソケットのファイルを消す
    let (tx, rx) = channel::<Event>();
    let socket = control::listen(tx.clone());
    if socket.is_err() && control::another_instance_running() {
        return control::join_running_instance(&opts); // 排他を使わない古い版の micmux が動いている
    }

    let armed = opts.armed;
    let max = (opts.max_seconds > 0).then(|| Duration::from_secs(opts.max_seconds));
    let watched = opts.watch_pid.into_iter().collect();
    let mut client = Client {
        opts,
        // 接続するまでは捨てる。接続したら ssh の stdin に差し替える
        writer: Arc::new(Mutex::new(Box::new(io::sink()))),
        tx,
        connected: false,
        armed,
        requested: None,
        capture: None,
        generation: 0,
        rtt_ms: None,
        sessions: 0,
        watched,
    };

    if client.opts.mux && client.opts.transport.is_none() {
        if let Err(e) = ssh::check_mux_config(&client.opts) {
            say(&e);
            return Err(e);
        }
        let deadline = Instant::now() + Duration::from_secs(120);
        while !ssh::master_up(&client.opts) {
            if Instant::now() >= deadline {
                let e = "端末用の SSH 接続が 120 秒待っても確立しませんでした".to_string();
                say(&e);
                return Err(e);
            }
            if let Err(Done) = client.next(&rx, Duration::from_millis(300)) {
                return Ok(());
            }
        }
    }
    let mut child = Transport(
        ssh::spawn_transport(&client.opts).map_err(|e| format!("接続コマンドを起動できません: {e}"))?,
    );
    *client.writer.lock().unwrap() = Box::new(child.0.stdin.take().expect("piped stdin"));
    let mut from_bridge = child.0.stdout.take().expect("piped stdout");
    {
        let tx = client.tx.clone();
        thread::spawn(move || {
            let closed = protocol::forward(&mut from_bridge, |m| tx.send(Event::Msg(m)).is_ok());
            let _ = tx.send(Event::Closed(closed));
        });
    }

    // ここから先で抜けると、child の drop で接続コマンドを止める
    client.send(&Msg::Hello { version: VERSION }).map_err(|e| format!("bridge へ送信できません: {e}"))?;
    // 応答の無い接続先で待ち続けると、操作用ソケットを握ったまま残り、以後の ssh がすべて
    // ここに相乗りしてしまう。裏で動くときは BatchMode で尋ねられることも無いので短く切る。
    // 端末から動かすときはパスフレーズなどの入力を待つので長めにとる
    let limit = Duration::from_secs(if client.opts.background { 30 } else { 120 });
    let deadline = Instant::now() + limit;
    let source_name = loop {
        if Instant::now() >= deadline {
            return Err(format!("bridge から {} 秒待っても応答がありません", limit.as_secs()));
        }
        match client.next(&rx, Duration::from_millis(500)) {
            Ok(Some(Event::Msg(Msg::HelloAck { version, source }))) if version == VERSION => break source,
            Ok(Some(Event::Msg(Msg::HelloAck { version, .. }))) => {
                return Err(format!("プロトコルのバージョンが違います (bridge {version}, client {VERSION})"));
            }
            Ok(Some(Event::Msg(Msg::Error { message }))) => return Err(format!("bridge: {message}")),
            Ok(Some(Event::Closed(e))) => {
                let detail = e.map(|e| format!(": {e}")).unwrap_or_default();
                return Err(format!(
                    "bridge を起動できませんでした{detail}\n  接続先に micmux が入っているか確認してください（PATH にない場合は --remote-cmd で場所を指定）"
                ));
            }
            Ok(_) => {}
            Err(Done) => return Ok(()),
        }
    };
    client.connected = true;

    {
        // 端末からの操作: Enter で arm/disarm の切り替え
        let tx = client.tx.clone();
        thread::spawn(move || {
            for line in io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                match Cmd::parse(&line) {
                    Some(cmd) => {
                        if tx.send(Event::Control(cmd, None)).is_err() {
                            break;
                        }
                    }
                    None => eprintln!("micmux: Enter=切り替え  a=arm  d=disarm  s=status  q=終了"),
                }
            }
        });
    }

    say(&format!("{} に接続しました。仮想マイク: {source_name}", client.opts.host));
    match &socket {
        Ok(s) => say(&format!("操作用ソケット: {}", s.path.display())),
        Err(e) => say(&format!("操作用ソケットは使えません: {e}")),
    }
    say(if client.armed {
        "armed: 録音要求を受け付けます (Enter で切り替え、q で終了)"
    } else {
        "disarmed: 録音要求は断ります (Enter で許可、q で終了)"
    });

    let started = Instant::now();
    let mut next_ping = Instant::now();
    let result = loop {
        let ev = match client.next(&rx, Duration::from_millis(500)) {
            Ok(ev) => ev,
            Err(Done) => break Ok(()),
        };
        match ev {
            Some(Event::Msg(msg)) => client.on_msg(msg, started),
            Some(Event::Closed(e)) => {
                client.end("切断");
                break Err(match e {
                    Some(e) => format!("bridge との通信に失敗しました: {e}"),
                    None => "接続が切れました".into(),
                });
            }
            _ => {}
        }

        // 連続録音時間の上限
        // 打ち切ったあとは、同じ要求が続いていても自動では再開しない
        if let Some((_, since, _)) = &client.capture {
            if max.is_some_and(|max| since.elapsed() >= max) {
                client.end("時間上限");
                say("連続録音時間の上限に達しました。続けるには `micmux arm`");
            }
        }
        if Instant::now() >= next_ping {
            next_ping = Instant::now() + Duration::from_secs(10);
            client.ping(started);
        }
    };

    // stdin を閉じれば bridge は後片付けをして終わる。終わらなければ child の drop で止める
    *client.writer.lock().unwrap() = Box::new(io::sink());
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && matches!(child.0.try_wait(), Ok(None)) {
        thread::sleep(Duration::from_millis(50));
    }
    result
}
