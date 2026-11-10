//! Screenshots and recordings over HTTP, for agents and scripts:
//!
//! - `GET /screenshot`: the screen as a PNG.
//! - `GET /record?seconds=N`: records N seconds, returns an MP4.
//! - `POST /record/start` -> `{"id": n}`, then `POST /record/stop?id=n`: returns an MP4
//!   of everything in between.
//!
//! Another encoder taking over mid-recording (new SPS/PPS) starts a new MP4 segment at
//! its keyframe. `format=zip` on `/record` or `/record/stop` returns `manifest.json` and
//! every `segment-k.mp4`; without it the response is the first segment, as an MP4, and
//! `x-voxl-segments` and `x-voxl-record-end` say when there was more.
//!
//! A recording is the H.264 stream voxl already encodes, rewrapped as MP4: the recorder
//! joins as one more viewer that acknowledges frames at once, so it costs almost no
//! CPU and never slows real viewers down. Like the stream, it holds frames only when
//! the screen changes, and it doesn't include the cursor (viewers draw it locally).

use crate::hub::{Client, Hub, Out};
use crate::mp4;
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

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
    active: Mutex<HashMap<u64, Active>>,
    next: AtomicU64,
}

struct Active {
    stop: oneshot::Sender<()>,
    done: Recording,
}

