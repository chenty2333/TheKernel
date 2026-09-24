//! Ring-local physical I/O plans, admission, and terminal retirement.

use super::*;

/// The only operations which may consume a prepared physical admission.
/// Generic streams and pseudo-files never construct this token.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PreparedPhysicalIoOperation {
    Read,
    Write,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PhysicalIoCompletionDisposition {
    Settled,
    Retained,
    Unknown,
}

pub(super) fn physical_io_completion_result(result: axfs_ng_vfs::VfsResult<usize>) -> i32 {
    match result {
        Ok(bytes) => i32::try_from(bytes).unwrap_or(-LinuxError::EOVERFLOW.code()),
        Err(error) => -LinuxError::from(error).code(),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PhysicalIoCompletionPass {
    pub(crate) drained: usize,
    pub(crate) continuation: bool,
}

/// Owned physical plan captured while the registered buffer lease is held.
/// The lease itself remains part of [`PreparedPhysicalIoAdmission`], so these
/// descriptors cannot outlive the pin or be paired with a different buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PreparedPhysicalIoPlan {
    pub(super) operation: PreparedPhysicalIoOperation,
    pub(super) offset: u64,
    pub(super) address: usize,
    pub(super) requested_len: usize,
    pub(super) allowed_len: usize,
    pub(super) physical: [PhysicalIoSegment; IO_URING_PHYSICAL_MAX_SEGMENTS],
    pub(super) physical_len: usize,
    /// Opaque SharedBlockDevice identity selected from the mounted
    /// filesystem binding. Zero is retained for unit-test-only reservations;
    /// production admission must bind a non-zero device before publication.
    pub(super) device_identity: usize,
    pub(super) device_generation: u64,
}

impl PreparedPhysicalIoPlan {
    pub(super) const fn new(
        operation: PreparedPhysicalIoOperation,
        offset: u64,
        address: usize,
        requested_len: usize,
        allowed_len: usize,
        physical: [PhysicalIoSegment; IO_URING_PHYSICAL_MAX_SEGMENTS],
        physical_len: usize,
    ) -> Self {
        Self {
            operation,
            offset,
            address,
            requested_len,
            allowed_len,
            physical,
            physical_len,
            device_identity: 0,
            device_generation: 0,
        }
    }

    pub(crate) fn bind_device(&mut self, identity: usize, generation: u64) -> AxResult<()> {
        if identity == 0 || self.device_identity != 0 {
            return Err(AxError::BadState);
        }
        self.device_identity = identity;
        self.device_generation = generation;
        Ok(())
    }

    pub(crate) fn physical_segments(&self) -> AxResult<&[PhysicalIoSegment]> {
        if self.physical_len == 0 || self.physical_len > self.physical.len() {
            return Err(AxError::BadState);
        }
        Ok(&self.physical[..self.physical_len])
    }

    pub(crate) const fn operation(&self) -> PreparedPhysicalIoOperation {
        self.operation
    }

    pub(crate) const fn offset(&self) -> u64 {
        self.offset
    }

    pub(crate) const fn address(&self) -> usize {
        self.address
    }

    pub(crate) const fn requested_len(&self) -> usize {
        self.requested_len
    }

    pub(crate) const fn allowed_len(&self) -> usize {
        self.allowed_len
    }

    pub(crate) const fn device_identity(&self) -> usize {
        self.device_identity
    }

    pub(crate) const fn device_generation(&self) -> u64 {
        self.device_generation
    }
}

/// Checks that two ordered physical SG lists describe the same byte stream.
///
/// The registered-buffer side and the lower filesystem plan may split a
/// physical range at different boundaries (for example, an extent boundary
/// may split one registered-buffer segment).  Compare the ranges with two
/// cursors instead of requiring descriptor-count or descriptor-boundary
/// equality.  Every descriptor is still checked for a non-zero length and a
/// representable physical end, and each compared chunk must begin at the
/// same physical address.  Thus a gap, overlap, reorder, truncation, or
/// overflow cannot be hidden by re-segmentation.
pub(super) fn physical_byte_streams_equivalent<Upper, Lower, UpperFields, LowerFields>(
    upper: &[Upper],
    lower: &[Lower],
    mut upper_fields: UpperFields,
    mut lower_fields: LowerFields,
) -> AxResult<()>
where
    UpperFields: FnMut(&Upper) -> (usize, usize),
    LowerFields: FnMut(&Lower) -> (usize, usize),
{
    fn checked_stream_len<Segment, Fields>(
        segments: &[Segment],
        fields: &mut Fields,
    ) -> AxResult<usize>
    where
        Fields: FnMut(&Segment) -> (usize, usize),
    {
        if segments.is_empty() {
            return Err(AxError::BadState);
        }
        segments.iter().try_fold(0usize, |total, segment| {
            let (paddr, len) = fields(segment);
            if len == 0 {
                return Err(AxError::BadState);
            }
            paddr.checked_add(len).ok_or(AxError::BadState)?;
            total.checked_add(len).ok_or(AxError::BadState)
        })
    }

    let upper_len = checked_stream_len(upper, &mut upper_fields)?;
    let lower_len = checked_stream_len(lower, &mut lower_fields)?;
    if upper_len != lower_len {
        return Err(AxError::BadState);
    }

    let mut upper_index = 0usize;
    let mut lower_index = 0usize;
    let mut upper_offset = 0usize;
    let mut lower_offset = 0usize;
    while upper_index < upper.len() && lower_index < lower.len() {
        let (upper_segment_paddr, upper_segment_len) = upper_fields(&upper[upper_index]);
        let (lower_segment_paddr, lower_segment_len) = lower_fields(&lower[lower_index]);
        let upper_paddr = upper_segment_paddr
            .checked_add(upper_offset)
            .ok_or(AxError::BadState)?;
        let lower_paddr = lower_segment_paddr
            .checked_add(lower_offset)
            .ok_or(AxError::BadState)?;
        if upper_paddr != lower_paddr {
            return Err(AxError::BadState);
        }

        let upper_remaining = upper_segment_len
            .checked_sub(upper_offset)
            .ok_or(AxError::BadState)?;
        let lower_remaining = lower_segment_len
            .checked_sub(lower_offset)
            .ok_or(AxError::BadState)?;
        let matched = upper_remaining.min(lower_remaining);
        if matched == 0 {
            return Err(AxError::BadState);
        }
        upper_offset = upper_offset.checked_add(matched).ok_or(AxError::BadState)?;
        lower_offset = lower_offset.checked_add(matched).ok_or(AxError::BadState)?;
        if upper_offset == upper_segment_len {
            upper_index = upper_index.checked_add(1).ok_or(AxError::BadState)?;
            upper_offset = 0;
        }
        if lower_offset == lower_segment_len {
            lower_index = lower_index.checked_add(1).ok_or(AxError::BadState)?;
            lower_offset = 0;
        }
    }

    if upper_index == upper.len()
        && lower_index == lower.len()
        && upper_offset == 0
        && lower_offset == 0
    {
        Ok(())
    } else {
        Err(AxError::BadState)
    }
}

/// A worker-safe physical operation after submitter-side policy admission.
///
/// Construction retains both the exact file lease and the exact registered
/// buffer lease.  The worker API accepts this token only; it cannot receive a
/// raw borrowed SG tuple, a numeric fd, or a task-derived credential view.
pub(crate) struct PreparedPhysicalIoAdmission {
    pub(super) file: Option<IoUringFileLease>,
    pub(super) buffer: Option<IoUringBufferLease>,
    pub(super) context: Option<IoOperationContext>,
    pub(super) plan: PreparedPhysicalIoPlan,
    /// Write-side policy reservations stay live through physical retirement.
    /// In particular, a memfd seal or set-id cleanup cannot race the device
    /// while the effect still owns the destination range.
    pub(super) memfd: Option<MemfdMutationGuard>,
    pub(super) privilege: Option<ContentWritePrivilegeGuard>,
    pub(super) mutation: Option<MutationAdmission>,
    /// Bound before lower publication so a defensive Drop can transfer the
    /// exact published owner into the ring's typed custody table.
    pub(super) worker_slot: Option<usize>,
    /// The vendor-owned effect contains the inode, range-cache lease, and
    /// staged cache transaction.  It must travel with the exact file/buffer
    /// leases until physical retirement; a worker must never reconstruct it
    /// from an fd, offset, or raw SG tuple.
    pub(super) effect: Option<PreparedPhysicalIoEffect>,
}

impl PreparedPhysicalIoAdmission {
    pub(crate) fn new(
        file: IoUringFileLease,
        buffer: IoUringBufferLease,
        context: IoOperationContext,
        plan: PreparedPhysicalIoPlan,
        memfd: Option<MemfdMutationGuard>,
        privilege: Option<ContentWritePrivilegeGuard>,
        mutation: Option<MutationAdmission>,
        effect: PreparedPhysicalIoEffect,
    ) -> AxResult<Self> {
        if plan.device_identity == 0 {
            return Err(AxError::BadState);
        }
        if plan.allowed_len == 0 || plan.allowed_len > plan.requested_len {
            return Err(AxError::BadState);
        }
        if plan.allowed_len != plan.requested_len {
            return Err(AxError::BadState);
        }
        let (address, length) = buffer.range()?;
        let address = usize::try_from(address).map_err(|_| AxError::BadAddress)?;
        let length = usize::try_from(length).map_err(|_| AxError::BadAddress)?;
        if address != plan.address || length != plan.requested_len {
            return Err(AxError::BadAddress);
        }
        let description = file.description()?;
        context.validate_for(description)?;
        let physical = plan.physical_segments()?;
        buffer.physical_segments_for_plan(physical, plan.allowed_len)?;
        let effect_plan = effect.plan();
        let effect_operation = match effect_plan.operation() {
            PhysicalIoOperation::Read => PreparedPhysicalIoOperation::Read,
            PhysicalIoOperation::Write => PreparedPhysicalIoOperation::Write,
        };
        let effect_offset_matches = effect_plan
            .extent(0)
            .is_some_and(|extent| extent.file_offset() == plan.offset);
        if effect_operation != plan.operation
            || !effect_offset_matches
            || effect_plan.io_bytes() != plan.allowed_len
            || physical_byte_streams_equivalent(
                physical,
                effect_plan.segments(),
                |segment| (segment.paddr, segment.len),
                |segment| (segment.paddr, segment.len),
            )
            .is_err()
        {
            return Err(AxError::BadState);
        }
        if plan.operation == PreparedPhysicalIoOperation::Write
            && (memfd.is_none() || privilege.is_none() || mutation.is_none())
        {
            return Err(AxError::BadState);
        }
        Ok(Self {
            file: Some(file),
            buffer: Some(buffer),
            context: Some(context),
            plan,
            memfd,
            privilege,
            mutation,
            worker_slot: None,
            effect: Some(effect),
        })
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        IoUringFileLease,
        IoUringBufferLease,
        IoOperationContext,
        PreparedPhysicalIoPlan,
        Option<MemfdMutationGuard>,
        Option<ContentWritePrivilegeGuard>,
        PreparedPhysicalIoEffect,
    ) {
        let mut this = self;
        (
            this.file.take().expect("admission file lease missing"),
            this.buffer.take().expect("admission buffer lease missing"),
            this.context.take().expect("admission context missing"),
            this.plan,
            this.memfd.take(),
            this.privilege.take(),
            this.effect.take().expect("admission effect missing"),
        )
    }

    pub(crate) fn operation(&self) -> PreparedPhysicalIoOperation {
        self.plan.operation
    }

    pub(crate) fn plan(&self) -> PreparedPhysicalIoPlan {
        self.plan
    }

    pub(crate) fn effect(&self) -> &PreparedPhysicalIoEffect {
        self.effect.as_ref().expect("admission effect missing")
    }

    pub(crate) fn effect_mut(&mut self) -> &mut PreparedPhysicalIoEffect {
        self.effect.as_mut().expect("admission effect missing")
    }

    /// Publishes the already prepared vendor effect. This is intentionally
    /// the only unsafe operation exposed by the admission token; callers must
    /// reserve a worker slot immediately before invoking it and must not
    /// fallback after a Published/Terminal outcome. io_uring uses the lower
    /// kernel-destination route so the device-global broker can drain this
    /// effect without stealing synchronous exact-route waiters.
    pub(crate) unsafe fn publish(&mut self) -> AxResult<PhysicalIoPublishOutcome> {
        // SAFETY: forwarded: the caller upholds `publish`'s contract, which this function's doc
        // states.
        unsafe { self.effect_mut().publish_kernel() }
    }

    pub(crate) fn into_effect(self) -> PreparedPhysicalIoEffect {
        let mut this = self;
        this.effect.take().expect("admission effect missing")
    }

    pub(super) fn is_published_unretired(&self) -> bool {
        let Some(effect) = self.effect.as_ref() else {
            // `into_parts`/`into_effect` take ownership of the effect before
            // this value's destructor runs.  An empty option is therefore a
            // moved-out, drop-safe admission rather than a quarantined one.
            return false;
        };
        // `publish()` may return an error after the lower hook has observed
        // an unknown publication; the wrapper marks that condition sticky
        // even if its inner state is still `Prepared`.  Conversely,
        // `is_published()` remains true after the typed settlement proof
        // transitions the effect to `Finalized`.  Ordinary destructors are
        // therefore allowed only for a genuinely never-published Prepared
        // effect or an exact Finalized effect. Every other state, including
        // a publish error/quarantine, keeps the composite owner fail-stop so
        // the registered-buffer pin cannot outlive DMA.
        match effect.state() {
            PhysicalIoEffectState::Finalized => false,
            PhysicalIoEffectState::Prepared => effect.is_published() || effect.is_quarantined(),
            _ => true,
        }
    }

    pub(crate) fn physical_extent_count(&self) -> AxResult<usize> {
        let count = self.effect().plan().extent_count();
        if count == 0 || count > IO_URING_PHYSICAL_MAX_EXTENTS {
            return Err(AxError::BadState);
        }
        Ok(count)
    }

    pub(super) fn bind_worker_slot(&mut self, slot: usize) -> AxResult<()> {
        if self.worker_slot.replace(slot).is_some() {
            return Err(AxError::BadState);
        }
        Ok(())
    }

    /// Releases the upper ring/file/buffer custody after a lower reset has
    /// proved that no device access remains.  The lower effect itself has no
    /// logical completion to finalize after reset; its fail-stop destructor
    /// retains any lower-owned cache/inode custody while the registered-buffer
    /// and policy leases in this admission are released.
    pub(super) fn retire_after_reset(mut self, proof: PhysicalIoResetProof) {
        if let Some(mut effect) = self.effect.take() {
            // The lower reset/retired result is the only evidence that the
            // device can no longer touch this effect.  Transition the
            // high-level owner first so its range lease, staged cache
            // transaction, inode, and location all release normally.
            effect.abort_after_reset(proof);
            drop(effect);
        }
    }
}

impl Drop for PreparedPhysicalIoAdmission {
    fn drop(&mut self) {
        if !self.is_published_unretired() {
            return;
        }
        // A published effect's vendor Drop deliberately fail-stops its
        // range/cache owner, but the registered-buffer and policy leases are
        // siblings here. Re-home every owner together in the ring's bounded
        // custody table so DMA can never outlive only the pin or only the
        // cache transaction. This path is defensive: normal publication
        // always moves the admission into `PhysicalIoWork` before the guard
        // is released.
        record_io_uring_physical_quarantine();
        let Some(buffer) = self.buffer.as_ref() else {
            panic!("published io_uring admission lost its buffer owner");
        };
        let Some(slot) = self.worker_slot else {
            panic!("published io_uring admission lost its worker slot");
        };
        let ring = Arc::clone(&buffer.ring);
        let admission = PreparedPhysicalIoAdmission {
            file: self.file.take(),
            buffer: self.buffer.take(),
            context: self.context.take(),
            plan: self.plan,
            memfd: self.memfd.take(),
            privilege: self.privilege.take(),
            mutation: self.mutation.take(),
            worker_slot: Some(slot),
            effect: self.effect.take(),
        };
        ring.park_physical_worker_custody(PhysicalIoWork {
            ring: Arc::clone(&ring),
            slot,
            issued: None,
            admission: Some(admission),
            pending_publication: false,
            #[cfg(test)]
            test_handle: None,
        });
    }
}

/// A published physical effect retained by the single task-context completion
/// owner.  The request token is kept by value until the owner has a typed
/// settlement proof; merely dropping this item must never synthesize a CQE.
pub(crate) struct PhysicalIoWork {
    pub(super) ring: Arc<IoUring>,
    pub(super) slot: usize,
    pub(super) issued: Option<IssuedRequest>,
    pub(super) admission: Option<PreparedPhysicalIoAdmission>,
    /// The logical slot is charged before publication. A pending owner has
    /// no lower child route yet, but retains the same request/admission lease
    /// until a bounded retry publishes it or teardown cancels it.
    pub(super) pending_publication: bool,
    #[cfg(test)]
    pub(super) test_handle: Option<u64>,
}

/// A terminal publication split that moves the complete physical owner out of
/// `PhysicalIoWork` before exposing a CQE. The payload keeps the issued proof
/// and ring alive while it first drops the DMA/cache admission, then drops the
/// now-empty work to release the physical slot, and only then publishes.
pub(super) struct PhysicalIoTerminalPayload {
    pub(super) ring: Arc<IoUring>,
    pub(super) work: Option<PhysicalIoWork>,
    pub(super) issued: Option<IssuedRequest>,
    pub(super) admission: Option<PreparedPhysicalIoAdmission>,
}

impl PhysicalIoTerminalPayload {
    pub(super) fn from_valid_work(mut work: PhysicalIoWork) -> Self {
        debug_assert!(work.issued().is_some() && work.admission().is_some());
        let ring = Arc::clone(&work.ring);
        let issued = work.take_issued();
        let admission = work.take_admission();
        Self {
            ring,
            work: Some(work),
            issued,
            admission,
        }
    }

    pub(super) fn retire(mut self) -> AxResult<(Arc<IoUring>, IssuedRequest)> {
        // This is deliberately before releasing the worker slot or publishing
        // the IssuedRequest: effect/range, file, registered-buffer, and
        // write-policy owners must all be gone before the user can observe
        // the CQE.
        drop(self.admission.take());
        // The work item is now empty, so its Drop only releases the exact
        // physical worker slot/QD charge. IssuedRequest remains in this typed
        // payload until complete_issued consumes it.
        drop(self.work.take());
        let issued = self.issued.take().ok_or(AxError::BadState)?;
        Ok((self.ring, issued))
    }
}

/// Keeps the reset terminal boundary explicit: all upper owners are retired,
/// then the physical work slot is released, and only then may the EIO CQE be
/// made visible.  The small state helper is shared by the production path and
/// the pure ordering test below so a future refactor cannot move publication
/// ahead of retirement accidentally.
pub(super) fn run_physical_reset_terminal_order(
    retire_owners: impl FnOnce(),
    release_work: impl FnOnce(),
    publish_cqe: impl FnOnce(),
) {
    retire_owners();
    release_work();
    publish_cqe();
}

impl PhysicalIoWork {
    pub(crate) fn issued(&self) -> Option<&IssuedRequest> {
        self.issued.as_ref()
    }

    pub(crate) fn admission(&self) -> Option<&PreparedPhysicalIoAdmission> {
        self.admission.as_ref()
    }

    pub(crate) fn admission_mut(&mut self) -> Option<&mut PreparedPhysicalIoAdmission> {
        self.admission.as_mut()
    }

    pub(crate) fn take_issued(&mut self) -> Option<IssuedRequest> {
        self.issued.take()
    }

    pub(crate) fn take_admission(&mut self) -> Option<PreparedPhysicalIoAdmission> {
        self.admission.take()
    }

    pub(crate) fn slot(&self) -> usize {
        self.slot
    }

    pub(super) fn device_identity(&self) -> usize {
        self.admission
            .as_ref()
            .map_or(0, |admission| admission.plan().device_identity())
    }

    pub(super) fn request_id(&self) -> Option<RequestId> {
        self.issued.as_ref().map(IssuedRequest::id)
    }

    pub(super) fn device_generation(&self) -> u64 {
        self.admission
            .as_ref()
            .map_or(0, |admission| admission.plan().device_generation())
    }

    pub(super) fn is_pending_publication(&self) -> bool {
        self.pending_publication
    }

    pub(super) fn matches_reset_identity(&self, request: RequestId, slot: usize) -> bool {
        self.slot == slot && self.request_id().is_none_or(|current| current == request)
    }

    pub(super) fn matches_reset_identity_with_generation(
        &self,
        request: RequestId,
        slot: usize,
        generation: u64,
    ) -> bool {
        self.matches_reset_identity(request, slot) && self.device_generation() == generation
    }

    pub(crate) fn publication(&self) -> Option<PhysicalIoPublication> {
        self.admission.as_ref()?.effect().publication()
    }

    pub(super) fn owns_handle(&self, handle: u64) -> bool {
        #[cfg(test)]
        if self.test_handle == Some(handle) {
            return true;
        }
        let Some(publication) = self.publication() else {
            return false;
        };
        (0..publication.count()).any(|index| publication.handle(index) == Some(handle))
    }

    pub(super) fn needs_finalization_retry(&self) -> bool {
        self.admission.as_ref().is_some_and(|admission| {
            matches!(
                admission.effect().state(),
                PhysicalIoEffectState::Completed | PhysicalIoEffectState::SettledFailure
            )
        })
    }
}

impl Drop for PhysicalIoWork {
    fn drop(&mut self) {
        if self
            .admission
            .as_ref()
            .is_some_and(PreparedPhysicalIoAdmission::is_published_unretired)
        {
            // Keep all DMA owners, including the registered-buffer pin and
            // issued proof, in fail-stop custody. Re-home the complete work
            // owner instead of dropping/leaking individual fields; releasing
            // the fixed slot here would let final close proceed while the
            // device still owns the physical ranges.
            record_io_uring_physical_quarantine();
            let work = PhysicalIoWork {
                ring: Arc::clone(&self.ring),
                slot: self.slot,
                issued: self.issued.take(),
                admission: self.admission.take(),
                pending_publication: self.pending_publication,
                #[cfg(test)]
                test_handle: None,
            };
            self.ring.park_physical_worker_custody(work);
            return;
        }
        if self.pending_publication
            && let (Some(request), Some(admission)) = (self.request_id(), self.admission.as_ref())
        {
            clear_physical_completion_pending_owner(
                &self.ring,
                request,
                self.slot,
                admission.plan().device_identity(),
                admission.plan().device_generation(),
            );
        }
        // A work item is only dropped after the completion owner has consumed
        // the exact device settlement (or moved the item into quarantine).
        // Clearing the bounded slot here makes the lease/drop ordering
        // explicit and wakes a parked final close without a polling retry.
        self.ring.release_physical_worker_slot(self.slot);
    }
}

/// Reversible reservation for one fixed-capacity physical worker slot.  The
/// reservation must be acquired before descriptor publication; commit only
/// installs already-owned effect/request tokens and cannot allocate.
pub(crate) struct PhysicalIoWorkerReservation<'a> {
    pub(super) ring: &'a IoUring,
    pub(super) owner: Arc<IoUring>,
    pub(super) slot: usize,
    pub(super) device_identity: usize,
    pub(super) routes: Option<PhysicalCompletionRouteReservation>,
    pub(super) admission_gate: Option<PhysicalCompletionAdmissionGuard>,
    /// Initial reservations own the QD charge and release it if dropped.
    /// A retry claims an already charged pending slot and must leave that
    /// charge with the pending work when the lower queue remains full.
    pub(super) owns_slot_charge: bool,
    pub(super) pending_claim: Option<(RequestId, u64)>,
    pub(super) committed: bool,
}

