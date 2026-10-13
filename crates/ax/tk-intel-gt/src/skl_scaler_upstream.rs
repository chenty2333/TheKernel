// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! ADL-P-reachable scaler paths from Linux v7.2.3 `skl_scaler.c`.
//!
//! The scaler allocator, range policy, register field calculations and DSB
//! write order follow the source. Display-20 CASF and pre-SKL scaler modes
//! are not selected by the configured display-13 target.
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::{ffi::c_void, mem::offset_of};

use tk_drm_upstream::{
    drm_fourcc_types::DrmFormatInfo,
    drm_modes_types::{DrmDisplayMode, MODE_NO_420, MODE_OK},
    drm_plane_types::{DRM_SCALING_FILTER_DEFAULT, DRM_SCALING_FILTER_NEAREST_NEIGHBOR},
    drm_rect_types::DrmRect,
};

use crate::{
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_limits_types_upstream::{Pipe, PlaneId},
    intel_display_reg_defs_upstream::reg_field_get,
    intel_display_regs_upstream::*,
    intel_display_types_upstream::{
        INTEL_OUTPUT_FORMAT_YCBCR420, IntelAtomicState, IntelCrtc, IntelCrtcScalerState,
        IntelCrtcState, IntelOutputFormat, IntelPlane, IntelPlaneState,
    },
    intel_display_wa_upstream::{__intel_display_wa, DISPLAY_VER, IntelDisplayWa},
    intel_workarounds_types_upstream::I915RegT,
};

const SKL_CRTC_INDEX: u32 = 31;
const DRM_MODE_FLAG_INTERLACE: u32 = 1 << 4;
const DRM_PLANE_NO_SCALING: i32 = 1 << 16;
const EINVAL: i32 = tk_linux_kapi::config::EINVAL;

unsafe extern "C" {
    fn intel_atomic_get_new_crtc_state(
        state: *mut IntelAtomicState,
        crtc: *mut IntelCrtc,
    ) -> *mut IntelCrtcState;
    fn intel_atomic_get_new_plane_state(
        state: *mut IntelAtomicState,
        plane: *mut IntelPlane,
    ) -> *mut IntelPlaneState;
    fn intel_atomic_get_plane_state(
        state: *mut IntelAtomicState,
        plane: *mut IntelPlane,
    ) -> *mut IntelPlaneState;
    fn drm_plane_from_index(
        dev: *mut tk_drm_upstream::drm_device_types::DrmDevice,
        index: i32,
    ) -> *mut tk_drm_upstream::drm_plane_types::DrmPlane;
    fn drm_plane_index(plane: *mut tk_drm_upstream::drm_plane_types::DrmPlane) -> u32;
    fn drm_rect_calc_hscale(src: *const DrmRect, dst: *const DrmRect, min: i32, max: i32) -> i32;
    fn drm_rect_calc_vscale(src: *const DrmRect, dst: *const DrmRect, min: i32, max: i32) -> i32;
    fn intel_format_info_is_yuv_semiplanar(info: *const DrmFormatInfo, modifier: u64) -> bool;
    fn icl_is_hdr_plane(display: *mut IntelDisplay, plane_id: PlaneId) -> bool;
    fn intel_de_read(display: *mut IntelDisplay, reg: I915RegT) -> u32;
    fn intel_de_write(display: *mut IntelDisplay, reg: I915RegT, val: u32);
    fn intel_de_write_fw(display: *mut IntelDisplay, reg: I915RegT, val: u32);
    fn intel_de_write_dsb(display: *mut IntelDisplay, dsb: *mut c_void, reg: I915RegT, val: u32);
    fn skl_plane_wm_level(
        pipe_wm: *const crate::intel_display_types_upstream::SklPipeWm,
        plane_id: PlaneId,
        level: i32,
    ) -> *const crate::intel_display_types_upstream::SklWmLevel;
    fn skl_plane_trans_wm(
        pipe_wm: *const crate::intel_display_types_upstream::SklPipeWm,
        plane_id: PlaneId,
    ) -> *const crate::intel_display_types_upstream::SklWmLevel;
}

#[inline]
unsafe fn display_from_crtc(crtc: *mut IntelCrtc) -> *mut IntelDisplay {
    if crtc.is_null() || unsafe { (*crtc).base.dev.is_null() } {
        return core::ptr::null_mut();
    }
    let drm = unsafe { (*crtc).base.dev };
    if unsafe { (*drm).dev_private.is_null() } {
        return core::ptr::null_mut();
    }
    let i915 = unsafe {
        (*drm)
            .dev_private
            .cast::<crate::linux_i915_private::DrmI915Private>()
    };
    unsafe { (*i915).display.cast::<IntelDisplay>() }
}

#[inline]
unsafe fn display_from_crtc_state(state: *const IntelCrtcState) -> *mut IntelDisplay {
    if state.is_null() || unsafe { (*state).uapi.crtc.is_null() } {
        return core::ptr::null_mut();
    }
    unsafe { display_from_crtc((*state).uapi.crtc.cast::<IntelCrtc>()) }
}

#[inline]
unsafe fn intel_plane_from_drm(
    plane: *mut tk_drm_upstream::drm_plane_types::DrmPlane,
) -> *mut IntelPlane {
    if plane.is_null() {
        return core::ptr::null_mut();
    }
    unsafe {
        (plane as *mut u8)
            .sub(offset_of!(IntelPlane, base))
            .cast::<IntelPlane>()
    }
}

#[inline]
fn mask_for_user(user: u32) -> Option<u32> {
    (user < 32).then(|| 1u32 << user)
}

#[inline]
fn min_src_size(is_semiplanar: bool) -> (i32, i32) {
    if is_semiplanar { (16, 16) } else { (8, 8) }
}

