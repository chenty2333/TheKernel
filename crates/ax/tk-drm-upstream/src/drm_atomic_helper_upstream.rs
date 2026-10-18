// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_atomic_helper.c` (batch B2b).
//!
//! Faithful translation of Linux DRM atomic helper library. Provides
//! default implementations of check and commit functions, legacy helper
//! emulations, nonblocking commit sequencing, suspend/resume helpers, etc.

#![allow(non_snake_case, non_camel_case_types, non_upper_case_globals, dead_code, unused_variables, unused_mut)]

use core::{ffi::c_void, mem::offset_of};

use crate::drm_atomic_types::{DrmAtomicCommit, DrmCrtcCommit};
use crate::drm_connector_types::{DrmConnector, DrmConnectorListIter, DrmConnectorState};
use crate::uapi_drm_consts::DRM_MODE_CONNECTOR_WRITEBACK;
use crate::drm_crtc_types::{DrmCrtc, DrmCrtcState, DrmModeSet};
use crate::drm_device_types::DrmDevice;
use crate::drm_encoder_types::DrmEncoder;
use crate::drm_framebuffer_types::DrmFramebuffer;
use crate::drm_modes_types::{DrmDisplayMode, DrmModeStatus};
use crate::drm_modeset_helper_vtables_types::{
    DrmConnectorHelperFuncs, DrmCrtcHelperFuncs, DrmEncoderHelperFuncs, DrmModeConfigHelperFuncs,
    DrmPlaneHelperFuncs,
};
use crate::drm_modeset_lock_types::DrmModesetAcquireCtx;
use crate::drm_plane_types::{DrmPlane, DrmPlaneState, DRM_PLANE_TYPE_PRIMARY};
use crate::drm_property_types::DrmPropertyBlob;
use crate::drm_rect_types::DrmRect;
use crate::drm_vblank_types::DrmPendingVblankEvent;
use crate::pending_stubs;

use tk_linux_kapi::completion::Completion;
use tk_linux_kapi::config::{
    EBUSY, EINVAL, ENOMEM, ERANGE, ERR_PTR, GFP_KERNEL, HZ, IS_ERR, PTR_ERR,
};
use tk_linux_kapi::locks::{
    spin_lock_irqsave_raw, spin_lock_raw, spin_unlock_irqrestore_raw, spin_unlock_raw,
};
use tk_linux_kapi::memory::{kfree, kmalloc, kref_get, kref_init, kref_put, kzalloc};
use tk_linux_kapi::primitives::ktime_get;
use tk_linux_kapi::types::{ListHead, WorkStruct};

#[repr(C)]
pub struct DrmColorop {
    pub dev: *mut DrmDevice,
    pub head: ListHead,
    pub index: u32,
    pub _pad: u32,
    pub base: crate::drm_mode_object_types::DrmModeObject,
    pub plane: *mut DrmPlane,
    pub state: *mut DrmColoropState,
}
#[repr(C)]
pub struct DrmColoropState {
    pub colorop: *mut DrmColorop,
    pub bypass: bool,
    pub curve_1d_type: u32,
    pub multiplier: u64,
    pub data: *mut DrmPropertyBlob,
    pub lut1d_interpolation: u32,
    pub lut3d_interpolation: u32,
    pub state: *mut DrmAtomicCommit,
}

pub const DRM_PLANE_COMMIT_ACTIVE_ONLY: u32 = 1 << 0;
pub const DRM_PLANE_COMMIT_NO_DISABLE_AFTER_MODESET: u32 = 1 << 1;
pub const DRM_MODE_DPMS_ON: i32 = 0;
pub const DRM_MODE_DPMS_OFF: i32 = 3;
pub const DRM_MODE_PAGE_FLIP_ASYNC: u32 = 0x02;
const DRIVER_GEM: u32 = 1 << 0;

#[repr(C)]
pub struct DrmWritebackConnector {
    pub base: DrmConnector,
    pub encoder: DrmEncoder,
    pub pixel_formats_blob_ptr: *mut DrmPropertyBlob,
}

#[repr(C)]
pub struct DrmWritebackJob {
    pub connector: *mut DrmWritebackConnector,
    pub prepared: bool,
    pub cleanup_work: WorkStruct,
    pub list_entry: ListHead,
    pub fb: *mut DrmFramebuffer,
    pub out_fence: *mut tk_linux_kapi::types::DmaFence,
    pub priv_data: *mut c_void,
}

#[inline]
unsafe fn drm_core_check_feature(dev: *const DrmDevice, feature: u32) -> bool {
    if dev.is_null() || (*dev).driver.is_null() {
        return false;
    }
    ((*(*dev).driver).driver_features & feature) != 0
}

#[inline]
unsafe fn drm_rect_equals(r1: *const DrmRect, r2: *const DrmRect) -> bool {
    (*r1).x1 == (*r2).x1 && (*r1).x2 == (*r2).x2 && (*r1).y1 == (*r2).y1 && (*r1).y2 == (*r2).y2
}

#[inline]
unsafe fn drm_atomic_crtc_needs_modeset(state: *const DrmCrtcState) -> bool {
    (*state).mode_changed() || (*state).active_changed() || (*state).connectors_changed()
}

#[inline]
unsafe fn drm_atomic_crtc_effectively_active(state: *const DrmCrtcState) -> bool {
    (*state).active || (*state).self_refresh_active
}

#[inline]
unsafe fn drm_atomic_plane_enabling(
    old_plane_state: *const DrmPlaneState,
    new_plane_state: *const DrmPlaneState,
) -> bool {
    (*old_plane_state).crtc.is_null() && !(*new_plane_state).crtc.is_null()
}

#[inline]
unsafe fn drm_atomic_plane_disabling(
    old_plane_state: *const DrmPlaneState,
    new_plane_state: *const DrmPlaneState,
) -> bool {
    !(*old_plane_state).crtc.is_null() && (*new_plane_state).crtc.is_null()
}

