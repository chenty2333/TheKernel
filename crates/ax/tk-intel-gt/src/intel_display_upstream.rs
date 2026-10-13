// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// ADL-P modesetting and atomic-commit path translated from Linux
// drivers/gpu/drm/i915/display/intel_display.c (Linux v7.2.3).
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::{
    ffi::{c_char, c_void},
    mem::offset_of,
    ptr,
};

use tk_drm_upstream::{
    drm_atomic_types::DrmAtomicCommit,
    drm_connector_types::{DrmConnector, DrmConnectorState},
    drm_crtc_types::DrmCrtc,
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
        DRM_MODE_FLAG_INTERLACE, DRM_MODE_FLAG_NCSYNC, DRM_MODE_FLAG_NHSYNC, DRM_MODE_FLAG_NVSYNC,
        DRM_MODE_FLAG_PCSYNC, DRM_MODE_FLAG_PHSYNC, DRM_MODE_FLAG_PIXMUX, DRM_MODE_FLAG_PVSYNC,
    },
};
use tk_linux_kapi::{
    config::{EINVAL, ENODEV, ENOMEM, EOPNOTSUPP, IS_ERR, PTR_ERR},
    memory::{kfree, kzalloc_objs},
    types::ListHead,
    workqueue::{flush_workqueue, queue_work},
};

