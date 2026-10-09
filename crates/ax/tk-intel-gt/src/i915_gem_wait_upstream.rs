// SPDX-License-Identifier: MIT
// Copyright © 2016 Intel Corporation.
// Source-order translation of Linux v7.2.3 drivers/gpu/drm/i915/gem/i915_gem_wait.c.

#![allow(dead_code, non_camel_case_types, non_snake_case, unsafe_code)]

use core::{
    ffi::{c_int, c_long, c_ulong, c_void},
    mem::{offset_of, size_of, zeroed},
    ptr,
    sync::atomic::AtomicI32,
};

use crate::{
    i915_gem_object_api_upstream::i915_gem_object_put,
    i915_gem_object_header_upstream::{DrmFileObjectLookup, i915_gem_object_lookup},
    i915_gem_object_types_upstream::DrmI915GemObject,
    i915_gem_object_upstream::i915_gem_object_wait_moving_fence,
    i915_request_types_upstream::I915Request,
    i915_request_upstream::{DmaFenceOps, i915_fence_ops, i915_request_wait_timeout, to_request},
    i915_scheduler_types_upstream::I915SchedAttr,
    intel_context_upstream::DmaFence,
    linux::{
        bits::test_bit,
        gem::DmaResv,
        irq::{local_bh_disable, local_bh_enable},
        primitives::ktime_get,
        rcu::{rcu_read_lock, rcu_read_unlock},
        registers::I915_PRIORITY_DISPLAY,
        requests::{
            DMA_FENCE_FLAG_SIGNALED_BIT, dma_fence_get, dma_fence_put, i915_request_started,
        },
        wait::might_sleep,
    },
    linux_config::{EAGAIN, ETIME, HZ},
};

const I915_WAIT_INTERRUPTIBLE: u32 = 1 << 0;
const I915_WAIT_PRIORITY: u32 = 1 << 1;
const I915_WAIT_ALL: u32 = 1 << 2;
const ENOENT: c_int = 2;

const DMA_RESV_USAGE_WRITE: i32 = 1;
const DMA_RESV_USAGE_READ: i32 = 2;

const NSEC_PER_SEC: u64 = 1_000_000_000;
const MAX_JIFFY_OFFSET: c_ulong = ((c_long::MAX as c_ulong) >> 1) - 1;
const MAX_SCHEDULE_TIMEOUT: c_ulong = c_long::MAX as c_ulong;

const _: [(); 1] = [(); (I915_WAIT_INTERRUPTIBLE == 0x1) as usize];
const _: [(); 1] = [(); (NSEC_PER_SEC % HZ as u64 == 0) as usize];

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
const _: [(); 48] = [(); size_of::<DmaResvIter>()];

