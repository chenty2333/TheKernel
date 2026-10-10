// SPDX-License-Identifier: MIT
// Copyright (C) 2016 Intel Corporation.
//
// Source-faithful Rust transcription of Linux 7.2.3
// drivers/gpu/drm/i915/i915_sw_fence.c.  The upstream file is MIT-licensed.
// Linux 7.2.3 target configuration: CONFIG_LOCKDEP=n,
// CONFIG_DRM_I915_SW_FENCE_DEBUG_OBJECTS=n, and
// CONFIG_DRM_I915_SW_FENCE_CHECK_DAG=n. Kernel DMA-fence operations without an
// existing LinuxKPI implementation use their real Linux API symbols; wait,
// allocation, timer, IRQ-work and RCU operations bind to the canonical modules.

#![allow(non_snake_case, non_camel_case_types, unsafe_op_in_unsafe_fn)]

use core::{
    ffi::{c_char, c_int, c_ulong, c_void},
    mem::offset_of,
    sync::atomic::{AtomicI32, AtomicPtr, Ordering},
};

use crate::{
    intel_context_types_upstream::{I915SwFence, I915SwFenceNotify},
    intel_context_upstream::{DmaFence, DmaFenceCb, IrqWork, RcuHead, WaitQueueEntry},
    intel_engine_cs_upstream::{ListHead, TimerList},
    linux::{irq, locks, memory, rcu, requests, timer, wait},
    linux_config::GFP_ATOMIC,
    linux_list::{INIT_LIST_HEAD, list_add_tail, list_del, list_empty, list_move_tail},
    linux_locks::{spin_lock_irqsave_raw, spin_unlock_irqrestore_raw},
};

const I915_SW_FENCE_FLAG_FENCE: u32 = 1 << 31;
const I915_SW_FENCE_FLAG_ALLOC: u32 = 1 << 30;
const DEBUG_FENCE_IDLE: i32 = 0;
const DEBUG_FENCE_NOTIFY: i32 = 1;
const EINVAL: i32 = 22;
const ENOMEM: i32 = 12;
const ENOENT: i32 = 2;
const ETIMEDOUT: i32 = 110;

#[repr(C)]
pub struct DmaResv {
    _opaque: [u8; 0],
}
#[repr(C)]
struct DmaResvIter {
    obj: *mut DmaResv,
    usage: u32,
    fence: *mut DmaFence,
    fence_usage: u32,
    index: u32,
    fences: *mut c_void,
    num_fences: u32,
    is_restarted: bool,
}
const _: [(); 48] = [(); core::mem::size_of::<DmaResvIter>()];

#[repr(C)]
pub struct I915SwDmaFenceCb {
    pub base: DmaFenceCb,
    pub fence: *mut I915SwFence,
}

#[repr(C)]
struct I915SwDmaFenceCbTimer {
    base: I915SwDmaFenceCb,
    dma: *mut DmaFence,
    timer: TimerList,
    work: IrqWork,
    rcu: RcuHead,
}

unsafe extern "C" {
    fn dma_fence_is_signaled(dma: *mut DmaFence) -> bool;
    fn dma_fence_wait(dma: *mut DmaFence, intr: bool) -> c_int;
    fn dma_fence_add_callback(
        dma: *mut DmaFence,
        cb: *mut DmaFenceCb,
        func: unsafe extern "C" fn(*mut DmaFence, *mut DmaFenceCb),
    ) -> c_int;
    fn dma_fence_driver_name(dma: *mut DmaFence) -> *const c_char;
    fn dma_fence_timeline_name(dma: *mut DmaFence) -> *const c_char;
    fn dma_resv_iter_next(cursor: *mut DmaResvIter) -> *mut DmaFence;
    fn round_jiffies_up(j: c_ulong) -> c_ulong;
    fn timer_shutdown_sync(timer: *mut TimerList) -> c_int;
}

// upstream: i915_sw_fence.c i915_sw_fence_debug_hint()
unsafe fn i915_sw_fence_debug_hint(addr: *mut I915SwFence) -> *mut c_void {
    unsafe {
        (*addr)
            .fn_
            .map(|f| f as *const () as *mut c_void)
            .unwrap_or(core::ptr::null_mut())
    }
}

