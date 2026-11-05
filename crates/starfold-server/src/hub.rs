//! Shared state between media producers (video/audio/cursor/clipboard) and websocket clients.

use crate::negotiate::{Choice, DecodeCap, Offer};
use bytes::Bytes;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;

pub enum Out {
    Bin(Bytes),
    Text(String),
}

pub struct Inflight {
    pub id: u32,
    pub sent: Instant,
    pub bytes: usize,
}

/// The protocol a client speaks: unknown until it sends `v,0|1` (protocol 1) or a
/// capability hello (protocol 2) first.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Proto {
    #[default]
    Unknown,
    Legacy,
    V2,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    /// On no cohort.
    #[default]
    Idle,
    /// Sent a `codec` message; waiting for codec-ready.
    Announced(Instant),
    /// Gets the cohort's frames.
    Ready,
    /// Told that nothing the server streams decodes there.
    NoCodec,
}

/// Which cohort's frames a client gets, under which epoch. Changed only with the lock
/// held, and frames are sent with it held, so nothing of an old configuration is
/// queued after the message that replaces it.
#[derive(Default)]
pub struct Route {
    pub proto: Proto,
    /// Protocol 2: the decoder configurations of the client's last capability hello,
    /// and the offer generation they answer.
    pub caps: Vec<DecodeCap>,
    pub caps_gen: u64,
    pub cohort: Option<u64>,
    pub choice: Option<Choice>,
    /// Protocol 2: the configuration generation, one per `codec` message.
    pub epoch: u32,
    pub phase: Phase,
    /// Choices that failed on this client (codec-error or no codec-ready in time).
    pub rejected: Vec<Choice>,
}

impl Route {
    /// Takes the client off its cohort: nothing more is sent to it until it is routed again.
    pub fn detach(&mut self) {
        self.cohort = None;
        self.choice = None;
        self.phase = Phase::Idle;
    }

    fn current(&self, epoch: u32) -> bool {
        self.proto == Proto::V2 && self.cohort.is_some() && self.epoch == epoch
    }
}

pub struct Client {
    pub id: u64,
    pub tx: mpsc::UnboundedSender<Out>,
    pub video: AtomicBool,
    pub audio: AtomicBool,
    pub inflight: Mutex<VecDeque<Inflight>>,
    /// Smoothed min RTT (ms), from pings.
    pub min_rtt_ms: AtomicU32,
    /// Exponentially-weighted estimate of delivered bytes/sec, from acks.
    pub delivered: Mutex<RateEst>,
    pub needs_keyframe: AtomicBool,
    /// Has received a keyframe since (re)starting video.
    pub started: AtomicBool,
    pub delay: Mutex<DelayEst>,
    pub route: Mutex<Route>,
}

/// Send->ack delay tracking: `base` is the (slowly expiring) minimum, the excess over
/// it is queueing somewhere on the path (network buffers or a slow decoder).
pub struct DelayEst {
    pub base_ms: f64,
    pub base_at: Instant,
    pub queue_ms: f64,
}

pub struct RateEst {
    pub window_start: Instant,
    pub bytes: usize,
    pub rate_bps: f64,
}

impl Client {
    pub fn send(&self, m: Out) {
        let _ = self.tx.send(m);
    }

    pub fn on_ack(&self, id: u32) {
        let mut q = self.inflight.lock();
        let mut acked = 0;
        let mut sample = None;
        while let Some(f) = q.front() {
            if wrapping_le(f.id, id) {
                acked += f.bytes;
                if f.id == id {
                    sample = Some(f.sent.elapsed().as_secs_f64() * 1000.0);
                }
                q.pop_front();
            } else {
                break;
            }
        }
        drop(q);
        if let Some(ms) = sample {
            let mut e = self.delay.lock();
            if ms < e.base_ms || e.base_at.elapsed().as_secs() >= 10 {
                e.base_ms = ms;
                e.base_at = Instant::now();
            }
            let qd = ms - e.base_ms;
            e.queue_ms = e.queue_ms * 0.7 + qd * 0.3;
        }
        let mut d = self.delivered.lock();
        d.bytes += acked;
        let el = d.window_start.elapsed().as_secs_f64();
        if el >= 0.5 {
            let r = d.bytes as f64 * 8.0 / el;
            d.rate_bps = if d.rate_bps == 0.0 {
                r
            } else {
                d.rate_bps * 0.6 + r * 0.4
            };
            d.bytes = 0;
            d.window_start = Instant::now();
        }
    }

