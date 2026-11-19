//! Builds libaom's AV1 encoder from the `aom` submodule: real-time only, no decoder, a
//! static library, nothing taken from the system. Checks its tools before cmake starts.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The libaom commit `aom` is pinned at: v3.15.3.
const COMMIT: &str = "b141d9c45db236335351c8a4b2a14b3ade27c937";
const SUBMODULE: &str = "crates/starfold-aom-sys/aom";

fn main() {
    let manifest = PathBuf::from(env("CARGO_MANIFEST_DIR"));
    let src = manifest.join("aom");
    let out = PathBuf::from(env("OUT_DIR"));
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=aom");

    if !src.join("CMakeLists.txt").is_file() {
        fail(&format!(
            "libaom's source is missing: {SUBMODULE} is a git submodule.\n\
             From the starfold checkout, run:\n\n    \
             git submodule update --init {SUBMODULE}\n\n\
             (or clone with --recursive). GitHub's source archives (.zip, .tar.gz) do not \
             contain submodules."
        ));
    }
    check_commit(&src);
    check_tools();
    bindings(&src, &out);
    // Linked under a name of its own, so no libaom in a system library directory can
    // stand in for it.
    let built = build(&src);
    let (from, to) = if env("CARGO_CFG_TARGET_ENV") == "msvc" {
        ("aom.lib", "starfold_aom.lib")
    } else {
        ("libaom.a", "libstarfold_aom.a")
    };
    let lib = out.join("lib");
    std::fs::create_dir_all(&lib).expect("create OUT_DIR/lib");
    std::fs::copy(built.join(from), lib.join(to)).expect("copy libaom");
    println!("cargo:rustc-link-search=native={}", lib.display());
    println!("cargo:rustc-link-lib=static=starfold_aom");
    println!("cargo:include={}", src.display());
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"))
}

fn fail(msg: &str) -> ! {
    eprintln!("\nerror: {msg}\n");
    std::process::exit(1);
}

/// In a git checkout, `aom` must be at [`COMMIT`]; a packaged crate has no `.git` to ask.
fn check_commit(src: &Path) {
    if !src.join(".git").exists() {
        return;
    }
    let Ok(o) = Command::new("git")
        .args(["-C"])
        .arg(src)
        .args(["rev-parse", "HEAD"])
        .stderr(Stdio::null())
        .output()
    else {
        return;
    };
    let head = String::from_utf8_lossy(&o.stdout).trim().to_string();
    if o.status.success() && head != COMMIT {
        fail(&format!(
            "{SUBMODULE} is at {head}, but starfold builds libaom {COMMIT} (v3.15.3).\n\
             Run:\n\n    git submodule update --init {SUBMODULE}"
        ));
    }
}

fn runs(program: &str, arg: &str) -> bool {
    Command::new(program)
        .arg(arg)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// cmake, perl (libaom generates headers with it), an assembler on x86 and libclang (for
/// the bindings); names each missing one with the line that installs it on this host.
fn check_tools() {
    let host = env("HOST");
    let arch = env("CARGO_CFG_TARGET_ARCH");
    let cmake = std::env::var("CMAKE").unwrap_or_else(|_| "cmake".into());
    let mut missing = Vec::new();
    if !runs(&cmake, "--version") {
        missing.push(Tool::Cmake);
    }
    if !runs("perl", "-v") {
        missing.push(Tool::Perl);
    }
    if matches!(arch.as_str(), "x86_64" | "x86")
        && !runs("nasm", "-v")
        && !runs("yasm", "--version")
    {
        missing.push(Tool::Nasm);
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
}

impl Tool {
    fn name(self) -> &'static str {
        match self {
            Tool::Cmake => "cmake",
            Tool::Perl => "perl",
            Tool::Nasm => "nasm (or yasm)",
            Tool::Libclang => "libclang",
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
        for p in pkgs(4).split(' ') {
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
    bindgen::Builder::default()
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
        .generate()
        .expect("bindgen libaom")
        .write_to_file(out.join("bindings.rs"))
        .expect("write libaom bindings");
}

/// Runs cmake; returns the directory that holds the static library.
fn build(src: &Path) -> PathBuf {
    let host = env("HOST");
    let target = env("TARGET");
    let os = env("CARGO_CFG_TARGET_OS");
    let arch = env("CARGO_CFG_TARGET_ARCH");
    let mut c = cmake::Config::new(src);
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
        let toolchain = match (os.as_str(), arch.as_str()) {
            ("windows", "x86_64") if target.ends_with("-gnu") => Some("x86_64-mingw-gcc.cmake"),
            ("linux", "aarch64") if target.ends_with("-gnu") => Some("arm64-linux-gcc.cmake"),
            _ => None,
        };
        match toolchain {
            Some(t) => {
                c.define("CMAKE_TOOLCHAIN_FILE", src.join("cmake/toolchains").join(t));
            }
            None => {
                let cpu = if arch == "aarch64" { "arm64" } else { &arch };
                c.define("AOM_TARGET_CPU", cpu);
            }
        }
    }
    c.build().join("lib")
}
