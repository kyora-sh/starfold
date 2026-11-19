//! Screenshots and recordings over HTTP, for agents and scripts:
//!
//! - `GET /screenshot`: the screen as a PNG.
//! - `GET /record?seconds=N`: records N seconds, returns an MP4.
//! - `POST /record/start` -> `{"id": "..."}`, then `POST /record/stop?id=...`: returns an
//!   MP4 of everything in between. Only a holder of the id that start returned can stop a
//!   recording or obtain its clip: the id is returned once, to the caller of start, over
//!   the same authenticated channel; it is 128 bits from the OS and compared in constant
//!   time.
//!
//! All three answer 403 unless the server runs with `--enable-recording`; then nothing
//! takes the stream and nothing is held. With it, a recording's video goes to a file with
//! no name in `--recording-dir` ([`spool`]) as it comes, and the answer is read back from
//! that file. In memory a recording holds the frames waiting for its disk ([`tap::QUEUE`],
//! or one frame of up to [`tap::LARGEST_FRAME`]), the writer's [`BUFFER`], and an index of
//! 16 bytes a frame, grown as frames come, for at most [`MAX_FPS`] frames a second of its
//! length (at most [`MAX_RECORD_SECONDS`]).
//!
//! A recording holds one of [`MAX_RECORDINGS`] places ([`Place`]) from its start until
//! exactly one of two things: its answer (the clip, then the response body) is dropped, or
//! it ends with no answer to give. The writer finishing never gives the place back, and a
//! writer stuck on its disk does not keep it ([`STALL`]).
//!
//! Another encoder taking over mid-recording (new SPS/PPS, a new AV1 sequence header, or
//! the other codec) starts a new MP4 segment at its keyframe. `format=zip` on `/record` or `/record/stop` returns `manifest.json` and
//! every `segment-k.mp4`; without it the response is the first segment, as an MP4, and
//! `x-starfold-segments` and `x-starfold-record-end` say how many there were and why the
//! recording ended.
//!
//! A recording is the stream starfold already encodes, H.264 or AV1, rewrapped as MP4
//! (`avc1`, or `av01` with the sequence header in `av1C`). It takes the
//! frames beside the viewers ([`tap`]), not as one of them, so it never holds the stream
//! back. Like the stream, it holds frames only when the screen changes, and it doesn't
//! include the cursor (viewers draw it locally), except on macOS with `--show-cursor` or
//! when the system cursor can't be read, where the capture carries it.

pub(crate) mod spool;
pub(crate) mod tap;

pub use spool::default_dir;
pub use tap::Taps;

use crate::encoder::Codec;
use crate::hub::Hub;
use crate::mp4;
use axum::body::Body;
use axum::extract::{Query, Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use parking_lot::Mutex;
use spool::Spool;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
use tap::{Msg, Tap, Tapped};
use tokio::sync::oneshot;

/// How long a started recording's clip waits for its `/record/stop` once the recording has
/// ended on its own, before it is dropped (the caller may have crashed or forgotten).
const UNCLAIMED: Duration = Duration::from_secs(60);
/// The most segments of one recording: an encoder flapping between two configurations
/// ends the recording here.
const MAX_SEGMENTS: usize = 16;
/// The most video one recording holds on disk. It keeps an MP4's 32-bit sizes valid.
pub const MAX_RECORDING_BYTES: u64 = 2 << 30;
/// The most frames a second a recording indexes (the fastest cohort, `--high-fps`).
pub const MAX_FPS: u64 = crate::video::HIGH_FPS as u64;
/// The longest `--max-record-seconds` the server starts with. A recording's index is 16
/// bytes a frame for [`MAX_FPS`] frames a second of its length, at most 1,152,000 bytes at
/// this bound, and its MP4 header's sample tables at most as much again. It keeps one
/// recording within 10 MiB while it records: its queue (8 MiB at most), its buffer and its
/// index.
pub const MAX_RECORD_SECONDS: u64 = 600;
/// What the writer gathers before each write; a sample this large or larger is written
/// directly.
pub const BUFFER: usize = 256 << 10;
/// What an answer reads from the spool at a time.
const CHUNK: usize = 64 << 10;
/// The most recordings at once, server-wide: `/record` and `/record/start` alike, each
/// until its answer is dropped or it ends with none.
pub const MAX_RECORDINGS: usize = 2;
/// The most bytes written between two checks of the free space; less when the free space
/// is closer to the floor (half the room left).
const CHECK_EVERY: u64 = 64 << 20;
/// An answer whose client takes no bytes for this long is dropped, and its recording's
/// place with it.
pub const IDLE: Duration = Duration::from_secs(30);
/// A writer that spends this long in one disk operation ends its recording `disk_slow`,
/// and the recording's place is given back; the stuck thread keeps only its file (and
/// the disk space it reserved), its buffer, its index and the frames it had queued, until
/// the operation returns.
pub const STALL: Duration = Duration::from_secs(10);

/// What a recording writes within; tests change it.
#[derive(Clone)]
struct Limits {
    /// The most video bytes one recording holds.
    max_bytes: u64,
    /// The free space to leave, from the filesystem's size ([`spool::floor`]).
    floor: Arc<dyn Fn(u64) -> u64 + Send + Sync>,
    /// How the free space is measured ([`spool::space`]).
    space: Arc<spool::Space>,
    check_every: u64,
    /// How long an answer waits for its client to take bytes.
    idle: Duration,
    /// How long one disk operation of the writer may take.
    stall: Duration,
    /// Runs before each sample is written, with the bytes written so far, as part of the
    /// write; an error ends the recording as a write error would. None in the server;
    /// tests block or fail here.
    before_write: Option<Arc<dyn Fn(u64) -> io::Result<()> + Send + Sync>>,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_bytes: MAX_RECORDING_BYTES,
            floor: Arc::new(spool::floor),
            space: Arc::new(spool::space),
            check_every: CHECK_EVERY,
            idle: IDLE,
            stall: STALL,
            before_write: None,
        }
    }
}

