// SPDX-License-Identifier: MIT
// Copyright © 2014-2019 Intel Corporation.
//
//! HuC records, enums, declarations, and inline helpers transcribed from Linux
//! v7.2.3 `drivers/gpu/drm/i915/gt/uc/intel_huc.h`.

#![allow(unsafe_code)]

use crate::{
    i915_request_types_upstream::DrmPrinter,
    intel_context_types_upstream::I915SwFence,
    intel_context_upstream::{Hrtimer, I915Vma},
    intel_uc_fw_types_upstream::{
        __intel_uc_fw_status, INTEL_UC_FIRMWARE_AVAILABLE, INTEL_UC_FIRMWARE_DISABLED,
        INTEL_UC_FIRMWARE_NOT_SUPPORTED, INTEL_UC_FIRMWARE_SELECTED, IntelUcFw,
        intel_uc_fw_is_available,
    },
    intel_workarounds_types_upstream::I915RegT,
    linux::gem_memory::NotifierBlock,
};

// Header forward declarations, used only as pointer types.
#[repr(C)]
pub struct BusType {
    _opaque: [u8; 0],
}

/// `enum intel_huc_delayed_load_status` (C `int` representation).
#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntelHucDelayedLoadStatusValue {
    WaitingOnGsc     = 0,
    WaitingOnPxp     = 1,
    DelayedLoadError = 2,
}

pub type IntelHucDelayedLoadStatus = i32;
pub const INTEL_HUC_WAITING_ON_GSC: IntelHucDelayedLoadStatus = 0;
pub const INTEL_HUC_WAITING_ON_PXP: IntelHucDelayedLoadStatus = 1;
pub const INTEL_HUC_DELAYED_LOAD_ERROR: IntelHucDelayedLoadStatus = 2;

/// `enum intel_huc_authentication_type` (C `int` representation).
#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntelHucAuthenticationTypeValue {
    ByGuc    = 0,
    ByGsc    = 1,
    MaxModes = 2,
}

pub type IntelHucAuthenticationType = i32;
pub const INTEL_HUC_AUTH_BY_GUC: IntelHucAuthenticationType = 0;
pub const INTEL_HUC_AUTH_BY_GSC: IntelHucAuthenticationType = 1;
pub const INTEL_HUC_AUTH_MAX_MODES: IntelHucAuthenticationType = 2;

/// Anonymous authentication-status record inside `struct intel_huc`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IntelHucAuthStatus {
    pub reg: I915RegT,
    pub mask: u32,
    pub value: u32,
}

/// Anonymous deferred-load record inside `struct intel_huc`.
#[repr(C)]
pub struct IntelHucDelayedLoad {
    pub fence: I915SwFence,
    pub timer: Hrtimer,
    pub nb: NotifierBlock,
    pub status: IntelHucDelayedLoadStatus,
}

/// `struct intel_huc`.
#[repr(C)]
pub struct IntelHuc {
    /// Generic uC firmware management.
    pub fw: IntelUcFw,
    pub status: [IntelHucAuthStatus; INTEL_HUC_AUTH_MAX_MODES as usize],
    pub delayed_load: IntelHucDelayedLoad,
    pub heci_pkt: *mut I915Vma,
    pub loaded_via_gsc: bool,
}

// Non-inline declarations owned by intel_huc.c, matching the header ABI.
unsafe extern "C" {
    pub fn intel_huc_sanitize(huc: *mut IntelHuc) -> i32;
    pub fn intel_huc_init_early(huc: *mut IntelHuc);
    pub fn intel_huc_fini_late(huc: *mut IntelHuc);
    pub fn intel_huc_init(huc: *mut IntelHuc) -> i32;
    pub fn intel_huc_fini(huc: *mut IntelHuc);
    pub fn intel_huc_auth(huc: *mut IntelHuc, auth_type: IntelHucAuthenticationType) -> i32;
    pub fn intel_huc_wait_for_auth_complete(
        huc: *mut IntelHuc,
        auth_type: IntelHucAuthenticationType,
    ) -> i32;
    pub fn intel_huc_is_authenticated(
        huc: *mut IntelHuc,
        auth_type: IntelHucAuthenticationType,
    ) -> bool;
    pub fn intel_huc_check_status(huc: *mut IntelHuc) -> i32;
    pub fn intel_huc_update_auth_status(huc: *mut IntelHuc);
    pub fn intel_huc_register_gsc_notifier(huc: *mut IntelHuc, bus: *const BusType);
    pub fn intel_huc_unregister_gsc_notifier(huc: *mut IntelHuc, bus: *const BusType);
    pub fn intel_huc_load_status(huc: *mut IntelHuc, printer: *mut DrmPrinter);
}

