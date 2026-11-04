use super::*;
use crate::encoder::{AccessUnit, Backend, BitstreamFormat, ColorInfo};
use crate::negotiate::Implementation;
use std::sync::{Condvar, Mutex as StdMutex};
use tokio::sync::mpsc::UnboundedReceiver;

const W: u32 = 640;
const H: u32 = 480;

const H264: Candidate = Candidate {
    codec: Codec::H264,
    backend: Backend::OpenH264,
    hardware: false,
};
const HEVC: Candidate = Candidate {
    codec: Codec::Hevc,
    backend: Backend::VideoToolbox,
    hardware: true,
};
const AV1: Candidate = Candidate {
    codec: Codec::Av1,
    backend: Backend::Nvenc,
    hardware: true,
};

/// Holds every HEVC encode until opened.
#[derive(Default)]
struct Gate {
    open: StdMutex<bool>,
    cv: Condvar,
}

impl Gate {
    fn set(&self, open: bool) {
        *self.open.lock().unwrap() = open;
        self.cv.notify_all();
    }
    fn wait(&self) {
        let mut g = self.open.lock().unwrap();
        while !*g {
            g = self.cv.wait(g).unwrap();
        }
    }
}

/// Emits a keyframe on request (and first), else a delta, with the configuration of
/// its codec on its first unit and after `relevel` encodes a new level.
struct Fake {
    codec: Codec,
    coded: (u32, u32),
    n: usize,
    relevel: Option<usize>,
    /// Every encode from this one on fails.
    fail_from: Option<usize>,
    /// Only this encode fails.
    fail_at: Option<usize>,
    gate: Option<Arc<Gate>>,
}

fn codec_string(c: Codec, level: u32) -> String {
    match c {
        Codec::H264 => format!("avc1.42c0{level:02x}"),
        Codec::Hevc => format!("hvc1.1.6.L{level}.B0"),
        Codec::Av1 => format!("av01.0.{level:02}M.08"),
    }
}

impl VideoEncoder for Fake {
    fn set_rate(&mut self, _: RateConfig) -> Result<(), EncodeError> {
        Ok(())
    }

    fn encode(
        &mut self,
        _: &FrameView<'_>,
        opts: EncodeOptions<'_>,
    ) -> Result<Vec<AccessUnit>, EncodeError> {
        if let Some(g) = &self.gate {
            g.wait();
        }
        self.n += 1;
        if self.fail_from.is_some_and(|f| self.n >= f) || self.fail_at == Some(self.n) {
            return Err(EncodeError("session lost".into()));
        }
        let relevel = self.relevel == Some(self.n);
        let key = opts.force_keyframe || self.n == 1 || relevel;
        let level = if self.relevel.is_some_and(|r| self.n >= r) {
            40
        } else {
            30
        };
        Ok(vec![AccessUnit {
            pts_ms: opts.pts_ms,
            keyframe: key,
            data: vec![0, 0, 0, 1, if key { 0x65 } else { 0x41 }, self.n as u8],
            config: (self.n == 1 || relevel).then(|| StreamConfig {
                codec: self.codec,
                codec_string: codec_string(self.codec, level),
                coded: self.coded,
                display: self.coded,
                format: BitstreamFormat::AnnexB,
                color: ColorInfo::SRGB_BT601_LIMITED,
            }),
        }])
    }

    fn drain(&mut self) -> Result<Vec<AccessUnit>, EncodeError> {
        Ok(Vec::new())
    }
}

#[derive(Default)]
struct Opts {
    gate: Option<Arc<Gate>>,
    /// HEVC cohorts fail to open (the offer's sample encode still works).
    hevc_fails: bool,
    /// The HEVC stream changes level on its 3rd picture.
    hevc_relevel: bool,
    /// HEVC cohorts' encoders fail from their 3rd picture on.
    hevc_breaks: bool,
    /// The second HEVC stripe's encoder fails from its 2nd picture on.
    hevc_second_stripe_breaks: bool,
    /// The first HEVC cohort encoder fails its 2nd picture only.
    hevc_fails_once: bool,
}

fn fakes(o: Opts) -> Open {
    let opened = Arc::new(AtomicUsize::new(0));
    Arc::new(move |c: &EncoderConfig| {
        let hevc = c.codec == Codec::Hevc;
        let earlier = if hevc {
            opened.fetch_add(1, Ordering::Relaxed)
        } else {
            0
        };
        if o.hevc_fails && earlier > 0 {
            return Err(EncodeError("no session".into()));
        }
        Ok(Box::new(Fake {
            codec: c.codec,
            coded: (c.width, c.height),
            n: 0,
            relevel: (hevc && o.hevc_relevel).then_some(3),
            fail_from: if hevc && o.hevc_breaks && earlier > 0 {
                Some(3)
            } else if hevc && o.hevc_second_stripe_breaks && earlier == 2 {
                Some(2)
            } else {
                None
            },
            fail_at: (hevc && o.hevc_fails_once && earlier == 1).then_some(2),
            gate: o.gate.clone().filter(|_| hevc),
        }) as Box<dyn VideoEncoder>)
    })
}

