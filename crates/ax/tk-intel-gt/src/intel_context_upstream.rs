// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
//
// Source-faithful Rust transcription of Linux 7.2.3
// drivers/gpu/drm/i915/gt/intel_context.c. The module is registered, and the
// IntelContext data layout is bound below. Remaining i915/GEM/RCU/locking
// operations are integration points, not substitute implementations. Keep
// source order and lifetime/locking edges intact when integrating them.

use core::{
    ffi::{c_ulong, c_void},
    mem::ManuallyDrop,
};

use crate::{
    intel_engine_cs_upstream::{
        AtomicT, DelayedWork, IntelEngineCs, IntelGt, IntelSseu, ListHead, LlistHead, LlistNode,
        Mutex, RbNode, RbRoot, RbRootCached, Spinlock, WorkStruct,
    },
    linux_config::*,
    linux_heap::{
        KmCache, SLAB_HWCACHE_ALIGN, kmem_cache_destroy, kmem_cache_free, kmem_cache_zalloc,
    },
    linux_list::*,
};

// The following layout-only types match the wt-dev Linux 7.2.3 x86_64/SMP
// configuration: PREEMPT_RT, LOCKDEP, DEBUG_LOCK_ALLOC, DEBUG_MUTEXES and
// DEBUG_SPINLOCK are unset; MUTEX_SPIN_ON_OWNER is enabled.  They describe
// embedded kernel-owned storage only; operations remain bound to the real
// kernel implementations. CONFIG_DRM_I915_SELFTEST and
// CONFIG_DRM_I915_SW_FENCE_CHECK_DAG are unset, so those conditional fields
// are absent here.

#[repr(C)]
pub struct RefcountT {
    pub refs: AtomicT,
}

#[repr(C)]
pub struct Kref {
    pub refcount: RefcountT,
}

#[repr(C, align(8))]
pub struct RcuHead {
    pub next: *mut RcuHead,
    pub func: Option<unsafe extern "C" fn(*mut RcuHead)>,
}

#[repr(C, align(8))]
pub struct DmaFence {
    _opaque: [u8; 64],
}

#[repr(C)]
pub struct DmaFenceCb {
    pub node: ListHead,
    pub func: Option<unsafe extern "C" fn(*mut DmaFence, *mut DmaFenceCb)>,
}

#[repr(C)]
pub struct ActiveNode {
    _opaque: [u8; 0],
}

#[repr(C)]
pub struct WaitQueueHead {
    pub lock: Spinlock,
    pub head: ListHead,
}

#[repr(C)]
pub struct I915ActiveFence {
    pub fence: *mut DmaFence,
    pub cb: DmaFenceCb,
}

