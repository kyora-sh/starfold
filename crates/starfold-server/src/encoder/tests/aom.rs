//! libaom's paint-over: refinement frames that re-code a still picture, and the references
//! that keep its rate control on the stream's own frames.

use super::super::aom::{as_qp, quantizer, references, screen_hash, Aom, Screen, Screens, SCREENS};
use super::*;

const W: usize = 640;
const H: usize = 384;
const FINE: Refine = Refine {
    qp: 12,
    max_bytes: None,
};

struct Stream {
    enc: Aom,
    pic: Picture,
    pts: i64,
    interval_ms: i64,
    scrolled: usize,
}

impl Stream {
    fn new(kbps: u32) -> Self {
        let cfg = EncoderConfig {
            codec: Codec::Av1,
            preset: String::new(),
            ..config(Backend::Libaom, W as u32, H as u32, kbps)
        };
        let mut s = Stream {
            enc: Aom::new(&cfg).unwrap(),
            pic: Picture::new(W, H),
            pts: 0,
            interval_ms: 33,
            scrolled: 0,
        };
        assert!(s.frame(None) > 0);
        s
    }

    fn frame(&mut self, refine: Option<Refine>) -> usize {
        let (n, key) = self.encode(refine, false, None);
        assert!(self.pts == 33 || !key, "a keyframe at {} ms", self.pts);
        n
    }

    /// Bytes out, and whether they are a keyframe.
    fn encode(
        &mut self,
        refine: Option<Refine>,
        force_keyframe: bool,
        static_mbs: Option<&mut [u8]>,
    ) -> (usize, bool) {
        self.pts += self.interval_ms;
        let aus = self
            .enc
            .encode(
                &self.pic.view(),
                EncodeOptions {
                    pts_ms: self.pts,
                    force_keyframe,
                    static_mbs,
                    refine,
                },
            )
            .unwrap();
        (
            aus.iter().map(|a| a.data.len()).sum(),
            aus.iter().any(|a| a.keyframe),
        )
    }

    /// Shows page `seed`, then sharpens it: the refinement stores it. Bytes of its first frame.
    fn page(&mut self, seed: u32) -> usize {
        self.pic.texture(seed << 29, 0, 0, W, H, 0);
        let n = self.frame(None);
        self.frame(None);
        assert!(self.frame(Some(FINE)) > 0, "page {seed} was not refined");
        n
    }

    /// The mean quantizer and bytes of the last ten of `frames` frames of scrolled text.
    fn scroll(&mut self, frames: usize) -> (f64, f64) {
        let (mut q, mut bytes) = (0.0, 0.0);
        for f in 0..frames {
            self.scrolled += 12;
            self.pic.texture(0, 0, 0, W, H, self.scrolled);
            let n = self.frame(None);
            if f + 10 >= frames {
                q += self.enc.last_quantizer().unwrap() as f64 / 10.0;
                bytes += n as f64 / 10.0;
            }
        }
        (q, bytes)
    }

    /// `frames` frames of full-picture motion, which a capped stream codes coarsely; scrolled
    /// text it codes all but losslessly with its screen-content tools.
    fn play(&mut self, frames: usize) {
        for f in 0..frames {
            self.pic.video(f);
            self.frame(None);
        }
    }

    fn psnr(&mut self) -> f64 {
        luma_psnr(&self.enc.reconstruction().unwrap(), W, &self.pic)
    }

    fn refinable(&self, qp: u8) -> bool {
        self.enc.refinable(Refine {
            qp,
            max_bytes: None,
        })
    }
}

#[test]
fn a_refused_rate_change_keeps_the_rate_in_force() {
    let mut s = Stream::new(2000);
    s.enc.cap_quantizer(10).unwrap();
    let before = s.enc.rate_state();
    assert!(s.enc.set_rate(RateConfig::new(40.0, 900, 10)).is_err());
    assert_eq!(s.enc.rate_state(), before);
    s.enc.set_rate(RateConfig::new(5.0, 900, 10)).unwrap();
    assert_eq!(s.enc.rate_state(), (900, 5, 100));
    assert!(s.frame(None) > 0);
}