/// One of the [`MAX_RECORDINGS`] places. It has one owner at a time: the recording's
/// route while it runs, then its clip, then the answer's body.
struct Place(Arc<AtomicUsize>);

impl Drop for Place {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Drops `x` on the blocking pool, as every spool is closed: closing a file is a file
/// operation, which never runs on an async worker. On a thread with no runtime, here.
pub(crate) fn off_workers<T: Send + 'static>(x: T) {
    match tokio::runtime::Handle::try_current() {
        Ok(rt) => {
            rt.spawn_blocking(move || drop(x));
        }
        Err(_) => drop(x),
    }
}

pub struct Recorder {
    pub hub: Arc<Hub>,
    /// Longest recording; a started one stops on its own after this.
    pub max: Duration,
    /// `--enable-recording`.
    on: bool,
    /// `--recording-dir`.
    dir: PathBuf,
    limits: Limits,
    /// Places held ([`MAX_RECORDINGS`]).
    running: Arc<AtomicUsize>,
    /// Disk space the recordings' spools reserved.
    budget: Arc<spool::Budget>,
    /// Started recordings by their id.
    active: Mutex<HashMap<String, Active>>,
    /// Numbers recordings in the log; never an id.
    next: AtomicU64,
}

struct Active {
    tap: Arc<Tap>,
    outcome: oneshot::Receiver<Outcome>,
    /// Its number in the log.
    n: u64,
}

/// Whether the command line turns recording on (`--enable-recording`; off by default).
pub fn enabled(a: &HashMap<String, String>) -> bool {
    crate::truthy(a.get("enable-recording"), false)
}

/// The recorder's command line, checked.
pub struct Settings {
    pub max: Duration,
    pub on: bool,
    pub dir: PathBuf,
}

/// `--max-record-seconds`, `--enable-recording` and `--recording-dir`. With recording on,
/// the length is at most [`MAX_RECORD_SECONDS`], and the directory is created and
/// resolved now, so the check that it is not inside `--files-root` (where the shared
/// folder would list and serve recordings) is of the directory recordings will use.
pub fn settings(a: &HashMap<String, String>) -> Result<Settings, String> {
    let on = enabled(a);
    let secs = crate::range_max(a.get("max-record-seconds"), 300.0);
    let given = a
        .get("recording-dir")
        .map_or_else(default_dir, PathBuf::from);
    if !on {
        let secs = secs.clamp(0.0, MAX_RECORD_SECONDS as f64);
        return Ok(Settings {
            max: Duration::from_secs_f64(secs),
            on,
            dir: given,
        });
    }
    if !(secs > 0.0 && secs <= MAX_RECORD_SECONDS as f64) {
        return Err(format!(
            "--max-record-seconds must be above 0 and at most {MAX_RECORD_SECONDS} \
             (a recording's index grows with its length)"
        ));
    }
    let existed = given.exists();
    std::fs::create_dir_all(&given)
        .and_then(|()| given.canonicalize())
        .map_err(|e| format!("--recording-dir {}: {e}", given.display()))
        .and_then(|dir| {
            let Some(root) = a.get("files-root") else {
                return Ok(dir);
            };
            let root = Path::new(root)
                .canonicalize()
                .map_err(|e| format!("--files-root {root}: {e}"))?;
            if !dir.starts_with(&root) {
                return Ok(dir);
            }
            if !existed {
                let _ = std::fs::remove_dir(&dir);
            }
            Err(format!(
                "--recording-dir {} is inside --files-root {}; recordings would be shared there",
                dir.display(),
                root.display()
            ))
        })
        .map(|dir| Settings {
            max: Duration::from_secs_f64(secs),
            on,
            dir,
        })
}

/// A recording's id: 128 random bits from the system, as 32 hex digits.
fn new_id() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("random recording id");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

const OFF: &str =
    r#"{"error":"recording is off (start the server with --enable-recording)","recording":false}"#;

/// The answer of every recording route while recording is off, before the request's query
/// is read.
fn off() -> Response {
    (
        StatusCode::FORBIDDEN,
        [(header::CONTENT_TYPE, "application/json")],
        OFF,
    )
        .into_response()
}

/// A request's query, read once recording is known to be on.
#[allow(clippy::result_large_err)]
fn query(req: &Request) -> Result<HashMap<String, String>, Response> {
    Query::<HashMap<String, String>>::try_from_uri(req.uri())
        .map(|Query(q)| q)
        .map_err(IntoResponse::into_response)
}

/// Why a recording ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum End {
    /// `/record/stop`.
    Stopped,
    /// The time asked for, or the length limit.
    TimeLimit,
    /// A frame of another size: an MP4 track keeps one.
    Resized,
    /// Past `MAX_SEGMENTS`.
    SegmentCap,
    /// A keyframe with new parameter sets that can't start a segment.
    BadParameterSets,
    /// A frame an MP4 sample can't be made of.
    BadFrame,
    /// A segment couldn't be written; the others are there.
    SegmentLost,
    /// The stream it recorded stopped being one picture per frame, or its writer or
    /// caller went away.
    Closed,
    /// The disk space it reserved, at most [`MAX_RECORDING_BYTES`].
    SizeLimit,
    /// [`MAX_FPS`] frames a second of its length.
    FrameCap,
    /// A frame larger than [`tap::LARGEST_FRAME`].
    LargeFrame,
    /// The disk fell [`tap::QUEUE`] behind the stream, or one write took [`STALL`].
    DiskSlow,
    /// The disk went below the free space a recording leaves.
    DiskSpace,
    /// Writing to the disk failed.
    DiskError,
}

impl End {
    fn name(self) -> &'static str {
        match self {
            End::Stopped => "stopped",
            End::TimeLimit => "time_limit",
            End::Resized => "resized",
            End::SegmentCap => "segment_cap",
            End::BadParameterSets => "bad_parameter_sets",
            End::BadFrame => "bad_frame",
            End::SegmentLost => "segment_lost",
            End::Closed => "closed",
            End::SizeLimit => "size_limit",
            End::FrameCap => "frame_cap",
            End::LargeFrame => "large_frame",
            End::DiskSlow => "disk_slow",
            End::DiskSpace => "disk_space",
            End::DiskError => "disk_error",
        }
    }
}

