// SPDX-License-Identifier: MIT
// Copyright © 2014-2019 Intel Corporation.
//
//! uC records and inline helpers transcribed from Linux v7.2.3
//! `drivers/gpu/drm/i915/gt/uc/intel_uc.h`.

#![allow(unsafe_code)]

use core::ffi::c_long;

use crate::{
    i915_request_types_upstream::DrmI915GemObject,
    intel_gsc_uc_types_upstream::{
        IntelGscUc, intel_gsc_uc_is_supported, intel_gsc_uc_is_used, intel_gsc_uc_is_wanted,
    },
    intel_guc_rc_types_upstream::{
        intel_guc_rc_is_supported, intel_guc_rc_is_used, intel_guc_rc_is_wanted,
    },
    intel_guc_slpc_upstream::{
        intel_guc_slpc_is_supported, intel_guc_slpc_is_used, intel_guc_slpc_is_wanted,
    },
    intel_guc_submission_types_upstream::{
        intel_guc_submission_is_supported, intel_guc_submission_is_used,
        intel_guc_submission_is_wanted,
    },
    intel_guc_types_upstream::{
        IntelGuc, intel_guc_is_supported, intel_guc_is_used, intel_guc_is_wanted,
    },
    intel_huc_types_upstream::{
        IntelHuc, intel_huc_is_supported, intel_huc_is_used, intel_huc_is_wanted,
    },
};

/// `struct intel_uc_ops`.
#[repr(C)]
pub struct IntelUcOps {
    pub sanitize: Option<unsafe extern "C" fn(*mut IntelUc) -> i32>,
    pub init_fw: Option<unsafe extern "C" fn(*mut IntelUc)>,
    pub fini_fw: Option<unsafe extern "C" fn(*mut IntelUc)>,
    pub init: Option<unsafe extern "C" fn(*mut IntelUc) -> i32>,
    pub fini: Option<unsafe extern "C" fn(*mut IntelUc)>,
    pub init_hw: Option<unsafe extern "C" fn(*mut IntelUc) -> i32>,
    pub fini_hw: Option<unsafe extern "C" fn(*mut IntelUc)>,
    pub resume_mappings: Option<unsafe extern "C" fn(*mut IntelUc)>,
}

/// `struct intel_uc`.
#[repr(C)]
pub struct IntelUc {
    pub ops: *const IntelUcOps,
    pub gsc: IntelGscUc,
    pub guc: IntelGuc,
    pub huc: IntelHuc,
    pub load_err_log: *mut DrmI915GemObject,
    pub reset_in_progress: bool,
    pub fw_table_invalid: bool,
}

/// Expand the `__uc_state_checker`/`uc_state_checkers` source macros.
macro_rules! state_checker {
    ($name:ident, $field:ident, $checker:path) => {
        #[inline]
        pub unsafe fn $name(uc: *mut IntelUc) -> bool {
            assert!(!uc.is_null());
            // SAFETY: caller guarantees a live IntelUc, as with the C helper.
            unsafe { $checker(core::ptr::addr_of_mut!((*uc).$field)) }
        }
    };
}

state_checker!(intel_uc_supports_guc, guc, intel_guc_is_supported);
state_checker!(intel_uc_wants_guc, guc, intel_guc_is_wanted);
state_checker!(intel_uc_uses_guc, guc, intel_guc_is_used);
state_checker!(intel_uc_supports_huc, huc, intel_huc_is_supported);
state_checker!(intel_uc_wants_huc, huc, intel_huc_is_wanted);
state_checker!(intel_uc_uses_huc, huc, intel_huc_is_used);
state_checker!(
    intel_uc_supports_guc_submission,
    guc,
    intel_guc_submission_is_supported
);
state_checker!(
    intel_uc_wants_guc_submission,
    guc,
    intel_guc_submission_is_wanted
);
state_checker!(
    intel_uc_uses_guc_submission,
    guc,
    intel_guc_submission_is_used
);
state_checker!(intel_uc_supports_guc_slpc, guc, intel_guc_slpc_is_supported);
state_checker!(intel_uc_wants_guc_slpc, guc, intel_guc_slpc_is_wanted);
state_checker!(intel_uc_uses_guc_slpc, guc, intel_guc_slpc_is_used);
state_checker!(intel_uc_supports_guc_rc, guc, intel_guc_rc_is_supported);
state_checker!(intel_uc_wants_guc_rc, guc, intel_guc_rc_is_wanted);
state_checker!(intel_uc_uses_guc_rc, guc, intel_guc_rc_is_used);
state_checker!(intel_uc_supports_gsc_uc, gsc, intel_gsc_uc_is_supported);
state_checker!(intel_uc_wants_gsc_uc, gsc, intel_gsc_uc_is_wanted);
state_checker!(intel_uc_uses_gsc_uc, gsc, intel_gsc_uc_is_used);

