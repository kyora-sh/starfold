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
//! 16 bytes a frame for at most [`MAX_FPS`] frames a second of its length.
//!
//! Another encoder taking over mid-recording (new SPS/PPS) starts a new MP4 segment at
//! its keyframe. `format=zip` on `/record` or `/record/stop` returns `manifest.json` and
//! every `segment-k.mp4`; without it the response is the first segment, as an MP4, and
//! `x-starfold-segments` and `x-starfold-record-end` say when there was more.
//!
//! A recording is the H.264 stream starfold already encodes, rewrapped as MP4. It takes the
//! frames beside the viewers ([`tap`]), not as one of them, so it never holds the stream
//! back. Like the stream, it holds frames only when the screen changes, and it doesn't
//! include the cursor (viewers draw it locally), except on macOS with `--show-cursor` or
//! when the system cursor can't be read, where the capture carries it.

pub(crate) mod spool;
pub(crate) mod tap;

pub use spool::default_dir;
pub use tap::Taps;

use crate::hub::Hub;
use crate::mp4;
use axum::body::Body;
use axum::extract::{Query, Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
use tap::{Msg, Tap, Tapped};
use tokio::sync::oneshot;

/// How long a started recording that ran into the length limit waits for its
/// `/record/stop` before it is discarded (the caller may have crashed or forgotten).
const UNCLAIMED: Duration = Duration::from_secs(60);
/// The most segments of one recording: an encoder flapping between two configurations
/// ends the recording here.
const MAX_SEGMENTS: usize = 16;
/// The most video one recording holds on disk. It keeps an MP4's 32-bit sizes valid.
pub const MAX_RECORDING_BYTES: u64 = 2 << 30;
/// The most frames a second a recording indexes (the fastest cohort, `--high-fps`).
pub const MAX_FPS: u64 = crate::video::HIGH_FPS as u64;
/// What the writer gathers before each write; a sample this large or larger is written
/// directly.
pub const BUFFER: usize = 256 << 10;
/// What an answer reads from the spool at a time.
const CHUNK: usize = 64 << 10;
/// The most recordings at once, server-wide: `/record` and `/record/start` alike, each
/// until its clip is answered or dropped.
pub const MAX_RECORDINGS: usize = 2;
/// The most bytes written between two checks of the free space; less when the free space
/// is closer to the floor (half the room left).
const CHECK_EVERY: u64 = 64 << 20;
/// An answer whose client takes no bytes for this long is dropped, and its recording's
/// spool and place with it.
pub const IDLE: Duration = Duration::from_secs(30);

/// What a recording writes within; tests change it.
#[derive(Clone)]
struct Limits {
    /// The most video bytes one recording holds.
    max_bytes: u64,
    /// The free space to leave, from the filesystem's size ([`spool::floor`]).
    floor: Arc<dyn Fn(u64) -> u64 + Send + Sync>,
    check_every: u64,
    /// How long an answer waits for its client to take bytes.
    idle: Duration,
    /// Runs before each sample is written, with the bytes written so far; an error ends
    /// the recording as a write error would. None in the server; tests block or fail here.
    before_write: Option<Arc<dyn Fn(u64) -> io::Result<()> + Send + Sync>>,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_bytes: MAX_RECORDING_BYTES,
            floor: Arc::new(spool::floor),
            check_every: CHECK_EVERY,
            idle: IDLE,
            before_write: None,
        }
    }
}

/// One of the [`MAX_RECORDINGS`] places, given back when its last holder lets go: the
/// recording's writer, the started recording's entry, its clip and its answer.
struct Place(Arc<AtomicUsize>);

impl Drop for Place {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
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
    /// Started recordings by their id.
    active: Mutex<HashMap<String, Active>>,
    /// Numbers recordings in the log; never an id.
    next: AtomicU64,
}

struct Active {
    tap: Arc<Tap>,
    done: Recording,
    /// Its number in the log.
    n: u64,
    _place: Arc<Place>,
}

