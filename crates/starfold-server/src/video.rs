//! Video pipeline: damage-driven capture -> I420 -> encoder cohorts -> clients, with
//! ack-based flow control per cohort.
//!
//! One capture feeds every cohort (negotiate.rs picks them). A cohort encodes the
//! captures it has room for and catches up on the damage of those it skipped, so a
//! slow viewer holds back only its own cohort.

use crate::capture::{Band, Capturer};
use crate::convert::I420;
use crate::encoder::{
    self, Codec, EncodeError, EncodeOptions, EncoderConfig, FrameView, RateConfig, Refine,
    StreamConfig, VideoEncoder,
};
use crate::hub::wait_any;
use crate::hub::{Client, Hub, Inflight, Out, Phase, Proto, Stats};
use crate::negotiate::{self, Candidate, Choice, DecodeCap, DecoderConfig, Offer, StripeOffer};
use bytes::Bytes;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(test)]
mod tests;

#[derive(Clone)]
pub struct VideoConfig {
    pub display: Option<String>,
    pub selection: encoder::Selection,
    pub fps: u32,
    pub preset: String,
    pub threads: u32,
    pub convert_threads: usize,
    pub crf: f32,
    pub max_kbps: u32,
    pub paintover_crf: f32,
    pub paintover_burst: u32,
    pub paintover_delay_ms: u64,
    pub min_kbps: u32,
    pub coalesce: Duration,
    pub stripes: u32,
    pub adaptive_threads: bool,
    pub high_fps: bool,
}

/// With --high-fps, the rate a cohort may take frames at on a hardware encoder measured there.
pub const HIGH_FPS: u32 = 120;
/// With --high-fps, the most the other encoders take; without it, what was measured.
const BASE_FPS: u32 = 60;

/// Whether `cand` may run at HIGH_FPS: so far only VideoToolbox in hardware is measured there.
fn high_fps_eligible(cand: Candidate) -> bool {
    cand.hardware && cand.backend == encoder::Backend::VideoToolbox
}

/// The rate a cohort falls back to when its encodes can't keep up with its cap.
fn base_fps(cfg: &VideoConfig) -> u32 {
    if cfg.high_fps {
        cfg.fps.min(BASE_FPS)
    } else {
        cfg.fps
    }
}

/// The most a cohort encoding with `cand` takes frames at.
fn fps_cap(cfg: &VideoConfig, cand: Candidate) -> u32 {
    if cfg.high_fps && high_fps_eligible(cand) {
        cfg.fps.max(HIGH_FPS)
    } else {
        base_fps(cfg)
    }
}

/// The startup warning for a --framerate nobody measured.
pub fn fps_warning(fps: u32, high_fps: bool, encoder: &str) -> Option<String> {
    (fps > BASE_FPS && !high_fps).then(|| {
        format!("unmeasured above {BASE_FPS} fps on {encoder}; use --high-fps on VideoToolbox")
    })
}

/// Opens an encoder: `encoder::open`, or a fake in tests.
pub type Open =
    Arc<dyn Fn(&EncoderConfig) -> Result<Box<dyn VideoEncoder>, EncodeError> + Send + Sync>;

/// One capture as the cohorts get it: the whole current picture and what changed in it.
#[derive(Clone)]
struct Frame {
    buf: Arc<I420>,
    paintover: bool,
    /// Last pass of a paint-over burst.
    paintover_final: bool,
    /// The QP a backend that refines by QP aims for: only once motion has stopped.
    refine_qp: Option<u8>,
    /// A pass after the ladder: its bytes don't count against the refinement share.
    refine_tail: bool,
    /// A tail pass on a screen idle for REFINE_IDLE: it may take the tail's budget.
    refine_idle: bool,
    /// Rows that changed in this capture.
    band: Option<Band>,
    /// Per-macroblock change map for this capture (None: analyse everything).
    mb_dirty: Option<Arc<Vec<u8>>>,
    captured_at: Instant,
    marker_ms: i64,
}

struct Job {
    frame: Frame,
    force_idr: bool,
    /// The keyframe gets the keyframe budget (not while the cohort is degraded).
    boost: bool,
    /// The rate the cohort takes frames at while the picture moves: each frame is budgeted for it.
    fps: u32,
}

fn union(a: Option<Band>, b: Band) -> Band {
    match a {
        None => b,
        Some(a) => Band {
            y0: a.y0.min(b.y0),
            y1: a.y1.max(b.y1),
        },
    }
}

fn union_opt(a: Option<Band>, b: Option<Band>) -> Option<Band> {
    match (a, b) {
        (a, None) => a,
        (a, Some(b)) => Some(union(a, b)),
    }
}

fn or_maps(a: &[u8], b: &[u8]) -> Vec<u8> {
    a.iter().zip(b).map(|(x, y)| x | y).collect()
}

/// What a cohort skipped while it had no room, encoded with its next frame.
struct Skipped {
    band: Option<Band>,
    /// None: analyse everything.
    mb: Option<Vec<u8>>,
}

fn skip(prev: Option<Skipped>, f: &Frame) -> Skipped {
    match prev {
        None => Skipped {
            band: f.band,
            mb: f.mb_dirty.as_ref().map(|m| m.to_vec()),
        },
        Some(p) => Skipped {
            band: union_opt(p.band, f.band),
            mb: match (p.mb, &f.mb_dirty) {
                (Some(a), Some(b)) => Some(or_maps(&a, b)),
                _ => None,
            },
        },
    }
}

fn catch_up(s: Skipped, mut f: Frame) -> Frame {
    f.band = union_opt(s.band, f.band);
    f.mb_dirty = match (s.mb, f.mb_dirty) {
        (Some(a), Some(b)) => Some(Arc::new(or_maps(&a, &b))),
        _ => None,
    };
    f
}

/// Concurrent encoder cohorts per stream: the best codec and H.264.
const MAX_COHORTS: usize = 2;
/// One buffer being filled, and per cohort one encoding and one queued.
const MAX_BUFS: usize = 1 + 2 * MAX_COHORTS;
/// How long a viewer has to configure its decoders for a `codec` message.
const READY_TIMEOUT: Duration = Duration::from_secs(5);
/// How often viewers are rerouted when nothing asked for it (for READY_TIMEOUT).
const TICK: Duration = Duration::from_millis(250);

/// Capture buffers, shared with the encoders while they read them. A buffer is free
/// when only the pool holds it; each remembers the rows that changed since it was last
/// converted.
struct Pool {
    bufs: Vec<(Arc<I420>, Option<Band>)>,
    w: usize,
    h: usize,
    full: Band,
}

impl Pool {
    fn new(w: usize, h: usize, full: Band) -> Self {
        Pool {
            bufs: (0..2)
                .map(|_| (Arc::new(I420::new(w, h)), Some(full)))
                .collect(),
            w,
            h,
            full,
        }
    }

    fn invalidate(&mut self) {
        for b in &mut self.bufs {
            b.1 = Some(self.full);
        }
    }

    fn free(&mut self) -> Option<usize> {
        if let Some(i) = self
            .bufs
            .iter_mut()
            .position(|b| Arc::get_mut(&mut b.0).is_some())
        {
            return Some(i);
        }
        (self.bufs.len() < MAX_BUFS).then(|| {
            self.bufs
                .push((Arc::new(I420::new(self.w, self.h)), Some(self.full)));
            self.bufs.len() - 1
        })
    }

    /// Brings free buffer `i` up to date (the rows it missed plus `band`) with `convert`
    /// and hands it out.
    fn fill(
        &mut self,
        i: usize,
        band: Option<Band>,
        convert: impl FnOnce(&mut I420, Band),
    ) -> Arc<I420> {
        let todo = union_opt(self.bufs[i].1.take(), band);
        if let Some(b) = band {
            for (j, x) in self.bufs.iter_mut().enumerate() {
                if j != i {
                    x.1 = Some(union(x.1, b));
                }
            }
        }
        if let Some(t) = todo {
            match Arc::get_mut(&mut self.bufs[i].0) {
                Some(buf) => convert(buf, t),
                // Not free after all: convert it next time.
                None => self.bufs[i].1 = Some(t),
            }
        }
        self.bufs[i].0.clone()
    }
}

/// What a cohort's encode thread shares with the capture thread.
struct Shared {
    id: u64,
    cand: Candidate,
    /// The configuration announced to viewers; the encode thread updates it when the
    /// stream's own parameter sets change.
    offer: Mutex<Offer>,
    /// Jobs queued or being encoded.
    busy: AtomicUsize,
    /// A keyframe job is with the encoder; viewers' requests stay set until it lands.
    idr_inflight: AtomicBool,
    /// Adaptive bitrate cap (kbit/s).
    target: AtomicU32,
    /// The most consecutive failed encodes of any stripe; at FAIL_AFTER the cohort is
    /// given up.
    failures: AtomicU32,
    /// When the capture thread last sent this cohort a keyframe job.
    last_key: Mutex<Instant>,
    /// Keyframes this cohort sent.
    keyframes: AtomicU64,
    /// What this cohort's refining passes cost, tail passes left out.
    paintover_bytes: AtomicU64,
    /// The last refining pass left something a tail pass could still sharpen, at the tail's
    /// budget and at the ladder's.
    refinable: AtomicBool,
    refinable_small: AtomicBool,
    /// The most this cohort takes frames at: its cap until encodes can't keep up, then
    /// `base_fps` until the encoder reopens.
    fps: AtomicU32,
    base_fps: u32,
    w: u32,
    h: u32,
}

/// Consecutive failed encodes after which a cohort's viewers move to their next codec.
const FAIL_AFTER: u32 = 3;
/// How often a cohort past FAIL_AFTER with nothing to fall back to (the last H.264
/// encoder) gets a keyframe; those wait for flow control and get no keyframe boost.
const RETRY_EVERY: Duration = Duration::from_secs(1);

impl Shared {
    /// A motion frame took `ms` to encode at `now`. Encodes that keep missing the cap's
    /// frame interval drop the cohort to its base rate, for good: winning the cap back
    /// mid-session isn't worth oscillating between the two.
    fn encoded(&self, watch: &mut SlowWatch, ms: f64, now: Instant) {
        let fps = self.fps.load(Ordering::Acquire);
        if fps > self.base_fps && watch.note(ms, fps, now) {
            self.fps.store(self.base_fps, Ordering::Release);
            log::info!(
                "cohort {}: {fps} -> {} fps (encode {:.1} ms)",
                self.id,
                self.base_fps,
                watch.ewma_ms.unwrap_or(ms)
            );
        }
    }

    /// A stripe keeps failing: keyframes are no longer a way to recover quickly.
    fn degraded(&self) -> bool {
        self.failures.load(Ordering::Acquire) >= FAIL_AFTER
    }

    /// The stripes' streams told their parameters (on their first frame or when they
    /// changed). Where they differ from what was announced, the cohort's protocol 2
    /// viewers get a new epoch and the frames again once ready for it.
    fn reconfigure(&self, hub: &Hub, configs: &[Option<StreamConfig>]) {
        let offer = {
            let mut o = self.offer.lock();
            let mut changed = false;
            for (s, c) in o.stripes.iter_mut().zip(configs) {
                if let Some(c) = c {
                    let d = DecoderConfig::of(c);
                    if s.config != d {
                        log::info!(
                            "cohort {}: stripe {} is now {} {}x{}",
                            self.id,
                            s.i,
                            d.codec,
                            d.coded_width,
                            d.coded_height
                        );
                        s.config = d;
                        changed = true;
                    }
                }
            }
            if !changed {
                return;
            }
            o.clone()
        };
        let clients = hub.clients.lock().clone();
        for c in clients {
            let mut r = c.route.lock();
            if r.cohort != Some(self.id) || r.proto != Proto::V2 {
                continue;
            }
            r.epoch = r.epoch.wrapping_add(1);
            r.phase = Phase::Announced(Instant::now());
            c.inflight.lock().clear();
            c.started.store(false, Ordering::Relaxed);
            c.needs_keyframe.store(false, Ordering::Relaxed);
            c.send(Out::Text(offer.announce(r.epoch, self.w, self.h)));
        }
    }
}

/// Encodes averaging more than this share of the frame interval can't keep up with the cap.
const SLOW_SHARE: f64 = 0.8;
/// For this long.
const SLOW_FOR: Duration = Duration::from_secs(2);

/// Motion encode times against a cap's frame interval.
#[derive(Default)]
struct SlowWatch {
    ewma_ms: Option<f64>,
    over_since: Option<Instant>,
    last: Option<Instant>,
}