// Declared by intel_guc.h and implemented with the GuC submission code.
unsafe extern "C" {
    fn intel_guc_wait_for_idle(guc: *mut IntelGuc, timeout: c_long) -> i32;
}

/// `intel_uc_wait_for_idle()`.
#[inline]
pub unsafe fn intel_uc_wait_for_idle(uc: *mut IntelUc, timeout: c_long) -> i32 {
    assert!(!uc.is_null());
    // SAFETY: caller passes a live uC object and the imported GuC routine owns
    // waiting/timeout behavior.
    unsafe { intel_guc_wait_for_idle(core::ptr::addr_of_mut!((*uc).guc), timeout) }
}

/// Dispatch one optional `intel_uc_ops` callback with its C default return.
macro_rules! dispatch_int_op {
    ($name:ident, $field:ident) => {
        #[inline]
        pub unsafe fn $name(uc: *mut IntelUc) -> i32 {
            assert!(!uc.is_null());
            // SAFETY: mirrors `uc->ops->_field` and callback invocation.
            unsafe {
                match (*(*uc).ops).$field {
                    Some(callback) => callback(uc),
                    None => 0,
                }
            }
        }
    };
}

/// Dispatch one optional void-returning `intel_uc_ops` callback.
macro_rules! dispatch_void_op {
    ($name:ident, $field:ident) => {
        #[inline]
        pub unsafe fn $name(uc: *mut IntelUc) {
            assert!(!uc.is_null());
            // SAFETY: mirrors `uc->ops->_field` and callback invocation.
            unsafe {
                if let Some(callback) = (*(*uc).ops).$field {
                    callback(uc);
                }
            }
        }
    };
}

dispatch_int_op!(intel_uc_sanitize, sanitize);
dispatch_void_op!(intel_uc_fetch_firmwares, init_fw);
dispatch_void_op!(intel_uc_cleanup_firmwares, fini_fw);
dispatch_int_op!(intel_uc_init, init);
dispatch_void_op!(intel_uc_fini, fini);
dispatch_int_op!(intel_uc_init_hw, init_hw);
dispatch_void_op!(intel_uc_fini_hw, fini_hw);
dispatch_void_op!(intel_uc_resume_mappings, resume_mappings);

const _: [(); 64] = [(); core::mem::size_of::<IntelUcOps>()];
const _: [(); 8] = [(); core::mem::align_of::<IntelUcOps>()];
const _: [(); 0] = [(); core::mem::offset_of!(IntelUcOps, sanitize)];
const _: [(); 8] = [(); core::mem::offset_of!(IntelUcOps, init_fw)];
const _: [(); 16] = [(); core::mem::offset_of!(IntelUcOps, fini_fw)];
const _: [(); 24] = [(); core::mem::offset_of!(IntelUcOps, init)];
const _: [(); 32] = [(); core::mem::offset_of!(IntelUcOps, fini)];
const _: [(); 40] = [(); core::mem::offset_of!(IntelUcOps, init_hw)];
const _: [(); 48] = [(); core::mem::offset_of!(IntelUcOps, fini_hw)];
const _: [(); 56] = [(); core::mem::offset_of!(IntelUcOps, resume_mappings)];

// Target x86_64 Linux 7.2.3, CONFIG_DRM_I915_SELFTEST=n. Expected sizes of
// the owner records are asserted by their respective header bindings.
const _: [(); 576] = [(); core::mem::size_of::<IntelGscUc>()];
const _: [(); 1768] = [(); core::mem::size_of::<IntelGuc>()];
const _: [(); 608] = [(); core::mem::size_of::<IntelHuc>()];
const _: [(); 0] = [(); core::mem::offset_of!(IntelUc, ops)];
const _: [(); 8] = [(); core::mem::offset_of!(IntelUc, gsc)];
const _: [(); 584] = [(); core::mem::offset_of!(IntelUc, guc)];
const _: [(); 2352] = [(); core::mem::offset_of!(IntelUc, huc)];
const _: [(); 2960] = [(); core::mem::offset_of!(IntelUc, load_err_log)];
const _: [(); 2968] = [(); core::mem::offset_of!(IntelUc, reset_in_progress)];
const _: [(); 2969] = [(); core::mem::offset_of!(IntelUc, fw_table_invalid)];
const _: [(); 2976] = [(); core::mem::size_of::<IntelUc>()];
const _: [(); 8] = [(); core::mem::align_of::<IntelUc>()];
