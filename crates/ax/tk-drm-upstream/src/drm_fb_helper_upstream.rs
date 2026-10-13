// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_fb_helper.c` (batch B2c).

#![allow(non_snake_case, non_upper_case_globals, dead_code, unsafe_code)]
#![allow(clippy::missing_safety_doc)]

use alloc::format;
use core::ffi::{c_char, c_int, c_long, c_ulong};
use core::mem::offset_of;

use tk_linux_kapi::config::{EAGAIN, EINVAL, ENODEV, ENOMEM, ERR_PTR, IS_ERR, PTR_ERR};
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
use tk_linux_kapi::print::{drm_log_at, DrmLogLevel};
use tk_linux_kapi::types::{ListHead, Mutex, WorkStruct};
use tk_linux_kapi::workqueue::{
    cancel_work_sync, queue_work, INIT_WORK, system_dfl_wq,
};

use crate::drm_client_types::DrmClientDev;
use crate::drm_device_types::DrmDevice;
use crate::drm_fb_helper_types::{DrmFbHelper, DrmFbHelperFuncs, DrmFbHelperSurfaceSize};
use crate::drm_rect_types::DrmRect;
use crate::fb_types::{
    FbCmap, FbDeferredIoPageRef, FbInfo, FbOps,
    FbVarScreenInfo, FBINFO_STATE_RUNNING, FBINFO_STATE_SUSPENDED,
};
use crate::uapi_drm_consts::{
    DRM_MODE_DPMS_OFF, DRM_MODE_DPMS_ON, DRM_MODE_DPMS_STANDBY,
    DRM_MODE_DPMS_SUSPEND,
};

const DRM_FBDEV_EMULATION: bool = true;
const DRM_FBDEV_OVERALLOC: u32 = 100;
const FB_TYPE_PACKED_PIXELS: u32 = 0;
const FB_VISUAL_TRUECOLOR: u32 = 2;
const FB_VISUAL_PSEUDOCOLOR: u32 = 3;
const FB_ACCEL_NONE: u32 = 0;
const FB_ACTIVATE_NOW: u32 = 0;
const FB_BLANK_UNBLANK: c_int = 0;
const FB_BLANK_NORMAL: c_int = 1;
const FB_BLANK_HSYNC_SUSPEND: c_int = 2;
const FB_BLANK_VSYNC_SUSPEND: c_int = 3;
const FB_BLANK_POWERDOWN: c_int = 4;
const FB_ROTATE_UR: c_int = 0;

static mut FB_CONSOLE_LOCK: Mutex = unsafe { core::mem::zeroed() };
static FB_CONSOLE_LOCK_INIT: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

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
            drm_fb_helper_set_suspend(helper, false);
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
        unsafe {
            let client = core::ptr::addr_of_mut!((*helper).client);
            let buffer = (*helper).buffer;
            if !(*info).fbdefio_state.is_null() {
                crate::fbmem_upstream::fb_deferred_io_cleanup(info);
            }
            drm_fb_helper_fini(helper);
            if !buffer.is_null() {
                // drm_client_buffer_delete() owns the matching GEM vunmap.
                crate::drm_client_upstream::drm_client_buffer_delete(buffer);
                (*helper).buffer = core::ptr::null_mut();
                (*helper).fb = core::ptr::null_mut();
            }
            // `drm_client_release()` invokes the free callback, so do not
            // access helper after this point.
            crate::drm_client_upstream::drm_client_release(client);
        }
    }
}

unsafe extern "C" fn drm_fb_helper_fb_mmap(info: *mut FbInfo, vma: *mut crate::fb_types::VmAreaStruct) -> i32 {
    if info.is_null() || vma.is_null() {
        return -EINVAL;
    }
    unsafe {
        if !(*info).fbdefio.is_null() {
            return crate::fbmem_upstream::fb_deferred_io_mmap(info, vma);
        }
    }
    -ENODEV
}