impl SlowWatch {
    /// True once encodes have been too slow for `fps` for SLOW_FOR; a pause of a second
    /// or more starts the count over.
    fn note(&mut self, ms: f64, fps: u32, now: Instant) -> bool {
        let e = self.ewma_ms.map_or(ms, |e| e * 0.8 + ms * 0.2);
        self.ewma_ms = Some(e);
        if self
            .last
            .is_some_and(|l| now.saturating_duration_since(l) >= Duration::from_secs(1))
        {
            self.over_since = None;
        }
        self.last = Some(now);
        if e <= SLOW_SHARE * 1000.0 / fps.max(1) as f64 {
            self.over_since = None;
            return false;
        }
        now.saturating_duration_since(*self.over_since.get_or_insert(now)) >= SLOW_FOR
    }
}

/// One encoder per stripe for one candidate, on its own thread.
struct Cohort {
    shared: Arc<Shared>,
    tx: Option<mpsc::SyncSender<Job>>,
    thread: Option<std::thread::JoinHandle<()>>,
    rc: RateCtl,
    skipped: Option<Skipped>,
    /// Had viewers at the last frame.
    active: bool,
    /// Who they were.
    viewers: Vec<u64>,
    refine: Refinement,
    pace: Pace,
    /// The capture this cohort last took a frame from.
    last_take: Option<Instant>,
}

/// The slowest rate an encoder budgets frames for: no frame may take more than a tenth of a
/// second's worth of the cap.
const FPS_FLOOR: u32 = 10;

/// How often a cohort takes a frame while the picture moves. Flow control and a busy encoder
/// skip frames, and an encoder that budgets for every capture would starve the ones that go out.
struct Pace {
    last: Option<Instant>,
    /// Smoothed interval between frames taken in motion (ms); pauses of a second or more are stills.
    interval_ms: Option<f64>,
    fps: u32,
    changed: Instant,
}

impl Pace {
    fn new(fps: u32) -> Self {
        Pace {
            last: None,
            interval_ms: None,
            fps,
            changed: Instant::now(),
        }
    }

    /// A motion frame went to the encoder at `now`. Changes the budgeted rate at most once a
    /// second, and only by a quarter or more.
    fn took(&mut self, now: Instant, max: u32, cohort: u64) {
        if let Some(gap) = self
            .last
            .map(|l| now.saturating_duration_since(l).as_secs_f64() * 1000.0)
        {
            if gap < 1000.0 {
                self.interval_ms = Some(self.interval_ms.map_or(gap, |i| i * 0.8 + gap * 0.2));
            }
        }
        self.last = Some(now);
        let Some(interval) = self.interval_ms else {
            return;
        };
        let want = ((1000.0 / interval.max(1.0)).round() as u32).clamp(FPS_FLOOR.min(max), max);
        let step = (want as f64 - self.fps as f64).abs() >= self.fps as f64 * 0.25;
        if step && now.saturating_duration_since(self.changed) >= Duration::from_secs(1) {
            log::info!(
                "cohort {cohort}: encoder rate {} -> {want} fps (a frame every {interval:.0} ms)",
                self.fps
            );
            self.fps = want;
            self.changed = now;
        }
    }
}

impl Cohort {
    fn fps(&self) -> u32 {
        self.shared.fps.load(Ordering::Acquire)
    }

    /// Whether what was encoded so far has reached this cohort's viewers. One that hasn't
    /// answered for a while holds back nobody: flow control deals with it.
    fn drained(&self, v: &View) -> bool {
        self.shared.busy.load(Ordering::Acquire) == 0
            && v.clients.iter().all(|c| {
                let (n, age) = c.backlog();
                n == 0 || age >= REFINE_DRAIN_MS
            })
    }

    /// One pass in flight at most: what a viewer types next never waits behind two. Nor does
    /// one go ahead of a keyframe that landed since the last look, which starts the ladder over.
    fn pass_due(&self, v: &View, now: Instant) -> bool {
        let r = &self.refine;
        r.burst.due(now)
            && (!r.burst.ladder
                || (self.drained(v)
                    && r.keyframes == self.shared.keyframes.load(Ordering::Acquire)))
    }
}

