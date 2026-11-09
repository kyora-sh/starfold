//! HTTP + WebSocket front end. Serves the embedded web client and the media/input socket.

use crate::frame::{self, FrameError};
use crate::hub::{Client, Hub, Out, Phase, Proto};
use crate::input::InputEvent;
use crate::negotiate::ClientMsg;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use bytes::Bytes;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

#[derive(Clone)]
pub struct AppState {
    pub hub: Arc<Hub>,
    pub audio: bool,
    pub epoch: Instant,
    pub files: Arc<crate::files::Files>,
    pub mic: Option<std::sync::mpsc::Sender<crate::mic::MicMsg>>,
    /// Clients may resize the desktop to their window (--enable-resize).
    pub resize: bool,
    /// Viewers only watch: input, clipboard writes and the microphone are refused (--read-only).
    pub read_only: bool,
    pub recorder: Arc<crate::record::Recorder>,
    /// The web client's page, with its title filled in (`index_html`).
    pub index: Bytes,
    /// The page's icon and its content type (--favicon).
    pub favicon: Bytes,
    pub favicon_type: &'static str,
    /// The host's encoders, probed at startup.
    pub encoders: Arc<crate::encoder::caps::Registry>,
    /// Every WebSocket message is binary with a kind byte (--framed, see frame.rs).
    pub framed: bool,
}

/// The socket --listen-unix made, with what it holds for the server's lifetime: its
/// directory, opened once and used for every later step, and the lock that keeps a second
/// server off the same path. Shutdown removes that socket and nothing that has taken its place.
#[cfg(unix)]
#[derive(Debug)]
pub struct SocketFile {
    dir: std::os::fd::OwnedFd,
    name: std::ffi::CString,
    dev: libc::dev_t,
    ino: libc::ino_t,
    _lock: std::os::fd::OwnedFd,
}

#[cfg(unix)]
enum Peer {
    Live,
    Gone,
}

#[cfg(unix)]
fn cvt(r: libc::c_int) -> std::io::Result<libc::c_int> {
    if r < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

#[cfg(unix)]
fn cname(s: &std::ffi::OsStr) -> std::io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(s.as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "a NUL in the path"))
}

/// Opens `name` in `dir` (or a path, with `dir` None), refusing a symlink.
#[cfg(unix)]
fn open_at(
    dir: Option<&std::os::fd::OwnedFd>,
    name: &std::ffi::CStr,
    flags: libc::c_int,
) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let at = dir.map_or(libc::AT_FDCWD, |d| d.as_raw_fd());
    let fd = cvt(unsafe {
        libc::openat(
            at,
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600 as libc::c_uint,
        )
    })?;
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
}

/// `name` in `dir`, not following a symlink.
#[cfg(unix)]
fn stat_at(dir: &std::os::fd::OwnedFd, name: &std::ffi::CStr) -> std::io::Result<libc::stat> {
    use std::os::fd::AsRawFd;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    cvt(unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            &mut st,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    })?;
    Ok(st)
}

#[cfg(unix)]
fn is_socket(st: &libc::stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFSOCK
}

#[cfg(unix)]
fn unlink_at(
    dir: &std::os::fd::OwnedFd,
    name: &std::ffi::CStr,
    flags: libc::c_int,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    cvt(unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), flags) }).map(|_| ())
}

/// Starts a connection to the socket at `path` without waiting for it: the socket, and
/// 0 or the error `connect` gave.
#[cfg(unix)]
fn nonblocking_connect(path: &Path) -> std::io::Result<(std::os::fd::OwnedFd, i32)> {
    use std::io::Error;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= addr.sun_path.len() {
        return Err(Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} is too long for a unix socket", path.display()),
        ));
    }
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (d, s) in addr.sun_path.iter_mut().zip(bytes) {
        *d = *s as libc::c_char;
    }
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(Error::last_os_error());
    }
    let r = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            &addr as *const libc::sockaddr_un as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    };
    let errno = match r {
        0 => 0,
        _ => Error::last_os_error().raw_os_error().unwrap_or(-1),
    };
    Ok((fd, errno))
}

/// Whether a server still listens at `path`, without waiting on it: only a refused
/// connection says it is gone. A full backlog counts as live; anything else is an error.
#[cfg(unix)]
fn probe(path: &Path) -> std::io::Result<Peer> {
    match nonblocking_connect(path)?.1 {
        0 | libc::EINPROGRESS | libc::EAGAIN => Ok(Peer::Live),
        libc::ECONNREFUSED => Ok(Peer::Gone),
        e => Err(std::io::Error::from_raw_os_error(e)),
    }
}

