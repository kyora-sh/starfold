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
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub struct SocketFile {
    parent: std::os::fd::OwnedFd,
    name: std::ffi::CString,
    dir: std::os::fd::OwnedFd,
    dir_id: (libc::dev_t, libc::ino_t),
    socket_id: Option<(libc::dev_t, libc::ino_t)>,
}

/// The socket's name in the --listen-unix directory.
#[cfg(target_os = "linux")]
const SOCKET: &std::ffi::CStr = c"s";

/// Where clients connect for --listen-unix `dir`.
#[cfg(target_os = "linux")]
pub fn unix_socket_path(dir: &Path) -> std::path::PathBuf {
    dir.join(SOCKET.to_str().unwrap())
}

#[cfg(target_os = "linux")]
fn cvt(r: libc::c_int) -> std::io::Result<libc::c_int> {
    if r < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

#[cfg(target_os = "linux")]
fn cname(s: &std::ffi::OsStr) -> std::io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(s.as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "a NUL in the path"))
}

/// Opens `name` in `dir` (or a path, with `dir` None), refusing a symlink.
#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
fn fstat(fd: &std::os::fd::OwnedFd) -> std::io::Result<libc::stat> {
    use std::os::fd::AsRawFd;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    cvt(unsafe { libc::fstat(fd.as_raw_fd(), &mut st) })?;
    Ok(st)
}

/// `name` in `dir`, not following a symlink.
#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
fn id(st: &libc::stat) -> (libc::dev_t, libc::ino_t) {
    (st.st_dev, st.st_ino)
}

#[cfg(target_os = "linux")]
fn is_socket(st: &libc::stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFSOCK
}

#[cfg(target_os = "linux")]
fn unlink_at(
    dir: &std::os::fd::OwnedFd,
    name: &std::ffi::CStr,
    flags: libc::c_int,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    cvt(unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), flags) }).map(|_| ())
}

#[cfg(target_os = "linux")]
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

/// The --listen-unix parent's contract, checked on the parent as opened: no other user can
/// rename or replace entries in it. That is a parent owned by this user or by root,
/// writable by no one else unless it is sticky (as /tmp is), and with no ACL entry that
/// lets anyone else write to it. With such a parent, nobody else can move the new
/// directory aside or put something in its place while it is set up.
#[cfg(target_os = "linux")]
fn check_parent(parent: &std::os::fd::OwnedFd, path: &Path) -> std::io::Result<()> {
    let st = fstat(parent)?;
    let refuse = |why: &str| {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "{}: {why}; --listen-unix needs a parent where no other user can rename \
                 entries, e.g. /tmp (sticky) or a directory only you can write",
                path.display()
            ),
        ))
    };
    let me = unsafe { libc::geteuid() };
    if st.st_uid != me && st.st_uid != 0 {
        return refuse(&format!("its parent belongs to user {}", st.st_uid));
    }
    let sticky = st.st_mode & libc::S_ISVTX != 0;
    if st.st_mode & 0o022 != 0 && !sticky {
        return refuse("its parent is writable by other users and not sticky");
    }
    if acl_lets_others_write(parent)? {
        return refuse("its parent has an ACL entry that lets other users add or remove entries");
    }
    Ok(())
}

