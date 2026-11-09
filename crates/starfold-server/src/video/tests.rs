use super::*;
use crate::encoder::{AccessUnit, Backend, BitstreamFormat, ColorInfo};
use crate::input::InputEvent;
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
    /// Refining passes this encoder still has something to sharpen with (None: none).
    refinable: Option<Arc<AtomicUsize>>,
    /// A refining pass spends this much of its budget (None: a few bytes).
    fill: Option<f64>,
    /// The frame rate of every rate change.
    rates: Option<Arc<StdMutex<Vec<u32>>>>,
    /// The least budget a step of sharpening needs (None: any).
    step_bytes: Option<usize>,
    /// Every encode takes this long.
    delay: Option<Duration>,
    /// Every n-th picture comes out a keyframe nobody asked for, taking this long.
    slow_keys: Option<(usize, Duration)>,
}

type Refines = Arc<StdMutex<Vec<(Option<Refine>, u32)>>>;

fn codec_string(c: Codec, high: bool, level: u32) -> String {
    match c {
        Codec::H264 if high => format!("avc1.6400{level:02x}"),
        Codec::H264 => format!("avc1.42c0{level:02x}"),
        Codec::Hevc => format!("hvc1.1.6.L{level}.B0"),
        Codec::Av1 => format!("av01.0.{level:02}M.08"),
        #[cfg(feature = "libvpx")]
        Codec::Vp8 | Codec::Vp9 => unreachable!("no bench arm in these tests"),
    }
}

impl VideoEncoder for Fake {
    fn set_rate(&mut self, r: RateConfig) -> Result<(), EncodeError> {
        self.kbps = r.max_kbps;
        if let Some(rates) = &self.rates {
            rates.lock().unwrap().push(r.fps);
        }
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
        if let Some(d) = self.delay {
            std::thread::sleep(d);
        }
        self.n += 1;
        let own_key = self
            .slow_keys
            .is_some_and(|(k, _)| self.n.is_multiple_of(k));
        if let (true, Some((_, d))) = (own_key, self.slow_keys) {
            std::thread::sleep(d);
        }
        if let Some(r) = &self.refines {
            r.lock().unwrap().push((opts.refine, self.kbps));
        }
        if let (Some(step), Some(r)) = (self.step_bytes, opts.refine) {
            if r.max_bytes.is_some_and(|m| m < step) {
                return Ok(Vec::new());
            }
        }
        if let (Some(left), Some(_)) = (&self.refinable, opts.refine) {
            let _ = left.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1));
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
        if let (Some(f), Some(r)) = (self.fill, opts.refine) {
            let mut data = vec![0; ((r.max_bytes.unwrap_or(0) as f64 * f) as usize).max(6)];
            data[..5].copy_from_slice(&[0, 0, 0, 1, 0x41]);
            return Ok(vec![AccessUnit {
                pts_ms: opts.pts_ms,
                keyframe: false,
                data,
                config: None,
            }]);
        }
        let relevel = self.relevel == Some(self.n);
        let key = opts.force_keyframe || self.n == 1 || relevel || own_key;
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

