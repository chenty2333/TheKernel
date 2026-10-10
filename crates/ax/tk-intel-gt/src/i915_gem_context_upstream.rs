// SPDX-License-Identifier: MIT
// Copyright © 2011-2012 Intel Corporation.
// Source-order Rust translation of Linux v7.2.3
// drivers/gpu/drm/i915/gem/i915_gem_context.c.
//
// i915-owned context/proto-context and engines state comes from the canonical
// `i915_gem_context_types_upstream` owner. UAPI packet records are represented
// locally from include/uapi/drm/i915_drm.h. Kernel task/capability and some DRM
// framework accessors remain explicit boundaries below; no error paths are
// converted to success stubs.

#![allow(non_snake_case, non_camel_case_types, unsafe_op_in_unsafe_fn)]

use core::{
    ffi::{c_char, c_int, c_long, c_ulong, c_void},
    mem::{offset_of, size_of},
    ptr,
    sync::atomic::{AtomicPtr, AtomicU64, Ordering},
};

use crate::{
    i915_gem_object_api_upstream::i915_gem_object_put,
    i915_gem_context_types_upstream::{
        CONTEXT_CLOSED, CONTEXT_FAST_HANG_JIFFIES, CONTEXT_USER_ENGINES, DrmI915FilePrivate,
        I915DrmClient, I915GemContext, I915GemEngineType, I915GemEngines,
        I915GemEnginesIter, I915GemProtoContext, I915GemProtoEngine, UCONTEXT_BANNABLE,
        UCONTEXT_LOW_LATENCY, UCONTEXT_NO_ERROR_CAPTURE, UCONTEXT_PERSISTENCE,
        UCONTEXT_RECOVERABLE,
    },
    i915_gem_object_types_upstream::{DrmI915GemObject, I915LutHandle},
    i915_request_types_upstream::I915Request,
    i915_vma_types_upstream::I915Vma,
    intel_context_types_upstream::{
        CONTEXT_ALLOC_BIT, CONTEXT_BANNED, CONTEXT_LOW_LATENCY, CONTEXT_PERMA_PIN,
        CONTEXT_USE_SEMAPHORES, I915Active, I915SwFence, I915SwFenceNotify, IntelContext,
        IntelWakerefT,
    },
    intel_context_api_upstream::intel_context_get,
    intel_context_upstream::{
        DmaFence, DrmGemObjectBaseLayout, DrmMmNode, I915ActiveFence,
        I915AddressSpace as OldI915AddressSpace, Kref,
        RcuHead, WaitQueueEntry,
    },
    intel_engine_cs_upstream::{
        AtomicT, IntelEngineCs, ListHead, Mutex, RbRoot, Spinlock, WorkStruct,
    },
    intel_engine_types_upstream::{
        COMPUTE_CLASS, I915_NUM_ENGINES as ENGINE_COUNT, INVALID_ENGINE,
        IntelEngineMask, RENDER_CLASS,
    },
    intel_gt_types_upstream::{IntelGt, IntelGtType},
    intel_gtt_api_upstream::{I915AddressSpace, I915Ppgtt, i915_address_space},
    intel_guc_actions_abi_types_upstream::*,
    intel_sseu_types_upstream::{IntelSseu, SseuDevInfo},
    linux::{
        bits::*, fields::*, gem::DrmFile, gem_memory::*, irq::*, locks::*, memory::*, mutex::*,
        rcu::*, registers::*, requests::*, sw_fence::*, workqueue::*, xarray::*,
        i915::{to_gt},
    },
    linux_config::{
        EBUSY, EFAULT, EINVAL, EIO, ENODEV, ENOENT, ENOMEM, GFP_KERNEL, HZ, I915_CONTEXT_DEFAULT_PRIORITY,
        ERR_PTR,
    },
    linux_i915_private::DrmI915Private,
};

const EPERM: i32 = 1;
const EBADF: i32 = 9;
const EEXIST: i32 = 17;
const I915_EXEC_RING_MASK: u32 = 0x3f;

