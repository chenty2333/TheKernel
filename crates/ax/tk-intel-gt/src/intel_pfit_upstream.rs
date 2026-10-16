// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// Source: drivers/gpu/drm/i915/display/intel_pfit.c (Linux v7.2.3), compute
// and mode-validation paths. The hardware enable/disable paths
// (ilk_pfit_enable/disable, gmch programming) are not reached by the link.
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::ptr;

use tk_drm_upstream::{
    drm_connector_types::DrmConnectorState,
    drm_modes_types::{DrmDisplayMode, DrmModeStatus},
    drm_plane_types::DRM_SCALING_FILTER_DEFAULT,
    drm_rect_types::DrmRect,
    drm_rect_upstream::{
        drm_rect_calc_hscale, drm_rect_calc_vscale, drm_rect_height, drm_rect_init, drm_rect_width,
    },
    uapi_drm_consts::{
        DRM_MODE_FLAG_INTERLACE, DRM_MODE_SCALE_ASPECT, DRM_MODE_SCALE_CENTER,
        DRM_MODE_SCALE_FULLSCREEN, DRM_MODE_SCALE_NONE,
    },
};
use tk_linux_kapi::config::EINVAL;

use crate::{
    intel_display_conversion_upstream::__drm_to_display,
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_types_upstream::{
        INTEL_OUTPUT_FORMAT_RGB, INTEL_OUTPUT_FORMAT_YCBCR420, IntelCrtc, IntelCrtcState,
        IntelOutputFormat,
    },
    intel_lvds_regs_upstream::LVDS_BORDER_ENABLE,
};

// intel_pfit_regs.h bit fields used by the compute path (REG_FIELD_PREP
// shifts are applied by hand).
const PFIT_ENABLE: u32 = 1 << 31;
const PFIT_PIPE_SHIFT: u32 = 29;
const PFIT_SCALING_AUTO: u32 = 0 << 26;
const PFIT_SCALING_PILLAR: u32 = 2 << 26;
const PFIT_SCALING_LETTER: u32 = 3 << 26;
const PFIT_FILTER_FUZZY: u32 = 0 << 24;
const PFIT_VERT_INTERP_BILINEAR: u32 = 1 << 10;
const PFIT_VERT_AUTO_SCALE: u32 = 1 << 9;
const PFIT_HORIZ_INTERP_BILINEAR: u32 = 1 << 6;
const PFIT_HORIZ_AUTO_SCALE: u32 = 1 << 5;
const PFIT_PANEL_8TO6_DITHER_ENABLE: u32 = 1 << 3;

const fn PFIT_VERT_SCALE(x: u32) -> u32 {
    x << 20
}

const fn PFIT_HORIZ_SCALE(x: u32) -> u32 {
    x << 4
}

/// `HAS_GMCH(display)`: `DISPLAY_INFO(display)->has_gmch` (bit 7 of the
/// device-info bitfield word at offset 46).
#[inline]
unsafe fn HAS_GMCH(display: *const IntelDisplay) -> bool {
    unsafe {
        (*crate::intel_display_reg_defs_upstream::DISPLAY_INFO(display))._bitfields_46[0] & (1 << 7)
            != 0
    }
}

/// `DISPLAY_VER()` for the display device.
#[inline]
unsafe fn display_ver(display: *const IntelDisplay) -> u16 {
    unsafe { (*display).info.__runtime_info.ip.ver }
}

#[inline]
unsafe fn drm_of_crtc_state(crtc_state: *const IntelCrtcState) -> *mut IntelDisplay {
    unsafe { __drm_to_display((*(*crtc_state).uapi.crtc).dev) }
}

/// `to_intel_display(crtc_state)`.
#[inline]
unsafe fn to_intel_display(crtc_state: *const IntelCrtcState) -> *mut IntelDisplay {
    unsafe { drm_of_crtc_state(crtc_state) }
}

/// `drm_rect_width(&crtc_state->pipe_src)` / `drm_rect_height(...)`.
#[inline]
unsafe fn pipe_src_wh(crtc_state: *const IntelCrtcState) -> (i32, i32) {
    unsafe {
        let src = &(*crtc_state).pipe_src;
        (drm_rect_width(src), drm_rect_height(src))
    }
}

