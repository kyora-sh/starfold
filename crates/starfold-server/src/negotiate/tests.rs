use super::*;
use crate::encoder::caps::{Chroma, EncoderCapability, ProbeStatus, ProfileCapability};

fn cap(codec: Codec, backend: Backend, hw: bool) -> EncoderCapability {
    let mut c = EncoderCapability::new(
        codec,
        backend,
        if hw {
            Acceleration::Hardware
        } else {
            Acceleration::Software
        },
        "dev",
    );
    c.profiles = vec![ProfileCapability::new("main", 8, Chroma::Yuv420)];
    c.low_latency = Some(true);
    c.keyframe_control = Some(true);
    c.implemented = true;
    c.status = ProbeStatus::Verified;
    c
}

fn reg(caps: Vec<EncoderCapability>) -> Registry {
    Registry {
        encoders: caps,
        diagnostics: Vec::new(),
    }
}

const AV1: Candidate = Candidate {
    codec: Codec::Av1,
    backend: Backend::Nvenc,
    hardware: true,
};
const HEVC: Candidate = Candidate {
    codec: Codec::Hevc,
    backend: Backend::VideoToolbox,
    hardware: true,
};
const H264: Candidate = Candidate {
    codec: Codec::H264,
    backend: Backend::OpenH264,
    hardware: false,
};

fn offer(cand: Candidate, codec: &str, stripes: &[(u32, u32)], w: u32) -> Offer {
    Offer {
        codec: cand.codec,
        format: if cand.codec == Codec::Av1 {
            "av1-low-overhead"
        } else {
            "annexb"
        },
        stripes: stripes
            .iter()
            .enumerate()
            .map(|(i, &(y, h))| StripeOffer {
                i,
                y,
                h,
                config: DecoderConfig {
                    codec: codec.into(),
                    coded_width: w,
                    coded_height: h,
                },
            })
            .collect(),
        cand,
    }
}

fn offers() -> Vec<Offer> {
    vec![
        offer(AV1, "av01.0.13M.08", &[(0, 1600)], 2560),
        offer(HEVC, "hvc1.1.6.L153.B0", &[(0, 1600)], 2560),
        offer(H264, "avc1.42c034", &[(0, 800), (800, 800)], 2560),
    ]
}

fn wc(codec: &str, w: u32, h: u32) -> DecodeCap {
    DecodeCap {
        codec: codec.into(),
        coded_width: w,
        coded_height: h,
        implementation: Implementation::WebCodecs,
        hardware_acceleration: Some("prefer-hardware".into()),
    }
}

fn dav1d(w: u32, h: u32) -> DecodeCap {
    DecodeCap {
        implementation: Implementation::Dav1dWasm,
        hardware_acceleration: None,
        ..wc("av01.0.13M.08", w, h)
    }
}

fn av1() -> DecodeCap {
    wc("av01.0.13M.08", 2560, 1600)
}
fn hevc() -> DecodeCap {
    wc("hvc1.1.6.L153.B0", 2560, 1600)
}
fn h264() -> DecodeCap {
    wc("avc1.42c034", 2560, 800)
}

fn choice(cand: Candidate, imp: Implementation) -> Choice {
    Choice {
        cand,
        implementation: Some(imp),
    }
}

fn picks(p: &Plan) -> Vec<Option<Codec>> {
    p.picks
        .iter()
        .map(|(_, c)| c.map(|c| c.cand.codec))
        .collect()
}

#[test]
fn candidates_rank_codecs_then_hardware() {
    let r = reg(vec![
        cap(Codec::H264, Backend::OpenH264, false),
        cap(Codec::Hevc, Backend::Vaapi, false),
        cap(Codec::Hevc, Backend::VideoToolbox, true),
        cap(Codec::Av1, Backend::Nvenc, true),
        cap(Codec::H264, Backend::VideoToolbox, true),
    ]);
    let c = candidates(&r, Backend::OpenH264);
    let got: Vec<(Codec, Backend)> = c.iter().map(|c| (c.codec, c.backend)).collect();
    assert_eq!(
        got,
        vec![
            (Codec::Hevc, Backend::VideoToolbox),
            // H.264 is the configured backend alone, whatever else is eligible.
            (Codec::H264, Backend::OpenH264),
        ]
    );
    let vt: Vec<(Backend, bool)> = candidates(&r, Backend::VideoToolbox)
        .iter()
        .filter(|c| c.codec == Codec::H264)
        .map(|c| (c.backend, c.hardware))
        .collect();
    assert_eq!(
        vt,
        vec![(Backend::VideoToolbox, true), (Backend::OpenH264, false)],
        "a hardware H.264 encoder falls back to OpenH264"
    );
}