/// Whether the command line turns recording on (`--enable-recording`; off by default).
pub fn enabled(a: &HashMap<String, String>) -> bool {
    crate::truthy(a.get("enable-recording"), false)
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

type Recording = tokio::task::JoinHandle<Result<Clip, String>>;

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
    /// A segment couldn't be written; the others are there.
    SegmentLost,
    /// The stream it recorded stopped being one picture per frame.
    Closed,
    /// [`MAX_RECORDING_BYTES`].
    SizeLimit,
    /// [`MAX_FPS`] frames a second of its length.
    FrameCap,
    /// A frame larger than [`tap::LARGEST_FRAME`].
    LargeFrame,
    /// The disk fell [`tap::QUEUE`] behind the stream.
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

/// One segment while it is recorded: where its samples start in the index, its parameter
/// sets, and when it ended (the next segment's first frame).
struct Part {
    from: usize,
    params: Option<(Vec<u8>, Vec<u8>)>,
    end_ms: Option<u64>,
}

impl Part {
    fn at(from: usize, keyframe: &[u8]) -> Part {
        Part {
            from,
            params: mp4::parameter_sets(keyframe).ok(),
            end_ms: None,
        }
    }
}

/// A recording in progress: each frame's sample goes to the spool as it comes, and only
/// its index entry stays in memory.
struct Writer {
    out: BufWriter<File>,
    /// Where the spool is (its free space, on Windows).
    dir: PathBuf,
    limits: Limits,
    /// Bytes written since the free space was last checked, and how many until the next.
    since: u64,
    next_check: u64,
    start: Instant,
    /// The most frames it indexes; the index is reserved at this once.
    cap: usize,
    index: Vec<mp4::Sample>,
    parts: Vec<Part>,
    size: Option<(u32, u32)>,
    first: Option<Vec<Vec<u8>>>,
    last_ms: Option<u64>,
    /// Sample bytes handed to `out`.
    written: u64,
}

impl Writer {
    fn new(file: File, dir: PathBuf, limits: Limits, start: Instant, cap: usize) -> Writer {
        Writer {
            out: BufWriter::with_capacity(BUFFER, file),
            dir,
            limits,
            since: 0,
            next_check: 0,
            start,
            cap,
            index: Vec::with_capacity(cap),
            parts: Vec::new(),
            size: None,
            first: None,
            last_ms: None,
            written: 0,
        }
    }

    /// Writes one tapped frame, or says why the recording ends before it. The recording's
    /// size is its first frame's, as its encoder was opened: the size of the pictures it
    /// records, not the hub's, which a resize may already have changed.
    fn frame(&mut self, f: &Tapped) -> Result<(), End> {
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
        let ms = f
            .captured_at
            .saturating_duration_since(self.start)
            .as_millis() as u64;
        let ms = self.last_ms.map_or(ms, |l| ms.max(l + 1));
        if f.key {
            let p = params(d);
            match &self.first {
                None => {
                    self.first = Some(p);
                    self.parts.push(Part::at(self.index.len(), d));
                }
                // Another encoder took over the stream: it can't share the track, so its
                // keyframe starts the next segment.
                Some(prev) if !p.is_empty() && *prev != p => {
                    if !complete(&p) {
                        return Err(End::BadParameterSets);
                    }
                    if self.parts.len() == MAX_SEGMENTS {
                        return Err(End::SegmentCap);
                    }
                    if let Some(last) = self.parts.last_mut() {
                        last.end_ms = Some(ms);
                    }
                    self.parts.push(Part::at(self.index.len(), d));
                    self.first = Some(p);
                }
                Some(_) => {}
            }
        }
        if self.parts.is_empty() {
            self.parts.push(Part {
                from: 0,
                params: None,
                end_ms: None,
            });
        }
        let units = mp4::units(d);
        let size: u64 = units.iter().map(|n| 4 + n.len() as u64).sum();
        if self.written + size > self.limits.max_bytes {
            return Err(End::SizeLimit);
        }
        if let Some(hook) = &self.limits.before_write {
            hook(self.written).map_err(disk_error)?;
        }
        // Straight from the stream's buffer into the writer's: the frame is not copied.
        for n in &units {
            self.out
                .write_all(&(n.len() as u32).to_be_bytes())
                .map_err(disk_error)?;
            self.out.write_all(n).map_err(disk_error)?;
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
    /// left, at most [`Limits::check_every`], so the floor is never passed by more than that
    /// (and what the buffer holds).
    fn check_space(&mut self) -> Result<(), End> {
        let (free, total) = spool::space(self.out.get_ref(), &self.dir).map_err(disk_error)?;
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
    fn finish(self, mut end: End) -> Result<Clip, String> {
        let end_ms = self.start.elapsed().as_millis() as u64;
        let Writer {
            mut out,
            index,
            parts,
            size,
            ..
        } = self;
        if let Err(e) = out.flush() {
            let e = disk_error(e);
            if end != End::DiskError {
                end = e;
            }
        }
        let (file, _) = out.into_parts();
        // A write that failed may have left part of a sample: only whole ones count.
        let on_disk = file.metadata().map_or(0, |m| m.len());
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
            file,
        )
    }
}

/// Records what the tap hands over until it is ended, `max` after the writer's start, or a
/// limit. `release` gives each frame's charge back to the tap once it is written.
fn run(
    rx: mpsc::Receiver<Msg>,
    mut w: Writer,
    max: Duration,
    release: impl Fn(usize),
) -> Result<Clip, String> {
    let deadline = w.start + max;
    let end = loop {
        let f = match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Msg::Frame(f)) => f,
            Ok(Msg::End(why)) => break why,
            Err(mpsc::RecvTimeoutError::Timeout) => break End::TimeLimit,
            Err(mpsc::RecvTimeoutError::Disconnected) => break End::Closed,
        };
        let done = w.frame(&f);
        let charge = f.charge;
        drop(f);
        release(charge);
        if let Err(why) = done {
            break why;
        }
    };
    drop(rx);
    w.finish(end)
}

/// One segment of a finished recording: its MP4 header and where its samples are.
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

struct Clip {
    segments: Vec<Segment>,
    end: End,
    file: File,
    /// Held, not read: the clip keeps its recording's place until it is dropped.
    _place: Option<Arc<Place>>,
    /// How long its answer waits for the client to take bytes.
    idle: Duration,
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
        file: File,
    ) -> Result<Clip, String> {
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
            let header = match &p.params {
                None => Err("recording doesn't start with a keyframe".to_string()),
                Some((sps, pps)) => mp4::header(samples, sps, pps, size.0, size.1, seg_end),
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
                    end = End::SegmentLost;
                    failed.get_or_insert(e);
                }
            }
        }
        match failed {
            Some(e) if segments.is_empty() => Err(e),
            _ if segments.is_empty() => Err("no frames were recorded".into()),
            _ => Ok(Clip {
                segments,
                end,
                file,
                _place: None,
                idle: IDLE,
            }),
        }
    }

    fn manifest(&self) -> serde_json::Value {
        let segments: Vec<_> = self
            .segments
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
        serde_json::json!({ "end_reason": self.end.name(), "segments": segments })
    }

    /// Writes every segment as an MP4 in a stored zip, with `manifest.json` first, as it
    /// goes: nothing but a read buffer is held.
    fn write_zip(&self, out: &mut dyn Write) -> io::Result<()> {
        use zip::write::SimpleFileOptions;
        let mut z = zip::ZipWriter::new_stream(out);
        let opts = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .large_file(false);
        z.start_file("manifest.json", opts)
            .map_err(io::Error::other)?;
        z.write_all(self.manifest().to_string().as_bytes())?;
        for (i, s) in self.segments.iter().enumerate() {
            z.start_file(segment_name(i), opts)
                .map_err(io::Error::other)?;
            copy_segment(&self.file, s, &mut z)?;
        }
        z.finish().map_err(io::Error::other)?;
        Ok(())
    }
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

