//! What the VA-API and NVENC encoders share: NV12 upload, the sample encode that verifies
//! a session, and opening each new session in a child process first.

use super::caps::Probe as Found;
use super::{
    Backend, Codec, EncodeError, EncodeOptions, EncoderConfig, FrameView, RateConfig, VideoEncoder,
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
