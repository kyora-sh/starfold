//! Cisco's OpenH264 binary, loaded at run time (the `openh264-cisco` feature). starfold neither
//! contains nor downloads it, and loads it only on 64-bit Linux, x86-64 and arm64, with glibc;
//! on any other target the feature builds, and OpenH264 is unavailable with [`NOT_AVAILABLE`].
//!
//! The file from `--openh264-lib` or `$STARFOLD_OPENH264_LIB` is read once into memory (a
//! regular file of at most 16 MiB, opened without blocking), and its sha256 must be the pinned
//! Cisco build. Those bytes go into a memfd sealed against writing, growing and shrinking,
//! which is read back and hashed again, and the library is loaded from `/proc/self/fd/N`, a
//! name no loaded object already has. Before any symbol lookup, the object glibc returned must
//! be that memfd's own mapping, and so must each entry point.
//! The memfd stays open for the life of the process; nothing is written to disk. starfold
//! calls the library only through the entry points held here, never through openh264-sys2's
//! `DynamicAPI`, so a build in which a dependent enables OpenH264's source build still never
//! calls that.

use super::openh264::Api;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Cisco's OpenH264 2.6.0 builds starfold loads: (sha256, Cisco's file name). Each is also in
/// openh264-sys2's own list of the builds its bindings fit.
pub const PINS: &[(&str, &str)] = &[
    (
        "2f0cde7c6a6abcf5cae76942894ea42897fa677bce4ed6c91a24dd1b041d5f04",
        "libopenh264-2.6.0-linux64.8.so",
    ),
    (
        "12e7b33623667cdab0e575170c147b1b36eadb77d0d2aa7ceb5afd3e58902140",
        "libopenh264-2.6.0-linux-arm64.8.so",
    ),
];

/// The library's path when `--openh264-lib` isn't given.
pub const LIB_ENV: &str = "STARFOLD_OPENH264_LIB";

/// The text Cisco's binary licence asks software using it to display.
pub const ATTRIBUTION: &str = "OpenH264 Video Codec provided by Cisco Systems, Inc.";

/// Why a build of the feature for any other target has no OpenH264.
pub const NOT_AVAILABLE: &str = "OpenH264 (Cisco) is not available on this platform yet: \
     starfold loads Cisco's binary only on 64-bit Linux, x86-64 and arm64, with glibc";

/// Cisco's file name for a target architecture starfold loads it on.
fn file_for(arch: &str) -> Option<&'static str> {
    match arch {
        "x86_64" => Some("libopenh264-2.6.0-linux64.8.so"),
        "aarch64" => Some("libopenh264-2.6.0-linux-arm64.8.so"),
        _ => None,
    }
}

/// Cisco's file name for this target, where starfold loads it.
pub fn platform_file() -> Option<&'static str> {
    file_for(std::env::consts::ARCH).filter(|_| cfg!(openh264_cisco_loadable))
}

struct Loaded {
    api: Api,
    what: String,
    /// The memfd and the loader's handle, held for the life of the process.
    #[cfg(openh264_cisco_loadable)]
    _held: Option<linux::Held>,
}

static LIBRARY: OnceLock<Result<Loaded, String>> = OnceLock::new();

/// Loads OpenH264 from `path`, else from `$STARFOLD_OPENH264_LIB`, for the life of the
/// process, and says what was loaded; later calls return the first call's outcome.
pub fn load(path: Option<&Path>) -> Result<&'static str, String> {
    LIBRARY
        .get_or_init(|| open(path, || {}))
        .as_ref()
        .map(|l| l.what.as_str())
        .map_err(Clone::clone)
}

/// The loaded library's entry points (loading it from `$STARFOLD_OPENH264_LIB` if nothing
/// has yet).
pub(crate) fn api() -> Result<Api, String> {
    LIBRARY
        .get_or_init(|| open(None, || {}))
        .as_ref()
        .map(|l| l.api)
        .map_err(Clone::clone)
}

