//! What encoders this host has: every backend's runtime API is probed once at startup
//! (never a table of GPU names), and the result is read-only from then on.

use super::{Backend, Codec, EncoderConfig, FrameView, RateConfig};
use serde::{Deserialize, Serialize};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Acceleration {
    Hardware,
    Software,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state", content = "reason")]
pub enum ProbeStatus {
    /// An encode session opened (and, where the probe can, encoded a sample).
    Verified,
    /// The runtime lists it; no session was opened.
    Discovered,
    /// Only in forms this build never encodes (10-bit, 4:2:2, 4:4:4).
    ProbeOnly,
    /// Not probed: an experimental probe left off (`--experimental-hw-probes`).
    Disabled,
    Unavailable(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PixelFormat {
    I420,
    Nv12,
    P010,
    Yuv444,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Chroma {
    #[serde(rename = "4:2:0")]
    Yuv420,
    #[serde(rename = "4:2:2")]
    Yuv422,
    #[serde(rename = "4:4:4")]
    Yuv444,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileCapability {
    pub name: String,
    pub bit_depth: u8,
    pub chroma: Chroma,
}

impl ProfileCapability {
    pub fn new(name: &str, bit_depth: u8, chroma: Chroma) -> Self {
        ProfileCapability {
            name: name.into(),
            bit_depth,
            chroma,
        }
    }

    /// 8-bit 4:2:0, the only form this build encodes.
    pub fn baseline(&self) -> bool {
        self.bit_depth == 8 && self.chroma == Chroma::Yuv420
    }
}

/// What the encoder reported; `None` is unknown, never "unlimited". Widths and heights
/// are separate ranges, so a width and a height that are each in range need not fit
/// together: `max_mbs` and `max_mbs_per_sec` say what does.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncoderLimits {
    pub min_width: Option<u32>,
    pub max_width: Option<u32>,
    pub min_height: Option<u32>,
    pub max_height: Option<u32>,
    /// Coded sizes are padded to a multiple of this.
    pub alignment: Option<u32>,
    /// Most 16x16 macroblocks in one picture.
    pub max_mbs: Option<u64>,
    /// Most macroblocks per second (the pixel rate), whatever the size and frame rate.
    pub max_mbs_per_sec: Option<u64>,
    pub max_kbps: Option<u32>,
    pub pixel_formats: Option<Vec<PixelFormat>>,
}

impl EncoderLimits {
    /// Whether a `w`x`h` stream at `fps` keeps within the limits: `Some(false)` if one
    /// is exceeded, `None` if one that applies is unknown (open a session to find out).
    pub fn admits(&self, w: u32, h: u32, fps: u32) -> Option<bool> {
        let mbs = w.div_ceil(16) as u64 * h.div_ceil(16) as u64;
        let checks = [
            self.min_width.map(|m| w >= m),
            self.max_width.map(|m| w <= m),
            self.min_height.map(|m| h >= m),
            self.max_height.map(|m| h <= m),
            self.max_mbs.map(|m| mbs <= m),
            self.max_mbs_per_sec
                .map(|m| mbs.saturating_mul(fps as u64) <= m),
        ];
        if checks.contains(&Some(false)) {
            Some(false)
        } else if checks.contains(&None) {
            None
        } else {
            Some(true)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncoderCapability {
    pub codec: Codec,
    pub backend: Backend,
    pub acceleration: Acceleration,
    /// The device or encoder instance: a render node, a GPU, an encoder ID.
    pub device_id: String,
    pub profiles: Vec<ProfileCapability>,
    pub limits: EncoderLimits,
    /// Encodes without lookahead or reordering.
    pub low_latency: Option<bool>,
    /// Emits an IDR when asked.
    pub keyframe_control: Option<bool>,
    /// This build has an encoder for it. Set by the registry, never by a probe.
    pub implemented: bool,
    pub status: ProbeStatus,
}

impl EncoderCapability {
    pub fn new(
        codec: Codec,
        backend: Backend,
        acceleration: Acceleration,
        device_id: impl Into<String>,
    ) -> Self {
        EncoderCapability {
            codec,
            backend,
            acceleration,
            device_id: device_id.into(),
            profiles: Vec::new(),
            limits: EncoderLimits::default(),
            low_latency: None,
            keyframe_control: None,
            implemented: false,
            status: ProbeStatus::Discovered,
        }
    }

    /// Can carry a stream: implemented, verified, 8-bit 4:2:0, and known to encode with
    /// low latency and IDRs on request. Unknown support is not support.
    pub fn eligible(&self) -> bool {
        self.implemented
            && self.status == ProbeStatus::Verified
            && self.low_latency == Some(true)
            && self.keyframe_control == Some(true)
            && self.profiles.iter().any(ProfileCapability::baseline)
    }
}

/// A probe finding that is no encoder: a missing library, a device that can't be
/// opened, a probe that failed, hung or crashed its child process.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub backend: Backend,
    pub device: Option<String>,
    pub reason: String,
}

#[derive(Debug, Default)]
pub struct Probe {
    pub caps: Vec<EncoderCapability>,
    pub diagnostics: Vec<Diagnostic>,
}

impl Probe {
    pub fn note(&mut self, backend: Backend, device: Option<&str>, reason: impl Into<String>) {
        self.diagnostics.push(Diagnostic {
            backend,
            device: device.map(str::to_owned),
            reason: reason.into(),
        });
    }

    pub fn failed(backend: Backend, reason: impl Into<String>) -> Probe {
        let mut p = Probe::default();
        p.note(backend, None, reason);
        p
    }
}

/// One backend's discovery, swappable for a fake in tests.
pub trait ProbeProvider: Send {
    fn backend(&self) -> Backend;
    fn probe(&self) -> Probe;
    /// Ends on its own in bounded time (a child it kills at its own timeout), so the
    /// registry waits for it past its deadline rather than leave it running.
    fn bounded(&self) -> bool {
        false
    }
}

/// The host's encoders and what the probes said about the rest.
#[derive(Debug, Default, Serialize)]
pub struct Registry {
    pub encoders: Vec<EncoderCapability>,
    pub diagnostics: Vec<Diagnostic>,
}

/// How long one backend may take; a driver that hangs is left behind.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

impl Registry {
    /// Runs every provider at once, each on its own thread.
    pub fn probe(providers: Vec<Box<dyn ProbeProvider>>, timeout: Duration) -> Registry {
        let (tx, rx) = mpsc::channel();
        let backends: Vec<Backend> = providers.iter().map(|p| p.backend()).collect();
        let bounded: Vec<bool> = providers.iter().map(|p| p.bounded()).collect();
        for (i, p) in providers.into_iter().enumerate() {
            let sender = tx.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("probe-{}", p.backend().name()))
                .spawn(move || {
                    let _ = sender.send((i, p.probe()));
                });
            if let Err(e) = spawned {
                let _ = tx.send((
                    i,
                    Probe::failed(backends[i], format!("no probe thread: {e}")),
                ));
            }
        }
        drop(tx);
        let deadline = Instant::now() + timeout;
        let mut done: Vec<Option<Probe>> = backends.iter().map(|_| None).collect();
        let mut late = false;
        while done
            .iter()
            .zip(&bounded)
            .any(|(d, &b)| d.is_none() && (b || !late))
        {
            let got = if late {
                rx.recv().ok()
            } else {
                match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(r) => Some(r),
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        late = true;
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => None,
                }
            };
            match got {
                Some((i, r)) => done[i] = Some(r),
                None => break,
            }
        }
        let mut reg = Registry::default();
        for (b, r) in backends.into_iter().zip(done) {
            let r = r.unwrap_or_else(|| {
                Probe::failed(
                    b,
                    format!("probe did not finish in {} s", timeout.as_secs()),
                )
            });
            reg.encoders.extend(r.caps.into_iter().map(|mut c| {
                c.implemented = c.backend.implements(c.codec);
                c
            }));
            reg.diagnostics.extend(r.diagnostics);
        }
        reg
    }

    /// The backends a stream of `codec` could use, in probe order.
    pub fn eligible(&self, codec: Codec) -> impl Iterator<Item = &EncoderCapability> {
        self.encoders
            .iter()
            .filter(move |c| c.codec == codec && c.eligible())
    }

    /// `backend`'s eligible `codec` encoder, or why there is none.
    pub fn check(&self, backend: Backend, codec: Codec) -> Result<&EncoderCapability, String> {
        if let Some(c) = self.eligible(codec).find(|c| c.backend == backend) {
            return Ok(c);
        }
        let why: Vec<String> = self
            .encoders
            .iter()
            .filter(|c| c.backend == backend && c.codec == codec)
            .map(|c| format!("{}: {}", c.device_id, describe(c)))
            .chain(
                self.diagnostics
                    .iter()
                    .filter(|d| d.backend == backend)
                    .map(|d| d.reason.clone()),
            )
            .collect();
        Err(if why.is_empty() {
            format!("no {} {codec:?} encoder found", backend.name())
        } else {
            format!(
                "no usable {} {codec:?} encoder ({})",
                backend.name(),
                why.join("; ")
            )
        })
    }

    pub fn log(&self) {
        for c in &self.encoders {
            log::info!(
                "encoder {:?} {} ({:?}, {}): {}",
                c.codec,
                c.backend.name(),
                c.acceleration,
                c.device_id,
                describe(c)
            );
        }
        for d in &self.diagnostics {
            log::info!(
                "encoder probe {}{}: {}",
                d.backend.name(),
                d.device
                    .as_ref()
                    .map(|d| format!(" {d}"))
                    .unwrap_or_default(),
                d.reason
            );
        }
    }
}

fn describe(c: &EncoderCapability) -> String {
    let state = match &c.status {
        ProbeStatus::Verified => "verified".to_string(),
        ProbeStatus::Discovered => "discovered".to_string(),
        ProbeStatus::ProbeOnly => "probe-only".to_string(),
        ProbeStatus::Disabled => "disabled (--experimental-hw-probes)".to_string(),
        ProbeStatus::Unavailable(r) => format!("unavailable: {r}"),
    };
    if c.eligible() {
        format!("{state}, eligible")
    } else if !c.implemented {
        format!("{state}, no encoder in this build")
    } else {
        state
    }
}

/// The providers for this platform and build. `hardware` false keeps to the software
/// encoders; the VA-API, NVENC and Media Foundation probes are experimental and run only
/// with `experimental`. Hardware probes run in a child process when this executable
/// takes `--probe-encoders` (see [`super::isolate`]), else in the server itself.
pub fn host_providers(hardware: bool, experimental: bool) -> Vec<Box<dyn ProbeProvider>> {
    let mut v: Vec<Box<dyn ProbeProvider>> = vec![Box::new(Software(Backend::OpenH264))];
    #[cfg(feature = "x264")]
    v.push(Box::new(Software(Backend::X264)));
    if !hardware {
        return v;
    }
    for p in hardware_providers(true) {
        let b = p.backend();
        if b != Backend::VideoToolbox && !experimental {
            v.push(Box::new(Disabled(b)));
        } else if let Some(c) = super::isolate::child(b, experimental, PROBE_TIMEOUT) {
            v.push(Box::new(c));
        } else {
            v.push(p);
        }
    }
    v
}

/// The whole startup probe.
pub fn host_probe(hardware: bool, experimental: bool) -> Registry {
    Registry::probe(host_providers(hardware, experimental), PROBE_TIMEOUT)
}

/// This platform's hardware probes, run in-process: the experimental ones only with
/// `experimental`.
#[cfg_attr(target_os = "macos", allow(clippy::vec_init_then_push))]
pub fn hardware_providers(experimental: bool) -> Vec<Box<dyn ProbeProvider>> {
    let mut v: Vec<Box<dyn ProbeProvider>> = Vec::new();
    #[cfg(target_os = "macos")]
    v.push(Box::new(super::videotoolbox::Probe));
    if !experimental {
        return v;
    }
    #[cfg(target_os = "linux")]
    v.push(Box::new(super::vaapi::Probe));
    #[cfg(any(target_os = "linux", windows))]
    v.push(Box::new(super::nvenc::Probe));
    #[cfg(windows)]
    v.push(Box::new(super::mf::Probe));
    v
}

/// Stands in for a probe left off: loads nothing, reports each codec as disabled.
struct Disabled(Backend);

impl ProbeProvider for Disabled {
    fn backend(&self) -> Backend {
        self.0
    }

    fn probe(&self) -> Probe {
        let caps = [Codec::H264, Codec::Hevc, Codec::Av1]
            .into_iter()
            .map(|codec| {
                let mut c =
                    EncoderCapability::new(codec, self.0, Acceleration::Hardware, "not probed");
                c.status = ProbeStatus::Disabled;
                c
            })
            .collect();
        Probe {
            caps,
            diagnostics: Vec::new(),
        }
    }
}

/// A compiled-in H.264 encoder, verified by encoding one small picture.
pub(crate) struct Software(pub Backend);

impl ProbeProvider for Software {
    fn backend(&self) -> Backend {
        self.0
    }

    fn probe(&self) -> Probe {
        let mut c = EncoderCapability::new(Codec::H264, self.0, Acceleration::Software, "cpu");
        c.profiles = vec![match self.0 {
            Backend::OpenH264 => ProfileCapability::new("constrained_baseline", 8, Chroma::Yuv420),
            _ => ProfileCapability::new("high", 8, Chroma::Yuv420),
        }];
        c.limits = EncoderLimits {
            min_width: Some(1),
            min_height: Some(1),
            alignment: Some(2),
            max_mbs: self.0.max_mbs().map(|m| m as u64),
            pixel_formats: Some(vec![PixelFormat::I420]),
            ..Default::default()
        };
        c.low_latency = Some(true);
        c.keyframe_control = Some(true);
        c.status = match sample_encode(self.0, Codec::H264) {
            Ok(()) => ProbeStatus::Verified,
            Err(e) => ProbeStatus::Unavailable(e),
        };
        Probe {
            caps: vec![c],
            diagnostics: Vec::new(),
        }
    }
}

/// Opens `backend` at 64x64 and asks it for an IDR on a flat grey picture.
pub fn sample_encode(backend: Backend, codec: Codec) -> Result<(), String> {
    let mut enc = super::open(&EncoderConfig {
        backend,
        codec,
        width: 64,
        height: 64,
        fps: 30,
        threads: 1,
        rate: RateConfig::new(24.0, 2000, 30),
        best_quality: 24.0,
        preset: "ultrafast".into(),
    })?;
    let (y, uv) = (vec![128u8; 64 * 64], vec![128u8; 32 * 32]);
    let frame = FrameView {
        width: 64,
        height: 64,
        y: &y,
        u: &uv,
        v: &uv,
        stride_y: 64,
        stride_uv: 32,
    };
    let mut aus = enc.encode(
        &frame,
        super::EncodeOptions {
            force_keyframe: true,
            ..Default::default()
        },
    )?;
    aus.extend(enc.drain()?);
    if aus.first().is_some_and(|a| a.keyframe) {
        Ok(())
    } else {
        Err("sample encode produced no IDR".into())
    }
}