/// A response body written by `write` on a blocking thread as the client reads it: at
/// most a few chunks are in memory at a time. A client that goes away, or takes no bytes
/// for `idle`, stops it. `write` owns what it reads from (the whole clip), so the spool
/// and the recording's place are held until the body is done or stopped.
fn streamed(
    idle: Duration,
    write: impl FnOnce(&mut dyn Write) -> io::Result<()> + Send + 'static,
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
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(4);
    let (fail_tx, fail) = oneshot::channel::<io::Error>();
    let rt = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        let chan = Chan { tx, rt, idle };
        let mut out = io::BufWriter::with_capacity(CHUNK, chan);
        if let Err(e) = write(&mut out).and_then(|()| out.flush()) {
            log::warn!("recording answer stopped: {e}");
            let _ = fail_tx.send(e);
        }
    });
    // The chunks, then the error that stopped them, if one did: the answer ends as cut.
    Body::from_stream(futures_util::stream::unfold(
        (rx, Some(fail)),
        |(mut rx, fail)| async move {
            match rx.recv().await {
                Some(c) => Some((Ok(c), (rx, fail))),
                None => match fail?.await {
                    Ok(e) => Some((Err(e), (rx, None))),
                    Err(_) => None,
                },
            }
        },
    ))
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

/// The first segment, as an MP4; the response names what else there was.
fn first_segment(clip: Clip) -> Response {
    let n = clip.segments.len();
    let end = clip.end;
    let Some(first) = clip.segments.first() else {
        return err(StatusCode::INTERNAL_SERVER_ERROR, "no segments");
    };
    let length = first.header.len() as u64 + first.len;
    let body = streamed(clip.idle, move |out| {
        let clip = clip;
        copy_segment(&clip.file, &clip.segments[0], out)
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
    if n > 1 || end == End::SegmentLost {
        crate::compat::insert_header(
            res.headers_mut(),
            "x-starfold-segments",
            header::HeaderValue::from(n),
        );
        crate::compat::insert_header(
            res.headers_mut(),
            "x-starfold-record-end",
            header::HeaderValue::from_static(end.name()),
        );
    }
    res
}

/// What to log when a request gets only the first of `n` segments. `/record` has no id.
fn partial_warning(route: &str, id: Option<u64>, n: usize, zip: bool) -> Option<String> {
    let id = id.map_or("none".to_string(), |i| i.to_string());
    (!zip && n > 1).then(|| {
        format!("{route}: recording {id} has {n} segments; returning the first (format=zip has them all)")
    })
}

async fn respond(done: Recording, zip: bool, route: &str, id: Option<u64>) -> Response {
    let clip = match done.await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    if let Some(w) = partial_warning(route, id, clip.segments.len(), zip) {
        log::warn!("{w}");
    }
    if !zip {
        return first_segment(clip);
    }
    (
        [
            (header::CONTENT_TYPE, "application/zip"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"starfold.zip\"",
            ),
        ],
        streamed(clip.idle, move |out| {
            let clip = clip;
            clip.write_zip(out)
        }),
    )
        .into_response()
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
fn place(r: &Recorder) -> Result<Arc<Place>, Response> {
    r.running
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < MAX_RECORDINGS).then_some(n + 1)
        })
        .map(|_| Arc::new(Place(r.running.clone())))
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
        spool::Refused::LowSpace => err(
            StatusCode::INSUFFICIENT_STORAGE,
            "not enough free space in --recording-dir",
        ),
    }
}

