//! Cisco's OpenH264 binary, loaded at run time (the `openh264-cisco` feature). starfold
//! neither contains nor downloads it: the file comes from `--openh264-lib` or
//! `$STARFOLD_OPENH264_LIB`, and only bytes whose sha256 is the pinned Cisco build for this
//! platform are loaded. They are loaded from a copy no other process can change between the
//! check and the load: a sealed memfd on Linux, a private file on other Unixes, and on Windows
//! the file itself, held open so that nobody can write, rename or delete it until it is loaded.

use openh264_sys2 as sys;
use sha2::Digest;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use sys::API;

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
    (
        "e3dc8bc01fe69363f61fd3c02fd27798537a585eadd38cd808f303d1ee505a19",
        "libopenh264-2.6.0-mac-x64.dylib",
    ),
    (
        "052e98bfcf7a9167d22f3bbb3f5988ef79065591f36af8b52924b22b13624551",
        "libopenh264-2.6.0-mac-arm64.dylib",
    ),
    (
        "2076cb5675ec6c1a4c70e7a2a322552f547b6eeed649d6dfcd9e02a543b24691",
        "openh264-2.6.0-win64.dll",
    ),
];

/// The library's path when `--openh264-lib` isn't given.
pub const LIB_ENV: &str = "STARFOLD_OPENH264_LIB";

/// The text Cisco's binary licence asks software using it to display.
pub const ATTRIBUTION: &str = "OpenH264 Video Codec provided by Cisco Systems, Inc.";

/// Larger than any pinned build (the biggest is under 2 MiB), so a path to a device or a huge
/// file fails instead of being read whole.
const MAX_BYTES: u64 = 16 << 20;

/// Cisco's file name for this platform, if a build is pinned for it.
pub fn platform_file() -> Option<&'static str> {
    let name = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "libopenh264-2.6.0-linux64.8.so",
        ("linux", "aarch64") => "libopenh264-2.6.0-linux-arm64.8.so",
        ("macos", "x86_64") => "libopenh264-2.6.0-mac-x64.dylib",
        ("macos", "aarch64") => "libopenh264-2.6.0-mac-arm64.dylib",
        ("windows", "x86_64") => "openh264-2.6.0-win64.dll",
        _ => return None,
    };
    Some(name)
}

fn platform_pins() -> Vec<(&'static str, &'static str)> {
    PINS.iter()
        .copied()
        .filter(|(_, name)| Some(*name) == platform_file())
        .collect()
}

struct Loaded {
    api: sys::DynamicAPI,
    what: String,
}

// The library's entry points are plain C functions, callable from any thread.
unsafe impl Send for Loaded {}
unsafe impl Sync for Loaded {}

static LIBRARY: OnceLock<Result<Loaded, String>> = OnceLock::new();

/// Loads OpenH264 from `path`, else from `$STARFOLD_OPENH264_LIB`, for the life of the
/// process, and says what was loaded; later calls return the first call's outcome.
pub fn load(path: Option<&Path>) -> Result<&'static str, String> {
    LIBRARY
        .get_or_init(|| open(path))
        .as_ref()
        .map(|l| l.what.as_str())
        .map_err(Clone::clone)
}

/// The loaded library (loading it from `$STARFOLD_OPENH264_LIB` if nothing has yet).
pub fn library() -> Result<&'static sys::DynamicAPI, String> {
    LIBRARY
        .get_or_init(|| open(None))
        .as_ref()
        .map(|l| &l.api)
        .map_err(Clone::clone)
}

fn no_library() -> String {
    match platform_file() {
        Some(f) => format!(
            "no OpenH264 library: get Cisco's {f} from https://www.openh264.org and pass its \
             path with --openh264-lib or {LIB_ENV}"
        ),
        None => format!(
            "no Cisco OpenH264 build is pinned for {} {}",
            std::env::consts::OS,
            std::env::consts::ARCH
        ),
    }
}

