//! What the server and a Quick Sync helper say to each other: length-prefixed binary
//! messages, requests on the helper's stdin, replies on its stdout. A message is a
//! little-endian `u32` length, then that many bytes: a kind byte and its fields. Every
//! length and count is capped, and the parsing has no `unsafe`, so a helper that writes
//! garbage costs the server an error, nothing more.

use crate::encoder::Codec;

/// The most a message may carry: a few IDRs' worth at 4096x4096.
pub const MAX_MESSAGE: usize = 32 << 20;
/// The most access units one reply may carry; a frame yields at most one.
pub const MAX_UNITS: usize = 4;
/// The longest text (an error, an implementation name) a reply may carry.
pub const MAX_TEXT: usize = 1024;

const OPEN: u8 = 1;
const FRAME: u8 = 2;
const RATE: u8 = 3;
const OPENED: u8 = 0x81;
const DONE: u8 = 0x82;
const RATE_SET: u8 = 0x83;
const ERROR: u8 = 0x84;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rate {
    pub max_kbps: u32,
    pub buf_kbit: u32,
    pub fps: u32,
    pub quality: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Open {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub rate: Rate,
    /// Bytes per frame slot in the shared section: an I420 picture at the session's size.
    pub slot_size: u32,
    pub slots: u8,
    /// The section's handle in the helper (Windows; 0 where there is none).
    pub section: u64,
    /// The DXGI adapter to open on.
    pub luid: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    Open(Open),
    /// Encode the picture in `slot`; `key` asks for an IDR.
    Frame {
        slot: u8,
        pts: i64,
        key: bool,
    },
    Rate(Rate),
}

/// One access unit: whether it is a key frame, and its bytes.
#[derive(Clone, Debug, PartialEq)]
pub struct Unit {
    pub key: bool,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Reply {
    Opened {
        low_power: bool,
        name: String,
    },
    /// The frame in `slot` is coded; its units, none if it was skipped.
    Done {
        slot: u8,
        units: Vec<Unit>,
    },
    RateSet,
    /// The request failed; `stopped`: the device hung, failed or was lost.
    Error {
        stopped: bool,
        reason: String,
    },
}

fn codec_byte(c: Codec) -> u8 {
    match c {
        Codec::Hevc => 2,
        _ => 1,
    }
}

/// A message with its length in front.
fn framed(body: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend(body);
    out
}

fn put_rate(b: &mut Vec<u8>, r: &Rate) {
    b.extend_from_slice(&r.max_kbps.to_le_bytes());
    b.extend_from_slice(&r.buf_kbit.to_le_bytes());
    b.extend_from_slice(&r.fps.to_le_bytes());
    b.extend_from_slice(&r.quality.to_le_bytes());
}

fn put_text(b: &mut Vec<u8>, s: &str) {
    let mut end = s.len().min(MAX_TEXT);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    b.extend_from_slice(&(end as u16).to_le_bytes());
    b.extend_from_slice(&s.as_bytes()[..end]);
}

impl Request {
    pub fn bytes(&self) -> Vec<u8> {
        let mut b = Vec::new();
        match self {
            Request::Open(o) => {
                b.push(OPEN);
                b.push(codec_byte(o.codec));
                for v in [o.width, o.height, o.fps] {
                    b.extend_from_slice(&v.to_le_bytes());
                }
                put_rate(&mut b, &o.rate);
                b.extend_from_slice(&o.slot_size.to_le_bytes());
                b.push(o.slots);
                b.extend_from_slice(&o.section.to_le_bytes());
                b.extend_from_slice(&o.luid.to_le_bytes());
            }
            Request::Frame { slot, pts, key } => {
                b.push(FRAME);
                b.push(*slot);
                b.extend_from_slice(&pts.to_le_bytes());
                b.push(*key as u8);
            }
            Request::Rate(r) => {
                b.push(RATE);
                put_rate(&mut b, r);
            }
        }
        framed(b)
    }

    /// A request's body (the bytes after its length).
    pub fn parse(body: &[u8]) -> Result<Request, String> {
        let mut c = Cursor::new(body);
        let r = match c.u8()? {
            OPEN => {
                let codec = match c.u8()? {
                    1 => Codec::H264,
                    2 => Codec::Hevc,
                    v => return Err(format!("unknown codec {v}")),
                };
                Request::Open(Open {
                    codec,
                    width: c.u32()?,
                    height: c.u32()?,
                    fps: c.u32()?,
                    rate: c.rate()?,
                    slot_size: c.u32()?,
                    slots: c.u8()?,
                    section: c.u64()?,
                    luid: c.u64()?,
                })
            }
            FRAME => Request::Frame {
                slot: c.u8()?,
                pts: c.u64()? as i64,
                key: c.flag()?,
            },
            RATE => Request::Rate(c.rate()?),
            k => return Err(format!("unknown request {k:#x}")),
        };
        c.end()?;
        Ok(r)
    }
}

impl Reply {
    pub fn bytes(&self) -> Vec<u8> {
        let mut b = Vec::new();
        match self {
            Reply::Opened { low_power, name } => {
                b.push(OPENED);
                b.push(*low_power as u8);
                put_text(&mut b, name);
            }
            Reply::Done { slot, units } => {
                b.push(DONE);
                b.push(*slot);
                b.push(units.len() as u8);
                for u in units {
                    b.push(u.key as u8);
                    b.extend_from_slice(&(u.data.len() as u32).to_le_bytes());
                    b.extend_from_slice(&u.data);
                }
            }
            Reply::RateSet => b.push(RATE_SET),
            Reply::Error { stopped, reason } => {
                b.push(ERROR);
                b.push(*stopped as u8);
                put_text(&mut b, reason);
            }
        }
        framed(b)
    }

    /// A reply's body (the bytes after its length).
    pub fn parse(body: &[u8]) -> Result<Reply, String> {
        let mut c = Cursor::new(body);
        let r = match c.u8()? {
            OPENED => Reply::Opened {
                low_power: c.flag()?,
                name: c.text()?,
            },
            DONE => {
                let slot = c.u8()?;
                let n = c.u8()? as usize;
                if n > MAX_UNITS {
                    return Err(format!("{n} units in one reply"));
                }
                let mut units = Vec::with_capacity(n);
                for _ in 0..n {
                    let key = c.flag()?;
                    let len = c.u32()? as usize;
                    units.push(Unit {
                        key,
                        data: c.bytes(len)?.to_vec(),
                    });
                }
                Reply::Done { slot, units }
            }
            RATE_SET => Reply::RateSet,
            ERROR => Reply::Error {
                stopped: c.flag()?,
                reason: c.text()?,
            },
            k => return Err(format!("unknown reply {k:#x}")),
        };
        c.end()?;
        Ok(r)
    }
}

/// Collects bytes as they arrive and cuts them into message bodies.
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
}

impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The next whole message body, if one is in; an error for a length over the cap.
    pub fn next(&mut self) -> Result<Option<Vec<u8>>, String> {
        let Some(head) = self.buf.get(..4) else {
            return Ok(None);
        };
        let len = u32::from_le_bytes([head[0], head[1], head[2], head[3]]) as usize;
        if len == 0 || len > MAX_MESSAGE {
            return Err(format!("a {len}-byte message (at most {MAX_MESSAGE})"));
        }
        if self.buf.len() < 4 + len {
            return Ok(None);
        }
        let body = self.buf[4..4 + len].to_vec();
        self.buf.drain(..4 + len);
        Ok(Some(body))
    }
}