/// `&crtc_state->hw.adjusted_mode`.
#[inline]
unsafe fn adjusted_mode_mut(crtc_state: *mut IntelCrtcState) -> *mut DrmDisplayMode {
    unsafe { ptr::addr_of_mut!((*crtc_state).hw.adjusted_mode) }
}

/// `panel_fitter_scaling()`.
// upstream: intel_pfit.c panel_fitter_scaling()
fn panel_fitter_scaling(source: u32, target: u32) -> u32 {
    const ACCURACY: u32 = 12;
    const FACTOR: u32 = 1 << ACCURACY;
    let ratio = source.wrapping_mul(FACTOR) / target;
    FACTOR.wrapping_mul(ratio).wrapping_add(FACTOR / 2) / FACTOR
}

/// `centre_horizontally()`: keep the hsync/hblank widths, centre the active area.
// upstream: intel_pfit.c centre_horizontally()
unsafe fn centre_horizontally(adjusted_mode: *mut DrmDisplayMode, width: i32) {
    unsafe {
        let m = &mut *adjusted_mode;
        let sync_width = m.crtc_hsync_end as u32 - m.crtc_hsync_start as u32;
        let blank_width = m.crtc_hblank_end as u32 - m.crtc_hblank_start as u32;
        let sync_pos = (blank_width.wrapping_sub(sync_width).wrapping_add(1)) / 2;
        let mut border = ((m.crtc_hdisplay as i32 - width + 1) / 2) as u32;
        border = border.wrapping_add(border & 1);

        m.crtc_hdisplay = width as u16;
        m.crtc_hblank_start = (width as u32).wrapping_add(border) as u16;
        m.crtc_hblank_end = (m.crtc_hblank_start as u32).wrapping_add(blank_width) as u16;
        m.crtc_hsync_start = (m.crtc_hblank_start as u32).wrapping_add(sync_pos) as u16;
        m.crtc_hsync_end = (m.crtc_hsync_start as u32).wrapping_add(sync_width) as u16;
    }
}

/// `centre_vertically()`.
// upstream: intel_pfit.c centre_vertically()
unsafe fn centre_vertically(adjusted_mode: *mut DrmDisplayMode, height: i32) {
    unsafe {
        let m = &mut *adjusted_mode;
        let sync_width = m.crtc_vsync_end as u32 - m.crtc_vsync_start as u32;
        let blank_width = m.crtc_vblank_end as u32 - m.crtc_vblank_start as u32;
        let sync_pos = (blank_width.wrapping_sub(sync_width).wrapping_add(1)) / 2;
        let border = ((m.crtc_vdisplay as i32 - height + 1) / 2) as u32;

        m.crtc_vdisplay = height as u16;
        m.crtc_vblank_start = (height as u32).wrapping_add(border) as u16;
        m.crtc_vblank_end = (m.crtc_vblank_start as u32).wrapping_add(blank_width) as u16;
        m.crtc_vsync_start = (m.crtc_vblank_start as u32).wrapping_add(sync_pos) as u16;
        m.crtc_vsync_end = (m.crtc_vsync_start as u32).wrapping_add(sync_width) as u16;
    }
}

/// `i965_scale_aspect()`.
// upstream: intel_pfit.c i965_scale_aspect()
unsafe fn i965_scale_aspect(crtc_state: *mut IntelCrtcState, pfit_control: &mut u32) {
    unsafe {
        let adjusted_mode = &(*crtc_state).hw.adjusted_mode;
        let (pipe_src_w, pipe_src_h) = pipe_src_wh(crtc_state);
        let scaled_width = (adjusted_mode.crtc_hdisplay as i32).wrapping_mul(pipe_src_h) as u32;
        let scaled_height = (pipe_src_w as u32).wrapping_mul(adjusted_mode.crtc_vdisplay as u32);

        if scaled_width > scaled_height {
            *pfit_control |= PFIT_ENABLE | PFIT_SCALING_PILLAR;
        } else if scaled_width < scaled_height {
            *pfit_control |= PFIT_ENABLE | PFIT_SCALING_LETTER;
        } else if adjusted_mode.crtc_hdisplay as i32 != pipe_src_w {
            *pfit_control |= PFIT_ENABLE | PFIT_SCALING_AUTO;
        }
    }
}

