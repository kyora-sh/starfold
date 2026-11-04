//! VA-API discovery: the encode entrypoints of every render node, through libva loaded
//! at runtime so hosts without it start as usual.

use super::caps::{
    Acceleration, Chroma, EncoderCapability, EncoderLimits, PixelFormat, Probe as Found,
    ProbeProvider, ProbeStatus, ProfileCapability,
};
use super::{Backend, Codec};
use libloading::Library;
use std::ffi::{c_char, c_int, c_void};
use std::os::fd::AsRawFd;

type VADisplay = *mut c_void;
type VAStatus = c_int;

const ENTRYPOINT_ENC_SLICE: c_int = 6;
const ENTRYPOINT_ENC_SLICE_LP: c_int = 8;
const ATTRIB_RT_FORMAT: c_int = 0;
const ATTRIB_MAX_WIDTH: c_int = 18;
const ATTRIB_MAX_HEIGHT: c_int = 19;
const ATTRIB_NOT_SUPPORTED: u32 = 0x8000_0000;
const RT_FORMAT_YUV420: u32 = 0x1;
const RT_FORMAT_YUV420_10: u32 = 0x100;
/// More than any driver reports; bounds what we allocate for its lists.
const MAX_LIST: c_int = 256;

#[repr(C)]
#[derive(Clone, Copy)]
struct VAConfigAttrib {
    kind: c_int,
    value: u32,
}

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
        let mut profs = vec![prof];
        // AV1's main profile is 8- and 10-bit; the RT formats say which.
        if e.profile == 32 && e.rt_format.is_some_and(|f| f & RT_FORMAT_YUV420_10 != 0) {
            profs.push(ProfileCapability::new("main", 10, Chroma::Yuv420));
        }
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
        if !c.profiles.iter().any(ProfileCapability::baseline) {
            c.status = ProbeStatus::ProbeOnly;
        }
    }
    out
}

/// libva's entry points, loaded once per probe.
struct Va {
    _va: Library,
    _drm: Library,
    get_display_drm: unsafe extern "C" fn(c_int) -> VADisplay,
    initialize: unsafe extern "C" fn(VADisplay, *mut c_int, *mut c_int) -> VAStatus,
    terminate: unsafe extern "C" fn(VADisplay) -> VAStatus,
    max_num_profiles: unsafe extern "C" fn(VADisplay) -> c_int,
    max_num_entrypoints: unsafe extern "C" fn(VADisplay) -> c_int,
    query_profiles: unsafe extern "C" fn(VADisplay, *mut c_int, *mut c_int) -> VAStatus,
    query_entrypoints: unsafe extern "C" fn(VADisplay, c_int, *mut c_int, *mut c_int) -> VAStatus,
    get_config_attributes:
        unsafe extern "C" fn(VADisplay, c_int, c_int, *mut VAConfigAttrib, c_int) -> VAStatus,
    vendor: unsafe extern "C" fn(VADisplay) -> *const c_char,
}

impl Va {
    fn load() -> Result<Va, String> {
        unsafe {
            let va = Library::new("libva.so.2").map_err(|e| format!("libva.so.2: {e}"))?;
            let drm = Library::new("libva-drm.so.2").map_err(|e| format!("libva-drm.so.2: {e}"))?;
            macro_rules! sym {
                ($lib:expr, $name:literal) => {
                    *$lib
                        .get(concat!($name, "\0").as_bytes())
                        .map_err(|e| format!("{}: {e}", $name))?
                };
            }
            Ok(Va {
                get_display_drm: sym!(drm, "vaGetDisplayDRM"),
                initialize: sym!(va, "vaInitialize"),
                terminate: sym!(va, "vaTerminate"),
                max_num_profiles: sym!(va, "vaMaxNumProfiles"),
                max_num_entrypoints: sym!(va, "vaMaxNumEntrypoints"),
                query_profiles: sym!(va, "vaQueryConfigProfiles"),
                query_entrypoints: sym!(va, "vaQueryConfigEntrypoints"),
                get_config_attributes: sym!(va, "vaGetConfigAttributes"),
                vendor: sym!(va, "vaQueryVendorString"),
                _va: va,
                _drm: drm,
            })
        }
    }

