//! VA-API: discovery of every render node's encode entrypoints, and EXPERIMENTAL
//! H.264, HEVC and AV1 encoders (8-bit 4:2:0, IDR + P, one reference). libva is loaded
//! at runtime so hosts without it start as usual. None of this has run on real
//! hardware yet: `--encoder auto` never picks it, only `--encoder vaapi` does.

mod av1;
mod bits;
mod driver;
mod ffi;
mod h264;
mod hevc;
mod session;
#[cfg(test)]
mod tests;

use super::caps::{
    Acceleration, Chroma, EncoderCapability, EncoderLimits, PixelFormat, Probe as Found,
    ProbeProvider, ProbeStatus, ProfileCapability,
};
use super::{Backend, Codec, EncodeError, EncoderConfig, FrameView, RateConfig, VideoEncoder};
use driver::{Driver, VaDriver};
use ffi::{
    VAConfigAttrib, VADisplay, VAStatus, Va, ATTRIB_MAX_HEIGHT, ATTRIB_MAX_WIDTH,
    ATTRIB_NOT_SUPPORTED, ATTRIB_RT_FORMAT, ENTRYPOINT_ENC_SLICE, ENTRYPOINT_ENC_SLICE_LP,
    RT_FORMAT_YUV420, RT_FORMAT_YUV420_10,
};
use session::{choose, Session};
use std::ffi::c_int;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// More than any driver reports; bounds what we allocate for its lists.
const MAX_LIST: c_int = 256;

/// One encode entrypoint of one profile, as libva reported it.
#[derive(Clone, Debug, PartialEq)]
pub struct Entrypoint {
    pub profile: c_int,
    pub entrypoint: c_int,
    pub rt_format: Option<u32>,
    pub max_width: Option<u32>,
    pub max_height: Option<u32>,
}

/// The codec and profile a VAProfile stands for.
fn profile(p: c_int) -> Option<(Codec, ProfileCapability)> {
    let (codec, name, depth, chroma) = match p {
        13 => (Codec::H264, "constrained_baseline", 8, Chroma::Yuv420),
        6 => (Codec::H264, "main", 8, Chroma::Yuv420),
        7 => (Codec::H264, "high", 8, Chroma::Yuv420),
        36 => (Codec::H264, "high10", 10, Chroma::Yuv420),
        17 => (Codec::Hevc, "main", 8, Chroma::Yuv420),
        18 => (Codec::Hevc, "main10", 10, Chroma::Yuv420),
        23 => (Codec::Hevc, "main12", 12, Chroma::Yuv420),
        24 => (Codec::Hevc, "main422_10", 10, Chroma::Yuv422),
        25 => (Codec::Hevc, "main422_12", 12, Chroma::Yuv422),
        26 => (Codec::Hevc, "main444", 8, Chroma::Yuv444),
        27 => (Codec::Hevc, "main444_10", 10, Chroma::Yuv444),
        28 => (Codec::Hevc, "main444_12", 12, Chroma::Yuv444),
        32 => (Codec::Av1, "main", 8, Chroma::Yuv420),
        33 => (Codec::Av1, "high", 8, Chroma::Yuv444),
        _ => return None,
    };
    Some((codec, ProfileCapability::new(name, depth, chroma)))
}

