//! 接続先から届いた文字列を、接続元の端末やログに出す前に無害にする。
//!
//! 接続先は無条件には信頼しない。届いた文字列をそのまま出すと、
//! - 端末の制御シーケンス（タイトルの書き換え、クリップボードへの書き込みなど）を流し込まれる
//! - 長い文字列や大量の出力で、裏で動くときのログがディスクを使い尽くす
//!
//! ので、制御文字を落とし、1 行の長さと行数を制限する。

use std::time::{Duration, Instant};

/// 接続先由来の 1 行に出す文字数の上限。超えた分は捨てて「…」を付ける。
pub const MAX_LINE_CHARS: usize = 300;

/// 端末が解釈しうる制御文字か。C0（タブを除く）、DEL、C1。改行の扱いは呼び出し側で決める。
fn is_unsafe(c: char) -> bool {
    c.is_control() && c != '\t'
}

/// 1 行の表示用にする。制御文字を落とし（改行は空白にする）、`max` 文字で切る。
pub fn line(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut n = 0;
    for c in s.chars() {
        let c = if c == '\n' { ' ' } else { c };
        if is_unsafe(c) {
            continue;
        }
        if n == max {
            out.push('…');
            break;
        }
        out.push(c);
        n += 1;
    }
    out
}

/// 一定時間あたりの行数の上限。
pub struct RateLimit {
    per_window: u32,
    window: Duration,
    start: Option<Instant>,
    used: u32,
    dropped: u64,
}

impl RateLimit {
    pub const fn new(per_window: u32, window: Duration) -> RateLimit {
        RateLimit { per_window, window, start: None, used: 0, dropped: 0 }
    }

    /// 1 行出してよいか。窓が切り替わったとき、前の窓で捨てた行があればその数も返す。
    pub fn allow(&mut self, now: Instant) -> (bool, u64) {
        let start = *self.start.get_or_insert(now);
        let mut dropped = 0;
        if now.duration_since(start) >= self.window {
            self.start = Some(now);
            self.used = 0;
            dropped = std::mem::take(&mut self.dropped);
        }
        if self.used < self.per_window {
            self.used += 1;
            (true, dropped)
        } else {
            self.dropped += 1;
            (false, dropped)
        }
    }
}

/// 中継する接続先の出力の総量の上限。bridge の普段のログは 1 回の録音で数百バイトなので、
/// これを超えるのは異常とみなし、以降は捨てる。
const RELAY_TOTAL_MAX: usize = 1024 * 1024;

/// ssh（接続先）の stderr を、接続元の stderr へ中継するときの濾過。
///
/// 行単位で溜めずに、届いた分をすぐ出す。改行の無いまま入力を待つメッセージがあっても
/// 表示が遅れないようにするため。
pub struct Relay {
    at_line_start: bool,
    /// 今の行を捨てている（行数の上限を超えた、または総量の上限に達した）。
    skipping: bool,
    col: usize,
    truncated: bool,
    total: usize,
    closed: bool,
    limit: RateLimit,
    /// 読み取りの境目で切れた多バイト文字の前半。
    tail: Vec<u8>,
}

impl Default for Relay {
    fn default() -> Relay {
        Relay {
            at_line_start: true,
            skipping: false,
            col: 0,
            truncated: false,
            total: 0,
            closed: false,
            limit: RateLimit::new(60, Duration::from_secs(60)),
            tail: Vec::new(),
        }
    }
}

