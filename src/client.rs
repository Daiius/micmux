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
use crate::sanitize::{self, RateLimit};
use crate::ssh;
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
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
    let file = open_log(log)?;
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate own executable: {e}"))?;
    let mut cmd = Command::new(exe);
    cmd.args(std::env::args_os().skip(1))
        .env(DETACHED_ENV, "1")
        .stdin(Stdio::null())
        .stdout(file.try_clone().map_err(|e| format!("{}: {e}", log.display()))?)
        .stderr(file);
    in_new_session(&mut cmd).spawn().map_err(|e| format!("failed to start in the background: {e}"))?;
    Ok(false)
}

/// 裏で動くときのログの場所。XDG_STATE_HOME も HOME も無ければ決められない。
///
/// /tmp には置かない。誰でも書ける場所では、別の利用者が先にディレクトリや symlink を
/// 用意しておき、こちらのログを読んだり、別のファイルへ書き込ませたりできてしまう。
pub fn default_log_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("micmux").join("client.log"))
}

/// これを超えたログは、起動時に 1 世代だけ退避する。
const LOG_ROTATE_BYTES: u64 = 1024 * 1024;

/// 裏で動くときのログを開く。録音の時刻や接続先が書かれるので、本人だけが読めるようにする。
fn open_log(log: &Path) -> Result<std::fs::File, String> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    let err = |p: &Path, e: io::Error| format!("{}: {e}", p.display());
    if let Some(dir) = log.parent() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(|e| err(dir, e))?;
        // 古い版が 0755 で作ったものも閉じる。symlink の先は変えない
        if std::fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir()) {
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
    }
    // 際限なく増えないよう、大きくなっていたら 1 世代だけ残して作り直す
    if std::fs::symlink_metadata(log).is_ok_and(|m| m.is_file() && m.len() > LOG_ROTATE_BYTES) {
        let mut old = log.as_os_str().to_owned();
        old.push(".1");
        std::fs::rename(log, &old).map_err(|e| err(log, e))?;
        let _ = std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o600));
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(log)
        .map_err(|e| err(log, e))?;
    // 古い版が 0644 で作ったログも、本人だけが読めるようにする
    file.set_permissions(std::fs::Permissions::from_mode(0o600)).map_err(|e| err(log, e))?;
    Ok(file)
}

/// 1 分あたりに出すログの行数。接続先が録音要求を連打するなどしても、ログが際限なく増えないように。
static SAY_LIMIT: Mutex<RateLimit> = Mutex::new(RateLimit::new(60, Duration::from_secs(60)));

/// 時刻つきでログに出す。裏で動いているときは stderr がログファイルになる。
///
/// 書き込みの失敗は無視する（`eprintln!` はディスクが一杯のときなどに panic する）。
fn say(text: &str) {
    let (ok, dropped) = SAY_LIMIT.lock().map_or((true, 0), |mut l| l.allow(Instant::now()));
    let mut err = io::stderr().lock();
    if dropped > 0 {
        let _ = writeln!(err, "{} micmux: too many log lines, {dropped} omitted", clock());
    }
    if ok {
        let _ = writeln!(err, "{} micmux: {text}", clock());
    }
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

/// 接続コマンド（ssh と、その先の bridge）の stderr を、制御文字を落とし量を制限してから
/// 自分の stderr へ流す。接続先の出力をそのまま通すと、端末の制御シーケンスを流し込まれたり、
/// 裏で動くときのログでディスクを使い尽くされたりする。
///
/// ssh 自身のメッセージ（認証の失敗など）もここを通る。パスフレーズや host key の確認は
/// ssh が端末 (/dev/tty) に直接尋ねるので、ここを通らない。
fn relay_stderr(mut from: impl Read + Send + 'static) {
    thread::spawn(move || {
        let mut relay = sanitize::Relay::default();
        let mut buf = [0u8; 4096];
        loop {
            let n = match from.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return,
            };
            let text = relay.feed(&buf[..n], Instant::now());
            if !text.is_empty() {
                let mut err = io::stderr().lock();
                let _ = err.write_all(text.as_bytes());
                let _ = err.flush();
            }
        }
    });
}

/// bridge まで繋ぐ子プロセス。どの経路で抜けても drop で止める。
struct Transport(Child);

