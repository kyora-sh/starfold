//! Intel Quick Sync through oneVPL: Intel's GPU runtime (`libmfx64-gen.dll`, part of the
//! graphics driver) loaded at runtime from the DriverStore folder the adapter's driver names,
//! as Intel's own dispatcher finds it. Discovery asks the runtime what it encodes
//! (`MFXQueryImplsDescription`) and nothing else; an EXPERIMENTAL H.264 and HEVC encoder
//! runs on it (8-bit 4:2:0, IDR + P, one reference, LowPower where the runtime takes it).
//! Windows only. `--encoder auto` never picks it, only `--encoder qsv` does.

mod driver;
mod ffi;
#[cfg(windows)]
mod locate;
mod session;
#[cfg(test)]
mod tests;

use super::caps::{
    Acceleration, Chroma, EncoderCapability, EncoderLimits, PixelFormat, ProbeStatus,
    ProfileCapability,
};
use super::{Backend, Codec, EncodeError, EncoderConfig, VideoEncoder};
use std::sync::Mutex;

#[cfg(windows)]
pub use windows_impl::{child_session, open, Probe};

/// More than any runtime lists; bounds what is read from its descriptions.
const MAX_LIST: usize = 64;

/// One encoder the runtime describes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CodecInfo {
    pub codec_id: u32,
    pub profiles: Vec<u32>,
    pub width: Option<(u32, u32)>,
    pub height: Option<(u32, u32)>,
    /// Takes NV12 input in system memory.
    pub nv12: bool,
}

/// One implementation (one device) the runtime describes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ImplInfo {
    pub name: String,
    pub api: (u16, u16),
    pub hardware: bool,
    pub vendor: u32,
    pub vendor_impl_id: u32,
    pub acceleration: u32,
    /// The DXGI adapter LUID, where the runtime reports one (API 2.6 and later).
    pub luid: Option<u64>,
    pub device_id: Option<u16>,
    pub codecs: Vec<CodecInfo>,
}

impl ImplInfo {
    pub fn encodes(&self, codec: Codec) -> bool {
        self.codecs
            .iter()
            .any(|c| codec_of(c.codec_id) == Some(codec))
    }
}

/// An Intel graphics adapter.
#[derive(Clone, Debug, PartialEq)]
pub struct Adapter {
    pub luid: u64,
    pub device_id: u32,
    pub name: String,
}

impl Adapter {
    pub fn label(&self) -> String {
        format!("luid:{:016x} {}", self.luid, self.name)
    }
}

/// The PCI device in a device instance ID such as `PCI\VEN_8086&DEV_46D1&...`, if it is
/// Intel's (what libvpl's DriverStore loader matches on).
pub fn intel_device(instance: &str) -> Option<u32> {
    let up = instance.to_ascii_uppercase();
    if !up.contains("VEN_8086") {
        return None;
    }
    let dev = &up[up.find("DEV_")? + 4..];
    let hex: String = dev.chars().take_while(char::is_ascii_hexdigit).collect();
    u32::from_str_radix(&hex, 16).ok().filter(|&d| d != 0)
}

/// The runtime to load for `a`: only the absolute path its driver's DriverStore entry gives,
/// never a name looked up on the DLL search path (where a planted DLL would load first).
pub fn runtime_path(found: Result<String, String>, a: &Adapter) -> Result<String, String> {
    match found {
        Ok(p) if ffi::windows_absolute(&p) => Ok(p),
        Ok(p) => Err(format!(
            "the DriverStorePathForVPL of {} is not an absolute path: {p}",
            a.label()
        )),
        Err(e) => Err(format!("no DriverStorePathForVPL for {}: {e}", a.label())),
    }
}

/// The codecs this encoder takes from the runtime's list: H.264 and HEVC, never others.
fn codec_of(id: u32) -> Option<Codec> {
    match id {
        ffi::CODEC_AVC => Some(Codec::H264),
        ffi::CODEC_HEVC => Some(Codec::Hevc),
        _ => None,
    }
}

fn profile(codec: Codec, p: u32) -> Option<ProfileCapability> {
    let (name, depth) = match (codec, p) {
        (Codec::H264, ffi::PROFILE_AVC_CONSTRAINED_BASELINE) => ("constrained_baseline", 8),
        (Codec::H264, ffi::PROFILE_AVC_BASELINE) => ("baseline", 8),
        (Codec::H264, ffi::PROFILE_AVC_MAIN) => ("main", 8),
        (Codec::H264, ffi::PROFILE_AVC_HIGH) => ("high", 8),
        (Codec::Hevc, ffi::PROFILE_HEVC_MAIN) => ("main", 8),
        (Codec::Hevc, ffi::PROFILE_HEVC_MAIN10) => ("main", 10),
        _ => return None,
    };
    Some(ProfileCapability::new(name, depth, Chroma::Yuv420))
}

