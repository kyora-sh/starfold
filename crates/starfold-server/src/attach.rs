//! `starfold attach DIR`: one connection to the socket that `--listen-unix DIR` serves,
//! relayed byte for byte to this process's stdin and stdout. A host that runs one per
//! connection (a pod exec stream, say) lets many viewers share one server.

pub const USAGE: &str = "\
Usage: starfold attach DIR

Connects to the socket DIR/s that `starfold --listen-unix DIR` serves and relays bytes
between it and stdin/stdout, unchanged, until either side ends (Linux only). Run one per
connection: the server's own login and host checks apply to what is relayed.

Before a byte is relayed it checks what the server checks: this user namespace's uid map
is the identity, and every directory from / to DIR belongs to this user or root and lets
no other user rename entries in it; DIR itself must not be a symlink, nor DIR/s. It
connects without waiting for room in a full backlog, then reads the uid the kernel recorded
for the listener when it called listen (SO_PEERCRED) and refuses unless it is this
process's own.

stdout carries only relayed bytes; errors go to stderr, with exit status 1 (2 for a bad
command line). At the end of stdin the socket's write side is shut down and the server's
reply relayed to its end; at the end of the socket attach exits 0.
";

/// Runs `starfold attach` with the arguments after `attach`, and returns the exit status.
pub fn main(args: &[String]) -> i32 {
    match args {
        [a] if a == "--help" || a == "-h" => {
            print!("{USAGE}");
            0
        }
        [dir] if !dir.is_empty() && !dir.starts_with('-') => run(std::path::Path::new(dir)),
        _ => {
            eprintln!("usage: starfold attach DIR (see starfold attach --help)");
            2
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn run(_dir: &std::path::Path) -> i32 {
    eprintln!(
        "starfold attach: not available on this platform: it reaches --listen-unix, which is \
         Linux only"
    );
    1
}

#[cfg(target_os = "linux")]
fn run(dir: &std::path::Path) -> i32 {
    let sock = match connect(dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("starfold attach {}: {e}", dir.display());
            return 1;
        }
    };
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("starfold attach: {e}");
            return 1;
        }
    };
    let done = rt.block_on(async {
        let stdin = Stdio::of(0).map_err(|e| format!("stdin: {e}"))?;
        let stdout = Stdio::of(1).map_err(|e| format!("stdout: {e}"))?;
        let sock = tokio::net::UnixStream::from_std(sock).map_err(|e| e.to_string())?;
        relay(stdin, stdout, sock).await
    });
    rt.shutdown_background();
    match done {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("starfold attach {}: {e}", dir.display());
            1
        }
    }
}

/// Connects to `dir`'s socket after the checks the server makes on its own side: the
/// identity uid map (so a uid read is exact) and the walk to `dir`'s parent, which must
/// pass the rules `--listen-unix` holds its path to. `dir` itself must be a directory (not
/// a symlink) that passes them too, and `s` in it a socket (not a symlink). The connect is
/// made relative to the directory as opened, never by a path, and doesn't wait for room in
/// a full backlog. The connection is kept only if its listener has this process's uid.
///
/// It changes the working directory for the connect (and back), so it runs while this is
/// the process's only thread: [`crate::main`] calls `attach` first thing.
#[cfg(target_os = "linux")]
pub fn connect(dir: &std::path::Path) -> Result<std::os::unix::net::UnixStream, String> {
    use crate::server;
    use std::path::Path;
    const WHO: &str = "starfold attach";
    server::identity_uid_map(Path::new("/proc")).map_err(|e| {
        format!(
            "refusing: {e}; attach runs only in a user namespace whose uid map is the identity \
             (\"0 0 4294967295\"), where the listener's uid is exact"
        )
    })?;
    let name = dir
        .file_name()
        .ok_or_else(|| format!("{} has no directory name", dir.display()))?;
    let parent_path = dir
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = server::open_trusted_dir(parent_path, dir, WHO).map_err(|e| e.to_string())?;
    let name = server::cname(name).map_err(|e| e.to_string())?;
    let symlink = |at: &std::os::fd::OwnedFd, name: &std::ffi::CStr| {
        server::stat_at(at, name).is_ok_and(|st| st.st_mode & libc::S_IFMT == libc::S_IFLNK)
    };
    let held = server::open_at(Some(&parent), &name, libc::O_PATH | libc::O_DIRECTORY).map_err(
        |e| match e.raw_os_error() {
            Some(libc::ELOOP | libc::ENOTDIR) if symlink(&parent, &name) => format!(
                "{} is a symlink; attach takes the directory itself",
                dir.display()
            ),
            _ => format!("{}: {e}", dir.display()),
        },
    )?;
    server::check_dir(&held, dir, dir, WHO).map_err(|e| e.to_string())?;
    let socket = server::unix_socket_path(dir);
    let s = c"s";
    let st = server::stat_at(&held, s).map_err(|e| format!("{}: {e}", socket.display()))?;
    match st.st_mode & libc::S_IFMT {
        libc::S_IFSOCK => {}
        libc::S_IFLNK => return Err(format!("{} is a symlink, not a socket", socket.display())),
        _ => return Err(format!("{} is not a socket", socket.display())),
    }
    let threads = std::fs::read_dir("/proc/self/task").map(|d| d.count());
    if !matches!(threads, Ok(1)) {
        return Err(format!(
            "it connects while this is the process's only thread, and this process has {}",
            match threads {
                Ok(n) => format!("{n} threads"),
                Err(e) => format!("threads that can't be counted ({e})"),
            }
        ));
    }
    let sock = connect_in(&held, s).map_err(|e| match e.raw_os_error() {
        Some(libc::EAGAIN) => format!(
            "{}: the listener's backlog is full; not waiting for room",
            socket.display()
        ),
        _ => format!("{}: {e}", socket.display()),
    })?;
    let own = unsafe { libc::geteuid() };
    listener_is_own(&sock, own).map_err(|e| format!("{}: {e}", socket.display()))?;
    Ok(sock)
}

