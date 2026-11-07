//! HEVC Main: VPS, SPS and PPS before every IDR and a packed slice header on every
//! picture, or no packed headers at all when the driver can't take all three (VA-API has
//! no field for the POC LSB length, so a driver-written slice header must go with a
//! driver-written SPS).

use super::bits::{nal, Bits};
use super::ffi::{self, VaId};
use super::session::{Buffer, Choice, Params, Picture, Rc, RcMode};
use crate::encoder::hevc::Units;
use crate::encoder::{AccessUnit, ColorInfo, EncodeError, EncoderConfig};

pub const PROFILES: &[i32] = &[ffi::PROFILE_HEVC_MAIN];
pub const EXTRA: &[i32] = &[ffi::ATTRIB_HEVC_FEATURES, ffi::ATTRIB_HEVC_BLOCK_SIZES];
const LOG2_MAX_POC_LSB: u32 = 8;
const ALL: u32 = ffi::PACKED_SEQUENCE | ffi::PACKED_PICTURE | ffi::PACKED_SLICE;
const NAL_IDR_W_RADL: u8 = 19;
const NAL_TRAIL_R: u8 = 1;

/// `general_level_idc` (30 x the level) for a `w`x`h` picture at `fps`.
pub fn level(w: u64, h: u64, fps: u64) -> u8 {
    let ps = w * h;
    const LEVELS: &[(u8, u64, u64)] = &[
        (60, 122_880, 3_686_400),
        (90, 552_960, 16_588_800),
        (93, 983_040, 33_177_600),
        (120, 2_228_224, 66_846_720),
        (123, 2_228_224, 133_693_440),
        (150, 8_912_896, 267_386_880),
        (153, 8_912_896, 534_773_760),
        (156, 8_912_896, 1_069_547_520),
        (180, 35_651_584, 1_069_547_520),
        (183, 35_651_584, 2_139_095_040),
    ];
    LEVELS
        .iter()
        .find(|l| ps <= l.1 && ps * fps <= l.2 && w * w <= 8 * l.1 && h * h <= 8 * l.1)
        .map_or(186, |l| l.0)
}

/// Coding tools, from what the driver supports or requires.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tools {
    pub log2_ctb: u32,
    pub log2_min_cb: u32,
    pub log2_max_tb: u32,
    pub log2_min_tb: u32,
    pub depth_inter: u32,
    pub depth_intra: u32,
    pub amp: bool,
    pub sao: bool,
    pub tmvp: bool,
    pub strong_intra: bool,
    pub sign_hiding: bool,
    pub transform_skip: bool,
    pub cu_qp_delta: bool,
}

impl Tools {
    pub fn new(features: Option<u32>, blocks: Option<u32>, low_power: bool, brc: bool) -> Tools {
        let f = |shift: u32, default: bool| match features.map(|v| (v >> shift) & 3) {
            Some(0) => false,
            Some(2) => true,
            Some(_) => default,
            None => default,
        };
        let (log2_ctb, log2_min_cb, log2_max_tb, log2_min_tb, depth_inter, depth_intra) =
            match blocks {
                Some(b) => (
                    (b & 3) + 3,
                    ((b >> 4) & 3) + 3,
                    ((b >> 6) & 3) + 2,
                    ((b >> 8) & 3) + 2,
                    (b >> 10) & 3,
                    (b >> 14) & 3,
                ),
                None if low_power => (6, 3, 5, 2, 2, 2),
                None => (5, 3, 5, 2, 2, 2),
            };
        Tools {
            log2_ctb,
            log2_min_cb,
            log2_max_tb,
            log2_min_tb,
            depth_inter,
            depth_intra,
            amp: f(4, !low_power),
            sao: f(6, !low_power),
            tmvp: f(10, true),
            strong_intra: f(12, false),
            sign_hiding: f(16, false),
            transform_skip: f(20, false),
            cu_qp_delta: f(22, brc),
        }
    }
}

