//! Hardware probes in a child process: the server runs its own binary again with
//! `--probe-encoders <backend>`, once per backend, so a driver that crashes or hangs
//! costs that backend and nothing else.

use super::caps::{Diagnostic, EncoderCapability, Probe, ProbeProvider};
use super::Backend;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::process::{ChildStdout, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
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
/// How long a killed child may take to exit.
const KILL_GRACE: Duration = Duration::from_secs(2);

static SELF_EXEC: AtomicBool = AtomicBool::new(false);
/// Killed children that outlived [`KILL_GRACE`] and got no reaper thread.
static STUCK: Mutex<Vec<std::process::Child>> = Mutex::new(Vec::new());

/// Marks the current executable as one that answers [`FLAG`]. Call it before any other
/// thread starts: it also gives `SIGCHLD` its default action back, as an ignored one
/// inherited across exec would have exited children reaped behind the server's back.
pub fn enable() {
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGCHLD, libc::SIG_DFL);
    }
    SELF_EXEC.store(true, Ordering::Relaxed);
}

/// A provider that probes `backend` in a child, if this executable takes [`FLAG`].
pub fn child(backend: Backend, experimental: bool, timeout: Duration) -> Option<Child> {
    if !SELF_EXEC.load(Ordering::Relaxed) {
        return None;
    }
    Some(Child {
        backend,
        command: Box::new(move || {
            let exe =
                std::env::current_exe().map_err(|e| format!("no path to this executable: {e}"))?;
            let mut cmd = Command::new(exe);
            cmd.arg(FLAG).arg(backend.name());
            if experimental {
                cmd.arg("--experimental-hw-probes");
            }
            Ok(cmd)
        }),
        timeout,
    })
}

/// Opens and runs a `backend` session for `spec` in a child, if this executable takes
/// [`FLAG`].
pub fn session(backend: Backend, spec: &str, timeout: Duration) -> Option<Probe> {
    if !SELF_EXEC.load(Ordering::Relaxed) {
        return None;
    }
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            return Some(Probe::failed(
                backend,
                format!("no path to this executable: {e}"),
            ))
        }
    };
    let mut cmd = Command::new(exe);
    cmd.arg(FLAG).arg(backend.name()).arg(SESSION).arg(spec);
    Some(run(cmd, backend, timeout))
}

/// After [`FLAG`] and a backend: open one session (`<codec>:<w>x<h>@<fps>`) and encode a
/// sample, rather than probe.
pub const SESSION: &str = "--session";

/// What a child prints: one line of JSON.
#[derive(Debug, Serialize, Deserialize)]
pub struct Report {
    pub probe: u32,
    pub backend: Backend,
    pub caps: Vec<EncoderCapability>,
    pub diagnostics: Vec<Diagnostic>,
}

/// What the parent writes on a child's stdin once the child can't outlive it.
pub const GO: u8 = b'g';
/// The exit code of a child whose stdin closed without [`GO`].
pub const NO_GO: i32 = 3;

/// The child's side, given the arguments after [`FLAG`]; returns the exit code. It does
/// nothing before [`GO`] arrives: no probing, spawning or library loading.
pub fn child_main(args: &[String]) -> i32 {
    let mut go = [0u8];
    if std::io::stdin().read_exact(&mut go).is_err() || go != [GO] {
        return NO_GO;
    }
    probe_main(args)
}

fn probe_main(args: &[String]) -> i32 {
    if let Some(i) = args.iter().position(|a| a == SESSION) {
        return session_main(args.first().map(String::as_str), args.get(i + 1));
    }
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
    print_report(&mut out, p.backend(), found)
}

#[cfg(any(target_os = "linux", windows))]
fn session_main(backend: Option<&str>, spec: Option<&String>) -> i32 {
    let open: fn(&str) -> Result<(), String> = match backend {
        #[cfg(target_os = "linux")]
        Some("vaapi") => super::vaapi::child_session,
        Some("nvenc") => super::nvenc::child_session,
        #[cfg(windows)]
        Some("mediafoundation") => super::mf::child_session,
        _ => {
            eprintln!("{FLAG}: {SESSION} takes vaapi, nvenc or mediafoundation and a session");
            return 2;
        }
    };
    let (Some(backend), Some(spec)) = (backend.and_then(|b| Backend::parse(b).ok()), spec) else {
        eprintln!("{FLAG}: {SESSION} takes a backend and a session");
        return 2;
    };
    let mut out = report_sink();
    let found = match open(spec) {
        Ok(()) => Probe::default(),
        Err(e) => Probe::failed(backend, e),
    };
    print_report(&mut out, backend, found)
}

