use super::*;
use std::sync::{Condvar, Mutex as StdMutex};
use tap::Seen;

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

/// A directory on disk for the tests' spools (their files have no name there).
fn dir() -> PathBuf {
    spool::default_dir()
}

/// `limits` with no free-space floor: the test machine's disk is not under test.
fn no_floor(limits: Limits) -> Limits {
    Limits {
        floor: Arc::new(|_| 0),
        ..limits
    }
}

fn recorder(hub: &Arc<Hub>, limits: Limits) -> Arc<Recorder> {
    Recorder::with_limits(
        hub.clone(),
        Duration::from_secs(30),
        true,
        dir(),
        no_floor(limits),
    )
}

/// A protocol 1 message: a keyframe with SPS (`profile`, `level`) and PPS, or a delta.
fn message(key: Option<(u8, u8)>) -> Bytes {
    let mut b = vec![0x01, key.is_some() as u8, 0, 0, 0, 1];
    b.resize(16, 0);
    match key {
        Some((profile, level)) => b.extend_from_slice(&[
            0, 0, 0, 1, 0x67, profile, 0, level, 0xac, 0, 0, 0, 1, 0x68, 0xee, 0, 0, 0, 1, 0x65,
            0xaa,
        ]),
        None => b.extend_from_slice(&[0, 0, 0, 1, 0x41, 0xbb]),
    }
    Bytes::from(b)
}

/// A message of `len` bytes: a keyframe (SPS, PPS, a slice) or a delta, filled out.
fn large(key: bool, len: usize) -> Bytes {
    let mut b = vec![0x01, key as u8, 0, 0, 0, 1];
    b.resize(16, 0);
    if key {
        b.extend_from_slice(&[
            0, 0, 0, 1, 0x67, 100, 0, 40, 0xac, 0, 0, 0, 1, 0x68, 0xee, 0, 0, 0, 1, 0x65,
        ]);
    } else {
        b.extend_from_slice(&[0, 0, 0, 1, 0x41]);
    }
    b.resize(len, 0xaa);
    Bytes::from(b)
}

fn frame_at(key: Option<(u8, u8)>, size: (u32, u32), at: Instant) -> Msg {
    Msg::Frame(Tapped {
        size,
        key: key.is_some(),
        captured_at: at,
        data: message(key),
        charge: 0,
    })
}

fn sized(key: Option<(u8, u8)>, size: (u32, u32)) -> Msg {
    frame_at(key, size, Instant::now())
}

/// A tapped frame of a 64x32 picture.
fn unit(key: Option<(u8, u8)>) -> Msg {
    sized(key, (64, 32))
}

fn stopped() -> Msg {
    Msg::End(End::Stopped, Instant::now())
}

/// A writer of a 30 s recording on a spool with nothing reserved.
fn writer(limits: Limits) -> Writer {
    writer_for(limits, Duration::from_secs(30))
}

fn writer_for(limits: Limits, max: Duration) -> Writer {
    Writer::new(
        Spool::bare(spool::anonymous(&dir()).unwrap()),
        dir(),
        no_floor(limits),
        max,
        Arc::new(Busy::new()),
    )
}

/// Records `msgs` in order under `limits`, until one ends the recording.
fn collected_with(msgs: Vec<Msg>, limits: Limits) -> Outcome {
    let (tx, rx) = mpsc::channel();
    for m in msgs {
        tx.send(m).unwrap();
    }
    run(rx, writer(limits), |_| {})
}

fn collected(msgs: Vec<Msg>) -> Clip {
    collected_with(msgs, Limits::default()).unwrap()
}

/// Records `units`, then a stop.
fn clip(units: &[Option<(u8, u8)>]) -> Clip {
    let mut msgs: Vec<Msg> = units.iter().map(|u| unit(*u)).collect();
    msgs.push(stopped());
    collected(msgs)
}

