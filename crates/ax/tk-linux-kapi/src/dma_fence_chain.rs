// SPDX-License-Identifier: GPL-2.0-only
// Copyright 2026 TheKernel contributors.
//! Linux `dma_fence_chain` initialization and helper functions.
//! Upstream reference: `drivers/dma-buf/dma-fence-chain.c`.

#![allow(unsafe_code)]

use core::{
    ffi::c_void,
    ptr,
    sync::atomic::{AtomicPtr, Ordering},
};
use crate::dma_fence::{dma_fence_default_wait, dma_fence_get, dma_fence_get_rcu, dma_fence_put, dma_fence_set_deadline};
use crate::types::{DmaFence, DmaFenceCb, DmaFenceChain, DmaFenceOps, DmaFenceTimestamp, IrqWork, ListHead, Spinlock};

const DMA_FENCE_FLAG_INLINE_LOCK_BIT: u32 = 1;
const DMA_FENCE_FLAG_SEQNO64_BIT: u32 = 2;

#[cfg(not(test))]
use crate::dma_fence::dma_fence_context_alloc;

#[cfg(test)]
mod test_mocks {
    use core::sync::atomic::{AtomicU64, Ordering};
    static NEXT_CONTEXT: AtomicU64 = AtomicU64::new(100);
    pub unsafe fn dma_fence_context_alloc(_num: u32) -> u64 {
        NEXT_CONTEXT.fetch_add(1, Ordering::Relaxed)
    }
}
#[cfg(test)]
use test_mocks::*;

#[inline]
unsafe fn to_dma_fence_chain(fence: *mut DmaFence) -> *mut DmaFenceChain {
    if fence.is_null()
        || unsafe { (*fence).ops != core::ptr::addr_of!(dma_fence_chain_ops).cast::<c_void>() }
    {
        core::ptr::null_mut()
    } else {
        fence.cast::<DmaFenceChain>()
    }
}

#[inline]
unsafe fn __dma_fence_is_later(fence: *mut DmaFence, f1: u64, f2: u64) -> bool {
    if unsafe { (*fence).flags & (1 << DMA_FENCE_FLAG_SEQNO64_BIT) } != 0 {
        f1 > f2
    } else {
        ((f1 as u32).wrapping_sub(f2 as u32) as i32) > 0
    }
}

// upstream: drivers/dma-buf/dma-fence-chain.c dma_fence_chain_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_chain_init(
    chain: *mut DmaFenceChain,
    prev: *mut DmaFence,
    fence: *mut DmaFence,
    mut seqno: u64,
) {
    assert!(!chain.is_null());
    unsafe {
        let prev_chain = to_dma_fence_chain(prev);
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*chain).prev), prev);
        (*chain).fence = fence;
        (*chain).prev_seqno = 0;

        let context = if !prev_chain.is_null() && __dma_fence_is_later(prev, seqno, (*prev).seqno) {
            let ctx = (*prev).context;
            (*chain).prev_seqno = (*prev).seqno;
            ctx
        } else {
            let ctx = dma_fence_context_alloc(1);
            if !prev_chain.is_null() {
                seqno = seqno.max((*prev).seqno);
            }
            ctx
        };

        let base = core::ptr::addr_of_mut!((*chain).base);
        (*base).lock.inline_lock = Spinlock::new();
        (*base).ops = core::ptr::addr_of!(dma_fence_chain_ops).cast::<c_void>();
        (*base).timestamp_union = DmaFenceTimestamp {
            cb_list: core::mem::ManuallyDrop::new(ListHead {
                next: core::ptr::null_mut(),
                prev: core::ptr::null_mut(),
            }),
        };
        crate::list::INIT_LIST_HEAD(core::ptr::addr_of_mut!((*base).timestamp_union.cb_list).cast());
        (*base).context = context;
        (*base).seqno = seqno;
        (*base).flags = (1 << DMA_FENCE_FLAG_SEQNO64_BIT) | (1 << DMA_FENCE_FLAG_INLINE_LOCK_BIT);
        (*base).error = 0;
        crate::memory::kref_init(core::ptr::addr_of_mut!((*base).refcount));
    }
}

fn dma_fence_chain_is(fence: *const DmaFence) -> bool {
    !fence.is_null()
        && unsafe { (*fence).ops == ptr::addr_of!(dma_fence_chain_ops).cast::<c_void>() }
}

pub unsafe fn dma_fence_chain_contained(fence: *mut DmaFence) -> *mut DmaFence {
    if dma_fence_chain_is(fence) {
        unsafe { (*fence.cast::<DmaFenceChain>()).fence }
    } else {
        fence
    }
}

#[unsafe(no_mangle)]
pub static dma_fence_chain_ops: DmaFenceOps = DmaFenceOps {
    get_driver_name: Some(dma_fence_chain_driver_name),
    get_timeline_name: Some(dma_fence_chain_timeline_name),
    enable_signaling: Some(dma_fence_chain_enable_signaling),
    signaled: Some(dma_fence_chain_signaled),
    wait: Some(dma_fence_default_wait),
    release: Some(dma_fence_chain_release),
    set_deadline: Some(dma_fence_chain_set_deadline),
};

