//! The VideoToolbox, CoreMedia, CoreVideo and CoreFoundation calls the encoder makes
//! beyond those in `mac::ffi`.

#![allow(non_upper_case_globals, non_snake_case)]

pub use crate::mac::ffi::{
    cf_string, kCFNumberDoubleType, kCFNumberSInt64Type,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, CFArrayGetCount, CFArrayGetValueAtIndex,
    CFArrayRef, CFDictionaryGetValue, CFDictionaryRef, CFNumberGetValue, CFRelease, CFRetain,
    CFStringRef, CFTypeRef, CMSampleBufferGetFormatDescription,
    CMSampleBufferGetSampleAttachmentsArray, CMSampleBufferRef, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferLockBaseAddress, CVPixelBufferRef,
    CVPixelBufferUnlockBaseAddress, OSStatus,
};
use std::ffi::c_void;

pub type VTCompressionSessionRef = *mut c_void;
pub type CVPixelBufferPoolRef = *mut c_void;
pub type CMBlockBufferRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CMTime {
    pub value: i64,
    pub timescale: i32,
    pub flags: u32,
    pub epoch: i64,
}
pub const kCMTimeFlags_Valid: u32 = 1;
pub const kCMTimeInvalid: CMTime = CMTime {
    value: 0,
    timescale: 0,
    flags: 0,
    epoch: 0,
};

pub const kCMVideoCodecType_H264: u32 = u32::from_be_bytes(*b"avc1");
pub const kCMVideoCodecType_HEVC: u32 = u32::from_be_bytes(*b"hvc1");
pub const kCMVideoCodecType_AV1: u32 = u32::from_be_bytes(*b"av01");
pub const kVTEncodeInfo_FrameDropped: u32 = 1 << 1;
pub const kCFNumberSInt32Type: isize = 3;

pub type VTCompressionOutputCallback = unsafe extern "C" fn(
    refcon: *mut c_void,
    frame_refcon: *mut c_void,
    status: OSStatus,
    flags: u32,
    sample: CMSampleBufferRef,
);

#[link(name = "VideoToolbox", kind = "framework")]
extern "C" {
    pub fn VTCompressionSessionCreate(
        allocator: CFTypeRef,
        width: i32,
        height: i32,
        codec: u32,
        encoder_spec: CFDictionaryRef,
        source_attrs: CFDictionaryRef,
        data_allocator: CFTypeRef,
        callback: Option<VTCompressionOutputCallback>,
        refcon: *mut c_void,
        out: *mut VTCompressionSessionRef,
    ) -> OSStatus;
    pub fn VTCompressionSessionInvalidate(s: VTCompressionSessionRef);
    pub fn VTCompressionSessionGetPixelBufferPool(
        s: VTCompressionSessionRef,
    ) -> CVPixelBufferPoolRef;
    pub fn VTCompressionSessionPrepareToEncodeFrames(s: VTCompressionSessionRef) -> OSStatus;
    pub fn VTCompressionSessionEncodeFrame(
        s: VTCompressionSessionRef,
        image: CVPixelBufferRef,
        pts: CMTime,
        duration: CMTime,
        frame_props: CFDictionaryRef,
        frame_refcon: *mut c_void,
        info_out: *mut u32,
    ) -> OSStatus;
    pub fn VTCompressionSessionCompleteFrames(
        s: VTCompressionSessionRef,
        until: CMTime,
    ) -> OSStatus;
    pub fn VTSessionSetProperty(s: CFTypeRef, key: CFStringRef, value: CFTypeRef) -> OSStatus;
    pub fn VTSessionCopyProperty(
        s: CFTypeRef,
        key: CFStringRef,
        allocator: CFTypeRef,
        out: *mut CFTypeRef,
    ) -> OSStatus;
    pub fn VTCopyVideoEncoderList(options: CFDictionaryRef, out: *mut CFArrayRef) -> OSStatus;

    pub static kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: CFStringRef;
    pub static kVTVideoEncoderSpecification_EnableLowLatencyRateControl: CFStringRef;
    pub static kVTCompressionPropertyKey_RealTime: CFStringRef;
    pub static kVTCompressionPropertyKey_AllowFrameReordering: CFStringRef;
    pub static kVTCompressionPropertyKey_ProfileLevel: CFStringRef;
    pub static kVTCompressionPropertyKey_AverageBitRate: CFStringRef;
    pub static kVTCompressionPropertyKey_ExpectedFrameRate: CFStringRef;
    pub static kVTCompressionPropertyKey_ColorPrimaries: CFStringRef;
    pub static kVTCompressionPropertyKey_TransferFunction: CFStringRef;
    pub static kVTCompressionPropertyKey_YCbCrMatrix: CFStringRef;
    pub static kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder: CFStringRef;
    pub static kVTCompressionPropertyKey_EncoderID: CFStringRef;
    pub static kVTProfileLevel_H264_High_AutoLevel: CFStringRef;
    pub static kVTProfileLevel_HEVC_Main_AutoLevel: CFStringRef;
    pub static kVTEncodeFrameOptionKey_ForceKeyFrame: CFStringRef;
    pub static kVTVideoEncoderList_CodecType: CFStringRef;
    pub static kVTVideoEncoderList_EncoderID: CFStringRef;
    pub static kVTVideoEncoderList_IsHardwareAccelerated: CFStringRef;
}

