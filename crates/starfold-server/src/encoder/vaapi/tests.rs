use super::bits::{leb128_fixed, nal, Bits};
use super::driver::Driver;
use super::ffi::{self, VaId};
use super::session::{choose, Rc, RcMode};
use super::*;
use crate::encoder::hw::{sample, sample_config, write_nv12};
use crate::encoder::{h264 as h264_nal, AccessUnit, EncodeOptions, FrameView, RateConfig};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

struct Reader {
    d: Vec<u8>,
    pos: usize,
}

impl Reader {
    /// Reads an Annex-B NAL unit's RBSP after `skip` header bytes.
    fn nal(n: &[u8], skip: usize) -> Self {
        let mut d = Vec::new();
        let mut zeros = 0;
        for &b in &n[skip..] {
            if zeros >= 2 && b == 3 {
                zeros = 0;
                continue;
            }
            zeros = if b == 0 { zeros + 1 } else { 0 };
            d.push(b);
        }
        Reader { d, pos: 0 }
    }

    fn u(&mut self, n: u32) -> u64 {
        let mut v = 0;
        for _ in 0..n {
            let b = (self.d[self.pos >> 3] >> (7 - (self.pos & 7))) & 1;
            v = v << 1 | b as u64;
            self.pos += 1;
        }
        v
    }

    fn ue(&mut self) -> u32 {
        let mut z = 0;
        while self.u(1) == 0 {
            z += 1;
        }
        ((1u64 << z) - 1 + self.u(z)) as u32
    }

    fn se(&mut self) -> i32 {
        let k = self.ue() as i64;
        (if k % 2 == 1 { (k + 1) / 2 } else { -k / 2 }) as i32
    }

    /// The RBSP ends here: a stop bit, then zeros.
    fn at_end(&mut self) -> bool {
        self.u(1) == 1 && (self.pos..self.d.len() * 8).all(|_| self.u(1) == 0)
    }
}

#[test]
fn exp_golomb_and_emulation_prevention() {
    let mut b = Bits::new();
    for v in [0, 1, 2, 3, 7, 254, 65535] {
        b.ue(v);
    }
    for v in [0, 1, -1, 26, -26] {
        b.se(v);
    }
    b.trailing();
    let mut r = Reader::nal(b.bytes(), 0);
    for v in [0, 1, 2, 3, 7, 254, 65535] {
        assert_eq!(r.ue(), v);
    }
    for v in [0, 1, -1, 26, -26] {
        assert_eq!(r.se(), v);
    }
    assert!(r.at_end());
    assert_eq!(
        nal(&[0x67], &[0, 0, 0, 0, 0, 1, 0, 0, 3]),
        vec![0, 0, 0, 1, 0x67, 0, 0, 3, 0, 0, 3, 0, 1, 0, 0, 3, 3]
    );
    assert_eq!(leb128_fixed(5, 4), vec![0x85, 0x80, 0x80, 0x00]);
    assert_eq!(leb128_fixed(300, 2), vec![0xac, 0x02]);
}

type Config = (i32, i32, Vec<(i32, u32)>);

#[derive(Default)]
struct Log {
    buffers: HashMap<VaId, (i32, Vec<u8>)>,
    frames: Vec<Vec<(i32, Vec<u8>)>>,
    surfaces: Vec<VaId>,
    encoded: Vec<VaId>,
    config: Vec<Config>,
    destroyed: bool,
}

/// A driver that keeps what it's given and codes each picture as its packed headers plus
/// `slice(idr)`.
struct Fake {
    log: Arc<Mutex<Log>>,
    profiles: Vec<(i32, Vec<i32>)>,
    packed: Option<u32>,
    rc: u32,
    extra: Vec<Option<u32>>,
    slice: fn(bool) -> Vec<u8>,
    fail_encode: Option<usize>,
    next: VaId,
    out: Vec<u8>,
}

impl Fake {
    fn new(profile: i32, slice: fn(bool) -> Vec<u8>) -> (Fake, Arc<Mutex<Log>>) {
        let log = Arc::new(Mutex::new(Log::default()));
        let f = Fake {
            log: log.clone(),
            profiles: vec![(profile, vec![ffi::ENTRYPOINT_ENC_SLICE_LP])],
            packed: Some(ffi::PACKED_SEQUENCE | ffi::PACKED_PICTURE | ffi::PACKED_SLICE),
            rc: ffi::RC_CBR | ffi::RC_VBR | ffi::RC_CQP,
            extra: Vec::new(),
            slice,
            fail_encode: None,
            next: 100,
            out: Vec::new(),
        };
        (f, log)
    }

    fn id(&mut self) -> VaId {
        self.next += 1;
        self.next
    }
}

fn err(s: &str) -> EncodeError {
    EncodeError(s.into())
}

impl Driver for Fake {
    fn entrypoints(&mut self, profile: i32) -> Result<Vec<i32>, EncodeError> {
        Ok(self
            .profiles
            .iter()
            .find(|p| p.0 == profile)
            .map(|p| p.1.clone())
            .unwrap_or_default())
    }

