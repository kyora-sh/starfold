//! A helper process and what the server shares with it: its stdin and stdout, read and
//! written without ever blocking, the shared section frames go through, and the means to
//! kill it. On Windows the helper starts suspended, inside a job object that kills it when
//! the server's handle closes (so it never outlives the server), inherits only its three
//! standard handles, and runs only once it is in the job. On Unix (the tests' fakes) it
//! leads its own process group.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A pipe end that never waits: `None` when nothing can be read or written right now.
pub trait Ready: Send {
    /// Reads what is there: `Some(0)` at the end.
    fn read_ready(&mut self, buf: &mut [u8]) -> std::io::Result<Option<usize>>;
    /// Writes what fits: `None` when nothing does.
    fn write_ready(&mut self, buf: &[u8]) -> std::io::Result<Option<usize>>;
}

/// Why a request was not written.
#[derive(Debug, PartialEq)]
pub enum Unwritten {
    /// The helper stopped reading: it missed the deadline.
    Late,
    /// Its stdin is gone: it exited or closed it.
    Closed(String),
}

/// Writes all of `buf` before `until`, or fails: a helper that stops reading costs the
/// server this deadline and no more.
pub fn write_by(p: &mut dyn Ready, mut buf: &[u8], until: Instant) -> Result<(), Unwritten> {
    while !buf.is_empty() {
        match p.write_ready(buf) {
            Ok(Some(0)) => return Err(Unwritten::Closed("closed its stdin".into())),
            Ok(Some(n)) => buf = &buf[n..],
            Ok(None) if Instant::now() >= until => return Err(Unwritten::Late),
            Ok(None) => std::thread::sleep(Duration::from_micros(200)),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(Unwritten::Closed(format!("could not be written to ({e})"))),
        }
    }
    Ok(())
}

/// The helper, reaped at most once.
pub struct Proc {
    inner: Mutex<Inner>,
    pid: u32,
}

impl Proc {
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// Kills the helper and whatever it started. Never waits.
    pub fn kill(&self) {
        let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !i.reaped {
            i.kill();
        }
    }

    /// Whether it has exited, reaping it if so. One that can't be waited for counts as
    /// still there.
    pub fn exited(&self) -> bool {
        let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !i.reaped && i.exited() {
            i.reaped = true;
        }
        i.reaped
    }
}

