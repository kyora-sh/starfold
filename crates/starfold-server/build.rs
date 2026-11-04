fn main() {
    let target = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let x264 = if target == "windows" {
        windows_libs()
    } else {
        unix_libs(&target)
    };
    #[cfg(feature = "x264")]
    x264_bindings(&x264.expect("x264"), &out);
    #[cfg(not(feature = "x264"))]
    let _ = x264;
    nasm_check();
    bundle_web(&out);
    println!("cargo:rerun-if-changed=build.rs");
}

/// x264's include paths, with the `x264` feature.
type X264 = Option<Vec<std::path::PathBuf>>;

#[cfg(feature = "x264")]
fn x264_bindings(include_paths: &[std::path::PathBuf], out: &std::path::Path) {
    let mut b = bindgen::Builder::default()
        .header_contents("wrap.h", "#include <stdint.h>\n#include <x264.h>\n")
        .allowlist_function("x264_.*")
        .allowlist_type("x264_.*")
        .allowlist_var("X264_.*")
        .layout_tests(false);
    for p in include_paths {
        b = b.clang_arg(format!("-I{}", p.display()));
    }
    let mut code = b.generate().expect("bindgen x264").to_string();
    // x264.h renames x264_encoder_open after the library's build number (a macro
    // bindgen can't follow); alias whichever version this x264 has.
    let open = code
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .find(|w| {
            w.strip_prefix("x264_encoder_open_")
                .is_some_and(|n| n.chars().all(|c| c.is_ascii_digit()))
        })
        .expect("x264_encoder_open_<build> in x264.h")
        .to_string();
    code.push_str(&format!("\npub use {open} as x264_encoder_open;\n"));
    std::fs::write(out.join("x264.rs"), code).unwrap();
}

/// openh264-sys2 quietly builds without its x86 assembly (several times slower) when
/// nasm is missing.
fn nasm_check() {
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let found = std::process::Command::new(std::env::var("NASM").unwrap_or("nasm".into()))
        .arg("-v")
        .output()
        .is_ok_and(|o| o.status.success());
    if matches!(arch.as_str(), "x86_64" | "x86") && !found {
        println!("cargo:warning=nasm not found: OpenH264 builds without its assembly (slow)");
    }
    println!("cargo:rerun-if-env-changed=NASM");
}

/// x264 (with the feature), libyuv, Opus and PulseAudio from pkg-config.
fn unix_libs(target: &str) -> X264 {
    let x264 = cfg!(feature = "x264").then(|| pkg_config::probe_library("x264").expect("x264"));
    // macOS captures NV12 (no libyuv) and gets audio from ScreenCaptureKit (no PulseAudio).
    let libs: &[&str] = if target == "macos" {
        &["opus"]
    } else {
        &["libyuv", "opus", "libpulse-simple"]
    };
    let others: Vec<_> = libs
        .iter()
        .filter_map(|&l| match pkg_config::probe_library(l) {
            // libyuv's CMake install writes no libyuv.pc, so Arch's and Debian's packages
            // have none: link it from the default library path.
            Err(_) if l == "libyuv" => {
                println!("cargo:rustc-link-lib=yuv");
                None
            }
            lib => Some(lib.expect(l)),
        })
        .collect();
    // Embed library dirs so the binary runs outside the build shell. The link args only
    // reach starfold's own binary, so programs that embed it get the dirs as DEP_STARFOLD_RPATH.
    let dirs: Vec<_> = x264
        .iter()
        .chain(others.iter())
        .flat_map(|lib| &lib.link_paths)
        .collect();
    for p in &dirs {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", p.display());
    }
    let rpath = std::env::join_paths(&dirs).expect("library dirs");
    println!("cargo:rpath={}", rpath.to_string_lossy());
    x264.map(|l| l.include_paths)
}

/// x264 (with the feature), libyuv and Opus from vcpkg (`vcpkg install libyuv opus`,
/// plus `x264` for the feature; triplet from VCPKGRS_TRIPLET, e.g. x64-windows-static-md
/// for a self-contained starfold.exe). Capture, audio and input use system DLLs.
fn windows_libs() -> X264 {
    let x264 = cfg!(feature = "x264").then(|| {
        vcpkg::Config::new()
            .find_package("x264")
            .expect("x264 from vcpkg")
            .include_paths
    });
    for lib in ["libyuv", "opus"] {
        vcpkg::Config::new()
            .find_package(lib)
            .unwrap_or_else(|e| panic!("{lib} from vcpkg: {e}"));
    }
    println!("cargo:rerun-if-env-changed=VCPKGRS_TRIPLET");
    x264
}

/// Bundles the TypeScript web client (packages/web/src) into OUT_DIR/web, which the server
/// embeds. Needs esbuild on PATH (or ESBUILD); shell.nix and default.nix provide it.
const WEB: &str = "../../packages/web";

fn bundle_web(out: &std::path::Path) {
    // npm installs esbuild on Windows as a .cmd shim.
    let default = if cfg!(windows) {
        "esbuild.cmd"
    } else {
        "esbuild"
    };
    let esbuild = std::env::var("ESBUILD").unwrap_or_else(|_| default.into());
    let status = std::process::Command::new(&esbuild)
        .args([
            "client=src/client/main.ts",
            "worker=src/worker.ts",
            "audio=src/worklets/audio.ts",
            "mic=src/worklets/mic.ts",
            "--bundle",
            "--format=iife",
            "--target=es2022",
            "--minify",
            "--log-level=warning",
        ])
        .arg(format!("--outdir={}", out.join("web").display()))
        .current_dir(WEB)
        .status()
        .unwrap_or_else(|e| {
            panic!(
                "running {esbuild} to bundle {WEB}/src failed ({e}); use nix-shell, or set ESBUILD"
            )
        });
    assert!(status.success(), "esbuild failed to bundle {WEB}/src");
    println!("cargo:rerun-if-changed={WEB}/src");
    println!("cargo:rerun-if-env-changed=ESBUILD");
}
