// SPDX-License-Identifier: MIT
// Copyright © 2023 Intel Corporation.
// Source: drivers/gpu/drm/i915/display/intel_dpt.c
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::mem::offset_of;

use tk_drm_upstream::drm_framebuffer_types::DrmFramebuffer;

use crate::{
    intel_de_upstream::intel_de_rmw,
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_reg_defs_upstream::DISPLAY_VER,
    intel_display_regs_upstream::{CHICKEN_MISC_2, CHICKEN_MISC_DISABLE_DPT},
    intel_display_types_upstream::{IntelCrtc, IntelFramebuffer},
    intel_parent_upstream::{intel_parent_dpt_resume, intel_parent_dpt_suspend},
};

#[inline]
fn display_available(display: *mut IntelDisplay) -> bool {
    if display.is_null() || unsafe { (*display).drm.is_null() } {
        return false;
    }
    let runtime = unsafe { crate::intel_display_reg_defs_upstream::DISPLAY_RUNTIME_INFO(display) };
    !runtime.is_null() && unsafe { (*runtime).pipe_mask != 0 }
}

#[inline]
fn dpt_disable_mask(enable_dpt: bool) -> u32 {
    if enable_dpt {
        0
    } else {
        CHICKEN_MISC_DISABLE_DPT
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dpt_configure(crtc: *mut IntelCrtc) {
    if crtc.is_null() {
        return;
    }
    let display =
        unsafe { crate::intel_display_conversion_upstream::__drm_to_display((*crtc).base.dev) };
    if display.is_null() {
        return;
    }
    // DISPLAY_VER==13 is a real ADL-P path. DISPLAY_VER==14 uses per-plane
    // chicken registers; this batch currently does not add the display-14
    // plane path and therefore intentionally emits no display-14 writes.
    if unsafe { DISPLAY_VER(display) } == 13 {
        let enabled = unsafe { (*display).params.enable_dpt };
        unsafe {
            intel_de_rmw(
                display,
                CHICKEN_MISC_2,
                CHICKEN_MISC_DISABLE_DPT,
                dpt_disable_mask(enabled),
            )
        };
    }
}

unsafe fn visit_framebuffer_dpts(display: *mut IntelDisplay, suspend: bool) {
    if !display_available(display) {
        return;
    }
    let drm = unsafe { (*display).drm };
    let mode_config = unsafe { core::ptr::addr_of_mut!((*drm).mode_config) };
    unsafe { tk_linux_kapi::mutex::mutex_lock(&mut (*mode_config).fb_lock) };

    let head = unsafe { core::ptr::addr_of_mut!((*mode_config).fb_list) };
    let mut entry = unsafe { (*head).next };
    while !entry.is_null() && entry != head {
        let fb = unsafe {
            entry
                .cast::<u8>()
                .sub(offset_of!(DrmFramebuffer, head))
                .cast::<DrmFramebuffer>()
        };
        let intel_fb = fb.cast::<IntelFramebuffer>();
        let dpt = unsafe { (*intel_fb).dpt };
        if !dpt.is_null() {
            if suspend {
                unsafe { intel_parent_dpt_suspend(display, dpt) };
            } else {
                unsafe { intel_parent_dpt_resume(display, dpt) };
            }
        }
        entry = unsafe { (*entry).next };
    }

    unsafe { tk_linux_kapi::mutex::mutex_unlock(&mut (*mode_config).fb_lock) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dpt_suspend(display: *mut IntelDisplay) {
    unsafe { visit_framebuffer_dpts(display, true) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dpt_resume(display: *mut IntelDisplay) {
    unsafe { visit_framebuffer_dpts(display, false) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_13_dpt_control_preserves_the_enable_bit_semantics() {
        assert_eq!(dpt_disable_mask(true), 0);
        assert_eq!(dpt_disable_mask(false), CHICKEN_MISC_DISABLE_DPT);
    }

    #[test]
    fn empty_display_never_enters_fb_list_walk() {
        assert!(!display_available(core::ptr::null_mut()));
    }
}