/// `i9xx_scale_aspect()`.
// upstream: intel_pfit.c i9xx_scale_aspect()
unsafe fn i9xx_scale_aspect(
    crtc_state: *mut IntelCrtcState,
    pfit_control: &mut u32,
    pfit_pgm_ratios: &mut u32,
    border: &mut u32,
) {
    unsafe {
        let adjusted_mode = adjusted_mode_mut(crtc_state);
        let (pipe_src_w, pipe_src_h) = pipe_src_wh(crtc_state);
        let hdisplay = (*adjusted_mode).crtc_hdisplay as i32;
        let vdisplay = (*adjusted_mode).crtc_vdisplay as i32;
        let scaled_width = hdisplay.wrapping_mul(pipe_src_h) as u32;
        let scaled_height = (pipe_src_w as u32).wrapping_mul(vdisplay as u32);

        if scaled_width > scaled_height {
            centre_horizontally(adjusted_mode, (scaled_height / pipe_src_h as u32) as i32);
            *border = LVDS_BORDER_ENABLE;
            if pipe_src_h != vdisplay {
                let bits = panel_fitter_scaling(pipe_src_h as u32, vdisplay as u32);
                *pfit_pgm_ratios |= PFIT_HORIZ_SCALE(bits) | PFIT_VERT_SCALE(bits);
                *pfit_control |=
                    PFIT_ENABLE | PFIT_VERT_INTERP_BILINEAR | PFIT_HORIZ_INTERP_BILINEAR;
            }
        } else if scaled_width < scaled_height {
            centre_vertically(adjusted_mode, (scaled_width / pipe_src_w as u32) as i32);
            *border = LVDS_BORDER_ENABLE;
            if pipe_src_w != hdisplay {
                let bits = panel_fitter_scaling(pipe_src_w as u32, hdisplay as u32);
                *pfit_pgm_ratios |= PFIT_HORIZ_SCALE(bits) | PFIT_VERT_SCALE(bits);
                *pfit_control |=
                    PFIT_ENABLE | PFIT_VERT_INTERP_BILINEAR | PFIT_HORIZ_INTERP_BILINEAR;
            }
        } else {
            *pfit_control |= PFIT_ENABLE
                | PFIT_VERT_AUTO_SCALE
                | PFIT_HORIZ_AUTO_SCALE
                | PFIT_VERT_INTERP_BILINEAR
                | PFIT_HORIZ_INTERP_BILINEAR;
        }
    }
}

/// `intel_gmch_pfit_check_timings()`.
// upstream: intel_pfit.c intel_gmch_pfit_check_timings()
unsafe fn intel_gmch_pfit_check_timings(crtc_state: *const IntelCrtcState) -> i32 {
    unsafe {
        let display = to_intel_display(crtc_state);
        let adjusted_mode = &(*crtc_state).hw.adjusted_mode;
        let min = if display_ver(display) >= 4 { 3 } else { 2 };
        if (adjusted_mode.crtc_hdisplay as i32) < min {
            return -EINVAL;
        }
        if (adjusted_mode.crtc_vdisplay as i32) < min {
            return -EINVAL;
        }
        0
    }
}

