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

/// The directory --listen-unix made and the socket in it, with what shutdown needs to
/// remove exactly those: both directories as opened, and each entry's device and inode.
#[cfg(unix)]
#[derive(Debug)]
pub struct SocketFile {
    parent: std::os::fd::OwnedFd,
    name: std::ffi::CString,
    dir: std::os::fd::OwnedFd,
    dir_id: (libc::dev_t, libc::ino_t),
    socket_id: Option<(libc::dev_t, libc::ino_t)>,
}

/// The socket's name in the --listen-unix directory.
#[cfg(unix)]
const SOCKET: &std::ffi::CStr = c"s";

/// Where clients connect for --listen-unix `dir`.
#[cfg(unix)]
pub fn unix_socket_path(dir: &Path) -> std::path::PathBuf {
    dir.join(SOCKET.to_str().unwrap())
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
        )
    })?;
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
}

#[cfg(unix)]
fn fstat(fd: &std::os::fd::OwnedFd) -> std::io::Result<libc::stat> {
    use std::os::fd::AsRawFd;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    cvt(unsafe { libc::fstat(fd.as_raw_fd(), &mut st) })?;
    Ok(st)
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
fn id(st: &libc::stat) -> (libc::dev_t, libc::ino_t) {
    (st.st_dev, st.st_ino)
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

#[cfg(unix)]
fn acl_left() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "the new directory kept an ACL",
    )
}

/// Removes the POSIX ACLs a new directory inherits from its parent's default ACL, and
/// checks none is left: its mode bits then say who reaches it and what is made in it.
#[cfg(target_os = "linux")]
fn clear_acl(dir: &std::os::fd::OwnedFd) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let absent = |e: std::io::Error| match e.raw_os_error() {
        Some(libc::ENODATA | libc::EOPNOTSUPP) => Ok(()),
        _ => Err(e),
    };
    for name in [c"system.posix_acl_default", c"system.posix_acl_access"] {
        if unsafe { libc::fremovexattr(dir.as_raw_fd(), name.as_ptr()) } < 0 {
            absent(std::io::Error::last_os_error())?;
        }
        let r = unsafe { libc::fgetxattr(dir.as_raw_fd(), name.as_ptr(), std::ptr::null_mut(), 0) };
        if r >= 0 {
            return Err(acl_left());
        }
        absent(std::io::Error::last_os_error())?;
    }
    Ok(())
}

/// Replaces the extended ACL a new directory inherits (which grants access whatever the
/// mode bits say) with an empty one, and checks it reads back empty.
#[cfg(target_os = "macos")]
fn clear_acl(dir: &std::os::fd::OwnedFd) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::raw::{c_int, c_void};
    extern "C" {
        fn acl_init(count: c_int) -> *mut c_void;
        fn acl_set_fd(fd: c_int, acl: *mut c_void) -> c_int;
        fn acl_get_fd(fd: c_int) -> *mut c_void;
        fn acl_get_entry(acl: *mut c_void, entry_id: c_int, entry: *mut *mut c_void) -> c_int;
        fn acl_free(obj: *mut c_void) -> c_int;
    }
    const ACL_FIRST_ENTRY: c_int = 0;
    unsafe {
        let empty = acl_init(0);
        if empty.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let set = acl_set_fd(dir.as_raw_fd(), empty);
        let e = std::io::Error::last_os_error();
        acl_free(empty);
        if set < 0 {
            return Err(e);
        }
        let acl = acl_get_fd(dir.as_raw_fd());
        if acl.is_null() {
            let e = std::io::Error::last_os_error();
            return match e.raw_os_error() {
                Some(libc::ENOENT) => Ok(()),
                _ => Err(e),
            };
        }
        let mut entry = std::ptr::null_mut();
        let any = acl_get_entry(acl, ACL_FIRST_ENTRY, &mut entry) == 0;
        acl_free(acl);
        if any {
            Err(acl_left())
        } else {
            Ok(())
        }
    }
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn clear_acl(_: &std::os::fd::OwnedFd) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "unix sockets need Linux or macOS",
    ))
}

