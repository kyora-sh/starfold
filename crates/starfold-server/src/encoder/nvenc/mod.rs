//! NVENC: discovery through the driver's own libraries, loaded at runtime (the API
//! version, then per CUDA device an encode session and its codecs, profiles, input
//! formats and limits), and EXPERIMENTAL H.264, HEVC and AV1 encoders (8-bit 4:2:0,
//! IDR + P, one reference, ultra-low-latency tuning). None of this has run on real
//! hardware yet: `--encoder auto` never picks it, only `--encoder nvenc` does.

mod driver;
mod ffi;
mod session;
#[cfg(test)]
mod tests;

use super::caps::{
    Acceleration, Chroma, EncoderCapability, EncoderLimits, PixelFormat, Probe as Found,
    ProbeProvider, ProbeStatus, ProfileCapability,
};
use super::{hw, Backend, Codec, EncodeError, EncoderConfig, VideoEncoder};
use driver::{Driver, NvDriver};
use ffi::guid;
use ffi::{
    Api, CapsParam, Cuda, Enc, FunctionList, Status, CAPS_PARAM_VER, FMT_IYUV, FMT_NV12,
    FMT_YUV420_10BIT, FMT_YUV444,
};
pub use ffi::{Guid, CODEC_AV1, CODEC_H264, CODEC_HEVC};
use session::Session;
use std::ffi::c_int;

/// More than any driver reports; bounds what we allocate for its lists.
const MAX_LIST: u32 = 64;
const MAX_DEVICES: c_int = 16;

/// Profile GUIDs and what they encode.
pub const PROFILES: [(Guid, &str, u8, Chroma); 10] = [
    (
        guid(
            0x0727bcaa,
            0x78c4,
            0x4c83,
            [0x8c, 0x2f, 0xef, 0x3d, 0xff, 0x26, 0x7c, 0x6a],
        ),
        "baseline",
        8,
        Chroma::Yuv420,
    ),
    (
        guid(
            0x60b5c1d4,
            0x67fe,
            0x4790,
            [0x94, 0xd5, 0xc4, 0x72, 0x6d, 0x7b, 0x6e, 0x6d],
        ),
        "main",
        8,
        Chroma::Yuv420,
    ),
    (
        guid(
            0xe7cbc309,
            0x4f7a,
            0x4b89,
            [0xaf, 0x2a, 0xd5, 0x37, 0xc9, 0x2b, 0xe3, 0x10],
        ),
        "high",
        8,
        Chroma::Yuv420,
    ),
    (
        guid(
            0x7ac663cb,
            0xa598,
            0x4960,
            [0xb8, 0x44, 0x33, 0x9b, 0x26, 0x1a, 0x7d, 0x52],
        ),
        "high444",
        8,
        Chroma::Yuv444,
    ),
    (
        guid(
            0xb405afac,
            0xf32b,
            0x417b,
            [0x89, 0xc4, 0x9a, 0xbe, 0xed, 0x3e, 0x59, 0x78],
        ),
        "progressive_high",
        8,
        Chroma::Yuv420,
    ),
    (
        guid(
            0xaec1bd87,
            0xe85b,
            0x48f2,
            [0x84, 0xc3, 0x98, 0xbc, 0xa6, 0x28, 0x50, 0x72],
        ),
        "constrained_high",
        8,
        Chroma::Yuv420,
    ),
    (
        guid(
            0xb514c39a,
            0xb55b,
            0x40fa,
            [0x87, 0x8f, 0xf1, 0x25, 0x3b, 0x4d, 0xfd, 0xec],
        ),
        "main",
        8,
        Chroma::Yuv420,
    ),
    (
        guid(
            0xfa4d2b6c,
            0x3a5b,
            0x411a,
            [0x80, 0x18, 0x0a, 0x3f, 0x5e, 0x3c, 0x9b, 0xe5],
        ),
        "main10",
        10,
        Chroma::Yuv420,
    ),
    (
        guid(
            0x51ec32b5,
            0x1b4c,
            0x453c,
            [0x9c, 0xbd, 0xb6, 0x16, 0xbd, 0x62, 0x13, 0x41],
        ),
        "frext",
        8,
        Chroma::Yuv444,
    ),
    (
        guid(
            0x5f2a39f5,
            0xf14e,
            0x4f95,
            [0x9a, 0x9e, 0xb7, 0x6d, 0x56, 0x8f, 0xcf, 0x97],
        ),
        "main",
        8,
        Chroma::Yuv420,
    ),
];

const CAPS_WIDTH_MAX: u32 = 16;
const CAPS_HEIGHT_MAX: u32 = 17;
const CAPS_MB_NUM_MAX: u32 = 31;
const CAPS_MB_PER_SEC_MAX: u32 = 32;
const CAPS_10BIT_ENCODE: u32 = 39;
const CAPS_WIDTH_MIN: u32 = 45;
const CAPS_HEIGHT_MIN: u32 = 46;