impl Drop for Transport {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// bridge から届いて、main がまだ受け取っていないメッセージの数の上限。
///
/// 読み取りスレッドが無制限に溜めると、接続先がメッセージを送り続けるだけで接続元のメモリを
/// 使い尽くせる。channel 自体は上限なしのままにして、bridge からの分だけをここで数える。
/// channel ごと有界にすると、操作用ソケットや取得スレッドからの送信まで止まりうる
/// （取得スレッドの終了通知が止まると、その終了を join で待つ main と互いに待ち合う）。
const MAX_INFLIGHT: usize = 64;

#[derive(Default)]
struct Inflight {
    count: Mutex<usize>,
    freed: Condvar,
}

impl Inflight {
    /// 空きができるまで待ってから 1 つ確保する。
    fn acquire(&self) {
        let mut n = self.count.lock().unwrap();
        while *n >= MAX_INFLIGHT {
            n = self.freed.wait(n).unwrap();
        }
        *n += 1;
    }

    fn release(&self) {
        let mut n = self.count.lock().unwrap();
        *n = n.saturating_sub(1);
        self.freed.notify_one();
    }
}

/// 相乗りしてくる ssh の登録数の上限。
const MAX_WATCHED: usize = 64;

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
    /// 連続録音時間の上限。None なら上限なし。
    max: Option<Duration>,
    inflight: Arc<Inflight>,
}

impl Client {
    fn send(&self, msg: &Msg) -> io::Result<()> {
        write_msg(&mut *self.writer.lock().unwrap(), msg)
    }

    fn begin(&mut self, session: u32) {
        // 録音中に届いた START で取得を始め直しても、連続録音時間は数え直さない。
        // 数え直すと、接続先が START を送り続けるだけで上限をすり抜けられる
        let since = self.capture.as_ref().map(|c| c.1);
        self.end("restart");
        self.generation += 1;
        let generation = self.generation;
        let tx = self.tx.clone();
        let capture = Capture::start(&self.opts.source, session, self.writer.clone(), move |r| {
            let _ = tx.send(Event::CaptureExit(generation, r));
        });
        self.capture = Some((capture, since.unwrap_or_else(Instant::now), generation));
        self.sessions += 1;
        say(&format!("● recording → {} (session {session})", self.opts.host));
    }

    fn end(&mut self, why: &str) {
        let Some((capture, started, _)) = self.capture.take() else { return };
        let session = capture.session;
        let stats = capture.stop();
        let dropped = stats.dropped_samples.load(Ordering::Relaxed);
        say(&format!(
            "○ stopped recording ({why}; session {session}, {:.1}s, {} KB, first audio after {} ms, peak {}{})",
            started.elapsed().as_secs_f32(),
            stats.bytes_sent.load(Ordering::Relaxed) / 1024,
            stats.first_audio_ms.load(Ordering::Relaxed),
            crate::capture::describe_peak(stats.peak.load(Ordering::Relaxed)),
            if dropped > 0 { format!(", dropped {dropped} samples") } else { String::new() },
        ));
    }