/// Binds a unix socket at `path` with permissions `mode` (--listen-unix).
///
/// The directory is opened once and has to be one only this user can change (a real
/// directory it owns, without group or other write); every later step works on that open
/// directory rather than on the path, so swapping a directory on the way there changes
/// nothing. A lock next to the socket (`NAME.lock`, kept for the server's lifetime) turns a
/// second server away. The socket is bound in a 0700 directory made here (`.sf`), given its
/// mode there and then linked into place, which never replaces anything. A socket left by a
/// server that is gone is replaced; one that may still be in use, or a file that isn't a
/// socket, is not.
#[cfg(unix)]
pub fn bind_unix(
    path: &Path,
    mode: u32,
) -> std::io::Result<(tokio::net::UnixListener, SocketFile)> {
    use std::io::{Error, ErrorKind};
    use std::os::fd::AsRawFd;
    let invalid = |m: String| Error::new(ErrorKind::InvalidInput, m);
    let name = path
        .file_name()
        .ok_or_else(|| invalid(format!("{} has no file name", path.display())))?;
    let dir_path = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let staged = dir_path.join(".sf").join("s");
    let max = unsafe { std::mem::zeroed::<libc::sockaddr_un>() }
        .sun_path
        .len()
        - 1;
    for (p, what) in [
        (path, "the socket"),
        (staged.as_path(), "where it is set up first"),
    ] {
        if p.as_os_str().len() > max {
            return Err(invalid(format!(
                "{} is too long for a unix socket: {} ({}) is {} bytes, at most {max}",
                path.display(),
                what,
                p.display(),
                p.as_os_str().len()
            )));
        }
    }
    let name = cname(name)?;
    let not_private = || {
        Error::new(
            ErrorKind::PermissionDenied,
            format!(
                "{} has to be a directory (not a symlink) that this user owns and only it can write",
                dir_path.display()
            ),
        )
    };
    let dir = open_at(
        None,
        &cname(dir_path.as_os_str())?,
        libc::O_RDONLY | libc::O_DIRECTORY,
    )
    .map_err(|e| match e.raw_os_error() {
        Some(libc::ELOOP | libc::ENOTDIR) => not_private(),
        _ => e,
    })?;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    cvt(unsafe { libc::fstat(dir.as_raw_fd(), &mut st) })?;
    if st.st_uid != unsafe { libc::geteuid() } || st.st_mode & 0o022 != 0 {
        return Err(not_private());
    }
    let mut lock_name = name.as_bytes().to_vec();
    lock_name.extend_from_slice(b".lock");
    let lock = open_at(
        Some(&dir),
        &std::ffi::CString::new(lock_name).unwrap(),
        libc::O_RDWR | libc::O_CREAT,
    )?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } < 0 {
        let e = Error::last_os_error();
        return Err(match e.raw_os_error() {
            Some(libc::EWOULDBLOCK) => Error::new(
                ErrorKind::AddrInUse,
                format!("another server listens at {}", path.display()),
            ),
            _ => e,
        });
    }
    match stat_at(&dir, &name) {
        Ok(st) if is_socket(&st) => match probe(path)? {
            Peer::Live => {
                return Err(Error::new(
                    ErrorKind::AddrInUse,
                    format!("{} is in use by another server", path.display()),
                ))
            }
            Peer::Gone => unlink_at(&dir, &name, 0)?,
        },
        Ok(_) => {
            return Err(Error::new(
                ErrorKind::AlreadyExists,
                format!("{} exists and is not a socket", path.display()),
            ))
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    // `.sf` is shared by every socket in this directory, one setup at a time.
    let setup = open_at(Some(&dir), c".starfold.lock", libc::O_RDWR | libc::O_CREAT)?;
    cvt(unsafe { libc::flock(setup.as_raw_fd(), libc::LOCK_EX) })?;
    let clear_stage = || -> std::io::Result<()> {
        match open_at(Some(&dir), c".sf", libc::O_RDONLY | libc::O_DIRECTORY) {
            Ok(stage) => {
                match unlink_at(&stage, c"s", 0) {
                    Err(e) if e.kind() != ErrorKind::NotFound => return Err(e),
                    _ => {}
                }
                unlink_at(&dir, c".sf", libc::AT_REMOVEDIR)
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    };
    clear_stage()?;
    cvt(unsafe { libc::mkdirat(dir.as_raw_fd(), c".sf".as_ptr(), 0o700) })?;
    let placed = (|| {
        let stage = open_at(Some(&dir), c".sf", libc::O_RDONLY | libc::O_DIRECTORY)?;
        let listener = tokio::net::UnixListener::bind(&staged)?;
        // Only this user can make a file in `.sf`, so a socket there is the one just bound,
        // wherever the path led.
        let st = stat_at(&stage, c"s")
            .ok()
            .filter(is_socket)
            .ok_or_else(|| {
                Error::other(format!("{} moved while it was set up", dir_path.display()))
            })?;
        cvt(unsafe { libc::fchmodat(stage.as_raw_fd(), c"s".as_ptr(), mode as libc::mode_t, 0) })?;
        let linked = unsafe {
            libc::linkat(
                stage.as_raw_fd(),
                c"s".as_ptr(),
                dir.as_raw_fd(),
                name.as_ptr(),
                0,
            )
        };
        if linked < 0 {
            let e = Error::last_os_error();
            return Err(match e.raw_os_error() {
                Some(libc::EEXIST) => Error::new(
                    ErrorKind::AddrInUse,
                    format!("{} appeared while it was set up", path.display()),
                ),
                _ => e,
            });
        }
        Ok((listener, st.st_dev, st.st_ino))
    })();
    let cleared = clear_stage();
    let (listener, dev, ino) = placed?;
    cleared?;
    Ok((
        listener,
        SocketFile {
            dir,
            name,
            dev,
            ino,
            _lock: lock,
        },
    ))
}

/// Removes the socket `bind_unix` made, if it is still there.
#[cfg(unix)]
pub fn remove_unix(file: &SocketFile) {
    if let Ok(st) = stat_at(&file.dir, &file.name) {
        if is_socket(&st) && st.st_dev == file.dev && st.st_ino == file.ino {
            let _ = unlink_at(&file.dir, &file.name, 0);
        }
    }
}

/// The web client's page, titled `title` (--title): the browser tab, and the name of
/// the app if a viewer installs it.
pub fn index_html(title: &str) -> Bytes {
    let mut escaped = String::with_capacity(title.len());
    for c in title.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            c => escaped.push(c),
        }
    }
    Bytes::from(include_str!("../../../packages/web/index.html").replace("{{title}}", &escaped))
}

/// The page's icon (--favicon): that file's bytes and content type, or voxl's own icon.
pub fn favicon(path: Option<&str>) -> Result<(Bytes, &'static str), String> {
    let Some(path) = path else {
        return Ok((
            Bytes::from_static(include_bytes!("../../../packages/web/favicon.ico")),
            "image/x-icon",
        ));
    };
    let ctype = match Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("ico") => "image/x-icon",
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        _ => {
            return Err(format!(
                "--favicon {path}: expected an .ico, .png, .gif, .webp or .svg file"
            ))
        }
    };
    let bytes = std::fs::read(path).map_err(|e| format!("--favicon {path}: {e}"))?;
    Ok((Bytes::from(bytes), ctype))
}

async fn favicon_route(State(s): State<AppState>) -> Response {
    (
        [
            (header::CONTENT_TYPE, s.favicon_type),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        s.favicon,
    )
        .into_response()
}

async fn index(State(s): State<AppState>) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        s.index,
    )
        .into_response()
}

macro_rules! asset {
    ($name:literal, $ctype:literal) => {
        get(|| async {
            (
                [
                    (header::CONTENT_TYPE, $ctype),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                include_bytes!(concat!("../../../packages/web/", $name)).as_slice(),
            )
        })
    };
}

/// Scripts bundled from web/src by build.rs.
macro_rules! bundled {
    ($name:literal) => {
        get(|| async {
            (
                [
                    (header::CONTENT_TYPE, "text/javascript"),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                include_bytes!(concat!(env!("OUT_DIR"), "/web/", $name)).as_slice(),
            )
        })
    };
}

/// dav1d-wasm (packages/web/dav1d), fetched only by workers that decode AV1 with it.
async fn dav1d_wasm() -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/wasm"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        include_bytes!("../../../packages/web/dav1d/dav1d.wasm").as_slice(),
    )
        .into_response()
}

pub fn router(state: AppState) -> Router {
    use crate::files;
    let file_routes = Router::new()
        .route("/files/list", get(files::list))
        .route("/files/download", get(files::download))
        .route("/files/upload", axum::routing::put(files::upload))
        .with_state(state.files.clone());
    use crate::record;
    let record_routes = Router::new()
        .route("/screenshot", get(record::screenshot))
        .route("/record", get(record::record))
        .route("/record/start", axum::routing::post(record::start))
        .route("/record/stop", axum::routing::post(record::stop))
        .with_state(state.recorder.clone());
    let wait_routes = Router::new()
        .route("/wait", get(crate::wait::wait))
        .with_state(state.hub.clone());
    // VOXL_TEST_HOOKS: POST /debug/renegotiate moves every protocol 2 viewer to a new
    // epoch (a codec switch, even to the same codec), and POST /debug/fail-h264 makes the
    // H.264 encoder fall back to the next one, for end-to-end tests.
    let debug_routes: Router = if std::env::var_os("VOXL_TEST_HOOKS").is_some() {
        Router::new()
            .route("/debug/renegotiate", axum::routing::post(renegotiate))
            .route("/debug/fail-h264", axum::routing::post(fail_h264))
            .with_state(state.hub.clone())
    } else {
        Router::new()
    };
    Router::new()
        .route("/", get(index))
        .route("/index.html", get(index))
        .route("/client.js", bundled!("client.js"))
        .route("/worker.js", bundled!("worker.js"))
        .route("/dav1d.js", bundled!("dav1d.js"))
        .route("/dav1d.wasm", get(dav1d_wasm))
        .route("/audio.js", bundled!("audio.js"))
        .route("/mic.js", bundled!("mic.js"))
        .route("/favicon.ico", get(favicon_route))
        .route("/inter.woff2", asset!("inter.woff2", "font/woff2"))
        .route("/healthz", get(|| async { "ok" }))
        .route("/stats", get(stats))
        .route("/ws", get(ws_handler))
        .route("/input", axum::routing::post(crate::actions::input))
        .with_state(state)
        .merge(file_routes)
        .merge(record_routes)
        .merge(wait_routes)
        .merge(debug_routes)
}

