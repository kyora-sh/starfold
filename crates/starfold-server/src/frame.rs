//! Framed WebSocket messages (--framed), for hops that carry binary messages only: every
//! message is binary, its first byte the kind of what follows, 0 for binary and 1 for
//! UTF-8 text. The protocol inside is the one plain WebSockets carry (web/src/frame.ts).

use bytes::Bytes;
use std::fmt;

/// The largest WebSocket message either side takes, kind byte included.
pub const MAX_MESSAGE: usize = 64 << 20;

pub const BINARY: u8 = 0;
pub const TEXT: u8 = 1;

#[derive(Debug, PartialEq, Eq)]
pub enum Payload {
    Binary(Bytes),
    Text(String),
}

#[derive(Debug, PartialEq, Eq)]
pub enum FrameError {
    /// A message without even the kind byte.
    Empty,
    UnknownKind(u8),
    BadUtf8,
    TooLarge(usize),
    /// A text message where only binary ones are framed.
    Unframed,
    /// A message the WebSocket itself refused for its size, before it could be decoded.
    Oversized,
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::Empty => write!(f, "an empty framed message"),
            FrameError::UnknownKind(k) => write!(f, "unknown framed message kind {k}"),
            FrameError::BadUtf8 => write!(f, "a framed text message that is not UTF-8"),
            FrameError::TooLarge(n) => {
                write!(f, "a framed message of {n} bytes (at most {MAX_MESSAGE})")
            }
            FrameError::Unframed => write!(f, "a text message on a framed socket"),
            FrameError::Oversized => write!(f, "a message over {MAX_MESSAGE} bytes"),
        }
    }
}

impl FrameError {
    /// The WebSocket close code for a peer that sent this.
    pub fn close_code(&self) -> u16 {
        match self {
            FrameError::BadUtf8 => 1007,
            FrameError::TooLarge(_) | FrameError::Oversized => 1009,
            FrameError::Empty | FrameError::UnknownKind(_) | FrameError::Unframed => 1003,
        }
    }
}

fn encode(kind: u8, payload: &[u8]) -> Result<Bytes, FrameError> {
    let n = payload.len() + 1;
    if n > MAX_MESSAGE {
        return Err(FrameError::TooLarge(n));
    }
    let mut m = Vec::with_capacity(n);
    m.push(kind);
    m.extend_from_slice(payload);
    Ok(Bytes::from(m))
}

pub fn encode_binary(payload: &[u8]) -> Result<Bytes, FrameError> {
    encode(BINARY, payload)
}

pub fn encode_text(text: &str) -> Result<Bytes, FrameError> {
    encode(TEXT, text.as_bytes())
}

pub fn decode(m: Bytes) -> Result<Payload, FrameError> {
    if m.len() > MAX_MESSAGE {
        return Err(FrameError::TooLarge(m.len()));
    }
    match m.first() {
        None => Err(FrameError::Empty),
        Some(&BINARY) => Ok(Payload::Binary(m.slice(1..))),
        Some(&TEXT) => String::from_utf8(m[1..].to_vec())
            .map(Payload::Text)
            .map_err(|_| FrameError::BadUtf8),
        Some(&k) => Err(FrameError::UnknownKind(k)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let bin = [0x05, 0, 1, 2, 255];
        let m = encode_binary(&bin).unwrap();
        assert_eq!(&m[..], &[0, 0x05, 0, 1, 2, 255]);
        assert_eq!(decode(m), Ok(Payload::Binary(Bytes::copy_from_slice(&bin))));
        let text = r#"{"t":"hello","w":1280,"note":"größe ✓"}"#;
        let m = encode_text(text).unwrap();
        assert_eq!(m[0], TEXT);
        assert_eq!(decode(m), Ok(Payload::Text(text.into())));
    }

    #[test]
    fn empty_payloads_are_messages() {
        assert_eq!(&encode_binary(&[]).unwrap()[..], &[BINARY]);
        assert_eq!(&encode_text("").unwrap()[..], &[TEXT]);
        assert_eq!(
            decode(Bytes::from_static(&[BINARY])),
            Ok(Payload::Binary(Bytes::new()))
        );
        assert_eq!(
            decode(Bytes::from_static(&[TEXT])),
            Ok(Payload::Text(String::new()))
        );
        assert_eq!(decode(Bytes::new()), Err(FrameError::Empty));
    }

    #[test]
    fn a_leading_bom_is_text() {
        assert_eq!(
            decode(Bytes::from_static(&[TEXT, 0xef, 0xbb, 0xbf])),
            Ok(Payload::Text("\u{feff}".into()))
        );
        let text = "\u{feff}k,65,1";
        assert_eq!(
            decode(encode_text(text).unwrap()),
            Ok(Payload::Text(text.into()))
        );
    }

    #[test]
    fn unknown_kinds_and_bad_text_are_refused() {
        for k in [2u8, 0x11, 255] {
            assert_eq!(
                decode(Bytes::from(vec![k, 1, 2])),
                Err(FrameError::UnknownKind(k))
            );
        }
        assert_eq!(
            decode(Bytes::from_static(&[TEXT, 0xff, 0xfe])),
            Err(FrameError::BadUtf8)
        );
        assert_eq!(FrameError::UnknownKind(2).close_code(), 1003);
        assert_eq!(FrameError::BadUtf8.close_code(), 1007);
    }

    #[test]
    fn size_is_capped_with_the_kind_byte() {
        let max = vec![7u8; MAX_MESSAGE - 1];
        let m = encode_binary(&max).unwrap();
        assert_eq!(m.len(), MAX_MESSAGE);
        assert!(matches!(decode(m), Ok(Payload::Binary(b)) if b.len() == MAX_MESSAGE - 1));
        let over = vec![7u8; MAX_MESSAGE];
        assert_eq!(
            encode_binary(&over),
            Err(FrameError::TooLarge(MAX_MESSAGE + 1))
        );
        let mut big = vec![BINARY];
        big.extend_from_slice(&over);
        assert_eq!(
            decode(Bytes::from(big)),
            Err(FrameError::TooLarge(MAX_MESSAGE + 1))
        );
        assert_eq!(FrameError::TooLarge(0).close_code(), 1009);
    }
}