unsafe extern "C" fn dma_fence_chain_driver_name(_: *mut DmaFence) -> *const core::ffi::c_char {
    c"dma_fence_chain".as_ptr()
}

unsafe extern "C" fn dma_fence_chain_timeline_name(_: *mut DmaFence) -> *const core::ffi::c_char {
    c"unbound".as_ptr()
}

unsafe extern "C" fn dma_fence_chain_enable_signaling(fence: *mut DmaFence) -> bool {
    let chain = fence.cast::<DmaFenceChain>();
    let base = unsafe { ptr::addr_of_mut!((*chain).base) };
    dma_fence_get(base);
    let mut iter = dma_fence_get(fence);
    while !iter.is_null() {
        let child = unsafe { dma_fence_chain_contained(iter) };
        if child.is_null() {
            iter = unsafe { dma_fence_chain_walk(iter) };
            continue;
        }
        dma_fence_get(child);
        let err = unsafe {
            crate::dma_fence::dma_fence_add_callback(
                child,
                ptr::addr_of_mut!((*chain).work).cast::<DmaFenceCb>(),
                dma_fence_chain_callback,
            )
        };
        if err == 0 {
            unsafe { dma_fence_put(iter) };
            return true;
        }
        unsafe { dma_fence_put(child) };
        iter = unsafe { dma_fence_chain_walk(iter) };
    }
    unsafe { dma_fence_put(base) };
    false
}

unsafe extern "C" fn dma_fence_chain_callback(fence: *mut DmaFence, callback: *mut DmaFenceCb) {
    let chain = unsafe {
        callback
            .cast::<u8>()
            .sub(core::mem::offset_of!(DmaFenceChain, work))
            .cast::<DmaFenceChain>()
    };
    unsafe {
        crate::irq_work::init_irq_work(ptr::addr_of_mut!((*chain).work), dma_fence_chain_work);
        crate::irq_work::irq_work_queue(ptr::addr_of_mut!((*chain).work));
        dma_fence_put(fence);
    }
}

unsafe extern "C" fn dma_fence_chain_work(work: *mut IrqWork) {
    let chain = unsafe {
        work.cast::<u8>()
            .sub(core::mem::offset_of!(DmaFenceChain, work))
            .cast::<DmaFenceChain>()
    };
    unsafe {
        if !dma_fence_chain_enable_signaling(ptr::addr_of_mut!((*chain).base)) {
            crate::dma_fence::dma_fence_signal(ptr::addr_of_mut!((*chain).base));
        }
        dma_fence_put(ptr::addr_of_mut!((*chain).base));
    }
}

unsafe extern "C" fn dma_fence_chain_signaled(fence: *mut DmaFence) -> bool {
    let mut iter = dma_fence_get(fence);
    while !iter.is_null() {
        let child = unsafe { dma_fence_chain_contained(iter) };
        if child.is_null()
            || !unsafe { crate::dma_fence::dma_fence_is_signaled(child) }
        {
            unsafe { dma_fence_put(iter) };
            return false;
        }
        iter = unsafe { dma_fence_chain_walk(iter) };
    }
    true
}

unsafe extern "C" fn dma_fence_chain_release(fence: *mut DmaFence) {
    let chain = fence.cast::<DmaFenceChain>();
    let mut prev = unsafe { (*chain).prev };
    while !prev.is_null() {
        if !dma_fence_chain_is(prev)
            || unsafe { crate::memory::kref_read(&(*prev).refcount) } > 1
        {
            break;
        }
        let prev_chain = prev.cast::<DmaFenceChain>();
        let next = unsafe { (*prev_chain).prev };
        unsafe {
            (*chain).prev = next;
            (*prev_chain).prev = ptr::null_mut();
            dma_fence_put(prev);
        }
        prev = next;
    }
    unsafe {
        dma_fence_put(prev);
        dma_fence_put((*chain).fence);
        crate::dma_fence::dma_fence_free(fence);
    }
}

