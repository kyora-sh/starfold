//! Builds libaom's AV1 encoder from the `aom` submodule: real-time only, no decoder, a
//! static library, nothing taken from the system. Checks the source and its tools before
//! cmake starts.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The libaom commit `aom` is pinned at: v3.15.3.
const COMMIT: &str = "b141d9c45db236335351c8a4b2a14b3ade27c937";
const SUBMODULE: &str = "crates/starfold-aom-sys/aom";
/// libaom's CMakeLists.txt: `cmake_minimum_required(VERSION 3.16)`.
const CMAKE_MIN: (u32, u32) = (3, 16);

fn main() {
    let manifest = PathBuf::from(env("CARGO_MANIFEST_DIR"));
    let src = manifest.join("aom");
    let out = PathBuf::from(env("OUT_DIR"));
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=layout.c");
    println!("cargo:rerun-if-changed=aom");
    for var in ["CMAKE", "CMAKE_GENERATOR", "NASM_PATH", "YASM_PATH"] {
        println!("cargo:rerun-if-env-changed={var}");
    }

    // docs.rs has no cmake or nasm and links nothing: the bindings are all its build reads.
    if std::env::var_os("DOCS_RS").is_some() {
        bindings(&src, &out);
        return;
    }
    // A crate cargo packaged (it writes .cargo_vcs_info.json) carries aom/ without its git
    // metadata; packaging checked the submodule, and the registry's checksum covers the rest.
    if src.join(".git").exists() || !manifest.join(".cargo_vcs_info.json").is_file() {
        check_source(&src);
    }
    check_tools();
    bindings(&src, &out);
    layout_probe(&src);
    let built = build(&src);
    // Linked under a name no other build uses (it carries a hash of this OUT_DIR), so no
    // library in another search directory can stand in for this one.
    let name = format!(
        "starfold_aom_{:016x}",
        fnv(out.as_os_str().as_encoded_bytes())
    );
    let (from, to) = if env("CARGO_CFG_TARGET_ENV") == "msvc" {
        ("aom.lib".to_string(), format!("{name}.lib"))
    } else {
        ("libaom.a".to_string(), format!("lib{name}.a"))
    };
    let lib = out.join("lib");
    std::fs::create_dir_all(&lib).expect("create OUT_DIR/lib");
    if !built.join(&from).is_file() {
        fail(&format!(
            "libaom's build left no {}",
            built.join(&from).display()
        ));
    }
    std::fs::copy(built.join(&from), lib.join(&to)).expect("copy libaom");
    println!("cargo:rustc-link-search=native={}", lib.display());
    println!("cargo:rustc-link-lib=static={name}");
    println!("cargo:include={}", src.display());
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"))
}

fn fail(msg: &str) -> ! {
    eprintln!("\nerror: {msg}\n");
    std::process::exit(1);
}

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
    })
}

