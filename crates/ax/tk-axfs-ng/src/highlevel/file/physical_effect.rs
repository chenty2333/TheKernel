//! Physical I/O effects and their reset proofs.

use super::*;

/// Owned high-level physical effect for a direct ext4 regular-file request.
///
/// Result of attempting to settle an owned effect.  `Retain` is deliberately
/// not a normal I/O error: the caller must keep the effect (and therefore its
/// range lease, staged cache transaction, and pin owner) until exact device
/// retirement is observed.  `Settled` or an exact reset proof permits the
/// owner to be dropped.
#[cfg(feature = "ext4")]
pub enum PhysicalIoSettleOutcome {
    Settled {
        result: VfsResult<usize>,
    },
    Retain {
        reason: PhysicalIoPendingReason,
    },
    /// Every published device handle has retired, but filesystem
    /// revalidation observed a transient `ResourceBusy`. The effect remains
    /// physically owned and must be retried by the task-context completion
    /// worker without submitting the device request again.
    RetryFinalization,
}

/// Proof that the lower block queue has stopped all access to a published
/// physical effect.  The proof is intentionally not constructible from the
/// public fields of [`BlockResetOutcome`]; callers must obtain it from the
/// lower reset result and a quarantined queue never produces one.
#[cfg(feature = "ext4")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysicalIoResetProof {
    pub(super) kind: PhysicalIoResetProofKind,
}

#[cfg(feature = "ext4")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PhysicalIoResetProofKind {
    Quiesced,
    Retired,
}

#[cfg(feature = "ext4")]
impl PhysicalIoResetProof {
    /// Converts an exact lower reset result into the upper-layer quiescence
    /// proof.  `Quarantined` deliberately has no conversion: upper owners
    /// must remain in typed custody until a later reset proves quiescence.
    pub fn from_lower_reset(outcome: BlockResetOutcome) -> Option<Self> {
        let kind = match outcome {
            BlockResetOutcome::Quiesced => PhysicalIoResetProofKind::Quiesced,
            BlockResetOutcome::Retired => PhysicalIoResetProofKind::Retired,
            BlockResetOutcome::Quarantined => return None,
        };
        Some(Self { kind })
    }
}

/// The exact inode reference, lower filesystem effect, range lease, and
/// staged cache pages all move together across a worker boundary.  No user
/// SG borrow or filesystem spin guard is retained.  A published request can
/// never become a fallback: abandoning it before exact retirement is a
/// fail-stop quarantine, and its owner fields are intentionally leaked rather
/// than released while device DMA may still be active.
#[cfg(feature = "ext4")]
pub struct PhysicalIoEffect {
    pub(super) location: ManuallyDrop<Location>,
    pub(super) inode: ManuallyDrop<Arc<crate::fs::ext4::Inode>>,
    pub(super) effect: Ext4PhysicalIoEffect,
    pub(super) range_lease: Option<RangeCacheLease>,
    pub(super) invalidation: Option<CachedPageInvalidationTransaction>,
    // A physical write may remain in flight after the prepare caller
    // returns. Retain the checked inode admission until exact retirement and
    // cache/range settlement; a borrowed witness is not sufficient here.
    pub(super) native_mutation: Option<FileAttrMutationGuard>,
    pub(super) published: bool,
    pub(super) quarantined: bool,
    pub(super) finalized: bool,
}

#[cfg(feature = "ext4")]
pub type PreparedPhysicalIoEffect = PhysicalIoEffect;

#[cfg(feature = "ext4")]
impl PhysicalIoEffect {
    pub(super) fn new(
        location: Location,
        inode: Arc<crate::fs::ext4::Inode>,
        effect: Ext4PhysicalIoEffect,
        range_lease: RangeCacheLease,
        invalidation: CachedPageInvalidationTransaction,
        native_mutation: Option<FileAttrMutationGuard>,
    ) -> Self {
        Self {
            location: ManuallyDrop::new(location),
            inode: ManuallyDrop::new(inode),
            effect,
            range_lease: Some(range_lease),
            invalidation: Some(invalidation),
            native_mutation,
            published: false,
            quarantined: false,
            finalized: false,
        }
    }

    pub fn plan(&self) -> PhysicalIoPlan {
        self.effect.plan()
    }

    pub fn state(&self) -> PhysicalIoEffectState {
        // Reset retirement is a high-level terminal transition.  The lower
        // effect has no logical completion to mark, but the reset proof is
        // enough to make all physical access impossible, so expose the same
        // drop-safe terminal state as an exact settled effect.
        if self.finalized {
            PhysicalIoEffectState::Finalized
        } else {
            self.effect.state()
        }
    }

    pub fn publication(&self) -> Option<PhysicalIoPublication> {
        self.effect.publication()
    }

    pub fn is_published(&self) -> bool {
        self.published
    }

    pub fn is_quarantined(&self) -> bool {
        self.quarantined
    }

