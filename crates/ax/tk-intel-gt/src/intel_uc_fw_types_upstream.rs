// SPDX-License-Identifier: MIT
// Copyright © 2014-2019 Intel Corporation.
//
//! Firmware type and constant bindings transcribed from Linux v7.2.3
//! `drivers/gpu/drm/i915/gt/uc/intel_uc_fw.h`.

use core::{
    ffi::c_char,
    mem::{align_of, offset_of, size_of},
};

use crate::{
    i915_request_types_upstream::DrmI915GemObject,
    i915_vma_resource_types_upstream::I915VmaResource, intel_context_upstream::I915Vma,
    intel_gt_types_upstream::IntelGt, linux_i915_private::DrmI915Private, linux_print::DrmPrinter,
};

/// Home of GuC, HuC, and DMC firmware blobs.
pub const INTEL_UC_FIRMWARE_URL: &str =
    "https://git.kernel.org/pub/scm/linux/kernel/git/firmware/linux-firmware.git/tree/i915";

/// Firmware status enum `intel_uc_fw_status` (C `int` representation).
#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntelUcFwStatusValue {
    NotSupported  = -1,
    Uninitialized = 0,
    Disabled      = 1,
    Selected      = 2,
    Missing       = 3,
    Error         = 4,
    Available     = 5,
    InitFail      = 6,
    Loadable      = 7,
    LoadFail      = 8,
    Transferred   = 9,
    Running       = 10,
}

/// C enum values are stored as an int and may include future/invalid values.
pub type IntelUcFwStatus = i32;
pub const INTEL_UC_FIRMWARE_NOT_SUPPORTED: IntelUcFwStatus = -1;
pub const INTEL_UC_FIRMWARE_UNINITIALIZED: IntelUcFwStatus = 0;
pub const INTEL_UC_FIRMWARE_DISABLED: IntelUcFwStatus = 1;
pub const INTEL_UC_FIRMWARE_SELECTED: IntelUcFwStatus = 2;
pub const INTEL_UC_FIRMWARE_MISSING: IntelUcFwStatus = 3;
pub const INTEL_UC_FIRMWARE_ERROR: IntelUcFwStatus = 4;
pub const INTEL_UC_FIRMWARE_AVAILABLE: IntelUcFwStatus = 5;
pub const INTEL_UC_FIRMWARE_INIT_FAIL: IntelUcFwStatus = 6;
pub const INTEL_UC_FIRMWARE_LOADABLE: IntelUcFwStatus = 7;
pub const INTEL_UC_FIRMWARE_LOAD_FAIL: IntelUcFwStatus = 8;
pub const INTEL_UC_FIRMWARE_TRANSFERRED: IntelUcFwStatus = 9;
pub const INTEL_UC_FIRMWARE_RUNNING: IntelUcFwStatus = 10;

/// Firmware type enum `intel_uc_fw_type` (C `int` representation).
#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntelUcFwTypeValue {
    Guc = 0,
    Huc = 1,
    Gsc = 2,
}

pub type IntelUcFwType = i32;
pub const INTEL_UC_FW_TYPE_GUC: IntelUcFwType = 0;
pub const INTEL_UC_FW_TYPE_HUC: IntelUcFwType = 1;
pub const INTEL_UC_FW_TYPE_GSC: IntelUcFwType = 2;
pub const INTEL_UC_FW_NUM_TYPES: i32 = 3;

/// `struct intel_uc_fw_ver`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IntelUcFwVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    pub build: u32,
}

/// `struct intel_uc_fw_file`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IntelUcFwFile {
    pub path: *const c_char,
    pub ver: IntelUcFwVersion,
}

/// C's anonymous `status`/`__status` union in `struct intel_uc_fw`.
#[repr(C)]
#[derive(Clone, Copy)]
pub union IntelUcFwStatusUnion {
    pub status: IntelUcFwStatus,
    pub __status: IntelUcFwStatus,
}

/// `struct intel_uc_fw`.
#[repr(C)]
pub struct IntelUcFw {
    pub r#type: IntelUcFwType,
    /// Anonymous C union whose public and internal aliases share storage.
    pub status: IntelUcFwStatusUnion,
    pub file_wanted: IntelUcFwFile,
    pub file_selected: IntelUcFwFile,
    pub user_overridden: bool,
    pub size: usize,
    pub obj: *mut DrmI915GemObject,
    pub needs_ggtt_mapping: bool,
    pub vma_res: I915VmaResource,
    pub rsa_data: *mut I915Vma,
    pub rsa_size: u32,
    pub ucode_size: u32,
    pub private_data_size: u32,
    pub dma_start_offset: u32,
    pub has_gsc_headers: bool,
}

