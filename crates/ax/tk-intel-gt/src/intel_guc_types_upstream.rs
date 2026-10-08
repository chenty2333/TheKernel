// SPDX-License-Identifier: MIT
// Copyright © 2014-2019 Intel Corporation.
//
//! i915-owned types and constants transcribed from Linux v7.2.3
//! `drivers/gpu/drm/i915/gt/uc/intel_guc.h`.
//!
//! This is the type/constant binding for the header. Inline operations and
//! non-inline declarations remain in their owning GuC implementation bindings.

use core::ffi::{c_ulong, c_void};

use crate::{
    guc_config::GUC_CTL_MAX_DWORDS,
    i915_request_types_upstream::I915Request,
    i915_scheduler_types_upstream::I915SchedEngine,
    intel_context_upstream::{I915Vma, WaitQueueHead},
    intel_engine_cs_upstream::{AtomicT, DelayedWork, ListHead, Mutex, Spinlock, WorkStruct},
    intel_engine_types_upstream::{ForcewakeDomains, IntelEngineMask},
    intel_guc_ct_types_upstream::IntelGucCt,
    intel_guc_log_types_upstream::IntelGucLog,
    intel_guc_slpc_types_upstream::IntelGucSlpc,
    intel_uc_fw_types_upstream::{
        __intel_uc_fw_status, INTEL_UC_FIRMWARE_SELECTED, IntelUcFw, IntelUcFwVersion,
        intel_uc_fw_is_available, intel_uc_fw_is_enabled, intel_uc_fw_is_running,
        intel_uc_fw_is_supported,
    },
    intel_workarounds_types_upstream::I915RegT as I915Reg,
    linux::{idr::Ida, iosys_map::IosysMap, xarray::XArray},
};

// Pointer-only types declared here or consumed only through pointers. The
// pointed-to layouts are owned by their respective upstream headers.
#[repr(C)]
pub struct GucAdsBlob {
    _opaque: [u8; 0],
}

#[repr(C)]
pub struct IntelGucStateCapture {
    _opaque: [u8; 0],
}

#[repr(C)]
pub struct Dentry {
    _opaque: [u8; 0],
}

#[repr(C)]
pub struct GucMmioReg {
    _opaque: [u8; 0],
}

/// Top-level GuC state from `struct intel_guc`.
#[repr(C)]
pub struct IntelGuc {
    /// GuC firmware.
    pub fw: IntelUcFw,
    /// GuC logging state.
    pub log: IntelGucLog,
    /// GuC command transport.
    pub ct: IntelGucCt,
    /// SLPC state.
    pub slpc: IntelGucSlpc,
    /// Error-state capture state.
    pub capture: *mut IntelGucStateCapture,

    /// debugfs node.
    pub dbgfs_node: *mut Dentry,
    /// Global engine used to submit requests to GuC.
    pub sched_engine: *mut I915SchedEngine,
    /// Request held while submission is stalled.
    pub stalled_request: *mut I915Request,
    /// C anonymous enum `submission_stall_reason` (int-sized).
    pub submission_stall_reason: i32,

    /// GuC receive-interrupt state lock.
    pub irq_lock: Spinlock,
    /// Enabled G2H event mask.
    pub msg_enabled_mask: u32,
    /// Outstanding submission G2H responses.
    pub outstanding_submission_g2h: AtomicT,
    /// Pending TLB invalidation requests.
    pub tlb_lookup: XArray,
    /// Initial/fallback waiter ID.
    pub serial_slot: u32,
    /// Next sequence number to allocate.
    pub next_seqno: u32,

    /// GuC interrupt callbacks.
    pub interrupts: IntelGucInterrupts,
    /// Submission state protected by its embedded lock.
    pub submission_state: IntelGucSubmissionState,

    pub submission_supported: bool,
    pub submission_selected: bool,
    pub submission_initialized: bool,
    pub submission_version: IntelUcFwVersion,
    pub rc_supported: bool,
    pub rc_selected: bool,

    /// GuC ADS allocation and mappings.
    pub ads_vma: *mut I915Vma,
    pub ads_map: IosysMap,
    pub ads_regset_size: u32,
    pub ads_regset_count: [u32; crate::intel_engine_cs_upstream::I915_NUM_ENGINES],
    pub ads_regset: *mut GucMmioReg,
    pub ads_golden_ctxt_size: u32,
    pub ads_waklv_size: u32,
    pub ads_capture_size: u32,

    /// GuC LRC descriptor pool allocation and mapping.
    pub lrc_desc_pool_v69: *mut I915Vma,
    pub lrc_desc_pool_vaddr_v69: *mut c_void,

    /// Context lookup by GuC ID.
    pub context_lookup: XArray,
    /// Firmware initialization parameters.
    pub params: [u32; GUC_CTL_MAX_DWORDS],
    /// GuC MMIO-send register configuration.
    pub send_regs: IntelGucSendRegs,
    /// Interrupt notification register.
    pub notify_reg: I915Reg,
    /// Deferred notification bitmask.
    pub mmio_msg: u32,
    /// Serializes GuC send actions.
    pub send_mutex: Mutex,
    /// Extended GT timestamp and sampling state.
    pub timestamp: IntelGucTimestamp,

    /// Worker that forces a GuC reset after a dead-GUC event.
    pub dead_guc_worker: WorkStruct,
    /// Jiffies timestamp of the previous dead-GUC event.
    pub last_dead_guc_jiffies: c_ulong,

    #[cfg(CONFIG_DRM_I915_SELFTEST)]
    pub number_guc_id_stolen: i32,
    #[cfg(CONFIG_DRM_I915_SELFTEST)]
    pub fast_response_selftest: u32,
}