impl PhysicalIoWorkerReservation<'_> {
    pub(crate) fn bind_admission(
        &self,
        admission: &mut PreparedPhysicalIoAdmission,
    ) -> AxResult<()> {
        let plan = admission.plan();
        if plan.device_identity() != self.device_identity
            || physical_completion_generation_for(self.device_identity)
                != Some(plan.device_generation())
        {
            return Err(AxError::BadState);
        }
        if admission
            .worker_slot
            .is_some_and(|bound| bound != self.slot)
        {
            return Err(AxError::BadState);
        }
        if admission.worker_slot.is_none() {
            admission.bind_worker_slot(self.slot)?;
        }
        Ok(())
    }

    pub(crate) fn reserve_completion_routes(&mut self, extent_count: usize) -> AxResult<()> {
        if self.routes.is_some() {
            return Err(AxError::BadState);
        }
        self.routes = Some(PhysicalCompletionRouteReservation::new_for_device(
            extent_count,
            self.device_identity,
        )?);
        Ok(())
    }

    /// Installs a pre-publication owner in the same fixed logical slot used
    /// by published work.  A route reservation, if any, is deliberately
    /// dropped first: PendingPublication owns no lower child route and must
    /// not consume a handle/cookie route while waiting for device credit.
    #[allow(clippy::result_large_err)]
    pub(crate) fn commit_pending(
        mut self,
        issued: IssuedRequest,
        mut admission: PreparedPhysicalIoAdmission,
    ) -> Result<(), (AxError, IssuedRequest, PreparedPhysicalIoAdmission)> {
        let request = issued.id();
        let generation = admission.plan().device_generation();
        let device_identity = admission.plan().device_identity();
        if admission.worker_slot.is_none() {
            admission.worker_slot = Some(self.slot);
        }
        drop(self.routes.take());

        let pending_claim = self.pending_claim.take();
        let mut state = self.ring.state.lock();
        let mut router = PHYSICAL_COMPLETION_ROUTER.lock();
        let slot_reserved = state
            .physical_slot_reserved
            .get(self.slot)
            .copied()
            .unwrap_or(false);
        let slot_available = state
            .physical_work
            .get(self.slot)
            .is_some_and(Option::is_none);
        if (!slot_reserved || !slot_available)
            && let Some((claimed_request, claimed_generation)) = pending_claim
            && let Some(owner) = router.pending.iter_mut().flatten().find(|owner| {
                owner.device_identity == device_identity
                    && owner.generation == claimed_generation
                    && owner.request == claimed_request
                    && owner.slot == self.slot
                    && Arc::ptr_eq(&owner.ring, &self.owner)
            })
        {
            owner.claimed = false;
        }
        if !slot_reserved || !slot_available {
            drop(router);
            drop(state);
            return Err((AxError::BadState, issued, admission));
        }

        if let Some((claimed_request, claimed_generation)) = pending_claim {
            let valid = router.pending.iter_mut().flatten().any(|owner| {
                let valid = owner.device_identity == device_identity
                    && owner.generation == claimed_generation
                    && owner.request == claimed_request
                    && owner.request == request
                    && owner.slot == self.slot
                    && Arc::ptr_eq(&owner.ring, &self.owner)
                    && owner.claimed;
                if valid {
                    owner.claimed = false;
                }
                valid
            });
            if !valid {
                if let Some(owner) = router.pending.iter_mut().flatten().find(|owner| {
                    owner.device_identity == device_identity
                        && owner.generation == claimed_generation
                        && owner.request == claimed_request
                        && owner.slot == self.slot
                        && Arc::ptr_eq(&owner.ring, &self.owner)
                }) {
                    owner.claimed = false;
                }
                drop(router);
                drop(state);
                return Err((AxError::BadState, issued, admission));
            }
            if claimed_generation != generation {
                if let Some(owner) = router.pending.iter_mut().flatten().find(|owner| {
                    owner.device_identity == device_identity
                        && owner.generation == claimed_generation
                        && owner.request == claimed_request
                        && owner.slot == self.slot
                        && Arc::ptr_eq(&owner.ring, &self.owner)
                }) {
                    owner.claimed = false;
                }
                drop(router);
                drop(state);
                return Err((AxError::BadState, issued, admission));
            }
        } else {
            if !physical_completion_device_pending_capacity_available(&router, device_identity) {
                drop(router);
                drop(state);
                return Err((AxError::ResourceBusy, issued, admission));
            }
            let Some(entry) = router.pending.iter_mut().find(|entry| entry.is_none()) else {
                drop(router);
                drop(state);
                return Err((AxError::ResourceBusy, issued, admission));
            };
            *entry = Some(PhysicalCompletionPendingOwner {
                device_identity,
                generation,
                ring: Arc::clone(&self.owner),
                request,
                slot: self.slot,
                claimed: false,
            });
            router.pending_count = router.pending_count.saturating_add(1);
        }

        *state
            .physical_work
            .get_mut(self.slot)
            .expect("validated slot") = Some(PhysicalIoWork {
            ring: Arc::clone(&self.owner),
            slot: self.slot,
            issued: Some(issued),
            admission: Some(admission),
            pending_publication: true,
            #[cfg(test)]
            test_handle: None,
        });
        state.physical_slot_reserved[self.slot] = false;
        self.committed = true;
        drop(router);
        drop(state);
        self.ring
            .physical_work_pending
            .store(true, Ordering::Release);
        wake_physical_completion_worker();
        Ok(())
    }

    pub(crate) fn commit(
        mut self,
        issued: IssuedRequest,
        mut admission: PreparedPhysicalIoAdmission,
    ) -> AxResult<()> {
        // The normal path always owns a route reservation made before
        // publication.  If that invariant is violated after an effect has
        // already been published, retain the effect in fail-stop custody
        // rather than letting this `Reservation` destructor release the slot
        // and making a synchronous fallback look valid.
        let request = issued.id();
        // The slot is bound before lower publication on the normal path. Keep
        // this idempotent defensive assignment for typed post-publication
        // custody if an internal caller reaches commit without that bind.
        if admission.worker_slot.is_none() {
            admission.worker_slot = Some(self.slot);
        }
        let pending_claim = self.pending_claim.take();
        let mut routes = self.routes.take();
        if routes.is_none() {
            let extent_count = admission.physical_extent_count().unwrap_or(1);
            routes = PhysicalCompletionRouteReservation::new_for_device(
                extent_count,
                self.device_identity,
            )
            .ok();
        }
        let had_routes = routes.is_some();
        let bytes = admission.plan().allowed_len();
        let admission_generation = admission.plan().device_generation();
        // A terminal publication may be a valid accepted prefix.  Its exact
        // handles still have to drain before the typed effect can settle the
        // terminal logical failure; only an absent/invalid handle is routed
        // into custody by `activate_locked`.  Do not use the sticky vendor
        // `is_quarantined` bit here: the vendor sets it for every terminal
        // publication, including the prefix whose handles remain observable.
        let publication = admission.effect().publication();
        let published_extent_count = publication.map_or(0, |published| published.count());
        // Route visibility and work-slot visibility form one publication
        // transaction. A completion worker may already be draining the
        // device while this submitter commits, so both owners stay locked
        // until the Work item and its routes are visible. Acquire the
        // sleeping ring mutex before the IRQ-disabling router lock: QD
        // contention may legitimately wait for the completion task to
        // release RingState, and doing that with interrupts/preemption
        // disabled is both illegal and hostile to the cache-hot completion
        // owner. The completion paths never retain RingState while acquiring
        // the router, so this order preserves the atomic publication without
        // introducing an inverse lock edge.
        let mut state = self.ring.state.lock();
        let mut router = PHYSICAL_COMPLETION_ROUTER.lock();
        let slot_reserved = state
            .physical_slot_reserved
            .get(self.slot)
            .copied()
            .unwrap_or(false);
        let Some(entry) = state.physical_work.get_mut(self.slot) else {
            if pending_claim.is_some() {
                clear_physical_completion_pending_owner_locked(
                    &mut router,
                    &self.owner,
                    request,
                    self.slot,
                    self.device_identity,
                    admission_generation,
                );
            }
            drop(state);
            if let Some(mut routes) = routes {
                // The work slot could not be installed, so even a valid
                // publication handle has no matching owner. Keep every
                // reserved extent in typed quarantine rather than exposing
                // an Owner route that would later manufacture an unknown/EIO
                // completion.
                let quarantined =
                    routes.activate_locked(&mut router, &self.owner, request, self.slot, None);
                if quarantined {
                    record_io_uring_physical_quarantine();
                }
            }
            drop(router);
            self.committed = true;
            record_io_uring_physical_quarantine();
            self.ring.park_physical_worker_custody(PhysicalIoWork {
                ring: Arc::clone(&self.owner),
                slot: self.slot,
                issued: Some(issued),
                admission: Some(admission),
                pending_publication: false,
                #[cfg(test)]
                test_handle: None,
            });
            return Err(AxError::BadState);
        };
        if entry.is_some() || !slot_reserved {
            if pending_claim.is_some() {
                clear_physical_completion_pending_owner_locked(
                    &mut router,
                    &self.owner,
                    request,
                    self.slot,
                    self.device_identity,
                    admission_generation,
                );
            }
            drop(state);
            if let Some(mut routes) = routes {
                let quarantined =
                    routes.activate_locked(&mut router, &self.owner, request, self.slot, None);
                if quarantined {
                    record_io_uring_physical_quarantine();
                }
            }
            drop(router);
            self.committed = true;
            record_io_uring_physical_quarantine();
            self.ring.park_physical_worker_custody(PhysicalIoWork {
                ring: Arc::clone(&self.owner),
                slot: self.slot,
                issued: Some(issued),
                admission: Some(admission),
                pending_publication: false,
                #[cfg(test)]
                test_handle: None,
            });
            return Err(AxError::BadState);
        }
        let work = PhysicalIoWork {
            ring: Arc::clone(&self.owner),
            slot: self.slot,
            issued: Some(issued),
            admission: Some(admission),
            pending_publication: false,
            #[cfg(test)]
            test_handle: None,
        };
        *entry = Some(work);
        state.physical_slot_reserved[self.slot] = false;
        let qd = state.physical_work_count;
        self.committed = true;
        let route_quarantined = if let Some(mut routes) = routes {
            routes.activate_locked(&mut router, &self.owner, request, self.slot, publication)
        } else {
            false
        };
        if pending_claim.is_some() {
            clear_physical_completion_pending_owner_locked(
                &mut router,
                &self.owner,
                request,
                self.slot,
                self.device_identity,
                admission_generation,
            );
        }
        drop(state);
        drop(router);
        if route_quarantined {
            record_io_uring_physical_quarantine();
        }
        if !had_routes {
            // A published effect without a route reservation can never be
            // demultiplexed.  The work item remains in its fixed slot and its
            // leases remain live until a typed reset path takes custody.
            record_io_uring_physical_quarantine();
        }
        record_io_uring_physical_submitted(bytes, qd, published_extent_count);
        self.ring
            .physical_work_pending
            .store(true, Ordering::Release);
        // Publish a durable edge only after both the ring work slot and all
        // exact lower routes are visible and their locks are released.  A
        // lower completion may have arrived before this commit and its IRQ
        // edge may already have been observed/cleared by the worker; this
        // exact identity edge forces the worker to revisit the newly
        // committed custody without aliasing a sibling device.
        let _ = mark_physical_completion_device_progress(self.device_identity);
        self.owner.enqueue_deferred();
        Ok(())
    }

    pub(crate) fn slot(&self) -> usize {
        self.slot
    }

    pub(crate) fn with_physical_publish<T>(&self, publish: impl FnOnce() -> T) -> AxResult<T> {
        if self.admission_gate.is_some() {
            PhysicalCompletionAdmissionGuard::with_publish_for(self.device_identity, publish)
                .ok_or(AxError::BadState)
        } else {
            Ok(publish())
        }
    }
}