    /// A display's encode entrypoints and vendor string.
    unsafe fn query(&self, dpy: VADisplay) -> Result<(Vec<Entrypoint>, String), String> {
        let (mut major, mut minor) = (0, 0);
        let r = (self.initialize)(dpy, &mut major, &mut minor);
        if r != 0 {
            return Err(format!("vaInitialize failed ({r})"));
        }
        let res = self.entrypoints(dpy).map(|e| (e, self.vendor_string(dpy)));
        (self.terminate)(dpy);
        res
    }

    unsafe fn list(
        n: c_int,
        fill: impl FnOnce(*mut c_int, *mut c_int) -> VAStatus,
    ) -> Result<Vec<c_int>, VAStatus> {
        let mut v = vec![0; n.clamp(0, MAX_LIST) as usize];
        let mut got: c_int = 0;
        let r = fill(v.as_mut_ptr(), &mut got);
        if r != 0 {
            return Err(r);
        }
        v.truncate(got.clamp(0, v.len() as c_int) as usize);
        Ok(v)
    }

    unsafe fn entrypoints(&self, dpy: VADisplay) -> Result<Vec<Entrypoint>, String> {
        let profiles = Self::list((self.max_num_profiles)(dpy), |p, n| {
            (self.query_profiles)(dpy, p, n)
        })
        .map_err(|r| format!("vaQueryConfigProfiles failed ({r})"))?;
        let max_eps = (self.max_num_entrypoints)(dpy);
        let mut out = Vec::new();
        for p in profiles.into_iter().filter(|&p| profile(p).is_some()) {
            let Ok(eps) = Self::list(max_eps, |e, n| (self.query_entrypoints)(dpy, p, e, n)) else {
                continue;
            };
            for e in eps
                .into_iter()
                .filter(|&e| matches!(e, ENTRYPOINT_ENC_SLICE | ENTRYPOINT_ENC_SLICE_LP))
            {
                let mut attrs = [ATTRIB_RT_FORMAT, ATTRIB_MAX_WIDTH, ATTRIB_MAX_HEIGHT]
                    .map(|kind| VAConfigAttrib { kind, value: 0 });
                let ok = (self.get_config_attributes)(dpy, p, e, attrs.as_mut_ptr(), 3) == 0;
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

    unsafe fn vendor_string(&self, dpy: VADisplay) -> String {
        let s = (self.vendor)(dpy);
        if s.is_null() {
            return String::new();
        }
        let n = libc::strnlen(s, 256);
        String::from_utf8_lossy(std::slice::from_raw_parts(s as *const u8, n)).into_owned()
    }
}

pub struct Probe;

impl ProbeProvider for Probe {
    fn backend(&self) -> Backend {
        Backend::Vaapi
    }

    fn probe(&self) -> Found {
        let mut nodes: Vec<_> = match std::fs::read_dir("/dev/dri") {
            Ok(d) => d
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("renderD"))
                })
                .collect(),
            Err(e) => return Found::failed(Backend::Vaapi, format!("no /dev/dri: {e}")),
        };
        nodes.sort();
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
            match unsafe { va.query(dpy) } {
                Ok((eps, vendor)) => {
                    let id = if vendor.is_empty() {
                        name.clone()
                    } else {
                        format!("{name} ({vendor})")
                    };
                    let caps = caps(&id, &eps);
                    if caps.is_empty() {
                        found.note(Backend::Vaapi, Some(&name), "no encode entrypoints");
                    }
                    found.caps.extend(caps);
                }
                Err(e) => found.note(Backend::Vaapi, Some(&name), e),
            }
            drop(file);
        }
        found
    }
}