    fn attribs(&mut self, _: i32, _: i32, kinds: &[i32]) -> Result<Vec<Option<u32>>, EncodeError> {
        Ok(kinds
            .iter()
            .enumerate()
            .map(|(i, &k)| match k {
                ffi::ATTRIB_RT_FORMAT => Some(ffi::RT_FORMAT_YUV420),
                ffi::ATTRIB_RATE_CONTROL => Some(self.rc),
                ffi::ATTRIB_PACKED_HEADERS => self.packed,
                ffi::ATTRIB_MAX_REF_FRAMES => Some(1),
                ffi::ATTRIB_MAX_WIDTH | ffi::ATTRIB_MAX_HEIGHT => Some(4096),
                _ => self.extra.get(i - 6).copied().flatten(),
            })
            .collect())
    }

    fn create_config(&mut self, p: i32, e: i32, a: &[(i32, u32)]) -> Result<VaId, EncodeError> {
        self.log.lock().unwrap().config.push((p, e, a.to_vec()));
        Ok(self.id())
    }

    fn create_surfaces(&mut self, _: u32, _: u32, n: usize) -> Result<Vec<VaId>, EncodeError> {
        let s: Vec<VaId> = (0..n).map(|_| self.id()).collect();
        self.log.lock().unwrap().surfaces = s.clone();
        Ok(s)
    }

    fn create_context(&mut self, _: VaId, _: u32, _: u32, _: &[VaId]) -> Result<VaId, EncodeError> {
        Ok(self.id())
    }

    fn create_buffer(&mut self, _: VaId, kind: i32, data: &[u8]) -> Result<VaId, EncodeError> {
        let id = self.id();
        self.log
            .lock()
            .unwrap()
            .buffers
            .insert(id, (kind, data.to_vec()));
        Ok(id)
    }

    fn create_coded(&mut self, _: VaId, _: usize) -> Result<VaId, EncodeError> {
        Ok(self.id())
    }

    fn upload(&mut self, _: VaId, size: (u32, u32), f: &FrameView<'_>) -> Result<(), EncodeError> {
        let mut dst = vec![0u8; size.0 as usize * size.1 as usize * 3 / 2];
        let s = size.0 as usize;
        write_nv12(&mut dst, [s, s], [0, s * size.1 as usize], size, f)
    }

    fn encode(&mut self, _: VaId, surface: VaId, ids: &[VaId]) -> Result<(), EncodeError> {
        let mut log = self.log.lock().unwrap();
        if self.fail_encode == Some(log.frames.len()) {
            self.fail_encode = None;
            log.frames.push(Vec::new());
            return Err(err("GPU hang"));
        }
        let bufs: Vec<(i32, Vec<u8>)> = ids.iter().map(|i| log.buffers[i].clone()).collect();
        let idr = bufs.iter().any(|b| b.0 == ffi::BUF_SEQUENCE);
        self.out = bufs
            .iter()
            .filter(|b| b.0 == ffi::BUF_PACKED_DATA)
            .flat_map(|b| b.1.clone())
            .collect();
        self.out.extend((self.slice)(idr));
        log.frames.push(bufs);
        log.encoded.push(surface);
        Ok(())
    }

    fn coded(&mut self, _: VaId) -> Result<Vec<u8>, EncodeError> {
        Ok(std::mem::take(&mut self.out))
    }

    fn destroy_buffer(&mut self, id: VaId) {
        self.log.lock().unwrap().buffers.remove(&id);
    }

    fn destroy(&mut self, _: VaId, _: VaId, _: &[VaId]) {
        self.log.lock().unwrap().destroyed = true;
    }
}

fn h264_slice(idr: bool) -> Vec<u8> {
    vec![0, 0, 0, 1, if idr { 0x65 } else { 0x41 }, 0x88, 0x80]
}

fn hevc_slice(_: bool) -> Vec<u8> {
    Vec::new()
}

fn av1_tile_group(_: bool) -> Vec<u8> {
    vec![0x22, 0x01, 0x00]
}

fn cfg(codec: Codec, w: u32, h: u32) -> EncoderConfig {
    sample_config(Backend::Vaapi, codec, w, h, 30)
}

fn grey(w: u32, h: u32) -> (Vec<u8>, Vec<u8>) {
    (
        vec![128; (w * h) as usize],
        vec![128; (w.div_ceil(2) * h.div_ceil(2)) as usize],
    )
}

fn encode(
    enc: &mut dyn VideoEncoder,
    w: u32,
    h: u32,
    key: bool,
) -> Result<Vec<AccessUnit>, EncodeError> {
    let (y, uv) = grey(w, h);
    let f = FrameView {
        width: w,
        height: h,
        y: &y,
        u: &uv,
        v: &uv,
        stride_y: w as usize,
        stride_uv: w.div_ceil(2) as usize,
    };
    enc.encode(
        &f,
        EncodeOptions {
            force_keyframe: key,
            ..Default::default()
        },
    )
}

fn find(frame: &[(i32, Vec<u8>)], kind: i32) -> Vec<Vec<u8>> {
    frame
        .iter()
        .filter(|b| b.0 == kind)
        .map(|b| b.1.clone())
        .collect()
}