/// What a render node's encode entrypoints add up to, one capability per codec.
/// Decode-only (VLD) and other non-encode entrypoints are not encoders.
pub fn caps(node: &str, entrypoints: &[Entrypoint]) -> Vec<EncoderCapability> {
    let mut out: Vec<EncoderCapability> = Vec::new();
    for e in entrypoints {
        if !matches!(e.entrypoint, ENTRYPOINT_ENC_SLICE | ENTRYPOINT_ENC_SLICE_LP) {
            continue;
        }
        let Some((codec, prof)) = profile(e.profile) else {
            continue;
        };
        let i = match out.iter().position(|c| c.codec == codec) {
            Some(i) => i,
            None => {
                let mut c =
                    EncoderCapability::new(codec, Backend::Vaapi, Acceleration::Hardware, node);
                c.limits = EncoderLimits {
                    pixel_formats: Some(Vec::new()),
                    ..Default::default()
                };
                out.push(c);
                out.len() - 1
            }
        };
        let c = &mut out[i];
        // AV1's main profile is 8- and 10-bit; only the RT formats say which, so without
        // them its depth stays unknown.
        let profs = match e.rt_format {
            _ if e.profile != 32 => vec![prof],
            Some(f) => [(RT_FORMAT_YUV420, 8), (RT_FORMAT_YUV420_10, 10)]
                .into_iter()
                .filter(|&(bit, _)| f & bit != 0)
                .map(|(_, depth)| ProfileCapability::new("main", depth, Chroma::Yuv420))
                .collect(),
            None => Vec::new(),
        };
        for p in profs {
            if !c.profiles.contains(&p) {
                c.profiles.push(p);
            }
        }
        let l = &mut c.limits;
        // Several entrypoints: the smaller maximum holds for whichever one is used.
        for (have, got) in [
            (&mut l.max_width, e.max_width),
            (&mut l.max_height, e.max_height),
        ] {
            if let Some(g) = got {
                *have = Some(have.map_or(g, |h| h.min(g)));
            }
        }
        if let (Some(f), Some(fmts)) = (e.rt_format, l.pixel_formats.as_mut()) {
            for (bit, fmt) in [
                (RT_FORMAT_YUV420, PixelFormat::Nv12),
                (RT_FORMAT_YUV420_10, PixelFormat::P010),
            ] {
                if f & bit != 0 && !fmts.contains(&fmt) {
                    fmts.push(fmt);
                }
            }
        }
    }
    for c in &mut out {
        if c.limits.pixel_formats.as_ref().is_some_and(Vec::is_empty) {
            c.limits.pixel_formats = None;
        }
        if !c.profiles.is_empty() && !c.profiles.iter().any(ProfileCapability::baseline) {
            c.status = ProbeStatus::ProbeOnly;
        }
    }
    out
}

/// A display's encode entrypoints and vendor string.
unsafe fn query(va: &Va, dpy: VADisplay) -> Result<(Vec<Entrypoint>, String), String> {
    let (mut major, mut minor) = (0, 0);
    let r = (va.initialize)(dpy, &mut major, &mut minor);
    if r != 0 {
        (va.terminate)(dpy);
        return Err(format!("vaInitialize failed ({r})"));
    }
    let res = entrypoints(va, dpy).map(|e| (e, vendor_string(va, dpy)));
    (va.terminate)(dpy);
    res
}

/// A list libva fills up to the `max` it reported, which is what it is given room for.
unsafe fn list(
    max: c_int,
    fill: impl FnOnce(*mut c_int, *mut c_int) -> VAStatus,
) -> Result<Vec<c_int>, String> {
    if max > MAX_LIST {
        return Err(format!("reports {max} list entries, more than {MAX_LIST}"));
    }
    if max <= 0 {
        return Ok(Vec::new());
    }
    let mut v = vec![0; max as usize];
    let mut got: c_int = 0;
    let r = fill(v.as_mut_ptr(), &mut got);
    if r != 0 {
        return Err(format!("failed ({r})"));
    }
    v.truncate(got.clamp(0, max) as usize);
    Ok(v)
}

unsafe fn entrypoints(va: &Va, dpy: VADisplay) -> Result<Vec<Entrypoint>, String> {
    let profiles = list((va.max_num_profiles)(dpy), |p, n| {
        (va.query_profiles)(dpy, p, n)
    })
    .map_err(|e| format!("vaQueryConfigProfiles {e}"))?;
    let max_eps = (va.max_num_entrypoints)(dpy);
    if max_eps > MAX_LIST {
        return Err(format!(
            "vaMaxNumEntrypoints reports {max_eps}, more than {MAX_LIST}"
        ));
    }
    let mut out = Vec::new();
    for p in profiles.into_iter().filter(|&p| profile(p).is_some()) {
        let Ok(eps) = list(max_eps, |e, n| (va.query_entrypoints)(dpy, p, e, n)) else {
            continue;
        };
        for e in eps
            .into_iter()
            .filter(|&e| matches!(e, ENTRYPOINT_ENC_SLICE | ENTRYPOINT_ENC_SLICE_LP))
        {
            let mut attrs = [ATTRIB_RT_FORMAT, ATTRIB_MAX_WIDTH, ATTRIB_MAX_HEIGHT]
                .map(|kind| VAConfigAttrib { kind, value: 0 });
            let ok = (va.get_config_attributes)(dpy, p, e, attrs.as_mut_ptr(), 3) == 0;
            let v = |i: usize| {
                (ok && attrs[i].value != ATTRIB_NOT_SUPPORTED && attrs[i].value != 0)
                    .then_some(attrs[i].value)
            };
            out.push(Entrypoint {
                profile: p,
                entrypoint: e,
                rt_format: v(0),
                max_width: v(1),
                max_height: v(2),
            });
        }
    }
    Ok(out)
}

