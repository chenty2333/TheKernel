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
        DrmI915GemObject, DrmPrinter, I915CaptureList, I915Request, I915RequestState,
        I915RequestWatchdog,
    },
    i915_scheduler_types_upstream::{I915Dependency, I915SchedNode},
    intel_breadcrumbs_upstream::{
        i915_request_cancel_breadcrumb, i915_request_enable_breadcrumb,
    },
    intel_context_types_upstream::{I915SwFence, IntelContext},
    intel_context_upstream::{
        DmaFence, DmaFenceCb, I915Vma, I915VmaResource, IntelRing, IntelTimeline, IrqWork,
        WaitQueueEntry,
    },
    intel_engine_cs_upstream::{IntelEngineCs, Spinlock},
    intel_engine_types_upstream::{IntelEngineMask, IntelEngineMaskT},
    intel_gt_types_upstream::IntelGt,
    intel_timeline_upstream::{
        intel_timeline_get_seqno, intel_timeline_sync_is_later, intel_timeline_sync_set,
    },
    intel_workarounds_types_upstream::I915RegT,
    linux::{
        bits::*, contexts::*, fields::*, irq::*, list::*, memory::*, mutex::*, pm::*,
        requests::*, rbtree::*, rcu::*, sw_fence::*, tasklet::*, timer::*, wait::*, workqueue::*,
    },
    linux_config::*,
    linux_heap::{kmem_cache_alloc, kmem_cache_free, KmCache},
    linux_i915_private::DrmI915Private,
    linux_print::DrmPrinter as _,
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
const I915_FENCE_GFP: u32 = GFP_ATOMIC;
const NOTIFY_DONE: i32 = 0;
const EAGAIN: i32 = 11;
const ETIMEDOUT: i32 = 110;
const ENOMEM: i32 = 12;
const EIO: i32 = 5;

