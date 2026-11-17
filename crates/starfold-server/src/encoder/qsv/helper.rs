//! The Quick Sync helper process: the only place Intel's runtime is loaded and called. It
//! serves one session, one request at a time ([`super::proto`]): requests on stdin, replies
//! on stdout, frames read from the server's section. A device that stops answering is
//! reported and the helper leaves at once, closing nothing; the server ends the rest.

use super::driver::STOPPED;
use super::proto::{Open, Reply, Request, Unit};
use super::session::Session;
use crate::encoder::hw::sample_config;
use crate::encoder::{Backend, EncodeError, EncodeOptions, FrameView, RateConfig, VideoEncoder};
use std::io::{Read, Write};

/// The most a request may be: they carry fields, never pictures.
const MAX_REQUEST: usize = 4096;

/// The pictures the server shares: `slots` slots of `slot` bytes each.
pub trait Frames {
    fn bytes(&self) -> &[u8];
}

/// Exit codes.
pub const DONE: i32 = 0;
pub const BAD_REQUEST: i32 = 2;
pub const STOPPED_EXIT: i32 = 3;

fn read_request(input: &mut impl Read) -> Result<Option<Request>, String> {
    let mut head = [0u8; 4];
    match input.read_exact(&mut head) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(format!("reading a request: {e}")),
    }
    let len = u32::from_le_bytes(head) as usize;
    if len == 0 || len > MAX_REQUEST {
        return Err(format!("a {len}-byte request"));
    }
    let mut body = vec![0u8; len];
    input
        .read_exact(&mut body)
        .map_err(|e| format!("reading a request: {e}"))?;
    Request::parse(&body).map(Some)
}

fn reply(output: &mut impl Write, r: &Reply) -> bool {
    output
        .write_all(&r.bytes())
        .and_then(|_| output.flush())
        .is_ok()
}

fn error(e: &EncodeError, stopped: bool) -> Reply {
    Reply::Error {
        stopped: stopped || e.0.starts_with(STOPPED),
        reason: e.0.clone(),
    }
}

