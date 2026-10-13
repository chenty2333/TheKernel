// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// Source: drivers/gpu/drm/i915/display/intel_plane.c (Linux v7.2.3)
//
// Plane state ownership, checked geometry and commit ordering shared by the
// universal-plane implementation. Display-13-specific register programming
// lives in skl_universal_plane_upstream.rs.
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::{ffi::c_void, mem::size_of, ptr};

use tk_drm_upstream::{
    drm_crtc_types::{DrmCrtc, DrmCrtcState},
    drm_framebuffer_types::DrmFramebuffer,
    drm_gem_types::DrmGemObject,
    drm_plane_types::{DrmPlane, DrmPlaneState},
    drm_rect_types::DrmRect,
    uapi_drm_consts::{
        DRM_MODE_REFLECT_MASK, DRM_MODE_REFLECT_X, DRM_MODE_ROTATE_0, DRM_MODE_ROTATE_90,
        DRM_MODE_ROTATE_180, DRM_MODE_ROTATE_270, DRM_MODE_ROTATE_MASK,
    },
};

use crate::{
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_limits_types_upstream::{I915_MAX_PLANES, PLANE_CURSOR, Pipe},
    intel_display_types_upstream::{
        IntelAtomicState, IntelCrtc, IntelCrtcState, IntelFramebuffer, IntelPlane, IntelPlaneState,
    },
    intel_display_wa_upstream::DISPLAY_VER,
    intel_wm_types_upstream::SklDdbEntry,
};

#[repr(C)]
struct IntelFbPinParams {
    view: *const crate::i915_gtt_view_types_upstream::I915GttView,
    alignment: u32,
    phys_alignment: u32,
    vtd_guard: u32,
    needs_cpu_lmem_access: bool,
    needs_low_address: bool,
    needs_physical: bool,
    needs_fence: bool,
}

const DRM_PLANE_NO_SCALING: i32 = 1 << 16;
const DRM_FORMAT_C8: u32 = tk_drm_upstream::uapi_drm_consts::DRM_FORMAT_C8;
const DRM_FORMAT_RGB565: u32 = tk_drm_upstream::uapi_drm_consts::DRM_FORMAT_RGB565;
const DRM_FORMAT_NV12: u32 = tk_drm_upstream::uapi_drm_consts::DRM_FORMAT_NV12;
const DRM_FORMAT_P010: u32 = tk_drm_upstream::uapi_drm_consts::DRM_FORMAT_P010;
const DRM_FORMAT_P012: u32 = tk_drm_upstream::uapi_drm_consts::DRM_FORMAT_P012;
const DRM_FORMAT_P016: u32 = tk_drm_upstream::uapi_drm_consts::DRM_FORMAT_P016;
const DRM_FORMAT_MOD_INTEL_BASE: u64 = 1u64 << 56;
const I915_FORMAT_MOD_Y_TILED_CCS: u64 = DRM_FORMAT_MOD_INTEL_BASE | 4;
const I915_FORMAT_MOD_Yf_TILED_CCS: u64 = DRM_FORMAT_MOD_INTEL_BASE | 5;

unsafe extern "C" {
    // Linux drm_framebuffer_get()/put() are inline wrappers over this KAPI
    // kref and the driver's release callback. B4d owns framebuffer lifetime.
    fn drm_framebuffer_free(refcount: *mut tk_linux_kapi::types::Kref);
    fn intel_crtc_for_pipe(display: *mut IntelDisplay, pipe: Pipe) -> *mut IntelCrtc;
    fn intel_fbc_dirty_rect_update_noarm(dsb: *mut c_void, plane: *mut IntelPlane);
    fn intel_plane_uses_fence(state: *const IntelPlaneState) -> bool;
    fn intel_fb_uses_dpt(fb: *const DrmFramebuffer) -> bool;
    fn intel_fb_view_vtd_guard(
        fb: *const DrmFramebuffer,
        view: *const crate::intel_display_types_upstream::IntelFbView,
        rotation: u32,
    ) -> u32;
    fn intel_parent_fb_pin_reuse_vma(
        display: *mut IntelDisplay,
        old_vma: *mut c_void,
        old_obj: *mut c_void,
        old_view: *const crate::i915_gtt_view_types_upstream::I915GttView,
        new_obj: *mut c_void,
        new_view: *const crate::i915_gtt_view_types_upstream::I915GttView,
        offset: *mut u32,
    ) -> *mut c_void;
    fn intel_parent_fb_pin_ggtt_pin(
        display: *mut IntelDisplay,
        obj: *mut c_void,
        params: *const c_void,
        vma: *mut *mut c_void,
        offset: *mut u32,
        fence_id: *mut i32,
    ) -> i32;
    fn intel_parent_fb_pin_dpt_pin(
        display: *mut IntelDisplay,
        obj: *mut c_void,
        dpt: *mut c_void,
        params: *const c_void,
        dpt_vma: *mut *mut c_void,
        ggtt_vma: *mut *mut c_void,
        offset: *mut u32,
    ) -> i32;
    fn intel_parent_fb_pin_ggtt_unpin(display: *mut IntelDisplay, vma: *mut c_void, fence_id: i32);
    fn intel_parent_fb_pin_dpt_unpin(
        display: *mut IntelDisplay,
        dpt: *mut c_void,
        dpt_vma: *mut c_void,
        ggtt_vma: *mut c_void,
    );
    fn intel_fb_bo(fb: *mut DrmFramebuffer) -> *mut c_void;
    fn intel_fb_needs_cpu_access(fb: *const DrmFramebuffer) -> bool;
    fn intel_fb_plane_get_modifiers(display: *mut IntelDisplay, caps: u8) -> *mut u64;
    fn intel_crtc_needs_modeset(state: *const IntelCrtcState) -> bool;
    fn skl_plane_relative_data_rate(
        crtc: *const IntelCrtcState,
        plane: *mut IntelPlane,
        width: i32,
        height: i32,
        cpp: i32,
    ) -> u32;
    fn intel_cursor_unpin_work(work: *mut tk_drm_upstream::kernel_placeholder_types::KthreadWork);
    fn skl_ddb_allocation_overlaps(
        ddb: *const SklDdbEntry,
        entries: *const SklDdbEntry,
        num_entries: i32,
        ignore_idx: i32,
    ) -> bool;
    fn skl_update_scaler_plane(
        crtc_state: *mut IntelCrtcState,
        plane_state: *mut IntelPlaneState,
    ) -> i32;
}

#[inline]
unsafe fn display_from_dev(
    dev: *mut tk_drm_upstream::drm_device_types::DrmDevice,
) -> *mut IntelDisplay {
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

#[inline]
unsafe fn plane_from_drm(plane: *mut DrmPlane) -> *mut IntelPlane {
    container_of!(plane, IntelPlane, base)
}

#[inline]
unsafe fn plane_state_from_drm(state: *mut DrmPlaneState) -> *mut IntelPlaneState {
    container_of!(state, IntelPlaneState, uapi)
}

#[inline]
unsafe fn crtc_from_drm(crtc: *mut DrmCrtc) -> *mut IntelCrtc {
    container_of!(crtc, IntelCrtc, base)
}

#[inline]
unsafe fn crtc_state_from_drm(state: *mut DrmCrtcState) -> *mut IntelCrtcState {
    container_of!(state, IntelCrtcState, uapi)
}

#[inline]
fn rect_width(rect: &DrmRect) -> i32 {
    tk_drm_upstream::drm_rect_upstream::drm_rect_width(rect)
}

#[inline]
fn rect_height(rect: &DrmRect) -> i32 {
    tk_drm_upstream::drm_rect_upstream::drm_rect_height(rect)
}

unsafe fn fb_get(fb: *mut DrmFramebuffer) {
    if !fb.is_null() {
        unsafe { tk_linux_kapi::primitives::kref_get(ptr::addr_of_mut!((*fb).base.refcount)) };
    }
}

unsafe fn fb_put(fb: *mut DrmFramebuffer) {
    if !fb.is_null() {
        unsafe {
            tk_linux_kapi::primitives::kref_put(
                ptr::addr_of_mut!((*fb).base.refcount),
                Some(drm_framebuffer_free),
            );
        }
    }
}

unsafe fn plane_fb_min_alignment(state: *const IntelPlaneState) -> u32 {
    if state.is_null() || unsafe { (*state).hw.fb.is_null() } {
        return 0;
    }
    unsafe { (*(*state).hw.fb.cast::<IntelFramebuffer>()).min_alignment }
}

unsafe fn plane_fb_min_phys_alignment(state: *const IntelPlaneState) -> u32 {
    if state.is_null() || unsafe { (*state).uapi.plane.is_null() || (*state).hw.fb.is_null() } {
        return 0;
    }
    let plane = unsafe { plane_from_drm((*state).uapi.plane) };
    if !unsafe { intel_plane_needs_physical(plane) } {
        return 0;
    }
    unsafe {
        (*plane)
            .min_alignment
            .map_or(0, |f| f(plane, (*state).hw.fb, 0))
    }
}

unsafe fn plane_fb_vtd_guard(state: *const IntelPlaneState) -> u32 {
    if state.is_null() {
        return 0;
    }
    unsafe { intel_fb_view_vtd_guard((*state).hw.fb, &(*state).view, (*state).hw.rotation) }
}

unsafe fn clear_hw_state(state: *mut IntelPlaneState) {
    unsafe {
        fb_put((*state).hw.fb);
        for blob in [
            (*state).hw.degamma_lut,
            (*state).hw.gamma_lut,
            (*state).hw.ctm,
            (*state).hw.lut_3d,
        ] {
            if !blob.is_null() {
                tk_drm_upstream::drm_property_upstream::drm_property_blob_put(blob);
            }
        }
        ptr::write_bytes(ptr::addr_of_mut!((*state).hw), 0, 1);
    }
}

unsafe fn state_reset(state: *mut IntelPlaneState, plane: *mut IntelPlane) {
    unsafe {
        ptr::write_bytes(state, 0, 1);
        tk_drm_upstream::drm_atomic_state_helper_upstream::__drm_atomic_helper_plane_state_reset(
            ptr::addr_of_mut!((*state).uapi),
            ptr::addr_of_mut!((*plane).base),
        );
        (*state).scaler_id = -1;
        (*state).fence_id = -1;
    }
}

// upstream: intel_plane.c intel_plane_alloc()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_plane_alloc() -> *mut IntelPlane {
    let plane = unsafe {
        tk_linux_kapi::memory::kzalloc(size_of::<IntelPlane>(), 0x0cc0).cast::<IntelPlane>()
    };
    if plane.is_null() {
        return tk_linux_kapi::config::ERR_PTR(-tk_linux_kapi::config::ENOMEM);
    }
    let state = unsafe {
        tk_linux_kapi::memory::kzalloc(size_of::<IntelPlaneState>(), 0x0cc0)
            .cast::<IntelPlaneState>()
    };
    if state.is_null() {
        unsafe { tk_linux_kapi::memory::kfree(plane.cast::<c_void>()) };
        return tk_linux_kapi::config::ERR_PTR(-tk_linux_kapi::config::ENOMEM);
    }
    unsafe {
        state_reset(state, plane);
        (*plane).base.state = ptr::addr_of_mut!((*state).uapi);
    }
    plane
}