// These are the source's CONFIG_DRM_I915_SW_FENCE_DEBUG_OBJECTS=n bodies.
// upstream: i915_sw_fence.c debug_fence_init()
unsafe fn debug_fence_init(fence: *mut I915SwFence) {
    let _ = fence;
}
// upstream: i915_sw_fence.c debug_fence_init_onstack()
unsafe fn debug_fence_init_onstack(fence: *mut I915SwFence) {
    let _ = fence;
}
// upstream: i915_sw_fence.c debug_fence_activate()
unsafe fn debug_fence_activate(fence: *mut I915SwFence) {
    let _ = fence;
}
// upstream: i915_sw_fence.c debug_fence_set_state()
unsafe fn debug_fence_set_state(fence: *mut I915SwFence, old: c_int, new: c_int) {
    let _ = (fence, old, new);
}
// upstream: i915_sw_fence.c debug_fence_deactivate()
unsafe fn debug_fence_deactivate(fence: *mut I915SwFence) {
    let _ = fence;
}
// upstream: i915_sw_fence.c debug_fence_destroy()
unsafe fn debug_fence_destroy(fence: *mut I915SwFence) {
    let _ = fence;
}
// upstream: i915_sw_fence.c debug_fence_free()
unsafe fn debug_fence_free(fence: *mut I915SwFence) {
    let _ = fence;
}
// upstream: i915_sw_fence.c debug_fence_assert()
unsafe fn debug_fence_assert(fence: *mut I915SwFence) {
    let _ = fence;
}

// i915_sw_fence.h's disabled-debug-object inline definition.
pub unsafe fn i915_sw_fence_fini(fence: *mut I915SwFence) {
    let _ = fence;
}

// Linux 7.2.3 include/linux/gfp.h inline helper for the supplied config.
#[inline]
pub(crate) fn gfpflags_allow_blocking(gfp: c_ulong) -> bool {
    let flags = gfp as u32;
    flags & (1 << 10) != 0 && flags & GFP_ATOMIC != GFP_ATOMIC
}

// i915_sw_fence.h's wait_event() helper, backed by the canonical wait adapter.
unsafe fn i915_sw_fence_wait(fence: *mut I915SwFence) {
    wait::wait_until(
        u64::MAX,
        || unsafe { crate::linux::sw_fence::i915_sw_fence_done(&*fence) },
        true,
    );
}

// upstream: i915_sw_fence.c __i915_sw_fence_notify()
unsafe fn __i915_sw_fence_notify(fence: *mut I915SwFence, state: I915SwFenceNotify) -> c_int {
    unsafe { (*fence).fn_.map(|notify| notify(fence, state)).unwrap_or(0) }
}

// upstream: i915_sw_fence.c __i915_sw_fence_wake_up_all()
unsafe fn __i915_sw_fence_wake_up_all(fence: *mut I915SwFence, continuation: *mut c_void) {
    unsafe {
        debug_fence_deactivate(fence);
        let pending = core::ptr::addr_of_mut!((*fence).pending.counter);
        AtomicI32::from_ptr(pending).store(-1, Ordering::Release);
        let wait = core::ptr::addr_of_mut!((*fence).wait);
        let head = core::ptr::addr_of_mut!((*wait).head);
        let mut irq_flags = 0;
        spin_lock_irqsave_raw(core::ptr::addr_of_mut!((*wait).lock), &mut irq_flags);
        if !continuation.is_null() {
            let continuation = continuation.cast::<ListHead>();
            let mut node = (*head).next;
            while node != head {
                let next = (*node).next;
                let entry = node
                    .cast::<u8>()
                    .wrapping_sub(offset_of!(WaitQueueEntry, entry))
                    .cast::<WaitQueueEntry>();
                if (*entry).flags & I915_SW_FENCE_FLAG_FENCE != 0 {
                    list_move_tail(core::ptr::addr_of_mut!((*entry).entry), continuation);
                } else if let Some(func) = (*entry).func {
                    let _ = func(entry, 3, 0, continuation.cast());
                }
                node = next;
            }
        } else {
            let mut extra = ListHead {
                next: core::ptr::null_mut(),
                prev: core::ptr::null_mut(),
            };
            INIT_LIST_HEAD(&mut extra);
            loop {
                let mut node = (*head).next;
                while node != head {
                    let next = (*node).next;
                    let entry = node
                        .cast::<u8>()
                        .wrapping_sub(offset_of!(WaitQueueEntry, entry))
                        .cast::<WaitQueueEntry>();
                    let wake_flags = if (*entry).flags & I915_SW_FENCE_FLAG_FENCE != 0 {
                        (*fence).error
                    } else {
                        0
                    };
                    if let Some(func) = (*entry).func {
                        let _ = func(entry, 3, wake_flags, (&mut extra as *mut ListHead).cast());
                    }
                    node = next;
                }
                if list_empty(&extra) {
                    break;
                }
                // list_splice_tail_init(&extra, &wait->head)
                let first = extra.next;
                let last = extra.prev;
                let tail = (*head).prev;
                (*tail).next = first;
                (*first).prev = tail;
                (*last).next = head;
                (*head).prev = last;
                INIT_LIST_HEAD(&mut extra);
            }
        }
        spin_unlock_irqrestore_raw(core::ptr::addr_of_mut!((*wait).lock), irq_flags);
        debug_fence_assert(fence);
    }
}

