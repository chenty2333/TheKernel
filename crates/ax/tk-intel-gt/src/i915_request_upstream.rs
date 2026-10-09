// SPDX-License-Identifier: MIT
// Copyright © 2008-2015 Intel Corporation.
// Source-order implementation translation from Linux v7.2.3
// drivers/gpu/drm/i915/i915_request.c. The upstream source carries the
// standard MIT grant; this file is an additive Rust implementation binding.

#![allow(non_snake_case, non_camel_case_types, unsafe_op_in_unsafe_fn)]

use core::{
    ffi::{c_char, c_int, c_long, c_ulong, c_void},
    mem::{offset_of, size_of},
    ptr,
    sync::atomic::{AtomicPtr, Ordering},
};

use crate::{
    i915_gem_context_types_upstream::I915GemContext,
    i915_request_types_upstream::{
        i915_request_timeline, DrmPrinter, I915CaptureList, I915Deps, I915Request,
        I915RequestState, I915RequestWatchdog,
    },
    i915_scheduler_types_upstream::{
        I915Dependency, I915SchedAttr, I915SchedNode, I915_SCHED_HAS_EXTERNAL_CHAIN,
    },
    i915_scheduler_upstream::*,
    i915_sw_fence_upstream::*,
    i915_gem_object_types_upstream::DrmI915GemObject,
    intel_context_types_upstream::{I915SwFence, I915SwFenceNotify, IntelContext},
    intel_context_upstream::{
        DmaFence, DmaFenceCb, DrmGemObjectBaseLayout, I915Vma, I915VmaResource, IntelRing,
        IrqWork, WaitQueueEntry,
    },
    intel_context_upstream::*,
    intel_context_api_upstream::{
        intel_context_cancel_request, intel_context_enter, intel_context_exit,
        intel_context_is_barrier, intel_context_is_parallel, intel_context_is_schedulable,
        intel_context_mark_active, intel_context_pin, intel_context_timeline_lock,
        intel_context_timeline_unlock, intel_context_to_parent, intel_context_unpin,
        intel_context_use_semaphores,
    },
    i915_active_upstream::*,
    intel_ring_upstream::*,
    intel_timeline_upstream::*,
    intel_timeline_types_upstream::IntelTimeline,
    intel_engine_api_upstream::{__intel_engine_flush_submission, intel_engine_get_sibling},
    intel_engine_cs_upstream::{AtomicT, IntelEngineCs, LlistNode, Spinlock},
    intel_engine_types_upstream::{
        IntelEngineMask, VIDEO_DECODE_CLASS, intel_engine_has_semaphores, intel_engine_is_virtual,
    },
    intel_gt_types_upstream::IntelGt,
    intel_timeline_types_upstream::I915Syncmap,
    intel_timeline_upstream::intel_timeline_get_seqno,
    intel_huc_types_upstream::intel_huc_wait_required,
    linux::{
        bits::*, contexts::*, fields::*, irq::*, list::*, locks::*, memory::*, mutex::*, pm::*,
        rbtree::*, rcu::*, registers::*, requests::*, tasklet::*, timer::*, wait::*,
        workqueue::*,
    },
    linux_config::*,
    linux_heap::*,
    linux_i915_private::DrmI915Private,
};

#[repr(C)]
struct ExecuteCb {
    work: IrqWork,
    fence: *mut I915SwFence,
}

/// Linux `struct dma_fence_ops`, needed for the per-request fence vtable.
#[repr(C)]
pub struct DmaFenceOps {
    pub get_driver_name: Option<unsafe extern "C" fn(*mut DmaFence) -> *const c_char>,
    pub get_timeline_name: Option<unsafe extern "C" fn(*mut DmaFence) -> *const c_char>,
    pub enable_signaling: Option<unsafe extern "C" fn(*mut DmaFence) -> bool>,
    pub signaled: Option<unsafe extern "C" fn(*mut DmaFence) -> bool>,
    pub wait: Option<unsafe extern "C" fn(*mut DmaFence, bool, c_long) -> c_long>,
    pub release: Option<unsafe extern "C" fn(*mut DmaFence)>,
    pub set_deadline: Option<unsafe extern "C" fn(*mut DmaFence, i64)>,
}

static mut SLAB_REQUESTS: *mut KmCache = ptr::null_mut();
static mut SLAB_EXECUTE_CBS: *mut KmCache = ptr::null_mut();

const DRIVER_NAME_BYTES: &[u8] = b"[ i915 ]\0";
const I915_WAIT_PRIORITY: u32 = 1 << 1;
const I915_WAIT_INTERRUPTIBLE: u32 = 1 << 0;
const I915_FENCE_GFP: u32 = GFP_ATOMIC;
const NOTIFY_DONE: i32 = 0;
const ERESTARTSYS: i32 = 512;
const MAX_ERRNO: i32 = 4095;
const POISON_FREE: u8 = 0x6b;
// Linux 7.2.3 include/linux/dma-fence.h enum dma_fence_flag_bits.
const DMA_FENCE_FLAG_ENABLE_SIGNAL_BIT: usize = 5;

#[inline]
unsafe fn to_request(fence: *mut DmaFence) -> *mut I915Request {
    fence.cast()
}

#[inline]
fn atomic_cmpxchg(value: &mut i32, old: i32, new: i32) -> i32 {
    let atom = unsafe { &*ptr::addr_of_mut!(*value).cast::<core::sync::atomic::AtomicI32>() };
    atom.compare_exchange(old, new, Ordering::SeqCst, Ordering::SeqCst)
        .unwrap_or_else(|observed| observed)
}

#[inline]
unsafe fn i915_request_gem_context(rq: *mut I915Request) -> *mut I915GemContext {
    unsafe { (*(*rq).context).gem_context }
}

unsafe fn i915_device_name(i915: *mut DrmI915Private) -> *const c_char {
    // drm_i915_private begins with drm_device; drm_device.dev is at byte 8
    // on the configured x86_64 ABI. device.init_name is at byte 80, with the
    // kobject name pointer at byte 0 as the fallback used by dev_name().
    let drm = i915.cast::<u8>();
    let dev = unsafe { ptr::read_unaligned(drm.add(8).cast::<*mut u8>()) };
    if dev.is_null() {
        return DRIVER_NAME_BYTES.as_ptr().cast();
    }
    let init_name = unsafe { ptr::read_unaligned(dev.add(80).cast::<*const c_char>()) };
    if !init_name.is_null() {
        init_name
    } else {
        unsafe { ptr::read_unaligned(dev.cast::<*const c_char>()) }
    }
}

// upstream i915_request.c:62
// upstream: i915_request.c i915_fence_get_driver_name()
unsafe extern "C" fn i915_fence_get_driver_name(fence: *mut DmaFence) -> *const c_char {
    unsafe { i915_device_name((*to_request(fence)).i915) }
}

// upstream i915_request.c:67
// upstream: i915_request.c i915_fence_get_timeline_name()
unsafe extern "C" fn i915_fence_get_timeline_name(fence: *mut DmaFence) -> *const c_char {
    if test_bit(DMA_FENCE_FLAG_SIGNALED_BIT, unsafe { &(*fence).flags }) {
        return b"signaled\0".as_ptr().cast();
    }
    let ctx = unsafe { i915_request_gem_context(to_request(fence)) };
    if ctx.is_null() {
        DRIVER_NAME_BYTES.as_ptr().cast()
    } else {
        unsafe { (*ctx).name.as_ptr() }
    }
}

// upstream i915_request.c:90
// upstream: i915_request.c i915_fence_signaled()
unsafe extern "C" fn i915_fence_signaled(fence: *mut DmaFence) -> bool {
    i915_request_completed(unsafe { &*to_request(fence) })
}

// upstream i915_request.c:95
// upstream: i915_request.c i915_fence_enable_signaling()
unsafe extern "C" fn i915_fence_enable_signaling(fence: *mut DmaFence) -> bool {
    unsafe { i915_request_enable_breadcrumb(to_request(fence)) }
}

// upstream i915_request.c:100
// upstream: i915_request.c i915_fence_wait()
unsafe extern "C" fn i915_fence_wait(
    fence: *mut DmaFence,
    interruptible: bool,
    timeout: c_long,
) -> c_long {
    unsafe {
        i915_request_wait_timeout(
            to_request(fence),
            u32::from(interruptible) | I915_WAIT_PRIORITY,
            timeout,
        )
    }
}

// upstream i915_request.c:109
// upstream: i915_request.c i915_request_slab_cache()
pub unsafe fn i915_request_slab_cache() -> *mut KmCache {
    unsafe { SLAB_REQUESTS }
}

// Source out-of-line Linux services that are not implemented by this file.
unsafe extern "C" {
    fn intel_rps_dec_waiters(rps: *mut crate::intel_rps_types_upstream::IntelRps);
    fn trace_i915_request_retire(rq: *mut I915Request);
    fn trace_i915_request_submit(rq: *mut I915Request);
    fn trace_i915_request_execute(rq: *mut I915Request);
    fn dma_fence_signal_locked(fence: *mut DmaFence);
    fn dma_fence_init(
        fence: *mut DmaFence,
        ops: *const DmaFenceOps,
        lock: *mut Spinlock,
        context: u64,
        seqno: u64,
    );
    fn i915_syncmap_set(root: *mut *mut I915Syncmap, id: u64, seqno: u32) -> i32;
    fn i915_syncmap_is_later(root: *mut *mut I915Syncmap, id: u64, seqno: u32) -> bool;
    fn i915_request_enable_breadcrumb(rq: *mut I915Request) -> bool;
    fn i915_request_cancel_breadcrumb(rq: *mut I915Request);
}

/// `i915_vma_resource_put()` is a static inline in the upstream VMA header.
#[inline]
unsafe fn i915_vma_resource_put(resource: *mut I915VmaResource) {
    unsafe { dma_fence_put(&mut (*resource).unbind_fence) };
}

