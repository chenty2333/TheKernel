// SPDX-License-Identifier: GPL-2.0-only
// Copyright 2026 TheKernel contributors.
//! Port of Linux `drivers/gpu/drm/sysfb/drm_sysfb_modeset.c` (v7.2.3) and the
//! vtable macros of `drm_sysfb_helper.h` (DRM_SYSFB_PLANE_FUNCS,
//! DRM_SYSFB_PLANE_HELPER_FUNCS, DRM_SYSFB_CRTC_FUNCS,
//! DRM_SYSFB_CRTC_HELPER_FUNCS, DRM_SYSFB_CONNECTOR_FUNCS,
//! DRM_SYSFB_CONNECTOR_HELPER_FUNCS, DRM_SYSFB_MODE_CONFIG_FUNCS).
//!
//! The header macros expand to designated initializers in each driver. They
//! are provided as `const fn`s that return the fully populated vtable, so a
//! driver adds its own members on top (efidrm sets `.destroy`).
//!
//! Deviations:
//! - `drm_sysfb_plane_helper_get_scanout_buffer()` takes its
//!   `struct drm_scanout_buffer *` as `*mut c_void`, matching the helper
//!   vtable slot it is installed into.
//! - `drm_warn_once()` is emulated with a per-call-site atomic flag.
//! - The `drm_dbg_kms()` trace lines of `drm_sysfb_build_fourcc_list()` are
//!   omitted.

#![allow(non_snake_case, dead_code, unsafe_code)]

use core::{
    ffi::{c_int, c_void},
    mem::{size_of, zeroed},
    ptr,
    sync::atomic::{AtomicBool, Ordering},
};

use tk_linux_kapi::{
    config::{EINVAL, ENOMEM, GFP_KERNEL},
    memory::{kfree, kzalloc_obj},
};

use crate::{
    drm_atomic_helper_upstream as ah,
    drm_atomic_state_helper_upstream::{
        __drm_atomic_helper_crtc_destroy_state, __drm_atomic_helper_crtc_duplicate_state,
        __drm_atomic_helper_crtc_reset, drm_atomic_helper_connector_destroy_state,
        drm_atomic_helper_connector_duplicate_state, drm_atomic_helper_connector_reset,
    },
    drm_atomic_types::DrmAtomicCommit,
    drm_atomic_upstream::{
        drm_atomic_get_new_crtc_state, drm_atomic_get_new_plane_state,
        drm_atomic_get_old_plane_state,
    },
    drm_connector_types::{DrmConnector, DrmConnectorFuncs},
    drm_crtc_types::{DrmCrtc, DrmCrtcFuncs, DrmCrtcState},
    drm_damage_helper_types::DrmAtomicHelperDamageIter,
    drm_damage_helper_upstream::{
        drm_atomic_helper_damage_iter_init, drm_atomic_helper_damage_iter_next,
    },
    drm_device_types::DrmDevice,
    drm_drv_upstream::{drm_dev_enter, drm_dev_exit},
    drm_edid_upstream::{drm_edid_connector_update, drm_edid_free, drm_edid_read_custom},
    drm_file_types::DrmFile,
    drm_format_helper_types::DrmFormatConvState,
    drm_format_helper_upstream as fh,
    drm_fourcc_types::DrmFormatInfo,
    drm_framebuffer_types::DrmFramebuffer,
    drm_gem_atomic_helper_types::DrmShadowPlaneState,
    drm_gem_atomic_helper_upstream::{
        __drm_gem_destroy_shadow_plane_state, __drm_gem_duplicate_shadow_plane_state,
        __drm_gem_reset_shadow_plane, drm_gem_begin_shadow_fb_access, drm_gem_end_shadow_fb_access,
        to_drm_shadow_plane_state,
    },
    drm_gem_framebuffer_helper_upstream::{
        drm_gem_fb_begin_cpu_access, drm_gem_fb_create_with_dirty, drm_gem_fb_end_cpu_access,
        iosys_map_incr,
    },
    drm_mode_config_types::DrmModeConfigFuncs,
    drm_modes_types::{DrmDisplayMode, DrmModeStatus},
    drm_modeset_helper_vtables_types::{
        DrmConnectorHelperFuncs, DrmCrtcHelperFuncs, DrmPlaneHelperFuncs,
    },
    drm_plane_types::{DrmPlane, DrmPlaneFuncs, DrmPlaneState},
    drm_probe_helper_upstream::{
        drm_connector_helper_get_modes_fixed, drm_crtc_helper_mode_valid_fixed,
        drm_helper_probe_single_connector_modes,
    },
    drm_property_types::DrmPropertyBlob,
    drm_rect_types::DrmRect,
    drm_rect_upstream::{drm_rect_height, drm_rect_init, drm_rect_intersect, drm_rect_width},
    drm_sysfb_helper_types::{
        DrmScanoutBuffer, DrmSysfbBlitFunc, DrmSysfbCrtcState, DrmSysfbDevice, DrmSysfbPlaneState,
        to_drm_sysfb_crtc_state, to_drm_sysfb_device, to_drm_sysfb_plane_state,
    },
    drm_sysfb_upstream::drm_warn,
    iosys_map_types::IosysMap,
    uapi_drm_consts::{
        DRM_FORMAT_ABGR1555, DRM_FORMAT_ABGR8888, DRM_FORMAT_ABGR2101010, DRM_FORMAT_ARGB1555,
        DRM_FORMAT_ARGB8888, DRM_FORMAT_ARGB2101010, DRM_FORMAT_BGR888, DRM_FORMAT_BGRA5551,
        DRM_FORMAT_BGRA8888, DRM_FORMAT_BGRA1010102, DRM_FORMAT_BGRX5551, DRM_FORMAT_BGRX8888,
        DRM_FORMAT_BGRX1010102, DRM_FORMAT_BIG_ENDIAN, DRM_FORMAT_RGB332, DRM_FORMAT_RGB565,
        DRM_FORMAT_RGB888, DRM_FORMAT_RGBA5551, DRM_FORMAT_RGBA8888, DRM_FORMAT_RGBA1010102,
        DRM_FORMAT_RGBX5551, DRM_FORMAT_RGBX8888, DRM_FORMAT_RGBX1010102, DRM_FORMAT_XBGR1555,
        DRM_FORMAT_XBGR8888, DRM_FORMAT_XBGR2101010, DRM_FORMAT_XRGB1555, DRM_FORMAT_XRGB8888,
        DRM_FORMAT_XRGB2101010, DRM_MODE_TYPE_DRIVER,
    },
    uapi_drm_mode_types::{DrmColorLut, DrmModeFbCmd2},
};

