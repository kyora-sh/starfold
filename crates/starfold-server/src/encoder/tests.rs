use super::h264::{nal_type, nal_units, ParamSets, NAL_IDR, NAL_PPS, NAL_SPS};
use super::*;

#[cfg(feature = "libaom")]
mod aom;
#[cfg(feature = "sw-arm")]
mod arms;
mod av1;

/// The software encoders built here, which every contract test below runs on.
fn backends() -> Vec<Backend> {
    vec![
        #[cfg(feature = "x264")]
        Backend::X264,
        #[cfg(feature = "libaom")]
        Backend::Libaom,
    ]
}

/// The codec `b` streams in these tests: AV1 from libaom, H.264 from the rest.
fn codec_of(b: Backend) -> Codec {
    #[cfg(feature = "libaom")]
    if b == Backend::Libaom {
        return Codec::Av1;
    }
    let _ = b;
    Codec::H264
}

fn config(backend: Backend, width: u32, height: u32, max_kbps: u32) -> EncoderConfig {
    let codec = codec_of(backend);
    EncoderConfig {
        backend,
        codec,
        width,
        height,
        fps: 30,
        threads: 1,
        rate: RateConfig::new(24.0, max_kbps, 30),
        best_quality: 16.0,
        preset: match codec {
            Codec::Av1 => String::new(),
            _ => "ultrafast".into(),
        },
    }
}

#[derive(Clone)]
struct Picture {
    w: usize,
    h: usize,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl Picture {
    fn new(w: usize, h: usize) -> Self {
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let mut p = Picture {
            w,
            h,
            y: vec![0; w * h],
            u: vec![128; cw * ch],
            v: vec![128; cw * ch],
        };
        p.texture(0, 0, 0, w, h, 0);
        p
    }

    /// Text-like detail in [x0, x1) x [y0, y1), shifted by `shift` rows.
    fn texture(&mut self, seed: u32, x0: usize, y0: usize, x1: usize, y1: usize, shift: usize) {
        for r in y0..y1 {
            for c in x0..x1 {
                let (a, b) = ((c as u32), (r + shift) as u32);
                let glyph = ((a / 7) ^ (b / 11)).wrapping_mul(2654435761) ^ seed;
                let ink = (a % 7 < 4 && b % 11 < 8) && glyph >> 29 != 0;
                self.y[r * self.w + c] = if ink { 30 } else { 220 };
            }
        }
    }

    /// Full-picture motion: a noisy gradient drifting diagonally.
    fn video(&mut self, f: usize) {
        let mut s = 0x9e3779b9u32 ^ f as u32;
        for r in 0..self.h {
            for c in 0..self.w {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                let g = ((c + 3 * f) as u32 ^ (r + 2 * f) as u32) & 0xff;
                self.y[r * self.w + c] = ((g * 3 + (s & 0x1f)) / 4) as u8;
            }
        }
        let cw = self.w.div_ceil(2);
        for (i, (u, v)) in self.u.iter_mut().zip(self.v.iter_mut()).enumerate() {
            *u = (96 + (i % cw + f) % 64) as u8;
            *v = (96 + (i / cw + 2 * f) % 64) as u8;
        }
    }

    fn view(&self) -> FrameView<'_> {
        FrameView {
            width: self.w as u32,
            height: self.h as u32,
            y: &self.y,
            u: &self.u,
            v: &self.v,
            stride_y: self.w,
            stride_uv: self.w.div_ceil(2),
        }
    }
}

fn encode_one(
    enc: &mut dyn VideoEncoder,
    p: &Picture,
    pts_ms: i64,
    force_keyframe: bool,
) -> Vec<AccessUnit> {
    enc.encode(
        &p.view(),
        EncodeOptions {
            pts_ms,
            force_keyframe,
            static_mbs: None,
            refine: None,
        },
    )
    .unwrap()
}

fn types(au: &AccessUnit) -> Vec<u8> {
    nal_units(&au.data).map(nal_type).collect()
}

/// The SPS fields a decoder sizes and colours the picture by.
#[derive(Debug, PartialEq)]
struct Sps {
    display: (u32, u32),
    color: Option<(bool, u8, u8, u8)>,
}

struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn bit(&mut self) -> u32 {
        let b = (self.d[self.pos / 8] >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        b as u32
    }
    fn bits(&mut self, n: u32) -> u32 {
        (0..n).fold(0, |a, _| a << 1 | self.bit())
    }
    fn ue(&mut self) -> u32 {
        let mut z = 0;
        while self.bit() == 0 {
            z += 1;
        }
        (1 << z) - 1 + self.bits(z)
    }
    fn se(&mut self) -> i32 {
        let k = self.ue() as i32;
        if k % 2 == 1 {
            (k + 1) / 2
        } else {
            -k / 2
        }
    }
}

fn parse_sps(nal: &[u8]) -> Sps {
    let mut rbsp = Vec::new();
    for (i, &b) in nal.iter().enumerate().skip(1) {
        if b == 3 && i >= 3 && nal[i - 1] == 0 && nal[i - 2] == 0 {
            continue;
        }
        rbsp.push(b);
    }
    let mut r = Bits { d: &rbsp, pos: 0 };
    let profile = r.bits(8) as u8;
    r.bits(16);
    r.ue();
    let mut chroma = 1;
    if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128) {
        chroma = r.ue();
        if chroma == 3 {
            r.bit();
        }
        r.ue();
        r.ue();
        r.bit();
        assert_eq!(r.bit(), 0, "scaling matrices aren't parsed");
    }
    r.ue();
    match r.ue() {
        0 => {
            r.ue();
        }
        1 => {
            r.bit();
            r.se();
            r.se();
            for _ in 0..r.ue() {
                r.se();
            }
        }
        _ => {}
    }
    r.ue();
    r.bit();
    let w = (r.ue() + 1) * 16;
    let h_units = r.ue() + 1;
    let frame_mbs_only = r.bit();
    let h = h_units * 16 * (2 - frame_mbs_only);
    if frame_mbs_only == 0 {
        r.bit();
    }
    r.bit();
    let (mut cw, mut ch) = (0, 0);
    if r.bit() == 1 {
        let (sx, sy) = if chroma == 1 { (2, 2) } else { (1, 1) };
        cw = (r.ue() + r.ue()) * sx;
        ch = (r.ue() + r.ue()) * sy * (2 - frame_mbs_only);
    }
    let mut color = None;
    if r.bit() == 1 {
        if r.bit() == 1 && r.bits(8) == 255 {
            r.bits(32);
        }
        if r.bit() == 1 {
            r.bit();
        }
        if r.bit() == 1 {
            r.bits(3);
            let full = r.bit() == 1;
            if r.bit() == 1 {
                let (p, t, m) = (r.bits(8) as u8, r.bits(8) as u8, r.bits(8) as u8);
                color = Some((full, p, t, m));
            }
        }
    }
    Sps {
        display: (w - cw, h - ch),
        color,
    }
}

fn sps_of(au: &AccessUnit) -> Sps {
    let sps = nal_units(&au.data)
        .find(|n| nal_type(n) == NAL_SPS)
        .expect("SPS");
    parse_sps(sps)
}

/// True when there is no ffmpeg to decode with: the test then says why and passes, unless
/// `STARFOLD_REQUIRE_FFMPEG=1`, which makes it fail.
fn without_ffmpeg(test: &str) -> bool {
    static FOUND: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    let found = FOUND.get_or_init(|| {
        match std::process::Command::new("ffmpeg")
            .arg("-version")
            .output()
        {
            Ok(o) if o.status.success() => Ok(()),
            Ok(o) => Err(format!("`ffmpeg -version` exited with {}", o.status)),
            Err(e) => Err(format!("no ffmpeg to run: {e}")),
        }
    });
    let Err(why) = found else {
        return false;
    };
    assert!(
        std::env::var_os("STARFOLD_REQUIRE_FFMPEG").is_none_or(|v| v != "1"),
        "{test}: {why}, and STARFOLD_REQUIRE_FFMPEG=1"
    );
    eprintln!("skipping {test}: {why}");
    true
}

