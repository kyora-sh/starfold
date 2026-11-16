//! Quick Sync from the server's side: every call into Intel's runtime happens in a helper
//! process, one per encoder, and the server only talks to it ([`super::proto`]) and kills it.
//! A call already inside the driver can't be stopped in the process that made it; a process
//! can be ended.
//!
//! - Every request has a deadline: opening 10 s, a frame 2 s. A watchdog thread holds the
//!   server to them whether or not anything is waiting on the helper.
//! - A missed deadline, or a helper reporting that the device hung, failed or was lost,
//!   gives Quick Sync up for the process: every helper is killed and reaped, the reason is
//!   logged and returned (it shows in `/stats`), streams fall back to software, and no
//!   helper starts again.
//! - A helper that dies on its own, or says something that isn't the protocol, is ended and
//!   counted: its stream fails over, one more may start, and a second death gives up.
//! - Two frames may be in flight. A third waits as the one pending picture, replaced by any
//!   newer one, and goes out when a slot frees; the frames in between are skipped, with no
//!   extra IDR (the encoder's reference is the last picture it coded).

use super::proc::{write_by, Pipes, Proc, Section, Unwritten};
use super::proto::{Decoder, Open, Rate, Reply, Request};
use crate::encoder::hw;
use crate::encoder::{
    AccessUnit, Codec, ColorInfo, EncodeError, EncodeOptions, EncoderConfig, FrameView, RateConfig,
    VideoEncoder,
};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

/// How long a helper may take.
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    pub open: Duration,
    pub frame: Duration,
    /// How long a killed helper may take to exit.
    pub grace: Duration,
}

pub const TIMING: Timing = Timing {
    open: Duration::from_secs(10),
    frame: Duration::from_secs(2),
    grace: Duration::from_secs(2),
};

/// Frames in flight at once.
pub const SLOTS: usize = 2;
/// How often the watchdog looks.
const WATCH: Duration = Duration::from_millis(20);
const POLL: Duration = Duration::from_micros(200);
/// How long a closed helper may take to leave on its own before it is killed.
const CLOSE: Duration = Duration::from_millis(200);