/// 150 / 75 / 90 ms: a link that drops keeps scroll's tail near x264's (1000 / 500 / 600
/// doubled it).
#[test]
fn the_rate_buffer_is_150_ms() {
    assert_eq!(Stream::new(4000).enc.rate_buffer(), (150, 75, 90));
}

#[test]
fn only_slot_0_is_refreshed_and_unused_names_are_on_the_spare_slot() {
    const LAST: usize = 0;
    const GOLDEN: usize = 3;
    for (refine, used) in [(false, LAST), (true, GOLDEN)] {
        let c = references(refine, Screen::None);
        assert_eq!(c.refresh, [1, 0, 0, 0, 0, 0, 0, 0], "refine {refine}");
        let mut reference = [0; 7];
        reference[used] = 1;
        assert_eq!(c.reference, reference, "refine {refine}");
        for (name, &slot) in c.ref_idx.iter().enumerate() {
            let want = if name == LAST || name == used { 0 } else { 7 };
            assert_eq!(slot, want, "refine {refine}, name {name}");
        }
    }
}

#[test]
fn the_ladder_maps_onto_libaoms_quantizer() {
    let rungs: Vec<_> = [0u8, 12, 13, 14, 15, 16, 20, 51].map(quantizer).into();
    assert_eq!(rungs, [4, 4, 8, 12, 16, 20, 25, 63]);
    for qp in 13..=51u8 {
        assert!(quantizer(qp) > quantizer(qp - 1), "qp {qp}");
    }
    for qp in 12..=51u8 {
        assert_eq!(as_qp(quantizer(qp)), qp as i32, "qp {qp}");
    }
    for q in 0..=63 {
        let qp = as_qp(q);
        assert!(quantizer(qp as u8) >= q, "q {q}");
        assert!(qp == 12 || quantizer(qp as u8 - 1) < q, "q {q}");
    }
}

#[test]
fn a_still_picture_is_sharp_after_one_pass_whose_budget_fits() {
    let mut s = Stream::new(1000);
    s.play(30);
    s.frame(None);
    s.frame(None);
    let still = s.psnr();
    assert!(s.refinable(20));
    let n = s.frame(Some(Refine {
        qp: 20,
        max_bytes: None,
    }));
    let psnr = s.psnr();
    eprintln!("one pass: {n} bytes, {psnr:.2} dB (still {still:.2})");
    assert!(n > 0);
    assert!(psnr >= still + 3.0, "{psnr:.2} from {still:.2}");
    assert_eq!(s.enc.last_quantizer().unwrap(), 4);
    for qp in [20u8, 16, 12] {
        assert!(!s.refinable(qp), "qp {qp}");
        let n = s.frame(Some(Refine {
            qp,
            max_bytes: None,
        }));
        assert_eq!(n, 0, "qp {qp}");
    }
    assert!((s.psnr() - psnr).abs() < 0.01);
}

#[test]
fn a_pass_that_cannot_fit_sends_nothing() {
    let mut s = Stream::new(2000);
    s.scroll(30);
    let small = Refine {
        qp: 12,
        max_bytes: Some(16),
    };
    assert!(!s.enc.refinable(small));
    assert_eq!(s.frame(Some(small)), 0);
    assert!(s.frame(None) > 0);
}

#[test]
fn motion_after_refinements_keeps_its_quantizer() {
    let mut s = Stream::new(2000);
    let (q, bytes) = s.scroll(40);
    for _ in 0..3 {
        s.frame(None);
        let n = s.frame(Some(Refine {
            qp: 20,
            max_bytes: None,
        }));
        assert!(n > 0, "no refinement went out");
        let (q_after, bytes_after) = s.scroll(40);
        eprintln!("motion q {q:.1} -> {q_after:.1}, bytes {bytes:.0} -> {bytes_after:.0}");
        assert!(q_after <= q + 6.0, "q {q:.1} -> {q_after:.1}");
        assert!(
            bytes_after >= bytes * 0.5 && bytes_after <= bytes * 2.0,
            "bytes {bytes:.0} -> {bytes_after:.0}"
        );
    }
}

