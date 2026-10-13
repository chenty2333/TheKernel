// SPDX-License-Identifier: MIT
// Copyright © 2025 Intel Corporation.
// Source: drivers/gpu/drm/i915/display/intel_initial_plane.c
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::{
    ffi::c_void,
    mem::{MaybeUninit, offset_of},
    ptr,
    sync::atomic::{AtomicI32, Ordering},
};

use tk_drm_upstream::{
    drm_crtc_types::DrmCrtc,
    drm_framebuffer_types::DrmFramebuffer,
    drm_gem_types::DrmGemObject,
    drm_plane_types::DrmPlane,
    uapi_drm_consts::{DRM_MODE_ROTATE_90, DRM_MODE_ROTATE_270},
};

use crate::{
    intel_de_upstream::intel_de_read,
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_limits_types_upstream::I915_MAX_PIPES,
    intel_display_regs_upstream::PIPE_FRMTMSTMP,
    intel_display_types_upstream::{
        IntelCrtc, IntelCrtcState, IntelInitialPlaneConfig, IntelPlane, IntelPlaneState,
    },
    intel_fb_upstream::{intel_fb_fill_view, intel_fb_modifier_uses_dpt},
    intel_frontbuffer_types_upstream::IntelFrontbuffer,
    intel_parent_upstream::{
        IntelDisplayInitialPlaneInterface, intel_display_parent, intel_parent_has_fenced_regions,
        intel_parent_irq_enabled,
    },
};

unsafe extern "C" {
    fn intel_plane_disable_noatomic(crtc: *mut IntelCrtc, plane: *mut IntelPlane);
    fn intel_plane_copy_uapi_to_hw_state(
        state: *mut IntelPlaneState,
        from: *const IntelPlaneState,
        crtc: *mut IntelCrtc,
    );
    fn intel_crtc_wait_for_next_vblank(crtc: *mut IntelCrtc);
}

#[inline]
unsafe fn initial_plane_interface(
    display: *mut IntelDisplay,
) -> *const IntelDisplayInitialPlaneInterface {
    let parent = unsafe { intel_display_parent(display) };
    if parent.is_null() {
        ptr::null()
    } else {
        unsafe { (*parent).initial_plane }
    }
}

#[inline]
unsafe fn initial_plane_ops_complete(display: *mut IntelDisplay) -> bool {
    let ops = unsafe { initial_plane_interface(display) };
    !ops.is_null()
        && unsafe {
            (*ops).alloc_obj.is_some() && (*ops).setup.is_some() && (*ops).config_fini.is_some()
        }
}

#[inline]
unsafe fn framebuffer_get(fb: *mut DrmFramebuffer) {
    if !fb.is_null() {
        unsafe { tk_linux_kapi::memory::kref_get(ptr::addr_of_mut!((*fb).base.refcount)) };
    }
}

#[inline]
unsafe fn framebuffer_put(fb: *mut DrmFramebuffer) {
    if !fb.is_null() {
        unsafe {
            tk_linux_kapi::memory::kref_put(
                ptr::addr_of_mut!((*fb).base.refcount),
                tk_drm_upstream::drm_framebuffer_upstream::drm_framebuffer_free,
            );
        }
    }
}

unsafe fn disable_primary(crtc: *mut IntelCrtc) {
    if crtc.is_null() {
        return;
    }
    let primary = unsafe { (*crtc).base.primary };
    if primary.is_null() {
        return;
    }
    unsafe { intel_plane_disable_noatomic(crtc, primary.cast()) };
}

unsafe fn active_crtc_state(crtc: *mut IntelCrtc) -> *mut IntelCrtcState {
    if crtc.is_null() || unsafe { (*crtc).base.state.is_null() } {
        ptr::null_mut()
    } else {
        unsafe { (*crtc).base.state.cast() }
    }
}

unsafe fn initial_plane_state(plane: *mut IntelPlane) -> *mut IntelPlaneState {
    if plane.is_null() || unsafe { (*plane).base.state.is_null() } {
        ptr::null_mut()
    } else {
        unsafe { (*plane).base.state.cast() }
    }
}