#[cfg(not(any(target_os = "linux", windows)))]
fn session_main(_: Option<&str>, _: Option<&String>) -> i32 {
    eprintln!("{FLAG}: {SESSION} is Linux and Windows only");
    2
}

fn print_report(out: &mut dyn Write, backend: Backend, found: Probe) -> i32 {
    let report = Report {
        probe: VERSION,
        backend,
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
    pub command: Box<dyn Fn() -> Result<Command, String> + Send>,
    pub timeout: Duration,
}

impl ProbeProvider for Child {
    fn backend(&self) -> Backend {
        self.backend
    }

    fn probe(&self) -> Probe {
        match (self.command)() {
            Ok(cmd) => run(cmd, self.backend, self.timeout),
            Err(e) => Probe::failed(self.backend, format!("probe didn't start: {e}")),
        }
    }

    fn bounded(&self) -> bool {
        true
    }
}

/// Runs `cmd` and reads its report on `backend`. A child that is still running after
/// `timeout` is killed and reaped; one that crashes, fails or prints something else
/// leaves a diagnostic in place of encoders. Returns within `timeout` plus a few seconds.
///
/// On Unix the child leads its own process group, and whatever is left in that group is
/// killed while the child is known to be unreaped, so its pid can't have been reused. On
/// Windows the child starts in a job object, and everything in the job is killed.
pub fn run(mut cmd: Command, backend: Backend, timeout: Duration) -> Probe {
    reap_stuck();
    if auto_reaps() {
        return Probe::failed(
            backend,
            "probe not run: SIGCHLD is ignored, so its child couldn't be killed safely",
        );
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::inherit());
    let (mut child, group) = match Group::spawn(&mut cmd) {
        Ok(c) => c,
        Err(e) => return Probe::failed(backend, format!("probe didn't start: {e}")),
    };
    let mut out = match Output::new(child.stdout.take()) {
        Ok(o) => o,
        Err(e) => {
            if !finished(&mut child).as_ref().err().is_some_and(lost) {
                group.kill(&mut child);
            }
            let _ = reap(child);
            return Probe::failed(backend, e);
        }
    };
    let deadline = Instant::now() + timeout;
    let late = loop {
        out.pump();
        if out.too_long() {
            break Some(OutputError::TooLong.to_string());
        }
        match finished(&mut child) {
            Ok(true) => break None,
            Ok(false) if Instant::now() >= deadline => {
                break Some(format!(
                    "probe did not finish in {} s and was killed",
                    timeout.as_secs()
                ))
            }
            Ok(false) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) if lost(&e) => return Probe::failed(backend, format!("probe wait failed: {e}")),
            Err(e) => break Some(format!("probe wait failed: {e}")),
        }
    };
    group.kill(&mut child);
    let status = reap(child);
    if let Some(e) = late {
        return Probe::failed(backend, e);
    }
    let out = out.finish(CLOSE_GRACE);
    if let Err(e @ OutputError::TooLong) = &out {
        return Probe::failed(backend, e.to_string());
    }
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => return Probe::failed(backend, exited(s)),
        Err(e) => return Probe::failed(backend, e),
    }
    match out {
        Ok(b) => parse(&b, backend),
        Err(e) => Probe::failed(backend, e.to_string()),
    }
}

/// Reaps a child that exited or was killed, waiting at most [`KILL_GRACE`]: one stuck in
/// the kernel past that is left to a thread that reaps it whenever it does exit, and one
/// that can't be waited for now is held for the next probe to reap.
fn reap(mut child: std::process::Child) -> Result<ExitStatus, String> {
    let end = Instant::now() + KILL_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(s)) => return Ok(s),
            Ok(None) if Instant::now() >= end => break,
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => {
                if !lost(&e) {
                    STUCK.lock().unwrap_or_else(|e| e.into_inner()).push(child);
                }
                return Err(format!("probe wait failed: {e}"));
            }
        }
    }
    let pid = child.id();
    let held = std::sync::Arc::new(Mutex::new(Some(child)));
    let theirs = held.clone();
    let spawned = std::thread::Builder::new()
        .name("probe-reap".into())
        .spawn(move || {
            if let Some(mut c) = theirs.lock().unwrap_or_else(|e| e.into_inner()).take() {
                let _ = c.wait();
            }
        });
    if spawned.is_err() {
        if let Some(c) = held.lock().unwrap_or_else(|e| e.into_inner()).take() {
            STUCK.lock().unwrap_or_else(|e| e.into_inner()).push(c);
        }
    }
    Err(format!(
        "probe {pid} was killed but did not exit in {} s",
        KILL_GRACE.as_secs()
    ))
}

