// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_atomic_uapi.c` (batch B2a).

#![allow(non_snake_case, dead_code, unsafe_code)]
#![allow(clippy::missing_safety_doc)]

use core::ffi::{c_int, c_void};
use core::mem::size_of;

use crate::drm_atomic_types::{DrmAtomicCommit, DrmCrtcCommit};
use crate::drm_connector_types::{DrmConnector, DrmConnectorState, DRM_LINK_STATUS_GOOD};
use crate::drm_crtc_types::{DrmCrtc, DrmCrtcState};
use crate::drm_device_types::DrmDevice;
use crate::drm_file_types::DrmFile;
use crate::drm_framebuffer_types::DrmFramebuffer;
use crate::drm_mode_object_types::DrmModeObject;
use crate::drm_modes_types::DrmDisplayMode;
use crate::drm_modeset_lock_types::DrmModesetAcquireCtx;
use crate::drm_modeset_lock_upstream::drm_modeset_lock;
use crate::drm_plane_types::{DrmPlane, DrmPlaneState, DRM_PLANE_TYPE_CURSOR};
use crate::drm_property_types::{DrmProperty, DrmPropertyBlob};
use crate::drm_vblank_types::DrmPendingVblankEvent;
use crate::uapi_drm_consts::*;
use crate::uapi_drm_mode_types::{DrmColorLut, DrmModeAtomic, DrmModeModeinfo, DrmModeRect};
use crate::uapi_drm_types::{DrmEvent, DrmEventVblank};

use tk_linux_kapi::config::{
    EDEADLK, EFAULT, EINVAL, ENOENT, ENOMEM, EOPNOTSUPP, GFP_KERNEL, IS_ERR, PTR_ERR,
};
use tk_linux_kapi::memory::{kfree, kzalloc};
use tk_linux_kapi::types::DmaFence;

const EACCES: i32 = 13;
const O_CLOEXEC: i32 = 0x80000;
const DRIVER_ATOMIC: u32 = 1 << 4;
const DRM_MODESET_ACQUIRE_INTERRUPTIBLE: u32 = 0x1;

#[repr(C)]
pub struct SyncFile {
    pub file: *mut c_void,
}

unsafe extern "C" {
    fn drm_property_create_blob(dev: *mut DrmDevice, length: usize, data: *const c_void) -> *mut DrmPropertyBlob;
    fn drm_property_blob_put(blob: *mut DrmPropertyBlob);
    fn drm_property_blob_get(blob: *mut DrmPropertyBlob) -> *mut DrmPropertyBlob;
    fn drm_property_lookup_blob(dev: *mut DrmDevice, id: u32) -> *mut DrmPropertyBlob;
    fn drm_property_replace_blob_from_id(
        dev: *mut DrmDevice,
        blob: *mut *mut DrmPropertyBlob,
        blob_id: u64,
        expected_size: isize,
        expected_elem_size: isize,
        replaced: *mut bool,
    ) -> i32;
    fn drm_property_change_valid_get(
        prop: *mut DrmProperty,
        value: u64,
        ref_obj: *mut *mut DrmModeObject,
    ) -> bool;
    fn drm_property_change_valid_put(prop: *mut DrmProperty, ref_obj: *mut DrmModeObject);
    fn drm_object_immutable_property_get_value(
        obj: *mut DrmModeObject,
        prop: *mut DrmProperty,
        val: *mut u64,
    ) -> i32;
    fn drm_mode_convert_to_umode(out: *mut DrmModeModeinfo, in_: *const DrmDisplayMode);
    fn drm_mode_convert_umode(dev: *mut DrmDevice, out: *mut DrmDisplayMode, in_: *const DrmModeModeinfo) -> i32;
    fn drm_mode_object_find(dev: *mut DrmDevice, file_priv: *mut DrmFile, id: u32, obj_type: u32) -> *mut DrmModeObject;
    fn drm_mode_object_put(obj: *mut DrmModeObject);
    fn drm_framebuffer_lookup(dev: *mut DrmDevice, file_priv: *mut DrmFile, id: u32) -> *mut DrmFramebuffer;
    fn drm_writeback_set_fb(conn_state: *mut c_void, fb: *mut c_void) -> i32;
    fn drm_writeback_get_out_fence(wb_conn: *mut c_void) -> *mut DmaFence;
    fn drm_event_reserve_init(dev: *mut DrmDevice, file_priv: *mut DrmFile, p: *mut crate::drm_file_types::DrmPendingEvent, e: *mut DrmEvent) -> i32;
    fn drm_event_cancel_free(dev: *mut DrmDevice, p: *mut crate::drm_file_types::DrmPendingEvent);
    fn sync_file_create(fence: *mut DmaFence) -> *mut SyncFile;
    fn sync_file_get_fence(fd: i32) -> *mut DmaFence;
    fn dma_fence_put(fence: *mut DmaFence);
    fn get_unused_fd_flags(flags: c_int) -> c_int;
    fn put_unused_fd(fd: c_int);
    fn fd_install(fd: c_int, file: *mut c_void);
    fn fput(file: *mut c_void);
    fn krealloc(p: *mut c_void, new_size: usize, flags: u32) -> *mut c_void;
}

#[inline]
unsafe fn drm_crtc_index(crtc: *const DrmCrtc) -> usize {
    unsafe { (*crtc).index as usize }
}

#[inline]
unsafe fn drm_connector_index(connector: *const DrmConnector) -> usize {
    unsafe { (*connector).index as usize }
}

#[inline]
unsafe fn drm_connector_mask(connector: *const DrmConnector) -> u32 {
    1u32 << unsafe { (*connector).index }
}

#[inline]
unsafe fn drm_plane_mask(plane: *const DrmPlane) -> u32 {
    1u32 << unsafe { (*plane).index }
}

#[inline]
unsafe fn drm_framebuffer_get(fb: *mut DrmFramebuffer) {
    unsafe {
        if !fb.is_null() {
            crate::drm_mode_object_upstream::drm_mode_object_get(core::ptr::addr_of_mut!((*fb).base));
        }
    }
}

#[inline]
unsafe fn drm_framebuffer_put(fb: *mut DrmFramebuffer) {
    unsafe {
        if !fb.is_null() {
            crate::drm_mode_object_upstream::drm_mode_object_put(core::ptr::addr_of_mut!((*fb).base));
        }
    }
}

#[inline]
unsafe fn drm_connector_get(connector: *mut DrmConnector) {
    unsafe {
        if !connector.is_null() {
            crate::drm_mode_object_upstream::drm_mode_object_get(core::ptr::addr_of_mut!((*connector).base));
        }
    }
}

#[inline]
unsafe fn drm_connector_put(connector: *mut DrmConnector) {
    unsafe {
        if !connector.is_null() {
            crate::drm_mode_object_upstream::drm_mode_object_put(core::ptr::addr_of_mut!((*connector).base));
        }
    }
}

#[inline]
unsafe fn drm_framebuffer_assign(p: *mut *mut DrmFramebuffer, fb: *mut DrmFramebuffer) {
    unsafe {
        if !fb.is_null() {
            drm_framebuffer_get(fb);
        }
        if !(*p).is_null() {
            drm_framebuffer_put(*p);
        }
        *p = fb;
    }
}

#[inline]
fn is_power_of_2(x: u64) -> bool {
    x != 0 && (x & (x - 1)) == 0
}

#[inline]
unsafe fn drm_crtc_find(dev: *mut DrmDevice, file_priv: *mut DrmFile, id: u32) -> *mut DrmCrtc {
    unsafe {
        let mo = drm_mode_object_find(dev, file_priv, id, DRM_MODE_OBJECT_CRTC);
        mo as *mut DrmCrtc
    }
}

#[inline]
unsafe fn obj_to_connector(obj: *mut DrmModeObject) -> *mut DrmConnector {
    unsafe {
        (obj as *mut u8).sub(core::mem::offset_of!(DrmConnector, base)) as *mut DrmConnector
    }
}