unsafe fn crtc_from_list_node(node: *mut tk_linux_kapi::types::ListHead) -> *mut IntelCrtc {
    let drm_crtc = unsafe {
        node.cast::<u8>()
            .sub(offset_of!(DrmCrtc, head))
            .cast::<DrmCrtc>()
    };
    drm_crtc.cast()
}

#[inline]
fn config_for_pipe(
    configs: *const IntelInitialPlaneConfig,
    pipe: i32,
) -> *const IntelInitialPlaneConfig {
    if configs.is_null() || pipe < 0 || pipe >= I915_MAX_PIPES {
        ptr::null()
    } else {
        unsafe { configs.add(pipe as usize) }
    }
}

#[inline]
fn same_initial_surface(
    config_a: *const IntelInitialPlaneConfig,
    config_b: *const IntelInitialPlaneConfig,
) -> bool {
    !config_a.is_null() && !config_b.is_null() && unsafe { (*config_a).base == (*config_b).base }
}

unsafe fn reuse_initial_plane_state(
    display: *mut IntelDisplay,
    pipe: i32,
    configs: *const IntelInitialPlaneConfig,
) -> *const IntelPlaneState {
    if display.is_null() || unsafe { (*display).drm.is_null() } {
        return ptr::null();
    }
    let this_config = config_for_pipe(configs, pipe);
    if this_config.is_null() {
        return ptr::null();
    }
    let head = unsafe { ptr::addr_of!((*(*display).drm).mode_config.crtc_list) };
    let mut node = unsafe { (*head).next };
    while !node.is_null() && node != head.cast_mut() {
        let crtc = unsafe { crtc_from_list_node(node) };
        let crtc_pipe = unsafe { (*crtc).pipe };
        let other_config = config_for_pipe(configs, crtc_pipe);
        let crtc_state = unsafe { active_crtc_state(crtc) };
        let primary = unsafe { (*crtc).base.primary };
        let plane = primary.cast::<IntelPlane>();
        let plane_state = unsafe { initial_plane_state(plane) };
        if !crtc_state.is_null()
            && unsafe { (*crtc_state).hw.active }
            && !other_config.is_null()
            && same_initial_surface(this_config, other_config)
            && !plane_state.is_null()
            && !unsafe { (*plane_state).ggtt_vma.is_null() }
        {
            return plane_state;
        }
        node = unsafe { (*node).next };
    }
    ptr::null()
}

unsafe fn alloc_initial_plane_object(
    display: *mut IntelDisplay,
    config: *mut IntelInitialPlaneConfig,
) -> *mut DrmGemObject {
    if display.is_null() || config.is_null() {
        return ptr::null_mut();
    }
    let fb = unsafe { (*config).fb };
    if fb.is_null() {
        return ptr::null_mut();
    }
    let modifier = unsafe { (*fb).modifier };
    match modifier {
        tk_drm_upstream::uapi_drm_consts::DRM_FORMAT_MOD_LINEAR => {}
        crate::intel_fb_upstream::I915_FORMAT_MOD_X_TILED
        | crate::intel_fb_upstream::I915_FORMAT_MOD_Y_TILED
            if unsafe { intel_parent_has_fenced_regions(display) } => {}
        crate::intel_fb_upstream::I915_FORMAT_MOD_X_TILED
        | crate::intel_fb_upstream::I915_FORMAT_MOD_Y_TILED => return ptr::null_mut(),
        _ => return ptr::null_mut(),
    }
    if unsafe { intel_fb_modifier_uses_dpt(display, modifier) }
        || unsafe { (*config).rotation as u32 & (DRM_MODE_ROTATE_90 | DRM_MODE_ROTATE_270) != 0 }
    {
        return ptr::null_mut();
    }
    let ops = unsafe { initial_plane_interface(display) };
    if ops.is_null() {
        return ptr::null_mut();
    }
    let Some(alloc_obj) = (unsafe { (*ops).alloc_obj }) else {
        return ptr::null_mut();
    };
    let obj = unsafe { alloc_obj((*display).drm, config) }.cast::<DrmGemObject>();
    if obj.is_null() || unsafe { tk_linux_kapi::config::IS_ERR(obj) } {
        ptr::null_mut()
    } else {
        obj
    }
}