// upstream: i915_sw_fence.c __i915_sw_fence_complete()
unsafe fn __i915_sw_fence_complete(fence: *mut I915SwFence, continuation: *mut c_void) {
    unsafe {
        debug_fence_assert(fence);
        let pending = core::ptr::addr_of_mut!((*fence).pending.counter);
        if AtomicI32::from_ptr(pending).fetch_sub(1, Ordering::AcqRel) != 1 {
            return;
        }
        debug_fence_set_state(fence, DEBUG_FENCE_IDLE, DEBUG_FENCE_NOTIFY);
        if __i915_sw_fence_notify(fence, I915SwFenceNotify::FenceComplete) != 0 {
            return;
        }
        debug_fence_set_state(fence, DEBUG_FENCE_NOTIFY, DEBUG_FENCE_IDLE);
        __i915_sw_fence_wake_up_all(fence, continuation);
        debug_fence_destroy(fence);
        let _ = __i915_sw_fence_notify(fence, I915SwFenceNotify::FenceFree);
    }
}

// upstream: i915_sw_fence.c i915_sw_fence_complete()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_sw_fence_complete(fence: *mut I915SwFence) {
    unsafe {
        debug_fence_assert(fence);
        if crate::linux::assertion::warn_on(crate::linux::sw_fence::i915_sw_fence_done(&*fence)) {
            return;
        }
        __i915_sw_fence_complete(fence, core::ptr::null_mut());
    }
}

// upstream: i915_sw_fence.c i915_sw_fence_await()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_sw_fence_await(fence: *mut I915SwFence) -> bool {
    unsafe {
        let pending = AtomicI32::from_ptr(core::ptr::addr_of_mut!((*fence).pending.counter));
        let mut observed = pending.load(Ordering::Relaxed);
        loop {
            if observed < 1 {
                return false;
            }
            match pending.compare_exchange_weak(
                observed,
                observed.wrapping_add(1),
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(current) => observed = current,
            }
        }
    }
}

// upstream: i915_sw_fence.c __i915_sw_fence_init()
pub unsafe fn __i915_sw_fence_init(
    fence: *mut I915SwFence,
    fn_: Option<unsafe extern "C" fn(*mut I915SwFence, I915SwFenceNotify) -> c_int>,
    name: *const c_char,
    key: *mut c_void,
) {
    unsafe {
        locks::spin_lock_init(&mut (*fence).wait.lock);
        wait::init_waitqueue_head(&mut (*fence).wait);
        let _ = (name, key); // CONFIG_LOCKDEP=n: Linux drops the name and class key.
        (*fence).fn_ = fn_;
        i915_sw_fence_reinit(fence);
    }
}

/// Lockdep-disabled `i915_sw_fence_init()` header wrapper. The configured
/// target discards the optional source name and class key in the common body.
pub unsafe fn i915_sw_fence_init(
    fence: *mut I915SwFence,
    fn_: Option<unsafe extern "C" fn(*mut I915SwFence, I915SwFenceNotify) -> c_int>,
) {
    unsafe { __i915_sw_fence_init(fence, fn_, core::ptr::null(), core::ptr::null_mut()) }
}

// upstream: i915_sw_fence.c i915_sw_fence_reinit()
pub unsafe fn i915_sw_fence_reinit(fence: *mut I915SwFence) {
    unsafe {
        debug_fence_init(fence);
        (*fence).pending.counter = 1;
        (*fence).error = 0;
    }
}

// upstream: i915_sw_fence.c i915_sw_fence_commit()
pub unsafe fn i915_sw_fence_commit(fence: *mut I915SwFence) {
    unsafe {
        debug_fence_activate(fence);
        i915_sw_fence_complete(fence);
    }
}

