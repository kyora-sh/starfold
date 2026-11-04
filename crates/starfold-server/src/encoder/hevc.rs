//! HEVC Annex-B NAL types and the `hvc1` codec string.

pub const NAL_VPS: u8 = 32;
pub const NAL_SPS: u8 = 33;
pub const NAL_PPS: u8 = 34;

pub fn nal_type(nal: &[u8]) -> u8 {
    (nal[0] >> 1) & 0x3f
}

/// BLA, IDR or CRA: with no frame reordering there are no leading pictures, so each
/// starts a stream a decoder can join.
pub fn is_random_access(t: u8) -> bool {
    (16..=21).contains(&t)
}

pub fn is_slice(t: u8) -> bool {
    t <= 9 || is_random_access(t)
}

/// The SPS without its two header bytes and emulation-prevention bytes.
fn rbsp(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal.iter().skip(2) {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// `hvc1.<profile>.<compatibility>.<tier><level>[.<constraints>]` (ISO/IEC 14496-15
/// annex E) from an SPS NAL unit's general profile_tier_level.
pub fn codec_string(sps: &[u8]) -> Option<String> {
    if sps.len() < 2 || nal_type(sps) != NAL_SPS {
        return None;
    }
    let r = rbsp(sps);
    // sps_video_parameter_set_id .. sps_temporal_id_nesting_flag, then 12 bytes of
    // general profile/tier/level.
    let ptl = r.get(1..13)?;
    let space = ptl[0] >> 6;
    let tier = if ptl[0] & 0x20 != 0 { 'H' } else { 'L' };
    let profile = ptl[0] & 0x1f;
    let compat = u32::from_be_bytes([ptl[1], ptl[2], ptl[3], ptl[4]]).reverse_bits();
    let constraints = &ptl[5..11];
    let level = ptl[11];
    let mut s = format!(
        "hvc1.{}{profile}.{compat:X}.{tier}{level}",
        ["", "A", "B", "C"][space as usize]
    );
    let used = constraints
        .iter()
        .rposition(|&b| b != 0)
        .map_or(0, |i| i + 1);
    for b in &constraints[..used] {
        s.push_str(&format!(".{b:X}"));
    }
    Some(s)
}
