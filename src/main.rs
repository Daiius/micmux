// 内蔵のマイク取得を含まないビルドでは、リサンプラーなど一部が使われない
#![cfg_attr(not(any(target_os = "macos", feature = "native")), allow(dead_code))]

mod bridge;
mod capture;
mod client;
mod debug;
mod protocol;
mod resample;

use std::process::{Command, ExitCode};
use std::time::Duration;

const USAGE: &str = "\
micmux — On-demand microphone forwarding over SSH

接続元 (マイクのある側):
  micmux connect <host> [options]   接続して録音要求を待つ
      --armed                 最初から録音を許可する（既定は disarmed）
      --max-seconds <n>       連続録音時間の上限（既定 120、0 で上限なし）
      --capture-cmd <cmd>     マイク取得に使うコマンド（s16le モノラルを stdout に出すもの）
                              例: 'rec -q -t raw -r 16000 -e signed -b 16 -c 1 -'
      --capture-rate <hz>     --capture-cmd の出力レート（既定 16000）
      --remote-cmd <path>     接続先での micmux の場所（既定 micmux）
      --ssh-arg <arg>         ssh に渡す追加の引数（複数回指定できる）
      --transport <cmd>       ssh の代わりに使う接続コマンド
      --sink-name <name>      仮想マイクの名前（既定 micmux → 録音デバイスは micmux.monitor）
      --set-default           接続中だけ仮想マイクを接続先の既定の録音デバイスにする
      --no-bell               断ったときにベルを鳴らさない
      --background            端末から切り離して裏で動く（ログは ~/.local/state/micmux/client.log）
      --watch-pid <pid>       このプロセスが終わったら自分も終わる
      --log <path>            --background のときのログの出力先
      --mux                   端末用の SSH 接続に相乗りする（認証が 1 回で済む。
                              ControlMaster / ControlPath / ControlPersist の設定が必要）
  micmux arm | disarm | toggle      動いている connect の録音許可を切り替える
  micmux status [--json]            動いている connect の状態を表示する
  micmux debug [<host>]             不具合調査用に、接続元の状況をまとめて表示する
      --remote-cmd / --capture-cmd / --capture-rate は connect と同じ

接続先 (アプリが動く側):
  micmux bridge [options]           connect が SSH 越しに起動する（手で実行する必要はない）
  micmux doctor                     接続先の音声環境を点検する
";

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = if args.is_empty() { String::new() } else { args.remove(0) };
    let result = match cmd.as_str() {
        "connect" => parse_connect(args).and_then(client::run),
        "bridge" => parse_bridge(args).and_then(bridge::run),
        "arm" | "disarm" | "toggle" => client::send_control(&cmd).map(|r| println!("{r}")),
        "status" => {
            let json = args.iter().any(|a| a == "--json");
            client::send_control(if json { "status-json" } else { "status" }).map(|r| println!("{r}"))
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
        other => Err(format!("不明なコマンド: {other}\n\n{USAGE}")),
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
        self.0.next().ok_or_else(|| format!("{flag} には値が必要です"))
    }

    fn number<T: std::str::FromStr>(&mut self, flag: &str) -> Result<T, String> {
        let v = self.value(flag)?;
        v.parse().map_err(|_| format!("{flag} の値が数値ではありません: {v}"))
    }
}

/// 接続先のシェルに渡すので、名前は安全な文字だけに限る。
fn checked_name(name: String) -> Result<String, String> {
    let ok = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    ok.then_some(name).ok_or_else(|| "--sink-name に使えるのは英数字と _ - だけです".into())
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
            "--watch-pid" => opts.watch_pid = Some(it.number(&a)?),
            "--log" => opts.log_path = Some(it.value(&a)?.into()),
            _ if a.starts_with('-') => return Err(format!("不明なオプション: {a}")),
            _ if host.is_none() => host = Some(a),
            _ => return Err(format!("余分な引数: {a}")),
        }
    }
    if let Some(cmd) = capture_cmd {
        opts.source = capture::Source::Command { cmd, rate: capture_rate };
    }
    opts.host = match (host, &opts.transport) {
        (Some(h), _) => h,
        (None, Some(_)) => "(transport)".into(),
        (None, None) => return Err(format!("接続先を指定してください\n\n{USAGE}")),
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
            _ if a.starts_with('-') => return Err(format!("不明なオプション: {a}")),
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
            _ => return Err(format!("不明なオプション: {a}")),
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
        let mark = if ok { "ok  " } else if required { "NG  " } else { "warn" };
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
        "サウンドサーバー (PulseAudio / pipewire-pulse) に接続できる",
        "pulseaudio か pipewire-pulse を入れて、ユーザーサービスとして起動する",
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
        "pacat がある（仮想マイクへの流し込みに使う）",
        "pulseaudio-utils を入れる",
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
        "ALSA からサウンドサーバーへ繋ぐプラグインがある（Claude Code など ALSA で録音するアプリに必要）",
        "libasound2-plugins (PulseAudio) か pipewire-alsa (PipeWire) を入れる",
    );
    let cards = std::fs::read_to_string("/proc/asound/cards")
        .map(|s| !s.trim().is_empty() && !s.contains("no soundcards"))
        .unwrap_or(false);
    report(
        cards || run("arecord", &["--version"]).is_some(),
        false,
        "サウンドカードがある、または arecord がある（Claude Code が録音方法を選ぶ条件）",
        "サウンドカードの無い VM では alsa-utils を入れる（arecord 経由で録音される）",
    );
    if failed {
        Err("必須の項目が満たされていません".into())
    } else {
        Ok(())
    }
}
