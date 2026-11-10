use super::*;
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// A fresh folder holding `root` (shared) and `outside` (not), removed on drop.
struct Tmp(PathBuf);

impl Tmp {
    fn new() -> Tmp {
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "starfold-files-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(p.join("root")).unwrap();
        std::fs::create_dir_all(p.join("outside")).unwrap();
        Tmp(p.canonicalize().unwrap())
    }
    fn root(&self) -> PathBuf {
        self.0.join("root")
    }
    fn outside(&self) -> PathBuf {
        self.0.join("outside")
    }
    fn files(&self, default_dir: &str) -> Arc<Files> {
        Arc::new(Files {
            root: Some(Root::open(&self.root()).unwrap()),
            upload: true,
            download: true,
            default_dir: default_dir.into(),
            max_upload_bytes: 0,
        })
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn q(pairs: &[(&str, &str)]) -> Q {
    Query(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    )
}

async fn put(f: &Arc<Files>, pairs: &[(&str, &str)], body: &str) -> (StatusCode, String) {
    let mut h = HeaderMap::new();
    h.insert(UPLOAD_HEADER, "1".parse().unwrap());
    let r = upload(State(f.clone()), q(pairs), h, Body::from(body.to_string())).await;
    let status = r.status();
    let b = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&b).into_owned())
}

async fn get_list(f: &Arc<Files>, path: &str) -> (StatusCode, serde_json::Value) {
    let r = list(State(f.clone()), q(&[("path", path)])).await;
    let status = r.status();
    let b = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&b).unwrap_or_default())
}

async fn get_file(f: &Arc<Files>, path: &str) -> (StatusCode, Vec<u8>) {
    let r = download(State(f.clone()), q(&[("path", path)])).await;
    let status = r.status();
    let b = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, b.to_vec())
}

fn is_empty(p: &Path) -> bool {
    std::fs::read_dir(p).unwrap().next().is_none()
}

