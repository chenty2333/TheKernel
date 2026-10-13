// SPDX-License-Identifier: MIT
// Copyright 2023 Intel Corporation.
// Source: drivers/gpu/drm/i915/display/intel_link_bw.c (Linux v7.2.3)
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::{ffi::c_char, ptr};

use crate::{
    intel_atomic_upstream::intel_atomic_get_crtc_state,
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_limits_types_upstream::{I915_MAX_PIPES, Pipe},
    intel_display_types_upstream::{IntelAtomicState, IntelConnector, IntelCrtc, IntelCrtcState},
    intel_link_bw_types_upstream::IntelLinkBwLimits,
    linux_config::{EINVAL, ENOSPC},
};

unsafe extern "C" {
    fn intel_crtc_for_pipe(display: *mut IntelDisplay, pipe: Pipe) -> *mut IntelCrtc;
    fn intel_atomic_get_new_crtc_state(
        state: *mut IntelAtomicState,
        crtc: *mut IntelCrtc,
    ) -> *mut IntelCrtcState;
    fn intel_modeset_pipes_in_mask_early(
        state: *mut IntelAtomicState,
        reason: *const c_char,
        pipe_mask: u8,
    ) -> i32;
}

#[inline]
unsafe fn state_to_display(state: *mut IntelAtomicState) -> *mut IntelDisplay {
    if state.is_null() || unsafe { (*state).base.dev.is_null() } {
        ptr::null_mut()
    } else {
        unsafe { crate::intel_display_conversion_upstream::__drm_to_display((*state).base.dev) }
    }
}

unsafe fn forced_link_bpp_x16(state: *mut IntelAtomicState, crtc: *mut IntelCrtc) -> i32 {
    if state.is_null() || crtc.is_null() || unsafe { (*state).base.connectors.is_null() } {
        return 0;
    }
    let num = unsafe { (*state).base.num_connector.max(0) as usize };
    let mut minimum = i32::MAX;
    for i in 0..num {
        let entry = unsafe { (*state).base.connectors.add(i) };
        let conn_state = unsafe { (*entry).new_state };
        let conn = unsafe { (*entry).ptr }.cast::<IntelConnector>();
        if conn_state.is_null() || conn.is_null() {
            continue;
        }
        if unsafe { (*conn_state).crtc } != unsafe { ptr::addr_of_mut!((*crtc).base) } {
            continue;
        }
        let force = unsafe { (*conn).link.force_bpp_x16 };
        if force > 0 {
            minimum = minimum.min(force);
        }
    }
    if minimum == i32::MAX { 0 } else { minimum }
}

/// Initialize per-pipe link-BPP ceilings from duplicated state and connector
/// force settings, matching the Linux atomic link-bandwidth pass.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_link_bw_init_limits(
    state: *mut IntelAtomicState,
    limits: *mut IntelLinkBwLimits,
) {
    if state.is_null() || limits.is_null() {
        return;
    }
    let display = unsafe { state_to_display(state) };
    if display.is_null() {
        return;
    }
    unsafe {
        (*limits).link_dsc_pipes = 0;
        (*limits).bpp_limit_reached_pipes = 0;
    }
    for pipe in 0..I915_MAX_PIPES {
        let crtc = unsafe { intel_crtc_for_pipe(display, pipe) };
        if crtc.is_null() {
            unsafe { (*limits).max_bpp_x16[pipe as usize] = i32::MAX };
            continue;
        }
        let crtc_state = unsafe { intel_atomic_get_new_crtc_state(state, crtc) };
        let mut max_bpp = if unsafe { (*state).base.duplicated() } && !crtc_state.is_null() {
            if unsafe { (*crtc_state).dsc.compression_enabled_on_link } {
                (*limits).link_dsc_pipes |= 1u8 << pipe;
            }
            unsafe { (*crtc_state).max_link_bpp_x16 as i32 }
        } else {
            i32::MAX
        };
        let forced = unsafe { forced_link_bpp_x16(state, crtc) };
        if forced != 0 {
            max_bpp = max_bpp.min(forced);
        }
        unsafe { (*limits).max_bpp_x16[pipe as usize] = max_bpp };
    }
}

#[inline]
fn pipe_bit(pipe: Pipe) -> Option<u8> {
    if pipe < 0 || pipe >= I915_MAX_PIPES {
        None
    } else {
        Some(1u8 << pipe)
    }
}

