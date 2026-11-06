use super::h264::{nal_type, nal_units, ParamSets, NAL_IDR, NAL_PPS, NAL_SPS};
use super::*;

fn backends() -> Vec<Backend> {
    vec![
        Backend::OpenH264,
        #[cfg(feature = "x264")]
        Backend::X264,
    ]
}

fn config(backend: Backend, width: u32, height: u32, max_kbps: u32) -> EncoderConfig {
    EncoderConfig {
        backend,
        codec: Codec::H264,
        width,
        height,
        fps: 30,
        threads: 1,
        rate: RateConfig::new(24.0, max_kbps, 30),
        best_quality: 16.0,
        preset: "ultrafast".into(),
    }
}

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
    profile: u8,
    constraints: u8,
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
    let constraints = r.bits(8) as u8;
    r.bits(8);
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
        profile,
        constraints,
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

fn decode_all(aus: &[AccessUnit]) -> Vec<(usize, usize)> {
    use ::openh264::formats::YUVSource;
    let mut dec = ::openh264::decoder::Decoder::new().unwrap();
    let mut sizes = Vec::new();
    for au in aus {
        if let Some(pic) = dec.decode(&au.data).unwrap() {
            sizes.push(pic.dimensions());
        }
    }
    sizes
}

#[test]
fn idr_on_request_with_parameter_sets() {
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
        for (f, au) in aus.iter().enumerate() {
            let t = types(au);
            let want_key = f == 0 || f == 7;
            assert_eq!(au.keyframe, want_key, "{b:?} frame {f}: {t:?}");
            assert_eq!(t.contains(&NAL_IDR), want_key, "{b:?} frame {f}");
            if want_key {
                let sps = t.iter().position(|&x| x == NAL_SPS).expect("SPS");
                let pps = t.iter().position(|&x| x == NAL_PPS).expect("PPS");
                let idr = t.iter().position(|&x| x == NAL_IDR).unwrap();
                assert!(sps < idr && pps < idr, "{b:?} frame {f}: {t:?}");
            }
            assert_eq!(au.config.is_some(), f == 0, "{b:?} frame {f}");
        }
        assert_eq!(decode_all(&aus).len(), aus.len(), "{b:?}");
        assert_eq!(decode_all(&aus[7..]).len(), aus.len() - 7, "{b:?}");
    }
}

#[test]
fn openh264_signals_constrained_baseline_and_srgb() {
    let mut enc = open(&config(Backend::OpenH264, 640, 480, 4000)).unwrap();
    let au = encode_one(&mut *enc, &Picture::new(640, 480), 0, false).remove(0);
    let sps = sps_of(&au);
    assert_eq!(sps.profile, 66);
    assert_eq!(sps.constraints & 0xc0, 0xc0, "constraint_set0 and 1");
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
    assert!(c.codec_string.starts_with("avc1.42"), "{}", c.codec_string);
    assert_eq!((c.coded, c.display), ((640, 480), (640, 480)));
}

