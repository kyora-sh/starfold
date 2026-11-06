//! Every bench arm in this build, through `open` like the server does.

use super::*;

fn arm_config(backend: Backend, codec: Codec, w: u32, h: u32, kbps: u32) -> EncoderConfig {
    EncoderConfig {
        codec,
        preset: String::new(),
        ..config(backend, w, h, kbps)
    }
}

fn each_arm(f: impl Fn(&'static str, Backend, Codec)) {
    for &(name, backend, codec) in ARMS {
        f(name, backend, codec);
    }
}

fn family(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "avc1.",
        Codec::Hevc => "hvc1.",
        Codec::Av1 => "av01.",
        #[cfg(feature = "libvpx")]
        Codec::Vp8 => "vp8",
        #[cfg(feature = "libvpx")]
        Codec::Vp9 => "vp09.",
    }
}

fn encode_all(enc: &mut dyn VideoEncoder, p: &Picture, f: i64, key: bool) -> Vec<AccessUnit> {
    encode_one(enc, p, f * 33, key)
}

#[test]
fn names_parse_to_their_arm() {
    each_arm(|name, backend, codec| {
        assert_eq!(
            Selection::parse(name),
            Ok(Selection::Arm(name, backend, codec))
        );
        assert_eq!(Selection::parse(name).unwrap().name(), name);
        assert!(backend.implements(codec));
        assert!(!backend.implements(Codec::H264));
    });
}

#[test]
fn first_picture_is_a_keyframe_with_the_config() {
    each_arm(|name, backend, codec| {
        let mut enc = open(&arm_config(backend, codec, 320, 240, 4000)).unwrap();
        let mut aus = encode_all(&mut *enc, &Picture::new(320, 240), 0, true);
        aus.extend(enc.drain().unwrap());
        let au = aus.first().unwrap_or_else(|| panic!("{name}: no output"));
        assert!(au.keyframe, "{name}");
        let c = au.config.as_ref().unwrap_or_else(|| panic!("{name}"));
        assert_eq!(c.codec, codec, "{name}");
        assert!(c.codec_string.starts_with(family(codec)), "{name}");
        assert_eq!(c.coded, (320, 240), "{name}");
    });
}

#[test]
fn keyframes_come_when_asked_and_only_then() {
    each_arm(|name, backend, codec| {
        let mut enc = open(&arm_config(backend, codec, 320, 240, 4000)).unwrap();
        let mut p = Picture::new(320, 240);
        let mut units = Vec::new();
        for f in 0..30i64 {
            p.texture(f as u32 + 1, 16, 16, 64, 64, 0);
            units.extend(encode_all(&mut *enc, &p, f, f == 0 || f == 20));
        }
        units.extend(enc.drain().unwrap());
        assert_eq!(units.len(), 30, "{name}: one unit per picture");
        let keys: Vec<usize> = units
            .iter()
            .enumerate()
            .filter(|(_, a)| a.keyframe)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(keys, vec![0, 20], "{name}");
    });
}

#[test]
fn odd_and_tiny_sizes_report_the_real_display() {
    each_arm(|name, backend, codec| {
        for (w, h) in [(321u32, 241u32), (33, 17)] {
            let mut enc = open(&arm_config(backend, codec, w, h, 4000)).unwrap();
            let mut aus = encode_all(&mut *enc, &Picture::new(w as usize, h as usize), 0, true);
            aus.extend(enc.drain().unwrap());
            let c = aus[0].config.as_ref().unwrap();
            assert_eq!(c.display, (w, h), "{name}");
        }
    });
}

#[test]
fn bad_pictures_are_errors() {
    each_arm(|name, backend, codec| {
        let mut enc = open(&arm_config(backend, codec, 320, 240, 4000)).unwrap();
        let other = Picture::new(160, 120);
        assert!(
            enc.encode(&other.view(), EncodeOptions::default()).is_err(),
            "{name}: wrong size"
        );
        let p = Picture::new(320, 240);
        let short = FrameView {
            y: &p.y[..p.y.len() - 1],
            ..p.view()
        };
        assert!(
            enc.encode(&short, EncodeOptions::default()).is_err(),
            "{name}: short plane"
        );
    });
}

#[test]
fn a_speed_that_is_not_a_number_is_an_error() {
    each_arm(|name, backend, codec| {
        let c = EncoderConfig {
            preset: "fast".into(),
            ..arm_config(backend, codec, 320, 240, 4000)
        };
        assert!(open(&c).is_err(), "{name}");
    });
}