type Recording = tokio::task::JoinHandle<Result<Clip, String>>;

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
    pub fn new(hub: Arc<Hub>, max: Duration) -> Arc<Self> {
        Arc::new(Recorder {
            hub,
            max,
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
            res.headers_mut()
                .insert("x-voxl-seq", header::HeaderValue::from(seq));
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
        res.headers_mut()
            .insert("x-voxl-segments", header::HeaderValue::from(n));
        res.headers_mut().insert(
            "x-voxl-record-end",
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

/// Joins as a viewer and collects frames until `stop`, `max` or a geometry change.
#[allow(clippy::result_large_err)]
fn begin(
    r: &Arc<Recorder>,
    stop: oneshot::Receiver<()>,
    max: Duration,
) -> Result<Recording, Response> {
    if r.hub.stripes.lock().len() > 1 {
        return Err(err(
            StatusCode::CONFLICT,
            "recording needs a single-stripe stream (--stripes 1)",
        ));
    }
    let (tx, rx) = mpsc::unbounded_channel();
    let client = r.hub.add_client(tx);
    // A protocol 1 viewer: the H.264 stream the MP4 writer takes.
    client.set_video(true);
    r.hub.replan();
    let hub = r.hub.clone();
    Ok(tokio::spawn(async move {
        let (parts, end) = collect(&hub, &client, rx, stop, max).await;
        // Writing takes a while: nothing acknowledges frames now, so leave first.
        hub.remove_client(client.id);
        tokio::task::spawn_blocking(move || Clip::write(parts, end))
            .await
            .map_err(|e| e.to_string())?
    }))
}

async fn collect(
    hub: &Hub,
    client: &Client,
    mut rx: mpsc::UnboundedReceiver<Out>,
    mut stop: oneshot::Receiver<()>,
    max: Duration,
) -> (Vec<Part>, End) {
    let start = Instant::now();
    let deadline = tokio::time::sleep(max);
    tokio::pin!(deadline);
    let mut parts: Vec<Part> = Vec::new();
    let mut frames: Vec<mp4::Frame> = Vec::new();
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
                                    frames: std::mem::take(&mut frames),
                                    size,
                                    end_ms: ms,
                                });
                                first = Some(p);
                            }
                            Some(_) => {}
                        }
                    }
                    if frames.is_empty() {
                        size = (hub.width.load(Ordering::Relaxed), hub.height.load(Ordering::Relaxed));
                    }
                    last_ms = Some(ms);
                    frames.push(mp4::Frame {
                        ms,
                        key,
                        data: b.slice(16..),
                    });
                }
                // A new size restarts the stream; an MP4 track keeps one size, so stop.
                Some(Out::Text(t)) if !frames.is_empty() && t.contains("\"hello\"") => {
                    break End::Resized
                }
                _ => {}
            },
        }
    };
    parts.push(Part {
        frames,
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

pub async fn record(
    State(r): State<Arc<Recorder>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let secs: f64 = q.get("seconds").and_then(|s| s.parse().ok()).unwrap_or(5.0);
    let Some(len) = record_len(secs, r.max) else {
        return err(StatusCode::BAD_REQUEST, "seconds must be positive");
    };
    let Some(zip) = wants_zip(&q) else {
        return err(StatusCode::BAD_REQUEST, BAD_FORMAT);
    };
    // The deadline ends it; the stop side is never used.
    let (_keep, stop) = oneshot::channel();
    let done = match begin(&r, stop, len) {
        Ok(d) => d,
        Err(e) => return e,
    };
    respond(done, zip, "/record", None).await
}

pub async fn start(State(r): State<Arc<Recorder>>) -> Response {
    let (stop_tx, stop) = oneshot::channel();
    let done = match begin(&r, stop, r.max) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let id = r.next.fetch_add(1, Ordering::Relaxed);
    r.active.lock().insert(
        id,
        Active {
            stop: stop_tx,
            done,
        },
    );
    log::info!("recording {id} started");
    // Nobody may ever stop it: once it has ended on its own, drop it after a grace period
    // instead of holding the clip in memory forever.
    let rec = r.clone();
    tokio::spawn(async move {
        tokio::time::sleep(rec.max + UNCLAIMED).await;
        if rec.active.lock().remove(&id).is_some() {
            log::info!("recording {id} was never stopped; discarded");
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
    let Some(zip) = wants_zip(&q) else {
        return err(StatusCode::BAD_REQUEST, BAD_FORMAT);
    };
    let id = q.get("id").and_then(|s| s.parse::<u64>().ok());
    let Some(a) = id.and_then(|id| r.active.lock().remove(&id)) else {
        return err(StatusCode::NOT_FOUND, "no such recording");
    };
    let _ = a.stop.send(());
    log::info!("recording {} stopped", id.unwrap_or(0));
    respond(a.done, zip, "/record/stop", id).await
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

    fn unit(key: Option<(u8, u8)>) -> Out {
        let mut b = vec![0x01, key.is_some() as u8, 0, 0, 0, 1];
        b.resize(16, 0);
        match key {
            Some((profile, level)) => b.extend_from_slice(&[
                0, 0, 0, 1, 0x67, profile, 0, level, 0xac, 0, 0, 0, 1, 0x68, 0xee, 0, 0, 0, 1,
                0x65, 0xaa,
            ]),
            None => b.extend_from_slice(&[0, 0, 0, 1, 0x41, 0xbb]),
        }
        Out::Bin(bytes::Bytes::from(b))
    }

    /// Queues `units` and a size change, which ends the recording in queue order.
    async fn clip(units: &[Option<(u8, u8)>]) -> Clip {
        let hub = Hub::new();
        let (tx, rx) = mpsc::unbounded_channel();
        let client = hub.add_client(tx.clone());
        let (_keep, stop) = oneshot::channel();
        for u in units {
            tx.send(unit(*u)).unwrap();
        }
        tx.send(Out::Text("{\"t\":\"hello\"}".into())).unwrap();
        let (parts, end) = tokio::time::timeout(
            Duration::from_secs(5),
            collect(&hub, &client, rx, stop, Duration::from_secs(30)),
        )
        .await
        .expect("the recording ended");
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
        assert_eq!(c.end, End::Resized);
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
        let hub = Hub::new();
        let (tx, rx) = mpsc::unbounded_channel();
        let client = hub.add_client(tx.clone());
        let (_keep, stop) = oneshot::channel();
        tx.send(unit(Some((100, 40)))).unwrap();
        let mut b = vec![0x01, 1, 0, 0, 0, 2];
        b.resize(16, 0);
        b.extend_from_slice(&[0, 0, 0, 1, 0x68, 0x11, 0, 0, 0, 1, 0x65, 0xaa]);
        tx.send(Out::Bin(bytes::Bytes::from(b))).unwrap();
        let (parts, end) = collect(&hub, &client, rx, stop, Duration::from_secs(30)).await;
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
        let Out::Bin(b) = unit(Some((100, 40))) else {
            unreachable!()
        };
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
        assert_eq!(r.headers()["x-voxl-segments"], "2");
        assert_eq!(r.headers()["x-voxl-record-end"], "segment_lost");
    }

    #[test]
    fn a_recording_with_no_segment_left_is_an_error() {
        let e = Clip::write(vec![part(Vec::new())], End::Stopped).err();
        assert_eq!(e.as_deref(), Some("no frames were recorded"));
    }

    #[tokio::test]
    async fn the_recorder_leaves_the_hub_when_it_ends() {
        let r = Recorder::new(Hub::new(), Duration::from_secs(30));
        let (stop_tx, stop) = oneshot::channel();
        let done = begin(&r, stop, Duration::from_secs(30)).ok().unwrap();
        assert_eq!(r.hub.clients.lock().len(), 1);
        stop_tx.send(()).unwrap();
        assert!(done.await.unwrap().is_err(), "no frames were recorded");
        assert!(r.hub.clients.lock().is_empty());
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
        assert!(r.headers().get("x-voxl-segments").is_none());
        assert!(r.headers().get("x-voxl-record-end").is_none());
        assert_eq!(body(r).await, mp4);
    }

    #[tokio::test]
    async fn without_zip_several_segments_are_the_first_and_headers() {
        let c = clip(&[Some((100, 40)), Some((66, 40))]).await;
        let mp4 = c.segments[0].mp4.clone();
        let r = first_segment(c);
        assert_eq!(r.headers()["x-voxl-segments"], "2");
        assert_eq!(r.headers()["x-voxl-record-end"], "resized");
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
        assert_eq!(m["end_reason"], "resized");
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
        let r = Recorder::new(Hub::new(), Duration::from_secs(30));
        let started = start(State(r.clone())).await;
        let id = serde_json::from_slice::<serde_json::Value>(&body(started).await).unwrap()["id"]
            .as_u64()
            .unwrap();
        let q = |f: &str| {
            Query(HashMap::from([
                ("id".to_string(), id.to_string()),
                ("format".to_string(), f.to_string()),
            ]))
        };
        let bad = stop(State(r.clone()), q("avi")).await;
        assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
        assert!(r.active.lock().contains_key(&id));
        let done = stop(State(r.clone()), q("zip")).await;
        assert_eq!(done.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(r.active.lock().is_empty());
    }
}
