//! `--help` and `--version`: printed instead of starting the server.

use crate::build_facts;

const USAGE: &str = "
Usage: starfold [--flag=value]...

Streams this machine's desktop to a browser at http://ADDR:PORT/ as H.264 over a
WebSocket, and prints a link that signs you in. Flags take --name=value or
--name value; true/false flags also work on their own (--read-only).

Server:
  --addr, --port                listen address (default ADDR_DEFAULT, 6901)
  --title                       the page's title (default: starfold)
  --favicon                     the page's icon: an .ico, .png, .gif, .webp or .svg
                                file (default: starfold's own)
  --allowed-hosts               host names to answer for besides IP addresses and
                                localhost (.example.com covers subdomains, * any)
  --allowed-origins             other sites whose pages may connect (* any)

Login:
  --enable-basic-auth           ask for a password (default true)
  --basic-auth-password         the password (default: $STARFOLD_PASSWORD, else
                                $SELKIES_BASIC_AUTH_PASSWORD, else one is made up
                                and printed at startup)
  --basic-auth-user             the user name a browser's sign-in has to give
                                (default: any)

Video:
  --framerate                   max frames per second (default 60)
  --high-fps                    hardware VideoToolbox streams take 120 frames
                                per second (or --framerate if higher); other
                                encoders take --framerate up to 60
                                (default false)
CRF_ENTRY
  --max-kbps, --min-kbps        adaptive bitrate ceiling and floor
                                (default 30000, 2000); --video-bitrate is
                                the same as --max-kbps
ENCODER_ENTRY
  --encoder-probe               probe the hardware encoders at startup
                                (default true)
  --experimental-hw-probes      experimental: also probe VA-API, NVENC, Media
                                Foundation and Quick Sync, each in a child
                                process (default false)
PRESET_ENTRY
OPENH264_ENTRY
  --encoder-threads            sliced threads (default cores/4, from 1 to 4)
  --adaptive-threads            encode on one thread, fanning out only under
                                sustained heavy motion (default false)
PAINTOVER_ENTRY
  --stripes                     encode the screen as N independent streams
                                (experimental, default 1)
  --convert-threads, --coalesce-us
                                tuning (default 4, 0)
";

#[cfg(not(any(target_os = "macos", windows)))]
const DESKTOP: &str = "
Desktop (the X11 display in $DISPLAY):
  --display                     X display to stream when $DISPLAY is unset
  --enable-clipboard            true, in, out or false (default true)
  --audio-enabled               desktop audio (default true)
  --audio-device-name           PulseAudio source (default output.monitor)
  --audio-bitrate               Opus bitrate (default 128000)
  --microphone-enabled          the browser's microphone into the desktop
                                (default false)
  --microphone-fifo             FIFO a PulseAudio pipe source reads
                                (default /tmp/starfold-mic.fifo)
  --enable-resize               the desktop follows the viewer's window
                                (default false)
  --resize-scale                desktop pixels per CSS pixel (default 1.5)
  --read-only                   viewers only watch (default false)
  --scroll-px-per-notch         desktop pixels one wheel notch scrolls (default 180)
  --round-corners               corner radius for top-level windows (default 0)
  --round-windows               title:radius,... rounds windows with that title
";

#[cfg(target_os = "macos")]
const DESKTOP: &str = "
Desktop (this Mac's screen):
  --display                     display to stream: an index (0 is the main
                                display) or a display ID
  --capture-scale               streamed pixels per point (default: native,
                                within the budget)
  --max-capture-pixels          pixel budget for the automatic scale
                                (default 4096000)
  --enable-clipboard            true, in, out or false (default true)
  --audio-enabled               system audio (default true)
  --audio-bitrate               Opus bitrate (default 128000)
  --microphone-enabled          the browser's microphone into the Mac
                                (default false)
  --microphone-device           output device to play it into (default: the
                                first loopback device found)
  --enable-resize               the display follows the viewer's window
                                (default false)
  --resize-scale                display pixels per CSS pixel (default: the
                                capture scale)
  --read-only                   viewers only watch (default false)
  --show-cursor                 include the host cursor in the video stream
                                (default: true with --read-only, else false)
  --scroll-px-per-notch         pixels one wheel notch scrolls (default 180)
";

#[cfg(windows)]
const DESKTOP: &str = "
Desktop (this PC's screen):
  --display                     display to stream, by index (0 is the primary)
  --enable-clipboard            true, in, out or false (default true)
  --audio-enabled               system audio (default true)
  --audio-device-name           output device whose sound to stream
                                (default: the default output device)
  --audio-bitrate               Opus bitrate (default 128000)
  --microphone-enabled          the browser's microphone into the PC
                                (default false)
  --microphone-device           output device to play it into (default: the
                                first virtual cable found)
  --enable-resize               the display follows the viewer's window
                                (default false)
  --resize-scale                display pixels per CSS pixel (default: the
                                display's scaling)
  --read-only                   viewers only watch (default false)
  --scroll-px-per-notch         pixels one wheel notch scrolls (default 180)
";

const FILES: &str = "
Files and recordings:
  --file-transfers              upload, download, upload,download or none
                                (default upload,download)
  --files-root                  folder the Files panel can see; without it
                                file transfer is off; symlinks in it are not
                                followed
  --upload-dir                  folder under the root that uploads go to
                                (default Downloads)
  --max-upload-mb               largest upload, 0 for no limit (default 0)
  --max-record-seconds          longest recording (default 300)

  -h, --help                    print this and exit
  -V, --version                 print the version and exit

Agents and scripts can use GET /screenshot, /record and /wait and POST /input; see
https://github.com/kyora-sh/starfold#for-agents-and-scripts
";

/// One flag's help entry: the description wrapped beside the flag, or under it when the flag
/// is too long to share its line.
fn entry(flag: &str, text: &str) -> String {
    const COLUMN: usize = 32;
    const WIDTH: usize = 46;
    let mut lines: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        match lines.last_mut() {
            Some(l) if l.len() + 1 + word.len() <= WIDTH => {
                l.push(' ');
                l.push_str(word);
            }
            _ => lines.push(word.to_string()),
        }
    }
    let indent = " ".repeat(COLUMN);
    let head = if flag.len() + 2 < COLUMN {
        format!("  {flag:<width$}", width = COLUMN - 2)
    } else {
        format!("  {flag}\n{indent}")
    };
    format!("{head}{}\n", lines.join(&format!("\n{indent}")))
}

/// The entries that name software encoders, from what this build includes.
fn encoder_entries(usage: &str) -> String {
    let sw = build_facts::software_h264();
    let arms = build_facts::bench_arms();
    let arms = match arms.is_empty() {
        true => String::new(),
        false => format!(
            "; or a bench arm, streamed beside {sw}: {}",
            arms.join(", ")
        ),
    };
    let fallback = match build_facts::unavailable_note(sw) {
        Some(why) => format!(
            "{sw} ({why}), so without a usable hardware H.264 encoder starfold exits at startup"
        ),
        None => sw.to_string(),
    };
    let pinnable: Vec<String> = build_facts::h264_encoders()
        .into_iter()
        .map(|name| match build_facts::unavailable_note(name) {
            Some(why) => format!("{name} ({why})"),
            None => name.to_string(),
        })
        .collect();
    let encoder = format!(
        "auto: each codec from its hardware encoder, H.264 from {fallback} where there is none \
         (default); or the H.264 encoder: {}, videotoolbox (macOS hardware), vaapi (Linux), \
         nvenc (Linux, Windows), mediafoundation (Windows) or qsv (64-bit Windows, Intel Quick \
         Sync){arms}; \
         vaapi, nvenc, mediafoundation and qsv are experimental, need --experimental-hw-probes \
         and auto never picks them; /stats shows what the probe found once starfold runs",
        pinnable.join(", ")
    );
    let crf: Vec<String> = build_facts::readers("crf")
        .into_iter()
        .map(|(name, what)| format!("{name}'s {what};"))
        .collect();
    let crf = format!("{} other encoders ignore it (default 24)", crf.join(" "));
    let presets: Vec<String> = build_facts::readers("preset")
        .into_iter()
        .map(|(name, what)| format!("{name}: {what}"))
        .collect();
    let preset = match presets.is_empty() {
        true => "ignored: this build's encoders take no preset".to_string(),
        false => format!(
            "for the encoder --encoder names (under auto, {sw}); the others keep their own \
             default. {}",
            presets.join("; ")
        ),
    };
    let paintover: Vec<String> = build_facts::readers("paintover-crf")
        .into_iter()
        .map(|(name, what)| match what {
            "paint-over quality" => name.to_string(),
            _ => format!("{name} (as its {what})"),
        })
        .collect();
    let paintover = match paintover.is_empty() {
        true => "this build's encoders ignore --paintover-crf".to_string(),
        false => format!(
            "--paintover-crf reaches {}, other encoders ignore it",
            paintover.join(", ")
        ),
    };
    let finest = build_facts::refining_to_paintover_crf();
    let finest = match finest.is_empty() {
        true => String::new(),
        false => format!(
            "; given, it is also the QP {} refines a still picture down to (default {})",
            finest.join(", "),
            crate::video::REFINE_RUNGS[2]
        ),
    };
    let paintover =
        format!("sharpening once the screen goes still (default 16, 3, 150); {paintover}{finest}");
    let paintover_flags = "--paintover-crf, --paintover-burst, --paintover-delay-ms";
    #[cfg(feature = "openh264-cisco")]
    let openh264 = openh264_entry(
        crate::encoder::cisco::platform_file(),
        crate::encoder::cisco::handed_over(),
    );
    #[cfg(not(feature = "openh264-cisco"))]
    let openh264 = String::new();
    usage
        .replace("OPENH264_ENTRY\n", &openh264)
        .replace("CRF_ENTRY\n", &entry("--crf", &crf))
        .replace("ENCODER_ENTRY\n", &entry("--encoder", &encoder))
        .replace("PRESET_ENTRY\n", &entry("--preset", &preset))
        .replace("PAINTOVER_ENTRY\n", &entry(paintover_flags, &paintover))
}

/// `--openh264-lib`'s entry, given Cisco's file for this target (none where starfold doesn't
/// load it) and whether the starfold binary handed a library or its reason over. Only that
/// binary, on a target it loads the file on, reads the flag; elsewhere `--encoder` gives the
/// reason OpenH264 is unavailable.
#[cfg(feature = "openh264-cisco")]
fn openh264_entry(file: Option<&str>, handed_over: bool) -> String {
    use crate::encoder::cisco;
    match file {
        Some(f) if handed_over => entry(
            "--openh264-lib",
            &format!(
                "Cisco's OpenH264 binary ({f}), loaded by the starfold binary at startup only if \
                 its sha256 is the build starfold pins (default ${}); without it there is no \
                 OpenH264. {}",
                cisco::LIB_ENV,
                cisco::ATTRIBUTION
            ),
        ),
        _ => String::new(),
    }
}

/// `--help`'s text, with `addr` as the default listen address.
fn help_text(version: &str, addr: &str) -> String {
    format!(
        "starfold {version}: low-latency desktop streaming for the browser.\n{}{DESKTOP}{FILES}",
        encoder_entries(&USAGE.replace("ADDR_DEFAULT", addr))
    )
}

/// `--version`'s line: the version, the software encoders built in and the binary's licence.
fn version_line(version: &str) -> String {
    format!(
        "starfold {version} (encoders: {}; licence: {})",
        build_facts::software_encoder_labels().join(", "),
        build_facts::licence()
    )
}

/// Which of the two the arguments ask for, if either. Help wins over version.
#[derive(Debug, PartialEq)]
enum Request {
    Help,
    Version,
}

fn request(args: &[String]) -> Option<Request> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        Some(Request::Help)
    } else if args.iter().any(|a| a == "--version" || a == "-V") {
        Some(Request::Version)
    } else {
        None
    }
}

/// Prints the help or the version if the arguments ask for either, and says whether it did.
pub fn print_if_asked(args: &[String]) -> bool {
    let version = env!("CARGO_PKG_VERSION");
    match request(args) {
        Some(Request::Help) => {
            // A Mac's or a PC's own screen is only served locally by default (see serve()).
            let addr = if cfg!(target_os = "linux") {
                "0.0.0.0"
            } else {
                "127.0.0.1"
            };
            print!("{}", help_text(version, addr));
            true
        }
        Some(Request::Version) => {
            println!("{}", version_line(version));
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn help_and_version_flags_are_recognized_anywhere() {
        assert_eq!(request(&args(&["--help"])), Some(Request::Help));
        assert_eq!(request(&args(&["--port=6901", "-h"])), Some(Request::Help));
        assert_eq!(request(&args(&["-V"])), Some(Request::Version));
        assert_eq!(
            request(&args(&["--version", "--port", "6901"])),
            Some(Request::Version)
        );
        assert_eq!(
            request(&args(&["--version", "--help"])),
            Some(Request::Help)
        );
    }

    #[test]
    fn help_and_version_name_only_the_software_encoders_built_in() {
        let text = format!(
            "{}\n{}",
            help_text("1.2.3", "0.0.0.0"),
            version_line("1.2.3")
        );
        let words = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let lower = words.to_lowercase();
        for e in build_facts::ENCODERS.iter().filter(|e| !e.hardware) {
            assert_eq!(lower.contains(e.name), e.built, "{}:\n{text}", e.name);
        }
        assert_eq!(lower.contains("x264"), cfg!(feature = "x264"), "{text}");
        assert_eq!(lower.contains("x265"), cfg!(feature = "x265"), "{text}");
        let sw = build_facts::software_h264();
        let fallback = match build_facts::unavailable_note(sw) {
            Some(why) => format!("H.264 from {sw} ({why}), so without"),
            None => format!("H.264 from {sw} where there is none"),
        };
        assert!(words.contains(&fallback), "{text}");
        assert!(!text.contains("_ENTRY"), "{text}");
    }

    fn help_words() -> String {
        help_text("1.2.3", "0.0.0.0")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The starfold binary that loads Cisco's binary shows `--openh264-lib` with Cisco's
    /// attribution exactly; the feature on a target starfold doesn't load it on says it is not
    /// available there yet; no other build mentions either.
    #[test]
    fn help_shows_ciscos_attribution_only_with_its_binary() {
        #[cfg(feature = "openh264-cisco")]
        crate::encoder::cisco::pretend_handed_over(true);
        let words = help_words();
        let attribution = "OpenH264 Video Codec provided by Cisco Systems, Inc.";
        let not_yet = "OpenH264 (Cisco) is not available on this platform yet";
        let cisco = cfg!(feature = "openh264-cisco");
        let loadable = cfg!(openh264_cisco_loadable);
        assert_eq!(words.contains(attribution), loadable, "{words}");
        assert_eq!(words.contains(not_yet), cisco && !loadable, "{words}");
        assert_eq!(words.contains("--openh264-lib"), loadable, "{words}");
        assert_eq!(
            words.contains("starfold exits at startup"),
            cisco && !loadable && !cfg!(feature = "x264"),
            "{words}"
        );
        #[cfg(feature = "openh264-cisco")]
        assert_eq!(crate::encoder::cisco::ATTRIBUTION, attribution);
    }

    /// A program that embeds the library (nothing handed over) offers neither the flag nor
    /// Cisco's attribution where the binary would load it, and says why OpenH264 is unavailable.
    #[test]
    fn help_in_a_program_that_embeds_the_library_offers_no_cisco_openh264() {
        #[cfg(feature = "openh264-cisco")]
        crate::encoder::cisco::pretend_handed_over(false);
        let words = help_words();
        let loadable = cfg!(openh264_cisco_loadable);
        assert!(!words.contains("--openh264-lib"), "{words}");
        assert!(
            !words.contains("OpenH264 Video Codec provided by Cisco Systems, Inc."),
            "{words}"
        );
        assert_eq!(
            words.contains("OpenH264 (Cisco) is available only in the starfold binary"),
            loadable,
            "{words}"
        );
    }

    /// `--crf` names openh264 as a reader only where it can run: in a build with OpenH264 from
    /// source, or in the starfold binary on a target it loads Cisco's binary on.
    #[test]
    fn crf_names_openh264_only_where_it_can_run() {
        let crf = |handed_over: bool| {
            #[cfg(feature = "openh264-cisco")]
            crate::encoder::cisco::pretend_handed_over(handed_over);
            let _ = handed_over;
            help_words().contains("openh264's lowest QP in motion;")
        };
        let source = cfg!(feature = "openh264");
        let loadable = cfg!(openh264_cisco_loadable);
        assert_eq!(crf(true), source || loadable);
        assert_eq!(crf(false), source);
    }

    /// `--openh264-lib` is offered by the starfold binary on a target it loads Cisco's file on,
    /// and nowhere else: not in a program that embeds the library, not on another target.
    #[cfg(feature = "openh264-cisco")]
    #[test]
    fn only_the_binary_where_it_loads_offers_the_library_flag() {
        let f = Some("libopenh264-2.6.0-linux64.8.so");
        assert!(openh264_entry(f, true).contains("--openh264-lib"));
        for (file, handed_over) in [(f, false), (None, true), (None, false)] {
            assert_eq!(openh264_entry(file, handed_over), "", "{file:?} {handed_over}");
        }
    }

    #[test]
    fn long_flags_get_their_own_line() {
        let e = entry("--a-flag-much-longer-than-the-column", "text");
        assert_eq!(
            e,
            format!(
                "  --a-flag-much-longer-than-the-column\n{}text\n",
                " ".repeat(32)
            )
        );
        assert_eq!(
            entry("--crf", "text"),
            format!("  --crf{}text\n", " ".repeat(25))
        );
    }

    #[test]
    fn version_names_the_licence_x264_makes_gpl() {
        let line = version_line("1.2.3");
        assert!(line.starts_with("starfold 1.2.3 (encoders: "));
        if cfg!(any(feature = "x264", feature = "x265")) {
            assert!(line.ends_with("; licence: GPL-2.0-or-later)"), "{line}");
        } else {
            assert!(line.ends_with("; licence: MIT)"), "{line}");
        }
    }

    #[test]
    fn other_flags_start_the_server() {
        assert_eq!(request(&args(&[])), None);
        assert_eq!(
            request(&args(&["--port=6901", "--read-only", "--title", "help"])),
            None
        );
    }
}