#[test]
fn h264_idr_then_p_with_reference_swap_and_idr_on_request() {
    let (fake, log) = Fake::new(ffi::PROFILE_H264_HIGH, h264_slice);
    let mut enc = session(Box::new(fake), &cfg(Codec::H264, 1920, 1080)).unwrap();
    let mut kinds = Vec::new();
    for key in [false, false, false, true, false] {
        let aus = encode(enc.as_mut(), 1920, 1080, key).unwrap();
        assert_eq!(aus.len(), 1);
        kinds.push((aus[0].keyframe, aus[0].config.clone()));
    }
    assert_eq!(
        kinds.iter().map(|k| k.0).collect::<Vec<_>>(),
        vec![true, false, false, true, false]
    );
    let first = kinds[0].1.as_ref().expect("config on the first IDR");
    assert_eq!(first.codec_string, "avc1.640028");
    assert_eq!(first.coded, (1920, 1080));
    assert!(kinds[3].1.is_none(), "same SPS, no new config");
    let log = log.lock().unwrap();
    let s = &log.surfaces;
    let mut seen = Vec::new();
    for (i, f) in log.frames.iter().enumerate() {
        let pic: ffi::VAEncPictureParameterBufferH264 = ffi::read(&find(f, ffi::BUF_PICTURE)[0]);
        let sl: ffi::VAEncSliceParameterBufferH264 = ffi::read(&find(f, ffi::BUF_SLICE)[0]);
        let idr = i == 0 || i == 3;
        assert_eq!(pic.pic_fields & 1 == 1, idr, "frame {i}");
        assert_eq!(find(f, ffi::BUF_SEQUENCE).len(), idr as usize);
        assert_eq!(find(f, ffi::BUF_PACKED_DATA).len(), if idr { 2 } else { 0 });
        assert_eq!(pic.curr_pic.picture_id, s[1 + i % 2], "recon alternates");
        let n = if i < 3 { i } else { i - 3 };
        assert_eq!(pic.frame_num as usize, n);
        if idr {
            assert_eq!(pic.reference_frames[0].picture_id, ffi::INVALID_ID);
            assert_eq!(sl.slice_type, 2);
            assert_eq!(sl.idr_pic_id, if i == 0 { 0 } else { 1 });
        } else {
            assert_eq!(pic.reference_frames[0].picture_id, s[1 + (i - 1) % 2]);
            assert_eq!(sl.ref_pic_list0[0].picture_id, s[1 + (i - 1) % 2]);
            assert_eq!(sl.ref_pic_list1[0].picture_id, ffi::INVALID_ID);
            assert_eq!(sl.slice_type, 0);
        }
        assert_eq!(sl.num_macroblocks, 120 * 68);
        seen.push(pic.curr_pic.picture_id);
    }
    assert!(log.encoded.iter().all(|&e| e == s[0]), "input surface");
    assert!(log.buffers.is_empty(), "parameter buffers destroyed");
    let (_, _, attribs) = &log.config[0];
    assert!(attribs.contains(&(ffi::ATTRIB_RATE_CONTROL, ffi::RC_CBR)));
    assert!(attribs.contains(&(
        ffi::ATTRIB_PACKED_HEADERS,
        ffi::PACKED_SEQUENCE | ffi::PACKED_PICTURE
    )));
    drop(log);
}

#[test]
fn h264_sps_reads_back() {
    let (fake, log) = Fake::new(ffi::PROFILE_H264_HIGH, h264_slice);
    let mut enc = session(Box::new(fake), &cfg(Codec::H264, 1366, 768)).unwrap();
    let au = encode(enc.as_mut(), 1366, 768, true).unwrap().remove(0);
    let nals: Vec<&[u8]> = h264_nal::nal_units(&au.data).collect();
    assert_eq!(
        nals.iter()
            .map(|n| h264_nal::nal_type(n))
            .collect::<Vec<_>>(),
        vec![7, 8, 5]
    );
    let mut r = Reader::nal(nals[0], 1);
    assert_eq!((r.u(8), r.u(8), r.u(8)), (100, 0, 32));
    assert_eq!(r.ue(), 0);
    assert_eq!((r.ue(), r.ue(), r.ue(), r.u(1), r.u(1)), (1, 0, 0, 0, 0));
    assert_eq!((r.ue(), r.ue(), r.ue(), r.u(1)), (4, 2, 1, 0));
    assert_eq!((r.ue() + 1, r.ue() + 1), (86, 48));
    assert_eq!((r.u(1), r.u(1), r.u(1)), (1, 1, 1));
    assert_eq!(
        (r.ue(), r.ue(), r.ue(), r.ue()),
        (0, 5, 0, 0),
        "crop 1376 to 1366"
    );
    assert_eq!((r.u(1), r.u(1), r.u(1), r.u(1)), (1, 0, 0, 1));
    assert_eq!((r.u(3), r.u(1), r.u(1)), (5, 0, 1));
    assert_eq!((r.u(8), r.u(8), r.u(8)), (1, 13, 6));
    for _ in 0..5 {
        assert_eq!(r.u(1), 0);
    }
    assert_eq!((r.u(1), r.u(1)), (1, 1));
    assert_eq!(
        (r.ue(), r.ue(), r.ue(), r.ue(), r.ue(), r.ue()),
        (0, 0, 15, 15, 0, 1),
        "no reordering"
    );
    assert!(r.at_end());
    let mut r = Reader::nal(nals[1], 1);
    assert_eq!((r.ue(), r.ue(), r.u(1), r.u(1)), (0, 0, 1, 0));
    assert_eq!((r.ue(), r.ue(), r.ue(), r.u(1), r.u(2)), (0, 0, 0, 0, 0));
    assert_eq!((r.se(), r.se(), r.se()), (-2, 0, 0));
    assert_eq!((r.u(1), r.u(1), r.u(1)), (1, 0, 0));
    assert_eq!((r.u(1), r.u(1), r.se()), (1, 0, 0));
    assert!(r.at_end());
    let seq: ffi::VAEncSequenceParameterBufferH264 =
        ffi::read(&find(&log.lock().unwrap().frames[0], ffi::BUF_SEQUENCE)[0]);
    assert_eq!(
        (seq.picture_width_in_mbs, seq.picture_height_in_mbs),
        (86, 48)
    );
    assert_eq!(
        (seq.frame_cropping_flag, seq.frame_crop_right_offset),
        (1, 5)
    );
}