#[tokio::test]
async fn uploads_land_in_the_default_folder_made_on_first_use() {
    let t = Tmp::new();
    let f = t.files("Downloads");
    let (s, b) = put(&f, &[("name", "a.txt")], "hello").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(b.contains("\"Downloads/a.txt\""), "{b}");
    assert_eq!(
        std::fs::read(t.root().join("Downloads/a.txt")).unwrap(),
        b"hello"
    );
    let (s, b) = put(&f, &[("name", "a.txt"), ("sub", "x/y")], "again").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(b.contains("\"Downloads/x/y/a.txt\""), "{b}");
    let (_, b) = put(&f, &[("name", "a.txt")], "two").await;
    assert!(b.contains("\"Downloads/a (1).txt\""), "{b}");
    assert_eq!(
        get_file(&f, "Downloads/a.txt").await,
        (StatusCode::OK, b"hello".to_vec())
    );
    let (s, l) = get_list(&f, "Downloads").await;
    assert_eq!(s, StatusCode::OK);
    let names: Vec<_> = l["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap().to_string())
        .collect();
    assert!(
        names.contains(&"a.txt".into()) && names.contains(&"x".into()),
        "{names:?}"
    );
    assert!(!names.iter().any(|n| n.ends_with(".part")), "{names:?}");
}

#[tokio::test]
async fn the_default_folder_is_listed_empty_before_it_exists() {
    let t = Tmp::new();
    let f = t.files("Downloads");
    let (s, l) = get_list(&f, "Downloads").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(l["entries"], serde_json::json!([]));
    assert!(!t.root().join("Downloads").exists());
    assert_eq!(get_list(&f, "Other").await.0, StatusCode::NOT_FOUND);
    let (s, _) = put(&f, &[("name", "a"), ("path", "Other")], "x").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert!(
        !t.root().join("Other").exists(),
        "only the default folder is made"
    );
}

#[tokio::test]
async fn a_symlinked_upload_folder_is_refused_whether_its_target_exists_or_not() {
    let t = Tmp::new();
    symlink(t.outside(), t.root().join("Downloads")).unwrap();
    let f = t.files("Downloads");
    assert_eq!(
        put(&f, &[("name", "a.txt")], "x").await.0,
        StatusCode::FORBIDDEN
    );
    assert!(is_empty(&t.outside()));

    let t = Tmp::new();
    symlink(t.outside().join("new"), t.root().join("Downloads")).unwrap();
    let f = t.files("Downloads");
    assert_eq!(
        put(&f, &[("name", "a.txt")], "x").await.0,
        StatusCode::FORBIDDEN
    );
    assert!(
        is_empty(&t.outside()),
        "nothing is made at the link's target"
    );
}

#[tokio::test]
async fn a_symlink_in_a_middle_component_is_refused() {
    let t = Tmp::new();
    symlink(t.outside(), t.root().join("a")).unwrap();
    let f = t.files("a/b/c");
    assert_eq!(
        put(&f, &[("name", "a.txt")], "x").await.0,
        StatusCode::FORBIDDEN
    );
    assert!(is_empty(&t.outside()));

    std::fs::create_dir_all(t.outside().join("b")).unwrap();
    std::fs::write(t.outside().join("b/secret"), "s").unwrap();
    assert_eq!(get_list(&f, "a/b").await.0, StatusCode::FORBIDDEN);
    assert_eq!(get_file(&f, "a/b/secret").await.0, StatusCode::FORBIDDEN);
    assert_eq!(
        put(&f, &[("name", "x"), ("path", "a/b")], "x").await.0,
        StatusCode::FORBIDDEN
    );
    assert!(!t.outside().join("b/x").exists());
}

#[tokio::test]
async fn a_symlink_in_an_uploaded_folder_path_is_refused() {
    let t = Tmp::new();
    std::fs::create_dir(t.root().join("Downloads")).unwrap();
    symlink(t.outside(), t.root().join("Downloads/out")).unwrap();
    let f = t.files("Downloads");
    let (s, _) = put(&f, &[("name", "a.txt"), ("sub", "out/new")], "x").await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert!(is_empty(&t.outside()));
    std::fs::write(t.root().join("Downloads/file"), "f").unwrap();
    let (s, _) = put(&f, &[("name", "a.txt"), ("sub", "file/x")], "x").await;
    assert_eq!(s, StatusCode::CONFLICT);
}

#[tokio::test]
async fn parent_and_absolute_paths_are_rejected() {
    let t = Tmp::new();
    std::fs::create_dir(t.root().join("d")).unwrap();
    let f = t.files("Downloads");
    for p in ["..", "../outside", "d/../..", "d/../d", "/etc", "/", "d\0x"] {
        assert_eq!(
            get_list(&f, p).await.0,
            StatusCode::BAD_REQUEST,
            "list {p:?}"
        );
        assert_eq!(
            get_file(&f, p).await.0,
            StatusCode::BAD_REQUEST,
            "download {p:?}"
        );
        let (s, _) = put(&f, &[("name", "a"), ("path", p)], "x").await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "upload {p:?}");
    }
    for name in ["..", ".", "a/b", "../x"] {
        let (s, _) = put(&f, &[("name", name)], "x").await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "name {name:?}");
    }
    let (s, _) = put(&f, &[("name", "a"), ("sub", "x/../y")], "x").await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(is_empty(&t.outside()));
}

