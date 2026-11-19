//! Minimal MP4 writer for recordings: the H.264 starfold already sends, rewrapped without
//! re-encoding. Frames arrive only when the screen changes, so each sample lasts until
//! the next one (variable frame rate).

/// One encoded frame: time since the recording started (ms), keyframe, Annex-B data.
#[cfg(test)]
pub struct Frame {
    pub ms: u64,
    pub key: bool,
    pub data: bytes::Bytes,
}

const TIMESCALE: u32 = 1000;
/// The least a recording's last frame lasts (ms).
pub const LAST_FRAME_MS: u64 = 17;

/// NAL units of an Annex-B stream (start codes removed).
pub fn nals(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = None;
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if let Some(s) = start {
                // A 4-byte start code leaves a zero at the end of the previous unit.
                let mut end = i;
                while end > s && data[end - 1] == 0 {
                    end -= 1;
                }
                out.push(&data[s..end]);
            }
            i += 3;
            start = Some(i);
        } else {
            i += 1;
        }
    }
    if let Some(s) = start {
        out.push(&data[s..]);
    }
    out.into_iter().filter(|n| !n.is_empty()).collect()
}

#[cfg(test)]
fn bx(name: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(body.len() + 8);
    v.extend_from_slice(&(body.len() as u32 + 8).to_be_bytes());
    v.extend_from_slice(name);
    v.extend_from_slice(body);
    v
}

/// A full box: version 0 and `flags`, then the body.
#[cfg(test)]
fn fbx(name: &[u8; 4], flags: u32, body: &[u8]) -> Vec<u8> {
    let mut b = flags.to_be_bytes().to_vec();
    b.extend_from_slice(body);
    bx(name, &b)
}

#[cfg(test)]
fn u32s(v: &[u32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_be_bytes()).collect()
}

/// Identity transformation matrix of movie and track headers.
const MATRIX: [u32; 9] = [0x10000, 0, 0, 0, 0x10000, 0, 0, 0, 0x4000_0000];

/// What the MP4's index keeps of one frame: when it was captured (ms), whether it is a
/// keyframe, and the size of its sample.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    pub ms: u64,
    pub key: bool,
    pub size: u32,
}

/// The NAL units of a frame's MP4 sample: parameter sets and delimiters left out. The
/// sample is each of them after its length (4 bytes, big-endian).
pub fn units(data: &[u8]) -> Vec<&[u8]> {
    nals(data)
        .into_iter()
        .filter(|n| !matches!(n[0] & 0x1f, 7..=9))
        .collect()
}

/// A keyframe's first SPS and PPS.
pub fn parameter_sets(keyframe: &[u8]) -> Result<(Vec<u8>, Vec<u8>), String> {
    let (mut sps, mut pps) = (None, None);
    for n in nals(keyframe) {
        match n[0] & 0x1f {
            7 if sps.is_none() => sps = Some(n.to_vec()),
            8 if pps.is_none() => pps = Some(n.to_vec()),
            _ => {}
        }
    }
    let (sps, pps) = (
        sps.ok_or("keyframe without SPS")?,
        pps.ok_or("keyframe without PPS")?,
    );
    if sps.len() < 4 {
        return Err("bad SPS".into());
    }
    Ok((sps, pps))
}

/// Writes `frames` (the first must be a keyframe) as an MP4 of `width`x`height` lasting
/// until `end_ms`: [`header`], then every frame's sample. Recordings write the two apart
/// (the samples go to disk as they come); this is the whole file.
#[cfg(test)]
pub fn write(frames: &[Frame], width: u32, height: u32, end_ms: u64) -> Result<Vec<u8>, String> {
    let first = frames.first().ok_or("no frames were recorded")?;
    if !first.key {
        return Err("recording doesn't start with a keyframe".into());
    }
    let (sps, pps) = parameter_sets(&first.data)?;
    let mut body = Vec::new();
    let mut index = Vec::with_capacity(frames.len());
    for f in frames {
        let at = body.len();
        for n in units(&f.data) {
            body.extend_from_slice(&(n.len() as u32).to_be_bytes());
            body.extend_from_slice(n);
        }
        index.push(Sample {
            ms: f.ms,
            key: f.key,
            size: (body.len() - at) as u32,
        });
    }
    let mut out = header(&index, &sps, &pps, width, height, end_ms)?;
    out.extend_from_slice(&body);
    Ok(out)
}