#[test]
fn auto_selection() {
    let sw = || cap(Codec::H264, Backend::OpenH264, false);
    let vt = || cap(Codec::H264, Backend::VideoToolbox, true);
    let auto = |caps: Vec<EncoderCapability>| h264_backend(&reg(caps), Selection::Auto);
    assert_eq!(auto(vec![sw(), vt()]), Ok(Backend::VideoToolbox));
    let mut discovered = vt();
    discovered.status = ProbeStatus::Discovered;
    let mut disabled = cap(Codec::H264, Backend::Vaapi, true);
    disabled.status = ProbeStatus::Disabled;
    disabled.implemented = false;
    let mut experimental = cap(Codec::H264, Backend::Nvenc, true);
    experimental.implemented = false;
    let mut no_idr = cap(Codec::H264, Backend::MediaFoundation, true);
    no_idr.keyframe_control = None;
    assert_eq!(
        auto(vec![discovered, disabled, experimental, no_idr, sw()]),
        Ok(Backend::OpenH264)
    );
    assert_eq!(auto(vec![sw()]), Ok(Backend::OpenH264));
    assert!(auto(vec![]).is_err(), "no OpenH264 either");
    let pinned = |b, caps| h264_backend(&reg(caps), Selection::Pinned(b));
    assert_eq!(
        pinned(Backend::OpenH264, vec![sw(), vt()]),
        Ok(Backend::OpenH264)
    );
    assert_eq!(
        pinned(Backend::VideoToolbox, vec![sw(), vt()]),
        Ok(Backend::VideoToolbox)
    );
    let mut lost = vt();
    lost.status = ProbeStatus::Unavailable("no session".into());
    let e = pinned(Backend::VideoToolbox, vec![sw(), lost]).unwrap_err();
    assert!(
        e.contains("videotoolbox") && e.contains("no session"),
        "{e}"
    );
    let e = pinned(Backend::VideoToolbox, vec![sw()]).unwrap_err();
    assert!(e.contains("no videotoolbox H264 encoder"), "{e}");
    let r = reg(vec![sw(), vt()]);
    let h: Vec<Backend> = candidates(&r, Backend::OpenH264)
        .iter()
        .map(|c| c.backend)
        .collect();
    assert_eq!(h, vec![Backend::OpenH264], "pinned OpenH264 stays software");
}

#[test]
fn experimental_backends_only_when_pinned() {
    let r = reg(vec![
        cap(Codec::H264, Backend::Vaapi, true),
        cap(Codec::Hevc, Backend::Vaapi, true),
        cap(Codec::Av1, Backend::Vaapi, true),
        cap(Codec::Hevc, Backend::VideoToolbox, true),
        cap(Codec::H264, Backend::OpenH264, false),
    ]);
    let h264 = h264_backend(&r, Selection::Auto).unwrap();
    assert_eq!(h264, Backend::OpenH264, "auto never picks VA-API");
    assert!(candidates(&r, h264)
        .iter()
        .all(|c| c.backend != Backend::Vaapi));
    let h264 = h264_backend(&r, Selection::Pinned(Backend::Vaapi)).unwrap();
    assert_eq!(h264, Backend::Vaapi);
    let got: Vec<(Codec, Backend)> = candidates(&r, h264)
        .iter()
        .map(|c| (c.codec, c.backend))
        .collect();
    assert_eq!(
        got,
        vec![
            (Codec::Av1, Backend::Vaapi),
            (Codec::Hevc, Backend::Vaapi),
            (Codec::Hevc, Backend::VideoToolbox),
            (Codec::H264, Backend::Vaapi),
            (Codec::H264, Backend::OpenH264),
        ]
    );
    let mut off = r.encoders.clone();
    for c in off.iter_mut().filter(|c| c.backend == Backend::Vaapi) {
        c.status = ProbeStatus::Disabled;
    }
    let e = h264_backend(&reg(off), Selection::Pinned(Backend::Vaapi)).unwrap_err();
    assert!(e.contains("--experimental-hw-probes"), "{e}");
}