/// What ffmpeg decodes from a stream of `codec` (Annex-B H.264, or AV1 in low-overhead
/// OBUs): the picture size, how many pictures came out, and the last one's luma.
fn decode_as(stream: &[u8], codec: Codec) -> ((usize, usize), usize, Vec<u8>) {
    use std::io::{BufRead, Read, Write};
    use std::process::{Command, Stdio};
    let format = match codec {
        Codec::Av1 => "obu",
        _ => "h264",
    };
    let mut child = Command::new("ffmpeg")
        .args(["-v", "error", "-xerror", "-f", format, "-i", "-"])
        .args(["-autoscale", "0", "-fps_mode", "passthrough"])
        .args(["-pix_fmt", "yuv420p", "-f", "yuv4mpegpipe", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("starting ffmpeg");
    let mut input = child.stdin.take().unwrap();
    let data = stream.to_vec();
    let writer = std::thread::spawn(move || input.write_all(&data));
    let mut out = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut line = Vec::new();
    out.read_until(b'\n', &mut line).unwrap();
    let header = String::from_utf8_lossy(&line).into_owned();
    let dim = |tag: char| {
        header
            .split_ascii_whitespace()
            .find_map(|f| f.strip_prefix(tag)?.parse::<usize>().ok())
            .unwrap_or(0)
    };
    let (w, h) = (dim('W'), dim('H'));
    let (mut luma, mut chroma) = (vec![0; w * h], vec![0; 2 * w.div_ceil(2) * h.div_ceil(2)]);
    let mut frames = 0;
    loop {
        line.clear();
        if out.read_until(b'\n', &mut line).unwrap() == 0 {
            break;
        }
        assert!(
            line.starts_with(b"FRAME"),
            "{header}: {}",
            String::from_utf8_lossy(&line)
        );
        out.read_exact(&mut luma).unwrap();
        out.read_exact(&mut chroma).unwrap();
        frames += 1;
    }
    let wrote = writer.join().unwrap();
    let done = child.wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&done.stderr);
    assert!(
        done.status.success() && err.is_empty() && wrote.is_ok(),
        "ffmpeg {} ({wrote:?}): {err}",
        done.status
    );
    ((w, h), frames, luma)
}

fn decode_all_as(aus: &[AccessUnit], codec: Codec) -> Vec<(usize, usize)> {
    let stream: Vec<u8> = aus.iter().flat_map(|a| a.data.iter().copied()).collect();
    let (size, frames, _) = decode_as(&stream, codec);
    vec![size; frames]
}

#[test]
fn idr_on_request_with_parameter_sets() {
    if without_ffmpeg("idr_on_request_with_parameter_sets") {
        return;
    }
    for b in backends() {
        let mut enc = open(&config(b, 320, 240, 4000)).unwrap();
        let mut p = Picture::new(320, 240);
        let mut aus = Vec::new();
        for f in 0..12 {
            p.texture(f as u32, 0, 0, 320, 48, 0);
            let got = encode_one(&mut *enc, &p, f * 33, f == 7);
            assert_eq!(got.len(), 1, "{b:?} frame {f}");
            aus.extend(got);
        }
        let codec = codec_of(b);
        for (f, au) in aus.iter().enumerate() {
            let want_key = f == 0 || f == 7;
            assert_eq!(au.keyframe, want_key, "{b:?} frame {f}");
            if codec == Codec::Av1 {
                assert_eq!(
                    super::av1::sequence_header(&au.data).is_some(),
                    want_key,
                    "{b:?} frame {f}: a sequence header exactly on keyframes"
                );
            } else {
                let t = types(au);
                assert_eq!(t.contains(&NAL_IDR), want_key, "{b:?} frame {f}");
                if want_key {
                    let sps = t.iter().position(|&x| x == NAL_SPS).expect("SPS");
                    let pps = t.iter().position(|&x| x == NAL_PPS).expect("PPS");
                    let idr = t.iter().position(|&x| x == NAL_IDR).unwrap();
                    assert!(sps < idr && pps < idr, "{b:?} frame {f}: {t:?}");
                }
            }
            assert_eq!(au.config.is_some(), f == 0, "{b:?} frame {f}");
        }
        assert_eq!(decode_all_as(&aus, codec).len(), aus.len(), "{b:?}");
        assert_eq!(
            decode_all_as(&aus[7..], codec).len(),
            aus.len() - 7,
            "{b:?}"
        );
    }
}

#[cfg(feature = "x264")]
#[test]
fn x264_signals_srgb_and_its_codec_string() {
    let mut enc = open(&config(Backend::X264, 640, 480, 4000)).unwrap();
    let au = encode_one(&mut *enc, &Picture::new(640, 480), 0, false).remove(0);
    let sps = sps_of(&au);
    assert_eq!(sps.color, Some((false, 1, 13, 6)));
    let c = au.config.expect("stream config");
    let sps_nal = nal_units(&au.data)
        .find(|n| nal_type(n) == NAL_SPS)
        .unwrap();
    assert_eq!(
        c.codec_string,
        format!(
            "avc1.{:02x}{:02x}{:02x}",
            sps_nal[1], sps_nal[2], sps_nal[3]
        )
    );
    assert_eq!((c.coded, c.display), ((640, 480), (640, 480)));
}

#[test]
fn odd_and_tiny_sizes_pad() {
    if without_ffmpeg("odd_and_tiny_sizes_pad") {
        return;
    }
    for b in backends() {
        for (w, h) in [
            (641, 361),
            (640, 361),
            (641, 360),
            (17, 9),
            (640, 7),
            (5, 40),
        ] {
            let mut enc = open(&config(b, w, h, 4000)).unwrap();
            let mut p = Picture::new(w as usize, h as usize);
            let mut aus = Vec::new();
            for f in 0..2 {
                p.texture(f, 0, 0, w as usize, h as usize, f as usize * 3);
                let opts = EncodeOptions {
                    pts_ms: f as i64 * 33,
                    ..Default::default()
                };
                let got = enc.encode(&p.view(), opts);
                aus.extend(got.unwrap_or_else(|e| panic!("{b:?} {w}x{h} frame {f}: {e}")));
            }
            let codec = codec_of(b);
            let refined = (codec == Codec::Av1).then(|| {
                let r = Refine {
                    qp: 12,
                    max_bytes: None,
                };
                assert!(enc.refinable(r), "{b:?} {w}x{h}");
                let opts = EncodeOptions {
                    pts_ms: 66,
                    refine: Some(r),
                    ..Default::default()
                };
                enc.encode(&p.view(), opts)
                    .unwrap_or_else(|e| panic!("{b:?} {w}x{h} refinement: {e}"))
            });
            if let Some(r) = refined {
                assert!(
                    !r.is_empty() && r.iter().all(|a| !a.keyframe),
                    "{b:?} {w}x{h}: a refinement pass through the padding"
                );
                aus.extend(r);
            }
            let side = |v: u32| (v.max(b.min_side()) + 1) & !1;
            let even = (side(w), side(h));
            let c = aus[0].config.clone().unwrap();
            assert_eq!((c.coded, c.display), (even, (w, h)), "{b:?} {w}x{h}");
            if codec == Codec::H264 {
                assert_eq!(sps_of(&aus[0]).display, even, "{b:?} {w}x{h}");
            }
            let sizes = decode_all_as(&aus, codec);
            assert_eq!(sizes.len(), aus.len(), "{b:?} {w}x{h}");
            assert!(
                sizes
                    .iter()
                    .all(|&s| s == (even.0 as usize, even.1 as usize)),
                "{b:?} {w}x{h}: {sizes:?}"
            );
        }
    }
}

#[test]
fn oversized_picture_fails_cleanly() {
    let e = open(&config(Backend::FakeH264, 8192, 4608, 20_000))
        .err()
        .expect("too large");
    assert!(e.0.contains("--stripes"), "{e}");
    assert!(Backend::FakeH264.fits(3840, 2160));
    assert!(Backend::FakeH264.fits(4096, 2304));
    assert!(!Backend::FakeH264.fits(4096, 2320));
}

#[test]
fn rate_change_without_reopen() {
    for b in backends() {
        let mut enc = open(&config(b, 640, 360, 20_000)).unwrap();
        let mut p = Picture::new(640, 360);
        let mut sizes = Vec::new();
        for f in 0..60 {
            if f == 30 {
                enc.set_rate(RateConfig::new(24.0, 300, 30)).unwrap();
            }
            p.video(f);
            let au = encode_one(&mut *enc, &p, f as i64 * 33, false).remove(0);
            assert_eq!(au.keyframe, f == 0, "{b:?} frame {f}");
            sizes.push(au.data.len());
        }
        let mean = |r: std::ops::Range<usize>| sizes[r.clone()].iter().sum::<usize>() / r.len();
        let (high, low) = (mean(10..30), mean(45..60));
        assert!(low * 3 < high, "{b:?}: {high} -> {low} bytes per frame");
    }
}

#[test]
fn short_or_wrapping_planes_are_rejected() {
    for b in backends() {
        let mut enc = open(&config(b, 64, 64, 4000)).unwrap();
        let p = Picture::new(64, 64);
        let mut v = p.view();
        v.y = &p.y[..64 * 63];
        assert!(enc.encode(&v, EncodeOptions::default()).is_err(), "{b:?}");
        let mut v = p.view();
        v.stride_y = usize::MAX / 63 + 1;
        assert!(enc.encode(&v, EncodeOptions::default()).is_err(), "{b:?}");
        let mut v = p.view();
        v.stride_uv = 16;
        assert!(enc.encode(&v, EncodeOptions::default()).is_err(), "{b:?}");
        let mut odd = open(&config(b, 63, 63, 4000)).unwrap();
        let q = Picture::new(63, 63);
        let mut v = q.view();
        v.v = &q.v[..10];
        assert!(odd.encode(&v, EncodeOptions::default()).is_err(), "{b:?}");
    }
    #[cfg(feature = "x264")]
    {
        let mut enc = open(&config(Backend::X264, 64, 64, 4000)).unwrap();
        let p = Picture::new(64, 64);
        let mut map = vec![1u8; 15];
        let opts = EncodeOptions {
            static_mbs: Some(&mut map),
            ..Default::default()
        };
        assert!(enc.encode(&p.view(), opts).is_err());
    }
}

/// `len` bytes of read-only address space reading as zeros, that no page backs until it is
/// touched; unmapped on drop.
#[cfg(all(unix, target_pointer_width = "64"))]
struct Untouched {
    at: *mut libc::c_void,
    len: usize,
}

#[cfg(all(unix, target_pointer_width = "64"))]
impl Untouched {
    fn new(len: usize) -> Self {
        let at = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        assert_ne!(
            at,
            libc::MAP_FAILED,
            "mapping {len} bytes: {}",
            std::io::Error::last_os_error()
        );
        Untouched { at, len }
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: a live, readable mapping of `len` bytes, never written, for `self`'s lifetime.
        unsafe { std::slice::from_raw_parts(self.at.cast(), self.len) }
    }
}

#[cfg(all(unix, target_pointer_width = "64"))]
impl Drop for Untouched {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.at, self.len) };
    }
}