/// The limits NVENC reported for one codec; `None` where the query failed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Limits {
    pub width: (Option<i32>, Option<i32>),
    pub height: (Option<i32>, Option<i32>),
    pub mbs: Option<i32>,
    pub mbs_per_sec: Option<i32>,
    pub ten_bit: Option<i32>,
}

/// One codec of one device, as NVENC reported it.
#[derive(Clone, Debug, PartialEq)]
pub struct CodecReport {
    pub codec: Guid,
    pub profiles: Vec<Guid>,
    pub formats: Vec<u32>,
    pub limits: Limits,
}

/// One capability per codec this build knows; unknown GUIDs are skipped.
pub fn caps(device: &str, reports: &[CodecReport]) -> Vec<EncoderCapability> {
    let pos = |v: Option<i32>| v.filter(|&v| v > 0).map(|v| v as u32);
    reports
        .iter()
        .filter_map(|r| {
            let codec = match r.codec {
                CODEC_H264 => Codec::H264,
                CODEC_HEVC => Codec::Hevc,
                CODEC_AV1 => Codec::Av1,
                _ => return None,
            };
            let mut c =
                EncoderCapability::new(codec, Backend::Nvenc, Acceleration::Hardware, device);
            for g in &r.profiles {
                if let Some(&(_, name, depth, chroma)) = PROFILES.iter().find(|p| p.0 == *g) {
                    c.profiles.push(ProfileCapability::new(name, depth, chroma));
                }
            }
            // AV1's main profile is 8- and 10-bit; the 10-bit cap says which.
            if codec == Codec::Av1 && r.limits.ten_bit.is_some_and(|v| v > 0) {
                c.profiles
                    .push(ProfileCapability::new("main", 10, Chroma::Yuv420));
            }
            let fmts: Vec<PixelFormat> = r
                .formats
                .iter()
                .filter_map(|&f| match f {
                    FMT_NV12 => Some(PixelFormat::Nv12),
                    FMT_IYUV => Some(PixelFormat::I420),
                    FMT_YUV444 => Some(PixelFormat::Yuv444),
                    FMT_YUV420_10BIT => Some(PixelFormat::P010),
                    _ => None,
                })
                .collect();
            let eight_bit_input = fmts
                .iter()
                .any(|f| matches!(f, PixelFormat::Nv12 | PixelFormat::I420));
            c.limits = EncoderLimits {
                min_width: pos(r.limits.width.0),
                max_width: pos(r.limits.width.1),
                min_height: pos(r.limits.height.0),
                max_height: pos(r.limits.height.1),
                max_mbs: pos(r.limits.mbs).map(u64::from),
                max_mbs_per_sec: pos(r.limits.mbs_per_sec).map(u64::from),
                pixel_formats: (!fmts.is_empty()).then_some(fmts),
                ..Default::default()
            };
            if !eight_bit_input || !c.profiles.iter().any(ProfileCapability::baseline) {
                c.status = ProbeStatus::ProbeOnly;
            }
            Some(c)
        })
        .collect()
}

/// An open encode session on one CUDA context, closed on drop.
struct Probing<'a> {
    nv: &'a FunctionList,
    enc: Enc,
}

impl Drop for Probing<'_> {
    fn drop(&mut self) {
        if let Some(f) = self.nv.destroy_encoder {
            unsafe { f(self.enc) };
        }
    }
}

impl Probing<'_> {
    /// A bounded list: `count` says how many, `fill` writes at most that many.
    unsafe fn list<T: Copy + Default>(
        count: impl FnOnce(*mut u32) -> Status,
        fill: impl FnOnce(*mut T, u32, *mut u32) -> Status,
    ) -> Result<Vec<T>, Status> {
        let mut n = 0u32;
        let r = count(&mut n);
        if r != 0 {
            return Err(r);
        }
        let mut v = vec![T::default(); n.min(MAX_LIST) as usize];
        if v.is_empty() {
            return Ok(v);
        }
        let mut got = 0u32;
        let r = fill(v.as_mut_ptr(), v.len() as u32, &mut got);
        if r != 0 {
            return Err(r);
        }
        v.truncate((got as usize).min(v.len()));
        Ok(v)
    }

    unsafe fn cap(&self, codec: Guid, which: u32) -> Option<i32> {
        let f = self.nv.get_encode_caps?;
        let mut p = CapsParam {
            version: CAPS_PARAM_VER,
            caps: which,
            reserved: [0; 62],
        };
        let mut v: c_int = 0;
        (f(self.enc, codec, &mut p, &mut v) == 0).then_some(v)
    }

    unsafe fn report(&self) -> Result<Vec<CodecReport>, String> {
        let nv = self.nv;
        let (Some(count), Some(guids), Some(pcount), Some(pguids), Some(fcount), Some(fmts)) = (
            nv.get_encode_guid_count,
            nv.get_encode_guids,
            nv.get_encode_profile_guid_count,
            nv.get_encode_profile_guids,
            nv.get_input_format_count,
            nv.get_input_formats,
        ) else {
            return Err("the driver's function list is incomplete".into());
        };
        let enc = self.enc;
        let codecs = Self::list(|n| count(enc, n), |g, cap, n| guids(enc, g, cap, n))
            .map_err(|r| format!("nvEncGetEncodeGUIDs failed ({r})"))?;
        Ok(codecs
            .into_iter()
            .map(|c| CodecReport {
                codec: c,
                profiles: Self::list(|n| pcount(enc, c, n), |g, cap, n| pguids(enc, c, g, cap, n))
                    .unwrap_or_default(),
                formats: Self::list(|n| fcount(enc, c, n), |f, cap, n| fmts(enc, c, f, cap, n))
                    .unwrap_or_default(),
                limits: Limits {
                    width: (self.cap(c, CAPS_WIDTH_MIN), self.cap(c, CAPS_WIDTH_MAX)),
                    height: (self.cap(c, CAPS_HEIGHT_MIN), self.cap(c, CAPS_HEIGHT_MAX)),
                    mbs: self.cap(c, CAPS_MB_NUM_MAX),
                    mbs_per_sec: self.cap(c, CAPS_MB_PER_SEC_MAX),
                    ten_bit: self.cap(c, CAPS_10BIT_ENCODE),
                },
            })
            .collect())
    }
}