#[test]
fn nvenc_only_when_pinned() {
    let r = reg(vec![
        cap(Codec::H264, Backend::Nvenc, true),
        cap(Codec::Hevc, Backend::Nvenc, true),
        cap(Codec::Av1, Backend::Nvenc, true),
        cap(Codec::H264, Backend::OpenH264, false),
    ]);
    let h264 = h264_backend(&r, Selection::Auto).unwrap();
    assert_eq!(h264, Backend::OpenH264, "auto never picks NVENC");
    assert!(candidates(&r, h264)
        .iter()
        .all(|c| c.backend != Backend::Nvenc));
    let h264 = h264_backend(&r, Selection::Pinned(Backend::Nvenc)).unwrap();
    assert_eq!(h264, Backend::Nvenc);
    let got: Vec<(Codec, Backend)> = candidates(&r, h264)
        .iter()
        .map(|c| (c.codec, c.backend))
        .collect();
    assert_eq!(
        got,
        vec![
            (Codec::Av1, Backend::Nvenc),
            (Codec::Hevc, Backend::Nvenc),
            (Codec::H264, Backend::Nvenc),
            (Codec::H264, Backend::OpenH264),
        ]
    );
    let mut off = r.encoders.clone();
    for c in off.iter_mut().filter(|c| c.backend == Backend::Nvenc) {
        c.status = ProbeStatus::Disabled;
    }
    let e = h264_backend(&reg(off), Selection::Pinned(Backend::Nvenc)).unwrap_err();
    assert!(e.contains("--experimental-hw-probes"), "{e}");
}

#[test]
fn mediafoundation_only_when_pinned() {
    let r = reg(vec![
        cap(Codec::H264, Backend::MediaFoundation, true),
        cap(Codec::Hevc, Backend::MediaFoundation, true),
        cap(Codec::Av1, Backend::MediaFoundation, true),
        cap(Codec::H264, Backend::OpenH264, false),
    ]);
    let h264 = h264_backend(&r, Selection::Auto).unwrap();
    assert_eq!(h264, Backend::OpenH264, "auto never picks Media Foundation");
    assert!(candidates(&r, h264)
        .iter()
        .all(|c| c.backend != Backend::MediaFoundation));
    let h264 = h264_backend(&r, Selection::Pinned(Backend::MediaFoundation)).unwrap();
    assert_eq!(h264, Backend::MediaFoundation);
    let got: Vec<(Codec, Backend)> = candidates(&r, h264)
        .iter()
        .map(|c| (c.codec, c.backend))
        .collect();
    assert_eq!(
        got,
        vec![
            (Codec::Av1, Backend::MediaFoundation),
            (Codec::Hevc, Backend::MediaFoundation),
            (Codec::H264, Backend::MediaFoundation),
            (Codec::H264, Backend::OpenH264),
        ]
    );
    let mut off = r.encoders.clone();
    for c in off
        .iter_mut()
        .filter(|c| c.backend == Backend::MediaFoundation)
    {
        c.status = ProbeStatus::Disabled;
    }
    let e = h264_backend(&reg(off), Selection::Pinned(Backend::MediaFoundation)).unwrap_err();
    assert!(e.contains("--experimental-hw-probes"), "{e}");
}

#[test]
fn chosen_is_each_codecs_first_candidate() {
    let r = reg(vec![
        cap(Codec::H264, Backend::OpenH264, false),
        cap(Codec::H264, Backend::VideoToolbox, true),
        cap(Codec::Hevc, Backend::Vaapi, false),
        cap(Codec::Hevc, Backend::VideoToolbox, true),
    ]);
    let c: Vec<(Codec, Backend)> = chosen(&candidates(&r, Backend::VideoToolbox))
        .iter()
        .map(|c| (c.codec, c.backend))
        .collect();
    assert_eq!(
        c,
        vec![
            (Codec::Hevc, Backend::VideoToolbox),
            (Codec::H264, Backend::VideoToolbox)
        ]
    );
}