pub struct Hevc {
    size: (u32, u32),
    pic: (u32, u32),
    level: u8,
    tools: Tools,
    color: ColorInfo,
    packed: bool,
    pps_qp: u8,
    units: Units,
    announced: bool,
}

impl Hevc {
    pub fn new(choice: &Choice, cfg: &EncoderConfig, rc: &Rc) -> Hevc {
        let tools = Tools::new(
            choice.extra.first().copied().flatten(),
            choice.extra.get(1).copied().flatten(),
            choice.low_power(),
            rc.mode != RcMode::Cqp,
        );
        let cb = 1 << tools.log2_min_cb;
        let pic = (cfg.width.div_ceil(cb) * cb, cfg.height.div_ceil(cb) * cb);
        Hevc {
            size: (cfg.width, cfg.height),
            pic,
            level: level(pic.0 as u64, pic.1 as u64, cfg.fps.max(1) as u64),
            tools,
            color: ColorInfo::SRGB_BT601_LIMITED,
            packed: choice.packed == Some(ALL),
            pps_qp: rc.qp,
            units: Units::new(cfg.width, cfg.height),
            announced: false,
        }
    }

    pub fn packed(driver: Option<u32>) -> Option<u32> {
        driver.map(|p| if p & ALL == ALL { ALL } else { 0 })
    }

    fn ctbs(&self) -> u32 {
        let c = 1 << self.tools.log2_ctb;
        self.pic.0.div_ceil(c) * self.pic.1.div_ceil(c)
    }

    fn ptl(&self, b: &mut Bits) {
        b.put(2, 0);
        b.flag(false);
        b.put(5, 1);
        b.put(32, 0x6000_0000);
        b.flag(true);
        b.flag(false);
        b.flag(false);
        b.flag(true);
        b.put(43, 0);
        b.put(1, 0);
        b.put(8, self.level as u64);
    }

    pub fn vps(&self) -> Vec<u8> {
        let mut b = Bits::new();
        b.put(4, 0);
        b.flag(true);
        b.flag(true);
        b.put(6, 0);
        b.put(3, 0);
        b.flag(true);
        b.put(16, 0xffff);
        self.ptl(&mut b);
        b.flag(false);
        b.ue(1);
        b.ue(0);
        b.ue(0);
        b.put(6, 0);
        b.ue(0);
        b.flag(false);
        b.flag(false);
        b.trailing();
        nal(&[32 << 1, 1], b.bytes())
    }

    pub fn sps(&self) -> Vec<u8> {
        let t = &self.tools;
        let mut b = Bits::new();
        b.put(4, 0);
        b.put(3, 0);
        b.flag(true);
        self.ptl(&mut b);
        b.ue(0);
        b.ue(1);
        b.ue(self.pic.0);
        b.ue(self.pic.1);
        let (cx, cy) = (
            (self.pic.0 - self.size.0) / 2,
            (self.pic.1 - self.size.1) / 2,
        );
        b.flag(cx > 0 || cy > 0);
        if cx > 0 || cy > 0 {
            b.ue(0);
            b.ue(cx);
            b.ue(0);
            b.ue(cy);
        }
        b.ue(0);
        b.ue(0);
        b.ue(LOG2_MAX_POC_LSB - 4);
        b.flag(false);
        b.ue(1);
        b.ue(0);
        b.ue(0);
        b.ue(t.log2_min_cb - 3);
        b.ue(t.log2_ctb - t.log2_min_cb);
        b.ue(t.log2_min_tb - 2);
        b.ue(t.log2_max_tb - t.log2_min_tb);
        b.ue(t.depth_inter);
        b.ue(t.depth_intra);
        b.flag(false);
        b.flag(t.amp);
        b.flag(t.sao);
        b.flag(false);
        b.ue(0);
        b.flag(false);
        b.flag(t.tmvp);
        b.flag(t.strong_intra);
        b.flag(true);
        b.flag(false);
        b.flag(false);
        b.flag(true);
        b.put(3, 5);
        b.flag(self.color.full_range);
        b.flag(true);
        b.put(8, self.color.primaries as u64);
        b.put(8, self.color.transfer as u64);
        b.put(8, self.color.matrix as u64);
        for _ in 0..7 {
            b.flag(false);
        }
        b.flag(false);
        b.trailing();
        nal(&[33 << 1, 1], b.bytes())
    }

