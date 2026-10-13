// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! ADL-P cursor-plane paths from Linux v7.2.3 `intel_cursor.c`.
//!
//! This keeps the gen9+ cursor validation, signed position encoding, cursor
//! size/rotation policy, watermark programming and register arm order. The
//! legacy cursor ioctl also keeps Linux's same-mode vblank/async fastpath;
//! eDP PSR-specific wait/lock helpers remain outside the configured build.
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::{ffi::c_void, mem::offset_of};

use tk_drm_upstream::{
    drm_crtc_types::{DrmCrtc, DrmCrtcState},
    drm_device_types::DrmDevice,
    drm_fourcc_types::DrmFormatInfo,
    drm_framebuffer_types::DrmFramebuffer,
    drm_modeset_lock_types::DrmModesetAcquireCtx,
    drm_plane_types::{DRM_PLANE_TYPE_CURSOR, DrmPlane, DrmPlaneFuncs, DrmPlaneState},
    drm_rect_types::DrmRect,
    drm_vblank_work_types::DrmVblankWork,
    kernel_placeholder_types::KthreadWork,
    uapi_drm_consts::{
        DRM_FORMAT_ARGB8888, DRM_FORMAT_MOD_LINEAR, DRM_MODE_ROTATE_0, DRM_MODE_ROTATE_180,
    },
    uapi_drm_mode_types::DrmPlaneSizeHint,
};

use crate::{
    intel_cursor_regs_upstream::*,
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_limits_types_upstream::{PLANE_CURSOR, Pipe, PlaneId},
    intel_display_power_types_upstream::{IntelDisplayPowerDomain, POWER_DOMAIN_PIPE_A},
    intel_display_reg_defs_upstream::reg_field_prep,
    intel_display_types_upstream::{
        IntelCrtc, IntelCrtcState, IntelFramebuffer, IntelPlane, IntelPlaneError, IntelPlaneState,
    },
    intel_display_wa_upstream::{__intel_display_wa, DISPLAY_VER, IntelDisplayWa},
    intel_workarounds_types_upstream::I915RegT,
};

const EINVAL: i32 = tk_linux_kapi::config::EINVAL;
const DRM_PLANE_NO_SCALING: i32 = 1 << 16;
const INTEL_PLANE_CAP_NONE: u8 = 0;