/// Segment `i` of `c` as the answer writes it.
fn mp4_of(c: &Clip, i: usize) -> Vec<u8> {
    let mut out = Vec::new();
    copy_segment(c.spool.file(), &c.segments[i], &mut out).unwrap();
    out
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

#[test]
fn another_encoders_keyframe_starts_a_segment() {
    let c = clip(&[
        Some((100, 40)),
        None,
        Some((100, 41)),
        None,
        Some((66, 41)),
        None,
    ]);
    assert_eq!(c.end, End::Stopped);
    assert_eq!(c.segments.len(), 2);
    let (a, b) = (&c.segments[0], &c.segments[1]);
    let (samples, total, sps, pps) = check(&mp4_of(&c, 0));
    assert_eq!(samples, 4, "a new level continues");
    assert_eq!(a.frames, 4);
    assert_eq!(total as u64, a.duration_ms);
    assert_eq!(sps, [0x67, 100, 0, 40, 0xac]);
    assert_eq!(pps, [0x68, 0xee]);
    let (samples, total, sps, pps) = check(&mp4_of(&c, 1));
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

#[test]
fn a_new_level_keeps_one_segment() {
    let c = clip(&[Some((100, 40)), None, Some((100, 41)), None]);
    assert_eq!(c.segments.len(), 1);
    assert_eq!(check(&mp4_of(&c, 0)).0, 4);
}

#[test]
fn a_flapping_encoder_ends_at_the_segment_cap() {
    let units: Vec<_> = (0..MAX_SEGMENTS + 5)
        .flat_map(|i| [Some((if i % 2 == 0 { 100 } else { 66 }, 40)), None])
        .collect();
    let c = clip(&units);
    assert_eq!(c.end, End::SegmentCap);
    assert_eq!(c.segments.len(), MAX_SEGMENTS);
    for i in 0..MAX_SEGMENTS {
        assert_eq!(check(&mp4_of(&c, i)).0, 2);
    }
}

#[test]
fn a_keyframe_that_cant_start_a_segment_ends_the_recording() {
    let mut b = vec![0x01, 1, 0, 0, 0, 2];
    b.resize(16, 0);
    b.extend_from_slice(&[0, 0, 0, 1, 0x68, 0x11, 0, 0, 0, 1, 0x65, 0xaa]);
    let bad = Msg::Frame(Tapped {
        size: (64, 32),
        key: true,
        captured_at: Instant::now(),
        data: Bytes::from(b),
        charge: 0,
    });
    let c = collected(vec![unit(Some((100, 40))), bad]);
    assert_eq!(c.end, End::BadParameterSets);
    assert_eq!(c.segments.len(), 1);
    assert_eq!(check(&mp4_of(&c, 0)).0, 1);
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

#[tokio::test]
async fn a_segment_that_cant_be_written_leaves_the_others() {
    let mut file = spool::anonymous(&dir()).unwrap();
    let sample = [0, 0, 0, 2, 0x65, 0xaa];
    for _ in 0..3 {
        file.write_all(&sample).unwrap();
    }
    let index: Vec<mp4::Sample> = (0..3)
        .map(|i| mp4::Sample {
            ms: i * 10,
            key: true,
            size: 6,
        })
        .collect();
    let key = &message(Some((100, 40)))[16..];
    let parts = [
        Part::at(0, key),
        Part {
            from: 1,
            params: None,
            end_ms: Some(20),
        },
        Part::at(2, key),
    ];
    let c = Clip::build(
        &index,
        3,
        &parts,
        (64, 32),
        End::Stopped,
        100,
        Spool::bare(file),
    )
    .unwrap();
    assert_eq!(c.end, End::SegmentLost);
    assert_eq!(c.segments.len(), 2);
    assert_eq!(c.segments[1].start, 12, "past the lost segment's sample");
    assert_eq!(check(&mp4_of(&c, 1)).0, 1);
    let r = first_segment(c, IDLE);
    assert_eq!(r.headers()["x-starfold-segments"], "2");
    assert_eq!(r.headers()["x-starfold-record-end"], "segment_lost");
    assert_eq!(r.headers()["x-voxl-segments"], "2");
    assert_eq!(r.headers()["x-voxl-record-end"], "segment_lost");
}

#[test]
fn a_recording_with_no_segment_left_is_an_error() {
    let file = spool::anonymous(&dir()).unwrap();
    let e = Clip::build(&[], 0, &[], (64, 32), End::Stopped, 100, Spool::bare(file))
        .err()
        .unwrap();
    assert_eq!(e.why, "no frames were recorded");
    assert_eq!(e.end, End::Stopped);
}

/// Stops a running recording; its outcome.
async fn stop_rec(rec: Rec) -> Outcome {
    rec.tap.end(End::Stopped, Instant::now());
    rec.outcome().await
}

#[tokio::test]
async fn a_recording_is_beside_the_viewers_and_leaves_when_it_ends() {
    let r = recorder(&Hub::new(), Limits::default());
    let rec = begin(&r, Duration::from_secs(30)).await.ok().unwrap();
    assert!(r.hub.taps.any());
    assert!(r.hub.clients.lock().is_empty(), "no viewer joined");
    assert!(stop_rec(rec).await.is_err(), "no frames were recorded");
    assert!(!r.hub.taps.any());
    assert_eq!(r.running.load(Ordering::Acquire), 0);
}

async fn body(r: Response) -> Vec<u8> {
    axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec()
}

#[tokio::test]
async fn one_segment_is_a_plain_mp4_that_says_why_it_ended() {
    let c = clip(&[Some((100, 40)), None]);
    let mp4 = mp4_of(&c, 0);
    let r = first_segment(c, IDLE);
    assert_eq!(r.headers()[header::CONTENT_TYPE], "video/mp4");
    assert_eq!(r.headers()[header::CONTENT_LENGTH], mp4.len().to_string());
    assert_eq!(r.headers()["x-starfold-segments"], "1");
    assert_eq!(r.headers()["x-starfold-record-end"], "stopped");
    assert_eq!(body(r).await, mp4);
}

#[tokio::test]
async fn a_one_segment_clip_cut_by_a_limit_says_so() {
    let c = collected_with(
        vec![unit(Some((100, 40))), unit(None), unit(None)],
        Limits {
            max_bytes: 6 + 6,
            ..Limits::default()
        },
    )
    .unwrap();
    let r = first_segment(c, IDLE);
    assert_eq!(r.headers()["x-starfold-segments"], "1");
    assert_eq!(r.headers()["x-starfold-record-end"], "size_limit");
}

#[tokio::test]
async fn without_zip_several_segments_are_the_first_and_headers() {
    let c = clip(&[Some((100, 40)), Some((66, 40))]);
    let mp4 = mp4_of(&c, 0);
    let r = first_segment(c, IDLE);
    assert_eq!(r.headers()["x-starfold-segments"], "2");
    assert_eq!(r.headers()["x-starfold-record-end"], "stopped");
    assert_eq!(body(r).await, mp4);
}

#[test]
fn the_zip_has_a_manifest_and_every_segment() {
    use std::io::Read;
    let c = clip(&[Some((100, 40)), None, Some((66, 40))]);
    let parts: Vec<Vec<u8>> = (0..2).map(|i| mp4_of(&c, i)).collect();
    let starts: Vec<u64> = c.segments.iter().map(|s| s.start_ms).collect();
    let mut z = Vec::new();
    write_zip(&c.segments, c.end, c.spool.file(), &mut z).unwrap();
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
    let r = recorder(&Hub::new(), Limits::default());
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
    assert_eq!(done.headers()["x-starfold-record-end"], "stopped");
    let j = json(&body(done).await);
    assert_eq!(j["error"], "no frames were recorded");
    assert_eq!(j["end_reason"], "stopped");
    assert!(r.active.lock().is_empty());
}

#[tokio::test]
async fn a_refused_first_keyframe_says_why() {
    let hub = Hub::new();
    let r = recorder(&hub, Limits::default());
    let id = started(&r).await.unwrap();
    hand_out(&hub, true, large(true, tap::LARGEST_FRAME + 1), (64, 32));
    let res = stop(State(r.clone()), with(&[("id", &id)])).await;
    assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(res.headers()["x-starfold-record-end"], "large_frame");
    assert_eq!(json(&body(res).await)["end_reason"], "large_frame");
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
    let r = Recorder::new(Hub::new(), Duration::from_secs(30), false, dir());
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
        crate::recorder_from_args(&a, &Hub::new()).unwrap().on
    };
    assert!(!read(&[]));
    assert!(!read(&["--port", "6901", "--read-only"]));
    assert!(!read(&["--enable-recording=false"]));
    assert!(read(&["--enable-recording"]));
    assert!(read(&["--enable-recording=true"]));
    assert!(read(&["--enable_recording", "1"]));
}

#[test]
fn the_longest_recording_is_bounded_at_startup() {
    let settings = |v: &[&str]| settings(&crate::parse_args(v.iter().map(|s| s.to_string())));
    let on = |secs: &str| settings(&["--enable-recording", "--max-record-seconds", secs]);
    let top = MAX_RECORD_SECONDS.to_string();
    assert_eq!(
        on(&top).unwrap().max,
        Duration::from_secs(MAX_RECORD_SECONDS)
    );
    assert_eq!(on("300").unwrap().max, Duration::from_secs(300));
    for bad in ["601", "3600", "10000000000000000", "1e300", "inf", "0"] {
        let e = on(bad).err().unwrap_or_else(|| panic!("{bad} was taken"));
        assert!(e.contains("--max-record-seconds"), "{bad}: {e}");
    }
    // Off, the flag can't start a recording; it doesn't stop the server.
    let off = settings(&["--max-record-seconds", "86400"]).unwrap();
    assert!(!off.on);
    assert_eq!(off.max, Duration::from_secs(MAX_RECORD_SECONDS));
    // The index at the bound: 16 bytes a frame, 120 frames a second.
    assert_eq!(16 * MAX_FPS * MAX_RECORD_SECONDS, 1_152_000);
}

/// Cohort 1's encode thread hands out a frame of a `size` picture, as `encode_loop` does.
fn encoded(hub: &Hub, key: Option<(u8, u8)>, size: (u32, u32)) {
    hand_out(hub, key.is_some(), message(key), size);
}

fn hand_out(hub: &Hub, key: bool, msg: Bytes, size: (u32, u32)) {
    hand_out_at(hub, key, msg, size, Instant::now());
}

fn hand_out_at(hub: &Hub, key: bool, msg: Bytes, size: (u32, u32), at: Instant) {
    hub.taps.set_cohort(1);
    hub.taps
        .offer(&mut Seen::default(), 1, size, key, at, &[msg]);
}

#[tokio::test]
async fn only_the_id_a_recording_was_started_with_stops_it() {
    let r = recorder(&Hub::new(), Limits::default());
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
    let r = Recorder::new(Hub::new(), Duration::from_secs(30), false, dir());
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
    let r = recorder(&hub, Limits::default());
    let rec = begin(&r, Duration::from_secs(30)).await.ok().unwrap();
    encoded(&hub, Some((100, 40)), (800, 600));
    for text in [
        r#"{"t":"hello","w":1024,"h":600}"#,
        r#"{"t":"hello"}"#,
        r#"{"t":"clip","d":"hello"}"#,
    ] {
        hub.broadcast(|| crate::hub::Out::Text(text.into()));
    }
    encoded(&hub, None, (800, 600));
    let c = stop_rec(rec).await.unwrap();
    assert_eq!(c.end, End::Stopped);
    assert_eq!(c.segments[0].frames, 2);
}

#[test]
fn a_frame_of_another_size_ends_a_recording() {
    for other in [(1024, 600), (800, 768)] {
        let c = collected(vec![
            sized(Some((100, 40)), (800, 600)),
            sized(None, (800, 600)),
            sized(Some((100, 40)), other),
            sized(None, other),
        ]);
        assert_eq!(c.end, End::Resized, "{other:?}");
        assert_eq!(c.segments.len(), 1);
        assert_eq!(c.segments[0].frames, 2, "{other:?}");
        assert_eq!(dims(&mp4_of(&c, 0)), (800, 600));
    }
}

#[tokio::test]
async fn a_stream_rebuilt_at_the_same_size_keeps_recording_from_its_keyframe() {
    // A resize the display refused rebuilds the capture at the size it had: a new cohort
    // whose frames the recording takes from its first keyframe on.
    let hub = Hub::new();
    let r = recorder(&hub, Limits::default());
    let rec = begin(&r, Duration::from_secs(30)).await.ok().unwrap();
    encoded(&hub, Some((100, 40)), (800, 600));
    encoded(&hub, None, (800, 600));
    hub.taps.set_cohort(2);
    assert!(
        hub.taps.wants_key(),
        "the new cohort is asked for a keyframe"
    );
    let mut seen = Seen::default();
    for key in [None, Some((100, 40)), None] {
        hub.taps.offer(
            &mut seen,
            2,
            (800, 600),
            key.is_some(),
            Instant::now(),
            &[message(key)],
        );
    }
    assert!(!hub.taps.wants_key());
    let c = stop_rec(rec).await.unwrap();
    assert_eq!(c.end, End::Stopped);
    assert_eq!(c.segments.len(), 1);
    assert_eq!(c.segments[0].frames, 4, "not the delta before the keyframe");
}

/// Starts a recording while the hub says `hub_size` and the encoder still hands out
/// pictures of `old`, then of `new`; returns the clip.
async fn across_a_resize(hub_size: (u32, u32), old: (u32, u32), new: (u32, u32)) -> Clip {
    let hub = Hub::new();
    hub.width.store(hub_size.0, Ordering::Relaxed);
    hub.height.store(hub_size.1, Ordering::Relaxed);
    let r = recorder(&hub, Limits::default());
    let rec = begin(&r, Duration::from_secs(30)).await.ok().unwrap();
    encoded(&hub, Some((100, 40)), old);
    encoded(&hub, None, old);
    encoded(&hub, Some((100, 40)), new);
    encoded(&hub, None, new);
    stop_rec(rec).await.unwrap()
}

#[tokio::test]
async fn a_resize_as_a_recording_starts_gives_it_the_size_of_the_frames_it_records() {
    // The rebuild has set the hub's new size while the old encoder's frames still come:
    // the clip is the old size, with only those frames, and the new size ends it.
    let c = across_a_resize((1024, 768), (800, 600), (1024, 768)).await;
    assert_eq!(c.end, End::Resized);
    assert_eq!(c.segments.len(), 1);
    assert_eq!(c.segments[0].frames, 2, "only the old-size frames");
    assert_eq!(dims(&mp4_of(&c, 0)), (800, 600));
    // The hub's size not yet updated while the new encoder's frames come already.
    let c = across_a_resize((800, 600), (1024, 768), (1024, 768)).await;
    assert_eq!(c.end, End::Stopped);
    assert_eq!(c.segments[0].frames, 4);
    assert_eq!(dims(&mp4_of(&c, 0)), (1024, 768));
}

#[tokio::test]
async fn a_stream_split_into_stripes_ends_a_recording_closed_and_refuses_a_start() {
    let hub = Hub::new();
    let r = recorder(&hub, Limits::default());
    let id = started(&r).await.unwrap();
    encoded(&hub, Some((100, 40)), (64, 32));
    // The encoder split the picture (a resize past what one stripe takes): two messages a
    // frame from here on.
    *hub.stripes.lock() = vec![(0, 16), (16, 16)];
    let two = [message(Some((100, 40))), message(Some((100, 40)))];
    hub.taps.offer(
        &mut Seen::default(),
        1,
        (64, 32),
        true,
        Instant::now(),
        &two,
    );
    assert!(!hub.taps.any(), "it stopped taking the stream at once");
    let res = stop(State(r.clone()), with(&[("id", &id)])).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()["x-starfold-record-end"], "closed");
    drop(res);
    assert!(until(|| r.running.load(Ordering::Acquire) == 0).await);
    assert_eq!(started(&r).await, Err(StatusCode::CONFLICT));
}

/// Holds whoever waits on it until opened.
#[derive(Default)]
struct Gate {
    open: StdMutex<bool>,
    cv: Condvar,
}

impl Gate {
    fn set(&self, open: bool) {
        *self.open.lock().unwrap() = open;
        self.cv.notify_all();
    }

    fn wait(&self) {
        let mut g = self.open.lock().unwrap();
        while !*g {
            g = self.cv.wait(g).unwrap();
        }
    }
}

/// Limits whose writer waits at `gate` before each sample.
fn gated(gate: &Arc<Gate>) -> Limits {
    let g = gate.clone();
    Limits {
        before_write: Some(Arc::new(move |_| {
            g.wait();
            Ok(())
        })),
        ..Limits::default()
    }
}

#[tokio::test]
async fn a_disk_that_falls_behind_ends_the_recording_and_its_queue_stays_in_budget() {
    let hub = Hub::new();
    let gate = Arc::new(Gate::default());
    let r = recorder(&hub, gated(&gate));
    let rec = begin(&r, Duration::from_secs(30)).await.ok().unwrap();
    let tap = rec.tap.clone();
    hand_out(&hub, true, large(true, 64 << 10), (64, 32));
    let mut handed = 1;
    while !tap.is_closed() {
        hand_out(&hub, false, large(false, 256 << 10), (64, 32));
        assert!(tap.queued() <= tap::QUEUE, "{}", tap.queued());
        handed += 1;
        assert!(handed < 100, "the queue never filled");
    }
    assert!(!hub.taps.any(), "a closed tap is no demand on the stream");
    gate.set(true);
    let c = rec.outcome().await.unwrap();
    assert_eq!(c.end, End::DiskSlow);
    assert_eq!(c.segments[0].frames, handed - 1, "every frame it took");
    assert_eq!(tap.queued(), 0);
}

#[test]
fn a_queue_takes_frames_up_to_exactly_its_budget() {
    let hub = Hub::new();
    let (tap, rx) = hub.taps.add();
    let each = tap::QUEUE / 4;
    for i in 0..4 {
        hand_out(&hub, i == 0, large(i == 0, each - tap::OVERHEAD), (64, 32));
    }
    assert_eq!(tap.queued(), tap::QUEUE);
    assert!(
        !tap.is_closed(),
        "a queue exactly full is within its budget"
    );
    assert_eq!(rx.try_iter().count(), 4);
    hand_out(&hub, false, large(false, 17), (64, 32));
    assert!(tap.is_closed(), "one byte past it is not");
}

#[test]
fn an_encode_thread_never_waits_on_the_tap_list() {
    let hub = Hub::new();
    let (tap, rx) = hub.taps.add();
    let mut seen = Seen::default();
    hub.taps.set_cohort(1);
    let key = message(Some((100, 40)));
    hub.taps.offer(
        &mut seen,
        1,
        (64, 32),
        true,
        Instant::now(),
        std::slice::from_ref(&key),
    );
    // A start changes the list and holds its lock while the encoder hands out frames.
    let (_late, late_rx) = hub.taps.add();
    let held = hub.taps.hold_list();
    let (done_tx, done) = std::sync::mpsc::channel();
    let h = hub.clone();
    std::thread::spawn(move || {
        h.taps.offer(
            &mut seen,
            1,
            (64, 32),
            true,
            Instant::now(),
            &[message(Some((100, 40)))],
        );
        let _ = done_tx.send(seen);
    });
    let mut seen = done
        .recv_timeout(Duration::from_secs(5))
        .expect("the encode thread waited on the list");
    assert!(
        late_rx.try_recv().is_err(),
        "the new tap is not in its copy yet"
    );
    assert!(hub.taps.wants_key(), "so its keyframe is still owed");
    drop(held);
    hub.taps
        .offer(&mut seen, 1, (64, 32), true, Instant::now(), &[key]);
    assert!(matches!(late_rx.try_recv(), Ok(Msg::Frame(f)) if f.key));
    assert!(!hub.taps.wants_key());
    assert_eq!(rx.try_iter().count(), 3);
    drop(tap);
}

#[test]
fn one_large_frame_is_taken_only_into_an_empty_queue() {
    let ends = |frames: &[(bool, usize)]| {
        let hub = Hub::new();
        let (tap, rx) = hub.taps.add();
        for &(key, len) in frames {
            hand_out(&hub, key, large(key, len), (64, 32));
        }
        let got: Vec<Option<End>> = rx
            .try_iter()
            .map(|m| match m {
                Msg::Frame(_) => None,
                Msg::End(e, _) => Some(e),
            })
            .collect();
        (got, tap.queued())
    };
    let big = tap::LARGEST_FRAME - tap::OVERHEAD;
    let (got, queued) = ends(&[(true, big)]);
    assert_eq!(got, [None], "into an empty queue");
    assert_eq!(queued, tap::LARGEST_FRAME);
    let (got, _) = ends(&[(true, big), (false, 100)]);
    assert_eq!(
        got,
        [None, Some(End::DiskSlow)],
        "nothing after it until written"
    );
    let (got, _) = ends(&[(true, 100), (false, tap::QUEUE)]);
    assert_eq!(
        got,
        [None, Some(End::DiskSlow)],
        "not into a queue that holds one"
    );
    let (got, queued) = ends(&[(true, big + 1)]);
    assert_eq!(got, [Some(End::LargeFrame)]);
    assert_eq!(queued, 0, "never queued");
}

#[test]
fn a_partial_write_keeps_the_complete_samples() {
    let mut w = writer(Limits::default());
    for m in [unit(Some((100, 40))), unit(None), unit(None)] {
        let Msg::Frame(f) = m else { unreachable!() };
        w.frame(&f).unwrap();
    }
    w.out.flush().unwrap();
    // The third sample only partly reached the file.
    let file = w.out.get_ref().file();
    let len = file.metadata().unwrap().len();
    file.set_len(len - 2).unwrap();
    let c = w.finish(End::DiskError, 100).unwrap();
    assert_eq!(c.end, End::DiskError);
    assert_eq!(c.segments[0].frames, 2);
    assert_eq!(check(&mp4_of(&c, 0)).0, 2);
}

#[tokio::test]
async fn a_write_error_ends_a_recording_at_once_on_a_still_screen() {
    let hub = Hub::new();
    let limits = Limits {
        before_write: Some(Arc::new(|written| match written {
            0 => Ok(()),
            _ => Err(io::Error::other("disk gone")),
        })),
        ..Limits::default()
    };
    let r = recorder(&hub, limits);
    let rec = begin(&r, Duration::from_secs(30)).await.ok().unwrap();
    encoded(&hub, Some((100, 40)), (64, 32));
    encoded(&hub, None, (64, 32));
    // Nothing else comes: the screen is still.
    let c = tokio::time::timeout(Duration::from_secs(5), rec.outcome())
        .await
        .expect("ended without another frame or a stop")
        .unwrap();
    assert_eq!(c.end, End::DiskError);
    assert_eq!(c.segments[0].frames, 1);
}

#[test]
fn a_recording_ends_at_its_frame_cap_and_its_size_limit() {
    let (tx, rx) = mpsc::channel();
    for m in [unit(Some((100, 40))), unit(None), unit(None), unit(None)] {
        tx.send(m).unwrap();
    }
    let mut w = writer(Limits::default());
    w.cap = 3;
    let c = run(rx, w, |_| {}).unwrap();
    assert_eq!(c.end, End::FrameCap);
    assert_eq!(c.segments[0].frames, 3);
    let one = 6 + 6;
    let c = collected_with(
        vec![unit(Some((100, 40))), unit(None), unit(None)],
        Limits {
            max_bytes: one,
            ..Limits::default()
        },
    )
    .unwrap();
    assert_eq!(c.end, End::SizeLimit);
    assert_eq!(c.segments[0].frames, 2);
    let on_disk = c.spool.file().metadata().unwrap().len();
    assert!(
        on_disk <= one,
        "{on_disk} bytes written, the limit is {one}"
    );
}

#[test]
fn a_recordings_length_is_by_its_frames_capture_times() {
    // Frames captured within the length, then one after it that reaches the writer before
    // its clock runs out (a disk that held the writer up): that one is not in it, and the
    // clip lasts its length, not the time the writer took.
    let max = Duration::from_millis(1000);
    let w = writer_for(Limits::default(), max);
    let start = w.start;
    let (tx, rx) = mpsc::channel();
    for (key, ms) in [
        (Some((100, 40)), 0),
        (None, 400),
        (None, 1500),
        (None, 1600),
    ] {
        tx.send(frame_at(key, (64, 32), start + Duration::from_millis(ms)))
            .unwrap();
    }
    let c = run(rx, w, |_| {}).unwrap();
    assert_eq!(c.end, End::TimeLimit);
    assert_eq!(c.segments[0].frames, 2);
    assert_eq!(c.segments[0].duration_ms, 1000);
    assert_eq!(check(&mp4_of(&c, 0)).1, 1000);
    // A stop that reaches a late writer: the clip ends when the stop was asked for.
    let w = writer(Limits::default());
    let start = w.start;
    let (tx, rx) = mpsc::channel();
    tx.send(frame_at(Some((100, 40)), (64, 32), start)).unwrap();
    tx.send(Msg::End(End::Stopped, start + Duration::from_millis(300)))
        .unwrap();
    std::thread::sleep(Duration::from_millis(600));
    let c = run(rx, w, |_| {}).unwrap();
    assert_eq!(c.end, End::Stopped);
    assert_eq!(
        c.segments[0].duration_ms, 300,
        "not the writer's late clock"
    );
}

/// Runs the test `name` alone in a process of its own and asserts it passed there: what
/// it measures is the whole process's memory.
fn alone(name: &str) {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([name, "--exact", "--test-threads=1", "--nocapture"])
        .env("STARFOLD_ALONE", name)
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    assert!(text.contains("1 passed"), "{text}");
    println!("{text}");
}

/// Reads a response body chunk by chunk, as a client does, keeping none of it.
async fn drain(r: Response) -> usize {
    use futures_util::StreamExt;
    let mut s = r.into_body().into_data_stream();
    let mut n = 0;
    while let Some(c) = s.next().await {
        n += c.unwrap().len();
    }
    n
}

/// What one recording may hold at its worst, at any length the server takes (at most
/// [`MAX_RECORD_SECONDS`]): while it records, the largest frame queued (8 MiB), the
/// writer's buffer (256 KiB) and an index of 16 bytes for 120 frames a second (1,152,000 B
/// at 600 s); once it ends, the index and the MP4 header written from it (its tables up to
/// as much again), then the header and the answer's chunks in flight.
const BOUND: usize = 10 << 20;

// The parts of BOUND there at once while it records, at the longest length: they fit.
const _: () = assert!(
    tap::LARGEST_FRAME + BUFFER + 16 * MAX_FPS as usize * MAX_RECORD_SECONDS as usize <= BOUND
);

/// Records every frame a recording of `secs` may take, each a keyframe a few ms after the
/// last with no two durations alike in a row (the MP4's largest tables), then its answer as
/// a zip; the peak of the process's memory above the start.
async fn full_index_peak(hub: &Arc<Hub>, r: &Arc<Recorder>, secs: u64) -> usize {
    let base = crate::counting::reset();
    let rec = begin(r, Duration::from_secs(secs)).await.ok().unwrap();
    let tap = rec.tap.clone();
    let t0 = Instant::now();
    let cap = (MAX_FPS * secs) as usize;
    let key = message(Some((100, 40)));
    for i in 0..=cap {
        let at = t0 + Duration::from_millis(3 * i as u64 + (i as u64 % 2));
        hand_out_at(hub, true, key.clone(), (64, 32), at);
        if i % 1000 == 0 {
            while tap.queued() > 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
    }
    let c = rec.outcome().await.unwrap();
    assert_eq!(c.end, End::FrameCap);
    assert_eq!(c.segments[0].frames, cap);
    let n = drain(all_segments(c, IDLE)).await;
    assert!(n > cap * 6);
    drop(tap);
    crate::counting::peak() - base
}

#[test]
fn a_recordings_memory_stays_within_its_bound() {
    const NAME: &str = "record::tests::a_recordings_memory_stays_within_its_bound";
    if std::env::var("STARFOLD_ALONE").as_deref() != Ok(NAME) {
        return alone(NAME);
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let hub = Hub::new();
        let gate = Arc::new(Gate::default());
        let r = Recorder::with_limits(
            hub.clone(),
            Duration::from_secs(300),
            true,
            dir(),
            no_floor(gated(&gate)),
        );
        // The disk stalls with the largest frame queued, and a frame after it is refused.
        let base = crate::counting::reset();
        let rec = begin(&r, r.max).await.ok().unwrap();
        hand_out(
            &hub,
            true,
            large(true, tap::LARGEST_FRAME - tap::OVERHEAD),
            (64, 32),
        );
        hand_out(&hub, false, large(false, 1000), (64, 32));
        tokio::time::sleep(Duration::from_millis(200)).await;
        gate.set(true);
        let c = rec.outcome().await.unwrap();
        assert_eq!(c.end, End::DiskSlow);
        let n = drain(first_segment(c, IDLE)).await;
        assert!(n > tap::LARGEST_FRAME / 2);
        let stalled = crate::counting::peak() - base;
        let full = full_index_peak(&hub, &r, 300).await;
        let longest = full_index_peak(&hub, &r, MAX_RECORD_SECONDS).await;
        println!(
            "peak above the start: stalled disk {stalled} B, full index and answer {full} B \
             (300 s), {longest} B ({MAX_RECORD_SECONDS} s)"
        );
        assert!(stalled <= BOUND, "{stalled}");
        assert!(full <= BOUND, "{full}");
        assert!(longest <= BOUND, "{longest}");
    });
}

/// Starts a recording through the route; its id.
async fn started(r: &Arc<Recorder>) -> Result<String, StatusCode> {
    let res = start(State(r.clone())).await;
    match res.status() {
        StatusCode::OK => Ok(json(&body(res).await)["id"].as_str().unwrap().to_string()),
        s => Err(s),
    }
}

/// Waits up to 5 s for `f`.
async fn until(mut f: impl FnMut() -> bool) -> bool {
    let t = Instant::now();
    while !f() {
        if t.elapsed() > Duration::from_secs(5) {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    true
}

#[tokio::test]
async fn a_third_recording_waits_for_a_place_and_an_unread_answer_holds_one() {
    for clip in [4 << 20, 64] {
        let hub = Hub::new();
        let r = recorder(&hub, Limits::default());
        let a = started(&r).await.unwrap();
        let _b = started(&r).await.unwrap();
        assert_eq!(started(&r).await, Err(StatusCode::TOO_MANY_REQUESTS));
        let res = record(State(r.clone()), with(&[("seconds", "1")])).await;
        assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
        // A clip larger than the answer's chunks in flight, and one that fits in them.
        hand_out(&hub, true, large(true, clip), (64, 32));
        // Stopped, and its answer not read yet: the answer holds the place, however small.
        let unread = stop(State(r.clone()), with(&[("id", &a)])).await;
        assert_eq!(unread.status(), StatusCode::OK);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            started(&r).await,
            Err(StatusCode::TOO_MANY_REQUESTS),
            "{clip}"
        );
        drop(unread);
        assert!(until(|| r.running.load(Ordering::Acquire) == 1).await);
        assert!(
            started(&r).await.is_ok(),
            "the place is free once the answer is gone"
        );
    }
}

#[tokio::test]
async fn an_answer_nobody_reads_is_dropped_after_its_idle_time() {
    for clip in [4 << 20, 64] {
        let hub = Hub::new();
        let limits = Limits {
            idle: Duration::from_millis(300),
            ..Limits::default()
        };
        let r = recorder(&hub, limits);
        let id = started(&r).await.unwrap();
        hand_out(&hub, true, large(true, clip), (64, 32));
        let res = stop(State(r.clone()), with(&[("id", &id)])).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert!(
            until(|| r.running.load(Ordering::Acquire) == 0).await,
            "{clip}: the place let go while the answer is still held"
        );
        let n = axum::body::to_bytes(res.into_body(), usize::MAX).await;
        assert!(n.is_err(), "{clip}: the answer ended short");
    }
}

#[tokio::test]
async fn a_record_request_that_goes_away_ends_its_recording() {
    let hub = Hub::new();
    let r = recorder(&hub, Limits::default());
    let req = tokio::spawn(record(State(r.clone()), with(&[("seconds", "30")])));
    assert!(until(|| hub.taps.any()).await);
    encoded(&hub, Some((100, 40)), (64, 32));
    req.abort();
    assert!(until(|| !hub.taps.any()).await, "no demand on the stream");
    assert!(until(|| r.running.load(Ordering::Acquire) == 0).await);
}

#[tokio::test]
async fn a_writer_stuck_on_its_disk_gives_back_its_place_and_its_demand() {
    let hub = Hub::new();
    let gate = Arc::new(Gate::default());
    let r = recorder(
        &hub,
        Limits {
            stall: Duration::from_millis(300),
            ..gated(&gate)
        },
    );
    let id = started(&r).await.unwrap();
    // Its first write never returns.
    encoded(&hub, Some((100, 40)), (64, 32));
    assert!(until(|| r.running.load(Ordering::Acquire) == 0).await);
    assert!(!hub.taps.any());
    assert_eq!(hub.taps.cohort(), 1);
    let res = stop(State(r.clone()), with(&[("id", &id)])).await;
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(res.headers()["x-starfold-record-end"], "disk_slow");
    assert!(started(&r).await.is_ok(), "its place is free");
    gate.set(true);
}

#[tokio::test]
async fn a_writer_stuck_on_its_disk_leaves_the_stream_at_once() {
    let hub = Hub::new();
    let gate = Arc::new(Gate::default());
    let r = recorder(
        &hub,
        Limits {
            stall: Duration::from_millis(300),
            ..gated(&gate)
        },
    );
    let rec = begin(&r, Duration::from_secs(30)).await.ok().unwrap();
    let tap = rec.tap.clone();
    encoded(&hub, Some((100, 40)), (64, 32));
    let out = rec.outcome().await;
    assert_eq!(out.err().unwrap().end, End::DiskSlow);
    assert!(tap.is_closed());
    assert!(!hub.taps.any(), "a recording-only cohort stops encoding");
    assert_eq!(r.running.load(Ordering::Acquire), 0);
    gate.set(true);
}

#[tokio::test]
async fn a_start_cancelled_while_its_spool_opens_keeps_its_place_until_the_open_returns() {
    let hub = Hub::new();
    let gate = Arc::new(Gate::default());
    let g = gate.clone();
    let r = recorder(
        &hub,
        Limits {
            space: Arc::new(move |f: &File, d: &Path| {
                g.wait();
                spool::space(f, d)
            }),
            ..Limits::default()
        },
    );
    for _ in 0..2 {
        let rr = r.clone();
        let t = tokio::spawn(async move { begin(&rr, Duration::from_secs(30)).await.is_ok() });
        tokio::time::sleep(Duration::from_millis(100)).await;
        t.abort();
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        r.running.load(Ordering::Acquire),
        2,
        "each blocked open still holds its place"
    );
    assert_eq!(started(&r).await, Err(StatusCode::TOO_MANY_REQUESTS));
    gate.set(true);
    assert!(until(|| r.running.load(Ordering::Acquire) == 0).await);
    assert!(
        until(|| r.budget.outstanding() == 0).await,
        "nothing reserved"
    );
}

#[test]
fn off_workers_drops_on_a_blocking_thread() {
    struct Probe(std::sync::mpsc::Sender<std::thread::ThreadId>);
    impl Drop for Probe {
        fn drop(&mut self) {
            let _ = self.0.send(std::thread::current().id());
        }
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = rt.block_on(async {
        off_workers(Probe(tx.clone()));
        std::thread::current().id()
    });
    let dropped_on = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_ne!(dropped_on, worker, "not on the async worker");
    // Without a runtime (a plain thread), where it is.
    off_workers(Probe(tx));
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        std::thread::current().id()
    );
}

/// A recorder whose disk has `free` bytes free of `total`, leaves `floor`, and whose
/// recordings hold at most `max_bytes`.
fn on_disk(hub: &Arc<Hub>, free: u64, total: u64, floor: u64, max_bytes: u64) -> Arc<Recorder> {
    Recorder::with_limits(
        hub.clone(),
        Duration::from_secs(30),
        true,
        dir(),
        Limits {
            floor: Arc::new(move |_| floor),
            space: Arc::new(move |_: &File, _: &Path| Ok((free, total))),
            max_bytes,
            ..Limits::default()
        },
    )
}

#[tokio::test]
async fn a_start_at_exactly_the_floor_is_refused() {
    let hub = Hub::new();
    let gib = 1u64 << 30;
    let r = on_disk(&hub, gib, 10 * gib, gib, 1 << 20);
    let res = start(State(r.clone())).await;
    assert_eq!(res.status(), StatusCode::INSUFFICIENT_STORAGE);
    assert_eq!(res.headers()["x-starfold-record-end"], "disk_space");
    assert_eq!(r.running.load(Ordering::Acquire), 0, "no place kept");
    // One recording's room above it starts.
    let r = on_disk(&hub, gib + (1 << 20), 10 * gib, gib, 1 << 20);
    assert!(started(&r).await.is_ok());
    assert_eq!(r.budget.outstanding(), 1 << 20);
}

#[tokio::test]
async fn two_recordings_cannot_spend_the_same_free_space() {
    let hub = Hub::new();
    let gib = 1u64 << 30;
    let max = 1u64 << 20;
    // Room for one and a half recordings above the floor.
    let r = on_disk(&hub, gib + max + max / 2, 10 * gib, gib, max);
    let a = started(&r).await.unwrap();
    let res = start(State(r.clone())).await;
    assert_eq!(res.status(), StatusCode::INSUFFICIENT_STORAGE);
    assert_eq!(r.budget.outstanding(), max, "the first holds its room");
    encoded(&hub, Some((100, 40)), (64, 32));
    drop(stop(State(r.clone()), with(&[("id", &a)])).await);
    assert!(
        until(|| r.budget.outstanding() == 0).await,
        "its room comes back when its spool is closed"
    );
    assert!(started(&r).await.is_ok());
}

#[tokio::test]
async fn a_recording_reserves_the_room_there_is_up_to_its_size_limit() {
    let hub = Hub::new();
    let gib = 1u64 << 30;
    // Less room above the floor than the size limit, and at least what a start needs: the
    // recording reserves that room, and that is the most it writes.
    let r = on_disk(
        &hub,
        gib + spool::MIN_ROOM,
        10 * gib,
        gib,
        2 * spool::MIN_ROOM,
    );
    let rec = begin(&r, Duration::from_secs(30)).await.ok().unwrap();
    assert_eq!(r.budget.outstanding(), spool::MIN_ROOM);
    let more = on_disk(
        &hub,
        gib + spool::MIN_ROOM - 1,
        10 * gib,
        gib,
        2 * spool::MIN_ROOM,
    );
    assert_eq!(
        start(State(more)).await.status(),
        StatusCode::INSUFFICIENT_STORAGE,
        "less than a start needs"
    );
    let mut w = writer(Limits::default());
    w.out = BufWriter::with_capacity(
        BUFFER,
        spool::open(
            &dir(),
            &spool::Rules {
                floor: &move |_: u64| gib,
                space: &move |_: &File, _: &Path| Ok((gib + 20, 10 * gib)),
                max_bytes: 20,
            },
            &Arc::default(),
        )
        .unwrap(),
    );
    assert_eq!(w.limit(), 20);
    drop(stop_rec(rec).await);
}

#[tokio::test]
async fn a_spool_on_memory_or_short_of_space_or_nowhere_is_refused() {
    let hub = Hub::new();
    let refused = |dir: PathBuf, floor: fn(u64) -> u64| {
        Recorder::with_limits(
            hub.clone(),
            Duration::from_secs(30),
            true,
            dir,
            Limits {
                floor: Arc::new(floor),
                ..Limits::default()
            },
        )
    };
    let r = refused(dir(), |total| total + 1);
    let res = start(State(r.clone())).await;
    assert_eq!(res.status(), StatusCode::INSUFFICIENT_STORAGE);
    let r = refused(dir().join("no-such-directory"), |_| 0);
    assert_eq!(
        start(State(r.clone())).await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    #[cfg(target_os = "linux")]
    if std::path::Path::new("/dev/shm").is_dir() {
        let r = refused(PathBuf::from("/dev/shm"), |_| 0);
        let res = start(State(r.clone())).await;
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(json(&body(res).await)["error"]
            .as_str()
            .unwrap()
            .contains("in memory"));
    }
    assert_eq!(r.running.load(Ordering::Acquire), 0, "no place kept");
    assert!(!hub.taps.any(), "nothing takes the stream");
}

#[tokio::test]
async fn a_recording_ends_when_another_writer_takes_the_disk_below_its_floor() {
    let hub = Hub::new();
    let low = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let l = low.clone();
    let r = Recorder::with_limits(
        hub.clone(),
        Duration::from_secs(30),
        true,
        dir(),
        Limits {
            floor: Arc::new(move |total| match l.load(Ordering::Acquire) {
                true => total,
                false => 0,
            }),
            check_every: 1,
            ..Limits::default()
        },
    );
    let rec = begin(&r, Duration::from_secs(30)).await.ok().unwrap();
    let tap = rec.tap.clone();
    encoded(&hub, Some((100, 40)), (64, 32));
    encoded(&hub, None, (64, 32));
    assert!(until(|| tap.queued() == 0).await, "both written");
    low.store(true, Ordering::Release);
    encoded(&hub, None, (64, 32));
    let c = rec.outcome().await.unwrap();
    assert_eq!(c.end, End::DiskSpace);
    assert_eq!(
        c.segments[0].frames, 3,
        "the frame it found the disk low after"
    );
}

#[test]
fn a_spool_leaves_nothing_behind() {
    let base = dir().join(format!("starfold-spool-test-{}", new_id()));
    std::fs::create_dir(&base).unwrap();
    let mut f = spool::anonymous(&base).unwrap();
    f.write_all(b"video").unwrap();
    let names = |d: &std::path::Path| -> Vec<String> {
        std::fs::read_dir(d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    };
    let own = base.join(spool::OWN);
    for name in names(&base) {
        assert_eq!(name, spool::OWN);
        // Windows deletes the file when it is closed; elsewhere it has no name from the start.
        #[cfg(not(windows))]
        assert!(names(&own).is_empty(), "no named file in {}", own.display());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&own).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
    }
    drop(f);
    if own.exists() {
        assert!(names(&own).is_empty(), "nothing left in {}", own.display());
    }
    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn a_recording_dir_inside_the_shared_folder_refuses_to_start() {
    let root = dir().join(format!("starfold-root-test-{}", new_id()));
    std::fs::create_dir(&root).unwrap();
    let args = |extra: &[&str]| {
        let mut v = vec!["--files-root".to_string(), root.display().to_string()];
        v.extend(extra.iter().map(|s| s.to_string()));
        crate::parse_args(v)
    };
    let inside = root.join("rec").display().to_string();
    let e = settings(&args(&["--enable-recording", "--recording-dir", &inside]));
    assert!(e.err().unwrap().contains("inside --files-root"));
    assert!(
        !root.join("rec").exists(),
        "nothing made in the shared folder"
    );
    let e = settings(&args(&[
        "--enable-recording",
        "--recording-dir",
        &root.display().to_string(),
    ]));
    assert!(e.is_err(), "the shared folder itself");
    assert!(settings(&args(&["--recording-dir", &inside])).is_ok());
    let outside = dir().display().to_string();
    let s = settings(&args(&["--enable-recording", "--recording-dir", &outside])).unwrap();
    assert!(s.dir.is_absolute());
    std::fs::remove_dir_all(&root).unwrap();
}
