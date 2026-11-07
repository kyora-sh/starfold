//! The NVENC calls an encode session makes, behind a trait so the session logic runs
//! against a fake on hosts without an NVIDIA GPU.

use super::ffi::{self, Api, Config, Cuda, Enc, Guid, InitParams};
use crate::encoder::hw::write_nv12;
use crate::encoder::{EncodeError, FrameView};
use std::ffi::{c_int, c_void};
use std::ptr;

pub trait Driver: Send {
    /// The P4 preset's configuration for `codec`, tuned for ultra-low latency.
    fn preset(&mut self, codec: Guid) -> Result<Box<Config>, EncodeError>;
    /// Initializes the encoder and allocates its NV12 input and bitstream buffers at the
    /// encode size.
    fn initialize(&mut self, init: &InitParams, cfg: &Config) -> Result<(), EncodeError>;
    fn reconfigure(
        &mut self,
        init: &InitParams,
        cfg: &Config,
        flags: u32,
    ) -> Result<(), EncodeError>;
    /// Uploads `f`, encodes it with the picture `flags` and returns its bitstream.
    fn encode(&mut self, f: &FrameView<'_>, flags: u32) -> Result<Vec<u8>, EncodeError>;
}

/// An encode session on its own CUDA context.
pub struct NvDriver {
    api: Api,
    cuda: Cuda,
    ctx: *mut c_void,
    enc: Enc,
    input: *mut c_void,
    output: *mut c_void,
    size: (u32, u32),
    frame: u32,
}

unsafe impl Send for NvDriver {}

impl NvDriver {
    pub fn open(device: c_int) -> Result<NvDriver, EncodeError> {
        let api = Api::load().map_err(EncodeError)?;
        let cuda = Cuda::load().map_err(EncodeError)?;
        if !(0..cuda.count().map_err(EncodeError)?).contains(&device) {
            return Err(EncodeError(format!("no CUDA device {device}")));
        }
        let ctx = cuda.context(device).map_err(EncodeError)?;
        let mut d = NvDriver {
            api,
            cuda,
            ctx,
            enc: ptr::null_mut(),
            input: ptr::null_mut(),
            output: ptr::null_mut(),
            size: (0, 0),
            frame: 0,
        };
        d.enc = d.api.open(ctx).map_err(EncodeError)?;
        let nv = &d.api.nv;
        let complete = nv.initialize_encoder.is_some()
            && nv.create_input_buffer.is_some()
            && nv.destroy_input_buffer.is_some()
            && nv.create_bitstream_buffer.is_some()
            && nv.destroy_bitstream_buffer.is_some()
            && nv.encode_picture.is_some()
            && nv.lock_bitstream.is_some()
            && nv.unlock_bitstream.is_some()
            && nv.lock_input_buffer.is_some()
            && nv.unlock_input_buffer.is_some()
            && nv.reconfigure_encoder.is_some()
            && nv.get_encode_preset_config_ex.is_some();
        if !complete {
            return Err(EncodeError(
                "the driver's function list is incomplete".into(),
            ));
        }
        Ok(d)
    }

    fn check(&self, what: &str, r: ffi::Status) -> Result<(), EncodeError> {
        if r == 0 {
            return Ok(());
        }
        let detail = self
            .api
            .nv
            .get_last_error_string
            .map(|f| unsafe { f(self.enc) })
            .filter(|s| !s.is_null())
            .map(|s| unsafe { ffi::c_string(s, 512) })
            .filter(|s| !s.is_empty())
            .map(|s| format!(": {s}"))
            .unwrap_or_default();
        Err(EncodeError(format!(
            "{what} failed: {} ({r}){detail}",
            ffi::status_name(r)
        )))
    }

    fn upload(&mut self, f: &FrameView<'_>) -> Result<u32, EncodeError> {
        let nv = &self.api.nv;
        let (lock, unlock) = (
            nv.lock_input_buffer.unwrap(),
            nv.unlock_input_buffer.unwrap(),
        );
        let mut l: Box<ffi::LockInputBuffer> = Box::new(unsafe { std::mem::zeroed() });
        l.version = ffi::LOCK_INPUT_BUFFER_VER;
        l.input_buffer = self.input;
        self.check("nvEncLockInputBuffer", unsafe { lock(self.enc, &mut *l) })?;
        let (w, h) = (self.size.0 as usize, self.size.1 as usize);
        let pitch = l.pitch as usize;
        let r = if l.buffer_data_ptr.is_null() {
            Err(EncodeError(
                "nvEncLockInputBuffer returned no memory".into(),
            ))
        } else if pitch < w {
            Err(EncodeError(format!(
                "nvEncLockInputBuffer pitch {pitch} for a {w}-wide picture"
            )))
        } else {
            let dst = unsafe {
                std::slice::from_raw_parts_mut(
                    l.buffer_data_ptr as *mut u8,
                    pitch * (h + h.div_ceil(2)),
                )
            };
            write_nv12(dst, [pitch, pitch], [0, pitch * h], self.size, f)
        };
        let u = unsafe { unlock(self.enc, self.input) };
        r?;
        self.check("nvEncUnlockInputBuffer", u)?;
        Ok(l.pitch)
    }