/// Kills every one of `procs` first, then reaps them all against one deadline, `grace`
/// away. Returns those still there after it: killed, but held in the kernel (a call stuck
/// in a driver); the caller keeps them.
pub fn end_all(procs: &[Arc<Proc>], grace: Duration) -> Vec<Arc<Proc>> {
    for p in procs {
        p.kill();
    }
    let end = Instant::now() + grace;
    let mut left: Vec<Arc<Proc>> = procs.to_vec();
    loop {
        left.retain(|p| !p.exited());
        if left.is_empty() || Instant::now() >= end {
            return left;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Why a helper didn't start, with any process that was started for it and is still
/// there after being killed: the caller keeps those, as it keeps any other.
pub struct NotStarted {
    pub why: String,
    pub left: Vec<Arc<Proc>>,
}

impl From<String> for NotStarted {
    fn from(why: String) -> NotStarted {
        NotStarted {
            why,
            left: Vec::new(),
        }
    }
}

/// The pipes the server holds: the helper's stdin and stdout.
pub struct Pipes {
    pub tx: Box<dyn Ready>,
    pub rx: Box<dyn Ready>,
}

#[cfg(unix)]
pub use unix::{spawn, Section};
#[cfg(windows)]
pub use win::{spawn_helper, Section};

#[cfg(unix)]
struct Inner {
    child: std::process::Child,
    reaped: bool,
}

#[cfg(unix)]
impl Inner {
    fn kill(&mut self) {
        if let Ok(pid) = libc::pid_t::try_from(self.child.id()) {
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
    }

    fn exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::process::{Command, Stdio};

    fn nonblocking(fd: i32) -> std::io::Result<()> {
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

    struct Fd<T>(T);

    impl<T: Read + Write + Send> Ready for Fd<T> {
        fn read_ready(&mut self, buf: &mut [u8]) -> std::io::Result<Option<usize>> {
            match self.0.read(buf) {
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
                r => r.map(Some),
            }
        }

        fn write_ready(&mut self, buf: &[u8]) -> std::io::Result<Option<usize>> {
            match self.0.write(buf) {
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
                r => r.map(Some),
            }
        }
    }

    /// Stdin only writes and stdout only reads; each side of [`Fd`] needs both traits.
    struct WriteOnly(std::process::ChildStdin);
    struct ReadOnly(std::process::ChildStdout);

    impl Read for WriteOnly {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::Unsupported.into())
        }
    }

    impl Write for WriteOnly {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.write(buf)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.0.flush()
        }
    }

    impl Read for ReadOnly {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.0.read(buf)
        }
    }

    impl Write for ReadOnly {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::Unsupported.into())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Starts `cmd` as a helper in a process group of its own.
    pub fn spawn(mut cmd: Command) -> Result<(Proc, Pipes), NotStarted> {
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("the helper didn't start: {e}"))?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("the helper has no pipes".to_string().into());
        };
        let pid = child.id();
        let proc = Proc {
            inner: Mutex::new(Inner {
                child,
                reaped: false,
            }),
            pid,
        };
        if let Err(e) = nonblocking(stdin.as_raw_fd()).and_then(|_| nonblocking(stdout.as_raw_fd()))
        {
            return Err(NotStarted {
                why: format!("the helper's pipes: {e}"),
                left: end_all(&[Arc::new(proc)], Duration::from_secs(2)),
            });
        }
        let pipes = Pipes {
            tx: Box::new(Fd(WriteOnly(stdin))),
            rx: Box::new(Fd(ReadOnly(stdout))),
        };
        Ok((proc, pipes))
    }

    /// Shared memory the frames go through. The tests' fakes don't map it, so it is
    /// anonymous here; only Windows hands it to the helper.
    pub struct Section {
        ptr: *mut u8,
        len: usize,
    }

    unsafe impl Send for Section {}

    impl Section {
        pub fn new(len: usize) -> Result<Section, String> {
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len.max(1),
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            if ptr == libc::MAP_FAILED {
                return Err(format!(
                    "no {len}-byte frame section: {}",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(Section {
                ptr: ptr.cast(),
                len,
            })
        }

        pub fn slice(&mut self) -> &mut [u8] {
            unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
        }

        pub fn share(&self, _: &Proc) -> Result<u64, String> {
            Ok(0)
        }
    }

    impl Drop for Section {
        fn drop(&mut self) {
            unsafe {
                libc::munmap(self.ptr.cast(), self.len.max(1));
            }
        }
    }
}

#[cfg(windows)]
struct Inner {
    process: std::os::windows::io::OwnedHandle,
    job: std::os::windows::io::OwnedHandle,
    /// It is in `job`: ending the job ends it and whatever it started.
    assigned: bool,
    reaped: bool,
}

#[cfg(windows)]
impl Inner {
    fn kill(&mut self) {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        let job = HANDLE(self.job.as_raw_handle());
        let ended = self.assigned
            && unsafe { windows::Win32::System::JobObjects::TerminateJobObject(job, 1) }.is_ok();
        if !ended {
            unsafe { win::TerminateProcess(self.process.as_raw_handle(), 1) };
        }
    }

    fn exited(&mut self) -> bool {
        use std::os::windows::io::AsRawHandle;
        unsafe { win::WaitForSingleObject(self.process.as_raw_handle(), 0) == 0 }
    }
}

#[cfg(windows)]
mod win {
    //! CreateProcessW with an explicit handle list and a suspended start, which
    //! `std::process::Command` doesn't offer, and the section calls. Prototypes as the
    //! Win32 documentation gives them.
    use super::*;
    use std::ffi::c_void;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

    type Handle = *mut c_void;
    type Bool = i32;

    #[repr(C)]
    struct SecurityAttributes {
        n_length: u32,
        security_descriptor: *mut c_void,
        inherit_handle: Bool,
    }

    #[repr(C)]
    struct StartupInfoW {
        cb: u32,
        reserved: *mut u16,
        desktop: *mut u16,
        title: *mut u16,
        x: u32,
        y: u32,
        x_size: u32,
        y_size: u32,
        x_count_chars: u32,
        y_count_chars: u32,
        fill_attribute: u32,
        flags: u32,
        show_window: u16,
        cb_reserved2: u16,
        reserved2: *mut u8,
        std_input: Handle,
        std_output: Handle,
        std_error: Handle,
    }

    #[repr(C)]
    struct StartupInfoExW {
        info: StartupInfoW,
        attribute_list: *mut c_void,
    }

    #[repr(C)]
    struct ProcessInformation {
        process: Handle,
        thread: Handle,
        process_id: u32,
        thread_id: u32,
    }

    const _: () = {
        assert!(std::mem::size_of::<SecurityAttributes>() == 24);
        assert!(std::mem::size_of::<StartupInfoW>() == 104);
        assert!(std::mem::size_of::<StartupInfoExW>() == 112);
        assert!(std::mem::size_of::<ProcessInformation>() == 24);
    };

    const HANDLE_FLAG_INHERIT: u32 = 1;
    const PIPE_NOWAIT: u32 = 1;
    const STARTF_USESTDHANDLES: u32 = 0x100;
    const CREATE_SUSPENDED: u32 = 0x4;
    const EXTENDED_STARTUPINFO_PRESENT: u32 = 0x0008_0000;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const PROC_THREAD_ATTRIBUTE_HANDLE_LIST: usize = 0x0002_0002;
    const PAGE_READWRITE: u32 = 0x04;
    const FILE_MAP_WRITE: u32 = 0x02;
    pub const FILE_MAP_READ: u32 = 0x04;
    const ERROR_BROKEN_PIPE: i32 = 109;
    const ERROR_NO_DATA: i32 = 232;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreatePipe(
            read: *mut Handle,
            write: *mut Handle,
            sa: *const SecurityAttributes,
            size: u32,
        ) -> Bool;
        fn SetHandleInformation(h: Handle, mask: u32, flags: u32) -> Bool;
        fn SetNamedPipeHandleState(
            h: Handle,
            mode: *const u32,
            max: *const u32,
            timeout: *const u32,
        ) -> Bool;
        fn PeekNamedPipe(
            h: Handle,
            buf: *mut c_void,
            size: u32,
            read: *mut u32,
            avail: *mut u32,
            left: *mut u32,
        ) -> Bool;
        fn InitializeProcThreadAttributeList(
            list: *mut c_void,
            count: u32,
            flags: u32,
            size: *mut usize,
        ) -> Bool;
        fn UpdateProcThreadAttribute(
            list: *mut c_void,
            flags: u32,
            attribute: usize,
            value: *const c_void,
            size: usize,
            previous: *mut c_void,
            returned: *const usize,
        ) -> Bool;
        fn DeleteProcThreadAttributeList(list: *mut c_void);
        fn CreateProcessW(
            app: *const u16,
            cmdline: *mut u16,
            pa: *const c_void,
            ta: *const c_void,
            inherit: Bool,
            flags: u32,
            env: *const c_void,
            dir: *const u16,
            si: *const StartupInfoExW,
            pi: *mut ProcessInformation,
        ) -> Bool;
        fn ResumeThread(thread: Handle) -> u32;
        pub fn TerminateProcess(process: Handle, code: u32) -> Bool;
        pub fn WaitForSingleObject(h: Handle, ms: u32) -> u32;
        fn CreateFileMappingW(
            file: Handle,
            sa: *const c_void,
            protect: u32,
            high: u32,
            low: u32,
            name: *const u16,
        ) -> Handle;
        pub fn MapViewOfFile(
            mapping: Handle,
            access: u32,
            high: u32,
            low: u32,
            len: usize,
        ) -> *mut c_void;
        pub fn UnmapViewOfFile(base: *const c_void) -> Bool;
        fn DuplicateHandle(
            src_process: Handle,
            src: Handle,
            dst_process: Handle,
            dst: *mut Handle,
            access: u32,
            inherit: Bool,
            options: u32,
        ) -> Bool;
        fn GetCurrentProcess() -> Handle;
    }

    fn last(what: &str) -> String {
        format!("{what}: {}", std::io::Error::last_os_error())
    }

    fn owned(h: Handle) -> OwnedHandle {
        unsafe { OwnedHandle::from_raw_handle(h) }
    }

    /// An anonymous pipe: (read end, write end), both inheritable.
    fn pipe() -> Result<(OwnedHandle, OwnedHandle), String> {
        let sa = SecurityAttributes {
            n_length: std::mem::size_of::<SecurityAttributes>() as u32,
            security_descriptor: std::ptr::null_mut(),
            inherit_handle: 1,
        };
        let (mut r, mut w) = (std::ptr::null_mut(), std::ptr::null_mut());
        if unsafe { CreatePipe(&mut r, &mut w, &sa, 1 << 16) } == 0 {
            return Err(last("CreatePipe"));
        }
        Ok((owned(r), owned(w)))
    }

    fn keep_here(h: &OwnedHandle) -> Result<(), String> {
        if unsafe { SetHandleInformation(h.as_raw_handle(), HANDLE_FLAG_INHERIT, 0) } == 0 {
            return Err(last("SetHandleInformation"));
        }
        Ok(())
    }

    /// The server's end of the helper's stdin: written without waiting (`PIPE_NOWAIT`).
    struct Tx(std::fs::File);
    /// The server's end of the helper's stdout: read only as far as `PeekNamedPipe` says.
    struct Rx(std::fs::File);

    impl Ready for Tx {
        fn read_ready(&mut self, _: &mut [u8]) -> std::io::Result<Option<usize>> {
            Err(std::io::ErrorKind::Unsupported.into())
        }

        fn write_ready(&mut self, buf: &[u8]) -> std::io::Result<Option<usize>> {
            match self.0.write(buf) {
                Ok(0) => Ok(None),
                Ok(n) => Ok(Some(n)),
                Err(e) if e.raw_os_error() == Some(ERROR_NO_DATA) => Ok(Some(0)),
                Err(e) => Err(e),
            }
        }
    }

    impl Ready for Rx {
        fn read_ready(&mut self, buf: &mut [u8]) -> std::io::Result<Option<usize>> {
            let mut avail = 0u32;
            let ok = unsafe {
                PeekNamedPipe(
                    self.0.as_raw_handle(),
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut avail,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                let e = std::io::Error::last_os_error();
                return match e.raw_os_error() {
                    Some(ERROR_BROKEN_PIPE) => Ok(Some(0)),
                    _ => Err(e),
                };
            }
            if avail == 0 {
                return Ok(None);
            }
            let n = buf.len().min(avail as usize);
            self.0.read(&mut buf[..n]).map(Some)
        }

        fn write_ready(&mut self, _: &[u8]) -> std::io::Result<Option<usize>> {
            Err(std::io::ErrorKind::Unsupported.into())
        }
    }

    /// A job object that kills what is in it once its last handle, the server's, closes.
    fn job() -> Result<OwnedHandle, String> {
        use windows::Win32::System::JobObjects::{
            CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        let job = unsafe { CreateJobObjectW(None, windows::core::PCWSTR::null()) }
            .map_err(|e| format!("no job object: {e}"))?;
        let owned_job = owned(job.0);
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
        Ok(owned_job)
    }

    /// Forwards the helper's stderr to the log, a line at a time and bounded.
    fn forward(stderr: std::fs::File, pid: u32) {
        let _ = std::thread::Builder::new()
            .name("qsv-helper-log".into())
            .spawn(move || {
                use std::io::BufRead;
                let lines = std::io::BufReader::new(stderr.take(1 << 20)).lines();
                for line in lines.map_while(Result::ok).take(1000) {
                    let end = line.len().min(512);
                    let end = (0..=end)
                        .rev()
                        .find(|&i| line.is_char_boundary(i))
                        .unwrap_or(0);
                    log::warn!("qsv helper {pid}: {}", &line[..end]);
                }
            });
    }

    /// Starts this executable as a Quick Sync helper with `args` after the helper flag:
    /// suspended, with only its three pipe ends inherited, put in a kill-on-close job, then
    /// resumed. Between CreateProcessW and the job it is suspended and has run nothing; only
    /// the server dying in that window leaves it (suspended, never running) behind.
    pub fn spawn_helper(args: &[&str]) -> Result<(Proc, Pipes), NotStarted> {
        let exe =
            std::env::current_exe().map_err(|e| format!("no path to this executable: {e}"))?;
        let exe = exe.to_string_lossy().into_owned();
        if exe.contains('"') {
            return Err(format!("{exe}: a path with a quote").into());
        }
        if args
            .iter()
            .any(|a| a.is_empty() || a.contains(['"', ' ', '\t']))
        {
            return Err(format!("helper arguments that need quoting: {args:?}").into());
        }
        let job = job()?;
        let (in_r, in_w) = pipe()?;
        let (out_r, out_w) = pipe()?;
        let (err_r, err_w) = pipe()?;
        for h in [&in_w, &out_r, &err_r] {
            keep_here(h)?;
        }
        let mode = PIPE_NOWAIT;
        if unsafe {
            SetNamedPipeHandleState(
                in_w.as_raw_handle(),
                &mode,
                std::ptr::null(),
                std::ptr::null(),
            )
        } == 0
        {
            return Err(last("SetNamedPipeHandleState").into());
        }
        let inherit: [Handle; 3] = [
            in_r.as_raw_handle(),
            out_w.as_raw_handle(),
            err_w.as_raw_handle(),
        ];
        let mut size = 0usize;
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size) };
        let mut list = vec![0u64; size.div_ceil(8).max(1)];
        let attrs: *mut c_void = list.as_mut_ptr().cast();
        if unsafe { InitializeProcThreadAttributeList(attrs, 1, 0, &mut size) } == 0 {
            return Err(last("InitializeProcThreadAttributeList").into());
        }
        let updated = unsafe {
            UpdateProcThreadAttribute(
                attrs,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
                inherit.as_ptr().cast(),
                std::mem::size_of_val(&inherit),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        if updated == 0 {
            let e = last("UpdateProcThreadAttribute");
            unsafe { DeleteProcThreadAttributeList(attrs) };
            return Err(e.into());
        }
        let si = StartupInfoExW {
            info: StartupInfoW {
                cb: std::mem::size_of::<StartupInfoExW>() as u32,
                reserved: std::ptr::null_mut(),
                desktop: std::ptr::null_mut(),
                title: std::ptr::null_mut(),
                x: 0,
                y: 0,
                x_size: 0,
                y_size: 0,
                x_count_chars: 0,
                y_count_chars: 0,
                fill_attribute: 0,
                flags: STARTF_USESTDHANDLES,
                show_window: 0,
                cb_reserved2: 0,
                reserved2: std::ptr::null_mut(),
                std_input: in_r.as_raw_handle(),
                std_output: out_w.as_raw_handle(),
                std_error: err_w.as_raw_handle(),
            },
            attribute_list: attrs,
        };
        let app: Vec<u16> = exe.encode_utf16().chain([0]).collect();
        let mut line = format!("\"{exe}\" {} qsv", super::super::HELPER_FLAG);
        for a in args {
            line.push(' ');
            line.push_str(a);
        }
        let mut cmdline: Vec<u16> = line.encode_utf16().chain([0]).collect();
        let mut pi = ProcessInformation {
            process: std::ptr::null_mut(),
            thread: std::ptr::null_mut(),
            process_id: 0,
            thread_id: 0,
        };
        let created = unsafe {
            CreateProcessW(
                app.as_ptr(),
                cmdline.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                CREATE_SUSPENDED | EXTENDED_STARTUPINFO_PRESENT | CREATE_NO_WINDOW,
                std::ptr::null(),
                std::ptr::null(),
                &si,
                &mut pi,
            )
        };
        let created_err = (created == 0).then(|| last("CreateProcessW"));
        unsafe { DeleteProcThreadAttributeList(attrs) };
        drop(list);
        drop((in_r, out_w, err_w));
        if let Some(e) = created_err {
            return Err(e.into());
        }
        let process = owned(pi.process);
        let thread = owned(pi.thread);
        let proc = Arc::new(Proc {
            inner: Mutex::new(Inner {
                process,
                job,
                assigned: false,
                reaped: false,
            }),
            pid: pi.process_id,
        });
        let failed = |why: String| NotStarted {
            why,
            left: end_all(std::slice::from_ref(&proc), Duration::from_secs(2)),
        };
        let assigned = {
            use windows::Win32::Foundation::HANDLE;
            use windows::Win32::System::JobObjects::AssignProcessToJobObject;
            let mut i = proc.inner.lock().unwrap_or_else(|e| e.into_inner());
            let r = unsafe {
                AssignProcessToJobObject(
                    HANDLE(i.job.as_raw_handle()),
                    HANDLE(i.process.as_raw_handle()),
                )
            };
            i.assigned = r.is_ok();
            r
        };
        if let Err(e) = assigned {
            return Err(failed(format!(
                "couldn't put the helper in a job object: {e}"
            )));
        }
        if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
            return Err(failed(last("ResumeThread")));
        }
        let Ok(proc) = Arc::try_unwrap(proc) else {
            return Err("the helper's process is shared".to_string().into());
        };
        drop(thread);
        forward(std::fs::File::from(err_r), pi.process_id);
        let pipes = Pipes {
            tx: Box::new(Tx(std::fs::File::from(in_w))),
            rx: Box::new(Rx(std::fs::File::from(out_r))),
        };
        Ok((proc, pipes))
    }

    /// An unnamed pagefile-backed section the frames go through; the helper gets a
    /// read-only handle to it.
    pub struct Section {
        handle: OwnedHandle,
        view: *mut u8,
        len: usize,
    }

    unsafe impl Send for Section {}

    impl Section {
        pub fn new(len: usize) -> Result<Section, String> {
            let invalid = -1isize as Handle;
            let h = unsafe {
                CreateFileMappingW(
                    invalid,
                    std::ptr::null(),
                    PAGE_READWRITE,
                    (len as u64 >> 32) as u32,
                    len as u32,
                    std::ptr::null(),
                )
            };
            if h.is_null() {
                return Err(last("CreateFileMappingW"));
            }
            let handle = owned(h);
            let view = unsafe { MapViewOfFile(h, FILE_MAP_WRITE, 0, 0, len) };
            if view.is_null() {
                return Err(last("MapViewOfFile"));
            }
            Ok(Section {
                handle,
                view: view.cast(),
                len,
            })
        }

        pub fn slice(&mut self) -> &mut [u8] {
            unsafe { std::slice::from_raw_parts_mut(self.view, self.len) }
        }

        /// The section's handle in `p`, read-only.
        pub fn share(&self, p: &Proc) -> Result<u64, String> {
            let i = p.inner.lock().unwrap_or_else(|e| e.into_inner());
            let mut out: Handle = std::ptr::null_mut();
            let ok = unsafe {
                DuplicateHandle(
                    GetCurrentProcess(),
                    self.handle.as_raw_handle(),
                    i.process.as_raw_handle(),
                    &mut out,
                    FILE_MAP_READ,
                    0,
                    0,
                )
            };
            if ok == 0 {
                return Err(last("DuplicateHandle"));
            }
            Ok(out as usize as u64)
        }
    }

    impl Drop for Section {
        fn drop(&mut self) {
            unsafe { UnmapViewOfFile(self.view.cast()) };
        }
    }
}

/// The helper's side of the section: mapped read-only from the handle the server gave it.
#[cfg(windows)]
pub struct Mapped {
    view: *const u8,
    len: usize,
}

#[cfg(windows)]
impl Mapped {
    pub fn open(handle: u64, len: usize) -> Result<Mapped, String> {
        let h = handle as usize as *mut std::ffi::c_void;
        let view = unsafe { win::MapViewOfFile(h, win::FILE_MAP_READ, 0, 0, len) };
        if view.is_null() {
            return Err(format!(
                "MapViewOfFile: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Mapped {
            view: view.cast(),
            len,
        })
    }

    pub fn slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.view, self.len) }
    }
}

#[cfg(windows)]
impl Drop for Mapped {
    fn drop(&mut self) {
        unsafe { win::UnmapViewOfFile(self.view.cast()) };
    }
}