#[inline]
unsafe fn obj_to_crtc(obj: *mut DrmModeObject) -> *mut DrmCrtc {
    unsafe {
        (obj as *mut u8).sub(core::mem::offset_of!(DrmCrtc, base)) as *mut DrmCrtc
    }
}

#[inline]
unsafe fn obj_to_plane(obj: *mut DrmModeObject) -> *mut DrmPlane {
    unsafe {
        (obj as *mut u8).sub(core::mem::offset_of!(DrmPlane, base)) as *mut DrmPlane
    }
}

#[inline]
unsafe fn get_user<T: Copy>(val: &mut T, ptr: *const T) -> bool {
    #[cfg(test)]
    {
        if ptr.is_null() {
            return false;
        }
        unsafe {
            *val = core::ptr::read(ptr);
        }
        return true;
    }
    #[cfg(not(test))]
    unsafe { tk_linux_kapi::mm_native::__get_user(val, ptr) == 0 }
}

#[inline]
unsafe fn put_user<T: Copy>(val: T, ptr: *mut T) -> bool {
    #[cfg(test)]
    {
        if ptr.is_null() {
            return false;
        }
        unsafe {
            core::ptr::write(ptr, val);
        }
        return true;
    }
    #[cfg(not(test))]
    unsafe { tk_linux_kapi::mm_native::__put_user(val, ptr) == 0 }
}

#[inline]
unsafe fn copy_from_user(dst: *mut c_void, src: *const c_void, size: usize) -> bool {
    #[cfg(test)]
    {
        if dst.is_null() || src.is_null() {
            return false;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(src as *const u8, dst as *mut u8, size);
        }
        return true;
    }
    #[cfg(not(test))]
    unsafe { tk_linux_kapi::mm_native::copy_from_user(dst, src, size) == 0 }
}

// upstream: drivers/gpu/drm/drm_atomic_uapi.c drm_atomic_set_mode_for_crtc()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_set_mode_for_crtc(
    state: *mut DrmCrtcState,
    mode: *const DrmDisplayMode,
) -> i32 {
    if state.is_null() {
        return -EINVAL;
    }
    let crtc = unsafe { (*state).crtc };

    if !mode.is_null() {
        let same = unsafe {
            core::slice::from_raw_parts(core::ptr::addr_of!((*state).mode) as *const u8, size_of::<DrmDisplayMode>())
                == core::slice::from_raw_parts(mode as *const u8, size_of::<DrmDisplayMode>())
        };
        if same {
            return 0;
        }
    }

    unsafe {
        if !(*state).mode_blob.is_null() {
            drm_property_blob_put((*state).mode_blob);
            (*state).mode_blob = core::ptr::null_mut();
        }

        if !mode.is_null() {
            let mut umode = core::mem::MaybeUninit::<DrmModeModeinfo>::zeroed();
            drm_mode_convert_to_umode(umode.as_mut_ptr(), mode);
            let blob = drm_property_create_blob((*crtc).dev, size_of::<DrmModeModeinfo>(), umode.as_ptr() as *const c_void);
            if IS_ERR(blob) {
                return PTR_ERR(blob);
            }

            core::ptr::copy_nonoverlapping(mode, core::ptr::addr_of_mut!((*state).mode), 1);
            (*state).mode_blob = blob;
            (*state).enable = true;
        } else {
            core::ptr::write_bytes(core::ptr::addr_of_mut!((*state).mode), 0, 1);
            (*state).enable = false;
        }
    }

    0
}

// upstream: drivers/gpu/drm/drm_atomic_uapi.c drm_atomic_set_mode_prop_for_crtc()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_set_mode_prop_for_crtc(
    state: *mut DrmCrtcState,
    blob: *mut DrmPropertyBlob,
) -> i32 {
    if state.is_null() {
        return -EINVAL;
    }
    let crtc = unsafe { (*state).crtc };

    if blob == unsafe { (*state).mode_blob } {
        return 0;
    }

    unsafe {
        if !(*state).mode_blob.is_null() {
            drm_property_blob_put((*state).mode_blob);
            (*state).mode_blob = core::ptr::null_mut();
        }

        core::ptr::write_bytes(core::ptr::addr_of_mut!((*state).mode), 0, 1);

        if !blob.is_null() {
            if (*blob).length != size_of::<DrmModeModeinfo>() {
                return -EINVAL;
            }

            let ret = drm_mode_convert_umode((*crtc).dev, core::ptr::addr_of_mut!((*state).mode), (*blob).data as *const DrmModeModeinfo);
            if ret != 0 {
                return -EINVAL;
            }

            (*state).mode_blob = drm_property_blob_get(blob);
            (*state).enable = true;
        } else {
            (*state).enable = false;
        }
    }

    0
}

// upstream: drivers/gpu/drm/drm_atomic_uapi.c drm_atomic_set_crtc_for_plane()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_set_crtc_for_plane(
    plane_state: *mut DrmPlaneState,
    crtc: *mut DrmCrtc,
) -> i32 {
    if plane_state.is_null() {
        return -EINVAL;
    }
    if unsafe { (*plane_state).crtc } == crtc {
        return 0;
    }

    let plane = unsafe { (*plane_state).plane };
    unsafe {
        if !(*plane_state).crtc.is_null() {
            let crtc_state = crate::drm_atomic_upstream::drm_atomic_get_crtc_state((*plane_state).state, (*plane_state).crtc);
            if IS_ERR(crtc_state) {
                return PTR_ERR(crtc_state);
            }
            (*crtc_state).plane_mask &= !drm_plane_mask(plane);
        }

        (*plane_state).crtc = crtc;

        if !crtc.is_null() {
            let crtc_state = crate::drm_atomic_upstream::drm_atomic_get_crtc_state((*plane_state).state, crtc);
            if IS_ERR(crtc_state) {
                return PTR_ERR(crtc_state);
            }
            (*crtc_state).plane_mask |= drm_plane_mask(plane);
        }
    }

    0
}

// upstream: drivers/gpu/drm/drm_atomic_uapi.c drm_atomic_set_fb_for_plane()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_set_fb_for_plane(
    plane_state: *mut DrmPlaneState,
    fb: *mut DrmFramebuffer,
) {
    if plane_state.is_null() {
        return;
    }
    unsafe {
        drm_framebuffer_assign(core::ptr::addr_of_mut!((*plane_state).fb), fb);
    }
}

// upstream: drivers/gpu/drm/drm_atomic_uapi.c drm_atomic_set_colorop_for_plane()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_set_colorop_for_plane(
    _plane_state: *mut DrmPlaneState,
    _colorop: *mut c_void,
) -> bool {
    false
}

// upstream: drivers/gpu/drm/drm_atomic_uapi.c drm_atomic_set_crtc_for_connector()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_set_crtc_for_connector(
    conn_state: *mut DrmConnectorState,
    crtc: *mut DrmCrtc,
) -> i32 {
    if conn_state.is_null() {
        return -EINVAL;
    }
    if unsafe { (*conn_state).crtc } == crtc {
        return 0;
    }

    let connector = unsafe { (*conn_state).connector };

    unsafe {
        if !(*conn_state).crtc.is_null() {
            let crtc_state = crate::drm_atomic_upstream::drm_atomic_get_new_crtc_state((*conn_state).state, (*conn_state).crtc);
            if !crtc_state.is_null() {
                (*crtc_state).connector_mask &= !drm_connector_mask(connector);
            }
            drm_connector_put(connector);
            (*conn_state).crtc = core::ptr::null_mut();
        }

        if !crtc.is_null() {
            let crtc_state = crate::drm_atomic_upstream::drm_atomic_get_crtc_state((*conn_state).state, crtc);
            if IS_ERR(crtc_state) {
                return PTR_ERR(crtc_state);
            }

            (*crtc_state).connector_mask |= drm_connector_mask(connector);
            drm_connector_get(connector);
            (*conn_state).crtc = crtc;
        }
    }

    0
}