impl Drop for Cohort {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// A cohort's viewers that get frames now, and whether one is waiting for a keyframe.
struct View {
    clients: Vec<Arc<Client>>,
    want_key: bool,
}

fn encoder_config(
    cfg: &VideoConfig,
    cand: Candidate,
    width: u32,
    height: u32,
    stripes: usize,
) -> EncoderConfig {
    let fps = fps_cap(cfg, cand);
    EncoderConfig {
        backend: cand.backend,
        codec: cand.codec,
        width,
        height,
        fps,
        threads: if cfg.adaptive_threads {
            1
        } else {
            (cfg.threads / stripes as u32).max(1)
        },
        rate: RateConfig::new(cfg.crf, cfg.max_kbps, fps),
        best_quality: cfg.crf,
        preset: cfg.preset.clone(),
    }
}

/// Each stripe may use up to twice its area share of the budget; the delay-based
/// controller reins the total in if several stripes move at once. 0 = uncapped.
fn stripe_share(target: u32, n: usize) -> u32 {
    if target == 0 {
        return 0;
    }
    ((target as u64 * 2) / n as u64).max(target as u64 / n as u64 + 1) as u32
}

/// A keyframe is what a new viewer sees first; don't starve it to the steady-state
/// frame budget (up to ~8 frames' worth at 4x the cap). An uncapped stream (cap 0)
/// stays uncapped.
fn keyframe_rate(crf: f32, cap: u32, fps: u32) -> RateConfig {
    let boost = if cap == 0 {
        0
    } else {
        cap.saturating_mul(4).max(16_000)
    };
    RateConfig {
        quality: crf,
        max_kbps: boost,
        buf_kbit: boost / fps.max(1) * 8,
        fps,
    }
}

/// The configuration a stream from `ecfg` starts with, read from an IDR encoded at the
/// rate a cohort's first keyframe gets (encoders derive the level from it).
fn sample(
    open: &Open,
    ecfg: &EncoderConfig,
    cfg: &VideoConfig,
    n: usize,
) -> Result<StreamConfig, String> {
    let mut enc = open(ecfg)?;
    let target = if cfg.max_kbps == 0 {
        0
    } else {
        stripe_share(START_KBPS.min(cfg.max_kbps as f64) as u32, n)
    };
    enc.set_rate(keyframe_rate(cfg.crf, target, ecfg.fps))?;
    let (w, h) = (ecfg.width as usize, ecfg.height as usize);
    let (y, uv) = (
        vec![128u8; w * h],
        vec![128u8; w.div_ceil(2) * h.div_ceil(2)],
    );
    let frame = FrameView {
        width: ecfg.width,
        height: ecfg.height,
        y: &y,
        u: &uv,
        v: &uv,
        stride_y: w,
        stride_uv: w.div_ceil(2),
    };
    let mut aus = enc.encode(
        &frame,
        EncodeOptions {
            force_keyframe: true,
            ..Default::default()
        },
    )?;
    aus.extend(enc.drain()?);
    aus.into_iter()
        .find_map(|a| a.config)
        .ok_or_else(|| "its first picture carried no stream configuration".into())
}

/// What `cand` streams at `w`x`h`: its stripes and their decoder configurations.
fn make_offer(
    cfg: &VideoConfig,
    open: &Open,
    cand: Candidate,
    w: u32,
    h: u32,
) -> Result<Offer, String> {
    // Hardware sessions are scarce: one picture unless it is too large for one.
    let want = if cand.hardware && cand.codec != Codec::H264 {
        1
    } else {
        cfg.stripes
    };
    let layout = fit_stripes(cand.backend, w, h, want);
    if layout.len() as u32 > want.clamp(1, 16) {
        log::info!(
            "{}x{} is too large for one {} picture: {} stripes",
            w,
            h,
            cand.backend.name(),
            layout.len()
        );
    }
    let mut seen: Vec<(u32, StreamConfig)> = Vec::new();
    let mut stripes = Vec::new();
    for (i, &(y, sh)) in layout.iter().enumerate() {
        let sc = match seen.iter().find(|(h, _)| *h == sh) {
            Some((_, c)) => c.clone(),
            None => {
                let c = sample(
                    open,
                    &encoder_config(cfg, cand, w, sh, layout.len()),
                    cfg,
                    layout.len(),
                )?;
                seen.push((sh, c.clone()));
                c
            }
        };
        stripes.push(StripeOffer {
            i,
            y,
            h: sh,
            config: DecoderConfig::of(&sc),
        });
    }
    let format = seen
        .first()
        .map(|(_, c)| negotiate::format_name(c.format))
        .ok_or("nothing to encode")?;
    Ok(Offer {
        codec: cand.codec,
        format,
        stripes,
        cand,
    })
}

/// A viewer as one routing pass sees it.
struct Snap {
    client: Arc<Client>,
    legacy: bool,
    caps: Vec<DecodeCap>,
    rejected: Vec<Choice>,
}

/// Routes viewers to cohorts and feeds the cohorts; owned by the capture thread, which
/// makes it the one place configurations change.
struct Engine {
    hub: Arc<Hub>,
    cfg: VideoConfig,
    w: u32,
    h: u32,
    h264: Candidate,
    offers: Vec<Offer>,
    /// Candidates whose encoders failed to open at this size.
    failed: Vec<Candidate>,
    cohorts: Vec<Cohort>,
    best: Option<Candidate>,
    open: Open,
    last_tick: Instant,
}

impl Engine {
    /// Offers every candidate that encodes at `w`x`h`, opens the H.264 cohort, and gives
    /// connected clients the new geometry and offers.
    fn new(
        cfg: VideoConfig,
        hub: Arc<Hub>,
        w: u32,
        h: u32,
        cands: &[Candidate],
        open: Open,
    ) -> Result<Engine, String> {
        let mut offers = Vec::new();
        let mut h264_err = String::from("no H.264 encoder");
        for &c in cands {
            match make_offer(&cfg, &open, c, w, h) {
                Ok(o) => {
                    log::info!(
                        "offering {:?} from {}: {}",
                        c.codec,
                        c.backend.name(),
                        o.stripes
                            .iter()
                            .map(|s| format!(
                                "{} {}x{}",
                                s.config.codec, s.config.coded_width, s.config.coded_height
                            ))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    offers.push(o)
                }
                Err(e) => {
                    log::warn!(
                        "{:?} from {} is not offered at {w}x{h}: {e}",
                        c.codec,
                        c.backend.name()
                    );
                    if c.codec == Codec::H264 {
                        h264_err = e;
                    }
                }
            }
        }
        let h264 = offers
            .iter()
            .map(|o| o.cand)
            .find(|c| c.codec == Codec::H264)
            .ok_or(h264_err)?;
        let mut eng = Engine {
            hub: hub.clone(),
            cfg,
            w,
            h,
            h264,
            offers,
            failed: Vec::new(),
            cohorts: Vec::new(),
            best: None,
            open,
            last_tick: Instant::now(),
        };
        let c = loop {
            match eng.open_cohort(eng.h264) {
                Ok(c) => break c,
                Err(e) => {
                    log::warn!("H264 from {} failed to open: {e}", eng.h264.backend.name());
                    eng.failed.push(eng.h264);
                    eng.h264 = eng.next_h264().ok_or(e)?;
                }
            }
        };
        *hub.stripes.lock() = c
            .shared
            .offer
            .lock()
            .stripes
            .iter()
            .map(|s| (s.y, s.h))
            .collect();
        eng.cohorts.push(c);
        *hub.offers.lock() = eng.offers.clone();
        hub.offer_gen.fetch_add(1, Ordering::Relaxed);
        eng.publish();
        // New geometry and offers (and a keyframe) for clients already connected; no-op at startup.
        hub.restart_clients();
        Ok(eng)
    }

    /// The H.264 candidate to fall back to from the current one.
    fn next_h264(&self) -> Option<Candidate> {
        self.offers
            .iter()
            .map(|o| o.cand)
            .find(|c| c.codec == Codec::H264 && *c != self.h264 && !self.failed.contains(c))
    }

    /// The H.264 cohort's encoder failed: the next H.264 encoder that opens takes its
    /// viewers. If none does, the failed one stays, as the last resort.
    fn replace_h264(&mut self) {
        let old = self.h264;
        while let Some(n) = self.next_h264() {
            if !self.cohorts.iter().any(|c| c.shared.cand == n) {
                match self.open_cohort(n) {
                    Ok(c) => self.cohorts.push(c),
                    Err(e) => {
                        log::warn!("H264 from {} failed to open: {e}", n.backend.name());
                        self.failed.push(n);
                        continue;
                    }
                }
            }
            log::warn!(
                "H264 from {} failed; falling back to {}",
                old.backend.name(),
                n.backend.name()
            );
            self.h264 = n;
            if self.best == Some(n) {
                self.best = None;
            }
            let stripes: Vec<(u32, u32)> = self
                .cohorts
                .iter()
                .filter(|c| c.shared.cand == n)
                .flat_map(|c| {
                    c.shared
                        .offer
                        .lock()
                        .stripes
                        .iter()
                        .map(|s| (s.y, s.h))
                        .collect::<Vec<_>>()
                })
                .collect();
            if *self.hub.stripes.lock() != stripes {
                *self.hub.stripes.lock() = stripes;
                self.hub.restart_clients();
            }
            return;
        }
        self.failed.retain(|c| *c != old);
    }

    fn open_cohort(&self, cand: Candidate) -> Result<Cohort, String> {
        let offer = self
            .offers
            .iter()
            .find(|o| o.cand == cand)
            .ok_or("not offered")?
            .clone();
        let n = offer.stripes.len();
        let mut stripes = Vec::new();
        for s in &offer.stripes {
            let ecfg = encoder_config(&self.cfg, cand, self.w, s.h, n);
            stripes.push(Stripe {
                y: s.y as usize,
                h: s.h as usize,
                enc: (self.open)(&ecfg)?,
                cfg: ecfg,
                max_threads: if self.cfg.adaptive_threads {
                    (self.cfg.threads / n as u32).max(1)
                } else {
                    1
                },
                applied_kbps: None,
                applied_crf: -1.0,
                applied_fps: 0,
                open: self.open.clone(),
            });
        }
        let rc = RateCtl::new(self.cfg.max_kbps as f64, self.cfg.min_kbps as f64);
        let fps = fps_cap(&self.cfg, cand);
        let shared = Arc::new(Shared {
            id: self.hub.next_cohort.fetch_add(1, Ordering::Relaxed),
            cand,
            offer: Mutex::new(offer),
            busy: AtomicUsize::new(0),
            idr_inflight: AtomicBool::new(false),
            target: AtomicU32::new(rc.target as u32),
            failures: AtomicU32::new(0),
            last_key: Mutex::new(Instant::now() - RETRY_EVERY),
            keyframes: AtomicU64::new(0),
            paintover_bytes: AtomicU64::new(0),
            refinable: AtomicBool::new(true),
            refinable_small: AtomicBool::new(true),
            fps: AtomicU32::new(fps),
            base_fps: base_fps(&self.cfg),
            w: self.w,
            h: self.h,
        });
        let (tx, rx) = mpsc::sync_channel::<Job>(1);
        let (hub, sh) = (self.hub.clone(), shared.clone());
        let (crf, po_crf, uncapped) =
            (self.cfg.crf, self.cfg.paintover_crf, self.cfg.max_kbps == 0);
        let thread = std::thread::Builder::new()
            .name(format!("encode-{}", shared.id))
            .spawn(move || encode_loop(stripes, rx, hub, sh, crf, po_crf, uncapped))
            .map_err(|e| e.to_string())?;
        log::info!(
            "cohort {}: {:?} from {}, {} stripe(s) at {}x{}",
            shared.id,
            cand.codec,
            cand.backend.name(),
            n,
            self.w,
            self.h
        );
        if fps > shared.base_fps {
            log::info!("cohort {}: up to {fps} fps (--high-fps)", shared.id);
        }
        Ok(Cohort {
            shared,
            tx: Some(tx),
            thread: Some(thread),
            rc,
            skipped: None,
            active: false,
            viewers: Vec::new(),
            refine: Refinement::new(&self.cfg, cand.backend.refines_by_qp(), self.w, self.h),
            pace: Pace::new(fps),
            last_take: None,
        })
    }

    /// Routes the viewers again when one asked for it (or a codec-ready is overdue):
    /// opens the best cohort the plan wants, falls back to the next candidate if its
    /// encoders fail to open, and closes cohorts nobody uses.
    fn replan(&mut self) {
        let due = self.last_tick.elapsed() >= TICK;
        if !self.hub.replan.swap(false, Ordering::AcqRel) && !due {
            return;
        }
        if due {
            self.last_tick = Instant::now();
            self.expire();
        }
        // A cohort whose encoder keeps failing: its candidate is off until the next size.
        // The last H.264 encoder is the last resort and stays.
        let h264_fallback = self.next_h264().is_some();
        for c in &self.cohorts {
            let sh = &c.shared;
            if (sh.cand != self.h264 || h264_fallback)
                && sh.failures.load(Ordering::Acquire) >= FAIL_AFTER
                && !self.failed.contains(&sh.cand)
            {
                log::warn!(
                    "{:?} from {} keeps failing to encode; trying the next encoder",
                    sh.cand.codec,
                    sh.cand.backend.name()
                );
                self.failed.push(sh.cand);
            }
        }
        if self.hub.fail_h264.swap(false, Ordering::AcqRel) && !self.failed.contains(&self.h264) {
            self.failed.push(self.h264);
        }
        if self.failed.contains(&self.h264) {
            self.replace_h264();
        }
        let failed = self.failed.clone();
        self.cohorts.retain(|c| !failed.contains(&c.shared.cand));
        if self.best.is_some_and(|b| failed.contains(&b)) {
            self.best = None;
        }
        let gen = self.hub.offer_gen.load(Ordering::Relaxed);
        loop {
            let clients = self.hub.video_clients();
            let snaps: Vec<Snap> = clients
                .into_iter()
                .filter_map(|c| {
                    let r = c.route.lock();
                    let (legacy, caps, rejected) = match r.proto {
                        Proto::Legacy => (true, Vec::new(), Vec::new()),
                        Proto::V2 if r.caps_gen == gen => {
                            (false, r.caps.clone(), r.rejected.clone())
                        }
                        _ => return None,
                    };
                    drop(r);
                    Some(Snap {
                        client: c,
                        legacy,
                        caps,
                        rejected,
                    })
                })
                .collect();
            let offers: Vec<Offer> = self
                .offers
                .iter()
                .filter(|o| !self.failed.contains(&o.cand))
                .cloned()
                .collect();
            let viewers: Vec<negotiate::Viewer> = snaps
                .iter()
                .map(|s| negotiate::Viewer {
                    id: s.client.id,
                    legacy: s.legacy,
                    caps: &s.caps,
                    rejected: &s.rejected,
                })
                .collect();
            let plan = negotiate::plan(&viewers, &offers, self.h264, self.best);
            if let Some(b) = plan.best {
                if !self.cohorts.iter().any(|c| c.shared.cand == b) {
                    match self.open_cohort(b) {
                        Ok(c) => self.cohorts.push(c),
                        Err(e) => {
                            log::warn!(
                                "{:?} from {} failed to open: {e}; trying the next codec",
                                b.codec,
                                b.backend.name()
                            );
                            self.failed.push(b);
                            continue;
                        }
                    }
                }
            }
            self.best = plan.best;
            let clients: Vec<Arc<Client>> = snaps.into_iter().map(|s| s.client).collect();
            self.apply(&clients, &plan.picks);
            break;
        }
        let (h264, best) = (self.h264, self.best);
        self.cohorts
            .retain(|c| c.shared.cand == h264 || Some(c.shared.cand) == best);
        self.publish();
    }

    /// Moves each viewer to its pick. A protocol 2 viewer gets a new epoch and the
    /// `codec` message, and the cohort's frames once it is ready for them.
    fn apply(&self, clients: &[Arc<Client>], picks: &[(u64, Option<Choice>)]) {
        for (c, (_, pick)) in clients.iter().zip(picks) {
            let mut r = c.route.lock();
            let target = pick.and_then(|p| {
                self.cohorts
                    .iter()
                    .find(|k| k.shared.cand == p.cand)
                    .map(|k| (p, &k.shared))
            });
            let Some((p, sh)) = target else {
                if r.proto == Proto::V2 && r.phase != Phase::NoCodec {
                    r.epoch = r.epoch.wrapping_add(1);
                    r.detach();
                    r.phase = Phase::NoCodec;
                    c.send(Out::Text(negotiate::no_codec(r.epoch)));
                    log::info!("client {}: no codec in common", c.id);
                }
                continue;
            };
            let id = sh.id;
            if r.cohort == Some(id) && r.choice == Some(p) {
                continue;
            }
            r.cohort = Some(id);
            r.choice = Some(p);
            c.inflight.lock().clear();
            c.started.store(false, Ordering::Relaxed);
            if r.proto == Proto::V2 {
                r.epoch = r.epoch.wrapping_add(1);
                r.phase = Phase::Announced(Instant::now());
                c.needs_keyframe.store(false, Ordering::Relaxed);
                // Read with the route held: a reconfigure that updates the offer after
                // this finds the viewer attached and announces again.
                let msg = sh.offer.lock().announce(r.epoch, self.w, self.h);
                c.send(Out::Text(msg));
                log::info!(
                    "client {}: {:?} from {} (epoch {}, {:?})",
                    c.id,
                    p.cand.codec,
                    p.cand.backend.name(),
                    r.epoch,
                    p.implementation
                );
            } else {
                r.phase = Phase::Ready;
                c.needs_keyframe.store(true, Ordering::Relaxed);
            }
        }
    }

    /// A viewer that hasn't confirmed its `codec` in time is moved to its next choice.
    fn expire(&self) {
        for c in self.hub.video_clients() {
            let mut r = c.route.lock();
            if let (Phase::Announced(t), Some(ch)) = (r.phase, r.choice) {
                if t.elapsed() >= READY_TIMEOUT {
                    log::warn!(
                        "client {}: no codec-ready for epoch {} in {} s; trying the next codec",
                        c.id,
                        r.epoch,
                        READY_TIMEOUT.as_secs()
                    );
                    if !r.rejected.contains(&ch) {
                        r.rejected.push(ch);
                    }
                    r.detach();
                }
            }
        }
    }

    /// The cohorts as /stats shows them.
    fn publish(&self) {
        let v = self
            .cohorts
            .iter()
            .map(|c| {
                let s = &c.shared;
                serde_json::json!({
                    "id": s.id,
                    "codec": s.cand.codec,
                    "backend": s.cand.backend,
                    "hardware": s.cand.hardware,
                    "viewers": self.hub.cohort_clients(s.id).len(),
                    "target_kbps": s.target.load(Ordering::Relaxed),
                    "fps": c.fps(),
                    "stripes": s.offer.lock().stripes,
                })
            })
            .collect();
        *self.hub.cohorts.lock() = serde_json::Value::Array(v);
        let live: Vec<Candidate> = self
            .offers
            .iter()
            .map(|o| o.cand)
            .filter(|c| !self.failed.contains(c))
            .collect();
        *self.hub.encoder.lock() = serde_json::json!({
            "mode": self.cfg.selection.name(),
            "chosen": negotiate::chosen(&live),
        });
    }

    fn views(&self) -> Vec<View> {
        self.cohorts
            .iter()
            .map(|c| {
                let clients = self.hub.cohort_clients(c.shared.id);
                let sh = &c.shared;
                let want_key = !sh.idr_inflight.load(Ordering::Acquire)
                    && (!sh.degraded() || sh.last_key.lock().elapsed() >= RETRY_EVERY)
                    && clients
                        .iter()
                        .any(|x| x.needs_keyframe.load(Ordering::Relaxed));
                View { clients, want_key }
            })
            .collect()
    }

    /// Each cohort's bitrate from its own viewers; a cohort that lost them all starts over.
    fn update_rates(&mut self, views: &[View]) {
        let mut shown = 0;
        for (c, v) in self.cohorts.iter_mut().zip(views) {
            // All of them gone and others come since the last look is a leave and a join.
            let replaced = c.active
                && !v.clients.is_empty()
                && !v.clients.iter().any(|x| c.viewers.contains(&x.id));
            let joined = !v.clients.is_empty() && (!c.active || replaced);
            if c.active && (v.clients.is_empty() || replaced) {
                c.rc = RateCtl::new(self.cfg.max_kbps as f64, self.cfg.min_kbps as f64);
                c.refine.reset();
                c.skipped = None;
            }
            if v.clients.is_empty() {
                c.active = false;
                c.skipped = None;
                c.refine.baseline(&c.shared);
            } else {
                c.active = true;
                c.rc.update(&v.clients);
            }
            if joined {
                c.refine.baseline(&c.shared);
                c.refine.rearm();
            }
            c.viewers.clear();
            c.viewers.extend(v.clients.iter().map(|x| x.id));
            c.shared.target.store(c.rc.target as u32, Ordering::Relaxed);
            if c.active {
                shown = shown.max(c.rc.target as u32);
            }
        }
        if shown == 0 {
            shown = self.cohorts.first().map_or(0, |c| c.rc.target as u32);
        }
        self.hub.target_kbps.store(shown, Ordering::Relaxed);
    }

    /// The rate the capture runs at: the highest cap of a cohort with viewers.
    fn capture_fps(&self, views: &[View]) -> u32 {
        self.cohorts
            .iter()
            .zip(views)
            .filter(|(_, v)| !v.clients.is_empty())
            .map(|(c, _)| c.fps())
            .max()
            .unwrap_or(self.cfg.fps)
    }

    /// Which cohorts could take the next capture: those with viewers, room in their queue,
    /// and either a keyframe owed or their viewers keeping up. A cohort capped below the
    /// capture's `interval_ms` keeps up at its own cap's interval.
    fn takes(&self, views: &[View], interval_ms: f64) -> Vec<bool> {
        let capture = self.capture_fps(views);
        self.cohorts
            .iter()
            .zip(views)
            .map(|(c, v)| {
                let ms = match c.fps() < capture {
                    true => 1000.0 / c.fps().max(1) as f64,
                    false => interval_ms,
                };
                !v.clients.is_empty()
                    && c.shared.busy.load(Ordering::Acquire) < 2
                    && ((v.want_key && !c.shared.degraded()) || flow_ok(&v.clients, ms))
            })
            .collect()
    }

    /// Cohorts capped below the capture rate whose last frame is too recent for their cap
    /// (with the capture's early allowance).
    fn early(&self, views: &[View], now: Instant) -> Vec<bool> {
        let capture = self.capture_fps(views);
        self.cohorts
            .iter()
            .map(|c| {
                let fps = c.fps();
                fps < capture
                    && c.last_take.is_some_and(|t| {
                        now.saturating_duration_since(t).as_secs_f64() < 0.85 / fps.max(1) as f64
                    })
            })
            .collect()
    }

    /// What this turn of the capture loop has to do besides any new damage.
    fn turn(&mut self, views: &[View], interval_ms: f64, now: Instant) -> Turn {
        self.update_refinement(views, now);
        let ready = self.takes(views, interval_ms);
        let early = self.early(views, now);
        let takes: Vec<bool> = ready.iter().zip(&early).map(|(&r, &e)| r && !e).collect();
        let passes = self.passes(views, &takes, now);
        let catch_up = self.catch_up_due(&ready);
        Turn {
            want_key: views.iter().any(|v| v.want_key),
            catch_up,
            paced: ready.iter().zip(&early).any(|(&r, &e)| r && e),
            takes,
            passes,
            at: now,
        }
    }

    /// Brings each watched cohort's refinement up to date with what its encoder did.
    fn update_refinement(&mut self, views: &[View], now: Instant) {
        let capped = self.cfg.max_kbps != 0;
        let input_at = *self.hub.input_at.lock();
        for (c, v) in self.cohorts.iter_mut().zip(views) {
            if !v.clients.is_empty() {
                // --max-kbps 0: the controller's floor applies after congestion, the budget doesn't.
                let kbps = if capped { c.rc.target as u32 } else { 0 };
                c.refine.update(&c.shared, kbps, now, input_at);
            }
        }
    }

    /// Which cohorts have a pass to take from this capture.
    fn passes(&self, views: &[View], takes: &[bool], now: Instant) -> Vec<bool> {
        self.cohorts
            .iter()
            .zip(views)
            .zip(takes)
            .map(|((c, v), &take)| take && c.pass_due(v, now))
            .collect()
    }

    /// How long until a cohort's pass can come due, in ms.
    fn refine_wait_ms(&self, views: &[View], now: Instant) -> i32 {
        self.cohorts
            .iter()
            .zip(views)
            .filter(|(_, v)| !v.clients.is_empty())
            .map(|(c, _)| match c.refine.burst.due(now) {
                true => 100,
                false => c.refine.burst.wait_ms(now),
            })
            .min()
            .unwrap_or(100)
    }

    /// What changed in this capture, for the cohorts with viewers (None: everything).
    fn mark(&mut self, views: &[View], dirty: Option<&[u8]>) {
        for (c, v) in self.cohorts.iter_mut().zip(views) {
            if !v.clients.is_empty() {
                match dirty {
                    Some(m) => c.refine.refiner.mark(m),
                    None => c.refine.refiner.mark_all(),
                }
            }
        }
    }

    /// The picture moved: every watched cohort's next burst starts over.
    fn moved(&mut self, views: &[View], now: Instant) {
        for (c, v) in self.cohorts.iter_mut().zip(views) {
            if !v.clients.is_empty() {
                c.refine.burst.moved(now);
            }
        }
    }

    /// Whether a cohort that skipped damage has room for it now: worth a capture even
    /// when nothing else changed, or that cohort's viewers keep stale regions.
    fn catch_up_due(&self, takes: &[bool]) -> bool {
        self.cohorts
            .iter()
            .zip(takes)
            .any(|(c, &take)| take && c.skipped.is_some())
    }

    /// Gives each cohort what this capture means for it; `f` is the capture itself. Motion
    /// reaches every watched cohort, whether or not it can take the frame.
    fn dispatch(&mut self, f: &Frame, views: &[View], turn: &Turn, now: Instant) {
        if f.band.is_some() {
            self.moved(views, now);
        }
        let input_at = *self.hub.input_at.lock();
        // While someone types, frames keep the full rate's budget: a key never waits behind a
        // frame sized for a slower rate. Pointer and wheel motion keep the rate frames are taken at.
        let typing = self
            .hub
            .key_at
            .lock()
            .is_some_and(|k| now.saturating_duration_since(k) < REFINE_IDLE);
        let stats = &self.hub.stats;
        let plans = turn.takes.iter().zip(&turn.passes);
        for ((c, v), (&take, &pass)) in self.cohorts.iter_mut().zip(views).zip(plans) {
            if v.clients.is_empty() {
                continue;
            }
            if !take {
                if f.band.is_some() {
                    c.skipped = Some(skip(c.skipped.take(), f));
                }
                continue;
            }
            let force = v.want_key;
            let max_fps = c.fps();
            let Some((mut frame, step)) = c.refine.plan(f, force, c.skipped.is_some(), pass) else {
                continue;
            };
            frame.refine_idle = frame.refine_tail && !c.refine.active(now, input_at);
            let Some(tx) = c.tx.as_ref() else { continue };
            let frame = match c.skipped.take() {
                Some(s) => catch_up(s, frame),
                None => frame,
            };
            if force {
                c.shared.idr_inflight.store(true, Ordering::Release);
                *c.shared.last_key.lock() = Instant::now();
            }
            c.shared.busy.fetch_add(1, Ordering::AcqRel);
            let motion = frame.band.is_some() && !matches!(step, Step::Pass);
            match tx.try_send(Job {
                frame,
                force_idr: force,
                boost: force && !c.shared.degraded(),
                fps: if typing {
                    max_fps
                } else {
                    c.pace.fps.min(max_fps)
                },
            }) {
                Ok(()) => {
                    c.last_take = Some(turn.at);
                    if motion {
                        c.pace.took(now, max_fps, c.shared.id);
                    }
                    c.refine.sent(step, stats, now)
                }
                Err(e) => {
                    c.shared.busy.fetch_sub(1, Ordering::AcqRel);
                    if force {
                        c.shared.idr_inflight.store(false, Ordering::Release);
                    }
                    let (mpsc::TrySendError::Full(j) | mpsc::TrySendError::Disconnected(j)) = e;
                    if !matches!(step, Step::Pass) && j.frame.band.is_some() {
                        c.skipped = Some(skip(None, &j.frame));
                    }
                }
            }
        }
    }
}

/// What a turn of the capture loop owes the cohorts.
struct Turn {
    takes: Vec<bool>,
    passes: Vec<bool>,
    want_key: bool,
    catch_up: bool,
    /// A cohort would take this capture but for its cap.
    paced: bool,
    at: Instant,
}

impl Turn {
    /// Something is due even if nothing changed on screen.
    fn work(&self) -> bool {
        self.want_key || self.catch_up || self.passes.contains(&true)
    }
}

/// One capture/encode setup for the current screen size.
struct Pipeline {
    cap: Capturer,
    engine: Engine,
}

/// Runs the pipeline. With a `Screen` (dynamic resolution), a client's size request
/// ends the capture loop: the encoders drain, the screen is resized and the pipeline
/// is rebuilt at the new size, and clients get the new geometry and a keyframe.
pub fn spawn(
    cfg: VideoConfig,
    hub: Arc<Hub>,
    mut screen: Option<crate::screen::Screen>,
    cands: Vec<Candidate>,
) -> Result<(), String> {
    if let Some(s) = &screen {
        *hub.resize_max.lock() = Some(s.max());
    }
    let open: Open = Arc::new(|c: &EncoderConfig| encoder::open(c));
    let mut p = build(&cfg, &hub, &cands, &open)?;
    std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || loop {
            let Pipeline {
                mut cap,
                mut engine,
            } = p;
            if let Err(e) = capture_loop(&mut cap, cfg.clone(), hub.clone(), &mut engine) {
                #[cfg(target_os = "linux")]
                let resized = cap.geometry_changed();
                #[cfg(not(target_os = "linux"))]
                let resized = false;
                if resized {
                    log::info!("capture geometry changed: {e}");
                } else {
                    log::error!("capture loop ended: {e}");
                    std::process::exit(1);
                }
            }
            drop(cap);
            drop(engine);
            let req = hub.resize_req.lock().take();
            if let (Some(s), Some((w, h))) = (screen.as_mut(), req) {
                if let Err(e) = s.resize(w, h) {
                    log::warn!("resize to {w}x{h} failed: {e}");
                }
            }
            p = match build(&cfg, &hub, &cands, &open) {
                Ok(p) => p,
                Err(e) => {
                    log::error!("video restart failed: {e}");
                    std::process::exit(1);
                }
            };
        })
        .unwrap();
    Ok(())
}

fn build(
    cfg: &VideoConfig,
    hub: &Arc<Hub>,
    cands: &[Candidate],
    open: &Open,
) -> Result<Pipeline, String> {
    let cap = Capturer::new(cfg.display.as_deref())?;
    let (w, h) = (cap.width, cap.height);
    hub.width.store(w, Ordering::Relaxed);
    hub.height.store(h, Ordering::Relaxed);
    hub.activity.reset(w, h);
    log::info!(
        "capturing {}x{} @ {} fps, {} encoder threads",
        w,
        h,
        cfg.fps,
        cfg.threads
    );
    let engine = Engine::new(cfg.clone(), hub.clone(), w, h, cands, open.clone())?;
    Ok(Pipeline { cap, engine })
}

/// No ack for this long with frames in flight: the client is gone, not slow.
const STALL_MS: u128 = 4000;

fn flow_ok(clients: &[Arc<Client>], interval_ms: f64) -> bool {
    for c in clients {
        let (n, age) = c.backlog();
        if n == 0 {
            continue;
        }
        let rtt = match c.min_rtt_ms.load(Ordering::Relaxed) {
            0 => 40,
            r => r,
        } as f64;
        // Enough frames in flight to cover one RTT plus decode, but never more than ~rtt+120ms of queue.
        let max_n = (((rtt + 40.0) / interval_ms).ceil() as usize + 2).clamp(3, 40);
        let max_age = rtt as u128 + 120;
        let silence = c.ack_silence_ms();
        if silence > STALL_MS {
            // Stalled client (hidden tab, dead socket): stop letting it gate everyone.
            log::warn!(
                "client {} stalled ({} frames, {} ms without an ack); resyncing",
                c.id,
                n,
                silence
            );
            c.inflight.lock().clear();
            c.needs_keyframe.store(true, Ordering::Relaxed);
            c.started.store(false, Ordering::Relaxed);
            continue;
        }
        if n >= max_n || age > max_age {
            return false;
        }
    }
    true
}

/// Delay-based bitrate adaptation: back off toward the measured delivery rate when
/// acks show queueing, probe upward slowly when the path is clear.
struct RateCtl {
    target: f64,
    max: f64,
    min: f64,
    last_eval: Instant,
    last_dec: Instant,
    last_inc: Instant,
    /// Ramp multiplicatively until the first sign of congestion (like TCP slow start),
    /// so a session neither starts by flooding a slow link nor crawls up on a fast one.
    slow_start: bool,
    /// Delivery rate observed at the last congestion: the path's likely capacity.
    ceiling: f64,
    ceiling_at: Instant,
}

/// Starting point for a new session's bitrate cap (kbit/s).
const START_KBPS: f64 = 8000.0;

impl RateCtl {
    fn new(max: f64, min: f64) -> Self {
        let past = Instant::now() - Duration::from_secs(10);
        RateCtl {
            target: START_KBPS.min(max),
            max,
            min,
            last_eval: past,
            last_dec: past,
            last_inc: past,
            slow_start: true,
            ceiling: f64::MAX,
            ceiling_at: past,
        }
    }

