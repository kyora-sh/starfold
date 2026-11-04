use super::*;

/// A provider that reports what it was given, after `delay`.
struct Fake {
    backend: Backend,
    caps: Vec<EncoderCapability>,
    diagnostics: Vec<(Option<&'static str>, &'static str)>,
    delay: Duration,
}

impl Fake {
    fn new(backend: Backend) -> Self {
        Fake {
            backend,
            caps: Vec::new(),
            diagnostics: Vec::new(),
            delay: Duration::ZERO,
        }
    }
}

impl ProbeProvider for Fake {
    fn backend(&self) -> Backend {
        self.backend
    }

    fn probe(&self) -> Probe {
        std::thread::sleep(self.delay);
        let mut p = Probe {
            caps: self.caps.clone(),
            diagnostics: Vec::new(),
        };
        for (d, r) in &self.diagnostics {
            p.note(self.backend, *d, *r);
        }
        p
    }
}

/// A hardware encoder that did everything right.
fn good(codec: Codec, backend: Backend) -> EncoderCapability {
    let mut c = EncoderCapability::new(codec, backend, Acceleration::Hardware, "gpu0");
    c.profiles = vec![ProfileCapability::new("main", 8, Chroma::Yuv420)];
    c.low_latency = Some(true);
    c.keyframe_control = Some(true);
    c.status = ProbeStatus::Verified;
    c
}

fn registry(fakes: Vec<Fake>) -> Registry {
    let mut v: Vec<Box<dyn ProbeProvider>> = vec![Box::new(Software(Backend::OpenH264))];
    v.extend(
        fakes
            .into_iter()
            .map(|f| Box::new(f) as Box<dyn ProbeProvider>),
    );
    Registry::probe(v, Duration::from_secs(5))
}

fn openh264_left(r: &Registry) {
    let c = r.check(Backend::OpenH264, Codec::H264).expect("OpenH264");
    assert_eq!(c.acceleration, Acceleration::Software);
    assert!(c.eligible());
}

#[test]
fn openh264_is_verified_by_a_real_encode() {
    let r = registry(Vec::new());
    openh264_left(&r);
    assert_eq!(r.eligible(Codec::H264).count(), 1);
    assert_eq!(r.eligible(Codec::Hevc).count(), 0);
    assert!(r.diagnostics.is_empty());
}

#[test]
fn missing_library_is_a_diagnostic() {
    let mut f = Fake::new(Backend::Vaapi);
    f.diagnostics = vec![(None, "libva.so.2: cannot open shared object file")];
    let r = registry(vec![f]);
    openh264_left(&r);
    assert!(r.encoders.iter().all(|c| c.backend != Backend::Vaapi));
    let e = r.check(Backend::Vaapi, Codec::H264).unwrap_err();
    assert!(e.contains("libva.so.2"), "{e}");
}

#[test]
fn denied_device_skips_only_that_device() {
    let mut f = Fake::new(Backend::Vaapi);
    f.diagnostics = vec![(Some("/dev/dri/renderD128"), "can't open: Permission denied")];
    let mut c = good(Codec::Hevc, Backend::Vaapi);
    c.device_id = "/dev/dri/renderD129".into();
    f.caps = vec![c];
    let r = registry(vec![f]);
    openh264_left(&r);
    assert_eq!(r.diagnostics.len(), 1);
    assert_eq!(
        r.diagnostics[0].device.as_deref(),
        Some("/dev/dri/renderD128")
    );
    let found: Vec<_> = r
        .encoders
        .iter()
        .filter(|c| c.backend == Backend::Vaapi)
        .collect();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].device_id, "/dev/dri/renderD129");
}

#[test]
fn unimplemented_backends_are_never_eligible() {
    let mut f = Fake::new(Backend::Nvenc);
    f.caps = vec![
        good(Codec::Av1, Backend::Nvenc),
        good(Codec::H264, Backend::Nvenc),
    ];
    let r = registry(vec![f]);
    for c in r.encoders.iter().filter(|c| c.backend == Backend::Nvenc) {
        assert!(!c.implemented && !c.eligible(), "{c:?}");
    }
    assert_eq!(r.eligible(Codec::Av1).count(), 0);
    let e = r.check(Backend::Nvenc, Codec::H264).unwrap_err();
    assert!(e.contains("no encoder in this build"), "{e}");
}

#[test]
fn a_probe_cannot_claim_an_implementation() {
    let mut c = good(Codec::Av1, Backend::VideoToolbox);
    c.implemented = true;
    let mut f = Fake::new(Backend::VideoToolbox);
    f.caps = vec![c];
    let r = registry(vec![f]);
    assert!(!r
        .encoders
        .iter()
        .any(|c| c.codec == Codec::Av1 && c.implemented));
}

