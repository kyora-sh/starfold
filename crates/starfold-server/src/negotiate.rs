//! Codec negotiation (protocol 2): the server offers the codecs its eligible encoders
//! produce, each viewer answers with the exact decoder configurations it tested, and
//! every viewer is routed to an encoder cohort.
//!
//! At most two cohorts run at once: the base one (recording, protocol 1 viewers when it
//! is H.264, and v2 viewers that decode nothing better) and one "best" cohort, which may
//! be the other base encoder for viewers that can't decode the first. A viewer whose
//! best choice would need a third joins the closest cohort it can decode.
//!
//! The base cohort is H.264 where there is an H.264 encoder (an eligible hardware one,
//! or x264), else AV1 from libaom. On a host with an eligible hardware encoder the best
//! cohort stays hardware; elsewhere it is AV1 from libaom.

use crate::encoder::caps::{Acceleration, Registry};
use crate::encoder::{
    Backend, BitstreamFormat, Codec, Selection, StreamConfig, SOFTWARE, SOFTWARE_CODEC,
};
use serde::{Deserialize, Serialize};

pub const PROTOCOL: u32 = 2;
/// The most decoder configurations one capability hello may list.
pub const MAX_DECODE_ENTRIES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Candidate {
    pub codec: Codec,
    pub backend: Backend,
    pub hardware: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Implementation {
    #[serde(rename = "webcodecs")]
    WebCodecs,
    #[serde(rename = "dav1d-wasm")]
    Dav1dWasm,
}

/// One configuration the viewer's decoder accepted, exactly as it was tested.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DecodeCap {
    pub codec: String,
    pub coded_width: u32,
    pub coded_height: u32,
    pub implementation: Implementation,
    #[serde(default)]
    pub hardware_acceleration: Option<String>,
}

/// What a viewer decodes the stream with: a cohort's encoder and the viewer's decoder.
/// Legacy viewers have no implementation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Choice {
    pub cand: Candidate,
    pub implementation: Option<Implementation>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DecoderConfig {
    pub codec: String,
    pub coded_width: u32,
    pub coded_height: u32,
}

impl DecoderConfig {
    pub fn of(c: &StreamConfig) -> Self {
        DecoderConfig {
            codec: c.codec_string.clone(),
            coded_width: c.coded.0,
            coded_height: c.coded.1,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StripeOffer {
    pub i: usize,
    pub y: u32,
    pub h: u32,
    pub config: DecoderConfig,
}

/// A codec the server can stream right now, with the configuration of every stripe.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Offer {
    pub codec: Codec,
    pub format: &'static str,
    pub stripes: Vec<StripeOffer>,
    #[serde(skip)]
    pub cand: Candidate,
}

pub fn format_name(f: BitstreamFormat) -> &'static str {
    match f {
        BitstreamFormat::AnnexB => "annexb",
        BitstreamFormat::Av1LowOverhead => "av1-low-overhead",
        #[cfg(feature = "libvpx")]
        BitstreamFormat::Vpx => "vpx",
    }
}

impl Offer {
    /// Whether the viewer's tested configurations cover every stripe with `imp`.
    fn decodable(&self, caps: &[DecodeCap], imp: Implementation) -> bool {
        if imp == Implementation::Dav1dWasm && self.codec != Codec::Av1 {
            return false;
        }
        self.stripes.iter().all(|s| {
            caps.iter().any(|c| {
                c.implementation == imp
                    && c.codec == s.config.codec
                    && c.coded_width == s.config.coded_width
                    && c.coded_height == s.config.coded_height
            })
        })
    }