/// The parent's access ACL has a named user or group entry with write permission that
/// its mask doesn't take away.
#[cfg(target_os = "linux")]
fn acl_lets_others_write(dir: &std::os::fd::OwnedFd) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd;
    let name = c"system.posix_acl_access";
    let mut buf = vec![0u8; 4096];
    let n = unsafe {
        libc::fgetxattr(
            dir.as_raw_fd(),
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    if n < 0 {
        let e = std::io::Error::last_os_error();
        return match e.raw_os_error() {
            Some(libc::ENODATA | libc::EOPNOTSUPP) => Ok(false),
            _ => Err(e),
        };
    }
    let entries: Vec<(u16, u16)> = buf[4.min(n as usize)..n as usize]
        .chunks_exact(8)
        .map(|e| {
            (
                u16::from_le_bytes([e[0], e[1]]),
                u16::from_le_bytes([e[2], e[3]]),
            )
        })
        .collect();
    const USER: u16 = 0x02;
    const GROUP: u16 = 0x08;
    const MASK: u16 = 0x10;
    const WRITE: u16 = 0x02;
    let mask_writes = entries
        .iter()
        .find(|e| e.0 == MASK)
        .is_none_or(|e| e.1 & WRITE != 0);
    Ok(mask_writes
        && entries
            .iter()
            .any(|e| (e.0 == USER || e.0 == GROUP) && e.1 & WRITE != 0))
}

/// Serves --listen-unix: makes the directory `path` (it must not exist yet; its parent
/// must) for this user alone and binds a socket named `s` in it with permissions `mode`.
/// Every step after the mkdir works on the directory as opened, and nothing that already
/// existed is used, replaced or removed: a path that exists, a server's or a stale one, is
/// an error. Group or other access in `mode` gives that class search permission on the
/// directory, once the socket has its mode.
///
/// The directory rules keep an honest setup tidy and fail early; they are not what keeps
/// other users out. That is the peer check every connection passes ([`PeerChecked`]).
///
/// It changes the process's working directory and umask while it binds (see [`bind_in`]),
/// so it runs before any other thread exists: the binary calls it first thing.
#[cfg(target_os = "linux")]
pub fn bind_unix_std(
    path: &Path,
    mode: u32,
) -> std::io::Result<(std::os::unix::net::UnixListener, SocketFile)> {
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
    check_parent(&parent, path)?;
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
    // Until the directory is open, what can't be set up takes back the empty directory just
    // made (an empty directory of ours under that name; a umask of 0777, say, makes one
    // even its owner can't open), so the next start isn't refused for it.
    let unmake = |e: Error| {
        let ours = stat_at(&parent, &name).is_ok_and(|s| {
            s.st_mode & libc::S_IFMT == libc::S_IFDIR && s.st_uid == unsafe { libc::geteuid() }
        });
        if ours {
            let _ = unlink_at(&parent, &name, libc::AT_REMOVEDIR);
        }
        e
    };
    let dir = open_at(Some(&parent), &name, libc::O_RDONLY | libc::O_DIRECTORY)
        .map_err(|e| match e.raw_os_error() {
            Some(libc::ELOOP | libc::ENOTDIR) => moved(),
            _ => e,
        })
        .map_err(unmake)?;
    let st = fstat(&dir).map_err(unmake)?;
    let made = stat_at(&parent, &name).map_err(unmake)?;
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
    // Connecting takes write permission on the socket, so only a class that has it gets
    // search permission on the directory.
    let reach =
        (if mode & 0o020 != 0 { 0o010 } else { 0 }) | (if mode & 0o002 != 0 { 0o001 } else { 0 });
    // The directory's group is the one the socket gets (this process's), whatever a setgid
    // parent gave it, so the group that may connect is the group that may search.
    let egid = unsafe { libc::getegid() };
    if st.st_gid != egid {
        let set = cvt(unsafe { libc::fchown(file.dir.as_raw_fd(), libc::uid_t::MAX, egid) })
            .is_ok()
            && fstat(&file.dir).is_ok_and(|s| s.st_gid == egid);
        if !set {
            remove_unix(&file);
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                format!(
                    "{}: its group can't be this process's ({egid}), as the socket's will be",
                    path.display()
                ),
            ));
        }
    }
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
        #[cfg(test)]
        if let Some(race) = BEFORE_BIND.with(|h| h.take()) {
            race();
        }
        let bound = bind_in(&file.dir, mode, &mut file.socket_id)?;
        set_mode(&file, 0o700 | reach)?;
        Ok(bound)
    })();
    match bound {
        Ok(listener) => Ok((listener, file)),
        Err(e) => {
            remove_unix(&file);
            Err(e)
        }
    }
}

/// Binds and listens on `s` in `dir`, with permissions `mode`, without a path: with `dir`
/// as the working directory and the umask that gives `mode`, `bind("s")`. The socket's
/// identity goes on record (`socket_id`) right after the bind, so the cleanup finds it
/// whatever fails next; then the mode is read back and the socket listens. The working
/// directory and the umask are restored before it returns, on every path. Both are the
/// process's, so nothing else may run meanwhile: no other thread exists.
#[cfg(target_os = "linux")]
pub fn bind_in(
    dir: &std::os::fd::OwnedFd,
    mode: u32,
    socket_id: &mut Option<(libc::dev_t, libc::ino_t)>,
) -> std::io::Result<std::os::unix::net::UnixListener> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let saved = open_at(None, c".", libc::O_PATH | libc::O_DIRECTORY)?;
    let umask = unsafe { libc::umask((0o777 & !mode) as libc::mode_t) };
    let bound = (|| {
        cvt(unsafe { libc::fchdir(dir.as_raw_fd()) })?;
        let fd =
            cvt(unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) })?;
        let sock = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
        let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        addr.sun_path[0] = b's' as libc::c_char;
        let len = std::mem::size_of::<libc::sa_family_t>() + 2;
        #[cfg(test)]
        fail_at("bind")?;
        cvt(unsafe {
            libc::bind(
                fd,
                &addr as *const libc::sockaddr_un as *const libc::sockaddr,
                len as libc::socklen_t,
            )
        })?;
        let st = stat_at(dir, SOCKET)?;
        if !is_socket(&st) {
            return Err(std::io::Error::other(
                "what was bound in the directory is no socket",
            ));
        }
        *socket_id = Some(id(&st));
        if st.st_mode as u32 & 0o777 != mode {
            return Err(std::io::Error::other(format!(
                "the socket came out mode {:o}, not {mode:o}",
                st.st_mode as u32 & 0o777
            )));
        }
        #[cfg(test)]
        fail_at("listen")?;
        cvt(unsafe { libc::listen(fd, 1024) })?;
        Ok(std::os::unix::net::UnixListener::from(sock))
    })();
    unsafe { libc::umask(umask) };
    let back = cvt(unsafe { libc::fchdir(saved.as_raw_fd()) });
    let bound = bound?;
    back?;
    Ok(bound)
}

