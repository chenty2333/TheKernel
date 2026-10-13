// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// Source: drivers/gpu/drm/i915/display/skl_universal_plane.c (Linux v7.2.3)
// Display-13 universal-plane format, geometry, and commit paths.
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::{
    ffi::{c_char, c_void},
    ptr,
};

use tk_drm_upstream::{
    drm_color_mgmt_types::{
        DRM_COLOR_YCBCR_BT601, DRM_COLOR_YCBCR_BT709, DRM_COLOR_YCBCR_BT2020,
        DRM_COLOR_YCBCR_FULL_RANGE, DRM_COLOR_YCBCR_LIMITED_RANGE,
    },
    drm_device_types::DrmDevice,
    drm_fourcc_types::DrmFormatInfo,
    drm_framebuffer_types::DrmFramebuffer,
    drm_modeset_helper_vtables_types::DrmPlaneHelperFuncs,
    drm_plane_types::{
        DRM_PLANE_TYPE_OVERLAY, DRM_PLANE_TYPE_PRIMARY, DrmPlane, DrmPlaneFuncs, DrmPlaneType,
    },
    drm_rect_types::DrmRect,
    uapi_drm_consts::*,
};

use crate::{
    intel_de_upstream::{intel_de_read, intel_de_write, intel_de_write_dsb},
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_limits_types_upstream::{
        PLANE_1, PLANE_2, PLANE_3, PLANE_4, PLANE_5, Pipe, PlaneId,
    },
    intel_display_power_types_upstream::{
        POWER_DOMAIN_PIPE_A, POWER_DOMAIN_PIPE_B, POWER_DOMAIN_PIPE_C, POWER_DOMAIN_PIPE_D,
    },
    intel_display_reg_defs_upstream::reg_field_prep,
    intel_display_regs_upstream::GEN9_PIPE_PLANE_FLIP_DONE,
    intel_display_types_upstream::{
        IntelCrtc, IntelCrtcState, IntelFramebuffer, IntelInitialPlaneConfig, IntelPlane,
        IntelPlaneError, IntelPlaneState, SklWmLevel,
    },
    intel_display_wa_upstream::{DISPLAY_VER, platform_alderlake_p},
    intel_workarounds_types_upstream::I915RegT,
    skl_universal_plane_regs_upstream::*,
};

const I915_FORMAT_MOD_X_TILED: u64 = (1u64 << 56) | 1;
const I915_FORMAT_MOD_Y_TILED: u64 = (1u64 << 56) | 2;
const I915_FORMAT_MOD_Yf_TILED: u64 = (1u64 << 56) | 3;
const I915_FORMAT_MOD_Y_TILED_CCS: u64 = (1u64 << 56) | 4;
const I915_FORMAT_MOD_Yf_TILED_CCS: u64 = (1u64 << 56) | 5;
const I915_FORMAT_MOD_Y_TILED_GEN12_RC_CCS: u64 = (1u64 << 56) | 6;
const I915_FORMAT_MOD_Y_TILED_GEN12_MC_CCS: u64 = (1u64 << 56) | 7;
const I915_FORMAT_MOD_Y_TILED_GEN12_RC_CCS_CC: u64 = (1u64 << 56) | 8;
const I915_FORMAT_MOD_4_TILED: u64 = (1u64 << 56) | 9;
const I915_FORMAT_MOD_4_TILED_DG2_RC_CCS: u64 = (1u64 << 56) | 10;
const I915_FORMAT_MOD_4_TILED_DG2_MC_CCS: u64 = (1u64 << 56) | 11;
const I915_FORMAT_MOD_4_TILED_DG2_RC_CCS_CC: u64 = (1u64 << 56) | 12;
const I915_FORMAT_MOD_4_TILED_MTL_RC_CCS: u64 = (1u64 << 56) | 13;
const I915_FORMAT_MOD_4_TILED_MTL_MC_CCS: u64 = (1u64 << 56) | 14;
const I915_FORMAT_MOD_4_TILED_MTL_RC_CCS_CC: u64 = (1u64 << 56) | 15;
const I915_FORMAT_MOD_4_TILED_LNL_CCS: u64 = (1u64 << 56) | 16;
const I915_FORMAT_MOD_4_TILED_BMG_CCS: u64 = (1u64 << 56) | 17;
const PLANE_COLOR_ALPHA_MASK: u32 = 3 << 4;
const PLANE_COLOR_ALPHA_DISABLE: u32 = 0;
const PLANE_COLOR_ALPHA_SW_PREMULTIPLY: u32 = 2 << 4;
const PLANE_COLOR_ALPHA_HW_PREMULTIPLY: u32 = 3 << 4;
const I915_SET_COLORKEY_SOURCE: u32 = 1;
const I915_SET_COLORKEY_DESTINATION: u32 = 2;
const PLANE_CUS_ENABLE: u32 = 1 << 31;
const PLANE_CUS_HPHASE_0: u32 = 0;
const PLANE_CUS_VPHASE_SIGN_NEGATIVE: u32 = 1 << 8;
const PLANE_CUS_VPHASE_0_25: u32 = 1 << 4;

unsafe extern "C" {
    fn intel_plane_format_mod_supported_async(
        plane: *mut DrmPlane,
        format: u32,
        modifier: u64,
    ) -> bool;
    fn intel_plane_can_async_flip(
        plane: *mut IntelPlane,
        info: *const DrmFormatInfo,
        modifier: u64,
    ) -> bool;
    fn intel_plane_destroy(plane: *mut DrmPlane);
    fn intel_plane_duplicate_state(
        plane: *mut DrmPlane,
    ) -> *mut tk_drm_upstream::drm_plane_types::DrmPlaneState;
    fn intel_plane_destroy_state(
        plane: *mut DrmPlane,
        state: *mut tk_drm_upstream::drm_plane_types::DrmPlaneState,
    );
    fn intel_plane_alloc() -> *mut IntelPlane;
    fn intel_plane_free(plane: *mut IntelPlane);
    fn intel_plane_check_clipping(
        state: *mut IntelPlaneState,
        crtc: *mut IntelCrtcState,
        min_scale: i32,
        max_scale: i32,
        can_position: bool,
    ) -> i32;
    fn intel_plane_check_src_coordinates(state: *mut IntelPlaneState) -> i32;
    fn intel_plane_pixel_rate(crtc: *const IntelCrtcState, plane: *const IntelPlaneState) -> u32;
    fn intel_plane_data_rate(
        crtc: *const IntelCrtcState,
        plane: *const IntelPlaneState,
        color_plane: i32,
    ) -> u32;
    fn intel_display_power_get_if_enabled(display: *mut IntelDisplay, domain: u32) -> *mut c_void;
    fn intel_display_power_put(display: *mut IntelDisplay, domain: u32, wakeref: *mut c_void);
    fn intel_display_wa(display: *mut IntelDisplay, wa: u32) -> bool;
    fn intel_fb_uses_dpt(fb: *const DrmFramebuffer) -> bool;
    fn intel_fb_plane_get_modifiers(display: *mut IntelDisplay, caps: u8) -> *mut u64;
    fn intel_fb_plane_supports_modifier(plane: *mut IntelPlane, modifier: u64) -> bool;
    fn intel_fb_is_ccs_modifier(modifier: u64) -> bool;
    fn intel_fb_is_rc_ccs_cc_modifier(modifier: u64) -> bool;
    fn intel_fb_is_ccs_aux_plane(fb: *const DrmFramebuffer, color_plane: i32) -> bool;
    fn intel_fb_is_tile4_modifier(modifier: u64) -> bool;
    fn intel_fb_supports_90_270_rotation(fb: *const IntelFramebuffer) -> bool;
    fn intel_fb_plane_get_subsampling(
        hsub: *mut i32,
        vsub: *mut i32,
        fb: *const DrmFramebuffer,
        color_plane: i32,
    );
    fn intel_add_fb_offsets(
        x: *mut i32,
        y: *mut i32,
        plane_state: *const IntelPlaneState,
        color_plane: i32,
    );
    fn intel_plane_compute_aligned_offset(
        x: *mut i32,
        y: *mut i32,
        plane_state: *const IntelPlaneState,
        color_plane: i32,
    ) -> u32;
    fn intel_plane_adjust_aligned_offset(
        x: *mut i32,
        y: *mut i32,
        plane_state: *const IntelPlaneState,
        color_plane: i32,
        old_offset: u32,
        new_offset: u32,
    ) -> u32;
    fn intel_plane_compute_gtt(plane_state: *mut IntelPlaneState) -> i32;
    fn skl_main_to_aux_plane(fb: *const DrmFramebuffer, main_plane: i32) -> i32;
    fn skl_ccs_to_main_plane(fb: *const DrmFramebuffer, ccs_plane: i32) -> i32;
    fn intel_framebuffer_alloc() -> *mut IntelFramebuffer;
    fn intel_fb_align_height(fb: *const DrmFramebuffer, color_plane: i32, height: u32) -> u32;
    fn drm_get_format_info(dev: *mut DrmDevice, format: u32, modifier: u64) -> *mut DrmFormatInfo;
    fn intel_scanout_needs_vtd_wa(display: *mut IntelDisplay) -> bool;
    fn drm_plane_helper_add(plane: *mut DrmPlane, funcs: *const DrmPlaneHelperFuncs);
    fn intel_prepare_plane_fb(
        plane: *mut DrmPlane,
        state: *mut tk_drm_upstream::drm_plane_types::DrmPlaneState,
    ) -> i32;
    fn intel_cleanup_plane_fb(
        plane: *mut DrmPlane,
        state: *mut tk_drm_upstream::drm_plane_types::DrmPlaneState,
    );
    fn intel_get_scanout_buffer(plane: *mut DrmPlane, sb: *mut c_void) -> i32;
    fn intel_panic_flush(plane: *mut DrmPlane);
    fn skl_program_plane_scaler(
        dsb: *mut c_void,
        plane: *mut IntelPlane,
        crtc_state: *const IntelCrtcState,
        plane_state: *const IntelPlaneState,
    );
    fn intel_plane_pin_fb(
        new_state: *mut IntelPlaneState,
        old_state: *const IntelPlaneState,
    ) -> i32;
    fn intel_color_plane_program_pipeline(dsb: *mut c_void, plane_state: *const IntelPlaneState);
    fn intel_color_plane_commit_arm(dsb: *mut c_void, plane_state: *const IntelPlaneState);
    fn bdw_enable_pipe_irq(display: *mut IntelDisplay, pipe: Pipe, bits: u32);
    fn bdw_disable_pipe_irq(display: *mut IntelDisplay, pipe: Pipe, bits: u32);
}

#[inline]
unsafe fn plane_from_drm(plane: *mut DrmPlane) -> *mut IntelPlane {
    container_of!(plane, IntelPlane, base)
}
pub(crate) unsafe fn intel_plane_from_drm(plane: *mut DrmPlane) -> *mut IntelPlane {
    unsafe { plane_from_drm(plane) }
}
#[inline]
unsafe fn state_from_drm(
    state: *mut tk_drm_upstream::drm_plane_types::DrmPlaneState,
) -> *mut IntelPlaneState {
    container_of!(state, IntelPlaneState, uapi)
}
#[inline]
unsafe fn display_from_plane(plane: *mut IntelPlane) -> *mut IntelDisplay {
    if plane.is_null() {
        return ptr::null_mut();
    }
    let dev = unsafe { (*plane).base.dev };
    if dev.is_null() || unsafe { (*dev).dev_private.is_null() } {
        return ptr::null_mut();
    }
    let i915 = unsafe {
        (*dev)
            .dev_private
            .cast::<crate::linux_i915_private::DrmI915Private>()
    };
    unsafe { (*i915).display.cast::<IntelDisplay>() }
}

unsafe fn skl_plane_fbc(display: *mut IntelDisplay, pipe: Pipe, plane_id: PlaneId) -> *mut c_void {
    if display.is_null() || !(0..4).contains(&pipe) {
        return ptr::null_mut();
    }
    let runtime = unsafe { &(*display).info.__runtime_info };
    if runtime.fbc_mask & (1u8 << pipe) == 0 {
        return ptr::null_mut();
    }
    let eligible = if unsafe { DISPLAY_VER(display) >= 20 } {
        unsafe { icl_is_hdr_plane(display, plane_id) }
    } else {
        plane_id == PLANE_1
    };
    if !eligible {
        return ptr::null_mut();
    }
    unsafe { (*display).fbc.instances[pipe as usize] }
}
#[inline]
fn rect_width(r: &DrmRect) -> i32 {
    tk_drm_upstream::drm_rect_upstream::drm_rect_width(r)
}
#[inline]
fn rect_height(r: &DrmRect) -> i32 {
    tk_drm_upstream::drm_rect_upstream::drm_rect_height(r)
}
#[inline]
fn semiplanar(format: u32) -> bool {
    matches!(
        format,
        DRM_FORMAT_NV12 | DRM_FORMAT_P010 | DRM_FORMAT_P012 | DRM_FORMAT_P016
    )
}

#[inline]
fn format_cpp(fmt: *const DrmFormatInfo, plane: usize) -> u32 {
    if fmt.is_null() || plane >= 4 {
        0
    } else {
        unsafe { (*fmt)._anon_6.cpp[plane] as u32 }
    }
}

