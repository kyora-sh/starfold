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
        let mut enc = open(&config(Backend::OpenH264, w as u32, h as u32, 30_000)).unwrap();
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
        cfg.fps = fps;
        cfg.threads = threads as u32;
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
            "bench backend={} workload={workload} threads={threads} cpu_s_per_s={:.4} kbps={:.0} p50_ms={:.2} p95_ms={:.2}",
            backend.name(),
            cpu_s / secs,
            bytes as f64 * 8.0 / secs / 1000.0,
            pct(0.5),
            pct(0.95)
        );
    }
}