    fn refinable(&self, r: Refine) -> bool {
        self.refinable
            .as_ref()
            .is_some_and(|n| n.load(Ordering::Relaxed) > 0)
            && self
                .step_bytes
                .is_none_or(|b| r.max_bytes.is_none_or(|m| m >= b))
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
    /// Refining passes the first H.264 stripe's encoder has something to sharpen with.
    h264_refinable: Option<Arc<AtomicUsize>>,
    /// H.264 refining passes spend this much of their budget.
    refine_fill: Option<f64>,
    /// The frame rate of every rate change of the first H.264 stripe's encoder.
    h264_rates: Option<Arc<StdMutex<Vec<u32>>>>,
    /// The least budget a step of H.264 sharpening needs.
    h264_step_bytes: Option<usize>,
    /// Every hardware H.264 encode takes this long.
    h264_hw_delay: Option<Duration>,
    /// Every n-th hardware H.264 picture is a slow keyframe nobody asked for.
    h264_hw_slow_keys: Option<(usize, Duration)>,
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
                refinable: None,
                fill: None,
                rates: None,
                step_bytes: None,
                delay: o.h264_hw_delay,
                slow_keys: o.h264_hw_slow_keys,
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
            refinable: o.h264_refinable.clone().filter(|_| !hevc && h264 == 1),
            fill: o.refine_fill.filter(|_| !hevc),
            rates: o.h264_rates.clone().filter(|_| !hevc && h264 == 1),
            step_bytes: o.h264_step_bytes.filter(|_| !hevc),
            delay: None,
            slow_keys: None,
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
        high_fps: false,
        source_fps: 0,
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
        refine_tail: false,
        refine_idle: false,
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

/// A viewer on H.264 (protocol 2 or 1) with its first keyframe acked.
fn watching(v2: bool) -> (Arc<Hub>, Engine, Arc<Client>, UnboundedReceiver<Out>) {
    let hub = Hub::new();
    let mut eng = engine(&hub, &[H264], Opts::default());
    let (c, mut rx) = client(&hub);
    if v2 {
        join_v2(&hub, &c, &[Codec::H264]);
        push(&mut eng);
        ready(&c, &drain(&mut rx));
    } else {
        join_legacy(&hub, &c);
    }
    push(&mut eng);
    let first = videos(&acked(&c, &mut rx));
    assert!(first.len() == 1 && first[0].1, "{first:?}");
    (hub, eng, c, rx)
}

/// Lets `ms` pass for `c`'s frames in flight and its last ack.
fn wait(c: &Client, ms: u64) {
    let d = Duration::from_millis(ms);
    for f in c.inflight.lock().iter_mut() {
        f.sent = f.sent.checked_sub(d).unwrap();
    }
    let mut at = c.acked_at.lock();
    *at = at.checked_sub(d).unwrap();
}

fn ack_one(c: &Client, v: (u8, bool, u32, Option<u32>)) {
    assert!(c.ack(v.3, v.2));
}

fn keyframes(m: &[Msg]) -> usize {
    videos(m).iter().filter(|v| v.1).count()
}

#[test]
fn a_slow_link_that_keeps_acking_is_not_resynced() {
    for v2 in [true, false] {
        let (_hub, mut eng, c, mut rx) = watching(v2);
        for _ in 0..4 {
            push(&mut eng);
        }
        let mut sent = videos(&drain(&mut rx));
        assert!(sent.len() >= 3, "{sent:?}");
        // A frame acked every second until one is left: it is 5 s old, the last ack 2 s.
        for _ in 0..5 {
            wait(&c, 1000);
            if sent.len() > 1 {
                ack_one(&c, sent.remove(0));
            }
            push(&mut eng);
            push(&mut eng);
        }
        assert!(c.backlog().1 > 4000, "v2={v2}");
        assert!(!c.needs_keyframe.load(Ordering::Relaxed), "v2={v2}");
        assert_eq!(keyframes(&drain(&mut rx)), 0, "v2={v2}");
        assert!(!c.inflight.lock().is_empty(), "v2={v2}");
    }
}

#[test]
fn a_viewer_that_stops_acking_is_resynced() {
    for v2 in [true, false] {
        let (_hub, mut eng, c, mut rx) = watching(v2);
        push(&mut eng);
        assert_eq!(videos(&drain(&mut rx)).len(), 1);
        wait(&c, 4100);
        push(&mut eng);
        assert!(c.needs_keyframe.load(Ordering::Relaxed), "v2={v2}");
        push_band(&mut eng, W, H, None);
        let v = videos(&drain(&mut rx));
        assert!(v.len() == 1 && v[0].1, "v2={v2}: {v:?}");
    }
}

/// The bound: a viewer whose acks stop while a keyframe is on its way is resynced in the
/// first capture turn more than STALL_MS after its last ack or that keyframe, whichever is later.
#[test]
fn acks_that_stop_during_a_keyframe_resync_within_the_bound() {
    for v2 in [true, false] {
        let (_hub, mut eng, c, mut rx) = watching(v2);
        push(&mut eng);
        let p = videos(&drain(&mut rx));
        wait(&c, 1000);
        ack_one(&c, p[0]);
        assert!(c.request_keyframe(v2.then(|| c.route.lock().epoch)));
        push_band(&mut eng, W, H, None);
        let k = videos(&drain(&mut rx));
        assert!(k.len() == 1 && k[0].1, "v2={v2}: {k:?}");
        wait(&c, STALL_MS as u64 - 100);
        push(&mut eng);
        push(&mut eng);
        assert!(!c.needs_keyframe.load(Ordering::Relaxed), "v2={v2}: early");
        assert_eq!(keyframes(&drain(&mut rx)), 0, "v2={v2}: early");
        wait(&c, 200);
        push(&mut eng);
        assert!(c.needs_keyframe.load(Ordering::Relaxed), "v2={v2}: late");
        push_band(&mut eng, W, H, None);
        assert_eq!(keyframes(&drain(&mut rx)), 1, "v2={v2}");
    }
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
        refine_tail: false,
        refine_idle: false,
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
        paced: false,
        pass_paced: false,
        at: Instant::now(),
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
            max_bytes: Some(start * REFINE_LADDER_MS / 8 * REFINE_PLAN_PCT / 100),
        })
    );
    assert_ne!(
        kbps,
        stripe_share(START_KBPS as u32, 1) * 2,
        "paint-over rate applied"
    );
}

/// A legacy viewer on OpenH264 whose encoder has `left` refining passes' worth to sharpen:
/// the QP of every pass over `turns` still turns, each acked.
fn tail(left: usize, turns: usize, motion_at: Option<usize>) -> Vec<u8> {
    let seen = Refines::default();
    let hub = Hub::new();
    let mut eng = engine(
        &hub,
        &[H264],
        Opts {
            refines: Some(seen.clone()),
            h264_refinable: Some(Arc::new(AtomicUsize::new(left))),
            ..Default::default()
        },
    );
    let (c, mut rx) = client(&hub);
    join_legacy(&hub, &c);
    push(&mut eng);
    acked(&c, &mut rx);
    let mut t = Instant::now();
    seen.lock().unwrap().clear();
    for i in 0..turns {
        if motion_at == Some(i) {
            t += Duration::from_secs(60);
            capture(&mut eng, W, H, Some(Band { y0: 0, y1: H }), t);
        } else {
            still(&mut eng, &mut t);
        }
        acked(&c, &mut rx);
    }
    let got = seen.lock().unwrap();
    got.iter().filter_map(|(r, _)| r.map(|r| r.qp)).collect()
}

#[test]
fn a_still_picture_is_refined_until_the_encoder_has_nothing_left() {
    assert_eq!(tail(6, 20, None), [20, 16, 12, 12, 12, 12]);
    assert_eq!(tail(0, 20, None), [20, 16, 12], "the ladder still runs");
}

#[test]
fn new_damage_cancels_the_tail() {
    assert_eq!(tail(100, 8, Some(5)), [20, 16, 12, 12, 20, 16]);
}

#[test]
fn the_tail_has_a_limit() {
    let qps = tail(1000, 200, None);
    assert_eq!(qps.len(), 3 + REFINE_TAIL_MAX as usize);
    assert!(qps[2..].iter().all(|&q| q == 12));
}

/// A legacy viewer on OpenH264 over `cfg`, its first stripe's encoder with `left` refining
/// passes' worth to sharpen, the keyframe acked and its ladder started.
fn refining(
    cfg: VideoConfig,
    left: &Arc<AtomicUsize>,
) -> (
    Arc<Hub>,
    Engine,
    Arc<Client>,
    UnboundedReceiver<Out>,
    Instant,
) {
    let hub = Hub::new();
    let opts = Opts {
        h264_refinable: Some(left.clone()),
        ..Default::default()
    };
    let mut eng = Engine::new(cfg, hub.clone(), W, H, &[H264], fakes(opts)).unwrap();
    let (c, mut rx) = client(&hub);
    join_legacy(&hub, &c);
    push(&mut eng);
    acked(&c, &mut rx);
    let mut t = Instant::now();
    still(&mut eng, &mut t);
    acked(&c, &mut rx);
    (hub, eng, c, rx, t)
}