async fn renegotiate(State(hub): State<Arc<Hub>>) -> Response {
    let mut n = 0;
    for c in hub.clients.lock().iter() {
        let mut r = c.route.lock();
        if r.proto == Proto::V2 && r.cohort.is_some() {
            r.detach();
            n += 1;
        }
    }
    hub.replan();
    (
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::json!({ "renegotiated": n }).to_string(),
    )
        .into_response()
}

async fn fail_h264(State(hub): State<Arc<Hub>>) -> axum::http::StatusCode {
    hub.fail_h264.store(true, Ordering::Release);
    hub.replan();
    axum::http::StatusCode::NO_CONTENT
}

/// A client's protocol and codec, for /stats.
fn route_stats(c: &Client) -> serde_json::Value {
    let r = c.route.lock();
    let protocol = match r.proto {
        Proto::Unknown => serde_json::Value::Null,
        Proto::Legacy => 1.into(),
        Proto::V2 => 2.into(),
    };
    let phase = match r.phase {
        Phase::Idle => "idle",
        Phase::Announced(_) => "announced",
        Phase::Ready => "ready",
        Phase::NoCodec => "no-codec",
    };
    serde_json::json!({
        "protocol": protocol,
        "cohort": r.cohort,
        "codec": r.choice.map(|c| c.cand.codec),
        "backend": r.choice.map(|c| c.cand.backend),
        "implementation": r.choice.and_then(|c| c.implementation),
        "epoch": r.epoch,
        "phase": phase,
    })
}

