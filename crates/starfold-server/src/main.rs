#[cfg(openh264_cisco_loadable)]
mod cisco_load;

fn main() {
    #[cfg(openh264_cisco_loadable)]
    load_openh264();
    starfold::main();
}

/// Cisco's OpenH264, loaded before starfold starts its logger, its panic hook or any thread,
/// and handed to the library; the outcome is logged once the logger runs. The encoder probe
/// children this binary starts (`--probe-encoders`) probe hardware encoders only, so they load
/// nothing (and refuse to run if a library was handed over).
#[cfg(openh264_cisco_loadable)]
fn load_openh264() {
    use starfold::encoder::cisco;
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some(starfold::encoder::isolate::FLAG) {
        return;
    }
    let handoff = match cisco::lib_path(&args) {
        None => Err(cisco::no_library()),
        Some(path) => cisco_load::load_alone(
            &path,
            &cisco::platform_pins(),
            cisco::platform_file().unwrap(),
        )
        .map(|o| {
            let threads = match o.threads {
                Some(1) => "with one thread".to_string(),
                n => format!("with an unchecked thread count ({n:?})"),
            };
            cisco::Handoff {
                what: format!("{} ({}, sha256 {})", cisco::ATTRIBUTION, o.file, o.sha256),
                from: format!("{}, loaded as {} {threads}", path.display(), o.name),
                library: o.library,
                memfd: o.memfd,
                create: o.create,
                destroy: o.destroy,
                version: o.version,
            }
        }),
    };
    // SAFETY: this is the starfold binary's main, before anything else; the pointers are the
    // loaded library's own entry points, and the library and its memfd go with them.
    if let Err(e) = unsafe { cisco::install(handoff) } {
        eprintln!("starfold: {e}");
        std::process::exit(2);
    }
}
