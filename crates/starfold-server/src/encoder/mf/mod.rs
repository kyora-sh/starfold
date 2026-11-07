//! Media Foundation: discovery of the hardware encoder MFTs for each output format, and
//! EXPERIMENTAL H.264, HEVC and AV1 encoders on them (8-bit 4:2:0, low-latency mode,
//! IDR + P, an IDR on request). None of this has run on real hardware or on Windows yet:
//! `--encoder auto` never picks it, only `--encoder mediafoundation` does.

#[cfg(windows)]
mod com;
mod driver;
mod session;
#[cfg(test)]
mod tests;

#[cfg(windows)]
pub use windows_impl::{child_session, open, Probe};

/// A session for `cfg` on `drv`.
pub(super) fn session(
    drv: Box<dyn driver::Driver>,
    cfg: &super::EncoderConfig,
) -> Result<Box<dyn super::VideoEncoder>, super::EncodeError> {
    Ok(Box::new(session::Session::new(drv, cfg)?))
}

#[cfg(windows)]
mod windows_impl {
    use super::super::caps::{
        Acceleration, Chroma, EncoderCapability, Probe as Found, ProbeProvider, ProbeStatus,
        ProfileCapability,
    };
    use super::super::{hw, Backend, Codec, EncodeError, EncoderConfig, VideoEncoder};
    use super::com::{self, MfDriver};
    use windows::Win32::Media::MediaFoundation::{
        IMFActivate, MFT_ENUM_HARDWARE_VENDOR_ID_Attribute, MFT_FRIENDLY_NAME_Attribute,
    };

    /// The side of the picture a probe encodes: the smallest `open` codes.
    const SAMPLE_SIDE: u32 = 128;

    const CODECS: [Codec; 3] = [Codec::H264, Codec::Hevc, Codec::Av1];

    /// The hardware MFTs that encode `codec`, best first, each with its device name.
    fn encoders(codec: Codec) -> Result<Vec<(String, IMFActivate)>, String> {
        com::com()?;
        let mf = com::mf()?;
        let subtype = com::subtype(codec).ok_or_else(|| format!("no {codec:?} subtype"))?;
        let all = unsafe { mf.encoders(subtype) }?;
        Ok(all
            .into_iter()
            .map(|a| (unsafe { com::device(&a) }, a))
            .collect())
    }

    fn open_on(a: &IMFActivate, cfg: &EncoderConfig) -> Result<Box<dyn VideoEncoder>, EncodeError> {
        super::session(Box::new(MfDriver::open(a)?), cfg)
    }

    pub struct Probe;

    impl ProbeProvider for Probe {
        fn backend(&self) -> Backend {
            Backend::MediaFoundation
        }

        fn probe(&self) -> Found {
            let mut found = Found::default();
            for codec in CODECS {
                let mfts = match encoders(codec) {
                    Ok(m) => m,
                    Err(e) if e.starts_with("MFTEnumEx") => {
                        found.note(Backend::MediaFoundation, None, format!("{codec:?}: {e}"));
                        continue;
                    }
                    Err(e) => return Found::failed(Backend::MediaFoundation, e),
                };
                for (_, a) in mfts {
                    let name = unsafe { com::string(&a, &MFT_FRIENDLY_NAME_Attribute) }
                        .unwrap_or_default();
                    let id =
                        match unsafe { com::string(&a, &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute) } {
                            Some(v) => format!("{name} ({v})"),
                            None => name,
                        };
                    let mut c = EncoderCapability::new(
                        codec,
                        Backend::MediaFoundation,
                        Acceleration::Hardware,
                        id,
                    );
                    verify(&mut c, &a);
                    found.caps.push(c);
                }
            }
            found
        }
    }

    /// Marks `c` verified if a session on `a` encodes a sample, unavailable if not.
    fn verify(c: &mut EncoderCapability, a: &IMFActivate) {
        let cfg = hw::sample_config(
            Backend::MediaFoundation,
            c.codec,
            SAMPLE_SIDE,
            SAMPLE_SIDE,
            30,
        );
        let r = open_on(a, &cfg)
            .and_then(|mut enc| hw::sample_with(enc.as_mut(), SAMPLE_SIDE, SAMPLE_SIDE, true));
        match r {
            Ok(()) => {
                let profile = if c.codec == Codec::H264 {
                    "high"
                } else {
                    "main"
                };
                c.profiles = vec![ProfileCapability::new(profile, 8, Chroma::Yuv420)];
                c.status = ProbeStatus::Verified;
                c.low_latency = Some(true);
                c.keyframe_control = Some(true);
            }
            Err(e) => c.status = ProbeStatus::Unavailable(format!("sample encode: {e}")),
        }
    }

    /// Opens a Media Foundation encoder for `cfg` on the first hardware MFT that takes
    /// it. Each MFT's session is first opened and run in a child process (when this
    /// executable takes `--probe-encoders`); one that crashes, hangs or fails there is
    /// never opened here.
    pub fn open(cfg: &EncoderConfig) -> Result<Box<dyn VideoEncoder>, EncodeError> {
        let mut last = EncodeError(format!(
            "no hardware Media Foundation {:?} encoder",
            cfg.codec
        ));
        for (device, a) in encoders(cfg.codec).map_err(EncodeError)? {
            let pre = hw::preflight(cfg, &device, |spec| {
                super::super::isolate::session(
                    Backend::MediaFoundation,
                    spec,
                    super::super::caps::PROBE_TIMEOUT,
                )
            });
            match pre.and_then(|()| open_on(&a, cfg)) {
                Ok(enc) => return Ok(enc),
                Err(e) => last = EncodeError(format!("{device}: {}", e.0)),
            }
        }
        Err(last)
    }

    /// The child's side of [`open`]: opens the session `spec` names on its MFT and
    /// encodes a sample, in-process.
    pub fn child_session(spec: &str) -> Result<(), String> {
        let (cfg, device) = hw::parse_spec(Backend::MediaFoundation, spec)
            .ok_or_else(|| format!("bad session {spec:?}"))?;
        let (_, a) = encoders(cfg.codec)?
            .into_iter()
            .find(|(d, _)| *d == device)
            .ok_or_else(|| format!("no MFT {device}"))?;
        let mut enc = open_on(&a, &cfg)?;
        hw::sample_with(enc.as_mut(), cfg.width, cfg.height, true).map_err(String::from)
    }
}