// upstream: drivers/gpu/drm/drm_atomic_uapi.c set_out_fence_for_crtc()
unsafe fn set_out_fence_for_crtc(
    state: *mut DrmAtomicCommit,
    crtc: *mut DrmCrtc,
    fence_ptr: *mut i32,
) {
    let index = unsafe { drm_crtc_index(crtc) };
    unsafe {
        (*(*state).crtcs.add(index)).out_fence_ptr = fence_ptr;
    }
}

// upstream: drivers/gpu/drm/drm_atomic_uapi.c get_out_fence_for_crtc()
unsafe fn get_out_fence_for_crtc(
    state: *mut DrmAtomicCommit,
    crtc: *mut DrmCrtc,
) -> *mut i32 {
    let index = unsafe { drm_crtc_index(crtc) };
    unsafe {
        let fence_ptr = (*(*state).crtcs.add(index)).out_fence_ptr;
        (*(*state).crtcs.add(index)).out_fence_ptr = core::ptr::null_mut();
        fence_ptr
    }
}

// upstream: drivers/gpu/drm/drm_atomic_uapi.c set_out_fence_for_connector()
unsafe fn set_out_fence_for_connector(
    state: *mut DrmAtomicCommit,
    connector: *mut DrmConnector,
    fence_ptr: *mut i32,
) -> i32 {
    let index = unsafe { drm_connector_index(connector) };
    if fence_ptr.is_null() {
        return 0;
    }
    if !unsafe { put_user(-1i32, fence_ptr) } {
        return -EFAULT;
    }
    unsafe {
        (*(*state).connectors.add(index)).out_fence_ptr = fence_ptr;
    }
    0
}

// upstream: drivers/gpu/drm/drm_atomic_uapi.c get_out_fence_for_connector()
unsafe fn get_out_fence_for_connector(
    state: *mut DrmAtomicCommit,
    connector: *mut DrmConnector,
) -> *mut i32 {
    let index = unsafe { drm_connector_index(connector) };
    unsafe {
        let fence_ptr = (*(*state).connectors.add(index)).out_fence_ptr;
        (*(*state).connectors.add(index)).out_fence_ptr = core::ptr::null_mut();
        fence_ptr
    }
}

// upstream: drivers/gpu/drm/drm_atomic_uapi.c drm_atomic_crtc_set_property()
unsafe fn drm_atomic_crtc_set_property(
    crtc: *mut DrmCrtc,
    state: *mut DrmCrtcState,
    property: *mut DrmProperty,
    val: u64,
) -> i32 {
    let dev = unsafe { (*crtc).dev };
    let config = unsafe { core::ptr::addr_of_mut!((*dev).mode_config) };
    let mut replaced = false;

    if property == unsafe { (*config).prop_active } {
        unsafe { (*state).active = val != 0 };
    } else if property == unsafe { (*config).prop_mode_id } {
        let mode = unsafe { drm_property_lookup_blob(dev, val as u32) };
        let ret = unsafe { drm_atomic_set_mode_prop_for_crtc(state, mode) };
        unsafe { drm_property_blob_put(mode); }
        return ret;
    } else if property == unsafe { (*config).prop_vrr_enabled } {
        unsafe { (*state).vrr_enabled = val != 0 };
    } else if property == unsafe { (*config).degamma_lut_property } {
        let elem_size = size_of::<DrmColorLut>();
        let mut lut_size: u64 = 0;
        let ret = unsafe {
            drm_object_immutable_property_get_value(
                core::ptr::addr_of_mut!((*crtc).base),
                (*config).degamma_lut_size_property,
                &mut lut_size,
            )
        };
        if ret != 0 {
            return ret;
        }

        let ret = unsafe {
            drm_property_replace_blob_from_id(
                dev,
                core::ptr::addr_of_mut!((*state).degamma_lut),
                val,
                (elem_size * lut_size as usize) as isize,
                elem_size as isize,
                &mut replaced,
            )
        };
        unsafe { (*state).set_color_mgmt_changed((*state).color_mgmt_changed() || replaced) };
        return ret;
    } else if property == unsafe { (*config).ctm_property } {
        let ret = unsafe {
            drm_property_replace_blob_from_id(
                dev,
                core::ptr::addr_of_mut!((*state).ctm),
                val,
                40, // sizeof(struct drm_color_ctm)
                -1,
                &mut replaced,
            )
        };
        unsafe { (*state).set_color_mgmt_changed((*state).color_mgmt_changed() || replaced) };
        return ret;
    } else if property == unsafe { (*config).gamma_lut_property } {
        let elem_size = size_of::<DrmColorLut>();
        let mut lut_size: u64 = 0;
        let ret = unsafe {
            drm_object_immutable_property_get_value(
                core::ptr::addr_of_mut!((*crtc).base),
                (*config).gamma_lut_size_property,
                &mut lut_size,
            )
        };
        if ret != 0 {
            return ret;
        }

        let ret = unsafe {
            drm_property_replace_blob_from_id(
                dev,
                core::ptr::addr_of_mut!((*state).gamma_lut),
                val,
                (elem_size * lut_size as usize) as isize,
                elem_size as isize,
                &mut replaced,
            )
        };
        unsafe { (*state).set_color_mgmt_changed((*state).color_mgmt_changed() || replaced) };
        return ret;
    } else if property == unsafe { (*config).background_color_property } {
        unsafe { (*state).background_color = val };
    } else if property == unsafe { (*config).prop_out_fence_ptr } {
        let fence_ptr = val as *mut i32;
        if fence_ptr.is_null() {
            return 0;
        }
        if !unsafe { put_user(-1i32, fence_ptr) } {
            return -EFAULT;
        }
        unsafe { set_out_fence_for_crtc((*state).state, crtc, fence_ptr) };
    } else if property == unsafe { (*crtc).scaling_filter_property } {
        unsafe { (*state).scaling_filter = val as u32 };
    } else {
        let funcs = unsafe { (*crtc).funcs };
        if !funcs.is_null() {
            if let Some(set_fn) = unsafe { (*funcs).atomic_set_property } {
                return unsafe { set_fn(crtc, state, property, val) };
            }
        }
        return -EINVAL;
    }

    0
}

// upstream: drivers/gpu/drm/drm_atomic_uapi.c drm_atomic_crtc_get_property()
unsafe fn drm_atomic_crtc_get_property(
    crtc: *mut DrmCrtc,
    state: *const DrmCrtcState,
    property: *mut DrmProperty,
    val: *mut u64,
) -> i32 {
    let dev = unsafe { (*crtc).dev };
    let config = unsafe { core::ptr::addr_of_mut!((*dev).mode_config) };

    if property == unsafe { (*config).prop_active } {
        unsafe { *val = crate::drm_atomic_state_helper_upstream::drm_atomic_crtc_effectively_active(state) as u64 };
    } else if property == unsafe { (*config).prop_mode_id } {
        unsafe { *val = if !(*state).mode_blob.is_null() { (*(*state).mode_blob).base.id as u64 } else { 0 } };
    } else if property == unsafe { (*config).prop_vrr_enabled } {
        unsafe { *val = (*state).vrr_enabled as u64 };
    } else if property == unsafe { (*config).degamma_lut_property } {
        unsafe { *val = if !(*state).degamma_lut.is_null() { (*(*state).degamma_lut).base.id as u64 } else { 0 } };
    } else if property == unsafe { (*config).ctm_property } {
        unsafe { *val = if !(*state).ctm.is_null() { (*(*state).ctm).base.id as u64 } else { 0 } };
    } else if property == unsafe { (*config).gamma_lut_property } {
        unsafe { *val = if !(*state).gamma_lut.is_null() { (*(*state).gamma_lut).base.id as u64 } else { 0 } };
    } else if property == unsafe { (*config).background_color_property } {
        unsafe { *val = (*state).background_color };
    } else if property == unsafe { (*config).prop_out_fence_ptr } {
        unsafe { *val = 0 };
    } else if property == unsafe { (*crtc).scaling_filter_property } {
        unsafe { *val = (*state).scaling_filter as u64 };
    } else {
        let funcs = unsafe { (*crtc).funcs };
        if !funcs.is_null() {
            if let Some(get_fn) = unsafe { (*funcs).atomic_get_property } {
                return unsafe { get_fn(crtc, state as *mut DrmCrtcState, property, val) };
            }
        }
        return -EINVAL;
    }

    0
}

