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
    intel_context_types_upstream::{CONTEXT_BANNED, I915SwFence, IntelContext, IntelWakerefT},
    intel_context_api_upstream::intel_context_get,
    intel_context_upstream::{
        DmaFence, DrmMmNode, I915ActiveFence, I915AddressSpace as OldI915AddressSpace, Kref,
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
        EFAULT, EINVAL, EIO, ENODEV, ENOENT, ENOMEM, GFP_KERNEL, HZ, I915_CONTEXT_DEFAULT_PRIORITY,
        ERR_PTR,
    },
    linux_i915_private::DrmI915Private,
};

const EPERM: i32 = 1;
const EBADF: i32 = 9;
const EEXIST: i32 = 17;
const I915_EXEC_RING_MASK: u32 = 0x3f;

/// Source `i915_gem_context_release()`: final reference schedules deferred
/// release on the device workqueue; it must not free the context inline.
unsafe extern "C" fn i915_gem_context_release(ref_: *mut Kref) {
    let ctx = unsafe {
        ref_.cast::<u8>()
            .sub(offset_of!(I915GemContext, r#ref))
            .cast::<I915GemContext>()
    };
    let i915 = unsafe { (*ctx).i915 };
    let wq = unsafe { (*i915).wq };
    unsafe { queue_work(wq, &mut (*ctx).release_work) };
}
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
const I915_CONTEXT_PARAM_PROTECTED_CONTENT: u64 = 0xe;
const I915_CONTEXT_PARAM_CONTEXT_IMAGE: u64 = 0xf;
const I915_CONTEXT_ENGINES_EXT_LOAD_BALANCE: u32 = 0;
const I915_CONTEXT_ENGINES_EXT_BOND: u32 = 1;
const I915_CONTEXT_ENGINES_EXT_PARALLEL_SUBMIT: u32 = 2;
const I915_CONTEXT_SSEU_FLAG_ENGINE_INDEX: u32 = 1 << 0;
const I915_CONTEXT_IMAGE_FLAG_ENGINE_INDEX: u32 = 1 << 0;
const I915_ENGINE_CLASS_INVALID_NONE: u16 = u16::MAX;
const I915_PRIORITY_NORMAL: i32 = 0;
const I915_ACTIVE_AWAIT_BARRIER: u32 = 1;
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

#[repr(C)]
#[derive(Clone, Copy)]
struct XaLimit {
    min: u32,
    max: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RadixTreeIter {
    index: c_ulong,
    next_index: c_ulong,
    tags: c_ulong,
    node: *mut c_void,
}

const _: [(); 4] = [(); size_of::<I915EngineClassInstance>()];
const _: [(); 32] = [(); size_of::<I915UserExtension>()];
const _: [(); 24] = [(); size_of::<DrmI915GemContextParam>()];
const _: [(); 32] = [(); size_of::<DrmI915GemContextParamSseu>()];
const _: [(); 24] = [(); size_of::<DrmI915GemContextCreateExt>()];
const _: [(); 24] = [(); size_of::<I915GemContextParamContextImage>()];

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
        class: u16,
        instance: u16,
    ) -> *mut IntelEngineCs;
    fn intel_has_reset_engine(gt: *const IntelGt) -> bool;
    fn intel_pxp_is_enabled(pxp: *const c_void) -> bool;
    fn intel_pxp_is_active(pxp: *const c_void) -> bool;
    fn intel_pxp_start(pxp: *mut c_void) -> c_int;
    fn intel_context_reconfigure_sseu(ce: *mut IntelContext, sseu: *const IntelSseu) -> c_int;
    fn intel_engine_pulse(engine: *mut IntelEngineCs) -> c_int;
    fn intel_gt_handle_error(
        gt: *mut IntelGt,
        mask: IntelEngineMask,
        flags: u32,
        fmt: *const c_char,
        ...
    );
    fn i915_request_get_rcu(rq: *mut I915Request) -> bool;
    fn i915_request_put(rq: *mut I915Request);
    fn i915_request_active_engine(rq: *mut I915Request, engine: *mut *mut IntelEngineCs) -> bool;
    fn i915_gem_object_get(obj: *mut DrmI915GemObject);
    fn i915_vma_close(vma: *mut I915Vma);
    fn drm_syncobj_create(syncobj: *mut *mut c_void, flags: u32, fence: *mut DmaFence) -> c_int;
    fn drm_syncobj_put(syncobj: *mut c_void);
    fn i915_drm_client_get(client: *mut I915DrmClient) -> *mut I915DrmClient;
    fn i915_drm_client_put(client: *mut I915DrmClient);
    fn i915_user_extensions(
        extensions: *mut I915UserExtension,
        funcs: *const Option<unsafe fn(*mut I915UserExtension, *mut c_void) -> c_int>,
        count: usize,
        data: *mut c_void,
    ) -> c_int;
    fn i915_gem_context_is_banned(i915: *mut DrmI915Private) -> bool;
    fn intel_gt_terminally_wedged(gt: *mut IntelGt) -> c_int;
    fn i915_reset_count(error: *const crate::linux_i915_private::I915GpuError) -> u32;
    fn radix_tree_next_chunk(
        root: *const XArray,
        iter: *mut RadixTreeIter,
        flags: u32,
    ) -> *mut *mut c_void;
    fn radix_tree_iter_delete(root: *mut XArray, iter: *mut RadixTreeIter, slot: *mut *mut c_void);
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
    fn i915_gem_context_open(i915: *mut DrmI915Private, file: *mut DrmFile) -> c_int;
}

unsafe fn file_private_view(file_priv: *mut DrmI915FilePrivate) -> *mut I915FilePrivateView {
    file_priv.cast()
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
    let mut next = limit.min;
    unsafe {
        xa_alloc_cyclic_irq(
            &mut *xa,
            &mut *id,
            value,
            crate::linux::xarray::XaLimit {
                min: limit.min,
                max: limit.max,
            },
            &mut next,
            GFP_KERNEL,
        )
    }
}

unsafe fn radix_tree_iter_init(iter: *mut RadixTreeIter, start: c_ulong) -> *mut *mut c_void {
    unsafe {
        (*iter).index = 0;
        (*iter).next_index = start;
    }
    ptr::null_mut()
}

unsafe fn radix_tree_next_slot(
    mut slot: *mut *mut c_void,
    iter: *mut RadixTreeIter,
) -> *mut *mut c_void {
    let mut count = unsafe { (*iter).next_index.wrapping_sub((*iter).index) };
    while count > 1 {
        slot = unsafe { slot.add(1) };
        unsafe { (*iter).index = (*iter).index.wrapping_add(1) };
        count -= 1;
        if !unsafe { ptr::read_volatile(slot) }.is_null() {
            return slot;
        }
    }
    ptr::null_mut()
}

unsafe fn radix_tree_for_each_slot_start(iter: *mut RadixTreeIter) {
    unsafe { radix_tree_iter_init(iter, 0) };
}

unsafe fn current_task_ptr() -> *mut c_void {
    axhal::percpu::current_task_ptr::<()>().cast()
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

unsafe fn i915_gem_context_no_error_capture(ctx: *const I915GemContext) -> bool {
    test_bit(UCONTEXT_NO_ERROR_CAPTURE, unsafe { &(*ctx).user_flags })
}
unsafe fn i915_gem_context_is_bannable(ctx: *const I915GemContext) -> bool {
    test_bit(UCONTEXT_BANNABLE, unsafe { &(*ctx).user_flags })
}
unsafe fn i915_gem_context_is_recoverable(ctx: *const I915GemContext) -> bool {
    test_bit(UCONTEXT_RECOVERABLE, unsafe { &(*ctx).user_flags })
}
unsafe fn i915_gem_context_is_persistent(ctx: *const I915GemContext) -> bool {
    test_bit(UCONTEXT_PERSISTENCE, unsafe { &(*ctx).user_flags })
}
unsafe fn i915_gem_context_user_engines(ctx: *const I915GemContext) -> bool {
    test_bit(CONTEXT_USER_ENGINES, unsafe { &(*ctx).flags })
}
unsafe fn i915_gem_context_is_closed(ctx: *const I915GemContext) -> bool {
    test_bit(CONTEXT_CLOSED, unsafe { &(*ctx).flags })
}
unsafe fn i915_gem_context_uses_protected_content(ctx: *const I915GemContext) -> bool {
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

unsafe fn i915_gem_context_has_full_ppgtt(ctx: *mut I915GemContext) -> bool {
    !unsafe { vm_pointer(ctx) }.is_null()
}

unsafe fn i915_gem_context_set_closed(ctx: *mut I915GemContext) {
    GEM_BUG_ON!(unsafe { i915_gem_context_is_closed(ctx) });
    set_bit(CONTEXT_CLOSED, unsafe { &mut (*ctx).flags });
}

unsafe fn i915_gem_context_get_engine(ctx: *mut I915GemContext, index: u32) -> *mut IntelContext {
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
