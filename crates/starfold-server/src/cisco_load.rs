//! Loads Cisco's OpenH264 binary. Compiled into the `starfold` binary (`main.rs`), which calls
//! [`load_alone`] first thing, before starfold starts its logger, its panic hook or any thread,
//! and into the library's own tests; the library itself never contains it.
//!
//! The file is read once into memory (a regular file of at most 16 MiB, opened without
//! blocking), and its sha256 must be the pinned Cisco build. Those bytes go into a memfd sealed
//! against writing, growing and shrinking, which is read back and hashed again, and the library
//! is loaded from `/proc/self/fd/N`. Before any symbol lookup, the object glibc returned must be
//! that memfd's own mapping (device and inode from `fstat` on the memfd), and so must each entry
//! point. The memfd stays open for the life of the process; nothing is written to disk.

#![allow(
    dead_code,
    reason = "the binary and the library's tests each use a different part of it"
)]

use openh264_sys2 as sys;
use sha2::Digest;
use std::ffi::{c_int, c_void};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::Path;

/// Larger than any pinned build (the biggest is under 2 MiB).
pub const MAX_BYTES: u64 = 16 << 20;

/// Why nothing is loaded while another thread runs.
pub const ONE_THREAD: &str =
    "not loading OpenH264 while the process runs more than one thread: starfold loads it first thing";

pub type Create = unsafe extern "C" fn(*mut *mut sys::ISVCEncoder) -> c_int;
pub type Destroy = unsafe extern "C" fn(*mut sys::ISVCEncoder);
pub type Version = unsafe extern "C" fn() -> sys::OpenH264Version;

/// Cisco's library, loaded from its checked bytes: its entry points, what it is, the loader's
/// handle and the memfd it was loaded from.
pub struct Opened {
    pub create: Create,
    pub destroy: Destroy,
    pub version: Version,
    /// Cisco's file name, from the pin the bytes matched.
    pub file: &'static str,
    pub sha256: String,
    /// The name it was loaded under, `/proc/self/fd/N`.
    pub name: String,
    /// The threads the process ran right before the load, as [`load_alone`] counted them; `None`
    /// when nothing counted them (the library's tests).
    pub threads: Option<usize>,
    pub library: libloading::Library,
    pub memfd: OwnedFd,
}

/// The number of threads this process runs: the entries of `/proc/self/task`. Any error opening
/// or reading the directory is an error, never a count.
pub fn threads() -> std::io::Result<usize> {
    count(std::fs::read_dir("/proc/self/task")?)
}

fn count<T>(entries: impl Iterator<Item = std::io::Result<T>>) -> std::io::Result<usize> {
    let mut n = 0;
    for e in entries {
        e?;
        n += 1;
    }
    Ok(n)
}

/// Whether a load may go ahead, given the thread count: exactly one, or it refuses.
fn alone(path: &Path, threads: std::io::Result<usize>) -> Result<(), String> {
    let shown = path.display();
    match threads {
        Ok(1) => Ok(()),
        Ok(n) => Err(format!(
            "OpenH264 library {shown}: {ONE_THREAD} ({n} threads)"
        )),
        Err(e) => Err(format!(
            "OpenH264 library {shown}: not loading OpenH264 without a thread count: \
             /proc/self/task: {e}"
        )),
    }
}

/// Loads the pinned build at `path`, but only while this process runs exactly one thread.
pub fn load_alone(
    path: &Path,
    pins: &[(&'static str, &'static str)],
    expected: &str,
) -> Result<Opened, String> {
    alone(path, threads())?;
    let mut opened = open_checked(path, pins, expected, || {})?;
    opened.threads = Some(1);
    Ok(opened)
}

