//! The libva calls an encode session makes, behind a trait so the session logic runs
//! against a fake on hosts without a GPU.

use super::ffi::{self, VAConfigAttrib, VADisplay, VAImage, Va, VaId};
use crate::encoder::{EncodeError, FrameView};
use std::ffi::{c_int, c_void};
use std::os::fd::AsRawFd;

pub trait Driver: Send {
    fn entrypoints(&mut self, profile: c_int) -> Result<Vec<c_int>, EncodeError>;
    /// Each attribute's value, `None` where the driver doesn't support it.
    fn attribs(
        &mut self,
        profile: c_int,
        entrypoint: c_int,
        kinds: &[c_int],
    ) -> Result<Vec<Option<u32>>, EncodeError>;
    fn create_config(
        &mut self,
        profile: c_int,
        entrypoint: c_int,
        attribs: &[(c_int, u32)],
    ) -> Result<VaId, EncodeError>;
    fn create_surfaces(&mut self, w: u32, h: u32, n: usize) -> Result<Vec<VaId>, EncodeError>;
    fn create_context(
        &mut self,
        config: VaId,
        w: u32,
        h: u32,
        targets: &[VaId],
    ) -> Result<VaId, EncodeError>;
    fn create_buffer(&mut self, ctx: VaId, kind: c_int, data: &[u8]) -> Result<VaId, EncodeError>;
    fn create_coded(&mut self, ctx: VaId, size: usize) -> Result<VaId, EncodeError>;
    /// Copies an I420 picture into an NV12 surface of `size`, repeating the last column
    /// and row into the padding.
    fn upload(
        &mut self,
        surface: VaId,
        size: (u32, u32),
        f: &FrameView<'_>,
    ) -> Result<(), EncodeError>;
    /// Encodes `surface` with `buffers` and waits for it.
    fn encode(&mut self, ctx: VaId, surface: VaId, buffers: &[VaId]) -> Result<(), EncodeError>;
    /// The coded buffer's segments, concatenated.
    fn coded(&mut self, buf: VaId) -> Result<Vec<u8>, EncodeError>;
    fn destroy_buffer(&mut self, buf: VaId);
    fn destroy(&mut self, config: VaId, ctx: VaId, surfaces: &[VaId]);
}

/// Writes `f` as NV12 into an image of `size` with the given pitches and offsets.
pub fn write_nv12(
    dst: &mut [u8],
    pitches: [usize; 2],
    offsets: [usize; 2],
    size: (u32, u32),
    f: &FrameView<'_>,
) -> Result<(), EncodeError> {
    f.check()?;
    let (aw, ah) = (size.0 as usize, size.1 as usize);
    let (w, h) = (f.width as usize, f.height as usize);
    let need = |off: usize, pitch: usize, rows: usize, cols: usize| {
        pitch >= cols && off + (rows.max(1) - 1) * pitch + cols <= dst.len()
    };
    if aw < w
        || ah < h
        || !need(offsets[0], pitches[0], ah, aw)
        || !need(offsets[1], pitches[1], ah.div_ceil(2), aw.div_ceil(2) * 2)
    {
        return Err(EncodeError(format!(
            "NV12 image of {}x{} doesn't hold a {w}x{h} picture",
            aw, ah
        )));
    }
    for r in 0..ah {
        let s = &f.y[r.min(h - 1) * f.stride_y..][..w];
        let d = &mut dst[offsets[0] + r * pitches[0]..][..aw];
        d[..w].copy_from_slice(s);
        d[w..].fill(s[w - 1]);
    }
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    for r in 0..ah.div_ceil(2) {
        let row = r.min(ch - 1) * f.stride_uv;
        let (u, v) = (&f.u[row..][..cw], &f.v[row..][..cw]);
        let d = &mut dst[offsets[1] + r * pitches[1]..][..aw.div_ceil(2) * 2];
        for (i, px) in d.chunks_exact_mut(2).enumerate() {
            let c = i.min(cw - 1);
            px[0] = u[c];
            px[1] = v[c];
        }
    }
    Ok(())
}