use crate::{
    intel_bios_upstream::IntelBiosEncoderData,
    intel_display_core_types_upstream::{IntelDisplay, IntelModesetFuncs},
    intel_display_device_upstream::intel_display_device_present,
    intel_display_limits_types_upstream::{I915_MAX_PIPES, Pipe},
    intel_display_types_upstream::{
        INTEL_OUTPUT_DDI, INTEL_OUTPUT_DP, INTEL_OUTPUT_DP_MST, INTEL_OUTPUT_EDP,
        INTEL_OUTPUT_HDMI, IntelAtomicState, IntelCrtc, IntelCrtcState, IntelDigitalPort,
        IntelEncoder, IntelPlane, IntelPlaneState,
    },
    intel_hti_upstream::{PHY_A, PHY_B, PHY_D, PHY_F, PHY_I, PHY_NONE, Phy},
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

/// Map ADL-P DDI port numbering to the PHY enumeration used by DPLL/TC code.
/// PORT_TC1..TC4 are wired to PHY_F..I; the ordinary A/B ports use PHY_A/B.
fn adlp_port_to_phy(port: i32) -> Phy {
    match port {
        0..=2 => PHY_A + port,
        3..=6 => PHY_F + port - 3,
        7..=8 => PHY_D + port - 7,
        _ => PHY_NONE,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_port_to_phy(display: *mut IntelDisplay, port: i32) -> Phy {
    if display.is_null() {
        return PHY_NONE;
    }
    if unsafe { display_ver(display) } >= DISPLAY_VER_ADLP {
        return adlp_port_to_phy(port);
    }
    // This translation target deliberately only initializes ADL-P. Do not
    // guess older-generation PHY topologies from their port numbering.
    PHY_NONE
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_encoder_to_phy(encoder: *mut IntelEncoder) -> Phy {
    if encoder.is_null() {
        return PHY_NONE;
    }
    let display =
        unsafe { crate::intel_display_conversion_upstream::__drm_to_display((*encoder).base.dev) };
    if display.is_null() {
        return PHY_NONE;
    }
    unsafe { intel_port_to_phy(display, (*encoder).port) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_encoder_is_combo(encoder: *mut IntelEncoder) -> bool {
    let phy = unsafe { intel_encoder_to_phy(encoder) };
    // ADL-P has combo PHYs A/B; the remaining populated outputs are Type-C PHYs.
    (PHY_A..=PHY_B).contains(&phy)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_encoder_is_tc(encoder: *mut IntelEncoder) -> bool {
    if encoder.is_null() {
        return false;
    }
    let output_type = unsafe { (*encoder).r#type };
    if !matches!(
        output_type,
        INTEL_OUTPUT_HDMI
            | INTEL_OUTPUT_DP
            | INTEL_OUTPUT_EDP
            | INTEL_OUTPUT_DDI
            | INTEL_OUTPUT_DP_MST
    ) {
        return false;
    }
    let phy = unsafe { intel_encoder_to_phy(encoder) };
    if !(PHY_F..=PHY_I).contains(&phy) {
        return false;
    }
    // Like upstream, a dedicated-external DDI is not a Type-C connection even
    // if its logical port number maps to a Type-C PHY.
    let dig_port = unsafe { encoder.cast::<IntelDigitalPort>() };
    unsafe { !(*dig_port).dedicated_external }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_encoder_to_tc(encoder: *mut IntelEncoder) -> i32 {
    if encoder.is_null() {
        return -1; // TC_PORT_NONE
    }
    match unsafe { intel_encoder_to_phy(encoder) } {
        phy @ PHY_F..=PHY_I => phy - PHY_F,
        _ => -1,
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

fn mode_valid_limits(mode: &DrmDisplayMode, ver: u16, max_clock: i32) -> DrmModeStatus {
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

fn cpu_transcoder_mode_valid_limits(
    mode: &DrmDisplayMode,
    ver: u16,
    has_g4x: bool,
) -> DrmModeStatus {
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
    if (ver >= 5 || has_g4x) && mode.hsync_start == mode.hdisplay {
        return MODE_H_ILLEGAL;
    }
    MODE_OK
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
    unsafe { mode_valid_limits(&*mode, display_ver(display), max_dotclock(display)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_cpu_transcoder_mode_valid(
    display: *mut IntelDisplay,
    mode: *const DrmDisplayMode,
) -> DrmModeStatus {
    if display.is_null() || mode.is_null() {
        return MODE_BAD;
    }
    unsafe {
        cpu_transcoder_mode_valid_limits(&*mode, display_ver(display), platform_has(display, 0))
    }
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
    let ret = unsafe { crate::intel_dpll_upstream::intel_dpll_crtc_compute_clock(state, crtc) };
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
        let ret = unsafe { crate::intel_dpll_upstream::intel_dpll_crtc_get_dpll(state, crtc) };
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
        || unsafe { crate::skl_scaler_upstream::skl_update_scaler_crtc(crtc_state) },
        || unsafe { crate::skl_scaler_upstream::intel_atomic_setup_scalers(state, crtc) },
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
            if retry_failed_config_with_bpp_limit(ret, failed_pipe, || unsafe {
                intel_link_bw_set_bpp_limit_for_pipe(
                    state,
                    &old_limits,
                    &mut new_limits,
                    failed_pipe,
                )
            }) {
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

fn retry_failed_config_with_bpp_limit(
    error: i32,
    failed_pipe: i32,
    set_limit: impl FnOnce() -> bool,
) -> bool {
    error == -EINVAL && failed_pipe >= 0 && set_limit()
}

fn cleanup_after_commit_stage_error(error: i32, cleanup: impl FnOnce()) -> i32 {
    if error != 0 {
        cleanup();
    }
    error
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
    let ret = unsafe {
        for_each_new_connector_in_state(state, |_, connector_state| {
            let encoder = (*connector_state).best_encoder.cast::<IntelEncoder>();
            if !encoder.is_null()
                && (*encoder).r#type == crate::intel_display_types_upstream::INTEL_OUTPUT_DP_MST
            {
                return -EOPNOTSUPP;
            }
            0
        })
    };
    if ret != 0 {
        return ret;
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
        if unsafe {
            !(*new).dp_tunnel_ref.tunnel.is_null()
                || (*new).uapi.vrr_enabled
                || (*new).use_dsb
                || (*new).use_flipq
        } {
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
        if unsafe { (*entry).new_state.is_null() } {
            continue;
        }
        let new = unsafe { (*entry).new_state.cast::<IntelCrtcState>() };
        if unsafe { (*new).dsc.compression_enable || (*new).dsc.compression_enabled_on_link } {
            return -EOPNOTSUPP;
        }
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
            unsafe { crate::intel_dpll_mgr_upstream::intel_dpll_release(state, crtc) };
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

/// Mark enabled CRTCs in a pipe mask for a full modeset and pull in their
/// connectors and planes before plane state calculation, matching Linux's
/// early modeset helper. MST/tunnel state is outside the ADL-P subset.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_modeset_pipes_in_mask_early(
    state: *mut IntelAtomicState,
    _reason: *const c_char,
    pipe_mask: u8,
) -> i32 {
    if state.is_null() || unsafe { (*state).base.dev.is_null() } {
        return -EINVAL;
    }
    let display =
        unsafe { crate::intel_display_conversion_upstream::__drm_to_display((*state).base.dev) };
    if display.is_null() {
        return -EINVAL;
    }
    for pipe in 0..I915_MAX_PIPES {
        if pipe_mask & (1u8 << pipe) == 0 {
            continue;
        }
        let crtc = unsafe { crate::intel_crtc_upstream::intel_crtc_for_pipe(display, pipe) };
        if crtc.is_null() {
            continue;
        }
        let crtc_state = unsafe {
            crate::intel_atomic_upstream::intel_atomic_get_crtc_state(&mut (*state).base, crtc)
        };
        if unsafe { IS_ERR(crtc_state) } {
            return unsafe { PTR_ERR(crtc_state.cast::<c_void>()) };
        }
        if unsafe {
            !(*crtc_state).hw.enable
                || crate::intel_atomic_upstream::intel_crtc_needs_modeset(crtc_state)
        } {
            continue;
        }
        let ret =
            unsafe { drm_atomic_add_affected_connectors(&mut (*state).base, &mut (*crtc).base) };
        if ret != 0 {
            return ret;
        }
        let ret = unsafe { drm_atomic_add_affected_planes(&mut (*state).base, &mut (*crtc).base) };
        if ret != 0 {
            return ret;
        }
        unsafe {
            (*crtc_state).uapi.set_mode_changed(true);
            (*crtc_state).update_pipe = false;
            (*crtc_state).update_m_n = false;
            (*crtc_state).update_lrr = false;
        }
    }
    0
}

/// Recommit the selected CRTC pipes while using a caller-owned DRM modeset
/// acquire context. `-EDEADLK` is returned unchanged so the caller can run the
/// standard ww-mutex backoff/retry loop.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_modeset_commit_pipes(
    display: *mut IntelDisplay,
    pipe_mask: u8,
    ctx: *mut DrmModesetAcquireCtx,
) -> i32 {
    if display.is_null() || unsafe { (*display).drm.is_null() } {
        return -EINVAL;
    }
    let state = unsafe { drm_atomic_commit_alloc((*display).drm) };
    if state.is_null() {
        return -ENOMEM;
    }
    let intel_state = state.cast::<IntelAtomicState>();
    unsafe {
        (*state).acquire_ctx = ctx;
        (*intel_state).internal = true;
    }
    let mut ret = 0;
    for pipe in 0..I915_MAX_PIPES {
        if pipe_mask & (1u8 << pipe) == 0 {
            continue;
        }
        let crtc = unsafe { crate::intel_crtc_upstream::intel_crtc_for_pipe(display, pipe) };
        if crtc.is_null() {
            continue;
        }
        let crtc_state =
            unsafe { crate::intel_atomic_upstream::intel_atomic_get_crtc_state(state, crtc) };
        if unsafe { IS_ERR(crtc_state) } {
            ret = unsafe { PTR_ERR(crtc_state.cast::<c_void>()) };
            break;
        }
        unsafe { (*crtc_state).uapi.set_connectors_changed(true) };
    }
    if ret == 0 {
        ret = unsafe { drm_atomic_commit(state) };
    }
    unsafe { drm_atomic_commit_put(state) };
    ret
}

/// Commit the BIOS-inherited ADL-P display state through the normal atomic
/// path, retrying ww-mutex deadlocks with DRM's standard backoff protocol.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_initial_commit(display: *mut IntelDisplay) -> i32 {
    if display.is_null() || unsafe { (*display).drm.is_null() } {
        return -EINVAL;
    }
    let state = unsafe { drm_atomic_commit_alloc((*display).drm) };
    if state.is_null() {
        return -ENOMEM;
    }
    let mut ctx = unsafe { core::mem::MaybeUninit::<DrmModesetAcquireCtx>::zeroed().assume_init() };
    unsafe {
        drm_modeset_acquire_init(&mut ctx, 0);
        (*state).acquire_ctx = &mut ctx;
        (*state.cast::<IntelAtomicState>()).internal = true;
    }

    let mut ret = 0;
    loop {
        let head = unsafe { ptr::addr_of_mut!((*display).pipe_list) };
        let mut node = unsafe { (*head).next };
        while node != head {
            let crtc = unsafe {
                node.cast::<u8>()
                    .sub(offset_of!(IntelCrtc, pipe_head))
                    .cast::<IntelCrtc>()
            };
            let crtc_state =
                unsafe { crate::intel_atomic_upstream::intel_atomic_get_crtc_state(state, crtc) };
            if unsafe { IS_ERR(crtc_state) } {
                ret = unsafe { PTR_ERR(crtc_state.cast::<c_void>()) };
                break;
            }
            if !unsafe { (*crtc_state).hw.active } {
                unsafe { (*crtc_state).inherited = false };
            } else {
                ret = unsafe { drm_atomic_add_affected_planes(state, &mut (*crtc).base) };
                if ret != 0 {
                    break;
                }
                unsafe { (*crtc_state).uapi.set_color_mgmt_changed(true) };
                let encoder_mask = unsafe { (*crtc_state).uapi.encoder_mask };
                let mut connectors_need_modeset = false;
                unsafe {
                    for_each_encoder(display, |encoder| {
                        let index = (*encoder).base.index;
                        if index >= 32 || encoder_mask & (1u32 << index) == 0 {
                            return;
                        }
                        if let Some(check) = (*encoder).initial_fastset_check {
                            if !check(encoder, crtc_state) {
                                connectors_need_modeset = true;
                            }
                        }
                    });
                }
                if connectors_need_modeset {
                    ret = unsafe { drm_atomic_add_affected_connectors(state, &mut (*crtc).base) };
                    if ret != 0 {
                        break;
                    }
                }
            }
            node = unsafe { (*node).next };
            if node.is_null() {
                ret = -EINVAL;
                break;
            }
        }
        if ret == 0 {
            ret = unsafe { drm_atomic_commit(state) };
        }
        if ret != -tk_linux_kapi::config::EDEADLK {
            break;
        }
        unsafe { drm_atomic_commit_clear(state) };
        let backoff = unsafe { drm_modeset_backoff(&mut ctx) };
        if backoff != 0 {
            ret = backoff;
            break;
        }
    }

    unsafe {
        drm_atomic_commit_put(state);
        drm_modeset_drop_locks(&mut ctx);
        drm_modeset_acquire_fini(&mut ctx);
    }
    ret
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
    get_initial_plane_config: Some(
        crate::skl_universal_plane_upstream::skl_get_initial_plane_config,
    ),
    fixup_initial_plane_config: Some(intel_skl_fixup_initial_plane_config),
    crtc_enable: Some(intel_hsw_crtc_enable),
    crtc_disable: Some(intel_hsw_crtc_disable),
    commit_modeset_enables: Some(intel_skl_commit_modeset_enables),
};

unsafe extern "C" fn intel_skl_fixup_initial_plane_config(
    crtc: *mut IntelCrtc,
    config: *mut crate::intel_display_types_upstream::IntelInitialPlaneConfig,
) -> bool {
    unsafe {
        crate::skl_universal_plane_upstream::skl_fixup_initial_plane_config(
            crtc,
            config.cast_const(),
        )
    }
}

#[inline]
fn intel_pipe_power_domain(pipe: Pipe) -> u32 {
    1 + pipe.max(0) as u32
}

#[inline]
fn intel_transcoder_power_domain(transcoder: i32) -> u32 {
    if transcoder == crate::intel_display_limits_types_upstream::TRANSCODER_EDP {
        crate::intel_display_power_types_upstream::POWER_DOMAIN_TRANSCODER_EDP
    } else {
        crate::intel_display_power_types_upstream::POWER_DOMAIN_TRANSCODER_A + transcoder as u32
    }
}

unsafe fn intel_read_pipe_timings(display: *mut IntelDisplay, state: *mut IntelCrtcState) {
    let transcoder = unsafe { (*state).cpu_transcoder as u32 };
    let mode = unsafe { &mut (*state).hw.adjusted_mode };
    let read = |reg| unsafe { crate::intel_de_upstream::intel_de_read(display, reg) };

    let value =
        read(unsafe { crate::intel_display_regs_upstream::TRANS_HTOTAL(display, transcoder) });
    mode.crtc_hdisplay = (crate::intel_display_reg_defs_upstream::reg_field_get(
        crate::intel_display_regs_upstream::HACTIVE_MASK,
        value,
    ) + 1) as u16;
    mode.crtc_htotal = (crate::intel_display_reg_defs_upstream::reg_field_get(
        crate::intel_display_regs_upstream::HTOTAL_MASK,
        value,
    ) + 1) as u16;
    let value =
        read(unsafe { crate::intel_display_regs_upstream::TRANS_HBLANK(display, transcoder) });
    mode.crtc_hblank_start = (crate::intel_display_reg_defs_upstream::reg_field_get(
        crate::intel_display_regs_upstream::HBLANK_START_MASK,
        value,
    ) + 1) as u16;
    mode.crtc_hblank_end = (crate::intel_display_reg_defs_upstream::reg_field_get(
        crate::intel_display_regs_upstream::HBLANK_END_MASK,
        value,
    ) + 1) as u16;
    let value =
        read(unsafe { crate::intel_display_regs_upstream::TRANS_HSYNC(display, transcoder) });
    mode.crtc_hsync_start = (crate::intel_display_reg_defs_upstream::reg_field_get(
        crate::intel_display_regs_upstream::HSYNC_START_MASK,
        value,
    ) + 1) as u16;
    mode.crtc_hsync_end = (crate::intel_display_reg_defs_upstream::reg_field_get(
        crate::intel_display_regs_upstream::HSYNC_END_MASK,
        value,
    ) + 1) as u16;
    let value =
        read(unsafe { crate::intel_display_regs_upstream::TRANS_VTOTAL(display, transcoder) });
    mode.crtc_vdisplay = (crate::intel_display_reg_defs_upstream::reg_field_get(
        crate::intel_display_regs_upstream::VACTIVE_MASK,
        value,
    ) + 1) as u16;
    mode.crtc_vtotal = (crate::intel_display_reg_defs_upstream::reg_field_get(
        crate::intel_display_regs_upstream::VTOTAL_MASK,
        value,
    ) + 1) as u16;
    let value =
        read(unsafe { crate::intel_display_regs_upstream::TRANS_VBLANK(display, transcoder) });
    mode.crtc_vblank_start = (crate::intel_display_reg_defs_upstream::reg_field_get(
        crate::intel_display_regs_upstream::VBLANK_START_MASK,
        value,
    ) + 1) as u16;
    mode.crtc_vblank_end = (crate::intel_display_reg_defs_upstream::reg_field_get(
        crate::intel_display_regs_upstream::VBLANK_END_MASK,
        value,
    ) + 1) as u16;
    let value =
        read(unsafe { crate::intel_display_regs_upstream::TRANS_VSYNC(display, transcoder) });
    mode.crtc_vsync_start = (crate::intel_display_reg_defs_upstream::reg_field_get(
        crate::intel_display_regs_upstream::VSYNC_START_MASK,
        value,
    ) + 1) as u16;
    mode.crtc_vsync_end = (crate::intel_display_reg_defs_upstream::reg_field_get(
        crate::intel_display_regs_upstream::VSYNC_END_MASK,
        value,
    ) + 1) as u16;

    let transconf =
        read(unsafe { crate::intel_display_regs_upstream::TRANSCONF(display, transcoder) });
    if transconf & crate::intel_display_regs_upstream::TRANSCONF_INTERLACE_MASK_HSW != 0 {
        mode.flags |= DRM_MODE_FLAG_INTERLACE;
        mode.crtc_vtotal = mode.crtc_vtotal.wrapping_add(1);
        mode.crtc_vblank_end = mode.crtc_vblank_end.wrapping_add(1);
    }
    if unsafe { display_ver(display) } >= DISPLAY_VER_ADLP
        && unsafe {
            (*state).cpu_transcoder != crate::intel_display_limits_types_upstream::TRANSCODER_EDP
        }
    {
        let latency = read(unsafe {
            crate::intel_display_regs_upstream::TRANS_SET_CONTEXT_LATENCY(display, transcoder)
        });
        unsafe { (*state).set_context_latency = latency as u16 };
        mode.crtc_vblank_start = mode.crtc_vdisplay.wrapping_add(latency as u16);
    }
}

/// Read the ADL-P pipe/transcoder register state. MST, DSI, and joiner
/// readout are intentionally not inferred; the corresponding entry fails
/// closed so setup cannot fabricate an active single-pipe configuration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hsw_get_pipe_config(
    crtc: *mut IntelCrtc,
    state: *mut IntelCrtcState,
) -> bool {
    if crtc.is_null() || state.is_null() {
        return false;
    }
    let display =
        unsafe { crate::intel_display_conversion_upstream::__drm_to_display((*crtc).base.dev) };
    let pipe = unsafe { (*crtc).pipe };
    if display.is_null() || !(0..4).contains(&pipe) {
        return false;
    }
    let power_set = unsafe { ptr::addr_of_mut!((*crtc).hw_readout_power_domains) };
    if !unsafe {
        crate::intel_display_power_upstream::intel_display_power_get_in_set_if_enabled(
            display,
            power_set,
            intel_pipe_power_domain(pipe),
        )
    } {
        return false;
    }

    let mut active_transcoders = 0u32;
    let pipe_transcoder = pipe as u32;
    if unsafe {
        crate::intel_display_power_upstream::intel_display_power_get_in_set_if_enabled(
            display,
            power_set,
            intel_transcoder_power_domain(pipe),
        )
    } {
        let func = unsafe {
            crate::intel_de_upstream::intel_de_read(
                display,
                crate::intel_display_regs_upstream::TRANS_DDI_FUNC_CTL(display, pipe_transcoder),
            )
        };
        if func & crate::intel_display_regs_upstream::TRANS_DDI_FUNC_ENABLE != 0 {
            active_transcoders |= 1u32 << pipe_transcoder;
        }
    }

    // ADL-P's panel transcoder is separate from pipes A-D; it can select any
    // of those pipes as its input.
    let edp = crate::intel_display_limits_types_upstream::TRANSCODER_EDP as u32;
    if unsafe {
        crate::intel_display_power_upstream::intel_display_power_get_in_set_if_enabled(
            display,
            power_set,
            intel_transcoder_power_domain(edp as i32),
        )
    } {
        let func = unsafe {
            crate::intel_de_upstream::intel_de_read(
                display,
                crate::intel_display_regs_upstream::TRANS_DDI_FUNC_CTL(display, edp),
            )
        };
        let input = match pipe {
            0 => crate::intel_display_regs_upstream::TRANS_DDI_EDP_INPUT_A_ONOFF,
            1 => crate::intel_display_regs_upstream::TRANS_DDI_EDP_INPUT_B_ONOFF,
            2 => crate::intel_display_regs_upstream::TRANS_DDI_EDP_INPUT_C_ONOFF,
            _ => crate::intel_display_regs_upstream::TRANS_DDI_EDP_INPUT_D_ONOFF,
        };
        if func & crate::intel_display_regs_upstream::TRANS_DDI_FUNC_ENABLE != 0
            && func & crate::intel_display_regs_upstream::TRANS_DDI_EDP_INPUT_MASK == input
        {
            active_transcoders |= 1u32 << edp;
            if input == crate::intel_display_regs_upstream::TRANS_DDI_EDP_INPUT_A_ONOFF {
                unsafe { (*state).pch_pfit.force_thru = true };
            }
        }
    }

    if active_transcoders.count_ones() != 1 {
        unsafe {
            crate::intel_display_power_upstream::intel_display_power_put_mask_in_set(
                display, power_set,
            )
        };
        return false;
    }
    let transcoder = active_transcoders.trailing_zeros() as i32;
    unsafe { (*state).cpu_transcoder = transcoder };
    let transconf = unsafe {
        crate::intel_de_upstream::intel_de_read(
            display,
            crate::intel_display_regs_upstream::TRANSCONF(display, transcoder as u32),
        )
    };
    if transconf & crate::intel_display_regs_upstream::TRANSCONF_ENABLE == 0 {
        unsafe {
            crate::intel_display_power_upstream::intel_display_power_put_mask_in_set(
                display, power_set,
            )
        };
        return false;
    }

    let chicken = unsafe {
        crate::intel_de_upstream::intel_de_read(
            display,
            crate::intel_display_regs_upstream::CHICKEN_TRANS(display, transcoder as u32),
        )
    };
    unsafe {
        (*state).framestart_delay = (crate::intel_display_reg_defs_upstream::reg_field_get(
            crate::intel_display_regs_upstream::HSW_FRAME_START_DELAY_MASK,
            chicken,
        ) + 1) as u8;
        intel_read_pipe_timings(display, state);
        (*state).pipe_src = tk_drm_upstream::drm_rect_types::DrmRect {
            x1: 0,
            y1: 0,
            x2: 0,
            y2: 0,
        };
        let pipesrc = crate::intel_de_upstream::intel_de_read(
            display,
            crate::intel_display_regs_upstream::PIPESRC(display, pipe as u32),
        );
        tk_drm_upstream::drm_rect_upstream::drm_rect_init(
            &mut (*state).pipe_src,
            0,
            0,
            (crate::intel_display_reg_defs_upstream::reg_field_get(
                crate::intel_display_regs_upstream::PIPESRC_WIDTH_MASK,
                pipesrc,
            ) + 1) as i32,
            (crate::intel_display_reg_defs_upstream::reg_field_get(
                crate::intel_display_regs_upstream::PIPESRC_HEIGHT_MASK,
                pipesrc,
            ) + 1) as i32,
        );
        let misc = crate::intel_de_upstream::intel_de_read(
            display,
            crate::intel_display_regs_upstream::PIPE_MISC(pipe as u32),
        );
        (*state).output_format = if misc
            & crate::intel_display_regs_upstream::PIPE_MISC_YUV420_ENABLE
            != 0
        {
            crate::intel_display_types_upstream::INTEL_OUTPUT_FORMAT_YCBCR420
        } else if misc & crate::intel_display_regs_upstream::PIPE_MISC_OUTPUT_COLORSPACE_YUV != 0 {
            crate::intel_display_types_upstream::INTEL_OUTPUT_FORMAT_YCBCR444
        } else {
            crate::intel_display_types_upstream::INTEL_OUTPUT_FORMAT_RGB
        };
        (*state).sink_format = (*state).output_format;
        let wm = crate::intel_de_upstream::intel_de_read(
            display,
            crate::intel_display_regs_upstream::WM_LINETIME(pipe as u32),
        );
        (*state).linetime = crate::intel_display_reg_defs_upstream::reg_field_get(
            crate::intel_display_regs_upstream::HSW_LINETIME_MASK,
            wm,
        ) as u16;
    }
    unsafe { crate::intel_color_upstream::intel_color_get_config(state) };

    let pfit_domain =
        crate::intel_display_power_types_upstream::POWER_DOMAIN_PIPE_PANEL_FITTER_A + pipe as u32;
    if unsafe {
        crate::intel_display_power_upstream::intel_display_power_get_in_set_if_enabled(
            display,
            power_set,
            pfit_domain,
        )
    } {
        unsafe { crate::skl_scaler_upstream::skl_scaler_get_config(state) };
    }
    unsafe {
        (*state).pixel_multiplier = if transcoder == edp as i32 {
            1
        } else {
            crate::intel_de_upstream::intel_de_read(
                display,
                crate::intel_display_regs_upstream::TRANS_MULT(display, transcoder as u32),
            ) + 1
        };
        crate::intel_display_power_upstream::intel_display_power_put_mask_in_set(
            display, power_set,
        );
    }
    true
}

unsafe fn intel_set_pipe_src_size(state: *const IntelCrtcState) {
    if state.is_null() || unsafe { (*state).uapi.crtc.is_null() } {
        return;
    }
    let crtc = unsafe { (*state).uapi.crtc.cast::<IntelCrtc>() };
    let display =
        unsafe { crate::intel_display_conversion_upstream::__drm_to_display((*crtc).base.dev) };
    if display.is_null() {
        return;
    }
    let width = unsafe { (*state).pipe_src.x2 - (*state).pipe_src.x1 };
    let height = unsafe { (*state).pipe_src.y2 - (*state).pipe_src.y1 };
    if width <= 0 || height <= 0 {
        return;
    }
    unsafe {
        crate::intel_de_upstream::intel_de_write(
            display,
            crate::intel_display_regs_upstream::PIPESRC(display, (*crtc).pipe as u32),
            crate::intel_display_regs_upstream::PIPESRC_WIDTH(width as u32 - 1)
                | crate::intel_display_regs_upstream::PIPESRC_HEIGHT(height as u32 - 1),
        );
    }
}

unsafe fn intel_set_pipe_misc(state: *const IntelCrtcState) {
    if state.is_null() || unsafe { (*state).uapi.crtc.is_null() } {
        return;
    }
    let crtc = unsafe { (*state).uapi.crtc.cast::<IntelCrtc>() };
    let display =
        unsafe { crate::intel_display_conversion_upstream::__drm_to_display((*crtc).base.dev) };
    if display.is_null() {
        return;
    }
    let mut val = match unsafe { (*state).pipe_bpp } {
        18 => crate::intel_display_regs_upstream::PIPE_MISC_BPC_6,
        24 => crate::intel_display_regs_upstream::PIPE_MISC_BPC_8,
        30 => crate::intel_display_regs_upstream::PIPE_MISC_BPC_10,
        36 if unsafe { display_ver(display) } >= DISPLAY_VER_ADLP => {
            crate::intel_display_regs_upstream::PIPE_MISC_BPC_12_ADLP
        }
        _ => return,
    };
    unsafe {
        if (*state).dither {
            val |= crate::intel_display_regs_upstream::PIPE_MISC_DITHER_ENABLE
                | crate::intel_display_regs_upstream::PIPE_MISC_DITHER_TYPE_SP;
        }
        if (*state).output_format != crate::intel_display_types_upstream::INTEL_OUTPUT_FORMAT_RGB {
            val |= crate::intel_display_regs_upstream::PIPE_MISC_OUTPUT_COLORSPACE_YUV;
        }
        if (*state).output_format
            == crate::intel_display_types_upstream::INTEL_OUTPUT_FORMAT_YCBCR420
        {
            val |= crate::intel_display_regs_upstream::PIPE_MISC_YUV420_ENABLE
                | crate::intel_display_regs_upstream::PIPE_MISC_YUV420_MODE_FULL_BLEND;
        }
        if unsafe { display_ver(display) } >= 12 {
            val |= crate::intel_display_regs_upstream::PIPE_MISC_PIXEL_ROUNDING_TRUNC;
        }
        crate::intel_de_upstream::intel_de_write(
            display,
            crate::intel_display_regs_upstream::PIPE_MISC((*crtc).pipe as u32),
            val,
        );
    }
}

unsafe fn intel_set_transcoder_timings(state: *const IntelCrtcState) {
    if state.is_null() || unsafe { (*state).uapi.crtc.is_null() } {
        return;
    }
    let crtc = unsafe { (*state).uapi.crtc.cast::<IntelCrtc>() };
    let display =
        unsafe { crate::intel_display_conversion_upstream::__drm_to_display((*crtc).base.dev) };
    if display.is_null() {
        return;
    }
    let trans = unsafe { (*state).cpu_transcoder as u32 };
    let mode = unsafe { &(*state).hw.adjusted_mode };
    unsafe {
        if display_ver(display) >= DISPLAY_VER_ADLP {
            crate::intel_de_upstream::intel_de_write(
                display,
                crate::intel_display_regs_upstream::TRANS_SET_CONTEXT_LATENCY(display, trans),
                (*state).set_context_latency as u32,
            );
        }
        crate::intel_de_upstream::intel_de_write(
            display,
            crate::intel_display_regs_upstream::TRANS_HTOTAL(display, trans),
            crate::intel_display_regs_upstream::HACTIVE(mode.crtc_hdisplay as u32 - 1)
                | crate::intel_display_regs_upstream::HTOTAL(mode.crtc_htotal as u32 - 1),
        );
        crate::intel_de_upstream::intel_de_write(
            display,
            crate::intel_display_regs_upstream::TRANS_HBLANK(display, trans),
            crate::intel_display_regs_upstream::HBLANK_START(mode.crtc_hblank_start as u32 - 1)
                | crate::intel_display_regs_upstream::HBLANK_END(mode.crtc_hblank_end as u32 - 1),
        );
        crate::intel_de_upstream::intel_de_write(
            display,
            crate::intel_display_regs_upstream::TRANS_HSYNC(display, trans),
            crate::intel_display_regs_upstream::HSYNC_START(mode.crtc_hsync_start as u32 - 1)
                | crate::intel_display_regs_upstream::HSYNC_END(mode.crtc_hsync_end as u32 - 1),
        );
        crate::intel_de_upstream::intel_de_write(
            display,
            crate::intel_display_regs_upstream::TRANS_VTOTAL(display, trans),
            crate::intel_display_regs_upstream::VACTIVE(mode.crtc_vdisplay as u32 - 1)
                | crate::intel_display_regs_upstream::VTOTAL(mode.crtc_vtotal as u32 - 1),
        );
        let vblank_start = if display_ver(display) >= DISPLAY_VER_ADLP {
            0
        } else {
            mode.crtc_vblank_start as u32 - 1
        };
        crate::intel_de_upstream::intel_de_write(
            display,
            crate::intel_display_regs_upstream::TRANS_VBLANK(display, trans),
            crate::intel_display_regs_upstream::VBLANK_START(vblank_start)
                | crate::intel_display_regs_upstream::VBLANK_END(mode.crtc_vblank_end as u32 - 1),
        );
        crate::intel_de_upstream::intel_de_write(
            display,
            crate::intel_display_regs_upstream::TRANS_VSYNC(display, trans),
            crate::intel_display_regs_upstream::VSYNC_START(mode.crtc_vsync_start as u32 - 1)
                | crate::intel_display_regs_upstream::VSYNC_END(mode.crtc_vsync_end as u32 - 1),
        );
    }
}

/// Public C ABI called by the DDI encoder enable/disable path.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_enable_transcoder(state: *const IntelCrtcState) {
    if state.is_null() || unsafe { (*state).uapi.crtc.is_null() } {
        return;
    }
    let crtc = unsafe { (*state).uapi.crtc.cast::<IntelCrtc>() };
    let display =
        unsafe { crate::intel_display_conversion_upstream::__drm_to_display((*crtc).base.dev) };
    if display.is_null() {
        return;
    }
    let trans = unsafe { (*state).cpu_transcoder as u32 };
    let reg = unsafe { crate::intel_display_regs_upstream::TRANSCONF(display, trans) };
    let mut value = unsafe { crate::intel_de_upstream::intel_de_read(display, reg) };
    if unsafe { (*state).dsc.compression_enable } {
        value &= !crate::intel_display_regs_upstream::TRANSCONF_PIXEL_COUNT_SCALING_MASK;
        value |= crate::intel_display_regs_upstream::TRANSCONF_PIXEL_COUNT_SCALING_X4;
    }
    unsafe {
        crate::intel_de_upstream::intel_de_write(
            display,
            reg,
            value | crate::intel_display_regs_upstream::TRANSCONF_ENABLE,
        );
        crate::intel_de_upstream::intel_de_posting_read(display, reg);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_disable_transcoder(state: *const IntelCrtcState) {
    if state.is_null() || unsafe { (*state).uapi.crtc.is_null() } {
        return;
    }
    let crtc = unsafe { (*state).uapi.crtc.cast::<IntelCrtc>() };
    let display =
        unsafe { crate::intel_display_conversion_upstream::__drm_to_display((*crtc).base.dev) };
    if display.is_null() {
        return;
    }
    let reg = unsafe {
        crate::intel_display_regs_upstream::TRANSCONF(display, (*state).cpu_transcoder as u32)
    };
    unsafe {
        let mut value = crate::intel_de_upstream::intel_de_read(display, reg);
        if (*state).dsc.compression_enable {
            value &= !crate::intel_display_regs_upstream::TRANSCONF_PIXEL_COUNT_SCALING_MASK;
        }
        value &= !crate::intel_display_regs_upstream::TRANSCONF_ENABLE;
        crate::intel_de_upstream::intel_de_write(display, reg, value);
        crate::intel_de_upstream::intel_de_posting_read(display, reg);
    }
}

unsafe extern "C" fn intel_hsw_crtc_get_power_domain_set(
    state: *const IntelCrtcState,
    wanted: *mut crate::intel_display_power_types_upstream::IntelDisplayPowerDomainSet,
) {
    if state.is_null() || wanted.is_null() || unsafe { (*state).uapi.crtc.is_null() } {
        return;
    }
    unsafe { ptr::write_bytes(wanted, 0, 1) };
    if unsafe { !(*state).hw.active } {
        return;
    }
    let crtc = unsafe { (*state).uapi.crtc.cast::<IntelCrtc>() };
    let dev = unsafe { (*crtc).base.dev };
    let display = unsafe { crate::intel_display_conversion_upstream::__drm_to_display(dev) };
    if dev.is_null() || display.is_null() {
        return;
    }
    unsafe { intel_power_domain_set_mask_add(wanted, intel_pipe_power_domain((*crtc).pipe)) };
    unsafe {
        intel_power_domain_set_mask_add(
            wanted,
            intel_transcoder_power_domain((*state).cpu_transcoder),
        )
    };
    if unsafe { (*state).pch_pfit.enabled || (*state).pch_pfit.force_thru } {
        unsafe {
            intel_power_domain_set_mask_add(
                wanted,
                crate::intel_display_power_types_upstream::POWER_DOMAIN_PIPE_PANEL_FITTER_A
                    + unsafe { (*crtc).pipe } as u32,
            )
        };
    }
    if unsafe { (*state).has_audio && has_ddi(display) } {
        unsafe {
            intel_power_domain_set_mask_add(
                wanted,
                crate::intel_display_power_types_upstream::POWER_DOMAIN_AUDIO_MMIO,
            )
        };
    }
    if unsafe { !(*state).intel_dpll.is_null() } {
        unsafe {
            intel_power_domain_set_mask_add(
                wanted,
                crate::intel_display_power_types_upstream::POWER_DOMAIN_DISPLAY_CORE,
            )
        };
    }
    if unsafe { (*state).dsc.compression_enable } {
        unsafe {
            intel_power_domain_set_mask_add(
                wanted,
                crate::intel_display_power_types_upstream::POWER_DOMAIN_TRANSCODER_VDSC_PW2,
            )
        };
    }
    unsafe {
        for_each_encoder(display, |encoder| {
            let index = (*encoder).base.index;
            if index < 32 && (*state).uapi.encoder_mask & (1u32 << index) != 0 {
                intel_power_domain_set_mask_add(wanted, (*encoder).power_domain);
            }
        });
    }
}

unsafe fn intel_power_domain_set_mask_add(
    set: *mut crate::intel_display_power_types_upstream::IntelDisplayPowerDomainSet,
    domain: u32,
) {
    if set.is_null() {
        return;
    }
    let word = (domain / 64) as usize;
    if word < 2 {
        unsafe { (*set).mask.bits[word] |= 1u64 << (domain % 64) };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_modeset_get_crtc_power_domains(
    state: *const IntelCrtcState,
    put: *mut crate::intel_display_power_types_upstream::IntelPowerDomainMask,
) {
    if state.is_null() || put.is_null() || unsafe { (*state).uapi.crtc.is_null() } {
        return;
    }
    let crtc = unsafe { (*state).uapi.crtc.cast::<IntelCrtc>() };
    let display =
        unsafe { crate::intel_display_conversion_upstream::__drm_to_display((*crtc).base.dev) };
    if display.is_null() {
        return;
    }
    let mut wanted = unsafe {
        core::mem::MaybeUninit::<
            crate::intel_display_power_types_upstream::IntelDisplayPowerDomainSet,
        >::zeroed()
        .assume_init()
    };
    unsafe { intel_hsw_crtc_get_power_domain_set(state, &mut wanted) };
    let old = unsafe { &mut (*crtc).enabled_power_domains.mask };
    for domain in 0..crate::intel_display_power_types_upstream::POWER_DOMAIN_NUM {
        let word = (domain / 64) as usize;
        let bit = 1u64 << (domain % 64);
        let had = old.bits[word] & bit != 0;
        let need = wanted.mask.bits[word] & bit != 0;
        if need && !had {
            unsafe {
                crate::intel_display_power_upstream::intel_display_power_get_in_set(
                    display,
                    &mut (*crtc).enabled_power_domains,
                    domain,
                )
            };
        } else if had && !need {
            unsafe { (*put).bits[word] |= bit };
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_modeset_put_crtc_power_domains(
    crtc: *mut IntelCrtc,
    put: *mut crate::intel_display_power_types_upstream::IntelPowerDomainMask,
) {
    if crtc.is_null() || put.is_null() {
        return;
    }
    let display =
        unsafe { crate::intel_display_conversion_upstream::__drm_to_display((*crtc).base.dev) };
    if !display.is_null() {
        let set =
            put.cast::<crate::intel_display_power_types_upstream::IntelDisplayPowerDomainSet>();
        unsafe {
            crate::intel_display_power_upstream::intel_display_power_put_mask_in_set(display, set)
        };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hsw_crtc_enable(state: *mut IntelAtomicState, crtc: *mut IntelCrtc) {
    if state.is_null() || crtc.is_null() {
        return;
    }
    let display =
        unsafe { crate::intel_display_conversion_upstream::__drm_to_display((*crtc).base.dev) };
    let new = unsafe { crate::intel_atomic_upstream::intel_atomic_get_new_crtc_state(state, crtc) };
    if display.is_null() || new.is_null() || unsafe { (*crtc).active } {
        return;
    }
    unsafe { crate::intel_dmc_upstream::intel_dmc_enable_pipe(new) };
    unsafe {
        for_each_new_connector_in_state(state, |_, connector_state| {
            if (*connector_state).crtc != &mut (*crtc).base {
                return 0;
            }
            let encoder = (*connector_state).best_encoder.cast::<IntelEncoder>();
            if encoder.is_null() {
                return -EINVAL;
            }
            if let Some(callback) = (*encoder).pre_pll_enable {
                callback(state, encoder, new, connector_state);
            }
            0
        });
    }
    if unsafe { !(*new).intel_dpll.is_null() } {
        unsafe { crate::intel_dpll_mgr_upstream::intel_dpll_enable(new) };
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
            if let Some(callback) = (*encoder).pre_enable {
                callback(state, encoder, new, connector_state);
            }
            0
        });
    }
    unsafe {
        intel_set_pipe_src_size(new);
        intel_set_pipe_misc(new);
        intel_set_transcoder_timings(new);
        if (*new).cpu_transcoder != crate::intel_display_limits_types_upstream::TRANSCODER_EDP {
            crate::intel_de_upstream::intel_de_write(
                display,
                crate::intel_display_regs_upstream::TRANS_MULT(
                    display,
                    (*new).cpu_transcoder as u32,
                ),
                (*new).pixel_multiplier.saturating_sub(1),
            );
        }
        crate::intel_de_upstream::intel_de_rmw(
            display,
            crate::intel_display_regs_upstream::CHICKEN_TRANS(
                display,
                (*new).cpu_transcoder as u32,
            ),
            crate::intel_display_regs_upstream::HSW_FRAME_START_DELAY_MASK,
            crate::intel_display_regs_upstream::HSW_FRAME_START_DELAY(
                (*new).framestart_delay.saturating_sub(1) as u32,
            ),
        );
        let mut transconf = crate::intel_display_regs_upstream::TRANSCONF_INTERLACE_PROGRESSIVE;
        if (*new).hw.adjusted_mode.flags & tk_drm_upstream::uapi_drm_consts::DRM_MODE_FLAG_INTERLACE
            != 0
        {
            transconf = crate::intel_display_regs_upstream::TRANSCONF_INTERLACE_W_SYNC_SHIFT;
        }
        transconf |= crate::intel_display_regs_upstream::TRANSCONF_GAMMA_MODE((*new).gamma_mode);
        let reg =
            crate::intel_display_regs_upstream::TRANSCONF(display, (*new).cpu_transcoder as u32);
        crate::intel_de_upstream::intel_de_write(display, reg, transconf);
        crate::intel_de_upstream::intel_de_posting_read(display, reg);
        (*crtc).active = true;
        if (*new).pch_pfit.enabled {
            crate::skl_scaler_upstream::skl_pfit_enable(new);
        }
        crate::intel_color_upstream::intel_color_modeset(new);
        (*new).linetime = ((i64::from((*new).hw.pipe_mode.crtc_htotal) * 8_000
            + i64::from((*new).pixel_rate).max(1)
            - 1)
            / i64::from((*new).pixel_rate).max(1))
        .clamp(0, 0x1ff) as u16;
        let _ = crate::intel_wm_upstream::intel_initial_watermarks(state, crtc);
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
            if let Some(callback) = (*encoder).enable {
                callback(state, encoder, new, connector_state);
            }
            if (*new).has_audio {
                if let Some(callback) = (*encoder).audio_enable {
                    callback(encoder, new, connector_state);
                }
            }
            0
        });
        crate::intel_fifo_underrun_upstream::intel_set_cpu_fifo_underrun_reporting(
            display,
            (*crtc).pipe,
            true,
        );
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hsw_crtc_disable(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
) {
    if state.is_null() || crtc.is_null() {
        return;
    }
    let display =
        unsafe { crate::intel_display_conversion_upstream::__drm_to_display((*crtc).base.dev) };
    let old = unsafe { crate::intel_atomic_upstream::intel_atomic_get_old_crtc_state(state, crtc) };
    if display.is_null() || old.is_null() {
        return;
    }
    unsafe {
        crate::intel_fifo_underrun_upstream::intel_set_cpu_fifo_underrun_reporting(
            display,
            (*crtc).pipe,
            false,
        );
        let count = (*state).base.num_connector.max(0) as usize;
        for index in 0..count {
            let entry = (*state).base.connectors.add(index);
            let connector_state = (*entry).old_state;
            if connector_state.is_null() || (*connector_state).crtc != &mut (*crtc).base {
                continue;
            }
            let encoder = (*connector_state).best_encoder.cast::<IntelEncoder>();
            if encoder.is_null() {
                continue;
            }
            if let Some(callback) = (*encoder).disable {
                callback(state, encoder, old, connector_state);
            }
            if let Some(callback) = (*encoder).post_disable {
                callback(state, encoder, old, connector_state);
            }
        }
        crate::intel_dpll_mgr_upstream::intel_dpll_disable(old);
        for index in 0..count {
            let entry = (*state).base.connectors.add(index);
            let connector_state = (*entry).old_state;
            if connector_state.is_null() || (*connector_state).crtc != &mut (*crtc).base {
                continue;
            }
            let encoder = (*connector_state).best_encoder.cast::<IntelEncoder>();
            if !encoder.is_null() {
                if let Some(callback) = (*encoder).post_pll_disable {
                    callback(state, encoder, old, connector_state);
                }
            }
        }
        crate::intel_dmc_upstream::intel_dmc_disable_pipe(old);
        (*crtc).active = false;
        crate::intel_fifo_underrun_upstream::intel_set_cpu_fifo_underrun_reporting(
            display,
            (*crtc).pipe,
            true,
        );
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_skl_commit_modeset_enables(state: *mut IntelAtomicState) {
    if state.is_null() || unsafe { (*state).base.dev.is_null() || (*state).base.crtcs.is_null() } {
        return;
    }
    let dev = unsafe { (*state).base.dev };
    let count = unsafe { (*dev).mode_config.num_crtc.max(0) as usize };
    for index in 0..count {
        let entry = unsafe { (*state).base.crtcs.add(index) };
        if unsafe { (*entry).ptr.is_null() || (*entry).new_state.is_null() } {
            continue;
        }
        let crtc = unsafe { (*entry).ptr.cast::<IntelCrtc>() };
        let new = unsafe { (*entry).new_state.cast::<IntelCrtcState>() };
        if !unsafe { (*new).hw.active } {
            continue;
        }
        if unsafe { (*new).use_dsb && !(*new).dsb_commit.is_null() } {
            unsafe { crate::intel_dsb_upstream::intel_dsb_commit((*new).dsb_commit.cast()) };
            continue;
        }
        if unsafe { crate::intel_atomic_upstream::intel_crtc_needs_modeset(new) } {
            unsafe { intel_hsw_crtc_enable(state, crtc) };
        } else {
            unsafe { crate::intel_crtc_upstream::intel_pipe_update_start(state, crtc) };
            if unsafe { intel_crtc_needs_fastset(new) } {
                unsafe {
                    intel_set_pipe_src_size(new);
                    intel_set_pipe_misc(new);
                    intel_set_transcoder_timings(new);
                }
                let connectors = unsafe { (*state).base.num_connector.max(0) as usize };
                for conn_index in 0..connectors {
                    let conn = unsafe { (*state).base.connectors.add(conn_index) };
                    if unsafe { (*conn).new_state.is_null() || (*conn).ptr.is_null() } {
                        continue;
                    }
                    let connector_state = unsafe { (*conn).new_state };
                    if unsafe { (*connector_state).crtc != &mut (*crtc).base } {
                        continue;
                    }
                    let encoder = unsafe { (*connector_state).best_encoder.cast::<IntelEncoder>() };
                    if !encoder.is_null() {
                        if let Some(update) = unsafe { (*encoder).update_pipe } {
                            unsafe { update(state, encoder, new, connector_state) };
                        }
                    }
                }
            }
            if unsafe { intel_crtc_needs_color_update(new) } {
                unsafe {
                    crate::intel_color_upstream::intel_color_commit_noarm(ptr::null_mut(), new);
                    crate::intel_color_upstream::intel_color_commit_arm(ptr::null_mut(), new);
                }
            }
            unsafe { crate::intel_wm_upstream::intel_atomic_update_watermarks(state, crtc) };
        }
        unsafe {
            crate::intel_plane_upstream::intel_crtc_planes_update_noarm(
                ptr::null_mut(),
                state,
                crtc,
            );
            crate::intel_plane_upstream::intel_crtc_planes_update_arm(ptr::null_mut(), state, crtc);
            if !crate::intel_atomic_upstream::intel_crtc_needs_modeset(new) {
                crate::intel_crtc_upstream::intel_pipe_update_end(state, crtc);
            }
        }
    }
}

unsafe fn intel_disable_plane_updates(state: *mut IntelAtomicState, crtc: *mut IntelCrtc) {
    if state.is_null() || crtc.is_null() || unsafe { (*state).base.dev.is_null() } {
        return;
    }
    let new = unsafe { crate::intel_atomic_upstream::intel_atomic_get_new_crtc_state(state, crtc) };
    if new.is_null() {
        return;
    }
    let count = unsafe { (*(*state).base.dev).mode_config.num_total_plane.max(0) as usize };
    for index in 0..count {
        let entry = unsafe { (*state).base.planes.add(index) };
        if unsafe { (*entry).ptr.is_null() || (*entry).new_state.is_null() } {
            continue;
        }
        let plane = unsafe { (*entry).ptr.cast::<IntelPlane>() };
        let bit = 1u8 << unsafe { (*plane).id };
        if unsafe { (*plane).pipe != (*crtc).pipe || (*new).update_planes & bit == 0 } {
            continue;
        }
        unsafe {
            crate::intel_plane_upstream::intel_plane_disable_arm(ptr::null_mut(), plane, new)
        };
    }
}

unsafe fn intel_atomic_dsb_prepare(state: *mut IntelAtomicState, crtc: *mut IntelCrtc) {
    if state.is_null() || crtc.is_null() {
        return;
    }
    let display =
        unsafe { crate::intel_display_conversion_upstream::__drm_to_display((*crtc).base.dev) };
    let new = unsafe { crate::intel_atomic_upstream::intel_atomic_get_new_crtc_state(state, crtc) };
    if display.is_null() || new.is_null() || unsafe { !(*new).hw.active } {
        return;
    }
    if unsafe { (*state).base.legacy_cursor_update() } {
        return;
    }
    let use_dsb = unsafe {
        !crate::intel_atomic_upstream::intel_crtc_needs_modeset(new)
            && !intel_crtc_needs_fastset(new)
            && !(*new).do_async_flip
            && !(*new).has_psr
            && !(*new).vrr.enable
    };
    // FlipQ remains disabled; DSB is opportunistic. intel_dsb_prepare()
    // validates platform/runtime support and falls back to regular writes.
    unsafe { (*new).use_flipq = false };
    unsafe { (*new).use_dsb = use_dsb };
    unsafe { crate::intel_color_upstream::intel_color_prepare_commit(state, crtc) };
}

unsafe fn intel_atomic_dsb_finish(state: *mut IntelAtomicState, crtc: *mut IntelCrtc) {
    if state.is_null() || crtc.is_null() {
        return;
    }
    let display =
        unsafe { crate::intel_display_conversion_upstream::__drm_to_display((*crtc).base.dev) };
    let new = unsafe { crate::intel_atomic_upstream::intel_atomic_get_new_crtc_state(state, crtc) };
    if display.is_null() || new.is_null() {
        return;
    }
    if unsafe { !(*new).use_dsb && (*new).dsb_color.is_null() } {
        return;
    }
    let size = if unsafe { (*new).use_dsb && (*new).plane_color_changed } {
        8192
    } else if unsafe { (*new).use_dsb } {
        1024
    } else {
        16
    };
    let dsb = unsafe { crate::intel_dsb_upstream::intel_dsb_prepare(state, crtc, 0, size) };
    if dsb.is_null() {
        unsafe { (*new).use_dsb = false };
        unsafe { crate::intel_color_upstream::intel_color_cleanup_commit(new) };
        return;
    }
    unsafe { (*new).dsb_commit = dsb.cast::<c_void>() };
    if unsafe { (*new).use_dsb } {
        if unsafe { intel_crtc_needs_color_update(new) } {
            unsafe { crate::intel_color_upstream::intel_color_commit_noarm(dsb.cast(), new) };
        }
        unsafe {
            crate::intel_plane_upstream::intel_crtc_planes_update_noarm(dsb.cast(), state, crtc);
            crate::intel_dsb_upstream::intel_dsb_vblank_evade(state, dsb);
        }
        if unsafe { intel_crtc_needs_color_update(new) } {
            unsafe { crate::intel_color_upstream::intel_color_commit_arm(dsb.cast(), new) };
        }
        unsafe {
            crate::intel_plane_upstream::intel_crtc_planes_update_arm(dsb.cast(), state, crtc);
            crate::skl_scaler_upstream::skl_detach_scalers(dsb.cast(), new);
            crate::intel_dsb_upstream::intel_dsb_wait_vblanks(dsb, 1);
        }
    }
    unsafe {
        if crate::intel_color_upstream::intel_color_uses_chained_dsb(new) {
            crate::intel_dsb_upstream::intel_dsb_chain(state, dsb, (*new).dsb_color.cast(), true);
        } else if crate::intel_color_upstream::intel_color_uses_gosub_dsb(new) {
            crate::intel_dsb_upstream::intel_dsb_gosub(dsb, (*new).dsb_color.cast());
        }
        if (*new).use_dsb {
            crate::intel_dsb_upstream::intel_dsb_interrupt(dsb);
        }
        crate::intel_dsb_upstream::intel_dsb_finish(dsb);
    }
}

unsafe fn intel_atomic_dsb_wait_commit(state: *mut IntelCrtcState) {
    if state.is_null() {
        return;
    }
    unsafe {
        if !(*state).dsb_commit.is_null() {
            crate::intel_dsb_upstream::intel_dsb_wait((*state).dsb_commit.cast());
        }
        crate::intel_color_upstream::intel_color_wait_commit(state);
    }
}

unsafe fn intel_atomic_dsb_cleanup(state: *mut IntelCrtcState) {
    if state.is_null() {
        return;
    }
    unsafe {
        if !(*state).dsb_commit.is_null() {
            crate::intel_dsb_upstream::intel_dsb_cleanup((*state).dsb_commit.cast());
            (*state).dsb_commit = ptr::null_mut();
        }
        crate::intel_color_upstream::intel_color_cleanup_commit(state);
    }
}

unsafe fn intel_encoders_update_prepare(state: *mut IntelAtomicState) {
    if state.is_null() || unsafe { (*state).base.dev.is_null() } {
        return;
    }
    let dev = unsafe { (*state).base.dev };
    let display = unsafe { crate::intel_display_conversion_upstream::__drm_to_display(dev) };
    if display.is_null() || unsafe { (*display).dpll.mgr.is_null() } {
        return;
    }
    let count = unsafe { (*dev).mode_config.num_crtc.max(0) as usize };
    for index in 0..count {
        let entry = unsafe { (*state).base.crtcs.add(index) };
        if unsafe { (*entry).new_state.is_null() || (*entry).old_state.is_null() } {
            continue;
        }
        let new = unsafe { (*entry).new_state.cast::<IntelCrtcState>() };
        let old = unsafe { (*entry).old_state.cast::<IntelCrtcState>() };
        if unsafe { crate::intel_atomic_upstream::intel_crtc_needs_modeset(new) } {
            continue;
        }
        unsafe {
            (*new).intel_dpll = (*old).intel_dpll;
            ptr::copy_nonoverlapping(
                ptr::addr_of!((*old).dpll_hw_state),
                ptr::addr_of_mut!((*new).dpll_hw_state),
                1,
            );
        }
    }
}

unsafe fn intel_send_disabled_crtc_events(state: *mut IntelAtomicState) {
    if state.is_null() || unsafe { (*state).base.dev.is_null() || (*state).base.crtcs.is_null() } {
        return;
    }
    let dev = unsafe { (*state).base.dev };
    let count = unsafe { (*dev).mode_config.num_crtc.max(0) as usize };
    for index in 0..count {
        let entry = unsafe { (*state).base.crtcs.add(index) };
        if unsafe { (*entry).ptr.is_null() || (*entry).new_state.is_null() } {
            continue;
        }
        let crtc = unsafe { (*entry).ptr.cast::<IntelCrtc>() };
        let new = unsafe { (*entry).new_state.cast::<IntelCrtcState>() };
        if !unsafe {
            crate::intel_atomic_upstream::intel_crtc_needs_modeset(new)
                && !(*new).hw.active
                && !(*new).uapi.event.is_null()
        } {
            continue;
        }
        unsafe {
            tk_linux_kapi::locks::spin_lock_irq(&mut (*dev).event_lock);
            tk_drm_upstream::drm_vblank_upstream::drm_crtc_send_vblank_event(
                &mut (*crtc).base,
                (*new).uapi.event,
            );
            tk_linux_kapi::locks::spin_unlock_irq(&mut (*dev).event_lock);
            (*new).uapi.event = ptr::null_mut();
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_commit_tail(atomic: *mut DrmAtomicCommit) {
    if atomic.is_null() || unsafe { (*atomic).dev.is_null() } {
        return;
    }
    let state = atomic.cast::<IntelAtomicState>();
    let dev = unsafe { (*atomic).dev };
    let display = unsafe { crate::intel_display_conversion_upstream::__drm_to_display(dev) };
    if display.is_null() {
        return;
    }
    let crtc_count = unsafe { (*dev).mode_config.num_crtc.max(0) as usize };
    for index in 0..crtc_count {
        let entry = unsafe { (*atomic).crtcs.add(index) };
        if unsafe { !(*entry).ptr.is_null() && !(*entry).new_state.is_null() } {
            unsafe {
                intel_atomic_dsb_prepare(state, (*entry).ptr.cast::<IntelCrtc>());
            }
        }
    }
    // Match intel_atomic_commit_fence_wait() while also releasing all DRM
    // plane fence references before the old framebuffer state is cleaned up.
    let _ = unsafe {
        tk_drm_upstream::drm_atomic_helper_upstream::drm_atomic_helper_wait_for_fences(
            dev, atomic, false,
        )
    };
    for index in 0..crtc_count {
        let entry = unsafe { (*atomic).crtcs.add(index) };
        if unsafe { !(*entry).ptr.is_null() && !(*entry).new_state.is_null() } {
            unsafe {
                intel_atomic_dsb_finish(state, (*entry).ptr.cast::<IntelCrtc>());
            }
        }
    }
    unsafe {
        tk_drm_upstream::drm_atomic_helper_upstream::drm_atomic_helper_wait_for_dependencies(
            atomic,
        );
        crate::intel_global_state_upstream::intel_atomic_global_state_wait_for_dependencies(state);
    }
    let dc_wakeref = unsafe {
        crate::intel_display_power_upstream::intel_display_power_get(
            display,
            crate::intel_display_power_types_upstream::POWER_DOMAIN_DC_OFF,
        )
    };
    let mut put_domains = unsafe {
        core::mem::MaybeUninit::<
            [crate::intel_display_power_types_upstream::IntelPowerDomainMask;
                I915_MAX_PIPES as usize],
        >::zeroed()
        .assume_init()
    };
    for index in 0..crtc_count.min(I915_MAX_PIPES as usize) {
        let entry = unsafe { (*atomic).crtcs.add(index) };
        if unsafe { (*entry).ptr.is_null() || (*entry).new_state.is_null() } {
            continue;
        }
        let crtc = unsafe { (*entry).ptr.cast::<IntelCrtc>() };
        let new = unsafe { (*entry).new_state.cast::<IntelCrtcState>() };
        if unsafe {
            crate::intel_atomic_upstream::intel_crtc_needs_modeset(new)
                || intel_crtc_needs_fastset(new)
        } {
            unsafe {
                intel_modeset_get_crtc_power_domains(new, &mut put_domains[index]);
            }
        }
        if unsafe { crate::intel_atomic_upstream::intel_crtc_needs_modeset(new) } {
            unsafe { intel_disable_plane_updates(state, crtc) };
            unsafe { crate::intel_crtc_upstream::intel_wait_for_vblank_workers(state) };
            let old = unsafe { (*entry).old_state.cast::<IntelCrtcState>() };
            if !old.is_null() && unsafe { (*old).hw.active } {
                if let Some(disable) = unsafe { (*(*display).modeset.funcs).crtc_disable } {
                    unsafe { disable(state, crtc) };
                }
            }
        }
        unsafe { (*crtc).config = new };
    }

    unsafe { crate::intel_pmdemand_upstream::intel_pmdemand_pre_plane_update(state.cast()) };
    if unsafe { (*state).modeset } {
        unsafe {
            tk_drm_upstream::drm_atomic_helper_upstream::drm_atomic_helper_update_legacy_modeset_state(
                dev, atomic,
            )
        };
    }
    unsafe { crate::intel_cdclk_upstream::intel_set_cdclk_pre_plane_update(state) };
    unsafe { crate::skl_watermark_upstream::intel_sagv_pre_plane_update(state) };
    unsafe { intel_send_disabled_crtc_events(state) };
    unsafe { intel_encoders_update_prepare(state) };
    unsafe { crate::skl_watermark_upstream::intel_dbuf_pre_plane_update(state) };

    if let Some(commit_enables) = unsafe { (*(*display).modeset.funcs).commit_modeset_enables } {
        unsafe { commit_enables(state) };
    }
    unsafe {
        crate::intel_crtc_upstream::intel_wait_for_vblank_workers(state);
        tk_drm_upstream::drm_atomic_helper_upstream::drm_atomic_helper_wait_for_flip_done(
            dev, atomic,
        );
    }
    for index in 0..crtc_count.min(I915_MAX_PIPES as usize) {
        let entry = unsafe { (*atomic).crtcs.add(index) };
        if unsafe { !(*entry).new_state.is_null() } {
            unsafe { intel_atomic_dsb_wait_commit((*entry).new_state.cast::<IntelCrtcState>()) };
        }
    }
    for index in 0..crtc_count.min(I915_MAX_PIPES as usize) {
        let entry = unsafe { (*atomic).crtcs.add(index) };
        if unsafe { (*entry).ptr.is_null() || (*entry).new_state.is_null() } {
            continue;
        }
        let crtc = unsafe { (*entry).ptr.cast::<IntelCrtc>() };
        unsafe {
            crate::intel_wm_upstream::intel_optimize_watermarks(state, crtc);
        }
    }
    unsafe {
        crate::skl_watermark_upstream::intel_dbuf_post_plane_update(state);
        crate::skl_watermark_upstream::intel_sagv_post_plane_update(state);
        crate::intel_cdclk_upstream::intel_set_cdclk_post_plane_update(state);
        crate::intel_pmdemand_upstream::intel_pmdemand_post_plane_update(state.cast());
    }
    for index in 0..crtc_count.min(I915_MAX_PIPES as usize) {
        let entry = unsafe { (*atomic).crtcs.add(index) };
        if unsafe { !(*entry).ptr.is_null() } {
            unsafe {
                intel_modeset_put_crtc_power_domains(
                    (*entry).ptr.cast::<IntelCrtc>(),
                    &mut put_domains[index],
                );
            }
        }
    }
    unsafe {
        tk_drm_upstream::drm_atomic_helper_upstream::drm_atomic_helper_fake_vblank(atomic);
        tk_drm_upstream::drm_atomic_helper_upstream::drm_atomic_helper_commit_hw_done(atomic);
        crate::intel_global_state_upstream::intel_atomic_global_state_commit_done(state);
        tk_drm_upstream::drm_atomic_helper_upstream::drm_atomic_helper_cleanup_planes(dev, atomic);
        for index in 0..crtc_count.min(I915_MAX_PIPES as usize) {
            let entry = (*atomic).crtcs.add(index);
            if !(*entry).new_state.is_null() {
                let new = (*entry).new_state.cast::<IntelCrtcState>();
                intel_atomic_dsb_cleanup(new);
            }
        }
        crate::intel_display_power_upstream::intel_display_power_put(
            display,
            crate::intel_display_power_types_upstream::POWER_DOMAIN_DC_OFF,
            dc_wakeref,
        );
        crate::intel_display_rpm_upstream::intel_display_rpm_put(display, (*state).wakeref);
        (*state).wakeref = ptr::null_mut();
    }
}

unsafe extern "C" fn intel_atomic_commit_work(work: *mut tk_linux_kapi::types::WorkStruct) {
    if work.is_null() {
        return;
    }
    let atomic = unsafe {
        work.cast::<u8>()
            .sub(offset_of!(DrmAtomicCommit, commit_work))
            .cast::<DrmAtomicCommit>()
    };
    unsafe {
        intel_atomic_commit_tail(atomic);
        tk_drm_upstream::drm_atomic_helper_upstream::drm_atomic_helper_commit_cleanup_done(atomic);
        tk_drm_upstream::drm_atomic_upstream::drm_atomic_commit_put(atomic);
    }
}

unsafe fn intel_atomic_track_fbs(state: *mut IntelAtomicState) {
    if state.is_null() || unsafe { (*state).base.dev.is_null() } {
        return;
    }
    let dev = unsafe { (*state).base.dev };
    let count = unsafe { (*dev).mode_config.num_total_plane.max(0) as usize };
    for index in 0..count {
        let entry = unsafe { (*state).base.planes.add(index) };
        if unsafe {
            (*entry).ptr.is_null() || (*entry).old_state.is_null() || (*entry).new_state.is_null()
        } {
            continue;
        }
        let plane = unsafe { (*entry).ptr.cast::<IntelPlane>() };
        let old = unsafe { (*entry).old_state.cast::<IntelPlaneState>() };
        let new = unsafe { (*entry).new_state.cast::<IntelPlaneState>() };
        let old_fb = unsafe {
            (*old)
                .hw
                .fb
                .cast::<crate::intel_display_types_upstream::IntelFramebuffer>()
        };
        let new_fb = unsafe {
            (*new)
                .hw
                .fb
                .cast::<crate::intel_display_types_upstream::IntelFramebuffer>()
        };
        let old_front = if old_fb.is_null() {
            ptr::null_mut()
        } else {
            unsafe { (*old_fb).frontbuffer }
        };
        let new_front = if new_fb.is_null() {
            ptr::null_mut()
        } else {
            unsafe { (*new_fb).frontbuffer }
        };
        unsafe {
            crate::intel_frontbuffer_upstream::intel_frontbuffer_track(
                old_front,
                new_front,
                (*plane).frontbuffer_bit,
            )
        };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_commit(
    dev: *mut DrmDevice,
    atomic: *mut DrmAtomicCommit,
    nonblock: bool,
) -> i32 {
    if dev.is_null() || atomic.is_null() || unsafe { (*atomic).dev != dev } {
        return -EINVAL;
    }
    let display = unsafe { crate::intel_display_conversion_upstream::__drm_to_display(dev) };
    if display.is_null()
        || unsafe { (*display).modeset.funcs.is_null() }
        || !unsafe {
            crate::intel_display_driver_upstream::intel_display_driver_check_access(display)
        }
    {
        return -ENODEV;
    }
    let state = atomic.cast::<IntelAtomicState>();
    unsafe { (*state).wakeref = crate::intel_display_rpm_upstream::intel_display_rpm_get(display) };

    let ret = unsafe {
        tk_drm_upstream::drm_atomic_helper_upstream::drm_atomic_helper_prepare_planes(dev, atomic)
    };
    if ret != 0 {
        return cleanup_after_commit_stage_error(ret, || unsafe {
            crate::intel_display_rpm_upstream::intel_display_rpm_put(display, (*state).wakeref);
            (*state).wakeref = ptr::null_mut();
        });
    }
    let ret = unsafe {
        tk_drm_upstream::drm_atomic_helper_upstream::drm_atomic_helper_setup_commit(
            atomic, nonblock,
        )
    };
    if ret != 0 {
        return cleanup_after_commit_stage_error(ret, || unsafe {
            tk_drm_upstream::drm_atomic_helper_upstream::drm_atomic_helper_unprepare_planes(
                dev, atomic,
            );
            crate::intel_display_rpm_upstream::intel_display_rpm_put(display, (*state).wakeref);
            (*state).wakeref = ptr::null_mut();
        });
    }
    let ret = unsafe {
        crate::intel_global_state_upstream::intel_atomic_global_state_setup_commit(state)
    };
    if ret != 0 {
        return cleanup_after_commit_stage_error(ret, || unsafe {
            tk_drm_upstream::drm_atomic_helper_upstream::drm_atomic_helper_unprepare_planes(
                dev, atomic,
            );
            crate::intel_display_rpm_upstream::intel_display_rpm_put(display, (*state).wakeref);
            (*state).wakeref = ptr::null_mut();
        });
    }
    let ret = unsafe {
        tk_drm_upstream::drm_atomic_helper_upstream::drm_atomic_helper_swap_state(atomic, true)
    };
    if ret != 0 {
        return cleanup_after_commit_stage_error(ret, || unsafe {
            tk_drm_upstream::drm_atomic_helper_upstream::drm_atomic_helper_unprepare_planes(
                dev, atomic,
            );
            crate::intel_display_rpm_upstream::intel_display_rpm_put(display, (*state).wakeref);
            (*state).wakeref = ptr::null_mut();
        });
    }
    unsafe {
        crate::intel_global_state_upstream::intel_atomic_swap_global_state(state);
        crate::intel_dpll_mgr_upstream::intel_dpll_swap_state(state);
        intel_atomic_track_fbs(state);
        tk_drm_upstream::drm_atomic_upstream::drm_atomic_commit_get(atomic);
        tk_linux_kapi::workqueue::INIT_WORK_C(&mut (*atomic).commit_work, intel_atomic_commit_work);
    }
    if nonblock {
        let queue = unsafe {
            if (*state).modeset {
                (*display).wq.modeset
            } else {
                (*display).wq.flip
            }
        };
        if !queue.is_null()
            && unsafe { queue_work(queue, ptr::addr_of_mut!((*atomic).commit_work)) }
        {
            return 0;
        }
    }
    if unsafe { (*state).modeset } {
        let queue = unsafe { (*display).wq.modeset };
        if !queue.is_null() {
            unsafe { flush_workqueue(queue) };
        }
    }
    unsafe { intel_atomic_commit_work(ptr::addr_of_mut!((*atomic).commit_work)) };
    0
}

/// Linux `intel_link_compute_m_n()` with fixed 27-bit data N and 19-bit link
/// N values. Ratios are shifted together until they fit the hardware fields.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_link_compute_m_n(
    bpp_x16: u16,
    nlanes: i32,
    pixel_clock: i32,
    link_clock: i32,
    bw_overhead: i32,
    mn: *mut crate::intel_display_types_upstream::IntelLinkMN,
) {
    if mn.is_null() || nlanes <= 0 || pixel_clock <= 0 || link_clock <= 0 || bpp_x16 == 0 {
        return;
    }
    let link_symbol_size =
        if tk_drm_upstream::drm_dp_helper_upstream::drm_dp_is_uhbr_rate(link_clock) {
            32i64
        } else {
            10i64
        };
    let link_symbol_clock =
        ((i64::from(link_clock) * 10 + link_symbol_size / 2) / link_symbol_size) as u32;
    let data_m = ((i64::from(pixel_clock) * i64::from(bpp_x16) * i64::from(bw_overhead.max(0))
        + (1_000_000i64 * 16 * 8 - 1))
        / (1_000_000i64 * 16 * 8)) as u32;
    let data_n = unsafe {
        tk_drm_upstream::drm_dp_helper_upstream::drm_dp_max_dprx_data_rate(link_clock, nlanes)
    }
    .max(0) as u32;
    let reduce = |numerator: u32, denominator: u32, constant_n: u32| {
        if denominator == 0 {
            return (0, 0);
        }
        let n = if constant_n != 0 {
            constant_n
        } else {
            denominator
                .next_power_of_two()
                .min(crate::intel_display_regs_upstream::DATA_LINK_N_MAX)
        };
        let mut m = ((u64::from(numerator) * u64::from(n)) / u64::from(denominator)) as u32;
        let mut n = n;
        while m > crate::intel_display_regs_upstream::DATA_LINK_M_N_MASK
            || n > crate::intel_display_regs_upstream::DATA_LINK_M_N_MASK
        {
            m >>= 1;
            n >>= 1;
        }
        (m, n)
    };
    let (data_m, data_n) = reduce(data_m, data_n, 0x0800_0000);
    let (link_m, link_n) = reduce(pixel_clock as u32, link_symbol_clock, 0x0008_0000);
    unsafe {
        (*mn).tu = 64;
        (*mn).data_m = data_m;
        (*mn).data_n = data_n;
        (*mn).link_m = link_m;
        (*mn).link_n = link_n;
    }
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
    use std::{
        cell::{Cell, RefCell},
        vec::Vec,
    };

    use tk_linux_kapi::config::{EINVAL, EOPNOTSUPP};

    use super::*;

    fn adlp_mode() -> DrmDisplayMode {
        let mut mode = unsafe { core::mem::MaybeUninit::<DrmDisplayMode>::zeroed().assume_init() };
        mode.clock = 148_500;
        mode.hdisplay = 1_920;
        mode.hsync_start = 2_008;
        mode.hsync_end = 2_052;
        mode.htotal = 2_200;
        mode.vdisplay = 1_080;
        mode.vsync_start = 1_084;
        mode.vsync_end = 1_089;
        mode.vtotal = 1_125;
        mode
    }

    #[test]
    fn generic_mode_validation_checks_adlp_timing_boundaries() {
        let mut mode = adlp_mode();
        assert_eq!(mode_valid_limits(&mode, DISPLAY_VER_ADLP, 300_000), MODE_OK);

        mode.htotal = 16_385;
        assert_eq!(
            mode_valid_limits(&mode, DISPLAY_VER_ADLP, 300_000),
            MODE_H_ILLEGAL
        );
        mode = adlp_mode();
        mode.vtotal = 8_193;
        assert_eq!(
            mode_valid_limits(&mode, DISPLAY_VER_ADLP, 300_000),
            MODE_V_ILLEGAL
        );
        mode = adlp_mode();
        mode.clock = 0;
        assert_eq!(
            mode_valid_limits(&mode, DISPLAY_VER_ADLP, 300_000),
            MODE_NOCLOCK
        );
        mode = adlp_mode();
        mode.htotal = 16_384;
        mode.clock = 250_000;
        assert_eq!(
            mode_valid_limits(&mode, DISPLAY_VER_ADLP, 300_000),
            MODE_H_ILLEGAL
        );
    }

    #[test]
    fn cpu_transcoder_validation_checks_blank_and_sync_boundaries() {
        let mut mode = adlp_mode();
        mode.hdisplay = 64;
        mode.hsync_start = 65;
        mode.htotal = 96;
        mode.vdisplay = 480;
        mode.vtotal = 485;
        assert_eq!(
            cpu_transcoder_mode_valid_limits(&mode, DISPLAY_VER_ADLP, false),
            MODE_OK
        );
        mode.htotal = 95;
        assert_eq!(
            cpu_transcoder_mode_valid_limits(&mode, DISPLAY_VER_ADLP, false),
            MODE_H_ILLEGAL
        );
        mode.htotal = 96;
        mode.vtotal = 484;
        assert_eq!(
            cpu_transcoder_mode_valid_limits(&mode, DISPLAY_VER_ADLP, false),
            MODE_V_ILLEGAL
        );
        mode.vtotal = 485;
        mode.hsync_start = mode.hdisplay;
        assert_eq!(
            cpu_transcoder_mode_valid_limits(&mode, DISPLAY_VER_ADLP, false),
            MODE_H_ILLEGAL
        );
    }

    #[test]
    fn atomic_config_retry_only_clamps_a_failed_pipe_on_einval() {
        let called = Cell::new(0);
        assert!(retry_failed_config_with_bpp_limit(-EINVAL, 2, || {
            called.set(called.get() + 1);
            true
        }));
        assert_eq!(called.get(), 1);
        assert!(!retry_failed_config_with_bpp_limit(-EINVAL, -1, || {
            called.set(called.get() + 1);
            true
        }));
        assert!(!retry_failed_config_with_bpp_limit(-ENOMEM, 2, || {
            called.set(called.get() + 1);
            true
        }));
        assert_eq!(called.get(), 1);
    }

    #[test]
    fn commit_stage_failure_runs_rollback_once_and_success_does_not() {
        let cleaned = Cell::new(0);
        assert_eq!(
            cleanup_after_commit_stage_error(-ENOMEM, || cleaned.set(cleaned.get() + 1)),
            -ENOMEM
        );
        assert_eq!(cleaned.get(), 1);
        assert_eq!(
            cleanup_after_commit_stage_error(0, || cleaned.set(cleaned.get() + 1)),
            0
        );
        assert_eq!(cleaned.get(), 1);
    }

    #[test]
    fn adlp_port_phy_mapping_matches_ddi_topology() {
        assert_eq!(adlp_port_to_phy(0), PHY_A);
        assert_eq!(adlp_port_to_phy(1), PHY_B);
        assert_eq!(adlp_port_to_phy(2), PHY_A + 2);
        assert_eq!(adlp_port_to_phy(3), PHY_F);
        assert_eq!(adlp_port_to_phy(4), PHY_F + 1);
        assert_eq!(adlp_port_to_phy(5), PHY_F + 2);
        assert_eq!(adlp_port_to_phy(6), PHY_I);
        assert_eq!(adlp_port_to_phy(7), PHY_D);
        assert_eq!(adlp_port_to_phy(8), PHY_D + 1);
        assert_eq!(adlp_port_to_phy(-1), PHY_NONE);
        assert_eq!(adlp_port_to_phy(9), PHY_NONE);
    }

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