// upstream: intel_plane.c intel_plane_duplicate_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_plane_duplicate_state(plane: *mut DrmPlane) -> *mut DrmPlaneState {
    if plane.is_null() || unsafe { (*plane).state.is_null() } {
        return ptr::null_mut();
    }
    let old = unsafe { plane_state_from_drm((*plane).state) };
    let new = unsafe {
        tk_linux_kapi::memory::kmalloc(size_of::<IntelPlaneState>(), 0x0cc0)
            .cast::<IntelPlaneState>()
    };
    if new.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        ptr::copy_nonoverlapping(old, new, 1);
        tk_drm_upstream::drm_atomic_state_helper_upstream::__drm_atomic_helper_plane_duplicate_state(
            plane,
            ptr::addr_of_mut!((*new).uapi),
        );
        (*new).ggtt_vma = ptr::null_mut();
        (*new).dpt_vma = ptr::null_mut();
        (*new).fence_id = -1;
        (*new).damage = DrmRect {
            x1: 0,
            y1: 0,
            x2: 0,
            y2: 0,
        };
    }
    unsafe { ptr::addr_of_mut!((*new).uapi) }
}

// upstream: intel_plane.c intel_plane_destroy_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_plane_destroy_state(
    _plane: *mut DrmPlane,
    state: *mut DrmPlaneState,
) {
    if state.is_null() {
        return;
    }
    let intel_state = unsafe { plane_state_from_drm(state) };
    unsafe {
        clear_hw_state(intel_state);
        tk_drm_upstream::drm_atomic_state_helper_upstream::__drm_atomic_helper_plane_destroy_state(
            state,
        );
        tk_linux_kapi::memory::kfree(intel_state.cast::<c_void>());
    }
}