#[test]
fn tail_bytes_cost_the_next_burst_nothing() {
    let left = Arc::new(AtomicUsize::new(10));
    let (hub, mut eng, c, mut rx, mut t) = refining(config(), &left);
    for _ in 0..3 {
        still(&mut eng, &mut t);
        acked(&c, &mut rx);
    }
    let charged = eng.cohorts[0]
        .shared
        .paintover_bytes
        .load(Ordering::Acquire);
    let sent = hub.stats.paintover_bytes.load(Ordering::Relaxed);
    assert!(charged > 0, "the ladder is charged");
    for _ in 0..20 {
        still(&mut eng, &mut t);
        acked(&c, &mut rx);
    }
    assert_eq!(left.load(Ordering::Relaxed), 0, "the tail ran");
    assert!(hub.stats.paintover_bytes.load(Ordering::Relaxed) > sent);
    assert_eq!(
        eng.cohorts[0]
            .shared
            .paintover_bytes
            .load(Ordering::Acquire),
        charged,
        "the tail is not"
    );
}

/// The top stripe's blocks were sharpened on a motion frame (no longer pending), yet its
/// encoder can still sharpen: the tail keeps reaching it until it can't.
#[test]
fn the_tail_reaches_every_stripe_that_can_still_sharpen() {
    let left = Arc::new(AtomicUsize::new(8));
    let (_hub, mut eng, c, mut rx, mut t) = refining(two_stripes(), &left);
    let (cols, rows) = (W.div_ceil(16) as usize, H.div_ceil(16) as usize);
    let r = &mut eng.cohorts[0].refine.refiner;
    for (i, p) in r.pending.iter_mut().enumerate() {
        *p &= i / cols >= rows / 2;
    }
    for _ in 0..30 {
        still(&mut eng, &mut t);
        acked(&c, &mut rx);
    }
    assert_eq!(left.load(Ordering::Relaxed), 0);
    assert!(
        !eng.cohorts[0].refine.refiner.any_pending(),
        "then it stops"
    );
}

/// Busy stop/start: `cycles` times motion then `still_ms` of still turns 50 ms apart, then a
/// last stop. Refining passes spend `fill` times their budget; `fall_to` drops the bitrate
/// target (kbit/s) at that stop.
struct Busy {
    cycles: usize,
    still_ms: u64,
    fill: f64,
    fall_to: Option<f64>,
}

struct BusyRun {
    /// From the last stop to its first pass.
    wait: Duration,
    /// Bytes charged against the share.
    charged: u64,
    /// The share of the target over every turn, plus the window it starts with.
    share: f64,
    /// The bitrate target when the last stop came (kbit/s).
    target_at_stop: u32,
    /// The least the budget held.
    min_tokens: f64,
    /// Turns a due ladder waited for budget, and how many of them came after the last stop.
    held: usize,
    held_after_stop: usize,
    /// The budget when the last stop came.
    tokens_at_stop: f64,
    /// Refining passes taken.
    passes: usize,
}

fn stop_start(b: Busy) -> BusyRun {
    let seen = Refines::default();
    let hub = Hub::new();
    let opts = Opts {
        refines: Some(seen.clone()),
        refine_fill: Some(b.fill),
        ..Default::default()
    };
    let mut eng = engine(&hub, &[H264], opts);
    let (c, mut rx) = client(&hub);
    join_legacy(&hub, &c);
    push(&mut eng);
    acked(&c, &mut rx);
    // The target holds still unless the case drops it: a run slow enough to cross the
    // controller's wall-clock windows would otherwise ramp it.
    let rc = &mut eng.cohorts[0].rc;
    (rc.max, rc.target) = (START_KBPS, START_KBPS);
    let passes = || {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|r| r.0.is_some())
            .count()
    };
    let step = Duration::from_millis(50);
    let mut t = Instant::now();
    let per_s = |kbps: u32| kbps as f64 * 1000.0 / 8.0;
    let first = eng.cohorts[0].shared.target.load(Ordering::Relaxed);
    let mut run = BusyRun {
        wait: Duration::ZERO,
        charged: 0,
        share: per_s(first) * REFINE_SHARE * REFINE_WINDOW_S,
        target_at_stop: 0,
        min_tokens: f64::INFINITY,
        held: 0,
        held_after_stop: 0,
        tokens_at_stop: 0.0,
        passes: 0,
    };
    let mut turn = |eng: &mut Engine, run: &mut BusyRun, t: Instant, motion: bool| {
        let band = motion.then_some(Band { y0: 0, y1: H });
        capture(eng, W, H, band, t);
        acked(&c, &mut rx);
        let k = &eng.cohorts[0];
        run.share += per_s(k.shared.target.load(Ordering::Relaxed)) * REFINE_SHARE * 0.05;
        let burst = &k.refine.burst;
        run.min_tokens = run.min_tokens.min(burst.tokens);
        let waiting = burst.left == burst.len && burst.tail == 0 && !burst.funded();
        let held = (waiting && t - burst.last_motion >= burst.gap(t)) as usize;
        run.held += held;
        if run.target_at_stop > 0 {
            run.held_after_stop += held;
        }
    };
    for _ in 0..b.cycles {
        turn(&mut eng, &mut run, t, true);
        for _ in 0..b.still_ms / 50 {
            t += step;
            turn(&mut eng, &mut run, t, false);
        }
        t += step;
    }
    if let Some(kbps) = b.fall_to {
        let rc = &mut eng.cohorts[0].rc;
        (rc.max, rc.target) = (kbps, kbps);
    }
    turn(&mut eng, &mut run, t, true);
    run.target_at_stop = eng.cohorts[0].shared.target.load(Ordering::Relaxed);
    run.tokens_at_stop = eng.cohorts[0].refine.burst.tokens;
    let stop = t;
    let before = passes();
    while passes() == before {
        t += step;
        turn(&mut eng, &mut run, t, false);
        assert!(t - stop < Duration::from_secs(120), "no pass at all");
    }
    run.wait = t - stop;
    run.charged = eng.cohorts[0]
        .shared
        .paintover_bytes
        .load(Ordering::Acquire);
    run.passes = passes();
    run
}