unsafe fn drm_crtc_commit_get(commit: *mut DrmCrtcCommit) -> *mut DrmCrtcCommit {
    if !commit.is_null() {
        kref_get(core::ptr::addr_of_mut!((*commit).r#ref));
    }
    commit
}

unsafe fn drm_crtc_commit_put(commit: *mut DrmCrtcCommit) {
    if !commit.is_null() {
        kref_put(
            core::ptr::addr_of_mut!((*commit).r#ref),
            crate::drm_atomic_upstream::__drm_crtc_commit_free,
        );
    }
}

unsafe fn drm_atomic_commit_get(state: *mut DrmAtomicCommit) -> *mut DrmAtomicCommit {
    if !state.is_null() {
        kref_get(core::ptr::addr_of_mut!((*state).r#ref));
    }
    state
}

unsafe fn drm_atomic_commit_put(state: *mut DrmAtomicCommit) {
    if !state.is_null() {
        kref_put(
            core::ptr::addr_of_mut!((*state).r#ref),
            crate::drm_atomic_upstream::__drm_atomic_commit_free,
        );
    }
}

unsafe fn init_commit(commit: *mut DrmCrtcCommit, crtc: *mut DrmCrtc) {
    (*commit).flip_done = Completion::new();
    (*commit).hw_done = Completion::new();
    (*commit).cleanup_done = Completion::new();
    (*commit).commit_entry.next = core::ptr::addr_of_mut!((*commit).commit_entry);
    (*commit).commit_entry.prev = core::ptr::addr_of_mut!((*commit).commit_entry);
    kref_init(core::ptr::addr_of_mut!((*commit).r#ref));
    (*commit).crtc = crtc;
}

unsafe extern "C" fn release_crtc_commit(completion: *mut Completion) {
    // SAFETY: `completion` is the embedded `flip_done` field of a live commit.
    let commit = completion
        .cast::<u8>()
        .sub(offset_of!(DrmCrtcCommit, flip_done))
        .cast::<DrmCrtcCommit>();
    drm_crtc_commit_put(commit);
}

unsafe fn init_work(work: *mut WorkStruct, func: unsafe extern "C" fn(*mut WorkStruct)) {
    (*work).data = 0;
    (*work).entry.next = core::ptr::addr_of_mut!((*work).entry);
    (*work).entry.prev = core::ptr::addr_of_mut!((*work).entry);
    (*work).function = Some(func);
}

unsafe extern "C" {
    fn drm_connector_get_single_encoder(connector: *mut DrmConnector) -> *mut DrmEncoder;
    fn dma_fence_wait(fence: *mut tk_linux_kapi::types::DmaFence, intr: bool) -> i32;
    fn dma_fence_set_deadline(fence: *mut tk_linux_kapi::types::DmaFence, deadline: i64);
    fn dma_fence_put(fence: *mut tk_linux_kapi::types::DmaFence);
}

// =========================================================================
// Section 1: Modeset Checks & Routing
// =========================================================================

// upstream: drm_atomic_helper.c drm_atomic_helper_plane_changed()
unsafe fn drm_atomic_helper_plane_changed(
    state: *mut DrmAtomicCommit,
    old_plane_state: *mut DrmPlaneState,
    plane_state: *mut DrmPlaneState,
    _plane: *mut DrmPlane,
) {
    if !(*old_plane_state).crtc.is_null() {
        let crtc_state = crate::drm_atomic_upstream::drm_atomic_get_new_crtc_state(state, (*old_plane_state).crtc);
        if !crtc_state.is_null() {
            (*crtc_state).set_planes_changed(true);
        }
    }
    if !(*plane_state).crtc.is_null() {
        let crtc_state = crate::drm_atomic_upstream::drm_atomic_get_new_crtc_state(state, (*plane_state).crtc);
        if !crtc_state.is_null() {
            (*crtc_state).set_planes_changed(true);
        }
    }
}

// upstream: drm_atomic_helper.c handle_conflicting_encoders()
unsafe fn handle_conflicting_encoders(
    state: *mut DrmAtomicCommit,
    disable_conflicting_encoders: bool,
) -> i32 {
    let mut encoder_mask = 0u32;
    let num_connectors = (*state).num_connector as usize;

    for i in 0..num_connectors {
        let conn_entry = (*state).connectors.add(i);
        let connector = (*conn_entry).ptr;
        let new_conn_state = (*conn_entry).new_state;
        if new_conn_state.is_null() || (*new_conn_state).crtc.is_null() {
            continue;
        }

        let funcs = (*connector).helper_private as *const DrmConnectorHelperFuncs;
        let mut new_encoder: *mut DrmEncoder = core::ptr::null_mut();

        if !funcs.is_null() && (*funcs).atomic_best_encoder.is_some() {
            new_encoder = ((*funcs).atomic_best_encoder.unwrap())(connector, state);
        } else if !funcs.is_null() && (*funcs).best_encoder.is_some() {
            new_encoder = ((*funcs).best_encoder.unwrap())(connector);
        } else {
            new_encoder = drm_connector_get_single_encoder(connector);
        }

        if !new_encoder.is_null() {
            let mask = 1u32 << (*new_encoder).index;
            if (encoder_mask & mask) != 0 {
                return -EINVAL;
            }
            encoder_mask |= mask;
        }
    }

    if encoder_mask == 0 {
        return 0;
    }

    let dev = (*state).dev;
    let mut conn_iter: DrmConnectorListIter = core::mem::zeroed();
    crate::drm_connector_upstream::drm_connector_list_iter_begin(dev, &mut conn_iter);
    let mut ret = 0;
    loop {
        let connector = crate::drm_connector_upstream::drm_connector_list_iter_next(&mut conn_iter);
        if connector.is_null() {
            break;
        }

        if !crate::drm_atomic_upstream::drm_atomic_get_new_connector_state(state, connector).is_null() {
            continue;
        }

        let encoder = if !(*connector).state.is_null() {
            (*(*connector).state).best_encoder
        } else {
            core::ptr::null_mut()
        };

        if encoder.is_null() || (encoder_mask & (1u32 << (*encoder).index)) == 0 {
            continue;
        }

        if !disable_conflicting_encoders {
            ret = -EINVAL;
            break;
        }

        let new_conn_state = crate::drm_atomic_upstream::drm_atomic_get_connector_state(state, connector);
        if IS_ERR(new_conn_state) {
            ret = PTR_ERR(new_conn_state);
            break;
        }

        let crtc_state = crate::drm_atomic_upstream::drm_atomic_get_new_crtc_state(state, (*new_conn_state).crtc);

        ret = crate::drm_atomic_uapi_upstream::drm_atomic_set_crtc_for_connector(new_conn_state, core::ptr::null_mut());
        if ret != 0 {
            break;
        }

        if !crtc_state.is_null() && (*crtc_state).connector_mask == 0 {
            ret = crate::drm_atomic_uapi_upstream::drm_atomic_set_mode_prop_for_crtc(crtc_state, core::ptr::null_mut());
            if ret < 0 {
                break;
            }
            (*crtc_state).active = false;
        }
    }
    crate::drm_connector_upstream::drm_connector_list_iter_end(&mut conn_iter);
    ret
}

// upstream: drm_atomic_helper.c set_best_encoder()
unsafe fn set_best_encoder(
    state: *mut DrmAtomicCommit,
    conn_state: *mut DrmConnectorState,
    encoder: *mut DrmEncoder,
) {
    if !(*conn_state).best_encoder.is_null() {
        let connector = (*conn_state).connector;
        let crtc = if !connector.is_null() && !(*connector).state.is_null() {
            (*(*connector).state).crtc
        } else {
            core::ptr::null_mut()
        };
        if !crtc.is_null() {
            let crtc_state = crate::drm_atomic_upstream::drm_atomic_get_new_crtc_state(state, crtc);
            if !crtc_state.is_null() {
                (*crtc_state).encoder_mask &= !(1u32 << (*(*conn_state).best_encoder).index);
            }
        }
    }

    if !encoder.is_null() {
        let crtc = (*conn_state).crtc;
        if !crtc.is_null() {
            let crtc_state = crate::drm_atomic_upstream::drm_atomic_get_new_crtc_state(state, crtc);
            if !crtc_state.is_null() {
                (*crtc_state).encoder_mask |= 1u32 << (*encoder).index;
            }
        }
    }

    (*conn_state).best_encoder = encoder;
}

// upstream: drm_atomic_helper.c steal_encoder()
unsafe fn steal_encoder(
    state: *mut DrmAtomicCommit,
    encoder: *mut DrmEncoder,
) {
    let num_connectors = (*state).num_connector as usize;
    for i in 0..num_connectors {
        let conn_entry = (*state).connectors.add(i);
        let old_conn_state = (*conn_entry).old_state;
        let new_conn_state = (*conn_entry).new_state;

        if new_conn_state.is_null() || (*new_conn_state).best_encoder != encoder {
            continue;
        }

        let encoder_crtc = (*old_conn_state).crtc;
        set_best_encoder(state, new_conn_state, core::ptr::null_mut());

        let crtc_state = crate::drm_atomic_upstream::drm_atomic_get_new_crtc_state(state, encoder_crtc);
        if !crtc_state.is_null() {
            (*crtc_state).set_connectors_changed(true);
        }
        return;
    }
}

// upstream: drm_atomic_helper.c update_connector_routing()
unsafe fn update_connector_routing(
    state: *mut DrmAtomicCommit,
    connector: *mut DrmConnector,
    old_connector_state: *mut DrmConnectorState,
    new_connector_state: *mut DrmConnectorState,
    added_by_user: bool,
) -> i32 {
    if (*old_connector_state).crtc != (*new_connector_state).crtc {
        if !(*old_connector_state).crtc.is_null() {
            let crtc_state = crate::drm_atomic_upstream::drm_atomic_get_new_crtc_state(state, (*old_connector_state).crtc);
            if !crtc_state.is_null() {
                (*crtc_state).set_connectors_changed(true);
            }
        }
        if !(*new_connector_state).crtc.is_null() {
            let crtc_state = crate::drm_atomic_upstream::drm_atomic_get_new_crtc_state(state, (*new_connector_state).crtc);
            if !crtc_state.is_null() {
                (*crtc_state).set_connectors_changed(true);
            }
        }
    }

    if (*new_connector_state).crtc.is_null() {
        set_best_encoder(state, new_connector_state, core::ptr::null_mut());
        return 0;
    }

    let crtc_state = crate::drm_atomic_upstream::drm_atomic_get_new_crtc_state(state, (*new_connector_state).crtc);

    let is_unregistered = (*connector).registration_state == crate::drm_connector_types::DRM_CONNECTOR_UNREGISTERED;
    if !(*state).duplicated() && is_unregistered && added_by_user && !crtc_state.is_null() && (*crtc_state).active {
        return -EINVAL;
    }

    let funcs = (*connector).helper_private as *const DrmConnectorHelperFuncs;
    let mut new_encoder: *mut DrmEncoder = core::ptr::null_mut();

    if !funcs.is_null() && (*funcs).atomic_best_encoder.is_some() {
        new_encoder = ((*funcs).atomic_best_encoder.unwrap())(connector, state);
    } else if !funcs.is_null() && (*funcs).best_encoder.is_some() {
        new_encoder = ((*funcs).best_encoder.unwrap())(connector);
    } else {
        new_encoder = drm_connector_get_single_encoder(connector);
    }

    if new_encoder.is_null() {
        return -EINVAL;
    }

    let crtc_mask = 1u32 << (*(*new_connector_state).crtc).index;
    if ((*new_encoder).possible_crtcs & crtc_mask) == 0 {
        return -EINVAL;
    }

    if new_encoder == (*new_connector_state).best_encoder {
        set_best_encoder(state, new_connector_state, new_encoder);
        return 0;
    }

    steal_encoder(state, new_encoder);
    set_best_encoder(state, new_connector_state, new_encoder);
    if !crtc_state.is_null() {
        (*crtc_state).set_connectors_changed(true);
    }
    0
}

// upstream: drm_atomic_helper.c mode_fixup()
unsafe fn mode_fixup(state: *mut DrmAtomicCommit) -> i32 {
    let dev = (*state).dev;
    let num_crtcs = (*dev).mode_config.num_crtc as usize;
    for i in 0..num_crtcs {
        let entry = (*state).crtcs.add(i);
        let new_crtc_state = (*entry).new_state;
        if new_crtc_state.is_null() {
            continue;
        }
        if !(*new_crtc_state).mode_changed() && !(*new_crtc_state).connectors_changed() {
            continue;
        }
        crate::drm_modes_upstream::drm_mode_copy(
            core::ptr::addr_of_mut!((*new_crtc_state).adjusted_mode),
            core::ptr::addr_of!((*new_crtc_state).mode),
        );
    }

    let num_connectors = (*state).num_connector as usize;
    for i in 0..num_connectors {
        let conn_entry = (*state).connectors.add(i);
        let new_conn_state = (*conn_entry).new_state;
        if new_conn_state.is_null() || (*new_conn_state).crtc.is_null() || (*new_conn_state).best_encoder.is_null() {
            continue;
        }

        let new_crtc_state = crate::drm_atomic_upstream::drm_atomic_get_new_crtc_state(state, (*new_conn_state).crtc);
        let encoder = (*new_conn_state).best_encoder;
        let funcs = (*encoder).helper_private as *const DrmEncoderHelperFuncs;

        let bridge = pending_stubs::bridge::drm_bridge_chain_get_first_bridge(encoder as *mut c_void);
        let mut ret = pending_stubs::bridge::drm_atomic_bridge_chain_check(
            bridge,
            new_crtc_state as *mut c_void,
            new_conn_state as *mut c_void,
        );
        pending_stubs::bridge::drm_bridge_put(bridge);
        if ret != 0 {
            return ret;
        }

        if !funcs.is_null() && (*funcs).atomic_check.is_some() {
            ret = ((*funcs).atomic_check.unwrap())(encoder, new_crtc_state, new_conn_state);
            if ret != 0 {
                return ret;
            }
        } else if !funcs.is_null() && (*funcs).mode_fixup.is_some() {
            let ok = ((*funcs).mode_fixup.unwrap())(
                encoder,
                core::ptr::addr_of_mut!((*new_crtc_state).mode),
                core::ptr::addr_of_mut!((*new_crtc_state).adjusted_mode),
            );
            if !ok {
                return -EINVAL;
            }
        }
    }

    for i in 0..num_crtcs {
        let entry = (*state).crtcs.add(i);
        let crtc = (*entry).ptr;
        let new_crtc_state = (*entry).new_state;
        if new_crtc_state.is_null() || !(*new_crtc_state).enable {
            continue;
        }
        if !(*new_crtc_state).mode_changed() && !(*new_crtc_state).connectors_changed() {
            continue;
        }

        let funcs = (*crtc).helper_private as *const DrmCrtcHelperFuncs;
        if funcs.is_null() || (*funcs).mode_fixup.is_none() {
            continue;
        }

        let ok = ((*funcs).mode_fixup.unwrap())(
            crtc,
            core::ptr::addr_of_mut!((*new_crtc_state).mode),
            core::ptr::addr_of_mut!((*new_crtc_state).adjusted_mode),
        );
        if !ok {
            return -EINVAL;
        }
    }

    0
}

// upstream: drm_atomic_helper.c mode_valid_path()
unsafe fn mode_valid_path(
    connector: *mut DrmConnector,
    encoder: *mut DrmEncoder,
    crtc: *mut DrmCrtc,
    mode: *const DrmDisplayMode,
) -> DrmModeStatus {
    let mut ret = pending_stubs::drm_client_probe::drm_encoder_mode_valid(encoder, mode);
    if ret != crate::drm_modes_types::MODE_OK {
        return ret;
    }

    let bridge = pending_stubs::bridge::drm_bridge_chain_get_first_bridge(encoder as *mut c_void);
    ret = pending_stubs::bridge::drm_bridge_chain_mode_valid(
        bridge,
        core::ptr::addr_of!((*connector).display_info) as *const c_void,
        mode as *const c_void,
    );
    pending_stubs::bridge::drm_bridge_put(bridge);
    if ret != crate::drm_modes_types::MODE_OK {
        return ret;
    }

    ret = pending_stubs::drm_client_probe::drm_crtc_mode_valid(crtc, mode);
    ret
}

// upstream: drm_atomic_helper.c mode_valid()
unsafe fn mode_valid(state: *mut DrmAtomicCommit) -> i32 {
    let num_connectors = (*state).num_connector as usize;
    for i in 0..num_connectors {
        let conn_entry = (*state).connectors.add(i);
        let connector = (*conn_entry).ptr;
        let conn_state = (*conn_entry).new_state;
        if conn_state.is_null() {
            continue;
        }
        let encoder = (*conn_state).best_encoder;
        let crtc = (*conn_state).crtc;
        if crtc.is_null() || encoder.is_null() {
            continue;
        }

        let crtc_state = crate::drm_atomic_upstream::drm_atomic_get_new_crtc_state(state, crtc);
        if crtc_state.is_null() {
            continue;
        }
        if !(*crtc_state).mode_changed() && !(*crtc_state).connectors_changed() {
            continue;
        }

        let mode = core::ptr::addr_of!((*crtc_state).mode);
        let status = mode_valid_path(connector, encoder, crtc, mode);
        if status != crate::drm_modes_types::MODE_OK {
            return -EINVAL;
        }
    }
    0
}

// upstream: drm_atomic_helper.c drm_atomic_check_valid_clones()
unsafe fn drm_atomic_check_valid_clones(
    state: *mut DrmAtomicCommit,
    crtc: *mut DrmCrtc,
) -> i32 {
    let dev = (*crtc).dev;
    let crtc_state = crate::drm_atomic_upstream::drm_atomic_get_new_crtc_state(state, crtc);
    let encoder_mask = (*crtc_state).encoder_mask;

    let head = core::ptr::addr_of_mut!((*dev).mode_config.encoder_list);
    let mut cur = (*head).next;
    while cur != head {
        let encoder = (cur as *mut u8).sub(8) as *mut DrmEncoder;
        let mask = 1u32 << (*encoder).index;
        if (encoder_mask & mask) != 0 {
            if (*encoder).possible_clones == 0 {
                cur = (*cur).next;
                continue;
            }
            if (encoder_mask & (*encoder).possible_clones) != encoder_mask {
                return -EINVAL;
            }
        }
        cur = (*cur).next;
    }

    0
}

// upstream: drm_atomic_helper.c drm_atomic_helper_check_modeset()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_check_modeset(
    dev: *mut DrmDevice,
    state: *mut DrmAtomicCommit,
) -> i32 {
    let mut connectors_mask = 0u64;
    let mut user_connectors_mask = 0u64;

    let num_connectors = (*state).num_connector as usize;
    for i in 0..num_connectors {
        user_connectors_mask |= 1u64 << i;
    }

    let num_crtcs = (*dev).mode_config.num_crtc as usize;
    for i in 0..num_crtcs {
        let entry = (*state).crtcs.add(i);
        let crtc = (*entry).ptr;
        let old_crtc_state = (*entry).old_state;
        let new_crtc_state = (*entry).new_state;

        if crtc.is_null() || old_crtc_state.is_null() || new_crtc_state.is_null() {
            continue;
        }

        let has_connectors = (*new_crtc_state).connector_mask != 0;

        if !crate::drm_modes_upstream::drm_mode_equal(
            core::ptr::addr_of!((*old_crtc_state).mode),
            core::ptr::addr_of!((*new_crtc_state).mode),
        ) {
            (*new_crtc_state).set_mode_changed(true);
        }

        if (*old_crtc_state).enable != (*new_crtc_state).enable {
            (*new_crtc_state).set_mode_changed(true);
            (*new_crtc_state).set_connectors_changed(true);
        }

        if (*old_crtc_state).active != (*new_crtc_state).active {
            (*new_crtc_state).set_active_changed(true);
        }

        if (*new_crtc_state).enable != has_connectors {
            return -EINVAL;
        }

        if crate::drm_vblank_upstream::drm_dev_has_vblank(dev) {
            (*new_crtc_state).no_vblank = false;
        } else {
            (*new_crtc_state).no_vblank = true;
        }
    }

    let mut ret = handle_conflicting_encoders(state, false);
    if ret != 0 {
        return ret;
    }

    for i in 0..num_connectors {
        let conn_entry = (*state).connectors.add(i);
        let connector = (*conn_entry).ptr;
        let old_connector_state = (*conn_entry).old_state;
        let new_connector_state = (*conn_entry).new_state;

        if connector.is_null() || old_connector_state.is_null() || new_connector_state.is_null() {
            continue;
        }

        let funcs = (*connector).helper_private as *const DrmConnectorHelperFuncs;

        ret = update_connector_routing(
            state,
            connector,
            old_connector_state,
            new_connector_state,
            ((1u64 << i) & user_connectors_mask) != 0,
        );
        if ret != 0 {
            return ret;
        }

        if !(*old_connector_state).crtc.is_null() {
            let new_crtc_state = crate::drm_atomic_upstream::drm_atomic_get_new_crtc_state(state, (*old_connector_state).crtc);
            if !new_crtc_state.is_null() {
                if (*old_connector_state).link_status != (*new_connector_state).link_status {
                    (*new_crtc_state).set_connectors_changed(true);
                }
                if (*old_connector_state).max_requested_bpc != (*new_connector_state).max_requested_bpc {
                    (*new_crtc_state).set_connectors_changed(true);
                }
            }
        }

        if !funcs.is_null() && (*funcs).atomic_check.is_some() {
            ret = ((*funcs).atomic_check.unwrap())(connector, state);
            if ret != 0 {
                return ret;
            }
        }

        connectors_mask |= 1u64 << i;
    }

    for i in 0..num_crtcs {
        let entry = (*state).crtcs.add(i);
        let crtc = (*entry).ptr;
        let new_crtc_state = (*entry).new_state;

        if crtc.is_null() || new_crtc_state.is_null() {
            continue;
        }

        if !drm_atomic_crtc_needs_modeset(new_crtc_state) {
            continue;
        }

        ret = crate::drm_atomic_upstream::drm_atomic_add_affected_connectors(state, crtc);
        if ret != 0 {
            return ret;
        }

        ret = crate::drm_atomic_upstream::drm_atomic_add_affected_planes(state, crtc);
        if ret != 0 {
            return ret;
        }

        ret = drm_atomic_check_valid_clones(state, crtc);
        if ret != 0 {
            return ret;
        }
    }

    let num_connectors_after = (*state).num_connector as usize;
    for i in 0..num_connectors_after {
        let conn_entry = (*state).connectors.add(i);
        let connector = (*conn_entry).ptr;

        if (connectors_mask & (1u64 << i)) != 0 {
            continue;
        }

        let funcs = (*connector).helper_private as *const DrmConnectorHelperFuncs;
        if !funcs.is_null() && (*funcs).atomic_check.is_some() {
            ret = ((*funcs).atomic_check.unwrap())(connector, state);
            if ret != 0 {
                return ret;
            }
        }
    }

    for i in 0..num_connectors_after {
        let conn_entry = (*state).connectors.add(i);
        let old_conn_state = (*conn_entry).old_state;
        let new_conn_state = (*conn_entry).new_state;

        if !old_conn_state.is_null() && !(*old_conn_state).best_encoder.is_null() {
            ret = crate::drm_atomic_upstream::drm_atomic_add_encoder_bridges(state, (*old_conn_state).best_encoder);
            if ret != 0 {
                return ret;
            }
        }

        if !new_conn_state.is_null() && !(*new_conn_state).best_encoder.is_null() {
            ret = crate::drm_atomic_upstream::drm_atomic_add_encoder_bridges(state, (*new_conn_state).best_encoder);
            if ret != 0 {
                return ret;
            }
        }
    }

    ret = mode_valid(state);
    if ret != 0 {
        return ret;
    }

    ret = mode_fixup(state);
    ret
}

// =========================================================================
// Section 2: Plane Check & Helpers
// =========================================================================

// upstream: drm_atomic_helper.c drm_atomic_helper_check_wb_connector_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_check_wb_connector_state(
    connector: *mut DrmConnector,
    state: *mut DrmAtomicCommit,
) -> i32 {
    let conn_state = crate::drm_atomic_upstream::drm_atomic_get_new_connector_state(state, connector);
    if conn_state.is_null() || (*conn_state).writeback_job.is_null() {
        return 0;
    }

    let wb_job = (*conn_state).writeback_job as *mut DrmWritebackJob;
    if wb_job.is_null() || (*wb_job).fb.is_null() {
        return 0;
    }

    let wb_conn = (*wb_job).connector;
    if wb_conn.is_null() || (*wb_conn).pixel_formats_blob_ptr.is_null() {
        return 0;
    }

    let pixel_format_blob = (*wb_conn).pixel_formats_blob_ptr;
    let nformats = (*pixel_format_blob).length / core::mem::size_of::<u32>();
    let formats = (*pixel_format_blob).data as *const u32;
    let fb = (*wb_job).fb;
    if fb.is_null() || (*fb).format.is_null() {
        return -EINVAL;
    }

    let fb_format = (*(*fb).format).format;
    for i in 0..nformats {
        if fb_format == *formats.add(i) {
            return 0;
        }
    }

    -EINVAL
}

// upstream: drm_atomic_helper.c drm_atomic_helper_check_plane_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_check_plane_state(
    plane_state: *mut DrmPlaneState,
    crtc_state: *const DrmCrtcState,
    min_scale: i32,
    max_scale: i32,
    can_position: bool,
    can_update_disabled: bool,
) -> i32 {
    let fb = (*plane_state).fb;
    let src = core::ptr::addr_of_mut!((*plane_state).src);
    let dst = core::ptr::addr_of_mut!((*plane_state).dst);
    let rotation = (*plane_state).rotation;
    let mut clip = DrmRect { x1: 0, y1: 0, x2: 0, y2: 0 };

    *src = DrmRect {
        x1: (*plane_state).src_x as i32,
        y1: (*plane_state).src_y as i32,
        x2: ((*plane_state).src_x + (*plane_state).src_w) as i32,
        y2: ((*plane_state).src_y + (*plane_state).src_h) as i32,
    };
    *dst = DrmRect {
        x1: (*plane_state).crtc_x,
        y1: (*plane_state).crtc_y,
        x2: (*plane_state).crtc_x + (*plane_state).crtc_w as i32,
        y2: (*plane_state).crtc_y + (*plane_state).crtc_h as i32,
    };

    if fb.is_null() {
        (*plane_state).visible = false;
        return 0;
    }

    if (*plane_state).crtc.is_null() {
        (*plane_state).visible = false;
        return 0;
    }

    if crtc_state.is_null() || (!(*crtc_state).enable && !can_update_disabled) {
        return -EINVAL;
    }

    crate::drm_rect_upstream::drm_rect_rotate(
        src,
        ((*fb).width as i32) << 16,
        ((*fb).height as i32) << 16,
        rotation,
    );

    let hscale = crate::drm_rect_upstream::drm_rect_calc_hscale(src, dst, min_scale, max_scale);
    let vscale = crate::drm_rect_upstream::drm_rect_calc_vscale(src, dst, min_scale, max_scale);
    if hscale < 0 || vscale < 0 {
        return -ERANGE;
    }

    if (*crtc_state).enable {
        crate::drm_modes_upstream::drm_mode_get_hv_timing(
            core::ptr::addr_of!((*crtc_state).mode),
            core::ptr::addr_of_mut!(clip.x2),
            core::ptr::addr_of_mut!(clip.y2),
        );
    }

    (*plane_state).visible = crate::drm_rect_upstream::drm_rect_clip_scaled(src, dst, core::ptr::addr_of!(clip));

    crate::drm_rect_upstream::drm_rect_rotate_inv(
        src,
        ((*fb).width as i32) << 16,
        ((*fb).height as i32) << 16,
        rotation,
    );

    if !(*plane_state).visible {
        return 0;
    }

    if !can_position && !drm_rect_equals(dst, core::ptr::addr_of!(clip)) {
        return -EINVAL;
    }

    0
}

// upstream: drm_atomic_helper.c drm_atomic_helper_check_crtc_primary_plane()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_check_crtc_primary_plane(
    crtc_state: *mut DrmCrtcState,
) -> i32 {
    let crtc = (*crtc_state).crtc;
    let dev = (*crtc).dev;
    let plane_mask = (*crtc_state).plane_mask;

    let head = core::ptr::addr_of_mut!((*dev).mode_config.plane_list);
    let mut cur = (*head).next;
    while cur != head {
        let plane = (cur as *mut u8).sub(8) as *mut DrmPlane;
        let mask = 1u32 << (*plane).index;
        if (plane_mask & mask) != 0 {
            if (*plane).r#type == DRM_PLANE_TYPE_PRIMARY {
                return 0;
            }
        }
        cur = (*cur).next;
    }

    -EINVAL
}

// upstream: drm_atomic_helper.c drm_atomic_helper_check_planes()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_check_planes(
    dev: *mut DrmDevice,
    state: *mut DrmAtomicCommit,
) -> i32 {
    let num_planes = (*dev).mode_config.num_total_plane as usize;
    let mut ret = 0;

    for i in 0..num_planes {
        let entry = (*state).planes.add(i);
        let plane = (*entry).ptr;
        let old_plane_state = (*entry).old_state;
        let new_plane_state = (*entry).new_state;

        if plane.is_null() || old_plane_state.is_null() || new_plane_state.is_null() {
            continue;
        }

        let funcs = (*plane).helper_private as *const DrmPlaneHelperFuncs;

        drm_atomic_helper_plane_changed(state, old_plane_state, new_plane_state, plane);

        crate::drm_damage_helper_upstream::drm_atomic_helper_check_plane_damage(state, new_plane_state);

        if !funcs.is_null() && (*funcs).atomic_check.is_some() {
            ret = ((*funcs).atomic_check.unwrap())(plane, state);
            if ret != 0 {
                return ret;
            }
        }
    }

    let num_crtcs = (*dev).mode_config.num_crtc as usize;
    for i in 0..num_crtcs {
        let entry = (*state).crtcs.add(i);
        let crtc = (*entry).ptr;
        let new_crtc_state = (*entry).new_state;

        if crtc.is_null() || new_crtc_state.is_null() {
            continue;
        }

        let funcs = (*crtc).helper_private as *const DrmCrtcHelperFuncs;
        if !funcs.is_null() && (*funcs).atomic_check.is_some() {
            ret = ((*funcs).atomic_check.unwrap())(crtc, state);
            if ret != 0 {
                return ret;
            }
        }
    }

    ret
}

// upstream: drm_atomic_helper.c drm_atomic_helper_check()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_check(
    dev: *mut DrmDevice,
    state: *mut DrmAtomicCommit,
) -> i32 {
    let mut ret = drm_atomic_helper_check_modeset(dev, state);
    if ret != 0 {
        return ret;
    }

    if (*dev).mode_config.normalize_zpos {
        ret = crate::drm_blend_upstream::drm_atomic_normalize_zpos(dev, state);
        if ret != 0 {
            return ret;
        }
    }

    ret = drm_atomic_helper_check_planes(dev, state);
    if ret != 0 {
        return ret;
    }

    if (*state).legacy_cursor_update() {
        let async_ok = drm_atomic_helper_async_check(dev, state) == 0;
        (*state).set_async_update(async_ok);
    }

    pending_stubs::self_refresh::drm_self_refresh_helper_alter_state(state as *mut c_void);

    ret
}

// =========================================================================
// Section 3: Modeset Commits (Disables, Enables, Set Mode)
// =========================================================================

// upstream: drm_atomic_helper.c crtc_needs_disable()
unsafe fn crtc_needs_disable(
    old_state: *mut DrmCrtcState,
    new_state: *mut DrmCrtcState,
) -> bool {
    if new_state.is_null() {
        return drm_atomic_crtc_effectively_active(old_state);
    }

    if (*old_state).self_refresh_active && (*old_state).crtc != (*new_state).crtc {
        return true;
    }

    (*old_state).active || ((*old_state).self_refresh_active && !(*new_state).active) || (*new_state).self_refresh_active
}

// upstream: drm_atomic_helper.c drm_atomic_helper_commit_encoder_bridge_disable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_commit_encoder_bridge_disable(
    _dev: *mut DrmDevice,
    state: *mut DrmAtomicCommit,
) {
    let num_connectors = (*state).num_connector as usize;
    for i in 0..num_connectors {
        let conn_entry = (*state).connectors.add(i);
        let old_conn_state = (*conn_entry).old_state;
        let new_conn_state = (*conn_entry).new_state;

        if old_conn_state.is_null() || (*old_conn_state).crtc.is_null() {
            continue;
        }

        let old_crtc_state = crate::drm_atomic_upstream::drm_atomic_get_old_crtc_state(state, (*old_conn_state).crtc);
        let new_crtc_state = if !new_conn_state.is_null() && !(*new_conn_state).crtc.is_null() {
            crate::drm_atomic_upstream::drm_atomic_get_new_crtc_state(state, (*new_conn_state).crtc)
        } else {
            core::ptr::null_mut()
        };

        let old_crtc = (*old_conn_state).crtc;
        if !crtc_needs_disable(old_crtc_state, new_crtc_state) || !drm_atomic_crtc_needs_modeset((*old_crtc).state) {
            continue;
        }

        let encoder = (*old_conn_state).best_encoder;
        if encoder.is_null() {
            continue;
        }

        let funcs = (*encoder).helper_private as *const DrmEncoderHelperFuncs;

        let bridge = pending_stubs::bridge::drm_bridge_chain_get_first_bridge(encoder as *mut c_void);
        pending_stubs::bridge::drm_atomic_bridge_chain_disable(bridge, state as *mut c_void);
        pending_stubs::bridge::drm_bridge_put(bridge);

        if !funcs.is_null() {
            if (*funcs).atomic_disable.is_some() {
                ((*funcs).atomic_disable.unwrap())(encoder, state);
            } else if !new_conn_state.is_null() && !(*new_conn_state).crtc.is_null() && (*funcs).prepare.is_some() {
                ((*funcs).prepare.unwrap())(encoder);
            } else if (*funcs).disable.is_some() {
                ((*funcs).disable.unwrap())(encoder);
            } else if (*funcs).dpms.is_some() {
                ((*funcs).dpms.unwrap())(encoder, DRM_MODE_DPMS_OFF);
            }
        }
    }
}

