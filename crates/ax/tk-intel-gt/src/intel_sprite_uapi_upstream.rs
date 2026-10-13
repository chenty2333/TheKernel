// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Legacy sprite color-key ioctl from Linux v7.2.3 `intel_sprite_uapi.c`.
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::{ffi::c_void, mem::offset_of};

use tk_drm_upstream::{
    drm_atomic_types::DrmAtomicCommit,
    drm_device_types::DrmDevice,
    drm_file_types::DrmFile,
    drm_mode_object_types::DrmModeObject,
    drm_modeset_lock_types::DrmModesetAcquireCtx,
    drm_plane_types::{DRM_PLANE_TYPE_OVERLAY, DrmPlane, DrmPlaneState},
    uapi_drm_consts::DRM_MODE_OBJECT_PLANE,
};

use crate::{
    i915_external_placeholder_types_upstream::DrmIntelSpriteColorkey,
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_limits_types_upstream::{PLANE_3, PLANE_PRIMARY, PlaneId},
    intel_display_types_upstream::{IntelAtomicState, IntelCrtc, IntelPlane, IntelPlaneState},
    intel_display_wa_upstream::DISPLAY_VER,
    linux_i915_private::DrmI915Private,
};

const I915_SET_COLORKEY_NONE: u32 = 1 << 0;
const I915_SET_COLORKEY_DESTINATION: u32 = 1 << 1;
const I915_SET_COLORKEY_SOURCE: u32 = 1 << 2;
const ENOENT: i32 = tk_linux_kapi::config::ENOENT;
const EINVAL: i32 = tk_linux_kapi::config::EINVAL;
const ENOMEM: i32 = tk_linux_kapi::config::ENOMEM;
const EDEADLK: i32 = tk_linux_kapi::config::EDEADLK;

unsafe extern "C" {
    fn drm_mode_object_find(
        dev: *mut DrmDevice,
        file_priv: *mut DrmFile,
        id: u32,
        object_type: u32,
    ) -> *mut DrmModeObject;
    fn drm_modeset_acquire_init(ctx: *mut DrmModesetAcquireCtx, flags: u32);
    fn drm_modeset_acquire_fini(ctx: *mut DrmModesetAcquireCtx);
    fn drm_modeset_drop_locks(ctx: *mut DrmModesetAcquireCtx);
    fn drm_modeset_backoff(ctx: *mut DrmModesetAcquireCtx) -> i32;
    fn drm_atomic_commit_alloc(dev: *mut DrmDevice) -> *mut DrmAtomicCommit;
    fn drm_atomic_get_plane_state(
        state: *mut DrmAtomicCommit,
        plane: *mut DrmPlane,
    ) -> *mut DrmPlaneState;
    fn drm_atomic_commit_clear(state: *mut DrmAtomicCommit);
    fn drm_atomic_commit(state: *mut DrmAtomicCommit) -> i32;
    fn intel_crtc_for_pipe(display: *mut IntelDisplay, pipe: i32) -> *mut IntelCrtc;
}

#[inline]
unsafe fn display_from_drm(dev: *mut DrmDevice) -> *mut IntelDisplay {
    if dev.is_null() || unsafe { (*dev).dev_private.is_null() } {
        return core::ptr::null_mut();
    }
    let i915 = unsafe { (*dev).dev_private.cast::<DrmI915Private>() };
    unsafe { (*i915).display.cast::<IntelDisplay>() }
}