// upstream: drivers/gpu/drm/drm_atomic_uapi.c drm_atomic_plane_set_property()
unsafe fn drm_atomic_plane_set_property(
    plane: *mut DrmPlane,
    state: *mut DrmPlaneState,
    file_priv: *mut DrmFile,
    property: *mut DrmProperty,
    val: u64,
) -> i32 {
    let dev = unsafe { (*plane).dev };
    let config = unsafe { core::ptr::addr_of_mut!((*dev).mode_config) };
    let mut replaced = false;

    if property == unsafe { (*config).prop_fb_id } {
        let fb = unsafe { drm_framebuffer_lookup(dev, file_priv, val as u32) };
        unsafe { drm_atomic_set_fb_for_plane(state, fb); }
        if !fb.is_null() {
            unsafe { drm_framebuffer_put(fb); }
        }
    } else if property == unsafe { (*config).prop_in_fence_fd } {
        if !unsafe { (*state).fence.is_null() } {
            return -EINVAL;
        }
        if val as i64 == -1 {
            return 0;
        }
        unsafe {
            (*state).fence = sync_file_get_fence(val as i32);
            if (*state).fence.is_null() {
                return -EINVAL;
            }
        }
    } else if property == unsafe { (*config).prop_crtc_id } {
        let crtc = if val != 0 {
            let c = unsafe { drm_crtc_find(dev, file_priv, val as u32) };
            if c.is_null() {
                return -EACCES;
            }
            c
        } else {
            core::ptr::null_mut()
        };
        return unsafe { drm_atomic_set_crtc_for_plane(state, crtc) };
    } else if property == unsafe { (*config).prop_crtc_x } {
        unsafe { (*state).crtc_x = val as i64 as i32 };
    } else if property == unsafe { (*config).prop_crtc_y } {
        unsafe { (*state).crtc_y = val as i64 as i32 };
    } else if property == unsafe { (*config).prop_crtc_w } {
        unsafe { (*state).crtc_w = val as u32 };
    } else if property == unsafe { (*config).prop_crtc_h } {
        unsafe { (*state).crtc_h = val as u32 };
    } else if property == unsafe { (*config).prop_src_x } {
        unsafe { (*state).src_x = val as u32 };
    } else if property == unsafe { (*config).prop_src_y } {
        unsafe { (*state).src_y = val as u32 };
    } else if property == unsafe { (*config).prop_src_w } {
        unsafe { (*state).src_w = val as u32 };
    } else if property == unsafe { (*config).prop_src_h } {
        unsafe { (*state).src_h = val as u32 };
    } else if property == unsafe { (*plane).alpha_property } {
        unsafe { (*state).alpha = val as u16 };
    } else if property == unsafe { (*plane).blend_mode_property } {
        unsafe { (*state).pixel_blend_mode = val as u16 };
    } else if property == unsafe { (*plane).rotation_property } {
        if !is_power_of_2(val & DRM_MODE_ROTATE_MASK as u64) {
            return -EINVAL;
        }
        unsafe { (*state).rotation = val as u32 };
    } else if property == unsafe { (*plane).zpos_property } {
        unsafe { (*state).zpos = val as u32 };
    } else if property == unsafe { (*plane).color_encoding_property } {
        unsafe { (*state).color_encoding = val as u32 };
    } else if property == unsafe { (*plane).color_range_property } {
        unsafe { (*state).color_range = val as u32 };
    } else if property == unsafe { (*config).prop_fb_damage_clips } {
        let ret = unsafe {
            drm_property_replace_blob_from_id(
                dev,
                core::ptr::addr_of_mut!((*state).fb_damage_clips),
                val,
                -1,
                size_of::<DrmModeRect>() as isize,
                &mut replaced,
            )
        };
        return ret;
    } else if property == unsafe { (*plane).scaling_filter_property } {
        unsafe { (*state).scaling_filter = val as u32 };
    } else if property == unsafe { (*plane).hotspot_x_property } {
        if unsafe { (*plane).r#type != DRM_PLANE_TYPE_CURSOR } {
            return -EINVAL;
        }
        unsafe { (*state).hotspot_x = val as i32 };
    } else if property == unsafe { (*plane).hotspot_y_property } {
        if unsafe { (*plane).r#type != DRM_PLANE_TYPE_CURSOR } {
            return -EINVAL;
        }
        unsafe { (*state).hotspot_y = val as i32 };
    } else {
        let funcs = unsafe { (*plane).funcs };
        if !funcs.is_null() {
            if let Some(set_fn) = unsafe { (*funcs).atomic_set_property } {
                return unsafe { set_fn(plane, state, property, val) };
            }
        }
        return -EINVAL;
    }

    0
}

// upstream: drivers/gpu/drm/drm_atomic_uapi.c drm_atomic_plane_get_property()
unsafe fn drm_atomic_plane_get_property(
    plane: *mut DrmPlane,
    state: *const DrmPlaneState,
    property: *mut DrmProperty,
    val: *mut u64,
) -> i32 {
    let dev = unsafe { (*plane).dev };
    let config = unsafe { core::ptr::addr_of_mut!((*dev).mode_config) };

    if property == unsafe { (*config).prop_fb_id } {
        unsafe { *val = if !(*state).fb.is_null() { (*(*state).fb).base.id as u64 } else { 0 } };
    } else if property == unsafe { (*config).prop_in_fence_fd } {
        unsafe { *val = !0u64 };
    } else if property == unsafe { (*config).prop_crtc_id } {
        unsafe { *val = if !(*state).crtc.is_null() { (*(*state).crtc).base.id as u64 } else { 0 } };
    } else if property == unsafe { (*config).prop_crtc_x } {
        unsafe { *val = (*state).crtc_x as i64 as u64 };
    } else if property == unsafe { (*config).prop_crtc_y } {
        unsafe { *val = (*state).crtc_y as i64 as u64 };
    } else if property == unsafe { (*config).prop_crtc_w } {
        unsafe { *val = (*state).crtc_w as u64 };
    } else if property == unsafe { (*config).prop_crtc_h } {
        unsafe { *val = (*state).crtc_h as u64 };
    } else if property == unsafe { (*config).prop_src_x } {
        unsafe { *val = (*state).src_x as u64 };
    } else if property == unsafe { (*config).prop_src_y } {
        unsafe { *val = (*state).src_y as u64 };
    } else if property == unsafe { (*config).prop_src_w } {
        unsafe { *val = (*state).src_w as u64 };
    } else if property == unsafe { (*config).prop_src_h } {
        unsafe { *val = (*state).src_h as u64 };
    } else if property == unsafe { (*plane).alpha_property } {
        unsafe { *val = (*state).alpha as u64 };
    } else if property == unsafe { (*plane).blend_mode_property } {
        unsafe { *val = (*state).pixel_blend_mode as u64 };
    } else if property == unsafe { (*plane).rotation_property } {
        unsafe { *val = (*state).rotation as u64 };
    } else if property == unsafe { (*plane).zpos_property } {
        unsafe { *val = (*state).zpos as u64 };
    } else if property == unsafe { (*plane).color_encoding_property } {
        unsafe { *val = (*state).color_encoding as u64 };
    } else if property == unsafe { (*plane).color_range_property } {
        unsafe { *val = (*state).color_range as u64 };
    } else if property == unsafe { (*config).prop_fb_damage_clips } {
        unsafe { *val = if !(*state).fb_damage_clips.is_null() { (*(*state).fb_damage_clips).base.id as u64 } else { 0 } };
    } else if property == unsafe { (*plane).scaling_filter_property } {
        unsafe { *val = (*state).scaling_filter as u64 };
    } else if property == unsafe { (*plane).hotspot_x_property } {
        unsafe { *val = (*state).hotspot_x as i64 as u64 };
    } else if property == unsafe { (*plane).hotspot_y_property } {
        unsafe { *val = (*state).hotspot_y as i64 as u64 };
    } else {
        let funcs = unsafe { (*plane).funcs };
        if !funcs.is_null() {
            if let Some(get_fn) = unsafe { (*funcs).atomic_get_property } {
                return unsafe { get_fn(plane, state as *mut DrmPlaneState, property, val) };
            }
        }
        return -EINVAL;
    }

    0
}