/// A nonblocking connect to the socket `name` in `dir`: the working directory goes to
/// `dir` for the connect and back after it. A full backlog is `EAGAIN`, not a wait.
#[cfg(target_os = "linux")]
fn connect_in(
    dir: &std::os::fd::OwnedFd,
    name: &std::ffi::CStr,
) -> std::io::Result<std::os::unix::net::UnixStream> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let cvt = |r: libc::c_int| match r {
        r if r < 0 => Err(std::io::Error::last_os_error()),
        r => Ok(r),
    };
    let sock = cvt(unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    })?;
    let sock = unsafe { OwnedFd::from_raw_fd(sock) };
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (d, b) in addr.sun_path.iter_mut().zip(name.to_bytes()) {
        *d = *b as libc::c_char;
    }
    let back = crate::server::open_at(None, c".", libc::O_PATH | libc::O_DIRECTORY).ok();
    cvt(unsafe { libc::fchdir(dir.as_raw_fd()) })?;
    let r = cvt(unsafe {
        libc::connect(
            sock.as_raw_fd(),
            (&raw const addr).cast(),
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    });
    if let Some(back) = back {
        unsafe { libc::fchdir(back.as_raw_fd()) };
    }
    r?;
    Ok(std::os::unix::net::UnixStream::from(sock))
}

/// stdin or stdout as the relay uses it. A pipe or a socket is driven by readiness
/// (nonblocking, with its original flags put back when it is dropped), so one handed over
/// already nonblocking works as one that isn't; anything else (a tty, a file) through
/// tokio's blocking file I/O.
#[cfg(target_os = "linux")]
enum Stdio {
    Ready(Ready),
    File(tokio::fs::File),
}

/// A duplicate of stdin or stdout, set nonblocking, and its flags as they were.
#[cfg(target_os = "linux")]
struct Ready {
    fd: tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>,
    flags: libc::c_int,
}

#[cfg(target_os = "linux")]
impl Drop for Ready {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_SETFL, self.flags) };
    }
}

#[cfg(target_os = "linux")]
impl Stdio {
    fn of(fd: std::os::fd::RawFd) -> std::io::Result<Stdio> {
        use std::os::fd::FromRawFd;
        let err = || std::io::Error::last_os_error();
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut st) } < 0 {
            return Err(err());
        }
        if !matches!(st.st_mode & libc::S_IFMT, libc::S_IFIFO | libc::S_IFSOCK) {
            return Ok(Stdio::File(unsafe { tokio::fs::File::from_raw_fd(fd) }));
        }
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            return Err(err());
        }
        let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        if dup < 0 {
            return Err(err());
        }
        let dup = unsafe { std::os::fd::OwnedFd::from_raw_fd(dup) };
        if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(err());
        }
        let ready = tokio::io::unix::AsyncFd::new(dup);
        let ready = ready.map(|fd| Ready { fd, flags });
        if ready.is_err() {
            unsafe { libc::fcntl(fd, libc::F_SETFL, flags) };
        }
        Ok(Stdio::Ready(ready?))
    }
}

/// One `read` or `write` on a ready descriptor: its count, or the error.
#[cfg(target_os = "linux")]
fn counted(n: isize) -> std::io::Result<usize> {
    match n {
        n if n < 0 => Err(std::io::Error::last_os_error()),
        n => Ok(n as usize),
    }
}