fn no_library() -> String {
    match platform_file() {
        Some(f) => format!(
            "no OpenH264 library: get Cisco's {f} from https://www.openh264.org and pass its \
             path with --openh264-lib or {LIB_ENV}"
        ),
        None => NOT_AVAILABLE.into(),
    }
}

/// Loads the library at `path`, else at `$STARFOLD_OPENH264_LIB`; `after_check` runs once its
/// bytes have passed the pin, before anything is loaded.
fn open(path: Option<&Path>, after_check: impl FnOnce()) -> Result<Loaded, String> {
    let env = std::env::var_os(LIB_ENV).filter(|p| !p.is_empty());
    let Some(path) = path.map(Path::to_path_buf).or(env.map(PathBuf::from)) else {
        // Tests encode with the dev-dependency's OpenH264 unless $STARFOLD_OPENH264_LIB
        // names Cisco's file; a starfold binary has no such fallback.
        #[cfg(test)]
        return Ok(Loaded {
            api: super::openh264::SOURCE,
            what: "OpenH264 compiled from source (tests only)".into(),
            #[cfg(openh264_cisco_loadable)]
            _held: None,
        });
        #[cfg(not(test))]
        return Err(no_library());
    };
    #[cfg(openh264_cisco_loadable)]
    return linux::open(&path, after_check);
    #[cfg(not(openh264_cisco_loadable))]
    {
        let _ = (path, after_check);
        Err(NOT_AVAILABLE.into())
    }
}

#[cfg(openh264_cisco_loadable)]
mod linux {
    use super::{no_library, platform_file, Api, Loaded, ATTRIBUTION, PINS};
    use sha2::Digest;
    use std::ffi::{c_int, c_void, CStr};
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
    use std::path::Path;

    /// Larger than any pinned build (the biggest is under 2 MiB).
    pub(super) const MAX_BYTES: u64 = 16 << 20;

    /// What keeps the library loaded: the loader's handle and the memfd it was loaded from.
    pub(super) struct Held {
        _lib: libloading::Library,
        _fd: OwnedFd,
        /// The name it was loaded under, `/proc/self/fd/N`.
        pub(super) name: String,
    }

    pub(super) fn open(path: &Path, after_check: impl FnOnce()) -> Result<Loaded, String> {
        let shown = path.display();
        let checked = Checked::read(path, &platform_pins())?;
        after_check();
        let fd = sealed_memfd(&checked.bytes, &checked.sha256)
            .map_err(|e| format!("OpenH264 library {shown}: its sealed copy: {e}"))?;
        drop(checked.bytes);
        let (api, held) = load_memfd(fd).map_err(|e| format!("OpenH264 library {shown}: {e}"))?;
        let v = unsafe { (api.version)() };
        if (v.uMajor, v.uMinor) != (2, 6) {
            return Err(format!(
                "OpenH264 library {shown}: reports version {}.{}, not 2.6",
                v.uMajor, v.uMinor
            ));
        }
        let what = format!(
            "{ATTRIBUTION} ({}, sha256 {})",
            checked.file, checked.sha256
        );
        log::info!("{what}, from {shown}, loaded as {}", held.name);
        Ok(Loaded {
            api,
            what,
            _held: Some(held),
        })
    }

    pub(super) fn platform_pins() -> Vec<(&'static str, &'static str)> {
        PINS.iter()
            .copied()
            .filter(|(_, name)| Some(*name) == platform_file())
            .collect()
    }