// upstream: drivers/gpu/drm/drm_atomic_uapi.c drm_atomic_color_set_data_property()
unsafe fn drm_atomic_color_set_data_property(
    _colorop: *mut c_void,
    _state: *mut c_void,
    _property: *mut DrmProperty,
    _val: u64,
    _replaced: *mut bool,
) -> i32 {
    -EINVAL
}

// upstream: drivers/gpu/drm/drm_atomic_uapi.c drm_atomic_colorop_set_property()
unsafe fn drm_atomic_colorop_set_property(
    _colorop: *mut c_void,
    _state: *mut c_void,
    _file_priv: *mut DrmFile,
    _property: *mut DrmProperty,
    _val: u64,
    _replaced: *mut bool,
) -> i32 {
    -EINVAL
}

// upstream: drivers/gpu/drm/drm_atomic_uapi.c drm_atomic_colorop_get_property()
unsafe fn drm_atomic_colorop_get_property(
    _colorop: *mut c_void,
    _state: *const c_void,
    _property: *mut DrmProperty,
    _val: *mut u64,
) -> i32 {
    -EINVAL
}

// upstream: drivers/gpu/drm/drm_atomic_uapi.c drm_atomic_set_writeback_fb_for_connector()
unsafe fn drm_atomic_set_writeback_fb_for_connector(
    conn_state: *mut DrmConnectorState,
    fb: *mut DrmFramebuffer,
) -> i32 {
    unsafe { drm_writeback_set_fb(conn_state as *mut c_void, fb as *mut c_void) }
}

// upstream: drivers/gpu/drm/drm_atomic_uapi.c drm_atomic_connector_set_property()
unsafe fn drm_atomic_connector_set_property(
    connector: *mut DrmConnector,
    state: *mut DrmConnectorState,
    file_priv: *mut DrmFile,
    property: *mut DrmProperty,
    val: u64,
) -> i32 {
    let dev = unsafe { (*connector).dev };
    let config = unsafe { core::ptr::addr_of_mut!((*dev).mode_config) };
    let mut replaced = false;

    if property == unsafe { (*config).prop_crtc_id } {
        let crtc = if val != 0 {
            let c = unsafe { drm_crtc_find(dev, file_priv, val as u32) };
            if c.is_null() {
                return -EACCES;
            }
            c
        } else {
            core::ptr::null_mut()
        };
        return unsafe { drm_atomic_set_crtc_for_connector(state, crtc) };
    } else if property == unsafe { (*config).dpms_property } {
        return -EINVAL;
    } else if property == unsafe { (*config).link_status_property } {
        if unsafe { (*state).link_status != DRM_LINK_STATUS_GOOD as u32 } {
            unsafe { (*state).link_status = val as u32 };
        }
    } else if property == unsafe { (*config).hdr_output_metadata_property } {
        let ret = unsafe {
            drm_property_replace_blob_from_id(
                dev,
                core::ptr::addr_of_mut!((*state).hdr_output_metadata),
                val,
                -1,
                -1,
                &mut replaced,
            )
        };
        return ret;
    } else if property == unsafe { (*config).aspect_ratio_property } {
        unsafe { (*state).picture_aspect_ratio = val as u32 };
    } else if property == unsafe { (*config).content_type_property } {
        unsafe { (*state).content_type = val as u32 };
    } else if property == unsafe { (*connector).scaling_mode_property } {
        unsafe { (*state).scaling_mode = val as u32 };
    } else if property == unsafe { (*config).content_protection_property } {
        if val == DRM_MODE_CONTENT_PROTECTION_ENABLED as u64 {
            return -EINVAL;
        }
        unsafe { (*state).content_protection = val as u32 };
    } else if property == unsafe { (*config).hdcp_content_type_property } {
        unsafe { (*state).hdcp_content_type = val as u32 };
    } else if property == unsafe { (*connector).colorspace_property } {
        unsafe { (*state).colorspace = val as u32 };
    } else if property == unsafe { (*config).writeback_fb_id_property } {
        let fb = unsafe { drm_framebuffer_lookup(dev, file_priv, val as u32) };
        let ret = unsafe { drm_atomic_set_writeback_fb_for_connector(state, fb) };
        if !fb.is_null() {
            unsafe { drm_framebuffer_put(fb); }
        }
        return ret;
    } else if property == unsafe { (*config).writeback_out_fence_ptr_property } {
        let fence_ptr = val as *mut i32;
        return unsafe { set_out_fence_for_connector((*state).state, connector, fence_ptr) };
    } else if property == unsafe { (*connector).max_bpc_property } {
        unsafe { (*state).max_requested_bpc = val as u8 };
    } else if property == unsafe { (*connector).privacy_screen_sw_state_property } {
        unsafe { (*state).privacy_screen_sw_state = val as u32 };
    } else if property == unsafe { (*connector).broadcast_rgb_property } {
        unsafe { (*state).hdmi.broadcast_rgb = val as u32 };
    } else {
        let funcs = unsafe { (*connector).funcs };
        if !funcs.is_null() {
            if let Some(set_fn) = unsafe { (*funcs).atomic_set_property } {
                return unsafe { set_fn(connector, state, property, val) };
            }
        }
        return -EINVAL;
    }

    0
}

// upstream: drivers/gpu/drm/drm_atomic_uapi.c drm_atomic_connector_get_property()
unsafe fn drm_atomic_connector_get_property(
    connector: *mut DrmConnector,
    state: *const DrmConnectorState,
    property: *mut DrmProperty,
    val: *mut u64,
) -> i32 {
    let dev = unsafe { (*connector).dev };
    let config = unsafe { core::ptr::addr_of_mut!((*dev).mode_config) };

    if property == unsafe { (*config).prop_crtc_id } {
        unsafe { *val = if !(*state).crtc.is_null() { (*(*state).crtc).base.id as u64 } else { 0 } };
    } else if property == unsafe { (*config).dpms_property } {
        unsafe {
            if !(*state).crtc.is_null() && !(*(*state).crtc).state.is_null() && (*(*(*state).crtc).state).self_refresh_active {
                *val = DRM_MODE_DPMS_ON as u64;
            } else {
                *val = (*connector).dpms as u64;
            }
        }
    } else if property == unsafe { (*config).link_status_property } {
        unsafe { *val = (*state).link_status as u64 };
    } else if property == unsafe { (*config).aspect_ratio_property } {
        unsafe { *val = (*state).picture_aspect_ratio as u64 };
    } else if property == unsafe { (*config).content_type_property } {
        unsafe { *val = (*state).content_type as u64 };
    } else if property == unsafe { (*connector).colorspace_property } {
        unsafe { *val = (*state).colorspace as u64 };
    } else if property == unsafe { (*connector).scaling_mode_property } {
        unsafe { *val = (*state).scaling_mode as u64 };
    } else if property == unsafe { (*config).hdr_output_metadata_property } {
        unsafe { *val = if !(*state).hdr_output_metadata.is_null() { (*(*state).hdr_output_metadata).base.id as u64 } else { 0 } };
    } else if property == unsafe { (*config).content_protection_property } {
        unsafe { *val = (*state).content_protection as u64 };
    } else if property == unsafe { (*config).hdcp_content_type_property } {
        unsafe { *val = (*state).hdcp_content_type as u64 };
    } else if property == unsafe { (*config).writeback_fb_id_property } {
        unsafe { *val = 0 };
    } else if property == unsafe { (*config).writeback_out_fence_ptr_property } {
        unsafe { *val = 0 };
    } else if property == unsafe { (*connector).max_bpc_property } {
        unsafe { *val = (*state).max_requested_bpc as u64 };
    } else if property == unsafe { (*connector).privacy_screen_sw_state_property } {
        unsafe { *val = (*state).privacy_screen_sw_state as u64 };
    } else if property == unsafe { (*connector).broadcast_rgb_property } {
        unsafe { *val = (*state).hdmi.broadcast_rgb as u64 };
    } else {
        let funcs = unsafe { (*connector).funcs };
        if !funcs.is_null() {
            if let Some(get_fn) = unsafe { (*funcs).atomic_get_property } {
                return unsafe { get_fn(connector, state as *mut DrmConnectorState, property, val) };
            }
        }
        return -EINVAL;
    }

    0
}