#[cfg(target_os = "linux")]
impl tokio::io::AsyncRead for Stdio {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        use std::os::fd::AsRawFd;
        use std::task::{ready, Poll};
        match self.get_mut() {
            Stdio::File(f) => std::pin::Pin::new(f).poll_read(cx, buf),
            Stdio::Ready(r) => loop {
                let mut guard = ready!(r.fd.poll_read_ready(cx))?;
                let unfilled = buf.initialize_unfilled();
                let read = guard.try_io(|fd| {
                    counted(unsafe {
                        libc::read(fd.as_raw_fd(), unfilled.as_mut_ptr().cast(), unfilled.len())
                    })
                });
                match read {
                    Ok(Ok(n)) => {
                        buf.advance(n);
                        return Poll::Ready(Ok(()));
                    }
                    Ok(Err(e)) => return Poll::Ready(Err(e)),
                    Err(_) => continue,
                }
            },
        }
    }
}

#[cfg(target_os = "linux")]
impl tokio::io::AsyncWrite for Stdio {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        data: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        use std::os::fd::AsRawFd;
        use std::task::{ready, Poll};
        match self.get_mut() {
            Stdio::File(f) => std::pin::Pin::new(f).poll_write(cx, data),
            Stdio::Ready(r) => loop {
                let mut guard = ready!(r.fd.poll_write_ready(cx))?;
                let written = guard.try_io(|fd| {
                    counted(unsafe {
                        libc::write(fd.as_raw_fd(), data.as_ptr().cast(), data.len())
                    })
                });
                if let Ok(n) = written {
                    return Poll::Ready(n);
                }
            },
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Stdio::File(f) => std::pin::Pin::new(f).poll_flush(cx),
            Stdio::Ready(_) => std::task::Poll::Ready(Ok(())),
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Stdio::File(f) => std::pin::Pin::new(f).poll_shutdown(cx),
            Stdio::Ready(_) => std::task::Poll::Ready(Ok(())),
        }
    }
}

/// The listener's uid, from the credentials the kernel recorded when it called `listen`
/// (SO_PEERCRED on our end), must be `own`.
#[cfg(target_os = "linux")]
fn listener_is_own(sock: &std::os::unix::net::UnixStream, own: u32) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let r = unsafe {
        libc::getsockopt(
            sock.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &mut len,
        )
    };
    if r < 0 {
        return Err(format!(
            "cannot read the listener's credentials: {}",
            std::io::Error::last_os_error()
        ));
    }
    if crate::server::admits(cred.uid, own) {
        Ok(())
    } else {
        Err(format!(
            "refusing: the listener is uid {}, not this process's uid {own}",
            cred.uid
        ))
    }
}

/// Where a copy toward the socket stopped.
#[cfg(target_os = "linux")]
enum Up {
    Input(std::io::Error),
    Socket,
}

/// Copies `input` to `sock` and `sock` to `output` until the socket ends. The end of
/// `input` shuts the socket's write side down and the copy from it goes on; the socket's
/// end is the relay's (`output` flushed). A failed write to the socket means the server
/// went: what it sent before is still relayed. Any other error ends the relay with it.
#[cfg(target_os = "linux")]
pub async fn relay<I, O, S>(mut input: I, mut output: O, sock: S) -> Result<(), String>
where
    I: tokio::io::AsyncRead + Unpin,
    O: tokio::io::AsyncWrite + Unpin,
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    const CHUNK: usize = 64 * 1024;
    let (mut from, mut to) = tokio::io::split(sock);
    let up = async {
        let mut buf = vec![0u8; CHUNK];
        loop {
            let n = input.read(&mut buf).await.map_err(Up::Input)?;
            if n == 0 {
                return to.shutdown().await.map_err(|_| Up::Socket);
            }
            to.write_all(&buf[..n]).await.map_err(|_| Up::Socket)?;
        }
    };
    let down = async {
        let mut buf = vec![0u8; CHUNK];
        loop {
            let n = from
                .read(&mut buf)
                .await
                .map_err(|e| format!("reading the socket: {e}"))?;
            if n == 0 {
                return Ok(());
            }
            output
                .write_all(&buf[..n])
                .await
                .map_err(|e| format!("writing stdout: {e}"))?;
            output
                .flush()
                .await
                .map_err(|e| format!("writing stdout: {e}"))?;
        }
    };
    tokio::pin!(up, down);
    tokio::select! {
        r = &mut down => r,
        r = &mut up => match r {
            Ok(()) | Err(Up::Socket) => down.await,
            Err(Up::Input(e)) => Err(format!("reading stdin: {e}")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Anything but one directory (or --help) is a usage error, before anything is touched.
    #[test]
    fn one_directory_or_a_usage_error() {
        for bad in [&[][..], &["a", "b"], &["--dir=a"], &["-x"], &[""]] {
            assert_eq!(main(&args(bad)), 2, "{bad:?}");
        }
        assert_eq!(main(&args(&["--help"])), 0);
        assert!(USAGE.starts_with("Usage: starfold attach DIR\n"));
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn off_linux_attach_is_refused() {
        assert_eq!(main(&args(&["/nonexistent"])), 1);
    }
}
