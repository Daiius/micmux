//! 接続元でのマイク取得。
//!
//! 録音セッションごとに取得用のスレッドを 1 本立て、止めるときはそのスレッドごと終わらせる。
//! 待機中はデバイスを開かないので、OS のマイク使用中表示も消える。
//!
//! 取得方法は 2 つ:
//! - native: cpal で既定の入力デバイスを開く（macOS では Core Audio）
//! - command: 任意のコマンドの stdout を s16le モノラルとして読む（`rec`, `ffmpeg` など）

use crate::protocol::{end_reason, write_msg, Msg};
use std::io::{self, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// 1 フレームの長さ。短いほど遅延は減るが SSH のパケット数が増える。
const FRAME_MS: u32 = 20;

pub type Writer = Arc<Mutex<Box<dyn Write + Send>>>;

#[derive(Clone, Debug)]
pub enum Source {
    Native,
    /// `sh -c` で実行するコマンドと、その出力のサンプルレート。
    Command { cmd: String, rate: u32 },
}

#[derive(Default)]
pub struct Stats {
    pub bytes_sent: AtomicU64,
    /// 送信が追いつかず捨てたサンプル数（native のみ）。
    pub dropped_samples: AtomicU64,
    /// 取得開始の指示から最初の PCM を送るまで (ms)。
    pub first_audio_ms: AtomicU64,
    /// 送ったサンプルの最大振幅 (0〜32768)。0 のままなら完全な無音。
    pub peak: AtomicU64,
}

/// 最大振幅を人が読める形にする。
pub fn describe_peak(peak: u64) -> String {
    if peak == 0 {
        return "0 (完全な無音。マイクの許可が下りていない可能性)".into();
    }
    format!("{:.0} dBFS", 20.0 * (peak as f64 / 32768.0).log10())
}

/// マイクを `duration` だけ開いて、取れた音の概要を返す。通信はしない。
pub fn mic_test(source: &Source, duration: std::time::Duration) -> Result<String, String> {
    struct Shared(Arc<Mutex<Vec<u8>>>);
    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let buf = Arc::new(Mutex::new(Vec::new()));
    let failure = Arc::new(Mutex::new(None));
    let writer: Writer = Arc::new(Mutex::new(Box::new(Shared(buf.clone()))));
    let capture = {
        let failure = failure.clone();
        Capture::start(source, 1, writer, move |r| *failure.lock().unwrap() = r.err())
    };
    thread::sleep(duration);
    let stats = capture.stop();
    if let Some(e) = failure.lock().unwrap().take() {
        return Err(e);
    }
    let bytes = buf.lock().unwrap().clone();
    let (mut rate, mut samples) = (0u32, 0u64);
    let mut reader = &bytes[..];
    while let Ok(Some(msg)) = crate::protocol::read_msg(&mut reader) {
        match msg {
            Msg::AudioBegin { rate: r, .. } => rate = r,
            Msg::Audio { pcm, .. } => samples += pcm.len() as u64 / 2,
            _ => {}
        }
    }
    if rate == 0 || samples == 0 {
        return Err("音声が 1 サンプルも取れませんでした".into());
    }
    Ok(format!(
        "{rate} Hz で {:.2} 秒ぶん取得、最初の音声まで {} ms、最大音量 {}",
        samples as f64 / rate as f64,
        stats.first_audio_ms.load(Ordering::Relaxed),
        describe_peak(stats.peak.load(Ordering::Relaxed)),
    ))
}

pub struct Capture {
    pub session: u32,
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    thread: Option<JoinHandle<()>>,
    stats: Arc<Stats>,
}

impl Capture {
    /// 取得を始める。`on_exit` は取得スレッドが自分から終わったとき（エラーなど）に呼ばれる。
    pub fn start(
        source: &Source,
        session: u32,
        writer: Writer,
        on_exit: impl FnOnce(Result<(), String>) + Send + 'static,
    ) -> Capture {
        let stop = Arc::new(AtomicBool::new(false));
        let child = Arc::new(Mutex::new(None));
        let stats = Arc::new(Stats::default());
        let ctx = Ctx {
            session,
            writer,
            stop: stop.clone(),
            child: child.clone(),
            stats: stats.clone(),
            began: Instant::now(),
        };
        let source = source.clone();
        let thread = thread::spawn(move || {
            let result = match &source {
                Source::Native => run_native(&ctx),
                Source::Command { cmd, rate } => run_command(&ctx, cmd, *rate),
            };
            let stopped = ctx.stop.load(Ordering::SeqCst);
            let reason = match (&result, stopped) {
                (_, true) | (Ok(()), _) => end_reason::STOPPED,
                (Err(_), false) => end_reason::CAPTURE_ERROR,
            };
            let _ = ctx.send(&Msg::AudioEnd { session: ctx.session, reason });
            if !stopped {
                on_exit(result);
            }
        });
        Capture { session, stop, child, thread: Some(thread), stats }
    }

    /// 取得を止め、デバイスが閉じるまで待つ。
    pub fn stop(mut self) -> Arc<Stats> {
        self.stop.store(true, Ordering::SeqCst);
        kill_child(&self.child);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        self.stats.clone()
    }
}

/// 取得コマンドが置かれていれば取り出して止める。stop() と取得スレッドのどちらが先でも 1 回だけ効く。
fn kill_child(slot: &Mutex<Option<Child>>) {
    let child = slot.lock().unwrap().take();
    if let Some(mut c) = child {
        let _ = c.kill();
        let _ = c.wait();
    }
}

struct Ctx {
    session: u32,
    writer: Writer,
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    stats: Arc<Stats>,
    began: Instant,
}

impl Ctx {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    fn send(&self, msg: &Msg) -> io::Result<()> {
        write_msg(&mut *self.writer.lock().unwrap(), msg)
    }

    fn send_pcm(&self, samples: &[i16]) -> Result<(), String> {
        let mut pcm = Vec::with_capacity(samples.len() * 2);
        for s in samples {
            pcm.extend_from_slice(&s.to_le_bytes());
        }
        self.send_bytes(pcm)
    }

    fn send_bytes(&self, pcm: Vec<u8>) -> Result<(), String> {
        if self.stats.bytes_sent.load(Ordering::Relaxed) == 0 {
            self.stats
                .first_audio_ms
                .store(self.began.elapsed().as_millis() as u64, Ordering::Relaxed);
        }
        self.stats.bytes_sent.fetch_add(pcm.len() as u64, Ordering::Relaxed);
        let peak = pcm
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]).unsigned_abs() as u64)
            .max()
            .unwrap_or(0);
        self.stats.peak.fetch_max(peak, Ordering::Relaxed);
        self.send(&Msg::Audio { session: self.session, pcm }).map_err(|e| format!("送信に失敗: {e}"))
    }
}

