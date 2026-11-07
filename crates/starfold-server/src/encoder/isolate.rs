//! Hardware probes in a child process: the server runs its own binary again with
//! `--probe-encoders <backend>`, once per backend, so a driver that crashes or hangs
//! costs that backend and nothing else.

use super::caps::{Diagnostic, EncoderCapability, Probe, ProbeProvider};
use super::Backend;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[cfg(test)]
mod tests;

/// The hidden flag, first on the child's command line.
pub const FLAG: &str = "--probe-encoders";
const VERSION: u32 = 1;
/// More than any probe prints; a child that writes more is not one of ours.
const MAX_OUTPUT: u64 = 1 << 20;
/// How long the child's output may stay open once it has exited or been killed.
const CLOSE_GRACE: Duration = Duration::from_secs(1);

static SELF_EXEC: AtomicBool = AtomicBool::new(false);

/// Marks the current executable as one that answers [`FLAG`].
pub fn enable() {
    SELF_EXEC.store(true, Ordering::Relaxed);
}

/// A provider that probes `backend` in a child, if this executable takes [`FLAG`].
pub fn child(backend: Backend, experimental: bool, timeout: Duration) -> Option<Child> {
    if !SELF_EXEC.load(Ordering::Relaxed) {
        return None;
    }
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            log::warn!("encoder probe in-process: no path to this executable ({e})");
            return None;
        }
    };
    Some(Child {
        backend,
        command: Box::new(move || {
            let mut cmd = Command::new(&exe);
            cmd.arg(FLAG).arg(backend.name());
            if experimental {
                cmd.arg("--experimental-hw-probes");
            }
            cmd
        }),
        timeout,
    })
}

/// What a child prints: one line of JSON.
#[derive(Debug, Serialize, Deserialize)]
pub struct Report {
    pub probe: u32,
    pub backend: Backend,
    pub caps: Vec<EncoderCapability>,
    pub diagnostics: Vec<Diagnostic>,
}

/// The child's side, given the arguments after [`FLAG`]; returns the exit code.
pub fn child_main(args: &[String]) -> i32 {
    let experimental = args.iter().any(|a| a == "--experimental-hw-probes");
    let Some(p) = args.first().and_then(|name| {
        super::caps::hardware_providers(experimental)
            .into_iter()
            .find(|p| p.backend().name() == name)
    }) else {
        eprintln!("{FLAG}: no such hardware probe in this build: {args:?}");
        return 2;
    };
    let mut out = report_sink();
    let found = p.probe();
    let report = Report {
        probe: VERSION,
        backend: p.backend(),
        caps: found.caps,
        diagnostics: found.diagnostics,
    };
    let Ok(line) = serde_json::to_string(&report) else {
        return 1;
    };
    match writeln!(out, "{line}").and_then(|_| out.flush()) {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

/// Where the report goes: the original stdout, with fd 1 pointed at stderr so whatever a
/// driver prints can't be taken for it.
#[cfg(unix)]
fn report_sink() -> Box<dyn Write> {
    use std::os::fd::FromRawFd;
    unsafe {
        let fd = libc::dup(1);
        if fd < 0 || libc::dup2(2, 1) < 0 {
            return Box::new(std::io::stdout());
        }
        Box::new(std::fs::File::from_raw_fd(fd))
    }
}

#[cfg(not(unix))]
fn report_sink() -> Box<dyn Write> {
    Box::new(std::io::stdout())
}

/// A backend probed by [`run`]ning a child.
pub struct Child {
    pub backend: Backend,
    pub command: Box<dyn Fn() -> Command + Send>,
    pub timeout: Duration,
}

impl ProbeProvider for Child {
    fn backend(&self) -> Backend {
        self.backend
    }

    fn probe(&self) -> Probe {
        run((self.command)(), self.backend, self.timeout)
    }
}

/// Runs `cmd` and reads its report on `backend`. A child that is still running after
/// `timeout` is killed and reaped; one that crashes, fails or prints something else
/// leaves a diagnostic in place of encoders. On Unix the child leads its own process
/// group, and whatever is left in that group is killed before the child is reaped, while
/// its pid can't have been reused.
pub fn run(mut cmd: Command, backend: Backend, timeout: Duration) -> Probe {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Probe::failed(backend, format!("probe didn't start: {e}")),
    };
    let (tx, rx) = mpsc::channel();
    if let Some(stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let r = stdout.take(MAX_OUTPUT + 1).read_to_end(&mut buf);
            let _ = tx.send(r.map(|_| buf));
        });
    }
    let deadline = Instant::now() + timeout;
    let late = loop {
        match finished(&mut child) {
            Ok(true) => break None,
            Ok(false) if Instant::now() >= deadline => {
                break Some(format!(
                    "probe did not finish in {} s and was killed",
                    timeout.as_secs()
                ))
            }
            Ok(false) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => break Some(format!("probe wait failed: {e}")),
        }
    };
    kill(&mut child);
    let status = child.wait();
    if let Some(e) = late {
        return Probe::failed(backend, e);
    }
    let status = match status {
        Ok(s) => s,
        Err(e) => return Probe::failed(backend, format!("probe wait failed: {e}")),
    };
    if !status.success() {
        return Probe::failed(backend, exited(status));
    }
    let out = match rx.recv_timeout(CLOSE_GRACE) {
        Ok(Ok(b)) if b.len() as u64 <= MAX_OUTPUT => b,
        Ok(Ok(_)) => return Probe::failed(backend, "probe printed more than 1 MiB"),
        Ok(Err(e)) => return Probe::failed(backend, format!("probe output: {e}")),
        Err(_) => return Probe::failed(backend, "probe output stayed open after it exited"),
    };
    parse(&out, backend)
}