unsafe fn find_initial_plane_obj(crtc: *mut IntelCrtc, configs: *mut IntelInitialPlaneConfig) {
    if crtc.is_null() || configs.is_null() {
        return;
    }
    let display =
        unsafe { crate::intel_display_conversion_upstream::__drm_to_display((*crtc).base.dev) };
    let pipe = unsafe { (*crtc).pipe };
    let config = config_for_pipe(configs, pipe).cast_mut();
    let primary = unsafe { (*crtc).base.primary };
    let plane = primary.cast::<IntelPlane>();
    let plane_state = unsafe { initial_plane_state(plane) };
    if config.is_null() || plane_state.is_null() {
        unsafe { disable_primary(crtc) };
        return;
    }
    let config_fb = unsafe { (*config).fb };
    if config_fb.is_null() {
        unsafe { disable_primary(crtc) };
        return;
    }

    let (fb, vma) = if !unsafe { alloc_initial_plane_object(display, config) }.is_null() {
        (config_fb, unsafe { (*config).vma })
    } else {
        let other = unsafe { reuse_initial_plane_state(display, pipe, configs) };
        if other.is_null() {
            unsafe { disable_primary(crtc) };
            return;
        }
        (unsafe { (*other).hw.fb }, unsafe { (*other).ggtt_vma })
    };
    if fb.is_null() || vma.is_null() {
        unsafe { disable_primary(crtc) };
        return;
    }
    let display = if display.is_null() {
        unsafe { crate::intel_display_conversion_upstream::__drm_to_display((*fb).dev) }
    } else {
        display
    };
    if display.is_null() {
        unsafe { disable_primary(crtc) };
        return;
    }

    unsafe {
        (*plane_state).uapi.rotation = (*config).rotation as u32;
        intel_fb_fill_view(
            fb.cast(),
            (*plane_state).uapi.rotation,
            ptr::addr_of_mut!((*plane_state).view),
        );
    }
    let ops = unsafe { initial_plane_interface(display) };
    let Some(setup) = (if ops.is_null() {
        None
    } else {
        unsafe { (*ops).setup }
    }) else {
        unsafe { disable_primary(crtc) };
        return;
    };
    let ret = unsafe { setup(ptr::addr_of_mut!((*plane_state).uapi), config, fb, vma) };
    if ret != 0 {
        unsafe { disable_primary(crtc) };
        return;
    }

    let width = unsafe { (*fb).width };
    let height = unsafe { (*fb).height };
    if width > (i32::MAX as u32 >> 16) {
        unsafe { disable_primary(crtc) };
        return;
    }
    if height > (i32::MAX as u32 >> 16) {
        unsafe { disable_primary(crtc) };
        return;
    }
    let src_w = width << 16;
    let src_h = height << 16;
    let uapi = unsafe { ptr::addr_of_mut!((*plane_state).uapi) };
    unsafe {
        (*uapi).src_x = 0;
        (*uapi).src_y = 0;
        (*uapi).src_w = src_w;
        (*uapi).src_h = src_h;
        (*uapi).src.x1 = 0;
        (*uapi).src.y1 = 0;
        (*uapi).src.x2 = src_w as i32;
        (*uapi).src.y2 = src_h as i32;
        (*uapi).crtc_x = 0;
        (*uapi).crtc_y = 0;
        (*uapi).crtc_w = width;
        (*uapi).crtc_h = height;
        (*uapi).dst.x1 = 0;
        (*uapi).dst.y1 = 0;
        (*uapi).dst.x2 = width as i32;
        (*uapi).dst.y2 = height as i32;
        (*uapi).fb = fb;
        framebuffer_get(fb);
        (*uapi).crtc = ptr::addr_of_mut!((*crtc).base);
    }
    unsafe { intel_plane_copy_uapi_to_hw_state(plane_state, plane_state, crtc) };
    let front = unsafe {
        (*fb.cast::<crate::intel_display_types_upstream::IntelFramebuffer>()).frontbuffer
    };
    if front.is_null() {
        unsafe { disable_primary(crtc) };
        return;
    }
    let bits = unsafe { ptr::addr_of_mut!((*front).bits.counter).cast::<AtomicI32>() };
    unsafe { (&*bits).fetch_or((*plane).frontbuffer_bit as i32, Ordering::Relaxed) };
}

