//! The video encoder boundary: a backend turns I420 frames into access units, and
//! callers only see the types here.

#[cfg(feature = "libaom")]
mod aom;
#[cfg(any(feature = "libaom", feature = "svt-av1", test))]
#[cfg_attr(not(any(feature = "libaom", feature = "svt-av1")), allow(dead_code))]
mod av1;
pub mod caps;
mod h264;
#[cfg(any(target_os = "macos", test, feature = "kvazaar", feature = "x265"))]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod hevc;
pub mod isolate;
#[cfg(feature = "kvazaar")]
mod kvazaar;
#[cfg(windows)]
mod mf;
#[cfg(any(target_os = "linux", windows))]
mod nvenc;
pub(crate) mod openh264;
#[cfg(feature = "svt-av1")]
mod svt;
#[cfg(test)]
mod tests;
#[cfg(target_os = "linux")]
mod vaapi;
#[cfg(target_os = "macos")]
mod videotoolbox;
#[cfg(feature = "libvpx")]
mod vpx;
#[cfg(feature = "x264")]
mod x264;
#[cfg(feature = "x265")]
mod x265;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    H264,
    Hevc,
    Av1,
    #[cfg(feature = "libvpx")]
    Vp8,
    #[cfg(feature = "libvpx")]
    Vp9,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BitstreamFormat {
    AnnexB,
    Av1LowOverhead,
    #[cfg(feature = "libvpx")]
    Vpx,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColorInfo {
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
    pub full_range: bool,
}

impl ColorInfo {
    /// sRGB content (BT.709 primaries, sRGB transfer) in BT.601 limited range, matching
    /// libyuv ARGBToI420 and the macOS NV12 capture.
    pub const SRGB_BT601_LIMITED: ColorInfo = ColorInfo {
        primaries: 1,
        transfer: 13,
        matrix: 6,
        full_range: false,
    };
}

/// What a decoder needs to know about the stream that follows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamConfig {
    pub codec: Codec,
    /// The WebCodecs codec string, read from the stream's own parameter sets.
    pub codec_string: String,
    pub coded: (u32, u32),
    pub display: (u32, u32),
    pub format: BitstreamFormat,
    pub color: ColorInfo,
}

/// One I420 picture; chroma planes are `width.div_ceil(2)` wide.
pub struct FrameView<'a> {
    pub width: u32,
    pub height: u32,
    pub y: &'a [u8],
    pub u: &'a [u8],
    pub v: &'a [u8],
    pub stride_y: usize,
    pub stride_uv: usize,
}

impl FrameView<'_> {
    /// Fails unless every plane holds the picture at its stride, and strides fit a C int.
    pub fn check(&self) -> Result<(), EncodeError> {
        let bad = || {
            EncodeError(format!(
                "I420 planes don't hold a {}x{} picture",
                self.width, self.height
            ))
        };
        let (w, h) = (self.width as usize, self.height as usize);
        let plane = |len: usize, stride: usize, w: usize, h: usize| {
            let need = (h.max(1) - 1).checked_mul(stride)?.checked_add(w)?;
            (w > 0 && stride >= w && stride <= i32::MAX as usize && len >= need).then_some(())
        };
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        plane(self.y.len(), self.stride_y, w, h)
            .and_then(|_| plane(self.u.len(), self.stride_uv, cw, ch))
            .and_then(|_| plane(self.v.len(), self.stride_uv, cw, ch))
            .ok_or_else(bad)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RateConfig {
    /// x264's CRF; backends without one ignore it.
    pub quality: f32,
    /// Bitrate cap in kbit/s (0 = uncapped).
    pub max_kbps: u32,
    /// Rate buffer in kbit (x264's VBV).
    pub buf_kbit: u32,
    /// Frames per second to budget each frame for: the rate frames actually arrive at.
    pub fps: u32,
}

impl RateConfig {
    /// A buffer of ~1.25 frames at `max_kbps`.
    pub fn new(quality: f32, max_kbps: u32, fps: u32) -> Self {
        RateConfig {
            quality,
            max_kbps,
            buf_kbit: max_kbps / fps.max(1) * 5 / 4,
            fps,
        }
    }
}

/// One pass of the paint-over after motion, for a backend that sharpens by QP.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Refine {
    /// The QP this pass aims for.
    pub qp: u8,
    /// What the frame may cost (None: uncapped).
    pub max_bytes: Option<usize>,
}