/// `intel_huc_is_supported()` from the header.
///
/// # Safety
/// `huc` must point to a live `intel_huc`.
pub unsafe fn intel_huc_is_supported(huc: *mut IntelHuc) -> bool {
    assert!(!huc.is_null());
    let fw = unsafe { core::ptr::addr_of!((*huc).fw) };
    unsafe { __intel_uc_fw_status(fw) != INTEL_UC_FIRMWARE_NOT_SUPPORTED }
}

/// `intel_huc_is_wanted()` from the header.
///
/// # Safety
/// `huc` must point to a live `intel_huc`.
pub unsafe fn intel_huc_is_wanted(huc: *mut IntelHuc) -> bool {
    assert!(!huc.is_null());
    let fw = unsafe { core::ptr::addr_of!((*huc).fw) };
    unsafe { __intel_uc_fw_status(fw) > INTEL_UC_FIRMWARE_DISABLED }
}

/// `intel_huc_is_used()` from the header.
///
/// # Safety
/// `huc` must point to a live `intel_huc`.
pub unsafe fn intel_huc_is_used(huc: *mut IntelHuc) -> bool {
    assert!(!huc.is_null());
    let fw = unsafe { core::ptr::addr_of!((*huc).fw) };
    unsafe {
        gem_bug_on!(__intel_uc_fw_status(fw) == INTEL_UC_FIRMWARE_SELECTED);
        intel_uc_fw_is_available(fw)
    }
}

/// `intel_huc_is_loaded_by_gsc()` from the header.
///
/// # Safety
/// `huc` must point to a live `intel_huc`.
pub unsafe fn intel_huc_is_loaded_by_gsc(huc: *const IntelHuc) -> bool {
    assert!(!huc.is_null());
    unsafe { (*huc).loaded_via_gsc }
}

/// `intel_huc_wait_required()` from the header.
///
/// # Safety
/// `huc` must point to a live `intel_huc`.
pub unsafe fn intel_huc_wait_required(huc: *mut IntelHuc) -> bool {
    assert!(!huc.is_null());
    unsafe {
        intel_huc_is_used(huc)
            && intel_huc_is_loaded_by_gsc(huc)
            && !intel_huc_is_authenticated(huc, INTEL_HUC_AUTH_BY_GSC)
    }
}

const _: [(); 4] = [(); core::mem::size_of::<IntelHucDelayedLoadStatusValue>()];
const _: [(); 4] = [(); core::mem::size_of::<IntelHucAuthenticationTypeValue>()];
const _: [(); 12] = [(); core::mem::size_of::<IntelHucAuthStatus>()];
const _: [(); 4] = [(); core::mem::align_of::<IntelHucAuthStatus>()];
const _: [(); 40] = [(); core::mem::size_of::<I915SwFence>()];
const _: [(); 80] = [(); core::mem::size_of::<Hrtimer>()];
const _: [(); 24] = [(); core::mem::size_of::<NotifierBlock>()];

// x86_64 Linux v7.2.3 target; no HuC-specific conditional fields are enabled.
const _: [(); 152] = [(); core::mem::size_of::<IntelHucDelayedLoad>()];
const _: [(); 8] = [(); core::mem::align_of::<IntelHucDelayedLoad>()];
const _: [(); 0] = [(); core::mem::offset_of!(IntelHucDelayedLoad, fence)];
const _: [(); 40] = [(); core::mem::offset_of!(IntelHucDelayedLoad, timer)];
const _: [(); 120] = [(); core::mem::offset_of!(IntelHucDelayedLoad, nb)];
const _: [(); 144] = [(); core::mem::offset_of!(IntelHucDelayedLoad, status)];
const _: [(); 608] = [(); core::mem::size_of::<IntelHuc>()];
const _: [(); 8] = [(); core::mem::align_of::<IntelHuc>()];
const _: [(); 0] = [(); core::mem::offset_of!(IntelHuc, fw)];
const _: [(); 416] = [(); core::mem::offset_of!(IntelHuc, status)];
const _: [(); 440] = [(); core::mem::offset_of!(IntelHuc, delayed_load)];
const _: [(); 592] = [(); core::mem::offset_of!(IntelHuc, heci_pkt)];
const _: [(); 600] = [(); core::mem::offset_of!(IntelHuc, loaded_via_gsc)];
