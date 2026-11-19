//! Intel Quick Sync through oneVPL: Intel's GPU runtime (`libmfx64-gen.dll`, part of the
//! graphics driver) loaded at runtime from the DriverStore folder the adapter's driver names,
//! as Intel's own dispatcher finds it. Discovery asks the runtime what it encodes
//! (`MFXQueryImplsDescription`) and nothing else; an EXPERIMENTAL H.264 and HEVC encoder
//! runs on it (8-bit 4:2:0, IDR + P, one reference, LowPower only where the runtime refuses
//! the full encoder).
//! Windows only. `--encoder auto` never picks it, only `--encoder qsv` does.
//!
//! The runtime is only ever loaded and called in another process: the probe's child, and
//! for streaming one helper per encoder ([`remote`], [`helper`]), which the server kills
//! when it stops answering.

mod driver;
mod ffi;
#[cfg(all(test, unix))]
mod give_up_tests;
mod helper;
#[cfg(test)]
mod helper_tests;
#[cfg(windows)]
mod locate;
#[cfg(test)]
mod low_power_tests;
mod probe;
mod proc;
mod proto;
#[cfg(all(test, unix))]
mod r1_tests;
#[cfg(test)]
mod rate_reset_tests;
mod remote;
mod session;
#[cfg(test)]
mod tests;

use super::caps::{
    Acceleration, Chroma, EncoderCapability, EncoderLimits, PixelFormat, ProbeStatus,
    ProfileCapability,
};
use super::{Backend, Codec};
#[cfg(test)]
use super::{EncodeError, EncoderConfig, VideoEncoder};

/// The hidden flag that makes this executable a Quick Sync helper.
pub const HELPER_FLAG: &str = "--encoder-helper";

#[cfg(windows)]
pub use helper::main as helper_main;
#[cfg(windows)]
pub use windows_impl::{open, provider};
#[cfg(windows)]
use windows_impl::{open_session, probe_here};

/// Why Quick Sync is off in this process, as it stands now.
#[cfg(windows)]
pub fn off() -> Option<String> {
    remote::POLICY.off()
}

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

