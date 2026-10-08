// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
//
//! Source-order type transcription of Linux v7.2.3
//! `drivers/gpu/drm/i915/gt/intel_engine_types.h`.
//!
//! This is a separate header binding: it deliberately does not merge into or
//! replace the existing `intel_engine_cs_upstream` records. Pointer-only
//! dependencies are imported from their owning header bindings; root must
//! explicitly switch users of this header's records to this module.

use core::ffi::{c_char, c_ulong};

use crate::{
    i915_request_types_upstream::I915Request,
    i915_scheduler_types_upstream::I915SchedEngine,
    intel_breadcrumbs_types_upstream::IntelBreadcrumbs,
    intel_context_types_upstream::{File, IntelContext, IntelContextOps, IntelWakerefT},
    intel_context_upstream::{I915Vma, IntelRing, IntelTimeline},
    intel_engine_cs_upstream::{
        AtomicNotifierHead, AtomicT, DelayedWork, HlistHead, IntelWakeref, ListHead, LlistHead,
        LlistNode, RbNode, RbRootCached, Seqcount, TimerList, WorkStruct,
    },
    intel_gt_types_upstream::IntelGt,
    intel_sseu_types_upstream::IntelSseu,
    intel_uncore_types_upstream::IntelUncore,
    intel_workarounds_types_upstream::{
        I915McrRegT as I915McrReg, I915RegT as I915Reg, I915WaList,
    },
    linux::fields::KtimeT,
    linux_i915_private::DrmI915Private,
};

// HW engine class and instance constants.
pub const RENDER_CLASS: i32 = 0;
pub const VIDEO_DECODE_CLASS: i32 = 1;
pub const VIDEO_ENHANCEMENT_CLASS: i32 = 2;
pub const COPY_ENGINE_CLASS: i32 = 3;
pub const OTHER_CLASS: i32 = 4;
pub const COMPUTE_CLASS: i32 = 5;
pub const MAX_ENGINE_CLASS: i32 = 5;
pub const MAX_ENGINE_INSTANCE: i32 = 8;

pub const I915_MAX_SLICES: usize = 3;
pub const I915_MAX_SUBSLICES: usize = 8;
pub const I915_CMD_HASH_ORDER: u32 = 9;

// Pointer-only C forward declarations used by this header.
#[repr(C)]
pub struct DrmI915RegTable {
    _opaque: [u8; 0],
}

#[repr(C)]
pub struct I915PerfGroup {
    _opaque: [u8; 0],
}

/// C typedef `intel_engine_mask_t` is exactly `u32`.
pub type IntelEngineMaskT = u32;
/// Rust spelling used by the existing GT call sites.
pub type IntelEngineMask = IntelEngineMaskT;
pub const ALL_ENGINES: IntelEngineMask = u32::MAX;
pub const VIRTUAL_ENGINES: IntelEngineMask = 1 << (u32::BITS - 1);

#[repr(C)]
pub struct IntelHwStatusPage {
    pub timelines: ListHead,
    pub vma: *mut I915Vma,
    pub addr: *mut u32,
}

// The including intel_sseu.h sets GEN_MAX_GSLICES = 64 / 4.
pub const GEN_MAX_GSLICES: usize = 16;

