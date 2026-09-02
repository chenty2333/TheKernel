//! Allocation-free final release of VMA-owned kernel objects.
//!
//! A mapping may be split, cloned, moved, or torn down from contexts which
//! cannot run arbitrary Rust destructors.  `DeferredMappingFinalizer` keeps
//! the finalizer object and its intrusive publication node allocated while a
//! mapping is first installed; final VMA release only pushes that node to the
//! policy worker.

use alloc::boxed::Box;
use core::{
    any::Any,
    ptr,
    sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering},
};

use axerrno::{AxError, AxResult};

/// Task-context cleanup owned by a logical VMA lease.
///
/// Implementors must tolerate finalization after the last mapping has already
/// disappeared from every page table.  The receiver is consumed so both the
/// implementation and its destructor execute only in the policy worker.
pub(crate) trait MappingFinalizer: Any + Send + Sync {
    fn as_any(&self) -> &dyn Any;
    fn finalize(self: Box<Self>);
}

static DEFERRED_MAPPING_FINALIZERS: AtomicPtr<DeferredMappingFinalizerInner> =
    AtomicPtr::new(ptr::null_mut());
static DEFERRED_MAPPING_FINALIZER_CREDITS: AtomicUsize = AtomicUsize::new(0);
static DEFERRED_MAPPING_FINALIZER_DRAINING: AtomicBool = AtomicBool::new(false);

const MAX_LIVE_DEFERRED_MAPPING_FINALIZERS: usize = 65_536;

/// One charged preallocation slot.  Keeping the charge as a field of the
/// leaked node means every successful construction releases it exactly when
/// the policy worker destroys that node.  In particular, a failed `Box::try_new`
/// drops this local guard and cannot strand a credit.
struct DeferredMappingFinalizerCredit;

impl DeferredMappingFinalizerCredit {
    fn try_acquire() -> AxResult<Self> {
        DEFERRED_MAPPING_FINALIZER_CREDITS
            .try_update(Ordering::AcqRel, Ordering::Acquire, |live| {
                (live < MAX_LIVE_DEFERRED_MAPPING_FINALIZERS).then_some(live + 1)
            })
            .map_err(|_| AxError::NoMemory)?;
        Ok(Self)
    }
}

impl Drop for DeferredMappingFinalizerCredit {
    fn drop(&mut self) {
        DEFERRED_MAPPING_FINALIZER_CREDITS.fetch_sub(1, Ordering::AcqRel);
    }
}

struct DeferredMappingFinalizerInner {
    next: AtomicPtr<Self>,
    references: AtomicUsize,
    finalizer: Box<dyn MappingFinalizer>,
    _credit: DeferredMappingFinalizerCredit,
}

/// An intrusive, allocation-free cloneable VMA ownership lease.
///
/// `Clone` performs only a refcount increment.  Final `Drop` never takes a
/// lock, allocates, or invokes the finalizer: it solely publishes its already
/// allocated node and wakes the task-context policy worker.
pub(crate) struct DeferredMappingFinalizer {
    inner: ptr::NonNull<DeferredMappingFinalizerInner>,
}

// The node is immutable after construction apart from its publication link
// and reference count; the finalizer itself is only touched by its sole owner.
unsafe impl Send for DeferredMappingFinalizer {}
unsafe impl Sync for DeferredMappingFinalizer {}

impl DeferredMappingFinalizer {
    /// Allocates the one node and reserves the one bounded live-lease credit.
    pub(crate) fn try_new(finalizer: Box<dyn MappingFinalizer>) -> AxResult<Self> {
        let credit = DeferredMappingFinalizerCredit::try_acquire()?;
        let inner = Box::try_new(DeferredMappingFinalizerInner {
            next: AtomicPtr::new(ptr::null_mut()),
            references: AtomicUsize::new(1),
            finalizer,
            _credit: credit,
        })
        .map_err(|_| AxError::NoMemory)?;
        Ok(Self {
            inner: ptr::NonNull::from(Box::leak(inner)),
        })
    }

    /// Stable identity for merge compatibility.  This is not an externally
    /// visible object ID and is safe only while a lease holds the node alive.
    pub(crate) fn identity(&self) -> usize {
        self.inner.as_ptr() as usize
    }