fn disk_error(e: io::Error) -> End {
    log::warn!("recording: writing failed: {e}");
    End::DiskError
}

/// A recording that has no clip to give, and why.
#[derive(Debug)]
struct Failed {
    end: End,
    why: String,
}

type Outcome = Result<Clip, Failed>;

/// When the writer's current disk operation began, for [`STALL`]; and whether its
/// recording gave up on it.
struct Busy {
    origin: Instant,
    /// Milliseconds from `origin` to the operation's start, plus one (0: none).
    since: AtomicU64,
    abandoned: AtomicBool,
}

impl Busy {
    fn new() -> Busy {
        Busy {
            origin: Instant::now(),
            since: AtomicU64::new(0),
            abandoned: AtomicBool::new(false),
        }
    }

    /// Marks one disk operation, until the guard is dropped.
    fn enter(&self) -> InDisk<'_> {
        let ms = self.origin.elapsed().as_millis() as u64 + 1;
        self.since.store(ms, Ordering::Release);
        InDisk(self)
    }

    /// Whether one disk operation has taken `limit` so far.
    fn stalled(&self, limit: Duration) -> bool {
        match self.since.load(Ordering::Acquire) {
            0 => false,
            ms => {
                (self.origin.elapsed().as_millis() as u64 + 1).saturating_sub(ms)
                    >= limit.as_millis() as u64
            }
        }
    }
}

struct InDisk<'a>(&'a Busy);

impl Drop for InDisk<'_> {
    fn drop(&mut self) {
        self.0.since.store(0, Ordering::Release);
    }
}

/// One segment while it is recorded: where its samples start in the index, its track
/// (from its keyframe's parameter sets or sequence header), and when it ended (the next
/// segment's first frame).
struct Part {
    from: usize,
    track: Option<mp4::Track>,
    end_ms: Option<u64>,
}

impl Part {
    fn at(from: usize, codec: Codec, keyframe: &[u8]) -> Part {
        Part {
            from,
            track: mp4::Track::of(codec, keyframe).ok(),
            end_ms: None,
        }
    }
}

/// A recording in progress: each frame's sample goes to the spool as it comes, and only
/// its index entry stays in memory.
struct Writer {
    out: BufWriter<Spool>,
    /// Where the spool is (its free space, on Windows).
    dir: PathBuf,
    limits: Limits,
    /// Bytes written since the free space was last checked, and how many until the next.
    since: u64,
    next_check: u64,
    start: Instant,
    /// Its length: frames captured this long after `start` or later are not in it.
    max: Duration,
    /// The most frames it indexes; the index grows up to it, never past.
    cap: usize,
    index: Vec<mp4::Sample>,
    parts: Vec<Part>,
    size: Option<(u32, u32)>,
    first: Option<(Codec, Vec<Vec<u8>>)>,
    last_ms: Option<u64>,
    /// Sample bytes handed to `out`.
    written: u64,
    busy: Arc<Busy>,
}

impl Writer {
    fn new(spool: Spool, dir: PathBuf, limits: Limits, max: Duration, busy: Arc<Busy>) -> Writer {
        Writer {
            out: BufWriter::with_capacity(BUFFER, spool),
            dir,
            limits,
            since: 0,
            next_check: 0,
            start: Instant::now(),
            max,
            cap: (MAX_FPS as f64 * max.as_secs_f64()).ceil() as usize,
            index: Vec::new(),
            parts: Vec::new(),
            size: None,
            first: None,
            last_ms: None,
            written: 0,
            busy,
        }
    }

    /// The bytes it may write: its size limit, within what its spool reserved.
    fn limit(&self) -> u64 {
        self.limits.max_bytes.min(self.out.get_ref().limit())
    }

    /// Milliseconds from the start to `at`, at most its length.
    fn ms(&self, at: Instant) -> u64 {
        at.saturating_duration_since(self.start)
            .min(self.max)
            .as_millis() as u64
    }

    /// Writes one tapped frame, or says why the recording ends before it. The recording's
    /// size is its first frame's, as its encoder was opened: the size of the pictures it
    /// records, not the hub's, which a resize may already have changed.
    fn frame(&mut self, f: &Tapped) -> Result<(), End> {
        if f.captured_at.saturating_duration_since(self.start) >= self.max {
            return Err(End::TimeLimit);
        }
        if *self.size.get_or_insert(f.size) != f.size {
            return Err(End::Resized);
        }
        if f.data.len() <= 16 {
            return Ok(());
        }
        let d = &f.data[16..];
        if self.index.len() >= self.cap {
            return Err(End::FrameCap);
        }
        let ms = self.ms(f.captured_at);
        let ms = self.last_ms.map_or(ms, |l| ms.max(l + 1));
        if f.key {
            let p = params(f.codec, d);
            match &self.first {
                None => {
                    self.first = Some(p);
                    self.parts.push(Part::at(self.index.len(), f.codec, d));
                }
                // Another encoder (or codec) took over the stream: it can't share the
                // track, so its keyframe starts the next segment.
                Some(prev) if prev.0 != p.0 || (!p.1.is_empty() && *prev != p) => {
                    if mp4::Track::of(f.codec, d).is_err() {
                        return Err(End::BadParameterSets);
                    }
                    if self.parts.len() == MAX_SEGMENTS {
                        return Err(End::SegmentCap);
                    }
                    if let Some(last) = self.parts.last_mut() {
                        last.end_ms = Some(ms);
                    }
                    self.parts.push(Part::at(self.index.len(), f.codec, d));
                    self.first = Some(p);
                }
                Some(_) => {}
            }
        } else if self.first.as_ref().is_some_and(|(c, _)| *c != f.codec) {
            return Err(End::BadFrame);
        }
        if self.parts.is_empty() {
            self.parts.push(Part {
                from: 0,
                track: None,
                end_ms: None,
            });
        }
        let units = mp4::sample_units(f.codec, d).map_err(|e| {
            log::warn!("recording: {e}");
            End::BadFrame
        })?;
        let size: u64 = units.iter().map(mp4::Unit::size).sum();
        if self.written + size > self.limit() {
            return Err(End::SizeLimit);
        }
        {
            let busy = self.busy.clone();
            let _disk = busy.enter();
            if let Some(hook) = &self.limits.before_write {
                hook(self.written).map_err(disk_error)?;
            }
            // Straight from the stream's buffer into the writer's: the frame is not copied.
            for u in &units {
                self.out.write_all(u.prefix()).map_err(disk_error)?;
                self.out.write_all(u.body).map_err(disk_error)?;
            }
        }
        if self.index.len() == self.index.capacity() {
            let more = self.index.len().max(64).min(self.cap - self.index.len());
            self.index.reserve_exact(more);
        }
        self.index.push(mp4::Sample {
            ms,
            key: f.key,
            size: size as u32,
        });
        self.written += size;
        self.last_ms = Some(ms);
        self.since += size;
        if self.since >= self.next_check {
            self.check_space()?;
        }
        Ok(())
    }