fn open(path: Option<&Path>) -> Result<Loaded, String> {
    let env = std::env::var_os(LIB_ENV).filter(|p| !p.is_empty());
    let Some(path) = path.map(Path::to_path_buf).or(env.map(PathBuf::from)) else {
        // Tests encode with the dev-dependency's OpenH264 unless $STARFOLD_OPENH264_LIB
        // names Cisco's file; a starfold binary has no such fallback.
        #[cfg(test)]
        return Ok(Loaded {
            api: sys::DynamicAPI::from_source(),
            what: "OpenH264 compiled from source (tests only)".into(),
        });
        #[cfg(not(test))]
        return Err(no_library());
    };
    let staged = Staged::new(&path, &platform_pins())?;
    let api = staged.load()?;
    let v = unsafe { api.WelsGetCodecVersion() };
    if (v.uMajor, v.uMinor) != (2, 6) {
        return Err(format!(
            "OpenH264 library {}: reports version {}.{}, not 2.6",
            path.display(),
            v.uMajor,
            v.uMinor
        ));
    }
    let what = format!("{ATTRIBUTION} ({}, sha256 {})", staged.file, staged.sha256);
    log::info!("{what}, from {}", path.display());
    Ok(Loaded { api, what })
}

fn sha256_hex(bytes: &[u8]) -> String {
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// One pinned build's verified bytes, held where no other process can change them until the
/// loader has read them.
struct Staged {
    sha256: String,
    file: &'static str,
    #[cfg(target_os = "linux")]
    memfd: std::fs::File,
    #[cfg(all(unix, not(target_os = "linux")))]
    copy: PrivateCopy,
    #[cfg(windows)]
    _held: std::fs::File,
    #[cfg(windows)]
    path: PathBuf,
}

impl Staged {
    fn new(path: &Path, pins: &[(&'static str, &'static str)]) -> Result<Self, String> {
        let shown = path.display();
        let fail = |e: std::io::Error| format!("OpenH264 library {shown}: {e}");
        let mut held = open_held(path).map_err(fail)?;
        let mut bytes = Vec::new();
        (&mut held)
            .take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(fail)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(format!(
                "OpenH264 library {shown}: larger than {} MiB, not a Cisco build",
                MAX_BYTES >> 20
            ));
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
        #[cfg(target_os = "linux")]
        return Ok(Staged {
            memfd: sealed_memfd(&bytes).map_err(fail)?,
            sha256,
            file,
        });
        #[cfg(all(unix, not(target_os = "linux")))]
        return Ok(Staged {
            copy: PrivateCopy::new(file, &bytes).map_err(fail)?,
            sha256,
            file,
        });
        #[cfg(windows)]
        return Ok(Staged {
            _held: held,
            path: path.to_path_buf(),
            sha256,
            file,
        });
    }

    /// The path the loader reads the verified bytes from.
    fn load_path(&self) -> PathBuf {
        #[cfg(target_os = "linux")]
        return PathBuf::from(format!(
            "/proc/self/fd/{}",
            std::os::fd::AsRawFd::as_raw_fd(&self.memfd)
        ));
        #[cfg(all(unix, not(target_os = "linux")))]
        return self.copy.file.clone();
        #[cfg(windows)]
        return self.path.clone();
    }

    fn load(&self) -> Result<sys::DynamicAPI, String> {
        let path = self.load_path();
        let shown = path.display();
        // A path with a slash: dlopen opens exactly that file, with no search.
        #[cfg(unix)]
        let api = unsafe { sys::libloading::APILoader::new(&path) };
        #[cfg(windows)]
        let api = unsafe {
            let path = path
                .to_str()
                .ok_or_else(|| format!("OpenH264 library {shown}: not a UTF-8 path"))?;
            let lib = crate::winlib::absolute(path)?;
            sys::libloading::APILoader::from_library(lib)
        };
        api.map(sys::DynamicAPI::Libloading)
            .map_err(|e| format!("OpenH264 library {shown}: {e}"))
    }
}

/// Opens the file for reading. On Windows others may only read it while it is open: no
/// writing, renaming or deleting until the loader has mapped it.
fn open_held(path: &Path) -> std::io::Result<std::fs::File> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x1;
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(path)
    }
    #[cfg(not(windows))]
    std::fs::File::open(path)
}