// upstream: skl_universal_plane.c icl_nv12_y_plane_mask()/icl_is_nv12_y_plane()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn icl_is_nv12_y_plane(
    display: *mut IntelDisplay,
    plane_id: PlaneId,
) -> bool {
    !display.is_null()
        && unsafe {
            DISPLAY_VER(display) >= 11
                && (if DISPLAY_VER(display) >= 13 {
                    plane_id == PLANE_4 || plane_id == PLANE_5
                } else {
                    plane_id == 6 || plane_id == 7
                })
        }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn icl_hdr_plane_mask() -> u8 {
    0b0000_0111
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn icl_is_hdr_plane(display: *mut IntelDisplay, plane_id: PlaneId) -> bool {
    !display.is_null() && unsafe { DISPLAY_VER(display) >= 11 && plane_id <= PLANE_3 }
}

// upstream: skl_universal_plane.c skl_format_to_fourcc()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_format_to_fourcc(format: i32, rgb_order: bool, alpha: bool) -> i32 {
    use crate::skl_universal_plane_regs_upstream::*;
    match format as u32 {
        PLANE_CTL_FORMAT_RGB_565 => DRM_FORMAT_RGB565 as i32,
        PLANE_CTL_FORMAT_NV12 => DRM_FORMAT_NV12 as i32,
        PLANE_CTL_FORMAT_XYUV => DRM_FORMAT_XYUV8888 as i32,
        PLANE_CTL_FORMAT_P010 => DRM_FORMAT_P010 as i32,
        PLANE_CTL_FORMAT_P012 => DRM_FORMAT_P012 as i32,
        PLANE_CTL_FORMAT_P016 => DRM_FORMAT_P016 as i32,
        PLANE_CTL_FORMAT_Y210 => DRM_FORMAT_Y210 as i32,
        PLANE_CTL_FORMAT_Y212 => DRM_FORMAT_Y212 as i32,
        PLANE_CTL_FORMAT_Y216 => DRM_FORMAT_Y216 as i32,
        PLANE_CTL_FORMAT_Y410 => DRM_FORMAT_XVYU2101010 as i32,
        PLANE_CTL_FORMAT_Y412 => DRM_FORMAT_XVYU12_16161616 as i32,
        PLANE_CTL_FORMAT_Y416 => DRM_FORMAT_XVYU16161616 as i32,
        PLANE_CTL_FORMAT_XRGB_2101010 => {
            (if rgb_order {
                if alpha {
                    DRM_FORMAT_ABGR2101010
                } else {
                    DRM_FORMAT_XBGR2101010
                }
            } else if alpha {
                DRM_FORMAT_ARGB2101010
            } else {
                DRM_FORMAT_XRGB2101010
            }) as i32
        }
        PLANE_CTL_FORMAT_XRGB_16161616F => {
            (if rgb_order {
                if alpha {
                    DRM_FORMAT_ABGR16161616F
                } else {
                    DRM_FORMAT_XBGR16161616F
                }
            } else if alpha {
                DRM_FORMAT_ARGB16161616F
            } else {
                DRM_FORMAT_XRGB16161616F
            }) as i32
        }
        _ => {
            (if rgb_order {
                if alpha {
                    DRM_FORMAT_ABGR8888
                } else {
                    DRM_FORMAT_XBGR8888
                }
            } else if alpha {
                DRM_FORMAT_ARGB8888
            } else {
                DRM_FORMAT_XRGB8888
            }) as i32
        }
    }
}

// Fourcc-to-control encoding is shared by check, initial-plane decoding and
// the live arm path. Unsupported formats fail closed instead of programming a
// plausible but wrong encoding.
pub fn plane_ctl_format(pixel_format: u32) -> Option<u32> {
    use crate::skl_universal_plane_regs_upstream::*;
    Some(match pixel_format {
        DRM_FORMAT_C8 => PLANE_CTL_FORMAT_INDEXED,
        DRM_FORMAT_RGB565 => PLANE_CTL_FORMAT_RGB_565,
        DRM_FORMAT_XBGR8888 | DRM_FORMAT_ABGR8888 => {
            PLANE_CTL_FORMAT_XRGB_8888 | PLANE_CTL_ORDER_RGBX
        }
        DRM_FORMAT_XRGB8888 | DRM_FORMAT_ARGB8888 => PLANE_CTL_FORMAT_XRGB_8888,
        DRM_FORMAT_XBGR2101010 | DRM_FORMAT_ABGR2101010 => {
            PLANE_CTL_FORMAT_XRGB_2101010 | PLANE_CTL_ORDER_RGBX
        }
        DRM_FORMAT_XRGB2101010 | DRM_FORMAT_ARGB2101010 => PLANE_CTL_FORMAT_XRGB_2101010,
        DRM_FORMAT_XBGR16161616F | DRM_FORMAT_ABGR16161616F => {
            PLANE_CTL_FORMAT_XRGB_16161616F | PLANE_CTL_ORDER_RGBX
        }
        DRM_FORMAT_XRGB16161616F | DRM_FORMAT_ARGB16161616F => PLANE_CTL_FORMAT_XRGB_16161616F,
        DRM_FORMAT_XYUV8888 => PLANE_CTL_FORMAT_XYUV,
        DRM_FORMAT_YUYV => PLANE_CTL_FORMAT_YUV422 | PLANE_CTL_YUV422_ORDER_YUYV,
        DRM_FORMAT_YVYU => PLANE_CTL_FORMAT_YUV422 | PLANE_CTL_YUV422_ORDER_YVYU,
        DRM_FORMAT_UYVY => PLANE_CTL_FORMAT_YUV422 | PLANE_CTL_YUV422_ORDER_UYVY,
        DRM_FORMAT_VYUY => PLANE_CTL_FORMAT_YUV422 | PLANE_CTL_YUV422_ORDER_VYUY,
        DRM_FORMAT_NV12 => PLANE_CTL_FORMAT_NV12,
        DRM_FORMAT_P010 => PLANE_CTL_FORMAT_P010,
        DRM_FORMAT_P012 => PLANE_CTL_FORMAT_P012,
        DRM_FORMAT_P016 => PLANE_CTL_FORMAT_P016,
        DRM_FORMAT_Y210 => PLANE_CTL_FORMAT_Y210,
        DRM_FORMAT_Y212 => PLANE_CTL_FORMAT_Y212,
        DRM_FORMAT_Y216 => PLANE_CTL_FORMAT_Y216,
        DRM_FORMAT_XVYU2101010 => PLANE_CTL_FORMAT_Y410,
        DRM_FORMAT_XVYU12_16161616 => PLANE_CTL_FORMAT_Y412,
        DRM_FORMAT_XVYU16161616 => PLANE_CTL_FORMAT_Y416,
        _ => return None,
    })
}

pub fn plane_ctl_tiling(modifier: u64) -> Option<u32> {
    use crate::skl_universal_plane_regs_upstream::*;
    Some(match modifier {
        DRM_FORMAT_MOD_LINEAR => PLANE_CTL_TILED_LINEAR,
        I915_FORMAT_MOD_X_TILED => PLANE_CTL_TILED_X,
        I915_FORMAT_MOD_Y_TILED => PLANE_CTL_TILED_Y,
        I915_FORMAT_MOD_4_TILED => PLANE_CTL_TILED_4,
        I915_FORMAT_MOD_4_TILED_DG2_RC_CCS => {
            PLANE_CTL_TILED_4
                | PLANE_CTL_RENDER_DECOMPRESSION_ENABLE
                | PLANE_CTL_CLEAR_COLOR_DISABLE
        }
        I915_FORMAT_MOD_4_TILED_DG2_MC_CCS => {
            PLANE_CTL_TILED_4 | PLANE_CTL_MEDIA_DECOMPRESSION_ENABLE | PLANE_CTL_CLEAR_COLOR_DISABLE
        }
        I915_FORMAT_MOD_4_TILED_DG2_RC_CCS_CC => {
            PLANE_CTL_TILED_4 | PLANE_CTL_RENDER_DECOMPRESSION_ENABLE
        }
        I915_FORMAT_MOD_4_TILED_MTL_RC_CCS => {
            PLANE_CTL_TILED_4
                | PLANE_CTL_RENDER_DECOMPRESSION_ENABLE
                | PLANE_CTL_CLEAR_COLOR_DISABLE
        }
        I915_FORMAT_MOD_4_TILED_MTL_RC_CCS_CC => {
            PLANE_CTL_TILED_4 | PLANE_CTL_RENDER_DECOMPRESSION_ENABLE
        }
        I915_FORMAT_MOD_4_TILED_MTL_MC_CCS => {
            PLANE_CTL_TILED_4 | PLANE_CTL_MEDIA_DECOMPRESSION_ENABLE
        }
        I915_FORMAT_MOD_4_TILED_BMG_CCS | I915_FORMAT_MOD_4_TILED_LNL_CCS => {
            PLANE_CTL_TILED_4 | PLANE_CTL_RENDER_DECOMPRESSION_ENABLE
        }
        I915_FORMAT_MOD_Y_TILED_CCS | I915_FORMAT_MOD_Y_TILED_GEN12_RC_CCS_CC => {
            PLANE_CTL_TILED_Y | PLANE_CTL_RENDER_DECOMPRESSION_ENABLE
        }
        I915_FORMAT_MOD_Y_TILED_GEN12_RC_CCS => {
            PLANE_CTL_TILED_Y
                | PLANE_CTL_RENDER_DECOMPRESSION_ENABLE
                | PLANE_CTL_CLEAR_COLOR_DISABLE
        }
        I915_FORMAT_MOD_Y_TILED_GEN12_MC_CCS => {
            PLANE_CTL_TILED_Y | PLANE_CTL_MEDIA_DECOMPRESSION_ENABLE
        }
        I915_FORMAT_MOD_Yf_TILED => PLANE_CTL_TILED_YF,
        I915_FORMAT_MOD_Yf_TILED_CCS => PLANE_CTL_TILED_YF | PLANE_CTL_RENDER_DECOMPRESSION_ENABLE,
        _ => return None,
    })
}

pub fn plane_ctl_rotation(rotation: u32) -> Option<u32> {
    use crate::skl_universal_plane_regs_upstream::*;
    Some(match rotation & DRM_MODE_ROTATE_MASK {
        DRM_MODE_ROTATE_0 => PLANE_CTL_ROTATE_0,
        DRM_MODE_ROTATE_90 => PLANE_CTL_ROTATE_270,
        DRM_MODE_ROTATE_180 => PLANE_CTL_ROTATE_180,
        DRM_MODE_ROTATE_270 => PLANE_CTL_ROTATE_90,
        _ => return None,
    })
}

// upstream: skl_universal_plane.c skl_plane_ctl_format()/tiling()/rotate()
pub unsafe fn skl_plane_ctl(state: *const IntelPlaneState) -> Option<u32> {
    if state.is_null() || unsafe { (*state).hw.fb.is_null() } {
        return None;
    }
    let fb = unsafe { (*state).hw.fb };
    let fmt = unsafe { (*fb).format };
    if fmt.is_null() {
        return None;
    }
    let format = plane_ctl_format(unsafe { (*fmt).format })?;
    let tiling = plane_ctl_tiling(unsafe { (*fb).modifier })?;
    let rotate = plane_ctl_rotation(unsafe { (*state).hw.rotation })?;
    let reflection = unsafe { (*state).hw.rotation & DRM_MODE_REFLECT_MASK };
    if reflection & DRM_MODE_REFLECT_Y != 0 {
        return None;
    }
    let mut ctl = PLANE_CTL_ENABLE | format | tiling | rotate;
    if reflection & DRM_MODE_REFLECT_X != 0 {
        ctl |= PLANE_CTL_FLIP_HORIZONTAL;
    }
    let ckey_flags = unsafe { (*state).ckey.flags };
    if ckey_flags & I915_SET_COLORKEY_DESTINATION != 0 {
        ctl |= PLANE_CTL_KEY_ENABLE_DESTINATION;
    } else if ckey_flags & I915_SET_COLORKEY_SOURCE != 0 {
        ctl |= PLANE_CTL_KEY_ENABLE_SOURCE;
    }
    let cpp = format_cpp(fmt, 0);
    if semiplanar(unsafe { (*fmt).format }) {
        if cpp == 2 {
            ctl |= PLANE_CTL_ARB_SLOTS(1);
        }
    } else if cpp == 8 {
        ctl |= PLANE_CTL_ARB_SLOTS(3);
    } else if cpp == 4 {
        ctl |= PLANE_CTL_ARB_SLOTS(1);
    }
    Some(ctl)
}

pub unsafe fn glk_plane_color_ctl(state: *const IntelPlaneState) -> u32 {
    if state.is_null() || unsafe { (*state).hw.fb.is_null() } {
        return 0;
    }
    let fb = unsafe { (*state).hw.fb };
    let fmt = unsafe { (*fb).format };
    if fmt.is_null() {
        return 0;
    }
    let alpha = if !unsafe { (*fmt).has_alpha } {
        PLANE_COLOR_ALPHA_DISABLE
    } else {
        match unsafe { (*state).hw.pixel_blend_mode } {
            0 => PLANE_COLOR_ALPHA_DISABLE,
            1 => PLANE_COLOR_ALPHA_SW_PREMULTIPLY,
            2 => PLANE_COLOR_ALPHA_HW_PREMULTIPLY,
            _ => PLANE_COLOR_ALPHA_DISABLE,
        }
    };
    let mut val = PLANE_CTL_PLANE_GAMMA_DISABLE | alpha;
    if unsafe { (*fmt).is_yuv } {
        // ICL HDR planes use the programmable input CSC; SDR uses fixed conversion.
        if unsafe { (*state).uapi.plane.is_null() } {
            return val;
        }
        let plane = unsafe { plane_from_drm((*state).uapi.plane) };
        let display = unsafe { display_from_plane(plane) };
        if unsafe { icl_is_hdr_plane(display, (*plane).id) } {
            val |= PLANE_COLOR_INPUT_CSC_ENABLE;
        }
        if unsafe { (*state).hw.color_range as u32 == 1 } {
            val |= PLANE_COLOR_YUV_RANGE_CORRECTION_DISABLE;
        } else if unsafe { (*state).hw.color_encoding as u32 == 1 } {
            val |= PLANE_COLOR_CSC_MODE_YUV709_TO_RGB709;
        } else {
            val |= PLANE_COLOR_CSC_MODE_YUV601_TO_RGB601;
        }
    }
    if unsafe { !(*state).hw.degamma_lut.is_null() } {
        val |= PLANE_COLOR_PRE_CSC_GAMMA_ENABLE;
    }
    if unsafe { !(*state).hw.ctm.is_null() || (*state).force_black } {
        val |= PLANE_COLOR_PLANE_CSC_ENABLE;
    }
    if unsafe { !(*state).hw.gamma_lut.is_null() } {
        val &= !PLANE_CTL_PLANE_GAMMA_DISABLE;
    }
    val
}

/// Link the ICL/ADLP UV plane's input CSC chroma selector to its companion Y
/// plane. The caller has already linked the two software plane states and
/// copied the shared surface state, matching `icl_link_nv12_planes()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn icl_link_nv12_planes(
    uv_state: *mut IntelPlaneState,
    y_state: *mut IntelPlaneState,
) {
    if uv_state.is_null() || y_state.is_null() {
        return;
    }
    let uv_uapi = unsafe { (*uv_state).uapi.plane };
    let y_uapi = unsafe { (*y_state).uapi.plane };
    if uv_uapi.is_null() || y_uapi.is_null() {
        return;
    }
    let uv_plane = unsafe { plane_from_drm(uv_uapi) };
    let y_plane = unsafe { plane_from_drm(y_uapi) };
    let display = unsafe { display_from_plane(uv_plane) };
    let uv_id = unsafe { (*uv_plane).id };
    let y_id = unsafe { (*y_plane).id };
    if display.is_null()
        || unsafe { icl_is_nv12_y_plane(display, uv_id) || !icl_is_nv12_y_plane(display, y_id) }
    {
        return;
    }
    unsafe {
        (*y_state).ctl |= PLANE_CTL_YUV420_Y_PLANE;
    }
    if unsafe { icl_is_hdr_plane(display, uv_id) } {
        let selector = match y_id {
            PLANE_4 => PLANE_CUS_Y_PLANE_4_RKL,
            PLANE_5 => PLANE_CUS_Y_PLANE_5_RKL,
            PLANE_6 => PLANE_CUS_Y_PLANE_6_ICL,
            PLANE_7 => PLANE_CUS_Y_PLANE_7_ICL,
            _ => return,
        };
        unsafe {
            (*uv_state).cus_ctl |= selector;
        }
    }
}

