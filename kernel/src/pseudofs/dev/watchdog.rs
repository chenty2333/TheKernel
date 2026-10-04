//! Linux watchdog OFD policy; hardware lives below axdriver. No boot opt-in,
//! no device node. Magic close is per OFD, not per duplicated descriptor.
use alloc::{borrow::Cow, sync::Arc};
use core::{
    any::Any,
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::{FsPath, Location, NodeFlags, VfsError, VfsResult};
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable};

use crate::{
    file::{FileLike, IoSrc, IoctlContext, Kstat},
    pseudofs::{DeviceOpen, DeviceOps},
};
static OPEN: AtomicBool = AtomicBool::new(false);
pub(crate) struct Watchdog;
struct Handle {
    location: Location,
    magic: AtomicBool,
    closed: AtomicBool,
    nonblocking: AtomicBool,
}
fn hw<T>(result: Result<T, impl core::fmt::Debug>) -> AxResult<T> {
    result.map_err(|_| AxError::Io)
}
fn read_int(cx: &IoctlContext, arg: usize) -> AxResult<u32> {
    let mut bytes = [0; 4];
    cx.user_memory()
        .read_into(arg as *const u8, &mut bytes)
        .map_err(crate::mm::map_usercopy_error)?;
    Ok(u32::from_ne_bytes(bytes))
}
fn write_int(cx: &IoctlContext, arg: usize, value: u32) -> AxResult<usize> {
    cx.user_memory()
        .write_bytes(arg, &value.to_ne_bytes())
        .map_err(crate::mm::map_usercopy_error)?;
    Ok(0)
}
impl Handle {
    fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        if self.magic.load(Ordering::Acquire) {
            let _ = axdriver::itco::set_enabled(false);
        } else {
            let _ = axdriver::itco::keepalive();
        }
        OPEN.store(false, Ordering::Release);
    }
}
impl Drop for Handle {
    fn drop(&mut self) {
        self.close();
    }
}
impl FileLike for Handle {
    fn set_nonblocking(&self, value: bool) -> AxResult {
        self.nonblocking.store(value, Ordering::Release);
        Ok(())
    }
    fn nonblocking(&self) -> bool {
        self.nonblocking.load(Ordering::Acquire)
    }
    fn uring_cmd_manifest(&self) -> &'static [crate::file::UringCmdManifest] {
        &[]
    }
    fn final_close(&self) {
        self.close();
    }
    fn stat(&self) -> AxResult<Kstat> {
        crate::file::fs::location_to_kstat(&self.location)
    }
    fn path(&self) -> AxResult<Cow<'_, FsPath>> {
        Ok(Cow::Borrowed(FsPath::new(b"/dev/watchdog")))
    }
    fn write(&self, source: &mut IoSrc) -> AxResult<usize> {
        let count = source.remaining();
        if count == 0 {
            return Ok(0);
        }
        let mut magic = false;
        let mut bytes = [0; 256];
        while source.remaining() > 0 {
            let n = source.remaining().min(bytes.len());
            source.read_exact(&mut bytes[..n])?;
            magic |= bytes[..n].contains(&b'V');
        }
        self.magic.store(magic, Ordering::Release);
        hw(axdriver::itco::keepalive())?;
        Ok(count)
    }
    fn ioctl(&self, cx: &IoctlContext, cmd: u32, arg: usize) -> AxResult<usize> {
        let (version, timeout, boot, _) = axdriver::itco::info().ok_or(AxError::NoSuchDevice)?;
        match cmd {
            0x80285700 => {
                let mut info = [0u8; 40];
                info[..4].copy_from_slice(&0x8180u32.to_ne_bytes());
                info[4..8].copy_from_slice(&version.to_ne_bytes());
                info[8..22].copy_from_slice(b"TheKernel iTCO");
                cx.user_memory()
                    .write_bytes(arg, &info)
                    .map_err(crate::mm::map_usercopy_error)?;
                Ok(0)
            }
            0x80045701 => write_int(cx, arg, 0),
            0x80045702 => write_int(cx, arg, if boot { 0x20 } else { 0 }),
            0x80045704 => {
                let value = read_int(cx, arg)?;
                if value == 0 || value & !3 != 0 {
                    return Err(AxError::InvalidInput);
                }
                if value & 1 != 0 {
                    hw(axdriver::itco::set_enabled(false))?;
                }
                if value & 2 != 0 {
                    hw(axdriver::itco::set_enabled(true))?;
                }
                Ok(0)
            }
            0x80045705 => {
                hw(axdriver::itco::keepalive())?;
                Ok(0)
            }
            0xc0045706 => {
                let value = read_int(cx, arg)?;
                if !(3..=614).contains(&value) {
                    return Err(AxError::InvalidInput);
                }
                hw(axdriver::itco::set_timeout(value))?;
                write_int(cx, arg, value)
            }
            0x80045707 => write_int(cx, arg, timeout),
            0x8004570a => write_int(cx, arg, hw(axdriver::itco::time_left())?),
            _ => Err(AxError::NotATty),
        }
    }
}
impl Pollable for Handle {
    fn poll(&self) -> IoEvents {
        IoEvents::WRITABLE
    }
    fn register<'a>(
        &'a self,
        _cx: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        PollRegistration::empty()
    }
}
impl DeviceOps for Watchdog {
    fn open_description(&self, location: &Location, flags: u32) -> VfsResult<Option<DeviceOpen>> {
        if !axdriver::itco::available() {
            return Err(VfsError::NoSuchDevice);
        }
        if OPEN
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(VfsError::ResourceBusy);
        }
        let handle = match Arc::try_new(Handle {
            location: location.clone(),
            magic: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            nonblocking: AtomicBool::new(flags & linux_raw_sys::general::O_NONBLOCK != 0),
        }) {
            Ok(handle) => handle,
            Err(_) => {
                OPEN.store(false, Ordering::Release);
                return Err(VfsError::NoMemory);
            }
        };
        if let Err(error) = hw(axdriver::itco::set_enabled(true)) {
            drop(handle);
            return Err(error);
        }
        Ok(Some(DeviceOpen::new(handle, None)))
    }
    fn read_at(&self, _: &mut [u8], _: u64) -> VfsResult<usize> {
        Err(VfsError::Unsupported)
    }
    fn write_at(&self, _: &[u8], _: u64) -> VfsResult<usize> {
        Err(VfsError::Unsupported)
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn flags(&self) -> NodeFlags {
        NodeFlags::STREAM | NodeFlags::NON_CACHEABLE | NodeFlags::NO_SEEK
    }
}