    /// Ends the recording below the floor; else sets when to look again: after half the room
    /// left, at most [`Limits::check_every`]. The spool's reservation keeps this server's
    /// recordings above the floor; this catches the disk filling from elsewhere.
    fn check_space(&mut self) -> Result<(), End> {
        let busy = self.busy.clone();
        let _disk = busy.enter();
        let (free, total) =
            (self.limits.space)(self.out.get_ref().file(), &self.dir).map_err(disk_error)?;
        let floor = (self.limits.floor)(total);
        if free < floor {
            log::warn!("recording: {free} bytes free, below the {floor} it leaves");
            return Err(End::DiskSpace);
        }
        self.since = 0;
        self.next_check = ((free - floor) / 2).min(self.limits.check_every);
        Ok(())
    }

    /// Writes out what is buffered and makes the clip of the samples that reached the file.
    /// `end_ms`: when the recording ended, from its start.
    fn finish(self, mut end: End, end_ms: u64) -> Outcome {
        let busy = self.busy.clone();
        let Writer {
            out,
            index,
            parts,
            size,
            ..
        } = self;
        let (spool, on_disk) = {
            let _disk = busy.enter();
            let mut out = out;
            if let Err(e) = out.flush() {
                let e = disk_error(e);
                if end != End::DiskError {
                    end = e;
                }
            }
            let (spool, _) = out.into_parts();
            // A write that failed may have left part of a sample: only whole ones count.
            let on_disk = spool.file().metadata().map_or(0, |m| m.len());
            (spool, on_disk)
        };
        let mut at = 0u64;
        let whole = index
            .iter()
            .take_while(|s| {
                at += s.size as u64;
                at <= on_disk
            })
            .count();
        Clip::build(
            &index,
            whole,
            &parts,
            size.unwrap_or_default(),
            end,
            end_ms,
            spool,
        )
    }
}

/// Records what the tap hands over until it is ended, its length after the writer's start
/// (by the frames' capture times, and by the clock when none comes), a limit, or its
/// recording gives up on it. `release` gives each frame's charge back to the tap once it is
/// written.
fn run(rx: mpsc::Receiver<Msg>, mut w: Writer, release: impl Fn(usize)) -> Outcome {
    let deadline = w.start + w.max;
    let mut last_at = w.start;
    let (end, at) = loop {
        if w.busy.abandoned.load(Ordering::Acquire) {
            break (End::DiskSlow, last_at);
        }
        let f = match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Msg::Frame(f)) => f,
            Ok(Msg::End(why, at)) => break (why, at),
            Err(mpsc::RecvTimeoutError::Timeout) => break (End::TimeLimit, deadline),
            Err(mpsc::RecvTimeoutError::Disconnected) => break (End::Closed, last_at),
        };
        let done = w.frame(&f);
        let (charge, at) = (f.charge, f.captured_at);
        drop(f);
        release(charge);
        if let Err(why) = done {
            break (why, at);
        }
        last_at = at;
    };
    for m in rx.try_iter() {
        if let Msg::Frame(f) = m {
            release(f.charge);
        }
    }
    drop(rx);
    let end_ms = w.ms(at);
    w.finish(end, end_ms)
}

/// One segment of a finished recording: its MP4 header and where its samples are.
#[derive(Clone)]
struct Segment {
    header: Vec<u8>,
    /// Its samples' offset and length in the spool.
    start: u64,
    len: u64,
    /// Since the recording started.
    start_ms: u64,
    duration_ms: u64,
    frames: usize,
}

/// A finished recording: its segments, read from its spool. It holds its recording's place
/// once its route has it, and hands both to its answer.
struct Clip {
    segments: Vec<Segment>,
    end: End,
    spool: Spool,
    place: Option<Place>,
}