#[tokio::test]
async fn downloads_never_go_through_a_symlink() {
    let t = Tmp::new();
    std::fs::write(t.outside().join("secret"), "s").unwrap();
    std::fs::write(t.root().join("inside"), "i").unwrap();
    symlink(t.outside().join("secret"), t.root().join("file-out")).unwrap();
    symlink(t.root().join("inside"), t.root().join("file-in")).unwrap();
    symlink(t.outside(), t.root().join("dir-out")).unwrap();
    std::fs::create_dir(t.root().join("folder")).unwrap();
    std::fs::write(t.root().join("folder/kept"), "k").unwrap();
    symlink(t.outside(), t.root().join("folder/dir-out")).unwrap();
    symlink(t.outside().join("secret"), t.root().join("folder/file-out")).unwrap();
    symlink("folder", t.root().join("dir-in")).unwrap();
    std::fs::create_dir(t.root().join("folder/sub")).unwrap();
    std::fs::write(t.root().join("folder/sub/deep"), "d").unwrap();
    let f = t.files("Downloads");
    for p in [
        "file-out",
        "file-in",
        "dir-out",
        "dir-out/secret",
        "folder/file-out",
        "dir-in",
        "dir-in/kept",
        "dir-in/sub/deep",
    ] {
        assert_eq!(get_file(&f, p).await.0, StatusCode::FORBIDDEN, "{p}");
    }
    assert_eq!(get_list(&f, "dir-in").await.0, StatusCode::FORBIDDEN);
    assert_eq!(get_list(&f, "dir-in/sub").await.0, StatusCode::FORBIDDEN);
    assert_eq!(
        get_file(&f, "inside").await,
        (StatusCode::OK, b"i".to_vec())
    );
    let (s, zip) = get_file(&f, "folder").await;
    assert_eq!(s, StatusCode::OK);
    let z = zip::ZipArchive::new(std::io::Cursor::new(zip)).unwrap();
    let names: Vec<_> = z.file_names().map(String::from).collect();
    assert!(names.contains(&"folder/kept".into()), "{names:?}");
    assert!(
        !names.iter().any(|n| n.contains("-out")),
        "links are left out: {names:?}"
    );
    let (_, l) = get_list(&f, "").await;
    let listed: Vec<_> = l["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap().to_string())
        .collect();
    assert!(listed.contains(&"inside".into()), "{listed:?}");
    assert!(
        !listed.iter().any(|n| n.contains("-out") || n == "file-in"),
        "links are not listed: {listed:?}"
    );
}

#[tokio::test]
async fn special_files_are_not_downloaded() {
    let t = Tmp::new();
    nix::unistd::mkfifo(
        &t.root().join("fifo"),
        nix::sys::stat::Mode::from_bits_truncate(0o600),
    )
    .unwrap();
    let f = t.files("Downloads");
    assert_eq!(get_file(&f, "fifo").await.0, StatusCode::BAD_REQUEST);
}

#[test]
fn the_component_walk_refuses_symlinks_on_its_own() {
    let t = Tmp::new();
    symlink(t.outside(), t.root().join("link")).unwrap();
    std::fs::write(t.root().join("file"), "f").unwrap();
    let root = Root::open(&t.root()).unwrap();
    let top = root.dir(&[], false).unwrap();
    assert!(matches!(
        top.child(OsStr::new("link"), false),
        Err(Refused::Link)
    ));
    assert!(matches!(
        top.child(OsStr::new("link"), true),
        Err(Refused::Link)
    ));
    assert!(matches!(
        top.child(OsStr::new("file"), true),
        Err(Refused::NotFolder)
    ));
    assert!(matches!(
        top.child(OsStr::new("none"), false),
        Err(Refused::Missing)
    ));
    assert!(matches!(top.open(OsStr::new("link")), Err(Refused::Link)));
    assert!(top.create_new(OsStr::new("link")).is_err());
    symlink(t.outside().join("made-through"), t.root().join("dangling")).unwrap();
    assert!(top.create_new(OsStr::new("dangling")).is_err());
    assert!(is_empty(&t.outside()));
    assert!(top.child(OsStr::new("made"), true).is_ok());
    assert!(t.root().join("made").is_dir());
}

#[tokio::test]
async fn without_a_root_every_endpoint_is_off() {
    let f = Arc::new(Files {
        root: None,
        upload: true,
        download: true,
        default_dir: "Downloads".into(),
        max_upload_bytes: 0,
    });
    assert_eq!(get_list(&f, "").await.0, StatusCode::FORBIDDEN);
    assert_eq!(get_file(&f, "").await.0, StatusCode::FORBIDDEN);
    assert_eq!(
        put(&f, &[("name", "a")], "x").await.0,
        StatusCode::FORBIDDEN
    );
}

#[test]
fn the_command_line_decides_whether_files_are_shared() {
    let off = Files::new(None, "upload,download", false, "Downloads", 0).unwrap();
    assert!(off.root.is_none());
    assert_eq!(off.hello()["upload"], false);
    assert_eq!(off.hello()["download"], false);
    let t = Tmp::new();
    let root = t.root();
    let on = Files::new(root.to_str(), "upload,download", false, "/Downloads/", 0).unwrap();
    assert_eq!(on.hello()["upload"], true);
    assert_eq!(on.hello()["download"], true);
    assert_eq!(on.hello()["dir"], "Downloads");
    let viewer = Files::new(root.to_str(), "upload,download", true, "Downloads", 0).unwrap();
    assert_eq!(viewer.hello()["upload"], false);
    let missing = t.0.join("missing");
    assert!(Files::new(missing.to_str(), "upload", false, "Downloads", 0).is_err());
    std::fs::write(t.0.join("file"), "f").unwrap();
    let file = t.0.join("file");
    assert!(Files::new(file.to_str(), "upload", false, "Downloads", 0).is_err());
}

#[tokio::test]
async fn a_path_past_the_limit_is_refused_before_anything_is_created() {
    let t = Tmp::new();
    let f = t.files("Downloads");
    let long = "d".repeat(200);
    let sub = vec![long.as_str(); 25].join("/");
    let (s, b) = put(&f, &[("name", "a.txt"), ("sub", &sub)], "deep").await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert!(is_empty(&t.root()), "nothing is created");
    assert_eq!(get_list(&f, &sub).await.0, StatusCode::BAD_REQUEST);
    let file = format!("{sub}/a.txt");
    assert_eq!(get_file(&f, &file).await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn without_a_name_lookup_uploads_and_downloads_are_refused() {
    let t = Tmp::new();
    std::fs::create_dir_all(t.root().join("Downloads/sub")).unwrap();
    std::fs::write(t.root().join("Downloads/a.txt"), "a").unwrap();
    let f = t.files("Downloads");
    beneath::LOOKUP_FAILS_UNDER.lock().unwrap().push(t.root());
    for path in ["", "Downloads", "Downloads/sub"] {
        let (s, b) = put(&f, &[("name", "new.txt"), ("path", path)], "x").await;
        assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR, "{path:?}: {b}");
    }
    for path in ["Downloads/a.txt", "Downloads", ""] {
        assert_eq!(
            get_file(&f, path).await.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            "{path:?}"
        );
    }
    assert_eq!(
        get_list(&f, "Downloads").await.0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    beneath::LOOKUP_FAILS_UNDER
        .lock()
        .unwrap()
        .retain(|r| r != &t.root());
    let left: Vec<_> = std::fs::read_dir(t.root().join("Downloads"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(left.len(), 2, "{left:?}");
    assert!(is_empty(&t.root().join("Downloads/sub")));
}

#[tokio::test]
async fn a_listing_stops_at_its_cap() {
    let t = Tmp::new();
    for i in 0..MAX_LISTED + 500 {
        std::fs::write(t.root().join(format!("f{i}")), "").unwrap();
    }
    let f = t.files("Downloads");
    let (s, list) = get_list(&f, "").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(list["entries"].as_array().unwrap().len(), MAX_LISTED);
    let top = Root::open(&t.root()).unwrap().dir(&[], false).unwrap();
    beneath::STATS.with(|n| n.set(0));
    assert_eq!(listed(&top).unwrap().len(), MAX_LISTED);
    assert_eq!(
        beneath::STATS.with(|n| n.get()),
        MAX_LISTED,
        "enumeration stops at the cap"
    );
}

#[test]
fn publishing_never_replaces_a_file() {
    let t = Tmp::new();
    std::fs::write(t.root().join("a.txt"), "first").unwrap();
    std::fs::write(t.root().join("b"), "second").unwrap();
    let top = Root::open(&t.root()).unwrap().dir(&[], false).unwrap();
    let e = top
        .publish(OsStr::new("b"), OsStr::new("a.txt"))
        .unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(t.root().join("a.txt")).unwrap(), b"first");
    assert_eq!(std::fs::read(t.root().join("b")).unwrap(), b"second");
    top.publish(OsStr::new("b"), OsStr::new("c")).unwrap();
    assert!(!t.root().join("b").exists());
    assert_eq!(std::fs::read(t.root().join("c")).unwrap(), b"second");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_uploads_of_one_name_never_overwrite_each_other() {
    const N: usize = 16;
    let t = Tmp::new();
    let f = t.files("Downloads");
    // Every upload has its temporary file open before any body ends, so all of them reach
    // the name choice and the publish together.
    let all_open = Arc::new(tokio::sync::Barrier::new(N));
    let uploads: Vec<_> = (0..N)
        .map(|i| {
            let (f, all_open) = (f.clone(), all_open.clone());
            tokio::spawn(async move {
                let body = futures_util::stream::once(async move {
                    Ok::<_, std::io::Error>(bytes::Bytes::from(format!("upload {i}")))
                })
                .chain(futures_util::stream::once(async move {
                    all_open.wait().await;
                    Ok(bytes::Bytes::new())
                }));
                let mut h = HeaderMap::new();
                h.insert(UPLOAD_HEADER, "1".parse().unwrap());
                let r = upload(
                    State(f),
                    q(&[("name", "a.txt")]),
                    h,
                    Body::from_stream(body),
                )
                .await;
                r.status()
            })
        })
        .collect();
    let mut saved = 0;
    for u in uploads {
        match u.await.unwrap() {
            StatusCode::OK => saved += 1,
            StatusCode::CONFLICT => {}
            s => panic!("{s}"),
        }
    }
    let mut contents: Vec<String> = std::fs::read_dir(t.root().join("Downloads"))
        .unwrap()
        .map(|e| std::fs::read_to_string(e.unwrap().path()).unwrap())
        .collect();
    contents.sort();
    contents.dedup();
    assert_eq!(contents.len(), saved, "{contents:?}");
    assert!(saved > 0);
}

#[tokio::test]
async fn a_cancelled_upload_leaves_nothing_behind() {
    let t = Tmp::new();
    let f = t.files("Downloads");
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(1);
    tx.send(Ok(bytes::Bytes::from_static(b"half")))
        .await
        .unwrap();
    let body = Body::from_stream(tokio_stream_from(rx));
    let mut h = HeaderMap::new();
    h.insert(UPLOAD_HEADER, "1".parse().unwrap());
    let up = upload(State(f.clone()), q(&[("name", "a.txt")]), h, body);
    let dir = t.root().join("Downloads");
    let waited = tokio::time::timeout(std::time::Duration::from_millis(500), up).await;
    assert!(waited.is_err(), "the upload waits for the rest of its body");
    assert!(dir.is_dir());
    assert!(
        is_empty(&dir),
        "{:?}",
        std::fs::read_dir(&dir).unwrap().collect::<Vec<_>>()
    );
    drop(tx);
}

fn tokio_stream_from<T: Send + 'static>(
    rx: tokio::sync::mpsc::Receiver<T>,
) -> impl futures_util::Stream<Item = T> + Send {
    futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|v| (v, rx)) })
}

#[tokio::test]
async fn a_folder_too_deep_fails_its_download_rather_than_truncating_it() {
    let t = Tmp::new();
    let deep: PathBuf = (0..MAX_DEPTH + 1).fold(t.root().join("top"), |p, i| p.join(i.to_string()));
    std::fs::create_dir_all(&deep).unwrap();
    let f = t.files("Downloads");
    let r = download(State(f.clone()), q(&[("path", "top")])).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert!(axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .is_err());
}

#[tokio::test]
async fn a_wide_folder_downloads_whole() {
    let t = Tmp::new();
    for i in 0..300 {
        let d = t.root().join("wide").join(format!("d{i:03}"));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("f"), i.to_string()).unwrap();
    }
    let f = t.files("Downloads");
    let (s, zipped) = get_file(&f, "wide").await;
    assert_eq!(s, StatusCode::OK);
    let z = zip::ZipArchive::new(std::io::Cursor::new(zipped)).unwrap();
    assert_eq!(z.file_names().filter(|n| n.ends_with("/f")).count(), 300);
}