fn config() -> VideoConfig {
    VideoConfig {
        display: None,
        encoder: Backend::OpenH264,
        fps: 30,
        preset: "ultrafast".into(),
        threads: 1,
        convert_threads: 1,
        crf: 24.0,
        max_kbps: 30000,
        paintover_crf: 16.0,
        paintover_burst: 3,
        paintover_delay_ms: 150,
        min_kbps: 2000,
        coalesce: Duration::ZERO,
        stripes: 1,
        adaptive_threads: false,
    }
}

fn engine(hub: &Arc<Hub>, cands: &[Candidate], o: Opts) -> Engine {
    Engine::new(config(), hub.clone(), W, H, cands, fakes(o)).unwrap()
}

fn client(hub: &Hub) -> (Arc<Client>, UnboundedReceiver<Out>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    (hub.add_client(tx), rx)
}

/// Answers the current offers with WebCodecs support for `codecs`, and turns video on.
fn join_v2(hub: &Hub, c: &Client, codecs: &[Codec]) {
    let caps = hub
        .offers
        .lock()
        .iter()
        .filter(|o| codecs.contains(&o.codec))
        .flat_map(|o| o.stripes.iter())
        .map(|s| DecodeCap {
            codec: s.config.codec.clone(),
            coded_width: s.config.coded_width,
            coded_height: s.config.coded_height,
            implementation: Implementation::WebCodecs,
            hardware_acceleration: Some("prefer-hardware".into()),
        })
        .collect();
    c.caps_hello(caps, hub.offer_gen.load(Ordering::Relaxed));
    c.set_video(true);
    hub.replan();
}

fn join_legacy(hub: &Hub, c: &Client) {
    c.set_video(true);
    hub.replan();
}

fn whole(w: u32, h: u32) -> Frame {
    Frame {
        buf: Arc::new(I420::new(w as usize, h as usize)),
        paintover: false,
        paintover_final: false,
        band: Some(Band { y0: 0, y1: h }),
        mb_dirty: None,
        captured_at: Instant::now(),
        marker_ms: -1,
    }
}

/// One capture through the capture loop's steps, with `band` changed; returns once
/// the cohorts are done with it.
fn push_band(eng: &mut Engine, w: u32, h: u32, band: Option<Band>) {
    eng.replan();
    let views = eng.views();
    eng.update_rates(&views);
    let takes = eng.takes(&views, 33.0);
    let mut f = whole(w, h);
    f.band = band;
    eng.dispatch(&f, &views, &takes);
    settle(eng);
}

fn push_at(eng: &mut Engine, w: u32, h: u32) {
    push_band(eng, w, h, Some(Band { y0: 0, y1: h }))
}

fn push(eng: &mut Engine) {
    push_at(eng, W, H)
}