    /// Borrows the concrete task-context owner while this lease keeps its
    /// node alive.  MM uses only the opaque identity; subsystem code may use
    /// this after dropping the address-space lock to perform a synchronous
    /// logical retirement whose eventual deferred finalizer becomes a no-op.
    pub(crate) fn downcast_ref<T: MappingFinalizer>(&self) -> Option<&T> {
        // SAFETY: this lease contributes one reference, so the immutable
        // finalizer allocation cannot be published or consumed concurrently.
        unsafe { self.inner.as_ref() }
            .finalizer
            .as_any()
            .downcast_ref()
    }
}

impl Clone for DeferredMappingFinalizer {
    fn clone(&self) -> Self {
        // SAFETY: the source lease keeps its immutable node alive throughout
        // this atomic retain.
        let inner = unsafe { self.inner.as_ref() };
        retain_mapping_finalizer(&inner.references);
        Self { inner: self.inner }
    }
}

impl Drop for DeferredMappingFinalizer {
    fn drop(&mut self) {
        // SAFETY: every counted clone has exclusive ownership of one logical
        // reference and the final release owns the unpublished node.
        let inner = unsafe { self.inner.as_ref() };
        if !release_mapping_finalizer(&inner.references) {
            return;
        }

        let node = self.inner.as_ptr();
        let mut head = DEFERRED_MAPPING_FINALIZERS.load(Ordering::Acquire);
        loop {
            // SAFETY: no consumer can observe `node` until its release-store
            // publication succeeds, so only this final Drop writes `next`.
            unsafe { (*node).next.store(head, Ordering::Relaxed) };
            match DEFERRED_MAPPING_FINALIZERS.compare_exchange_weak(
                head,
                node,
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    crate::deferred_work::wake_policy_worker();
                    return;
                }
                Err(observed) => head = observed,
            }
        }
    }
}

fn retain_mapping_finalizer(references: &AtomicUsize) {
    let mut current = references.load(Ordering::Relaxed);
    loop {
        if current == usize::MAX {
            // Saturation deliberately turns this lease into a permanent leak:
            // an uncounted clone must never permit premature finalization.
            return;
        }
        match references.compare_exchange_weak(
            current,
            current.saturating_add(1),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return,
            Err(observed) => current = observed,
        }
    }
}

fn release_mapping_finalizer(references: &AtomicUsize) -> bool {
    let mut current = references.load(Ordering::Acquire);
    loop {
        if current == usize::MAX || current == 0 {
            return false;
        }
        match references.compare_exchange_weak(
            current,
            current - 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return current == 1,
            Err(observed) => current = observed,
        }
    }
}

fn pop_mapping_finalizer() -> Option<Box<DeferredMappingFinalizerInner>> {
    let mut head = DEFERRED_MAPPING_FINALIZERS.load(Ordering::Acquire);
    loop {
        if head.is_null() {
            return None;
        }
        // SAFETY: publication retains the allocation until the unique policy
        // consumer removes it.
        let next = unsafe { (*head).next.load(Ordering::Relaxed) };
        match DEFERRED_MAPPING_FINALIZERS.compare_exchange_weak(
            head,
            next,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                // SAFETY: successful removal transfers the sole allocation
                // owner to this task-context drain.
                return Some(unsafe { Box::from_raw(head) });
            }
            Err(observed) => head = observed,
        }
    }
}

struct MappingFinalizerDrainGuard;

impl MappingFinalizerDrainGuard {
    fn try_enter() -> Option<Self> {
        DEFERRED_MAPPING_FINALIZER_DRAINING
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
            .then_some(Self)
    }
}

impl Drop for MappingFinalizerDrainGuard {
    fn drop(&mut self) {
        DEFERRED_MAPPING_FINALIZER_DRAINING.store(false, Ordering::Release);
    }
}

pub(crate) fn has_deferred_mapping_finalizer_work() -> bool {
    !DEFERRED_MAPPING_FINALIZERS
        .load(Ordering::Acquire)
        .is_null()
}

/// Runs at most `budget` finalizers in task context.  The policy worker yields
/// between calls when producers keep publishing work, preventing teardown from
/// starving the rest of the policy queues.
pub(crate) fn drain_deferred_mapping_finalizers(budget: usize) {
    let Some(_guard) = MappingFinalizerDrainGuard::try_enter() else {
        return;
    };
    for _ in 0..budget {
        let Some(inner) = pop_mapping_finalizer() else {
            return;
        };
        let DeferredMappingFinalizerInner { finalizer, .. } = *inner;
        finalizer.finalize();
    }
}
