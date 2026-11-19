//! Minimal MP4 writer for recordings: the H.264 or AV1 starfold already sends, rewrapped
//! without re-encoding. Frames arrive only when the screen changes, so each sample lasts
//! until the next one (variable frame rate).

use crate::encoder::{av1, Codec};

/// One encoded frame: time since the recording started (ms), keyframe, Annex-B data or an
/// AV1 temporal unit.
#[cfg(test)]
pub struct Frame {
    pub ms: u64,
    pub key: bool,
    pub data: bytes::Bytes,
}

/// What a track's sample entry is made from, taken from its first keyframe.
#[derive(Clone, Debug, PartialEq)]
pub enum Track {
    /// `avc1`: the keyframe's first SPS and PPS.
    Avc { sps: Vec<u8>, pps: Vec<u8> },
    /// `av01`: `av1C`'s four fixed bytes and the sequence header OBU as its configOBUs.
    Av1 { config: [u8; 4], sequence: Vec<u8> },
}

impl Track {
    pub fn of(codec: Codec, keyframe: &[u8]) -> Result<Track, String> {
        match codec {
            Codec::H264 => parameter_sets(keyframe).map(|(sps, pps)| Track::Avc { sps, pps }),
            Codec::Av1 => {
                let sequence = av1::sequence_header_obu(keyframe)
                    .ok_or("keyframe without a sequence header")?;
                if sequence[0] & 2 == 0 {
                    return Err("a sequence header OBU without a size field".into());
                }
                let config = av1::sequence_header(sequence)
                    .and_then(av1::av1c)
                    .ok_or("bad sequence header")?;
                Ok(Track::Av1 {
                    config,
                    sequence: sequence.to_vec(),
                })
            }
            c => Err(format!("a recording holds H.264 or AV1, not {c:?}")),
        }
    }
}

/// One piece of a frame's MP4 sample: its prefix (an H.264 unit's length; an OBU's header
/// and size field), then its body.
pub struct Unit<'a> {
    prefix: [u8; 10],
    prefix_len: u8,
    pub body: &'a [u8],
}

impl Unit<'_> {
    pub fn prefix(&self) -> &[u8] {
        &self.prefix[..self.prefix_len as usize]
    }

    pub fn size(&self) -> u64 {
        self.prefix_len as u64 + self.body.len() as u64
    }
}

fn h264_unit(n: &[u8]) -> Unit<'_> {
    let mut prefix = [0u8; 10];
    prefix[..4].copy_from_slice(&(n.len() as u32).to_be_bytes());
    Unit {
        prefix,
        prefix_len: 4,
        body: n,
    }
}

fn av1_unit(o: av1::Obu<'_>) -> Unit<'_> {
    let mut prefix = [0u8; 10];
    let mut len = o.header.len();
    prefix[..len].copy_from_slice(o.header);
    prefix[0] |= 2;
    let mut size = o.payload.len() as u64;
    loop {
        let b = (size & 0x7f) as u8;
        size >>= 7;
        prefix[len] = if size == 0 { b } else { b | 0x80 };
        len += 1;
        if size == 0 {
            break;
        }
    }
    Unit {
        prefix,
        prefix_len: len as u8,
        body: o.payload,
    }
}

/// A temporal unit's OBUs but its temporal delimiters, tile lists and padding.
fn kept_obus(data: &[u8]) -> impl Iterator<Item = Result<av1::Obu<'_>, &'static str>> {
    av1::obus(data).filter(|o| !matches!(o, Ok(o) if matches!(o.kind, 2 | 8 | 15)))
}

/// A frame's MP4 sample, read from the frame in place. H.264: its NAL units without
/// parameter sets and delimiters, each after its length (4 bytes, big-endian). AV1
/// (AV1-ISOBMFF section 2.4): its OBUs without temporal delimiters, tile lists and padding,
/// each with a size field.
pub struct SampleUnits<'a> {
    codec: Codec,
    data: &'a [u8],
    /// The sample's bytes.
    pub size: u64,
}

impl<'a> SampleUnits<'a> {
    /// The sample's pieces, in order.
    pub fn iter(&self) -> impl Iterator<Item = Unit<'a>> + 'a {
        let (codec, data) = (self.codec, self.data);
        let h264 = (codec == Codec::H264).then(|| units(data).map(h264_unit));
        let obus =
            (codec == Codec::Av1).then(|| kept_obus(data).map_while(Result::ok).map(av1_unit));
        h264.into_iter().flatten().chain(obus.into_iter().flatten())
    }
}

