// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// Source: drivers/gpu/drm/i915/display/intel_wm.c (Linux v7.2.3).
#![allow(
    non_snake_case,
    dead_code,
    unsafe_code,
    unsafe_op_in_unsafe_fn,
    clippy::missing_safety_doc
)]

use core::ptr;

use crate::{
    intel_display_core_types_upstream::{IntelDisplay, IntelWmFuncs},
    intel_display_limits_types_upstream::PLANE_CURSOR,
    intel_display_types_upstream::{IntelAtomicState, IntelCrtc, IntelCrtcState, IntelPlaneState},
    intel_display_wa_upstream::DISPLAY_VER,
    linux_i915_private::DrmI915Private,
};

pub(crate) static SKL_WM_FUNCS: IntelWmFuncs = IntelWmFuncs {
    update_wm: None,
    compute_watermarks: None,
    initial_watermarks: None,
    atomic_update_watermarks: None,
    optimize_watermarks: None,
    compute_global_watermarks: Some(crate::skl_watermark_upstream::skl_compute_wm),
    get_hw_state: Some(crate::skl_watermark_upstream::skl_wm_get_hw_state),
    sanitize: Some(crate::skl_watermark_upstream::skl_wm_sanitize),
};

unsafe fn funcs(display: *mut IntelDisplay) -> *mut IntelWmFuncs {
    if display.is_null() {
        ptr::null_mut()
    } else {
        (*display).wm.funcs
    }
}

unsafe fn display_from_atomic(state: *mut IntelAtomicState) -> *mut IntelDisplay {
    if state.is_null() || (*state).base.dev.is_null() || (*(*state).base.dev).dev_private.is_null()
    {
        return ptr::null_mut();
    }
    (*(*(*state).base.dev).dev_private.cast::<DrmI915Private>())
        .display
        .cast::<IntelDisplay>()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_wm_init(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    if DISPLAY_VER(display) >= 9 {
        crate::skl_watermark_upstream::skl_wm_init(display);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_update_watermarks(display: *mut IntelDisplay) {
    if let Some(update) = funcs(display).as_ref().and_then(|f| f.update_wm) {
        update(display);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_wm_compute(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
) -> i32 {
    let display = display_from_atomic(state);
    if display.is_null() {
        return -crate::linux_config::EINVAL;
    }
    funcs(display)
        .as_ref()
        .and_then(|f| f.compute_watermarks)
        .map_or(0, |compute| compute(state, crtc))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_initial_watermarks(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
) -> bool {
    let display = display_from_atomic(state);
    if display.is_null() {
        return false;
    }
    if let Some(initial) = funcs(display).as_ref().and_then(|f| f.initial_watermarks) {
        initial(state, crtc);
        true
    } else {
        false
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_update_watermarks(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
) {
    let display = display_from_atomic(state);
    if !display.is_null() {
        if let Some(update) = funcs(display)
            .as_ref()
            .and_then(|f| f.atomic_update_watermarks)
        {
            update(state, crtc);
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_optimize_watermarks(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
) {
    let display = display_from_atomic(state);
    if !display.is_null() {
        if let Some(optimize) = funcs(display).as_ref().and_then(|f| f.optimize_watermarks) {
            optimize(state, crtc);
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_compute_global_watermarks(state: *mut IntelAtomicState) -> i32 {
    let display = display_from_atomic(state);
    if display.is_null() {
        return -crate::linux_config::EINVAL;
    }
    funcs(display)
        .as_ref()
        .and_then(|f| f.compute_global_watermarks)
        .map_or(0, |compute| compute(state))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_wm_get_hw_state(display: *mut IntelDisplay) {
    if let Some(get) = funcs(display).as_ref().and_then(|f| f.get_hw_state) {
        get(display);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_wm_sanitize(display: *mut IntelDisplay) {
    if let Some(sanitize) = funcs(display).as_ref().and_then(|f| f.sanitize) {
        sanitize(display);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_wm_plane_visible(
    crtc_state: *const IntelCrtcState,
    plane_state: *const IntelPlaneState,
) -> bool {
    if crtc_state.is_null() || plane_state.is_null() || !(*crtc_state).hw.active {
        return false;
    }
    let plane = (*plane_state)
        .uapi
        .plane
        .cast::<crate::intel_display_types_upstream::IntelPlane>();
    if plane.is_null() {
        return false;
    }
    if (*plane).id == PLANE_CURSOR {
        !(*plane_state).hw.fb.is_null()
    } else {
        (*plane_state).uapi.visible
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_print_wm_latency(
    display: *mut IntelDisplay,
    name: *const i8,
    wm: *const u16,
) {
    if display.is_null() || wm.is_null() {
        return;
    }
    for level in 0..(*display).wm.num_levels as usize {
        let mut latency = *wm.add(level) as u32;
        if latency == 0 {
            continue;
        }
        if DISPLAY_VER(display) >= 9 {
            latency *= 10;
        } else if level > 0 {
            latency *= 5;
        }
        let name = if name.is_null() { c"WM".as_ptr() } else { name };
        drm_dbg!(
            &(*display).drm,
            "{} WM{} latency {} ({}.{} usec)\n",
            core::ffi::CStr::from_ptr(name)
                .to_string_lossy()
                .into_owned(),
            level,
            *wm.add(level),
            latency / 10,
            latency % 10
        );
    }
}

#[allow(dead_code)]
fn _unused_types() {
    let _ = &SKL_WM_FUNCS;
}