pub struct Probe;

impl ProbeProvider for Probe {
    fn backend(&self) -> Backend {
        Backend::Nvenc
    }

    fn probe(&self) -> Found {
        match unsafe { probe() } {
            Ok(f) => f,
            Err(e) => Found::failed(Backend::Nvenc, e),
        }
    }
}

unsafe fn probe() -> Result<Found, String> {
    let api = Api::load()?;
    let nv = &api.nv;
    let cuda = Cuda::load()?;
    let n = cuda.count()?;
    let mut found = Found::default();
    if n <= 0 {
        found.note(Backend::Nvenc, None, "no CUDA devices");
    }
    for i in 0..n.min(MAX_DEVICES) {
        let id = format!("cuda:{i} {}", cuda.name(i));
        let ctx = match cuda.context(i) {
            Ok(c) => c,
            Err(e) => {
                found.note(Backend::Nvenc, Some(&id), e);
                continue;
            }
        };
        let mut caps = match api.open(ctx) {
            Ok(enc) => {
                let s = Probing { nv, enc };
                s.report().map(|r| caps(&id, &r))
            }
            Err(e) => Err(e),
        };
        (cuda.ctx_destroy)(ctx);
        match &mut caps {
            Ok(caps) => {
                for c in caps
                    .iter_mut()
                    .filter(|c| c.status == ProbeStatus::Discovered)
                {
                    verify(c, i);
                }
                found.caps.append(caps);
            }
            Err(e) => found.note(Backend::Nvenc, Some(&id), e.clone()),
        }
    }
    Ok(found)
}

/// The side of the picture a probe encodes: the smallest `open` codes.
const SAMPLE_SIDE: u32 = 160;

/// Marks `c` verified if a session on `device` encodes a sample, unavailable if not.
fn verify(c: &mut EncoderCapability, device: c_int) {
    if !Backend::Nvenc.implements(c.codec) {
        return;
    }
    let cfg = hw::sample_config(Backend::Nvenc, c.codec, SAMPLE_SIDE, SAMPLE_SIDE, 30);
    let r = open_on(device, &cfg)
        .and_then(|mut enc| hw::sample(enc.as_mut(), SAMPLE_SIDE, SAMPLE_SIDE));
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
    drv: Box<dyn Driver>,
    cfg: &EncoderConfig,
) -> Result<Box<dyn VideoEncoder>, EncodeError> {
    Ok(Box::new(Session::new(drv, cfg)?))
}

fn open_on(device: c_int, cfg: &EncoderConfig) -> Result<Box<dyn VideoEncoder>, EncodeError> {
    NvDriver::open(device).and_then(|d| session(Box::new(d), cfg))
}

/// The CUDA device sessions open on.
const DEVICE: c_int = 0;

/// Opens an NVENC encoder for `cfg` on the first CUDA device. Each new session is first
/// opened and run in a child process (when this executable takes `--probe-encoders`);
/// one that crashes, hangs or fails there is never opened here, and the caller falls back
/// to its next encoder.
pub fn open(cfg: &EncoderConfig) -> Result<Box<dyn VideoEncoder>, EncodeError> {
    hw::preflight(cfg, &DEVICE.to_string(), |spec| {
        super::isolate::session(Backend::Nvenc, spec, super::caps::PROBE_TIMEOUT)
    })?;
    open_on(DEVICE, cfg)
}

/// The child's side of [`open`]: opens the session `spec` names on its device and
/// encodes a sample, in-process.
pub fn child_session(spec: &str) -> Result<(), String> {
    let (cfg, device) = hw::parse_spec(Backend::Nvenc, spec)
        .and_then(|(c, d)| Some((c, d.parse::<c_int>().ok().filter(|&d| d >= 0)?)))
        .ok_or_else(|| format!("bad session {spec:?}"))?;
    let mut enc = open_on(device, &cfg)?;
    hw::sample(enc.as_mut(), cfg.width, cfg.height).map_err(String::from)
}
