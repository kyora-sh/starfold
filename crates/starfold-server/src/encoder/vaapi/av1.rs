//! AV1 profile 0: the sequence header and frame header are packed by us (the driver
//! patches q index, loop filter and CDEF at the offsets given under bitrate control and
//! adds the tile group), a temporal delimiter goes in front. Every inter frame
//! references the last one through slot 0; no order-hint dependent tools.

use super::bits::{leb128_fixed, Bits};
use super::ffi;
use super::session::{Buffer, Choice, Params, Picture, RcMode};
use crate::encoder::{
    av1, AccessUnit, BitstreamFormat, Codec, ColorInfo, EncodeError, EncoderConfig, StreamConfig,
};

pub const PROFILES: &[i32] = &[ffi::PROFILE_AV1_0];
pub const EXTRA: &[i32] = &[ffi::ATTRIB_AV1_EXT1, ffi::ATTRIB_AV1_EXT2];
const ORDER_HINT_BITS: u32 = 8;
const PRIMARY_REF_NONE: u64 = 7;
const NEED: u32 = ffi::PACKED_SEQUENCE | ffi::PACKED_PICTURE;
const TD: [u8; 2] = [0x12, 0x00];
const CDEF: (u8, u8) = (2, 1);

/// `seq_level_idx` for a `w`x`h` picture at `fps`.
pub fn level(w: u64, h: u64, fps: u64) -> u8 {
    const LEVELS: &[(u8, u64, u64, u64, u64)] = &[
        (0, 147_456, 4_423_680, 2048, 1152),
        (1, 278_784, 8_363_520, 2816, 1584),
        (4, 665_856, 19_975_680, 4352, 2448),
        (5, 1_065_024, 31_950_720, 5504, 3096),
        (8, 2_359_296, 70_778_880, 6144, 3456),
        (9, 2_359_296, 141_557_760, 6144, 3456),
        (12, 8_912_896, 267_386_880, 8192, 4352),
        (13, 8_912_896, 534_773_760, 8192, 4352),
        (14, 8_912_896, 1_069_547_520, 8192, 4352),
        (16, 35_651_584, 1_069_547_520, 16384, 8704),
        (17, 35_651_584, 2_139_095_040, 16384, 8704),
    ];
    let ps = w * h;
    LEVELS
        .iter()
        .find(|l| ps <= l.1 && ps * fps <= l.2 && w <= l.3 && h <= l.4)
        .map_or(18, |l| l.0)
}

fn tile_log2(blk: u32, target: u32) -> u32 {
    let mut k = 0;
    while (blk << k) < target {
        k += 1;
    }
    k
}

/// Uniform tiles, as few as the size allows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tiles {
    pub cols_log2: u32,
    pub rows_log2: u32,
    min_cols_log2: u32,
    max_cols_log2: u32,
    min_rows_log2: u32,
    max_rows_log2: u32,
    pub widths: Vec<u16>,
    pub heights: Vec<u16>,
}

impl Tiles {
    pub fn new(w: u32, h: u32) -> Tiles {
        let sb_cols = (2 * w.div_ceil(8)).div_ceil(16);
        let sb_rows = (2 * h.div_ceil(8)).div_ceil(16);
        let min_cols_log2 = tile_log2(4096 >> 6, sb_cols);
        let max_cols_log2 = tile_log2(1, sb_cols.min(64));
        let max_rows_log2 = tile_log2(1, sb_rows.min(64));
        let min_tiles_log2 = min_cols_log2.max(tile_log2((4096 * 2304) >> 12, sb_rows * sb_cols));
        let cols_log2 = min_cols_log2;
        let min_rows_log2 = min_tiles_log2.saturating_sub(cols_log2);
        let rows_log2 = min_rows_log2;
        let split = |n: u32, log2: u32| -> Vec<u16> {
            let size = (n + (1 << log2) - 1) >> log2;
            let mut v = Vec::new();
            let mut left = n;
            while left > 0 {
                let s = size.min(left);
                v.push((s - 1) as u16);
                left -= s;
            }
            v
        };
        Tiles {
            cols_log2,
            rows_log2,
            min_cols_log2,
            max_cols_log2,
            min_rows_log2,
            max_rows_log2,
            widths: split(sb_cols, cols_log2),
            heights: split(sb_rows, rows_log2),
        }
    }