#[inline]
fn max_src_size(display_ver: u8) -> (i32, i32) {
    if display_ver >= 14 {
        (4096, 8192)
    } else if display_ver >= 12 {
        (5120, 8192)
    } else if display_ver == 11 {
        (5120, 4096)
    } else {
        (4096, 4096)
    }
}

#[inline]
fn max_dst_size(display_ver: u8) -> (i32, i32) {
    if display_ver >= 12 {
        (8192, 8192)
    } else if display_ver == 11 {
        (5120, 4096)
    } else {
        (4096, 4096)
    }
}

fn scaler_request_in_range(
    display_ver: u8,
    pipe_src_w: i32,
    pipe_src_h: i32,
    src_w: i32,
    src_h: i32,
    dst_w: i32,
    dst_h: i32,
    is_semiplanar: bool,
) -> bool {
    let (min_src_w, min_src_h) = min_src_size(is_semiplanar);
    let (max_src_w, max_src_h) = max_src_size(display_ver);
    let (max_dst_w, max_dst_h) = max_dst_size(display_ver);
    src_w >= min_src_w
        && src_h >= min_src_h
        && dst_w >= 8
        && dst_h >= 8
        && src_w <= max_src_w
        && src_h <= max_src_h
        && dst_w <= max_dst_w
        && dst_h <= max_dst_h
        && pipe_src_w <= max_dst_w
        && pipe_src_h <= max_dst_h
}

/// Stage a scaler request or release the scaler currently owned by `user`.
unsafe fn update_scaler(
    crtc_state: *mut IntelCrtcState,
    force_detach: bool,
    user: u32,
    scaler_id: *mut i32,
    src_w: i32,
    src_h: i32,
    dst_w: i32,
    dst_h: i32,
    format: *const DrmFormatInfo,
    modifier: u64,
    need_scaler: bool,
) -> i32 {
    if crtc_state.is_null() || scaler_id.is_null() {
        return -EINVAL;
    }
    let Some(user_mask) = mask_for_user(user) else {
        return -EINVAL;
    };
    let display = unsafe { display_from_crtc_state(crtc_state) };
    if display.is_null() {
        return -EINVAL;
    }
    let scaler_state = unsafe { &mut (*crtc_state).scaler_state };

    let need_scaler = need_scaler || src_w != dst_w || src_h != dst_h;

    if unsafe { DISPLAY_VER(display) >= 9 }
        && unsafe { (*crtc_state).hw.enable }
        && need_scaler
        && unsafe { (*crtc_state).hw.adjusted_mode.flags & DRM_MODE_FLAG_INTERLACE != 0 }
    {
        return -EINVAL;
    }

    if force_detach || !need_scaler {
        let id = unsafe { *scaler_id };
        if id >= 0 {
            if (id as usize) < scaler_state.scalers.len() {
                scaler_state.scalers[id as usize].in_use = false;
            }
            scaler_state.scaler_users &= !user_mask;
            unsafe { *scaler_id = -1 };
        }
        return 0;
    }

    let semiplanar =
        !format.is_null() && unsafe { intel_format_info_is_yuv_semiplanar(format, modifier) };
    let pipe_src_w = unsafe { (*crtc_state).pipe_src.x2 - (*crtc_state).pipe_src.x1 };
    let pipe_src_h = unsafe { (*crtc_state).pipe_src.y2 - (*crtc_state).pipe_src.y1 };
    if !scaler_request_in_range(
        unsafe { DISPLAY_VER(display) },
        pipe_src_w,
        pipe_src_h,
        src_w,
        src_h,
        dst_w,
        dst_h,
        semiplanar,
    ) {
        return -EINVAL;
    }

    scaler_state.scaler_users |= user_mask;
    0
}

/// Linux `skl_update_scaler_crtc()`. The display-13 path has no CASF hook;
/// CRTC scaling is only staged when the pipe fitter is explicitly requested.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_update_scaler_crtc(crtc_state: *mut IntelCrtcState) -> i32 {
    if crtc_state.is_null() {
        return -EINVAL;
    }
    let state = unsafe { &mut *crtc_state };
    let mode = &state.hw.pipe_mode;
    let (width, height) = if state.pch_pfit.enabled {
        (
            state.pch_pfit.dst.x2 - state.pch_pfit.dst.x1,
            state.pch_pfit.dst.y2 - state.pch_pfit.dst.y1,
        )
    } else {
        (mode.crtc_hdisplay as i32, mode.crtc_vdisplay as i32)
    };
    unsafe {
        update_scaler(
            crtc_state,
            !state.hw.active,
            SKL_CRTC_INDEX,
            &mut state.scaler_state.scaler_id,
            state.pipe_src.x2 - state.pipe_src.x1,
            state.pipe_src.y2 - state.pipe_src.y1,
            width,
            height,
            core::ptr::null(),
            0,
            state.pch_pfit.enabled,
        )
    }
}

/// Linux `skl_update_scaler_plane()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_update_scaler_plane(
    crtc_state: *mut IntelCrtcState,
    plane_state: *mut IntelPlaneState,
) -> i32 {
    if crtc_state.is_null() || plane_state.is_null() {
        return -EINVAL;
    }
    let state = unsafe { &mut *plane_state };
    let plane = unsafe { intel_plane_from_drm(state.uapi.plane) };
    if plane.is_null() {
        return -EINVAL;
    }
    let fb = state.hw.fb;
    let visible = state.uapi.visible;
    let format = if fb.is_null() {
        core::ptr::null()
    } else {
        unsafe { (*fb).format }
    };
    let modifier = if fb.is_null() {
        0
    } else {
        unsafe { (*fb).modifier }
    };
    let mut need_scaler = false;
    if !format.is_null() {
        let display = unsafe { display_from_crtc_state(crtc_state) };
        let hdr_plane = !display.is_null() && unsafe { icl_is_hdr_plane(display, (*plane).id) };
        if !hdr_plane && unsafe { intel_format_info_is_yuv_semiplanar(format, modifier) } {
            need_scaler = true;
        }
    }
    unsafe {
        update_scaler(
            crtc_state,
            fb.is_null() || !visible,
            state
                .uapi
                .plane
                .as_ref()
                .map_or(0, |_| drm_plane_index(state.uapi.plane)),
            &mut state.scaler_id,
            (state.uapi.src.x2 - state.uapi.src.x1) >> 16,
            (state.uapi.src.y2 - state.uapi.src.y1) >> 16,
            state.uapi.dst.x2 - state.uapi.dst.x1,
            state.uapi.dst.y2 - state.uapi.dst.y1,
            format,
            modifier,
            need_scaler,
        )
    }
}

