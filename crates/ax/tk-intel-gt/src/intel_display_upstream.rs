// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// ADL-P modesetting and atomic-commit path translated from Linux
// drivers/gpu/drm/i915/display/intel_display.c (Linux v7.2.3).
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::{ffi::c_void, mem::offset_of, ptr};

use tk_drm_upstream::{
    drm_atomic_types::DrmAtomicCommit,
    drm_connector_types::{DrmConnector, DrmConnectorState},
    drm_crtc_types::{DrmCrtc, DrmCrtcState},
    drm_device_types::DrmDevice,
    drm_encoder_types::DrmEncoder,
    drm_modes_types::{
        DrmDisplayMode, DrmModeStatus, MODE_BAD, MODE_CLOCK_HIGH, MODE_H_ILLEGAL, MODE_HSYNC,
        MODE_NO_VSCAN, MODE_NOCLOCK, MODE_OK, MODE_V_ILLEGAL,
    },
    drm_modeset_lock_types::DrmModesetAcquireCtx,
    drm_plane_types::DrmPlane,
    uapi_drm_consts::{
        DRM_MODE_FLAG_BCAST, DRM_MODE_FLAG_CLKDIV2, DRM_MODE_FLAG_CSYNC, DRM_MODE_FLAG_HSKEW,
        DRM_MODE_FLAG_NCSYNC, DRM_MODE_FLAG_NHSYNC, DRM_MODE_FLAG_NVSYNC, DRM_MODE_FLAG_PCSYNC,
        DRM_MODE_FLAG_PHSYNC, DRM_MODE_FLAG_PIXMUX, DRM_MODE_FLAG_PVSYNC,
    },
};
use tk_linux_kapi::{
    config::{EINVAL, ENODEV, ENOMEM, EOPNOTSUPP, GFP_KERNEL, IS_ERR, PTR_ERR},
    memory::{kfree, kzalloc_objs},
    types::ListHead,
    workqueue::{flush_workqueue, queue_work},
};

use crate::{
    intel_bios_upstream::IntelBiosEncoderData,
    intel_display_core_types_upstream::{IntelDisplay, IntelModesetFuncs},
    intel_display_device_types_upstream::IntelDisplayDeviceInfo,
    intel_display_device_upstream::intel_display_device_present,
    intel_display_limits_types_upstream::{I915_MAX_PIPES, Pipe},
    intel_display_types_upstream::{
        IntelAtomicState, IntelCrtc, IntelCrtcState, IntelEncoder, IntelPlane, IntelPlaneState,
    },
    intel_link_bw_types_upstream::IntelLinkBwLimits,
};

const DISPLAY_VER_SCALED_PLANES: u16 = 9;
const DISPLAY_VER_ADLP: u16 = 13;

const HAS_DDI_BIT: u8 = 1 << 3;

unsafe extern "C" {
    fn intel_pps_unlock_regs_wa(display: *mut IntelDisplay);
    fn intel_bios_for_each_encoder(
        display: *mut IntelDisplay,
        callback: unsafe extern "C" fn(*mut IntelDisplay, *const IntelBiosEncoderData),
    );
    fn intel_ddi_init(display: *mut IntelDisplay, devdata: *const IntelBiosEncoderData);
    fn intel_init_pch_refclk(display: *mut IntelDisplay);
    fn intel_initial_plane_vblank_wait(crtc: *mut IntelCrtc);
    fn intel_set_memory_cxsr(display: *mut IntelDisplay, enable: bool) -> bool;
    fn hsw_ips_disable(state: *mut IntelCrtcState) -> bool;
    fn intel_dpll_crtc_get_dpll(state: *mut IntelAtomicState, crtc: *mut IntelCrtc) -> i32;
    fn skl_update_scaler_crtc(state: *mut IntelCrtcState) -> i32;
    fn intel_atomic_setup_scalers(state: *mut IntelAtomicState, crtc: *mut IntelCrtc) -> i32;
    fn intel_dpll_crtc_compute_clock(state: *mut IntelAtomicState, crtc: *mut IntelCrtc) -> i32;
    fn intel_link_bw_init_limits(state: *mut IntelAtomicState, limits: *mut IntelLinkBwLimits);
    fn intel_link_bw_set_bpp_limit_for_pipe(
        state: *mut IntelAtomicState,
        old_limits: *const IntelLinkBwLimits,
        new_limits: *mut IntelLinkBwLimits,
        pipe: Pipe,
    ) -> bool;
    fn intel_link_bw_atomic_check(
        state: *mut IntelAtomicState,
        limits: *mut IntelLinkBwLimits,
    ) -> i32;
    fn drm_atomic_helper_check_modeset(dev: *mut DrmDevice, state: *mut DrmAtomicCommit) -> i32;
    fn drm_mode_copy(dst: *mut DrmDisplayMode, src: *const DrmDisplayMode);
    fn drm_mode_set_name(mode: *mut DrmDisplayMode);
    fn drm_mode_get_hv_timing(mode: *const DrmDisplayMode, hdisplay: *mut i32, vdisplay: *mut i32);
    fn drm_mode_set_crtcinfo(mode: *mut DrmDisplayMode, flags: i32);
    fn drm_property_replace_blob(
        blob: *mut *mut tk_drm_upstream::drm_property_types::DrmPropertyBlob,
        new_blob: *mut tk_drm_upstream::drm_property_types::DrmPropertyBlob,
    ) -> bool;
    fn drm_plane_mask(plane: *const DrmPlane) -> u32;
    fn intel_dpll_release(state: *mut IntelAtomicState, crtc: *mut IntelCrtc);
    fn intel_crtc_min_cdclk(state: *const IntelCrtcState) -> i32;
    fn intel_compute_global_watermarks(state: *mut IntelAtomicState) -> i32;
    fn intel_bw_atomic_check(state: *mut IntelAtomicState) -> i32;
    fn intel_cdclk_atomic_check(state: *mut IntelAtomicState) -> i32;
    fn intel_pmdemand_atomic_check(state: *mut IntelAtomicState) -> i32;
    fn intel_atomic_global_state_setup_commit(state: *mut IntelAtomicState) -> i32;
    fn intel_atomic_swap_global_state(state: *mut IntelAtomicState);
    fn intel_atomic_global_state_wait_for_dependencies(state: *mut IntelAtomicState);
    fn intel_atomic_global_state_commit_done(state: *mut IntelAtomicState);
    fn intel_plane_atomic_check(state: *mut IntelAtomicState) -> i32;
    fn drm_atomic_commit_alloc(dev: *mut DrmDevice) -> *mut DrmAtomicCommit;
    fn drm_atomic_commit_clear(state: *mut DrmAtomicCommit);
    fn drm_atomic_commit_put(state: *mut DrmAtomicCommit);
    fn drm_atomic_commit(state: *mut DrmAtomicCommit) -> i32;
    fn drm_atomic_add_affected_planes(state: *mut DrmAtomicCommit, crtc: *mut DrmCrtc) -> i32;
    fn drm_atomic_add_affected_connectors(state: *mut DrmAtomicCommit, crtc: *mut DrmCrtc) -> i32;
    fn drm_modeset_acquire_init(ctx: *mut DrmModesetAcquireCtx, flags: u32);
    fn drm_modeset_backoff(ctx: *mut DrmModesetAcquireCtx) -> i32;
    fn drm_modeset_drop_locks(ctx: *mut DrmModesetAcquireCtx);
    fn drm_modeset_acquire_fini(ctx: *mut DrmModesetAcquireCtx);
}

#[inline]
unsafe fn display_ver(display: *const IntelDisplay) -> u16 {
    if display.is_null() {
        0
    } else {
        unsafe { (*display).info.__runtime_info.ip.ver }
    }
}

#[inline]
unsafe fn platform_has(display: *const IntelDisplay, bit: u32) -> bool {
    if display.is_null() || bit >= 128 {
        return false;
    }
    unsafe { (*display).platform._anon_0_0.bitmap[(bit / 64) as usize] & (1u64 << (bit % 64)) != 0 }
}

