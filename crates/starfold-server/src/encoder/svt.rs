//! SVT-AV1 (BSD-3-Clause-Clear, only with the `svt-av1` feature): AV1 low-delay real-time coding
//! (`rtc`) with screen-content mode, presets 10-13.

#[allow(
    non_camel_case_types,
    non_upper_case_globals,
    non_snake_case,
    dead_code,
    unused_imports,
    clippy::all
)]
mod ffi {
    include!(concat!(env!("OUT_DIR"), "/SvtAv1Enc.rs"));
}

use super::{
    av1, AccessUnit, BitstreamFormat, Codec, ColorInfo, EncodeError, EncodeOptions, EncoderConfig,
    FrameView, RateConfig, StreamConfig, VideoEncoder,
};
use std::{mem, ptr};

const PRESET: i32 = 12;
/// SVT-AV1 refuses targets above 100 Mbit/s, at open and on the fly.
const UNCAPPED_KBPS: u32 = 100_000;

pub struct Svt {
    handle: *mut ffi::EbComponentType,
    width: u32,
    height: u32,
    pts: i64,
    rate: Option<ffi::SvtAv1RateInfo>,
    announced: bool,
    ready: bool,
}

unsafe impl Send for Svt {}

fn kbps(rate: RateConfig) -> u32 {
    if rate.max_kbps == 0 {
        UNCAPPED_KBPS
    } else {
        rate.max_kbps.min(UNCAPPED_KBPS)
    }
}

fn check(what: &str, r: ffi::EbErrorType) -> Result<(), EncodeError> {
    if r == ffi::EbErrorType_EB_ErrorNone {
        Ok(())
    } else {
        Err(EncodeError(format!("SVT-AV1 {what} failed ({r:#x})")))
    }
}

/// AV1 level x10 (40 = 4.0) whose picture size fits.
fn level(w: u32, h: u32) -> u32 {
    match w as u64 * h as u64 {
        0..=2_228_224 => 40,
        2_228_225..=8_912_896 => 50,
        _ => 60,
    }
}

impl Svt {
    pub fn new(c: &EncoderConfig) -> Result<Self, EncodeError> {
        if c.codec != Codec::Av1 {
            return Err(EncodeError(format!("SVT-AV1 can't encode {:?}", c.codec)));
        }
        let preset = super::arm_speed("svt-av1", &c.preset, PRESET)?;
        let preset = i8::try_from(preset)
            .ok()
            .filter(|p| (-2..=13).contains(p))
            .ok_or_else(|| EncodeError(format!("SVT-AV1 preset {preset} is not in -2..=13")))?;
        let mut cfg: ffi::EbSvtAv1EncConfiguration = unsafe { mem::zeroed() };
        let mut handle: *mut ffi::EbComponentType = ptr::null_mut();
        check("init handle", unsafe {
            ffi::svt_av1_enc_init_handle(&mut handle, &mut cfg)
        })?;
        let mut enc = Svt {
            handle,
            width: c.width,
            height: c.height,
            pts: -1,
            rate: None,
            announced: false,
            ready: false,
        };
        let color = ColorInfo::SRGB_BT601_LIMITED;
        cfg.enc_mode = preset;
        cfg.source_width = c.width;
        cfg.source_height = c.height;
        cfg.frame_rate_numerator = c.fps.max(1);
        cfg.frame_rate_denominator = 1;
        cfg.encoder_color_format = ffi::EbColorFormat_EB_YUV420;
        cfg.encoder_bit_depth = 8;
        cfg.rtc = true;
        cfg.pred_structure = ffi::PredStructure_LOW_DELAY as _;
        cfg.rate_control_mode = ffi::SvtAv1RcMode_SVT_AV1_RC_MODE_CBR as u8;
        cfg.target_bit_rate = kbps(c.rate).saturating_mul(1000);
        cfg.max_qp_allowed = 63;
        cfg.min_qp_allowed = (c.rate.quality.round().max(1.0) as u32).min(63);
        cfg.intra_period_length = -1;
        cfg.force_key_frames = true;
        cfg.look_ahead_distance = 0;
        cfg.scene_change_detection = 0;
        cfg.screen_content_mode = 1;
        cfg.enable_intrabc = true;
        cfg.level = level(c.width, c.height);
        cfg.level_of_parallelism = c.threads.clamp(1, 6);
        cfg.color_primaries = color.primaries as _;
        cfg.transfer_characteristics = color.transfer as _;
        cfg.matrix_coefficients = color.matrix as _;
        cfg.color_range = ffi::EbColorRange_EB_CR_STUDIO_RANGE as _;
        check("parameters", unsafe {
            ffi::svt_av1_enc_set_parameter(enc.handle, &mut cfg)
        })?;
        check("init", unsafe { ffi::svt_av1_enc_init(enc.handle) })?;
        enc.ready = true;
        Ok(enc)
    }

    /// The packet for the picture just sent. In low-delay mode `get_packet` blocks until it is
    /// ready (SVT-AV1's API contract), so there is no polling here.
    fn packet(&mut self) -> Result<Option<AccessUnit>, EncodeError> {
        let mut out: *mut ffi::EbBufferHeaderType = ptr::null_mut();
        let r = unsafe { ffi::svt_av1_enc_get_packet(self.handle, &mut out, 0) };
        if r == ffi::EbErrorType_EB_NoErrorEmptyQueue {
            return Ok(None);
        }
        check("get packet", r)?;
        if out.is_null() {
            return Err(EncodeError("SVT-AV1 returned no buffer".into()));
        }
        let unit = self.unit(unsafe { &*out });
        unsafe { ffi::svt_av1_enc_release_out_buffer(&mut out) };
        unit
    }