unsafe fn vendor_string(va: &Va, dpy: VADisplay) -> String {
    let s = (va.vendor)(dpy);
    if s.is_null() {
        return String::new();
    }
    let n = libc::strnlen(s, 256);
    String::from_utf8_lossy(std::slice::from_raw_parts(s as *const u8, n)).into_owned()
}

pub struct Probe;

impl ProbeProvider for Probe {
    fn backend(&self) -> Backend {
        Backend::Vaapi
    }

    fn probe(&self) -> Found {
        let nodes = match render_nodes() {
            Ok(n) => n,
            Err(e) => return Found::failed(Backend::Vaapi, e),
        };
        if nodes.is_empty() {
            return Found::failed(Backend::Vaapi, "no render nodes in /dev/dri");
        }
        let va = match Va::load() {
            Ok(v) => v,
            Err(e) => return Found::failed(Backend::Vaapi, e),
        };
        let mut found = Found::default();
        for node in nodes {
            let name = node.display().to_string();
            let file = match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&node)
            {
                Ok(f) => f,
                Err(e) => {
                    found.note(Backend::Vaapi, Some(&name), format!("can't open: {e}"));
                    continue;
                }
            };
            let dpy = unsafe { (va.get_display_drm)(file.as_raw_fd()) };
            if dpy.is_null() {
                found.note(Backend::Vaapi, Some(&name), "vaGetDisplayDRM failed");
                continue;
            }
            match unsafe { query(&va, dpy) } {
                Ok((eps, vendor)) => {
                    let id = if vendor.is_empty() {
                        name.clone()
                    } else {
                        format!("{name} ({vendor})")
                    };
                    let mut caps = caps(&id, &eps);
                    drop(file);
                    for c in caps
                        .iter_mut()
                        .filter(|c| c.status == ProbeStatus::Discovered)
                    {
                        verify(c, &node);
                    }
                    if caps.is_empty() {
                        found.note(Backend::Vaapi, Some(&name), "no encode entrypoints");
                    }
                    found.caps.extend(caps);
                }
                Err(e) => found.note(Backend::Vaapi, Some(&name), e),
            }
        }
        found
    }
}

/// The render nodes in `/dev/dri`, sorted.
fn render_nodes() -> Result<Vec<PathBuf>, String> {
    let mut nodes: Vec<PathBuf> = std::fs::read_dir("/dev/dri")
        .map_err(|e| format!("no /dev/dri: {e}"))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("renderD"))
        })
        .collect();
    nodes.sort();
    Ok(nodes)
}

/// The side of the picture a probe encodes: the smallest `open` codes.
const SAMPLE_SIDE: u32 = 128;

