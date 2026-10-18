// SPDX-License-Identifier: GPL-2.0-only
// Copyright 2026 TheKernel contributors.
//! LinuxKPI dma-fence unwrapping: Linux 7.2.3 `drivers/dma-buf/dma-fence-unwrap.c`
//! and `include/linux/dma-fence-unwrap.h`. A cursor walks the members of
//! `dma_fence_chain` and `dma_fence_array` containers; the merge combines
//! several fence trees into one fence that waits for every pending member.

#![allow(unsafe_code)]

use alloc::vec::Vec;
use core::{cmp::Ordering, ptr};

use crate::{
    dma_fence::{
        dma_fence_allocate_private_stub, dma_fence_context_alloc, dma_fence_get,
        dma_fence_is_later, dma_fence_is_signaled, dma_fence_put, dma_fence_timestamp,
    },
    dma_fence_array::{dma_fence_array_create_owned, dma_fence_array_first, dma_fence_array_next},
    dma_fence_chain::{dma_fence_chain_contained, dma_fence_chain_walk},
    types::DmaFence,
};

/// `struct dma_fence_unwrap`: the cursor of a `dma_fence_unwrap_for_each()`.
#[repr(C)]
pub struct DmaFenceUnwrap {
    /// Potential `dma_fence_chain`, but can be any fence.
    pub chain: *mut DmaFence,
    /// Potential `dma_fence_array`, but can be any fence.
    pub array: *mut DmaFence,
    /// Last returned index when `array` is a `dma_fence_array`.
    pub index: u32,
}

impl DmaFenceUnwrap {
    /// A cursor that has not started iterating.
    pub const fn empty() -> Self {
        Self {
            chain: ptr::null_mut(),
            array: ptr::null_mut(),
            index: 0,
        }
    }
}

/// Start a new array iteration over the chain's contained fence.
///
/// # Safety
/// `cursor.chain` must be null or a live fence owned by the cursor.
unsafe fn unwrap_array(cursor: &mut DmaFenceUnwrap) -> *mut DmaFence {
    cursor.array = unsafe { dma_fence_chain_contained(cursor.chain) };
    cursor.index = 0;
    unsafe { dma_fence_array_first(cursor.array) }
}

/// Linux `dma_fence_unwrap_first()`: the first fence under `head`. The cursor
/// takes its own reference to `head`.
///
/// # Safety
/// `head` must be null or a live fence; `cursor` must not be iterating.
pub unsafe fn dma_fence_unwrap_first(
    head: *mut DmaFence,
    cursor: &mut DmaFenceUnwrap,
) -> *mut DmaFence {
    cursor.chain = unsafe { dma_fence_get(head) };
    unsafe { unwrap_array(cursor) }
}

/// Linux `dma_fence_unwrap_next()`: the next fence under the cursor's head, or
/// null when the iteration is complete.
///
/// # Safety
/// `cursor` must have been started by `dma_fence_unwrap_first()`.
pub unsafe fn dma_fence_unwrap_next(cursor: &mut DmaFenceUnwrap) -> *mut DmaFence {
    cursor.index += 1;
    let tmp = unsafe { dma_fence_array_next(cursor.array, cursor.index) };
    if !tmp.is_null() {
        return tmp;
    }
    cursor.chain = unsafe { dma_fence_chain_walk(cursor.chain) };
    unsafe { unwrap_array(cursor) }
}

/// Linux `fence_cmp()`: order by context, then latest-first within a context.
///
/// # Safety
/// Both fences must be live.
unsafe fn fence_cmp(a: *mut DmaFence, b: *mut DmaFence) -> Ordering {
    let (context_a, context_b) = unsafe { ((*a).context, (*b).context) };
    if context_a < context_b {
        return Ordering::Less;
    } else if context_a > context_b {
        return Ordering::Greater;
    }
    if unsafe { dma_fence_is_later(b, a) } {
        Ordering::Greater
    } else if unsafe { dma_fence_is_later(a, b) } {
        Ordering::Less
    } else {
        Ordering::Equal
    }
}

/// Linux `dma_fence_dedup_array()`: keep only the most recent fence of each
/// context, dropping the others. Returns the number of fences kept, which
/// occupy the front of `fences`.
///
/// # Safety
/// Every entry must be a live fence reference owned by the caller.
pub unsafe fn dma_fence_dedup_array(fences: &mut [*mut DmaFence]) -> usize {
    if fences.is_empty() {
        return 0;
    }
    // SAFETY: fence_cmp only reads live fences, which the caller guarantees.
    fences.sort_by(|a, b| unsafe { fence_cmp(*a, *b) });
    // Only keep the most recent fence for each context.
    let mut j = 0;
    for i in 1..fences.len() {
        if unsafe { (*fences[i]).context == (*fences[j]).context } {
            unsafe { dma_fence_put(fences[i]) };
        } else {
            j += 1;
            fences[j] = fences[i];
        }
    }
    j + 1
}

/// Linux `__dma_fence_unwrap_merge()` behind the `dma_fence_unwrap_merge()`
/// macro: a fence that signals when every pending member of every input has
/// signaled. Returns null on allocation failure. The inputs are borrowed.
///
/// # Safety
/// Every input must be a live fence.
pub unsafe fn dma_fence_unwrap_merge(fences: &[*mut DmaFence]) -> *mut DmaFence {
    let mut pending: Vec<*mut DmaFence> = Vec::new();
    let mut out_of_memory = false;
    let mut timestamp: i64 = 0;

    for &head in fences {
        let mut cursor = DmaFenceUnwrap::empty();
        let mut tmp = unsafe { dma_fence_unwrap_first(head, &mut cursor) };
        while !tmp.is_null() {
            if !unsafe { dma_fence_is_signaled(tmp) } {
                if !out_of_memory {
                    if pending.try_reserve(1).is_ok() {
                        pending.push(unsafe { dma_fence_get(tmp) });
                    } else {
                        out_of_memory = true;
                    }
                }
            } else {
                let t = unsafe { dma_fence_timestamp(tmp) };
                if t > timestamp {
                    timestamp = t;
                }
            }
            tmp = unsafe { dma_fence_unwrap_next(&mut cursor) };
        }
    }

    if out_of_memory {
        for fence in pending.drain(..) {
            // SAFETY: every entry is an owned reference retained by this call.
            unsafe { dma_fence_put(fence) };
        }
        return ptr::null_mut();
    }

    match pending.len() {
        0 => unsafe { dma_fence_allocate_private_stub(timestamp) },
        1 => pending[0],
        _ => {
            let kept = unsafe { dma_fence_dedup_array(&mut pending) };
            pending.truncate(kept);
            if kept == 1 {
                return pending[0];
            }
            // SAFETY: the vector owns each reference; the helper transfers
            // them and a kernel-allocated pointer buffer to the array.
            unsafe { dma_fence_array_create_owned(pending, dma_fence_context_alloc(1), 1).cast() }
        }
    }
}