unsafe fn reduce_bpp(
    state: *mut IntelAtomicState,
    limits: *mut IntelLinkBwLimits,
    pipe_mask: u8,
    reason: *const c_char,
    reduce_forced_bpp: bool,
) -> i32 {
    if state.is_null() || limits.is_null() {
        return -EINVAL;
    }
    let display = unsafe { state_to_display(state) };
    if display.is_null() {
        return -EINVAL;
    }
    let mut selected_crtc = ptr::null_mut::<IntelCrtc>();
    let mut selected_pipe = -1;
    let mut max_link_bpp = 0i32;
    for pipe in 0..I915_MAX_PIPES {
        let Some(bit) = pipe_bit(pipe) else { continue };
        if pipe_mask & bit == 0 || unsafe { (*limits).bpp_limit_reached_pipes & bit != 0 } {
            continue;
        }
        let crtc = unsafe { intel_crtc_for_pipe(display, pipe) };
        if crtc.is_null() {
            continue;
        }
        let crtc_state =
            unsafe { intel_atomic_get_crtc_state(ptr::addr_of_mut!((*state).base), crtc) };
        if unsafe { tk_linux_kapi::config::IS_ERR(crtc_state) } {
            return unsafe { tk_linux_kapi::config::PTR_ERR(crtc_state) };
        }
        if crtc_state.is_null() {
            continue;
        }
        let link_bpp = if unsafe { (*crtc_state).dsc.compression_enable } {
            unsafe { (*crtc_state).dsc.compressed_bpp_x16 as i32 }
        } else {
            // Linux stores pipe bpp in whole bits; convert to Q4.
            unsafe { (*crtc_state).pipe_bpp.saturating_mul(16) }
        };
        let forced = unsafe { forced_link_bpp_x16(state, crtc) };
        if !reduce_forced_bpp && forced != 0 && link_bpp <= forced {
            continue;
        }
        if link_bpp > max_link_bpp {
            max_link_bpp = link_bpp;
            selected_crtc = crtc;
            selected_pipe = pipe;
        }
    }
    if selected_pipe < 0 || selected_crtc.is_null() {
        return -ENOSPC;
    }
    let bit = 1u8 << selected_pipe;
    unsafe { (*limits).max_bpp_x16[selected_pipe as usize] = max_link_bpp.saturating_sub(1) };
    unsafe { intel_modeset_pipes_in_mask_early(state, reason, bit) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_link_bw_reduce_bpp(
    state: *mut IntelAtomicState,
    limits: *mut IntelLinkBwLimits,
    pipe_mask: u8,
    reason: *const c_char,
) -> i32 {
    let mut ret = unsafe { reduce_bpp(state, limits, pipe_mask, reason, false) };
    if ret == -ENOSPC {
        ret = unsafe { reduce_bpp(state, limits, pipe_mask, reason, true) };
    }
    ret
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_link_bw_compute_pipe_bpp(state: *mut IntelCrtcState) -> bool {
    if state.is_null() {
        return false;
    }
    let max_bpp = unsafe { (*state).max_link_bpp_x16 };
    let pipe_bpp = unsafe { (*state).pipe_bpp }.min(max_bpp >> 4);
    let pipe_bpp = pipe_bpp.div_euclid(6) * 6;
    if pipe_bpp < 18 {
        return false;
    }
    unsafe { (*state).pipe_bpp = pipe_bpp };
    true
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_link_bw_set_bpp_limit_for_pipe(
    state: *mut IntelAtomicState,
    old_limits: *const IntelLinkBwLimits,
    new_limits: *mut IntelLinkBwLimits,
    pipe: Pipe,
) -> bool {
    if state.is_null() || old_limits.is_null() || new_limits.is_null() {
        return false;
    }
    let Some(bit) = pipe_bit(pipe) else {
        return false;
    };
    unsafe {
        if (*new_limits).max_bpp_x16[pipe as usize] == (*old_limits).max_bpp_x16[pipe as usize]
            || (*new_limits).bpp_limit_reached_pipes & bit != 0
        {
            return false;
        }
        (*new_limits).max_bpp_x16[pipe as usize] = (*old_limits).max_bpp_x16[pipe as usize];
        (*new_limits).bpp_limit_reached_pipes |= bit;
    }
    true
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_link_bw_atomic_check(
    state: *mut IntelAtomicState,
    _new_limits: *mut IntelLinkBwLimits,
) -> i32 {
    if state.is_null() || unsafe { state_to_display(state).is_null() } {
        return -EINVAL;
    }
    // ADL-P external SST has no shared MST, tunneled-DP, or FDI link. Its
    // per-pipe bandwidth has already been checked during DP config compute.
    // Leave unsupported shared-link protocols to their owning batches rather
    // than fabricating an availability result here.
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_link_bw_connector_debugfs_add(_connector: *mut IntelConnector) {
    // TheKernel's display debugfs surface is outside the ADL-P SST path.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_bpp_rounds_down_to_rgb_or_yuv_420_multiples() {
        let mut state: IntelCrtcState = unsafe { core::mem::zeroed() };
        state.pipe_bpp = 34;
        state.max_link_bpp_x16 = 40 << 4;
        assert!(unsafe { intel_link_bw_compute_pipe_bpp(&mut state) });
        assert_eq!(state.pipe_bpp, 30);
        state.pipe_bpp = 23;
        state.max_link_bpp_x16 = 24 << 4;
        assert!(unsafe { intel_link_bw_compute_pipe_bpp(&mut state) });
        assert_eq!(state.pipe_bpp, 18);
        state.pipe_bpp = 17;
        state.max_link_bpp_x16 = 17 << 4;
        assert!(!unsafe { intel_link_bw_compute_pipe_bpp(&mut state) });
    }

    #[test]
    fn pipe_mask_is_bounded_to_four_pipes() {
        assert_eq!(pipe_bit(0), Some(1));
        assert_eq!(pipe_bit(3), Some(8));
        assert_eq!(pipe_bit(4), None);
        assert_eq!(pipe_bit(-1), None);
    }
}