#[repr(C)]
struct DmaFenceArrayView {
    base: DmaFence,
    num_fences: u32,
    _num_pending: AtomicI32,
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
struct DrmI915GemWait {
    bo_handle: u32,
    flags: u32,
    timeout_ns: i64,
}
const _: [(); 16] = [(); size_of::<DrmI915GemWait>()];
const _: [(); 8] = [(); offset_of!(DrmI915GemWait, timeout_ns)];

unsafe extern "C" {
    fn dma_fence_wait_timeout(fence: *mut DmaFence, interruptible: bool, timeout: c_long)
    -> c_long;
    fn dma_resv_iter_next(cursor: *mut DmaResvIter) -> *mut DmaFence;
    fn intel_rps_boost(rq: *mut I915Request);
    static dma_fence_array_ops: DmaFenceOps;
    static dma_fence_chain_ops: DmaFenceOps;
}

#[inline]
fn dma_resv_usage_rw(write: bool) -> i32 {
    if write {
        DMA_RESV_USAGE_READ
    } else {
        DMA_RESV_USAGE_WRITE
    }
}

#[inline]
fn dma_fence_is_i915(fence: *const DmaFence) -> bool {
    !fence.is_null() && unsafe { (*fence).ops == ptr::addr_of!(i915_fence_ops).cast::<c_void>() }
}

#[inline]
fn dma_fence_is_array(fence: *const DmaFence) -> bool {
    !fence.is_null()
        && unsafe { (*fence).ops == ptr::addr_of!(dma_fence_array_ops).cast::<c_void>() }
}

#[inline]
fn dma_fence_is_chain(fence: *const DmaFence) -> bool {
    !fence.is_null()
        && unsafe { (*fence).ops == ptr::addr_of!(dma_fence_chain_ops).cast::<c_void>() }
}

#[inline]
unsafe fn dma_resv_iter_begin(cursor: &mut DmaResvIter, obj: *mut DmaResv, usage: i32) {
    unsafe { ptr::write_bytes(cursor, 0, 1) };
    cursor.obj = obj;
    cursor.usage = usage;
}

#[inline]
unsafe fn dma_resv_iter_end(cursor: &mut DmaResvIter) {
    unsafe { dma_fence_put(cursor.fence) };
}

#[inline]
unsafe fn dma_fence_is_signaled(fence: *mut DmaFence) -> bool {
    unsafe { crate::i915_gem_clflush_upstream::dma_fence_is_signaled(fence) }
}

// upstream: gem/i915_gem_wait.c i915_gem_object_wait_fence()
unsafe fn i915_gem_object_wait_fence(fence: *mut DmaFence, flags: u32, timeout: c_long) -> c_long {
    if test_bit(DMA_FENCE_FLAG_SIGNALED_BIT, unsafe { &(*fence).flags }) {
        return timeout;
    }

    if dma_fence_is_i915(fence) {
        unsafe { i915_request_wait_timeout(to_request(fence), flags, timeout) }
    } else {
        unsafe { dma_fence_wait_timeout(fence, flags & I915_WAIT_INTERRUPTIBLE != 0, timeout) }
    }
}

// upstream: gem/i915_gem_wait.c i915_gem_object_boost()
unsafe fn i915_gem_object_boost(resv: *mut DmaResv, flags: u32) {
    let mut cursor: DmaResvIter = unsafe { zeroed() };
    unsafe {
        dma_resv_iter_begin(
            &mut cursor,
            resv,
            dma_resv_usage_rw(flags & I915_WAIT_ALL != 0),
        )
    };
    loop {
        let fence = unsafe { dma_resv_iter_next(&mut cursor) };
        if fence.is_null() {
            break;
        }
        if dma_fence_is_i915(fence) {
            let rq = unsafe { to_request(fence) };
            if !i915_request_started(rq) {
                unsafe { intel_rps_boost(rq) };
            }
        }
    }
    unsafe { dma_resv_iter_end(&mut cursor) };
}

// upstream: gem/i915_gem_wait.c i915_gem_object_wait_reservation()
unsafe fn i915_gem_object_wait_reservation(
    resv: *mut DmaResv,
    flags: u32,
    mut timeout: c_long,
) -> c_long {
    let mut cursor: DmaResvIter = unsafe { zeroed() };
    let mut ret = if timeout != 0 { timeout } else { 1 };

    unsafe { i915_gem_object_boost(resv, flags) };

    unsafe {
        dma_resv_iter_begin(
            &mut cursor,
            resv,
            dma_resv_usage_rw(flags & I915_WAIT_ALL != 0),
        )
    };
    loop {
        let fence = unsafe { dma_resv_iter_next(&mut cursor) };
        if fence.is_null() {
            break;
        }
        ret = unsafe { i915_gem_object_wait_fence(fence, flags, timeout) };
        if ret <= 0 {
            break;
        }

        if timeout != 0 {
            timeout = ret;
        }
    }
    unsafe { dma_resv_iter_end(&mut cursor) };

    ret
}

// upstream: gem/i915_gem_wait.c fence_set_priority()
unsafe fn fence_set_priority(fence: *mut DmaFence, attr: *const I915SchedAttr) {
    if unsafe { dma_fence_is_signaled(fence) } || !dma_fence_is_i915(fence) {
        return;
    }

    let rq = unsafe { to_request(fence) };
    let engine = unsafe { (*rq).engine };
    rcu_read_lock();
    let schedule = unsafe { (*(*engine).sched_engine).schedule };
    if let Some(schedule) = schedule {
        unsafe { schedule(rq, attr) };
    }
    rcu_read_unlock();
}

// upstream: gem/i915_gem_wait.c i915_gem_fence_wait_priority()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_fence_wait_priority(
    fence: *mut DmaFence,
    attr: *const I915SchedAttr,
) {
    if unsafe { dma_fence_is_signaled(fence) } {
        return;
    }

    local_bh_disable();

    if dma_fence_is_array(fence) {
        let array = fence.cast::<DmaFenceArrayView>();
        for index in 0..(unsafe { (*array).num_fences } as usize) {
            let child = unsafe { (*(*array).fences.add(index)) };
            unsafe { fence_set_priority(child, attr) };
        }
    } else if dma_fence_is_chain(fence) {
        // dma_fence_chain_for_each() takes a reference to the current node.
        // The upstream loop breaks after its first iteration, so no walk call
        // is needed and the retained head reference is dropped below.
        let iter = dma_fence_get(fence);
        if !iter.is_null() {
            let chain = iter.cast::<DmaFenceChainView>();
            unsafe { fence_set_priority((*chain).fence, attr) };
        }
        unsafe { dma_fence_put(iter) };
    } else {
        unsafe { fence_set_priority(fence, attr) };
    }

    local_bh_enable();
}

// upstream: gem/i915_gem_wait.c i915_gem_fence_wait_priority_display()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_fence_wait_priority_display(fence: *mut DmaFence) {
    let attr = I915SchedAttr {
        priority: I915_PRIORITY_DISPLAY,
    };
    unsafe { i915_gem_fence_wait_priority(fence, &attr) };
}