// upstream: drivers/gpu/drm/drm_atomic_uapi.c drm_atomic_get_property()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_get_property(
    obj: *mut DrmModeObject,
    property: *mut DrmProperty,
    val: *mut u64,
) -> i32 {
    if obj.is_null() || property.is_null() || val.is_null() {
        return -EINVAL;
    }

    match unsafe { (*obj).r#type } {
        DRM_MODE_OBJECT_CONNECTOR => {
            let connector = unsafe { obj_to_connector(obj) };
            unsafe { drm_atomic_connector_get_property(connector, (*connector).state, property, val) }
        }
        DRM_MODE_OBJECT_CRTC => {
            let crtc = unsafe { obj_to_crtc(obj) };
            unsafe { drm_atomic_crtc_get_property(crtc, (*crtc).state, property, val) }
        }
        DRM_MODE_OBJECT_PLANE => {
            let plane = unsafe { obj_to_plane(obj) };
            unsafe { drm_atomic_plane_get_property(plane, (*plane).state, property, val) }
        }
        _ => -EINVAL,
    }
}

// upstream: drivers/gpu/drm/drm_atomic_uapi.c create_vblank_event()
unsafe fn create_vblank_event(
    crtc: *mut DrmCrtc,
    user_data: u64,
) -> *mut DrmPendingVblankEvent {
    let e = unsafe { kzalloc(size_of::<DrmPendingVblankEvent>(), GFP_KERNEL) as *mut DrmPendingVblankEvent };
    if e.is_null() {
        return core::ptr::null_mut();
    }

    unsafe {
        let base_ptr = core::ptr::addr_of_mut!((*e).event.base) as *mut DrmEvent;
        let vbl_ptr = core::ptr::addr_of_mut!((*e).event.vbl) as *mut DrmEventVblank;
        (*base_ptr).r#type = DRM_EVENT_FLIP_COMPLETE;
        (*base_ptr).length = size_of::<DrmEventVblank>() as u32;
        (*vbl_ptr).crtc_id = (*crtc).base.id;
        (*vbl_ptr).user_data = user_data;
    }

    e
}

// upstream: drivers/gpu/drm/drm_atomic_uapi.c drm_atomic_connector_commit_dpms()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_connector_commit_dpms(
    state: *mut DrmAtomicCommit,
    connector: *mut DrmConnector,
    mode: i32,
) -> i32 {
    if state.is_null() || connector.is_null() {
        return -EINVAL;
    }

    let dev = unsafe { (*state).dev };
    let config = unsafe { core::ptr::addr_of_mut!((*dev).mode_config) };
    let ret = unsafe { drm_modeset_lock(core::ptr::addr_of_mut!((*config).connection_mutex), (*state).acquire_ctx) };
    if ret != 0 {
        return ret;
    }

    let target_mode = if mode == DRM_MODE_DPMS_ON as i32 { DRM_MODE_DPMS_ON as i32 } else { DRM_MODE_DPMS_OFF as i32 };
    let old_mode = unsafe { (*connector).dpms };
    if old_mode == target_mode {
        return 0;
    }

    unsafe { (*connector).dpms = target_mode };

    let crtc = unsafe {
        if !(*connector).state.is_null() {
            (*(*connector).state).crtc
        } else {
            core::ptr::null_mut()
        }
    };

    if crtc.is_null() {
        return 0;
    }

    let ret = unsafe { crate::drm_atomic_upstream::drm_atomic_add_affected_connectors(state, crtc) };
    if ret != 0 {
        unsafe { (*connector).dpms = old_mode };
        return ret;
    }

    let crtc_state = unsafe { crate::drm_atomic_upstream::drm_atomic_get_crtc_state(state, crtc) };
    if IS_ERR(crtc_state) {
        unsafe { (*connector).dpms = old_mode };
        return PTR_ERR(crtc_state);
    }

    let mut active = false;
    unsafe {
        let num_conns = (*state).num_connector as usize;
        for i in 0..num_conns {
            let entry = (*state).connectors.add(i);
            if (*entry).ptr.is_null() {
                continue;
            }
            if (*(*entry).new_state).crtc != crtc {
                continue;
            }
            if (*(*entry).ptr).dpms == DRM_MODE_DPMS_ON as i32 {
                active = true;
                break;
            }
        }

        (*crtc_state).active = active;
    }

    let ret = unsafe { crate::drm_atomic_upstream::drm_atomic_commit(state) };
    if ret != 0 {
        unsafe { (*connector).dpms = old_mode };
    }

    ret
}

// upstream: drivers/gpu/drm/drm_atomic_uapi.c drm_atomic_check_prop_changes()
unsafe fn drm_atomic_check_prop_changes(
    ret: i32,
    old_val: u64,
    prop_value: u64,
    _prop: *mut DrmProperty,
) -> i32 {
    if ret != 0 || old_val != prop_value {
        return -EINVAL;
    }
    0
}

// upstream: drivers/gpu/drm/drm_atomic_uapi.c drm_atomic_set_property()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_set_property(
    state: *mut DrmAtomicCommit,
    file_priv: *mut DrmFile,
    obj: *mut DrmModeObject,
    prop: *mut DrmProperty,
    prop_value: u64,
    async_flip: bool,
) -> i32 {
    if state.is_null() || obj.is_null() || prop.is_null() {
        return -EINVAL;
    }

    let mut ref_obj: *mut DrmModeObject = core::ptr::null_mut();
    if !unsafe { drm_property_change_valid_get(prop, prop_value, &mut ref_obj) } {
        return -EINVAL;
    }

    let mut ret = -EINVAL;
    let mut old_val: u64 = 0;

    match unsafe { (*obj).r#type } {
        DRM_MODE_OBJECT_CONNECTOR => {
            let connector = unsafe { obj_to_connector(obj) };
            let connector_state = unsafe { crate::drm_atomic_upstream::drm_atomic_get_connector_state(state, connector) };
            if IS_ERR(connector_state) {
                ret = PTR_ERR(connector_state);
            } else if async_flip {
                let get_ret = unsafe { drm_atomic_connector_get_property(connector, connector_state, prop, &mut old_val) };
                ret = unsafe { drm_atomic_check_prop_changes(get_ret, old_val, prop_value, prop) };
            } else {
                ret = unsafe { drm_atomic_connector_set_property(connector, connector_state, file_priv, prop, prop_value) };
            }
        }
        DRM_MODE_OBJECT_CRTC => {
            let crtc = unsafe { obj_to_crtc(obj) };
            let crtc_state = unsafe { crate::drm_atomic_upstream::drm_atomic_get_crtc_state(state, crtc) };
            if IS_ERR(crtc_state) {
                ret = PTR_ERR(crtc_state);
            } else if async_flip {
                let get_ret = unsafe { drm_atomic_crtc_get_property(crtc, crtc_state, prop, &mut old_val) };
                ret = unsafe { drm_atomic_check_prop_changes(get_ret, old_val, prop_value, prop) };
            } else {
                ret = unsafe { drm_atomic_crtc_set_property(crtc, crtc_state, prop, prop_value) };
            }
        }
        DRM_MODE_OBJECT_PLANE => {
            let plane = unsafe { obj_to_plane(obj) };
            let plane_state = unsafe { crate::drm_atomic_upstream::drm_atomic_get_plane_state(state, plane) };
            if IS_ERR(plane_state) {
                ret = PTR_ERR(plane_state);
            } else if async_flip {
                let get_ret = unsafe { drm_atomic_plane_get_property(plane, plane_state, prop, &mut old_val) };
                ret = unsafe { drm_atomic_check_prop_changes(get_ret, old_val, prop_value, prop) };
                if ret != 0 {
                    let dev = unsafe { (*plane).dev };
                    let config = unsafe { core::ptr::addr_of_mut!((*dev).mode_config) };
                    if prop == unsafe { (*config).prop_fb_id }
                        || prop == unsafe { (*config).prop_in_fence_fd }
                        || prop == unsafe { (*config).prop_fb_damage_clips }
                    {
                        ret = unsafe { drm_atomic_plane_set_property(plane, plane_state, file_priv, prop, prop_value) };
                    }
                }
            } else {
                ret = unsafe { drm_atomic_plane_set_property(plane, plane_state, file_priv, prop, prop_value) };
            }
        }
        _ => {
            ret = -EINVAL;
        }
    }

    unsafe { drm_property_change_valid_put(prop, ref_obj); }
    ret
}