fn run_command(ctx: &Ctx, cmd: &str, rate: u32) -> Result<(), String> {
    let mut child = Command::new("sh")
        .args(["-c", cmd])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("取得コマンドを起動できません: {e}"))?;
    let mut out = child.stdout.take().expect("piped stdout");
    *ctx.child.lock().unwrap() = Some(child);
    if ctx.stopped() {
        // 起動と停止が行き違った場合。stop() が child を見る前だったかもしれないので自分で片付ける
        kill_child(&ctx.child);
        return Ok(());
    }

    ctx.send(&Msg::AudioBegin { session: ctx.session, rate, channels: 1 })
        .map_err(|e| format!("送信に失敗: {e}"))?;
    let frame_bytes = (rate * FRAME_MS / 1000) as usize * 2;
    let mut buf = vec![0u8; frame_bytes];
    let mut filled = 0;
    while !ctx.stopped() {
        match out.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("取得コマンドの出力を読めません: {e}")),
        }
        if filled == frame_bytes {
            ctx.send_bytes(buf.clone())?;
            filled = 0;
        }
    }
    // 読む側を先に閉じる。止める指示で抜けた場合、child はまだ録音を続けていて、読まれない
    // パイプが埋まると書き込みで止まったまま終わらなくなる
    drop(out);
    // ロックは取り出すときだけ握る。握ったまま待つと、stop() がロック待ちで止まる
    let child = ctx.child.lock().unwrap().take();
    let status = child.map(|c| reap(ctx, c));
    if ctx.stopped() {
        return Ok(());
    }
    match status {
        Some(Ok(s)) if s.success() => Ok(()),
        Some(Ok(s)) => Err(format!("取得コマンドが終了しました ({s})")),
        _ => Err("取得コマンドが終了しました".into()),
    }
}