/// [`header`] as it was first written, each box built whole and then copied into its
/// parent; tests check `header` against it byte for byte.
#[cfg(test)]
fn reference_header(
    index: &[Sample],
    sps: &[u8],
    pps: &[u8],
    width: u32,
    height: u32,
    end_ms: u64,
) -> Result<Vec<u8>, String> {
    let first = index.first().ok_or("no frames were recorded")?;
    if !first.key {
        return Err("recording doesn't start with a keyframe".into());
    }
    let (mut sizes, mut durations, mut sync) = (
        Vec::with_capacity(index.len()),
        Vec::with_capacity(index.len()),
        Vec::new(),
    );
    for (i, f) in index.iter().enumerate() {
        sizes.push(f.size);
        let next = index
            .get(i + 1)
            .map(|n| n.ms)
            .unwrap_or(end_ms.max(f.ms + LAST_FRAME_MS));
        durations.push(next.saturating_sub(f.ms).max(1) as u32);
        if f.key {
            sync.push(i as u32 + 1);
        }
    }
    let total: u64 = durations.iter().map(|&d| d as u64).sum();
    let n = index.len() as u32;
    let mdat_len: u64 = sizes.iter().map(|&s| s as u64).sum();

    let ftyp = bx(b"ftyp", b"isom\0\0\x02\0isomiso2avc1mp41");
    if mdat_len + 8 > u32::MAX as u64 {
        return Err("recording too large".into());
    }
    // Patched in once moov's size is known (it doesn't depend on this value).
    let chunk_offset = 0;

    let mut avcc = vec![1, sps[1], sps[2], sps[3], 0xff, 0xe1];
    avcc.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    avcc.extend_from_slice(sps);
    avcc.push(1);
    avcc.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    avcc.extend_from_slice(pps);
    if matches!(sps[1], 100 | 110 | 122 | 144) {
        // 4:2:0, 8-bit, no SPS extensions.
        avcc.extend_from_slice(&[0xfd, 0xf8, 0xf8, 0]);
    }
    let mut avc1 = vec![0u8; 6];
    avc1.extend_from_slice(&1u16.to_be_bytes()); // data reference index
    avc1.extend_from_slice(&[0u8; 16]);
    avc1.extend_from_slice(&(width as u16).to_be_bytes());
    avc1.extend_from_slice(&(height as u16).to_be_bytes());
    avc1.extend_from_slice(&u32s(&[0x48_0000, 0x48_0000, 0]));
    avc1.extend_from_slice(&1u16.to_be_bytes()); // frame count
    avc1.extend_from_slice(&[0u8; 32]); // compressor name
    avc1.extend_from_slice(&[0, 0x18, 0xff, 0xff]); // depth, pre-defined
    avc1.extend_from_slice(&bx(b"avcC", &avcc));
    let stsd = fbx(b"stsd", 0, &[u32s(&[1]), bx(b"avc1", &avc1)].concat());

    // Runs of equal durations.
    let mut stts: Vec<(u32, u32)> = Vec::new();
    for &d in &durations {
        match stts.last_mut() {
            Some((c, last)) if *last == d => *c += 1,
            _ => stts.push((1, d)),
        }
    }
    let stts = fbx(
        b"stts",
        0,
        &[
            u32s(&[stts.len() as u32]),
            stts.iter().flat_map(|&(c, d)| u32s(&[c, d])).collect(),
        ]
        .concat(),
    );
    let stss = fbx(
        b"stss",
        0,
        &[u32s(&[sync.len() as u32]), u32s(&sync)].concat(),
    );
    let stsz = fbx(b"stsz", 0, &[u32s(&[0, n]), u32s(&sizes)].concat());
    let stsc = fbx(b"stsc", 0, &u32s(&[1, 1, n, 1]));
    let stco = fbx(b"stco", 0, &u32s(&[1, chunk_offset]));
    let stbl = bx(b"stbl", &[stsd, stts, stss, stsz, stsc, stco].concat());

    let dinf = bx(
        b"dinf",
        &fbx(b"dref", 0, &[u32s(&[1]), fbx(b"url ", 1, &[])].concat()),
    );
    let vmhd = fbx(b"vmhd", 1, &[0u8; 8]);
    let minf = bx(b"minf", &[vmhd, dinf, stbl].concat());
    let mut hdlr = u32s(&[0]);
    hdlr.extend_from_slice(b"vide");
    hdlr.extend_from_slice(&[0u8; 12]);
    hdlr.extend_from_slice(b"starfold\0");
    let hdlr = fbx(b"hdlr", 0, &hdlr);
    let mut mdhd = u32s(&[0, 0, TIMESCALE, total as u32]);
    mdhd.extend_from_slice(&[0x55, 0xc4, 0, 0]); // language "und"
    let mdia = bx(b"mdia", &[fbx(b"mdhd", 0, &mdhd), hdlr, minf].concat());

    let mut tkhd = u32s(&[0, 0, 1, 0, total as u32, 0, 0]);
    tkhd.extend_from_slice(&[0u8; 4]); // layer, alternate group
    tkhd.extend_from_slice(&[0u8; 4]); // volume, reserved
    tkhd.extend_from_slice(&u32s(&MATRIX));
    tkhd.extend_from_slice(&u32s(&[width << 16, height << 16]));
    let trak = bx(b"trak", &[fbx(b"tkhd", 3, &tkhd), mdia].concat());

    let mut mvhd = u32s(&[0, 0, TIMESCALE, total as u32, 0x10000]);
    mvhd.extend_from_slice(&[1, 0, 0, 0]); // volume 1.0, reserved
    mvhd.extend_from_slice(&[0u8; 8]);
    mvhd.extend_from_slice(&u32s(&MATRIX));
    mvhd.extend_from_slice(&[0u8; 24]); // pre-defined
    mvhd.extend_from_slice(&u32s(&[2])); // next track id
    let mut moov = bx(b"moov", &[fbx(b"mvhd", 0, &mvhd), trak].concat());

    // moov goes first, so a player can start before the whole file has arrived. stco is
    // the last box in moov, and its one chunk offset the last 4 bytes: the samples start
    // right after ftyp, moov and mdat's header.
    let offset = (ftyp.len() + moov.len() + 8) as u32;
    let at = moov.len() - 4;
    moov[at..].copy_from_slice(&offset.to_be_bytes());
    let mut out = Vec::with_capacity(offset as usize);
    out.extend_from_slice(&ftyp);
    out.extend_from_slice(&moov);
    out.extend_from_slice(&(mdat_len as u32 + 8).to_be_bytes());
    out.extend_from_slice(b"mdat");
    Ok(out)
}