#[repr(C)]
pub struct I915Active {
    pub count: AtomicT,
    pub mutex: Mutex,
    pub tree_lock: Spinlock,
    pub cache: *mut ActiveNode,
    pub tree: RbRoot,
    pub excl: I915ActiveFence,
    pub flags: c_ulong,
    pub active: Option<unsafe fn(*mut I915Active) -> i32>,
    pub retire: Option<unsafe fn(*mut I915Active)>,
    pub work: WorkStruct,
    pub preallocated_barriers: LlistHead,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub enum I915SwFenceNotify {
    FenceComplete = 0,
    FenceFree     = 1,
}

#[repr(C)]
pub struct I915SwFence {
    pub wait: WaitQueueHead,
    pub fn_: Option<unsafe fn(*mut I915SwFence, I915SwFenceNotify) -> i32>,
    pub pending: AtomicT,
    pub error: i32,
}

#[repr(C)]
pub struct EwmaRuntime {
    pub internal: c_ulong,
}

#[repr(C)]
pub struct IntelContextRuntimeStats {
    pub avg: EwmaRuntime,
    pub total: u64,
    pub last: u32,
}

#[repr(C)]
pub struct IntelContextStats {
    pub active: u64,
    pub runtime: IntelContextRuntimeStats,
}

#[repr(C)]
pub struct I915AddressSpace {
    _opaque: [u8; 0],
}

#[repr(C)]
pub struct I915GemContext {
    _opaque: [u8; 0],
}

#[repr(C)]
pub struct I915GemWwCtx {
    _opaque: [u8; 0],
}

// Linux v7.2.3 framework records embedded by value in i915_vma and
// i915_request. Their storage is kernel-owned; only their ABI size/alignment
// is needed by this source file.
#[repr(C, align(8))]
pub struct DrmMmNode {
    _opaque: [u8; 168],
}

#[repr(C, align(8))]
pub struct I915GttView {
    _opaque: [u8; 56],
}

#[repr(C)]
pub struct I915PageSizes {
    pub phys: u32,
    pub sg: u32,
}

#[repr(C, align(8))]
pub union I915RequestSubmitUnion {
    pub _opaque: [u64; 5],
}

#[repr(C, align(8))]
pub struct IrqWork {
    _opaque: [u8; 32],
}

#[repr(C, align(8))]
pub struct I915SchedNode {
    _opaque: [u8; 64],
}

#[repr(C, align(8))]
pub struct I915Dependency {
    _opaque: [u8; 72],
}

#[repr(C, align(8))]
pub struct Hrtimer {
    _opaque: [u8; 80],
}

#[repr(C, align(8))]
pub struct WaitQueueEntry {
    _opaque: [u8; 40],
}

#[repr(C)]
pub struct I915RequestWatchdog {
    pub link: LlistNode,
    pub timer: Hrtimer,
}

#[repr(C)]
pub struct I915Priolist {
    pub requests: ListHead,
    pub node: RbNode,
    pub priority: i32,
}

#[repr(C)]
pub struct I915SchedAttr {
    pub priority: i32,
}

#[repr(C)]
pub struct TaskletStruct {
    pub next: *mut TaskletStruct,
    pub state: c_ulong,
    pub count: AtomicT,
    pub use_callback: bool,
    // Same 8-byte anonymous-union slot as tasklet_struct.func; registered
    // i915 submission paths select and access the callback arm.
    pub callback: Option<unsafe fn(*mut TaskletStruct)>,
    pub data: c_ulong,
}

#[repr(C)]
pub struct I915SchedEngine {
    pub ref_: Kref,
    pub lock: Spinlock,
    pub requests: ListHead,
    pub hold: ListHead,
    pub tasklet: TaskletStruct,
    pub default_priolist: I915Priolist,
    pub queue_priority_hint: i32,
    pub queue: RbRootCached,
    pub no_priolist: bool,
    pub private_data: *mut c_void,
    pub destroy: Option<unsafe fn(*mut Kref)>,
    pub disabled: Option<unsafe fn(*mut I915SchedEngine) -> bool>,
    pub kick_backend: Option<unsafe fn(*const I915Request, i32)>,
    pub bump_inflight_request_prio: Option<unsafe fn(*mut I915Request, i32)>,
    pub retire_inflight_request_prio: Option<unsafe fn(*mut I915Request)>,
    pub schedule: Option<unsafe fn(*mut I915Request, *const I915SchedAttr)>,
}

#[repr(C)]
pub struct PinCookie;

// Pointer-only Linux types. These are intentionally incomplete just like C
// forward declarations; fields embedded in the records below have explicit
// source-derived layouts.
#[repr(C)]
pub struct I915VmaOps {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct SgTable {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct I915FenceReg {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct I915MmapOffset {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct I915VmaResource {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct I915CaptureList {
    _opaque: [u8; 0],
}

// CONFIG_DRM_I915_CAPTURE_ERROR is enabled in the source kernel build used
// for these layouts, so capture_list is present in I915Request.
#[repr(C)]
pub struct I915Request {
    pub fence: DmaFence,
    pub lock: Spinlock,
    pub i915: *mut c_void,
    pub engine: *mut IntelEngineCs,
    pub context: *mut IntelContext,
    pub ring: *mut IntelRing,
    pub timeline: *mut IntelTimeline,
    pub signal_link: ListHead,
    pub signal_node: LlistNode,
    pub rcustate: c_ulong,
    pub cookie: PinCookie,
    pub submit: I915SwFence,
    pub submit_union: I915RequestSubmitUnion,
    pub execute_cb: LlistHead,
    pub semaphore: I915SwFence,
    pub submit_work: IrqWork,
    pub sched: I915SchedNode,
    pub dep: I915Dependency,
    pub execution_mask: u32,
    pub hwsp_seqno: *const u32,
    pub head: u32,
    pub infix: u32,
    pub postfix: u32,
    pub tail: u32,
    pub wa_tail: u32,
    pub reserved_space: u32,
    pub batch_res: *mut I915VmaResource,
    pub capture_list: *mut I915CaptureList,
    pub emitted_jiffies: c_ulong,
    pub link: ListHead,
    pub watchdog: I915RequestWatchdog,
    pub guc_fence_link: ListHead,
    pub guc_prio: u8,
    pub hucq: WaitQueueEntry,
}

#[repr(C)]
pub struct I915Vma {
    pub node: DrmMmNode,
    pub vm: *mut I915AddressSpace,
    pub ops: *const I915VmaOps,
    pub obj: *mut DrmI915GemObject,
    pub pages: *mut SgTable,
    pub iomap: *mut c_void,
    pub private: *mut c_void,
    pub fence: *mut I915FenceReg,
    pub size: u64,
    pub page_sizes: I915PageSizes,
    pub mmo: *mut I915MmapOffset,
    pub guard: u32,
    pub fence_size: u32,
    pub fence_alignment: u32,
    pub display_alignment: u32,
    pub open_count: AtomicT,
    pub flags: AtomicT,
    pub active: I915Active,
    pub pages_count: AtomicT,
    pub vm_ddestroy: bool,
    pub gtt_view: I915GttView,
    pub vm_link: ListHead,
    pub obj_link: ListHead,
    pub obj_node: RbNode,
    pub evict_link: ListHead,
    pub closed_link: ListHead,
    pub resource: *mut I915VmaResource,
}

#[repr(C)]
pub struct IntelRing {
    pub ref_: Kref,
    pub vma: *mut I915Vma,
    pub vaddr: *mut c_void,
    pub pin_count: AtomicT,
    pub head: u32,
    pub tail: u32,
    pub emit: u32,
    pub space: u32,
    pub size: u32,
    pub wrap: u32,
    pub effective_size: u32,
}

// drm_i915_gem_object.mm layout from gem/i915_gem_object_types.h. The object
// prefix/suffix remain opaque; mm and its dirty bitfield word are represented
// so the one source access has the real C byte offset and bit semantics.
#[repr(C, align(8))]
pub struct I915GemObjectPageIter {
    _opaque: [u8; 56],
}

#[repr(C)]
pub struct I915GemObjectMm {
    pub pages_pin_count: AtomicT,
    pub shrink_pin: AtomicT,
    pub ttm_shrinkable: bool,
    pub unknown_state: bool,
    pub _pad0: [u8; 6],
    pub placements: *mut *mut c_void,
    pub n_placements: i32,
    pub _pad1: [u8; 4],
    pub region: *mut c_void,
    pub res: *mut c_void,
    pub region_link: ListHead,
    pub rsgt: *mut c_void,
    pub pages: *mut SgTable,
    pub mapping: *mut c_void,
    pub page_sizes: I915PageSizes,
    pub get_page: I915GemObjectPageIter,
    pub get_dma_page: I915GemObjectPageIter,
    pub link: ListHead,
    /// C bitfield storage: madv occupies bits 0..1; dirty is bit 2.
    pub madv_dirty: u32,
    pub tlb: [u32; 2],
}

impl I915GemObjectMm {
    /// Set only upstream's `dirty:1` bit, retaining the `madv:2` bits.
    pub fn set_dirty(&mut self) {
        self.madv_dirty |= 1 << 2;
    }
}

#[repr(C, align(8))]
pub struct DrmI915GemObject {
    pub _prefix: [u8; 688],
    pub mm: I915GemObjectMm,
    pub _suffix: [u8; 216],
}

#[repr(C)]
pub struct I915Syncmap {
    _opaque: [u8; 0],
}

#[repr(C)]
pub struct IntelTimeline {
    pub fence_context: u64,
    pub seqno: u32,
    pub mutex: Mutex,
    pub pin_count: AtomicT,
    pub active_count: AtomicT,
    pub hwsp_map: *mut c_void,
    pub hwsp_seqno: *const u32,
    pub hwsp_ggtt: *mut I915Vma,
    pub hwsp_offset: u32,
    pub has_initial_breadcrumb: bool,
    pub requests: ListHead,
    pub last_request: I915ActiveFence,
    pub active: I915Active,
    pub retire: *mut IntelTimeline,
    pub sync: *mut I915Syncmap,
    pub link: ListHead,
    pub gt: *mut IntelGt,
    pub engine_link: ListHead,
    pub kref: Kref,
    pub rcu: RcuHead,
}

const _: [(); 64] = [(); core::mem::size_of::<DmaFence>()];
const _: [(); 168] = [(); core::mem::size_of::<DrmMmNode>()];
const _: [(); 8] = [(); core::mem::align_of::<DrmMmNode>()];
const _: [(); 56] = [(); core::mem::size_of::<I915GttView>()];
const _: [(); 8] = [(); core::mem::align_of::<I915GttView>()];
const _: [(); 8] = [(); core::mem::size_of::<I915PageSizes>()];
const _: [(); 4] = [(); core::mem::align_of::<I915PageSizes>()];
const _: [(); 584] = [(); core::mem::size_of::<I915Vma>()];
const _: [(); 8] = [(); core::mem::align_of::<I915Vma>()];
const _: [(); 168] = [(); core::mem::offset_of!(I915Vma, vm)];
const _: [(); 184] = [(); core::mem::offset_of!(I915Vma, obj)];
const _: [(); 272] = [(); core::mem::offset_of!(I915Vma, active)];
const _: [(); 432] = [(); core::mem::offset_of!(I915Vma, gtt_view)];
const _: [(); 488] = [(); core::mem::offset_of!(I915Vma, vm_link)];
const _: [(); 520] = [(); core::mem::offset_of!(I915Vma, obj_node)];
const _: [(); 576] = [(); core::mem::offset_of!(I915Vma, resource)];
const _: [(); 56] = [(); core::mem::size_of::<IntelRing>()];
const _: [(); 8] = [(); core::mem::align_of::<IntelRing>()];
const _: [(); 8] = [(); core::mem::offset_of!(IntelRing, vma)];
const _: [(); 24] = [(); core::mem::offset_of!(IntelRing, pin_count)];
const _: [(); 28] = [(); core::mem::offset_of!(IntelRing, head)];
const _: [(); 52] = [(); core::mem::offset_of!(IntelRing, effective_size)];
const _: [(); 40] = [(); core::mem::size_of::<I915RequestSubmitUnion>()];
const _: [(); 672] = [(); core::mem::size_of::<I915Request>()];
const _: [(); 8] = [(); core::mem::align_of::<I915Request>()];
const _: [(); 88] = [(); core::mem::offset_of!(I915Request, context)];
const _: [(); 144] = [(); core::mem::offset_of!(I915Request, submit)];
const _: [(); 184] = [(); core::mem::offset_of!(I915Request, submit_union)];
const _: [(); 304] = [(); core::mem::offset_of!(I915Request, sched)];
const _: [(); 368] = [(); core::mem::offset_of!(I915Request, dep)];
const _: [(); 440] = [(); core::mem::offset_of!(I915Request, execution_mask)];
const _: [(); 480] = [(); core::mem::offset_of!(I915Request, batch_res)];
const _: [(); 488] = [(); core::mem::offset_of!(I915Request, capture_list)];
const _: [(); 520] = [(); core::mem::offset_of!(I915Request, watchdog)];
const _: [(); 632] = [(); core::mem::offset_of!(I915Request, hucq)];
const _: [(); 240] = [(); core::mem::size_of::<I915GemObjectMm>()];
const _: [(); 8] = [(); core::mem::align_of::<I915GemObjectMm>()];
const _: [(); 224] = [(); core::mem::offset_of!(I915GemObjectMm, madv_dirty)];
const _: [(); 1144] = [(); core::mem::size_of::<DrmI915GemObject>()];
const _: [(); 688] = [(); core::mem::offset_of!(DrmI915GemObject, mm)];
const _: [(); 912] = [(); core::mem::offset_of!(DrmI915GemObject, mm)
    + core::mem::offset_of!(I915GemObjectMm, madv_dirty)];
const _: [(); 360] = [(); core::mem::size_of::<IntelTimeline>()];
const _: [(); 8] = [(); core::mem::align_of::<IntelTimeline>()];
const _: [(); 32] = [(); core::mem::size_of::<I915ActiveFence>()];
const _: [(); 0] = [(); core::mem::offset_of!(IntelTimeline, fence_context)];
const _: [(); 8] = [(); core::mem::offset_of!(IntelTimeline, seqno)];
const _: [(); 16] = [(); core::mem::offset_of!(IntelTimeline, mutex)];
const _: [(); 40] = [(); core::mem::offset_of!(IntelTimeline, pin_count)];
const _: [(); 44] = [(); core::mem::offset_of!(IntelTimeline, active_count)];
const _: [(); 48] = [(); core::mem::offset_of!(IntelTimeline, hwsp_map)];
const _: [(); 56] = [(); core::mem::offset_of!(IntelTimeline, hwsp_seqno)];
const _: [(); 64] = [(); core::mem::offset_of!(IntelTimeline, hwsp_ggtt)];
const _: [(); 72] = [(); core::mem::offset_of!(IntelTimeline, hwsp_offset)];
const _: [(); 76] = [(); core::mem::offset_of!(IntelTimeline, has_initial_breadcrumb)];
const _: [(); 80] = [(); core::mem::offset_of!(IntelTimeline, requests)];
const _: [(); 96] = [(); core::mem::offset_of!(IntelTimeline, last_request)];
const _: [(); 128] = [(); core::mem::offset_of!(IntelTimeline, active)];
const _: [(); 280] = [(); core::mem::offset_of!(IntelTimeline, retire)];
const _: [(); 288] = [(); core::mem::offset_of!(IntelTimeline, sync)];
const _: [(); 296] = [(); core::mem::offset_of!(IntelTimeline, link)];
const _: [(); 312] = [(); core::mem::offset_of!(IntelTimeline, gt)];
const _: [(); 320] = [(); core::mem::offset_of!(IntelTimeline, engine_link)];
const _: [(); 336] = [(); core::mem::offset_of!(IntelTimeline, kref)];
const _: [(); 344] = [(); core::mem::offset_of!(IntelTimeline, rcu)];
const _: [(); 48] = [(); core::mem::size_of::<I915Priolist>()];
const _: [(); 0] = [(); core::mem::offset_of!(I915Priolist, requests)];
const _: [(); 16] = [(); core::mem::offset_of!(I915Priolist, node)];
const _: [(); 40] = [(); core::mem::offset_of!(I915Priolist, priority)];
const _: [(); 40] = [(); core::mem::size_of::<TaskletStruct>()];
const _: [(); 8] = [(); core::mem::align_of::<TaskletStruct>()];
const _: [(); 16] = [(); core::mem::offset_of!(TaskletStruct, count)];
const _: [(); 20] = [(); core::mem::offset_of!(TaskletStruct, use_callback)];
const _: [(); 24] = [(); core::mem::offset_of!(TaskletStruct, callback)];
const _: [(); 32] = [(); core::mem::offset_of!(TaskletStruct, data)];
const _: [(); 216] = [(); core::mem::size_of::<I915SchedEngine>()];
const _: [(); 8] = [(); core::mem::align_of::<I915SchedEngine>()];
const _: [(); 4] = [(); core::mem::offset_of!(I915SchedEngine, lock)];
const _: [(); 8] = [(); core::mem::offset_of!(I915SchedEngine, requests)];
const _: [(); 24] = [(); core::mem::offset_of!(I915SchedEngine, hold)];
const _: [(); 40] = [(); core::mem::offset_of!(I915SchedEngine, tasklet)];
const _: [(); 80] = [(); core::mem::offset_of!(I915SchedEngine, default_priolist)];
const _: [(); 128] = [(); core::mem::offset_of!(I915SchedEngine, queue_priority_hint)];
const _: [(); 136] = [(); core::mem::offset_of!(I915SchedEngine, queue)];
const _: [(); 160] = [(); core::mem::offset_of!(I915SchedEngine, private_data)];
const _: [(); 168] = [(); core::mem::offset_of!(I915SchedEngine, destroy)];
const _: [(); 176] = [(); core::mem::offset_of!(I915SchedEngine, disabled)];
const _: [(); 184] = [(); core::mem::offset_of!(I915SchedEngine, kick_backend)];
const _: [(); 192] = [(); core::mem::offset_of!(I915SchedEngine, bump_inflight_request_prio)];
const _: [(); 200] = [(); core::mem::offset_of!(I915SchedEngine, retire_inflight_request_prio)];
const _: [(); 208] = [(); core::mem::offset_of!(I915SchedEngine, schedule)];

#[repr(C)]
pub struct RefTracker {
    _opaque: [u8; 0],
}

pub type IntelWakerefHandle = *mut RefTracker;

#[repr(C)]
pub struct IntelContextOps {
    pub flags: c_ulong,
    pub alloc: Option<unsafe fn(*mut IntelContext) -> i32>,
    pub revoke: Option<unsafe fn(*mut IntelContext, *mut I915Request, u32)>,
    pub close: Option<unsafe fn(*mut IntelContext)>,
    pub pre_pin: Option<unsafe fn(*mut IntelContext, *mut I915GemWwCtx, *mut *mut c_void) -> i32>,
    pub pin: Option<unsafe fn(*mut IntelContext, *mut c_void) -> i32>,
    pub unpin: Option<unsafe fn(*mut IntelContext)>,
    pub post_unpin: Option<unsafe fn(*mut IntelContext)>,
    pub cancel_request: Option<unsafe fn(*mut IntelContext, *mut I915Request)>,
    pub enter: Option<unsafe fn(*mut IntelContext)>,
    pub exit: Option<unsafe fn(*mut IntelContext)>,
    pub sched_disable: Option<unsafe fn(*mut IntelContext)>,
    pub update_stats: Option<unsafe fn(*mut IntelContext)>,
    pub reset: Option<unsafe fn(*mut IntelContext)>,
    pub destroy: Option<unsafe fn(*mut Kref)>,
    pub create_virtual:
        Option<unsafe fn(*mut *mut IntelEngineCs, u32, c_ulong) -> *mut IntelContext>,
    pub create_parallel: Option<unsafe fn(*mut *mut IntelEngineCs, u32, u32) -> *mut IntelContext>,
    pub get_sibling: Option<unsafe fn(*mut IntelEngineCs, u32) -> *mut IntelEngineCs>,
}

#[repr(C)]
pub union IntelContextRef {
    pub refcount: ManuallyDrop<Kref>,
    pub rcu: ManuallyDrop<RcuHead>,
}

#[repr(C)]
pub struct IntelContextWatchdog {
    pub timeout_us: u64,
}

#[repr(C)]
pub union IntelContextLrc {
    pub desc: u64,
    pub regs: IntelContextLrcRegs,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IntelContextLrcRegs {
    pub lrca: u32,
    pub ccid: u32,
}

#[repr(C)]
pub struct IntelContextGucState {
    pub lock: Spinlock,
    pub sched_state: u32,
    pub fences: ListHead,
    pub blocked: I915SwFence,
    pub requests: ListHead,
    pub prio: u8,
    pub prio_count: [u32; 4],
    pub sched_disable_delay_work: DelayedWork,
}

#[repr(C)]
pub struct IntelContextGucId {
    pub id: u16,
    pub ref_: AtomicT,
    pub link: ListHead,
}

#[repr(C)]
pub union IntelContextParallelChildLink {
    pub child_list: ManuallyDrop<ListHead>,
    pub child_link: ManuallyDrop<ListHead>,
}

#[repr(C)]
pub struct IntelContextParallelGuc {
    pub wqi_head: u16,
    pub wqi_tail: u16,
    pub wq_head: *mut u32,
    pub wq_tail: *mut u32,
    pub wq_status: *mut u32,
    pub parent_page: u8,
}

#[repr(C)]
pub struct IntelContextParallel {
    pub children: IntelContextParallelChildLink,
    pub parent: *mut IntelContext,
    pub last_rq: *mut I915Request,
    pub fence_context: u64,
    pub seqno: u32,
    pub number_children: u8,
    pub child_index: u8,
    pub guc: IntelContextParallelGuc,
}

#[repr(C)]
pub struct IntelContext {
    pub ref_: IntelContextRef,
    pub engine: *mut IntelEngineCs,
    pub inflight: *mut IntelEngineCs,
    pub vm: *mut I915AddressSpace,
    pub gem_context: *mut I915GemContext,
    pub default_state: *mut c_void,
    pub signal_link: ListHead,
    pub signals: ListHead,
    pub signal_lock: Spinlock,
    pub state: *mut I915Vma,
    pub ring_size: u32,
    pub ring: *mut IntelRing,
    pub timeline: *mut IntelTimeline,
    pub wakeref: IntelWakerefHandle,
    pub flags: c_ulong,
    pub watchdog: IntelContextWatchdog,
    pub lrc_reg_state: *mut u32,
    pub lrc: IntelContextLrc,
    pub tag: u32,
    pub stats: IntelContextStats,
    pub active_count: u32,
    pub pin_count: AtomicT,
    pub pin_mutex: Mutex,
    pub active: I915Active,
    pub ops: *const IntelContextOps,
    pub sseu: IntelSseu,
    pub pinned_contexts_link: ListHead,
    pub wa_bb_page: u8,
    pub guc_state: IntelContextGucState,
    pub guc_id: IntelContextGucId,
    pub destroyed_link: ListHead,
    pub parallel: IntelContextParallel,
}

// C layout checks for the configured Linux 7.2.3 x86_64 build. Key offsets
// include the anonymous unions and the nested GuC/parallel records that the
// source functions address directly.
const _: [(); 16] = [(); core::mem::size_of::<IntelContextRef>()];
const _: [(); 152] = [(); core::mem::size_of::<I915Active>()];
const _: [(); 40] = [(); core::mem::size_of::<I915SwFence>()];
const _: [(); 192] = [(); core::mem::size_of::<IntelContextGucState>()];
const _: [(); 88] = [(); core::mem::size_of::<IntelContextParallel>()];
const _: [(); 752] = [(); core::mem::size_of::<IntelContext>()];
const _: [(); 16] = [(); core::mem::offset_of!(IntelContext, engine)];
const _: [(); 72] = [(); core::mem::offset_of!(IntelContext, signals)];
const _: [(); 96] = [(); core::mem::offset_of!(IntelContext, state)];
const _: [(); 176] = [(); core::mem::offset_of!(IntelContext, stats)];
const _: [(); 216] = [(); core::mem::offset_of!(IntelContext, pin_mutex)];
const _: [(); 240] = [(); core::mem::offset_of!(IntelContext, active)];
const _: [(); 392] = [(); core::mem::offset_of!(IntelContext, ops)];
const _: [(); 432] = [(); core::mem::offset_of!(IntelContext, guc_state)];
const _: [(); 624] = [(); core::mem::offset_of!(IntelContext, guc_id)];
const _: [(); 648] = [(); core::mem::offset_of!(IntelContext, destroyed_link)];
const _: [(); 664] = [(); core::mem::offset_of!(IntelContext, parallel)];
const _: [(); 24] = [(); core::mem::offset_of!(IntelContextGucState, blocked)];
const _: [(); 104] = [(); core::mem::offset_of!(IntelContextGucState, sched_disable_delay_work)];
const _: [(); 16] = [(); core::mem::offset_of!(IntelContextParallel, parent)];
const _: [(); 48] = [(); core::mem::offset_of!(IntelContextParallel, guc)];

pub const COPS_HAS_INFLIGHT_BIT: usize = 0;
pub const COPS_HAS_INFLIGHT: c_ulong = 1 << COPS_HAS_INFLIGHT_BIT;
pub const COPS_RUNTIME_CYCLES_BIT: usize = 1;
pub const COPS_RUNTIME_CYCLES: c_ulong = 1 << COPS_RUNTIME_CYCLES_BIT;

pub const CONTEXT_BARRIER_BIT: usize = 0;
pub const CONTEXT_ALLOC_BIT: usize = 1;
pub const CONTEXT_INIT_BIT: usize = 2;
pub const CONTEXT_VALID_BIT: usize = 3;
pub const CONTEXT_CLOSED_BIT: usize = 4;
pub const CONTEXT_USE_SEMAPHORES: usize = 5;
pub const CONTEXT_BANNED: usize = 6;
pub const CONTEXT_FORCE_SINGLE_SUBMISSION: usize = 7;
pub const CONTEXT_NOPREEMPT: usize = 8;
pub const CONTEXT_LRCA_DIRTY: usize = 9;
pub const CONTEXT_GUC_INIT: usize = 10;
pub const CONTEXT_PERMA_PIN: usize = 11;
pub const CONTEXT_IS_PARKING: usize = 12;
pub const CONTEXT_EXITING: usize = 13;
pub const CONTEXT_LOW_LATENCY: usize = 14;
pub const CONTEXT_OWN_STATE: usize = 15;

pub const GUC_INVALID_CONTEXT_ID: u16 = u16::MAX;
pub const GUC_CLIENT_PRIORITY_NUM: usize = 4;

static mut SLAB_CE: *mut KmCache = core::ptr::null_mut();

// upstream: intel_context.c intel_context_alloc()
unsafe fn intel_context_alloc() -> *mut IntelContext {
    kmem_cache_zalloc(SLAB_CE, GFP_KERNEL)
}

// upstream: intel_context.c rcu_context_free()
unsafe extern "C" fn rcu_context_free(rcu: *mut RcuHead) {
    let ce = container_of!(rcu, IntelContext, ref_);

    trace_intel_context_free(ce);
    if intel_context_has_own_state(ce) {
        fput((*ce).default_state);
    }
    kmem_cache_free(SLAB_CE, ce.cast::<c_void>());
}

// upstream: intel_context.c intel_context_free()
pub unsafe fn intel_context_free(ce: *mut IntelContext) {
    call_rcu(
        (&mut (*ce).ref_.rcu as *mut ManuallyDrop<RcuHead>).cast::<RcuHead>(),
        rcu_context_free,
    );
}

// upstream: intel_context.c intel_context_create()
pub unsafe fn intel_context_create(engine: *mut IntelEngineCs) -> *mut IntelContext {
    let ce = intel_context_alloc();
    if ce.is_null() {
        return ERR_PTR(-ENOMEM);
    }

    intel_context_init(ce, engine);
    trace_intel_context_create(ce);
    ce
}

// upstream: intel_context.c intel_context_alloc_state()
pub unsafe fn intel_context_alloc_state(ce: *mut IntelContext) -> i32 {
    if mutex_lock_interruptible(&mut (*ce).pin_mutex) != 0 {
        return -EINTR;
    }

    let err = (|| {
        let mut ctx: *mut I915GemContext;

        if !test_bit(CONTEXT_ALLOC_BIT, &(*ce).flags) {
            if intel_context_is_banned(ce) {
                return -EIO;
            }

            let err = ((*(*ce).ops).alloc)(ce);
            if unlikely(err != 0) {
                return err;
            }

            set_bit(CONTEXT_ALLOC_BIT, &mut (*ce).flags);

            rcu_read_lock();
            ctx = rcu_dereference((*ce).gem_context);
            if !ctx.is_null() && !kref_get_unless_zero(&mut (*ctx).refcount) {
                ctx = core::ptr::null_mut();
            }
            rcu_read_unlock();
            if !ctx.is_null() {
                if !(*ctx).client.is_null() {
                    i915_drm_client_add_context_objects((*ctx).client, ce);
                }
                i915_gem_context_put(ctx);
            }
        }

        0
    })();
    mutex_unlock(&mut (*ce).pin_mutex);
    err
}

// upstream: intel_context.c intel_context_active_acquire()
unsafe fn intel_context_active_acquire(ce: *mut IntelContext) -> i32 {
    __i915_active_acquire(&mut (*ce).active);

    if intel_context_is_barrier(ce)
        || intel_engine_uses_guc((*ce).engine)
        || intel_context_is_parallel(ce)
    {
        return 0;
    }

    // Preallocate tracking nodes.
    let err = i915_active_acquire_preallocate_barrier(&mut (*ce).active, (*ce).engine);
    if err != 0 {
        i915_active_release(&mut (*ce).active);
    }
    err
}

// upstream: intel_context.c intel_context_active_release()
unsafe fn intel_context_active_release(ce: *mut IntelContext) {
    // Nodes preallocated in intel_context_active().
    i915_active_acquire_barrier(&mut (*ce).active);
    i915_active_release(&mut (*ce).active);
}

// upstream: intel_context.c __context_pin_state()
unsafe fn __context_pin_state(vma: *mut I915Vma, ww: *mut I915GemWwCtx) -> i32 {
    let bias = i915_ggtt_pin_bias(vma) | PIN_OFFSET_BIAS;
    let err = i915_ggtt_pin(vma, ww, 0, bias | PIN_HIGH);
    if err != 0 {
        return err;
    }

    let err = i915_active_acquire(&mut (*vma).active);
    if err != 0 {
        i915_vma_unpin(vma);
        return err;
    }

    // Mark it globally pinned so the shrinker cannot reclaim it before release.
    i915_vma_make_unshrinkable(vma);
    (*(*vma).obj).mm.set_dirty();
    0
}

// upstream: intel_context.c __context_unpin_state()
unsafe fn __context_unpin_state(vma: *mut I915Vma) {
    i915_vma_make_shrinkable(vma);
    i915_active_release(&mut (*vma).active);
    __i915_vma_unpin(vma);
}

// upstream: intel_context.c __ring_active()
unsafe fn __ring_active(ring: *mut IntelRing, ww: *mut I915GemWwCtx) -> i32 {
    let err = intel_ring_pin(ring, ww);
    if err != 0 {
        return err;
    }

    let err = i915_active_acquire(&mut (*(*ring).vma).active);
    if err != 0 {
        intel_ring_unpin(ring);
        return err;
    }

    0
}

// upstream: intel_context.c __ring_retire()
unsafe fn __ring_retire(ring: *mut IntelRing) {
    i915_active_release(&mut (*(*ring).vma).active);
    intel_ring_unpin(ring);
}

// upstream: intel_context.c intel_context_pre_pin()
unsafe fn intel_context_pre_pin(ce: *mut IntelContext, ww: *mut I915GemWwCtx) -> i32 {
    CE_TRACE!(ce, "active\n");

    let err = __ring_active((*ce).ring, ww);
    if err != 0 {
        return err;
    }

    let err = intel_timeline_pin((*ce).timeline, ww);
    if err != 0 {
        __ring_retire((*ce).ring);
        return err;
    }

    if (*ce).state.is_null() {
        return 0;
    }

    let err = __context_pin_state((*ce).state, ww);
    if err != 0 {
        intel_timeline_unpin((*ce).timeline);
        __ring_retire((*ce).ring);
        return err;
    }
    0
}

// upstream: intel_context.c intel_context_post_unpin()
unsafe fn intel_context_post_unpin(ce: *mut IntelContext) {
    if !(*ce).state.is_null() {
        __context_unpin_state((*ce).state);
    }

    intel_timeline_unpin((*ce).timeline);
    __ring_retire((*ce).ring);
}

// upstream: intel_context.c __intel_context_do_pin_ww()
pub unsafe fn __intel_context_do_pin_ww(ce: *mut IntelContext, ww: *mut I915GemWwCtx) -> i32 {
    let mut handoff = false;
    let mut vaddr: *mut c_void = core::ptr::null_mut();
    let mut err = 0;

    if unlikely(!test_bit(CONTEXT_ALLOC_BIT, &(*ce).flags)) {
        err = intel_context_alloc_state(ce);
        if err != 0 {
            return err;
        }
    }

    // Always pin context/ring/timeline here to hold a reference for
    // __intel_context_active(), avoiding pin_mutex versus dma_resv_lock inversion.
    err = i915_gem_object_lock((*(*(*ce).timeline).hwsp_ggtt).obj, ww);
    if err == 0 {
        err = i915_gem_object_lock((*(*(*ce).ring).vma).obj, ww);
    }
    if err == 0 && !(*ce).state.is_null() {
        err = i915_gem_object_lock((*(*ce).state).obj, ww);
    }
    if err == 0 {
        err = intel_context_pre_pin(ce, ww);
    }
    if err != 0 {
        return err;
    }

    err = ((*(*ce).ops).pre_pin)(ce, ww, &mut vaddr);
    if err != 0 {
        intel_context_post_unpin(ce);
        i915_gem_ww_unlock_single((*(*(*ce).timeline).hwsp_ggtt).obj);
        return err;
    }

    err = i915_active_acquire(&mut (*ce).active);
    if err != 0 {
        ((*(*ce).ops).post_unpin)(ce);
        intel_context_post_unpin(ce);
        i915_gem_ww_unlock_single((*(*(*ce).timeline).hwsp_ggtt).obj);
        return err;
    }

    err = mutex_lock_interruptible(&mut (*ce).pin_mutex);
    if err != 0 {
        i915_active_release(&mut (*ce).active);
        ((*(*ce).ops).post_unpin)(ce);
        intel_context_post_unpin(ce);
        i915_gem_ww_unlock_single((*(*(*ce).timeline).hwsp_ggtt).obj);
        return err;
    }

    intel_engine_pm_might_get((*ce).engine);

    if unlikely(intel_context_is_closed(ce)) {
        err = -ENOENT;
    } else if likely(!atomic_add_unless(&mut (*ce).pin_count, 1, 0)) {
        err = intel_context_active_acquire(ce);
        if err == 0 {
            err = ((*(*ce).ops).pin)(ce, vaddr);
            if err != 0 {
                intel_context_active_release(ce);
            } else {
                CE_TRACE!(
                    ce,
                    "pin ring:{start:%08x, head:%04x, tail:%04x}\n",
                    i915_ggtt_offset((*(*ce).ring).vma),
                    (*(*ce).ring).head,
                    (*(*ce).ring).tail,
                );

                handoff = true;
                smp_mb__before_atomic(); // Flush pin before it is visible.
                atomic_inc(&mut (*ce).pin_count);
            }
        }
    }

    if err == 0 {
        GEM_BUG_ON!(!intel_context_is_pinned(ce)); // No overflow.
        trace_intel_context_do_pin(ce);
    }

    mutex_unlock(&mut (*ce).pin_mutex);
    i915_active_release(&mut (*ce).active);
    if !handoff {
        ((*(*ce).ops).post_unpin)(ce);
    }
    intel_context_post_unpin(ce);

    // Unlock the shared hwsp_ggtt object. The other locked global state is
    // pinned and stays resident until explicitly unpinned.
    i915_gem_ww_unlock_single((*(*(*ce).timeline).hwsp_ggtt).obj);
    err
}

// upstream: intel_context.c __intel_context_do_pin()
pub unsafe fn __intel_context_do_pin(ce: *mut IntelContext) -> i32 {
    let mut ww = I915GemWwCtx::default();
    i915_gem_ww_ctx_init(&mut ww, true);
    let mut err;
    loop {
        err = __intel_context_do_pin_ww(ce, &mut ww);
        if err != -EDEADLK {
            break;
        }
        err = i915_gem_ww_ctx_backoff(&mut ww);
        if err != 0 {
            break;
        }
    }
    i915_gem_ww_ctx_fini(&mut ww);
    err
}

// upstream: intel_context.c __intel_context_do_unpin()
pub unsafe fn __intel_context_do_unpin(ce: *mut IntelContext, sub: i32) {
    if !atomic_sub_and_test(sub, &mut (*ce).pin_count) {
        return;
    }

    CE_TRACE!(ce, "unpin\n");
    ((*(*ce).ops).unpin)(ce);
    ((*(*ce).ops).post_unpin)(ce);

    // Keep an extra reference: active_release() may asynchronously drop the
    // only reference keeping this context alive.
    intel_context_get(ce);
    intel_context_active_release(ce);
    trace_intel_context_do_unpin(ce);
    intel_context_put(ce);
}

// upstream: intel_context.c __intel_context_retire()
unsafe fn __intel_context_retire(active: *mut I915Active) {
    let ce = container_of!(active, IntelContext, active);

    CE_TRACE!(
        ce,
        "retire runtime: {{ total:%lluns, avg:%lluns }}\n",
        intel_context_get_total_runtime_ns(ce),
        intel_context_get_avg_runtime_ns(ce),
    );

    set_bit(CONTEXT_VALID_BIT, &mut (*ce).flags);
    intel_context_post_unpin(ce);
    intel_context_put(ce);
}

// upstream: intel_context.c __intel_context_active()
unsafe fn __intel_context_active(active: *mut I915Active) -> i32 {
    let ce = container_of!(active, IntelContext, active);

    intel_context_get(ce);

    // Everything should already be activated by intel_context_pre_pin().
    GEM_WARN_ON!(!i915_active_acquire_if_busy(
        &mut (*(*(*ce).ring).vma).active,
    ));
    __intel_ring_pin((*ce).ring);

    __intel_timeline_pin((*ce).timeline);

    if !(*ce).state.is_null() {
        GEM_WARN_ON!(!i915_active_acquire_if_busy(&mut (*(*ce).state).active));
        __i915_vma_pin((*ce).state);
        i915_vma_make_unshrinkable((*ce).state);
    }

    0
}

// upstream: intel_context.c sw_fence_dummy_notify()
unsafe fn sw_fence_dummy_notify(_sf: *mut I915SwFence, _state: I915SwFenceNotify) -> i32 {
    NOTIFY_DONE
}

// upstream: intel_context.c intel_context_init()
pub unsafe fn intel_context_init(ce: *mut IntelContext, engine: *mut IntelEngineCs) {
    GEM_BUG_ON!((*engine).cops.is_null());
    GEM_BUG_ON!((*(*engine).gt).vm.is_null());

    kref_init((&mut (*ce).ref_.refcount as *mut ManuallyDrop<Kref>).cast::<Kref>());

    (*ce).engine = engine;
    (*ce).ops = (*engine).cops.cast::<IntelContextOps>();
    (*ce).sseu = (*engine).sseu;
    (*ce).ring = core::ptr::null_mut();
    (*ce).ring_size = SZ_4K;

    ewma_runtime_init(&mut (*ce).stats.runtime.avg);

    (*ce).vm = i915_vm_get((*(*engine).gt).vm.cast::<I915AddressSpace>());

    // signal_link/lock is used under RCU.
    spin_lock_init(&mut (*ce).signal_lock);
    INIT_LIST_HEAD(&mut (*ce).signals);

    mutex_init(&mut (*ce).pin_mutex);

    spin_lock_init(&mut (*ce).guc_state.lock);
    INIT_LIST_HEAD(&mut (*ce).guc_state.fences);
    INIT_LIST_HEAD(&mut (*ce).guc_state.requests);

    (*ce).guc_id.id = GUC_INVALID_CONTEXT_ID;
    INIT_LIST_HEAD(&mut (*ce).guc_id.link);

    INIT_LIST_HEAD(&mut (*ce).destroyed_link);

    INIT_LIST_HEAD(
        (&mut (*ce).parallel.children.child_list as *mut ManuallyDrop<ListHead>).cast::<ListHead>(),
    );

    // Initialize fence as complete unless schedule-disable is pending.
    i915_sw_fence_init(&mut (*ce).guc_state.blocked, sw_fence_dummy_notify);
    i915_sw_fence_commit(&mut (*ce).guc_state.blocked);

    i915_active_init(
        &mut (*ce).active,
        __intel_context_active,
        __intel_context_retire,
        0,
    );
}

// upstream: intel_context.c intel_context_fini()
pub unsafe fn intel_context_fini(ce: *mut IntelContext) {
    let mut child: *mut IntelContext;
    let mut next: *mut IntelContext;

    if !(*ce).timeline.is_null() {
        intel_timeline_put((*ce).timeline);
    }
    i915_vm_put((*ce).vm);

    // Drop the creation reference held for each child.
    if intel_context_is_parent(ce) {
        for_each_child_safe!(ce, child, next, {
            intel_context_put(child);
        });
    }

    mutex_destroy(&mut (*ce).pin_mutex);
    i915_active_fini(&mut (*ce).active);
    i915_sw_fence_fini(&mut (*ce).guc_state.blocked);
}

// upstream: intel_context.c i915_context_module_exit()
pub unsafe fn i915_context_module_exit() {
    kmem_cache_destroy(SLAB_CE);
}

// upstream: intel_context.c i915_context_module_init()
pub unsafe fn i915_context_module_init() -> i32 {
    SLAB_CE = KMEM_CACHE!(IntelContext, SLAB_HWCACHE_ALIGN);
    if SLAB_CE.is_null() {
        return -ENOMEM;
    }

    0
}

// upstream: intel_context.c intel_context_enter_engine()
pub unsafe fn intel_context_enter_engine(ce: *mut IntelContext) {
    intel_engine_pm_get((*ce).engine);
    intel_timeline_enter((*ce).timeline);
}

// upstream: intel_context.c intel_context_exit_engine()
pub unsafe fn intel_context_exit_engine(ce: *mut IntelContext) {
    intel_timeline_exit((*ce).timeline);
    intel_engine_pm_put((*ce).engine);
}

// upstream: intel_context.c intel_context_prepare_remote_request()
pub unsafe fn intel_context_prepare_remote_request(
    ce: *mut IntelContext,
    rq: *mut I915Request,
) -> i32 {
    let tl = (*ce).timeline;

    // This function is only suitable for remotely modifying this context.
    GEM_BUG_ON!((*rq).context == ce);

    if rcu_access_pointer((*rq).timeline) != tl {
        // Timeline sharing: queue this switch after current activity.
        let err = i915_active_fence_set(&mut (*tl).last_request, rq);
        if err != 0 {
            return err;
        }
    }

    // Keep context image and timeline pinned until modifying request retires;
    // transfer the already-pinned ce reference into the tracked active request.
    GEM_BUG_ON!(i915_active_is_idle(&mut (*ce).active));
    i915_active_add_request(&mut (*ce).active, rq)
}

// upstream: intel_context.c intel_context_create_request()
pub unsafe fn intel_context_create_request(ce: *mut IntelContext) -> *mut I915Request {
    let mut ww = I915GemWwCtx::default();
    let mut rq: *mut I915Request;
    let mut err;

    i915_gem_ww_ctx_init(&mut ww, true);
    loop {
        err = intel_context_pin_ww(ce, &mut ww);
        if err == 0 {
            rq = i915_request_create(ce);
            intel_context_unpin(ce);
            break;
        } else if err == -EDEADLK {
            err = i915_gem_ww_ctx_backoff(&mut ww);
            if err == 0 {
                continue;
            }
            rq = ERR_PTR(err);
            break;
        } else {
            rq = ERR_PTR(err);
            break;
        }
    }

    i915_gem_ww_ctx_fini(&mut ww);

    if IS_ERR(rq) {
        return rq;
    }

    // timeline->mutex is logically inner but used as outer; retain the
    // selftest lockdep workaround and its exact order.
    lockdep_unpin_lock(&mut (*(*ce).timeline).mutex, (*rq).cookie);
    mutex_release(&mut (*(*ce).timeline).mutex.dep_map, _RET_IP_);
    mutex_acquire(
        &mut (*(*ce).timeline).mutex.dep_map,
        SINGLE_DEPTH_NESTING,
        0,
        _RET_IP_,
    );
    (*rq).cookie = lockdep_pin_lock(&mut (*(*ce).timeline).mutex);

    rq
}

// upstream: intel_context.c intel_context_get_active_request()
pub unsafe fn intel_context_get_active_request(ce: *mut IntelContext) -> *mut I915Request {
    let parent = intel_context_to_parent(ce);
    let mut rq: *mut I915Request;
    let mut active: *mut I915Request = core::ptr::null_mut();
    let mut flags = 0;

    GEM_BUG_ON!(!intel_engine_uses_guc((*ce).engine));

    // The parent list includes all contexts in this relationship, so compare
    // each request's context while searching newest-to-oldest.
    spin_lock_irqsave(&mut (*parent).guc_state.lock, &mut flags);
    list_for_each_entry_reverse!(rq, &(*parent).guc_state.requests, sched.link, {
        if (*rq).context != ce {
            continue;
        }
        if i915_request_completed(rq) {
            break;
        }

        active = rq;
    });
    if !active.is_null() {
        active = i915_request_get_rcu(active);
    }
    spin_unlock_irqrestore(&mut (*parent).guc_state.lock, flags);

    active
}

// upstream: intel_context.c intel_context_bind_parent_child()
pub unsafe fn intel_context_bind_parent_child(parent: *mut IntelContext, child: *mut IntelContext) {
    // Caller validates usage; keep the upstream assertions as the contract.
    GEM_BUG_ON!(intel_context_is_pinned(parent));
    GEM_BUG_ON!(intel_context_is_child(parent));
    GEM_BUG_ON!(intel_context_is_pinned(child));
    GEM_BUG_ON!(intel_context_is_child(child));
    GEM_BUG_ON!(intel_context_is_parent(child));

    (*parent).parallel.child_index = (*parent).parallel.number_children;
    (*parent).parallel.number_children += 1;
    list_add_tail(
        (&mut (*child).parallel.children.child_link as *mut ManuallyDrop<ListHead>)
            .cast::<ListHead>(),
        (&mut (*parent).parallel.children.child_list as *mut ManuallyDrop<ListHead>)
            .cast::<ListHead>(),
    );
    (*child).parallel.parent = parent;
}

// upstream: intel_context.c intel_context_get_total_runtime_ns()
pub unsafe fn intel_context_get_total_runtime_ns(ce: *mut IntelContext) -> u64 {
    if let Some(update_stats) = (*(*ce).ops).update_stats {
        update_stats(ce);
    }

    let mut total = (*ce).stats.runtime.total;
    if (*(*ce).ops).flags & COPS_RUNTIME_CYCLES != 0 {
        total *= (*(*(*ce).engine).gt).clock_period_ns;
    }

    let mut active = READ_ONCE!((*ce).stats.active);
    if active != 0 {
        active = intel_context_clock() - active;
    }

    total + active
}

// upstream: intel_context.c intel_context_get_avg_runtime_ns()
pub unsafe fn intel_context_get_avg_runtime_ns(ce: *mut IntelContext) -> u64 {
    let mut avg = ewma_runtime_read(&(*ce).stats.runtime.avg);

    if (*(*ce).ops).flags & COPS_RUNTIME_CYCLES != 0 {
        avg *= (*(*(*ce).engine).gt).clock_period_ns;
    }

    avg
}

// upstream: intel_context.c intel_context_ban()
pub unsafe fn intel_context_ban(ce: *mut IntelContext, rq: *mut I915Request) -> bool {
    let ret = intel_context_set_banned(ce);

    trace_intel_context_ban(ce);

    if let Some(revoke) = (*(*ce).ops).revoke {
        revoke(ce, rq, INTEL_CONTEXT_BANNED_PREEMPT_TIMEOUT_MS);
    }

    ret
}

// upstream: intel_context.c intel_context_revoke()
pub unsafe fn intel_context_revoke(ce: *mut IntelContext) -> bool {
    let ret = intel_context_set_exiting(ce);

    if let Some(revoke) = (*(*ce).ops).revoke {
        revoke(
            ce,
            core::ptr::null_mut(),
            (*(*ce).engine).props.preempt_timeout_ms,
        );
    }

    ret
}

// Upstream's trailing CONFIG_DRM_I915_SELFTEST include is not an intel_context.c
// function; the test implementation remains in the separate source file.