// upstream: drm_sysfb_modeset.c DRM_PLANE_NO_SCALING (drm_plane_helper.h)
const DRM_PLANE_NO_SCALING: i32 = 1 << 16;

// upstream: include/linux/dma-direction.h DMA_FROM_DEVICE
const DMA_FROM_DEVICE: c_int = 2;

// upstream: include/drm/drm_edid.h EDID_LENGTH
const EDID_LENGTH: usize = 128;

// upstream: include/drm/drm_modes.h DRM_MODE_RES_MM(res, dpi)
const fn DRM_MODE_RES_MM(res: u64, dpi: u64) -> u64 {
    (res * 254) / (dpi * 10)
}

// upstream: drm_sysfb_modeset.c drm_sysfb_plane_state_destroy()
unsafe fn drm_sysfb_plane_state_destroy(sysfb_plane_state: *mut DrmSysfbPlaneState) {
    __drm_gem_destroy_shadow_plane_state(&mut (*sysfb_plane_state).base);
    kfree(sysfb_plane_state);
}

// upstream: drm_sysfb_modeset.c drm_sysfb_crtc_state_destroy()
unsafe fn drm_sysfb_crtc_state_destroy(sysfb_crtc_state: *mut DrmSysfbCrtcState) {
    __drm_atomic_helper_crtc_destroy_state(&mut (*sysfb_crtc_state).base);
    kfree(sysfb_crtc_state);
}

// ---------------------------------------------------------------------------
// Display modes
// ---------------------------------------------------------------------------

// upstream: drm_sysfb_modeset.c drm_sysfb_mode()
/// Builds the fixed display mode for a firmware framebuffer. A zero physical
/// size is derived assuming a 96 dpi monitor.
pub fn drm_sysfb_mode(width: u32, height: u32, width_mm: u32, height_mm: u32) -> DrmDisplayMode {
    // Assume a monitor resolution of 96 dpi to get a somewhat reasonable screen size.
    let width_mm = if width_mm == 0 {
        DRM_MODE_RES_MM(width as u64, 96) as u32
    } else {
        width_mm
    };
    let height_mm = if height_mm == 0 {
        DRM_MODE_RES_MM(height as u64, 96) as u32
    } else {
        height_mm
    };

    // DRM_MODE_INIT(60, width, height, width_mm, height_mm)
    // pix = hd * vd * hz / 1000 (kHz), computed in unsigned int as in C.
    let pix = width.wrapping_mul(height).wrapping_mul(60) / 1000;

    // SAFETY: DrmDisplayMode is plain data (integers, a list head and a name
    // buffer); the all-zero bit pattern is its C compound-literal default.
    let mut mode: DrmDisplayMode = unsafe { zeroed() };
    mode.r#type = DRM_MODE_TYPE_DRIVER as u8;
    mode.clock = pix as i32;
    mode.hdisplay = width as u16;
    mode.hsync_start = width as u16;
    mode.hsync_end = width as u16;
    mode.htotal = width as u16;
    mode.vdisplay = height as u16;
    mode.vsync_start = height as u16;
    mode.vsync_end = height as u16;
    mode.vtotal = height as u16;
    mode.width_mm = width_mm as u16;
    mode.height_mm = height_mm as u16;
    mode
}

// ---------------------------------------------------------------------------
// Format list
// ---------------------------------------------------------------------------