// upstream: drm_atomic_helper.c drm_atomic_helper_commit_crtc_disable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_commit_crtc_disable(
    dev: *mut DrmDevice,
    state: *mut DrmAtomicCommit,
) {
    let num_crtcs = (*dev).mode_config.num_crtc as usize;
    for i in 0..num_crtcs {
        let entry = (*state).crtcs.add(i);
        let crtc = (*entry).ptr;
        let old_crtc_state = (*entry).old_state;
        let new_crtc_state = (*entry).new_state;

        if crtc.is_null() || old_crtc_state.is_null() || new_crtc_state.is_null() {
            continue;
        }

        if !drm_atomic_crtc_needs_modeset(new_crtc_state) {
            continue;
        }

        if !crtc_needs_disable(old_crtc_state, new_crtc_state) {
            continue;
        }

        let funcs = (*crtc).helper_private as *const DrmCrtcHelperFuncs;
        if !funcs.is_null() {
            if (*new_crtc_state).enable && (*funcs).prepare.is_some() {
                ((*funcs).prepare.unwrap())(crtc);
            } else if (*funcs).atomic_disable.is_some() {
                ((*funcs).atomic_disable.unwrap())(crtc, state);
            } else if (*funcs).disable.is_some() {
                ((*funcs).disable.unwrap())(crtc);
            } else if (*funcs).dpms.is_some() {
                ((*funcs).dpms.unwrap())(crtc, DRM_MODE_DPMS_OFF);
            }
        }

        if !crate::drm_vblank_upstream::drm_dev_has_vblank(dev) {
            continue;
        }

        let ret = crate::drm_vblank_upstream::drm_crtc_vblank_get(crtc);
        if ret == 0 {
            crate::drm_vblank_upstream::drm_crtc_vblank_put(crtc);
        }
    }
}

// upstream: drm_atomic_helper.c drm_atomic_helper_commit_encoder_bridge_post_disable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_commit_encoder_bridge_post_disable(
    _dev: *mut DrmDevice,
    state: *mut DrmAtomicCommit,
) {
    let num_connectors = (*state).num_connector as usize;
    for i in 0..num_connectors {
        let conn_entry = (*state).connectors.add(i);
        let old_conn_state = (*conn_entry).old_state;
        let new_conn_state = (*conn_entry).new_state;

        if old_conn_state.is_null() || (*old_conn_state).crtc.is_null() {
            continue;
        }

        let old_crtc_state = crate::drm_atomic_upstream::drm_atomic_get_old_crtc_state(state, (*old_conn_state).crtc);
        let new_crtc_state = if !new_conn_state.is_null() && !(*new_conn_state).crtc.is_null() {
            crate::drm_atomic_upstream::drm_atomic_get_new_crtc_state(state, (*new_conn_state).crtc)
        } else {
            core::ptr::null_mut()
        };

        let old_crtc = (*old_conn_state).crtc;
        if !crtc_needs_disable(old_crtc_state, new_crtc_state) || !drm_atomic_crtc_needs_modeset((*old_crtc).state) {
            continue;
        }

        let encoder = (*old_conn_state).best_encoder;
        if encoder.is_null() {
            continue;
        }

        let bridge = pending_stubs::bridge::drm_bridge_chain_get_first_bridge(encoder as *mut c_void);
        pending_stubs::bridge::drm_atomic_bridge_chain_post_disable(bridge, state as *mut c_void);
        pending_stubs::bridge::drm_bridge_put(bridge);
    }
}

// upstream: drm_atomic_helper.c disable_outputs()
unsafe fn disable_outputs(dev: *mut DrmDevice, state: *mut DrmAtomicCommit) {
    drm_atomic_helper_commit_encoder_bridge_disable(dev, state);
    drm_atomic_helper_commit_encoder_bridge_post_disable(dev, state);
    drm_atomic_helper_commit_crtc_disable(dev, state);
}

// upstream: drm_atomic_helper.c drm_atomic_helper_update_legacy_modeset_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_update_legacy_modeset_state(
    _dev: *mut DrmDevice,
    state: *mut DrmAtomicCommit,
) {
    let num_connectors = (*state).num_connector as usize;
    for i in 0..num_connectors {
        let conn_entry = (*state).connectors.add(i);
        let connector = (*conn_entry).ptr;
        let old_conn_state = (*conn_entry).old_state;
        let new_conn_state = (*conn_entry).new_state;

        if !(*connector).encoder.is_null() {
            (*(*connector).encoder).crtc = core::ptr::null_mut();
            (*connector).encoder = core::ptr::null_mut();
        }

        let crtc = (*new_conn_state).crtc;
        if (crtc.is_null() && !(*old_conn_state).crtc.is_null())
            || (!crtc.is_null() && drm_atomic_crtc_needs_modeset((*crtc).state))
        {
            let mut mode = DRM_MODE_DPMS_OFF;
            if !crtc.is_null() && (*(*crtc).state).active {
                mode = DRM_MODE_DPMS_ON;
            }
            (*connector).dpms = mode;
        }
    }

    for i in 0..num_connectors {
        let conn_entry = (*state).connectors.add(i);
        let connector = (*conn_entry).ptr;
        let new_conn_state = (*conn_entry).new_state;

        if (*new_conn_state).crtc.is_null() {
            continue;
        }
        if (*new_conn_state).best_encoder.is_null() {
            continue;
        }

        (*connector).encoder = (*new_conn_state).best_encoder;
        (*(*connector).encoder).crtc = (*new_conn_state).crtc;
    }

    let dev = (*state).dev;
    let num_crtcs = (*dev).mode_config.num_crtc as usize;
    for i in 0..num_crtcs {
        let entry = (*state).crtcs.add(i);
        let crtc = (*entry).ptr;
        let new_crtc_state = (*entry).new_state;
        if crtc.is_null() || new_crtc_state.is_null() {
            continue;
        }

        let primary = (*crtc).primary;
        crate::drm_modes_upstream::drm_mode_copy(
            core::ptr::addr_of_mut!((*crtc).mode),
            core::ptr::addr_of!((*new_crtc_state).mode),
        );
        (*crtc).enabled = (*new_crtc_state).enable;

        let new_plane_state = crate::drm_atomic_upstream::drm_atomic_get_new_plane_state(state, primary);
        if !new_plane_state.is_null() && (*new_plane_state).crtc == crtc {
            (*crtc).x = ((*new_plane_state).src_x >> 16) as i32;
            (*crtc).y = ((*new_plane_state).src_y >> 16) as i32;
        }
    }
}

// upstream: drm_atomic_helper.c drm_atomic_helper_calc_timestamping_constants()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_calc_timestamping_constants(
    state: *mut DrmAtomicCommit,
) {
    let dev = (*state).dev;
    let num_crtcs = (*dev).mode_config.num_crtc as usize;
    for i in 0..num_crtcs {
        let entry = (*state).crtcs.add(i);
        let crtc = (*entry).ptr;
        let new_crtc_state = (*entry).new_state;
        if crtc.is_null() || new_crtc_state.is_null() {
            continue;
        }

        if (*new_crtc_state).enable {
            crate::drm_vblank_upstream::drm_calc_timestamping_constants(
                crtc,
                core::ptr::addr_of!((*new_crtc_state).adjusted_mode),
            );
        }
    }
}

// upstream: drm_atomic_helper.c drm_atomic_helper_commit_crtc_set_mode()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_commit_crtc_set_mode(
    _dev: *mut DrmDevice,
    state: *mut DrmAtomicCommit,
) {
    let dev = (*state).dev;
    let num_crtcs = (*dev).mode_config.num_crtc as usize;
    for i in 0..num_crtcs {
        let entry = (*state).crtcs.add(i);
        let crtc = (*entry).ptr;
        let new_crtc_state = (*entry).new_state;
        if crtc.is_null() || new_crtc_state.is_null() {
            continue;
        }

        if !(*new_crtc_state).mode_changed() {
            continue;
        }

        let funcs = (*crtc).helper_private as *const DrmCrtcHelperFuncs;
        if (*new_crtc_state).enable && !funcs.is_null() && (*funcs).mode_set_nofb.is_some() {
            ((*funcs).mode_set_nofb.unwrap())(crtc);
        }
    }

    let num_connectors = (*state).num_connector as usize;
    for i in 0..num_connectors {
        let conn_entry = (*state).connectors.add(i);
        let new_conn_state = (*conn_entry).new_state;
        if new_conn_state.is_null() || (*new_conn_state).best_encoder.is_null() {
            continue;
        }

        let encoder = (*new_conn_state).best_encoder;
        let funcs = (*encoder).helper_private as *const DrmEncoderHelperFuncs;
        let new_crtc_state = (*(*new_conn_state).crtc).state;
        let mode = core::ptr::addr_of_mut!((*new_crtc_state).mode);
        let adjusted_mode = core::ptr::addr_of_mut!((*new_crtc_state).adjusted_mode);

        if !(*new_crtc_state).mode_changed() && !(*new_crtc_state).connectors_changed() {
            continue;
        }

        if !funcs.is_null() && (*funcs).atomic_mode_set.is_some() {
            ((*funcs).atomic_mode_set.unwrap())(encoder, new_crtc_state, new_conn_state);
        } else if !funcs.is_null() && (*funcs).mode_set.is_some() {
            ((*funcs).mode_set.unwrap())(encoder, mode, adjusted_mode);
        }

        let bridge = pending_stubs::bridge::drm_bridge_chain_get_first_bridge(encoder as *mut c_void);
        pending_stubs::bridge::drm_bridge_chain_mode_set(bridge, mode as *const c_void, adjusted_mode as *const c_void);
        pending_stubs::bridge::drm_bridge_put(bridge);
    }
}

// upstream: drm_atomic_helper.c drm_atomic_helper_commit_modeset_disables()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_commit_modeset_disables(
    dev: *mut DrmDevice,
    state: *mut DrmAtomicCommit,
) {
    disable_outputs(dev, state);
    drm_atomic_helper_update_legacy_modeset_state(dev, state);
    drm_atomic_helper_calc_timestamping_constants(state);
    drm_atomic_helper_commit_crtc_set_mode(dev, state);
}

// upstream: drm_atomic_helper.c drm_atomic_helper_commit_writebacks()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_commit_writebacks(
    _dev: *mut DrmDevice,
    state: *mut DrmAtomicCommit,
) {
    let num_connectors = (*state).num_connector as usize;
    for i in 0..num_connectors {
        let conn_entry = (*state).connectors.add(i);
        let connector = (*conn_entry).ptr;
        let new_conn_state = (*conn_entry).new_state;
        if connector.is_null() || new_conn_state.is_null() {
            continue;
        }

        let funcs = (*connector).helper_private as *const DrmConnectorHelperFuncs;
        if funcs.is_null() || (*funcs).atomic_commit.is_none() {
            continue;
        }

        let wb_job = (*new_conn_state).writeback_job as *mut DrmWritebackJob;
        if !wb_job.is_null() && !(*wb_job).fb.is_null() {
            ((*funcs).atomic_commit.unwrap())(connector, state);
        }
    }
}

