// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
//
//! Source-order type transcription of Linux v7.2.3
//! `drivers/gpu/drm/i915/gt/intel_gt_types.h`.
//!
//! This is a separate header binding from `intel_engine_types_upstream`.
//! `IntelGt` and its nested i915 records are defined here; root integration
//! must switch consumers rather than editing or aliasing the existing record.

use core::ffi::{c_char, c_ulong};

use crate::{
    i915_vma_types_upstream::I915Vma,
    intel_context_types_upstream::IntelWakerefT,
    intel_engine_cs_upstream::{
        AtomicT, DelayedWork, I915PerfGt, Kobject, ListHead, LlistHead, Mutex, SeqcountMutex,
        Spinlock, WorkStruct,
    },
    intel_engine_types_upstream::{
        I915_NUM_ENGINES, IntelEngineCs, IntelEngineMask, MAX_ENGINE_CLASS, MAX_ENGINE_INSTANCE,
    },
    intel_gsc_types_upstream::IntelGsc,
    intel_gt_buffer_pool_types_upstream::IntelGtBufferPool,
    intel_gtt_api_upstream::{I915AddressSpace, I915Ggtt},
    intel_hwconfig_types_upstream::IntelHwconfig,
    intel_llc_types_upstream::IntelLlc,
    intel_migrate_types_upstream::IntelMigrate,
    intel_rc6_types_upstream::IntelRc6,
    intel_reset_types_upstream::IntelReset,
    intel_rps_types_upstream::IntelRps,
    intel_sseu_types_upstream::SseuDevInfo,
    intel_uc_types_upstream::IntelUc,
    intel_uncore_types_upstream::IntelUncore,
    intel_wakeref_types_upstream::IntelWakeref,
    intel_wopcm_types_upstream::IntelWopcm,
    intel_workarounds_types_upstream::I915WaList,
    linux_i915_private::DrmI915Private,
};

// Linux x86_64 phys_addr_t representation used by this target.
pub type PhysAddrT = u64;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IntelMmioRange {
    pub start: u32,
    pub end: u32,
}

#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntelSteeringTypeValue {
    L3Bank           = 0,
    Mslice           = 1,
    Lncf             = 2,
    Gam              = 3,
    Dss              = 4,
    Oaddrm           = 5,
    Instance0        = 6,
    NumSteeringTypes = 7,
}
pub type IntelSteeringType = i32;
pub const L3BANK: IntelSteeringType = IntelSteeringTypeValue::L3Bank as i32;
pub const MSLICE: IntelSteeringType = IntelSteeringTypeValue::Mslice as i32;
pub const LNCF: IntelSteeringType = IntelSteeringTypeValue::Lncf as i32;
pub const GAM: IntelSteeringType = IntelSteeringTypeValue::Gam as i32;
pub const DSS: IntelSteeringType = IntelSteeringTypeValue::Dss as i32;
pub const OADDRM: IntelSteeringType = IntelSteeringTypeValue::Oaddrm as i32;
pub const INSTANCE0: IntelSteeringType = IntelSteeringTypeValue::Instance0 as i32;
pub const NUM_STEERING_TYPES: IntelSteeringType = IntelSteeringTypeValue::NumSteeringTypes as i32;

#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntelSubmissionMethodValue {
    Ring = 0,
    Elsp = 1,
    Guc  = 2,
}
pub type IntelSubmissionMethod = i32;
pub const INTEL_SUBMISSION_RING: IntelSubmissionMethod = IntelSubmissionMethodValue::Ring as i32;
pub const INTEL_SUBMISSION_ELSP: IntelSubmissionMethod = IntelSubmissionMethodValue::Elsp as i32;
pub const INTEL_SUBMISSION_GUC: IntelSubmissionMethod = IntelSubmissionMethodValue::Guc as i32;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct GtDefaults {
    pub min_freq: u32,
    pub max_freq: u32,
    pub rps_up_threshold: u8,
    pub rps_down_threshold: u8,
}

#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntelGtTypeValue {
    Primary = 0,
    Tile    = 1,
    Media   = 2,
}
pub type IntelGtType = i32;
pub const GT_PRIMARY: IntelGtType = IntelGtTypeValue::Primary as i32;
pub const GT_TILE: IntelGtType = IntelGtTypeValue::Tile as i32;
pub const GT_MEDIA: IntelGtType = IntelGtTypeValue::Media as i32;

#[repr(C)]
pub struct IntelGtTlb {
    pub invalidate_lock: Mutex,
    pub seqno: SeqcountMutex,
}

#[repr(C)]
pub struct IntelGtTimelines {
    pub lock: Spinlock,
    pub active_list: ListHead,
}

#[repr(C)]
pub struct IntelGtRequests {
    pub retire_work: DelayedWork,
}

#[repr(C)]
pub struct IntelGtWatchdog {
    pub list: LlistHead,
    pub work: WorkStruct,
}