#[test]
fn h264_without_packed_headers_needs_the_drivers_own() {
    let (mut fake, _) = Fake::new(ffi::PROFILE_H264_MAIN, h264_slice);
    fake.packed = None;
    let mut enc = session(Box::new(fake), &cfg(Codec::H264, 640, 480)).unwrap();
    let e = encode(enc.as_mut(), 640, 480, false).err().unwrap();
    assert!(e.0.contains("without a picture"), "{e}");
}

#[test]
fn a_failed_encode_restarts_with_an_idr() {
    let (mut fake, log) = Fake::new(ffi::PROFILE_H264_HIGH, h264_slice);
    fake.fail_encode = Some(1);
    let mut enc = session(Box::new(fake), &cfg(Codec::H264, 320, 240)).unwrap();
    assert!(encode(enc.as_mut(), 320, 240, false).unwrap()[0].keyframe);
    assert_eq!(
        encode(enc.as_mut(), 320, 240, false).err().unwrap().0,
        "GPU hang"
    );
    assert!(encode(enc.as_mut(), 320, 240, false).unwrap()[0].keyframe);
    assert!(!encode(enc.as_mut(), 320, 240, false).unwrap()[0].keyframe);
    assert!(log.lock().unwrap().buffers.is_empty());
    assert!(encode(enc.as_mut(), 320, 200, false).is_err(), "wrong size");
    drop(enc);
    assert!(log.lock().unwrap().destroyed);
}

#[test]
fn rate_control_follows_rate_config() {
    let capped = RateConfig::new(24.0, 8000, 30);
    let uncapped = RateConfig::new(30.0, 0, 30);
    let all = ffi::RC_CBR | ffi::RC_VBR | ffi::RC_CQP;
    assert_eq!(Rc::mode(&capped, all), Ok(RcMode::Cbr));
    assert_eq!(Rc::mode(&capped, ffi::RC_VBR), Ok(RcMode::Vbr));
    assert_eq!(Rc::mode(&uncapped, all), Ok(RcMode::Cqp));
    assert_eq!(Rc::mode(&uncapped, ffi::RC_CBR), Ok(RcMode::Cbr));
    assert!(Rc::mode(&capped, 0).is_err());
    let rc = Rc::new(RcMode::Cbr, &capped, None);
    assert_eq!((rc.bps, rc.buffer_bits, rc.qp), (8_000_000, 332_000, 24));
    let misc = rc.misc(true);
    assert_eq!(misc.len(), 3);
    let r: ffi::Misc<ffi::VAEncMiscParameterRateControl> = ffi::read(&misc[1].data);
    assert_eq!(
        (r.kind, r.data.bits_per_second, r.data.rc_flags),
        (1, 8_000_000, 3)
    );
    assert_eq!(Rc::new(RcMode::Cqp, &uncapped, None).misc(false).len(), 1);

    let (fake, log) = Fake::new(ffi::PROFILE_H264_HIGH, h264_slice);
    let mut enc = session(Box::new(fake), &cfg(Codec::H264, 320, 240)).unwrap();
    encode(enc.as_mut(), 320, 240, false).unwrap();
    encode(enc.as_mut(), 320, 240, false).unwrap();
    enc.set_rate(RateConfig::new(24.0, 2000, 30)).unwrap();
    encode(enc.as_mut(), 320, 240, false).unwrap();
    encode(enc.as_mut(), 320, 240, false).unwrap();
    let log = log.lock().unwrap();
    let misc: Vec<usize> = log
        .frames
        .iter()
        .map(|f| find(f, ffi::BUF_MISC).len())
        .collect();
    assert_eq!(misc, vec![3, 0, 3, 0]);
    let r: ffi::Misc<ffi::VAEncMiscParameterRateControl> =
        ffi::read(&find(&log.frames[2], ffi::BUF_MISC)[1]);
    assert_eq!(
        (r.data.bits_per_second, r.data.rc_flags & 1),
        (2_000_000, 1)
    );
}