// upstream: drm_sysfb_modeset.c to_nonalpha_fourcc()
fn to_nonalpha_fourcc(fourcc: u32) -> u32 {
    // only handle formats with depth != 0 and alpha channel
    match fourcc {
        DRM_FORMAT_ARGB1555 => DRM_FORMAT_XRGB1555,
        DRM_FORMAT_ABGR1555 => DRM_FORMAT_XBGR1555,
        DRM_FORMAT_RGBA5551 => DRM_FORMAT_RGBX5551,
        DRM_FORMAT_BGRA5551 => DRM_FORMAT_BGRX5551,
        DRM_FORMAT_ARGB8888 => DRM_FORMAT_XRGB8888,
        DRM_FORMAT_ABGR8888 => DRM_FORMAT_XBGR8888,
        DRM_FORMAT_RGBA8888 => DRM_FORMAT_RGBX8888,
        DRM_FORMAT_BGRA8888 => DRM_FORMAT_BGRX8888,
        DRM_FORMAT_ARGB2101010 => DRM_FORMAT_XRGB2101010,
        DRM_FORMAT_ABGR2101010 => DRM_FORMAT_XBGR2101010,
        DRM_FORMAT_RGBA1010102 => DRM_FORMAT_RGBX1010102,
        DRM_FORMAT_BGRA1010102 => DRM_FORMAT_BGRX1010102,
        _ => fourcc,
    }
}

// upstream: drm_sysfb_modeset.c is_listed_fourcc()
fn is_listed_fourcc(fourccs: &[u32], fourcc: u32) -> bool {
    fourccs.iter().any(|&f| f == fourcc)
}

// upstream: drm_sysfb_modeset.c drm_sysfb_build_fourcc_list()
/// Builds the supported color-format list: native formats (alpha variants
/// replaced by their non-alpha equivalents) first, then the emulated
/// XRGB8888. Returns the number of entries written to `fourccs_out`.
pub fn drm_sysfb_build_fourcc_list(
    dev: *const DrmDevice,
    native_fourccs: &[u32],
    fourccs_out: &mut [u32],
) -> usize {
    // XRGB8888 is the default fallback format for most of userspace.
    const EXTRA_FOURCCS: [u32; 1] = [DRM_FORMAT_XRGB8888];
    let nfourccs_out = fourccs_out.len();
    let mut n = 0usize;

    // The device's native formats go first.
    for &native in native_fourccs {
        let fourcc = to_nonalpha_fourcc(native);
        if is_listed_fourcc(&fourccs_out[..n], fourcc) {
            continue; // skip duplicate entries
        } else if n == nfourccs_out {
            drm_warn(dev, "Ignoring native format\n");
            continue; // end of available output buffer
        }
        fourccs_out[n] = fourcc;
        n += 1;
    }

    // The extra formats, emulated by the driver, go second.
    for &extra in EXTRA_FOURCCS.iter() {
        if n >= nfourccs_out {
            break;
        }
        if is_listed_fourcc(&fourccs_out[..n], extra) {
            continue; // skip duplicate and native entries
        }
        fourccs_out[n] = extra;
        n += 1;
    }

    n
}

// ---------------------------------------------------------------------------
// Plane
// ---------------------------------------------------------------------------

// upstream: drm_sysfb_modeset.c drm_sysfb_memcpy()
unsafe extern "C" fn drm_sysfb_memcpy(
    dst: *mut IosysMap,
    dst_pitch: *const u32,
    src: *const IosysMap,
    fb: *const DrmFramebuffer,
    clip: *const DrmRect,
    _state: *mut DrmFormatConvState,
) {
    fh::drm_fb_memcpy(dst, dst_pitch, src, fb, clip);
}

// upstream: drm_sysfb_modeset.c drm_sysfb_get_blit_func()
/// Returns the blit helper that converts `src_format` to `dst_format`, or
/// `None` if no converter exists.
pub fn drm_sysfb_get_blit_func(dst_format: u32, src_format: u32) -> Option<DrmSysfbBlitFunc> {
    if src_format == dst_format {
        return Some(drm_sysfb_memcpy);
    } else if src_format == DRM_FORMAT_XRGB8888 {
        match dst_format {
            DRM_FORMAT_RGB565 => return Some(fh::drm_fb_xrgb8888_to_rgb565),
            x if x == DRM_FORMAT_RGB565 | DRM_FORMAT_BIG_ENDIAN => {
                return Some(fh::drm_fb_xrgb8888_to_rgb565be);
            }
            DRM_FORMAT_XRGB1555 => return Some(fh::drm_fb_xrgb8888_to_xrgb1555),
            DRM_FORMAT_ARGB1555 => return Some(fh::drm_fb_xrgb8888_to_argb1555),
            DRM_FORMAT_RGBA5551 => return Some(fh::drm_fb_xrgb8888_to_rgba5551),
            DRM_FORMAT_RGB888 => return Some(fh::drm_fb_xrgb8888_to_rgb888),
            DRM_FORMAT_BGR888 => return Some(fh::drm_fb_xrgb8888_to_bgr888),
            DRM_FORMAT_ARGB8888 => return Some(fh::drm_fb_xrgb8888_to_argb8888),
            DRM_FORMAT_XBGR8888 => return Some(fh::drm_fb_xrgb8888_to_xbgr8888),
            DRM_FORMAT_ABGR8888 => return Some(fh::drm_fb_xrgb8888_to_abgr8888),
            DRM_FORMAT_XRGB2101010 => return Some(fh::drm_fb_xrgb8888_to_xrgb2101010),
            DRM_FORMAT_ARGB2101010 => return Some(fh::drm_fb_xrgb8888_to_argb2101010),
            DRM_FORMAT_BGRX8888 => return Some(fh::drm_fb_xrgb8888_to_bgrx8888),
            DRM_FORMAT_RGB332 => return Some(fh::drm_fb_xrgb8888_to_rgb332),
            _ => {}
        }
    }
    None
}

