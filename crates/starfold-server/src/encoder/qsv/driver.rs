//! The oneVPL calls an encode session makes, behind a trait so the session logic runs
//! against a fake on hosts without an Intel GPU.

use super::ffi::{self, Bitstream, EncodeCtrl, FrameSurface1, Status, SyncPoint, VideoParam};
use crate::encoder::FrameView;

/// What went wrong outside a plain status: the call and its status, or a description.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fault {
    Status(&'static str, Status),
    Other(String),
}

/// One session on one runtime. Statuses come back as the runtime gave them.
pub trait Runtime: Send {
    fn query(&mut self, par: &mut VideoParam) -> Status;
    fn init(&mut self, par: &mut VideoParam) -> Status;
    fn reset(&mut self, par: &mut VideoParam) -> Status;
    fn video_param(&mut self, par: &mut VideoParam) -> Status;
    /// A runtime surface holding `f` as NV12 (the session's size, the last column and row
    /// repeated), stamped `ts` (90 kHz). The caller releases it.
    fn surface(&mut self, f: &FrameView<'_>, ts: u64) -> Result<*mut FrameSurface1, Fault>;
    fn release(&mut self, s: *mut FrameSurface1);
    fn encode(
        &mut self,
        ctrl: Option<&mut EncodeCtrl>,
        s: *mut FrameSurface1,
        bs: &mut Bitstream,
        sync: &mut SyncPoint,
    ) -> Status;
    /// Waits at most `wait_ms` for `sync`.
    fn sync(&mut self, sync: SyncPoint, wait_ms: u32) -> Status;
    /// The device stopped answering: closing the session could block, so it is left open.
    fn abandon(&mut self) {}
}

#[cfg(windows)]
pub use real::MfxRuntime;

#[cfg(windows)]
mod real {
    use super::*;
    use crate::encoder::hw::write_nv12;
    use ffi::{InitializationParam, Session};

    /// An encode session on Intel's runtime, closed on drop unless abandoned.
    pub struct MfxRuntime {
        rt: ffi::Runtime,
        session: Session,
        encoder: bool,
        abandoned: bool,
    }

    unsafe impl Send for MfxRuntime {}

    impl MfxRuntime {
        /// A session on the implementation `vendor_impl_id` of `rt` through `acceleration`.
        pub fn open(
            rt: ffi::Runtime,
            acceleration: u32,
            vendor_impl_id: u32,
        ) -> Result<MfxRuntime, String> {
            let par = InitializationParam {
                acceleration_mode: acceleration,
                reserved: [0; 3],
                num_ext_param: 0,
                ext_param: std::ptr::null_mut(),
                vendor_impl_id,
                reserved2: [0; 3],
            };
            let mut session: Session = std::ptr::null_mut();
            let r = unsafe { (rt.initialize)(par, &mut session) };
            if r != ffi::ERR_NONE || session.is_null() {
                return Err(format!(
                    "MFXInitialize failed: {} ({r})",
                    ffi::status_name(r)
                ));
            }
            Ok(MfxRuntime {
                rt,
                session,
                encoder: false,
                abandoned: false,
            })
        }
    }

    impl Drop for MfxRuntime {
        fn drop(&mut self) {
            if self.abandoned {
                return;
            }
            unsafe {
                if self.encoder {
                    (self.rt.encode_close)(self.session);
                }
                (self.rt.close)(self.session);
            }
        }
    }

    impl Runtime for MfxRuntime {
        fn query(&mut self, par: &mut VideoParam) -> Status {
            let p: *mut VideoParam = par;
            unsafe { (self.rt.encode_query)(self.session, p, p) }
        }

        fn init(&mut self, par: &mut VideoParam) -> Status {
            let r = unsafe { (self.rt.encode_init)(self.session, par) };
            if r >= ffi::ERR_NONE {
                self.encoder = true;
            }
            r
        }