#[test]
fn hevc_headers_read_back_and_carry_every_idr() {
    let (mut fake, log) = Fake::new(ffi::PROFILE_HEVC_MAIN, hevc_slice);
    fake.extra = vec![None, None];
    let mut enc = session(Box::new(fake), &cfg(Codec::Hevc, 1920, 1080)).unwrap();
    let mut out = Vec::new();
    for key in [false, false, true] {
        out.push(encode(enc.as_mut(), 1920, 1080, key).unwrap().remove(0));
    }
    assert_eq!(
        out.iter().map(|a| a.keyframe).collect::<Vec<_>>(),
        vec![true, false, true]
    );
    assert_eq!(
        out[0].config.as_ref().unwrap().codec_string,
        "hvc1.1.6.L120.90"
    );
    let types = |d: &[u8]| -> Vec<u8> {
        h264_nal::nal_units(d)
            .map(crate::encoder::hevc::nal_type)
            .collect()
    };
    assert_eq!(types(&out[0].data), vec![32, 33, 34, 19]);
    assert_eq!(types(&out[1].data), vec![1]);
    assert_eq!(types(&out[2].data), vec![32, 33, 34, 19]);
    let sps = h264_nal::nal_units(&out[0].data).nth(1).unwrap().to_vec();
    let mut r = Reader::nal(&sps, 2);
    assert_eq!((r.u(4), r.u(3), r.u(1)), (0, 0, 1));
    r.u(96);
    assert_eq!((r.ue(), r.ue(), r.ue(), r.ue()), (0, 1, 1920, 1080));
    assert_eq!(r.u(1), 0, "no conformance window at 1080");
    assert_eq!((r.ue(), r.ue(), r.ue()), (0, 0, 4));
    assert_eq!((r.u(1), r.ue(), r.ue(), r.ue()), (0, 1, 0, 0));
    assert_eq!(
        (r.ue(), r.ue(), r.ue(), r.ue(), r.ue(), r.ue()),
        (0, 3, 0, 3, 2, 2)
    );
    assert_eq!(
        (r.u(1), r.u(1), r.u(1), r.u(1)),
        (0, 0, 0, 0),
        "LP: no AMP/SAO"
    );
    assert_eq!((r.ue(), r.u(1), r.u(1), r.u(1), r.u(1)), (0, 0, 1, 0, 1));
    let p = h264_nal::nal_units(&out[1].data).next().unwrap().to_vec();
    let mut r = Reader::nal(&p, 2);
    assert_eq!((r.u(1), r.ue(), r.ue()), (1, 0, 1));
    assert_eq!((r.u(8), r.u(1)), (1, 0), "POC LSB 1, explicit RPS");
    assert_eq!((r.ue(), r.ue(), r.ue(), r.u(1)), (1, 0, 0, 1));
    assert_eq!(r.u(1), 1, "temporal MVP");
    assert_eq!((r.u(1), r.ue(), r.se()), (0, 0, 0));
    assert!(r.at_end());
    let log = log.lock().unwrap();
    let pic: ffi::VAEncPictureParameterBufferHEVC =
        ffi::read(&find(&log.frames[1], ffi::BUF_PICTURE)[0]);
    assert_eq!(pic.reference_frames[0].picture_id, log.surfaces[1]);
    assert_eq!(pic.decoded_curr_pic.picture_id, log.surfaces[2]);
    assert_eq!((pic.nal_unit_type, pic.collocated_ref_pic_index), (1, 0));
    let sl: ffi::VAEncSliceParameterBufferHEVC =
        ffi::read(&find(&log.frames[1], ffi::BUF_SLICE)[0]);
    assert_eq!(sl.num_ctu_in_slice, 30 * 17);
    assert_eq!(sl.ref_pic_list0[0].pic_order_cnt, 0);
}

#[test]
fn hevc_without_every_packed_header_packs_none() {
    assert_eq!(
        hevc::Hevc::packed(Some(ffi::PACKED_SEQUENCE | ffi::PACKED_PICTURE)),
        Some(0)
    );
    let t = hevc::Tools::new(Some(2 << 4 | 1 << 6), Some(1 | 1 << 4), false, true);
    assert!(t.amp && t.sao && !t.tmvp && !t.cu_qp_delta);
    assert_eq!((t.log2_ctb, t.log2_min_cb), (4, 4));
    let (mut fake, _) = Fake::new(ffi::PROFILE_HEVC_MAIN, |idr| {
        let mut v = Vec::new();
        if idr {
            v.extend(nal(
                &[33 << 1, 1],
                &[1, 1, 0x60, 0, 0, 0, 0x90, 0, 0, 0, 0, 0, 93],
            ));
        }
        v.extend(nal(&[if idr { 19 << 1 } else { 2 }, 1], &[0x80]));
        v
    });
    fake.packed = Some(ffi::PACKED_SEQUENCE);
    fake.extra = vec![None, None];
    let mut enc = session(Box::new(fake), &cfg(Codec::Hevc, 640, 480)).unwrap();
    let au = encode(enc.as_mut(), 640, 480, false).unwrap().remove(0);
    assert!(au.keyframe && au.config.is_some());
}