    pub(super) unsafe fn publish_route(
        &mut self,
        kernel_worker: bool,
    ) -> VfsResult<PhysicalIoPublishOutcome> {
        let submitted = if kernel_worker {
            unsafe {
                self.inode
                    .publish_owned_physical_effect_kernel(&mut self.effect)
            }
        } else {
            unsafe { self.inode.publish_owned_physical_effect(&mut self.effect) }
        };
        let outcome = match submitted {
            Ok(outcome) => outcome,
            Err(error) => {
                // A lower error is not enough evidence that a driver did not
                // expose a descriptor.  Keep every owner in fail-stop mode;
                // adapters that can prove queue-full/unsupported return the
                // explicit NotSubmitted outcome instead.
                self.published = true;
                self.quarantined = true;
                return Err(error);
            }
        };
        match outcome {
            PhysicalIoPublishOutcome::NotSubmitted(_) => {}
            PhysicalIoPublishOutcome::Published(_) => self.published = true,
            PhysicalIoPublishOutcome::Terminal(_) => {
                self.published = true;
                self.quarantined = true;
            }
        }
        Ok(outcome)
    }

    /// Publishes all mapped extents in one atomic exact-route batch. The
    /// caller remains the exact completion waiter for this compatibility path.
    pub unsafe fn publish(&mut self) -> VfsResult<PhysicalIoPublishOutcome> {
        unsafe { self.publish_route(false) }
    }

    /// Publishes an io_uring-owned effect to the device-global task-context
    /// completion worker. The lower broker still authenticates completion by
    /// raw handle and cookie; this route is distinct from synchronous exact
    /// waiters so the worker cannot steal their mailbox records.
    pub unsafe fn publish_kernel(&mut self) -> VfsResult<PhysicalIoPublishOutcome> {
        unsafe { self.publish_route(true) }
    }

    /// Settles after observing exact handle/cookie completions.  Device
    /// failures and terminal partial publications are settled logical
    /// failures once every accepted handle has retired; malformed/missing
    /// observations return `Retain` and keep all owners live.
    pub fn settle(&mut self, completions: &[PhysicalIoCompletion]) -> PhysicalIoSettleOutcome {
        if self.finalized {
            return PhysicalIoSettleOutcome::Retain {
                reason: PhysicalIoPendingReason::NotPublished,
            };
        }
        if !self.published {
            return PhysicalIoSettleOutcome::Retain {
                reason: PhysicalIoPendingReason::NotPublished,
            };
        }
        if self.quarantined && self.effect.publication().is_none() {
            return PhysicalIoSettleOutcome::Retain {
                reason: PhysicalIoPendingReason::MalformedPublication,
            };
        }
        match self
            .inode
            .settle_owned_physical_effect(&mut self.effect, completions)
        {
            PhysicalIoSettlement::Retain(reason) => PhysicalIoSettleOutcome::Retain { reason },
            PhysicalIoSettlement::Settled { plan, success } => self.finalize_settled(plan, success),
        }
    }

    /// Retries only the filesystem finalization phase after all exact device
    /// handles have already retired. This path never records another device
    /// completion and never republishes the effect.
    pub fn retry_finalization(&mut self) -> PhysicalIoSettleOutcome {
        let (plan, success) = match self.effect.state() {
            PhysicalIoEffectState::Completed => (self.effect.plan(), true),
            PhysicalIoEffectState::SettledFailure => (self.effect.plan(), false),
            _ => {
                return PhysicalIoSettleOutcome::Retain {
                    reason: PhysicalIoPendingReason::NotPublished,
                };
            }
        };
        self.finalize_settled(plan, success)
    }

    pub(super) fn finalize_settled(
        &mut self,
        plan: PhysicalIoPlan,
        success: bool,
    ) -> PhysicalIoSettleOutcome {
        let result = self
            .inode
            .finalize_settled_physical_effect(&mut self.effect, plan, success);
        if matches!(result, Err(VfsError::ResourceBusy)) {
            // The lower effect remains Completed/SettledFailure until the
            // filesystem can revalidate it. Keep every owner, including the
            // issued request token in io_uring, live for a bounded retry.
            // The range lease excludes cache-side mutation, but it cannot
            // replace this extent rewalk: mapping_seq is local to an
            // InodeRef, and hard-link aliases may carry distinct shared
            // leases. Keep this typed retry until the filesystem proves the
            // mapping terminally.
            return PhysicalIoSettleOutcome::RetryFinalization;
        }
        if self.effect.state() != PhysicalIoEffectState::Finalized {
            // A settlement proof without the lower finalization transition
            // is an internal protocol failure. Keep all owners quarantined
            // instead of allowing Drop to infer that physical retirement was
            // complete.
            return PhysicalIoSettleOutcome::Retain {
                reason: PhysicalIoPendingReason::NotPublished,
            };
        }
        // The lower effect has proved exact physical retirement and the
        // filesystem finalization is now terminal. It is therefore safe to
        // release the range/pin owners even when the logical result is EIO.
        self.finalized = true;
        if success {
            // A completed physical write makes the old cache copy stale even
            // when mapping revalidation fails. Never restore it after this
            // point.
            if let Some(invalidation) = self.invalidation.take() {
                invalidation.commit_discard();
            }
        } else {
            // A failed device request is logically unsuccessful; its exact
            // retirement is proven, so restoring the staged cache transaction
            // is safe for reads. A failed write may have reached the medium
            // before reporting an error, so retaining the old cache would
            // expose stale data.
            self.abort_cache_transaction_after_failure();
        }
        // Cache restoration/discard must precede range release; only once
        // both are settled may a fileattr setter enter this inode again.
        drop_prepared_physical_effect_owners(&mut self.invalidation, &mut self.range_lease);
        let _ = self.native_mutation.take();
        PhysicalIoSettleOutcome::Settled { result }
    }