#[derive(Default)]
pub struct EncodeOptions<'a> {
    pub pts_ms: i64,
    pub force_keyframe: bool,
    /// One byte per macroblock (row-major, `width.div_ceil(16)` per row), nonzero where
    /// nothing changed since the last frame.
    pub static_mbs: Option<&'a mut [u8]>,
    /// Only with `Backend::refines_by_qp`; no frame comes out if no step of it fits.
    pub refine: Option<Refine>,
}

pub struct AccessUnit {
    pub pts_ms: i64,
    /// An IDR with its parameter sets: decodable on its own.
    pub keyframe: bool,
    pub data: Vec<u8>,
    /// Set on the first unit and whenever the stream's parameters change.
    pub config: Option<StreamConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodeError(pub String);

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<EncodeError> for String {
    fn from(e: EncodeError) -> String {
        e.0
    }
}

pub trait VideoEncoder: Send {
    fn set_rate(&mut self, rate: RateConfig) -> Result<(), EncodeError>;
    /// Zero units for a skipped frame; delayed output carries its own frame's pts.
    fn encode(
        &mut self,
        frame: &FrameView<'_>,
        opts: EncodeOptions<'_>,
    ) -> Result<Vec<AccessUnit>, EncodeError>;
    fn drain(&mut self) -> Result<Vec<AccessUnit>, EncodeError>;
    /// Whether a pass like `r` would still sharpen something within its budget.
    fn refinable(&self, _r: Refine) -> bool {
        false
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    OpenH264,
    #[cfg(feature = "x264")]
    X264,
    #[cfg(feature = "libvpx")]
    Libvpx,
    #[cfg(feature = "libaom")]
    Libaom,
    #[cfg(feature = "svt-av1")]
    SvtAv1,
    #[cfg(feature = "kvazaar")]
    Kvazaar,
    #[cfg(feature = "x265")]
    X265,
    VideoToolbox,
    Vaapi,
    Nvenc,
    MediaFoundation,
}

impl Backend {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "openh264" => Ok(Backend::OpenH264),
            #[cfg(feature = "x264")]
            "x264" => Ok(Backend::X264),
            #[cfg(not(feature = "x264"))]
            "x264" => Err("this build has no x264 (build with `--features x264`)".into()),
            #[cfg(target_os = "macos")]
            "videotoolbox" => Ok(Backend::VideoToolbox),
            #[cfg(not(target_os = "macos"))]
            "videotoolbox" => Err("videotoolbox is macOS-only".into()),
            "vaapi" | "nvenc" | "mediafoundation" => {
                Err(format!("{s} is probed (see /stats) but has no encoder yet"))
            }
            _ => Err(format!(
                "unknown encoder {s:?} (auto, openh264{}{})",
                if cfg!(feature = "x264") { ", x264" } else { "" },
                if cfg!(target_os = "macos") {
                    ", videotoolbox"
                } else {
                    ""
                }
            )),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Backend::OpenH264 => "openh264",
            #[cfg(feature = "x264")]
            Backend::X264 => "x264",
            #[cfg(feature = "libvpx")]
            Backend::Libvpx => "libvpx",
            #[cfg(feature = "libaom")]
            Backend::Libaom => "libaom",
            #[cfg(feature = "svt-av1")]
            Backend::SvtAv1 => "svt-av1",
            #[cfg(feature = "kvazaar")]
            Backend::Kvazaar => "kvazaar",
            #[cfg(feature = "x265")]
            Backend::X265 => "x265",
            Backend::VideoToolbox => "videotoolbox",
            Backend::Vaapi => "vaapi",
            Backend::Nvenc => "nvenc",
            Backend::MediaFoundation => "mediafoundation",
        }
    }