// upstream: drm_sysfb_modeset.c drm_sysfb_plane_helper_begin_fb_access()
pub unsafe extern "C" fn drm_sysfb_plane_helper_begin_fb_access(
    plane: *mut DrmPlane,
    plane_state: *mut DrmPlaneState,
) -> i32 {
    let dev = (*plane).dev;
    let sysfb_plane_state = to_drm_sysfb_plane_state(plane_state);
    let fb = (*plane_state).fb;

    let ret = drm_gem_begin_shadow_fb_access(plane, plane_state);
    if ret != 0 {
        return ret;
    }
    if fb.is_null() {
        return 0;
    }

    let ret = -EINVAL;

    let crtc_state = drm_atomic_get_new_crtc_state((*plane_state).state, (*plane_state).crtc);
    if crtc_state.is_null() {
        drm_warn(dev, "missing new CRTC state\n");
        drm_gem_end_shadow_fb_access(plane, plane_state);
        return ret;
    }

    let sysfb_crtc_state = to_drm_sysfb_crtc_state(crtc_state);
    if (*sysfb_crtc_state).format.is_null() {
        drm_warn(dev, "missing CRTC color format\n");
        drm_gem_end_shadow_fb_access(plane, plane_state);
        return ret;
    }

    let blit_to_crtc =
        drm_sysfb_get_blit_func((*(*sysfb_crtc_state).format).format, (*(*fb).format).format);
    let Some(blit_to_crtc) = blit_to_crtc else {
        static WARNED: AtomicBool = AtomicBool::new(false);
        if !WARNED.swap(true, Ordering::Relaxed) {
            drm_warn(dev, "No blit helper found for the requested format pair.\n");
        }
        drm_gem_end_shadow_fb_access(plane, plane_state);
        return ret;
    };

    (*sysfb_plane_state).blit_to_crtc = Some(blit_to_crtc);
    0
}

// upstream: drm_sysfb_modeset.c drm_sysfb_plane_helper_atomic_check()
pub unsafe extern "C" fn drm_sysfb_plane_helper_atomic_check(
    plane: *mut DrmPlane,
    new_state: *mut DrmAtomicCommit,
) -> i32 {
    let sysfb = to_drm_sysfb_device((*plane).dev);
    let new_plane_state = drm_atomic_get_new_plane_state(new_state, plane);
    let new_shadow_plane_state = to_drm_shadow_plane_state(new_plane_state);
    let new_fb = (*new_plane_state).fb;
    let new_crtc = (*new_plane_state).crtc;

    let mut new_crtc_state: *mut DrmCrtcState = ptr::null_mut();
    if !new_crtc.is_null() {
        new_crtc_state = drm_atomic_get_new_crtc_state(new_state, (*new_plane_state).crtc);
    }

    let ret = ah::drm_atomic_helper_check_plane_state(
        new_plane_state,
        new_crtc_state,
        DRM_PLANE_NO_SCALING,
        DRM_PLANE_NO_SCALING,
        false,
        false,
    );
    if ret != 0 {
        return ret;
    } else if !(*new_plane_state).visible {
        return 0;
    }

    new_crtc_state = drm_atomic_get_new_crtc_state(new_state, (*new_plane_state).crtc);
    let new_sysfb_crtc_state = to_drm_sysfb_crtc_state(new_crtc_state);
    (*new_sysfb_crtc_state).format = (*sysfb).fb_format;

    if (*new_fb).format.cast_const() != (*new_sysfb_crtc_state).format {
        // format conversion necessary; reserve buffer
        let buf = fh::drm_format_conv_state_reserve(
            &mut (*new_shadow_plane_state).fmtcnv_state,
            (*sysfb).fb_pitch as usize,
            GFP_KERNEL,
        );
        if buf.is_null() {
            return -ENOMEM;
        }
    }

    0
}