#[inline]
unsafe fn platform_bit(display: *const IntelDisplay, bit: usize) -> bool {
    if display.is_null() || bit >= 72 {
        return false;
    }
    unsafe {
        let bytes = core::ptr::addr_of!((*display).platform).cast::<u8>();
        (bytes.add(bit / 8).read() & (1 << (bit % 8))) != 0
    }
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

/// Apply the source/destination-key ownership rules to one plane's atomic
/// state, field-for-field with Linux `intel_plane_set_ckey()`.
unsafe fn set_colorkey(plane_state: *mut IntelPlaneState, key: &DrmIntelSpriteColorkey) {
    if plane_state.is_null() {
        return;
    }
    let plane = unsafe { intel_plane_from_drm((*plane_state).uapi.plane) };
    if plane.is_null() {
        return;
    }
    let version = unsafe {
        let drm = (*plane_state)
            .uapi
            .plane
            .as_ref()
            .map_or(core::ptr::null_mut(), |p| p.dev);
        let display = display_from_drm(drm);
        if display.is_null() {
            0
        } else {
            DISPLAY_VER(display)
        }
    };
    unsafe {
        (*plane_state).ckey.plane_id = key.plane_id;
        (*plane_state).ckey.min_value = key.min_value;
        (*plane_state).ckey.channel_mask = key.channel_mask;
        (*plane_state).ckey.max_value = key.max_value;
        (*plane_state).ckey.flags = colorkey_flags((*plane).id, key.flags, version);
    }
}

fn colorkey_flags(plane_id: PlaneId, flags: u32, display_ver: u8) -> u32 {
    if plane_id == PLANE_PRIMARY && flags & I915_SET_COLORKEY_SOURCE != 0 {
        return 0;
    }
    if display_ver >= 9 && plane_id != PLANE_PRIMARY && flags & I915_SET_COLORKEY_DESTINATION != 0 {
        return 0;
    }
    flags
}

/// Linux `intel_sprite_set_colorkey_ioctl()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_sprite_set_colorkey_ioctl(
    dev: *mut DrmDevice,
    data: *mut c_void,
    file_priv: *mut DrmFile,
) -> i32 {
    if dev.is_null() || data.is_null() || file_priv.is_null() {
        return -EINVAL;
    }
    let display = unsafe { display_from_drm(dev) };
    if display.is_null() {
        return -EINVAL;
    }
    let key = unsafe { &mut *data.cast::<DrmIntelSpriteColorkey>() };
    key.flags &= !I915_SET_COLORKEY_NONE;
    if key.flags & !(I915_SET_COLORKEY_DESTINATION | I915_SET_COLORKEY_SOURCE) != 0
        || key.flags & (I915_SET_COLORKEY_DESTINATION | I915_SET_COLORKEY_SOURCE)
            == (I915_SET_COLORKEY_DESTINATION | I915_SET_COLORKEY_SOURCE)
    {
        return -EINVAL;
    }
    if (unsafe { platform_bit(display, 20) || platform_bit(display, 27) })
        && key.flags & I915_SET_COLORKEY_DESTINATION != 0
    {
        return -EINVAL;
    }

    let mode_object =
        unsafe { drm_mode_object_find(dev, file_priv, key.plane_id, DRM_MODE_OBJECT_PLANE) };
    if mode_object.is_null() {
        return -ENOENT;
    }
    let plane = unsafe {
        (mode_object as *mut u8)
            .sub(offset_of!(DrmPlane, base))
            .cast::<DrmPlane>()
    };
    if unsafe { (*plane).r#type != DRM_PLANE_TYPE_OVERLAY } {
        return -ENOENT;
    }
    let intel_plane = unsafe { intel_plane_from_drm(plane) };
    if unsafe { DISPLAY_VER(display) >= 9 }
        && unsafe { (*intel_plane).id >= PLANE_3 }
        && key.flags & I915_SET_COLORKEY_DESTINATION != 0
    {
        return -EINVAL;
    }

    let mut ctx = core::mem::MaybeUninit::<DrmModesetAcquireCtx>::zeroed();
    let ctx = ctx.as_mut_ptr();
    unsafe { drm_modeset_acquire_init(ctx, 0) };

    let state = unsafe { drm_atomic_commit_alloc((*plane).dev) };
    let mut ret = 0;
    if state.is_null() {
        ret = -ENOMEM;
        unsafe {
            drm_modeset_drop_locks(ctx);
            drm_modeset_acquire_fini(ctx);
        }
        return ret;
    }
    unsafe {
        (*state).acquire_ctx = ctx;
        (*state.cast::<IntelAtomicState>()).internal = true;
    }

    loop {
        let plane_state = unsafe { drm_atomic_get_plane_state(state, plane) };
        ret = if tk_linux_kapi::config::IS_ERR(plane_state) {
            tk_linux_kapi::config::PTR_ERR(plane_state)
        } else if plane_state.is_null() {
            -EINVAL
        } else {
            unsafe { set_colorkey(plane_state.cast::<IntelPlaneState>(), key) };
            0
        };

        if ret == 0 && unsafe { DISPLAY_VER(display) >= 9 } {
            let crtc = unsafe { intel_crtc_for_pipe(display, (*intel_plane).pipe) };
            if crtc.is_null() || unsafe { (*crtc).base.primary.is_null() } {
                ret = -ENOENT;
            } else {
                let primary = unsafe { (*crtc).base.primary };
                let primary_state = unsafe { drm_atomic_get_plane_state(state, primary) };
                ret = if tk_linux_kapi::config::IS_ERR(primary_state) {
                    tk_linux_kapi::config::PTR_ERR(primary_state)
                } else if primary_state.is_null() {
                    -EINVAL
                } else {
                    unsafe { set_colorkey(primary_state.cast::<IntelPlaneState>(), key) };
                    0
                };
            }
        }

        if ret == 0 {
            ret = unsafe { drm_atomic_commit(state) };
        }
        if ret != -EDEADLK {
            break;
        }
        unsafe {
            drm_atomic_commit_clear(state);
            let _ = drm_modeset_backoff(ctx);
        }
    }

    unsafe {
        tk_drm_upstream::drm_atomic_upstream::drm_atomic_commit_put(state);
        drm_modeset_drop_locks(ctx);
        drm_modeset_acquire_fini(ctx);
    }
    ret
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skl_destination_key_is_owned_by_primary_only() {
        assert_eq!(
            colorkey_flags(PLANE_PRIMARY, I915_SET_COLORKEY_SOURCE, 13),
            0
        );
        assert_eq!(
            colorkey_flags(1, I915_SET_COLORKEY_DESTINATION, 8),
            I915_SET_COLORKEY_DESTINATION
        );
        assert_eq!(colorkey_flags(1, I915_SET_COLORKEY_DESTINATION, 13), 0);
        assert_eq!(
            colorkey_flags(1, I915_SET_COLORKEY_SOURCE, 13),
            I915_SET_COLORKEY_SOURCE
        );
    }
}