    fn unit(&mut self, b: &ffi::EbBufferHeaderType) -> Result<Option<AccessUnit>, EncodeError> {
        if b.flags & ffi::EB_BUFFERFLAG_ERROR_MASK != 0 {
            return Err(EncodeError(format!("SVT-AV1 packet flags {:#x}", b.flags)));
        }
        if b.p_buffer.is_null() || b.n_filled_len == 0 {
            return Ok(None);
        }
        if b.n_filled_len > b.n_alloc_len {
            return Err(EncodeError("SVT-AV1 packet longer than its buffer".into()));
        }
        let data =
            unsafe { std::slice::from_raw_parts(b.p_buffer, b.n_filled_len as usize) }.to_vec();
        let keyframe = b.pic_type == ffi::EbAv1PictureType_EB_AV1_KEY_PICTURE;
        let config = if keyframe && !self.announced {
            let codec_string = av1::codec_string(&data)
                .ok_or_else(|| EncodeError("SVT-AV1 keyframe without a sequence header".into()))?;
            self.announced = true;
            Some(StreamConfig {
                codec: Codec::Av1,
                codec_string,
                coded: (self.width, self.height),
                display: (self.width, self.height),
                format: BitstreamFormat::Av1LowOverhead,
                color: ColorInfo::SRGB_BT601_LIMITED,
            })
        } else {
            None
        };
        Ok(Some(AccessUnit {
            pts_ms: b.pts,
            keyframe,
            data,
            config,
        }))
    }
}

impl VideoEncoder for Svt {
    /// Sent with the next picture as a rate-change event.
    fn set_rate(&mut self, rate: RateConfig) -> Result<(), EncodeError> {
        self.rate = Some(ffi::SvtAv1RateInfo {
            seq_qp: 0,
            target_bit_rate: kbps(rate).saturating_mul(1000),
        });
        Ok(())
    }

    fn encode(
        &mut self,
        p: &FrameView<'_>,
        opts: EncodeOptions<'_>,
    ) -> Result<Vec<AccessUnit>, EncodeError> {
        if (p.width, p.height) != (self.width, self.height) {
            return Err(EncodeError(format!(
                "SVT-AV1 opened at {}x{}, got {}x{}",
                self.width, self.height, p.width, p.height
            )));
        }
        p.check()?;
        self.pts = opts.pts_ms.max(self.pts + 1);
        let mut io = ffi::EbSvtIOFormat {
            luma: p.y.as_ptr() as *mut u8,
            cb: p.u.as_ptr() as *mut u8,
            cr: p.v.as_ptr() as *mut u8,
            y_stride: p.stride_y as u32,
            cr_stride: p.stride_uv as u32,
            cb_stride: p.stride_uv as u32,
        };
        let mut rate = self.rate.take();
        let mut node = rate.as_mut().map(|r| ffi::EbPrivDataNode {
            node_type: ffi::PrivDataType_RATE_CHANGE_EVENT,
            data: r as *mut _ as *mut _,
            size: mem::size_of::<ffi::SvtAv1RateInfo>() as u32,
            next: ptr::null_mut(),
        });
        let mut header: ffi::EbBufferHeaderType = unsafe { mem::zeroed() };
        header.size = mem::size_of::<ffi::EbBufferHeaderType>() as u32;
        header.p_buffer = &mut io as *mut _ as *mut u8;
        header.n_filled_len = (p.y.len() + p.u.len() + p.v.len()) as u32;
        header.n_alloc_len = header.n_filled_len;
        header.pts = self.pts;
        header.pic_type = if opts.force_keyframe {
            ffi::EbAv1PictureType_EB_AV1_KEY_PICTURE
        } else {
            ffi::EbAv1PictureType_EB_AV1_INVALID_PICTURE
        };
        header.p_app_private = node
            .as_mut()
            .map_or(ptr::null_mut(), |n| n as *mut _ as *mut _);
        check("send picture", unsafe {
            ffi::svt_av1_enc_send_picture(self.handle, &mut header)
        })?;
        Ok(self.packet()?.into_iter().collect())
    }

    fn drain(&mut self) -> Result<Vec<AccessUnit>, EncodeError> {
        let mut eos: ffi::EbBufferHeaderType = unsafe { mem::zeroed() };
        eos.size = mem::size_of::<ffi::EbBufferHeaderType>() as u32;
        eos.flags = ffi::EB_BUFFERFLAG_EOS;
        eos.pic_type = ffi::EbAv1PictureType_EB_AV1_INVALID_PICTURE;
        check("end of stream", unsafe {
            ffi::svt_av1_enc_send_picture(self.handle, &mut eos)
        })?;
        let mut out = Vec::new();
        loop {
            let mut b: *mut ffi::EbBufferHeaderType = ptr::null_mut();
            let r = unsafe { ffi::svt_av1_enc_get_packet(self.handle, &mut b, 1) };
            if r == ffi::EbErrorType_EB_NoErrorEmptyQueue || b.is_null() {
                return Ok(out);
            }
            check("get packet", r)?;
            let last = unsafe { (*b).flags } & ffi::EB_BUFFERFLAG_EOS != 0;
            let unit = self.unit(unsafe { &*b });
            unsafe { ffi::svt_av1_enc_release_out_buffer(&mut b) };
            out.extend(unit?);
            if last {
                return Ok(out);
            }
        }
    }
}

impl Drop for Svt {
    fn drop(&mut self) {
        unsafe {
            if self.ready {
                ffi::svt_av1_enc_deinit(self.handle);
            }
            if !self.handle.is_null() {
                ffi::svt_av1_enc_deinit_handle(self.handle);
            }
        }
    }
}