fn settle(eng: &Engine) {
    let t = Instant::now();
    while eng
        .cohorts
        .iter()
        .any(|c| c.shared.busy.load(Ordering::Acquire) > 0)
    {
        assert!(t.elapsed() < Duration::from_secs(10), "encoder stuck");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[derive(Debug)]
enum Msg {
    Json(serde_json::Value),
    /// Type byte, keyframe, frame id, epoch (protocol 2).
    Video(u8, bool, u32, Option<u32>),
}

fn drain(rx: &mut UnboundedReceiver<Out>) -> Vec<Msg> {
    let mut v = Vec::new();
    while let Ok(m) = rx.try_recv() {
        v.push(match m {
            Out::Text(t) => Msg::Json(serde_json::from_str(&t).unwrap()),
            Out::Bin(b) => {
                let id = u32::from_be_bytes([b[2], b[3], b[4], b[5]]);
                let epoch = (b[0] == negotiate::VIDEO_V2)
                    .then(|| u32::from_be_bytes([b[16], b[17], b[18], b[19]]));
                Msg::Video(b[0], b[1] & 1 == 1, id, epoch)
            }
        });
    }
    v
}

fn codec_msgs(m: &[Msg]) -> Vec<&serde_json::Value> {
    m.iter()
        .filter_map(|m| match m {
            Msg::Json(j) if j["t"] == "codec" => Some(j),
            _ => None,
        })
        .collect()
}

fn videos(m: &[Msg]) -> Vec<(u8, bool, u32, Option<u32>)> {
    m.iter()
        .filter_map(|m| match m {
            Msg::Video(t, k, id, e) => Some((*t, *k, *id, *e)),
            _ => None,
        })
        .collect()
}

fn ready(c: &Client, m: &[Msg]) -> u32 {
    let e = codec_msgs(m).last().expect("a codec message")["epoch"]
        .as_u64()
        .unwrap() as u32;
    assert!(c.codec_ready(e));
    e
}

fn cohort_of(c: &Client) -> Option<u64> {
    c.route.lock().cohort
}

#[test]
fn legacy_viewers_get_protocol_1_h264() {
    let hub = Hub::new();
    let mut eng = engine(&hub, &[HEVC, H264], Opts::default());
    let (c, mut rx) = client(&hub);
    join_legacy(&hub, &c);
    for _ in 0..3 {
        push(&mut eng);
    }
    let m = drain(&mut rx);
    assert!(codec_msgs(&m).is_empty());
    let v = videos(&m);
    assert_eq!(v.len(), 3);
    assert!(v
        .iter()
        .all(|f| f.0 == negotiate::VIDEO_V1 && f.3.is_none()));
    assert!(v[0].1, "starts with a keyframe");
    assert_eq!(
        eng.cohorts.len(),
        1,
        "no HEVC cohort without a viewer for it"
    );
    assert!(c.ack(None, v[2].2));
    assert!(
        !c.ack(Some(0), v[2].2),
        "no protocol 2 acks from a protocol 1 viewer"
    );
}

#[test]
fn hello_offers_every_candidate() {
    let hub = Hub::new();
    let _eng = engine(&hub, &[HEVC, H264], Opts::default());
    let h: serde_json::Value = serde_json::from_str(&hub.hello()).unwrap();
    assert_eq!(h["protocol"], 2);
    assert_eq!(h["offer"], 1);
    assert_eq!(h["stripes"], serde_json::json!([[0, H]]));
    let o = h["videoOffers"].as_array().unwrap();
    assert_eq!(o.len(), 2);
    assert_eq!(o[0]["codec"], "hevc");
    assert_eq!(o[0]["stripes"][0]["config"]["codec"], "hvc1.1.6.L30.B0");
    assert_eq!(o[1]["codec"], "h264");
    assert_eq!(o[1]["stripes"][0]["config"]["codedWidth"], W);
}

#[test]
fn v2_viewer_is_announced_then_gets_epoch_frames() {
    let hub = Hub::new();
    let mut eng = engine(&hub, &[HEVC, H264], Opts::default());
    let (c, mut rx) = client(&hub);
    join_v2(&hub, &c, &[Codec::Hevc, Codec::H264]);
    push(&mut eng);
    let m = drain(&mut rx);
    let cm = codec_msgs(&m);
    assert_eq!(cm.len(), 1);
    assert_eq!(cm[0]["codec"], "hevc");
    assert_eq!(cm[0]["epoch"], 1);
    assert_eq!(cm[0]["stripes"][0]["config"]["codec"], "hvc1.1.6.L30.B0");
    assert!(videos(&m).is_empty(), "no frames before codec-ready");
    assert!(!c.codec_ready(2), "a future epoch");
    assert!(c.codec_ready(1));
    assert!(!c.codec_ready(1), "twice");
    push(&mut eng);
    push(&mut eng);
    let v = videos(&drain(&mut rx));
    assert_eq!(v.len(), 2);
    assert!(v
        .iter()
        .all(|f| f.0 == negotiate::VIDEO_V2 && f.3 == Some(1)));
    assert!(v[0].1, "the first frame after ready is a keyframe");
    let s = route_codec(&c);
    assert_eq!(s, Some(Codec::Hevc));
}

fn route_codec(c: &Client) -> Option<Codec> {
    c.route.lock().choice.map(|c| c.cand.codec)
}

#[test]
fn stale_epochs_are_rejected() {
    let hub = Hub::new();
    let mut eng = engine(&hub, &[HEVC, H264], Opts::default());
    let (c, mut rx) = client(&hub);
    join_v2(&hub, &c, &[Codec::Hevc, Codec::H264]);
    push(&mut eng);
    ready(&c, &drain(&mut rx));
    push(&mut eng);
    let v = videos(&drain(&mut rx));
    let last = v.last().unwrap().2;
    assert!(c.ack(Some(1), last));
    // A switch (here: the test hook's renegotiation) makes epoch 1 stale.
    c.route.lock().detach();
    hub.replan();
    push(&mut eng);
    let m = drain(&mut rx);
    assert_eq!(codec_msgs(&m)[0]["epoch"], 2);
    assert!(videos(&m).is_empty());
    assert!(!c.ack(Some(1), last), "ack for the old epoch");
    assert!(
        !c.request_keyframe(Some(1)),
        "keyframe request for the old epoch"
    );
    assert!(!c.ack(Some(2), last), "ack before ready");
    assert!(c.codec_error(1).is_none(), "error for the old epoch");
    assert!(!c.codec_ready(1));
    assert!(c.codec_ready(2));
    assert!(!c.ack(None, last), "a protocol 1 ack");
    push(&mut eng);
    let v = videos(&drain(&mut rx));
    assert!(v[0].1 && v[0].3 == Some(2));
    assert!(c.ack(Some(2), v[0].2));
    assert!(c.request_keyframe(Some(2)));
}

#[test]
fn a_switch_mid_frame_sends_nothing_of_the_old_cohort() {
    let gate = Arc::new(Gate::default());
    gate.set(true);
    let hub = Hub::new();
    let mut eng = engine(
        &hub,
        &[HEVC, H264],
        Opts {
            gate: Some(gate.clone()),
            ..Default::default()
        },
    );
    let (c, mut rx) = client(&hub);
    join_v2(&hub, &c, &[Codec::Hevc, Codec::H264]);
    push(&mut eng);
    ready(&c, &drain(&mut rx));
    push(&mut eng);
    assert_eq!(videos(&drain(&mut rx)).len(), 1);
    let hevc = cohort_of(&c).unwrap();
    // The HEVC encoder is in the middle of a picture when the viewer reports a decoder
    // error and is moved to H.264.
    gate.set(false);
    eng.replan();
    let views = eng.views();
    let takes = eng.takes(&views, 33.0);
    eng.dispatch(&whole(W, H), &views, &takes);
    assert_eq!(c.codec_error(1).map(|c| c.cand.codec), Some(Codec::Hevc));
    // The picture finishes after the switch (closing the HEVC cohort waits for it).
    let g = gate.clone();
    let opener = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        g.set(true);
    });
    hub.replan();
    eng.replan();
    opener.join().unwrap();
    settle(&eng);
    let m = drain(&mut rx);
    assert!(
        videos(&m).is_empty(),
        "the old cohort's frame reached the viewer: {m:?}"
    );
    let cm = codec_msgs(&m);
    assert_eq!(cm.len(), 1);
    assert_eq!(
        (cm[0]["codec"].as_str(), cm[0]["epoch"].as_u64()),
        (Some("h264"), Some(2))
    );
    assert_ne!(cohort_of(&c), Some(hevc));
    assert!(c.codec_ready(2));
    push(&mut eng);
    let v = videos(&drain(&mut rx));
    assert!(v[0].1 && v[0].3 == Some(2));
    assert_eq!(
        eng.cohorts.len(),
        1,
        "the HEVC cohort closed with its last viewer"
    );
}