#[repr(C)]
struct DrmOutFenceState {
    out_fence_ptr: *mut i32,
    sync_file: *mut SyncFile,
    fd: i32,
}

// upstream: drivers/gpu/drm/drm_atomic_uapi.c setup_out_fence()
unsafe fn setup_out_fence(fence_state: *mut DrmOutFenceState, fence: *mut DmaFence) -> i32 {
    let fd = unsafe { get_unused_fd_flags(O_CLOEXEC as c_int) };
    if fd < 0 {
        return fd;
    }
    unsafe {
        (*fence_state).fd = fd;
        if !put_user(fd, (*fence_state).out_fence_ptr) {
            return -EFAULT;
        }

        let sync_file = sync_file_create(fence);
        if sync_file.is_null() {
            return -ENOMEM;
        }
        (*fence_state).sync_file = sync_file;
    }
    0
}

// upstream: drivers/gpu/drm/drm_atomic_uapi.c prepare_signaling()
unsafe fn prepare_signaling(
    dev: *mut DrmDevice,
    state: *mut DrmAtomicCommit,
    arg: *mut DrmModeAtomic,
    file_priv: *mut DrmFile,
    fence_state: *mut *mut DrmOutFenceState,
    num_fences: *mut u32,
) -> i32 {
    if (unsafe { (*arg).flags } & DRM_MODE_ATOMIC_TEST_ONLY) != 0 {
        return 0;
    }

    let mut c = 0;
    let config = unsafe { core::ptr::addr_of_mut!((*dev).mode_config) };

    unsafe {
        let num_crtcs = (*config).num_crtc as usize;
        for i in 0..num_crtcs {
            if (*state).crtcs.is_null() {
                break;
            }
            let entry = (*state).crtcs.add(i);
            let crtc = (*entry).ptr;
            if crtc.is_null() {
                continue;
            }
            let crtc_state = (*entry).new_state;
            let fence_ptr = get_out_fence_for_crtc(state, crtc);

            if ((*arg).flags & DRM_MODE_PAGE_FLIP_EVENT != 0) || !fence_ptr.is_null() {
                let e = create_vblank_event(crtc, (*arg).user_data);
                if e.is_null() {
                    return -ENOMEM;
                }
                (*crtc_state).event = e;
            }

            if ((*arg).flags & DRM_MODE_PAGE_FLIP_EVENT) != 0 {
                let e = (*crtc_state).event;
                if !file_priv.is_null() {
                    let ret = drm_event_reserve_init(
                        dev,
                        file_priv,
                        core::ptr::addr_of_mut!((*e).base),
                        core::ptr::addr_of_mut!((*e).event.base) as *mut DrmEvent,
                    );
                    if ret != 0 {
                        kfree(e);
                        (*crtc_state).event = core::ptr::null_mut();
                        return ret;
                    }
                }
            }

            if !fence_ptr.is_null() {
                let cur_count = *num_fences as usize;
                let new_count = cur_count + 1;
                let f = krealloc(*fence_state as *mut c_void, size_of::<DrmOutFenceState>() * new_count, GFP_KERNEL) as *mut DrmOutFenceState;
                if f.is_null() {
                    return -ENOMEM;
                }
                *fence_state = f;
                core::ptr::write_bytes(f.add(cur_count), 0, 1);
                (*f.add(cur_count)).out_fence_ptr = fence_ptr;

                let fence = crate::drm_crtc_upstream::drm_crtc_create_fence(crtc);
                if fence.is_null() {
                    return -ENOMEM;
                }

                let ret = setup_out_fence(f.add(cur_count), fence);
                if ret != 0 {
                    dma_fence_put(fence);
                    return ret;
                }
                *num_fences = new_count as u32;

                (*(*crtc_state).event).base.fence = fence;
            }

            c += 1;
        }

        let num_conns = (*state).num_connector as usize;
        for i in 0..num_conns {
            let entry = (*state).connectors.add(i);
            let conn = (*entry).ptr;
            if conn.is_null() {
                continue;
            }
            let conn_state = (*entry).new_state;
            if (*conn_state).writeback_job.is_null() {
                continue;
            }

            let fence_ptr = get_out_fence_for_connector(state, conn);
            if fence_ptr.is_null() {
                continue;
            }

            let cur_count = *num_fences as usize;
            let new_count = cur_count + 1;
            let f = krealloc(*fence_state as *mut c_void, size_of::<DrmOutFenceState>() * new_count, GFP_KERNEL) as *mut DrmOutFenceState;
            if f.is_null() {
                return -ENOMEM;
            }
            *fence_state = f;
            core::ptr::write_bytes(f.add(cur_count), 0, 1);
            (*f.add(cur_count)).out_fence_ptr = fence_ptr;

            let fence = drm_writeback_get_out_fence(conn as *mut c_void);
            if fence.is_null() {
                return -ENOMEM;
            }

            let ret = setup_out_fence(f.add(cur_count), fence);
            if ret != 0 {
                dma_fence_put(fence);
                return ret;
            }
            *num_fences = new_count as u32;
        }

        if c == 0 && ((*arg).flags & DRM_MODE_PAGE_FLIP_EVENT != 0) {
            return -EINVAL;
        }
    }

    0
}

// upstream: drivers/gpu/drm/drm_atomic_uapi.c complete_signaling()
unsafe fn complete_signaling(
    dev: *mut DrmDevice,
    state: *mut DrmAtomicCommit,
    fence_state: *mut DrmOutFenceState,
    num_fences: u32,
    install_fds: bool,
) {
    if install_fds {
        for i in 0..(num_fences as usize) {
            unsafe {
                let entry = fence_state.add(i);
                fd_install((*entry).fd, (*(*entry).sync_file).file);
            }
        }
        unsafe { kfree(fence_state); }
        return;
    }

    let dev_ptr = dev;
    unsafe {
        let num_crtcs = (*(*state).dev).mode_config.num_crtc as usize;
        for i in 0..num_crtcs {
            if (*state).crtcs.is_null() {
                break;
            }
            let entry = (*state).crtcs.add(i);
            if (*entry).ptr.is_null() {
                continue;
            }
            let crtc_state = (*entry).new_state;
            let event = (*crtc_state).event;
            if !event.is_null() && (!(*event).base.fence.is_null() || !(*event).base.file_priv.is_null()) {
                drm_event_cancel_free(dev_ptr, core::ptr::addr_of_mut!((*event).base));
                (*crtc_state).event = core::ptr::null_mut();
            }
        }

        if fence_state.is_null() {
            return;
        }

        for i in 0..(num_fences as usize) {
            let entry = fence_state.add(i);
            if !(*entry).sync_file.is_null() {
                fput((*(*entry).sync_file).file);
            }
            if (*entry).fd >= 0 {
                put_unused_fd((*entry).fd);
            }
            if !(*entry).out_fence_ptr.is_null() {
                let _ = put_user(-1i32, (*entry).out_fence_ptr);
            }
        }

        kfree(fence_state);
    }
}

