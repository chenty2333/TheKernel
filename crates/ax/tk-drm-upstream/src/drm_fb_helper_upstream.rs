// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_fb_helper.c` (batch B2c).

#![allow(non_snake_case, non_upper_case_globals, dead_code, unsafe_code)]
#![allow(clippy::missing_safety_doc)]

use alloc::format;
use core::ffi::{c_char, c_int, c_long, c_ulong, c_void};
use core::mem::offset_of;

use tk_linux_kapi::config::{EBUSY, EINVAL, ENODEV, ENOMEM, EAGAIN, ENOTTY, ERR_PTR, IS_ERR, PTR_ERR};
#[cfg(not(test))]
use tk_linux_kapi::mutex::{mutex_destroy, mutex_init, mutex_lock, mutex_trylock, mutex_unlock};
#[cfg(test)]
unsafe fn mutex_destroy<M>(_mutex: M) {}
#[cfg(test)]
unsafe fn mutex_init<M>(_mutex: M) {}
#[cfg(test)]
unsafe fn mutex_lock<M>(_mutex: M) {}
#[cfg(test)]
unsafe fn mutex_unlock<M>(_mutex: M) {}
#[cfg(test)]
unsafe fn mutex_trylock<M>(_mutex: M) -> bool { true }

use tk_linux_kapi::locks::{spin_lock_init, spin_lock_irqsave_raw, spin_unlock_irqrestore_raw};
use tk_linux_kapi::memory::{kfree, kzalloc};
use tk_linux_kapi::print::{drm_log_at, DrmLogLevel};
use tk_linux_kapi::types::{ListHead, Mutex, Spinlock, WorkStruct};
use tk_linux_kapi::workqueue::{
    cancel_work_sync, flush_work, queue_work, INIT_WORK, system_dfl_wq,
};

use crate::drm_client_types::{DrmClientBuffer, DrmClientDev};
use crate::drm_crtc_types::{DrmModeSet};
use crate::drm_device_types::DrmDevice;
use crate::drm_fb_helper_types::{DrmFbHelper, DrmFbHelperFuncs, DrmFbHelperSurfaceSize};
use crate::drm_format_helper_types::DrmFormatInfo;
use crate::drm_framebuffer_types::DrmFramebuffer;
use crate::drm_gem_types::DrmGemObject;
use crate::drm_modes_types::DrmDisplayMode;
use crate::drm_rect_types::DrmRect;
use crate::fb_types::{
    FbCmap, FbDeferredIo, FbDeferredIoPageRef, FbFixScreenInfo, FbInfo, FbOps,
    FbVarScreenInfo, FBINFO_STATE_RUNNING, FBINFO_STATE_SUSPENDED,
};
use crate::uapi_drm_consts::{
    DRM_FORMAT_C1, DRM_FORMAT_C2, DRM_FORMAT_C4, DRM_FORMAT_INVALID,
    DRM_FORMAT_XRGB8888, DRM_MODE_DPMS_OFF, DRM_MODE_DPMS_ON, DRM_MODE_DPMS_STANDBY,
    DRM_MODE_DPMS_SUSPEND, DRM_MODE_ROTATE_0, DRM_MODE_ROTATE_180, DRM_MODE_ROTATE_270,
    DRM_MODE_ROTATE_90, DRM_MODE_ROTATE_MASK,
};

const DRM_FBDEV_EMULATION: bool = true;
const DRM_FBDEV_OVERALLOC: u32 = 100;
const FB_TYPE_PACKED_PIXELS: u32 = 0;
const FB_VISUAL_TRUECOLOR: u32 = 2;
const FB_VISUAL_PSEUDOCOLOR: u32 = 3;
const FB_ACCEL_NONE: u32 = 0;
const FB_ACTIVATE_NOW: u32 = 0;
const FB_ACTIVATE_KD_TEXT: u32 = 128;
const FB_BLANK_UNBLANK: c_int = 0;
const FB_BLANK_NORMAL: c_int = 1;
const FB_BLANK_HSYNC_SUSPEND: c_int = 2;
const FB_BLANK_VSYNC_SUSPEND: c_int = 3;
const FB_BLANK_POWERDOWN: c_int = 4;
const FB_ROTATE_UR: c_int = 0;
const FB_ROTATE_CW: c_int = 1;
const FB_ROTATE_UD: c_int = 2;
const FB_ROTATE_CCW: c_int = 3;
const DRM_FORMAT_MAX_PLANES: usize = 4;