#[test]
fn selection_parse() {
    assert_eq!(Selection::parse("auto"), Ok(Selection::Auto));
    assert_eq!(
        Selection::parse("openh264"),
        Ok(Selection::Pinned(Backend::OpenH264))
    );
    #[cfg(target_os = "linux")]
    assert_eq!(
        Selection::parse("vaapi"),
        Ok(Selection::Pinned(Backend::Vaapi))
    );
    #[cfg(not(target_os = "linux"))]
    assert!(Selection::parse("vaapi").is_err());
    #[cfg(any(target_os = "linux", windows))]
    assert_eq!(
        Selection::parse("nvenc"),
        Ok(Selection::Pinned(Backend::Nvenc))
    );
    #[cfg(not(any(target_os = "linux", windows)))]
    assert!(Selection::parse("nvenc").is_err());
    #[cfg(windows)]
    assert_eq!(
        Selection::parse("mediafoundation"),
        Ok(Selection::Pinned(Backend::MediaFoundation))
    );
    #[cfg(not(windows))]
    assert!(Selection::parse("mediafoundation").is_err());
    assert!(Selection::parse("h265").unwrap_err().contains("auto"));
    assert_eq!(Selection::Auto.name(), "auto");
}

#[test]
fn only_eligible_encoders_are_candidates() {
    let mut unverified = cap(Codec::Hevc, Backend::VideoToolbox, true);
    unverified.status = ProbeStatus::Discovered;
    let mut unimplemented = cap(Codec::Av1, Backend::Nvenc, true);
    unimplemented.implemented = false;
    let mut ten_bit = cap(Codec::Hevc, Backend::Vaapi, true);
    ten_bit.profiles = vec![ProfileCapability::new("main10", 10, Chroma::Yuv420)];
    let mut no_idr = cap(Codec::Av1, Backend::Vaapi, true);
    no_idr.keyframe_control = None;
    let r = reg(vec![unverified, unimplemented, ten_bit, no_idr]);
    let c = candidates(&r, Backend::OpenH264);
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].codec, Codec::H264);
}

#[cfg(feature = "x264")]
#[test]
fn x264_only_when_configured() {
    let r = reg(vec![
        cap(Codec::H264, Backend::OpenH264, false),
        cap(Codec::H264, Backend::X264, false),
    ]);
    let h: Vec<Backend> = candidates(&r, Backend::OpenH264)
        .iter()
        .map(|c| c.backend)
        .collect();
    assert_eq!(h, vec![Backend::OpenH264]);
    let h: Vec<Backend> = candidates(&r, Backend::X264)
        .iter()
        .map(|c| c.backend)
        .collect();
    assert_eq!(h, vec![Backend::X264]);
}

#[test]
fn preference_order() {
    let o = offers();
    let all = [
        av1(),
        hevc(),
        h264(),
        wc("avc1.42c034", 2560, 800),
        dav1d(2560, 1600),
    ];
    let p = preferences(&all, &[], &o);
    assert_eq!(
        p,
        vec![
            choice(AV1, Implementation::WebCodecs),
            choice(HEVC, Implementation::WebCodecs),
            choice(H264, Implementation::WebCodecs),
            choice(AV1, Implementation::Dav1dWasm),
        ]
    );
    // dav1d-wasm comes after every WebCodecs codec, and never stands in for HEVC.
    let p = preferences(
        &[
            dav1d(2560, 1600),
            h264(),
            DecodeCap {
                implementation: Implementation::Dav1dWasm,
                ..hevc()
            },
        ],
        &[],
        &o,
    );
    assert_eq!(
        p,
        vec![
            choice(H264, Implementation::WebCodecs),
            choice(AV1, Implementation::Dav1dWasm)
        ]
    );
}

#[test]
fn an_entry_covers_only_its_exact_configuration() {
    let o = offers();
    // Another size, another level, a missing stripe size: none of them is support.
    for caps in [
        vec![wc("hvc1.1.6.L153.B0", 1920, 1080)],
        vec![wc("hvc1.1.6.L120.B0", 2560, 1600)],
        vec![wc("avc1.42c034", 2560, 1600)],
    ] {
        assert!(preferences(&caps, &[], &o).is_empty(), "{caps:?}");
    }
    // Every stripe of a multi-stripe offer must be covered.
    let o = vec![offer(H264, "avc1.42c034", &[(0, 800), (800, 400)], 2560)];
    assert!(preferences(&[h264()], &[], &o).is_empty());
    assert_eq!(
        preferences(&[h264(), wc("avc1.42c034", 2560, 400)], &[], &o).len(),
        1
    );
}