/// Reads `path` once, checks its sha256 against `pins`, and loads those bytes from a sealed
/// memfd; `after_check` runs once the bytes have passed, before anything is loaded.
pub fn open_checked(
    path: &Path,
    pins: &[(&'static str, &'static str)],
    expected: &str,
    after_check: impl FnOnce(),
) -> Result<Opened, String> {
    let shown = path.display();
    let checked = Checked::read(path, pins, expected)?;
    after_check();
    let fd = sealed_memfd(&checked.bytes, &checked.sha256)
        .map_err(|e| format!("OpenH264 library {shown}: its sealed copy: {e}"))?;
    drop(checked.bytes);
    let opened = load_memfd(fd, checked.file, checked.sha256)
        .map_err(|e| format!("OpenH264 library {shown}: {e}"))?;
    let v = unsafe { (opened.version)() };
    if (v.uMajor, v.uMinor) != (2, 6) {
        return Err(format!(
            "OpenH264 library {shown}: reports version {}.{}, not 2.6",
            v.uMajor, v.uMinor
        ));
    }
    Ok(opened)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A candidate's bytes, read once, whose sha256 is the pinned build `file`.
pub struct Checked {
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub file: &'static str,
}

impl Checked {
    pub fn read(
        path: &Path,
        pins: &[(&'static str, &'static str)],
        expected: &str,
    ) -> Result<Self, String> {
        let shown = path.display();
        let fail = |e: std::io::Error| format!("OpenH264 library {shown}: {e}");
        // Without blocking: a FIFO would otherwise hang here.
        let f = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .map_err(fail)?;
        let meta = f.metadata().map_err(fail)?;
        if !meta.is_file() {
            return Err(format!("OpenH264 library {shown}: not a regular file"));
        }
        let too_large = || {
            format!(
                "OpenH264 library {shown}: larger than {} MiB, not a Cisco build",
                MAX_BYTES >> 20
            )
        };
        if meta.len() > MAX_BYTES {
            return Err(too_large());
        }
        let mut bytes = Vec::new();
        f.take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(fail)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(too_large());
        }
        let sha256 = sha256_hex(&bytes);
        let Some(&(_, file)) = pins.iter().find(|(h, _)| *h == sha256) else {
            return Err(format!(
                "OpenH264 library {shown}: sha256 {sha256} is not Cisco's {expected}, the build \
                 starfold pins for this platform"
            ));
        };
        Ok(Checked {
            bytes,
            sha256,
            file,
        })
    }
}

const SEALS: c_int =
    libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;

/// `bytes` in a new memfd, sealed so that nothing can write, grow or shrink it any more, then
/// read back and hashed again.
pub fn sealed_memfd(bytes: &[u8], sha256: &str) -> std::io::Result<OwnedFd> {
    let name = c"starfold-openh264";
    let flags = libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING;
    // MFD_EXEC keeps it executable under vm.memfd_noexec = 1; kernels before 6.3 don't know
    // the flag. Under vm.memfd_noexec = 2 the kernel refuses an executable memfd, and so
    // starfold can't load the library at all.
    let mut fd = unsafe { libc::memfd_create(name.as_ptr(), flags | libc::MFD_EXEC) };
    if fd < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL) {
        fd = unsafe { libc::memfd_create(name.as_ptr(), flags) };
    }
    if fd < 0 {
        let e = std::io::Error::last_os_error();
        return Err(match e.raw_os_error() {
            Some(libc::EACCES) => std::io::Error::other(format!(
                "the kernel refuses an executable memfd ({e}), as vm.memfd_noexec = 2 does"
            )),
            _ => e,
        });
    }
    let mut f = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    f.write_all(bytes)?;
    if unsafe { libc::fcntl(f.as_raw_fd(), libc::F_ADD_SEALS, SEALS) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let sealed = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_GET_SEALS) };
    if sealed < 0 || sealed & SEALS != SEALS {
        return Err(std::io::Error::other("the memfd did not take its seals"));
    }
    let mut again = vec![0u8; bytes.len()];
    f.read_exact_at(&mut again, 0)?;
    if f.metadata()?.len() != bytes.len() as u64 || sha256_hex(&again) != sha256 {
        return Err(std::io::Error::other(
            "the sealed copy is not the bytes that were checked",
        ));
    }
    Ok(f.into())
}

/// A file's device (major, minor) and inode.
pub type FileId = (u32, u32, u64);

/// The device and inode of the file `fd` refers to, from `fstat` on the descriptor itself.
pub fn file_id(fd: &OwnedFd) -> std::io::Result<FileId> {
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd.as_raw_fd(), st.as_mut_ptr()) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let st = unsafe { st.assume_init() };
    Ok((libc::major(st.st_dev), libc::minor(st.st_dev), st.st_ino))
}

/// The file mapped at `addr` in this process, from `/proc/self/maps`.
pub fn mapping_of(addr: usize) -> std::io::Result<Option<FileId>> {
    Ok(mapping_in(&std::fs::read("/proc/self/maps")?, addr))
}

