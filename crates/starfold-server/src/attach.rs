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
no other user rename entries in it. It then reads the listening process's uid
(SO_PEERCRED) and refuses unless it is this process's own.

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
    use std::os::fd::FromRawFd;
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
    let (stdin, stdout) = unsafe {
        (
            tokio::fs::File::from_raw_fd(0),
            tokio::fs::File::from_raw_fd(1),
        )
    };
    let done = rt.block_on(async {
        let sock = connect(dir)?;
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
/// identity uid map (so a uid read is exact) and the walk to `dir`, which must pass the
/// rules `--listen-unix` holds its path to. The socket is reached through the directory as
/// the walk opened it, and kept only if its listener has this process's uid.
#[cfg(target_os = "linux")]
pub fn connect(dir: &std::path::Path) -> Result<tokio::net::UnixStream, String> {
    use crate::server;
    use std::os::fd::AsRawFd;
    server::identity_uid_map(std::path::Path::new("/proc")).map_err(|e| {
        format!(
            "refusing: {e}; attach runs only in a user namespace whose uid map is the identity \
             (\"0 0 4294967295\"), where the listener's uid is exact"
        )
    })?;
    let held = server::open_trusted_dir(dir, dir, "starfold attach").map_err(|e| e.to_string())?;
    let socket = server::unix_socket_path(dir);
    let through = std::path::Path::new("/proc/self/fd")
        .join(held.as_raw_fd().to_string())
        .join(socket.file_name().unwrap());
    let sock = std::os::unix::net::UnixStream::connect(&through)
        .map_err(|e| format!("{}: {e}", socket.display()))?;
    let own = unsafe { libc::geteuid() };
    listener_is_own(&sock, own).map_err(|e| format!("{}: {e}", socket.display()))?;
    sock.set_nonblocking(true).map_err(|e| e.to_string())?;
    tokio::net::UnixStream::from_std(sock).map_err(|e| e.to_string())
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