/// Reads a frame's sample without copying it or listing its pieces: an error for an AV1
/// frame with a malformed OBU or none to keep.
pub fn sample_units(codec: Codec, data: &[u8]) -> Result<SampleUnits<'_>, String> {
    let size = match codec {
        Codec::H264 => units(data).map(|n| 4 + n.len() as u64).sum(),
        Codec::Av1 => {
            let (mut size, mut any) = (0, false);
            for o in kept_obus(data) {
                size += av1_unit(o?).size();
                any = true;
            }
            if !any {
                return Err("an AV1 frame with no OBU to record".into());
            }
            size
        }
        c => return Err(format!("a recording holds H.264 or AV1, not {c:?}")),
    };
    Ok(SampleUnits { codec, data, size })
}

const TIMESCALE: u32 = 1000;
/// The least a recording's last frame lasts (ms).
pub const LAST_FRAME_MS: u64 = 17;

/// NAL units of an Annex-B stream (start codes removed), read in place.
pub fn nals(data: &[u8]) -> Nals<'_> {
    Nals {
        data,
        start: start_code(data, 0).map(|i| i + 3),
    }
}

/// Where the first start code at or after `from` is.
fn start_code(data: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            return Some(i);
        }
        i += 1;
    }
    None
}

pub struct Nals<'a> {
    data: &'a [u8],
    /// Where the next unit starts, after its start code.
    start: Option<usize>,
}