    /// `v,0|1`. A client that sends it before any capability hello speaks protocol 1.
    pub fn set_video(&self, on: bool) {
        let mut r = self.route.lock();
        if r.proto == Proto::Unknown {
            r.proto = Proto::Legacy;
        }
        self.video.store(on, Ordering::Relaxed);
        if r.proto == Proto::V2 {
            // Turned back on, it gets a new epoch (its worker reset its decoders).
            r.detach();
        }
        if on {
            self.inflight.lock().clear();
            self.started.store(false, Ordering::Relaxed);
            self.needs_keyframe
                .store(r.proto == Proto::Legacy, Ordering::Relaxed);
        }
    }

    /// A capability hello answering offer generation `gen`: the client speaks protocol 2
    /// from now on and is negotiated afresh.
    pub fn caps_hello(&self, caps: Vec<DecodeCap>, gen: u64) {
        let mut r = self.route.lock();
        r.proto = Proto::V2;
        r.caps = caps;
        r.caps_gen = gen;
        r.rejected.clear();
        r.detach();
        self.started.store(false, Ordering::Relaxed);
        self.needs_keyframe.store(false, Ordering::Relaxed);
    }

    /// codec-ready for `epoch`: false unless it is the epoch announced last.
    pub fn codec_ready(&self, epoch: u32) -> bool {
        let mut r = self.route.lock();
        if !r.current(epoch) || !matches!(r.phase, Phase::Announced(_)) {
            return false;
        }
        r.phase = Phase::Ready;
        self.inflight.lock().clear();
        self.started.store(false, Ordering::Relaxed);
        self.needs_keyframe.store(true, Ordering::Relaxed);
        true
    }

    /// codec-error for `epoch`: the choice is never tried on this client again. Returns
    /// it, or None for a stale epoch.
    pub fn codec_error(&self, epoch: u32) -> Option<Choice> {
        let mut r = self.route.lock();
        if !r.current(epoch) {
            return None;
        }
        let c = r.choice?;
        if !r.rejected.contains(&c) {
            r.rejected.push(c);
        }
        r.detach();
        Some(c)
    }

    /// An ack: `a,<frame>` (protocol 1, `epoch` None) or one for `epoch`. Acks for an
    /// older configuration, or in the other protocol, are dropped.
    pub fn ack(&self, epoch: Option<u32>, frame: u32) -> bool {
        let r = self.route.lock();
        let ok = match epoch {
            None => r.proto == Proto::Legacy,
            Some(e) => r.current(e) && r.phase == Phase::Ready,
        };
        if ok {
            self.on_ack(frame);
        }
        ok
    }

    /// A keyframe request (`kf`, or one for `epoch`), on the same terms as `ack`.
    pub fn request_keyframe(&self, epoch: Option<u32>) -> bool {
        let r = self.route.lock();
        let ok = match epoch {
            None => r.proto == Proto::Legacy,
            Some(e) => r.current(e) && r.phase == Phase::Ready,
        };
        if ok {
            self.started.store(false, Ordering::Relaxed);
            self.needs_keyframe.store(true, Ordering::Relaxed);
        }
        ok
    }

    /// Frames in flight and age of the oldest unacknowledged one.
    pub fn backlog(&self) -> (usize, u128) {
        let q = self.inflight.lock();
        (
            q.len(),
            q.front().map(|f| f.sent.elapsed().as_millis()).unwrap_or(0),
        )
    }
}