/// `gmch_panel_fitting()`: pre-Ironlake (GMCH) panel fitter.
// upstream: intel_pfit.c gmch_panel_fitting()
unsafe fn gmch_panel_fitting(
    crtc_state: *mut IntelCrtcState,
    conn_state: *const DrmConnectorState,
) -> i32 {
    unsafe {
        let display = to_intel_display(crtc_state);
        let crtc = (*crtc_state).uapi.crtc.cast::<IntelCrtc>();
        let mut pfit_control: u32 = 0;
        let mut pfit_pgm_ratios: u32 = 0;
        let mut border: u32 = 0;
        let (pipe_src_w, pipe_src_h) = pipe_src_wh(crtc_state);

        'out: {
            let adjusted_mode = adjusted_mode_mut(crtc_state);
            // Native modes don't need fitting.
            if (*adjusted_mode).crtc_hdisplay as i32 == pipe_src_w
                && (*adjusted_mode).crtc_vdisplay as i32 == pipe_src_h
            {
                break 'out;
            }

            // TODO in Linux: downscaling for i965+ is not implemented.
            if ((*adjusted_mode).crtc_hdisplay as i32) < pipe_src_w {
                return -EINVAL;
            }
            if ((*adjusted_mode).crtc_vdisplay as i32) < pipe_src_h {
                return -EINVAL;
            }

            match (*conn_state).scaling_mode {
                DRM_MODE_SCALE_CENTER => {
                    centre_horizontally(adjusted_mode, pipe_src_w);
                    centre_vertically(adjusted_mode, pipe_src_h);
                    border = LVDS_BORDER_ENABLE;
                }
                DRM_MODE_SCALE_ASPECT => {
                    if display_ver(display) >= 4 {
                        i965_scale_aspect(crtc_state, &mut pfit_control);
                    } else {
                        i9xx_scale_aspect(
                            crtc_state,
                            &mut pfit_control,
                            &mut pfit_pgm_ratios,
                            &mut border,
                        );
                    }
                }
                DRM_MODE_SCALE_FULLSCREEN => {
                    // Full scaling, even if it changes the aspect ratio.
                    if pipe_src_h != (*adjusted_mode).crtc_vdisplay as i32
                        || pipe_src_w != (*adjusted_mode).crtc_hdisplay as i32
                    {
                        pfit_control |= PFIT_ENABLE;
                        if display_ver(display) >= 4 {
                            pfit_control |= PFIT_SCALING_AUTO;
                        } else {
                            pfit_control |= PFIT_VERT_AUTO_SCALE
                                | PFIT_VERT_INTERP_BILINEAR
                                | PFIT_HORIZ_AUTO_SCALE
                                | PFIT_HORIZ_INTERP_BILINEAR;
                        }
                    }
                }
                _ => return -EINVAL,
            }

            // 965+ wants fuzzy fitting.
            if display_ver(display) >= 4 {
                pfit_control |= ((*crtc).pipe as u32) << PFIT_PIPE_SHIFT | PFIT_FILTER_FUZZY;
            }
        }

        // out:
        if pfit_control & PFIT_ENABLE == 0 {
            pfit_control = 0;
            pfit_pgm_ratios = 0;
        }

        if display_ver(display) < 4 && (*crtc_state).pipe_bpp == 18 {
            pfit_control |= PFIT_PANEL_8TO6_DITHER_ENABLE;
        }

        (*crtc_state).gmch_pfit.control = pfit_control;
        (*crtc_state).gmch_pfit.pgm_ratios = pfit_pgm_ratios;
        (*crtc_state).gmch_pfit.lvds_border_bits = border;

        if pfit_control & PFIT_ENABLE == 0 {
            return 0;
        }

        intel_gmch_pfit_check_timings(crtc_state)
    }
}

/// `intel_pch_pfit_check_dst_window()`.
// upstream: intel_pfit.c intel_pch_pfit_check_dst_window()
unsafe fn intel_pch_pfit_check_dst_window(crtc_state: *const IntelCrtcState) -> i32 {
    unsafe {
        let adjusted_mode = &(*crtc_state).hw.adjusted_mode;
        let dst = &(*crtc_state).pch_pfit.dst;
        let width = drm_rect_width(dst);
        let height = drm_rect_height(dst);
        let x = dst.x1;
        let y = dst.y1;

        if adjusted_mode.flags & DRM_MODE_FLAG_INTERLACE != 0 && (y & 1 != 0 || height & 1 != 0) {
            return -EINVAL;
        }
        if adjusted_mode.crtc_hdisplay as i32 != 2 * x + width
            || adjusted_mode.crtc_vdisplay as i32 != 2 * y + height
        {
            return -EINVAL;
        }
        if x == 1 {
            return -EINVAL;
        }
        0
    }
}

