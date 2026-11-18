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
//! joins the stream and nothing is held. With it, a recording is held in memory and is
//! not bounded beyond `--max-record-seconds`.
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

pub(crate) mod tap;

pub use tap::Taps;

use crate::hub::Hub;
use crate::mp4;
use axum::extract::{Query, Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
use tap::{Msg, Tap};
use tokio::sync::oneshot;

/// How long a started recording that ran into the length limit waits for its
/// `/record/stop` before it is discarded (the caller may have crashed or forgotten).
const UNCLAIMED: Duration = Duration::from_secs(60);
/// The most segments of one recording: an encoder flapping between two configurations
/// ends the recording here.
const MAX_SEGMENTS: usize = 16;

pub struct Recorder {
    pub hub: Arc<Hub>,
    /// Longest recording; a started one stops on its own after this.
    pub max: Duration,
    /// `--enable-recording`.
    on: bool,
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
        }
    }
}

/// One segment's frames, until they are written as an MP4.
struct Part {
    frames: Vec<mp4::Frame>,
    size: (u32, u32),
    end_ms: u64,
}

impl Part {
    fn write(self) -> Result<Segment, String> {
        let mp4 = mp4::write(&self.frames, self.size.0, self.size.1, self.end_ms)?;
        let (start_ms, last_ms) = match (self.frames.first(), self.frames.last()) {
            (Some(f), Some(l)) => (f.ms, l.ms),
            _ => (0, 0),
        };
        Ok(Segment {
            mp4,
            start_ms,
            duration_ms: self.end_ms.max(last_ms + mp4::LAST_FRAME_MS) - start_ms,
            frames: self.frames.len(),
        })
    }
}

struct Segment {
    mp4: Vec<u8>,
    /// Since the recording started.
    start_ms: u64,
    duration_ms: u64,
    frames: usize,
}

struct Clip {
    segments: Vec<Segment>,
    end: End,
}

impl Clip {
    /// Writes the MP4s. One that fails doesn't take the others with it; only with none
    /// left is the recording an error.
    fn write(parts: Vec<Part>, mut end: End) -> Result<Clip, String> {
        let mut segments = Vec::with_capacity(parts.len());
        let mut failed = None;
        for p in parts {
            match p.write() {
                Ok(s) => segments.push(s),
                Err(e) => {
                    log::warn!("recording segment lost: {e}");
                    end = End::SegmentLost;
                    failed.get_or_insert(e);
                }
            }
        }
        match failed {
            Some(e) if segments.is_empty() => Err(e),
            _ => Ok(Clip { segments, end }),
        }
    }

    fn into_zip(self) -> zip::result::ZipResult<Vec<u8>> {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let file = |i: usize| format!("segment-{}.mp4", i + 1);
        let segments: Vec<_> = self
            .segments
            .iter()
            .enumerate()
            .map(|(i, s)| {
                serde_json::json!({
                    "file": file(i),
                    "start_ms": s.start_ms,
                    "duration_ms": s.duration_ms,
                    "frames": s.frames,
                })
            })
            .collect();
        let manifest = serde_json::json!({ "end_reason": self.end.name(), "segments": segments });
        let total: usize = self.segments.iter().map(|s| s.mp4.len()).sum();
        let mut z = zip::ZipWriter::new(std::io::Cursor::new(Vec::with_capacity(total + 4096)));
        let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        z.start_file("manifest.json", opts)?;
        z.write_all(manifest.to_string().as_bytes())?;
        for (i, s) in self.segments.into_iter().enumerate() {
            z.start_file(file(i), opts)?;
            z.write_all(&s.mp4)?;
        }
        Ok(z.finish()?.into_inner())
    }
}

