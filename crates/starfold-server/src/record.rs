//! Screenshots and recordings over HTTP, for agents and scripts:
//!
//! - `GET /screenshot`: the screen as a PNG.
//! - `GET /record?seconds=N`: records N seconds, returns an MP4.
//! - `POST /record/start` -> `{"id": "..."}`, then `POST /record/stop?id=...`: returns an
//!   MP4 of everything in between. The id is 128 random bits: only the caller that started
//!   a recording can stop it and get it.
//!
//! Recording is off unless the server runs with `--enable-recording`; then the three
//! answer 403. While a recording runs its video goes to a file with no name in
//! `--recording-dir` (see `spool`), never into memory, and the answer is read back from
//! that file. At most [`MAX_RECORDINGS`] run at once, each at most
//! [`MAX_RECORDING_BYTES`]; a recording also ends when the disk falls behind or runs low.
//!
//! Another encoder taking over mid-recording (new SPS/PPS) starts a new MP4 segment at
//! its keyframe. `format=zip` on `/record` or `/record/stop` returns `manifest.json` and
//! every `segment-k.mp4`; without it the response is the first segment, as an MP4, and
//! `x-starfold-segments` and `x-starfold-record-end` say when there was more.
//!
//! A recording is the H.264 stream starfold already encodes, rewrapped as MP4: the recorder
//! joins as one more viewer that acknowledges frames at once, so it costs almost no
//! CPU and never slows real viewers down. Like the stream, it holds frames only when
//! the screen changes, and it doesn't include the cursor (viewers draw it locally),
//! except on macOS with `--show-cursor` or when the system cursor can't be read, where
//! the capture carries it.

mod spool;

pub use spool::default_dir;

use crate::hub::{Client, Hub, Out};
use crate::mp4;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

/// How long a started recording that ran into the length limit waits for its
/// `/record/stop` before it is discarded (the caller may have crashed or forgotten).
const UNCLAIMED: Duration = Duration::from_secs(60);
/// The most segments of one recording: an encoder flapping between two configurations
/// ends the recording here.
const MAX_SEGMENTS: usize = 16;
/// The most recordings at once, server-wide: `/record` and `/record/start` alike, a
/// started one until it is stopped or discarded and its answer sent.
pub const MAX_RECORDINGS: usize = 2;
/// The most video one recording holds on disk. It keeps an MP4's 32-bit sizes valid.
pub const MAX_RECORDING_BYTES: u64 = 2 << 30;
/// The most video queued for the disk; a recording whose disk falls this far behind ends.
const QUEUE_BYTES: usize = 8 << 20;
/// How often (bytes written) a recording checks the free space again.
const CHECK_EVERY: u64 = 64 << 20;
/// What an answer reads from the spool at a time.
const CHUNK: usize = 64 << 10;

pub struct Recorder {
    pub hub: Arc<Hub>,
    /// Longest recording; a started one stops on its own after this.
    pub max: Duration,
    /// `--enable-recording`.
    on: bool,
    /// `--recording-dir`.
    dir: PathBuf,
    limits: spool::Limits,
    /// Recordings that hold one of the [`MAX_RECORDINGS`] places.
    running: Arc<AtomicUsize>,
    /// Started recordings by their id.
    active: Mutex<HashMap<String, Active>>,
    /// Numbers recordings in the log; never an id.
    next: AtomicU64,
}

struct Active {
    stop: oneshot::Sender<()>,
    done: Recording,
    /// Its number in the log.
    n: u64,
    _place: Arc<Place>,
}

type Recording = tokio::task::JoinHandle<Result<Clip, String>>;

/// One of the [`MAX_RECORDINGS`] places, given back when its last holder lets go: the
/// recording's task, the started recording's entry, and its answer.
struct Place(Arc<AtomicUsize>);