    fn deny(&mut self, session: u32) {
        let bell = if self.opts.bell { "\x07" } else { "" };
        say(&format!("{bell}denied a recording request (disarmed); press Enter or run `micmux arm` to allow"));
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
                if self.connected { "" } else { " (connecting)" },
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
                say("armed: accepting recording requests");
                if let (Some(session), None) = (self.requested, &self.capture) {
                    self.begin(session);
                }
            }
            Cmd::Disarm => {
                self.armed = false;
                self.end("disarm");
                say("disarmed: denying recording requests");
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
            format!("error: connected to {}, cannot be used for {host}", self.opts.host)
        } else {
            // --watch-pid 無しで起動された（端末から手で動かしている）なら、終了の条件にしない。
            // 相乗りしてきた ssh が終わっただけで、手で起動したものまで終わってしまわないように
            if self.opts.watch_pid.is_none() || self.watched.contains(&pid) {
                "ok".into()
            } else if self.watched.len() >= MAX_WATCHED {
                format!("error: too many watched processes (max {MAX_WATCHED})")
            } else {
                self.watched.push(pid);
                "ok".into()
            }
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
        if let Some(Event::Msg(_)) = ev {
            self.inflight.release();
        }
        let rest = match ev {
            Some(Event::Control(cmd, reply)) => {
                if !self.on_control(cmd, reply) {
                    self.end("exit");
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
                    self.end("capture ended");
                    if let Err(e) = result {
                        say(&format!("cannot capture from the microphone: {e}"));
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
                self.end("exit");
                say("all watched ssh processes have exited, exiting");
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
                    self.end("request ended");
                }
            }
            Msg::Pong { nonce } => {
                self.rtt_ms = Some(started.elapsed().as_millis().saturating_sub(nonce as u128));
            }
            Msg::Ping { nonce } => {
                let _ = self.send(&Msg::Pong { nonce });
            }
            Msg::Error { message } => {
                say(&format!("bridge error: {}", sanitize::line(&message, sanitize::MAX_LINE_CHARS)))
            }
            other => say(&format!("unexpected message: {}", other.summary())),
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
        let log = opts.log_path.clone().or_else(default_log_path).ok_or(
            "cannot choose a log location (neither XDG_STATE_HOME nor HOME is set); use --log",
        )?;
        if !detach(&log)? {
            return Ok(());
        }
        say(&format!(
            "started (version {}, pid {}, host {}, watch-pid {:?}, mux {}, capture {:?})",
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
        max,
        inflight: Arc::default(),
    };

    if client.opts.mux && client.opts.transport.is_none() {
        if let Err(e) = ssh::check_mux_config(&client.opts) {
            say(&e);
            return Err(e);
        }
        let deadline = Instant::now() + Duration::from_secs(120);
        while !ssh::master_up(&client.opts) {
            if Instant::now() >= deadline {
                let e = "the terminal's SSH connection was not established within 120 seconds".to_string();
                say(&e);
                return Err(e);
            }
            if let Err(Done) = client.next(&rx, Duration::from_millis(300)) {
                return Ok(());
            }
        }
    }
    let mut child = Transport(
        ssh::spawn_transport(&client.opts).map_err(|e| format!("cannot start the transport command: {e}"))?,
    );
    *client.writer.lock().unwrap() = Box::new(child.0.stdin.take().expect("piped stdin"));
    let mut from_bridge = child.0.stdout.take().expect("piped stdout");
    {
        let tx = client.tx.clone();
        let inflight = client.inflight.clone();
        thread::spawn(move || {
            let closed = protocol::forward(&mut from_bridge, |m| {
                inflight.acquire();
                tx.send(Event::Msg(m)).is_ok()
            });
            let _ = tx.send(Event::Closed(closed));
        });
    }
    if let Some(stderr) = child.0.stderr.take() {
        relay_stderr(stderr);
    }

    // ここから先で抜けると、child の drop で接続コマンドを止める
    client.send(&Msg::Hello { version: VERSION }).map_err(|e| format!("cannot send to bridge: {e}"))?;
    // 応答の無い接続先で待ち続けると、操作用ソケットを握ったまま残り、以後の ssh がすべて
    // ここに相乗りしてしまう。裏で動くときは BatchMode で尋ねられることも無いので短く切る。
    // 端末から動かすときはパスフレーズなどの入力を待つので長めにとる
    let limit = Duration::from_secs(if client.opts.background { 30 } else { 120 });
    let deadline = Instant::now() + limit;
    let source_name = loop {
        if Instant::now() >= deadline {
            return Err(format!("no response from bridge within {} seconds", limit.as_secs()));
        }
        match client.next(&rx, Duration::from_millis(500)) {
            Ok(Some(Event::Msg(Msg::HelloAck { version, source }))) if version == VERSION => {
                break sanitize::line(&source, sanitize::MAX_LINE_CHARS);
            }
            Ok(Some(Event::Msg(Msg::HelloAck { version, .. }))) => {
                return Err(format!("protocol version mismatch (bridge {version}, client {VERSION})"));
            }
            Ok(Some(Event::Msg(Msg::Error { message }))) => {
                return Err(format!("bridge: {}", sanitize::line(&message, sanitize::MAX_LINE_CHARS)));
            }
            Ok(Some(Event::Closed(e))) => {
                let detail = e.map(|e| format!(": {e}")).unwrap_or_default();
                return Err(format!(
                    "failed to start bridge{detail}\n  check that micmux is installed on the remote host (if it is not on PATH, use --remote-cmd)"
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
                    None => eprintln!("micmux: Enter=toggle  a=arm  d=disarm  s=status  q=quit"),
                }
            }
        });
    }

    say(&format!("connected to {}; virtual mic: {source_name}", client.opts.host));
    match &socket {
        Ok(s) => say(&format!("control socket: {}", s.path.display())),
        Err(e) => say(&format!("control socket unavailable: {e}")),
    }
    say(if client.armed {
        "armed: accepting recording requests (Enter to toggle, q to quit)"
    } else {
        "disarmed: denying recording requests (Enter to allow, q to quit)"
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
                client.end("disconnected");
                break Err(match e {
                    Some(e) => format!("communication with bridge failed: {e}"),
                    None => "connection lost".into(),
                });
            }
            _ => {}
        }

        // 連続録音時間の上限。打ち切ったら disarm する。
        // armed のままにすると、接続先が START を送り直すだけで録音が再開してしまう
        if let (Some((_, since, _)), Some(max)) = (&client.capture, client.max) {
            if since.elapsed() >= max {
                client.armed = false;
                client.end("time limit");
                say(&format!(
                    "reached the recording limit ({} s), disarmed; run `micmux arm` to continue",
                    max.as_secs()
                ));
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
