//! starfold: low-latency desktop streaming (H.264, HEVC or AV1 over WebSocket) for the
//! browser, from an X11 display on Linux or the screen of a Mac or a Windows machine.
//!
//! The `starfold` binary calls [`main`]; other programs embed the server by calling
//! [`run`] with a command line, or [`main`] from their own `main`.

mod access;
mod actions;
mod attach;
#[cfg(target_os = "linux")]
mod audio;
mod build_facts;
#[cfg(target_os = "linux")]
mod capture;
mod compat;
mod convert;
#[cfg(target_os = "linux")]
mod corners;
#[cfg(test)]
mod counting;
#[cfg(target_os = "linux")]
mod desktop;
pub mod encoder;
mod files;
mod frame;
mod help;
mod hub;
#[cfg(target_os = "linux")]
mod input;
mod login;
#[cfg(target_os = "macos")]
mod mac;
mod mic;
mod mp4;
mod negotiate;
mod readonly;
mod record;
#[cfg(target_os = "linux")]
mod screen;
mod server;
mod video;
mod wait;
#[cfg(windows)]
mod win;
#[cfg(any(windows, test))]
mod winlib;

#[cfg(test)]
#[global_allocator]
static ALLOC: counting::Counting = counting::Counting;

// The macOS and Windows backends provide the same modules with the same interfaces.
#[cfg(target_os = "macos")]
use mac::{audio, capture, desktop, input, screen};
#[cfg(windows)]
use win::{audio, capture, desktop, input, screen};

use std::collections::HashMap;
use std::future::IntoFuture;
use std::time::Instant;

/// Parses `--key=value` / `--key value` / `--flag`; unknown options are accepted so the
/// binary can take the same argument list as the Python selkies service it replaces.
fn parse_args(args: impl IntoIterator<Item = String>) -> HashMap<String, String> {
    let mut m = HashMap::new();
    let args: Vec<String> = args.into_iter().collect();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some(k) = a.strip_prefix("--") {
            if let Some((k, v)) = k.split_once('=') {
                m.insert(k.replace('_', "-"), v.to_string());
            } else if i + 1 < args.len() && !args[i + 1].starts_with("--") {
                m.insert(k.replace('_', "-"), args[i + 1].clone());
                i += 1;
            } else {
                m.insert(k.replace('_', "-"), "true".into());
            }
        }
        i += 1;
    }
    m
}

/// Selkies ranges look like "60-60"; take the upper bound.
fn range_max(v: Option<&String>, default: f64) -> f64 {
    v.and_then(|s| s.rsplit('-').next()?.trim().parse().ok())
        .unwrap_or(default)
}

pub(crate) fn truthy(v: Option<&String>, default: bool) -> bool {
    v.map(|s| matches!(s.as_str(), "true" | "1" | "yes" | "on"))
        .unwrap_or(default)
}

/// Where the server listens: TCP on --addr/--port unless --listen-tcp=false, and a
/// unix socket in the new directory --listen-unix with permissions --listen-unix-mode
/// (octal, default 0600).
#[derive(Debug, PartialEq)]
struct Listen {
    tcp: bool,
    #[cfg_attr(not(unix), allow(dead_code))]
    unix: Option<(std::path::PathBuf, u32)>,
}

fn listen_config(a: &HashMap<String, String>) -> Result<Listen, String> {
    if a.contains_key("listen-unix") && cfg!(not(target_os = "linux")) {
        return Err("--listen-unix is not available on this platform yet".into());
    }
    let tcp = truthy(a.get("listen-tcp"), true);
    let mode = match a.get("listen-unix-mode") {
        None => 0o600,
        Some(m) => u32::from_str_radix(m.trim_start_matches("0o"), 8)
            .ok()
            .filter(|m| *m <= 0o777)
            .ok_or_else(|| {
                format!("--listen-unix-mode {m}: expected octal permissions like 0600")
            })?,
    };
    let unix = match a.get("listen-unix").map(String::as_str) {
        None => None,
        Some("" | "true") => return Err("--listen-unix needs a path".into()),
        Some(p) => Some((p.into(), mode)),
    };
    if !tcp && unix.is_none() {
        return Err("--listen-tcp=false needs --listen-unix: nothing to listen on".into());
    }
    Ok(Listen { tcp, unix })
}