/// Reads one implementation description, its lists bounded by [`MAX_LIST`].
///
/// # Safety
/// `d` must be a description the runtime returned and has not released.
pub unsafe fn read_impl(d: &ffi::ImplDescription) -> ImplInfo {
    let list = |p: *const u8, n: usize| if p.is_null() { 0 } else { n.min(MAX_LIST) };
    let mut codecs = Vec::new();
    let enc = &d.enc;
    for i in 0..list(enc.codecs as *const u8, enc.num_codecs as usize) {
        let c = &*enc.codecs.add(i);
        let mut info = CodecInfo {
            codec_id: c.codec_id,
            ..Default::default()
        };
        for j in 0..list(c.profiles as *const u8, c.num_profiles as usize) {
            let p = &*c.profiles.add(j);
            info.profiles.push(p.profile);
            let mems = list(p.mem_desc as *const u8, p.num_mem_types as usize);
            let mem = (0..mems)
                .map(|k| &*p.mem_desc.add(k))
                .find(|m| m.mem_handle_type == ffi::RESOURCE_SYSTEM_SURFACE)
                .or_else(|| (mems > 0).then(|| &*p.mem_desc));
            if let Some(m) = mem {
                info.width.get_or_insert((m.width.min, m.width.max));
                info.height.get_or_insert((m.height.min, m.height.max));
                let n = list(m.color_formats as *const u8, m.num_color_formats as usize);
                info.nv12 |= (0..n).any(|k| *m.color_formats.add(k) == ffi::FOURCC_NV12);
            }
        }
        codecs.push(info);
    }
    ImplInfo {
        name: ffi::c_string(&d.impl_name),
        api: (d.api_version.major, d.api_version.minor),
        hardware: d.impl_type == ffi::IMPL_TYPE_HARDWARE,
        vendor: d.vendor_id,
        vendor_impl_id: d.vendor_impl_id,
        acceleration: d.acceleration_mode,
        luid: None,
        device_id: None,
        codecs,
    }
}

/// The implementation for `a` among `impls`: Intel hardware on the adapter's LUID, else on
/// its PCI device, else the only one.
pub fn pick(impls: &[ImplInfo], a: &Adapter) -> Result<ImplInfo, String> {
    let hw: Vec<&ImplInfo> = impls
        .iter()
        .filter(|i| i.hardware && i.vendor == ffi::VENDOR_INTEL)
        .collect();
    let by_device: Vec<&ImplInfo> = hw
        .iter()
        .copied()
        .filter(|i| i.device_id.map(u32::from) == Some(a.device_id))
        .collect();
    fn only<'a>(v: &[&'a ImplInfo]) -> Option<&'a ImplInfo> {
        match v {
            [i] => Some(*i),
            _ => None,
        }
    }
    hw.iter()
        .copied()
        .find(|i| i.luid == Some(a.luid))
        .or_else(|| only(&by_device))
        .or_else(|| only(&hw))
        .cloned()
        .ok_or_else(|| {
            format!(
                "the runtime lists {} Intel hardware implementation(s), none for this adapter",
                hw.len()
            )
        })
}

/// One capability per codec the runtime lists that this encoder takes.
pub fn caps(device: &str, imp: &ImplInfo) -> Vec<EncoderCapability> {
    let pos = |v: u32| (v > 0).then_some(v);
    imp.codecs
        .iter()
        .filter_map(|ci| {
            let codec = codec_of(ci.codec_id)?;
            let mut c = EncoderCapability::new(codec, Backend::Qsv, Acceleration::Hardware, device);
            for p in ci.profiles.iter().filter_map(|&p| profile(codec, p)) {
                if !c.profiles.contains(&p) {
                    c.profiles.push(p);
                }
            }
            c.limits = EncoderLimits {
                min_width: ci.width.and_then(|w| pos(w.0)),
                max_width: ci.width.and_then(|w| pos(w.1)),
                min_height: ci.height.and_then(|h| pos(h.0)),
                max_height: ci.height.and_then(|h| pos(h.1)),
                pixel_formats: ci.nv12.then(|| vec![PixelFormat::Nv12]),
                ..Default::default()
            };
            if !ci.nv12 || !c.profiles.iter().any(ProfileCapability::baseline) {
                c.status = ProbeStatus::ProbeOnly;
            }
            Some(c)
        })
        .collect()
}

