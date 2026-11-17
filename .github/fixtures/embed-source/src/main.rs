//! starfold, embedded, beside the application's own OpenH264 compiled from source:
//! `--own-encoder` creates and destroys an encoder with it; anything else runs starfold.

use openh264_sys2 as sys;

fn own_encoder() {
    let mut enc = std::ptr::null_mut();
    let r = unsafe { sys::source::APILoader::WelsCreateSVCEncoder(&mut enc) };
    assert!(r == 0 && !enc.is_null(), "WelsCreateSVCEncoder failed ({r})");
    unsafe { sys::source::APILoader::WelsDestroySVCEncoder(enc) };
    println!("the application's own OpenH264 encoder: created and destroyed");
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--own-encoder") {
        own_encoder();
        return;
    }
    starfold::main();
}
