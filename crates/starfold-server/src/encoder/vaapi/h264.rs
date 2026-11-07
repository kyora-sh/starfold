//! H.264: SPS and PPS packed before every IDR (when the driver takes packed headers),
//! slice headers from the driver. POC type 2, so no reordering and no POC in the slice.

use super::bits::{nal, Bits};
use super::ffi::{self, VaId};
use super::session::{Buffer, Choice, Params, Picture, Rc, RcMode};
use crate::encoder::h264::ParamSets;
use crate::encoder::{AccessUnit, ColorInfo, EncodeError, EncoderConfig};

pub const PROFILES: &[i32] = &[
    ffi::PROFILE_H264_HIGH,
    ffi::PROFILE_H264_MAIN,
    ffi::PROFILE_H264_CB,
];
const LOG2_MAX_FRAME_NUM: u32 = 8;
const BOTH: u32 = ffi::PACKED_SEQUENCE | ffi::PACKED_PICTURE;

/// The lowest level whose frame size, frame dimensions and macroblock rate hold a
/// `w`x`h` macroblock stream at `fps`.
pub fn level(w: u64, h: u64, fps: u64) -> u8 {
    let mbs = w * h;
    const LEVELS: &[(u8, u64, u64)] = &[
        (30, 1620, 40_500),
        (31, 3600, 108_000),
        (32, 5120, 216_000),
        (40, 8192, 245_760),
        (42, 8704, 522_240),
        (50, 22_080, 589_824),
        (51, 36_864, 983_040),
        (52, 36_864, 2_073_600),
        (60, 139_264, 4_177_920),
        (61, 139_264, 8_355_840),
    ];
    LEVELS
        .iter()
        .find(|l| mbs <= l.1 && mbs * fps <= l.2 && w * w <= 8 * l.1 && h * h <= 8 * l.1)
        .map_or(62, |l| l.0)
}

pub struct H264 {
    profile: i32,
    size: (u32, u32),
    mbs: (u32, u32),
    level: u8,
    color: ColorInfo,
    packed: bool,
    pps_qp: u8,
    units: ParamSets,
}

impl H264 {
    pub fn new(choice: &Choice, cfg: &EncoderConfig, rc: &Rc) -> H264 {
        let mbs = (cfg.width.div_ceil(16), cfg.height.div_ceil(16));
        H264 {
            profile: choice.profile,
            size: (cfg.width, cfg.height),
            mbs,
            level: level(mbs.0 as u64, mbs.1 as u64, cfg.fps.max(1) as u64),
            color: ColorInfo::SRGB_BT601_LIMITED,
            packed: choice.packed == Some(BOTH),
            pps_qp: rc.qp,
            units: ParamSets::default(),
        }
    }

    /// SPS and PPS packed together or not at all.
    pub fn packed(driver: Option<u32>) -> Option<u32> {
        driver.map(|p| if p & BOTH == BOTH { BOTH } else { 0 })
    }

    fn high(&self) -> bool {
        self.profile == ffi::PROFILE_H264_HIGH
    }

    fn cabac(&self) -> bool {
        self.profile != ffi::PROFILE_H264_CB
    }

    fn crop(&self) -> (u32, u32) {
        (
            (self.mbs.0 * 16 - self.size.0) / 2,
            (self.mbs.1 * 16 - self.size.1) / 2,
        )
    }

    pub fn sps(&self) -> Vec<u8> {
        let mut b = Bits::new();
        let (idc, constraints) = match self.profile {
            ffi::PROFILE_H264_HIGH => (100, 0),
            ffi::PROFILE_H264_MAIN => (77, 0x40),
            _ => (66, 0xc0),
        };
        b.put(8, idc);
        b.put(8, constraints);
        b.put(8, self.level as u64);
        b.ue(0);
        if self.high() {
            b.ue(1);
            b.ue(0);
            b.ue(0);
            b.flag(false);
            b.flag(false);
        }
        b.ue(LOG2_MAX_FRAME_NUM - 4);
        b.ue(2);
        b.ue(1);
        b.flag(false);
        b.ue(self.mbs.0 - 1);
        b.ue(self.mbs.1 - 1);
        b.flag(true);
        b.flag(true);
        let (cx, cy) = self.crop();
        b.flag(cx > 0 || cy > 0);
        if cx > 0 || cy > 0 {
            b.ue(0);
            b.ue(cx);
            b.ue(0);
            b.ue(cy);
        }
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
        b.flag(false);
        b.flag(false);
        b.flag(false);
        b.flag(false);
        b.flag(false);
        b.flag(true);
        b.flag(true);
        b.ue(0);
        b.ue(0);
        b.ue(15);
        b.ue(15);
        b.ue(0);
        b.ue(1);
        b.trailing();
        nal(&[0x67], b.bytes())
    }

    pub fn pps(&self) -> Vec<u8> {
        let mut b = Bits::new();
        b.ue(0);
        b.ue(0);
        b.flag(self.cabac());
        b.flag(false);
        b.ue(0);
        b.ue(0);
        b.ue(0);
        b.flag(false);
        b.put(2, 0);
        b.se(self.pps_qp as i32 - 26);
        b.se(0);
        b.se(0);
        b.flag(true);
        b.flag(false);
        b.flag(false);
        if self.high() {
            b.flag(true);
            b.flag(false);
            b.se(0);
        }
        b.trailing();
        nal(&[0x68], b.bytes())
    }

