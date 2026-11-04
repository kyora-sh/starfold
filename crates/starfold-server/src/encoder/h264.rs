//! Annex-B access units: NAL walking, parameter-set bookkeeping and the codec string.

use super::{AccessUnit, BitstreamFormat, Codec, ColorInfo, StreamConfig};

pub const NAL_IDR: u8 = 5;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;

/// The NAL units (header byte first, start code stripped) of an Annex-B buffer.
pub fn nal_units(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let ends: Vec<usize> = starts
        .iter()
        .skip(1)
        .map(|&s| {
            let mut e = s - 3;
            while e > 0 && data[e - 1] == 0 {
                e -= 1;
            }
            e
        })
        .chain(std::iter::once(data.len()))
        .collect();
    starts
        .into_iter()
        .zip(ends)
        .filter(|(s, e)| s < e)
        .map(move |(s, e)| &data[s..e])
}

pub fn nal_type(nal: &[u8]) -> u8 {
    nal[0] & 0x1f
}

/// `avc1.PPCCLL` from an SPS NAL unit.
pub fn codec_string(sps: &[u8]) -> Option<String> {
    (sps.len() >= 4 && nal_type(sps) == NAL_SPS)
        .then(|| format!("avc1.{:02x}{:02x}{:02x}", sps[1], sps[2], sps[3]))
}

/// Keeps every IDR self-contained: one missing its SPS or PPS gets the last ones seen.
#[derive(Default)]
pub struct ParamSets {
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

impl ParamSets {
    /// Turns one encoder output into an access unit; `config` is set when the SPS is
    /// new. `None` for output with no picture in it.
    pub fn unit(
        &mut self,
        mut data: Vec<u8>,
        pts_ms: i64,
        coded: (u32, u32),
        color: ColorInfo,
    ) -> Option<AccessUnit> {
        let (mut sps, mut pps, mut idr, mut slice) = (None, None, false, false);
        for n in nal_units(&data) {
            match nal_type(n) {
                NAL_SPS => sps = Some(n.to_vec()),
                NAL_PPS => pps = Some(n.to_vec()),
                NAL_IDR => (idr, slice) = (true, true),
                1..=4 => slice = true,
                _ => {}
            }
        }
        if !slice {
            return None;
        }
        let changed = sps.is_some() && sps != self.sps;
        if let Some(s) = sps.clone() {
            self.sps = Some(s);
        }
        if let Some(p) = pps.clone() {
            self.pps = Some(p);
        }
        if idr && (sps.is_none() || pps.is_none()) {
            let (Some(s), Some(p)) = (&self.sps, &self.pps) else {
                log::error!("H.264 IDR without parameter sets; dropped");
                return None;
            };
            let mut head = Vec::with_capacity(s.len() + p.len() + 8 + data.len());
            for ps in [(sps.is_none(), s), (pps.is_none(), p)] {
                if ps.0 {
                    head.extend_from_slice(&[0, 0, 0, 1]);
                    head.extend_from_slice(ps.1);
                }
            }
            head.extend_from_slice(&data);
            data = head;
        }
        let config = changed
            .then(|| self.sps.as_deref().and_then(codec_string))
            .flatten()
            .map(|codec_string| StreamConfig {
                codec: Codec::H264,
                codec_string,
                coded,
                display: coded,
                format: BitstreamFormat::AnnexB,
                color,
            });
        Some(AccessUnit {
            pts_ms,
            keyframe: idr,
            data,
            config,
        })
    }
}