// upstream: gem/i915_gem_wait.c i915_gem_object_wait_priority()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_object_wait_priority(
    obj: *mut DrmI915GemObject,
    flags: u32,
    attr: *const I915SchedAttr,
) -> c_int {
    let resv = unsafe { (&(*obj).base.base).resv.cast::<DmaResv>() };
    let mut cursor: DmaResvIter = unsafe { zeroed() };
    unsafe {
        dma_resv_iter_begin(
            &mut cursor,
            resv,
            dma_resv_usage_rw(flags & I915_WAIT_ALL != 0),
        )
    };

    loop {
        let fence = unsafe { dma_resv_iter_next(&mut cursor) };
        if fence.is_null() {
            break;
        }
        unsafe { i915_gem_fence_wait_priority(fence, attr) };
    }
    unsafe { dma_resv_iter_end(&mut cursor) };
    0
}

/// Wait for rendering to the object to be completed.
// upstream: gem/i915_gem_wait.c i915_gem_object_wait()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_object_wait(
    obj: *mut DrmI915GemObject,
    flags: u32,
    timeout: c_long,
) -> c_int {
    might_sleep();
    gem_bug_on!(timeout < 0);

    let resv = unsafe { (&(*obj).base.base).resv.cast::<DmaResv>() };
    let timeout = unsafe { i915_gem_object_wait_reservation(resv, flags, timeout) };
    if timeout < 0 {
        return timeout as c_int;
    }
    if timeout == 0 { -(ETIME as c_int) } else { 0 }
}

// upstream: gem/i915_gem_wait.c nsecs_to_jiffies_timeout()
#[inline]
fn nsecs_to_jiffies_timeout(n: u64) -> c_ulong {
    // Linux 7.2.3 nsecs_to_jiffies64() selects its exact HZ=100 division path.
    let jiffies = n / (NSEC_PER_SEC / HZ as u64);
    core::cmp::min(MAX_JIFFY_OFFSET, jiffies as c_ulong + 1)
}

// upstream: gem/i915_gem_wait.c to_wait_timeout()
fn to_wait_timeout(timeout_ns: i64) -> c_ulong {
    if timeout_ns < 0 {
        return MAX_SCHEDULE_TIMEOUT;
    }
    if timeout_ns == 0 {
        return 0;
    }

    nsecs_to_jiffies_timeout(timeout_ns as u64)
}

#[inline]
fn nsecs_to_jiffies(n: u64) -> c_ulong {
    // Same Linux 7.2.3 HZ=100 fast path as nsecs_to_jiffies64().
    (n / (NSEC_PER_SEC / HZ as u64)) as c_ulong
}

#[inline]
fn ktime_sub(lhs: i64, rhs: i64) -> i64 {
    lhs - rhs
}

#[inline]
fn ktime_to_ns(value: i64) -> i64 {
    value
}

// upstream: gem/i915_gem_wait.c i915_gem_wait_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_wait_ioctl(
    _dev: *mut c_void,
    data: *mut c_void,
    file: *mut c_void,
) -> c_int {
    let args = data.cast::<DrmI915GemWait>();
    if unsafe { (*args).flags } != 0 {
        return -crate::linux_config::EINVAL;
    }

    let obj =
        unsafe { i915_gem_object_lookup(file.cast::<DrmFileObjectLookup>(), (*args).bo_handle) };
    if obj.is_null() {
        return -ENOENT;
    }

    let start = ktime_get();
    let mut ret = unsafe {
        i915_gem_object_wait(
            obj,
            I915_WAIT_INTERRUPTIBLE | I915_WAIT_PRIORITY | I915_WAIT_ALL,
            to_wait_timeout((*args).timeout_ns) as c_long,
        )
    };

    if unsafe { (*args).timeout_ns } > 0 {
        let elapsed = ktime_to_ns(ktime_sub(ktime_get(), start));
        unsafe { (*args).timeout_ns -= elapsed };
        if unsafe { (*args).timeout_ns } < 0 {
            unsafe { (*args).timeout_ns = 0 };
        }

        // Accommodate the one-jiffy mismatch between the nanosecond and
        // jiffies conversions without hiding a longer unfinished wait.
        if ret == -(ETIME as c_int) && nsecs_to_jiffies(unsafe { (*args).timeout_ns } as u64) == 0 {
            unsafe { (*args).timeout_ns = 0 };
        }
        if ret == -(ETIME as c_int) && unsafe { (*args).timeout_ns } != 0 {
            ret = -EAGAIN;
        }
    }

    unsafe { i915_gem_object_put(obj) };
    ret
}

/// Wait for any pending asynchronous migration operation on the object.
// upstream: gem/i915_gem_wait.c i915_gem_object_wait_migration()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_object_wait_migration(
    obj: *mut DrmI915GemObject,
    flags: u32,
) -> c_int {
    might_sleep();
    unsafe { i915_gem_object_wait_moving_fence(obj, flags & I915_WAIT_INTERRUPTIBLE != 0) }
}