impl Drop for PhysicalIoWorkerReservation<'_> {
    fn drop(&mut self) {
        if let Some((request, generation)) = self.pending_claim.take() {
            let _ = set_physical_completion_pending_claim(
                &self.owner,
                request,
                self.slot,
                self.device_identity,
                generation,
                false,
            );
            let mut state = self.ring.state.lock();
            if state
                .physical_work
                .get(self.slot)
                .is_some_and(Option::is_some)
                && state
                    .physical_slot_reserved
                    .get(self.slot)
                    .copied()
                    .unwrap_or(false)
            {
                state.physical_slot_reserved[self.slot] = false;
            }
        }
        if self.committed {
            return;
        }
        if self.owns_slot_charge {
            self.ring.release_reserved_physical_worker_slot(self.slot);
        }
    }
}

impl IoUring {
    /// Reserves one of the preallocated physical completion slots.  This is
    /// the only capacity check on the physical path and runs before an effect
    /// is published to the device.
    pub(crate) fn reserve_physical_worker_slot(&self) -> AxResult<PhysicalIoWorkerReservation<'_>> {
        self.reserve_physical_worker_slot_for_device(physical_completion_default_identity())
    }

    pub(crate) fn reserve_physical_worker_slot_for_device(
        &self,
        device_identity: usize,
    ) -> AxResult<PhysicalIoWorkerReservation<'_>> {
        let admission_gate = PhysicalCompletionAdmissionGuard::begin_for(device_identity)?;
        let owner = self
            .self_weak
            .get()
            .ok_or(AxError::BadState)?
            .upgrade()
            .ok_or(AxError::BadState)?;
        let mut state = self.state.lock();
        if state.physical_work_count >= IO_URING_PHYSICAL_MAX_QD {
            return Err(AxError::ResourceBusy);
        }
        let slot = state
            .physical_work
            .iter()
            .enumerate()
            .find_map(|(slot, entry)| {
                (entry.is_none() && !state.physical_slot_reserved[slot]).then_some(slot)
            })
            .ok_or(AxError::ResourceBusy)?;
        // Mark the slot before releasing the state lock.  A reservation is
        // still a live QD owner even though its work item has not been
        // published into `physical_work` yet; without this bit two submitter
        // tasks could reserve the same empty entry concurrently.
        let next_count = state
            .physical_work_count
            .checked_add(1)
            .ok_or(AxError::BadState)?;
        state.physical_slot_reserved[slot] = true;
        state.physical_work_count = next_count;
        Ok(PhysicalIoWorkerReservation {
            ring: self,
            owner,
            slot,
            device_identity,
            routes: None,
            admission_gate,
            owns_slot_charge: true,
            pending_claim: None,
            committed: false,
        })
    }

    /// Claims an already charged PendingPublication slot for one retry.  The
    /// lifecycle guard is acquired before the pending metadata is claimed, so
    /// device reset/close either waits for this retry or observes the owner as
    /// an unclaimed pending request; it can never retire a half-published
    /// owner by stale `(ring, slot)` identity alone.
    pub(super) fn reserve_pending_physical_worker_slot_for_retry(
        &self,
        device_identity: usize,
        request: RequestId,
        slot: usize,
        generation: u64,
    ) -> AxResult<PhysicalIoWorkerReservation<'_>> {
        let admission_gate = PhysicalCompletionAdmissionGuard::begin_for(device_identity)?;
        let owner = self
            .self_weak
            .get()
            .ok_or(AxError::BadState)?
            .upgrade()
            .ok_or(AxError::BadState)?;
        if physical_completion_generation_for(device_identity) != Some(generation) {
            return Err(AxError::BadState);
        }
        let mut state = self.state.lock();
        let mut router = PHYSICAL_COMPLETION_ROUTER.lock();
        let Some(pending) = router.pending.iter_mut().flatten().find(|pending| {
            pending.device_identity == device_identity
                && pending.generation == generation
                && pending.request == request
                && pending.slot == slot
                && Arc::ptr_eq(&pending.ring, &owner)
                && !pending.claimed
        }) else {
            return Err(AxError::BadState);
        };
        if !state
            .physical_work
            .get(slot)
            .and_then(Option::as_ref)
            .is_some_and(|work| {
                work.pending_publication
                    && work.request_id() == Some(request)
                    && work.device_identity() == device_identity
                    && work.device_generation() == generation
            })
            || state
                .physical_slot_reserved
                .get(slot)
                .copied()
                .unwrap_or(true)
        {
            return Err(AxError::BadState);
        }
        pending.claimed = true;
        state.physical_slot_reserved[slot] = true;
        drop(router);
        drop(state);
        Ok(PhysicalIoWorkerReservation {
            ring: self,
            owner,
            slot,
            device_identity,
            routes: None,
            admission_gate,
            owns_slot_charge: false,
            pending_claim: Some((request, generation)),
            committed: false,
        })
    }

    pub(super) fn take_pending_physical_worker_for_retry(
        &self,
        device_identity: usize,
        request: RequestId,
        slot: usize,
        generation: u64,
    ) -> Option<PhysicalIoWork> {
        let mut state = self.state.lock();
        if !state
            .physical_slot_reserved
            .get(slot)
            .copied()
            .unwrap_or(false)
            || !state
                .physical_work
                .get(slot)
                .and_then(Option::as_ref)
                .is_some_and(|work| {
                    work.is_pending_publication()
                        && work.device_identity() == device_identity
                        && work.device_generation() == generation
                        && work.request_id() == Some(request)
                })
        {
            return None;
        }
        state.physical_work.get_mut(slot).and_then(Option::take)
    }

    pub(super) fn release_reserved_physical_worker_slot(&self, slot: usize) {
        let mut state = self.state.lock();
        if state.physical_work.get(slot).is_some_and(Option::is_none)
            && state
                .physical_slot_reserved
                .get(slot)
                .copied()
                .unwrap_or(false)
        {
            state.physical_slot_reserved[slot] = false;
            state.physical_work_count = state.physical_work_count.saturating_sub(1);
            let wake_close = self.final_close_requested.load(Ordering::Acquire)
                && state.physical_work_count == 0;
            if wake_close {
                self.close_waiting_on_physical
                    .store(false, Ordering::Release);
            }
            drop(state);
            if wake_close && let Some(ring) = self.self_weak.get().and_then(Weak::upgrade) {
                ring.enqueue_deferred();
            }
        }
    }

    pub(super) fn release_physical_worker_slot(&self, slot: usize) {
        let mut state = self.state.lock();
        // `PhysicalIoWork` is always dropped after extraction.  The fence bit
        // remains set until this destructor takes the charge back, so a
        // submitter cannot reserve the empty slot between extraction and the
        // owner drop.  Reservation drops use the sibling helper above.
        let slot_is_empty = state.physical_work.get(slot).is_some_and(Option::is_none);
        let slot_fenced = state
            .physical_slot_reserved
            .get(slot)
            .copied()
            .unwrap_or(false);
        if slot_is_empty && slot_fenced {
            state.physical_slot_reserved[slot] = false;
            state.physical_work_count = state.physical_work_count.saturating_sub(1);
        }
        let pending = state.physical_work_count != 0;
        self.physical_work_pending.store(pending, Ordering::Release);
        if self.final_close_requested.load(Ordering::Acquire) && !pending {
            self.close_waiting_on_physical
                .store(false, Ordering::Release);
            drop(state);
            if let Some(ring) = self.self_weak.get().and_then(Weak::upgrade) {
                ring.enqueue_deferred();
            }
        }
    }

    /// Takes one queued item without releasing its QD charge.  The returned
    /// owner keeps that charge until it is dropped after exact settlement.
    pub(crate) fn take_physical_worker_work(&self) -> Option<PhysicalIoWork> {
        let mut state = self.state.lock();
        let slot = state
            .physical_work
            .iter()
            .enumerate()
            .find_map(|(slot, entry)| {
                (entry.is_some() && !state.physical_slot_reserved[slot]).then_some(slot)
            });
        let item = slot.and_then(|slot| self.take_physical_worker_work_at_slot(&mut state, slot));
        if state.physical_work.iter().all(Option::is_none) {
            self.physical_work_pending.store(false, Ordering::Release);
        }
        item
    }

    /// Keeps a published owner in bounded typed custody after an internal
    /// hand-off invariant fails.  The slot fence is intentionally retained;
    /// only a later typed reset/retirement path may make that slot reusable.
    pub(super) fn park_physical_worker_custody(&self, work: PhysicalIoWork) {
        let mut state = self.state.lock();
        let Some(entry) = state
            .physical_custody
            .iter_mut()
            .find(|entry| entry.is_none())
        else {
            // The custody array is twice the maximum live QD, while every
            // work owner consumes one QD charge. Exhaustion therefore means
            // the ring metadata invariant was already corrupted; aborting is
            // safer than dropping a published DMA owner.
            panic!("io_uring physical custody capacity exhausted");
        };
        *entry = Some(work);
    }

    pub(super) fn take_physical_worker_work_at_slot(
        &self,
        state: &mut RingState,
        slot: usize,
    ) -> Option<PhysicalIoWork> {
        if !state.physical_work.get(slot).is_some_and(Option::is_some)
            || state
                .physical_slot_reserved
                .get(slot)
                .copied()
                .unwrap_or(true)
        {
            return None;
        }
        // The extracted owner remains QD-charged, but the empty table entry
        // is fenced until retention/reinsert or the owner's terminal drop.
        state.physical_slot_reserved[slot] = true;
        let item = state.physical_work.get_mut(slot).and_then(Option::take);
        if item.is_some() && state.physical_work.iter().all(Option::is_none) {
            self.physical_work_pending.store(false, Ordering::Release);
        }
        item
    }

    pub(super) fn take_physical_worker_work_for_handle(
        &self,
        handle: u64,
    ) -> Option<PhysicalIoWork> {
        self.take_physical_worker_work_for_device(physical_completion_default_identity(), handle)
    }

    pub(super) fn take_physical_worker_work_for_device(
        &self,
        device_identity: usize,
        handle: u64,
    ) -> Option<PhysicalIoWork> {
        let mut state = self.state.lock();
        let slot = state.physical_work.iter().position(|entry| {
            entry.as_ref().is_some_and(|work| {
                work.device_identity() == device_identity && work.owns_handle(handle)
            })
        })?;
        self.take_physical_worker_work_at_slot(&mut state, slot)
    }

    pub(super) fn take_physical_worker_work_for_finalization(&self) -> Option<PhysicalIoWork> {
        let mut state = self.state.lock();
        let slot = state.physical_work.iter().position(|entry| {
            entry
                .as_ref()
                .is_some_and(PhysicalIoWork::needs_finalization_retry)
        })?;
        self.take_physical_worker_work_at_slot(&mut state, slot)
    }

    pub(super) fn take_physical_worker_work_for_finalization_at_slot(
        &self,
        slot: usize,
    ) -> Option<PhysicalIoWork> {
        let mut state = self.state.lock();
        if !state
            .physical_work
            .get(slot)
            .and_then(Option::as_ref)
            .is_some_and(PhysicalIoWork::needs_finalization_retry)
        {
            return None;
        }
        self.take_physical_worker_work_at_slot(&mut state, slot)
    }

    pub(super) fn has_physical_finalization_retry(&self) -> bool {
        self.state.lock().physical_work.iter().any(|entry| {
            entry
                .as_ref()
                .is_some_and(PhysicalIoWork::needs_finalization_retry)
        })
    }

    pub(super) fn has_physical_finalization_retry_at_slot(&self, slot: usize) -> bool {
        self.state
            .lock()
            .physical_work
            .get(slot)
            .and_then(Option::as_ref)
            .is_some_and(PhysicalIoWork::needs_finalization_retry)
    }

    pub(super) fn retain_physical_worker_work(&self, work: PhysicalIoWork) -> AxResult<()> {
        let slot = work.slot();
        let mut state = self.state.lock();
        let slot_available = state.physical_work.get(slot).is_some_and(Option::is_none)
            && state
                .physical_slot_reserved
                .get(slot)
                .copied()
                .unwrap_or(false);
        if state.physical_work.get(slot).is_none() {
            drop(state);
            self.park_physical_worker_custody(work);
            return Err(AxError::BadState);
        }
        if !slot_available {
            drop(state);
            self.park_physical_worker_custody(work);
            return Err(AxError::BadState);
        }
        state.physical_slot_reserved[slot] = false;
        state.physical_work[slot] = Some(work);
        self.physical_work_pending.store(true, Ordering::Release);
        Ok(())
    }

    /// Removes one exact published owner for the reset supervisor.  The
    /// owner may still be in its reusable worker slot or already parked in
    /// typed custody after a failed hand-off; both cases keep the slot fence
    /// until the owner is dropped after reset evidence.
    pub(super) fn take_physical_worker_for_reset(
        &self,
        request: RequestId,
        worker_slot: usize,
        generation: u64,
    ) -> Option<PhysicalIoWork> {
        self.take_physical_worker_for_reset_for_device(
            physical_completion_default_identity(),
            request,
            worker_slot,
            generation,
        )
    }

    pub(super) fn take_physical_worker_for_reset_for_device(
        &self,
        device_identity: usize,
        request: RequestId,
        worker_slot: usize,
        generation: u64,
    ) -> Option<PhysicalIoWork> {
        let mut state = self.state.lock();
        let work_slot = state
            .physical_work
            .iter()
            .enumerate()
            .find_map(|(slot, entry)| {
                entry
                    .as_ref()
                    .is_some_and(|work| {
                        slot == worker_slot
                            && work.device_identity() == device_identity
                            && work.matches_reset_identity_with_generation(
                                request,
                                worker_slot,
                                generation,
                            )
                    })
                    .then_some(slot)
            });
        if let Some(work_slot) = work_slot {
            return self.take_physical_worker_work_at_slot(&mut state, work_slot);
        }
        let custody = state.physical_custody.iter().position(|entry| {
            entry.as_ref().is_some_and(|work| {
                work.device_identity() == device_identity
                    && work.matches_reset_identity_with_generation(request, worker_slot, generation)
            })
        })?;
        state.physical_custody[custody].take()
    }

    pub(super) fn has_physical_worker_request(
        &self,
        request: RequestId,
        worker_slot: usize,
        generation: u64,
    ) -> bool {
        self.has_physical_worker_request_for_device(
            physical_completion_default_identity(),
            request,
            worker_slot,
            generation,
        )
    }

    pub(super) fn has_physical_worker_request_for_device(
        &self,
        device_identity: usize,
        request: RequestId,
        worker_slot: usize,
        generation: u64,
    ) -> bool {
        let state = self.state.lock();
        state.physical_work.iter().any(|entry| {
            entry.as_ref().is_some_and(|work| {
                work.device_identity() == device_identity
                    && work.matches_reset_identity_with_generation(request, worker_slot, generation)
            })
        }) || state.physical_custody.iter().any(|entry| {
            entry.as_ref().is_some_and(|work| {
                work.device_identity() == device_identity
                    && work.matches_reset_identity_with_generation(request, worker_slot, generation)
            })
        })
    }

    /// Finishes one already extracted ring owner after the lower reset has
    /// proved quiescence.  The reset transaction extracts every owner and
    /// validates every route before committing; this final step is therefore
    /// deliberately infallible and cannot leave a partially released owner
    /// set behind a fallible CQE/ring operation.
    pub(super) fn finish_physical_worker_after_reset(
        &self,
        mut work: PhysicalIoWork,
        proof: PhysicalIoResetProof,
    ) {
        let admission = work.take_admission();
        let issued = work.take_issued();
        run_physical_reset_terminal_order(
            || {
                if let Some(admission) = admission {
                    // The lower reset proof makes it safe to release the
                    // effect's range/cache/inode custody, along with the
                    // sibling file, buffer and policy leases.
                    admission.retire_after_reset(proof);
                }
            },
            || {
                // Dropping the now-empty work releases the exact fixed slot
                // and QD charge before any user-visible completion exists.
                drop(work);
            },
            || {
                // The reset owner may race a close/cancel terminal claimant.
                // The request proof is consumed either way; a failed
                // publication is handled by the normal close path.
                if let Some(issued) = issued {
                    let _ = self.complete_issued(
                        issued,
                        TerminalCause::Completed,
                        -LinuxError::EIO.code(),
                        0,
                    );
                }
            },
        );
    }

    /// Retires one exact ring owner after the lower reset has proved
    /// quiescence.  Kept as a small typed helper for callers that already own
    /// the reset identity; the global reset path uses the batch transaction
    /// above so route/work release cannot partially commit.
    pub(super) fn retire_physical_worker_after_reset(
        &self,
        request: RequestId,
        worker_slot: usize,
        generation: u64,
        proof: PhysicalIoResetProof,
    ) -> AxResult<bool> {
        let Some(work) = self.take_physical_worker_for_reset(request, worker_slot, generation)
        else {
            return Ok(false);
        };
        self.finish_physical_worker_after_reset(work, proof);
        Ok(true)
    }

    /// Release the exact retired lower route while the complete upper owner
    /// is still recoverable. On mismatch, keep its issued proof and work-slot
    /// fence in custody; no terminal publication may pass this boundary.
    pub(super) fn release_terminal_physical_routes(
        self: &Arc<Self>,
        work: PhysicalIoWork,
        route_handle: Option<u64>,
    ) -> AxResult<PhysicalIoWork> {
        let device_identity = work.device_identity();
        if let Some(request) = work.request_id() {
            if release_physical_completion_routes_for_device(
                device_identity,
                self,
                request,
                route_handle,
            ) {
                return Ok(work);
            }
            quarantine_physical_completion_routes_for_device(device_identity, self, request, None);
        }
        self.park_physical_worker_custody(work);
        Err(AxError::BadState)
    }

    pub(super) fn publish_terminal_physical_work(
        self: &Arc<Self>,
        work: PhysicalIoWork,
        result: i32,
        route_handle: Option<u64>,
    ) -> AxResult<PhysicalIoCompletionDisposition> {
        let Some(request) = work.request_id() else {
            self.park_physical_worker_custody(work);
            return Err(AxError::BadState);
        };
        let device_identity = work.device_identity();
        let Some(operation) = work.admission().map(PreparedPhysicalIoAdmission::operation) else {
            quarantine_physical_completion_routes_for_device(
                device_identity,
                self,
                request,
                route_handle,
            );
            self.park_physical_worker_custody(work);
            return Err(AxError::BadState);
        };
        if work.issued().is_none() {
            quarantine_physical_completion_routes_for_device(
                device_identity,
                self,
                request,
                route_handle,
            );
            self.park_physical_worker_custody(work);
            return Err(AxError::BadState);
        }
        // Settlement has already proved lower DMA retirement. Release its
        // exact route before consuming the upper payload so a route mismatch
        // retains the whole owner in custody. The worker slot stays fenced
        // until payload retirement; all owners and charges are gone before CQE.
        let work = self.release_terminal_physical_routes(work, route_handle)?;
        let payload = PhysicalIoTerminalPayload::from_valid_work(work);
        let (completion_ring, issued) = payload.retire()?;
        let completed_bytes = result.max(0) as usize;
        record_io_uring_physical_completed(completed_bytes);
        match operation {
            PreparedPhysicalIoOperation::Read => {
                record_io_uring_dma_direct_read_hit(completed_bytes)
            }
            PreparedPhysicalIoOperation::Write => {
                record_io_uring_dma_direct_write_hit(completed_bytes)
            }
        }
        completion_ring.complete_issued(issued, TerminalCause::Completed, result, 0)?;
        Ok(PhysicalIoCompletionDisposition::Settled)
    }

    /// Retries only a previously settled filesystem finalization. A bounded
    /// caller invokes this from the existing task-context physical worker;
    /// no lower completion is replayed and no physical request is reissued.
    pub(super) fn retry_physical_finalization(
        self: &Arc<Self>,
    ) -> AxResult<Option<PhysicalIoCompletionDisposition>> {
        let Some(work) = self.take_physical_worker_work_for_finalization() else {
            return Ok(None);
        };
        let device_identity = work.device_identity();
        let result = self.retry_physical_finalization_work(work);
        if result.is_err() {
            mark_physical_completion_device_reset_pending(device_identity);
        }
        result
    }

    pub(super) fn retry_physical_finalization_at_slot(
        self: &Arc<Self>,
        slot: usize,
    ) -> AxResult<Option<PhysicalIoCompletionDisposition>> {
        let Some(work) = self.take_physical_worker_work_for_finalization_at_slot(slot) else {
            return Ok(None);
        };
        let device_identity = work.device_identity();
        let result = self.retry_physical_finalization_work(work);
        if result.is_err() {
            // Finalization failure belongs to this work's exact lower queue;
            // do not fence a sibling device merely because both share the
            // one task-context completion owner.
            mark_physical_completion_device_reset_pending(device_identity);
        }
        result
    }

    pub(super) fn retry_physical_finalization_work(
        self: &Arc<Self>,
        mut work: PhysicalIoWork,
    ) -> AxResult<Option<PhysicalIoCompletionDisposition>> {
        let Some(request) = work.request_id() else {
            self.park_physical_worker_custody(work);
            return Err(AxError::BadState);
        };
        let device_identity = work.device_identity();
        let Some(admission) = work.admission_mut() else {
            quarantine_physical_completion_routes_for_device(device_identity, self, request, None);
            self.park_physical_worker_custody(work);
            return Err(AxError::BadState);
        };
        let outcome = admission.effect_mut().retry_finalization();
        match outcome {
            PhysicalIoSettleOutcome::RetryFinalization => {
                self.retain_physical_worker_work(work)?;
                Ok(Some(PhysicalIoCompletionDisposition::Retained))
            }
            PhysicalIoSettleOutcome::Settled { result } => {
                let result = physical_io_completion_result(result);
                self.publish_terminal_physical_work(work, result, None)
                    .map(Some)
            }
            PhysicalIoSettleOutcome::Retain { .. } => {
                quarantine_physical_completion_routes_for_device(
                    device_identity,
                    self,
                    request,
                    None,
                );
                self.park_physical_worker_custody(work);
                Err(AxError::BadState)
            }
        }
    }

    /// Applies one exact task-context device completion.  The block wait
    /// owner supplies these records after `wait_any_physical_completion`; IRQ
    /// code never calls this method.  A retained result puts the work owner
    /// back into its fixed slot, while a settled result is the only path that
    /// consumes the issued token and publishes a CQE.
    pub(super) fn consume_physical_completion(
        self: &Arc<Self>,
        completion: PhysicalIoCompletion,
    ) -> AxResult<PhysicalIoCompletionDisposition> {
        self.consume_physical_completion_for_device(
            physical_completion_default_identity(),
            completion,
        )
    }

    pub(super) fn consume_physical_completion_for_device(
        self: &Arc<Self>,
        device_identity: usize,
        completion: PhysicalIoCompletion,
    ) -> AxResult<PhysicalIoCompletionDisposition> {
        let slot = {
            let state = self.state.lock();
            state.physical_work.iter().position(|entry| {
                entry.as_ref().is_some_and(|work| {
                    work.device_identity() == device_identity && work.owns_handle(completion.handle)
                })
            })
        };
        let Some(slot) = slot else {
            // A duplicate or a malformed driver ordering can expose a route
            // whose Work owner is already being consumed. Preserve the
            // observation in non-replayable custody; never turn it into a
            // synthetic EIO or silently discard the device record.
            quarantine_physical_completion_for_device(device_identity, completion, false)?;
            return Ok(PhysicalIoCompletionDisposition::Unknown);
        };
        self.consume_physical_completion_for_device_at_slot(device_identity, slot, completion)
    }

    pub(super) fn consume_physical_completion_for_device_at_slot(
        self: &Arc<Self>,
        device_identity: usize,
        slot: usize,
        completion: PhysicalIoCompletion,
    ) -> AxResult<PhysicalIoCompletionDisposition> {
        let work = {
            let mut state = self.state.lock();
            let matches = state
                .physical_work
                .get(slot)
                .and_then(Option::as_ref)
                .is_some_and(|work| {
                    work.device_identity() == device_identity && work.owns_handle(completion.handle)
                });
            matches.then(|| self.take_physical_worker_work_at_slot(&mut state, slot))
        }
        .flatten();
        let Some(mut work) = work else {
            // The route lookup supplied an exact fixed slot, but a competing
            // completion/finalizer may already own it. Revalidate the handle
            // under RingState and retain this observation instead of scanning
            // the other 31 slots or aliasing a recycled owner.
            quarantine_physical_completion_for_device(device_identity, completion, false)?;
            return Ok(PhysicalIoCompletionDisposition::Unknown);
        };
        let Some(request) = work.request_id() else {
            self.park_physical_worker_custody(work);
            return Err(AxError::BadState);
        };
        let Some(admission) = work.admission_mut() else {
            quarantine_physical_completion_routes_for_device(
                device_identity,
                self,
                request,
                Some(completion.handle),
            );
            self.park_physical_worker_custody(work);
            return Err(AxError::BadState);
        };
        let settlement = admission
            .effect_mut()
            .settle(core::slice::from_ref(&completion));
        // `settle` consumes exactly one lower completion.  A normal partial
        // batch reports `MissingCompletion` until the remaining children
        // arrive; the final child can report either `Settled` or a bounded
        // finalization retry.  Count only those outcomes, never protocol
        // failures that merely retain the owner in quarantine custody.
        if matches!(
            &settlement,
            PhysicalIoSettleOutcome::Retain {
                reason: PhysicalIoPendingReason::MissingCompletion { .. }
            } | PhysicalIoSettleOutcome::Settled { .. }
                | PhysicalIoSettleOutcome::RetryFinalization
        ) {
            record_io_uring_physical_child_completed();
        }
        match settlement {
            PhysicalIoSettleOutcome::Retain { reason } => {
                // A multi-extent effect normally returns Retain while it is
                // waiting for the remaining exact handles. The completion
                // has already been consumed into the vendor effect and must
                // not fill the bounded quarantine slab. Only protocol
                // failures retain the observation as diagnostic custody.
                let quarantine = retained_completion_needs_quarantine(reason);
                if quarantine {
                    quarantine_physical_completion_routes_for_device(
                        device_identity,
                        self,
                        request,
                        Some(completion.handle),
                    );
                }
                self.retain_physical_worker_work(work)?;
                if quarantine {
                    quarantine_physical_completion_for_device(device_identity, completion, false)?;
                }
                Ok(PhysicalIoCompletionDisposition::Retained)
            }
            PhysicalIoSettleOutcome::Settled { result } => {
                let result = physical_io_completion_result(result);
                self.publish_terminal_physical_work(work, result, Some(completion.handle))
            }
            PhysicalIoSettleOutcome::RetryFinalization => {
                // Every device handle has already retired. Keep the exact
                // work/issued/effect owner and retry only filesystem
                // finalization from the task-context continuation.
                self.retain_physical_worker_work(work)?;
                Ok(PhysicalIoCompletionDisposition::Retained)
            }
        }
    }

    pub(crate) fn physical_worker_len(&self) -> usize {
        self.state.lock().physical_work_count
    }

    pub(crate) fn close_waiting_on_physical(&self) -> bool {
        self.close_waiting_on_physical.load(Ordering::Acquire)
    }
}
