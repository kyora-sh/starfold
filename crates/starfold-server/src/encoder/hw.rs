//! What the VA-API, NVENC and Media Foundation encoders share: NV12 upload, checking
//! their output, the sample encode that verifies a session, and opening each new session
//! in a child process first.

use super::caps::Probe as Found;
use super::h264::ParamSets;
use super::{
    av1, hevc, AccessUnit, Backend, BitstreamFormat, Codec, ColorInfo, EncodeError, EncodeOptions,
    EncoderConfig, FrameView, RateConfig, StreamConfig, VideoEncoder,
};
use std::sync::Mutex;

/// Writes `f` as NV12 into an image of `size` with the given pitches and offsets.
pub fn write_nv12(
    dst: &mut [u8],
    pitches: [usize; 2],
    offsets: [usize; 2],
    size: (u32, u32),
    f: &FrameView<'_>,
) -> Result<(), EncodeError> {
    f.check()?;
    let (aw, ah) = (size.0 as usize, size.1 as usize);
    let (w, h) = (f.width as usize, f.height as usize);
    let need = |off: usize, pitch: usize, rows: usize, cols: usize| {
        pitch >= cols && off + (rows.max(1) - 1) * pitch + cols <= dst.len()
    };
    if aw < w
        || ah < h
        || !need(offsets[0], pitches[0], ah, aw)
        || !need(offsets[1], pitches[1], ah.div_ceil(2), aw.div_ceil(2) * 2)
    {
        return Err(EncodeError(format!(
            "NV12 image of {}x{} doesn't hold a {w}x{h} picture",
            aw, ah
        )));
    }
    for r in 0..ah {
        let s = &f.y[r.min(h - 1) * f.stride_y..][..w];
        let d = &mut dst[offsets[0] + r * pitches[0]..][..aw];
        d[..w].copy_from_slice(s);
        d[w..].fill(s[w - 1]);
    }
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    for r in 0..ah.div_ceil(2) {
        let row = r.min(ch - 1) * f.stride_uv;
        let (u, v) = (&f.u[row..][..cw], &f.v[row..][..cw]);
        let d = &mut dst[offsets[1] + r * pitches[1]..][..aw.div_ceil(2) * 2];
        for (i, px) in d.chunks_exact_mut(2).enumerate() {
            let c = i.min(cw - 1);
            px[0] = u[c];
            px[1] = v[c];
        }
    }
    Ok(())
}

/// A temporal delimiter OBU.
const TD: [u8; 2] = [0x12, 0x00];

enum Units {
    H264(ParamSets),
    Hevc(hevc::Units),
    Av1(Option<Vec<u8>>),
}

impl Units {
    fn new(codec: Codec, (w, h): (u32, u32)) -> Units {
        match codec {
            Codec::Hevc => Units::Hevc(hevc::Units::new(w, h)),
            Codec::Av1 => Units::Av1(None),
            _ => Units::H264(ParamSets::default()),
        }
    }
}

/// A hardware encoder's output, checked against what was asked for, with a stream config
/// whenever its parameter sets change.
pub struct Stream {
    name: &'static str,
    codec: Codec,
    size: (u32, u32),
    color: ColorInfo,
    /// Takes a key frame where a P frame was asked for: the encoder keeps its own GOP.
    extra_keyframes: bool,
    /// A stream config has gone out.
    configured: bool,
    units: Units,
}

impl Stream {
    pub fn new(
        name: &'static str,
        codec: Codec,
        size: (u32, u32),
        color: ColorInfo,
        extra_keyframes: bool,
    ) -> Stream {
        Stream {
            name,
            codec,
            size,
            color,
            extra_keyframes,
            configured: false,
            units: Units::new(codec, size),
        }
    }

    /// The access unit in `data`. A rejected one leaves the parameter-set readers as new,
    /// so the next IDR carries a config.
    pub fn unit(
        &mut self,
        data: Vec<u8>,
        pts_ms: i64,
        idr: bool,
    ) -> Result<Option<AccessUnit>, EncodeError> {
        let r = self.check(data, pts_ms, idr);
        if r.is_err() {
            self.units = Units::new(self.codec, self.size);
        }
        r
    }