// upstream: intel_plane.c intel_plane_free()/intel_plane_destroy()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_plane_free(plane: *mut IntelPlane) {
    if plane.is_null() || tk_linux_kapi::config::IS_ERR(plane.cast::<c_void>()) {
        return;
    }
    unsafe {
        if !(*plane).base.state.is_null() {
            intel_plane_destroy_state(ptr::addr_of_mut!((*plane).base), (*plane).base.state);
        }
        tk_linux_kapi::memory::kfree(plane.cast::<c_void>());
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_plane_destroy(plane: *mut DrmPlane) {
    if plane.is_null() {
        return;
    }
    tk_drm_upstream::drm_plane_upstream::drm_plane_cleanup(plane);
    unsafe { tk_linux_kapi::memory::kfree(plane_from_drm(plane).cast::<c_void>()) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_plane_needs_low_address(display: *mut IntelDisplay) -> bool {
    if display.is_null() {
        return false;
    }
    let info = unsafe { (*display).info.__device_info };
    !info.is_null() && unsafe { (*info)._bitfields_46[0] & (1 << 7) != 0 }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_plane_needs_physical(plane: *mut IntelPlane) -> bool {
    if plane.is_null() || unsafe { (*plane).id != PLANE_CURSOR } {
        return false;
    }
    let display = unsafe { display_from_dev((*plane).base.dev) };
    if display.is_null() {
        return false;
    }
    let info = unsafe { (*display).info.__device_info };
    !info.is_null() && unsafe { (*info)._bitfields_46[0] & 1 != 0 }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_plane_needs_fence(display: *mut IntelDisplay) -> bool {
    !display.is_null() && unsafe { DISPLAY_VER(display) < 4 }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_plane_can_async_flip(
    plane: *mut IntelPlane,
    info: *const tk_drm_upstream::drm_fourcc_types::DrmFormatInfo,
    modifier: u64,
) -> bool {
    if plane.is_null() || info.is_null() {
        return false;
    }
    let semiplanar = unsafe {
        (*info).format == DRM_FORMAT_NV12
            || (*info).format == DRM_FORMAT_P010
            || (*info).format == DRM_FORMAT_P012
            || (*info).format == DRM_FORMAT_P016
    };
    if semiplanar || unsafe { (*info).format == DRM_FORMAT_C8 } {
        return false;
    }
    unsafe { (*plane).can_async_flip.is_some_and(|can| can(modifier)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_plane_format_mod_supported_async(
    plane: *mut DrmPlane,
    format: u32,
    modifier: u64,
) -> bool {
    if plane.is_null() || unsafe { (*plane).funcs.is_null() } {
        return false;
    }
    let funcs = unsafe { &*(*plane).funcs };
    if !funcs
        .format_mod_supported
        .is_some_and(|check| unsafe { check(plane, format, modifier) })
    {
        return false;
    }
    let info = unsafe {
        tk_drm_upstream::drm_fourcc_upstream::drm_get_format_info((*plane).dev, format, modifier)
    };
    if info.is_null() {
        return false;
    }
    unsafe { intel_plane_can_async_flip(plane_from_drm(plane), info, modifier) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_adjusted_rate(
    src: *const DrmRect,
    dst: *const DrmRect,
    rate: u32,
) -> u32 {
    if src.is_null() || dst.is_null() {
        return 0;
    }
    let (sw, sh, dw, dh) = unsafe {
        (
            rect_width(&*src) >> 16,
            rect_height(&*src) >> 16,
            rect_width(&*dst),
            rect_height(&*dst),
        )
    };
    let dw = core::cmp::min(sw, dw);
    let dh = core::cmp::min(sh, dh);
    if dw <= 0 || dh <= 0 {
        return 0;
    }
    let numerator = (rate as u64)
        .saturating_mul(sw.max(0) as u64)
        .saturating_mul(sh.max(0) as u64);
    ((numerator + (dw as u64 * dh as u64) - 1) / (dw as u64 * dh as u64)).min(u32::MAX as u64)
        as u32
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_plane_pixel_rate(
    crtc_state: *const IntelCrtcState,
    plane_state: *const IntelPlaneState,
) -> u32 {
    if crtc_state.is_null() || plane_state.is_null() {
        return 0;
    }
    unsafe {
        intel_adjusted_rate(
            ptr::addr_of!((*plane_state).uapi.src),
            ptr::addr_of!((*plane_state).uapi.dst),
            (*crtc_state).pixel_rate,
        )
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_plane_data_rate(
    crtc_state: *const IntelCrtcState,
    plane_state: *const IntelPlaneState,
    color_plane: i32,
) -> u32 {
    if crtc_state.is_null()
        || plane_state.is_null()
        || unsafe { !(*plane_state).uapi.visible || (*plane_state).hw.fb.is_null() }
    {
        return 0;
    }
    let fb = unsafe { (*plane_state).hw.fb };
    let fmt = unsafe { (*fb).format };
    if fmt.is_null() || !(0..4).contains(&color_plane) {
        return 0;
    }
    let cpp = unsafe { (*fmt)._anon_6.cpp[color_plane as usize] as u32 };
    unsafe { intel_plane_pixel_rate(crtc_state, plane_state).saturating_mul(cpp) }
}

// upstream: intel_plane.c intel_plane_pin_fb()
#[inline]
fn checked_plane_surface_address(base: u32, surface_offset: u32) -> Option<u32> {
    base.checked_add(surface_offset)
}

// A successful GGTT/DPT pin or reuse result transfers one pin reference to the
// candidate plane state. On failure before that state is published, release
// exactly that reference with the matching parent unpin operation. The parent
// provider remains responsible for proving that its VMA covers the selected
// framebuffer view and for returning its surface-register base in `offset`.
unsafe fn unpin_candidate_fb(
    display: *mut IntelDisplay,
    fb: *mut IntelFramebuffer,
    dpt_vma: *mut c_void,
    ggtt_vma: *mut c_void,
    fence_id: i32,
) {
    if fb.is_null() {
        return;
    }
    unsafe {
        if intel_fb_uses_dpt(ptr::addr_of!((*fb).base)) {
            if !dpt_vma.is_null() || !ggtt_vma.is_null() {
                intel_parent_fb_pin_dpt_unpin(display, (*fb).dpt, dpt_vma, ggtt_vma);
            }
        } else if !ggtt_vma.is_null() {
            intel_parent_fb_pin_ggtt_unpin(display, ggtt_vma, fence_id);
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_plane_pin_fb(
    plane_state: *mut IntelPlaneState,
    old_plane_state: *const IntelPlaneState,
) -> i32 {
    if plane_state.is_null()
        || old_plane_state.is_null()
        || unsafe { (*plane_state).hw.fb.is_null() }
        || unsafe { (*plane_state).uapi.plane.is_null() }
    {
        return -tk_linux_kapi::config::EINVAL;
    }
    let display = unsafe { display_from_dev((*(*plane_state).uapi.plane).dev) };
    if display.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let plane = unsafe { plane_from_drm((*plane_state).uapi.plane) };
    let fb = unsafe { (*plane_state).hw.fb.cast::<IntelFramebuffer>() };
    let old_fb = unsafe { (*old_plane_state).hw.fb.cast::<IntelFramebuffer>() };
    let old_vma = unsafe { (*old_plane_state).ggtt_vma };
    let old_obj = if unsafe { (*old_plane_state).hw.fb.is_null() } {
        ptr::null_mut()
    } else {
        unsafe { intel_fb_bo((*old_plane_state).hw.fb) }
    };
    let obj = unsafe { intel_fb_bo(ptr::addr_of_mut!((*fb).base)) };
    if obj.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let mut offset = 0u32;
    let old_view = if old_fb.is_null() {
        ptr::null()
    } else {
        unsafe { ptr::addr_of!((*old_fb).normal_view.gtt) }
    };
    let vma = unsafe {
        intel_parent_fb_pin_reuse_vma(
            display,
            old_vma,
            old_obj,
            old_view,
            obj,
            ptr::addr_of!((*fb).normal_view.gtt),
            &mut offset,
        )
    };
    let mut ggtt_vma = vma;
    let mut dpt_vma = ptr::null_mut();
    let mut fence_id = -1i32;
    if ggtt_vma.is_null() {
        let alignment = unsafe { plane_fb_min_alignment(plane_state) };
        let vtd_guard = unsafe { plane_fb_vtd_guard(plane_state) };
        let needs_cpu_lmem_access = unsafe { intel_fb_needs_cpu_access(ptr::addr_of!((*fb).base)) };
        if !unsafe { intel_fb_uses_dpt(ptr::addr_of!((*fb).base)) } {
            let pin = IntelFbPinParams {
                view: ptr::addr_of!((*plane_state).view.gtt),
                alignment,
                phys_alignment: unsafe { plane_fb_min_phys_alignment(plane_state) },
                vtd_guard,
                needs_cpu_lmem_access,
                needs_low_address: unsafe { intel_plane_needs_low_address(display) },
                needs_physical: unsafe { intel_plane_needs_physical(plane) },
                needs_fence: unsafe { intel_plane_needs_fence(display) },
            };
            let fence_ptr = if unsafe { intel_plane_uses_fence(plane_state) } {
                &mut fence_id as *mut i32
            } else {
                ptr::null_mut()
            };
            let ret = unsafe {
                intel_parent_fb_pin_ggtt_pin(
                    display,
                    obj,
                    &pin as *const _ as *const c_void,
                    &mut ggtt_vma,
                    &mut offset,
                    fence_ptr,
                )
            };
            if ret != 0 {
                return ret;
            }
        } else {
            let pin = IntelFbPinParams {
                view: ptr::addr_of!((*plane_state).view.gtt),
                alignment,
                phys_alignment: 0,
                vtd_guard: 0,
                needs_cpu_lmem_access,
                needs_low_address: false,
                needs_physical: false,
                needs_fence: false,
            };
            let ret = unsafe {
                intel_parent_fb_pin_dpt_pin(
                    display,
                    obj,
                    (*fb).dpt,
                    &pin as *const _ as *const c_void,
                    &mut dpt_vma,
                    &mut ggtt_vma,
                    &mut offset,
                )
            };
            if ret != 0 {
                return ret;
            }
        }
    }
    if ggtt_vma.is_null() {
        unsafe {
            unpin_candidate_fb(display, fb, dpt_vma, ggtt_vma, fence_id);
        }
        return -tk_linux_kapi::config::ENOMEM;
    }
    // The parent pin provider owns validating the selected GGTT/DPT view's
    // backing extent. Reject only arithmetic which cannot be represented in
    // the u32 plane-surface register field; DPT offsets are not object bytes.
    let surf_offset = unsafe { (*plane).surf_offset.map_or(0, |f| f(plane_state)) };
    let Some(surf) = checked_plane_surface_address(offset, surf_offset) else {
        unsafe {
            unpin_candidate_fb(display, fb, dpt_vma, ggtt_vma, fence_id);
        }
        return -tk_linux_kapi::config::ERANGE;
    };
    unsafe {
        (*plane_state).dpt_vma = dpt_vma;
        (*plane_state).ggtt_vma = ggtt_vma;
        (*plane_state).fence_id = fence_id as i8;
        (*plane_state).surf = surf;
    }
    0
}

// upstream: intel_plane.c intel_plane_unpin_fb()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_plane_unpin_fb(state: *mut IntelPlaneState) {
    if state.is_null() || unsafe { (*state).hw.fb.is_null() || (*state).uapi.plane.is_null() } {
        return;
    }
    let display = unsafe { display_from_dev((*(*state).uapi.plane).dev) };
    let fb = unsafe { (*state).hw.fb.cast::<IntelFramebuffer>() };
    if unsafe { !intel_fb_uses_dpt(ptr::addr_of!((*fb).base)) } {
        unsafe {
            intel_parent_fb_pin_ggtt_unpin(display, (*state).ggtt_vma, (*state).fence_id as i32);
            (*state).ggtt_vma = ptr::null_mut();
            (*state).fence_id = -1;
        }
    } else {
        unsafe {
            intel_parent_fb_pin_dpt_unpin(display, (*fb).dpt, (*state).dpt_vma, (*state).ggtt_vma);
            (*state).dpt_vma = ptr::null_mut();
            (*state).ggtt_vma = ptr::null_mut();
        }
    }
}

unsafe fn add_dma_resv_fences(
    resv: *mut crate::linux::gem::DmaResv,
    state: *mut DrmPlaneState,
) -> i32 {
    use tk_linux_kapi::types::{DmaFence, DmaFenceChain};
    if resv.is_null() || state.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let mut fence = if unsafe { (*state).fence.is_null() } {
        ptr::null_mut()
    } else {
        crate::linux::requests::dma_fence_get(unsafe { (*state).fence })
    };
    let mut new = ptr::null_mut::<DmaFence>();
    let ret = unsafe { crate::linux::gem::dma_resv_get_singleton(resv, 1, &mut new) };
    if ret != 0 {
        unsafe {
            crate::linux::requests::dma_fence_put(fence);
        }
        return ret;
    }
    if !new.is_null() && !fence.is_null() {
        let chain = crate::linux::requests::dma_fence_chain_alloc::<DmaFenceChain>();
        if chain.is_null() {
            unsafe {
                crate::linux::requests::dma_fence_put(fence);
                crate::linux::requests::dma_fence_put(new);
            }
            return -tk_linux_kapi::config::ENOMEM;
        }
        unsafe {
            tk_linux_kapi::dma_fence_chain::dma_fence_chain_init(chain, fence, new, 1);
            fence = ptr::addr_of_mut!((*chain).base);
        }
    } else if !new.is_null() {
        fence = new;
    }
    unsafe {
        crate::linux::requests::dma_fence_put((*state).fence);
        (*state).fence = fence;
    }
    0
}

// upstream: intel_plane.c intel_prepare_plane_fb()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_prepare_plane_fb(
    plane_drm: *mut DrmPlane,
    state_drm: *mut DrmPlaneState,
) -> i32 {
    if plane_drm.is_null() || state_drm.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let plane = unsafe { plane_from_drm(plane_drm) };
    let state = unsafe { plane_state_from_drm(state_drm) };
    let display = unsafe { display_from_dev((*plane_drm).dev) };
    if display.is_null() {
        return -tk_linux_kapi::config::ENODEV;
    }
    let atomic = unsafe { container_of!((*state).uapi.state, IntelAtomicState, base) };
    let (old, _) = unsafe { lookup_plane_state(atomic, plane) };
    if old.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let obj = if unsafe { (*state).hw.fb.is_null() } {
        ptr::null_mut()
    } else {
        unsafe { intel_fb_bo((*state).hw.fb) }
    };
    let old_obj = if unsafe { (*old).hw.fb.is_null() } {
        ptr::null_mut()
    } else {
        unsafe { intel_fb_bo((*old).hw.fb) }
    };
    if !old_obj.is_null() {
        let old_crtc = if unsafe { (*old).hw.crtc.is_null() } {
            ptr::null_mut()
        } else {
            unsafe { crtc_from_drm((*old).hw.crtc) }
        };
        let (_, new_crtc) = if old_crtc.is_null() {
            (ptr::null_mut(), ptr::null_mut())
        } else {
            unsafe { lookup_crtc_state(atomic, old_crtc) }
        };
        if !new_crtc.is_null() && unsafe { intel_crtc_needs_modeset(new_crtc) } {
            let gem_obj = old_obj.cast::<DrmGemObject>();
            let ret = unsafe {
                add_dma_resv_fences(
                    (*gem_obj).resv.cast::<crate::linux::gem::DmaResv>(),
                    state_drm,
                )
            };
            if ret != 0 {
                return ret;
            }
        }
    }
    if obj.is_null() {
        return 0;
    }
    let ret = unsafe { intel_plane_pin_fb(state, old) };
    if ret != 0 {
        return ret;
    }
    let ret = unsafe {
        tk_drm_upstream::drm_gem_atomic_helper_upstream::drm_gem_plane_helper_prepare_fb(
            plane_drm, state_drm,
        )
    };
    if ret < 0 {
        unsafe {
            intel_plane_unpin_fb(state);
        }
        return ret;
    }
    if unsafe { !(*state).uapi.fence.is_null() } {
        unsafe {
            crate::intel_parent_upstream::intel_parent_fence_priority_display(
                display,
                (*state).uapi.fence.cast(),
            );
            crate::intel_display_rps_upstream::intel_display_rps_boost_after_vblank(
                (*state).hw.crtc,
                (*state).uapi.fence,
            );
        }
    }
    unsafe {
        crate::intel_display_rps_upstream::intel_display_rps_mark_interactive(
            display, atomic, true,
        );
    }
    0
}

// upstream: intel_plane.c intel_cleanup_plane_fb()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_cleanup_plane_fb(
    plane: *mut DrmPlane,
    state_drm: *mut DrmPlaneState,
) {
    if plane.is_null() || state_drm.is_null() {
        return;
    }
    let display = unsafe { display_from_dev((*plane).dev) };
    let state = unsafe { plane_state_from_drm(state_drm) };
    if unsafe { (*state).hw.fb.is_null() || intel_fb_bo((*state).hw.fb).is_null() } {
        return;
    }
    let atomic = unsafe { container_of!((*state).uapi.state, IntelAtomicState, base) };
    unsafe {
        crate::intel_display_rps_upstream::intel_display_rps_mark_interactive(
            display, atomic, false,
        );
        intel_plane_unpin_fb(state);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_plane_copy_hw_state(
    state: *mut IntelPlaneState,
    from: *const IntelPlaneState,
) {
    if state.is_null() || from.is_null() {
        return;
    }
    unsafe {
        clear_hw_state(state);
        ptr::copy_nonoverlapping(ptr::addr_of!((*from).hw), ptr::addr_of_mut!((*state).hw), 1);
        fb_get((*state).hw.fb);
        for blob in [
            (*state).hw.degamma_lut,
            (*state).hw.gamma_lut,
            (*state).hw.ctm,
            (*state).hw.lut_3d,
        ] {
            if !blob.is_null() {
                tk_drm_upstream::drm_property_upstream::drm_property_blob_get(blob);
            }
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_plane_copy_uapi_to_hw_state(
    state: *mut IntelPlaneState,
    from: *const IntelPlaneState,
    crtc: *mut IntelCrtc,
) {
    if state.is_null() || from.is_null() || crtc.is_null() {
        return;
    }
    unsafe {
        let ctm = (*from).hw.ctm;
        let degamma_lut = (*from).hw.degamma_lut;
        let gamma_lut = (*from).hw.gamma_lut;
        let lut_3d = (*from).hw.lut_3d;
        clear_hw_state(state);
        (*state).hw.crtc = if (*from).uapi.crtc.is_null() {
            ptr::null_mut()
        } else {
            ptr::addr_of_mut!((*crtc).base)
        };
        (*state).hw.fb = (*from).uapi.fb;
        fb_get((*state).hw.fb);
        (*state).hw.alpha = (*from).uapi.alpha;
        (*state).hw.pixel_blend_mode = (*from).uapi.pixel_blend_mode;
        (*state).hw.rotation = (*from).uapi.rotation;
        (*state).hw.color_encoding = (*from).uapi.color_encoding;
        (*state).hw.color_range = (*from).uapi.color_range;
        (*state).hw.scaling_filter = (*from).uapi.scaling_filter;
        (*state).hw.ctm = ctm;
        (*state).hw.degamma_lut = degamma_lut;
        (*state).hw.gamma_lut = gamma_lut;
        (*state).hw.lut_3d = lut_3d;
        for blob in [
            (*state).hw.degamma_lut,
            (*state).hw.gamma_lut,
            (*state).hw.ctm,
            (*state).hw.lut_3d,
        ] {
            if !blob.is_null() {
                tk_drm_upstream::drm_property_upstream::drm_property_blob_get(blob);
            }
        }
        ptr::copy_nonoverlapping(
            ptr::addr_of!((*from).uapi.src),
            ptr::addr_of_mut!((*state).uapi.src),
            1,
        );
        ptr::copy_nonoverlapping(
            ptr::addr_of!((*from).uapi.dst),
            ptr::addr_of_mut!((*state).uapi.dst),
            1,
        );
    }
}

unsafe fn unlink_nv12_plane(crtc_state: *mut IntelCrtcState, plane_state: *mut IntelPlaneState) {
    if crtc_state.is_null() || plane_state.is_null() {
        return;
    }
    let linked = unsafe { (*plane_state).planar_linked_plane };
    if linked.is_null() {
        return;
    }
    unsafe {
        (*plane_state).planar_linked_plane = ptr::null_mut();
        if !(*plane_state).is_y_plane {
            return;
        }
        (*plane_state).is_y_plane = false;
        let id = (*plane_from_drm((*plane_state).uapi.plane)).id;
        let bit = 1u8 << id;
        (*crtc_state).enabled_planes &= !bit;
        (*crtc_state).active_planes &= !bit;
        (*crtc_state).update_planes |= bit;
        (*crtc_state).data_rate[id as usize] = 0;
        (*crtc_state).rel_data_rate[id as usize] = 0;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_plane_set_invisible(
    crtc_state: *mut IntelCrtcState,
    plane_state: *mut IntelPlaneState,
) {
    if crtc_state.is_null() || plane_state.is_null() {
        return;
    }
    unsafe {
        unlink_nv12_plane(crtc_state, plane_state);
        let id = (*plane_from_drm((*plane_state).uapi.plane)).id as usize;
        let bit = 1u8 << id;
        (*crtc_state).active_planes &= !bit;
        (*crtc_state).scaled_planes &= !bit;
        (*crtc_state).nv12_planes &= !bit;
        (*crtc_state).c8_planes &= !bit;
        (*crtc_state).async_flip_planes &= !bit;
        (*crtc_state).data_rate[id] = 0;
        (*crtc_state).data_rate_y[id] = 0;
        (*crtc_state).rel_data_rate[id] = 0;
        (*crtc_state).rel_data_rate_y[id] = 0;
        (*crtc_state).plane_min_cdclk[id] = 0;
        (*plane_state).uapi.visible = false;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_plane_check_clipping(
    plane_state: *mut IntelPlaneState,
    crtc_state: *mut IntelCrtcState,
    min_scale: i32,
    max_scale: i32,
    can_position: bool,
) -> i32 {
    if plane_state.is_null() || crtc_state.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    unsafe {
        let fb = (*plane_state).hw.fb;
        if fb.is_null() {
            (*plane_state).uapi.visible = false;
            return 0;
        }
        let src = ptr::addr_of_mut!((*plane_state).uapi.src);
        let dst = ptr::addr_of_mut!((*plane_state).uapi.dst);
        let clip = ptr::addr_of!((*crtc_state).pipe_src);
        let width = ((*fb).width as i32) << 16;
        let height = ((*fb).height as i32) << 16;
        tk_drm_upstream::drm_rect_upstream::drm_rect_rotate(
            src,
            width,
            height,
            (*plane_state).hw.rotation,
        );
        let hs = tk_drm_upstream::drm_rect_upstream::drm_rect_calc_hscale(
            src, dst, min_scale, max_scale,
        );
        let vs = tk_drm_upstream::drm_rect_upstream::drm_rect_calc_vscale(
            src, dst, min_scale, max_scale,
        );
        if hs < 0 || vs < 0 {
            return -tk_linux_kapi::config::ERANGE;
        }
        (*plane_state).uapi.visible =
            tk_drm_upstream::drm_rect_upstream::drm_rect_clip_scaled(src, dst, clip);
        tk_drm_upstream::drm_rect_upstream::drm_rect_rotate_inv(
            src,
            width,
            height,
            (*plane_state).hw.rotation,
        );
        let dst_ref = &mut *dst;
        let clip_ref = &*clip;
        if !can_position
            && (*plane_state).uapi.visible
            && (dst_ref.x1 != clip_ref.x1
                || dst_ref.y1 != clip_ref.y1
                || dst_ref.x2 != clip_ref.x2
                || dst_ref.y2 != clip_ref.y2)
        {
            return -tk_linux_kapi::config::EINVAL;
        }
        dst_ref.x1 -= clip_ref.x1;
        dst_ref.x2 -= clip_ref.x1;
        dst_ref.y1 -= clip_ref.y1;
        dst_ref.y2 -= clip_ref.y1;
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_plane_check_src_coordinates(state: *mut IntelPlaneState) -> i32 {
    if state.is_null() || unsafe { (*state).hw.fb.is_null() } {
        return -tk_linux_kapi::config::EINVAL;
    }
    unsafe {
        let fb = (*state).hw.fb;
        let fmt = (*fb).format;
        if fmt.is_null() {
            return -tk_linux_kapi::config::EINVAL;
        }
        if (*fb).modifier == I915_FORMAT_MOD_Y_TILED_CCS
            || (*fb).modifier == I915_FORMAT_MOD_Yf_TILED_CCS
        {
            return 0;
        }
        let src = &mut (*state).uapi.src;
        let src_x = src.x1 >> 16;
        let src_y = src.y1 >> 16;
        let src_w = rect_width(src) >> 16;
        let src_h = rect_height(src) >> 16;
        src.x1 = src_x << 16;
        src.y1 = src_y << 16;
        src.x2 = (src_x + src_w) << 16;
        src.y2 = (src_y + src_h) << 16;
        let rotated = matches!(
            (*state).hw.rotation & DRM_MODE_ROTATE_MASK,
            DRM_MODE_ROTATE_90 | DRM_MODE_ROTATE_270
        );
        let mut hsub = (*fmt).hsub.max(1) as u32;
        let mut vsub = (*fmt).vsub.max(1) as u32;
        if (*fmt).format == DRM_FORMAT_RGB565 && rotated {
            hsub = 2;
            vsub = 2;
        }
        if rotated {
            hsub = hsub.max(vsub);
            vsub = hsub;
        }
        if src_x < 0
            || src_y < 0
            || src_w <= 0
            || src_h <= 0
            || src_x as u32 % hsub != 0
            || src_w as u32 % hsub != 0
            || src_y as u32 % vsub != 0
            || src_h as u32 % vsub != 0
        {
            return -tk_linux_kapi::config::EINVAL;
        }
    }
    0
}

unsafe fn lookup_plane_state(
    state: *mut IntelAtomicState,
    plane: *mut IntelPlane,
) -> (*mut IntelPlaneState, *mut IntelPlaneState) {
    if state.is_null() || plane.is_null() {
        return (ptr::null_mut(), ptr::null_mut());
    }
    let dev = unsafe { (*state).base.dev };
    if dev.is_null()
        || unsafe { (*dev).mode_config.num_total_plane <= 0 || (*state).base.planes.is_null() }
    {
        return (ptr::null_mut(), ptr::null_mut());
    }
    for i in 0..unsafe { (*dev).mode_config.num_total_plane as usize } {
        let entry = unsafe { (*state).base.planes.add(i) };
        if unsafe { (*entry).ptr == ptr::addr_of_mut!((*plane).base) } {
            let old = unsafe { (*entry).old_state };
            let new = unsafe { (*entry).new_state };
            let old = if old.is_null() {
                unsafe { (*plane).base.state }
            } else {
                old
            };
            return (
                if old.is_null() {
                    ptr::null_mut()
                } else {
                    unsafe { plane_state_from_drm(old) }
                },
                if new.is_null() {
                    ptr::null_mut()
                } else {
                    unsafe { plane_state_from_drm(new) }
                },
            );
        }
    }
    (ptr::null_mut(), ptr::null_mut())
}

unsafe fn lookup_crtc_state(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
) -> (*mut IntelCrtcState, *mut IntelCrtcState) {
    if state.is_null() || crtc.is_null() {
        return (ptr::null_mut(), ptr::null_mut());
    }
    let dev = unsafe { (*state).base.dev };
    if dev.is_null() || unsafe { (*dev).mode_config.num_crtc <= 0 || (*state).base.crtcs.is_null() }
    {
        return (ptr::null_mut(), ptr::null_mut());
    }
    for i in 0..unsafe { (*dev).mode_config.num_crtc as usize } {
        let entry = unsafe { (*state).base.crtcs.add(i) };
        if unsafe { (*entry).ptr == ptr::addr_of_mut!((*crtc).base) } {
            let old = unsafe { (*entry).old_state };
            let new = unsafe { (*entry).new_state };
            let old = if old.is_null() {
                unsafe { (*crtc).base.state }
            } else {
                old
            };
            return (
                if old.is_null() {
                    ptr::null_mut()
                } else {
                    unsafe { crtc_state_from_drm(old) }
                },
                if new.is_null() {
                    ptr::null_mut()
                } else {
                    unsafe { crtc_state_from_drm(new) }
                },
            );
        }
    }
    (ptr::null_mut(), ptr::null_mut())
}

// upstream: intel_plane.c intel_crtc_get_plane()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_crtc_get_plane(
    crtc: *mut IntelCrtc,
    plane_id: u32,
) -> *mut IntelPlane {
    if crtc.is_null() || unsafe { (*crtc).base.dev.is_null() } {
        return ptr::null_mut();
    }
    let dev = unsafe { (*crtc).base.dev };
    let head = unsafe { ptr::addr_of_mut!((*dev).mode_config.plane_list) };
    let mut entry = unsafe { (*head).next };
    while !entry.is_null() && entry != head {
        let drm_plane = unsafe { container_of!(entry, DrmPlane, head) };
        let plane = unsafe { plane_from_drm(drm_plane) };
        if unsafe { (*plane).pipe == (*crtc).pipe && (*plane).id == plane_id } {
            return plane;
        }
        entry = unsafe { (*entry).next };
    }
    ptr::null_mut()
}

unsafe fn copy_plane_damage(
    new_state: *mut IntelPlaneState,
    old_state: *const IntelPlaneState,
    from_state: *const IntelPlaneState,
) {
    if new_state.is_null() || old_state.is_null() || from_state.is_null() {
        return;
    }
    let plane = unsafe { (*new_state).uapi.plane };
    if plane.is_null() {
        return;
    }
    let display = unsafe { display_from_dev((*plane).dev) };
    if display.is_null() || unsafe { DISPLAY_VER(display) < 12 } {
        return;
    }
    let merged = unsafe {
        tk_drm_upstream::drm_damage_helper_upstream::drm_atomic_helper_damage_merged(
            &(*old_state).uapi,
            &(*from_state).uapi,
            ptr::addr_of_mut!((*new_state).damage),
        )
    };
    if !merged {
        unsafe {
            ptr::copy_nonoverlapping(
                ptr::addr_of!((*from_state).uapi.src),
                ptr::addr_of_mut!((*new_state).damage),
                1,
            );
        }
    }
}

unsafe fn atomic_get_plane_state(
    state: *mut IntelAtomicState,
    plane: *mut IntelPlane,
) -> Result<*mut IntelPlaneState, i32> {
    let uapi = unsafe {
        tk_drm_upstream::drm_atomic_upstream::drm_atomic_get_plane_state(
            &mut (*state).base,
            &mut (*plane).base,
        )
    };
    if tk_linux_kapi::config::IS_ERR(uapi.cast::<c_void>()) {
        return Err(tk_linux_kapi::config::PTR_ERR(uapi.cast::<c_void>()));
    }
    if uapi.is_null() {
        return Err(-tk_linux_kapi::config::ENOMEM);
    }
    Ok(unsafe { plane_state_from_drm(uapi) })
}

unsafe fn add_planes_to_state(state: *mut IntelAtomicState, crtc: *mut IntelCrtc, mask: u8) -> i32 {
    if state.is_null() || crtc.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let dev = unsafe { (*crtc).base.dev };
    if dev.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let head = unsafe { ptr::addr_of_mut!((*dev).mode_config.plane_list) };
    let mut entry = unsafe { (*head).next };
    while !entry.is_null() && entry != head {
        let drm_plane = unsafe { container_of!(entry, DrmPlane, head) };
        let plane = unsafe { plane_from_drm(drm_plane) };
        let id = unsafe { (*plane).id };
        if unsafe { (*plane).pipe == (*crtc).pipe } && mask & (1u8 << id) != 0 {
            if let Err(err) = unsafe { atomic_get_plane_state(state, plane) } {
                return err;
            }
        }
        entry = unsafe { (*entry).next };
    }
    0
}

// upstream: intel_plane.c intel_plane_add_affected()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_plane_add_affected(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
) -> i32 {
    if state.is_null() || crtc.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let (old, new) = unsafe { lookup_crtc_state(state, crtc) };
    let old_mask = if old.is_null() {
        0
    } else {
        unsafe { (*old).enabled_planes }
    };
    let new_mask = if new.is_null() {
        0
    } else {
        unsafe { (*new).enabled_planes }
    };
    unsafe { add_planes_to_state(state, crtc, old_mask | new_mask) }
}

unsafe fn link_nv12_planes(
    crtc_state: *mut IntelCrtcState,
    uv: *mut IntelPlaneState,
    y: *mut IntelPlaneState,
) {
    let uv_plane = unsafe { plane_from_drm((*uv).uapi.plane) };
    let y_plane = unsafe { plane_from_drm((*y).uapi.plane) };
    let uv_id = unsafe { (*uv_plane).id as usize };
    let y_id = unsafe { (*y_plane).id as usize };
    unsafe {
        (*uv).planar_linked_plane = y_plane;
        (*y).is_y_plane = true;
        (*y).planar_linked_plane = uv_plane;
        (*crtc_state).enabled_planes |= 1 << y_id;
        (*crtc_state).active_planes |= 1 << y_id;
        (*crtc_state).update_planes |= 1 << y_id;
        (*crtc_state).data_rate[y_id] = (*crtc_state).data_rate_y[uv_id];
        (*crtc_state).rel_data_rate[y_id] = (*crtc_state).rel_data_rate_y[uv_id];
        intel_plane_copy_hw_state(y, uv);
        ptr::copy_nonoverlapping(
            ptr::addr_of!((*uv).uapi.src),
            ptr::addr_of_mut!((*y).uapi.src),
            1,
        );
        ptr::copy_nonoverlapping(
            ptr::addr_of!((*uv).uapi.dst),
            ptr::addr_of_mut!((*y).uapi.dst),
            1,
        );
        (*y).ctl = (*uv).ctl;
        (*y).color_ctl = (*uv).color_ctl;
        ptr::copy_nonoverlapping(ptr::addr_of!((*uv).view), ptr::addr_of_mut!((*y).view), 1);
        (*y).decrypt = (*uv).decrypt;
    }
    crate::skl_universal_plane_upstream::icl_link_nv12_planes(uv, y);
}

unsafe fn check_nv12_planes(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
    crtc_state: *mut IntelCrtcState,
) -> i32 {
    if state.is_null()
        || crtc.is_null()
        || crtc_state.is_null()
        || unsafe { (*crtc_state).nv12_planes == 0 }
    {
        return 0;
    }
    let display = unsafe { display_from_dev((*state).base.dev) };
    let head = unsafe { ptr::addr_of_mut!((*(*state).base.dev).mode_config.plane_list) };
    let mut entry = unsafe { (*head).next };
    while !entry.is_null() && entry != head {
        let plane = unsafe { plane_from_drm(container_of!(entry, DrmPlane, head)) };
        let uv_id = unsafe { (*plane).id };
        if unsafe { (*plane).pipe == (*crtc).pipe && (*crtc_state).nv12_planes & (1 << uv_id) != 0 }
        {
            let (old_uv, new_uv) = unsafe { lookup_plane_state(state, plane) };
            let uv_state = if !new_uv.is_null() {
                new_uv
            } else {
                match unsafe { atomic_get_plane_state(state, plane) } {
                    Ok(s) => s,
                    Err(e) => return e,
                }
            };
            let mut y_plane = ptr::null_mut();
            let mut y_state = ptr::null_mut();
            let mut y_entry = unsafe { (*head).next };
            while !y_entry.is_null() && y_entry != head {
                let candidate = unsafe { plane_from_drm(container_of!(y_entry, DrmPlane, head)) };
                if unsafe {
                    (*candidate).pipe == (*crtc).pipe
                        && crate::skl_universal_plane_upstream::icl_is_nv12_y_plane(
                            display,
                            (*candidate).id,
                        )
                        && (*crtc_state).active_planes & (1 << (*candidate).id) == 0
                } {
                    y_plane = candidate;
                    y_state = match unsafe { atomic_get_plane_state(state, candidate) } {
                        Ok(s) => s,
                        Err(e) => return e,
                    };
                    break;
                }
                y_entry = unsafe { (*y_entry).next };
            }
            if y_plane.is_null() || y_state.is_null() {
                return -tk_linux_kapi::config::EINVAL;
            }
            unsafe {
                link_nv12_planes(crtc_state, uv_state, y_state);
            }
            let _ = old_uv;
        }
        entry = unsafe { (*entry).next };
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_plane_atomic_check_with_state(
    old_crtc: *const IntelCrtcState,
    new_crtc: *mut IntelCrtcState,
    old_plane: *const IntelPlaneState,
    new_plane: *mut IntelPlaneState,
) -> i32 {
    if new_crtc.is_null() || old_plane.is_null() || new_plane.is_null() {
        return 0;
    }
    let plane = unsafe { plane_from_drm((*new_plane).uapi.plane) };
    let display = unsafe { display_from_dev((*plane).base.dev) };
    unsafe {
        intel_plane_set_invisible(new_crtc, new_plane);
        let id = (*plane).id as usize;
        let bit = 1u8 << id;
        (*new_crtc).enabled_planes &= !bit;
        if (*new_plane).hw.crtc.is_null() && (*old_plane).hw.crtc.is_null() {
            return 0;
        }
        if !(*new_plane).hw.crtc.is_null() && (*plane).check_plane.is_some() {
            let ret = (*plane).check_plane.unwrap()(new_crtc, new_plane);
            if ret != 0 {
                return ret;
            }
        }
        if DISPLAY_VER(display) >= 9 && (*plane).id != PLANE_CURSOR {
            let ret = skl_update_scaler_plane(new_crtc, new_plane);
            if ret != 0 {
                return ret;
            }
        }
        if !(*new_plane).hw.fb.is_null() {
            (*new_crtc).enabled_planes |= bit;
        }
        if (*new_plane).uapi.visible {
            (*new_crtc).active_planes |= bit;
        }
        if (*new_plane).uapi.visible
            && (rect_width(&(*new_plane).uapi.src) >> 16 != rect_width(&(*new_plane).uapi.dst)
                || rect_height(&(*new_plane).uapi.src) >> 16 != rect_height(&(*new_plane).uapi.dst))
        {
            (*new_crtc).scaled_planes |= bit;
        }
        if (*new_plane).uapi.visible && !(*new_plane).hw.fb.is_null() {
            let f = (*(*new_plane).hw.fb).format;
            let semiplanar = !f.is_null()
                && ((*f).format == DRM_FORMAT_NV12
                    || (*f).format == DRM_FORMAT_P010
                    || (*f).format == DRM_FORMAT_P012
                    || (*f).format == DRM_FORMAT_P016);
            if semiplanar {
                (*new_crtc).nv12_planes |= bit;
            }
            if !f.is_null() && (*f).format == DRM_FORMAT_C8 {
                (*new_crtc).c8_planes |= bit;
            }
            if semiplanar {
                (*new_crtc).data_rate_y[id] = intel_plane_data_rate(new_crtc, new_plane, 0);
                (*new_crtc).data_rate[id] = intel_plane_data_rate(new_crtc, new_plane, 1);
                let width = rect_width(&(*new_plane).uapi.src) >> 16;
                let height = rect_height(&(*new_plane).uapi.src) >> 16;
                let cpp_y = (*f)._anon_6.cpp[0] as i32;
                let cpp_uv = (*f)._anon_6.cpp[1] as i32;
                (*new_crtc).rel_data_rate_y[id] = intel_adjusted_rate(
                    &(*new_plane).uapi.src,
                    &(*new_plane).uapi.dst,
                    skl_plane_relative_data_rate(new_crtc, plane, width, height, cpp_y) as u32,
                ) as u64;
                (*new_crtc).rel_data_rate[id] = intel_adjusted_rate(
                    &(*new_plane).uapi.src,
                    &(*new_plane).uapi.dst,
                    skl_plane_relative_data_rate(new_crtc, plane, width / 2, height / 2, cpp_uv)
                        as u32,
                ) as u64;
            } else {
                (*new_crtc).data_rate[id] = intel_plane_data_rate(new_crtc, new_plane, 0);
                let width = rect_width(&(*new_plane).uapi.src) >> 16;
                let height = rect_height(&(*new_plane).uapi.src) >> 16;
                let cpp = (*f)._anon_6.cpp[0] as i32;
                (*new_crtc).rel_data_rate[id] = intel_adjusted_rate(
                    &(*new_plane).uapi.src,
                    &(*new_plane).uapi.dst,
                    skl_plane_relative_data_rate(new_crtc, plane, width, height, cpp) as u32,
                ) as u64;
            }
        }
        if (*new_plane).uapi.visible || (*old_plane).uapi.visible {
            (*new_crtc).update_planes |= bit;
        }
        let was_crtc_enabled = !old_crtc.is_null() && (*old_crtc).hw.active;
        let is_crtc_enabled = (*new_crtc).hw.active;
        let mut was_visible = (*old_plane).uapi.visible;
        let mut visible = (*new_plane).uapi.visible;
        if !was_crtc_enabled {
            was_visible = false;
        }
        if !is_crtc_enabled {
            intel_plane_set_invisible(new_crtc, new_plane);
            visible = false;
        }
        if !was_visible && !visible {
            return 0;
        }
        if was_visible || visible {
            (*new_crtc).fb_bits |= (*plane).frontbuffer_bit;
        }
        if (*new_crtc).uapi.async_flip
            && (*plane).async_flip.is_some()
            && (DISPLAY_VER(display) < 9 || (!old_crtc.is_null() && (*old_crtc).uapi.async_flip))
        {
            (*new_crtc).do_async_flip = true;
        }
        if (*new_crtc).uapi.async_flip {
            (*new_crtc).async_flip_planes |= bit;
        }
        let _ = old_crtc;
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_plane_atomic_check(state: *mut IntelAtomicState) -> i32 {
    if state.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let dev = unsafe { (*state).base.dev };
    if dev.is_null() || unsafe { (*state).base.crtcs.is_null() || (*state).base.planes.is_null() } {
        return -tk_linux_kapi::config::EINVAL;
    }
    let num_crtcs = unsafe { (*dev).mode_config.num_crtc.max(0) as usize };
    for i in 0..num_crtcs {
        let entry = unsafe { (*state).base.crtcs.add(i) };
        if unsafe { (*entry).new_state.is_null() || (*entry).ptr.is_null() } {
            continue;
        }
        let crtc = unsafe { crtc_from_drm((*entry).ptr) };
        let ret = unsafe { intel_plane_add_affected(state, crtc) };
        if ret != 0 {
            return ret;
        }
    }

    let count = unsafe { (*dev).mode_config.num_total_plane.max(0) as usize };
    let display = unsafe { display_from_dev(dev) };
    for i in 0..count {
        let entry = unsafe { (*state).base.planes.add(i) };
        let new_uapi = unsafe { (*entry).new_state };
        if new_uapi.is_null() || unsafe { (*entry).ptr.is_null() } {
            continue;
        }
        let plane = unsafe { plane_from_drm((*entry).ptr) };
        let (old, new) = unsafe { lookup_plane_state(state, plane) };
        if old.is_null() || new.is_null() {
            continue;
        }
        let crtc = unsafe { intel_crtc_for_pipe(display, (*plane).pipe) };
        if crtc.is_null() {
            continue;
        }
        let (old_crtc, mut new_crtc) = unsafe { lookup_crtc_state(state, crtc) };
        if new_crtc.is_null() {
            let raw = unsafe {
                tk_drm_upstream::drm_atomic_upstream::drm_atomic_get_crtc_state(
                    &mut (*state).base,
                    &mut (*crtc).base,
                )
            };
            if tk_linux_kapi::config::IS_ERR(raw.cast::<c_void>()) {
                return tk_linux_kapi::config::PTR_ERR(raw.cast::<c_void>());
            }
            if raw.is_null() {
                return -tk_linux_kapi::config::ENOMEM;
            }
            let (_, current) = unsafe { lookup_crtc_state(state, crtc) };
            new_crtc = current;
        }
        if new_crtc.is_null() {
            return -tk_linux_kapi::config::EINVAL;
        }
        unsafe {
            copy_plane_damage(new, old, new);
            intel_plane_copy_uapi_to_hw_state(new, new, crtc);
            (*new).uapi.visible = false;
        }
        let ret = unsafe { intel_plane_atomic_check_with_state(old_crtc, new_crtc, old, new) };
        if ret != 0 {
            return ret;
        }
    }

    for i in 0..num_crtcs {
        let entry = unsafe { (*state).base.crtcs.add(i) };
        if unsafe { (*entry).new_state.is_null() || (*entry).ptr.is_null() } {
            continue;
        }
        let crtc = unsafe { crtc_from_drm((*entry).ptr) };
        let (_, crtc_state) = unsafe { lookup_crtc_state(state, crtc) };
        if crtc_state.is_null() {
            continue;
        }
        let ret = unsafe { check_nv12_planes(state, crtc, crtc_state) };
        if ret != 0 {
            return ret;
        }
    }

    for i in 0..count {
        let entry = unsafe { (*state).base.planes.add(i) };
        if unsafe { (*entry).new_state.is_null() || (*entry).ptr.is_null() } {
            continue;
        }
        let plane = unsafe { plane_from_drm((*entry).ptr) };
        let new = unsafe { plane_state_from_drm((*entry).new_state) };
        if unsafe {
            !(*new).uapi.visible || (*plane).min_cdclk.is_none() || (*new).hw.crtc.is_null()
        } {
            continue;
        }
        let crtc = unsafe { crtc_from_drm((*new).hw.crtc) };
        let (_, crtc_state) = unsafe { lookup_crtc_state(state, crtc) };
        if !crtc_state.is_null() {
            unsafe {
                (*crtc_state).plane_min_cdclk[(*plane).id as usize] =
                    (*plane).min_cdclk.unwrap()(crtc_state, new);
            }
        }
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_plane_update_noarm(
    dsb: *mut c_void,
    plane: *mut IntelPlane,
    crtc_state: *const IntelCrtcState,
    plane_state: *const IntelPlaneState,
) {
    if plane.is_null() || crtc_state.is_null() || plane_state.is_null() {
        return;
    }
    unsafe {
        if !(*plane).fbc.is_null() {
            intel_fbc_dirty_rect_update_noarm(dsb, plane);
        }
        if let Some(update) = (*plane).update_noarm {
            update(
                dsb,
                plane,
                crtc_state as *mut IntelCrtcState,
                plane_state as *mut IntelPlaneState,
            );
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_plane_update_arm(
    dsb: *mut c_void,
    plane: *mut IntelPlane,
    crtc_state: *const IntelCrtcState,
    plane_state: *const IntelPlaneState,
) {
    if plane.is_null() || crtc_state.is_null() || plane_state.is_null() {
        return;
    }
    unsafe {
        if (*crtc_state).do_async_flip && (*plane).async_flip.is_some() {
            (*plane).async_flip.unwrap()(
                dsb,
                plane,
                crtc_state as *mut IntelCrtcState,
                plane_state as *mut IntelPlaneState,
                true,
            );
        } else if let Some(update) = (*plane).update_arm {
            update(
                dsb,
                plane,
                crtc_state as *mut IntelCrtcState,
                plane_state as *mut IntelPlaneState,
            );
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_plane_disable_arm(
    dsb: *mut c_void,
    plane: *mut IntelPlane,
    crtc_state: *const IntelCrtcState,
) {
    if plane.is_null() || crtc_state.is_null() {
        return;
    }
    if let Some(disable) = unsafe { (*plane).disable_arm } {
        unsafe {
            disable(dsb, plane, crtc_state as *mut IntelCrtcState);
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_crtc_planes_update_noarm(
    dsb: *mut c_void,
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
) {
    if state.is_null() || crtc.is_null() {
        return;
    }
    let (_, new_crtc) = unsafe { lookup_crtc_state(state, crtc) };
    if new_crtc.is_null() || unsafe { (*new_crtc).do_async_flip } {
        return;
    }
    let dev = unsafe { (*state).base.dev };
    let count = if dev.is_null() {
        0
    } else {
        unsafe { (*dev).mode_config.num_total_plane.max(0) as usize }
    };
    for i in 0..count {
        let entry = unsafe { (*state).base.planes.add(i) };
        if unsafe { (*entry).new_state.is_null() || (*entry).ptr.is_null() } {
            continue;
        }
        let plane = unsafe { plane_from_drm((*entry).ptr) };
        let new = unsafe { plane_state_from_drm((*entry).new_state) };
        let bit = 1u8 << unsafe { (*plane).id };
        if unsafe {
            (*plane).pipe != (*crtc).pipe
                || (*new_crtc).update_planes & bit == 0
                || (!(*new).uapi.visible && !(*new).is_y_plane)
        } {
            continue;
        }
        unsafe {
            intel_plane_update_noarm(dsb, plane, new_crtc, new);
        }
    }
}

unsafe fn ddb_move_is_safe(
    new_main: *const SklDdbEntry,
    new_y: *const SklDdbEntry,
    active_main: *const SklDdbEntry,
    active_y: *const SklDdbEntry,
    plane_id: i32,
) -> bool {
    !unsafe { skl_ddb_allocation_overlaps(new_main, active_main, I915_MAX_PLANES as i32, plane_id) }
        && !unsafe {
            skl_ddb_allocation_overlaps(new_y, active_y, I915_MAX_PLANES as i32, plane_id)
        }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_crtc_planes_update_arm(
    dsb: *mut c_void,
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
) {
    if state.is_null() || crtc.is_null() {
        return;
    }
    let (old_crtc, new_crtc) = unsafe { lookup_crtc_state(state, crtc) };
    if new_crtc.is_null() {
        return;
    }
    let dev = unsafe { (*state).base.dev };
    let count = if dev.is_null() {
        0
    } else {
        unsafe { (*dev).mode_config.num_total_plane.max(0) as usize }
    };
    if !old_crtc.is_null() && unsafe { DISPLAY_VER(display_from_dev(dev)) >= 9 } {
        let mut pending = unsafe { (*new_crtc).update_planes };
        let mut ddb: [SklDdbEntry; I915_MAX_PLANES as usize] = unsafe { core::mem::zeroed() };
        let mut ddb_y: [SklDdbEntry; I915_MAX_PLANES as usize] = unsafe { core::mem::zeroed() };
        let (old_wm, new_wm) =
            unsafe { (&(*old_crtc).wm._anon_0_0.skl, &(*new_crtc).wm._anon_0_0.skl) };
        unsafe {
            ptr::copy_nonoverlapping(
                old_wm.plane_ddb.as_ptr(),
                ddb.as_mut_ptr(),
                I915_MAX_PLANES as usize,
            );
            ptr::copy_nonoverlapping(
                old_wm.plane_ddb_y.as_ptr(),
                ddb_y.as_mut_ptr(),
                I915_MAX_PLANES as usize,
            );
        }
        while pending != 0 {
            let mut chosen: *mut IntelPlane = ptr::null_mut();
            let mut chosen_state: *mut IntelPlaneState = ptr::null_mut();
            let mut chosen_id = 0usize;
            for i in 0..count {
                let entry = unsafe { (*state).base.planes.add(i) };
                if unsafe { (*entry).new_state.is_null() || (*entry).ptr.is_null() } {
                    continue;
                }
                let plane = unsafe { plane_from_drm((*entry).ptr) };
                let id = unsafe { (*plane).id };
                if unsafe { (*plane).pipe != (*crtc).pipe || pending & (1 << id) == 0 } {
                    continue;
                }
                let idx = id as usize;
                if !unsafe {
                    ddb_move_is_safe(
                        &new_wm.plane_ddb[idx],
                        &new_wm.plane_ddb_y[idx],
                        ddb.as_ptr(),
                        ddb_y.as_ptr(),
                        id as i32,
                    )
                } {
                    continue;
                }
                chosen = plane;
                chosen_state = unsafe { plane_state_from_drm((*entry).new_state) };
                chosen_id = idx;
                break;
            }
            // Overlapping entries cannot be armed in a defined order without
            // corrupting DDB ownership; leave the remaining planes untouched.
            if chosen.is_null() {
                break;
            }
            let id = unsafe { (*chosen).id };
            pending &= !(1 << id);
            unsafe {
                ptr::copy_nonoverlapping(&new_wm.plane_ddb[chosen_id], &mut ddb[chosen_id], 1);
                ptr::copy_nonoverlapping(&new_wm.plane_ddb_y[chosen_id], &mut ddb_y[chosen_id], 1);
            }
            if unsafe { (*chosen_state).uapi.visible || (*chosen_state).is_y_plane } {
                unsafe {
                    intel_plane_update_arm(dsb, chosen, new_crtc, chosen_state);
                }
            } else {
                unsafe {
                    intel_plane_disable_arm(dsb, chosen, new_crtc);
                }
            }
        }
    } else {
        for i in 0..count {
            let entry = unsafe { (*state).base.planes.add(i) };
            if unsafe { (*entry).new_state.is_null() || (*entry).ptr.is_null() } {
                continue;
            }
            let plane = unsafe { plane_from_drm((*entry).ptr) };
            let new = unsafe { plane_state_from_drm((*entry).new_state) };
            let bit = 1u8 << unsafe { (*plane).id };
            if unsafe { (*plane).pipe != (*crtc).pipe || (*new_crtc).update_planes & bit == 0 } {
                continue;
            }
            if unsafe { (*new).uapi.visible } {
                unsafe {
                    intel_plane_update_arm(dsb, plane, new_crtc, new);
                }
            } else {
                unsafe {
                    intel_plane_disable_arm(dsb, plane, new_crtc);
                }
            }
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_plane_init_cursor_vblank_work(
    _old: *mut IntelPlaneState,
    _new: *mut IntelPlaneState,
) {
    if _old.is_null() || _new.is_null() {
        return;
    }
    unsafe {
        if (*_old).ggtt_vma.is_null() || (*_old).ggtt_vma == (*_new).ggtt_vma {
            return;
        }
        tk_drm_upstream::drm_vblank_work_upstream::drm_vblank_work_init(
            ptr::addr_of_mut!((*_old).unpin_work),
            (*_old).hw.crtc,
            intel_cursor_unpin_work,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adjusted_rate_caps_downscale_and_preserves_upsampling() {
        let src = DrmRect {
            x1: 0,
            y1: 0,
            x2: 1920 << 16,
            y2: 1080 << 16,
        };
        let down = DrmRect {
            x1: 0,
            y1: 0,
            x2: 960,
            y2: 540,
        };
        let up = DrmRect {
            x1: 0,
            y1: 0,
            x2: 3840,
            y2: 2160,
        };
        assert_eq!(
            unsafe { intel_adjusted_rate(&src, &down, 148_500) },
            594_000
        );
        assert_eq!(unsafe { intel_adjusted_rate(&src, &up, 148_500) }, 148_500);
    }

    #[test]
    fn clipping_rejects_out_of_range_scale_and_translates_to_pipe_space() {
        let mut state: IntelPlaneState = unsafe { core::mem::zeroed() };
        let mut crtc: IntelCrtcState = unsafe { core::mem::zeroed() };
        let mut fb: DrmFramebuffer = unsafe { core::mem::zeroed() };
        fb.width = 200;
        fb.height = 100;
        state.hw.fb = &mut fb;
        state.uapi.src = DrmRect {
            x1: 0,
            y1: 0,
            x2: 200 << 16,
            y2: 100 << 16,
        };
        state.uapi.dst = DrmRect {
            x1: -10,
            y1: 20,
            x2: 190,
            y2: 120,
        };
        crtc.pipe_src = DrmRect {
            x1: 0,
            y1: 0,
            x2: 180,
            y2: 100,
        };
        assert_eq!(
            unsafe {
                intel_plane_check_clipping(
                    &mut state,
                    &mut crtc,
                    DRM_PLANE_NO_SCALING,
                    DRM_PLANE_NO_SCALING,
                    true,
                )
            },
            0
        );
        assert!(state.uapi.visible);
        assert_eq!(state.uapi.dst.x1, 0);
        assert_eq!(state.uapi.dst.x2, 180);
    }

    #[test]
    fn cursor_surface_address_checks_register_width_without_treating_dpt_as_bytes() {
        assert_eq!(checked_plane_surface_address(0x1000, 0x2004), Some(0x3004));
        assert_eq!(
            checked_plane_surface_address(u32::MAX - 4, 4),
            Some(u32::MAX)
        );
        assert_eq!(checked_plane_surface_address(u32::MAX - 3, 4), None);
        assert_eq!(checked_plane_surface_address(0xffff_f000, 0x1000), None);
    }

    #[test]
    fn ddb_arm_order_waits_until_the_old_overlapping_entry_has_moved() {
        let mut old_main: [SklDdbEntry; I915_MAX_PLANES as usize] = unsafe { core::mem::zeroed() };
        let mut new_main: [SklDdbEntry; I915_MAX_PLANES as usize] = unsafe { core::mem::zeroed() };
        let old_y: [SklDdbEntry; I915_MAX_PLANES as usize] = unsafe { core::mem::zeroed() };
        let new_y: [SklDdbEntry; I915_MAX_PLANES as usize] = unsafe { core::mem::zeroed() };
        old_main[0] = SklDdbEntry { start: 0, end: 10 };
        old_main[1] = SklDdbEntry { start: 10, end: 20 };
        old_main[2] = SklDdbEntry { start: 20, end: 30 };
        new_main[0] = SklDdbEntry { start: 10, end: 20 };
        new_main[1] = SklDdbEntry { start: 30, end: 40 };
        assert!(!unsafe {
            ddb_move_is_safe(
                &new_main[0],
                &new_y[0],
                old_main.as_ptr(),
                old_y.as_ptr(),
                0,
            )
        });
        assert!(unsafe {
            ddb_move_is_safe(
                &new_main[1],
                &new_y[1],
                old_main.as_ptr(),
                old_y.as_ptr(),
                1,
            )
        });
        unsafe {
            ptr::copy_nonoverlapping(&new_main[1], &mut old_main[1], 1);
        }
        assert!(unsafe {
            ddb_move_is_safe(
                &new_main[0],
                &new_y[0],
                old_main.as_ptr(),
                old_y.as_ptr(),
                0,
            )
        });
    }
}