unsafe extern "C" {
    fn intel_plane_alloc() -> *mut IntelPlane;
    fn intel_plane_free(plane: *mut IntelPlane);
    fn intel_plane_destroy(plane: *mut DrmPlane);
    fn intel_plane_duplicate_state(plane: *mut DrmPlane) -> *mut DrmPlaneState;
    fn intel_plane_destroy_state(plane: *mut DrmPlane, state: *mut DrmPlaneState);
    fn intel_plane_unpin_fb(plane_state: *mut IntelPlaneState);
    fn intel_plane_atomic_check_with_state(
        old_crtc: *const IntelCrtcState,
        new_crtc: *mut IntelCrtcState,
        old_plane: *const IntelPlaneState,
        new_plane: *mut IntelPlaneState,
    ) -> i32;
    fn intel_plane_copy_uapi_to_hw_state(
        state: *mut IntelPlaneState,
        from: *const IntelPlaneState,
        crtc: *mut IntelCrtc,
    );
    fn intel_plane_pin_fb(
        plane_state: *mut IntelPlaneState,
        old_plane_state: *const IntelPlaneState,
    ) -> i32;
    fn intel_plane_update_noarm(
        dsb: *mut c_void,
        plane: *mut IntelPlane,
        crtc_state: *const IntelCrtcState,
        plane_state: *const IntelPlaneState,
    );
    fn intel_plane_update_arm(
        dsb: *mut c_void,
        plane: *mut IntelPlane,
        crtc_state: *const IntelCrtcState,
        plane_state: *const IntelPlaneState,
    );
    fn intel_plane_disable_arm(
        dsb: *mut c_void,
        plane: *mut IntelPlane,
        crtc_state: *const IntelCrtcState,
    );
    fn intel_crtc_duplicate_state(crtc: *mut DrmCrtc) -> *mut DrmCrtcState;
    fn intel_crtc_destroy_state(crtc: *mut DrmCrtc, state: *mut DrmCrtcState);
    fn intel_crtc_needs_modeset(state: *const IntelCrtcState) -> bool;
    fn intel_vblank_evade_init(
        old_state: *const IntelCrtcState,
        new_state: *const IntelCrtcState,
        evade: *mut crate::intel_vblank_types_upstream::IntelVblankEvadeCtx,
    );
    fn intel_vblank_evade(
        evade: *mut crate::intel_vblank_types_upstream::IntelVblankEvadeCtx,
    ) -> i32;
    fn intel_frontbuffer_flush(
        front: *mut crate::intel_frontbuffer_types_upstream::IntelFrontbuffer,
        origin: i32,
    );
    fn intel_frontbuffer_track(
        old: *mut crate::intel_frontbuffer_types_upstream::IntelFrontbuffer,
        new: *mut crate::intel_frontbuffer_types_upstream::IntelFrontbuffer,
        frontbuffer_bits: u32,
    );
    fn intel_plane_format_mod_supported_async(
        plane: *mut DrmPlane,
        format: u32,
        modifier: u64,
    ) -> bool;
    fn intel_plane_helper_add(plane: *mut IntelPlane);
    fn intel_plane_check_clipping(
        plane_state: *mut IntelPlaneState,
        crtc_state: *mut IntelCrtcState,
        min_scale: i32,
        max_scale: i32,
        can_position: bool,
    ) -> i32;
    fn intel_plane_check_src_coordinates(plane_state: *mut IntelPlaneState) -> i32;
    fn intel_plane_compute_gtt(plane_state: *mut IntelPlaneState) -> i32;
    fn intel_plane_compute_aligned_offset(
        x: *mut i32,
        y: *mut i32,
        plane_state: *const IntelPlaneState,
        color_plane: i32,
    ) -> u32;
    fn intel_add_fb_offsets(
        x: *mut i32,
        y: *mut i32,
        plane_state: *const IntelPlaneState,
        color_plane: i32,
    );
    fn intel_scanout_needs_vtd_wa(display: *mut IntelDisplay) -> bool;
    fn intel_fb_plane_get_modifiers(display: *mut IntelDisplay, caps: u8) -> *mut u64;
    fn intel_fb_plane_supports_modifier(plane: *mut IntelPlane, modifier: u64) -> bool;
    fn intel_display_power_get_if_enabled(
        display: *mut IntelDisplay,
        domain: IntelDisplayPowerDomain,
    ) -> *mut c_void;
    fn intel_display_power_put(
        display: *mut IntelDisplay,
        domain: IntelDisplayPowerDomain,
        wakeref: *mut c_void,
    );
    fn intel_de_read(display: *mut IntelDisplay, reg: I915RegT) -> u32;
    fn intel_de_write_dsb(display: *mut IntelDisplay, dsb: *mut c_void, reg: I915RegT, val: u32);
    fn intel_de_write_fw(display: *mut IntelDisplay, reg: I915RegT, val: u32);
    fn intel_de_write(display: *mut IntelDisplay, reg: I915RegT, val: u32);
    fn drm_atomic_helper_update_plane(
        plane: *mut DrmPlane,
        crtc: *mut DrmCrtc,
        fb: *mut DrmFramebuffer,
        crtc_x: i32,
        crtc_y: i32,
        crtc_w: u32,
        crtc_h: u32,
        src_x: u32,
        src_y: u32,
        src_w: u32,
        src_h: u32,
        ctx: *mut DrmModesetAcquireCtx,
    ) -> i32;
    fn drm_atomic_helper_disable_plane(plane: *mut DrmPlane, ctx: *mut DrmModesetAcquireCtx)
    -> i32;
    fn drm_universal_plane_init(
        dev: *mut DrmDevice,
        plane: *mut DrmPlane,
        possible_crtcs: u32,
        funcs: *const DrmPlaneFuncs,
        formats: *const u32,
        format_count: u32,
        modifiers: *const u64,
        plane_type: u32,
        name: *const core::ffi::c_char,
    ) -> i32;
    fn drm_plane_create_rotation_property(
        plane: *mut DrmPlane,
        rotation: u32,
        supported: u32,
    ) -> i32;
    fn drm_plane_create_zpos_immutable_property(plane: *mut DrmPlane, zpos: u32) -> i32;
    fn drm_plane_enable_fb_damage_clips(plane: *mut DrmPlane);
    fn drm_plane_add_size_hints_property(
        plane: *mut DrmPlane,
        hints: *const DrmPlaneSizeHint,
        count: i32,
    ) -> i32;
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
unsafe fn display_from_drm(dev: *mut DrmDevice) -> *mut IntelDisplay {
    if dev.is_null() || unsafe { (*dev).dev_private.is_null() } {
        return core::ptr::null_mut();
    }
    let i915 = unsafe {
        (*dev)
            .dev_private
            .cast::<crate::linux_i915_private::DrmI915Private>()
    };
    unsafe { (*i915).display.cast::<IntelDisplay>() }
}

#[inline]
unsafe fn platform_bit(display: *const IntelDisplay, bit: usize) -> bool {
    if display.is_null() || bit >= 72 {
        return false;
    }
    unsafe {
        let bytes = core::ptr::addr_of!((*display).platform).cast::<u8>();
        let byte = bytes.add(bit / 8).read();
        byte & (1 << (bit % 8)) != 0
    }
}

#[inline]
unsafe fn display_from_plane_state(plane_state: *const IntelPlaneState) -> *mut IntelDisplay {
    if plane_state.is_null() || unsafe { (*plane_state).uapi.plane.is_null() } {
        return core::ptr::null_mut();
    }
    unsafe { display_from_drm((*(*plane_state).uapi.plane).dev) }
}

#[inline]
unsafe fn display_from_crtc_state(crtc_state: *const IntelCrtcState) -> *mut IntelDisplay {
    if crtc_state.is_null() || unsafe { (*crtc_state).uapi.crtc.is_null() } {
        return core::ptr::null_mut();
    }
    let crtc = unsafe { (*crtc_state).uapi.crtc };
    unsafe { display_from_drm((*crtc).dev) }
}

#[inline]
unsafe fn intel_plane_from_drm(plane: *mut DrmPlane) -> *mut IntelPlane {
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
fn cursor_dimensions_valid(width: i32, height: i32, max_width: u32, max_height: u32) -> bool {
    width > 0 && height > 0 && width as u32 <= max_width && height as u32 <= max_height
}

fn cursor_size_valid(
    width: i32,
    height: i32,
    max_width: u32,
    max_height: u32,
    has_cur_fbc: bool,
    rotation: u32,
) -> bool {
    if !cursor_dimensions_valid(width, height, max_width, max_height)
        || !matches!(width, 64 | 128 | 256)
    {
        return false;
    }
    if has_cur_fbc && rotation & DRM_MODE_ROTATE_0 != 0 {
        (8..=width).contains(&height)
    } else {
        height == width
    }
}

fn cursor_position_value(
    x: i32,
    y: i32,
    height: i32,
    early_tpt: bool,
    update_y: i32,
) -> Option<u32> {
    let y = if early_tpt {
        let y = i64::from(y) - i64::from(update_y);
        y.max(-i64::from(height) + 1)
    } else {
        i64::from(y)
    };
    let x = i64::from(x);
    let xmag = x.unsigned_abs();
    let ymag = y.unsigned_abs();
    if xmag > u64::from(CURSOR_POS_X_MASK >> 0) || ymag > u64::from(CURSOR_POS_Y_MASK >> 16) {
        return None;
    }
    let mut pos = 0;
    if x < 0 {
        pos |= CURSOR_POS_X_SIGN;
    }
    if y < 0 {
        pos |= CURSOR_POS_Y_SIGN;
    }
    pos |= CURSOR_POS_X(xmag as u32);
    pos |= CURSOR_POS_Y(ymag as u32);
    Some(pos)
}

fn cursor_ctl(width: i32, rotation: u32, wa_22012358565: bool) -> u32 {
    let mut ctl = match width {
        64 => MCURSOR_MODE_64_ARGB_AX,
        128 => MCURSOR_MODE_128_ARGB_AX,
        256 => MCURSOR_MODE_256_ARGB_AX,
        _ => return 0,
    };
    if rotation & DRM_MODE_ROTATE_180 != 0 {
        ctl |= MCURSOR_ROTATE_180;
    }
    if wa_22012358565 {
        ctl |= MCURSOR_ARB_SLOTS(1);
    }
    ctl
}

#[inline]
fn cursor_format_supported(format: u32) -> bool {
    format == DRM_FORMAT_ARGB8888
}

fn has_cursor_fbc(display: *mut IntelDisplay) -> bool {
    !display.is_null() && unsafe { matches!(DISPLAY_VER(display), 7 | 13) }
}

unsafe fn cursor_position(
    crtc_state: *const IntelCrtcState,
    plane_state: *const IntelPlaneState,
    early_tpt: bool,
) -> u32 {
    if crtc_state.is_null() || plane_state.is_null() {
        return 0;
    }
    let dst = unsafe { &(*plane_state).uapi.dst };
    let update_y = if early_tpt {
        unsafe { (*crtc_state).psr2_su_area.y1 }
    } else {
        0
    };
    cursor_position_value(
        dst.x1,
        dst.y1,
        dst.y2.saturating_sub(dst.y1),
        early_tpt,
        update_y,
    )
    .unwrap_or(0)
}

#[inline]
unsafe fn cursor_size_ok(plane_state: *const IntelPlaneState) -> bool {
    if plane_state.is_null() || unsafe { (*plane_state).uapi.plane.is_null() } {
        return false;
    }
    let display = unsafe { display_from_plane_state(plane_state) };
    if display.is_null() || unsafe { (*display).drm.is_null() } {
        return false;
    }
    let config = unsafe { &(*(*display).drm).mode_config };
    let dst = unsafe { &(*plane_state).uapi.dst };
    cursor_dimensions_valid(
        dst.x2 - dst.x1,
        dst.y2 - dst.y1,
        config.cursor_width,
        config.cursor_height,
    )
}

unsafe fn i9xx_cursor_size_ok(plane_state: *const IntelPlaneState) -> bool {
    if plane_state.is_null() || unsafe { (*plane_state).uapi.plane.is_null() } {
        return false;
    }
    let display = unsafe { display_from_plane_state(plane_state) };
    if display.is_null() || !unsafe { cursor_size_ok(plane_state) } {
        return false;
    }
    let config = unsafe { &(*(*display).drm).mode_config };
    let dst = unsafe { &(*plane_state).uapi.dst };
    cursor_size_valid(
        dst.x2 - dst.x1,
        dst.y2 - dst.y1,
        config.cursor_width,
        config.cursor_height,
        has_cursor_fbc(display),
        unsafe { (*plane_state).hw.rotation },
    )
}

unsafe fn cursor_check_surface(plane_state: *mut IntelPlaneState) -> i32 {
    if plane_state.is_null() || unsafe { (*plane_state).uapi.plane.is_null() } {
        return -EINVAL;
    }
    let display = unsafe { display_from_plane_state(plane_state) };
    let plane = unsafe { intel_plane_from_drm((*plane_state).uapi.plane) };
    if display.is_null() || plane.is_null() {
        return -EINVAL;
    }
    let ret = unsafe { intel_plane_compute_gtt(plane_state) };
    if ret != 0 || !unsafe { (*plane_state).uapi.visible } {
        return ret;
    }
    let mut src_x = unsafe { (*plane_state).uapi.src.x1 >> 16 };
    let mut src_y = unsafe { (*plane_state).uapi.src.y1 >> 16 };
    unsafe { intel_add_fb_offsets(&mut src_x, &mut src_y, plane_state, 0) };
    let offset =
        unsafe { intel_plane_compute_aligned_offset(&mut src_x, &mut src_y, plane_state, 0) };
    if src_x != 0 || src_y != 0 {
        return -EINVAL;
    }

    // drm_rect_translate_to(): keep the source extent but return its origin
    // to the post-offset coordinates so the generic source-bound check sees
    // the exact programmed view.
    let src = unsafe { &mut (*plane_state).uapi.src };
    let width = src.x2 - src.x1;
    let height = src.y2 - src.y1;
    src.x1 = src_x.wrapping_shl(16);
    src.y1 = src_y.wrapping_shl(16);
    src.x2 = src.x1.wrapping_add(width);
    src.y2 = src.y1.wrapping_add(height);

    // ADL-P is not GMCH, so the old 180-degree framebuffer-offset workaround
    // is not part of this configured path.
    unsafe {
        (*plane_state).view.color_plane[0].offset = offset;
        (*plane_state).view.color_plane[0].x = src_x as u32;
        (*plane_state).view.color_plane[0].y = src_y as u32;
    }
    0
}

unsafe fn check_cursor(crtc_state: *mut IntelCrtcState, plane_state: *mut IntelPlaneState) -> i32 {
    if crtc_state.is_null() || plane_state.is_null() {
        return -EINVAL;
    }
    let display = unsafe { display_from_plane_state(plane_state) };
    if display.is_null() {
        return -EINVAL;
    }
    let fb = unsafe { (*plane_state).hw.fb };
    if !fb.is_null() && unsafe { (*fb).modifier != DRM_FORMAT_MOD_LINEAR } {
        return -EINVAL;
    }

    let original_src = unsafe {
        let src = &(*plane_state).uapi.src;
        DrmRect {
            x1: src.x1,
            y1: src.y1,
            x2: src.x2,
            y2: src.y2,
        }
    };
    let original_dst = unsafe {
        let dst = &(*plane_state).uapi.dst;
        DrmRect {
            x1: dst.x1,
            y1: dst.y1,
            x2: dst.x2,
            y2: dst.y2,
        }
    };
    let ret = unsafe {
        intel_plane_check_clipping(
            plane_state,
            crtc_state,
            DRM_PLANE_NO_SCALING,
            DRM_PLANE_NO_SCALING,
            true,
        )
    };
    if ret != 0 {
        return ret;
    }
    unsafe {
        (*plane_state).uapi.src = original_src;
        (*plane_state).uapi.dst = original_dst;
        let pipe_src = &(*crtc_state).pipe_src;
        (*plane_state).uapi.dst.x1 -= pipe_src.x1;
        (*plane_state).uapi.dst.x2 -= pipe_src.x1;
        (*plane_state).uapi.dst.y1 -= pipe_src.y1;
        (*plane_state).uapi.dst.y2 -= pipe_src.y1;
    }

    let ret = unsafe { cursor_check_surface(plane_state) };
    if ret != 0 || !unsafe { (*plane_state).uapi.visible } {
        return ret;
    }
    unsafe { intel_plane_check_src_coordinates(plane_state) }
}

unsafe fn i9xx_cursor_ctl(plane_state: *mut IntelPlaneState) -> u32 {
    if plane_state.is_null() {
        return 0;
    }
    let width = unsafe { (*plane_state).uapi.dst.x2 - (*plane_state).uapi.dst.x1 };
    let display = unsafe { display_from_plane_state(plane_state) };
    let rotation = unsafe { (*plane_state).hw.rotation };
    let workaround = !display.is_null()
        && unsafe { __intel_display_wa(display, IntelDisplayWa::Wa22012358565, core::ptr::null()) };
    cursor_ctl(width, rotation, workaround)
}

unsafe fn i9xx_check_cursor(
    crtc_state: *mut IntelCrtcState,
    plane_state: *mut IntelPlaneState,
) -> i32 {
    let ret = unsafe { check_cursor(crtc_state, plane_state) };
    if ret != 0 {
        return ret;
    }
    let fb = unsafe { (*plane_state).hw.fb };
    if fb.is_null() {
        return 0;
    }
    if !unsafe { i9xx_cursor_size_ok(plane_state) } {
        return -EINVAL;
    }
    let width = unsafe { (*plane_state).uapi.dst.x2 - (*plane_state).uapi.dst.x1 };
    let pitch = unsafe { (*fb).pitches[0] };
    let cpp = unsafe { (*(*fb).format)._anon_6.cpp[0] };
    if unsafe { (*plane_state).uapi.visible }
        && unsafe { (*plane_state).view.color_plane[0].mapping_stride != pitch }
    {
        // Upstream drm_WARN_ON only; the hard check below remains authoritative.
    }
    if pitch != width as u32 * u32::from(cpp) {
        return -EINVAL;
    }
    unsafe { (*plane_state).ctl = i9xx_cursor_ctl(plane_state) };
    0
}

fn cursor_wm_reg_val(level: &crate::intel_display_types_upstream::SklWmLevel) -> u32 {
    let mut value = 0;
    if level.enable {
        value |= CUR_WM_EN;
    }
    if level.ignore_lines {
        value |= CUR_WM_IGNORE_LINES;
    }
    value |= reg_field_prep(CUR_WM_BLOCKS_MASK, u32::from(level.blocks));
    value |= reg_field_prep(CUR_WM_LINES_MASK, u32::from(level.lines));
    value
}

fn cursor_ddb_reg_val(entry: &crate::intel_wm_types_upstream::SklDdbEntry) -> u32 {
    if entry.end == 0 {
        0
    } else {
        CUR_BUF_END(u32::from(entry.end - 1)) | CUR_BUF_START(u32::from(entry.start))
    }
}

unsafe fn write_cursor_wm(
    dsb: *mut c_void,
    plane: *mut IntelPlane,
    crtc_state: *const IntelCrtcState,
) {
    if plane.is_null() || crtc_state.is_null() {
        return;
    }
    let display = unsafe { display_from_drm((*plane).base.dev) };
    if display.is_null() {
        return;
    }
    let pipe = unsafe { (*plane).pipe as u32 };
    let plane_id = unsafe { (*plane).id };
    let levels = unsafe { (*display).wm.num_levels };
    let wm_state = unsafe { &(*crtc_state).wm._anon_0_0.skl };
    let pipe_wm = &wm_state.optimal;
    let ddb = &wm_state.plane_ddb[plane_id as usize];
    for level in 0..levels {
        let entry = unsafe { skl_plane_wm_level(pipe_wm, plane_id, i32::from(level)) };
        if !entry.is_null() {
            unsafe {
                intel_de_write_dsb(
                    display,
                    dsb,
                    CUR_WM(pipe, u32::from(level)),
                    cursor_wm_reg_val(&*entry),
                );
            }
        }
    }
    let trans = unsafe { skl_plane_trans_wm(pipe_wm, plane_id) };
    if !trans.is_null() {
        unsafe {
            intel_de_write_dsb(display, dsb, CUR_WM_TRANS(pipe), cursor_wm_reg_val(&*trans));
        }
    }
    if unsafe { DISPLAY_VER(display) >= 13 && !platform_bit(display, 2) } {
        let wm = &pipe_wm.planes[plane_id as usize];
        unsafe {
            intel_de_write_dsb(
                display,
                dsb,
                CUR_WM_SAGV(pipe),
                cursor_wm_reg_val(&wm.sagv.wm0),
            );
            intel_de_write_dsb(
                display,
                dsb,
                CUR_WM_SAGV_TRANS(pipe),
                cursor_wm_reg_val(&wm.sagv.trans_wm),
            );
        }
    }
    unsafe { intel_de_write_dsb(display, dsb, CUR_BUF_CFG(pipe), cursor_ddb_reg_val(ddb)) };
}

unsafe fn i9xx_cursor_update_arm(
    dsb: *mut c_void,
    plane: *mut IntelPlane,
    crtc_state: *const IntelCrtcState,
    plane_state: *const IntelPlaneState,
) {
    if plane.is_null() || crtc_state.is_null() {
        return;
    }
    let display = unsafe { display_from_crtc_state(crtc_state) };
    if display.is_null() {
        return;
    }
    let pipe = unsafe { (*plane).pipe as u32 };
    let mut ctl = 0;
    let mut base = 0;
    let mut pos = 0;
    let mut fbc_ctl = 0;
    if !plane_state.is_null() && unsafe { (*plane_state).uapi.visible } {
        let state = unsafe { &*plane_state };
        let width = state.uapi.dst.x2 - state.uapi.dst.x1;
        let height = state.uapi.dst.y2 - state.uapi.dst.y1;
        // The gen11+ ADL-P cursor has no per-pipe gamma/CSC control bits.
        ctl = state.ctl;
        if unsafe { DISPLAY_VER(display) < 14 && width != height } {
            fbc_ctl = CUR_FBC_EN | CUR_FBC_HEIGHT((height - 1) as u32);
        }
        base = state.surf;
        pos = unsafe { cursor_position(crtc_state, plane_state, false) };
    }

    if unsafe { DISPLAY_VER(display) >= 9 } {
        unsafe { write_cursor_wm(dsb, plane, crtc_state) };
    }
    if !plane_state.is_null() {
        unsafe { i9xx_cursor_update_sel_fetch_arm(dsb, plane, crtc_state, plane_state) };
    } else {
        unsafe { i9xx_cursor_disable_sel_fetch_arm(dsb, plane, crtc_state) };
    }
    if unsafe {
        plane.as_ref().is_some_and(|p| {
            p.cursor.base != base || p.cursor.size != fbc_ctl || p.cursor.cntl != ctl
        })
    } {
        if has_cursor_fbc(display) {
            unsafe { intel_de_write_dsb(display, dsb, CUR_FBC_CTL(display, pipe), fbc_ctl) };
        }
        // Preserve source register-arm order: control, position, then base.
        unsafe {
            intel_de_write_dsb(display, dsb, CURCNTR(display, pipe), ctl);
            intel_de_write_dsb(display, dsb, CURPOS(display, pipe), pos);
            intel_de_write_dsb(display, dsb, CURBASE(display, pipe), base);
            (*plane).cursor.base = base;
            (*plane).cursor.size = fbc_ctl;
            (*plane).cursor.cntl = ctl;
        }
    } else {
        unsafe {
            intel_de_write_dsb(display, dsb, CURPOS(display, pipe), pos);
            intel_de_write_dsb(display, dsb, CURBASE(display, pipe), base);
        }
    }
}

unsafe extern "C" fn i9xx_cursor_update_arm_callback(
    dsb: *mut c_void,
    plane: *mut IntelPlane,
    crtc_state: *mut IntelCrtcState,
    plane_state: *mut IntelPlaneState,
) {
    unsafe { i9xx_cursor_update_arm(dsb, plane, crtc_state, plane_state) };
}

unsafe extern "C" fn i9xx_cursor_disable_arm_callback(
    dsb: *mut c_void,
    plane: *mut IntelPlane,
    crtc_state: *mut IntelCrtcState,
) {
    unsafe { i9xx_cursor_update_arm(dsb, plane, crtc_state, core::ptr::null()) };
}

unsafe fn i9xx_cursor_disable_sel_fetch_arm(
    dsb: *mut c_void,
    plane: *mut IntelPlane,
    crtc_state: *const IntelCrtcState,
) {
    if plane.is_null() || crtc_state.is_null() || !unsafe { (*crtc_state).enable_psr2_sel_fetch } {
        return;
    }
    let display = unsafe { display_from_drm((*plane).base.dev) };
    if !display.is_null() {
        unsafe {
            intel_de_write_dsb(display, dsb, SEL_FETCH_CUR_CTL((*plane).pipe as u32), 0);
        }
    }
}

unsafe fn wa_16021440873(
    dsb: *mut c_void,
    plane: *mut IntelPlane,
    crtc_state: *const IntelCrtcState,
    plane_state: *const IntelPlaneState,
) {
    if plane.is_null() || crtc_state.is_null() || plane_state.is_null() {
        return;
    }
    let display = unsafe { display_from_drm((*plane).base.dev) };
    if display.is_null() {
        return;
    }
    let ctl = (unsafe { (*plane_state).ctl } & !MCURSOR_MODE_MASK) | MCURSOR_MODE_64_2B;
    let early_y = unsafe { ((*crtc_state).pipe_src.y2 - (*crtc_state).pipe_src.y1 + 1) as u32 };
    unsafe {
        intel_de_write_dsb(display, dsb, SEL_FETCH_CUR_CTL((*plane).pipe as u32), ctl);
        intel_de_write_dsb(
            display,
            dsb,
            CURPOS_ERLY_TPT(display, (*plane).pipe as u32),
            CURSOR_POS_Y(early_y),
        );
    }
}

unsafe fn i9xx_cursor_update_sel_fetch_arm(
    dsb: *mut c_void,
    plane: *mut IntelPlane,
    crtc_state: *const IntelCrtcState,
    plane_state: *const IntelPlaneState,
) {
    if plane.is_null() || crtc_state.is_null() || !unsafe { (*crtc_state).enable_psr2_sel_fetch } {
        return;
    }
    let display = unsafe { display_from_drm((*plane).base.dev) };
    if display.is_null() {
        return;
    }

    let has_update = !plane_state.is_null()
        && unsafe { (*plane_state).psr2_sel_fetch_area.y2 > (*plane_state).psr2_sel_fetch_area.y1 };
    if has_update {
        if unsafe { (*crtc_state).enable_psr2_su_region_et } {
            let early_pos = unsafe { cursor_position(crtc_state, plane_state, true) };
            unsafe {
                intel_de_write_dsb(
                    display,
                    dsb,
                    CURPOS_ERLY_TPT(display, (*plane).pipe as u32),
                    early_pos,
                );
            }
        }
        unsafe {
            intel_de_write_dsb(
                display,
                dsb,
                SEL_FETCH_CUR_CTL((*plane).pipe as u32),
                (*plane_state).ctl,
            );
        }
    } else if unsafe { (*crtc_state).enable_psr2_su_region_et } {
        // Wa_16021440873: keep the selective-fetch cursor above the region.
        unsafe { wa_16021440873(dsb, plane, crtc_state, plane_state) };
    } else {
        unsafe { i9xx_cursor_disable_sel_fetch_arm(dsb, plane, crtc_state) };
    }
}

unsafe extern "C" fn i9xx_cursor_get_hw_state(plane: *mut IntelPlane, pipe: *mut Pipe) -> bool {
    if plane.is_null() || pipe.is_null() {
        return false;
    }
    let display = unsafe {
        let dev = (*plane).base.dev;
        if dev.is_null() || (*dev).dev_private.is_null() {
            core::ptr::null_mut()
        } else {
            let i915 = (*dev)
                .dev_private
                .cast::<crate::linux_i915_private::DrmI915Private>();
            (*i915).display.cast::<IntelDisplay>()
        }
    };
    if display.is_null() {
        return false;
    }
    let domain = POWER_DOMAIN_PIPE_A + unsafe { (*plane).pipe as IntelDisplayPowerDomain };
    let wakeref = unsafe { intel_display_power_get_if_enabled(display, domain) };
    if wakeref.is_null() {
        return false;
    }
    let value = unsafe { intel_de_read(display, CURCNTR(display, (*plane).pipe as u32)) };
    let enabled = value & MCURSOR_MODE_MASK != 0;
    unsafe {
        *pipe = (*plane).pipe;
        intel_display_power_put(display, domain, wakeref);
    }
    enabled
}

unsafe extern "C" fn i9xx_cursor_surf_offset(state: *mut IntelPlaneState) -> u32 {
    if state.is_null() {
        0
    } else {
        unsafe { (*state).view.color_plane[0].offset }
    }
}

unsafe extern "C" fn i9xx_cursor_max_stride(
    plane: *mut IntelPlane,
    _info: *mut DrmFormatInfo,
    _modifier: u64,
    _rotation: u32,
) -> u32 {
    if plane.is_null() || unsafe { (*plane).base.dev.is_null() } {
        return 0;
    }
    unsafe {
        (*(*plane).base.dev)
            .mode_config
            .cursor_width
            .saturating_mul(4)
    }
}

unsafe extern "C" fn i9xx_cursor_min_alignment(
    plane: *mut IntelPlane,
    _fb: *mut DrmFramebuffer,
    _color_plane: i32,
) -> u32 {
    if plane.is_null() {
        return 4096;
    }
    let display = unsafe {
        let dev = (*plane).base.dev;
        if dev.is_null() || (*dev).dev_private.is_null() {
            core::ptr::null_mut()
        } else {
            let i915 = (*dev)
                .dev_private
                .cast::<crate::linux_i915_private::DrmI915Private>();
            (*i915).display.cast::<IntelDisplay>()
        }
    };
    if !display.is_null() && unsafe { intel_scanout_needs_vtd_wa(display) } {
        64 * 1024
    } else {
        4096
    }
}

unsafe extern "C" fn i9xx_check_cursor_callback(
    crtc_state: *mut IntelCrtcState,
    plane_state: *mut IntelPlaneState,
) -> i32 {
    unsafe { i9xx_check_cursor(crtc_state, plane_state) }
}

unsafe extern "C" fn cursor_capture_error(
    crtc: *mut IntelCrtc,
    plane: *mut IntelPlane,
    error: *mut IntelPlaneError,
) {
    if crtc.is_null() || plane.is_null() || error.is_null() {
        return;
    }
    let display = unsafe {
        let dev = (*plane).base.dev;
        if dev.is_null() || (*dev).dev_private.is_null() {
            core::ptr::null_mut()
        } else {
            let i915 = (*dev)
                .dev_private
                .cast::<crate::linux_i915_private::DrmI915Private>();
            (*i915).display.cast::<IntelDisplay>()
        }
    };
    if display.is_null() {
        return;
    }
    let pipe = unsafe { (*crtc).pipe as u32 };
    unsafe {
        (*error).ctl = intel_de_read(display, CURCNTR(display, pipe));
        (*error).surf = intel_de_read(display, CURBASE(display, pipe));
        (*error).surflive = intel_de_read(display, CURSURFLIVE(display, pipe));
    }
}

unsafe extern "C" fn cursor_format_mod_supported(
    drm_plane: *mut DrmPlane,
    format: u32,
    modifier: u64,
) -> bool {
    if !cursor_format_supported(format) {
        return false;
    }
    let plane = unsafe { intel_plane_from_drm(drm_plane) };
    !plane.is_null() && unsafe { intel_fb_plane_supports_modifier(plane, modifier) }
}

#[inline]
unsafe fn intel_crtc_from_drm(crtc: *mut DrmCrtc) -> *mut IntelCrtc {
    if crtc.is_null() {
        return core::ptr::null_mut();
    }
    unsafe {
        (crtc as *mut u8)
            .sub(offset_of!(IntelCrtc, base))
            .cast::<IntelCrtc>()
    }
}

#[inline]
unsafe fn intel_frontbuffer_from_fb(
    fb: *mut DrmFramebuffer,
) -> *mut crate::intel_frontbuffer_types_upstream::IntelFrontbuffer {
    if fb.is_null() {
        return core::ptr::null_mut();
    }
    let intel_fb = unsafe {
        (fb as *mut u8)
            .sub(offset_of!(IntelFramebuffer, base))
            .cast::<IntelFramebuffer>()
    };
    unsafe { (*intel_fb).frontbuffer }
}

#[allow(clippy::too_many_arguments)]
fn cursor_fastpath_reusable(
    active: bool,
    needs_modeset: bool,
    needs_fastset: bool,
    joined_pipes: bool,
    same_crtc: bool,
    old_src_w: u32,
    old_src_h: u32,
    old_crtc_w: u32,
    old_crtc_h: u32,
    src_w: u32,
    src_h: u32,
    crtc_w: u32,
    crtc_h: u32,
    old_has_fb: bool,
    new_has_fb: bool,
) -> bool {
    active
        && !needs_modeset
        && !needs_fastset
        && !joined_pipes
        && same_crtc
        && old_src_w == src_w
        && old_src_h == src_h
        && old_crtc_w == crtc_w
        && old_crtc_h == crtc_h
        && old_has_fb == new_has_fb
}

/// Linux `intel_legacy_cursor_update()` fastpath for ordinary DP/HDMI
/// cursor updates. eDP PSR helpers are intentionally absent from this
/// configured build; all DP/HDMI state and register sequencing stays here.
unsafe fn legacy_cursor_update_fast(
    drm_plane: *mut DrmPlane,
    drm_crtc: *mut DrmCrtc,
    fb: *mut DrmFramebuffer,
    crtc_x: i32,
    crtc_y: i32,
    crtc_w: u32,
    crtc_h: u32,
    src_x: u32,
    src_y: u32,
    src_w: u32,
    src_h: u32,
) -> Option<i32> {
    let plane = unsafe { intel_plane_from_drm(drm_plane) };
    let crtc = unsafe { intel_crtc_from_drm(drm_crtc) };
    if plane.is_null()
        || crtc.is_null()
        || unsafe { (*plane).base.state.is_null() || (*crtc).base.state.is_null() }
    {
        return None;
    }
    let old_plane_state = unsafe { (*plane).base.state.cast::<IntelPlaneState>() };
    let old_crtc_state = unsafe { (*crtc).base.state.cast::<IntelCrtcState>() };

    // A modeset, fastset, joined pipe, inactive pipe, or pending commit needs
    // the serialized atomic helper path rather than the direct register arm.
    if !unsafe { (*old_crtc_state).hw.active }
        || unsafe { intel_crtc_needs_modeset(old_crtc_state) }
        || unsafe { (*old_crtc_state).update_pipe }
        || unsafe { (*old_crtc_state).joiner_pipes != 0 }
    {
        return None;
    }
    let commit = unsafe { (*old_plane_state).uapi.commit };
    if !commit.is_null() && !unsafe { (*commit).hw_done.try_wait() } {
        return None;
    }
    let old_uapi = unsafe { &(*old_plane_state).uapi };
    let reusable = cursor_fastpath_reusable(
        unsafe { (*old_crtc_state).hw.active },
        unsafe { intel_crtc_needs_modeset(old_crtc_state) },
        unsafe { (*old_crtc_state).update_pipe },
        unsafe { (*old_crtc_state).joiner_pipes != 0 },
        old_uapi.crtc == unsafe { core::ptr::addr_of_mut!((*crtc).base) },
        old_uapi.src_w,
        old_uapi.src_h,
        old_uapi.crtc_w,
        old_uapi.crtc_h,
        src_w,
        src_h,
        crtc_w,
        crtc_h,
        !old_uapi.fb.is_null(),
        !fb.is_null(),
    );
    if !reusable {
        return None;
    }

    let new_plane_state = unsafe { intel_plane_duplicate_state(drm_plane) };
    if new_plane_state.is_null() {
        return Some(-tk_linux_kapi::config::ENOMEM);
    }
    let new_plane_state = new_plane_state.cast::<IntelPlaneState>();
    let new_crtc_state = unsafe { intel_crtc_duplicate_state(drm_crtc) };
    if new_crtc_state.is_null() {
        unsafe { intel_plane_destroy_state(drm_plane, &mut (*new_plane_state).uapi) };
        return Some(-tk_linux_kapi::config::ENOMEM);
    }
    let new_crtc_state = new_crtc_state.cast::<IntelCrtcState>();

    unsafe {
        tk_drm_upstream::drm_atomic_uapi_upstream::drm_atomic_set_fb_for_plane(
            &mut (*new_plane_state).uapi,
            fb,
        );
        (*new_plane_state).uapi.src_x = src_x;
        (*new_plane_state).uapi.src_y = src_y;
        (*new_plane_state).uapi.src_w = src_w;
        (*new_plane_state).uapi.src_h = src_h;
        (*new_plane_state).uapi.crtc_x = crtc_x;
        (*new_plane_state).uapi.crtc_y = crtc_y;
        (*new_plane_state).uapi.crtc_w = crtc_w;
        (*new_plane_state).uapi.crtc_h = crtc_h;
        intel_plane_copy_uapi_to_hw_state(new_plane_state, new_plane_state, crtc);
    }

    let mut ret = unsafe {
        intel_plane_atomic_check_with_state(
            old_crtc_state,
            new_crtc_state,
            old_plane_state,
            new_plane_state,
        )
    };
    if ret == 0 {
        if unsafe { !(*new_plane_state).hw.fb.is_null() } {
            ret = unsafe { intel_plane_pin_fb(new_plane_state, old_plane_state) };
        }
    }
    if ret != 0 {
        unsafe {
            intel_crtc_destroy_state(drm_crtc, &mut (*new_crtc_state).uapi);
            intel_plane_destroy_state(drm_plane, &mut (*new_plane_state).uapi);
        }
        return Some(ret);
    }

    // Keep frontbuffer invalidation ordering identical to the upstream fast
    // path: flush the new scanout, then transfer this cursor's busy bit.
    const ORIGIN_CURSOR_UPDATE: i32 = 4;
    let new_front = unsafe { intel_frontbuffer_from_fb((*new_plane_state).hw.fb) };
    let old_front = unsafe { intel_frontbuffer_from_fb((*old_plane_state).hw.fb) };
    unsafe {
        intel_frontbuffer_flush(new_front, ORIGIN_CURSOR_UPDATE);
        intel_frontbuffer_track(old_front, new_front, (*plane).frontbuffer_bit);
        (*plane).base.state = &mut (*new_plane_state).uapi;
        (*old_crtc_state).active_planes = (*new_crtc_state).active_planes;
    }

    // PSR locking/waiting is eDP-only in the configured display manifest.
    // DP/HDMI still uses the exact vblank-evasion and IRQ-protected arm path.
    let mut evade =
        core::mem::MaybeUninit::<crate::intel_vblank_types_upstream::IntelVblankEvadeCtx>::uninit();
    unsafe { intel_vblank_evade_init(old_crtc_state, old_crtc_state, evade.as_mut_ptr()) };
    let vblank_ret =
        unsafe { tk_drm_upstream::drm_vblank_upstream::drm_crtc_vblank_get(&mut (*crtc).base) };
    tk_linux_kapi::locks::local_irq_disable();
    if vblank_ret == 0 {
        unsafe {
            intel_vblank_evade(evade.as_mut_ptr());
            tk_drm_upstream::drm_vblank_upstream::drm_crtc_vblank_put(&mut (*crtc).base);
        }
    }
    if unsafe { (*new_plane_state).uapi.visible } {
        unsafe {
            intel_plane_update_noarm(
                core::ptr::null_mut(),
                plane,
                old_crtc_state,
                new_plane_state,
            );
            intel_plane_update_arm(
                core::ptr::null_mut(),
                plane,
                old_crtc_state,
                new_plane_state,
            );
        }
    } else {
        unsafe { intel_plane_disable_arm(core::ptr::null_mut(), plane, old_crtc_state) };
    }
    tk_linux_kapi::locks::local_irq_enable();

    let mut old_to_destroy = old_plane_state;
    if unsafe { (*old_plane_state).ggtt_vma != (*new_plane_state).ggtt_vma } {
        unsafe {
            tk_drm_upstream::drm_vblank_work_upstream::drm_vblank_work_init(
                &mut (*old_plane_state).unpin_work,
                &mut (*crtc).base,
                intel_cursor_unpin_work,
            );
            let count = tk_drm_upstream::drm_vblank_upstream::drm_crtc_accurate_vblank_count(
                &mut (*crtc).base,
            );
            let _ = tk_drm_upstream::drm_vblank_work_upstream::drm_vblank_work_schedule(
                &mut (*old_plane_state).unpin_work,
                count + 1,
                false,
            );
        }
        old_to_destroy = core::ptr::null_mut();
    } else {
        unsafe { intel_plane_unpin_fb(old_plane_state) };
    }

    unsafe {
        intel_crtc_destroy_state(drm_crtc, &mut (*new_crtc_state).uapi);
        if !old_to_destroy.is_null() {
            intel_plane_destroy_state(drm_plane, &mut (*old_to_destroy).uapi);
        }
    }
    Some(0)
}

/// Legacy DRM cursor update callback. Preserve Linux's direct vblank/async
/// update for same-mode moves; everything else uses the atomic helper.
unsafe extern "C" fn legacy_cursor_update(
    plane: *mut DrmPlane,
    crtc: *mut DrmCrtc,
    fb: *mut DrmFramebuffer,
    crtc_x: i32,
    crtc_y: i32,
    crtc_w: u32,
    crtc_h: u32,
    src_x: u32,
    src_y: u32,
    src_w: u32,
    src_h: u32,
    ctx: *mut DrmModesetAcquireCtx,
) -> i32 {
    if let Some(ret) = unsafe {
        legacy_cursor_update_fast(
            plane, crtc, fb, crtc_x, crtc_y, crtc_w, crtc_h, src_x, src_y, src_w, src_h,
        )
    } {
        return ret;
    }
    unsafe {
        drm_atomic_helper_update_plane(
            plane, crtc, fb, crtc_x, crtc_y, crtc_w, crtc_h, src_x, src_y, src_w, src_h, ctx,
        )
    }
}

static CURSOR_PLANE_FUNCS: DrmPlaneFuncs = DrmPlaneFuncs {
    update_plane: Some(legacy_cursor_update),
    disable_plane: Some(drm_atomic_helper_disable_plane),
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
    format_mod_supported: Some(cursor_format_mod_supported),
    format_mod_supported_async: Some(intel_plane_format_mod_supported_async),
};

/// Source-equivalent ADL-P defaults (256x256 cursor plane).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_cursor_mode_config_init(display: *mut IntelDisplay) {
    if display.is_null() || unsafe { (*display).drm.is_null() } {
        return;
    }
    let config = unsafe { &mut (*(*display).drm).mode_config };
    if unsafe { platform_bit(display, 4) } {
        config.cursor_width = 64;
        config.cursor_height = 1023;
    } else if unsafe { platform_bit(display, 6) } {
        config.cursor_width = 512;
        config.cursor_height = 1023;
    } else if unsafe {
        platform_bit(display, 3)
            || platform_bit(display, 5)
            || platform_bit(display, 7)
            || platform_bit(display, 8)
    } {
        config.cursor_width = 64;
        config.cursor_height = 64;
    } else {
        config.cursor_width = 256;
        config.cursor_height = 256;
    }
}

unsafe fn cursor_add_size_hints(plane: *mut IntelPlane) {
    if plane.is_null() || unsafe { (*plane).base.dev.is_null() } {
        return;
    }
    let config = unsafe { &(*(*plane).base.dev).mode_config };
    let mut hints: [DrmPlaneSizeHint; 4] = core::array::from_fn(|_| DrmPlaneSizeHint {
        width: 0,
        height: 0,
    });
    let max_size = config.cursor_width.min(config.cursor_height);
    let mut size = 64u32;
    let mut count = 0usize;
    while size <= max_size && count < hints.len() {
        hints[count] = DrmPlaneSizeHint {
            width: size as u16,
            height: size as u16,
        };
        count += 1;
        size = size.saturating_mul(2);
    }
    unsafe {
        drm_plane_add_size_hints_property(&mut (*plane).base, hints.as_ptr(), count as i32);
    }
}

/// Linux `intel_cursor_plane_create()` for the configured ADL-P display.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_cursor_plane_create(
    display: *mut IntelDisplay,
    pipe: Pipe,
) -> *mut IntelPlane {
    if display.is_null() || unsafe { (*display).drm.is_null() } || !(0..4).contains(&pipe) {
        return tk_linux_kapi::config::ERR_PTR(-EINVAL);
    }
    let cursor = unsafe { intel_plane_alloc() };
    if tk_linux_kapi::config::IS_ERR(cursor) {
        return cursor;
    }
    if cursor.is_null() {
        return tk_linux_kapi::config::ERR_PTR(-tk_linux_kapi::config::ENOMEM);
    }
    unsafe {
        (*cursor).pipe = pipe;
        (*cursor).i9xx_plane = pipe as u32;
        (*cursor).id = PLANE_CURSOR;
        (*cursor).frontbuffer_bit = 1u32 << (pipe as u32 * 8 + PLANE_CURSOR);
        (*cursor).max_stride = Some(i9xx_cursor_max_stride);
        (*cursor).min_alignment = Some(i9xx_cursor_min_alignment);
        (*cursor).update_arm = Some(i9xx_cursor_update_arm_callback);
        (*cursor).disable_arm = Some(i9xx_cursor_disable_arm_callback);
        (*cursor).get_hw_state = Some(i9xx_cursor_get_hw_state);
        (*cursor).check_plane = Some(i9xx_check_cursor_callback);
        (*cursor).surf_offset = Some(i9xx_cursor_surf_offset);
        (*cursor).capture_error = Some(cursor_capture_error);
        if intel_scanout_needs_vtd_wa(display) {
            (*cursor).vtd_guard = 2;
        }
        (*cursor).cursor.base = u32::MAX;
        (*cursor).cursor.cntl = u32::MAX;
        if has_cursor_fbc(display) {
            (*cursor).cursor.size = u32::MAX;
        }
    }

    let modifiers = unsafe { intel_fb_plane_get_modifiers(display, INTEL_PLANE_CAP_NONE) };
    let name = match pipe {
        0 => c"cursor A".as_ptr(),
        1 => c"cursor B".as_ptr(),
        2 => c"cursor C".as_ptr(),
        _ => c"cursor D".as_ptr(),
    };
    let formats = [DRM_FORMAT_ARGB8888];
    let ret = unsafe {
        drm_universal_plane_init(
            (*display).drm,
            &mut (*cursor).base,
            0,
            &CURSOR_PLANE_FUNCS,
            formats.as_ptr(),
            formats.len() as u32,
            modifiers,
            DRM_PLANE_TYPE_CURSOR,
            name,
        )
    };
    if !modifiers.is_null() {
        unsafe { tk_linux_kapi::memory::kfree(modifiers.cast::<c_void>()) };
    }
    if ret != 0 {
        unsafe { intel_plane_free(cursor) };
        return tk_linux_kapi::config::ERR_PTR(ret);
    }

    unsafe {
        if DISPLAY_VER(display) >= 4 {
            let _ = drm_plane_create_rotation_property(
                &mut (*cursor).base,
                DRM_MODE_ROTATE_0,
                DRM_MODE_ROTATE_0 | DRM_MODE_ROTATE_180,
            );
        }
        cursor_add_size_hints(cursor);
        let runtime = &(*display).info.__runtime_info;
        let zpos = u32::from(runtime.num_sprites[pipe as usize]) + 1;
        let _ = drm_plane_create_zpos_immutable_property(&mut (*cursor).base, zpos);
        if DISPLAY_VER(display) >= 12 {
            drm_plane_enable_fb_damage_clips(&mut (*cursor).base);
        }
        intel_plane_helper_add(cursor);
    }
    cursor
}