/// The `starfold` binary's `main`: [`run`] with the process's arguments, probing the
/// hardware encoders in child processes that run this executable again, or `starfold
/// attach DIR` (see attach.rs). A program that embeds the server can call it from its own
/// `main` to get the same isolation.
pub fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("attach") {
        std::process::exit(attach::main(&args[1..]));
    }
    if args.first().map(String::as_str) == Some(encoder::isolate::FLAG) {
        std::process::exit(encoder::isolate::child_main(&args[1..]));
    }
    #[cfg(all(windows, target_arch = "x86_64"))]
    if args.first().map(String::as_str) == Some(encoder::HELPER_FLAG) {
        std::process::exit(encoder::helper_main(&args[1..]));
    }
    let prebound = prebind(&args);
    encoder::isolate::enable();
    run_with(args, prebound);
}

/// The socket --listen-unix binds before the server starts, what removes it, and which of
/// SIGTERM and SIGINT were blocked for it (the ones that weren't blocked already).
#[cfg(target_os = "linux")]
struct Prebound {
    listener: std::os::unix::net::UnixListener,
    file: &'static server::SocketFile,
    blocked: libc::sigset_t,
}
#[cfg(not(target_os = "linux"))]
type Prebound = std::convert::Infallible;

/// --listen-unix's socket, bound first thing, while this is the process's only thread:
/// the bind changes the process's working directory and umask for a moment (see
/// [`server::bind_in`]). It is bound only in a user namespace whose uid map is the identity
/// ([`server::uid_map_is_identity`]), checked before anything is made. SIGTERM and SIGINT
/// wait (blocked) from just before the bind until the server's handlers take them, first
/// thing in [`serve`], so one that comes meanwhile still removes the socket. A bad flag,
/// a refusal or a bind that fails ends the process here, before any desktop is touched.
fn prebind(args: &[String]) -> Option<Prebound> {
    if help::asked(args) {
        return None;
    }
    let listen = listen_config(&parse_args(args.iter().cloned())).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(2);
    });
    #[cfg(target_os = "linux")]
    if let Some((path, mode)) = listen.unix {
        let threads = std::fs::read_dir("/proc/self/task").map(|d| d.count());
        if !matches!(threads, Ok(1)) {
            eprintln!(
                "--listen-unix {}: it binds while this is the process's only thread, and \
                 this process has {}",
                path.display(),
                match threads {
                    Ok(n) => format!("{n} threads"),
                    Err(e) => format!("threads that can't be counted ({e})"),
                }
            );
            std::process::exit(1);
        }
        if let Err(e) = server::uid_map_is_identity(std::path::Path::new("/proc")) {
            eprintln!("{e}");
            std::process::exit(1);
        }
        let blocked = block_shutdown_signals();
        return match server::bind_unix_std(&path, mode) {
            Ok((listener, file)) => Some(Prebound {
                listener,
                file: server::remove_at_exit(file),
                blocked,
            }),
            Err(e) => {
                eprintln!("--listen-unix {}: {e}", path.display());
                std::process::exit(1);
            }
        };
    }
    #[cfg(not(target_os = "linux"))]
    let _ = listen;
    None
}