fn tile_width_bytes(modifier: u64) -> u32 {
    match modifier {
        DRM_FORMAT_MOD_LINEAR => 64,
        I915_FORMAT_MOD_X_TILED => 512,
        I915_FORMAT_MOD_Y_TILED
        | I915_FORMAT_MOD_Y_TILED_CCS
        | I915_FORMAT_MOD_Y_TILED_GEN12_RC_CCS
        | I915_FORMAT_MOD_Y_TILED_GEN12_MC_CCS
        | I915_FORMAT_MOD_Y_TILED_GEN12_RC_CCS_CC
        | I915_FORMAT_MOD_Yf_TILED
        | I915_FORMAT_MOD_Yf_TILED_CCS => 128,
        I915_FORMAT_MOD_4_TILED
        | I915_FORMAT_MOD_4_TILED_DG2_RC_CCS
        | I915_FORMAT_MOD_4_TILED_DG2_MC_CCS
        | I915_FORMAT_MOD_4_TILED_DG2_RC_CCS_CC
        | I915_FORMAT_MOD_4_TILED_MTL_RC_CCS
        | I915_FORMAT_MOD_4_TILED_MTL_MC_CCS
        | I915_FORMAT_MOD_4_TILED_MTL_RC_CCS_CC
        | I915_FORMAT_MOD_4_TILED_BMG_CCS
        | I915_FORMAT_MOD_4_TILED_LNL_CCS => 256,
        _ => 0,
    }
}

fn tile_height(modifier: u64) -> u32 {
    match modifier {
        DRM_FORMAT_MOD_LINEAR => 1,
        I915_FORMAT_MOD_X_TILED => 8,
        I915_FORMAT_MOD_Y_TILED
        | I915_FORMAT_MOD_Y_TILED_CCS
        | I915_FORMAT_MOD_Y_TILED_GEN12_RC_CCS
        | I915_FORMAT_MOD_Y_TILED_GEN12_MC_CCS
        | I915_FORMAT_MOD_Y_TILED_GEN12_RC_CCS_CC
        | I915_FORMAT_MOD_Yf_TILED
        | I915_FORMAT_MOD_Yf_TILED_CCS => 32,
        I915_FORMAT_MOD_4_TILED
        | I915_FORMAT_MOD_4_TILED_DG2_RC_CCS
        | I915_FORMAT_MOD_4_TILED_DG2_MC_CCS
        | I915_FORMAT_MOD_4_TILED_DG2_RC_CCS_CC
        | I915_FORMAT_MOD_4_TILED_MTL_RC_CCS
        | I915_FORMAT_MOD_4_TILED_MTL_MC_CCS
        | I915_FORMAT_MOD_4_TILED_MTL_RC_CCS_CC
        | I915_FORMAT_MOD_4_TILED_BMG_CCS
        | I915_FORMAT_MOD_4_TILED_LNL_CCS => 32,
        _ => 0,
    }
}

unsafe fn plane_stride_mult(fb: *const DrmFramebuffer, color_plane: usize, rotation: u32) -> u32 {
    if fb.is_null() {
        return 0;
    }
    let modifier = unsafe { (*fb).modifier };
    let width_bytes = tile_width_bytes(modifier);
    if modifier == DRM_FORMAT_MOD_LINEAR {
        64
    } else if rotation & (DRM_MODE_ROTATE_90 | DRM_MODE_ROTATE_270) != 0 {
        tile_height(modifier)
    } else {
        let _ = color_plane;
        width_bytes
    }
}

unsafe fn plane_stride(state: *const IntelPlaneState, color_plane: usize) -> u32 {
    if state.is_null() || unsafe { (*state).hw.fb.is_null() } || color_plane >= 4 {
        return 0;
    }
    let fb = unsafe { (*state).hw.fb };
    let fmt = unsafe { (*fb).format };
    if fmt.is_null() || color_plane >= unsafe { (*fmt).num_planes as usize } {
        return 0;
    }
    let mult = unsafe { plane_stride_mult(fb, color_plane, (*state).hw.rotation) };
    if mult == 0 {
        return 0;
    }
    unsafe { (*state).view.color_plane[color_plane].scanout_stride / mult }
}

unsafe fn plane_surf_address(state: *const IntelPlaneState, color_plane: usize) -> u32 {
    let offset = unsafe { (*state).view.color_plane[color_plane].offset };
    let fb = unsafe { (*state).hw.fb };
    if unsafe { intel_fb_uses_dpt(fb) } {
        offset >> 9
    } else {
        offset
    }
}

