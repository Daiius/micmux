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

use crate::capture::{Capture, Source, Writer};
use crate::protocol::{end_reason, read_msg, write_msg, Msg, VERSION};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{channel, sync_channel, Receiver, RecvTimeoutError, Sender, SyncSender};
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

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Cmd {
    Arm,
    Disarm,
    Toggle,
    Status,
    StatusJson,
    Quit,
}

impl Cmd {
    pub fn parse(s: &str) -> Option<Cmd> {
        Some(match s.trim() {
            "arm" | "a" => Cmd::Arm,
            "disarm" | "d" => Cmd::Disarm,
            "toggle" | "" => Cmd::Toggle,
            "status" | "s" => Cmd::Status,
            "status-json" => Cmd::StatusJson,
            "quit" | "q" => Cmd::Quit,
            _ => return None,
        })
    }
}

enum Event {
    Msg(Msg),
    Closed(Option<io::Error>),
    Control(Cmd, Option<SyncSender<String>>),
    /// 別の ssh からの「自分が生きているあいだも動いていてほしい」という登録。
    Watch { pid: i32, host: String, reply: SyncSender<String> },
    /// 取得スレッドが自分から終わった。番号は取得の世代番号。
    CaptureExit(u64, Result<(), String>),
}

pub fn control_path() -> PathBuf {
    if let Some(p) = std::env::var_os("MICMUX_SOCKET") {
        return p.into();
    }
    // 環境変数があるときの場所は変えないこと。動いているインスタンスと食い違う
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .or_else(|| std::env::var_os("TMPDIR"))
        .or_else(user_temp_dir)
        .unwrap_or_else(|| "/tmp".into());
    let user = std::env::var("USER").ok().or_else(login_name).unwrap_or_else(|| "user".into());
    PathBuf::from(dir).join(format!("micmux-{user}.sock"))
}

/// TMPDIR の無い起動元（キーバインドの常駐ツールなど）からでも、同じ場所を指すようにする。
/// macOS の TMPDIR はこの値そのものなので、環境変数がある場合と一致する。
#[cfg(target_os = "macos")]
fn user_temp_dir() -> Option<std::ffi::OsString> {
    use std::os::unix::ffi::OsStringExt;
    let mut buf = vec![0u8; 1024];
    let n = unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, buf.as_mut_ptr().cast(), buf.len()) };
    if n == 0 || n > buf.len() {
        return None;
    }
    buf.truncate(n - 1); // 末尾の NUL
    Some(std::ffi::OsString::from_vec(buf))
}

#[cfg(not(target_os = "macos"))]
fn user_temp_dir() -> Option<std::ffi::OsString> {
    None
}

/// USER の無い起動元向けに、ユーザー名をパスワードデータベースから引く。
fn login_name() -> Option<String> {
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    let rc = unsafe { libc::getpwuid_r(libc::getuid(), &mut pw, buf.as_mut_ptr(), buf.len(), &mut found) };
    if rc != 0 || found.is_null() || pw.pw_name.is_null() {
        return None;
    }
    Some(unsafe { std::ffi::CStr::from_ptr(pw.pw_name) }.to_string_lossy().into_owned())
}

/// 動いている client に 1 行のコマンドを送り、返事を受け取る（`micmux arm` など）。
pub fn send_control(cmd: &str) -> Result<String, String> {
    let path = control_path();
    let mut s = UnixStream::connect(&path)
        .map_err(|e| format!("micmux connect が動いていません ({}: {e})", path.display()))?;
    s.set_read_timeout(Some(Duration::from_secs(3))).ok();
    writeln!(s, "{cmd}").map_err(|e| e.to_string())?;
    let mut reply = String::new();
    BufReader::new(s).read_line(&mut reply).map_err(|e| e.to_string())?;
    Ok(reply.trim_end().to_string())
}

/// 既に動いている `micmux connect` があるか。
fn another_instance_running() -> bool {
    UnixStream::connect(control_path()).is_ok()
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

/// 切り離されたあとの自分であることを示す環境変数。
const DETACHED_ENV: &str = "MICMUX_DETACHED";

/// 端末から切り離す。自分自身を新しいセッションで起動し直し、親はすぐに戻る
/// （ssh の LocalCommand を待たせない）。戻り値が false なら親側で、そのまま終了してよい。
///
/// fork するだけでは済ませない。macOS では fork しただけの子プロセスから Core Audio などの
/// システムフレームワークを使うと動作が保証されないので、必ず exec し直す。
fn detach(log: &std::path::Path) -> Result<bool, String> {
    use std::os::unix::process::CommandExt;
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
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().map_err(|e| format!("裏での起動に失敗しました: {e}"))?;
    Ok(false)
}

pub fn default_log_path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into())).join(".local/state")
    });
    base.join("micmux").join("client.log")
}