#[test]
fn rejected_choices_are_skipped() {
    let o = offers();
    let rej = [choice(HEVC, Implementation::WebCodecs)];
    let p = preferences(&[hevc(), h264()], &rej, &o);
    assert_eq!(p, vec![choice(H264, Implementation::WebCodecs)]);
}

fn viewer<'a>(id: u64, caps: &'a [DecodeCap]) -> Viewer<'a> {
    Viewer {
        id,
        legacy: false,
        caps,
        rejected: &[],
    }
}

#[test]
fn legacy_viewers_get_h264() {
    let o = offers();
    let v = [Viewer {
        id: 1,
        legacy: true,
        caps: &[],
        rejected: &[],
    }];
    let p = plan(&v, &o, H264, None);
    assert_eq!(p.best, None);
    assert_eq!(
        p.picks,
        vec![(
            1,
            Some(Choice {
                cand: H264,
                implementation: None
            })
        )]
    );
}

#[test]
fn two_cohorts_then_the_closest() {
    let o = offers();
    let a = [hevc(), h264()];
    let b = [h264()];
    let c = [av1(), hevc(), h264()];
    let d = [av1(), h264()];
    // A: HEVC, B: H.264 only. Two cohorts.
    let p = plan(&[viewer(1, &a), viewer(2, &b)], &o, H264, None);
    assert_eq!(p.best, Some(HEVC));
    assert_eq!(picks(&p), vec![Some(Codec::Hevc), Some(Codec::H264)]);
    // C would rather have AV1, but that would be a third cohort: it joins HEVC.
    let v = [viewer(1, &a), viewer(2, &b), viewer(3, &c)];
    let p = plan(&v, &o, H264, p.best);
    assert_eq!(p.best, Some(HEVC));
    assert_eq!(
        picks(&p),
        vec![Some(Codec::Hevc), Some(Codec::H264), Some(Codec::Hevc)]
    );
    // D decodes AV1 and H.264, not HEVC: H.264.
    let v = [viewer(1, &a), viewer(2, &b), viewer(3, &c), viewer(4, &d)];
    let p = plan(&v, &o, H264, p.best);
    assert_eq!(
        picks(&p),
        vec![
            Some(Codec::Hevc),
            Some(Codec::H264),
            Some(Codec::Hevc),
            Some(Codec::H264)
        ]
    );
    // A leaves: HEVC is nobody's first choice now, AV1 is, and C and D move up.
    let v = [viewer(2, &b), viewer(3, &c), viewer(4, &d)];
    let p = plan(&v, &o, H264, p.best);
    assert_eq!(p.best, Some(AV1));
    assert_eq!(
        picks(&p),
        vec![Some(Codec::H264), Some(Codec::Av1), Some(Codec::Av1)]
    );
    // The same viewers again: nothing moves.
    assert_eq!(plan(&v, &o, H264, p.best), p);
}

#[test]
fn the_best_cohort_stays_while_it_is_a_first_choice() {
    let o = offers();
    let c = [av1(), hevc(), h264()];
    let a = [hevc(), h264()];
    // AV1 runs for C; A arriving later doesn't take it away.
    let p = plan(&[viewer(1, &c), viewer(2, &a)], &o, H264, Some(AV1));
    assert_eq!(p.best, Some(AV1));
    assert_eq!(picks(&p), vec![Some(Codec::Av1), Some(Codec::H264)]);
}

#[test]
fn dav1d_wasm_only_without_a_native_decoder() {
    let o = offers();
    let wasm_and_h264 = [dav1d(2560, 1600), h264()];
    let wasm_only = [dav1d(2560, 1600)];
    let p = plan(&[viewer(1, &wasm_and_h264)], &o, H264, None);
    assert_eq!(p.best, None);
    assert_eq!(picks(&p), vec![Some(Codec::H264)]);
    let p = plan(&[viewer(1, &wasm_only)], &o, H264, None);
    assert_eq!(p.best, Some(AV1));
    assert_eq!(
        p.picks[0].1.unwrap().implementation,
        Some(Implementation::Dav1dWasm)
    );
}