/// A ladder is three 200 ms passes planned at 55%: refilled at a 10% share, 3.3 s of debt at
/// the target it was charged at. The first pass after a stop waits that, scaled by how far the
/// target has fallen since and by how far the passes overshot their plan, plus the longest gap.
fn bound(overshoot: f64, fall: f64) -> Duration {
    Duration::from_millis(150 << REFINE_MAX_BACKOFF)
        + Duration::from_secs_f64(3.3 * overshoot * fall)
        + Duration::from_millis(100)
}

/// Dense text: no step fits a ladder pass's budget, so its passes come out empty and sharpen
/// nothing; one fits a tail pass's. The ladder still hands over to the tail, at the tail's budget.
#[test]
fn a_picture_too_dense_for_a_ladder_pass_still_gets_its_tail() {
    let seen = Refines::default();
    let hub = Hub::new();
    let per_s = START_KBPS as usize * 1000 / 8;
    let opts = Opts {
        refines: Some(seen.clone()),
        h264_refinable: Some(Arc::new(AtomicUsize::new(6))),
        h264_step_bytes: Some(per_s / 2 * 55 / 100),
        ..Default::default()
    };
    let mut eng = engine(&hub, &[H264], opts);
    let (c, mut rx) = client(&hub);
    join_legacy(&hub, &c);
    push(&mut eng);
    acked(&c, &mut rx);
    let mut t = Instant::now();
    for _ in 0..20 {
        still(&mut eng, &mut t);
        acked(&c, &mut rx);
    }
    let passes: Vec<(u8, Option<usize>)> = seen
        .lock()
        .unwrap()
        .iter()
        .filter_map(|r| r.0.map(|r| (r.qp, r.max_bytes)))
        .collect();
    let (ladder, tail) = (Some(per_s / 5 * 55 / 100), Some(per_s * 55 / 100));
    let mut want = vec![(20, ladder), (16, ladder), (12, ladder)];
    want.extend([(12, tail); 6]);
    assert_eq!(passes, want);
}

/// A legacy viewer on OpenH264 with `left` passes' worth to sharpen (each step needing at
/// least `step` bytes, if given), its keyframe acked; motion at the returned instant.
fn refining_from(
    left: usize,
    step: Option<usize>,
) -> (
    Arc<Hub>,
    Engine,
    Arc<Client>,
    UnboundedReceiver<Out>,
    Refines,
    Instant,
) {
    let seen = Refines::default();
    let hub = Hub::new();
    let opts = Opts {
        refines: Some(seen.clone()),
        h264_refinable: Some(Arc::new(AtomicUsize::new(left))),
        h264_step_bytes: step,
        ..Default::default()
    };
    let mut eng = engine(&hub, &[H264], opts);
    let (c, mut rx) = client(&hub);
    join_legacy(&hub, &c);
    push(&mut eng);
    acked(&c, &mut rx);
    let t = Instant::now();
    capture(&mut eng, W, H, Some(Band { y0: 0, y1: H }), t);
    acked(&c, &mut rx);
    seen.lock().unwrap().clear();
    (hub, eng, c, rx, seen, t)
}

/// Still turns 100 ms apart from `from` to `to`, each acked; the passes they took, and when.
fn still_until(
    eng: &mut Engine,
    c: &Client,
    rx: &mut UnboundedReceiver<Out>,
    seen: &Refines,
    from: Instant,
    to: Instant,
) -> Vec<(Duration, Option<usize>)> {
    let mut out = Vec::new();
    let mut t = from;
    while t < to {
        t += Duration::from_millis(100);
        let before = seen.lock().unwrap().len();
        capture(eng, W, H, None, t);
        acked(c, rx);
        for r in seen.lock().unwrap()[before..].iter().filter_map(|r| r.0) {
            out.push((t - from, r.max_bytes));
        }
    }
    out
}

/// What a viewer types next waits behind one pass at most: while input or damage is recent,
/// every pass, tail ones included, keeps to the ladder's budget; only an idle screen gets the
/// tail's.
#[test]
fn a_pass_while_input_or_damage_is_recent_stays_ladder_sized() {
    let per_s = START_KBPS as usize * 1000 / 8;
    let (ladder, tail) = (Some(per_s / 5 * 55 / 100), Some(per_s * 55 / 100));
    let (hub, mut eng, c, mut rx, seen, t) = refining_from(200, None);
    let busy = still_until(
        &mut eng,
        &c,
        &mut rx,
        &seen,
        t,
        t + REFINE_IDLE - Duration::from_millis(100),
    );
    assert!(busy.len() > 3, "the tail ran while busy: {busy:?}");
    assert!(busy.iter().all(|p| p.1 == ladder), "{busy:?}");
    let idle = still_until(
        &mut eng,
        &c,
        &mut rx,
        &seen,
        t + REFINE_IDLE,
        t + REFINE_IDLE * 2,
    );
    assert!(
        !idle.is_empty() && idle.iter().all(|p| p.1 == tail),
        "{idle:?}"
    );
    // Input with nothing on screen to show for it yet counts as much as damage.
    let typed = t + REFINE_IDLE * 2;
    *hub.input_at.lock() = Some(typed);
    let after = still_until(
        &mut eng,
        &c,
        &mut rx,
        &seen,
        typed,
        typed + REFINE_IDLE - Duration::from_millis(100),
    );
    assert!(
        !after.is_empty() && after.iter().all(|p| p.1 == ladder),
        "{after:?}"
    );
}

/// Dense text: no step fits a ladder pass. While busy no tail pass is taken at all; once the
/// screen has been idle for REFINE_IDLE the tail runs at its budget until nothing is left.
#[test]
fn a_dense_picture_waits_for_idle_then_converges() {
    let per_s = START_KBPS as usize * 1000 / 8;
    let tail = Some(per_s * 55 / 100);
    let (_hub, mut eng, c, mut rx, seen, t) = refining_from(6, Some(per_s / 2 * 55 / 100));
    let busy = still_until(
        &mut eng,
        &c,
        &mut rx,
        &seen,
        t,
        t + REFINE_IDLE - Duration::from_millis(100),
    );
    assert_eq!(
        busy.len(),
        3,
        "the ladder, and no tail while busy: {busy:?}"
    );
    let idle = still_until(
        &mut eng,
        &c,
        &mut rx,
        &seen,
        t + REFINE_IDLE - Duration::from_millis(100),
        t + REFINE_IDLE * 3,
    );
    assert_eq!(idle.len(), 6, "{idle:?}");
    assert!(idle.iter().all(|p| p.1 == tail), "{idle:?}");
    assert!(
        !eng.cohorts[0].refine.refiner.any_pending(),
        "converged and quiet"
    );
}