    fn bitstream(&mut self) -> Result<Vec<u8>, EncodeError> {
        let nv = &self.api.nv;
        let (lock, unlock) = (nv.lock_bitstream.unwrap(), nv.unlock_bitstream.unwrap());
        let mut l: Box<ffi::LockBitstream> = Box::new(unsafe { std::mem::zeroed() });
        l.version = ffi::LOCK_BITSTREAM_VER;
        l.output_bitstream = self.output;
        self.check("nvEncLockBitstream", unsafe { lock(self.enc, &mut *l) })?;
        let n = l.bitstream_size_in_bytes as usize;
        let data = if l.bitstream_buffer_ptr.is_null() {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(l.bitstream_buffer_ptr as *const u8, n) }.to_vec()
        };
        let u = unsafe { unlock(self.enc, self.output) };
        self.check("nvEncUnlockBitstream", u)?;
        Ok(data)
    }
}

impl Drop for NvDriver {
    fn drop(&mut self) {
        let nv = &self.api.nv;
        unsafe {
            if !self.input.is_null() {
                if let Some(f) = nv.destroy_input_buffer {
                    f(self.enc, self.input);
                }
            }
            if !self.output.is_null() {
                if let Some(f) = nv.destroy_bitstream_buffer {
                    f(self.enc, self.output);
                }
            }
            if !self.enc.is_null() {
                if let Some(f) = nv.destroy_encoder {
                    f(self.enc);
                }
            }
            (self.cuda.ctx_destroy)(self.ctx);
        }
    }
}

impl Driver for NvDriver {
    fn preset(&mut self, codec: Guid) -> Result<Box<Config>, EncodeError> {
        let get = self.api.nv.get_encode_preset_config_ex.unwrap();
        let mut p: Box<ffi::PresetConfig> = Box::new(unsafe { std::mem::zeroed() });
        p.version = ffi::PRESET_CONFIG_VER;
        p.preset_cfg.version = ffi::CONFIG_VER;
        self.check("nvEncGetEncodePresetConfigEx", unsafe {
            get(
                self.enc,
                codec,
                ffi::PRESET_P4,
                ffi::TUNING_ULTRA_LOW_LATENCY,
                &mut *p,
            )
        })?;
        let mut cfg = Box::new(p.preset_cfg);
        cfg.version = ffi::CONFIG_VER;
        Ok(cfg)
    }

    fn initialize(&mut self, init: &InitParams, cfg: &Config) -> Result<(), EncodeError> {
        let nv = &self.api.nv;
        let mut c = Box::new(*cfg);
        let mut i = Box::new(*init);
        i.encode_config = &mut *c;
        self.check("nvEncInitializeEncoder", unsafe {
            nv.initialize_encoder.unwrap()(self.enc, &mut *i)
        })?;
        self.size = (init.encode_width, init.encode_height);
        let mut b: Box<ffi::CreateInputBuffer> = Box::new(unsafe { std::mem::zeroed() });
        b.version = ffi::CREATE_INPUT_BUFFER_VER;
        b.width = init.encode_width;
        b.height = init.encode_height;
        b.buffer_fmt = ffi::FMT_NV12;
        self.check("nvEncCreateInputBuffer", unsafe {
            nv.create_input_buffer.unwrap()(self.enc, &mut *b)
        })?;
        self.input = b.input_buffer;
        let mut o: Box<ffi::CreateBitstreamBuffer> = Box::new(unsafe { std::mem::zeroed() });
        o.version = ffi::CREATE_BITSTREAM_BUFFER_VER;
        self.check("nvEncCreateBitstreamBuffer", unsafe {
            nv.create_bitstream_buffer.unwrap()(self.enc, &mut *o)
        })?;
        self.output = o.bitstream_buffer;
        if self.input.is_null() || self.output.is_null() {
            return Err(EncodeError("NVENC returned no buffer".into()));
        }
        Ok(())
    }

    fn reconfigure(
        &mut self,
        init: &InitParams,
        cfg: &Config,
        flags: u32,
    ) -> Result<(), EncodeError> {
        let mut c = Box::new(*cfg);
        let mut r: Box<ffi::Reconfigure> = Box::new(unsafe { std::mem::zeroed() });
        r.version = ffi::RECONFIGURE_PARAMS_VER;
        r.reinit = *init;
        r.reinit.encode_config = &mut *c;
        r.flags = flags;
        self.check("nvEncReconfigureEncoder", unsafe {
            self.api.nv.reconfigure_encoder.unwrap()(self.enc, &mut *r)
        })
    }

    fn encode(&mut self, f: &FrameView<'_>, flags: u32) -> Result<Vec<u8>, EncodeError> {
        if (f.width, f.height) != self.size {
            return Err(EncodeError(format!(
                "NVENC input is {}x{}, got {}x{}",
                self.size.0, self.size.1, f.width, f.height
            )));
        }
        let pitch = self.upload(f)?;
        let mut p: Box<ffi::PicParams> = Box::new(unsafe { std::mem::zeroed() });
        p.version = ffi::PIC_PARAMS_VER;
        p.input_width = self.size.0;
        p.input_height = self.size.1;
        p.input_pitch = pitch;
        p.encode_pic_flags = flags;
        p.frame_idx = self.frame;
        p.input_time_stamp = self.frame as u64;
        p.input_buffer = self.input;
        p.output_bitstream = self.output;
        p.buffer_fmt = ffi::FMT_NV12;
        p.picture_struct = ffi::PIC_STRUCT_FRAME;
        self.check("nvEncEncodePicture", unsafe {
            self.api.nv.encode_picture.unwrap()(self.enc, &mut *p)
        })?;
        self.frame = self.frame.wrapping_add(1);
        self.bitstream()
    }
}