static mut FB_CONSOLE_LOCK: Mutex = unsafe { core::mem::zeroed() };

unsafe extern "C" {
    fn fb_sys_read(info: *mut FbInfo, buf: *mut c_char, count: usize, ppos: *mut c_long) -> isize;
    fn fb_sys_write(info: *mut FbInfo, buf: *const c_char, count: usize, ppos: *mut c_long) -> isize;
    fn sys_fillrect(info: *mut FbInfo, rect: *const crate::fb_types::FbFillRect);
    fn sys_copyarea(info: *mut FbInfo, area: *const crate::fb_types::FbCopyArea);
    fn sys_imageblit(info: *mut FbInfo, image: *const crate::fb_types::FbImage);
    fn fb_deferred_io_mmap(info: *mut FbInfo, vma: *mut crate::fb_types::VmAreaStruct) -> i32;
}

unsafe fn fb_helper_from_info(info: *mut FbInfo) -> *mut DrmFbHelper {
    if info.is_null() {
        core::ptr::null_mut()
    } else {
        unsafe { (*info).par.cast::<DrmFbHelper>() }
    }
}

unsafe fn fb_helper_resume_worker(work: *mut WorkStruct) {
    if work.is_null() {
        return;
    }
    unsafe {
        let helper = work.cast::<u8>().sub(offset_of!(DrmFbHelper, resume_work)).cast::<DrmFbHelper>();
        if !(*helper).info.is_null() {
            mutex_lock(core::ptr::addr_of_mut!(FB_CONSOLE_LOCK));
            crate::fbmem_upstream::fb_set_suspend((*helper).info, 0);
            mutex_unlock(core::ptr::addr_of_mut!(FB_CONSOLE_LOCK));
        }
    }
}

unsafe fn fb_helper_damage_work(work: *mut WorkStruct) {
    if work.is_null() {
        return;
    }
    unsafe {
        let helper = work.cast::<u8>().sub(offset_of!(DrmFbHelper, damage_work)).cast::<DrmFbHelper>();
        if !(*helper).info.is_null() && (*(*helper).info).state == FBINFO_STATE_RUNNING {
            drm_fb_helper_fb_dirty(helper);
        }
    }
}

unsafe extern "C" fn drm_fb_helper_fb_destroy(info: *mut FbInfo) {
    let helper = unsafe { fb_helper_from_info(info) };
    if !helper.is_null() {
        unsafe { drm_fb_helper_fini(helper) };
    }
}

unsafe extern "C" fn drm_fb_helper_fb_mmap(info: *mut FbInfo, vma: *mut crate::fb_types::VmAreaStruct) -> i32 {
    if info.is_null() || vma.is_null() {
        return -EINVAL;
    }
    unsafe {
        if !(*info).fbdefio.is_null() {
            return fb_deferred_io_mmap(info, vma);
        }
    }
    -ENODEV
}

static DRM_FB_HELPER_FB_OPS: FbOps = FbOps {
    owner: core::ptr::null_mut(),
    fb_open: None,
    fb_release: None,
    fb_read: Some(fb_sys_read),
    fb_write: Some(fb_sys_write),
    fb_check_var: Some(drm_fb_helper_check_var),
    fb_set_par: Some(drm_fb_helper_set_par),
    fb_setcolreg: None,
    fb_setcmap: Some(drm_fb_helper_setcmap),
    fb_blank: Some(drm_fb_helper_blank),
    fb_pan_display: Some(drm_fb_helper_pan_display),
    fb_fillrect: Some(sys_fillrect),
    fb_copyarea: Some(sys_copyarea),
    fb_imageblit: Some(sys_imageblit),
    fb_cursor: None,
    fb_sync: None,
    fb_ioctl: Some(drm_fb_helper_ioctl),
    fb_compat_ioctl: None,
    fb_mmap: Some(drm_fb_helper_fb_mmap),
    fb_get_caps: None,
    fb_destroy: Some(drm_fb_helper_fb_destroy),
};

