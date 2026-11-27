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
use std::sync::mpsc::{channel, sync_channel, RecvTimeoutError, Sender, SyncSender};
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
    pub max_seconds: u64,
    pub source: Source,
    pub bell: bool,
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
    CaptureExit(u32, Result<(), String>),
}

pub fn control_path() -> PathBuf {
    if let Some(p) = std::env::var_os("MICMUX_SOCKET") {
        return p.into();
    }
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .or_else(|| std::env::var_os("TMPDIR"))
        .unwrap_or_else(|| "/tmp".into());
    let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
    PathBuf::from(dir).join(format!("micmux-{user}.sock"))
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

fn spawn_control_socket(tx: Sender<Event>) -> Result<PathBuf, String> {
    let path = control_path();
    if path.exists() {
        if UnixStream::connect(&path).is_ok() {
            return Err(format!("別の micmux connect が動いています ({})", path.display()));
        }
        let _ = std::fs::remove_file(&path); // 前回の残骸
    }
    let listener = UnixListener::bind(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            conn.set_read_timeout(Some(Duration::from_secs(2))).ok();
            let mut line = String::new();
            let Ok(reader) = conn.try_clone() else { continue };
            if BufReader::new(reader).read_line(&mut line).is_err() {
                continue;
            }
            let reply = match Cmd::parse(&line) {
                // 空行での toggle は端末からの操作専用
                Some(cmd) if !line.trim().is_empty() => {
                    let (rtx, rrx) = sync_channel(1);
                    if tx.send(Event::Control(cmd, Some(rtx))).is_err() {
                        return;
                    }
                    rrx.recv_timeout(Duration::from_secs(2)).unwrap_or_else(|_| "timeout".into())
                }
                _ => "error: unknown command".into(),
            };
            let _ = writeln!(conn, "{reply}");
        }
    });
    Ok(path)
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
            let mut c = Command::new("ssh");
            c.args([
                "-T", // PTY を割り当てない（バイナリが変換されないように）
                "-o", "Compression=no",
                // 端末用の接続と TCP を分ける。既存の多重化接続にも相乗りしない
                "-o", "ControlMaster=no",
                "-o", "ControlPath=none",
                "-o", "ServerAliveInterval=10",
                "-o", "ServerAliveCountMax=3",
            ]);
            c.args(&opts.ssh_args).arg(&opts.host).arg("--").arg(remote);
            c
        }
    };
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn()
}

struct Client {
    opts: Options,
    writer: Writer,
    tx: Sender<Event>,
    armed: bool,
    requested: Option<u32>,
    capture: Option<(Capture, Instant)>,
    rtt_ms: Option<u128>,
    sessions: u64,
}

impl Client {
    fn say(&self, text: &str) {
        eprintln!("micmux: {text}");
    }

    fn send(&self, msg: &Msg) -> io::Result<()> {
        write_msg(&mut *self.writer.lock().unwrap(), msg)
    }

    fn begin(&mut self, session: u32) {
        self.end("切り替え");
        let tx = self.tx.clone();
        let capture = Capture::start(&self.opts.source, session, self.writer.clone(), move |r| {
            let _ = tx.send(Event::CaptureExit(session, r));
        });
        self.capture = Some((capture, Instant::now()));
        self.sessions += 1;
        self.say(&format!("● 録音中 → {} (session {session})", self.opts.host));
    }