#[inline]
fn has_casf(display: *mut IntelDisplay, scaler_id: i32) -> bool {
    !display.is_null() && unsafe { DISPLAY_VER(display) >= 20 } && scaler_id == 1
}

fn allocate_scaler(
    scaler_state: &mut IntelCrtcScalerState,
    crtc: &IntelCrtc,
    display_ver: u8,
    casf_scaler: bool,
) -> i32 {
    let max = (crtc.num_scalers as usize).min(scaler_state.scalers.len());
    let used = scaler_state
        .scalers
        .iter()
        .enumerate()
        .fold(0u32, |mask, (id, scaler)| {
            mask | (u32::from(scaler.in_use) << id)
        });
    let Some(id) = scaler_candidate(used, max, display_ver, casf_scaler) else {
        return -1;
    };
    scaler_state.scalers[id].in_use = true;
    id as i32
}

fn scaler_candidate(
    used: u32,
    num_scalers: usize,
    display_ver: u8,
    casf_scaler: bool,
) -> Option<usize> {
    for id in 0..num_scalers.min(2) {
        if used & (1 << id) != 0 {
            continue;
        }
        if casf_scaler && !(display_ver >= 20 && id == 1) {
            continue;
        }
        return Some(id);
    }
    None
}

#[inline]
fn calculate_max_scale(display_ver: u8, is_yuv_semiplanar: bool, scaler_id: i32) -> (i32, i32) {
    if display_ver >= 14 {
        (
            0x30000 - 1,
            if scaler_id == 0 { 0x30000 - 1 } else { 0x10000 },
        )
    } else if display_ver >= 10 || !is_yuv_semiplanar {
        (0x30000 - 1, 0x30000 - 1)
    } else {
        (0x20000 - 1, 0x20000 - 1)
    }
}

unsafe fn setup_scaler(
    crtc_state: *mut IntelCrtcState,
    num_scalers_need: u32,
    crtc: *mut IntelCrtc,
    plane_state: *mut IntelPlaneState,
    scaler_id: *mut i32,
    casf_scaler: bool,
) -> i32 {
    if crtc_state.is_null() || crtc.is_null() || scaler_id.is_null() {
        return -EINVAL;
    }
    let display = unsafe { display_from_crtc(crtc) };
    if display.is_null() {
        return -EINVAL;
    }
    let scaler_state = unsafe { &mut (*crtc_state).scaler_state };
    if unsafe { *scaler_id < 0 } {
        unsafe {
            *scaler_id = allocate_scaler(scaler_state, &*crtc, DISPLAY_VER(display), casf_scaler);
        }
    }
    let id = unsafe { *scaler_id };
    if id < 0 || (id as usize) >= scaler_state.scalers.len() {
        return -EINVAL;
    }

    let mut mode: u32;
    let mut hscale = 0;
    let mut vscale = 0;
    if !plane_state.is_null() && unsafe { !(*plane_state).hw.fb.is_null() } {
        let fb = unsafe { (*plane_state).hw.fb };
        let format = unsafe { (*fb).format };
        let plane = unsafe { (*plane_state).uapi.plane.cast::<IntelPlane>() };
        if !format.is_null() && unsafe { (*format).is_yuv && (*format).num_planes > 1 } {
            if unsafe { DISPLAY_VER(display) == 9 } {
                mode = SKL_PS_SCALER_MODE_NV12;
            } else if unsafe { icl_is_hdr_plane(display, (*plane).id) } {
                mode = PS_SCALER_MODE_NORMAL;
            } else {
                mode = PS_SCALER_MODE_PLANAR;
                let linked = unsafe { (*plane_state).planar_linked_plane };
                if !linked.is_null() {
                    mode |= PS_BINDING_Y_PLANE(unsafe { (*linked).id });
                }
            }
        } else if unsafe { DISPLAY_VER(display) >= 10 } {
            mode = PS_SCALER_MODE_NORMAL;
        } else if num_scalers_need == 1 && unsafe { (*crtc).num_scalers > 1 } {
            scaler_state.scalers[id as usize].in_use = false;
            unsafe { *scaler_id = 0 };
            scaler_state.scalers[0].in_use = true;
            mode = SKL_PS_SCALER_MODE_HQ;
        } else {
            mode = SKL_PS_SCALER_MODE_DYN;
        }

        let semiplanar = unsafe { intel_format_info_is_yuv_semiplanar(format, (*fb).modifier) };
        let (max_hscale, max_vscale) =
            calculate_max_scale(unsafe { DISPLAY_VER(display) }, semiplanar, id);
        let src = unsafe { &(*plane_state).uapi.src };
        let dst = unsafe { &(*plane_state).uapi.dst };
        hscale = unsafe { drm_rect_calc_hscale(src, dst, 1, max_hscale) };
        vscale = unsafe { drm_rect_calc_vscale(src, dst, 1, max_vscale) };
        if hscale < 0 || vscale < 0 {
            return -EINVAL;
        }
        let _ = semiplanar;
    } else if unsafe { DISPLAY_VER(display) >= 10 } {
        mode = PS_SCALER_MODE_NORMAL;
    } else if num_scalers_need == 1 && unsafe { (*crtc).num_scalers > 1 } {
        let id = unsafe { *scaler_id };
        scaler_state.scalers[id as usize].in_use = false;
        unsafe { *scaler_id = 0 };
        scaler_state.scalers[0].in_use = true;
        mode = SKL_PS_SCALER_MODE_HQ;
    } else {
        mode = SKL_PS_SCALER_MODE_DYN;
    }

    if unsafe { (*crtc_state).pch_pfit.enabled } {
        let pipe_src = unsafe { &(*crtc_state).pipe_src };
        let src = DrmRect {
            x1: 0,
            y1: 0,
            x2: (pipe_src.x2 - pipe_src.x1).wrapping_shl(16),
            y2: (pipe_src.y2 - pipe_src.y1).wrapping_shl(16),
        };
        let dst = unsafe { &(*crtc_state).pch_pfit.dst };
        let (mut max_hscale, mut max_vscale) =
            calculate_max_scale(unsafe { DISPLAY_VER(display) }, false, unsafe {
                *scaler_id
            });
        if unsafe { (*crtc_state).output_format == INTEL_OUTPUT_FORMAT_YCBCR420 } {
            max_hscale = 0x18000 - 1;
            max_vscale = 0x10000;
        }
        hscale = unsafe { drm_rect_calc_hscale(&src, dst, 0, max_hscale) };
        vscale = unsafe { drm_rect_calc_vscale(&src, dst, 0, max_vscale) };
        if hscale < 0 || vscale < 0 {
            return -EINVAL;
        }
    }

    scaler_state.scalers[unsafe { *scaler_id } as usize].hscale = hscale;
    scaler_state.scalers[unsafe { *scaler_id } as usize].vscale = vscale;
    scaler_state.scalers[unsafe { *scaler_id } as usize].mode = mode;
    0
}