#[repr(C)]
pub struct IntelInstdone {
    pub instdone: u32,
    pub slice_common: u32,
    pub slice_common_extra: [u32; 2],
    pub sampler: [[u32; I915_MAX_SUBSLICES]; GEN_MAX_GSLICES],
    pub row: [[u32; I915_MAX_SUBSLICES]; GEN_MAX_GSLICES],
    pub geom_svg: [[u32; I915_MAX_SUBSLICES]; GEN_MAX_GSLICES],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct I915WaCtxBb {
    pub offset: u32,
    pub size: u32,
}

#[repr(C)]
pub struct I915CtxWorkarounds {
    pub indirect_ctx: I915WaCtxBb,
    pub per_ctx: I915WaCtxBb,
    pub vma: *mut I915Vma,
}

pub const I915_MAX_VCS: usize = 8;
pub const I915_MAX_VECS: usize = 4;
pub const I915_MAX_SFC: usize = I915_MAX_VCS / 2;
pub const I915_MAX_CCS: usize = 4;
pub const I915_MAX_RCS: usize = 1;
pub const I915_MAX_BCS: usize = 9;
// From i915_pmu.h: I915_SAMPLE_SEMA is enum value 2, then + 1.
pub const I915_ENGINE_SAMPLE_COUNT: usize = 3;

// Dense engine IDs. The C enum has 32-bit int representation; the count
// enumerator has value 27. INVALID_ENGINE is a separate -1 cast macro.
#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntelEngineIdValue {
    Rcs0       = 0,
    Bcs0       = 1,
    Bcs1       = 2,
    Bcs2       = 3,
    Bcs3       = 4,
    Bcs4       = 5,
    Bcs5       = 6,
    Bcs6       = 7,
    Bcs7       = 8,
    Bcs8       = 9,
    Vcs0       = 10,
    Vcs1       = 11,
    Vcs2       = 12,
    Vcs3       = 13,
    Vcs4       = 14,
    Vcs5       = 15,
    Vcs6       = 16,
    Vcs7       = 17,
    Vecs0      = 18,
    Vecs1      = 19,
    Vecs2      = 20,
    Vecs3      = 21,
    Ccs0       = 22,
    Ccs1       = 23,
    Ccs2       = 24,
    Ccs3       = 25,
    Gsc0       = 26,
    NumEngines = 27,
}

pub type IntelEngineId = i32;
pub const RCS0: IntelEngineId = IntelEngineIdValue::Rcs0 as i32;
pub const BCS0: IntelEngineId = IntelEngineIdValue::Bcs0 as i32;
pub const BCS1: IntelEngineId = IntelEngineIdValue::Bcs1 as i32;
pub const BCS2: IntelEngineId = IntelEngineIdValue::Bcs2 as i32;
pub const BCS3: IntelEngineId = IntelEngineIdValue::Bcs3 as i32;
pub const BCS4: IntelEngineId = IntelEngineIdValue::Bcs4 as i32;
pub const BCS5: IntelEngineId = IntelEngineIdValue::Bcs5 as i32;
pub const BCS6: IntelEngineId = IntelEngineIdValue::Bcs6 as i32;
pub const BCS7: IntelEngineId = IntelEngineIdValue::Bcs7 as i32;
pub const BCS8: IntelEngineId = IntelEngineIdValue::Bcs8 as i32;
pub const VCS0: IntelEngineId = IntelEngineIdValue::Vcs0 as i32;
pub const VCS1: IntelEngineId = IntelEngineIdValue::Vcs1 as i32;
pub const VCS2: IntelEngineId = IntelEngineIdValue::Vcs2 as i32;
pub const VCS3: IntelEngineId = IntelEngineIdValue::Vcs3 as i32;
pub const VCS4: IntelEngineId = IntelEngineIdValue::Vcs4 as i32;
pub const VCS5: IntelEngineId = IntelEngineIdValue::Vcs5 as i32;
pub const VCS6: IntelEngineId = IntelEngineIdValue::Vcs6 as i32;
pub const VCS7: IntelEngineId = IntelEngineIdValue::Vcs7 as i32;
pub const VECS0: IntelEngineId = IntelEngineIdValue::Vecs0 as i32;
pub const VECS1: IntelEngineId = IntelEngineIdValue::Vecs1 as i32;
pub const VECS2: IntelEngineId = IntelEngineIdValue::Vecs2 as i32;
pub const VECS3: IntelEngineId = IntelEngineIdValue::Vecs3 as i32;
pub const CCS0: IntelEngineId = IntelEngineIdValue::Ccs0 as i32;
pub const CCS1: IntelEngineId = IntelEngineIdValue::Ccs1 as i32;
pub const CCS2: IntelEngineId = IntelEngineIdValue::Ccs2 as i32;
pub const CCS3: IntelEngineId = IntelEngineIdValue::Ccs3 as i32;
pub const GSC0: IntelEngineId = IntelEngineIdValue::Gsc0 as i32;
pub const I915_NUM_ENGINES: IntelEngineId = IntelEngineIdValue::NumEngines as i32;
pub const INVALID_ENGINE: i32 = -1;

