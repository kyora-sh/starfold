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
    backend: Backend::FakeH264,
    hardware: false,
};
/// This build's software base encoder: libaom's AV1, or x264's H.264 where it is built.
const SW: Candidate = Candidate {
    codec: SOFTWARE_CODEC,
    backend: SOFTWARE,
    hardware: false,
};

fn sw() -> EncoderCapability {
    cap(SOFTWARE_CODEC, SOFTWARE, false)
}

fn hw264(backend: Backend) -> Candidate {
    Candidate {
        codec: Codec::H264,
        backend,
        hardware: true,
    }
}

fn pairs(c: &[Candidate]) -> Vec<(Codec, Backend)> {
    c.iter().map(|c| (c.codec, c.backend)).collect()
}

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
fn candidates_rank_hardware_then_codecs() {
    let mut caps = vec![
        cap(Codec::H264, Backend::FakeH264, false),
        cap(Codec::Hevc, Backend::Vaapi, false),
        cap(Codec::Hevc, Backend::VideoToolbox, true),
        cap(Codec::Av1, Backend::Nvenc, true),
        cap(Codec::H264, Backend::VideoToolbox, true),
    ];
    assert_eq!(
        pairs(&candidates(&reg(caps.clone()), H264)),
        vec![
            (Codec::Hevc, Backend::VideoToolbox),
            // H.264 is the base backend alone, whatever else is eligible.
            (Codec::H264, Backend::FakeH264),
        ]
    );
    caps.push(sw());
    let c = candidates(&reg(caps), hw264(Backend::VideoToolbox));
    assert_eq!(
        pairs(&c[..2]),
        vec![
            (Codec::Hevc, Backend::VideoToolbox),
            (Codec::H264, Backend::VideoToolbox)
        ]
    );
    assert_eq!(
        c.last(),
        Some(&SW),
        "a hardware H.264 encoder falls back to the software one"
    );
}

#[test]
fn auto_selection() {
    let vt = || cap(Codec::H264, Backend::VideoToolbox, true);
    let auto = |caps: Vec<EncoderCapability>| base_backend(&reg(caps), Selection::Auto);
    assert_eq!(auto(vec![sw(), vt()]), Ok(hw264(Backend::VideoToolbox)));
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
        Ok(SW)
    );
    assert_eq!(auto(vec![sw()]), Ok(SW));
    assert!(auto(vec![]).is_err(), "no software encoder either");
    let pinned = |b, caps| base_backend(&reg(caps), Selection::Pinned(b));
    assert_eq!(pinned(SOFTWARE, vec![sw(), vt()]), Ok(SW));
    assert_eq!(
        pinned(Backend::VideoToolbox, vec![sw(), vt()]),
        Ok(hw264(Backend::VideoToolbox))
    );
    assert_eq!(
        pinned(
            Backend::FakeH264,
            vec![sw(), vt(), cap(Codec::H264, Backend::FakeH264, false)]
        ),
        Ok(H264)
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
    assert_eq!(
        candidates_for(&r, Selection::Pinned(SOFTWARE), SW),
        vec![SW],
        "the pinned software encoder stays software"
    );
}

#[test]
fn experimental_backends_only_when_pinned() {
    let r = reg(vec![
        cap(Codec::H264, Backend::Vaapi, true),
        cap(Codec::Hevc, Backend::Vaapi, true),
        cap(Codec::Av1, Backend::Vaapi, true),
        cap(Codec::Hevc, Backend::VideoToolbox, true),
        sw(),
    ]);
    let base = base_backend(&r, Selection::Auto).unwrap();
    assert_eq!(base, SW, "auto never picks VA-API");
    assert!(candidates(&r, base)
        .iter()
        .all(|c| c.backend != Backend::Vaapi));
    let base = base_backend(&r, Selection::Pinned(Backend::Vaapi)).unwrap();
    assert_eq!(base, hw264(Backend::Vaapi));
    assert_eq!(
        pairs(&candidates_for(&r, Selection::Pinned(Backend::Vaapi), base)),
        vec![
            (Codec::Av1, Backend::Vaapi),
            (Codec::Hevc, Backend::Vaapi),
            (Codec::H264, Backend::Vaapi),
            (SOFTWARE_CODEC, SOFTWARE),
        ],
        "a pinned encoder streams alone, the software one only its fallback"
    );
    let mut off = r.encoders.clone();
    for c in off.iter_mut().filter(|c| c.backend == Backend::Vaapi) {
        c.status = ProbeStatus::Disabled;
    }
    let e = base_backend(&reg(off), Selection::Pinned(Backend::Vaapi)).unwrap_err();
    assert!(e.contains("--experimental-hw-probes"), "{e}");
}