#[test]
fn the_first_pass_after_a_busy_stop_comes_within_the_bound() {
    let busy = |cycles, fill, fall_to| {
        stop_start(Busy {
            cycles,
            still_ms: 1500,
            fill,
            fall_to,
        })
    };
    for cycles in [1, 3, 30] {
        let r = busy(cycles, 1.0, None);
        let b = bound(1.0, 1.0);
        assert!(
            r.wait <= b,
            "{cycles} cycle(s): first pass after {:?}, bound {b:?}",
            r.wait
        );
    }
    let r = busy(30, 1.8, None);
    let b = bound(1.8, 1.0);
    assert!(
        r.wait <= b,
        "passes 1.8x their plan: first pass after {:?}, bound {b:?}",
        r.wait
    );
    // Passes over their plan leave the budget in debt at the stop; the target then falls 4x.
    let r = busy(3, 1.8, Some(2000.0));
    assert!(r.tokens_at_stop < 0.0, "no debt to repay at the stop");
    assert!(
        r.held_after_stop > 0,
        "repayment never held the next ladder"
    );
    let fall = START_KBPS / r.target_at_stop as f64;
    let b = bound(1.8, fall);
    assert!(
        r.wait <= b,
        "target fell {fall}x: first pass after {:?}, bound {b:?}",
        r.wait
    );
}

/// Passes that overshoot their plan keep the budget in debt, so ladders wait for it; what is
/// charged still stays within the share of the target as it was, plus the window it starts with
/// and the last ladder, which finishes once begun.
#[test]
fn a_busy_screen_keeps_its_charged_refinement_within_the_share() {
    let r = stop_start(Busy {
        cycles: 30,
        still_ms: 3000,
        fill: 1.8,
        fall_to: None,
    });
    assert!(
        r.min_tokens < 0.0 && r.held > 0,
        "the budget never held a ladder back"
    );
    let last_ladder = 3.0 * 1.8 * START_KBPS * 1000.0 / 8.0 * 0.2 * 0.55;
    // Each charged pass carries its stripe header; before the first turn the budget refills over
    // the real time setup took, under one 50 ms step.
    let slack = (r.passes * HDR) as f64 + START_KBPS * 1000.0 / 8.0 * REFINE_SHARE * 0.05;
    let allowed = r.share + last_ladder + slack;
    assert!(
        (r.charged as f64) <= allowed,
        "charged {} against {allowed:.0} allowed",
        r.charged
    );
}

/// Frames skipped by flow control or a busy encoder don't count: the encoder budgets each frame
/// for the rate the cohort actually takes them at in motion, never below FPS_FLOOR, and a still
/// in between doesn't drag it down.
#[test]
fn the_encoder_budgets_for_the_rate_a_cohort_takes_frames_at() {
    let rates = Arc::new(StdMutex::new(Vec::new()));
    let hub = Hub::new();
    let opts = Opts {
        h264_rates: Some(rates.clone()),
        ..Default::default()
    };
    let mut eng = engine(&hub, &[H264], opts);
    let (c, mut rx) = client(&hub);
    join_legacy(&hub, &c);
    push(&mut eng);
    acked(&c, &mut rx);
    let mut t = Instant::now();
    let mut motion = |eng: &mut Engine, t: &mut Instant, every_ms: u64, for_ms: u64| {
        for _ in 0..for_ms / every_ms {
            *t += Duration::from_millis(every_ms);
            capture(eng, W, H, Some(Band { y0: 0, y1: H }), *t);
            acked(&c, &mut rx);
        }
    };
    let last = |rates: &Arc<StdMutex<Vec<u32>>>| *rates.lock().unwrap().last().unwrap();
    motion(&mut eng, &mut t, 100, 3000);
    assert_eq!(last(&rates), 10);
    t += Duration::from_secs(2);
    motion(&mut eng, &mut t, 33, 4000);
    assert_eq!(last(&rates), 30, "{:?}", rates.lock().unwrap());
    motion(&mut eng, &mut t, 400, 6000);
    assert_eq!(last(&rates), FPS_FLOOR);
    assert!(rates.lock().unwrap().iter().all(|&f| f >= FPS_FLOOR));
}