#[test]
fn odd_and_tiny_sizes_pad() {
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
            let side = |v: u32| (v.max(b.min_side()) + 1) & !1;
            let even = (side(w), side(h));
            let c = aus[0].config.clone().unwrap();
            assert_eq!((c.coded, c.display), (even, (w, h)), "{b:?} {w}x{h}");
            assert_eq!(sps_of(&aus[0]).display, even, "{b:?} {w}x{h}");
            let sizes = decode_all(&aus);
            assert_eq!(sizes.len(), 2, "{b:?} {w}x{h}");
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
fn oversized_picture_fails_cleanly() {
    let e = open(&config(Backend::OpenH264, 8192, 4608, 20_000))
        .err()
        .expect("too large");
    assert!(e.0.contains("--stripes"), "{e}");
    assert!(Backend::OpenH264.fits(3840, 2160));
    assert!(Backend::OpenH264.fits(4096, 2304));
    assert!(!Backend::OpenH264.fits(4096, 2320));
}

#[test]
fn largest_picture_encodes() {
    for (w, h) in [(3840, 2160), (4096, 2304)] {
        let mut enc = open(&config(Backend::OpenH264, w, h, 20_000)).unwrap();
        let aus = encode_one(&mut *enc, &Picture::new(w as usize, h as usize), 0, false);
        assert!(aus[0].keyframe, "{w}x{h}");
        assert_eq!(decode_all(&aus), vec![(w as usize, h as usize)]);
    }
}

#[test]
fn threads_slice_the_picture() {
    let mut cfg = config(Backend::OpenH264, 1280, 720, 8000);
    cfg.threads = 4;
    let mut enc = open(&cfg).unwrap();
    let au = encode_one(&mut *enc, &Picture::new(1280, 720), 0, false).remove(0);
    let slices = types(&au).iter().filter(|&&t| t == NAL_IDR).count();
    assert_eq!(slices, 4);
    assert_eq!(decode_all(&[au]), vec![(1280, 720)]);
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
    assert_eq!(Backend::parse("openh264"), Ok(Backend::OpenH264));
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
    assert!(Backend::parse("nvenc")
        .unwrap_err()
        .contains("no encoder yet"));
}

#[test]
fn backends_without_the_codec_refuse_to_open() {
    let mut cfg = config(Backend::OpenH264, 64, 64, 1000);
    cfg.codec = Codec::Hevc;
    assert!(open(&cfg).is_err());
    cfg.backend = Backend::Nvenc;
    cfg.codec = Codec::H264;
    assert!(open(&cfg).is_err());
}

/// What the startup probe finds on this Mac (printed for the PR, `--nocapture`).
#[cfg(target_os = "macos")]
#[test]
fn videotoolbox_probe() {
    use super::caps::{host_providers, Registry, PROBE_TIMEOUT};
    let r = Registry::probe(host_providers(true, false), PROBE_TIMEOUT);
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
                let sps = sps_of(&aus[0]);
                assert_eq!(sps.profile, 100);
                assert_eq!(sps.display, (w as u32, h as u32));
                assert_eq!(sps.color, Some((false, 1, 13, 6)));
                assert_eq!(decode_all(&aus).len(), aus.len());
                assert_eq!(decode_all(&aus[30..]).len(), aus.len() - 30);
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
#[test]
#[ignore]
fn dump_streams() {
    let dir = std::path::PathBuf::from(std::env::var("STREAM_DIR").expect("STREAM_DIR"));
    let (w, h) = (1280usize, 720usize);
    for workload in ["typing", "scrolling", "video"] {
        let mut cfg = config(Backend::OpenH264, w as u32, h as u32, 30_000);
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
/// `cargo test --release cpu_per_stream -- --ignored --nocapture` (BACKEND=x264 with the
/// x264 feature; FRAMES, THREADS). One line per workload, `bench` first.
#[cfg(unix)]
#[test]
#[ignore]
fn cpu_per_stream() {
    let backend = Backend::parse(&std::env::var("BACKEND").unwrap_or("openh264".into())).unwrap();
    let env = |k: &str, d: usize| {
        std::env::var(k)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(d)
    };
    let (frames, threads) = (env("FRAMES", 300), env("THREADS", 1));
    let codec = match std::env::var("CODEC").as_deref() {
        Ok("hevc") => Codec::Hevc,
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

/// An OpenH264 stream decoded as it goes: scrolled text, then whatever the test does.
struct Refining {
    enc: Box<dyn VideoEncoder>,
    dec: ::openh264::decoder::Decoder,
    pic: Picture,
    pts: i64,
    scrolled: usize,
    last_psnr: f64,
}

impl Refining {
    fn config(threads: u32, fps: u32) -> EncoderConfig {
        let mut cfg = config(Backend::OpenH264, 640, 480, 8000);
        cfg.threads = threads;
        cfg.fps = fps;
        cfg.best_quality = 24.0;
        cfg
    }

    fn new(threads: u32, fps: u32) -> Self {
        Self::with(open(&Self::config(threads, fps)).unwrap())
    }

    fn with(enc: Box<dyn VideoEncoder>) -> Self {
        let (w, h) = (640, 480);
        let mut me = Refining {
            enc,
            dec: ::openh264::decoder::Decoder::new().unwrap(),
            pic: Picture::new(w as usize, h as usize),
            pts: 0,
            scrolled: 0,
            last_psnr: 0.0,
        };
        me.step(true, None);
        me
    }

    /// Bytes and whether it was a keyframe; a frame that doesn't come out is 0 bytes.
    fn step(&mut self, force_keyframe: bool, refine: Option<Refine>) -> (usize, bool) {
        use ::openh264::formats::YUVSource;
        self.pts += 33;
        let aus = self
            .enc
            .encode(
                &self.pic.view(),
                EncodeOptions {
                    pts_ms: self.pts,
                    force_keyframe,
                    static_mbs: None,
                    refine,
                },
            )
            .unwrap();
        let (mut n, mut key) = (0, false);
        for au in &aus {
            n += au.data.len();
            key |= au.keyframe;
            if let Some(d) = self.dec.decode(&au.data).unwrap() {
                self.last_psnr = luma_psnr(d.y(), d.strides().0, &self.pic);
            }
        }
        (n, key)
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

/// An odd size opens OpenH264 padded; the refinement worker must still hear when a pass
/// would sharpen, down to the finest rung.
#[test]
fn a_padded_encoder_refines_to_the_last_rung() {
    for (w, h) in [(1920u32, 1080u32), (1921, 1080)] {
        let mut enc = open(&EncoderConfig {
            best_quality: 24.0,
            ..config(Backend::OpenH264, w, h, 8000)
        })
        .unwrap();
        let mut p = Picture::new(w as usize, h as usize);
        encode_one(&mut *enc, &p, 0, true);
        for f in 1..30i64 {
            p.texture(0, 0, 0, w as usize, h as usize, f as usize * 12);
            encode_one(&mut *enc, &p, f * 33, false);
        }
        let mut pts = 30 * 33;
        let rungs: Vec<u8> = [20u8, 16, 12]
            .into_iter()
            .take_while(|&qp| {
                let r = Refine {
                    qp,
                    max_bytes: None,
                };
                if !enc.refinable(r) {
                    return false;
                }
                pts += 33;
                let opts = EncodeOptions {
                    pts_ms: pts,
                    force_keyframe: false,
                    static_mbs: None,
                    refine: Some(r),
                };
                !enc.encode(&p.view(), opts).unwrap().is_empty()
            })
            .collect();
        assert_eq!(rungs, [20, 16, 12], "{w}x{h}");
    }
}

#[test]
fn refining_sharpens_without_an_idr_at_any_thread_count_or_rate() {
    for (threads, fps) in [(1, 30), (4, 30), (1, 120), (4, 120)] {
        let mut s = Refining::new(threads, fps);
        s.scroll(30);
        let motion = s.last_psnr;
        s.enc.set_rate(RateConfig::new(24.0, 3000, fps)).unwrap();
        let mut prev = motion;
        for qp in [20, 16, 12] {
            let (n, key) = s.pass(qp, None);
            assert!(
                n > 0 && !key,
                "t{threads} f{fps} qp {qp}: {n} bytes, key {key}"
            );
            assert!(
                s.last_psnr > prev,
                "t{threads} f{fps} qp {qp}: {} after {prev}",
                s.last_psnr
            );
            prev = s.last_psnr;
        }
        assert!(prev > motion + 3.0, "t{threads} f{fps}: {motion} -> {prev}");
        s.scroll(5);
    }
}

/// The keyframe budget and a stripe's share both go past what the encoder opened with.
#[test]
fn refining_works_above_the_rate_the_encoder_opened_with() {
    let mut s = Refining::new(1, 30);
    s.enc.set_rate(RateConfig::new(24.0, 64_000, 30)).unwrap();
    assert!(s.step(true, None).1);
    let (n, key) = s.pass(20, None);
    assert!(
        n > 0 && !key,
        "right after the boosted keyframe: {n} bytes, key {key}"
    );
    s.enc.set_rate(RateConfig::new(24.0, 40_000, 30)).unwrap();
    s.scroll(5);
    let (n, key) = s.pass(16, None);
    assert!(n > 0 && !key, "at a raised cap: {n} bytes, key {key}");
    s.enc.set_rate(RateConfig::new(24.0, 2_000, 30)).unwrap();
    s.scroll(5);
    let (n, key) = s.pass(12, None);
    assert!(n > 0 && !key, "at a lowered cap: {n} bytes, key {key}");
}

/// The library really refuses the switch: nothing goes out, nothing fails, motion carries
/// on under rate control exactly as if no pass had been asked for, and no later pass tries.
#[test]
fn a_refused_switch_costs_nothing() {
    let mut enc = openh264::OpenH264::new(&Refining::config(1, 30)).unwrap();
    enc.break_switch = true;
    let mut broken = Refining::with(Box::new(enc));
    let mut plain = Refining::new(1, 30);
    assert_eq!(broken.scroll(10), plain.scroll(10));
    let before = broken.last_psnr;
    for qp in [20, 16, 12, 20, 16, 12] {
        assert_eq!(broken.pass(qp, None), (0, false));
    }
    assert_eq!(broken.last_psnr, before);
    assert_eq!(
        broken.scroll(10),
        plain.scroll(10),
        "rate control untouched"
    );
}

#[test]
fn refining_leaves_rate_control_where_it_was() {
    let mut plain = Refining::new(1, 30);
    let mut refined = Refining::new(1, 30);
    plain.scroll(30);
    refined.scroll(30);
    for _ in 0..3 {
        plain.step(false, None);
    }
    for qp in [20, 16, 12] {
        refined.pass(qp, None);
    }
    // The sharper reference costs a little for a moment; what matters is where it settles.
    plain.scroll(10);
    refined.scroll(10);
    let (a, b) = (plain.scroll(110) as f64, refined.scroll(110) as f64);
    assert!((b / a - 1.0).abs() < 0.03, "motion after: {a} vs {b} bytes");
}

#[test]
fn a_refining_pass_with_nothing_to_gain_sends_nothing() {
    let mut s = Refining::new(1, 30);
    s.scroll(5);
    assert!(s.pass(12, None).0 > 0);
    let before = s.last_psnr;
    assert_eq!(s.pass(16, None).0, 0);
    assert_eq!(s.pass(12, None).0, 0);
    assert_eq!(s.last_psnr, before);
}

#[test]
fn a_keyframe_stays_under_rate_control() {
    let mut s = Refining::new(1, 30);
    s.scroll(5);
    let refine = Refine {
        qp: 12,
        max_bytes: None,
    };
    let (plain, key) = s.step(true, None);
    assert!(key);
    let (with, key) = s.step(true, Some(refine));
    assert!(key);
    assert!(with < plain * 2, "IDR at the pass's QP: {with} vs {plain}");
}

/// A reopened encoder's next picture is an IDR even though nobody asked for one.
#[test]
fn a_new_encoders_first_frame_stays_under_rate_control() {
    let p = Picture::new(640, 480);
    let first = |refine| {
        let mut enc = open(&Refining::config(1, 30)).unwrap();
        let opts = EncodeOptions {
            refine,
            ..Default::default()
        };
        let aus = enc.encode(&p.view(), opts).unwrap();
        assert!(aus[0].keyframe);
        aus[0].data.len()
    };
    let refine = Refine {
        qp: 12,
        max_bytes: None,
    };
    assert_eq!(first(Some(refine)), first(None));
}

#[test]
fn a_tight_budget_takes_a_smaller_step() {
    let refine = |qp, max_bytes| Refine {
        qp,
        max_bytes: Some(max_bytes),
    };
    let s = openh264::Sharpness::new(8160, 24);
    assert_eq!(s.pick(refine(20, 200_000)), Some(20));
    assert_eq!(s.pick(refine(20, 90_000)), Some(22));
    assert_eq!(s.pick(refine(20, 41_000)), Some(23));
    assert_eq!(s.pick(refine(20, 40_000)), None);
    assert_eq!(s.pick(refine(24, 200_000)), None);
    // A coarse picture sharpens far further for the same bytes.
    let s = openh264::Sharpness::new(8160, 48);
    assert_eq!(s.pick(refine(20, 50_000)), Some(44));
    let mut s = openh264::Sharpness::new(8160, 24);
    s.off = true;
    assert_eq!(s.pick(refine(20, 200_000)), None);
}

#[test]
fn the_estimate_learns_from_what_frames_cost() {
    let budget = |max_bytes| Refine {
        qp: 12,
        max_bytes: Some(max_bytes),
    };
    let mut s = openh264::Sharpness::new(1000, 24);
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
    let mut s = openh264::Sharpness::new(1000, 24);
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
    let mut s = openh264::Sharpness::new(1000, 12);
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
    let mut s = openh264::Sharpness::new(1000, 12);
    s.moved(24, Some(&[0u8; 10]));
    assert_eq!(s.qp.iter().filter(|&&q| q == 24).count(), 1000);
    s.refined(12, 0);
    assert_eq!(s.pick(budget(1_000_000)), None, "nothing left to sharpen");
}

/// Thousands of rate-control switches against the same motion without them, in a long mix
/// of scrolling, typing and video: `SOAK_FRAMES` frames at each cap, decoded throughout.
#[test]
#[ignore]
fn refine_soak() {
    let frames: usize = std::env::var("SOAK_FRAMES").map_or(60_000, |v| v.parse().unwrap());
    const SEGMENT: usize = 24;
    let window = frames / 4 / SEGMENT * SEGMENT;
    for kbps in [8000, 2000] {
        let run = |ladder: bool| {
            let mut s = Refining::new(1, 30);
            s.enc.set_rate(RateConfig::new(24.0, kbps, 30)).unwrap();
            let (w, h) = (s.pic.w, s.pic.h);
            let (mut bytes, mut passes, mut pass_bytes) = (vec![0usize; 4], 0usize, 0usize);
            for f in 0..window * 4 {
                match (f / SEGMENT) % 4 {
                    0 | 3 => {
                        s.scrolled += 12;
                        s.pic.texture(0, 0, 0, w, h, s.scrolled);
                    }
                    1 => s.pic.texture(f as u32, 40, 40, 200, 56, 0),
                    _ => s.pic.video(f),
                }
                let (n, key) = s.step(false, None);
                assert!(!key, "motion got an IDR at {f}");
                bytes[f / window] += n;
                if ladder && f % SEGMENT == SEGMENT - 1 {
                    for qp in [20, 16, 12] {
                        let max = Some(kbps as usize * 200 / 8);
                        let (n, key) = s.pass(qp, max);
                        assert!(!key, "a pass got an IDR at {f}");
                        passes += (n > 0) as usize;
                        pass_bytes += n;
                    }
                }
            }
            (bytes, passes, pass_bytes)
        };
        let (base, _, _) = run(false);
        let (with, passes, pass_bytes) = run(true);
        let per_s = |b: usize| b as f64 * 8.0 / 1000.0 / (window as f64 / 30.0);
        for q in 0..4 {
            let ratio = with[q] as f64 / base[q] as f64;
            println!(
                "soak {kbps} kbps quarter {q}: motion {:.0} kbps, with switches {:.0} kbps, ratio {ratio:.3}",
                per_s(base[q]),
                per_s(with[q])
            );
            assert!(per_s(with[q]) < kbps as f64 * 1.1, "over the cap");
        }
        println!(
            "soak {kbps} kbps: {passes} fixed-QP frames, {} KB",
            pass_bytes / 1024
        );
        let r: Vec<f64> = (0..4).map(|q| with[q] as f64 / base[q] as f64).collect();
        assert!(passes >= 1000);
        assert!(
            r.iter().all(|x| (x - 1.0).abs() < 0.06),
            "motion bytes off baseline: {r:?}"
        );
    }
}