    pub(super) fn abort_cache_transaction_after_failure(&mut self) {
        if self.effect.plan().operation() == PhysicalIoOperation::Write {
            if let Some(invalidation) = self.invalidation.take() {
                invalidation.commit_discard();
            }
        } else {
            // Dropping an uncommitted transaction restores the exact staged
            // pages, which is the read-failure behavior.
            let _ = self.invalidation.take();
        }
    }

    /// Aborts a published effect after the lower device has provided an
    /// exact reset/quiescence proof.  This is the only reset path that may
    /// turn a published-but-unsettled effect into a releasable terminal
    /// state.  The logical result remains EIO at the io_uring layer; this
    /// method only closes the high-level ownership transition.
    pub fn abort_after_reset(&mut self, proof: PhysicalIoResetProof) {
        let _reset_kind = proof.kind;
        if self.finalized || !self.published {
            return;
        }
        self.finalized = true;
        self.abort_cache_transaction_after_failure();
        drop_prepared_physical_effect_owners(&mut self.invalidation, &mut self.range_lease);
        let _ = self.native_mutation.take();
    }

    /// Compatibility spelling for callers that used the old finalization
    /// verb.  The return type is intentionally typed so a caller cannot
    /// mistake a retained/quarantined effect for a finalized I/O error.
    pub fn finalize(&mut self, completions: &[PhysicalIoCompletion]) -> PhysicalIoSettleOutcome {
        self.settle(completions)
    }
}

#[cfg(feature = "ext4")]
pub(super) fn drop_prepared_physical_effect_owners(
    invalidation: &mut Option<CachedPageInvalidationTransaction>,
    range_lease: &mut Option<RangeCacheLease>,
) {
    // An exact direct lease drop can synchronously discard an unlinked file's
    // cache. Roll back staged pages first so that cleanup cannot run before
    // the invalidation transaction restores them.
    let _ = invalidation.take();
    let _ = range_lease.take();
}

#[cfg(feature = "ext4")]
impl Drop for PhysicalIoEffect {
    fn drop(&mut self) {
        if (self.published || self.quarantined) && !self.finalized {
            // Published-but-unretired effects are fail-stop.  In particular,
            // do not drop the range lease or let the staged invalidation
            // transaction restore cache pages while DMA may still run.  The
            // owner must be transferred to a reset/quarantine supervisor;
            // this Drop path intentionally leaks it if no supervisor exists.
            let _ = self.range_lease.take().map(core::mem::forget);
            let _ = self.invalidation.take().map(core::mem::forget);
            let _ = self.native_mutation.take().map(core::mem::forget);
            return;
        }
        // ManuallyDrop makes the fail-stop branch above explicit.  In the
        // ordinary prepared or settled path these two owners still need a
        // normal destructor call.
        drop_prepared_physical_effect_owners(&mut self.invalidation, &mut self.range_lease);
        unsafe {
            ManuallyDrop::drop(&mut self.inode);
            ManuallyDrop::drop(&mut self.location);
        }
    }
}

impl Drop for CachedFileShared {
    fn drop(&mut self) {
        let cache = self.page_cache.lock();
        let remaining = cache.len();
        let active = cache.iter().filter(|(_, page)| page.is_active()).count();
        drop(cache);
        let observed = FILE_CACHE_RESIDENT_PAGES.load(Ordering::Acquire);
        debug_assert!(
            observed >= remaining,
            "file-cache resident accounting underflow on final shared drop"
        );
        file_cache_resident_sub(remaining);
        file_cache_active_sub(active);
        // Registry identities are never reused, but retaining their shadows
        // after the final cache owner is gone only wastes the global budget.
        clear_all_file_cache_shadows(self);
        // Final Arc release makes the registered Weak impossible to upgrade.
        // The pointer check prevents a stale release from deleting a newer
        // shared state installed for the same inode-generation identity.
        remove_released_cached_file_registry_entry(self.registry_key, core::ptr::from_ref(self));
    }
}