#[test]
fn two_viewers_two_cohorts_and_a_third_joins_the_closest() {
    let hub = Hub::new();
    let mut eng = engine(&hub, &[AV1, HEVC, H264], Opts::default());
    let (a, mut ra) = client(&hub);
    let (b, mut rb) = client(&hub);
    let (c, mut rc) = client(&hub);
    join_v2(&hub, &a, &[Codec::Hevc, Codec::H264]);
    join_v2(&hub, &b, &[Codec::H264]);
    push(&mut eng);
    assert_eq!(eng.cohorts.len(), 2);
    assert_eq!(route_codec(&a), Some(Codec::Hevc));
    assert_eq!(route_codec(&b), Some(Codec::H264));
    join_v2(&hub, &c, &[Codec::Av1, Codec::Hevc, Codec::H264]);
    push(&mut eng);
    assert_eq!(eng.cohorts.len(), 2, "never a third cohort");
    assert_eq!(route_codec(&c), Some(Codec::Hevc));
    assert_eq!(cohort_of(&c), cohort_of(&a));
    for (x, r) in [(&a, &mut ra), (&b, &mut rb), (&c, &mut rc)] {
        ready(x, &drain(r));
    }
    push(&mut eng);
    let (va, vb, vc) = (
        videos(&drain(&mut ra)),
        videos(&drain(&mut rb)),
        videos(&drain(&mut rc)),
    );
    assert_eq!((va.len(), vb.len(), vc.len()), (1, 1, 1));
    assert_eq!(va[0].2, vc[0].2, "A and C share the HEVC stream");
    let stats = hub.cohorts.lock().clone();
    assert_eq!(stats.as_array().unwrap().len(), 2);
    // A leaves: HEVC was only A's first choice, so C gets AV1 (a new epoch).
    hub.remove_client(a.id);
    push(&mut eng);
    assert_eq!(route_codec(&c), Some(Codec::Av1));
    let cm = drain(&mut rc);
    assert_eq!(codec_msgs(&cm)[0]["codec"], "av1");
    assert_eq!(eng.cohorts.len(), 2);
    assert!(eng.cohorts.iter().all(|k| k.shared.cand != HEVC));
}

