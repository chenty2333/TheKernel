// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_atomic.c` (batch B2a).

#![allow(non_snake_case, dead_code, unsafe_code)]
#![allow(clippy::missing_safety_doc)]

use alloc::format;
use core::ffi::{c_char, c_int, c_void};
use core::mem::size_of;

use crate::drm_atomic_types::{
    DrmAtomicCommit, DrmBridgeState, DrmColoropsState, DrmConnnectorsState, DrmCrtcCommit,
    DrmCrtcsState, DrmPlanesState, DrmPrivateObj, DrmPrivateObjsState, DrmPrivateState,
    DrmPrivateStateFuncs,
};
use crate::drm_connector_types::{DrmConnector, DrmConnectorListIter, DrmConnectorState, DRM_LINK_STATUS_GOOD};
use crate::drm_crtc_types::{DrmCrtc, DrmCrtcState, DrmModeSet};
use crate::drm_device_types::DrmDevice;
use crate::drm_encoder_types::DrmEncoder;
use crate::drm_framebuffer_types::DrmFramebuffer;
use crate::drm_modes_types::DrmDisplayMode;
use crate::drm_plane_types::{DrmPlane, DrmPlaneState};
use crate::drm_print_types::DrmPrinter;
use crate::drm_rect_types::DrmRect;
use crate::uapi_drm_consts::*;
use crate::uapi_drm_mode_types::DrmModeRect;

use tk_linux_kapi::config::{
    CONFIG_HZ, EDEADLK, EINVAL, ENOMEM, ENOSPC, EOPNOTSUPP, ERANGE, ETIMEDOUT, GFP_KERNEL, HZ,
    IS_ERR, PTR_ERR,
};
use tk_linux_kapi::list::{list_add_tail, list_del, INIT_LIST_HEAD};
use tk_linux_kapi::memory::{kfree, kref_init, kzalloc, kzalloc_objs};
use tk_linux_kapi::types::Kref;

const DRIVER_ATOMIC: u32 = 1 << 4;

// upstream: include/drm/drm_atomic.h drm_atomic_crtc_needs_modeset()
#[inline]
pub unsafe fn drm_atomic_crtc_needs_modeset(state: *const DrmCrtcState) -> bool {
    unsafe {
        (*state).mode_changed()
            || (*state).active_changed()
            || (*state).connectors_changed()
    }
}

unsafe extern "C" {
    fn drm_dev_get(dev: *mut DrmDevice) -> *mut DrmDevice;
    fn drm_dev_put(dev: *mut DrmDevice);
    fn drm_modeset_lock(
        lock: *mut crate::drm_modeset_lock_types::DrmModesetLock,
        ctx: *mut crate::drm_modeset_lock_types::DrmModesetAcquireCtx,
    ) -> i32;
    fn drm_modeset_unlock(lock: *mut crate::drm_modeset_lock_types::DrmModesetLock);
    fn drm_connector_list_iter_begin(dev: *mut DrmDevice, iter: *mut DrmConnectorListIter);
    fn drm_connector_list_iter_next(iter: *mut DrmConnectorListIter) -> *mut DrmConnector;
    fn drm_connector_list_iter_end(iter: *mut DrmConnectorListIter);
    fn drm_writeback_cleanup_job(job: *mut c_void);
    fn krealloc(p: *mut c_void, new_size: usize, flags: u32) -> *mut c_void;
}

#[inline]
unsafe fn drm_crtc_index(crtc: *const DrmCrtc) -> usize {
    unsafe { (*crtc).index as usize }
}

#[inline]
unsafe fn drm_plane_index(plane: *const DrmPlane) -> usize {
    unsafe { (*plane).index as usize }
}

#[inline]
unsafe fn drm_connector_index(connector: *const DrmConnector) -> usize {
    unsafe { (*connector).index as usize }
}

#[inline]
unsafe fn drm_crtc_mask(crtc: *const DrmCrtc) -> u32 {
    1u32 << unsafe { (*crtc).index }
}

#[inline]
unsafe fn drm_plane_mask(plane: *const DrmPlane) -> u32 {
    1u32 << unsafe { (*plane).index }
}

#[inline]
unsafe fn drm_connector_mask(connector: *const DrmConnector) -> u32 {
    1u32 << unsafe { (*connector).index }
}

#[inline]
unsafe fn drm_mode_object_put(obj: *mut crate::drm_mode_object_types::DrmModeObject) {
    if !obj.is_null() {
        crate::drm_mode_object_upstream::drm_mode_object_put(obj);
    }
}

#[inline]
unsafe fn drm_connector_get(connector: *mut DrmConnector) {
    if !connector.is_null() {
        crate::drm_mode_object_upstream::drm_mode_object_get(core::ptr::addr_of_mut!((*connector).base));
    }
}

#[inline]
unsafe fn drm_connector_put(connector: *mut DrmConnector) {
    if !connector.is_null() {
        crate::drm_mode_object_upstream::drm_mode_object_put(core::ptr::addr_of_mut!((*connector).base));
    }
}