impl Drop for Place {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Why a recording ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    /// `/record/stop`.
    Stopped,
    /// The time asked for, or the length limit.
    TimeLimit,
    /// A new size: an MP4 track keeps one.
    Resized,
    /// Past `MAX_SEGMENTS`.
    SegmentCap,
    /// A keyframe with new parameter sets that can't start a segment.
    BadParameterSets,
    /// A segment couldn't be written; the others are there.
    SegmentLost,
    /// The hub went away.
    Closed,
    /// [`MAX_RECORDING_BYTES`].
    SizeLimit,
    /// The disk fell behind the stream.
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
            End::DiskSlow => "disk_slow",
            End::DiskSpace => "disk_space",
            End::DiskError => "disk_error",
        }
    }

    fn from_disk(stop: &spool::Stop) -> End {
        match stop {
            spool::Stop::Full => End::SizeLimit,
            spool::Stop::Slow => End::DiskSlow,
            spool::Stop::Space => End::DiskSpace,
            spool::Stop::Error(e) => {
                log::warn!("recording: writing failed: {e}");
                End::DiskError
            }
        }
    }

    fn is_disk(self) -> bool {
        matches!(
            self,
            End::SizeLimit | End::DiskSlow | End::DiskSpace | End::DiskError
        )
    }
}

/// One segment while it is recorded: the index of its samples (their bytes are in the
/// spool, in order), its parameter sets and size.
struct Part {
    samples: Vec<mp4::Sample>,
    params: Option<(Vec<u8>, Vec<u8>)>,
    size: (u32, u32),
    end_ms: u64,
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
    file: Arc<File>,
    _place: Arc<Place>,
}

