// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// Source: drivers/gpu/drm/i915/display/intel_fbc.c and intel_fbc_regs.h (Linux v7.2.3).
//! Frame-buffer compression: the dirty-rectangle update path.
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::ffi::c_void;

use tk_drm_upstream::drm_rect_types::DrmRect;
use tk_linux_kapi::mutex::{mutex_lock, mutex_unlock};

use crate::{
    intel_de_upstream::intel_de_write_dsb, intel_display_core_types_upstream::IntelDisplay,
    intel_display_reg_defs_upstream::mmio, intel_display_types_upstream::IntelPlane,
    intel_fbc_types_upstream::IntelFbc, intel_workarounds_types_upstream::I915RegT,
};

/// `DISPLAY_VER()` for the display device.
#[inline]
unsafe fn display_ver(display: *const IntelDisplay) -> u16 {
    unsafe { (*display).info.__runtime_info.ip.ver }
}

/// `HAS_FBC_DIRTY_RECT()` (intel_display_device.h).
#[inline]
unsafe fn HAS_FBC_DIRTY_RECT(display: *const IntelDisplay) -> bool {
    unsafe { display_ver(display) >= 30 }
}

/// `XE3_FBC_DIRTY_RECT(fbc_id)` (intel_fbc_regs.h): `_MMIO_PIPE(id, 0x43230, 0x43270)`.
#[inline]
fn XE3_FBC_DIRTY_RECT(fbc_id: u32) -> I915RegT {
    mmio(if fbc_id == 0 { 0x43230 } else { 0x43270 })
}

/// `FBC_DIRTY_RECT_START_LINE(val)`: bits 15:0.
#[inline]
const fn FBC_DIRTY_RECT_START_LINE(val: i32) -> u32 {
    (val as u32) & 0xffff
}

/// `FBC_DIRTY_RECT_END_LINE(val)`: bits 31:16.
#[inline]
const fn FBC_DIRTY_RECT_END_LINE(val: i32) -> u32 {
    ((val as u32) << 16) & 0xffff_0000
}

/// `drm_rect_visible()` (drm_rect.h, inline).
#[inline]
fn drm_rect_visible(r: &DrmRect) -> bool {
    r.x2 > r.x1 && r.y2 > r.y1
}

/// `intel_fbc_program_dirty_rect()`.
// upstream: intel_fbc.c intel_fbc_program_dirty_rect()
unsafe fn intel_fbc_program_dirty_rect(
    dsb: *mut c_void,
    fbc: *mut IntelFbc,
    fbc_dirty_rect: &DrmRect,
) {
    unsafe {
        let display = (*fbc).display;
        // drm_WARN_ON(display->drm, fbc_dirty_rect->y2 == 0)
        drm_WARN_ON!((*display).drm, fbc_dirty_rect.y2 == 0);
        intel_de_write_dsb(
            display,
            dsb,
            XE3_FBC_DIRTY_RECT((*fbc).id),
            FBC_DIRTY_RECT_START_LINE(fbc_dirty_rect.y1)
                | FBC_DIRTY_RECT_END_LINE(fbc_dirty_rect.y2 - 1),
        );
    }
}

/// `intel_fbc_dirty_rect_update()`.
// upstream: intel_fbc.c intel_fbc_dirty_rect_update()
unsafe fn intel_fbc_dirty_rect_update(dsb: *mut c_void, fbc: *mut IntelFbc) {
    unsafe {
        let fbc_dirty_rect = &(*fbc).state.dirty_rect;
        if !drm_rect_visible(fbc_dirty_rect) {
            return;
        }
        intel_fbc_program_dirty_rect(dsb, fbc, fbc_dirty_rect);
    }
}

/// `intel_fbc_dirty_rect_update_noarm()`: program the dirty rectangle of the
/// plane's FBC under the FBC lock, without arming it.
// upstream: intel_fbc.c intel_fbc_dirty_rect_update_noarm()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_fbc_dirty_rect_update_noarm(
    dsb: *mut c_void,
    plane: *mut IntelPlane,
) {
    unsafe {
        let display = crate::intel_display_conversion_upstream::__drm_to_display((*plane).base.dev);
        let fbc = (*plane).fbc.cast::<IntelFbc>();

        if !HAS_FBC_DIRTY_RECT(display) {
            return;
        }

        mutex_lock(core::ptr::addr_of_mut!((*fbc).lock));
        if (*fbc).state.plane == plane {
            intel_fbc_dirty_rect_update(dsb, fbc);
        }
        mutex_unlock(core::ptr::addr_of_mut!((*fbc).lock));
    }
}