const I915_CLIENT_SCORE_BANNED: i32 = 9;
const CAP_SYS_NICE: u32 = 23;
const CAP_SYS_ADMIN: u32 = 21;
const I915_CONTEXT_CREATE_FLAGS_USE_EXTENSIONS: u32 = 1 << 0;
const I915_CONTEXT_CREATE_FLAGS_SINGLE_TIMELINE: u32 = 1 << 1;
const I915_CONTEXT_CREATE_FLAGS_UNKNOWN: u32 = !0x3;
const I915_CONTEXT_CREATE_EXT_SETPARAM: u32 = 0;
const I915_CONTEXT_CREATE_EXT_CLONE: u32 = 1;
const I915_CONTEXT_PARAM_BAN_PERIOD: u64 = 0x1;
const I915_CONTEXT_PARAM_NO_ZEROMAP: u64 = 0x2;
const I915_CONTEXT_PARAM_GTT_SIZE: u64 = 0x3;
const I915_CONTEXT_PARAM_NO_ERROR_CAPTURE: u64 = 0x4;
const I915_CONTEXT_PARAM_BANNABLE: u64 = 0x5;
const I915_CONTEXT_PARAM_PRIORITY: u64 = 0x6;
const I915_CONTEXT_PARAM_SSEU: u64 = 0x7;
const I915_CONTEXT_PARAM_RECOVERABLE: u64 = 0x8;
const I915_CONTEXT_PARAM_VM: u64 = 0x9;
const I915_CONTEXT_PARAM_ENGINES: u64 = 0xa;
const I915_CONTEXT_PARAM_PERSISTENCE: u64 = 0xb;
const I915_CONTEXT_PARAM_RINGSIZE: u64 = 0xc;
const I915_CONTEXT_PARAM_PROTECTED_CONTENT: u64 = 0xd;
const I915_CONTEXT_PARAM_LOW_LATENCY: u64 = 0xe;
const I915_CONTEXT_PARAM_CONTEXT_IMAGE: u64 = 0xf;
const I915_CONTEXT_ENGINES_EXT_LOAD_BALANCE: u32 = 0;
const I915_CONTEXT_ENGINES_EXT_BOND: u32 = 1;
const I915_CONTEXT_ENGINES_EXT_PARALLEL_SUBMIT: u32 = 2;
const I915_CONTEXT_SSEU_FLAG_ENGINE_INDEX: u32 = 1 << 0;
const I915_CONTEXT_IMAGE_FLAG_ENGINE_INDEX: u32 = 1 << 0;
const I915_ENGINE_CLASS_INVALID_NONE: u16 = u16::MAX;
const I915_PRIORITY_NORMAL: i32 = 0;
// Linux 7.2.3 target auto.conf sets this numeric request-timeout default.
const CONFIG_DRM_I915_REQUEST_TIMEOUT: u32 = 20_000;
const DRM_SYNCOBJ_CREATE_SIGNALED: u32 = 1;
const I915_ACTIVE_AWAIT_BARRIER: u32 = 1 << 2;
const I915_CONTEXT_IMAGE_SIZE_ALIGN: usize = 8;
const PIDTYPE_PID: i32 = 0;
const XA_FLAGS_ALLOC1: u32 = 1 << 1;
const XA_LIMIT_32B_MIN: u32 = 0;
const XA_LIMIT_32B_MAX: u32 = u32::MAX;
const NOTIFY_DONE: i32 = 0;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct I915EngineClassInstance {
    pub engine_class: u16,
    pub engine_instance: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct I915UserExtension {
    pub next_extension: u64,
    pub name: u32,
    pub flags: u32,
    pub rsvd: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DrmI915GemContextParam {
    pub ctx_id: u32,
    pub size: u32,
    pub param: u64,
    pub value: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DrmI915GemContextParamSseu {
    pub engine: I915EngineClassInstance,
    pub flags: u32,
    pub slice_mask: u64,
    pub subslice_mask: u64,
    pub min_eus_per_subslice: u16,
    pub max_eus_per_subslice: u16,
    pub rsvd: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DrmI915GemContextCreateExt {
    pub ctx_id: u32,
    pub flags: u32,
    pub extensions: u64,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct I915ContextParamEngines {
    pub extensions: u64,
    pub engines: [I915EngineClassInstance; 0],
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct I915ContextEnginesLoadBalance {
    pub base: I915UserExtension,
    pub engine_index: u16,
    pub num_siblings: u16,
    pub flags: u32,
    pub mbz64: u64,
    pub engines: [I915EngineClassInstance; 0],
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct I915ContextEnginesBond {
    pub base: I915UserExtension,
    pub master: I915EngineClassInstance,
    pub virtual_index: u16,
    pub num_bonds: u16,
    pub flags: u64,
    pub mbz64: [u64; 4],
    pub engines: [I915EngineClassInstance; 0],
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct I915ContextEnginesParallelSubmit {
    pub base: I915UserExtension,
    pub engine_index: u16,
    pub width: u16,
    pub num_siblings: u16,
    pub mbz16: u16,
    pub flags: u64,
    pub mbz64: [u64; 3],
    pub engines: [I915EngineClassInstance; 0],
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct I915GemContextParamContextImage {
    pub engine: I915EngineClassInstance,
    pub flags: u32,
    pub size: u32,
    pub mbz: u32,
    pub image: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DrmI915GemContextCreateExtSetparam {
    pub base: I915UserExtension,
    pub param: DrmI915GemContextParam,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DrmI915GemContextDestroy {
    pub ctx_id: u32,
    pub pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DrmI915GemVmControl {
    pub extensions: u64,
    pub flags: u32,
    pub vm_id: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DrmI915ResetStats {
    pub ctx_id: u32,
    pub flags: u32,
    pub reset_count: u32,
    pub batch_active: u32,
    pub batch_pending: u32,
    pub pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct I915GemContextCreateExtData {
    pub pc: *mut I915GemProtoContext,
    pub fpriv: *mut DrmI915FilePrivate,
}

#[repr(C)]
struct I915ClientContexts {
    lock: Spinlock,
    list: ListHead,
}

#[repr(C)]
struct I915FileOrRcu {
    _opaque: [u8; 16],
}

#[repr(C)]
struct I915FilePrivateView {
    i915: *mut DrmI915Private,
    file_or_rcu: I915FileOrRcu,
    proto_context_lock: Mutex,
    proto_context_xa: XArray,
    context_xa: XArray,
    vm_xa: XArray,
    bsd_engine: u32,
    ban_score: AtomicT,
    hang_timestamp: c_ulong,
    client: *mut I915DrmClient,
}

#[repr(C)]
struct I915ClientView {
    ref_: Kref,
    ctx_lock: Spinlock,
    ctx_list: ListHead,
}

/// Source-derived `i915_drm_client` prefix for transferring context runtime.
/// This sequence and offset are also asserted in `i915_drm_client_upstream`;
/// the opaque public context-type owner does not expose this member yet.
#[repr(C)]
struct I915DrmClientRuntimeView {
    kref: Kref,
    ctx_lock: Spinlock,
    ctx_list: ListHead,
    objects_lock: Spinlock,
    objects_list: ListHead,
    past_runtime: [AtomicU64; 5],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct XaLimit {
    min: u32,
    max: u32,
}

const _: [(); 4] = [(); size_of::<I915EngineClassInstance>()];
const _: [(); 32] = [(); size_of::<I915UserExtension>()];
const _: [(); 24] = [(); size_of::<DrmI915GemContextParam>()];
const _: [(); 32] = [(); size_of::<DrmI915GemContextParamSseu>()];
const _: [(); 16] = [(); size_of::<DrmI915GemContextCreateExt>()];
const _: [(); 24] = [(); size_of::<I915GemContextParamContextImage>()];
const _: [(); 48] = [(); offset_of!(I915DrmClientRuntimeView, past_runtime)];

// Linux/UAPI and i915 helpers not owned by this source file. Static-inline
// services are implemented locally below instead of being declared as symbols.
unsafe extern "C" {
    fn __copy_from_user(to: *mut c_void, from: *const c_void, size: usize) -> usize;
    fn __copy_to_user(to: *mut c_void, from: *const c_void, size: usize) -> usize;
    fn memdup_user(src: *const c_void, len: usize) -> *mut c_void;
    fn shmem_create_from_data(name: *const c_char, data: *mut c_void, len: usize) -> *mut c_void;
    pub fn fput(file: *mut c_void);
    fn get_task_pid(task: *mut c_void, pidtype: c_int) -> *mut c_void;
    fn put_pid(pid: *mut c_void);
    fn pid_nr(pid: *mut c_void) -> c_int;
    fn __i915_drm_client_free(kref: *mut Kref);
    fn intel_engine_lookup_user(
        i915: *mut DrmI915Private,
        class: u8,
        instance: u8,
    ) -> *mut IntelEngineCs;
    fn intel_has_reset_engine(gt: *const IntelGt) -> bool;
    fn intel_pxp_is_enabled(pxp: *const c_void) -> bool;
    fn intel_pxp_is_active(pxp: *const c_void) -> bool;
    fn intel_pxp_start(pxp: *mut c_void) -> c_int;
    fn intel_context_reconfigure_sseu(ce: *mut IntelContext, sseu: *const IntelSseu) -> c_int;
    fn intel_engine_pulse(engine: *mut IntelEngineCs) -> c_int;
    fn i915_request_get_rcu(rq: *mut I915Request) -> *mut I915Request;
    fn i915_request_put(rq: *mut I915Request);
    fn i915_request_active_engine(rq: *mut I915Request, engine: *mut *mut IntelEngineCs) -> bool;
    fn i915_gem_object_get(obj: *mut DrmI915GemObject) -> *mut DrmI915GemObject;
    fn i915_vma_close(vma: *mut I915Vma);
    fn drm_syncobj_create(syncobj: *mut *mut c_void, flags: u32, fence: *mut DmaFence) -> c_int;
    fn drm_syncobj_put(syncobj: *mut c_void);
    fn i915_drm_client_get(client: *mut I915DrmClient) -> *mut I915DrmClient;
    fn i915_drm_client_put(client: *mut I915DrmClient);
    fn i915_user_extensions(
        extensions: *mut I915UserExtension,
        funcs: *const Option<unsafe extern "C" fn(*mut I915UserExtension, *mut c_void) -> c_int>,
        count: u32,
        data: *mut c_void,
    ) -> c_int;
    fn intel_gt_terminally_wedged(gt: *mut IntelGt) -> c_int;
    fn i915_reset_count(error: *const crate::linux_i915_private::I915GpuError) -> u32;
    fn xa_store(xa: *mut XArray, index: c_ulong, entry: *mut c_void, gfp: u32) -> *mut c_void;
    fn xa_erase(xa: *mut XArray, index: c_ulong) -> *mut c_void;
    fn xa_find(xa: *mut XArray, index: *mut c_ulong, max: c_ulong, filter: u32) -> *mut c_void;
    fn xa_find_after(
        xa: *mut XArray,
        index: *mut c_ulong,
        max: c_ulong,
        filter: u32,
    ) -> *mut c_void;
    fn __xa_alloc(
        xa: *mut XArray,
        id: *mut u32,
        entry: *mut c_void,
        limit: XaLimit,
        gfp: u32,
    ) -> c_int;
    fn capable(cap: u32) -> bool;
    fn get_task_comm(buf: *mut c_char, task: *const c_void);
    fn task_pid_nr(task: *const c_void) -> c_int;
    fn kmem_cache_create(
        name: *const c_char,
        size: usize,
        align: usize,
        flags: u32,
        ctor: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> *mut crate::linux::heap::KmCache;
    fn kmem_cache_alloc(cache: *mut crate::linux::heap::KmCache, flags: u32) -> *mut c_void;
    fn kmem_cache_destroy(cache: *mut crate::linux::heap::KmCache);
    fn trace_i915_context_free(ctx: *mut I915GemContext);
    fn trace_i915_context_create(ctx: *mut I915GemContext);
}

static mut SLAB_LUTS: *mut crate::linux::heap::KmCache = ptr::null_mut();

unsafe fn file_private_view(file_priv: *mut DrmI915FilePrivate) -> *mut I915FilePrivateView {
    file_priv.cast()
}

pub(crate) unsafe fn i915_gem_file_bsd_engine(file_priv: *mut DrmI915FilePrivate) -> i32 {
    unsafe { (*file_private_view(file_priv)).bsd_engine as i32 }
}

pub(crate) unsafe fn i915_gem_file_set_bsd_engine(
    file_priv: *mut DrmI915FilePrivate,
    engine: u32,
) {
    unsafe { (*file_private_view(file_priv)).bsd_engine = engine };
}

unsafe fn client_view(client: *mut I915DrmClient) -> *mut I915ClientView {
    client.cast()
}

unsafe fn vm_pointer(ctx: *const I915GemContext) -> *mut I915AddressSpace {
    unsafe { (*ctx).vm.cast() }
}

unsafe fn context_registry(i915: *mut DrmI915Private) -> *mut I915ClientContexts {
    // In i915_drv.h, `gem.contexts` immediately follows gt[], sysfs_gt, media_gt.
    let base = i915.cast::<u8>();
    let offset = offset_of!(DrmI915Private, gt)
        + size_of::<[*mut IntelGt; crate::linux_i915_private::I915_MAX_GT]>()
        + 2 * size_of::<*mut c_void>();
    unsafe { base.add(offset).cast::<I915ClientContexts>() }
}

unsafe fn scheduler_caps(i915: *mut DrmI915Private) -> u32 {
    // i915_drv.h places intel_driver_caps immediately after runtime_info.
    let offset =
        offset_of!(DrmI915Private, runtime) + size_of::<crate::linux::i915::IntelRuntimeInfo>();
    unsafe { *i915.cast::<u8>().add(offset).cast::<u32>() }
}

pub(crate) unsafe fn set_has_logical_contexts(i915: *mut DrmI915Private) {
    let offset =
        offset_of!(DrmI915Private, runtime) + size_of::<crate::linux::i915::IntelRuntimeInfo>() + 4;
    unsafe { *i915.cast::<u8>().add(offset) = 1 };
}

unsafe fn has_logical_contexts(i915: *mut DrmI915Private) -> bool {
    let offset =
        offset_of!(DrmI915Private, runtime) + size_of::<crate::linux::i915::IntelRuntimeInfo>() + 4;
    unsafe { *i915.cast::<u8>().add(offset) != 0 }
}

unsafe fn i915_vm_get(vm: *mut I915AddressSpace) -> *mut I915AddressSpace {
    unsafe { crate::intel_gtt_api_upstream::i915_vm_get(vm) }
}
unsafe fn i915_vm_put(vm: *mut I915AddressSpace) {
    unsafe { crate::intel_gtt_api_upstream::i915_vm_put(vm) };
}

unsafe fn copy_from_user<T: Copy>(dst: *mut T, src: *const c_void, size: usize) -> usize {
    unsafe { __copy_from_user(dst.cast(), src, size) }
}
unsafe fn copy_to_user<T: Copy>(dst: *mut c_void, src: *const T, size: usize) -> usize {
    unsafe { __copy_to_user(dst, src.cast(), size) }
}
unsafe fn get_user<T: Copy + Default>(dst: *mut T, src: *const T) -> c_int {
    let mut value = T::default();
    if unsafe { copy_from_user(&mut value, src.cast(), size_of::<T>()) } != 0 {
        return -EFAULT;
    }
    unsafe { ptr::write(dst, value) };
    0
}
unsafe fn check_user_mbz<T: Copy + Default + PartialEq>(src: *const T) -> c_int {
    let mut value = T::default();
    if unsafe { copy_from_user(&mut value, src.cast(), size_of::<T>()) } != 0 {
        -EFAULT
    } else if value != T::default() {
        -EINVAL
    } else {
        0
    }
}

unsafe fn xa_alloc<T>(xa: *mut XArray, id: *mut u32, value: *mut T, limit: XaLimit) -> c_int {
    unsafe { __xa_alloc(xa, id, value.cast(), limit, GFP_KERNEL) }
}


unsafe fn current_task_ptr() -> *mut c_void {
    axhal::percpu::current_task_ptr::<()>().cast_mut().cast()
}

unsafe fn set_context_bit(ctx: *mut I915GemContext, bit: u32, value: bool) {
    if value {
        set_bit(bit, unsafe { &mut (*ctx).user_flags });
    } else {
        clear_bit(bit, unsafe { &mut (*ctx).user_flags });
    }
}

unsafe fn context_set_engine_bit(ctx: *mut I915GemContext, bit: u32, value: bool) {
    if value {
        set_bit(bit, unsafe { &mut (*ctx).flags });
    } else {
        clear_bit(bit, unsafe { &mut (*ctx).flags });
    }
}

unsafe fn context_engine(ctx: *const I915GemContext, index: u32) -> *mut IntelContext {
    let engines = unsafe { READ_ONCE!((*ctx).engines) };
    if engines.is_null() || index >= unsafe { (*engines).num_engines } {
        return ERR_PTR(-EINVAL);
    }
    let ce = unsafe { *(*engines).engines.as_ptr().add(index as usize) };
    if ce.is_null() {
        ERR_PTR(-EINVAL)
    } else {
        unsafe { crate::intel_context_api_upstream::intel_context_get(ce) }
    }
}

unsafe fn context_engines_lock(ctx: *mut I915GemContext) -> *mut I915GemEngines {
    unsafe { mutex_lock(&mut (*ctx).engines_mutex) };
    unsafe { READ_ONCE!((*ctx).engines) }
}
unsafe fn context_engines_unlock(ctx: *mut I915GemContext) {
    unsafe { mutex_unlock(&mut (*ctx).engines_mutex) };
}

pub(crate) unsafe fn i915_gem_context_get(ctx: *mut I915GemContext) -> *mut I915GemContext {
    unsafe { kref_get(ptr::addr_of_mut!((*ctx).r#ref)) };
    ctx
}
pub unsafe fn i915_gem_context_put(ctx: *mut I915GemContext) {
    unsafe { kref_put(ptr::addr_of_mut!((*ctx).r#ref), i915_gem_context_release) };
}

pub(crate) unsafe fn i915_gem_context_no_error_capture(ctx: *const I915GemContext) -> bool {
    test_bit(UCONTEXT_NO_ERROR_CAPTURE, unsafe { &(*ctx).user_flags })
}
unsafe fn i915_gem_context_is_bannable(ctx: *const I915GemContext) -> bool {
    test_bit(UCONTEXT_BANNABLE, unsafe { &(*ctx).user_flags })
}
pub(crate) unsafe fn i915_gem_context_is_recoverable(ctx: *const I915GemContext) -> bool {
    test_bit(UCONTEXT_RECOVERABLE, unsafe { &(*ctx).user_flags })
}
unsafe fn i915_gem_context_is_persistent(ctx: *const I915GemContext) -> bool {
    test_bit(UCONTEXT_PERSISTENCE, unsafe { &(*ctx).user_flags })
}
pub(crate) unsafe fn i915_gem_context_user_engines(ctx: *const I915GemContext) -> bool {
    test_bit(CONTEXT_USER_ENGINES, unsafe { &(*ctx).flags })
}
pub(crate) unsafe fn i915_gem_context_is_closed(ctx: *const I915GemContext) -> bool {
    test_bit(CONTEXT_CLOSED, unsafe { &(*ctx).flags })
}
pub(crate) unsafe fn i915_gem_context_uses_protected_content(ctx: *const I915GemContext) -> bool {
    unsafe { (*ctx).uses_protected_content }
}

unsafe fn i915_gem_context_get_eb_vm(ctx: *mut I915GemContext) -> *mut I915AddressSpace {
    let mut vm = unsafe { vm_pointer(ctx) };
    if vm.is_null() {
        let i915 = unsafe { (*ctx).i915 };
        vm = unsafe { (*to_gt(i915)).ggtt }.cast::<I915AddressSpace>();
    }
    unsafe { i915_vm_get(vm) }
}

pub(crate) unsafe fn i915_gem_context_has_full_ppgtt(ctx: *mut I915GemContext) -> bool {
    !unsafe { vm_pointer(ctx) }.is_null()
}

// `HAS_FULL_PPGTT(i915)` in i915_drv.h compares RUNTIME_INFO(i915)->ppgtt_type
// to INTEL_PPGTT_FULL, whose UAPI value is 2.
#[inline]
unsafe fn has_full_ppgtt(i915: *const DrmI915Private) -> bool {
    unsafe { (*i915).runtime.ppgtt_type >= 2 }
}

// Source `ALL_L3_SLICES` -> `NUM_L3_SLICES`: Haswell GT3 has two slices;
// otherwise the `has_l3_dpf` device flag is bit 17 in DEV_INFO_FOR_EACH_FLAG.
#[inline]
unsafe fn all_l3_slices(i915: *mut DrmI915Private) -> u8 {
    let info = unsafe { crate::linux::i915::INTEL_INFO(i915) };
    let count = if unsafe { crate::linux::i915::IS_HASWELL(i915) && (*info).gt == 3 } {
        2
    } else {
        ((unsafe { (*info).flags[2] } & (1 << 1)) != 0) as u8
    };
    ((1u8 << count) - 1) as u8
}

unsafe fn i915_gem_context_set_closed(ctx: *mut I915GemContext) {
    GEM_BUG_ON!(unsafe { i915_gem_context_is_closed(ctx) });
    set_bit(CONTEXT_CLOSED, unsafe { &mut (*ctx).flags });
}

pub(crate) unsafe fn i915_gem_context_get_engine(
    ctx: *mut I915GemContext,
    index: u32,
) -> *mut IntelContext {
    unsafe { context_engine(ctx, index) }
}

#[inline]
fn array_index_nospec(index: u32, size: u32) -> u32 {
    if index < size { index } else { 0 }
}

#[inline]
fn u64_to_user_ptr(value: u64) -> *mut c_void {
    value as usize as *mut c_void
}

#[inline]
fn i915_vma_to_object(vma: *mut I915Vma) -> *mut DrmI915GemObject {
    unsafe { (*vma).obj.cast() }
}

#[inline]
fn is_err_ptr<T>(ptr_: *const T) -> bool {
    let value = ptr_ as isize;
    value < 0 && value >= -4095
}

#[inline]
fn ptr_err<T>(ptr_: *const T) -> c_int {
    ptr_ as isize as c_int
}

// Linux 7.2.3 i915_gem_context.c function translations follow in C source
// order. Functions whose low-level owners are outside this file call the
// corresponding source API or retain an explicit unresolved owner binding.

// upstream: i915_gem_context.c i915_lut_handle_alloc()
pub unsafe fn i915_lut_handle_alloc() -> *mut I915LutHandle {
    unsafe { kmem_cache_alloc(SLAB_LUTS, GFP_KERNEL).cast() }
}

// upstream: i915_gem_context.c i915_lut_handle_free()
pub unsafe fn i915_lut_handle_free(lut: *mut I915LutHandle) {
    unsafe { crate::linux::heap::kmem_cache_free(SLAB_LUTS, lut.cast()) };
}

// The radix-tree slot iterator has the same layout as Linux's
// radix_tree_iter; the actual tree and VMA remain owned by their canonical
// owners, not by a guessed context/object overlay.
#[repr(C)]
#[derive(Clone, Copy)]
struct I915RadixTreeIter {
    index: c_ulong,
    next_index: c_ulong,
    tags: c_ulong,
    node: *mut c_void,
}

unsafe extern "C" {
    fn radix_tree_next_chunk(
        root: *const crate::intel_context_upstream::RadixTreeRoot,
        iter: *mut I915RadixTreeIter,
        flags: u32,
    ) -> *mut *mut c_void;
    fn radix_tree_next_slot(
        slot: *mut *mut c_void,
        iter: *mut I915RadixTreeIter,
        flags: u32,
    ) -> *mut *mut c_void;
    fn radix_tree_iter_delete(
        root: *mut crate::intel_context_upstream::RadixTreeRoot,
        iter: *mut I915RadixTreeIter,
        slot: *mut *mut c_void,
    );
}

// upstream: i915_gem_context.c lut_close()
unsafe fn lut_close(ctx: *mut I915GemContext) {
    let mut iter = I915RadixTreeIter {
        index: 0,
        next_index: 0,
        tags: 0,
        node: ptr::null_mut(),
    };
    let root = unsafe { ptr::addr_of_mut!((*ctx).handles_vma) };
    let mut slot: *mut *mut c_void = ptr::null_mut();

    unsafe { mutex_lock(ptr::addr_of_mut!((*ctx).lut_mutex)) };
    unsafe { rcu_read_lock() };
    loop {
        if slot.is_null() {
            slot = unsafe { radix_tree_next_chunk(root, &mut iter, 0) };
            if slot.is_null() {
                break;
            }
        }

        let vma = unsafe { ptr::read_volatile(slot).cast::<I915Vma>() };
        if !vma.is_null() {
            let obj = unsafe { i915_vma_to_object(vma) };
            // The DRM GEM owner is the first member of the i915 object's base
            // union; cast the union storage instead of dereferencing its
            // ManuallyDrop arm.
            let gem_obj = obj.cast::<DrmGemObjectBaseLayout>();
            let refcount = unsafe { &mut (*gem_obj).refcount };
            if crate::linux::memory::kref_get_unless_zero(refcount) {
                let mut lut = ptr::null_mut::<I915LutHandle>();
                unsafe { spin_lock(&mut (*obj).lut_lock) };
                let head = ptr::addr_of_mut!((*obj).lut_list);
                let mut link = unsafe { (*head).next };
                while link != head {
                    let candidate = unsafe {
                        link.cast::<u8>()
                            .sub(offset_of!(I915LutHandle, obj_link))
                            .cast::<I915LutHandle>()
                    };
                    if unsafe { (*candidate).ctx == ctx && (*candidate).handle == iter.index as u32 }
                    {
                        lut = candidate;
                        unsafe { crate::linux_list::list_del(ptr::addr_of_mut!((*candidate).obj_link)) };
                        break;
                    }
                    link = unsafe { (*link).next };
                }
                unsafe { spin_unlock(&mut (*obj).lut_lock) };

                if !lut.is_null() {
                    unsafe { i915_lut_handle_free(lut) };
                    unsafe { radix_tree_iter_delete(root, &mut iter, slot) };
                    unsafe { i915_vma_close(vma) };
                    unsafe { i915_gem_object_put(obj) };
                }
                unsafe { i915_gem_object_put(obj) };
            }
        }
        slot = unsafe { radix_tree_next_slot(slot, &mut iter, 0) };
    }
    unsafe { rcu_read_unlock() };
    unsafe { mutex_unlock(ptr::addr_of_mut!((*ctx).lut_mutex)) };
}

const LOOKUP_USER_INDEX: u32 = 1 << 0;

// upstream: i915_gem_context.c lookup_user_engine()
unsafe fn lookup_user_engine(
    ctx: *mut I915GemContext,
    flags: u32,
    ci: *const I915EngineClassInstance,
) -> *mut IntelContext {
    let user_engines = unsafe { i915_gem_context_user_engines(ctx) };
    if (flags & LOOKUP_USER_INDEX != 0) != user_engines {
        return ERR_PTR(-EINVAL);
    }
    let idx = if !user_engines {
        let engine = unsafe {
            intel_engine_lookup_user((*ctx).i915, (*ci).engine_class as u8, (*ci).engine_instance as u8)
        };
        if engine.is_null() {
            return ERR_PTR(-EINVAL);
        }
        unsafe { (*engine).legacy_idx as u32 }
    } else {
        unsafe { (*ci).engine_instance as u32 }
    };
    unsafe { i915_gem_context_get_engine(ctx, idx) }
}

const I915_CONTEXT_MAX_USER_PRIORITY: i64 = 1023;
const I915_CONTEXT_MIN_USER_PRIORITY: i64 = -1023;
const I915_SCHEDULER_CAP_PRIORITY: u32 = 1 << 1;
const I915_SCHEDULER_CAP_PREEMPTION: u32 = 1 << 2;

// upstream: i915_gem_context.c validate_priority()
unsafe fn validate_priority(
    i915: *mut DrmI915Private,
    args: *const DrmI915GemContextParam,
) -> c_int {
    let priority = unsafe { (*args).value as i64 };
    if unsafe { (*args).size } != 0 {
        return -EINVAL;
    }
    if unsafe { scheduler_caps(i915) & I915_SCHEDULER_CAP_PRIORITY == 0 } {
        return -ENODEV;
    }
    if !(I915_CONTEXT_MIN_USER_PRIORITY..=I915_CONTEXT_MAX_USER_PRIORITY).contains(&priority) {
        return -EINVAL;
    }
    if priority > I915_CONTEXT_DEFAULT_PRIORITY as i64 && !unsafe { capable(CAP_SYS_NICE) } {
        return -EPERM;
    }
    0
}

// upstream: i915_gem_context.c proto_context_close()
unsafe fn proto_context_close(i915: *mut DrmI915Private, pc: *mut I915GemProtoContext) {
    if pc.is_null() {
        return;
    }
    if !unsafe { (*pc).pxp_wakeref }.is_null() {
        unsafe {
            crate::intel_runtime_pm_upstream::intel_runtime_pm_put_raw(
                ptr::addr_of_mut!((*i915).runtime_pm).cast(),
                (*pc).pxp_wakeref,
            )
        };
    }
    if !unsafe { (*pc).vm }.is_null() {
        unsafe { i915_vm_put((*pc).vm) };
    }
    if !unsafe { (*pc).user_engines }.is_null() {
        for i in 0..unsafe { (*pc).num_user_engines.max(0) as usize } {
            unsafe { kfree(*(*pc).user_engines.add(i).as_ref().unwrap().siblings) };
        }
        unsafe { kfree((*pc).user_engines) };
    }
    unsafe { kfree(pc) };
}

// upstream: i915_gem_context.c proto_context_set_persistence()
unsafe fn proto_context_set_persistence(
    i915: *mut DrmI915Private,
    pc: *mut I915GemProtoContext,
    persist: bool,
) -> c_int {
    if persist {
        if !unsafe { (*i915).params.enable_hangcheck } {
            return -EINVAL;
        }
        unsafe { (*pc).user_flags |= 1 << UCONTEXT_PERSISTENCE };
    } else {
        if unsafe { scheduler_caps(i915) & I915_SCHEDULER_CAP_PREEMPTION == 0 } {
            return -ENODEV;
        }
        if !unsafe { intel_has_reset_engine(to_gt(i915)) } {
            return -ENODEV;
        }
        unsafe { (*pc).user_flags &= !(1 << UCONTEXT_PERSISTENCE) };
    }
    0
}

// upstream: i915_gem_context.c proto_context_set_protected()
unsafe fn proto_context_set_protected(
    i915: *mut DrmI915Private,
    pc: *mut I915GemProtoContext,
    protected: bool,
) -> c_int {
    let mut ret = 0;
    // TODO(owner): the canonical DrmI915Private owner does not expose `pxp`;
    // this named source member intentionally remains an unresolved owner
    // dependency rather than an inferred opaque-tail offset.
    let pxp = unsafe { (*i915).pxp };
    if !protected {
        unsafe { (*pc).uses_protected_content = false };
    } else if !unsafe { intel_pxp_is_enabled(pxp) } {
        ret = -ENODEV;
    } else if unsafe { (*pc).user_flags & (1 << UCONTEXT_RECOVERABLE) != 0 }
        || unsafe { (*pc).user_flags & (1 << UCONTEXT_BANNABLE) == 0 }
    {
        ret = -EPERM;
    } else {
        unsafe { (*pc).uses_protected_content = true };
        unsafe {
            (*pc).pxp_wakeref = crate::intel_runtime_pm_upstream::intel_runtime_pm_get(
                ptr::addr_of_mut!((*i915).runtime_pm).cast(),
            )
        };
        if !unsafe { intel_pxp_is_active(pxp) } {
            ret = unsafe { intel_pxp_start(pxp) };
        }
    }
    ret
}

// upstream: i915_gem_context.c proto_context_create()
unsafe fn proto_context_create(
    fpriv: *mut DrmI915FilePrivate,
    i915: *mut DrmI915Private,
    flags: u32,
) -> *mut I915GemProtoContext {
    let pc = unsafe { kzalloc_obj::<I915GemProtoContext>() };
    if pc.is_null() {
        return ERR_PTR(-ENOMEM);
    }
    unsafe {
        (*pc).fpriv = fpriv;
        (*pc).num_user_engines = -1;
        (*pc).user_engines = ptr::null_mut();
        (*pc).user_flags = (1 << UCONTEXT_BANNABLE) | (1 << UCONTEXT_RECOVERABLE);
        if (*i915).params.enable_hangcheck {
            (*pc).user_flags |= 1 << UCONTEXT_PERSISTENCE;
        }
        (*pc).sched.priority = I915_PRIORITY_NORMAL;
    }
    if flags & I915_CONTEXT_CREATE_FLAGS_SINGLE_TIMELINE != 0 {
        if !unsafe { crate::linux::i915::HAS_EXECLISTS(i915) } {
            unsafe { proto_context_close(i915, pc) };
            return ERR_PTR(-EINVAL);
        }
        unsafe { (*pc).single_timeline = true };
    }
    pc
}

// upstream: i915_gem_context.c proto_context_register_locked()
unsafe fn proto_context_register_locked(
    fpriv: *mut DrmI915FilePrivate,
    pc: *mut I915GemProtoContext,
    id: *mut u32,
) -> c_int {
    let view = unsafe { file_private_view(fpriv) };
    let mut ret = unsafe {
        xa_alloc(
            ptr::addr_of_mut!((*view).context_xa),
            id,
            ptr::null_mut::<c_void>(),
            XaLimit { min: XA_LIMIT_32B_MIN, max: XA_LIMIT_32B_MAX },
        )
    };
    if ret != 0 {
        return ret;
    }
    let old = unsafe {
        xa_store(
            ptr::addr_of_mut!((*view).proto_context_xa),
            *id as c_ulong,
            pc.cast(),
            GFP_KERNEL,
        )
    };
    if is_err_ptr(old) {
        unsafe { xa_erase(ptr::addr_of_mut!((*view).context_xa), *id as c_ulong) };
        ret = ptr_err(old);
        return ret;
    }
    debug_assert!(old.is_null());
    0
}

// upstream: i915_gem_context.c proto_context_register()
unsafe fn proto_context_register(
    fpriv: *mut DrmI915FilePrivate,
    pc: *mut I915GemProtoContext,
    id: *mut u32,
) -> c_int {
    let view = unsafe { file_private_view(fpriv) };
    unsafe { mutex_lock(ptr::addr_of_mut!((*view).proto_context_lock)) };
    let ret = unsafe { proto_context_register_locked(fpriv, pc, id) };
    unsafe { mutex_unlock(ptr::addr_of_mut!((*view).proto_context_lock)) };
    ret
}

// upstream: i915_gem_context.c i915_gem_vm_lookup()
unsafe fn i915_gem_vm_lookup(file_priv: *mut DrmI915FilePrivate, id: u32) -> *mut I915AddressSpace {
    let view = unsafe { file_private_view(file_priv) };
    unsafe { xa_lock(&mut (*view).vm_xa) };
    let vm = unsafe { xa_load::<_, I915AddressSpace>(&mut (*view).vm_xa, id) };
    if !vm.is_null() {
        unsafe { kref_get(ptr::addr_of_mut!((*vm).r#ref)) };
    }
    unsafe { xa_unlock(&mut (*view).vm_xa) };
    vm
}

// upstream: i915_gem_context.c set_proto_ctx_vm()
unsafe fn set_proto_ctx_vm(
    fpriv: *mut DrmI915FilePrivate,
    pc: *mut I915GemProtoContext,
    args: *const DrmI915GemContextParam,
) -> c_int {
    let i915 = unsafe { (*file_private_view(fpriv)).i915 };
    let value = unsafe { (*args).value };
    if unsafe { (*args).size } != 0 {
        return -EINVAL;
    }
    if !unsafe { has_full_ppgtt(i915) } {
        return -ENODEV;
    }
    if value >> 32 != 0 {
        return -ENOENT;
    }
    let vm = unsafe { i915_gem_vm_lookup(fpriv, value as u32) };
    if vm.is_null() {
        return -ENOENT;
    }
    let old = unsafe { (*pc).vm };
    if !old.is_null() {
        unsafe { i915_vm_put(old) };
    }
    unsafe { (*pc).vm = vm };
    0
}

#[repr(C)]
struct SetProtoCtxEngines {
    i915: *mut DrmI915Private,
    num_engines: u32,
    engines: *mut I915GemProtoEngine,
}

// ABI of i915_user_extension_fn from i915_user_extensions.h.
type I915UserExtensionFn =
    Option<unsafe extern "C" fn(*mut I915UserExtension, *mut c_void) -> c_int>;

unsafe fn extension_payload<T>(base: *mut I915UserExtension) -> *mut T {
    unsafe {
        // Every i915_user_extension-derived UAPI record starts with `base`
        // at offset zero, as defined by the corresponding C declarations.
        base.cast::<T>()
    }
}

// upstream: i915_gem_context.c set_proto_ctx_engines_balance()
unsafe extern "C" fn set_proto_ctx_engines_balance(
    base: *mut I915UserExtension,
    data: *mut c_void,
) -> c_int {
    let ext = unsafe { extension_payload::<I915ContextEnginesLoadBalance>(base) };
    let set = unsafe { &mut *data.cast::<SetProtoCtxEngines>() };
    let i915 = set.i915;
    if !unsafe { crate::linux::i915::HAS_EXECLISTS(i915) } {
        return -ENODEV;
    }
    let mut idx = 0u16;
    if unsafe { get_user(&mut idx, ptr::addr_of!((*ext).engine_index)) } != 0 {
        return -EFAULT;
    }
    if idx as u32 >= set.num_engines {
        return -EINVAL;
    }
    idx = array_index_nospec(idx as u32, set.num_engines) as u16;
    let pe = unsafe { &mut *set.engines.add(idx as usize) };
    if pe.r#type != I915GemEngineType::Invalid {
        return -EEXIST;
    }
    let mut num_siblings = 0u16;
    if unsafe { get_user(&mut num_siblings, ptr::addr_of!((*ext).num_siblings)) } != 0 {
        return -EFAULT;
    }
    let mut err = unsafe { check_user_mbz(ptr::addr_of!((*ext).flags)) };
    if err != 0 {
        return err;
    }
    err = unsafe { check_user_mbz(ptr::addr_of!((*ext).mbz64)) };
    if err != 0 {
        return err;
    }
    if num_siblings == 0 {
        return 0;
    }
    let siblings = unsafe { kmalloc_objs::<*mut IntelEngineCs, _>(num_siblings as usize) };
    if siblings.is_null() {
        return -ENOMEM;
    }
    for n in 0..num_siblings as usize {
        let mut ci = I915EngineClassInstance { engine_class: 0, engine_instance: 0 };
        let ci_ptr = unsafe { ptr::addr_of!((*ext).engines).cast::<I915EngineClassInstance>().add(n) };
        if unsafe { copy_from_user(&mut ci, ci_ptr.cast(), size_of::<I915EngineClassInstance>()) } != 0 {
            unsafe { kfree(siblings) };
            return -EFAULT;
        }
        let engine = unsafe { intel_engine_lookup_user(i915, ci.engine_class as u8, ci.engine_instance as u8) };
        if engine.is_null() {
            unsafe { kfree(siblings) };
            return -EINVAL;
        }
        unsafe { *siblings.add(n) = engine };
    }
    if num_siblings == 1 {
        pe.r#type = I915GemEngineType::Physical;
        pe.engine = unsafe { *siblings };
        unsafe { kfree(siblings) };
    } else {
        pe.r#type = I915GemEngineType::Balanced;
        pe.num_siblings = num_siblings as u32;
        pe.siblings = siblings;
    }
    0
}

// upstream: i915_gem_context.c set_proto_ctx_engines_bond()
unsafe extern "C" fn set_proto_ctx_engines_bond(
    base: *mut I915UserExtension,
    data: *mut c_void,
) -> c_int {
    let ext = unsafe { extension_payload::<I915ContextEnginesBond>(base) };
    let set = unsafe { &mut *data.cast::<SetProtoCtxEngines>() };
    let i915 = set.i915;
    if unsafe { crate::linux::i915::GRAPHICS_VER(i915) >= 12
        && !crate::linux::i915::IS_TIGERLAKE(i915)
        && !crate::linux::i915::IS_ROCKETLAKE(i915)
        && !crate::linux::i915::IS_ALDERLAKE_S(i915) }
    {
        return -ENODEV;
    }
    let mut idx = 0u16;
    if unsafe { get_user(&mut idx, ptr::addr_of!((*ext).virtual_index)) } != 0 {
        return -EFAULT;
    }
    if idx as u32 >= set.num_engines {
        return -EINVAL;
    }
    idx = array_index_nospec(idx as u32, set.num_engines) as u16;
    let pe = unsafe { &*set.engines.add(idx as usize) };
    if pe.r#type == I915GemEngineType::Invalid {
        return -EINVAL;
    }
    if pe.r#type != I915GemEngineType::Physical {
        return -EINVAL;
    }
    let mut err = unsafe { check_user_mbz(ptr::addr_of!((*ext).flags)) };
    if err != 0 {
        return err;
    }
    for n in 0..4 {
        err = unsafe { check_user_mbz(ptr::addr_of!((*ext).mbz64).cast::<u64>().add(n)) };
        if err != 0 {
            return err;
        }
    }
    let mut ci = I915EngineClassInstance { engine_class: 0, engine_instance: 0 };
    if unsafe { copy_from_user(&mut ci, ptr::addr_of!((*ext).master).cast(), size_of::<I915EngineClassInstance>()) } != 0 {
        return -EFAULT;
    }
    let master = unsafe { intel_engine_lookup_user(i915, ci.engine_class as u8, ci.engine_instance as u8) };
    if master.is_null() {
        return -EINVAL;
    }
    if unsafe { crate::intel_engine_api_upstream::intel_engine_uses_guc(master) } {
        return -ENODEV;
    }
    let mut num_bonds = 0u16;
    if unsafe { get_user(&mut num_bonds, ptr::addr_of!((*ext).num_bonds)) } != 0 {
        return -EFAULT;
    }
    for n in 0..num_bonds as usize {
        if unsafe { copy_from_user(&mut ci, ptr::addr_of!((*ext).engines).cast::<I915EngineClassInstance>().add(n).cast(), size_of::<I915EngineClassInstance>()) } != 0 {
            return -EFAULT;
        }
        if unsafe { intel_engine_lookup_user(i915, ci.engine_class as u8, ci.engine_instance as u8) }.is_null() {
            return -EINVAL;
        }
    }
    0
}

// upstream: i915_gem_context.c set_proto_ctx_engines_parallel_submit()
unsafe extern "C" fn set_proto_ctx_engines_parallel_submit(
    base: *mut I915UserExtension,
    data: *mut c_void,
) -> c_int {
    let ext = unsafe { extension_payload::<I915ContextEnginesParallelSubmit>(base) };
    let set = unsafe { &mut *data.cast::<SetProtoCtxEngines>() };
    let i915 = set.i915;
    let mut slot = 0u16;
    let mut width = 0u16;
    let mut num_siblings = 0u16;
    if unsafe { get_user(&mut slot, ptr::addr_of!((*ext).engine_index)) } != 0
        || unsafe { get_user(&mut width, ptr::addr_of!((*ext).width)) } != 0
        || unsafe { get_user(&mut num_siblings, ptr::addr_of!((*ext).num_siblings)) } != 0
    {
        return -EFAULT;
    }
    if !unsafe { crate::intel_uc_types_upstream::intel_uc_uses_guc_submission(ptr::addr_of_mut!((*to_gt(i915)).uc)) }
        && num_siblings != 1
    {
        return -EINVAL;
    }
    if slot as u32 >= set.num_engines {
        return -EINVAL;
    }
    slot = array_index_nospec(slot as u32, set.num_engines) as u16;
    let pe = unsafe { &mut *set.engines.add(slot as usize) };
    if pe.r#type != I915GemEngineType::Invalid {
        return -EINVAL;
    }
    let mut flags = 0u64;
    if unsafe { get_user(&mut flags, ptr::addr_of!((*ext).flags)) } != 0 {
        return -EFAULT;
    }
    if flags != 0 {
        return -EINVAL;
    }
    for n in 0..3 {
        let err = unsafe { check_user_mbz(ptr::addr_of!((*ext).mbz64).cast::<u64>().add(n)) };
        if err != 0 {
            return err;
        }
    }
    if width < 2 || num_siblings == 0 {
        return -EINVAL;
    }
    let total = num_siblings as usize * width as usize;
    let siblings = unsafe { kmalloc_objs::<*mut IntelEngineCs, _>(total) };
    if siblings.is_null() {
        return -ENOMEM;
    }
    let mut previous_class = 0u16;
    let mut previous_mask = 0 as IntelEngineMask;
    for i in 0..width as usize {
        let mut current_mask = 0 as IntelEngineMask;
        for j in 0..num_siblings as usize {
            let n = i * num_siblings as usize + j;
            let mut ci = I915EngineClassInstance { engine_class: 0, engine_instance: 0 };
            let ci_ptr = unsafe { ptr::addr_of!((*ext).engines).cast::<I915EngineClassInstance>().add(n) };
            if unsafe { copy_from_user(&mut ci, ci_ptr.cast(), size_of::<I915EngineClassInstance>()) } != 0 {
                unsafe { kfree(siblings) };
                return -EFAULT;
            }
            let engine = unsafe { intel_engine_lookup_user(i915, ci.engine_class as u8, ci.engine_instance as u8) };
            if engine.is_null() {
                unsafe { kfree(siblings) };
                return -EINVAL;
            }
            if unsafe { (*engine).class as u32 == RENDER_CLASS as u32 || (*engine).class as u32 == COMPUTE_CLASS as u32 } {
                unsafe { kfree(siblings) };
                return -EINVAL;
            }
            if n != 0 && previous_class != ci.engine_class {
                unsafe { kfree(siblings) };
                return -EINVAL;
            }
            previous_class = ci.engine_class;
            current_mask |= unsafe { (*engine).logical_mask };
            unsafe { *siblings.add(n) = engine };
        }
        if i > 0 && current_mask != (previous_mask << 1) {
            unsafe { kfree(siblings) };
            return -EINVAL;
        }
        previous_mask = current_mask;
    }
    pe.r#type = I915GemEngineType::Parallel;
    pe.num_siblings = num_siblings as u32;
    pe.width = width as u32;
    pe.siblings = siblings;
    0
}

static SET_PROTO_CTX_ENGINES_EXTENSIONS: [I915UserExtensionFn; 3] = [
    Some(set_proto_ctx_engines_balance),
    Some(set_proto_ctx_engines_bond),
    Some(set_proto_ctx_engines_parallel_submit),
];

// upstream: i915_gem_context.c set_proto_ctx_engines()
unsafe fn set_proto_ctx_engines(
    fpriv: *mut DrmI915FilePrivate,
    pc: *mut I915GemProtoContext,
    args: *const DrmI915GemContextParam,
) -> c_int {
    let i915 = unsafe { (*file_private_view(fpriv)).i915 };
    if unsafe { (*pc).num_user_engines >= 0 } {
        return -EINVAL;
    }
    let size = unsafe { (*args).size as usize };
    let base_size = size_of::<I915ContextParamEngines>();
    let engine_size = size_of::<I915EngineClassInstance>();
    if size < base_size || (size - base_size) % engine_size != 0 {
        return -EINVAL;
    }
    let num_engines = (size - base_size) / engine_size;
    if num_engines > (I915_EXEC_RING_MASK + 1) as usize {
        return -EINVAL;
    }
    let engines = unsafe { kzalloc_objs::<I915GemProtoEngine, _>(num_engines) };
    if engines.is_null() {
        return -ENOMEM;
    }
    let user = u64_to_user_ptr(unsafe { (*args).value }).cast::<I915ContextParamEngines>();
    for n in 0..num_engines {
        let mut ci = I915EngineClassInstance { engine_class: 0, engine_instance: 0 };
        let ci_ptr = unsafe { ptr::addr_of!((*user).engines).cast::<I915EngineClassInstance>().add(n) };
        if unsafe { copy_from_user(&mut ci, ci_ptr.cast(), engine_size) } != 0 {
            unsafe { kfree(engines) };
            return -EFAULT;
        }
        unsafe { ptr::write_bytes(engines.add(n), 0, 1) };
        if ci.engine_class == u16::MAX && ci.engine_instance == I915_ENGINE_CLASS_INVALID_NONE {
            unsafe { (*engines.add(n)).r#type = I915GemEngineType::Invalid };
            continue;
        }
        let engine = unsafe { intel_engine_lookup_user(i915, ci.engine_class as u8, ci.engine_instance as u8) };
        if engine.is_null() {
            unsafe { kfree(engines) };
            return -ENOENT;
        }
        unsafe {
            (*engines.add(n)).r#type = I915GemEngineType::Physical;
            (*engines.add(n)).engine = engine;
        }
    }
    let mut extensions = 0u64;
    let mut ret = -EFAULT;
    if unsafe { get_user(&mut extensions, ptr::addr_of!((*user).extensions)) } == 0 {
        let mut set = SetProtoCtxEngines { i915, num_engines: num_engines as u32, engines };
        ret = unsafe {
            i915_user_extensions(
                u64_to_user_ptr(extensions).cast(),
                SET_PROTO_CTX_ENGINES_EXTENSIONS.as_ptr(),
                SET_PROTO_CTX_ENGINES_EXTENSIONS.len() as u32,
                ptr::addr_of_mut!(set).cast(),
            )
        };
    }
    if ret != 0 {
        for n in 0..num_engines {
            unsafe { kfree((*engines.add(n)).siblings) };
        }
        unsafe { kfree(engines) };
        return ret;
    }
    unsafe {
        (*pc).num_user_engines = num_engines as i32;
        (*pc).user_engines = engines;
    }
    0
}

// upstream: i915_gem_context.c set_proto_ctx_sseu()
unsafe fn set_proto_ctx_sseu(
    fpriv: *mut DrmI915FilePrivate,
    pc: *mut I915GemProtoContext,
    args: *mut DrmI915GemContextParam,
) -> c_int {
    let i915 = unsafe { (*file_private_view(fpriv)).i915 };
    if unsafe { (*args).size as usize } < size_of::<DrmI915GemContextParamSseu>() {
        return -EINVAL;
    }
    if unsafe { crate::linux::i915::GRAPHICS_VER(i915) } != 11 {
        return -ENODEV;
    }
    let mut user = core::mem::MaybeUninit::<DrmI915GemContextParamSseu>::uninit();
    if unsafe { copy_from_user(user.as_mut_ptr(), u64_to_user_ptr((*args).value), size_of::<DrmI915GemContextParamSseu>()) } != 0 {
        return -EFAULT;
    }
    let user = unsafe { user.assume_init() };
    if user.rsvd != 0 || user.flags & !I915_CONTEXT_SSEU_FLAG_ENGINE_INDEX != 0 {
        return -EINVAL;
    }
    if (user.flags & I915_CONTEXT_SSEU_FLAG_ENGINE_INDEX != 0) != (unsafe { (*pc).num_user_engines >= 0 }) {
        return -EINVAL;
    }
    let target = if unsafe { (*pc).num_user_engines >= 0 } {
        let idx = user.engine.engine_instance as u32;
        let count = unsafe { (*pc).num_user_engines as u32 };
        if idx >= count {
            return -EINVAL;
        }
        let idx = array_index_nospec(idx, count);
        let pe = unsafe { &mut *(*pc).user_engines.add(idx as usize) };
        if pe.engine.is_null() || unsafe { (*pe.engine).class as u32 != RENDER_CLASS as u32 } {
            return -EINVAL;
        }
        ptr::addr_of_mut!(pe.sseu)
    } else {
        if user.engine.engine_class != RENDER_CLASS as u16 || user.engine.engine_instance != 0 {
            return -EINVAL;
        }
        ptr::addr_of_mut!((*pc).legacy_rcs_sseu)
    };
    let gt = unsafe { to_gt(i915) };
    let ret = unsafe { i915_gem_user_to_context_sseu(gt, &user, target) };
    if ret == 0 {
        unsafe { (*args).size = size_of::<DrmI915GemContextParamSseu>() as u32 };
    }
    ret
}

// upstream: i915_gem_context.c set_proto_ctx_param()
unsafe fn set_proto_ctx_param(
    fpriv: *mut DrmI915FilePrivate,
    pc: *mut I915GemProtoContext,
    args: *mut DrmI915GemContextParam,
) -> c_int {
    let i915 = unsafe { (*file_private_view(fpriv)).i915 };
    let mut ret = 0;
    match unsafe { (*args).param } {
        I915_CONTEXT_PARAM_NO_ERROR_CAPTURE => {
            if unsafe { (*args).size } != 0 { ret = -EINVAL; }
            else if unsafe { (*args).value != 0 } { unsafe { (*pc).user_flags |= 1 << UCONTEXT_NO_ERROR_CAPTURE; } }
            else { unsafe { (*pc).user_flags &= !(1 << UCONTEXT_NO_ERROR_CAPTURE); } }
        }
        I915_CONTEXT_PARAM_BANNABLE => {
            if unsafe { (*args).size } != 0 { ret = -EINVAL; }
            else if unsafe { !capable(CAP_SYS_ADMIN) && (*args).value == 0 } { ret = -EPERM; }
            else if unsafe { (*args).value != 0 } { unsafe { (*pc).user_flags |= 1 << UCONTEXT_BANNABLE; } }
            else if unsafe { (*pc).uses_protected_content } { ret = -EPERM; }
            else { unsafe { (*pc).user_flags &= !(1 << UCONTEXT_BANNABLE); } }
        }
        I915_CONTEXT_PARAM_LOW_LATENCY => {
            let gt = unsafe { to_gt(i915) };
            if unsafe { crate::intel_uc_types_upstream::intel_uc_uses_guc_submission(ptr::addr_of_mut!((*gt).uc)) } {
                unsafe { (*pc).user_flags |= 1 << UCONTEXT_LOW_LATENCY; };
            } else { ret = -EINVAL; }
        }
        I915_CONTEXT_PARAM_RECOVERABLE => {
            if unsafe { (*args).size } != 0 { ret = -EINVAL; }
            else if unsafe { (*args).value == 0 } { unsafe { (*pc).user_flags &= !(1 << UCONTEXT_RECOVERABLE); } }
            else if unsafe { (*pc).uses_protected_content } { ret = -EPERM; }
            else { unsafe { (*pc).user_flags |= 1 << UCONTEXT_RECOVERABLE; } }
        }
        I915_CONTEXT_PARAM_PRIORITY => {
            ret = unsafe { validate_priority(i915, args) };
            if ret == 0 { unsafe { (*pc).sched.priority = (*args).value as i32; } }
        }
        I915_CONTEXT_PARAM_SSEU => ret = unsafe { set_proto_ctx_sseu(fpriv, pc, args) },
        I915_CONTEXT_PARAM_VM => ret = unsafe { set_proto_ctx_vm(fpriv, pc, args) },
        I915_CONTEXT_PARAM_ENGINES => ret = unsafe { set_proto_ctx_engines(fpriv, pc, args) },
        I915_CONTEXT_PARAM_PERSISTENCE => {
            if unsafe { (*args).size } != 0 { ret = -EINVAL; }
            else { ret = unsafe { proto_context_set_persistence(i915, pc, (*args).value != 0) }; }
        }
        I915_CONTEXT_PARAM_PROTECTED_CONTENT => {
            ret = unsafe { proto_context_set_protected(i915, pc, (*args).value != 0) };
        }
        I915_CONTEXT_PARAM_NO_ZEROMAP
        | I915_CONTEXT_PARAM_BAN_PERIOD
        | I915_CONTEXT_PARAM_RINGSIZE
        | I915_CONTEXT_PARAM_CONTEXT_IMAGE => ret = -EINVAL,
        _ => ret = -EINVAL,
    }
    ret
}

// Child-list traversal follows intel_context.parallel.children and the
// parallel.children.child_link member from intel_context_types.h.
unsafe fn for_each_context_child(
    parent: *mut IntelContext,
    mut visit: impl FnMut(*mut IntelContext),
) {
    let head = unsafe {
        ptr::addr_of_mut!((*parent).parallel.children.child_list)
            .cast::<ListHead>()
    };
    let mut child = ptr::null_mut::<IntelContext>();
    list_for_each_entry!(child, head, parallel.children.child_link, {
        visit(child);
    });
}

unsafe fn next_gem_engine(it: *mut I915GemEnginesIter) -> *mut IntelContext {
    let engines = unsafe { (*it).engines };
    if engines.is_null() {
        return ptr::null_mut();
    }
    loop {
        let idx = unsafe { (*it).idx };
        if idx >= unsafe { (*engines).num_engines } {
            return ptr::null_mut();
        }
        unsafe { (*it).idx = idx + 1 };
        let ce = unsafe { *(*engines).engines.as_ptr().add(idx as usize) };
        if !ce.is_null() {
            return ce;
        }
    }
}

// upstream: i915_gem_context.c intel_context_set_gem()
unsafe fn intel_context_set_gem(
    ce: *mut IntelContext,
    ctx: *mut I915GemContext,
    sseu: IntelSseu,
) -> c_int {
    let mut ret = 0;
    gem_bug_on!(unsafe { !(*ce).gem_context.is_null() });
    unsafe { ptr::write_volatile(ptr::addr_of_mut!((*ce).gem_context), ctx) };
    gem_bug_on!(unsafe { crate::intel_context_api_upstream::intel_context_is_pinned(ce) });
    unsafe {
        (*ce).ring_size = if (*(*ce).engine).class as u32 == COMPUTE_CLASS as u32 {
            512 * 1024
        } else {
            16 * 1024
        };
        i915_vm_put((*ce).vm);
        (*ce).vm = i915_gem_context_get_eb_vm(ctx);
    }
    if unsafe { (*ctx).sched.priority >= I915_PRIORITY_NORMAL }
        && unsafe { crate::intel_engine_types_upstream::intel_engine_has_timeslices((*ce).engine) }
        && unsafe { crate::intel_engine_types_upstream::intel_engine_has_semaphores((*ce).engine) }
    {
        set_bit(CONTEXT_USE_SEMAPHORES, unsafe { &mut (*ce).flags });
    }
    if CONFIG_DRM_I915_REQUEST_TIMEOUT != 0
        && unsafe { (*(*ctx).i915).params.request_timeout_ms != 0 }
    {
        let timeout_ms = unsafe { (*(*ctx).i915).params.request_timeout_ms };
        unsafe { (*ce).watchdog.timeout_us = timeout_ms as u64 * 1000 };
    }
    if sseu.slice_mask != 0
        && unsafe { (*(*ce).engine).class as u32 == RENDER_CLASS as u32 }
    {
        ret = unsafe { intel_context_reconfigure_sseu(ce, &sseu) };
    }
    if test_bit(UCONTEXT_LOW_LATENCY, unsafe { &(*ctx).user_flags }) {
        set_bit(CONTEXT_LOW_LATENCY, unsafe { &mut (*ce).flags });
    }
    ret
}

// upstream: i915_gem_context.c __unpin_engines()
unsafe fn __unpin_engines(e: *mut I915GemEngines, count: u32) {
    let mut count = count;
    while count != 0 {
        count -= 1;
        let ce = unsafe { *(*e).engines.as_ptr().add(count as usize) };
        if ce.is_null() || !test_bit(CONTEXT_PERMA_PIN, unsafe { &(*ce).flags }) {
            continue;
        }
        unsafe {
            for_each_context_child(ce, |child| {
                crate::intel_context_api_upstream::intel_context_unpin(child);
            });
            crate::intel_context_api_upstream::intel_context_unpin(ce);
        }
    }
}

// upstream: i915_gem_context.c unpin_engines()
unsafe fn unpin_engines(e: *mut I915GemEngines) {
    unsafe { __unpin_engines(e, (*e).num_engines) };
}

// upstream: i915_gem_context.c __free_engines()
unsafe fn __free_engines(e: *mut I915GemEngines, count: u32) {
    let mut count = count;
    while count != 0 {
        count -= 1;
        let ce = unsafe { *(*e).engines.as_ptr().add(count as usize) };
        if !ce.is_null() {
            unsafe { crate::intel_context_api_upstream::intel_context_put(ce) };
        }
    }
    unsafe { kfree(e) };
}

// upstream: i915_gem_context.c free_engines()
unsafe fn free_engines(e: *mut I915GemEngines) {
    unsafe { __free_engines(e, (*e).num_engines) };
}

// upstream: i915_gem_context.c free_engines_rcu()
unsafe extern "C" fn free_engines_rcu(rcu: *mut RcuHead) {
    let engines = container_of!(rcu, I915GemEngines, link_or_rcu.rcu).cast::<I915GemEngines>();
    unsafe {
        crate::i915_sw_fence_upstream::i915_sw_fence_fini(ptr::addr_of_mut!((*engines).fence));
        free_engines(engines);
    }
}

// upstream: i915_gem_context.c accumulate_runtime()
unsafe fn accumulate_runtime(client: *mut I915DrmClient, engines: *mut I915GemEngines) {
    if client.is_null() {
        return;
    }
    let client_view = client.cast::<I915DrmClientRuntimeView>();
    let mut it = I915GemEnginesIter { idx: 0, engines };
    loop {
        let ce = unsafe { next_gem_engine(&mut it) };
        if ce.is_null() {
            break;
        }
        let class = unsafe { (*(*ce).engine).uabi_class as usize };
        gem_bug_on!(class >= unsafe { (*client_view).past_runtime.len() });
        let runtime = unsafe { crate::intel_context_upstream::intel_context_get_total_runtime_ns(ce) };
        unsafe { (*client_view).past_runtime[class].fetch_add(runtime, Ordering::Relaxed) };
    }
}

// upstream: i915_gem_context.c engines_notify()
unsafe extern "C" fn engines_notify(
    fence: *mut I915SwFence,
    state: I915SwFenceNotify,
) -> c_int {
    let engines = container_of!(fence, I915GemEngines, fence).cast::<I915GemEngines>();
    let ctx = unsafe { (*engines).ctx };
    match state {
        I915SwFenceNotify::FenceComplete => {
            let link = unsafe {
                ptr::addr_of_mut!((*engines).link_or_rcu.link).cast::<ListHead>()
            };
            if !unsafe { crate::linux_list::list_empty(&*link) } {
                let mut flags = 0;
                unsafe { crate::linux::locks::spin_lock_irqsave(&mut (*ctx).stale.lock, &mut flags) };
                unsafe { crate::linux_list::list_del(link) };
                unsafe { crate::linux::locks::spin_unlock_irqrestore(&mut (*ctx).stale.lock, flags) };
            }
            unsafe {
                accumulate_runtime((*ctx).client, engines);
                i915_gem_context_put(ctx);
            }
        }
        I915SwFenceNotify::FenceFree => unsafe {
            let rcu = ptr::addr_of_mut!((*engines).link_or_rcu.rcu).cast::<RcuHead>();
            crate::linux::rcu::init_rcu_head(&mut *rcu);
            crate::linux::rcu::call_rcu(rcu, free_engines_rcu);
        },
    }
    NOTIFY_DONE
}

// upstream: i915_gem_context.c alloc_engines()
unsafe fn alloc_engines(count: u32) -> *mut I915GemEngines {
    let Some(bytes) = size_of::<I915GemEngines>()
        .checked_add((count as usize).saturating_mul(size_of::<*mut IntelContext>()))
    else {
        return ptr::null_mut();
    };
    let engines = kzalloc(bytes, GFP_KERNEL).cast::<I915GemEngines>();
    if engines.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        crate::i915_sw_fence_upstream::i915_sw_fence_init(
            ptr::addr_of_mut!((*engines).fence),
            Some(engines_notify),
        );
    }
    engines
}

// upstream: i915_gem_context.c default_engines()
unsafe fn default_engines(ctx: *mut I915GemContext, rcs_sseu: IntelSseu) -> *mut I915GemEngines {
    let max = ENGINE_COUNT as u32;
    let engines = unsafe { alloc_engines(max) };
    if engines.is_null() {
        return ERR_PTR::<I915GemEngines>(-ENOMEM);
    }
    unsafe {
        (*engines).num_engines = 0;
        let i915 = (*ctx).i915;
        // The UABI tree overlay is pinned to the source layout in the engine
        // registration owner.
        let root = crate::intel_engine_user_upstream::engine_uabi_tree(i915);
        let mut rb = crate::linux::rbtree::rb_first(root);
        while !rb.is_null() {
            let engine = container_of!(rb, IntelEngineCs, uabi).cast::<IntelEngineCs>();
            let legacy_idx = (*engine).legacy_idx;
            rb = crate::linux::rbtree::rb_next(rb);
            if legacy_idx == INVALID_ENGINE {
                continue;
            }
            gem_bug_on!((legacy_idx as u32) >= max);
            let slot = ptr::addr_of_mut!(*(*engines).engines.as_mut_ptr().add(legacy_idx as usize));
            gem_bug_on!(!(*slot).is_null());
            let ce = crate::intel_context_upstream::intel_context_create(engine);
            if is_err_ptr(ce) {
                let err = ptr_err(ce);
                free_engines(engines);
                return ERR_PTR::<I915GemEngines>(err);
            }
            *slot = ce;
            (*engines).num_engines = core::cmp::max((*engines).num_engines, (legacy_idx + 1) as u32);
            let sseu = if (*engine).class as u32 == RENDER_CLASS as u32 {
                rcs_sseu
            } else {
                core::mem::zeroed()
            };
            let ret = intel_context_set_gem(ce, ctx, sseu);
            if ret != 0 {
                free_engines(engines);
                return ERR_PTR::<I915GemEngines>(ret);
            }
        }
    }
    engines
}

// upstream: i915_gem_context.c perma_pin_contexts()
unsafe fn perma_pin_contexts(ce: *mut IntelContext) -> c_int {
    gem_bug_on!(!unsafe { crate::intel_context_api_upstream::intel_context_is_parent(ce) });
    let ret = unsafe { crate::intel_context_api_upstream::intel_context_pin(ce) };
    if ret != 0 {
        return ret;
    }
    let mut pinned = 0u32;
    let mut failed = 0;
    unsafe {
        for_each_context_child(ce, |child| {
            if failed == 0 {
                failed = crate::intel_context_api_upstream::intel_context_pin(child);
                if failed == 0 {
                    pinned += 1;
                }
            }
        });
    }
    if failed == 0 {
        set_bit(CONTEXT_PERMA_PIN, unsafe { &mut (*ce).flags });
        return 0;
    }
    unsafe { crate::intel_context_api_upstream::intel_context_unpin(ce) };
    let mut seen = 0u32;
    unsafe {
        for_each_context_child(ce, |child| {
            if seen < pinned {
                crate::intel_context_api_upstream::intel_context_unpin(child);
                seen += 1;
            }
        });
    }
    failed
}

// upstream: i915_gem_context.c user_engines()
unsafe fn user_engines(
    ctx: *mut I915GemContext,
    num_engines: u32,
    pe: *mut I915GemProtoEngine,
) -> *mut I915GemEngines {
    let engines = unsafe { alloc_engines(num_engines) };
    if engines.is_null() {
        return ERR_PTR::<I915GemEngines>(-ENOMEM);
    }
    unsafe { (*engines).num_engines = num_engines };
    for n in 0..num_engines as usize {
        let prototype = unsafe { &mut *pe.add(n) };
        let ce = match prototype.r#type {
            I915GemEngineType::Physical => unsafe {
                crate::intel_context_upstream::intel_context_create(prototype.engine)
            },
            I915GemEngineType::Balanced => unsafe {
                crate::intel_engine_cs_upstream::intel_engine_create_virtual(
                    prototype.siblings,
                    prototype.num_siblings,
                    0,
                )
            },
            I915GemEngineType::Parallel => unsafe {
                crate::intel_engine_api_upstream::intel_engine_create_parallel(
                    prototype.siblings,
                    prototype.num_siblings,
                    prototype.width,
                )
            },
            I915GemEngineType::Invalid => continue,
        };
        if is_err_ptr(ce) {
            let err = ptr_err(ce);
            unsafe { free_engines(engines) };
            return ERR_PTR::<I915GemEngines>(err);
        }
        unsafe { *(*engines).engines.as_mut_ptr().add(n) = ce };
        let ret = unsafe { intel_context_set_gem(ce, ctx, prototype.sseu) };
        if ret != 0 {
            unsafe { free_engines(engines) };
            return ERR_PTR::<I915GemEngines>(ret);
        }
        let mut child_ret = 0;
        unsafe {
            for_each_context_child(ce, |child| {
                if child_ret == 0 {
                    child_ret = intel_context_set_gem(child, ctx, prototype.sseu);
                }
            });
        }
        if child_ret != 0 {
            unsafe { free_engines(engines) };
            return ERR_PTR::<I915GemEngines>(child_ret);
        }
        if prototype.r#type == I915GemEngineType::Parallel {
            let ret = unsafe { perma_pin_contexts(ce) };
            if ret != 0 {
                unsafe { free_engines(engines) };
                return ERR_PTR::<I915GemEngines>(ret);
            }
        }
    }
    engines
}

// kfree_rcu() is the Linux RCU callback body for this context owner.
unsafe extern "C" fn free_context_rcu(rcu: *mut RcuHead) {
    let ctx = container_of!(rcu, I915GemContext, rcu).cast::<I915GemContext>();
    unsafe { kfree(ctx) };
}

// upstream: i915_gem_context.c i915_gem_context_release_work()
unsafe extern "C" fn i915_gem_context_release_work(work: *mut WorkStruct) {
    let ctx = container_of!(work, I915GemContext, release_work).cast::<I915GemContext>();
    unsafe {
        trace_i915_context_free(ctx);
        GEM_BUG_ON!(i915_gem_context_is_closed(ctx) == false);
        let registry = context_registry((*ctx).i915);
        crate::linux::locks::spin_lock(&mut (*registry).lock);
        crate::linux_list::list_del(ptr::addr_of_mut!((*ctx).link));
        crate::linux::locks::spin_unlock(&mut (*registry).lock);

        if !(*ctx).syncobj.is_null() {
            drm_syncobj_put((*ctx).syncobj.cast());
        }
        if !(*ctx).vm.is_null() {
            i915_vm_put((*ctx).vm);
        }
        if !(*ctx).pxp_wakeref.is_null() {
            crate::intel_runtime_pm_upstream::intel_runtime_pm_put_raw(
                ptr::addr_of_mut!((*(*ctx).i915).runtime_pm).cast(),
                (*ctx).pxp_wakeref,
            );
        }
        if !(*ctx).client.is_null() {
            i915_drm_client_put((*ctx).client);
        }
        crate::linux::mutex::mutex_destroy(&mut (*ctx).engines_mutex);
        crate::linux::mutex::mutex_destroy(&mut (*ctx).lut_mutex);
        put_pid((*ctx).pid.cast());
        crate::linux::mutex::mutex_destroy(&mut (*ctx).mutex);
        let rcu = ptr::addr_of_mut!((*ctx).rcu);
        crate::linux::rcu::call_rcu(rcu, free_context_rcu);
    }
}

// upstream: i915_gem_context.c i915_gem_context_release()
unsafe extern "C" fn i915_gem_context_release(ref_: *mut Kref) {
    let ctx = container_of!(ref_, I915GemContext, r#ref).cast::<I915GemContext>();
    unsafe {
        crate::linux::workqueue::queue_work((*(*ctx).i915).wq, &mut (*ctx).release_work);
    }
}

// upstream: i915_gem_context.c __context_engines_static()
unsafe fn __context_engines_static(ctx: *const I915GemContext) -> *mut I915GemEngines {
    unsafe {
        AtomicPtr::from_ptr(ptr::addr_of!((*ctx).engines).cast_mut())
            .load(Ordering::Acquire)
    }
}

// upstream: i915_gem_context.c __reset_context()
unsafe fn __reset_context(ctx: *mut I915GemContext, engine: *mut IntelEngineCs) {
    let message = b"context closure in %s\0";
    unsafe {
        crate::intel_reset_upstream::intel_gt_handle_error_format(
            (*engine).gt,
            (*engine).mask,
            0,
            message.as_ptr().cast(),
            &[&(*ctx).name.as_ptr() as &dyn crate::linux::print::CFormatArg],
        );
    }
}

// upstream: i915_gem_context.c __cancel_engine()
unsafe fn __cancel_engine(engine: *mut IntelEngineCs) -> bool {
    unsafe { intel_engine_pulse(engine) == 0 }
}

// upstream: i915_gem_context.c active_engine()
unsafe fn active_engine(ce: *mut IntelContext) -> *mut IntelEngineCs {
    if unsafe { crate::intel_context_api_upstream::intel_context_has_inflight(ce) } {
        return unsafe { crate::linux::contexts::intel_context_inflight(ce) };
    }
    if unsafe { (*ce).timeline.is_null() } {
        return ptr::null_mut();
    }
    let mut engine = ptr::null_mut::<IntelEngineCs>();
    let mut rq = ptr::null_mut::<I915Request>();
    crate::linux::rcu::rcu_read_lock();
    list_for_each_entry_reverse!(rq, unsafe { ptr::addr_of_mut!((*(*ce).timeline).requests) }, link, {
        if unsafe { i915_request_get_rcu(rq) }.is_null() {
            break;
        }
        let mut found = true;
        if unsafe { (*rq).timeline == (*ce).timeline } {
            found = unsafe { i915_request_active_engine(rq, &mut engine) };
        }
        unsafe { i915_request_put(rq) };
        if found {
            break;
        }
    });
    crate::linux::rcu::rcu_read_unlock();
    engine
}

// upstream: i915_gem_context.c kill_engines()
unsafe fn kill_engines(engines: *mut I915GemEngines, exit: bool, persistent: bool) {
    let mut it = I915GemEnginesIter { idx: 0, engines };
    loop {
        let ce = unsafe { next_gem_engine(&mut it) };
        if ce.is_null() {
            break;
        }
        if (exit || !persistent)
            && unsafe { crate::intel_context_upstream::intel_context_revoke(ce) }
        {
            continue;
        }
        let engine = unsafe { active_engine(ce) };
        if !engine.is_null() && !unsafe { __cancel_engine(engine) } && (exit || !persistent) {
            unsafe { __reset_context((*engines).ctx, engine) };
        }
    }
}

// upstream: i915_gem_context.c kill_context()
unsafe fn kill_context(ctx: *mut I915GemContext) {
    let head = unsafe { ptr::addr_of_mut!((*ctx).stale.engines) };
    unsafe { crate::linux::locks::spin_lock_irq(&mut (*ctx).stale.lock) };
    GEM_BUG_ON!(unsafe { !i915_gem_context_is_closed(ctx) });
    let mut node = unsafe { (*head).next };
    while node != head {
        let pos = container_of!(node, I915GemEngines, link_or_rcu.link).cast::<I915GemEngines>();
        let mut next = unsafe { (*node).next };
        let fence = unsafe { ptr::addr_of_mut!((*pos).fence) };
        if !unsafe { crate::linux::sw_fence::i915_sw_fence_await(&mut *fence) } {
            unsafe { crate::linux_list::list_del_init(node) };
            node = next;
            continue;
        }
        unsafe { crate::linux::locks::spin_unlock_irq(&mut (*ctx).stale.lock) };
        unsafe {
            kill_engines(
                pos,
                !(*(*ctx).i915).params.enable_hangcheck,
                i915_gem_context_is_persistent(ctx),
            );
        }
        unsafe { crate::linux::locks::spin_lock_irq(&mut (*ctx).stale.lock) };
        GEM_BUG_ON!(unsafe { crate::linux::sw_fence::i915_sw_fence_signaled(&*fence) });
        next = unsafe { (*node).next };
        unsafe { crate::linux_list::list_del_init(node) };
        unsafe { crate::i915_sw_fence_upstream::i915_sw_fence_complete(fence) };
        node = next;
    }
    unsafe { crate::linux::locks::spin_unlock_irq(&mut (*ctx).stale.lock) };
}

// upstream: i915_gem_context.c engines_idle_release()
unsafe fn engines_idle_release(ctx: *mut I915GemContext, engines: *mut I915GemEngines) {
    let link = unsafe { ptr::addr_of_mut!((*engines).link_or_rcu.link).cast::<ListHead>() };
    unsafe {
        crate::linux_list::INIT_LIST_HEAD(link);
        (*engines).ctx = i915_gem_context_get(ctx);
    }
    let mut it = I915GemEnginesIter { idx: 0, engines };
    let mut ret = 0;
    loop {
        let ce = unsafe { next_gem_engine(&mut it) };
        if ce.is_null() {
            break;
        }
        unsafe { crate::intel_context_api_upstream::intel_context_close(ce) };
        if !unsafe { crate::intel_context_api_upstream::intel_context_pin_if_active(ce) } {
            continue;
        }
        ret = unsafe {
            crate::i915_active_upstream::i915_sw_fence_await_active(
                ptr::addr_of_mut!((*engines).fence),
                ptr::addr_of_mut!((*ce).active),
                I915_ACTIVE_AWAIT_BARRIER,
            )
        };
        unsafe { crate::intel_context_api_upstream::intel_context_unpin(ce) };
        if ret != 0 {
            break;
        }
    }
    unsafe { crate::linux::locks::spin_lock_irq(&mut (*ctx).stale.lock) };
    if !unsafe { i915_gem_context_is_closed(ctx) } {
        unsafe { crate::linux_list::list_add_tail(link, &mut (*ctx).stale.engines) };
    }
    unsafe { crate::linux::locks::spin_unlock_irq(&mut (*ctx).stale.lock) };
    if unsafe { crate::linux_list::list_empty(&*link) } {
        unsafe { kill_engines(engines, true, i915_gem_context_is_persistent(ctx)) };
    }
    unsafe { crate::i915_sw_fence_upstream::i915_sw_fence_commit(&mut (*engines).fence) };
}

// upstream: i915_gem_context.c set_closed_name()
unsafe fn set_closed_name(ctx: *mut I915GemContext) {
    let name = unsafe { (*ctx).name.as_mut_ptr() };
    let mut len = 0usize;
    while len < unsafe { (*ctx).name.len() } && unsafe { *name.add(len) } != 0 {
        len += 1;
    }
    let mut bracket = None;
    for idx in 0..len {
        if unsafe { *name.add(idx) } == b'[' as c_char {
            bracket = Some(idx);
        }
    }
    let Some(open) = bracket else { return };
    unsafe { *name.add(open) = b'<' as c_char };
    for idx in open + 1..len {
        if unsafe { *name.add(idx) } == b']' as c_char {
            unsafe { *name.add(idx) = b'>' as c_char };
            break;
        }
    }
}

// upstream: i915_gem_context.c context_close()
unsafe fn context_close(ctx: *mut I915GemContext) {
    let engines_lock = unsafe { ptr::addr_of_mut!((*ctx).engines_mutex) };
    crate::linux::mutex::mutex_lock(unsafe { &mut *engines_lock });
    unsafe {
        unpin_engines(__context_engines_static(ctx));
    }
    let engines = unsafe {
        AtomicPtr::from_ptr(ptr::addr_of_mut!((*ctx).engines))
            .swap(ptr::null_mut(), Ordering::AcqRel)
    };
    unsafe { engines_idle_release(ctx, engines) };
    unsafe { i915_gem_context_set_closed(ctx) };
    crate::linux::mutex::mutex_unlock(unsafe { &mut *engines_lock });

    let ctx_lock = unsafe { ptr::addr_of_mut!((*ctx).mutex) };
    crate::linux::mutex::mutex_lock(unsafe { &mut *ctx_lock });
    unsafe { set_closed_name(ctx) };
    unsafe { lut_close(ctx) };
    unsafe { (*ctx).file_priv = ERR_PTR::<DrmI915FilePrivate>(-EBADF) };
    let client = unsafe { (*ctx).client };
    if !client.is_null() {
        let view = client.cast::<I915ClientView>();
        crate::linux::locks::spin_lock(unsafe { &mut (*view).ctx_lock });
        unsafe { crate::linux_list::list_del_rcu(ptr::addr_of_mut!((*ctx).client_link)) };
        crate::linux::locks::spin_unlock(unsafe { &mut (*view).ctx_lock });
    }
    crate::linux::mutex::mutex_unlock(unsafe { &mut *ctx_lock });
    unsafe { kill_context(ctx) };
    unsafe { i915_gem_context_put(ctx) };
}

// upstream: i915_gem_context.c __context_set_persistence()
unsafe fn __context_set_persistence(ctx: *mut I915GemContext, state: bool) -> c_int {
    if unsafe { i915_gem_context_is_persistent(ctx) } == state {
        return 0;
    }
    if state {
        if unsafe { !(*(*ctx).i915).params.enable_hangcheck } {
            return -EINVAL;
        }
        set_bit(UCONTEXT_PERSISTENCE, unsafe { &mut (*ctx).user_flags });
    } else {
        if unsafe { scheduler_caps((*ctx).i915) & I915_SCHEDULER_CAP_PREEMPTION == 0 } {
            return -ENODEV;
        }
        if unsafe { !intel_has_reset_engine(to_gt((*ctx).i915)) } {
            return -ENODEV;
        }
        clear_bit(UCONTEXT_PERSISTENCE, unsafe { &mut (*ctx).user_flags });
    }
    0
}

// Upstream's `i915_gem_context_set_user_engines()` / clear helper semantics.
unsafe fn i915_gem_context_set_user_engines(ctx: *mut I915GemContext) {
    set_bit(CONTEXT_USER_ENGINES, unsafe { &mut (*ctx).flags });
}
unsafe fn i915_gem_context_clear_user_engines(ctx: *mut I915GemContext) {
    clear_bit(CONTEXT_USER_ENGINES, unsafe { &mut (*ctx).flags });
}

// upstream: i915_gem_context.c i915_gem_create_context()
unsafe fn i915_gem_create_context(
    i915: *mut DrmI915Private,
    pc: *const I915GemProtoContext,
) -> *mut I915GemContext {
    let ctx = unsafe { kzalloc_obj::<I915GemContext>() };
    if ctx.is_null() {
        return ERR_PTR::<I915GemContext>(-ENOMEM);
    }
    unsafe {
        kref_init(ptr::addr_of_mut!((*ctx).r#ref));
        (*ctx).i915 = i915;
        (*ctx).sched = (*pc).sched;
        crate::linux::mutex::mutex_init(&mut (*ctx).mutex);
        crate::linux_list::INIT_LIST_HEAD(&mut (*ctx).link);
        crate::linux::workqueue::INIT_WORK_C(&mut (*ctx).release_work, i915_gem_context_release_work);
        crate::linux::locks::spin_lock_init(&mut (*ctx).stale.lock);
        crate::linux_list::INIT_LIST_HEAD(&mut (*ctx).stale.engines);
    }

    let mut vm = ptr::null_mut::<I915AddressSpace>();
    unsafe {
        if !(*pc).vm.is_null() {
            vm = i915_vm_get((*pc).vm);
        } else if has_full_ppgtt(i915) {
            let ppgtt = crate::intel_gtt_api_upstream::i915_ppgtt_create(to_gt(i915), 0);
            if is_err_ptr(ppgtt) {
                let err = ptr_err(ppgtt);
                kfree(ctx);
                return ERR_PTR::<I915GemContext>(err);
            }
            (*ppgtt).vm.fpriv = (*pc).fpriv;
            vm = ptr::addr_of_mut!((*ppgtt).vm);
        }
        if !vm.is_null() {
            (*ctx).vm = vm;
        }

        // Set before creating engines: intel_context_set_gem() consumes flags.
        (*ctx).user_flags = (*pc).user_flags;
        crate::linux::mutex::mutex_init(&mut (*ctx).engines_mutex);
    }
    let engines = unsafe {
        if (*pc).num_user_engines >= 0 {
            i915_gem_context_set_user_engines(ctx);
            user_engines(ctx, (*pc).num_user_engines as u32, (*pc).user_engines)
        } else {
            i915_gem_context_clear_user_engines(ctx);
            default_engines(ctx, (*pc).legacy_rcs_sseu)
        }
    };
    if is_err_ptr(engines) {
        let err = ptr_err(engines);
        unsafe {
            if !(*ctx).vm.is_null() {
                i915_vm_put((*ctx).vm);
            }
            kfree(ctx);
        }
        return ERR_PTR::<I915GemContext>(err);
    }
    unsafe {
        AtomicPtr::from_ptr(ptr::addr_of_mut!((*ctx).engines)).store(engines, Ordering::Release);
        INIT_RADIX_TREE!(&mut (*ctx).handles_vma, GFP_KERNEL);
        crate::linux::mutex::mutex_init(&mut (*ctx).lut_mutex);
        (*ctx).remap_slice = all_l3_slices(i915);
        let now = crate::linux::primitives::jiffies() as c_ulong;
        for stamp in &mut (*ctx).hang_timestamp {
            *stamp = now.wrapping_sub(CONTEXT_FAST_HANG_JIFFIES as c_ulong);
        }
    }
    if unsafe { (*pc).single_timeline } {
        let ret = unsafe {
            drm_syncobj_create(
                ptr::addr_of_mut!((*ctx).syncobj).cast(),
                DRM_SYNCOBJ_CREATE_SIGNALED,
                ptr::null_mut(),
            )
        };
        if ret != 0 {
            unsafe {
                free_engines(engines);
                if !(*ctx).vm.is_null() { i915_vm_put((*ctx).vm); }
                kfree(ctx);
            }
            return ERR_PTR::<I915GemContext>(ret);
        }
    }
    if unsafe { (*pc).uses_protected_content } {
        unsafe {
            (*ctx).pxp_wakeref = crate::intel_runtime_pm_upstream::intel_runtime_pm_get(
                ptr::addr_of_mut!((*i915).runtime_pm).cast(),
            );
            (*ctx).uses_protected_content = true;
        }
    }
    unsafe { trace_i915_context_create(ctx) };
    ctx
}

// upstream: i915_gem_context.c init_contexts()
unsafe fn init_contexts(contexts: *mut I915ClientContexts) {
    unsafe {
        crate::linux::locks::spin_lock_init(&mut (*contexts).lock);
        crate::linux_list::INIT_LIST_HEAD(&mut (*contexts).list);
    }
}

// upstream: i915_gem_context.c i915_gem_init__contexts()
pub unsafe fn i915_gem_init__contexts(i915: *mut DrmI915Private) {
    unsafe { init_contexts(context_registry(i915)) };
}

// `list_add_tail_rcu()` from Linux 7.2.3 list.h.
unsafe fn list_add_tail_rcu_context(new: *mut ListHead, head: *mut ListHead) {
    unsafe {
        let prev = (*head).prev;
        (*new).next = head;
        (*new).prev = prev;
        core::sync::atomic::fence(Ordering::Release);
        (*prev).next = new;
        (*head).prev = new;
    }
}

// upstream: i915_gem_context.c gem_context_register()
unsafe fn gem_context_register(ctx: *mut I915GemContext, fpriv: *mut DrmI915FilePrivate, id: u32) {
    unsafe {
        let view = file_private_view(fpriv);
        (*ctx).file_priv = fpriv;
        let task = current_task_ptr();
        (*ctx).pid = get_task_pid(task, PIDTYPE_PID).cast();
        (*ctx).client = i915_drm_client_get((*view).client);
        let mut comm = [0 as c_char; crate::i915_gem_context_types_upstream::TASK_COMM_LEN];
        get_task_comm(comm.as_mut_ptr(), task);
        snprintf(
            (*ctx).name.as_mut_ptr(),
            (*ctx).name.len(),
            b"%s[%d]\0".as_ptr().cast(),
            comm.as_ptr(),
            pid_nr((*ctx).pid.cast()),
        );
        let client = (*ctx).client.cast::<I915ClientView>();
        crate::linux::locks::spin_lock(&mut (*client).ctx_lock);
        list_add_tail_rcu_context(ptr::addr_of_mut!((*ctx).client_link), &mut (*client).ctx_list);
        crate::linux::locks::spin_unlock(&mut (*client).ctx_lock);

        let registry = context_registry((*ctx).i915);
        crate::linux::locks::spin_lock(&mut (*registry).lock);
        crate::linux_list::list_add_tail(&mut (*ctx).link, &mut (*registry).list);
        crate::linux::locks::spin_unlock(&mut (*registry).lock);

        let old = xa_store(ptr::addr_of_mut!((*view).context_xa), id as c_ulong, ctx.cast(), GFP_KERNEL);
        if !old.is_null() {
            WARN_ON!(true);
        }
    }
}

unsafe extern "C" {
    #[link_name = "tk_linux_snprintf"]
    fn snprintf(buf: *mut c_char, size: usize, fmt: *const c_char, ...) -> c_int;
}

// upstream: i915_gem_context.c i915_gem_context_open()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_context_open(i915: *mut DrmI915Private, file: *mut DrmFile) -> c_int {
    let fpriv = unsafe { (*file).driver_priv.cast::<DrmI915FilePrivate>() };
    let view = unsafe { file_private_view(fpriv) };
    crate::linux::mutex::mutex_init(unsafe { &mut (*view).proto_context_lock });
    crate::linux::xarray::xa_init_flags(unsafe { &mut (*view).proto_context_xa }, XA_FLAGS_ALLOC);
    crate::linux::xarray::xa_init_flags(unsafe { &mut (*view).context_xa }, XA_FLAGS_ALLOC1);
    crate::linux::xarray::xa_init_flags(unsafe { &mut (*view).vm_xa }, XA_FLAGS_ALLOC1);
    let pc = unsafe { proto_context_create(fpriv, i915, 0) };
    if is_err_ptr(pc) {
        let err = ptr_err(pc);
        unsafe {
            crate::linux::xarray::xa_destroy(&mut (*view).vm_xa);
            crate::linux::xarray::xa_destroy(&mut (*view).context_xa);
            crate::linux::xarray::xa_destroy(&mut (*view).proto_context_xa);
        }
        crate::linux::mutex::mutex_destroy(unsafe { &mut (*view).proto_context_lock });
        return err;
    }
    let ctx = unsafe { i915_gem_create_context(i915, pc) };
    unsafe { proto_context_close(i915, pc) };
    if is_err_ptr(ctx) {
        let err = ptr_err(ctx);
        unsafe {
            crate::linux::xarray::xa_destroy(&mut (*view).vm_xa);
            crate::linux::xarray::xa_destroy(&mut (*view).context_xa);
            crate::linux::xarray::xa_destroy(&mut (*view).proto_context_xa);
        }
        crate::linux::mutex::mutex_destroy(unsafe { &mut (*view).proto_context_lock });
        return err;
    }
    unsafe { gem_context_register(ctx, fpriv, 0) };
    0
}

// upstream: i915_gem_context.c i915_gem_context_close()
pub unsafe fn i915_gem_context_close(file: *mut DrmFile) {
    let fpriv = unsafe { (*file).driver_priv.cast::<DrmI915FilePrivate>() };
    let view = unsafe { file_private_view(fpriv) };
    xa_for_each!(unsafe { &mut (*view).proto_context_xa }, idx, pc, {
        unsafe { proto_context_close((*view).i915, pc) };
        let _ = idx;
    });
    unsafe { crate::linux::xarray::xa_destroy(&mut (*view).proto_context_xa) };
    crate::linux::mutex::mutex_destroy(unsafe { &mut (*view).proto_context_lock });
    xa_for_each!(unsafe { &mut (*view).context_xa }, idx, ctx, {
        unsafe { context_close(ctx) };
        let _ = idx;
    });
    unsafe { crate::linux::xarray::xa_destroy(&mut (*view).context_xa) };
    xa_for_each!(unsafe { &mut (*view).vm_xa }, idx, vm, {
        unsafe { i915_vm_put(vm) };
        let _ = idx;
    });
    unsafe { crate::linux::xarray::xa_destroy(&mut (*view).vm_xa) };
}

// upstream: i915_gem_context.c i915_gem_vm_create_ioctl()
pub unsafe fn i915_gem_vm_create_ioctl(dev: *mut c_void, data: *mut c_void, file: *mut DrmFile) -> c_int {
    let i915 = dev.cast::<DrmI915Private>();
    let args = data.cast::<DrmI915GemVmControl>();
    let fpriv = unsafe { (*file).driver_priv.cast::<DrmI915FilePrivate>() };
    if !unsafe { has_full_ppgtt(i915) } {
        return -ENODEV;
    }
    if unsafe { (*args).flags != 0 } { return -EINVAL; }
    let ppgtt = unsafe { crate::intel_gtt_api_upstream::i915_ppgtt_create(to_gt(i915), 0) };
    if is_err_ptr(ppgtt) { return ptr_err(ppgtt); }
    if unsafe { (*args).extensions != 0 } {
        let ret = unsafe {
            i915_user_extensions(
                u64_to_user_ptr((*args).extensions).cast(),
                ptr::null(),
                0,
                ppgtt.cast(),
            )
        };
        if ret != 0 {
            unsafe { i915_vm_put(ptr::addr_of_mut!((*ppgtt).vm)) };
            return ret;
        }
    }
    let view = unsafe { file_private_view(fpriv) };
    let mut id = 0u32;
    let ret = unsafe {
        xa_alloc(
            ptr::addr_of_mut!((*view).vm_xa),
            &mut id,
            ptr::addr_of_mut!((*ppgtt).vm),
            XaLimit { min: XA_LIMIT_32B_MIN, max: XA_LIMIT_32B_MAX },
        )
    };
    if ret != 0 {
        unsafe { i915_vm_put(ptr::addr_of_mut!((*ppgtt).vm)) };
        return ret;
    }
    gem_bug_on!(id == 0);
    unsafe {
        (*args).vm_id = id;
        (*ppgtt).vm.fpriv = fpriv;
    }
    0
}

// upstream: i915_gem_context.c i915_gem_vm_destroy_ioctl()
pub unsafe fn i915_gem_vm_destroy_ioctl(_dev: *mut c_void, data: *mut c_void, file: *mut DrmFile) -> c_int {
    let args = data.cast::<DrmI915GemVmControl>();
    let fpriv = unsafe { (*file).driver_priv.cast::<DrmI915FilePrivate>() };
    if unsafe { (*args).flags != 0 || (*args).extensions != 0 } {
        return -EINVAL;
    }
    let view = unsafe { file_private_view(fpriv) };
    let vm = unsafe { xa_erase(ptr::addr_of_mut!((*view).vm_xa), (*args).vm_id as c_ulong) };
    if vm.is_null() {
        return -ENOENT;
    }
    unsafe { i915_vm_put(vm.cast()) };
    0
}

// upstream: i915_gem_context.c get_ppgtt()
unsafe fn get_ppgtt(
    file_priv: *mut DrmI915FilePrivate,
    ctx: *mut I915GemContext,
    args: *mut DrmI915GemContextParam,
) -> c_int {
    if !unsafe { i915_gem_context_has_full_ppgtt(ctx) } {
        return -ENODEV;
    }
    let vm = unsafe { vm_pointer(ctx) };
    gem_bug_on!(vm.is_null());
    // Hold the extra reference before publishing the ID in vm_xa.
    unsafe { i915_vm_get(vm) };
    let view = unsafe { file_private_view(file_priv) };
    let mut id = 0u32;
    let err = unsafe {
        xa_alloc(
            ptr::addr_of_mut!((*view).vm_xa),
            &mut id,
            vm,
            XaLimit { min: XA_LIMIT_32B_MIN, max: XA_LIMIT_32B_MAX },
        )
    };
    if err != 0 {
        unsafe { i915_vm_put(vm) };
        return err;
    }
    gem_bug_on!(id == 0);
    unsafe {
        (*args).value = id as u64;
        (*args).size = 0;
    }
    err
}

// upstream: i915_gem_context.c i915_gem_user_to_context_sseu()
pub unsafe fn i915_gem_user_to_context_sseu(
    gt: *mut IntelGt,
    user: *const DrmI915GemContextParamSseu,
    context: *mut IntelSseu,
) -> c_int {
    let device = unsafe { ptr::addr_of!((*gt).info.sseu) };
    let i915 = unsafe { (*gt).i915 };
    let dev_subslice_mask = unsafe {
        crate::intel_sseu_types_upstream::intel_sseu_get_hsw_subslices(device, 0)
    };
    let user_slice = unsafe { (*user).slice_mask };
    let user_subslice = unsafe { (*user).subslice_mask };
    let user_min_eus = unsafe { (*user).min_eus_per_subslice };
    let user_max_eus = unsafe { (*user).max_eus_per_subslice };
    if user_slice == 0 || user_subslice == 0 || user_min_eus == 0 || user_max_eus == 0 {
        return -EINVAL;
    }
    if user_max_eus < user_min_eus {
        return -EINVAL;
    }
    // The UAPI fields are wider than the internal u8 representation.
    let (Ok(slice_mask), Ok(subslice_mask), Ok(min_eus), Ok(max_eus)) = (
        u8::try_from(user_slice),
        u8::try_from(user_subslice),
        u8::try_from(user_min_eus),
        u8::try_from(user_max_eus),
    ) else {
        return -EINVAL;
    };
    if slice_mask & !unsafe { (*device).slice_mask } != 0
        || (subslice_mask as u32) & !dev_subslice_mask != 0
        || max_eus > unsafe { (*device).max_eus_per_subslice }
    {
        return -EINVAL;
    }
    unsafe {
        (*context).slice_mask = slice_mask;
        (*context).subslice_mask = subslice_mask;
        (*context).min_eus_per_subslice = min_eus;
        (*context).max_eus_per_subslice = max_eus;
    }
    if unsafe { crate::linux::i915::GRAPHICS_VER(i915) } == 11 {
        let hw_s = crate::linux::primitives::hweight8(unsafe { (*device).slice_mask });
        let hw_ss_per_s = crate::linux::primitives::hweight8(dev_subslice_mask as u8);
        let req_s = crate::linux::primitives::hweight8(slice_mask);
        let req_ss = crate::linux::primitives::hweight8(subslice_mask);
        if req_s > 1 && req_ss != hw_ss_per_s {
            return -EINVAL;
        }
        if req_ss > 4 && req_ss & 1 != 0 {
            return -EINVAL;
        }
        if req_s == 1 && req_ss < hw_ss_per_s && req_ss > hw_ss_per_s / 2 {
            return -EINVAL;
        }
        if req_s != 1 && req_s != hw_s {
            return -EINVAL;
        }
        if req_s == 1 && req_ss != hw_ss_per_s && req_ss != hw_ss_per_s / 2 {
            return -EINVAL;
        }
        let device_max_eus = unsafe { (*device).max_eus_per_subslice as u16 };
        if user_min_eus != device_max_eus || user_max_eus != device_max_eus {
            return -EINVAL;
        }
    }
    0
}

// upstream: i915_gem_context.c set_sseu()
unsafe fn set_sseu(ctx: *mut I915GemContext, args: *mut DrmI915GemContextParam) -> c_int {
    if unsafe { (*args).size as usize } < size_of::<DrmI915GemContextParamSseu>() {
        return -EINVAL;
    }
    let i915 = unsafe { (*ctx).i915 };
    if unsafe { crate::linux::i915::GRAPHICS_VER(i915) } != 11 {
        return -ENODEV;
    }
    let mut user_sseu = DrmI915GemContextParamSseu {
        engine: I915EngineClassInstance { engine_class: 0, engine_instance: 0 },
        flags: 0,
        slice_mask: 0,
        subslice_mask: 0,
        min_eus_per_subslice: 0,
        max_eus_per_subslice: 0,
        rsvd: 0,
    };
    if unsafe { copy_from_user(&mut user_sseu, u64_to_user_ptr((*args).value), size_of::<DrmI915GemContextParamSseu>()) } != 0 {
        return -EFAULT;
    }
    if user_sseu.rsvd != 0 || user_sseu.flags & !I915_CONTEXT_SSEU_FLAG_ENGINE_INDEX != 0 {
        return -EINVAL;
    }
    let lookup = if user_sseu.flags & I915_CONTEXT_SSEU_FLAG_ENGINE_INDEX != 0 { LOOKUP_USER_INDEX } else { 0 };
    let ce = unsafe { lookup_user_engine(ctx, lookup, ptr::addr_of!(user_sseu.engine)) };
    if is_err_ptr(ce) {
        return ptr_err(ce);
    }
    let mut ret = 0;
    if unsafe { (*(*ce).engine).class as u32 } != RENDER_CLASS as u32 {
        ret = -ENODEV;
    } else {
        let mut sseu = IntelSseu::default();
        ret = unsafe { i915_gem_user_to_context_sseu((*(*ce).engine).gt, &user_sseu, &mut sseu) };
        if ret == 0 {
            ret = unsafe { intel_context_reconfigure_sseu(ce, &sseu) };
        }
        if ret == 0 {
            unsafe { (*args).size = size_of::<DrmI915GemContextParamSseu>() as u32 };
        }
    }
    unsafe { crate::intel_context_api_upstream::intel_context_put(ce) };
    ret
}

// upstream: i915_gem_context.c set_persistence()
unsafe fn set_persistence(ctx: *mut I915GemContext, args: *const DrmI915GemContextParam) -> c_int {
    if unsafe { (*args).size } != 0 {
        return -EINVAL;
    }
    unsafe { __context_set_persistence(ctx, (*args).value != 0) }
}

// upstream: i915_gem_context.c set_priority()
unsafe fn set_priority(ctx: *mut I915GemContext, args: *const DrmI915GemContextParam) -> c_int {
    let i915 = unsafe { (*ctx).i915 };
    let err = unsafe { validate_priority(i915, args) };
    if err != 0 {
        return err;
    }
    unsafe { (*ctx).sched.priority = (*args).value as i32 };
    let engines = unsafe { context_engines_lock(ctx) };
    let mut it = I915GemEnginesIter { idx: 0, engines };
    loop {
        let ce = unsafe { i915_gem_engines_iter_next(&mut it) };
        if ce.is_null() {
            break;
        }
        let engine = unsafe { (*ce).engine };
        if !unsafe { crate::intel_engine_types_upstream::intel_engine_has_timeslices(engine) } {
            continue;
        }
        if unsafe { (*ctx).sched.priority >= I915_PRIORITY_NORMAL }
            && unsafe { crate::intel_engine_types_upstream::intel_engine_has_semaphores(engine) }
        {
            unsafe { crate::intel_context_api_upstream::intel_context_set_use_semaphores(ce) };
        } else {
            unsafe { crate::intel_context_api_upstream::intel_context_clear_use_semaphores(ce) };
        }
    }
    unsafe { context_engines_unlock(ctx) };
    0
}

// upstream: i915_gem_context.c get_protected()
unsafe fn get_protected(ctx: *mut I915GemContext, args: *mut DrmI915GemContextParam) -> c_int {
    unsafe {
        (*args).size = 0;
        (*args).value = i915_gem_context_uses_protected_content(ctx) as u64;
    }
    0
}

// This target config sets CONFIG_DRM_I915_REPLAY_GPU_HANGS_API=n. Keep the
// opt-in debug ABI source-faithful but disabled for this configured build.
const CONFIG_DRM_I915_REPLAY_GPU_HANGS_API: bool = false;

// upstream: i915_gem_context.c set_context_image()
unsafe fn set_context_image(ctx: *mut I915GemContext, args: *mut DrmI915GemContextParam) -> c_int {
    if !CONFIG_DRM_I915_REPLAY_GPU_HANGS_API
        || !unsafe { (*(*ctx).i915).params.enable_debug_only_api }
    {
        return -EINVAL;
    }
    if unsafe { (*args).size as usize } < size_of::<I915GemContextParamContextImage>() {
        return -EINVAL;
    }
    let mut user = I915GemContextParamContextImage {
        engine: I915EngineClassInstance { engine_class: 0, engine_instance: 0 },
        flags: 0,
        size: 0,
        mbz: 0,
        image: 0,
    };
    if unsafe { copy_from_user(&mut user, u64_to_user_ptr((*args).value), size_of::<I915GemContextParamContextImage>()) } != 0 {
        return -EFAULT;
    }
    if user.mbz != 0 || user.flags & !I915_CONTEXT_IMAGE_FLAG_ENGINE_INDEX != 0 {
        return -EINVAL;
    }
    let lookup = if user.flags & I915_CONTEXT_IMAGE_FLAG_ENGINE_INDEX != 0 { LOOKUP_USER_INDEX } else { 0 };
    let ce = unsafe { lookup_user_engine(ctx, lookup, ptr::addr_of!(user.engine)) };
    if is_err_ptr(ce) {
        return ptr_err(ce);
    }
    let mut ret = 0;
    let context_size = unsafe { (*(*ce).engine).context_size as usize };
    if (user.size as usize) < context_size {
        ret = -EINVAL;
    } else if WARN_ON_ONCE!(test_bit(CONTEXT_ALLOC_BIT, unsafe { &(*ce).flags })) {
        ret = -EBUSY;
    } else {
        let state = unsafe { memdup_user(u64_to_user_ptr(user.image), context_size) };
        if is_err_ptr(state) {
            ret = ptr_err(state);
        } else {
            let shmem_state = unsafe {
                shmem_create_from_data((*(*ce).engine).name.as_ptr(), state, context_size)
            };
            if is_err_ptr(shmem_state) {
                ret = ptr_err(shmem_state);
            } else if crate::linux::bits::test_and_set_bit(
                crate::intel_context_types_upstream::CONTEXT_OWN_STATE,
                unsafe { &mut (*ce).flags },
            ) {
                ret = -EBUSY;
                unsafe { fput(shmem_state) };
            } else {
                unsafe {
                    (*ce).default_state = shmem_state.cast();
                    (*args).size = size_of::<I915GemContextParamContextImage>() as u32;
                }
            }
            unsafe { kfree(state) };
        }
    }
    unsafe { crate::intel_context_api_upstream::intel_context_put(ce) };
    ret
}

// upstream: i915_gem_context.c ctx_setparam()
unsafe fn ctx_setparam(
    _fpriv: *mut DrmI915FilePrivate,
    ctx: *mut I915GemContext,
    args: *mut DrmI915GemContextParam,
) -> c_int {
    let value = unsafe { (*args).value };
    match unsafe { (*args).param } {
        I915_CONTEXT_PARAM_NO_ERROR_CAPTURE => {
            if unsafe { (*args).size } != 0 {
                -EINVAL
            } else {
                unsafe { set_context_bit(ctx, UCONTEXT_NO_ERROR_CAPTURE, value != 0) };
                0
            }
        }
        I915_CONTEXT_PARAM_BANNABLE => {
            if unsafe { (*args).size } != 0 {
                -EINVAL
            } else if !unsafe { capable(CAP_SYS_ADMIN) } && value == 0 {
                -EPERM
            } else if value != 0 {
                unsafe { set_context_bit(ctx, UCONTEXT_BANNABLE, true) };
                0
            } else if unsafe { i915_gem_context_uses_protected_content(ctx) } {
                -EPERM
            } else {
                unsafe { set_context_bit(ctx, UCONTEXT_BANNABLE, false) };
                0
            }
        }
        I915_CONTEXT_PARAM_RECOVERABLE => {
            if unsafe { (*args).size } != 0 {
                -EINVAL
            } else if value == 0 {
                unsafe { set_context_bit(ctx, UCONTEXT_RECOVERABLE, false) };
                0
            } else if unsafe { i915_gem_context_uses_protected_content(ctx) } {
                -EPERM
            } else {
                unsafe { set_context_bit(ctx, UCONTEXT_RECOVERABLE, true) };
                0
            }
        }
        I915_CONTEXT_PARAM_PRIORITY => unsafe { set_priority(ctx, args) },
        I915_CONTEXT_PARAM_SSEU => unsafe { set_sseu(ctx, args) },
        I915_CONTEXT_PARAM_PERSISTENCE => unsafe { set_persistence(ctx, args) },
        I915_CONTEXT_PARAM_CONTEXT_IMAGE => unsafe { set_context_image(ctx, args) },
        I915_CONTEXT_PARAM_PROTECTED_CONTENT
        | I915_CONTEXT_PARAM_LOW_LATENCY
        | I915_CONTEXT_PARAM_NO_ZEROMAP
        | I915_CONTEXT_PARAM_BAN_PERIOD
        | I915_CONTEXT_PARAM_RINGSIZE
        | I915_CONTEXT_PARAM_VM
        | I915_CONTEXT_PARAM_ENGINES => -EINVAL,
        _ => -EINVAL,
    }
}

// upstream: i915_gem_context.c create_setparam()
unsafe extern "C" fn create_setparam(ext: *mut I915UserExtension, data: *mut c_void) -> c_int {
    let mut local = DrmI915GemContextCreateExtSetparam {
        base: I915UserExtension { next_extension: 0, name: 0, flags: 0, rsvd: [0; 4] },
        param: DrmI915GemContextParam { ctx_id: 0, size: 0, param: 0, value: 0 },
    };
    if unsafe { copy_from_user(&mut local, ext.cast(), size_of::<DrmI915GemContextCreateExtSetparam>()) } != 0 {
        return -EFAULT;
    }
    if local.param.ctx_id != 0 {
        return -EINVAL;
    }
    let create = data.cast::<I915GemContextCreateExtData>();
    unsafe { set_proto_ctx_param((*create).fpriv, (*create).pc, ptr::addr_of_mut!(local.param)) }
}

// upstream: i915_gem_context.c invalid_ext()
unsafe extern "C" fn invalid_ext(_ext: *mut I915UserExtension, _data: *mut c_void) -> c_int {
    -EINVAL
}

static CREATE_EXTENSIONS: [I915UserExtensionFn; 2] =
    [Some(create_setparam), Some(invalid_ext)];

// upstream: i915_gem_context.c client_is_banned()
unsafe fn client_is_banned(file_priv: *mut DrmI915FilePrivate) -> bool {
    let view = unsafe { file_private_view(file_priv) };
    crate::linux_memory::atomic_read(unsafe { &(*view).ban_score }) >= I915_CLIENT_SCORE_BANNED
}

// upstream: i915_gem_context.c __context_lookup()
unsafe fn __context_lookup(file_priv: *mut DrmI915FilePrivate, id: u32) -> *mut I915GemContext {
    let view = unsafe { file_private_view(file_priv) };
    unsafe { rcu_read_lock() };
    let mut ctx = unsafe {
        crate::linux::xarray::xa_load::<_, I915GemContext>(&mut (*view).context_xa, id)
    };
    if !ctx.is_null() && !crate::linux_memory::kref_get_unless_zero(unsafe { &mut (*ctx).r#ref }) {
        ctx = ptr::null_mut();
    }
    unsafe { rcu_read_unlock() };
    ctx
}

// upstream: i915_gem_context.c finalize_create_context_locked()
unsafe fn finalize_create_context_locked(
    file_priv: *mut DrmI915FilePrivate,
    pc: *mut I915GemProtoContext,
    id: u32,
) -> *mut I915GemContext {
    let view = unsafe { file_private_view(file_priv) };
    lockdep_assert_held!(unsafe { &(*view).proto_context_lock });
    let ctx = unsafe { i915_gem_create_context((*view).i915, pc) };
    if is_err_ptr(ctx) {
        return ctx;
    }
    unsafe { i915_gem_context_get(ctx) };
    unsafe { gem_context_register(ctx, file_priv, id) };
    let old = unsafe { xa_erase(ptr::addr_of_mut!((*view).proto_context_xa), id as c_ulong) };
    gem_bug_on!(old != pc.cast());
    unsafe { proto_context_close((*view).i915, pc) };
    ctx
}

// upstream: i915_gem_context.c i915_gem_context_lookup()
pub unsafe fn i915_gem_context_lookup(file_priv: *mut DrmI915FilePrivate, id: u32) -> *mut I915GemContext {
    let view = unsafe { file_private_view(file_priv) };
    let mut ctx = unsafe { __context_lookup(file_priv, id) };
    if !ctx.is_null() {
        return ctx;
    }
    unsafe { mutex_lock(ptr::addr_of_mut!((*view).proto_context_lock)) };
    ctx = unsafe { __context_lookup(file_priv, id) };
    if ctx.is_null() {
        let pc = unsafe {
            crate::linux::xarray::xa_load::<_, I915GemProtoContext>(&mut (*view).proto_context_xa, id)
        };
        if pc.is_null() {
            ctx = ERR_PTR(-ENOENT);
        } else {
            ctx = unsafe { finalize_create_context_locked(file_priv, pc, id) };
        }
    }
    unsafe { mutex_unlock(ptr::addr_of_mut!((*view).proto_context_lock)) };
    ctx
}

// upstream: i915_gem_context.c i915_gem_context_create_ioctl()
pub unsafe fn i915_gem_context_create_ioctl(
    dev: *mut crate::linux::gem::DrmDevice,
    data: *mut c_void,
    file: *mut DrmFile,
) -> c_int {
    let i915 = unsafe { crate::linux::i915::to_i915(dev.cast()) };
    let args = data.cast::<DrmI915GemContextCreateExt>();
    if !unsafe { has_logical_contexts(i915) } {
        return -ENODEV;
    }
    if unsafe { (*args).flags & I915_CONTEXT_CREATE_FLAGS_UNKNOWN != 0 } {
        return -EINVAL;
    }
    let ret = unsafe { intel_gt_terminally_wedged(to_gt(i915)) };
    if ret != 0 {
        return ret;
    }
    let fpriv = unsafe { (*file).driver_priv.cast::<DrmI915FilePrivate>() };
    if unsafe { client_is_banned(fpriv) } {
        // Upstream drm_dbg also reads current->comm; the task_struct member
        // owner is not modeled here, so no guessed offset/accessor is used.
        return -EIO;
    }
    let mut ext_data = I915GemContextCreateExtData {
        pc: unsafe { proto_context_create(fpriv, i915, (*args).flags) },
        fpriv,
    };
    if is_err_ptr(ext_data.pc) {
        return ptr_err(ext_data.pc);
    }
    if unsafe { (*args).flags & I915_CONTEXT_CREATE_FLAGS_USE_EXTENSIONS != 0 } {
        let ret = unsafe {
            i915_user_extensions(
                u64_to_user_ptr((*args).extensions).cast(),
                CREATE_EXTENSIONS.as_ptr(),
                CREATE_EXTENSIONS.len() as u32,
                ptr::addr_of_mut!(ext_data).cast(),
            )
        };
        if ret != 0 {
            unsafe { proto_context_close(i915, ext_data.pc) };
            return ret;
        }
    }
    let mut id = 0u32;
    if unsafe { crate::linux::i915::GRAPHICS_VER(i915) } > 12 {
        let view = unsafe { file_private_view(fpriv) };
        let ret = unsafe {
            xa_alloc(
                ptr::addr_of_mut!((*view).context_xa),
                &mut id,
                ptr::null_mut::<I915GemContext>(),
                XaLimit { min: XA_LIMIT_32B_MIN, max: XA_LIMIT_32B_MAX },
            )
        };
        if ret != 0 {
            unsafe { proto_context_close(i915, ext_data.pc) };
            return ret;
        }
        let ctx = unsafe { i915_gem_create_context(i915, ext_data.pc) };
        if is_err_ptr(ctx) {
            let err = ptr_err(ctx);
            unsafe { proto_context_close(i915, ext_data.pc) };
            return err;
        }
        unsafe {
            proto_context_close(i915, ext_data.pc);
            gem_context_register(ctx, fpriv, id);
        }
    } else {
        let ret = unsafe { proto_context_register(fpriv, ext_data.pc, &mut id) };
        if ret < 0 {
            unsafe { proto_context_close(i915, ext_data.pc) };
            return ret;
        }
    }
    unsafe { (*args).ctx_id = id };
    0
}

// upstream: i915_gem_context.c i915_gem_context_destroy_ioctl()
pub unsafe fn i915_gem_context_destroy_ioctl(
    _dev: *mut crate::linux::gem::DrmDevice,
    data: *mut c_void,
    file: *mut DrmFile,
) -> c_int {
    let args = data.cast::<DrmI915GemContextDestroy>();
    let fpriv = unsafe { (*file).driver_priv.cast::<DrmI915FilePrivate>() };
    let view = unsafe { file_private_view(fpriv) };
    if unsafe { (*args).pad != 0 } {
        return -EINVAL;
    }
    let id = unsafe { (*args).ctx_id };
    if id == 0 {
        return -ENOENT;
    }
    unsafe { mutex_lock(ptr::addr_of_mut!((*view).proto_context_lock)) };
    let ctx = unsafe { xa_erase(ptr::addr_of_mut!((*view).context_xa), id as c_ulong) }
        .cast::<I915GemContext>();
    let pc = unsafe { xa_erase(ptr::addr_of_mut!((*view).proto_context_xa), id as c_ulong) }
        .cast::<I915GemProtoContext>();
    unsafe { mutex_unlock(ptr::addr_of_mut!((*view).proto_context_lock)) };
    if ctx.is_null() && pc.is_null() {
        return -ENOENT;
    }
    GEM_WARN_ON!(!ctx.is_null() && !pc.is_null());
    if !pc.is_null() {
        unsafe { proto_context_close((*view).i915, pc) };
    }
    if !ctx.is_null() {
        unsafe { context_close(ctx) };
    }
    0
}

// upstream: i915_gem_context.c get_sseu()
unsafe fn get_sseu(ctx: *mut I915GemContext, args: *mut DrmI915GemContextParam) -> c_int {
    if unsafe { (*args).size } == 0 {
        unsafe { (*args).size = size_of::<DrmI915GemContextParamSseu>() as u32 };
        return 0;
    }
    if unsafe { (*args).size as usize } < size_of::<DrmI915GemContextParamSseu>() {
        return -EINVAL;
    }
    let mut user_sseu = DrmI915GemContextParamSseu {
        engine: I915EngineClassInstance { engine_class: 0, engine_instance: 0 },
        flags: 0,
        slice_mask: 0,
        subslice_mask: 0,
        min_eus_per_subslice: 0,
        max_eus_per_subslice: 0,
        rsvd: 0,
    };
    if unsafe { copy_from_user(&mut user_sseu, u64_to_user_ptr((*args).value), size_of::<DrmI915GemContextParamSseu>()) } != 0 {
        return -EFAULT;
    }
    if user_sseu.rsvd != 0 || user_sseu.flags & !I915_CONTEXT_SSEU_FLAG_ENGINE_INDEX != 0 {
        return -EINVAL;
    }
    let lookup = if user_sseu.flags & I915_CONTEXT_SSEU_FLAG_ENGINE_INDEX != 0 { LOOKUP_USER_INDEX } else { 0 };
    let ce = unsafe { lookup_user_engine(ctx, lookup, ptr::addr_of!(user_sseu.engine)) };
    if is_err_ptr(ce) {
        return ptr_err(ce);
    }
    let err = unsafe { crate::intel_context_api_upstream::intel_context_lock_pinned(ce) };
    if err != 0 {
        unsafe { crate::intel_context_api_upstream::intel_context_put(ce) };
        return err;
    }
    unsafe {
        user_sseu.slice_mask = (*ce).sseu.slice_mask as u64;
        user_sseu.subslice_mask = (*ce).sseu.subslice_mask as u64;
        user_sseu.min_eus_per_subslice = (*ce).sseu.min_eus_per_subslice as u16;
        user_sseu.max_eus_per_subslice = (*ce).sseu.max_eus_per_subslice as u16;
        crate::intel_context_api_upstream::intel_context_unlock_pinned(ce);
        crate::intel_context_api_upstream::intel_context_put(ce);
    }
    if unsafe { copy_to_user(u64_to_user_ptr((*args).value), &user_sseu, size_of::<DrmI915GemContextParamSseu>()) } != 0 {
        return -EFAULT;
    }
    unsafe { (*args).size = size_of::<DrmI915GemContextParamSseu>() as u32 };
    0
}

// upstream: i915_gem_context.c i915_gem_context_getparam_ioctl()
pub unsafe fn i915_gem_context_getparam_ioctl(
    _dev: *mut crate::linux::gem::DrmDevice,
    data: *mut c_void,
    file: *mut DrmFile,
) -> c_int {
    let fpriv = unsafe { (*file).driver_priv.cast::<DrmI915FilePrivate>() };
    let args = data.cast::<DrmI915GemContextParam>();
    let ctx = unsafe { i915_gem_context_lookup(fpriv, (*args).ctx_id) };
    if is_err_ptr(ctx) {
        return ptr_err(ctx);
    }
    let mut ret = 0;
    match unsafe { (*args).param } {
        I915_CONTEXT_PARAM_GTT_SIZE => {
            unsafe { (*args).size = 0 };
            let vm = unsafe { i915_gem_context_get_eb_vm(ctx) };
            unsafe {
                (*args).value = (*vm).total;
                i915_vm_put(vm);
            }
        }
        I915_CONTEXT_PARAM_NO_ERROR_CAPTURE => unsafe {
            (*args).size = 0;
            (*args).value = i915_gem_context_no_error_capture(ctx) as u64;
        },
        I915_CONTEXT_PARAM_BANNABLE => unsafe {
            (*args).size = 0;
            (*args).value = i915_gem_context_is_bannable(ctx) as u64;
        },
        I915_CONTEXT_PARAM_RECOVERABLE => unsafe {
            (*args).size = 0;
            (*args).value = i915_gem_context_is_recoverable(ctx) as u64;
        },
        I915_CONTEXT_PARAM_PRIORITY => unsafe {
            (*args).size = 0;
            (*args).value = (*ctx).sched.priority as u64;
        },
        I915_CONTEXT_PARAM_SSEU => ret = unsafe { get_sseu(ctx, args) },
        I915_CONTEXT_PARAM_VM => ret = unsafe { get_ppgtt(fpriv, ctx, args) },
        I915_CONTEXT_PARAM_PERSISTENCE => unsafe {
            (*args).size = 0;
            (*args).value = i915_gem_context_is_persistent(ctx) as u64;
        },
        I915_CONTEXT_PARAM_PROTECTED_CONTENT => ret = unsafe { get_protected(ctx, args) },
        I915_CONTEXT_PARAM_NO_ZEROMAP
        | I915_CONTEXT_PARAM_BAN_PERIOD
        | I915_CONTEXT_PARAM_ENGINES
        | I915_CONTEXT_PARAM_RINGSIZE
        | I915_CONTEXT_PARAM_CONTEXT_IMAGE
        | I915_CONTEXT_PARAM_LOW_LATENCY
        | _ => ret = -EINVAL,
    }
    unsafe { i915_gem_context_put(ctx) };
    ret
}

// upstream: i915_gem_context.c i915_gem_context_setparam_ioctl()
pub unsafe fn i915_gem_context_setparam_ioctl(
    _dev: *mut crate::linux::gem::DrmDevice,
    data: *mut c_void,
    file: *mut DrmFile,
) -> c_int {
    let fpriv = unsafe { (*file).driver_priv.cast::<DrmI915FilePrivate>() };
    let view = unsafe { file_private_view(fpriv) };
    let args = data.cast::<DrmI915GemContextParam>();
    unsafe { mutex_lock(ptr::addr_of_mut!((*view).proto_context_lock)) };
    let ctx = unsafe { __context_lookup(fpriv, (*args).ctx_id) };
    let mut ret = 0;
    if ctx.is_null() {
        let pc = unsafe {
            crate::linux::xarray::xa_load::<_, I915GemProtoContext>(
                &mut (*view).proto_context_xa,
                (*args).ctx_id,
            )
        };
        if pc.is_null() {
            ret = -ENOENT;
        } else {
            WARN_ON!(unsafe { crate::linux::i915::GRAPHICS_VER((*view).i915) > 12 });
            ret = unsafe { set_proto_ctx_param(fpriv, pc, args) };
        }
    }
    unsafe { mutex_unlock(ptr::addr_of_mut!((*view).proto_context_lock)) };
    if !ctx.is_null() {
        ret = unsafe { ctx_setparam(fpriv, ctx, args) };
        unsafe { i915_gem_context_put(ctx) };
    }
    ret
}

// upstream: i915_gem_context.c i915_gem_context_reset_stats_ioctl()
pub unsafe fn i915_gem_context_reset_stats_ioctl(
    dev: *mut crate::linux::gem::DrmDevice,
    data: *mut c_void,
    file: *mut DrmFile,
) -> c_int {
    let i915 = unsafe { crate::linux::i915::to_i915(dev.cast()) };
    let args = data.cast::<DrmI915ResetStats>();
    if unsafe { (*args).flags != 0 || (*args).pad != 0 } {
        return -EINVAL;
    }
    let fpriv = unsafe { (*file).driver_priv.cast::<DrmI915FilePrivate>() };
    let ctx = unsafe { i915_gem_context_lookup(fpriv, (*args).ctx_id) };
    if is_err_ptr(ctx) {
        return ptr_err(ctx);
    }
    unsafe {
        (*args).reset_count = if capable(CAP_SYS_ADMIN) {
            i915_reset_count(ptr::addr_of!((*i915).gpu_error))
        } else {
            0
        };
        (*args).batch_active = crate::linux_memory::atomic_read(&(*ctx).guilty_count) as u32;
        (*args).batch_pending = crate::linux_memory::atomic_read(&(*ctx).active_count) as u32;
        i915_gem_context_put(ctx);
    }
    0
}

// upstream: i915_gem_context.c i915_gem_engines_iter_next()
pub unsafe fn i915_gem_engines_iter_next(it: *mut I915GemEnginesIter) -> *mut IntelContext {
    let engines = unsafe { (*it).engines };
    if engines.is_null() {
        return ptr::null_mut();
    }
    loop {
        if unsafe { (*it).idx >= (*engines).num_engines } {
            return ptr::null_mut();
        }
        let idx = unsafe { (*it).idx };
        unsafe { (*it).idx = idx + 1 };
        let ce = unsafe { *(*engines).engines.as_ptr().add(idx as usize) };
        if !ce.is_null() {
            return ce;
        }
    }
}

// upstream: i915_gem_context.c i915_gem_context_module_exit()
pub unsafe fn i915_gem_context_module_exit() {
    unsafe { kmem_cache_destroy(SLAB_LUTS) };
}

// upstream: i915_gem_context.c i915_gem_context_module_init()
pub unsafe fn i915_gem_context_module_init() -> c_int {
    SLAB_LUTS = unsafe {
        kmem_cache_create(
            b"i915_lut_handle\0".as_ptr().cast(),
            size_of::<I915LutHandle>(),
            core::mem::align_of::<I915LutHandle>(),
            0,
            None,
        )
    };
    if SLAB_LUTS.is_null() {
        return -ENOMEM;
    }
    // The configured target has CONFIG_DRM_I915_REPLAY_GPU_HANGS_API=n, so
    // upstream's opt-in debug-only warning banner is not emitted.
    0
}