// upstream: drm_fb_helper.c drm_fb_helper_prepare()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_fb_helper_prepare(
    dev: *mut DrmDevice,
    helper: *mut DrmFbHelper,
    mut preferred_bpp: u32,
    funcs: *mut DrmFbHelperFuncs,
) {
    if dev.is_null() || helper.is_null() {
        return;
    }
    if preferred_bpp == 0 {
        preferred_bpp = 32;
    }
    unsafe {
        spin_lock_init(&mut (*helper).damage_lock);
        INIT_WORK(&mut (*helper).resume_work, |work| unsafe { fb_helper_resume_worker(work) });
        INIT_WORK(&mut (*helper).damage_work, |work| unsafe { fb_helper_damage_work(work) });
        (*helper).damage_clip = crate::uapi_drm_types::DrmClipRect {
            x1: u16::MAX,
            y1: u16::MAX,
            x2: 0,
            y2: 0,
        };
        mutex_init(core::ptr::addr_of_mut!((*helper).lock));
        (*helper).funcs = funcs;
        (*helper).dev = dev;
        (*helper).preferred_bpp = preferred_bpp as i32;
    }
}

// upstream: drm_fb_helper.c drm_fb_helper_unprepare()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_fb_helper_unprepare(helper: *mut DrmFbHelper) {
    if helper.is_null() {
        return;
    }
    unsafe { mutex_destroy(&mut (*helper).lock) };
}

// upstream: drm_fb_helper.c drm_fb_helper_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_fb_helper_init(dev: *mut DrmDevice, helper: *mut DrmFbHelper) -> i32 {
    if dev.is_null() || helper.is_null() {
        return -EINVAL;
    }
    unsafe { (*dev).fb_helper = helper };
    0
}

unsafe fn drm_fb_helper_alloc_info(helper: *mut DrmFbHelper) -> *mut FbInfo {
    let info = unsafe { crate::fbmem_upstream::fb_alloc_info() };
    if info.is_null() {
        return ERR_PTR(-ENOMEM);
    }
    unsafe {
        (*info).fbops = core::ptr::addr_of!(DRM_FB_HELPER_FB_OPS);
        (*info).par = helper.cast();
        (*info).skip_vt_switch = true;
        (*info).skip_panic = false;
        (*helper).info = info;
        let ret = crate::fbmem_upstream::fb_alloc_cmap(core::ptr::addr_of_mut!((*info).cmap), 256, 0);
        if ret != 0 {
            (*helper).info = core::ptr::null_mut();
            crate::fbmem_upstream::fb_release_info(info);
            return ERR_PTR(ret);
        }
    }
    info
}

unsafe fn drm_fb_helper_release_info(helper: *mut DrmFbHelper) {
    unsafe {
        let info = (*helper).info;
        if info.is_null() {
            return;
        }
        (*helper).info = core::ptr::null_mut();
        if (*info).cmap.len != 0 {
            crate::fbmem_upstream::fb_dealloc_cmap(core::ptr::addr_of_mut!((*info).cmap));
        }
        crate::fbmem_upstream::fb_release_info(info);
    }
}

// upstream: drm_fb_helper.c drm_fb_helper_unregister_info()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_fb_helper_unregister_info(helper: *mut DrmFbHelper) {
    if helper.is_null() {
        return;
    }
    unsafe {
        if !(*helper).info.is_null() {
            crate::fbmem_upstream::unregister_framebuffer((*helper).info);
        }
    }
}

// upstream: drm_fb_helper.c drm_fb_helper_fini()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_fb_helper_fini(helper: *mut DrmFbHelper) {
    if helper.is_null() {
        return;
    }
    unsafe {
        let dev = (*helper).dev;
        if !dev.is_null() && (*dev).fb_helper == helper {
            (*dev).fb_helper = core::ptr::null_mut();
        }
        if !DRM_FBDEV_EMULATION {
            return;
        }
        cancel_work_sync(core::ptr::addr_of_mut!((*helper).resume_work));
        cancel_work_sync(core::ptr::addr_of_mut!((*helper).damage_work));
        drm_fb_helper_release_info(helper);
    }
}

