// 内蔵のマイク取得を含まないビルドでは、リサンプラーなど一部が使われない
#![cfg_attr(not(any(target_os = "macos", feature = "native")), allow(dead_code))]

mod bridge;
mod capture;
mod client;
mod control;
mod debug;
mod protocol;
mod resample;
mod sanitize;
mod ssh;

use std::process::{Command, ExitCode};
use std::time::Duration;

const USAGE: &str = "\
micmux — On-demand microphone forwarding over SSH

Local side (where the microphone is):
  micmux connect <host> [options]   connect and wait for recording requests
      --armed                 allow recording from the start (default: disarmed)
      --max-seconds <n>       limit on continuous recording (default 120, 0 for no limit)
      --capture-cmd <cmd>     capture with this command (must write s16le mono to stdout)
                              e.g. 'rec -q -t raw -r 16000 -e signed -b 16 -c 1 -'
      --capture-rate <hz>     sample rate of the --capture-cmd output (default 16000)
      --remote-cmd <path>     path to micmux on the remote host (default micmux)
      --ssh-arg <arg>         extra argument for ssh (repeatable)
      --transport <cmd>       connect with this command instead of ssh
      --sink-name <name>      name of the virtual mic (default micmux; source micmux.monitor)
      --set-default           make the virtual mic the remote default source while connected
                              (on PipeWire, through a separate source micmux_mic)
      --no-bell               do not ring the bell when denying a request
      --background            detach from the terminal (log: ~/.local/state/micmux/client.log)
      --watch-pid <pid>       exit when this process exits
      --log <path>            log file for --background
      --mux                   share the terminal's SSH connection (authenticate once;
                              requires ControlMaster / ControlPath / ControlPersist)
  micmux arm | disarm | toggle      allow or deny recording in the running connect
  micmux status [--json]            show the state of the running connect
  micmux debug [<host>]             print local diagnostics for troubleshooting
      --remote-cmd / --capture-cmd / --capture-rate work as for connect

Remote side (where the apps run):
  micmux bridge [options]           started by connect over SSH (no need to run it by hand)
  micmux doctor                     check the remote audio setup