/// A plane whose rows times its stride doesn't fit a C int is refused, though it holds them
/// all; two rows fewer, it fits. Any plane the bound refuses and the rest of the check accepts
/// is longer than `i32::MAX` bytes, so the luma plane is a 2 GiB mapping that nothing reads:
/// it takes address space, not memory.
#[cfg(all(unix, target_pointer_width = "64"))]
#[test]
fn a_plane_past_a_c_int_is_refused() {
    let stride = 1usize << 26;
    let y = Untouched::new(32 * stride);
    let uv = vec![128u8; 32 * 16];
    let at = |height: u32| FrameView {
        width: 64,
        height,
        y: y.bytes(),
        u: &uv,
        v: &uv,
        stride_y: stride,
        stride_uv: 32,
    };
    assert_eq!(32 * stride, i32::MAX as usize + 1);
    assert!(at(32).check().is_err(), "32 rows at 1 << 26");
    assert!(at(30).check().is_ok(), "30 rows at 1 << 26");
}

/// A plane with a stride wider than the picture that ends right after the last row's
/// pixels is refused before any encoder sees it: x264's C copy steps its pointer a stride
/// past every row, the last one too, which would point beyond its end.
/// With every row's stride there, it encodes.
#[test]
fn a_padded_plane_must_hold_its_last_rows_stride() {
    let (w, h, pad) = (64usize, 48usize, 16usize);
    let (cw, ch) = (w / 2, h / 2);
    let (sy, suv) = (w + pad, cw + pad / 2);
    let p = Picture::new(w, h);
    let padded = |src: &[u8], rows: usize, cols: usize, stride: usize| {
        let mut out = vec![0u8; rows * stride];
        for r in 0..rows {
            out[r * stride..][..cols].copy_from_slice(&src[r * cols..][..cols]);
        }
        out
    };
    let (y, u, v) = (
        padded(&p.y, h, w, sy),
        padded(&p.u, ch, cw, suv),
        padded(&p.v, ch, cw, suv),
    );
    let ends_at_last_pixel = |len: usize, rows: usize, cols: usize, stride: usize| {
        assert_eq!(len, rows * stride);
        (rows - 1) * stride + cols
    };
    let short = FrameView {
        width: w as u32,
        height: h as u32,
        y: &y[..ends_at_last_pixel(y.len(), h, w, sy)],
        u: &u[..ends_at_last_pixel(u.len(), ch, cw, suv)],
        v: &v[..ends_at_last_pixel(v.len(), ch, cw, suv)],
        stride_y: sy,
        stride_uv: suv,
    };
    assert!(short.check().is_err());
    let full = FrameView {
        y: &y,
        u: &u,
        v: &v,
        ..short
    };
    assert!(full.check().is_ok());
    for b in backends() {
        let mut enc = open(&config(b, w as u32, h as u32, 4000)).unwrap();
        let Err(e) = enc.encode(&short, EncodeOptions::default()) else {
            panic!("{b:?}: encoded a plane short of its last row's stride");
        };
        assert!(e.0.contains("don't hold a 64x48 picture"), "{b:?}: {e}");
        let only_luma = FrameView { y: short.y, ..full };
        assert!(
            enc.encode(&only_luma, EncodeOptions::default()).is_err(),
            "{b:?}: luma alone"
        );
        let aus = enc.encode(&full, EncodeOptions::default()).unwrap();
        assert!(!aus.is_empty(), "{b:?}");
    }
}

#[test]
fn idr_missing_parameter_sets_gets_cached_ones() {
    let nal = |t: u8, body: &[u8]| [&[0, 0, 0, 1, t][..], body].concat();
    let mut ps = ParamSets::default();
    let c = ColorInfo::SRGB_BT601_LIMITED;
    let sps = nal(0x67, &[0x42, 0xc0, 0x1f, 0x8c]);
    let pps = nal(0x68, &[0xce, 0x3c, 0x80]);
    let first = [sps.clone(), pps.clone(), nal(0x65, &[0x88, 0x80])].concat();
    let a = ps.unit(first.clone(), 0, (16, 16), c).unwrap();
    assert!(a.keyframe);
    assert_eq!(a.config.unwrap().codec_string, "avc1.42c01f");
    assert_eq!(a.data, first);
    let p = ps.unit(nal(0x41, &[0x9a]), 1, (16, 16), c).unwrap();
    assert!(!p.keyframe && p.config.is_none());
    let bare = nal(0x65, &[0x88, 0x84]);
    let b = ps.unit(bare.clone(), 2, (16, 16), c).unwrap();
    assert!(b.keyframe && b.config.is_none());
    assert_eq!(b.data, [sps, pps, bare].concat());
    assert!(ps.unit(nal(0x06, &[5, 1, 0]), 3, (16, 16), c).is_none());
}