impl Clip {
    /// The segments of the first `whole` samples of `index`, which reached the spool. One
    /// whose MP4 can't be made doesn't take the others with it; only with none left is the
    /// recording an error.
    fn build(
        index: &[mp4::Sample],
        whole: usize,
        parts: &[Part],
        size: (u32, u32),
        mut end: End,
        end_ms: u64,
        spool: Spool,
    ) -> Outcome {
        let mut segments = Vec::with_capacity(parts.len());
        let mut failed = None;
        let mut offset = 0u64;
        for (i, p) in parts.iter().enumerate() {
            let to = parts.get(i + 1).map_or(index.len(), |n| n.from);
            let kept = to.min(whole);
            if kept <= p.from {
                continue;
            }
            let samples = &index[p.from..kept];
            let len: u64 = samples.iter().map(|s| s.size as u64).sum();
            let start = offset;
            offset += len;
            let seg_end = match kept < to {
                true => index[kept].ms,
                false => p.end_ms.unwrap_or(end_ms),
            };
            let header = match &p.track {
                None => Err("recording doesn't start with a keyframe".to_string()),
                Some(track) => mp4::header(samples, track, size.0, size.1, seg_end),
            };
            match header {
                Ok(header) => {
                    let (start_ms, last_ms) = (samples[0].ms, samples[samples.len() - 1].ms);
                    segments.push(Segment {
                        header,
                        start,
                        len,
                        start_ms,
                        duration_ms: seg_end.max(last_ms + mp4::LAST_FRAME_MS) - start_ms,
                        frames: samples.len(),
                    });
                }
                Err(e) => {
                    log::warn!("recording segment lost: {e}");
                    failed.get_or_insert(e);
                }
            }
        }
        if failed.is_some() && !segments.is_empty() {
            end = End::SegmentLost;
        }
        match failed {
            Some(why) if segments.is_empty() => Err(Failed { end, why }),
            _ if segments.is_empty() => Err(Failed {
                end,
                why: "no frames were recorded".into(),
            }),
            _ => Ok(Clip {
                segments,
                end,
                spool,
                place: None,
            }),
        }
    }
}

fn manifest(segments: &[Segment], end: End) -> serde_json::Value {
    let segments: Vec<_> = segments
        .iter()
        .enumerate()
        .map(|(i, s)| {
            serde_json::json!({
                "file": segment_name(i),
                "start_ms": s.start_ms,
                "duration_ms": s.duration_ms,
                "frames": s.frames,
            })
        })
        .collect();
    serde_json::json!({ "end_reason": end.name(), "segments": segments })
}

/// Writes every segment as an MP4 in a stored zip, with `manifest.json` first, as it goes:
/// nothing but a read buffer is held.
fn write_zip(segments: &[Segment], end: End, file: &File, out: &mut dyn Write) -> io::Result<()> {
    use zip::write::SimpleFileOptions;
    let mut z = zip::ZipWriter::new_stream(out);
    let opts = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .large_file(false);
    z.start_file("manifest.json", opts)
        .map_err(io::Error::other)?;
    z.write_all(manifest(segments, end).to_string().as_bytes())?;
    for (i, s) in segments.iter().enumerate() {
        z.start_file(segment_name(i), opts)
            .map_err(io::Error::other)?;
        copy_segment(file, s, &mut z)?;
    }
    z.finish().map_err(io::Error::other)?;
    Ok(())
}

fn segment_name(i: usize) -> String {
    format!("segment-{}.mp4", i + 1)
}

/// Writes one segment as an MP4: its header, then its samples read from the spool.
fn copy_segment(file: &File, s: &Segment, out: &mut dyn Write) -> io::Result<()> {
    out.write_all(&s.header)?;
    let mut buf = vec![0u8; CHUNK];
    let (mut at, end) = (s.start, s.start + s.len);
    while at < end {
        let want = ((end - at) as usize).min(CHUNK);
        let n = spool::read_at(file, &mut buf[..want], at)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        out.write_all(&buf[..n])?;
        at += n as u64;
    }
    Ok(())
}

/// An answer's hold on its recording: the place and the spool. The body has it until the
/// body is dropped, or its client takes no bytes for its idle time.
struct Hold {
    _place: Option<Place>,
    _spool: Arc<Spool>,
}

/// The body's side of its [`Hold`]: let go when the body is dropped.
struct Holding(Arc<Mutex<Option<Hold>>>);

impl Drop for Holding {
    fn drop(&mut self) {
        let gone = self.0.lock().take();
        drop(gone);
    }
}

/// A response body that `write` produces on a blocking thread from the spool's file as the
/// client reads it: at most a few chunks are in memory at a time. The body holds the
/// recording's place and spool ([`Hold`]): they go when it is dropped, or when no bytes
/// have been taken from it for `idle` (a client that stopped reading, or a spool read that
/// doesn't return), which ends it as cut. The thread holds only the file, and stops at its
/// next chunk once nothing takes them.
fn streamed(
    place: Option<Place>,
    spool: Spool,
    idle: Duration,
    write: impl FnOnce(&File, &mut dyn Write) -> io::Result<()> + Send + 'static,
) -> Body {
    struct Chan {
        tx: tokio::sync::mpsc::Sender<Bytes>,
        rt: tokio::runtime::Handle,
        idle: Duration,
    }
    impl Write for Chan {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            let chunk = Bytes::copy_from_slice(b);
            match self
                .rt
                .block_on(tokio::time::timeout(self.idle, self.tx.send(chunk)))
            {
                Ok(Ok(())) => Ok(b.len()),
                Ok(Err(_)) => Err(io::ErrorKind::BrokenPipe.into()),
                Err(_) => Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("the client took nothing for {} s", self.idle.as_secs()),
                )),
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let spool = Arc::new(spool);
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(4);
    let (fail_tx, fail) = oneshot::channel::<io::Error>();
    let rt = tokio::runtime::Handle::current();
    let reader = spool.clone();
    tokio::task::spawn_blocking(move || {
        let chan = Chan { tx, rt, idle };
        let mut out = io::BufWriter::with_capacity(CHUNK, chan);
        if let Err(e) = write(reader.file(), &mut out).and_then(|()| out.flush()) {
            log::warn!("recording answer stopped: {e}");
            let _ = fail_tx.send(e);
        }
    });
    let hold = Arc::new(Mutex::new(Some(Hold {
        _place: place,
        _spool: spool,
    })));
    let taken = Arc::new(Mutex::new(Instant::now()));
    let cut = Arc::new(tokio::sync::Notify::new());
    {
        let (hold, taken, cut) = (hold.clone(), taken.clone(), cut.clone());
        tokio::spawn(async move {
            loop {
                let due = *taken.lock() + idle;
                tokio::time::sleep_until(due.into()).await;
                let mut h = hold.lock();
                if h.is_none() {
                    return;
                }
                if taken.lock().elapsed() >= idle {
                    let gone = h.take();
                    drop(h);
                    log::warn!(
                        "recording answer: no bytes taken for {} s; dropped",
                        idle.as_secs()
                    );
                    drop(gone);
                    cut.notify_one();
                    return;
                }
            }
        });
    }
    struct Stream {
        rx: tokio::sync::mpsc::Receiver<Bytes>,
        fail: Option<oneshot::Receiver<io::Error>>,
        holding: Holding,
        taken: Arc<Mutex<Instant>>,
        cut: Arc<tokio::sync::Notify>,
        done: bool,
    }
    let state = Stream {
        rx,
        fail: Some(fail),
        holding: Holding(hold),
        taken,
        cut,
        done: false,
    };
    // The chunks, then the error that stopped them, if one did: the answer ends as cut.
    Body::from_stream(futures_util::stream::unfold(state, |mut s| async move {
        if s.done {
            return None;
        }
        let gone = || io::Error::new(io::ErrorKind::TimedOut, "the answer was idle too long");
        if s.holding.0.lock().is_none() {
            s.done = true;
            return Some((Err(gone()), s));
        }
        let next = tokio::select! {
            c = s.rx.recv() => c,
            _ = s.cut.notified() => {
                s.done = true;
                return Some((Err(gone()), s));
            }
        };
        match next {
            Some(c) => {
                *s.taken.lock() = Instant::now();
                Some((Ok(c), s))
            }
            None => match s.fail.take()?.await {
                Ok(e) => {
                    s.done = true;
                    Some((Err(e), s))
                }
                Err(_) => None,
            },
        }
    }))
}