/// A key typed within REFINE_IDLE keeps frames at the full rate's budget, so it never waits
/// behind a frame sized for a slower one; wheel and pointer input keep the rate frames are taken at.
#[test]
fn typing_keeps_the_full_rate_budget_and_the_wheel_does_not() {
    let rates = Arc::new(StdMutex::new(Vec::new()));
    let hub = Hub::new();
    let opts = Opts {
        h264_rates: Some(rates.clone()),
        ..Default::default()
    };
    let mut eng = engine(&hub, &[H264], opts);
    let (c, mut rx) = client(&hub);
    join_legacy(&hub, &c);
    push(&mut eng);
    acked(&c, &mut rx);
    let mut t = Instant::now();
    let mut motion = |eng: &mut Engine, t: &mut Instant, frames: usize| {
        for _ in 0..frames {
            *t += Duration::from_millis(100);
            capture(eng, W, H, Some(Band { y0: 0, y1: H }), *t);
            acked(&c, &mut rx);
        }
    };
    let last = |rates: &Arc<StdMutex<Vec<u32>>>| *rates.lock().unwrap().last().unwrap();
    motion(&mut eng, &mut t, 30);
    assert_eq!(last(&rates), 10);
    hub.note_input_at(
        &InputEvent::Key {
            keysym: 0x61,
            down: true,
        },
        t,
    );
    motion(&mut eng, &mut t, 1);
    assert_eq!(last(&rates), 30, "a key: the full rate's budget");
    t += Duration::from_secs(3);
    for ev in [
        InputEvent::Scroll { dx: 0, dy: 1 },
        InputEvent::ScrollPx { dx: 0.0, dy: 12.0 },
        InputEvent::Move { x: 1, y: 2 },
        InputEvent::Button {
            button: 1,
            down: true,
        },
    ] {
        hub.note_input_at(&ev, t);
    }
    motion(&mut eng, &mut t, 3);
    assert_eq!(last(&rates), 10, "the wheel: the rate frames are taken at");
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
#[cfg(feature = "openh264")]
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

#[cfg(feature = "openh264")]
#[test]
fn a_refused_switch_is_no_keyframe_storm() {
    let (keys, _, failures, needs_key) = passes_through_openh264(true);
    assert_eq!((keys, failures, needs_key), (1, 0, false));
}

#[cfg(feature = "openh264")]
#[test]
fn the_switch_through_the_engine_sends_its_frames() {
    let (keys, frames, failures, needs_key) = passes_through_openh264(false);
    assert_eq!((keys, failures, needs_key), (1, 0, false));
    assert!(
        frames > 0,
        "the control: passes do come out when the switch works"
    );
}

fn high_fps() -> VideoConfig {
    VideoConfig {
        fps: 60,
        high_fps: true,
        ..config()
    }
}

#[test]
fn high_fps_caps_hardware_videotoolbox_only() {
    let on = high_fps();
    assert_eq!(fps_cap(&on, H264_HW), 120);
    assert_eq!(fps_cap(&on, HEVC), 120);
    assert_eq!(fps_cap(&on, H264), 60);
    assert_eq!(fps_cap(&on, HEVC_SW), 60, "VideoToolbox in software");
    assert_eq!(fps_cap(&on, AV1), 60, "NVENC isn't measured at 120");
    let off = VideoConfig {
        high_fps: false,
        ..high_fps()
    };
    for c in [H264_HW, HEVC, H264, AV1] {
        assert_eq!(fps_cap(&off, c), 60);
    }
    let fast = VideoConfig {
        fps: 144,
        ..high_fps()
    };
    assert_eq!((fps_cap(&fast, HEVC), fps_cap(&fast, H264)), (144, 60));
    assert_eq!(base_fps(&fast), 60);
    let slow = VideoConfig {
        fps: 30,
        ..high_fps()
    };
    assert_eq!((fps_cap(&slow, HEVC), fps_cap(&slow, H264)), (120, 30));
    let forced = VideoConfig {
        fps: 120,
        high_fps: false,
        ..high_fps()
    };
    assert_eq!((fps_cap(&forced, H264), base_fps(&forced)), (120, 120));
    assert_eq!(encoder_config(&on, HEVC, W, H, 1).fps, 120);
    assert_eq!(encoder_config(&on, HEVC, W, H, 1).rate.fps, 120);
    assert_eq!(encoder_config(&on, H264, W, H, 1).fps, 60);
}

#[test]
fn framerate_above_60_without_high_fps_warns() {
    assert_eq!(
        fps_warning(120, false, "openh264").as_deref(),
        Some("unmeasured above 60 fps on openh264; use --high-fps on VideoToolbox")
    );
    assert_eq!(fps_warning(60, false, "openh264"), None);
    assert_eq!(fps_warning(120, true, "videotoolbox"), None);
}

#[test]
fn high_fps_leaves_an_openh264_server_as_it_was() {
    for on in [false, true] {
        let hub = Hub::new();
        let cfg = VideoConfig {
            high_fps: on,
            ..high_fps()
        };
        let mut eng = Engine::new(cfg, hub.clone(), W, H, &[H264], fakes(Opts::default())).unwrap();
        let (c, _rx) = client(&hub);
        join_legacy(&hub, &c);
        push(&mut eng);
        let views = eng.views();
        assert_eq!(eng.capture_fps(&views), 60);
        assert_eq!((eng.cohorts[0].fps(), eng.cohorts[0].pace.fps), (60, 60));
        assert_eq!(eng.early(&views, Instant::now()), vec![false]);
    }
}

/// Captures at the rate the cohorts ask for, from `t` for `n` turns, each viewer acking what
/// it got when `acks` says so. Returns the frames each viewer got and the turns at which a
/// cohort was held back by its cap alone.
fn run_captures(
    eng: &mut Engine,
    t: &mut Instant,
    n: usize,
    viewers: &mut [(&Client, &mut UnboundedReceiver<Out>, bool)],
) -> (Vec<usize>, usize) {
    let mut got = vec![0; viewers.len()];
    let mut paced = 0;
    for _ in 0..n {
        eng.replan();
        let views = eng.views();
        eng.update_rates(&views);
        let fps = eng.arrival_fps(&views);
        let turn = eng.turn(&views, 1000.0 / eng.capture_fps(&views) as f64, *t);
        paced += turn.paced as usize;
        eng.mark(&views, None);
        eng.dispatch(&whole(W, H), &views, &turn, *t);
        settle(eng);
        for (i, (c, rx, ack)) in viewers.iter_mut().enumerate() {
            let epoch = c.route.lock().epoch;
            let legacy = c.route.lock().proto == Proto::Legacy;
            for v in videos(&drain(rx)) {
                got[i] += 1;
                if *ack {
                    assert!(c.ack((!legacy).then_some(epoch), v.2));
                }
            }
        }
        *t += Duration::from_micros(1_000_000 / fps as u64);
    }
    (got, paced)
}

type Viewer = (Arc<Client>, UnboundedReceiver<Out>);

/// A HEVC viewer on hardware VideoToolbox and a legacy one on OpenH264, both started.
fn mixed(hub: &Arc<Hub>, o: Opts) -> (Engine, Viewer, Viewer) {
    let mut eng = Engine::new(high_fps(), hub.clone(), W, H, &[HEVC, H264], fakes(o)).unwrap();
    let (a, mut ra) = client(hub);
    let (b, mut rb) = client(hub);
    a.hz.store(120, Ordering::Relaxed);
    join_v2(hub, &a, &[Codec::Hevc]);
    join_legacy(hub, &b);
    push(&mut eng);
    ready(&a, &drain(&mut ra));
    for v in videos(&drain(&mut rb)) {
        assert!(b.ack(None, v.2));
    }
    (eng, (a, ra), (b, rb))
}

#[test]
fn a_software_cohort_keeps_60_while_the_capture_runs_at_120() {
    let rates = Arc::new(StdMutex::new(Vec::new()));
    let hub = Hub::new();
    let (mut eng, (a, mut ra), (b, mut rb)) = mixed(
        &hub,
        Opts {
            h264_rates: Some(rates.clone()),
            ..Default::default()
        },
    );
    let views = eng.views();
    assert_eq!(eng.capture_fps(&views), 120);
    let fps: Vec<(Candidate, u32, u32)> = eng
        .cohorts
        .iter()
        .map(|c| (c.shared.cand, c.fps(), c.pace.fps))
        .collect();
    assert!(fps.contains(&(HEVC, 120, 120)), "{fps:?}");
    assert!(fps.contains(&(H264, 60, 60)), "{fps:?}");
    let mut t = Instant::now() + Duration::from_secs(1);
    let (got, paced) = run_captures(
        &mut eng,
        &mut t,
        120,
        &mut [(&*a, &mut ra, true), (&*b, &mut rb, true)],
    );
    assert!(got[0] >= 118, "the HEVC viewer at 120: {got:?}");
    assert!(
        (59..=61).contains(&got[1]),
        "the OpenH264 viewer at 60: {got:?}"
    );
    assert!(paced >= 59, "{paced}");
    assert!(rates.lock().unwrap().iter().all(|&f| f <= 60), "{rates:?}");
}

#[test]
fn a_60_cohort_behind_the_capture_keeps_up_at_its_own_interval() {
    let hub = Hub::new();
    let (mut eng, (a, mut ra), (b, mut rb)) = mixed(&hub, Opts::default());
    let mut t = Instant::now() + Duration::from_secs(1);
    let (got, _) = run_captures(
        &mut eng,
        &mut t,
        60,
        &mut [(&*a, &mut ra, true), (&*b, &mut rb, false)],
    );
    // 40 ms RTT + 40 ms over 60 fps frames, plus 2 (12 at 120 fps).
    assert_eq!(got[1], 7, "{got:?}");
}

#[test]
fn skipped_damage_is_caught_up_once_the_cap_allows() {
    let hub = Hub::new();
    let (mut eng, (a, mut ra), (b, mut rb)) = mixed(&hub, Opts::default());
    let mut t = Instant::now() + Duration::from_secs(1);
    let viewers = &mut [(&*a, &mut ra, true), (&*b, &mut rb, true)];
    run_captures(&mut eng, &mut t, 1, viewers);
    let (got, _) = run_captures(&mut eng, &mut t, 1, viewers);
    assert_eq!(got, vec![1, 0], "OpenH264 skips the second capture");
    let views = eng.views();
    let turn = eng.turn(&views, 1000.0 / 120.0, t - Duration::from_millis(5));
    assert!(turn.catch_up, "a capture is due for the skipped damage");
    assert!(
        !turn.takes[eng
            .cohorts
            .iter()
            .position(|c| c.shared.cand == H264)
            .unwrap()]
    );
}

#[test]
fn the_capture_follows_the_fastest_watched_cohort() {
    let hub = Hub::new();
    let o = Opts::default();
    let mut eng =
        Engine::new(high_fps(), hub.clone(), W, H, &[HEVC_SW, H264_HW], fakes(o)).unwrap();
    let (a, _ra) = client(&hub);
    let (b, mut rb) = client(&hub);
    a.hz.store(120, Ordering::Relaxed);
    join_legacy(&hub, &a);
    join_v2(&hub, &b, &[Codec::Hevc]);
    push(&mut eng);
    ready(&b, &drain(&mut rb));
    push(&mut eng);
    let fps = |eng: &Engine| eng.capture_fps(&eng.views());
    assert_eq!(fps(&eng), 120);
    hub.remove_client(a.id);
    push(&mut eng);
    assert_eq!(
        eng.cohorts.len(),
        2,
        "the hardware H.264 cohort stays, unwatched"
    );
    assert_eq!(fps(&eng), 60);
    assert!(eng.early(&eng.views(), Instant::now()).iter().all(|e| !e));
}

#[test]
fn slow_encodes_count_for_two_seconds_without_a_pause() {
    let t = Instant::now();
    let at = |ms: u64| t + Duration::from_millis(ms);
    let mut w = SlowWatch::default();
    // At 120 fps the interval is 8.3 ms: 6.7 ms is SLOW_SHARE of it.
    for i in 0..500 {
        assert!(!w.note(6.0, 120, at(i * 8)));
    }
    let mut w = SlowWatch::default();
    for i in 0..=248 {
        assert!(!w.note(7.5, 120, at(i * 8)), "{i}");
    }
    assert!(w.note(7.5, 120, at(2000)));
    let mut w = SlowWatch::default();
    for i in 0..200 {
        w.note(7.5, 120, at(i * 8));
    }
    for i in 200..210 {
        w.note(1.0, 120, at(i * 8));
    }
    for i in 210..458 {
        assert!(
            !w.note(7.5, 120, at(i * 8)),
            "a fast spell starts over: {i}"
        );
    }
    let mut w = SlowWatch::default();
    w.note(7.5, 120, at(0));
    assert!(!w.note(7.5, 120, at(2500)), "a pause starts over");
    assert!(!w.note(7.5, 60, at(5000)), "7.5 ms keeps up with 60");
}

#[test]
fn a_cohort_too_slow_for_120_drops_to_60_until_it_reopens() {
    let hub = Hub::new();
    let eng = Engine::new(
        high_fps(),
        hub.clone(),
        W,
        H,
        &[H264_HW, H264],
        fakes(Opts::default()),
    )
    .unwrap();
    let sh = eng.cohorts[0].shared.clone();
    assert_eq!(sh.cand, H264_HW);
    assert_eq!(sh.fps.load(Ordering::Acquire), 120);
    let mut w = SlowWatch::default();
    let t = Instant::now();
    for i in 0..300 {
        sh.encoded(&mut w, 9.0, t + Duration::from_millis(i * 8));
    }
    assert_eq!(sh.fps.load(Ordering::Acquire), 60);
    for i in 300..1000 {
        sh.encoded(&mut w, 0.5, t + Duration::from_millis(i * 8));
    }
    assert_eq!(
        sh.fps.load(Ordering::Acquire),
        60,
        "no way back mid-session"
    );
    assert_eq!(eng.open_cohort(H264_HW).unwrap().fps(), 120);
}

#[test]
fn the_encode_loop_watches_motion_encode_times() {
    let hub = Hub::new();
    let o = Opts {
        h264_hw_delay: Some(Duration::from_millis(8)),
        ..Default::default()
    };
    let mut eng = Engine::new(high_fps(), hub.clone(), W, H, &[H264_HW, H264], fakes(o)).unwrap();
    let (c, mut rx) = client(&hub);
    c.hz.store(120, Ordering::Relaxed);
    join_legacy(&hub, &c);
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(20) && eng.cohorts[0].fps() == 120 {
        push(&mut eng);
        for v in videos(&drain(&mut rx)) {
            assert!(c.ack(None, v.2));
        }
    }
    assert_eq!(eng.cohorts[0].fps(), 60);
    assert!(start.elapsed() >= SLOW_FOR);
}

#[test]
fn a_paced_source_runs_at_the_most_any_cohort_takes() {
    let on = high_fps();
    assert_eq!(source_fps(&on, &[H264]), 60);
    assert_eq!(source_fps(&on, &[HEVC_SW, H264]), 60);
    assert_eq!(source_fps(&on, &[H264_HW, H264]), 120);
    let off = VideoConfig {
        high_fps: false,
        ..high_fps()
    };
    assert_eq!(source_fps(&off, &[H264_HW, H264]), 60);
}

#[test]
fn a_cohort_keeps_its_cap_under_a_faster_source() {
    let hub = Hub::new();
    let cfg = VideoConfig {
        source_fps: 120,
        ..high_fps()
    };
    let mut eng = Engine::new(cfg, hub.clone(), W, H, &[H264], fakes(Opts::default())).unwrap();
    let (c, mut rx) = client(&hub);
    join_legacy(&hub, &c);
    push(&mut eng);
    for v in videos(&drain(&mut rx)) {
        assert!(c.ack(None, v.2));
    }
    let mut t = Instant::now() + Duration::from_secs(1);
    let (got, _) = run_captures(&mut eng, &mut t, 120, &mut [(&*c, &mut rx, true)]);
    assert!((59..=61).contains(&got[0]), "{got:?}");
}

#[test]
fn keyframes_an_encoder_makes_by_itself_are_not_timed() {
    let hub = Hub::new();
    let o = Opts {
        h264_hw_slow_keys: Some((3, Duration::from_millis(30))),
        ..Default::default()
    };
    let mut eng = Engine::new(high_fps(), hub.clone(), W, H, &[H264_HW, H264], fakes(o)).unwrap();
    let (c, mut rx) = client(&hub);
    c.hz.store(120, Ordering::Relaxed);
    join_legacy(&hub, &c);
    let start = Instant::now();
    while start.elapsed() < SLOW_FOR + Duration::from_millis(500) {
        push(&mut eng);
        for v in videos(&drain(&mut rx)) {
            assert!(c.ack(None, v.2));
        }
    }
    assert_eq!(eng.cohorts[0].fps(), 120);
}

#[test]
fn a_pass_held_back_by_the_cap_is_still_work() {
    let mut p = pair();
    p.still();
    p.ack_hevc();
    p.ack_h264();
    let later = p.t + Duration::from_secs(60);
    let views = p.eng.views();
    assert!(p
        .eng
        .passes(&views, &p.eng.takes(&views, 33.0), later)
        .contains(&true));
    p.eng.cfg.source_fps = 120;
    for c in &mut p.eng.cohorts {
        c.last_take = Some(later - Duration::from_millis(5));
    }
    let turn = p.eng.turn(&views, 33.0, later);
    assert!(!turn.passes.contains(&true), "too soon for a 30 fps cap");
    assert!(
        turn.work(),
        "the loop waits for the cap, not the idle timeout"
    );
}

#[test]
fn the_slowest_viewer_holds_a_120_cohort_down_to_60_not_below() {
    let hub = Hub::new();
    let (mut eng, (a, _ra), _b) = mixed(&hub, Opts::default());
    let hevc = |eng: &Engine| {
        let c = eng.cohorts.iter().find(|c| c.shared.cand == HEVC).unwrap();
        (c.fps(), c.shared.fps.load(Ordering::Acquire))
    };
    let at = |eng: &mut Engine, hz: u32| {
        a.hz.store(hz, Ordering::Relaxed);
        push(eng);
        hevc(eng).0
    };
    assert_eq!(at(&mut eng, 120), 120);
    assert_eq!(at(&mut eng, 90), 90);
    assert_eq!(at(&mut eng, 30), 60, "not below the base rate");
    assert_eq!(at(&mut eng, 0), 60, "a viewer that didn't say");
    assert_eq!(at(&mut eng, 144), 120, "not above the cap");
    let (c, mut rc) = client(&hub);
    join_v2(&hub, &c, &[Codec::Hevc]);
    push(&mut eng);
    ready(&c, &drain(&mut rc));
    push(&mut eng);
    assert_eq!(hevc(&eng).0, 60, "a second viewer that didn't say");
    assert_eq!(eng.capture_fps(&eng.views()), 60);
    hub.remove_client(c.id);
    push(&mut eng);
    assert_eq!(hevc(&eng).0, 120);
    a.slow.store(true, Ordering::Relaxed);
    push(&mut eng);
    assert_eq!(hevc(&eng), (60, 120), "slow holds it down; the cap stays");
}

#[test]
fn viewers_change_nothing_without_high_fps() {
    let hub = Hub::new();
    let mut eng = engine(&hub, &[HEVC, H264], Opts::default());
    let (a, mut ra) = client(&hub);
    join_v2(&hub, &a, &[Codec::Hevc]);
    push(&mut eng);
    ready(&a, &drain(&mut ra));
    for hz in [0, 20, 144] {
        a.hz.store(hz, Ordering::Relaxed);
        push(&mut eng);
        assert!(eng.cohorts.iter().all(|c| c.fps() == 30), "{hz}");
    }
}