// upstream: i915_sw_fence.c i915_sw_fence_wake()
unsafe extern "C" fn i915_sw_fence_wake(
    wq: *mut WaitQueueEntry,
    _mode: u32,
    flags: c_int,
    key: *mut c_void,
) -> c_int {
    unsafe {
        let fence = (*wq).private.cast::<I915SwFence>();
        crate::linux::sw_fence::i915_sw_fence_set_error_once(&mut *fence, flags);
        list_del(core::ptr::addr_of_mut!((*wq).entry));
        __i915_sw_fence_complete(fence, key);
        if (*wq).flags & I915_SW_FENCE_FLAG_ALLOC != 0 {
            memory::kfree(wq);
        }
        0
    }
}

// upstream: i915_sw_fence.c i915_sw_fence_check_if_after()
unsafe fn i915_sw_fence_check_if_after(
    fence: *mut I915SwFence,
    signaler: *const I915SwFence,
) -> bool {
    let _ = (fence, signaler);
    false
}

// upstream: i915_sw_fence.c __i915_sw_fence_await_sw_fence()
unsafe fn __i915_sw_fence_await_sw_fence(
    fence: *mut I915SwFence,
    signaler: *mut I915SwFence,
    mut wq: *mut WaitQueueEntry,
    gfp: c_ulong,
) -> c_int {
    unsafe {
        debug_fence_assert(fence);
        // might_sleep_if() is a debug-only scheduler annotation in this config.
        if crate::linux::sw_fence::i915_sw_fence_done(&*signaler) {
            crate::linux::sw_fence::i915_sw_fence_set_error_once(&mut *fence, (*signaler).error);
            return 0;
        }
        debug_fence_assert(signaler);
        // The source DAG checker rejects cycles before it reserves a signaler.
        if i915_sw_fence_check_if_after(fence, signaler) {
            return -EINVAL;
        }

        let mut flags = I915_SW_FENCE_FLAG_FENCE;
        if wq.is_null() {
            wq = memory::kmalloc(core::mem::size_of::<WaitQueueEntry>(), gfp as u32)
                .cast::<WaitQueueEntry>();
            if wq.is_null() {
                if !gfpflags_allow_blocking(gfp) {
                    return -ENOMEM;
                }
                i915_sw_fence_wait(signaler);
                crate::linux::sw_fence::i915_sw_fence_set_error_once(
                    &mut *fence,
                    (*signaler).error,
                );
                return 0;
            }
            flags |= I915_SW_FENCE_FLAG_ALLOC;
        }

        INIT_LIST_HEAD(core::ptr::addr_of_mut!((*wq).entry));
        (*wq).flags = flags;
        (*wq).func = Some(i915_sw_fence_wake);
        (*wq).private = fence.cast();
        let _ = i915_sw_fence_await(fence);

        let mut irq_flags = 0;
        spin_lock_irqsave_raw(
            core::ptr::addr_of_mut!((*signaler).wait.lock),
            &mut irq_flags,
        );
        let pending = if !crate::linux::sw_fence::i915_sw_fence_done(&*signaler) {
            list_add_tail(
                core::ptr::addr_of_mut!((*wq).entry),
                core::ptr::addr_of_mut!((*signaler).wait.head),
            );
            1
        } else {
            let _ = i915_sw_fence_wake(wq, 0, (*signaler).error, core::ptr::null_mut());
            0
        };
        spin_unlock_irqrestore_raw(core::ptr::addr_of_mut!((*signaler).wait.lock), irq_flags);
        pending
    }
}

// upstream: i915_sw_fence.c i915_sw_fence_await_sw_fence()
pub unsafe fn i915_sw_fence_await_sw_fence(
    fence: *mut I915SwFence,
    signaler: *mut I915SwFence,
    wq: *mut WaitQueueEntry,
) -> c_int {
    unsafe { __i915_sw_fence_await_sw_fence(fence, signaler, wq, 0) }
}

// upstream: i915_sw_fence.c i915_sw_fence_await_sw_fence_gfp()
pub unsafe fn i915_sw_fence_await_sw_fence_gfp(
    fence: *mut I915SwFence,
    signaler: *mut I915SwFence,
    gfp: c_ulong,
) -> c_int {
    unsafe { __i915_sw_fence_await_sw_fence(fence, signaler, core::ptr::null_mut(), gfp) }
}