// upstream: drm_atomic_helper.c drm_atomic_helper_commit_encoder_bridge_pre_enable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_commit_encoder_bridge_pre_enable(
    _dev: *mut DrmDevice,
    state: *mut DrmAtomicCommit,
) {
    let num_connectors = (*state).num_connector as usize;
    for i in 0..num_connectors {
        let conn_entry = (*state).connectors.add(i);
        let new_conn_state = (*conn_entry).new_state;
        if new_conn_state.is_null() || (*new_conn_state).best_encoder.is_null() {
            continue;
        }

        let crtc = (*new_conn_state).crtc;
        if crtc.is_null() || !(*(*crtc).state).active || !drm_atomic_crtc_needs_modeset((*crtc).state) {
            continue;
        }

        let encoder = (*new_conn_state).best_encoder;
        let bridge = pending_stubs::bridge::drm_bridge_chain_get_first_bridge(encoder as *mut c_void);
        pending_stubs::bridge::drm_atomic_bridge_chain_pre_enable(bridge, state as *mut c_void);
        pending_stubs::bridge::drm_bridge_put(bridge);
    }
}

// upstream: drm_atomic_helper.c drm_atomic_helper_commit_crtc_enable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_commit_crtc_enable(
    dev: *mut DrmDevice,
    state: *mut DrmAtomicCommit,
) {
    let num_crtcs = (*dev).mode_config.num_crtc as usize;
    for i in 0..num_crtcs {
        let entry = (*state).crtcs.add(i);
        let crtc = (*entry).ptr;
        let new_crtc_state = (*entry).new_state;
        if crtc.is_null() || new_crtc_state.is_null() {
            continue;
        }

        if !drm_atomic_crtc_needs_modeset(new_crtc_state) {
            continue;
        }
        if !(*new_crtc_state).active {
            continue;
        }

        let funcs = (*crtc).helper_private as *const DrmCrtcHelperFuncs;
        if (*new_crtc_state).enable && !funcs.is_null() {
            if (*funcs).atomic_enable.is_some() {
                ((*funcs).atomic_enable.unwrap())(crtc, state);
            } else if (*funcs).commit.is_some() {
                ((*funcs).commit.unwrap())(crtc);
            }
        }
    }
}

// upstream: drm_atomic_helper.c drm_atomic_helper_commit_encoder_bridge_enable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_commit_encoder_bridge_enable(
    _dev: *mut DrmDevice,
    state: *mut DrmAtomicCommit,
) {
    let num_connectors = (*state).num_connector as usize;
    for i in 0..num_connectors {
        let conn_entry = (*state).connectors.add(i);
        let new_conn_state = (*conn_entry).new_state;
        if new_conn_state.is_null() || (*new_conn_state).best_encoder.is_null() {
            continue;
        }

        let crtc = (*new_conn_state).crtc;
        if crtc.is_null() || !(*(*crtc).state).active || !drm_atomic_crtc_needs_modeset((*crtc).state) {
            continue;
        }

        let encoder = (*new_conn_state).best_encoder;
        let funcs = (*encoder).helper_private as *const DrmEncoderHelperFuncs;
        let bridge = pending_stubs::bridge::drm_bridge_chain_get_first_bridge(encoder as *mut c_void);

        if !funcs.is_null() {
            if (*funcs).atomic_enable.is_some() {
                ((*funcs).atomic_enable.unwrap())(encoder, state);
            } else if (*funcs).enable.is_some() {
                ((*funcs).enable.unwrap())(encoder);
            } else if (*funcs).commit.is_some() {
                ((*funcs).commit.unwrap())(encoder);
            }
        }

        pending_stubs::bridge::drm_atomic_bridge_chain_enable(bridge, state as *mut c_void);
        pending_stubs::bridge::drm_bridge_put(bridge);
    }
}

// upstream: drm_atomic_helper.c drm_atomic_helper_commit_modeset_enables()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_commit_modeset_enables(
    dev: *mut DrmDevice,
    state: *mut DrmAtomicCommit,
) {
    drm_atomic_helper_commit_crtc_enable(dev, state);
    drm_atomic_helper_commit_encoder_bridge_pre_enable(dev, state);
    drm_atomic_helper_commit_encoder_bridge_enable(dev, state);
    drm_atomic_helper_commit_writebacks(dev, state);
}

// =========================================================================
// Section 4: Fences, Waits & Commit Tail
// =========================================================================

// upstream: drm_atomic_helper.c set_fence_deadline()
unsafe fn set_fence_deadline(
    _dev: *mut DrmDevice,
    state: *mut DrmAtomicCommit,
) {
    let dev = (*state).dev;
    let num_crtcs = (*dev).mode_config.num_crtc as usize;
    let mut vbltime: i64 = 0;

    for i in 0..num_crtcs {
        let entry = (*state).crtcs.add(i);
        let crtc = (*entry).ptr;
        let new_crtc_state = (*entry).new_state;
        if crtc.is_null() || new_crtc_state.is_null() {
            continue;
        }

        if drm_atomic_crtc_needs_modeset(new_crtc_state) {
            continue;
        }
        if !(*new_crtc_state).active {
            continue;
        }

        let mut v: i64 = 0;
        if crate::drm_vblank_upstream::drm_crtc_next_vblank_start(crtc, &mut v) != 0 {
            continue;
        }

        if vbltime == 0 || v < vbltime {
            vbltime = v;
        }
    }

    if vbltime == 0 {
        return;
    }

    let num_planes = (*dev).mode_config.num_total_plane as usize;
    for i in 0..num_planes {
        let entry = (*state).planes.add(i);
        let new_plane_state = (*entry).new_state;
        if new_plane_state.is_null() || (*new_plane_state).fence.is_null() {
            continue;
        }

        dma_fence_set_deadline((*new_plane_state).fence, vbltime);
    }
}

// upstream: drm_atomic_helper.c drm_atomic_helper_wait_for_fences()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_wait_for_fences(
    dev: *mut DrmDevice,
    state: *mut DrmAtomicCommit,
    pre_swap: bool,
) -> i32 {
    set_fence_deadline(dev, state);

    let num_planes = (*dev).mode_config.num_total_plane as usize;
    for i in 0..num_planes {
        let entry = (*state).planes.add(i);
        let new_plane_state = (*entry).new_state;
        if new_plane_state.is_null() || (*new_plane_state).fence.is_null() {
            continue;
        }

        let ret = dma_fence_wait((*new_plane_state).fence, pre_swap);
        if ret != 0 {
            return ret;
        }

        dma_fence_put((*new_plane_state).fence);
        (*new_plane_state).fence = core::ptr::null_mut();
    }

    0
}

// upstream: drm_atomic_helper.c drm_atomic_helper_wait_for_vblanks()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_wait_for_vblanks(
    dev: *mut DrmDevice,
    state: *mut DrmAtomicCommit,
) {
    if (*state).legacy_cursor_update() {
        return;
    }

    let num_crtcs = (*dev).mode_config.num_crtc as usize;
    let mut crtc_mask = 0u32;

    for i in 0..num_crtcs {
        let entry = (*state).crtcs.add(i);
        let crtc = (*entry).ptr;
        let new_crtc_state = (*entry).new_state;
        if crtc.is_null() || new_crtc_state.is_null() || !(*new_crtc_state).active {
            continue;
        }

        let ret = crate::drm_vblank_upstream::drm_crtc_vblank_get(crtc);
        if ret != 0 {
            continue;
        }

        crtc_mask |= 1u32 << (*crtc).index;
        (*entry).last_vblank_count = crate::drm_vblank_upstream::drm_crtc_vblank_count(crtc);
    }

    for i in 0..num_crtcs {
        let entry = (*state).crtcs.add(i);
        let crtc = (*entry).ptr;
        if crtc.is_null() {
            continue;
        }

        if (crtc_mask & (1u32 << (*crtc).index)) == 0 {
            continue;
        }

        let queue = crate::drm_vblank_upstream::drm_crtc_vblank_waitqueue(crtc);
        if !queue.is_null() {
            let last = (*entry).last_vblank_count;
            let _ = tk_linux_kapi::wait_event_timeout!(
                *queue,
                last != crate::drm_vblank_upstream::drm_crtc_vblank_count(crtc),
                tk_linux_kapi::primitives::msecs_to_jiffies(1000u32) as i64,
            );
        }

        crate::drm_vblank_upstream::drm_crtc_vblank_put(crtc);
    }
}

// upstream: drm_atomic_helper.c drm_atomic_helper_wait_for_flip_done()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_wait_for_flip_done(
    dev: *mut DrmDevice,
    state: *mut DrmAtomicCommit,
) {
    let num_crtcs = (*dev).mode_config.num_crtc as usize;
    for i in 0..num_crtcs {
        let entry = (*state).crtcs.add(i);
        let commit = (*entry).commit;
        let crtc = (*entry).ptr;
        if crtc.is_null() || commit.is_null() {
            continue;
        }

        let _ = (*commit).flip_done.wait_timeout(10 * HZ as i64);
    }

    if !(*state).fake_commit.is_null() {
        (*(*state).fake_commit).flip_done.complete_all();
    }
}

// upstream: drm_atomic_helper.c drm_atomic_helper_commit_tail()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_commit_tail(state: *mut DrmAtomicCommit) {
    let dev = (*state).dev;

    drm_atomic_helper_commit_modeset_disables(dev, state);
    drm_atomic_helper_commit_planes(dev, state, 0);
    drm_atomic_helper_commit_modeset_enables(dev, state);
    drm_atomic_helper_fake_vblank(state);
    drm_atomic_helper_commit_hw_done(state);
    drm_atomic_helper_wait_for_vblanks(dev, state);
    drm_atomic_helper_cleanup_planes(dev, state);
}

// upstream: drm_atomic_helper.c drm_atomic_helper_commit_tail_rpm()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_commit_tail_rpm(state: *mut DrmAtomicCommit) {
    let dev = (*state).dev;

    drm_atomic_helper_commit_modeset_disables(dev, state);
    drm_atomic_helper_commit_modeset_enables(dev, state);
    drm_atomic_helper_commit_planes(dev, state, DRM_PLANE_COMMIT_ACTIVE_ONLY);
    drm_atomic_helper_fake_vblank(state);
    drm_atomic_helper_commit_hw_done(state);
    drm_atomic_helper_wait_for_vblanks(dev, state);
    drm_atomic_helper_cleanup_planes(dev, state);
}

// upstream: drm_atomic_helper.c commit_tail()
unsafe fn commit_tail(state: *mut DrmAtomicCommit) {
    let dev = (*state).dev;
    let ret = drm_atomic_helper_wait_for_fences(dev, state, false);
    if ret != 0 {
        drm_atomic_helper_commit_error(state, ret);
    } else {
        let funcs = (*dev).mode_config.helper_private as *const DrmModeConfigHelperFuncs;
        let mut new_self_refresh_mask = 0u32;
        let start = ktime_get();

        drm_atomic_helper_wait_for_dependencies(state);

        let num_crtcs = (*dev).mode_config.num_crtc as usize;
        for i in 0..num_crtcs {
            let entry = (*state).crtcs.add(i);
            let new_crtc_state = (*entry).new_state;
            if !new_crtc_state.is_null() && (*new_crtc_state).self_refresh_active {
                new_self_refresh_mask |= 1u32 << i;
            }
        }

        if !funcs.is_null() && (*funcs).atomic_commit_tail.is_some() {
            ((*funcs).atomic_commit_tail.unwrap())(state);
        } else {
            drm_atomic_helper_commit_tail(state);
        }

        let end = ktime_get();
        let commit_time_ms = (end - start) / 1_000_000;
        if commit_time_ms > 0 {
            pending_stubs::self_refresh::drm_self_refresh_helper_update_avg_times(
                state as *mut c_void,
                commit_time_ms as u32,
                new_self_refresh_mask,
            );
        }
    }

    drm_atomic_helper_commit_cleanup_done(state);
    drm_atomic_commit_put(state);
}

// upstream: drm_atomic_helper.c commit_work()
unsafe extern "C" fn commit_work(work: *mut WorkStruct) {
    // SAFETY: `work` is the embedded `commit_work` field of a live state.
    let state = work
        .cast::<u8>()
        .sub(offset_of!(DrmAtomicCommit, commit_work))
        .cast::<DrmAtomicCommit>();
    commit_tail(state);
}

// upstream: drm_atomic_helper.c drm_atomic_helper_async_check()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_async_check(
    _dev: *mut DrmDevice,
    state: *mut DrmAtomicCommit,
) -> i32 {
    let dev = (*state).dev;
    let num_crtcs = (*dev).mode_config.num_crtc as usize;
    for i in 0..num_crtcs {
        let entry = (*state).crtcs.add(i);
        let crtc_state = (*entry).new_state;
        if !crtc_state.is_null() && drm_atomic_crtc_needs_modeset(crtc_state) {
            return -EINVAL;
        }
    }

    let num_planes = (*dev).mode_config.num_total_plane as usize;
    let mut n_planes = 0;
    let mut target_plane: *mut DrmPlane = core::ptr::null_mut();
    let mut target_old_plane_state: *mut DrmPlaneState = core::ptr::null_mut();
    let mut target_new_plane_state: *mut DrmPlaneState = core::ptr::null_mut();

    for i in 0..num_planes {
        let entry = (*state).planes.add(i);
        let plane = (*entry).ptr;
        let old_plane_state = (*entry).old_state;
        let new_plane_state = (*entry).new_state;
        if !plane.is_null() && !old_plane_state.is_null() && !new_plane_state.is_null() {
            n_planes += 1;
            target_plane = plane;
            target_old_plane_state = old_plane_state;
            target_new_plane_state = new_plane_state;
        }
    }

    if n_planes != 1 {
        return -EINVAL;
    }

    if (*target_new_plane_state).crtc.is_null()
        || (*target_old_plane_state).crtc != (*target_new_plane_state).crtc
    {
        return -EINVAL;
    }

    let funcs = (*target_plane).helper_private as *const DrmPlaneHelperFuncs;
    if funcs.is_null() || (*funcs).atomic_async_update.is_none() {
        return -EINVAL;
    }

    if !(*target_new_plane_state).fence.is_null() {
        return -EINVAL;
    }

    if !(*target_old_plane_state).commit.is_null()
        && !(*(*target_old_plane_state).commit).hw_done.try_wait()
    {
        return -EBUSY;
    }

    if !funcs.is_null() && (*funcs).atomic_async_check.is_some() {
        return ((*funcs).atomic_async_check.unwrap())(target_plane, state, false);
    }

    0
}

// upstream: drm_atomic_helper.c drm_atomic_helper_async_commit()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_async_commit(
    _dev: *mut DrmDevice,
    state: *mut DrmAtomicCommit,
) {
    let dev = (*state).dev;
    let num_planes = (*dev).mode_config.num_total_plane as usize;
    for i in 0..num_planes {
        let entry = (*state).planes.add(i);
        let plane = (*entry).ptr;
        let plane_state = (*entry).new_state;
        if plane.is_null() || plane_state.is_null() {
            continue;
        }

        let funcs = (*plane).helper_private as *const DrmPlaneHelperFuncs;
        if !funcs.is_null() && (*funcs).atomic_async_update.is_some() {
            ((*funcs).atomic_async_update.unwrap())(plane, state);
        }
    }
}

// upstream: drm_atomic_helper.c drm_atomic_helper_commit()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_commit(
    dev: *mut DrmDevice,
    state: *mut DrmAtomicCommit,
    nonblock: bool,
) -> i32 {
    if (*state).async_update() {
        let mut ret = drm_atomic_helper_prepare_planes(dev, state);
        if ret != 0 {
            return ret;
        }

        drm_atomic_helper_async_commit(dev, state);
        drm_atomic_helper_unprepare_planes(dev, state);
        return 0;
    }

    let mut ret = drm_atomic_helper_setup_commit(state, nonblock);
    if ret != 0 {
        return ret;
    }

    init_work(core::ptr::addr_of_mut!((*state).commit_work), commit_work);

    ret = drm_atomic_helper_prepare_planes(dev, state);
    if ret != 0 {
        return ret;
    }

    if !nonblock {
        ret = drm_atomic_helper_wait_for_fences(dev, state, true);
        if ret != 0 {
            drm_atomic_helper_unprepare_planes(dev, state);
            return ret;
        }
    }

    // Keep commits for one object in hardware-submission order. For a
    // blocking commit this wait also leaves the pre-swap callback immediately
    // adjacent to the state publication below.
    ret = wait_for_hw_done(state);
    if ret != 0 {
        drm_atomic_helper_unprepare_planes(dev, state);
        return ret;
    }

    if !nonblock {
        let funcs = (*dev).mode_config.helper_private as *const DrmModeConfigHelperFuncs;
        if !funcs.is_null() && (*funcs).atomic_commit_pre_swap.is_some() {
            ret = ((*funcs).atomic_commit_pre_swap.unwrap())(state);
            if ret != 0 {
                drm_atomic_helper_unprepare_planes(dev, state);
                return ret;
            }
        }
    }

    ret = drm_atomic_helper_swap_state(state, false);
    if ret != 0 {
        drm_atomic_helper_unprepare_planes(dev, state);
        return ret;
    }

    drm_atomic_commit_get(state);
    if nonblock {
        tk_linux_kapi::workqueue::queue_work(
            tk_linux_kapi::workqueue::system_dfl_wq,
            core::ptr::addr_of_mut!((*state).commit_work),
        );
    } else {
        commit_tail(state);
    }

    0
}