    pub fn pps(&self) -> Vec<u8> {
        let t = &self.tools;
        let mut b = Bits::new();
        b.ue(0);
        b.ue(0);
        b.flag(false);
        b.flag(false);
        b.put(3, 0);
        b.flag(t.sign_hiding);
        b.flag(false);
        b.ue(0);
        b.ue(0);
        b.se(self.pps_qp as i32 - 26);
        b.flag(false);
        b.flag(t.transform_skip);
        b.flag(t.cu_qp_delta);
        if t.cu_qp_delta {
            b.ue(0);
        }
        b.se(0);
        b.se(0);
        for _ in 0..10 {
            b.flag(false);
        }
        b.ue(0);
        b.flag(false);
        b.flag(false);
        b.trailing();
        nal(&[34 << 1, 1], b.bytes())
    }

    pub fn slice_header(&self, idr: bool, poc: u64, qp_delta: i32) -> Vec<u8> {
        let t = &self.tools;
        let kind = if idr { NAL_IDR_W_RADL } else { NAL_TRAIL_R };
        let mut b = Bits::new();
        b.flag(true);
        if idr {
            b.flag(false);
        }
        b.ue(0);
        b.ue(if idr { 2 } else { 1 });
        if !idr {
            b.put(LOG2_MAX_POC_LSB, poc % (1 << LOG2_MAX_POC_LSB));
            b.flag(false);
            b.ue(1);
            b.ue(0);
            b.ue(0);
            b.flag(true);
            if t.tmvp {
                b.flag(true);
            }
        }
        if t.sao {
            b.flag(true);
            b.flag(true);
        }
        if !idr {
            b.flag(false);
            b.ue(0);
        }
        b.se(qp_delta);
        b.trailing();
        nal(&[kind << 1, 1], b.bytes())
    }

    fn picture(id: VaId, poc: u64, flags: u32) -> ffi::VAPictureHEVC {
        let mut p: ffi::VAPictureHEVC = ffi::zeroed();
        p.picture_id = id;
        p.pic_order_cnt = (poc % (1 << 31)) as i32;
        p.flags = flags;
        p
    }

    fn invalid() -> ffi::VAPictureHEVC {
        Self::picture(ffi::INVALID_ID, 0, ffi::PICTURE_HEVC_INVALID)
    }
}

impl Params for Hevc {
    fn surface_size(&self) -> (u32, u32) {
        self.pic
    }