/// A non-blocking, close-on-exec pipe for waking a thread that polls file descriptors
/// (`pipe2` with those flags, which macOS lacks).
#[cfg(unix)]
pub fn wake_pipe() -> nix::Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    use nix::fcntl::{fcntl, FcntlArg, FdFlag, OFlag};
    let (r, w) = nix::unistd::pipe()?;
    for fd in [&r, &w] {
        fcntl(fd, FcntlArg::F_SETFL(OFlag::O_NONBLOCK))?;
        fcntl(fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
    }
    Ok((r, w))
}

/// Something `wait_any` can wait for: a file descriptor on Unix, a handle on Windows.
#[cfg(unix)]
pub type Waitable<'a> = std::os::fd::BorrowedFd<'a>;
#[cfg(windows)]
pub type Waitable<'a> = std::os::windows::io::BorrowedHandle<'a>;

/// Wakes a thread blocked in `wait_any`: a pipe on Unix, an event on Windows.
pub struct Wake {
    #[cfg(unix)]
    r: std::os::fd::OwnedFd,
    #[cfg(unix)]
    w: std::os::fd::OwnedFd,
    #[cfg(windows)]
    event: std::os::windows::io::OwnedHandle,
}

impl Wake {
    #[cfg(unix)]
    pub fn new() -> std::io::Result<Self> {
        let (r, w) = wake_pipe()?;
        Ok(Wake { r, w })
    }

    #[cfg(windows)]
    pub fn new() -> std::io::Result<Self> {
        use std::os::windows::io::FromRawHandle;
        // Manual reset: signalled until drained, however many wakes came in between.
        let h = unsafe { windows::Win32::System::Threading::CreateEventW(None, true, false, None) }
            .map_err(std::io::Error::other)?;
        Ok(Wake {
            event: unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(h.0) },
        })
    }

    pub fn signal(&self) {
        #[cfg(unix)]
        let _ = nix::unistd::write(&self.w, &[1u8]);
        #[cfg(windows)]
        unsafe {
            use std::os::windows::io::AsRawHandle;
            let _ = windows::Win32::System::Threading::SetEvent(
                windows::Win32::Foundation::HANDLE(self.event.as_raw_handle()),
            );
        }
    }

    pub fn drain(&self) {
        #[cfg(unix)]
        {
            let mut buf = [0u8; 64];
            while matches!(nix::unistd::read(&self.r, &mut buf), Ok(n) if n > 0) {}
        }
        #[cfg(windows)]
        unsafe {
            use std::os::windows::io::AsRawHandle;
            let _ = windows::Win32::System::Threading::ResetEvent(
                windows::Win32::Foundation::HANDLE(self.event.as_raw_handle()),
            );
        }
    }

    pub fn waitable(&self) -> Waitable<'_> {
        #[cfg(unix)]
        return std::os::fd::AsFd::as_fd(&self.r);
        #[cfg(windows)]
        return std::os::windows::io::AsHandle::as_handle(&self.event);
    }
}

/// Blocks until one of `w` is ready or `timeout_ms` passes (negative: no timeout).
pub fn wait_any(w: &[Waitable], timeout_ms: i32) {
    #[cfg(unix)]
    {
        use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
        let mut p: Vec<PollFd> = w
            .iter()
            .map(|&fd| PollFd::new(fd, PollFlags::POLLIN))
            .collect();
        let _ = poll(
            &mut p,
            PollTimeout::try_from(timeout_ms).unwrap_or(PollTimeout::NONE),
        );
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        let hs: Vec<HANDLE> = w.iter().map(|h| HANDLE(h.as_raw_handle())).collect();
        let ms = if timeout_ms < 0 {
            windows::Win32::System::Threading::INFINITE
        } else {
            timeout_ms as u32
        };
        unsafe { windows::Win32::System::Threading::WaitForMultipleObjects(&hs, false, ms) };
    }
}