/// Frames now come a third as often: CBR's target is the bitrate times the frame interval,
/// so each gets about three times the bytes (at least twice, with room for the 150 ms
/// buffer's correction), and the stream carries on without a keyframe. The bitrate is low
/// enough that neither rate reaches the quantizer floor, where bytes follow the picture.
#[test]
fn a_lower_frame_rate_budgets_more_per_frame_without_a_keyframe() {
    let mut s = Stream::new(500);
    let run = |s: &mut Stream, from: usize, step: usize| {
        let bytes: Vec<usize> = (0..45)
            .map(|i| {
                s.pic.video(from + i * step);
                s.frame(None)
            })
            .collect();
        let tail = &bytes[15..];
        (
            tail.iter().sum::<usize>() / tail.len(),
            s.enc.last_quantizer().unwrap(),
        )
    };
    let (at30, q30) = run(&mut s, 0, 1);
    s.enc.set_rate(RateConfig::new(24.0, 500, 10)).unwrap();
    s.interval_ms = 100;
    let (at10, q10) = run(&mut s, 45, 3);
    eprintln!(
        "libaom 500 kbps: {at30} bytes a frame at 30 fps (q {q30}), {at10} at 10 fps (q {q10})"
    );
    assert!(q30 > 24 && q10 > 24, "q {q30} and {q10}: on the floor");
    assert!(
        at10 >= at30 * 2,
        "{at30} bytes a frame at 30 fps, {at10} at 10"
    );
}