#[test]
fn the_servers_sample_encode_passes() {
    each_arm(|name, backend, codec| {
        caps::sample_encode(backend, codec).unwrap_or_else(|e| panic!("{name}: {e}"));
    });
}

#[test]
fn a_rate_change_keeps_encoding() {
    each_arm(|name, backend, codec| {
        let mut enc = open(&arm_config(backend, codec, 320, 240, 4000)).unwrap();
        let mut p = Picture::new(320, 240);
        let mut units = encode_all(&mut *enc, &p, 0, true);
        for (f, kbps) in [(1i64, 1000u32), (2, 0), (3, 8000)] {
            enc.set_rate(RateConfig::new(16.0, kbps, 30)).unwrap();
            p.texture(f as u32, 16, 16, 64, 64, 0);
            units.extend(encode_all(&mut *enc, &p, f, false));
        }
        units.extend(enc.drain().unwrap());
        assert_eq!(units.len(), 4, "{name}");
    });
}

/// `bench/vs/source.c`'s video picture as 4:2:0: a pan, grain and moving discs, 30 frames long.
fn film(w: usize, h: usize, grain: u32) -> Vec<Picture> {
    use std::f64::consts::PI;
    const LOOP: usize = 30;
    let mut s = 0x9e3779b9u32;
    let mut next = move || {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        s
    };
    let noise = |n: u32| (n % grain.max(1)) as i32 - grain as i32 / 2;
    (0..LOOP)
        .map(|f| {
            let (pan, ph) = ((f * w / LOOP) as f64, 2.0 * PI * f as f64 / LOOP as f64);
            let mut rgb = vec![[0i32; 3]; w * h];
            for y in 0..h {
                let b =
                    (y as f64 / 61.0 + ph).sin() * 40.0 + (y as f64 / 17.0 - 2.0 * ph).sin() * 12.0;
                for x in 0..w {
                    let t = 2.0 * PI * (x as f64 + pan) / w as f64;
                    let base = 128 + ((3.0 * t).sin() * 50.0 + (11.0 * t).sin() * 20.0 + b) as i32;
                    let n = next();
                    rgb[y * w + x] = [
                        base + noise(n),
                        base / 2 + 40 + noise(n >> 8),
                        255 - base + noise(n >> 16),
                    ];
                }
            }
            for k in 0..8usize {
                let t = ph + k as f64 * 0.8;
                let cx = w as f64 / 2.0 + (t * (1 + k % 2) as f64).cos() * w as f64 * 0.38;
                let cy = h as f64 / 2.0 + t.sin() * h as f64 * 0.36;
                let rad = 60.0 + k as f64 * 14.0;
                for y in 0..h {
                    for x in 0..w {
                        if (x as f64 - cx).powi(2) + (y as f64 - cy).powi(2) < rad * rad {
                            let blue = (next() & 31) as i32 + 40;
                            rgb[y * w + x] = [k as i32 * 31, 255 - k as i32 * 25, blue];
                        }
                    }
                }
            }
            i420(&mut rgb, w, h)
        })
        .collect()
}

/// RGB to BT.601 limited-range 4:2:0, chroma averaged over each 2x2 block.
fn i420(rgb: &mut [[i32; 3]], w: usize, h: usize) -> Picture {
    let mut p = Picture::new(w, h);
    let cw = w.div_ceil(2);
    for (i, c) in rgb.iter_mut().enumerate() {
        c.iter_mut().for_each(|v| *v = (*v).clamp(0, 255));
        p.y[i] = (16 + ((66 * c[0] + 129 * c[1] + 25 * c[2] + 128) >> 8)) as u8;
    }
    for (i, (u, v)) in p.u.iter_mut().zip(p.v.iter_mut()).enumerate() {
        let (x, y) = (i % cw * 2, i / cw * 2);
        let mut sum = [0i32; 3];
        for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            let c = rgb[(y + dy).min(h - 1) * w + (x + dx).min(w - 1)];
            sum.iter_mut().zip(c).for_each(|(a, b)| *a += b);
        }
        let [r, g, b] = sum.map(|v| v / 4);
        *u = (128 + ((-38 * r - 74 * g + 112 * b + 128) >> 8)) as u8;
        *v = (128 + ((112 * r - 94 * g - 18 * b + 128) >> 8)) as u8;
    }
    p
}