struct Cursor<'a> {
    b: &'a [u8],
}

impl<'a> Cursor<'a> {
    fn new(b: &'a [u8]) -> Cursor<'a> {
        Cursor { b }
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8], String> {
        if n > self.b.len() {
            return Err(format!("a message ends {} bytes short", n - self.b.len()));
        }
        let (head, rest) = self.b.split_at(n);
        self.b = rest;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.bytes(1)?[0])
    }

    fn flag(&mut self) -> Result<bool, String> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            v => Err(format!("{v} where a flag belongs")),
        }
    }

    fn u32(&mut self) -> Result<u32, String> {
        let b = self.bytes(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64, String> {
        let b = self.bytes(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }

    fn rate(&mut self) -> Result<Rate, String> {
        Ok(Rate {
            max_kbps: self.u32()?,
            buf_kbit: self.u32()?,
            fps: self.u32()?,
            quality: f32::from_bits(self.u32()?),
        })
    }

    fn text(&mut self) -> Result<String, String> {
        let b = self.bytes(2)?;
        let n = u16::from_le_bytes([b[0], b[1]]) as usize;
        if n > MAX_TEXT {
            return Err(format!("a {n}-byte text (at most {MAX_TEXT})"));
        }
        Ok(String::from_utf8_lossy(self.bytes(n)?).into_owned())
    }

    fn end(&self) -> Result<(), String> {
        match self.b.len() {
            0 => Ok(()),
            n => Err(format!("{n} bytes after a message")),
        }
    }
}