/// Serves the server until its stdin closes; returns the exit code. `map` gives the frame
/// section `Open` names, `open` the session (and its implementation's name) on `luid`.
pub fn serve(
    mut input: impl Read,
    mut output: impl Write,
    map: impl FnOnce(&Open) -> Result<Box<dyn Frames>, String>,
    open: impl FnOnce(&crate::encoder::EncoderConfig, u64) -> Result<(Session, String), EncodeError>,
) -> i32 {
    let o = match read_request(&mut input) {
        Ok(Some(Request::Open(o))) => o,
        Ok(None) => return DONE,
        Ok(Some(_)) => {
            eprintln!("the first request was not Open");
            return BAD_REQUEST;
        }
        Err(e) => {
            eprintln!("{e}");
            return BAD_REQUEST;
        }
    };
    let (w, h) = (o.width as usize, o.height as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let need = w * h + 2 * cw * ch;
    if o.width == 0 || o.height == 0 || o.slot_size as usize != need || o.slots == 0 {
        reply(
            &mut output,
            &Reply::Error {
                stopped: false,
                reason: format!(
                    "a {}x{} picture in {}-byte slots",
                    o.width, o.height, o.slot_size
                ),
            },
        );
        return BAD_REQUEST;
    }
    let frames = match map(&o) {
        Ok(f) => f,
        Err(reason) => {
            reply(
                &mut output,
                &Reply::Error {
                    stopped: false,
                    reason,
                },
            );
            return BAD_REQUEST;
        }
    };
    let mut cfg = sample_config(Backend::Qsv, o.codec, o.width, o.height, o.fps);
    cfg.rate = RateConfig {
        max_kbps: o.rate.max_kbps,
        buf_kbit: o.rate.buf_kbit,
        fps: o.rate.fps,
        quality: o.rate.quality,
    };
    let mut session = match open(&cfg, o.luid) {
        Ok((s, name)) => {
            if let Some(why) = &s.low_power_why {
                eprintln!("LowPower on: the runtime refused the full encoder ({why})");
            }
            let opened = Reply::Opened {
                low_power: s.low_power,
                name,
            };
            if !reply(&mut output, &opened) {
                return DONE;
            }
            s
        }
        Err(e) => {
            let r = error(&e, false);
            reply(&mut output, &r);
            return match r {
                Reply::Error { stopped: true, .. } => STOPPED_EXIT,
                _ => BAD_REQUEST,
            };
        }
    };
    loop {
        let req = match read_request(&mut input) {
            Ok(Some(r)) => r,
            Ok(None) => return DONE,
            Err(e) => {
                eprintln!("{e}");
                return BAD_REQUEST;
            }
        };
        let r = match req {
            Request::Open(_) => {
                eprintln!("a second Open");
                return BAD_REQUEST;
            }
            Request::Frame { slot, pts, key } => {
                let bytes = frames.bytes();
                let start = slot as usize * need;
                let Some(pic) = (slot < o.slots)
                    .then(|| bytes.get(start..start + need))
                    .flatten()
                else {
                    eprintln!("a frame in slot {slot}, out of the section");
                    return BAD_REQUEST;
                };
                let f = FrameView {
                    width: o.width,
                    height: o.height,
                    y: &pic[..w * h],
                    u: &pic[w * h..w * h + cw * ch],
                    v: &pic[w * h + cw * ch..],
                    stride_y: w,
                    stride_uv: cw,
                };
                let opts = EncodeOptions {
                    pts_ms: pts,
                    force_keyframe: key,
                    ..Default::default()
                };
                match session.encode(&f, opts) {
                    Ok(aus) => Reply::Done {
                        slot,
                        units: aus
                            .into_iter()
                            .map(|a| Unit {
                                key: a.keyframe,
                                data: a.data,
                            })
                            .collect(),
                    },
                    Err(e) => error(&e, session.stopped().is_some()),
                }
            }
            Request::Rate(r) => {
                let rate = RateConfig {
                    max_kbps: r.max_kbps,
                    buf_kbit: r.buf_kbit,
                    fps: r.fps,
                    quality: r.quality,
                };
                match session.set_rate(rate) {
                    Ok(()) => Reply::RateSet {
                        kbps: session.rate_in_force().unwrap_or(0),
                    },
                    Err(e) => error(&e, session.stopped().is_some()),
                }
            }
        };
        let stopped = matches!(r, Reply::Error { stopped: true, .. });
        if !reply(&mut output, &r) {
            return DONE;
        }
        if stopped {
            std::mem::forget(session);
            return STOPPED_EXIT;
        }
    }
}

/// `--encoder-helper qsv`: serves the server on stdin and stdout. With `--probe` after it,
/// runs the Quick Sync probe here instead and prints its report, as a probe child does.
#[cfg(windows)]
pub fn main(args: &[String]) -> i32 {
    use super::proc::Mapped;
    match args {
        [qsv] if qsv == "qsv" => {}
        [qsv, probe] if qsv == "qsv" && probe == "--probe" => {
            return crate::encoder::isolate::report_main(Backend::Qsv, super::probe_here);
        }
        _ => {
            eprintln!("{} takes qsv [--probe]", super::HELPER_FLAG);
            return BAD_REQUEST;
        }
    }
    struct Shared(Mapped);
    impl Frames for Shared {
        fn bytes(&self) -> &[u8] {
            self.0.slice()
        }
    }
    let code = serve(
        std::io::stdin().lock(),
        std::io::stdout().lock(),
        |o| {
            let len = o.slot_size as usize * o.slots as usize;
            Mapped::open(o.section, len).map(|m| Box::new(Shared(m)) as Box<dyn Frames>)
        },
        super::open_session,
    );
    if code == STOPPED_EXIT {
        // Nothing of the session is closed: the device stopped answering.
        std::process::exit(code);
    }
    code
}