/// The report in a child's output: its last line.
pub fn parse(out: &[u8], backend: Backend) -> Probe {
    let text = String::from_utf8_lossy(out);
    let Some(line) = text.lines().rev().find(|l| !l.trim().is_empty()) else {
        return Probe::failed(backend, "probe printed no report");
    };
    let r: Report = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => return Probe::failed(backend, format!("probe report unreadable: {e}")),
    };
    if r.probe != VERSION {
        return Probe::failed(
            backend,
            format!("probe report version {} (want {VERSION})", r.probe),
        );
    }
    let foreign = std::iter::once(r.backend)
        .chain(r.caps.iter().map(|c| c.backend))
        .chain(r.diagnostics.iter().map(|d| d.backend))
        .find(|&b| b != backend);
    if let Some(f) = foreign {
        return Probe::failed(backend, format!("probe reported on {}", f.name()));
    }
    Probe {
        caps: r.caps,
        diagnostics: r.diagnostics,
    }
}

/// Whether the child has exited, leaving it unreaped.
#[cfg(unix)]
fn finished(child: &mut std::process::Child) -> std::io::Result<bool> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let r = unsafe {
        libc::waitid(
            libc::P_PID,
            child.id() as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if r != 0 {
        return Err(std::io::Error::last_os_error());
    }
    #[cfg(target_os = "linux")]
    let pid = unsafe { info.si_pid() };
    #[cfg(not(target_os = "linux"))]
    let pid = info.si_pid;
    Ok(pid != 0)
}

#[cfg(not(unix))]
fn finished(child: &mut std::process::Child) -> std::io::Result<bool> {
    child.try_wait().map(|s| s.is_some())
}

/// Kills the child's process group; the unreaped child keeps its id from being reused.
#[cfg(unix)]
fn kill(child: &mut std::process::Child) {
    if let Ok(pid) = libc::pid_t::try_from(child.id()) {
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn kill(child: &mut std::process::Child) {
    let _ = child.kill();
}

#[cfg(unix)]
fn exited(s: std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match (s.signal(), s.code()) {
        (Some(sig), _) => format!("probe exited with signal {sig}"),
        (None, Some(c)) => format!("probe exited with status {c}"),
        (None, None) => format!("probe exited ({s})"),
    }
}

#[cfg(not(unix))]
fn exited(s: std::process::ExitStatus) -> String {
    match s.code() {
        Some(c) => format!("probe exited with status {c:#x}"),
        None => format!("probe exited ({s})"),
    }
}