unsafe fn plane_color_plane(state: *const IntelPlaneState) -> usize {
    if unsafe { !(*state).planar_linked_plane.is_null() && !(*state).is_y_plane } {
        1
    } else {
        0
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_plane_surf_offset(state: *mut IntelPlaneState) -> u32 {
    if state.is_null() {
        return 0;
    }
    let color = unsafe { plane_color_plane(state) };
    let mut surface = unsafe { plane_surf_address(state, color) };
    if unsafe { (*state).decrypt } {
        surface |= PLANE_SURF_DECRYPT;
    }
    surface
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_plane_aux_dist(
    state: *const IntelPlaneState,
    color_plane: i32,
) -> u32 {
    if state.is_null() || color_plane < 0 || unsafe { (*state).hw.fb.is_null() } {
        return 0;
    }
    let fb = unsafe { (*state).hw.fb };
    let aux = unsafe { skl_main_to_aux_plane(fb, color_plane) };
    if aux <= 0 {
        return 0;
    }
    let dist = unsafe {
        plane_surf_address(state, aux as usize)
            .wrapping_sub(plane_surf_address(state, color_plane as usize))
    };
    if unsafe { DISPLAY_VER(display_from_plane(plane_from_drm((*state).uapi.plane))) < 12 } {
        dist | PLANE_AUX_STRIDE(unsafe { plane_stride(state, aux as usize) })
    } else {
        dist
    }
}

unsafe fn plane_key_val(state: *const IntelPlaneState) -> u32 {
    if state.is_null() {
        0
    } else {
        unsafe { (*state).ckey.min_value }
    }
}
unsafe fn plane_key_max(state: *const IntelPlaneState) -> u32 {
    if state.is_null() {
        0
    } else {
        unsafe {
            ((*state).ckey.max_value & 0x00ff_ffff)
                | PLANE_KEYMAX_ALPHA(((*state).hw.alpha >> 8) as u32)
        }
    }
}
unsafe fn plane_key_mask(state: *const IntelPlaneState) -> u32 {
    if state.is_null() {
        return 0;
    }
    unsafe {
        let mut value = (*state).ckey.channel_mask & 0x07ff_ffff;
        if (*state).hw.alpha >> 8 < 0xff {
            value |= PLANE_KEYMSK_ALPHA_ENABLE;
        }
        value
    }
}

unsafe fn wm_reg_val(level: *const SklWmLevel) -> u32 {
    if level.is_null() {
        return 0;
    }
    unsafe {
        let mut val = if (*level).enable { PLANE_WM_EN } else { 0 };
        if (*level).ignore_lines {
            val |= PLANE_WM_IGNORE_LINES;
        }
        if (*level).auto_min_alloc_wm_enable {
            val |= PLANE_WM_AUTO_MIN_ALLOC_EN;
        }
        val |= reg_field_prep(PLANE_WM_BLOCKS_MASK, (*level).blocks as u32);
        val |= reg_field_prep(PLANE_WM_LINES_MASK, (*level).lines as u32);
        val
    }
}

unsafe fn ddb_reg_val(entry: *const crate::intel_wm_types_upstream::SklDdbEntry) -> u32 {
    if entry.is_null() || unsafe { (*entry).end == 0 } {
        0
    } else {
        PLANE_BUF_END(unsafe { (*entry).end as u32 - 1 })
            | PLANE_BUF_START(unsafe { (*entry).start as u32 })
    }
}

unsafe fn skl_write_plane_wm(
    dsb: *mut c_void,
    plane: *mut IntelPlane,
    state: *const IntelCrtcState,
) {
    if plane.is_null() || state.is_null() {
        return;
    }
    let display = unsafe { display_from_plane(plane) };
    if display.is_null() {
        return;
    }
    let id = unsafe { (*plane).id as usize };
    let pipe = unsafe { (*plane).pipe as u32 };
    let wm = unsafe { &(*state).wm._anon_0_0.skl };
    let levels = unsafe { (*display).wm.num_levels as usize }.min(8);
    for level in 0..levels {
        let plane_wm = unsafe { ptr::addr_of!((*wm).optimal.planes[id].wm[level]) };
        unsafe {
            intel_de_write_dsb(
                display,
                dsb,
                PLANE_WM(pipe, id as u32, level as u32),
                wm_reg_val(plane_wm),
            );
        }
    }
    unsafe {
        intel_de_write_dsb(
            display,
            dsb,
            PLANE_WM_TRANS(pipe, id as u32),
            wm_reg_val(ptr::addr_of!((*wm).optimal.planes[id].trans_wm)),
        );
        intel_de_write_dsb(
            display,
            dsb,
            PLANE_BUF_CFG(pipe, id as u32),
            ddb_reg_val(ptr::addr_of!((*wm).plane_ddb[id])),
        );
        if DISPLAY_VER(display) >= 13
            && unsafe { (*display).platform._anon_0_0.bitmap[0] & (1 << 2) == 0 }
        {
            intel_de_write_dsb(
                display,
                dsb,
                PLANE_WM_SAGV(pipe, id as u32),
                wm_reg_val(ptr::addr_of!((*wm).optimal.planes[id].sagv.wm0)),
            );
            intel_de_write_dsb(
                display,
                dsb,
                PLANE_WM_SAGV_TRANS(pipe, id as u32),
                wm_reg_val(ptr::addr_of!((*wm).optimal.planes[id].sagv.trans_wm)),
            );
        }
    }
}

unsafe extern "C" fn skl_plane_update_noarm(
    dsb: *mut c_void,
    plane: *mut IntelPlane,
    crtc_state: *mut IntelCrtcState,
    state: *mut IntelPlaneState,
) {
    if plane.is_null() || crtc_state.is_null() || state.is_null() {
        return;
    }
    let display = unsafe { display_from_plane(plane) };
    let pipe = unsafe { (*plane).pipe as u32 };
    let id = unsafe { (*plane).id };
    let x = if unsafe { (*state).scaler_id >= 0 } {
        0
    } else {
        unsafe { (*state).uapi.dst.x1.max(0) as u32 }
    };
    let y = if unsafe { (*state).scaler_id >= 0 } {
        0
    } else {
        unsafe { (*state).uapi.dst.y1.max(0) as u32 }
    };
    let src_w = (unsafe { rect_width(&(*state).uapi.src) } >> 16).max(1) as u32;
    let src_h = (unsafe { rect_height(&(*state).uapi.src) } >> 16).max(1) as u32;
    let color = unsafe { plane_color_plane(state) };
    unsafe {
        intel_de_write_dsb(
            display,
            dsb,
            PLANE_STRIDE(pipe, id),
            PLANE_STRIDE_(plane_stride(state, color)),
        );
        intel_de_write_dsb(
            display,
            dsb,
            PLANE_POS(pipe, id),
            PLANE_POS_Y(y) | PLANE_POS_X(x),
        );
        intel_de_write_dsb(
            display,
            dsb,
            PLANE_SIZE(pipe, id),
            PLANE_HEIGHT(src_h - 1) | PLANE_WIDTH(src_w - 1),
        );
        skl_write_plane_wm(dsb, plane, crtc_state);
    }
}

unsafe extern "C" fn skl_plane_update_arm(
    dsb: *mut c_void,
    plane: *mut IntelPlane,
    crtc_state: *mut IntelCrtcState,
    state: *mut IntelPlaneState,
) {
    if plane.is_null() || crtc_state.is_null() || state.is_null() {
        return;
    }
    let display = unsafe { display_from_plane(plane) };
    let pipe = unsafe { (*plane).pipe as u32 };
    let id = unsafe { (*plane).id };
    let color = unsafe { plane_color_plane(state) };
    let x = unsafe { (*state).view.color_plane[color].x };
    let y = unsafe { (*state).view.color_plane[color].y };
    let Some(mut ctl) = (unsafe { skl_plane_ctl(state) }) else {
        return;
    };
    let color_ctl = unsafe { glk_plane_color_ctl(state) };
    if unsafe {
        (*plane).need_async_flip_toggle_wa && (*crtc_state).async_flip_planes & (1 << id) != 0
    } {
        ctl |= PLANE_CTL_ASYNC_FLIP;
    }
    unsafe {
        intel_de_write_dsb(display, dsb, PLANE_KEYVAL(pipe, id), plane_key_val(state));
        intel_de_write_dsb(display, dsb, PLANE_KEYMSK(pipe, id), plane_key_mask(state));
        intel_de_write_dsb(display, dsb, PLANE_KEYMAX(pipe, id), plane_key_max(state));
        intel_de_write_dsb(
            display,
            dsb,
            PLANE_OFFSET(pipe, id),
            PLANE_OFFSET_Y(y) | PLANE_OFFSET_X(x),
        );
        intel_de_write_dsb(
            display,
            dsb,
            PLANE_AUX_DIST(pipe, id),
            skl_plane_aux_dist(state, color as i32),
        );
        intel_de_write_dsb(
            display,
            dsb,
            PLANE_AUX_OFFSET(pipe, id),
            PLANE_OFFSET_Y((*state).view.color_plane[1].y)
                | PLANE_OFFSET_X((*state).view.color_plane[1].x),
        );
        intel_de_write_dsb(display, dsb, PLANE_COLOR_CTL(pipe, id), color_ctl);
        if (*state).scaler_id >= 0 {
            skl_program_plane_scaler(dsb, plane, crtc_state, state);
        }
        intel_de_write_dsb(display, dsb, PLANE_CTL(pipe, id), ctl);
        intel_de_write_dsb(display, dsb, PLANE_SURF(pipe, id), (*state).surf);
    }
}

unsafe extern "C" fn skl_plane_disable_arm(
    dsb: *mut c_void,
    plane: *mut IntelPlane,
    crtc_state: *mut IntelCrtcState,
) {
    if plane.is_null() || crtc_state.is_null() {
        return;
    }
    let display = unsafe { display_from_plane(plane) };
    let pipe = unsafe { (*plane).pipe as u32 };
    let id = unsafe { (*plane).id };
    unsafe {
        skl_write_plane_wm(dsb, plane, crtc_state);
        intel_de_write_dsb(display, dsb, PLANE_CTL(pipe, id), 0);
        intel_de_write_dsb(display, dsb, PLANE_SURF(pipe, id), 0);
    }
}

unsafe fn icl_program_input_csc(
    dsb: *mut c_void,
    plane: *mut IntelPlane,
    state: *const IntelPlaneState,
) {
    if plane.is_null() || state.is_null() {
        return;
    }
    let display = unsafe { display_from_plane(plane) };
    let pipe = unsafe { (*plane).pipe as u32 };
    let id = unsafe { (*plane).id };
    let enc = unsafe { (*state).hw.color_encoding as usize }.min(2);
    const CSC: [[u16; 9]; 3] = [
        [0x7af8, 0x7800, 0, 0x8b28, 0x7800, 0x9ac0, 0, 0x7800, 0x7dd8],
        [0x7c98, 0x7800, 0, 0x9ef8, 0x7800, 0xac00, 0, 0x7800, 0x7ed8],
        [0x7bc8, 0x7800, 0, 0x8928, 0x7800, 0xaa88, 0, 0x7800, 0x7f10],
    ];
    let c = &CSC[enc];
    unsafe {
        intel_de_write_dsb(
            display,
            dsb,
            PLANE_INPUT_CSC_COEFF(pipe, id, 0),
            (c[0] as u32) << 16 | c[1] as u32,
        );
        intel_de_write_dsb(
            display,
            dsb,
            PLANE_INPUT_CSC_COEFF(pipe, id, 1),
            (c[2] as u32) << 16,
        );
        intel_de_write_dsb(
            display,
            dsb,
            PLANE_INPUT_CSC_COEFF(pipe, id, 2),
            (c[3] as u32) << 16 | c[4] as u32,
        );
        intel_de_write_dsb(
            display,
            dsb,
            PLANE_INPUT_CSC_COEFF(pipe, id, 3),
            (c[5] as u32) << 16,
        );
        intel_de_write_dsb(
            display,
            dsb,
            PLANE_INPUT_CSC_COEFF(pipe, id, 4),
            (c[6] as u32) << 16 | c[7] as u32,
        );
        intel_de_write_dsb(
            display,
            dsb,
            PLANE_INPUT_CSC_COEFF(pipe, id, 5),
            (c[8] as u32) << 16,
        );
        intel_de_write_dsb(display, dsb, PLANE_INPUT_CSC_PREOFF(pipe, id, 0), 0x1800);
        intel_de_write_dsb(display, dsb, PLANE_INPUT_CSC_PREOFF(pipe, id, 1), 0);
        intel_de_write_dsb(display, dsb, PLANE_INPUT_CSC_PREOFF(pipe, id, 2), 0x1800);
        for i in 0..3 {
            intel_de_write_dsb(display, dsb, PLANE_INPUT_CSC_POSTOFF(pipe, id, i), 0);
        }
    }
}

unsafe fn icl_clear_plane_csc(dsb: *mut c_void, plane: *mut IntelPlane) {
    if plane.is_null() {
        return;
    }
    let display = unsafe { display_from_plane(plane) };
    let pipe = unsafe { (*plane).pipe as u32 };
    let id = unsafe { (*plane).id };
    unsafe {
        for i in 0..6 {
            intel_de_write_dsb(display, dsb, PLANE_CSC_COEFF(pipe, id, i), 0);
        }
        for i in 0..3 {
            intel_de_write_dsb(display, dsb, PLANE_CSC_PREOFF(pipe, id, i), 0);
            intel_de_write_dsb(display, dsb, PLANE_CSC_POSTOFF(pipe, id, i), 0);
        }
    }
}

unsafe fn icl_sel_fetch_noarm(
    dsb: *mut c_void,
    plane: *mut IntelPlane,
    cs: *const IntelCrtcState,
    state: *const IntelPlaneState,
    color: usize,
) {
    if plane.is_null() || cs.is_null() || state.is_null() || unsafe { !(*cs).enable_psr2_sel_fetch }
    {
        return;
    }
    let display = unsafe { display_from_plane(plane) };
    let pipe = unsafe { (*plane).pipe as u32 };
    let clip = unsafe { &(*state).psr2_sel_fetch_area };
    let x = unsafe { (*state).view.color_plane[color].x };
    let y = unsafe {
        let base = (*state).view.color_plane[color].y;
        if color == 0 {
            base + clip.y1.max(0) as u32
        } else {
            base + ((clip.y1.max(0) + 1) / 2) as u32
        }
    };
    let pos_y = if unsafe { (*cs).enable_psr2_su_region_et } {
        unsafe { ((*state).uapi.dst.y1 - (*cs).psr2_su_area.y1).max(0) }
    } else {
        unsafe { clip.y1 + (*state).uapi.dst.y1 }
    };
    let width = unsafe { rect_width(&(*state).uapi.src) >> 16 };
    unsafe {
        intel_de_write_dsb(
            display,
            dsb,
            SEL_FETCH_PLANE_POS(pipe, (*plane).id),
            ((pos_y as u32) << 16) | (*state).uapi.dst.x1 as u32,
        );
        intel_de_write_dsb(
            display,
            dsb,
            SEL_FETCH_PLANE_OFFSET(pipe, (*plane).id),
            ((y as u32) << 16) | x as u32,
        );
        intel_de_write_dsb(
            display,
            dsb,
            SEL_FETCH_PLANE_SIZE(pipe, (*plane).id),
            (((rect_height(clip) - 1).max(0) as u32) << 16) | (width - 1).max(0) as u32,
        );
    }
}

unsafe fn icl_sel_fetch_arm(
    dsb: *mut c_void,
    plane: *mut IntelPlane,
    cs: *const IntelCrtcState,
    state: *const IntelPlaneState,
) {
    if plane.is_null() || cs.is_null() || state.is_null() || unsafe { !(*cs).enable_psr2_sel_fetch }
    {
        return;
    }
    let display = unsafe { display_from_plane(plane) };
    let pipe = unsafe { (*plane).pipe as u32 };
    let enabled = unsafe { rect_height(&(*state).psr2_sel_fetch_area) > 0 };
    unsafe {
        intel_de_write_dsb(
            display,
            dsb,
            SEL_FETCH_PLANE_CTL(pipe, (*plane).id),
            if enabled {
                SEL_FETCH_PLANE_CTL_ENABLE
            } else {
                0
            },
        );
    }
}

/// ICL-family update path. ADLP uses this path, not the base SKL variant.
unsafe extern "C" fn icl_plane_update_noarm(
    dsb: *mut c_void,
    plane: *mut IntelPlane,
    cs: *mut IntelCrtcState,
    state: *mut IntelPlaneState,
) {
    if plane.is_null() || cs.is_null() || state.is_null() {
        return;
    }
    let display = unsafe { display_from_plane(plane) };
    let pipe = unsafe { (*plane).pipe as u32 };
    let id = unsafe { (*plane).id };
    let color = unsafe { plane_color_plane(state) };
    let scaler = unsafe { (*state).scaler_id >= 0 };
    let x = unsafe { (*state).view.color_plane[color].x };
    let y = unsafe { (*state).view.color_plane[color].y };
    let crtc_x = if scaler {
        0
    } else {
        unsafe { (*state).uapi.dst.x1 }
    };
    let crtc_y = if scaler {
        0
    } else {
        unsafe { (*state).uapi.dst.y1 }
    };
    let src_w = (unsafe { rect_width(&(*state).uapi.src) } >> 16).max(1) as u32;
    let src_h = (unsafe { rect_height(&(*state).uapi.src) } >> 16).max(1) as u32;
    unsafe {
        intel_color_plane_program_pipeline(dsb, state);
        intel_de_write_dsb(
            display,
            dsb,
            PLANE_STRIDE(pipe, id),
            PLANE_STRIDE_(plane_stride(state, color)),
        );
        intel_de_write_dsb(
            display,
            dsb,
            PLANE_POS(pipe, id),
            PLANE_POS_Y(crtc_y as u32) | PLANE_POS_X(crtc_x as u32),
        );
        intel_de_write_dsb(
            display,
            dsb,
            PLANE_SIZE(pipe, id),
            PLANE_HEIGHT(src_h - 1) | PLANE_WIDTH(src_w - 1),
        );
        intel_de_write_dsb(display, dsb, PLANE_KEYVAL(pipe, id), plane_key_val(state));
        intel_de_write_dsb(display, dsb, PLANE_KEYMSK(pipe, id), plane_key_mask(state));
        intel_de_write_dsb(display, dsb, PLANE_KEYMAX(pipe, id), plane_key_max(state));
        intel_de_write_dsb(
            display,
            dsb,
            PLANE_OFFSET(pipe, id),
            PLANE_OFFSET_Y(y as u32) | PLANE_OFFSET_X(x as u32),
        );
        let fb_modifier = (*(*state).hw.fb).modifier;
        if intel_fb_is_rc_ccs_cc_modifier(fb_modifier) {
            intel_de_write_dsb(
                display,
                dsb,
                PLANE_CC_VAL(pipe, id, 0),
                (*state).ccval as u32,
            );
            intel_de_write_dsb(
                display,
                dsb,
                PLANE_CC_VAL(pipe, id, 1),
                ((*state).ccval >> 32) as u32,
            );
        }
        if DISPLAY_VER(display) >= 12 || platform_alderlake_p(display) {
            intel_de_write_dsb(
                display,
                dsb,
                PLANE_AUX_DIST(pipe, id),
                skl_plane_aux_dist(state, color as i32),
            );
        }
        if icl_is_hdr_plane(display, id) {
            intel_de_write_dsb(display, dsb, PLANE_CUS_CTL(pipe, id), (*state).cus_ctl);
        }
        intel_de_write_dsb(
            display,
            dsb,
            PLANE_COLOR_CTL(pipe, id),
            glk_plane_color_ctl(state),
        );
        if !(*(*state).hw.fb).format.is_null()
            && (*(*(*state).hw.fb).format).is_yuv
            && icl_is_hdr_plane(display, id)
        {
            icl_program_input_csc(dsb, plane, state);
        }
        skl_write_plane_wm(dsb, plane, cs);
        if (*state).force_black {
            icl_clear_plane_csc(dsb, plane);
        }
        icl_sel_fetch_noarm(dsb, plane, cs, state, color);
    }
}

unsafe extern "C" fn icl_plane_update_arm(
    dsb: *mut c_void,
    plane: *mut IntelPlane,
    cs: *mut IntelCrtcState,
    state: *mut IntelPlaneState,
) {
    if plane.is_null() || cs.is_null() || state.is_null() {
        return;
    }
    let display = unsafe { display_from_plane(plane) };
    let pipe = unsafe { (*plane).pipe as u32 };
    let id = unsafe { (*plane).id };
    let mut ctl = unsafe { (*state).ctl };
    if unsafe { (*plane).need_async_flip_toggle_wa && (*cs).async_flip_planes & (1 << id) != 0 } {
        ctl |= PLANE_CTL_ASYNC_FLIP;
    }
    unsafe {
        if (*state).scaler_id >= 0 {
            skl_program_plane_scaler(dsb, plane, cs, state);
        }
        icl_sel_fetch_arm(dsb, plane, cs, state);
        intel_color_plane_commit_arm(dsb, state);
        intel_de_write_dsb(display, dsb, PLANE_CTL(pipe, id), ctl);
        intel_de_write_dsb(display, dsb, PLANE_SURF(pipe, id), (*state).surf);
    }
}

unsafe extern "C" fn icl_plane_disable_arm(
    dsb: *mut c_void,
    plane: *mut IntelPlane,
    cs: *mut IntelCrtcState,
) {
    if plane.is_null() || cs.is_null() {
        return;
    }
    let display = unsafe { display_from_plane(plane) };
    let pipe = unsafe { (*plane).pipe as u32 };
    let id = unsafe { (*plane).id };
    unsafe {
        if icl_is_hdr_plane(display, id) {
            intel_de_write_dsb(display, dsb, PLANE_CUS_CTL(pipe, id), 0);
        }
        skl_write_plane_wm(dsb, plane, cs);
        if (*cs).enable_psr2_sel_fetch {
            intel_de_write_dsb(display, dsb, SEL_FETCH_PLANE_CTL(pipe, id), 0);
        }
        intel_de_write_dsb(display, dsb, PLANE_CTL(pipe, id), 0);
        intel_de_write_dsb(display, dsb, PLANE_SURF(pipe, id), 0);
    }
}

unsafe extern "C" fn plane_capture_error(
    crtc: *mut IntelCrtc,
    plane: *mut IntelPlane,
    error: *mut IntelPlaneError,
) {
    if crtc.is_null() || plane.is_null() || error.is_null() {
        return;
    }
    let display = unsafe { display_from_plane(plane) };
    let pipe = unsafe { (*crtc).pipe as u32 };
    let id = unsafe { (*plane).id };
    unsafe {
        (*error).ctl = intel_de_read(display, PLANE_CTL(pipe, id));
        (*error).surf = intel_de_read(display, PLANE_SURF(pipe, id));
        (*error).surflive = intel_de_read(display, PLANE_SURFLIVE(pipe, id));
    }
}

unsafe extern "C" fn plane_get_hw_state(plane: *mut IntelPlane, pipe: *mut Pipe) -> bool {
    if plane.is_null() || pipe.is_null() {
        return false;
    }
    let display = unsafe { display_from_plane(plane) };
    let p = unsafe { (*plane).pipe };
    let domain = match p {
        0 => POWER_DOMAIN_PIPE_A,
        1 => POWER_DOMAIN_PIPE_B,
        2 => POWER_DOMAIN_PIPE_C,
        3 => POWER_DOMAIN_PIPE_D,
        _ => return false,
    };
    let wakeref = unsafe { intel_display_power_get_if_enabled(display, domain) };
    if wakeref.is_null() {
        return false;
    }
    let ctl = unsafe { intel_de_read(display, PLANE_CTL(p as u32, (*plane).id)) };
    unsafe {
        *pipe = p;
        intel_display_power_put(display, domain, wakeref);
    }
    ctl & PLANE_CTL_ENABLE != 0
}

unsafe fn plane_max_stride(
    _plane: *mut IntelPlane,
    info: *mut DrmFormatInfo,
    _modifier: u64,
    rotation: u32,
) -> u32 {
    if info.is_null() {
        return 0;
    }
    let cpp = format_cpp(info, 0);
    if cpp == 0 {
        return 0;
    }
    if rotation & (DRM_MODE_ROTATE_90 | DRM_MODE_ROTATE_270) != 0 {
        65_536u32.min(128 * 1024 / cpp)
    } else {
        (65_536u32.saturating_mul(cpp)).min(128 * 1024)
    }
}

unsafe fn plane_min_width(fb: *mut DrmFramebuffer, color_plane: i32, _rotation: u32) -> i32 {
    if fb.is_null() || color_plane < 0 {
        return 1;
    }
    let cpp = unsafe { format_cpp((*fb).format, color_plane as usize) }.max(1);
    (16 / cpp) as i32
}

unsafe fn plane_max_width(fb: *mut DrmFramebuffer, color_plane: i32, _rotation: u32) -> i32 {
    let _ = (fb, color_plane);
    5120
}

unsafe fn hdr_plane_max_width(fb: *mut DrmFramebuffer, _color_plane: i32, _rotation: u32) -> i32 {
    if fb.is_null() {
        return 5120;
    }
    let fmt = unsafe { (*fb).format };
    if !fmt.is_null() && semiplanar(unsafe { (*fmt).format }) {
        4096
    } else {
        5120
    }
}

unsafe fn plane_max_height(_fb: *mut DrmFramebuffer, _color_plane: i32, _rotation: u32) -> i32 {
    4320
}

unsafe fn plane_min_alignment(
    plane: *mut IntelPlane,
    fb: *mut DrmFramebuffer,
    color_plane: i32,
) -> u32 {
    if fb.is_null() {
        return 0;
    }
    let mult = if unsafe { intel_fb_uses_dpt(fb) } {
        512
    } else {
        1
    };
    if unsafe { intel_fb_is_ccs_aux_plane(fb, color_plane) } {
        return mult * 4096;
    }
    if unsafe {
        platform_alderlake_p(display_from_plane(plane))
            && intel_plane_can_async_flip(plane, (*fb).format, (*fb).modifier)
    } {
        return mult * 16 * 1024;
    }
    match unsafe { (*fb).modifier } {
        DRM_FORMAT_MOD_LINEAR
        | I915_FORMAT_MOD_X_TILED
        | I915_FORMAT_MOD_Y_TILED
        | I915_FORMAT_MOD_4_TILED => mult * 4096,
        I915_FORMAT_MOD_Y_TILED_GEN12_RC_CCS
        | I915_FORMAT_MOD_Y_TILED_GEN12_MC_CCS
        | I915_FORMAT_MOD_Y_TILED_GEN12_RC_CCS_CC
        | I915_FORMAT_MOD_4_TILED_MTL_RC_CCS
        | I915_FORMAT_MOD_4_TILED_MTL_MC_CCS
        | I915_FORMAT_MOD_4_TILED_MTL_RC_CCS_CC
        | I915_FORMAT_MOD_4_TILED_DG2_RC_CCS
        | I915_FORMAT_MOD_4_TILED_DG2_MC_CCS
        | I915_FORMAT_MOD_4_TILED_DG2_RC_CCS_CC
        | I915_FORMAT_MOD_4_TILED_BMG_CCS
        | I915_FORMAT_MOD_4_TILED_LNL_CCS => (mult * 16 * 1024).max(16 * 1024),
        _ => 0,
    }
}

fn ccs_modifier(modifier: u64) -> bool {
    matches!(
        modifier,
        I915_FORMAT_MOD_Y_TILED_CCS
            | I915_FORMAT_MOD_Yf_TILED_CCS
            | I915_FORMAT_MOD_Y_TILED_GEN12_RC_CCS
            | I915_FORMAT_MOD_Y_TILED_GEN12_MC_CCS
            | I915_FORMAT_MOD_Y_TILED_GEN12_RC_CCS_CC
            | I915_FORMAT_MOD_4_TILED_DG2_RC_CCS
            | I915_FORMAT_MOD_4_TILED_DG2_MC_CCS
            | I915_FORMAT_MOD_4_TILED_DG2_RC_CCS_CC
            | I915_FORMAT_MOD_4_TILED_MTL_RC_CCS
            | I915_FORMAT_MOD_4_TILED_MTL_MC_CCS
            | I915_FORMAT_MOD_4_TILED_MTL_RC_CCS_CC
            | I915_FORMAT_MOD_4_TILED_BMG_CCS
            | I915_FORMAT_MOD_4_TILED_LNL_CCS
    )
}

unsafe fn check_main_surface(state: *mut IntelPlaneState) -> i32 {
    if state.is_null() || unsafe { (*state).hw.fb.is_null() } {
        return -tk_linux_kapi::config::EINVAL;
    }
    let plane = unsafe { plane_from_drm((*state).uapi.plane) };
    let fb = unsafe { (*state).hw.fb };
    let fmt = unsafe { (*fb).format };
    if fmt.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let x0 = unsafe { (*state).uapi.src.x1 >> 16 };
    let y0 = unsafe { (*state).uapi.src.y1 >> 16 };
    let w = unsafe { rect_width(&(*state).uapi.src) >> 16 };
    let h = unsafe { rect_height(&(*state).uapi.src) >> 16 };
    let cpp = format_cpp(fmt, 0).max(1) as i32;
    let min_w = unsafe { plane_min_width(fb, 0, (*state).hw.rotation) };
    let max_w = unsafe {
        (*plane)
            .max_width
            .map_or(5120, |f| f(fb, 0, (*state).hw.rotation))
    };
    if w < min_w
        || w > max_w
        || h < 1
        || h > unsafe { plane_max_height(fb, 0, (*state).hw.rotation) }
    {
        return -tk_linux_kapi::config::EINVAL;
    }
    let mut x = x0;
    let mut y = y0;
    unsafe {
        intel_add_fb_offsets(&mut x, &mut y, state, 0);
    }
    let mut offset = unsafe { intel_plane_compute_aligned_offset(&mut x, &mut y, state, 0) };
    let alignment = unsafe { plane_min_alignment(plane, fb, 0) };
    if alignment != 0 && !alignment.is_power_of_two() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let aux = unsafe { skl_main_to_aux_plane(fb, 0) };
    if aux > 0 {
        let aux_offset = unsafe { (*state).view.color_plane[aux as usize].offset };
        if offset > aux_offset && alignment != 0 {
            offset = unsafe {
                intel_plane_adjust_aligned_offset(
                    &mut x,
                    &mut y,
                    state,
                    0,
                    offset,
                    aux_offset & !(alignment - 1),
                )
            };
        }
    }
    if unsafe { (*fb).modifier == I915_FORMAT_MOD_X_TILED } {
        while (x + w).saturating_mul(cpp) as u32
            > unsafe { (*state).view.color_plane[0].mapping_stride }
        {
            if offset == 0 {
                return -tk_linux_kapi::config::EINVAL;
            }
            offset = unsafe {
                intel_plane_adjust_aligned_offset(
                    &mut x,
                    &mut y,
                    state,
                    0,
                    offset,
                    offset.saturating_sub(alignment),
                )
            };
        }
    }
    if unsafe { DISPLAY_VER(display_from_plane(plane)) >= 13 } && (x > 65535 || y > 65535) {
        return -tk_linux_kapi::config::ERANGE;
    }
    unsafe {
        (*state).view.color_plane[0].offset = offset;
        (*state).view.color_plane[0].x = x as u32;
        (*state).view.color_plane[0].y = y as u32;
        (*state).uapi.src.x1 += (x - x0) << 16;
        (*state).uapi.src.y1 += (y - y0) << 16;
        (*state).uapi.src.x2 += (x - x0) << 16;
        (*state).uapi.src.y2 += (y - y0) << 16;
    }
    0
}

unsafe fn check_nv12_surface(state: *mut IntelPlaneState) -> i32 {
    if state.is_null() || unsafe { (*state).hw.fb.is_null() } {
        return -tk_linux_kapi::config::EINVAL;
    }
    let fb = unsafe { (*state).hw.fb };
    let mut x = unsafe { ((*state).uapi.src.x1 >> 16) / 2 };
    let mut y = unsafe { ((*state).uapi.src.y1 >> 16) / 2 };
    let x2 = unsafe { ((*state).uapi.src.x2 + 0xffff) >> 17 };
    let y2 = unsafe { ((*state).uapi.src.y2 + 0xffff) >> 17 };
    let w = x2 - x;
    let h = y2 - y;
    let plane = unsafe { plane_from_drm((*state).uapi.plane) };
    if w <= 0
        || h <= 0
        || w > unsafe {
            (*plane)
                .max_width
                .map_or(5120, |f| f(fb, 1, (*state).hw.rotation))
        }
        || h > unsafe { plane_max_height(fb, 1, (*state).hw.rotation) }
    {
        return -tk_linux_kapi::config::EINVAL;
    }
    unsafe {
        intel_add_fb_offsets(&mut x, &mut y, state, 1);
    }
    let offset = unsafe { intel_plane_compute_aligned_offset(&mut x, &mut y, state, 1) };
    unsafe {
        (*state).view.color_plane[1].offset = offset;
        (*state).view.color_plane[1].x = x.max(0) as u32;
        (*state).view.color_plane[1].y = y.max(0) as u32;
    }
    0
}

unsafe fn check_plane_surface(state: *mut IntelPlaneState) -> i32 {
    let ret = unsafe { intel_plane_compute_gtt(state) };
    if ret != 0 || !unsafe { (*state).uapi.visible } {
        return ret;
    }
    let fb = unsafe { (*state).hw.fb };
    if fb.is_null() {
        return 0;
    }
    let fmt = unsafe { (*fb).format };
    if !fmt.is_null() && semiplanar(unsafe { (*fmt).format }) {
        let ret = unsafe { check_nv12_surface(state) };
        if ret != 0 {
            return ret;
        }
    }
    unsafe { check_main_surface(state) }
}

unsafe fn check_fb(crtc_state: *const IntelCrtcState, state: *const IntelPlaneState) -> i32 {
    if state.is_null() || unsafe { (*state).hw.fb.is_null() } {
        return 0;
    }
    let fb = unsafe { (*state).hw.fb };
    let rotation = unsafe { (*state).hw.rotation };
    if rotation & !(DRM_MODE_ROTATE_0 | DRM_MODE_ROTATE_180 | DRM_MODE_REFLECT_X) != 0
        && unsafe { intel_fb_is_ccs_modifier((*fb).modifier) }
    {
        return -tk_linux_kapi::config::EINVAL;
    }
    if rotation & DRM_MODE_REFLECT_X != 0
        && unsafe {
            (*fb).modifier == DRM_FORMAT_MOD_LINEAR
                && DISPLAY_VER(display_from_plane(plane_from_drm((*state).uapi.plane))) < 35
        }
    {
        return -tk_linux_kapi::config::EINVAL;
    }
    if rotation & (DRM_MODE_ROTATE_90 | DRM_MODE_ROTATE_270) != 0 {
        let intel_fb = fb.cast::<IntelFramebuffer>();
        if !unsafe { intel_fb_supports_90_270_rotation(intel_fb) } {
            return -tk_linux_kapi::config::EINVAL;
        }
    }
    if !crtc_state.is_null()
        && unsafe {
            (*crtc_state).hw.adjusted_mode.flags & DRM_MODE_FLAG_INTERLACE != 0
                && (*fb).modifier != DRM_FORMAT_MOD_LINEAR
                && (*fb).modifier != I915_FORMAT_MOD_X_TILED
        }
    {
        return -tk_linux_kapi::config::EINVAL;
    }
    0
}

unsafe fn plane_check(state: *mut IntelCrtcState, plane_state: *mut IntelPlaneState) -> i32 {
    if state.is_null() || plane_state.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let display = unsafe { display_from_plane(plane_from_drm((*plane_state).uapi.plane)) };
    let fb = unsafe { (*plane_state).hw.fb };
    let ret = unsafe { check_fb(state, plane_state) };
    if ret != 0 {
        return ret;
    }
    if fb.is_null() {
        unsafe {
            (*plane_state).uapi.visible = false;
            return 0;
        }
    }
    let fmt = unsafe { (*fb).format };
    if fmt.is_null()
        || plane_ctl_format(unsafe { (*fmt).format }).is_none()
        || plane_ctl_tiling(unsafe { (*fb).modifier }).is_none()
    {
        return -tk_linux_kapi::config::EINVAL;
    }
    let scalable = unsafe { (*fmt).format != DRM_FORMAT_C8 };
    let (min_scale, max_scale) = if scalable {
        (1, 0x30000 - 1)
    } else {
        (1 << 16, 1 << 16)
    };
    let ret = unsafe { intel_plane_check_clipping(plane_state, state, min_scale, max_scale, true) };
    if ret != 0 {
        return ret;
    }
    let ret = unsafe { check_plane_surface(plane_state) };
    if ret != 0 {
        return ret;
    }
    if !unsafe { (*plane_state).uapi.visible } {
        return 0;
    }
    let ret = unsafe { intel_plane_check_src_coordinates(plane_state) };
    if ret != 0 {
        return ret;
    }
    let ret = unsafe { icl_plane_check_nv12_rotation(plane_state) };
    if ret != 0 {
        return ret;
    }
    let ctl = unsafe { skl_plane_ctl(plane_state) };
    let Some(ctl) = ctl else {
        return -tk_linux_kapi::config::EINVAL;
    };
    unsafe {
        (*plane_state).ctl = ctl;
        (*plane_state).color_ctl = glk_plane_color_ctl(plane_state);
        (*plane_state).cus_ctl = if semiplanar((*fmt).format)
            && icl_is_hdr_plane(display, (*plane_from_drm((*plane_state).uapi.plane)).id)
        {
            PLANE_CUS_ENABLE
                | PLANE_CUS_HPHASE_0
                | PLANE_CUS_VPHASE_SIGN_NEGATIVE
                | PLANE_CUS_VPHASE_0_25
        } else {
            0
        };
        if (*plane_state).hw.alpha >> 8 == 0 {
            (*plane_state).uapi.visible = false;
        }
    }
    0
}

unsafe fn icl_plane_check_nv12_rotation(state: *const IntelPlaneState) -> i32 {
    if state.is_null() || unsafe { (*state).hw.fb.is_null() } {
        return 0;
    }
    let fmt = unsafe { (*(*state).hw.fb).format };
    if !fmt.is_null() && semiplanar(unsafe { (*fmt).format }) {
        let sw = unsafe { rect_width(&(*state).uapi.src) >> 16 };
        let rot = unsafe { (*state).hw.rotation };
        if sw & 3 != 0
            && (rot == DRM_MODE_ROTATE_270 || rot == DRM_MODE_REFLECT_X | DRM_MODE_ROTATE_90)
        {
            return -tk_linux_kapi::config::EINVAL;
        }
    }
    0
}

unsafe extern "C" fn plane_check_callback(
    crtc_state: *mut IntelCrtcState,
    state: *mut IntelPlaneState,
) -> i32 {
    unsafe { plane_check(crtc_state, state) }
}

unsafe extern "C" fn plane_format_mod_supported(
    plane: *mut DrmPlane,
    format: u32,
    modifier: u64,
) -> bool {
    if plane.is_null() {
        return false;
    }
    let intel_plane = unsafe { plane_from_drm(plane) };
    if !unsafe { intel_fb_plane_supports_modifier(intel_plane, modifier) } {
        return false;
    }
    let basic = matches!(
        format,
        DRM_FORMAT_XRGB8888
            | DRM_FORMAT_XBGR8888
            | DRM_FORMAT_ARGB8888
            | DRM_FORMAT_ABGR8888
            | DRM_FORMAT_XRGB2101010
            | DRM_FORMAT_XBGR2101010
            | DRM_FORMAT_ARGB2101010
            | DRM_FORMAT_ABGR2101010
            | DRM_FORMAT_RGB565
            | DRM_FORMAT_YUYV
            | DRM_FORMAT_YVYU
            | DRM_FORMAT_UYVY
            | DRM_FORMAT_VYUY
            | DRM_FORMAT_NV12
            | DRM_FORMAT_XYUV8888
            | DRM_FORMAT_P010
            | DRM_FORMAT_P012
            | DRM_FORMAT_P016
            | DRM_FORMAT_XVYU2101010
            | DRM_FORMAT_C8
            | DRM_FORMAT_XBGR16161616F
            | DRM_FORMAT_ABGR16161616F
            | DRM_FORMAT_XRGB16161616F
            | DRM_FORMAT_ARGB16161616F
            | DRM_FORMAT_Y210
            | DRM_FORMAT_Y212
            | DRM_FORMAT_Y216
            | DRM_FORMAT_XVYU12_16161616
            | DRM_FORMAT_XVYU16161616
    );
    if !basic {
        return false;
    }
    if unsafe { intel_fb_is_ccs_modifier(modifier) } {
        return matches!(
            format,
            DRM_FORMAT_XRGB8888
                | DRM_FORMAT_XBGR8888
                | DRM_FORMAT_ARGB8888
                | DRM_FORMAT_ABGR8888
                | DRM_FORMAT_XRGB2101010
                | DRM_FORMAT_XBGR2101010
                | DRM_FORMAT_ARGB2101010
                | DRM_FORMAT_ABGR2101010
        );
    }
    modifier == DRM_FORMAT_MOD_LINEAR
        || modifier == I915_FORMAT_MOD_X_TILED
        || modifier == I915_FORMAT_MOD_Y_TILED
        || modifier == I915_FORMAT_MOD_Yf_TILED
        || modifier == I915_FORMAT_MOD_4_TILED
}

unsafe extern "C" fn plane_format_mod_supported_async(
    plane: *mut DrmPlane,
    format: u32,
    modifier: u64,
) -> bool {
    unsafe { intel_plane_format_mod_supported_async(plane, format, modifier) }
}

unsafe extern "C" fn tgl_plane_can_async_flip(modifier: u64) -> bool {
    matches!(
        modifier,
        DRM_FORMAT_MOD_LINEAR
            | I915_FORMAT_MOD_X_TILED
            | I915_FORMAT_MOD_Y_TILED
            | I915_FORMAT_MOD_4_TILED
            | I915_FORMAT_MOD_Y_TILED_GEN12_RC_CCS
            | I915_FORMAT_MOD_4_TILED_MTL_RC_CCS
            | I915_FORMAT_MOD_4_TILED_DG2_RC_CCS
            | I915_FORMAT_MOD_4_TILED_BMG_CCS
            | I915_FORMAT_MOD_4_TILED_LNL_CCS
    )
}

unsafe extern "C" fn plane_min_width_cb(
    fb: *mut DrmFramebuffer,
    color_plane: i32,
    rotation: u32,
) -> i32 {
    unsafe { plane_min_width(fb, color_plane, rotation) }
}
unsafe extern "C" fn plane_max_width_cb(
    fb: *mut DrmFramebuffer,
    color_plane: i32,
    rotation: u32,
) -> i32 {
    unsafe { plane_max_width(fb, color_plane, rotation) }
}
unsafe extern "C" fn hdr_plane_max_width_cb(
    fb: *mut DrmFramebuffer,
    color_plane: i32,
    rotation: u32,
) -> i32 {
    unsafe { hdr_plane_max_width(fb, color_plane, rotation) }
}
unsafe extern "C" fn plane_max_height_cb(
    fb: *mut DrmFramebuffer,
    color_plane: i32,
    rotation: u32,
) -> i32 {
    unsafe { plane_max_height(fb, color_plane, rotation) }
}
unsafe extern "C" fn plane_min_alignment_cb(
    plane: *mut IntelPlane,
    fb: *mut DrmFramebuffer,
    color_plane: i32,
) -> u32 {
    unsafe { plane_min_alignment(plane, fb, color_plane) }
}
unsafe extern "C" fn plane_max_stride_cb(
    plane: *mut IntelPlane,
    fmt: *mut DrmFormatInfo,
    modifier: u64,
    rotation: u32,
) -> u32 {
    unsafe { plane_max_stride(plane, fmt, modifier, rotation) }
}

unsafe extern "C" fn plane_min_cdclk(
    crtc: *mut IntelCrtcState,
    state: *mut IntelPlaneState,
) -> i32 {
    if crtc.is_null() || state.is_null() {
        return 0;
    }
    let pixel_rate = unsafe { intel_plane_pixel_rate(crtc, state) };
    ((pixel_rate >> 1) + (pixel_rate & 1)) as i32
}

unsafe extern "C" fn plane_surf_offset_cb(state: *mut IntelPlaneState) -> u32 {
    unsafe { skl_plane_surf_offset(state) }
}

unsafe extern "C" fn plane_async_flip(
    dsb: *mut c_void,
    plane: *mut IntelPlane,
    crtc: *mut IntelCrtcState,
    state: *mut IntelPlaneState,
    async_flip: bool,
) {
    if plane.is_null() || crtc.is_null() || state.is_null() {
        return;
    }
    let display = unsafe { display_from_plane(plane) };
    let pipe = unsafe { (*plane).pipe as u32 };
    let id = unsafe { (*plane).id };
    let mut ctl = unsafe { (*state).ctl };
    let mut surf = unsafe { (*state).surf };
    if async_flip {
        if unsafe { DISPLAY_VER(display) >= 30 } {
            surf |= PLANE_SURF_ASYNC_UPDATE;
        } else {
            ctl |= PLANE_CTL_ASYNC_FLIP;
        }
    }
    unsafe {
        intel_de_write_dsb(display, dsb, PLANE_CTL(pipe, id), ctl);
        intel_de_write_dsb(display, dsb, PLANE_SURF(pipe, id), surf);
    }
}

unsafe extern "C" fn plane_enable_flip_done(plane: *mut IntelPlane) {
    if plane.is_null() {
        return;
    }
    let display = unsafe { display_from_plane(plane) };
    let pipe = unsafe { (*plane).pipe };
    let bits = GEN9_PIPE_PLANE_FLIP_DONE(unsafe { (*plane).id });
    unsafe {
        crate::linux::locks::spin_lock_irq(&mut (*display).irq.lock);
        bdw_enable_pipe_irq(display, pipe, bits);
        crate::linux::locks::spin_unlock_irq(&mut (*display).irq.lock);
    }
}
unsafe extern "C" fn plane_disable_flip_done(plane: *mut IntelPlane) {
    if plane.is_null() {
        return;
    }
    let display = unsafe { display_from_plane(plane) };
    let pipe = unsafe { (*plane).pipe };
    let bits = GEN9_PIPE_PLANE_FLIP_DONE(unsafe { (*plane).id });
    unsafe {
        crate::linux::locks::spin_lock_irq(&mut (*display).irq.lock);
        bdw_disable_pipe_irq(display, pipe, bits);
        crate::linux::locks::spin_unlock_irq(&mut (*display).irq.lock);
    }
}
unsafe extern "C" fn plane_disable_tiling(plane: *mut IntelPlane) {
    if plane.is_null() || unsafe { (*plane).base.state.is_null() } {
        return;
    }
    let state = unsafe { state_from_drm((*plane).base.state) };
    if unsafe { (*state).hw.fb.is_null() } {
        return;
    }
    let display = unsafe { display_from_plane(plane) };
    let pipe = unsafe { (*plane).pipe as u32 };
    let id = unsafe { (*plane).id };
    let mut ctl = unsafe { intel_de_read(display, PLANE_CTL(pipe, id)) };
    if unsafe { intel_fb_uses_dpt((*state).hw.fb) } {
        ctl &= !PLANE_CTL_RENDER_DECOMPRESSION_ENABLE;
    } else {
        let stride = unsafe { (*state).view.color_plane[0].scanout_stride / 64 };
        ctl &= !PLANE_CTL_TILED_MASK;
        unsafe {
            crate::intel_de_upstream::intel_de_write_fw(
                display,
                PLANE_STRIDE(pipe, id),
                PLANE_STRIDE_(stride),
            );
        }
    }
    unsafe {
        crate::intel_de_upstream::intel_de_write_fw(display, PLANE_CTL(pipe, id), ctl);
    }
}

static PLANE_FUNCS: DrmPlaneFuncs = DrmPlaneFuncs {
    update_plane: Some(tk_drm_upstream::drm_atomic_helper_upstream::drm_atomic_helper_update_plane),
    disable_plane: Some(
        tk_drm_upstream::drm_atomic_helper_upstream::drm_atomic_helper_disable_plane,
    ),
    destroy: Some(intel_plane_destroy),
    reset: None,
    set_property: None,
    atomic_duplicate_state: Some(intel_plane_duplicate_state),
    atomic_destroy_state: Some(intel_plane_destroy_state),
    atomic_set_property: None,
    atomic_get_property: None,
    late_register: None,
    early_unregister: None,
    atomic_print_state: None,
    format_mod_supported: Some(plane_format_mod_supported),
    format_mod_supported_async: Some(plane_format_mod_supported_async),
};

static PLANE_HELPER_FUNCS: DrmPlaneHelperFuncs = DrmPlaneHelperFuncs {
    prepare_fb: Some(intel_prepare_plane_fb),
    cleanup_fb: Some(intel_cleanup_plane_fb),
    begin_fb_access: None,
    end_fb_access: None,
    atomic_check: None,
    atomic_update: None,
    atomic_enable: None,
    atomic_disable: None,
    atomic_async_check: None,
    atomic_async_update: None,
    get_scanout_buffer: None,
    panic_flush: None,
};

static PRIMARY_PLANE_HELPER_FUNCS: DrmPlaneHelperFuncs = DrmPlaneHelperFuncs {
    prepare_fb: Some(intel_prepare_plane_fb),
    cleanup_fb: Some(intel_cleanup_plane_fb),
    begin_fb_access: None,
    end_fb_access: None,
    atomic_check: None,
    atomic_update: None,
    atomic_enable: None,
    atomic_disable: None,
    atomic_async_check: None,
    atomic_async_update: None,
    get_scanout_buffer: Some(intel_get_scanout_buffer),
    panic_flush: Some(intel_panic_flush),
};

// upstream: intel_plane.c intel_plane_helper_add()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_plane_helper_add(plane: *mut IntelPlane) {
    if plane.is_null() {
        return;
    }
    unsafe {
        let funcs = if (*plane).base.r#type == DRM_PLANE_TYPE_PRIMARY {
            &PRIMARY_PLANE_HELPER_FUNCS
        } else {
            &PLANE_HELPER_FUNCS
        };
        drm_plane_helper_add(ptr::addr_of_mut!((*plane).base), funcs);
    }
}

// upstream: skl_universal_plane.c skl_universal_plane_create() for ADLP/13+
#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_universal_plane_create(
    display: *mut IntelDisplay,
    pipe: Pipe,
    plane_id: PlaneId,
) -> *mut IntelPlane {
    if display.is_null() || unsafe { (*display).drm.is_null() || DISPLAY_VER(display) < 13 } {
        return tk_linux_kapi::config::ERR_PTR(-tk_linux_kapi::config::EINVAL);
    }
    let plane = unsafe { intel_plane_alloc() };
    if plane.is_null() || tk_linux_kapi::config::IS_ERR(plane.cast::<c_void>()) {
        return plane;
    }
    unsafe {
        (*plane).pipe = pipe;
        (*plane).id = plane_id;
        (*plane).fbc = skl_plane_fbc(display, pipe, plane_id);
        (*plane).frontbuffer_bit = 1u32 << ((pipe as u32) * 8 + plane_id);
        (*plane).min_width = Some(plane_min_width_cb);
        (*plane).max_width = Some(if icl_is_hdr_plane(display, plane_id) {
            hdr_plane_max_width_cb
        } else {
            plane_max_width_cb
        });
        (*plane).max_height = Some(plane_max_height_cb);
        (*plane).min_alignment = Some(plane_min_alignment_cb);
        (*plane).max_stride = Some(plane_max_stride_cb);
        (*plane).can_async_flip = if plane_id == PLANE_1 {
            Some(tgl_plane_can_async_flip)
        } else {
            None
        };
        (*plane).min_cdclk = Some(plane_min_cdclk);
        (*plane).surf_offset = Some(plane_surf_offset_cb);
        if DISPLAY_VER(display) >= 11 {
            (*plane).update_noarm = Some(icl_plane_update_noarm);
            (*plane).update_arm = Some(icl_plane_update_arm);
            (*plane).disable_arm = Some(icl_plane_disable_arm);
        } else {
            (*plane).update_noarm = Some(skl_plane_update_noarm);
            (*plane).update_arm = Some(skl_plane_update_arm);
            (*plane).disable_arm = Some(skl_plane_disable_arm);
        }
        (*plane).capture_error = Some(plane_capture_error);
        (*plane).get_hw_state = Some(plane_get_hw_state);
        (*plane).check_plane = Some(plane_check_callback);
        (*plane).async_flip = if plane_id == PLANE_1 {
            Some(plane_async_flip)
        } else {
            None
        };
        (*plane).enable_flip_done = if plane_id == PLANE_1 {
            Some(plane_enable_flip_done)
        } else {
            None
        };
        (*plane).disable_flip_done = if plane_id == PLANE_1 {
            Some(plane_disable_flip_done)
        } else {
            None
        };
        (*plane).disable_tiling = Some(plane_disable_tiling);
    }
    let display_wa_mask = unsafe { (*display).info.__device_info };
    if !display_wa_mask.is_null() && unsafe { (*display_wa_mask)._bitfields_46[0] & (1 << 6) != 0 }
    {
        unsafe {
            (*plane).vtd_guard = 168;
        }
    }
    let formats = if unsafe { icl_is_hdr_plane(display, plane_id) } {
        &ICL_HDR_FORMATS[..]
    } else if unsafe { icl_is_nv12_y_plane(display, plane_id) } {
        &ICL_SDR_Y_FORMATS[..]
    } else {
        &ICL_SDR_UV_FORMATS[..]
    };
    let caps = 0x01 | 0x02 | 0x04 | 0x08 | 0x10;
    let modifiers = unsafe { intel_fb_plane_get_modifiers(display, caps) };
    if modifiers.is_null() {
        unsafe {
            intel_plane_free(plane);
        }
        return tk_linux_kapi::config::ERR_PTR(-tk_linux_kapi::config::ENOMEM);
    }
    let name = alloc::format!("plane {}{}\0", plane_id + 1, (b'A' + pipe as u8) as char);
    let ret = unsafe {
        tk_drm_upstream::drm_plane_upstream::drm_universal_plane_init(
            (*display).drm,
            ptr::addr_of_mut!((*plane).base),
            0,
            &PLANE_FUNCS,
            formats.as_ptr(),
            formats.len() as u32,
            modifiers,
            if plane_id == PLANE_1 {
                DRM_PLANE_TYPE_PRIMARY
            } else {
                DRM_PLANE_TYPE_OVERLAY
            },
            name.as_ptr() as *const c_char,
        )
    };
    unsafe {
        tk_linux_kapi::memory::kfree(modifiers.cast::<c_void>());
    }
    if ret != 0 {
        unsafe {
            intel_plane_free(plane);
        }
        return tk_linux_kapi::config::ERR_PTR(ret);
    }

    let rotations = DRM_MODE_ROTATE_0 | DRM_MODE_ROTATE_180 | DRM_MODE_REFLECT_X;
    unsafe {
        tk_drm_upstream::drm_blend_upstream::drm_plane_create_rotation_property(
            ptr::addr_of_mut!((*plane).base),
            DRM_MODE_ROTATE_0,
            rotations,
        );
        tk_drm_upstream::drm_color_mgmt_upstream::drm_plane_create_color_properties(
            ptr::addr_of_mut!((*plane).base),
            (1 << DRM_COLOR_YCBCR_BT601)
                | (1 << DRM_COLOR_YCBCR_BT709)
                | (1 << DRM_COLOR_YCBCR_BT2020),
            (1 << DRM_COLOR_YCBCR_LIMITED_RANGE) | (1 << DRM_COLOR_YCBCR_FULL_RANGE),
            DRM_COLOR_YCBCR_BT709,
            DRM_COLOR_YCBCR_LIMITED_RANGE,
        );
        tk_drm_upstream::drm_blend_upstream::drm_plane_create_alpha_property(ptr::addr_of_mut!(
            (*plane).base
        ));
        tk_drm_upstream::drm_blend_upstream::drm_plane_create_blend_mode_property(
            ptr::addr_of_mut!((*plane).base),
            (1 << 0) | (1 << 1) | (1 << 2),
        );
        tk_drm_upstream::drm_blend_upstream::drm_plane_create_zpos_immutable_property(
            ptr::addr_of_mut!((*plane).base),
            plane_id,
        );
        tk_drm_upstream::drm_plane_upstream::drm_plane_enable_fb_damage_clips(ptr::addr_of_mut!(
            (*plane).base
        ));
        tk_drm_upstream::drm_plane_upstream::drm_plane_create_scaling_filter_property(
            ptr::addr_of_mut!((*plane).base),
            (1 << 0) | (1 << 1),
        );
        intel_plane_helper_add(plane);
    }
    plane
}