/// Whether a wait error means the child is no longer ours to signal: something else
/// reaped it and its pid may be in use again.
#[cfg(unix)]
fn lost(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(libc::ECHILD)
}

#[cfg(not(unix))]
fn lost(_: &std::io::Error) -> bool {
    false
}

/// Whether exited children are reaped without a wait, so a pid stops being ours.
#[cfg(unix)]
fn auto_reaps() -> bool {
    let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
    let r = unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut old) };
    r != 0 || old.sa_sigaction == libc::SIG_IGN || old.sa_flags & libc::SA_NOCLDWAIT != 0
}

#[cfg(not(unix))]
fn auto_reaps() -> bool {
    false
}

/// Reaps whichever held children have exited since.
fn reap_stuck() {
    let mut stuck = STUCK.lock().unwrap_or_else(|e| e.into_inner());
    stuck.retain_mut(|c| matches!(c.try_wait(), Ok(None)));
}

#[derive(Debug)]
enum OutputError {
    TooLong,
    Read(std::io::Error),
    Open,
}

impl std::fmt::Display for OutputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OutputError::TooLong => write!(f, "probe printed more than {} KiB", MAX_OUTPUT >> 10),
            OutputError::Read(e) => write!(f, "probe output: {e}"),
            OutputError::Open => f.write_str("probe output stayed open after it exited"),
        }
    }
}

/// The child's stdout, read without blocking from the waiting loop, so a child never
/// blocks on a full pipe and nothing is left reading once [`run`] returns.
struct Output<P: Pipe = ChildStdout> {
    pipe: Option<P>,
    buf: Vec<u8>,
    err: Option<OutputError>,
}

impl<P: Pipe> Output<P> {
    fn new(pipe: Option<P>) -> Result<Output<P>, String> {
        if let Some(p) = &pipe {
            p.nonblocking().map_err(|e| format!("probe output: {e}"))?;
        }
        Ok(Output {
            pipe,
            buf: Vec::new(),
            err: None,
        })
    }

    fn too_long(&self) -> bool {
        matches!(self.err, Some(OutputError::TooLong))
    }

