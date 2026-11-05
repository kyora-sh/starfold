//! Every bench arm in this build, through `open` like the server does.

use super::*;

fn arm_config(backend: Backend, codec: Codec, w: u32, h: u32, kbps: u32) -> EncoderConfig {
    EncoderConfig {
        codec,
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

/// One arm through typing, scrolling and video, for `bench/arms/run.sh`: ARM, PRESET, KBPS,
/// CRF, FRAMES, THREADS, WORKLOAD; DUMP=<dir> writes the bitstream and the source for
/// ffmpeg to score. One `bench` line per workload.
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
    let workloads = var("WORKLOAD", "typing,scrolling,video");
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
        if let Some(s) = stream.as_mut() {
            ivf_header(s, codec, w as u16, h as u16);
        }
        let mut p = Picture::new(w, h);
        let (mut bytes, mut lat, mut cpu_s, mut units) = (0usize, Vec::new(), 0.0, 0usize);
        for f in 0..frames {
            match workload {
                "typing" => {
                    let (x, y) = (200 + (f % 120) * 12, 300 + (f / 120) * 22);
                    p.texture(f as u32 + 1, x, y, x + 12, y + 22, 0);
                }
                "scrolling" => p.texture(0, 300, 90, 1500, 990, f * 8),
                _ => p.video(f),
            }
            if let Some(s) = source.as_mut() {
                s.write_all(&p.y).unwrap();
                s.write_all(&p.u).unwrap();
                s.write_all(&p.v).unwrap();
            }
            let (c0, t0) = (cpu(), std::time::Instant::now());
            let aus = encode_one(&mut *enc, &p, f as i64 * 1000 / fps as i64, f == 0);
            let (dt, dc) = (t0.elapsed().as_secs_f64() * 1e3, cpu() - c0);
            for a in &aus {
                if let Some(s) = stream.as_mut() {
                    ivf_frame(s, codec, units as u64, &a.data);
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
            if let Some(s) = stream.as_mut() {
                ivf_frame(s, codec, units as u64, &a.data);
            }
            units += 1;
        }
        lat.sort_by(|a, b| a.total_cmp(b));
        let secs = lat.len() as f64 / fps as f64;
        let pct = |q: f64| lat[((lat.len() - 1) as f64 * q).round() as usize];
        println!(
            "bench arm={arm} preset={} crf={crf} target_kbps={kbps} workload={workload} threads={threads} frames={frames} units={units} cpu_s_per_s={:.4} kbps={:.0} p50_ms={:.2} p95_ms={:.2}",
            var("PRESET", "default"),
            cpu_s / secs,
            bytes as f64 * 8.0 / secs / 1000.0,
            pct(0.5),
            pct(0.95)
        );
    }
}

/// VP8 and VP9 need a container for ffmpeg; the others are self-delimiting.
fn ivf_fourcc(codec: Codec) -> Option<&'static [u8; 4]> {
    match codec {
        #[cfg(feature = "libvpx")]
        Codec::Vp8 => Some(b"VP80"),
        #[cfg(feature = "libvpx")]
        Codec::Vp9 => Some(b"VP90"),
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
