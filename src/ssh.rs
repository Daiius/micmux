//! 接続元から起動する ssh の組み立てと、`--mux`（SSH の多重化への相乗り）の下調べ。

use crate::client::Options;
use std::io;
use std::process::{Child, Command, Stdio};

/// micmux が起動した ssh であることを示す環境変数。
///
/// ~/.ssh/config の `Match ... exec` から micmux を起動している場合、micmux 自身の ssh でも
/// その exec が走る。そこで起動された micmux がこれを見て何もせずに終われば、再帰しない。
pub const INTERNAL_ENV: &str = "MICMUX_INTERNAL";

/// micmux が起動する ssh はすべてこれで作る。
pub fn command() -> Command {
    let mut c = Command::new("ssh");
    c.env(INTERNAL_ENV, "1");
    c
}

/// `--mux` に要る設定が揃っているかを確かめる。
pub fn check_mux_config(opts: &Options) -> Result<(), String> {
    let conf = command()
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
pub fn master_up(opts: &Options) -> bool {
    command()
        .args(&opts.ssh_args)
        .args(["-O", "check", &opts.host])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// bridge まで繋ぐ子プロセス（ssh、または `--transport` のコマンド）を起動する。
/// 標準入出力がそのまま bridge との通信路になる。
pub fn spawn_transport(opts: &Options) -> io::Result<Child> {
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
            let mut c = command();
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
}