// upstream: i915_sw_fence.c dma_i915_sw_fence_wake()
unsafe extern "C" fn dma_i915_sw_fence_wake(dma: *mut DmaFence, data: *mut DmaFenceCb) {
    unsafe {
        let cb = data.cast::<I915SwDmaFenceCb>();
        crate::linux::sw_fence::i915_sw_fence_set_error_once(&mut *(*cb).fence, (*dma).error);
        i915_sw_fence_complete((*cb).fence);
        memory::kfree(cb);
    }
}

// upstream: i915_sw_fence.c timer_i915_sw_fence_wake()
unsafe extern "C" fn timer_i915_sw_fence_wake(timer: *mut TimerList) {
    unsafe {
        let cb = timer
            .cast::<u8>()
            .wrapping_sub(offset_of!(I915SwDmaFenceCbTimer, timer))
            .cast::<I915SwDmaFenceCbTimer>();
        let fence = AtomicPtr::from_ptr(core::ptr::addr_of_mut!((*cb).base.fence))
            .swap(core::ptr::null_mut(), Ordering::AcqRel);
        if fence.is_null() {
            return;
        }
        rcu::rcu_read_lock();
        let driver = dma_fence_driver_name((*cb).dma);
        let timeline = dma_fence_timeline_name((*cb).dma);
        let driver = if driver.is_null() {
            "unknown".into()
        } else {
            core::ffi::CStr::from_ptr(driver).to_string_lossy()
        };
        let timeline = if timeline.is_null() {
            "unknown".into()
        } else {
            core::ffi::CStr::from_ptr(timeline).to_string_lossy()
        };
        axlog::warn!(
            "Asynchronous wait on fence {}:{}:{:x} timed out (hint:{:p})",
            driver,
            timeline,
            (*(*cb).dma).seqno,
            i915_sw_fence_debug_hint(fence),
        );
        rcu::rcu_read_unlock();
        crate::linux::sw_fence::i915_sw_fence_set_error_once(&mut *fence, -ETIMEDOUT);
        i915_sw_fence_complete(fence);
    }
}

// upstream: i915_sw_fence.c dma_i915_sw_fence_wake_timer()
unsafe extern "C" fn dma_i915_sw_fence_wake_timer(dma: *mut DmaFence, data: *mut DmaFenceCb) {
    unsafe {
        let cb = data
            .cast::<u8>()
            .wrapping_sub(
                offset_of!(I915SwDmaFenceCbTimer, base) + offset_of!(I915SwDmaFenceCb, base),
            )
            .cast::<I915SwDmaFenceCbTimer>();
        let fence = AtomicPtr::from_ptr(core::ptr::addr_of_mut!((*cb).base.fence))
            .swap(core::ptr::null_mut(), Ordering::AcqRel);
        if !fence.is_null() {
            crate::linux::sw_fence::i915_sw_fence_set_error_once(&mut *fence, (*dma).error);
            i915_sw_fence_complete(fence);
        }
        irq::irq_work_queue(&mut (*cb).work);
    }
}

// upstream: i915_sw_fence.c irq_i915_sw_fence_work()
unsafe extern "C" fn irq_i915_sw_fence_work(wrk: *mut IrqWork) {
    unsafe {
        let cb = wrk
            .cast::<u8>()
            .wrapping_sub(offset_of!(I915SwDmaFenceCbTimer, work))
            .cast::<I915SwDmaFenceCbTimer>();
        let _ = timer_shutdown_sync(&mut (*cb).timer);
        requests::dma_fence_put((*cb).dma);
        rcu::call_rcu(&mut (*cb).rcu, free_timer_cb_rcu);
    }
}

// The timer callback runs after its final RCU grace period is scheduled.
unsafe extern "C" fn free_timer_cb_rcu(head: *mut RcuHead) {
    unsafe {
        let cb = head
            .cast::<u8>()
            .wrapping_sub(offset_of!(I915SwDmaFenceCbTimer, rcu))
            .cast::<I915SwDmaFenceCbTimer>();
        memory::kfree(cb);
    }
}