/// 起動の排他を取る。別の micmux が握っていれば `None`。返ったファイルを閉じるまで排他が続くので、
/// 操作用ソケットを作ってから終わるまで握っておく。
///
/// ソケットのファイルだけでは排他にならない。前回の残骸がある状態で 2 つが同時に起動すると、
/// 両方が「残骸だ」と判断し、後から消した方が先に作られたソケットを消してしまう。
/// flock はプロセスが終われば（異常終了でも）OS が外すので、残骸の心配が無い。
/// Rust はファイルを CLOEXEC で開くので、ssh などの子プロセスが排他を持ち越すこともない。
fn lock_instance() -> Result<Option<std::fs::File>, String> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;
    let mut path = control_path().into_os_string();
    path.push(".lock");
    let path = PathBuf::from(path);
    // ロックファイルは消さない。消すと、消す前に開いた側と後に作った側で排他が分かれる
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(file));
    }
    let e = io::Error::last_os_error();
    if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(None)
    } else {
        Err(format!("{}: {e}", path.display()))
    }
}

/// bind したソケットの (dev, ino)。後片付けで、自分のものかを確かめるのに使う。
type SocketId = (u64, u64);

fn socket_id(path: &std::path::Path) -> Option<SocketId> {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

/// 排他を握った状態で呼ぶ。
fn spawn_control_socket(tx: Sender<Event>) -> Result<(PathBuf, SocketId), String> {
    let path = control_path();
    if path.exists() {
        // 排他を使わない古い版の micmux が動いている場合
        if another_instance_running() {
            return Err(format!("別の micmux connect が動いています ({})", path.display()));
        }
        let _ = std::fs::remove_file(&path); // 前回の残骸
    }
    let listener = UnixListener::bind(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    let id = socket_id(&path).ok_or_else(|| format!("{} を作れませんでした", path.display()))?;
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            conn.set_read_timeout(Some(Duration::from_secs(2))).ok();
            let mut line = String::new();
            let Ok(reader) = conn.try_clone() else { continue };
            if BufReader::new(reader).read_line(&mut line).is_err() {
                continue;
            }
            let (rtx, rrx) = sync_channel(1);
            let mut words = line.split_whitespace();
            let event = match (words.next(), words.next().and_then(|p| p.parse().ok()), words.next()) {
                (Some("watch"), Some(pid), Some(host)) => {
                    Some(Event::Watch { pid, host: host.to_string(), reply: rtx })
                }
                // 空行での toggle は端末からの操作専用
                _ if line.trim().is_empty() => None,
                _ => Cmd::parse(&line).map(|cmd| Event::Control(cmd, Some(rtx))),
            };
            let reply = match event {
                Some(ev) => {
                    if tx.send(ev).is_err() {
                        return;
                    }
                    rrx.recv_timeout(Duration::from_secs(2)).unwrap_or_else(|_| "timeout".into())
                }
                None => "error: unknown command".into(),
            };
            let _ = writeln!(conn, "{reply}");
        }
    });
    Ok((path, id))
}

/// micmux が起動した ssh であることを示す環境変数。
///
/// ~/.ssh/config の `Match ... exec` から micmux を起動している場合、micmux 自身の ssh でも
/// その exec が走る。そこで起動された micmux がこれを見て何もせずに終われば、再帰しない。
pub const INTERNAL_ENV: &str = "MICMUX_INTERNAL";

fn ssh_command() -> Command {
    let mut c = Command::new("ssh");
    c.env(INTERNAL_ENV, "1");
    c
}

/// `--mux` に要る設定が揃っているかを確かめる。
fn check_mux_config(opts: &Options) -> Result<(), String> {
    let conf = ssh_command()
        .args(&opts.ssh_args)
        .args(["-G", &opts.host])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("ssh を実行できません: {e}"))?;
    match mux_config_problem(&String::from_utf8_lossy(&conf.stdout)) {
        Some(key) => Err(format!("--mux には ~/.ssh/config の {} に {key} の設定が必要です", opts.host)),
        None => Ok(()),
    }
}