static ICL_HDR_FORMATS: [u32; 29] = [
    DRM_FORMAT_C8,
    DRM_FORMAT_RGB565,
    DRM_FORMAT_XRGB8888,
    DRM_FORMAT_XBGR8888,
    DRM_FORMAT_ARGB8888,
    DRM_FORMAT_ABGR8888,
    DRM_FORMAT_XRGB2101010,
    DRM_FORMAT_XBGR2101010,
    DRM_FORMAT_ARGB2101010,
    DRM_FORMAT_ABGR2101010,
    DRM_FORMAT_XRGB16161616F,
    DRM_FORMAT_XBGR16161616F,
    DRM_FORMAT_ARGB16161616F,
    DRM_FORMAT_ABGR16161616F,
    DRM_FORMAT_YUYV,
    DRM_FORMAT_YVYU,
    DRM_FORMAT_UYVY,
    DRM_FORMAT_VYUY,
    DRM_FORMAT_NV12,
    DRM_FORMAT_P010,
    DRM_FORMAT_P012,
    DRM_FORMAT_P016,
    DRM_FORMAT_Y210,
    DRM_FORMAT_Y212,
    DRM_FORMAT_Y216,
    DRM_FORMAT_XYUV8888,
    DRM_FORMAT_XVYU2101010,
    DRM_FORMAT_XVYU12_16161616,
    DRM_FORMAT_XVYU16161616,
];
static ICL_SDR_Y_FORMATS: [u32; 19] = [
    DRM_FORMAT_C8,
    DRM_FORMAT_RGB565,
    DRM_FORMAT_XRGB8888,
    DRM_FORMAT_XBGR8888,
    DRM_FORMAT_ARGB8888,
    DRM_FORMAT_ABGR8888,
    DRM_FORMAT_XRGB2101010,
    DRM_FORMAT_XBGR2101010,
    DRM_FORMAT_ARGB2101010,
    DRM_FORMAT_ABGR2101010,
    DRM_FORMAT_YUYV,
    DRM_FORMAT_YVYU,
    DRM_FORMAT_UYVY,
    DRM_FORMAT_VYUY,
    DRM_FORMAT_Y210,
    DRM_FORMAT_Y212,
    DRM_FORMAT_Y216,
    DRM_FORMAT_XYUV8888,
    DRM_FORMAT_XVYU2101010,
];
static ICL_SDR_UV_FORMATS: [u32; 23] = [
    DRM_FORMAT_C8,
    DRM_FORMAT_RGB565,
    DRM_FORMAT_XRGB8888,
    DRM_FORMAT_XBGR8888,
    DRM_FORMAT_ARGB8888,
    DRM_FORMAT_ABGR8888,
    DRM_FORMAT_XRGB2101010,
    DRM_FORMAT_XBGR2101010,
    DRM_FORMAT_ARGB2101010,
    DRM_FORMAT_ABGR2101010,
    DRM_FORMAT_YUYV,
    DRM_FORMAT_YVYU,
    DRM_FORMAT_UYVY,
    DRM_FORMAT_VYUY,
    DRM_FORMAT_NV12,
    DRM_FORMAT_P010,
    DRM_FORMAT_P012,
    DRM_FORMAT_P016,
    DRM_FORMAT_Y210,
    DRM_FORMAT_Y212,
    DRM_FORMAT_Y216,
    DRM_FORMAT_XYUV8888,
    DRM_FORMAT_XVYU2101010,
];