/// Whether Quick Sync is still in use, and why not. After a host-side GPU reset or a hung
/// engine, a guest's encode sessions can keep timing out until the host is repaired: once one
/// session sees that, every session stops calling the runtime, no new one loads it, and the
/// streams fall back to their next encoder with the reason in the log and /stats.
pub struct Gate(Mutex<Option<String>>);

impl Gate {
    pub const fn new() -> Gate {
        Gate(Mutex::new(None))
    }

    /// Stops using Quick Sync for good; the first reason stays.
    pub fn give_up(&self, why: &str) {
        let mut g = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_none() {
            log::error!("{why}; Quick Sync is off until starfold restarts");
            *g = Some(why.to_string());
        }
    }

    pub fn given_up(&self) -> Option<String> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl Default for Gate {
    fn default() -> Gate {
        Gate::new()
    }
}

/// This process's gate.
pub static GATE: Gate = Gate::new();

/// A session for `cfg` on `rt`, unless `gate` has given Quick Sync up.
pub(super) fn session(
    rt: Box<dyn driver::Runtime>,
    cfg: &EncoderConfig,
    gate: &'static Gate,
) -> Result<Box<dyn VideoEncoder>, EncodeError> {
    Ok(Box::new(session::Session::new(rt, cfg, gate)?))
}

#[cfg(windows)]
mod windows_impl {
    use super::super::caps::{Probe as Found, ProbeProvider, ProbeStatus, PROBE_TIMEOUT};
    use super::super::{hw, Backend, Codec, EncodeError, EncoderConfig, VideoEncoder};
    use super::driver::MfxRuntime;
    use super::{caps, ffi, locate, pick, read_impl, session, Adapter, ImplInfo, GATE};

    fn given_up() -> Option<String> {
        GATE.given_up()
    }

    /// The side of the picture a probe encodes: the smallest `open` codes.
    const SAMPLE_SIDE: u32 = 160;
    /// More implementations than one runtime has.
    const MAX_IMPLS: u32 = 16;

    /// The implementations the runtime describes, with their devices where it says.
    unsafe fn describe(rt: &ffi::Runtime) -> Result<Vec<ImplInfo>, String> {
        let mut n = 0u32;
        let h = (rt.query_impls)(ffi::IMPLCAPS_IMPLDESCSTRUCTURE, &mut n);
        if h.is_null() || n == 0 {
            return Err(format!(
                "{}: MFXQueryImplsDescription listed no implementation",
                rt.path
            ));
        }
        let mut xn = 0u32;
        let x = (rt.query_impls)(ffi::IMPLCAPS_DEVICE_ID_EXTENDED, &mut xn);
        let mut v = Vec::new();
        for i in 0..n.min(MAX_IMPLS) as usize {
            let d = *h.add(i);
            if d.is_null() {
                continue;
            }
            let mut info = read_impl(&*(d as *const ffi::ImplDescription));
            if !x.is_null() && i < xn as usize {
                if let Some(e) = (*x.add(i) as *const ffi::ExtendedDeviceId).as_ref() {
                    info.device_id = Some(e.device_id);
                    if e.luid_valid != 0 {
                        info.luid = Some(u64::from_le_bytes(e.device_luid));
                    }
                }
            }
            v.push(info);
        }
        for i in 0..n as usize {
            let d = *h.add(i);
            if !d.is_null() {
                (rt.release_impl)(d);
            }
        }
        if !x.is_null() {
            for i in 0..xn as usize {
                let d = *x.add(i);
                if !d.is_null() {
                    (rt.release_impl)(d);
                }
            }
        }
        Ok(v)
    }

    /// The runtime for `a` and its implementation there.
    fn runtime(a: &Adapter) -> Result<(ffi::Runtime, ImplInfo), String> {
        let path = runtime_path(locate::driver_store_runtime(a.device_id), a)?;
        let rt = ffi::Runtime::load(&path)?;
        let impls = unsafe { describe(&rt)? };
        let imp = pick(&impls, a)?;
        Ok((rt, imp))
    }

    fn device(a: &Adapter, imp: &ImplInfo) -> String {
        format!(
            "{} ({}, oneVPL API {}.{})",
            a.label(),
            imp.name,
            imp.api.0,
            imp.api.1
        )
    }

    pub struct Probe;

    impl ProbeProvider for Probe {
        fn backend(&self) -> Backend {
            Backend::Qsv
        }

