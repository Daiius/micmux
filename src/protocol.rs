//! client と bridge が SSH の標準入出力上で交わすフレーム。
//!
//! ```text
//! +------+-------------+---------+
//! | type | length (LE) | payload |
//! |  u8  |     u32     |  bytes  |
//! +------+-------------+---------+
//! ```
//!
//! SSH が順序と完全性を保証するので、再送や順序番号は持たない。
//! 録音セッションの取り違えは `session` で防ぐ。

use std::io::{self, Read, Write};

pub const MAGIC: [u8; 4] = *b"MMUX";
pub const VERSION: u16 = 1;
/// 不正な長さで巨大なメモリを確保させないための上限。
pub const MAX_PAYLOAD: usize = 64 * 1024;

/// `AudioEnd` の理由。
pub mod end_reason {
    /// 取得を止めた（要求終了・disarm・時間上限のいずれでも）。
    pub const STOPPED: u8 = 0;
    /// armed でないので送らない。
    pub const DENIED: u8 = 1;
    /// マイクを取得できなかった。
    pub const CAPTURE_ERROR: u8 = 4;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    /// client → bridge。最初に必ず送る。
    Hello { version: u16 },
    /// bridge → client。`source` は仮想マイクの source 名。
    HelloAck { version: u16, source: String },
    /// bridge → client。録音要求が発生した。
    Start { session: u32 },
    /// bridge → client。録音要求が無くなった。
    Stop { session: u32 },
    /// client → bridge。これから送る PCM (s16le) の形式。
    AudioBegin { session: u32, rate: u32, channels: u8 },
    /// client → bridge。s16le の PCM。
    Audio { session: u32, pcm: Vec<u8> },
    /// client → bridge。このセッションの音声は終わり（または送らない）。
    AudioEnd { session: u32, reason: u8 },
    Ping { nonce: u64 },
    Pong { nonce: u64 },
    Error { message: String },
}

const T_HELLO: u8 = 1;
const T_HELLO_ACK: u8 = 2;
const T_START: u8 = 3;
const T_STOP: u8 = 4;
const T_AUDIO_BEGIN: u8 = 5;
const T_AUDIO: u8 = 6;
const T_AUDIO_END: u8 = 7;
const T_PING: u8 = 8;
const T_PONG: u8 = 9;
const T_ERROR: u8 = 10;

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

impl Msg {
    pub fn encode(&self) -> Vec<u8> {
        let mut p = Vec::new();
        let ty = match self {
            Msg::Hello { version } => {
                p.extend_from_slice(&MAGIC);
                p.extend_from_slice(&version.to_le_bytes());
                T_HELLO
            }
            Msg::HelloAck { version, source } => {
                p.extend_from_slice(&MAGIC);
                p.extend_from_slice(&version.to_le_bytes());
                p.extend_from_slice(source.as_bytes());
                T_HELLO_ACK
            }
            Msg::Start { session } => {
                p.extend_from_slice(&session.to_le_bytes());
                T_START
            }
            Msg::Stop { session } => {
                p.extend_from_slice(&session.to_le_bytes());
                T_STOP
            }
            Msg::AudioBegin { session, rate, channels } => {
                p.extend_from_slice(&session.to_le_bytes());
                p.extend_from_slice(&rate.to_le_bytes());
                p.push(*channels);
                T_AUDIO_BEGIN
            }
            Msg::Audio { session, pcm } => {
                p.extend_from_slice(&session.to_le_bytes());
                p.extend_from_slice(pcm);
                T_AUDIO
            }
            Msg::AudioEnd { session, reason } => {
                p.extend_from_slice(&session.to_le_bytes());
                p.push(*reason);
                T_AUDIO_END
            }
            Msg::Ping { nonce } => {
                p.extend_from_slice(&nonce.to_le_bytes());
                T_PING
            }
            Msg::Pong { nonce } => {
                p.extend_from_slice(&nonce.to_le_bytes());
                T_PONG
            }
            Msg::Error { message } => {
                let b = message.as_bytes();
                p.extend_from_slice(&b[..b.len().min(MAX_PAYLOAD)]);
                T_ERROR
            }
        };
        assert!(p.len() <= MAX_PAYLOAD, "payload too large: {}", p.len());
        let mut out = Vec::with_capacity(5 + p.len());
        out.push(ty);
        out.extend_from_slice(&(p.len() as u32).to_le_bytes());
        out.extend_from_slice(&p);
        out
    }

    /// ログ用の短い説明。種類と長さだけで、中身（接続先が決める文字列や音声）は含めない。
    pub fn summary(&self) -> String {
        let name = match self {
            Msg::Hello { .. } => "Hello",
            Msg::HelloAck { .. } => "HelloAck",
            Msg::Start { .. } => "Start",
            Msg::Stop { .. } => "Stop",
            Msg::AudioBegin { .. } => "AudioBegin",
            Msg::Audio { .. } => "Audio",
            Msg::AudioEnd { .. } => "AudioEnd",
            Msg::Ping { .. } => "Ping",
            Msg::Pong { .. } => "Pong",
            Msg::Error { .. } => "Error",
        };
        format!("{name} ({} bytes)", self.encode().len() - 5)
    }