static mut DRM_FB_HELPER_FB_OPS: FbOps = FbOps {
    owner: core::ptr::null_mut(),
    fb_open: None,
    fb_release: None,
    fb_read: Some(drm_fb_helper_sys_read),
    fb_write: Some(drm_fb_helper_sys_write),
    fb_check_var: Some(drm_fb_helper_check_var),
    fb_set_par: Some(drm_fb_helper_set_par),
    fb_setcolreg: None,
    fb_setcmap: Some(drm_fb_helper_setcmap),
    fb_blank: Some(drm_fb_helper_blank),
    fb_pan_display: Some(drm_fb_helper_pan_display),
    fb_fillrect: Some(drm_fb_helper_sys_fillrect),
    fb_copyarea: Some(drm_fb_helper_sys_copyarea),
    fb_imageblit: Some(drm_fb_helper_sys_imageblit),
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
        INIT_WORK(&mut (*helper).resume_work, |work| fb_helper_resume_worker(work));
        INIT_WORK(&mut (*helper).damage_work, |work| fb_helper_damage_work(work));
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

unsafe fn drm_fb_helper_cleanup_probe_resources(helper: *mut DrmFbHelper) {
    if helper.is_null() { return; }
    unsafe {
        let info = (*helper).info;
        if !info.is_null() && !(*info).fbdefio_state.is_null() {
            crate::fbmem_upstream::fb_deferred_io_cleanup(info);
        }
        drm_fb_helper_release_info(helper);
        if !(*helper).buffer.is_null() {
            crate::drm_client_upstream::drm_client_buffer_delete((*helper).buffer);
            (*helper).buffer = core::ptr::null_mut();
        }
        (*helper).fb = core::ptr::null_mut();
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

#[inline]
fn fb_channel(value: u16, field: &crate::fb_types::FbBitfield) -> u32 {
    if field.length == 0 || field.length > 16 || field.offset >= 32 {
        return 0;
    }
    let component = (value as u32) >> (16 - field.length);
    component << field.offset
}

unsafe fn fb_helper_set_color(info: *mut FbInfo, index: u32, red: u16, green: u16, blue: u16, transp: u16) -> i32 {
    if info.is_null() {
        return -EINVAL;
    }
    unsafe {
        let helper = fb_helper_from_info(info);
        if helper.is_null() {
            return -EINVAL;
        }
        let cmap = &mut (*info).cmap;
        if index >= cmap.len {
            return -EINVAL;
        }
        if !cmap.red.is_null() { *cmap.red.add(index as usize) = red; }
        if !cmap.green.is_null() { *cmap.green.add(index as usize) = green; }
        if !cmap.blue.is_null() { *cmap.blue.add(index as usize) = blue; }
        if !cmap.transp.is_null() { *cmap.transp.add(index as usize) = transp; }
        let var = &(*info).var;
        if var.bits_per_pixel > 8 && index < (*helper).pseudo_palette.len() as u32 {
            (*helper).pseudo_palette[index as usize] = fb_channel(red, &var.red)
                | fb_channel(green, &var.green)
                | fb_channel(blue, &var.blue)
                | fb_channel(transp, &var.transp);
        }
    }
    0
}

unsafe extern "C" fn drm_fb_helper_setcmap(cmap: *mut FbCmap, info: *mut FbInfo) -> i32 {
    if cmap.is_null() || info.is_null() {
        return -EINVAL;
    }
    unsafe {
        let end = match (*cmap).start.checked_add((*cmap).len) {
            Some(end) if end <= (*info).cmap.len => end,
            _ => return -EINVAL,
        };
        for index in (*cmap).start..end {
            let offset = (index - (*cmap).start) as usize;
            let value = |ptr: *mut u16| if ptr.is_null() { 0 } else { *ptr.add(offset) };
            let ret = fb_helper_set_color(info, index, value((*cmap).red), value((*cmap).green), value((*cmap).blue), value((*cmap).transp));
            if ret != 0 { return ret; }
        }
    }
    0
}

unsafe extern "C" fn drm_fb_helper_check_var(var: *mut FbVarScreenInfo, info: *mut FbInfo) -> i32 {
    if var.is_null() || info.is_null() { return -EINVAL; }
    unsafe {
        let helper = fb_helper_from_info(info);
        if helper.is_null() { return -EINVAL; }
        let fb = (*helper).fb;
        if fb.is_null() || (*fb).format.is_null() || ((*var).bits_per_pixel != (*(*fb).format).depth as u32 && (*var).bits_per_pixel != 32) {
            return -EINVAL;
        }
        if (*var).xres == 0 || (*var).yres == 0 || (*var).xres > (*fb).width || (*var).yres > (*fb).height {
            return -EINVAL;
        }
        if (*var).xoffset.checked_add((*var).xres).is_none_or(|x| x > (*var).xres_virtual)
            || (*var).yoffset.checked_add((*var).yres).is_none_or(|y| y > (*var).yres_virtual) {
            return -EINVAL;
        }
        (*var).activate = FB_ACTIVATE_NOW;
    }
    0
}

unsafe extern "C" fn drm_fb_helper_set_par(info: *mut FbInfo) -> i32 {
    if info.is_null() { return -EINVAL; }
    unsafe {
        let helper = fb_helper_from_info(info);
        if helper.is_null() { return -EINVAL; }
        let ret = crate::drm_client_modeset_upstream::drm_client_modeset_probe(
            core::ptr::addr_of_mut!((*helper).client), (*info).var.xres, (*info).var.yres,
        );
        if ret != 0 { return ret; }
        let mut node = (*helper).client.modesets;
        for _ in 0..(*(*helper).dev).mode_config.num_crtc.max(0) as usize {
            if node.is_null() { break; }
            (*node).fb = (*helper).fb;
            (*node).x = (*info).var.xoffset;
            (*node).y = (*info).var.yoffset;
            node = node.add(1);
        }
        crate::drm_client_modeset_upstream::drm_client_modeset_commit(&mut (*helper).client)
    }
}

unsafe extern "C" fn drm_fb_helper_blank(blank: c_int, info: *mut FbInfo) -> i32 {
    if info.is_null() { return -EINVAL; }
    let mode = match blank {
        FB_BLANK_UNBLANK => DRM_MODE_DPMS_ON,
        FB_BLANK_NORMAL => DRM_MODE_DPMS_STANDBY,
        FB_BLANK_HSYNC_SUSPEND => DRM_MODE_DPMS_STANDBY,
        FB_BLANK_VSYNC_SUSPEND => DRM_MODE_DPMS_SUSPEND,
        FB_BLANK_POWERDOWN => DRM_MODE_DPMS_OFF,
        _ => return -EINVAL,
    };
    unsafe {
        let helper = fb_helper_from_info(info);
        if helper.is_null() { return -EINVAL; }
        crate::drm_client_modeset_upstream::drm_client_modeset_dpms(&mut (*helper).client, mode as i32)
    }
}

unsafe extern "C" fn drm_fb_helper_pan_display(var: *mut FbVarScreenInfo, info: *mut FbInfo) -> i32 {
    if var.is_null() || info.is_null() { return -EINVAL; }
    let ret = unsafe { drm_fb_helper_check_var(var, info) };
    if ret != 0 { return ret; }
    unsafe {
        (*info).var.xoffset = (*var).xoffset;
        (*info).var.yoffset = (*var).yoffset;
        drm_fb_helper_set_par(info)
    }
}

unsafe extern "C" fn drm_fb_helper_ioctl(_info: *mut FbInfo, _cmd: u32, _arg: c_ulong) -> i32 {
    -25 // -ENOTTY
}

unsafe extern "C" fn drm_fb_helper_sys_read(info: *mut FbInfo, buf: *mut c_char, count: usize, ppos: *mut c_long) -> isize {
    unsafe { crate::fb_sysmem_upstream::fb_sys_read(info, buf, count, ppos) }
}

unsafe extern "C" fn drm_fb_helper_sys_write(info: *mut FbInfo, buf: *const c_char, count: usize, ppos: *mut c_long) -> isize {
    if ppos.is_null() { return -(EINVAL as isize); }
    let start = unsafe { *ppos };
    let written = unsafe { crate::fb_sysmem_upstream::fb_sys_write(info, buf, count, ppos) };
    if written > 0 { unsafe { drm_fb_helper_damage_range(info, start, written as usize); } }
    written
}

unsafe extern "C" fn drm_fb_helper_sys_fillrect(info: *mut FbInfo, rect: *const crate::fb_types::FbFillRect) {
    if rect.is_null() { return; }
    unsafe {
        crate::fb_sysmem_upstream::sys_fillrect(info, rect);
        drm_fb_helper_damage_area(info, (*rect).dx, (*rect).dy, (*rect).width, (*rect).height);
    }
}

unsafe extern "C" fn drm_fb_helper_sys_copyarea(info: *mut FbInfo, area: *const crate::fb_types::FbCopyArea) {
    if area.is_null() { return; }
    unsafe {
        crate::fb_sysmem_upstream::sys_copyarea(info, area);
        drm_fb_helper_damage_area(info, (*area).dx, (*area).dy, (*area).width, (*area).height);
    }
}

unsafe extern "C" fn drm_fb_helper_sys_imageblit(info: *mut FbInfo, image: *const crate::fb_types::FbImage) {
    if image.is_null() { return; }
    unsafe {
        crate::fb_sysmem_upstream::sys_imageblit(info, image);
        drm_fb_helper_damage_area(info, (*image).dx, (*image).dy, (*image).width, (*image).height);
    }
}

// Linux's fill_var/fill_fix are static helpers in drm_fb_helper.c; keep this
// public fill_info ABI for driver.fbdev_probe callbacks.
fn drm_fb_helper_fill_var(info: *mut FbInfo, helper: *mut DrmFbHelper, sizes: *mut DrmFbHelperSurfaceSize) {
    unsafe {
        let fb = (*helper).fb;
        let format = (*fb).format;
        (*info).var = FbVarScreenInfo::default();
        (*info).var.xres = (*sizes).fb_width;
        (*info).var.yres = (*sizes).fb_height;
        (*info).var.xres_virtual = (*fb).width;
        (*info).var.yres_virtual = (*fb).height;
        (*info).var.bits_per_pixel = crate::drm_fourcc_upstream::drm_format_info_bpp(format, 0);
        (*info).var.activate = FB_ACTIVATE_NOW;
        (*info).var.rotate = FB_ROTATE_UR as u32;
        match (*format).format {
            crate::uapi_drm_consts::DRM_FORMAT_RGB565 => {
                (*info).var.red = crate::fb_types::FbBitfield { offset: 11, length: 5, msb_right: 0 };
                (*info).var.green = crate::fb_types::FbBitfield { offset: 5, length: 6, msb_right: 0 };
                (*info).var.blue = crate::fb_types::FbBitfield { offset: 0, length: 5, msb_right: 0 };
            }
            crate::uapi_drm_consts::DRM_FORMAT_XRGB8888 => {
                (*info).var.red = crate::fb_types::FbBitfield { offset: 16, length: 8, msb_right: 0 };
                (*info).var.green = crate::fb_types::FbBitfield { offset: 8, length: 8, msb_right: 0 };
                (*info).var.blue = crate::fb_types::FbBitfield { offset: 0, length: 8, msb_right: 0 };
            }
            _ => {
                (*info).var.red = crate::fb_types::FbBitfield { offset: 16, length: 8, msb_right: 0 };
                (*info).var.green = crate::fb_types::FbBitfield { offset: 8, length: 8, msb_right: 0 };
                (*info).var.blue = crate::fb_types::FbBitfield { offset: 0, length: 8, msb_right: 0 };
                if (*format).has_alpha {
                    (*info).var.transp = crate::fb_types::FbBitfield { offset: 24, length: 8, msb_right: 0 };
                }
            }
        }
        (*info).pseudo_palette = (*helper).pseudo_palette.as_mut_ptr().cast();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_fb_helper_fill_info(
    info: *mut FbInfo, helper: *mut DrmFbHelper, sizes: *mut DrmFbHelperSurfaceSize,
) {
    if info.is_null() || helper.is_null() || sizes.is_null() || unsafe { (*helper).fb.is_null() || (*(*helper).fb).format.is_null() } { return; }
    unsafe {
        let fb = (*helper).fb;
        (*info).fix.type_ = FB_TYPE_PACKED_PIXELS;
        (*info).fix.visual = if (*(*fb).format).is_color_indexed { FB_VISUAL_PSEUDOCOLOR } else { FB_VISUAL_TRUECOLOR };
        (*info).fix.mmio_start = 0;
        (*info).fix.mmio_len = 0;
        (*info).fix.type_aux = 0;
        (*info).fix.xpanstep = 1;
        (*info).fix.ypanstep = 1;
        (*info).fix.ywrapstep = 0;
        (*info).fix.accel = FB_ACCEL_NONE;
        (*info).fix.line_length = (*fb).pitches[0];
        drm_fb_helper_fill_var(info, helper, sizes);
        (*info).par = helper.cast();
        (*info).fix.id = [0; 16];
        if !(*(*helper).dev).driver.is_null() && !(*(*(*helper).dev).driver).name.is_null() {
            let name = core::ffi::CStr::from_ptr((*(*(*helper).dev).driver).name).to_bytes();
            for (i, byte) in name.iter().copied().chain(b"drmfb".iter().copied()).take(15).enumerate() {
                (*info).fix.id[i] = byte as c_char;
            }
        }
    }
}

unsafe fn drm_fb_helper_fb_dirty(helper: *mut DrmFbHelper) {
    if helper.is_null() { return; }
    unsafe {
        let mut flags = 0 as c_ulong;
        spin_lock_irqsave_raw(core::ptr::addr_of_mut!((*helper).damage_lock), &mut flags);
        let clip = crate::uapi_drm_types::DrmClipRect {
            x1: (*helper).damage_clip.x1,
            y1: (*helper).damage_clip.y1,
            x2: (*helper).damage_clip.x2,
            y2: (*helper).damage_clip.y2,
        };
        (*helper).damage_clip = crate::uapi_drm_types::DrmClipRect { x1: u16::MAX, y1: u16::MAX, x2: 0, y2: 0 };
        spin_unlock_irqrestore_raw(core::ptr::addr_of_mut!((*helper).damage_lock), flags);
        if clip.x1 >= clip.x2 || clip.y1 >= clip.y2 { return; }
        let ret = if !(*helper).funcs.is_null() {
            if let Some(dirty) = (*(*helper).funcs).fb_dirty {
                dirty(helper, &clip as *const _ as *mut _)
            } else {
                let mut rect = DrmRect { x1: clip.x1 as i32, y1: clip.y1 as i32, x2: clip.x2 as i32, y2: clip.y2 as i32 };
                crate::drm_client_upstream::drm_client_buffer_flush((*helper).buffer, &mut rect)
            }
        } else {
            let mut rect = DrmRect { x1: clip.x1 as i32, y1: clip.y1 as i32, x2: clip.x2 as i32, y2: clip.y2 as i32 };
            crate::drm_client_upstream::drm_client_buffer_flush((*helper).buffer, &mut rect)
        };
        if ret != 0 { drm_log_at(DrmLogLevel::Warn, "drm", file!(), line!(), &format!("fbdev damage callback failed: {ret}")); }
    }
}

unsafe fn drm_fb_helper_add_damage(helper: *mut DrmFbHelper, clip: crate::uapi_drm_types::DrmClipRect) {
    if helper.is_null() || clip.x1 >= clip.x2 || clip.y1 >= clip.y2 { return; }
    unsafe {
        let mut flags = 0 as c_ulong;
        spin_lock_irqsave_raw(core::ptr::addr_of_mut!((*helper).damage_lock), &mut flags);
        let current = &mut (*helper).damage_clip;
        current.x1 = current.x1.min(clip.x1);
        current.y1 = current.y1.min(clip.y1);
        current.x2 = current.x2.max(clip.x2);
        current.y2 = current.y2.max(clip.y2);
        spin_unlock_irqrestore_raw(core::ptr::addr_of_mut!((*helper).damage_lock), flags);
        if (*helper).damage_work.function.is_some() { queue_work(system_dfl_wq, core::ptr::addr_of_mut!((*helper).damage_work)); }
    }
}

// Linux helper called by system-memory fb read/write wrappers. `off` and len
// are clipped to the allocated framebuffer; row/pixel arithmetic is checked.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_fb_helper_damage_range(info: *mut FbInfo, off: c_long, len: usize) {
    if info.is_null() || off < 0 || len == 0 { return; }
    unsafe {
        let bound = if (*info).screen_size == 0 { (*info).fix.smem_len as usize } else { (*info).screen_size.min((*info).fix.smem_len as c_ulong) as usize };
        if let Some(clip) = damage_range_to_clip(
            off as usize, len, bound, (*info).fix.line_length as usize,
            (*info).var.xres, (*info).var.yres, (*info).var.bits_per_pixel,
        ) { drm_fb_helper_add_damage(fb_helper_from_info(info), clip); }
    }
}

fn damage_range_to_clip(
    start: usize, len: usize, bound: usize, line: usize,
    xres: u32, yres: u32, bpp: u32,
) -> Option<crate::uapi_drm_types::DrmClipRect> {
    if len == 0 || line == 0 || xres == 0 || yres == 0 || bpp == 0 { return None; }
    let xres = xres.min(u16::MAX as u32);
    let yres = yres.min(u16::MAX as u32);
    let end = start.saturating_add(len).min(bound);
    if start >= end { return None; }
    let y1 = (start / line).min(yres as usize);
    let y2 = end.saturating_add(line - 1).checked_div(line)?.min(yres as usize);
    if y1 >= y2 { return None; }
    let (x1, x2) = if y2 - y1 > 1 {
        (0, xres as usize)
    } else {
        let row_base = y1 * line;
        let first_bits = (start - row_base).saturating_mul(8);
        let last_bits = (end - row_base).saturating_mul(8);
        let x1 = (first_bits / bpp as usize).min(xres as usize);
        let x2 = last_bits.saturating_add(bpp as usize - 1).checked_div(bpp as usize)?.min(xres as usize);
        (x1, x2)
    };
    if x1 >= x2 { return None; }
    Some(crate::uapi_drm_types::DrmClipRect { x1: x1 as u16, y1: y1 as u16, x2: x2 as u16, y2: y2 as u16 })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_fb_helper_damage_area(info: *mut FbInfo, x: u32, y: u32, width: u32, height: u32) {
    if info.is_null() || width == 0 || height == 0 { return; }
    unsafe {
        let xres = (*info).var.xres.min(u16::MAX as u32);
        let yres = (*info).var.yres.min(u16::MAX as u32);
        let x1 = x.min(xres);
        let y1 = y.min(yres);
        let x2 = x.saturating_add(width).min(xres);
        let y2 = y.saturating_add(height).min(yres);
        if x1 < x2 && y1 < y2 { drm_fb_helper_add_damage(fb_helper_from_info(info), crate::uapi_drm_types::DrmClipRect { x1: x1 as u16, y1: y1 as u16, x2: x2 as u16, y2: y2 as u16 }); }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_fb_helper_deferred_io(info: *mut FbInfo, pages: *mut ListHead) {
    if info.is_null() || pages.is_null() { return; }
    unsafe {
        let mut min = usize::MAX;
        let mut max = 0usize;
        let mut node = (*pages).next;
        while !node.is_null() && !core::ptr::eq(node, pages) {
            let page_ref = node.cast::<u8>().sub(offset_of!(FbDeferredIoPageRef, list)).cast::<FbDeferredIoPageRef>();
            let start = (*page_ref).offset as usize;
            min = min.min(start);
            max = max.max(start.saturating_add(4096));
            node = (*node).next;
        }
        if min != usize::MAX { drm_fb_helper_damage_range(info, min as c_long, max.saturating_sub(min)); }
    }
}

unsafe fn drm_fb_helper_set_suspend(helper: *mut DrmFbHelper, suspend: bool) {
    if helper.is_null() { return; }
    unsafe {
        let info = (*helper).info;
        if info.is_null() { return; }
        let state = if suspend { FBINFO_STATE_SUSPENDED } else { FBINFO_STATE_RUNNING };
        if (*info).state == state { return; }
        if suspend { cancel_work_sync(core::ptr::addr_of_mut!((*helper).damage_work)); }
        if let Some(callback) = (*helper).funcs.as_ref().and_then(|f| f.fb_set_suspend) {
            callback(helper, suspend);
        } else {
            crate::fbmem_upstream::fb_set_suspend(info, suspend as c_int);
        }
        (*info).state = state;
        if !suspend { queue_work(system_dfl_wq, core::ptr::addr_of_mut!((*helper).damage_work)); }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_fb_helper_set_suspend_unlocked(helper: *mut DrmFbHelper, suspend: bool) {
    if helper.is_null() { return; }
    unsafe {
        if !FB_CONSOLE_LOCK_INIT.swap(true, core::sync::atomic::Ordering::AcqRel) { mutex_init(core::ptr::addr_of_mut!(FB_CONSOLE_LOCK)); }
        if suspend {
            mutex_lock(core::ptr::addr_of_mut!(FB_CONSOLE_LOCK));
            drm_fb_helper_set_suspend(helper, true);
            mutex_unlock(core::ptr::addr_of_mut!(FB_CONSOLE_LOCK));
        } else if mutex_trylock(core::ptr::addr_of_mut!(FB_CONSOLE_LOCK)) {
            drm_fb_helper_set_suspend(helper, false);
            mutex_unlock(core::ptr::addr_of_mut!(FB_CONSOLE_LOCK));
        } else {
            queue_work(system_dfl_wq, core::ptr::addr_of_mut!((*helper).resume_work));
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_fb_helper_restore_fbdev_mode_unlocked(helper: *mut DrmFbHelper, _force: bool) {
    if helper.is_null() { return; }
    unsafe {
        if !(*helper).info.is_null() {
            let _ = crate::drm_client_modeset_upstream::drm_client_modeset_commit(&mut (*helper).client);
            if let Some(callback) = (*helper).funcs.as_ref().and_then(|f| f.fb_restore) { callback(helper); }
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_fb_helper_hotplug_event(helper: *mut DrmFbHelper) -> i32 {
    if helper.is_null() { return -EINVAL; }
    unsafe {
        if (*helper).info.is_null() { return drm_fb_helper_initial_config(helper); }
        let ret = crate::drm_client_modeset_upstream::drm_client_modeset_probe(&mut (*helper).client, (*(*helper).info).var.xres, (*(*helper).info).var.yres);
        if ret != 0 { return ret; }
        drm_fb_helper_restore_fbdev_mode_unlocked(helper, false);
        0
    }
}

unsafe fn drm_fb_helper_find_sizes(helper: *mut DrmFbHelper, sizes: *mut DrmFbHelperSurfaceSize) -> i32 {
    if helper.is_null() || sizes.is_null() || unsafe { (*helper).dev.is_null() } { return -EINVAL; }
    unsafe {
        let dev = (*helper).dev;
        *sizes = core::mem::zeroed();
        (*sizes).fb_width = u32::MAX;
        (*sizes).fb_height = u32::MAX;
        let mut crtc_count = 0usize;
        let num_crtc = (*dev).mode_config.num_crtc.max(0) as usize;
        for index in 0..num_crtc {
            let modeset = (*helper).client.modesets.add(index);
            if (*modeset).mode.is_null() { continue; }
            crtc_count += 1;
            let width = (*(*modeset).mode).hdisplay as u32 + (*modeset).x;
            let height = (*(*modeset).mode).vdisplay as u32 + (*modeset).y;
            (*sizes).surface_width = (*sizes).surface_width.max(width);
            (*sizes).surface_height = (*sizes).surface_height.max(height);
            (*sizes).fb_width = (*sizes).fb_width.min(width);
            (*sizes).fb_height = (*sizes).fb_height.min(height);
        }
        if crtc_count == 0 || (*sizes).fb_width == u32::MAX || (*sizes).fb_height == u32::MAX {
            return -EAGAIN;
        }

        let preferred_bpp = if (*helper).preferred_bpp <= 0 { 32 } else { (*helper).preferred_bpp as u32 };
        let requested = crate::drm_fourcc_upstream::drm_driver_color_mode_format(dev, preferred_bpp);
        let mut format = 0;
        for index in 0..num_crtc {
            let modeset = (*helper).client.modesets.add(index);
            if (*modeset).mode.is_null() || (*modeset).crtc.is_null() { continue; }
            let plane = (*(*modeset).crtc).primary;
            if plane.is_null() || (*plane).format_types.is_null() { continue; }
            for fmt_index in 0..(*plane).format_count as usize {
                let candidate = *(*plane).format_types.add(fmt_index);
                if candidate == requested {
                    format = candidate;
                    break;
                }
            }
            if format != 0 { break; }
        }
        if format == 0 {
            format = crate::drm_fourcc_upstream::drm_driver_legacy_fb_format(dev, 32, 24);
        }
        let format_info = crate::drm_fourcc_upstream::drm_format_info(format);
        if format_info.is_null() { return -EINVAL; }
        (*sizes).surface_bpp = crate::drm_fourcc_upstream::drm_format_info_bpp(format_info, 0);
        (*sizes).surface_depth = (*format_info).depth as u32;

        let overalloc = DRM_FBDEV_OVERALLOC.max(100);
        (*sizes).surface_height = (*sizes).surface_height.saturating_mul(overalloc) / 100;
        if (*dev).mode_config.max_height != 0 {
            (*sizes).surface_height = (*sizes).surface_height.min((*dev).mode_config.max_height);
        }
        if (*dev).mode_config.max_width != 0 {
            (*sizes).surface_width = (*sizes).surface_width.min((*dev).mode_config.max_width);
        }
        0
    }
}

unsafe fn drm_fb_helper_invoke_driver_probe(
    helper: *mut DrmFbHelper,
    sizes: *mut DrmFbHelperSurfaceSize,
) -> i32 {
    if helper.is_null() || unsafe { (*helper).dev.is_null() } { return -EINVAL; }
    unsafe {
        let info = (*helper).info;
        if info.is_null() || (*info).par != helper.cast() { return -EINVAL; }
        let dev = (*helper).dev;
        let driver = (*dev).driver;
        if driver.is_null() { return -EINVAL; }
        let Some(probe) = (*driver).fbdev_probe else { return -EINVAL };
        probe(helper, sizes)
    }
}

unsafe fn drm_fb_helper_single_fb_probe(helper: *mut DrmFbHelper) -> i32 {
    if helper.is_null() || unsafe { (*helper).dev.is_null() } { return -EINVAL; }
    unsafe {
        let dev = (*helper).dev;
        let driver = (*dev).driver;
        if driver.is_null() || (*driver).fbdev_probe.is_none() { return -EINVAL; }
        let mut sizes: DrmFbHelperSurfaceSize = core::mem::zeroed();
        let ret = drm_fb_helper_find_sizes(helper, &mut sizes);
        if ret != 0 {
            if ret == -EAGAIN && !(*helper).deferred_setup {
                let _ = crate::drm_client_modeset_upstream::drm_client_modeset_commit(&mut (*helper).client);
            }
            return ret;
        }
        let ret = drm_fb_helper_invoke_driver_probe(helper, &mut sizes);
        if ret < 0 { return ret; }
        if (*helper).fb.is_null() { return -EINVAL; }
        (*(*helper).fb).comm = [0; 16];
        for (index, byte) in b"[fbcon]".iter().copied().enumerate() {
            (*(*helper).fb).comm[index] = byte as c_char;
        }
        0
    }
}

unsafe fn drm_fb_helper_setup_crtcs_fb(helper: *mut DrmFbHelper) {
    if helper.is_null() || unsafe { (*helper).fb.is_null() } { return; }
    unsafe {
        let num_crtc = (*(*helper).dev).mode_config.num_crtc.max(0) as usize;
        for index in 0..num_crtc {
            let modeset = (*helper).client.modesets.add(index);
            if !(*modeset).mode.is_null() && (*modeset).num_connectors != 0 {
                (*modeset).fb = (*helper).fb;
            }
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_fb_helper_initial_config(helper: *mut DrmFbHelper) -> i32 {
    if helper.is_null() || unsafe { (*helper).dev.is_null() } { return -EINVAL; }
    unsafe {
        let dev = (*helper).dev;
        let client = &mut (*helper).client as *mut DrmClientDev;
        if !DRM_FBDEV_EMULATION { return 0; }
        if (*client).modesets.is_null() {
            let ret = crate::drm_client_modeset_upstream::drm_client_modeset_create(client);
            if ret != 0 { return ret; }
        }
        mutex_lock(core::ptr::addr_of_mut!((*helper).lock));
        let ret = crate::drm_client_modeset_upstream::drm_client_modeset_probe(
            client, (*dev).mode_config.max_width, (*dev).mode_config.max_height,
        );
        if ret != 0 {
            mutex_unlock(core::ptr::addr_of_mut!((*helper).lock));
            return ret;
        }
        let info = drm_fb_helper_alloc_info(helper);
        if IS_ERR(info) {
            mutex_unlock(core::ptr::addr_of_mut!((*helper).lock));
            return PTR_ERR(info);
        }
        let mut ret = drm_fb_helper_single_fb_probe(helper);
        if ret < 0 {
            if ret == -EAGAIN {
                (*helper).deferred_setup = true;
                ret = 0;
            }
            mutex_unlock(core::ptr::addr_of_mut!((*helper).lock));
            drm_fb_helper_cleanup_probe_resources(helper);
            return ret;
        }
        drm_fb_helper_setup_crtcs_fb(helper);
        (*helper).deferred_setup = false;
        (*info).var.pixclock = 0;
        mutex_unlock(core::ptr::addr_of_mut!((*helper).lock));
        ret = crate::fbmem_upstream::register_framebuffer(info);
        if ret != 0 {
            drm_fb_helper_cleanup_probe_resources(helper);
            return ret;
        }
        crate::drm_client_modeset_upstream::drm_client_modeset_commit(client)
    }
}

#[cfg(test)]
mod tests {
    use super::damage_range_to_clip;
    use super::{drm_fb_helper_alloc_info, drm_fb_helper_cleanup_probe_resources, drm_fb_helper_invoke_driver_probe};
    use crate::drm_device_types::DrmDevice;
    use crate::drm_drv_types::DrmDriver;
    use crate::drm_fb_helper_types::{DrmFbHelper, DrmFbHelperSurfaceSize};
    use crate::fb_types::{FbInfo, FBINFO_STATE_RUNNING};
    use alloc::boxed::Box;
    use core::mem::MaybeUninit;
    use core::sync::atomic::{AtomicBool, Ordering};
    use tk_linux_kapi::config::EINVAL;

    static PROBE_SAW_ALLOCATED_INFO: AtomicBool = AtomicBool::new(false);

    unsafe extern "C" fn test_fbdev_probe(helper: *mut DrmFbHelper, _sizes: *mut DrmFbHelperSurfaceSize) -> i32 {
        unsafe {
            if !(*helper).info.is_null() && (*(*helper).info).par == helper.cast() {
                PROBE_SAW_ALLOCATED_INFO.store(true, Ordering::SeqCst);
                0
            } else {
                -EINVAL
            }
        }
    }

    #[test]
    fn driver_probe_requires_alloc_info_and_helper_par_before_callback() {
        let mut driver = Box::new(unsafe { MaybeUninit::<DrmDriver>::zeroed().assume_init() });
        driver.fbdev_probe = Some(test_fbdev_probe);
        let mut dev = Box::new(unsafe { MaybeUninit::<DrmDevice>::zeroed().assume_init() });
        dev.driver = &mut *driver;
        let mut helper = Box::new(unsafe { MaybeUninit::<DrmFbHelper>::zeroed().assume_init() });
        helper.dev = &mut *dev;
        let helper_ptr = &mut *helper as *mut DrmFbHelper;
        let mut sizes = DrmFbHelperSurfaceSize {
            fb_width: 800, fb_height: 600, surface_width: 800,
            surface_height: 600, surface_bpp: 32, surface_depth: 24,
        };
        PROBE_SAW_ALLOCATED_INFO.store(false, Ordering::SeqCst);

        assert_eq!(unsafe { drm_fb_helper_invoke_driver_probe(helper_ptr, &mut sizes) }, -EINVAL);
        assert!(!PROBE_SAW_ALLOCATED_INFO.load(Ordering::SeqCst));

        let mut info = Box::new(unsafe { MaybeUninit::<FbInfo>::zeroed().assume_init() });
        info.state = FBINFO_STATE_RUNNING;
        info.par = helper_ptr.cast();
        helper.info = &mut *info;
        assert_eq!(unsafe { drm_fb_helper_invoke_driver_probe(helper_ptr, &mut sizes) }, 0);
        assert!(PROBE_SAW_ALLOCATED_INFO.load(Ordering::SeqCst));
    }

    #[test]
    fn failed_probe_cleanup_releases_allocated_info_and_resets_helper_fields() {
        let mut helper = Box::new(unsafe { MaybeUninit::<DrmFbHelper>::zeroed().assume_init() });
        let helper_ptr = &mut *helper as *mut DrmFbHelper;
        let info = unsafe { drm_fb_helper_alloc_info(helper_ptr) };
        assert!(!info.is_null());
        assert_eq!(unsafe { (*info).par }, helper_ptr.cast());
        helper.fb = 1usize as *mut crate::drm_framebuffer_types::DrmFramebuffer;
        unsafe { drm_fb_helper_cleanup_probe_resources(helper_ptr) };
        assert!(helper.info.is_null());
        assert!(helper.buffer.is_null());
        assert!(helper.fb.is_null());
    }

    #[test]
    fn damage_range_clips_single_line_and_full_multiline_rows() {
        let clip = damage_range_to_clip(4, 4, 400, 40, 10, 10, 32).unwrap();
        assert_eq!((clip.x1, clip.y1, clip.x2, clip.y2), (1, 0, 2, 1));

        // A single rectangle covering rows with partial first/last bytes must
        // include the intervening pixels rather than becoming an inverted box.
        let clip = damage_range_to_clip(1, 42, 400, 40, 10, 10, 32).unwrap();
        assert_eq!((clip.x1, clip.y1, clip.x2, clip.y2), (0, 0, 10, 2));
    }

    #[test]
    fn damage_range_rejects_empty_padding_and_out_of_bounds_ranges() {
        assert!(damage_range_to_clip(0, 0, 400, 40, 10, 10, 32).is_none());
        assert!(damage_range_to_clip(400, 1, 400, 40, 10, 10, 32).is_none());
        assert!(damage_range_to_clip(39, 1, 400, 40, 9, 10, 32).is_none());
        assert!(damage_range_to_clip(0, 1, 400, 0, 10, 10, 32).is_none());
    }
}