// upstream: drm_sysfb_modeset.c drm_sysfb_plane_helper_atomic_update()
pub unsafe extern "C" fn drm_sysfb_plane_helper_atomic_update(
    plane: *mut DrmPlane,
    state: *mut DrmAtomicCommit,
) {
    let dev = (*plane).dev;
    let sysfb = to_drm_sysfb_device(dev);
    let plane_state = drm_atomic_get_new_plane_state(state, plane);
    let old_plane_state = drm_atomic_get_old_plane_state(state, plane);
    let sysfb_plane_state = to_drm_sysfb_plane_state(plane_state);
    let shadow_plane_state: *mut DrmShadowPlaneState = &mut (*sysfb_plane_state).base;
    let fb = (*plane_state).fb;
    let dst_pitch: u32 = (*sysfb).fb_pitch;
    let crtc_state = drm_atomic_get_new_crtc_state(state, (*plane_state).crtc);
    let sysfb_crtc_state = to_drm_sysfb_crtc_state(crtc_state);
    let dst_format = (*sysfb_crtc_state).format;
    let blit_to_crtc = (*sysfb_plane_state).blit_to_crtc;

    let ret = drm_gem_fb_begin_cpu_access(fb, DMA_FROM_DEVICE);
    if ret != 0 {
        return;
    }

    let mut idx: c_int = 0;
    if drm_dev_enter(dev, &mut idx) {
        let mut iter: DrmAtomicHelperDamageIter = zeroed();
        let mut damage: DrmRect = zeroed();
        drm_atomic_helper_damage_iter_init(&mut iter, old_plane_state, plane_state);
        // drm_atomic_for_each_plane_damage(&iter, &damage)
        while drm_atomic_helper_damage_iter_next(&mut iter, &mut damage) {
            let mut dst: IosysMap = ptr::read(&(*sysfb).fb_addr);
            let mut dst_clip: DrmRect = ptr::read(&(*plane_state).dst);

            if !drm_rect_intersect(&mut dst_clip, &damage) {
                continue;
            }

            iosys_map_incr(
                &mut dst,
                fh::drm_fb_clip_offset(dst_pitch, dst_format, &dst_clip) as usize,
            );
            let blit = blit_to_crtc.expect("sysfb blit helper set by begin_fb_access");
            blit(
                &mut dst,
                &dst_pitch,
                (*shadow_plane_state).data.as_ptr(),
                fb,
                &damage,
                &mut (*shadow_plane_state).fmtcnv_state,
            );
        }
        drm_dev_exit(idx);
    }

    drm_gem_fb_end_cpu_access(fb, DMA_FROM_DEVICE);
}

// upstream: drm_sysfb_modeset.c drm_sysfb_plane_helper_atomic_disable()
pub unsafe extern "C" fn drm_sysfb_plane_helper_atomic_disable(
    plane: *mut DrmPlane,
    state: *mut DrmAtomicCommit,
) {
    let dev = (*plane).dev;
    let sysfb = to_drm_sysfb_device(dev);
    let dst: IosysMap = ptr::read(&(*sysfb).fb_addr);
    let plane_state = drm_atomic_get_new_plane_state(state, plane);
    let mut dst_vmap = dst._anon_0.vaddr_iomem.cast::<u8>(); // TODO (upstream): use mapping abstraction
    let dst_pitch = (*sysfb).fb_pitch;
    let dst_format = (*sysfb).fb_format;
    let mut dst_clip: DrmRect = zeroed();

    drm_rect_init(
        &mut dst_clip,
        ((*plane_state).src_x >> 16) as i32,
        ((*plane_state).src_y >> 16) as i32,
        ((*plane_state).src_w >> 16) as i32,
        ((*plane_state).src_h >> 16) as i32,
    );

    let lines = drm_rect_height(&dst_clip) as usize;
    let linepixels = drm_rect_width(&dst_clip) as usize;

    let mut idx: c_int = 0;
    if !drm_dev_enter(dev, &mut idx) {
        return;
    }

    // Clear buffer to black if disabled
    dst_vmap = dst_vmap.add(fh::drm_fb_clip_offset(dst_pitch, dst_format, &dst_clip) as usize);
    let cpp0 = (*dst_format)._anon_6.cpp[0] as usize;
    for _ in 0..lines {
        // memset_io()
        ptr::write_bytes(dst_vmap, 0, linepixels * cpp0);
        dst_vmap = dst_vmap.add(dst_pitch as usize);
    }

    drm_dev_exit(idx);
}

// upstream: drm_sysfb_modeset.c drm_sysfb_plane_helper_get_scanout_buffer()
/// `sb` is the `struct drm_scanout_buffer *` (see the module header).
pub unsafe extern "C" fn drm_sysfb_plane_helper_get_scanout_buffer(
    plane: *mut DrmPlane,
    sb: *mut c_void,
) -> i32 {
    let sysfb = to_drm_sysfb_device((*plane).dev);
    let sb = sb.cast::<DrmScanoutBuffer>();

    (*sb).width = (*sysfb).fb_mode.hdisplay as u32;
    (*sb).height = (*sysfb).fb_mode.vdisplay as u32;
    (*sb).format = (*sysfb).fb_format;
    (*sb).pitch[0] = (*sysfb).fb_pitch;
    (*sb).map[0] = ptr::read(&(*sysfb).fb_addr);
    0
}