    fn update(&mut self, clients: &[Arc<Client>]) {
        if self.last_eval.elapsed() < Duration::from_millis(100) {
            return;
        }
        self.last_eval = Instant::now();
        let mut congested = false;
        let mut clear = true;
        let mut rate = f64::MAX;
        let mut delivered = 0f64;
        for c in clients {
            if !c.started.load(Ordering::Relaxed) {
                continue;
            }
            let q = c.delay.lock().queue_ms;
            let (_, age) = c.backlog();
            let rtt = match c.min_rtt_ms.load(Ordering::Relaxed) {
                0 => 40,
                r => r,
            } as f64;
            if q > 30.0 || age as f64 > rtt + 80.0 {
                congested = true;
                let r = c.delivered.lock().rate_bps / 1000.0;
                if r > 0.0 {
                    rate = rate.min(r);
                }
            }
            if q > 15.0 {
                clear = false;
            }
            delivered = delivered.max(c.delivered.lock().rate_bps / 1000.0);
        }
        let now = Instant::now();
        if congested && self.last_dec.elapsed() >= Duration::from_millis(300) {
            let mut t = self.target * 0.85;
            // Delivery rate only says something about capacity when we were actually
            // pushing near the target; congestion from one big frame on an idle screen
            // would otherwise pin the ceiling at a few kbps.
            if rate.is_finite() && rate >= self.target * 0.5 {
                self.ceiling = rate;
                self.ceiling_at = now;
                t = t.min(rate * 0.85);
            }
            self.target = t.max(self.min);
            self.last_dec = now;
            self.slow_start = false;
            log::debug!(
                "rate: congested, target {:.0} kbps (delivered {:.0})",
                self.target,
                rate
            );
        } else if self.slow_start
            && !congested
            && clear
            && self.last_inc.elapsed() >= Duration::from_millis(250)
        {
            // Only probe upward once the content actually presses against the cap.
            if delivered >= self.target * 0.5 {
                self.target = (self.target * 1.5).min(self.max);
                self.last_inc = now;
                log::debug!(
                    "rate: slow start, target {:.0} kbps (delivered {:.0})",
                    self.target,
                    delivered
                );
            }
        } else if !congested
            && clear
            && self.last_dec.elapsed() >= Duration::from_secs(1)
            && delivered >= self.target * 0.5
        {
            // Only raise the cap while the content actually uses it: an idle screen says
            // nothing about the path, and an inflated cap overshoots the next burst.
            // Paths change; forget a capacity estimate that hasn't been confirmed lately.
            if self.ceiling_at.elapsed() >= Duration::from_secs(15) {
                self.ceiling = f64::MAX;
            }
            // Recover quickly to just under the known capacity, then probe past it gently
            // so a busy link isn't driven into the queue every couple of seconds.
            if self.target < self.ceiling * 0.9
                && self.last_inc.elapsed() >= Duration::from_millis(200)
            {
                self.target = (self.target * 1.06 + 100.0)
                    .min(self.ceiling * 0.9)
                    .min(self.max);
                self.last_inc = now;
            } else if self.target >= self.ceiling * 0.9
                && self.last_inc.elapsed() >= Duration::from_millis(1000)
            {
                self.target = (self.target * 1.02).min(self.max);
                self.last_inc = now;
            }
        }
    }
}

/// Regional sharpening. Blocks encoded while they changed carry motion-level quality;
/// once a block has been still for a while it is re-encoded at paint-over quality,
/// even while other parts of the screen keep moving (a playing video would otherwise
/// keep the frame-level paint-over from ever running).
struct Refiner {
    epoch: Instant,
    changed_ms: Vec<u32>,
    pending: Vec<bool>,
    last_refine: Instant,
    cols: usize,
}

/// One cohort's paint-over after motion: a few passes once the picture has been still for a
/// moment, none while it keeps changing. New damage starts them over. An encoder that refines
/// by QP takes them as a ladder: a burst then waits longer after costly ones close together,
/// and for its share of the bitrate, but isn't dropped, so the picture still converges once
/// the stops end. Any other encoder takes plain passes after the fixed delay.
struct Burst {
    len: u32,
    delay: Duration,
    left: u32,
    last_motion: Instant,
    backoff: u32,
    last_burst: Option<Instant>,
    /// Paint-over bytes sent so far, and when the last burst started.
    spent: u64,
    spent_at_start: u64,
    tokens: f64,
    refilled: Instant,
    /// The bitrate cap in kbit/s; 0 is uncapped.
    target: u32,
    /// The encoder refines by QP.
    ladder: bool,
    /// Last-rung passes after the ladder, while the encoder still has something one of them
    /// could sharpen: they fill an idle link, so their bytes aren't counted in `spent`.
    tail: u32,
}

/// Coarse to fine: a thin link gets something early, and new damage saves the costly last one.
const REFINE_RUNGS: [u8; 3] = [20, 16, 12];
/// The most of the bitrate cap a ladder pass may take. Ladder passes count against the share,
/// so a stop leaves little debt and the burst after the next one isn't kept waiting.
const REFINE_LADDER_MS: usize = 200;
/// The most of the bitrate cap a tail pass may take on an idle screen; the viewer waits for it.
const REFINE_FRAME_MS: usize = 1000;
/// Input or damage this recent keeps tail passes to the ladder's size, so what is typed next
/// never waits behind a large one.
const REFINE_IDLE: Duration = Duration::from_secs(2);
/// The share of that a pass is planned at: estimates of one picture's cost were off by up to
/// 1.8x on dense text, so a pass stays within the cap unless one is off by more.
const REFINE_PLAN_PCT: usize = 55;
/// A burst this cheap (a cursor, a typed glyph) doesn't count towards backing off.
const REFINE_CHEAP_BYTES: u64 = 32 * 1024;
const REFINE_MAX_BACKOFF: u32 = 4;
/// Each step of backing off wears off after this long without a burst, so a stream that
/// keeps stopping still gets one about this often.
const REFINE_QUIET: Duration = Duration::from_secs(5);
/// Bursts may take this share of the cap, averaged over the window: enough for a whole
/// ladder after a single stop, even on a thin link.
const REFINE_SHARE: f64 = 0.10;
const REFINE_WINDOW_S: f64 = 10.0;
/// The longest a pass waits for the last one to arrive.
const REFINE_DRAIN_MS: u128 = 1000;
/// The most last-rung passes one still picture gets: an estimate that never settles can't
/// keep a link busy.
const REFINE_TAIL_MAX: u32 = 64;

impl Burst {
    fn new(len: u32, delay: Duration, now: Instant) -> Self {
        Burst {
            len,
            delay,
            left: 0,
            last_motion: now,
            backoff: 0,
            last_burst: None,
            spent: 0,
            spent_at_start: 0,
            tokens: f64::INFINITY,
            refilled: now,
            target: 0,
            ladder: true,
            tail: 0,
        }
    }
    fn moved(&mut self, now: Instant) {
        self.left = self.len;
        self.tail = 0;
        self.last_motion = now;
    }
    /// Brings the byte budget up to date with what has been sent and the current cap.
    fn account(&mut self, now: Instant, target_kbps: u32, spent: u64) {
        let rate = target_kbps as f64 * 1000.0 / 8.0 * REFINE_SHARE;
        let dt = now.saturating_duration_since(self.refilled).as_secs_f64();
        self.tokens = match target_kbps {
            0 => f64::INFINITY,
            _ => (self.tokens + dt * rate - spent.saturating_sub(self.spent) as f64)
                .min(rate * REFINE_WINDOW_S),
        };
        self.refilled = now;
        self.spent = spent;
        self.target = target_kbps;
    }
    fn level(&self, now: Instant) -> u32 {
        let quiet = self.last_burst.map_or(0, |t| {
            (now.saturating_duration_since(t).as_secs_f64() / REFINE_QUIET.as_secs_f64()) as u32
        });
        self.backoff.saturating_sub(quiet)
    }
    /// How long the picture must stay still before a burst.
    fn gap(&self, now: Instant) -> Duration {
        match self.ladder {
            true => self.delay * (1 << self.level(now)),
            false => self.delay,
        }
    }
    /// Only a burst's first pass waits for budget: one under way finishes.
    fn funded(&self) -> bool {
        !self.ladder
            || self.left < self.len
            || self.tail > 0
            || self.target == 0
            || self.tokens > 0.0
    }
    fn due(&self, now: Instant) -> bool {
        self.left > 0
            && now.saturating_duration_since(self.last_motion) >= self.gap(now)
            && self.funded()
    }
    /// How long until `due` can change, in ms.
    fn wait_ms(&self, now: Instant) -> i32 {
        if self.left == 0 || !self.funded() {
            return 100;
        }
        let still = now.saturating_duration_since(self.last_motion).as_millis();
        (self.gap(now).as_millis().saturating_sub(still)) as i32 + 1
    }
    fn stop(&mut self) {
        self.left = 0;
        self.tail = 0;
    }
    /// The next pass's QP; the last ones of a ladder get the rungs, so any burst length ends
    /// at the finest.
    fn rung(&self) -> Option<u8> {
        let last = REFINE_RUNGS.len() - 1;
        self.ladder
            .then(|| REFINE_RUNGS[last.saturating_sub(self.left as usize - 1)])
    }
    /// Takes a pass.
    fn pass(&mut self, now: Instant) {
        if self.ladder && self.left == self.len && self.tail == 0 {
            let costly = self.spent - self.spent_at_start >= REFINE_CHEAP_BYTES;
            self.backoff = (self.level(now) + costly as u32).min(REFINE_MAX_BACKOFF);
            self.last_burst = Some(now);
            self.spent_at_start = self.spent;
        }
        self.left -= 1;
    }
    /// One more pass at the last rung.
    fn more(&mut self) {
        self.left = 1;
        self.tail += 1;
    }
}

/// What a cohort has left to sharpen and how far along it is. Only the capture thread touches
/// it, and only for this cohort: what the others do can't reach it.
struct Refinement {
    burst: Burst,
    refiner: Refiner,
    /// The cohort's keyframes as of the last look.
    keyframes: u64,
    /// A ladder's last pass is with the encoder.
    settling: bool,
}

/// What a job does to the refinement once it is queued.
enum Step {
    Key,
    Plain,
    /// Motion that sharpens these blocks too.
    Piggyback(Vec<u8>),
    Pass,
}

impl Refinement {
    fn new(cfg: &VideoConfig, ladder: bool, w: u32, h: u32) -> Self {
        let (w, h) = (w as usize, h as usize);
        let delay = Duration::from_millis(cfg.paintover_delay_ms);
        Refinement {
            burst: Burst {
                ladder,
                ..Burst::new(cfg.paintover_burst, delay, Instant::now())
            },
            refiner: Refiner::new(w.div_ceil(16) * h.div_ceil(16), w.div_ceil(16)),
            keyframes: 0,
            settling: false,
        }
    }

