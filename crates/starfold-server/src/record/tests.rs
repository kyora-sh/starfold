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

/// A directory on disk for the tests' spools (their files have no name there).
fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The server's limits, but no free-space floor (the test machine's disk is not under
/// test) unless a test sets one.
fn limits() -> spool::Limits {
    spool::Limits {
        max_bytes: MAX_RECORDING_BYTES,
        buffer: spool::BUFFER,
        queue_bytes: QUEUE_BYTES,
        check_every: CHECK_EVERY,
        floor: Arc::new(|_| 0),
        before_write: None,
    }
}

fn recorder(on: bool, limits: spool::Limits) -> Arc<Recorder> {
    Recorder::with_limits(Hub::new(), Duration::from_secs(30), on, dir(), limits)
}

fn a_place() -> Arc<Place> {
    Arc::new(Place(Arc::new(AtomicUsize::new(1))))
}

fn unit(key: Option<(u8, u8)>) -> Out {
    let mut b = vec![0x01, key.is_some() as u8, 0, 0, 0, 1];
    b.resize(16, 0);
    match key {
        Some((profile, level)) => b.extend_from_slice(&[
            0, 0, 0, 1, 0x67, profile, 0, level, 0xac, 0, 0, 0, 1, 0x68, 0xee, 0, 0, 0, 1, 0x65,
            0xaa,
        ]),
        None => b.extend_from_slice(&[0, 0, 0, 1, 0x41, 0xbb]),
    }
    Out::Bin(bytes::Bytes::from(b))
}

/// Records `units` (then a size change, which ends the recording in queue order) under
/// `limits`; returns the clip, or why there is none.
async fn record_units(units: &[Option<(u8, u8)>], limits: spool::Limits) -> Result<Clip, String> {
    let hub = Hub::new();
    let (tx, rx) = mpsc::unbounded_channel();
    let client = hub.add_client(tx.clone());
    let (_keep, stop) = oneshot::channel();
    for u in units {
        tx.send(unit(*u)).unwrap();
    }
    tx.send(Out::Text("{\"t\":\"hello\"}".into())).unwrap();
    let file = spool::anonymous(&dir()).unwrap();
    let mut writer = spool::Writer::spawn(file, dir(), limits).unwrap();
    let (parts, end) = tokio::time::timeout(
        Duration::from_secs(5),
        collect(
            &hub,
            &client,
            rx,
            stop,
            Duration::from_secs(30),
            &mut writer,
        ),
    )
    .await
    .expect("the recording ended");
    let done = tokio::task::spawn_blocking(move || writer.finish())
        .await
        .unwrap()
        .unwrap();
    Clip::build(parts, end, done, a_place())
}

async fn clip(units: &[Option<(u8, u8)>]) -> Clip {
    record_units(units, limits()).await.unwrap()
}