#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    pub fn CMSampleBufferGetDataBuffer(sb: CMSampleBufferRef) -> CMBlockBufferRef;
    pub fn CMBlockBufferGetDataLength(bb: CMBlockBufferRef) -> usize;
    pub fn CMBlockBufferCopyDataBytes(
        bb: CMBlockBufferRef,
        offset: usize,
        len: usize,
        dst: *mut c_void,
    ) -> OSStatus;
    pub fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
        desc: CFTypeRef,
        index: usize,
        ptr_out: *mut *const u8,
        size_out: *mut usize,
        count_out: *mut usize,
        nal_header_len_out: *mut i32,
    ) -> OSStatus;
    pub fn CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
        desc: CFTypeRef,
        index: usize,
        ptr_out: *mut *const u8,
        size_out: *mut usize,
        count_out: *mut usize,
        nal_header_len_out: *mut i32,
    ) -> OSStatus;
    pub static kCMSampleAttachmentKey_NotSync: CFStringRef;
}

#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    pub fn CVPixelBufferPoolCreatePixelBuffer(
        allocator: CFTypeRef,
        pool: CVPixelBufferPoolRef,
        out: *mut CVPixelBufferRef,
    ) -> i32;
    pub fn CVPixelBufferGetPlaneCount(pb: CVPixelBufferRef) -> usize;
    pub fn CVPixelBufferGetWidthOfPlane(pb: CVPixelBufferRef, plane: usize) -> usize;
    pub fn CVPixelBufferGetHeightOfPlane(pb: CVPixelBufferRef, plane: usize) -> usize;
    pub static kCVPixelBufferPixelFormatTypeKey: CFStringRef;
    pub static kCVPixelBufferWidthKey: CFStringRef;
    pub static kCVPixelBufferHeightKey: CFStringRef;
    pub static kCVPixelBufferIOSurfacePropertiesKey: CFStringRef;
    pub static kCVImageBufferColorPrimaries_ITU_R_709_2: CFStringRef;
    pub static kCVImageBufferTransferFunction_sRGB: CFStringRef;
    pub static kCVImageBufferYCbCrMatrix_ITU_R_601_4: CFStringRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    pub fn CFDictionaryCreate(
        allocator: CFTypeRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        n: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFDictionaryRef;
    pub fn CFNumberCreate(allocator: CFTypeRef, ty: isize, value: *const c_void) -> CFTypeRef;
    pub fn CFBooleanGetValue(b: CFTypeRef) -> bool;
    pub fn CFGetTypeID(cf: CFTypeRef) -> usize;
    pub fn CFBooleanGetTypeID() -> usize;
    pub fn CFNumberGetTypeID() -> usize;
    pub fn CFStringGetTypeID() -> usize;
    pub fn CFDictionaryGetTypeID() -> usize;
    pub static kCFBooleanTrue: CFTypeRef;
    pub static kCFBooleanFalse: CFTypeRef;
    pub static kCFTypeDictionaryKeyCallBacks: [usize; 6];
    pub static kCFTypeDictionaryValueCallBacks: [usize; 5];
}
