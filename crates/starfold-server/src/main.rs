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
/// nothing.
#[cfg(openh264_cisco_loadable)]
fn load_openh264() {
    use starfold::encoder::cisco;
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some(starfold::encoder::isolate::FLAG) {
        return;
    }
    let loaded = match cisco::lib_path(&args) {
        None => Err(cisco::no_library()),
        Some(path) => cisco_load::load_alone(&path, cisco::PINS, cisco::platform_file().unwrap())
            .map(|o| {
                let o = o.keep();
                cisco::Library {
                    create: o.create,
                    destroy: o.destroy,
                    version: o.version,
                    what: format!("{} ({}, sha256 {})", cisco::ATTRIBUTION, o.file, o.sha256),
                    from: format!("{}, loaded as {}", path.display(), o.name),
                }
            }),
    };
    cisco::install(loaded);
}