/// Blocks SIGTERM and SIGINT on the calling thread (and the threads it starts), and returns
/// those of the two that weren't blocked before: what [`unblock_shutdown_signals`] gives
/// back, so a caller's own mask comes out as it was.
#[cfg(target_os = "linux")]
fn block_shutdown_signals() -> libc::sigset_t {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        let mut before: libc::sigset_t = std::mem::zeroed();
        let mut ours: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigemptyset(&mut ours);
        for sig in [libc::SIGTERM, libc::SIGINT] {
            libc::sigaddset(&mut set, sig);
        }
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut before);
        for sig in [libc::SIGTERM, libc::SIGINT] {
            if libc::sigismember(&before, sig) == 0 {
                libc::sigaddset(&mut ours, sig);
            }
        }
        ours
    }
}

/// Unblocks on the calling thread what [`block_shutdown_signals`] blocked, and nothing else.
#[cfg(target_os = "linux")]
fn unblock_shutdown_signals(blocked: &libc::sigset_t) {
    unsafe { libc::pthread_sigmask(libc::SIG_UNBLOCK, blocked, std::ptr::null_mut()) };
}

/// Runs the server with these command-line flags (without the program name) until it's
/// told to stop. Takes over the calling thread, which on macOS has to be the main thread.
/// Called directly (not through [`main`]), it probes the hardware encoders in-process, so
/// a driver that crashes there takes the server with it; and it refuses --listen-unix,
/// which binds before any thread starts, something only [`main`] can promise.
pub fn run(args: impl IntoIterator<Item = String>) {
    run_with(args.into_iter().collect(), None);
}

fn run_with(args: Vec<String>, prebound: Option<Prebound>) {
    // --help and --version print and return instead of starting the server.
    if help::print_if_asked(&args) {
        return;
    }
    let a = parse_args(args);
    let listen = listen_config(&a).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(2);
    });
    if let Err(e) = record::settings(&a) {
        eprintln!("{e}");
        std::process::exit(2);
    }
    if listen.unix.is_some() && prebound.is_none() {
        eprintln!(
            "--listen-unix needs the starfold binary: it binds before any thread starts, \
             which a program that embeds the server can't promise"
        );
        std::process::exit(2);
    }
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .try_init();
    // A panic on any thread ends the process, as in starfold's own release build (panic = "abort"),
    // rather than leaving the server up without that thread when the embedder unwinds.
    #[cfg(panic = "unwind")]
    {
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            hook(info);
            std::process::abort();
        }));
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");
    // AppKit and the keyboard layout APIs want the main thread, so on macOS it runs the
    // main dispatch queue and the server runs beside it.
    #[cfg(target_os = "macos")]
    {
        std::thread::spawn(move || rt.block_on(serve(a, listen, prebound)));
        dispatch2::dispatch_main();
    }
    #[cfg(not(target_os = "macos"))]
    rt.block_on(serve(a, listen, prebound));
}

/// The shared folder `serve` uses. --file-transfers=upload,download (as in selkies);
/// "none" turns both off. Whether it was given matters under --read-only (files.rs).
/// No --files-root, no file transfer: there is no default folder to share.
fn files_from_args(a: &HashMap<String, String>, read_only: bool) -> Result<files::Files, String> {
    files::Files::new(
        a.get("files-root").map(String::as_str),
        a.get("file-transfers").map(String::as_str),
        read_only,
        a.get("upload-dir").map_or("Downloads", String::as_str),
        (range_max(a.get("max-upload-mb"), 0.0) * 1048576.0) as u64,
    )
}

/// The recorder `serve` uses: GET /screenshot; /record* only with --enable-recording (off
/// by default); --max-record-seconds caps a recording's length; recordings spool in
/// --recording-dir ([`record::settings`]).
fn recorder_from_args(
    a: &HashMap<String, String>,
    hub: &std::sync::Arc<hub::Hub>,
) -> Result<std::sync::Arc<record::Recorder>, String> {
    let s = record::settings(a)?;
    Ok(record::Recorder::new(hub.clone(), s.max, s.on, s.dir))
}