unsafe fn setup_crtc_scaler(state: *mut IntelAtomicState, crtc: *mut IntelCrtc) -> i32 {
    let crtc_state = unsafe { intel_atomic_get_new_crtc_state(state, crtc) };
    if crtc_state.is_null() {
        return -EINVAL;
    }
    let users = unsafe { (*crtc_state).scaler_state.scaler_users.count_ones() };
    unsafe {
        setup_scaler(
            crtc_state,
            users,
            crtc,
            core::ptr::null_mut(),
            &mut (*crtc_state).scaler_state.scaler_id,
            (*crtc_state).pch_pfit.casf.enable,
        )
    }
}

unsafe fn setup_plane_scaler(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
    plane: *mut IntelPlane,
) -> i32 {
    if state.is_null() || crtc.is_null() || plane.is_null() {
        return -EINVAL;
    }
    let display = unsafe { display_from_crtc(crtc) };
    if display.is_null() {
        return -EINVAL;
    }
    if unsafe { (*plane).pipe != (*crtc).pipe } {
        return 0;
    }
    let mut plane_state = unsafe { intel_atomic_get_new_plane_state(state, plane) };
    if plane_state.is_null() && unsafe { DISPLAY_VER(display) >= 10 } {
        return 0;
    }
    plane_state = unsafe { intel_atomic_get_plane_state(state, plane) };
    if tk_linux_kapi::config::IS_ERR(plane_state) {
        return tk_linux_kapi::config::PTR_ERR(plane_state);
    }
    if plane_state.is_null() {
        return -EINVAL;
    }
    let crtc_state = unsafe { intel_atomic_get_new_crtc_state(state, crtc) };
    if crtc_state.is_null() {
        return -EINVAL;
    }
    let users = unsafe { (*crtc_state).scaler_state.scaler_users.count_ones() };
    unsafe {
        setup_scaler(
            crtc_state,
            users,
            crtc,
            plane_state,
            &mut (*plane_state).scaler_id,
            false,
        )
    }
}

/// Assign staged plane/pipe scaling requests to the finite per-pipe scaler
/// pool. User bit 31 is reserved for the CRTC panel fitter, per Linux.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_setup_scalers(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
) -> i32 {
    if state.is_null() || crtc.is_null() {
        return -EINVAL;
    }
    let crtc_state = unsafe { intel_atomic_get_new_crtc_state(state, crtc) };
    if crtc_state.is_null() {
        return -EINVAL;
    }
    let display = unsafe { display_from_crtc(crtc) };
    if display.is_null() {
        return -EINVAL;
    }
    let users = unsafe { (*crtc_state).scaler_state.scaler_users };
    let count = users.count_ones();
    if count > unsafe { (*crtc).num_scalers.max(0) as u32 } {
        return -EINVAL;
    }
    for user in 0..32u32 {
        if users & (1u32 << user) == 0 {
            continue;
        }
        if user == SKL_CRTC_INDEX {
            let ret = unsafe { setup_crtc_scaler(state, crtc) };
            if ret != 0 {
                return ret;
            }
        } else {
            let drm_plane = unsafe { drm_plane_from_index((*display).drm, user as i32) };
            if drm_plane.is_null() {
                return -EINVAL;
            }
            let plane = unsafe { intel_plane_from_drm(drm_plane) };
            let ret = unsafe { setup_plane_scaler(state, crtc, plane) };
            if ret != 0 {
                return ret;
            }
        }
    }
    0
}

/// Linux scaler initial phase in signed Q2.14 form (plus phase-trip bit).
fn scaler_calc_phase(sub: i32, scale: i32, chroma_cosited: bool) -> u16 {
    if sub <= 0 {
        return 0;
    }
    let mut phase = -0x8000i32;
    let mut trip = 0u16;
    if chroma_cosited {
        phase += (sub - 1) * 0x8000 / sub;
    }
    phase += scale / (2 * sub);
    if phase < 0 {
        phase = 0x10000 + phase;
    } else {
        trip = PS_PHASE_TRIP as u16;
    }
    (((phase >> 2) as u32 & PS_PHASE_MASK) as u16) | trip
}