    /// The server's `codec` message announcing this configuration to a viewer.
    pub fn announce(&self, epoch: u32, w: u32, h: u32) -> String {
        serde_json::json!({
            "t": "codec",
            "epoch": epoch,
            "codec": self.codec,
            "backend": self.cand.backend,
            "hardware": self.cand.hardware,
            "format": self.format,
            "w": w,
            "h": h,
            "stripes": self.stripes,
        })
        .to_string()
    }
}

/// The close code for a protocol 1 viewer while the base cohort is not H.264.
pub const NO_H264_CLOSE: u16 = 4406;
/// Its reason (a close reason holds at most 123 bytes).
pub const NO_H264_REASON: &str =
    "this server streams AV1 and protocol 1 decodes only H.264: use a protocol 2 client";

/// The server's answer when nothing it offers decodes on the viewer.
pub fn no_codec(epoch: u32) -> String {
    serde_json::json!({"t": "codec", "epoch": epoch, "error": "no-common-codec"}).to_string()
}

fn codec_rank(c: Codec) -> u8 {
    match c {
        Codec::Av1 => 0,
        Codec::Hevc => 1,
        Codec::H264 => 2,
        #[cfg(feature = "libvpx")]
        Codec::Vp8 | Codec::Vp9 => 1,
    }
}

/// The base cohort's encoder `sel` stands for here: the pinned one, which must be eligible
/// (for H.264 if it encodes that, else for AV1), or the first eligible hardware H.264 one
/// that isn't experimental, else [`SOFTWARE`].
pub fn base_backend(reg: &Registry, sel: Selection) -> Result<Candidate, String> {
    let b = match sel {
        Selection::Pinned(b) => b,
        Selection::Auto => reg
            .eligible(Codec::H264)
            .find(|e| e.acceleration == Acceleration::Hardware && !e.backend.experimental())
            .map_or(SOFTWARE, |e| e.backend),
        #[cfg(feature = "sw-arm")]
        Selection::Arm(..) => SOFTWARE,
    };
    let codec = match b.implements(Codec::H264) {
        true => Codec::H264,
        false => Codec::Av1,
    };
    reg.check(b, codec).map(|e| Candidate {
        codec,
        backend: b,
        hardware: e.acceleration == Acceleration::Hardware,
    })
}

/// The encoders a stream may use, best first ([`rank`]: hardware before software, then
/// AV1, HEVC, H.264): the eligible AV1 and HEVC ones and the base one (`base_backend`),
/// then [`SOFTWARE`] for when a hardware base one fails. An experimental backend takes part only when it is the base one,
/// i.e. pinned with `--encoder`.
pub fn candidates(reg: &Registry, base: Candidate) -> Vec<Candidate> {
    let mut v: Vec<Candidate> = Vec::new();
    for codec in [Codec::Av1, Codec::Hevc] {
        for e in reg
            .eligible(codec)
            .filter(|e| !e.backend.experimental() || e.backend == base.backend)
        {
            let c = Candidate {
                codec,
                backend: e.backend,
                hardware: e.acceleration == Acceleration::Hardware,
            };
            if !v
                .iter()
                .any(|x| x.codec == c.codec && x.backend == c.backend)
            {
                v.push(c);
            }
        }
    }
    if !v.contains(&base) {
        v.push(base);
    }
    v.sort_by_key(|c| rank(*c));
    let software = Candidate {
        codec: SOFTWARE_CODEC,
        backend: SOFTWARE,
        hardware: false,
    };
    if base.hardware
        && !v.contains(&software)
        && reg.eligible(software.codec).any(|e| e.backend == software.backend)
    {
        v.push(software);
    }
    v
}

/// Whether every eligible hardware candidate (not an experimental one, which only a pin
/// brings in) ranks before every software one: a CPU encoder ahead of an idle hardware one
/// costs cores for every viewer. False ranks by codec first, so libaom's AV1 would lead on
/// a host with hardware H.264 or HEVC.
const HARDWARE_FIRST: bool = true;

/// A candidate's place in [`candidates`], lowest first.
fn rank(c: Candidate) -> (bool, u8, bool) {
    let leads = c.hardware && !c.backend.experimental();
    (HARDWARE_FIRST && !leads, codec_rank(c.codec), !c.hardware)
}

/// The base cohort's encoders in the order it falls back through them: the H.264 ones in
/// `cands`' order, then, where none of them is software, libaom's AV1.
pub fn base_chain(cands: &[Candidate]) -> Vec<Candidate> {
    let mut v: Vec<Candidate> = cands
        .iter()
        .copied()
        .filter(|c| c.codec == Codec::H264)
        .collect();
    if !v.iter().any(|c| !c.hardware) {
        v.extend(cands.iter().copied().filter(|c| software_av1(*c)).take(1));
    }
    v
}

/// libaom's AV1, the base encoder of a build without x264.
fn software_av1(c: Candidate) -> bool {
    #[cfg(feature = "libaom")]
    if c.backend == Backend::Libaom && c.codec == Codec::Av1 {
        return true;
    }
    #[cfg(test)]
    if c.backend == Backend::FakeAv1 {
        return c.codec == Codec::Av1;
    }
    let _ = c;
    false
}

/// A bench arm's candidates: the arm first, then only the base cohort's. Fails if the arm
/// can't encode here, so a bench run never measures the fallback by mistake.
#[cfg(feature = "sw-arm")]
pub fn arm_candidates(
    backend: Backend,
    codec: Codec,
    cands: Vec<Candidate>,
) -> Result<Vec<Candidate>, String> {
    crate::encoder::caps::sample_encode(backend, codec)?;
    let arm = Candidate {
        codec,
        backend,
        hardware: false,
    };
    Ok(std::iter::once(arm)
        .chain(base_chain(&cands).into_iter().filter(|c| *c != arm))
        .collect())
}

/// Each codec's first candidate, as the startup log and /stats name it.
pub fn chosen(cands: &[Candidate]) -> Vec<Candidate> {
    let mut v: Vec<Candidate> = Vec::new();
    for c in cands {
        if !v.iter().any(|x| x.codec == c.codec) {
            v.push(*c);
        }
    }
    v
}

/// A viewer's choices, best first: every offer it decodes with WebCodecs in the
/// server's order, then AV1 through dav1d-wasm, which is only ever a last resort.
pub fn preferences(caps: &[DecodeCap], rejected: &[Choice], offers: &[Offer]) -> Vec<Choice> {
    let mut v = Vec::new();
    for imp in [Implementation::WebCodecs, Implementation::Dav1dWasm] {
        for o in offers {
            let c = Choice {
                cand: o.cand,
                implementation: Some(imp),
            };
            if o.decodable(caps, imp) && !rejected.contains(&c) && !v.contains(&c) {
                v.push(c);
            }
        }
    }
    v
}

pub struct Viewer<'a> {
    pub id: u64,
    pub legacy: bool,
    pub caps: &'a [DecodeCap],
    pub rejected: &'a [Choice],
}

