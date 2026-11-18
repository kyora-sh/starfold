//! Cisco's OpenH264 binary, as the `openh264-cisco` feature uses it. starfold neither contains
//! nor downloads it.
//!
//! The library never loads it. Only the `starfold` binary does, in `cisco_load.rs`, first thing
//! in its `main`, before starfold starts its logger, its panic hook or any thread, and only on
//! 64-bit Linux, x86-64 and arm64, with glibc. It hands the outcome here through [`install`];
//! the encoders read it from [`api`]. A program that embeds the library gets "available only
//! in the starfold binary" instead, and any other target gets [`NOT_AVAILABLE`]. starfold calls
//! the library only through the entry points installed here, never through openh264-sys2's
//! `DynamicAPI`.

use super::openh264::Api;
use std::ffi::c_int;
use std::path::PathBuf;
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

/// Why a program that embeds starfold's library has no Cisco OpenH264.
pub const BINARY_ONLY: &str = "OpenH264 (Cisco) is available only in the starfold binary: \
     a program that embeds starfold's library does not load it";

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

/// The pins the loader checks against: this target's build only, so another architecture's
/// authentic file is refused at the hash check.
pub fn platform_pins() -> Vec<(&'static str, &'static str)> {
    PINS.iter()
        .copied()
        .filter(|(_, f)| Some(*f) == platform_file())
        .collect()
}

pub type Create = unsafe extern "C" fn(*mut *mut openh264_sys2::ISVCEncoder) -> c_int;
pub type Destroy = unsafe extern "C" fn(*mut openh264_sys2::ISVCEncoder);
pub type Version = unsafe extern "C" fn() -> openh264_sys2::OpenH264Version;

/// Cisco's library as the starfold binary loaded it: the loader's handle and the memfd it was
/// loaded from, which keep its code mapped; its entry points; what it is (Cisco's attribution,
/// file and sha256); and where it came from. [`install`] keeps all of it for the life of the
/// process.
#[cfg(openh264_cisco_loadable)]
#[doc(hidden)]
pub struct Handoff {
    pub library: libloading::Library,
    pub memfd: std::os::fd::OwnedFd,
    pub create: Create,
    pub destroy: Destroy,
    pub version: Version,
    pub what: String,
    pub from: String,
}

struct Loaded {
    api: Api,
    what: String,
    from: String,
    /// What keeps the library mapped. In a static, so never dropped.
    #[cfg(openh264_cisco_loadable)]
    _keep: Option<(libloading::Library, std::os::fd::OwnedFd)>,
}

static LIBRARY: OnceLock<Result<Loaded, String>> = OnceLock::new();
static HANDED_OVER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The seals a hand-over's memfd must carry: nothing can write to it, grow it, shrink it or
/// change its seals any more.
#[cfg(openh264_cisco_loadable)]
const SEALS: c_int =
    libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;

/// Refuses a descriptor that does not carry every one of [`SEALS`]. Only a memfd created with
/// `MFD_ALLOW_SEALING` (or `MFD_NOEXEC_SEAL`, which implies it) can: the kernel answers
/// `F_GET_SEALS` only for a tmpfs or hugetlbfs file, and every other such file has `F_SEAL_SEAL`
/// from its creation, so it can never take `F_SEAL_WRITE`. With `F_SEAL_SEAL` among them the
/// seals are final, and with `F_SEAL_WRITE` no writable shared mapping of the file exists.
#[cfg(openh264_cisco_loadable)]
fn sealed(memfd: &std::os::fd::OwnedFd) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    let seals = unsafe { libc::fcntl(memfd.as_raw_fd(), libc::F_GET_SEALS) };
    if seals < 0 {
        return Err(format!(
            "OpenH264: the hand-over's descriptor is not a sealable memfd: {}",
            std::io::Error::last_os_error()
        ));
    }
    if seals & SEALS != SEALS {
        return Err(format!(
            "OpenH264: the hand-over's memfd is not sealed against writing, growing, shrinking \
             and further sealing (its seals: {seals:#x})"
        ));
    }
    Ok(())
}