/// The file mapped at `addr`, from the text of a maps file. Only the numeric fields are read:
/// a path may hold any bytes.
pub fn mapping_in(maps: &[u8], addr: usize) -> Option<FileId> {
    maps.split(|&b| b == b'\n').find_map(|l| {
        let mut w = l
            .split(|&b| b == b' ')
            .filter(|f| !f.is_empty())
            .map(std::str::from_utf8);
        let (lo, hi) = w.next()?.ok()?.split_once('-')?;
        let lo = usize::from_str_radix(lo, 16).ok()?;
        let hi = usize::from_str_radix(hi, 16).ok()?;
        if !(lo..hi).contains(&addr) {
            return None;
        }
        let (major, minor) = w.nth(2)?.ok()?.split_once(':')?;
        Some((
            u32::from_str_radix(major, 16).ok()?,
            u32::from_str_radix(minor, 16).ok()?,
            w.next()?.ok()?.parse().ok()?,
        ))
    })
}

/// The public head of glibc's `struct link_map` (`<link.h>`).
#[repr(C)]
struct LinkMap {
    l_addr: usize,
    _l_name: *const std::ffi::c_char,
    l_ld: *const c_void,
}

/// The object glibc handed back for `lib`, by its load address and its dynamic section.
fn linked(lib: libloading::Library) -> (libloading::Library, Result<[usize; 2], String>) {
    let raw = libloading::os::unix::Library::from(lib).into_raw();
    let mut map: *mut LinkMap = std::ptr::null_mut();
    let r = unsafe { libc::dlinfo(raw, libc::RTLD_DI_LINKMAP, (&raw mut map).cast()) };
    let lib = libloading::Library::from(unsafe { libloading::os::unix::Library::from_raw(raw) });
    let at = if r != 0 || map.is_null() {
        Err("dlinfo found no link map".to_string())
    } else {
        let m = unsafe { &*map };
        Ok([m.l_addr, m.l_ld as usize])
    };
    (lib, at)
}

/// Loads the library in the sealed memfd `fd` from `/proc/self/fd/N`. Before it looks up any
/// symbol, the object glibc returned must be the memfd's own mapping; each entry point must
/// then lie in that mapping too.
fn load_memfd(fd: OwnedFd, file: &'static str, sha256: String) -> Result<Opened, String> {
    let name = format!("/proc/self/fd/{}", fd.as_raw_fd());
    load_as(name, fd, file, sha256)
}