#[derive(Debug, PartialEq, Eq)]
pub struct Plan {
    /// The cohort besides the base one, if any viewer gets it.
    pub best: Option<Candidate>,
    /// Each viewer's choice; None: nothing the server can stream decodes there (for a
    /// protocol 1 viewer: the base cohort is not H.264).
    pub picks: Vec<(u64, Option<Choice>)>,
}

/// Routes `viewers` (in the order they joined) to at most two cohorts. The current best
/// cohort stays while it is still some viewer's first choice, so viewers aren't moved
/// back and forth as others come and go; otherwise the earliest viewer whose first
/// choice is another cohort decides it. Protocol 1 viewers get the base cohort if it is
/// H.264, else nothing.
pub fn plan(
    viewers: &[Viewer<'_>],
    offers: &[Offer],
    base: Candidate,
    current: Option<Candidate>,
) -> Plan {
    let prefs: Vec<Vec<Choice>> = viewers
        .iter()
        .map(|v| {
            if v.legacy {
                Vec::new()
            } else {
                preferences(v.caps, v.rejected, offers)
            }
        })
        .collect();
    let firsts = || {
        prefs
            .iter()
            .filter_map(|p| p.first())
            .map(|c| c.cand)
            .filter(|c| *c != base)
    };
    let best = current
        .filter(|b| firsts().any(|c| c == *b))
        .or_else(|| firsts().next());
    let picks = viewers
        .iter()
        .zip(&prefs)
        .map(|(v, p)| {
            let pick = if v.legacy {
                (base.codec == Codec::H264).then_some(Choice {
                    cand: base,
                    implementation: None,
                })
            } else {
                p.iter()
                    .find(|c| c.cand == base || Some(c.cand) == best)
                    .copied()
            };
            (v.id, pick)
        })
        .collect::<Vec<_>>();
    Plan { best, picks }
}

/// Text messages of protocol 2 from the client (the CSV commands stay as they are).
#[derive(Debug, PartialEq, Deserialize)]
#[serde(tag = "t")]
pub enum ClientMsg {
    #[serde(rename = "hello")]
    Hello {
        v: u32,
        /// The `offer` generation of the server hello these configurations answer.
        #[serde(default)]
        offer: Option<u64>,
        #[serde(default)]
        decode: Vec<serde_json::Value>,
        /// The display's refresh rate the page measured (frames per second); anything but
        /// a number is ignored, not the hello.
        #[serde(default)]
        hz: Option<serde_json::Value>,
    },
    #[serde(rename = "codec-ready")]
    Ready { epoch: u32 },
    #[serde(rename = "codec-error")]
    Error {
        epoch: u32,
        #[serde(default)]
        reason: String,
    },
    #[serde(rename = "ack")]
    Ack { epoch: u32, frame: u32 },
    #[serde(rename = "keyframe")]
    Keyframe { epoch: u32 },
    /// The decoder fell behind twice within 10 s: no more than the base rate for this viewer.
    #[serde(rename = "slow")]
    Slow,
    /// The display's refresh rate, measured after the hello went out.
    #[serde(rename = "hz")]
    Hz {
        #[serde(default)]
        hz: Option<serde_json::Value>,
    },
}

impl ClientMsg {
    pub fn parse(t: &str) -> Option<ClientMsg> {
        serde_json::from_str(t).ok()
    }
}

/// The entries of a capability hello this server understands; others are skipped.
pub fn decode_caps(entries: &[serde_json::Value]) -> Vec<DecodeCap> {
    entries
        .iter()
        .take(MAX_DECODE_ENTRIES)
        .filter_map(|e| serde_json::from_value(e.clone()).ok())
        .collect()
}

/// Video message type bytes: protocol 1 and protocol 2 (with the epoch).
pub const VIDEO_V1: u8 = 0x01;
pub const VIDEO_V2: u8 = 0x05;
/// Header bytes of each.
pub const HDR_V1: usize = 16;
pub const HDR_V2: usize = 20;

/// The protocol 2 form of a protocol 1 video message: the same header with the epoch
/// appended, then the same payload.
pub fn video_v2(v1: &[u8], epoch: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(v1.len() + HDR_V2 - HDR_V1);
    out.extend_from_slice(&v1[..HDR_V1]);
    out[0] = VIDEO_V2;
    out.extend_from_slice(&epoch.to_be_bytes());
    out.extend_from_slice(&v1[HDR_V1..]);
    out
}

#[cfg(test)]
mod tests;
