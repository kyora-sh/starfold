//! libva, loaded at runtime, and the parameter buffers the encoders fill.

#![allow(non_camel_case_types)]

use libloading::Library;
use std::ffi::{c_char, c_int, c_uint, c_void};

pub type VADisplay = *mut c_void;
pub type VAStatus = c_int;
pub type VaId = u32;

pub const INVALID_ID: VaId = 0xffff_ffff;

pub const PROFILE_H264_MAIN: c_int = 6;
pub const PROFILE_H264_HIGH: c_int = 7;
pub const PROFILE_H264_CB: c_int = 13;
pub const PROFILE_HEVC_MAIN: c_int = 17;
pub const PROFILE_AV1_0: c_int = 32;

pub const ENTRYPOINT_ENC_SLICE: c_int = 6;
pub const ENTRYPOINT_ENC_SLICE_LP: c_int = 8;

pub const ATTRIB_RT_FORMAT: c_int = 0;
pub const ATTRIB_RATE_CONTROL: c_int = 5;
pub const ATTRIB_PACKED_HEADERS: c_int = 10;
pub const ATTRIB_MAX_REF_FRAMES: c_int = 13;
pub const ATTRIB_MAX_WIDTH: c_int = 18;
pub const ATTRIB_MAX_HEIGHT: c_int = 19;
pub const ATTRIB_HEVC_FEATURES: c_int = 50;
pub const ATTRIB_HEVC_BLOCK_SIZES: c_int = 51;
pub const ATTRIB_AV1_EXT1: c_int = 53;
pub const ATTRIB_AV1_EXT2: c_int = 54;
pub const ATTRIB_NOT_SUPPORTED: u32 = 0x8000_0000;

pub const RT_FORMAT_YUV420: u32 = 0x1;
pub const RT_FORMAT_YUV420_10: u32 = 0x100;
pub const RC_CBR: u32 = 0x2;
pub const RC_VBR: u32 = 0x4;
pub const RC_CQP: u32 = 0x10;
pub const PACKED_SEQUENCE: u32 = 0x1;
pub const PACKED_PICTURE: u32 = 0x2;
pub const PACKED_SLICE: u32 = 0x4;

pub const BUF_CODED: c_int = 21;
pub const BUF_SEQUENCE: c_int = 22;
pub const BUF_PICTURE: c_int = 23;
pub const BUF_SLICE: c_int = 24;
pub const BUF_PACKED_PARAM: c_int = 25;
pub const BUF_PACKED_DATA: c_int = 26;
pub const BUF_MISC: c_int = 27;

pub const MISC_FRAME_RATE: u32 = 0;
pub const MISC_RATE_CONTROL: u32 = 1;
pub const MISC_HRD: u32 = 5;

pub const PACKED_TYPE_SEQUENCE: u32 = 1;
pub const PACKED_TYPE_PICTURE: u32 = 2;
pub const PACKED_TYPE_SLICE: u32 = 3;

pub const FOURCC_NV12: u32 = u32::from_le_bytes(*b"NV12");
pub const SURFACE_ATTRIB_PIXEL_FORMAT: c_int = 1;
pub const SURFACE_ATTRIB_SETTABLE: u32 = 0x2;
pub const GENERIC_VALUE_INTEGER: c_int = 1;
pub const PROGRESSIVE: c_int = 0x1;