#[test]
fn av1_headers_read_back() {
    let (mut fake, log) = Fake::new(ffi::PROFILE_AV1_0, av1_tile_group);
    fake.extra = vec![Some(0x1f), Some(3 << 2 | 4 << 4)];
    let mut enc = session(Box::new(fake), &cfg(Codec::Av1, 1920, 1080)).unwrap();
    let k = encode(enc.as_mut(), 1920, 1080, false).unwrap().remove(0);
    let p = encode(enc.as_mut(), 1920, 1080, false).unwrap().remove(0);
    assert!(k.keyframe && !p.keyframe);
    assert_eq!(&k.data[..2], &[0x12, 0x00], "temporal delimiter");
    assert_eq!(
        k.config.as_ref().unwrap().codec_string,
        "av01.0.08M.08.0.110.01.13.06.0"
    );
    assert_eq!(
        k.config.as_ref().unwrap().format,
        crate::encoder::BitstreamFormat::Av1LowOverhead
    );
    assert!(p.config.is_none());
    let log = log.lock().unwrap();
    for (f, key) in [(&log.frames[0], true), (&log.frames[1], false)] {
        let pic: ffi::VAEncPictureParameterBufferAV1 = ffi::read(&find(f, ffi::BUF_PICTURE)[0]);
        let headers = find(f, ffi::BUF_PACKED_DATA);
        let fh = headers.last().unwrap();
        assert_eq!(fh[0], 0x1a);
        let size: Vec<u8> = fh[1..5].to_vec();
        let len = (size[0] & 0x7f) as usize
            | ((size[1] & 0x7f) as usize) << 7
            | ((size[2] & 0x7f) as usize) << 14
            | (size[3] as usize) << 21;
        assert_eq!(len, fh.len() - 5);
        let bits = |off: u32, n: u32| -> u64 {
            (off..off + n).fold(0, |v, i| {
                v << 1 | ((fh[(i >> 3) as usize] >> (7 - (i & 7))) & 1) as u64
            })
        };
        assert_eq!(bits(pic.bit_offset_qindex, 8), pic.base_qindex as u64);
        assert_eq!(bits(pic.bit_offset_segmentation, 1), 0);
        assert_eq!(
            bits(pic.bit_offset_loopfilter_params, 6),
            pic.filter_level[0] as u64
        );
        assert_eq!(pic.size_in_bits_cdef_params, 16);
        assert_eq!(
            bits(pic.size_in_bits_frame_hdr_obu - 1, 1),
            1,
            "trailing one"
        );
        assert_eq!(pic.byte_offset_frame_hdr_obu_size, 1);
        assert_eq!(headers.len(), if key { 2 } else { 1 });
        assert_eq!(pic.refresh_frame_flags, if key { 0xff } else { 0x01 });
        assert_eq!(pic.picture_flags & 3, !key as u32);
        assert_eq!((pic.tile_cols, pic.tile_rows), (1, 1));
        assert_eq!(pic.width_in_sbs_minus_1[0], 29);
        assert_eq!(pic.interpolation_filter, 4);
        if !key {
            assert_eq!(pic.reference_frames[0], log.surfaces[1]);
            assert_eq!(pic.reconstructed_frame, log.surfaces[2]);
        }
    }
}

#[test]
fn av1_tiles_and_driver_requirements() {
    assert_eq!(av1::Tiles::new(1920, 1080).widths, vec![29]);
    let t = av1::Tiles::new(7680, 4320);
    assert_eq!((t.cols_log2, t.widths), (1, vec![59, 59]));
    let (mut fake, _) = Fake::new(ffi::PROFILE_AV1_0, av1_tile_group);
    fake.packed = Some(ffi::PACKED_SEQUENCE);
    let e = session(Box::new(fake), &cfg(Codec::Av1, 640, 480))
        .err()
        .unwrap();
    assert!(e.0.contains("packed"), "{e}");
}

#[test]
fn choose_prefers_low_power_and_the_best_profile() {
    let (mut fake, _) = Fake::new(ffi::PROFILE_H264_MAIN, h264_slice);
    fake.profiles = vec![
        (
            ffi::PROFILE_H264_MAIN,
            vec![ffi::ENTRYPOINT_ENC_SLICE, ffi::ENTRYPOINT_ENC_SLICE_LP],
        ),
        (ffi::PROFILE_H264_CB, vec![ffi::ENTRYPOINT_ENC_SLICE]),
    ];
    let c = choose(&mut fake, h264::PROFILES, (640, 480), &[]).unwrap();
    assert_eq!(
        (c.profile, c.entrypoint),
        (ffi::PROFILE_H264_MAIN, ffi::ENTRYPOINT_ENC_SLICE_LP)
    );
    assert!(choose(&mut fake, h264::PROFILES, (8192, 480), &[]).is_err());
    fake.profiles.clear();
    assert!(session(Box::new(fake), &cfg(Codec::H264, 640, 480)).is_err());
}

#[test]
fn levels() {
    let m = 4_000_000;
    assert_eq!(h264::level(120, 68, 30, m, true), Some((40, 25_000_000)));
    assert_eq!(h264::level(120, 68, 60, m, true).unwrap().0, 42);
    assert_eq!(h264::level(240, 135, 30, m, true).unwrap().0, 51);
    assert_eq!(
        h264::level(240, 8, 30, m, true).unwrap().0,
        40,
        "a 3840x128 stripe is too wide for 3.1"
    );
    assert_eq!(
        h264::level(20, 15, 30, 12_000_000, false).unwrap().0,
        31,
        "bitrate"
    );
    assert_eq!(h264::level(20, 15, 30, 2_000_000_000, true), None);
    assert_eq!(hevc::level(1920, 1080, 60, m).unwrap().0, 123);
    assert_eq!(
        hevc::level(3840, 128, 30, m).unwrap().0,
        120,
        "too wide for 3.1"
    );
    assert_eq!(
        hevc::level(320, 240, 30, m),
        Some((90, 6_000_000)),
        "4 Mbit/s is past 2"
    );
    assert_eq!(hevc::level(128, 17_000, 30, m), None);
    assert_eq!(av1::level(3840, 2160, 30, m).unwrap().0, 12);
    assert_eq!(
        av1::level(3000, 64, 30, m).unwrap().0,
        4,
        "too wide for 2.1"
    );
    assert_eq!(av1::level(128, 9000, 30, m), None, "taller than any level");
    let (fake, _) = Fake::new(ffi::PROFILE_AV1_0, av1_tile_group);
    let e = session(
        Box::new(fake),
        &sample_config(Backend::Vaapi, Codec::Av1, 4096, 4096, 300),
    )
    .err()
    .unwrap();
    assert!(e.0.contains("no AV1 level"), "{e}");
}