#[test]
fn legacy_and_v2_share_the_h264_cohort() {
    let hub = Hub::new();
    let mut eng = engine(&hub, &[H264], Opts::default());
    let (l, mut rl) = client(&hub);
    let (n, mut rn) = client(&hub);
    join_legacy(&hub, &l);
    join_v2(&hub, &n, &[Codec::H264]);
    push(&mut eng);
    ready(&n, &drain(&mut rn));
    push(&mut eng);
    let (vl, vn) = (videos(&drain(&mut rl)), videos(&drain(&mut rn)));
    assert_eq!(cohort_of(&l), cohort_of(&n));
    assert_eq!(vl.last().unwrap().0, negotiate::VIDEO_V1);
    assert_eq!(vn.last().unwrap().0, negotiate::VIDEO_V2);
    assert_eq!(vl.last().unwrap().2, vn.last().unwrap().2);
}

#[test]
fn an_encoder_that_fails_to_open_falls_back() {
    let hub = Hub::new();
    let mut eng = engine(
        &hub,
        &[HEVC, H264],
        Opts {
            hevc_fails: true,
            ..Default::default()
        },
    );
    let (c, mut rx) = client(&hub);
    join_v2(&hub, &c, &[Codec::Hevc, Codec::H264]);
    push(&mut eng);
    let m = drain(&mut rx);
    let cm = codec_msgs(&m);
    assert_eq!(cm.len(), 1);
    assert_eq!(cm[0]["codec"], "h264");
    assert_eq!(eng.failed, vec![HEVC]);
    // A second viewer doesn't try HEVC again.
    let (d, _rd) = client(&hub);
    join_v2(&hub, &d, &[Codec::Hevc, Codec::H264]);
    push(&mut eng);
    assert_eq!(route_codec(&d), Some(Codec::H264));
}

#[test]
fn errors_and_timeouts_walk_down_to_no_codec() {
    let hub = Hub::new();
    let mut eng = engine(&hub, &[HEVC, H264], Opts::default());
    let (c, mut rx) = client(&hub);
    join_v2(&hub, &c, &[Codec::Hevc, Codec::H264]);
    push(&mut eng);
    assert_eq!(codec_msgs(&drain(&mut rx))[0]["codec"], "hevc");
    assert!(c.codec_error(1).is_some());
    hub.replan();
    push(&mut eng);
    assert_eq!(codec_msgs(&drain(&mut rx))[0]["codec"], "h264");
    // No codec-ready in time.
    c.route.lock().phase = Phase::Announced(Instant::now() - READY_TIMEOUT);
    eng.last_tick = Instant::now() - TICK;
    push(&mut eng);
    let m = drain(&mut rx);
    let cm = codec_msgs(&m);
    assert_eq!(cm[0]["error"], "no-common-codec");
    assert_eq!(c.route.lock().phase, Phase::NoCodec);
    // Said once, not on every replan.
    hub.replan();
    push(&mut eng);
    assert!(codec_msgs(&drain(&mut rx)).is_empty());
    // A new capability hello starts over.
    join_v2(&hub, &c, &[Codec::Hevc, Codec::H264]);
    push(&mut eng);
    assert_eq!(codec_msgs(&drain(&mut rx))[0]["codec"], "hevc");
}

#[test]
fn no_common_codec_and_stale_offers() {
    let hub = Hub::new();
    let mut eng = engine(&hub, &[H264], Opts::default());
    let (c, mut rx) = client(&hub);
    c.caps_hello(Vec::new(), hub.offer_gen.load(Ordering::Relaxed));
    c.set_video(true);
    hub.replan();
    push(&mut eng);
    assert_eq!(codec_msgs(&drain(&mut rx))[0]["error"], "no-common-codec");
    // Capabilities for an older offer generation wait for the answer to the new one.
    let (d, mut rd) = client(&hub);
    d.caps_hello(Vec::new(), 0);
    d.set_video(true);
    hub.replan();
    push(&mut eng);
    assert!(drain(&mut rd).is_empty());
    assert_eq!(d.route.lock().phase, Phase::Idle);
}