    fn write(&self, b: &mut Bits, size_bytes: u32) {
        b.flag(true);
        if self.min_cols_log2 < self.max_cols_log2 {
            b.flag(false);
        }
        if self.min_rows_log2 < self.max_rows_log2 {
            b.flag(false);
        }
        if self.cols_log2 > 0 || self.rows_log2 > 0 {
            b.put(self.cols_log2 + self.rows_log2, 0);
            b.put(2, (size_bytes - 1) as u64);
        }
    }

    fn count(&self) -> usize {
        self.widths.len() * self.heights.len()
    }
}

/// Where the driver patches a frame header, in bits from the start of the packed data.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Offsets {
    pub qindex: u32,
    pub segmentation: u32,
    pub loop_filter: u32,
    pub cdef: u32,
    pub cdef_bits: u32,
    pub header_bits: u32,
}

pub struct Av1 {
    size: (u32, u32),
    level: u8,
    tiles: Tiles,
    color: ColorInfo,
    /// The `interpolation_filter` value, 4 for switchable.
    filter: u8,
    tx_select: bool,
    obu_size_bytes: usize,
    tile_size_bytes: u32,
    seq_header: Option<Vec<u8>>,
}

impl Av1 {
    pub fn new(choice: &Choice, cfg: &EncoderConfig) -> Result<Av1, EncodeError> {
        if choice.packed.is_none_or(|p| p & NEED != NEED) {
            return Err(EncodeError(
                "the driver doesn't take packed AV1 sequence and frame headers".into(),
            ));
        }
        let ext1 = choice.extra.first().copied().flatten();
        let ext2 = choice.extra.get(1).copied().flatten();
        let filters = ext1.map_or(1, |v| v & 0x1f);
        let filter = if filters & 0x10 != 0 {
            4
        } else {
            (0..4).find(|i| filters & (1 << i) != 0).unwrap_or(0)
        };
        let (tile_size_bytes, obu_size_bytes, tx_modes) = match ext2 {
            Some(v) => ((v & 3) + 1, ((v >> 2) & 3) as usize + 1, (v >> 4) & 7),
            None => (4, 4, 4),
        };
        Ok(Av1 {
            size: (cfg.width, cfg.height),
            level: level(cfg.width as u64, cfg.height as u64, cfg.fps.max(1) as u64),
            tiles: Tiles::new(cfg.width, cfg.height),
            color: ColorInfo::SRGB_BT601_LIMITED,
            filter,
            tx_select: tx_modes & 4 != 0 || tx_modes == 0,
            obu_size_bytes,
            tile_size_bytes,
            seq_header: None,
        })
    }

    pub fn packed(driver: Option<u32>) -> Option<u32> {
        driver.map(|p| p & NEED)
    }

    /// The sequence header OBU.
    pub fn sequence_header(&self) -> Vec<u8> {
        let (w, h) = self.size;
        let wb = 32 - (w - 1).max(1).leading_zeros();
        let hb = 32 - (h - 1).max(1).leading_zeros();
        let mut b = Bits::new();
        b.put(3, 0);
        b.flag(false);
        b.flag(false);
        b.flag(false);
        b.flag(false);
        b.put(5, 0);
        b.put(12, 0);
        b.put(5, self.level as u64);
        if self.level > 7 {
            b.flag(false);
        }
        b.put(4, (wb - 1) as u64);
        b.put(4, (hb - 1) as u64);
        b.put(wb, (w - 1) as u64);
        b.put(hb, (h - 1) as u64);
        b.flag(false);
        for _ in 0..7 {
            b.flag(false);
        }
        b.flag(true);
        b.flag(false);
        b.flag(false);
        b.flag(false);
        b.flag(false);
        b.put(3, (ORDER_HINT_BITS - 1) as u64);
        b.flag(false);
        b.flag(true);
        b.flag(false);
        b.flag(false);
        b.flag(false);
        b.flag(true);
        b.put(8, self.color.primaries as u64);
        b.put(8, self.color.transfer as u64);
        b.put(8, self.color.matrix as u64);
        b.flag(self.color.full_range);
        b.put(2, 0);
        b.flag(false);
        b.flag(false);
        b.trailing();
        let payload = b.into_bytes();
        let mut out = vec![0x0a];
        out.extend(leb128(payload.len()));
        out.extend(payload);
        out
    }