#[inline]
unsafe fn drm_crtc_commit_put(commit: *mut DrmCrtcCommit) {
    if !commit.is_null() {
        tk_linux_kapi::memory::kref_put(core::ptr::addr_of_mut!((*commit).r#ref), __drm_crtc_commit_free);
    }
}

#[inline]
pub unsafe fn drm_atomic_commit_get(state: *mut DrmAtomicCommit) {
    if !state.is_null() {
        unsafe { tk_linux_kapi::memory::kref_get(core::ptr::addr_of_mut!((*state).r#ref)); }
    }
}

pub use drm_atomic_commit_get as drm_atomic_state_get;

#[inline]
pub unsafe fn drm_atomic_commit_put(state: *mut DrmAtomicCommit) {
    if !state.is_null() {
        tk_linux_kapi::memory::kref_put(core::ptr::addr_of_mut!((*state).r#ref), __drm_atomic_commit_free);
    }
}

pub use drm_atomic_commit_put as drm_atomic_state_put;

#[inline]
pub unsafe fn drm_atomic_get_old_crtc_state(
    state: *const DrmAtomicCommit,
    crtc: *mut DrmCrtc,
) -> *mut DrmCrtcState {
    if state.is_null() || crtc.is_null() || (*state).crtcs.is_null() {
        return core::ptr::null_mut();
    }
    unsafe { (*(*state).crtcs.add(drm_crtc_index(crtc))).old_state }
}

#[inline]
pub unsafe fn drm_atomic_get_new_crtc_state(
    state: *const DrmAtomicCommit,
    crtc: *mut DrmCrtc,
) -> *mut DrmCrtcState {
    if state.is_null() || crtc.is_null() || (*state).crtcs.is_null() {
        return core::ptr::null_mut();
    }
    unsafe { (*(*state).crtcs.add(drm_crtc_index(crtc))).new_state }
}

#[inline]
pub unsafe fn drm_atomic_get_old_plane_state(
    state: *const DrmAtomicCommit,
    plane: *mut DrmPlane,
) -> *mut DrmPlaneState {
    if state.is_null() || plane.is_null() || (*state).planes.is_null() {
        return core::ptr::null_mut();
    }
    unsafe { (*(*state).planes.add(drm_plane_index(plane))).old_state }
}

#[inline]
pub unsafe fn drm_atomic_get_new_plane_state(
    state: *const DrmAtomicCommit,
    plane: *mut DrmPlane,
) -> *mut DrmPlaneState {
    if state.is_null() || plane.is_null() || (*state).planes.is_null() {
        return core::ptr::null_mut();
    }
    unsafe { (*(*state).planes.add(drm_plane_index(plane))).new_state }
}

#[inline]
pub unsafe fn drm_atomic_get_old_connector_state(
    state: *const DrmAtomicCommit,
    connector: *mut DrmConnector,
) -> *mut DrmConnectorState {
    if state.is_null() || connector.is_null() || (*state).connectors.is_null() {
        return core::ptr::null_mut();
    }
    let index = drm_connector_index(connector);
    if index >= (*state).num_connector as usize {
        return core::ptr::null_mut();
    }
    unsafe { (*(*state).connectors.add(index)).old_state }
}

#[inline]
pub unsafe fn drm_atomic_get_new_connector_state(
    state: *const DrmAtomicCommit,
    connector: *mut DrmConnector,
) -> *mut DrmConnectorState {
    if state.is_null() || connector.is_null() || (*state).connectors.is_null() {
        return core::ptr::null_mut();
    }
    let index = drm_connector_index(connector);
    if index >= (*state).num_connector as usize {
        return core::ptr::null_mut();
    }
    unsafe { (*(*state).connectors.add(index)).new_state }
}

#[inline]
unsafe fn drm_plane_state_src(state: *const DrmPlaneState) -> DrmRect {
    DrmRect {
        x1: (*state).src_x as i32,
        y1: (*state).src_y as i32,
        x2: ((*state).src_x + (*state).src_w) as i32,
        y2: ((*state).src_y + (*state).src_h) as i32,
    }
}

#[inline]
unsafe fn drm_plane_state_dest(state: *const DrmPlaneState) -> DrmRect {
    DrmRect {
        x1: (*state).crtc_x,
        y1: (*state).crtc_y,
        x2: (*state).crtc_x + (*state).crtc_w as i32,
        y2: (*state).crtc_y + (*state).crtc_h as i32,
    }
}

#[inline]
fn is_power_of_2(x: u32) -> bool {
    x != 0 && (x & (x - 1)) == 0
}

#[inline]
fn drm_rotation_90_or_270(rotation: u32) -> bool {
    (rotation & (DRM_MODE_ROTATE_90 | DRM_MODE_ROTATE_270)) != 0
}

#[inline]
unsafe fn drm_core_check_feature(dev: *const DrmDevice, feature: u32) -> bool {
    if dev.is_null() {
        return false;
    }
    let drv = (*dev).driver;
    if drv.is_null() {
        return false;
    }
    ((*drv).driver_features & feature) != 0
}

#[inline]
unsafe fn drm_drv_uses_atomic_modeset(dev: *const DrmDevice) -> bool {
    drm_core_check_feature(dev, DRIVER_ATOMIC)
}

#[inline]
unsafe fn drm_priv_to_bridge_state(obj_state: *mut DrmPrivateState) -> *mut DrmBridgeState {
    obj_state as *mut DrmBridgeState
}

// upstream: drivers/gpu/drm/drm_atomic.c __drm_crtc_commit_free()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __drm_crtc_commit_free(kref: *mut Kref) {
    if kref.is_null() {
        return;
    }
    let commit = (kref as *mut u8).sub(core::mem::offset_of!(DrmCrtcCommit, r#ref)) as *mut DrmCrtcCommit;
    kfree(commit);
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_crtc_commit_wait()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_crtc_commit_wait(commit: *mut DrmCrtcCommit) -> i32 {
    let timeout = 10 * HZ as i64;
    if commit.is_null() {
        return 0;
    }

    let ret = unsafe { (*commit).hw_done.wait_timeout(timeout) };
    if ret == 0 {
        return -ETIMEDOUT;
    }

    let ret = unsafe { (*commit).flip_done.wait_timeout(timeout) };
    if ret == 0 {
        return -ETIMEDOUT;
    }

    0
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_commit_default_release()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_commit_default_release(state: *mut DrmAtomicCommit) {
    if state.is_null() {
        return;
    }
    unsafe {
        kfree((*state).connectors);
        kfree((*state).crtcs);
        kfree((*state).planes);
        kfree((*state).colorops);
        kfree((*state).private_objs);
    }
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_commit_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_commit_init(
    dev: *mut DrmDevice,
    state: *mut DrmAtomicCommit,
) -> i32 {
    if dev.is_null() || state.is_null() {
        return -EINVAL;
    }
    unsafe {
        kref_init(core::ptr::addr_of_mut!((*state).r#ref));
        (*state).set_allow_modeset(true);

        let num_crtc = (*dev).mode_config.num_crtc;
        if num_crtc > 0 {
            (*state).crtcs = kzalloc_objs::<DrmCrtcsState, i32>(num_crtc);
            if (*state).crtcs.is_null() {
                drm_atomic_commit_default_release(state);
                return -ENOMEM;
            }
        }

        let num_planes = (*dev).mode_config.num_total_plane;
        if num_planes > 0 {
            (*state).planes = kzalloc_objs::<DrmPlanesState, i32>(num_planes);
            if (*state).planes.is_null() {
                drm_atomic_commit_default_release(state);
                return -ENOMEM;
            }
        }

        let num_colorops = (*dev).mode_config.num_colorop;
        if num_colorops > 0 {
            (*state).colorops = kzalloc_objs::<DrmColoropsState, i32>(num_colorops);
            if (*state).colorops.is_null() {
                drm_atomic_commit_default_release(state);
                return -ENOMEM;
            }
        }

        drm_dev_get(dev);
        (*state).dev = dev;
    }

    0
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_commit_alloc()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_commit_alloc(dev: *mut DrmDevice) -> *mut DrmAtomicCommit {
    if dev.is_null() {
        return core::ptr::null_mut();
    }
    let config = unsafe { core::ptr::addr_of_mut!((*dev).mode_config) };
    let funcs = unsafe { (*config).funcs };

    if funcs.is_null() || unsafe { (*funcs).atomic_state_alloc.is_none() } {
        let state = unsafe { kzalloc(size_of::<DrmAtomicCommit>(), GFP_KERNEL) as *mut DrmAtomicCommit };
        if state.is_null() {
            return core::ptr::null_mut();
        }
        if unsafe { drm_atomic_commit_init(dev, state) } < 0 {
            unsafe { kfree(state); }
            return core::ptr::null_mut();
        }
        return state;
    }

    unsafe { ((*funcs).atomic_state_alloc.unwrap())(dev) }
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_commit_default_clear()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_commit_default_clear(state: *mut DrmAtomicCommit) {
    if state.is_null() {
        return;
    }
    let dev = unsafe { (*state).dev };
    if dev.is_null() {
        return;
    }
    let config = unsafe { core::ptr::addr_of_mut!((*dev).mode_config) };

    unsafe {
        (*state).set_checked(false);

        let num_conn = (*state).num_connector as usize;
        for i in 0..num_conn {
            let conn_entry = (*state).connectors.add(i);
            let connector = (*conn_entry).ptr;
            if connector.is_null() {
                continue;
            }

            let funcs = (*connector).funcs;
            if !funcs.is_null() {
                if let Some(destroy_fn) = (*funcs).atomic_destroy_state {
                    destroy_fn(connector, (*conn_entry).state_to_destroy);
                }
            }

            (*conn_entry).ptr = core::ptr::null_mut();
            (*conn_entry).state_to_destroy = core::ptr::null_mut();
            (*conn_entry).old_state = core::ptr::null_mut();
            (*conn_entry).new_state = core::ptr::null_mut();
            drm_connector_put(connector);
        }

        let num_crtc = (*config).num_crtc as usize;
        for i in 0..num_crtc {
            if (*state).crtcs.is_null() {
                break;
            }
            let crtc_entry = (*state).crtcs.add(i);
            let crtc = (*crtc_entry).ptr;
            if crtc.is_null() {
                continue;
            }

            let funcs = (*crtc).funcs;
            if !funcs.is_null() {
                if let Some(destroy_fn) = (*funcs).atomic_destroy_state {
                    destroy_fn(crtc, (*crtc_entry).state_to_destroy);
                }
            }

            (*crtc_entry).ptr = core::ptr::null_mut();
            (*crtc_entry).state_to_destroy = core::ptr::null_mut();
            (*crtc_entry).old_state = core::ptr::null_mut();
            (*crtc_entry).new_state = core::ptr::null_mut();

            if !(*crtc_entry).commit.is_null() {
                drm_crtc_commit_put((*crtc_entry).commit);
                (*crtc_entry).commit = core::ptr::null_mut();
            }
        }

        let num_planes = (*config).num_total_plane as usize;
        for i in 0..num_planes {
            if (*state).planes.is_null() {
                break;
            }
            let plane_entry = (*state).planes.add(i);
            let plane = (*plane_entry).ptr;
            if plane.is_null() {
                continue;
            }

            let funcs = (*plane).funcs;
            if !funcs.is_null() {
                if let Some(destroy_fn) = (*funcs).atomic_destroy_state {
                    destroy_fn(plane, (*plane_entry).state_to_destroy);
                }
            }

            (*plane_entry).ptr = core::ptr::null_mut();
            (*plane_entry).state_to_destroy = core::ptr::null_mut();
            (*plane_entry).old_state = core::ptr::null_mut();
            (*plane_entry).new_state = core::ptr::null_mut();
        }

        let num_priv = (*state).num_private_objs as usize;
        for i in 0..num_priv {
            if (*state).private_objs.is_null() {
                break;
            }
            let priv_entry = (*state).private_objs.add(i);
            let obj = (*priv_entry).ptr;
            if obj.is_null() {
                continue;
            }

            let funcs = (*obj).funcs;
            if !funcs.is_null() {
                if let Some(destroy_fn) = (*funcs).atomic_destroy_state {
                    destroy_fn(obj, (*priv_entry).state_to_destroy);
                }
            }

            (*priv_entry).ptr = core::ptr::null_mut();
            (*priv_entry).state_to_destroy = core::ptr::null_mut();
            (*priv_entry).old_state = core::ptr::null_mut();
            (*priv_entry).new_state = core::ptr::null_mut();
        }
        (*state).num_private_objs = 0;

        if !(*state).fake_commit.is_null() {
            drm_crtc_commit_put((*state).fake_commit);
            (*state).fake_commit = core::ptr::null_mut();
        }
    }
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_commit_clear()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_commit_clear(state: *mut DrmAtomicCommit) {
    if state.is_null() {
        return;
    }
    let dev = unsafe { (*state).dev };
    if dev.is_null() {
        return;
    }
    let config = unsafe { core::ptr::addr_of_mut!((*dev).mode_config) };
    let funcs = unsafe { (*config).funcs };

    if !funcs.is_null() {
        if let Some(clear_fn) = unsafe { (*funcs).atomic_state_clear } {
            unsafe { clear_fn(state); }
            return;
        }
    }

    unsafe { drm_atomic_commit_default_clear(state); }
}

// upstream: drivers/gpu/drm/drm_atomic.c __drm_atomic_commit_free()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __drm_atomic_commit_free(ref_: *mut Kref) {
    if ref_.is_null() {
        return;
    }
    let state = (ref_ as *mut u8).sub(core::mem::offset_of!(DrmAtomicCommit, r#ref)) as *mut DrmAtomicCommit;
    let dev = unsafe { (*state).dev };
    if dev.is_null() {
        return;
    }
    let config = unsafe { core::ptr::addr_of_mut!((*dev).mode_config) };

    unsafe {
        drm_atomic_commit_clear(state);

        let funcs = (*config).funcs;
        if !funcs.is_null() && (*funcs).atomic_state_free.is_some() {
            ((*funcs).atomic_state_free.unwrap())(state);
        } else {
            drm_atomic_commit_default_release(state);
            kfree(state);
        }

        drm_dev_put(dev);
    }
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_get_crtc_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_get_crtc_state(
    state: *mut DrmAtomicCommit,
    crtc: *mut DrmCrtc,
) -> *mut DrmCrtcState {
    if state.is_null() || crtc.is_null() {
        return tk_linux_kapi::config::ERR_PTR(-EINVAL);
    }

    let index = unsafe { drm_crtc_index(crtc) };
    let crtc_state = unsafe { drm_atomic_get_new_crtc_state(state, crtc) };
    if !crtc_state.is_null() {
        return crtc_state;
    }

    let ret = unsafe { drm_modeset_lock(core::ptr::addr_of_mut!((*crtc).mutex), (*state).acquire_ctx) };
    if ret != 0 {
        return tk_linux_kapi::config::ERR_PTR(ret);
    }

    let funcs = unsafe { (*crtc).funcs };
    if funcs.is_null() || unsafe { (*funcs).atomic_duplicate_state.is_none() } {
        return tk_linux_kapi::config::ERR_PTR(-EINVAL);
    }

    let crtc_state = unsafe { ((*funcs).atomic_duplicate_state.unwrap())(crtc) };
    if crtc_state.is_null() {
        return tk_linux_kapi::config::ERR_PTR(-ENOMEM);
    }

    unsafe {
        let entry = (*state).crtcs.add(index);
        (*entry).state_to_destroy = crtc_state;
        (*entry).old_state = (*crtc).state;
        (*entry).new_state = crtc_state;
        (*entry).ptr = crtc;
        (*crtc_state).state = state;
    }

    crtc_state
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_crtc_check()
unsafe fn drm_atomic_crtc_check(
    old_crtc_state: *const DrmCrtcState,
    new_crtc_state: *const DrmCrtcState,
) -> i32 {
    let crtc = unsafe { (*new_crtc_state).crtc };
    let dev = unsafe { (*crtc).dev };

    if unsafe { (*new_crtc_state).active && !(*new_crtc_state).enable } {
        return -EINVAL;
    }

    if unsafe { drm_core_check_feature(dev, DRIVER_ATOMIC) } {
        if unsafe { (*new_crtc_state).enable && (*new_crtc_state).mode_blob.is_null() } {
            return -EINVAL;
        }
        if unsafe { !(*new_crtc_state).enable && !(*new_crtc_state).mode_blob.is_null() } {
            return -EINVAL;
        }
    }

    if unsafe { !(*new_crtc_state).event.is_null() && !(*new_crtc_state).active && !(*old_crtc_state).active } {
        return -EINVAL;
    }

    0
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_crtc_print_state()
unsafe fn drm_atomic_crtc_print_state(p: *mut DrmPrinter, state: *const DrmCrtcState) {
    if p.is_null() || state.is_null() {
        return;
    }
    let crtc = unsafe { (*state).crtc };
    if crtc.is_null() {
        return;
    }

    let line = format!("crtc[{}]: \n", unsafe { (*crtc).base.id });
    unsafe { crate::drm_print_upstream::drm_printf(p, &line) };
    let line = format!("\tenable={}\n", unsafe { (*state).enable as i32 });
    unsafe { crate::drm_print_upstream::drm_printf(p, &line) };
    let line = format!("\tactive={}\n", unsafe { (*state).active as i32 });
    unsafe { crate::drm_print_upstream::drm_printf(p, &line) };
    let line = format!("\tself_refresh_active={}\n", unsafe { (*state).self_refresh_active as i32 });
    unsafe { crate::drm_print_upstream::drm_printf(p, &line) };
    let line = format!("\tplanes_changed={}\n", unsafe { (*state).planes_changed() as i32 });
    unsafe { crate::drm_print_upstream::drm_printf(p, &line) };
    let line = format!("\tmode_changed={}\n", unsafe { (*state).mode_changed() as i32 });
    unsafe { crate::drm_print_upstream::drm_printf(p, &line) };
    let line = format!("\tactive_changed={}\n", unsafe { (*state).active_changed() as i32 });
    unsafe { crate::drm_print_upstream::drm_printf(p, &line) };
    let line = format!("\tconnectors_changed={}\n", unsafe { (*state).connectors_changed() as i32 });
    unsafe { crate::drm_print_upstream::drm_printf(p, &line) };
    let line = format!("\tcolor_mgmt_changed={}\n", unsafe { (*state).color_mgmt_changed() as i32 });
    unsafe { crate::drm_print_upstream::drm_printf(p, &line) };
    let line = format!("\tplane_mask={:x}\n", unsafe { (*state).plane_mask });
    unsafe { crate::drm_print_upstream::drm_printf(p, &line) };
    let line = format!("\tconnector_mask={:x}\n", unsafe { (*state).connector_mask });
    unsafe { crate::drm_print_upstream::drm_printf(p, &line) };
    let line = format!("\tencoder_mask={:x}\n", unsafe { (*state).encoder_mask });
    unsafe { crate::drm_print_upstream::drm_printf(p, &line) };

    let funcs = unsafe { (*crtc).funcs };
    if !funcs.is_null() {
        if let Some(print_fn) = unsafe { (*funcs).atomic_print_state } {
            unsafe { print_fn(p, state as *mut DrmCrtcState); }
        }
    }
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_connector_check()
unsafe fn drm_atomic_connector_check(
    connector: *mut DrmConnector,
    state: *mut DrmConnectorState,
) -> i32 {
    let info = unsafe { core::ptr::addr_of!((*connector).display_info) };
    let bpc = unsafe { (*info).bpc };
    unsafe { (*state).max_bpc = if bpc != 0 { bpc as u8 } else { 8 } };

    if !unsafe { (*connector).max_bpc_property.is_null() } {
        unsafe {
            (*state).max_bpc = core::cmp::min((*state).max_bpc, (*state).max_requested_bpc);
        }
    }

    let is_writeback = unsafe { (*connector).connector_type == DRM_MODE_CONNECTOR_WRITEBACK as i32 };
    let writeback_job = unsafe { (*state).writeback_job };

    if !is_writeback || writeback_job.is_null() {
        return 0;
    }

    let fb = unsafe { *(writeback_job as *const *mut DrmFramebuffer) };
    if !fb.is_null() && unsafe { (*state).crtc.is_null() } {
        return -EINVAL;
    }

    let mut crtc_state: *mut DrmCrtcState = core::ptr::null_mut();
    if !unsafe { (*state).crtc.is_null() } {
        crtc_state = unsafe { drm_atomic_get_new_crtc_state((*state).state, (*state).crtc) };
    }

    if !fb.is_null() && (crtc_state.is_null() || !unsafe { (*crtc_state).active }) {
        return -EINVAL;
    }

    if fb.is_null() {
        unsafe {
            drm_writeback_cleanup_job(writeback_job);
            (*state).writeback_job = core::ptr::null_mut();
        }
    }

    0
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_get_plane_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_get_plane_state(
    state: *mut DrmAtomicCommit,
    plane: *mut DrmPlane,
) -> *mut DrmPlaneState {
    if state.is_null() || plane.is_null() {
        return tk_linux_kapi::config::ERR_PTR(-EINVAL);
    }

    let index = unsafe { drm_plane_index(plane) };
    let plane_state = unsafe { drm_atomic_get_new_plane_state(state, plane) };
    if !plane_state.is_null() {
        return plane_state;
    }

    let ret = unsafe { drm_modeset_lock(core::ptr::addr_of_mut!((*plane).mutex), (*state).acquire_ctx) };
    if ret != 0 {
        return tk_linux_kapi::config::ERR_PTR(ret);
    }

    let funcs = unsafe { (*plane).funcs };
    if funcs.is_null() || unsafe { (*funcs).atomic_duplicate_state.is_none() } {
        return tk_linux_kapi::config::ERR_PTR(-EINVAL);
    }

    let plane_state = unsafe { ((*funcs).atomic_duplicate_state.unwrap())(plane) };
    if plane_state.is_null() {
        return tk_linux_kapi::config::ERR_PTR(-ENOMEM);
    }

    unsafe {
        let entry = (*state).planes.add(index);
        (*entry).state_to_destroy = plane_state;
        (*entry).ptr = plane;
        (*entry).old_state = (*plane).state;
        (*entry).new_state = plane_state;
        (*plane_state).state = state;

        if !(*plane_state).crtc.is_null() {
            let crtc_state = drm_atomic_get_crtc_state(state, (*plane_state).crtc);
            if IS_ERR(crtc_state) {
                return crtc_state.cast();
            }
        }
    }

    plane_state
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_get_colorop_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_get_colorop_state(
    _state: *mut DrmAtomicCommit,
    _colorop: *mut c_void,
) -> *mut c_void {
    core::ptr::null_mut()
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_get_old_colorop_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_get_old_colorop_state(
    _state: *mut DrmAtomicCommit,
    _colorop: *mut c_void,
) -> *mut c_void {
    core::ptr::null_mut()
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_get_new_colorop_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_get_new_colorop_state(
    _state: *mut DrmAtomicCommit,
    _colorop: *mut c_void,
) -> *mut c_void {
    core::ptr::null_mut()
}

// upstream: drivers/gpu/drm/drm_atomic.c plane_switching_crtc()
unsafe fn plane_switching_crtc(
    old_plane_state: *const DrmPlaneState,
    new_plane_state: *const DrmPlaneState,
) -> bool {
    if unsafe { (*old_plane_state).crtc.is_null() || (*new_plane_state).crtc.is_null() } {
        return false;
    }
    unsafe { (*old_plane_state).crtc != (*new_plane_state).crtc }
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_plane_check()
unsafe fn drm_atomic_plane_check(
    old_plane_state: *const DrmPlaneState,
    new_plane_state: *const DrmPlaneState,
) -> i32 {
    let plane = unsafe { (*new_plane_state).plane };
    let crtc = unsafe { (*new_plane_state).crtc };
    let fb = unsafe { (*new_plane_state).fb };

    if !crtc.is_null() && fb.is_null() {
        return -EINVAL;
    } else if !fb.is_null() && crtc.is_null() {
        return -EINVAL;
    }

    if crtc.is_null() {
        return 0;
    }

    if unsafe { ((*plane).possible_crtcs & drm_crtc_mask(crtc)) == 0 } {
        return -EINVAL;
    }

    let format = unsafe { (*(*fb).format).format };
    let modifier = unsafe { (*fb).modifier };
    if unsafe { !crate::drm_plane_upstream::drm_plane_has_format(plane, format, modifier) } {
        return -EINVAL;
    }

    let crtc_w = unsafe { (*new_plane_state).crtc_w };
    let crtc_h = unsafe { (*new_plane_state).crtc_h };
    let crtc_x = unsafe { (*new_plane_state).crtc_x };
    let crtc_y = unsafe { (*new_plane_state).crtc_y };

    if crtc_w > i32::MAX as u32
        || crtc_x > i32::MAX - crtc_w as i32
        || crtc_h > i32::MAX as u32
        || crtc_y > i32::MAX - crtc_h as i32
    {
        return -ERANGE;
    }

    let fb_width = unsafe { (*fb).width << 16 };
    let fb_height = unsafe { (*fb).height << 16 };
    let src_w = unsafe { (*new_plane_state).src_w };
    let src_h = unsafe { (*new_plane_state).src_h };
    let src_x = unsafe { (*new_plane_state).src_x };
    let src_y = unsafe { (*new_plane_state).src_y };

    if src_w > fb_width
        || src_x > fb_width - src_w
        || src_h > fb_height
        || src_y > fb_height - src_h
    {
        return -ENOSPC;
    }

    let mut clips = unsafe { crate::drm_plane_upstream::__drm_plane_get_damage_clips(new_plane_state) };
    let mut num_clips = unsafe { crate::drm_plane_upstream::drm_plane_get_damage_clips_count(new_plane_state) };

    while num_clips > 0 {
        unsafe {
            if (*clips).x1 >= (*clips).x2
                || (*clips).y1 >= (*clips).y2
                || (*clips).x1 < 0
                || (*clips).y1 < 0
                || (*clips).x2 > fb_width as i32
                || (*clips).y2 > fb_height as i32
            {
                return -EINVAL;
            }
            clips = clips.add(1);
        }
        num_clips -= 1;
    }

    if plane_switching_crtc(old_plane_state, new_plane_state) {
        return -EINVAL;
    }

    0
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_colorop_print_state()
unsafe fn drm_atomic_colorop_print_state(_p: *mut DrmPrinter, _state: *const c_void) {}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_plane_print_state()
unsafe fn drm_atomic_plane_print_state(p: *mut DrmPrinter, state: *const DrmPlaneState) {
    if p.is_null() || state.is_null() {
        return;
    }
    let plane = unsafe { (*state).plane };
    if plane.is_null() {
        return;
    }

    let line = format!("plane[{}]: \n", unsafe { (*plane).base.id });
    unsafe { crate::drm_print_upstream::drm_printf(p, &line) };
    let crtc = unsafe { (*state).crtc };
    let line = format!("\tcrtc={}\n", if !crtc.is_null() { "crtc" } else { "(null)" });
    unsafe { crate::drm_print_upstream::drm_printf(p, &line) };
    let fb = unsafe { (*state).fb };
    let line = format!("\tfb={}\n", if !fb.is_null() { unsafe { (*fb).base.id } } else { 0 });
    unsafe { crate::drm_print_upstream::drm_printf(p, &line) };
    if !fb.is_null() {
        unsafe { crate::drm_framebuffer_upstream::drm_framebuffer_print_info(p, 2, fb); }
    }
    let line = format!("\trotation={:x}\n", unsafe { (*state).rotation });
    unsafe { crate::drm_print_upstream::drm_printf(p, &line) };
    let line = format!("\tnormalized-zpos={:x}\n", unsafe { (*state).normalized_zpos });
    unsafe { crate::drm_print_upstream::drm_printf(p, &line) };
    let line = format!("\tcolor_mgmt_changed={}\n", unsafe { (*state).color_mgmt_changed() as i32 });
    unsafe { crate::drm_print_upstream::drm_printf(p, &line) };

    let funcs = unsafe { (*plane).funcs };
    if !funcs.is_null() {
        if let Some(print_fn) = unsafe { (*funcs).atomic_print_state } {
            unsafe { print_fn(p, state as *mut DrmPlaneState); }
        }
    }
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_private_obj_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_private_obj_init(
    dev: *mut DrmDevice,
    obj: *mut DrmPrivateObj,
    funcs: *const DrmPrivateStateFuncs,
) -> i32 {
    if dev.is_null() || obj.is_null() || funcs.is_null() {
        return -EINVAL;
    }

    unsafe {
        core::ptr::write_bytes(obj, 0, 1);
        crate::drm_modeset_lock_upstream::drm_modeset_lock_init(core::ptr::addr_of_mut!((*obj).lock));

        (*obj).dev = dev;
        (*obj).funcs = funcs as *mut DrmPrivateStateFuncs;

        let priv_list = core::ptr::addr_of_mut!((*dev).mode_config.privobj_list);
        list_add_tail(core::ptr::addr_of_mut!((*obj).head), priv_list);

        if let Some(create_fn) = (*funcs).atomic_create_state {
            let state = create_fn(obj);
            if IS_ERR(state) {
                return PTR_ERR(state);
            }
            (*obj).state = state;
        }
    }

    0
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_private_obj_fini()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_private_obj_fini(obj: *mut DrmPrivateObj) {
    if obj.is_null() {
        return;
    }
    unsafe {
        list_del(core::ptr::addr_of_mut!((*obj).head));
        let funcs = (*obj).funcs;
        if !funcs.is_null() {
            if let Some(destroy_fn) = (*funcs).atomic_destroy_state {
                destroy_fn(obj, (*obj).state);
            }
        }
        // drm_modeset_lock_fini is not needed
    }
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_get_private_obj_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_get_private_obj_state(
    state: *mut DrmAtomicCommit,
    obj: *mut DrmPrivateObj,
) -> *mut DrmPrivateState {
    if state.is_null() || obj.is_null() {
        return tk_linux_kapi::config::ERR_PTR(-EINVAL);
    }

    let obj_state = unsafe { drm_atomic_get_new_private_obj_state(state, obj) };
    if !obj_state.is_null() {
        return obj_state;
    }

    let ret = unsafe { drm_modeset_lock(core::ptr::addr_of_mut!((*obj).lock), (*state).acquire_ctx) };
    if ret != 0 {
        return tk_linux_kapi::config::ERR_PTR(ret);
    }

    let num_objs = unsafe { (*state).num_private_objs + 1 } as usize;
    let size = size_of::<DrmPrivateObjsState>() * num_objs;
    let arr = unsafe { krealloc((*state).private_objs as *mut c_void, size, GFP_KERNEL) as *mut DrmPrivateObjsState };
    if arr.is_null() {
        return tk_linux_kapi::config::ERR_PTR(-ENOMEM);
    }

    unsafe {
        (*state).private_objs = arr;
        let index = (*state).num_private_objs as usize;
        core::ptr::write_bytes((*state).private_objs.add(index), 0, 1);

        let funcs = (*obj).funcs;
        if funcs.is_null() || (*funcs).atomic_duplicate_state.is_none() {
            return tk_linux_kapi::config::ERR_PTR(-EINVAL);
        }

        let obj_state = ((*funcs).atomic_duplicate_state.unwrap())(obj);
        if obj_state.is_null() {
            return tk_linux_kapi::config::ERR_PTR(-ENOMEM);
        }

        let entry = (*state).private_objs.add(index);
        (*entry).state_to_destroy = obj_state;
        (*entry).old_state = (*obj).state;
        (*entry).new_state = obj_state;
        (*entry).ptr = obj;
        (*obj_state).state = state;

        (*state).num_private_objs = num_objs as i32;

        obj_state
    }
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_get_old_private_obj_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_get_old_private_obj_state(
    state: *const DrmAtomicCommit,
    obj: *mut DrmPrivateObj,
) -> *mut DrmPrivateState {
    if state.is_null() || obj.is_null() || (*state).private_objs.is_null() {
        return core::ptr::null_mut();
    }
    unsafe {
        for i in 0..((*state).num_private_objs as usize) {
            if (*(*state).private_objs.add(i)).ptr == obj {
                return (*(*state).private_objs.add(i)).old_state;
            }
        }
    }
    core::ptr::null_mut()
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_get_new_private_obj_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_get_new_private_obj_state(
    state: *const DrmAtomicCommit,
    obj: *mut DrmPrivateObj,
) -> *mut DrmPrivateState {
    if state.is_null() || obj.is_null() || (*state).private_objs.is_null() {
        return core::ptr::null_mut();
    }
    unsafe {
        for i in 0..((*state).num_private_objs as usize) {
            if (*(*state).private_objs.add(i)).ptr == obj {
                return (*(*state).private_objs.add(i)).new_state;
            }
        }
    }
    core::ptr::null_mut()
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_get_old_connector_for_encoder()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_get_old_connector_for_encoder(
    state: *const DrmAtomicCommit,
    encoder: *mut DrmEncoder,
) -> *mut DrmConnector {
    if state.is_null() || encoder.is_null() || (*state).connectors.is_null() {
        return core::ptr::null_mut();
    }
    unsafe {
        for i in 0..((*state).num_connector as usize) {
            let conn = (*(*state).connectors.add(i)).ptr;
            let conn_state = (*(*state).connectors.add(i)).old_state;
            if !conn_state.is_null() && (*conn_state).best_encoder == encoder {
                return conn;
            }
        }
    }
    core::ptr::null_mut()
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_get_new_connector_for_encoder()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_get_new_connector_for_encoder(
    state: *const DrmAtomicCommit,
    encoder: *mut DrmEncoder,
) -> *mut DrmConnector {
    if state.is_null() || encoder.is_null() || (*state).connectors.is_null() {
        return core::ptr::null_mut();
    }
    unsafe {
        for i in 0..((*state).num_connector as usize) {
            let conn = (*(*state).connectors.add(i)).ptr;
            let conn_state = (*(*state).connectors.add(i)).new_state;
            if !conn_state.is_null() && (*conn_state).best_encoder == encoder {
                return conn;
            }
        }
    }
    core::ptr::null_mut()
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_get_connector_for_encoder()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_get_connector_for_encoder(
    encoder: *const DrmEncoder,
    ctx: *mut crate::drm_modeset_lock_types::DrmModesetAcquireCtx,
) -> *mut DrmConnector {
    if encoder.is_null() {
        return tk_linux_kapi::config::ERR_PTR(-EINVAL);
    }
    let dev = unsafe { (*encoder).dev };
    if dev.is_null() {
        return tk_linux_kapi::config::ERR_PTR(-EINVAL);
    }

    let conn_mutex = unsafe { core::ptr::addr_of_mut!((*dev).mode_config.connection_mutex) };
    let ret = unsafe { drm_modeset_lock(conn_mutex, ctx) };
    if ret != 0 {
        return tk_linux_kapi::config::ERR_PTR(ret);
    }

    let mut out_connector: *mut DrmConnector = tk_linux_kapi::config::ERR_PTR(-EINVAL);
    let mut conn_iter = core::mem::MaybeUninit::<DrmConnectorListIter>::uninit();

    unsafe {
        drm_connector_list_iter_begin(dev, conn_iter.as_mut_ptr());
        let iter = conn_iter.as_mut_ptr();
        loop {
            let connector = drm_connector_list_iter_next(iter);
            if connector.is_null() {
                break;
            }
            if (*connector).state.is_null() {
                continue;
            }
            if encoder as *mut DrmEncoder == (*(*connector).state).best_encoder {
                out_connector = connector;
                break;
            }
        }
        drm_connector_list_iter_end(iter);
        drm_modeset_unlock(conn_mutex);
    }

    out_connector
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_get_old_crtc_for_encoder()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_get_old_crtc_for_encoder(
    state: *mut DrmAtomicCommit,
    encoder: *mut DrmEncoder,
) -> *mut DrmCrtc {
    let connector = unsafe { drm_atomic_get_old_connector_for_encoder(state, encoder) };
    if connector.is_null() {
        return core::ptr::null_mut();
    }
    let conn_state = unsafe { drm_atomic_get_old_connector_state(state, connector) };
    if conn_state.is_null() {
        return core::ptr::null_mut();
    }
    unsafe { (*conn_state).crtc }
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_get_new_crtc_for_encoder()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_get_new_crtc_for_encoder(
    state: *mut DrmAtomicCommit,
    encoder: *mut DrmEncoder,
) -> *mut DrmCrtc {
    let connector = unsafe { drm_atomic_get_new_connector_for_encoder(state, encoder) };
    if connector.is_null() {
        return core::ptr::null_mut();
    }
    let conn_state = unsafe { drm_atomic_get_new_connector_state(state, connector) };
    if conn_state.is_null() {
        return core::ptr::null_mut();
    }
    unsafe { (*conn_state).crtc }
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_get_connector_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_get_connector_state(
    state: *mut DrmAtomicCommit,
    connector: *mut DrmConnector,
) -> *mut DrmConnectorState {
    if state.is_null() || connector.is_null() {
        return tk_linux_kapi::config::ERR_PTR(-EINVAL);
    }

    let dev = unsafe { (*connector).dev };
    let config = unsafe { core::ptr::addr_of_mut!((*dev).mode_config) };

    let ret = unsafe { drm_modeset_lock(core::ptr::addr_of_mut!((*config).connection_mutex), (*state).acquire_ctx) };
    if ret != 0 {
        return tk_linux_kapi::config::ERR_PTR(ret);
    }

    let index = unsafe { drm_connector_index(connector) };
    if index >= unsafe { (*state).num_connector as usize } {
        let alloc = core::cmp::max(index + 1, unsafe { (*config).num_connector as usize });
        let size = size_of::<DrmConnnectorsState>() * alloc;
        let c = unsafe { krealloc((*state).connectors as *mut c_void, size, GFP_KERNEL) as *mut DrmConnnectorsState };
        if c.is_null() {
            return tk_linux_kapi::config::ERR_PTR(-ENOMEM);
        }

        unsafe {
            (*state).connectors = c;
            let old_num = (*state).num_connector as usize;
            core::ptr::write_bytes((*state).connectors.add(old_num), 0, alloc - old_num);
            (*state).num_connector = alloc as i32;
        }
    }

    let connector_state = unsafe { drm_atomic_get_new_connector_state(state, connector) };
    if !connector_state.is_null() {
        return connector_state;
    }

    let funcs = unsafe { (*connector).funcs };
    if funcs.is_null() || unsafe { (*funcs).atomic_duplicate_state.is_none() } {
        return tk_linux_kapi::config::ERR_PTR(-EINVAL);
    }

    let connector_state = unsafe { ((*funcs).atomic_duplicate_state.unwrap())(connector) };
    if connector_state.is_null() {
        return tk_linux_kapi::config::ERR_PTR(-ENOMEM);
    }

    unsafe {
        drm_connector_get(connector);
        let entry = (*state).connectors.add(index);
        (*entry).state_to_destroy = connector_state;
        (*entry).old_state = (*connector).state;
        (*entry).new_state = connector_state;
        (*entry).ptr = connector;
        (*connector_state).state = state;

        if !(*connector_state).crtc.is_null() {
            let crtc_state = drm_atomic_get_crtc_state(state, (*connector_state).crtc);
            if IS_ERR(crtc_state) {
                return crtc_state.cast();
            }
        }
    }

    connector_state
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_connector_print_state()
unsafe fn drm_atomic_connector_print_state(p: *mut DrmPrinter, state: *const DrmConnectorState) {
    if p.is_null() || state.is_null() {
        return;
    }
    let connector = unsafe { (*state).connector };
    if connector.is_null() {
        return;
    }

    let line = format!("connector[{}]: \n", unsafe { (*connector).base.id });
    unsafe { crate::drm_print_upstream::drm_printf(p, &line) };
    let crtc = unsafe { (*state).crtc };
    let line = format!("\tcrtc={}\n", if !crtc.is_null() { "crtc" } else { "(null)" });
    unsafe { crate::drm_print_upstream::drm_printf(p, &line) };

    let funcs = unsafe { (*connector).funcs };
    if !funcs.is_null() {
        if let Some(print_fn) = unsafe { (*funcs).atomic_print_state } {
            unsafe { print_fn(p, state as *mut DrmConnectorState); }
        }
    }
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_get_bridge_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_get_bridge_state(
    state: *mut DrmAtomicCommit,
    bridge: *mut c_void,
) -> *mut DrmBridgeState {
    if state.is_null() || bridge.is_null() {
        return tk_linux_kapi::config::ERR_PTR(-EINVAL);
    }
    let obj = bridge as *mut DrmPrivateObj;
    let obj_state = unsafe { drm_atomic_get_private_obj_state(state, obj) };
    if IS_ERR(obj_state) {
        return obj_state.cast();
    }
    unsafe { drm_priv_to_bridge_state(obj_state) }
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_get_old_bridge_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_get_old_bridge_state(
    state: *const DrmAtomicCommit,
    bridge: *mut c_void,
) -> *mut DrmBridgeState {
    if state.is_null() || bridge.is_null() {
        return core::ptr::null_mut();
    }
    let obj = bridge as *mut DrmPrivateObj;
    let obj_state = unsafe { drm_atomic_get_old_private_obj_state(state, obj) };
    if obj_state.is_null() {
        return core::ptr::null_mut();
    }
    unsafe { drm_priv_to_bridge_state(obj_state) }
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_get_new_bridge_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_get_new_bridge_state(
    state: *const DrmAtomicCommit,
    bridge: *mut c_void,
) -> *mut DrmBridgeState {
    if state.is_null() || bridge.is_null() {
        return core::ptr::null_mut();
    }
    let obj = bridge as *mut DrmPrivateObj;
    let obj_state = unsafe { drm_atomic_get_new_private_obj_state(state, obj) };
    if obj_state.is_null() {
        return core::ptr::null_mut();
    }
    unsafe { drm_priv_to_bridge_state(obj_state) }
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_add_encoder_bridges()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_add_encoder_bridges(
    _state: *mut DrmAtomicCommit,
    _encoder: *mut DrmEncoder,
) -> i32 {
    // Bridge chain is always empty in TheKernel
    0
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_add_affected_connectors()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_add_affected_connectors(
    state: *mut DrmAtomicCommit,
    crtc: *mut DrmCrtc,
) -> i32 {
    if state.is_null() || crtc.is_null() {
        return -EINVAL;
    }

    let crtc_state = unsafe { drm_atomic_get_crtc_state(state, crtc) };
    if IS_ERR(crtc_state) {
        return PTR_ERR(crtc_state);
    }

    let dev = unsafe { (*state).dev };
    let config = unsafe { core::ptr::addr_of_mut!((*dev).mode_config) };
    let ret = unsafe { drm_modeset_lock(core::ptr::addr_of_mut!((*config).connection_mutex), (*state).acquire_ctx) };
    if ret != 0 {
        return ret;
    }

    let mut conn_iter = core::mem::MaybeUninit::<DrmConnectorListIter>::uninit();
    unsafe {
        drm_connector_list_iter_begin(dev, conn_iter.as_mut_ptr());
        let iter = conn_iter.as_mut_ptr();
        loop {
            let connector = drm_connector_list_iter_next(iter);
            if connector.is_null() {
                break;
            }
            if (unsafe { (*crtc_state).connector_mask } & unsafe { drm_connector_mask(connector) }) == 0 {
                continue;
            }

            let conn_state = drm_atomic_get_connector_state(state, connector);
            if IS_ERR(conn_state) {
                drm_connector_list_iter_end(iter);
                return PTR_ERR(conn_state);
            }
        }
        drm_connector_list_iter_end(iter);
    }

    0
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_add_affected_planes()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_add_affected_planes(
    state: *mut DrmAtomicCommit,
    crtc: *mut DrmCrtc,
) -> i32 {
    if state.is_null() || crtc.is_null() {
        return -EINVAL;
    }

    let old_crtc_state = unsafe { drm_atomic_get_old_crtc_state(state, crtc) };
    if old_crtc_state.is_null() {
        return 0;
    }

    let dev = unsafe { (*state).dev };
    let plane_mask = unsafe { (*old_crtc_state).plane_mask };
    let head = unsafe { core::ptr::addr_of_mut!((*dev).mode_config.plane_list) };
    let mut link = unsafe { (*head).next };

    while link != head {
        let plane = unsafe { (link as *mut u8).sub(core::mem::offset_of!(DrmPlane, head)) as *mut DrmPlane };
        link = unsafe { (*link).next };

        if (plane_mask & (1u32 << unsafe { (*plane).index })) == 0 {
            continue;
        }

        let plane_state = unsafe { drm_atomic_get_plane_state(state, plane) };
        if IS_ERR(plane_state) {
            return PTR_ERR(plane_state);
        }
    }

    0
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_add_affected_colorops()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_add_affected_colorops(
    _state: *mut DrmAtomicCommit,
    _plane: *mut DrmPlane,
) -> i32 {
    0
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_check_only()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_check_only(state: *mut DrmAtomicCommit) -> i32 {
    if state.is_null() {
        return -EINVAL;
    }
    let dev = unsafe { (*state).dev };
    if dev.is_null() {
        return -EINVAL;
    }
    let config = unsafe { core::ptr::addr_of_mut!((*dev).mode_config) };

    unsafe {
        let num_planes = (*config).num_total_plane as usize;
        for i in 0..num_planes {
            if (*state).planes.is_null() {
                break;
            }
            let entry = (*state).planes.add(i);
            if (*entry).ptr.is_null() {
                continue;
            }
            let ret = drm_atomic_plane_check((*entry).old_state, (*entry).new_state);
            if ret != 0 {
                return ret;
            }
        }

        let num_crtcs = (*config).num_crtc as usize;
        for i in 0..num_crtcs {
            if (*state).crtcs.is_null() {
                break;
            }
            let entry = (*state).crtcs.add(i);
            if (*entry).ptr.is_null() {
                continue;
            }
            let ret = drm_atomic_crtc_check((*entry).old_state, (*entry).new_state);
            if ret != 0 {
                return ret;
            }
        }

        let num_conns = (*state).num_connector as usize;
        for i in 0..num_conns {
            if (*state).connectors.is_null() {
                break;
            }
            let entry = (*state).connectors.add(i);
            if (*entry).ptr.is_null() {
                continue;
            }
            let ret = drm_atomic_connector_check((*entry).ptr, (*entry).new_state);
            if ret != 0 {
                return ret;
            }
        }

        let funcs = (*config).funcs;
        if !funcs.is_null() {
            if let Some(check_fn) = (*funcs).atomic_check {
                let ret = check_fn(dev, state);
                if ret != 0 {
                    return ret;
                }
            }
        }

        if !(*state).allow_modeset() {
            for i in 0..num_crtcs {
                if (*state).crtcs.is_null() {
                    break;
                }
                let entry = (*state).crtcs.add(i);
                if (*entry).ptr.is_null() {
                    continue;
                }
                let new_state = (*entry).new_state;
                if drm_atomic_crtc_needs_modeset(new_state) {
                    return -EINVAL;
                }
            }
        }

        (*state).set_checked(true);
    }

    0
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_commit()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_commit(state: *mut DrmAtomicCommit) -> i32 {
    if state.is_null() {
        return -EINVAL;
    }
    let dev = unsafe { (*state).dev };
    if dev.is_null() {
        return -EINVAL;
    }
    let config = unsafe { core::ptr::addr_of_mut!((*dev).mode_config) };

    let ret = unsafe { drm_atomic_check_only(state) };
    if ret != 0 {
        return ret;
    }

    let funcs = unsafe { (*config).funcs };
    if funcs.is_null() || unsafe { (*funcs).atomic_commit.is_none() } {
        return -EOPNOTSUPP;
    }

    unsafe { ((*funcs).atomic_commit.unwrap())(dev, state, false) }
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_nonblocking_commit()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_nonblocking_commit(state: *mut DrmAtomicCommit) -> i32 {
    if state.is_null() {
        return -EINVAL;
    }
    let dev = unsafe { (*state).dev };
    if dev.is_null() {
        return -EINVAL;
    }
    let config = unsafe { core::ptr::addr_of_mut!((*dev).mode_config) };

    let ret = unsafe { drm_atomic_check_only(state) };
    if ret != 0 {
        return ret;
    }

    let funcs = unsafe { (*config).funcs };
    if funcs.is_null() || unsafe { (*funcs).atomic_commit.is_none() } {
        return -EOPNOTSUPP;
    }

    unsafe { ((*funcs).atomic_commit.unwrap())(dev, state, true) }
}

// upstream: drivers/gpu/drm/drm_atomic.c __drm_atomic_helper_disable_plane()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __drm_atomic_helper_disable_plane(
    _plane: *mut DrmPlane,
    plane_state: *mut DrmPlaneState,
) -> i32 {
    if plane_state.is_null() {
        return -EINVAL;
    }

    let ret = unsafe { crate::drm_atomic_uapi_upstream::drm_atomic_set_crtc_for_plane(plane_state, core::ptr::null_mut()) };
    if ret != 0 {
        return ret;
    }

    unsafe {
        crate::drm_atomic_uapi_upstream::drm_atomic_set_fb_for_plane(plane_state, core::ptr::null_mut());
        (*plane_state).crtc_x = 0;
        (*plane_state).crtc_y = 0;
        (*plane_state).crtc_w = 0;
        (*plane_state).crtc_h = 0;
        (*plane_state).src_x = 0;
        (*plane_state).src_y = 0;
        (*plane_state).src_w = 0;
        (*plane_state).src_h = 0;
    }

    0
}

// upstream: drivers/gpu/drm/drm_atomic.c update_output_state()
unsafe fn update_output_state(
    state: *mut DrmAtomicCommit,
    set: *mut DrmModeSet,
) -> i32 {
    let dev = unsafe { (*(*set).crtc).dev };
    let ret = unsafe {
        drm_modeset_lock(core::ptr::addr_of_mut!((*dev).mode_config.connection_mutex), (*state).acquire_ctx)
    };
    if ret != 0 {
        return ret;
    }

    let ret = unsafe { drm_atomic_add_affected_connectors(state, (*set).crtc) };
    if ret != 0 {
        return ret;
    }

    unsafe {
        let num_conns = (*state).num_connector as usize;
        for i in 0..num_conns {
            let entry = (*state).connectors.add(i);
            if (*entry).ptr.is_null() {
                continue;
            }
            let new_conn_state = (*entry).new_state;
            if (*new_conn_state).crtc == (*set).crtc {
                let ret = crate::drm_atomic_uapi_upstream::drm_atomic_set_crtc_for_connector(new_conn_state, core::ptr::null_mut());
                if ret != 0 {
                    return ret;
                }
                (*new_conn_state).link_status = DRM_LINK_STATUS_GOOD;
            }
        }

        let num_set_conns = (*set).num_connectors as usize;
        for i in 0..num_set_conns {
            let conn = *(*set).connectors.add(i);
            let new_conn_state = drm_atomic_get_connector_state(state, conn);
            if IS_ERR(new_conn_state) {
                return PTR_ERR(new_conn_state);
            }
            let ret = crate::drm_atomic_uapi_upstream::drm_atomic_set_crtc_for_connector(new_conn_state, (*set).crtc);
            if ret != 0 {
                return ret;
            }
        }

        let num_crtcs = (*dev).mode_config.num_crtc as usize;
        for i in 0..num_crtcs {
            let entry = (*state).crtcs.add(i);
            let crtc = (*entry).ptr;
            if crtc.is_null() || crtc == (*set).crtc {
                continue;
            }
            let new_crtc_state = (*entry).new_state;
            if (*new_crtc_state).connector_mask == 0 {
                let ret = crate::drm_atomic_uapi_upstream::drm_atomic_set_mode_prop_for_crtc(new_crtc_state, core::ptr::null_mut());
                if ret < 0 {
                    return ret;
                }
                (*new_crtc_state).active = false;
            }
        }
    }

    0
}

// upstream: drivers/gpu/drm/drm_atomic.c __drm_atomic_helper_set_config()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __drm_atomic_helper_set_config(
    set: *mut DrmModeSet,
    state: *mut DrmAtomicCommit,
) -> i32 {
    if set.is_null() || state.is_null() {
        return -EINVAL;
    }
    let crtc = unsafe { (*set).crtc };
    if crtc.is_null() {
        return -EINVAL;
    }

    let crtc_state = unsafe { drm_atomic_get_crtc_state(state, crtc) };
    if IS_ERR(crtc_state) {
        return PTR_ERR(crtc_state);
    }

    let primary = unsafe { (*crtc).primary };
    let primary_state = unsafe { drm_atomic_get_plane_state(state, primary) };
    if IS_ERR(primary_state) {
        return PTR_ERR(primary_state);
    }

    if unsafe { (*set).mode.is_null() } {
        let ret = unsafe { crate::drm_atomic_uapi_upstream::drm_atomic_set_mode_for_crtc(crtc_state, core::ptr::null()) };
        if ret != 0 {
            return ret;
        }
        unsafe {
            (*crtc_state).active = false;
        }

        let ret = unsafe { crate::drm_atomic_uapi_upstream::drm_atomic_set_crtc_for_plane(primary_state, core::ptr::null_mut()) };
        if ret != 0 {
            return ret;
        }

        unsafe {
            crate::drm_atomic_uapi_upstream::drm_atomic_set_fb_for_plane(primary_state, core::ptr::null_mut());
        }

        return unsafe { update_output_state(state, set) };
    }

    let ret = unsafe { crate::drm_atomic_uapi_upstream::drm_atomic_set_mode_for_crtc(crtc_state, (*set).mode) };
    if ret != 0 {
        return ret;
    }
    unsafe {
        (*crtc_state).active = true;
    }

    let ret = unsafe { crate::drm_atomic_uapi_upstream::drm_atomic_set_crtc_for_plane(primary_state, crtc) };
    if ret != 0 {
        return ret;
    }

    let mut hdisplay: i32 = 0;
    let mut vdisplay: i32 = 0;
    unsafe {
        crate::drm_modes_upstream::drm_mode_get_hv_timing((*set).mode, &mut hdisplay, &mut vdisplay);
        crate::drm_atomic_uapi_upstream::drm_atomic_set_fb_for_plane(primary_state, (*set).fb);
        (*primary_state).crtc_x = 0;
        (*primary_state).crtc_y = 0;
        (*primary_state).crtc_w = hdisplay as u32;
        (*primary_state).crtc_h = vdisplay as u32;
        (*primary_state).src_x = ((*set).x as u32) << 16;
        (*primary_state).src_y = ((*set).y as u32) << 16;

        if drm_rotation_90_or_270((*primary_state).rotation) {
            (*primary_state).src_w = (vdisplay as u32) << 16;
            (*primary_state).src_h = (hdisplay as u32) << 16;
        } else {
            (*primary_state).src_w = (hdisplay as u32) << 16;
            (*primary_state).src_h = (vdisplay as u32) << 16;
        }
    }

    unsafe { update_output_state(state, set) }
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_private_obj_print_state()
unsafe fn drm_atomic_private_obj_print_state(p: *mut DrmPrinter, state: *const DrmPrivateState) {
    if p.is_null() || state.is_null() {
        return;
    }
    let obj = unsafe { (*state).obj };
    if obj.is_null() {
        return;
    }
    let funcs = unsafe { (*obj).funcs };
    if !funcs.is_null() {
        if let Some(print_fn) = unsafe { (*funcs).atomic_print_state } {
            unsafe { print_fn(p, state as *mut DrmPrivateState); }
        }
    }
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_print_new_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_print_new_state(
    state: *const DrmAtomicCommit,
    p: *mut DrmPrinter,
) {
    if p.is_null() || state.is_null() {
        return;
    }
    let dev = unsafe { (*state).dev };
    if dev.is_null() {
        return;
    }
    let config = unsafe { core::ptr::addr_of_mut!((*dev).mode_config) };

    unsafe {
        let num_planes = (*config).num_total_plane as usize;
        for i in 0..num_planes {
            if (*state).planes.is_null() {
                break;
            }
            let entry = (*state).planes.add(i);
            if !(*entry).new_state.is_null() {
                drm_atomic_plane_print_state(p, (*entry).new_state);
            }
        }

        let num_crtcs = (*config).num_crtc as usize;
        for i in 0..num_crtcs {
            if (*state).crtcs.is_null() {
                break;
            }
            let entry = (*state).crtcs.add(i);
            if !(*entry).new_state.is_null() {
                drm_atomic_crtc_print_state(p, (*entry).new_state);
            }
        }

        let num_conns = (*state).num_connector as usize;
        for i in 0..num_conns {
            if (*state).connectors.is_null() {
                break;
            }
            let entry = (*state).connectors.add(i);
            if !(*entry).new_state.is_null() {
                drm_atomic_connector_print_state(p, (*entry).new_state);
            }
        }

        let num_priv = (*state).num_private_objs as usize;
        for i in 0..num_priv {
            if (*state).private_objs.is_null() {
                break;
            }
            let entry = (*state).private_objs.add(i);
            if !(*entry).new_state.is_null() {
                drm_atomic_private_obj_print_state(p, (*entry).new_state);
            }
        }
    }
}

// upstream: drivers/gpu/drm/drm_atomic.c __drm_state_dump()
unsafe fn __drm_state_dump(dev: *mut DrmDevice, p: *mut DrmPrinter, take_locks: bool) {
    if dev.is_null() || p.is_null() {
        return;
    }
    if !unsafe { drm_drv_uses_atomic_modeset(dev) } {
        return;
    }
    let config = unsafe { core::ptr::addr_of_mut!((*dev).mode_config) };

    unsafe {
        let plane_head = core::ptr::addr_of_mut!((*config).plane_list);
        let mut link = (*plane_head).next;
        while link != plane_head {
            let plane = (link as *mut u8).sub(core::mem::offset_of!(DrmPlane, head)) as *mut DrmPlane;
            link = (*link).next;

            if take_locks {
                drm_modeset_lock(core::ptr::addr_of_mut!((*plane).mutex), core::ptr::null_mut());
            }
            drm_atomic_plane_print_state(p, (*plane).state);
            if take_locks {
                drm_modeset_unlock(core::ptr::addr_of_mut!((*plane).mutex));
            }
        }

        let crtc_head = core::ptr::addr_of_mut!((*config).crtc_list);
        let mut link = (*crtc_head).next;
        while link != crtc_head {
            let crtc = (link as *mut u8).sub(core::mem::offset_of!(DrmCrtc, head)) as *mut DrmCrtc;
            link = (*link).next;

            if take_locks {
                drm_modeset_lock(core::ptr::addr_of_mut!((*crtc).mutex), core::ptr::null_mut());
            }
            drm_atomic_crtc_print_state(p, (*crtc).state);
            if take_locks {
                drm_modeset_unlock(core::ptr::addr_of_mut!((*crtc).mutex));
            }
        }

        let mut conn_iter = core::mem::MaybeUninit::<DrmConnectorListIter>::uninit();
        drm_connector_list_iter_begin(dev, conn_iter.as_mut_ptr());
        let iter = conn_iter.as_mut_ptr();
        if take_locks {
            drm_modeset_lock(core::ptr::addr_of_mut!((*config).connection_mutex), core::ptr::null_mut());
        }
        loop {
            let connector = drm_connector_list_iter_next(iter);
            if connector.is_null() {
                break;
            }
            drm_atomic_connector_print_state(p, (*connector).state);
        }
        if take_locks {
            drm_modeset_unlock(core::ptr::addr_of_mut!((*config).connection_mutex));
        }
        drm_connector_list_iter_end(iter);

        let priv_head = core::ptr::addr_of_mut!((*config).privobj_list);
        let mut link = (*priv_head).next;
        while link != priv_head {
            let obj = (link as *mut u8).sub(core::mem::offset_of!(DrmPrivateObj, head)) as *mut DrmPrivateObj;
            link = (*link).next;

            if take_locks {
                drm_modeset_lock(core::ptr::addr_of_mut!((*obj).lock), core::ptr::null_mut());
            }
            drm_atomic_private_obj_print_state(p, (*obj).state);
            if take_locks {
                drm_modeset_unlock(core::ptr::addr_of_mut!((*obj).lock));
            }
        }
    }
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_state_dump()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_state_dump(dev: *mut DrmDevice, p: *mut DrmPrinter) {
    unsafe { __drm_state_dump(dev, p, false); }
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_state_info() (CONFIG_DEBUG_FS - omitted)
unsafe fn drm_state_info() -> i32 {
    0
}

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_debugfs_init() (CONFIG_DEBUG_FS - omitted)
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_debugfs_init(_dev: *mut DrmDevice) {}