#[test]
fn a_changed_stream_configuration_is_a_new_epoch() {
    let hub = Hub::new();
    let mut eng = engine(
        &hub,
        &[HEVC, H264],
        Opts {
            hevc_relevel: true,
            ..Default::default()
        },
    );
    let (c, mut rx) = client(&hub);
    join_v2(&hub, &c, &[Codec::Hevc, Codec::H264]);
    push(&mut eng);
    ready(&c, &drain(&mut rx));
    push(&mut eng);
    push(&mut eng);
    push(&mut eng);
    let m = drain(&mut rx);
    let cm = codec_msgs(&m);
    assert_eq!(cm.len(), 1, "{m:?}");
    assert_eq!(cm[0]["epoch"], 2);
    assert_eq!(cm[0]["stripes"][0]["config"]["codec"], "hvc1.1.6.L40.B0");
    // Frames of the new configuration wait for the viewer to confirm it.
    let v = videos(&m);
    assert!(v.iter().all(|f| f.3 == Some(1)), "{v:?}");
    assert!(c.codec_ready(2));
    push(&mut eng);
    let v = videos(&drain(&mut rx));
    assert!(v[0].1 && v[0].3 == Some(2));
}

#[test]
fn a_slow_viewer_holds_back_only_its_cohort() {
    let hub = Hub::new();
    let mut eng = engine(&hub, &[HEVC, H264], Opts::default());
    let (slow, mut rs) = client(&hub);
    let (fast, mut rf) = client(&hub);
    join_v2(&hub, &slow, &[Codec::Hevc, Codec::H264]);
    join_legacy(&hub, &fast);
    push(&mut eng);
    ready(&slow, &drain(&mut rs));
    for v in videos(&drain(&mut rf)) {
        assert!(fast.ack(None, v.2));
    }
    let mut fast_frames = 0;
    for _ in 0..40 {
        push(&mut eng);
        for v in videos(&drain(&mut rf)) {
            assert!(fast.ack(None, v.2));
            fast_frames += 1;
        }
    }
    let slow_frames = videos(&drain(&mut rs)).len();
    assert_eq!(fast_frames, 40, "the acking viewer got every frame");
    assert!(
        (1..10).contains(&slow_frames),
        "the viewer that never acks got {slow_frames} frames"
    );
    let hevc = &eng.cohorts.iter().find(|k| k.shared.cand == HEVC).unwrap();
    assert!(
        hevc.skipped.is_some(),
        "its cohort keeps the damage it skipped"
    );
    // Once it catches up, the damage it skipped is due even if nothing else changes.
    assert!(!eng.catch_up_due(&eng.views(), 33.0));
    slow.inflight.lock().clear();
    assert!(eng.catch_up_due(&eng.views(), 33.0));
    let views = eng.views();
    let takes = eng.takes(&views, 33.0);
    let mut still = whole(W, H);
    still.band = None;
    eng.dispatch(&still, &views, &takes);
    settle(&eng);
    assert_eq!(
        videos(&drain(&mut rs)).len(),
        1,
        "the skipped damage was sent"
    );
    let hevc = &eng.cohorts.iter().find(|k| k.shared.cand == HEVC).unwrap();
    assert!(hevc.skipped.is_none());
    assert!(!eng.catch_up_due(&eng.views(), 33.0));
}

#[test]
fn an_encoder_that_keeps_failing_falls_back() {
    let hub = Hub::new();
    let mut eng = engine(
        &hub,
        &[HEVC, H264],
        Opts {
            hevc_breaks: true,
            ..Default::default()
        },
    );
    let (c, mut rx) = client(&hub);
    join_v2(&hub, &c, &[Codec::Hevc, Codec::H264]);
    push(&mut eng);
    ready(&c, &drain(&mut rx));
    for _ in 0..(2 + FAIL_AFTER) {
        push(&mut eng);
    }
    push(&mut eng);
    let m = drain(&mut rx);
    let cm = codec_msgs(&m);
    assert_eq!(cm.len(), 1, "{m:?}");
    assert_eq!(cm[0]["codec"], "h264");
    assert_eq!(route_codec(&c), Some(Codec::H264));
    assert!(eng.failed.contains(&HEVC));
    assert!(eng.cohorts.iter().all(|k| k.shared.cand != HEVC));
}

const HEVC_SW: Candidate = Candidate {
    codec: Codec::Hevc,
    backend: Backend::VideoToolbox,
    hardware: false,
};