    /// Whether this build has an encoder for `codec` on this backend.
    pub fn implements(self, codec: Codec) -> bool {
        match self {
            Backend::OpenH264 => codec == Codec::H264,
            #[cfg(feature = "x264")]
            Backend::X264 => codec == Codec::H264,
            #[cfg(feature = "libvpx")]
            Backend::Libvpx => matches!(codec, Codec::Vp8 | Codec::Vp9),
            #[cfg(feature = "libaom")]
            Backend::Libaom => codec == Codec::Av1,
            #[cfg(feature = "svt-av1")]
            Backend::SvtAv1 => codec == Codec::Av1,
            #[cfg(feature = "kvazaar")]
            Backend::Kvazaar => codec == Codec::Hevc,
            #[cfg(feature = "x265")]
            Backend::X265 => codec == Codec::Hevc,
            Backend::VideoToolbox => {
                cfg!(target_os = "macos") && matches!(codec, Codec::H264 | Codec::Hevc)
            }
            Backend::Vaapi | Backend::Nvenc | Backend::MediaFoundation => false,
        }
    }

    /// The most 16x16 macroblocks one encoder takes, if it has a limit.
    pub fn max_mbs(self) -> Option<usize> {
        match self {
            Backend::OpenH264 => Some(openh264::MAX_MBS),
            #[cfg(feature = "x264")]
            Backend::X264 => None,
            #[cfg(feature = "libvpx")]
            Backend::Libvpx => None,
            #[cfg(feature = "libaom")]
            Backend::Libaom => None,
            #[cfg(feature = "svt-av1")]
            Backend::SvtAv1 => None,
            #[cfg(feature = "kvazaar")]
            Backend::Kvazaar => None,
            #[cfg(feature = "x265")]
            Backend::X265 => None,
            // H.264 level 5.2's frame size; HEVC level 5.1's is about the same.
            Backend::VideoToolbox => Some(openh264::MAX_MBS),
            Backend::Vaapi | Backend::Nvenc | Backend::MediaFoundation => None,
        }
    }

    /// The smallest width and height one encoder takes.
    pub fn min_side(self) -> u32 {
        match self {
            Backend::OpenH264 => 16,
            #[cfg(feature = "x264")]
            Backend::X264 => 2,
            #[cfg(feature = "libvpx")]
            Backend::Libvpx => 16,
            #[cfg(feature = "libaom")]
            Backend::Libaom => 16,
            #[cfg(feature = "svt-av1")]
            Backend::SvtAv1 => 64,
            #[cfg(feature = "kvazaar")]
            Backend::Kvazaar => 64,
            #[cfg(feature = "x265")]
            Backend::X265 => 64,
            Backend::VideoToolbox => 64,
            Backend::Vaapi | Backend::Nvenc | Backend::MediaFoundation => 16,
        }
    }

    /// Sharpens a paint-over with `EncodeOptions::refine`, not `RateConfig::quality`.
    pub fn refines_by_qp(self) -> bool {
        self == Backend::OpenH264
    }

    pub fn fits(self, width: u32, height: u32) -> bool {
        let mbs = width.div_ceil(16) as usize * height.div_ceil(16) as usize;
        self.max_mbs().is_none_or(|m| mbs <= m)
    }
}

/// `--encoder`: `auto` streams each codec from its eligible hardware encoder (H.264
/// from OpenH264 where there is none); a backend name pins the H.264 one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selection {
    Auto,
    Pinned(Backend),
    /// A bench arm: this encoder, with the OpenH264 cohort beside it.
    #[cfg(feature = "sw-arm")]
    Arm(&'static str, Backend, Codec),
}

