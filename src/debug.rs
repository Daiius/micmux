//! 接続元での不具合調査用。状況をひとまとめに出力する。
//!
//! 何も変更しない。マイクは合計で数秒だけ開く。

use crate::capture::{self, Source};
use crate::{client, control, sanitize, ssh};
use std::process::{Command, Stdio};
use std::time::Duration;

fn section(title: &str) {
    println!("\n== {title}");
}

/// コマンドを実行して、標準出力と標準エラーをまとめて返す。
fn capture_output(cmd: &mut Command) -> String {
    match cmd.stdin(Stdio::null()).output() {
        Ok(out) => {
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            if !out.status.success() {
                text.push_str(&format!("(終了コード: {})\n", out.status));
            }
            text
        }
        Err(e) => format!("(実行できません: {e})\n"),
    }
}

/// 字下げして出す。関係のない長いコマンドラインで埋もれないよう、1 行は 200 文字まで。
/// ログの末尾や接続先の出力には接続先由来の文字列が入るので、制御文字を落としてから出す。
fn indent(text: &str) {
    for line in text.lines() {
        println!("  {}", sanitize::line(line, 200));
    }
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| "(未設定)".into())
}

pub fn run(host: Option<String>, remote_cmd: &str, source: &Source) -> Result<(), String> {
    section("micmux");
    println!("  version: {}", env!("CARGO_PKG_VERSION"));
    println!("  build:   {} {}", std::env::consts::OS, std::env::consts::ARCH);
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
    println!("  exe:     {exe}");
    println!("  更新日時: {}", capture_output(Command::new("ls").args(["-l", &exe])).trim());
    println!("  --background に対応: はい（この行が出ていれば新しい版）");
    if cfg!(target_os = "macos") {
        println!("  macOS:   {}", capture_output(Command::new("sw_vers").arg("-productVersion")).trim());
    }

    section("環境");
    // マイクの許可は、起動元のターミナルアプリに対して与えられる
    println!("  TERM_PROGRAM:  {}", env("TERM_PROGRAM"));
    println!("  SHELL:         {}", env("SHELL"));
    println!("  TMUX:          {}", if std::env::var_os("TMUX").is_some() { "tmux の中" } else { "tmux の外" });
    println!("  SSH_AUTH_SOCK: {}", env("SSH_AUTH_SOCK"));
    println!("  PATH 上の micmux: {}", capture_output(Command::new("sh").args(["-c", "command -v micmux"])).trim());

    section("動いている micmux");
    let socket = control::control_path();
    println!("  操作用ソケット: {} ({})", socket.display(), if socket.exists() { "ある" } else { "無い" });
    match control::send_control("status") {
        Ok(s) => println!("  status: {s}"),
        Err(e) => println!("  status: {e}"),
    }
    indent(&capture_output(Command::new("sh").args(["-c", "ps -axo pid,ppid,lstart,command | grep '[m]icmux' || echo '(micmux のプロセスは無い)'"])));

    section("裏で動かしたときのログ (末尾 40 行)");
    match client::default_log_path() {
        Some(log) => {
            println!("  {}", log.display());
            // 不正な UTF-8 が混ざっていても読めるようにする
            match std::fs::read(&log) {
                Ok(bytes) => {
                    let text = String::from_utf8_lossy(&bytes);
                    let lines: Vec<&str> = text.lines().collect();
                    indent(&lines[lines.len().saturating_sub(40)..].join("\n"));
                }
                Err(e) => println!("  (読めません: {e})"),
            }
        }
        None => println!("  (XDG_STATE_HOME も HOME も無いので場所が決まりません)"),
    }

    if let Some(host) = &host {
        section(&format!("ssh の設定 ({host})"));
        let keys = [
            "hostname ", "user ", "port ", "permitlocalcommand ", "localcommand ", "remotecommand ",
            "requesttty ", "controlmaster ", "controlpath ", "controlpersist ", "identityagent ",
            "proxyjump ", "proxycommand ", "batchmode ",
        ];
        let conf = capture_output(ssh::command().args(["-G", host]));
        indent(&conf.lines().filter(|l| keys.iter().any(|k| l.starts_with(k))).collect::<Vec<_>>().join("\n"));

        section("端末なしでの接続（裏で動く micmux と同じ条件）");
        let remote = format!("echo \"remote PATH=$PATH\"; {remote_cmd} --version; {remote_cmd} doctor");
        indent(&capture_output(ssh::command().args([
            "-T",
            "-o", "BatchMode=yes",
            "-o", "ControlMaster=no",
            "-o", "ControlPath=none",
            "-o", "PermitLocalCommand=no",
            "-o", "ConnectTimeout=10",
            host,
            "--",
            &remote,
        ])));
    } else {
        section("ssh");
        println!("  (接続先を指定すると ssh の設定と接続も点検します: micmux debug <host>)");
    }

    section("マイク（この端末から直接）");
    match capture::mic_test(source, Duration::from_millis(1500)) {
        Ok(s) => println!("  {s}"),
        Err(e) => println!("  失敗: {e}"),
    }

    section("マイク（端末から切り離して。--background と同じ条件）");
    let mut cmd = Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
    cmd.arg("mic-test");
    if let Source::Command { cmd: c, rate } = source {
        cmd.args(["--capture-cmd", c, "--capture-rate", &rate.to_string()]);
    }
    indent(&capture_output(client::in_new_session(&mut cmd)));

    println!("\n以上です。ホスト名やユーザー名が含まれるので、貼る前に目を通してください。");
    Ok(())
}