#[test]
fn a_failing_stripe_is_not_hidden_by_the_others() {
    let hub = Hub::new();
    let cfg = VideoConfig {
        stripes: 2,
        ..config()
    };
    let o = Opts {
        hevc_second_stripe_breaks: true,
        ..Default::default()
    };
    let mut eng = Engine::new(cfg, hub.clone(), W, H, &[HEVC_SW, H264], fakes(o)).unwrap();
    assert_eq!(eng.offers[0].stripes.len(), 2);
    let (c, mut rx) = client(&hub);
    join_v2(&hub, &c, &[Codec::Hevc, Codec::H264]);
    push(&mut eng);
    ready(&c, &drain(&mut rx));
    push(&mut eng);
    assert!(videos(&drain(&mut rx))[0].1, "both stripes start");
    // The bottom stripe's session breaks; afterwards only the top one changes. Its
    // successes don't clear the bottom one's failures, and each failure asks for a
    // keyframe, which tries the bottom one again.
    push(&mut eng);
    let top = Some(Band { y0: 0, y1: H / 2 });
    for _ in 0..=FAIL_AFTER {
        push_band(&mut eng, W, H, top);
    }
    assert_eq!(route_codec(&c), Some(Codec::H264));
    assert!(eng.failed.contains(&HEVC_SW));
}

#[test]
fn a_failed_update_is_redone() {
    let hub = Hub::new();
    let mut eng = engine(
        &hub,
        &[HEVC, H264],
        Opts {
            hevc_fails_once: true,
            ..Default::default()
        },
    );
    let (c, mut rx) = client(&hub);
    join_v2(&hub, &c, &[Codec::Hevc, Codec::H264]);
    push(&mut eng);
    ready(&c, &drain(&mut rx));
    push(&mut eng);
    push(&mut eng);
    assert_eq!(
        videos(&drain(&mut rx)).len(),
        1,
        "the second picture failed"
    );
    // Nothing else changes on screen, yet the cohort owes a keyframe.
    assert!(eng.views().iter().any(|v| v.want_key));
    push_band(&mut eng, W, H, None);
    let v = videos(&drain(&mut rx));
    assert_eq!(v.len(), 1);
    assert!(v[0].1, "redone as a keyframe");
    assert_eq!(
        route_codec(&c),
        Some(Codec::Hevc),
        "one failure is not a fallback"
    );
}

#[test]
fn announcements_carry_the_cohorts_current_configuration() {
    let hub = Hub::new();
    let mut eng = engine(&hub, &[H264], Opts::default());
    eng.cohorts[0].shared.offer.lock().stripes[0].config.codec = "avc1.42c034".into();
    let (c, mut rx) = client(&hub);
    join_v2(&hub, &c, &[Codec::H264]);
    push(&mut eng);
    let m = drain(&mut rx);
    assert_eq!(
        codec_msgs(&m)[0]["stripes"][0]["config"]["codec"],
        "avc1.42c034"
    );
}

#[test]
fn video_off_and_on_is_a_new_epoch() {
    let hub = Hub::new();
    let mut eng = engine(&hub, &[H264], Opts::default());
    let (c, mut rx) = client(&hub);
    join_v2(&hub, &c, &[Codec::H264]);
    push(&mut eng);
    ready(&c, &drain(&mut rx));
    push(&mut eng);
    assert_eq!(videos(&drain(&mut rx)).len(), 1);
    c.set_video(false);
    hub.replan();
    push(&mut eng);
    assert!(drain(&mut rx).is_empty());
    c.set_video(true);
    hub.replan();
    push(&mut eng);
    let m = drain(&mut rx);
    assert_eq!(codec_msgs(&m)[0]["epoch"], 2);
    assert!(videos(&m).is_empty());
}

#[test]
fn skipped_damage_is_caught_up() {
    let f = |band: Option<Band>, mb: Option<Vec<u8>>| Frame {
        buf: Arc::new(I420::new(32, 32)),
        paintover: false,
        paintover_final: false,
        band,
        mb_dirty: mb.map(Arc::new),
        captured_at: Instant::now(),
        marker_ms: -1,
    };
    let s = skip(
        None,
        &f(Some(Band { y0: 0, y1: 16 }), Some(vec![1, 0, 0, 0])),
    );
    let s = skip(Some(s), &f(None, Some(vec![0, 0, 1, 0])));
    let out = catch_up(s, f(Some(Band { y0: 16, y1: 32 }), Some(vec![0, 1, 0, 0])));
    let b = out.band.unwrap();
    assert_eq!((b.y0, b.y1), (0, 32));
    assert_eq!(out.mb_dirty.as_deref().unwrap().as_slice(), &[1, 1, 1, 0]);
    let s = skip(None, &f(None, None));
    assert!(catch_up(s, f(None, Some(vec![0; 4]))).mb_dirty.is_none());
}