#[test]
fn a_temporary_name_does_not_grow_with_the_file_name() {
    let (a, b) = (part_name().unwrap(), part_name().unwrap());
    assert_eq!(a.len(), PART_PREFIX.len() + 32 + ".part".len());
    assert_ne!(a, b);
    assert!(reserved(&a));
}

#[tokio::test]
async fn an_upload_cannot_take_a_temporary_name() {
    let t = Tmp::new();
    let f = t.files("Downloads");
    for name in [
        ".starfold-upload-0123.part",
        ".STARFOLD-Upload-x",
        ".starfold-upload-",
    ] {
        assert_eq!(
            put(&f, &[("name", name)], "x").await.0,
            StatusCode::BAD_REQUEST,
            "{name}"
        );
    }
    assert_eq!(
        put(&f, &[("name", ".starfold-uploads")], "x").await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn a_taken_name_gets_a_number_within_the_name_limit() {
    let t = Tmp::new();
    let f = t.files("Downloads");
    let long = format!("{}.txt", "a".repeat(MAX_NAME - 4));
    let wide = format!("{}b.txt", "é".repeat(125));
    for name in [&long, &wide] {
        assert_eq!(name.len(), MAX_NAME);
        let mut saved = Vec::new();
        for _ in 0..3 {
            let (s, b) = put(&f, &[("name", name)], "x").await;
            assert_eq!(s, StatusCode::OK, "{b}");
            let v: serde_json::Value = serde_json::from_str(&b).unwrap();
            saved.push(
                v["path"]
                    .as_str()
                    .unwrap()
                    .rsplit('/')
                    .next()
                    .unwrap()
                    .to_string(),
            );
        }
        assert_eq!(saved[0], *name);
        for n in &saved[1..] {
            assert!(n.len() <= MAX_NAME && n.ends_with(").txt"), "{n}");
        }
        assert_ne!(saved[1], saved[2]);
    }
}

/// The zip of a folder much wider than the descriptors a process may hold: run as a child
/// with RLIMIT_NOFILE lowered, by the test below.
#[test]
#[ignore]
fn zip_child_under_a_low_fd_limit() {
    let Some(dir) = std::env::var_os("STARFOLD_FD_CHILD") else {
        return;
    };
    let limit = libc::rlimit {
        rlim_cur: 64,
        rlim_max: 64,
    };
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
    let root = Root::open(Path::new(&dir)).unwrap();
    let top = root.dir(&["wide"], false).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let reader = std::thread::spawn(move || {
        let mut all = Vec::new();
        while let Some(chunk) = rx.blocking_recv() {
            let chunk: std::io::Result<bytes::Bytes> = chunk;
            all.extend_from_slice(&chunk.unwrap());
        }
        all
    });
    let out = ChunkWriter {
        tx,
        buf: Vec::with_capacity(CHUNK),
    };
    write_zip(top, "wide", out).unwrap();
    let zipped = reader.join().unwrap();
    let z = zip::ZipArchive::new(std::io::Cursor::new(zipped)).unwrap();
    println!(
        "FILES {}",
        z.file_names().filter(|n| n.ends_with("/f")).count()
    );
}

#[test]
fn a_zip_needs_one_descriptor_per_level_not_per_folder() {
    let t = Tmp::new();
    for i in 0..200 {
        let d = t.root().join("wide").join(format!("d{i:03}")).join("inner");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("f"), i.to_string()).unwrap();
    }
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "files::tests::zip_child_under_a_low_fd_limit",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("STARFOLD_FD_CHILD", t.root())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("FILES 200"), "{stdout}");
}