        fn reset(&mut self, par: &mut VideoParam) -> Status {
            unsafe { (self.rt.encode_reset)(self.session, par) }
        }

        fn video_param(&mut self, par: &mut VideoParam) -> Status {
            unsafe { (self.rt.encode_get_video_param)(self.session, par) }
        }

        fn surface(&mut self, f: &FrameView<'_>, ts: u64) -> Result<*mut FrameSurface1, Fault> {
            let mut s: *mut FrameSurface1 = std::ptr::null_mut();
            let r = unsafe { (self.rt.get_surface_for_encode)(self.session, &mut s) };
            if r != ffi::ERR_NONE {
                return Err(Fault::Status("MFXMemory_GetSurfaceForEncode", r));
            }
            let fi = unsafe { s.as_ref().map(|s| s.frame_interface) };
            let Some(Some(fi)) = fi.map(|p| unsafe { p.as_ref() }) else {
                return Err(Fault::Other(
                    "MFXMemory_GetSurfaceForEncode returned no surface interface".into(),
                ));
            };
            let (Some(map), Some(unmap)) = (fi.map, fi.unmap) else {
                self.release(s);
                return Err(Fault::Other(
                    "the surface interface has no Map/Unmap".into(),
                ));
            };
            let r = unsafe { map(s, ffi::MAP_WRITE) };
            if r != ffi::ERR_NONE {
                self.release(s);
                return Err(Fault::Status("mfxFrameSurfaceInterface::Map", r));
            }
            let w = unsafe { write(&mut *s, f, ts) };
            let u = unsafe { unmap(s) };
            if let Err(e) = w {
                self.release(s);
                return Err(e);
            }
            if u != ffi::ERR_NONE {
                self.release(s);
                return Err(Fault::Status("mfxFrameSurfaceInterface::Unmap", u));
            }
            Ok(s)
        }

        fn release(&mut self, s: *mut FrameSurface1) {
            unsafe {
                if let Some(Some(release)) = s
                    .as_ref()
                    .and_then(|s| s.frame_interface.as_ref())
                    .map(|fi| fi.release)
                {
                    release(s);
                }
            }
        }

        fn encode(
            &mut self,
            ctrl: Option<&mut EncodeCtrl>,
            s: *mut FrameSurface1,
            bs: &mut Bitstream,
            sync: &mut SyncPoint,
        ) -> Status {
            let ctrl = ctrl.map_or(std::ptr::null_mut(), |c| c as *mut EncodeCtrl);
            unsafe { (self.rt.encode_frame_async)(self.session, ctrl, s, bs, sync) }
        }

        fn sync(&mut self, sync: SyncPoint, wait_ms: u32) -> Status {
            unsafe { (self.rt.sync_operation)(self.session, sync, wait_ms) }
        }

        fn abandon(&mut self) {
            self.abandoned = true;
        }
    }

    /// Writes `f` into the mapped surface `s`.
    unsafe fn write(s: &mut FrameSurface1, f: &FrameView<'_>, ts: u64) -> Result<(), Fault> {
        let d = &mut s.data;
        let pitch = ((d.pitch_high as usize) << 16) | d.pitch_low as usize;
        let (w, h) = (s.info.width as usize, s.info.height as usize);
        if d.y.is_null() || d.uv.is_null() {
            return Err(Fault::Other("the mapped surface has no NV12 planes".into()));
        }
        let (y, uv) = (d.y as usize, d.uv as usize);
        if pitch < w || uv < y + pitch * h {
            return Err(Fault::Other(format!(
                "the mapped surface's planes don't hold a {w}x{h} NV12 picture (pitch {pitch})"
            )));
        }
        let len = uv - y + pitch * h.div_ceil(2);
        let dst = std::slice::from_raw_parts_mut(d.y, len);
        write_nv12(dst, [pitch, pitch], [0, uv - y], (w as u32, h as u32), f)
            .map_err(|e| Fault::Other(e.0))?;
        d.time_stamp = ts;
        Ok(())
    }
}