// upstream: drm_sysfb_modeset.c drm_sysfb_plane_reset()
pub unsafe extern "C" fn drm_sysfb_plane_reset(plane: *mut DrmPlane) {
    if !(*plane).state.is_null() {
        drm_sysfb_plane_state_destroy(to_drm_sysfb_plane_state((*plane).state));
    }

    let sysfb_plane_state = kzalloc_obj::<DrmSysfbPlaneState>();
    if !sysfb_plane_state.is_null() {
        __drm_gem_reset_shadow_plane(plane, &mut (*sysfb_plane_state).base);
    } else {
        __drm_gem_reset_shadow_plane(plane, ptr::null_mut());
    }
}

// upstream: drm_sysfb_modeset.c drm_sysfb_plane_atomic_duplicate_state()
pub unsafe extern "C" fn drm_sysfb_plane_atomic_duplicate_state(
    plane: *mut DrmPlane,
) -> *mut DrmPlaneState {
    let plane_state = (*plane).state;
    if tk_linux_kapi::assert::warn_on(plane_state.is_null()) {
        return ptr::null_mut();
    }

    let sysfb_plane_state = to_drm_sysfb_plane_state(plane_state);

    let new_sysfb_plane_state = kzalloc_obj::<DrmSysfbPlaneState>();
    if new_sysfb_plane_state.is_null() {
        return ptr::null_mut();
    }

    let new_shadow_plane_state: *mut DrmShadowPlaneState = &mut (*new_sysfb_plane_state).base;
    __drm_gem_duplicate_shadow_plane_state(plane, new_shadow_plane_state);

    (*new_sysfb_plane_state).blit_to_crtc = (*sysfb_plane_state).blit_to_crtc;

    &mut (*new_shadow_plane_state).base as *mut DrmPlaneState
}

// upstream: drm_sysfb_modeset.c drm_sysfb_plane_atomic_destroy_state()
pub unsafe extern "C" fn drm_sysfb_plane_atomic_destroy_state(
    _plane: *mut DrmPlane,
    plane_state: *mut DrmPlaneState,
) {
    drm_sysfb_plane_state_destroy(to_drm_sysfb_plane_state(plane_state));
}

// ---------------------------------------------------------------------------
// CRTC
// ---------------------------------------------------------------------------

// upstream: drm_sysfb_modeset.c drm_sysfb_crtc_helper_mode_valid()
pub unsafe extern "C" fn drm_sysfb_crtc_helper_mode_valid(
    crtc: *mut DrmCrtc,
    mode: *mut DrmDisplayMode,
) -> DrmModeStatus {
    let sysfb = to_drm_sysfb_device((*crtc).dev);
    drm_crtc_helper_mode_valid_fixed(crtc, mode, &(*sysfb).fb_mode)
}

// upstream: drm_sysfb_modeset.c drm_sysfb_crtc_helper_atomic_check()
pub unsafe extern "C" fn drm_sysfb_crtc_helper_atomic_check(
    crtc: *mut DrmCrtc,
    new_state: *mut DrmAtomicCommit,
) -> i32 {
    let dev = (*crtc).dev;
    let sysfb = to_drm_sysfb_device(dev);
    let new_crtc_state = drm_atomic_get_new_crtc_state(new_state, crtc);

    if !(*new_crtc_state).enable {
        return 0;
    }

    let ret = ah::drm_atomic_helper_check_crtc_primary_plane(new_crtc_state);
    if ret != 0 {
        return ret;
    }

    if (*new_crtc_state).color_mgmt_changed() {
        let gamma_lut_length = (*sysfb).fb_gamma_lut_size as usize * size_of::<DrmColorLut>();
        let gamma_lut: *const DrmPropertyBlob = (*new_crtc_state).gamma_lut;

        if !gamma_lut.is_null() && (*gamma_lut).length != gamma_lut_length {
            return -EINVAL;
        }
    }

    0
}

// upstream: drm_sysfb_modeset.c drm_sysfb_crtc_reset()
pub unsafe extern "C" fn drm_sysfb_crtc_reset(crtc: *mut DrmCrtc) {
    let sysfb = to_drm_sysfb_device((*crtc).dev);

    if !(*crtc).state.is_null() {
        drm_sysfb_crtc_state_destroy(to_drm_sysfb_crtc_state((*crtc).state));
    }

    let sysfb_crtc_state = kzalloc_obj::<DrmSysfbCrtcState>();
    if !sysfb_crtc_state.is_null() {
        (*sysfb_crtc_state).format = (*sysfb).fb_format;
        __drm_atomic_helper_crtc_reset(crtc, &mut (*sysfb_crtc_state).base);
    } else {
        __drm_atomic_helper_crtc_reset(crtc, ptr::null_mut());
    }
}

