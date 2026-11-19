//! AV1 codec strings (`av01.<profile>.<level><tier>.<depth>[...]`, AV1-ISOBMFF section 5)
//! from the sequence header OBU, as the web client's `codecs.ts` reads them.

struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn bit(&mut self) -> Option<u32> {
        let b = *self.d.get(self.pos >> 3)?;
        let v = (b >> (7 - (self.pos & 7))) & 1;
        self.pos += 1;
        Some(v as u32)
    }

    fn f(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u64;
        for _ in 0..n {
            v = v << 1 | self.bit()? as u64;
        }
        u32::try_from(v).ok()
    }

    fn uvlc(&mut self) -> Option<u32> {
        let mut zeros = 0u32;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros >= 32 {
                return Some(u32::MAX);
            }
        }
        Some((1u32 << zeros) - 1 + self.f(zeros)?)
    }
}

fn leb128(d: &[u8], mut i: usize) -> Option<(usize, usize)> {
    let mut v = 0u64;
    for k in 0..8 {
        let b = *d.get(i)?;
        i += 1;
        v |= ((b & 0x7f) as u64) << (7 * k);
        if b & 0x80 == 0 {
            return Some((usize::try_from(v).ok()?, i));
        }
    }
    None
}

/// One OBU of a temporal unit.
pub struct Obu<'a> {
    pub kind: u8,
    /// Its header byte and, with the extension flag, the extension byte.
    pub header: &'a [u8],
    pub payload: &'a [u8],
    /// Header, size field (if any) and payload.
    pub whole: &'a [u8],
}

/// The OBU starting at `i` and where the next one starts; `None` if it runs past the end.
fn obu_at(d: &[u8], i: usize) -> Option<(Obu<'_>, usize)> {
    let h = *d.get(i)?;
    let header_end = i + 1 + ((h >> 2) & 1) as usize;
    let mut j = header_end;
    let mut size = d.len().checked_sub(j)?;
    if h & 2 != 0 {
        (size, j) = leb128(d, j)?;
    }
    let end = j.checked_add(size).filter(|&e| e <= d.len())?;
    let obu = Obu {
        kind: (h >> 3) & 0xf,
        header: &d[i..header_end],
        payload: &d[j..end],
        whole: &d[i..end],
    };
    Some((obu, end))
}

/// Every OBU of a temporal unit, in order; `None` if one runs past the end.
pub fn obus(d: &[u8]) -> Option<Vec<Obu<'_>>> {
    let mut v = Vec::new();
    let mut i = 0;
    while i < d.len() {
        let (o, next) = obu_at(d, i)?;
        v.push(o);
        i = next;
    }
    Some(v)
}

/// The kind and payload of the first OBU in a temporal unit that `want` takes.
fn first_obu(d: &[u8], want: impl Fn(u8) -> bool) -> Option<(u8, &[u8])> {
    first_whole_obu(d, want).map(|o| (o.kind, o.payload))
}

fn first_whole_obu(d: &[u8], want: impl Fn(u8) -> bool) -> Option<Obu<'_>> {
    let mut i = 0;
    while i < d.len() {
        let (o, next) = obu_at(d, i)?;
        if want(o.kind) {
            return Some(o);
        }
        i = next;
    }
    None
}

/// The payload of the first sequence header OBU in a temporal unit.
pub fn sequence_header(d: &[u8]) -> Option<&[u8]> {
    first_obu(d, |k| k == 1).map(|(_, p)| p)
}

/// The first sequence header OBU in a temporal unit, header and size field included.
pub fn sequence_header_obu(d: &[u8]) -> Option<&[u8]> {
    first_whole_obu(d, |k| k == 1).map(|o| o.whole)
}

/// The four fixed bytes of an `av1C` box (AV1-ISOBMFF section 2.3.3) for a sequence
/// header's payload; its configOBUs follow them.
pub fn av1c(sh: &[u8]) -> Option<[u8; 4]> {
    let s = sequence(sh)?;
    Some([
        0x81,
        (s.profile << 5 | s.level) as u8,
        (s.tier << 7
            | ((s.depth > 8) as u32) << 6
            | ((s.depth == 12) as u32) << 5
            | s.mono << 4
            | s.ssx << 3
            | s.ssy << 2
            | s.csp) as u8,
        0,
    ])
}

/// The frame_type (0 key, 1 inter, 2 intra-only, 3 switch) of the first frame in a
/// temporal unit of a stream without a reduced still-picture header; `None` without a
/// frame or for one that only shows an existing frame.
#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
pub fn frame_type(d: &[u8]) -> Option<u8> {
    let (_, p) = first_obu(d, |k| k == 3 || k == 6)?;
    let b = *p.first()?;
    (b & 0x80 == 0).then_some((b >> 5) & 3)
}