/// `bench/scroll.html` as Chrome renders it: raw BGRX, `w` wide (`bench/vs/setup.sh`'s page).
fn page(path: &str, w: usize) -> Picture {
    let raw = std::fs::read(path).unwrap_or_else(|e| panic!("PAGE {path}: {e}"));
    assert!(
        raw.len().is_multiple_of(w * 4),
        "PAGE {path} is not {w} BGRX pixels wide"
    );
    let h = (raw.len() / (w * 4)) & !1;
    let mut rgb: Vec<[i32; 3]> = raw[..w * h * 4]
        .chunks_exact(4)
        .map(|px| [px[2] as i32, px[1] as i32, px[0] as i32])
        .collect();
    i420(&mut rgb, w, h)
}

/// Rows [y0, y0 + rows) and columns [x0, x0 + cols) of `from`, at (x, y) in `to`; even origins.
#[allow(clippy::too_many_arguments)]
fn blit(
    to: &mut Picture,
    x: usize,
    y: usize,
    from: &Picture,
    x0: usize,
    y0: usize,
    cols: usize,
    rows: usize,
) {
    for r in 0..rows {
        let (d, s) = ((y + r) * to.w + x, (y0 + r) * from.w + x0);
        to.y[d..d + cols].copy_from_slice(&from.y[s..s + cols]);
    }
    let (tcw, fcw, cc) = (to.w.div_ceil(2), from.w.div_ceil(2), cols.div_ceil(2));
    for r in 0..rows.div_ceil(2) {
        let (d, s) = ((y / 2 + r) * tcw + x / 2, (y0 / 2 + r) * fcw + x0 / 2);
        to.u[d..d + cc].copy_from_slice(&from.u[s..s + cc]);
        to.v[d..d + cc].copy_from_slice(&from.v[s..s + cc]);
    }
}

/// The 22-row band below the first screen with the most ink: where typed glyphs come from.
fn inkiest_line(page: &Picture, h: usize) -> usize {
    (h..page.h - 22)
        .step_by(2)
        .max_by_key(|&y| {
            (y..y + 22)
                .map(|r| {
                    page.y[r * page.w + 80..r * page.w + page.w - 100]
                        .iter()
                        .filter(|&&v| v < 128)
                        .count()
                })
                .sum::<usize>()
        })
        .unwrap()
}

