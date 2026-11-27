//! 動いている `micmux connect` を外から操作するためのソケットと、起動の排他。
//!
//! 同じ利用者の `micmux connect` は 1 つだけ動く。2 本目以降の ssh から起動されたものは、
//! 動いている方に「自分が生きているあいだも動いていてほしい」と登録して終わる（相乗り）。

use crate::client::{Event, Options};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, Sender};
use std::thread;
use std::time::{Duration, Instant};

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
pub fn another_instance_running() -> bool {
    UnixStream::connect(control_path()).is_ok()
}

/// 起動の排他を取る。別の micmux が握っていれば `None`。返ったファイルを閉じるまで排他が続くので、
/// 操作用ソケットを作ってから終わるまで握っておく。
///
/// ソケットのファイルだけでは排他にならない。前回の残骸がある状態で 2 つが同時に起動すると、
/// 両方が「残骸だ」と判断し、後から消した方が先に作られたソケットを消してしまう。
/// flock はプロセスが終われば（異常終了でも）OS が外すので、残骸の心配が無い。
/// Rust はファイルを CLOEXEC で開くので、ssh などの子プロセスが排他を持ち越すこともない。
pub fn lock_instance() -> Result<Option<std::fs::File>, String> {
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
fn socket_id(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

/// 操作用ソケットのファイル。どの経路で終わっても、drop で消す。
/// 自分が bind したものでなければ消さない（別の micmux のソケットを消すと、操作が届かなくなる）。
pub struct SocketFile {
    pub path: PathBuf,
    id: (u64, u64),
}

impl Drop for SocketFile {
    fn drop(&mut self) {
        if socket_id(&self.path) == Some(self.id) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// 操作用ソケットを開き、届いた操作を `tx` へ渡すスレッドを立てる。排他を握った状態で呼ぶ。
pub fn listen(tx: Sender<Event>) -> Result<SocketFile, String> {
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
                (Some("watch"), Some(pid), Some(_)) if !valid_watch_pid(pid) => {
                    Err(format!("error: pid {pid} は監視できません"))
                }
                (Some("watch"), Some(pid), Some(host)) => {
                    Ok(Event::Watch { pid, host: host.to_string(), reply: rtx })
                }
                // 空行での toggle は端末からの操作専用
                _ if line.trim().is_empty() => Err("error: unknown command".into()),
                _ => Cmd::parse(&line)
                    .map(|cmd| Event::Control(cmd, Some(rtx)))
                    .ok_or_else(|| "error: unknown command".into()),
            };
            let reply = match event {
                Ok(ev) => {
                    if tx.send(ev).is_err() {
                        return;
                    }
                    rrx.recv_timeout(Duration::from_secs(2)).unwrap_or_else(|_| "timeout".into())
                }
                Err(e) => e,
            };
            let _ = writeln!(conn, "{reply}");
        }
    });
    Ok(SocketFile { path, id })
}

/// 生きているあいだ動き続ける対象として受け付ける pid か。
///
/// 0 と負の値は kill(2) ではプロセスグループや全プロセスを指し、存在確認が常に成功してしまう。
/// 1 (init / launchd) は終わらない。どれを登録しても micmux が終わらなくなるので断る。
pub fn valid_watch_pid(pid: i32) -> bool {
    pid > 1
}

/// 動いている micmux に、この ssh が生きているあいだも動き続けるよう登録する。
pub fn join_running_instance(opts: &Options) -> Result<(), String> {
    let Some(pid) = opts.watch_pid else {
        return Err(format!("別の micmux connect が動いています ({})", control_path().display()));
    };
    let reply = send_control(&format!("watch {pid} {}", opts.host))?;
    if reply != "ok" {
        if !opts.background {
            return Err(reply);
        }
        eprintln!("micmux: {reply}");
    }
    Ok(())
}

/// 排他を握っている別の micmux が操作用ソケットを開くのを待ってから、相乗りする。
pub fn join_when_ready(opts: &Options) -> Result<(), String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_pid_must_be_above_one() {
        for pid in [i32::MIN, -1, 0, 1] {
            assert!(!valid_watch_pid(pid), "{pid}");
        }
        assert!(valid_watch_pid(2));
        assert!(valid_watch_pid(i32::MAX));
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