pub fn codec_string(temporal_unit: &[u8]) -> Option<String> {
    from_sequence_header(sequence_header(temporal_unit)?)
}

fn from_sequence_header(sh: &[u8]) -> Option<String> {
    let s = sequence(sh)?;
    let base = format!(
        "av01.{}.{:02}{}.{:02}",
        s.profile,
        s.level,
        if s.tier == 1 { 'H' } else { 'M' },
        s.depth
    );
    let rest = format!(
        "{}.{}{}{}.{:02}.{:02}.{:02}.{}",
        s.mono, s.ssx, s.ssy, s.csp, s.cp, s.tc, s.mc, s.range
    );
    Some(if rest == "0.110.01.01.01.0" {
        base
    } else {
        format!("{base}.{rest}")
    })
}

/// What a sequence header says about the stream's format.
struct Sequence {
    profile: u32,
    level: u32,
    tier: u32,
    depth: u32,
    mono: u32,
    ssx: u32,
    ssy: u32,
    csp: u32,
    cp: u32,
    tc: u32,
    mc: u32,
    range: u32,
}

fn sequence(sh: &[u8]) -> Option<Sequence> {
    let b = &mut Bits { d: sh, pos: 0 };
    let profile = b.f(3)?;
    b.f(1)?;
    let reduced = b.f(1)? == 1;
    let (mut level, mut tier) = (0, 0);
    if reduced {
        level = b.f(5)?;
    } else {
        let (mut decoder_model, mut delay_bits) = (false, 0);
        if b.f(1)? == 1 {
            b.f(32)?;
            b.f(32)?;
            if b.f(1)? == 1 {
                b.uvlc()?;
            }
            decoder_model = b.f(1)? == 1;
            if decoder_model {
                delay_bits = b.f(5)? + 1;
                b.f(32)?;
                b.f(5)?;
                b.f(5)?;
            }
        }
        let display_delay = b.f(1)? == 1;
        let points = b.f(5)? + 1;
        for op in 0..points {
            b.f(12)?;
            let l = b.f(5)?;
            let t = if l > 7 { b.f(1)? } else { 0 };
            if op == 0 {
                (level, tier) = (l, t);
            }
            if decoder_model && b.f(1)? == 1 {
                b.f(delay_bits)?;
                b.f(delay_bits)?;
                b.f(1)?;
            }
            if display_delay && b.f(1)? == 1 {
                b.f(4)?;
            }
        }
    }
    let w_bits = b.f(4)? + 1;
    let h_bits = b.f(4)? + 1;
    b.f(w_bits)?;
    b.f(h_bits)?;
    if !reduced && b.f(1)? == 1 {
        b.f(4)?;
        b.f(3)?;
    }
    b.f(3)?;
    if !reduced {
        b.f(4)?;
        let order_hint = b.f(1)? == 1;
        if order_hint {
            b.f(2)?;
        }
        let sct = if b.f(1)? == 1 { 2 } else { b.f(1)? };
        if sct > 0 && b.f(1)? == 0 {
            b.f(1)?;
        }
        if order_hint {
            b.f(3)?;
        }
    }
    b.f(3)?;
    let high = b.f(1)? == 1;
    let depth = match (profile, high) {
        (2, true) if b.f(1)? == 1 => 12,
        (_, true) => 10,
        _ => 8,
    };
    let mono = if profile == 1 { 0 } else { b.f(1)? };
    let (mut cp, mut tc, mut mc) = (2, 2, 2);
    if b.f(1)? == 1 {
        (cp, tc, mc) = (b.f(8)?, b.f(8)?, b.f(8)?);
    }
    let (mut ssx, mut ssy, mut csp) = (1, 1, 0);
    let range;
    if mono == 1 {
        range = b.f(1)?;
    } else if (cp, tc, mc) == (1, 13, 0) {
        range = 1;
        (ssx, ssy) = (0, 0);
    } else {
        range = b.f(1)?;
        if profile == 1 {
            (ssx, ssy) = (0, 0);
        } else if profile == 2 {
            if depth == 12 {
                ssx = b.f(1)?;
                ssy = if ssx == 1 { b.f(1)? } else { 0 };
            } else {
                (ssx, ssy) = (1, 0);
            }
        }
        if ssx == 1 && ssy == 1 {
            csp = b.f(2)?;
        }
    }
    Some(Sequence {
        profile,
        level,
        tier,
        depth,
        mono,
        ssx,
        ssy,
        csp,
        cp,
        tc,
        mc,
        range,
    })
}