#[test]
fn buffers_are_reused_once_the_encoders_let_go() {
    let full = Band { y0: 0, y1: 32 };
    let mut pool = Pool::new(32, 32, full);
    let mut converted = Vec::new();
    let i = pool.free().unwrap();
    let a = pool.fill(i, Some(Band { y0: 0, y1: 16 }), |_, b| {
        converted.push((b.y0, b.y1))
    });
    let j = pool.free().unwrap();
    assert_ne!(i, j, "a buffer an encoder holds is not free");
    let b = pool.fill(j, Some(Band { y0: 16, y1: 32 }), |_, b| {
        converted.push((b.y0, b.y1))
    });
    let mut held = vec![a, b];
    while let Some(k) = pool.free() {
        held.push(pool.fill(k, None, |_, _| {}));
    }
    assert_eq!(
        held.len(),
        MAX_BUFS,
        "the pool grows to its cap, no further"
    );
    drop(held);
    let k = pool.free().unwrap();
    assert!(k < 2);
    let _ = pool.fill(k, None, |_, b| converted.push((b.y0, b.y1)));
    // Each buffer converts what it missed: the whole screen at first, then the other's band.
    assert_eq!(converted[0], (0, 32));
    assert_eq!(converted[1], (0, 32));
    assert_eq!(converted[2], if k == 0 { (16, 32) } else { (0, 16) });
}

/// NAL unit types of an HEVC Annex-B access unit.
#[cfg(target_os = "macos")]
fn hevc_nal_types(d: &[u8]) -> Vec<u8> {
    (0..d.len().saturating_sub(3))
        .filter(|&i| d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1)
        .map(|i| (d[i + 3] >> 1) & 0x3f)
        .collect()
}

/// The kyora-macos runner: VideoToolbox HEVC is offered, a viewer that decodes it gets
/// an HEVC cohort, and its frames are HEVC (parameter sets and an IRAP picture first).
#[cfg(target_os = "macos")]
#[test]
fn videotoolbox_hevc_negotiated() {
    use crate::encoder::caps::{host_providers, Registry, PROBE_TIMEOUT};
    let reg = Registry::probe(host_providers(true, false), PROBE_TIMEOUT);
    let cands = negotiate::candidates(&reg, Backend::OpenH264);
    if !cands.iter().any(|c| c.codec == Codec::Hevc) {
        assert!(
            std::env::var_os("STARFOLD_EXPECT_HEVC").is_none(),
            "no eligible HEVC encoder: {:?}",
            reg.encoders
        );
        eprintln!("no eligible HEVC encoder on this Mac; skipped");
        return;
    }
    let hub = Hub::new();
    let open: Open = Arc::new(|c: &EncoderConfig| encoder::open(c));
    let mut eng = Engine::new(config(), hub.clone(), 1280, 720, &cands, open).unwrap();
    let (c, mut rx) = client(&hub);
    join_v2(&hub, &c, &[Codec::Hevc, Codec::H264]);
    push_at(&mut eng, 1280, 720);
    let m = drain(&mut rx);
    let cm = codec_msgs(&m);
    assert_eq!(cm[0]["codec"], "hevc", "{m:?}");
    let s = cm[0]["stripes"][0]["config"]["codec"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(s.starts_with("hvc1."), "{s}");
    assert_eq!(
        c.route.lock().choice.unwrap().cand.backend,
        Backend::VideoToolbox
    );
    assert!(c.codec_ready(1));
    // The first real picture may tell a configuration other than the sample's: that is
    // announced as a new epoch, which a viewer confirms like the first.
    let mut got = Vec::new();
    let mut epoch = 1;
    for _ in 0..20 {
        push_at(&mut eng, 1280, 720);
        while let Ok(m) = rx.try_recv() {
            match m {
                Out::Bin(b) => got.push(b),
                Out::Text(t) => {
                    let j: serde_json::Value = serde_json::from_str(&t).unwrap();
                    if j["t"] == "codec" {
                        println!("re-announced: {j}");
                        assert_eq!(j["codec"], "hevc");
                        epoch = j["epoch"].as_u64().unwrap() as u32;
                        assert!(c.codec_ready(epoch));
                    }
                }
            }
        }
    }
    let got: Vec<_> = got
        .into_iter()
        .filter(|b| b[16..20] == epoch.to_be_bytes())
        .collect();
    assert!(
        !got.is_empty(),
        "no frames in epoch {epoch}: encoded {}, keyframes {}, route {:?}",
        hub.stats.encoded.load(Ordering::Relaxed),
        hub.stats.keyframes.load(Ordering::Relaxed),
        c.route.lock().phase
    );
    let first = &got[0];
    assert_eq!(first[0], negotiate::VIDEO_V2);
    assert_eq!(first[1] & 1, 1, "starts with a keyframe");
    let types = hevc_nal_types(&first[20..]);
    assert!(
        types.contains(&32) && types.contains(&33) && types.contains(&34),
        "{types:?}"
    );
    assert!(types.iter().any(|t| (16..=21).contains(t)), "{types:?}");
    println!(
        "hevc negotiated: {s} from videotoolbox, {} frames, first NAL types {types:?}",
        got.len()
    );
}
