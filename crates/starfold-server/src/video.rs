//! Video pipeline: damage-driven capture -> I420 -> encoder cohorts -> clients, with
//! ack-based flow control per cohort.
//!
//! One capture feeds every cohort (negotiate.rs picks them). A cohort encodes the
//! captures it has room for and catches up on the damage of those it skipped, so a
//! slow viewer holds back only its own cohort.

use crate::capture::{Band, Capturer};
use crate::convert::I420;
use crate::encoder::{
    self, Codec, EncodeError, EncodeOptions, EncoderConfig, FrameView, RateConfig, StreamConfig,
    VideoEncoder,
};
use crate::hub::wait_any;
use crate::hub::{Client, Hub, Inflight, Out, Phase, Proto};
use crate::negotiate::{self, Candidate, Choice, DecodeCap, DecoderConfig, Offer, StripeOffer};
use bytes::Bytes;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
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
    w: u32,
    h: u32,
}

/// Consecutive failed encodes after which a cohort's viewers move to their next codec.
const FAIL_AFTER: u32 = 3;
/// How often a cohort past FAIL_AFTER with nothing to fall back to (the last H.264
/// encoder) gets a keyframe; those wait for flow control and get no keyframe boost.
const RETRY_EVERY: Duration = Duration::from_secs(1);