/// A render node's VA display.
pub struct VaDriver {
    va: Va,
    dpy: VADisplay,
    _node: std::fs::File,
    /// `vaDeriveImage` failed once: upload through this image and `vaPutImage`.
    staging: Option<VAImage>,
    derive: bool,
}

unsafe impl Send for VaDriver {}

impl VaDriver {
    pub fn open(node: &std::path::Path) -> Result<VaDriver, EncodeError> {
        let va = Va::load().map_err(EncodeError)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(node)
            .map_err(|e| EncodeError(format!("{}: {e}", node.display())))?;
        let dpy = unsafe { (va.get_display_drm)(file.as_raw_fd()) };
        if dpy.is_null() {
            return Err(EncodeError(format!(
                "{}: vaGetDisplayDRM failed",
                node.display()
            )));
        }
        let (mut major, mut minor) = (0, 0);
        let r = unsafe { (va.initialize)(dpy, &mut major, &mut minor) };
        if r != 0 {
            let e = va.describe(r);
            unsafe { (va.terminate)(dpy) };
            return Err(EncodeError(format!("vaInitialize: {e}")));
        }
        Ok(VaDriver {
            va,
            dpy,
            _node: file,
            staging: None,
            derive: true,
        })
    }

    fn check(&self, what: &str, r: ffi::VAStatus) -> Result<(), EncodeError> {
        if r == 0 {
            Ok(())
        } else {
            Err(EncodeError(format!("{what}: {}", self.va.describe(r))))
        }
    }

    /// Maps `image`, writes the picture into it and unmaps it.
    fn fill(
        &mut self,
        image: &VAImage,
        size: (u32, u32),
        f: &FrameView<'_>,
    ) -> Result<(), EncodeError> {
        if image.format.fourcc != ffi::FOURCC_NV12 || image.num_planes < 2 {
            return Err(EncodeError("VA image is not NV12".into()));
        }
        let mut p: *mut c_void = std::ptr::null_mut();
        self.check("vaMapBuffer", unsafe {
            (self.va.map_buffer)(self.dpy, image.buf, &mut p)
        })?;
        if p.is_null() {
            unsafe { (self.va.unmap_buffer)(self.dpy, image.buf) };
            return Err(EncodeError("vaMapBuffer returned no memory".into()));
        }
        let dst = unsafe { std::slice::from_raw_parts_mut(p as *mut u8, image.data_size as usize) };
        let r = write_nv12(
            dst,
            [image.pitches[0] as usize, image.pitches[1] as usize],
            [image.offsets[0] as usize, image.offsets[1] as usize],
            (
                size.0.min(image.width as u32),
                size.1.min(image.height as u32),
            ),
            f,
        );
        unsafe { (self.va.unmap_buffer)(self.dpy, image.buf) };
        r
    }
}

impl Drop for VaDriver {
    fn drop(&mut self) {
        if let Some(img) = self.staging.take() {
            unsafe { (self.va.destroy_image)(self.dpy, img.image_id) };
        }
        unsafe { (self.va.terminate)(self.dpy) };
    }
}

impl Driver for VaDriver {
    fn entrypoints(&mut self, profile: c_int) -> Result<Vec<c_int>, EncodeError> {
        let max = unsafe { (self.va.max_num_entrypoints)(self.dpy) };
        if !(0..=256).contains(&max) {
            return Err(EncodeError(format!("vaMaxNumEntrypoints reports {max}")));
        }
        let mut v = vec![0; max as usize];
        let mut n = 0;
        self.check("vaQueryConfigEntrypoints", unsafe {
            (self.va.query_entrypoints)(self.dpy, profile, v.as_mut_ptr(), &mut n)
        })?;
        v.truncate(n.clamp(0, max) as usize);
        Ok(v)
    }