impl Recorder {
    /// `on`: [`enabled`]; `dir`: `--recording-dir`.
    pub fn new(hub: Arc<Hub>, max: Duration, on: bool, dir: PathBuf) -> Arc<Self> {
        Self::with_limits(hub, max, on, dir, Limits::default())
    }

    fn with_limits(
        hub: Arc<Hub>,
        max: Duration,
        on: bool,
        dir: PathBuf,
        limits: Limits,
    ) -> Arc<Self> {
        Arc::new(Recorder {
            hub,
            max,
            on,
            dir,
            limits,
            running: Arc::new(AtomicUsize::new(0)),
            budget: Arc::default(),
            active: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
        })
    }
}

/// The screen as captured (BGRX, 4 bytes a pixel, rows packed), for a screenshot.
pub struct Snapshot {
    pub width: usize,
    pub height: usize,
    pub bgrx: Vec<u8>,
    /// The latest change (`/wait`) the pixels include.
    pub seq: u64,
}

impl Snapshot {
    /// RGB (8 bits per channel), exactly as on screen.
    fn to_rgb(&self) -> Vec<u8> {
        let mut out = vec![0u8; self.width * self.height * 3];
        for (o, p) in out.chunks_exact_mut(3).zip(self.bgrx.chunks_exact(4)) {
            o.copy_from_slice(&[p[2], p[1], p[0]]);
        }
        out
    }
}

fn err(code: StatusCode, msg: impl Into<String>) -> Response {
    (
        code,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::json!({ "error": msg.into() }).to_string(),
    )
        .into_response()
}