pub const PICTURE_H264_INVALID: u32 = 0x1;
pub const PICTURE_H264_SHORT_TERM_REFERENCE: u32 = 0x8;
pub const PICTURE_HEVC_INVALID: u32 = 0x1;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct VAConfigAttrib {
    pub kind: c_int,
    pub value: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAGenericValue {
    pub kind: c_int,
    pub value: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VASurfaceAttrib {
    pub kind: c_int,
    pub flags: u32,
    pub value: VAGenericValue,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAImageFormat {
    pub fourcc: u32,
    pub byte_order: u32,
    pub bits_per_pixel: u32,
    pub depth: u32,
    pub red_mask: u32,
    pub green_mask: u32,
    pub blue_mask: u32,
    pub alpha_mask: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAImage {
    pub image_id: VaId,
    pub format: VAImageFormat,
    pub buf: VaId,
    pub width: u16,
    pub height: u16,
    pub data_size: u32,
    pub num_planes: u32,
    pub pitches: [u32; 3],
    pub offsets: [u32; 3],
    pub num_palette_entries: i32,
    pub entry_bytes: i32,
    pub component_order: [i8; 4],
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VACodedBufferSegment {
    pub size: u32,
    pub bit_offset: u32,
    pub status: u32,
    pub reserved: u32,
    pub buf: *mut c_void,
    pub next: *mut c_void,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncPackedHeaderParameterBuffer {
    pub kind: u32,
    pub bit_length: u32,
    pub has_emulation_bytes: u8,
    pub va_reserved: [u32; 4],
}

/// A misc parameter buffer: its type, then the parameters.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Misc<T> {
    pub kind: u32,
    pub data: T,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncMiscParameterRateControl {
    pub bits_per_second: u32,
    pub target_percentage: u32,
    pub window_size: u32,
    pub initial_qp: u32,
    pub min_qp: u32,
    pub basic_unit_size: u32,
    pub rc_flags: u32,
    pub icq_quality_factor: u32,
    pub max_qp: u32,
    pub quality_factor: u32,
    pub target_frame_size: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncMiscParameterFrameRate {
    pub framerate: u32,
    pub framerate_flags: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncMiscParameterHRD {
    pub initial_buffer_fullness: u32,
    pub buffer_size: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAPictureH264 {
    pub picture_id: VaId,
    pub frame_idx: u32,
    pub flags: u32,
    pub top_field_order_cnt: i32,
    pub bottom_field_order_cnt: i32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncSequenceParameterBufferH264 {
    pub seq_parameter_set_id: u8,
    pub level_idc: u8,
    pub intra_period: u32,
    pub intra_idr_period: u32,
    pub ip_period: u32,
    pub bits_per_second: u32,
    pub max_num_ref_frames: u32,
    pub picture_width_in_mbs: u16,
    pub picture_height_in_mbs: u16,
    pub seq_fields: u32,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub num_ref_frames_in_pic_order_cnt_cycle: u8,
    pub offset_for_non_ref_pic: i32,
    pub offset_for_top_to_bottom_field: i32,
    pub offset_for_ref_frame: [i32; 256],
    pub frame_cropping_flag: u8,
    pub frame_crop_left_offset: u32,
    pub frame_crop_right_offset: u32,
    pub frame_crop_top_offset: u32,
    pub frame_crop_bottom_offset: u32,
    pub vui_parameters_present_flag: u8,
    pub vui_fields: u32,
    pub aspect_ratio_idc: u8,
    pub sar_width: u32,
    pub sar_height: u32,
    pub num_units_in_tick: u32,
    pub time_scale: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncPictureParameterBufferH264 {
    pub curr_pic: VAPictureH264,
    pub reference_frames: [VAPictureH264; 16],
    pub coded_buf: VaId,
    pub pic_parameter_set_id: u8,
    pub seq_parameter_set_id: u8,
    pub last_picture: u8,
    pub frame_num: u16,
    pub pic_init_qp: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub chroma_qp_index_offset: i8,
    pub second_chroma_qp_index_offset: i8,
    pub pic_fields: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncSliceParameterBufferH264 {
    pub macroblock_address: u32,
    pub num_macroblocks: u32,
    pub macroblock_info: VaId,
    pub slice_type: u8,
    pub pic_parameter_set_id: u8,
    pub idr_pic_id: u16,
    pub pic_order_cnt_lsb: u16,
    pub delta_pic_order_cnt_bottom: i32,
    pub delta_pic_order_cnt: [i32; 2],
    pub direct_spatial_mv_pred_flag: u8,
    pub num_ref_idx_active_override_flag: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub ref_pic_list0: [VAPictureH264; 32],
    pub ref_pic_list1: [VAPictureH264; 32],
    pub luma_log2_weight_denom: u8,
    pub chroma_log2_weight_denom: u8,
    pub luma_weight_l0_flag: u8,
    pub luma_weight_l0: [i16; 32],
    pub luma_offset_l0: [i16; 32],
    pub chroma_weight_l0_flag: u8,
    pub chroma_weight_l0: [[i16; 2]; 32],
    pub chroma_offset_l0: [[i16; 2]; 32],
    pub luma_weight_l1_flag: u8,
    pub luma_weight_l1: [i16; 32],
    pub luma_offset_l1: [i16; 32],
    pub chroma_weight_l1_flag: u8,
    pub chroma_weight_l1: [[i16; 2]; 32],
    pub chroma_offset_l1: [[i16; 2]; 32],
    pub cabac_init_idc: u8,
    pub slice_qp_delta: i8,
    pub disable_deblocking_filter_idc: u8,
    pub slice_alpha_c0_offset_div2: i8,
    pub slice_beta_offset_div2: i8,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAPictureHEVC {
    pub picture_id: VaId,
    pub pic_order_cnt: i32,
    pub flags: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncSequenceParameterBufferHEVC {
    pub general_profile_idc: u8,
    pub general_level_idc: u8,
    pub general_tier_flag: u8,
    pub intra_period: u32,
    pub intra_idr_period: u32,
    pub ip_period: u32,
    pub bits_per_second: u32,
    pub pic_width_in_luma_samples: u16,
    pub pic_height_in_luma_samples: u16,
    pub seq_fields: u32,
    pub log2_min_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_luma_coding_block_size: u8,
    pub log2_min_transform_block_size_minus2: u8,
    pub log2_diff_max_min_transform_block_size: u8,
    pub max_transform_hierarchy_depth_inter: u8,
    pub max_transform_hierarchy_depth_intra: u8,
    pub pcm_sample_bit_depth_luma_minus1: u32,
    pub pcm_sample_bit_depth_chroma_minus1: u32,
    pub log2_min_pcm_luma_coding_block_size_minus3: u32,
    pub log2_max_pcm_luma_coding_block_size_minus3: u32,
    pub vui_parameters_present_flag: u8,
    pub vui_fields: u32,
    pub aspect_ratio_idc: u8,
    pub sar_width: u32,
    pub sar_height: u32,
    pub vui_num_units_in_tick: u32,
    pub vui_time_scale: u32,
    pub min_spatial_segmentation_idc: u16,
    pub max_bytes_per_pic_denom: u8,
    pub max_bits_per_min_cu_denom: u8,
    pub scc_fields: u32,
    pub va_reserved: [u32; 7],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncPictureParameterBufferHEVC {
    pub decoded_curr_pic: VAPictureHEVC,
    pub reference_frames: [VAPictureHEVC; 15],
    pub coded_buf: VaId,
    pub collocated_ref_pic_index: u8,
    pub last_picture: u8,
    pub pic_init_qp: u8,
    pub diff_cu_qp_delta_depth: u8,
    pub pps_cb_qp_offset: i8,
    pub pps_cr_qp_offset: i8,
    pub num_tile_columns_minus1: u8,
    pub num_tile_rows_minus1: u8,
    pub column_width_minus1: [u8; 19],
    pub row_height_minus1: [u8; 21],
    pub log2_parallel_merge_level_minus2: u8,
    pub ctu_max_bitsize_allowed: u8,
    pub num_ref_idx_l0_default_active_minus1: u8,
    pub num_ref_idx_l1_default_active_minus1: u8,
    pub slice_pic_parameter_set_id: u8,
    pub nal_unit_type: u8,
    pub pic_fields: u32,
    pub hierarchical_level_plus1: u8,
    pub va_byte_reserved: u8,
    pub scc_fields: u16,
    pub va_reserved: [u32; 15],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncSliceParameterBufferHEVC {
    pub slice_segment_address: u32,
    pub num_ctu_in_slice: u32,
    pub slice_type: u8,
    pub slice_pic_parameter_set_id: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub ref_pic_list0: [VAPictureHEVC; 15],
    pub ref_pic_list1: [VAPictureHEVC; 15],
    pub luma_log2_weight_denom: u8,
    pub delta_chroma_log2_weight_denom: i8,
    pub delta_luma_weight_l0: [i8; 15],
    pub luma_offset_l0: [i8; 15],
    pub delta_chroma_weight_l0: [[i8; 2]; 15],
    pub chroma_offset_l0: [[i8; 2]; 15],
    pub delta_luma_weight_l1: [i8; 15],
    pub luma_offset_l1: [i8; 15],
    pub delta_chroma_weight_l1: [[i8; 2]; 15],
    pub chroma_offset_l1: [[i8; 2]; 15],
    pub max_num_merge_cand: u8,
    pub slice_qp_delta: i8,
    pub slice_cb_qp_offset: i8,
    pub slice_cr_qp_offset: i8,
    pub slice_beta_offset_div2: i8,
    pub slice_tc_offset_div2: i8,
    pub slice_fields: u32,
    pub pred_weight_table_bit_offset: u32,
    pub pred_weight_table_bit_length: u32,
    pub va_reserved: [u32; 6],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncSequenceParameterBufferAV1 {
    pub seq_profile: u8,
    pub seq_level_idx: u8,
    pub seq_tier: u8,
    pub hierarchical_flag: u8,
    pub intra_period: u32,
    pub ip_period: u32,
    pub bits_per_second: u32,
    pub seq_fields: u32,
    pub order_hint_bits_minus_1: u8,
    pub va_reserved: [u32; 16],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncSegParamAV1 {
    pub seg_flags: u8,
    pub segment_number: u8,
    pub feature_data: [[i16; 8]; 8],
    pub feature_mask: [u8; 8],
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncWarpedMotionParamsAV1 {
    pub wmtype: c_int,
    pub wmmat: [i32; 8],
    pub invalid: u8,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncPictureParameterBufferAV1 {
    pub frame_width_minus_1: u16,
    pub frame_height_minus_1: u16,
    pub reconstructed_frame: VaId,
    pub coded_buf: VaId,
    pub reference_frames: [VaId; 8],
    pub ref_frame_idx: [u8; 7],
    pub hierarchical_level_plus1: u8,
    pub primary_ref_frame: u8,
    pub order_hint: u8,
    pub refresh_frame_flags: u8,
    pub reserved8bits1: u8,
    pub ref_frame_ctrl_l0: u32,
    pub ref_frame_ctrl_l1: u32,
    pub picture_flags: u32,
    pub seg_id_block_size: u8,
    pub num_tile_groups_minus1: u8,
    pub temporal_id: u8,
    pub filter_level: [u8; 2],
    pub filter_level_u: u8,
    pub filter_level_v: u8,
    pub loop_filter_flags: u8,
    pub superres_scale_denominator: u8,
    pub interpolation_filter: u8,
    pub ref_deltas: [i8; 8],
    pub mode_deltas: [i8; 2],
    pub base_qindex: u8,
    pub y_dc_delta_q: i8,
    pub u_dc_delta_q: i8,
    pub u_ac_delta_q: i8,
    pub v_dc_delta_q: i8,
    pub v_ac_delta_q: i8,
    pub min_base_qindex: u8,
    pub max_base_qindex: u8,
    pub qmatrix_flags: u16,
    pub reserved16bits1: u16,
    pub mode_control_flags: u32,
    pub segments: VAEncSegParamAV1,
    pub tile_cols: u8,
    pub tile_rows: u8,
    pub reserved16bits2: u16,
    pub width_in_sbs_minus_1: [u16; 63],
    pub height_in_sbs_minus_1: [u16; 63],
    pub context_update_tile_id: u16,
    pub cdef_damping_minus_3: u8,
    pub cdef_bits: u8,
    pub cdef_y_strengths: [u8; 8],
    pub cdef_uv_strengths: [u8; 8],
    pub loop_restoration_flags: u16,
    pub wm: [VAEncWarpedMotionParamsAV1; 7],
    pub bit_offset_qindex: u32,
    pub bit_offset_segmentation: u32,
    pub bit_offset_loopfilter_params: u32,
    pub bit_offset_cdef_params: u32,
    pub size_in_bits_cdef_params: u32,
    pub byte_offset_frame_hdr_obu_size: u32,
    pub size_in_bits_frame_hdr_obu: u32,
    pub tile_group_obu_hdr_info: u8,
    pub number_skip_frames: u8,
    pub reserved16bits3: u16,
    pub skip_frames_reduced_size: i32,
    pub va_reserved: [u32; 16],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncTileGroupBufferAV1 {
    pub tg_start: u8,
    pub tg_end: u8,
    pub va_reserved: [u32; 4],
}

/// A parameter struct with every byte zero, which is how libva wants unused fields.
pub fn zeroed<T: Copy>() -> T {
    unsafe { std::mem::zeroed() }
}

/// A parameter struct's bytes, for `vaCreateBuffer`.
pub fn bytes<T: Copy>(t: &T) -> Vec<u8> {
    unsafe { std::slice::from_raw_parts(t as *const T as *const u8, std::mem::size_of::<T>()) }
        .to_vec()
}

/// Reads a parameter struct back from its bytes.
#[cfg(test)]
pub fn read<T: Copy>(b: &[u8]) -> T {
    assert_eq!(b.len(), std::mem::size_of::<T>());
    unsafe { std::ptr::read_unaligned(b.as_ptr() as *const T) }
}

/// libva's entry points.
pub struct Va {
    _va: Library,
    _drm: Library,
    pub get_display_drm: unsafe extern "C" fn(c_int) -> VADisplay,
    pub initialize: unsafe extern "C" fn(VADisplay, *mut c_int, *mut c_int) -> VAStatus,
    pub terminate: unsafe extern "C" fn(VADisplay) -> VAStatus,
    pub error_str: unsafe extern "C" fn(VAStatus) -> *const c_char,
    pub max_num_profiles: unsafe extern "C" fn(VADisplay) -> c_int,
    pub max_num_entrypoints: unsafe extern "C" fn(VADisplay) -> c_int,
    pub query_profiles: unsafe extern "C" fn(VADisplay, *mut c_int, *mut c_int) -> VAStatus,
    pub query_entrypoints:
        unsafe extern "C" fn(VADisplay, c_int, *mut c_int, *mut c_int) -> VAStatus,
    pub get_config_attributes:
        unsafe extern "C" fn(VADisplay, c_int, c_int, *mut VAConfigAttrib, c_int) -> VAStatus,
    pub vendor: unsafe extern "C" fn(VADisplay) -> *const c_char,
    pub create_config: unsafe extern "C" fn(
        VADisplay,
        c_int,
        c_int,
        *mut VAConfigAttrib,
        c_int,
        *mut VaId,
    ) -> VAStatus,
    pub destroy_config: unsafe extern "C" fn(VADisplay, VaId) -> VAStatus,
    pub create_surfaces: unsafe extern "C" fn(
        VADisplay,
        c_uint,
        c_uint,
        c_uint,
        *mut VaId,
        c_uint,
        *mut VASurfaceAttrib,
        c_uint,
    ) -> VAStatus,
    pub destroy_surfaces: unsafe extern "C" fn(VADisplay, *mut VaId, c_int) -> VAStatus,
    pub create_context: unsafe extern "C" fn(
        VADisplay,
        VaId,
        c_int,
        c_int,
        c_int,
        *mut VaId,
        c_int,
        *mut VaId,
    ) -> VAStatus,
    pub destroy_context: unsafe extern "C" fn(VADisplay, VaId) -> VAStatus,
    pub create_buffer: unsafe extern "C" fn(
        VADisplay,
        VaId,
        c_int,
        c_uint,
        c_uint,
        *mut c_void,
        *mut VaId,
    ) -> VAStatus,
    pub destroy_buffer: unsafe extern "C" fn(VADisplay, VaId) -> VAStatus,
    pub map_buffer: unsafe extern "C" fn(VADisplay, VaId, *mut *mut c_void) -> VAStatus,
    pub unmap_buffer: unsafe extern "C" fn(VADisplay, VaId) -> VAStatus,
    pub begin_picture: unsafe extern "C" fn(VADisplay, VaId, VaId) -> VAStatus,
    pub render_picture: unsafe extern "C" fn(VADisplay, VaId, *mut VaId, c_int) -> VAStatus,
    pub end_picture: unsafe extern "C" fn(VADisplay, VaId) -> VAStatus,
    pub sync_surface: unsafe extern "C" fn(VADisplay, VaId) -> VAStatus,
    pub derive_image: unsafe extern "C" fn(VADisplay, VaId, *mut VAImage) -> VAStatus,
    pub create_image:
        unsafe extern "C" fn(VADisplay, *mut VAImageFormat, c_int, c_int, *mut VAImage) -> VAStatus,
    pub put_image: unsafe extern "C" fn(
        VADisplay,
        VaId,
        VaId,
        c_int,
        c_int,
        c_uint,
        c_uint,
        c_int,
        c_int,
        c_uint,
        c_uint,
    ) -> VAStatus,
    pub destroy_image: unsafe extern "C" fn(VADisplay, VaId) -> VAStatus,
}

impl Va {
    pub fn load() -> Result<Va, String> {
        unsafe {
            let va = Library::new("libva.so.2").map_err(|e| format!("libva.so.2: {e}"))?;
            let drm = Library::new("libva-drm.so.2").map_err(|e| format!("libva-drm.so.2: {e}"))?;
            macro_rules! sym {
                ($lib:expr, $name:literal) => {
                    *$lib
                        .get(concat!($name, "\0").as_bytes())
                        .map_err(|e| format!("{}: {e}", $name))?
                };
            }
            Ok(Va {
                get_display_drm: sym!(drm, "vaGetDisplayDRM"),
                initialize: sym!(va, "vaInitialize"),
                terminate: sym!(va, "vaTerminate"),
                error_str: sym!(va, "vaErrorStr"),
                max_num_profiles: sym!(va, "vaMaxNumProfiles"),
                max_num_entrypoints: sym!(va, "vaMaxNumEntrypoints"),
                query_profiles: sym!(va, "vaQueryConfigProfiles"),
                query_entrypoints: sym!(va, "vaQueryConfigEntrypoints"),
                get_config_attributes: sym!(va, "vaGetConfigAttributes"),
                vendor: sym!(va, "vaQueryVendorString"),
                create_config: sym!(va, "vaCreateConfig"),
                destroy_config: sym!(va, "vaDestroyConfig"),
                create_surfaces: sym!(va, "vaCreateSurfaces"),
                destroy_surfaces: sym!(va, "vaDestroySurfaces"),
                create_context: sym!(va, "vaCreateContext"),
                destroy_context: sym!(va, "vaDestroyContext"),
                create_buffer: sym!(va, "vaCreateBuffer"),
                destroy_buffer: sym!(va, "vaDestroyBuffer"),
                map_buffer: sym!(va, "vaMapBuffer"),
                unmap_buffer: sym!(va, "vaUnmapBuffer"),
                begin_picture: sym!(va, "vaBeginPicture"),
                render_picture: sym!(va, "vaRenderPicture"),
                end_picture: sym!(va, "vaEndPicture"),
                sync_surface: sym!(va, "vaSyncSurface"),
                derive_image: sym!(va, "vaDeriveImage"),
                create_image: sym!(va, "vaCreateImage"),
                put_image: sym!(va, "vaPutImage"),
                destroy_image: sym!(va, "vaDestroyImage"),
                _va: va,
                _drm: drm,
            })
        }
    }

    pub fn describe(&self, r: VAStatus) -> String {
        let s = unsafe { (self.error_str)(r) };
        if s.is_null() {
            return format!("status {r}");
        }
        let n = unsafe { libc::strnlen(s, 256) };
        let msg = unsafe { std::slice::from_raw_parts(s as *const u8, n) };
        format!("{} ({r})", String::from_utf8_lossy(msg))
    }
}