#[test]
fn a_stored_screen_is_refreshed_by_its_refinement_and_referenced_alone() {
    const ALTREF: usize = 6;
    let c = references(true, Screen::Store(2));
    assert_eq!(c.refresh, [1, 0, 1, 0, 0, 0, 0, 0]);
    assert_eq!(c.reference, [0, 0, 0, 1, 0, 0, 0]);
    assert_eq!(c.ref_idx, [0, 7, 7, 0, 7, 7, 2]);
    assert_eq!(c.ref_idx[ALTREF], 2);
    let c = references(false, Screen::Back(4));
    assert_eq!(c.refresh, [1, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(c.reference, [0, 0, 0, 1, 0, 0, 0]);
    assert_eq!(c.ref_idx, [0, 7, 7, 4, 7, 7, 7]);
    assert_eq!(
        references(false, Screen::Store(2)).refresh,
        references(false, Screen::None).refresh
    );
}

#[test]
fn screens_fill_empty_slots_first_then_evict_the_least_recently_used() {
    let mut s = Screens::default();
    for h in 1..=SCREENS as u64 {
        let k = s.place(h * 10 + 1);
        assert_eq!(k, h as usize);
        s.store(k, h * 10 + 1);
    }
    assert_eq!(s.place(21), 2, "a stored screen keeps its slot");
    s.used(1);
    s.used(3);
    assert_eq!(s.place(99), 2);
    s.store(2, 99);
    assert_eq!(s.place(98), 4);
    s.store(4, 98);
    assert_eq!(s.hashes(), [11, 99, 31, 98, 51, 61]);
    s.clear();
    assert_eq!(s.hashes(), [0; SCREENS]);
    assert_eq!(s.place(7), 1);
}

#[test]
fn a_return_to_a_stored_screen_costs_a_fraction_of_the_page() {
    let bytes = [false, true].map(|on| {
        let mut s = Stream::new(4000);
        s.enc.stored_screens(on);
        s.page(1);
        s.page(2);
        s.pic.texture(1 << 29, 0, 0, W, H, 0);
        let b = s.frame(None);
        let psnr = s.psnr();
        eprintln!("stored screens {on}: back to page 1 in {b} bytes, {psnr:.2} dB");
        assert!(psnr > 30.0, "{psnr:.2} dB");
        b
    });
    assert!(bytes[1] * 4 < bytes[0], "{bytes:?}");
}

#[test]
fn a_keyframe_clears_the_stored_screens() {
    let mut s = Stream::new(4000);
    s.enc.stored_screens(true);
    s.page(1);
    s.page(2);
    assert_eq!(s.enc.screens().unwrap().iter().filter(|&&h| h != 0).count(), 2);
    s.pic.texture(1 << 29, 0, 0, W, H, 0);
    s.frame(None);
    let (_, key) = s.encode(None, true, None);
    assert!(key);
    assert_eq!(s.enc.screens().unwrap(), [0; SCREENS]);
}

/// A viewer that joins, or loses a frame, asks for a keyframe while the stream predicts from
/// a stored screen: the keyframe empties the table, so no later frame references a slot the
/// viewer never received, and a return to an old page is coded as a new one.
#[test]
fn after_a_requested_keyframe_no_frame_references_a_screen_from_before_it() {
    let mut s = Stream::new(4000);
    s.enc.stored_screens(true);
    s.page(1);
    let first = s.page(2);
    s.pic.texture(1 << 29, 0, 0, W, H, 0);
    let back = s.frame(None);
    assert!(back * 4 < first, "back {back}, page {first}");
    assert!(s.encode(None, true, None).1);
    s.pic.texture(2 << 29, 0, 0, W, H, 0);
    let again = s.frame(None);
    let psnr = s.psnr();
    eprintln!("page 2 again after the keyframe: {again} bytes, {psnr:.2} dB");
    assert!(again > back * 4, "{again} bytes, as if page 2 were still stored");
    assert!(psnr > 30.0, "{psnr:.2} dB");
}

/// A hash that matches the wrong screen: the frame predicts from another page, and the
/// residual still codes this one.
#[test]
fn a_wrong_match_decodes_to_the_right_picture() {
    let mut s = Stream::new(4000);
    s.enc.stored_screens(true);
    s.page(1);
    s.page(2);
    let page2 = s.pic.clone();
    s.page(3);
    s.enc.force_wrong_match();
    s.pic.texture(1 << 29, 0, 0, W, H, 0);
    let n = s.frame(None);
    let recon = s.enc.reconstruction().unwrap();
    let right = luma_psnr(&recon, W, &s.pic);
    let wrong = luma_psnr(&recon, W, &page2);
    eprintln!("wrong match: {n} bytes, {right:.2} dB against the source, {wrong:.2} against the page predicted from");
    assert!(right > 30.0, "{right:.2} dB");
    assert!(right > wrong + 10.0, "{right:.2} vs {wrong:.2}");
}

#[test]
fn the_hash_runs_only_when_most_of_the_screen_changed() {
    let mbs = W.div_ceil(16) * H.div_ceil(16);
    let mut s = Stream::new(4000);
    s.enc.stored_screens(true);
    s.frame(None);
    let before = s.enc.hashed();
    let mut map = vec![1u8; mbs];
    map[..mbs / 2].fill(0);
    s.pic.texture(4 << 29, 0, 0, W, H / 2, 0);
    s.encode(None, false, Some(&mut map));
    assert_eq!(s.enc.hashed(), before, "half the screen changed");
    let mut map = vec![0u8; mbs];
    map[..mbs / 4].fill(1);
    s.pic.texture(5 << 29, 0, 0, W, H, 0);
    s.encode(None, false, Some(&mut map));
    assert_eq!(s.enc.hashed(), before + 1, "three quarters changed");
    s.enc.stored_screens(false);
    s.pic.texture(6 << 29, 0, 0, W, H, 0);
    s.frame(None);
    assert_eq!(s.enc.hashed(), before + 1, "stored screens off");
}

#[test]
fn the_hash_tells_pages_apart() {
    let a = Picture::new(W, H);
    let mut b = a.clone();
    assert_eq!(screen_hash(&a.view()), screen_hash(&b.view()));
    b.texture(1 << 29, 0, 0, W, H, 0);
    assert_ne!(screen_hash(&a.view()), screen_hash(&b.view()));
    assert_ne!(screen_hash(&a.view()), 0);
}

/// `cargo test --release -- --ignored stored_screen_hash_cost`: the hash's cost at 1080p.
#[test]
#[ignore]
fn stored_screen_hash_cost_at_1080p() {
    let mut p = Picture::new(1920, 1080);
    let mut us: Vec<f64> = (0..2000)
        .map(|f| {
            p.texture((f as u32 % 7) << 29, 0, 0, 1920, 1080, f % 64);
            let t = std::time::Instant::now();
            std::hint::black_box(screen_hash(&p.view()));
            t.elapsed().as_secs_f64() * 1e6
        })
        .collect();
    us.sort_by(f64::total_cmp);
    println!(
        "stored-screen hash 1080p: p50 {:.0} us, p95 {:.0} us, max {:.0} us (n={})",
        us[us.len() / 2],
        us[us.len() * 95 / 100],
        us[us.len() - 1],
        us.len()
    );
}