/// A bench arm's speed from `--preset`: a number, or `default` for an empty preset and
/// for x264's preset name, which the server passes when none is given.
#[cfg(any(feature = "libvpx", feature = "libaom", feature = "svt-av1"))]
fn arm_speed(arm: &str, preset: &str, default: i32) -> Result<std::os::raw::c_int, EncodeError> {
    match preset {
        "" | "ultrafast" => Ok(default),
        p => p
            .parse()
            .map_err(|_| EncodeError(format!("{arm} speed {p:?} is not a number"))),
    }
}

/// The `--encoder` names of this build's bench arms.
#[cfg(feature = "sw-arm")]
pub const ARMS: &[(&str, Backend, Codec)] = &[
    #[cfg(feature = "libvpx")]
    ("libvpx-vp8", Backend::Libvpx, Codec::Vp8),
    #[cfg(feature = "libvpx")]
    ("libvpx-vp9", Backend::Libvpx, Codec::Vp9),
    #[cfg(feature = "libaom")]
    ("libaom", Backend::Libaom, Codec::Av1),
    #[cfg(feature = "svt-av1")]
    ("svt-av1", Backend::SvtAv1, Codec::Av1),
    #[cfg(feature = "kvazaar")]
    ("kvazaar", Backend::Kvazaar, Codec::Hevc),
    #[cfg(feature = "x265")]
    ("x265", Backend::X265, Codec::Hevc),
];

impl Selection {
    pub fn parse(s: &str) -> Result<Self, String> {
        #[cfg(feature = "sw-arm")]
        if let Some(&(name, backend, codec)) = ARMS.iter().find(|a| a.0 == s) {
            return Ok(Selection::Arm(name, backend, codec));
        }
        match s {
            "auto" => Ok(Selection::Auto),
            _ => Backend::parse(s).map(Selection::Pinned),
        }
    }

    pub fn is_arm(self) -> bool {
        #[cfg(feature = "sw-arm")]
        if matches!(self, Selection::Arm(..)) {
            return true;
        }
        false
    }

    pub fn name(self) -> &'static str {
        match self {
            Selection::Auto => "auto",
            Selection::Pinned(b) => b.name(),
            #[cfg(feature = "sw-arm")]
            Selection::Arm(name, ..) => name,
        }
    }
}

#[derive(Clone, Debug)]
pub struct EncoderConfig {
    pub backend: Backend,
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub threads: u32,
    pub rate: RateConfig,
    /// OpenH264's lowest QP: its bitrate mode otherwise spends the whole cap.
    pub best_quality: f32,
    /// x264's preset; a bench arm's preset or speed.
    pub preset: String,
}

/// Opens `cfg.backend`. Sizes it can't code (odd, or below its minimum) are coded
/// larger, the last column and row repeated, and report the real size as `display`.
pub fn open(cfg: &EncoderConfig) -> Result<Box<dyn VideoEncoder>, EncodeError> {
    if !cfg.backend.implements(cfg.codec) {
        return Err(EncodeError(format!(
            "this build has no {} {:?} encoder",
            cfg.backend.name(),
            cfg.codec
        )));
    }
    if cfg.width == 0 || cfg.height == 0 {
        return Err(EncodeError(format!(
            "can't encode a {}x{} picture",
            cfg.width, cfg.height
        )));
    }
    if !cfg.backend.fits(cfg.width, cfg.height) {
        return Err(EncodeError(format!(
            "{} encodes at most {} macroblocks per picture, {}x{} has {} (use more --stripes)",
            cfg.backend.name(),
            cfg.backend.max_mbs().unwrap_or(0),
            cfg.width,
            cfg.height,
            cfg.width.div_ceil(16) * cfg.height.div_ceil(16)
        )));
    }
    let side = |v: u32| (v.max(cfg.backend.min_side()) + 1) & !1;
    let coded = EncoderConfig {
        width: side(cfg.width),
        height: side(cfg.height),
        ..cfg.clone()
    };
    let inner: Box<dyn VideoEncoder> = match cfg.backend {
        Backend::OpenH264 => Box::new(openh264::OpenH264::new(&coded)?),
        #[cfg(feature = "x264")]
        Backend::X264 => Box::new(x264::X264::new(&coded)?),
        #[cfg(feature = "libvpx")]
        Backend::Libvpx => Box::new(vpx::Vpx::new(&coded)?),
        #[cfg(feature = "libaom")]
        Backend::Libaom => Box::new(aom::Aom::new(&coded)?),
        #[cfg(feature = "svt-av1")]
        Backend::SvtAv1 => Box::new(svt::Svt::new(&coded)?),
        #[cfg(feature = "kvazaar")]
        Backend::Kvazaar => Box::new(kvazaar::Kvazaar::new(&coded)?),
        #[cfg(feature = "x265")]
        Backend::X265 => Box::new(x265::X265::new(&coded)?),
        #[cfg(target_os = "macos")]
        Backend::VideoToolbox => Box::new(videotoolbox::VideoToolbox::new(&coded)?),
        b => return Err(EncodeError(format!("no {} encoder", b.name()))),
    };
    if (coded.width, coded.height) == (cfg.width, cfg.height) {
        return Ok(inner);
    }
    Ok(Box::new(Pad {
        inner,
        display: (cfg.width, cfg.height),
        coded: (coded.width, coded.height),
        planes: Default::default(),
    }))
}