/// The bound listener of [`bind_unix_std`], registered with tokio; on a failure the socket
/// and directory are removed.
#[cfg(target_os = "linux")]
pub fn register(
    listener: std::os::unix::net::UnixListener,
    file: &SocketFile,
) -> std::io::Result<tokio::net::UnixListener> {
    let r = (|| {
        listener.set_nonblocking(true)?;
        #[cfg(test)]
        if REGISTRATION_FAILS.with(|f| f.take()) {
            return Err(std::io::Error::other(
                "injected: the listener's registration failed",
            ));
        }
        tokio::net::UnixListener::from_std(listener)
    })();
    if r.is_err() {
        remove_unix(file);
    }
    r
}

#[cfg(target_os = "linux")]
static AT_EXIT: std::sync::OnceLock<SocketFile> = std::sync::OnceLock::new();

/// Keeps `file` for the rest of the process, and removes what it names when the process
/// exits through `exit` (a startup error after the bind as much as a shutdown). A crash
/// or a kill by signal leaves it, and the next start on the path is refused.
#[cfg(target_os = "linux")]
pub fn remove_at_exit(file: SocketFile) -> &'static SocketFile {
    extern "C" fn remove() {
        if let Some(file) = AT_EXIT.get() {
            remove_unix(file);
        }
    }
    let file = AT_EXIT.get_or_init(|| file);
    unsafe { libc::atexit(remove) };
    file
}

/// The uid an unmapped user reads as (the default overflow uid): never let in.
#[cfg(target_os = "linux")]
const OVERFLOW_UID: u32 = 65534;

/// Whether a peer whose uid is `peer` may talk to a server running as `own`: the same
/// user only, and never the overflow uid, which stands for anyone the kernel can't name
/// here.
#[cfg(target_os = "linux")]
pub fn admits(peer: u32, own: u32) -> bool {
    peer == own && peer != OVERFLOW_UID
}

/// The unix listener with the check that keeps other users out: every accepted
/// connection's peer credentials (SO_PEERCRED, recorded by the kernel at the peer's
/// `connect`) are read before a byte is read or written, and a connection from any uid
/// but the server's own is closed there.
#[cfg(target_os = "linux")]
pub struct PeerChecked(pub tokio::net::UnixListener);