/// Serves --listen-unix: makes the directory `path` (it must not exist yet; its parent
/// must) for this user alone and binds a socket named `s` in it with permissions `mode`.
/// Every step after the mkdir works on the directory as opened, and nothing that already
/// existed is used, replaced or removed: a path that exists, a server's or a stale one, is
/// an error. Group or other access in `mode` gives that class search permission on the
/// directory, once the socket has its mode.
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
        .ok_or_else(|| invalid(format!("{} has no directory name", path.display())))?;
    let socket = unix_socket_path(path);
    let max = unsafe { std::mem::zeroed::<libc::sockaddr_un>() }
        .sun_path
        .len()
        - 1;
    if socket.as_os_str().len() > max {
        return Err(invalid(format!(
            "{} is too long for a unix socket: {} bytes, at most {max}",
            socket.display(),
            socket.as_os_str().len()
        )));
    }
    let parent_path = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = cname(name)?;
    let parent = open_at(
        None,
        &cname(parent_path.as_os_str())?,
        libc::O_RDONLY | libc::O_DIRECTORY,
    )?;
    if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } < 0 {
        let e = Error::last_os_error();
        return Err(match e.raw_os_error() {
            Some(libc::EEXIST) => Error::new(
                ErrorKind::AlreadyExists,
                format!(
                    "{} exists: --listen-unix makes a new directory there",
                    path.display()
                ),
            ),
            _ => e,
        });
    }
    let moved = || {
        Error::new(
            ErrorKind::PermissionDenied,
            format!("{} changed while it was set up", path.display()),
        )
    };
    let dir =
        open_at(Some(&parent), &name, libc::O_RDONLY | libc::O_DIRECTORY).map_err(|e| {
            match e.raw_os_error() {
                Some(libc::ELOOP | libc::ENOTDIR) => moved(),
                _ => e,
            }
        })?;
    let st = fstat(&dir)?;
    let made = stat_at(&parent, &name)?;
    if st.st_uid != unsafe { libc::geteuid() }
        || st.st_mode & libc::S_IFMT != libc::S_IFDIR
        || id(&st) != id(&made)
    {
        return Err(moved());
    }
    let mut file = SocketFile {
        parent,
        name,
        dir,
        dir_id: id(&st),
        socket_id: None,
    };
    let reach =
        (if mode & 0o070 != 0 { 0o010 } else { 0 }) | (if mode & 0o007 != 0 { 0o001 } else { 0 });
    let set_mode = |file: &SocketFile, m: u32| -> std::io::Result<()> {
        cvt(unsafe { libc::fchmod(file.dir.as_raw_fd(), m as libc::mode_t) })?;
        match fstat(&file.dir)?.st_mode as u32 & 0o7777 == m {
            true => Ok(()),
            false => Err(moved()),
        }
    };
    let bound = (|| {
        clear_acl(&file.dir)?;
        set_mode(&file, 0o700)?;
        let listener = tokio::net::UnixListener::bind(&socket)?;
        // Only this user can add to the directory, so a socket in it is the one just bound.
        let st = stat_at(&file.dir, SOCKET)
            .ok()
            .filter(is_socket)
            .ok_or_else(moved)?;
        file.socket_id = Some(id(&st));
        cvt(unsafe {
            libc::fchmodat(
                file.dir.as_raw_fd(),
                SOCKET.as_ptr(),
                mode as libc::mode_t,
                0,
            )
        })?;
        set_mode(&file, 0o700 | reach)?;
        Ok(listener)
    })();
    match bound {
        Ok(listener) => Ok((listener, file)),
        Err(e) => {
            remove_unix(&file);
            Err(e)
        }
    }
}