/// Uploads whose destination is `.0` wait here twice, after choosing it and before
/// publishing, until `.1` of them (or the test) have each time.
static BEFORE_PUBLISH: std::sync::Mutex<Option<(String, Arc<std::sync::Barrier>)>> =
    std::sync::Mutex::new(None);

/// Held for the whole of each test that sets [`BEFORE_PUBLISH`], so they run one at a time.
static HOOK_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub(super) fn before_publish(dest: &str) {
    let hook = BEFORE_PUBLISH.lock().unwrap().clone();
    if let Some((name, all_chosen)) = hook {
        if name == dest {
            all_chosen.wait();
            all_chosen.wait();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_uploads_publishing_one_name_get_one_save_and_one_conflict() {
    let _one_at_a_time = HOOK_TESTS.lock().await;
    let t = Tmp::new();
    let f = t.files("Downloads");
    let name = "collide-two-uploads.txt";
    *BEFORE_PUBLISH.lock().unwrap() = Some((name.into(), Arc::new(std::sync::Barrier::new(2))));
    let (a, b) = {
        let (fa, fb) = (f.clone(), f.clone());
        tokio::join!(
            tokio::spawn(async move { put(&fa, &[("name", name)], "first body").await }),
            tokio::spawn(async move { put(&fb, &[("name", name)], "second body").await })
        )
    };
    *BEFORE_PUBLISH.lock().unwrap() = None;
    let (a, b) = (a.unwrap(), b.unwrap());
    let mut codes = [a.0, b.0];
    codes.sort();
    assert_eq!(codes, [StatusCode::OK, StatusCode::CONFLICT], "{a:?} {b:?}");
    let saved = if a.0 == StatusCode::OK {
        "first body"
    } else {
        "second body"
    };
    let dir = t.root().join("Downloads");
    assert_eq!(std::fs::read_to_string(dir.join(name)).unwrap(), saved);
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        1,
        "the loser's temporary is gone"
    );
}

#[tokio::test]
async fn reserved_names_are_refused_in_every_component() {
    let t = Tmp::new();
    std::fs::create_dir_all(t.root().join("a")).unwrap();
    let f = t.files("Downloads");
    for sub in [".starfold-upload-x", "ok/.STARFOLD-UPLOAD-y"] {
        let (s, b) = put(&f, &[("name", "f.txt"), ("sub", sub)], "x").await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "sub {sub}: {b}");
    }
    for path in ["a/.starfold-upload-x", ".Starfold-Upload-y/a"] {
        let (s, b) = put(&f, &[("name", "f.txt"), ("path", path)], "x").await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "path {path}: {b}");
        assert_eq!(
            get_list(&f, path).await.0,
            StatusCode::BAD_REQUEST,
            "{path}"
        );
        assert_eq!(
            get_file(&f, path).await.0,
            StatusCode::BAD_REQUEST,
            "{path}"
        );
    }
}