/// Retire a prior cursor framebuffer only after its scheduled vblank work.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_cursor_unpin_work(base: *mut KthreadWork) {
    if base.is_null() {
        return;
    }
    // DrmVblankWork.base is at offset zero; IntelPlaneState::unpin_work is
    // embedded at its Linux-layout offset in the containing plane state.
    let work = base.cast::<DrmVblankWork>();
    let plane_state = unsafe {
        (work as *mut u8)
            .sub(offset_of!(IntelPlaneState, unpin_work))
            .cast::<IntelPlaneState>()
    };
    let drm_plane = unsafe { (*plane_state).uapi.plane };
    let plane = unsafe { intel_plane_from_drm(drm_plane) };
    if plane.is_null() {
        return;
    }
    unsafe {
        intel_plane_unpin_fb(plane_state);
        intel_plane_destroy_state(drm_plane, &mut (*plane_state).uapi);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_size_format_and_position_boundaries() {
        assert!(cursor_size_valid(64, 8, 256, 256, true, DRM_MODE_ROTATE_0));
        assert!(cursor_size_valid(
            256,
            256,
            256,
            256,
            true,
            DRM_MODE_ROTATE_0
        ));
        assert!(!cursor_size_valid(64, 7, 256, 256, true, DRM_MODE_ROTATE_0));
        assert!(!cursor_size_valid(
            96,
            96,
            256,
            256,
            true,
            DRM_MODE_ROTATE_0
        ));
        assert!(!cursor_size_valid(
            257,
            256,
            256,
            256,
            true,
            DRM_MODE_ROTATE_0
        ));
        assert_eq!(
            cursor_position_value(-1, -1, 64, false, 0),
            Some(CURSOR_POS_X_SIGN | CURSOR_POS_Y_SIGN | CURSOR_POS_X(1) | CURSOR_POS_Y(1))
        );
        assert_eq!(
            cursor_position_value(0x7fff, 0x7fff, 64, false, 0),
            Some(CURSOR_POS_X(0x7fff) | CURSOR_POS_Y(0x7fff))
        );
        assert_eq!(cursor_position_value(0x8000, 0, 64, false, 0), None);
        assert_eq!(
            cursor_position_value(0, -2, 16, true, 4),
            Some(CURSOR_POS_Y_SIGN | CURSOR_POS_Y(6))
        );
        assert!(cursor_format_supported(DRM_FORMAT_ARGB8888));
        assert!(!cursor_format_supported(
            tk_drm_upstream::uapi_drm_consts::DRM_FORMAT_XRGB8888
        ));
    }

    #[test]
    fn cursor_ctl_keeps_supported_power_of_two_modes_and_rotation() {
        assert_eq!(cursor_ctl(64, 0, false), MCURSOR_MODE_64_ARGB_AX);
        assert_eq!(
            cursor_ctl(128, DRM_MODE_ROTATE_180, true),
            MCURSOR_MODE_128_ARGB_AX | MCURSOR_ROTATE_180 | MCURSOR_ARB_SLOTS(1)
        );
        assert_eq!(cursor_ctl(96, 0, false), 0);
    }

    #[test]
    fn cursor_fastpath_only_accepts_same_crtc_and_geometry() {
        let eligible = |same_crtc, src_w, crtc_w, active, modeset, fastset, joined| {
            cursor_fastpath_reusable(
                active, modeset, fastset, joined, same_crtc, 64, 64, 64, 64, src_w, 64, crtc_w, 64,
                true, true,
            )
        };
        assert!(eligible(true, 64, 64, true, false, false, false));
        assert!(!eligible(false, 64, 64, true, false, false, false));
        assert!(!eligible(true, 63, 64, true, false, false, false));
        assert!(!eligible(true, 64, 63, true, false, false, false));
        assert!(!eligible(true, 64, 64, false, false, false, false));
        assert!(!eligible(true, 64, 64, true, true, false, false));
        assert!(!eligible(true, 64, 64, true, false, true, false));
        assert!(!eligible(true, 64, 64, true, false, false, true));
        assert!(!cursor_fastpath_reusable(
            true, false, false, false, true, 64, 64, 64, 64, 64, 64, 64, 64, false, true
        ));
    }
}