#[test]
fn a_higher_rate_raises_the_level_at_an_idr() {
    let (fake, log) = Fake::new(ffi::PROFILE_H264_HIGH, h264_slice);
    let mut enc = session(Box::new(fake), &cfg(Codec::H264, 320, 240)).unwrap();
    let first = encode(enc.as_mut(), 320, 240, false).unwrap().remove(0);
    assert_eq!(first.config.unwrap().codec_string, "avc1.64001e");
    assert!(!encode(enc.as_mut(), 320, 240, false).unwrap()[0].keyframe);
    enc.set_rate(RateConfig::new(24.0, 20_000, 30)).unwrap();
    let au = encode(enc.as_mut(), 320, 240, false).unwrap().remove(0);
    assert!(au.keyframe, "a new level takes an IDR");
    assert_eq!(au.config.unwrap().codec_string, "avc1.640020");
    enc.set_rate(RateConfig::new(24.0, 2_000_000, 30)).unwrap();
    assert!(encode(enc.as_mut(), 320, 240, false).unwrap()[0].keyframe);
    let log = log.lock().unwrap();
    let seq = |i: usize| -> ffi::VAEncSequenceParameterBufferH264 {
        ffi::read(&find(&log.frames[i], ffi::BUF_SEQUENCE)[0])
    };
    assert_eq!((seq(2).level_idc, seq(2).bits_per_second), (32, 20_000_000));
    assert_eq!(
        (seq(3).level_idc, seq(3).bits_per_second),
        (62, 1_000_000_000),
        "past every level: the highest, at its cap"
    );
}

#[test]
fn h264_packs_both_parameter_sets_or_neither() {
    let both = ffi::PACKED_SEQUENCE | ffi::PACKED_PICTURE;
    assert_eq!(
        h264::H264::packed(Some(both | ffi::PACKED_SLICE)),
        Some(both)
    );
    assert_eq!(h264::H264::packed(Some(ffi::PACKED_SEQUENCE)), Some(0));
    assert_eq!(h264::H264::packed(Some(ffi::PACKED_PICTURE)), Some(0));
    assert_eq!(h264::H264::packed(None), None);
    let (mut fake, log) = Fake::new(ffi::PROFILE_H264_HIGH, h264_slice);
    fake.packed = Some(ffi::PACKED_SEQUENCE);
    let mut enc = session(Box::new(fake), &cfg(Codec::H264, 320, 240)).unwrap();
    assert!(
        encode(enc.as_mut(), 320, 240, false).is_err(),
        "no SPS from this driver"
    );
    let log = log.lock().unwrap();
    assert!(find(&log.frames[0], ffi::BUF_PACKED_DATA).is_empty());
    assert!(log.config[0].2.contains(&(ffi::ATTRIB_PACKED_HEADERS, 0)));
}

#[test]
fn child_sessions_need_an_absolute_node() {
    assert!(child_session("h264:64x64@30,0,0,30,24,dev")
        .unwrap_err()
        .contains("bad session"));
}