// upstream: drm_sysfb_modeset.c drm_sysfb_crtc_atomic_duplicate_state()
pub unsafe extern "C" fn drm_sysfb_crtc_atomic_duplicate_state(
    crtc: *mut DrmCrtc,
) -> *mut DrmCrtcState {
    let crtc_state = (*crtc).state;
    if tk_linux_kapi::assert::warn_on(crtc_state.is_null()) {
        return ptr::null_mut();
    }

    let new_sysfb_crtc_state = kzalloc_obj::<DrmSysfbCrtcState>();
    if new_sysfb_crtc_state.is_null() {
        return ptr::null_mut();
    }

    let sysfb_crtc_state = to_drm_sysfb_crtc_state(crtc_state);
    __drm_atomic_helper_crtc_duplicate_state(crtc, &mut (*new_sysfb_crtc_state).base);
    (*new_sysfb_crtc_state).format = (*sysfb_crtc_state).format;

    &mut (*new_sysfb_crtc_state).base
}

// upstream: drm_sysfb_modeset.c drm_sysfb_crtc_atomic_destroy_state()
pub unsafe extern "C" fn drm_sysfb_crtc_atomic_destroy_state(
    _crtc: *mut DrmCrtc,
    crtc_state: *mut DrmCrtcState,
) {
    drm_sysfb_crtc_state_destroy(to_drm_sysfb_crtc_state(crtc_state));
}

// ---------------------------------------------------------------------------
// Connector
// ---------------------------------------------------------------------------

// upstream: drm_sysfb_modeset.c drm_sysfb_get_edid_block()
/// EDID block reader for `drm_edid_read_custom()`. `data` is the owning
/// `DrmSysfbDevice`. `buf` must be at least `len` bytes.
pub unsafe extern "C" fn drm_sysfb_get_edid_block(
    data: *mut c_void,
    buf: *mut u8,
    block: u32,
    len: usize,
) -> i32 {
    let sysfb = data.cast::<DrmSysfbDevice>();
    let edid = (*sysfb).edid;
    let off = block as usize * EDID_LENGTH;
    let end = off + len;

    if edid.is_null() {
        return -EINVAL;
    }
    if end > EDID_LENGTH {
        return -EINVAL;
    }

    ptr::copy_nonoverlapping(edid.add(off), buf, len);

    // We don't have EDID extensions available and reporting them will upset
    // DRM helpers. Clear the extension field and update the checksum.
    *buf.add(127) = (*buf.add(127)).wrapping_add(*buf.add(126));
    *buf.add(126) = 0;

    0
}

// upstream: drm_sysfb_modeset.c drm_sysfb_connector_helper_get_modes()
pub unsafe extern "C" fn drm_sysfb_connector_helper_get_modes(connector: *mut DrmConnector) -> i32 {
    let sysfb = to_drm_sysfb_device((*connector).dev);

    if !(*sysfb).edid.is_null() {
        let drm_edid =
            drm_edid_read_custom(connector, drm_sysfb_get_edid_block, sysfb.cast::<c_void>());
        drm_edid_connector_update(connector, drm_edid);
        drm_edid_free(drm_edid);
    }

    // Return the fixed mode even with EDID
    drm_connector_helper_get_modes_fixed(connector, &(*sysfb).fb_mode)
}

// ---------------------------------------------------------------------------
// Vtables (drm_sysfb_helper.h DRM_SYSFB_*_FUNCS macros)
// ---------------------------------------------------------------------------

// upstream: drm_sysfb_helper.h DRM_SYSFB_PLANE_HELPER_FUNCS
pub const fn DRM_SYSFB_PLANE_HELPER_FUNCS() -> DrmPlaneHelperFuncs {
    // SAFETY: the vtable is plain Option<fn> slots; all-zero means "unset".
    let mut f: DrmPlaneHelperFuncs = unsafe { zeroed() };
    f.begin_fb_access = Some(drm_sysfb_plane_helper_begin_fb_access);
    f.end_fb_access = Some(drm_gem_end_shadow_fb_access);
    f.atomic_check = Some(drm_sysfb_plane_helper_atomic_check);
    f.atomic_update = Some(drm_sysfb_plane_helper_atomic_update);
    f.atomic_disable = Some(drm_sysfb_plane_helper_atomic_disable);
    f.get_scanout_buffer = Some(drm_sysfb_plane_helper_get_scanout_buffer);
    f
}

// upstream: drm_sysfb_helper.h DRM_SYSFB_PLANE_FUNCS
pub const fn DRM_SYSFB_PLANE_FUNCS() -> DrmPlaneFuncs {
    // SAFETY: see DRM_SYSFB_PLANE_HELPER_FUNCS.
    let mut f: DrmPlaneFuncs = unsafe { zeroed() };
    f.reset = Some(drm_sysfb_plane_reset);
    f.update_plane = Some(ah::drm_atomic_helper_update_plane);
    f.disable_plane = Some(ah::drm_atomic_helper_disable_plane);
    f.atomic_duplicate_state = Some(drm_sysfb_plane_atomic_duplicate_state);
    f.atomic_destroy_state = Some(drm_sysfb_plane_atomic_destroy_state);
    f
}

// upstream: drm_sysfb_helper.h DRM_SYSFB_CRTC_HELPER_FUNCS
pub const fn DRM_SYSFB_CRTC_HELPER_FUNCS() -> DrmCrtcHelperFuncs {
    // SAFETY: see DRM_SYSFB_PLANE_HELPER_FUNCS.
    let mut f: DrmCrtcHelperFuncs = unsafe { zeroed() };
    f.mode_valid = Some(drm_sysfb_crtc_helper_mode_valid);
    f.atomic_check = Some(drm_sysfb_crtc_helper_atomic_check);
    f
}