#[test]
fn nvenc_only_when_pinned() {
    let r = reg(vec![
        cap(Codec::H264, Backend::Nvenc, true),
        cap(Codec::Hevc, Backend::Nvenc, true),
        cap(Codec::Av1, Backend::Nvenc, true),
        sw(),
    ]);
    let base = base_backend(&r, Selection::Auto).unwrap();
    assert_eq!(base, SW, "auto never picks NVENC");
    assert!(candidates(&r, base)
        .iter()
        .all(|c| c.backend != Backend::Nvenc));
    let base = base_backend(&r, Selection::Pinned(Backend::Nvenc)).unwrap();
    assert_eq!(base, hw264(Backend::Nvenc));
    assert_eq!(
        pairs(&candidates_for(&r, Selection::Pinned(Backend::Nvenc), base)),
        vec![
            (Codec::Av1, Backend::Nvenc),
            (Codec::Hevc, Backend::Nvenc),
            (Codec::H264, Backend::Nvenc),
            (SOFTWARE_CODEC, SOFTWARE),
        ]
    );
    let mut off = r.encoders.clone();
    for c in off.iter_mut().filter(|c| c.backend == Backend::Nvenc) {
        c.status = ProbeStatus::Disabled;
    }
    let e = base_backend(&reg(off), Selection::Pinned(Backend::Nvenc)).unwrap_err();
    assert!(e.contains("--experimental-hw-probes"), "{e}");
}

#[test]
fn mediafoundation_only_when_pinned() {
    let r = reg(vec![
        cap(Codec::H264, Backend::MediaFoundation, true),
        cap(Codec::Hevc, Backend::MediaFoundation, true),
        cap(Codec::Av1, Backend::MediaFoundation, true),
        sw(),
    ]);
    let base = base_backend(&r, Selection::Auto).unwrap();
    assert_eq!(base, SW, "auto never picks Media Foundation");
    assert!(candidates(&r, base)
        .iter()
        .all(|c| c.backend != Backend::MediaFoundation));
    let base = base_backend(&r, Selection::Pinned(Backend::MediaFoundation)).unwrap();
    assert_eq!(base, hw264(Backend::MediaFoundation));
    assert_eq!(
        pairs(&candidates_for(
            &r,
            Selection::Pinned(Backend::MediaFoundation),
            base
        )),
        vec![
            (Codec::Av1, Backend::MediaFoundation),
            (Codec::Hevc, Backend::MediaFoundation),
            (Codec::H264, Backend::MediaFoundation),
            (SOFTWARE_CODEC, SOFTWARE),
        ]
    );
    let mut off = r.encoders.clone();
    for c in off
        .iter_mut()
        .filter(|c| c.backend == Backend::MediaFoundation)
    {
        c.status = ProbeStatus::Disabled;
    }
    let e = base_backend(&reg(off), Selection::Pinned(Backend::MediaFoundation)).unwrap_err();
    assert!(e.contains("--experimental-hw-probes"), "{e}");
}

#[test]
fn chosen_is_each_codecs_first_candidate() {
    let r = reg(vec![
        cap(Codec::H264, Backend::FakeH264, false),
        cap(Codec::H264, Backend::VideoToolbox, true),
        cap(Codec::Hevc, Backend::Vaapi, false),
        cap(Codec::Hevc, Backend::VideoToolbox, true),
    ]);
    let c: Vec<(Codec, Backend)> = chosen(&candidates(&r, hw264(Backend::VideoToolbox)))
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
    let e = Selection::parse("openh264").unwrap_err();
    assert!(e.contains("no longer has OpenH264"), "{e}");
    #[cfg(feature = "libaom")]
    assert_eq!(
        Selection::parse("libaom"),
        Ok(Selection::Pinned(Backend::Libaom))
    );
    #[cfg(feature = "x264")]
    assert_eq!(
        Selection::parse("x264"),
        Ok(Selection::Pinned(Backend::X264))
    );
    #[cfg(not(feature = "x264"))]
    assert!(Selection::parse("x264")
        .unwrap_err()
        .contains("--features x264"));
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
    assert_eq!(candidates(&r, H264), vec![H264]);
}