/// Takes one of the places, opens a spool (off the async workers), takes a place beside
/// the stream ([`tap`]) and records until its tap is ended, `max`, a frame of another size
/// or a limit.
#[allow(clippy::type_complexity)]
async fn begin(
    r: &Arc<Recorder>,
    max: Duration,
) -> Result<(Recording, Arc<Tap>, Arc<Place>), Response> {
    if r.hub.stripes.lock().len() > 1 {
        return Err(err(
            StatusCode::CONFLICT,
            "recording needs a single-stripe stream (--stripes 1)",
        ));
    }
    let place = place(r)?;
    let (dir, floor) = (r.dir.clone(), r.limits.floor.clone());
    let file = tokio::task::spawn_blocking(move || spool::open(&dir, &*floor))
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|(why, detail)| {
            log::warn!("recording refused ({}): {detail}", r.dir.display());
            refused(why)
        })?;
    let cap = (MAX_FPS as f64 * max.as_secs_f64()).ceil() as usize;
    let (tap, rx) = r.hub.taps.add();
    r.hub.replan();
    let w = Writer::new(file, r.dir.clone(), r.limits.clone(), Instant::now(), cap);
    let (hub, t, idle, p) = (r.hub.clone(), tap.clone(), r.limits.idle, place.clone());
    let done = tokio::task::spawn_blocking(move || {
        let clip = run(rx, w, max, |c| t.release(c));
        t.close();
        hub.taps.remove(&t);
        hub.replan();
        clip.map(|c| Clip {
            _place: Some(p),
            idle,
            ..c
        })
    });
    Ok((done, tap, place))
}

/// A keyframe's SPS and PPS, the level left out: an MP4 track keeps the first
/// keyframe's, which still describe a stream that only changed level.
fn params(d: &[u8]) -> Vec<Vec<u8>> {
    mp4::nals(d)
        .into_iter()
        .filter(|n| matches!(n[0] & 0x1f, 7 | 8))
        .map(|n| {
            let mut v = n.to_vec();
            if v[0] & 0x1f == 7 && v.len() > 3 {
                v[3] = 0;
            }
            v
        })
        .collect()
}

/// Whether parameter sets from `params` can open an MP4 (`mp4::header` needs these).
fn complete(p: &[Vec<u8>]) -> bool {
    p.iter().any(|n| n[0] & 0x1f == 7 && n.len() >= 4) && p.iter().any(|n| n[0] & 0x1f == 8)
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
    // The deadline ends it.
    let (done, _tap, _place) = match begin(&r, len).await {
        Ok(d) => d,
        Err(e) => return e,
    };
    respond(done, zip, "/record", None).await
}

pub async fn start(State(r): State<Arc<Recorder>>) -> Response {
    if !r.on {
        return off();
    }
    let (done, tap, place) = match begin(&r, r.max).await {
        Ok(d) => d,
        Err(e) => return e,
    };
    let id = new_id();
    let n = r.next.fetch_add(1, Ordering::Relaxed);
    r.active.lock().insert(
        id.clone(),
        Active {
            tap,
            done,
            n,
            _place: place,
        },
    );
    log::info!("recording {n} started");
    // Nobody may ever stop it: once it has ended on its own, drop it after a grace period
    // instead of holding its spool forever.
    let rec = r.clone();
    let key = id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(rec.max + UNCLAIMED).await;
        if rec.active.lock().remove(&key).is_some() {
            log::info!("recording {n} was never stopped; discarded");
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
    a.tap.end(End::Stopped);
    log::info!("recording {} stopped", a.n);
    respond(a.done, zip, "/record/stop", Some(a.n)).await
}

#[cfg(test)]
mod tests;