#[cfg(target_os = "linux")]
impl axum::serve::Listener for PeerChecked {
    type Io = tokio::net::UnixStream;
    type Addr = tokio::net::unix::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let own = unsafe { libc::geteuid() };
        loop {
            let (stream, addr) = axum::serve::Listener::accept(&mut self.0).await;
            match stream.peer_cred() {
                Ok(c) if admits(c.uid(), own) => return (stream, addr),
                Ok(c) => log::warn!(
                    "unix socket: refused a connection from uid {} (only uid {own} is let in)",
                    c.uid()
                ),
                Err(e) => log::warn!("unix socket: refused a connection without credentials: {e}"),
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.0.local_addr()
    }
}

#[cfg(all(target_os = "linux", test))]
thread_local! {
    /// Runs on this thread just before `bind_unix` binds, once: what a race would do there.
    pub static BEFORE_BIND: std::cell::Cell<Option<Box<dyn FnOnce() + Send>>> = const { std::cell::Cell::new(None) };
    /// The next `bind_unix` on this thread fails to register its listener with tokio (as
    /// an exhausted epoll watch limit makes it), once.
    pub static REGISTRATION_FAILS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The step of `bind_in` that fails next on this thread, once: "bind" or "listen".
    pub static FAIL_AT: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
}

#[cfg(all(target_os = "linux", test))]
fn fail_at(step: &'static str) -> std::io::Result<()> {
    match FAIL_AT.with(|f| f.get()) == Some(step) {
        true => {
            FAIL_AT.with(|f| f.set(None));
            Err(std::io::Error::other(format!("injected: {step} failed")))
        }
        false => Ok(()),
    }
}

/// [`bind_unix_std`] and [`register`] for tests, which run on many threads: the bind runs
/// on a thread of its own that has unshared its working directory and umask
/// (`unshare(CLONE_FS)`), so it changes them for itself alone. The hooks set on the
/// calling thread go with it.
#[cfg(all(target_os = "linux", test))]
pub fn bind_unix(
    path: &Path,
    mode: u32,
) -> std::io::Result<(tokio::net::UnixListener, SocketFile)> {
    let path = path.to_path_buf();
    let race = BEFORE_BIND.with(|h| h.take());
    let fail = FAIL_AT.with(|f| f.take());
    let (l, file) = std::thread::spawn(move || {
        cvt(unsafe { libc::unshare(libc::CLONE_FS) })?;
        BEFORE_BIND.with(|h| h.set(race));
        FAIL_AT.with(|f| f.set(fail));
        bind_unix_std(&path, mode)
    })
    .join()
    .unwrap()?;
    let l = register(l, &file)?;
    Ok((l, file))
}

/// Removes what `bind_unix` made, each only if it is still the entry that was made (type,
/// device and inode: a freed socket's inode can come back as another file): the socket,
/// then the directory if nothing else is in it.
#[cfg(target_os = "linux")]
pub fn remove_unix(file: &SocketFile) {
    let at = |dir: &std::os::fd::OwnedFd, name: &std::ffi::CStr, kind, want| {
        stat_at(dir, name).is_ok_and(|st| st.st_mode & libc::S_IFMT == kind && id(&st) == want)
    };
    if let Some(socket) = file.socket_id {
        if at(&file.dir, SOCKET, libc::S_IFSOCK, socket) {
            let _ = unlink_at(&file.dir, SOCKET, 0);
        }
    }
    if at(&file.parent, &file.name, libc::S_IFDIR, file.dir_id) {
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

/// The page's icon (--favicon): that file's bytes and content type, or starfold's own icon.
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
    // STARFOLD_TEST_HOOKS: POST /debug/renegotiate moves every protocol 2 viewer to a new
    // epoch (a codec switch, even to the same codec), and POST /debug/fail-h264 makes the
    // H.264 encoder fall back to the next one, for end-to-end tests.
    let debug_routes: Router = if crate::compat::is_set("STARFOLD_TEST_HOOKS") {
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
        "encoder": crate::encoder::with_live(&s.hub.encoder.lock(), &crate::encoder::turned_off()),
        "encoders": &*s.encoders,
    });
    (
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

async fn ws_handler(ws: WebSocketUpgrade, State(s): State<AppState>) -> Response {
    let ws = ws.max_message_size(frame::MAX_MESSAGE);
    // Framed, a message up to the cap may come as one frame as well as in several.
    let ws = match s.framed {
        true => ws.max_frame_size(frame::MAX_MESSAGE),
        false => ws,
    };
    ws.on_upgrade(move |socket| handle_socket(socket, s))
}

async fn handle_socket(socket: WebSocket, s: AppState) {
    use futures_util::{SinkExt, StreamExt};
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Out>();
    let (close_tx, mut close_rx) = oneshot::channel::<FrameError>();
    let framed = s.framed;
    let client = s.hub.add_client(tx);
    log::info!("client {} connected", client.id);
    s.hub.send_first_hello(&client);
    if let Some(c) = s.hub.cursor.lock().clone() {
        client.send(Out::Bin(c));
    }
    let mut writer = tokio::spawn(async move {
        let dbg = crate::compat::is_set("STARFOLD_DEBUG_MARKER");
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
                if framed {
                    refused = refused_by_websocket(e);
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

/// What a framed socket closes with for a message the WebSocket itself refused: one over
/// `frame::MAX_MESSAGE` however it was fragmented (1009), or a text message that isn't
/// UTF-8 (1007; a text message has no place there in any case). None: no close frame.
fn refused_by_websocket(e: axum::Error) -> Option<FrameError> {
    match e.into_inner().downcast_ref::<tungstenite::Error>()? {
        tungstenite::Error::Capacity(_) => Some(FrameError::Oversized),
        tungstenite::Error::Utf8(_) => Some(FrameError::BadUtf8),
        _ => None,
    }
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
            if !Hub::answers(offer, gen) {
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
    use crate::hub::HelloStep;

    /// A client as `handle_socket` registers one, and the generations of the hellos queued
    /// to it, in order.
    fn connected(hub: &Hub) -> (Arc<Client>, mpsc::UnboundedReceiver<Out>) {
        let (tx, rx) = mpsc::unbounded_channel::<Out>();
        (hub.add_client(tx), rx)
    }

    fn hello_gens(rx: &mut mpsc::UnboundedReceiver<Out>) -> Vec<u64> {
        let mut v = Vec::new();
        while let Ok(m) = rx.try_recv() {
            if let Out::Text(t) = m {
                let j: serde_json::Value = serde_json::from_str(&t).unwrap();
                if j["t"] == "hello" {
                    v.push(j["offer"].as_u64().unwrap());
                }
            }
        }
        v
    }

    /// What the video thread does for a new screen size: a new offer generation, then a
    /// hello at it to every connected client.
    fn new_size(hub: &Hub) {
        hub.next_offer_gen();
        hub.restart_clients();
    }

    /// A new size between a client's registration and its first hello's enqueue: its
    /// queue still ends on the newest generation, and only that one's answer is taken.
    #[test]
    fn a_new_size_while_a_viewer_connects_ends_its_queue_on_the_newest_hello() {
        let hub = Hub::new();
        let (c, mut rx) = connected(&hub);
        hub.send_first_hello_with(&c, |s| {
            if s == HelloStep::Built(0) {
                new_size(&hub);
            }
        });
        let gens = hello_gens(&mut rx);
        assert_eq!(gens, vec![1, 0, 1], "restart hello, ours at 0, ours at 1");
        let now = hub.offer_gen.load(Ordering::SeqCst);
        assert_eq!(gens.last(), Some(&now));
        assert!(Hub::answers(gens.last().copied(), now));
        assert!(
            !Hub::answers(Some(0), now),
            "an answer to the stale hello is refused"
        );
    }

    /// Two new sizes between a check and the next enqueue: the loop checks again after
    /// every enqueue, so the queue still ends on the newest.
    #[test]
    fn two_new_sizes_between_a_check_and_the_next_hello_still_end_on_the_newest() {
        let hub = Hub::new();
        let (c, mut rx) = connected(&hub);
        hub.send_first_hello_with(&c, |s| match s {
            HelloStep::Built(0) => new_size(&hub),
            HelloStep::Built(1) => {
                new_size(&hub);
                new_size(&hub);
            }
            _ => {}
        });
        let gens = hello_gens(&mut rx);
        assert_eq!(gens, vec![1, 0, 2, 3, 1, 3]);
        let now = hub.offer_gen.load(Ordering::SeqCst);
        assert_eq!((now, gens.last()), (3, Some(&3)));
        assert!(Hub::answers(Some(3), now));
    }

    /// No new size meanwhile: exactly one hello.
    #[test]
    fn a_viewer_that_connects_gets_one_hello_when_nothing_changes() {
        let hub = Hub::new();
        let (c, mut rx) = connected(&hub);
        let mut steps = Vec::new();
        hub.send_first_hello_with(&c, |s| steps.push(s));
        assert_eq!(steps, vec![HelloStep::Built(0), HelloStep::Sent(0)]);
        assert_eq!(hello_gens(&mut rx), vec![0]);
        hub.send_first_hello(&c);
        assert_eq!(
            hello_gens(&mut rx),
            vec![0],
            "the plain entry point: one hello"
        );
    }

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
                root: None,
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

        let dir = std::env::temp_dir().join(format!("starfold-favicon-{}", std::process::id()));
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

    #[cfg(target_os = "linux")]
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

        /// A framed server, and a viewer of it connected over its unix socket in a
        /// directory named for `name`.
        async fn framed_viewer(
            hub: &Arc<Hub>,
            name: &str,
        ) -> (
            WebSocketStream<tokio::net::UnixStream>,
            tokio::task::JoinHandle<std::io::Result<()>>,
            std::path::PathBuf,
        ) {
            let mut s = app(hub, false, None);
            s.framed = true;
            let d = dir(name);
            let path = d.join("sf");
            let (l, _) = bind_unix(&path, 0o600).unwrap();
            let server = tokio::spawn(axum::serve(l, router(s)).into_future());
            let sock = tokio::net::UnixStream::connect(unix_socket_path(&path))
                .await
                .unwrap();
            let (mut ws, _) = tokio_tungstenite::client_async("ws://localhost/ws", sock)
                .await
                .unwrap();
            next(&mut ws, true).await;
            (ws, server, d)
        }

        /// An outer text message that isn't even UTF-8, which the WebSocket refuses before
        /// it can be unframed, still gets a close code (1007), not a bare disconnect.
        #[tokio::test]
        async fn a_framed_socket_closes_on_outer_text_that_is_not_utf8() {
            use tokio_tungstenite::tungstenite::protocol::frame::coding::{Data, OpCode};
            use tokio_tungstenite::tungstenite::protocol::frame::Frame;
            let (hub, _input) = wired();
            let (mut ws, server, d) = framed_viewer(&hub, "outer-utf8").await;
            let bad = Frame::message(
                Bytes::from_static(&[0xff, 0xfe]),
                OpCode::Data(Data::Text),
                true,
            );
            ws.send(Ws::Frame(bad)).await.unwrap();
            assert_eq!(close_code(&mut ws).await, 1007);
            gone(&hub).await;
            server.abort();
            std::fs::remove_dir_all(&d).unwrap();
        }

        /// A 20 MiB framed message in one WebSocket frame is taken like the same message in
        /// several: the session goes on.
        #[tokio::test]
        async fn a_framed_message_under_the_cap_in_one_frame_is_taken() {
            let (hub, input) = wired();
            let (mut ws, server, d) = framed_viewer(&hub, "one-frame").await;
            let big = frame::encode_binary(&vec![0u8; 20 << 20]).unwrap();
            ws.send(Ws::Binary(big)).await.unwrap();
            ws.send(Ws::Binary(frame::encode_text("k,65,1").unwrap()))
                .await
                .unwrap();
            let (typed, _input) = key(input).await;
            assert!(typed, "the session ended at the 20 MiB frame");
            assert_eq!(hub.clients.lock().len(), 1);
            server.abort();
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
            let started = Arc::new(std::sync::Barrier::new(2));
            let watcher = {
                let (stop, started) = (stop.clone(), started.clone());
                std::thread::spawn(move || {
                    let mut seen = std::collections::BTreeSet::from([umask()]);
                    started.wait();
                    while !stop.load(Ordering::Relaxed) {
                        seen.insert(umask());
                    }
                    seen
                })
            };
            started.wait();
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
            // Whoever can write the (sticky) parent: the directory made in it is private all
            // the same.
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o1777)).unwrap();
            for (mode, dir_mode) in [
                (0o600, 0o700),
                (0o660, 0o710),
                (0o606, 0o701),
                (0o666, 0o711),
                // Read alone doesn't let a class connect, so it doesn't let it search.
                (0o640, 0o700),
                (0o604, 0o700),
                (0o620, 0o710),
                (0o602, 0o701),
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

        /// Under a setgid parent of another group the directory would take that group; it
        /// gets this process's instead, the socket's, so the group that may connect is the
        /// one that may search. Needs a supplementary group other than the effective one.
        #[test]
        fn a_setgid_parent_leaves_the_directory_in_the_sockets_group() {
            use std::os::unix::fs::MetadataExt;
            let rt = runtime();
            let _guard = rt.enter();
            let egid = unsafe { libc::getegid() };
            let mut groups = vec![0 as libc::gid_t; 256];
            let n = unsafe { libc::getgroups(groups.len() as libc::c_int, groups.as_mut_ptr()) };
            let Some(other) = groups[..n.max(0) as usize]
                .iter()
                .copied()
                .find(|&g| g != egid)
            else {
                eprintln!("skipped: no supplementary group other than {egid}");
                return;
            };
            let d = dir("setgid");
            let c = std::ffi::CString::new(d.as_os_str().as_encoded_bytes()).unwrap();
            assert_eq!(
                unsafe { libc::chown(c.as_ptr(), libc::uid_t::MAX, other) },
                0
            );
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o2700)).unwrap();
            let at = d.join("sf");
            let (l, file) = bind_unix(&at, 0o660).unwrap();
            let meta = |p: &std::path::Path| std::fs::metadata(p).unwrap();
            assert_eq!(meta(&at).gid(), egid, "the directory's group");
            assert_eq!(
                meta(&unix_socket_path(&at)).gid(),
                egid,
                "the socket's group"
            );
            assert_eq!(meta(&at).mode() & 0o7777, 0o710);
            drop(l);
            remove_unix(&file);
            std::fs::remove_dir_all(&d).unwrap();
        }

        /// A parent in which other users could rename entries is refused before anything
        /// is made in it: writable by group or other without the sticky bit. A sticky
        /// world-writable one (as /tmp) is taken.
        #[test]
        fn a_parent_others_can_rename_in_is_refused() {
            let rt = runtime();
            let _guard = rt.enter();
            let d = dir("contract");
            for mode in [0o770, 0o707, 0o777, 0o730] {
                std::fs::set_permissions(&d, std::fs::Permissions::from_mode(mode)).unwrap();
                let e = bind_unix(&d.join("sf"), 0o600).unwrap_err();
                assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied, "{mode:o}");
                assert!(e.to_string().contains("not sticky"), "{mode:o}: {e}");
                assert_eq!(std::fs::read_dir(&d).unwrap().count(), 0, "{mode:o}");
            }
            for mode in [0o1777, 0o1770, 0o755, 0o700] {
                std::fs::set_permissions(&d, std::fs::Permissions::from_mode(mode)).unwrap();
                let at = d.join("sf");
                let (l, file) = bind_unix(&at, 0o600).unwrap_or_else(|e| panic!("{mode:o}: {e}"));
                drop(l);
                remove_unix(&file);
            }
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::remove_dir_all(&d).unwrap();
        }

        /// A parent whose access ACL lets a named user write is refused, whatever its mode
        /// bits say.
        #[cfg(target_os = "linux")]
        #[test]
        fn a_parent_with_a_write_acl_entry_is_refused() {
            use std::os::unix::ffi::OsStrExt;
            let rt = runtime();
            let _guard = rt.enter();
            let d = dir("acl-parent");
            // Sticky, so its group bits (the ACL's mask) alone don't refuse it: the ACL
            // entry does.
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o1700)).unwrap();
            let c = std::ffi::CString::new(d.as_os_str().as_bytes()).unwrap();
            // user::rwx, user:nobody:rwx, group::---, mask::rwx, other::---.
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
                    c.as_ptr(),
                    c"system.posix_acl_access".as_ptr(),
                    acl.as_ptr().cast(),
                    acl.len(),
                    0,
                )
            };
            if set != 0 {
                let e = std::io::Error::last_os_error();
                assert_eq!(e.raw_os_error(), Some(libc::EOPNOTSUPP), "{e}");
                eprintln!("skipped: no ACLs on this filesystem");
                std::fs::remove_dir_all(&d).unwrap();
                return;
            }
            let e = bind_unix(&d.join("sf"), 0o600).unwrap_err();
            assert!(e.to_string().contains("ACL entry"), "{e}");
            assert_eq!(std::fs::read_dir(&d).unwrap().count(), 0);
            std::fs::remove_dir_all(&d).unwrap();
        }

        /// The directory is renamed away just before the bind and another put at its path:
        /// the socket still lands in the directory that was made and opened, never in the
        /// replacement.
        #[cfg(target_os = "linux")]
        #[test]
        fn the_bind_lands_in_the_opened_directory_after_a_rename() {
            use std::os::unix::fs::FileTypeExt;
            let rt = runtime();
            let _guard = rt.enter();
            let d = dir("rename");
            let at = d.join("sf");
            let aside = d.join("aside");
            let (from, to) = (at.clone(), aside.clone());
            BEFORE_BIND.with(|h| {
                h.set(Some(Box::new(move || {
                    std::fs::rename(&from, &to).unwrap();
                    std::fs::create_dir(&from).unwrap();
                })))
            });
            let (l, file) = bind_unix(&at, 0o600).unwrap();
            assert!(
                std::fs::symlink_metadata(aside.join("s"))
                    .unwrap()
                    .file_type()
                    .is_socket(),
                "the socket isn't in the directory that was made"
            );
            assert!(
                !at.join("s").exists(),
                "the socket landed in the replacement"
            );
            drop(l);
            remove_unix(&file);
            assert!(at.exists(), "the replacement was removed");
            std::fs::remove_dir_all(&d).unwrap();
        }

        /// Under a umask of 0777 the directory comes out mode 000, which even its owner
        /// can't open: the setup fails, and takes the directory back so the next start can
        /// make it. The umask is the process's, so this runs in a child process of its own.
        #[test]
        fn a_directory_its_owner_cant_open_is_taken_back() {
            const CHILD: &str = "STARFOLD_TEST_UMASK_CHILD";
            if let Some(at) = std::env::var_os(CHILD) {
                if unsafe { libc::geteuid() } == 0 {
                    eprintln!("skipped: root opens a mode-000 directory");
                    return;
                }
                unsafe { libc::umask(0o777) };
                let rt = runtime();
                let _guard = rt.enter();
                let at = std::path::PathBuf::from(at);
                let e = bind_unix(&at, 0o600).unwrap_err();
                assert!(!at.exists(), "{} left behind ({e})", at.display());
                return;
            }
            let d = dir("umask-child");
            let at = d.join("sf");
            let name = "server::tests::socket::a_directory_its_owner_cant_open_is_taken_back";
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", name, "--nocapture", "--test-threads=1"])
                .env(CHILD, &at)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(String::from_utf8_lossy(&out.stdout).contains("1 passed"));
            assert!(!at.exists());
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
            remove_unix(&file);
            assert!(at.exists(), "removed the directory now at the path");
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
            assert!(e.to_string().contains("too long for a unix socket"), "{e}");
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

        /// A listener bound but refused by tokio's registration (an exhausted epoll watch
        /// limit, say) leaves neither the socket nor the directory: the socket was on
        /// record before the registration, and the next start on the path works.
        #[test]
        fn a_failed_registration_leaves_nothing_behind() {
            let rt = runtime();
            let _guard = rt.enter();
            let d = dir("registration");
            let at = d.join("sf");
            REGISTRATION_FAILS.with(|f| f.set(true));
            let e = bind_unix(&at, 0o600).unwrap_err();
            assert!(e.to_string().contains("registration"), "{e}");
            assert!(!at.exists(), "{} left behind", at.display());
            let (l, file) = bind_unix(&at, 0o600).expect("a start after it");
            drop(l);
            remove_unix(&file);
            std::fs::remove_dir_all(&d).unwrap();
        }

        /// The bind sets the working directory and the umask for a moment; afterwards both
        /// are what they were (the directory by identity), whether it succeeded or failed
        /// at the bind or at the listen. A failure leaves nothing behind. Run on a thread
        /// with its own working directory and umask, as the binary's bind runs alone.
        #[test]
        fn the_bind_restores_the_working_directory_and_umask_on_every_path() {
            let d = dir("restore");
            let base = d.clone();
            std::thread::spawn(move || {
                cvt(unsafe { libc::unshare(libc::CLONE_FS) }).unwrap();
                unsafe { libc::umask(0o027) };
                std::env::set_current_dir(&base).unwrap();
                let here = || {
                    let st = std::fs::metadata(".").unwrap();
                    use std::os::unix::fs::MetadataExt;
                    (st.dev(), st.ino())
                };
                let umask = || unsafe {
                    let u = libc::umask(0);
                    libc::umask(u);
                    u
                };
                let before = (here(), umask());
                for (i, fail) in [None, Some("bind"), Some("listen")].into_iter().enumerate() {
                    let at = base.join(format!("r{i}"));
                    FAIL_AT.with(|f| f.set(fail));
                    let r = bind_unix_std(&at, 0o600);
                    assert_eq!((here(), umask()), before, "{fail:?}");
                    match fail {
                        None => {
                            let (l, file) = r.unwrap();
                            assert!(at.join("s").exists());
                            drop(l);
                            remove_unix(&file);
                        }
                        Some(step) => {
                            let e = r.unwrap_err();
                            assert!(e.to_string().contains(step), "{e}");
                        }
                    }
                    assert!(!at.exists(), "{fail:?}: {} left behind", at.display());
                }
            })
            .join()
            .unwrap();
            std::fs::remove_dir_all(&d).unwrap();
        }

        /// Only the server's own uid is let in, and never the overflow uid an unmapped user
        /// reads as, even for a server that runs as it.
        #[test]
        fn only_the_servers_own_uid_is_admitted() {
            assert!(admits(1000, 1000));
            assert!(admits(0, 0));
            assert!(!admits(1001, 1000));
            assert!(!admits(0, 1000));
            assert!(!admits(OVERFLOW_UID, 1000));
            assert!(!admits(OVERFLOW_UID, OVERFLOW_UID));
        }

        /// A client running as another user (nobody, through `sudo -n`) reaches the socket
        /// (mode 0666) and gets nothing: the connection closes before a byte either way.
        /// Skipped where `sudo -n` or python3 isn't there.
        #[tokio::test]
        async fn another_user_gets_nothing_through_the_socket() {
            let ok = |c: &mut std::process::Command| c.output().is_ok_and(|o| o.status.success());
            if !ok(std::process::Command::new("sudo")
                .args(["-n", "-u", "nobody", "python3", "-c", ""]))
            {
                eprintln!("skipped: no `sudo -n -u nobody python3` here");
                return;
            }
            let (hub, _input) = wired();
            let d = dir("other-user");
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o711)).unwrap();
            let path = d.join("sf");
            let (l, file) = bind_unix(&path, 0o666).unwrap();
            let server = tokio::spawn(
                axum::serve(PeerChecked(l), router(app(&hub, false, None))).into_future(),
            );
            let client = "import socket, sys\n\
                s = socket.socket(socket.AF_UNIX)\n\
                s.connect(sys.argv[1])\n\
                s.settimeout(5)\n\
                try:\n    s.sendall(b'GET /healthz HTTP/1.1\\r\\nHost: localhost\\r\\n\\r\\n')\n\
                except OSError:\n    pass\n\
                print(len(s.recv(100)))\n";
            let socket = unix_socket_path(&path);
            let to = socket.clone();
            let out = tokio::task::spawn_blocking(move || {
                std::process::Command::new("sudo")
                    .args(["-n", "-u", "nobody", "python3", "-c", client])
                    .arg(&to)
                    .output()
            })
            .await
            .unwrap()
            .unwrap();
            let said = String::from_utf8_lossy(&out.stdout);
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert_eq!(said.trim(), "0", "the other user read {said} bytes");
            assert!(hub.clients.lock().is_empty());
            // The server's own user still gets in.
            let mut own = tokio::net::UnixStream::connect(&socket).await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            own.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            let mut reply = Vec::new();
            own.read_to_end(&mut reply).await.unwrap();
            assert!(
                reply.starts_with(b"HTTP/1.1 200"),
                "{}",
                String::from_utf8_lossy(&reply)
            );
            server.abort();
            drop(file);
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::remove_dir_all(&d).unwrap();
        }
    }
}