#[repr(C)]
pub struct IntelGtStats {
    pub active: bool,
    pub lock: SeqcountMutex,
    pub total: i64,
    pub start: i64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IntelGtCcs {
    pub cslices: IntelEngineMask,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IntelGtSteering {
    pub groupid: u8,
    pub instanceid: u8,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IntelGtInfo {
    pub id: u32,
    pub engine_mask: IntelEngineMask,
    pub l3bank_mask: u32,
    pub num_engines: u8,
    pub sfc_mask: u8,
    pub vdbox_sfc_access: u8,
    pub sseu: SseuDevInfo,
    pub mslice_mask: c_ulong,
    pub hwconfig: IntelHwconfig,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IntelGtMocs {
    pub uc_index: u8,
    pub wb_index: u8,
}

#[repr(C)]
pub struct IntelGt {
    pub i915: *mut DrmI915Private,
    pub name: *const c_char,
    pub type_: IntelGtType,

    pub uncore: *mut IntelUncore,
    pub ggtt: *mut I915Ggtt,

    pub uc: IntelUc,
    pub gsc: IntelGsc,
    pub wopcm: IntelWopcm,

    pub tlb: IntelGtTlb,

    pub wa_list: I915WaList,

    pub timelines: IntelGtTimelines,

    pub requests: IntelGtRequests,

    pub watchdog: IntelGtWatchdog,

    pub wakeref: IntelWakeref,
    pub user_wakeref: AtomicT,

    pub closed_vma: ListHead,
    pub closed_lock: Spinlock,

    pub last_init_time: i64,
    pub reset: IntelReset,

    pub awake: IntelWakerefT,

    pub clock_frequency: u32,
    pub clock_period_ns: u32,

    pub llc: IntelLlc,
    pub rc6: IntelRc6,
    pub rps: IntelRps,

    pub irq_lock: *mut Spinlock,
    pub gt_imr: u32,
    pub pm_ier: u32,
    pub pm_imr: u32,

    pub pm_guc_events: u32,

    pub stats: IntelGtStats,

    pub engine: [*mut IntelEngineCs; I915_NUM_ENGINES as usize],
    pub engine_class:
        [[*mut IntelEngineCs; MAX_ENGINE_INSTANCE as usize + 1]; MAX_ENGINE_CLASS as usize + 1],
    pub submission_method: IntelSubmissionMethod,

    pub ccs: IntelGtCcs,

    pub vm: *mut I915AddressSpace,

    pub buffer_pool: IntelGtBufferPool,

    pub scratch: *mut I915Vma,

    pub migrate: IntelMigrate,

    pub steering_table: [*const IntelMmioRange; NUM_STEERING_TYPES as usize],

    pub default_steering: IntelGtSteering,

    pub mcr_lock: Spinlock,

    pub phys_addr: PhysAddrT,

    pub info: IntelGtInfo,

    pub mocs: IntelGtMocs,

    pub sysfs_gt: Kobject,

    pub defaults: GtDefaults,
    pub sysfs_defaults: *mut Kobject,

    pub wedge: WorkStruct,

    pub perf: I915PerfGt,

    pub ggtt_link: ListHead,
}

#[repr(C)]
pub struct IntelGtDefinition {
    pub type_: IntelGtType,
    pub name: *mut c_char,
    pub mapping_base: u32,
    pub gsi_offset: u32,
    pub engine_mask: IntelEngineMask,
}

#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntelGtScratchFieldValue {
    Default      = 0,
    RenderFlush  = 128,
    CoherentL3Wa = 256,
}
pub type IntelGtScratchField = i32;
pub const INTEL_GT_SCRATCH_FIELD_DEFAULT: IntelGtScratchField =
    IntelGtScratchFieldValue::Default as i32;
pub const INTEL_GT_SCRATCH_FIELD_RENDER_FLUSH: IntelGtScratchField =
    IntelGtScratchFieldValue::RenderFlush as i32;
pub const INTEL_GT_SCRATCH_FIELD_COHERENTL3_WA: IntelGtScratchField =
    IntelGtScratchFieldValue::CoherentL3Wa as i32;

// Source layout checks for the Linux x86_64 wt-dev target configuration.
const _: [(); 8] = [(); core::mem::size_of::<IntelMmioRange>()];
const _: [(); 4] = [(); core::mem::size_of::<IntelSteeringTypeValue>()];
const _: [(); 4] = [(); core::mem::size_of::<IntelSubmissionMethodValue>()];
const _: [(); 4] = [(); core::mem::size_of::<IntelGtTypeValue>()];
const _: [(); 12] = [(); core::mem::size_of::<GtDefaults>()];
const _: [(); 32] = [(); core::mem::size_of::<IntelGtTlb>()];
const _: [(); 24] = [(); core::mem::size_of::<IntelGtTimelines>()];
const _: [(); 24] = [(); core::mem::size_of::<IntelGtStats>()];
const _: [(); 216] = [(); core::mem::size_of::<IntelGtInfo>()];
const _: [(); 32] = [(); core::mem::size_of::<IntelGtDefinition>()];
const _: [(); 5336] = [(); core::mem::size_of::<IntelGt>()];
const _: [(); 4024] = [(); core::mem::offset_of!(IntelGt, engine)];
const _: [(); 4936] = [(); core::mem::offset_of!(IntelGt, info)];
