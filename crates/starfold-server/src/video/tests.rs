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
const H264_HW: Candidate = Candidate {
    codec: Codec::H264,
    backend: Backend::VideoToolbox,
    hardware: true,
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
    /// Hardware H.264 streams High profile, OpenH264 Constrained Baseline.
    high: bool,
    coded: (u32, u32),
    n: usize,
    relevel: Option<usize>,
    /// Every encode from this one on fails.
    fail_from: Option<usize>,
    /// Only this encode fails.
    fail_at: Option<usize>,
    gate: Option<Arc<Gate>>,
    /// Counts this encoder's requested keyframes: (all, at the keyframe budget).
    keys: Option<Arc<(AtomicUsize, AtomicUsize)>>,
    kbps: u32,
    /// Every encode's refine option and the rate it ran at.
    refines: Option<Refines>,
    /// A requested keyframe comes out as nothing at all.
    empty_keys: bool,
}

type Refines = Arc<StdMutex<Vec<(Option<Refine>, u32)>>>;

fn codec_string(c: Codec, high: bool, level: u32) -> String {
    match c {
        Codec::H264 if high => format!("avc1.6400{level:02x}"),
        Codec::H264 => format!("avc1.42c0{level:02x}"),
        Codec::Hevc => format!("hvc1.1.6.L{level}.B0"),
        Codec::Av1 => format!("av01.0.{level:02}M.08"),
    }
}