/// Appends a box's size (patched by [`close`]) and name; where it starts.
fn open(out: &mut Vec<u8>, name: &[u8; 4]) -> usize {
    let at = out.len();
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(name);
    at
}

/// [`open`] for a full box: version 0 and `flags` follow the name.
fn open_full(out: &mut Vec<u8>, name: &[u8; 4], flags: u32) -> usize {
    let at = open(out, name);
    out.extend_from_slice(&flags.to_be_bytes());
    at
}

/// Patches the size of the box opened at `at`, which ends here.
fn close(out: &mut [u8], at: usize) {
    let len = (out.len() - at) as u32;
    out[at..at + 4].copy_from_slice(&len.to_be_bytes());
}

fn put(out: &mut Vec<u8>, v: &[u32]) {
    for x in v {
        out.extend_from_slice(&x.to_be_bytes());
    }
}

/// Everything of the MP4 before the samples (ftyp, moov, mdat's header) for the samples
/// `index` describes (the first must be a keyframe), whose bytes follow it in order. It is
/// written once into a buffer of its exact size: the sample tables, 4 bytes a sample for
/// sizes and at most 4 for keyframes and 8 for durations, exist only there.
pub fn header(
    index: &[Sample],
    sps: &[u8],
    pps: &[u8],
    width: u32,
    height: u32,
    end_ms: u64,
) -> Result<Vec<u8>, String> {
    let first = index.first().ok_or("no frames were recorded")?;
    if !first.key {
        return Err("recording doesn't start with a keyframe".into());
    }
    let duration = |i: usize| {
        let f = &index[i];
        let next = index
            .get(i + 1)
            .map(|n| n.ms)
            .unwrap_or(end_ms.max(f.ms + LAST_FRAME_MS));
        next.saturating_sub(f.ms).max(1) as u32
    };
    let (mut runs, mut keys, mut total, mut mdat_len) = (0usize, 0usize, 0u64, 0u64);
    let mut last = None;
    for (i, f) in index.iter().enumerate() {
        let d = duration(i);
        if last != Some(d) {
            runs += 1;
            last = Some(d);
        }
        total += d as u64;
        keys += f.key as usize;
        mdat_len += f.size as u64;
    }
    if mdat_len + 8 > u32::MAX as u64 {
        return Err("recording too large".into());
    }
    let n = index.len();
    let high = matches!(sps[1], 100 | 110 | 122 | 144);
    // Every box's fixed part (under 700 bytes), the parameter sets and the tables.
    let size = 700 + sps.len() + pps.len() + 8 * runs + 4 * keys + 4 * n;
    let mut out = Vec::with_capacity(size);

    let ftyp = open(&mut out, b"ftyp");
    out.extend_from_slice(b"isom\0\0\x02\0isomiso2avc1mp41");
    close(&mut out, ftyp);
    let moov = open(&mut out, b"moov");
    let mvhd = open_full(&mut out, b"mvhd", 0);
    put(&mut out, &[0, 0, TIMESCALE, total as u32, 0x10000]);
    out.extend_from_slice(&[1, 0, 0, 0]); // volume 1.0, reserved
    out.extend_from_slice(&[0u8; 8]);
    put(&mut out, &MATRIX);
    out.extend_from_slice(&[0u8; 24]); // pre-defined
    put(&mut out, &[2]); // next track id
    close(&mut out, mvhd);
    let trak = open(&mut out, b"trak");
    let tkhd = open_full(&mut out, b"tkhd", 3);
    put(&mut out, &[0, 0, 1, 0, total as u32, 0, 0]);
    out.extend_from_slice(&[0u8; 4]); // layer, alternate group
    out.extend_from_slice(&[0u8; 4]); // volume, reserved
    put(&mut out, &MATRIX);
    put(&mut out, &[width << 16, height << 16]);
    close(&mut out, tkhd);
    let mdia = open(&mut out, b"mdia");
    let mdhd = open_full(&mut out, b"mdhd", 0);
    put(&mut out, &[0, 0, TIMESCALE, total as u32]);
    out.extend_from_slice(&[0x55, 0xc4, 0, 0]); // language "und"
    close(&mut out, mdhd);
    let hdlr = open_full(&mut out, b"hdlr", 0);
    put(&mut out, &[0]);
    out.extend_from_slice(b"vide");
    out.extend_from_slice(&[0u8; 12]);
    out.extend_from_slice(b"starfold\0");
    close(&mut out, hdlr);
    let minf = open(&mut out, b"minf");
    let vmhd = open_full(&mut out, b"vmhd", 1);
    out.extend_from_slice(&[0u8; 8]);
    close(&mut out, vmhd);
    let dinf = open(&mut out, b"dinf");
    let dref = open_full(&mut out, b"dref", 0);
    put(&mut out, &[1]);
    let url = open_full(&mut out, b"url ", 1);
    close(&mut out, url);
    close(&mut out, dref);
    close(&mut out, dinf);
    let stbl = open(&mut out, b"stbl");
    let stsd = open_full(&mut out, b"stsd", 0);
    put(&mut out, &[1]);
    let avc1 = open(&mut out, b"avc1");
    out.extend_from_slice(&[0u8; 6]);
    out.extend_from_slice(&1u16.to_be_bytes()); // data reference index
    out.extend_from_slice(&[0u8; 16]);
    out.extend_from_slice(&(width as u16).to_be_bytes());
    out.extend_from_slice(&(height as u16).to_be_bytes());
    put(&mut out, &[0x48_0000, 0x48_0000, 0]);
    out.extend_from_slice(&1u16.to_be_bytes()); // frame count
    out.extend_from_slice(&[0u8; 32]); // compressor name
    out.extend_from_slice(&[0, 0x18, 0xff, 0xff]); // depth, pre-defined
    let avcc = open(&mut out, b"avcC");
    out.extend_from_slice(&[1, sps[1], sps[2], sps[3], 0xff, 0xe1]);
    out.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    out.extend_from_slice(sps);
    out.push(1);
    out.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    out.extend_from_slice(pps);
    if high {
        // 4:2:0, 8-bit, no SPS extensions.
        out.extend_from_slice(&[0xfd, 0xf8, 0xf8, 0]);
    }
    close(&mut out, avcc);
    close(&mut out, avc1);
    close(&mut out, stsd);
    // Runs of equal durations.
    let stts = open_full(&mut out, b"stts", 0);
    put(&mut out, &[runs as u32]);
    let mut run: Option<(u32, u32)> = None;
    for i in 0..n {
        let d = duration(i);
        run = match run {
            Some((c, last)) if last == d => Some((c + 1, d)),
            Some((c, last)) => {
                put(&mut out, &[c, last]);
                Some((1, d))
            }
            None => Some((1, d)),
        };
    }
    if let Some((c, last)) = run {
        put(&mut out, &[c, last]);
    }
    close(&mut out, stts);
    let stss = open_full(&mut out, b"stss", 0);
    put(&mut out, &[keys as u32]);
    for (i, f) in index.iter().enumerate() {
        if f.key {
            put(&mut out, &[i as u32 + 1]);
        }
    }
    close(&mut out, stss);
    let stsz = open_full(&mut out, b"stsz", 0);
    put(&mut out, &[0, n as u32]);
    for f in index {
        put(&mut out, &[f.size]);
    }
    close(&mut out, stsz);
    let stsc = open_full(&mut out, b"stsc", 0);
    put(&mut out, &[1, 1, n as u32, 1]);
    close(&mut out, stsc);
    let stco = open_full(&mut out, b"stco", 0);
    put(&mut out, &[1]);
    // moov goes first, so a player can start before the whole file has arrived: the one
    // chunk's offset is where the samples start, right after moov and mdat's header.
    let offset = out.len();
    put(&mut out, &[0]);
    close(&mut out, stco);
    close(&mut out, stbl);
    close(&mut out, minf);
    close(&mut out, mdia);
    close(&mut out, trak);
    close(&mut out, moov);
    let samples = (out.len() + 8) as u32;
    out[offset..offset + 4].copy_from_slice(&samples.to_be_bytes());
    put(&mut out, &[mdat_len as u32 + 8]);
    out.extend_from_slice(b"mdat");
    debug_assert!(out.len() <= size, "{} > {size}", out.len());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_header_is_the_reference_builders_byte_for_byte() {
        let sps = [0x67, 100, 0, 40, 0xac];
        let base = [0x67, 66, 0, 30, 0xac];
        let pps = [0x68, 0xee];
        let shapes: [&dyn Fn(u64) -> Sample; 4] = [
            // Keyframes now and then, durations in runs.
            &|i| Sample {
                ms: i * 33,
                key: i % 30 == 0,
                size: 100 + (i % 7) as u32,
            },
            // Every frame a keyframe, no two durations alike in a row.
            &|i| Sample {
                ms: 3 * i + i % 2,
                key: true,
                size: 37,
            },
            // A still screen: a few frames far apart.
            &|i| Sample {
                ms: i * 5000,
                key: i == 0,
                size: 1 << 16,
            },
            // Frames on the same millisecond.
            &|i| Sample {
                ms: i / 3,
                key: i == 0,
                size: 1,
            },
        ];
        for (k, shape) in shapes.iter().enumerate() {
            for n in [1u64, 2, 3, 100, 2001] {
                let index: Vec<Sample> = (0..n).map(shape).collect();
                for (sps, end) in [(&sps, 0), (&base, n * 40 + 500)] {
                    let want = reference_header(&index, sps, &pps, 640, 480, end).unwrap();
                    let got = header(&index, sps, &pps, 640, 480, end).unwrap();
                    assert!(got == want, "shape {k}, {n} samples, end {end}");
                }
            }
        }
        let late = [Sample {
            ms: 0,
            key: false,
            size: 1,
        }];
        assert_eq!(
            header(&late, &sps, &pps, 1, 1, 0),
            reference_header(&late, &sps, &pps, 1, 1, 0)
        );
        assert_eq!(
            header(&[], &sps, &pps, 1, 1, 0),
            reference_header(&[], &sps, &pps, 1, 1, 0)
        );
    }

    #[test]
    fn splits_annex_b() {
        let d = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4, 5,
        ];
        let n = nals(&d);
        assert_eq!(
            n,
            vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 4, 5][..]]
        );
    }

    #[test]
    fn moov_first_and_chunk_offset_points_at_the_samples() {
        let key = bytes::Bytes::from_static(&[
            0, 0, 0, 1, 0x67, 100, 0, 40, 0xac, 0, 0, 0, 1, 0x68, 0xee, 0, 0, 0, 1, 0x65, 0xaa,
        ]);
        let delta = bytes::Bytes::from_static(&[0, 0, 0, 1, 0x41, 0xbb, 0xcc]);
        let frames = [
            Frame {
                ms: 0,
                key: true,
                data: key,
            },
            Frame {
                ms: 40,
                key: false,
                data: delta,
            },
        ];
        let out = write(&frames, 64, 32, 100).unwrap();
        let pos = |t: &[u8]| out.windows(4).position(|w| w == t).unwrap();
        assert!(pos(b"moov") < pos(b"mdat"));
        let stco = pos(b"stco");
        let off = u32::from_be_bytes(out[stco + 12..stco + 16].try_into().unwrap()) as usize;
        assert_eq!(off, pos(b"mdat") + 4);
        // Both samples, length-prefixed, parameter sets left out.
        assert_eq!(
            &out[off..],
            &[0, 0, 0, 2, 0x65, 0xaa, 0, 0, 0, 3, 0x41, 0xbb, 0xcc]
        );
    }
}