#[test]
fn no_common_codec() {
    let o = offers();
    let vp9 = [wc("vp09.00.10.08", 2560, 1600)];
    let p = plan(&[viewer(1, &[]), viewer(2, &vp9)], &o, H264, None);
    assert_eq!(p.best, None);
    assert_eq!(picks(&p), vec![None, None]);
}

#[test]
fn failures_walk_down_and_end() {
    let o = offers();
    let all = [av1(), hevc(), h264()];
    let mut rejected = Vec::new();
    let mut seen = Vec::new();
    for _ in 0..10 {
        let v = [Viewer {
            id: 1,
            legacy: false,
            caps: &all,
            rejected: &rejected,
        }];
        let Some(c) = plan(&v, &o, H264, None).picks[0].1 else {
            break;
        };
        seen.push(c.cand.codec);
        rejected.push(c);
    }
    assert_eq!(seen, vec![Codec::Av1, Codec::Hevc, Codec::H264]);
}

#[test]
fn client_messages() {
    let caps = r#"{"t":"hello","v":2,"offer":3,"decode":[
        {"codec":"hvc1.1.6.L153.B0","codedWidth":2560,"codedHeight":1600,"implementation":"webcodecs","hardwareAcceleration":"prefer-hardware"},
        {"codec":"av01.0.13M.08","codedWidth":2560,"codedHeight":1600,"implementation":"dav1d-wasm"},
        {"codec":"vp8","codedWidth":2560,"codedHeight":1600,"implementation":"libvpx-wasm"},
        {"codec":"avc1.42c034"}]}"#;
    let Some(ClientMsg::Hello { v, offer, decode }) = ClientMsg::parse(caps) else {
        panic!("not a hello");
    };
    assert_eq!((v, offer), (2, Some(3)));
    let d = decode_caps(&decode);
    assert_eq!(
        d.len(),
        2,
        "unknown implementations and partial entries are skipped"
    );
    assert_eq!(d[0], hevc());
    assert_eq!(d[1], dav1d(2560, 1600));
    assert_eq!(
        ClientMsg::parse(r#"{"t":"codec-ready","epoch":7}"#),
        Some(ClientMsg::Ready { epoch: 7 })
    );
    assert_eq!(
        ClientMsg::parse(r#"{"t":"codec-error","epoch":7,"reason":"unsupported"}"#),
        Some(ClientMsg::Error {
            epoch: 7,
            reason: "unsupported".into()
        })
    );
    assert_eq!(
        ClientMsg::parse(r#"{"t":"ack","epoch":7,"frame":123}"#),
        Some(ClientMsg::Ack {
            epoch: 7,
            frame: 123
        })
    );
    assert_eq!(
        ClientMsg::parse(r#"{"t":"keyframe","epoch":7}"#),
        Some(ClientMsg::Keyframe { epoch: 7 })
    );
    for bad in [
        r#"{"t":"ack","frame":1}"#,
        r#"{"t":"mystery"}"#,
        r#"{"t":"ack","epoch":-1,"frame":1}"#,
        "not json",
    ] {
        assert_eq!(ClientMsg::parse(bad), None, "{bad}");
    }
    let many = format!(
        r#"{{"t":"hello","v":2,"decode":[{}]}}"#,
        vec![r#"{"codec":"avc1.42c034","codedWidth":16,"codedHeight":16,"implementation":"webcodecs"}"#; 500].join(",")
    );
    let Some(ClientMsg::Hello { decode, offer, .. }) = ClientMsg::parse(&many) else {
        panic!();
    };
    assert_eq!(offer, None);
    assert_eq!(decode_caps(&decode).len(), MAX_DECODE_ENTRIES);
}

#[test]
fn server_messages() {
    let o = offer(HEVC, "hvc1.1.6.L153.B0", &[(0, 1600)], 2560);
    let m: serde_json::Value = serde_json::from_str(&o.announce(7, 2560, 1600)).unwrap();
    assert_eq!(
        m,
        serde_json::json!({"t":"codec","epoch":7,"codec":"hevc","backend":"videotoolbox","hardware":true,"format":"annexb","w":2560,"h":1600,
            "stripes":[{"i":0,"y":0,"h":1600,"config":{"codec":"hvc1.1.6.L153.B0","codedWidth":2560,"codedHeight":1600}}]})
    );
    let offers = serde_json::to_value(vec![o]).unwrap();
    assert!(offers[0].get("cand").is_none());
    let m: serde_json::Value = serde_json::from_str(&no_codec(3)).unwrap();
    assert_eq!(
        m,
        serde_json::json!({"t":"codec","epoch":3,"error":"no-common-codec"})
    );
}

#[test]
fn video_v2_layout() {
    let mut v1 = vec![VIDEO_V1, 1, 0, 0, 0, 9, 0, 0, 0, 5, 0, 1, 1, 0, 0, 0];
    v1.extend_from_slice(&[0, 0, 0, 1, 0x65, 0xaa]);
    let v2 = video_v2(&v1, 0x0102_0304);
    assert_eq!(v2.len(), v1.len() + 4);
    assert_eq!(v2[0], VIDEO_V2);
    assert_eq!(&v2[1..HDR_V1], &v1[1..HDR_V1]);
    assert_eq!(&v2[HDR_V1..HDR_V2], &[1, 2, 3, 4]);
    assert_eq!(&v2[HDR_V2..], &v1[HDR_V1..]);
}

/// What `--encoder auto` picks on this host: VideoToolbox H.264 and HEVC on the
/// kyora-macos runner, OpenH264 alone where no hardware encoder is default-safe yet.
#[test]
fn auto_on_this_host() {
    use crate::encoder::caps::{host_providers, PROBE_TIMEOUT};
    let reg = Registry::probe(host_providers(true, false), PROBE_TIMEOUT);
    let h264 = h264_backend(&reg, Selection::Auto).unwrap();
    let got: Vec<(Codec, Backend, bool)> = chosen(&candidates(&reg, h264))
        .iter()
        .map(|c| (c.codec, c.backend, c.hardware))
        .collect();
    println!("--encoder auto: {got:?}");
    if cfg!(target_os = "macos") {
        if std::env::var_os("STARFOLD_EXPECT_HW").is_some() {
            assert_eq!(
                got,
                vec![
                    (Codec::Hevc, Backend::VideoToolbox, true),
                    (Codec::H264, Backend::VideoToolbox, true)
                ]
            );
        }
    } else {
        assert_eq!(got, vec![(Codec::H264, Backend::OpenH264, false)]);
    }
}

#[test]
fn the_other_h264_encoder_serves_viewers_that_cant_decode_the_first() {
    const VT264: Candidate = Candidate {
        codec: Codec::H264,
        backend: Backend::VideoToolbox,
        hardware: true,
    };
    let offers = vec![
        offer(HEVC, "hvc1.1.6.L153.B0", &[(0, 1600)], 2560),
        offer(VT264, "avc1.640034", &[(0, 1600)], 2560),
        offer(H264, "avc1.42c034", &[(0, 1600)], 2560),
    ];
    let baseline = [wc("avc1.42c034", 2560, 1600)];
    let both = [wc("avc1.640034", 2560, 1600), wc("avc1.42c034", 2560, 1600)];
    let rejected = [choice(VT264, Implementation::WebCodecs)];
    let viewers = [
        viewer(1, &both),
        viewer(2, &baseline),
        Viewer {
            id: 3,
            legacy: false,
            caps: &both,
            rejected: &rejected,
        },
        Viewer {
            id: 4,
            legacy: true,
            caps: &[],
            rejected: &[],
        },
    ];
    let p = plan(&viewers, &offers, VT264, None);
    assert_eq!(p.best, Some(H264));
    let got: Vec<Option<Backend>> = p
        .picks
        .iter()
        .map(|(_, c)| c.map(|c| c.cand.backend))
        .collect();
    assert_eq!(
        got,
        vec![
            Some(Backend::VideoToolbox),
            Some(Backend::OpenH264),
            Some(Backend::OpenH264),
            Some(Backend::VideoToolbox)
        ]
    );
    let first = [hevc(), wc("avc1.42c034", 2560, 1600)];
    let p = plan(
        &[viewer(5, &first), viewer(2, &baseline)],
        &offers,
        VT264,
        None,
    );
    assert_eq!(
        p.best,
        Some(HEVC),
        "the earliest viewer decides the second cohort"
    );
    assert_eq!(p.picks[1].1, None);
}