/// `intel_pch_pfit_check_src_size()`.
// upstream: intel_pfit.c intel_pch_pfit_check_src_size()
unsafe fn intel_pch_pfit_check_src_size(crtc_state: *const IntelCrtcState) -> i32 {
    unsafe {
        let display = to_intel_display(crtc_state);
        let crtc = (*crtc_state).uapi.crtc.cast::<IntelCrtc>();
        let (pipe_src_w, pipe_src_h) = pipe_src_wh(crtc_state);
        let (max_src_w, max_src_h) = if display_ver(display) >= 8 {
            (4096, 4096)
        } else if display_ver(display) >= 7 {
            (if (*crtc).pipe == 0 { 4096 } else { 2048 }, 4096)
        } else {
            (4096, 4096)
        };
        if pipe_src_w > max_src_w || pipe_src_h > max_src_h {
            return -EINVAL;
        }
        0
    }
}

/// `intel_pch_pfit_check_scaling()`.
// upstream: intel_pfit.c intel_pch_pfit_check_scaling()
unsafe fn intel_pch_pfit_check_scaling(crtc_state: *const IntelCrtcState) -> i32 {
    unsafe {
        let dst = &(*crtc_state).pch_pfit.dst;
        let (pipe_src_w, pipe_src_h) = pipe_src_wh(crtc_state);
        let max_scale = 0x12000;
        let mut src = DrmRect {
            x1: 0,
            y1: 0,
            x2: 0,
            y2: 0,
        };
        drm_rect_init(&mut src, 0, 0, pipe_src_w << 16, pipe_src_h << 16);

        let hscale = drm_rect_calc_hscale(&src, dst, 0, max_scale);
        if hscale < 0 {
            return hscale;
        }
        let vscale = drm_rect_calc_vscale(&src, dst, 0, max_scale);
        if vscale < 0 {
            return vscale;
        }
        0
    }
}

/// `intel_pch_pfit_check_timings()`.
// upstream: intel_pfit.c intel_pch_pfit_check_timings()
unsafe fn intel_pch_pfit_check_timings(crtc_state: *const IntelCrtcState) -> i32 {
    unsafe {
        if (*crtc_state).hw.adjusted_mode.crtc_vdisplay < 7 {
            return -EINVAL;
        }
        0
    }
}

/// `intel_pch_pfit_check_cloning()`.
// upstream: intel_pfit.c intel_pch_pfit_check_cloning()
unsafe fn intel_pch_pfit_check_cloning(crtc_state: *const IntelCrtcState) -> i32 {
    unsafe {
        let mask = (*crtc_state).uapi.encoder_mask;
        if mask != 0 && !(mask & (mask - 1) == 0) {
            return -EINVAL;
        }
        0
    }
}