// upstream: i915_sw_fence.c i915_sw_fence_await_dma_fence()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_sw_fence_await_dma_fence(
    fence: *mut I915SwFence,
    dma: *mut DmaFence,
    timeout: c_ulong,
    gfp: u32,
) -> c_int {
    unsafe {
        debug_fence_assert(fence);
        if dma_fence_is_signaled(dma) {
            crate::linux::sw_fence::i915_sw_fence_set_error_once(&mut *fence, (*dma).error);
            return 0;
        }
        let cb = memory::kmalloc(
            if timeout != 0 {
                core::mem::size_of::<I915SwDmaFenceCbTimer>()
            } else {
                core::mem::size_of::<I915SwDmaFenceCb>()
            },
            gfp as u32,
        )
        .cast::<I915SwDmaFenceCb>();
        if cb.is_null() {
            if !gfpflags_allow_blocking(gfp as c_ulong) {
                return -ENOMEM;
            }
            let ret = dma_fence_wait(dma, false);
            if ret != 0 {
                return ret;
            }
            crate::linux::sw_fence::i915_sw_fence_set_error_once(&mut *fence, (*dma).error);
            return 0;
        }
        (*cb).fence = fence;
        let _ = i915_sw_fence_await(fence);
        let mut func =
            dma_i915_sw_fence_wake as unsafe extern "C" fn(*mut DmaFence, *mut DmaFenceCb);
        if timeout != 0 {
            let timer = cb.cast::<I915SwDmaFenceCbTimer>();
            (*timer).dma = requests::dma_fence_get(dma);
            irq::init_irq_work(&mut (*timer).work, irq_i915_sw_fence_work);
            timer::timer_setup(&mut (*timer).timer, timer_i915_sw_fence_wake, 0x0020_0000);
            let expires =
                crate::linux::primitives::jiffies().wrapping_add(timeout as u64) as c_ulong;
            let expires = round_jiffies_up(expires);
            timer::mod_timer(&mut (*timer).timer, expires);
            func = dma_i915_sw_fence_wake_timer;
        }
        let mut ret = dma_fence_add_callback(dma, &mut (*cb).base, func);
        if ret == 0 {
            ret = 1;
        } else {
            func(dma, &mut (*cb).base);
            if ret == -ENOENT {
                ret = 0;
            }
        }
        ret
    }
}

// upstream: i915_sw_fence.c __dma_i915_sw_fence_wake()
unsafe extern "C" fn __dma_i915_sw_fence_wake(dma: *mut DmaFence, data: *mut DmaFenceCb) {
    unsafe {
        let cb = data.cast::<I915SwDmaFenceCb>();
        crate::linux::sw_fence::i915_sw_fence_set_error_once(&mut *(*cb).fence, (*dma).error);
        i915_sw_fence_complete((*cb).fence);
    }
}

// upstream: i915_sw_fence.c __i915_sw_fence_await_dma_fence()
pub unsafe fn __i915_sw_fence_await_dma_fence(
    fence: *mut I915SwFence,
    dma: *mut DmaFence,
    cb: *mut I915SwDmaFenceCb,
) -> c_int {
    unsafe {
        debug_fence_assert(fence);
        if dma_fence_is_signaled(dma) {
            crate::linux::sw_fence::i915_sw_fence_set_error_once(&mut *fence, (*dma).error);
            return 0;
        }
        (*cb).fence = fence;
        let _ = i915_sw_fence_await(fence);
        let mut ret = 1;
        if dma_fence_add_callback(dma, &mut (*cb).base, __dma_i915_sw_fence_wake) != 0 {
            __dma_i915_sw_fence_wake(dma, &mut (*cb).base);
            ret = 0;
        }
        ret
    }
}

// upstream: i915_sw_fence.c i915_sw_fence_await_reservation()
pub unsafe fn i915_sw_fence_await_reservation(
    fence: *mut I915SwFence,
    resv: *mut DmaResv,
    write: bool,
    timeout: c_ulong,
    gfp: u32,
) -> c_int {
    unsafe {
        debug_fence_assert(fence);
        let mut cursor: DmaResvIter = core::mem::zeroed();
        cursor.obj = resv;
        cursor.usage = if write { 2 } else { 1 };
        cursor.fence = core::ptr::null_mut();
        let mut ret = 0;
        loop {
            let f = dma_resv_iter_next(&mut cursor);
            if f.is_null() {
                break;
            }
            let pending = i915_sw_fence_await_dma_fence(fence, f, timeout, gfp);
            if pending < 0 {
                ret = pending;
                break;
            }
            ret |= pending;
        }
        requests::dma_fence_put(cursor.fence);
        ret
    }
}