#[test]
fn eligibility_needs_every_property_known() {
    let base = {
        let mut c = good(Codec::H264, Backend::OpenH264);
        c.implemented = true;
        c
    };
    assert!(base.eligible());
    type Case = (&'static str, fn(&mut EncoderCapability));
    let cases: [Case; 6] = [
        ("session failure", |c| {
            c.status = ProbeStatus::Unavailable("session failed".into())
        }),
        ("only listed", |c| c.status = ProbeStatus::Discovered),
        ("decode-only or 10-bit", |c| {
            c.status = ProbeStatus::ProbeOnly
        }),
        ("unknown low latency", |c| c.low_latency = None),
        ("no keyframe control", |c| c.keyframe_control = Some(false)),
        ("10-bit only", |c| {
            c.profiles = vec![ProfileCapability::new("main10", 10, Chroma::Yuv420)]
        }),
    ];
    for (what, f) in cases {
        let mut c = base.clone();
        f(&mut c);
        assert!(!c.eligible(), "{what}");
    }
}

#[test]
fn session_failure_keeps_the_diagnosis() {
    let mut c = good(Codec::Hevc, Backend::VideoToolbox);
    c.status = ProbeStatus::Unavailable("VTCompressionSessionCreate failed (-12902)".into());
    let mut f = Fake::new(Backend::VideoToolbox);
    f.caps = vec![c];
    let r = registry(vec![f]);
    openh264_left(&r);
    let e = r.check(Backend::VideoToolbox, Codec::Hevc).unwrap_err();
    assert!(e.contains("-12902"), "{e}");
}

#[test]
fn unknown_limits_stay_unknown() {
    let l = EncoderLimits::default();
    assert_eq!(l.admits(1920, 1080, 30), None);
    assert_eq!(l.admits(16384, 16384, 240), None);
    let some = EncoderLimits {
        max_width: Some(4096),
        max_height: Some(4096),
        ..Default::default()
    };
    assert_eq!(some.admits(8192, 1080, 30), Some(false));
    assert_eq!(
        some.admits(1920, 1080, 30),
        None,
        "the pixel rate is unknown"
    );
}

#[test]
fn size_limits_are_not_independent() {
    let l = EncoderLimits {
        min_width: Some(144),
        max_width: Some(4096),
        min_height: Some(144),
        max_height: Some(4096),
        max_mbs: Some(36864),
        max_mbs_per_sec: Some(36864 * 30),
        ..Default::default()
    };
    assert_eq!(l.admits(4096, 2304, 30), Some(true));
    assert_eq!(
        l.admits(4096, 4096, 30),
        Some(false),
        "both sides in range, too many MBs"
    );
    assert_eq!(
        l.admits(4096, 2304, 60),
        Some(false),
        "fits, but not at 60 fps"
    );
    assert_eq!(l.admits(100, 1080, 30), Some(false));
}

#[test]
fn hung_probes_become_diagnostics() {
    let mut slow = Fake::new(Backend::Nvenc);
    slow.delay = Duration::from_secs(3);
    let v: Vec<Box<dyn ProbeProvider>> =
        vec![Box::new(Software(Backend::OpenH264)), Box::new(slow)];
    let t = Instant::now();
    let r = Registry::probe(v, Duration::from_millis(500));
    assert!(t.elapsed() < Duration::from_secs(2));
    openh264_left(&r);
    assert_eq!(r.diagnostics.len(), 1);
    assert_eq!(r.diagnostics[0].backend, Backend::Nvenc);
    assert!(
        r.diagnostics[0].reason.contains("did not finish"),
        "{:?}",
        r.diagnostics
    );
}

#[test]
fn software_only_probes_load_no_hardware_backend() {
    let backends: Vec<Backend> = host_providers(false).iter().map(|p| p.backend()).collect();
    assert!(
        backends
            .iter()
            .all(|b| b.name() == "openh264" || b.name() == "x264"),
        "{backends:?}"
    );
}

#[test]
fn stats_json_shape() {
    let mut f = Fake::new(Backend::Vaapi);
    f.diagnostics = vec![(Some("/dev/dri/renderD128"), "can't open")];
    let r = registry(vec![f]);
    let v = serde_json::to_value(&r).unwrap();
    let e = &v["encoders"][0];
    assert_eq!(e["codec"], "h264");
    assert_eq!(e["backend"], "openh264");
    assert_eq!(e["acceleration"], "software");
    assert_eq!(e["status"]["state"], "verified");
    assert_eq!(e["implemented"], true);
    assert_eq!(e["profiles"][0]["chroma"], "4:2:0");
    assert!(e["limits"]["max_mbs_per_sec"].is_null());
    assert_eq!(v["diagnostics"][0]["backend"], "vaapi");
    let mut c = good(Codec::Hevc, Backend::Vaapi);
    c.status = ProbeStatus::Unavailable("busy".into());
    let s = serde_json::to_value(&c).unwrap();
    assert_eq!(
        s["status"],
        serde_json::json!({"state": "unavailable", "reason": "busy"})
    );
}

#[test]
fn hevc_codec_string() {
    use super::super::hevc;
    // Main, level 3.1, progressive and frame-only; an emulation-prevention byte inside.
    let sps = [
        0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0xb0, 0x00, 0x00, 0x03, 0x00, 0x00,
        0x03, 0x00, 0x5d, 0xa0,
    ];
    assert_eq!(hevc::nal_type(&sps), hevc::NAL_SPS);
    assert_eq!(hevc::codec_string(&sps).as_deref(), Some("hvc1.1.6.L93.B0"));
    let mut high = sps;
    high[3] = 0x22;
    high[4] = 0x20;
    assert_eq!(
        hevc::codec_string(&high).as_deref(),
        Some("hvc1.2.4.H93.B0")
    );
    assert_eq!(hevc::codec_string(&sps[..8]), None);
    assert_eq!(hevc::codec_string(&[0x40, 0x01, 0, 0]), None, "a VPS");
}

#[cfg(target_os = "linux")]
#[test]
fn vaapi_encode_entrypoints_only() {
    use super::super::vaapi::{caps, Entrypoint};
    let ep = |profile, entrypoint, rt_format| Entrypoint {
        profile,
        entrypoint,
        rt_format,
        max_width: Some(4096),
        max_height: Some(4096),
    };
    let got = caps(
        "/dev/dri/renderD128",
        &[
            // H.264 High: decode (VLD) only.
            ep(7, 1, Some(1)),
            // HEVC Main: decode and low-power encode; Main10: encode.
            ep(17, 1, Some(1)),
            ep(17, 8, Some(1)),
            ep(18, 8, Some(0x100)),
            // AV1 profile 0: 8- and 10-bit encode, limits unknown.
            Entrypoint {
                max_width: None,
                max_height: None,
                ..ep(32, 6, Some(0x101))
            },
            // HEVC 4:4:4 alone would be probe-only.
            ep(26, 6, Some(4)),
        ],
    );
    assert!(
        got.iter().all(|c| c.codec != Codec::H264),
        "decode-only is no encoder"
    );
    let hevc = got.iter().find(|c| c.codec == Codec::Hevc).unwrap();
    assert_eq!(hevc.status, ProbeStatus::Discovered);
    let names: Vec<_> = hevc.profiles.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["main", "main10", "main444"]);
    assert_eq!(hevc.limits.max_width, Some(4096));
    assert_eq!(
        hevc.limits.pixel_formats,
        Some(vec![PixelFormat::Nv12, PixelFormat::P010])
    );
    let av1 = got.iter().find(|c| c.codec == Codec::Av1).unwrap();
    assert_eq!(av1.profiles.len(), 2);
    assert_eq!(av1.limits.max_width, None);
    let av1_10 = caps("/dev/dri/renderD129", &[ep(32, 6, Some(0x100))]);
    assert_eq!(
        av1_10[0].profiles,
        [ProfileCapability::new("main", 10, Chroma::Yuv420)]
    );
    assert_eq!(av1_10[0].status, ProbeStatus::ProbeOnly, "10-bit only");
    let only444 = caps("/dev/dri/renderD129", &[ep(26, 6, Some(4))]);
    assert_eq!(only444[0].status, ProbeStatus::ProbeOnly);
    assert_eq!(only444[0].limits.pixel_formats, None);
    let r = registry(vec![{
        let mut f = Fake::new(Backend::Vaapi);
        f.caps = got;
        f
    }]);
    assert_eq!(
        r.eligible(Codec::Hevc).count(),
        0,
        "discovered, not implemented"
    );
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn nvenc_codecs_profiles_and_limits() {
    use super::super::nvenc::{caps, CodecReport, Guid, Limits, CODEC_AV1, CODEC_H264, PROFILES};
    let high = PROFILES[2].0;
    let got = caps(
        "cuda:0 test",
        &[
            CodecReport {
                codec: CODEC_H264,
                profiles: vec![high, Guid::default()],
                formats: vec![0x1, 0x10000],
                limits: Limits {
                    width: (Some(145), Some(4096)),
                    height: (Some(49), Some(4096)),
                    mbs: Some(65536),
                    mbs_per_sec: Some(983040),
                    ten_bit: Some(0),
                },
            },
            CodecReport {
                codec: CODEC_AV1,
                profiles: vec![PROFILES[9].0],
                formats: vec![0x10000],
                limits: Limits {
                    ten_bit: Some(1),
                    ..Default::default()
                },
            },
            CodecReport {
                codec: Guid::default(),
                profiles: Vec::new(),
                formats: Vec::new(),
                limits: Limits::default(),
            },
        ],
    );
    assert_eq!(got.len(), 2, "unknown codec GUIDs are skipped");
    let h = &got[0];
    assert_eq!((h.codec, &h.profiles[0].name[..]), (Codec::H264, "high"));
    assert_eq!(h.profiles.len(), 1);
    assert_eq!(h.status, ProbeStatus::Discovered);
    assert_eq!(
        (h.limits.min_width, h.limits.max_mbs_per_sec),
        (Some(145), Some(983040))
    );
    assert_eq!(
        h.limits.pixel_formats,
        Some(vec![PixelFormat::Nv12, PixelFormat::P010])
    );
    let a = &got[1];
    assert_eq!(a.profiles.len(), 2);
    assert_eq!(a.status, ProbeStatus::ProbeOnly, "10-bit input only");
    assert_eq!(a.limits.max_width, None);
}