/// `pch_panel_fitting()`: Ironlake-and-later PCH panel fitter.
// upstream: intel_pfit.c pch_panel_fitting()
unsafe fn pch_panel_fitting(
    crtc_state: *mut IntelCrtcState,
    conn_state: *const DrmConnectorState,
) -> i32 {
    unsafe {
        let display = to_intel_display(crtc_state);
        let (pipe_src_w, pipe_src_h) = pipe_src_wh(crtc_state);
        let mut ret: i32;
        let x: i32;
        let y: i32;
        let width: i32;
        let height: i32;

        {
            let adjusted_mode = &(*crtc_state).hw.adjusted_mode;
            // Native modes don't need fitting.
            if adjusted_mode.crtc_hdisplay as i32 == pipe_src_w
                && adjusted_mode.crtc_vdisplay as i32 == pipe_src_h
                && (*crtc_state).output_format != INTEL_OUTPUT_FORMAT_YCBCR420
                && (*crtc_state).hw.sharpness_strength == 0
            {
                return 0;
            }
        }

        let hdisplay = (*crtc_state).hw.adjusted_mode.crtc_hdisplay as i32;
        let vdisplay = (*crtc_state).hw.adjusted_mode.crtc_vdisplay as i32;

        match (*conn_state).scaling_mode {
            DRM_MODE_SCALE_CENTER => {
                if hdisplay < pipe_src_w || vdisplay < pipe_src_h {
                    return -EINVAL;
                }
                width = pipe_src_w;
                height = pipe_src_h;
                x = (hdisplay - width + 1) / 2;
                y = (vdisplay - height + 1) / 2;
            }
            DRM_MODE_SCALE_ASPECT => {
                // Scale but preserve the aspect ratio.
                let scaled_width = (hdisplay as u32).wrapping_mul(pipe_src_h as u32);
                let scaled_height = (pipe_src_w as u32).wrapping_mul(vdisplay as u32);
                if scaled_width > scaled_height {
                    // pillar
                    let mut w = (scaled_height / pipe_src_h as u32) as i32;
                    if w & 1 != 0 {
                        w += 1;
                    }
                    width = w;
                    x = (hdisplay - width + 1) / 2;
                    y = 0;
                    height = vdisplay;
                } else if scaled_width < scaled_height {
                    // letter
                    let mut h = (scaled_width / pipe_src_w as u32) as i32;
                    if h & 1 != 0 {
                        h += 1;
                    }
                    height = h;
                    y = (vdisplay - height + 1) / 2;
                    x = 0;
                    width = hdisplay;
                } else {
                    x = 0;
                    y = 0;
                    width = hdisplay;
                    height = vdisplay;
                }
            }
            DRM_MODE_SCALE_NONE | DRM_MODE_SCALE_FULLSCREEN => {
                // SCALE_NONE falls through to FULLSCREEN in Linux (WARNs only).
                x = 0;
                y = 0;
                width = hdisplay;
                height = vdisplay;
            }
            _ => return -EINVAL,
        }

        if (*crtc_state).hw.sharpness_strength != 0
            && (width != pipe_src_w
                || height != pipe_src_h
                || (*crtc_state).hw.scaling_filter != DRM_SCALING_FILTER_DEFAULT
                || (*crtc_state).output_format != INTEL_OUTPUT_FORMAT_RGB)
        {
            return -EINVAL;
        }

        drm_rect_init(&mut (*crtc_state).pch_pfit.dst, x, y, width, height);
        (*crtc_state).pch_pfit.enabled = true;

        // SKL+ have unified scalers: the checks are done in one place.
        if display_ver(display) >= 9 {
            return 0;
        }

        ret = intel_pch_pfit_check_dst_window(crtc_state);
        if ret != 0 {
            return ret;
        }
        ret = intel_pch_pfit_check_src_size(crtc_state);
        if ret != 0 {
            return ret;
        }
        ret = intel_pch_pfit_check_scaling(crtc_state);
        if ret != 0 {
            return ret;
        }
        ret = intel_pch_pfit_check_timings(crtc_state);
        if ret != 0 {
            return ret;
        }
        ret = intel_pch_pfit_check_cloning(crtc_state);
        if ret != 0 {
            return ret;
        }
        0
    }
}

/// `intel_pfit_mode_valid()`.
// upstream: intel_pfit.c intel_pfit_mode_valid()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pfit_mode_valid(
    display: *mut IntelDisplay,
    mode: *const DrmDisplayMode,
    output_format: IntelOutputFormat,
    num_joined_pipes: i32,
) -> DrmModeStatus {
    unsafe {
        crate::skl_scaler_upstream::skl_scaler_mode_valid(
            display,
            mode,
            output_format,
            num_joined_pipes,
        ) as DrmModeStatus
    }
}

/// `intel_pfit_compute_config()`.
// upstream: intel_pfit.c intel_pfit_compute_config()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pfit_compute_config(
    crtc_state: *mut IntelCrtcState,
    conn_state: *const DrmConnectorState,
) -> i32 {
    unsafe {
        let display = to_intel_display(crtc_state);
        if HAS_GMCH(display) {
            gmch_panel_fitting(crtc_state, conn_state)
        } else {
            pch_panel_fitting(crtc_state, conn_state)
        }
    }
}