async fn stats(State(s): State<AppState>) -> Response {
    let st = &s.hub.stats;
    let clients: Vec<_> = s
        .hub
        .clients
        .lock()
        .iter()
        .map(|c| {
            let (n, age) = c.backlog();
            serde_json::json!({"id": c.id, "video": c.video.load(Ordering::Relaxed), "inflight": n, "oldest_ms": age,
                "min_rtt_ms": c.min_rtt_ms.load(Ordering::Relaxed), "queue_ms": c.delay.lock().queue_ms as u64, "delivered_kbps": (c.delivered.lock().rate_bps / 1000.0) as u64,
                "stream": route_stats(c)})
        })
        .collect();
    let body = serde_json::json!({
        "captured": st.captured.load(Ordering::Relaxed),
        "encoded": st.encoded.load(Ordering::Relaxed),
        "bytes": st.bytes.load(Ordering::Relaxed),
        "encode_us_total": st.encode_us.load(Ordering::Relaxed),
        "grab_us_total": st.grab_us.load(Ordering::Relaxed),
        "convert_us_total": st.convert_us.load(Ordering::Relaxed),
        "flow_blocked": st.flow_blocked.load(Ordering::Relaxed),
        "paintovers": st.paintovers.load(Ordering::Relaxed),
        "keyframes": st.keyframes.load(Ordering::Relaxed),
        "refines": st.refines.load(Ordering::Relaxed),
        "paintover_frames": st.paintover_frames.load(Ordering::Relaxed),
        "paintover_bytes": st.paintover_bytes.load(Ordering::Relaxed),
        "target_kbps": s.hub.target_kbps.load(Ordering::Relaxed),
        "damage_events": crate::capture::DAMAGE_EVENTS.load(Ordering::Relaxed),
        "damage_wait_us_total": st.damage_wait_us.load(Ordering::Relaxed),
        "send_lat_us_total": st.send_lat_us.load(Ordering::Relaxed),
        "clients": clients,
        "cohorts": &*s.hub.cohorts.lock(),
        "encoder": &*s.hub.encoder.lock(),
        "encoders": &*s.encoders,
    });
    (
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

async fn ws_handler(ws: WebSocketUpgrade, State(s): State<AppState>) -> Response {
    ws.max_message_size(frame::MAX_MESSAGE)
        .on_upgrade(move |socket| handle_socket(socket, s))
}

async fn handle_socket(socket: WebSocket, s: AppState) {
    use futures_util::{SinkExt, StreamExt};
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Out>();
    let (close_tx, mut close_rx) = oneshot::channel::<FrameError>();
    let framed = s.framed;
    let client = s.hub.add_client(tx);
    log::info!("client {} connected", client.id);
    client.send(Out::Text(s.hub.hello()));
    if let Some(c) = s.hub.cursor.lock().clone() {
        client.send(Out::Bin(c));
    }
    let mut writer = tokio::spawn(async move {
        let dbg = std::env::var("VOXL_DEBUG_MARKER").is_ok();
        loop {
            let m = tokio::select! {
                biased;
                Ok(e) = &mut close_rx => {
                    let _ = sink
                        .send(Message::Close(Some(CloseFrame {
                            code: e.close_code(),
                            reason: e.to_string().into(),
                        })))
                        .await;
                    break;
                }
                m = rx.recv() => match m {
                    Some(m) => m,
                    None => break,
                },
            };
            let t = Instant::now();
            let big = matches!(&m, Out::Bin(b) if b.len() > 2000);
            let msg = if framed {
                let f = match &m {
                    Out::Bin(b) => frame::encode_binary(b),
                    Out::Text(t) => frame::encode_text(t),
                };
                match f {
                    Ok(f) => Message::Binary(f),
                    Err(e) => {
                        log::warn!("not sent: {e}");
                        continue;
                    }
                }
            } else {
                match m {
                    Out::Bin(b) => Message::Binary(b),
                    Out::Text(t) => Message::Text(t.into()),
                }
            };
            if sink.send(msg).await.is_err() {
                break;
            }
            if dbg && big {
                log::info!(
                    "ws send took {} us (queued {})",
                    t.elapsed().as_micros(),
                    rx.len()
                );
            }
        }
    });
    let pinger = {
        let c = client.clone();
        let epoch = s.epoch;
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(Duration::from_millis(500));
            loop {
                iv.tick().await;
                let t = epoch.elapsed().as_micros() as u64;
                c.send(Out::Text(format!("{{\"t\":\"ping\",\"s\":{t}}}")));
            }
        })
    };
    let mut rtts: std::collections::VecDeque<u32> = Default::default();
    let mut refused = None;
    while let Some(msg) = stream.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                if framed && oversized(e) {
                    refused = Some(FrameError::Oversized);
                }
                break;
            }
        };
        let msg = if framed {
            match unframe(msg) {
                Ok(m) => m,
                Err(e) => {
                    refused = Some(e);
                    break;
                }
            }
        } else {
            msg
        };
        match msg {
            Message::Text(t) => handle_text(&s, &client, t.as_str(), &mut rtts),
            // Every binary message from a viewer acts on the desktop (clipboard images,
            // microphone audio), so a read-only viewer's are all dropped.
            Message::Binary(_) if s.read_only => {}
            // 0x11: clipboard image (PNG) pasted on the client.
            Message::Binary(b) if b.first() == Some(&0x11) && b.len() > 1 => {
                if let Some(tx) = s.hub.input.lock().as_ref() {
                    let _ = tx.send(InputEvent::Clipboard(crate::desktop::ClipContent::Image(
                        b[1..].to_vec(),
                    )));
                }
            }
            // 0x12: microphone Opus packet, 0x13: microphone PCM (s16le mono 48 kHz).
            Message::Binary(b) if matches!(b.first(), Some(0x12 | 0x13)) && b.len() > 1 => {
                if let Some(tx) = &s.mic {
                    let data = b.slice(1..);
                    let _ = tx.send(if b[0] == 0x12 {
                        crate::mic::MicMsg::Opus(client.id, data)
                    } else {
                        crate::mic::MicMsg::Pcm(client.id, data)
                    });
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    pinger.abort();
    if let Some(e) = refused {
        log::warn!("client {}: {e}; closing", client.id);
        let _ = close_tx.send(e);
        let _ = tokio::time::timeout(Duration::from_secs(1), &mut writer).await;
    }
    writer.abort();
    if let Some(tx) = s.hub.input.lock().as_ref() {
        let _ = tx.send(InputEvent::ReleaseAll);
    }
    if let Some(tx) = &s.mic {
        let _ = tx.send(crate::mic::MicMsg::Stop(client.id));
    }
    s.hub.remove_client(client.id);
    log::info!("client {} disconnected", client.id);
}

/// Whether the WebSocket refused a message for its size (more than `frame::MAX_MESSAGE`,
/// however it was fragmented).
fn oversized(e: axum::Error) -> bool {
    e.into_inner()
        .downcast_ref::<tungstenite::Error>()
        .is_some_and(|e| matches!(e, tungstenite::Error::Capacity(_)))
}

/// A framed message (--framed) as the message plain WebSockets would have carried.
fn unframe(m: Message) -> Result<Message, FrameError> {
    match m {
        Message::Binary(b) => Ok(match frame::decode(b)? {
            frame::Payload::Binary(b) => Message::Binary(b),
            frame::Payload::Text(t) => Message::Text(t.into()),
        }),
        Message::Text(_) => Err(FrameError::Unframed),
        m => Ok(m),
    }
}

fn num<T: std::str::FromStr>(it: &mut std::str::Split<'_, char>) -> Option<T> {
    it.next()?.trim().parse().ok()
}

/// Commands a --read-only viewer may send: acks, video and audio on/off, keyframe
/// requests and ping replies. Everything else acts on the desktop (input, clipboard,
/// microphone, resizing) and is dropped. The web client keeps the same list.
const READ_ONLY_COMMANDS: &[&str] = &["a", "v", "au", "kf", "po"];

/// The display rate a client measured, if it is a sensible number.
fn set_hz(c: &Client, hz: Option<serde_json::Value>) {
    if let Some(hz) = hz.and_then(|v| v.as_f64()).filter(|h| *h >= 1.0) {
        c.hz.store(hz.round().min(1000.0) as u32, Ordering::Relaxed);
    }
}

/// Protocol 2's JSON messages. All of them are about watching (capabilities, decoder
/// state, acks and keyframe requests), so read-only viewers send them too.
fn handle_json(s: &AppState, c: &Arc<Client>, m: ClientMsg) {
    match m {
        ClientMsg::Hello {
            v,
            offer,
            decode,
            hz,
        } if v >= crate::negotiate::PROTOCOL => {
            set_hz(c, hz);
            let gen = s.hub.offer_gen.load(Ordering::Relaxed);
            // It answers offers that have since changed; its answer to the new ones follows.
            if offer.is_some_and(|o| o != gen) {
                return;
            }
            c.caps_hello(crate::negotiate::decode_caps(&decode), gen);
            s.hub.replan();
        }
        ClientMsg::Hello { .. } => {}
        ClientMsg::Ready { epoch } => {
            if c.codec_ready(epoch) {
                s.hub.wake();
            }
        }
        ClientMsg::Error { epoch, reason } => {
            if let Some(ch) = c.codec_error(epoch) {
                log::warn!(
                    "client {}: {:?} from {} failed ({reason}); trying the next codec",
                    c.id,
                    ch.cand.codec,
                    ch.cand.backend.name()
                );
                s.hub.replan();
            }
        }
        ClientMsg::Ack { epoch, frame } => {
            if c.ack(Some(epoch), frame) {
                s.hub.wake();
            }
        }
        ClientMsg::Keyframe { epoch } => {
            if c.request_keyframe(Some(epoch)) {
                s.hub.wake();
            }
        }
        ClientMsg::Hz { hz } => {
            set_hz(c, hz);
            s.hub.wake();
        }
        ClientMsg::Slow => {
            if !c.slow.swap(true, Ordering::Relaxed) {
                log::info!(
                    "client {}: its decoder keeps falling behind; no more than 60 fps for it",
                    c.id
                );
            }
        }
    }
}

fn handle_text(s: &AppState, c: &Arc<Client>, t: &str, rtts: &mut std::collections::VecDeque<u32>) {
    if t.starts_with('{') {
        if let Some(m) = ClientMsg::parse(t) {
            handle_json(s, c, m);
        }
        return;
    }
    let (cmd, rest) = t.split_once(',').unwrap_or((t, ""));
    // --read-only: accept only what watching needs, right where commands are applied,
    // so a client that ignores the hello flag cannot drive the desktop, whatever
    // commands are added later.
    if s.read_only && !READ_ONLY_COMMANDS.contains(&cmd) {
        return;
    }
    let input = |ev: InputEvent| {
        if let Some(tx) = s.hub.input.lock().as_ref() {
            s.hub.note_input(&ev);
            let _ = tx.send(ev);
        }
    };
    let mut it = rest.split(',');
    match cmd {
        "a" => {
            if let Some(id) = num::<u32>(&mut it) {
                if c.ack(None, id) {
                    s.hub.wake();
                }
            }
        }
        "m" => {
            if let (Some(x), Some(y)) = (num(&mut it), num(&mut it)) {
                input(InputEvent::Move { x, y });
            }
        }
        "b" => {
            if let (Some(button), Some(d)) = (num::<u8>(&mut it), num::<u8>(&mut it)) {
                input(InputEvent::Button {
                    button,
                    down: d == 1,
                });
            }
        }
        "s" => {
            if let (Some(dx), Some(dy)) = (num(&mut it), num(&mut it)) {
                input(InputEvent::Scroll { dx, dy });
            }
        }
        "sp" => {
            if let (Some(dx), Some(dy)) = (num::<f64>(&mut it), num::<f64>(&mut it)) {
                input(InputEvent::ScrollPx { dx, dy });
            }
        }
        "k" => {
            if let (Some(keysym), Some(d)) = (num::<u32>(&mut it), num::<u8>(&mut it)) {
                input(InputEvent::Key {
                    keysym,
                    down: d == 1,
                });
            }
        }
        "r" => input(InputEvent::ReleaseAll),
        "c" => input(InputEvent::Clipboard(crate::desktop::ClipContent::Text(
            rest.to_string(),
        ))),
        "v" => {
            c.set_video(rest == "1");
            s.hub.replan();
        }
        "au" => c.audio.store(rest == "1" && s.audio, Ordering::Relaxed),
        "mi" => {
            if let Some(tx) = &s.mic {
                let _ = tx.send(if rest == "1" {
                    crate::mic::MicMsg::Start(c.id)
                } else {
                    crate::mic::MicMsg::Stop(c.id)
                });
            }
        }
        // sz,w,h: the desktop size this client's window wants (dynamic resolution).
        // The latest request wins; the video thread clamps it and applies it.
        "sz" if s.resize => {
            if let (Some(w), Some(h), Some(max)) = (
                num::<u32>(&mut it),
                num::<u32>(&mut it),
                *s.hub.resize_max.lock(),
            ) {
                let want = crate::screen::fit(w, h, max);
                let current = (
                    s.hub.width.load(Ordering::Relaxed),
                    s.hub.height.load(Ordering::Relaxed),
                );
                *s.hub.resize_req.lock() = (want != current).then_some(want);
                s.hub.wake();
            }
        }
        "kf" => {
            if c.request_keyframe(None) {
                s.hub.wake();
            }
        }
        "po" => {
            if let Some(sent) = num::<u64>(&mut it) {
                let now = s.epoch.elapsed().as_micros() as u64;
                let rtt = (now.saturating_sub(sent) / 1000) as u32;
                rtts.push_back(rtt.max(1));
                if rtts.len() > 20 {
                    rtts.pop_front();
                }
                c.min_rtt_ms
                    .store(*rtts.iter().min().unwrap(), Ordering::Relaxed);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dav1d_wasm_is_served_as_wasm() {
        let r = dav1d_wasm().await;
        assert_eq!(r.headers()[header::CONTENT_TYPE], "application/wasm");
        let body = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..4], b"\0asm");
    }

    /// Every command the web client sends (web/src/protocol.ts), with arguments.
    const ALL: &[&str] = &[
        "a,1",
        "m,10,20",
        "b,1,1",
        "b,1,0",
        "s,0,1",
        "sp,0.0,12.0",
        "k,65,1",
        "k,65,0",
        "r",
        "c,text",
        "v,1",
        "au,1",
        "mi,1",
        "sz,640,480",
        "kf",
        "po,1",
    ];

    fn app(
        hub: &Arc<Hub>,
        read_only: bool,
        mic: Option<std::sync::mpsc::Sender<crate::mic::MicMsg>>,
    ) -> AppState {
        AppState {
            hub: hub.clone(),
            audio: true,
            epoch: Instant::now(),
            files: Arc::new(crate::files::Files {
                root: std::env::temp_dir(),
                upload: false,
                download: false,
                default_dir: String::new(),
                max_upload_bytes: 0,
            }),
            mic,
            resize: true,
            read_only,
            recorder: crate::record::Recorder::new(hub.clone(), Duration::from_secs(1)),
            index: Bytes::new(),
            favicon: Bytes::new(),
            favicon_type: "image/x-icon",
            encoders: Default::default(),
            framed: false,
        }
    }

    /// Sends every command to a server that has input, a microphone and resizing wired
    /// up, and returns (input events, microphone messages, a resize was asked for).
    fn drive(read_only: bool) -> (usize, usize, bool) {
        let hub = Hub::new();
        hub.width.store(1280, Ordering::Relaxed);
        hub.height.store(800, Ordering::Relaxed);
        *hub.resize_max.lock() = Some((2560, 1600));
        let (input_tx, input_rx) = std::sync::mpsc::channel();
        *hub.input.lock() = Some(input_tx);
        let (mic_tx, mic_rx) = std::sync::mpsc::channel();
        let s = app(&hub, read_only, Some(mic_tx));
        let (tx, _rx) = mpsc::unbounded_channel();
        let client = hub.add_client(tx);
        let mut rtts = Default::default();
        for c in ALL {
            handle_text(&s, &client, c, &mut rtts);
        }
        let resized = hub.resize_req.lock().is_some();
        (
            input_rx.try_iter().count(),
            mic_rx.try_iter().count(),
            resized,
        )
    }

    #[test]
    fn only_keys_count_as_typing() {
        let hub = Hub::new();
        hub.width.store(1280, Ordering::Relaxed);
        hub.height.store(800, Ordering::Relaxed);
        let (input_tx, _input_rx) = std::sync::mpsc::channel();
        *hub.input.lock() = Some(input_tx);
        let s = app(&hub, false, None);
        let (tx, _rx) = mpsc::unbounded_channel();
        let client = hub.add_client(tx);
        let mut rtts = Default::default();
        for c in ["m,10,20", "b,1,1", "b,1,0", "s,0,1", "sp,0.0,12.0"] {
            handle_text(&s, &client, c, &mut rtts);
        }
        assert!(hub.input_at.lock().is_some(), "pointer input went unnoted");
        assert!(
            hub.key_at.lock().is_none(),
            "pointer input counted as typing"
        );
        handle_text(&s, &client, "k,65,1", &mut rtts);
        assert!(hub.key_at.lock().is_some(), "a key went unnoted");
    }

    #[test]
    fn read_only_viewers_cannot_act_on_the_desktop() {
        let (input, mic, resized) = drive(true);
        assert_eq!(input, 0, "input events from a read-only viewer");
        assert_eq!(mic, 0, "microphone messages from a read-only viewer");
        assert!(!resized, "a read-only viewer resized the desktop");
    }

    #[test]
    fn other_viewers_still_can() {
        // The same commands do reach the desktop without --read-only, so the test above
        // checks the guard rather than an unwired server.
        let (input, mic, resized) = drive(false);
        assert!(input >= 9, "{input} input events");
        assert_eq!(mic, 1);
        assert!(resized);
    }

    #[test]
    fn the_hello_says_the_display_rate_and_slow_takes_it_back() {
        let hub = Hub::new();
        let s = app(&hub, true, None);
        let mut rtts = Default::default();
        let (tx, _rx) = mpsc::unbounded_channel();
        let c = hub.add_client(tx);
        assert_eq!(c.display_hz(), None);
        let hello = |hz: &str| format!(r#"{{"t":"hello","v":2,"offer":0,"decode":[],"hz":{hz}}}"#);
        handle_text(&s, &c, &hello("119.88"), &mut rtts);
        assert_eq!(c.display_hz(), Some(120));
        handle_text(&s, &c, &hello("-5"), &mut rtts);
        assert_eq!(c.display_hz(), Some(120), "nonsense is ignored");
        let (tx, _rx) = mpsc::unbounded_channel();
        let odd = hub.add_client(tx);
        for hz in [r#""120""#, "null", "{}"] {
            odd.route.lock().proto = Proto::Unknown;
            handle_text(&s, &odd, &hello(hz), &mut rtts);
            assert_eq!(odd.route.lock().proto, Proto::V2, "the hello stands: {hz}");
            assert_eq!(odd.display_hz(), None, "{hz}");
        }
        handle_text(&s, &odd, r#"{"t":"hz","hz":143.9}"#, &mut rtts);
        assert_eq!(odd.display_hz(), Some(144), "measured after the hello");
        handle_text(&s, &c, r#"{"t":"slow"}"#, &mut rtts);
        assert_eq!(c.display_hz(), None);
    }

    #[test]
    fn protocol_is_told_by_the_first_message() {
        let hub = Hub::new();
        let s = app(&hub, true, None);
        let mut rtts = Default::default();
        let (tx, _rx) = mpsc::unbounded_channel();
        let old = hub.add_client(tx);
        handle_text(&s, &old, "v,1", &mut rtts);
        assert_eq!(old.route.lock().proto, Proto::Legacy);
        // Read-only viewers negotiate too: every protocol 2 message is about watching.
        let (tx, _rx) = mpsc::unbounded_channel();
        let new = hub.add_client(tx);
        let caps = r#"{"t":"hello","v":2,"offer":0,"decode":[{"codec":"avc1.42c01f","codedWidth":640,"codedHeight":480,"implementation":"webcodecs"}]}"#;
        handle_text(&s, &new, caps, &mut rtts);
        handle_text(&s, &new, "v,1", &mut rtts);
        let r = new.route.lock();
        assert_eq!(r.proto, Proto::V2);
        assert_eq!(r.caps.len(), 1);
        drop(r);
        assert!(new.video.load(Ordering::Relaxed));
        // An answer to an older offer generation is ignored.
        let (tx, _rx) = mpsc::unbounded_channel();
        let late = hub.add_client(tx);
        handle_text(
            &s,
            &late,
            &caps.replace("\"offer\":0", "\"offer\":9"),
            &mut rtts,
        );
        assert_eq!(late.route.lock().proto, Proto::Unknown);
        // A v2 ack is ignored from a protocol 1 viewer, and `a,` from a protocol 2 one.
        old.inflight.lock().push_back(crate::hub::Inflight {
            id: 1,
            sent: Instant::now(),
            bytes: 1,
        });
        handle_text(&s, &old, r#"{"t":"ack","epoch":0,"frame":1}"#, &mut rtts);
        assert_eq!(old.inflight.lock().len(), 1);
        handle_text(&s, &old, "a,1", &mut rtts);
        assert_eq!(old.inflight.lock().len(), 0);
        new.inflight.lock().push_back(crate::hub::Inflight {
            id: 1,
            sent: Instant::now(),
            bytes: 1,
        });
        handle_text(&s, &new, "a,1", &mut rtts);
        assert_eq!(new.inflight.lock().len(), 1);
    }

    #[test]
    fn favicon_uses_file_extension_for_content_type() {
        let (bytes, content_type) = favicon(None).unwrap();
        assert_eq!(content_type, "image/x-icon");
        assert!(!bytes.is_empty());

        let dir = std::env::temp_dir().join(format!("voxl-favicon-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("icon.PNG");
        std::fs::write(&png, b"\\x89PNG").unwrap();
        let (bytes, content_type) = favicon(Some(png.to_str().unwrap())).unwrap();
        assert_eq!(content_type, "image/png");
        assert_eq!(&bytes[..], b"\\x89PNG");
        assert!(favicon(Some(dir.join("icon.txt").to_str().unwrap())).is_err());
        assert!(favicon(Some(dir.join("missing.ico").to_str().unwrap())).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    mod socket {
        use super::*;
        use crate::frame::Payload;
        use futures_util::{SinkExt, StreamExt};
        use std::future::IntoFuture;
        use std::os::unix::fs::PermissionsExt;
        use tokio::io::{AsyncRead, AsyncWrite};
        use tokio_tungstenite::tungstenite::Message as Ws;
        use tokio_tungstenite::WebSocketStream;

        fn dir(name: &str) -> std::path::PathBuf {
            let d = std::env::temp_dir().join(format!("sf-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
            d
        }

        fn runtime() -> tokio::runtime::Runtime {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
        }

        fn wired() -> (Arc<Hub>, std::sync::mpsc::Receiver<InputEvent>) {
            let hub = Hub::new();
            hub.width.store(1280, Ordering::Relaxed);
            hub.height.store(800, Ordering::Relaxed);
            let (tx, rx) = std::sync::mpsc::channel();
            *hub.input.lock() = Some(tx);
            (hub, rx)
        }

        async fn raw<S: AsyncRead + AsyncWrite + Unpin>(ws: &mut WebSocketStream<S>) -> Ws {
            tokio::time::timeout(Duration::from_secs(5), ws.next())
                .await
                .expect("nothing within 5 s")
                .expect("the server hung up")
                .expect("a WebSocket error")
        }

        /// The next message other than a ping, as plain WebSockets would carry it.
        async fn next<S: AsyncRead + AsyncWrite + Unpin>(
            ws: &mut WebSocketStream<S>,
            framed: bool,
        ) -> Payload {
            loop {
                let p = match (raw(ws).await, framed) {
                    (Ws::Binary(b), true) => frame::decode(b).unwrap(),
                    (Ws::Binary(b), false) => Payload::Binary(b),
                    (Ws::Text(t), false) => Payload::Text(t.to_string()),
                    (m, _) => panic!("unexpected {m:?}"),
                };
                if !matches!(&p, Payload::Text(t) if t.starts_with(r#"{"t":"ping""#)) {
                    return p;
                }
            }
        }

        async fn close_code<S: AsyncRead + AsyncWrite + Unpin>(ws: &mut WebSocketStream<S>) -> u16 {
            loop {
                if let Ws::Close(c) = raw(ws).await {
                    return c.map_or(0, |c| c.code.into());
                }
            }
        }

        async fn key(
            rx: std::sync::mpsc::Receiver<InputEvent>,
        ) -> (bool, std::sync::mpsc::Receiver<InputEvent>) {
            tokio::task::spawn_blocking(move || {
                let ev = rx.recv_timeout(Duration::from_secs(5));
                (
                    matches!(
                        ev,
                        Ok(InputEvent::Key {
                            keysym: 65,
                            down: true
                        })
                    ),
                    rx,
                )
            })
            .await
            .unwrap()
        }

        async fn gone(hub: &Hub) {
            for _ in 0..500 {
                if hub.clients.lock().is_empty() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("the client was never removed");
        }

        #[tokio::test]
        async fn a_framed_session_over_a_unix_socket() {
            let (hub, input) = wired();
            let mut s = app(&hub, false, None);
            s.framed = true;
            let d = dir("framed");
            let path = d.join("s.sock");
            let (l, _) = bind_unix(&path, 0o600).unwrap();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            let server = tokio::spawn(axum::serve(l, router(s)).into_future());
            let sock = tokio::net::UnixStream::connect(&path).await.unwrap();
            let (mut ws, _) = tokio_tungstenite::client_async("ws://localhost/ws", sock)
                .await
                .unwrap();

            let Payload::Text(hello) = next(&mut ws, true).await else {
                panic!("the hello is text");
            };
            assert_eq!(hello, hub.hello());
            let v: serde_json::Value = serde_json::from_str(&hello).unwrap();
            assert_eq!(
                (v["t"].as_str(), v["w"].as_u64()),
                (Some("hello"), Some(1280))
            );

            let video = Bytes::from_static(&[0x05, 1, 0, 0, 0, 7, 0xaa, 0xbb]);
            hub.clients.lock()[0].send(Out::Bin(video.clone()));
            assert_eq!(next(&mut ws, true).await, Payload::Binary(video));

            ws.send(Ws::Binary(frame::encode_text("k,65,1").unwrap()))
                .await
                .unwrap();
            let (typed, input) = key(input).await;
            assert!(typed, "the key never reached the desktop");

            ws.close(None).await.unwrap();
            gone(&hub).await;
            let released =
                tokio::task::spawn_blocking(move || input.recv_timeout(Duration::from_secs(5)))
                    .await
                    .unwrap();
            assert!(matches!(released, Ok(InputEvent::ReleaseAll)));
            server.abort();
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[tokio::test]
        async fn a_framed_socket_closes_on_text_and_unknown_kinds() {
            let (hub, input) = wired();
            let mut s = app(&hub, false, None);
            s.framed = true;
            let d = dir("refuse");
            let path = d.join("s.sock");
            let server = tokio::spawn(
                axum::serve(bind_unix(&path, 0o600).unwrap().0, router(s)).into_future(),
            );
            for (bad, code) in [
                (Ws::Text("k,65,1".into()), 1003),
                (Ws::Binary(Bytes::from_static(&[9, b'k'])), 1003),
                (Ws::Binary(Bytes::new()), 1003),
                (Ws::Binary(Bytes::from_static(&[1, 0xff])), 1007),
            ] {
                let sock = tokio::net::UnixStream::connect(&path).await.unwrap();
                let (mut ws, _) = tokio_tungstenite::client_async("ws://localhost/ws", sock)
                    .await
                    .unwrap();
                next(&mut ws, true).await;
                ws.send(bad).await.unwrap();
                assert_eq!(close_code(&mut ws).await, code);
                gone(&hub).await;
            }
            let events: Vec<_> = input.try_iter().collect();
            assert!(
                events.iter().all(|e| matches!(e, InputEvent::ReleaseAll)),
                "a refused message acted on the desktop"
            );
            server.abort();
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[tokio::test]
        async fn a_fragmented_message_over_the_limit_closes_with_1009() {
            use tokio_tungstenite::tungstenite::protocol::frame::coding::{Data, OpCode};
            use tokio_tungstenite::tungstenite::protocol::frame::Frame;
            let (hub, _input) = wired();
            let mut s = app(&hub, false, None);
            s.framed = true;
            let d = dir("oversized");
            let path = d.join("s.sock");
            let (l, file) = bind_unix(&path, 0o600).unwrap();
            let server = tokio::spawn(axum::serve(l, router(s)).into_future());
            let sock = tokio::net::UnixStream::connect(&path).await.unwrap();
            let (mut ws, _) = tokio_tungstenite::client_async("ws://localhost/ws", sock)
                .await
                .unwrap();
            next(&mut ws, true).await;
            // Five 15 MiB fragments: each under the 16 MiB frame limit, 75 MiB together.
            let part = Bytes::from(vec![0u8; 15 << 20]);
            for i in 0..5 {
                let op = if i == 0 { Data::Binary } else { Data::Continue };
                let f = Frame::message(part.clone(), OpCode::Data(op), i == 4);
                if ws.send(Ws::Frame(f)).await.is_err() {
                    break;
                }
            }
            assert_eq!(close_code(&mut ws).await, 1009);
            gone(&hub).await;
            server.abort();
            drop(file);
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[tokio::test]
        async fn tcp_without_framed_is_unchanged() {
            let (hub, input) = wired();
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = l.local_addr().unwrap().port();
            let server = tokio::spawn(axum::serve(l, router(app(&hub, false, None))).into_future());
            let sock = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            let (mut ws, _) =
                tokio_tungstenite::client_async(format!("ws://127.0.0.1:{port}/ws"), sock)
                    .await
                    .unwrap();
            let Ws::Text(hello) = raw(&mut ws).await else {
                panic!("the hello is a text message");
            };
            assert_eq!(hello.as_str(), hub.hello());
            let video = Bytes::from_static(&[0x05, 1, 0, 0, 0, 7]);
            hub.clients.lock()[0].send(Out::Bin(video.clone()));
            assert_eq!(next(&mut ws, false).await, Payload::Binary(video));
            ws.send(Ws::Text("k,65,1".into())).await.unwrap();
            assert!(key(input).await.0);
            ws.close(None).await.unwrap();
            gone(&hub).await;
            server.abort();
        }

        #[test]
        fn a_unix_socket_replaces_only_a_dead_one() {
            let d = dir("bind");
            let path = d.join("s.sock");
            let rt = runtime();
            let _guard = rt.enter();
            drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
            let (l, _) = bind_unix(&path, 0o660).unwrap();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o660);
            let e = bind_unix(&path, 0o600).unwrap_err();
            assert_eq!(e.kind(), std::io::ErrorKind::AddrInUse);
            drop(l);
            let file = d.join("plain");
            std::fs::write(&file, b"x").unwrap();
            assert_eq!(
                bind_unix(&file, 0o600).unwrap_err().kind(),
                std::io::ErrorKind::AlreadyExists
            );
            assert_eq!(std::fs::read(&file).unwrap(), b"x");
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn a_socket_that_may_still_be_in_use_is_kept() {
            // Root connects whatever the mode, so only another user sees the refusal.
            if unsafe { libc::geteuid() } == 0 {
                return;
            }
            let d = dir("denied");
            let path = d.join("s.sock");
            let rt = runtime();
            let _guard = rt.enter();
            let live = std::os::unix::net::UnixListener::bind(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
            let e = bind_unix(&path, 0o600).unwrap_err();
            assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied, "{e}");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            std::os::unix::net::UnixStream::connect(&path).expect("the live socket is gone");
            drop(live);
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn a_full_backlog_is_a_live_server_and_is_not_waited_on() {
            use std::os::fd::AsRawFd;
            let d = dir("backlog");
            let path = d.join("s.sock");
            let live = std::os::unix::net::UnixListener::bind(&path).unwrap();
            assert_eq!(unsafe { libc::listen(live.as_raw_fd(), 0) }, 0);
            let mut queued = Vec::new();
            loop {
                let (fd, e) = nonblocking_connect(&path).unwrap();
                if e == libc::EAGAIN {
                    break;
                }
                assert_eq!(e, 0);
                queued.push(fd);
                assert!(queued.len() < 64, "the backlog never filled");
            }
            let (tx, rx) = std::sync::mpsc::channel();
            let p = path.clone();
            std::thread::spawn(move || {
                let rt = runtime();
                let _guard = rt.enter();
                let _ = tx.send(bind_unix(&p, 0o600).map(|_| ()));
            });
            let r = rx
                .recv_timeout(Duration::from_secs(5))
                .expect("binding waited on a server that doesn't accept");
            assert_eq!(r.unwrap_err().kind(), std::io::ErrorKind::AddrInUse);
            assert!(path.exists());
            drop((queued, live));
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn binding_leaves_the_umask_alone() {
            use std::sync::atomic::AtomicBool;
            let umask = || {
                std::fs::read_to_string("/proc/self/status")
                    .unwrap()
                    .lines()
                    .find_map(|l| l.strip_prefix("Umask:").map(|v| v.trim().to_string()))
                    .unwrap()
            };
            let before = umask();
            let stop = Arc::new(AtomicBool::new(false));
            let watcher = {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    let mut seen = std::collections::BTreeSet::new();
                    while !stop.load(Ordering::Relaxed) {
                        seen.insert(umask());
                    }
                    seen
                })
            };
            let d = dir("umask");
            let rt = runtime();
            let _guard = rt.enter();
            for i in 0..1000 {
                let path = d.join(format!("s{i}"));
                drop(bind_unix(&path, 0o666).unwrap());
                std::fs::remove_file(&path).unwrap();
            }
            stop.store(true, Ordering::Relaxed);
            let seen = watcher.join().unwrap();
            assert_eq!(seen.into_iter().collect::<Vec<_>>(), vec![before]);
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn only_a_private_directory_takes_the_socket() {
            let rt = runtime();
            let _guard = rt.enter();
            let d = dir("shared");
            for mode in [0o777, 0o770, 0o1777] {
                std::fs::set_permissions(&d, std::fs::Permissions::from_mode(mode)).unwrap();
                let e = bind_unix(&d.join("s.sock"), 0o600).unwrap_err();
                assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied, "{mode:o}");
                assert!(!d.join("s.sock").exists());
            }
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
            let target = dir("target");
            std::os::unix::fs::symlink(&target, d.join("via")).unwrap();
            let e = bind_unix(&d.join("via").join("s.sock"), 0o600).unwrap_err();
            assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied);
            assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
            std::fs::remove_dir_all(&d).unwrap();
            std::fs::remove_dir_all(&target).unwrap();
        }

        #[test]
        fn shutdown_removes_only_our_socket() {
            let rt = runtime();
            let _guard = rt.enter();
            let d = dir("cleanup");
            let path = d.join("s.sock");
            let (l, file) = bind_unix(&path, 0o600).unwrap();
            assert!(
                !d.join(".sf").exists(),
                "the staging directory was left behind"
            );
            drop(l);
            std::fs::remove_file(&path).unwrap();
            std::fs::write(&path, b"x").unwrap();
            remove_unix(&file);
            assert_eq!(std::fs::read(&path).unwrap(), b"x");
            std::fs::remove_file(&path).unwrap();
            drop(file);
            let (l, file) = bind_unix(&path, 0o600).unwrap();
            remove_unix(&file);
            assert!(!path.exists());
            drop((l, file));
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn shutdown_works_in_the_directory_it_bound_in_wherever_the_path_now_leads() {
            let rt = runtime();
            let _guard = rt.enter();
            let d = dir("moved");
            let away = dir("moved-away");
            std::fs::remove_dir(&away).unwrap();
            let path = d.join("s.sock");
            let (l, file) = bind_unix(&path, 0o600).unwrap();
            std::fs::rename(&d, &away).unwrap();
            std::fs::create_dir(&d).unwrap();
            drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
            remove_unix(&file);
            assert!(path.exists(), "removed what now sits at the path");
            assert!(!away.join("s.sock").exists(), "left its own socket");
            drop((l, file));
            std::fs::remove_dir_all(&d).unwrap();
            std::fs::remove_dir_all(&away).unwrap();
        }

        #[test]
        fn a_second_server_on_the_path_is_turned_away() {
            let rt = runtime();
            let _guard = rt.enter();
            let d = dir("second");
            let path = d.join("s.sock");
            let (l, file) = bind_unix(&path, 0o600).unwrap();
            std::fs::remove_file(&path).unwrap();
            let e = bind_unix(&path, 0o600).unwrap_err();
            assert_eq!(e.kind(), std::io::ErrorKind::AddrInUse, "{e}");
            drop((l, file));
            drop(bind_unix(&path, 0o600).unwrap());
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn servers_starting_together_get_one_socket() {
            let d = dir("race");
            for round in 0..20 {
                let path = d.join(format!("s{round}.sock"));
                let go = Arc::new(std::sync::Barrier::new(6));
                let (tx, rx) = std::sync::mpsc::channel();
                let threads: Vec<_> = (0..6)
                    .map(|_| {
                        let (go, tx, path) = (go.clone(), tx.clone(), path.clone());
                        std::thread::spawn(move || {
                            let rt = runtime();
                            let _guard = rt.enter();
                            go.wait();
                            let r = bind_unix(&path, 0o600);
                            let won = r.as_ref().is_ok_and(|(_, f)| {
                                let m = std::fs::symlink_metadata(&path).unwrap();
                                use std::os::unix::fs::MetadataExt;
                                m.ino() == f.ino
                            });
                            tx.send((r.is_ok(), won)).unwrap();
                            // Hold on until everyone has tried.
                            std::thread::sleep(Duration::from_millis(300));
                            drop(r);
                        })
                    })
                    .collect();
                drop(tx);
                let results: Vec<_> = rx.iter().collect();
                for t in threads {
                    t.join().unwrap();
                }
                let ok = results.iter().filter(|r| r.0).count();
                assert_eq!(ok, 1, "round {round}: {results:?}");
                assert!(
                    results.iter().any(|r| r.1),
                    "round {round}: the path isn't the winner's"
                );
            }
            std::fs::remove_dir_all(&d).unwrap();
        }

        fn max_path() -> usize {
            unsafe { std::mem::zeroed::<libc::sockaddr_un>() }
                .sun_path
                .len()
                - 1
        }

        #[test]
        fn a_path_too_long_for_a_socket_is_refused_up_front() {
            let rt = runtime();
            let _guard = rt.enter();
            let d = dir("long");
            let base = d.as_os_str().len() + 1;
            let path = d.join("a".repeat(max_path() + 1 - base));
            assert_eq!(path.as_os_str().len(), max_path() + 1);
            let e = bind_unix(&path, 0o600).unwrap_err();
            assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput, "{e}");
            assert_eq!(std::fs::read_dir(&d).unwrap().count(), 0);
            let path = d.join("a".repeat(max_path() - base));
            let (l, file) = bind_unix(&path, 0o600).expect("the longest path that fits");
            std::os::unix::net::UnixStream::connect(&path).unwrap();
            drop((l, file));
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn a_short_name_whose_setup_path_is_too_long_is_refused_clearly() {
            let rt = runtime();
            let _guard = rt.enter();
            let d = dir("stage");
            // `.sf/s` is 6 bytes past the directory, `/abc` 4.
            let sub = d.join("d".repeat(max_path() - 5 - d.as_os_str().len() - 1));
            std::fs::create_dir(&sub).unwrap();
            std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o700)).unwrap();
            let path = sub.join("abc");
            assert!(path.as_os_str().len() <= max_path());
            let e = bind_unix(&path, 0o600).unwrap_err();
            assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput, "{e}");
            assert!(e.to_string().contains("set up first"), "{e}");
            assert_eq!(std::fs::read_dir(&sub).unwrap().count(), 0);
            std::fs::remove_dir_all(&d).unwrap();
        }
    }
}