fn wrapping_le(a: u32, b: u32) -> bool {
    b.wrapping_sub(a) < 0x8000_0000
}

#[derive(Default)]
pub struct Stats {
    pub captured: AtomicU64,
    pub encoded: AtomicU64,
    pub bytes: AtomicU64,
    pub encode_us: AtomicU64,
    pub grab_us: AtomicU64,
    pub convert_us: AtomicU64,
    pub flow_blocked: AtomicU64,
    pub paintovers: AtomicU64,
    /// Paint-over frames that came out, and their bytes.
    pub paintover_frames: AtomicU64,
    pub paintover_bytes: AtomicU64,
    pub keyframes: AtomicU64,
    pub refines: AtomicU64,
    pub damage_wait_us: AtomicU64,
    pub send_lat_us: AtomicU64,
}

pub struct Hub {
    pub clients: Mutex<Vec<Arc<Client>>>,
    pub next_id: AtomicU64,
    pub stats: Stats,
    pub width: AtomicU32,
    pub height: AtomicU32,
    /// Encoded stripes as (y, height).
    pub stripes: Mutex<Vec<(u32, u32)>>,
    /// Adaptive bitrate cap applied by the encoder (kbit/s).
    pub target_kbps: AtomicU32,
    /// Last cursor message, replayed to new clients.
    pub cursor: Mutex<Option<Bytes>>,
    /// Wakes the capture thread (acks, joins, keyframe requests).
    pub waker: Wake,
    pub input: Mutex<Option<std::sync::mpsc::Sender<crate::input::InputEvent>>>,
    pub last_clipboard: Mutex<Option<String>>,
    /// Desktop size a client asked for (dynamic resolution), applied by the video thread.
    pub resize_req: Mutex<Option<(u32, u32)>>,
    /// Largest desktop size (Some when dynamic resolution is on).
    pub resize_max: Mutex<Option<(u32, u32)>>,
    /// Server settings sent in every `hello` besides the geometry (audio, files, ...).
    pub hello_extra: Mutex<serde_json::Map<String, serde_json::Value>>,
    /// Screenshot requests, answered by the capture thread with the current screen.
    pub snapshots: Mutex<Vec<tokio::sync::oneshot::Sender<Arc<crate::record::Snapshot>>>>,
    /// When each part of the screen last changed (`/wait`).
    pub activity: crate::wait::Activity,
    /// What the server can stream at the current size (`videoOffers` in the hello), and
    /// its generation: capability hellos for an older one are ignored.
    pub offers: Mutex<Vec<Offer>>,
    pub offer_gen: AtomicU64,
    /// Viewers changed: the video thread routes them again.
    pub replan: AtomicBool,
    /// Test hook: the video thread treats the H.264 encoder as failed.
    pub fail_h264: AtomicBool,
    pub next_cohort: AtomicU64,
    /// The running encoder cohorts, for /stats.
    pub cohorts: Mutex<serde_json::Value>,
    /// `--encoder` and the encoder each codec streams from, for /stats.
    pub encoder: Mutex<serde_json::Value>,
}

impl Hub {
    pub fn new() -> Arc<Self> {
        Arc::new(Hub {
            clients: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            stats: Stats::default(),
            width: AtomicU32::new(0),
            height: AtomicU32::new(0),
            target_kbps: AtomicU32::new(0),
            stripes: Mutex::new(Vec::new()),
            cursor: Mutex::new(None),
            waker: Wake::new().expect("wake"),
            input: Mutex::new(None),
            last_clipboard: Mutex::new(None),
            resize_req: Mutex::new(None),
            resize_max: Mutex::new(None),
            hello_extra: Mutex::new(serde_json::Map::new()),
            snapshots: Mutex::new(Vec::new()),
            activity: Default::default(),
            offers: Mutex::new(Vec::new()),
            offer_gen: AtomicU64::new(0),
            replan: AtomicBool::new(false),
            fail_h264: AtomicBool::new(false),
            next_cohort: AtomicU64::new(1),
            cohorts: Mutex::new(serde_json::Value::Array(Vec::new())),
            encoder: Mutex::new(serde_json::Value::Null),
        })
    }