    /// The last viewer left: the next one starts from nothing.
    fn reset(&mut self) {
        let b = &self.burst;
        self.burst = Burst {
            ladder: b.ladder,
            ..Burst::new(b.len, b.delay, Instant::now())
        };
        self.settling = false;
        self.refiner.clear_all();
    }

    /// A keyframe that landed unseen may be what the viewer started on.
    fn rearm(&mut self) {
        if self.burst.ladder {
            self.burst.moved(Instant::now());
            self.refiner.mark_all();
        }
    }

    /// With no viewers nothing the encoder still finishes counts for anyone.
    fn baseline(&mut self, sh: &Shared) {
        self.keyframes = sh.keyframes.load(Ordering::Acquire);
        self.burst.spent = sh.paintover_bytes.load(Ordering::Acquire);
        self.burst.spent_at_start = self.burst.spent;
    }

    /// A keyframe that went out is squeezed into the rate cap, so it starts a ladder. One only
    /// asked for says nothing: an encoder that drops them is asked again and again.
    /// Input or damage came within REFINE_IDLE of `now`.
    fn active(&self, now: Instant, input_at: Option<Instant>) -> bool {
        let last = input_at.map_or(self.burst.last_motion, |i| i.max(self.burst.last_motion));
        now.saturating_duration_since(last) < REFINE_IDLE
    }