    fn check(
        &mut self,
        data: Vec<u8>,
        pts_ms: i64,
        idr: bool,
    ) -> Result<Option<AccessUnit>, EncodeError> {
        let (name, codec) = (self.name, self.codec);
        let au = match &mut self.units {
            Units::H264(p) => p.unit(data, pts_ms, self.size, self.color),
            Units::Hevc(u) => u.unit(data, pts_ms),
            Units::Av1(seq) => av1_unit(name, seq, data, pts_ms, self.size, self.color)?,
        };
        let kind = |k: bool| if k { "an IDR" } else { "a P frame" };
        match &au {
            None if idr => Err(EncodeError(format!(
                "{name} {codec:?} IDR came out without a picture"
            ))),
            Some(a) if a.keyframe != idr && !(a.keyframe && self.extra_keyframes) => {
                Err(EncodeError(format!(
                    "{name} {codec:?} asked for {}, got {}",
                    kind(idr),
                    kind(a.keyframe)
                )))
            }
            Some(a) if a.config.is_none() && !self.configured => Err(EncodeError(format!(
                "{name} {codec:?} stream began without readable parameter sets"
            ))),
            _ => {
                self.configured |= au.as_ref().is_some_and(|a| a.config.is_some());
                Ok(au)
            }
        }
    }
}

/// One AV1 temporal unit, a temporal delimiter put in front where it has none; a key
/// frame needs a sequence header.
pub fn av1_unit(
    name: &str,
    seq: &mut Option<Vec<u8>>,
    data: Vec<u8>,
    pts_ms: i64,
    size: (u32, u32),
    color: ColorInfo,
) -> Result<Option<AccessUnit>, EncodeError> {
    if data.is_empty() {
        return Ok(None);
    }
    let mut tu = Vec::with_capacity(data.len() + TD.len());
    if (data[0] >> 3) & 0xf != 2 {
        tu.extend_from_slice(&TD);
    }
    tu.extend(data);
    let key = av1::frame_type(&tu)
        .ok_or_else(|| EncodeError(format!("{name} AV1 output has no frame")))?
        == 0;
    let sh = av1::sequence_header(&tu).map(<[u8]>::to_vec);
    let config = match (&sh, key) {
        (None, true) => {
            return Err(EncodeError(format!(
                "{name} AV1 key frame without a sequence header"
            )))
        }
        (Some(s), true) if seq.as_ref() != Some(s) => {
            let codec_string = av1::codec_string(&tu)
                .ok_or_else(|| EncodeError("unreadable AV1 sequence header".into()))?;
            *seq = sh;
            Some(StreamConfig {
                codec: Codec::Av1,
                codec_string,
                coded: size,
                display: size,
                format: BitstreamFormat::Av1LowOverhead,
                color,
            })
        }
        _ => None,
    };
    Ok(Some(AccessUnit {
        pts_ms,
        keyframe: key,
        data: tu,
        config,
    }))
}

/// A hardware session's rate control.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rc {
    pub cbr: bool,
    pub bps: u32,
    pub buffer_bits: u32,
    pub fps: u32,
    pub qp: u32,
}

impl Rc {
    /// CBR when `rate` caps the bitrate, else constant QP. A session keeps its mode, and
    /// an uncapped rate on a CBR one keeps its bitrate.
    pub fn new(codec: Codec, rate: &RateConfig, prev: Option<&Rc>) -> Rc {
        let cbr = prev.map_or(rate.max_kbps > 0, |p| p.cbr);
        let (bps, buffer_bits) = match (cbr, rate.max_kbps, prev) {
            (false, ..) => (0, 0),
            (true, 0, Some(p)) => (p.bps, p.buffer_bits),
            (true, k, _) => (
                k.saturating_mul(1000),
                rate.buf_kbit.max(1).saturating_mul(1000),
            ),
        };
        let q = rate.quality.round() as i32;
        let qp = match codec {
            Codec::Av1 => (q * 5).clamp(1, 255),
            _ => q.clamp(1, 51),
        } as u32;
        Rc {
            cbr,
            bps,
            buffer_bits,
            fps: rate.fps.max(1),
            qp,
        }
    }
}

pub fn sample_config(
    backend: Backend,
    codec: Codec,
    width: u32,
    height: u32,
    fps: u32,
) -> EncoderConfig {
    EncoderConfig {
        backend,
        codec,
        width,
        height,
        fps,
        threads: 1,
        rate: RateConfig::new(24.0, 4000, fps),
        best_quality: 24.0,
        preset: String::new(),
    }
}