/// One arm through typing, scrolling, settled and video, for `bench/arms/run.sh`: ARM, PRESET,
/// KBPS, CRF, FRAMES, THREADS, WORKLOAD, GRAIN, PAGE; DUMP=<dir> writes the bitstream, the source
/// and `<arm>-<workload>.units` (source frame, bytes, keyframe per unit) for ffmpeg to score. One
/// `bench` line per workload. With PAGE (Chrome's render of `bench/scroll.html`), the text
/// workloads use its anti-aliased text as the bench source does: scrolling 20 rows a frame, typing
/// a 10x22 cell of a real text line every third frame; without it, two-level synthetic text.
/// `settled` scrolls for two seconds, then holds still.
#[cfg(unix)]
#[test]
#[ignore]
fn arm_bench() {
    use std::io::Write;
    let var = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    let arm = var("ARM", "libvpx-vp9");
    let num = |k: &str, d: usize| var(k, "").parse().unwrap_or(d);
    let (frames, threads, kbps) = (num("FRAMES", 300), num("THREADS", 4), num("KBPS", 8000));
    let crf: f32 = var("CRF", "24").parse().unwrap();
    let (backend, codec) = match Selection::parse(&arm).unwrap() {
        Selection::Arm(_, b, c) => (b, c),
        Selection::Pinned(b) => (b, Codec::H264),
        Selection::Auto => panic!("name an arm"),
    };
    let (w, h, fps) = (1920usize, 1080usize, 30u32);
    let cpu = || unsafe {
        let mut r: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut r);
        (r.ru_utime.tv_sec + r.ru_stime.tv_sec) as f64
            + (r.ru_utime.tv_usec + r.ru_stime.tv_usec) as f64 / 1e6
    };
    let workloads = var("WORKLOAD", "typing,scrolling,settled,video");
    let real = std::env::var("PAGE").ok().map(|p| page(&p, 1920));
    let frame_of = |pts: i64| (pts as f64 * fps as f64 / 1000.0).round() as u64;
    for workload in workloads.split(',') {
        let mut enc = open(&EncoderConfig {
            fps,
            threads: threads as u32,
            preset: var("PRESET", ""),
            best_quality: crf,
            rate: RateConfig::new(crf, kbps as u32, fps),
            ..arm_config(backend, codec, w as u32, h as u32, kbps as u32)
        })
        .unwrap();
        let dump = std::env::var("DUMP").ok();
        let mut stream = dump.as_ref().map(|d| {
            let f = std::fs::File::create(format!("{d}/{arm}-{workload}.bin")).unwrap();
            std::io::BufWriter::new(f)
        });
        let mut source = dump.as_ref().map(|d| {
            let f = std::fs::File::create(format!("{d}/{arm}-{workload}.yuv")).unwrap();
            std::io::BufWriter::new(f)
        });
        let mut index = dump.as_ref().map(|d| {
            let f = std::fs::File::create(format!("{d}/{arm}-{workload}.units")).unwrap();
            std::io::BufWriter::new(f)
        });
        if let Some(s) = stream.as_mut() {
            ivf_header(s, codec, w as u16, h as u16);
        }
        let mut p = Picture::new(w, h);
        let reel = (workload == "video").then(|| film(w, h, num("GRAIN", 4) as u32));
        let typed_from = real.as_ref().map(|pg| inkiest_line(pg, h));
        let mut blank = Picture::new(w, 24);
        blank.y.fill(235);
        if let Some(pg) = &real {
            blit(&mut p, 0, 0, pg, 0, 0, w, h);
            if workload == "typing" {
                blit(&mut p, 0, 72, &blank, 0, 0, w, 24);
            }
        }
        let (mut cx, mut cy) = (80usize, 74usize);
        let mut codec_string = String::from("none");
        let (mut bytes, mut total, mut lat, mut cpu_s, mut units) =
            (0usize, 0usize, Vec::new(), 0.0, 0usize);
        for f in 0..frames {
            let scrolls =
                workload == "scrolling" || (workload == "settled" && f < 2 * fps as usize);
            match (workload, &real) {
                ("typing", Some(pg)) if f % 3 == 0 => {
                    blit(&mut p, cx, cy, pg, cx, typed_from.unwrap(), 10, 22);
                    cx += 10;
                    if cx > w - 100 {
                        (cx, cy) = (80, if cy + 22 > h - 60 { 74 } else { cy + 22 });
                        blit(&mut p, 0, cy, &blank, 0, 0, w, 22);
                    }
                }
                ("typing", Some(_)) => {}
                ("typing", None) => {
                    let (x, y) = (200 + (f % 120) * 12, 300 + (f / 120) * 22);
                    p.texture(f as u32 + 1, x, y, x + 12, y + 22, 0);
                }
                ("scrolling" | "settled", Some(pg)) if scrolls => {
                    blit(&mut p, 0, 0, pg, 0, ((f + 1) * 20 % (pg.h - h)) & !1, w, h)
                }
                ("scrolling" | "settled", None) if scrolls => {
                    p.texture(0, 300, 90, 1500, 990, f * 8)
                }
                ("settled", _) => {}
                ("video", _) => {}
                (other, _) => panic!("no workload {other}"),
            }
            let p = reel.as_ref().map_or(&p, |r| &r[f % r.len()]);
            if let Some(s) = source.as_mut() {
                s.write_all(&p.y).unwrap();
                s.write_all(&p.u).unwrap();
                s.write_all(&p.v).unwrap();
            }
            let (c0, t0) = (cpu(), std::time::Instant::now());
            let aus = encode_one(&mut *enc, p, f as i64 * 1000 / fps as i64, f == 0);
            let (dt, dc) = (t0.elapsed().as_secs_f64() * 1e3, cpu() - c0);
            for a in &aus {
                if let Some(c) = &a.config {
                    codec_string.clone_from(&c.codec_string);
                }
                total += a.data.len();
                if let Some(s) = stream.as_mut() {
                    ivf_frame(s, codec, units as u64, &a.data);
                }
                if let Some(s) = index.as_mut() {
                    writeln!(
                        s,
                        "{} {} {}",
                        frame_of(a.pts_ms),
                        a.data.len(),
                        a.keyframe as u8
                    )
                    .unwrap();
                }
                units += 1;
            }
            if f >= fps as usize {
                lat.push(dt);
                cpu_s += dc;
                bytes += aus.iter().map(|a| a.data.len()).sum::<usize>();
            }
        }
        for a in enc.drain().unwrap() {
            total += a.data.len();
            if let Some(s) = stream.as_mut() {
                ivf_frame(s, codec, units as u64, &a.data);
            }
            if let Some(s) = index.as_mut() {
                writeln!(
                    s,
                    "{} {} {}",
                    frame_of(a.pts_ms),
                    a.data.len(),
                    a.keyframe as u8
                )
                .unwrap();
            }
            units += 1;
        }
        lat.sort_by(|a, b| a.total_cmp(b));
        let secs = lat.len() as f64 / fps as f64;
        let pct = |q: f64| lat[((lat.len() - 1) as f64 * q).round() as usize];
        println!(
            "bench arm={arm} preset={} crf={crf} target_kbps={kbps} workload={workload} text={} codec={codec_string} threads={threads} frames={frames} units={units} cpu_s_per_s={:.4} kbps={:.0} total_kbps={:.0} p50_ms={:.2} p95_ms={:.2}",
            var("PRESET", "default"),
            if real.is_some() { "page" } else { "synthetic" },
            cpu_s / secs,
            bytes as f64 * 8.0 / secs / 1000.0,
            total as f64 * 8.0 * fps as f64 / frames as f64 / 1000.0,
            pct(0.5),
            pct(0.95)
        );
    }
}