/// `bytes` in an anonymous file sealed against any change.
#[cfg(target_os = "linux")]
fn sealed_memfd(bytes: &[u8]) -> std::io::Result<std::fs::File> {
    use std::io::Write;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let fd = unsafe {
        libc::memfd_create(
            c"starfold-openh264".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    file.write_all(bytes)?;
    let seals = libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, seals) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(file)
}

/// `bytes` in a new directory only this user can enter; removed once loaded (the mapping
/// stays).
#[cfg(all(unix, not(target_os = "linux")))]
struct PrivateCopy {
    dir: PathBuf,
    file: PathBuf,
}

#[cfg(all(unix, not(target_os = "linux")))]
impl PrivateCopy {
    fn new(name: &str, bytes: &[u8]) -> std::io::Result<Self> {
        use std::io::Write;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let mut tag = [0u8; 8];
        getrandom::fill(&mut tag).map_err(std::io::Error::other)?;
        let tag: String = tag.iter().map(|b| format!("{b:02x}")).collect();
        let dir = std::env::temp_dir().join(format!("starfold-openh264-{tag}"));
        std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
        let file = dir.join(name);
        let copy = PrivateCopy { dir, file };
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o500)
            .open(&copy.file)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        Ok(copy)
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
impl Drop for PrivateCopy {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.file);
        let _ = std::fs::remove_dir(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sf-cisco-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn only_the_pinned_build_is_staged() {
        let dir = scratch("refuse");
        let missing = dir.join("missing.so");
        let e = Staged::new(&missing, &platform_pins()).err().unwrap();
        assert!(e.contains("missing.so"), "{e}");
        let other = dir.join("libopenh264.so");
        std::fs::write(&other, b"not cisco").unwrap();
        let e = Staged::new(&other, &platform_pins()).err().unwrap();
        assert!(e.contains(&sha256_hex(b"not cisco")), "{e}");
        assert!(
            e.contains("not Cisco's") || platform_file().is_none(),
            "{e}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Another platform's pinned build is refused here, whatever its hash.
    #[test]
    fn only_this_platforms_pin_counts() {
        let here: Vec<_> = platform_pins();
        assert!(here.len() <= 1);
        if let Some(f) = platform_file() {
            assert_eq!(here, vec![*PINS.iter().find(|p| p.1 == f).unwrap()]);
        }
    }

    /// Checked bytes and loaded bytes are the same bytes: replacing the file, in place or
    /// by a rename over it, after the check leaves what the loader reads unchanged.
    #[test]
    fn a_file_swapped_after_the_check_is_not_what_loads() {
        let dir = scratch("swap");
        let path = dir.join("lib");
        let checked = b"the checked bytes".to_vec();
        std::fs::write(&path, &checked).unwrap();
        let sum = sha256_hex(&checked);
        let pins = [(sum.leak() as &str, "lib")];
        let staged = Staged::new(&path, &pins).unwrap();
        let _ = std::fs::write(&path, b"swapped in place");
        let other = dir.join("other");
        std::fs::write(&other, b"swapped by rename").unwrap();
        let _ = std::fs::rename(&other, &path);
        assert_eq!(std::fs::read(staged.load_path()).unwrap(), checked);
        drop(staged);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_huge_file_is_not_read_whole() {
        #[cfg(unix)]
        {
            let e = Staged::new(Path::new("/dev/zero"), &platform_pins())
                .err()
                .unwrap();
            assert!(e.contains("larger than"), "{e}");
        }
    }

    #[test]
    fn pins_are_sha256_and_in_openh264_sys2s_own_list() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // openh264-sys2 is pinned exactly; its source is in cargo's registry.
        assert!(include_str!("../../Cargo.toml").contains("openh264-sys2 = { version = \"=0.9.9\""));
        let home = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")))
            .expect("CARGO_HOME or HOME");
        let src = home.join("registry").join("src");
        let list = std::fs::read_dir(&src)
            .unwrap_or_else(|e| panic!("{}: {e}", src.display()))
            .filter_map(Result::ok)
            .map(|d| d.path().join("openh264-sys2-0.9.9/src/blobs/hashes.txt"))
            .find(|p| p.is_file())
            .unwrap_or_else(|| panic!("no openh264-sys2 0.9.9 under {}", src.display()));
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
        let v = unsafe { library().unwrap().WelsGetCodecVersion() };
        let cisco = std::env::var_os(LIB_ENV).is_some();
        let want = if cisco { (2, 6) } else { (2, 7) };
        assert_eq!((v.uMajor, v.uMinor), want);
    }
}
