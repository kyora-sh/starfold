use super::*;
use std::os::unix::fs::symlink;
use std::path::PathBuf;

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
    let f = t.files("Downloads");
    for p in [
        "file-out",
        "file-in",
        "dir-out",
        "dir-out/secret",
        "folder/file-out",
        "dir-in",
        "dir-in/kept",
    ] {
        assert_eq!(get_file(&f, p).await.0, StatusCode::FORBIDDEN, "{p}");
    }
    assert_eq!(get_list(&f, "dir-in").await.0, StatusCode::FORBIDDEN);
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
