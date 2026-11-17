//! Quick Sync from the server's side: every call into Intel's runtime happens in a helper
//! process, one per encoder, and the server only talks to it ([`super::proto`]) and kills it.
//! A call already inside the driver can't be stopped in the process that made it; a process
//! can be ended.
//!
//! - Every request has a deadline: opening 10 s, a frame or a rate change 2 s. The caller
//!   waits for the reply until then, and a watchdog thread holds the server to it as well;
//!   a reply read at or past its deadline is a miss, however long it waited to be read.
//! - A missed deadline, or a helper reporting that the device hung, failed or was lost,
//!   gives Quick Sync up for the process: every helper is signalled at once, then all are
//!   reaped against one deadline, the reason is logged and returned (it shows in `/stats`),
//!   streams fall back to software, and no helper starts again. A helper still there after
//!   that is kept and counted in the reason.
//! - A helper that dies on its own (while coding or idle), or says something that isn't the
//!   protocol, is ended and counted: its stream fails over, one more may start, and a second
//!   death gives up.
//! - One frame at a time, as the other hardware encoders: `encode` returns that frame's
//!   picture, so nothing coded is left waiting in the pipe when capture goes idle.

use super::proc::{end_all, write_by, NotStarted, Pipes, Proc, Section, Unwritten};
use super::proto::{Decoder, Open, Rate, Reply, Request};
use crate::encoder::hw;
use crate::encoder::{
    AccessUnit, Codec, ColorInfo, EncodeError, EncodeOptions, EncoderConfig, FrameView, RateConfig,
    VideoEncoder,
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a helper may take.
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    pub open: Duration,
    pub frame: Duration,
    /// How long killed helpers may take to exit, all together.
    pub grace: Duration,
}

pub const TIMING: Timing = Timing {
    open: Duration::from_secs(10),
    frame: Duration::from_secs(2),
    grace: Duration::from_secs(2),
};

/// How often the watchdog looks.
const WATCH: Duration = Duration::from_millis(20);
const POLL: Duration = Duration::from_micros(200);
/// How long a closed helper may take to leave on its own before it is killed.
const CLOSE: Duration = Duration::from_millis(200);