/// Encodes an IDR and a P frame of flat grey with `enc`.
pub fn sample(enc: &mut dyn VideoEncoder, width: u32, height: u32) -> Result<(), EncodeError> {
    let (w, h) = (width as usize, height as usize);
    let (y, uv) = (
        vec![128u8; w * h],
        vec![128u8; w.div_ceil(2) * h.div_ceil(2)],
    );
    let frame = FrameView {
        width,
        height,
        y: &y,
        u: &uv,
        v: &uv,
        stride_y: w,
        stride_uv: w.div_ceil(2),
    };
    for (i, key) in [true, false].into_iter().enumerate() {
        let aus = enc.encode(
            &frame,
            EncodeOptions {
                pts_ms: i as i64,
                force_keyframe: key,
                ..Default::default()
            },
        )?;
        if aus.len() != 1 || aus[0].keyframe != key || (key && aus[0].config.is_none()) {
            return Err(EncodeError(format!(
                "sample {} came out as {} unit(s)",
                if key { "IDR" } else { "P frame" },
                aus.len()
            )));
        }
    }
    Ok(())
}

/// Sessions a child has already opened (or failed to), so a driver that crashes on one
/// costs a child, not the server.
static PREFLIGHT: Mutex<Vec<(String, Result<(), String>)>> = Mutex::new(Vec::new());

fn codec_name(c: Codec) -> &'static str {
    match c {
        Codec::H264 => "h264",
        Codec::Hevc => "hevc",
        Codec::Av1 => "av1",
        #[allow(unreachable_patterns)]
        _ => "other",
    }
}

/// The `--session` argument for `cfg` on `device`:
/// `<codec>:<w>x<h>@<fps>,<max kbps>,<buffer kbit>,<rate fps>,<quality>,<device>`.
pub fn spec(cfg: &EncoderConfig, device: &str) -> String {
    let r = &cfg.rate;
    format!(
        "{}:{}x{}@{},{},{},{},{},{device}",
        codec_name(cfg.codec),
        cfg.width,
        cfg.height,
        cfg.fps,
        r.max_kbps,
        r.buf_kbit,
        r.fps,
        r.quality,
    )
}

/// What decides whether a session opens: the backend, device and the parameters its
/// session setup sees (the frame rates and rate control mode, not the bitrate).
fn key(cfg: &EncoderConfig, device: &str) -> String {
    format!(
        "{} {}:{}x{}@{}/{},{},{device}",
        cfg.backend.name(),
        codec_name(cfg.codec),
        cfg.width,
        cfg.height,
        cfg.fps,
        cfg.rate.fps,
        if cfg.rate.max_kbps > 0 {
            "capped"
        } else {
            "uncapped"
        },
    )
}

pub fn parse_spec(backend: Backend, s: &str) -> Option<(EncoderConfig, String)> {
    let (codec, rest) = s.split_once(':')?;
    let mut f = rest.splitn(6, ',');
    let (size, fps) = f.next()?.split_once('@')?;
    let (w, h) = size.split_once('x')?;
    let codec = match codec {
        "h264" => Codec::H264,
        "hevc" => Codec::Hevc,
        "av1" => Codec::Av1,
        _ => return None,
    };
    let (w, h, fps): (u32, u32, u32) = (w.parse().ok()?, h.parse().ok()?, fps.parse().ok()?);
    let mut cfg = sample_config(backend, codec, w, h, fps);
    cfg.rate = RateConfig {
        max_kbps: f.next()?.parse().ok()?,
        buf_kbit: f.next()?.parse().ok()?,
        fps: f.next()?.parse().ok()?,
        quality: f.next()?.parse().ok()?,
    };
    let device = f.next()?.to_string();
    (w > 0 && h > 0 && fps > 0 && !device.is_empty()).then_some((cfg, device))
}

/// Runs `child` once per session [`key`] and remembers what it said; `None` from it (no
/// child possible here) passes.
pub fn preflight(
    cfg: &EncoderConfig,
    device: &str,
    child: impl FnOnce(&str) -> Option<Found>,
) -> Result<(), EncodeError> {
    let key = key(cfg, device);
    let mut done = PREFLIGHT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, r)) = done.iter().find(|(k, _)| *k == key) {
        return r.clone().map_err(EncodeError);
    }
    let Some(found) = child(&spec(cfg, device)) else {
        return Ok(());
    };
    let r = if found.diagnostics.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{key} failed in a child process: {}",
            found
                .diagnostics
                .iter()
                .map(|d| d.reason.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        ))
    };
    done.push((key, r.clone()));
    r.map_err(EncodeError)
}

#[cfg(test)]
mod tests;