// upstream: skl_universal_plane.c skl_get_initial_plane_config()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_get_initial_plane_config(
    crtc: *mut IntelCrtc,
    config: *mut IntelInitialPlaneConfig,
) {
    if crtc.is_null() || config.is_null() {
        return;
    }
    let display = unsafe {
        let dev = (*crtc).base.dev;
        if dev.is_null() || (*dev).dev_private.is_null() {
            return;
        }
        let i915 = (*dev)
            .dev_private
            .cast::<crate::linux_i915_private::DrmI915Private>();
        (*i915).display.cast::<IntelDisplay>()
    };
    let primary = unsafe { (*crtc).base.primary };
    if primary.is_null() {
        return;
    }
    if unsafe { (*crtc).base.state.is_null() } {
        return;
    }
    let crtc_state = unsafe { container_of!((*crtc).base.state, IntelCrtcState, uapi) };
    if unsafe { (*crtc_state).joiner_pipes != 0 } {
        return;
    }
    let plane = unsafe { plane_from_drm(primary) };
    let mut pipe = unsafe { (*crtc).pipe };
    if !unsafe {
        (*plane)
            .get_hw_state
            .is_some_and(|get| get(plane, &mut pipe))
    } {
        return;
    }
    let fb = unsafe { intel_framebuffer_alloc() };
    if fb.is_null() {
        return;
    }
    unsafe {
        (*fb).base.dev = (*display).drm;
        let ctl = intel_de_read(display, PLANE_CTL(pipe as u32, (*plane).id));
        let fmt_code = ctl
            & if DISPLAY_VER(display) >= 11 {
                PLANE_CTL_FORMAT_MASK_ICL
            } else {
                PLANE_CTL_FORMAT_MASK_SKL
            };
        let alpha = if DISPLAY_VER(display) >= 10 {
            intel_de_read(display, PLANE_COLOR_CTL(pipe as u32, (*plane).id))
                & PLANE_COLOR_ALPHA_MASK
        } else {
            ctl & PLANE_CTL_ALPHA_MASK
        };
        let fourcc =
            skl_format_to_fourcc(fmt_code as i32, ctl & PLANE_CTL_ORDER_RGBX != 0, alpha != 0)
                as u32;
        let modifier = match ctl & PLANE_CTL_TILED_MASK {
            PLANE_CTL_TILED_LINEAR => DRM_FORMAT_MOD_LINEAR,
            PLANE_CTL_TILED_X => I915_FORMAT_MOD_X_TILED,
            PLANE_CTL_TILED_Y => {
                if ctl & PLANE_CTL_RENDER_DECOMPRESSION_ENABLE != 0 {
                    if DISPLAY_VER(display) >= 14 {
                        I915_FORMAT_MOD_4_TILED_MTL_RC_CCS
                    } else if DISPLAY_VER(display) >= 12 {
                        I915_FORMAT_MOD_Y_TILED_GEN12_RC_CCS
                    } else {
                        I915_FORMAT_MOD_Y_TILED_CCS
                    }
                } else if ctl & PLANE_CTL_MEDIA_DECOMPRESSION_ENABLE != 0 {
                    if DISPLAY_VER(display) >= 14 {
                        I915_FORMAT_MOD_4_TILED_MTL_MC_CCS
                    } else {
                        I915_FORMAT_MOD_Y_TILED_GEN12_MC_CCS
                    }
                } else {
                    I915_FORMAT_MOD_Y_TILED
                }
            }
            PLANE_CTL_TILED_4 => {
                let rc_mask = PLANE_CTL_RENDER_DECOMPRESSION_ENABLE | PLANE_CTL_CLEAR_COLOR_DISABLE;
                if ctl & rc_mask == rc_mask {
                    if DISPLAY_VER(display) >= 14 {
                        I915_FORMAT_MOD_4_TILED_MTL_RC_CCS
                    } else {
                        I915_FORMAT_MOD_4_TILED_DG2_RC_CCS
                    }
                } else if ctl & PLANE_CTL_MEDIA_DECOMPRESSION_ENABLE != 0 {
                    if DISPLAY_VER(display) >= 14 {
                        I915_FORMAT_MOD_4_TILED_MTL_MC_CCS
                    } else {
                        I915_FORMAT_MOD_4_TILED_DG2_MC_CCS
                    }
                } else if ctl & PLANE_CTL_RENDER_DECOMPRESSION_ENABLE != 0 {
                    if DISPLAY_VER(display) >= 14 {
                        I915_FORMAT_MOD_4_TILED_MTL_RC_CCS_CC
                    } else {
                        I915_FORMAT_MOD_4_TILED_DG2_RC_CCS_CC
                    }
                } else {
                    I915_FORMAT_MOD_4_TILED
                }
            }
            _ => {
                tk_linux_kapi::memory::kfree(fb.cast::<c_void>());
                return;
            }
        };
        (*fb).base.modifier = modifier;
        (*fb).base.format = drm_get_format_info((*display).drm, fourcc, modifier);
        if (*fb).base.format.is_null() {
            tk_linux_kapi::memory::kfree(fb.cast::<c_void>());
            return;
        }
        let rotation = match ctl & PLANE_CTL_ROTATE_MASK {
            PLANE_CTL_ROTATE_0 => DRM_MODE_ROTATE_0,
            PLANE_CTL_ROTATE_90 => DRM_MODE_ROTATE_270,
            PLANE_CTL_ROTATE_180 => DRM_MODE_ROTATE_180,
            PLANE_CTL_ROTATE_270 => DRM_MODE_ROTATE_90,
            _ => DRM_MODE_ROTATE_0,
        };
        let rotation = if DISPLAY_VER(display) >= 11 && ctl & PLANE_CTL_FLIP_HORIZONTAL != 0 {
            rotation | DRM_MODE_REFLECT_X
        } else {
            rotation
        };
        if rotation & (DRM_MODE_ROTATE_90 | DRM_MODE_ROTATE_270) != 0 {
            tk_linux_kapi::memory::kfree(fb.cast::<c_void>());
            return;
        }
        (*config).rotation = rotation as u8;
        (*config).base =
            intel_de_read(display, PLANE_SURF(pipe as u32, (*plane).id)) & PLANE_SURF_ADDR_MASK;
        let size = intel_de_read(display, PLANE_SIZE(pipe as u32, (*plane).id));
        (*fb).base.height = ((size & PLANE_HEIGHT_MASK) >> 16) + 1;
        (*fb).base.width = (size & PLANE_WIDTH_MASK) + 1;
        let stride =
            intel_de_read(display, PLANE_STRIDE(pipe as u32, (*plane).id)) & PLANE_STRIDE__MASK;
        (*fb).base.pitches[0] = stride * plane_stride_mult(&mut (*fb).base, 0, rotation);
        (*config).size = ((*fb).base.pitches[0]
            * intel_fb_align_height(&(*fb).base, 0, (*fb).base.height))
            as i32;
        (*config).fb = ptr::addr_of_mut!((*fb).base);
        (*config).vma = ptr::null_mut();
    }
}