#[test]
fn unknown_encoder_names() {
    assert!(Backend::parse("openh264")
        .unwrap_err()
        .contains("no longer has OpenH264"));
    assert!(Backend::parse("x265").is_err());
    #[cfg(not(feature = "x264"))]
    assert!(Backend::parse("x264")
        .unwrap_err()
        .contains("--features x264"));
    #[cfg(feature = "x264")]
    assert_eq!(Backend::parse("x264"), Ok(Backend::X264));
    #[cfg(target_os = "macos")]
    assert_eq!(Backend::parse("videotoolbox"), Ok(Backend::VideoToolbox));
    #[cfg(not(target_os = "macos"))]
    assert!(Backend::parse("videotoolbox")
        .unwrap_err()
        .contains("macOS"));
    #[cfg(any(target_os = "linux", windows))]
    assert_eq!(Backend::parse("nvenc"), Ok(Backend::Nvenc));
    #[cfg(not(any(target_os = "linux", windows)))]
    assert!(Backend::parse("nvenc").unwrap_err().contains("Windows"));
    #[cfg(windows)]
    assert_eq!(
        Backend::parse("mediafoundation"),
        Ok(Backend::MediaFoundation)
    );
    #[cfg(not(windows))]
    assert!(Backend::parse("mediafoundation")
        .unwrap_err()
        .contains("Windows-only"));
    #[cfg(all(windows, target_arch = "x86_64"))]
    assert_eq!(Backend::parse("qsv"), Ok(Backend::Qsv));
    #[cfg(not(all(windows, target_arch = "x86_64")))]
    assert!(Backend::parse("qsv").unwrap_err().contains("Windows-only"));
}

#[test]
fn backends_without_the_codec_refuse_to_open() {
    let mut cfg = config(SOFTWARE, 64, 64, 1000);
    cfg.codec = Codec::Hevc;
    assert!(open(&cfg).is_err());
    #[cfg(not(windows))]
    {
        cfg.backend = Backend::MediaFoundation;
        cfg.codec = Codec::H264;
        assert!(open(&cfg).is_err());
    }
}

/// What the startup probe finds on this Mac (printed for the PR, `--nocapture`).
#[cfg(target_os = "macos")]
#[test]
fn videotoolbox_probe() {
    use super::caps::{host_providers, Registry, PROBE_TIMEOUT};
    let r = Registry::probe(host_providers(true, false, true), PROBE_TIMEOUT);
    println!("{}", serde_json::to_string_pretty(&r).unwrap());
    for codec in [Codec::H264, Codec::Hevc] {
        let c = r.check(Backend::VideoToolbox, codec).unwrap();
        assert_eq!(c.acceleration, super::caps::Acceleration::Hardware);
    }
    let av1 = r
        .encoders
        .iter()
        .find(|c| c.backend == Backend::VideoToolbox && c.codec == Codec::Av1)
        .expect("AV1 is probed, not assumed");
    assert!(!av1.eligible());
}

#[cfg(target_os = "macos")]
fn vt(codec: Codec, w: u32, h: u32, kbps: u32) -> Box<dyn VideoEncoder> {
    let mut cfg = config(Backend::VideoToolbox, w, h, kbps);
    cfg.codec = codec;
    open(&cfg).unwrap()
}

/// Whether the startup probe finds `codec` in hardware VideoToolbox; says so when it doesn't.
#[cfg(target_os = "macos")]
fn vt_hardware(codec: Codec) -> bool {
    use super::caps::{host_providers, Acceleration, Registry, PROBE_TIMEOUT};
    let r = Registry::probe(host_providers(true, false, true), PROBE_TIMEOUT);
    let hw = r
        .check(Backend::VideoToolbox, codec)
        .is_ok_and(|c| c.acceleration == Acceleration::Hardware);
    if !hw {
        eprintln!("skipped: no hardware VideoToolbox {codec:?} encoder");
    }
    hw
}

/// A hardware encoder at `fps` and `kbps`, and 16 pictures of full-screen motion to cycle.
#[cfg(target_os = "macos")]
fn vt_at(
    codec: Codec,
    w: usize,
    h: usize,
    kbps: u32,
    fps: u32,
) -> (Box<dyn VideoEncoder>, Vec<Picture>) {
    let mut cfg = config(Backend::VideoToolbox, w as u32, h as u32, kbps);
    cfg.codec = codec;
    cfg.fps = fps;
    cfg.rate = RateConfig::new(24.0, kbps, fps);
    let pics = (0..16)
        .map(|f| {
            let mut p = Picture::new(w, h);
            p.video(f);
            p
        })
        .collect();
    (open(&cfg).unwrap(), pics)
}

/// Prints encode times of 1080p and 1440p motion at 120 fps timestamps (`--nocapture`): a
/// cohort whose encodes average more than 0.8 of the 8.3 ms interval falls back to 60.
#[cfg(target_os = "macos")]
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "times the encoder: needs an optimized build"
)]
fn videotoolbox_encode_times_at_120_fps() {
    for codec in [Codec::H264, Codec::Hevc] {
        if !vt_hardware(codec) {
            continue;
        }
        for (w, h) in [(1920, 1080), (2560, 1440)] {
            let (mut enc, pics) = vt_at(codec, w, h, 20_000, 120);
            let mut ms = Vec::new();
            for f in 0..360usize {
                let t = std::time::Instant::now();
                let got = encode_one(&mut *enc, &pics[f % 16], (f * 1000 / 120) as i64, false);
                if f >= 10 {
                    ms.push(t.elapsed().as_secs_f64() * 1000.0);
                }
                assert_eq!(got.len(), 1, "{codec:?} {w}x{h} frame {f}");
            }
            ms.sort_by(f64::total_cmp);
            let p95 = ms[ms.len() * 95 / 100];
            println!(
                "{codec:?} {w}x{h} at 120 fps: encode p50 {:.2} ms, p95 {p95:.2} ms",
                ms[ms.len() / 2]
            );
        }
    }
}

/// VideoToolbox budgets by timestamp: at 120 fps and at 60 each second carries the cap's
/// bytes (within 15%), and moving between the two rates costs no keyframe.
#[cfg(target_os = "macos")]
#[test]
fn videotoolbox_budgets_by_timestamp_at_120_and_60_fps() {
    let kbps = 8000;
    for codec in [Codec::H264, Codec::Hevc] {
        if !vt_hardware(codec) {
            continue;
        }
        let (mut enc, pics) = vt_at(codec, 1920, 1080, kbps, 120);
        let mut aus = Vec::new();
        let mut f = 0;
        for (phase, fps) in [120u32, 60, 120].into_iter().enumerate() {
            enc.set_rate(RateConfig::new(24.0, kbps, fps)).unwrap();
            let start = phase as i64 * 3000;
            for n in 0..3 * fps as i64 {
                let pts = start + n * 1000 / fps as i64;
                aus.extend(encode_one(&mut *enc, &pics[f % 16], pts, false));
                f += 1;
            }
        }
        aus.extend(enc.drain().unwrap());
        let keys: Vec<i64> = aus
            .iter()
            .filter(|a| a.keyframe)
            .map(|a| a.pts_ms)
            .collect();
        assert_eq!(keys, vec![0], "{codec:?}");
        for (phase, fps) in [120u32, 60, 120].into_iter().enumerate() {
            for s in 1..3 {
                let from = phase as i64 * 3000 + s * 1000;
                let bytes: usize = aus
                    .iter()
                    .filter(|a| (from..from + 1000).contains(&a.pts_ms))
                    .map(|a| a.data.len())
                    .sum();
                let got = bytes as f64 * 8.0 / 1000.0;
                println!("{codec:?} at {fps} fps, second {from} ms: {got:.0} kbit");
                assert!(
                    (got - kbps as f64).abs() <= kbps as f64 * 0.15,
                    "{codec:?} at {fps} fps, second {from} ms: {got:.0} kbit"
                );
            }
        }
    }
}

