// SPDX-License-Identifier: MIT
// Copyright © 2026 Intel Corporation
// Source: drivers/gpu/drm/i915/display/intel_bo.c

#![allow(non_snake_case, dead_code, unsafe_code)]

use core::ffi::c_void;

use crate::linux::gem::DrmGemObject;
use crate::intel_display_conversion_upstream::__drm_to_display;
use crate::intel_display_core_types_upstream::IntelDisplay;
use crate::intel_parent_upstream::intel_display_parent;

#[inline]
unsafe fn to_intel_display_from_obj(obj: *mut c_void) -> *mut IntelDisplay {
    if obj.is_null() {
        return core::ptr::null_mut();
    }
    let base = obj.cast::<DrmGemObject>();
    if (*base).dev.is_null() {
        return core::ptr::null_mut();
    }
    __drm_to_display((*base).dev.cast())
}

// upstream: intel_bo.c intel_bo_is_tiled()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bo_is_tiled(obj: *mut c_void) -> bool {
    let display = to_intel_display_from_obj(obj);
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).bo.is_null() {
        if let Some(f) = (*(*p).bo).is_tiled {
            return f(obj);
        }
    }
    false
}

// upstream: intel_bo.c intel_bo_is_userptr()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bo_is_userptr(obj: *mut c_void) -> bool {
    let display = to_intel_display_from_obj(obj);
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).bo.is_null() {
        if let Some(f) = (*(*p).bo).is_userptr {
            return f(obj);
        }
    }
    false
}

// upstream: intel_bo.c intel_bo_is_shmem()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bo_is_shmem(obj: *mut c_void) -> bool {
    let display = to_intel_display_from_obj(obj);
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).bo.is_null() {
        if let Some(f) = (*(*p).bo).is_shmem {
            return f(obj);
        }
    }
    false
}

// upstream: intel_bo.c intel_bo_is_protected()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bo_is_protected(obj: *mut c_void) -> bool {
    let display = to_intel_display_from_obj(obj);
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).bo.is_null() {
        if let Some(f) = (*(*p).bo).is_protected {
            return f(obj);
        }
    }
    false
}

// upstream: intel_bo.c intel_bo_key_check()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bo_key_check(obj: *mut c_void) -> i32 {
    let display = to_intel_display_from_obj(obj);
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).bo.is_null() {
        if let Some(f) = (*(*p).bo).key_check {
            return f(obj);
        }
    }
    0
}

// upstream: intel_bo.c intel_bo_fb_mmap()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bo_fb_mmap(obj: *mut c_void, vma: *mut c_void) -> i32 {
    let display = to_intel_display_from_obj(obj);
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).bo.is_null() {
        if let Some(f) = (*(*p).bo).fb_mmap {
            return f(obj, vma);
        }
    }
    -crate::linux_config::ENODEV
}

// upstream: intel_bo.c intel_bo_read_from_page(obj, offset, dst, size)
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bo_read_from_page(
    obj: *mut c_void,
    offset: u64,
    dst: *mut c_void,
    size: i32,
) -> i32 {
    let display = to_intel_display_from_obj(obj);
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).bo.is_null() {
        if let Some(f) = (*(*p).bo).read_from_page {
            return f(obj, offset, dst, size);
        }
    }
    -crate::linux_config::ENODEV
}

// upstream: intel_bo.c intel_bo_describe()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bo_describe(m: *mut c_void, obj: *mut c_void) {
    let display = to_intel_display_from_obj(obj);
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).bo.is_null() {
        if let Some(f) = (*(*p).bo).describe {
            f(m, obj);
        }
    }
}

// upstream: intel_bo.c intel_bo_framebuffer_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bo_framebuffer_init(
    obj: *mut c_void,
    mode_cmd: *mut c_void,
) -> i32 {
    let display = to_intel_display_from_obj(obj);
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).bo.is_null() {
        if let Some(f) = (*(*p).bo).framebuffer_init {
            return f(obj, mode_cmd);
        }
    }
    -crate::linux_config::ENODEV
}

// upstream: intel_bo.c intel_bo_framebuffer_fini()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bo_framebuffer_fini(obj: *mut c_void) {
    let display = to_intel_display_from_obj(obj);
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).bo.is_null() {
        if let Some(f) = (*(*p).bo).framebuffer_fini {
            f(obj);
        }
    }
}

// upstream: intel_bo.c intel_bo_framebuffer_lookup()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bo_framebuffer_lookup(
    display: *mut IntelDisplay,
    filp: *mut c_void,
    user_mode_cmd: *const c_void,
) -> *mut c_void {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).bo.is_null() && !display.is_null() {
        if let Some(f) = (*(*p).bo).framebuffer_lookup {
            return f((*display).drm, filp, user_mode_cmd);
        }
    }
    core::ptr::null_mut()
}

// upstream: intel_bo.c intel_bo_fbdev_pitch_align()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bo_fbdev_pitch_align(
    _display: *mut IntelDisplay,
    stride: u32,
) -> u32 {
    let p = intel_display_parent(_display);
    if !p.is_null() && !(*p).bo.is_null() {
        if let Some(f) = (*(*p).bo).fbdev_pitch_align {
            return f(stride);
        }
    }
    stride
}

// upstream: intel_bo.c intel_bo_fbdev_create()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bo_fbdev_create(
    display: *mut IntelDisplay,
    size: i32,
) -> *mut c_void {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).bo.is_null() && !display.is_null() {
        if let Some(f) = (*(*p).bo).fbdev_create {
            return f((*display).drm, size);
        }
    }
    core::ptr::null_mut()
}

// upstream: intel_bo.c intel_bo_fbdev_destroy()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bo_fbdev_destroy(obj: *mut c_void) {
    let display = to_intel_display_from_obj(obj);
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).bo.is_null() {
        if let Some(f) = (*(*p).bo).fbdev_destroy {
            f(obj);
        }
    }
}

// upstream: intel_bo.c intel_bo_fbdev_fill_info()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bo_fbdev_fill_info(
    obj: *mut c_void,
    info: *mut c_void,
    vma: *mut c_void,
) -> i32 {
    let display = to_intel_display_from_obj(obj);
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).bo.is_null() {
        if let Some(f) = (*(*p).bo).fbdev_fill_info {
            return f(obj, info, vma);
        }
    }
    -crate::linux_config::ENODEV
}