fn git(src: &Path, args: &[&str]) -> Option<String> {
    let o = Command::new("git")
        .arg("-C")
        .arg(src)
        .args(args)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    o.status
        .success()
        .then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// `aom` must be the git submodule, checked out at [`COMMIT`] with no tracked file changed.
/// Without git, or without the submodule's git metadata, the source can't be checked, and
/// the build stops.
fn check_source(src: &Path) {
    let fix = format!(
        "From the starfold checkout, run:\n\n    git submodule update --init --force {SUBMODULE}"
    );
    if !src.join("CMakeLists.txt").is_file() {
        fail(&format!(
            "libaom's source is missing: {SUBMODULE} is a git submodule.\n{fix}\n\n\
             (or clone with --recursive). GitHub's source archives (.zip, .tar.gz) do not \
             contain submodules."
        ));
    }
    if !src.join(".git").exists() {
        fail(&format!(
            "{SUBMODULE} has no git metadata, so it can't be checked against the pinned \
             libaom commit {COMMIT}. It must be the git submodule.\n{fix}"
        ));
    }
    let Some(head) = git(src, &["rev-parse", "HEAD"]) else {
        fail(&format!(
            "git is needed to check {SUBMODULE} against the pinned libaom commit {COMMIT}, \
             and `git rev-parse HEAD` failed there (is git installed?).\n{fix}"
        ));
    };
    if head != COMMIT {
        fail(&format!(
            "{SUBMODULE} is at {head}, but starfold builds libaom {COMMIT} (v3.15.3).\n{fix}"
        ));
    }
    match git(src, &["status", "--porcelain", "--untracked-files=no"]) {
        Some(s) if s.is_empty() => {}
        Some(s) => fail(&format!(
            "{SUBMODULE} has local changes to libaom's files:\n{s}\n{fix}"
        )),
        None => fail(&format!("`git status` failed in {SUBMODULE}.\n{fix}")),
    }
    // libaom's include path puts the source root before the build directory, so a stray
    // config/aom_config.h there would replace the generated one.
    match git(src, &["ls-files", "--others", "--", "config"]) {
        Some(s) if s.is_empty() => {}
        Some(s) => fail(&format!(
            "{SUBMODULE} has files under config/ that are not libaom's, and libaom's build \
             would read them before its own generated configuration:\n{s}\n\
             Remove them, or run:\n\n    git -C {SUBMODULE} clean -fdx config"
        )),
        None => fail(&format!("`git ls-files` failed in {SUBMODULE}.\n{fix}")),
    }
    // A submodule's HEAD lives in the superproject's .git/modules, outside `aom`: watch it, so
    // moving the submodule to another commit runs this check again.
    // (Changed files inside `aom` are already watched through the directory.)
    if let Some(head_file) = git(
        src,
        &["rev-parse", "--path-format=absolute", "--git-path", "HEAD"],
    ) {
        println!("cargo:rerun-if-changed={head_file}");
    }
}

fn runs(program: &Path, arg: &str) -> Option<String> {
    let o = Command::new(program)
        .arg(arg)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    o.status
        .success()
        .then(|| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// `program` on PATH, or in the directory `hint` names (libaom's cmake searches
/// `$NASM_PATH` and `$YASM_PATH` the same way).
fn found(program: &str, arg: &str, hint: Option<&str>) -> bool {
    runs(Path::new(program), arg).is_some()
        || hint
            .and_then(std::env::var_os)
            .is_some_and(|d| runs(&Path::new(&d).join(program), arg).is_some())
}

/// `cmake --version`'s first line, `cmake version 3.28.3`, as (3, 28).
fn cmake_version(out: &str) -> Option<(u32, u32)> {
    let v = out.lines().next()?.split_whitespace().nth(2)?;
    let mut parts = v.split('.').map(|p| p.parse::<u32>().ok());
    Some((parts.next()??, parts.next()??))
}

/// cmake (3.16 or newer), perl (libaom generates headers with it), an assembler on x86, a
/// cross compiler and assembler where libaom's toolchain file names them, and libclang (for
/// the bindings); names each missing one with the line that installs it on this host.
fn check_tools() {
    let host = env("HOST");
    let target = env("TARGET");
    let arch = env("CARGO_CFG_TARGET_ARCH");
    let cmake = std::env::var("CMAKE").unwrap_or_else(|_| "cmake".into());
    let mut missing = Vec::new();
    match runs(Path::new(&cmake), "--version")
        .as_deref()
        .map(cmake_version)
    {
        Some(Some(v)) if v >= CMAKE_MIN => {}
        Some(Some((a, b))) => fail(&format!(
            "libaom needs cmake {}.{} or newer; `{cmake}` is {a}.{b}.",
            CMAKE_MIN.0, CMAKE_MIN.1
        )),
        _ => missing.push(Tool::Cmake),
    }
    if !found("perl", "-v", None) {
        missing.push(Tool::Perl);
    }
    if matches!(arch.as_str(), "x86_64" | "x86")
        && !found("nasm", "-v", Some("NASM_PATH"))
        && !found("yasm", "--version", Some("YASM_PATH"))
    {
        missing.push(Tool::Nasm);
    }
    if host != target {
        if let Some((_, prefix)) = toolchain(&target) {
            let as_needed = target.starts_with("aarch64");
            if !found(&format!("{prefix}gcc"), "--version", None)
                || (as_needed && !found(&format!("{prefix}as"), "--version", None))
            {
                missing.push(Tool::Cross(prefix));
            }
        }
    }
    if !libclang_loads() {
        missing.push(Tool::Libclang);
    }
    if missing.is_empty() {
        return;
    }
    let names: Vec<_> = missing.iter().map(|t| t.name()).collect();
    fail(&format!(
        "starfold builds libaom (its AV1 encoder) from source and needs {} on this machine.\n\
         Install with:\n\n{}",
        names.join(", "),
        install_lines(&host, &missing)
    ));
}

/// libaom's toolchain file for a cross target, and the compiler prefix it uses.
fn toolchain(target: &str) -> Option<(&'static str, &'static str)> {
    match target {
        "x86_64-pc-windows-gnu" => Some(("x86_64-mingw-gcc.cmake", "x86_64-w64-mingw32-")),
        "aarch64-unknown-linux-gnu" => Some(("arm64-linux-gcc.cmake", "aarch64-linux-gnu-")),
        _ => None,
    }
}

fn libclang_loads() -> bool {
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let ok = std::panic::catch_unwind(bindgen::clang_version).is_ok();
    std::panic::set_hook(hook);
    ok
}

#[derive(Clone, Copy, PartialEq)]
enum Tool {
    Cmake,
    Perl,
    Nasm,
    Libclang,
    Cross(&'static str),
}

impl Tool {
    fn name(self) -> String {
        match self {
            Tool::Cmake => "cmake".into(),
            Tool::Perl => "perl".into(),
            Tool::Nasm => "nasm (or yasm)".into(),
            Tool::Libclang => "libclang".into(),
            Tool::Cross(p) => format!("the cross compiler and assembler {p}gcc, {p}as"),
        }
    }

    /// Package names: Debian/Ubuntu, Fedora, Arch, Homebrew, winget.
    fn packages(self) -> [Option<&'static str>; 5] {
        match self {
            Tool::Cmake => [
                Some("cmake"),
                Some("cmake"),
                Some("cmake"),
                Some("cmake"),
                Some("Kitware.CMake"),
            ],
            Tool::Perl => [
                Some("perl"),
                Some("perl"),
                Some("perl"),
                None,
                Some("StrawberryPerl.StrawberryPerl"),
            ],
            Tool::Nasm => [
                Some("nasm"),
                Some("nasm"),
                Some("nasm"),
                Some("nasm"),
                Some("NASM.NASM"),
            ],
            Tool::Libclang => [
                Some("libclang-dev"),
                Some("clang-devel"),
                Some("clang"),
                None,
                Some("LLVM.LLVM"),
            ],
            Tool::Cross("x86_64-w64-mingw32-") => [
                Some("mingw-w64"),
                Some("mingw64-gcc"),
                Some("mingw-w64-gcc"),
                Some("mingw-w64"),
                None,
            ],
            Tool::Cross(_) => [
                Some("gcc-aarch64-linux-gnu"),
                Some("gcc-aarch64-linux-gnu"),
                Some("aarch64-linux-gnu-gcc"),
                None,
                None,
            ],
        }
    }
}

fn install_lines(host: &str, missing: &[Tool]) -> String {
    let pkgs = |i: usize| -> String {
        missing
            .iter()
            .filter_map(|t| t.packages()[i])
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut lines = Vec::new();
    if host.contains("apple") {
        if !pkgs(3).is_empty() {
            lines.push(format!("    brew install {}", pkgs(3)));
        }
        if missing
            .iter()
            .any(|t| matches!(t, Tool::Perl | Tool::Libclang))
        {
            lines.push("    xcode-select --install   # perl and libclang".into());
        }
    } else if host.contains("windows") {
        for p in pkgs(4).split(' ').filter(|p| !p.is_empty()) {
            lines.push(format!("    winget install {p}"));
        }
    } else {
        lines.push(format!(
            "    sudo apt install {}   # Debian, Ubuntu",
            pkgs(0)
        ));
        lines.push(format!("    sudo dnf install {}   # Fedora", pkgs(1)));
        lines.push(format!("    sudo pacman -S {}   # Arch", pkgs(2)));
    }
    lines.join("\n")
}

/// The encoder API: `aom/aom_encoder.h` and `aom/aomcx.h`, which need no generated header.
fn bindings(src: &Path, out: &Path) {
    let allow = "(aom|aome|av1|AOM|AV1|STARFOLD).*";
    let r = bindgen::Builder::default()
        .header_contents(
            "wrap.h",
            "#include <aom/aom_encoder.h>\n#include <aom/aomcx.h>\n\
             static const int STARFOLD_AOM_ABI = AOM_ENCODER_ABI_VERSION;\n",
        )
        .clang_arg(format!("-I{}", src.display()))
        .allowlist_function(allow)
        .allowlist_type(allow)
        .allowlist_var(allow)
        .layout_tests(false)
        .generate();
    match r {
        Ok(b) => b
            .write_to_file(out.join("bindings.rs"))
            .expect("write libaom bindings"),
        Err(e) => fail(&format!(
            "libclang could not read libaom's headers for {} ({e}). A cross build also needs \
             the target's C headers: mingw-w64 for Windows, libc6-dev-arm64-cross (Debian, \
             Ubuntu) for aarch64 Linux.",
            env("TARGET")
        )),
    }
}

/// layout.c, compiled with the C compiler and flags cmake gets, reports the sizes and
/// offsets the bindings must match; `check_layout` compares them at run time.
fn layout_probe(src: &Path) {
    cc::Build::new()
        .file("layout.c")
        .include(src)
        .compile("starfold_aom_layout");
}

/// Runs cmake; returns the directory that holds the static library.
fn build(src: &Path) -> PathBuf {
    let host = env("HOST");
    let target = env("TARGET");
    let os = env("CARGO_CFG_TARGET_OS");
    let arch = env("CARGO_CFG_TARGET_ARCH");
    let mut c = cmake::Config::new(src);
    // libaom's install rules (which put the library where this script copies it from) are
    // skipped under Xcode.
    if std::env::var("CMAKE_GENERATOR").is_ok_and(|g| g == "Xcode") {
        println!("cargo:warning=libaom is built with Unix Makefiles, not Xcode");
        c.generator("Unix Makefiles");
    }
    c.profile("Release")
        .define("CONFIG_REALTIME_ONLY", "1")
        .define("CONFIG_AV1_DECODER", "0")
        .define("BUILD_SHARED_LIBS", "0")
        .define("ENABLE_APPS", "0")
        .define("ENABLE_DOCS", "0")
        .define("ENABLE_EXAMPLES", "0")
        .define("ENABLE_TESTDATA", "0")
        .define("ENABLE_TESTS", "0")
        .define("ENABLE_TOOLS", "0")
        .define("CMAKE_INSTALL_LIBDIR", "lib");
    if os != "windows" {
        c.define("CONFIG_PIC", "1");
    }
    if env("CARGO_CFG_TARGET_ENV") == "msvc" {
        c.define("CMAKE_POLICY_DEFAULT_CMP0091", "NEW")
            .define("CMAKE_MSVC_RUNTIME_LIBRARY", "MultiThreaded");
    }
    if host != target {
        match toolchain(&target) {
            Some((file, _)) => {
                c.define(
                    "CMAKE_TOOLCHAIN_FILE",
                    src.join("cmake/toolchains").join(file),
                );
            }
            None => {
                let cpu = if arch == "aarch64" { "arm64" } else { &arch };
                c.define("AOM_TARGET_CPU", cpu);
            }
        }
    }
    c.build().join("lib")
}
