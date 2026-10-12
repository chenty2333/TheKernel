//! Port of Linux `drivers/dma-buf/dma-fence-chain.c` and the merge half of
//! `dma-fence-unwrap.c`, as used by timeline syncobjs.
//!
//! A timeline syncobj holds the newest chain node. Each node names its
//! predecessor, its own point's fence and the sequence number of that
//! predecessor, so the node for a point is found by walking backwards.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};

use axerrno::{AxError, AxResult};

use crate::drm::fence::Fence;

/// dma_fence_context_alloc(1): every fresh chain context is distinct.
static NEXT_CONTEXT: AtomicU64 = AtomicU64::new(1);

fn context_alloc() -> u64 {
    NEXT_CONTEXT.fetch_add(1, Ordering::Relaxed)
}

/// dma_fence_chain_init(): create the node for `seqno` after `prev`.
///
/// A later point reuses the predecessor's context, so the chain stays
/// monotonic. Otherwise (out-of-order or first point) it opens a new context
/// and clamps the sequence number to the predecessor's.
pub(crate) fn init(
    prev: Option<Arc<Fence>>,
    fence: Arc<Fence>,
    mut seqno: u64,
) -> AxResult<Arc<Fence>> {
    let mut prev_seqno = 0;
    let context = match prev.as_ref().and_then(|prev| prev.chain()) {
        Some(link) if seqno > link.seqno => {
            prev_seqno = link.seqno;
            link.context
        }
        Some(link) => {
            seqno = seqno.max(link.seqno);
            context_alloc()
        }
        None => context_alloc(),
    };
    Fence::chain_node(prev, fence, context, seqno, prev_seqno)
}

/// dma_fence_chain_walk(): consume `fence` and return its first unsignaled
/// predecessor, garbage-collecting predecessors whose own point has signaled.
/// Returns `None` at the end of the chain (or for a non-chain fence).
pub(crate) fn walk(fence: Option<Arc<Fence>>) -> Option<Arc<Fence>> {
    let fence = fence?;
    let link = fence.chain()?;
    loop {
        let prev = link.prev()?;
        let replacement = match prev.chain() {
            Some(prev_link) => {
                if !prev_link.contained().is_signaled() {
                    return Some(prev);
                }
                prev_link.prev()
            }
            None => {
                if !prev.is_signaled() {
                    return Some(prev);
                }
                None
            }
        };
        link.replace_prev_if(&prev, replacement);
    }
}

/// dma_fence_chain_find_seqno(): retarget `pfence` to the node that holds
/// `seqno`, or to `None` when every node of that point has already signaled
/// and the chain has been walked past it. Point zero never resolves.
pub(crate) fn find_seqno(pfence: &mut Option<Arc<Fence>>, seqno: u64) -> AxResult<()> {
    if seqno == 0 {
        return Ok(());
    }
    let head = pfence.clone().ok_or(AxError::InvalidInput)?;
    let head_link = head.chain().ok_or(AxError::InvalidInput)?;
    if head_link.seqno < seqno {
        return Err(AxError::InvalidInput);
    }
    let context = head_link.context;
    let mut current = Some(head);
    *pfence = loop {
        let Some(node) = current.take() else {
            break None;
        };
        let stop = match node.chain() {
            Some(link) => link.context != context || link.prev_seqno < seqno,
            None => true,
        };
        if stop {
            break Some(node);
        }
        current = walk(Some(node));
    };
    Ok(())
}

/// dma_fence_chain_find_seqno() as a lookup that never mutates its input. A
/// missing seqno returns `None`, and a point that has signaled resolves to the
/// stub fence (the same result the Linux callers substitute for NULL).
pub(crate) fn lookup(head: Option<Arc<Fence>>, seqno: u64) -> AxResult<Option<Arc<Fence>>> {
    let mut slot = head;
    find_seqno(&mut slot, seqno)?;
    Ok(slot)
}

/// Garbage-collect a chain after a new node has been appended, as
/// drm_syncobj_add_point() does by walking the previous head once.
pub(crate) fn collect(mut current: Option<Arc<Fence>>) {
    while current.is_some() {
        current = walk(current);
    }
}