    fn picture(id: VaId, n: u64) -> ffi::VAPictureH264 {
        let mut p: ffi::VAPictureH264 = ffi::zeroed();
        p.picture_id = id;
        p.frame_idx = (n % (1 << LOG2_MAX_FRAME_NUM)) as u32;
        p.flags = ffi::PICTURE_H264_SHORT_TERM_REFERENCE;
        p.top_field_order_cnt = ((2 * n) % (1 << 31)) as i32;
        p.bottom_field_order_cnt = p.top_field_order_cnt;
        p
    }

    fn invalid() -> ffi::VAPictureH264 {
        let mut p: ffi::VAPictureH264 = ffi::zeroed();
        p.picture_id = ffi::INVALID_ID;
        p.flags = ffi::PICTURE_H264_INVALID;
        p
    }
}

impl Params for H264 {
    fn surface_size(&self) -> (u32, u32) {
        (self.mbs.0 * 16, self.mbs.1 * 16)
    }

    fn buffers(&mut self, p: &Picture, misc: Vec<Buffer>) -> Vec<Buffer> {
        let mut out = Vec::new();
        if p.idr {
            if p.rc.mode == RcMode::Cqp {
                self.pps_qp = p.rc.qp;
            }
            let s = ffi::param(|s: &mut ffi::VAEncSequenceParameterBufferH264| {
                s.level_idc = self.level;
                s.intra_period = p.rc.fps.saturating_mul(60);
                s.intra_idr_period = s.intra_period;
                s.ip_period = 1;
                s.bits_per_second = if p.rc.mode == RcMode::Cqp {
                    0
                } else {
                    p.rc.bps
                };
                s.max_num_ref_frames = 1;
                s.picture_width_in_mbs = self.mbs.0 as u16;
                s.picture_height_in_mbs = self.mbs.1 as u16;
                s.seq_fields = 1 | 1 << 2 | 1 << 5 | (LOG2_MAX_FRAME_NUM - 4) << 6 | 2 << 10;
                let (cx, cy) = self.crop();
                s.frame_cropping_flag = (cx > 0 || cy > 0) as u8;
                s.frame_crop_right_offset = cx;
                s.frame_crop_bottom_offset = cy;
                s.vui_parameters_present_flag = 1;
                s.vui_fields = 1 << 2 | 15 << 3 | 15 << 8 | 1 << 15;
            });
            out.push(Buffer::new(ffi::BUF_SEQUENCE, s));
        }
        out.extend(misc);
        let pic = ffi::param(|pic: &mut ffi::VAEncPictureParameterBufferH264| {
            pic.curr_pic = Self::picture(p.recon, p.n);
            pic.reference_frames = [Self::invalid(); 16];
            if let Some(r) = p.reference {
                pic.reference_frames[0] = Self::picture(r, p.n - 1);
            }
            pic.coded_buf = p.coded;
            pic.frame_num = (p.n % (1 << LOG2_MAX_FRAME_NUM)) as u16;
            pic.pic_init_qp = self.pps_qp;
            pic.pic_fields = p.idr as u32
                | 1 << 1
                | (self.cabac() as u32) << 3
                | (self.high() as u32) << 8
                | 1 << 9;
        });
        out.push(Buffer::new(ffi::BUF_PICTURE, pic));
        if p.idr && self.packed {
            out.extend(Buffer::packed(ffi::PACKED_TYPE_SEQUENCE, self.sps()));
            out.extend(Buffer::packed(ffi::PACKED_TYPE_PICTURE, self.pps()));
        }
        let sl = ffi::param(|sl: &mut ffi::VAEncSliceParameterBufferH264| {
            sl.num_macroblocks = self.mbs.0 * self.mbs.1;
            sl.macroblock_info = ffi::INVALID_ID;
            sl.slice_type = if p.idr { 2 } else { 0 };
            sl.idr_pic_id = (p.idrs % 65536) as u16;
            sl.direct_spatial_mv_pred_flag = 1;
            sl.ref_pic_list0 = [Self::invalid(); 32];
            sl.ref_pic_list1 = [Self::invalid(); 32];
            if let Some(r) = p.reference {
                sl.ref_pic_list0[0] = Self::picture(r, p.n - 1);
            }
            if p.rc.mode == RcMode::Cqp {
                sl.slice_qp_delta = (p.rc.qp as i32 - self.pps_qp as i32) as i8;
            }
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
        let au = self.units.unit(data, pts_ms, self.size, self.color);
        match &au {
            None if idr => Err(EncodeError(
                "VA-API H.264 IDR came out without a picture".into(),
            )),
            Some(a) if a.keyframe != idr => Err(EncodeError(format!(
                "VA-API H.264 asked for {}, got {}",
                if idr { "an IDR" } else { "a P frame" },
                if a.keyframe { "an IDR" } else { "a P frame" }
            ))),
            _ => Ok(au),
        }
    }
}