    /// Reads what the child has written so far; true once the pipe is closed.
    fn pump(&mut self) -> bool {
        let Some(p) = self.pipe.as_mut() else {
            return true;
        };
        let mut chunk = [0u8; 8192];
        loop {
            match p.read_ready(&mut chunk) {
                Ok(Some(0)) => break,
                Ok(Some(n)) => {
                    self.buf.extend_from_slice(&chunk[..n]);
                    if self.buf.len() as u64 > MAX_OUTPUT {
                        self.err = Some(OutputError::TooLong);
                        break;
                    }
                }
                Ok(None) => return false,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    self.err = Some(OutputError::Read(e));
                    break;
                }
            }
        }
        self.pipe = None;
        true
    }

    fn finish(mut self, grace: Duration) -> Result<Vec<u8>, OutputError> {
        let end = Instant::now() + grace;
        while !self.pump() {
            if Instant::now() >= end {
                return Err(OutputError::Open);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        match self.err {
            Some(e) => Err(e),
            None => Ok(self.buf),
        }
    }
}

/// The read end of a pipe that [`Output`] polls.
trait Pipe: Read {
    fn nonblocking(&self) -> std::io::Result<()>;
    /// Reads what is there without waiting: `None` when nothing is, `Some(0)` at the end.
    fn read_ready(&mut self, buf: &mut [u8]) -> std::io::Result<Option<usize>>;
}

#[cfg(unix)]
impl<T: Read + std::os::fd::AsRawFd> Pipe for T {
    fn nonblocking(&self) -> std::io::Result<()> {
        let fd = self.as_raw_fd();
        let ok = unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            flags >= 0 && libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) >= 0
        };
        if ok {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    fn read_ready(&mut self, buf: &mut [u8]) -> std::io::Result<Option<usize>> {
        match self.read(buf) {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            r => r.map(Some),
        }
    }
}

/// Windows pipes have no non-blocking reads: only what `PeekNamedPipe` says is there
/// is read, so a pipe that a process outside the job still holds can't block the reader.
#[cfg(windows)]
impl<T: Read + std::os::windows::io::AsRawHandle> Pipe for T {
    fn nonblocking(&self) -> std::io::Result<()> {
        Ok(())
    }

    fn read_ready(&mut self, buf: &mut [u8]) -> std::io::Result<Option<usize>> {
        use windows::Win32::Foundation::{ERROR_BROKEN_PIPE, HANDLE};
        use windows::Win32::System::Pipes::PeekNamedPipe;
        let mut ready = 0u32;
        let peeked = unsafe {
            PeekNamedPipe(
                HANDLE(self.as_raw_handle()),
                None,
                0,
                None,
                Some(&mut ready),
                None,
            )
        };
        match peeked {
            Err(e) if e.code() == ERROR_BROKEN_PIPE.to_hresult() => Ok(Some(0)),
            Err(e) => Err(std::io::Error::other(e)),
            Ok(()) if ready == 0 => Ok(None),
            Ok(()) => {
                let n = buf.len().min(ready as usize);
                self.read(&mut buf[..n]).map(Some)
            }
        }
    }
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

/// Whether the child has exited, leaving it unreaped. An error (the child already reaped
/// behind our back, as with an inherited `SIGCHLD` of `SIG_IGN`) means its pid is no
/// longer ours to signal.
#[cfg(unix)]
fn finished(child: &mut std::process::Child) -> std::io::Result<bool> {
    loop {
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
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        #[cfg(target_os = "linux")]
        let pid = unsafe { info.si_pid() };
        #[cfg(not(target_os = "linux"))]
        let pid = info.si_pid;
        return Ok(pid != 0);
    }
}

#[cfg(not(unix))]
fn finished(child: &mut std::process::Child) -> std::io::Result<bool> {
    child.try_wait().map(|s| s.is_some())
}

/// Spawns `cmd` and sends it [`GO`] only once `confine` has made sure it can't outlive
/// this process. A child that is never confined (this process died first, or `confine`
/// failed) reads the end of its stdin and exits without doing anything. `cmd` keeps a
/// read end of that pipe, so writing [`GO`] to a child that already exited can't fail
/// with `SIGPIPE`.
fn start(
    cmd: &mut Command,
    confine: impl FnOnce(&std::process::Child) -> Result<(), String>,
) -> Result<std::process::Child, String> {
    let (stdin, mut go) = std::io::pipe().map_err(|e| format!("no pipe for its stdin: {e}"))?;
    cmd.stdin(stdin);
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    if let Err(e) = confine(&child) {
        drop(go);
        let _ = child.kill();
        let _ = reap(child);
        return Err(e);
    }
    let _ = go.write_all(&[GO]);
    Ok(child)
}

/// The child and whatever it starts, killed together.
#[cfg(unix)]
struct Group;

#[cfg(unix)]
impl Group {
    fn spawn(cmd: &mut Command) -> Result<(std::process::Child, Group), String> {
        std::os::unix::process::CommandExt::process_group(cmd, 0);
        start(cmd, |_| Ok(())).map(|c| (c, Group))
    }

    /// Kills the child's process group; the unreaped child keeps its id from being reused.
    fn kill(&self, child: &mut std::process::Child) {
        if let Ok(pid) = libc::pid_t::try_from(child.id()) {
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
    }
}

/// A job object that kills what is in it once its last handle closes, so the child's
/// descendants go with it, also when the server itself dies.
#[cfg(windows)]
struct Group(std::os::windows::io::OwnedHandle);

#[cfg(windows)]
impl Group {
    fn spawn(cmd: &mut Command) -> Result<(std::process::Child, Group), String> {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        let job = unsafe { CreateJobObjectW(None, windows::core::PCWSTR::null()) }
            .map_err(|e| format!("no job object: {e}"))?;
        let group = Group(unsafe { OwnedHandle::from_raw_handle(job.0) });
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
        unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&limits).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        }
        .map_err(|e| format!("job object limits: {e}"))?;
        let child = start(cmd, |c| {
            unsafe { AssignProcessToJobObject(job, HANDLE(c.as_raw_handle())) }
                .map_err(|e| format!("couldn't put it in a job object: {e}"))
        })?;
        Ok((child, group))
    }

    fn kill(&self, child: &mut std::process::Child) {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        let job = HANDLE(self.0.as_raw_handle());
        if unsafe { windows::Win32::System::JobObjects::TerminateJobObject(job, 1) }.is_err() {
            let _ = child.kill();
        }
    }
}

#[cfg(unix)]
fn exited(s: ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match (s.signal(), s.code()) {
        (Some(sig), _) => format!("probe exited with signal {sig}"),
        (None, Some(c)) => format!("probe exited with status {c}"),
        (None, None) => format!("probe exited ({s})"),
    }
}

#[cfg(not(unix))]
fn exited(s: ExitStatus) -> String {
    match s.code() {
        Some(c) => format!("probe exited with status {c:#x}"),
        None => format!("probe exited ({s})"),
    }
}