/// dma_fence_unwrap_merge() of one fence: the pending leaves of its chain, as
/// a single fence. No pending leaf gives a signaled stub.
pub(crate) fn unwrap_merge(fence: Arc<Fence>) -> AxResult<Arc<Fence>> {
    let mut pending = Vec::new();
    let mut current = Some(fence);
    while let Some(node) = current.take() {
        let contained = match node.chain() {
            Some(link) => link.contained().clone(),
            None => node.clone(),
        };
        Fence::collect_pending(&contained, &mut pending)?;
        current = walk(Some(node));
    }
    match pending.len() {
        0 => Ok(Fence::new(true)),
        1 => Ok(pending.pop().unwrap()),
        _ => Fence::join(&pending),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(prev: Option<Arc<Fence>>, fence: Arc<Fence>, seqno: u64) -> Arc<Fence> {
        init(prev, fence, seqno).unwrap()
    }

    #[test]
    fn chain_point_completes_only_after_every_older_producer() {
        let a = Fence::new(false);
        let b = Fence::new(false);
        let node1 = point(None, a.clone(), 1);
        let node2 = point(Some(node1.clone()), b.clone(), 2);
        assert!(!node2.is_signaled());
        b.signal();
        // The newer producer is not enough: point 2 also waits for point 1.
        assert!(!node2.is_signaled());
        a.signal();
        assert!(node2.is_signaled());
        assert!(node1.is_signaled());
    }

    #[test]
    fn chain_error_is_visible_on_the_point_and_still_completes_it() {
        let failed = Fence::new(false);
        let node = point(None, failed.clone(), 3);
        failed.signal_error();
        assert!(node.is_signaled());
        assert!(node.is_failed());
        // The exact contained fence still reports the failure to direct users.
        assert!(node.chain().unwrap().contained().is_failed());
    }

    #[test]
    fn find_seqno_selects_the_first_point_at_or_after_the_request() {
        let first = Fence::new(false);
        let second = Fence::new(false);
        let n1 = point(None, first, 1);
        let n5 = point(Some(n1.clone()), second, 5);
        let mut slot = Some(n5.clone());
        find_seqno(&mut slot, 3).unwrap();
        assert!(Arc::ptr_eq(slot.as_ref().unwrap(), &n5));
        let mut slot = Some(n5.clone());
        find_seqno(&mut slot, 1).unwrap();
        assert!(Arc::ptr_eq(slot.as_ref().unwrap(), &n1));
        let mut slot = Some(n5.clone());
        assert_eq!(find_seqno(&mut slot, 6), Err(AxError::InvalidInput));
        let mut slot = Some(n5);
        find_seqno(&mut slot, 0).unwrap();
        assert!(slot.is_some());
    }

    #[test]
    fn out_of_order_point_opens_a_new_context_and_clamps_seqno() {
        let first = Fence::new(false);
        let late = Fence::new(false);
        let n5 = point(None, first, 5);
        let n4 = point(Some(n5.clone()), late, 4);
        let link5 = n5.chain().unwrap();
        let link4 = n4.chain().unwrap();
        assert_ne!(link4.context, link5.context);
        assert_eq!(link4.seqno, 5);
        assert_eq!(link4.prev_seqno, 0);
    }

    #[test]
    fn signaled_predecessors_are_garbage_collected_by_walk() {
        let a = Fence::new(false);
        let b = Fence::new(false);
        let n1 = point(None, a.clone(), 1);
        let n2 = point(Some(n1.clone()), b.clone(), 2);
        a.signal();
        // Walking past n1 removes it from n2's chain (its own point signaled).
        assert!(walk(Some(n2.clone())).is_none());
        assert!(n2.chain().unwrap().prev().is_none());
        b.signal();
        assert!(n2.is_signaled());
    }

    #[test]
    fn unwrap_merge_keeps_only_pending_contained_fences() {
        let done = Fence::new(false);
        let pending = Fence::new(false);
        let n1 = point(None, done.clone(), 1);
        let n2 = point(Some(n1), pending.clone(), 2);
        done.signal();
        let merged = unwrap_merge(n2).unwrap();
        assert!(!merged.is_signaled());
        pending.signal();
        assert!(merged.is_signaled());
        assert!(!unwrap_merge(Fence::new(false)).unwrap().is_signaled());
        assert!(unwrap_merge(Fence::new(true)).unwrap().is_signaled());
    }
}