/// `struct intel_guc`'s anonymous interrupt sub-structure.
#[repr(C)]
pub struct IntelGucInterrupts {
    pub enabled: bool,
    pub reset: Option<unsafe extern "C" fn(*mut IntelGuc)>,
    pub enable: Option<unsafe extern "C" fn(*mut IntelGuc)>,
    pub disable: Option<unsafe extern "C" fn(*mut IntelGuc)>,
}

/// `struct intel_guc`'s anonymous submission-state sub-structure.
#[repr(C)]
pub struct IntelGucSubmissionState {
    pub lock: Spinlock,
    pub guc_ids: Ida,
    pub num_guc_ids: i32,
    pub guc_ids_bitmap: *mut c_ulong,
    pub guc_id_list: ListHead,
    pub guc_ids_in_use: u32,
    pub destroyed_contexts: ListHead,
    pub destroyed_worker: WorkStruct,
    pub reset_fail_worker: WorkStruct,
    pub reset_fail_mask: IntelEngineMask,
    pub sched_disable_delay_ms: u32,
    pub sched_disable_gucid_threshold: u32,
}

/// `struct intel_guc`'s anonymous send-register sub-structure.
#[repr(C)]
pub struct IntelGucSendRegs {
    pub base: u32,
    pub count: u32,
    pub fw_domains: ForcewakeDomains,
}

/// `struct intel_guc`'s anonymous timestamp sub-structure.
#[repr(C)]
pub struct IntelGucTimestamp {
    pub lock: Spinlock,
    pub gt_stamp: u64,
    pub ping_delay: c_ulong,
    pub work: DelayedWork,
    pub shift: u32,
    pub last_stat_jiffies: c_ulong,
}

/// `struct intel_guc_tlb_wait`.
#[repr(C)]
pub struct IntelGucTlbWait {
    pub wq: WaitQueueHead,
    pub busy: bool,
}

// The source's anonymous enum uses C's int representation. Keep the stored
// member as i32 so arbitrary C enum values stay representable at the boundary.
#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntelGucSubmissionStallReasonValue {
    None            = 0,
    RegisterContext = 1,
    MoveLrcTail     = 2,
    AddRequest      = 3,
}

pub type IntelGucSubmissionStallReason = i32;
pub const STALL_NONE: IntelGucSubmissionStallReason =
    IntelGucSubmissionStallReasonValue::None as i32;
pub const STALL_REGISTER_CONTEXT: IntelGucSubmissionStallReason =
    IntelGucSubmissionStallReasonValue::RegisterContext as i32;
pub const STALL_MOVE_LRC_TAIL: IntelGucSubmissionStallReason =
    IntelGucSubmissionStallReasonValue::MoveLrcTail as i32;
pub const STALL_ADD_REQUEST: IntelGucSubmissionStallReason =
    IntelGucSubmissionStallReasonValue::AddRequest as i32;

/// Highest GGTT address accepted by the GuC, from `GUC_GGTT_TOP`.
pub const GUC_GGTT_TOP: u32 = 0xfee0_0000;

// x86_64 target layout derived from Linux v7.2.3 and the checked-in config.
const _: [(); 160] = [(); core::mem::size_of::<IntelGucSubmissionState>()];
const _: [(); 128] = [(); core::mem::size_of::<IntelGucTimestamp>()];
const _: [(); 32] = [(); core::mem::size_of::<IntelGucTlbWait>()];
const _: [(); 1304] = [(); core::mem::offset_of!(IntelGuc, ads_vma)];
const _: [(); 1496] = [(); core::mem::offset_of!(IntelGuc, params)];
const _: [(); 1600] = [(); core::mem::offset_of!(IntelGuc, timestamp)];
const _: [(); 1760] = [(); core::mem::offset_of!(IntelGuc, last_dead_guc_jiffies)];
const _: [(); 1768] = [(); core::mem::size_of::<IntelGuc>()];

/// `intel_guc_is_supported()` from intel_guc.h.
///
/// # Safety
/// `guc` must point to a live GuC record.
pub unsafe fn intel_guc_is_supported(guc: *const IntelGuc) -> bool {
    assert!(!guc.is_null());
    let fw = unsafe { core::ptr::addr_of!((*guc).fw) };
    unsafe { intel_uc_fw_is_supported(fw) }
}

/// `intel_guc_is_wanted()` from intel_guc.h.
///
/// # Safety
/// `guc` must point to a live GuC record.
pub unsafe fn intel_guc_is_wanted(guc: *const IntelGuc) -> bool {
    assert!(!guc.is_null());
    let fw = unsafe { core::ptr::addr_of!((*guc).fw) };
    unsafe { intel_uc_fw_is_enabled(fw) }
}

/// `intel_guc_is_fw_running()` from intel_guc.h.
///
/// # Safety
/// `guc` must point to a live GuC record.
pub unsafe fn intel_guc_is_fw_running(guc: *const IntelGuc) -> bool {
    assert!(!guc.is_null());
    let fw = unsafe { core::ptr::addr_of!((*guc).fw) };
    unsafe { intel_uc_fw_is_running(fw) }
}

/// `intel_guc_is_used()` from intel_guc.h.
///
/// # Safety
/// `guc` must point to a live GuC record.
pub unsafe fn intel_guc_is_used(guc: *mut IntelGuc) -> bool {
    assert!(!guc.is_null());
    let fw = unsafe { core::ptr::addr_of!((*guc).fw) };
    let status = unsafe { __intel_uc_fw_status(fw) };
    gem_bug_on!(status == INTEL_UC_FIRMWARE_SELECTED);
    unsafe { intel_uc_fw_is_available(fw) }
}