pub const fn _BCS(instance: i32) -> i32 {
    BCS0 + instance
}
pub const fn _VCS(instance: i32) -> i32 {
    VCS0 + instance
}
pub const fn _VECS(instance: i32) -> i32 {
    VECS0 + instance
}
pub const fn _CCS(instance: i32) -> i32 {
    CCS0 + instance
}

// DECLARE_EWMA(_engine_latency, 6, 4) contributes this source-layout record.
#[repr(C)]
pub struct EwmaEngineLatency {
    pub internal: c_ulong,
}

// The source's Linux completion is done:u32 plus swait_queue_head. The target
// config has non-RT, non-lockdep raw_spinlock_t (one u32) and list_head.
#[repr(C)]
pub struct SwaitQueueHead {
    pub lock: crate::intel_engine_cs_upstream::Spinlock,
    pub task_list: ListHead,
}

#[repr(C)]
pub struct Completion {
    pub done: u32,
    pub wait: SwaitQueueHead,
}

#[repr(C)]
pub struct StPreemptHang {
    pub completion: Completion,
    pub count: u32,
}

pub const ERROR_CSB: u32 = 1 << 31;
pub const ERROR_PREEMPT: u32 = 1 << 30;
pub const EXECLIST_MAX_PORTS: usize = 2;

#[repr(C)]
pub struct IntelEngineExeclists {
    pub timer: TimerList,
    pub preempt: TimerList,
    pub preempt_target: *const I915Request,
    pub ccid: u32,
    pub yield_: u32,
    pub error_interrupt: u32,
    pub reset_ccid: u32,
    pub submit_reg: *mut u32,
    pub ctrl_reg: *mut u32,
    pub active: *const *mut I915Request,
    pub inflight: [*mut I915Request; EXECLIST_MAX_PORTS + 1],
    pub pending: [*mut I915Request; EXECLIST_MAX_PORTS + 1],
    pub port_mask: u32,
    pub virtual_: RbRootCached,
    pub csb_write: *mut u32,
    pub csb_status: *mut u64,
    pub csb_size: u8,
    pub csb_head: u8,
    #[cfg(CONFIG_DRM_I915_SELFTEST)]
    pub preempt_hang: StPreemptHang,
}