#[inline]
fn coef_tap(index: i32) -> i32 {
    index % 7
}

#[inline]
fn nearest_filter_coef(tap: i32) -> u16 {
    if tap == 3 { 0x0800 } else { 0x3000 }
}

unsafe fn program_nearest_filter_coefs(
    display: *mut IntelDisplay,
    dsb: *mut c_void,
    pipe: Pipe,
    scaler_id: i32,
    set: i32,
) {
    unsafe {
        intel_de_write_dsb(
            display,
            dsb,
            GLK_PS_COEF_INDEX_SET(pipe as u32, scaler_id as u32, set as u32),
            PS_COEF_INDEX_AUTO_INC,
        );
    }
    for i in (0..17 * 7).step_by(2) {
        let lo = nearest_filter_coef(coef_tap(i));
        let hi = nearest_filter_coef(coef_tap(i + 1));
        unsafe {
            intel_de_write_dsb(
                display,
                dsb,
                GLK_PS_COEF_DATA_SET(pipe as u32, scaler_id as u32, set as u32),
                u32::from(lo) | (u32::from(hi) << 16),
            );
        }
    }
    unsafe {
        intel_de_write_dsb(
            display,
            dsb,
            GLK_PS_COEF_INDEX_SET(pipe as u32, scaler_id as u32, set as u32),
            0,
        );
    }
}

#[inline]
fn filter_select(filter: u32, casf: bool) -> u32 {
    if filter == DRM_SCALING_FILTER_NEAREST_NEIGHBOR || casf {
        PS_FILTER_PROGRAMMED
            | PS_Y_VERT_FILTER_SELECT(0)
            | PS_Y_HORZ_FILTER_SELECT(0)
            | PS_UV_VERT_FILTER_SELECT(0)
            | PS_UV_HORZ_FILTER_SELECT(0)
    } else {
        PS_FILTER_MEDIUM
    }
}

unsafe fn setup_filter(
    display: *mut IntelDisplay,
    dsb: *mut c_void,
    pipe: Pipe,
    scaler_id: i32,
    set: i32,
    filter: u32,
) {
    if filter == DRM_SCALING_FILTER_NEAREST_NEIGHBOR {
        unsafe { program_nearest_filter_coefs(display, dsb, pipe, scaler_id, set) };
    } else if filter != DRM_SCALING_FILTER_DEFAULT {
        // Linux MISSING_CASE(filter); no write is performed for unknown filters.
    }
}

/// Program the pipe fitter when the staged CRTC state requested it. This is
/// retained for source-equivalent ordering even though ADL-P bring-up does
/// not enable the legacy panel fitter.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_pfit_enable(crtc_state: *const IntelCrtcState) {
    if crtc_state.is_null() {
        return;
    }
    let state = unsafe { &*crtc_state };
    if !state.pch_pfit.enabled || state.uapi.crtc.is_null() {
        return;
    }
    let crtc = state.uapi.crtc.cast::<IntelCrtc>();
    let display = unsafe { display_from_crtc(crtc) };
    if display.is_null() || state.scaler_state.scaler_id < 0 {
        return;
    }
    let scaler_id = state.scaler_state.scaler_id as usize;
    if scaler_id >= state.scaler_state.scalers.len() {
        return;
    }
    let dst = &state.pch_pfit.dst;
    let width = dst.x2 - dst.x1;
    let height = dst.y2 - dst.y1;
    let src = DrmRect {
        x1: 0,
        y1: 0,
        x2: (state.pipe_src.x2 - state.pipe_src.x1).saturating_mul(1 << 16),
        y2: (state.pipe_src.y2 - state.pipe_src.y1).saturating_mul(1 << 16),
    };
    let hscale = unsafe { drm_rect_calc_hscale(&src, dst, 0, i32::MAX) };
    let vscale = unsafe { drm_rect_calc_vscale(&src, dst, 0, i32::MAX) };
    let hphase = scaler_calc_phase(1, hscale, false);
    let vphase = scaler_calc_phase(1, vscale, false);
    let id = scaler_id as u32;
    let pipe = unsafe { (*crtc).pipe as u32 };
    let filter = filter_select(state.hw.scaling_filter, state.pch_pfit.casf.enable);
    let ps_ctrl =
        PS_SCALER_EN | PS_BINDING_PIPE | state.scaler_state.scalers[scaler_id].mode | filter;

    unsafe {
        if __intel_display_wa(display, IntelDisplayWa::Wa14011503117, core::ptr::null()) {
            adl_scaler_ecc_mask(crtc_state);
        }
        setup_filter(
            display,
            core::ptr::null_mut(),
            pipe as Pipe,
            id as i32,
            0,
            state.hw.scaling_filter,
        );
        intel_de_write_fw(display, SKL_PS_CTRL(pipe, id), ps_ctrl);
        intel_de_write_fw(
            display,
            SKL_PS_VPHASE(pipe, id),
            PS_Y_PHASE(0) | PS_UV_RGB_PHASE(vphase as u32),
        );
        intel_de_write_fw(
            display,
            SKL_PS_HPHASE(pipe, id),
            PS_Y_PHASE(0) | PS_UV_RGB_PHASE(hphase as u32),
        );
        intel_de_write_fw(
            display,
            SKL_PS_WIN_POS(pipe, id),
            PS_WIN_XPOS(dst.x1 as u32) | PS_WIN_YPOS(dst.y1 as u32),
        );
        intel_de_write_fw(
            display,
            SKL_PS_WIN_SZ(pipe, id),
            PS_WIN_XSIZE(width as u32) | PS_WIN_YSIZE(height as u32),
        );
    }
}