/// The runtime to load for `a`: only the absolute local path its driver's DriverStore entry
/// gives (as [`crate::winlib::absolute`] takes it), never a name looked up on the DLL search
/// path (where a planted DLL would load first).
pub fn runtime_path(found: Result<String, String>, a: &Adapter) -> Result<String, String> {
    let p = found.map_err(|e| format!("no DriverStorePathForVPL for {}: {e}", a.label()))?;
    crate::winlib::check_absolute(&p).map_err(|e| {
        format!(
            "the DriverStorePathForVPL of {} is unusable: {e}",
            a.label()
        )
    })?;
    Ok(p)
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

/// A session for `cfg` on `rt`.
#[cfg(test)]
fn session(
    rt: Box<dyn driver::Runtime>,
    cfg: &EncoderConfig,
) -> Result<Box<dyn VideoEncoder>, EncodeError> {
    Ok(Box::new(session::Session::new(rt, cfg)?))
}

/// Encodes a sample of `side` x `side` on `s`. A session that stopped is put in `kept`, not
/// dropped: a frame may still be in flight in it, writing into its buffers, so all of it
/// (runtime, buffers, bitstream) stays until the process exits.
#[cfg(any(windows, test))]
fn sample_on(
    mut s: session::Session,
    side: u32,
    kept: &mut Vec<session::Session>,
) -> Result<(), super::EncodeError> {
    let r = super::hw::sample(&mut s, side, side);
    if s.stopped().is_some() {
        kept.push(s);
    }
    r
}

/// The side of the picture a probe encodes: the smallest `open` codes.
#[cfg(any(windows, test))]
const SAMPLE_SIDE: u32 = 160;

/// What the probe helper is told after `--probe` when the stream runs uncapped.
pub const PROBE_UNCAPPED: &str = "--uncapped";

/// The probe's sample session for `codec`, in the stream's own mode: constant bitrate when
/// the stream is `capped` (`--max-kbps` above 0), constant QP when it isn't. A probe then
/// passes or fails on what the stream will ask of the runtime, and an uncapped stream's
/// probe asks nothing about HRD conformance (see [`session`]).
#[cfg(any(windows, test))]
fn probe_config(codec: Codec, capped: bool) -> super::EncoderConfig {
    let mut cfg = super::hw::sample_config(Backend::Qsv, codec, SAMPLE_SIDE, SAMPLE_SIDE, 30);
    if !capped {
        cfg.rate.max_kbps = 0;
    }
    cfg
}

#[cfg(windows)]
mod windows_impl {
    use super::super::caps::{Probe as Found, ProbeProvider, ProbeStatus};
    use super::super::{isolate, Backend, EncodeError, EncoderConfig, VideoEncoder};
    use super::driver::{MfxRuntime, STOPPED};
    use super::probe::{self, VERIFIED};
    use super::remote::{Remote, POLICY};
    use super::session::Session;
    use super::{caps, ffi, locate, pick, proc, read_impl, runtime_path, Adapter, ImplInfo};
    use super::{probe_config, PROBE_UNCAPPED, SAMPLE_SIDE};
    use std::time::Duration;
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

    /// The server's Quick Sync probe: run in a helper, or not at all; in the stream's mode,
    /// constant bitrate if `capped`, else constant QP ([`probe_config`]).
    pub fn provider(timeout: Duration, capped: bool) -> Box<dyn ProbeProvider> {
        let args: &'static [&'static str] = if capped {
            &["--probe"]
        } else {
            &["--probe", PROBE_UNCAPPED]
        };
        Box::new(probe::Probe {
            helpers: isolate::self_exec(),
            spawn: Box::new(move || proc::spawn_helper(args)),
            policy: &POLICY,
            verified: &VERIFIED,
            timeout,
        })
    }

    /// The probe itself, in this process: only a helper runs it. Its sample sessions run at
    /// constant bitrate if `capped`, else at constant QP.
    pub fn probe_here(capped: bool) -> Found {
        let adapters = match locate::intel_adapters() {
            Ok(a) => a,
            Err(e) => return Found::failed(Backend::Qsv, e),
        };
        let mut found = Found::default();
        let mut kept = Vec::new();
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
            let mut stopped = None;
            for c in cs
                .iter_mut()
                .filter(|c| c.status == ProbeStatus::Discovered)
            {
                stopped = verify(c, a.luid, capped, &mut kept);
                if stopped.is_some() {
                    break;
                }
            }
            found.caps.append(&mut cs);
            if let Some(why) = stopped {
                found.note(
                    Backend::Qsv,
                    Some(&a.label()),
                    format!("{why}; no further adapter is probed"),
                );
                break;
            }
        }
        std::mem::forget(kept);
        found
    }

    /// Marks `c` verified if a session on `luid` encodes a sample, unavailable if not; the
    /// reason (starting with [`STOPPED`]) if the device stopped answering (hung, failed or
    /// was lost), its session then in `kept`.
    fn verify(
        c: &mut super::EncoderCapability,
        luid: u64,
        capped: bool,
        kept: &mut Vec<Session>,
    ) -> Option<String> {
        if !Backend::Qsv.implements(c.codec) {
            return None;
        }
        let cfg = probe_config(c.codec, capped);
        let r = open_session(&cfg, luid).and_then(|(s, _)| super::sample_on(s, SAMPLE_SIDE, kept));
        match r {
            Ok(()) => {
                c.status = ProbeStatus::Verified;
                c.low_latency = Some(true);
                c.keyframe_control = Some(true);
                None
            }
            Err(e) => {
                c.status = ProbeStatus::Unavailable(format!("sample encode: {e}"));
                e.0.starts_with(STOPPED).then_some(e.0)
            }
        }
    }

    /// A session on adapter `luid`, in this process: the helper's, or a probe child's. Its
    /// name is the runtime implementation's.
    pub fn open_session(cfg: &EncoderConfig, luid: u64) -> Result<(Session, String), EncodeError> {
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
        Ok((Session::new(Box::new(drv), cfg)?, imp.name))
    }

    /// Opens a Quick Sync encoder for `cfg` in a helper process, on the adapter the probe
    /// verified `cfg`'s codec on. The server itself never loads or calls Intel's runtime.
    pub fn open(cfg: &EncoderConfig) -> Result<Box<dyn VideoEncoder>, EncodeError> {
        if !isolate::self_exec() {
            return Err(EncodeError(
                "Quick Sync needs the helper process: this program doesn't run starfold's helpers"
                    .into(),
            ));
        }
        let luid = VERIFIED.adapter(cfg.codec).ok_or_else(|| {
            EncodeError(format!(
                "no Intel adapter passed the Quick Sync probe for {:?}",
                cfg.codec
            ))
        })?;
        Ok(Box::new(Remote::open(
            &POLICY,
            || proc::spawn_helper(&[]),
            cfg,
            luid,
        )?))
    }
}