#[cfg(feature = "x264")]
#[test]
fn x264_only_when_configured() {
    let r = reg(vec![
        cap(Codec::H264, Backend::FakeH264, false),
        cap(Codec::H264, Backend::X264, false),
    ]);
    assert_eq!(candidates(&r, H264), vec![H264]);
    let x264 = Candidate {
        codec: Codec::H264,
        backend: Backend::X264,
        hardware: false,
    };
    assert_eq!(candidates(&r, x264), vec![x264]);
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
    let Some(ClientMsg::Hello {
        v, offer, decode, ..
    }) = ClientMsg::parse(caps)
    else {
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
/// kyora-macos runner ahead of libaom's AV1, the software ones alone where no hardware
/// encoder is default-safe yet.
#[test]
fn auto_on_this_host() {
    use crate::encoder::caps::{host_providers, PROBE_TIMEOUT};
    let reg = Registry::probe(host_providers(true, false, true), PROBE_TIMEOUT);
    let base = base_backend(&reg, Selection::Auto).unwrap();
    let got: Vec<(Codec, Backend, bool)> = chosen(&candidates(&reg, base))
        .iter()
        .map(|c| (c.codec, c.backend, c.hardware))
        .collect();
    println!("--encoder auto: {got:?}");
    if cfg!(target_os = "macos") {
        if std::env::var_os("STARFOLD_EXPECT_HW").is_some() {
            let want: Vec<(Codec, Backend, bool)> = [
                (Codec::Hevc, Backend::VideoToolbox, true),
                (Codec::H264, Backend::VideoToolbox, true),
                #[cfg(feature = "libaom")]
                (Codec::Av1, Backend::Libaom, false),
            ]
            .into();
            assert_eq!(got, want);
        }
    } else {
        let mut want: Vec<(Codec, Backend, bool)> = [
            #[cfg(feature = "libaom")]
            (Codec::Av1, Backend::Libaom, false),
        ]
        .into();
        if SOFTWARE_CODEC == Codec::H264 {
            want.push((Codec::H264, SOFTWARE, false));
        }
        assert_eq!(got, want);
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
            Some(Backend::FakeH264),
            Some(Backend::FakeH264),
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

const FAKE_AV1: Candidate = Candidate {
    codec: Codec::Av1,
    backend: Backend::FakeAv1,
    hardware: false,
};

#[cfg(feature = "libaom")]
const LIBAOM: Candidate = Candidate {
    codec: Codec::Av1,
    backend: Backend::Libaom,
    hardware: false,
};

#[cfg(all(feature = "x264", feature = "libaom"))]
#[test]
fn the_base_chain_falls_back_from_hardware_to_x264() {
    let r = reg(vec![
        cap(Codec::H264, Backend::VideoToolbox, true),
        cap(Codec::H264, Backend::X264, false),
        cap(Codec::Av1, Backend::Libaom, false),
    ]);
    let base = base_backend(&r, Selection::Auto).unwrap();
    assert_eq!(base, hw264(Backend::VideoToolbox));
    let x264 = Candidate {
        codec: Codec::H264,
        backend: Backend::X264,
        hardware: false,
    };
    assert_eq!(base_chain(&candidates(&r, base)), vec![base, x264, LIBAOM]);
    let pinned = candidates_for(&r, Selection::Pinned(Backend::VideoToolbox), base);
    assert_eq!(pinned, vec![base, x264, LIBAOM]);
    assert_eq!(base_chain(&pinned), vec![base, x264, LIBAOM]);
    let alone = base_backend(&r, Selection::Pinned(Backend::X264)).unwrap();
    assert_eq!(alone, x264);
    assert_eq!(
        candidates_for(&r, Selection::Pinned(Backend::X264), alone),
        vec![x264],
        "a pinned x264 streams H.264 to every viewer"
    );
}

#[cfg(feature = "libaom")]
#[test]
fn a_pinned_hardware_encoder_leads_libaom() {
    let r = reg(vec![
        cap(Codec::Av1, Backend::Libaom, false),
        cap(Codec::Hevc, Backend::VideoToolbox, true),
        cap(Codec::Hevc, Backend::Vaapi, true),
        cap(Codec::H264, Backend::Vaapi, true),
    ]);
    let sel = Selection::Pinned(Backend::Vaapi);
    let base = base_backend(&r, sel).unwrap();
    assert_eq!(
        pairs(&candidates_for(&r, sel, base)),
        vec![
            (Codec::Hevc, Backend::Vaapi),
            (Codec::H264, Backend::Vaapi),
            (Codec::Av1, Backend::Libaom),
        ]
    );
}

#[cfg(feature = "libaom")]
#[test]
fn a_pinned_libaom_streams_only_av1() {
    let r = reg(vec![
        cap(Codec::Av1, Backend::Libaom, false),
        cap(Codec::Hevc, Backend::VideoToolbox, true),
        cap(Codec::H264, Backend::VideoToolbox, true),
    ]);
    let sel = Selection::Pinned(Backend::Libaom);
    let base = base_backend(&r, sel).unwrap();
    assert_eq!(candidates_for(&r, sel, base), vec![LIBAOM]);
}

#[cfg(feature = "libaom")]
#[test]
fn the_base_chain_falls_back_from_hardware_to_libaom_without_x264() {
    let r = reg(vec![
        cap(Codec::H264, Backend::VideoToolbox, true),
        cap(Codec::Av1, Backend::Libaom, false),
    ]);
    let base = base_backend(&r, Selection::Auto).unwrap();
    assert_eq!(base, hw264(Backend::VideoToolbox));
    assert_eq!(base_chain(&candidates(&r, base)), vec![base, LIBAOM]);
}

#[test]
fn a_pinned_backend_streams_its_base_codec() {
    #[cfg(feature = "libaom")]
    assert_eq!(base_codec(Backend::Libaom), Codec::Av1);
    #[cfg(feature = "x264")]
    assert_eq!(base_codec(Backend::X264), Codec::H264);
    assert_eq!(base_codec(Backend::FakeAv1), Codec::Av1);
    assert_eq!(base_codec(Backend::Nvenc), Codec::H264);
}

#[cfg(feature = "libaom")]
#[test]
fn libaom_alone_is_the_base_chain() {
    let r = reg(vec![cap(Codec::Av1, Backend::Libaom, false)]);
    let auto = base_backend(&r, Selection::Auto);
    if cfg!(feature = "x264") {
        assert!(auto.unwrap_err().contains("no x264 H264 encoder"));
    } else {
        assert_eq!(auto, Ok(LIBAOM));
    }
    let base = base_backend(&r, Selection::Pinned(Backend::Libaom)).unwrap();
    assert_eq!(base, LIBAOM);
    assert_eq!(
        candidates_for(&r, Selection::Pinned(Backend::Libaom), base),
        vec![LIBAOM]
    );
    assert_eq!(base_chain(&candidates(&r, base)), vec![LIBAOM]);
}

#[test]
fn a_legacy_viewer_gets_nothing_while_the_base_is_av1() {
    let o = vec![offer(FAKE_AV1, "av01.0.13M.08", &[(0, 1600)], 2560)];
    let a = [av1()];
    let v = [
        Viewer {
            id: 1,
            legacy: true,
            caps: &[],
            rejected: &[],
        },
        viewer(2, &a),
    ];
    let p = plan(&v, &o, FAKE_AV1, None);
    assert_eq!(p.best, None);
    assert_eq!(
        p.picks,
        vec![
            (1, None),
            (2, Some(choice(FAKE_AV1, Implementation::WebCodecs)))
        ]
    );
}

#[test]
fn both_rank_orders_on_a_host_with_hardware() {
    let r = reg(vec![
        cap(Codec::H264, Backend::VideoToolbox, true),
        cap(Codec::Hevc, Backend::VideoToolbox, true),
        cap(Codec::Av1, Backend::FakeAv1, false),
    ]);
    let base = hw264(Backend::VideoToolbox);
    assert_eq!(
        pairs(&ranked(&r, base, true)),
        vec![
            (Codec::Hevc, Backend::VideoToolbox),
            (Codec::H264, Backend::VideoToolbox),
            (Codec::Av1, Backend::FakeAv1),
        ],
        "hardware first: the software AV1 encoder ranks last"
    );
    assert_eq!(
        pairs(&ranked(&r, base, false)),
        vec![
            (Codec::Av1, Backend::FakeAv1),
            (Codec::Hevc, Backend::VideoToolbox),
            (Codec::H264, Backend::VideoToolbox),
        ],
        "codec first: the software AV1 encoder leads"
    );
    assert_eq!(candidates(&r, base), ranked(&r, base, HARDWARE_FIRST));
}

#[test]
fn both_rank_orders_on_a_host_without_hardware() {
    let r = reg(vec![
        cap(Codec::Av1, Backend::FakeAv1, false),
        cap(Codec::H264, Backend::FakeH264, false),
    ]);
    for hardware_first in [true, false] {
        assert_eq!(
            ranked(&r, H264, hardware_first),
            vec![FAKE_AV1, H264],
            "hardware first: {hardware_first}"
        );
    }
}