unsafe fn intel_timeline_sync_is_later(tl: *mut IntelTimeline, fence: *const DmaFence) -> bool {
    unsafe {
        i915_syncmap_is_later(
            ptr::addr_of_mut!((*tl).sync),
            (*fence).context,
            (*fence).seqno as u32,
        )
    }
}

unsafe fn intel_timeline_sync_set(tl: *mut IntelTimeline, fence: *const DmaFence) -> i32 {
    unsafe {
        i915_syncmap_set(
            ptr::addr_of_mut!((*tl).sync),
            (*fence).context,
            (*fence).seqno as u32,
        )
    }
}

// upstream i915_request.c:114
// upstream: i915_request.c i915_fence_release()
unsafe extern "C" fn i915_fence_release(fence: *mut DmaFence) {
    let rq = unsafe { to_request(fence) };
    gem_bug_on!(unsafe { (*rq).guc_prio != GUC_PRIO_INIT && (*rq).guc_prio != GUC_PRIO_FINI });
    unsafe { i915_request_free_capture_list((*rq).capture_list) };
    if !unsafe { (*rq).batch_res }.is_null() {
        unsafe { i915_vma_resource_put((*rq).batch_res) };
        unsafe { (*rq).batch_res = ptr::null_mut() };
    }
    unsafe {
        i915_sw_fence_fini(&mut (*rq).submit);
        i915_sw_fence_fini(&mut (*rq).semaphore);
    }
    let mask = unsafe { (*rq).execution_mask };
    if mask.count_ones() == 1 {
        let engine = unsafe { (*rq).engine };
        if !engine.is_null() {
            let slot = ptr::addr_of_mut!((*engine).request_pool);
            let atomic_slot = unsafe { &*slot.cast::<AtomicPtr<I915Request>>() };
            if atomic_slot
                .compare_exchange(ptr::null_mut(), rq, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
        }
    }
    unsafe { kmem_cache_free(SLAB_REQUESTS, rq.cast()) };
}

#[allow(non_upper_case_globals)]
pub static i915_fence_ops: DmaFenceOps = DmaFenceOps {
    get_driver_name: Some(i915_fence_get_driver_name),
    get_timeline_name: Some(i915_fence_get_timeline_name),
    enable_signaling: Some(i915_fence_enable_signaling),
    signaled: Some(i915_fence_signaled),
    wait: Some(i915_fence_wait),
    release: Some(i915_fence_release),
    set_deadline: None,
};

// upstream i915_request.c:184
// upstream: i915_request.c irq_execute_cb()
unsafe extern "C" fn irq_execute_cb(wrk: *mut IrqWork) {
    let cb = unsafe {
        wrk.cast::<u8>()
            .sub(offset_of!(ExecuteCb, work))
            .cast::<ExecuteCb>()
    };
    unsafe { i915_sw_fence_complete((*cb).fence) };
    unsafe { kmem_cache_free(SLAB_EXECUTE_CBS, cb.cast()) };
}

// upstream i915_request.c:192
// upstream: i915_request.c __notify_execute_cb()
unsafe fn __notify_execute_cb(rq: *mut I915Request, queue: fn(*mut IrqWork) -> bool) {
    if unsafe { llist_empty(&(*rq).execute_cb) } {
        return;
    }
    let mut node = unsafe { llist_del_all(&mut (*rq).execute_cb) };
    while !node.is_null() {
        let next = unsafe { (*node).next };
        let work = unsafe {
            node.cast::<u8>()
                .sub(offset_of!(ExecuteCb, work) + offset_of!(IrqWork, node))
                .cast::<IrqWork>()
        };
        let _ = queue(work);
        node = next;
    }
}

// upstream i915_request.c:206
fn irq_work_queue_adapter(work: *mut IrqWork) -> bool {
    crate::linux::irq::irq_work_queue(work)
}

// upstream: i915_request.c __notify_execute_cb_irq()
unsafe fn __notify_execute_cb_irq(rq: *mut I915Request) {
    unsafe { __notify_execute_cb(rq, irq_work_queue_adapter) };
}

// upstream i915_request.c:211
// upstream: i915_request.c irq_work_imm()
fn irq_work_imm(wrk: *mut IrqWork) -> bool {
    if let Some(func) = unsafe { (*wrk).func } {
        unsafe { func(wrk) };
    }
    false
}

// upstream i915_request.c:217
// upstream: i915_request.c i915_request_notify_execute_cb_imm()
pub unsafe fn i915_request_notify_execute_cb_imm(rq: *mut I915Request) {
    unsafe { __notify_execute_cb(rq, irq_work_imm) };
}

// upstream i915_request.c:224
// upstream: i915_request.c __i915_request_fill()
unsafe fn __i915_request_fill(rq: *mut I915Request, value: u8) {
    let ring = unsafe { (*rq).ring };
    let vaddr = unsafe { (*ring).vaddr }.cast::<u8>();
    let mut head = unsafe { (*rq).infix };
    let postfix = unsafe { (*rq).postfix };
    let ring_size = unsafe { (*ring).size };
    if postfix < head {
        unsafe { ptr::write_bytes(vaddr.add(head as usize), value, (ring_size - head) as usize) };
        head = 0;
    }
    unsafe { ptr::write_bytes(vaddr.add(head as usize), value, (postfix - head) as usize) };
}

// upstream i915_request.c:245
// upstream: i915_request.c i915_request_active_engine()
pub unsafe fn i915_request_active_engine(
    rq: *mut I915Request,
    active: *mut *mut IntelEngineCs,
) -> bool {
    let mut locked = unsafe { ptr::read_volatile(ptr::addr_of!((*rq).engine)) };
    unsafe { spin_lock_irq(&mut (*(*locked).sched_engine).lock) };
    loop {
        let engine = unsafe { ptr::read_volatile(ptr::addr_of!((*rq).engine)) };
        if locked == engine {
            break;
        }
        unsafe { spin_unlock(&mut (*(*locked).sched_engine).lock) };
        locked = engine;
        unsafe { spin_lock_irq(&mut (*(*locked).sched_engine).lock) };
    }
    let mut ret = false;
    if i915_request_is_active(unsafe { &*rq }) {
        if !unsafe { __i915_request_is_complete(rq) } {
            unsafe { *active = locked };
        }
        ret = true;
    }
    unsafe { spin_unlock_irq(&mut (*(*locked).sched_engine).lock) };
    ret
}

// upstream i915_request.c:278
// upstream: i915_request.c __rq_watchdog_expired()
unsafe extern "C" fn __rq_watchdog_expired(
    timer: *mut crate::intel_context_upstream::Hrtimer,
) -> i32 {
    let rq = unsafe {
        timer
            .cast::<u8>()
            .sub(offset_of!(I915RequestWatchdog, timer))
            .sub(offset_of!(I915Request, watchdog))
            .cast::<I915Request>()
    };
    let gt = unsafe { (*(*rq).engine).gt };
    if !i915_request_completed(unsafe { &*rq }) {
        if unsafe { llist_add(&mut (*rq).watchdog.link, &mut (*gt).watchdog.list) } {
            unsafe { queue_work((*(*gt).i915).unordered_wq, &mut (*gt).watchdog.work) };
        }
    } else {
        unsafe { i915_request_put(rq) };
    }
    HRTIMER_NORESTART
}

// upstream i915_request.c:294
// upstream: i915_request.c __rq_init_watchdog()
unsafe fn __rq_init_watchdog(rq: *mut I915Request) {
    unsafe {
        hrtimer_setup(
            &mut (*rq).watchdog.timer,
            Some(__rq_watchdog_expired),
            CLOCK_MONOTONIC,
            HRTIMER_MODE_REL,
        )
    };
}

// upstream i915_request.c:301
// upstream: i915_request.c __rq_arm_watchdog()
unsafe fn __rq_arm_watchdog(rq: *mut I915Request) {
    let timeout = unsafe { (*(*rq).context).watchdog.timeout_us };
    if timeout == 0 {
        return;
    }
    unsafe { i915_request_get(rq) };
    unsafe {
        hrtimer_start_range_ns(
            &mut (*rq).watchdog.timer,
            ns_to_ktime(timeout as u64 * NSEC_PER_USEC),
            NSEC_PER_MSEC,
            HRTIMER_MODE_REL,
        )
    };
}

// upstream i915_request.c:318
// upstream: i915_request.c __rq_cancel_watchdog()
unsafe fn __rq_cancel_watchdog(rq: *mut I915Request) {
    if unsafe { hrtimer_try_to_cancel(&mut (*rq).watchdog.timer) } > 0 {
        unsafe { i915_request_put(rq) };
    }
}

// upstream i915_request.c:333 (CONFIG_DRM_I915_CAPTURE_ERROR=y)
// upstream: i915_request.c i915_request_free_capture_list()
pub unsafe fn i915_request_free_capture_list(mut capture: *mut I915CaptureList) {
    while !capture.is_null() {
        let next = unsafe { (*capture).next };
        unsafe { i915_vma_resource_put((*capture).vma_res) };
        unsafe { kfree(capture) };
        capture = next;
    }
}

// upstream i915_request.c:358
// upstream: i915_request.c i915_request_retire()
pub unsafe fn i915_request_retire(rq: *mut I915Request) -> bool {
    if !unsafe { __i915_request_is_complete(rq) } {
        return false;
    }
    axlog::trace!(
        "i915 request retire fence={:#x}:{:#x}",
        unsafe { (*rq).fence.context },
        unsafe { (*rq).fence.seqno }
    );
    gem_bug_on!(!i915_sw_fence_signaled(unsafe { &(*rq).submit }));
    unsafe { trace_i915_request_retire(rq) };
    i915_request_mark_complete(unsafe { &*rq });
    unsafe { __rq_cancel_watchdog(rq) };
    gem_bug_on!(!list_is_first(
        &(*rq).link,
        &i915_request_timeline(rq).as_ref().unwrap().requests
    ));
    if CONFIG_DRM_I915_DEBUG_GEM {
        unsafe { __i915_request_fill(rq, POISON_FREE as u8) };
    }
    unsafe { (*(*rq).ring).head = (*rq).postfix };
    if !i915_request_signaled(rq) {
        unsafe { spin_lock_irq(&mut (*rq).lock) };
        unsafe { dma_fence_signal_locked(&mut (*rq).fence) };
        unsafe { spin_unlock_irq(&mut (*rq).lock) };
    }
    if test_and_set_bit(I915_FENCE_FLAG_BOOST, unsafe { &mut (*rq).fence.flags }) {
        unsafe { intel_rps_dec_waiters(&mut (*(*(*rq).engine).gt).rps) };
    }
    if let Some(remove_active) = unsafe { (*(*rq).engine).remove_active_request } {
        unsafe { remove_active(rq) };
    }
    gem_bug_on!(!unsafe { llist_empty(&(*rq).execute_cb) });
    unsafe { __list_del_entry(&mut (*rq).link) };
    unsafe { intel_context_exit((*rq).context) };
    unsafe { intel_context_unpin((*rq).context) };
    unsafe { i915_sched_node_fini(&mut (*rq).sched) };
    unsafe { i915_request_put(rq) };
    true
}

// upstream i915_request.c:420
// upstream: i915_request.c i915_request_retire_upto()
pub unsafe fn i915_request_retire_upto(rq: *mut I915Request) {
    let tl = unsafe { i915_request_timeline(rq) };
    gem_bug_on!(!unsafe { __i915_request_is_complete(rq) });
    loop {
        let tmp = unsafe {
            list_first_entry!(&(*tl).requests, I915Request, link)
        };
        gem_bug_on!(!i915_request_completed(unsafe { &*tmp }));
        let done = unsafe { i915_request_retire(tmp) };
        if !done || tmp == rq {
            break;
        }
    }
}

// upstream i915_request.c:434
// upstream: i915_request.c __engine_active()
unsafe fn __engine_active(engine: *mut IntelEngineCs) -> *const *mut I915Request {
    unsafe { ptr::read_volatile(ptr::addr_of!((*engine).execlists.active)) }
}

// upstream i915_request.c:440
// upstream: i915_request.c __request_in_flight()
unsafe fn __request_in_flight(signal: *mut I915Request) -> bool {
    if !i915_request_is_ready(unsafe { &*signal }) {
        return false;
    }
    if intel_context_inflight(unsafe { (*signal).context }).is_null() {
        return false;
    }
    let mut inflight = false;
    rcu_read_lock();
    let mut port = unsafe { __engine_active((*signal).engine) };
    loop {
        let rq = unsafe { ptr::read_volatile(port) };
        if rq.is_null() {
            break;
        }
        if unsafe { (*rq).context == (*signal).context } {
            inflight = i915_seqno_passed(unsafe { (*rq).fence.seqno as u32 }, unsafe {
                (*signal).fence.seqno as u32
            });
            break;
        }
        port = unsafe { port.add(1) };
    }
    rcu_read_unlock();
    inflight
}

// upstream i915_request.c:502
// upstream: i915_request.c __await_execution()
unsafe fn __await_execution(rq: *mut I915Request, signal: *mut I915Request, gfp: u32) -> i32 {
    if i915_request_is_active(unsafe { &*signal }) {
        return 0;
    }
    let cb = unsafe { kmem_cache_zalloc::<ExecuteCb>(SLAB_EXECUTE_CBS, gfp) };
    if cb.is_null() {
        return -ENOMEM;
    }
    unsafe { (*cb).fence = ptr::addr_of_mut!((*rq).submit) };
    unsafe { crate::i915_sw_fence_upstream::i915_sw_fence_await(&mut (*rq).submit) };
    unsafe { init_irq_work(&mut (*cb).work, irq_execute_cb) };
    if unsafe { llist_add(core::ptr::addr_of_mut!((*cb).work.node).cast::<LlistNode>(), &mut (*signal).execute_cb) } {
        if i915_request_is_active(unsafe { &*signal }) || unsafe { __request_in_flight(signal) } {
            unsafe { i915_request_notify_execute_cb_imm(signal) };
        }
    }
    0
}

// upstream i915_request.c:542
// upstream: i915_request.c fatal_error()
fn fatal_error(error: i32) -> bool {
    error != 0 && error != -EAGAIN && error != -ETIMEDOUT
}

// upstream i915_request.c:554
// upstream: i915_request.c __i915_request_skip()
pub unsafe fn __i915_request_skip(rq: *mut I915Request) {
    gem_bug_on!(!fatal_error(unsafe { (*rq).fence.error }));
    if unsafe { (*rq).infix == (*rq).postfix } {
        return;
    }
    unsafe { __i915_request_fill(rq, 0) };
    unsafe { (*rq).infix = (*rq).postfix };
}

// upstream i915_request.c:590
// upstream: i915_request.c i915_request_set_error_once()
pub unsafe fn i915_request_set_error_once(rq: *mut I915Request, error: i32) -> bool {
    gem_bug_on!((error as isize) >= 0 || (error as isize) < -(MAX_ERRNO as isize));
    if i915_request_signaled(rq) {
        return false;
    }
    let mut old = unsafe { atomic_read(&*ptr::addr_of!((*rq).fence.error).cast::<AtomicT>()) };
    loop {
        if fatal_error(old) {
            return false;
        }
        match unsafe { atomic_cmpxchg(&mut (*rq).fence.error, old, error) } {
            observed if observed == old => return true,
            observed => old = observed,
        }
    }
}

// upstream i915_request.c:606
// upstream: i915_request.c i915_request_mark_eio()
pub unsafe fn i915_request_mark_eio(rq: *mut I915Request) -> *mut I915Request {
    if unsafe { __i915_request_is_complete(rq) } {
        return ptr::null_mut();
    }
    gem_bug_on!(i915_request_signaled(rq));
    let rq = unsafe { i915_request_get(rq) };
    unsafe { i915_request_set_error_once(rq, -EIO) };
    i915_request_mark_complete(unsafe { &*rq });
    rq
}

// upstream i915_request.c:626
// upstream: i915_request.c __i915_request_submit()
pub unsafe fn __i915_request_submit(request: *mut I915Request) -> bool {
    let engine = unsafe { (*request).engine };
    gem_bug_on!(!irqs_disabled());
    lockdep_assert_held(unsafe { &(*(*engine).sched_engine).lock });
    let mut result = false;
    if unsafe { __i915_request_is_complete(request) } {
        unsafe { list_del_init(&mut (*request).sched.link) };
        return submit_active(request, engine, result);
    }
    if unsafe { !intel_context_is_schedulable((*request).context) } {
        unsafe { i915_request_set_error_once(request, -EIO) };
    }
    if fatal_error(unsafe { (*request).fence.error }) {
        unsafe { __i915_request_skip(request) };
    }
    if unsafe { (*request).sched.semaphores != 0 && i915_sw_fence_signaled(&(*request).semaphore) }
    {
        unsafe { (*engine).saturated |= (*request).sched.semaphores };
    }
    if let Some(emit) = unsafe { (*engine).emit_fini_breadcrumb } {
        unsafe {
            emit(
                request,
                (*(*request).ring).vaddr.cast::<u32>().add((*request).postfix as usize),
            )
        };
    }
    unsafe { trace_i915_request_execute(request) };
    if let Some(bump) = unsafe { (*engine).bump_serial } {
        unsafe { bump(engine) };
    } else {
        unsafe { (*engine).serial += 1 };
    }
    result = true;
    gem_bug_on!(test_bit(I915_FENCE_FLAG_ACTIVE, unsafe {
        &(*request).fence.flags
    }));
    if let Some(add_active) = unsafe { (*engine).add_active_request } {
        unsafe { add_active(request) };
    }
    submit_active(request, engine, result)
}

unsafe fn submit_active(
    request: *mut I915Request,
    _engine: *mut IntelEngineCs,
    result: bool,
) -> bool {
    clear_bit(I915_FENCE_FLAG_PQUEUE, unsafe {
        &mut (*request).fence.flags
    });
    set_bit(I915_FENCE_FLAG_ACTIVE, unsafe {
        &mut (*request).fence.flags
    });
    unsafe { __notify_execute_cb_irq(request) };
    if test_bit(DMA_FENCE_FLAG_ENABLE_SIGNAL_BIT, unsafe {
        &(*request).fence.flags
    }) {
        unsafe { i915_request_enable_breadcrumb(request) };
    }
    result
}

// upstream i915_request.c:699
// upstream: i915_request.c i915_request_submit()
pub unsafe fn i915_request_submit(request: *mut I915Request) {
    let engine = unsafe { (*request).engine };
    let mut flags = 0;
    unsafe { spin_lock_irqsave(&mut (*(*engine).sched_engine).lock, &mut flags) };
    unsafe { __i915_request_submit(request) };
    unsafe { spin_unlock_irqrestore(&mut (*(*engine).sched_engine).lock, flags) };
}

// upstream i915_request.c:712
// upstream: i915_request.c __i915_request_unsubmit()
pub unsafe fn __i915_request_unsubmit(request: *mut I915Request) {
    let engine = unsafe { (*request).engine };
    gem_bug_on!(!irqs_disabled());
    lockdep_assert_held(unsafe { &(*(*engine).sched_engine).lock });
    gem_bug_on!(!test_bit(I915_FENCE_FLAG_ACTIVE, unsafe {
        &(*request).fence.flags
    }));
    clear_bit_unlock(I915_FENCE_FLAG_ACTIVE, unsafe {
        &mut (*request).fence.flags
    });
    if test_bit(DMA_FENCE_FLAG_ENABLE_SIGNAL_BIT, unsafe {
        &(*request).fence.flags
    }) {
        unsafe { i915_request_cancel_breadcrumb(request) };
    }
    if unsafe { (*request).sched.semaphores != 0 && __i915_request_has_started(request) } {
        unsafe { (*request).sched.semaphores = 0 };
    }
}

// upstream i915_request.c:750
// upstream: i915_request.c i915_request_unsubmit()
pub unsafe fn i915_request_unsubmit(request: *mut I915Request) {
    let engine = unsafe { (*request).engine };
    let mut flags = 0;
    unsafe { spin_lock_irqsave(&mut (*(*engine).sched_engine).lock, &mut flags) };
    unsafe { __i915_request_unsubmit(request) };
    unsafe { spin_unlock_irqrestore(&mut (*(*engine).sched_engine).lock, flags) };
}

// upstream i915_request.c:763
// upstream: i915_request.c i915_request_cancel()
pub unsafe fn i915_request_cancel(rq: *mut I915Request, error: i32) {
    if !unsafe { i915_request_set_error_once(rq, error) } {
        return;
    }
    set_bit(I915_FENCE_FLAG_SENTINEL, unsafe { &mut (*rq).fence.flags });
    unsafe { intel_context_cancel_request((*rq).context, rq) };
}

// upstream i915_request.c:773
// upstream: i915_request.c submit_notify()
unsafe extern "C" fn submit_notify(fence: *mut I915SwFence, state: I915SwFenceNotify) -> i32 {
    let request = unsafe {
        fence
            .cast::<u8>()
            .sub(offset_of!(I915Request, submit))
            .cast::<I915Request>()
    };
    match state {
        I915SwFenceNotify::FenceComplete => {
            unsafe { trace_i915_request_submit(request) };
            if unsafe { (*fence).error != 0 } {
                unsafe { i915_request_set_error_once(request, (*fence).error) };
            } else {
                unsafe { __rq_arm_watchdog(request) };
            }
            rcu_read_lock();
            if let Some(submit) = unsafe { (*(*request).engine).submit_request } {
                unsafe { submit(request) };
            }
            rcu_read_unlock();
        }
        I915SwFenceNotify::FenceFree => unsafe { i915_request_put(request) },
    }
    NOTIFY_DONE
}

// upstream i915_request.c:809
// upstream: i915_request.c semaphore_notify()
unsafe extern "C" fn semaphore_notify(fence: *mut I915SwFence, state: I915SwFenceNotify) -> i32 {
    let rq = unsafe {
        fence
            .cast::<u8>()
            .sub(offset_of!(I915Request, semaphore))
            .cast::<I915Request>()
    };
    if let I915SwFenceNotify::FenceFree = state {
        unsafe { i915_request_put(rq) };
    }
    NOTIFY_DONE
}

// upstream i915_request.c:826
// upstream: i915_request.c retire_requests()
unsafe fn retire_requests(tl: *mut IntelTimeline) {
    let mut rq = unsafe {
        list_first_entry_or_null!(&(*tl).requests, I915Request, link)
    };
    while !rq.is_null() {
        let next =
            unsafe { list_next_entry_or_null(rq, offset_of!(I915Request, link), &(*tl).requests) };
        if !unsafe { i915_request_retire(rq) } {
            break;
        }
        rq = next;
    }
}

// Upstream i915 list helper: return the entry after `rq`, or NULL at the head.
unsafe fn list_next_entry_or_null(
    rq: *mut I915Request,
    offset: usize,
    head: *const crate::intel_engine_cs_upstream::ListHead,
) -> *mut I915Request {
    let next = unsafe { ptr::read_volatile(ptr::addr_of!((*rq).link.next)) };
    if core::ptr::eq(next, head as *mut _) {
        ptr::null_mut()
    } else {
        (next as usize).wrapping_sub(offset) as *mut I915Request
    }
}

// upstream i915_request.c:835
// upstream: i915_request.c request_alloc_slow()
unsafe fn request_alloc_slow(
    tl: *mut IntelTimeline,
    reserved: *mut *mut I915Request,
    gfp: u32,
) -> *mut I915Request {
    if !gfpflags_allow_blocking(gfp as c_ulong) {
        let rq = unsafe { ptr::replace(reserved, ptr::null_mut()) };
        if !rq.is_null() {
            return rq;
        }
        return unsafe { kmem_cache_zalloc::<I915Request>(SLAB_REQUESTS, gfp) };
    }
    if !unsafe { list_empty(&(*tl).requests) } {
        let oldest = unsafe {
            list_first_entry!(&(*tl).requests, I915Request, link)
        };
        unsafe { i915_request_retire(oldest) };
        let rq = unsafe {
            kmem_cache_zalloc::<I915Request>(
                SLAB_REQUESTS,
                gfp | __GFP_RETRY_MAYFAIL | __GFP_NOWARN,
            )
        };
        if !rq.is_null() {
            return rq;
        }
        let last = unsafe {
            list_last_entry!(&(*tl).requests, I915Request, link)
        };
        unsafe { cond_synchronize_rcu((*last).rcustate) };
        unsafe { retire_requests(tl) };
    }
    unsafe { kmem_cache_zalloc::<I915Request>(SLAB_REQUESTS, gfp) }
}

// upstream i915_request.c:874
// upstream: i915_request.c __i915_request_ctor()
unsafe fn __i915_request_ctor(arg: *mut c_void) {
    let rq = arg.cast::<I915Request>();
    unsafe { spin_lock_init(&mut (*rq).lock) };
    unsafe { i915_sched_node_init(&mut (*rq).sched) };
    unsafe { i915_sw_fence_init(&mut (*rq).submit, Some(submit_notify)) };
    unsafe { i915_sw_fence_init(&mut (*rq).semaphore, Some(semaphore_notify)) };
    if CONFIG_DRM_I915_CAPTURE_ERROR {
        unsafe { (*rq).capture_list = ptr::null_mut() };
    }
    unsafe { (*rq).batch_res = ptr::null_mut() };
    unsafe { init_llist_head(&mut (*rq).execute_cb) };
}

// CONFIG_DRM_I915_SELFTEST=n for the recorded target configuration.
unsafe fn clear_batch_ptr(rq: *mut I915Request) {
    if CONFIG_DRM_I915_SELFTEST {
        unsafe { (*rq).batch_res = ptr::null_mut() };
    }
}

// upstream i915_request.c:895
// upstream: i915_request.c __i915_request_create()
pub unsafe fn __i915_request_create(ce: *mut IntelContext, gfp: u32) -> *mut I915Request {
    let tl = unsafe { (*ce).timeline };
    let mut rq = unsafe {
        kmem_cache_zalloc::<I915Request>(SLAB_REQUESTS, gfp | __GFP_RETRY_MAYFAIL | __GFP_NOWARN)
    };
    let mut seqno = 0;
    let mut ret = 0;
    unsafe { intel_context_pin(ce) };
    if rq.is_null() {
        rq = unsafe { request_alloc_slow(tl, &mut (*(*ce).engine).request_pool, gfp) };
        if rq.is_null() {
            ret = -ENOMEM;
            unsafe { intel_context_unpin(ce) };
            return ret as isize as *mut I915Request;
        }
    }
    unsafe { __i915_request_ctor(rq.cast()) };
    unsafe {
        (*rq).context = ce;
        (*rq).engine = (*ce).engine;
        (*rq).ring = (*ce).ring;
        (*rq).execution_mask = (*(*ce).engine).mask;
        (*rq).i915 = (*(*ce).engine).i915;
    }
    ret = unsafe { intel_timeline_get_seqno(tl, rq, &mut seqno) };
    if ret != 0 {
        unsafe { kmem_cache_free(SLAB_REQUESTS, rq.cast()) };
        unsafe { intel_context_unpin(ce) };
        return ret as isize as *mut I915Request;
    }
    unsafe {
        dma_fence_init(
            &mut (*rq).fence,
            &i915_fence_ops,
            &mut (*rq).lock,
            (*tl).fence_context,
            seqno as u64,
        )
    };
    unsafe { (*rq).timeline = tl };
    unsafe { (*rq).hwsp_seqno = (*tl).hwsp_seqno };
    gem_bug_on!(unsafe { __i915_request_is_complete(rq) });
    unsafe { (*rq).rcustate = get_state_synchronize_rcu() };
    unsafe { (*rq).guc_prio = GUC_PRIO_INIT };
    unsafe { i915_sw_fence_reinit(&mut (*i915_request_get(rq)).submit) };
    unsafe { i915_sw_fence_reinit(&mut (*i915_request_get(rq)).semaphore) };
    unsafe { i915_sched_node_reinit(&mut (*rq).sched) };
    unsafe { clear_batch_ptr(rq) };
    unsafe { __rq_init_watchdog(rq) };
    if CONFIG_DRM_I915_CAPTURE_ERROR {
        gem_bug_on!(!unsafe { (*rq).capture_list }.is_null());
    }
    gem_bug_on!(!unsafe { llist_empty(&(*rq).execute_cb) });
    gem_bug_on!(!unsafe { (*rq).batch_res }.is_null());
    unsafe {
        (*rq).reserved_space =
            2 * (*(*ce).engine).emit_fini_breadcrumb_dw * size_of::<u32>() as u32;
        (*rq).head = (*(*ce).ring).emit;
    }
    let request_alloc = unsafe { (*(*ce).engine).request_alloc };
    if let Some(alloc) = request_alloc {
        ret = unsafe { alloc(rq) };
    } else {
        ret = -EINVAL;
    }
    if ret != 0 {
        unsafe { (*ce).ring.as_mut().unwrap().emit = (*rq).head };
        gem_bug_on!(!unsafe { list_empty(&(*rq).sched.signalers_list) });
        gem_bug_on!(!unsafe { list_empty(&(*rq).sched.waiters_list) });
        unsafe { kmem_cache_free(SLAB_REQUESTS, rq.cast()) };
        unsafe { intel_context_unpin(ce) };
        return ret as isize as *mut I915Request;
    }
    unsafe { (*rq).infix = (*(*ce).ring).emit };
    unsafe { intel_context_mark_active(ce) };
    unsafe { list_add_tail_rcu(&mut (*rq).link, &mut (*tl).requests) };
    rq
}

// upstream i915_request.c:1030
// upstream: i915_request.c i915_request_create()
pub unsafe fn i915_request_create(ce: *mut IntelContext) -> *mut I915Request {
    let tl = unsafe { intel_context_timeline_lock(ce) };
    if IS_ERR(tl) {
        return tl.cast();
    }
    let first =
        unsafe { list_first_entry!(&(*tl).requests, I915Request, link) };
    if !unsafe { list_is_last(&(*first).link, &(*tl).requests) } {
        unsafe { i915_request_retire(first) };
    }
    unsafe { intel_context_enter(ce) };
    let rq = unsafe { __i915_request_create(ce, GFP_KERNEL) };
    unsafe { intel_context_exit(ce) };
    if IS_ERR(rq) {
        unsafe { intel_context_timeline_unlock(tl) };
        return rq;
    }
    unsafe { (*rq).cookie = lockdep_pin_lock(&mut (*tl).mutex) };
    rq
}

// upstream i915_request.c:1060
// upstream: i915_request.c i915_request_await_start()
unsafe fn i915_request_await_start(rq: *mut I915Request, signal: *mut I915Request) -> i32 {
    if unsafe { i915_request_timeline(rq) == (*signal).timeline }
        || i915_request_started(unsafe { &*signal })
    {
        return 0;
    }
    let mut fence: *mut DmaFence = ptr::null_mut();
    rcu_read_lock();
    let pos = unsafe { ptr::read_volatile((*signal).link.prev) };
    if !unsafe { __i915_request_has_started(signal) } {
        let timeline = unsafe { rcu_dereference!((*signal).timeline) };
        if pos != unsafe { ptr::addr_of_mut!((*timeline).requests) } {
            let prev = unsafe { list_entry!(pos, I915Request, link) };
            if !unsafe { i915_request_get_rcu(prev) }.is_null() {
                if unsafe {
                    ptr::eq(ptr::read_volatile(ptr::addr_of!((*prev).link.next)) as *const _, ptr::addr_of!((*signal).link))
                } {
                    fence = unsafe { ptr::addr_of_mut!((*prev).fence) };
                } else {
                    unsafe { i915_request_put(prev) };
                }
            }
        }
    }
    rcu_read_unlock();
    if fence.is_null() {
        return 0;
    }
    let err = if !unsafe { intel_timeline_sync_is_later(i915_request_timeline(rq), &*fence) } {
        unsafe { i915_sw_fence_await_dma_fence(&mut (*rq).submit, fence, 0 as c_ulong, I915_FENCE_GFP as c_ulong) }
    } else {
        0
    };
    unsafe { dma_fence_put(fence) };
    err
}

// upstream i915_request.c:1126
// upstream: i915_request.c already_busywaiting()
unsafe fn already_busywaiting(rq: *mut I915Request) -> IntelEngineMask {
    unsafe { (*rq).sched.semaphores | ptr::read_volatile(ptr::addr_of!((*(*rq).engine).saturated)) }
}

// upstream i915_request.c:1144
// upstream: i915_request.c __emit_semaphore_wait()
unsafe fn __emit_semaphore_wait(to: *mut I915Request, from: *mut I915Request, seqno: u32) -> i32 {
    let has_token = unsafe { GRAPHICS_VER((*(*to).engine).i915) >= 12 };
    gem_bug_on!(unsafe { GRAPHICS_VER((*(*to).engine).i915) < 8 });
    gem_bug_on!(i915_request_has_initial_breadcrumb(unsafe { &*to }));
    let mut hwsp_offset = 0;
    let err = unsafe { intel_timeline_read_hwsp(from, to, &mut hwsp_offset) };
    if err != 0 {
        return err;
    }
    let len = if has_token { 6 } else { 4 };
    let cs = unsafe { intel_ring_begin(to, len) };
    if IS_ERR(cs) {
        return PTR_ERR(cs);
    }
    unsafe {
        *cs.add(0) = MI_SEMAPHORE_WAIT
            | MI_SEMAPHORE_GLOBAL_GTT
            | MI_SEMAPHORE_POLL
            | MI_SEMAPHORE_SAD_GTE_SDD + has_token as u32;
        *cs.add(1) = seqno;
        *cs.add(2) = hwsp_offset;
        *cs.add(3) = 0;
        if has_token {
            *cs.add(4) = 0;
            *cs.add(5) = MI_NOOP;
        }
        intel_ring_advance(to, cs.add(len as usize));
    }
    0
}

// upstream i915_request.c:1195
// upstream: i915_request.c can_use_semaphore_wait()
unsafe fn can_use_semaphore_wait(to: *mut I915Request, from: *mut I915Request) -> bool {
    unsafe { (*(*(*to).engine).gt).ggtt == (*(*(*from).engine).gt).ggtt }
}

// upstream i915_request.c:1195
// upstream: i915_request.c intel_timeline_sync_has_start()
unsafe fn intel_timeline_sync_has_start(tl: *mut IntelTimeline, fence: *const DmaFence) -> bool {
    unsafe {
        i915_syncmap_is_later(
            ptr::addr_of_mut!((*tl).sync),
            (*fence).context,
            (*fence).seqno as u32 - 1,
        )
    }
}

// upstream i915_request.c:1201
// upstream: i915_request.c intel_timeline_sync_set_start()
unsafe fn intel_timeline_sync_set_start(tl: *mut IntelTimeline, fence: *const DmaFence) -> i32 {
    unsafe {
        i915_syncmap_set(
            ptr::addr_of_mut!((*tl).sync),
            (*fence).context,
            (*fence).seqno as u32 - 1,
        )
    }
}

// upstream i915_request.c:1201
// upstream: i915_request.c emit_semaphore_wait()
unsafe fn emit_semaphore_wait(to: *mut I915Request, from: *mut I915Request, gfp: u32) -> i32 {
    let mask = unsafe { ptr::read_volatile(ptr::addr_of!((*(*from).engine).mask)) };
    let mut wait = ptr::addr_of_mut!((*to).submit);
    if unsafe { can_use_semaphore_wait(to, from) }
        && unsafe { intel_context_use_semaphores((*to).context) }
        && !i915_request_has_initial_breadcrumb(unsafe { &*to })
        && unsafe { (*from).sched.flags & I915_SCHED_HAS_EXTERNAL_CHAIN == 0 }
        && unsafe { already_busywaiting(to) & mask == 0 }
        && unsafe { i915_request_await_start(to, from) >= 0 }
        && unsafe { __await_execution(to, from, gfp) == 0 }
        && unsafe { __emit_semaphore_wait(to, from, (*from).fence.seqno as u32) == 0 }
    {
        unsafe { (*to).sched.semaphores |= mask };
        wait = ptr::addr_of_mut!((*to).semaphore);
    }
    unsafe { i915_sw_fence_await_dma_fence(wait, &mut (*from).fence, 0, I915_FENCE_GFP as c_ulong) }
}

// upstream i915_request.c:1264
// upstream: i915_request.c __i915_request_await_execution()
unsafe fn __i915_request_await_execution(to: *mut I915Request, from: *mut I915Request) -> i32 {
    gem_bug_on!(intel_context_is_barrier(unsafe { (*from).context }));
    let err = unsafe { __await_execution(to, from, I915_FENCE_GFP) };
    if err != 0 {
        return err;
    }
    if unsafe { intel_timeline_sync_has_start(i915_request_timeline(to), &(*from).fence) } {
        return 0;
    }
    let err = unsafe { i915_request_await_start(to, from) };
    if err < 0 {
        return err;
    }
    if unsafe { can_use_semaphore_wait(to, from) && intel_engine_has_semaphores((*to).engine) }
        && !i915_request_has_initial_breadcrumb(unsafe { &*to })
    {
        let err = unsafe { __emit_semaphore_wait(to, from, (*from).fence.seqno as u32 - 1) };
        if err < 0 {
            return err;
        }
    }
    if unsafe { (*(*(*to).engine).sched_engine).schedule.is_some() } {
        let err = unsafe {
            i915_sched_node_add_dependency(
                &mut (*to).sched,
                &mut (*from).sched,
                I915_DEPENDENCY_WEAK,
            )
        };
        if err < 0 {
            return err;
        }
    }
    unsafe { intel_timeline_sync_set_start(i915_request_timeline(to), &(*from).fence) }
}

#[repr(C)]
struct DmaFenceArrayView {
    base: DmaFence,
    num_fences: u32,
    _num_pending: AtomicT,
    fences: *mut *mut DmaFence,
}

#[repr(C)]
struct DmaFenceChainView {
    base: DmaFence,
    prev: *mut DmaFence,
    prev_seqno: u64,
    fence: *mut DmaFence,
}

#[repr(C)]
struct I915DepsView {
    single: *mut DmaFence,
    fences: *mut *mut DmaFence,
    num_deps: u32,
    fences_size: u32,
    gfp: u32,
}

#[repr(C)]
struct DmaResv {
    _opaque: [u8; 0],
}

#[repr(C)]
struct DmaResvIter {
    obj: *mut DmaResv,
    usage: i32,
    fence: *mut DmaFence,
    fence_usage: i32,
    index: u32,
    fences: *mut c_void,
    num_fences: u32,
    is_restarted: bool,
}

unsafe extern "C" {
    fn dma_fence_get(fence: *mut DmaFence) -> *mut DmaFence;
    fn dma_fence_put(fence: *mut DmaFence);
    fn dma_fence_chain_walk(fence: *mut DmaFence) -> *mut DmaFence;
    fn i915_fence_context_timeout(context: u64) -> c_ulong;
    fn dma_fence_add_callback(
        fence: *mut DmaFence,
        cb: *mut DmaFenceCb,
        callback: unsafe extern "C" fn(*mut DmaFence, *mut DmaFenceCb),
    ) -> i32;
    fn dma_fence_remove_callback(fence: *mut DmaFence, cb: *mut DmaFenceCb) -> bool;
    fn dma_fence_is_signaled(fence: *mut DmaFence) -> bool;
    fn dma_fence_timeline_name(fence: *mut DmaFence) -> *const c_char;
    fn dma_resv_iter_next(cursor: *mut DmaResvIter) -> *mut DmaFence;
    static dma_fence_array_ops: DmaFenceOps;
    static dma_fence_chain_ops: DmaFenceOps;
}

unsafe fn is_i915_fence(fence: *mut DmaFence) -> bool {
    unsafe { (*fence).ops == ptr::addr_of!(i915_fence_ops).cast::<c_void>() }
}

unsafe fn dma_fence_is_array(fence: *const DmaFence) -> bool {
    !fence.is_null()
        && unsafe { (*fence).ops == ptr::addr_of!(dma_fence_array_ops).cast::<c_void>() }
}

unsafe fn dma_fence_is_chain(fence: *const DmaFence) -> bool {
    !fence.is_null()
        && unsafe { (*fence).ops == ptr::addr_of!(dma_fence_chain_ops).cast::<c_void>() }
}

// upstream i915_request.c:1336
// upstream: i915_request.c mark_external()
unsafe fn mark_external(rq: *mut I915Request) {
    unsafe { (*rq).sched.flags |= I915_SCHED_HAS_EXTERNAL_CHAIN };
}

// upstream i915_request.c:1349
// upstream: i915_request.c __i915_request_await_external()
unsafe fn __i915_request_await_external(rq: *mut I915Request, fence: *mut DmaFence) -> i32 {
    unsafe { mark_external(rq) };
    unsafe {
        i915_sw_fence_await_dma_fence(
            &mut (*rq).submit,
            fence,
            i915_fence_context_timeout((*fence).context),
            I915_FENCE_GFP as c_ulong,
        )
    }
}

// upstream i915_request.c:1358
// upstream: i915_request.c i915_request_await_external()
unsafe fn i915_request_await_external(rq: *mut I915Request, fence: *mut DmaFence) -> i32 {
    if !unsafe { dma_fence_is_chain(fence) } {
        return unsafe { __i915_request_await_external(rq, fence) };
    }
    let mut iter = unsafe { dma_fence_get(fence) };
    let mut err = 0;
    while !iter.is_null() {
        let chain = iter.cast::<DmaFenceChainView>();
        let child = unsafe { (*chain).fence };
        if !unsafe { is_i915_fence(child) } {
            err = unsafe { __i915_request_await_external(rq, iter) };
            break;
        }
        err = unsafe { i915_request_await_dma_fence(rq, child) };
        if err < 0 {
            break;
        }
        iter = unsafe { dma_fence_chain_walk(iter) };
    }
    unsafe { dma_fence_put(iter) };
    err
}

// upstream i915_request.c:1384
// upstream: i915_request.c is_parallel_rq()
unsafe fn is_parallel_rq(rq: *mut I915Request) -> bool {
    unsafe { intel_context_is_parallel((*rq).context) }
}

// upstream i915_request.c:1389
// upstream: i915_request.c request_to_parent()
unsafe fn request_to_parent(rq: *mut I915Request) -> *mut IntelContext {
    unsafe { intel_context_to_parent((*rq).context) }
}

// upstream i915_request.c:1395
// upstream: i915_request.c is_same_parallel_context()
unsafe fn is_same_parallel_context(to: *mut I915Request, from: *mut I915Request) -> bool {
    unsafe { is_parallel_rq(to) && request_to_parent(to) == request_to_parent(from) }
}

// upstream i915_request.c:1403
// upstream: i915_request.c i915_request_await_execution()
pub unsafe fn i915_request_await_execution(rq: *mut I915Request, mut fence: *mut DmaFence) -> i32 {
    let mut child = ptr::addr_of_mut!(fence);
    let mut nchild = 1u32;
    if unsafe { dma_fence_is_array(fence) } {
        let array = fence.cast::<DmaFenceArrayView>();
        child = unsafe { (*array).fences };
        nchild = unsafe { (*array).num_fences };
        gem_bug_on!(nchild == 0);
    }
    while nchild != 0 {
        let current = unsafe { ptr::read(child) };
        child = unsafe { child.add(1) };
        nchild -= 1;
        if test_bit(DMA_FENCE_FLAG_SIGNALED_BIT, unsafe { &(*current).flags })
            || unsafe { (*current).context == (*rq).fence.context }
        {
            continue;
        }
        let ret = if unsafe { is_i915_fence(current) } {
            if unsafe { is_same_parallel_context(rq, to_request(current)) } {
                continue;
            }
            unsafe { __i915_request_await_execution(rq, to_request(current)) }
        } else {
            unsafe { i915_request_await_external(rq, current) }
        };
        if ret < 0 {
            return ret;
        }
    }
    0
}

// upstream i915_request.c:1449
// upstream: i915_request.c await_request_submit()
unsafe fn await_request_submit(to: *mut I915Request, from: *mut I915Request) -> i32 {
    if unsafe { (*to).engine == ptr::read_volatile(ptr::addr_of!((*from).engine)) } {
        unsafe {
            i915_sw_fence_await_sw_fence_gfp(&mut (*to).submit, &mut (*from).submit, I915_FENCE_GFP as c_ulong)
        }
    } else {
        unsafe { __i915_request_await_execution(to, from) }
    }
}

// upstream i915_request.c:1468
// upstream: i915_request.c i915_request_await_request()
unsafe fn i915_request_await_request(to: *mut I915Request, from: *mut I915Request) -> i32 {
    gem_bug_on!(to == from);
    gem_bug_on!(unsafe { (*to).timeline == (*from).timeline });
    if i915_request_completed(unsafe { &*from }) {
        unsafe { i915_sw_fence_set_error_once(&mut (*to).submit, (*from).fence.error) };
        return 0;
    }
    if unsafe { (*(*(*to).engine).sched_engine).schedule.is_some() } {
        let ret = unsafe {
            i915_sched_node_add_dependency(
                &mut (*to).sched,
                &mut (*from).sched,
                I915_DEPENDENCY_EXTERNAL,
            )
        };
        if ret < 0 {
            return ret;
        }
    }
    if !unsafe { intel_engine_uses_guc((*to).engine) }
        && (unsafe {
            (*to).execution_mask | ptr::read_volatile(ptr::addr_of!((*from).execution_mask))
        })
        .count_ones()
            == 1
    {
        unsafe { await_request_submit(to, from) }
    } else {
        unsafe { emit_semaphore_wait(to, from, I915_FENCE_GFP) }
    }
}

// upstream i915_request.c:1500
// upstream: i915_request.c i915_request_await_dma_fence()
pub unsafe fn i915_request_await_dma_fence(rq: *mut I915Request, fence: *mut DmaFence) -> i32 {
    let mut child = ptr::addr_of_mut!(fence);
    let mut nchild = 1u32;
    if unsafe { dma_fence_is_array(fence) } {
        let array = fence.cast::<DmaFenceArrayView>();
        child = unsafe { (*array).fences };
        nchild = unsafe { (*array).num_fences };
        gem_bug_on!(nchild == 0);
    }
    while nchild != 0 {
        let current = unsafe { ptr::read(child) };
        child = unsafe { child.add(1) };
        nchild -= 1;
        if test_bit(DMA_FENCE_FLAG_SIGNALED_BIT, unsafe { &(*current).flags })
            || unsafe { (*current).context == (*rq).fence.context }
        {
            continue;
        }
        if unsafe {
            (*current).context != 0
                && intel_timeline_sync_is_later(i915_request_timeline(rq), current)
        } {
            continue;
        }
        let ret = if unsafe { is_i915_fence(current) } {
            if unsafe { is_same_parallel_context(rq, to_request(current)) } {
                continue;
            }
            unsafe { i915_request_await_request(rq, to_request(current)) }
        } else {
            unsafe { i915_request_await_external(rq, current) }
        };
        if ret < 0 {
            return ret;
        }
        if unsafe { (*current).context != 0 } {
            unsafe { intel_timeline_sync_set(i915_request_timeline(rq), current) };
        }
    }
    0
}

// upstream i915_request.c:1602
// upstream: i915_request.c i915_request_await_deps()
pub unsafe fn i915_request_await_deps(rq: *mut I915Request, deps: *const I915Deps) -> i32 {
    let deps = deps.cast::<I915DepsView>();
    let count = unsafe { (*deps).num_deps };
    for index in 0..count {
        let fence = unsafe { *(*deps).fences.add(index as usize) };
        let err = unsafe { i915_request_await_dma_fence(rq, fence) };
        if err != 0 {
            return err;
        }
    }
    0
}

// upstream i915_request.c:1612
// upstream: i915_request.c i915_request_await_object()
pub unsafe fn i915_request_await_object(
    to: *mut I915Request,
    obj: *mut DrmI915GemObject,
    write: bool,
) -> i32 {
    // The i915 object begins with `drm_gem_object`; use the shared source-layout
    // view rather than a target-specific hard-coded byte offset.
    let base = obj.cast::<DrmGemObjectBaseLayout>();
    let resv = unsafe { (*base).resv.cast::<DmaResv>() };
    let mut cursor: DmaResvIter = unsafe { core::mem::zeroed() };
    cursor.obj = resv;
    cursor.usage = if write { 2 } else { 1 }; // READ for a write; WRITE for a read.
    let mut ret = 0;
    loop {
        let fence = unsafe { dma_resv_iter_next(&mut cursor) };
        if fence.is_null() {
            break;
        }
        ret = unsafe { i915_request_await_dma_fence(to, fence) };
        if ret != 0 {
            break;
        }
    }
    unsafe { dma_fence_put(cursor.fence) };
    ret
}

// upstream i915_request.c:1621
// upstream: i915_request.c i915_request_await_huc()
unsafe fn i915_request_await_huc(rq: *mut I915Request) {
    let huc = unsafe { &mut (*(*(*(*rq).context).engine).gt).uc.huc };
    if unsafe { (*(*rq).context).gem_context.is_null() } {
        return;
    }
    if unsafe { intel_huc_wait_required(huc) } {
        unsafe {
            i915_sw_fence_await_sw_fence(
                &mut (*rq).submit,
                &mut (*huc).delayed_load.fence,
                &mut (*rq).hucq,
            )
        };
    }
}

// upstream i915_request.c:1635
// upstream: i915_request.c __i915_request_ensure_parallel_ordering()
unsafe fn __i915_request_ensure_parallel_ordering(
    rq: *mut I915Request,
    timeline: *mut IntelTimeline,
) -> *mut I915Request {
    gem_bug_on!(!unsafe { is_parallel_rq(rq) });
    let parent = unsafe { request_to_parent(rq) };
    let prev = unsafe { (*parent).parallel.last_rq };
    if !prev.is_null() {
        if !unsafe { __i915_request_is_complete(prev) } {
            unsafe {
                i915_sw_fence_await_sw_fence(
                    &mut (*rq).submit,
                    &mut (*prev).submit,
                    core::ptr::addr_of_mut!((*rq).submit_union.submitq).cast::<WaitQueueEntry>(),
                )
            };
            if unsafe { (*(*(*rq).engine).sched_engine).schedule.is_some() } {
                unsafe {
                    __i915_sched_node_add_dependency(
                        &mut (*rq).sched,
                        &mut (*prev).sched,
                        &mut (*rq).dep,
                        0,
                    )
                };
            }
        }
        unsafe { i915_request_put(prev) };
    }
    unsafe { (*parent).parallel.last_rq = i915_request_get(rq) };
    let fence = unsafe { __i915_active_fence_set(&mut (*timeline).last_request, &mut (*rq).fence) };
    unsafe { to_request(fence) }
}

// upstream i915_request.c:1670
// upstream: i915_request.c __i915_request_ensure_ordering()
unsafe fn __i915_request_ensure_ordering(
    rq: *mut I915Request,
    timeline: *mut IntelTimeline,
) -> *mut I915Request {
    gem_bug_on!(unsafe { is_parallel_rq(rq) });
    let fence = unsafe { __i915_active_fence_set(&mut (*timeline).last_request, &mut (*rq).fence) };
    let prev = if fence.is_null() {
        ptr::null_mut()
    } else {
        unsafe { to_request(fence) }
    };
    if !prev.is_null() && !unsafe { __i915_request_is_complete(prev) } {
        let uses_guc = unsafe { intel_engine_uses_guc((*rq).engine) };
        let pow2 = (unsafe { ptr::read_volatile(ptr::addr_of!((*prev).engine)) }
            .as_ref()
            .unwrap()
            .mask
            | unsafe { (*(*rq).engine).mask })
        .count_ones()
            == 1;
        let same_context = unsafe { (*prev).context == (*rq).context };
        gem_bug_on!(
            same_context
                && i915_seqno_passed(unsafe { (*prev).fence.seqno as u32 }, unsafe { (*rq).fence.seqno as u32 })
        );
        if (same_context && uses_guc) || (!uses_guc && pow2) {
            unsafe {
                i915_sw_fence_await_sw_fence(
                    &mut (*rq).submit,
                    &mut (*prev).submit,
                    core::ptr::addr_of_mut!((*rq).submit_union.submitq).cast::<WaitQueueEntry>(),
                )
            };
        } else {
            unsafe {
                __i915_sw_fence_await_dma_fence(
                    &mut (*rq).submit,
                    &mut (*prev).fence,
                    core::ptr::addr_of_mut!((*rq).submit_union.dmaq).cast::<I915SwDmaFenceCb>(),
                )
            };
        }
        if unsafe { (*(*(*rq).engine).sched_engine).schedule.is_some() } {
            unsafe {
                __i915_sched_node_add_dependency(
                    &mut (*rq).sched,
                    &mut (*prev).sched,
                    &mut (*rq).dep,
                    0,
                )
            };
        }
    }
    prev
}

// upstream i915_request.c:1719
// upstream: i915_request.c __i915_request_add_to_timeline()
unsafe fn __i915_request_add_to_timeline(rq: *mut I915Request) -> *mut I915Request {
    let timeline = unsafe { i915_request_timeline(rq) };
    if unsafe { (*(*rq).engine).class as i32 == VIDEO_DECODE_CLASS } {
        unsafe { i915_request_await_huc(rq) };
    }
    let prev = if unsafe { is_parallel_rq(rq) } {
        unsafe { __i915_request_ensure_parallel_ordering(rq, timeline) }
    } else {
        unsafe { __i915_request_ensure_ordering(rq, timeline) }
    };
    if !prev.is_null() {
        unsafe { i915_request_put(prev) };
    }
    gem_bug_on!(unsafe { (*timeline).seqno != (*rq).fence.seqno });
    prev
}

// upstream i915_request.c:1787
// upstream: i915_request.c __i915_request_commit()
pub unsafe fn __i915_request_commit(rq: *mut I915Request) -> *mut I915Request {
    let engine = unsafe { (*rq).engine };
    let ring = unsafe { (*rq).ring };
    gem_bug_on!(unsafe { (*rq).reserved_space > (*ring).space });
    unsafe { (*rq).reserved_space = 0 };
    unsafe { (*rq).emitted_jiffies = jiffies() };
    let dw = unsafe { (*engine).emit_fini_breadcrumb_dw };
    let cs = unsafe { intel_ring_begin(rq, dw) };
    gem_bug_on!(IS_ERR(cs));
    unsafe { (*rq).postfix = intel_ring_offset(rq, cs.cast::<c_void>()) };
    unsafe { __i915_request_add_to_timeline(rq) }
}

// upstream i915_request.c:1817
// upstream: i915_request.c __i915_request_queue_bh()
pub unsafe fn __i915_request_queue_bh(rq: *mut I915Request) {
    unsafe { i915_sw_fence_commit(&mut (*rq).semaphore) };
    unsafe { i915_sw_fence_commit(&mut (*rq).submit) };
}

// upstream i915_request.c:1823
// upstream: i915_request.c __i915_request_queue()
unsafe fn __i915_request_queue(rq: *mut I915Request, attr: *const I915SchedAttr) {
    let engine = unsafe { (*rq).engine };
    if !attr.is_null() {
        if let Some(schedule) = unsafe { (*(*engine).sched_engine).schedule } {
            unsafe { schedule(rq, attr) };
        }
    }
    local_bh_disable();
    unsafe { __i915_request_queue_bh(rq) };
    local_bh_enable();
}

// upstream i915_request.c:1845
// upstream: i915_request.c i915_request_add()
pub unsafe fn i915_request_add(rq: *mut I915Request) {
    let tl = unsafe { i915_request_timeline(rq) };
    lockdep_assert_held(unsafe { &(*tl).mutex });
    unsafe { lockdep_unpin_lock(&mut (*tl).mutex, (*rq).cookie) };
    unsafe { __i915_request_commit(rq) };
    let mut attr: I915SchedAttr = unsafe { core::mem::zeroed() };
    rcu_read_lock();
    let ctx = unsafe { rcu_dereference!((*(*rq).context).gem_context) };
    if !ctx.is_null() {
        attr = unsafe { (*ctx).sched };
    }
    rcu_read_unlock();
    unsafe { __i915_request_queue(rq, &attr) };
    unsafe { mutex_unlock(&mut (*tl).mutex) };
}

// upstream i915_request.c:1869
// upstream: i915_request.c local_clock_ns()
fn local_clock_ns(cpu: &mut u32) -> u64 {
    *cpu = axhal::percpu::this_cpu_id() as u32;
    axhal::time::monotonic_time_nanos() as u64
}

// upstream i915_request.c:1897
// upstream: i915_request.c busywait_stop()
fn busywait_stop(timeout: u64, cpu: u32) -> bool {
    let mut this_cpu = 0;
    if local_clock_ns(&mut this_cpu).wrapping_sub(timeout) as i64 >= 0 {
        return true;
    }
    this_cpu != cpu
}

unsafe extern "C" {
    fn signal_pending_state(state: i32, task: *mut c_void) -> bool;
    fn io_schedule_timeout(timeout: c_long) -> c_long;
    fn wake_up_process(task: *mut c_void) -> i32;
    fn set_current_state(state: i32);
    fn __set_current_state(state: i32);
    fn need_resched() -> bool;
    fn cpu_relax();
    fn scnprintf(buf: *mut c_char, size: usize, fmt: *const c_char, ...) -> i32;
    fn trace_i915_request_wait_begin(rq: *mut I915Request, flags: u32);
    fn trace_i915_request_wait_end(rq: *mut I915Request);
    fn intel_rps_boost(rq: *mut I915Request);
}

// upstream i915_request.c:1925
// upstream: i915_request.c __i915_spin_request()
unsafe fn __i915_spin_request(rq: *const I915Request, state: i32) -> bool {
    if !unsafe { i915_request_is_running_local(rq) } {
        return false;
    }
    let mut cpu = 0;
    let timeout = local_clock_ns(&mut cpu)
        .wrapping_add(unsafe { (*(*rq).engine).props.max_busywait_duration_ns });
    loop {
        if unsafe { dma_fence_is_signaled(ptr::addr_of!((*rq).fence) as *mut DmaFence) } {
            return true;
        }
        if unsafe { signal_pending_state(state as i32, current_task_ptr()) } || busywait_stop(timeout, cpu)
        {
            break;
        }
        unsafe { cpu_relax() };
        if unsafe { need_resched() } {
            break;
        }
    }
    false
}

unsafe fn i915_request_is_running_local(rq: *const I915Request) -> bool {
    if !i915_request_is_active(unsafe { &*rq }) {
        return false;
    }
    rcu_read_lock();
    let running =
        unsafe { __i915_request_has_started(rq) } && i915_request_is_active(unsafe { &*rq });
    rcu_read_unlock();
    running
}

#[repr(C)]
struct RequestWait {
    cb: DmaFenceCb,
    tsk: *mut c_void,
}

fn current_task_ptr() -> *mut c_void {
    axhal::percpu::current_task_ptr::<()>().cast_mut().cast::<c_void>()
}

// upstream i915_request.c:1958
// upstream: i915_request.c request_wait_wake()
unsafe extern "C" fn request_wait_wake(_fence: *mut DmaFence, cb: *mut DmaFenceCb) {
    let wait = unsafe {
        cb.cast::<u8>()
            .sub(offset_of!(RequestWait, cb))
            .cast::<RequestWait>()
    };
    let task = unsafe { ptr::replace(&mut (*wait).tsk, ptr::null_mut()) };
    if !task.is_null() {
        unsafe { wake_up_process(task) };
    }
}

// upstream i915_request.c:1981
// upstream: i915_request.c i915_request_wait_timeout()
pub unsafe fn i915_request_wait_timeout(
    rq: *mut I915Request,
    flags: u32,
    mut timeout: c_long,
) -> c_long {
    let state = if flags & I915_WAIT_INTERRUPTIBLE != 0 {
        TASK_INTERRUPTIBLE
    } else {
        TASK_UNINTERRUPTIBLE
    };
    let mut wait = RequestWait {
        cb: unsafe { core::mem::zeroed() },
        tsk: ptr::null_mut(),
    };
    might_sleep();
    gem_bug_on!(timeout < 0);
    if unsafe { dma_fence_is_signaled(&mut (*rq).fence) } {
        return if timeout == 0 { 1 } else { timeout };
    }
    if timeout == 0 {
        return -(ETIME as c_long);
    }
    unsafe { trace_i915_request_wait_begin(rq, flags) };
    // The source lockdep acquire/release annotations are compiled out when
    // CONFIG_LOCKDEP=n in this target configuration.
    if CONFIG_DRM_I915_MAX_REQUEST_BUSYWAIT != 0 && unsafe { __i915_spin_request(rq, state as i32) } {
        unsafe { trace_i915_request_wait_end(rq) };
        return timeout;
    }
    if flags & I915_WAIT_PRIORITY != 0 && !i915_request_started(unsafe { &*rq }) {
        unsafe { intel_rps_boost(rq) };
    }
    wait.tsk = current_task_ptr();
    if unsafe { dma_fence_add_callback(&mut (*rq).fence, &mut wait.cb, request_wait_wake) } != 0 {
        unsafe { trace_i915_request_wait_end(rq) };
        return timeout;
    }
    if i915_request_is_ready(unsafe { &*rq }) {
        unsafe { __intel_engine_flush_submission((*rq).engine, false) };
    }
    loop {
        unsafe { set_current_state(state as i32) };
        if unsafe { dma_fence_is_signaled(&mut (*rq).fence) } {
            break;
        }
        if unsafe { signal_pending_state(state as i32, current_task_ptr()) } {
            timeout = -(ERESTARTSYS as c_long);
            break;
        }
        if timeout == 0 {
            timeout = -(ETIME as c_long);
            break;
        }
        timeout = unsafe { io_schedule_timeout(timeout) };
    }
    unsafe { __set_current_state(TASK_RUNNING as i32) };
    if !wait.tsk.is_null() {
        unsafe { dma_fence_remove_callback(&mut (*rq).fence, &mut wait.cb) };
    }
    unsafe { trace_i915_request_wait_end(rq) };
    timeout
}

// upstream i915_request.c:2124
// upstream: i915_request.c i915_request_wait()
pub unsafe fn i915_request_wait(rq: *mut I915Request, flags: u32, timeout: c_long) -> c_long {
    let ret = unsafe { i915_request_wait_timeout(rq, flags, timeout) };
    if ret == 0 {
        return -(ETIME as c_long);
    }
    if ret > 0 && timeout == 0 {
        return 0;
    }
    ret
}

// upstream i915_request.c:2140
// upstream: i915_request.c print_sched_attr()
unsafe fn print_sched_attr(attr: *const I915SchedAttr, buf: *mut c_char, x: i32, len: i32) -> i32 {
    if unsafe { (*attr).priority == I915_PRIORITY_INVALID } {
        return x;
    }
    (unsafe {
        scnprintf(
            buf.add(x as usize),
            (len - x) as usize,
            b" prio=%d\0".as_ptr().cast(),
            (*attr).priority,
        )
    }) + x
}

// upstream i915_request.c:2147
// upstream: i915_request.c queue_status()
unsafe fn queue_status(rq: *const I915Request) -> c_char {
    if i915_request_is_active(unsafe { &*rq }) {
        return b'E' as c_char;
    }
    if i915_request_is_ready(unsafe { &*rq }) {
        return if unsafe { intel_engine_is_virtual((*rq).engine) } {
            b'V' as c_char
        } else {
            b'R' as c_char
        };
    }
    b'U' as c_char
}

// upstream i915_request.c:2158
// upstream: i915_request.c run_status()
unsafe fn run_status(rq: *const I915Request) -> *const c_char {
    if unsafe { __i915_request_is_complete(rq) } {
        return b"!\0".as_ptr().cast();
    }
    if unsafe { __i915_request_has_started(rq) } {
        return b"*\0".as_ptr().cast();
    }
    if !i915_sw_fence_signaled(unsafe { &(*rq).semaphore }) {
        return b"&\0".as_ptr().cast();
    }
    b"\0".as_ptr().cast()
}

// upstream i915_request.c:2172
// upstream: i915_request.c fence_status()
unsafe fn fence_status(rq: *const I915Request) -> *const c_char {
    if test_bit(DMA_FENCE_FLAG_SIGNALED_BIT, unsafe { &(*rq).fence.flags }) {
        return b"+\0".as_ptr().cast();
    }
    if test_bit(DMA_FENCE_FLAG_ENABLE_SIGNAL_BIT, unsafe {
        &(*rq).fence.flags
    }) {
        return b"-\0".as_ptr().cast();
    }
    b"\0".as_ptr().cast()
}

// upstream i915_request.c:2180
// upstream: i915_request.c i915_request_show()
pub unsafe fn i915_request_show(
    printer: *mut DrmPrinter,
    rq: *const I915Request,
    prefix: *const c_char,
    indent: i32,
) {
    let mut buf = [0i8; 80];
    let x = unsafe { print_sched_attr(&(*rq).sched.attr, buf.as_mut_ptr(), 0, buf.len() as i32) };
    let timeline_name = unsafe { dma_fence_timeline_name(ptr::addr_of!((*rq).fence) as *mut DmaFence) };
    rcu_read_lock();
    drm_printf!(
        printer,
        "%s%.*s%c %llx:%lld%s%s %s @ %dms: %s\n",
        prefix,
        indent,
        "                ",
        unsafe { queue_status(rq) },
        unsafe { (*rq).fence.context },
        unsafe { (*rq).fence.seqno },
        unsafe { run_status(rq) },
        unsafe { fence_status(rq) },
        buf.as_ptr(),
        jiffies_to_msecs(jiffies().wrapping_sub(unsafe { (*rq).emitted_jiffies })),
        unsafe { rcu_dereference!(timeline_name) },
    );
    rcu_read_unlock();
    let _ = x;
}

// upstream i915_request.c:2245
// upstream: i915_request.c engine_match_ring()
unsafe fn engine_match_ring(engine: *mut IntelEngineCs, rq: *mut I915Request) -> bool {
    let reg = crate::intel_engine_regs_upstream::RING_START(unsafe { (*engine).mmio_base });
    let ring_start =
        unsafe { crate::intel_uncore_types_upstream::intel_uncore_read((*engine).uncore, reg) };
    ring_start == unsafe { crate::linux::i915::i915_ggtt_offset((*(*rq).ring).vma) }
}

// upstream i915_request.c:2251
// upstream: i915_request.c match_ring()
unsafe fn match_ring(rq: *mut I915Request) -> bool {
    let engine = unsafe { (*rq).engine };
    if !unsafe { intel_engine_is_virtual(engine) } {
        return unsafe { engine_match_ring(engine, rq) };
    }
    let mut i = 0;
    loop {
        let sibling = unsafe { intel_engine_get_sibling(engine, i) };
        if sibling.is_null() {
            break;
        }
        if unsafe { engine_match_ring(sibling, rq) } {
            return true;
        }
        i += 1;
    }
    false
}

// upstream i915_request.c:2265
// upstream: i915_request.c i915_test_request_state()
pub unsafe fn i915_test_request_state(rq: *mut I915Request) -> I915RequestState {
    if i915_request_completed(unsafe { &*rq }) {
        return I915RequestState::Complete;
    }
    if !i915_request_started(unsafe { &*rq }) {
        return I915RequestState::Pending;
    }
    if unsafe { match_ring(rq) } {
        return I915RequestState::Active;
    }
    I915RequestState::Queued
}

// upstream i915_request.c:2284
// upstream: i915_request.c i915_request_module_exit()
pub unsafe fn i915_request_module_exit() {
    unsafe { kmem_cache_destroy(SLAB_EXECUTE_CBS) };
    unsafe { kmem_cache_destroy(SLAB_REQUESTS) };
}

// upstream i915_request.c:2290
// upstream: i915_request.c i915_request_module_init()
pub unsafe fn i915_request_module_init() -> i32 {
    SLAB_REQUESTS = unsafe {
        kmem_cache_create::<I915Request>(
            SLAB_HWCACHE_ALIGN | SLAB_RECLAIM_ACCOUNT | SLAB_TYPESAFE_BY_RCU,
        )
    };
    if unsafe { SLAB_REQUESTS.is_null() } {
        return -ENOMEM;
    }
    SLAB_EXECUTE_CBS = unsafe {
        kmem_cache_create::<ExecuteCb>(
            SLAB_HWCACHE_ALIGN | SLAB_RECLAIM_ACCOUNT | SLAB_TYPESAFE_BY_RCU,
        )
    };
    if unsafe { SLAB_EXECUTE_CBS.is_null() } {
        unsafe { kmem_cache_destroy(SLAB_REQUESTS) };
        return -ENOMEM;
    }
    0
}