#[inline]
unsafe fn to_request(fence: *mut DmaFence) -> *mut I915Request {
    fence.cast()
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
unsafe extern "C" fn i915_fence_get_driver_name(fence: *mut DmaFence) -> *const c_char {
    unsafe { i915_device_name((*to_request(fence)).i915) }
}

// upstream i915_request.c:67
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
unsafe extern "C" fn i915_fence_signaled(fence: *mut DmaFence) -> bool {
    i915_request_completed(unsafe { &*to_request(fence) })
}

// upstream i915_request.c:95
unsafe extern "C" fn i915_fence_enable_signaling(fence: *mut DmaFence) -> bool {
    unsafe { i915_request_enable_breadcrumb(to_request(fence)) }
}

// upstream i915_request.c:100
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
pub unsafe fn i915_request_slab_cache() -> *mut KmCache {
    unsafe { SLAB_REQUESTS }
}

// Source out-of-line Linux services that are not implemented by this file.
unsafe extern "C" {
    fn i915_vma_resource_put(resource: *mut I915VmaResource);
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
}

// upstream i915_request.c:114
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

pub static I915_FENCE_OPS: DmaFenceOps = DmaFenceOps {
    get_driver_name: Some(i915_fence_get_driver_name),
    get_timeline_name: Some(i915_fence_get_timeline_name),
    enable_signaling: Some(i915_fence_enable_signaling),
    signaled: Some(i915_fence_signaled),
    wait: Some(i915_fence_wait),
    release: Some(i915_fence_release),
    set_deadline: None,
};

// upstream i915_request.c:184
unsafe extern "C" fn irq_execute_cb(wrk: *mut IrqWork) {
    let cb = unsafe { wrk.cast::<u8>().sub(offset_of!(ExecuteCb, work)).cast::<ExecuteCb>() };
    unsafe { i915_sw_fence_complete((*cb).fence) };
    unsafe { kmem_cache_free(SLAB_EXECUTE_CBS, cb.cast()) };
}

// upstream i915_request.c:192
unsafe fn __notify_execute_cb(rq: *mut I915Request, queue: fn(*mut IrqWork) -> bool) {
    if unsafe { llist_empty(&(*rq).execute_cb) } {
        return;
    }
    let mut node = unsafe { llist_del_all(&mut (*rq).execute_cb) };
    while !node.is_null() {
        let next = unsafe { (*node).next };
        let work = unsafe { node.cast::<u8>().sub(offset_of!(ExecuteCb, work) + offset_of!(IrqWork, node)).cast::<IrqWork>() };
        let _ = queue(work);
        node = next;
    }
}

// upstream i915_request.c:206
fn irq_work_queue_adapter(work: *mut IrqWork) -> bool {
    crate::linux::irq::irq_work_queue(work)
}

unsafe fn __notify_execute_cb_irq(rq: *mut I915Request) {
    unsafe { __notify_execute_cb(rq, irq_work_queue_adapter) };
}

// upstream i915_request.c:211
unsafe fn irq_work_imm(wrk: *mut IrqWork) -> bool {
    if let Some(func) = unsafe { (*wrk).func } {
        unsafe { func(wrk) };
    }
    false
}

// upstream i915_request.c:217
pub unsafe fn i915_request_notify_execute_cb_imm(rq: *mut I915Request) {
    unsafe { __notify_execute_cb(rq, irq_work_imm) };
}

// upstream i915_request.c:224
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
unsafe extern "C" fn __rq_watchdog_expired(timer: *mut crate::intel_context_upstream::Hrtimer) -> i32 {
    let rq = unsafe {
        timer.cast::<u8>().sub(offset_of!(I915RequestWatchdog, timer)).sub(offset_of!(I915Request, watchdog)).cast::<I915Request>()
    };
    let gt = unsafe { (*(*rq).engine).gt };
    if !i915_request_completed(unsafe { &*rq }) {
        if unsafe { llist_add(&mut (*rq).watchdog.link, &mut (*gt).watchdog.list) } {
            unsafe { queue_work((*(*gt).i915).unordered_wq.cast(), &mut (*gt).watchdog.work) };
        }
    } else {
        unsafe { i915_request_put(rq) };
    }
    HRTIMER_NORESTART
}

// upstream i915_request.c:294
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
unsafe fn __rq_cancel_watchdog(rq: *mut I915Request) {
    if unsafe { hrtimer_try_to_cancel(&mut (*rq).watchdog.timer) } > 0 {
        unsafe { i915_request_put(rq) };
    }
}

// upstream i915_request.c:333 (CONFIG_DRM_I915_CAPTURE_ERROR=y)
pub unsafe fn i915_request_free_capture_list(mut capture: *mut I915CaptureList) {
    while !capture.is_null() {
        let next = unsafe { (*capture).next };
        unsafe { i915_vma_resource_put((*capture).vma_res) };
        unsafe { kfree(capture) };
        capture = next;
    }
}

// upstream i915_request.c:835
unsafe fn request_alloc_slow(
    tl: *mut IntelTimeline,
    reserved: *mut *mut I915Request,
    gfp: u32,
) -> *mut I915Request {
    if !gfpflags_allow_blocking(gfp) {
        let rq = unsafe { ptr::replace(reserved, ptr::null_mut()) };
        if !rq.is_null() {
            return rq;
        }
        return unsafe { kmem_cache_alloc(SLAB_REQUESTS, gfp) }.cast();
    }
    if !unsafe { list_empty(&(*tl).requests) } {
        let oldest = unsafe { list_first_entry::<I915Request>(&(*tl).requests, offset_of!(I915Request, link)) };
        unsafe { i915_request_retire(oldest) };
        let rq = unsafe { kmem_cache_alloc(SLAB_REQUESTS, gfp | __GFP_RETRY_MAYFAIL | __GFP_NOWARN) }.cast();
        if !rq.is_null() {
            return rq;
        }
        let last = unsafe { list_last_entry::<I915Request>(&(*tl).requests, offset_of!(I915Request, link)) };
        unsafe { cond_synchronize_rcu((*last).rcustate) };
        unsafe { retire_requests(tl) };
    }
    unsafe { kmem_cache_alloc(SLAB_REQUESTS, gfp) }.cast()
}

// upstream i915_request.c:874
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
        unsafe { (*rq).batch = ptr::null_mut() };
    }
}