    fn decode(ty: u8, p: &[u8]) -> io::Result<Msg> {
        let u32_at = |i: usize| -> io::Result<u32> {
            p.get(i..i + 4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
                .ok_or_else(|| invalid("short payload"))
        };
        let u64_at = |i: usize| -> io::Result<u64> {
            p.get(i..i + 8)
                .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
                .ok_or_else(|| invalid("short payload"))
        };
        let hello = || -> io::Result<u16> {
            if p.len() < 6 || p[..4] != MAGIC {
                return Err(invalid("bad magic (check for stray output on stdout)"));
            }
            Ok(u16::from_le_bytes([p[4], p[5]]))
        };
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        Ok(match ty {
            T_HELLO => Msg::Hello { version: hello()? },
            T_HELLO_ACK => Msg::HelloAck { version: hello()?, source: text(&p[6..]) },
            T_START => Msg::Start { session: u32_at(0)? },
            T_STOP => Msg::Stop { session: u32_at(0)? },
            T_AUDIO_BEGIN => Msg::AudioBegin {
                session: u32_at(0)?,
                rate: u32_at(4)?,
                channels: *p.get(8).ok_or_else(|| invalid("short payload"))?,
            },
            T_AUDIO => Msg::Audio { session: u32_at(0)?, pcm: p[4..].to_vec() },
            T_AUDIO_END => Msg::AudioEnd {
                session: u32_at(0)?,
                reason: *p.get(4).ok_or_else(|| invalid("short payload"))?,
            },
            T_PING => Msg::Ping { nonce: u64_at(0)? },
            T_PONG => Msg::Pong { nonce: u64_at(0)? },
            T_ERROR => Msg::Error { message: text(p) },
            other => return Err(invalid(format!("unknown message type {other}"))),
        })
    }
}

pub fn write_msg<W: Write>(w: &mut W, msg: &Msg) -> io::Result<()> {
    w.write_all(&msg.encode())?;
    w.flush()
}

/// 1 メッセージ読む。メッセージ境界でのきれいな EOF は `Ok(None)`。
/// 1 回の read で 1 メッセージが揃うとは仮定しない。
pub fn read_msg<R: Read>(r: &mut R) -> io::Result<Option<Msg>> {
    let mut head = [0u8; 5];
    let mut got = 0;
    while got < head.len() {
        match r.read(&mut head[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let len = u32::from_le_bytes(head[1..5].try_into().unwrap()) as usize;
    if len > MAX_PAYLOAD {
        return Err(invalid(format!("payload length {len} exceeds limit")));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Msg::decode(head[0], &payload).map(Some)
}

/// 読めたメッセージを順に `deliver` へ渡す。読めなくなるか、`deliver` が false を返したら戻る。
/// 戻り値は読めなくなった理由（メッセージ境界でのきれいな EOF や、`deliver` が断った場合は `None`）。
pub fn forward<R: Read>(r: &mut R, mut deliver: impl FnMut(Msg) -> bool) -> Option<io::Error> {
    loop {
        match read_msg(r) {
            Ok(Some(m)) => {
                if !deliver(m) {
                    return None;
                }
            }
            Ok(None) => return None,
            Err(e) => return Some(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1 バイトずつしか返さない reader。
    struct Trickle<'a>(&'a [u8]);
    impl Read for Trickle<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.0.is_empty() || buf.is_empty() {
                return Ok(0);
            }
            buf[0] = self.0[0];
            self.0 = &self.0[1..];
            Ok(1)
        }
    }

    fn samples() -> Vec<Msg> {
        vec![
            Msg::Hello { version: VERSION },
            Msg::HelloAck { version: VERSION, source: "micmux.monitor".into() },
            Msg::Start { session: 7 },
            Msg::Stop { session: 7 },
            Msg::AudioBegin { session: 7, rate: 16000, channels: 1 },
            Msg::Audio { session: 7, pcm: (0..=255u8).collect() },
            Msg::AudioEnd { session: 7, reason: end_reason::DENIED },
            Msg::Ping { nonce: u64::MAX },
            Msg::Pong { nonce: 42 },
            Msg::Error { message: "日本語のエラー".into() },
        ]
    }

    #[test]
    fn roundtrip_with_split_reads() {
        let msgs = samples();
        let bytes: Vec<u8> = msgs.iter().flat_map(|m| m.encode()).collect();
        let mut r = Trickle(&bytes);
        for m in &msgs {
            assert_eq!(read_msg(&mut r).unwrap().as_ref(), Some(m));
        }
        assert_eq!(read_msg(&mut r).unwrap(), None);
    }

    #[test]
    fn summary_omits_contents() {
        let m = Msg::Audio { session: 1, pcm: vec![0xAB; 60_000] };
        assert_eq!(m.summary(), "Audio (60004 bytes)");
        let e = Msg::Error { message: "\x1b]0;x\x07".into() };
        assert_eq!(e.summary(), "Error (6 bytes)");
    }

    #[test]
    fn rejects_oversized_length() {
        let mut bytes = vec![T_AUDIO];
        bytes.extend_from_slice(&(MAX_PAYLOAD as u32 + 1).to_le_bytes());
        assert_eq!(read_msg(&mut &bytes[..]).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_unknown_type_bad_magic_and_truncation() {
        let bytes = [200u8, 0, 0, 0, 0];
        assert!(read_msg(&mut &bytes[..]).is_err());

        // ログインシェルの出力などが混ざった場合
        let mut bad = vec![T_HELLO_ACK];
        bad.extend_from_slice(&6u32.to_le_bytes());
        bad.extend_from_slice(b"Welcom");
        assert!(read_msg(&mut &bad[..]).is_err());

        let full = Msg::Start { session: 1 }.encode();
        assert_eq!(
            read_msg(&mut &full[..full.len() - 1]).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        // payload が型に対して短い
        let short = [T_START, 2, 0, 0, 0, 1, 2];
        assert!(read_msg(&mut &short[..]).is_err());
    }
}