// =========================================================================
// Section 5: Setup Commit, Dependencies & Signal Completion
// =========================================================================

// upstream: drm_atomic_helper.c stall_checks()
unsafe fn stall_checks(crtc: *mut DrmCrtc, nonblock: bool) -> i32 {
    let mut stall_commit: *mut DrmCrtcCommit = core::ptr::null_mut();
    let mut completed = true;
    let mut i = 0;
    let mut ret: i64 = 0;

    spin_lock_raw(core::ptr::addr_of_mut!((*crtc).commit_lock));
    let head = core::ptr::addr_of_mut!((*crtc).commit_list);
    let mut cur = (*head).next;
    while cur != head {
        let commit = cur
            .cast::<u8>()
            .sub(offset_of!(DrmCrtcCommit, commit_entry))
            .cast::<DrmCrtcCommit>();
        if i == 0 {
            completed = (*commit).flip_done.try_wait();
            if !completed && nonblock {
                spin_unlock_raw(core::ptr::addr_of_mut!((*crtc).commit_lock));
                return -EBUSY;
            }
        } else if i == 1 {
            stall_commit = drm_crtc_commit_get(commit);
            break;
        }
        i += 1;
        cur = (*cur).next;
    }
    spin_unlock_raw(core::ptr::addr_of_mut!((*crtc).commit_lock));

    if stall_commit.is_null() {
        return 0;
    }

    ret = (*stall_commit).cleanup_done.wait_interruptible_timeout(10 * HZ as i64);
    drm_crtc_commit_put(stall_commit);

    if ret < 0 {
        ret as i32
    } else {
        0
    }
}

// upstream: drm_atomic_helper.c crtc_or_fake_commit()
unsafe fn crtc_or_fake_commit(
    state: *mut DrmAtomicCommit,
    crtc: *mut DrmCrtc,
) -> *mut DrmCrtcCommit {
    if !crtc.is_null() {
        let new_crtc_state = crate::drm_atomic_upstream::drm_atomic_get_new_crtc_state(state, crtc);
        if !new_crtc_state.is_null() {
            return (*new_crtc_state).commit;
        }
    }

    if (*state).fake_commit.is_null() {
        let fake = tk_linux_kapi::memory::kzalloc(
            core::mem::size_of::<DrmCrtcCommit>(),
            GFP_KERNEL,
        ) as *mut DrmCrtcCommit;
        if fake.is_null() {
            return core::ptr::null_mut();
        }
        init_commit(fake, core::ptr::null_mut());
        (*state).fake_commit = fake;
    }

    (*state).fake_commit
}

// upstream: drm_atomic_helper.c drm_atomic_helper_setup_commit()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_setup_commit(
    state: *mut DrmAtomicCommit,
    nonblock: bool,
) -> i32 {
    let dev = (*state).dev;
    let num_crtcs = (*dev).mode_config.num_crtc as usize;
    let funcs = (*dev).mode_config.helper_private as *const DrmModeConfigHelperFuncs;

    for i in 0..num_crtcs {
        let entry = (*state).crtcs.add(i);
        let crtc = (*entry).ptr;
        let old_crtc_state = (*entry).old_state;
        let new_crtc_state = (*entry).new_state;

        if crtc.is_null() || old_crtc_state.is_null() || new_crtc_state.is_null() {
            continue;
        }

        let commit = tk_linux_kapi::memory::kzalloc(
            core::mem::size_of::<DrmCrtcCommit>(),
            GFP_KERNEL,
        ) as *mut DrmCrtcCommit;
        if commit.is_null() {
            return -ENOMEM;
        }

        init_commit(commit, crtc);
        (*new_crtc_state).commit = commit;

        let ret = stall_checks(crtc, nonblock);
        if ret != 0 {
            return ret;
        }

        if !(*old_crtc_state).active && !(*new_crtc_state).active {
            (*commit).flip_done.complete_all();
            continue;
        }

        if (*state).legacy_cursor_update() {
            (*commit).flip_done.complete_all();
            continue;
        }

        if (*new_crtc_state).event.is_null() {
            (*commit).event = tk_linux_kapi::memory::kzalloc(
                core::mem::size_of::<DrmPendingVblankEvent>(),
                GFP_KERNEL,
            ) as *mut DrmPendingVblankEvent;
            if (*commit).event.is_null() {
                return -ENOMEM;
            }
            (*new_crtc_state).event = (*commit).event;
        }

        (*(*new_crtc_state).event).base.completion = core::ptr::addr_of_mut!((*commit).flip_done);
        (*(*new_crtc_state).event).base.completion_release = Some(release_crtc_commit);
        drm_crtc_commit_get(commit);

        (*commit).abort_completion = true;

        (*entry).commit = commit;
        drm_crtc_commit_get(commit);
    }

    let num_connectors = (*state).num_connector as usize;
    for i in 0..num_connectors {
        let conn_entry = (*state).connectors.add(i);
        let old_conn_state = (*conn_entry).old_state;
        let new_conn_state = (*conn_entry).new_state;
        if old_conn_state.is_null() || new_conn_state.is_null() {
            continue;
        }

        if nonblock && !(*old_conn_state).commit.is_null() && !(*(*old_conn_state).commit).flip_done.try_wait() {
            return -EBUSY;
        }

        let target_crtc = if !(*new_conn_state).crtc.is_null() {
            (*new_conn_state).crtc
        } else {
            (*old_conn_state).crtc
        };
        let commit = crtc_or_fake_commit(state, target_crtc);
        if commit.is_null() {
            return -ENOMEM;
        }
        (*new_conn_state).commit = drm_crtc_commit_get(commit);
    }

    let num_planes = (*dev).mode_config.num_total_plane as usize;
    for i in 0..num_planes {
        let entry = (*state).planes.add(i);
        let old_plane_state = (*entry).old_state;
        let new_plane_state = (*entry).new_state;
        if old_plane_state.is_null() || new_plane_state.is_null() {
            continue;
        }

        if nonblock && !(*old_plane_state).commit.is_null() && !(*(*old_plane_state).commit).flip_done.try_wait() {
            return -EBUSY;
        }

        let target_crtc = if !(*new_plane_state).crtc.is_null() {
            (*new_plane_state).crtc
        } else {
            (*old_plane_state).crtc
        };
        let commit = crtc_or_fake_commit(state, target_crtc);
        if commit.is_null() {
            return -ENOMEM;
        }
        (*new_plane_state).commit = drm_crtc_commit_get(commit);
    }

    if !funcs.is_null() && (*funcs).atomic_commit_setup.is_some() {
        return ((*funcs).atomic_commit_setup.unwrap())(state);
    }

    0
}

// upstream: drm_atomic_helper.c drm_atomic_helper_wait_for_dependencies()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_wait_for_dependencies(
    state: *mut DrmAtomicCommit,
) {
    let dev = (*state).dev;
    let num_crtcs = (*dev).mode_config.num_crtc as usize;
    for i in 0..num_crtcs {
        let entry = (*state).crtcs.add(i);
        let old_crtc_state = (*entry).old_state;
        if !old_crtc_state.is_null() && !(*old_crtc_state).commit.is_null() {
            let _ = crate::drm_atomic_upstream::drm_crtc_commit_wait((*old_crtc_state).commit);
        }
    }

    let num_connectors = (*state).num_connector as usize;
    for i in 0..num_connectors {
        let conn_entry = (*state).connectors.add(i);
        let old_conn_state = (*conn_entry).old_state;
        if !old_conn_state.is_null() && !(*old_conn_state).commit.is_null() {
            let _ = crate::drm_atomic_upstream::drm_crtc_commit_wait((*old_conn_state).commit);
        }
    }

    let num_planes = (*dev).mode_config.num_total_plane as usize;
    for i in 0..num_planes {
        let entry = (*state).planes.add(i);
        let old_plane_state = (*entry).old_state;
        if !old_plane_state.is_null() && !(*old_plane_state).commit.is_null() {
            let _ = crate::drm_atomic_upstream::drm_crtc_commit_wait((*old_plane_state).commit);
        }
    }
}

// upstream: drm_atomic_helper.c drm_atomic_helper_fake_vblank()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_fake_vblank(state: *mut DrmAtomicCommit) {
    let dev = (*state).dev;
    let num_crtcs = (*dev).mode_config.num_crtc as usize;
    for i in 0..num_crtcs {
        let entry = (*state).crtcs.add(i);
        let crtc = (*entry).ptr;
        let new_crtc_state = (*entry).new_state;

        if crtc.is_null() || new_crtc_state.is_null() || !(*new_crtc_state).no_vblank {
            continue;
        }

        let mut flags: core::ffi::c_ulong = 0;
        spin_lock_irqsave_raw(
            core::ptr::addr_of_mut!((*dev).event_lock),
            &mut flags,
        );
        if !(*new_crtc_state).event.is_null() {
            crate::drm_vblank_upstream::drm_crtc_send_vblank_event(crtc, (*new_crtc_state).event);
            (*new_crtc_state).event = core::ptr::null_mut();
        }
        spin_unlock_irqrestore_raw(
            core::ptr::addr_of_mut!((*dev).event_lock),
            flags,
        );
    }
}

// upstream: drm_atomic_helper.c drm_atomic_helper_commit_hw_done()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_commit_hw_done(state: *mut DrmAtomicCommit) {
    let dev = (*state).dev;
    let num_crtcs = (*dev).mode_config.num_crtc as usize;
    for i in 0..num_crtcs {
        let entry = (*state).crtcs.add(i);
        let old_crtc_state = (*entry).old_state;
        let new_crtc_state = (*entry).new_state;

        if new_crtc_state.is_null() || (*new_crtc_state).commit.is_null() {
            continue;
        }

        let commit = (*new_crtc_state).commit;

        if !old_crtc_state.is_null() {
            if !(*old_crtc_state).commit.is_null() {
                drm_crtc_commit_put((*old_crtc_state).commit);
            }
            (*old_crtc_state).commit = drm_crtc_commit_get(commit);
        }

        (*commit).hw_done.complete_all();
    }

    if !(*state).fake_commit.is_null() {
        (*(*state).fake_commit).hw_done.complete_all();
        (*(*state).fake_commit).flip_done.complete_all();
    }
}

/// Finish a nonblocking commit whose input fence or hardware submission failed
/// after the new software state was published. No successful flip event is
/// reported; out-fences receive the error, pending events are canceled, and
/// the device is unplugged so clients can observe that recovery is required.
/// Driver-owned resources that hardware may still access must remain alive
/// until the driver has confirmed that the device is no longer using them.
///
/// A driver calling this from its `atomic_commit_tail` callback must return
/// immediately afterwards rather than invoking the normal helper tail.
///
/// # Safety
/// `state` must be a live committed atomic state with a failed asynchronous
/// submission associated with it. `error` should be a negative Linux errno.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_commit_error(
    state: *mut DrmAtomicCommit,
    error: i32,
) {
    if state.is_null() {
        return;
    }

    // SAFETY: The caller contract guarantees a live committed state; the
    // device and all state arrays remain owned until this worker completes.
    unsafe {
        let dev = (*state).dev;
        if dev.is_null() {
            return;
        }
        let error = if error < 0 {
            error
        } else {
            -tk_linux_kapi::config::EIO
        };

        // Stop later commits before waking waiters that depend on this one.
        crate::drm_drv_upstream::drm_dev_unplug(dev);
        drm_atomic_helper_commit_hw_done(state);

        let num_crtcs = (*dev).mode_config.num_crtc as usize;
        for i in 0..num_crtcs {
            let entry = (*state).crtcs.add(i);
            let new_crtc_state = (*entry).new_state;
            if new_crtc_state.is_null() {
                continue;
            }

            let event = (*new_crtc_state).event;
            if !event.is_null() {
                let event_base = core::ptr::addr_of_mut!((*event).base);
                let fence = (*event_base).fence;
                if !fence.is_null() {
                    tk_linux_kapi::dma_fence::dma_fence_set_error(fence, error);
                    tk_linux_kapi::dma_fence::dma_fence_signal(fence);
                    tk_linux_kapi::dma_fence::dma_fence_put(fence);
                    (*event_base).fence = core::ptr::null_mut();
                }

                let completion = (*event_base).completion;
                if !completion.is_null() {
                    if let Some(completion_ref) = completion.as_ref() {
                        completion_ref.complete_all();
                    }
                    if let Some(release) = (*event_base).completion_release {
                        release(completion);
                    }
                    (*event_base).completion = core::ptr::null_mut();
                    (*event_base).completion_release = None;
                }

                let commit = (*new_crtc_state).commit;
                if !commit.is_null() && (*commit).event == event {
                    (*commit).event = core::ptr::null_mut();
                }
                crate::drm_file_upstream::drm_event_cancel_free(dev, event_base);
                (*new_crtc_state).event = core::ptr::null_mut();
            }

            let commit = (*new_crtc_state).commit;
            if !commit.is_null() {
                (*commit).flip_done.complete_all();
            }
        }

        // The helper tail is skipped on failure; retire old-state resources
        // without claiming that a new frame was displayed successfully.
        drm_atomic_helper_cleanup_planes(dev, state);
    }
}

// upstream: drm_atomic_helper.c drm_atomic_helper_commit_cleanup_done()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_commit_cleanup_done(state: *mut DrmAtomicCommit) {
    let dev = (*state).dev;
    let num_crtcs = (*dev).mode_config.num_crtc as usize;
    for i in 0..num_crtcs {
        let entry = (*state).crtcs.add(i);
        let crtc = (*entry).ptr;
        let old_crtc_state = (*entry).old_state;

        if crtc.is_null() || old_crtc_state.is_null() || (*old_crtc_state).commit.is_null() {
            continue;
        }

        let commit = (*old_crtc_state).commit;
        (*commit).cleanup_done.complete_all();

        spin_lock_raw(core::ptr::addr_of_mut!((*crtc).commit_lock));
        tk_linux_kapi::list::list_del(core::ptr::addr_of_mut!((*commit).commit_entry));
        spin_unlock_raw(core::ptr::addr_of_mut!((*crtc).commit_lock));
    }

    if !(*state).fake_commit.is_null() {
        (*(*state).fake_commit).cleanup_done.complete_all();
    }
}

// =========================================================================
// Section 6: Planes Commit, Swap & Lifecycle
// =========================================================================

// upstream: drm_atomic_helper.c drm_atomic_helper_prepare_planes()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_prepare_planes(
    dev: *mut DrmDevice,
    state: *mut DrmAtomicCommit,
) -> i32 {
    let num_connectors = (*state).num_connector as usize;
    for i in 0..num_connectors {
        let conn_entry = (*state).connectors.add(i);
        let new_conn_state = (*conn_entry).new_state;
        if new_conn_state.is_null() || (*new_conn_state).writeback_job.is_null() {
            continue;
        }

        let ret = pending_stubs::bridge::drm_writeback_prepare_job((*new_conn_state).writeback_job);
        if ret < 0 {
            return ret;
        }
    }

    let num_planes = (*dev).mode_config.num_total_plane as usize;
    let mut prepared_planes = 0usize;
    let mut accessed_planes = 0usize;

    for i in 0..num_planes {
        let entry = (*state).planes.add(i);
        let plane = (*entry).ptr;
        let new_plane_state = (*entry).new_state;

        if plane.is_null() || new_plane_state.is_null() {
            continue;
        }

        let funcs = (*plane).helper_private as *const DrmPlaneHelperFuncs;
        let mut ret = 0;

        if !funcs.is_null() && (*funcs).prepare_fb.is_some() {
            ret = ((*funcs).prepare_fb.unwrap())(plane, new_plane_state);
        } else {
            if drm_core_check_feature(dev, DRIVER_GEM) {
                ret = crate::drm_gem_atomic_helper_upstream::drm_gem_plane_helper_prepare_fb(plane, new_plane_state);
            }
        }

        if ret != 0 {
            for j in 0..prepared_planes {
                let prev_entry = (*state).planes.add(j);
                let prev_plane = (*prev_entry).ptr;
                let prev_state = (*prev_entry).new_state;
                if prev_plane.is_null() || prev_state.is_null() {
                    continue;
                }
                let prev_funcs = (*prev_plane).helper_private as *const DrmPlaneHelperFuncs;
                if !prev_funcs.is_null() && (*prev_funcs).cleanup_fb.is_some() {
                    ((*prev_funcs).cleanup_fb.unwrap())(prev_plane, prev_state);
                }
            }
            return ret;
        }
        prepared_planes = i + 1;
    }

    for i in 0..num_planes {
        let entry = (*state).planes.add(i);
        let plane = (*entry).ptr;
        let new_plane_state = (*entry).new_state;

        if plane.is_null() || new_plane_state.is_null() {
            continue;
        }

        let funcs = (*plane).helper_private as *const DrmPlaneHelperFuncs;
        if !funcs.is_null() && (*funcs).begin_fb_access.is_some() {
            let ret = ((*funcs).begin_fb_access.unwrap())(plane, new_plane_state);
            if ret != 0 {
                for j in 0..accessed_planes {
                    let prev_entry = (*state).planes.add(j);
                    let prev_plane = (*prev_entry).ptr;
                    let prev_state = (*prev_entry).new_state;
                    if prev_plane.is_null() || prev_state.is_null() {
                        continue;
                    }
                    let prev_funcs = (*prev_plane).helper_private as *const DrmPlaneHelperFuncs;
                    if !prev_funcs.is_null() && (*prev_funcs).end_fb_access.is_some() {
                        ((*prev_funcs).end_fb_access.unwrap())(prev_plane, prev_state);
                    }
                }
                for j in 0..num_planes {
                    let prev_entry = (*state).planes.add(j);
                    let prev_plane = (*prev_entry).ptr;
                    let prev_state = (*prev_entry).new_state;
                    if prev_plane.is_null() || prev_state.is_null() {
                        continue;
                    }
                    let prev_funcs = (*prev_plane).helper_private as *const DrmPlaneHelperFuncs;
                    if !prev_funcs.is_null() && (*prev_funcs).cleanup_fb.is_some() {
                        ((*prev_funcs).cleanup_fb.unwrap())(prev_plane, prev_state);
                    }
                }
                return ret;
            }
        }
        accessed_planes = i + 1;
    }

    0
}