impl Shared {
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

/// One encoder per stripe for one candidate, on its own thread.
struct Cohort {
    shared: Arc<Shared>,
    tx: Option<mpsc::SyncSender<Job>>,
    thread: Option<std::thread::JoinHandle<()>>,
    rc: RateCtl,
    skipped: Option<Skipped>,
    /// Had viewers at the last frame.
    active: bool,
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
    EncoderConfig {
        backend: cand.backend,
        codec: cand.codec,
        width,
        height,
        fps: cfg.fps,
        threads: if cfg.adaptive_threads {
            1
        } else {
            (cfg.threads / stripes as u32).max(1)
        },
        rate: RateConfig::new(cfg.crf, cfg.max_kbps, cfg.fps),
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
    enc.set_rate(keyframe_rate(cfg.crf, target, cfg.fps))?;
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
                open: self.open.clone(),
            });
        }
        let rc = RateCtl::new(self.cfg.max_kbps as f64, self.cfg.min_kbps as f64);
        let shared = Arc::new(Shared {
            id: self.hub.next_cohort.fetch_add(1, Ordering::Relaxed),
            cand,
            offer: Mutex::new(offer),
            busy: AtomicUsize::new(0),
            idr_inflight: AtomicBool::new(false),
            target: AtomicU32::new(rc.target as u32),
            failures: AtomicU32::new(0),
            last_key: Mutex::new(Instant::now() - RETRY_EVERY),
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
        Ok(Cohort {
            shared,
            tx: Some(tx),
            thread: Some(thread),
            rc,
            skipped: None,
            active: false,
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
            if v.clients.is_empty() {
                if c.active {
                    c.rc = RateCtl::new(self.cfg.max_kbps as f64, self.cfg.min_kbps as f64);
                }
                c.active = false;
                c.skipped = None;
            } else {
                c.active = true;
                c.rc.update(&v.clients);
            }
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

    /// Which cohorts take the next capture: those with viewers, room in their queue, and
    /// either a keyframe owed or their viewers keeping up.
    fn takes(&self, views: &[View], interval_ms: f64) -> Vec<bool> {
        self.cohorts
            .iter()
            .zip(views)
            .map(|(c, v)| {
                !v.clients.is_empty()
                    && c.shared.busy.load(Ordering::Acquire) < 2
                    && ((v.want_key && !c.shared.degraded()) || flow_ok(&v.clients, interval_ms))
            })
            .collect()
    }

    /// Whether a cohort that skipped damage has room for it now: worth a capture even
    /// when nothing else changed, or that cohort's viewers keep stale regions.
    fn catch_up_due(&self, views: &[View], interval_ms: f64) -> bool {
        self.cohorts
            .iter()
            .zip(self.takes(views, interval_ms))
            .any(|(c, take)| take && c.skipped.is_some())
    }

    fn dispatch(&mut self, f: &Frame, views: &[View], takes: &[bool]) {
        for ((c, v), &take) in self.cohorts.iter_mut().zip(views).zip(takes) {
            if v.clients.is_empty() {
                continue;
            }
            if !take {
                c.skipped = Some(skip(c.skipped.take(), f));
                continue;
            }
            let frame = match c.skipped.take() {
                Some(s) => catch_up(s, f.clone()),
                None => f.clone(),
            };
            let Some(tx) = c.tx.as_ref() else { continue };
            let force = v.want_key;
            if force {
                c.shared.idr_inflight.store(true, Ordering::Release);
                *c.shared.last_key.lock() = Instant::now();
            }
            c.shared.busy.fetch_add(1, Ordering::AcqRel);
            if let Err(e) = tx.try_send(Job {
                frame,
                force_idr: force,
                boost: force && !c.shared.degraded(),
            }) {
                c.shared.busy.fetch_sub(1, Ordering::AcqRel);
                if force {
                    c.shared.idr_inflight.store(false, Ordering::Release);
                }
                let (mpsc::TrySendError::Full(j) | mpsc::TrySendError::Disconnected(j)) = e;
                c.skipped = Some(skip(None, &j.frame));
            }
        }
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
        if age > 4000 {
            // Stalled client (hidden tab, dead socket): stop letting it gate everyone.
            log::warn!(
                "client {} stalled ({} frames, {} ms); resyncing",
                c.id,
                n,
                age
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
    /// Blocks due for sharpening (1 = refine) and the row band they span.
    fn due(&mut self) -> Option<(Vec<u8>, Band)> {
        if self.last_refine.elapsed() < REFINE_EVERY {
            return None;
        }
        let now = self.now();
        let mut map = vec![0u8; self.pending.len()];
        let (mut r0, mut r1, mut any) = (usize::MAX, 0, false);
        for (i, m) in map.iter_mut().enumerate() {
            if self.pending[i] && now.saturating_sub(self.changed_ms[i]) >= REFINE_STILL_MS {
                *m = 1;
                self.pending[i] = false;
                let row = i / self.cols;
                r0 = r0.min(row);
                r1 = r1.max(row + 1);
                any = true;
            }
        }
        if !any {
            return None;
        }
        self.last_refine = Instant::now();
        Some((
            map,
            Band {
                y0: (r0 * 16) as u32,
                y1: (r1 * 16) as u32,
            },
        ))
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
    let interval = Duration::from_secs_f64(1.0 / cfg.fps as f64);
    // Allow slight early capture so source jitter doesn't alias into dropped frames.
    let min_interval = interval.mul_f64(0.85);
    let interval_ms = interval.as_secs_f64() * 1000.0;
    let mut last_capture = Instant::now() - interval;
    let mut last_motion = Instant::now();
    let mut paintover_left = 0u32;
    let mut had_clients = false;
    let mut full_next = true;
    let mut refiner = Refiner::new(w.div_ceil(16) * h.div_ceil(16), w.div_ceil(16));
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
        let want_key = views.iter().any(|v| v.want_key);
        if !had_clients {
            had_clients = true;
            // Our shm copy may be stale: re-grab everything once.
            cap.grab(cap.full())?;
            pool.invalidate();
            cap.damaged = true;
            full_next = true;
        }
        let mut paintover_due = paintover_left > 0
            && last_motion.elapsed() >= Duration::from_millis(cfg.paintover_delay_ms);
        let catch_up = eng.catch_up_due(&views, interval_ms);
        if !cap.damaged && !want_key && !paintover_due && !catch_up {
            let timeout = if paintover_left == 0 {
                100
            } else {
                cfg.paintover_delay_ms
                    .saturating_sub(last_motion.elapsed().as_millis() as u64) as i32
                    + 1
            };
            wait_any(&[cap.fd(), hub.waker.waitable()], timeout);
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
        let takes = eng.takes(&views, interval_ms);
        if !takes.iter().any(|&t| t) {
            hub.stats.flow_blocked.fetch_add(1, Ordering::Relaxed);
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
        let mut mb_dirty = (!full_next).then(|| cap.take_mb_map());
        if full_next {
            cap.take_mb_map();
            full_next = false;
        }
        match &mb_dirty {
            Some(m) => refiner.mark(m),
            None => refiner.mark_all(),
        }
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
        if !is_motion && !want_key && !paintover_due && !catch_up {
            // Damage was empty after all.
            continue;
        }
        // Sharpen only what was sent at motion quality; nothing pending, nothing to send.
        let paint_map = if paintover_due && !is_motion && !want_key {
            match refiner.pending_map() {
                Some(m) => Some(m),
                None => {
                    paintover_left = 0;
                    if !catch_up {
                        continue;
                    }
                    paintover_due = false;
                    None
                }
            }
        } else {
            None
        };
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

        let mut refine = None;
        let paintover = if is_motion {
            last_motion = Instant::now();
            paintover_left = cfg.paintover_burst;
            // Piggyback sharpening of blocks that have gone still on this frame.
            refine = if want_key { None } else { refiner.due() };
            refine.is_some()
        } else if paintover_due {
            paintover_left -= 1;
            hub.stats.paintovers.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            false
        };
        if want_key {
            // A keyframe is squeezed into the rate cap; sharpen all of it once still.
            refiner.mark_all();
        }
        let frame_paintover = paintover && refine.is_none();
        if frame_paintover && paintover_left == 0 {
            refiner.clear_all();
        }
        // A refinement frame encodes what moved plus what is due; everything else stays skipped.
        let mb_map = if want_key {
            None
        } else if frame_paintover {
            paint_map.map(|(m, b)| {
                band = Some(match band {
                    Some(x) => union(Some(x), b),
                    None => b,
                });
                Arc::new(m)
            })
        } else if let Some((rmap, rband)) = refine.take() {
            band = Some(match band {
                Some(b) => union(Some(b), rband),
                None => rband,
            });
            let mut m = mb_dirty.take().unwrap_or_else(|| vec![1; rmap.len()]);
            for (d, r) in m.iter_mut().zip(rmap) {
                *d |= r;
            }
            hub.stats.refines.fetch_add(1, Ordering::Relaxed);
            Some(Arc::new(m))
        } else {
            mb_dirty.take().map(Arc::new)
        };
        last_capture = t0;
        eng.dispatch(
            &Frame {
                buf,
                paintover,
                paintover_final: frame_paintover && paintover_left == 0,
                mb_dirty: mb_map,
                band,
                captured_at: origin,
                marker_ms,
            },
            &views,
            &takes,
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
    mb_dirty: Option<Arc<Vec<u8>>>,
    mb_cols: usize,
}

struct StripeResult {
    i: usize,
    /// Keyframe and the stripe's message (header space first); None: nothing to send.
    out: Option<(bool, Vec<u8>)>,
    config: Option<StreamConfig>,
    failed: bool,
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
        let rate = if job.force_idr && job.boost {
            s.applied_kbps = None;
            Some(keyframe_rate(job.crf, job.cap, s.cfg.fps))
        } else {
            let moved = s
                .applied_kbps
                .is_none_or(|a| (job.cap as f64 - a as f64).abs() > a as f64 * 0.04);
            (job.crf != s.applied_crf || moved).then(|| {
                s.applied_crf = job.crf;
                s.applied_kbps = Some(job.cap);
                RateConfig::new(job.crf, job.cap, s.cfg.fps)
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
        let r = StripeResult {
            i,
            out: res,
            config,
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
    }) = rx.recv()
    {
        let t0 = Instant::now();
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
                    mb_dirty: job.mb_dirty.clone(),
                    mb_cols: job.buf.width.div_ceil(16),
                });
                pending += 1;
            }
        }
        let mut results: Vec<Option<(bool, Vec<u8>)>> = (0..n).map(|_| None).collect();
        let mut configs: Vec<Option<StreamConfig>> = (0..n).map(|_| None).collect();
        let mut failed = false;
        for _ in 0..pending {
            if let Ok(r) = done_rx.recv() {
                failed |= r.failed;
                fails[r.i] = if r.failed { fails[r.i] + 1 } else { 0 };
                results[r.i] = r.out;
                configs[r.i] = r.config;
            }
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
            band,
            captured_at,
            marker_ms,
            ..
        } = job;
        drop(buf);
        for (y, h, touched) in stripes.iter_mut() {
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
        let total: usize = parts.iter().map(|p| p.2.len()).sum();
        hub.stats.encode_us.fetch_add(enc_us, Ordering::Relaxed);
        hub.stats.encoded.fetch_add(1, Ordering::Relaxed);
        hub.stats.bytes.fetch_add(total as u64, Ordering::Relaxed);
        if all_key {
            hub.stats.keyframes.fetch_add(1, Ordering::Relaxed);
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