        fn probe(&self) -> Found {
            let adapters = match locate::intel_adapters() {
                Ok(a) => a,
                Err(e) => return Found::failed(Backend::Qsv, e),
            };
            let mut found = Found::default();
            if adapters.is_empty() {
                found.note(Backend::Qsv, None, "no Intel graphics adapter");
            }
            for a in &adapters {
                let (rt, imp) = match runtime(a) {
                    Ok(r) => r,
                    Err(e) => {
                        found.note(Backend::Qsv, Some(&a.label()), e);
                        continue;
                    }
                };
                drop(rt);
                let mut cs = caps(&device(a, &imp), &imp);
                if cs.is_empty() {
                    found.note(
                        Backend::Qsv,
                        Some(&a.label()),
                        format!("{} lists no H.264 or HEVC encoder", imp.name),
                    );
                }
                for c in cs
                    .iter_mut()
                    .filter(|c| c.status == ProbeStatus::Discovered)
                {
                    verify(c, a.luid);
                }
                found.caps.append(&mut cs);
            }
            found
        }
    }

    /// Marks `c` verified if a session on `luid` encodes a sample, unavailable if not.
    fn verify(c: &mut super::EncoderCapability, luid: u64) {
        if !Backend::Qsv.implements(c.codec) {
            return;
        }
        let cfg = hw::sample_config(Backend::Qsv, c.codec, SAMPLE_SIDE, SAMPLE_SIDE, 30);
        let r = open_on(luid, &cfg)
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

    fn open_on(luid: u64, cfg: &EncoderConfig) -> Result<Box<dyn VideoEncoder>, EncodeError> {
        if let Some(why) = given_up() {
            return Err(EncodeError(why));
        }
        let a = locate::intel_adapters()
            .map_err(EncodeError)?
            .into_iter()
            .find(|a| a.luid == luid)
            .ok_or_else(|| EncodeError(format!("no Intel adapter with LUID {luid:016x}")))?;
        let (rt, imp) = runtime(&a).map_err(EncodeError)?;
        if !imp.encodes(cfg.codec) {
            return Err(EncodeError(format!(
                "{} lists no {:?} encoder",
                imp.name, cfg.codec
            )));
        }
        let drv =
            MfxRuntime::open(rt, imp.acceleration, imp.vendor_impl_id).map_err(EncodeError)?;
        session(Box::new(drv), cfg, &GATE)
    }

    fn spec_device(luid: u64) -> String {
        format!("{luid:016x}")
    }

    /// Opens a Quick Sync encoder for `cfg` on the first Intel adapter whose runtime encodes
    /// its codec. Each new session is first opened and run in a child process (when this
    /// executable takes `--probe-encoders`); one that crashes, hangs or fails there is
    /// never opened here, and the caller falls back to its next encoder.
    pub fn open(cfg: &EncoderConfig) -> Result<Box<dyn VideoEncoder>, EncodeError> {
        if let Some(why) = given_up() {
            return Err(EncodeError(why));
        }
        let adapters = locate::intel_adapters().map_err(EncodeError)?;
        let mut why = Vec::new();
        let luid = adapters
            .iter()
            .find(|a| match runtime(a) {
                Ok((_, imp)) if imp.encodes(cfg.codec) => true,
                Ok((_, imp)) => {
                    why.push(format!(
                        "{}: {} lists no {:?} encoder",
                        a.label(),
                        imp.name,
                        cfg.codec
                    ));
                    false
                }
                Err(e) => {
                    why.push(format!("{}: {e}", a.label()));
                    false
                }
            })
            .map(|a| a.luid)
            .ok_or_else(|| {
                EncodeError(if why.is_empty() {
                    "no Intel graphics adapter".into()
                } else {
                    why.join("; ")
                })
            })?;
        hw::preflight(cfg, &spec_device(luid), |spec| {
            super::super::isolate::session(Backend::Qsv, spec, PROBE_TIMEOUT)
        })?;
        open_on(luid, cfg)
    }

    /// The child's side of [`open`]: opens the session `spec` names on its adapter and
    /// encodes a sample, in-process.
    pub fn child_session(spec: &str) -> Result<(), String> {
        let (cfg, luid) = hw::parse_spec(Backend::Qsv, spec)
            .and_then(|(c, d)| Some((c, u64::from_str_radix(&d, 16).ok()?)))
            .ok_or_else(|| format!("bad session {spec:?}"))?;
        if !matches!(cfg.codec, Codec::H264 | Codec::Hevc) {
            return Err(format!(
                "Quick Sync here encodes H.264 and HEVC, not {:?}",
                cfg.codec
            ));
        }
        let mut enc = open_on(luid, &cfg)?;
        hw::sample(enc.as_mut(), cfg.width, cfg.height).map_err(String::from)
    }
}