struct Live {
    id: u64,
    proc: Arc<Proc>,
    /// When its oldest request is due, and what that request is.
    due: Option<(Instant, &'static str)>,
}

#[derive(Default)]
struct State {
    off: Option<String>,
    deaths: u32,
    spawned: u32,
    next: u64,
    live: Vec<Live>,
}

/// Quick Sync's state in this process: whether it is given up, its helpers, their
/// deadlines.
pub struct Policy {
    state: Mutex<State>,
    timing: Timing,
    watchdog: Once,
}

impl Policy {
    pub const fn new(timing: Timing) -> Policy {
        Policy {
            state: Mutex::new(State {
                off: None,
                deaths: 0,
                spawned: 0,
                next: 0,
                live: Vec::new(),
            }),
            timing,
            watchdog: Once::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Why Quick Sync is off in this process, if it is.
    pub fn off(&self) -> Option<String> {
        self.lock().off.clone()
    }

    /// Helpers started so far.
    #[cfg(all(test, unix))]
    pub fn spawned(&self) -> u32 {
        self.lock().spawned
    }

    /// Turns Quick Sync off for good: kills and reaps every helper. The first reason stays.
    pub fn give_up(&self, why: &str) {
        let live = {
            let mut s = self.lock();
            if s.off.is_none() {
                log::error!("qsv given up: {why}; Quick Sync is off until starfold restarts");
                s.off = Some(format!("qsv given up: {why}"));
            }
            std::mem::take(&mut s.live)
        };
        for l in live {
            l.proc.end(self.timing.grace);
        }
    }

    /// A helper died on its own or broke the protocol; the second time, Quick Sync is off.
    fn died(&self, why: &str) {
        let deaths = {
            let mut s = self.lock();
            s.deaths += 1;
            s.deaths
        };
        if deaths >= 2 {
            self.give_up(&format!("a second helper failed ({why})"));
        } else {
            log::warn!("a Quick Sync helper failed: {why}; one more may start");
        }
    }

    fn register(&'static self, proc: Arc<Proc>) -> Result<u64, String> {
        self.watchdog.call_once(|| {
            let _ = std::thread::Builder::new()
                .name("qsv-watchdog".into())
                .spawn(move || self.watch());
        });
        let mut s = self.lock();
        if let Some(off) = &s.off {
            let off = off.clone();
            drop(s);
            proc.end(self.timing.grace);
            return Err(off);
        }
        s.next += 1;
        let id = s.next;
        s.live.push(Live {
            id,
            proc,
            due: None,
        });
        Ok(id)
    }

    fn due(&self, id: u64, due: Option<(Instant, &'static str)>) {
        if let Some(l) = self.lock().live.iter_mut().find(|l| l.id == id) {
            l.due = due;
        }
    }

    /// Takes helper `id` off the list; the caller ends it.
    fn forget(&self, id: u64) {
        self.lock().live.retain(|l| l.id != id);
    }

    fn watch(&'static self) {
        loop {
            std::thread::sleep(WATCH);
            let late = {
                let s = self.lock();
                if s.off.is_some() {
                    return;
                }
                let now = Instant::now();
                s.live
                    .iter()
                    .find_map(|l| l.due.filter(|d| d.0 <= now).map(|d| (l.proc.id(), d.1)))
            };
            if let Some((pid, what)) = late {
                self.give_up(&format!("helper {pid} missed its deadline ({what})"));
            }
        }
    }
}

/// This process's Quick Sync.
pub static POLICY: Policy = Policy::new(TIMING);

#[derive(Clone, Copy)]
struct Sent {
    pts: i64,
    key: bool,
    at: Instant,
}

/// The picture waiting for a free slot: the newest one only.
struct Pending {
    planes: Vec<u8>,
    pts: i64,
    key: bool,
}

enum Expect {
    Frame(u8),
    Rate,
}

/// An I420 picture laid out as the helper reads it from a slot: Y, then U, then V, each at
/// its own width.
pub fn slot_size(w: u32, h: u32) -> usize {
    let (w, h) = (w as usize, h as usize);
    w * h + 2 * w.div_ceil(2) * h.div_ceil(2)
}

fn copy_planes(f: &FrameView<'_>, dst: &mut [u8]) {
    let (w, h) = (f.width as usize, f.height as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let (y, uv) = dst.split_at_mut(w * h);
    let (u, v) = uv.split_at_mut(cw * ch);
    for r in 0..h {
        y[r * w..][..w].copy_from_slice(&f.y[r * f.stride_y..][..w]);
    }
    for r in 0..ch {
        u[r * cw..][..cw].copy_from_slice(&f.u[r * f.stride_uv..][..cw]);
        v[r * cw..][..cw].copy_from_slice(&f.v[r * f.stride_uv..][..cw]);
    }
}

/// A Quick Sync encoder in a helper process.
pub struct Remote {
    policy: &'static Policy,
    id: u64,
    proc: Arc<Proc>,
    pipes: Pipes,
    dec: Decoder,
    section: Section,
    slot: usize,
    size: (u32, u32),
    fps: u32,
    rate: Rate,
    slots: [Option<Sent>; SLOTS],
    pending: Option<Pending>,
    order: VecDeque<Expect>,
    ready: Vec<AccessUnit>,
    /// A request failed short of stopping the helper; its error goes to the next caller.
    failed: Option<EncodeError>,
    stream: hw::Stream,
    /// Why this helper is no longer used.
    gone: Option<String>,
    /// The implementation the helper opened, and whether it runs LowPower.
    pub name: String,
    pub low_power: bool,
    /// Every frame request sent: slot, pts, IDR asked.
    #[cfg(test)]
    pub sent: Vec<(u8, i64, bool)>,
}

fn rate_of(r: &RateConfig) -> Rate {
    Rate {
        max_kbps: r.max_kbps,
        buf_kbit: r.buf_kbit,
        fps: r.fps,
        quality: r.quality,
    }
}

impl Remote {
    /// Starts a helper with `spawn` and opens a session for `cfg` on adapter `luid`, unless
    /// Quick Sync is off in `policy`: then nothing starts.
    pub fn open(
        policy: &'static Policy,
        spawn: impl FnOnce() -> Result<(Proc, Pipes), String>,
        cfg: &EncoderConfig,
        luid: u64,
    ) -> Result<Remote, EncodeError> {
        if let Some(off) = policy.off() {
            return Err(EncodeError(off));
        }
        if !matches!(cfg.codec, Codec::H264 | Codec::Hevc) {
            return Err(EncodeError(format!(
                "Quick Sync here encodes H.264 and HEVC, not {:?}",
                cfg.codec
            )));
        }
        let slot = slot_size(cfg.width, cfg.height);
        let section = Section::new(slot * SLOTS).map_err(EncodeError)?;
        policy.lock().spawned += 1;
        let (proc, pipes) = spawn().map_err(EncodeError)?;
        let proc = Arc::new(proc);
        let id = policy.register(proc.clone()).map_err(EncodeError)?;
        let rate = rate_of(&cfg.rate);
        let mut r = Remote {
            policy,
            id,
            proc,
            pipes,
            dec: Decoder::default(),
            section,
            slot,
            size: (cfg.width, cfg.height),
            fps: cfg.fps.max(1),
            rate,
            slots: [None; SLOTS],
            pending: None,
            order: VecDeque::new(),
            ready: Vec::new(),
            failed: None,
            stream: hw::Stream::new(
                "Quick Sync",
                cfg.codec,
                (cfg.width, cfg.height),
                ColorInfo::SRGB_BT601_LIMITED,
                true,
            ),
            gone: None,
            name: String::new(),
            low_power: false,
            #[cfg(test)]
            sent: Vec::new(),
        };
        let shared = r.section.share(&r.proc).map_err(EncodeError)?;
        let open = Request::Open(Open {
            codec: cfg.codec,
            width: cfg.width,
            height: cfg.height,
            fps: cfg.fps,
            rate,
            slot_size: slot as u32,
            slots: SLOTS as u8,
            section: shared,
            luid,
        });
        let until = Instant::now() + policy.timing.open;
        policy.due(id, Some((until, "opening")));
        r.send(&open, until)?;
        loop {
            match r.next(until)? {
                Some(Reply::Opened { low_power, name }) => {
                    r.name = name;
                    r.low_power = low_power;
                    policy.due(id, None);
                    return Ok(r);
                }
                Some(Reply::Error { stopped, reason }) => {
                    return Err(r.refused(stopped, reason));
                }
                Some(_) => return Err(r.broke("a reply before it opened")),
                None if Instant::now() >= until => {
                    policy.give_up(&format!(
                        "helper {} did not open in {} s",
                        r.proc.id(),
                        policy.timing.open.as_secs()
                    ));
                    return Err(r.stopped());
                }
                None => {}
            }
        }
    }

    /// The helper's process id.
    #[cfg(all(test, unix))]
    pub fn pid(&self) -> u32 {
        self.proc.id()
    }

    /// The error once this helper or Quick Sync is done.
    fn stopped(&mut self) -> EncodeError {
        let why = self
            .policy
            .off()
            .or_else(|| self.gone.clone())
            .unwrap_or_else(|| "the Quick Sync helper is gone".into());
        self.gone.get_or_insert(why.clone());
        EncodeError(why)
    }

    fn usable(&mut self) -> Result<(), EncodeError> {
        if self.gone.is_some() || self.policy.off().is_some() {
            return Err(self.stopped());
        }
        Ok(())
    }

    /// The helper reported an error: the device stopping gives Quick Sync up.
    fn refused(&mut self, stopped: bool, reason: String) -> EncodeError {
        if stopped {
            self.policy.give_up(&reason);
            return self.stopped();
        }
        EncodeError(reason)
    }

    /// The helper died or broke the protocol: it is ended and counted.
    fn broke(&mut self, what: &str) -> EncodeError {
        let why = format!("helper {} {what}", self.proc.id());
        self.policy.forget(self.id);
        self.proc.end(self.policy.timing.grace);
        self.policy.died(&why);
        self.gone = Some(why);
        self.stopped()
    }

    fn send(&mut self, r: &Request, until: Instant) -> Result<(), EncodeError> {
        match write_by(&mut *self.pipes.tx, &r.bytes(), until) {
            Ok(()) => Ok(()),
            Err(Unwritten::Late) => {
                let pid = self.proc.id();
                self.policy
                    .give_up(&format!("helper {pid} missed its deadline (a request)"));
                Err(self.stopped())
            }
            Err(Unwritten::Closed(how)) => Err(self.broke(&how)),
        }
    }

    /// The next reply, waiting until `until` at most; `None` if none came.
    fn next(&mut self, until: Instant) -> Result<Option<Reply>, EncodeError> {
        let mut chunk = [0u8; 1 << 16];
        loop {
            if self.policy.off().is_some() {
                return Err(self.stopped());
            }
            match self.dec.next() {
                Ok(Some(body)) => {
                    return match Reply::parse(&body) {
                        Ok(r) => Ok(Some(r)),
                        Err(e) => Err(self.broke(&format!("sent a bad reply ({e})"))),
                    }
                }
                Ok(None) => {}
                Err(e) => return Err(self.broke(&format!("sent a bad reply ({e})"))),
            }
            match self.pipes.rx.read_ready(&mut chunk) {
                Ok(Some(0)) => {
                    let how = if self.proc.exited() {
                        "exited"
                    } else {
                        "closed its stdout"
                    };
                    return Err(self.broke(how));
                }
                Ok(Some(n)) => self.dec.push(&chunk[..n]),
                Ok(None) if Instant::now() >= until => return Ok(None),
                Ok(None) => std::thread::sleep(POLL),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(self.broke(&format!("could not be read ({e})"))),
            }
        }
    }

    /// When the oldest frame in flight is due.
    fn due(&self) -> Option<Instant> {
        self.slots
            .iter()
            .flatten()
            .map(|s| s.at + self.policy.timing.frame)
            .min()
    }

    fn track(&self) {
        let due = self.due().map(|d| (d, "a frame"));
        let due = due.or_else(|| {
            self.order
                .iter()
                .any(|e| matches!(e, Expect::Rate))
                .then(|| (Instant::now() + self.policy.timing.frame, "a rate change"))
        });
        self.policy.due(self.id, due);
    }

    /// Writes `planes` (or `f`) into `slot` and asks the helper to code it.
    fn submit(
        &mut self,
        slot: usize,
        f: Option<&FrameView<'_>>,
        planes: Option<&[u8]>,
        pts: i64,
        key: bool,
    ) -> Result<(), EncodeError> {
        let dst = &mut self.section.slice()[slot * self.slot..][..self.slot];
        match (f, planes) {
            (Some(f), _) => copy_planes(f, dst),
            (None, Some(p)) => dst.copy_from_slice(p),
            (None, None) => return Err(EncodeError("no picture to send".into())),
        }
        let at = Instant::now();
        self.slots[slot] = Some(Sent { pts, key, at });
        self.order.push_back(Expect::Frame(slot as u8));
        #[cfg(test)]
        self.sent.push((slot as u8, pts, key));
        self.track();
        let req = Request::Frame {
            slot: slot as u8,
            pts,
            key,
        };
        self.send(&req, at + self.policy.timing.frame)
    }

    /// Handles replies until `until`, or until `done` holds.
    fn pump(&mut self, until: Instant, done: impl Fn(&Remote) -> bool) -> Result<(), EncodeError> {
        loop {
            if done(self) {
                return Ok(());
            }
            if let Some(due) = self.due() {
                if Instant::now() >= due {
                    let pid = self.proc.id();
                    self.policy
                        .give_up(&format!("helper {pid} missed its deadline (a frame)"));
                    return Err(self.stopped());
                }
            }
            let wait = self.due().map_or(until, |d| d.min(until));
            match self.next(wait)? {
                Some(r) => self.apply(r)?,
                None if Instant::now() >= until => return Ok(()),
                None => {}
            }
        }
    }

    fn apply(&mut self, r: Reply) -> Result<(), EncodeError> {
        let Some(expect) = self.order.pop_front() else {
            return Err(self.broke("replied to nothing"));
        };
        match (expect, r) {
            (Expect::Frame(slot), Reply::Done { slot: s, units }) if s == slot => {
                let Some(sent) = self.slots[slot as usize].take() else {
                    return Err(self.broke("finished a frame twice"));
                };
                for u in units {
                    match self.stream.unit(u.data, sent.pts, sent.key) {
                        Ok(Some(au)) => self.ready.push(au),
                        Ok(None) => {}
                        Err(e) => {
                            self.failed.get_or_insert(e);
                        }
                    }
                }
                if let Some(p) = self.pending.take() {
                    self.submit(slot as usize, None, Some(&p.planes), p.pts, p.key)?;
                }
                self.track();
                Ok(())
            }
            (Expect::Frame(slot), Reply::Error { stopped, reason }) => {
                self.slots[slot as usize] = None;
                let e = self.refused(stopped, reason);
                self.failed.get_or_insert(e);
                self.usable()?;
                if let Some(p) = self.pending.take() {
                    self.submit(slot as usize, None, Some(&p.planes), p.pts, true)?;
                }
                self.track();
                Ok(())
            }
            (Expect::Rate, Reply::RateSet) => {
                self.track();
                Ok(())
            }
            (Expect::Rate, Reply::Error { stopped, reason }) => {
                let e = self.refused(stopped, reason);
                self.failed.get_or_insert(e);
                self.track();
                self.usable()
            }
            _ => Err(self.broke("answered out of turn")),
        }
    }

    fn take(&mut self) -> Result<Vec<AccessUnit>, EncodeError> {
        if let Some(e) = self.failed.take() {
            self.ready.clear();
            return Err(e);
        }
        Ok(std::mem::take(&mut self.ready))
    }
}

impl VideoEncoder for Remote {
    fn set_rate(&mut self, rate: RateConfig) -> Result<(), EncodeError> {
        self.usable()?;
        let r = rate_of(&rate);
        if r == self.rate {
            return Ok(());
        }
        let until = Instant::now() + self.policy.timing.frame;
        self.order.push_back(Expect::Rate);
        self.track();
        self.send(&Request::Rate(r), until)?;
        self.pump(until, |s| {
            !s.order.iter().any(|e| matches!(e, Expect::Rate))
        })?;
        if self.order.iter().any(|e| matches!(e, Expect::Rate)) {
            let pid = self.proc.id();
            self.policy
                .give_up(&format!("helper {pid} missed its deadline (a rate change)"));
            return Err(self.stopped());
        }
        self.rate = r;
        match self.failed.take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn encode(
        &mut self,
        f: &FrameView<'_>,
        opts: EncodeOptions<'_>,
    ) -> Result<Vec<AccessUnit>, EncodeError> {
        self.usable()?;
        if (f.width, f.height) != self.size {
            return Err(EncodeError(format!(
                "encoder opened at {}x{}, got {}x{}",
                self.size.0, self.size.1, f.width, f.height
            )));
        }
        f.check()?;
        self.pump(Instant::now(), |_| false)?;
        let Some(slot) = self.slots.iter().position(Option::is_none) else {
            let key = opts.force_keyframe || self.pending.as_ref().is_some_and(|p| p.key);
            let mut planes = self.pending.take().map(|p| p.planes).unwrap_or_default();
            planes.resize(self.slot, 0);
            copy_planes(f, &mut planes);
            self.pending = Some(Pending {
                planes,
                pts: opts.pts_ms,
                key,
            });
            return self.take();
        };
        self.submit(slot, Some(f), None, opts.pts_ms, opts.force_keyframe)?;
        let patience =
            Duration::from_millis(1000 / u64::from(self.fps)).max(Duration::from_millis(5));
        let until = Instant::now() + patience.min(self.policy.timing.frame);
        self.pump(until, |s| s.slots[slot].is_none())?;
        self.take()
    }

    fn drain(&mut self) -> Result<Vec<AccessUnit>, EncodeError> {
        self.usable()?;
        while self.slots.iter().any(Option::is_some) || self.pending.is_some() {
            let until = self.due().unwrap_or_else(Instant::now) + POLL;
            self.pump(until, |s| {
                s.slots.iter().all(Option::is_none) && s.pending.is_none()
            })?;
            if let Some(due) = self.due() {
                if Instant::now() >= due {
                    let pid = self.proc.id();
                    self.policy
                        .give_up(&format!("helper {pid} missed its deadline (a frame)"));
                    return Err(self.stopped());
                }
            }
        }
        self.take()
    }
}

impl Drop for Remote {
    /// Closes the helper's stdin, gives it a moment to leave, then ends it either way.
    fn drop(&mut self) {
        self.policy.forget(self.id);
        let pipes = std::mem::replace(&mut self.pipes, closed());
        drop(pipes);
        let end = Instant::now() + CLOSE;
        while Instant::now() < end && !self.proc.exited() {
            std::thread::sleep(Duration::from_millis(5));
        }
        self.proc.end(self.policy.timing.grace);
    }
}

/// Pipes that are already closed, standing in for a helper's after its own are dropped.
fn closed() -> Pipes {
    struct Closed;
    impl super::proc::Ready for Closed {
        fn read_ready(&mut self, _: &mut [u8]) -> std::io::Result<Option<usize>> {
            Ok(Some(0))
        }
        fn write_ready(&mut self, _: &[u8]) -> std::io::Result<Option<usize>> {
            Ok(Some(0))
        }
    }
    Pipes {
        tx: Box::new(Closed),
        rx: Box::new(Closed),
    }
}