// upstream: drm_atomic_helper.c drm_atomic_helper_unprepare_planes()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_unprepare_planes(
    dev: *mut DrmDevice,
    state: *mut DrmAtomicCommit,
) {
    let num_planes = (*dev).mode_config.num_total_plane as usize;
    for i in 0..num_planes {
        let entry = (*state).planes.add(i);
        let plane = (*entry).ptr;
        let new_plane_state = (*entry).new_state;
        if plane.is_null() || new_plane_state.is_null() {
            continue;
        }

        let funcs = (*plane).helper_private as *const DrmPlaneHelperFuncs;
        if !funcs.is_null() && (*funcs).end_fb_access.is_some() {
            ((*funcs).end_fb_access.unwrap())(plane, new_plane_state);
        }
    }

    for i in 0..num_planes {
        let entry = (*state).planes.add(i);
        let plane = (*entry).ptr;
        let new_plane_state = (*entry).new_state;
        if plane.is_null() || new_plane_state.is_null() {
            continue;
        }

        let funcs = (*plane).helper_private as *const DrmPlaneHelperFuncs;
        if !funcs.is_null() && (*funcs).cleanup_fb.is_some() {
            ((*funcs).cleanup_fb.unwrap())(plane, new_plane_state);
        }
    }
}

// upstream: drm_atomic_helper.c plane_crtc_active()
unsafe fn plane_crtc_active(state: *const DrmPlaneState) -> bool {
    !state.is_null() && !(*state).crtc.is_null() && !(*(*state).crtc).state.is_null() && (*(*(*state).crtc).state).active
}

// upstream: drm_atomic_helper.c drm_atomic_helper_commit_planes()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_commit_planes(
    dev: *mut DrmDevice,
    state: *mut DrmAtomicCommit,
    flags: u32,
) {
    let active_only = (flags & DRM_PLANE_COMMIT_ACTIVE_ONLY) != 0;
    let no_disable = (flags & DRM_PLANE_COMMIT_NO_DISABLE_AFTER_MODESET) != 0;

    let num_crtcs = (*dev).mode_config.num_crtc as usize;
    for i in 0..num_crtcs {
        let entry = (*state).crtcs.add(i);
        let crtc = (*entry).ptr;
        let old_crtc_state = (*entry).old_state;
        if crtc.is_null() || old_crtc_state.is_null() {
            continue;
        }

        let funcs = (*crtc).helper_private as *const DrmCrtcHelperFuncs;
        let new_crtc_state = (*entry).new_state;

        if !funcs.is_null() && (*funcs).atomic_begin.is_some() {
            if active_only && (new_crtc_state.is_null() || !(*new_crtc_state).active) {
                continue;
            }
            ((*funcs).atomic_begin.unwrap())(crtc, state);
        }
    }

    let num_planes = (*dev).mode_config.num_total_plane as usize;
    for i in 0..num_planes {
        let entry = (*state).planes.add(i);
        let plane = (*entry).ptr;
        let old_plane_state = (*entry).old_state;
        let new_plane_state = (*entry).new_state;
        if plane.is_null() || old_plane_state.is_null() || new_plane_state.is_null() {
            continue;
        }

        let funcs = (*plane).helper_private as *const DrmPlaneHelperFuncs;
        if funcs.is_null() {
            continue;
        }

        let disabling = drm_atomic_plane_disabling(old_plane_state, new_plane_state);

        if active_only {
            if disabling {
                let was_active = plane_crtc_active(old_plane_state);
                if !was_active {
                    continue;
                }
            } else {
                let is_active = plane_crtc_active(new_plane_state);
                if !is_active {
                    continue;
                }
            }
        }

        if disabling && (*funcs).atomic_disable.is_some() {
            let crtc_state = (*(*old_plane_state).crtc).state;
            if drm_atomic_crtc_needs_modeset(crtc_state) && no_disable {
                continue;
            }
            ((*funcs).atomic_disable.unwrap())(plane, state);
        } else if !(*new_plane_state).crtc.is_null() || disabling {
            if (*funcs).atomic_update.is_some() {
                ((*funcs).atomic_update.unwrap())(plane, state);
            }
            if !disabling && (*funcs).atomic_enable.is_some() {
                if drm_atomic_plane_enabling(old_plane_state, new_plane_state) {
                    ((*funcs).atomic_enable.unwrap())(plane, state);
                }
            }
        }
    }

    for i in 0..num_crtcs {
        let entry = (*state).crtcs.add(i);
        let crtc = (*entry).ptr;
        let new_crtc_state = (*entry).new_state;
        if crtc.is_null() || new_crtc_state.is_null() {
            continue;
        }

        let funcs = (*crtc).helper_private as *const DrmCrtcHelperFuncs;
        if !funcs.is_null() && (*funcs).atomic_flush.is_some() {
            if active_only && !(*new_crtc_state).active {
                continue;
            }
            ((*funcs).atomic_flush.unwrap())(crtc, state);
        }
    }

    for i in 0..num_planes {
        let entry = (*state).planes.add(i);
        let plane = (*entry).ptr;
        let old_plane_state = (*entry).old_state;
        if plane.is_null() || old_plane_state.is_null() {
            continue;
        }

        let funcs = (*plane).helper_private as *const DrmPlaneHelperFuncs;
        if !funcs.is_null() && (*funcs).end_fb_access.is_some() {
            ((*funcs).end_fb_access.unwrap())(plane, old_plane_state);
        }
    }
}

// upstream: drm_atomic_helper.c drm_atomic_helper_commit_planes_on_crtc()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_commit_planes_on_crtc(
    old_crtc_state: *mut DrmCrtcState,
) {
    let crtc = (*old_crtc_state).crtc;
    let old_state = (*old_crtc_state).state;
    let new_crtc_state = crate::drm_atomic_upstream::drm_atomic_get_new_crtc_state(old_state, crtc);

    let mut plane_mask = (*old_crtc_state).plane_mask;
    if !new_crtc_state.is_null() {
        plane_mask |= (*new_crtc_state).plane_mask;
    }

    let crtc_funcs = (*crtc).helper_private as *const DrmCrtcHelperFuncs;
    if !crtc_funcs.is_null() && (*crtc_funcs).atomic_begin.is_some() {
        ((*crtc_funcs).atomic_begin.unwrap())(crtc, old_state);
    }

    let dev = (*crtc).dev;
    let head = core::ptr::addr_of_mut!((*dev).mode_config.plane_list);
    let mut cur = (*head).next;
    while cur != head {
        let plane = (cur as *mut u8).sub(8) as *mut DrmPlane;
        let mask = 1u32 << (*plane).index;
        if (plane_mask & mask) != 0 {
            let old_plane_state = crate::drm_atomic_upstream::drm_atomic_get_old_plane_state(old_state, plane);
            let new_plane_state = crate::drm_atomic_upstream::drm_atomic_get_new_plane_state(old_state, plane);
            let plane_funcs = (*plane).helper_private as *const DrmPlaneHelperFuncs;

            if !old_plane_state.is_null() && !plane_funcs.is_null() {
                let disabling = drm_atomic_plane_disabling(old_plane_state, new_plane_state);
                if disabling && (*plane_funcs).atomic_disable.is_some() {
                    ((*plane_funcs).atomic_disable.unwrap())(plane, old_state);
                } else if !new_plane_state.is_null() && (!(*new_plane_state).crtc.is_null() || disabling) {
                    if (*plane_funcs).atomic_update.is_some() {
                        ((*plane_funcs).atomic_update.unwrap())(plane, old_state);
                    }
                    if !disabling && (*plane_funcs).atomic_enable.is_some() {
                        if drm_atomic_plane_enabling(old_plane_state, new_plane_state) {
                            ((*plane_funcs).atomic_enable.unwrap())(plane, old_state);
                        }
                    }
                }
            }
        }
        cur = (*cur).next;
    }

    if !crtc_funcs.is_null() && (*crtc_funcs).atomic_flush.is_some() {
        ((*crtc_funcs).atomic_flush.unwrap())(crtc, old_state);
    }
}

// upstream: drm_atomic_helper.c drm_atomic_helper_disable_planes_on_crtc()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_disable_planes_on_crtc(
    old_crtc_state: *mut DrmCrtcState,
    atomic: bool,
) {
    let crtc = (*old_crtc_state).crtc;
    let crtc_funcs = (*crtc).helper_private as *const DrmCrtcHelperFuncs;

    if atomic && !crtc_funcs.is_null() && (*crtc_funcs).atomic_begin.is_some() {
        ((*crtc_funcs).atomic_begin.unwrap())(crtc, core::ptr::null_mut());
    }

    let dev = (*crtc).dev;
    let plane_mask = (*old_crtc_state).plane_mask;
    let head = core::ptr::addr_of_mut!((*dev).mode_config.plane_list);
    let mut cur = (*head).next;
    while cur != head {
        let plane = (cur as *mut u8).sub(8) as *mut DrmPlane;
        let mask = 1u32 << (*plane).index;
        if (plane_mask & mask) != 0 {
            let plane_funcs = (*plane).helper_private as *const DrmPlaneHelperFuncs;
            if !plane_funcs.is_null() && (*plane_funcs).atomic_disable.is_some() {
                ((*plane_funcs).atomic_disable.unwrap())(plane, core::ptr::null_mut());
            }
        }
        cur = (*cur).next;
    }

    if atomic && !crtc_funcs.is_null() && (*crtc_funcs).atomic_flush.is_some() {
        ((*crtc_funcs).atomic_flush.unwrap())(crtc, core::ptr::null_mut());
    }
}

// upstream: drm_atomic_helper.c drm_atomic_helper_cleanup_planes()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_cleanup_planes(
    dev: *mut DrmDevice,
    state: *mut DrmAtomicCommit,
) {
    let num_planes = (*dev).mode_config.num_total_plane as usize;
    for i in 0..num_planes {
        let entry = (*state).planes.add(i);
        let plane = (*entry).ptr;
        let old_plane_state = (*entry).old_state;
        if plane.is_null() || old_plane_state.is_null() {
            continue;
        }

        let funcs = (*plane).helper_private as *const DrmPlaneHelperFuncs;
        if !funcs.is_null() && (*funcs).cleanup_fb.is_some() {
            ((*funcs).cleanup_fb.unwrap())(plane, old_plane_state);
        }
    }
}

// upstream: drm_atomic_helper.c drm_atomic_helper_swap_state()
/// Waits for the previous hardware phase associated with objects in `state`.
///
/// # Safety
/// `state` must be a live atomic state whose old-state commit references and
/// device arrays remain valid for the duration of each wait.
unsafe fn wait_for_hw_done(state: *mut DrmAtomicCommit) -> i32 {
    // SAFETY: The function contract keeps `state`, the device arrays, and all
    // old commit references live while the blocking completion waits execute.
    let dev = (*state).dev;
    let num_crtcs = (*dev).mode_config.num_crtc as usize;
    let num_connectors = (*state).num_connector as usize;
    let num_planes = (*dev).mode_config.num_total_plane as usize;

    for i in 0..num_crtcs {
        let old = (*(*state).crtcs.add(i)).old_state;
        if !old.is_null() && !(*old).commit.is_null() {
            let ret = (*(*old).commit).hw_done.wait_interruptible();
            if ret != 0 {
                return ret;
            }
        }
    }

    for i in 0..num_connectors {
        let old = (*(*state).connectors.add(i)).old_state;
        if !old.is_null() && !(*old).commit.is_null() {
            let ret = (*(*old).commit).hw_done.wait_interruptible();
            if ret != 0 {
                return ret;
            }
        }
    }

    for i in 0..num_planes {
        let old = (*(*state).planes.add(i)).old_state;
        if !old.is_null() && !(*old).commit.is_null() {
            let ret = (*(*old).commit).hw_done.wait_interruptible();
            if ret != 0 {
                return ret;
            }
        }
    }

    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_swap_state(
    state: *mut DrmAtomicCommit,
    stall: bool,
) -> i32 {
    let dev = (*state).dev;
    let num_crtcs = (*dev).mode_config.num_crtc as usize;
    let num_connectors = (*state).num_connector as usize;
    let num_planes = (*dev).mode_config.num_total_plane as usize;

    if stall {
        let ret = wait_for_hw_done(state);
        if ret != 0 {
            return ret;
        }
    }

    for i in 0..num_connectors {
        let conn_entry = (*state).connectors.add(i);
        let connector = (*conn_entry).ptr;
        let old_conn_state = (*conn_entry).old_state;
        let new_conn_state = (*conn_entry).new_state;
        if connector.is_null() || old_conn_state.is_null() || new_conn_state.is_null() {
            continue;
        }

        (*old_conn_state).state = state;
        (*new_conn_state).state = core::ptr::null_mut();

        (*conn_entry).state_to_destroy = old_conn_state;
        (*connector).state = new_conn_state;
    }

    for i in 0..num_crtcs {
        let entry = (*state).crtcs.add(i);
        let crtc = (*entry).ptr;
        let old_crtc_state = (*entry).old_state;
        let new_crtc_state = (*entry).new_state;
        if crtc.is_null() || old_crtc_state.is_null() || new_crtc_state.is_null() {
            continue;
        }

        (*old_crtc_state).state = state;
        (*new_crtc_state).state = core::ptr::null_mut();

        (*entry).state_to_destroy = old_crtc_state;
        (*crtc).state = new_crtc_state;

        if !(*new_crtc_state).commit.is_null() {
            spin_lock_raw(core::ptr::addr_of_mut!((*crtc).commit_lock));
            tk_linux_kapi::list::list_add(
                core::ptr::addr_of_mut!((*(*new_crtc_state).commit).commit_entry),
                core::ptr::addr_of_mut!((*crtc).commit_list),
            );
            spin_unlock_raw(core::ptr::addr_of_mut!((*crtc).commit_lock));

            (*(*new_crtc_state).commit).event = core::ptr::null_mut();
        }
    }

    let num_colorops = if (*state).colorops.is_null() {
        0
    } else {
        (*(*state).dev).mode_config.num_colorop as usize
    };
    for i in 0..num_colorops {
        let entry = (*state).colorops.add(i);
        let colorop = (*entry).ptr as *mut DrmColorop;
        let old_colorop_state = (*entry).old_state as *mut DrmColoropState;
        let new_colorop_state = (*entry).new_state as *mut DrmColoropState;
        if colorop.is_null() || old_colorop_state.is_null() || new_colorop_state.is_null() {
            continue;
        }

        (*old_colorop_state).state = state;
        (*new_colorop_state).state = core::ptr::null_mut();

        (*entry).state = old_colorop_state as *mut c_void;
        (*colorop).state = new_colorop_state;
    }

    // Skip drm_panic_lock / unlock: CONFIG_DRM_PANIC is disabled in 7.2.3-q35-graphics.config
    for i in 0..num_planes {
        let entry = (*state).planes.add(i);
        let plane = (*entry).ptr;
        let old_plane_state = (*entry).old_state;
        let new_plane_state = (*entry).new_state;
        if plane.is_null() || old_plane_state.is_null() || new_plane_state.is_null() {
            continue;
        }

        (*old_plane_state).state = state;
        (*new_plane_state).state = core::ptr::null_mut();

        (*entry).state_to_destroy = old_plane_state;
        (*plane).state = new_plane_state;
    }

    let num_priv = (*state).num_private_objs as usize;
    for i in 0..num_priv {
        let entry = (*state).private_objs.add(i);
        let obj = (*entry).ptr;
        let old_obj_state = (*entry).old_state;
        let new_obj_state = (*entry).new_state;
        if obj.is_null() || old_obj_state.is_null() || new_obj_state.is_null() {
            continue;
        }

        (*old_obj_state).state = state;
        (*new_obj_state).state = core::ptr::null_mut();

        (*entry).state_to_destroy = old_obj_state;
        (*obj).state = new_obj_state;
    }

    0
}

// =========================================================================
// Section 7: Legacy Helpers, Suspend / Resume & Page Flip
// =========================================================================

// upstream: drm_atomic_helper.c drm_atomic_helper_update_plane()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_update_plane(
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
    let dev = (*plane).dev;
    let state = crate::drm_atomic_upstream::drm_atomic_commit_alloc(dev);
    if state.is_null() {
        return -ENOMEM;
    }

    (*state).acquire_ctx = ctx;
    let plane_state = crate::drm_atomic_upstream::drm_atomic_get_plane_state(state, plane);
    if IS_ERR(plane_state) {
        let ret = PTR_ERR(plane_state);
        crate::drm_atomic_upstream::drm_atomic_commit_put(state);
        return ret;
    }

    let mut ret = crate::drm_atomic_uapi_upstream::drm_atomic_set_crtc_for_plane(plane_state, crtc);
    if ret != 0 {
        crate::drm_atomic_upstream::drm_atomic_commit_put(state);
        return ret;
    }

    crate::drm_atomic_uapi_upstream::drm_atomic_set_fb_for_plane(plane_state, fb);
    (*plane_state).crtc_x = crtc_x;
    (*plane_state).crtc_y = crtc_y;
    (*plane_state).crtc_w = crtc_w;
    (*plane_state).crtc_h = crtc_h;
    (*plane_state).src_x = src_x;
    (*plane_state).src_y = src_y;
    (*plane_state).src_w = src_w;
    (*plane_state).src_h = src_h;

    if !crtc.is_null() && plane == (*crtc).cursor {
        (*state).set_legacy_cursor_update(true);
    }

    ret = crate::drm_atomic_upstream::drm_atomic_commit(state);
    crate::drm_atomic_upstream::drm_atomic_commit_put(state);
    ret
}