/// The starfold binary's hand-over: what its `main` loaded, or why nothing was. The library and
/// memfd move into a static here and are never unloaded or closed.
///
/// # Safety
///
/// Only the `starfold` binary's `main` calls this, once, first thing, before any encoder
/// exists. In `Ok(h)`:
///
/// - `h.library` is Cisco's OpenH264 2.6 for this target, loaded from the contents of
///   `h.memfd`, and no code outside this function closes a reference to it. `install` checks
///   that `h.memfd` carries `F_SEAL_WRITE | F_SEAL_GROW | F_SEAL_SHRINK | F_SEAL_SEAL` and
///   refuses it otherwise, so the mapped code can't change while the library is installed.
/// - `h.create`, `h.destroy` and `h.version` are that library's own entry points, which
///   starfold calls with these C signatures:
///   - `int WelsCreateSVCEncoder(ISVCEncoder** ppEncoder)`
///   - `void WelsDestroySVCEncoder(ISVCEncoder* pEncoder)`
///   - `OpenH264Version WelsGetCodecVersion(void)`
/// - The encoder `WelsCreateSVCEncoder` creates is a C++ `ISVCEncoder` (`codec_api.h`). starfold
///   calls the methods of its vtable as C functions taking the encoder first, under the
///   Itanium C++ ABI's virtual calls (its AArch64 variant on arm64), by vtable slot counted from 0:
///   - 1 `int InitializeExt(ISVCEncoder*, const SEncParamExt*)`
///   - 2 `int GetDefaultParams(ISVCEncoder*, SEncParamExt*)`
///   - 3 `int Uninitialize(ISVCEncoder*)`
///   - 4 `int EncodeFrame(ISVCEncoder*, const SSourcePicture*, SFrameBSInfo*)`
///   - 6 `int ForceIntraFrame(ISVCEncoder*, bool bIDR, int iLayerId)`, the three-argument C++
///     method, called through a pointer of that type with `bIDR` true and `iLayerId` -1, not
///     through openh264-sys2's two-argument `ForceIntraFrame` type for the slot;
///   - 7 `int SetOption(ISVCEncoder*, ENCODER_OPTION, void*)`, with `TRACE_LEVEL`,
///     `DATAFORMAT` and `RC_MODE` on an `int`, `FRAME_RATE` on a `float`, `BITRATE` and
///     `MAX_BITRATE` on an `SBitrateInfo`, and `SVC_ENCODE_PARAM_EXT` on an `SEncParamExt`;
///   - 8 `int GetOption(ISVCEncoder*, ENCODER_OPTION, void*)`, with `SVC_ENCODE_PARAM_EXT` on an
///     `SEncParamExt` and `GET_STATISTICS` on an `SEncoderStatistics`.
/// - Each structure and constant above has the layout and value openh264-sys2 0.9.9's bindings
///   give it, and the buffers `EncodeFrame` reports in `SFrameBSInfo` stay valid until that
///   encoder's next `EncodeFrame` or `Uninitialize`.
///
/// The encoders make these calls from one thread at a time per encoder, with no further check.
///
/// A descriptor without those seals, a second call, or one after the encoders have looked, is
/// refused: nothing is installed and the `Handoff` given is dropped.
#[cfg(openh264_cisco_loadable)]
#[doc(hidden)]
pub unsafe fn install(handoff: Result<Handoff, String>) -> Result<(), String> {
    if let Ok(h) = &handoff {
        sealed(&h.memfd)?;
    }
    let loaded = handoff.map(|h| Loaded {
        api: Api {
            create: h.create,
            destroy: h.destroy,
            version: h.version,
        },
        what: h.what,
        from: h.from,
        _keep: Some((h.library, h.memfd)),
    });
    if LIBRARY.set(loaded).is_err() {
        return Err(
            "OpenH264: a library was already handed over, or the encoders already looked".into(),
        );
    }
    HANDED_OVER.store(true, std::sync::atomic::Ordering::Release);
    Ok(())
}

