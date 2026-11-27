//! 整数比のダウンサンプリング（例: 48 kHz → 16 kHz）。
//!
//! 単純な間引きは折り返し雑音を生むので、窓関数法の FIR ローパスを通してから間引く。
//! 44.1 kHz のように 16 kHz の整数倍でないレートは変換せずそのまま送り、
//! 受信側のサウンドサーバーのリサンプラーに任せる。

pub const TARGET_RATE: u32 = 16_000;

pub struct Decimator {
    factor: usize,
    taps: Vec<f32>,
    /// 直近の入力。先頭 `taps.len() - 1` 個は前回からの持ち越し。
    hist: Vec<f32>,
    /// 次に出力するサンプルの、hist 内での開始位置。
    phase: usize,
}

impl Decimator {
    /// `in_rate` を 16 kHz へ落とす係数を選ぶ。整数倍でなければ素通し。
    pub fn for_rate(in_rate: u32) -> Decimator {
        let factor = if in_rate > TARGET_RATE && in_rate % TARGET_RATE == 0 {
            (in_rate / TARGET_RATE) as usize
        } else {
            1
        };
        Decimator::new(factor)
    }

    pub fn new(factor: usize) -> Decimator {
        let factor = factor.max(1);
        let taps = if factor == 1 { vec![1.0] } else { lowpass(factor) };
        let hist = vec![0.0; taps.len() - 1];
        Decimator { factor, taps, hist, phase: 0 }
    }

    pub fn out_rate(&self, in_rate: u32) -> u32 {
        in_rate / self.factor as u32
    }

    pub fn process(&mut self, input: &[i16], out: &mut Vec<i16>) {
        if self.factor == 1 {
            out.extend_from_slice(input);
            return;
        }
        self.hist.extend(input.iter().map(|&s| s as f32));
        let n = self.taps.len();
        while self.phase + n <= self.hist.len() {
            let acc: f32 = self.hist[self.phase..self.phase + n]
                .iter()
                .zip(&self.taps)
                .map(|(x, h)| x * h)
                .sum();
            out.push(acc.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16);
            self.phase += self.factor;
        }
        // 次回に必要な分だけ残す
        let drop = self.phase.min(self.hist.len());
        self.hist.drain(..drop);
        self.phase -= drop;
    }
}

/// Blackman 窓の sinc ローパス。遮断周波数は出力ナイキストの 0.9 倍。
fn lowpass(factor: usize) -> Vec<f32> {
    let n = 24 * factor + 1;
    let fc = 0.45 / factor as f64; // 入力サンプルレートに対する比
    let mid = (n - 1) as f64 / 2.0;
    let mut taps: Vec<f64> = (0..n)
        .map(|i| {
            let x = i as f64 - mid;
            let sinc = if x == 0.0 {
                2.0 * fc
            } else {
                (2.0 * std::f64::consts::PI * fc * x).sin() / (std::f64::consts::PI * x)
            };
            let t = 2.0 * std::f64::consts::PI * i as f64 / (n - 1) as f64;
            let window = 0.42 - 0.5 * t.cos() + 0.08 * (2.0 * t).cos();
            sinc * window
        })
        .collect();
    let sum: f64 = taps.iter().sum();
    taps.iter_mut().for_each(|t| *t /= sum);
    taps.into_iter().map(|t| t as f32).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f64, rate: f64, len: usize) -> Vec<i16> {
        (0..len)
            .map(|i| (10_000.0 * (2.0 * std::f64::consts::PI * freq * i as f64 / rate).sin()) as i16)
            .collect()
    }

    fn rms(x: &[i16]) -> f64 {
        (x.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
    }

    /// 入力をばらばらの大きさに分けて流しても結果が変わらないことも兼ねて確かめる。
    fn run(input: &[i16], factor: usize, chunk: usize) -> Vec<i16> {
        let mut d = Decimator::new(factor);
        let mut out = Vec::new();
        for c in input.chunks(chunk) {
            d.process(c, &mut out);
        }
        out
    }

    #[test]
    fn passes_speech_band_and_rejects_aliases() {
        let pass = run(&sine(1_000.0, 48_000.0, 48_000), 3, 480);
        let stop = run(&sine(12_000.0, 48_000.0, 48_000), 3, 480);
        let reference = rms(&sine(1_000.0, 16_000.0, 16_000));
        let settled = |v: &[i16]| rms(&v[200..]);
        assert!((settled(&pass) / reference - 1.0).abs() < 0.02, "passband gain");
        assert!(settled(&stop) / reference < 0.003, "stopband leak {}", settled(&stop) / reference);
    }

    #[test]
    fn chunking_does_not_change_output() {
        let input = sine(700.0, 48_000.0, 9_601);
        let a = run(&input, 3, 1);
        let b = run(&input, 3, 4_096);
        assert_eq!(a, b);
        // 先頭は無音で埋めてあるので、入力 3 サンプルごとに 1 つ出る
        assert_eq!(a.len(), (input.len() - 1) / 3 + 1);
    }

    #[test]
    fn picks_factor_only_for_integer_ratios() {
        assert_eq!(Decimator::for_rate(48_000).out_rate(48_000), 16_000);
        assert_eq!(Decimator::for_rate(96_000).out_rate(96_000), 16_000);
        assert_eq!(Decimator::for_rate(44_100).out_rate(44_100), 44_100);
        assert_eq!(Decimator::for_rate(16_000).out_rate(16_000), 16_000);
    }
}