    pub(super) fn sha256_hex(bytes: &[u8]) -> String {
        sha2::Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// A candidate's bytes, read once, whose sha256 is the pinned build `file`.
    pub(super) struct Checked {
        pub(super) bytes: Vec<u8>,
        pub(super) sha256: String,
        pub(super) file: &'static str,
    }

    impl Checked {
        pub(super) fn read(
            path: &Path,
            pins: &[(&'static str, &'static str)],
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
                return Err(match platform_file() {
                    Some(f) => format!(
                        "OpenH264 library {shown}: sha256 {sha256} is not Cisco's {f}, the build \
                         starfold pins for this platform"
                    ),
                    None => no_library(),
                });
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

    /// `bytes` in a new memfd, sealed so that nothing can write, grow or shrink it any more,
    /// then read back and hashed again.
    pub(super) fn sealed_memfd(bytes: &[u8], sha256: &str) -> std::io::Result<OwnedFd> {
        let name = c"starfold-openh264";
        let flags = libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING;
        // MFD_EXEC keeps it executable under vm.memfd_noexec = 1; kernels before 6.3 don't
        // know the flag. Under vm.memfd_noexec = 2 the kernel refuses an executable memfd,
        // and so starfold can't load the library at all.
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

    pub(super) fn proc_name(fd: c_int) -> String {
        format!("/proc/self/fd/{fd}")
    }

    /// N, for a name that is exactly `/proc/self/fd/N`.
    pub(super) fn fd_of_name(name: &str) -> Option<c_int> {
        let digits = name.strip_prefix("/proc/self/fd/")?;
        if !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let n: c_int = digits.parse().ok()?;
        (n.to_string() == digits).then_some(n)
    }

    /// N for every loaded object named `/proc/self/fd/N`.
    pub(super) fn loaded_fd_names() -> Vec<c_int> {
        unsafe extern "C" fn each(
            info: *mut libc::dl_phdr_info,
            _: libc::size_t,
            names: *mut c_void,
        ) -> c_int {
            let (names, name) = unsafe { (&mut *names.cast::<Vec<c_int>>(), (*info).dlpi_name) };
            if !name.is_null() {
                let name = unsafe { CStr::from_ptr(name) };
                if let Some(n) = name.to_str().ok().and_then(fd_of_name) {
                    names.push(n);
                }
            }
            0
        }
        let mut names: Vec<c_int> = Vec::new();
        unsafe { libc::dl_iterate_phdr(Some(each), (&raw mut names).cast()) };
        names
    }

    /// `fd` if no loaded object is named `/proc/self/fd/<fd>`, else a duplicate numbered above
    /// every such name: glibc's dlopen returns an object already loaded under the name it is
    /// given, whatever file that name now opens.
    pub(super) fn unused_name(fd: OwnedFd) -> std::io::Result<OwnedFd> {
        let taken = loaded_fd_names();
        if !taken.contains(&fd.as_raw_fd()) {
            return Ok(fd);
        }
        let above = taken.iter().max().map_or(0, |n| n + 1);
        let dup = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, above) };
        if dup < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { OwnedFd::from_raw_fd(dup) })
    }

    /// A file's device (major, minor) and inode.
    pub(super) type FileId = (u32, u32, u64);

    pub(super) fn file_id(fd: &OwnedFd) -> std::io::Result<FileId> {
        let m = std::fs::metadata(proc_name(fd.as_raw_fd()))?;
        Ok((libc::major(m.dev()), libc::minor(m.dev()), m.ino()))
    }

    /// The file mapped at `addr` in this process, from `/proc/self/maps`.
    pub(super) fn mapping_of(addr: usize) -> std::io::Result<Option<FileId>> {
        Ok(mapping_in(&std::fs::read("/proc/self/maps")?, addr))
    }

    /// The file mapped at `addr`, from the text of a maps file. Only the numeric fields are
    /// read: a path may hold any bytes.
    pub(super) fn mapping_in(maps: &[u8], addr: usize) -> Option<FileId> {
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

    /// The object glibc handed back for `lib`, by its load address and its dynamic section,
    /// each with the file mapped there.
    fn linked(lib: libloading::Library) -> (libloading::Library, Result<[usize; 2], String>) {
        let raw = libloading::os::unix::Library::from(lib).into_raw();
        let mut map: *mut LinkMap = std::ptr::null_mut();
        let r = unsafe { libc::dlinfo(raw, libc::RTLD_DI_LINKMAP, (&raw mut map).cast()) };
        let lib =
            libloading::Library::from(unsafe { libloading::os::unix::Library::from_raw(raw) });
        let at = if r != 0 || map.is_null() {
            Err("dlinfo found no link map".to_string())
        } else {
            let m = unsafe { &*map };
            Ok([m.l_addr, m.l_ld as usize])
        };
        (lib, at)
    }

    /// Loads the library in the sealed memfd `fd` from `/proc/self/fd/N`, under a number no
    /// loaded object is named by. Before it looks up any symbol, the object glibc returned
    /// must be the memfd's own mapping, so nothing of an object loaded earlier under that
    /// name runs; each entry point must then lie in that mapping too.
    pub(super) fn load_memfd(fd: OwnedFd) -> Result<(Api, Held), String> {
        let fd = unused_name(fd).map_err(|e| format!("choosing its descriptor: {e}"))?;
        let name = proc_name(fd.as_raw_fd());
        let ours = file_id(&fd).map_err(|e| format!("{name}: {e}"))?;
        let in_ours = |addr: usize| -> Result<bool, String> {
            let found = mapping_of(addr).map_err(|e| format!("/proc/self/maps: {e}"))?;
            Ok(found == Some(ours))
        };
        #[allow(
            clippy::disallowed_methods,
            reason = "Linux only: a /proc/self/fd path to a sealed memfd, with no search"
        )]
        let lib = unsafe { libloading::Library::new(&name) }.map_err(|e| format!("{name}: {e}"))?;
        let (lib, at) = linked(lib);
        for addr in at.map_err(|e| format!("{name}: {e}"))? {
            if !in_ours(addr)? {
                return Err(format!(
                    "{name}: the object loaded under that name is not the sealed copy (its link \
                     map lies outside it); refused before looking up any symbol"
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
        type Enc = *mut openh264_sys2::ISVCEncoder;
        type Create = unsafe extern "C" fn(*mut Enc) -> c_int;
        type Destroy = unsafe extern "C" fn(Enc);
        type Version = unsafe extern "C" fn() -> openh264_sys2::OpenH264Version;
        let api = unsafe {
            Api {
                create: std::mem::transmute::<*mut c_void, Create>(entry("WelsCreateSVCEncoder")?),
                destroy: std::mem::transmute::<*mut c_void, Destroy>(entry(
                    "WelsDestroySVCEncoder",
                )?),
                version: std::mem::transmute::<*mut c_void, Version>(entry("WelsGetCodecVersion")?),
            }
        };
        Ok((
            api,
            Held {
                _lib: lib,
                _fd: fd,
                name,
            },
        ))
    }

    #[cfg(test)]
    impl Held {
        pub(super) fn fd(&self) -> &OwnedFd {
            &self._fd
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_are_sha256_and_in_openh264_sys2s_own_list() {
        let meta = std::process::Command::new(env!("CARGO"))
            .args(["metadata", "--format-version", "1", "--locked"])
            .args(["--filter-platform", env!("STARFOLD_TARGET")])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .expect("cargo metadata");
        assert!(
            meta.status.success(),
            "{}",
            String::from_utf8_lossy(&meta.stderr)
        );
        let meta: serde_json::Value = serde_json::from_slice(&meta.stdout).unwrap();
        let manifest = meta["packages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "openh264-sys2")
            .and_then(|p| p["manifest_path"].as_str())
            .expect("openh264-sys2 in the dependency graph");
        let list = Path::new(manifest).with_file_name("src/blobs/hashes.txt");
        let list = std::fs::read_to_string(&list).unwrap();
        for (h, name) in PINS {
            assert!(
                h.len() == 64
                    && h.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "{name}"
            );
            assert!(
                list.lines().any(|l| {
                    let mut w = l.split_whitespace();
                    w.next() == Some(*h)
                        && w.next().map(|n| n.trim_start_matches('*')) == Some(*name)
                }),
                "{name} {h} is not in openh264-sys2's list"
            );
        }
    }

    /// With `$STARFOLD_OPENH264_LIB` set (CI fetches Cisco's file), every encoder test runs on
    /// it, and this one says so; set but empty, as when the fetch failed, fails rather than
    /// testing the source build instead.
    #[test]
    fn the_library_tests_run_on() {
        let v = unsafe { (api().unwrap().version)() };
        let cisco = std::env::var_os(LIB_ENV).is_some();
        let want = if cisco { (2, 6) } else { (2, 7) };
        assert_eq!((v.uMajor, v.uMinor), want);
    }

    /// Each architecture starfold loads Cisco's binary on has exactly one pin.
    #[test]
    fn both_architectures_have_their_pin() {
        for arch in ["x86_64", "aarch64"] {
            let f = file_for(arch).unwrap();
            assert_eq!(PINS.iter().filter(|p| p.1 == f).count(), 1, "{arch}: {f}");
        }
        assert_eq!(PINS.len(), 2);
    }

    #[test]
    fn only_64_bit_linux_gnu_x86_64_and_arm64_load_it() {
        assert_eq!(
            platform_file().is_some(),
            cfg!(all(
                target_os = "linux",
                target_env = "gnu",
                target_pointer_width = "64",
                any(target_arch = "x86_64", target_arch = "aarch64")
            ))
        );
        assert!(NOT_AVAILABLE.starts_with("OpenH264 (Cisco) is not available on this platform yet"));
    }
}

#[cfg(all(test, openh264_cisco_loadable))]
mod linux_tests {
    use super::linux::*;
    use super::*;
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::sync::Mutex;

    /// Held by every test that loads a library or relies on which `/proc/self/fd` names are
    /// loaded, so that none of them sees another's descriptor numbers change under it.
    static LOADS: Mutex<()> = Mutex::new(());

    fn loads() -> std::sync::MutexGuard<'static, ()> {
        LOADS.lock().unwrap_or_else(|e| e.into_inner())
    }

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

    /// Cisco's real file, as CI fetches it.
    fn real_lib() -> Option<PathBuf> {
        let p = std::env::var_os(LIB_ENV).filter(|p| !p.is_empty());
        if p.is_none() {
            eprintln!("skipped: {LIB_ENV} is not set");
        }
        p.map(PathBuf::from)
    }

    #[test]
    fn only_the_pinned_build_is_read() {
        let dir = scratch("refuse");
        let missing = dir.join("missing.so");
        let e = Checked::read(&missing, &platform_pins()).err().unwrap();
        assert!(e.contains("missing.so"), "{e}");
        let other = dir.join("libopenh264.so");
        std::fs::write(&other, b"not cisco").unwrap();
        let e = Checked::read(&other, &platform_pins()).err().unwrap();
        assert!(e.contains(&sha256_hex(b"not cisco")), "{e}");
        assert!(e.contains("not Cisco's"), "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The other architecture's pinned build is refused here, whatever its hash.
    #[test]
    fn only_this_platforms_pin_counts() {
        let f = platform_file().unwrap();
        assert_eq!(
            platform_pins(),
            vec![*PINS.iter().find(|p| p.1 == f).unwrap()]
        );
    }

    #[test]
    fn a_large_or_special_file_is_refused_before_reading() {
        let dir = scratch("large");
        let big = dir.join("big.so");
        std::fs::File::create(&big)
            .unwrap()
            .set_len(MAX_BYTES + 1)
            .unwrap();
        let e = Checked::read(&big, &platform_pins()).err().unwrap();
        assert!(e.contains("larger than"), "{e}");
        let e = Checked::read(Path::new("/dev/zero"), &platform_pins())
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
            let _ = tx.send(Checked::read(&p, &platform_pins()).err());
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
        use std::io::Write;
        let dir = scratch("seal");
        let given = dir.join("lib.so");
        let bytes = b"the checked bytes".to_vec();
        std::fs::write(&given, &bytes).unwrap();
        let checked = Checked::read(&given, &pin_of(&bytes)).unwrap();
        std::fs::write(&given, b"swapped in place").unwrap();
        let fd = sealed_memfd(&checked.bytes, &checked.sha256).unwrap();
        let name = proc_name(fd.as_raw_fd());
        assert_eq!(std::fs::read(&name).unwrap(), bytes);
        let mut f = std::fs::File::from(fd);
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

    #[test]
    fn a_name_is_exactly_proc_self_fd_n() {
        assert_eq!(fd_of_name("/proc/self/fd/42"), Some(42));
        assert_eq!(fd_of_name("/proc/self/fd/0"), Some(0));
        for name in [
            "/proc/self/fd/042",
            "/proc/self/fd/",
            "/proc/self/fd/4x",
            "/proc/self/fd/-1",
            "/proc/self/fd/42/",
            "/proc/1/fd/42",
            "proc/self/fd/42",
        ] {
            assert_eq!(fd_of_name(name), None, "{name}");
        }
    }

    /// A descriptor no loaded object is named after keeps its number.
    #[test]
    fn a_free_name_is_kept() {
        let _g = loads();
        let fd = sealed_memfd(b"x", &sha256_hex(b"x")).unwrap();
        let n = fd.as_raw_fd();
        assert!(!loaded_fd_names().contains(&n));
        assert_eq!(unused_name(fd).unwrap().as_raw_fd(), n);
    }

    /// With Cisco's real file: after the check, the given file is replaced, in place and then
    /// by a rename over it, by bytes that are no library at all, and what production's
    /// `open()` loads is still Cisco's OpenH264 2.6, from the memfd, not from the given file.
    #[test]
    fn ciscos_bytes_load_after_the_given_file_is_replaced() {
        let _g = loads();
        let Some(real) = real_lib() else { return };
        let dir = scratch("swap");
        let given = dir.join(platform_file().unwrap());
        std::fs::copy(&real, &given).unwrap();
        let other = dir.join("other");
        let loaded = super::open(Some(&given), || {
            std::fs::write(&given, b"not a library").unwrap();
            std::fs::write(&other, b"not a library either").unwrap();
            std::fs::rename(&other, &given).unwrap();
        })
        .unwrap();
        assert_eq!(std::fs::read(&given).unwrap(), b"not a library either");
        let v = unsafe { (loaded.api.version)() };
        assert_eq!((v.uMajor, v.uMinor), (2, 6));
        let held = loaded._held.as_ref().unwrap();
        assert_eq!(
            mapping_of(loaded.api.create as usize).unwrap(),
            Some(file_id(held.fd()).unwrap())
        );
        let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
        assert!(!maps.contains(dir.to_str().unwrap()), "{maps}");
        drop(loaded);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// With Cisco's real file as the dummy: an object is loaded first under the name
    /// `/proc/self/fd/N`, from another memfd, and then N is made to refer to the sealed memfd
    /// starfold loads. glibc would hand back the dummy for that name; starfold loads its
    /// memfd under another number, and its entry points are in that memfd's mapping.
    #[test]
    fn an_object_already_loaded_under_the_name_is_not_what_loads() {
        let _g = loads();
        let Some(real) = real_lib() else { return };
        let bytes = std::fs::read(&real).unwrap();
        let sha = sha256_hex(&bytes);
        let handed = sealed_memfd(&bytes, &sha).unwrap();
        let n = handed.as_raw_fd();
        let dummy_name = proc_name(n);
        #[allow(clippy::disallowed_methods, reason = "a test's dummy, from a memfd")]
        let dummy = unsafe { libloading::Library::new(&dummy_name) }.unwrap();
        let dummy_id = file_id(&handed).unwrap();
        assert!(loaded_fd_names().contains(&n));
        let ours = sealed_memfd(&bytes, &sha).unwrap();
        let ours_id = file_id(&ours).unwrap();
        assert_ne!(ours_id, dummy_id);
        // n is this test's own descriptor: it now refers to the memfd starfold is handed.
        assert_eq!(unsafe { libc::dup2(ours.as_raw_fd(), n) }, n);
        drop(ours);
        assert_eq!(file_id(&handed).unwrap(), ours_id);
        let (api, held) = load_memfd(handed).unwrap();
        assert_ne!(held.name, dummy_name);
        assert_eq!(mapping_of(api.create as usize).unwrap(), Some(ours_id));
        assert_eq!(mapping_of(api.version as usize).unwrap(), Some(ours_id));
        let v = unsafe { (api.version)() };
        assert_eq!((v.uMajor, v.uMinor), (2, 6));
        drop(held);
        drop(dummy);
    }

    /// With Cisco's real file as the earlier object: an embedding process loads it by its
    /// path, then opens the same inode as `/proc/self/fd/N`, which glibc records as another
    /// name of that object, and gives N up while keeping the object. `dl_iterate_phdr` reports
    /// only the path, so starfold's memfd keeps N, and glibc hands back the earlier object for
    /// that name. starfold refuses it from its link map, before it looks up any symbol.
    #[test]
    fn an_alias_of_an_earlier_object_is_refused_before_any_symbol_lookup() {
        let _g = loads();
        let Some(real) = real_lib() else { return };
        let dir = scratch("alias");
        let path = dir.join(platform_file().unwrap());
        std::fs::copy(&real, &path).unwrap();
        #[allow(
            clippy::disallowed_methods,
            reason = "a test's earlier object, by its path"
        )]
        let first = unsafe { libloading::Library::new(&path) }.unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let n = file.as_raw_fd();
        let alias = proc_name(n);
        #[allow(
            clippy::disallowed_methods,
            reason = "the same inode by its descriptor's name"
        )]
        let again = unsafe { libloading::Library::new(&alias) }.unwrap();
        drop(again);
        assert!(!loaded_fd_names().contains(&n));
        let bytes = std::fs::read(&real).unwrap();
        let ours = sealed_memfd(&bytes, &sha256_hex(&bytes)).unwrap();
        // n is this test's own descriptor: it now refers to the sealed memfd.
        assert_eq!(unsafe { libc::dup2(ours.as_raw_fd(), n) }, n);
        drop(ours);
        let handed: OwnedFd = file.into();
        let c = std::ffi::CString::new(alias.clone()).unwrap();
        let h = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_LAZY | libc::RTLD_NOLOAD) };
        assert!(
            !h.is_null(),
            "glibc kept no second name for the earlier object"
        );
        unsafe { libc::dlclose(h) };
        let e = load_memfd(handed)
            .err()
            .expect("the earlier object was accepted");
        assert!(e.contains("before looking up any symbol"), "{e}");
        drop(first);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A mapped file whose path is not UTF-8 leaves the maps check working.
    #[test]
    fn a_path_that_is_not_utf8_does_not_stop_the_maps_check() {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::MetadataExt;
        let dir = scratch("maps");
        let p = dir.join(std::ffi::OsStr::from_bytes(b"map-\xff.bin"));
        std::fs::write(&p, [0u8; 4096]).unwrap();
        let f = std::fs::File::open(&p).unwrap();
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
        let meta = std::fs::metadata(&p).unwrap();
        let want = (libc::major(meta.dev()), libc::minor(meta.dev()), meta.ino());
        assert_eq!(mapping_of(m as usize).unwrap(), Some(want));
        unsafe { libc::munmap(m, 4096) };
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