/// VP8, VP9 and AV1 go in IVF for ffmpeg; H.264 and HEVC are self-delimiting.
fn ivf_fourcc(codec: Codec) -> Option<&'static [u8; 4]> {
    match codec {
        #[cfg(feature = "libvpx")]
        Codec::Vp8 => Some(b"VP80"),
        #[cfg(feature = "libvpx")]
        Codec::Vp9 => Some(b"VP90"),
        Codec::Av1 => Some(b"AV01"),
        _ => None,
    }
}

fn ivf_header(s: &mut impl std::io::Write, codec: Codec, w: u16, h: u16) {
    let Some(fourcc) = ivf_fourcc(codec) else {
        return;
    };
    s.write_all(b"DKIF\0\0\x20\0").unwrap();
    s.write_all(fourcc).unwrap();
    s.write_all(&w.to_le_bytes()).unwrap();
    s.write_all(&h.to_le_bytes()).unwrap();
    s.write_all(&30u32.to_le_bytes()).unwrap();
    s.write_all(&1u32.to_le_bytes()).unwrap();
    s.write_all(&0u64.to_le_bytes()).unwrap();
}

fn ivf_frame(s: &mut impl std::io::Write, codec: Codec, pts: u64, data: &[u8]) {
    if ivf_fourcc(codec).is_some() {
        s.write_all(&(data.len() as u32).to_le_bytes()).unwrap();
        s.write_all(&pts.to_le_bytes()).unwrap();
    }
    s.write_all(data).unwrap();
}

#[cfg(feature = "kvazaar")]
#[test]
fn a_kvazaar_rate_change_reopens_with_a_keyframe() {
    let mut enc = open(&arm_config(Backend::Kvazaar, Codec::Hevc, 320, 240, 4000)).unwrap();
    let mut p = Picture::new(320, 240);
    let mut units = encode_all(&mut *enc, &p, 0, true);
    p.texture(1, 16, 16, 64, 64, 0);
    units.extend(encode_all(&mut *enc, &p, 1, false));
    enc.set_rate(RateConfig::new(16.0, 4000, 30)).unwrap();
    p.texture(2, 16, 16, 64, 64, 0);
    units.extend(encode_all(&mut *enc, &p, 2, false));
    enc.set_rate(RateConfig::new(16.0, 1000, 30)).unwrap();
    p.texture(3, 16, 16, 64, 64, 0);
    units.extend(encode_all(&mut *enc, &p, 3, false));
    units.extend(enc.drain().unwrap());
    let keys: Vec<bool> = units.iter().map(|a| a.keyframe).collect();
    assert_eq!(keys, vec![true, false, false, true]);
}

#[cfg(feature = "kvazaar")]
#[test]
fn a_kvazaar_rate_change_before_the_first_picture_applies() {
    let mut enc = open(&arm_config(Backend::Kvazaar, Codec::Hevc, 320, 240, 4000)).unwrap();
    enc.set_rate(RateConfig::new(16.0, 1000, 30)).unwrap();
    let mut p = Picture::new(320, 240);
    let mut units = encode_all(&mut *enc, &p, 0, true);
    p.texture(1, 16, 16, 64, 64, 0);
    units.extend(encode_all(&mut *enc, &p, 1, false));
    units.extend(enc.drain().unwrap());
    let keys: Vec<bool> = units.iter().map(|a| a.keyframe).collect();
    assert_eq!(keys, vec![true, false]);
}

#[cfg(feature = "svt-av1")]
#[test]
fn an_svt_preset_out_of_range_is_an_error() {
    for preset in ["268", "14", "-3"] {
        let c = EncoderConfig {
            preset: preset.into(),
            ..arm_config(Backend::SvtAv1, Codec::Av1, 320, 240, 4000)
        };
        assert!(open(&c).is_err(), "{preset}");
    }
}