impl VideoEncoder for Fake {
    fn set_rate(&mut self, r: RateConfig) -> Result<(), EncodeError> {
        self.kbps = r.max_kbps;
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
        if let Some(r) = &self.refines {
            r.lock().unwrap().push((opts.refine, self.kbps));
        }
        if let (Some(k), true) = (&self.keys, opts.force_keyframe) {
            k.0.fetch_add(1, Ordering::Relaxed);
            if self.kbps >= 16_000 {
                k.1.fetch_add(1, Ordering::Relaxed);
            }
        }
        if self.empty_keys && opts.force_keyframe {
            return Ok(Vec::new());
        }
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
                codec_string: codec_string(self.codec, self.high, level),
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
    /// HEVC cohorts' encoders fail from their 3rd picture on, and so do reopened ones.
    hevc_breaks: bool,
    /// The second HEVC stripe's encoder fails from its 2nd picture on, and so do
    /// reopened ones.
    hevc_second_stripe_breaks: bool,
    /// The first HEVC cohort encoder fails its 2nd picture only.
    hevc_fails_once: bool,
    /// The second H.264 stripe's session fails every picture, and so does every one
    /// opened after it (or, with `h264_reopen_heals`, only that first one).
    h264_second_stripe_dead: bool,
    h264_reopen_heals: bool,
    /// Keyframes requested from the first H.264 stripe's encoder.
    h264_keys: Option<Arc<(AtomicUsize, AtomicUsize)>>,
    /// Hardware H.264 cohorts fail to open (the offer's sample encode still works).
    h264_hw_fails: bool,
    /// Hardware H.264 cohorts' encoders fail from their 3rd picture on, and so do
    /// reopened ones.
    h264_hw_breaks: bool,
    refines: Option<Refines>,
    /// H.264 cohorts' encoders drop every keyframe (the offer's sample encode still works).
    h264_empty_keys: bool,
}

fn fakes(o: Opts) -> Open {
    let opened = Arc::new(AtomicUsize::new(0));
    let h264_opened = Arc::new(AtomicUsize::new(0));
    let hw_opened = Arc::new(AtomicUsize::new(0));
    Arc::new(move |c: &EncoderConfig| {
        if c.codec == Codec::H264 && c.backend == Backend::VideoToolbox {
            let earlier = hw_opened.fetch_add(1, Ordering::Relaxed);
            if o.h264_hw_fails && earlier > 0 {
                return Err(EncodeError("no session".into()));
            }
            return Ok(Box::new(Fake {
                codec: c.codec,
                high: true,
                coded: (c.width, c.height),
                n: 0,
                relevel: None,
                fail_from: (o.h264_hw_breaks && earlier > 0).then_some(if earlier == 1 {
                    3
                } else {
                    1
                }),
                fail_at: None,
                gate: None,
                keys: None,
                kbps: 0,
                refines: o.refines.clone(),
                empty_keys: false,
            }) as Box<dyn VideoEncoder>);
        }
        let hevc = c.codec == Codec::Hevc;
        let earlier = if hevc {
            opened.fetch_add(1, Ordering::Relaxed)
        } else {
            0
        };
        // The sample's is 0, the cohort's stripes 1 and 2, reopened ones after.
        let h264 = if hevc {
            0
        } else {
            h264_opened.fetch_add(1, Ordering::Relaxed)
        };
        let dead = o.h264_second_stripe_dead && (h264 == 2 || (h264 > 2 && !o.h264_reopen_heals));
        if o.hevc_fails && earlier > 0 {
            return Err(EncodeError("no session".into()));
        }
        Ok(Box::new(Fake {
            codec: c.codec,
            high: false,
            coded: (c.width, c.height),
            n: 0,
            relevel: (hevc && o.hevc_relevel).then_some(3),
            // A broken backend: the sessions reopened after a failure fail at once.
            fail_from: if hevc && o.hevc_breaks && earlier > 0 {
                Some(if earlier == 1 { 3 } else { 1 })
            } else if hevc && o.hevc_second_stripe_breaks && earlier >= 2 {
                Some(if earlier == 2 { 2 } else { 1 })
            } else if dead {
                Some(1)
            } else {
                None
            },
            fail_at: (hevc && o.hevc_fails_once && earlier == 1).then_some(2),
            gate: o.gate.clone().filter(|_| hevc),
            keys: o.h264_keys.clone().filter(|_| !hevc && h264 == 1),
            kbps: 0,
            refines: o.refines.clone().filter(|_| !hevc),
            empty_keys: o.h264_empty_keys && !hevc && h264 > 0,
        }) as Box<dyn VideoEncoder>)
    })
}

fn config() -> VideoConfig {
    VideoConfig {
        display: None,
        selection: encoder::Selection::Auto,
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
        refine_qp: None,
        band: Some(Band { y0: 0, y1: h }),
        mb_dirty: None,
        captured_at: Instant::now(),
        marker_ms: -1,
    }
}

/// One turn of the capture loop at `now`: `band` changed, or nothing did. Returns once the
/// cohorts are done with it.
fn capture(eng: &mut Engine, w: u32, h: u32, band: Option<Band>, now: Instant) {
    eng.replan();
    let views = eng.views();
    eng.update_rates(&views);
    let turn = eng.turn(&views, 33.0, now);
    let mut f = whole(w, h);
    f.band = band;
    if band.is_some() {
        eng.mark(&views, None);
    }
    eng.dispatch(&f, &views, &turn, now);
    settle(eng);
}

fn push_band(eng: &mut Engine, w: u32, h: u32, band: Option<Band>) {
    capture(eng, w, h, band, Instant::now())
}

/// A turn with nothing new on screen, long enough after the last for any gap to be over.
fn still(eng: &mut Engine, t: &mut Instant) {
    *t += Duration::from_secs(60);
    capture(eng, W, H, None, *t);
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
    let turn = eng.turn(&views, 33.0, Instant::now());
    eng.dispatch(&whole(W, H), &views, &turn, Instant::now());
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
    assert!(!eng.catch_up_due(&eng.takes(&eng.views(), 33.0)));
    slow.inflight.lock().clear();
    assert!(eng.catch_up_due(&eng.takes(&eng.views(), 33.0)));
    let views = eng.views();
    let turn = eng.turn(&views, 33.0, Instant::now());
    let mut still = whole(W, H);
    still.band = None;
    eng.dispatch(&still, &views, &turn, Instant::now());
    settle(&eng);
    assert_eq!(
        videos(&drain(&mut rs)).len(),
        1,
        "the skipped damage was sent"
    );
    let hevc = &eng.cohorts.iter().find(|k| k.shared.cand == HEVC).unwrap();
    assert!(hevc.skipped.is_none());
    assert!(!eng.catch_up_due(&eng.takes(&eng.views(), 33.0)));
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

/// `c`'s messages, with every frame in them acked.
fn acked(c: &Client, rx: &mut UnboundedReceiver<Out>) -> Vec<Msg> {
    let m = drain(rx);
    for v in videos(&m) {
        c.ack(v.3, v.2);
    }
    m
}

#[test]
fn a_hardware_h264_encoder_that_keeps_failing_falls_back_to_openh264() {
    let hub = Hub::new();
    let mut eng = engine(
        &hub,
        &[H264_HW, H264],
        Opts {
            h264_hw_breaks: true,
            ..Default::default()
        },
    );
    assert_eq!(eng.h264, H264_HW);
    assert_eq!(hub.encoder.lock()["chosen"][0]["backend"], "videotoolbox");
    let (l, mut rl) = client(&hub);
    let (n, mut rn) = client(&hub);
    join_legacy(&hub, &l);
    join_v2(&hub, &n, &[Codec::H264]);
    push(&mut eng);
    acked(&l, &mut rl);
    let m = acked(&n, &mut rn);
    assert_eq!(codec_msgs(&m)[0]["backend"], "videotoolbox");
    ready(&n, &m);
    let mut m = Vec::new();
    for _ in 0..(3 + FAIL_AFTER) {
        push(&mut eng);
        acked(&l, &mut rl);
        m.extend(acked(&n, &mut rn));
    }
    assert_eq!(eng.h264, H264);
    assert!(eng.failed.contains(&H264_HW));
    assert!(eng.cohorts.iter().all(|k| k.shared.cand != H264_HW));
    assert_eq!(hub.encoder.lock()["mode"], "auto");
    assert_eq!(hub.encoder.lock()["chosen"][0]["backend"], "openh264");
    let cm = codec_msgs(&m);
    assert_eq!(cm.len(), 1, "{m:?}");
    assert_eq!(cm[0]["codec"], "h264");
    assert_eq!(cm[0]["backend"], "openh264");
    assert_eq!(cm[0]["hardware"], false);
    ready(&n, &m);
    assert_eq!(cohort_of(&l), cohort_of(&n));
    let (mut vl, mut vn) = (Vec::new(), Vec::new());
    for _ in 0..3 {
        push(&mut eng);
        vl.extend(videos(&acked(&l, &mut rl)));
        vn.extend(videos(&acked(&n, &mut rn)));
    }
    assert!(!vl.is_empty(), "the legacy viewer gets OpenH264 frames");
    assert!(vl.iter().all(|f| f.0 == negotiate::VIDEO_V1));
    assert!(
        !vn.is_empty() && vn[0].1,
        "the v2 viewer starts with a keyframe"
    );
}

#[test]
fn the_test_hook_fails_the_h264_encoder() {
    let hub = Hub::new();
    let mut eng = engine(&hub, &[H264_HW, H264], Opts::default());
    let (l, mut rl) = client(&hub);
    join_legacy(&hub, &l);
    push(&mut eng);
    assert!(videos(&acked(&l, &mut rl))[0].1);
    assert_eq!(eng.h264, H264_HW);
    let before = cohort_of(&l);
    hub.fail_h264.store(true, Ordering::Release);
    hub.replan();
    push(&mut eng);
    assert_eq!(eng.h264, H264);
    assert_ne!(cohort_of(&l), before);
    assert!(
        videos(&acked(&l, &mut rl))[0].1,
        "the new encoder starts with a keyframe"
    );
}

/// Answers the current offers with WebCodecs support for `backend`'s, and turns video on.
fn join_v2_from(hub: &Hub, c: &Client, backend: Backend) {
    let caps = hub
        .offers
        .lock()
        .iter()
        .filter(|o| o.cand.backend == backend)
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

#[test]
fn a_viewer_that_cant_decode_hardware_h264_gets_openh264() {
    let hub = Hub::new();
    let mut eng = engine(
        &hub,
        &[H264_HW, H264],
        Opts {
            h264_hw_breaks: true,
            ..Default::default()
        },
    );
    let (l, mut rl) = client(&hub);
    let (n, mut rn) = client(&hub);
    join_legacy(&hub, &l);
    join_v2_from(&hub, &n, Backend::OpenH264);
    push(&mut eng);
    acked(&l, &mut rl);
    let m = acked(&n, &mut rn);
    let cm = codec_msgs(&m);
    assert_eq!(cm[0]["backend"], "openh264", "{m:?}");
    assert_eq!(cm[0]["stripes"][0]["config"]["codec"], "avc1.42c01e");
    ready(&n, &m);
    assert_eq!(eng.cohorts.len(), 2);
    assert_ne!(cohort_of(&l), cohort_of(&n));
    let openh264 = cohort_of(&n);
    // The hardware one fails: its viewers join the OpenH264 cohort already running.
    for _ in 0..(3 + FAIL_AFTER) {
        push(&mut eng);
        acked(&l, &mut rl);
        acked(&n, &mut rn);
    }
    assert_eq!(eng.h264, H264);
    assert_eq!(eng.best, None);
    assert_eq!(eng.cohorts.len(), 1);
    assert_eq!(cohort_of(&l), openh264);
    assert_eq!(cohort_of(&n), openh264);
    let mut vl = Vec::new();
    for _ in 0..3 {
        push(&mut eng);
        vl.extend(videos(&acked(&l, &mut rl)));
        acked(&n, &mut rn);
    }
    assert!(!vl.is_empty());
}

#[test]
fn a_hardware_h264_encoder_that_fails_to_open_falls_back() {
    let hub = Hub::new();
    let mut eng = engine(
        &hub,
        &[H264_HW, H264],
        Opts {
            h264_hw_fails: true,
            ..Default::default()
        },
    );
    assert_eq!(eng.h264, H264);
    assert_eq!(eng.failed, vec![H264_HW]);
    let (l, mut rl) = client(&hub);
    join_legacy(&hub, &l);
    push(&mut eng);
    assert!(!videos(&drain(&mut rl)).is_empty());
    assert_eq!(hub.encoder.lock()["chosen"][0]["backend"], "openh264");
}

#[test]
fn the_last_h264_encoder_stays() {
    let hub = Hub::new();
    let mut eng = engine(
        &hub,
        &[H264_HW],
        Opts {
            h264_hw_breaks: true,
            ..Default::default()
        },
    );
    let (l, mut rl) = client(&hub);
    join_legacy(&hub, &l);
    for _ in 0..(3 + FAIL_AFTER) {
        push(&mut eng);
        acked(&l, &mut rl);
    }
    assert!(eng.cohorts[0].shared.degraded());
    assert_eq!(eng.h264, H264_HW);
    assert!(eng.failed.is_empty());
    assert_eq!(eng.cohorts.len(), 1);
    assert!(cohort_of(&l).is_some());
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

fn two_stripes() -> VideoConfig {
    VideoConfig {
        stripes: 2,
        ..config()
    }
}

#[test]
fn a_dead_h264_stripe_gets_a_keyframe_a_second_at_most() {
    let keys = Arc::new((AtomicUsize::new(0), AtomicUsize::new(0)));
    let o = Opts {
        h264_second_stripe_dead: true,
        h264_keys: Some(keys.clone()),
        ..Default::default()
    };
    let hub = Hub::new();
    let mut eng = Engine::new(two_stripes(), hub.clone(), W, H, &[H264], fakes(o)).unwrap();
    let (c, _rx) = client(&hub);
    join_legacy(&hub, &c);
    // An idle desktop: nothing changes for 2.5 s.
    let t = Instant::now();
    while t.elapsed() < Duration::from_millis(2500) {
        push_band(&mut eng, W, H, None);
        std::thread::sleep(Duration::from_millis(5));
    }
    let (all, boosted) = (
        keys.0.load(Ordering::Relaxed),
        keys.1.load(Ordering::Relaxed),
    );
    // FAIL_AFTER quick ones at the keyframe budget, then about one a second without it.
    assert_eq!(
        boosted as u32, FAIL_AFTER,
        "{all} keyframes, {boosted} boosted"
    );
    assert!(
        (FAIL_AFTER as usize + 1..=FAIL_AFTER as usize + 4).contains(&all),
        "{all} keyframes in 2.5 s"
    );
    assert!(eng.cohorts[0].shared.degraded());
}

#[test]
fn a_degraded_cohorts_keyframes_wait_for_flow_control() {
    let o = Opts {
        h264_second_stripe_dead: true,
        ..Default::default()
    };
    let hub = Hub::new();
    let mut eng = Engine::new(two_stripes(), hub.clone(), W, H, &[H264], fakes(o)).unwrap();
    let (c, _rx) = client(&hub);
    join_legacy(&hub, &c);
    for _ in 0..FAIL_AFTER {
        push_band(&mut eng, W, H, None);
    }
    assert!(eng.cohorts[0].shared.degraded());
    *eng.cohorts[0].shared.last_key.lock() = Instant::now() - RETRY_EVERY;
    for id in 0..40 {
        c.inflight.lock().push_back(crate::hub::Inflight {
            id,
            sent: Instant::now(),
            bytes: 1,
        });
    }
    let views = eng.views();
    assert!(views[0].want_key);
    assert_eq!(
        eng.takes(&views, 33.0),
        vec![false],
        "a viewer that is behind"
    );
    c.inflight.lock().clear();
    assert_eq!(eng.takes(&eng.views(), 33.0), vec![true]);
}

#[test]
fn a_failed_session_is_reopened() {
    let o = Opts {
        h264_second_stripe_dead: true,
        h264_reopen_heals: true,
        ..Default::default()
    };
    let hub = Hub::new();
    let mut eng = Engine::new(two_stripes(), hub.clone(), W, H, &[H264], fakes(o)).unwrap();
    let (c, mut rx) = client(&hub);
    join_legacy(&hub, &c);
    push(&mut eng);
    assert!(
        videos(&drain(&mut rx)).is_empty(),
        "the bottom stripe failed"
    );
    push_band(&mut eng, W, H, None);
    let v = videos(&drain(&mut rx));
    assert_eq!(v.len(), 2, "both stripes of one frame");
    assert!(
        v.iter().all(|f| f.1 && f.2 == v[0].2),
        "a keyframe on every stripe, the bottom one from its new session: {v:?}"
    );
    assert!(!eng.cohorts[0].shared.degraded());
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
        refine_qp: None,
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

/// Takes the whole burst, as the capture loop does while `due`.
fn passes(b: &mut Burst, now: Instant) -> Vec<Option<u8>> {
    std::iter::from_fn(|| b.due(now).then(|| take(b, now))).collect()
}

fn take(b: &mut Burst, now: Instant) -> Option<u8> {
    let qp = b.rung();
    b.pass(now);
    qp
}

#[test]
fn a_qp_encoder_takes_the_ladder() {
    let now = Instant::now();
    let mut b = Burst::new(3, NO_DELAY, now);
    b.moved(now);
    assert_eq!(passes(&mut b, now), [Some(20), Some(16), Some(12)]);
}

/// An HEVC viewer on a plain cohort and an H.264 viewer on a ladder, keyframes landed.
struct Pair {
    hub: Arc<Hub>,
    eng: Engine,
    seen: Refines,
    hevc: Arc<Client>,
    hevc_rx: UnboundedReceiver<Out>,
    h264: Arc<Client>,
    h264_rx: UnboundedReceiver<Out>,
    t: Instant,
}

fn pair() -> Pair {
    let seen = Refines::default();
    let hub = Hub::new();
    let opts = Opts {
        refines: Some(seen.clone()),
        ..Default::default()
    };
    let mut eng = engine(&hub, &[HEVC, H264], opts);
    let (hevc, mut hevc_rx) = client(&hub);
    let (h264, mut h264_rx) = client(&hub);
    join_v2(&hub, &hevc, &[Codec::Hevc]);
    join_legacy(&hub, &h264);
    push(&mut eng);
    ready(&hevc, &drain(&mut hevc_rx));
    acked(&h264, &mut h264_rx);
    push(&mut eng);
    acked(&hevc, &mut hevc_rx);
    acked(&h264, &mut h264_rx);
    let t = Instant::now();
    seen.lock().unwrap().clear();
    Pair {
        hub,
        eng,
        seen,
        hevc,
        hevc_rx,
        h264,
        h264_rx,
        t,
    }
}

impl Pair {
    fn cohort(&self, codec: Codec) -> &Cohort {
        let c = self.eng.cohorts.iter();
        c.into_iter()
            .find(|c| c.shared.cand.codec == codec)
            .unwrap()
    }
    fn left(&self, codec: Codec) -> u32 {
        self.cohort(codec).refine.burst.left
    }
    fn qps(&self) -> Vec<Option<u8>> {
        let s = self.seen.lock().unwrap();
        s.iter().map(|(r, _)| r.map(|r| r.qp)).collect()
    }
    fn still(&mut self) {
        still(&mut self.eng, &mut self.t);
    }
    fn ack_hevc(&mut self) {
        acked(&self.hevc, &mut self.hevc_rx);
    }
    fn ack_h264(&mut self) {
        acked(&self.h264, &mut self.h264_rx);
    }
    /// Turns with both viewers answering, until neither cohort has a pass left.
    fn finish(&mut self) {
        for _ in 0..8 {
            self.still();
            self.ack_hevc();
            self.ack_h264();
        }
    }
    /// The H.264 viewer falls far enough behind for flow control to stop its cohort.
    fn stall_h264(&mut self) {
        let mut q = self.h264.inflight.lock();
        q.front_mut().unwrap().sent -= Duration::from_millis(500);
    }
}

#[test]
fn each_cohort_refines_its_own_way() {
    let mut p = pair();
    p.finish();
    assert_eq!(p.qps(), [Some(20), Some(16), Some(12)]);
    assert_eq!((p.left(Codec::Hevc), p.left(Codec::H264)), (0, 0));
    let bytes = |c: &Cohort| c.shared.paintover_bytes.load(Ordering::Relaxed);
    assert_eq!(
        bytes(p.cohort(Codec::Hevc)),
        0,
        "plain passes have no budget"
    );
    assert!(bytes(p.cohort(Codec::H264)) > 0);
}

#[test]
fn an_idle_desktop_returns_to_zero_bits() {
    let mut p = pair();
    p.finish();
    let bytes = p.hub.stats.bytes.load(Ordering::Relaxed);
    let encoded = p.hub.stats.encoded.load(Ordering::Relaxed);
    p.finish();
    assert_eq!(p.hub.stats.bytes.load(Ordering::Relaxed), bytes);
    assert_eq!(p.hub.stats.encoded.load(Ordering::Relaxed), encoded);
    let views = p.eng.views();
    let later = p.t + Duration::from_secs(3600);
    assert!(!p.eng.turn(&views, 33.0, later).work(), "no capture either");
}

#[test]
fn a_cohort_that_joins_mid_ladder_changes_nothing_for_the_other() {
    let seen = Refines::default();
    let hub = Hub::new();
    let opts = Opts {
        refines: Some(seen.clone()),
        ..Default::default()
    };
    let mut eng = engine(&hub, &[HEVC, H264], opts);
    let (b, mut rb) = client(&hub);
    join_legacy(&hub, &b);
    push(&mut eng);
    acked(&b, &mut rb);
    let mut t = Instant::now();
    still(&mut eng, &mut t);
    seen.lock().unwrap().clear();
    still(&mut eng, &mut t);
    acked(&b, &mut rb);
    let (a, mut ra) = client(&hub);
    join_v2(&hub, &a, &[Codec::Hevc]);
    still(&mut eng, &mut t);
    acked(&b, &mut rb);
    ready(&a, &drain(&mut ra));
    still(&mut eng, &mut t);
    acked(&b, &mut rb);
    let qps: Vec<_> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|s| s.0.unwrap().qp)
        .collect();
    assert_eq!(qps, [20, 16, 12]);
    let plain = eng.cohorts.iter().find(|c| !c.refine.burst.ladder).unwrap();
    assert_eq!(plain.shared.keyframes.load(Ordering::Relaxed), 1);
}

#[test]
fn a_slow_viewer_holds_back_only_its_own_ladder() {
    let mut p = pair();
    p.still();
    assert_eq!(p.qps(), [Some(20)]);
    for _ in 0..4 {
        p.still();
        p.ack_hevc();
    }
    assert_eq!(p.qps(), [Some(20)], "not acknowledged yet");
    assert_eq!(p.left(Codec::H264), 2);
    assert_eq!(p.left(Codec::Hevc), 0, "the other cohort finished");
    p.ack_h264();
    p.finish();
    assert_eq!(p.qps(), [Some(20), Some(16), Some(12)]);
}

#[test]
fn a_viewer_that_stopped_answering_holds_back_no_pass() {
    let mut p = pair();
    p.still();
    p.still();
    assert_eq!(p.qps(), [Some(20)]);
    let mut q = p.h264.inflight.lock();
    q.front_mut().unwrap().sent -= Duration::from_millis(REFINE_DRAIN_MS as u64);
    drop(q);
    let views = p.eng.views();
    let i = p
        .eng
        .cohorts
        .iter()
        .position(|c| c.refine.burst.ladder)
        .unwrap();
    assert!(p.eng.cohorts[i].drained(&views[i]));
}

/// Flow control has it, and it's old enough for the drain gate to let it by: no capture for it.
#[test]
fn a_blocked_cohort_has_no_pass_to_take() {
    let mut p = pair();
    p.still();
    let mut q = p.h264.inflight.lock();
    q.front_mut().unwrap().sent -= Duration::from_millis(REFINE_DRAIN_MS as u64 + 500);
    drop(q);
    let views = p.eng.views();
    let i = p
        .eng
        .cohorts
        .iter()
        .position(|c| c.refine.burst.ladder)
        .unwrap();
    let later = p.t + Duration::from_secs(60);
    assert!(p.eng.cohorts[i].pass_due(&views[i], later));
    let takes = p.eng.takes(&views, 33.0);
    assert!(!takes[i]);
    assert!(!p.eng.passes(&views, &takes, later)[i]);
    p.ack_h264();
    let turn = p.eng.turn(&views, 33.0, later);
    assert!(turn.passes[i], "its viewer answered");
}

#[test]
fn a_pass_is_work_for_the_capture_loop() {
    let turn = |pass| Turn {
        takes: vec![true],
        passes: vec![pass],
        want_key: false,
        catch_up: false,
    };
    assert!(turn(true).work());
    assert!(!turn(false).work());
}

#[test]
fn a_pass_waits_for_the_encoder_too() {
    let mut p = pair();
    p.still();
    p.ack_h264();
    let views = p.eng.views();
    let i = p
        .eng
        .cohorts
        .iter()
        .position(|c| c.refine.burst.ladder)
        .unwrap();
    let c = &p.eng.cohorts[i];
    assert!(c.drained(&views[i]));
    c.shared.busy.fetch_add(1, Ordering::AcqRel);
    assert!(!c.drained(&views[i]), "a pass is still being encoded");
    c.shared.busy.fetch_sub(1, Ordering::AcqRel);
}

/// Motion it couldn't take: once its viewer answers it gets that first, under rate control,
/// and then the whole ladder, once. The desktop stays still, so nothing starts it again.
#[test]
fn a_recovered_cohort_catches_up_then_refines_once() {
    let mut p = pair();
    p.finish();
    p.seen.lock().unwrap().clear();
    push(&mut p.eng);
    p.ack_hevc();
    p.stall_h264();
    push(&mut p.eng);
    p.ack_hevc();
    assert!(p.cohort(Codec::H264).skipped.is_some());
    assert_eq!(p.left(Codec::H264), 3);
    let done = p.qps().len();
    p.still();
    p.ack_hevc();
    assert_eq!(p.left(Codec::Hevc), 2, "the other cohort doesn't wait");
    assert_eq!(p.left(Codec::H264), 3);
    p.ack_h264();
    p.still();
    assert!(p.cohort(Codec::H264).skipped.is_none());
    assert_eq!(p.left(Codec::H264), 3, "a catch-up is no pass");
    assert_eq!(p.qps()[done..], [None]);
    p.finish();
    assert_eq!(p.qps()[done..], [None, Some(20), Some(16), Some(12)]);
    let bytes = p.hub.stats.bytes.load(Ordering::Relaxed);
    p.finish();
    assert_eq!(p.hub.stats.bytes.load(Ordering::Relaxed), bytes);
}

/// A catch-up due exactly when the only pass of a burst is.
#[test]
fn a_catch_up_takes_no_pass_of_a_one_pass_burst() {
    let mut cfg = config();
    cfg.paintover_burst = 1;
    let seen = Refines::default();
    let hub = Hub::new();
    let opts = Opts {
        refines: Some(seen.clone()),
        ..Default::default()
    };
    let mut eng = Engine::new(cfg, hub.clone(), W, H, &[H264], fakes(opts)).unwrap();
    let (c, mut rx) = client(&hub);
    join_legacy(&hub, &c);
    push(&mut eng);
    acked(&c, &mut rx);
    let mut t = Instant::now();
    still(&mut eng, &mut t);
    eng.cohorts[0].skipped = Some(skip(None, &whole(W, H)));
    seen.lock().unwrap().clear();
    still(&mut eng, &mut t);
    acked(&c, &mut rx);
    assert_eq!(eng.cohorts[0].refine.burst.left, 1);
    still(&mut eng, &mut t);
    let qps: Vec<_> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|s| s.0.map(|r| r.qp))
        .collect();
    assert_eq!(qps, [None, Some(12)]);
}

#[test]
fn a_job_that_found_no_room_changes_no_refinement() {
    let hub = Hub::new();
    let mut eng = engine(&hub, &[H264], Opts::default());
    let (c, mut rx) = client(&hub);
    join_legacy(&hub, &c);
    push(&mut eng);
    acked(&c, &mut rx);
    let mut t = Instant::now();
    still(&mut eng, &mut t);
    let (tx, _) = mpsc::sync_channel::<Job>(1);
    let queue = eng.cohorts[0].tx.replace(tx);
    still(&mut eng, &mut t);
    let r = &eng.cohorts[0].refine;
    assert_eq!(r.burst.left, 3);
    assert!(r.refiner.any_pending());
    assert!(eng.cohorts[0].skipped.is_none(), "a pass is no damage");
    eng.cohorts[0].tx = queue;
    still(&mut eng, &mut t);
    assert_eq!(eng.cohorts[0].refine.burst.left, 2);
}

/// The last pass went out, then its viewer fell behind and the picture moved: the ladder is
/// on again, though the cohort took none of that motion.
#[test]
fn motion_a_blocked_cohort_missed_rearms_a_finished_ladder() {
    let mut p = pair();
    for _ in 0..2 {
        p.still();
        p.ack_h264();
        p.ack_hevc();
    }
    p.still();
    p.ack_hevc();
    assert_eq!(p.left(Codec::H264), 0);
    p.stall_h264();
    push(&mut p.eng);
    p.ack_hevc();
    assert!(p.cohort(Codec::H264).skipped.is_some());
    assert_eq!(p.left(Codec::H264), 3);
    p.ack_h264();
    p.finish();
    let qps = p.qps();
    assert_eq!(qps[3..], [None, Some(20), Some(16), Some(12)]);
}

/// The last viewer left and the next joined between two looks: no sample ever saw it empty.
#[test]
fn a_viewer_that_replaces_the_last_inherits_nothing() {
    let mut p = pair();
    p.eng.cohorts.iter_mut().for_each(|c| {
        c.refine.burst.backoff = REFINE_MAX_BACKOFF;
        c.refine.burst.tokens = -1.0;
        c.refine.burst.last_burst = Some(Instant::now());
    });
    let id = p.h264.id;
    p.hub.remove_client(id);
    let (next, mut rx) = client(&p.hub);
    join_legacy(&p.hub, &next);
    p.still();
    let r = &p.cohort(Codec::H264).refine;
    assert_eq!((r.burst.backoff, r.burst.left), (0, 3));
    assert!(r.burst.tokens > 0.0);
    assert!(r.refiner.any_pending());
    let other = &p.cohort(Codec::Hevc).refine.burst;
    assert_eq!(other.backoff, REFINE_MAX_BACKOFF, "its own stays");
    acked(&next, &mut rx);
}

/// Blocks gone still ride on one cohort's motion frame; the other's stay as they are.
#[test]
fn a_cohort_piggybacks_only_its_own_blocks() {
    let mut p = pair();
    p.finish();
    push(&mut p.eng);
    p.ack_hevc();
    p.ack_h264();
    let i = p
        .eng
        .cohorts
        .iter()
        .position(|c| c.refine.burst.ladder)
        .unwrap();
    let old = Duration::from_secs(1);
    let r = &mut p.eng.cohorts[i].refine.refiner;
    r.epoch -= old;
    r.last_refine -= old;
    let views = p.eng.views();
    let turn = p.eng.turn(&views, 33.0, Instant::now());
    let mut f = whole(W, H);
    let blocks = (W.div_ceil(16) * H.div_ceil(16)) as usize;
    f.band = Some(Band { y0: 0, y1: 16 });
    f.mb_dirty = Some(Arc::new(vec![0; blocks]));
    p.eng.dispatch(&f, &views, &turn, Instant::now());
    settle(&p.eng);
    assert_eq!(p.hub.stats.refines.load(Ordering::Relaxed), 1);
    assert!(!p.cohort(Codec::H264).refine.refiner.any_pending());
    assert!(p.cohort(Codec::Hevc).refine.refiner.any_pending());
}

/// Its keyframe may have landed before the look that saw the cohort empty, and been absorbed.
#[test]
fn a_viewer_joining_an_empty_ladder_cohort_gets_a_ladder() {
    let mut p = pair();
    p.finish();
    let id = p.h264.id;
    p.hub.remove_client(id);
    p.still();
    assert_eq!(p.left(Codec::H264), 0);
    let (next, _rx) = client(&p.hub);
    join_legacy(&p.hub, &next);
    p.eng.replan();
    let views = p.eng.views();
    p.eng.update_rates(&views);
    assert_eq!(p.left(Codec::H264), 3);
    assert!(p.cohort(Codec::H264).refine.refiner.any_pending());
}

#[test]
fn a_cohort_that_leaves_changes_nothing_for_the_other() {
    let mut p = pair();
    p.still();
    p.ack_h264();
    p.ack_hevc();
    assert_eq!(p.left(Codec::H264), 2);
    let id = p.hevc.id;
    p.hub.remove_client(id);
    p.finish();
    assert_eq!(p.qps(), [Some(20), Some(16), Some(12)]);
}

#[test]
fn another_cohorts_keyframe_starts_no_ladder() {
    let mut p = pair();
    p.finish();
    p.seen.lock().unwrap().clear();
    let keys = |p: &Pair| {
        p.cohort(Codec::Hevc)
            .shared
            .keyframes
            .load(Ordering::Relaxed)
    };
    let before = keys(&p);
    p.hevc.needs_keyframe.store(true, Ordering::Relaxed);
    p.still();
    assert_eq!(keys(&p), before + 1);
    let id = p.hevc.id;
    p.hub.remove_client(id);
    p.finish();
    assert_eq!(p.left(Codec::H264), 0);
    assert_eq!(p.qps(), [] as [Option<u8>; 0]);
}

#[test]
fn a_cohorts_own_keyframe_starts_its_ladder_over_and_only_a_landed_one() {
    let hub = Hub::new();
    let opts = Opts {
        h264_empty_keys: true,
        ..Default::default()
    };
    let mut eng = engine(&hub, &[H264], opts);
    let (c, _rx) = client(&hub);
    join_legacy(&hub, &c);
    for _ in 0..3 {
        push(&mut eng);
    }
    assert_eq!(hub.stats.keyframes.load(Ordering::Relaxed), 0, "dropped");
    assert!(eng.views()[0].want_key, "and asked for again");
    let motion = eng.cohorts[0].refine.burst.last_motion;
    let mut t = Instant::now();
    still(&mut eng, &mut t);
    still(&mut eng, &mut t);
    assert_eq!(eng.cohorts[0].refine.burst.last_motion, motion);
    let sh = eng.cohorts[0].shared.clone();
    sh.keyframes.fetch_add(1, Ordering::Release);
    still(&mut eng, &mut t);
    assert_eq!(eng.cohorts[0].refine.burst.last_motion, t);
}

#[test]
fn a_keyframe_not_yet_looked_at_holds_back_a_pass() {
    let mut p = pair();
    let views = p.eng.views();
    let i = p
        .eng
        .cohorts
        .iter()
        .position(|c| c.refine.burst.ladder)
        .unwrap();
    p.ack_h264();
    let later = p.t + Duration::from_secs(60);
    assert!(p.eng.cohorts[i].pass_due(&views[i], later));
    let sh = &p.eng.cohorts[i].shared;
    sh.keyframes.fetch_add(1, Ordering::Release);
    assert!(!p.eng.cohorts[i].pass_due(&views[i], later));
}

/// What the encoder finished for viewers now gone is nobody's: not the next viewer's budget,
/// nor a keyframe it never saw.
#[test]
fn a_cohort_starts_over_when_its_viewers_left() {
    let mut p = pair();
    p.still();
    p.ack_h264();
    p.still();
    let id = p.h264.id;
    p.hub.remove_client(id);
    let sh = p.cohort(Codec::H264).shared.clone();
    sh.keyframes.fetch_add(1, Ordering::Release);
    sh.paintover_bytes.fetch_add(1_000_000, Ordering::Release);
    p.still();
    p.still();
    let r = &p.cohort(Codec::H264).refine;
    assert_eq!((r.burst.left, r.burst.backoff), (0, 0));
    assert_eq!(r.keyframes, sh.keyframes.load(Ordering::Relaxed));
    assert_eq!(r.burst.spent, sh.paintover_bytes.load(Ordering::Relaxed));
    assert!(!r.refiner.any_pending());
}

/// None of what bounds a ladder touches plain passes: they wait for the shortest gap, whatever
/// was spent and however often, and for nobody's acknowledgement.
#[test]
fn plain_passes_have_no_budget_and_no_backoff() {
    let now = Instant::now();
    let mut b = Burst::new(3, GAP, now);
    b.account(now, 2000, 0);
    for i in 1..5 {
        b.account(now, 2000, 500_000 * i);
        b.moved(now);
        for _ in 0..3 {
            b.pass(now);
        }
    }
    b.moved(now);
    assert!(b.gap(now) > GAP, "backed off");
    assert!(!b.funded(), "and over budget");
    b.ladder = false;
    for i in 0..8 {
        let t = now + Duration::from_secs(i);
        assert_eq!(b.gap(t), GAP);
        b.moved(t);
        assert!(b.due(t + GAP));
        assert_eq!(passes(&mut b, t + GAP), [None, None, None]);
    }
}

/// Damage goes to whoever can take it; a capture made for another cohort sends this one nothing.
#[test]
fn a_cohort_gets_no_job_for_anothers_pass() {
    let mut p = pair();
    p.finish();
    let plain = p.cohort(Codec::Hevc).shared.clone();
    let done = p.hub.stats.encoded.load(Ordering::Relaxed);
    p.eng.cohorts.iter_mut().for_each(|c| {
        if c.refine.burst.ladder {
            c.refine.burst.moved(Instant::now());
            c.refine.refiner.mark_all();
        }
    });
    let before = plain.busy.load(Ordering::Relaxed);
    p.still();
    assert_eq!(plain.busy.load(Ordering::Relaxed), before);
    assert_eq!(p.hub.stats.encoded.load(Ordering::Relaxed), done + 1);
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

const NO_DELAY: Duration = Duration::ZERO;
const GAP: Duration = Duration::from_millis(150);

/// One whole ladder at `now`; bytes sent count towards the next burst.
fn ladder(b: &mut Burst, now: Instant, spent: u64) -> Vec<u8> {
    b.account(now, 0, spent);
    b.moved(now);
    std::iter::from_fn(|| b.due(now).then(|| take(b, now).unwrap())).collect()
}

#[test]
fn motion_stopping_refines_once_then_goes_quiet() {
    let now = Instant::now();
    let mut b = Burst::new(3, NO_DELAY, now);
    assert!(!b.due(now), "nothing to refine before any motion");
    assert_eq!(ladder(&mut b, now, 0), [20, 16, 12]);
    assert!(!b.due(now));
}

#[test]
fn a_burst_waits_for_the_picture_to_stay_still() {
    let now = Instant::now();
    let mut b = Burst::new(3, GAP, now);
    b.moved(now);
    assert!(!b.due(now));
    assert!(!b.due(now + GAP / 2));
    assert!(b.due(now + GAP));
    b.moved(now + GAP / 2);
    assert!(!b.due(now + GAP), "motion starts the wait over");
}

#[test]
fn new_damage_cancels_the_rest_and_starts_over() {
    let now = Instant::now();
    let mut b = Burst::new(3, NO_DELAY, now);
    b.moved(now);
    assert_eq!(take(&mut b, now), Some(20));
    b.moved(now);
    assert_eq!(take(&mut b, now), Some(20), "the finer rungs are gone");
    assert_eq!(take(&mut b, now), Some(16));
    assert_eq!(take(&mut b, now), Some(12));
    assert!(!b.due(now));
}

#[test]
fn any_burst_length_ends_at_the_finest_rung() {
    let qps = |n| {
        ladder(
            &mut Burst::new(n, NO_DELAY, Instant::now()),
            Instant::now(),
            0,
        )
    };
    assert_eq!(qps(0), [] as [u8; 0]);
    assert_eq!(qps(1), [12]);
    assert_eq!(qps(2), [16, 12]);
    assert_eq!(qps(5), [20, 20, 20, 16, 12]);
}

/// A cursor blinks: damage, a still moment, damage. Each blink refines only its own
/// blocks, a burst at most, then nothing is pending until the next one.
#[test]
fn a_blinking_cursor_does_not_storm() {
    let now = Instant::now();
    let mut b = Burst::new(3, NO_DELAY, now);
    let mut r = Refiner::new(100, 10);
    r.clear_all();
    let mut passes = 0;
    for _ in 0..10 {
        let mut dirty = vec![0u8; 100];
        dirty[42] = 1;
        r.mark(&dirty);
        b.moved(now);
        while b.due(now) {
            let Some((map, band)) = r.pending_map() else {
                b.stop();
                break;
            };
            assert_eq!(map.iter().filter(|&&m| m != 0).count(), 1);
            assert_eq!((band.y0, band.y1), (64, 80));
            b.pass(now);
            passes += 1;
            if b.left == 0 {
                r.clear_all();
            }
        }
        assert!(r.pending_map().is_none());
    }
    assert_eq!(passes, 30);
}

#[test]
fn a_constantly_changing_region_gets_no_burst() {
    let now = Instant::now();
    let mut b = Burst::new(3, GAP, now);
    for i in 0..5 {
        let t = now + GAP / 3 * i;
        b.moved(t);
        assert!(!b.due(t + GAP / 4));
    }
}

/// Stops `step` apart, each costing `cost`: the gap each next burst has to wait for.
fn gaps(cost: u64, step: Duration, n: u32) -> Vec<u128> {
    let start = Instant::now();
    let mut b = Burst::new(3, GAP, start);
    let mut out = Vec::new();
    for i in 0..n {
        let now = start + step * i;
        out.push(b.gap(now).as_millis());
        b.account(now, 0, cost * i as u64);
        b.moved(now);
        for _ in 0..3 {
            b.pass(now);
        }
    }
    out
}

#[test]
fn a_single_stop_refines_after_the_shortest_gap() {
    assert_eq!(gaps(500_000, Duration::from_secs(1), 1), [150]);
    let now = Instant::now();
    let mut b = Burst::new(3, GAP, now);
    b.account(now, 2000, 0);
    b.moved(now);
    assert!(b.due(now + GAP), "even at the lowest cap");
}

#[test]
fn repeated_costly_stops_back_off() {
    assert_eq!(
        gaps(500_000, Duration::from_secs(1), 8),
        [150, 150, 300, 600, 1200, 2400, 2400, 2400]
    );
}

#[test]
fn cheap_stops_do_not_back_off() {
    assert_eq!(
        gaps(2_000, Duration::from_secs(1), 6),
        [150, 150, 150, 150, 150, 150]
    );
}

#[test]
fn a_quiet_spell_ends_the_backoff() {
    let g = gaps(500_000, REFINE_QUIET, 6);
    assert_eq!(g, [150; 6], "stops this far apart aren't repeated");
    let start = Instant::now();
    let mut b = Burst::new(3, GAP, start);
    for i in 0..6 {
        let now = start + Duration::from_secs(i);
        b.account(now, 0, 500_000 * i as u32 as u64);
        b.moved(now);
        for _ in 0..3 {
            b.pass(now);
        }
    }
    let last = start + Duration::from_secs(5);
    let gap = |after: Duration| b.gap(last + after).as_millis();
    assert_eq!(gap(Duration::from_secs(1)), 2400);
    assert_eq!(gap(REFINE_QUIET), 1200);
    assert_eq!(gap(REFINE_QUIET * 3), 300);
    assert_eq!(gap(REFINE_QUIET * 4), 150);
}

/// The picture keeps stopping for less than the gap: a burst every `REFINE_QUIET` or so
/// goes through anyway, and once the stops end the picture converges within the longest
/// gap, however much was spent.
#[test]
fn repeated_stops_keep_refining_now_and_then_and_converge_after() {
    let start = Instant::now();
    let mut b = Burst::new(3, GAP, start);
    let still = Duration::from_millis(450);
    let (mut bursts, mut spent) = (Vec::new(), 0);
    for i in 0..300u32 {
        let stop = start + Duration::from_millis(900) * i;
        b.account(stop, 2000, spent);
        b.moved(stop);
        if b.due(stop + still) {
            bursts.push(stop);
            spent += 100_000;
            for _ in 0..3 {
                b.pass(stop + still);
            }
        } else {
            b.stop();
        }
    }
    let spacing = |w: &[Instant]| (w[1] - w[0]).as_secs_f64();
    let late: Vec<f64> = bursts[bursts.len() / 2..].windows(2).map(spacing).collect();
    assert!(late.len() >= 15, "{} bursts", bursts.len());
    assert!(
        late.iter().all(|&s| s < 2.0 * REFINE_QUIET.as_secs_f64()),
        "{late:?}"
    );
    let last = start + Duration::from_millis(900) * 300;
    b.moved(last);
    let settled = last + GAP * (1 << REFINE_MAX_BACKOFF) + Duration::from_secs(5);
    b.account(settled, 2000, spent);
    assert!(b.due(settled));
}

#[test]
fn spending_the_share_holds_a_burst_until_it_refills() {
    let now = Instant::now();
    let mut b = Burst::new(3, NO_DELAY, now);
    // 2 Mbit/s: 25 KB/s, 250 KB over the window.
    b.account(now, 2000, 0);
    assert_eq!(b.tokens, 250_000.0);
    b.moved(now);
    assert!(b.due(now), "a full window pays for a ladder");
    b.pass(now);
    b.account(now, 2000, 300_000);
    assert!(b.due(now), "one under way finishes");
    b.pass(now);
    b.pass(now);
    b.moved(now);
    assert!(!b.due(now), "over budget");
    assert_eq!(b.wait_ms(now), 100);
    let later = now + Duration::from_millis(1900);
    b.account(later, 2000, 300_000);
    assert!(!b.due(later));
    let later = now + Duration::from_millis(2100);
    b.account(later, 2000, 300_000);
    assert!(b.due(later), "refilled: the picture still converges");
}

#[test]
fn an_uncapped_stream_has_no_byte_budget() {
    let now = Instant::now();
    let mut b = Burst::new(3, NO_DELAY, now);
    b.account(now, 0, u32::MAX as u64);
    b.moved(now);
    assert!(b.due(now));
}

#[test]
fn what_an_uncapped_stream_spent_is_no_debt_under_a_cap() {
    let now = Instant::now();
    let mut b = Burst::new(3, NO_DELAY, now);
    b.account(now, 0, 0);
    b.account(now, 0, 1_000_000);
    b.account(now, 0, 2_000_000);
    b.account(now, 2000, 2_000_000);
    b.moved(now);
    assert!(b.due(now));
}

fn refines_through(cand: Candidate) -> Vec<(Option<Refine>, u32)> {
    let seen = Refines::default();
    let hub = Hub::new();
    let cands = [cand];
    let mut eng = engine(
        &hub,
        &cands,
        Opts {
            refines: Some(seen.clone()),
            ..Default::default()
        },
    );
    let (c, mut rx) = client(&hub);
    join_legacy(&hub, &c);
    push(&mut eng);
    acked(&c, &mut rx);
    let mut t = Instant::now();
    still(&mut eng, &mut t);
    seen.lock().unwrap().clear();
    still(&mut eng, &mut t);
    let got = seen.lock().unwrap().clone();
    got
}

#[test]
fn a_pass_reaches_openh264_with_its_budget_and_no_rate_change() {
    let seen = refines_through(H264);
    let (refine, kbps) = seen.last().copied().unwrap();
    let start = START_KBPS as usize;
    assert_eq!(
        refine,
        Some(Refine {
            qp: 20,
            max_bytes: Some(start * REFINE_FRAME_MS / 8),
        })
    );
    assert_ne!(
        kbps,
        stripe_share(START_KBPS as u32, 1) * 2,
        "paint-over rate applied"
    );
}

#[test]
fn a_pass_stays_a_rate_change_for_other_encoders() {
    let seen = refines_through(H264_HW);
    let (refine, kbps) = seen.last().copied().unwrap();
    assert_eq!(refine, None);
    assert_eq!(kbps, stripe_share(START_KBPS as u32, 1) * 2);
}

/// Real OpenH264 through the engine: whether the library takes the switch or refuses it,
/// a viewer gets no keyframe beyond its first and the stream carries on.
fn passes_through_openh264(break_switch: bool) -> (u64, u64, u32, bool) {
    let hub = Hub::new();
    let open: Open = Arc::new(move |c: &EncoderConfig| {
        let mut e = encoder::openh264::OpenH264::new(c)?;
        e.break_switch = break_switch;
        Ok(Box::new(e) as Box<dyn VideoEncoder>)
    });
    let mut eng = Engine::new(config(), hub.clone(), W, H, &[H264], open).unwrap();
    let (c, mut rx) = client(&hub);
    join_legacy(&hub, &c);
    let ack = |c: &Client, rx: &mut UnboundedReceiver<Out>| {
        for f in videos(&drain(rx)) {
            c.ack(None, f.2);
        }
    };
    push(&mut eng);
    ack(&c, &mut rx);
    let mut t = Instant::now();
    for _ in 0..3 {
        still(&mut eng, &mut t);
        for _ in REFINE_RUNGS {
            still(&mut eng, &mut t);
            ack(&c, &mut rx);
        }
        push(&mut eng);
        ack(&c, &mut rx);
    }
    let stats = &hub.stats;
    (
        stats.keyframes.load(Ordering::Relaxed),
        stats.paintover_frames.load(Ordering::Relaxed),
        eng.cohorts[0].shared.failures.load(Ordering::Acquire),
        c.needs_keyframe.load(Ordering::Relaxed),
    )
}

#[test]
fn a_refused_switch_is_no_keyframe_storm() {
    let (keys, _, failures, needs_key) = passes_through_openh264(true);
    assert_eq!((keys, failures, needs_key), (1, 0, false));
}

#[test]
fn the_switch_through_the_engine_sends_its_frames() {
    let (keys, frames, failures, needs_key) = passes_through_openh264(false);
    assert_eq!((keys, failures, needs_key), (1, 0, false));
    assert!(
        frames > 0,
        "the control: passes do come out when the switch works"
    );
}