/// Segment `i` of `c` as the answer writes it.
fn mp4_of(c: &Clip, i: usize) -> Vec<u8> {
    let mut out = Vec::new();
    copy_segment(&c.file, &c.segments[i], &mut out).unwrap();
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

#[tokio::test]
async fn a_new_level_keeps_one_segment() {
    let c = clip(&[Some((100, 40)), None, Some((100, 41)), None]).await;
    assert_eq!(c.segments.len(), 1);
    assert_eq!(check(&mp4_of(&c, 0)).0, 4);
}

#[tokio::test]
async fn a_flapping_encoder_ends_at_the_segment_cap() {
    let units: Vec<_> = (0..MAX_SEGMENTS + 5)
        .flat_map(|i| [Some((if i % 2 == 0 { 100 } else { 66 }, 40)), None])
        .collect();
    let c = clip(&units).await;
    assert_eq!(c.end, End::SegmentCap);
    assert_eq!(c.segments.len(), MAX_SEGMENTS);
    for i in 0..c.segments.len() {
        assert_eq!(check(&mp4_of(&c, i)).0, 2);
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
    let mut writer =
        spool::Writer::spawn(spool::anonymous(&dir()).unwrap(), dir(), limits()).unwrap();
    let (parts, end) = collect(
        &hub,
        &client,
        rx,
        stop,
        Duration::from_secs(30),
        &mut writer,
    )
    .await;
    let done = tokio::task::spawn_blocking(move || writer.finish())
        .await
        .unwrap()
        .unwrap();
    let c = Clip::build(parts, end, done, a_place()).unwrap();
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

/// A keyframe's sample in the spool and its part.
fn keyframe_part(file: &mut File) -> Part {
    let Out::Bin(b) = unit(Some((100, 40))) else {
        unreachable!()
    };
    let sample = mp4::sample(&b[16..]);
    file.write_all(&sample).unwrap();
    Part {
        samples: vec![mp4::Sample {
            ms: 0,
            key: true,
            size: sample.len() as u32,
        }],
        params: mp4::parameter_sets(&b[16..]).ok(),
        size: (64, 32),
        end_ms: 100,
    }
}

fn empty_part() -> Part {
    Part {
        samples: Vec::new(),
        params: None,
        size: (64, 32),
        end_ms: 100,
    }
}

fn done(file: File, samples: usize) -> spool::Done {
    spool::Done {
        file,
        samples,
        stop: None,
    }
}

#[test]
fn a_segment_that_cant_be_written_leaves_the_others() {
    let mut file = spool::anonymous(&dir()).unwrap();
    let parts = vec![
        keyframe_part(&mut file),
        empty_part(),
        keyframe_part(&mut file),
    ];
    let c = Clip::build(parts, End::Stopped, done(file, 2), a_place()).unwrap();
    assert_eq!(c.end, End::SegmentLost);
    assert_eq!(c.segments.len(), 2);
    assert_eq!(
        check(&mp4_of(&c, 1)).0,
        1,
        "the second keyframe's own bytes"
    );
}

#[tokio::test]
async fn a_lost_segment_is_named_in_the_headers() {
    let mut file = spool::anonymous(&dir()).unwrap();
    let parts = vec![
        keyframe_part(&mut file),
        empty_part(),
        keyframe_part(&mut file),
    ];
    let c = Clip::build(parts, End::Stopped, done(file, 2), a_place()).unwrap();
    let r = first_segment(c);
    assert_eq!(r.headers()["x-starfold-segments"], "2");
    assert_eq!(r.headers()["x-starfold-record-end"], "segment_lost");
    assert_eq!(r.headers()["x-voxl-segments"], "2");
    assert_eq!(r.headers()["x-voxl-record-end"], "segment_lost");
}

#[test]
fn a_recording_with_no_segment_left_is_an_error() {
    let file = spool::anonymous(&dir()).unwrap();
    let e = Clip::build(vec![empty_part()], End::Stopped, done(file, 0), a_place()).err();
    assert_eq!(e.as_deref(), Some("no frames were recorded"));
}

#[tokio::test]
async fn the_recorder_leaves_the_hub_when_it_ends() {
    let r = recorder(true, limits());
    let (stop_tx, stop) = oneshot::channel();
    let done = begin(&r, stop, Duration::from_secs(30), a_place())
        .ok()
        .unwrap();
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
async fn one_segment_streams_byte_for_byte_what_the_writer_makes() {
    let units = [Some((100, 40)), None, None];
    let ms = [0u64, 40, 95];
    let frames: Vec<mp4::Frame> = units
        .iter()
        .zip(ms)
        .map(|(u, ms)| {
            let Out::Bin(b) = unit(*u) else {
                unreachable!()
            };
            mp4::Frame {
                ms,
                key: u.is_some(),
                data: b.slice(16..),
            }
        })
        .collect();
    let mut file = spool::anonymous(&dir()).unwrap();
    let mut samples = Vec::new();
    for f in &frames {
        let s = mp4::sample(&f.data);
        file.write_all(&s).unwrap();
        samples.push(mp4::Sample {
            ms: f.ms,
            key: f.key,
            size: s.len() as u32,
        });
    }
    let part = Part {
        samples,
        params: mp4::parameter_sets(&frames[0].data).ok(),
        size: (64, 32),
        end_ms: 120,
    };
    let c = Clip::build(vec![part], End::Stopped, done(file, 3), a_place()).unwrap();
    let r = first_segment(c);
    assert_eq!(r.headers()[header::CONTENT_TYPE], "video/mp4");
    assert!(r.headers().get("x-starfold-segments").is_none());
    assert!(r.headers().get("x-starfold-record-end").is_none());
    let length: usize = r.headers()[header::CONTENT_LENGTH]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    let got = body(r).await;
    assert_eq!(got.len(), length);
    assert_eq!(got, mp4::write(&frames, 64, 32, 120).unwrap());
}

#[tokio::test]
async fn without_zip_several_segments_are_the_first_and_headers() {
    let c = clip(&[Some((100, 40)), Some((66, 40))]).await;
    let first = mp4_of(&c, 0);
    let r = first_segment(c);
    assert_eq!(r.headers()["x-starfold-segments"], "2");
    assert_eq!(r.headers()["x-starfold-record-end"], "resized");
    assert_eq!(body(r).await, first);
}

#[tokio::test]
async fn the_zip_streams_a_manifest_and_every_segment() {
    use std::io::Read;
    let c = clip(&[Some((100, 40)), None, Some((66, 40))]).await;
    let parts: Vec<Vec<u8>> = (0..c.segments.len()).map(|i| mp4_of(&c, i)).collect();
    let starts: Vec<u64> = c.segments.iter().map(|s| s.start_ms).collect();
    let z = body(streamed(move |out| c.write_zip(out)).into_response()).await;
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

fn json(b: &[u8]) -> serde_json::Value {
    serde_json::from_slice(b).unwrap()
}

fn query(pairs: &[(&str, &str)]) -> Query<HashMap<String, String>> {
    Query(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    )
}

#[tokio::test]
async fn a_bad_format_leaves_the_recording_running() {
    let r = recorder(true, limits());
    let started = start(State(r.clone())).await;
    let id = json(&body(started).await)["id"]
        .as_str()
        .unwrap()
        .to_string();
    let bad = stop(State(r.clone()), query(&[("id", &id), ("format", "avi")])).await;
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    assert!(r.active.lock().contains_key(&id));
    let done = stop(State(r.clone()), query(&[("id", &id), ("format", "zip")])).await;
    assert_eq!(done.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(r.active.lock().is_empty());
}

#[tokio::test]
async fn recording_is_off_unless_enabled() {
    let r = recorder(false, limits());
    for res in [
        record(State(r.clone()), query(&[("seconds", "1")])).await,
        start(State(r.clone())).await,
        stop(State(r.clone()), query(&[("id", "1")])).await,
    ] {
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let j = json(&body(res).await);
        assert_eq!(j["recording"], false);
        assert!(j["error"].as_str().unwrap().contains("--enable-recording"));
    }
    assert!(r.hub.clients.lock().is_empty());
    assert_eq!(r.running.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn at_most_two_recordings_run_at_once() {
    let r = recorder(true, limits());
    let a = json(&body(start(State(r.clone())).await).await);
    let b = json(&body(start(State(r.clone())).await).await);
    assert!(a["id"].is_string() && b["id"].is_string());
    assert_eq!(r.hub.clients.lock().len(), 2);
    let third = start(State(r.clone())).await;
    assert_eq!(third.status(), StatusCode::TOO_MANY_REQUESTS);
    let fourth = record(State(r.clone()), query(&[("seconds", "1")])).await;
    assert_eq!(fourth.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        r.hub.clients.lock().len(),
        2,
        "nothing started for the refused"
    );
    let id = a["id"].as_str().unwrap();
    let _ = body(stop(State(r.clone()), query(&[("id", id)])).await).await;
    let again = start(State(r.clone())).await;
    assert_eq!(
        again.status(),
        StatusCode::OK,
        "a stopped recording gives its place back"
    );
}

#[tokio::test]
async fn only_the_id_a_recording_was_started_with_stops_it() {
    let r = recorder(true, limits());
    let id = json(&body(start(State(r.clone())).await).await)["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(id.len(), 32);
    assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
    let mut other = id.clone().into_bytes();
    other[31] = if other[31] == b'0' { b'1' } else { b'0' };
    for wrong in [
        "1",
        "0",
        "",
        &id[..31],
        std::str::from_utf8(&other).unwrap(),
    ] {
        let res = stop(State(r.clone()), query(&[("id", wrong)])).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "{wrong:?}");
    }
    assert!(r.active.lock().contains_key(&id), "still running");
    let res = stop(State(r.clone()), query(&[("id", &id)])).await;
    assert_ne!(res.status(), StatusCode::NOT_FOUND);
    assert!(r.active.lock().is_empty());
}

#[tokio::test]
async fn the_size_limit_ends_a_recording_with_the_clip_so_far() {
    let Out::Bin(b) = unit(Some((100, 40))) else {
        unreachable!()
    };
    let key = mp4::sample(&b[16..]).len() as u64;
    let Out::Bin(b) = unit(None) else {
        unreachable!()
    };
    let delta = mp4::sample(&b[16..]).len() as u64;
    let mut l = limits();
    l.max_bytes = key + 2 * delta;
    let c = record_units(&[Some((100, 40)), None, None, None, None], l)
        .await
        .unwrap();
    assert_eq!(c.end, End::SizeLimit);
    assert_eq!(c.segments[0].frames, 3);
    assert_eq!(c.segments[0].len, key + 2 * delta);
    assert_eq!(check(&mp4_of(&c, 0)).0, 3);
}

#[tokio::test]
async fn a_disk_that_falls_behind_ends_the_recording_not_the_stream() {
    let release = Arc::new(std::sync::Barrier::new(2));
    let gate = release.clone();
    let stalled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut l = limits();
    l.buffer = 1;
    l.queue_bytes = 64;
    l.before_write = Some(Arc::new(move |written| {
        if written > 0 && !stalled.swap(true, Ordering::AcqRel) {
            // The disk stops answering after the first sample, until the test lets it go.
            gate.wait();
        }
        Ok(())
    }));
    let units: Vec<_> = std::iter::once(Some((100, 40)))
        .chain(std::iter::repeat_n(None, 200))
        .collect();
    let hub = Hub::new();
    let (tx, rx) = mpsc::unbounded_channel();
    let client = hub.add_client(tx.clone());
    let (_keep, stop) = oneshot::channel();
    for u in &units {
        tx.send(unit(*u)).unwrap();
    }
    let mut writer = spool::Writer::spawn(spool::anonymous(&dir()).unwrap(), dir(), l).unwrap();
    let t = Instant::now();
    let (parts, end) = tokio::time::timeout(
        Duration::from_secs(2),
        collect(
            &hub,
            &client,
            rx,
            stop,
            Duration::from_secs(30),
            &mut writer,
        ),
    )
    .await
    .expect("the stream never waited on the disk");
    assert_eq!(end, End::DiskSlow);
    assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
    release.wait();
    let done = tokio::task::spawn_blocking(move || writer.finish())
        .await
        .unwrap()
        .unwrap();
    let c = Clip::build(parts, end, done, a_place()).unwrap();
    assert_eq!(c.end, End::DiskSlow);
    assert!(c.segments[0].frames >= 1);
}

#[tokio::test]
async fn a_write_error_ends_the_recording_cleanly() {
    let mut l = limits();
    l.buffer = 1;
    let Out::Bin(b) = unit(Some((100, 40))) else {
        unreachable!()
    };
    let key = mp4::sample(&b[16..]).len() as u64;
    l.before_write = Some(Arc::new(move |written| {
        if written > key {
            Err(io::Error::other("no space left on device"))
        } else {
            Ok(())
        }
    }));
    let c = record_units(&[Some((100, 40)), None, None, None], l)
        .await
        .unwrap();
    assert_eq!(c.end, End::DiskError);
    assert_eq!(c.segments[0].frames, 2, "what reached the disk");
    assert_eq!(check(&mp4_of(&c, 0)).0, 2);
    let again = clip(&[Some((100, 40)), None]).await;
    assert_eq!(again.segments[0].frames, 2, "the next recording works");
}

#[tokio::test]
async fn a_spool_that_cant_be_used_refuses_the_recording_and_holds_nothing() {
    #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
    let mut cases: Vec<(PathBuf, spool::Limits, StatusCode, &str)> = vec![
        (
            dir().join("no-such-directory"),
            limits(),
            StatusCode::SERVICE_UNAVAILABLE,
            "writable --recording-dir",
        ),
        (
            dir(),
            spool::Limits {
                floor: Arc::new(|_| u64::MAX),
                ..limits()
            },
            StatusCode::INSUFFICIENT_STORAGE,
            "not enough free space",
        ),
    ];
    #[cfg(target_os = "linux")]
    if std::path::Path::new("/dev/shm").is_dir() {
        cases.push((
            PathBuf::from("/dev/shm"),
            limits(),
            StatusCode::SERVICE_UNAVAILABLE,
            "is in memory",
        ));
    }
    for (d, l, code, says) in cases {
        let r = Recorder::with_limits(Hub::new(), Duration::from_secs(30), true, d.clone(), l);
        let res = start(State(r.clone())).await;
        assert_eq!(res.status(), code, "{}", d.display());
        let j = json(&body(res).await);
        assert!(j["error"].as_str().unwrap().contains(says), "{j}");
        assert!(r.hub.clients.lock().is_empty());
        assert_eq!(r.running.load(Ordering::Acquire), 0);
    }
}

#[tokio::test]
async fn running_low_on_space_ends_the_recording() {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let mut l = limits();
    l.buffer = 1;
    l.check_every = 1;
    // Room at the first check, none at the next.
    l.floor = Arc::new(move |_| {
        if seen.fetch_add(1, Ordering::AcqRel) == 0 {
            0
        } else {
            u64::MAX
        }
    });
    let c = record_units(&[Some((100, 40)), None, None, None], l)
        .await
        .unwrap();
    assert_eq!(c.end, End::DiskSpace);
    assert!(calls.load(Ordering::Acquire) >= 2);
}

#[test]
fn a_spool_leaves_nothing_in_its_directory() {
    let d = std::env::temp_dir().join(format!("starfold-spool-test-{}", new_id()));
    std::fs::create_dir(&d).unwrap();
    let mut f = spool::anonymous(&d).unwrap();
    f.write_all(b"video").unwrap();
    #[cfg(unix)]
    assert_eq!(
        std::fs::read_dir(&d).unwrap().count(),
        0,
        "no name while open"
    );
    drop(f);
    assert_eq!(std::fs::read_dir(&d).unwrap().count(), 0);
    std::fs::remove_dir(&d).unwrap();
}

#[test]
fn the_free_space_left_is_the_smaller_of_a_gib_and_a_tenth() {
    assert_eq!(spool::floor(100 << 30), 1 << 30);
    assert_eq!(spool::floor(5 << 30), (5 << 30) / 10);
    assert_eq!(spool::floor(0), 0);
}