    fn buffers(&mut self, p: &Picture, misc: Vec<Buffer>) -> Vec<Buffer> {
        let t = self.tools;
        let mut out = Vec::new();
        if p.idr {
            if p.rc.mode == RcMode::Cqp {
                self.pps_qp = p.rc.qp;
            }
            let s = ffi::param(|s: &mut ffi::VAEncSequenceParameterBufferHEVC| {
                s.general_profile_idc = 1;
                s.general_level_idc = self.level;
                s.intra_period = p.rc.fps.saturating_mul(60);
                s.intra_idr_period = s.intra_period;
                s.ip_period = 1;
                s.bits_per_second = if p.rc.mode == RcMode::Cqp {
                    0
                } else {
                    p.rc.bps
                };
                s.pic_width_in_luma_samples = self.pic.0 as u16;
                s.pic_height_in_luma_samples = self.pic.1 as u16;
                s.seq_fields = 1
                    | (t.strong_intra as u32) << 10
                    | (t.amp as u32) << 11
                    | (t.sao as u32) << 12
                    | (t.tmvp as u32) << 15
                    | 1 << 16;
                s.log2_min_luma_coding_block_size_minus3 = (t.log2_min_cb - 3) as u8;
                s.log2_diff_max_min_luma_coding_block_size = (t.log2_ctb - t.log2_min_cb) as u8;
                s.log2_min_transform_block_size_minus2 = (t.log2_min_tb - 2) as u8;
                s.log2_diff_max_min_transform_block_size = (t.log2_max_tb - t.log2_min_tb) as u8;
                s.max_transform_hierarchy_depth_inter = t.depth_inter as u8;
                s.max_transform_hierarchy_depth_intra = t.depth_intra as u8;
            });
            out.push(Buffer::new(ffi::BUF_SEQUENCE, s));
        }
        out.extend(misc);
        let refs = |r: VaId| Self::picture(r, p.n - 1, 0);
        let pic = ffi::param(|pic: &mut ffi::VAEncPictureParameterBufferHEVC| {
            pic.decoded_curr_pic = Self::picture(p.recon, p.n, 0);
            pic.reference_frames = [Self::invalid(); 15];
            if let Some(r) = p.reference {
                pic.reference_frames[0] = refs(r);
            }
            pic.coded_buf = p.coded;
            pic.collocated_ref_pic_index = if p.reference.is_some() && t.tmvp {
                0
            } else {
                0xff
            };
            pic.pic_init_qp = self.pps_qp;
            pic.nal_unit_type = if p.idr { NAL_IDR_W_RADL } else { NAL_TRAIL_R };
            pic.pic_fields = p.idr as u32
                | (if p.idr { 1 } else { 2 }) << 1
                | 1 << 4
                | (t.sign_hiding as u32) << 6
                | (t.transform_skip as u32) << 8
                | (t.cu_qp_delta as u32) << 9;
        });
        out.push(Buffer::new(ffi::BUF_PICTURE, pic));
        let qp_delta = if p.rc.mode == RcMode::Cqp {
            p.rc.qp as i32 - self.pps_qp as i32
        } else {
            0
        };
        if self.packed {
            if p.idr {
                let mut seq = self.vps();
                seq.extend(self.sps());
                out.extend(Buffer::packed(ffi::PACKED_TYPE_SEQUENCE, seq));
                out.extend(Buffer::packed(ffi::PACKED_TYPE_PICTURE, self.pps()));
            }
            out.extend(Buffer::packed(
                ffi::PACKED_TYPE_SLICE,
                self.slice_header(p.idr, p.n, qp_delta),
            ));
        }
        let sl = ffi::param(|sl: &mut ffi::VAEncSliceParameterBufferHEVC| {
            sl.num_ctu_in_slice = self.ctbs();
            sl.slice_type = if p.idr { 2 } else { 1 };
            sl.ref_pic_list0 = [Self::invalid(); 15];
            sl.ref_pic_list1 = [Self::invalid(); 15];
            if let Some(r) = p.reference {
                sl.ref_pic_list0[0] = refs(r);
            }
            sl.max_num_merge_cand = 5;
            sl.slice_qp_delta = qp_delta as i8;
            let tmvp = t.tmvp && !p.idr;
            sl.slice_fields = 1
                | (tmvp as u32) << 4
                | (t.sao as u32) << 5
                | (t.sao as u32) << 6
                | (tmvp as u32) << 13;
        });
        out.push(Buffer::new(ffi::BUF_SLICE, sl));
        out
    }

    fn unit(
        &mut self,
        data: Vec<u8>,
        pts_ms: i64,
        idr: bool,
    ) -> Result<Option<AccessUnit>, EncodeError> {
        let au = self.units.unit(data, pts_ms);
        match &au {
            None if idr => Err(EncodeError(
                "VA-API HEVC IDR came out without a picture".into(),
            )),
            Some(a) if a.keyframe != idr => Err(EncodeError(format!(
                "VA-API HEVC keyframe {} where {} was asked for",
                a.keyframe, idr
            ))),
            Some(a) if !self.announced && a.config.is_none() => Err(EncodeError(
                "VA-API HEVC stream starts without an SPS".into(),
            )),
            _ => {
                self.announced = true;
                Ok(au)
            }
        }
    }
}