// upstream: skl_universal_plane.c skl_fixup_initial_plane_config()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_fixup_initial_plane_config(
    crtc: *mut IntelCrtc,
    config: *const IntelInitialPlaneConfig,
) -> bool {
    if crtc.is_null() || config.is_null() {
        return false;
    }
    let primary = unsafe { (*crtc).base.primary };
    if primary.is_null() {
        return false;
    }
    let plane = unsafe { plane_from_drm(primary) };
    if unsafe { (*plane).base.state.is_null() } {
        return false;
    }
    let state = unsafe { state_from_drm((*plane).base.state) };
    if unsafe { !(*state).uapi.visible || (*config).base == (*state).surf } {
        return false;
    }
    let display = unsafe { display_from_plane(plane) };
    unsafe {
        intel_de_write(
            display,
            PLANE_SURF((*crtc).pipe as u32, (*plane).id),
            (*state).surf,
        );
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skl_universal_plane_regs_upstream::*;

    #[test]
    fn display13_format_control_codes_match_register_fields() {
        assert_eq!(
            plane_ctl_format(DRM_FORMAT_XRGB8888),
            Some(PLANE_CTL_FORMAT_XRGB_8888)
        );
        assert_eq!(
            plane_ctl_format(DRM_FORMAT_ABGR8888),
            Some(PLANE_CTL_FORMAT_XRGB_8888 | PLANE_CTL_ORDER_RGBX)
        );
        assert_eq!(
            plane_ctl_format(DRM_FORMAT_NV12),
            Some(PLANE_CTL_FORMAT_NV12)
        );
        assert_eq!(plane_ctl_format(u32::MAX), None);
    }

    #[test]
    fn tiling_and_rotation_encodings_reject_unknown_combinations() {
        assert_eq!(
            plane_ctl_tiling(I915_FORMAT_MOD_Y_TILED_GEN12_RC_CCS),
            Some(
                PLANE_CTL_TILED_Y
                    | PLANE_CTL_RENDER_DECOMPRESSION_ENABLE
                    | PLANE_CTL_CLEAR_COLOR_DISABLE
            )
        );
        assert_eq!(plane_ctl_tiling(u64::MAX), None);
        assert_eq!(
            plane_ctl_rotation(DRM_MODE_ROTATE_90),
            Some(PLANE_CTL_ROTATE_270)
        );
        assert_eq!(plane_ctl_rotation(0), None);
    }

    #[test]
    fn adlp_stride_limits_obey_byte_boundary_in_both_orientations() {
        // Use the canonical DRM format metadata: cpp[0] is 4 for XRGB8888.
        // Zero-initializing the anonymous format-info union made this an
        // invalid fixture and incorrectly exercised the cpp==0 guard.
        let info = tk_drm_upstream::drm_fourcc_upstream::drm_format_info(DRM_FORMAT_XRGB8888);
        assert!(!info.is_null());
        assert_eq!(
            unsafe {
                plane_max_stride(
                    ptr::null_mut(),
                    info.cast_mut(),
                    DRM_FORMAT_MOD_LINEAR,
                    DRM_MODE_ROTATE_0,
                )
            },
            128 * 1024
        );
        assert_eq!(
            unsafe {
                plane_max_stride(
                    ptr::null_mut(),
                    info.cast_mut(),
                    DRM_FORMAT_MOD_LINEAR,
                    DRM_MODE_ROTATE_90,
                )
            },
            32768
        );
    }
}