impl Relay {
    /// 読み取った分を渡し、出してよい文字列を受け取る。
    pub fn feed(&mut self, bytes: &[u8], now: Instant) -> String {
        let mut data = std::mem::take(&mut self.tail);
        data.extend_from_slice(bytes);
        let cut = complete_len(&data);
        self.tail = data.split_off(cut);
        let mut out = String::new();
        for c in String::from_utf8_lossy(&data).chars() {
            if self.at_line_start {
                self.at_line_start = false;
                (self.col, self.truncated) = (0, false);
                if self.total + out.len() >= RELAY_TOTAL_MAX {
                    if !self.closed {
                        self.closed = true;
                        out.push_str("micmux: 接続先の出力が多すぎるので、以降は表示しません\n");
                    }
                    self.skipping = true;
                } else {
                    let (ok, dropped) = self.limit.allow(now);
                    if dropped > 0 {
                        out.push_str(&format!("micmux: 接続先の出力が多すぎるので {dropped} 行を省きました\n"));
                    }
                    self.skipping = !ok;
                }
            }
            if c == '\n' {
                if !self.skipping {
                    out.push('\n');
                }
                self.at_line_start = true;
            } else if self.skipping || is_unsafe(c) {
                continue;
            } else if self.col < MAX_LINE_CHARS {
                out.push(c);
                self.col += 1;
            } else if !self.truncated {
                out.push('…');
                self.truncated = true;
            }
        }
        self.total += out.len();
        out
    }
}

/// 末尾に続きの届いていない多バイト文字があれば、その手前までの長さ。
fn complete_len(b: &[u8]) -> usize {
    for back in 1..=b.len().min(3) {
        let i = b.len() - back;
        let x = b[i];
        if x & 0xC0 == 0x80 {
            continue; // 継続バイト
        }
        let need = match x {
            0xF0..=0xFF => 4,
            0xE0..=0xEF => 3,
            0xC0..=0xDF => 2,
            _ => 1,
        };
        return if need > back { i } else { b.len() };
    }
    b.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_strips_controls_and_truncates() {
        assert_eq!(line("src\x1b]0;TITLE\x07\x1b[31mRED", 100), "src]0;TITLE[31mRED");
        assert_eq!(line("a\nb\r\tc\u{9b}d\u{7f}", 100), "a b\tcd");
        assert_eq!(line("日本語のエラー", 3), "日本語…");
        assert_eq!(line("abc", 3), "abc");
    }

    #[test]
    fn rate_limit_reports_dropped_lines_in_next_window() {
        let mut l = RateLimit::new(2, Duration::from_secs(60));
        let t = Instant::now();
        assert_eq!(l.allow(t), (true, 0));
        assert_eq!(l.allow(t), (true, 0));
        assert_eq!(l.allow(t), (false, 0));
        assert_eq!(l.allow(t), (false, 0));
        assert_eq!(l.allow(t + Duration::from_secs(61)), (true, 2));
    }

    #[test]
    fn relay_strips_controls_and_passes_partial_lines() {
        let mut r = Relay::default();
        let t = Instant::now();
        assert_eq!(r.feed(b"Enter passphrase: ", t), "Enter passphrase: ");
        assert_eq!(r.feed(b"\r\n\x1b]52;c;AAAA\x07ok\n", t), "\n]52;c;AAAAok\n");
    }

    #[test]
    fn relay_keeps_multibyte_chars_split_across_reads() {
        let mut r = Relay::default();
        let t = Instant::now();
        let bytes = "録音".as_bytes();
        let a = r.feed(&bytes[..4], t);
        let b = r.feed(&bytes[4..], t);
        assert_eq!(a + &b, "録音");
    }

    #[test]
    fn relay_truncates_long_lines() {
        let mut r = Relay::default();
        let out = r.feed(&[b'x'; 1000], Instant::now());
        assert_eq!(out.chars().count(), MAX_LINE_CHARS + 1);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn relay_limits_lines_per_minute_and_total() {
        let mut r = Relay::default();
        let t = Instant::now();
        let out = r.feed("l\n".repeat(100).as_bytes(), t);
        assert_eq!(out.lines().count(), 60);
        let out = r.feed(b"next\n", t + Duration::from_secs(61));
        assert_eq!(out, "micmux: 接続先の出力が多すぎるので 40 行を省きました\nnext\n");

        // 総量の上限を超えたら、行数に余裕があっても以降は出さない
        let mut r = Relay::default();
        let long = format!("{}\n", "x".repeat(MAX_LINE_CHARS));
        let mut shown = 0;
        for i in 0..20_000u64 {
            shown += r.feed(long.as_bytes(), t + Duration::from_secs(61 * i)).len();
        }
        assert!(shown < RELAY_TOTAL_MAX + 1024, "shown {shown}");
        assert!(r.closed);
    }
}