// upstream: drivers/gpu/drm/drm_atomic_uapi.c set_async_flip()
unsafe fn set_async_flip(state: *mut DrmAtomicCommit) {
    let dev = unsafe { (*state).dev };
    let num_crtcs = unsafe { (*dev).mode_config.num_crtc as usize };
    unsafe {
        for i in 0..num_crtcs {
            if (*state).crtcs.is_null() {
                break;
            }
            let entry = (*state).crtcs.add(i);
            if (*entry).ptr.is_null() {
                continue;
            }
            (*(*entry).new_state).async_flip = true;
        }
    }
}

// upstream: drivers/gpu/drm/drm_atomic_uapi.c drm_mode_atomic_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_atomic_ioctl(
    dev: *mut DrmDevice,
    data: *mut c_void,
    file_priv: *mut DrmFile,
) -> i32 {
    if dev.is_null() || data.is_null() || file_priv.is_null() {
        return -EINVAL;
    }

    if unsafe { !crate::drm_color_mgmt_upstream::drm_core_check_feature(dev, DRIVER_ATOMIC) } {
        return -EOPNOTSUPP;
    }

    if unsafe { !(*file_priv).atomic } {
        return -EINVAL;
    }

    let arg = data as *mut DrmModeAtomic;
    if (unsafe { (*arg).flags } & !DRM_MODE_ATOMIC_FLAGS) != 0 {
        return -EINVAL;
    }
    if unsafe { (*arg).reserved } != 0 {
        return -EINVAL;
    }

    let mut async_flip = false;
    if (unsafe { (*arg).flags } & DRM_MODE_PAGE_FLIP_ASYNC) != 0 {
        if unsafe { !(*dev).mode_config.async_page_flip } {
            return -EINVAL;
        }
        async_flip = true;
    }

    if ((unsafe { (*arg).flags } & DRM_MODE_ATOMIC_TEST_ONLY) != 0)
        && ((unsafe { (*arg).flags } & DRM_MODE_PAGE_FLIP_EVENT) != 0)
    {
        return -EINVAL;
    }

    let state = unsafe { crate::drm_atomic_upstream::drm_atomic_commit_alloc(dev) };
    if state.is_null() {
        return -ENOMEM;
    }

    let mut ctx = core::mem::MaybeUninit::<DrmModesetAcquireCtx>::uninit();
    unsafe {
        crate::drm_modeset_lock_upstream::drm_modeset_acquire_init(ctx.as_mut_ptr(), DRM_MODESET_ACQUIRE_INTERRUPTIBLE);
        (*state).acquire_ctx = ctx.as_mut_ptr();
        (*state).set_allow_modeset(((*arg).flags & DRM_MODE_ATOMIC_ALLOW_MODESET) != 0);
        (*state).set_plane_color_pipeline((*file_priv).plane_color_pipeline);
    }

    let objs_ptr = unsafe { (*arg).objs_ptr as *const u32 };
    let count_props_ptr = unsafe { (*arg).count_props_ptr as *const u32 };
    let props_ptr = unsafe { (*arg).props_ptr as *const u32 };
    let prop_values_ptr = unsafe { (*arg).prop_values_ptr as *const u64 };

    let mut ret: i32;

    'retry: loop {
        let mut copied_objs: usize = 0;
        let mut copied_props: usize = 0;
        let mut fence_state: *mut DrmOutFenceState = core::ptr::null_mut();
        let mut num_fences: u32 = 0;

        ret = 0;
        for _i in 0..(unsafe { (*arg).count_objs } as usize) {
            let mut obj_id: u32 = 0;
            let mut count_props: u32 = 0;

            if !unsafe { get_user(&mut obj_id, objs_ptr.add(copied_objs)) } {
                ret = -EFAULT;
                break;
            }

            let obj = unsafe { drm_mode_object_find(dev, file_priv, obj_id, DRM_MODE_OBJECT_ANY) };
            if obj.is_null() {
                ret = -ENOENT;
                break;
            }

            if unsafe { (*obj).properties.is_null() } {
                unsafe { drm_mode_object_put(obj); }
                ret = -ENOENT;
                break;
            }

            if !unsafe { get_user(&mut count_props, count_props_ptr.add(copied_objs)) } {
                unsafe { drm_mode_object_put(obj); }
                ret = -EFAULT;
                break;
            }

            copied_objs += 1;

            for _j in 0..(count_props as usize) {
                let mut prop_id: u32 = 0;
                let mut prop_value: u64 = 0;

                if !unsafe { get_user(&mut prop_id, props_ptr.add(copied_props)) } {
                    unsafe { drm_mode_object_put(obj); }
                    ret = -EFAULT;
                    break;
                }

                let prop = unsafe { crate::drm_mode_object_upstream::drm_mode_obj_find_prop_id(obj, prop_id) };
                if prop.is_null() {
                    unsafe { drm_mode_object_put(obj); }
                    ret = -ENOENT;
                    break;
                }

                if !unsafe { copy_from_user(core::ptr::addr_of_mut!(prop_value) as *mut c_void, prop_values_ptr.add(copied_props) as *const c_void, size_of::<u64>()) } {
                    unsafe { drm_mode_object_put(obj); }
                    ret = -EFAULT;
                    break;
                }

                ret = unsafe { drm_atomic_set_property(state, file_priv, obj, prop, prop_value, async_flip) };
                if ret != 0 {
                    unsafe { drm_mode_object_put(obj); }
                    break;
                }

                copied_props += 1;
            }

            unsafe { drm_mode_object_put(obj); }
            if ret != 0 {
                break;
            }
        }

        if ret == 0 {
            ret = unsafe { prepare_signaling(dev, state, arg, file_priv, &mut fence_state, &mut num_fences) };
        }

        if ret == 0 {
            if (unsafe { (*arg).flags } & DRM_MODE_PAGE_FLIP_ASYNC) != 0 {
                unsafe { set_async_flip(state); }
            }

            if (unsafe { (*arg).flags } & DRM_MODE_ATOMIC_TEST_ONLY) != 0 {
                ret = unsafe { crate::drm_atomic_upstream::drm_atomic_check_only(state) };
            } else if (unsafe { (*arg).flags } & DRM_MODE_ATOMIC_NONBLOCK) != 0 {
                ret = unsafe { crate::drm_atomic_upstream::drm_atomic_nonblocking_commit(state) };
            } else {
                ret = unsafe { crate::drm_atomic_upstream::drm_atomic_commit(state) };
            }
        }

        unsafe { complete_signaling(dev, state, fence_state, num_fences, ret == 0); }

        if ret == -EDEADLK {
            unsafe {
                crate::drm_atomic_upstream::drm_atomic_commit_clear(state);
                ret = crate::drm_modeset_lock_upstream::drm_modeset_backoff(ctx.as_mut_ptr());
            }
            if ret == 0 {
                continue 'retry;
            }
        }

        break;
    }

    unsafe {
        crate::drm_atomic_upstream::drm_atomic_commit_put(state);
        crate::drm_modeset_lock_upstream::drm_modeset_drop_locks(ctx.as_mut_ptr());
        crate::drm_modeset_lock_upstream::drm_modeset_acquire_fini(ctx.as_mut_ptr());
    }

    ret
}