    /// The frame header OBU and where the driver may patch it.
    pub fn frame_header(&self, key: bool, order_hint: u64, qindex: u8) -> (Vec<u8>, Offsets) {
        let mut b = Bits::new();
        let head = 8 + 8 * self.obu_size_bytes as u32;
        let at = |b: &Bits| head + b.len() as u32;
        let mut o = Offsets::default();
        b.flag(false);
        b.put(2, if key { 0 } else { 1 });
        b.flag(true);
        if !key {
            b.flag(false);
        }
        b.flag(false);
        b.flag(false);
        b.put(ORDER_HINT_BITS, order_hint % (1 << ORDER_HINT_BITS));
        if !key {
            b.put(3, PRIMARY_REF_NONE);
            b.put(8, 0x01);
        }
        if key {
            b.flag(false);
        } else {
            b.flag(false);
            for _ in 0..7 {
                b.put(3, 0);
            }
            b.flag(false);
            b.flag(false);
            b.flag(self.filter == 4);
            if self.filter != 4 {
                b.put(2, self.filter as u64);
            }
            b.flag(false);
        }
        b.flag(false);
        self.tiles.write(&mut b, self.tile_size_bytes);
        o.qindex = at(&b);
        b.put(8, qindex as u64);
        b.flag(false);
        b.flag(false);
        b.flag(false);
        b.flag(false);
        o.segmentation = at(&b);
        b.flag(false);
        b.flag(false);
        o.loop_filter = at(&b);
        let lf = filter_level(qindex);
        b.put(6, lf as u64);
        b.put(6, lf as u64);
        if lf > 0 {
            b.put(6, lf as u64);
            b.put(6, lf as u64);
        }
        b.put(3, 0);
        b.flag(false);
        o.cdef = at(&b);
        b.put(2, 0);
        b.put(2, 0);
        b.put(4, CDEF.0 as u64);
        b.put(2, CDEF.1 as u64);
        b.put(4, CDEF.0 as u64);
        b.put(2, CDEF.1 as u64);
        o.cdef_bits = at(&b) - o.cdef;
        b.flag(self.tx_select);
        if !key {
            b.flag(false);
        }
        b.flag(false);
        if !key {
            for _ in 0..7 {
                b.flag(false);
            }
        }
        b.put(1, 1);
        o.header_bits = at(&b);
        b.align(0);
        let payload = b.into_bytes();
        let mut out = vec![0x1a];
        out.extend(leb128_fixed(payload.len() as u32, self.obu_size_bytes));
        out.extend(payload);
        (out, o)
    }
}

/// A loop filter level that follows the quantizer, as the driver would pick.
fn filter_level(qindex: u8) -> u8 {
    (qindex / 6).min(63)
}

fn leb128(mut v: usize) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return out;
        }
        out.push(b | 0x80);
    }
}

/// AV1 q index for an H.264-scale QP.
pub fn qindex(qp: u8) -> u8 {
    ((qp as u32 * 255 / 51) as u8).max(1)
}

impl Params for Av1 {
    fn surface_size(&self) -> (u32, u32) {
        (self.size.0.div_ceil(16) * 16, self.size.1.div_ceil(16) * 16)
    }