#[inline]
unsafe fn has_ddi(display: *const IntelDisplay) -> bool {
    if display.is_null() || unsafe { (*display).info.__device_info.is_null() } {
        return false;
    }
    unsafe { (*(*display).info.__device_info)._bitfields_46[0] & HAS_DDI_BIT != 0 }
}

#[inline]
unsafe fn max_dotclock(display: *const IntelDisplay) -> i32 {
    if display.is_null() {
        0
    } else {
        unsafe { (*display).cdclk.max_dotclk_freq as i32 }
    }
}

/// Maximum uncompressed dot clock accepted by the display IP.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_max_uncompressed_dotclock(display: *mut IntelDisplay) -> i32 {
    if display.is_null() {
        return 0;
    }
    let ver = unsafe { display_ver(display) };
    let mut limit = unsafe { max_dotclock(display) };
    if ver >= 30 {
        limit = limit.min(1_350_000);
    }
    limit
}

/// Validate generic timing limits before encoder-specific validation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_mode_valid(
    dev: *mut DrmDevice,
    mode: *mut DrmDisplayMode,
) -> DrmModeStatus {
    if dev.is_null() || mode.is_null() {
        return MODE_BAD;
    }
    let display = unsafe { crate::intel_display_conversion_upstream::__drm_to_display(dev) };
    if !unsafe { intel_display_device_present(display) } {
        return MODE_BAD;
    }
    let mode = unsafe { &*mode };
    if mode.vscan > 1 {
        return MODE_NO_VSCAN;
    }
    if mode.flags & DRM_MODE_FLAG_HSKEW != 0 {
        return MODE_H_ILLEGAL;
    }
    if mode.flags & (DRM_MODE_FLAG_CSYNC | DRM_MODE_FLAG_NCSYNC | DRM_MODE_FLAG_PCSYNC) != 0 {
        return MODE_HSYNC;
    }
    if mode.flags & (DRM_MODE_FLAG_BCAST | DRM_MODE_FLAG_PIXMUX | DRM_MODE_FLAG_CLKDIV2) != 0 {
        return MODE_BAD;
    }
    if mode.clock <= 0 {
        return MODE_NOCLOCK;
    }

    let ver = unsafe { display_ver(display) };
    let max_clock = unsafe { max_dotclock(display) };
    if mode.clock > max_clock {
        return MODE_CLOCK_HIGH;
    }
    let (hdisplay_max, vdisplay_max, htotal_max, vtotal_max) = if ver >= 11 {
        (16_384u16, 8_192u16, 16_384u16, 8_192u16)
    } else if ver >= 9 {
        (8_192, 4_096, 8_192, 8_192)
    } else if ver >= 3 {
        (4_096, 4_096, 8_192, 8_192)
    } else {
        (2_048, 2_048, 4_096, 4_096)
    };
    if mode.hdisplay > hdisplay_max
        || mode.hsync_start > htotal_max
        || mode.hsync_end > htotal_max
        || mode.htotal > htotal_max
    {
        return MODE_H_ILLEGAL;
    }
    if mode.vdisplay > vdisplay_max
        || mode.vsync_start > vtotal_max
        || mode.vsync_end > vtotal_max
        || mode.vtotal > vtotal_max
    {
        return MODE_V_ILLEGAL;
    }
    let line_time_num = i64::from(mode.htotal) * 1_000;
    let line_time_us = (line_time_num + i64::from(mode.clock) - 1) / i64::from(mode.clock);
    if line_time_us > 64 {
        return MODE_H_ILLEGAL;
    }
    MODE_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_cpu_transcoder_mode_valid(
    display: *mut IntelDisplay,
    mode: *const DrmDisplayMode,
) -> DrmModeStatus {
    if display.is_null() || mode.is_null() {
        return MODE_BAD;
    }
    let mode = unsafe { &*mode };
    let ver = unsafe { display_ver(display) };
    if ver >= 5 {
        if mode.hdisplay < 64 || mode.htotal.saturating_sub(mode.hdisplay) < 32 {
            return MODE_H_ILLEGAL;
        }
        if mode.vtotal.saturating_sub(mode.vdisplay) < 5 {
            return MODE_V_ILLEGAL;
        }
    } else {
        if mode.htotal.saturating_sub(mode.hdisplay) < 32 {
            return MODE_H_ILLEGAL;
        }
        if mode.vtotal.saturating_sub(mode.vdisplay) < 3 {
            return MODE_V_ILLEGAL;
        }
    }
    if (ver >= 5 || unsafe { platform_has(display, 0) }) && mode.hsync_start == mode.hdisplay {
        return MODE_H_ILLEGAL;
    }
    MODE_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_mode_valid_max_plane_size(
    display: *mut IntelDisplay,
    mode: *const DrmDisplayMode,
    num_joined_pipes: i32,
) -> DrmModeStatus {
    if display.is_null() || mode.is_null() {
        return MODE_BAD;
    }
    let ver = unsafe { display_ver(display) };
    if ver < DISPLAY_VER_SCALED_PLANES {
        return MODE_OK;
    }
    let joins = num_joined_pipes.max(1);
    let (max_width, max_height) = if ver >= 30 {
        (6_144i32.saturating_mul(joins), 4_800)
    } else if ver >= 11 {
        (5_120i32.saturating_mul(joins), 4_320)
    } else {
        (5_120, 4_096)
    };
    let mode = unsafe { &*mode };
    if i32::from(mode.hdisplay) > max_width {
        return MODE_H_ILLEGAL;
    }
    if i32::from(mode.vdisplay) > max_height {
        return MODE_V_ILLEGAL;
    }
    MODE_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_min_pipe_bpp() -> i32 {
    18
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_max_pipe_bpp(display: *mut IntelDisplay) -> i32 {
    if display.is_null() {
        return 0;
    }
    if unsafe { platform_has(display, 0) || platform_has(display, 20) || platform_has(display, 27) }
    {
        30
    } else if unsafe { display_ver(display) >= 5 } {
        36
    } else {
        24
    }
}

unsafe fn for_each_new_connector_in_state(
    state: *mut IntelAtomicState,
    mut f: impl FnMut(*mut DrmConnector, *mut DrmConnectorState) -> i32,
) -> i32 {
    if state.is_null() || unsafe { (*state).base.connectors.is_null() } {
        return if state.is_null() { -EINVAL } else { 0 };
    }
    let count = unsafe { (*state).base.num_connector.max(0) as usize };
    for index in 0..count {
        let entry = unsafe { (*state).base.connectors.add(index) };
        let connector = unsafe { (*entry).ptr };
        let new_state = unsafe { (*entry).new_state };
        if connector.is_null() || new_state.is_null() {
            continue;
        }
        let ret = f(connector, new_state);
        if ret != 0 {
            return ret;
        }
    }
    0
}

fn bpc_to_bpp(bpc: i32) -> i32 {
    match bpc {
        6..=7 => 18,
        8..=9 => 24,
        10..=11 => 30,
        12..=16 => 36,
        _ => -EINVAL,
    }
}

unsafe fn compute_sink_pipe_bpp(
    connector_state: *const DrmConnectorState,
    crtc_state: *mut IntelCrtcState,
) -> i32 {
    if connector_state.is_null() || crtc_state.is_null() {
        return -EINVAL;
    }
    let connector = unsafe { (*connector_state).connector };
    if connector.is_null() {
        return -EINVAL;
    }
    let display =
        unsafe { crate::intel_display_conversion_upstream::__drm_to_display((*connector).dev) };
    if display.is_null() {
        return -EINVAL;
    }
    let edid_bpc = match unsafe { (*connector).display_info.bpc } {
        0 => 8,
        value => value as i32,
    };
    let max_edid_bpp = bpc_to_bpp(edid_bpc);
    let target_bpp = bpc_to_bpp(unsafe { (*connector_state).max_bpc as i32 });
    if max_edid_bpp < 0 || target_bpp < 0 {
        return -EINVAL;
    }
    unsafe {
        (*crtc_state).max_pipe_bpp = (*crtc_state).pipe_bpp.min(max_edid_bpp);
        if target_bpp < (*crtc_state).pipe_bpp {
            (*crtc_state).pipe_bpp = target_bpp;
        }
    }
    0
}

unsafe fn compute_baseline_pipe_bpp(state: *mut IntelAtomicState, crtc: *mut IntelCrtc) -> i32 {
    if state.is_null() || crtc.is_null() {
        return -EINVAL;
    }
    let display =
        unsafe { crate::intel_display_conversion_upstream::__drm_to_display((*crtc).base.dev) };
    if display.is_null() {
        return -EINVAL;
    }
    let crtc_state =
        unsafe { crate::intel_atomic_upstream::intel_atomic_get_new_crtc_state(state, crtc) };
    if crtc_state.is_null() {
        return -EINVAL;
    }
    unsafe { (*crtc_state).pipe_bpp = intel_display_max_pipe_bpp(display) };
    unsafe {
        for_each_new_connector_in_state(state, |_, connector_state| {
            if (*connector_state).crtc != &mut (*crtc).base {
                return 0;
            }
            compute_sink_pipe_bpp(connector_state, crtc_state)
        })
    }
}

unsafe fn intel_mode_from_crtc_timings(mode: *mut DrmDisplayMode, timings: *const DrmDisplayMode) {
    if mode.is_null() || timings.is_null() {
        return;
    }
    unsafe {
        (*mode).hdisplay = (*timings).crtc_hdisplay;
        (*mode).htotal = (*timings).crtc_htotal;
        (*mode).hsync_start = (*timings).crtc_hsync_start;
        (*mode).hsync_end = (*timings).crtc_hsync_end;
        (*mode).vdisplay = (*timings).crtc_vdisplay;
        (*mode).vtotal = (*timings).crtc_vtotal;
        (*mode).vsync_start = (*timings).crtc_vsync_start;
        (*mode).vsync_end = (*timings).crtc_vsync_end;
        (*mode).flags = (*timings).flags;
        (*mode).r#type = 1; // DRM_MODE_TYPE_DRIVER
        (*mode).clock = (*timings).crtc_clock;
        drm_mode_set_name(mode);
    }
}

unsafe fn intel_crtc_compute_pixel_rate(state: *mut IntelCrtcState) {
    if state.is_null() {
        return;
    }
    if unsafe { (*state).pch_pfit.enabled } {
        let mut src = tk_drm_upstream::drm_rect_types::DrmRect {
            x1: 0,
            y1: 0,
            x2: (unsafe { (*state).pipe_src.x2 - (*state).pipe_src.x1 }) << 16,
            y2: (unsafe { (*state).pipe_src.y2 - (*state).pipe_src.y1 }) << 16,
        };
        unsafe {
            (*state).pixel_rate = crate::intel_plane_upstream::intel_adjusted_rate(
                &src,
                &(*state).pch_pfit.dst,
                (*state).hw.pipe_mode.crtc_clock as u32,
            );
        }
        let _ = &mut src;
    } else {
        unsafe { (*state).pixel_rate = (*state).hw.pipe_mode.crtc_clock.max(0) as u32 };
    }
}

unsafe fn intel_crtc_compute_pipe_src(state: *mut IntelCrtcState) -> i32 {
    if state.is_null() || unsafe { (*state).uapi.crtc.is_null() } {
        return -EINVAL;
    }
    if unsafe { (*state).joiner_pipes != 0 || (*state).splitter.enable } {
        return -EOPNOTSUPP;
    }
    if unsafe { ((*state).pipe_src.x2 - (*state).pipe_src.x1) & 1 != 0 && (*state).double_wide } {
        return -EINVAL;
    }
    0
}

unsafe fn intel_crtc_compute_pipe_mode(state: *mut IntelCrtcState) -> i32 {
    if state.is_null() || unsafe { (*state).uapi.crtc.is_null() } {
        return -EINVAL;
    }
    let display = unsafe {
        crate::intel_display_conversion_upstream::__drm_to_display((*(*state).uapi.crtc).dev)
    };
    if display.is_null() {
        return -EINVAL;
    }
    unsafe { drm_mode_copy(&mut (*state).hw.pipe_mode, &(*state).hw.adjusted_mode) };
    let clock = unsafe { (*state).hw.pipe_mode.crtc_clock };
    let limit = unsafe { (*display).cdclk.max_dotclk_freq as i32 };
    if clock <= 0 || clock > limit {
        return -EINVAL;
    }
    0
}

unsafe fn intel_crtc_compute_config(state: *mut IntelAtomicState, crtc: *mut IntelCrtc) -> i32 {
    if state.is_null() || crtc.is_null() {
        return -EINVAL;
    }
    let crtc_state =
        unsafe { crate::intel_atomic_upstream::intel_atomic_get_new_crtc_state(state, crtc) };
    if crtc_state.is_null() {
        return -EINVAL;
    }
    let ret = unsafe { intel_dpll_crtc_compute_clock(state, crtc) };
    if ret != 0 {
        return ret;
    }
    // PSR is compile-time disabled in the ADL-P bring-up, so the source's
    // optional DSB/PSR minimum context-latency contribution is zero.
    unsafe { (*crtc_state).set_context_latency = 0 };
    let ret = unsafe { intel_crtc_compute_pipe_src(crtc_state) };
    if ret != 0 {
        return ret;
    }
    let ret = unsafe { intel_crtc_compute_pipe_mode(crtc_state) };
    if ret != 0 {
        return ret;
    }
    unsafe { intel_crtc_compute_pixel_rate(crtc_state) };
    0
}

unsafe fn check_single_encoder_cloning(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
    encoder: *const IntelEncoder,
) -> bool {
    if state.is_null() || crtc.is_null() || encoder.is_null() {
        return false;
    }
    let mut valid = true;
    unsafe {
        let _ = for_each_new_connector_in_state(state, |_, connector_state| {
            if (*connector_state).crtc != &mut (*crtc).base {
                return 0;
            }
            let other = (*connector_state).best_encoder.cast::<IntelEncoder>();
            if !encoders_cloneable(encoder, other) {
                valid = false;
            }
            0
        });
    }
    valid
}

/// Full display-IP modeset pipe configuration used by the atomic retry loop.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_modeset_pipe_config(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
    limits: *const IntelLinkBwLimits,
) -> i32 {
    if state.is_null() || crtc.is_null() || limits.is_null() {
        return -EINVAL;
    }
    let display =
        unsafe { crate::intel_display_conversion_upstream::__drm_to_display((*crtc).base.dev) };
    let crtc_state =
        unsafe { crate::intel_atomic_upstream::intel_atomic_get_new_crtc_state(state, crtc) };
    if display.is_null()
        || crtc_state.is_null()
        || unsafe { (*crtc).pipe < 0 || (*crtc).pipe >= I915_MAX_PIPES }
    {
        return -EINVAL;
    }
    let pipe = unsafe { (*crtc).pipe as usize };
    unsafe {
        (*crtc_state).cpu_transcoder = (*crtc).pipe;
        (*crtc_state).framestart_delay = 1;
        if (*crtc_state).hw.adjusted_mode.flags & (DRM_MODE_FLAG_PHSYNC | DRM_MODE_FLAG_NHSYNC) == 0
        {
            (*crtc_state).hw.adjusted_mode.flags |= DRM_MODE_FLAG_NHSYNC;
        }
        if (*crtc_state).hw.adjusted_mode.flags & (DRM_MODE_FLAG_PVSYNC | DRM_MODE_FLAG_NVSYNC) == 0
        {
            (*crtc_state).hw.adjusted_mode.flags |= DRM_MODE_FLAG_NVSYNC;
        }
    }
    let ret = unsafe { compute_baseline_pipe_bpp(state, crtc) };
    if ret != 0 {
        return ret;
    }
    unsafe {
        (*crtc_state).dsc.compression_enabled_on_link =
            (*limits).link_dsc_pipes & (1u8 << pipe) != 0;
        (*crtc_state).max_link_bpp_x16 = (*limits).max_bpp_x16[pipe];
        if (*crtc_state).pipe_bpp.saturating_mul(16) > (*crtc_state).max_link_bpp_x16 {
            (*crtc_state).bw_constrained = true;
        }
        let mut hdisplay = 0;
        let mut vdisplay = 0;
        drm_mode_get_hv_timing(&(*crtc_state).hw.mode, &mut hdisplay, &mut vdisplay);
        tk_drm_upstream::drm_rect_upstream::drm_rect_init(
            &mut (*crtc_state).pipe_src,
            0,
            0,
            hdisplay,
            vdisplay,
        );
        (*crtc_state).output_types = 0;
        (*crtc_state).port_clock = 0;
        (*crtc_state).pixel_multiplier = 1;
    }

    let ret = unsafe {
        for_each_new_connector_in_state(state, |_, connector_state| {
            if (*connector_state).crtc != &mut (*crtc).base {
                return 0;
            }
            let encoder = (*connector_state).best_encoder.cast::<IntelEncoder>();
            if encoder.is_null() {
                return -EINVAL;
            }
            if !check_single_encoder_cloning(state, crtc, encoder) {
                return -EINVAL;
            }
            let output_type = if let Some(compute) = (*encoder).compute_output_type {
                compute(encoder, crtc_state, connector_state)
            } else {
                (*encoder).r#type
            };
            if output_type >= 32 {
                return -EINVAL;
            }
            (*crtc_state).output_types |= 1u32 << output_type;
            0
        })
    };
    if ret != 0 {
        return ret;
    }

    unsafe { drm_mode_set_crtcinfo(&mut (*crtc_state).hw.adjusted_mode, 1 << 1) };
    let ret = unsafe {
        for_each_new_connector_in_state(state, |_, connector_state| {
            if (*connector_state).crtc != &mut (*crtc).base {
                return 0;
            }
            let encoder = (*connector_state).best_encoder.cast::<IntelEncoder>();
            if encoder.is_null() {
                return -EINVAL;
            }
            let compute = match (*encoder).compute_config {
                Some(compute) => compute,
                None => return -EOPNOTSUPP,
            };
            compute(encoder, crtc_state, connector_state)
        })
    };
    if ret != 0 {
        return ret;
    }
    unsafe {
        if (*crtc_state).port_clock == 0 {
            (*crtc_state).port_clock = (*crtc_state)
                .hw
                .adjusted_mode
                .crtc_clock
                .saturating_mul((*crtc_state).pixel_multiplier as i32);
        }
    }
    let ret = unsafe { intel_crtc_compute_config(state, crtc) };
    if ret != 0 {
        return ret;
    }
    unsafe {
        (*crtc_state).dither = (*crtc_state).pipe_bpp == 18 && !(*crtc_state).dither_force_disable;
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_modeset_pipe_config_late(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
) -> i32 {
    if state.is_null() || crtc.is_null() {
        return -EINVAL;
    }
    unsafe {
        for_each_new_connector_in_state(state, |_, connector_state| {
            if (*connector_state).crtc != &mut (*crtc).base {
                return 0;
            }
            let encoder = (*connector_state).best_encoder.cast::<IntelEncoder>();
            if encoder.is_null() {
                return -EINVAL;
            }
            match (*encoder).compute_config_late {
                Some(compute) => compute(
                    encoder,
                    crate::intel_atomic_upstream::intel_atomic_get_new_crtc_state(state, crtc),
                    connector_state,
                ),
                None => 0,
            }
        })
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_crtc_get_pipe_config(state: *mut IntelCrtcState) -> bool {
    if state.is_null() || unsafe { (*state).uapi.crtc.is_null() } {
        return false;
    }
    let crtc = unsafe { (*state).uapi.crtc.cast::<IntelCrtc>() };
    let display =
        unsafe { crate::intel_display_conversion_upstream::__drm_to_display((*crtc).base.dev) };
    if display.is_null() || unsafe { (*display).modeset.funcs.is_null() } {
        return false;
    }
    let get = unsafe { (*(*display).modeset.funcs).get_pipe_config };
    let Some(get) = get else { return false };
    if !unsafe { get(crtc, state) } {
        return false;
    }
    unsafe {
        (*state).hw.active = true;
        intel_crtc_readout_derived_state(state);
    }
    true
}

unsafe fn intel_crtc_readout_derived_state(state: *mut IntelCrtcState) {
    if state.is_null() {
        return;
    }
    unsafe {
        drm_mode_copy(&mut (*state).hw.pipe_mode, &(*state).hw.adjusted_mode);
        if (*state).splitter.enable || (*state).joiner_pipes != 0 {
            (*state).hw.active = false;
            return;
        }
        intel_mode_from_crtc_timings(&mut (*state).hw.adjusted_mode, &(*state).hw.pipe_mode);
        drm_mode_copy(&mut (*state).hw.mode, &(*state).hw.pipe_mode);
        intel_mode_from_crtc_timings(&mut (*state).hw.mode, &(*state).hw.mode);
        (*state).hw.mode.hdisplay = ((*state).pipe_src.x2 - (*state).pipe_src.x1) as u16;
        (*state).hw.mode.vdisplay = ((*state).pipe_src.y2 - (*state).pipe_src.y1) as u16;
        intel_crtc_compute_pixel_rate(state);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_encoder_get_config(
    encoder: *mut IntelEncoder,
    state: *mut IntelCrtcState,
) {
    if encoder.is_null() || state.is_null() {
        return;
    }
    if let Some(get_config) = unsafe { (*encoder).get_config } {
        unsafe { get_config(encoder, state) };
        unsafe { intel_crtc_readout_derived_state(state) };
    }
}

/// Keep the UAPI plane mask synchronized with the driver's logical plane ids.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_set_plane_visible(
    crtc_state: *mut IntelCrtcState,
    plane_state: *mut IntelPlaneState,
    visible: bool,
) {
    if crtc_state.is_null() || plane_state.is_null() {
        return;
    }
    let plane = unsafe { (*plane_state).uapi.plane.cast::<IntelPlane>() };
    if plane.is_null() {
        return;
    }
    unsafe {
        (*plane_state).uapi.visible = visible;
        let index = (*plane).base.index;
        if index >= 32 {
            return;
        }
        let mask = 1u32 << index;
        if visible {
            (*crtc_state).uapi.plane_mask |= mask;
        } else {
            (*crtc_state).uapi.plane_mask &= !mask;
        }
    }
}

/// Reconstruct the Intel active/enabled plane masks from DRM's unique mask.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_plane_fixup_bitmasks(crtc_state: *mut IntelCrtcState) {
    if crtc_state.is_null() || unsafe { (*crtc_state).uapi.crtc.is_null() } {
        return;
    }
    let crtc = unsafe { (*crtc_state).uapi.crtc.cast::<IntelCrtc>() };
    let dev = unsafe { (*crtc).base.dev };
    if dev.is_null() {
        return;
    }
    let drm_mask = unsafe { (*crtc_state).uapi.plane_mask };
    let count = unsafe { (*dev).mode_config.num_total_plane.max(0) as usize };
    if unsafe { (*dev).mode_config.plane_list.next.is_null() } {
        return;
    }
    let head = unsafe { ptr::addr_of_mut!((*dev).mode_config.plane_list) };
    let mut node = unsafe { (*head).next };
    let plane_head_offset = offset_of!(IntelPlane, base) + offset_of!(DrmPlane, head);
    let mut enabled = 0u8;
    while !node.is_null() && node != head {
        let plane = unsafe {
            node.cast::<u8>()
                .sub(plane_head_offset)
                .cast::<IntelPlane>()
        };
        let index = unsafe { (*plane).base.index };
        let id = unsafe { (*plane).id };
        if index < 32 && id < 8 && drm_mask & (1u32 << index) != 0 {
            enabled |= 1u8 << id;
        }
        node = unsafe { (*node).next };
    }
    let _ = count;
    unsafe {
        (*crtc_state).enabled_planes = enabled;
        (*crtc_state).active_planes = enabled;
    }
}

/// Disable a plane outside an atomic transaction while preserving watermark,
/// underrun, and vblank ordering. This is used by the initial-plane recovery
/// path and is also required by the B4d initial-plane implementation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_plane_disable_noatomic(
    crtc: *mut IntelCrtc,
    plane: *mut IntelPlane,
) {
    if crtc.is_null() || plane.is_null() {
        return;
    }
    let display =
        unsafe { crate::intel_display_conversion_upstream::__drm_to_display((*crtc).base.dev) };
    let crtc_state = unsafe { (*crtc).base.state.cast::<IntelCrtcState>() };
    let plane_state = unsafe { (*plane).base.state.cast::<IntelPlaneState>() };
    if display.is_null() || crtc_state.is_null() || plane_state.is_null() {
        return;
    }
    unsafe {
        crate::intel_plane_upstream::intel_plane_set_invisible(crtc_state, plane_state);
        intel_set_plane_visible(crtc_state, plane_state, false);
        intel_plane_fixup_bitmasks(crtc_state);
        crate::display_pending_stubs::b4::skl_wm_plane_disable_noatomic(crtc, plane);

        // IPS only exists on the Haswell platform. It is not an ADL-P feature;
        // retain the source ordering for that platform without introducing a
        // fake IPS implementation on display IP 13.
        if platform_has(display, 21)
            && ((*crtc_state).active_planes
                & !(1u8 << crate::intel_display_limits_types_upstream::PLANE_CURSOR))
                == 0
            && hsw_ips_disable(crtc_state)
        {
            (*crtc_state).ips_enabled = false;
            intel_initial_plane_vblank_wait(crtc);
        }

        // ADL-P has no GMCH memory-self-refresh path, but preserve the Linux
        // sequencing for platforms where this CRTC entry is reused.
        if display_ver(display) < DISPLAY_VER_SCALED_PLANES
            && platform_has(display, 7)
            && intel_set_memory_cxsr(display, false)
        {
            intel_initial_plane_vblank_wait(crtc);
        }
        if display_ver(display) == 2 && (*crtc_state).active_planes == 0 {
            crate::intel_fifo_underrun_upstream::intel_set_cpu_fifo_underrun_reporting(
                display,
                (*crtc).pipe,
                false,
            );
        }
        crate::intel_plane_upstream::intel_plane_disable_arm(ptr::null_mut(), plane, crtc_state);
        intel_initial_plane_vblank_wait(crtc);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_crtc_needs_fastset(state: *const IntelCrtcState) -> bool {
    !state.is_null() && unsafe { (*state).update_pipe }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_crtc_needs_color_update(state: *const IntelCrtcState) -> bool {
    !state.is_null()
        && unsafe {
            (*state).uapi.color_mgmt_changed()
                || (*state).update_pipe
                || crate::intel_atomic_upstream::intel_crtc_needs_modeset(state)
        }
}

fn run_watermark_scaler_check<W, S, A>(
    mut watermarks: W,
    needs_pipe_scaler: bool,
    mut stage_pipe_scaler: S,
    mut allocate_scalers: A,
) -> i32
where
    W: FnMut() -> i32,
    S: FnMut() -> i32,
    A: FnMut() -> i32,
{
    let ret = watermarks();
    if ret != 0 {
        return ret;
    }
    if needs_pipe_scaler {
        let ret = stage_pipe_scaler();
        if ret != 0 {
            return ret;
        }
    }
    allocate_scalers()
}

unsafe fn intel_hsw_compute_linetime_wm(state: *mut IntelCrtcState) {
    if state.is_null() {
        return;
    }
    if unsafe { !(*state).hw.enable } {
        unsafe { (*state).linetime = 0 };
        return;
    }
    let htotal = i64::from(unsafe { (*state).hw.pipe_mode.crtc_htotal });
    let rate = i64::from(unsafe { (*state).pixel_rate });
    if rate <= 0 {
        unsafe { (*state).linetime = 0x1ff };
        return;
    }
    let numerator = htotal.saturating_mul(8_000);
    let rounded = (numerator + rate - 1) / rate;
    unsafe { (*state).linetime = rounded.clamp(0, 0x1ff) as u16 };
}

/// Per-CRTC part of Linux `intel_atomic_check()`. Plane checks have already
/// staged plane scaler users; this preserves watermark -> pipe-fitter staging
/// -> finite-scaler allocation ordering.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_crtc_atomic_check(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
) -> i32 {
    if state.is_null() || crtc.is_null() {
        return -EINVAL;
    }
    let display =
        unsafe { crate::intel_display_conversion_upstream::__drm_to_display((*crtc).base.dev) };
    if display.is_null() {
        return -EINVAL;
    }
    let crtc_state =
        unsafe { crate::intel_atomic_upstream::intel_atomic_get_new_crtc_state(state, crtc) };
    if crtc_state.is_null() || unsafe { IS_ERR(crtc_state) } {
        return if unsafe { IS_ERR(crtc_state) } {
            unsafe { PTR_ERR(crtc_state.cast::<c_void>()) }
        } else {
            -EINVAL
        };
    }

    if unsafe { crate::intel_atomic_upstream::intel_crtc_needs_modeset(crtc_state) } {
        let ret = unsafe { intel_dpll_crtc_get_dpll(state, crtc) };
        if ret != 0 {
            return ret;
        }
    }
    let ret = unsafe { crate::intel_color_upstream::intel_color_check(state, crtc) };
    if ret != 0 {
        return ret;
    }

    let ver = unsafe { display_ver(display) };
    if ver < DISPLAY_VER_SCALED_PLANES {
        // This translation owns the ADL-P display-IP 13 path. Legacy
        // watermark/IPS configurations are deliberately not emulated here.
        return -EOPNOTSUPP;
    }
    let needs_pipe_scaler = unsafe {
        crate::intel_atomic_upstream::intel_crtc_needs_modeset(crtc_state)
            || intel_crtc_needs_fastset(crtc_state)
    };
    let ret = run_watermark_scaler_check(
        || unsafe { crate::intel_wm_upstream::intel_wm_compute(state, crtc) },
        needs_pipe_scaler,
        || unsafe { skl_update_scaler_crtc(crtc_state) },
        || unsafe { intel_atomic_setup_scalers(state, crtc) },
    );
    if ret != 0 {
        return ret;
    }
    unsafe { intel_hsw_compute_linetime_wm(crtc_state) };
    0
}

unsafe fn intel_atomic_check_crtcs(state: *mut IntelAtomicState) -> i32 {
    if state.is_null() || unsafe { (*state).base.dev.is_null() || (*state).base.crtcs.is_null() } {
        return -EINVAL;
    }
    let count = unsafe { (*(*state).base.dev).mode_config.num_crtc.max(0) as usize };
    for index in 0..count {
        let entry = unsafe { (*state).base.crtcs.add(index) };
        if unsafe { (*entry).new_state.is_null() || (*entry).ptr.is_null() } {
            continue;
        }
        let ret = unsafe { intel_crtc_atomic_check(state, (*entry).ptr.cast()) };
        if ret != 0 {
            return ret;
        }
    }
    0
}

unsafe fn intel_crtc_copy_uapi_to_hw_state_nomodeset(state: *mut IntelCrtcState) -> i32 {
    if state.is_null() {
        return -EINVAL;
    }
    unsafe {
        let _ = drm_property_replace_blob(&mut (*state).hw.degamma_lut, (*state).uapi.degamma_lut);
        let _ = drm_property_replace_blob(&mut (*state).hw.gamma_lut, (*state).uapi.gamma_lut);
        let _ = drm_property_replace_blob(&mut (*state).hw.ctm, (*state).uapi.ctm);
        (*state).hw.background_color =
            crate::intel_color_upstream::intel_color_background_color_drm_to_hw(
                (*state).uapi.background_color,
            );
    }
    0
}

unsafe fn intel_crtc_copy_uapi_to_hw_state_modeset(state: *mut IntelCrtcState) -> i32 {
    if state.is_null() {
        return -EINVAL;
    }
    unsafe {
        if (*state).joiner_pipes != 0 || (*state).splitter.enable {
            return -EOPNOTSUPP;
        }
        (*state).hw.enable = (*state).uapi.enable;
        (*state).hw.active = (*state).uapi.active;
        drm_mode_copy(&mut (*state).hw.mode, &(*state).uapi.mode);
        drm_mode_copy(&mut (*state).hw.adjusted_mode, &(*state).uapi.adjusted_mode);
        (*state).hw.scaling_filter = (*state).uapi.scaling_filter;
        (*state).hw.sharpness_strength = (*state).uapi.sharpness_strength;
    }
    unsafe { intel_crtc_copy_uapi_to_hw_state_nomodeset(state) }
}

/// Reset derived hardware configuration while retaining the UAPI and the
/// refcounted state which DRM duplicates across an atomic transaction.
unsafe fn intel_crtc_prepare_cleared_state(state: *mut IntelCrtcState) -> i32 {
    if state.is_null() {
        return -EINVAL;
    }
    unsafe {
        // DP tunnel stream state is an optional subsystem in this port. Do
        // not clear/duplicate a live reference without its lifetime helpers.
        if !(*state).dp_tunnel_ref.tunnel.is_null() {
            return -EOPNOTSUPP;
        }
    }
    let saved = unsafe { kzalloc_objs::<IntelCrtcState, _>(1) };
    if saved.is_null() {
        return -ENOMEM;
    }
    unsafe {
        ptr::copy_nonoverlapping(
            ptr::addr_of!((*state).uapi),
            ptr::addr_of_mut!((*saved).uapi),
            1,
        );
        (*saved).inherited = (*state).inherited;
        ptr::copy_nonoverlapping(
            ptr::addr_of!((*state).scaler_state),
            ptr::addr_of_mut!((*saved).scaler_state),
            1,
        );
        (*saved).intel_dpll = (*state).intel_dpll;
        ptr::copy_nonoverlapping(
            ptr::addr_of!((*state).dpll_hw_state),
            ptr::addr_of_mut!((*saved).dpll_hw_state),
            1,
        );
        ptr::copy_nonoverlapping(
            ptr::addr_of!((*state).icl_port_dplls),
            ptr::addr_of_mut!((*saved).icl_port_dplls),
            1,
        );
        (*saved).crc_enabled = (*state).crc_enabled;

        crate::intel_atomic_upstream::intel_crtc_free_hw_state(state);
        ptr::write_bytes(state, 0, 1);
        ptr::copy_nonoverlapping(
            ptr::addr_of!((*saved).uapi),
            ptr::addr_of_mut!((*state).uapi),
            1,
        );
        (*state).inherited = (*saved).inherited;
        ptr::copy_nonoverlapping(
            ptr::addr_of!((*saved).scaler_state),
            ptr::addr_of_mut!((*state).scaler_state),
            1,
        );
        (*state).intel_dpll = (*saved).intel_dpll;
        ptr::copy_nonoverlapping(
            ptr::addr_of!((*saved).dpll_hw_state),
            ptr::addr_of_mut!((*state).dpll_hw_state),
            1,
        );
        ptr::copy_nonoverlapping(
            ptr::addr_of!((*saved).icl_port_dplls),
            ptr::addr_of_mut!((*state).icl_port_dplls),
            1,
        );
        (*state).crc_enabled = (*saved).crc_enabled;
        kfree(saved.cast::<c_void>());
    }
    unsafe { intel_crtc_copy_uapi_to_hw_state_modeset(state) }
}

unsafe fn intel_atomic_check_config(
    state: *mut IntelAtomicState,
    limits: *const IntelLinkBwLimits,
    failed_pipe: *mut i32,
) -> i32 {
    if state.is_null() || limits.is_null() || failed_pipe.is_null() {
        return -EINVAL;
    }
    unsafe { *failed_pipe = -1 };
    let dev = unsafe { (*state).base.dev };
    if dev.is_null() || unsafe { (*state).base.crtcs.is_null() } {
        return -EINVAL;
    }
    let count = unsafe { (*dev).mode_config.num_crtc.max(0) as usize };
    for index in 0..count {
        let entry = unsafe { (*state).base.crtcs.add(index) };
        if unsafe { (*entry).ptr.is_null() || (*entry).new_state.is_null() } {
            continue;
        }
        let crtc = unsafe { (*entry).ptr.cast::<IntelCrtc>() };
        let new = unsafe { (*entry).new_state.cast::<IntelCrtcState>() };
        if unsafe { (*new).joiner_pipes != 0 || (*new).splitter.enable } {
            unsafe { *failed_pipe = (*crtc).pipe };
            return -EOPNOTSUPP;
        }
        if !unsafe { crate::intel_atomic_upstream::intel_crtc_needs_modeset(new) } {
            let ret = unsafe { intel_crtc_copy_uapi_to_hw_state_nomodeset(new) };
            if ret != 0 {
                unsafe { *failed_pipe = (*crtc).pipe };
                return ret;
            }
            continue;
        }
        let ret = unsafe { intel_crtc_prepare_cleared_state(new) };
        if ret != 0 {
            unsafe { *failed_pipe = (*crtc).pipe };
            return ret;
        }
        if !unsafe { (*new).hw.enable } {
            continue;
        }
        let ret = unsafe { intel_modeset_pipe_config(state, crtc, limits) };
        if ret != 0 {
            unsafe { *failed_pipe = (*crtc).pipe };
            return ret;
        }
    }
    for index in 0..count {
        let entry = unsafe { (*state).base.crtcs.add(index) };
        if unsafe { (*entry).ptr.is_null() || (*entry).new_state.is_null() } {
            continue;
        }
        let crtc = unsafe { (*entry).ptr.cast::<IntelCrtc>() };
        let new = unsafe { (*entry).new_state.cast::<IntelCrtcState>() };
        if !unsafe { crate::intel_atomic_upstream::intel_crtc_needs_modeset(new) }
            || !unsafe { (*new).hw.enable }
        {
            continue;
        }
        let ret = unsafe { intel_modeset_pipe_config_late(state, crtc) };
        if ret != 0 {
            unsafe { *failed_pipe = (*crtc).pipe };
            return ret;
        }
    }
    0
}

unsafe fn intel_atomic_check_config_and_link(state: *mut IntelAtomicState) -> i32 {
    if state.is_null() {
        return -EINVAL;
    }
    if unsafe { (*state).base.dev.is_null() } {
        return -EINVAL;
    }
    let mut new_limits =
        unsafe { core::mem::MaybeUninit::<IntelLinkBwLimits>::zeroed().assume_init() };
    unsafe { intel_link_bw_init_limits(state, &mut new_limits) };
    let mut old_limits =
        unsafe { core::mem::MaybeUninit::<IntelLinkBwLimits>::zeroed().assume_init() };
    unsafe { ptr::copy_nonoverlapping(&new_limits, &mut old_limits, 1) };
    let max_retries = unsafe { (*(*state).base.dev).mode_config.num_crtc.max(0) as usize + 1 };
    for _ in 0..max_retries {
        let mut failed_pipe = -1;
        let ret = unsafe { intel_atomic_check_config(state, &new_limits, &mut failed_pipe) };
        if ret != 0 {
            if ret == -EINVAL
                && failed_pipe >= 0
                && unsafe {
                    intel_link_bw_set_bpp_limit_for_pipe(
                        state,
                        &old_limits,
                        &mut new_limits,
                        failed_pipe,
                    )
                }
            {
                continue;
            }
            return ret;
        }
        unsafe { ptr::copy_nonoverlapping(&new_limits, &mut old_limits, 1) };
        let ret = unsafe { intel_link_bw_atomic_check(state, &mut new_limits) };
        if ret != -tk_linux_kapi::config::EAGAIN {
            return ret;
        }
    }
    -EINVAL
}

unsafe fn connector_state_for_check(
    state: *mut IntelAtomicState,
    connector: *mut DrmConnector,
) -> *mut DrmConnectorState {
    if state.is_null() || connector.is_null() {
        return ptr::null_mut();
    }
    let count = unsafe { (*state).base.num_connector.max(0) as usize };
    for index in 0..count {
        let entry = unsafe { (*state).base.connectors.add(index) };
        if unsafe { (*entry).ptr == connector } {
            return unsafe { (*entry).new_state };
        }
    }
    unsafe { (*connector).state }
}

/// Reject simultaneous SST/MST use of one physical DDI port. MST itself is
/// not implemented by this ADL-P subset, but existing unchanged connectors
/// still participate in conflict detection during atomic checks.
unsafe fn check_digital_port_conflicts(state: *mut IntelAtomicState) -> bool {
    if state.is_null() || unsafe { (*state).base.dev.is_null() } {
        return false;
    }
    let dev = unsafe { (*state).base.dev };
    let head = unsafe { ptr::addr_of_mut!((*dev).mode_config.connector_list) };
    let mut node = unsafe { (*head).next };
    if node.is_null() {
        return false;
    }
    let mut used_ports = 0u32;
    let mut used_mst_ports = 0u32;
    let mut valid = true;
    let connector_head_offset = offset_of!(DrmConnector, head);
    while node != head {
        let connector = unsafe {
            node.cast::<u8>()
                .sub(connector_head_offset)
                .cast::<DrmConnector>()
        };
        let connector_state = unsafe { connector_state_for_check(state, connector) };
        if !connector_state.is_null() {
            let encoder = unsafe { (*connector_state).best_encoder.cast::<IntelEncoder>() };
            if !encoder.is_null() {
                let output = unsafe { (*encoder).r#type };
                let port = unsafe { (*encoder).port };
                if (0..32).contains(&port) {
                    let bit = 1u32 << port;
                    match output {
                        crate::intel_display_types_upstream::INTEL_OUTPUT_DDI
                        | crate::intel_display_types_upstream::INTEL_OUTPUT_DP
                        | crate::intel_display_types_upstream::INTEL_OUTPUT_HDMI
                        | crate::intel_display_types_upstream::INTEL_OUTPUT_EDP => {
                            if output == crate::intel_display_types_upstream::INTEL_OUTPUT_DDI
                                && !unsafe {
                                    has_ddi(
                                        crate::intel_display_conversion_upstream::__drm_to_display(
                                            dev,
                                        ),
                                    )
                                }
                            {
                                valid = false;
                            }
                            if used_ports & bit != 0 {
                                valid = false;
                            }
                            used_ports |= bit;
                        }
                        crate::intel_display_types_upstream::INTEL_OUTPUT_DP_MST => {
                            used_mst_ports |= bit;
                        }
                        _ => {}
                    }
                }
            }
        }
        node = unsafe { (*node).next };
        if node.is_null() {
            return false;
        }
    }
    valid && (used_ports & used_mst_ports == 0)
}

/// Validate and prepare an Intel atomic state. Optional MST, DP-tunnel,
/// joiner/splitter, VRR and async-flip paths fail closed until their complete
/// subsystem translations are connected.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_check(
    dev: *mut DrmDevice,
    atomic: *mut DrmAtomicCommit,
) -> i32 {
    if dev.is_null() || atomic.is_null() || unsafe { (*atomic).dev != dev } {
        return -EINVAL;
    }
    let display = unsafe { crate::intel_display_conversion_upstream::__drm_to_display(dev) };
    if !unsafe { crate::intel_display_driver_upstream::intel_display_driver_check_access(display) }
    {
        return -ENODEV;
    }
    let state = atomic.cast::<IntelAtomicState>();
    if unsafe { (*state).base.crtcs.is_null() } {
        return -EINVAL;
    }

    let crtc_count = unsafe { (*dev).mode_config.num_crtc.max(0) as usize };
    let mut any_modeset = false;
    for index in 0..crtc_count {
        let entry = unsafe { (*state).base.crtcs.add(index) };
        if unsafe { (*entry).new_state.is_null() || (*entry).old_state.is_null() } {
            continue;
        }
        let new = unsafe { (*entry).new_state.cast::<IntelCrtcState>() };
        let old = unsafe { (*entry).old_state.cast::<IntelCrtcState>() };
        if unsafe { (*new).joiner_pipes != 0 || (*new).splitter.enable } {
            return -EOPNOTSUPP;
        }
        if unsafe { (*new).dp_tunnel_ref.tunnel.is_null() == false }
            || unsafe { (*new).uapi.vrr_enabled }
        {
            return -EOPNOTSUPP;
        }
        if unsafe { !(*state).internal } {
            unsafe { (*new).inherited = false };
        }
        if unsafe {
            (*new).inherited != (*old).inherited
                || (*new).uapi.scaling_filter != (*old).uapi.scaling_filter
                || (*new).uapi.sharpness_strength != (*old).uapi.sharpness_strength
        } {
            unsafe { (*new).uapi.set_mode_changed(true) };
        }
    }

    let ret = unsafe { drm_atomic_helper_check_modeset(dev, atomic) };
    if ret != 0 {
        return ret;
    }
    for index in 0..crtc_count {
        let entry = unsafe { (*state).base.crtcs.add(index) };
        if unsafe { (*entry).ptr.is_null() || (*entry).new_state.is_null() } {
            continue;
        }
        let new = unsafe { (*entry).new_state.cast::<IntelCrtcState>() };
        if unsafe { (*new).uapi.async_flip || (*new).do_async_flip } {
            return -EOPNOTSUPP;
        }
    }

    let ret = unsafe { intel_atomic_check_config_and_link(state) };
    if ret != 0 {
        return ret;
    }

    for index in 0..crtc_count {
        let entry = unsafe { (*state).base.crtcs.add(index) };
        if unsafe { (*entry).ptr.is_null() || (*entry).new_state.is_null() } {
            continue;
        }
        let crtc = unsafe { (*entry).ptr.cast::<IntelCrtc>() };
        let new = unsafe { (*entry).new_state.cast::<IntelCrtcState>() };
        if unsafe { crate::intel_atomic_upstream::intel_crtc_needs_modeset(new) } {
            any_modeset = true;
            unsafe { intel_dpll_release(state, crtc) };
        }
    }
    if any_modeset && !unsafe { check_digital_port_conflicts(state) } {
        return -EINVAL;
    }

    let ret = unsafe { crate::intel_plane_upstream::intel_plane_atomic_check(state) };
    if ret != 0 {
        return ret;
    }
    for index in 0..crtc_count {
        let entry = unsafe { (*state).base.crtcs.add(index) };
        if unsafe { (*entry).new_state.is_null() } {
            continue;
        }
        let new = unsafe { (*entry).new_state.cast::<IntelCrtcState>() };
        unsafe { (*new).min_cdclk = crate::intel_cdclk_upstream::intel_crtc_min_cdclk(new) };
    }
    let ret = unsafe { crate::intel_wm_upstream::intel_compute_global_watermarks(state) };
    if ret != 0 {
        return ret;
    }
    let ret = unsafe { crate::intel_bw_upstream::intel_bw_atomic_check(state) };
    if ret != 0 {
        return ret;
    }
    let ret = unsafe { crate::intel_cdclk_upstream::intel_cdclk_atomic_check(state) };
    if ret != 0 {
        return ret;
    }
    if any_modeset {
        unsafe { (*state).modeset = true };
    }
    let ret = unsafe { crate::intel_pmdemand_upstream::intel_pmdemand_atomic_check(state.cast()) };
    if ret != 0 {
        return ret;
    }
    let ret = unsafe { intel_atomic_check_crtcs(state) };
    if ret != 0 {
        return ret;
    }
    for index in 0..crtc_count {
        let entry = unsafe { (*state).base.crtcs.add(index) };
        if unsafe { (*entry).new_state.is_null() } {
            continue;
        }
        let new = unsafe { (*entry).new_state.cast::<IntelCrtcState>() };
        unsafe { crate::intel_color_upstream::intel_color_assert_luts(new) };
        if unsafe {
            crate::intel_atomic_upstream::intel_crtc_needs_modeset(new)
                && intel_crtc_needs_fastset(new)
        } {
            return -EINVAL;
        }
        if unsafe { (*new).dp_tunnel_ref.tunnel.is_null() == false } {
            return -EOPNOTSUPP;
        }
    }
    0
}

#[inline]
unsafe fn encoders_cloneable(a: *const IntelEncoder, b: *const IntelEncoder) -> bool {
    !a.is_null()
        && !b.is_null()
        && (a == b
            || (unsafe { (*a).cloneable & (1u16 << (*b).r#type) != 0 }
                && unsafe { (*b).cloneable & (1u16 << (*a).r#type) != 0 }))
}

#[inline]
unsafe fn encoder_from_list(node: *mut ListHead) -> *mut IntelEncoder {
    unsafe {
        node.cast::<u8>()
            .sub(offset_of!(DrmEncoder, head))
            .cast::<DrmEncoder>()
            .cast::<IntelEncoder>()
    }
}

unsafe fn for_each_encoder(display: *mut IntelDisplay, mut f: impl FnMut(*mut IntelEncoder)) {
    if display.is_null() || unsafe { (*display).drm.is_null() } {
        return;
    }
    let head = unsafe { ptr::addr_of_mut!((*(*display).drm).mode_config.encoder_list) };
    let mut node = unsafe { (*head).next };
    while node != head {
        let encoder = unsafe { encoder_from_list(node) };
        f(encoder);
        node = unsafe { (*node).next };
    }
}

unsafe fn intel_encoder_possible_clones(encoder: *mut IntelEncoder) -> u32 {
    if encoder.is_null() {
        return 0;
    }
    let display =
        unsafe { crate::intel_display_conversion_upstream::__drm_to_display((*encoder).base.dev) };
    let mut possible = 0u32;
    unsafe {
        for_each_encoder(display, |other| {
            if encoders_cloneable(encoder, other) {
                let index = (*other).base.index;
                if index < 32 {
                    possible |= 1u32 << index;
                }
            }
        });
    }
    possible
}

unsafe fn intel_encoder_possible_crtcs(encoder: *mut IntelEncoder) -> u32 {
    if encoder.is_null() {
        return 0;
    }
    let display =
        unsafe { crate::intel_display_conversion_upstream::__drm_to_display((*encoder).base.dev) };
    if display.is_null() {
        return 0;
    }
    let head = unsafe { ptr::addr_of_mut!((*display).pipe_list) };
    let mut node = unsafe { (*head).next };
    let mut possible = 0u32;
    while node != head {
        let crtc = unsafe {
            node.cast::<u8>()
                .sub(offset_of!(IntelCrtc, pipe_head))
                .cast::<IntelCrtc>()
        };
        let index = unsafe { (*crtc).base.index };
        let pipe = unsafe { (*crtc).pipe };
        if (0..8).contains(&pipe)
            && index < 32
            && unsafe { (*encoder).pipe_mask & (1u8 << pipe) != 0 }
        {
            possible |= 1u32 << index;
        }
        node = unsafe { (*node).next };
    }
    possible
}

/// Initialize the ADL-P display modeset vtable. The initial-state readout and
/// per-generation CRTC hooks are supplied by the adjacent display translation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_init_display_hooks(display: *mut IntelDisplay) {
    if display.is_null() || unsafe { display_ver(display) < DISPLAY_VER_SCALED_PLANES } {
        return;
    }
    unsafe {
        (*display).modeset.funcs = ptr::addr_of!(SKL_DISPLAY_FUNCS).cast_mut();
    }
}

static SKL_DISPLAY_FUNCS: IntelModesetFuncs = IntelModesetFuncs {
    get_pipe_config: Some(intel_hsw_get_pipe_config),
    get_initial_plane_config: Some(skl_get_initial_plane_config),
    fixup_initial_plane_config: Some(skl_fixup_initial_plane_config),
    crtc_enable: Some(intel_hsw_crtc_enable),
    crtc_disable: Some(intel_hsw_crtc_disable),
    commit_modeset_enables: Some(intel_skl_commit_modeset_enables),
};

unsafe extern "C" {
    fn intel_hsw_get_pipe_config(crtc: *mut IntelCrtc, state: *mut IntelCrtcState) -> bool;
    fn skl_get_initial_plane_config(
        crtc: *mut IntelCrtc,
        config: *mut crate::intel_display_types_upstream::IntelInitialPlaneConfig,
    );
    fn skl_fixup_initial_plane_config(
        crtc: *mut IntelCrtc,
        config: *mut crate::intel_display_types_upstream::IntelInitialPlaneConfig,
    ) -> bool;
    fn intel_hsw_crtc_enable(state: *mut IntelAtomicState, crtc: *mut IntelCrtc);
    fn intel_hsw_crtc_disable(state: *mut IntelAtomicState, crtc: *mut IntelCrtc);
    fn intel_skl_commit_modeset_enables(state: *mut IntelAtomicState);
}

/// Probe outputs through the DDI/VBT path used by ADL-P, then publish DRM
/// encoder masks in the same order as Linux.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_setup_outputs(display: *mut IntelDisplay) {
    if !unsafe { intel_display_device_present(display) } {
        return;
    }
    unsafe { intel_pps_unlock_regs_wa(display) };
    if unsafe { display_ver(display) } < DISPLAY_VER_SCALED_PLANES || !unsafe { has_ddi(display) } {
        return;
    }
    unsafe { intel_bios_for_each_encoder(display, intel_ddi_init) };
    unsafe {
        for_each_encoder(display, |encoder| {
            (*encoder).base.possible_crtcs = intel_encoder_possible_crtcs(encoder);
            (*encoder).base.possible_clones = intel_encoder_possible_clones(encoder);
        });
        intel_init_pch_refclk(display);
        tk_drm_upstream::drm_modeset_helper_upstream::drm_helper_move_panel_connectors_to_head(
            (*display).drm,
        );
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, vec::Vec};

    use tk_linux_kapi::config::{EINVAL, EOPNOTSUPP};

    use super::run_watermark_scaler_check;

    #[test]
    fn watermark_scaler_order_preserves_error_short_circuiting() {
        let calls = RefCell::new(Vec::new());
        let ret = run_watermark_scaler_check(
            || {
                calls.borrow_mut().push("wm");
                0
            },
            true,
            || {
                calls.borrow_mut().push("pipe-scaler");
                0
            },
            || {
                calls.borrow_mut().push("scaler-allocation");
                0
            },
        );
        assert_eq!(ret, 0);
        assert_eq!(
            calls.borrow().as_slice(),
            ["wm", "pipe-scaler", "scaler-allocation"]
        );

        calls.borrow_mut().clear();
        let ret = run_watermark_scaler_check(
            || {
                calls.borrow_mut().push("wm");
                -EINVAL
            },
            true,
            || {
                calls.borrow_mut().push("pipe-scaler");
                0
            },
            || {
                calls.borrow_mut().push("scaler-allocation");
                0
            },
        );
        assert_eq!(ret, -EINVAL);
        assert_eq!(calls.borrow().as_slice(), ["wm"]);

        calls.borrow_mut().clear();
        let ret = run_watermark_scaler_check(
            || {
                calls.borrow_mut().push("wm");
                0
            },
            true,
            || {
                calls.borrow_mut().push("pipe-scaler");
                -EOPNOTSUPP
            },
            || {
                calls.borrow_mut().push("scaler-allocation");
                0
            },
        );
        assert_eq!(ret, -EOPNOTSUPP);
        assert_eq!(calls.borrow().as_slice(), ["wm", "pipe-scaler"]);
    }
}