";

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = if args.is_empty() { String::new() } else { args.remove(0) };
    let result = match cmd.as_str() {
        "connect" => parse_connect(args).and_then(client::run),
        "bridge" => parse_bridge(args).and_then(bridge::run),
        "arm" | "disarm" | "toggle" => control::send_control(&cmd).map(|r| println!("{r}")),
        "status" => {
            let json = args.iter().any(|a| a == "--json");
            control::send_control(if json { "status-json" } else { "status" }).map(|r| println!("{r}"))
        }
        "doctor" => doctor(),
        "debug" => parse_debug(args).and_then(|(host, remote, source)| debug::run(host, &remote, &source)),
        // debug が切り離した状態でのマイク取得を試すために自分を呼ぶ
        "mic-test" => parse_debug(args).and_then(|(_, _, source)| {
            capture::mic_test(&source, Duration::from_millis(1500)).map(|s| println!("{s}"))
        }),
        "--version" | "-V" | "version" => {
            println!("micmux {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "" | "help" | "--help" | "-h" => {
            print!("{USAGE}");
            Ok(())
        }
        other => Err(format!("unknown command: {other}\n\n{USAGE}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("micmux: {e}");
            ExitCode::FAILURE
        }
    }
}

struct Args(std::vec::IntoIter<String>);

impl Args {
    fn value(&mut self, flag: &str) -> Result<String, String> {
        self.0.next().ok_or_else(|| format!("{flag} requires a value"))
    }

    fn number<T: std::str::FromStr>(&mut self, flag: &str) -> Result<T, String> {
        let v = self.value(flag)?;
        v.parse().map_err(|_| format!("{flag} expects a number: {v}"))
    }
}

/// 接続先のシェルに渡すので、名前は安全な文字だけに限る。
fn checked_name(name: String) -> Result<String, String> {
    let ok = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    ok.then_some(name).ok_or_else(|| "--sink-name may contain only letters, digits, '_' and '-'".into())
}

fn parse_connect(args: Vec<String>) -> Result<client::Options, String> {
    let mut it = Args(args.into_iter());
    let mut host = None;
    let mut opts = client::Options {
        host: String::new(),
        transport: None,
        remote_cmd: "micmux".into(),
        ssh_args: Vec::new(),
        bridge_args: Vec::new(),
        armed: false,
        max_seconds: 120,
        source: capture::Source::Native,
        bell: true,
        background: false,
        watch_pid: None,
        log_path: None,
        mux: false,
    };
    let mut capture_cmd = None;
    let mut capture_rate = resample::TARGET_RATE;
    while let Some(a) = it.0.next() {
        match a.as_str() {
            "--armed" => opts.armed = true,
            "--no-bell" => opts.bell = false,
            "--max-seconds" => opts.max_seconds = it.number(&a)?,
            "--capture-cmd" => capture_cmd = Some(it.value(&a)?),
            "--capture-rate" => capture_rate = it.number(&a)?,
            "--remote-cmd" => opts.remote_cmd = it.value(&a)?,
            "--ssh-arg" => opts.ssh_args.push(it.value(&a)?),
            "--transport" => opts.transport = Some(it.value(&a)?),
            "--sink-name" => {
                opts.bridge_args.push(a.clone());
                opts.bridge_args.push(checked_name(it.value(&a)?)?);
            }
            "--set-default" => opts.bridge_args.push(a),
            "--background" => opts.background = true,
            "--mux" => opts.mux = true,
            "--watch-pid" => {
                let pid = it.number(&a)?;
                if !control::valid_watch_pid(pid) {
                    return Err(format!("--watch-pid needs a pid of 2 or greater: {pid}"));
                }
                opts.watch_pid = Some(pid);
            }
            "--log" => opts.log_path = Some(it.value(&a)?.into()),
            _ if a.starts_with('-') => return Err(format!("unknown option: {a}")),
            _ if host.is_none() => host = Some(a),
            _ => return Err(format!("unexpected argument: {a}")),
        }
    }
    if let Some(cmd) = capture_cmd {
        opts.source = capture::Source::Command { cmd, rate: capture_rate };
    }
    opts.host = match (host, &opts.transport) {
        (Some(h), _) => h,
        (None, Some(_)) => "(transport)".into(),
        (None, None) => return Err(format!("missing <host>\n\n{USAGE}")),
    };
    Ok(opts)
}

fn parse_debug(args: Vec<String>) -> Result<(Option<String>, String, capture::Source), String> {
    let mut it = Args(args.into_iter());
    let (mut host, mut remote, mut cmd, mut rate) = (None, "micmux".to_string(), None, resample::TARGET_RATE);
    while let Some(a) = it.0.next() {
        match a.as_str() {
            "--remote-cmd" => remote = it.value(&a)?,
            "--capture-cmd" => cmd = Some(it.value(&a)?),
            "--capture-rate" => rate = it.number(&a)?,
            _ if a.starts_with('-') => return Err(format!("unknown option: {a}")),
            _ => host = Some(a),
        }
    }
    let source = match cmd {
        Some(cmd) => capture::Source::Command { cmd, rate },
        None => capture::Source::Native,
    };
    Ok((host, remote, source))
}

fn parse_bridge(args: Vec<String>) -> Result<bridge::Options, String> {
    let mut it = Args(args.into_iter());
    let mut opts = bridge::Options::default();
    while let Some(a) = it.0.next() {
        match a.as_str() {
            "--sink-name" => opts.sink_name = checked_name(it.value(&a)?)?,
            "--set-default" => opts.set_default = true,
            "--stop-holdoff-ms" => opts.stop_holdoff = Duration::from_millis(it.number(&a)?),
            "--latency-ms" => opts.latency_ms = it.number(&a)?,
            "--idle-timeout-ms" => opts.idle_timeout = Duration::from_millis(it.number(&a)?),
            _ => return Err(format!("unknown option: {a}")),
        }
    }
    Ok(opts)
}

/// 接続先の音声環境の点検。アプリが仮想マイクに辿り着けるかを順に見る。
fn doctor() -> Result<(), String> {
    let run = |cmd: &str, args: &[&str]| -> Option<String> {
        let out = Command::new(cmd).args(args).env("LC_ALL", "C").output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let mut failed = false;
    let mut report = |ok: bool, required: bool, what: &str, hint: &str| {
        let mark = if ok { "ok  " } else if required { "fail" } else { "warn" };
        println!("[{mark}] {what}");
        if !ok {
            println!("       → {hint}");
            failed |= required;
        }
    };

    let info = run("pactl", &["info"]);
    report(
        info.is_some(),
        true,
        "sound server (PulseAudio / pipewire-pulse) is reachable",
        "install pulseaudio or pipewire-pulse and start it as a user service",
    );
    if let Some(info) = &info {
        for key in ["Server Name:", "Default Source:"] {
            if let Some(l) = info.lines().find(|l| l.starts_with(key)) {
                println!("       {l}");
            }
        }
    }
    report(
        run("pacat", &["--version"]).is_some(),
        true,
        "pacat is installed (used to feed the virtual mic)",
        "install pulseaudio-utils",
    );
    let plugin = ["/usr/lib/x86_64-linux-gnu", "/usr/lib/aarch64-linux-gnu", "/usr/lib64", "/usr/lib"]
        .iter()
        .any(|d| {
            ["libasound_module_pcm_pulse.so", "libasound_module_pcm_pipewire.so"]
                .iter()
                .any(|f| std::path::Path::new(d).join("alsa-lib").join(f).exists())
        });
    report(
        plugin,
        false,
        "ALSA plugin for the sound server is installed (needed by apps that record via ALSA, such as Claude Code)",
        "install libasound2-plugins (PulseAudio) or pipewire-alsa (PipeWire)",
    );
    let cards = std::fs::read_to_string("/proc/asound/cards")
        .map(|s| !s.trim().is_empty() && !s.contains("no soundcards"))
        .unwrap_or(false);
    report(
        cards || run("arecord", &["--version"]).is_some(),
        false,
        "a sound card or arecord is present (Claude Code needs one to choose a recording method)",
        "on a VM without a sound card, install alsa-utils (recording then goes through arecord)",
    );
    if failed {
        Err("required checks failed".into())
    } else {
        Ok(())
    }
}