struct Live {
    id: u64,
    proc: Arc<Proc>,
    /// When its request is due, and what that request is.
    due: Option<(Instant, &'static str)>,
}

struct State {
    off: Option<String>,
    deaths: u32,
    spawned: u32,
    next: u64,
    live: Vec<Live>,
    /// Helpers killed that did not exit: never dropped, so never mistaken for gone.
    stuck: Vec<Arc<Proc>>,
    watchdog: bool,
    #[cfg(test)]
    said: Vec<String>,
}

/// What [`Policy::settle`] does to a helper's entry: a new deadline, none, or off the list
/// (the caller ends the helper).
pub enum Then {
    Due(Instant, &'static str),
    Clear,
    Forget,
}

/// Starts the thread that enforces `policy`'s deadlines.
pub type StartWatchdog = fn(&'static Policy) -> std::io::Result<()>;

/// Quick Sync's state in this process: whether it is given up, its helpers, their
/// deadlines.
pub struct Policy {
    state: Mutex<State>,
    timing: Timing,
    start_watchdog: StartWatchdog,
}

fn watchdog_thread(p: &'static Policy) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("qsv-watchdog".into())
        .spawn(move || p.watch())
        .map(|_| ())
}

impl Policy {
    pub const fn new(timing: Timing) -> Policy {
        Policy::with_watchdog(timing, watchdog_thread)
    }

    pub const fn with_watchdog(timing: Timing, start_watchdog: StartWatchdog) -> Policy {
        Policy {
            state: Mutex::new(State {
                off: None,
                deaths: 0,
                spawned: 0,
                next: 0,
                live: Vec::new(),
                stuck: Vec::new(),
                watchdog: false,
                #[cfg(test)]
                said: Vec::new(),
            }),
            timing,
            start_watchdog,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Why Quick Sync is off in this process, if it is, with the helpers that did not exit.
    pub fn off(&self) -> Option<String> {
        let mut s = self.lock();
        s.stuck.retain(|p| !p.exited());
        let n = s.stuck.len();
        s.off.clone().map(|o| match n {
            0 => o,
            n => format!("{o}; {n} helper(s) did not exit"),
        })
    }

    /// Logs `line` about a helper at `level`; a test reads them back with [`Policy::said`].
    fn say(&self, level: log::Level, line: String) {
        log::log!(level, "{line}");
        #[cfg(test)]
        self.lock().said.push(line);
    }

    /// Every line logged about a helper so far.
    #[cfg(all(test, unix))]
    pub fn said(&self) -> Vec<String> {
        self.lock().said.clone()
    }

    /// Helpers started so far.
    #[cfg(all(test, unix))]
    pub fn spawned(&self) -> u32 {
        self.lock().spawned
    }

    /// Turns Quick Sync off for good: every missed deadline and every stopped device, from
    /// any helper or the probe, ends here. Under the lock it only marks Quick Sync off and
    /// signals every helper, and `also` if it isn't one of them; they are reaped against one
    /// deadline after it, and only then is anything logged: one line for each tracked helper
    /// it ended, then the reason. The first reason stays.
    pub fn give_up(&self, why: &str, also: Option<&Arc<Proc>>) {
        let (live, tracked, first) = {
            let mut s = self.lock();
            let first = s.off.is_none();
            if first {
                s.off = Some(format!("qsv given up: {why}"));
            }
            let mut live: Vec<Arc<Proc>> = std::mem::take(&mut s.live)
                .into_iter()
                .map(|l| l.proc)
                .collect();
            let tracked: Vec<u32> = live.iter().map(|p| p.id()).collect();
            if let Some(p) = also.filter(|p| !live.iter().any(|q| Arc::ptr_eq(q, p))) {
                live.push(p.clone());
            }
            for p in &live {
                p.kill();
            }
            (live, tracked, first)
        };
        let left = end_all(&live, self.timing.grace);
        let stuck = left.len();
        self.keep(left);
        for pid in tracked {
            let line = format!("qsv helper {pid}: ended: killed, Quick Sync given up ({why})");
            self.say(log::Level::Warn, line);
        }
        if first {
            log::error!("qsv given up: {why}; Quick Sync is off until starfold restarts");
        }
        if stuck > 0 {
            log::error!("{stuck} Quick Sync helper(s) did not exit after being killed");
        }
    }

    /// `p` missed its deadline for `what`: Quick Sync is given up, `p` with every helper.
    pub fn missed(&self, p: &Arc<Proc>, what: &str) {
        let why = format!("helper {} missed its deadline ({what})", p.id());
        self.give_up(&why, Some(p));
    }

    pub fn timing(&self) -> Timing {
        self.timing
    }

    /// Holds on to helpers that did not exit after being killed, each once however many
    /// paths end it.
    pub fn keep(&self, left: Vec<Arc<Proc>>) {
        if left.is_empty() {
            return;
        }
        let mut s = self.lock();
        for p in left {
            if !s.stuck.iter().any(|q| Arc::ptr_eq(q, &p)) {
                s.stuck.push(p);
            }
        }
    }

    /// Kills one helper and reaps it within the grace; one that stays turns Quick Sync off.
    pub fn end(&self, p: &Arc<Proc>) {
        let left = end_all(std::slice::from_ref(p), self.timing.grace);
        if !left.is_empty() {
            self.keep(left);
            self.give_up(
                &format!("helper {} did not exit after being killed", p.id()),
                None,
            );
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
            self.give_up(&format!("a second helper failed ({why})"), None);
        } else {
            log::warn!("a Quick Sync helper failed: {why}; one more may start");
        }
    }

    /// Tracks `proc`, starting the watchdog first if it isn't running. A helper that can't
    /// be held to its deadlines is ended.
    pub(super) fn register(&'static self, proc: Arc<Proc>) -> Result<u64, String> {
        let mut s = self.lock();
        if let Some(off) = s.off.clone() {
            drop(s);
            self.end(&proc);
            return Err(off);
        }
        if !s.watchdog {
            if let Err(e) = (self.start_watchdog)(self) {
                drop(s);
                self.end(&proc);
                return Err(format!(
                    "the Quick Sync watchdog didn't start ({e}), so no helper runs"
                ));
            }
            s.watchdog = true;
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

    /// The one way a helper's entry changes once it is registered (a give-up takes them all).
    /// Under the lock the clock is read first: an entry at or past its deadline is a miss,
    /// however the helper's work ended or how long that took to be seen, so Quick Sync is
    /// given up with that helper and `then` is not applied. Whether `then` was applied;
    /// false too if Quick Sync is off or `id` is no longer listed.
    pub(super) fn settle(&self, id: u64, then: Then) -> bool {
        let (proc, what) = {
            let mut s = self.lock();
            if s.off.is_some() {
                return false;
            }
            let Some(i) = s.live.iter().position(|l| l.id == id) else {
                return false;
            };
            match s.live[i].due {
                Some((at, what)) if Instant::now() >= at => (s.live[i].proc.clone(), what),
                _ => {
                    match then {
                        Then::Due(at, what) => s.live[i].due = Some((at, what)),
                        Then::Clear => s.live[i].due = None,
                        Then::Forget => drop(s.live.remove(i)),
                    }
                    return true;
                }
            }
        };
        self.missed(&proc, what);
        false
    }

    fn watch(&'static self) {
        while !self.look() {
            std::thread::sleep(WATCH);
        }
    }

    /// One look of the watchdog: a helper past its deadline gives Quick Sync up. Whether
    /// Quick Sync is off.
    pub(super) fn look(&self) -> bool {
        let late = {
            let s = self.lock();
            if s.off.is_some() {
                return true;
            }
            let now = Instant::now();
            s.live
                .iter()
                .find_map(|l| l.due.filter(|d| d.0 <= now).map(|d| (l.proc.clone(), d.1)))
        };
        if let Some((proc, what)) = late {
            self.missed(&proc, what);
        }
        self.lock().off.is_some()
    }
}

/// This process's Quick Sync.
pub static POLICY: Policy = Policy::new(TIMING);

/// An I420 picture laid out as the helper reads it from the section: Y, then U, then V,
/// each at its own width.
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
    size: (u32, u32),
    rate: Rate,
    stream: hw::Stream,
    /// Why this helper is no longer used.
    gone: Option<String>,
    /// The implementation the helper opened, and whether it runs LowPower.
    pub name: String,
    pub low_power: bool,
    /// Every frame request sent: pts, IDR asked.
    #[cfg(test)]
    pub sent: Vec<(i64, bool)>,
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
        spawn: impl FnOnce() -> Result<(Proc, Pipes), NotStarted>,
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
        let section = Section::new(slot).map_err(EncodeError)?;
        policy.lock().spawned += 1;
        let (proc, pipes) = match spawn() {
            Ok(p) => p,
            Err(n) => {
                if !n.left.is_empty() {
                    policy.keep(n.left);
                    policy.give_up(
                        &format!("a helper that failed to start did not exit ({})", n.why),
                        None,
                    );
                }
                return Err(EncodeError(n.why));
            }
        };
        let proc = Arc::new(proc);
        let id = match policy.register(proc.clone()) {
            Ok(id) => id,
            Err(e) => return Err(EncodeError(e)),
        };
        let rate = rate_of(&cfg.rate);
        let mut r = Remote {
            policy,
            id,
            proc,
            pipes,
            dec: Decoder::default(),
            section,
            size: (cfg.width, cfg.height),
            rate,
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
            slots: 1,
            section: shared,
            luid,
        });
        match r.request(&open, "opening", policy.timing.open)? {
            Reply::Opened { low_power, name } => {
                r.name = name;
                r.low_power = low_power;
                policy.say(log::Level::Info, r.opened_line(cfg.codec));
                Ok(r)
            }
            Reply::Error { stopped, reason } => {
                let e = r.refused(stopped, reason);
                r.gone.get_or_insert(e.0.clone());
                Err(e)
            }
            _ => Err(r.broke("answered before it opened")),
        }
    }

    /// The line logged when the helper opened its session for `codec`: its process, the
    /// implementation, the codec, and whether LowPower is on (only when the runtime refused it
    /// off; the helper's own line says how).
    fn opened_line(&self, codec: Codec) -> String {
        let codec = if codec == Codec::Hevc {
            "HEVC"
        } else {
            "H.264"
        };
        let mode = if self.low_power { "on" } else { "off" };
        format!(
            "qsv helper {}: opened {} for {codec}, LowPower {mode}",
            self.proc.id(),
            self.name
        )
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
            self.policy.give_up(&reason, Some(&self.proc));
            return self.stopped();
        }
        EncodeError(reason)
    }

    /// The helper died or broke the protocol: it is ended and counted.
    fn broke(&mut self, what: &str) -> EncodeError {
        let listed = self.policy.settle(self.id, Then::Forget);
        if !listed && self.policy.off().is_some() {
            return self.stopped();
        }
        let pid = self.proc.id();
        let why = format!("helper {pid} {what}");
        self.policy.end(&self.proc);
        if listed {
            let line = format!("qsv helper {pid}: ended: {what}");
            self.policy.say(log::Level::Warn, line);
        }
        self.policy.died(&why);
        self.gone = Some(why);
        self.stopped()
    }

    fn send(&mut self, r: &Request, what: &str, until: Instant) -> Result<(), EncodeError> {
        match write_by(&mut *self.pipes.tx, &r.bytes(), until) {
            Ok(()) => Ok(()),
            Err(Unwritten::Late) => {
                self.policy.missed(&self.proc, what);
                Err(self.stopped())
            }
            Err(Unwritten::Closed(how)) => Err(self.broke(&how)),
        }
    }

    /// Sends `req` and waits for its reply, `deadline` at most: past it, Quick Sync is
    /// given up, even if the reply is there by the time it is read.
    fn request(
        &mut self,
        req: &Request,
        what: &'static str,
        deadline: Duration,
    ) -> Result<Reply, EncodeError> {
        let until = Instant::now() + deadline;
        if !self.policy.settle(self.id, Then::Due(until, what)) {
            return Err(self.stopped());
        }
        self.send(req, what, until)?;
        match self.next(until)? {
            Some(r) if self.policy.settle(self.id, Then::Clear) => Ok(r),
            Some(_) => Err(self.stopped()),
            None => {
                self.policy.missed(&self.proc, what);
                Err(self.stopped())
            }
        }
    }

    /// Sets this helper's deadline for `what` to now, as a request that is overdue.
    #[cfg(all(test, unix))]
    pub fn overdue(&self, what: &'static str) {
        self.policy.settle(self.id, Then::Due(Instant::now(), what));
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
}

impl VideoEncoder for Remote {
    fn set_rate(&mut self, rate: RateConfig) -> Result<(), EncodeError> {
        self.usable()?;
        let r = rate_of(&rate);
        if r == self.rate {
            return Ok(());
        }
        match self.request(&Request::Rate(r), "a rate change", self.policy.timing.frame)? {
            Reply::RateSet => {
                self.rate = r;
                Ok(())
            }
            Reply::Error { stopped, reason } => Err(self.refused(stopped, reason)),
            _ => Err(self.broke("answered a rate change out of turn")),
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
        copy_planes(f, self.section.slice());
        let (pts, key) = (opts.pts_ms, opts.force_keyframe);
        #[cfg(test)]
        self.sent.push((pts, key));
        let req = Request::Frame { slot: 0, pts, key };
        match self.request(&req, "a frame", self.policy.timing.frame)? {
            Reply::Done { units, .. } if units.len() <= 1 => {
                let mut out = Vec::new();
                for u in units {
                    out.extend(self.stream.unit(u.data, pts, key)?);
                }
                Ok(out)
            }
            Reply::Done { .. } => Err(self.broke("coded one frame as more than one picture")),
            Reply::Error { stopped, reason } => Err(self.refused(stopped, reason)),
            _ => Err(self.broke("answered a frame out of turn")),
        }
    }

    /// Nothing is ever in flight between calls.
    fn drain(&mut self) -> Result<Vec<AccessUnit>, EncodeError> {
        self.usable()?;
        Ok(Vec::new())
    }
}

impl Drop for Remote {
    /// Closes the helper's stdin, gives it a moment to leave, then ends it either way. One
    /// found dead before that died on its own: it counts. Its end is logged here unless the
    /// path that took it off the list (a give-up, a failed request) logged it already.
    fn drop(&mut self) {
        let listed = self.policy.settle(self.id, Then::Forget);
        let died = self.gone.is_none() && self.policy.off().is_none() && self.proc.exited();
        let pipes = std::mem::replace(&mut self.pipes, closed());
        drop(pipes);
        let end = Instant::now() + CLOSE;
        while Instant::now() < end && !self.proc.exited() {
            std::thread::sleep(Duration::from_millis(5));
        }
        self.policy.end(&self.proc);
        if listed {
            let pid = self.proc.id();
            let (level, how) = if died {
                (log::Level::Warn, "exited while idle")
            } else {
                (log::Level::Info, "closed")
            };
            self.policy
                .say(level, format!("qsv helper {pid}: ended: {how}"));
        }
        if died {
            self.policy
                .died(&format!("helper {} exited while idle", self.proc.id()));
        }
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