    pub fn wake(&self) {
        self.waker.signal();
    }

    pub fn drain_wake(&self) {
        self.waker.drain();
    }

    /// Asks the video thread to route viewers again.
    pub fn replan(&self) {
        self.replan.store(true, Ordering::Release);
        self.wake();
    }

    pub fn add_client(&self, tx: mpsc::UnboundedSender<Out>) -> Arc<Client> {
        let c = Arc::new(Client {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            tx,
            video: AtomicBool::new(false),
            audio: AtomicBool::new(false),
            inflight: Mutex::new(VecDeque::new()),
            min_rtt_ms: AtomicU32::new(0),
            delivered: Mutex::new(RateEst {
                window_start: Instant::now(),
                bytes: 0,
                rate_bps: 0.0,
            }),
            needs_keyframe: AtomicBool::new(true),
            started: AtomicBool::new(false),
            delay: Mutex::new(DelayEst {
                base_ms: f64::MAX,
                base_at: Instant::now(),
                queue_ms: 0.0,
            }),
            route: Mutex::new(Route::default()),
        });
        self.clients.lock().push(c.clone());
        c
    }

    pub fn remove_client(&self, id: u64) {
        self.clients.lock().retain(|c| c.id != id);
        self.replan();
    }

    pub fn video_clients(&self) -> Vec<Arc<Client>> {
        self.clients
            .lock()
            .iter()
            .filter(|c| c.video.load(Ordering::Relaxed))
            .cloned()
            .collect()
    }

    /// The `hello` message: current geometry plus the server's settings, and for
    /// protocol 2 clients the codecs on offer. `stripes` is the H.264 stream's layout.
    pub fn hello(&self) -> String {
        let mut m = self.hello_extra.lock().clone();
        m.insert("t".into(), "hello".into());
        m.insert("w".into(), self.width.load(Ordering::Relaxed).into());
        m.insert("h".into(), self.height.load(Ordering::Relaxed).into());
        m.insert("stripes".into(), serde_json::json!(*self.stripes.lock()));
        m.insert("protocol".into(), crate::negotiate::PROTOCOL.into());
        m.insert(
            "offer".into(),
            self.offer_gen.load(Ordering::Relaxed).into(),
        );
        m.insert("videoOffers".into(), serde_json::json!(*self.offers.lock()));
        serde_json::Value::Object(m).to_string()
    }

    /// Video clients on cohort `id` that get its frames.
    pub fn cohort_clients(&self, id: u64) -> Vec<Arc<Client>> {
        self.video_clients()
            .into_iter()
            .filter(|c| {
                let r = c.route.lock();
                r.cohort == Some(id) && (r.proto == Proto::Legacy || r.phase == Phase::Ready)
            })
            .collect()
    }

    /// After the stream restarts (new size): every client needs the new geometry and
    /// a keyframe, acks for the old stream's frames no longer apply, and protocol 2
    /// clients answer the new offers before they are routed again.
    pub fn restart_clients(&self) {
        let hello = self.hello();
        for c in self.clients.lock().iter() {
            let mut r = c.route.lock();
            r.detach();
            c.inflight.lock().clear();
            c.started.store(false, Ordering::Relaxed);
            c.needs_keyframe
                .store(r.proto != Proto::V2, Ordering::Relaxed);
            c.send(Out::Text(hello.clone()));
        }
        self.replan();
    }

    pub fn broadcast(&self, m: impl Fn() -> Out) {
        for c in self.clients.lock().iter() {
            c.send(m());
        }
    }
}