/// Whether the starfold binary's `main` handed over a library, or the reason it has none.
/// Without that (a program that embeds the library), `--version` and `--help` don't offer
/// Cisco's OpenH264.
pub fn handed_over() -> bool {
    #[cfg(test)]
    if let Some(v) = PRETEND.with(std::cell::Cell::get) {
        return v;
    }
    HANDED_OVER.load(std::sync::atomic::Ordering::Acquire)
}

#[cfg(test)]
thread_local! {
    static PRETEND: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// For this test thread, what [`handed_over`] says.
#[cfg(test)]
pub(crate) fn pretend_handed_over(v: bool) {
    PRETEND.with(|p| p.set(Some(v)));
}

fn library() -> &'static Result<Loaded, String> {
    LIBRARY.get_or_init(not_installed)
}

/// What was loaded (Cisco's attribution, file and sha256), or why OpenH264 is unavailable.
pub fn status() -> Result<&'static str, String> {
    library()
        .as_ref()
        .map(|l| l.what.as_str())
        .map_err(Clone::clone)
}

/// Where the loaded library came from: its path and the name it was loaded under.
pub fn origin() -> Option<&'static str> {
    library().as_ref().ok().map(|l| l.from.as_str())
}

/// The loaded library's entry points.
pub(crate) fn api() -> Result<Api, String> {
    library().as_ref().map(|l| l.api).map_err(Clone::clone)
}

/// The library's path, from `--openh264-lib` in `args` (without the program name) or from
/// `$STARFOLD_OPENH264_LIB`.
#[doc(hidden)]
pub fn lib_path(args: &[String]) -> Option<PathBuf> {
    crate::parse_args(args.iter().cloned())
        .remove("openh264-lib")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os(LIB_ENV).map(PathBuf::from))
        .filter(|p| !p.as_os_str().is_empty())
}

/// Why OpenH264 is unavailable when no library was given.
#[doc(hidden)]
pub fn no_library() -> String {
    match platform_file() {
        Some(f) => format!(
            "no OpenH264 library: get Cisco's {f} from https://www.openh264.org and pass its \
             path with --openh264-lib or {LIB_ENV}"
        ),
        None => NOT_AVAILABLE.into(),
    }
}

/// Nothing was installed: the library runs without the starfold binary's `main`.
#[cfg(not(test))]
fn not_installed() -> Result<Loaded, String> {
    Err(match platform_file() {
        Some(_) => BINARY_ONLY.into(),
        None => NOT_AVAILABLE.into(),
    })
}

/// The library's tests encode with the dev-dependency's OpenH264, unless
/// `$STARFOLD_OPENH264_LIB` names Cisco's file: then with that, loaded by the binary's own
/// loader (without its one-thread check, as the test harness runs threads).
#[cfg(test)]
fn not_installed() -> Result<Loaded, String> {
    let source = || {
        Ok(Loaded {
            api: super::openh264::SOURCE,
            what: "OpenH264 compiled from source (tests only)".into(),
            from: "the dev-dependency".into(),
            #[cfg(openh264_cisco_loadable)]
            _keep: None,
        })
    };
    let Some(path) = lib_path(&[]) else {
        return source();
    };
    #[cfg(openh264_cisco_loadable)]
    {
        let o = cisco_load::open_checked(&path, &platform_pins(), platform_file().unwrap(), || {})?;
        Ok(Loaded {
            api: Api {
                create: o.create,
                destroy: o.destroy,
                version: o.version,
            },
            what: format!("{ATTRIBUTION} ({}, sha256 {})", o.file, o.sha256),
            from: format!("{}, loaded as {}", path.display(), o.name),
            _keep: Some((o.library, o.memfd)),
        })
    }
    #[cfg(not(openh264_cisco_loadable))]
    {
        let _ = path;
        source()
    }
}