impl Clip {
    /// The segments of the samples that reached the spool. One whose MP4 can't be made
    /// doesn't take the others with it; only with none left is the recording an error.
    fn build(
        parts: Vec<Part>,
        mut end: End,
        done: spool::Done,
        place: Arc<Place>,
    ) -> Result<Clip, String> {
        if let Some(stop) = &done.stop {
            if !end.is_disk() {
                end = End::from_disk(stop);
            }
        }
        let mut left = done.samples;
        let mut offset = 0u64;
        let mut segments = Vec::with_capacity(parts.len());
        let mut failed = None;
        for p in parts {
            let keep = p.samples.len().min(left);
            left -= keep;
            if keep == 0 && !p.samples.is_empty() {
                // None of it reached the spool before the recording ended.
                continue;
            }
            let samples = &p.samples[..keep];
            let len: u64 = samples.iter().map(|s| s.size as u64).sum();
            let start = offset;
            offset += len;
            let end_ms = p.samples.get(keep).map_or(p.end_ms, |next| next.ms);
            let header = match (samples.first(), &p.params) {
                (None, _) => Err("no frames were recorded".to_string()),
                (Some(_), None) => Err("recording doesn't start with a keyframe".to_string()),
                (Some(_), Some((sps, pps))) => {
                    mp4::header(samples, sps, pps, p.size.0, p.size.1, end_ms)
                }
            };
            match header {
                Ok(header) => {
                    let start_ms = samples[0].ms;
                    let last_ms = samples[keep - 1].ms;
                    segments.push(Segment {
                        header,
                        start,
                        len,
                        start_ms,
                        duration_ms: end_ms.max(last_ms + mp4::LAST_FRAME_MS) - start_ms,
                        frames: keep,
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
                file: Arc::new(done.file),
                _place: place,
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
/// most a few chunks are in memory at a time, and a client that goes away stops it.
fn streamed(write: impl FnOnce(&mut dyn Write) -> io::Result<()> + Send + 'static) -> Body {
    struct Chan(mpsc::Sender<io::Result<Bytes>>);
    impl Write for Chan {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0
                .blocking_send(Ok(Bytes::copy_from_slice(b)))
                .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let (tx, rx) = mpsc::channel::<io::Result<Bytes>>(4);
    tokio::task::spawn_blocking(move || {
        let mut out = io::BufWriter::with_capacity(CHUNK, Chan(tx.clone()));
        let result = write(&mut out).and_then(|()| out.flush());
        if let Err(e) = result {
            log::warn!("recording answer stopped: {e}");
            let _ = tx.blocking_send(Err(e));
        }
    });
    Body::from_stream(futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    }))
}

impl Recorder {
    /// `on`: `--enable-recording`; `dir`: `--recording-dir`.
    pub fn new(hub: Arc<Hub>, max: Duration, on: bool, dir: PathBuf) -> Arc<Self> {
        Self::with_limits(
            hub,
            max,
            on,
            dir,
            spool::Limits {
                max_bytes: MAX_RECORDING_BYTES,
                buffer: spool::BUFFER,
                queue_bytes: QUEUE_BYTES,
                check_every: CHECK_EVERY,
                floor: Arc::new(spool::floor),
                before_write: None,
            },
        )
    }

    fn with_limits(
        hub: Arc<Hub>,
        max: Duration,
        on: bool,
        dir: PathBuf,
        limits: spool::Limits,
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

/// A recording's id: 128 random bits from the system, as 32 hex digits.
fn new_id() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("random recording id");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The answer of every recording route while recording is off.
fn off() -> Response {
    (
        StatusCode::FORBIDDEN,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::json!({
            "error": "recording is off (start the server with --enable-recording)",
            "recording": false,
        })
        .to_string(),
    )
        .into_response()
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
    let body = streamed(move |out| copy_segment(&clip.file, &clip.segments[0], out));
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
        streamed(move |out| clip.write_zip(out)),
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

/// Opens a spool, joins as a viewer and records until `stop`, `max`, a geometry change or
/// a limit. `place` stays held until the clip is answered or dropped.
#[allow(clippy::result_large_err)]
fn begin(
    r: &Arc<Recorder>,
    stop: oneshot::Receiver<()>,
    max: Duration,
    place: Arc<Place>,
) -> Result<Recording, Response> {
    if r.hub.stripes.lock().len() > 1 {
        return Err(err(
            StatusCode::CONFLICT,
            "recording needs a single-stripe stream (--stripes 1)",
        ));
    }
    let file = spool::open(&r.dir, &r.limits).map_err(|(why, detail)| {
        log::warn!("recording refused ({}): {detail}", r.dir.display());
        refused(why)
    })?;
    let mut writer = spool::Writer::spawn(file, r.dir.clone(), r.limits.clone())
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let (tx, rx) = mpsc::unbounded_channel();
    let client = r.hub.add_client(tx);
    // A protocol 1 viewer: the H.264 stream the MP4 writer takes.
    client.set_video(true);
    r.hub.replan();
    let hub = r.hub.clone();
    Ok(tokio::spawn(async move {
        let (parts, end) = collect(&hub, &client, rx, stop, max, &mut writer).await;
        // Writing the rest takes a while: nothing acknowledges frames now, so leave first.
        hub.remove_client(client.id);
        let done = tokio::task::spawn_blocking(move || writer.finish())
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        Clip::build(parts, end, done, place)
    }))
}

/// Collects frames into the spool until `stop`, `max`, a geometry change or a limit. Each
/// frame is acknowledged and queued for the disk at once: nothing here waits on the disk.
async fn collect(
    hub: &Hub,
    client: &Client,
    mut rx: mpsc::UnboundedReceiver<Out>,
    mut stop: oneshot::Receiver<()>,
    max: Duration,
    writer: &mut spool::Writer,
) -> (Vec<Part>, End) {
    let start = Instant::now();
    let deadline = tokio::time::sleep(max);
    tokio::pin!(deadline);
    let mut parts: Vec<Part> = Vec::new();
    let mut samples: Vec<mp4::Sample> = Vec::new();
    let mut part_params: Option<(Vec<u8>, Vec<u8>)> = None;
    let mut size = (0, 0);
    let mut first: Option<Vec<Vec<u8>>> = None;
    let mut last_ms = None;
    let end = loop {
        tokio::select! {
            _ = &mut stop => break End::Stopped,
            _ = &mut deadline => break End::TimeLimit,
            m = rx.recv() => match m {
                None => break End::Closed,
                Some(Out::Bin(b)) if b.len() > 16 && b[0] == 0x01 => {
                    // Acknowledge at once, so the recorder never holds the stream back.
                    client.on_ack(u32::from_be_bytes([b[2], b[3], b[4], b[5]]));
                    hub.wake();
                    let key = b[1] & 1 == 1;
                    // Time the frame was captured: now, less the pipeline time in its header.
                    let pipe = u32::from_be_bytes([b[6], b[7], b[8], b[9]]) as u64 / 10;
                    let ms = (start.elapsed().as_millis() as u64).saturating_sub(pipe);
                    let ms = last_ms.map_or(ms, |l: u64| ms.max(l + 1));
                    if key {
                        let p = params(&b[16..]);
                        match &first {
                            None => first = Some(p),
                            // Another encoder took over the stream: it can't share the
                            // track, so its keyframe starts the next segment.
                            Some(f) if !p.is_empty() && *f != p => {
                                if !complete(&p) {
                                    break End::BadParameterSets;
                                }
                                if parts.len() + 1 == MAX_SEGMENTS {
                                    break End::SegmentCap;
                                }
                                parts.push(Part {
                                    samples: std::mem::take(&mut samples),
                                    params: part_params.take(),
                                    size,
                                    end_ms: ms,
                                });
                                first = Some(p);
                            }
                            Some(_) => {}
                        }
                        if part_params.is_none() {
                            part_params = mp4::parameter_sets(&b[16..]).ok();
                        }
                    }
                    if samples.is_empty() {
                        size = (hub.width.load(Ordering::Relaxed), hub.height.load(Ordering::Relaxed));
                    }
                    let sample = Bytes::from(mp4::sample(&b[16..]));
                    let len = sample.len() as u32;
                    if let Err(stop) = writer.push(sample) {
                        break End::from_disk(&stop);
                    }
                    last_ms = Some(ms);
                    samples.push(mp4::Sample { ms, key, size: len });
                }
                // A new size restarts the stream; an MP4 track keeps one size, so stop.
                Some(Out::Text(t)) if !samples.is_empty() && t.contains("\"hello\"") => {
                    break End::Resized
                }
                _ => {}
            },
        }
    };
    parts.push(Part {
        samples,
        params: part_params,
        size,
        end_ms: start.elapsed().as_millis() as u64,
    });
    (parts, end)
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

pub async fn record(
    State(r): State<Arc<Recorder>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if !r.on {
        return off();
    }
    let secs: f64 = q.get("seconds").and_then(|s| s.parse().ok()).unwrap_or(5.0);
    let Some(len) = record_len(secs, r.max) else {
        return err(StatusCode::BAD_REQUEST, "seconds must be positive");
    };
    let Some(zip) = wants_zip(&q) else {
        return err(StatusCode::BAD_REQUEST, BAD_FORMAT);
    };
    let place = match place(&r) {
        Ok(p) => p,
        Err(e) => return e,
    };
    // The deadline ends it; the stop side is never used.
    let (_keep, stop) = oneshot::channel();
    let done = match begin(&r, stop, len, place) {
        Ok(d) => d,
        Err(e) => return e,
    };
    respond(done, zip, "/record", None).await
}

pub async fn start(State(r): State<Arc<Recorder>>) -> Response {
    if !r.on {
        return off();
    }
    let place = match place(&r) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let (stop_tx, stop) = oneshot::channel();
    let done = match begin(&r, stop, r.max, place.clone()) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let id = new_id();
    let n = r.next.fetch_add(1, Ordering::Relaxed);
    r.active.lock().insert(
        id.clone(),
        Active {
            stop: stop_tx,
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

pub async fn stop(
    State(r): State<Arc<Recorder>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if !r.on {
        return off();
    }
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
    let _ = a.stop.send(());
    log::info!("recording {} stopped", a.n);
    respond(a.done, zip, "/record/stop", Some(a.n)).await
}

#[cfg(test)]
mod tests;