/// The parameter structs against libva's own headers, where they're installed.
#[test]
fn layouts_match_libva() {
    let inc = std::path::Path::new("/usr/include/va/va_enc_av1.h");
    if !inc.exists() {
        eprintln!("skipped: no libva headers (apt install libva-dev)");
        return;
    }
    use std::mem::{offset_of, size_of};
    macro_rules! sizes {
        ($($t:ident),*) => { vec![$((stringify!($t), size_of::<ffi::$t>())),*] };
    }
    let mut want = sizes!(
        VAConfigAttrib,
        VASurfaceAttrib,
        VAImageFormat,
        VAImage,
        VACodedBufferSegment,
        VAEncPackedHeaderParameterBuffer,
        VAEncMiscParameterRateControl,
        VAEncMiscParameterFrameRate,
        VAEncMiscParameterHRD,
        VAPictureH264,
        VAEncSequenceParameterBufferH264,
        VAEncPictureParameterBufferH264,
        VAEncSliceParameterBufferH264,
        VAPictureHEVC,
        VAEncSequenceParameterBufferHEVC,
        VAEncPictureParameterBufferHEVC,
        VAEncSliceParameterBufferHEVC,
        VAEncSequenceParameterBufferAV1,
        VAEncSegParamAV1,
        VAEncWarpedMotionParamsAV1,
        VAEncPictureParameterBufferAV1,
        VAEncTileGroupBufferAV1
    );
    let offsets: Vec<(&str, &str, usize)> = vec![
        (
            "VAEncSequenceParameterBufferH264",
            "time_scale",
            offset_of!(ffi::VAEncSequenceParameterBufferH264, time_scale),
        ),
        (
            "VAEncPictureParameterBufferH264",
            "pic_fields",
            offset_of!(ffi::VAEncPictureParameterBufferH264, pic_fields),
        ),
        (
            "VAEncSliceParameterBufferH264",
            "cabac_init_idc",
            offset_of!(ffi::VAEncSliceParameterBufferH264, cabac_init_idc),
        ),
        (
            "VAEncSequenceParameterBufferHEVC",
            "scc_fields",
            offset_of!(ffi::VAEncSequenceParameterBufferHEVC, scc_fields),
        ),
        (
            "VAEncPictureParameterBufferHEVC",
            "pic_fields",
            offset_of!(ffi::VAEncPictureParameterBufferHEVC, pic_fields),
        ),
        (
            "VAEncSliceParameterBufferHEVC",
            "slice_fields",
            offset_of!(ffi::VAEncSliceParameterBufferHEVC, slice_fields),
        ),
        (
            "VAEncPictureParameterBufferAV1",
            "base_qindex",
            offset_of!(ffi::VAEncPictureParameterBufferAV1, base_qindex),
        ),
        (
            "VAEncPictureParameterBufferAV1",
            "segments",
            offset_of!(ffi::VAEncPictureParameterBufferAV1, segments),
        ),
        (
            "VAEncPictureParameterBufferAV1",
            "wm",
            offset_of!(ffi::VAEncPictureParameterBufferAV1, wm),
        ),
        (
            "VAEncPictureParameterBufferAV1",
            "bit_offset_qindex",
            offset_of!(ffi::VAEncPictureParameterBufferAV1, bit_offset_qindex),
        ),
        (
            "VAEncPictureParameterBufferAV1",
            "tile_group_obu_hdr_info",
            offset_of!(ffi::VAEncPictureParameterBufferAV1, tile_group_obu_hdr_info),
        ),
        ("VAImage", "pitches", offset_of!(ffi::VAImage, pitches)),
        (
            "VACodedBufferSegment",
            "buf",
            offset_of!(ffi::VACodedBufferSegment, buf),
        ),
    ];
    want.push((
        "VAEncMiscParameterBuffer_header",
        offset_of!(ffi::Misc<u32>, data),
    ));
    let dir = std::env::temp_dir().join(format!("va-layout-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut c = String::from(
        "#include <stdio.h>\n#include <stddef.h>\n#include <va/va.h>\n#include <va/va_enc_h264.h>\n#include <va/va_enc_hevc.h>\n#include <va/va_enc_av1.h>\nint main(void){\n",
    );
    for (t, _) in &want {
        if *t == "VAEncMiscParameterBuffer_header" {
            c.push_str("printf(\"%zu\\n\", offsetof(VAEncMiscParameterBuffer, data));\n");
        } else {
            c.push_str(&format!("printf(\"%zu\\n\", sizeof({t}));\n"));
        }
    }
    for (t, f, _) in &offsets {
        c.push_str(&format!("printf(\"%zu\\n\", offsetof({t}, {f}));\n"));
    }
    c.push_str("return 0;}\n");
    std::fs::write(dir.join("l.c"), c).unwrap();
    let cc = std::process::Command::new("cc")
        .arg(dir.join("l.c"))
        .arg("-o")
        .arg(dir.join("l"))
        .status();
    if !cc.is_ok_and(|s| s.success()) {
        panic!("libva headers present but the layout program didn't compile");
    }
    let out = std::process::Command::new(dir.join("l")).output().unwrap();
    let got: Vec<usize> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| l.parse().unwrap())
        .collect();
    let _ = std::fs::remove_dir_all(&dir);
    let names: Vec<String> = want
        .iter()
        .map(|w| w.0.to_string())
        .chain(offsets.iter().map(|o| format!("{}.{}", o.0, o.1)))
        .collect();
    let rust: Vec<usize> = want
        .iter()
        .map(|w| w.1)
        .chain(offsets.iter().map(|o| o.2))
        .collect();
    let bad: Vec<String> = names
        .iter()
        .zip(rust.iter().zip(&got))
        .filter(|(_, (r, c))| r != c)
        .map(|(n, (r, c))| format!("{n}: rust {r}, C {c}"))
        .collect();
    assert_eq!(got.len(), rust.len());
    assert!(bad.is_empty(), "{bad:#?}");
    eprintln!("checked {} layouts against libva", got.len());
}

/// Real hardware: opens every codec on the first render node and encodes a few frames.
#[test]
#[ignore = "needs a VA-API GPU (/dev/dri/renderD*)"]
fn hardware_smoke() {
    if render_nodes().map_or(true, |n| n.is_empty()) {
        eprintln!("skipped: no render node");
        return;
    }
    for codec in [Codec::H264, Codec::Hevc, Codec::Av1] {
        let c = cfg(codec, 1280, 720);
        match open_on(&render_nodes().unwrap()[0], &c) {
            Ok(mut enc) => {
                sample(enc.as_mut(), 1280, 720).unwrap();
                eprintln!("{codec:?}: ok");
            }
            Err(e) => eprintln!("{codec:?}: {e}"),
        }
    }
}