// upstream: drm_atomic_helper.c drm_atomic_helper_disable_plane()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_disable_plane(
    plane: *mut DrmPlane,
    ctx: *mut DrmModesetAcquireCtx,
) -> i32 {
    let dev = (*plane).dev;
    let state = crate::drm_atomic_upstream::drm_atomic_commit_alloc(dev);
    if state.is_null() {
        return -ENOMEM;
    }

    (*state).acquire_ctx = ctx;
    let plane_state = crate::drm_atomic_upstream::drm_atomic_get_plane_state(state, plane);
    if IS_ERR(plane_state) {
        let ret = PTR_ERR(plane_state);
        crate::drm_atomic_upstream::drm_atomic_commit_put(state);
        return ret;
    }

    if !(*plane_state).crtc.is_null() && (*(*plane_state).crtc).cursor == plane {
        (*(*plane_state).state).set_legacy_cursor_update(true);
    }

    let mut ret = crate::drm_atomic_upstream::__drm_atomic_helper_disable_plane(plane, plane_state);
    if ret != 0 {
        crate::drm_atomic_upstream::drm_atomic_commit_put(state);
        return ret;
    }

    ret = crate::drm_atomic_upstream::drm_atomic_commit(state);
    crate::drm_atomic_upstream::drm_atomic_commit_put(state);
    ret
}

// upstream: drm_atomic_helper.c drm_atomic_helper_set_config()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_set_config(
    set: *mut DrmModeSet,
    ctx: *mut DrmModesetAcquireCtx,
) -> i32 {
    let crtc = (*set).crtc;
    let dev = (*crtc).dev;
    let state = crate::drm_atomic_upstream::drm_atomic_commit_alloc(dev);
    if state.is_null() {
        return -ENOMEM;
    }

    (*state).acquire_ctx = ctx;
    let mut ret = crate::drm_atomic_upstream::__drm_atomic_helper_set_config(set, state);
    if ret != 0 {
        crate::drm_atomic_upstream::drm_atomic_commit_put(state);
        return ret;
    }

    ret = handle_conflicting_encoders(state, true);
    if ret != 0 {
        crate::drm_atomic_upstream::drm_atomic_commit_put(state);
        return ret;
    }

    ret = crate::drm_atomic_upstream::drm_atomic_commit(state);
    crate::drm_atomic_upstream::drm_atomic_commit_put(state);
    ret
}

// upstream: drm_atomic_helper.c drm_atomic_helper_disable_all()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_disable_all(
    dev: *mut DrmDevice,
    ctx: *mut DrmModesetAcquireCtx,
) -> i32 {
    let state = crate::drm_atomic_upstream::drm_atomic_commit_alloc(dev);
    if state.is_null() {
        return -ENOMEM;
    }

    (*state).acquire_ctx = ctx;

    let head = core::ptr::addr_of_mut!((*dev).mode_config.crtc_list);
    let mut cur = (*head).next;
    let mut ret = 0;
    while cur != head {
        let crtc = cur
            .cast::<u8>()
            .sub(offset_of!(DrmCrtc, head))
            .cast::<DrmCrtc>();
        let crtc_state = crate::drm_atomic_upstream::drm_atomic_get_crtc_state(state, crtc);
        if IS_ERR(crtc_state) {
            ret = PTR_ERR(crtc_state);
            crate::drm_atomic_upstream::drm_atomic_commit_put(state);
            return ret;
        }

        (*crtc_state).active = false;

        ret = crate::drm_atomic_uapi_upstream::drm_atomic_set_mode_prop_for_crtc(crtc_state, core::ptr::null_mut());
        if ret < 0 {
            crate::drm_atomic_upstream::drm_atomic_commit_put(state);
            return ret;
        }

        ret = crate::drm_atomic_upstream::drm_atomic_add_affected_planes(state, crtc);
        if ret < 0 {
            crate::drm_atomic_upstream::drm_atomic_commit_put(state);
            return ret;
        }

        ret = crate::drm_atomic_upstream::drm_atomic_add_affected_connectors(state, crtc);
        if ret < 0 {
            crate::drm_atomic_upstream::drm_atomic_commit_put(state);
            return ret;
        }

        cur = (*cur).next;
    }

    let num_connectors = (*state).num_connector as usize;
    for i in 0..num_connectors {
        let conn_entry = (*state).connectors.add(i);
        let conn_state = (*conn_entry).new_state;
        if conn_state.is_null() {
            continue;
        }
        ret = crate::drm_atomic_uapi_upstream::drm_atomic_set_crtc_for_connector(conn_state, core::ptr::null_mut());
        if ret < 0 {
            crate::drm_atomic_upstream::drm_atomic_commit_put(state);
            return ret;
        }
    }

    let num_planes = (*dev).mode_config.num_total_plane as usize;
    for i in 0..num_planes {
        let entry = (*state).planes.add(i);
        let plane_state = (*entry).new_state;
        if plane_state.is_null() {
            continue;
        }

        ret = crate::drm_atomic_uapi_upstream::drm_atomic_set_crtc_for_plane(plane_state, core::ptr::null_mut());
        if ret < 0 {
            crate::drm_atomic_upstream::drm_atomic_commit_put(state);
            return ret;
        }

        crate::drm_atomic_uapi_upstream::drm_atomic_set_fb_for_plane(plane_state, core::ptr::null_mut());
    }

    ret = crate::drm_atomic_upstream::drm_atomic_commit(state);
    crate::drm_atomic_upstream::drm_atomic_commit_put(state);
    ret
}

// upstream: drm_atomic_helper.c drm_atomic_helper_reset_crtc()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_reset_crtc(
    crtc: *mut DrmCrtc,
    ctx: *mut DrmModesetAcquireCtx,
) -> i32 {
    let dev = (*crtc).dev;
    let state = crate::drm_atomic_upstream::drm_atomic_commit_alloc(dev);
    if state.is_null() {
        return -ENOMEM;
    }

    (*state).acquire_ctx = ctx;
    let crtc_state = crate::drm_atomic_upstream::drm_atomic_get_crtc_state(state, crtc);
    if IS_ERR(crtc_state) {
        let ret = PTR_ERR(crtc_state);
        crate::drm_atomic_upstream::drm_atomic_commit_put(state);
        return ret;
    }

    (*crtc_state).set_connectors_changed(true);
    let ret = crate::drm_atomic_upstream::drm_atomic_commit(state);
    crate::drm_atomic_upstream::drm_atomic_commit_put(state);
    ret
}

// upstream: drm_atomic_helper.c drm_atomic_helper_shutdown()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_shutdown(dev: *mut DrmDevice) {
    if dev.is_null() {
        return;
    }

    let mut ctx: DrmModesetAcquireCtx = core::mem::zeroed();
    let mut ret: i32;

    crate::drm_modeset_lock_upstream::drm_modeset_acquire_init(&mut ctx, 0);
    loop {
        ret = crate::drm_modeset_lock_upstream::drm_modeset_lock_all_ctx(dev, &mut ctx);
        if ret == 0 {
            ret = drm_atomic_helper_disable_all(dev, &mut ctx);
        }
        if ret == -tk_linux_kapi::config::EDEADLK {
            ret = crate::drm_modeset_lock_upstream::drm_modeset_backoff(&mut ctx);
            if ret == 0 {
                continue;
            }
        }
        break;
    }
    crate::drm_modeset_lock_upstream::drm_modeset_drop_locks(&mut ctx);
    crate::drm_modeset_lock_upstream::drm_modeset_acquire_fini(&mut ctx);
}

// upstream: drm_atomic_helper.c drm_atomic_helper_duplicate_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_duplicate_state(
    dev: *mut DrmDevice,
    ctx: *mut DrmModesetAcquireCtx,
) -> *mut DrmAtomicCommit {
    let state = crate::drm_atomic_upstream::drm_atomic_commit_alloc(dev);
    if state.is_null() {
        return ERR_PTR(-ENOMEM);
    }

    (*state).acquire_ctx = ctx;
    (*state).set_duplicated(true);

    let head = core::ptr::addr_of_mut!((*dev).mode_config.crtc_list);
    let mut cur = (*head).next;
    while cur != head {
        let crtc = cur
            .cast::<u8>()
            .sub(offset_of!(DrmCrtc, head))
            .cast::<DrmCrtc>();
        let crtc_state = crate::drm_atomic_upstream::drm_atomic_get_crtc_state(state, crtc);
        if IS_ERR(crtc_state) {
            let err = PTR_ERR(crtc_state);
            crate::drm_atomic_upstream::drm_atomic_commit_put(state);
            return ERR_PTR(err);
        }
        cur = (*cur).next;
    }

    let plane_head = core::ptr::addr_of_mut!((*dev).mode_config.plane_list);
    let mut plane_cur = (*plane_head).next;
    while plane_cur != plane_head {
        let plane = (plane_cur as *mut u8).sub(8) as *mut DrmPlane;
        let plane_state = crate::drm_atomic_upstream::drm_atomic_get_plane_state(state, plane);
        if IS_ERR(plane_state) {
            let err = PTR_ERR(plane_state);
            crate::drm_atomic_upstream::drm_atomic_commit_put(state);
            return ERR_PTR(err);
        }

        if !(*plane_state).color_pipeline.is_null() {
            let err = crate::drm_atomic_upstream::drm_atomic_add_affected_colorops(state, plane);
            if err != 0 {
                crate::drm_atomic_upstream::drm_atomic_commit_put(state);
                return ERR_PTR(err);
            }
        }
        plane_cur = (*plane_cur).next;
    }

    let mut conn_iter: DrmConnectorListIter = core::mem::zeroed();
    crate::drm_connector_upstream::drm_connector_list_iter_begin(dev, &mut conn_iter);
    loop {
        let conn = crate::drm_connector_upstream::drm_connector_list_iter_next(&mut conn_iter);
        if conn.is_null() {
            break;
        }

        let conn_state = crate::drm_atomic_upstream::drm_atomic_get_connector_state(state, conn);
        if IS_ERR(conn_state) {
            let err = PTR_ERR(conn_state);
            crate::drm_connector_upstream::drm_connector_list_iter_end(&mut conn_iter);
            crate::drm_atomic_upstream::drm_atomic_commit_put(state);
            return ERR_PTR(err);
        }
    }
    crate::drm_connector_upstream::drm_connector_list_iter_end(&mut conn_iter);

    (*state).acquire_ctx = core::ptr::null_mut();
    state
}

// upstream: drm_atomic_helper.c drm_atomic_helper_suspend()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_suspend(
    dev: *mut DrmDevice,
) -> *mut DrmAtomicCommit {
    let mut ctx: DrmModesetAcquireCtx = core::mem::zeroed();
    let mut state: *mut DrmAtomicCommit = ERR_PTR(-EINVAL);
    let mut ret: i32;

    crate::drm_modeset_lock_upstream::drm_modeset_acquire_init(&mut ctx, 0);
    loop {
        ret = crate::drm_modeset_lock_upstream::drm_modeset_lock_all_ctx(dev, &mut ctx);
        if ret == 0 {
            state = drm_atomic_helper_duplicate_state(dev, &mut ctx);
            if !IS_ERR(state) {
                ret = drm_atomic_helper_disable_all(dev, &mut ctx);
                if ret < 0 {
                    crate::drm_atomic_upstream::drm_atomic_commit_put(state);
                    state = ERR_PTR(ret);
                }
            } else {
                ret = PTR_ERR(state);
            }
        }
        if ret == -tk_linux_kapi::config::EDEADLK {
            ret = crate::drm_modeset_lock_upstream::drm_modeset_backoff(&mut ctx);
            if ret == 0 {
                continue;
            }
        }
        break;
    }
    crate::drm_modeset_lock_upstream::drm_modeset_drop_locks(&mut ctx);
    crate::drm_modeset_lock_upstream::drm_modeset_acquire_fini(&mut ctx);

    if ret != 0 && !IS_ERR(state) {
        return ERR_PTR(ret);
    }
    state
}

// upstream: drm_atomic_helper.c drm_atomic_helper_commit_duplicated_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_commit_duplicated_state(
    state: *mut DrmAtomicCommit,
    ctx: *mut DrmModesetAcquireCtx,
) -> i32 {
    (*state).acquire_ctx = ctx;

    let dev = (*state).dev;
    let num_colorops = if (*state).colorops.is_null() {
        0
    } else {
        (*dev).mode_config.num_colorop as usize
    };
    for i in 0..num_colorops {
        let entry = (*state).colorops.add(i);
        let colorop = (*entry).ptr as *mut DrmColorop;
        if !colorop.is_null() {
            (*entry).old_state = (*colorop).state as *mut c_void;
        }
    }

    let num_planes = (*dev).mode_config.num_total_plane as usize;
    for i in 0..num_planes {
        let entry = (*state).planes.add(i);
        let plane = (*entry).ptr;
        if !plane.is_null() {
            (*entry).old_state = (*plane).state;
        }
    }

    let num_crtcs = (*dev).mode_config.num_crtc as usize;
    for i in 0..num_crtcs {
        let entry = (*state).crtcs.add(i);
        let crtc = (*entry).ptr;
        if !crtc.is_null() {
            (*entry).old_state = (*crtc).state;
        }
    }

    let num_connectors = (*state).num_connector as usize;
    for i in 0..num_connectors {
        let conn_entry = (*state).connectors.add(i);
        let connector = (*conn_entry).ptr;
        if !connector.is_null() {
            (*conn_entry).old_state = (*connector).state;
        }
    }

    let ret = crate::drm_atomic_upstream::drm_atomic_commit(state);
    (*state).acquire_ctx = core::ptr::null_mut();
    ret
}

// upstream: drm_atomic_helper.c drm_atomic_helper_resume()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_resume(
    dev: *mut DrmDevice,
    state: *mut DrmAtomicCommit,
) -> i32 {
    crate::drm_mode_config_upstream::drm_mode_config_reset(dev);

    let mut ctx: DrmModesetAcquireCtx = core::mem::zeroed();
    let mut err: i32;

    crate::drm_modeset_lock_upstream::drm_modeset_acquire_init(&mut ctx, 0);
    loop {
        err = crate::drm_modeset_lock_upstream::drm_modeset_lock_all_ctx(dev, &mut ctx);
        if err == 0 {
            err = drm_atomic_helper_commit_duplicated_state(state, &mut ctx);
        }
        if err == -tk_linux_kapi::config::EDEADLK {
            err = crate::drm_modeset_lock_upstream::drm_modeset_backoff(&mut ctx);
            if err == 0 {
                continue;
            }
        }
        break;
    }
    crate::drm_modeset_lock_upstream::drm_modeset_drop_locks(&mut ctx);
    crate::drm_modeset_lock_upstream::drm_modeset_acquire_fini(&mut ctx);

    crate::drm_atomic_upstream::drm_atomic_commit_put(state);
    err
}

// upstream: drm_atomic_helper.c page_flip_common()
unsafe fn page_flip_common(
    state: *mut DrmAtomicCommit,
    crtc: *mut DrmCrtc,
    fb: *mut DrmFramebuffer,
    event: *mut DrmPendingVblankEvent,
    flags: u32,
) -> i32 {
    let plane = (*crtc).primary;
    let crtc_state = crate::drm_atomic_upstream::drm_atomic_get_crtc_state(state, crtc);
    if IS_ERR(crtc_state) {
        return PTR_ERR(crtc_state);
    }

    (*crtc_state).event = event;
    (*crtc_state).async_flip = (flags & DRM_MODE_PAGE_FLIP_ASYNC) != 0;

    let plane_state = crate::drm_atomic_upstream::drm_atomic_get_plane_state(state, plane);
    if IS_ERR(plane_state) {
        return PTR_ERR(plane_state);
    }

    let mut ret = crate::drm_atomic_uapi_upstream::drm_atomic_set_crtc_for_plane(plane_state, crtc);
    if ret != 0 {
        return ret;
    }
    crate::drm_atomic_uapi_upstream::drm_atomic_set_fb_for_plane(plane_state, fb);

    (*state).set_allow_modeset(false);
    if !(*crtc_state).active {
        return -EINVAL;
    }

    0
}