    fn buffers(&mut self, p: &Picture, misc: Vec<Buffer>) -> Vec<Buffer> {
        let brc = p.rc.mode != RcMode::Cqp;
        let mut out = Vec::new();
        if p.idr {
            let s = ffi::param(|s: &mut ffi::VAEncSequenceParameterBufferAV1| {
                s.seq_level_idx = self.level;
                s.intra_period = p.rc.fps.saturating_mul(60);
                s.ip_period = 1;
                s.bits_per_second = if brc { p.rc.bps } else { 0 };
                s.seq_fields = 1 << 8 | 1 << 12 | 1 << 17 | 1 << 18;
                s.order_hint_bits_minus_1 = (ORDER_HINT_BITS - 1) as u8;
            });
            out.push(Buffer::new(ffi::BUF_SEQUENCE, s));
        }
        out.extend(misc);
        let q = qindex(p.rc.qp);
        let (header, off) = self.frame_header(p.idr, p.n, q);
        let pic = ffi::param(|pic: &mut ffi::VAEncPictureParameterBufferAV1| {
            pic.frame_width_minus_1 = (self.size.0 - 1) as u16;
            pic.frame_height_minus_1 = (self.size.1 - 1) as u16;
            pic.reconstructed_frame = p.recon;
            pic.coded_buf = p.coded;
            pic.reference_frames = [ffi::INVALID_ID; 8];
            if let Some(r) = p.reference {
                pic.reference_frames[0] = r;
                pic.ref_frame_ctrl_l0 = 1;
            }
            pic.primary_ref_frame = PRIMARY_REF_NONE as u8;
            pic.order_hint = (p.n % (1 << ORDER_HINT_BITS)) as u8;
            pic.refresh_frame_flags = if p.idr { 0xff } else { 0x01 };
            pic.picture_flags = if p.idr { 1 << 2 } else { 1 };
            let lf = filter_level(q);
            pic.filter_level = [lf, lf];
            pic.filter_level_u = lf;
            pic.filter_level_v = lf;
            pic.interpolation_filter = self.filter;
            pic.base_qindex = q;
            pic.min_base_qindex = 1;
            pic.max_base_qindex = 255;
            pic.mode_control_flags = (if self.tx_select { 2 } else { 1 }) << 7;
            pic.tile_cols = self.tiles.widths.len() as u8;
            pic.tile_rows = self.tiles.heights.len() as u8;
            for (d, s) in pic.width_in_sbs_minus_1.iter_mut().zip(&self.tiles.widths) {
                *d = *s;
            }
            for (d, s) in pic
                .height_in_sbs_minus_1
                .iter_mut()
                .zip(&self.tiles.heights)
            {
                *d = *s;
            }
            pic.cdef_y_strengths[0] = CDEF.0 << 2 | CDEF.1;
            pic.cdef_uv_strengths[0] = CDEF.0 << 2 | CDEF.1;
            for wm in &mut pic.wm {
                wm.wmmat[2] = 1 << 16;
                wm.wmmat[5] = 1 << 16;
            }
            if brc {
                pic.bit_offset_qindex = off.qindex;
                pic.bit_offset_segmentation = off.segmentation;
                pic.bit_offset_loopfilter_params = off.loop_filter;
                pic.bit_offset_cdef_params = off.cdef;
                pic.size_in_bits_cdef_params = off.cdef_bits;
                pic.byte_offset_frame_hdr_obu_size = 1;
                pic.size_in_bits_frame_hdr_obu = off.header_bits;
            }
            pic.tile_group_obu_hdr_info = 1 << 1;
        });
        out.push(Buffer::new(ffi::BUF_PICTURE, pic));
        if p.idr {
            out.extend(Buffer::packed(
                ffi::PACKED_TYPE_SEQUENCE,
                self.sequence_header(),
            ));
        }
        out.extend(Buffer::packed(ffi::PACKED_TYPE_PICTURE, header));
        let tg = ffi::param(|tg: &mut ffi::VAEncTileGroupBufferAV1| {
            tg.tg_end = (self.tiles.count() - 1) as u8;
        });
        out.push(Buffer::new(ffi::BUF_SLICE, tg));
        out
    }

    fn unit(
        &mut self,
        data: Vec<u8>,
        pts_ms: i64,
        idr: bool,
    ) -> Result<Option<AccessUnit>, EncodeError> {
        if data.is_empty() {
            return Err(EncodeError("VA-API AV1 produced no data".into()));
        }
        let mut tu = Vec::with_capacity(data.len() + 2);
        if (data[0] >> 3) & 0xf != 2 {
            tu.extend_from_slice(&TD);
        }
        tu.extend(data);
        let sh = av1::sequence_header(&tu).map(<[u8]>::to_vec);
        let config = match (&sh, idr) {
            (None, true) => {
                return Err(EncodeError(
                    "VA-API AV1 key frame without a sequence header".into(),
                ))
            }
            (Some(s), true) if self.seq_header.as_ref() != Some(s) => {
                let codec_string = av1::codec_string(&tu)
                    .ok_or_else(|| EncodeError("unreadable AV1 sequence header".into()))?;
                self.seq_header = sh.clone();
                Some(StreamConfig {
                    codec: Codec::Av1,
                    codec_string,
                    coded: self.size,
                    display: self.size,
                    format: BitstreamFormat::Av1LowOverhead,
                    color: self.color,
                })
            }
            _ => None,
        };
        Ok(Some(AccessUnit {
            pts_ms,
            keyframe: idr,
            data: tu,
            config,
        }))
    }
}