fn sample_config(codec: Codec, width: u32, height: u32, fps: u32) -> EncoderConfig {
    EncoderConfig {
        backend: Backend::Vaapi,
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
fn sample(enc: &mut dyn VideoEncoder, width: u32, height: u32) -> Result<(), EncodeError> {
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
            super::EncodeOptions {
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

/// Marks `c` verified if a session on `node` encodes a sample, unavailable if not.
fn verify(c: &mut EncoderCapability, node: &Path) {
    if !Backend::Vaapi.implements(c.codec) {
        return;
    }
    let cfg = sample_config(c.codec, SAMPLE_SIDE, SAMPLE_SIDE, 30);
    let r = VaDriver::open(node).and_then(|d| {
        let mut enc = session(Box::new(d), &cfg)?;
        sample(enc.as_mut(), SAMPLE_SIDE, SAMPLE_SIDE)
    });
    match r {
        Ok(()) => {
            c.status = ProbeStatus::Verified;
            c.low_latency = Some(true);
            c.keyframe_control = Some(true);
        }
        Err(e) => c.status = ProbeStatus::Unavailable(format!("sample encode: {e}")),
    }
}

/// A session for `cfg` on `drv`.
pub(super) fn session(
    mut drv: Box<dyn Driver>,
    cfg: &EncoderConfig,
) -> Result<Box<dyn VideoEncoder>, EncodeError> {
    let size = (cfg.width, cfg.height);
    Ok(match cfg.codec {
        Codec::H264 => {
            let mut c = choose(drv.as_mut(), h264::PROFILES, size, &[])?;
            c.packed = h264::H264::packed(c.packed);
            Box::new(Session::new(drv, &c, cfg, |c, rc| {
                Ok(h264::H264::new(c, cfg, rc))
            })?)
        }
        Codec::Hevc => {
            let mut c = choose(drv.as_mut(), hevc::PROFILES, size, hevc::EXTRA)?;
            c.packed = hevc::Hevc::packed(c.packed);
            Box::new(Session::new(drv, &c, cfg, |c, rc| {
                Ok(hevc::Hevc::new(c, cfg, rc))
            })?)
        }
        Codec::Av1 => {
            let mut c = choose(drv.as_mut(), av1::PROFILES, size, av1::EXTRA)?;
            c.packed = av1::Av1::packed(c.packed);
            Box::new(Session::new(drv, &c, cfg, |c, _| av1::Av1::new(c, cfg))?)
        }
        #[allow(unreachable_patterns)]
        c => return Err(EncodeError(format!("VA-API has no {c:?} encoder"))),
    })
}

fn open_on(node: &Path, cfg: &EncoderConfig) -> Result<Box<dyn VideoEncoder>, EncodeError> {
    VaDriver::open(node).and_then(|d| session(Box::new(d), cfg))
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

/// The `--session` argument for `cfg` on `node`:
/// `<codec>:<w>x<h>@<fps>,<max kbps>,<buffer kbit>,<rate fps>,<quality>,<node>`.
pub fn spec(cfg: &EncoderConfig, node: &Path) -> String {
    let r = &cfg.rate;
    format!(
        "{}:{}x{}@{},{},{},{},{},{}",
        codec_name(cfg.codec),
        cfg.width,
        cfg.height,
        cfg.fps,
        r.max_kbps,
        r.buf_kbit,
        r.fps,
        r.quality,
        node.display()
    )
}

/// What decides whether a session opens: the parameters `vaCreateConfig`,
/// `vaCreateSurfaces` and `vaCreateContext` see (the rate control mode, not the rate).
fn key(cfg: &EncoderConfig, node: &Path) -> String {
    format!(
        "{}:{}x{}@{},{},{}",
        codec_name(cfg.codec),
        cfg.width,
        cfg.height,
        cfg.fps,
        if cfg.rate.max_kbps > 0 {
            "capped"
        } else {
            "uncapped"
        },
        node.display()
    )
}

pub fn parse_spec(s: &str) -> Option<(EncoderConfig, PathBuf)> {
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
    let mut cfg = sample_config(codec, w, h, fps);
    cfg.rate = RateConfig {
        max_kbps: f.next()?.parse().ok()?,
        buf_kbit: f.next()?.parse().ok()?,
        fps: f.next()?.parse().ok()?,
        quality: f.next()?.parse().ok()?,
    };
    let node = PathBuf::from(f.next()?);
    (w > 0 && h > 0 && fps > 0 && node.is_absolute()).then_some((cfg, node))
}

/// Opens a VA-API encoder for `cfg` on the first render node that takes it. On each
/// node, each new session is first opened and run in a child process (when this
/// executable takes `--probe-encoders`); one that crashes, hangs or fails there is never
/// opened here, and the caller falls back to its next encoder.
pub fn open(cfg: &EncoderConfig) -> Result<Box<dyn VideoEncoder>, EncodeError> {
    let nodes = render_nodes().map_err(EncodeError)?;
    let mut why = Vec::new();
    for node in nodes {
        let r = preflight(cfg, &node, |spec| {
            super::isolate::session(spec, super::caps::PROBE_TIMEOUT)
        })
        .and_then(|()| open_on(&node, cfg));
        match r {
            Ok(e) => return Ok(e),
            Err(e) => why.push(format!("{}: {e}", node.display())),
        }
    }
    Err(EncodeError(if why.is_empty() {
        "no render nodes in /dev/dri".into()
    } else {
        why.join("; ")
    }))
}

/// Runs `child` once per session [`key`] and remembers what it said; `None` from it (no
/// child possible here) passes.
pub(super) fn preflight(
    cfg: &EncoderConfig,
    node: &Path,
    child: impl FnOnce(&str) -> Option<Found>,
) -> Result<(), EncodeError> {
    let key = key(cfg, node);
    let mut done = PREFLIGHT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, r)) = done.iter().find(|(k, _)| *k == key) {
        return r.clone().map_err(EncodeError);
    }
    let Some(found) = child(&spec(cfg, node)) else {
        return Ok(());
    };
    let r = if found.diagnostics.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "VA-API {key} failed in a child process: {}",
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

/// The child's side of [`open`]: opens the session `spec` names on its node and encodes
/// a sample, in-process.
pub fn child_session(spec: &str) -> Result<(), String> {
    let (cfg, node) = parse_spec(spec).ok_or_else(|| format!("bad session {spec:?}"))?;
    let mut enc = open_on(&node, &cfg)?;
    sample(enc.as_mut(), cfg.width, cfg.height).map_err(String::from)
}