// upstream: drm_atomic_helper.c drm_atomic_helper_page_flip()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_page_flip(
    crtc: *mut DrmCrtc,
    fb: *mut DrmFramebuffer,
    event: *mut DrmPendingVblankEvent,
    flags: u32,
    ctx: *mut DrmModesetAcquireCtx,
) -> i32 {
    let plane = (*crtc).primary;
    let dev = (*plane).dev;
    let state = crate::drm_atomic_upstream::drm_atomic_commit_alloc(dev);
    if state.is_null() {
        return -ENOMEM;
    }

    (*state).acquire_ctx = ctx;
    let mut ret = page_flip_common(state, crtc, fb, event, flags);
    if ret != 0 {
        crate::drm_atomic_upstream::drm_atomic_commit_put(state);
        return ret;
    }

    ret = crate::drm_atomic_upstream::drm_atomic_nonblocking_commit(state);
    crate::drm_atomic_upstream::drm_atomic_commit_put(state);
    ret
}

// upstream: drm_atomic_helper.c drm_atomic_helper_page_flip_target()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_page_flip_target(
    crtc: *mut DrmCrtc,
    fb: *mut DrmFramebuffer,
    event: *mut DrmPendingVblankEvent,
    flags: u32,
    target: u32,
    ctx: *mut DrmModesetAcquireCtx,
) -> i32 {
    let plane = (*crtc).primary;
    let dev = (*plane).dev;
    let state = crate::drm_atomic_upstream::drm_atomic_commit_alloc(dev);
    if state.is_null() {
        return -ENOMEM;
    }

    (*state).acquire_ctx = ctx;
    let mut ret = page_flip_common(state, crtc, fb, event, flags);
    if ret != 0 {
        crate::drm_atomic_upstream::drm_atomic_commit_put(state);
        return ret;
    }

    let crtc_state = crate::drm_atomic_upstream::drm_atomic_get_new_crtc_state(state, crtc);
    if crtc_state.is_null() {
        crate::drm_atomic_upstream::drm_atomic_commit_put(state);
        return -EINVAL;
    }
    (*crtc_state).target_vblank = target;

    ret = crate::drm_atomic_upstream::drm_atomic_nonblocking_commit(state);
    crate::drm_atomic_upstream::drm_atomic_commit_put(state);
    ret
}

// upstream: drm_atomic_helper.c drm_atomic_helper_bridge_propagate_bus_fmt()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_bridge_propagate_bus_fmt(
    _bridge: *mut c_void,
    _bridge_state: *mut c_void,
    _crtc_state: *mut DrmCrtcState,
    _conn_state: *mut DrmConnectorState,
    output_fmt: u32,
    num_input_fmts: *mut u32,
) -> *mut u32 {
    let input_fmts = tk_linux_kapi::memory::kzalloc(
        core::mem::size_of::<u32>(),
        GFP_KERNEL,
    ) as *mut u32;
    if input_fmts.is_null() {
        if !num_input_fmts.is_null() {
            *num_input_fmts = 0;
        }
        return core::ptr::null_mut();
    }

    if !num_input_fmts.is_null() {
        *num_input_fmts = 1;
    }
    *input_fmts = output_fmt;
    input_fmts
}

#[cfg(test)]
mod commit_failure_tests {
    use core::mem::zeroed;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::{
        drm_atomic_types::{DrmCrtcsState, DrmPlanesState},
        drm_file_types::{DrmFile, DrmMinor},
        test_support::TestDrm,
        uapi_drm_types::DrmEvent,
    };

    static PRE_SWAP_CALLS: AtomicUsize = AtomicUsize::new(0);
    static PREPARE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static CLEANUP_CALLS: AtomicUsize = AtomicUsize::new(0);
    static TAIL_CALLS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn fail_pre_swap(_state: *mut DrmAtomicCommit) -> i32 {
        PRE_SWAP_CALLS.fetch_add(1, Ordering::SeqCst);
        -tk_linux_kapi::config::EIO
    }

    unsafe extern "C" fn prepare_fb(_plane: *mut DrmPlane, _state: *mut DrmPlaneState) -> i32 {
        PREPARE_CALLS.fetch_add(1, Ordering::SeqCst);
        0
    }

    unsafe extern "C" fn cleanup_fb(_plane: *mut DrmPlane, _state: *mut DrmPlaneState) {
        CLEANUP_CALLS.fetch_add(1, Ordering::SeqCst);
    }

    unsafe extern "C" fn commit_tail(_state: *mut DrmAtomicCommit) {
        TAIL_CALLS.fetch_add(1, Ordering::SeqCst);
    }

    static MODE_HELPERS: DrmModeConfigHelperFuncs = DrmModeConfigHelperFuncs {
        atomic_commit_tail: Some(commit_tail),
        atomic_commit_setup: None,
        atomic_commit_pre_swap: Some(fail_pre_swap),
    };

    static PLANE_HELPERS: DrmPlaneHelperFuncs = DrmPlaneHelperFuncs {
        prepare_fb: Some(prepare_fb),
        cleanup_fb: Some(cleanup_fb),
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

    #[test]
    fn failed_pre_swap_keeps_old_state_and_unprepares_planes() {
        PRE_SWAP_CALLS.store(0, Ordering::SeqCst);
        PREPARE_CALLS.store(0, Ordering::SeqCst);
        CLEANUP_CALLS.store(0, Ordering::SeqCst);
        TAIL_CALLS.store(0, Ordering::SeqCst);

        let mut drm = TestDrm::new(0);
        let dev = drm.ptr();
        // SAFETY: These DRM C-layout objects are zero-initialised fixtures;
        // the pointers and fields used by this test are explicitly filled in below.
        let mut plane: DrmPlane = unsafe { zeroed() };
        // SAFETY: The atomic plane states are zero-initialised C-layout fixtures.
        let mut old_state: DrmPlaneState = unsafe { zeroed() };
        // SAFETY: The atomic plane states are zero-initialised C-layout fixtures.
        let mut new_state: DrmPlaneState = unsafe { zeroed() };
        // SAFETY: The state-array entry is a zero-initialised C-layout fixture.
        let mut plane_entry: DrmPlanesState = unsafe { zeroed() };
        // SAFETY: The atomic state is a zero-initialised C-layout fixture.
        let mut state: DrmAtomicCommit = unsafe { zeroed() };
        plane.dev = dev;
        plane.state = &mut old_state;
        plane.helper_private = core::ptr::addr_of!(PLANE_HELPERS).cast_mut().cast();
        old_state.plane = &mut plane;
        new_state.plane = &mut plane;
        new_state.state = &mut state;
        plane_entry.ptr = &mut plane;
        plane_entry.old_state = &mut old_state;
        plane_entry.new_state = &mut new_state;
        state.dev = dev;
        state.planes = &mut plane_entry;
        // SAFETY: `dev` is live and exclusively accessed during fixture setup.
        unsafe {
            (*dev).mode_config.num_total_plane = 1;
            (*dev).mode_config.helper_private = core::ptr::addr_of!(MODE_HELPERS).cast_mut().cast();
        }

        // SAFETY: The device, state arrays, and callback tables above remain
        // live through the synchronous commit attempt.
        let result = unsafe { drm_atomic_helper_commit(dev, &mut state, false) };

        assert_eq!(result, -tk_linux_kapi::config::EIO);
        assert_eq!(PRE_SWAP_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(PREPARE_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(CLEANUP_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(TAIL_CALLS.load(Ordering::SeqCst), 0);
        assert!(core::ptr::eq(plane.state, &old_state));
        assert!(plane_entry.state_to_destroy.is_null());
        assert!(core::ptr::eq(new_state.state, &state));

        // SAFETY: setup_commit owns one fake commit reference and the new plane
        // state owns the other; release both before dropping the fixture.
        unsafe {
            drm_crtc_commit_put(new_state.commit);
            drm_crtc_commit_put(state.fake_commit);
        }
    }

    unsafe extern "C" fn release_test_fence(_fence: *mut tk_linux_kapi::types::DmaFence) {}

    static TEST_FENCE_OPS: tk_linux_kapi::types::DmaFenceOps = tk_linux_kapi::types::DmaFenceOps {
        get_driver_name: None,
        get_timeline_name: None,
        enable_signaling: None,
        signaled: None,
        wait: None,
        release: Some(release_test_fence),
        set_deadline: None,
    };

    unsafe extern "C" fn discard_event_bytes(
        _context: *mut c_void,
        _bytes: *const c_void,
        _len: usize,
    ) -> isize {
        0
    }

    #[test]
    fn commit_error_fails_out_fence_cancels_event_and_completes_dependencies() {
        let mut drm = TestDrm::new(0);
        let dev = drm.ptr();
        // SAFETY: These DRM file objects are zero-initialised C-layout test fixtures;
        // the fields needed by event and poll paths are initialized immediately below.
        let mut file: DrmFile = unsafe { zeroed() };
        // SAFETY: `DrmMinor` is a zero-initialised C-layout test fixture.
        let mut minor: DrmMinor = unsafe { zeroed() };
        // SAFETY: `File` is a zero-initialised C-layout test fixture.
        let mut filp: tk_linux_kapi::types::File = unsafe { zeroed() };
        // SAFETY: All fixture pointers are live; the mutexes, spinlock, waitqueue,
        // and lists are initialized before the event/file helpers access them.
        unsafe {
            tk_linux_kapi::mutex::mutex_init(core::ptr::addr_of_mut!((*dev).filelist_mutex));
            tk_linux_kapi::locks::spin_lock_init(&mut (*dev).event_lock);
            tk_linux_kapi::list::INIT_LIST_HEAD(core::ptr::addr_of_mut!((*dev).filelist));
            tk_linux_kapi::list::INIT_LIST_HEAD(core::ptr::addr_of_mut!(file.lhead));
            tk_linux_kapi::list::INIT_LIST_HEAD(core::ptr::addr_of_mut!(file.pending_event_list));
            tk_linux_kapi::list::INIT_LIST_HEAD(core::ptr::addr_of_mut!(file.event_list));
            tk_linux_kapi::wait::init_waitqueue_head(&mut file.event_wait);
            tk_linux_kapi::mutex::mutex_init(core::ptr::addr_of_mut!(file.event_read_lock));
            file.event_space = 4096;
            minor.dev = dev;
            file.minor = &mut minor;
            file.filp = &mut filp;
            filp.private_data = (&mut file as *mut DrmFile).cast();
            tk_linux_kapi::list::list_add(
                core::ptr::addr_of_mut!(file.lhead),
                core::ptr::addr_of_mut!((*dev).filelist),
            );
        }

        let pending = tk_linux_kapi::memory::kzalloc(
            core::mem::size_of::<DrmPendingVblankEvent>(),
            GFP_KERNEL,
        )
        .cast::<DrmPendingVblankEvent>();
        let user_event = tk_linux_kapi::memory::kzalloc(core::mem::size_of::<DrmEvent>(), GFP_KERNEL)
            .cast::<DrmEvent>();
        assert!(!pending.is_null() && !user_event.is_null());
        // SAFETY: The fence and DRM state types are zero-initialised C-layout fixtures;
        // `init_commit` initializes the completion and reference-count fields before use.
        let mut out_fence: tk_linux_kapi::types::DmaFence = unsafe { zeroed() };
        // SAFETY: `DrmCrtc` is a zero-initialised C-layout fixture whose used fields are set below.
        let mut crtc: DrmCrtc = unsafe { zeroed() };
        // SAFETY: `DrmCrtcCommit` is initialized with `init_commit` before use.
        let mut commit: DrmCrtcCommit = unsafe { zeroed() };
        // SAFETY: CRTC-state fixtures are zero-initialised and their relevant pointers are set below.
        let mut old_crtc_state: DrmCrtcState = unsafe { zeroed() };
        // SAFETY: CRTC-state fixtures are zero-initialised and their relevant pointers are set below.
        let mut new_crtc_state: DrmCrtcState = unsafe { zeroed() };
        // SAFETY: The state-array entry is a zero-initialised C-layout fixture.
        let mut crtc_entry: DrmCrtcsState = unsafe { zeroed() };
        // SAFETY: The atomic state is a zero-initialised C-layout fixture.
        let mut state: DrmAtomicCommit = unsafe { zeroed() };
        let event_header = DrmEvent {
            r#type: 0x8000_0000,
            length: core::mem::size_of::<DrmEvent>() as u32,
        };

        // SAFETY: Allocated event objects, fence, DRM structs and file/device fixtures
        // remain live and are wired together before any helper call below.
        unsafe {
            tk_linux_kapi::dma_fence::dma_fence_init(
                &mut out_fence,
                core::ptr::addr_of!(TEST_FENCE_OPS),
                core::ptr::null_mut(),
                0,
                2,
            );
            assert!(!tk_linux_kapi::dma_fence::dma_fence_get(&mut out_fence).is_null());
            (*user_event) = event_header;
            (*pending).base.fence = &mut out_fence;
            (*pending).base.file_priv = &mut file;
            tk_linux_kapi::locks::spin_lock_init(&mut crtc.commit_lock);
            tk_linux_kapi::list::INIT_LIST_HEAD(core::ptr::addr_of_mut!(crtc.commit_list));
            init_commit(&mut commit, &mut crtc);
            commit.event = pending;
            new_crtc_state.commit = &mut commit;
            new_crtc_state.event = pending;
            crtc_entry.ptr = &mut crtc;
            crtc_entry.old_state = &mut old_crtc_state;
            crtc_entry.new_state = &mut new_crtc_state;
            state.dev = dev;
            state.crtcs = &mut crtc_entry;
            (*dev).mode_config.num_crtc = 1;
            tk_linux_kapi::list::list_add(
                core::ptr::addr_of_mut!(commit.commit_entry),
                core::ptr::addr_of_mut!(crtc.commit_list),
            );
            assert_eq!(
                crate::drm_file_upstream::drm_event_reserve_init(
                    dev,
                    &mut file,
                    &mut (*pending).base,
                    user_event,
                ),
                0
            );
        }

        // SAFETY: `state` owns a valid CRTC entry and event; the file, device,
        // fence, and commit objects stay live throughout error completion.
        unsafe { drm_atomic_helper_commit_error(&mut state, -tk_linux_kapi::config::EIO) };
        // SAFETY: `commit_entry` was linked into `crtc.commit_list` and
        // `commit_hw_done` installed the dependency in `old_crtc_state`.
        unsafe { drm_atomic_helper_commit_cleanup_done(&mut state) };

        // SAFETY: `dev` is the live device owned by the `TestDrm` fixture.
        assert!(unsafe { (*dev).unplugged.load(Ordering::Acquire) });
        assert!(commit.hw_done.is_done());
        assert!(commit.flip_done.is_done());
        assert!(commit.cleanup_done.is_done());
        assert!(new_crtc_state.event.is_null());
        assert!(commit.event.is_null());
        assert_eq!(file.event_space, 4096);
        assert!(tk_linux_kapi::list::list_empty(&file.pending_event_list));
        assert!(tk_linux_kapi::list::list_empty(&file.event_list));
        // SAFETY: `out_fence` remains live until the final put below.
        assert_eq!(
            unsafe { tk_linux_kapi::dma_fence::dma_fence_get_status(&mut out_fence) },
            -tk_linux_kapi::config::EIO
        );
        // SAFETY: `out_fence` remains live until the final put below.
        assert!(unsafe { tk_linux_kapi::dma_fence::dma_fence_is_signaled(&mut out_fence) });
        // SAFETY: The file points at the live `DrmFile` fixture, and unplugged read/poll
        // paths operate on the initialized event waitqueue and empty event lists.
        let poll = unsafe { crate::drm_file_upstream::drm_poll(&mut filp, core::ptr::null_mut()) };
        assert_ne!(poll & 0x0008, 0);
        assert_ne!(poll & 0x0010, 0);
        assert_eq!(poll & 0x0041, 0);
        // SAFETY: `filp` points at the initialized fixture and the writer does not retain
        // or dereference its null context or event-byte arguments.
        assert_eq!(
            unsafe {
                crate::drm_file_upstream::drm_read_kernel_to(
                    &mut filp,
                    4096,
                    core::ptr::null_mut(),
                    discard_event_bytes,
                )
            },
            -(crate::drm_drv_upstream::ENODEV as isize)
        );

        // SAFETY: The fence still has its original owned reference; the file remains
        // linked to the live device list and is removed before the fixture is dropped.
        unsafe {
            tk_linux_kapi::dma_fence::dma_fence_put(&mut out_fence);
            tk_linux_kapi::list::list_del(core::ptr::addr_of_mut!(file.lhead));
        }
    }
}