/// Read the C status union with the header's precondition check.
///
/// # Safety
/// `uc_fw` must point to a live firmware record.
pub unsafe fn __intel_uc_fw_status(uc_fw: *const IntelUcFw) -> IntelUcFwStatus {
    assert!(!uc_fw.is_null());
    let status = unsafe { (*uc_fw).status.status };
    gem_bug_on!(status == INTEL_UC_FIRMWARE_UNINITIALIZED);
    status
}

/// `intel_uc_fw_is_supported()` from intel_uc_fw.h.
///
/// # Safety
/// `uc_fw` must point to a live firmware record.
pub unsafe fn intel_uc_fw_is_supported(uc_fw: *const IntelUcFw) -> bool {
    unsafe { __intel_uc_fw_status(uc_fw) != INTEL_UC_FIRMWARE_NOT_SUPPORTED }
}

/// `intel_uc_fw_is_enabled()` from intel_uc_fw.h.
///
/// # Safety
/// `uc_fw` must point to a live firmware record.
pub unsafe fn intel_uc_fw_is_enabled(uc_fw: *const IntelUcFw) -> bool {
    unsafe { __intel_uc_fw_status(uc_fw) > INTEL_UC_FIRMWARE_DISABLED }
}

/// `intel_uc_fw_is_available()` from intel_uc_fw.h.
///
/// # Safety
/// `uc_fw` must point to a live firmware record.
pub unsafe fn intel_uc_fw_is_available(uc_fw: *const IntelUcFw) -> bool {
    unsafe { __intel_uc_fw_status(uc_fw) >= INTEL_UC_FIRMWARE_AVAILABLE }
}

/// `intel_uc_fw_is_running()` from intel_uc_fw.h.
///
/// # Safety
/// `uc_fw` must point to a live firmware record.
pub unsafe fn intel_uc_fw_is_running(uc_fw: *const IntelUcFw) -> bool {
    unsafe { __intel_uc_fw_status(uc_fw) == INTEL_UC_FIRMWARE_RUNNING }
}

/// Reserve `SZ_2M` per firmware image in the GGTT.
pub const INTEL_UC_RSVD_GGTT_PER_FW: i32 = 2 * 1024 * 1024;

const _: [(); 4] = [(); size_of::<IntelUcFwStatusValue>()];
const _: [(); 4] = [(); size_of::<IntelUcFwTypeValue>()];
const _: [(); 16] = [(); size_of::<IntelUcFwVersion>()];
const _: [(); 4] = [(); align_of::<IntelUcFwVersion>()];
const _: [(); 24] = [(); size_of::<IntelUcFwFile>()];
const _: [(); 8] = [(); offset_of!(IntelUcFwFile, ver)];
const _: [(); 8] = [(); align_of::<IntelUcFwFile>()];

// x86_64 Linux v7.2.3 layout with CONFIG_DRM_I915_CAPTURE_ERROR=y. The
// VMA-resource owner binding carries the configuration-sensitive embedded
// record; these outer offsets/sizes check its required placement.
const _: [(); 0] = [(); offset_of!(IntelUcFw, r#type)];
const _: [(); 4] = [(); offset_of!(IntelUcFw, status)];
const _: [(); 8] = [(); offset_of!(IntelUcFw, file_wanted)];
const _: [(); 32] = [(); offset_of!(IntelUcFw, file_selected)];
const _: [(); 56] = [(); offset_of!(IntelUcFw, user_overridden)];
const _: [(); 64] = [(); offset_of!(IntelUcFw, size)];
const _: [(); 72] = [(); offset_of!(IntelUcFw, obj)];
const _: [(); 80] = [(); offset_of!(IntelUcFw, needs_ggtt_mapping)];
const _: [(); 88] = [(); offset_of!(IntelUcFw, vma_res)];
const _: [(); 384] = [(); offset_of!(IntelUcFw, rsa_data)];
const _: [(); 392] = [(); offset_of!(IntelUcFw, rsa_size)];
const _: [(); 396] = [(); offset_of!(IntelUcFw, ucode_size)];
const _: [(); 400] = [(); offset_of!(IntelUcFw, private_data_size)];
const _: [(); 404] = [(); offset_of!(IntelUcFw, dma_start_offset)];
const _: [(); 408] = [(); offset_of!(IntelUcFw, has_gsc_headers)];
const _: [(); 8] = [(); align_of::<IntelUcFw>()];
const _: [(); 416] = [(); size_of::<IntelUcFw>()];