unsafe extern "C" fn dma_fence_chain_set_deadline(fence: *mut DmaFence, deadline: i64) {
    let mut iter = dma_fence_get(fence);
    while !iter.is_null() {
        let child = unsafe { dma_fence_chain_contained(iter) };
        if !child.is_null() {
            unsafe { dma_fence_set_deadline(child, deadline) };
        }
        iter = unsafe { dma_fence_chain_walk(iter) };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_chain_walk(fence: *mut DmaFence) -> *mut DmaFence {
    if !dma_fence_chain_is(fence) {
        unsafe { dma_fence_put(fence) };
        return ptr::null_mut();
    }
    let chain = fence.cast::<DmaFenceChain>();
    let mut result = ptr::null_mut();
    loop {
        crate::rcu::rcu_read_lock();
        let prev = unsafe { AtomicPtr::from_ptr(ptr::addr_of_mut!((*chain).prev)) }
            .load(Ordering::Acquire);
        let prev_ref = dma_fence_get_rcu(prev);
        crate::rcu::rcu_read_unlock();
        if prev_ref.is_null() {
            break;
        }
        let replacement = if dma_fence_chain_is(prev_ref) {
            let previous = prev_ref.cast::<DmaFenceChain>();
            let child = unsafe { (*previous).fence };
            if child.is_null()
                || !unsafe { crate::dma_fence::dma_fence_is_signaled(child) }
            {
                result = prev_ref;
                break;
            }
            crate::rcu::rcu_read_lock();
            let replacement = unsafe {
                dma_fence_get_rcu(
                    AtomicPtr::from_ptr(ptr::addr_of_mut!((*previous).prev))
                        .load(Ordering::Acquire),
                )
            };
            crate::rcu::rcu_read_unlock();
            replacement
        } else if unsafe { !crate::dma_fence::dma_fence_is_signaled(prev_ref) } {
            result = prev_ref;
            break;
        } else {
            ptr::null_mut()
        };
        let result = unsafe {
            AtomicPtr::from_ptr(ptr::addr_of_mut!((*chain).prev)).compare_exchange(
                prev_ref,
                replacement,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
        };
        if result.is_ok() {
            unsafe { dma_fence_put(prev_ref) };
        } else {
            unsafe { dma_fence_put(replacement) };
        }
        unsafe { dma_fence_put(prev_ref) };
    }
    unsafe { dma_fence_put(fence) };
    result
}

pub fn dma_fence_chain_alloc<T>() -> *mut T {
    crate::memory::kmalloc(
        core::mem::size_of::<DmaFenceChain>(),
        crate::config::GFP_KERNEL,
    )
    .cast()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_chain_free(chain: *mut c_void) {
    if !chain.is_null() {
        unsafe { crate::memory::kfree(chain) };
    }
}

/// Linux `dma_fence_chain_find_seqno()`. The native timeline probe runs first
/// through the installed native fence hooks; the chain walk follows.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_chain_find_seqno(fence: *mut *mut DmaFence, seqno: u64) -> i32 {
    if seqno == 0 {
        return 0;
    }
    if fence.is_null() || unsafe { (*fence).is_null() } {
        return -(crate::config::EINVAL as i32);
    }
    unsafe { dma_fence_chain_find_seqno_walk(fence, seqno) }
}

/// Walk the chain for `seqno` once the native timeline probe has declined it.
pub unsafe fn dma_fence_chain_find_seqno_walk(fence: *mut *mut DmaFence, seqno: u64) -> i32 {
    let start = unsafe { *fence };
    if !dma_fence_chain_is(start) || unsafe { (*start).seqno < seqno } {
        return -(crate::config::EINVAL as i32);
    }
    let root = start.cast::<DmaFenceChain>();
    let context = unsafe { (*root).base.context };
    let mut iter = dma_fence_get(start);
    loop {
        if !dma_fence_chain_is(iter) {
            break;
        }
        let current = iter.cast::<DmaFenceChain>();
        if unsafe { (*iter).context != context || (*current).prev_seqno < seqno } {
            break;
        }
        iter = unsafe { dma_fence_chain_walk(iter) };
    }
    unsafe {
        *fence = iter;
        dma_fence_put(start);
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dma_fence_chain_init_progression() {
        let mut node1 = core::mem::MaybeUninit::<DmaFenceChain>::zeroed();
        let chain1 = node1.as_mut_ptr();
        unsafe {
            dma_fence_chain_init(chain1, core::ptr::null_mut(), core::ptr::null_mut(), 10);
            assert_eq!((*chain1).prev, core::ptr::null_mut());
            assert_eq!((*chain1).prev_seqno, 0);
            assert_eq!((*chain1).base.seqno, 10);
            assert!((*chain1).base.context >= 100);
        }

        // Sequential later point in same chain: reuses context
        let mut node2 = core::mem::MaybeUninit::<DmaFenceChain>::zeroed();
        let chain2 = node2.as_mut_ptr();
        unsafe {
            let prev_fence = core::ptr::addr_of_mut!((*chain1).base);
            dma_fence_chain_init(chain2, prev_fence, core::ptr::null_mut(), 15);
            assert_eq!((*chain2).prev, prev_fence);
            assert_eq!((*chain2).prev_seqno, 10);
            assert_eq!((*chain2).base.seqno, 15);
            assert_eq!((*chain2).base.context, (*chain1).base.context);
        }

        // Out of order point: gets fresh context and clamped seqno
        let mut node3 = core::mem::MaybeUninit::<DmaFenceChain>::zeroed();
        let chain3 = node3.as_mut_ptr();
        unsafe {
            let prev_fence2 = core::ptr::addr_of_mut!((*chain2).base);
            dma_fence_chain_init(chain3, prev_fence2, core::ptr::null_mut(), 5);
            assert_eq!((*chain3).prev, prev_fence2);
            assert_eq!((*chain3).prev_seqno, 0);
            assert_eq!((*chain3).base.seqno, 15); // clamped to max(15, 5)
            assert_ne!((*chain3).base.context, (*chain2).base.context);
        }
    }
}