/// `ssh -G` の出力から、`--mux` に足りない設定の名前を返す。
fn mux_config_problem(conf: &str) -> Option<&'static str> {
    let value = |key: &str| conf.lines().find_map(|l| l.strip_prefix(key)).map(str::trim).unwrap_or("");
    // 未設定や no は "false" と出る。これだと端末用の ssh がマスターにならず、待っても繋がらない
    if matches!(value("controlmaster "), "" | "false" | "no") {
        return Some("ControlMaster");
    }
    if matches!(value("controlpath "), "" | "none") {
        return Some("ControlPath");
    }
    if matches!(value("controlpersist "), "" | "no") {
        // これが無いと、端末用の ssh はログアウト時にこちらの接続が終わるのを待ち、
        // こちらはその ssh が終わるのを待つので、互いに終われなくなる
        return Some("ControlPersist");
    }
    None
}

/// 端末用の SSH がマスターとして立ち上がっている（＝利用者が認証を済ませた）か。
///
/// 立ち上がる前に接続すると、こちらの ssh が先に認証を始めてしまい、認証が 2 回になる。
fn master_up(opts: &Options) -> bool {
    ssh_command()
        .args(&opts.ssh_args)
        .args(["-O", "check", &opts.host])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// 操作用ソケットのファイルを、どの経路で終わっても消す。
/// 自分が bind したものでなければ消さない（別の micmux のソケットを消すと、操作が届かなくなる）。
struct SocketFile(Option<(PathBuf, SocketId)>);

impl Drop for SocketFile {
    fn drop(&mut self) {
        if let Some((p, id)) = self.0.take() {
            if socket_id(&p) == Some(id) {
                let _ = std::fs::remove_file(p);
            }
        }
    }
}

/// 動いている micmux に、この ssh が生きているあいだも動き続けるよう登録する。
fn join_running_instance(opts: &Options) -> Result<(), String> {
    match opts.watch_pid {
        Some(pid) => {
            let reply = send_control(&format!("watch {pid} {}", opts.host))?;
            if reply == "ok" || opts.background {
                if reply != "ok" {
                    eprintln!("micmux: {reply}");
                }
                Ok(())
            } else {
                Err(reply)
            }
        }
        None => Err(format!("別の micmux connect が動いています ({})", control_path().display())),
    }
}

fn spawn_transport(opts: &Options) -> io::Result<Child> {
    let remote = std::iter::once(opts.remote_cmd.as_str())
        .chain(["bridge"])
        .chain(opts.bridge_args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    let mut cmd = match &opts.transport {
        Some(t) => {
            let mut c = Command::new("sh");
            c.args(["-c", t]);
            c
        }
        None => {
            let mut c = ssh_command();
            c.args([
                "-T", // PTY を割り当てない（バイナリが変換されないように）
                "-o", "Compression=no",
                // 自分がマスターになることはない
                "-o", "ControlMaster=no",
            ]);
            if !opts.mux {
                // 端末用の接続と TCP を分ける。既存の多重化接続にも相乗りしない
                c.args(["-o", "ControlPath=none"]);
            }
            c.args([
                "-o", "ServerAliveInterval=10",
                "-o", "ServerAliveCountMax=3",
                // ~/.ssh/config の LocalCommand から起動された場合に、自分の接続でまた
                // LocalCommand が走って micmux が micmux を呼び続けるのを防ぐ
                "-o", "PermitLocalCommand=no",
            ]);
            if opts.background {
                // 端末が無いのでパスフレーズなどを尋ねられない。待ち続けずに失敗させる
                c.args(["-o", "BatchMode=yes"]);
            }
            c.args(&opts.ssh_args).arg(&opts.host).arg("--").arg(remote);
            c
        }
    };
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn()
}

/// ログ用の現地時刻 (HH:MM:SS)。
pub fn clock() -> String {
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
    }
}

/// 終わり方。`Ok` なら正常終了として扱う。
type Finish = Result<(), String>;

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
    fn say(&self, text: &str) {
        eprintln!("{} micmux: {text}", clock());
    }

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
        self.say(&format!("● 録音中 → {} (session {session})", self.opts.host));
    }

    fn end(&mut self, why: &str) {
        let Some((capture, started, _)) = self.capture.take() else { return };
        let session = capture.session;
        let stats = capture.stop();
        let dropped = stats.dropped_samples.load(Ordering::Relaxed);
        self.say(&format!(
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
        self.say(&format!(
            "{bell}録音要求を断りました (disarmed)。許可するには Enter か `micmux arm`"
        ));
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
                self.say("armed: 録音要求を受け付けます");
                if let (Some(session), None) = (self.requested, &self.capture) {
                    self.begin(session);
                }
            }
            Cmd::Disarm => {
                self.armed = false;
                self.end("disarm");
                self.say("disarmed: 録音要求を断ります");
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
                    None if matches!(cmd, Cmd::Status | Cmd::StatusJson) => self.say(&text),
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
    fn next(&mut self, rx: &Receiver<Event>, timeout: Duration) -> Result<Option<Event>, Finish> {
        let ev = match rx.recv_timeout(timeout) {
            Ok(ev) => Some(ev),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return Err(Ok(())),
        };
        let rest = match ev {
            Some(Event::Control(cmd, reply)) => {
                if !self.on_control(cmd, reply) {
                    self.end("終了");
                    return Err(Ok(()));
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
                        self.say(&format!("マイクを取得できません: {e}"));
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
                self.say("監視対象の ssh がすべて終了したので終わります");
                return Err(Ok(()));
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
            Msg::Error { message } => self.say(&format!("bridge のエラー: {message}")),
            other => self.say(&format!("想定外のメッセージ: {other:?}")),
        }
    }
}

/// 排他を握っている別の micmux が操作用ソケットを開くのを待ってから、相乗りする。
fn join_when_ready(opts: &Options) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !another_instance_running() {
        if Instant::now() >= deadline {
            return Err(format!(
                "別の micmux connect が起動中ですが、操作用ソケットが開きません ({})",
                control_path().display()
            ));
        }
        thread::sleep(Duration::from_millis(50));
    }
    join_running_instance(opts)
}

pub fn run(opts: Options) -> Result<(), String> {
    if std::env::var_os(INTERNAL_ENV).is_some() {
        return Ok(()); // micmux 自身の ssh から呼ばれた。何もしない
    }
    if another_instance_running() {
        // 同じホストへ 2 本目の ssh を張った場合。動いている方に相乗りして終わる
        return join_running_instance(&opts);
    }
    if opts.background {
        let log = opts.log_path.clone().unwrap_or_else(default_log_path);
        if !detach(&log)? {
            return Ok(());
        }
    }
    let watch_pid = opts.watch_pid;
    if opts.background {
        eprintln!(
            "{} micmux: 起動 (version {}, pid {}, host {}, watch-pid {:?}, mux {}, 取得 {:?})",
            clock(),
            env!("CARGO_PKG_VERSION"),
            std::process::id(),
            opts.host,
            watch_pid,
            opts.mux,
            opts.source,
        );
    }

    // ほぼ同時に起動した別の micmux に先を越された場合は、そちらに相乗りする
    let Some(_lock) = lock_instance()? else {
        return join_when_ready(&opts);
    };
    // 接続を待っているあいだに来る別の ssh からの登録も受けられるよう、先にソケットを開く
    let (tx, rx) = channel::<Event>();
    let socket = spawn_control_socket(tx.clone());
    if socket.is_err() && another_instance_running() {
        return join_running_instance(&opts); // 排他を使わない古い版の micmux が動いている
    }
    let _socket_file = SocketFile(socket.as_ref().ok().cloned());

    let armed = opts.armed;
    let max = (opts.max_seconds > 0).then(|| Duration::from_secs(opts.max_seconds));
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
        watched: watch_pid.into_iter().collect(),
    };

    if client.opts.mux && client.opts.transport.is_none() {
        if let Err(e) = check_mux_config(&client.opts) {
            client.say(&e);
            return Err(e);
        }
        let deadline = Instant::now() + Duration::from_secs(120);
        while !master_up(&client.opts) {
            if Instant::now() >= deadline {
                let e = "端末用の SSH 接続が 120 秒待っても確立しませんでした".to_string();
                client.say(&e);
                return Err(e);
            }
            if let Err(finish) = client.next(&rx, Duration::from_millis(300)) {
                return finish;
            }
        }
    }
    let mut child = spawn_transport(&client.opts).map_err(|e| format!("接続コマンドを起動できません: {e}"))?;
    *client.writer.lock().unwrap() = Box::new(child.stdin.take().expect("piped stdin"));
    let mut from_bridge = child.stdout.take().expect("piped stdout");
    {
        let tx = client.tx.clone();
        thread::spawn(move || loop {
            let ev = match read_msg(&mut from_bridge) {
                Ok(Some(m)) => Event::Msg(m),
                Ok(None) => Event::Closed(None),
                Err(e) => Event::Closed(Some(e)),
            };
            let done = matches!(ev, Event::Closed(_));
            if tx.send(ev).is_err() || done {
                return;
            }
        });
    }

    let stop_child = |child: &mut Child| {
        let _ = child.kill();
        let _ = child.wait();
    };
    let fail = |child: &mut Child, msg: String| -> Result<(), String> {
        stop_child(child);
        Err(msg)
    };

    if let Err(e) = client.send(&Msg::Hello { version: VERSION }) {
        return fail(&mut child, format!("bridge へ送信できません: {e}"));
    }
    // 応答の無い接続先で待ち続けると、操作用ソケットを握ったまま残り、以後の ssh がすべて
    // ここに相乗りしてしまう。裏で動くときは BatchMode で尋ねられることも無いので短く切る。
    // 端末から動かすときはパスフレーズなどの入力を待つので長めにとる
    let limit = Duration::from_secs(if client.opts.background { 30 } else { 120 });
    let deadline = Instant::now() + limit;
    let source_name = loop {
        if Instant::now() >= deadline {
            return fail(&mut child, format!("bridge から {} 秒待っても応答がありません", limit.as_secs()));
        }
        match client.next(&rx, Duration::from_millis(500)) {
            Ok(Some(Event::Msg(Msg::HelloAck { version, source }))) if version == VERSION => break source,
            Ok(Some(Event::Msg(Msg::HelloAck { version, .. }))) => {
                return fail(&mut child, format!("プロトコルのバージョンが違います (bridge {version}, client {VERSION})"));
            }
            Ok(Some(Event::Msg(Msg::Error { message }))) => return fail(&mut child, format!("bridge: {message}")),
            Ok(Some(Event::Closed(e))) => {
                let detail = e.map(|e| format!(": {e}")).unwrap_or_default();
                return fail(
                    &mut child,
                    format!(
                        "bridge を起動できませんでした{detail}\n  接続先に micmux が入っているか確認してください（PATH にない場合は --remote-cmd で場所を指定）"
                    ),
                );
            }
            Ok(_) => {}
            Err(finish) => {
                stop_child(&mut child);
                return finish;
            }
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

    client.say(&format!(
        "{} に接続しました。仮想マイク: {source_name}",
        client.opts.host
    ));
    match &socket {
        Ok((p, _)) => client.say(&format!("操作用ソケット: {}", p.display())),
        Err(e) => client.say(&format!("操作用ソケットは使えません: {e}")),
    }
    client.say(if client.armed {
        "armed: 録音要求を受け付けます (Enter で切り替え、q で終了)"
    } else {
        "disarmed: 録音要求は断ります (Enter で許可、q で終了)"
    });

    let started = Instant::now();
    let mut next_ping = Instant::now();
    let result = loop {
        let ev = match client.next(&rx, Duration::from_millis(500)) {
            Ok(ev) => ev,
            Err(finish) => break finish,
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
                client.say("連続録音時間の上限に達しました。続けるには `micmux arm`");
            }
        }
        if Instant::now() >= next_ping {
            next_ping = Instant::now() + Duration::from_secs(10);
            client.ping(started);
        }
    };

    // stdin を閉じれば bridge は後片付けをして終わる
    *client.writer.lock().unwrap() = Box::new(io::sink());
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && matches!(child.try_wait(), Ok(None)) {
        thread::sleep(Duration::from_millis(50));
    }
    stop_child(&mut child);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mux_config_requires_master_path_and_persist() {
        let ok = "controlmaster auto\ncontrolpath /x/cm-%C\ncontrolpersist 60\n";
        assert_eq!(mux_config_problem(ok), None);
        let yes = "controlmaster true\ncontrolpath /x\ncontrolpersist yes\n";
        assert_eq!(mux_config_problem(yes), None);
        // 未設定の ssh -G の出力
        assert_eq!(mux_config_problem("controlmaster false\ncontrolpersist no\n"), Some("ControlMaster"));
        assert_eq!(
            mux_config_problem("controlmaster auto\ncontrolpersist 60\n"),
            Some("ControlPath")
        );
        assert_eq!(
            mux_config_problem("controlmaster auto\ncontrolpath /x\ncontrolpersist no\n"),
            Some("ControlPersist")
        );
    }

    #[test]
    fn login_name_matches_user() {
        let name = login_name().expect("ユーザー名を引けない");
        assert!(!name.is_empty());
        if let Ok(user) = std::env::var("USER") {
            assert_eq!(name, user);
        }
    }
}