    fn attribs(
        &mut self,
        profile: c_int,
        entrypoint: c_int,
        kinds: &[c_int],
    ) -> Result<Vec<Option<u32>>, EncodeError> {
        let mut a: Vec<VAConfigAttrib> = kinds
            .iter()
            .map(|&kind| VAConfigAttrib { kind, value: 0 })
            .collect();
        self.check("vaGetConfigAttributes", unsafe {
            (self.va.get_config_attributes)(
                self.dpy,
                profile,
                entrypoint,
                a.as_mut_ptr(),
                a.len() as c_int,
            )
        })?;
        Ok(a.iter()
            .map(|a| (a.value != ffi::ATTRIB_NOT_SUPPORTED).then_some(a.value))
            .collect())
    }

    fn create_config(
        &mut self,
        profile: c_int,
        entrypoint: c_int,
        attribs: &[(c_int, u32)],
    ) -> Result<VaId, EncodeError> {
        let mut a: Vec<VAConfigAttrib> = attribs
            .iter()
            .map(|&(kind, value)| VAConfigAttrib { kind, value })
            .collect();
        let mut id = ffi::INVALID_ID;
        self.check("vaCreateConfig", unsafe {
            (self.va.create_config)(
                self.dpy,
                profile,
                entrypoint,
                a.as_mut_ptr(),
                a.len() as c_int,
                &mut id,
            )
        })?;
        Ok(id)
    }

    fn create_surfaces(&mut self, w: u32, h: u32, n: usize) -> Result<Vec<VaId>, EncodeError> {
        let mut attr = ffi::VASurfaceAttrib {
            kind: ffi::SURFACE_ATTRIB_PIXEL_FORMAT,
            flags: ffi::SURFACE_ATTRIB_SETTABLE,
            value: ffi::VAGenericValue {
                kind: ffi::GENERIC_VALUE_INTEGER,
                value: ffi::FOURCC_NV12 as usize as *mut c_void,
            },
        };
        let mut ids = vec![ffi::INVALID_ID; n];
        self.check("vaCreateSurfaces", unsafe {
            (self.va.create_surfaces)(
                self.dpy,
                ffi::RT_FORMAT_YUV420,
                w,
                h,
                ids.as_mut_ptr(),
                n as u32,
                &mut attr,
                1,
            )
        })?;
        Ok(ids)
    }

    fn create_context(
        &mut self,
        config: VaId,
        w: u32,
        h: u32,
        targets: &[VaId],
    ) -> Result<VaId, EncodeError> {
        let mut t = targets.to_vec();
        let mut id = ffi::INVALID_ID;
        self.check("vaCreateContext", unsafe {
            (self.va.create_context)(
                self.dpy,
                config,
                w as c_int,
                h as c_int,
                ffi::PROGRESSIVE,
                t.as_mut_ptr(),
                t.len() as c_int,
                &mut id,
            )
        })?;
        Ok(id)
    }

    fn create_buffer(&mut self, ctx: VaId, kind: c_int, data: &[u8]) -> Result<VaId, EncodeError> {
        let mut id = ffi::INVALID_ID;
        self.check("vaCreateBuffer", unsafe {
            (self.va.create_buffer)(
                self.dpy,
                ctx,
                kind,
                data.len() as u32,
                1,
                data.as_ptr() as *mut c_void,
                &mut id,
            )
        })?;
        Ok(id)
    }

    fn create_coded(&mut self, ctx: VaId, size: usize) -> Result<VaId, EncodeError> {
        let size = u32::try_from(size).map_err(|_| EncodeError("coded buffer too big".into()))?;
        let mut id = ffi::INVALID_ID;
        self.check("vaCreateBuffer (coded)", unsafe {
            (self.va.create_buffer)(
                self.dpy,
                ctx,
                ffi::BUF_CODED,
                size,
                1,
                std::ptr::null_mut(),
                &mut id,
            )
        })?;
        Ok(id)
    }