/// 出力を読み終えた取得コマンドの終了を待つ。
///
/// child はもう stop() からは見えないので、止める指示が出ていればここで kill する。
/// stdout だけ閉じて動き続けるコマンドもあるので、指示が無くても長くは待たない。
fn reap(ctx: &Ctx, mut child: Child) -> io::Result<std::process::ExitStatus> {
    let grace = Instant::now() + Duration::from_millis(500);
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if ctx.stopped() || Instant::now() >= grace {
            let _ = child.kill();
            return child.wait();
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(not(any(target_os = "macos", feature = "native")))]
fn run_native(_ctx: &Ctx) -> Result<(), String> {
    Err("このビルドには内蔵のマイク取得が入っていません。--capture-cmd を指定するか、--features native でビルドしてください".into())
}

#[cfg(any(target_os = "macos", feature = "native"))]
fn run_native(ctx: &Ctx) -> Result<(), String> {
    use crate::resample::Decimator;
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender, TrySendError};
    use std::time::Duration;

    /// コールバックから送信側へ渡す。コールバック内では通信もロック待ちもしない。
    fn build<T>(
        device: &cpal::Device,
        config: &cpal::StreamConfig,
        tx: SyncSender<Vec<i16>>,
        stats: Arc<Stats>,
    ) -> Result<cpal::Stream, cpal::BuildStreamError>
    where
        T: cpal::SizedSample,
        f32: cpal::FromSample<T>,
    {
        let channels = config.channels.max(1) as usize;
        device.build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                // モノラルへ混ぜて s16 にする
                let mono: Vec<i16> = data
                    .chunks(channels)
                    .map(|frame| {
                        let sum: f32 =
                            frame.iter().map(|&s| <f32 as cpal::Sample>::from_sample(s)).sum();
                        (sum / frame.len() as f32 * 32767.0).clamp(-32768.0, 32767.0) as i16
                    })
                    .collect();
                let n = mono.len() as u64;
                if let Err(TrySendError::Full(_)) = tx.try_send(mono) {
                    stats.dropped_samples.fetch_add(n, Ordering::Relaxed);
                }
            },
            |e| eprintln!("micmux: 入力ストリームのエラー: {e}"),
            None,
        )
    }

    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or("入力デバイスがありません（マイクのアクセス権限も確認してください）")?;
    let supported = device
        .default_input_config()
        .map_err(|e| format!("入力デバイスの設定を取得できません: {e}"))?;
    let in_rate = supported.sample_rate().0;
    let config: cpal::StreamConfig = supported.config();

    // 約 1.3 秒ぶん溜まったら古い順ではなく新しい方を捨てる（数は記録する）
    let (tx, rx) = sync_channel::<Vec<i16>>(64);
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => build::<f32>(&device, &config, tx, ctx.stats.clone()),
        cpal::SampleFormat::I16 => build::<i16>(&device, &config, tx, ctx.stats.clone()),
        cpal::SampleFormat::U16 => build::<u16>(&device, &config, tx, ctx.stats.clone()),
        cpal::SampleFormat::I32 => build::<i32>(&device, &config, tx, ctx.stats.clone()),
        other => return Err(format!("未対応のサンプル形式です: {other:?}")),
    }
    .map_err(|e| format!("入力ストリームを開けません: {e}"))?;
    stream.play().map_err(|e| format!("入力ストリームを開始できません: {e}"))?;

    let mut decimator = Decimator::for_rate(in_rate);
    let out_rate = decimator.out_rate(in_rate);
    ctx.send(&Msg::AudioBegin { session: ctx.session, rate: out_rate, channels: 1 })
        .map_err(|e| format!("送信に失敗: {e}"))?;

    let frame = (out_rate * FRAME_MS / 1000) as usize;
    let mut pending: Vec<i16> = Vec::with_capacity(frame * 4);
    while !ctx.stopped() {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(chunk) => {
                decimator.process(&chunk, &mut pending);
                while pending.len() >= frame {
                    ctx.send_pcm(&pending[..frame])?;
                    pending.drain(..frame);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Err("入力ストリームが終了しました".into()),
        }
    }
    drop(stream); // ここでデバイスが閉じる
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;
    use std::time::Duration;

    fn sink() -> Writer {
        Arc::new(Mutex::new(Box::new(io::sink())))
    }

    fn command(cmd: &str) -> Source {
        Source::Command { cmd: cmd.into(), rate: 16_000 }
    }

    /// 出力を出し続けているコマンドでも、stop() はすぐに戻る。
    #[test]
    fn stop_returns_while_command_is_producing() {
        for _ in 0..20 {
            let capture = Capture::start(&command("exec cat /dev/zero"), 1, sink(), |_| {});
            thread::sleep(Duration::from_millis(5));
            let t = Instant::now();
            capture.stop();
            assert!(t.elapsed() < Duration::from_secs(2), "stop took {:?}", t.elapsed());
        }
    }

    /// stdout を閉じたまま動き続けるコマンドでも、取得は終わり、stop() も止まらない。
    #[test]
    fn command_closing_stdout_does_not_hang() {
        let (tx, rx) = channel();
        let capture = Capture::start(&command("exec 1>&-; exec sleep 30"), 1, sink(), move |r| {
            let _ = tx.send(r);
        });
        let result = rx.recv_timeout(Duration::from_secs(5)).expect("取得が終わらない");
        assert!(result.is_err());
        let t = Instant::now();
        capture.stop();
        assert!(t.elapsed() < Duration::from_secs(2), "stop took {:?}", t.elapsed());
    }
}
