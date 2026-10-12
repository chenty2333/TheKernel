// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Linux `dma_fence_chain` initialization and helper functions.
//! Upstream reference: `drivers/dma-buf/dma-fence-chain.c`.

#![allow(unsafe_code)]

use core::ffi::c_void;
use crate::types::{DmaFence, DmaFenceChain, DmaFenceTimestamp, ListHead, Spinlock};

const DMA_FENCE_FLAG_INLINE_LOCK_BIT: u32 = 1;
const DMA_FENCE_FLAG_SEQNO64_BIT: u32 = 2;

#[cfg(not(test))]
unsafe extern "C" {
    pub static dma_fence_chain_ops: c_void;
    pub fn dma_fence_context_alloc(num: u32) -> u64;
}

#[cfg(test)]
mod test_mocks {
    use core::sync::atomic::{AtomicU64, Ordering};
    pub static dma_fence_chain_ops: u8 = 42;
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