// upstream i915_request.c:895
pub unsafe fn __i915_request_create(ce: *mut IntelContext, gfp: u32) -> *mut I915Request {
    let tl = unsafe { (*ce).timeline };
    let mut rq = unsafe {
        kmem_cache_alloc(SLAB_REQUESTS, gfp | __GFP_RETRY_MAYFAIL | __GFP_NOWARN)
    }
    .cast::<I915Request>();
    let mut seqno = 0;
    let mut ret = 0;
    unsafe { intel_context_pin(ce) };
    if rq.is_null() {
        rq = unsafe { request_alloc_slow(tl, &mut (*(*(*ce).engine).request_pool), gfp) };
        if rq.is_null() {
            ret = -ENOMEM;
            unsafe { intel_context_unpin(ce) };
            return ret as isize as *mut I915Request;
        }
    }
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
            &I915_FENCE_OPS,
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
        (*rq).reserved_space = 2 * (*(*ce).engine).emit_fini_breadcrumb_dw * size_of::<u32>() as u32;
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
pub unsafe fn i915_request_create(ce: *mut IntelContext) -> *mut I915Request {
    let tl = unsafe { intel_context_timeline_lock(ce) };
    if IS_ERR(tl) {
        return tl.cast();
    }
    let first = unsafe { list_first_entry::<I915Request>(&(*tl).requests, offset_of!(I915Request, link)) };
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
unsafe fn i915_request_await_start(rq: *mut I915Request, signal: *mut I915Request) -> i32 {
    if unsafe { i915_request_timeline(rq) == (*signal).timeline } || i915_request_started(unsafe { &*signal }) {
        return 0;
    }
    let mut fence: *mut DmaFence = ptr::null_mut();
    rcu_read_lock();
    let pos = unsafe { ptr::read_volatile((*signal).link.prev) };
    if !unsafe { __i915_request_has_started(signal) } {
        let timeline = unsafe { rcu_dereference((*signal).timeline) };
        if pos != unsafe { ptr::addr_of_mut!((*timeline).requests) } {
            let prev = unsafe { list_entry::<I915Request>(pos, offset_of!(I915Request, link)) };
            if unsafe { i915_request_get_rcu(prev) } {
                if unsafe { ptr::read_volatile((*prev).link.next) == ptr::addr_of_mut!((*signal).link) } {
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
        unsafe { i915_sw_fence_await_dma_fence(&mut (*rq).submit, fence, 0, I915_FENCE_GFP) }
    } else {
        0
    };
    unsafe { dma_fence_put(fence) };
    err
}

// upstream i915_request.c:1126
unsafe fn already_busywaiting(rq: *mut I915Request) -> IntelEngineMask {
    unsafe { (*rq).sched.semaphores | ptr::read_volatile(ptr::addr_of!((*(*rq).engine).saturated)) }
}

// upstream i915_request.c:1144
unsafe fn __emit_semaphore_wait(to: *mut I915Request, from: *mut I915Request, seqno: u32) -> i32 {
    let has_token = unsafe { GRAPHICS_VER((*(*to).engine).i915) >= 12 };
    gem_bug_on!(unsafe { GRAPHICS_VER((*(*to).engine).i915) < 8 });
    gem_bug_on!(i915_request_has_initial_breadcrumb(unsafe { &*to }));
    let mut hwsp_offset = 0;
    let err = unsafe { intel_timeline_read_hwsp(from, to, &mut hwsp_offset) };
    if err != 0 { return err; }
    let len = if has_token { 6 } else { 4 };
    let cs = unsafe { intel_ring_begin(to, len) };
    if IS_ERR(cs) { return PTR_ERR(cs); }
    unsafe {
        *cs.add(0) = MI_SEMAPHORE_WAIT | MI_SEMAPHORE_GLOBAL_GTT | MI_SEMAPHORE_POLL |
            MI_SEMAPHORE_SAD_GTE_SDD + has_token as u32;
        *cs.add(1) = seqno;
        *cs.add(2) = hwsp_offset;
        *cs.add(3) = 0;
        if has_token { *cs.add(4) = 0; *cs.add(5) = MI_NOOP; }
        intel_ring_advance(to, cs.add(len as usize));
    }
    0
}

// upstream i915_request.c:1195
unsafe fn can_use_semaphore_wait(to: *mut I915Request, from: *mut I915Request) -> bool {
    unsafe { (*(*(*to).engine).gt).ggtt == (*(*(*from).engine).gt).ggtt }
}

// upstream i915_request.c:1201
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
        && unsafe { __emit_semaphore_wait(to, from, (*from).fence.seqno) == 0 }
    {
        unsafe { (*to).sched.semaphores |= mask };
        wait = ptr::addr_of_mut!((*to).semaphore);
    }
    unsafe { i915_sw_fence_await_dma_fence(wait, &mut (*from).fence, 0, I915_FENCE_GFP) }
}

// upstream i915_request.c:1264
unsafe fn __i915_request_await_execution(to: *mut I915Request, from: *mut I915Request) -> i32 {
    gem_bug_on!(intel_context_is_barrier(unsafe { (*from).context }));
    let err = unsafe { __await_execution(to, from, I915_FENCE_GFP) };
    if err != 0 { return err; }
    if unsafe { intel_timeline_sync_has_start(i915_request_timeline(to), &(*from).fence) } {
        return 0;
    }
    let err = unsafe { i915_request_await_start(to, from) };
    if err < 0 { return err; }
    if unsafe { can_use_semaphore_wait(to, from) && intel_engine_has_semaphores((*to).engine) }
        && !i915_request_has_initial_breadcrumb(unsafe { &*to })
    {
        let err = unsafe { __emit_semaphore_wait(to, from, (*from).fence.seqno - 1) };
        if err < 0 { return err; }
    }
    if unsafe { (*(*(*to).engine).sched_engine).schedule.is_some() } {
        let err = unsafe { i915_sched_node_add_dependency(&mut (*to).sched, &mut (*from).sched, I915_DEPENDENCY_WEAK) };
        if err < 0 { return err; }
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

unsafe extern "C" {
    fn dma_fence_get(fence: *mut DmaFence) -> *mut DmaFence;
    fn dma_fence_put(fence: *mut DmaFence);
    fn dma_fence_is_array(fence: *const DmaFence) -> bool;
    fn dma_fence_is_chain(fence: *const DmaFence) -> bool;
    fn dma_fence_chain_walk(fence: *mut DmaFence) -> *mut DmaFence;
    fn dma_fence_context_timeout(context: u64) -> c_ulong;
    fn dma_fence_add_callback(
        fence: *mut DmaFence,
        cb: *mut DmaFenceCb,
        callback: unsafe extern "C" fn(*mut DmaFence, *mut DmaFenceCb),
    ) -> i32;
    fn dma_fence_remove_callback(fence: *mut DmaFence, cb: *mut DmaFenceCb) -> bool;
    fn dma_fence_is_signaled(fence: *mut DmaFence) -> bool;
    fn dma_fence_timeline_name(fence: *mut DmaFence) -> *const *const c_char;
    fn intel_engine_flush_submission(engine: *mut IntelEngineCs, do_lock: bool);
    fn i915_request_is_running(rq: *const I915Request) -> bool;
    fn intel_engine_is_virtual(engine: *const IntelEngineCs) -> bool;
    fn intel_engine_get_sibling(engine: *mut IntelEngineCs, index: i32) -> *mut IntelEngineCs;
    fn i915_ggtt_offset(vma: *mut I915Vma) -> u32;
    fn intel_engine_read(engine: *mut IntelEngineCs, reg: I915RegT) -> u32;
    fn i915_request_trace_wait_begin(rq: *mut I915Request, flags: u32);
    fn i915_request_trace_wait_end(rq: *mut I915Request);
    fn intel_rps_boost(rq: *mut I915Request);
}

unsafe fn is_i915_fence(fence: *mut DmaFence) -> bool {
    unsafe { (*fence).ops == ptr::addr_of!(I915_FENCE_OPS) }
}

// upstream i915_request.c:1336
unsafe fn mark_external(rq: *mut I915Request) {
    unsafe { (*rq).sched.flags |= I915_SCHED_HAS_EXTERNAL_CHAIN };
}

// upstream i915_request.c:1349
unsafe fn __i915_request_await_external(rq: *mut I915Request, fence: *mut DmaFence) -> i32 {
    unsafe { mark_external(rq) };
    unsafe {
        i915_sw_fence_await_dma_fence(
            &mut (*rq).submit,
            fence,
            dma_fence_context_timeout((*fence).context),
            I915_FENCE_GFP,
        )
    }
}

// upstream i915_request.c:1358
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
        if err < 0 { break; }
        iter = unsafe { dma_fence_chain_walk(iter) };
    }
    unsafe { dma_fence_put(iter) };
    err
}

// upstream i915_request.c:1384
unsafe fn is_parallel_rq(rq: *mut I915Request) -> bool {
    unsafe { intel_context_is_parallel((*rq).context) }
}

// upstream i915_request.c:1389
unsafe fn request_to_parent(rq: *mut I915Request) -> *mut IntelContext {
    unsafe { intel_context_to_parent((*rq).context) }
}

// upstream i915_request.c:1395
unsafe fn is_same_parallel_context(to: *mut I915Request, from: *mut I915Request) -> bool {
    unsafe { is_parallel_rq(to) && request_to_parent(to) == request_to_parent(from) }
}

// upstream i915_request.c:1403
pub unsafe fn i915_request_await_execution(rq: *mut I915Request, fence: *mut DmaFence) -> i32 {
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
        { continue; }
        let ret = if unsafe { is_i915_fence(current) } {
            if unsafe { is_same_parallel_context(rq, to_request(current)) } { continue; }
            unsafe { __i915_request_await_execution(rq, to_request(current)) }
        } else {
            unsafe { i915_request_await_external(rq, current) }
        };
        if ret < 0 { return ret; }
    }
    0
}

// upstream i915_request.c:1449
unsafe fn await_request_submit(to: *mut I915Request, from: *mut I915Request) -> i32 {
    if unsafe { (*to).engine == ptr::read_volatile(ptr::addr_of!((*from).engine)) } {
        unsafe { i915_sw_fence_await_sw_fence_gfp(&mut (*to).submit, &mut (*from).submit, I915_FENCE_GFP) }
    } else {
        unsafe { __i915_request_await_execution(to, from) }
    }
}

// upstream i915_request.c:1468
unsafe fn i915_request_await_request(to: *mut I915Request, from: *mut I915Request) -> i32 {
    gem_bug_on!(to == from);
    gem_bug_on!(unsafe { (*to).timeline == (*from).timeline });
    if i915_request_completed(unsafe { &*from }) {
        unsafe { i915_sw_fence_set_error_once(&mut (*to).submit, (*from).fence.error) };
        return 0;
    }
    if unsafe { (*(*(*to).engine).sched_engine).schedule.is_some() } {
        let ret = unsafe { i915_sched_node_add_dependency(&mut (*to).sched, &mut (*from).sched, I915_DEPENDENCY_EXTERNAL) };
        if ret < 0 { return ret; }
    }
    if !unsafe { intel_engine_uses_guc((*to).engine) }
        && (unsafe { (*to).execution_mask | ptr::read_volatile(ptr::addr_of!((*from).execution_mask)) }).count_ones() == 1
    {
        unsafe { await_request_submit(to, from) }
    } else {
        unsafe { emit_semaphore_wait(to, from, I915_FENCE_GFP) }
    }
}

// upstream i915_request.c:1500
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
        { continue; }
        if unsafe { (*current).context != 0 && intel_timeline_sync_is_later(i915_request_timeline(rq), current) } {
            continue;
        }
        let ret = if unsafe { is_i915_fence(current) } {
            if unsafe { is_same_parallel_context(rq, to_request(current)) } { continue; }
            unsafe { i915_request_await_request(rq, to_request(current)) }
        } else {
            unsafe { i915_request_await_external(rq, current) }
        };
        if ret < 0 { return ret; }
        if unsafe { (*current).context != 0 } {
            unsafe { intel_timeline_sync_set(i915_request_timeline(rq), current) };
        }
    }
    0
}

// upstream i915_request.c:1602
pub unsafe fn i915_request_await_deps(rq: *mut I915Request, deps: *const I915DepsView) -> i32 {
    let count = unsafe { (*deps).num_deps };
    for index in 0..count {
        let fence = unsafe { *(*deps).fences.add(index as usize) };
        let err = unsafe { i915_request_await_dma_fence(rq, fence) };
        if err != 0 { return err; }
    }
    0
}

// upstream i915_request.c:1612
pub unsafe fn i915_request_await_object(
    to: *mut I915Request,
    obj: *mut DrmI915GemObject,
    write: bool,
) -> i32 {
    unsafe extern "C" {
        fn i915_request_each_object_resv_fence(
            request: *mut I915Request,
            object: *mut DrmI915GemObject,
            write: bool,
        ) -> i32;
    }
    unsafe { i915_request_each_object_resv_fence(to, obj, write) }
}

// upstream i915_request.c:1621
unsafe fn i915_request_await_huc(rq: *mut I915Request) {
    let huc = unsafe { &mut (*(*(*(*rq).context).engine).gt).uc.huc };
    if unsafe { (*(*rq).context).gem_context.is_null() } { return; }
    if unsafe { intel_huc_wait_required(huc) } {
        unsafe { i915_sw_fence_await_sw_fence(&mut (*rq).submit, &mut (*huc).delayed_load.fence, &mut (*rq).hucq) };
    }
}

// upstream i915_request.c:1635
unsafe fn __i915_request_ensure_parallel_ordering(
    rq: *mut I915Request,
    timeline: *mut IntelTimeline,
) -> *mut I915Request {
    gem_bug_on!(!unsafe { is_parallel_rq(rq) });
    let parent = unsafe { request_to_parent(rq) };
    let prev = unsafe { (*parent).parallel.last_rq };
    if !prev.is_null() {
        if !unsafe { __i915_request_is_complete(prev) } {
            unsafe { i915_sw_fence_await_sw_fence(&mut (*rq).submit, &mut (*prev).submit, &mut (*rq).submitq) };
            if unsafe { (*(*(*rq).engine).sched_engine).schedule.is_some() } {
                unsafe { __i915_sched_node_add_dependency(&mut (*rq).sched, &mut (*prev).sched, &mut (*rq).dep, 0) };
            }
        }
        unsafe { i915_request_put(prev) };
    }
    unsafe { (*parent).parallel.last_rq = i915_request_get(rq) };
    let fence = unsafe { __i915_active_fence_set(&mut (*timeline).last_request, &mut (*rq).fence) };
    unsafe { to_request(fence) }
}

// upstream i915_request.c:1670
unsafe fn __i915_request_ensure_ordering(rq: *mut I915Request, timeline: *mut IntelTimeline) -> *mut I915Request {
    gem_bug_on!(unsafe { is_parallel_rq(rq) });
    let fence = unsafe { __i915_active_fence_set(&mut (*timeline).last_request, &mut (*rq).fence) };
    let prev = if fence.is_null() { ptr::null_mut() } else { unsafe { to_request(fence) } };
    if !prev.is_null() && !unsafe { __i915_request_is_complete(prev) } {
        let uses_guc = unsafe { intel_engine_uses_guc((*rq).engine) };
        let pow2 = (unsafe { ptr::read_volatile(ptr::addr_of!((*prev).engine)) }.as_ref().unwrap().mask
            | unsafe { (*(*rq).engine).mask }).count_ones() == 1;
        let same_context = unsafe { (*prev).context == (*rq).context };
        gem_bug_on!(same_context && i915_seqno_passed(unsafe { (*prev).fence.seqno }, unsafe { (*rq).fence.seqno }));
        if (same_context && uses_guc) || (!uses_guc && pow2) {
            unsafe { i915_sw_fence_await_sw_fence(&mut (*rq).submit, &mut (*prev).submit, &mut (*rq).submitq) };
        } else {
            unsafe { __i915_sw_fence_await_dma_fence(&mut (*rq).submit, &mut (*prev).fence, &mut (*rq).dmaq) };
        }
        if unsafe { (*(*(*rq).engine).sched_engine).schedule.is_some() } {
            unsafe { __i915_sched_node_add_dependency(&mut (*rq).sched, &mut (*prev).sched, &mut (*rq).dep, 0) };
        }
    }
    prev
}

// upstream i915_request.c:1719
unsafe fn __i915_request_add_to_timeline(rq: *mut I915Request) -> *mut I915Request {
    let timeline = unsafe { i915_request_timeline(rq) };
    if unsafe { (*(*rq).engine).class == VIDEO_DECODE_CLASS } {
        unsafe { i915_request_await_huc(rq) };
    }
    let prev = if unsafe { is_parallel_rq(rq) } {
        unsafe { __i915_request_ensure_parallel_ordering(rq, timeline) }
    } else {
        unsafe { __i915_request_ensure_ordering(rq, timeline) }
    };
    if !prev.is_null() { unsafe { i915_request_put(prev) }; }
    gem_bug_on!(unsafe { (*timeline).seqno != (*rq).fence.seqno as u32 });
    prev
}

// upstream i915_request.c:1787
pub unsafe fn __i915_request_commit(rq: *mut I915Request) -> *mut I915Request {
    let engine = unsafe { (*rq).engine };
    let ring = unsafe { (*rq).ring };
    gem_bug_on!(unsafe { (*rq).reserved_space > (*ring).space });
    unsafe { (*rq).reserved_space = 0 };
    unsafe { (*rq).emitted_jiffies = jiffies };
    let dw = unsafe { (*engine).emit_fini_breadcrumb_dw };
    let cs = unsafe { intel_ring_begin(rq, dw) };
    gem_bug_on!(IS_ERR(cs));
    unsafe { (*rq).postfix = intel_ring_offset(rq, cs) };
    unsafe { __i915_request_add_to_timeline(rq) }
}

// upstream i915_request.c:1817
pub unsafe fn __i915_request_queue_bh(rq: *mut I915Request) {
    unsafe { i915_sw_fence_commit(&mut (*rq).semaphore) };
    unsafe { i915_sw_fence_commit(&mut (*rq).submit) };
}

// upstream i915_request.c:1823
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
pub unsafe fn i915_request_add(rq: *mut I915Request) {
    let tl = unsafe { i915_request_timeline(rq) };
    lockdep_assert_held(unsafe { &(*tl).mutex });
    unsafe { lockdep_unpin_lock(&mut (*tl).mutex, (*rq).cookie) };
    unsafe { __i915_request_commit(rq) };
    let mut attr = I915SchedAttr::default();
    rcu_read_lock();
    let ctx = unsafe { rcu_dereference((*(*rq).context).gem_context) };
    if !ctx.is_null() { attr = unsafe { (*ctx).sched }; }
    rcu_read_unlock();
    unsafe { __i915_request_queue(rq, &attr) };
    unsafe { mutex_unlock(&mut (*tl).mutex) };
}


// upstream i915_request.c:358
pub unsafe fn i915_request_retire(rq: *mut I915Request) -> bool {
    if !unsafe { __i915_request_is_complete(rq) } {
        return false;
    }
    axlog::trace!("i915 request retire fence={:#x}:{:#x}", unsafe { (*rq).fence.context }, unsafe { (*rq).fence.seqno });
    gem_bug_on!(!i915_sw_fence_signaled(unsafe { &(*rq).submit }));
    unsafe { trace_i915_request_retire(rq) };
    i915_request_mark_complete(unsafe { &*rq });
    unsafe { __rq_cancel_watchdog(rq) };
    gem_bug_on!(!list_is_first(&(*rq).link, &i915_request_timeline(rq).as_ref().unwrap().requests));
    if CONFIG_DRM_I915_DEBUG_GEM {
        unsafe { __i915_request_fill(rq, POISON_FREE as u8) };
    }
    unsafe { (*(*rq).ring).head = (*rq).postfix };
    if !i915_request_signaled(unsafe { &(*rq).fence }) {
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
pub unsafe fn i915_request_retire_upto(rq: *mut I915Request) {
    let tl = unsafe { i915_request_timeline(rq) };
    gem_bug_on!(!unsafe { __i915_request_is_complete(rq) });
    loop {
        let tmp = unsafe { list_first_entry::<I915Request>(&(*tl).requests, offset_of!(I915Request, link)) };
        gem_bug_on!(!i915_request_completed(unsafe { &*tmp }));
        let done = unsafe { i915_request_retire(tmp) };
        if !done || tmp == rq {
            break;
        }
    }
}

// upstream i915_request.c:434
unsafe fn __engine_active(engine: *mut IntelEngineCs) -> *mut *mut I915Request {
    unsafe { ptr::read_volatile(ptr::addr_of_mut!((*engine).execlists.active)) }
}

// upstream i915_request.c:440
unsafe fn __request_in_flight(signal: *mut I915Request) -> bool {
    if !i915_request_is_ready(unsafe { &*signal }) {
        return false;
    }
    if !intel_context_inflight(unsafe { (*signal).context }) {
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
            inflight = i915_seqno_passed(unsafe { (*rq).fence.seqno }, unsafe { (*signal).fence.seqno });
            break;
        }
        port = unsafe { port.add(1) };
    }
    rcu_read_unlock();
    inflight
}

// upstream i915_request.c:502
unsafe fn __await_execution(rq: *mut I915Request, signal: *mut I915Request, gfp: u32) -> i32 {
    if i915_request_is_active(unsafe { &*signal }) {
        return 0;
    }
    let cb = unsafe { kmem_cache_alloc(SLAB_EXECUTE_CBS, gfp) }.cast::<ExecuteCb>();
    if cb.is_null() {
        return -ENOMEM;
    }
    unsafe { (*cb).fence = ptr::addr_of_mut!((*rq).submit) };
    unsafe { i915_sw_fence_await(&mut (*rq).submit) };
    unsafe { init_irq_work(&mut (*cb).work, Some(irq_execute_cb)) };
    if unsafe { llist_add(&mut (*cb).work.node.llist, &mut (*signal).execute_cb) } {
        if i915_request_is_active(unsafe { &*signal }) || unsafe { __request_in_flight(signal) } {
            unsafe { i915_request_notify_execute_cb_imm(signal) };
        }
    }
    0
}

// upstream i915_request.c:542
fn fatal_error(error: i32) -> bool {
    !matches!(error, 0 | -EAGAIN | -ETIMEDOUT)
}

// upstream i915_request.c:554
pub unsafe fn __i915_request_skip(rq: *mut I915Request) {
    gem_bug_on!(!fatal_error(unsafe { (*rq).fence.error }));
    if unsafe { (*rq).infix == (*rq).postfix } {
        return;
    }
    unsafe { __i915_request_fill(rq, 0) };
    unsafe { (*rq).infix = (*rq).postfix };
}

// upstream i915_request.c:590
pub unsafe fn i915_request_set_error_once(rq: *mut I915Request, error: i32) -> bool {
    gem_bug_on!((error as isize) >= 0 || (error as isize) < -(MAX_ERRNO as isize));
    if i915_request_signaled(unsafe { &(*rq).fence }) {
        return false;
    }
    let mut old = unsafe { atomic_read(&(*rq).fence.error) };
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
pub unsafe fn i915_request_mark_eio(rq: *mut I915Request) -> *mut I915Request {
    if unsafe { __i915_request_is_complete(rq) } {
        return ptr::null_mut();
    }
    gem_bug_on!(i915_request_signaled(unsafe { &(*rq).fence }));
    let rq = unsafe { i915_request_get(rq) };
    unsafe { i915_request_set_error_once(rq, -EIO) };
    i915_request_mark_complete(unsafe { &*rq });
    rq
}

// upstream i915_request.c:626
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
    if unsafe { (*request).sched.semaphores != 0 && i915_sw_fence_signaled(&(*request).semaphore) } {
        unsafe { (*engine).saturated |= (*request).sched.semaphores };
    }
    if let Some(emit) = unsafe { (*engine).emit_fini_breadcrumb } {
        unsafe { emit(request, (*(*request).ring).vaddr.add((*request).postfix as usize)) };
    }
    unsafe { trace_i915_request_execute(request) };
    if let Some(bump) = unsafe { (*engine).bump_serial } {
        unsafe { bump(engine) };
    } else {
        unsafe { (*engine).serial += 1 };
    }
    result = true;
    gem_bug_on!(test_bit(I915_FENCE_FLAG_ACTIVE, unsafe { &(*request).fence.flags }));
    if let Some(add_active) = unsafe { (*engine).add_active_request } {
        unsafe { add_active(request) };
    }
    submit_active(request, engine, result)
}

unsafe fn submit_active(request: *mut I915Request, _engine: *mut IntelEngineCs, result: bool) -> bool {
    clear_bit(I915_FENCE_FLAG_PQUEUE, unsafe { &mut (*request).fence.flags });
    set_bit(I915_FENCE_FLAG_ACTIVE, unsafe { &mut (*request).fence.flags });
    unsafe { __notify_execute_cb_irq(request) };
    if test_bit(DMA_FENCE_FLAG_ENABLE_SIGNAL_BIT, unsafe { &(*request).fence.flags }) {
        unsafe { i915_request_enable_breadcrumb(request) };
    }
    result
}

// upstream i915_request.c:699
pub unsafe fn i915_request_submit(request: *mut I915Request) {
    let engine = unsafe { (*request).engine };
    let mut flags = 0;
    unsafe { spin_lock_irqsave(&mut (*(*engine).sched_engine).lock, &mut flags) };
    unsafe { __i915_request_submit(request) };
    unsafe { spin_unlock_irqrestore(&mut (*(*engine).sched_engine).lock, flags) };
}

// upstream i915_request.c:712
pub unsafe fn __i915_request_unsubmit(request: *mut I915Request) {
    let engine = unsafe { (*request).engine };
    gem_bug_on!(!irqs_disabled());
    lockdep_assert_held(unsafe { &(*(*engine).sched_engine).lock });
    gem_bug_on!(!test_bit(I915_FENCE_FLAG_ACTIVE, unsafe { &(*request).fence.flags }));
    clear_bit_unlock(I915_FENCE_FLAG_ACTIVE, unsafe { &mut (*request).fence.flags });
    if test_bit(DMA_FENCE_FLAG_ENABLE_SIGNAL_BIT, unsafe { &(*request).fence.flags }) {
        unsafe { i915_request_cancel_breadcrumb(request) };
    }
    if unsafe { (*request).sched.semaphores != 0 && __i915_request_has_started(request) } {
        unsafe { (*request).sched.semaphores = 0 };
    }
}

// upstream i915_request.c:750
pub unsafe fn i915_request_unsubmit(request: *mut I915Request) {
    let engine = unsafe { (*request).engine };
    let mut flags = 0;
    unsafe { spin_lock_irqsave(&mut (*(*engine).sched_engine).lock, &mut flags) };
    unsafe { __i915_request_unsubmit(request) };
    unsafe { spin_unlock_irqrestore(&mut (*(*engine).sched_engine).lock, flags) };
}

// upstream i915_request.c:763
pub unsafe fn i915_request_cancel(rq: *mut I915Request, error: i32) {
    if !unsafe { i915_request_set_error_once(rq, error) } {
        return;
    }
    set_bit(I915_FENCE_FLAG_SENTINEL, unsafe { &mut (*rq).fence.flags });
    unsafe { intel_context_cancel_request((*rq).context, rq) };
}

// upstream i915_request.c:773
unsafe fn submit_notify(fence: *mut I915SwFence, state: I915SwFenceNotify) -> i32 {
    let request = unsafe { fence.cast::<u8>().sub(offset_of!(I915Request, submit)).cast::<I915Request>() };
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
unsafe fn semaphore_notify(fence: *mut I915SwFence, state: I915SwFenceNotify) -> i32 {
    let rq = unsafe { fence.cast::<u8>().sub(offset_of!(I915Request, semaphore)).cast::<I915Request>() };
    if let I915SwFenceNotify::FenceFree = state {
        unsafe { i915_request_put(rq) };
    }
    NOTIFY_DONE
}

// upstream i915_request.c:826
unsafe fn retire_requests(tl: *mut IntelTimeline) {
    let mut rq = unsafe { list_first_entry_or_null::<I915Request>(&(*tl).requests, offset_of!(I915Request, link)) };
    while !rq.is_null() {
        let next = unsafe { list_next_entry_or_null(rq, offset_of!(I915Request, link), &(*tl).requests) };
        if !unsafe { i915_request_retire(rq) } {
            break;
        }
        rq = next;
    }
}