/// Loads `name`, which must hand back the library in `fd`'s mapping: the link map is checked
/// against `fd` before any symbol lookup.
fn load_as(
    name: String,
    fd: OwnedFd,
    file: &'static str,
    sha256: String,
) -> Result<Opened, String> {
    let ours = file_id(&fd).map_err(|e| format!("{name}: {e}"))?;
    let in_ours = |addr: usize| -> Result<bool, String> {
        let found = mapping_of(addr).map_err(|e| format!("/proc/self/maps: {e}"))?;
        Ok(found == Some(ours))
    };
    #[allow(
        clippy::disallowed_methods,
        reason = "Linux only: the loader's path is /proc/self/fd/N of a sealed memfd, no search"
    )]
    let lib = unsafe { libloading::Library::new(&name) }.map_err(|e| format!("{name}: {e}"))?;
    let (lib, at) = linked(lib);
    for addr in at.map_err(|e| format!("{name}: {e}"))? {
        if !in_ours(addr)? {
            return Err(format!(
                "{name}: the object loaded under that name is not the sealed copy (its link map \
                 lies outside it); refused before looking up any symbol"
            ));
        }
    }
    let entry = |sym: &str| -> Result<*mut c_void, String> {
        let p = *unsafe { lib.get::<*mut c_void>(sym.as_bytes()) }
            .map_err(|e| format!("{sym}: {e}"))?;
        if p.is_null() {
            return Err(format!("{sym}: null"));
        }
        if !in_ours(p as usize)? {
            return Err(format!("{name}: {sym} is not in the sealed copy"));
        }
        Ok(p)
    };
    let (create, destroy, version) = unsafe {
        (
            std::mem::transmute::<*mut c_void, Create>(entry("WelsCreateSVCEncoder")?),
            std::mem::transmute::<*mut c_void, Destroy>(entry("WelsDestroySVCEncoder")?),
            std::mem::transmute::<*mut c_void, Version>(entry("WelsGetCodecVersion")?),
        )
    };
    Ok(Opened {
        create,
        destroy,
        version,
        file,
        sha256,
        name,
        threads: None,
        library: lib,
        memfd: fd,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const LIB_ENV: &str = "STARFOLD_OPENH264_LIB";

    fn scratch(tag: &str) -> PathBuf {
        let mut r = [0u8; 12];
        getrandom::fill(&mut r).unwrap();
        let r: String = r.iter().map(|b| format!("{b:02x}")).collect();
        let dir = std::env::temp_dir().join(format!("sf-cisco-{tag}-{r}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn pin_of(bytes: &[u8]) -> [(&'static str, &'static str); 1] {
        [(sha256_hex(bytes).leak() as &str, "lib.so")]
    }

    /// This target's pin and Cisco's file name for it.
    fn here() -> (Vec<(&'static str, &'static str)>, &'static str) {
        let f = if cfg!(target_arch = "x86_64") {
            "libopenh264-2.6.0-linux64.8.so"
        } else {
            "libopenh264-2.6.0-linux-arm64.8.so"
        };
        let pins = [
            (
                "2f0cde7c6a6abcf5cae76942894ea42897fa677bce4ed6c91a24dd1b041d5f04",
                "libopenh264-2.6.0-linux64.8.so",
            ),
            (
                "12e7b33623667cdab0e575170c147b1b36eadb77d0d2aa7ceb5afd3e58902140",
                "libopenh264-2.6.0-linux-arm64.8.so",
            ),
        ];
        (pins.into_iter().filter(|p| p.1 == f).collect(), f)
    }

    /// Cisco's real file, as CI fetches it.
    fn real_lib() -> Option<PathBuf> {
        let p = std::env::var_os(LIB_ENV).filter(|p| !p.is_empty());
        if p.is_none() {
            eprintln!("skipped: {LIB_ENV} is not set");
        }
        p.map(PathBuf::from)
    }

    /// With another thread running, nothing is read or loaded: the refusal comes before the
    /// file is even opened (it doesn't exist here).
    #[test]
    fn a_load_while_another_thread_runs_is_refused() {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let other = std::thread::spawn(move || {
            let _ = rx.recv();
        });
        assert!(threads().unwrap() > 1);
        let (pins, f) = here();
        let e = load_alone(Path::new("/nonexistent/lib.so"), &pins, f)
            .err()
            .unwrap();
        assert!(e.contains(ONE_THREAD), "{e}");
        tx.send(()).unwrap();
        other.join().unwrap();
    }

    /// An error reading `/proc/self/task`, before or among its entries, is no count: the load is
    /// refused. Only a count of exactly one lets it go ahead.
    #[test]
    fn a_thread_count_that_cannot_be_read_refuses_the_load() {
        let denied = || std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(count([Err::<(), _>(denied())].into_iter()).is_err());
        assert!(count([Ok(()), Err(denied())].into_iter()).is_err());
        assert_eq!(count([Ok(())].into_iter()).unwrap(), 1);
        let p = Path::new("/x/lib.so");
        assert!(alone(p, Err(denied()))
            .unwrap_err()
            .contains("without a thread count"));
        assert!(alone(p, Ok(0)).unwrap_err().contains(ONE_THREAD));
        assert!(alone(p, Ok(2)).unwrap_err().contains(ONE_THREAD));
        assert!(alone(p, Ok(1)).is_ok());
    }

    #[test]
    fn only_the_pinned_build_is_read() {
        let dir = scratch("refuse");
        let (pins, f) = here();
        let missing = dir.join("missing.so");
        let e = Checked::read(&missing, &pins, f).err().unwrap();
        assert!(e.contains("missing.so"), "{e}");
        let other = dir.join("libopenh264.so");
        std::fs::write(&other, b"not cisco").unwrap();
        let e = Checked::read(&other, &pins, f).err().unwrap();
        assert!(e.contains(&sha256_hex(b"not cisco")), "{e}");
        assert!(e.contains("not Cisco's"), "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_large_or_special_file_is_refused_before_reading() {
        let dir = scratch("large");
        let (pins, f) = here();
        let big = dir.join("big.so");
        File::create(&big).unwrap().set_len(MAX_BYTES + 1).unwrap();
        let e = Checked::read(&big, &pins, f).err().unwrap();
        assert!(e.contains("larger than"), "{e}");
        let e = Checked::read(Path::new("/dev/zero"), &pins, f)
            .err()
            .unwrap();
        assert!(e.contains("not a regular file"), "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A FIFO with no writer would block an ordinary open; it is refused at once.
    #[test]
    fn a_fifo_is_refused_without_blocking() {
        let dir = scratch("fifo");
        let fifo = dir.join("lib.so");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let (tx, rx) = std::sync::mpsc::channel();
        let p = fifo.clone();
        std::thread::spawn(move || {
            let (pins, f) = here();
            let _ = tx.send(Checked::read(&p, &pins, f).err());
        });
        let e = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("reading a FIFO blocked")
            .expect("a FIFO was accepted");
        assert!(e.contains("not a regular file"), "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The memfd holds the checked bytes, not what the given file holds later, and nothing can
    /// change it: not a write, not a resize, not a write through a name reopened for writing.
    #[test]
    fn the_sealed_copy_is_the_checked_bytes_and_cannot_change() {
        let dir = scratch("seal");
        let given = dir.join("lib.so");
        let bytes = b"the checked bytes".to_vec();
        std::fs::write(&given, &bytes).unwrap();
        let checked = Checked::read(&given, &pin_of(&bytes), "lib.so").unwrap();
        std::fs::write(&given, b"swapped in place").unwrap();
        let fd = sealed_memfd(&checked.bytes, &checked.sha256).unwrap();
        let name = format!("/proc/self/fd/{}", fd.as_raw_fd());
        assert_eq!(std::fs::read(&name).unwrap(), bytes);
        let mut f = File::from(fd);
        assert!(f.write_all(b"x").is_err());
        assert!(f.set_len(1).is_err());
        assert!(f.set_len(1 << 20).is_err());
        let reopened = std::fs::OpenOptions::new().write(true).open(&name);
        assert!(reopened.and_then(|mut w| w.write_all(b"x")).is_err());
        assert_eq!(std::fs::read(&name).unwrap(), bytes);
        let e = sealed_memfd(&bytes, &sha256_hex(b"other")).err().unwrap();
        assert!(e.to_string().contains("not the bytes"), "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A mapped file whose path is not UTF-8 leaves the maps check working.
    #[test]
    fn a_path_that_is_not_utf8_does_not_stop_the_maps_check() {
        use std::os::unix::ffi::OsStrExt;
        let dir = scratch("maps");
        let p = dir.join(std::ffi::OsStr::from_bytes(b"map-\xff.bin"));
        std::fs::write(&p, [0u8; 4096]).unwrap();
        let f = File::open(&p).unwrap();
        let m = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                f.as_raw_fd(),
                0,
            )
        };
        assert_ne!(m, libc::MAP_FAILED);
        assert!(std::str::from_utf8(&std::fs::read("/proc/self/maps").unwrap()).is_err());
        let want = file_id(&OwnedFd::from(f)).unwrap();
        assert_eq!(mapping_of(m as usize).unwrap(), Some(want));
        unsafe { libc::munmap(m, 4096) };
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A name that hands back an object other than the sealed copy, here the C library this
    /// process already runs, is refused by the link-map check, before any symbol is looked up.
    #[test]
    fn a_handle_that_is_not_the_sealed_copy_is_refused_before_any_symbol_lookup() {
        let bytes = b"never loaded";
        let fd = sealed_memfd(bytes, &sha256_hex(bytes)).unwrap();
        let e = load_as("libc.so.6".into(), fd, "lib.so", sha256_hex(bytes))
            .err()
            .unwrap();
        assert!(e.contains("refused before looking up any symbol"), "{e}");
    }

    /// With Cisco's real file: after the check, the given file is replaced, in place and then
    /// by a rename over it, by bytes that are no library at all, and what the loader loads is
    /// still Cisco's OpenH264 2.6, from the memfd, not from the given file.
    #[test]
    fn ciscos_bytes_load_after_the_given_file_is_replaced() {
        let Some(real) = real_lib() else { return };
        let (pins, f) = here();
        let dir = scratch("swap");
        let given = dir.join(f);
        std::fs::copy(&real, &given).unwrap();
        let other = dir.join("other");
        let opened = open_checked(&given, &pins, f, || {
            std::fs::write(&given, b"not a library").unwrap();
            std::fs::write(&other, b"not a library either").unwrap();
            std::fs::rename(&other, &given).unwrap();
        })
        .unwrap();
        assert_eq!(std::fs::read(&given).unwrap(), b"not a library either");
        let v = unsafe { (opened.version)() };
        assert_eq!((v.uMajor, v.uMinor), (2, 6));
        assert_eq!(
            mapping_of(opened.create as usize).unwrap(),
            Some(file_id(&opened.memfd).unwrap())
        );
        let maps = std::fs::read("/proc/self/maps").unwrap();
        let dir_bytes = dir.as_os_str().as_encoded_bytes();
        assert!(!maps.windows(dir_bytes.len()).any(|w| w == dir_bytes));
        drop(opened);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