// upstream: drm_sysfb_helper.h DRM_SYSFB_CRTC_FUNCS
pub const fn DRM_SYSFB_CRTC_FUNCS() -> DrmCrtcFuncs {
    // SAFETY: see DRM_SYSFB_PLANE_HELPER_FUNCS.
    let mut f: DrmCrtcFuncs = unsafe { zeroed() };
    f.reset = Some(drm_sysfb_crtc_reset);
    f.set_config = Some(ah::drm_atomic_helper_set_config);
    f.page_flip = Some(ah::drm_atomic_helper_page_flip);
    f.atomic_duplicate_state = Some(drm_sysfb_crtc_atomic_duplicate_state);
    f.atomic_destroy_state = Some(drm_sysfb_crtc_atomic_destroy_state);
    f
}

// upstream: drm_sysfb_helper.h DRM_SYSFB_CONNECTOR_HELPER_FUNCS
pub const fn DRM_SYSFB_CONNECTOR_HELPER_FUNCS() -> DrmConnectorHelperFuncs {
    // SAFETY: see DRM_SYSFB_PLANE_HELPER_FUNCS.
    let mut f: DrmConnectorHelperFuncs = unsafe { zeroed() };
    f.get_modes = Some(drm_sysfb_connector_helper_get_modes);
    f
}

// upstream: drm_sysfb_helper.h DRM_SYSFB_CONNECTOR_FUNCS
pub const fn DRM_SYSFB_CONNECTOR_FUNCS() -> DrmConnectorFuncs {
    // SAFETY: see DRM_SYSFB_PLANE_HELPER_FUNCS.
    let mut f: DrmConnectorFuncs = unsafe { zeroed() };
    f.reset = Some(drm_atomic_helper_connector_reset);
    f.fill_modes = Some(drm_helper_probe_single_connector_modes);
    f.atomic_duplicate_state = Some(drm_atomic_helper_connector_duplicate_state);
    f.atomic_destroy_state = Some(drm_atomic_helper_connector_destroy_state);
    f
}

// The `fb_create` slot is declared with mutable format and command pointers;
// the Linux callback takes const pointers. This shim adapts the slot type.
// upstream: drm_sysfb_helper.h DRM_SYSFB_MODE_CONFIG_FUNCS (.fb_create)
unsafe extern "C" fn drm_sysfb_fb_create(
    dev: *mut DrmDevice,
    file: *mut DrmFile,
    info: *mut DrmFormatInfo,
    mode_cmd: *mut DrmModeFbCmd2,
) -> *mut DrmFramebuffer {
    drm_gem_fb_create_with_dirty(dev, file, info, mode_cmd)
}

// upstream: drm_sysfb_helper.h DRM_SYSFB_MODE_CONFIG_FUNCS
pub const fn DRM_SYSFB_MODE_CONFIG_FUNCS() -> DrmModeConfigFuncs {
    // SAFETY: see DRM_SYSFB_PLANE_HELPER_FUNCS.
    let mut f: DrmModeConfigFuncs = unsafe { zeroed() };
    f.fb_create = Some(drm_sysfb_fb_create);
    f.atomic_check = Some(ah::drm_atomic_helper_check);
    f.atomic_commit = Some(ah::drm_atomic_helper_commit);
    f
}

/// Installs the helper vtable on a plane (`drm_plane_helper_add()`, inline in
/// include/drm/drm_modeset_helper_vtables.h).
///
/// # Safety
/// `plane` must be a live, initialized `DrmPlane`; `funcs` must outlive it.
#[unsafe(export_name = "drm_plane_helper_add")]
pub unsafe extern "C" fn drm_plane_helper_add(plane: *mut DrmPlane, funcs: *const DrmPlaneHelperFuncs) {
    (*plane).helper_private = funcs as *mut DrmPlaneHelperFuncs;
}

/// Installs the helper vtable on a CRTC (`drm_crtc_helper_add()`).
///
/// # Safety
/// `crtc` must be a live, initialized `DrmCrtc`; `funcs` must outlive it.
pub unsafe fn drm_crtc_helper_add(crtc: *mut DrmCrtc, funcs: *const DrmCrtcHelperFuncs) {
    (*crtc).helper_private = funcs as *mut DrmCrtcHelperFuncs;
}

/// Installs the helper vtable on a connector (`drm_connector_helper_add()`).
///
/// # Safety
/// `connector` must be a live, initialized `DrmConnector`; `funcs` must
/// outlive it.
#[unsafe(export_name = "drm_connector_helper_add")]
pub unsafe extern "C" fn drm_connector_helper_add(
    connector: *mut DrmConnector,
    funcs: *const DrmConnectorHelperFuncs,
) {
    (*connector).helper_private = funcs as *mut DrmConnectorHelperFuncs;
}