#[cfg(all(test, openh264_cisco_loadable))]
#[path = "../cisco_load.rs"]
mod cisco_load;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

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
        let cisco = std::env::var_os(LIB_ENV).is_some() && cfg!(openh264_cisco_loadable);
        let want = if cisco { (2, 6) } else { (2, 7) };
        assert_eq!((v.uMajor, v.uMinor), want);
    }

    /// The loader is given this architecture's pin only: on x86-64 the authentic arm64 file's
    /// sha256 is no pin, and the other way round.
    #[test]
    fn the_loader_gets_this_architectures_pin_only() {
        let pins = platform_pins();
        match platform_file() {
            Some(f) => {
                assert_eq!(pins.len(), 1, "{pins:?}");
                assert_eq!(pins[0].1, f);
                let other = PINS.iter().find(|p| p.1 != f).unwrap();
                assert!(!pins.iter().any(|p| p.0 == other.0), "{pins:?}");
            }
            None => assert!(pins.is_empty(), "{pins:?}"),
        }
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
        assert!(
            BINARY_ONLY.starts_with("OpenH264 (Cisco) is available only in the starfold binary")
        );
    }

    /// The path comes from `--openh264-lib` in either form, before the environment's.
    #[test]
    fn the_path_is_the_flags_then_the_environments() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            lib_path(&args(&["--openh264-lib=/a/lib.so"])),
            Some(PathBuf::from("/a/lib.so"))
        );
        assert_eq!(
            lib_path(&args(&["--port", "1", "--openh264-lib", "/b/lib.so"])),
            Some(PathBuf::from("/b/lib.so"))
        );
        assert_eq!(lib_path(&args(&["--openh264-lib="])), None);
    }

    /// A memfd holding a byte, with `seals` added.
    #[cfg(openh264_cisco_loadable)]
    fn memfd_with(seals: c_int) -> std::os::fd::OwnedFd {
        use std::io::Write;
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        let fd = unsafe {
            libc::memfd_create(
                c"sf-handoff".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        assert!(fd >= 0, "{}", std::io::Error::last_os_error());
        let mut f = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
        f.write_all(b"x").unwrap();
        if seals != 0 {
            assert_eq!(
                unsafe { libc::fcntl(f.as_raw_fd(), libc::F_ADD_SEALS, seals) },
                0
            );
        }
        f.into()
    }

    /// A hand-over of `memfd`, with this process's own handle and the tests' entry points.
    #[cfg(openh264_cisco_loadable)]
    fn handoff(memfd: std::os::fd::OwnedFd) -> Handoff {
        let api = super::super::openh264::SOURCE;
        Handoff {
            library: libloading::os::unix::Library::this().into(),
            memfd,
            create: api.create,
            destroy: api.destroy,
            version: api.version,
            what: "a test".into(),
            from: "a test".into(),
        }
    }

    /// `install` refuses a memfd that lacks any one of the four seals, an unsealed one and a
    /// descriptor that is no memfd, before it installs anything.
    #[cfg(openh264_cisco_loadable)]
    #[test]
    fn a_hand_over_without_every_seal_is_refused() {
        let all = SEALS;
        let mut cases: Vec<(String, std::os::fd::OwnedFd)> = [
            ("WRITE", libc::F_SEAL_WRITE),
            ("GROW", libc::F_SEAL_GROW),
            ("SHRINK", libc::F_SEAL_SHRINK),
            ("SEAL", libc::F_SEAL_SEAL),
        ]
        .into_iter()
        .map(|(name, s)| (format!("without F_SEAL_{name}"), memfd_with(all & !s)))
        .collect();
        cases.push(("unsealed".into(), memfd_with(0)));
        let file =
            std::fs::File::open(env!("CARGO_MANIFEST_DIR").to_string() + "/Cargo.toml").unwrap();
        cases.push(("a regular file".into(), file.into()));
        for (what, fd) in cases {
            let e = unsafe { install(Ok(handoff(fd))) }.unwrap_err();
            assert!(e.contains("the hand-over's"), "{what}: {e}");
        }
    }

    /// The memfd the binary's loader seals passes the same check.
    #[cfg(openh264_cisco_loadable)]
    #[test]
    fn the_loaders_own_sealed_memfd_is_accepted() {
        let bytes = b"sealed by the loader";
        let fd = cisco_load::sealed_memfd(bytes, &cisco_load::sha256_hex(bytes)).unwrap();
        sealed(&fd).unwrap();
        sealed(&memfd_with(SEALS)).unwrap();
    }
}