#[tokio::test]
async fn an_upload_in_progress_is_never_listed_zipped_or_downloaded() {
    let t = Tmp::new();
    std::fs::create_dir_all(t.root().join("Downloads")).unwrap();
    std::fs::write(t.root().join("Downloads/done.txt"), "done").unwrap();
    let f = t.files("Downloads");
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(1);
    tx.send(Ok(bytes::Bytes::from_static(b"half")))
        .await
        .unwrap();
    let mut h = HeaderMap::new();
    h.insert(UPLOAD_HEADER, "1".parse().unwrap());
    let held = tokio::spawn(upload(
        State(f.clone()),
        q(&[("name", "big.bin")]),
        h,
        Body::from_stream(tokio_stream_from(rx)),
    ));
    let dir = t.root().join("Downloads");
    let temp = loop {
        let found = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .find(|n| reserved(n));
        if let Some(n) = found {
            break n.into_string().unwrap();
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    let (s, list) = get_list(&f, "Downloads").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(list["entries"].as_array().unwrap().len(), 1, "{list}");
    let (s, zipped) = get_file(&f, "Downloads").await;
    assert_eq!(s, StatusCode::OK);
    let z = zip::ZipArchive::new(std::io::Cursor::new(zipped)).unwrap();
    let names: Vec<&str> = z.file_names().collect();
    assert!(names.iter().all(|n| !n.contains(PART_PREFIX)), "{names:?}");
    assert!(names.iter().any(|n| n.ends_with("done.txt")), "{names:?}");
    assert_eq!(
        get_file(&f, &format!("Downloads/{temp}")).await.0,
        StatusCode::BAD_REQUEST
    );
    held.abort();
    let _ = held.await;
    drop(tx);
}

#[tokio::test]
async fn an_entry_the_filesystem_names_reserved_is_never_opened() {
    let t = Tmp::new();
    let held = t.root().join(".starfold-upload-held");
    std::fs::create_dir_all(&held).unwrap();
    std::fs::write(held.join("f"), "temp").unwrap();
    std::fs::write(t.root().join(".starfold-upload-0a.part"), "temp").unwrap();
    let root = Root::open(&t.root()).unwrap();
    let top = root.dir(&[], false).unwrap();
    // Opened by the handle layer itself, under the real name and under spellings some
    // filesystems equate with it (HFS+ ignores U+200C): never Ok.
    for name in [
        ".starfold-upload-held",
        ".\u{200C}starfold-upload-held",
        ".STARFOLD-UPLOAD-held",
    ] {
        assert!(top.child(OsStr::new(name), false).is_err(), "{name:?}");
        assert!(top.open(OsStr::new(name)).is_err(), "{name:?}");
    }
    assert!(matches!(
        top.child(OsStr::new(".starfold-upload-held"), false),
        Err(Refused::Reserved)
    ));
    assert!(matches!(
        top.open(OsStr::new(".starfold-upload-0a.part")),
        Err(Refused::Reserved)
    ));
    assert!(matches!(
        root.dir(&[".starfold-upload-held"], false),
        Err(Refused::Reserved)
    ));
    let f = t.files("Downloads");
    for path in [
        ".\u{200C}starfold-upload-held",
        ".\u{17F}tarfold-upload-held/f",
        "a\u{FEFF}b",
    ] {
        assert_eq!(
            get_list(&f, path).await.0,
            StatusCode::BAD_REQUEST,
            "{path:?}"
        );
        assert_eq!(
            get_file(&f, path).await.0,
            StatusCode::BAD_REQUEST,
            "{path:?}"
        );
    }
    for name in ["a\u{200B}.txt", ".\u{200C}starfold-upload-x"] {
        assert_eq!(
            put(&f, &[("name", name)], "x").await.0,
            StatusCode::BAD_REQUEST,
            "{name:?}"
        );
        assert_eq!(
            put(&f, &[("name", "ok.txt"), ("sub", name)], "x").await.0,
            StatusCode::BAD_REQUEST,
            "sub {name:?}"
        );
    }
    let (s, list) = get_list(&f, "").await;
    assert_eq!(s, StatusCode::OK);
    assert!(!list.to_string().contains("starfold-upload"), "{list}");
    let (s, zipped) = get_file(&f, "").await;
    assert_eq!(s, StatusCode::OK);
    let z = zip::ZipArchive::new(std::io::Cursor::new(zipped)).unwrap();
    assert!(
        z.file_names().all(|n| !n.contains("starfold-upload")),
        "{:?}",
        z.file_names().collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn a_root_moved_somewhere_deeper_is_measured_where_it_is_now() {
    let t = Tmp::new();
    let f = t.files("Downloads");
    let limit = libc::PATH_MAX as usize;
    let mut deep = t.0.join("moved");
    while deep.as_os_str().len() < limit - 400 {
        deep.push("m".repeat(100));
    }
    std::fs::create_dir_all(&deep).unwrap();
    let moved = deep.join("root");
    std::fs::rename(t.root(), &moved).unwrap();
    let sub = "s".repeat(200);
    let (s, b) = put(&f, &[("name", "a.txt"), ("sub", &sub)], "x").await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert!(is_empty(&moved), "nothing is created");
    std::fs::rename(&moved, t.root()).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_root_moved_during_an_upload_is_measured_again_before_publishing() {
    let _one_at_a_time = HOOK_TESTS.lock().await;
    let t = Tmp::new();
    let f = t.files("Downloads");
    let limit = libc::PATH_MAX as usize;
    let mut deep = t.0.join("moved");
    while deep.as_os_str().len() < limit - 200 {
        deep.push("m".repeat(100));
    }
    std::fs::create_dir_all(&deep).unwrap();
    let moved = deep.join("root");
    let name = format!("{}.txt", "a".repeat(MAX_NAME - 4));
    let gate = Arc::new(std::sync::Barrier::new(2));
    *BEFORE_PUBLISH.lock().unwrap() = Some((name.clone(), gate.clone()));
    let (fu, n) = (f.clone(), name.clone());
    let upload =
        tokio::spawn(async move { put(&fu, &[("name", n.as_str()), ("path", "")], "x").await });
    let (root, to) = (t.root(), moved.clone());
    tokio::task::spawn_blocking(move || {
        gate.wait();
        std::fs::rename(root, to).unwrap();
        gate.wait();
    })
    .await
    .unwrap();
    let (s, b) = upload.await.unwrap();
    *BEFORE_PUBLISH.lock().unwrap() = None;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert!(
        is_empty(&moved),
        "the temporary is gone and nothing is published"
    );
    std::fs::rename(&moved, t.root()).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_moved_during_an_upload_is_measured_where_it_is_now() {
    let _one_at_a_time = HOOK_TESTS.lock().await;
    let t = Tmp::new();
    std::fs::create_dir_all(t.root().join("d")).unwrap();
    let f = t.files("Downloads");
    let limit = libc::PATH_MAX as usize;
    let mut deep = t.0.join("moved");
    while deep.as_os_str().len() < limit - 200 {
        deep.push("m".repeat(100));
    }
    std::fs::create_dir_all(&deep).unwrap();
    let moved = deep.join("d");
    let name = format!("{}.txt", "b".repeat(MAX_NAME - 4));
    let gate = Arc::new(std::sync::Barrier::new(2));
    *BEFORE_PUBLISH.lock().unwrap() = Some((name.clone(), gate.clone()));
    let (fu, n) = (f.clone(), name.clone());
    let upload =
        tokio::spawn(async move { put(&fu, &[("name", n.as_str()), ("path", "d")], "x").await });
    let (from, to) = (t.root().join("d"), moved.clone());
    tokio::task::spawn_blocking(move || {
        gate.wait();
        std::fs::rename(from, to).unwrap();
        gate.wait();
    })
    .await
    .unwrap();
    let (s, b) = upload.await.unwrap();
    *BEFORE_PUBLISH.lock().unwrap() = None;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert!(
        is_empty(&moved),
        "the temporary is gone and nothing is published"
    );
}

#[test]
fn a_root_ending_in_a_separator_counts_it_once() {
    let root = Root::open(Path::new("/")).unwrap();
    let limit = libc::PATH_MAX as usize;
    // "/" + components joined by "/", NUL included: exactly the limit.
    let longest = limit - 1;
    let mut parts = Vec::new();
    let mut len = 0;
    while longest - len > 202 {
        parts.push("p".repeat(200));
        len += 201;
    }
    parts.push("p".repeat(longest - len - 1));
    let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
    assert!(root.fits(&parts, 0).is_ok());
    assert!(root.fits(&parts, 1).is_err());
}