unsafe fn pipe_scaler_hw_state(crtc_state: *mut IntelCrtcState) -> i32 {
    if crtc_state.is_null() || unsafe { (*crtc_state).uapi.crtc.is_null() } {
        return -1;
    }
    let crtc = unsafe { (*crtc_state).uapi.crtc.cast::<IntelCrtc>() };
    let display = unsafe { display_from_crtc(crtc) };
    if display.is_null() {
        return -1;
    }
    for id in 0..unsafe { (*crtc).num_scalers.max(0).min(2) } {
        let ctl = unsafe { intel_de_read(display, SKL_PS_CTRL((*crtc).pipe as u32, id as u32)) };
        if ctl & (PS_SCALER_EN | PS_BINDING_MASK) == (PS_SCALER_EN | PS_BINDING_PIPE) {
            return id;
        }
    }
    -1
}

/// Program the assigned plane scaler, preserving Linux's filter/phase/window
/// register order. The call is normally appended to the plane's DSB update.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_program_plane_scaler(
    dsb: *mut c_void,
    plane: *mut IntelPlane,
    crtc_state: *const IntelCrtcState,
    plane_state: *const IntelPlaneState,
) {
    if plane.is_null() || crtc_state.is_null() || plane_state.is_null() {
        return;
    }
    let p = unsafe { &*plane };
    let state = unsafe { &*plane_state };
    let crtc = unsafe { (*crtc_state).uapi.crtc.cast::<IntelCrtc>() };
    let display = unsafe { display_from_crtc(crtc) };
    if display.is_null() || state.scaler_id < 0 || state.scaler_id >= 2 || state.hw.fb.is_null() {
        return;
    }
    let fb = unsafe { &*state.hw.fb };
    let format = fb.format;
    let src = &state.uapi.src;
    let dst = &state.uapi.dst;
    let hscale = unsafe { drm_rect_calc_hscale(src, dst, 0, i32::MAX) };
    let vscale = unsafe { drm_rect_calc_vscale(src, dst, 0, i32::MAX) };
    let semiplanar = !format.is_null()
        && unsafe { intel_format_info_is_yuv_semiplanar(format, fb.modifier) }
        && !unsafe { icl_is_hdr_plane(display, p.id) };
    let (y_hphase, y_vphase, uv_hphase, uv_vphase) = if semiplanar {
        (
            scaler_calc_phase(1, hscale, false),
            scaler_calc_phase(1, vscale, false),
            scaler_calc_phase(2, hscale, true),
            scaler_calc_phase(2, vscale, false),
        )
    } else {
        (
            0,
            0,
            scaler_calc_phase(1, hscale, false),
            scaler_calc_phase(1, vscale, false),
        )
    };
    let scaler = unsafe { &(*crtc_state).scaler_state.scalers[state.scaler_id as usize] };
    let pipe = p.pipe as u32;
    let id = state.scaler_id as u32;
    let width = dst.x2 - dst.x1;
    let height = dst.y2 - dst.y1;
    let ps_ctrl = PS_SCALER_EN
        | PS_BINDING_PLANE(p.id)
        | scaler.mode
        | filter_select(state.hw.scaling_filter, false);
    unsafe {
        setup_filter(
            display,
            dsb,
            p.pipe,
            state.scaler_id,
            0,
            state.hw.scaling_filter,
        );
        intel_de_write_dsb(display, dsb, SKL_PS_CTRL(pipe, id), ps_ctrl);
        intel_de_write_dsb(
            display,
            dsb,
            SKL_PS_VPHASE(pipe, id),
            PS_Y_PHASE(y_vphase as u32) | PS_UV_RGB_PHASE(uv_vphase as u32),
        );
        intel_de_write_dsb(
            display,
            dsb,
            SKL_PS_HPHASE(pipe, id),
            PS_Y_PHASE(y_hphase as u32) | PS_UV_RGB_PHASE(uv_hphase as u32),
        );
        intel_de_write_dsb(
            display,
            dsb,
            SKL_PS_WIN_POS(pipe, id),
            PS_WIN_XPOS(dst.x1 as u32) | PS_WIN_YPOS(dst.y1 as u32),
        );
        intel_de_write_dsb(
            display,
            dsb,
            SKL_PS_WIN_SZ(pipe, id),
            PS_WIN_XSIZE(width as u32) | PS_WIN_YSIZE(height as u32),
        );
    }
}