/// ~60 frames of text, scrolling and motion through the hardware encoder: one access
/// unit per frame, IDRs exactly where asked with their parameter sets first, and a
/// bitrate change without reopening.
#[cfg(target_os = "macos")]
#[test]
fn videotoolbox_encodes_h264_and_hevc() {
    use super::hevc;
    for codec in [Codec::H264, Codec::Hevc] {
        let (w, h) = (1280, 720);
        let mut enc = vt(codec, w as u32, h as u32, 8000);
        let mut p = Picture::new(w, h);
        let mut aus = Vec::new();
        for f in 0..60usize {
            match f / 20 {
                0 => p.texture(f as u32 + 1, 100 + f * 12, 200, 112 + f * 12, 222, 0),
                1 => p.texture(0, 200, 60, 1000, 660, f * 8),
                _ => p.video(f),
            }
            if f == 40 {
                enc.set_rate(RateConfig::new(24.0, 2000, 30)).unwrap();
            }
            let got = encode_one(&mut *enc, &p, f as i64 * 33, f == 30);
            assert_eq!(got.len(), 1, "{codec:?} frame {f}");
            aus.extend(got);
        }
        aus.extend(enc.drain().unwrap());
        assert_eq!(aus.len(), 60, "{codec:?}");
        for (f, au) in aus.iter().enumerate() {
            let want_key = f == 0 || f == 30;
            assert_eq!(au.keyframe, want_key, "{codec:?} frame {f}");
            assert_eq!(au.pts_ms, f as i64 * 33, "{codec:?} frame {f}");
            assert_eq!(au.config.is_some(), f == 0, "{codec:?} frame {f}");
            let t: Vec<u8> = nal_units(&au.data)
                .map(|n| match codec {
                    Codec::H264 => nal_type(n),
                    _ => hevc::nal_type(n),
                })
                .collect();
            let params: &[u8] = match codec {
                Codec::H264 => &[NAL_SPS, NAL_PPS],
                _ => &[hevc::NAL_VPS, hevc::NAL_SPS, hevc::NAL_PPS],
            };
            let first_slice = t
                .iter()
                .position(|&x| match codec {
                    Codec::H264 => matches!(x, 1 | NAL_IDR),
                    _ => hevc::is_slice(x),
                })
                .expect("a slice");
            for ps in params {
                let at = t.iter().position(|x| x == ps);
                assert_eq!(
                    at.is_some_and(|i| i < first_slice),
                    want_key,
                    "{codec:?} frame {f}: {t:?}"
                );
            }
        }
        let c = aus[0].config.as_ref().unwrap();
        assert_eq!(c.coded, (w as u32, h as u32));
        match codec {
            Codec::H264 => {
                assert!(c.codec_string.starts_with("avc1.64"), "{}", c.codec_string);
                let sps_nal = nal_units(&aus[0].data)
                    .find(|n| nal_type(n) == NAL_SPS)
                    .unwrap();
                assert_eq!(sps_nal[1], 100);
                let sps = sps_of(&aus[0]);
                assert_eq!(sps.display, (w as u32, h as u32));
                assert_eq!(sps.color, Some((false, 1, 13, 6)));
                assert_eq!(decode_all_as(&aus, Codec::H264).len(), aus.len());
                assert_eq!(decode_all_as(&aus[30..], Codec::H264).len(), aus.len() - 30);
            }
            _ => assert!(c.codec_string.starts_with("hvc1.1."), "{}", c.codec_string),
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn videotoolbox_pads_small_and_odd_sizes() {
    for codec in [Codec::H264, Codec::Hevc] {
        for (w, h) in [(64, 64), (333, 77), (2, 2), (1920, 1080)] {
            let mut enc = vt(codec, w as u32, h as u32, 4000);
            let au = encode_one(&mut *enc, &Picture::new(w, h), 0, false).remove(0);
            assert!(au.keyframe, "{codec:?} {w}x{h}");
            assert_eq!(au.config.unwrap().display, (w as u32, h as u32));
        }
    }
}

/// Writes the default encoder's typing, scrolling and video streams to `$STREAM_DIR`
/// for bench/webcodecs-decode.mjs: per access unit a big-endian u32 length, a keyframe
/// byte and the Annex-B bytes.
#[cfg(feature = "x264")]
#[test]
#[ignore]
fn dump_streams() {
    let dir = std::path::PathBuf::from(std::env::var("STREAM_DIR").expect("STREAM_DIR"));
    let (w, h) = (1280usize, 720usize);
    for workload in ["typing", "scrolling", "video"] {
        let mut cfg = config(SOFTWARE, w as u32, h as u32, 30_000);
        cfg.best_quality = cfg.rate.quality;
        let mut enc = open(&cfg).unwrap();
        let mut p = Picture::new(w, h);
        let mut out = Vec::new();
        for f in 0..120 {
            match workload {
                "typing" => {
                    let (x, y) = (100 + (f % 80) * 12, 200 + (f / 80) * 22);
                    p.texture(f as u32 + 1, x, y, x + 12, y + 22, 0);
                }
                "scrolling" => p.texture(0, 200, 60, 1000, 660, f * 8),
                _ => p.video(f),
            }
            for au in encode_one(&mut *enc, &p, f as i64 * 33, f == 60) {
                out.extend_from_slice(&(au.data.len() as u32).to_be_bytes());
                out.push(au.keyframe as u8);
                out.extend_from_slice(&au.data);
            }
        }
        std::fs::write(dir.join(format!("{workload}.h264s")), out).unwrap();
    }
}

/// CPU per stream at 1080p30 for typing, scrolling and video-like content:
/// `cargo test --release cpu_per_stream -- --ignored --nocapture` (BACKEND, CODEC, FRAMES,
/// THREADS). One line per workload, `bench` first.
#[cfg(unix)]
#[test]
#[ignore]
fn cpu_per_stream() {
    let backend =
        Backend::parse(&std::env::var("BACKEND").unwrap_or(SOFTWARE.name().into())).unwrap();
    let env = |k: &str, d: usize| {
        std::env::var(k)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(d)
    };
    let (frames, threads) = (env("FRAMES", 300), env("THREADS", 1));
    let codec = match std::env::var("CODEC").as_deref() {
        Ok("hevc") => Codec::Hevc,
        Ok("av1") => Codec::Av1,
        Ok("h264") => Codec::H264,
        _ if backend == SOFTWARE => SOFTWARE_CODEC,
        _ => Codec::H264,
    };
    let (w, h, fps) = (1920usize, 1080usize, 30u32);
    let cols = w.div_ceil(16);
    let mbs = cols * h.div_ceil(16);
    let cpu = || unsafe {
        let mut r: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut r);
        (r.ru_utime.tv_sec + r.ru_stime.tv_sec) as f64
            + (r.ru_utime.tv_usec + r.ru_stime.tv_usec) as f64 / 1e6
    };
    for workload in ["typing", "scrolling", "video"] {
        let mut cfg = config(backend, w as u32, h as u32, 30_000);
        cfg.codec = codec;
        cfg.fps = fps;
        cfg.threads = threads as u32;
        cfg.best_quality = cfg.rate.quality;
        let mut enc = open(&cfg).unwrap();
        let mut p = Picture::new(w, h);
        let (mut bytes, mut lat, mut cpu_s) = (0usize, Vec::new(), 0.0);
        for f in 0..frames {
            let mut dirty = vec![0u8; mbs];
            let mut mark = |x0: usize, y0: usize, x1: usize, y1: usize| {
                for r in y0 / 16..y1.div_ceil(16) {
                    dirty[r * cols + x0 / 16..r * cols + x1.div_ceil(16)].fill(1);
                }
            };
            match workload {
                "typing" => {
                    let (x, y) = (200 + (f % 120) * 12, 300 + (f / 120) * 22);
                    p.texture(f as u32 + 1, x, y, x + 12, y + 22, 0);
                    mark(x, y, x + 12, y + 22);
                }
                "scrolling" => {
                    p.texture(0, 300, 90, 1500, 990, f * 8);
                    mark(300, 90, 1500, 990);
                }
                _ => {
                    p.video(f);
                    mark(0, 0, w, h);
                }
            }
            let mut hints: Vec<u8> = dirty.iter().map(|&d| (d == 0) as u8).collect();
            let (c0, t0) = (cpu(), std::time::Instant::now());
            let aus = enc
                .encode(
                    &p.view(),
                    EncodeOptions {
                        pts_ms: f as i64 * 1000 / fps as i64,
                        force_keyframe: f == 0,
                        static_mbs: Some(&mut hints),
                        refine: None,
                    },
                )
                .unwrap();
            let (dt, dc) = (t0.elapsed().as_secs_f64() * 1e3, cpu() - c0);
            if f >= fps as usize {
                lat.push(dt);
                cpu_s += dc;
                bytes += aus.iter().map(|a| a.data.len()).sum::<usize>();
            }
        }
        lat.sort_by(|a, b| a.total_cmp(b));
        let secs = lat.len() as f64 / fps as f64;
        let pct = |q: f64| lat[((lat.len() - 1) as f64 * q).round() as usize];
        println!(
            "bench backend={} codec={codec:?} workload={workload} threads={threads} cpu_s_per_s={:.4} kbps={:.0} p50_ms={:.2} p95_ms={:.2}",
            backend.name(),
            cpu_s / secs,
            bytes as f64 * 8.0 / secs / 1000.0,
            pct(0.5),
            pct(0.95)
        );
    }
}

fn luma_psnr(a: &[u8], stride: usize, p: &Picture) -> f64 {
    let se: u64 = (0..p.h)
        .flat_map(|r| (0..p.w).map(move |c| (r, c)))
        .map(|(r, c)| (a[r * stride + c] as i64 - p.y[r * p.w + c] as i64).pow(2) as u64)
        .sum();
    match se {
        0 => 99.0,
        _ => 10.0 * (255.0f64 * 255.0 * (p.w * p.h) as f64 / se as f64).log10(),
    }
}

/// A stream of scrolled text, then whatever the test does, decoded when a test asks how sharp
/// it is.
#[cfg(feature = "x264")]
struct Refining {
    enc: Box<dyn VideoEncoder>,
    pic: Picture,
    pts: i64,
    scrolled: usize,
    /// What each frame tells the encoder didn't change (x264 marks up its copy).
    map: Option<Vec<u8>>,
    /// Every byte the encoder sent, in order.
    stream: Vec<u8>,
    /// How many pictures the stream holds, and the last one sent.
    frames: usize,
    shown: Picture,
    /// The PSNR [`Refining::psnr`] found, and at how many pictures.
    decoded: Option<(usize, f64)>,
}

#[cfg(feature = "x264")]
impl Refining {
    fn config(backend: Backend, threads: u32, fps: u32) -> EncoderConfig {
        let mut cfg = config(backend, 640, 480, 8000);
        cfg.threads = threads;
        cfg.fps = fps;
        cfg.best_quality = 24.0;
        cfg
    }

    fn on(backend: Backend, threads: u32, fps: u32) -> Self {
        Self::with(open(&Self::config(backend, threads, fps)).unwrap())
    }

    fn with(enc: Box<dyn VideoEncoder>) -> Self {
        Self::sized(enc, 640, 480)
    }

    fn sized(enc: Box<dyn VideoEncoder>, w: usize, h: usize) -> Self {
        let mut me = Refining {
            enc,
            pic: Picture::new(w, h),
            pts: 0,
            scrolled: 0,
            map: None,
            stream: Vec::new(),
            frames: 0,
            shown: Picture::new(w, h),
            decoded: None,
        };
        me.step(true, None);
        me
    }

    /// Bytes and whether it was a keyframe; a frame that doesn't come out is 0 bytes.
    fn step(&mut self, force_keyframe: bool, refine: Option<Refine>) -> (usize, bool) {
        self.pts += 33;
        let mut map = self.map.clone();
        let aus = self
            .enc
            .encode(
                &self.pic.view(),
                EncodeOptions {
                    pts_ms: self.pts,
                    force_keyframe,
                    static_mbs: map.as_deref_mut(),
                    refine,
                },
            )
            .unwrap();
        let (mut n, mut key) = (0, false);
        for au in &aus {
            n += au.data.len();
            key |= au.keyframe;
            self.stream.extend_from_slice(&au.data);
            self.frames += 1;
            self.shown.clone_from(&self.pic);
        }
        (n, key)
    }

    /// The luma PSNR of the last picture the stream holds, decoded from the whole stream.
    fn psnr(&mut self) -> f64 {
        if let Some((frames, psnr)) = self.decoded {
            if frames == self.frames {
                return psnr;
            }
        }
        let (size, frames, luma) = decode_as(&self.stream, Codec::H264);
        assert_eq!(frames, self.frames, "pictures decoded");
        assert_eq!(size, (self.pic.w, self.pic.h));
        let psnr = luma_psnr(&luma, size.0, &self.shown);
        self.decoded = Some((self.frames, psnr));
        psnr
    }

    fn scroll(&mut self, frames: usize) -> usize {
        let (w, h) = (self.pic.w, self.pic.h);
        (0..frames)
            .map(|_| {
                self.scrolled += 12;
                self.pic.texture(0, 0, 0, w, h, self.scrolled);
                let (n, key) = self.step(false, None);
                assert!(!key, "motion got an IDR");
                n
            })
            .sum()
    }

    fn pass(&mut self, qp: u8, max_bytes: Option<usize>) -> (usize, bool) {
        self.step(false, Some(Refine { qp, max_bytes }))
    }
}

/// x264 under VBV, as it streams, at one thread and sliced, each with and without a byte
/// budget: every pass sends bytes and no IDR. At one thread no pass leaves the picture worse,
/// a budget holds within twice itself, and the ladder ends far sharper than motion left it and
/// complete. Sliced runs under VBV assert only bytes and no IDR: x264's row rate control reads
/// the other slices' live size estimates (`ratecontrol.c` `x264_ratecontrol_mb`), so their
/// pictures and sizes depend on thread timing; under the full suite's load a first budgeted
/// step once came out 6.06e-5 dB below the one before. Sliced refinement is asserted rung by
/// rung without VBV, below. A budgeted first step may change nothing: motion records its blocks
/// at QP 52, above any QP a pass aims for, until a pass proves better.
#[cfg(feature = "x264")]
#[test]
fn x264_refining_sharpens_without_an_idr_at_any_thread_count_or_budget() {
    if without_ffmpeg("x264_refining_sharpens_without_an_idr_at_any_thread_count_or_budget") {
        return;
    }
    for (threads, max_bytes) in [(1, None), (1, Some(60_000)), (4, None), (4, Some(60_000))] {
        let mut s = Refining::on(Backend::X264, threads, 30);
        s.enc.set_rate(RateConfig::new(24.0, 3000, 30)).unwrap();
        s.scroll(30);
        let motion = s.psnr();
        let mut prev = motion;
        for qp in [20, 16, 12] {
            let (n, key) = s.pass(qp, max_bytes);
            let at = format!("t{threads} {max_bytes:?} qp {qp}");
            assert!(n > 0 && !key, "{at}: {n} bytes, key {key}");
            if threads == 1 {
                assert!(max_bytes.is_none_or(|m| n <= m * 2), "{at}: {n} bytes");
                assert!(s.psnr() >= prev, "{at}: {} after {prev}", s.psnr());
            }
            prev = s.psnr();
        }
        if threads == 1 {
            assert!(prev > motion + 3.0, "t1 {max_bytes:?}: {motion} -> {prev}");
            assert!(!s.enc.refinable(Refine {
                qp: 12,
                max_bytes: None
            }));
        }
        s.scroll(5);
    }
}

/// Sliced x264 without VBV codes the same bytes whatever the threads' timing: no slice reads
/// another's state while it encodes (row rate control returns before its cross-slice read
/// without VBV, no deblocking across slices, slice results merged in thread order). So each
/// rung of the ladder is strictly sharper than the one before, and a second encoder fed the
/// same frames sends the same bytes.
#[cfg(feature = "x264")]
#[test]
fn x264_sliced_refining_without_vbv_sharpens_every_rung_the_same_every_run() {
    if without_ffmpeg("x264_sliced_refining_without_vbv_sharpens_every_rung_the_same_every_run") {
        return;
    }
    let run = || {
        let mut cfg = Refining::config(Backend::X264, 4, 30);
        cfg.rate = RateConfig::new(24.0, 0, 30);
        let mut s = Refining::with(open(&cfg).unwrap());
        s.scroll(30);
        let motion = s.psnr();
        let mut prev = motion;
        for qp in [20, 16, 12] {
            let (n, key) = s.pass(qp, None);
            assert!(n > 0 && !key, "qp {qp}: {n} bytes, key {key}");
            assert!(s.psnr() > prev, "qp {qp}: {} after {prev}", s.psnr());
            prev = s.psnr();
        }
        assert!(prev > motion + 3.0, "{motion} -> {prev}");
        assert!(!s.enc.refinable(Refine {
            qp: 12,
            max_bytes: None
        }));
        s.scroll(5);
        s.stream
    };
    let first = run();
    let second = run();
    assert!(!first.is_empty());
    assert!(
        first == second,
        "two encoders sent different bytes: {} and {}",
        first.len(),
        second.len()
    );
}

/// 1080p text under a 2 Mbit/s VBV, refined as the engine does it: the ladder at its planned
/// budget, then tail passes at the idle one while the encoder says one would still sharpen.
/// First after the whole screen scrolled, then after only a band did, with x264 told the
/// rest didn't change. Each settles within the tail limit, about as sharp as with no VBV at
/// all: reaching QP 12 in several steps rather than one costs up to ~0.6 dB, a block left
/// coarse several.
#[cfg(feature = "x264")]
#[test]
fn x264_converges_under_a_tight_vbv() {
    if without_ffmpeg("x264_converges_under_a_tight_vbv") {
        return;
    }
    let (w, h): (usize, usize) = (1920, 1080);
    let cols = w / 16;
    let band = 20..48;
    let still_outside: Vec<u8> = (0..h.div_ceil(16))
        .flat_map(|r| std::iter::repeat_n(u8::from(!band.contains(&r)), cols))
        .collect();
    let run = |kbps: u32| {
        let mut cfg = config(Backend::X264, w as u32, h as u32, kbps);
        cfg.best_quality = 24.0;
        let mut s = Refining::sized(open(&cfg).unwrap(), w, h);
        let budget = |ms: usize| (kbps > 0).then_some(kbps as usize * ms / 8 * 55 / 100);
        let settle = |s: &mut Refining| {
            for qp in [20, 16, 12] {
                s.pass(qp, budget(200));
            }
            let tail = Refine {
                qp: 12,
                max_bytes: budget(1000),
            };
            let mut tails = 0;
            while s.enc.refinable(tail) {
                assert!(
                    tails < 64,
                    "{kbps} kbit/s: still refinable after {tails} tails"
                );
                s.step(false, Some(tail));
                tails += 1;
            }
            (s.psnr(), tails)
        };
        s.scroll(30);
        let whole = settle(&mut s);
        s.map = Some(still_outside.clone());
        for _ in 0..10 {
            s.scrolled += 12;
            s.pic
                .texture(0, 0, band.start * 16, w, band.end * 16, s.scrolled);
            s.step(false, None);
        }
        let banded = settle(&mut s);
        (whole, banded)
    };
    let (capped, free) = (run(2000), run(0));
    for (what, c, f) in [("whole", capped.0, free.0), ("band", capped.1, free.1)] {
        assert!(
            c.0 > f.0 - 1.0,
            "{what}: {:.2} dB after {} tails at 2 Mbit/s, {:.2} dB uncapped",
            c.0,
            c.1,
            f.0
        );
    }
}

/// A capped x264 stream at CRF 10 may start a motion frame at QP 10 and raise its rows far past
/// 12: whatever it started at, the blocks it coded still want sharpening once it stops.
#[cfg(feature = "x264")]
#[test]
fn x264_motion_counts_as_coarse_whatever_qp_it_started_at() {
    let mut cfg = config(Backend::X264, 640, 480, 500);
    cfg.rate = RateConfig::new(10.0, 500, 30);
    cfg.best_quality = 10.0;
    let mut s = Refining::sized(open(&cfg).unwrap(), 640, 480);
    s.scroll(10);
    for qp in [20, 12] {
        let r = Refine {
            qp,
            max_bytes: None,
        };
        assert!(s.enc.refinable(r), "qp {qp}");
    }
}

/// What x264 does with motion after passes is what it does without them: the pass's lower CRF,
/// CRF-max and wider VBV are all undone, so under a tight cap a busy frame afterwards is held
/// to the buffer as before, not let through by a leftover CRF-max (also across a rate change
/// that lands before the next frame).
#[cfg(feature = "x264")]
#[test]
fn x264_motion_after_passes_keeps_its_rate_control() {
    let run = |passes: bool| {
        let mut s = Refining::on(Backend::X264, 1, 30);
        s.enc.set_rate(RateConfig::new(24.0, 1000, 30)).unwrap();
        s.scroll(10);
        for qp in [20, 16, 12] {
            match passes {
                true => s.pass(qp, Some(60_000)),
                false => s.step(false, None),
            };
        }
        s.enc.set_rate(RateConfig::new(24.0, 1000, 30)).unwrap();
        (0..8)
            .map(|f| {
                s.pic.video(f);
                s.step(false, None).0
            })
            .collect::<Vec<_>>()
    };
    let (with, without) = (run(true), run(false));
    let (a, b) = (with.iter().sum::<usize>(), without.iter().sum::<usize>());
    assert!(
        (a as f64) < b as f64 * 1.25,
        "busy frames after passes: {with:?} vs {without:?}"
    );
}

/// An encoder that is asked for passes it can't fit never changes its rate control: its stream
/// is byte for byte the one it makes without them.
#[cfg(feature = "x264")]
#[test]
fn x264_a_pass_not_taken_leaves_the_stream_byte_identical() {
    let run = |ask: bool| {
        let mut enc = open(&Refining::config(Backend::X264, 1, 30)).unwrap();
        let mut p = Picture::new(640, 480);
        let mut out = Vec::new();
        for f in 0..20i64 {
            if f % 5 == 4 {
                if ask {
                    let opts = EncodeOptions {
                        pts_ms: f * 33,
                        refine: Some(Refine {
                            qp: 20,
                            max_bytes: Some(10),
                        }),
                        ..Default::default()
                    };
                    assert!(enc.encode(&p.view(), opts).unwrap().is_empty());
                }
                continue;
            }
            p.texture(0, 0, 0, 640, 480, f as usize * 12);
            let opts = EncodeOptions {
                pts_ms: f * 33,
                force_keyframe: f == 0,
                ..Default::default()
            };
            out.extend(
                enc.encode(&p.view(), opts)
                    .unwrap()
                    .into_iter()
                    .map(|a| a.data),
            );
        }
        out
    };
    assert!(run(true) == run(false));
}

/// Adaptive quantisation offsets each block's QP, so no pass could prove it held: x264 then
/// never says a pass would sharpen.
#[cfg(feature = "x264")]
#[test]
fn x264_with_adaptive_quantisation_takes_no_tails() {
    let mut cfg = Refining::config(Backend::X264, 1, 30);
    cfg.preset = "superfast".into();
    let mut s = Refining::sized(open(&cfg).unwrap(), 640, 480);
    s.scroll(5);
    assert!(!s.enc.refinable(Refine {
        qp: 12,
        max_bytes: None
    }));
}

/// ultrafast writes CAVLC, where x264 may re-code a block at a higher QP to fit a level into
/// the escape code: a pass below QP 12 still runs at its QP and sharpens, but never counts as
/// settled, so the encoder keeps offering one (the tail limit ends them).
#[cfg(feature = "x264")]
#[test]
fn x264_a_cavlc_pass_below_12_runs_but_never_settles() {
    if without_ffmpeg("x264_a_cavlc_pass_below_12_runs_but_never_settles") {
        return;
    }
    let mut s = Refining::on(Backend::X264, 1, 30);
    s.scroll(5);
    let motion = s.psnr();
    let below = Refine {
        qp: 8,
        max_bytes: None,
    };
    for i in 0..5 {
        let (n, key) = s.step(false, Some(below));
        assert!(n > 0 && !key, "pass {i}: {n} bytes");
        assert!(s.enc.refinable(below), "pass {i} counted as settled");
    }
    assert!(s.psnr() > motion + 3.0, "{motion} -> {}", s.psnr());
    assert!(s.pass(12, None).0 > 0);
    assert!(!s.enc.refinable(Refine {
        qp: 12,
        max_bytes: None
    }));
}

/// Motion counts as unknown, above 51: with --paintover-crf=51 a pass at 51 still runs after it,
/// and then counts as settled.
#[cfg(feature = "x264")]
#[test]
fn x264_motion_still_gets_a_pass_at_51() {
    let mut s = Refining::on(Backend::X264, 1, 30);
    s.enc.set_rate(RateConfig::new(24.0, 500, 30)).unwrap();
    s.scroll(10);
    let at51 = Refine {
        qp: 51,
        max_bytes: None,
    };
    assert!(s.enc.refinable(at51));
    let (n, key) = s.step(false, Some(at51));
    assert!(n > 0 && !key, "{n} bytes");
    assert!(!s.enc.refinable(at51));
}

#[cfg(feature = "x264")]
#[test]
fn x264_a_pass_that_cannot_fit_sends_nothing() {
    if without_ffmpeg("x264_a_pass_that_cannot_fit_sends_nothing") {
        return;
    }
    let mut s = Refining::on(Backend::X264, 1, 30);
    s.scroll(5);
    let before = s.psnr();
    assert!(!s.enc.refinable(Refine {
        qp: 20,
        max_bytes: Some(100)
    }));
    assert_eq!(s.pass(20, Some(100)), (0, false));
    assert_eq!(s.psnr(), before);
    assert!(s.pass(20, None).0 > 0);
}

#[cfg(feature = "x264")]
#[test]
fn x264_a_refining_pass_with_nothing_to_gain_sends_nothing() {
    if without_ffmpeg("x264_a_refining_pass_with_nothing_to_gain_sends_nothing") {
        return;
    }
    let mut s = Refining::on(Backend::X264, 1, 30);
    s.scroll(5);
    assert!(s.pass(12, None).0 > 0);
    let before = s.psnr();
    assert_eq!(s.pass(16, None).0, 0);
    assert_eq!(s.pass(12, None).0, 0);
    assert_eq!(s.psnr(), before);
    s.scroll(1);
    assert!(s.pass(12, None).0 > 0, "motion makes room again");
}

/// The pass's wider VBV and lower CRF are undone: motion after costs no more than it would
/// have (a sharper reference may make it a little cheaper).
#[cfg(feature = "x264")]
#[test]
fn x264_refining_leaves_rate_control_where_it_was() {
    let mut plain = Refining::on(Backend::X264, 1, 30);
    let mut refined = Refining::on(Backend::X264, 1, 30);
    for s in [&mut plain, &mut refined] {
        s.enc.set_rate(RateConfig::new(24.0, 2000, 30)).unwrap();
        s.scroll(30);
    }
    for _ in 0..3 {
        plain.step(false, None);
    }
    for qp in [20, 16, 12] {
        refined.pass(qp, Some(60_000));
    }
    plain.scroll(10);
    refined.scroll(10);
    let (a, b) = (plain.scroll(110) as f64, refined.scroll(110) as f64);
    assert!(
        (-0.10..0.03).contains(&(b / a - 1.0)),
        "motion after: {a} vs {b} bytes"
    );
}

#[cfg(feature = "x264")]
#[test]
fn x264_first_frame_and_keyframes_stay_under_rate_control() {
    let p = Picture::new(640, 480);
    let refine = Refine {
        qp: 12,
        max_bytes: None,
    };
    let first = |refine| {
        let mut enc = open(&Refining::config(Backend::X264, 1, 30)).unwrap();
        let opts = EncodeOptions {
            refine,
            ..Default::default()
        };
        let aus = enc.encode(&p.view(), opts).unwrap();
        assert!(aus[0].keyframe);
        aus[0].data.len()
    };
    assert_eq!(first(Some(refine)), first(None));
    let mut s = Refining::on(Backend::X264, 1, 30);
    s.scroll(5);
    let (plain, key) = s.step(true, None);
    assert!(key);
    let (with, key) = s.step(true, Some(refine));
    assert!(key);
    assert!(with < plain * 2, "IDR at the pass's QP: {with} vs {plain}");
}

#[test]
fn a_tight_budget_takes_a_smaller_step() {
    let refine = |qp, max_bytes| Refine {
        qp,
        max_bytes: Some(max_bytes),
    };
    let s = sharpness::Sharpness::new(8160, 24);
    assert_eq!(s.pick(refine(20, 200_000)), Some(20));
    assert_eq!(s.pick(refine(20, 90_000)), Some(22));
    assert_eq!(s.pick(refine(20, 41_000)), Some(23));
    assert_eq!(s.pick(refine(20, 40_000)), None);
    assert_eq!(s.pick(refine(24, 200_000)), None);
    // A coarse picture sharpens far further for the same bytes.
    let s = sharpness::Sharpness::new(8160, 48);
    assert_eq!(s.pick(refine(20, 50_000)), Some(44));
    let mut s = sharpness::Sharpness::new(8160, 24);
    s.off = true;
    assert_eq!(s.pick(refine(20, 200_000)), None);
}

#[test]
fn the_estimate_learns_from_what_frames_cost() {
    let budget = |max_bytes| Refine {
        qp: 12,
        max_bytes: Some(max_bytes),
    };
    let mut s = sharpness::Sharpness::new(1000, 24);
    assert_eq!(s.pick(budget(10_000)), Some(22));
    // More than expected counts at once, up to the most a screen costs.
    s.refined(22, 80_000);
    assert_eq!(s.pick(budget(30_000)), Some(20));
    // Cheaper than expected counts by half; next to nothing doesn't count.
    s.refined(20, 2_000);
    assert_eq!(s.pick(budget(16_000)), Some(18));
    s.refined(20, 13);
    assert_eq!(s.pick(budget(16_000)), Some(18));
    // Motion puts the picture back where it was.
    s.moved(30, None);
    assert_eq!(s.pick(budget(1_000)), None);
    assert_eq!(s.pick(budget(1_000_000)), Some(12));
    // A stuck estimate comes back down.
    s.refined(29, 1_000_000);
    assert_eq!(s.pick(budget(6_000)), None);
    for _ in 0..8 {
        s.skipped();
    }
    assert_eq!(s.pick(budget(6_000)), Some(28));
    // A skip needn't lower an estimate that was already low.
    let mut s = sharpness::Sharpness::new(1000, 24);
    s.skipped();
    assert_eq!(s.pick(budget(5_000)), Some(23));
    assert_eq!(s.pick(budget(4_999)), None);
}

#[test]
fn the_estimate_counts_every_block_still_coarse() {
    let budget = |max_bytes| Refine {
        qp: 12,
        max_bytes: Some(max_bytes),
    };
    let moved = |range: std::ops::Range<usize>| {
        let mut map = vec![1u8; 1000];
        map[range].fill(0);
        map
    };
    // Only what changed moved: the rest stays as sharp as it was.
    let mut s = sharpness::Sharpness::new(1000, 12);
    s.moved(24, Some(&moved(0..100)));
    assert_eq!(s.pick(budget(1_000_000)), Some(12));
    assert_eq!(s.pick(budget(500)), Some(23));
    assert_eq!(s.pick(budget(499)), None);
    // A region that went still earlier still counts when another one stops later.
    s.moved(24, Some(&moved(100..1000)));
    assert_eq!(s.pick(budget(500)), None);
    assert_eq!(s.pick(budget(5_000)), Some(23));
    // A pass that only got part of the way leaves all of it that coarse, however small
    // the next stop is.
    s.refined(20, 0);
    s.moved(24, Some(&moved(0..1)));
    assert_eq!(s.pick(budget(5_000)), Some(20));
    assert_eq!(s.pick(budget(5_020)), Some(19));
    // A map that doesn't match counts as everything.
    let mut s = sharpness::Sharpness::new(1000, 12);
    s.moved(24, Some(&[0u8; 10]));
    assert_eq!(s.qp.iter().filter(|&&q| q == 24).count(), 1000);
    s.refined(12, 0);
    assert_eq!(s.pick(budget(1_000_000)), None, "nothing left to sharpen");
}