/// Copies each picture into the coded size, repeating the last column and row.
struct Pad {
    inner: Box<dyn VideoEncoder>,
    display: (u32, u32),
    coded: (u32, u32),
    planes: [Vec<u8>; 3],
}

fn pad_plane(
    dst: &mut Vec<u8>,
    src: &[u8],
    stride: usize,
    w: usize,
    h: usize,
    pw: usize,
    ph: usize,
) {
    dst.resize(pw * ph, 0);
    for r in 0..ph {
        let s = &src[r.min(h - 1) * stride..][..w];
        let d = &mut dst[r * pw..][..pw];
        d[..w].copy_from_slice(s);
        d[w..].fill(s[w - 1]);
    }
}

impl Pad {
    fn fix(&self, mut aus: Vec<AccessUnit>) -> Vec<AccessUnit> {
        for au in &mut aus {
            if let Some(c) = &mut au.config {
                c.display = self.display;
            }
        }
        aus
    }
}

impl VideoEncoder for Pad {
    fn set_rate(&mut self, rate: RateConfig) -> Result<(), EncodeError> {
        self.inner.set_rate(rate)
    }

    fn encode(
        &mut self,
        f: &FrameView<'_>,
        opts: EncodeOptions<'_>,
    ) -> Result<Vec<AccessUnit>, EncodeError> {
        if (f.width, f.height) != self.display {
            return Err(EncodeError(format!(
                "encoder opened at {}x{}, got {}x{}",
                self.display.0, self.display.1, f.width, f.height
            )));
        }
        f.check()?;
        let (w, h) = (f.width as usize, f.height as usize);
        let (pw, ph) = (self.coded.0 as usize, self.coded.1 as usize);
        let [y, u, v] = &mut self.planes;
        pad_plane(y, f.y, f.stride_y, w, h, pw, ph);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        pad_plane(u, f.u, f.stride_uv, cw, ch, pw / 2, ph / 2);
        pad_plane(v, f.v, f.stride_uv, cw, ch, pw / 2, ph / 2);
        let padded = FrameView {
            width: self.coded.0,
            height: self.coded.1,
            y,
            u,
            v,
            stride_y: pw,
            stride_uv: pw / 2,
        };
        let aus = self.inner.encode(&padded, opts)?;
        Ok(self.fix(aus))
    }

    fn drain(&mut self) -> Result<Vec<AccessUnit>, EncodeError> {
        let aus = self.inner.drain()?;
        Ok(self.fix(aus))
    }

    fn refinable(&self, r: Refine) -> bool {
        self.inner.refinable(r)
    }
}