    fn upload(
        &mut self,
        surface: VaId,
        size: (u32, u32),
        f: &FrameView<'_>,
    ) -> Result<(), EncodeError> {
        if self.derive {
            let mut img: VAImage = ffi::zeroed();
            let r = unsafe { (self.va.derive_image)(self.dpy, surface, &mut img) };
            if r == 0 {
                let res = self.fill(&img, size, f);
                unsafe { (self.va.destroy_image)(self.dpy, img.image_id) };
                return res;
            }
            self.derive = false;
        }
        let img = match self.staging {
            Some(i) => i,
            None => {
                let mut fmt: ffi::VAImageFormat = ffi::zeroed();
                fmt.fourcc = ffi::FOURCC_NV12;
                fmt.byte_order = 1;
                fmt.bits_per_pixel = 12;
                let mut img: VAImage = ffi::zeroed();
                self.check("vaCreateImage", unsafe {
                    (self.va.create_image)(
                        self.dpy,
                        &mut fmt,
                        size.0 as c_int,
                        size.1 as c_int,
                        &mut img,
                    )
                })?;
                self.staging = Some(img);
                img
            }
        };
        self.fill(&img, size, f)?;
        self.check("vaPutImage", unsafe {
            (self.va.put_image)(
                self.dpy,
                surface,
                img.image_id,
                0,
                0,
                size.0,
                size.1,
                0,
                0,
                size.0,
                size.1,
            )
        })
    }

    fn encode(&mut self, ctx: VaId, surface: VaId, buffers: &[VaId]) -> Result<(), EncodeError> {
        self.check("vaBeginPicture", unsafe {
            (self.va.begin_picture)(self.dpy, ctx, surface)
        })?;
        let mut b = buffers.to_vec();
        let r =
            unsafe { (self.va.render_picture)(self.dpy, ctx, b.as_mut_ptr(), b.len() as c_int) };
        let end = unsafe { (self.va.end_picture)(self.dpy, ctx) };
        self.check("vaRenderPicture", r)?;
        self.check("vaEndPicture", end)?;
        self.check("vaSyncSurface", unsafe {
            (self.va.sync_surface)(self.dpy, surface)
        })
    }

    fn coded(&mut self, buf: VaId) -> Result<Vec<u8>, EncodeError> {
        let mut p: *mut c_void = std::ptr::null_mut();
        self.check("vaMapBuffer (coded)", unsafe {
            (self.va.map_buffer)(self.dpy, buf, &mut p)
        })?;
        let mut out = Vec::new();
        let mut seg = p as *const ffi::VACodedBufferSegment;
        let mut n = 0;
        while !seg.is_null() && n < 1024 {
            let s = unsafe { &*seg };
            if !s.buf.is_null() && s.size > 0 {
                out.extend_from_slice(unsafe {
                    std::slice::from_raw_parts(s.buf as *const u8, s.size as usize)
                });
            }
            seg = s.next as *const ffi::VACodedBufferSegment;
            n += 1;
        }
        unsafe { (self.va.unmap_buffer)(self.dpy, buf) };
        Ok(out)
    }

    fn destroy_buffer(&mut self, buf: VaId) {
        unsafe { (self.va.destroy_buffer)(self.dpy, buf) };
    }

    fn destroy(&mut self, config: VaId, ctx: VaId, surfaces: &[VaId]) {
        unsafe {
            if ctx != ffi::INVALID_ID {
                (self.va.destroy_context)(self.dpy, ctx);
            }
            if !surfaces.is_empty() {
                let mut s = surfaces.to_vec();
                (self.va.destroy_surfaces)(self.dpy, s.as_mut_ptr(), s.len() as c_int);
            }
            if config != ffi::INVALID_ID {
                (self.va.destroy_config)(self.dpy, config);
            }
        }
    }
}