async fn serve(a: HashMap<String, String>, listen: Listen, prebound: Option<Prebound>) {
    let hub = hub::Hub::new();
    // SIGTERM and SIGINT lead to the shutdown below (which removes the unix socket) from
    // here on, before anything that can block; one that came since the socket was bound
    // (blocked until now) is taken now. Only what prebind blocked is unblocked.
    #[cfg(target_os = "linux")]
    let socket_file = prebound.as_ref().map(|p| p.file);
    #[cfg(unix)]
    let (mut term, mut int) = {
        use tokio::signal::unix::{signal, SignalKind};
        (
            signal(SignalKind::terminate()).unwrap(),
            signal(SignalKind::interrupt()).unwrap(),
        )
    };
    #[cfg(target_os = "linux")]
    if let Some(p) = &prebound {
        unblock_shutdown_signals(&p.blocked);
    }
    let shutdown_hub = hub.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        tokio::select! { _ = term.recv() => {}, _ = int.recv() => {} }
        #[cfg(windows)]
        let _ = tokio::signal::ctrl_c().await;
        log::info!("shutting down");
        if let Some(tx) = shutdown_hub.input.lock().as_ref() {
            let _ = tx.send(input::InputEvent::Shutdown);
        }
        #[cfg(target_os = "linux")]
        if let Some(file) = socket_file {
            server::remove_unix(file);
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        std::process::exit(0);
    });
    // macOS: --display picks a screen (index or display ID); DISPLAY there belongs to XQuartz.
    #[cfg(target_os = "linux")]
    let display = std::env::var("DISPLAY")
        .ok()
        .or_else(|| a.get("display").cloned());
    #[cfg(not(target_os = "linux"))]
    let display = a.get("display").cloned();
    #[cfg(target_os = "macos")]
    mac::init(&a);
    #[cfg(windows)]
    win::init(&a);
    // A Mac's or a PC's own screen is only served locally unless --addr says otherwise;
    // a VM's X11 display sits behind the platform's proxy.
    let default_addr = if cfg!(not(target_os = "linux")) {
        "127.0.0.1"
    } else {
        "0.0.0.0"
    };
    let addr = a
        .get("addr")
        .cloned()
        .unwrap_or_else(|| default_addr.into());
    let port: u16 = a.get("port").and_then(|p| p.parse().ok()).unwrap_or(6901);
    let ncpu = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4) as u32;
    let selection = match a.get("encoder").map_or(Ok(encoder::Selection::Auto), |e| {
        encoder::Selection::parse(e)
    }) {
        Ok(e) => e,
        Err(e) => {
            log::error!("--encoder: {e}");
            std::process::exit(2);
        }
    };
    let encoders = std::sync::Arc::new(encoder::caps::host_probe(
        truthy(a.get("encoder-probe"), true),
        truthy(a.get("experimental-hw-probes"), false),
    ));
    encoders.log();
    let encoder = match negotiate::h264_backend(&encoders, selection) {
        Ok(b) => b,
        Err(e) => {
            log::error!("--encoder {}: {e}", selection.name());
            std::process::exit(2);
        }
    };
    if !selection.is_arm() {
        let uncapped = range_max(a.get("max-kbps").or(a.get("video-bitrate")), 30000.0) as u32 == 0;
        for flag in build_facts::FLAGS
            .into_iter()
            .filter(|f| a.contains_key(*f))
        {
            if flag == "crf" && encoder == encoder::Backend::OpenH264 {
                log::warn!(
                    "--crf with OpenH264 is its lowest QP in motion; bitrate follows --max-kbps"
                );
            } else if !build_facts::reads(encoder.name(), flag, uncapped) {
                log::warn!("{}", build_facts::flag_ignored(flag, encoder.name()));
            }
        }
    }
    let vcfg = video::VideoConfig {
        display: display.clone(),
        selection,
        fps: range_max(a.get("framerate"), 60.0) as u32,
        preset: a
            .get("preset")
            .cloned()
            .unwrap_or_else(|| encoder::DEFAULT_PRESET.into()),
        // Four sliced threads: lower latency than one, and the encoders' slicing overhead
        // grows with thread count, so more threads only add CPU (x264, 2560x1600).
        threads: a
            .get("encoder-threads")
            .and_then(|v| v.parse().ok())
            .unwrap_or((ncpu / 4).clamp(1, 4)),
        convert_threads: a
            .get("convert-threads")
            .and_then(|v| v.parse().ok())
            .unwrap_or(4),
        crf: range_max(a.get("crf"), 24.0) as f32,
        // Selkies' --video-bitrate becomes the ceiling for adaptive bitrate.
        max_kbps: range_max(a.get("max-kbps").or(a.get("video-bitrate")), 30000.0) as u32,
        paintover_crf: range_max(a.get("paintover-crf"), 16.0) as f32,
        paintover_qp: a.contains_key("paintover-crf").then(|| {
            range_max(a.get("paintover-crf"), 16.0)
                .round()
                .clamp(0.0, 51.0) as u8
        }),
        paintover_burst: range_max(a.get("paintover-burst"), 3.0) as u32,
        paintover_delay_ms: range_max(a.get("paintover-delay-ms"), 150.0) as u64,
        min_kbps: range_max(a.get("min-kbps"), 2000.0) as u32,
        stripes: range_max(a.get("stripes"), 1.0) as u32,
        // Opt-in: one thread unless a frame would miss the frame budget. x264's sliced
        // threads cost ~3x the CPU on desktop content, so this saves 20-55% CPU, but
        // small updates encode a few ms slower on one thread.
        adaptive_threads: truthy(a.get("adaptive-threads"), false),
        coalesce: std::time::Duration::from_micros(range_max(a.get("coalesce-us"), 0.0) as u64),
        high_fps: truthy(a.get("high-fps"), false),
        source_fps: 0,
    };
    if let Some(w) = video::fps_warning(vcfg.fps, vcfg.high_fps, encoder.name()) {
        log::warn!("{w}");
    }
    let clip = a
        .get("enable-clipboard")
        .map(|s| s.as_str())
        .unwrap_or("true");
    let clip_mode = desktop::ClipboardMode {
        to_server: matches!(clip, "true" | "in"),
        to_client: matches!(clip, "true" | "out"),
    };
    let audio_on = truthy(a.get("audio-enabled"), true);

    // --read-only: the viewer watches the desktop, it does not drive it. Only the
    // commands watching needs are accepted (see server::READ_ONLY_COMMANDS), uploads
    // are refused, and the page hides the controls that would send anything else.
    let read_only = truthy(a.get("read-only"), false);
    // --enable-resize (as in selkies): clients resize the desktop to their window. Not
    // for read-only viewers: a new size changes the desktop for everyone.
    let enable_resize = truthy(a.get("enable-resize"), false);
    if enable_resize && read_only {
        log::warn!("--enable-resize is ignored with --read-only: viewers can't resize the desktop");
    }
    let screen = (enable_resize && !read_only)
        .then(|| match screen::Screen::open(display.as_deref()) {
            Ok(s) => {
                log::info!("dynamic resolution on, up to {}x{}", s.max().0, s.max().1);
                Some(s)
            }
            Err(e) => {
                log::warn!("dynamic resolution unavailable: {e}");
                None
            }
        })
        .flatten();
    let resize_on = screen.is_some();
    // Desktop pixels per CSS pixel of the viewer's window. The default matches a
    // 144 DPI desktop (apps drawn at 1.5x), so windows look as big as in the viewer;
    // on a Mac, the capture's pixels per point.
    #[cfg(target_os = "linux")]
    let default_scale = 1.5;
    #[cfg(target_os = "macos")]
    let default_scale = mac::capture_scale();
    // On Windows, the display's scaling (150% -> 1.5).
    #[cfg(windows)]
    let default_scale = win::dpi_scale();
    let resize_scale = range_max(a.get("resize-scale"), default_scale);
    let mut candidates = negotiate::candidates(&encoders, encoder);
    // STARFOLD_TEST_HOOKS: always something for POST /debug/fail-h264 to fall back to.
    if crate::compat::is_set("STARFOLD_TEST_HOOKS") && encoder != encoder::SOFTWARE_H264 {
        let fallback = negotiate::Candidate {
            codec: encoder::Codec::H264,
            backend: encoder::SOFTWARE_H264,
            hardware: false,
        };
        if !candidates.contains(&fallback) {
            candidates.push(fallback);
        }
    }
    #[cfg(feature = "sw-arm")]
    if let encoder::Selection::Arm(name, backend, codec) = selection {
        candidates = negotiate::arm_candidates(backend, codec, candidates).unwrap_or_else(|e| {
            log::error!("--encoder {name}: {e}");
            std::process::exit(2);
        });
    }
    for c in negotiate::chosen(&candidates) {
        log::info!(
            "--encoder {}: {:?} from {} ({})",
            selection.name(),
            c.codec,
            c.backend.name(),
            if c.hardware { "hardware" } else { "software" }
        );
    }
    let vcfg = video::VideoConfig {
        source_fps: match cfg!(target_os = "macos") {
            true => video::source_fps(&vcfg, &candidates),
            false => 0,
        },
        ..vcfg
    };
    #[cfg(target_os = "macos")]
    mac::set_fps(vcfg.source_fps);
    if let Err(e) = video::spawn(vcfg, hub.clone(), screen, candidates) {
        log::error!("video init failed: {e}");
        std::process::exit(1);
    }
    let radius = range_max(a.get("round-corners"), 0.0) as u16;
    // --round-windows=title:radius,...: round specific windows (docks included) by title.
    let named: Vec<(String, u16)> = a
        .get("round-windows")
        .map(|v| {
            v.split(',')
                .filter_map(|e| {
                    e.rsplit_once(':')
                        .and_then(|(n, r)| Some((n.to_string(), r.parse().ok()?)))
                })
                .collect()
        })
        .unwrap_or_default();
    if radius > 0 || !named.is_empty() {
        #[cfg(target_os = "linux")]
        if let Err(e) = corners::spawn(display.clone(), radius, named) {
            log::warn!("rounded corners unavailable: {e}");
        }
        #[cfg(not(target_os = "linux"))]
        log::info!("--round-corners/--round-windows only apply to X11 desktops");
    }
    let clipboard = match desktop::spawn(display.clone(), hub.clone(), clip_mode) {
        Ok(h) => Some(h),
        Err(e) => {
            log::error!("cursor/clipboard init failed: {e}");
            None
        }
    };
    let px_per_notch = range_max(a.get("scroll-px-per-notch"), 180.0);
    match input::spawn(display.clone(), clipboard, px_per_notch) {
        Ok(tx) => *hub.input.lock() = Some(tx),
        Err(e) => log::error!("input init failed: {e}"),
    }
    if audio_on {
        let dev = a
            .get("audio-device-name")
            .cloned()
            .unwrap_or_else(|| "output.monitor".into());
        let br = range_max(a.get("audio-bitrate"), 128000.0) as i32;
        audio::spawn(dev, br, hub.clone());
    }

    let files = files_from_args(&a, read_only).unwrap_or_else(|e| {
        log::error!("{e}");
        std::process::exit(2);
    });
    let files = std::sync::Arc::new(files);
    match &files.root {
        Some(root) => log::info!(
            "file transfer under {}: upload {}, download {}",
            root.path().display(),
            if files.upload { "on" } else { "off" },
            if files.download { "on" } else { "off" },
        ),
        None => log::info!("file transfer: off (share a folder with --files-root)"),
    }
    let recorder = recorder_from_args(&a, &hub).unwrap_or_else(|e| {
        log::error!("{e}");
        std::process::exit(2);
    });
    // --microphone-enabled (as in selkies): browser microphone -> a FIFO that a
    // PulseAudio module-pipe-source reads as the VM's microphone, or on macOS an
    // output device that loops back to an input (BlackHole and the like).
    #[cfg(target_os = "linux")]
    let mic_sink = Some(compat::mic_fifos(
        a.get("microphone-fifo").map(String::as_str),
    ));
    #[cfg(target_os = "macos")]
    let mic_sink = truthy(a.get("microphone-enabled"), false)
        .then(|| mac::mic::find_device(a.get("microphone-device").map(|s| s.as_str())))
        .flatten();
    // Windows: a virtual audio cable's playback side (VB-CABLE and the like).
    #[cfg(windows)]
    let mic_sink = truthy(a.get("microphone-enabled"), false)
        .then(|| win::mic::find_device(a.get("microphone-device").map(|s| s.as_str())))
        .flatten();
    let mic = truthy(a.get("microphone-enabled"), false)
        .then_some(())
        .and(mic_sink)
        .map(|sink| mic::spawn(sink, hub.clone()));
    {
        let mut extra = hub.hello_extra.lock();
        extra.insert("os".into(), std::env::consts::OS.into());
        extra.insert("audio".into(), audio_on.into());
        extra.insert("mic".into(), mic.is_some().into());
        if read_only {
            extra.insert("readOnly".into(), true.into());
        }
        extra.insert("files".into(), files.hello());
        if resize_on {
            extra.insert(
                "resize".into(),
                serde_json::json!({ "scale": resize_scale }),
            );
        }
    }
    // --favicon: the tab's icon (default: starfold's own).
    let (favicon, favicon_type) = server::favicon(a.get("favicon").map(|s| s.as_str()))
        .unwrap_or_else(|e| {
            log::error!("{e}");
            std::process::exit(1);
        });
    // --framed: for hops that carry binary WebSocket messages only (see frame.rs).
    let framed = truthy(a.get("framed"), false);
    let state = server::AppState {
        hub: hub.clone(),
        audio: audio_on,
        epoch: Instant::now(),
        files,
        mic,
        resize: resize_on,
        read_only,
        recorder,
        // --title: the browser tab's title.
        index: server::index_html(a.get("title").map_or("starfold", |t| t.as_str())),
        favicon,
        favicon_type,
        encoders,
        framed,
    };
    // --allowed-hosts and --allowed-origins: the host names and other sites starfold answers
    // besides IP addresses, localhost and its own page (see access.rs).
    let access = std::sync::Arc::new(access::Access::new(
        a.get("allowed-hosts").map(|s| s.as_str()),
        a.get("allowed-origins").map(|s| s.as_str()),
    ));
    // The login (see login.rs): a password, given or made up at startup, unless
    // --enable-basic-auth=false.
    let login =
        login::Login::from_args(&a).map(|(l, generated)| (std::sync::Arc::new(l), generated));
    let mut app = server::router(state);
    if let Some((l, _)) = &login {
        app = app.layer(axum::middleware::from_fn_with_state(
            l.clone(),
            login::check,
        ));
    }
    // Added last, so it runs first: a request for the wrong host or from another site's
    // page is turned away before the login is asked for.
    let app = app.layer(axum::middleware::from_fn_with_state(access, access::check));
    // A port that can't be had ends the process through `exit`, which removes the unix
    // socket bound before it.
    let tcp = match listen.tcp {
        true => match tokio::net::TcpListener::bind((addr.as_str(), port)).await {
            Ok(l) => Some(l),
            Err(e) => {
                log::error!("listening on {addr}:{port}: {e}");
                std::process::exit(1);
            }
        },
        false => None,
    };
    #[cfg(target_os = "linux")]
    let unix = match (prebound, &listen.unix) {
        (Some(p), Some((path, mode))) => {
            let l = server::unix_server(p.listener, p.file).unwrap_or_else(|e| {
                log::error!("--listen-unix {}: {e}", path.display());
                std::process::exit(1);
            });
            log::info!(
                "listening on unix:{} (mode {mode:o}; only uid {} is let in)",
                server::unix_socket_path(path).display(),
                unsafe { libc::geteuid() }
            );
            Some(l)
        }
        _ => None,
    };
    #[cfg(not(target_os = "linux"))]
    let _ = prebound;
    if tcp.is_some() {
        log::info!("listening on http://{addr}:{port}");
    }
    // Where to point a browser on this machine.
    let page = match addr.as_str() {
        _ if tcp.is_none() => "/ on the unix socket".into(),
        "0.0.0.0" | "::" => format!("http://localhost:{port}/"),
        a if a.contains(':') => format!("http://[{a}]:{port}/"),
        a => format!("http://{a}:{port}/"),
    };
    match &login {
        Some((l, true)) => log::info!(
            "open {page}?token={} to sign in (the password is {0}; choose one with --basic-auth-password or STARFOLD_PASSWORD)",
            l.password()
        ),
        Some((_, false)) => log::info!(
            "login on: sign in at {page} with the password, or open {page}?token=<password>"
        ),
        None => log::info!("login off (--enable-basic-auth=false): anyone who reaches {page} can use this desktop"),
    }
    if framed {
        log::info!("--framed: every WebSocket message is binary with a kind byte");
    }
    let mut servers = tokio::task::JoinSet::new();
    #[cfg(target_os = "linux")]
    if let Some(l) = unix {
        servers.spawn(axum::serve(l, app.clone()).into_future());
    }
    if let Some(l) = tcp {
        use axum::serve::ListenerExt;
        let l = l.tap_io(|tcp| {
            let _ = tcp.set_nodelay(true);
        });
        servers.spawn(axum::serve(l, app).into_future());
    }
    while let Some(r) = servers.join_next().await {
        r.unwrap().unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listen(args: &[&str]) -> Result<Listen, String> {
        listen_config(&parse_args(args.iter().map(|s| s.to_string())))
    }

    #[test]
    fn tcp_alone_unless_a_unix_socket_is_asked_for() {
        let tcp_only = Listen {
            tcp: true,
            unix: None,
        };
        assert_eq!(listen(&[]), Ok(tcp_only));
        // --listen-unix is Linux's only; elsewhere asking for it is refused at start.
        let unix = |args: &[&str], want: Listen| match cfg!(target_os = "linux") {
            true => assert_eq!(listen(args), Ok(want)),
            false => assert_eq!(
                listen(args),
                Err("--listen-unix is not available on this platform yet".into())
            ),
        };
        unix(
            &["--listen-unix", "/run/sf.sock"],
            Listen {
                tcp: true,
                unix: Some(("/run/sf.sock".into(), 0o600)),
            },
        );
        unix(
            &[
                "--listen-unix=/run/sf.sock",
                "--listen-tcp=false",
                "--listen-unix-mode=0660",
            ],
            Listen {
                tcp: false,
                unix: Some(("/run/sf.sock".into(), 0o660)),
            },
        );
    }

    #[test]
    fn nothing_to_listen_on_or_a_bad_mode_is_an_error() {
        assert!(listen(&["--listen-tcp=false"]).is_err());
        assert!(listen(&["--listen-unix"]).is_err());
        for mode in ["0999", "rw", "01777"] {
            assert!(
                listen(&["--listen-unix=/s", &format!("--listen-unix-mode={mode}")]).is_err(),
                "{mode}"
            );
        }
    }

    /// Off Linux the platform refusal comes before anything else said about --listen-unix:
    /// a bare flag, a bad mode or TCP off all get it.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn off_linux_the_refusal_comes_first() {
        let refused = Err("--listen-unix is not available on this platform yet".to_string());
        for args in [
            &["--listen-unix"][..],
            &["--listen-unix=/s", "--listen-unix-mode=rw"],
            &["--listen-unix=/s", "--listen-tcp=false"],
        ] {
            assert_eq!(listen(args), refused, "{args:?}");
        }
    }
}