pub const INTEL_ENGINE_CS_MAX_NAME: usize = 8;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IntelEngineExeclistsStats {
    pub active: u32,
    pub lock: Seqcount,
    pub total: KtimeT,
    pub start: KtimeT,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IntelEngineGucStats {
    pub running: bool,
    pub prev_total: u32,
    pub total_gt_clks: u64,
    pub start_gt_clk: u64,
    pub total: u64,
}

#[repr(C)]
pub union IntelEngineStatsData {
    pub execlists: IntelEngineExeclistsStats,
    pub guc: IntelEngineGucStats,
}

#[repr(C)]
pub struct IntelEngineStats {
    pub data: IntelEngineStatsData,
    pub rps: KtimeT,
}

#[repr(C)]
pub union IntelEngineTlbInvReg {
    pub reg: I915Reg,
    pub mcr_reg: I915McrReg,
}

#[repr(C)]
pub struct IntelEngineTlbInv {
    pub mcr: bool,
    pub reg: IntelEngineTlbInvReg,
    pub request: u32,
    pub done: u32,
}

// `struct i915_pmu_sample` from i915_pmu.h, embedded by this header's engine
// PMU record.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct I915PmuSample {
    pub cur: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IntelEngineHeartbeat {
    pub work: DelayedWork,
    pub systole: *mut I915Request,
    pub blocked: c_ulong,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IntelEngineLegacy {
    pub ring: *mut IntelRing,
    pub timeline: *mut IntelTimeline,
}

#[repr(C)]
pub struct IntelEnginePmu {
    pub enable: u32,
    pub enable_count: [u32; I915_ENGINE_SAMPLE_COUNT],
    pub sample: [I915PmuSample; I915_ENGINE_SAMPLE_COUNT],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IntelEngineResetOps {
    pub prepare: Option<unsafe extern "C" fn(*mut IntelEngineCs)>,
    pub rewind: Option<unsafe extern "C" fn(*mut IntelEngineCs, bool)>,
    pub cancel: Option<unsafe extern "C" fn(*mut IntelEngineCs)>,
    pub finish: Option<unsafe extern "C" fn(*mut IntelEngineCs)>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union IntelEngineUabi {
    pub uabi_llist: LlistNode,
    pub uabi_list: ListHead,
    pub uabi_node: RbNode,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IntelEngineProps {
    pub heartbeat_interval_ms: c_ulong,
    pub max_busywait_duration_ns: c_ulong,
    pub preempt_timeout_ms: c_ulong,
    pub stop_timeout_ms: c_ulong,
    pub timeslice_duration_ms: c_ulong,
}

pub const I915_ENGINE_USING_CMD_PARSER: u32 = 1 << 0;
pub const I915_ENGINE_SUPPORTS_STATS: u32 = 1 << 1;
pub const I915_ENGINE_HAS_PREEMPTION: u32 = 1 << 2;
pub const I915_ENGINE_HAS_SEMAPHORES: u32 = 1 << 3;
pub const I915_ENGINE_HAS_TIMESLICES: u32 = 1 << 4;
pub const I915_ENGINE_IS_VIRTUAL: u32 = 1 << 5;
pub const I915_ENGINE_HAS_RELATIVE_MMIO: u32 = 1 << 6;
pub const I915_ENGINE_REQUIRES_CMD_PARSER: u32 = 1 << 7;
pub const I915_ENGINE_WANT_FORCED_PREEMPTION: u32 = 1 << 8;
pub const I915_ENGINE_HAS_RCS_REG_STATE: u32 = 1 << 9;
pub const I915_ENGINE_HAS_EU_PRIORITY: u32 = 1 << 10;
pub const I915_ENGINE_FIRST_RENDER_COMPUTE: u32 = 1 << 11;
pub const I915_ENGINE_USES_WA_HOLD_SWITCHOUT: u32 = 1 << 12;

pub const EMIT_INVALIDATE: u32 = 1 << 0;
pub const EMIT_FLUSH: u32 = 1 << 1;
pub const EMIT_BARRIER: u32 = EMIT_INVALIDATE | EMIT_FLUSH;
pub const I915_DISPATCH_SECURE: u32 = 1 << 0;
pub const I915_DISPATCH_PINNED: u32 = 1 << 1;

// C enum forcewake_domains is an int-sized bitmask defined by intel_uncore.h.
pub type ForcewakeDomains = i32;

#[repr(C)]
pub struct IntelEngineCs {
    pub i915: *mut DrmI915Private,
    pub gt: *mut IntelGt,
    pub uncore: *mut IntelUncore,
    pub name: [c_char; INTEL_ENGINE_CS_MAX_NAME],

    pub id: IntelEngineId,
    pub legacy_idx: IntelEngineId,

    pub guc_id: u32,

    pub mask: IntelEngineMask,
    pub reset_domain: u32,
    pub logical_mask: IntelEngineMask,

    pub class: u8,
    pub instance: u8,

    pub uabi_class: u16,
    pub uabi_instance: u16,

    pub uabi_capabilities: u32,
    pub context_size: u32,
    pub mmio_base: u32,

    pub tlb_inv: IntelEngineTlbInv,

    pub fw_domain: ForcewakeDomains,
    pub fw_active: u32,

    pub context_tag: c_ulong,

    pub uabi: IntelEngineUabi,

    pub sseu: IntelSseu,

    pub sched_engine: *mut I915SchedEngine,

    pub request_pool: *mut I915Request,

    pub hung_ce: *mut IntelContext,

    pub barrier_tasks: LlistHead,

    pub kernel_context: *mut IntelContext,
    pub bind_context: *mut IntelContext,
    pub bind_context_ready: bool,

    pub pinned_contexts_list: ListHead,

    pub saturated: IntelEngineMask,

    pub heartbeat: IntelEngineHeartbeat,

    pub serial: c_ulong,

    pub wakeref_serial: c_ulong,
    pub wakeref_track: IntelWakerefT,
    pub wakeref: IntelWakeref,

    pub default_state: *mut File,

    pub legacy: IntelEngineLegacy,

    pub latency: EwmaEngineLatency,

    pub breadcrumbs: *mut IntelBreadcrumbs,

    pub pmu: IntelEnginePmu,

    pub status_page: IntelHwStatusPage,
    pub wa_ctx: I915CtxWorkarounds,
    pub ctx_wa_list: I915WaList,
    pub wa_list: I915WaList,
    pub whitelist: I915WaList,

    pub irq_keep_mask: u32,
    pub irq_enable_mask: u32,
    pub irq_enable: Option<unsafe extern "C" fn(*mut IntelEngineCs)>,
    pub irq_disable: Option<unsafe extern "C" fn(*mut IntelEngineCs)>,
    pub irq_handler: Option<unsafe extern "C" fn(*mut IntelEngineCs, u16)>,

    pub sanitize: Option<unsafe extern "C" fn(*mut IntelEngineCs)>,
    pub resume: Option<unsafe extern "C" fn(*mut IntelEngineCs) -> i32>,

    pub reset: IntelEngineResetOps,

    pub park: Option<unsafe extern "C" fn(*mut IntelEngineCs)>,
    pub unpark: Option<unsafe extern "C" fn(*mut IntelEngineCs)>,

    pub bump_serial: Option<unsafe extern "C" fn(*mut IntelEngineCs)>,

    pub set_default_submission: Option<unsafe extern "C" fn(*mut IntelEngineCs)>,

    pub cops: *const IntelContextOps,

    pub request_alloc: Option<unsafe extern "C" fn(*mut I915Request) -> i32>,

    pub emit_flush: Option<unsafe extern "C" fn(*mut I915Request, u32) -> i32>,
    pub emit_bb_start: Option<unsafe extern "C" fn(*mut I915Request, u64, u32, u32) -> i32>,
    pub emit_init_breadcrumb: Option<unsafe extern "C" fn(*mut I915Request) -> i32>,
    pub emit_fini_breadcrumb: Option<unsafe extern "C" fn(*mut I915Request, *mut u32) -> *mut u32>,
    pub emit_fini_breadcrumb_dw: u32,

    pub submit_request: Option<unsafe extern "C" fn(*mut I915Request)>,

    pub release: Option<unsafe extern "C" fn(*mut IntelEngineCs)>,

    pub add_active_request: Option<unsafe extern "C" fn(*mut I915Request)>,
    pub remove_active_request: Option<unsafe extern "C" fn(*mut I915Request)>,

    pub busyness: Option<unsafe extern "C" fn(*mut IntelEngineCs, *mut KtimeT) -> KtimeT>,

    pub execlists: IntelEngineExeclists,

    pub retire: *mut IntelTimeline,
    pub retire_work: WorkStruct,

    pub context_status_notifier: AtomicNotifierHead,

    pub flags: u32,

    pub cmd_hash: [HlistHead; 1 << I915_CMD_HASH_ORDER],

    pub reg_tables: *const DrmI915RegTable,
    pub reg_table_count: i32,

    pub get_cmd_length_mask: Option<unsafe extern "C" fn(u32) -> u32>,

    pub stats: IntelEngineStats,

    pub props: IntelEngineProps,
    pub defaults: IntelEngineProps,

    #[cfg(CONFIG_DRM_I915_SELFTEST)]
    pub reset_timeout: FaultAttr,

    pub oa_group: *mut I915PerfGroup,
}

// Source inline helpers in intel_engine_types.h. Flags are a C bitmask; each
// boolean helper tests the corresponding bit without changing the record.
#[inline]
pub unsafe fn intel_engine_using_cmd_parser(engine: *const IntelEngineCs) -> bool {
    (*engine).flags & I915_ENGINE_USING_CMD_PARSER != 0
}

#[inline]
pub unsafe fn intel_engine_requires_cmd_parser(engine: *const IntelEngineCs) -> bool {
    (*engine).flags & I915_ENGINE_REQUIRES_CMD_PARSER != 0
}

#[inline]
pub unsafe fn intel_engine_supports_stats(engine: *const IntelEngineCs) -> bool {
    (*engine).flags & I915_ENGINE_SUPPORTS_STATS != 0
}

#[inline]
pub unsafe fn intel_engine_has_preemption(engine: *const IntelEngineCs) -> bool {
    (*engine).flags & I915_ENGINE_HAS_PREEMPTION != 0
}

#[inline]
pub unsafe fn intel_engine_has_semaphores(engine: *const IntelEngineCs) -> bool {
    (*engine).flags & I915_ENGINE_HAS_SEMAPHORES != 0
}

#[inline]
pub unsafe fn intel_engine_has_timeslices(engine: *const IntelEngineCs) -> bool {
    if crate::linux_config::CONFIG_DRM_I915_TIMESLICE_DURATION == 0 {
        return false;
    }

    (*engine).flags & I915_ENGINE_HAS_TIMESLICES != 0
}

#[inline]
pub unsafe fn intel_engine_is_virtual(engine: *const IntelEngineCs) -> bool {
    (*engine).flags & I915_ENGINE_IS_VIRTUAL != 0
}

#[inline]
pub unsafe fn intel_engine_has_relative_mmio(engine: *const IntelEngineCs) -> bool {
    (*engine).flags & I915_ENGINE_HAS_RELATIVE_MMIO != 0
}

#[inline]
pub unsafe fn intel_engine_uses_wa_hold_switchout(engine: *mut IntelEngineCs) -> bool {
    (*engine).flags & I915_ENGINE_USES_WA_HOLD_SWITCHOUT != 0
}

// Source-derived fixed-layout checks for members whose field layouts are
// independent of the external IntelContext/IntelWakeref configuration.
const _: [(); 4] = [(); core::mem::size_of::<IntelEngineIdValue>()];
const _: [(); 32] = [(); core::mem::size_of::<IntelHwStatusPage>()];
const _: [(); 1552] = [(); core::mem::size_of::<IntelInstdone>()];
const _: [(); 8] = [(); core::mem::size_of::<I915WaCtxBb>()];
const _: [(); 24] = [(); core::mem::size_of::<I915CtxWorkarounds>()];
const _: [(); 8] = [(); core::mem::size_of::<EwmaEngineLatency>()];
const _: [(); 32] = [(); core::mem::size_of::<Completion>()];
const _: [(); 40] = [(); core::mem::size_of::<StPreemptHang>()];
const _: [(); 24] = [(); core::mem::size_of::<IntelEngineExeclistsStats>()];
const _: [(); 32] = [(); core::mem::size_of::<IntelEngineGucStats>()];
const _: [(); 32] = [(); core::mem::size_of::<IntelEngineStatsData>()];
const _: [(); 40] = [(); core::mem::size_of::<IntelEngineStats>()];
const _: [(); 8] = [(); core::mem::size_of::<I915PmuSample>()];
const _: [(); 40] = [(); core::mem::size_of::<IntelEnginePmu>()];
const _: [(); 24] = [(); core::mem::size_of::<IntelEngineUabi>()];
const _: [(); 4] = [(); core::mem::size_of::<IntelEngineTlbInvReg>()];
const _: [(); 16] = [(); core::mem::size_of::<IntelEngineTlbInv>()];
// x86_64 wt-dev layout assertions already established by the source-derived
// engine ABI binding; they require the same non-RT, non-lockdep, no-selftest
// config and external header records noted below.
#[cfg(not(CONFIG_DRM_I915_SELFTEST))]
const _: [(); 224] = [(); core::mem::size_of::<IntelEngineExeclists>()];
#[cfg(not(CONFIG_DRM_I915_SELFTEST))]
const _: [(); 5496] = [(); core::mem::size_of::<IntelEngineCs>()];
#[cfg(not(CONFIG_DRM_I915_SELFTEST))]
const _: [(); 960] = [(); core::mem::offset_of!(IntelEngineCs, execlists)];
#[cfg(not(CONFIG_DRM_I915_SELFTEST))]
const _: [(); 576] = [(); core::mem::offset_of!(IntelEngineCs, status_page)];
#[cfg(not(CONFIG_DRM_I915_SELFTEST))]
const _: [(); 5408] = [(); core::mem::offset_of!(IntelEngineCs, props)];

// intel_engine_cs offsets through the stable source header prefix.
const _: [(); 24] = [(); core::mem::offset_of!(IntelEngineCs, name)];
const _: [(); 32] = [(); core::mem::offset_of!(IntelEngineCs, id)];
const _: [(); 40] = [(); core::mem::offset_of!(IntelEngineCs, guc_id)];
const _: [(); 76] = [(); core::mem::offset_of!(IntelEngineCs, tlb_inv)];