unsafe fn detach_scaler(dsb: *mut c_void, crtc: *mut IntelCrtc, id: i32) {
    if crtc.is_null() || id < 0 || id >= 2 {
        return;
    }
    let display = unsafe { display_from_crtc(crtc) };
    if display.is_null() {
        return;
    }
    let pipe = unsafe { (*crtc).pipe as u32 };
    unsafe {
        intel_de_write_dsb(display, dsb, SKL_PS_CTRL(pipe, id as u32), 0);
        intel_de_write_dsb(display, dsb, SKL_PS_WIN_POS(pipe, id as u32), 0);
        intel_de_write_dsb(display, dsb, SKL_PS_WIN_SZ(pipe, id as u32), 0);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_detach_scalers(dsb: *mut c_void, crtc_state: *const IntelCrtcState) {
    if crtc_state.is_null() || unsafe { (*crtc_state).uapi.crtc.is_null() } {
        return;
    }
    let crtc = unsafe { (*crtc_state).uapi.crtc.cast::<IntelCrtc>() };
    for id in 0..unsafe { (*crtc).num_scalers.max(0).min(2) } {
        if !unsafe { (*crtc_state).scaler_state.scalers[id as usize].in_use } {
            unsafe { detach_scaler(dsb, crtc, id) };
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_scaler_disable(old_crtc_state: *const IntelCrtcState) {
    if old_crtc_state.is_null() || unsafe { (*old_crtc_state).uapi.crtc.is_null() } {
        return;
    }
    let crtc = unsafe { (*old_crtc_state).uapi.crtc.cast::<IntelCrtc>() };
    for id in 0..unsafe { (*crtc).num_scalers.max(0).min(2) } {
        unsafe { detach_scaler(core::ptr::null_mut(), crtc, id) };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_scaler_get_config(crtc_state: *mut IntelCrtcState) {
    if crtc_state.is_null() || unsafe { (*crtc_state).uapi.crtc.is_null() } {
        return;
    }
    let crtc = unsafe { (*crtc_state).uapi.crtc.cast::<IntelCrtc>() };
    let scaler_id = unsafe { pipe_scaler_hw_state(crtc_state) };
    if scaler_id < 0 || scaler_id >= 2 {
        return;
    }
    let display = unsafe { display_from_crtc(crtc) };
    if display.is_null() {
        return;
    }
    let pipe = unsafe { (*crtc).pipe as u32 };
    let pos = unsafe { intel_de_read(display, SKL_PS_WIN_POS(pipe, scaler_id as u32)) };
    let size = unsafe { intel_de_read(display, SKL_PS_WIN_SZ(pipe, scaler_id as u32)) };
    unsafe {
        (*crtc_state).pch_pfit.enabled = true;
        (*crtc_state).pch_pfit.dst.x1 = reg_field_get(PS_WIN_XPOS_MASK, pos) as i32;
        (*crtc_state).pch_pfit.dst.y1 = reg_field_get(PS_WIN_YPOS_MASK, pos) as i32;
        (*crtc_state).pch_pfit.dst.x2 =
            (*crtc_state).pch_pfit.dst.x1 + reg_field_get(PS_WIN_XSIZE_MASK, size) as i32;
        (*crtc_state).pch_pfit.dst.y2 =
            (*crtc_state).pch_pfit.dst.y1 + reg_field_get(PS_WIN_YSIZE_MASK, size) as i32;
        (*crtc_state).scaler_state.scalers[scaler_id as usize].in_use = true;
        (*crtc_state).scaler_state.scaler_id = scaler_id;
        (*crtc_state).scaler_state.scaler_users |= 1u32 << SKL_CRTC_INDEX;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn adl_scaler_ecc_mask(crtc_state: *const IntelCrtcState) {
    if crtc_state.is_null() || !unsafe { (*crtc_state).pch_pfit.enabled } {
        return;
    }
    let display = unsafe { display_from_crtc_state(crtc_state) };
    if !display.is_null() {
        unsafe { intel_de_write(display, XELPD_DISPLAY_ERR_FATAL_MASK, u32::MAX) };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn adl_scaler_ecc_unmask(crtc_state: *const IntelCrtcState) {
    if crtc_state.is_null() || unsafe { (*crtc_state).uapi.crtc.is_null() } {
        return;
    }
    let scaler_id = unsafe { (*crtc_state).scaler_state.scaler_id };
    if scaler_id < 0 || scaler_id >= 2 {
        return;
    }
    let crtc = unsafe { (*crtc_state).uapi.crtc.cast::<IntelCrtc>() };
    let display = unsafe { display_from_crtc(crtc) };
    if display.is_null() {
        return;
    }
    unsafe {
        intel_de_write_fw(
            display,
            SKL_PS_ECC_STAT((*crtc).pipe as u32, scaler_id as u32),
            1,
        );
        intel_de_write(display, XELPD_DISPLAY_ERR_FATAL_MASK, 0);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_scaler_1st_prefill_adjustment(
    _crtc_state: *const IntelCrtcState,
) -> u32 {
    0x10000
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_scaler_2nd_prefill_adjustment(
    _crtc_state: *const IntelCrtcState,
) -> u32 {
    0x10000
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_scaler_1st_prefill_lines(crtc_state: *const IntelCrtcState) -> u32 {
    if crtc_state.is_null() {
        return 0;
    }
    (unsafe { (*crtc_state).scaler_state.scaler_users.count_ones() > 0 })
        .then_some(4 << 16)
        .unwrap_or(0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_scaler_2nd_prefill_lines(crtc_state: *const IntelCrtcState) -> u32 {
    if crtc_state.is_null() {
        return 0;
    }
    (unsafe {
        (*crtc_state).scaler_state.scaler_users.count_ones() > 1 && (*crtc_state).pch_pfit.enabled
    })
    .then_some(4 << 16)
    .unwrap_or(0)
}

unsafe fn scaler_max_scale(crtc_state: *const IntelCrtcState, max_scale: u32) -> u32 {
    let display = unsafe { display_from_crtc_state(crtc_state) };
    if display.is_null() || crtc_state.is_null() {
        return 0x10000;
    }
    let clock = unsafe { (*crtc_state).hw.pipe_mode.crtc_clock };
    if clock <= 0 {
        return 0x10000;
    }
    let numerator = u64::from(unsafe { (*display).cdclk.max_dotclk_freq }) << 16;
    let max_clock_ratio = numerator
        .saturating_add(clock as u64 - 1)
        .checked_div(clock as u64)
        .unwrap_or(0)
        .min(u32::MAX as u64) as u32;
    max_scale.min(max_clock_ratio)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_scaler_max_total_scale(crtc_state: *const IntelCrtcState) -> u32 {
    if crtc_state.is_null() || unsafe { (*crtc_state).uapi.crtc.is_null() } {
        return 0x10000;
    }
    let crtc = unsafe { (*crtc_state).uapi.crtc.cast::<IntelCrtc>() };
    let count = unsafe { (*crtc).num_scalers };
    if count < 1 {
        return 0x10000;
    }
    let mut scale = 9u32 << 16;
    if count > 1 {
        scale = scale.saturating_mul(9);
    }
    unsafe { scaler_max_scale(crtc_state, scale) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_scaler_max_hscale(crtc_state: *const IntelCrtcState) -> u32 {
    if crtc_state.is_null() || unsafe { (*crtc_state).uapi.crtc.is_null() } {
        return 0x10000;
    }
    if unsafe { (*(*crtc_state).uapi.crtc.cast::<IntelCrtc>()).num_scalers < 1 } {
        return 0x10000;
    }
    unsafe { scaler_max_scale(crtc_state, 3u32 << 16) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_scaler_max_scale(crtc_state: *const IntelCrtcState) -> u32 {
    if crtc_state.is_null() || unsafe { (*crtc_state).uapi.crtc.is_null() } {
        return 0x10000;
    }
    if unsafe { (*(*crtc_state).uapi.crtc.cast::<IntelCrtc>()).num_scalers < 1 } {
        return 0x10000;
    }
    unsafe { scaler_max_scale(crtc_state, 9u32 << 16) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_scaler_1st_prefill_adjustment_worst(
    crtc_state: *const IntelCrtcState,
) -> u32 {
    if crtc_state.is_null() || unsafe { (*crtc_state).uapi.crtc.is_null() } {
        return 0x10000;
    }
    if unsafe { (*(*crtc_state).uapi.crtc.cast::<IntelCrtc>()).num_scalers > 0 } {
        unsafe { skl_scaler_max_scale(crtc_state) }
    } else {
        0x10000
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_scaler_2nd_prefill_adjustment_worst(
    crtc_state: *const IntelCrtcState,
) -> u32 {
    if crtc_state.is_null() || unsafe { (*crtc_state).uapi.crtc.is_null() } {
        return 0x10000;
    }
    if unsafe { (*(*crtc_state).uapi.crtc.cast::<IntelCrtc>()).num_scalers > 1 } {
        unsafe { skl_scaler_max_scale(crtc_state) }
    } else {
        0x10000
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_scaler_1st_prefill_lines_worst(
    crtc_state: *const IntelCrtcState,
) -> u32 {
    if crtc_state.is_null() || unsafe { (*crtc_state).uapi.crtc.is_null() } {
        return 0;
    }
    if unsafe { (*(*crtc_state).uapi.crtc.cast::<IntelCrtc>()).num_scalers > 0 } {
        4 << 16
    } else {
        0
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_scaler_2nd_prefill_lines_worst(
    crtc_state: *const IntelCrtcState,
) -> u32 {
    if crtc_state.is_null() || unsafe { (*crtc_state).uapi.crtc.is_null() } {
        return 0;
    }
    if unsafe { (*(*crtc_state).uapi.crtc.cast::<IntelCrtc>()).num_scalers > 1 } {
        4 << 16
    } else {
        0
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_scaler_mode_valid(
    display: *mut IntelDisplay,
    mode: *const DrmDisplayMode,
    output_format: IntelOutputFormat,
    num_joined_pipes: i32,
) -> i32 {
    if display.is_null() || mode.is_null() {
        return MODE_OK;
    }
    if num_joined_pipes < 2 && output_format == INTEL_OUTPUT_FORMAT_YCBCR420 {
        let (max_w, _) = max_src_size(unsafe { DISPLAY_VER(display) });
        if unsafe { (*mode).hdisplay as i32 > max_w } {
            return MODE_NO_420;
        }
    }
    MODE_OK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaler_phase_matches_source_q_format_vectors() {
        assert_eq!(scaler_calc_phase(1, 0x10000, false), PS_PHASE_TRIP as u16);
        assert_eq!(scaler_calc_phase(1, 0x08000, false), 0x3000);
        assert_eq!(scaler_calc_phase(2, 0x10000, false), 0x3000);
        assert_eq!(scaler_calc_phase(2, 0x10000, true), PS_PHASE_TRIP as u16);
    }

    #[test]
    fn adlp_scaler_size_bounds_and_semiplanar_minimum_are_enforced() {
        assert!(scaler_request_in_range(
            13, 5120, 2160, 5120, 2160, 3840, 2160, false
        ));
        assert!(!scaler_request_in_range(
            13, 8193, 2160, 5120, 2160, 3840, 2160, false
        ));
        assert!(!scaler_request_in_range(
            13, 1920, 1080, 15, 1080, 1920, 1080, true
        ));
        assert!(scaler_request_in_range(13, 1920, 1080, 16, 16, 8, 8, true));
        assert_eq!(max_src_size(13), (5120, 8192));
        assert_eq!(max_dst_size(13), (8192, 8192));
    }

    #[test]
    fn scaler_user_bit_rejects_out_of_range_index() {
        assert_eq!(mask_for_user(0), Some(1));
        assert_eq!(mask_for_user(31), Some(1 << 31));
        assert_eq!(mask_for_user(32), None);
    }

    #[test]
    fn scaler_pool_allocation_respects_in_use_and_casf_reservation() {
        assert_eq!(scaler_candidate(0b00, 2, 13, false), Some(0));
        assert_eq!(scaler_candidate(0b01, 2, 13, false), Some(1));
        assert_eq!(scaler_candidate(0b11, 2, 13, false), None);
        assert_eq!(scaler_candidate(0b00, 2, 19, true), None);
        assert_eq!(scaler_candidate(0b01, 2, 20, true), Some(1));
    }

    #[test]
    fn adlp_ratio_limits_match_source_generation_policy() {
        assert_eq!(calculate_max_scale(13, false, 0), (0x2ffff, 0x2ffff));
        assert_eq!(calculate_max_scale(13, true, 1), (0x2ffff, 0x2ffff));
        assert_eq!(calculate_max_scale(14, false, 1), (0x2ffff, 0x10000));
        assert_eq!(calculate_max_scale(14, false, 0), (0x2ffff, 0x2ffff));
    }
}