pub async fn screenshot(State(r): State<Arc<Recorder>>) -> Response {
    let (tx, rx) = oneshot::channel();
    r.hub.snapshots.lock().push(tx);
    r.hub.wake();
    let Ok(Ok(frame)) = tokio::time::timeout(Duration::from_secs(5), rx).await else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "no frame from capture");
    };
    let seq = frame.seq;
    let png = tokio::task::spawn_blocking(move || {
        let (w, h) = (frame.width as u32, frame.height as u32);
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        enc.write_header()
            .and_then(|mut w| w.write_image_data(&frame.to_rgb()))
            .map(|_| out)
            .map_err(|e| e.to_string())
    })
    .await;
    match png {
        Ok(Ok(png)) => {
            let mut res = (
                [
                    (header::CONTENT_TYPE, "image/png"),
                    (header::CACHE_CONTROL, "no-store"),
                ],
                png,
            )
                .into_response();
            // For `/wait?change&since=`: changes after this screenshot.
            crate::compat::insert_header(
                res.headers_mut(),
                "x-starfold-seq",
                header::HeaderValue::from(seq),
            );
            res
        }
        Ok(Err(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// The segment count and the end reason, on every answer with a clip.
fn ends(res: &mut Response, segments: usize, end: End) {
    crate::compat::insert_header(
        res.headers_mut(),
        "x-starfold-segments",
        header::HeaderValue::from(segments),
    );
    crate::compat::insert_header(
        res.headers_mut(),
        "x-starfold-record-end",
        header::HeaderValue::from_static(end.name()),
    );
}

/// The first segment, as an MP4; the response names what else there was and why the
/// recording ended. `idle`: how long the answer waits for its client.
fn first_segment(clip: Clip, idle: Duration) -> Response {
    let Clip {
        mut segments,
        end,
        spool,
        place,
    } = clip;
    let n = segments.len();
    if n == 0 {
        return err(StatusCode::INTERNAL_SERVER_ERROR, "no segments");
    }
    segments.truncate(1);
    let first = segments.pop().expect("one segment");
    let length = first.header.len() as u64 + first.len;
    let body = streamed(place, spool, idle, move |file, out| {
        copy_segment(file, &first, out)
    });
    let mut res = (
        [
            (header::CONTENT_TYPE, "video/mp4"),
            (
                header::CONTENT_DISPOSITION,
                "inline; filename=\"starfold.mp4\"",
            ),
        ],
        body,
    )
        .into_response();
    res.headers_mut()
        .insert(header::CONTENT_LENGTH, header::HeaderValue::from(length));
    ends(&mut res, n, end);
    res
}

/// Every segment and the manifest, as a zip (no Content-Length: it is written as it goes).
fn all_segments(clip: Clip, idle: Duration) -> Response {
    let Clip {
        segments,
        end,
        spool,
        place,
    } = clip;
    let n = segments.len();
    let body = streamed(place, spool, idle, move |file, out| {
        write_zip(&segments, end, file, out)
    });
    let mut res = (
        [
            (header::CONTENT_TYPE, "application/zip"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"starfold.zip\"",
            ),
        ],
        body,
    )
        .into_response();
    ends(&mut res, n, end);
    res
}

/// The answer of a recording with no clip: why, with its end reason.
fn failed(f: Failed) -> Response {
    let code = match f.end {
        End::DiskSpace => StatusCode::INSUFFICIENT_STORAGE,
        End::DiskSlow => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let mut res = (
        code,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::json!({ "error": f.why, "end_reason": f.end.name() }).to_string(),
    )
        .into_response();
    ends(&mut res, 0, f.end);
    res
}

/// What to log when a request gets only the first of `n` segments. `/record` has no id.
fn partial_warning(route: &str, id: Option<u64>, n: usize, zip: bool) -> Option<String> {
    let id = id.map_or("none".to_string(), |i| i.to_string());
    (!zip && n > 1).then(|| {
        format!("{route}: recording {id} has {n} segments; returning the first (format=zip has them all)")
    })
}

fn respond(out: Outcome, idle: Duration, zip: bool, route: &str, id: Option<u64>) -> Response {
    let clip = match out {
        Ok(c) => c,
        Err(f) => return failed(f),
    };
    if let Some(w) = partial_warning(route, id, clip.segments.len(), zip) {
        log::warn!("{w}");
    }
    match zip {
        true => all_segments(clip, idle),
        false => first_segment(clip, idle),
    }
}

/// Whether `format=` asks for the zip of all segments. None for a format we don't have.
fn wants_zip(q: &HashMap<String, String>) -> Option<bool> {
    match q.get("format").map(String::as_str) {
        None | Some("mp4") => Some(false),
        Some("zip") => Some(true),
        Some(_) => None,
    }
}

const BAD_FORMAT: &str = "format must be mp4 or zip";

/// A place for one more recording, or the 429 that refuses it.
#[allow(clippy::result_large_err)]
fn place(r: &Recorder) -> Result<Place, Response> {
    r.running
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < MAX_RECORDINGS).then_some(n + 1)
        })
        .map(|_| Place(r.running.clone()))
        .map_err(|_| {
            err(
                StatusCode::TOO_MANY_REQUESTS,
                format!("at most {MAX_RECORDINGS} recordings run at once; stop one first"),
            )
        })
}

/// Why a recording couldn't start on its spool, as an answer.
fn refused(why: spool::Refused) -> Response {
    match why {
        spool::Refused::NotWritable => err(
            StatusCode::SERVICE_UNAVAILABLE,
            "recording needs a writable --recording-dir",
        ),
        spool::Refused::InMemory => err(
            StatusCode::SERVICE_UNAVAILABLE,
            "--recording-dir is in memory (tmpfs or ramfs); recording needs a directory on disk",
        ),
        spool::Refused::LowSpace => {
            let mut res = err(
                StatusCode::INSUFFICIENT_STORAGE,
                "not enough free space in --recording-dir",
            );
            ends(&mut res, 0, End::DiskSpace);
            res
        }
    }
}

#[allow(clippy::result_large_err)]
fn single_stripe(r: &Recorder) -> Result<(), Response> {
    match r.hub.stripes.lock().len() > 1 {
        true => Err(err(
            StatusCode::CONFLICT,
            "recording needs a single-stripe stream (--stripes 1, and a picture the encoder \
             doesn't split)",
        )),
        false => Ok(()),
    }
}

/// A running recording as its route holds it: the place, until the clip takes it.
struct Rec {
    hub: Arc<Hub>,
    tap: Arc<Tap>,
    done: oneshot::Receiver<Outcome>,
    busy: Arc<Busy>,
    place: Place,
    stall: Duration,
    /// Ends the recording if the route goes away first.
    _ender: Ender,
}

/// Ends a recording whose route went away (a `/record` client that left).
struct Ender(Arc<Tap>);

impl Drop for Ender {
    fn drop(&mut self) {
        self.0.end(End::Closed, Instant::now());
    }
}

impl Rec {
    /// The recording's clip, which then holds the place; or why there is none, and the
    /// place goes with this. A writer that has spent [`STALL`] in one disk operation is left
    /// to it: the recording ends `disk_slow` at once, its tap leaves the stream, and the
    /// writer stops at its next frame once the disk returns.
    async fn outcome(self) -> Outcome {
        let Rec {
            hub,
            tap,
            mut done,
            busy,
            place,
            stall,
            _ender,
        } = self;
        let mut tick = tokio::time::interval((stall / 4).max(Duration::from_millis(5)));
        loop {
            tokio::select! {
                r = &mut done => return match r {
                    Ok(Ok(mut c)) => {
                        c.place = Some(place);
                        Ok(c)
                    }
                    Ok(Err(f)) => Err(f),
                    Err(_) => Err(Failed {
                        end: End::Closed,
                        why: "the recording's writer stopped".into(),
                    }),
                },
                _ = tick.tick() => if busy.stalled(stall) {
                    busy.abandoned.store(true, Ordering::Release);
                    hub.taps.remove(&tap, End::DiskSlow);
                    hub.replan();
                    log::warn!("recording: one write took over {} s; ended", stall.as_secs());
                    return Err(Failed {
                        end: End::DiskSlow,
                        why: format!("a write to --recording-dir took over {} s", stall.as_secs()),
                    });
                },
            }
        }
    }
}

/// Takes one of the places, opens a spool (off the async workers, the place with it),
/// takes a place beside the stream ([`tap`]) and records until its tap is ended, `max`, a
/// frame of another size or a limit.
async fn begin(r: &Arc<Recorder>, max: Duration) -> Result<Rec, Response> {
    single_stripe(r)?;
    let place = place(r)?;
    let (dir, limits, budget) = (r.dir.clone(), r.limits.clone(), r.budget.clone());
    // The place goes into the open and comes back with it: a start cancelled while the open
    // blocks leaves the place there until the open returns.
    let (place, spool) = tokio::task::spawn_blocking(move || {
        let rules = spool::Rules {
            floor: &*limits.floor,
            space: &*limits.space,
            max_bytes: limits.max_bytes,
        };
        (place, spool::open(&dir, &rules, &budget))
    })
    .await
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let spool = spool.map_err(|(why, detail)| {
        log::warn!("recording refused ({}): {detail}", r.dir.display());
        refused(why)
    })?;
    // The layout may have changed while the spool opened.
    single_stripe(r)?;
    let (tap, rx) = r.hub.taps.add();
    r.hub.replan();
    let busy = Arc::new(Busy::new());
    let w = Writer::new(spool, r.dir.clone(), r.limits.clone(), max, busy.clone());
    let (tx, done) = oneshot::channel();
    let (hub, t) = (r.hub.clone(), tap.clone());
    tokio::task::spawn_blocking(move || {
        let out = run(rx, w, |c| t.release(c));
        hub.taps.remove(&t, End::Closed);
        hub.replan();
        // With nobody to take it, the outcome is dropped here, on this thread.
        drop(tx.send(out));
    });
    Ok(Rec {
        hub: r.hub.clone(),
        tap: tap.clone(),
        done,
        busy,
        place,
        stall: r.limits.stall,
        _ender: Ender(tap),
    })
}

/// A keyframe's codec and what describes its stream: H.264's SPS and PPS, the level left
/// out (an MP4 track keeps the first keyframe's, which still describe a stream that only
/// changed level); AV1's sequence header.
fn params(codec: Codec, d: &[u8]) -> (Codec, Vec<Vec<u8>>) {
    let p = match codec {
        Codec::H264 => mp4::nals(d)
            .into_iter()
            .filter(|n| matches!(n[0] & 0x1f, 7 | 8))
            .map(|n| {
                let mut v = n.to_vec();
                if v[0] & 0x1f == 7 && v.len() > 3 {
                    v[3] = 0;
                }
                v
            })
            .collect(),
        Codec::Av1 => crate::encoder::av1::sequence_header(d)
            .map(|s| vec![s.to_vec()])
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    (codec, p)
}

/// How long `GET /record?seconds=` records: `secs`, at most `max`. None unless positive.
fn record_len(secs: f64, max: Duration) -> Option<Duration> {
    if secs.is_nan() || secs <= 0.0 {
        return None;
    }
    // Cap before converting: from_secs_f64 panics on infinite or huge values, and
    // with panic = "abort" that would take the whole server down.
    Some(Duration::from_secs_f64(secs.min(max.as_secs_f64())))
}

pub async fn record(State(r): State<Arc<Recorder>>, req: Request) -> Response {
    if !r.on {
        return off();
    }
    let q = match query(&req) {
        Ok(q) => q,
        Err(e) => return e,
    };
    let secs: f64 = q.get("seconds").and_then(|s| s.parse().ok()).unwrap_or(5.0);
    let Some(len) = record_len(secs, r.max) else {
        return err(StatusCode::BAD_REQUEST, "seconds must be positive");
    };
    let Some(zip) = wants_zip(&q) else {
        return err(StatusCode::BAD_REQUEST, BAD_FORMAT);
    };
    // The deadline ends it; a client that leaves first ends it as it goes.
    let rec = match begin(&r, len).await {
        Ok(d) => d,
        Err(e) => return e,
    };
    respond(rec.outcome().await, r.limits.idle, zip, "/record", None)
}

pub async fn start(State(r): State<Arc<Recorder>>) -> Response {
    if !r.on {
        return off();
    }
    let rec = match begin(&r, r.max).await {
        Ok(d) => d,
        Err(e) => return e,
    };
    let id = new_id();
    let n = r.next.fetch_add(1, Ordering::Relaxed);
    let (tx, outcome) = oneshot::channel();
    r.active.lock().insert(
        id.clone(),
        Active {
            tap: rec.tap.clone(),
            outcome,
            n,
        },
    );
    log::info!("recording {n} started");
    // Nobody may ever stop it: once it has ended, its clip waits UNCLAIMED, then goes.
    let (rec_r, key) = (r.clone(), id.clone());
    tokio::spawn(async move {
        let out = rec.outcome().await;
        if let Err(out) = tx.send(out) {
            off_workers(out);
            return;
        }
        tokio::time::sleep(UNCLAIMED).await;
        let gone = rec_r.active.lock().remove(&key);
        if let Some(a) = gone {
            log::info!("recording {n} was never stopped; discarded");
            off_workers(a);
        }
    });
    (
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::json!({ "id": id, "max_seconds": r.max.as_secs() }).to_string(),
    )
        .into_response()
}

pub async fn stop(State(r): State<Arc<Recorder>>, req: Request) -> Response {
    if !r.on {
        return off();
    }
    let q = match query(&req) {
        Ok(q) => q,
        Err(e) => return e,
    };
    let Some(zip) = wants_zip(&q) else {
        return err(StatusCode::BAD_REQUEST, BAD_FORMAT);
    };
    let id = q.get("id").map_or("", String::as_str);
    let found = {
        let mut active = r.active.lock();
        let key = active.keys().find(|k| crate::login::same(k, id)).cloned();
        key.and_then(|k| active.remove(&k))
    };
    let Some(a) = found else {
        return err(StatusCode::NOT_FOUND, "no such recording");
    };
    a.tap.end(End::Stopped, Instant::now());
    log::info!("recording {} stopped", a.n);
    let out = a.outcome.await.unwrap_or_else(|_| {
        Err(Failed {
            end: End::Closed,
            why: "the recording went away".into(),
        })
    });
    respond(out, r.limits.idle, zip, "/record/stop", Some(a.n))
}

#[cfg(test)]
mod tests;