unsafe fn plane_config_fini(display: *mut IntelDisplay, config: *mut IntelInitialPlaneConfig) {
    if display.is_null() || config.is_null() {
        return;
    }
    let fb = unsafe { (*config).fb };
    if !fb.is_null() {
        let refcount = unsafe { tk_linux_kapi::memory::kref_read(&(*fb).base.refcount) };
        if refcount != 0 {
            unsafe { framebuffer_put(fb) };
        } else {
            unsafe { tk_linux_kapi::memory::kfree(fb.cast::<c_void>()) };
        }
    }
    let ops = unsafe { initial_plane_interface(display) };
    if !ops.is_null() {
        if let Some(config_fini) = unsafe { (*ops).config_fini } {
            unsafe { config_fini(config) };
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_initial_plane_vblank_wait(crtc: *mut IntelCrtc) {
    if crtc.is_null() {
        return;
    }
    let display =
        unsafe { crate::intel_display_conversion_upstream::__drm_to_display((*crtc).base.dev) };
    if display.is_null() {
        return;
    }
    if unsafe { intel_parent_irq_enabled(display) } {
        unsafe { intel_crtc_wait_for_next_vblank(crtc) };
        return;
    }
    let pipe = unsafe { (*crtc).pipe };
    if pipe < 0 {
        return;
    }
    let reg = PIPE_FRMTMSTMP(pipe as u32);
    let start = unsafe { intel_de_read(display, reg) };
    for _ in 0..1000 {
        tk_linux_kapi::primitives::udelay(1000);
        if unsafe { intel_de_read(display, reg) } != start {
            return;
        }
    }
    let _ = tk_linux_kapi::assert::warn_on(true);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_initial_plane_config(display: *mut IntelDisplay) {
    if display.is_null() || unsafe { (*display).drm.is_null() } {
        return;
    }
    let modeset_funcs = unsafe { (*display).modeset.funcs };
    let ops_ready = !modeset_funcs.is_null() && unsafe { initial_plane_ops_complete(display) };
    let mut configs: [IntelInitialPlaneConfig; I915_MAX_PIPES as usize] =
        unsafe { MaybeUninit::zeroed().assume_init() };
    let head = unsafe { ptr::addr_of!((*(*display).drm).mode_config.crtc_list) };
    let mut node = unsafe { (*head).next };
    while !node.is_null() && node != head.cast_mut() {
        let crtc = unsafe { crtc_from_list_node(node) };
        let crtc_state = unsafe { active_crtc_state(crtc) };
        let pipe = unsafe { (*crtc).pipe };
        if !crtc_state.is_null()
            && unsafe { (*crtc_state).hw.active }
            && pipe >= 0
            && pipe < I915_MAX_PIPES
        {
            let config = ptr::addr_of_mut!(configs[pipe as usize]);
            if !ops_ready {
                unsafe { disable_primary(crtc) };
            } else {
                let get_config = if modeset_funcs.is_null() {
                    None
                } else {
                    unsafe { (*modeset_funcs).get_initial_plane_config }
                };
                if let Some(get_config) = get_config {
                    unsafe { get_config(crtc, config) };
                    unsafe { find_initial_plane_obj(crtc, configs.as_mut_ptr()) };
                    if let Some(fixup) = unsafe { (*modeset_funcs).fixup_initial_plane_config } {
                        if unsafe { fixup(crtc, config) } {
                            unsafe { intel_initial_plane_vblank_wait(crtc) };
                        }
                    }
                } else {
                    unsafe { disable_primary(crtc) };
                }
                unsafe { plane_config_fini(display, config) };
            }
        }
        node = unsafe { (*node).next };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_plane_surface_reuse_requires_exact_base() {
        let make = |base| IntelInitialPlaneConfig {
            fb: ptr::null_mut(),
            vma: ptr::null_mut(),
            size: 4096,
            base,
            rotation: 0,
            _pad_25: [0; 7],
        };
        let a = make(0x1000);
        let same = make(0x1000);
        let different = make(0x2000);
        assert!(same_initial_surface(&a, &same));
        assert!(!same_initial_surface(&a, &different));
    }
}