    fn end(&mut self, why: &str) {
        let Some((capture, started)) = self.capture.take() else { return };
        let session = capture.session;
        let stats = capture.stop();
        let dropped = stats.dropped_samples.load(Ordering::Relaxed);
        self.say(&format!(
            "○ 録音停止 ({why}; session {session}, {:.1}s, {} KB, 最初の音声まで {} ms{})",
            started.elapsed().as_secs_f32(),
            stats.bytes_sent.load(Ordering::Relaxed) / 1024,
            stats.first_audio_ms.load(Ordering::Relaxed),
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

    fn status(&self, json: bool) -> String {
        let recording = self.capture.is_some();
        if json {
            format!(
                "{{\"host\":\"{}\",\"connected\":true,\"armed\":{},\"requested\":{},\"recording\":{},\"sessions\":{},\"rtt_ms\":{}}}",
                self.opts.host.replace('\\', "\\\\").replace('"', "\\\""),
                self.armed,
                self.requested.is_some(),
                recording,
                self.sessions,
                self.rtt_ms.map_or("null".into(), |r| r.to_string()),
            )
        } else {
            format!(
                "host={} armed={} requested={} recording={} sessions={} rtt={}",
                self.opts.host,
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
                if self.capture.as_ref().is_some_and(|(c, _)| c.session == session) {
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

pub fn run(opts: Options) -> Result<(), String> {
    let mut child = spawn_transport(&opts).map_err(|e| format!("接続コマンドを起動できません: {e}"))?;
    let writer: Writer = Arc::new(Mutex::new(Box::new(child.stdin.take().expect("piped stdin"))));
    let mut from_bridge = child.stdout.take().expect("piped stdout");

    let (tx, rx) = channel::<Event>();
    {
        let tx = tx.clone();
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

    let fail = |child: &mut Child, msg: String| -> Result<(), String> {
        let _ = child.kill();
        let _ = child.wait();
        Err(msg)
    };

    write_msg(&mut *writer.lock().unwrap(), &Msg::Hello { version: VERSION })
        .map_err(|e| format!("bridge へ送信できません: {e}"))?;
    let source_name = loop {
        match rx.recv() {
            Ok(Event::Msg(Msg::HelloAck { version, source })) if version == VERSION => break source,
            Ok(Event::Msg(Msg::HelloAck { version, .. })) => {
                return fail(&mut child, format!("プロトコルのバージョンが違います (bridge {version}, client {VERSION})"));
            }
            Ok(Event::Msg(Msg::Error { message })) => return fail(&mut child, format!("bridge: {message}")),
            Ok(Event::Closed(e)) => {
                let detail = e.map(|e| format!(": {e}")).unwrap_or_default();
                return fail(
                    &mut child,
                    format!(
                        "bridge を起動できませんでした{detail}\n  接続先に micmux が入っているか確認してください（PATH にない場合は --remote-cmd で場所を指定）"
                    ),
                );
            }
            Ok(_) => {}
            Err(_) => return fail(&mut child, "bridge との接続が切れました".into()),
        }
    };

    let socket = spawn_control_socket(tx.clone());
    {
        // 端末からの操作: Enter で arm/disarm の切り替え
        let tx = tx.clone();
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

    let armed = opts.armed;
    let max = Duration::from_secs(opts.max_seconds);
    let mut client = Client {
        opts,
        writer,
        tx,
        armed,
        requested: None,
        capture: None,
        rtt_ms: None,
        sessions: 0,
    };
    client.say(&format!(
        "{} に接続しました。仮想マイク: {source_name}",
        client.opts.host
    ));
    match &socket {
        Ok(p) => client.say(&format!("操作用ソケット: {}", p.display())),
        Err(e) => client.say(&format!("操作用ソケットは使えません: {e}")),
    }
    client.say(if armed {
        "armed: 録音要求を受け付けます (Enter で切り替え、q で終了)"
    } else {
        "disarmed: 録音要求は断ります (Enter で許可、q で終了)"
    });

    let started = Instant::now();
    let mut next_ping = Instant::now();
    let result = loop {
        let ev = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(ev) => Some(ev),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break Ok(()),
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
            Some(Event::Control(cmd, reply)) => match client.control(cmd) {
                Some(text) => match reply {
                    Some(r) => drop(r.send(text)),
                    None if matches!(cmd, Cmd::Status | Cmd::StatusJson) => client.say(&text),
                    None => {}
                },
                None => {
                    if let Some(r) = reply {
                        let _ = r.send("bye".into());
                    }
                    client.end("終了");
                    break Ok(());
                }
            },
            Some(Event::CaptureExit(session, result)) => {
                if client.capture.as_ref().is_some_and(|(c, _)| c.session == session) {
                    client.end("取得が終了");
                    if let Err(e) = result {
                        client.say(&format!("マイクを取得できません: {e}"));
                    }
                }
            }
            None => {}
        }

        // 連続録音時間の上限
        // 打ち切ったあとは、同じ要求が続いていても自動では再開しない
        if let Some((_, since)) = &client.capture {
            if since.elapsed() >= max {
                client.end("時間上限");
                client.say("連続録音時間の上限に達しました。続けるには `micmux arm`");
            }
        }
        if Instant::now() >= next_ping {
            next_ping = Instant::now() + Duration::from_secs(10);
            // 録音中は音声の流れを邪魔しない
            if client.capture.is_none() {
                let _ = client.send(&Msg::Ping { nonce: started.elapsed().as_millis() as u64 });
            }
        }
    };

    if let Ok(p) = socket {
        let _ = std::fs::remove_file(p);
    }
    // stdin を閉じれば bridge は後片付けをして終わる
    *client.writer.lock().unwrap() = Box::new(io::sink());
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && matches!(child.try_wait(), Ok(None)) {
        thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    result
}