impl Recorder {
    /// `on`: [`enabled`].
    pub fn new(hub: Arc<Hub>, max: Duration, on: bool) -> Arc<Self> {
        Arc::new(Recorder {
            hub,
            max,
            on,
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

fn video(mp4: Vec<u8>) -> Response {
    (
        [
            (header::CONTENT_TYPE, "video/mp4"),
            (
                header::CONTENT_DISPOSITION,
                "inline; filename=\"starfold.mp4\"",
            ),
        ],
        mp4,
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
    let Some(first) = clip.segments.into_iter().next() else {
        return err(StatusCode::INTERNAL_SERVER_ERROR, "no segments");
    };
    let mut res = video(first.mp4);
    if n > 1 || clip.end == End::SegmentLost {
        crate::compat::insert_header(
            res.headers_mut(),
            "x-starfold-segments",
            header::HeaderValue::from(n),
        );
        crate::compat::insert_header(
            res.headers_mut(),
            "x-starfold-record-end",
            header::HeaderValue::from_static(clip.end.name()),
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
    match tokio::task::spawn_blocking(move || clip.into_zip()).await {
        Ok(Ok(z)) => (
            [
                (header::CONTENT_TYPE, "application/zip"),
                (
                    header::CONTENT_DISPOSITION,
                    "attachment; filename=\"starfold.zip\"",
                ),
            ],
            z,
        )
            .into_response(),
        Ok(Err(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
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

/// Takes a place beside the stream ([`tap`]) and records until its tap is ended, `max`, a
/// frame of another size or a limit.
#[allow(clippy::result_large_err)]
fn begin(r: &Arc<Recorder>, max: Duration) -> Result<(Recording, Arc<Tap>), Response> {
    if r.hub.stripes.lock().len() > 1 {
        return Err(err(
            StatusCode::CONFLICT,
            "recording needs a single-stripe stream (--stripes 1)",
        ));
    }
    let (tap, rx) = r.hub.taps.add();
    r.hub.replan();
    let (hub, t) = (r.hub.clone(), tap.clone());
    let start = Instant::now();
    let done = tokio::task::spawn_blocking(move || {
        let (parts, end) = collect(&rx, start, max);
        hub.taps.remove(&t);
        hub.replan();
        Clip::write(parts, end)
    });
    Ok((done, tap))
}

/// Collects tapped frames until the tap is ended, `max` after `start`, or a frame of
/// another size. The recording's size is its first frame's, as its encoder was opened:
/// the size of the pictures it records, not the hub's, which a resize may already have
/// changed.
fn collect(rx: &mpsc::Receiver<Msg>, start: Instant, max: Duration) -> (Vec<Part>, End) {
    let deadline = start + max;
    let mut parts: Vec<Part> = Vec::new();
    let mut frames: Vec<mp4::Frame> = Vec::new();
    let mut size: Option<(u32, u32)> = None;
    let mut first: Option<Vec<Vec<u8>>> = None;
    let mut last_ms = None;
    let end = loop {
        let f = match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Msg::Frame(f)) => f,
            Ok(Msg::End(why)) => break why,
            Err(mpsc::RecvTimeoutError::Timeout) => break End::TimeLimit,
            Err(mpsc::RecvTimeoutError::Disconnected) => break End::Closed,
        };
        if *size.get_or_insert(f.size) != f.size {
            break End::Resized;
        }
        if f.data.len() <= 16 {
            continue;
        }
        let ms = f.captured_at.saturating_duration_since(start).as_millis() as u64;
        let ms = last_ms.map_or(ms, |l: u64| ms.max(l + 1));
        if f.key {
            let p = params(&f.data[16..]);
            match &first {
                None => first = Some(p),
                // Another encoder took over the stream: it can't share the track, so its
                // keyframe starts the next segment.
                Some(prev) if !p.is_empty() && *prev != p => {
                    if !complete(&p) {
                        break End::BadParameterSets;
                    }
                    if parts.len() + 1 == MAX_SEGMENTS {
                        break End::SegmentCap;
                    }
                    parts.push(Part {
                        frames: std::mem::take(&mut frames),
                        size: f.size,
                        end_ms: ms,
                    });
                    first = Some(p);
                }
                Some(_) => {}
            }
        }
        last_ms = Some(ms);
        frames.push(mp4::Frame {
            ms,
            key: f.key,
            data: f.data.slice(16..),
        });
    };
    parts.push(Part {
        frames,
        size: size.unwrap_or_default(),
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

/// Whether parameter sets from `params` can open an MP4 (`mp4::write` needs these).
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
    let (done, _tap) = match begin(&r, len) {
        Ok(d) => d,
        Err(e) => return e,
    };
    respond(done, zip, "/record", None).await
}

pub async fn start(State(r): State<Arc<Recorder>>) -> Response {
    if !r.on {
        return off();
    }
    let (done, tap) = match begin(&r, r.max) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let id = new_id();
    let n = r.next.fetch_add(1, Ordering::Relaxed);
    r.active.lock().insert(id.clone(), Active { tap, done, n });
    log::info!("recording {n} started");
    // Nobody may ever stop it: once it has ended on its own, drop it after a grace period
    // instead of holding the clip in memory forever.
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
mod tests {
    use super::*;

    #[test]
    fn record_len_is_positive_and_capped() {
        let max = Duration::from_secs(300);
        assert_eq!(record_len(2.5, max), Some(Duration::from_millis(2500)));
        assert_eq!(record_len(1e100, max), Some(max));
        assert_eq!(record_len(f64::INFINITY, max), Some(max));
        assert_eq!(record_len(0.0, max), None);
        assert_eq!(record_len(-1.0, max), None);
        assert_eq!(record_len(f64::NAN, max), None);
    }

    /// A protocol 1 message: a keyframe with SPS (`profile`, `level`) and PPS, or a delta.
    fn message(key: Option<(u8, u8)>) -> bytes::Bytes {
        let mut b = vec![0x01, key.is_some() as u8, 0, 0, 0, 1];
        b.resize(16, 0);
        match key {
            Some((profile, level)) => b.extend_from_slice(&[
                0, 0, 0, 1, 0x67, profile, 0, level, 0xac, 0, 0, 0, 1, 0x68, 0xee, 0, 0, 0, 1,
                0x65, 0xaa,
            ]),
            None => b.extend_from_slice(&[0, 0, 0, 1, 0x41, 0xbb]),
        }
        bytes::Bytes::from(b)
    }

    /// A tapped frame of a 64x32 picture.
    fn unit(key: Option<(u8, u8)>) -> Msg {
        sized(key, (64, 32))
    }

    fn sized(key: Option<(u8, u8)>, size: (u32, u32)) -> Msg {
        Msg::Frame(tap::Tapped {
            size,
            key: key.is_some(),
            captured_at: Instant::now(),
            data: message(key),
        })
    }

    /// Records `msgs` in order, until one ends the recording.
    fn collected(msgs: Vec<Msg>) -> (Vec<Part>, End) {
        let (tx, rx) = mpsc::channel();
        for m in msgs {
            tx.send(m).unwrap();
        }
        collect(&rx, Instant::now(), Duration::from_secs(30))
    }

    /// Records `units`, then a stop.
    async fn clip(units: &[Option<(u8, u8)>]) -> Clip {
        let mut msgs: Vec<Msg> = units.iter().map(|u| unit(*u)).collect();
        msgs.push(Msg::End(End::Stopped));
        let (parts, end) = collected(msgs);
        Clip::write(parts, end).unwrap()
    }

    /// The payload of the box at `path`, each name found among the previous one's children.
    fn find<'a>(mut b: &'a [u8], path: &[&[u8; 4]]) -> &'a [u8] {
        for name in path {
            let mut at = 0;
            b = loop {
                let len = u32::from_be_bytes(b[at..at + 4].try_into().unwrap()) as usize;
                if &b[at + 4..at + 8] == *name {
                    break &b[at + 8..at + len];
                }
                at += len;
            };
        }
        b
    }

    fn be32(b: &[u8]) -> u32 {
        u32::from_be_bytes(b[..4].try_into().unwrap())
    }

    /// Checks the boxes agree; returns the sample count, their total duration, avcC's SPS and PPS.
    fn check(mp4: &[u8]) -> (u32, u32, Vec<u8>, Vec<u8>) {
        assert_eq!(&mp4[4..8], b"ftyp");
        let moov = find(mp4, &[b"moov"]);
        let mdat = find(mp4, &[b"mdat"]);
        let stbl = find(mp4, &[b"moov", b"trak", b"mdia", b"minf", b"stbl"]);
        let samples = be32(&find(stbl, &[b"stsz"])[8..]);
        let stts = find(stbl, &[b"stts"]);
        let runs = be32(&stts[4..]) as usize;
        let total: u32 = (0..runs)
            .map(|i| be32(&stts[8 + i * 8..]) * be32(&stts[12 + i * 8..]))
            .sum();
        assert_eq!(be32(&find(moov, &[b"mvhd"])[16..]), total);
        let sizes = find(stbl, &[b"stsz"]);
        let bytes: u32 = (0..samples as usize)
            .map(|i| be32(&sizes[12 + i * 4..]))
            .sum();
        assert_eq!(bytes as usize, mdat.len());
        let at = moov.windows(4).position(|w| w == b"avcC").unwrap() + 4;
        let c = &moov[at..];
        let sps_len = u16::from_be_bytes([c[6], c[7]]) as usize;
        let sps = c[8..8 + sps_len].to_vec();
        let p = 8 + sps_len;
        let pps_len = u16::from_be_bytes([c[p + 1], c[p + 2]]) as usize;
        (samples, total, sps, c[p + 3..p + 3 + pps_len].to_vec())
    }

    #[tokio::test]
    async fn another_encoders_keyframe_starts_a_segment() {
        let c = clip(&[
            Some((100, 40)),
            None,
            Some((100, 41)),
            None,
            Some((66, 41)),
            None,
        ])
        .await;
        assert_eq!(c.end, End::Stopped);
        assert_eq!(c.segments.len(), 2);
        let (a, b) = (&c.segments[0], &c.segments[1]);
        let (samples, total, sps, pps) = check(&a.mp4);
        assert_eq!(samples, 4, "a new level continues");
        assert_eq!(a.frames, 4);
        assert_eq!(total as u64, a.duration_ms);
        assert_eq!(sps, [0x67, 100, 0, 40, 0xac]);
        assert_eq!(pps, [0x68, 0xee]);
        let (samples, total, sps, pps) = check(&b.mp4);
        assert_eq!(samples, 2);
        assert_eq!(b.frames, 2);
        assert_eq!(total as u64, b.duration_ms);
        assert_eq!(sps, [0x67, 66, 0, 41, 0xac]);
        assert_eq!(pps, [0x68, 0xee]);
        assert!(a.start_ms < b.start_ms);
        assert!(
            b.start_ms <= a.start_ms + a.duration_ms,
            "no gap between the segments"
        );
    }

    #[tokio::test]
    async fn a_new_level_keeps_one_segment() {
        let c = clip(&[Some((100, 40)), None, Some((100, 41)), None]).await;
        assert_eq!(c.segments.len(), 1);
        assert_eq!(check(&c.segments[0].mp4).0, 4);
    }

    #[tokio::test]
    async fn a_flapping_encoder_ends_at_the_segment_cap() {
        let units: Vec<_> = (0..MAX_SEGMENTS + 5)
            .flat_map(|i| [Some((if i % 2 == 0 { 100 } else { 66 }, 40)), None])
            .collect();
        let c = clip(&units).await;
        assert_eq!(c.end, End::SegmentCap);
        assert_eq!(c.segments.len(), MAX_SEGMENTS);
        for s in &c.segments {
            assert_eq!(check(&s.mp4).0, 2);
        }
    }

    #[tokio::test]
    async fn a_keyframe_that_cant_start_a_segment_ends_the_recording() {
        let mut b = vec![0x01, 1, 0, 0, 0, 2];
        b.resize(16, 0);
        b.extend_from_slice(&[0, 0, 0, 1, 0x68, 0x11, 0, 0, 0, 1, 0x65, 0xaa]);
        let bad = Msg::Frame(tap::Tapped {
            size: (64, 32),
            key: true,
            captured_at: Instant::now(),
            data: bytes::Bytes::from(b),
        });
        let (parts, end) = collected(vec![unit(Some((100, 40))), bad]);
        let c = Clip::write(parts, end).unwrap();
        assert_eq!(c.end, End::BadParameterSets);
        assert_eq!(c.segments.len(), 1);
        assert_eq!(check(&c.segments[0].mp4).0, 1);
    }

    #[test]
    fn only_a_request_that_loses_segments_is_warned_about() {
        let w = partial_warning("/record/stop", Some(7), 3, false).unwrap();
        for s in ["/record/stop", "recording 7", "3 segments"] {
            assert!(w.contains(s), "{w}");
        }
        let w = partial_warning("/record", None, 2, false).unwrap();
        for s in ["/record:", "recording none", "2 segments"] {
            assert!(w.contains(s), "{w}");
        }
        assert_eq!(partial_warning("/record/stop", Some(7), 3, true), None);
        assert_eq!(partial_warning("/record/stop", Some(7), 1, false), None);
        assert_eq!(partial_warning("/record", None, 1, false), None);
    }

    fn part(frames: Vec<mp4::Frame>) -> Part {
        Part {
            frames,
            size: (64, 32),
            end_ms: 100,
        }
    }

    fn keyframe() -> Vec<mp4::Frame> {
        let b = message(Some((100, 40)));
        vec![mp4::Frame {
            ms: 0,
            key: true,
            data: b.slice(16..),
        }]
    }

    #[test]
    fn a_segment_that_cant_be_written_leaves_the_others() {
        let parts = vec![part(keyframe()), part(Vec::new()), part(keyframe())];
        let c = Clip::write(parts, End::Stopped).unwrap();
        assert_eq!(c.end, End::SegmentLost);
        assert_eq!(c.segments.len(), 2);
        let r = first_segment(c);
        assert_eq!(r.headers()["x-starfold-segments"], "2");
        assert_eq!(r.headers()["x-starfold-record-end"], "segment_lost");
        assert_eq!(r.headers()["x-voxl-segments"], "2");
        assert_eq!(r.headers()["x-voxl-record-end"], "segment_lost");
    }

    #[test]
    fn a_recording_with_no_segment_left_is_an_error() {
        let e = Clip::write(vec![part(Vec::new())], End::Stopped).err();
        assert_eq!(e.as_deref(), Some("no frames were recorded"));
    }

    #[tokio::test]
    async fn a_recording_is_beside_the_viewers_and_leaves_when_it_ends() {
        let r = Recorder::new(Hub::new(), Duration::from_secs(30), true);
        let (done, tap) = begin(&r, Duration::from_secs(30)).ok().unwrap();
        assert!(r.hub.taps.any());
        assert!(r.hub.clients.lock().is_empty(), "no viewer joined");
        tap.end(End::Stopped);
        assert!(done.await.unwrap().is_err(), "no frames were recorded");
        assert!(!r.hub.taps.any());
    }

    async fn body(r: Response) -> Vec<u8> {
        axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec()
    }

    #[tokio::test]
    async fn one_segment_is_a_plain_mp4() {
        let c = clip(&[Some((100, 40)), None]).await;
        let mp4 = c.segments[0].mp4.clone();
        let r = first_segment(c);
        assert_eq!(r.headers()[header::CONTENT_TYPE], "video/mp4");
        assert!(r.headers().get("x-starfold-segments").is_none());
        assert!(r.headers().get("x-starfold-record-end").is_none());
        assert_eq!(body(r).await, mp4);
    }

    #[tokio::test]
    async fn without_zip_several_segments_are_the_first_and_headers() {
        let c = clip(&[Some((100, 40)), Some((66, 40))]).await;
        let mp4 = c.segments[0].mp4.clone();
        let r = first_segment(c);
        assert_eq!(r.headers()["x-starfold-segments"], "2");
        assert_eq!(r.headers()["x-starfold-record-end"], "stopped");
        assert_eq!(body(r).await, mp4);
    }

    #[tokio::test]
    async fn the_zip_has_a_manifest_and_every_segment() {
        use std::io::Read;
        let c = clip(&[Some((100, 40)), None, Some((66, 40))]).await;
        let parts: Vec<Vec<u8>> = c.segments.iter().map(|s| s.mp4.clone()).collect();
        let starts: Vec<u64> = c.segments.iter().map(|s| s.start_ms).collect();
        let z = c.into_zip().unwrap();
        let mut a = zip::ZipArchive::new(std::io::Cursor::new(z)).unwrap();
        let read = |a: &mut zip::ZipArchive<_>, name: &str| {
            let mut v = Vec::new();
            a.by_name(name).unwrap().read_to_end(&mut v).unwrap();
            v
        };
        let m: serde_json::Value = serde_json::from_slice(&read(&mut a, "manifest.json")).unwrap();
        assert_eq!(m["end_reason"], "stopped");
        let segs = m["segments"].as_array().unwrap();
        assert_eq!(segs.len(), 2);
        for (i, s) in segs.iter().enumerate() {
            assert_eq!(s["start_ms"], starts[i]);
            assert_eq!(read(&mut a, s["file"].as_str().unwrap()), parts[i]);
        }
        assert_eq!(segs[0]["file"], "segment-1.mp4");
        assert_eq!(segs[0]["frames"], 2);
        assert_eq!(segs[1]["frames"], 1);
    }

    #[tokio::test]
    async fn a_bad_format_leaves_the_recording_running() {
        let r = Recorder::new(Hub::new(), Duration::from_secs(30), true);
        let started = start(State(r.clone())).await;
        let id = json(&body(started).await)["id"]
            .as_str()
            .unwrap()
            .to_string();
        let bad = stop(State(r.clone()), with(&[("id", &id), ("format", "avi")])).await;
        assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
        assert!(r.active.lock().contains_key(&id));
        let done = stop(State(r.clone()), with(&[("id", &id), ("format", "zip")])).await;
        assert_eq!(done.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(r.active.lock().is_empty());
    }

    fn json(b: &[u8]) -> serde_json::Value {
        serde_json::from_slice(b).unwrap()
    }

    /// A request with these query pairs (plain values, no escaping needed).
    fn with(pairs: &[(&str, &str)]) -> Request {
        let q: Vec<String> = pairs.iter().map(|(k, v)| format!("{k}={v}")).collect();
        Request::builder()
            .uri(format!("/r?{}", q.join("&")))
            .body(axum::body::Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn recording_is_off_unless_enabled_and_then_nothing_joins() {
        let r = Recorder::new(Hub::new(), Duration::from_secs(30), false);
        for res in [
            record(State(r.clone()), with(&[("seconds", "1")])).await,
            start(State(r.clone())).await,
            stop(State(r.clone()), with(&[("id", "1")])).await,
        ] {
            assert_eq!(res.status(), StatusCode::FORBIDDEN);
            let j = json(&body(res).await);
            assert_eq!(j["recording"], false);
            assert!(j["error"].as_str().unwrap().contains("--enable-recording"));
        }
        assert!(r.hub.clients.lock().is_empty(), "no viewer joined");
        assert!(!r.hub.taps.any(), "nothing takes the stream");
        assert!(r.active.lock().is_empty());
    }

    #[test]
    fn the_command_line_leaves_recording_off_unless_asked() {
        // As the server builds its recorder from them.
        let read = |v: &[&str]| {
            let a = crate::parse_args(v.iter().map(|s| s.to_string()));
            crate::recorder_from_args(&a, &Hub::new()).on
        };
        assert!(!read(&[]));
        assert!(!read(&["--port", "6901", "--read-only"]));
        assert!(!read(&["--enable-recording=false"]));
        assert!(read(&["--enable-recording"]));
        assert!(read(&["--enable-recording=true"]));
        assert!(read(&["--enable_recording", "1"]));
    }

    /// Cohort 1's encode thread hands out a frame of a `size` picture, as `encode_loop` does.
    fn encoded(hub: &Hub, key: Option<(u8, u8)>, size: (u32, u32)) {
        hub.taps.set_cohort(1);
        hub.taps
            .offer(1, size, key.is_some(), Instant::now(), &[message(key)]);
    }

    #[tokio::test]
    async fn only_the_id_a_recording_was_started_with_stops_it() {
        let r = Recorder::new(Hub::new(), Duration::from_secs(30), true);
        let id = json(&body(start(State(r.clone())).await).await)["id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(id.len(), 32);
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
        let mut other = id.clone().into_bytes();
        other[31] = if other[31] == b'0' { b'1' } else { b'0' };
        let other = String::from_utf8(other).unwrap();
        for wrong in ["1", "0", "", &id[..31], other.as_str()] {
            let res = stop(State(r.clone()), with(&[("id", wrong)])).await;
            assert_eq!(res.status(), StatusCode::NOT_FOUND, "{wrong:?}");
        }
        assert!(r.active.lock().contains_key(&id), "still running");
        for u in [Some((100, 40)), None, None] {
            encoded(&r.hub, u, (64, 32));
        }
        let res = stop(State(r.clone()), with(&[("id", &id)])).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()[header::CONTENT_TYPE], "video/mp4");
        assert!(r.active.lock().is_empty());
        let mp4 = body(res).await;
        assert_eq!(check(&mp4).0, 3, "the clip has the three frames");
        let res = stop(State(r.clone()), with(&[("id", &id)])).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "an id stops once");
    }

    #[tokio::test]
    async fn while_off_a_query_is_not_read() {
        let r = Recorder::new(Hub::new(), Duration::from_secs(30), false);
        for res in [
            record(
                State(r.clone()),
                with(&[("seconds", "x"), ("format", "avi")]),
            )
            .await,
            stop(State(r.clone()), with(&[("id", "1"), ("format", "avi")])).await,
        ] {
            assert_eq!(res.status(), StatusCode::FORBIDDEN);
            assert_eq!(body(res).await, OFF.as_bytes());
        }
    }

    #[tokio::test]
    async fn broadcasts_do_not_reach_a_recording() {
        let hub = Hub::new();
        let r = Recorder::new(hub.clone(), Duration::from_secs(30), true);
        let (done, tap) = begin(&r, Duration::from_secs(30)).ok().unwrap();
        encoded(&hub, Some((100, 40)), (800, 600));
        for text in [
            r#"{"t":"hello","w":1024,"h":600}"#,
            r#"{"t":"hello"}"#,
            r#"{"t":"clip","d":"hello"}"#,
        ] {
            hub.broadcast(|| crate::hub::Out::Text(text.into()));
        }
        encoded(&hub, None, (800, 600));
        tap.end(End::Stopped);
        let c = done.await.unwrap().unwrap();
        assert_eq!(c.end, End::Stopped);
        assert_eq!(c.segments[0].frames, 2);
    }

    #[tokio::test]
    async fn a_frame_of_another_size_ends_a_recording() {
        for other in [(1024, 600), (800, 768)] {
            let (parts, end) = collected(vec![
                sized(Some((100, 40)), (800, 600)),
                sized(None, (800, 600)),
                sized(Some((100, 40)), other),
                sized(None, other),
            ]);
            assert_eq!(end, End::Resized, "{other:?}");
            let c = Clip::write(parts, end).unwrap();
            assert_eq!(c.segments.len(), 1);
            assert_eq!(c.segments[0].frames, 2, "{other:?}");
            assert_eq!(dims(&c.segments[0].mp4), (800, 600));
        }
    }

    #[tokio::test]
    async fn a_stream_rebuilt_at_the_same_size_keeps_recording_from_its_keyframe() {
        // A resize the display refused rebuilds the capture at the size it had: a new cohort
        // whose frames the recording takes from its first keyframe on.
        let hub = Hub::new();
        let r = Recorder::new(hub.clone(), Duration::from_secs(30), true);
        let (done, tap) = begin(&r, Duration::from_secs(30)).ok().unwrap();
        encoded(&hub, Some((100, 40)), (800, 600));
        encoded(&hub, None, (800, 600));
        hub.taps.set_cohort(2);
        assert!(
            hub.taps.wants_key(),
            "the new cohort is asked for a keyframe"
        );
        for key in [None, Some((100, 40)), None] {
            hub.taps.offer(
                2,
                (800, 600),
                key.is_some(),
                Instant::now(),
                &[message(key)],
            );
        }
        tap.end(End::Stopped);
        let c = done.await.unwrap().unwrap();
        assert_eq!(c.end, End::Stopped);
        assert_eq!(c.segments.len(), 1);
        assert_eq!(c.segments[0].frames, 4, "not the delta before the keyframe");
    }

    /// The width and height an MP4's avc1 sample entry gives its samples.
    fn dims(mp4: &[u8]) -> (u16, u16) {
        let stsd = find(mp4, &[b"moov", b"trak", b"mdia", b"minf", b"stbl", b"stsd"]);
        assert_eq!(&stsd[12..16], b"avc1");
        let at = 16 + 24;
        (
            u16::from_be_bytes([stsd[at], stsd[at + 1]]),
            u16::from_be_bytes([stsd[at + 2], stsd[at + 3]]),
        )
    }

    /// Starts a recording while the hub says `hub_size` and the encoder still hands out
    /// pictures of `old`, then of `new`; returns the clip.
    async fn across_a_resize(hub_size: (u32, u32), old: (u32, u32), new: (u32, u32)) -> Clip {
        let hub = Hub::new();
        hub.width.store(hub_size.0, Ordering::Relaxed);
        hub.height.store(hub_size.1, Ordering::Relaxed);
        let r = Recorder::new(hub.clone(), Duration::from_secs(30), true);
        let (done, tap) = begin(&r, Duration::from_secs(30)).ok().unwrap();
        encoded(&hub, Some((100, 40)), old);
        encoded(&hub, None, old);
        encoded(&hub, Some((100, 40)), new);
        encoded(&hub, None, new);
        tap.end(End::Stopped);
        done.await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn a_resize_as_a_recording_starts_gives_it_the_size_of_the_frames_it_records() {
        // The rebuild has set the hub's new size while the old encoder's frames still come:
        // the clip is the old size, with only those frames, and the new size ends it.
        let c = across_a_resize((1024, 768), (800, 600), (1024, 768)).await;
        assert_eq!(c.end, End::Resized);
        assert_eq!(c.segments.len(), 1);
        assert_eq!(c.segments[0].frames, 2, "only the old-size frames");
        assert_eq!(dims(&c.segments[0].mp4), (800, 600));
        // The hub's size not yet updated while the new encoder's frames come already.
        let c = across_a_resize((800, 600), (1024, 768), (1024, 768)).await;
        assert_eq!(c.end, End::Stopped);
        assert_eq!(c.segments[0].frames, 4);
        assert_eq!(dims(&c.segments[0].mp4), (1024, 768));
    }
}