    fn update(&mut self, sh: &Shared, kbps: u32, now: Instant, input_at: Option<Instant>) {
        if self.burst.ladder {
            self.burst
                .account(now, kbps, sh.paintover_bytes.load(Ordering::Acquire));
            let sent = sh.keyframes.load(Ordering::Acquire);
            if sent != self.keyframes {
                self.keyframes = sent;
                self.burst.moved(now);
                self.refiner.mark_all();
            }
        }
        // Nothing pending, nothing to send.
        if self.burst.due(now) && !self.refiner.any_pending() {
            self.burst.stop();
        }
        // The last pass has landed: another if the encoder has room for one, else done.
        // While input or damage is recent, a tail pass gets only the ladder's budget: one that
        // only the tail's would fit waits for the screen to go idle.
        self.settling &= self.burst.left == 0;
        if self.settling && sh.busy.load(Ordering::Acquire) == 0 {
            let more = sh.refinable.load(Ordering::Acquire) && self.burst.tail < REFINE_TAIL_MAX;
            let fits = !self.active(now, input_at) || sh.refinable_small.load(Ordering::Acquire);
            match (more, fits) {
                (true, true) => {
                    self.settling = false;
                    self.burst.more();
                }
                (true, false) => {}
                (false, _) => {
                    self.settling = false;
                    self.burst.stop();
                    self.refiner.clear_all();
                }
            }
        }
    }

    /// What capture `f` means for this cohort, if anything. Changes nothing: `sent` does, once
    /// the job is queued.
    fn plan(&self, f: &Frame, key: bool, skipped: bool, pass: bool) -> Option<(Frame, Step)> {
        let mut out = f.clone();
        if key {
            out.mb_dirty = None;
            return Some((out, Step::Key));
        }
        if skipped {
            return Some((out, Step::Plain));
        }
        if let Some(b) = f.band {
            // Piggyback sharpening of blocks that have gone still on this frame: it encodes
            // what moved plus what is due, and everything else stays skipped.
            let Some((due, band)) = self.refiner.due() else {
                return Some((out, Step::Plain));
            };
            let mut m = f
                .mb_dirty
                .as_ref()
                .map_or_else(|| vec![1; due.len()], |m| m.to_vec());
            for (d, r) in m.iter_mut().zip(&due) {
                *d |= r;
            }
            out.band = Some(union(Some(b), band));
            out.mb_dirty = Some(Arc::new(m));
            out.paintover = true;
            return Some((out, Step::Piggyback(due)));
        }
        if !pass {
            return None;
        }
        let (m, band) = self.refiner.pending_map()?;
        out.band = Some(band);
        out.mb_dirty = Some(Arc::new(m));
        out.paintover = true;
        out.paintover_final = self.burst.left == 1;
        out.refine_qp = self.burst.rung();
        out.refine_tail = self.burst.tail > 0;
        Some((out, Step::Pass))
    }