/// Removes what `bind_unix` made, each only if it is still the entry that was made: the
/// socket, then the directory if nothing else is in it.
#[cfg(unix)]
pub fn remove_unix(file: &SocketFile) {
    let at = |dir: &std::os::fd::OwnedFd, name: &std::ffi::CStr, want| {
        stat_at(dir, name).is_ok_and(|st| id(&st) == want)
    };
    if let Some(socket) = file.socket_id {
        if at(&file.dir, SOCKET, socket) {
            let _ = unlink_at(&file.dir, SOCKET, 0);
        }
    }
    if at(&file.parent, &file.name, file.dir_id) {
        let _ = unlink_at(&file.parent, &file.name, libc::AT_REMOVEDIR);
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
            let path = d.join("sf");
            let (l, _) = bind_unix(&path, 0o600).unwrap();
            let mode = std::fs::metadata(unix_socket_path(&path))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
            let server = tokio::spawn(axum::serve(l, router(s)).into_future());
            let sock = tokio::net::UnixStream::connect(unix_socket_path(&path))
                .await
                .unwrap();
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
            let path = d.join("sf");
            let server = tokio::spawn(
                axum::serve(bind_unix(&path, 0o600).unwrap().0, router(s)).into_future(),
            );
            for (bad, code) in [
                (Ws::Text("k,65,1".into()), 1003),
                (Ws::Binary(Bytes::from_static(&[9, b'k'])), 1003),
                (Ws::Binary(Bytes::new()), 1003),
                (Ws::Binary(Bytes::from_static(&[1, 0xff])), 1007),
            ] {
                let sock = tokio::net::UnixStream::connect(unix_socket_path(&path))
                    .await
                    .unwrap();
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
            let path = d.join("sf");
            let (l, file) = bind_unix(&path, 0o600).unwrap();
            let server = tokio::spawn(axum::serve(l, router(s)).into_future());
            let sock = tokio::net::UnixStream::connect(unix_socket_path(&path))
                .await
                .unwrap();
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
        fn a_path_that_exists_is_refused_and_left_as_it_was() {
            let rt = runtime();
            let _guard = rt.enter();
            let d = dir("exists");
            let refused = |p: &std::path::Path| {
                let e = bind_unix(p, 0o600).unwrap_err();
                assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists, "{e}");
            };
            let empty = d.join("empty");
            std::fs::create_dir(&empty).unwrap();
            refused(&empty);
            assert_eq!(std::fs::read_dir(&empty).unwrap().count(), 0);
            let stale = d.join("stale");
            std::fs::create_dir(&stale).unwrap();
            drop(std::os::unix::net::UnixListener::bind(unix_socket_path(&stale)).unwrap());
            refused(&stale);
            assert!(
                unix_socket_path(&stale).exists(),
                "a stale socket was replaced"
            );
            let live = d.join("live");
            std::fs::create_dir(&live).unwrap();
            let server = std::os::unix::net::UnixListener::bind(unix_socket_path(&live)).unwrap();
            refused(&live);
            std::os::unix::net::UnixStream::connect(unix_socket_path(&live))
                .expect("the live socket is gone");
            let plain = d.join("plain");
            std::fs::write(&plain, b"x").unwrap();
            refused(&plain);
            assert_eq!(std::fs::read(&plain).unwrap(), b"x");
            let target = dir("exists-target");
            std::os::unix::fs::symlink(&target, d.join("link")).unwrap();
            refused(&d.join("link"));
            assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
            drop(server);
            std::fs::remove_dir_all(&d).unwrap();
            std::fs::remove_dir_all(&target).unwrap();
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
                let (l, file) = bind_unix(&d.join(format!("s{i}")), 0o666).unwrap();
                drop(l);
                remove_unix(&file);
            }
            stop.store(true, Ordering::Relaxed);
            let seen = watcher.join().unwrap();
            assert_eq!(seen.into_iter().collect::<Vec<_>>(), vec![before]);
            assert_eq!(std::fs::read_dir(&d).unwrap().count(), 0);
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn the_directory_lets_in_only_who_may_connect() {
            let rt = runtime();
            let _guard = rt.enter();
            let d = dir("modes");
            // Whoever can write the parent: the directory made in it is private all the same.
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o777)).unwrap();
            for (mode, dir_mode) in [
                (0o600, 0o700),
                (0o660, 0o710),
                (0o606, 0o701),
                (0o666, 0o711),
            ] {
                let at = d.join(format!("m{mode:o}"));
                let (l, file) = bind_unix(&at, mode).unwrap();
                let m = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode();
                assert_eq!(m(&at) & 0o7777, dir_mode, "{mode:o}");
                assert_eq!(m(&unix_socket_path(&at)) & 0o777, mode, "{mode:o}");
                drop(l);
                remove_unix(&file);
                assert!(!at.exists());
            }
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn a_symlinked_parent_is_refused() {
            let rt = runtime();
            let _guard = rt.enter();
            let d = dir("via");
            let target = dir("via-target");
            std::os::unix::fs::symlink(&target, d.join("via")).unwrap();
            assert!(bind_unix(&d.join("via").join("sf"), 0o600).is_err());
            assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
            std::fs::remove_dir_all(&d).unwrap();
            std::fs::remove_dir_all(&target).unwrap();
        }

        #[test]
        fn internal_names_are_free_for_the_directory() {
            let rt = runtime();
            let _guard = rt.enter();
            let d = dir("names");
            let held: Vec<_> = ["s", "s.lock", ".sf", ".starfold.lock"]
                .into_iter()
                .map(|n| {
                    let at = d.join(n);
                    let r = bind_unix(&at, 0o600).unwrap_or_else(|e| panic!("{n}: {e}"));
                    std::os::unix::net::UnixStream::connect(unix_socket_path(&at)).unwrap();
                    r
                })
                .collect();
            for (l, file) in held {
                drop(l);
                remove_unix(&file);
            }
            assert_eq!(std::fs::read_dir(&d).unwrap().count(), 0);
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn shutdown_removes_only_what_it_made() {
            let rt = runtime();
            let _guard = rt.enter();
            let d = dir("cleanup");
            let at = d.join("sf");
            let (l, file) = bind_unix(&at, 0o600).unwrap();
            drop(l);
            remove_unix(&file);
            assert!(!at.exists());

            let (l, file) = bind_unix(&at, 0o600).unwrap();
            drop(l);
            std::fs::write(at.join("other"), b"x").unwrap();
            remove_unix(&file);
            assert!(!unix_socket_path(&at).exists(), "left its own socket");
            assert_eq!(std::fs::read(at.join("other")).unwrap(), b"x");
            std::fs::remove_dir_all(&at).unwrap();

            let (l, file) = bind_unix(&at, 0o600).unwrap();
            drop(l);
            std::fs::remove_file(unix_socket_path(&at)).unwrap();
            std::fs::write(unix_socket_path(&at), b"x").unwrap();
            remove_unix(&file);
            assert_eq!(std::fs::read(unix_socket_path(&at)).unwrap(), b"x");
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn shutdown_works_in_the_directory_it_made_wherever_the_path_now_leads() {
            let rt = runtime();
            let _guard = rt.enter();
            let d = dir("moved");
            let at = d.join("sf");
            let away = d.join("away");
            let (l, file) = bind_unix(&at, 0o600).unwrap();
            std::fs::rename(&at, &away).unwrap();
            std::fs::create_dir(&at).unwrap();
            drop(std::os::unix::net::UnixListener::bind(unix_socket_path(&at)).unwrap());
            remove_unix(&file);
            assert!(
                unix_socket_path(&at).exists(),
                "removed what now sits at the path"
            );
            assert!(!unix_socket_path(&away).exists(), "left its own socket");
            drop(l);
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn a_second_server_on_the_path_is_turned_away() {
            let rt = runtime();
            let _guard = rt.enter();
            let d = dir("second");
            let at = d.join("sf");
            let (l, file) = bind_unix(&at, 0o600).unwrap();
            let e = bind_unix(&at, 0o600).unwrap_err();
            assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists, "{e}");
            std::os::unix::net::UnixStream::connect(unix_socket_path(&at)).unwrap();
            drop(l);
            remove_unix(&file);
            let (l, file) = bind_unix(&at, 0o600).unwrap();
            drop(l);
            remove_unix(&file);
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn servers_starting_together_get_one_socket() {
            let d = dir("race");
            for round in 0..20 {
                let at = d.join(format!("s{round}"));
                let go = Arc::new(std::sync::Barrier::new(6));
                let (tx, rx) = std::sync::mpsc::channel();
                let threads: Vec<_> = (0..6)
                    .map(|_| {
                        let (go, tx, at) = (go.clone(), tx.clone(), at.clone());
                        std::thread::spawn(move || {
                            let rt = runtime();
                            let _guard = rt.enter();
                            go.wait();
                            let r = bind_unix(&at, 0o600);
                            let won = r.as_ref().is_ok_and(|(_, f)| {
                                use std::os::unix::fs::MetadataExt;
                                let m = std::fs::symlink_metadata(unix_socket_path(&at)).unwrap();
                                f.socket_id == Some((m.dev() as _, m.ino() as _))
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
            // `<dir>/s`: the directory's name plus 2 bytes, after `<d>/`.
            let base = d.as_os_str().len() + 1;
            let at = d.join("a".repeat(max_path() + 1 - base - 2));
            assert_eq!(unix_socket_path(&at).as_os_str().len(), max_path() + 1);
            let e = bind_unix(&at, 0o600).unwrap_err();
            assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput, "{e}");
            assert_eq!(std::fs::read_dir(&d).unwrap().count(), 0);
            let at = d.join("a".repeat(max_path() - base - 2));
            let (l, file) = bind_unix(&at, 0o600).expect("the longest path that fits");
            std::os::unix::net::UnixStream::connect(unix_socket_path(&at)).unwrap();
            drop((l, file));
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn an_inherited_acl_is_cleared() {
            use std::os::unix::ffi::OsStrExt;
            let rt = runtime();
            let _guard = rt.enter();
            let d = dir("acl");
            let c = |p: &std::path::Path| std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
            // A default ACL (version 2): user::rwx, user:nobody:rwx, group::---, mask::rwx, other::---.
            let mut acl = 2u32.to_le_bytes().to_vec();
            for (tag, perm, id) in [
                (0x01u16, 7u16, u32::MAX),
                (0x02, 7, 65534),
                (0x04, 0, u32::MAX),
                (0x10, 7, u32::MAX),
                (0x20, 0, u32::MAX),
            ] {
                acl.extend(tag.to_le_bytes());
                acl.extend(perm.to_le_bytes());
                acl.extend(id.to_le_bytes());
            }
            let set = unsafe {
                libc::setxattr(
                    c(&d).as_ptr(),
                    c"system.posix_acl_default".as_ptr(),
                    acl.as_ptr().cast(),
                    acl.len(),
                    0,
                )
            };
            assert_eq!(set, 0, "{}", std::io::Error::last_os_error());
            let has = |p: &std::path::Path, name: &std::ffi::CStr| unsafe {
                libc::getxattr(c(p).as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0) >= 0
            };
            let plain = d.join("plain");
            std::fs::create_dir(&plain).unwrap();
            assert!(has(&plain, c"system.posix_acl_access"), "no ACL to inherit");
            let at = d.join("sf");
            let (l, file) = bind_unix(&at, 0o660).unwrap();
            for (p, name) in [
                (at.clone(), c"system.posix_acl_default"),
                (at.clone(), c"system.posix_acl_access"),
                (unix_socket_path(&at), c"system.posix_acl_access"),
            ] {
                assert!(!has(&p, name), "{} kept {name:?}", p.display());
            }
            drop((l, file));
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[cfg(target_os = "macos")]
        #[test]
        fn an_inherited_acl_is_cleared() {
            let rt = runtime();
            let _guard = rt.enter();
            let d = dir("acl");
            let granted = std::process::Command::new("/bin/chmod")
                .arg("+a")
                .arg("everyone allow list,search,add_file,add_subdirectory,delete_child,file_inherit,directory_inherit")
                .arg(&d)
                .status()
                .unwrap();
            assert!(granted.success());
            // `ls -led` lists ACL entries on the lines after the file's own.
            let acl = |p: &std::path::Path| {
                let out = std::process::Command::new("/bin/ls")
                    .arg("-led")
                    .arg(p)
                    .output()
                    .unwrap();
                String::from_utf8_lossy(&out.stdout).lines().count() > 1
            };
            let plain = d.join("plain");
            std::fs::create_dir(&plain).unwrap();
            assert!(acl(&plain), "no ACL to inherit");
            let at = d.join("sf");
            let (l, file) = bind_unix(&at, 0o600).unwrap();
            assert!(!acl(&at), "the directory kept an ACL");
            assert!(!acl(&unix_socket_path(&at)), "the socket has an ACL");
            drop((l, file));
            std::fs::remove_dir_all(&d).unwrap();
        }
    }
}