impl<'a> Iterator for Nals<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        let data = self.data;
        loop {
            let s = self.start?;
            let n = match start_code(data, s) {
                Some(i) => {
                    self.start = Some(i + 3);
                    // A 4-byte start code leaves a zero at the end of the previous unit.
                    let mut end = i;
                    while end > s && data[end - 1] == 0 {
                        end -= 1;
                    }
                    &data[s..end]
                }
                None => {
                    self.start = None;
                    &data[s..]
                }
            };
            if !n.is_empty() {
                return Some(n);
            }
        }
    }
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
pub fn units(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    nals(data).filter(|n| !matches!(n[0] & 0x1f, 7..=9))
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
pub fn write(
    codec: Codec,
    frames: &[Frame],
    width: u32,
    height: u32,
    end_ms: u64,
) -> Result<Vec<u8>, String> {
    let first = frames.first().ok_or("no frames were recorded")?;
    if !first.key {
        return Err("recording doesn't start with a keyframe".into());
    }
    let track = Track::of(codec, &first.data)?;
    let mut body = Vec::new();
    let mut index = Vec::with_capacity(frames.len());
    for f in frames {
        let at = body.len();
        for u in sample_units(codec, &f.data)?.iter() {
            body.extend_from_slice(u.prefix());
            body.extend_from_slice(u.body);
        }
        index.push(Sample {
            ms: f.ms,
            key: f.key,
            size: (body.len() - at) as u32,
        });
    }
    let mut out = header(&index, &track, width, height, end_ms)?;
    out.extend_from_slice(&body);
    Ok(out)
}

/// Checks that `ffprobe` and `ffmpeg` read `mp4` as one `codec` video stream of `size`,
/// `frames` frames and `duration_ms`, a keyframe first, every frame decoded without an
/// error. Skipped (false) where they aren't installed; the VM gate installs them.
#[cfg(test)]
pub(crate) fn played_by_ffmpeg(
    mp4: &[u8],
    codec: &str,
    size: (u32, u32),
    frames: usize,
    duration_ms: u64,
) -> bool {
    let found = |cmd: &str| std::process::Command::new(cmd).arg("-version").output();
    if found("ffprobe").is_err() || found("ffmpeg").is_err() {
        eprintln!("ffprobe/ffmpeg not installed: skipped");
        return false;
    }
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("starfold-{}-{n}.mp4", std::process::id()));
    std::fs::write(&path, mp4).unwrap();
    struct Remove<'a>(&'a std::path::Path);
    impl Drop for Remove<'_> {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(self.0);
        }
    }
    let _remove = Remove(&path);
    let p = path.to_str().unwrap();
    // `args` split at spaces, the file in place of `IN`; its standard output.
    let run = |cmd: &str, args: &str| {
        let o = std::process::Command::new(cmd)
            .args(args.split(' ').map(|a| if a == "IN" { p } else { a }))
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).into_owned()
    };
    let text = run(
        "ffprobe",
        "-v error -select_streams v:0 -count_frames -show_entries \
         stream=codec_name,width,height,nb_read_frames,duration -of default=nw=1 IN",
    );
    eprintln!("ffprobe: {}", text.replace('\n', " "));
    let has = |s: &str| text.lines().any(|l| l == s);
    assert!(has(&format!("codec_name={codec}")), "{text}");
    assert!(has(&format!("width={}", size.0)), "{text}");
    assert!(has(&format!("height={}", size.1)), "{text}");
    assert!(has(&format!("nb_read_frames={frames}")), "{text}");
    let duration: f64 = text
        .lines()
        .find_map(|l| l.strip_prefix("duration="))
        .and_then(|d| d.parse().ok())
        .unwrap_or(0.0);
    assert!(
        (duration - duration_ms as f64 / 1000.0).abs() < 0.002,
        "{text}"
    );
    let flags = run(
        "ffprobe",
        "-v error -select_streams v:0 -show_entries packet=flags -of csv=p=0 IN",
    );
    assert!(flags.starts_with('K'), "a keyframe first: {flags}");
    run("ffmpeg", "-v error -xerror -i IN -f null -");
    true
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
    track: &Track,
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
    let (entry, params) = match track {
        Track::Avc { sps, pps } => (b"avc1", sps.len() + pps.len()),
        Track::Av1 { sequence, .. } => (b"av01", sequence.len()),
    };
    // Every box's fixed part (under 700 bytes), the parameter sets and the tables.
    let size = 700 + params + 8 * runs + 4 * keys + 4 * n;
    let mut out = Vec::with_capacity(size);

    let ftyp = open(&mut out, b"ftyp");
    out.extend_from_slice(b"isom\0\0\x02\0isomiso2");
    out.extend_from_slice(entry);
    out.extend_from_slice(b"mp41");
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
    let sample_entry = open(&mut out, entry);
    out.extend_from_slice(&[0u8; 6]);
    out.extend_from_slice(&1u16.to_be_bytes()); // data reference index
    out.extend_from_slice(&[0u8; 16]);
    out.extend_from_slice(&(width as u16).to_be_bytes());
    out.extend_from_slice(&(height as u16).to_be_bytes());
    put(&mut out, &[0x48_0000, 0x48_0000, 0]);
    out.extend_from_slice(&1u16.to_be_bytes()); // frame count
    out.extend_from_slice(&[0u8; 32]); // compressor name
    out.extend_from_slice(&[0, 0x18, 0xff, 0xff]); // depth, pre-defined
    match track {
        Track::Avc { sps, pps } => {
            let avcc = open(&mut out, b"avcC");
            out.extend_from_slice(&[1, sps[1], sps[2], sps[3], 0xff, 0xe1]);
            out.extend_from_slice(&(sps.len() as u16).to_be_bytes());
            out.extend_from_slice(sps);
            out.push(1);
            out.extend_from_slice(&(pps.len() as u16).to_be_bytes());
            out.extend_from_slice(pps);
            if matches!(sps[1], 100 | 110 | 122 | 144) {
                // 4:2:0, 8-bit, no SPS extensions.
                out.extend_from_slice(&[0xfd, 0xf8, 0xf8, 0]);
            }
            close(&mut out, avcc);
        }
        Track::Av1 { config, sequence } => {
            let av1c = open(&mut out, b"av1C");
            out.extend_from_slice(config);
            out.extend_from_slice(sequence);
            close(&mut out, av1c);
        }
    }
    close(&mut out, sample_entry);
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
                    let got = header(&index, &avc(sps, &pps), 640, 480, end).unwrap();
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
            header(&late, &avc(&sps, &pps), 1, 1, 0),
            reference_header(&late, &sps, &pps, 1, 1, 0)
        );
        assert_eq!(
            header(&[], &avc(&sps, &pps), 1, 1, 0),
            reference_header(&[], &sps, &pps, 1, 1, 0)
        );
    }

    fn avc(sps: &[u8], pps: &[u8]) -> Track {
        Track::Avc {
            sps: sps.to_vec(),
            pps: pps.to_vec(),
        }
    }

    /// The committed AV1 fixture's temporal units (an IVF file).
    fn fixture_av1() -> Vec<Vec<u8>> {
        let ivf = include_bytes!("../../../bench/tests/fixtures/av1-640x360.ivf");
        let mut v = Vec::new();
        let mut i = u16::from_le_bytes([ivf[6], ivf[7]]) as usize;
        while i + 12 <= ivf.len() {
            let size = u32::from_le_bytes(ivf[i..i + 4].try_into().unwrap()) as usize;
            v.push(ivf[i + 12..i + 12 + size].to_vec());
            i += 12 + size;
        }
        v
    }

    /// Where box `name` starts (its size field) in `out`.
    fn find(out: &[u8], name: &[u8; 4]) -> usize {
        out.windows(4).position(|w| w == name).unwrap() - 4
    }

    #[test]
    fn an_av1_track_is_av01_with_the_sequence_header_in_av1c() {
        let tus = fixture_av1();
        assert!(tus.len() > 1, "{} temporal units", tus.len());
        let frames: Vec<Frame> = tus
            .iter()
            .enumerate()
            .map(|(i, tu)| Frame {
                ms: 33 * i as u64,
                key: i == 0,
                data: bytes::Bytes::copy_from_slice(tu),
            })
            .collect();
        let out = write(Codec::Av1, &frames, 640, 360, 0).unwrap();
        assert_eq!(&out[8..12], b"isom");
        assert_eq!(&out[16..32], b"isomiso2av01mp41");
        assert!(!out.windows(4).any(|w| w == b"avc1"));
        let seq = av1::sequence_header_obu(&tus[0]).unwrap();
        let at = find(&out, b"av1C");
        let len = u32::from_be_bytes(out[at..at + 4].try_into().unwrap()) as usize;
        assert_eq!(len, 8 + 4 + seq.len());
        assert_eq!(&out[at + 8..at + 12], &[0x81, 1, 0x0c, 0], "av01.0.01M.08");
        assert_eq!(&out[at + 12..at + len], seq);
        let stsd = find(&out, b"stsd");
        assert_eq!(&out[stsd + 20..stsd + 24], b"av01", "the sample entry");
        // The samples: every OBU but the temporal delimiters, each with its size field.
        let stco = find(&out, b"stco");
        let off = u32::from_be_bytes(out[stco + 16..stco + 20].try_into().unwrap()) as usize;
        let mut want = Vec::new();
        for tu in &tus {
            for o in av1::obus(tu).map(Result::unwrap) {
                if o.kind != 2 {
                    assert_ne!(o.header[0] & 2, 0, "the fixture's OBUs carry sizes");
                    want.extend_from_slice(o.whole);
                }
            }
        }
        assert_eq!(&out[off..], &want[..]);
    }

    #[test]
    fn an_obu_without_a_size_field_gets_one_and_padding_is_left_out() {
        // A temporal delimiter, padding, then a frame OBU (kind 6) with no size field.
        let tu = [0x12, 0x00, 0x7a, 0x01, 0xff, 0x30, 0xaa, 0xbb, 0xcc];
        let sample = sample_units(Codec::Av1, &tu).unwrap();
        let units: Vec<_> = sample.iter().collect();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].prefix(), &[0x32, 3]);
        assert_eq!(units[0].body, &[0xaa, 0xbb, 0xcc]);
        assert_eq!(units[0].size(), 5);
        assert_eq!(sample.size, 5);
        let long = [&[0x32u8, 0x80, 0x01][..], &[7u8; 128][..]].concat();
        let sample = sample_units(Codec::Av1, &long).unwrap();
        assert_eq!(sample.iter().next().unwrap().prefix(), &[0x32, 0x80, 0x01]);
        assert_eq!(sample.size, 131);
        assert!(sample_units(Codec::Av1, &[0x32, 0x05, 0x00]).is_err());
    }

    /// A sample's bytes, as the recorder writes them.
    fn sample_bytes(codec: Codec, data: &[u8]) -> Vec<u8> {
        let sample = sample_units(codec, data).unwrap();
        let mut out = Vec::new();
        for u in sample.iter() {
            out.extend_from_slice(u.prefix());
            out.extend_from_slice(u.body);
        }
        assert_eq!(out.len() as u64, sample.size);
        out
    }

    #[test]
    fn an_obu_with_an_extension_keeps_it_before_its_new_size_field() {
        assert_eq!(
            sample_bytes(Codec::Av1, &[0x34, 0x48, 0xaa, 0xbb]),
            [0x36, 0x48, 0x02, 0xaa, 0xbb]
        );
        assert!(
            sample_units(Codec::Av1, &[0x34]).is_err(),
            "no extension byte"
        );
        assert!(sample_units(Codec::Av1, &[0x12, 0x00, 0x36]).is_err());
    }

    #[test]
    fn tile_lists_are_left_out_and_a_frame_with_nothing_else_is_an_error() {
        let tu = [0x12, 0x00, 0x42, 0x01, 0x00, 0x32, 0x01, 0xaa, 0x7a, 0x00];
        assert_eq!(sample_bytes(Codec::Av1, &tu), [0x32, 0x01, 0xaa]);
        for tu in [
            &[0x12, 0x00][..],
            &[0x12, 0x00, 0x7a, 0x01, 0xff, 0x42, 0x00][..],
            &[][..],
        ] {
            assert!(sample_units(Codec::Av1, tu).is_err(), "{tu:?}");
        }
    }

    #[test]
    fn an_obu_with_the_forbidden_bit_is_an_error() {
        assert!(sample_units(Codec::Av1, &[0xb2, 0x01, 0x00]).is_err());
        assert!(sample_units(Codec::Av1, &[0x32, 0x01, 0x00, 0xb2, 0x01, 0x00]).is_err());
        assert!(av1::obus(&[0x92, 0x00]).next().unwrap().is_err());
    }

    #[test]
    fn an_av1_keyframe_without_a_sequence_header_opens_no_track() {
        assert_eq!(
            Track::of(Codec::Av1, &[0x12, 0x00, 0x32, 0x01, 0x10]),
            Err("keyframe without a sequence header".into())
        );
        assert!(Track::of(Codec::Hevc, &[0, 0, 0, 1, 0x40]).is_err());
    }

    #[test]
    fn a_sequence_header_without_a_size_field_or_too_long_opens_no_track() {
        let tus = fixture_av1();
        let payload = av1::sequence_header(&tus[0]).unwrap();
        assert!(Track::of(Codec::Av1, &tus[0]).is_ok());
        let sizeless = [&[0x12, 0x00, 0x08][..], payload].concat();
        assert_eq!(
            Track::of(Codec::Av1, &sizeless),
            Err("a sequence header OBU without a size field".into())
        );
        let mut padded = payload.to_vec();
        padded.resize(av1::MAX_SEQUENCE_HEADER, 0);
        assert!(av1::av1c(&padded).is_some());
        padded.push(0);
        assert_eq!(av1::av1c(&padded), None);
        let n = padded.len();
        let long = [
            &[0x12, 0x00, 0x0a, (n & 0x7f) as u8 | 0x80, (n >> 7) as u8][..],
            &padded,
        ]
        .concat();
        assert_eq!(av1::sequence_header(&long), Some(&padded[..]));
        assert_eq!(
            Track::of(Codec::Av1, &long),
            Err("bad sequence header".into())
        );
    }

    #[test]
    fn h264_samples_are_their_units_after_their_lengths() {
        let d = [
            0, 0, 0, 1, 0x09, 0xf0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 4, 5, 0,
            0, 1, 0x65, 6,
        ];
        assert_eq!(
            sample_bytes(Codec::H264, &d),
            [0, 0, 0, 3, 0x65, 4, 5, 0, 0, 0, 2, 0x65, 6]
        );
        assert_eq!(sample_bytes(Codec::H264, &[1, 2, 3]), [] as [u8; 0]);
    }

    #[test]
    fn ffmpeg_decodes_an_av1_mp4() {
        let tus = fixture_av1();
        let frames: Vec<Frame> = tus
            .iter()
            .enumerate()
            .map(|(i, tu)| Frame {
                ms: 40 * i as u64,
                key: i == 0,
                data: bytes::Bytes::copy_from_slice(tu),
            })
            .collect();
        let end = 40 * tus.len() as u64;
        let out = write(Codec::Av1, &frames, 640, 360, end).unwrap();
        played_by_ffmpeg(&out, "av1", (640, 360), tus.len(), end);
    }

    #[test]
    fn splits_annex_b() {
        let d = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4, 5,
        ];
        let n: Vec<_> = nals(&d).collect();
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
        let out = write(Codec::H264, &frames, 64, 32, 100).unwrap();
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