    fn sent(&mut self, step: Step, stats: &Stats, now: Instant) {
        match step {
            // A keyframe is squeezed into the rate cap; sharpen all of it once still.
            Step::Key => self.refiner.mark_all(),
            Step::Plain => {}
            Step::Piggyback(due) => {
                self.refiner.taken(&due);
                stats.refines.fetch_add(1, Ordering::Relaxed);
            }
            Step::Pass => {
                self.burst.pass(now);
                stats.paintovers.fetch_add(1, Ordering::Relaxed);
                match (self.burst.left, self.burst.ladder) {
                    (0, true) => self.settling = true,
                    (0, false) => self.refiner.clear_all(),
                    _ => {}
                }
            }
        }
    }
}

const REFINE_STILL_MS: u32 = 300;
const REFINE_EVERY: Duration = Duration::from_millis(250);

impl Refiner {
    fn new(blocks: usize, cols: usize) -> Self {
        Refiner {
            epoch: Instant::now(),
            changed_ms: vec![0; blocks],
            pending: vec![false; blocks],
            last_refine: Instant::now(),
            cols,
        }
    }
    fn now(&self) -> u32 {
        self.epoch.elapsed().as_millis() as u32
    }
    fn mark(&mut self, dirty: &[u8]) {
        let now = self.now();
        for (i, &d) in dirty.iter().enumerate() {
            if d != 0 {
                self.pending[i] = true;
                self.changed_ms[i] = now;
            }
        }
    }
    fn mark_all(&mut self) {
        let now = self.now();
        self.pending.fill(true);
        self.changed_ms.fill(now);
    }
    fn clear_all(&mut self) {
        self.pending.fill(false);
    }
    /// Every block not yet sharpened (1 = encode) and the row band they span.
    fn pending_map(&self) -> Option<(Vec<u8>, Band)> {
        let (mut r0, mut r1) = (usize::MAX, 0);
        let map: Vec<u8> = self
            .pending
            .iter()
            .enumerate()
            .map(|(i, &p)| {
                if p {
                    r0 = r0.min(i / self.cols);
                    r1 = r1.max(i / self.cols + 1);
                }
                p as u8
            })
            .collect();
        (r1 > r0).then(|| {
            (
                map,
                Band {
                    y0: (r0 * 16) as u32,
                    y1: (r1 * 16) as u32,
                },
            )
        })
    }
    fn any_pending(&self) -> bool {
        self.pending.iter().any(|&p| p)
    }
    /// Blocks due for sharpening (1 = refine) and the row band they span.
    fn due(&self) -> Option<(Vec<u8>, Band)> {
        if self.last_refine.elapsed() < REFINE_EVERY {
            return None;
        }
        let now = self.now();
        let mut map = vec![0u8; self.pending.len()];
        let (mut r0, mut r1, mut any) = (usize::MAX, 0, false);
        for (i, m) in map.iter_mut().enumerate() {
            if self.pending[i] && now.saturating_sub(self.changed_ms[i]) >= REFINE_STILL_MS {
                *m = 1;
                let row = i / self.cols;
                r0 = r0.min(row);
                r1 = r1.max(row + 1);
                any = true;
            }
        }
        any.then(|| {
            (
                map,
                Band {
                    y0: (r0 * 16) as u32,
                    y1: (r1 * 16) as u32,
                },
            )
        })
    }
    /// The blocks of `due` went out.
    fn taken(&mut self, due: &[u8]) {
        for (p, &d) in self.pending.iter_mut().zip(due) {
            *p &= d == 0;
        }
        self.last_refine = Instant::now();
    }
}

/// Notes the damage the capturer has taken for /wait, whether or not anyone is watching.
fn note_changes(cap: &mut Capturer, hub: &Hub) {
    if !cap.changes.is_empty() {
        hub.activity.note(&cap.changes);
        cap.changes.clear();
    }
}

fn capture_loop(
    cap: &mut Capturer,
    cfg: VideoConfig,
    hub: Arc<Hub>,
    eng: &mut Engine,
) -> Result<(), String> {
    let (w, h) = (cap.width as usize, cap.height as usize);
    let mut pool = Pool::new(w, h, cap.full());
    let mut fps = cfg.fps;
    let mut last_capture = Instant::now() - Duration::from_secs(1);
    let mut had_clients = false;
    let mut full_next = true;
    let debug_marker = std::env::var("VOXL_DEBUG_MARKER").is_ok();

    loop {
        if hub.resize_req.lock().is_some() || cap.needs_rebuild() {
            return Ok(());
        }
        cap.pump()?;
        if cap.needs_rebuild() {
            return Ok(());
        }
        note_changes(cap, &hub);
        hub.drain_wake();
        let snaps = std::mem::take(&mut *hub.snapshots.lock());
        if !snaps.is_empty() {
            // The whole current screen, whether or not anyone is watching. Damage
            // stays pending, so the stream still sends what changed. Only the pixels
            // are copied here (the framebuffer is live); the PNG is made off this thread.
            // The capturer takes the damage for exactly these pixels with them, so the
            // screenshot's seq covers what it shows and nothing drawn after it.
            let bgrx = cap.snapshot()?;
            note_changes(cap, &hub);
            let shot = Arc::new(crate::record::Snapshot {
                width: w,
                height: h,
                bgrx,
                seq: hub.activity.seq(),
            });
            for tx in snaps {
                let _ = tx.send(shot.clone());
            }
        }
        eng.replan();
        let views = eng.views();
        eng.update_rates(&views);
        if views.iter().all(|v| v.clients.is_empty()) {
            had_clients = false;
            wait_any(&[cap.fd(), hub.waker.waitable()], 500);
            continue;
        }
        if !had_clients {
            had_clients = true;
            // Our shm copy may be stale: re-grab everything once.
            cap.grab(cap.full())?;
            pool.invalidate();
            cap.damaged = true;
            full_next = true;
        }
        let want = eng.capture_fps(&views);
        if want != fps {
            log::info!("capture {fps} -> {want} fps");
            fps = want;
        }
        let interval = Duration::from_secs_f64(1.0 / fps as f64);
        // Allow slight early capture so source jitter doesn't alias into dropped frames.
        let min_interval = interval.mul_f64(0.85);
        let interval_ms = interval.as_secs_f64() * 1000.0;
        let now = Instant::now();
        let turn = eng.turn(&views, interval_ms, now);
        if !cap.damaged && !turn.work() {
            wait_any(
                &[cap.fd(), hub.waker.waitable()],
                eng.refine_wait_ms(&views, now),
            );
            continue;
        }
        let since = last_capture.elapsed();
        if since < min_interval {
            let rem = min_interval - since;
            wait_any(
                &[hub.waker.waitable()],
                (rem.as_micros() as i32 + 999) / 1000,
            );
            continue;
        }
        // Apps present a frame as a burst of puts; give the burst a moment to land so
        // the capture holds one whole frame instead of its first tiles.
        if let Some(t) = cap.damaged_at {
            let el = t.elapsed();
            if el < cfg.coalesce {
                let rem = cfg.coalesce - el;
                std::thread::sleep(rem);
                continue;
            }
        }
        if !turn.takes.contains(&true) {
            if !turn.paced {
                hub.stats.flow_blocked.fetch_add(1, Ordering::Relaxed);
            }
            wait_any(&[cap.fd(), hub.waker.waitable()], 4);
            continue;
        }
        // Need a free buffer (the encoders may still hold the others).
        let Some(idx) = pool.free() else {
            wait_any(&[hub.waker.waitable()], 50);
            continue;
        };

        let t0 = Instant::now();
        let origin = cap.damaged_at.unwrap_or(t0);
        hub.stats
            .damage_wait_us
            .fetch_add((t0 - origin).as_micros() as u64, Ordering::Relaxed);
        let mut band = None;
        let mb_dirty = (!full_next).then(|| cap.take_mb_map());
        if full_next {
            cap.take_mb_map();
            full_next = false;
        }
        eng.mark(&views, mb_dirty.as_deref());
        if cap.damaged {
            band = cap.take_damage()?;
            if let Some(b) = band {
                cap.grab(b)?;
            }
        }
        let t1 = Instant::now();
        if debug_marker {
            log::info!(
                "cap dt {:.1} ms band {:?} grab {} us",
                last_capture.elapsed().as_secs_f64() * 1000.0,
                band,
                (t1 - t0).as_micros()
            );
        }
        let marker_ms = match cap.bgrx() {
            Some((px, stride)) if debug_marker => marker_probe(px, stride, "grab"),
            _ => -1,
        };
        let is_motion = band.is_some();
        if !is_motion && !turn.work() {
            // Damage was empty after all.
            continue;
        }
        let buf = pool.fill(idx, band, |b, t| {
            cap.convert(b, t.y0 as usize, t.y1 as usize, cfg.convert_threads)
        });
        let t2 = Instant::now();
        hub.stats
            .grab_us
            .fetch_add((t1 - t0).as_micros() as u64, Ordering::Relaxed);
        hub.stats
            .convert_us
            .fetch_add((t2 - t1).as_micros() as u64, Ordering::Relaxed);
        hub.stats.captured.fetch_add(1, Ordering::Relaxed);

        last_capture = t0;
        eng.dispatch(
            &Frame {
                buf,
                paintover: false,
                paintover_final: false,
                refine_tail: false,
                refine_idle: false,
                refine_qp: None,
                mb_dirty: mb_dirty.map(Arc::new),
                band,
                captured_at: origin,
                marker_ms,
            },
            &views,
            &turn,
            Instant::now(),
        );
    }
}

/// Stripes for `n` (more if a picture would be too large for the encoder).
fn fit_stripes(backend: encoder::Backend, width: u32, height: u32, n: u32) -> Vec<(u32, u32)> {
    let mut n = n.clamp(1, 16);
    loop {
        let layout = stripe_layout(height, n);
        if n >= 16 || layout.iter().all(|&(_, h)| backend.fits(width, h)) {
            return layout;
        }
        n += 1;
    }
}

/// Split the screen into horizontal stripes, each an independent H.264 stream.
/// x264 pays ~9 ms of CPU per 2560x1600 frame even when nothing changed, and sliced
/// threading triples that; independent single-threaded stripes are encoded in
/// parallel (low latency) and only when they changed (low CPU).
fn stripe_layout(height: u32, n: u32) -> Vec<(u32, u32)> {
    let n = n.clamp(1, 16);
    let each = (height.div_ceil(n) + 15) & !15;
    let mut v = Vec::new();
    let mut y = 0;
    while y < height {
        let h = each.min(height - y);
        v.push((y, h));
        y += h;
    }
    v
}

struct Stripe {
    y: usize,
    h: usize,
    enc: Box<dyn VideoEncoder>,
    cfg: EncoderConfig,
    /// Sliced threads to fan out to under sustained heavy motion (1 = never).
    max_threads: u32,
    /// The cap last applied (None: apply the next one).
    applied_kbps: Option<u32>,
    applied_crf: f32,
    applied_fps: u32,
    open: Open,
}

const HDR: usize = negotiate::HDR_V1;

struct StripeJob {
    /// The encode loop keeps its own reference until every stripe has replied.
    buf: Arc<I420>,
    ts: i64,
    force_idr: bool,
    boost: bool,
    crf: f32,
    cap: u32,
    refine: Option<PassBudget>,
    fps: u32,
    mb_dirty: Option<Arc<Vec<u8>>>,
    mb_cols: usize,
}

/// A refining pass's QP, what its frame may cost, and what a tail pass after it may on a busy
/// and on an idle screen (None: uncapped).
#[derive(Clone, Copy)]
struct PassBudget {
    qp: u8,
    max_bytes: Option<usize>,
    busy: Option<usize>,
    idle: Option<usize>,
}

struct StripeResult {
    i: usize,
    /// Keyframe and the stripe's message (header space first); None: nothing to send.
    out: Option<(bool, Vec<u8>)>,
    config: Option<StreamConfig>,
    failed: bool,
    /// A refining pass left something a tail pass could still sharpen, at the tail's budget and
    /// at the ladder's.
    refinable: bool,
    refinable_small: bool,
}

/// x264's sliced threading costs ~3x the CPU of one thread on desktop content (and on
/// small updates is even slower), so a stripe encodes single-threaded and only fans out
/// while sustained heavy motion would otherwise blow the frame budget.
struct ThreadGovernor {
    threads: u32,
    ewma_ms: f64,
    heavy: u32,
    light_since: Option<Instant>,
    budget_ms: f64,
    /// One-thread encode time as a function of changed macroblocks, learned from
    /// single-threaded frames, to tell when one thread would fit again.
    fit: CostFit,
    /// Predicted one-thread time while threaded.
    predicted_ms: f64,
}

/// Exponentially weighted least squares fit of encode ms = a + b * changed MBs.
#[derive(Default)]
struct CostFit {
    w: f64,
    x: f64,
    y: f64,
    xx: f64,
    xy: f64,
}

impl CostFit {
    fn add(&mut self, x: f64, y: f64) {
        let k = 0.995;
        self.w = self.w * k + 1.0;
        self.x = self.x * k + x;
        self.y = self.y * k + y;
        self.xx = self.xx * k + x * x;
        self.xy = self.xy * k + x * y;
    }
    fn predict(&self, x: f64) -> Option<f64> {
        if self.w < 30.0 {
            return None;
        }
        let var = self.xx * self.w - self.x * self.x;
        if var <= f64::EPSILON * self.w * self.w {
            return Some(self.y / self.w);
        }
        let b = (self.xy * self.w - self.x * self.y) / var;
        let a = (self.y - b * self.x) / self.w;
        Some(a + b * x)
    }
}

impl ThreadGovernor {
    fn want(&mut self, enc_ms: f64, changed_mbs: f64, max: u32) -> Option<u32> {
        self.ewma_ms = self.ewma_ms * 0.8 + enc_ms * 0.2;
        if self.threads == 1 {
            self.fit.add(changed_mbs, enc_ms);
            // Before one thread runs out of frame budget: encode times vary from frame to
            // frame, and at ~90% of the budget on average enough frames miss it to drop
            // the frame rate.
            self.heavy = if self.ewma_ms > self.budget_ms * 0.65 {
                self.heavy + 1
            } else {
                0
            };
            if self.heavy >= 20 {
                self.predicted_ms = self.ewma_ms;
                return Some(max);
            }
            return None;
        }
        // Without enough one-thread history, assume one thread is no faster than these.
        let one = self.fit.predict(changed_mbs).unwrap_or(enc_ms);
        self.predicted_ms = self.predicted_ms * 0.8 + one * 0.2;
        if self.predicted_ms < self.budget_ms * 0.5 {
            // One thread would fit with room to spare (switching reopens the encoder,
            // which costs a keyframe, so not on a brief lull).
            let since = *self.light_since.get_or_insert_with(Instant::now);
            (since.elapsed() >= Duration::from_secs(2)).then_some(1)
        } else {
            self.light_since = None;
            None
        }
    }
}

fn stripe_worker(
    i: usize,
    mut s: Stripe,
    rx: mpsc::Receiver<StripeJob>,
    done: mpsc::Sender<StripeResult>,
    debug: bool,
) {
    let mut gov = ThreadGovernor {
        threads: 1,
        ewma_ms: 0.0,
        heavy: 0,
        light_since: None,
        budget_ms: 1000.0 / s.cfg.fps as f64,
        fit: CostFit::default(),
        predicted_ms: 0.0,
    };
    while let Ok(job) = rx.recv() {
        let buf = &*job.buf;
        // A frame at a fixed QP is outside rate control; leave its budget alone.
        let refine = job
            .refine
            .filter(|_| !job.force_idr && s.cfg.backend.refines_by_qp());
        let rate = if refine.is_some() {
            None
        } else if job.force_idr && job.boost {
            s.applied_kbps = None;
            Some(keyframe_rate(job.crf, job.cap, job.fps))
        } else {
            let moved = s
                .applied_kbps
                .is_none_or(|a| (job.cap as f64 - a as f64).abs() > a as f64 * 0.04);
            (job.crf != s.applied_crf || moved || job.fps != s.applied_fps).then(|| {
                s.applied_crf = job.crf;
                s.applied_kbps = Some(job.cap);
                s.applied_fps = job.fps;
                RateConfig::new(job.crf, job.cap, job.fps)
            })
        };
        if let Some(r) = rate {
            if let Err(e) = s.enc.set_rate(r) {
                log::warn!("stripe {i}: {e}");
            }
        }
        let suv = buf.stride_uv();
        let frame = FrameView {
            width: buf.width as u32,
            height: s.h as u32,
            y: &buf.y[s.y * buf.width..],
            u: &buf.u[(s.y / 2) * suv..],
            v: &buf.v[(s.y / 2) * suv..],
            stride_y: buf.width,
            stride_uv: suv,
        };
        // Nonzero on unchanged macroblocks of this stripe (x264's X264_MBINFO_CONSTANT).
        let mut info = job.mb_dirty.as_ref().map(|d| {
            let rows = s.h.div_ceil(16);
            let start = (s.y / 16) * job.mb_cols;
            d[start..start + rows * job.mb_cols]
                .iter()
                .map(|&x| (x == 0) as u8)
                .collect::<Vec<u8>>()
        });
        let te = Instant::now();
        let r = s.enc.encode(
            &frame,
            EncodeOptions {
                pts_ms: job.ts,
                force_keyframe: job.force_idr,
                refine: refine.map(|b| Refine {
                    qp: b.qp,
                    max_bytes: b.max_bytes,
                }),
                static_mbs: info.as_deref_mut(),
            },
        );
        let mut config = None;
        let out = r.map(|aus| {
            let mut out = Vec::with_capacity(HDR + aus.iter().map(|a| a.data.len()).sum::<usize>());
            out.resize(HDR, 0);
            let key = aus.first().is_some_and(|a| a.keyframe);
            for au in aus {
                if let Some(c) = au.config {
                    log::info!(
                        "stripe {i}: {} {}x{}",
                        c.codec_string,
                        c.display.0,
                        c.display.1
                    );
                    config = Some(c);
                }
                out.extend_from_slice(&au.data);
            }
            (key, out)
        });
        if debug {
            let cpu_now = thread_cpu_us();
            log::info!(
                "stripe y{} encode {} us wall, thread cpu total {} us, {} bytes",
                s.y,
                te.elapsed().as_micros(),
                cpu_now,
                out.as_ref().map_or(0, |o| o.1.len())
            );
        }
        if s.max_threads > 1 {
            // Changed macroblocks in this stripe (all of them without a damage map).
            let changed = info
                .as_ref()
                .map_or((s.h.div_ceil(16) * job.mb_cols) as f64, |m| {
                    m.iter().filter(|&&c| c == 0).count() as f64
                });
            if let Some(t) = gov.want(te.elapsed().as_secs_f64() * 1000.0, changed, s.max_threads) {
                match (s.open)(&EncoderConfig {
                    threads: t,
                    ..s.cfg.clone()
                }) {
                    Ok(e) => {
                        log::info!(
                            "stripe {i}: encoder now {t} thread(s) (encode {:.1} ms)",
                            gov.ewma_ms
                        );
                        // A fresh encoder opens with an IDR on its next frame.
                        s.enc = e;
                        s.applied_kbps = None;
                        gov.threads = t;
                        gov.heavy = 0;
                        gov.light_since = None;
                    }
                    Err(e) => log::error!("encoder reopen failed: {e}"),
                }
            }
        }
        let failed = out.is_err();
        let res = match out {
            Ok((key, out)) if out.len() > HDR => Some((key, out)),
            Ok(_) => None,
            Err(e) => {
                log::error!("stripe {i}: {e}");
                // A session that failed may stay broken: the next picture gets a fresh
                // one (which starts with an IDR).
                match (s.open)(&EncoderConfig {
                    threads: if s.max_threads > 1 {
                        gov.threads
                    } else {
                        s.cfg.threads
                    },
                    ..s.cfg.clone()
                }) {
                    Ok(e) => {
                        s.enc = e;
                        s.applied_kbps = None;
                    }
                    Err(e) => log::error!("stripe {i}: encoder reopen failed: {e}"),
                }
                None
            }
        };
        // Done with the picture before saying so: the capture thread reuses it then.
        drop(job);
        let can = |max_bytes| {
            s.enc.refinable(Refine {
                qp: REFINE_RUNGS[REFINE_RUNGS.len() - 1],
                max_bytes,
            })
        };
        let (refinable, refinable_small) =
            refine.map_or((false, false), |b| (can(b.idle), can(b.busy)));
        let r = StripeResult {
            i,
            out: res,
            config,
            refinable,
            refinable_small,
            failed,
        };
        if done.send(r).is_err() {
            return;
        }
    }
}

fn encode_loop(
    stripes_in: Vec<Stripe>,
    rx: mpsc::Receiver<Job>,
    hub: Arc<Hub>,
    sh: Arc<Shared>,
    crf: f32,
    po_crf: f32,
    // --max-kbps 0: the controller's floor still applies after congestion; don't pass it on.
    uncapped: bool,
) {
    let start = Instant::now();
    let mut frame_id: u32 = 0;
    let n = stripes_in.len();
    let debug_stripes = std::env::var("VOXL_DEBUG_STRIPES").is_ok();
    // Geometry and paint-over bookkeeping stay here; encoders move to their workers.
    let mut stripes: Vec<(usize, usize, bool)> =
        stripes_in.iter().map(|s| (s.y, s.h, false)).collect();
    // Consecutive failed encodes per stripe; a stripe not asked to encode hasn't recovered.
    let mut fails = vec![0u32; n];
    let mut slow = SlowWatch::default();
    let (done_tx, done_rx) = mpsc::channel::<StripeResult>();
    let workers: Vec<(mpsc::Sender<StripeJob>, std::thread::JoinHandle<()>)> = stripes_in
        .into_iter()
        .enumerate()
        .map(|(i, s)| {
            let (tx, rx) = mpsc::channel();
            let done = done_tx.clone();
            let h = std::thread::Builder::new()
                .name(format!("enc-{}-{i}", sh.id))
                .spawn(move || stripe_worker(i, s, rx, done, debug_stripes))
                .unwrap();
            (tx, h)
        })
        .collect();
    // Lets the capture thread queue the next job, once this one is sent.
    let finish = |force_idr: bool| {
        if force_idr {
            sh.idr_inflight.store(false, Ordering::Release);
        }
        sh.busy.fetch_sub(1, Ordering::AcqRel);
        hub.wake();
    };
    while let Ok(Job {
        frame: job,
        force_idr,
        boost,
        fps,
    }) = rx.recv()
    {
        let t0 = Instant::now();
        let motion = !force_idr && !job.paintover && job.refine_qp.is_none();
        let selected: Vec<bool> = stripes
            .iter()
            .map(|&(y, h, touched)| {
                force_idr
                    || (job.paintover && touched)
                    || job
                        .band
                        .is_some_and(|b| (b.y0 as usize) < y + h && (b.y1 as usize) > y)
            })
            .collect();
        if !selected.iter().any(|&x| x) {
            if job.refine_qp.is_some() {
                sh.refinable.store(false, Ordering::Release);
                sh.refinable_small.store(false, Ordering::Release);
            }
            drop(job);
            finish(force_idr);
            continue;
        }
        let target = sh.target.load(Ordering::Relaxed);
        let want_crf = if job.paintover { po_crf } else { crf };
        // Each stripe may use up to twice its area share of the budget; the delay-based
        // controller reins the total in if several stripes move at once.
        let share = if uncapped { 0 } else { stripe_share(target, n) };
        let ts = job.captured_at.duration_since(start).as_millis() as i64;
        let budget =
            |ms: usize| (!uncapped).then_some(target as usize * ms / 8 / n * REFINE_PLAN_PCT / 100);
        let refine = job.refine_qp.map(|qp| {
            let ms = match job.refine_tail && job.refine_idle {
                true => REFINE_FRAME_MS,
                false => REFINE_LADDER_MS,
            };
            PassBudget {
                qp,
                max_bytes: budget(ms),
                busy: budget(REFINE_LADDER_MS),
                idle: budget(REFINE_FRAME_MS),
            }
        });
        // Persistent per-stripe workers keep each encoder's state warm in one core's cache.
        let mut pending = 0;
        for (i, &on) in selected.iter().enumerate() {
            if on {
                let cap = if job.paintover {
                    share.saturating_mul(2)
                } else {
                    share
                };
                let _ = workers[i].0.send(StripeJob {
                    buf: job.buf.clone(),
                    ts,
                    force_idr,
                    boost,
                    crf: want_crf,
                    cap,
                    refine,
                    fps,
                    mb_dirty: job.mb_dirty.clone(),
                    mb_cols: job.buf.width.div_ceil(16),
                });
                pending += 1;
            }
        }
        let mut results: Vec<Option<(bool, Vec<u8>)>> = (0..n).map(|_| None).collect();
        let mut configs: Vec<Option<StreamConfig>> = (0..n).map(|_| None).collect();
        let mut failed = false;
        let mut refinable = vec![false; n];
        let mut refinable_small = false;
        for _ in 0..pending {
            if let Ok(r) = done_rx.recv() {
                failed |= r.failed;
                refinable[r.i] = r.refinable;
                refinable_small |= r.refinable_small;
                fails[r.i] = if r.failed { fails[r.i] + 1 } else { 0 };
                results[r.i] = r.out;
                configs[r.i] = r.config;
            }
        }
        if job.refine_qp.is_some() {
            sh.refinable
                .store(refinable.contains(&true), Ordering::Release);
            sh.refinable_small.store(refinable_small, Ordering::Release);
        }
        let worst = fails.iter().copied().max().unwrap_or(0);
        sh.failures.store(worst, Ordering::Release);
        if failed {
            // What failed is redone as a keyframe (counting towards FAIL_AFTER), so a
            // lost update can't leave a still screen stale; past FAIL_AFTER the capture
            // thread admits those keyframes only now and then (Shared::degraded).
            for c in hub.cohort_clients(sh.id) {
                c.needs_keyframe.store(true, Ordering::Relaxed);
            }
            if worst >= FAIL_AFTER {
                hub.replan();
            }
        }
        let Frame {
            buf,
            paintover,
            paintover_final,
            refine_qp,
            refine_tail,
            band,
            captured_at,
            marker_ms,
            ..
        } = job;
        drop(buf);
        for (i, (y, h, touched)) in stripes.iter_mut().enumerate() {
            // A pass by QP keeps each stripe it reached as long as that stripe can still sharpen,
            // whether or not its blocks are pending any more.
            if refine_qp.is_some() {
                if selected[i] {
                    *touched = refinable[i];
                }
                continue;
            }
            if paintover_final {
                *touched = false;
            }
            if band.is_some_and(|b| (b.y0 as usize) < *y + *h && (b.y1 as usize) > *y) {
                *touched = true;
            }
        }
        let parts: Vec<(usize, bool, Vec<u8>)> = results
            .into_iter()
            .enumerate()
            .filter_map(|(i, r)| r.map(|(k, o)| (i, k, o)))
            .collect();
        let all_key = parts.len() == n && parts.iter().all(|p| p.1);
        if force_idr && !all_key {
            // Requests stay set (and are retried) until every stripe really sent an IDR.
            log::warn!("keyframe request produced no IDR on every stripe; retrying");
        }
        if configs.iter().any(Option::is_some) {
            sh.reconfigure(&hub, &configs);
        }
        if parts.is_empty() {
            finish(force_idr);
            continue;
        }
        let enc_us = t0.elapsed().as_micros() as u64;
        if motion {
            sh.encoded(&mut slow, enc_us as f64 / 1000.0, Instant::now());
        }
        let total: usize = parts.iter().map(|p| p.2.len()).sum();
        hub.stats.encode_us.fetch_add(enc_us, Ordering::Relaxed);
        hub.stats.encoded.fetch_add(1, Ordering::Relaxed);
        hub.stats.bytes.fetch_add(total as u64, Ordering::Relaxed);
        if refine_qp.is_some() && !all_key {
            hub.stats.paintover_frames.fetch_add(1, Ordering::Relaxed);
            hub.stats
                .paintover_bytes
                .fetch_add(total as u64, Ordering::Relaxed);
            if !refine_tail {
                sh.paintover_bytes
                    .fetch_add(total as u64, Ordering::Release);
            }
        }
        if all_key {
            hub.stats.keyframes.fetch_add(1, Ordering::Relaxed);
            sh.keyframes.fetch_add(1, Ordering::Release);
        }
        frame_id = frame_id.wrapping_add(1);
        // Server-side pipeline latency (capture -> sent) in 1/10 ms, for client stats.
        let mut pipe = (captured_at.elapsed().as_micros() / 100).min(u32::MAX as u128) as u32;
        if marker_ms >= 0 {
            // Debug: wall-clock send time instead of pipeline latency.
            pipe = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u32;
        }
        let nparts = parts.len() as u8;
        let msgs: Vec<Bytes> = parts
            .into_iter()
            .map(|(i, key, mut out)| {
                // type, flags, frame id, pipe, stripe, stripe count, parts in frame, reserved, y
                out[0] = negotiate::VIDEO_V1;
                out[1] = (key as u8) | if paintover { 2 } else { 0 };
                out[2..6].copy_from_slice(&frame_id.to_be_bytes());
                out[6..10].copy_from_slice(&pipe.to_be_bytes());
                out[10] = i as u8;
                out[11] = n as u8;
                out[12] = nparts;
                out[13] = 0;
                out[14..16].copy_from_slice(&(stripes[i].0 as u16).to_be_bytes());
                Bytes::from(out)
            })
            .collect();
        deliver(&hub, sh.id, frame_id, all_key, total, &msgs);
        finish(force_idr);
        hub.stats
            .send_lat_us
            .fetch_add(captured_at.elapsed().as_micros() as u64, Ordering::Relaxed);
        if marker_ms >= 0 {
            log::info!(
                "marker sent age {} ms (encode {} us)",
                marker_age(marker_ms),
                enc_us
            );
        }
    }
}

/// Sends one encoded frame to cohort `cohort`'s viewers that get frames: protocol 1
/// clients as is, protocol 2 clients in the epoch they confirmed. A viewer that has
/// started over sees nothing until a frame that is a keyframe on every stripe.
fn deliver(hub: &Hub, cohort: u64, frame_id: u32, all_key: bool, bytes: usize, msgs: &[Bytes]) {
    let now = Instant::now();
    let mut v2: Vec<(u32, Vec<Bytes>)> = Vec::new();
    for c in hub.video_clients() {
        let r = c.route.lock();
        if r.cohort != Some(cohort) {
            continue;
        }
        let epoch = match (r.proto, r.phase) {
            (Proto::Legacy, _) => None,
            (Proto::V2, Phase::Ready) => Some(r.epoch),
            _ => continue,
        };
        if !c.started.load(Ordering::Relaxed) {
            if !all_key {
                continue;
            }
            c.started.store(true, Ordering::Relaxed);
            c.inflight.lock().clear();
        }
        if all_key {
            c.needs_keyframe.store(false, Ordering::Relaxed);
        }
        {
            let mut q = c.inflight.lock();
            if q.len() > 600 {
                log::warn!("client {} hopelessly behind; dropping", c.id);
                drop(q);
                c.send(Out::Text("{\"t\":\"close\"}".into()));
                continue;
            }
            q.push_back(Inflight {
                id: frame_id,
                sent: now,
                bytes,
            });
        }
        let Some(e) = epoch else {
            for m in msgs {
                c.send(Out::Bin(m.clone()));
            }
            continue;
        };
        let i = match v2.iter().position(|(x, _)| *x == e) {
            Some(i) => i,
            None => {
                v2.push((
                    e,
                    msgs.iter()
                        .map(|m| Bytes::from(negotiate::video_v2(m, e)))
                        .collect(),
                ));
                v2.len() - 1
            }
        };
        for m in &v2[i].1 {
            c.send(Out::Bin(m.clone()));
        }
    }
}

/// Debug: decode the bench loadgen marker at (300,150) and log its age.
/// CPU time this thread has used, in microseconds.
#[cfg(unix)]
fn thread_cpu_us() -> i64 {
    use nix::time::{clock_gettime, ClockId};
    clock_gettime(ClockId::CLOCK_THREAD_CPUTIME_ID)
        .map(|t| t.tv_sec() * 1_000_000 + t.tv_nsec() / 1000)
        .unwrap_or(0)
}

#[cfg(windows)]
fn thread_cpu_us() -> i64 {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
    let (mut c, mut e, mut k, mut u) = Default::default();
    let ft = |f: FILETIME| ((f.dwHighDateTime as i64) << 32 | f.dwLowDateTime as i64) / 10;
    match unsafe { GetThreadTimes(GetCurrentThread(), &mut c, &mut e, &mut k, &mut u) } {
        Ok(()) => ft(k) + ft(u),
        Err(_) => 0,
    }
}

fn marker_probe(buf: &[u8], stride: usize, what: &str) -> i64 {
    let mut v: u64 = 0;
    let (mx, my) = std::env::var("VOXL_DEBUG_MARKER")
        .ok()
        .and_then(|s| {
            s.split_once(',')
                .and_then(|(a, b)| Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?)))
        })
        .unwrap_or((300, 150));
    for b in 0..40 {
        let x = mx + (b % 20) * 24 + 12;
        let y = my + (b / 20) * 24 + 12;
        let i = y * stride + x * 4;
        if buf[i + 2] > 128 {
            v |= 1 << b;
        }
    }
    let ms = (v & 0xffffff) as i64;
    let now = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64)
        & 0xffffff;
    log::info!("marker {} frame {} age {} ms", what, v >> 24, now - ms);
    ms
}

fn marker_age(ms: i64) -> i64 {
    let now = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64)
        & 0xffffff;
    now - ms
}
