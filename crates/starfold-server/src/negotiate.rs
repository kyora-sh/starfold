//! Codec negotiation (protocol 2): the server offers the codecs its eligible encoders
//! produce, each viewer answers with the exact decoder configurations it tested, and
//! every viewer is routed to an encoder cohort.
//!
//! At most two cohorts run at once: the H.264 one (legacy viewers, recording, and v2
//! viewers that decode nothing better) and one "best" cohort, which may be the other
//! H.264 encoder for viewers that can't decode the first. A viewer whose best choice
//! would need a third joins the closest cohort it can decode.

use crate::encoder::caps::{Acceleration, Registry};
use crate::encoder::{Backend, BitstreamFormat, Codec, Selection, StreamConfig};
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

/// The H.264 backend `sel` stands for here: the pinned one, which must be eligible, or
/// the first eligible hardware one, else OpenH264.
pub fn h264_backend(reg: &Registry, sel: Selection) -> Result<Backend, String> {
    let b = match sel {
        Selection::Pinned(b) => b,
        Selection::Auto => reg
            .eligible(Codec::H264)
            .find(|e| e.acceleration == Acceleration::Hardware)
            .map_or(Backend::OpenH264, |e| e.backend),
        #[cfg(feature = "sw-arm")]
        Selection::Arm(..) => Backend::OpenH264,
    };
    reg.check(b, Codec::H264).map(|_| b)
}

/// The encoders a stream may use, best first: eligible AV1, then HEVC (hardware before
/// software), then the H.264 backend (`h264_backend`), which also serves legacy viewers
/// and recording, and OpenH264 for when a hardware one fails.
pub fn candidates(reg: &Registry, h264: Backend) -> Vec<Candidate> {
    let mut v: Vec<Candidate> = Vec::new();
    for codec in [Codec::Av1, Codec::Hevc] {
        for e in reg.eligible(codec) {
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
    v.sort_by_key(|c| (codec_rank(c.codec), !c.hardware));
    let hardware = reg
        .eligible(Codec::H264)
        .any(|e| e.backend == h264 && e.acceleration == Acceleration::Hardware);
    v.push(Candidate {
        codec: Codec::H264,
        backend: h264,
        hardware,
    });
    if hardware
        && reg
            .eligible(Codec::H264)
            .any(|e| e.backend == Backend::OpenH264)
    {
        v.push(Candidate {
            codec: Codec::H264,
            backend: Backend::OpenH264,
            hardware: false,
        });
    }
    v
}

/// A bench arm's candidates: the arm first, then only the H.264 cohort. Fails if the arm
/// can't encode here, so a bench run never measures the fallback by mistake.
#[cfg(feature = "sw-arm")]
pub fn arm_candidates(
    backend: Backend,
    codec: Codec,
    h264: Vec<Candidate>,
) -> Result<Vec<Candidate>, String> {
    crate::encoder::caps::sample_encode(backend, codec)?;
    let arm = Candidate {
        codec,
        backend,
        hardware: false,
    };
    Ok(std::iter::once(arm)
        .chain(h264.into_iter().filter(|c| c.codec == Codec::H264))
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
    /// The cohort besides the H.264 one, if any viewer gets it.
    pub best: Option<Candidate>,
    /// Each viewer's choice; None: nothing the server can stream decodes there.
    pub picks: Vec<(u64, Option<Choice>)>,
}

/// Routes `viewers` (in the order they joined) to at most two cohorts. The current best
/// cohort stays while it is still some viewer's first choice, so viewers aren't moved
/// back and forth as others come and go; otherwise the earliest viewer whose first
/// choice is another cohort decides it.
pub fn plan(
    viewers: &[Viewer<'_>],
    offers: &[Offer],
    h264: Candidate,
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
            .filter(|c| *c != h264)
    };
    let best = current
        .filter(|b| firsts().any(|c| c == *b))
        .or_else(|| firsts().next());
    let picks = viewers
        .iter()
        .zip(&prefs)
        .map(|(v, p)| {
            let pick = if v.legacy {
                Some(Choice {
                    cand: h264,
                    implementation: None,
                })
            } else {
                p.iter()
                    .find(|c| c.cand == h264 || Some(c.cand) == best)
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
        /// The display's refresh rate the page measured (frames per second).
        #[serde(default)]
        hz: Option<f64>,
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
