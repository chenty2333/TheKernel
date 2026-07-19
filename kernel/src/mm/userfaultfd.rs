//! Address-space adapter for the bounded generic fault broker.
//!
//! Linux-visible validation remains in `thekernel-linux-mm`; queue identity,
//! coalescing, and waiter ownership remain in `thekernel-axfault`.  This
//! dormant adapter now owns bounded REGISTER/UNREGISTER transactions and the
//! per-address-space handler registry. It still does not route page faults or
//! expose COPY/ZEROPAGE/WAKE resolution.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};

use axerrno::{AxError, AxResult};
use axfault::{CompletionVisibility, FaultBroker};
use axpoll::PollSet;
use thekernel_linux_mm::{
    AddressSpaceId, FaultDisposition, FaultHandlerId, FaultRequest, MappingGeneration, MappingId,
    MappingSnapshot, MmError, PageRange, UffdApiState, UffdIoctls, UffdRegisterMode,
    UffdRegistration, UffdRegistrationId, UffdRegistrationIntent, UffdRegistrationPlan,
    UffdRegistrationReplacement, UffdRegistrationRequest, UffdRegistrationTable,
};

use super::AddrSpace;

pub(crate) const UFFD_MAX_HANDLERS: usize = 16;
pub(crate) const UFFD_MAX_REGISTRATIONS: usize = 64;
pub(crate) const UFFD_MAX_REQUESTS: usize = 64;
pub(crate) const UFFD_MAX_WAITERS: usize = 128;
pub(crate) const UFFD_POLL_CAPACITY: usize = 256;
const UFFD_MAX_TXN_FRAGMENTS: usize = UFFD_MAX_REGISTRATIONS;
const UFFD_MAX_PROTECT_CANDIDATES: usize = UFFD_MAX_TXN_FRAGMENTS * 3;
const UFFD_PLAN_SLOTS: usize = 2;

pub(crate) type UffdPollSet = PollSet<UFFD_POLL_CAPACITY>;
type UffdBroker = FaultBroker<FaultRequest, FaultHandlerId, FaultDisposition>;
type UffdRegistrations = UffdRegistrationTable<UFFD_MAX_REGISTRATIONS>;

static NEXT_UFFD_HANDLER_ID: AtomicU64 = AtomicU64::new(1);

fn allocate_handler_id() -> AxResult<FaultHandlerId> {
    let raw = NEXT_UFFD_HANDLER_ID
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(1)
        })
        .map_err(|_| AxError::NoMemory)?;
    FaultHandlerId::new(raw).map_err(uffd_policy_error)
}

pub(crate) fn uffd_policy_error(error: MmError) -> AxError {
    match error {
        MmError::CapacityExceeded | MmError::IdExhausted => AxError::NoMemory,
        MmError::Busy | MmError::OwnerBusy | MmError::UffdRegistrationOverlap => {
            AxError::ResourceBusy
        }
        MmError::AccessDenied => AxError::OperationNotPermitted,
        MmError::RangeNotMapped | MmError::StaleGeneration => AxError::BadAddress,
        MmError::Closing | MmError::TearingDown | MmError::Closed => AxError::BadState,
        _ => AxError::InvalidInput,
    }
}

fn broker_config_error(error: axfault::BrokerConfigError) -> AxError {
    match error {
        axfault::BrokerConfigError::NoMemory => AxError::NoMemory,
        axfault::BrokerConfigError::UnboundedCapacity => AxError::InvalidInput,
        axfault::BrokerConfigError::BrokerIdentityExhausted => AxError::NoMemory,
        _ => AxError::InvalidInput,
    }
}

struct UffdHandlerState {
    id: FaultHandlerId,
    readiness: Arc<UffdPollSet>,
}

#[derive(Clone, Copy)]
struct UffdWakeSpan {
    handler: FaultHandlerId,
    address_space: AddressSpaceId,
    mapping: MappingId,
    generation: MappingGeneration,
    range: PageRange,
}

impl UffdWakeSpan {
    fn matches(self, handler: FaultHandlerId, request: FaultRequest) -> bool {
        let key = request.key();
        handler == self.handler
            && request.handler() == self.handler
            && key.address_space() == self.address_space
            && key.mapping() == self.mapping
            && key.generation() == self.generation
            && self
                .range
                .user_range()
                .contains_address(key.page_address().get())
    }
}

struct UffdTxnScratch {
    snapshots: Vec<MappingSnapshot>,
    removed: Vec<UffdRegistrationId>,
    register_replacements: Vec<UffdRegistrationRequest>,
    mapping_replacements: Vec<UffdRegistrationReplacement>,
    wake_spans: Vec<UffdWakeSpan>,
}

impl UffdTxnScratch {
    fn try_new() -> AxResult<Self> {
        fn reserved<T>() -> AxResult<Vec<T>> {
            let mut values = Vec::new();
            values
                .try_reserve_exact(UFFD_MAX_TXN_FRAGMENTS)
                .map_err(|_| AxError::NoMemory)?;
            Ok(values)
        }

        Ok(Self {
            snapshots: reserved()?,
            removed: reserved()?,
            register_replacements: reserved()?,
            mapping_replacements: reserved()?,
            wake_spans: reserved()?,
        })
    }

    fn clear_transaction(&mut self) {
        self.removed.clear();
        self.register_replacements.clear();
        self.mapping_replacements.clear();
        self.wake_spans.clear();
    }
}

/// Copy-only authority for one preflighted mapping-sidecar transaction.
///
/// The token intentionally borrows neither the address space nor its UFFD
/// state. `AddrSpace::unmap` can therefore keep it across the main MM commit
/// without holding a second mutable borrow. The nonce prevents a stale token
/// from consuming a later plan that reused the same bounded slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct UffdPlanToken {
    slot: u8,
    nonce: u64,
}

/// A mapping operation with no intersecting registration owns no plan slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OptionalUffdPlan {
    Noop,
    Armed(UffdPlanToken),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UffdRemapKind {
    Duplicate,
    Move,
}

/// Copy-only sidecar authority frozen before one remap transaction mutates
/// its VMA/PTE state.
///
/// This value is deliberately move-only even though its leaf tokens are Copy.
/// The enclosing address-space transaction must resolve every alternative
/// exactly once; duplicating the set would permit a stale token to consume a
/// bounded slot twice.
#[must_use = "a prepared remap sidecar must be resolved exactly once"]
pub(crate) enum PreparedRemapUffd {
    None,
    FixedDuplicate {
        destination: OptionalUffdPlan,
    },
    FixedMove {
        on_failure: OptionalUffdPlan,
        on_success: OptionalUffdPlan,
    },
    NonfixedMove {
        on_success: OptionalUffdPlan,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RemapUffdOutcome {
    Preserved,
    DestructiveFailure,
    Committed,
}

#[derive(Clone, Copy)]
struct ArmedUffdPlan {
    nonce: u64,
    plan: UffdRegistrationPlan,
}

#[derive(Clone, Copy)]
struct UffdProtectCandidate {
    replacement: UffdRegistrationReplacement,
    post_vma: MappingSnapshot,
}

/// Storage reserved before the address-space state is installed.
///
/// The payload types are all Copy and have no external ownership. Clearing a
/// slot while the address-space lock is held therefore neither frees memory
/// nor drops wake-capable resources.
struct UffdPlanSlot {
    removed: Vec<UffdRegistrationId>,
    replacements: Vec<UffdRegistrationReplacement>,
    protect_candidates: Vec<UffdProtectCandidate>,
    armed: Option<ArmedUffdPlan>,
}

impl UffdPlanSlot {
    fn try_new() -> AxResult<Self> {
        fn reserved<T>(capacity: usize) -> AxResult<Vec<T>> {
            let mut values = Vec::new();
            values
                .try_reserve_exact(capacity)
                .map_err(|_| AxError::NoMemory)?;
            Ok(values)
        }

        Ok(Self {
            removed: reserved(UFFD_MAX_TXN_FRAGMENTS)?,
            replacements: reserved(UFFD_MAX_TXN_FRAGMENTS)?,
            protect_candidates: reserved(UFFD_MAX_PROTECT_CANDIDATES)?,
            armed: None,
        })
    }

    fn clear_payload(&mut self) {
        self.removed.clear();
        self.replacements.clear();
        self.protect_candidates.clear();
    }

    fn push_removed(&mut self, id: UffdRegistrationId) -> AxResult {
        if self.removed.len() == UFFD_MAX_TXN_FRAGMENTS {
            return Err(AxError::NoMemory);
        }
        self.removed.push(id);
        Ok(())
    }

    fn push_replacement(&mut self, replacement: UffdRegistrationReplacement) -> AxResult {
        if self.replacements.len() == UFFD_MAX_TXN_FRAGMENTS {
            return Err(AxError::NoMemory);
        }
        self.replacements.push(replacement);
        Ok(())
    }

    fn push_protect_candidate(&mut self, candidate: UffdProtectCandidate) -> AxResult {
        if self.protect_candidates.len() == UFFD_MAX_PROTECT_CANDIDATES {
            return Err(AxError::NoMemory);
        }
        self.protect_candidates.push(candidate);
        Ok(())
    }
}

/// Wake ownership collected while the address-space lock is held.
///
/// The caller must invoke [`Self::finish`] only after releasing that lock.
#[must_use = "release the address-space lock, then finish deferred UFFD wake ownership"]
pub(crate) struct DeferredUffdWake {
    fault_completion: Option<Arc<UffdPollSet>>,
    handlers: [Option<Arc<UffdPollSet>>; UFFD_MAX_HANDLERS],
}

impl DeferredUffdWake {
    fn empty() -> Self {
        Self {
            fault_completion: None,
            handlers: core::array::from_fn(|_| None),
        }
    }

    fn add_handler(&mut self, readiness: Arc<UffdPollSet>) {
        if self
            .handlers
            .iter()
            .flatten()
            .any(|current| Arc::ptr_eq(current, &readiness))
        {
            return;
        }
        let slot = self
            .handlers
            .iter_mut()
            .find(|slot| slot.is_none())
            .expect("affected UFFD handlers exceed fixed handler capacity");
        *slot = Some(readiness);
    }

    pub(crate) fn finish(self) {
        if let Some(completion) = self.fault_completion {
            completion.wake();
        }
        for readiness in self.handlers.into_iter().flatten() {
            readiness.wake();
        }
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.fault_completion.is_none() && self.handlers.iter().all(Option::is_none)
    }
}

/// Lazily allocated state for one address space.
///
/// The registration and handler arrays are fixed-size.  The broker reserves
/// its complete request and waiter storage during construction, so observing
/// readiness and claiming an event never allocates.
pub(crate) struct UffdAddressSpaceState {
    pub(crate) registrations: UffdRegistrations,
    broker: UffdBroker,
    handlers: [Option<UffdHandlerState>; UFFD_MAX_HANDLERS],
    fault_completion: Arc<UffdPollSet>,
    scratch: UffdTxnScratch,
    plan_slots: [UffdPlanSlot; UFFD_PLAN_SLOTS],
    next_plan_nonce: u64,
}

impl UffdAddressSpaceState {
    pub(crate) fn try_new_boxed() -> AxResult<Box<Self>> {
        let broker = UffdBroker::try_new(UFFD_MAX_REQUESTS, UFFD_MAX_WAITERS)
            .map_err(broker_config_error)?;
        let state = Self {
            registrations: UffdRegistrations::new(1).map_err(uffd_policy_error)?,
            broker,
            handlers: core::array::from_fn(|_| None),
            fault_completion: Arc::try_new(UffdPollSet::new()).map_err(|_| AxError::NoMemory)?,
            scratch: UffdTxnScratch::try_new()?,
            plan_slots: [UffdPlanSlot::try_new()?, UffdPlanSlot::try_new()?],
            next_plan_nonce: 1,
        };
        Box::try_new(state).map_err(|_| AxError::NoMemory)
    }

    fn reset_unarmed_plan_slot(&mut self, slot_index: usize) -> AxResult {
        let slot = self
            .plan_slots
            .get_mut(slot_index)
            .ok_or(AxError::InvalidInput)?;
        if slot.armed.is_some() {
            return Err(AxError::BadState);
        }
        slot.clear_payload();
        Ok(())
    }

    fn finish_plan_preflight(
        &mut self,
        slot_index: usize,
        plan: Option<UffdRegistrationPlan>,
    ) -> AxResult<OptionalUffdPlan> {
        let Some(plan) = plan else {
            self.plan_slots[slot_index].clear_payload();
            return Ok(OptionalUffdPlan::Noop);
        };
        let nonce = self.next_plan_nonce;
        let Some(next_nonce) = nonce.checked_add(1) else {
            self.plan_slots[slot_index].clear_payload();
            return Err(AxError::NoMemory);
        };
        self.next_plan_nonce = next_nonce;
        let slot = &mut self.plan_slots[slot_index];
        debug_assert!(slot.armed.is_none());
        slot.armed = Some(ArmedUffdPlan { nonce, plan });
        Ok(OptionalUffdPlan::Armed(UffdPlanToken {
            slot: slot_index as u8,
            nonce,
        }))
    }

    fn fail_plan_preflight<T>(&mut self, slot_index: usize, error: AxError) -> AxResult<T> {
        let slot = &mut self.plan_slots[slot_index];
        debug_assert!(slot.armed.is_none());
        slot.clear_payload();
        Err(error)
    }

    /// Preflights the canonical registration fragments produced by `mprotect`.
    ///
    /// `post_vma` projects each source fragment onto the exact VMA that the
    /// concrete MemorySet transaction will publish. It may return `None` only
    /// when the whole source registration has an exact topology no-op proof;
    /// an access-only change to one unchanged VMA is such a no-op because
    /// registration identity deliberately excludes VMA access bits. Returning
    /// a mixture of `Some` and `None` for one split source fails closed: once a
    /// source participates, every surviving fragment must remain represented.
    /// The callback must follow MemorySet's flags/lineage/backend merge law and
    /// must not allocate. Linux-MM then folds only strictly adjacent,
    /// same-owner fragments covered by that one post-state VMA while
    /// preserving the registration/fault epoch.
    pub(crate) fn preflight_protect<F>(
        &mut self,
        slot_index: usize,
        range: PageRange,
        mut post_vma: F,
    ) -> AxResult<OptionalUffdPlan>
    where
        F: FnMut(UffdRegistration, PageRange) -> AxResult<Option<MappingSnapshot>>,
    {
        self.reset_unarmed_plan_slot(slot_index)?;
        let result = (|| {
            let Self {
                registrations,
                plan_slots,
                ..
            } = self;
            let slot = &mut plan_slots[slot_index];

            for registration in registrations.iter() {
                let registered = registration.range();
                let mut cuts = [registered.start(); 4];
                let mut cut_count = 1;
                for boundary in [range.start(), range.end()] {
                    if registered.start() < boundary && boundary < registered.end() {
                        cuts[cut_count] = boundary;
                        cut_count += 1;
                    }
                }
                cuts[cut_count] = registered.end();
                cut_count += 1;

                let fragment_count = cut_count - 1;
                let mut projected_count = 0usize;
                for fragment in cuts[..cut_count].windows(2) {
                    let start = fragment[0];
                    let end = fragment[1];
                    let fragment = PageRange::with_page_size(
                        start,
                        end.checked_sub(start).ok_or(AxError::BadState)?,
                        registered.page_size(),
                    )
                    .map_err(uffd_policy_error)?;
                    let Some(snapshot) = post_vma(registration, fragment)? else {
                        continue;
                    };
                    let request = registration
                        .refreshed_fragment(snapshot, fragment)
                        .map_err(uffd_policy_error)?;
                    slot.push_protect_candidate(UffdProtectCandidate {
                        replacement: UffdRegistrationReplacement::new(registration.id(), request),
                        post_vma: snapshot,
                    })?;
                    projected_count += 1;
                }
                if projected_count != 0 && projected_count != fragment_count {
                    return Err(AxError::BadState);
                }
                if projected_count != 0 {
                    slot.push_removed(registration.id())?;
                }
            }

            if slot.protect_candidates.is_empty() {
                return Ok(None);
            }

            slot.protect_candidates.sort_unstable_by_key(|candidate| {
                (
                    candidate.post_vma.range().start(),
                    candidate.post_vma.range().end(),
                    candidate.replacement.request().range().start(),
                    candidate.replacement.request().range().end(),
                )
            });
            let mut last_post_vma = None;
            for index in 0..slot.protect_candidates.len() {
                let candidate = slot.protect_candidates[index];
                let folded = if last_post_vma == Some(candidate.post_vma) {
                    slot.replacements
                        .last()
                        .copied()
                        .map(|previous| {
                            previous
                                .canonical_union(candidate.replacement, candidate.post_vma)
                                .map_err(uffd_policy_error)
                        })
                        .transpose()?
                        .flatten()
                } else {
                    None
                };
                if let Some(folded) = folded {
                    *slot
                        .replacements
                        .last_mut()
                        .expect("canonical protect replacement disappeared") = folded;
                } else {
                    slot.push_replacement(candidate.replacement)?;
                }
                last_post_vma = Some(candidate.post_vma);
            }
            slot.protect_candidates.clear();

            // A post-VMA may change solely because MemorySet merged backends
            // that carry different UFFD owners. Such an owner remains an exact
            // no-op and must not consume a fresh registration identity on each
            // mprotect cycle. Split or union outputs necessarily differ in
            // range or source multiplicity and remain in the transaction.
            let replacements = &slot.replacements;
            slot.removed.retain(|id| {
                let source = registrations
                    .get(*id)
                    .expect("projected UFFD source disappeared during preflight");
                let mut matching = replacements
                    .iter()
                    .filter(|replacement| replacement.source() == *id);
                let Some(only) = matching.next() else {
                    return true;
                };
                matching.next().is_some() || only.request().range() != source.range()
            });
            let removed = &slot.removed;
            slot.replacements
                .retain(|replacement| removed.contains(&replacement.source()));

            if slot.removed.is_empty() {
                return Ok(None);
            }
            registrations
                .preflight_mapping_replace(&slot.removed, &slot.replacements)
                .map(Some)
                .map_err(uffd_policy_error)
        })();

        match result {
            Ok(plan) => self.finish_plan_preflight(slot_index, plan),
            Err(error) => self.fail_plan_preflight(slot_index, error),
        }
    }

    /// Preflights pure mapping retirement for one or two bounded ranges.
    ///
    /// The ranges are normalized without allocation. Intersecting records are
    /// removed and up to three survivors are refreshed in the same all-or-none
    /// table plan. This path deliberately does not inspect or complete the
    /// fault broker: ordinary mapping teardown does not acquire userfaultfd
    /// wake ownership.
    pub(crate) fn preflight_unmap_ranges<F>(
        &mut self,
        slot_index: usize,
        ranges: [Option<PageRange>; 2],
        mut current: F,
    ) -> AxResult<OptionalUffdPlan>
    where
        F: FnMut(UffdRegistration) -> AxResult<MappingSnapshot>,
    {
        self.reset_unarmed_plan_slot(slot_index)?;
        let result = (|| {
            let mut normalized = [None, None];
            let mut count = 0usize;
            for range in ranges.into_iter().flatten() {
                if count == 0 {
                    normalized[0] = Some(range);
                    count = 1;
                    continue;
                }
                let first = normalized[0].expect("first UFFD retirement range disappeared");
                if range.page_size() != first.page_size() {
                    return Err(AxError::InvalidInput);
                }
                if range.start() < first.start() {
                    normalized[1] = normalized[0];
                    normalized[0] = Some(range);
                } else {
                    normalized[1] = Some(range);
                }
                count = 2;
            }
            if count == 2 {
                let first = normalized[0].expect("first UFFD retirement range disappeared");
                let second = normalized[1].expect("second UFFD retirement range disappeared");
                if second.start() <= first.end() {
                    normalized[0] = Some(
                        PageRange::with_page_size(
                            first.start(),
                            first
                                .end()
                                .max(second.end())
                                .checked_sub(first.start())
                                .ok_or(AxError::BadState)?,
                            first.page_size(),
                        )
                        .map_err(uffd_policy_error)?,
                    );
                    normalized[1] = None;
                }
            }

            let Self {
                registrations,
                plan_slots,
                ..
            } = self;
            let slot = &mut plan_slots[slot_index];

            for registration in registrations.iter() {
                let registered = registration.range();
                let mut survivors = [(0usize, 0usize); 3];
                let mut survivor_count = 0usize;
                let mut cursor = registered.start();
                let mut intersects = false;
                for retirement in normalized.into_iter().flatten() {
                    if retirement.end() <= cursor || registered.end() <= retirement.start() {
                        continue;
                    }
                    intersects = true;
                    let survivor_end = registered.end().min(retirement.start());
                    if cursor < survivor_end {
                        survivors[survivor_count] = (cursor, survivor_end);
                        survivor_count += 1;
                    }
                    cursor = cursor.max(retirement.end().min(registered.end()));
                    if cursor == registered.end() {
                        break;
                    }
                }
                if !intersects {
                    continue;
                }
                if cursor < registered.end() {
                    survivors[survivor_count] = (cursor, registered.end());
                    survivor_count += 1;
                }
                slot.push_removed(registration.id())?;

                let snapshot = if survivor_count != 0 {
                    Some(current(registration)?)
                } else {
                    None
                };
                for (start, end) in survivors[..survivor_count].iter().copied() {
                    let survivor = PageRange::with_page_size(
                        start,
                        end.checked_sub(start).ok_or(AxError::BadState)?,
                        registered.page_size(),
                    )
                    .map_err(uffd_policy_error)?;
                    let request = registration
                        .refreshed_fragment(
                            snapshot.expect("surviving UFFD fragment has a current VMA"),
                            survivor,
                        )
                        .map_err(uffd_policy_error)?;
                    slot.push_replacement(UffdRegistrationReplacement::new(
                        registration.id(),
                        request,
                    ))?;
                }
            }

            if slot.removed.is_empty() {
                return Ok(None);
            }
            registrations
                .preflight_mapping_replace(&slot.removed, &slot.replacements)
                .map(Some)
                .map_err(uffd_policy_error)
        })();

        match result {
            Ok(plan) => self.finish_plan_preflight(slot_index, plan),
            Err(error) => self.fail_plan_preflight(slot_index, error),
        }
    }

    /// One-range compatibility wrapper used by ordinary `munmap`.
    pub(crate) fn preflight_unmap<F>(
        &mut self,
        slot_index: usize,
        range: PageRange,
        current: F,
    ) -> AxResult<OptionalUffdPlan>
    where
        F: FnMut(UffdRegistration) -> AxResult<MappingSnapshot>,
    {
        self.preflight_unmap_ranges(slot_index, [Some(range), None], current)
    }

    /// Freezes every mapping-sidecar outcome that one remap transaction may
    /// publish. Both fixed-move alternatives observe the same unchanged table
    /// revision; failure to arm the second releases the first immediately.
    pub(crate) fn preflight_remap<F>(
        &mut self,
        kind: UffdRemapKind,
        fixed: bool,
        source: PageRange,
        destination: PageRange,
        mut current: F,
    ) -> AxResult<PreparedRemapUffd>
    where
        F: FnMut(UffdRegistration) -> AxResult<MappingSnapshot>,
    {
        match (kind, fixed) {
            (UffdRemapKind::Duplicate, false) => Ok(PreparedRemapUffd::None),
            (UffdRemapKind::Duplicate, true) => {
                let destination =
                    self.preflight_unmap_ranges(0, [Some(destination), None], &mut current)?;
                Ok(PreparedRemapUffd::FixedDuplicate { destination })
            }
            (UffdRemapKind::Move, false) => {
                let on_success =
                    self.preflight_unmap_ranges(0, [Some(source), None], &mut current)?;
                Ok(PreparedRemapUffd::NonfixedMove { on_success })
            }
            (UffdRemapKind::Move, true) => {
                let on_failure =
                    self.preflight_unmap_ranges(0, [Some(destination), None], &mut current)?;
                let on_success = match self.preflight_unmap_ranges(
                    1,
                    [Some(destination), Some(source)],
                    &mut current,
                ) {
                    Ok(plan) => plan,
                    Err(error) => {
                        self.abort_plan(on_failure);
                        return Err(error);
                    }
                };
                Ok(PreparedRemapUffd::FixedMove {
                    on_failure,
                    on_success,
                })
            }
        }
    }

    fn preflight_boundary_extension(
        &mut self,
        slot_index: usize,
        address_space: AddressSpaceId,
        mapping: MappingId,
        old_boundary: usize,
        new_boundary: usize,
        head: bool,
    ) -> AxResult<OptionalUffdPlan> {
        self.reset_unarmed_plan_slot(slot_index)?;
        let result = (|| {
            let Self {
                registrations,
                plan_slots,
                ..
            } = self;
            let slot = &mut plan_slots[slot_index];
            for registration in registrations.iter() {
                if registration.address_space() != address_space
                    || registration.mapping() != mapping
                {
                    continue;
                }
                let range = registration.range();
                let reaches_boundary = if head {
                    range.start() == old_boundary
                } else {
                    range.end() == old_boundary
                };
                if !reaches_boundary {
                    continue;
                }
                if !slot.removed.is_empty() {
                    return Err(AxError::BadState);
                }
                let new_range = if head {
                    PageRange::with_page_size(
                        new_boundary,
                        range
                            .end()
                            .checked_sub(new_boundary)
                            .ok_or(AxError::InvalidInput)?,
                        range.page_size(),
                    )
                } else {
                    PageRange::with_page_size(
                        range.start(),
                        new_boundary
                            .checked_sub(range.start())
                            .ok_or(AxError::InvalidInput)?,
                        range.page_size(),
                    )
                }
                .map_err(uffd_policy_error)?;
                let replacement = if head {
                    registration.head_extension_replacement(address_space, mapping, new_range)
                } else {
                    registration.tail_extension_replacement(address_space, mapping, new_range)
                }
                .map_err(uffd_policy_error)?;
                slot.push_removed(registration.id())?;
                slot.push_replacement(replacement)?;
            }

            if slot.removed.is_empty() {
                return Ok(None);
            }
            registrations
                .preflight_mapping_replace(&slot.removed, &slot.replacements)
                .map(Some)
                .map_err(uffd_policy_error)
        })();

        match result {
            Ok(plan) => self.finish_plan_preflight(slot_index, plan),
            Err(error) => self.fail_plan_preflight(slot_index, error),
        }
    }

    pub(crate) fn preflight_tail_extension(
        &mut self,
        slot_index: usize,
        address_space: AddressSpaceId,
        mapping: MappingId,
        old_end: usize,
        new_end: usize,
    ) -> AxResult<OptionalUffdPlan> {
        self.preflight_boundary_extension(
            slot_index,
            address_space,
            mapping,
            old_end,
            new_end,
            false,
        )
    }

    pub(crate) fn preflight_head_extension(
        &mut self,
        slot_index: usize,
        address_space: AddressSpaceId,
        mapping: MappingId,
        old_start: usize,
        new_start: usize,
    ) -> AxResult<OptionalUffdPlan> {
        self.preflight_boundary_extension(
            slot_index,
            address_space,
            mapping,
            old_start,
            new_start,
            true,
        )
    }

    /// Consumes every token owned by a remap plan-set. The unchosen
    /// alternative is always released before the chosen table commit.
    pub(crate) fn resolve_remap(&mut self, prepared: PreparedRemapUffd, outcome: RemapUffdOutcome) {
        match prepared {
            PreparedRemapUffd::None => {}
            PreparedRemapUffd::FixedDuplicate { destination } => match outcome {
                RemapUffdOutcome::Preserved => self.abort_plan(destination),
                RemapUffdOutcome::DestructiveFailure | RemapUffdOutcome::Committed => {
                    self.commit_plan(destination);
                }
            },
            PreparedRemapUffd::FixedMove {
                on_failure,
                on_success,
            } => match outcome {
                RemapUffdOutcome::Preserved => {
                    self.abort_plan(on_failure);
                    self.abort_plan(on_success);
                }
                RemapUffdOutcome::DestructiveFailure => {
                    self.abort_plan(on_success);
                    self.commit_plan(on_failure);
                }
                RemapUffdOutcome::Committed => {
                    self.abort_plan(on_failure);
                    self.commit_plan(on_success);
                }
            },
            PreparedRemapUffd::NonfixedMove { on_success } => match outcome {
                RemapUffdOutcome::Committed => self.commit_plan(on_success),
                RemapUffdOutcome::Preserved | RemapUffdOutcome::DestructiveFailure => {
                    self.abort_plan(on_success);
                }
            },
        }
    }

    /// Commits a mapping-sidecar plan after the main MM transaction succeeded.
    ///
    /// The address-space lock excludes legitimate registration-table changes
    /// between preflight and commit. A token or table revision mismatch is thus
    /// internal state corruption and must fail-stop instead of returning an
    /// errno after the VMA/PTE commit is already visible.
    pub(crate) fn commit_plan(&mut self, plan: OptionalUffdPlan) {
        let OptionalUffdPlan::Armed(token) = plan else {
            return;
        };
        let slot_index = usize::from(token.slot);
        let Self {
            registrations,
            plan_slots,
            ..
        } = self;
        let slot = plan_slots
            .get_mut(slot_index)
            .expect("UFFD mapping plan token names an invalid slot");
        let armed = slot
            .armed
            .expect("UFFD mapping plan token was already consumed");
        assert_eq!(
            armed.nonce, token.nonce,
            "stale UFFD mapping plan token reused a bounded slot"
        );
        registrations
            .commit_mapping_replace(armed.plan, &slot.removed, &slot.replacements, |_| {})
            .expect("UFFD mapping plan became stale under the address-space lock");
        slot.armed = None;
        slot.clear_payload();
    }

    /// Releases an uncommitted mapping-sidecar plan without changing the table.
    pub(crate) fn abort_plan(&mut self, plan: OptionalUffdPlan) {
        let OptionalUffdPlan::Armed(token) = plan else {
            return;
        };
        let slot = self
            .plan_slots
            .get_mut(usize::from(token.slot))
            .expect("UFFD mapping plan token names an invalid slot");
        let armed = slot
            .armed
            .expect("UFFD mapping plan token was already consumed");
        assert_eq!(
            armed.nonce, token.nonce,
            "stale UFFD mapping plan token reused a bounded slot"
        );
        slot.armed = None;
        slot.clear_payload();
    }

    fn take_snapshot_scratch(&mut self) -> Vec<MappingSnapshot> {
        core::mem::take(&mut self.scratch.snapshots)
    }

    fn restore_snapshot_scratch(&mut self, snapshots: Vec<MappingSnapshot>) {
        debug_assert!(self.scratch.snapshots.is_empty());
        self.scratch.snapshots = snapshots;
    }

    fn project_registration_epochs(
        snapshots: &mut [MappingSnapshot],
        registrations: &UffdRegistrations,
    ) -> AxResult {
        for snapshot in snapshots {
            if let Some(epoch) = registrations
                .epoch_for_mapping(snapshot.address_space(), snapshot.mapping())
                .map_err(uffd_policy_error)?
            {
                *snapshot = snapshot.with_generation(epoch);
            }
        }
        Ok(())
    }

    pub(crate) fn register_range(
        &mut self,
        api: &UffdApiState,
        handler: FaultHandlerId,
        range: PageRange,
        mode: UffdRegisterMode,
        snapshots: &mut [MappingSnapshot],
    ) -> AxResult<UffdIoctls> {
        self.handler(handler)?;
        Self::project_registration_epochs(snapshots, &self.registrations)?;
        let intent =
            UffdRegistrationIntent::new(handler, range, mode).map_err(uffd_policy_error)?;
        let plan = self
            .registrations
            .preflight_register_delta(api, intent, snapshots)
            .map_err(uffd_policy_error)?;
        if plan.is_noop() {
            return Ok(UffdIoctls::MISSING_RANGE_PROFILE);
        }
        if plan.removed() > self.scratch.removed.capacity()
            || plan.replacements() > self.scratch.register_replacements.capacity()
        {
            return Err(AxError::NoMemory);
        }

        self.scratch.clear_transaction();
        let result = (|| {
            self.registrations
                .replay_register_delta(
                    plan,
                    intent,
                    snapshots,
                    |id| self.scratch.removed.push(id),
                    |request| self.scratch.register_replacements.push(request),
                )
                .map_err(uffd_policy_error)?;

            let commit = if self.scratch.removed.is_empty() {
                let plan = self
                    .registrations
                    .preflight_register(api, &self.scratch.register_replacements)
                    .map_err(uffd_policy_error)?;
                self.registrations.commit_register(
                    plan,
                    &self.scratch.register_replacements,
                    |_| {},
                )
            } else {
                let plan = self
                    .registrations
                    .preflight_replace(
                        api,
                        &self.scratch.removed,
                        &self.scratch.register_replacements,
                    )
                    .map_err(uffd_policy_error)?;
                self.registrations.commit_replace(
                    plan,
                    &self.scratch.removed,
                    &self.scratch.register_replacements,
                    |_| {},
                )
            };
            commit
                .map(|_| UffdIoctls::MISSING_RANGE_PROFILE)
                .map_err(uffd_policy_error)
        })();
        self.scratch.clear_transaction();
        result
    }

    fn snapshot_for_fragment(
        snapshots: &[MappingSnapshot],
        address_space: AddressSpaceId,
        mapping: MappingId,
        range: PageRange,
    ) -> AxResult<MappingSnapshot> {
        snapshots
            .iter()
            .copied()
            .find(|snapshot| {
                snapshot.address_space() == address_space
                    && snapshot.mapping() == mapping
                    && snapshot.range().contains(range)
            })
            .ok_or(AxError::BadState)
    }

    pub(crate) fn unregister_range(
        &mut self,
        api: &UffdApiState,
        range: PageRange,
        snapshots: &mut [MappingSnapshot],
    ) -> AxResult<DeferredUffdWake> {
        let address_space = self
            .registrations
            .validate_unregister_vmas(api, range, snapshots)
            .map_err(uffd_policy_error)?;
        Self::project_registration_epochs(snapshots, &self.registrations)?;

        self.scratch.clear_transaction();
        let result = (|| {
            for registration in self.registrations.intersecting(address_space, range) {
                self.handler(registration.handler())?;
                if self.scratch.removed.len() == self.scratch.removed.capacity()
                    || self.scratch.wake_spans.len() == self.scratch.wake_spans.capacity()
                {
                    return Err(AxError::NoMemory);
                }
                let wake_start = registration.range().start().max(range.start());
                let wake_end = registration.range().end().min(range.end());
                let wake_range = PageRange::with_page_size(
                    wake_start,
                    wake_end.checked_sub(wake_start).ok_or(AxError::BadState)?,
                    range.page_size(),
                )
                .map_err(uffd_policy_error)?;
                self.scratch.removed.push(registration.id());
                self.scratch.wake_spans.push(UffdWakeSpan {
                    handler: registration.handler(),
                    address_space: registration.address_space(),
                    mapping: registration.mapping(),
                    generation: registration.generation(),
                    range: wake_range,
                });

                for survivor in [
                    (registration.range().start(), wake_start),
                    (wake_end, registration.range().end()),
                ] {
                    let (start, end) = survivor;
                    if start == end {
                        continue;
                    }
                    if self.scratch.mapping_replacements.len()
                        == self.scratch.mapping_replacements.capacity()
                    {
                        return Err(AxError::NoMemory);
                    }
                    let survivor = PageRange::with_page_size(
                        start,
                        end.checked_sub(start).ok_or(AxError::BadState)?,
                        range.page_size(),
                    )
                    .map_err(uffd_policy_error)?;
                    let current = Self::snapshot_for_fragment(
                        snapshots,
                        registration.address_space(),
                        registration.mapping(),
                        survivor,
                    )?;
                    let request = registration
                        .refreshed_fragment(current, survivor)
                        .map_err(uffd_policy_error)?;
                    self.scratch
                        .mapping_replacements
                        .push(UffdRegistrationReplacement::new(registration.id(), request));
                }
            }

            if self.scratch.removed.is_empty() {
                return Ok(DeferredUffdWake::empty());
            }
            let plan = self
                .registrations
                .preflight_mapping_replace(
                    &self.scratch.removed,
                    &self.scratch.mapping_replacements,
                )
                .map_err(uffd_policy_error)?;
            self.registrations
                .commit_mapping_replace(
                    plan,
                    &self.scratch.removed,
                    &self.scratch.mapping_replacements,
                    |_| {},
                )
                .map_err(uffd_policy_error)?;

            let mut deferred = DeferredUffdWake::empty();
            let (completed, released) = {
                let wake_spans = &self.scratch.wake_spans;
                let completed = self.broker.complete_where(
                    |snapshot| {
                        wake_spans
                            .iter()
                            .copied()
                            .any(|span| span.matches(*snapshot.handler(), *snapshot.key()))
                    },
                    FaultDisposition::Cancelled,
                    CompletionVisibility::Visible,
                );
                let released = self.broker.release_where(|snapshot| {
                    wake_spans
                        .iter()
                        .copied()
                        .any(|span| span.matches(*snapshot.handler(), *snapshot.key()))
                });
                (completed, released)
            };
            if completed.requests_completed() != 0 {
                // Readiness is a hint. Waking every affected handler keeps the
                // hot broker path to one bounded scan and DeferredUffdWake
                // deduplicates shared handler readiness objects.
                for index in 0..self.scratch.wake_spans.len() {
                    let span = self.scratch.wake_spans[index];
                    deferred.add_handler(
                        self.handler(span.handler)
                            .expect("registered UFFD handler disappeared under address-space lock")
                            .readiness
                            .clone(),
                    );
                }
            }
            if completed.waiters_released() + released.waiters_released() != 0 {
                deferred.fault_completion = Some(self.fault_completion.clone());
            }
            Ok(deferred)
        })();
        self.scratch.clear_transaction();
        result
    }

    #[cfg(test)]
    pub(crate) fn set_test_snapshots(&mut self, snapshots: &[MappingSnapshot]) -> AxResult {
        if snapshots.len() > self.scratch.snapshots.capacity() {
            return Err(AxError::NoMemory);
        }
        self.scratch.snapshots.clear();
        self.scratch.snapshots.extend_from_slice(snapshots);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn register_test_range(
        &mut self,
        api: &UffdApiState,
        handler: FaultHandlerId,
        range: PageRange,
        mode: UffdRegisterMode,
    ) -> AxResult<UffdIoctls> {
        let mut snapshots = self.take_snapshot_scratch();
        let result = self.register_range(api, handler, range, mode, &mut snapshots);
        self.restore_snapshot_scratch(snapshots);
        result
    }

    #[cfg(test)]
    pub(crate) fn unregister_test_range(
        &mut self,
        api: &UffdApiState,
        range: PageRange,
    ) -> AxResult<DeferredUffdWake> {
        let mut snapshots = self.take_snapshot_scratch();
        let result = self.unregister_range(api, range, &mut snapshots);
        self.restore_snapshot_scratch(snapshots);
        result
    }

    pub(crate) fn attach_handler(
        &mut self,
        readiness: Arc<UffdPollSet>,
    ) -> AxResult<FaultHandlerId> {
        let slot = self
            .handlers
            .iter()
            .position(Option::is_none)
            .ok_or(AxError::NoMemory)?;
        let id = allocate_handler_id()?;
        self.handlers[slot] = Some(UffdHandlerState { id, readiness });
        Ok(id)
    }

    fn handler(&self, id: FaultHandlerId) -> AxResult<&UffdHandlerState> {
        self.handlers
            .iter()
            .flatten()
            .find(|handler| handler.id == id)
            .ok_or(AxError::BadFileDescriptor)
    }

    pub(crate) fn detach_handler(&mut self, id: FaultHandlerId) -> AxResult<DeferredUffdWake> {
        let slot = self
            .handlers
            .iter()
            .position(|handler| handler.as_ref().is_some_and(|handler| handler.id == id))
            .ok_or(AxError::BadFileDescriptor)?;
        self.registrations
            .detach_handler(id)
            .map_err(uffd_policy_error)?;
        let completed = self
            .broker
            .detach_handler(id, FaultDisposition::HandlerDetached);
        let readiness = self.handlers[slot]
            .take()
            .map(|handler| handler.readiness)
            .ok_or(AxError::BadState)?;
        let mut deferred = DeferredUffdWake::empty();
        deferred.add_handler(readiness);
        if completed.waiters_released() != 0 {
            deferred.fault_completion = Some(self.fault_completion.clone());
        }
        Ok(deferred)
    }

    fn has_handlers(&self) -> bool {
        self.handlers.iter().any(Option::is_some)
    }

    pub(crate) fn pending(&self, id: FaultHandlerId) -> AxResult<bool> {
        self.handler(id)?;
        Ok(self.broker.has_pending(id))
    }

    pub(crate) fn claim_next(
        &mut self,
        id: FaultHandlerId,
    ) -> AxResult<Option<DeliveredUffdEvent>> {
        self.handler(id)?;
        Ok(self
            .broker
            .claim_next(id)
            .map(|snapshot| DeliveredUffdEvent {
                request: *snapshot.key(),
            }))
    }

    #[cfg(test)]
    pub(crate) fn admit_test_request(
        &mut self,
        handler: FaultHandlerId,
        request: FaultRequest,
    ) -> axfault::FaultAdmission {
        assert_eq!(request.handler(), handler);
        self.handler(handler).expect("test handler is live");
        self.broker
            .admit(handler, request)
            .expect("test broker has capacity")
    }

    #[cfg(test)]
    pub(crate) fn complete_test_request(&mut self, request: axfault::RequestToken) {
        self.broker
            .complete(
                request,
                FaultDisposition::ZeroFill,
                axfault::CompletionVisibility::Visible,
            )
            .expect("test request is live");
    }

    #[cfg(test)]
    pub(crate) fn observe_test_waiter(
        &self,
        waiter: axfault::WaiterToken,
    ) -> Result<axfault::WaiterObservation<FaultDisposition>, axfault::WaiterError> {
        self.broker.waiter(waiter)
    }

    #[cfg(test)]
    pub(crate) fn fault_completion_for_test(&self) -> Arc<UffdPollSet> {
        self.fault_completion.clone()
    }
}

/// RAII abort owner used by `PreparedProtect`.
///
/// A failed or simply dropped main-MM transaction releases only the preflight
/// proof and its Copy payload. Successful MM commit explicitly consumes the
/// table plan before mapping generations are published.
pub(crate) struct PreparedUffdMutation<'a> {
    state: &'a mut UffdAddressSpaceState,
    plan: OptionalUffdPlan,
    finished: bool,
}

impl<'a> PreparedUffdMutation<'a> {
    pub(crate) const fn new(state: &'a mut UffdAddressSpaceState, plan: OptionalUffdPlan) -> Self {
        Self {
            state,
            plan,
            finished: false,
        }
    }

    pub(crate) fn commit(mut self) {
        // `commit_plan` treats an impossible token/revision mismatch as
        // fail-stop. Disarm this outer RAII owner first so host unwind does
        // not attempt a second abort with the same corrupt token and turn the
        // original invariant failure into a double panic.
        self.finished = true;
        self.state.commit_plan(self.plan);
    }
}

impl Drop for PreparedUffdMutation<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.state.abort_plan(self.plan);
        }
    }
}

impl Drop for UffdAddressSpaceState {
    fn drop(&mut self) {
        // AddrSpace destruction can outlive a non-CLOEXEC userfaultfd OFD.
        // Publish terminal ownership and wake every retained file context;
        // the file then observes an inert old-mm binding rather than an error.
        let mut wake_completion = false;
        for handler in self.handlers.iter().flatten() {
            let completed = self
                .broker
                .detach_handler(handler.id, FaultDisposition::HandlerDetached);
            handler.readiness.wake();
            wake_completion |= completed.waiters_released() != 0;
        }
        if wake_completion {
            self.fault_completion.wake();
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct DeliveredUffdEvent {
    request: FaultRequest,
}

impl DeliveredUffdEvent {
    pub(crate) const fn request(self) -> FaultRequest {
        self.request
    }
}

/// Ownership retired by final OFD close.  Wake and destruction happen only
/// after the caller releases the address-space lock.
pub(crate) struct DetachedUffdHandler {
    wake: DeferredUffdWake,
    retired_state: Option<Box<UffdAddressSpaceState>>,
}

impl DetachedUffdHandler {
    pub(crate) fn finish(self) {
        self.wake.finish();
        drop(self.retired_state);
    }
}

impl AddrSpace {
    pub(crate) const fn needs_uffd_state(&self) -> bool {
        self.uffd.is_none()
    }

    /// Installs a state candidate prepared outside the address-space lock and
    /// attaches one handler.  `WouldBlock` asks the caller to allocate a
    /// candidate and retry.  A racing winner leaves an unused candidate owned
    /// by the caller for lock-external destruction.
    pub(crate) fn attach_uffd_handler(
        &mut self,
        candidate: &mut Option<Box<UffdAddressSpaceState>>,
        readiness: Arc<UffdPollSet>,
    ) -> AxResult<FaultHandlerId> {
        let installed = self.uffd.is_none();
        if installed {
            self.uffd = Some(candidate.take().ok_or(AxError::WouldBlock)?);
        }
        let attached = self
            .uffd
            .as_mut()
            .expect("userfault state was installed")
            .attach_handler(readiness);
        if attached.is_err()
            && installed
            && self
                .uffd
                .as_ref()
                .is_some_and(|state| !state.has_handlers())
        {
            *candidate = self.uffd.take();
        }
        attached
    }

    pub(crate) fn register_uffd_range(
        &mut self,
        api: &UffdApiState,
        handler: FaultHandlerId,
        range: PageRange,
        mode: UffdRegisterMode,
    ) -> AxResult<UffdIoctls> {
        let mut snapshots = self
            .uffd
            .as_mut()
            .ok_or(AxError::BadFileDescriptor)?
            .take_snapshot_scratch();
        snapshots.clear();
        let scanned = self.append_uffd_mapping_snapshots(range, &mut snapshots);
        let result = match scanned {
            Ok(()) => self
                .uffd
                .as_mut()
                .expect("UFFD state disappeared under address-space lock")
                .register_range(api, handler, range, mode, &mut snapshots),
            Err(error) => Err(error),
        };
        self.uffd
            .as_mut()
            .expect("UFFD state disappeared under address-space lock")
            .restore_snapshot_scratch(snapshots);
        result
    }

    pub(crate) fn unregister_uffd_range(
        &mut self,
        api: &UffdApiState,
        range: PageRange,
    ) -> AxResult<DeferredUffdWake> {
        let mut snapshots = self
            .uffd
            .as_mut()
            .ok_or(AxError::BadFileDescriptor)?
            .take_snapshot_scratch();
        snapshots.clear();
        let scanned = self.append_uffd_mapping_snapshots(range, &mut snapshots);
        let result = match scanned {
            Ok(()) => self
                .uffd
                .as_mut()
                .expect("UFFD state disappeared under address-space lock")
                .unregister_range(api, range, &mut snapshots),
            Err(error) => Err(error),
        };
        self.uffd
            .as_mut()
            .expect("UFFD state disappeared under address-space lock")
            .restore_snapshot_scratch(snapshots);
        result
    }

    pub(crate) fn detach_uffd_handler(
        &mut self,
        handler: FaultHandlerId,
    ) -> AxResult<DetachedUffdHandler> {
        let (wake, retire_state) = {
            let state = self.uffd.as_mut().ok_or(AxError::BadFileDescriptor)?;
            let wake = state.detach_handler(handler)?;
            (wake, !state.has_handlers())
        };
        let retired_state =
            retire_state.then(|| self.uffd.take().expect("empty userfault state disappeared"));
        Ok(DetachedUffdHandler {
            wake,
            retired_state,
        })
    }

    pub(crate) fn uffd_handler_pending(&self, handler: FaultHandlerId) -> AxResult<bool> {
        self.uffd
            .as_ref()
            .ok_or(AxError::BadFileDescriptor)?
            .pending(handler)
    }

    pub(crate) fn claim_uffd_event(
        &mut self,
        handler: FaultHandlerId,
    ) -> AxResult<Option<DeliveredUffdEvent>> {
        self.uffd
            .as_mut()
            .ok_or(AxError::BadFileDescriptor)?
            .claim_next(handler)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::vec;

    use memory_addr::PAGE_SIZE_4K;
    use thekernel_linux_mm::{
        FaultAccess, FaultKey, FaultType, MappingAccess, MappingKind, MappingSnapshot, UFFD_API,
    };

    use super::*;

    fn snapshot(
        mapping: u64,
        generation: u64,
        start: usize,
        length: usize,
        kind: MappingKind,
    ) -> MappingSnapshot {
        MappingSnapshot::from_raw(
            1,
            mapping,
            generation,
            start,
            length,
            PAGE_SIZE_4K,
            MappingAccess::new(true, true, false).bits(),
            kind,
            true,
            false,
        )
        .unwrap()
    }

    fn projected_fragment(mapping: MappingSnapshot, range: PageRange) -> MappingSnapshot {
        MappingSnapshot::new(
            mapping.address_space(),
            mapping.mapping(),
            mapping.generation(),
            range,
            mapping.access(),
            mapping.kind(),
            mapping.long_term_pinnable(),
            mapping.writable_file_pin_supported(),
        )
    }

    fn page_range(start: usize, length: usize) -> PageRange {
        PageRange::new(start, length, PAGE_SIZE_4K).unwrap()
    }

    fn initialized_api() -> UffdApiState {
        let mut api = UffdApiState::new();
        let negotiation = api.prepare_raw(UFFD_API, 0).unwrap();
        api.commit(negotiation).unwrap();
        api
    }

    fn registered_remap_state() -> (UffdAddressSpaceState, MappingSnapshot, MappingSnapshot) {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let source_handler = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let destination_handler = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let api = initialized_api();
        let source = snapshot(
            2,
            61,
            0x1000,
            2 * PAGE_SIZE_4K,
            MappingKind::AnonymousPrivate,
        );
        let destination = snapshot(
            3,
            67,
            0x5000,
            2 * PAGE_SIZE_4K,
            MappingKind::AnonymousPrivate,
        );
        for (handler, mapping) in [(source_handler, source), (destination_handler, destination)] {
            let mut current = [mapping];
            state
                .register_range(
                    &api,
                    handler,
                    mapping.range(),
                    UffdRegisterMode::MISSING,
                    &mut current,
                )
                .unwrap();
        }
        (state, source, destination)
    }

    fn request_for(
        snapshot: MappingSnapshot,
        handler: FaultHandlerId,
        page: usize,
    ) -> FaultRequest {
        FaultRequest::new(
            FaultKey::from_address(snapshot, page, FaultAccess::Read).unwrap(),
            handler,
            FaultType::Missing,
        )
    }

    fn request(handler: FaultHandlerId, page: usize) -> FaultRequest {
        request_for(
            snapshot(2, 1, page, PAGE_SIZE_4K, MappingKind::AnonymousPrivate),
            handler,
            page,
        )
    }

    #[test]
    fn pending_is_derived_from_authoritative_broker_phase() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let readiness = Arc::new(UffdPollSet::new());
        let handler = state.attach_handler(readiness).unwrap();
        let admission = state
            .broker
            .admit(handler, request(handler, 0x1000))
            .unwrap();

        assert!(state.pending(handler).unwrap());
        state
            .broker
            .complete(
                admission.request(),
                FaultDisposition::ZeroFill,
                axfault::CompletionVisibility::Visible,
            )
            .unwrap();
        assert!(!state.pending(handler).unwrap());
        assert!(state.claim_next(handler).unwrap().is_none());
    }

    #[test]
    fn claim_moves_event_out_of_readiness_without_a_side_counter() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let readiness = Arc::new(UffdPollSet::new());
        let handler = state.attach_handler(readiness).unwrap();
        state
            .broker
            .admit(handler, request(handler, 0x2000))
            .unwrap();

        assert!(state.pending(handler).unwrap());
        let event = state.claim_next(handler).unwrap().unwrap();
        assert_eq!(event.request().key().page_address().get(), 0x2000);
        assert!(!state.pending(handler).unwrap());
        assert!(state.claim_next(handler).unwrap().is_none());
    }

    #[test]
    fn register_allows_holes_and_publishes_only_mapped_fragments() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let handler = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let api = initialized_api();
        let mut mappings = [
            snapshot(2, 1, 0x1000, 0x2000, MappingKind::AnonymousPrivate),
            snapshot(3, 1, 0x5000, 0x2000, MappingKind::AnonymousPrivate),
        ];

        let ioctls = state
            .register_range(
                &api,
                handler,
                page_range(0x1000, 0x6000),
                UffdRegisterMode::MISSING,
                &mut mappings,
            )
            .unwrap();

        assert_eq!(ioctls, UffdIoctls::MISSING_RANGE_PROFILE);
        let mut registered: Vec<_> = state.registrations.iter().collect();
        registered.sort_by_key(|registration| registration.range().start());
        assert_eq!(registered.len(), 2);
        assert_eq!(registered[0].range(), page_range(0x1000, 0x2000));
        assert_eq!(registered[1].range(), page_range(0x5000, 0x2000));
    }

    #[test]
    fn register_late_incompatible_vma_is_atomic() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let handler = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let api = initialized_api();
        let mut mappings = [
            snapshot(2, 1, 0x1000, 0x2000, MappingKind::AnonymousPrivate),
            snapshot(3, 1, 0x5000, 0x2000, MappingKind::FilePrivate),
        ];

        assert_eq!(
            state.register_range(
                &api,
                handler,
                page_range(0x1000, 0x6000),
                UffdRegisterMode::MISSING,
                &mut mappings,
            ),
            Err(AxError::InvalidInput)
        );
        assert!(state.registrations.is_empty());
    }

    #[test]
    fn register_foreign_overlap_is_busy_without_mutation() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let first = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let second = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let api = initialized_api();
        let mut mappings = [snapshot(
            2,
            1,
            0x1000,
            0x8000,
            MappingKind::AnonymousPrivate,
        )];
        state
            .register_range(
                &api,
                first,
                page_range(0x2000, 0x2000),
                UffdRegisterMode::MISSING,
                &mut mappings,
            )
            .unwrap();

        assert_eq!(
            state.register_range(
                &api,
                second,
                page_range(0x3000, 0x2000),
                UffdRegisterMode::MISSING,
                &mut mappings,
            ),
            Err(AxError::ResourceBusy)
        );
        let registered: Vec<_> = state.registrations.iter().collect();
        assert_eq!(registered.len(), 1);
        assert_eq!(registered[0].handler(), first);
        assert_eq!(registered[0].range(), page_range(0x2000, 0x2000));
    }

    #[test]
    fn same_handler_subset_extension_and_bridge_stay_canonical() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let handler = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let api = initialized_api();
        let mut mappings = [snapshot(
            2,
            1,
            0x1000,
            0x8000,
            MappingKind::AnonymousPrivate,
        )];
        for range in [
            page_range(0x2000, 0x2000),
            page_range(0x3000, 0x1000),
            page_range(0x1000, 0x3000),
            page_range(0x6000, 0x1000),
            page_range(0x4000, 0x2000),
        ] {
            state
                .register_range(
                    &api,
                    handler,
                    range,
                    UffdRegisterMode::MISSING,
                    &mut mappings,
                )
                .unwrap();
        }
        let registered: Vec<_> = state.registrations.iter().collect();
        assert_eq!(registered.len(), 1);
        assert_eq!(registered[0].range(), page_range(0x1000, 0x6000));
    }

    #[test]
    fn registration_capacity_failure_leaves_existing_table_unchanged() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let handler = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let api = initialized_api();
        for index in 0..UFFD_MAX_REGISTRATIONS {
            let start = 0x1000 + index * 0x2000;
            let mut mapping = [snapshot(
                10 + index as u64,
                1,
                start,
                PAGE_SIZE_4K,
                MappingKind::AnonymousPrivate,
            )];
            state
                .register_range(
                    &api,
                    handler,
                    page_range(start, PAGE_SIZE_4K),
                    UffdRegisterMode::MISSING,
                    &mut mapping,
                )
                .unwrap();
        }
        assert_eq!(state.registrations.len(), UFFD_MAX_REGISTRATIONS);

        let start = 0x1000 + UFFD_MAX_REGISTRATIONS * 0x2000;
        let mut overflow = [snapshot(
            1000,
            1,
            start,
            PAGE_SIZE_4K,
            MappingKind::AnonymousPrivate,
        )];
        assert_eq!(
            state.register_range(
                &api,
                handler,
                page_range(start, PAGE_SIZE_4K),
                UffdRegisterMode::MISSING,
                &mut overflow,
            ),
            Err(AxError::NoMemory)
        );
        assert_eq!(state.registrations.len(), UFFD_MAX_REGISTRATIONS);
    }

    #[test]
    fn unregister_trims_mixed_owners_and_wakes_only_exact_registered_faults() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let first = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let second = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let api = initialized_api();
        let first_mapping = snapshot(2, 7, 0x1000, 0x3000, MappingKind::AnonymousPrivate);
        let second_mapping = snapshot(3, 9, 0x6000, 0x3000, MappingKind::AnonymousPrivate);
        for (handler, mapping) in [(first, first_mapping), (second, second_mapping)] {
            let mut current = [mapping];
            state
                .register_range(
                    &api,
                    handler,
                    mapping.range(),
                    UffdRegisterMode::MISSING,
                    &mut current,
                )
                .unwrap();
        }

        let first_fault =
            state.admit_test_request(first, request_for(first_mapping, first, 0x1000));
        let second_fault =
            state.admit_test_request(second, request_for(second_mapping, second, 0x6000));
        let gap_mapping = snapshot(99, 1, 0x4000, 0x2000, MappingKind::AnonymousPrivate);
        let gap_fault = state.admit_test_request(first, request_for(gap_mapping, first, 0x4000));

        let mut current = [first_mapping, second_mapping];
        let deferred = state
            .unregister_range(&api, page_range(0x1000, 0x8000), &mut current)
            .unwrap();

        assert!(state.registrations.is_empty());
        assert_eq!(
            state.observe_test_waiter(first_fault.waiter()).unwrap(),
            axfault::WaiterObservation::Ready(FaultDisposition::Cancelled)
        );
        assert_eq!(
            state.observe_test_waiter(second_fault.waiter()).unwrap(),
            axfault::WaiterObservation::Ready(FaultDisposition::Cancelled)
        );
        assert_eq!(
            state.observe_test_waiter(gap_fault.waiter()).unwrap(),
            axfault::WaiterObservation::Pending
        );
        assert!(!deferred.is_empty());
        deferred.finish();
    }

    #[test]
    fn unregister_middle_preserves_left_and_right_registration_epoch() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let handler = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let api = initialized_api();
        let mapping = snapshot(2, 17, 0x1000, 0x8000, MappingKind::AnonymousPrivate);
        let mut current = [mapping];
        state
            .register_range(
                &api,
                handler,
                mapping.range(),
                UffdRegisterMode::MISSING,
                &mut current,
            )
            .unwrap();

        let deferred = state
            .unregister_range(&api, page_range(0x4000, 0x2000), &mut current)
            .unwrap();
        assert!(deferred.is_empty());
        let mut fragments: Vec<_> = state.registrations.iter().collect();
        fragments.sort_by_key(|registration| registration.range().start());
        assert_eq!(fragments.len(), 2);
        assert_eq!(fragments[0].range(), page_range(0x1000, 0x3000));
        assert_eq!(fragments[1].range(), page_range(0x6000, 0x3000));
        assert_eq!(fragments[0].generation(), fragments[1].generation());
        assert_eq!(fragments[0].generation().get(), 17);
        deferred.finish();
    }

    #[test]
    fn protect_plan_splits_boundaries_and_preserves_fault_epoch() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let handler = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let api = initialized_api();
        let mapping = snapshot(2, 17, 0x1000, 0x8000, MappingKind::AnonymousPrivate);
        let mut current = [mapping];
        state
            .register_range(
                &api,
                handler,
                mapping.range(),
                UffdRegisterMode::MISSING,
                &mut current,
            )
            .unwrap();

        let plan = state
            .preflight_protect(0, page_range(0x4000, 0x2000), |registration, fragment| {
                assert_eq!(registration.mapping(), mapping.mapping());
                Ok(Some(projected_fragment(mapping, fragment)))
            })
            .unwrap();
        assert!(matches!(plan, OptionalUffdPlan::Armed(_)));
        assert_eq!(state.registrations.len(), 1);
        state.commit_plan(plan);

        let mut fragments: Vec<_> = state.registrations.iter().collect();
        fragments.sort_by_key(|registration| registration.range().start());
        assert_eq!(fragments.len(), 3);
        assert_eq!(fragments[0].range(), page_range(0x1000, 0x3000));
        assert_eq!(fragments[1].range(), page_range(0x4000, 0x2000));
        assert_eq!(fragments[2].range(), page_range(0x6000, 0x3000));
        for fragment in fragments {
            assert_eq!(fragment.handler(), handler);
            assert_eq!(fragment.mapping(), mapping.mapping());
            assert_eq!(fragment.generation().get(), 17);
            assert_eq!(fragment.mode(), UffdRegisterMode::MISSING);
        }
    }

    #[test]
    fn protect_split_restore_churn_recanonicalizes_three_fragments_to_one() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let handler = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let api = initialized_api();
        let mapping = snapshot(2, 41, 0x1000, 0x4000, MappingKind::AnonymousPrivate);
        let mut current = [mapping];
        state
            .register_range(
                &api,
                handler,
                mapping.range(),
                UffdRegisterMode::MISSING,
                &mut current,
            )
            .unwrap();

        for _ in 0..(UFFD_MAX_REGISTRATIONS + 1) {
            let split = state
                .preflight_protect(0, page_range(0x2000, PAGE_SIZE_4K), |_, fragment| {
                    Ok(Some(projected_fragment(mapping, fragment)))
                })
                .unwrap();
            state.commit_plan(split);
            assert_eq!(state.registrations.len(), 3);

            let restore = state
                .preflight_protect(0, page_range(0x2000, PAGE_SIZE_4K), |_, _| {
                    Ok(Some(mapping))
                })
                .unwrap();
            state.commit_plan(restore);
            let canonical: Vec<_> = state.registrations.iter().collect();
            assert_eq!(canonical.len(), 1);
            assert_eq!(canonical[0].range(), mapping.range());
            assert_eq!(canonical[0].handler(), handler);
            assert_eq!(canonical[0].generation(), mapping.generation());
        }
    }

    #[test]
    fn protect_post_vma_never_merges_adjacent_different_handlers() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let first_handler = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let second_handler = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let api = initialized_api();
        let mapping = snapshot(2, 43, 0x1000, 0x2000, MappingKind::AnonymousPrivate);
        let left = page_range(0x1000, PAGE_SIZE_4K);
        let right = page_range(0x2000, PAGE_SIZE_4K);
        for (handler, range) in [(first_handler, left), (second_handler, right)] {
            let mut current = [mapping];
            state
                .register_range(
                    &api,
                    handler,
                    range,
                    UffdRegisterMode::MISSING,
                    &mut current,
                )
                .unwrap();
        }
        let before: Vec<_> = state.registrations.iter().collect();

        let plan = state
            .preflight_protect(0, right, |_, _| Ok(Some(mapping)))
            .unwrap();
        assert_eq!(plan, OptionalUffdPlan::Noop);
        assert_eq!(state.registrations.iter().collect::<Vec<_>>(), before);
        assert_eq!(state.next_plan_nonce, 1);
    }

    #[test]
    fn protect_post_vma_never_merges_adjacent_different_fault_epochs() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let handler = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let api = initialized_api();
        let post_vma = snapshot(2, 47, 0x1000, 0x2000, MappingKind::AnonymousPrivate);
        let left = page_range(0x1000, PAGE_SIZE_4K);
        let right = page_range(0x2000, PAGE_SIZE_4K);
        for (generation, range) in [(43, left), (47, right)] {
            let source = snapshot(
                post_vma.mapping().get(),
                generation,
                post_vma.range().start(),
                post_vma.range().len(),
                MappingKind::AnonymousPrivate,
            );
            let request =
                UffdRegistrationRequest::new(handler, source, range, UffdRegisterMode::MISSING)
                    .unwrap();
            state.registrations.register(&api, request).unwrap();
        }
        let before: Vec<_> = state.registrations.iter().collect();

        let plan = state
            .preflight_protect(0, right, |_, _| Ok(Some(post_vma)))
            .unwrap();
        assert_eq!(plan, OptionalUffdPlan::Noop);
        assert_eq!(state.registrations.iter().collect::<Vec<_>>(), before);
        assert_eq!(state.next_plan_nonce, 1);
    }

    #[test]
    fn dropped_prepared_protect_aborts_registration_transaction() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let handler = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let api = initialized_api();
        let mapping = snapshot(2, 19, 0x1000, 0x6000, MappingKind::AnonymousPrivate);
        let mut current = [mapping];
        state
            .register_range(
                &api,
                handler,
                mapping.range(),
                UffdRegisterMode::MISSING,
                &mut current,
            )
            .unwrap();
        let before: Vec<_> = state.registrations.iter().collect();

        let plan = state
            .preflight_protect(0, page_range(0x3000, 0x1000), |_, fragment| {
                Ok(Some(projected_fragment(mapping, fragment)))
            })
            .unwrap();
        drop(PreparedUffdMutation::new(&mut state, plan));

        assert_eq!(state.registrations.iter().collect::<Vec<_>>(), before);
        assert!(state.plan_slots[0].armed.is_none());
        assert!(state.plan_slots[0].removed.is_empty());
        assert!(state.plan_slots[0].replacements.is_empty());
    }

    #[test]
    fn protect_capacity_failure_leaves_table_and_plan_slot_unchanged() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let handler = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let api = initialized_api();
        let first = snapshot(2, 23, 0x1000, 0x2000, MappingKind::AnonymousPrivate);
        let mut first_current = [first];
        state
            .register_range(
                &api,
                handler,
                first.range(),
                UffdRegisterMode::MISSING,
                &mut first_current,
            )
            .unwrap();
        for index in 1..UFFD_MAX_REGISTRATIONS {
            let start = 0x10000 + index * 0x2000;
            let mapping = snapshot(
                2 + index as u64,
                23 + index as u64,
                start,
                PAGE_SIZE_4K,
                MappingKind::AnonymousPrivate,
            );
            let mut current = [mapping];
            state
                .register_range(
                    &api,
                    handler,
                    mapping.range(),
                    UffdRegisterMode::MISSING,
                    &mut current,
                )
                .unwrap();
        }
        let before: Vec<_> = state.registrations.iter().collect();

        assert_eq!(
            state.preflight_protect(
                0,
                page_range(0x2000, PAGE_SIZE_4K),
                |registration, fragment| {
                    Ok((registration.mapping() == first.mapping())
                        .then_some(projected_fragment(first, fragment)))
                },
            ),
            Err(AxError::NoMemory)
        );
        assert_eq!(state.registrations.iter().collect::<Vec<_>>(), before);
        assert!(state.plan_slots[0].armed.is_none());
        assert!(state.plan_slots[0].removed.is_empty());
        assert!(state.plan_slots[0].replacements.is_empty());
    }

    #[test]
    fn ordinary_unmap_trims_epoch_without_completing_or_waking_faults() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let handler = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let api = initialized_api();
        let mapping = snapshot(2, 29, 0x1000, 0x8000, MappingKind::AnonymousPrivate);
        let mut current = [mapping];
        state
            .register_range(
                &api,
                handler,
                mapping.range(),
                UffdRegisterMode::MISSING,
                &mut current,
            )
            .unwrap();
        let admission = state.admit_test_request(handler, request_for(mapping, handler, 0x4000));

        let plan = state
            .preflight_unmap(0, page_range(0x4000, 0x2000), |_| Ok(mapping))
            .unwrap();
        state.commit_plan(plan);

        let mut fragments: Vec<_> = state.registrations.iter().collect();
        fragments.sort_by_key(|registration| registration.range().start());
        assert_eq!(fragments.len(), 2);
        assert_eq!(fragments[0].range(), page_range(0x1000, 0x3000));
        assert_eq!(fragments[1].range(), page_range(0x6000, 0x3000));
        assert!(
            fragments
                .iter()
                .all(|registration| registration.generation().get() == 29)
        );
        assert!(state.pending(handler).unwrap());
        assert_eq!(
            state.observe_test_waiter(admission.waiter()).unwrap(),
            axfault::WaiterObservation::Pending
        );
        let plan = state
            .preflight_unmap(0, page_range(0, 0xa000), |_| {
                panic!("fully removed UFFD registrations need no survivor snapshot")
            })
            .unwrap();
        state.commit_plan(plan);
        assert!(state.registrations.is_empty());
        assert!(state.pending(handler).unwrap());
        assert_eq!(
            state.observe_test_waiter(admission.waiter()).unwrap(),
            axfault::WaiterObservation::Pending
        );
        // The pending observation above is the no-wake assertion. Retire the
        // synthetic request explicitly so this test does not rely on state
        // teardown to release its bounded broker slot.
        state.complete_test_request(admission.request());
    }

    #[test]
    fn unmap_prefix_and_suffix_trim_without_inverted_fragments() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let handler = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let api = initialized_api();
        let mapping = snapshot(2, 30, 0x2000, 0x4000, MappingKind::AnonymousPrivate);
        let mut current = [mapping];
        state
            .register_range(
                &api,
                handler,
                mapping.range(),
                UffdRegisterMode::MISSING,
                &mut current,
            )
            .unwrap();

        let prefix = state
            .preflight_unmap(0, page_range(0x1000, 0x2000), |_| Ok(mapping))
            .unwrap();
        state.commit_plan(prefix);
        let after_prefix = state.registrations.iter().next().unwrap();
        assert_eq!(after_prefix.range(), page_range(0x3000, 0x3000));
        assert_eq!(after_prefix.generation().get(), 30);

        let suffix = state
            .preflight_unmap(0, page_range(0x5000, 0x2000), |_| Ok(mapping))
            .unwrap();
        state.commit_plan(suffix);
        let after_suffix: Vec<_> = state.registrations.iter().collect();
        assert_eq!(after_suffix.len(), 1);
        assert_eq!(after_suffix[0].range(), page_range(0x3000, 0x2000));
        assert_eq!(after_suffix[0].handler(), handler);
        assert_eq!(after_suffix[0].mapping(), mapping.mapping());
        assert_eq!(after_suffix[0].generation().get(), 30);
    }

    #[test]
    fn two_retirement_ranges_produce_three_epoch_preserving_survivors() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let handler = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let api = initialized_api();
        let mapping = snapshot(2, 71, 0x1000, 0x9000, MappingKind::AnonymousPrivate);
        let mut current = [mapping];
        state
            .register_range(
                &api,
                handler,
                mapping.range(),
                UffdRegisterMode::MISSING,
                &mut current,
            )
            .unwrap();

        let plan = state
            .preflight_unmap_ranges(
                0,
                [
                    Some(page_range(0x3000, PAGE_SIZE_4K)),
                    Some(page_range(0x6000, 0x2000)),
                ],
                |_| Ok(mapping),
            )
            .unwrap();
        state.commit_plan(plan);

        let mut survivors: Vec<_> = state.registrations.iter().collect();
        survivors.sort_by_key(|registration| registration.range().start());
        assert_eq!(survivors.len(), 3);
        assert_eq!(survivors[0].range(), page_range(0x1000, 0x2000));
        assert_eq!(survivors[1].range(), page_range(0x4000, 0x2000));
        assert_eq!(survivors[2].range(), page_range(0x8000, 0x2000));
        assert!(survivors.iter().all(|registration| {
            registration.handler() == handler
                && registration.mapping() == mapping.mapping()
                && registration.generation().get() == 71
        }));
    }

    #[test]
    fn two_retirement_ranges_normalize_reverse_touching_union() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let handler = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let api = initialized_api();
        let mapping = snapshot(2, 73, 0x1000, 0x8000, MappingKind::AnonymousPrivate);
        let mut current = [mapping];
        state
            .register_range(
                &api,
                handler,
                mapping.range(),
                UffdRegisterMode::MISSING,
                &mut current,
            )
            .unwrap();

        let plan = state
            .preflight_unmap_ranges(
                0,
                [
                    Some(page_range(0x5000, 0x2000)),
                    Some(page_range(0x3000, 0x2000)),
                ],
                |_| Ok(mapping),
            )
            .unwrap();
        state.commit_plan(plan);

        let mut survivors: Vec<_> = state.registrations.iter().collect();
        survivors.sort_by_key(|registration| registration.range().start());
        assert_eq!(survivors.len(), 2);
        assert_eq!(survivors[0].range(), page_range(0x1000, 0x2000));
        assert_eq!(survivors[1].range(), page_range(0x7000, 0x2000));
    }

    #[test]
    fn fixed_move_resolves_both_alternatives_and_releases_bounded_slots() {
        for (outcome, expected_ranges) in [
            (
                RemapUffdOutcome::Preserved,
                [
                    Some(page_range(0x1000, 0x2000)),
                    Some(page_range(0x5000, 0x2000)),
                ],
            ),
            (
                RemapUffdOutcome::DestructiveFailure,
                [Some(page_range(0x1000, 0x2000)), None],
            ),
            (RemapUffdOutcome::Committed, [None, None]),
        ] {
            let (mut state, source, destination) = registered_remap_state();
            let prepared = state
                .preflight_remap(
                    UffdRemapKind::Move,
                    true,
                    source.range(),
                    destination.range(),
                    |_| panic!("fully retired remap records need no survivor snapshot"),
                )
                .unwrap();
            assert!(state.plan_slots.iter().all(|slot| slot.armed.is_some()));

            state.resolve_remap(prepared, outcome);

            let mut ranges: Vec<_> = state
                .registrations
                .iter()
                .map(UffdRegistration::range)
                .collect();
            ranges.sort_by_key(|range| range.start());
            let expected: Vec<_> = expected_ranges.into_iter().flatten().collect();
            assert_eq!(ranges, expected);
            assert!(state.plan_slots.iter().all(|slot| {
                slot.armed.is_none() && slot.removed.is_empty() && slot.replacements.is_empty()
            }));
        }
    }

    #[test]
    fn duplicate_and_nonfixed_move_resolve_only_their_owned_delta() {
        for outcome in [
            RemapUffdOutcome::Preserved,
            RemapUffdOutcome::DestructiveFailure,
            RemapUffdOutcome::Committed,
        ] {
            let (mut duplicate, source, destination) = registered_remap_state();
            let prepared = duplicate
                .preflight_remap(
                    UffdRemapKind::Duplicate,
                    true,
                    source.range(),
                    destination.range(),
                    |_| panic!("fully retired destination needs no survivor snapshot"),
                )
                .unwrap();
            assert!(duplicate.plan_slots[0].armed.is_some());
            assert!(duplicate.plan_slots[1].armed.is_none());
            duplicate.resolve_remap(prepared, outcome);
            let mut duplicate_ranges: Vec<_> = duplicate
                .registrations
                .iter()
                .map(UffdRegistration::range)
                .collect();
            duplicate_ranges.sort_by_key(|range| range.start());
            let expected = if outcome == RemapUffdOutcome::Preserved {
                vec![source.range(), destination.range()]
            } else {
                vec![source.range()]
            };
            assert_eq!(duplicate_ranges, expected);
            assert!(duplicate.plan_slots.iter().all(|slot| slot.armed.is_none()));
        }

        let mut moved = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let handler = moved.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let api = initialized_api();
        let source = snapshot(2, 83, 0x1000, 0x2000, MappingKind::AnonymousPrivate);
        let destination = page_range(0x5000, 0x2000);
        let mut current = [source];
        moved
            .register_range(
                &api,
                handler,
                source.range(),
                UffdRegisterMode::MISSING,
                &mut current,
            )
            .unwrap();
        let prepared = moved
            .preflight_remap(
                UffdRemapKind::Move,
                false,
                source.range(),
                destination,
                |_| panic!("fully retired source needs no survivor snapshot"),
            )
            .unwrap();
        moved.resolve_remap(prepared, RemapUffdOutcome::DestructiveFailure);
        assert_eq!(moved.registrations.len(), 1);
        let retry = moved
            .preflight_remap(
                UffdRemapKind::Move,
                false,
                source.range(),
                destination,
                |_| panic!("fully retired source needs no survivor snapshot"),
            )
            .unwrap();
        moved.resolve_remap(retry, RemapUffdOutcome::Committed);
        assert!(moved.registrations.is_empty());

        let nonce = moved.next_plan_nonce;
        let duplicate = moved
            .preflight_remap(
                UffdRemapKind::Duplicate,
                false,
                source.range(),
                destination,
                |_| panic!("nonfixed duplicate must not inspect registrations"),
            )
            .unwrap();
        assert!(matches!(duplicate, PreparedRemapUffd::None));
        assert_eq!(moved.next_plan_nonce, nonce);
        moved.resolve_remap(duplicate, RemapUffdOutcome::Committed);
    }

    #[test]
    fn failed_second_fixed_move_alternative_releases_first_slot() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let handler = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let api = initialized_api();
        let spanning = snapshot(2, 79, 0x1000, 0x9000, MappingKind::AnonymousPrivate);
        let mut current = [spanning];
        state
            .register_range(
                &api,
                handler,
                spanning.range(),
                UffdRegisterMode::MISSING,
                &mut current,
            )
            .unwrap();
        for index in 0..(UFFD_MAX_REGISTRATIONS - 2) {
            let mapping = snapshot(
                3 + index as u64,
                80 + index as u64,
                0x10000 + index * 0x2000,
                PAGE_SIZE_4K,
                MappingKind::AnonymousPrivate,
            );
            let mut current = [mapping];
            state
                .register_range(
                    &api,
                    handler,
                    mapping.range(),
                    UffdRegisterMode::MISSING,
                    &mut current,
                )
                .unwrap();
        }
        assert_eq!(state.registrations.len(), UFFD_MAX_REGISTRATIONS - 1);
        let before: Vec<_> = state.registrations.iter().collect();

        assert!(matches!(
            state.preflight_remap(
                UffdRemapKind::Move,
                true,
                page_range(0x3000, PAGE_SIZE_4K),
                page_range(0x6000, PAGE_SIZE_4K),
                |_| Ok(spanning),
            ),
            Err(AxError::NoMemory)
        ));
        assert_eq!(state.registrations.iter().collect::<Vec<_>>(), before);
        assert!(state.plan_slots.iter().all(|slot| {
            slot.armed.is_none() && slot.removed.is_empty() && slot.replacements.is_empty()
        }));
    }

    #[test]
    fn lineage_boundary_extensions_preserve_fault_epoch() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let handler = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let api = initialized_api();
        let mapping = snapshot(2, 89, 0x2000, 0x4000, MappingKind::AnonymousPrivate);
        let tail_registration = page_range(0x3000, 0x3000);
        let mut current = [mapping];
        state
            .register_range(
                &api,
                handler,
                tail_registration,
                UffdRegisterMode::MISSING,
                &mut current,
            )
            .unwrap();

        let tail = state
            .preflight_tail_extension(
                0,
                mapping.address_space(),
                mapping.mapping(),
                mapping.range().end(),
                0x8000,
            )
            .unwrap();
        state.commit_plan(tail);
        let extended = state.registrations.iter().next().unwrap();
        assert_eq!(extended.range(), page_range(0x3000, 0x5000));
        assert_eq!(extended.generation().get(), 89);
        assert_eq!(extended.handler(), handler);

        let remove = state
            .preflight_unmap(0, page_range(0x3000, 0x5000), |_| {
                panic!("fully removed extension needs no snapshot")
            })
            .unwrap();
        state.commit_plan(remove);
        let head_registration = page_range(0x2000, 0x3000);
        let mut current = [mapping];
        state
            .register_range(
                &api,
                handler,
                head_registration,
                UffdRegisterMode::MISSING,
                &mut current,
            )
            .unwrap();
        let head = state
            .preflight_head_extension(
                0,
                mapping.address_space(),
                mapping.mapping(),
                mapping.range().start(),
                0x1000,
            )
            .unwrap();
        state.commit_plan(head);
        let extended = state.registrations.iter().next().unwrap();
        assert_eq!(extended.range(), page_range(0x1000, 0x4000));
        assert_eq!(extended.generation().get(), 89);
        assert_eq!(extended.handler(), handler);
    }

    #[test]
    fn nonboundary_growth_does_not_extend_userfaultfd_authority() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let handler = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let api = initialized_api();
        let mapping = snapshot(2, 97, 0x2000, 0x5000, MappingKind::AnonymousPrivate);
        let registered = page_range(0x3000, 0x3000);
        let mut current = [mapping];
        state
            .register_range(
                &api,
                handler,
                registered,
                UffdRegisterMode::MISSING,
                &mut current,
            )
            .unwrap();
        let nonce = state.next_plan_nonce;

        let tail = state
            .preflight_tail_extension(
                0,
                mapping.address_space(),
                mapping.mapping(),
                mapping.range().end(),
                0x8000,
            )
            .unwrap();
        assert_eq!(tail, OptionalUffdPlan::Noop);
        let head = state
            .preflight_head_extension(
                0,
                mapping.address_space(),
                mapping.mapping(),
                mapping.range().start(),
                0x1000,
            )
            .unwrap();
        assert_eq!(head, OptionalUffdPlan::Noop);
        assert_eq!(state.next_plan_nonce, nonce);
        assert_eq!(
            state.registrations.iter().next().unwrap().range(),
            registered
        );
        assert!(state.plan_slots[0].armed.is_none());
    }

    #[test]
    fn mapping_hook_noop_does_not_arm_or_consume_a_plan_slot() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let plan = state
            .preflight_unmap(0, page_range(0x1000, PAGE_SIZE_4K), |_| {
                panic!("no registration should request a current VMA")
            })
            .unwrap();
        assert_eq!(plan, OptionalUffdPlan::Noop);
        assert_eq!(state.next_plan_nonce, 1);
        assert!(state.plan_slots[0].armed.is_none());
        assert!(state.plan_slots[0].removed.is_empty());
        assert!(state.plan_slots[0].replacements.is_empty());
    }

    #[test]
    fn unchanged_protect_boundary_does_not_fragment_registration() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let handler = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let api = initialized_api();
        let mapping = snapshot(2, 31, 0x1000, 0x4000, MappingKind::AnonymousPrivate);
        let mut current = [mapping];
        state
            .register_range(
                &api,
                handler,
                mapping.range(),
                UffdRegisterMode::MISSING,
                &mut current,
            )
            .unwrap();
        let before: Vec<_> = state.registrations.iter().collect();

        let plan = state
            .preflight_protect(0, page_range(0x2000, PAGE_SIZE_4K), |registration, _| {
                assert_eq!(registration.mapping(), mapping.mapping());
                Ok(None)
            })
            .unwrap();

        assert_eq!(plan, OptionalUffdPlan::Noop);
        assert_eq!(state.next_plan_nonce, 1);
        assert_eq!(state.registrations.iter().collect::<Vec<_>>(), before);
        assert!(state.plan_slots[0].armed.is_none());
        assert!(state.plan_slots[0].removed.is_empty());
        assert!(state.plan_slots[0].replacements.is_empty());
    }

    #[test]
    fn protect_projection_rejects_partial_source_coverage_without_mutation() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let handler = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let api = initialized_api();
        let mapping = snapshot(2, 33, 0x1000, 0x4000, MappingKind::AnonymousPrivate);
        let mut current = [mapping];
        state
            .register_range(
                &api,
                handler,
                mapping.range(),
                UffdRegisterMode::MISSING,
                &mut current,
            )
            .unwrap();
        let before: Vec<_> = state.registrations.iter().collect();

        assert_eq!(
            state.preflight_protect(
                0,
                page_range(0x2000, PAGE_SIZE_4K),
                |registration, fragment| {
                    if fragment.start() == registration.range().start() {
                        Ok(None)
                    } else {
                        Ok(Some(projected_fragment(mapping, fragment)))
                    }
                },
            ),
            Err(AxError::BadState)
        );
        assert_eq!(state.registrations.iter().collect::<Vec<_>>(), before);
        assert!(state.plan_slots[0].armed.is_none());
        assert!(state.plan_slots[0].removed.is_empty());
        assert!(state.plan_slots[0].replacements.is_empty());
        assert!(state.plan_slots[0].protect_candidates.is_empty());
    }

    #[test]
    fn protect_projection_allows_unrelated_noop_and_complete_affected_source() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let handler = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let api = initialized_api();
        let affected = snapshot(2, 35, 0x1000, 0x4000, MappingKind::AnonymousPrivate);
        let unrelated = snapshot(3, 37, 0x10000, PAGE_SIZE_4K, MappingKind::AnonymousPrivate);
        for mapping in [affected, unrelated] {
            let mut current = [mapping];
            state
                .register_range(
                    &api,
                    handler,
                    mapping.range(),
                    UffdRegisterMode::MISSING,
                    &mut current,
                )
                .unwrap();
        }

        let plan = state
            .preflight_protect(
                0,
                page_range(0x2000, PAGE_SIZE_4K),
                |registration, fragment| {
                    Ok((registration.mapping() == affected.mapping())
                        .then_some(projected_fragment(affected, fragment)))
                },
            )
            .unwrap();
        assert!(matches!(plan, OptionalUffdPlan::Armed(_)));
        state.commit_plan(plan);

        let mut registrations: Vec<_> = state.registrations.iter().collect();
        registrations.sort_by_key(|registration| registration.range().start());
        assert_eq!(registrations.len(), 4);
        assert_eq!(registrations[0].range(), page_range(0x1000, PAGE_SIZE_4K));
        assert_eq!(registrations[1].range(), page_range(0x2000, PAGE_SIZE_4K));
        assert_eq!(registrations[2].range(), page_range(0x3000, 0x2000));
        assert_eq!(registrations[3].range(), unrelated.range());
        assert_eq!(registrations[3].mapping(), unrelated.mapping());
        assert_eq!(registrations[3].generation(), unrelated.generation());
    }

    #[test]
    fn aborted_unmap_plan_preserves_table_and_releases_slot_for_reuse() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let handler = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let api = initialized_api();
        let mapping = snapshot(2, 36, 0x1000, 0x4000, MappingKind::AnonymousPrivate);
        let mut current = [mapping];
        state
            .register_range(
                &api,
                handler,
                mapping.range(),
                UffdRegisterMode::MISSING,
                &mut current,
            )
            .unwrap();
        let before: Vec<_> = state.registrations.iter().collect();
        let range = page_range(0x2000, PAGE_SIZE_4K);

        let plan = state.preflight_unmap(0, range, |_| Ok(mapping)).unwrap();
        assert!(matches!(plan, OptionalUffdPlan::Armed(_)));
        state.abort_plan(plan);
        assert_eq!(state.registrations.iter().collect::<Vec<_>>(), before);
        assert!(state.plan_slots[0].armed.is_none());
        assert!(state.plan_slots[0].removed.is_empty());
        assert!(state.plan_slots[0].replacements.is_empty());

        let retry = state.preflight_unmap(0, range, |_| Ok(mapping)).unwrap();
        state.commit_plan(retry);
        let mut survivors: Vec<_> = state.registrations.iter().collect();
        survivors.sort_by_key(|registration| registration.range().start());
        assert_eq!(survivors.len(), 2);
        assert_eq!(survivors[0].range(), page_range(0x1000, PAGE_SIZE_4K));
        assert_eq!(survivors[1].range(), page_range(0x3000, 0x2000));
    }

    #[test]
    fn exhausted_nonce_clears_unarmed_payload_for_immediate_reuse() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let handler = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let api = initialized_api();
        let mapping = snapshot(2, 31, 0x1000, 0x4000, MappingKind::AnonymousPrivate);
        let mut current = [mapping];
        state
            .register_range(
                &api,
                handler,
                mapping.range(),
                UffdRegisterMode::MISSING,
                &mut current,
            )
            .unwrap();
        state.next_plan_nonce = u64::MAX;

        assert_eq!(
            state.preflight_protect(0, page_range(0x2000, PAGE_SIZE_4K), |_, fragment| {
                Ok(Some(projected_fragment(mapping, fragment)))
            }),
            Err(AxError::NoMemory)
        );
        assert!(state.plan_slots[0].armed.is_none());
        assert!(state.plan_slots[0].removed.is_empty());
        assert!(state.plan_slots[0].replacements.is_empty());

        state.next_plan_nonce = 1;
        let plan = state
            .preflight_protect(0, page_range(0x2000, PAGE_SIZE_4K), |_, fragment| {
                Ok(Some(projected_fragment(mapping, fragment)))
            })
            .unwrap();
        state.abort_plan(plan);
    }

    #[test]
    #[should_panic(expected = "stale UFFD mapping plan token reused a bounded slot")]
    fn stale_mapping_plan_token_fails_stop() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let handler = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let api = initialized_api();
        let mapping = snapshot(2, 37, 0x1000, 0x4000, MappingKind::AnonymousPrivate);
        let mut current = [mapping];
        state
            .register_range(
                &api,
                handler,
                mapping.range(),
                UffdRegisterMode::MISSING,
                &mut current,
            )
            .unwrap();
        let OptionalUffdPlan::Armed(token) = state
            .preflight_protect(0, page_range(0x2000, PAGE_SIZE_4K), |_, fragment| {
                Ok(Some(projected_fragment(mapping, fragment)))
            })
            .unwrap()
        else {
            panic!("protect split must arm a plan");
        };
        state.commit_plan(OptionalUffdPlan::Armed(UffdPlanToken {
            nonce: token.nonce + 1,
            ..token
        }));
    }

    #[test]
    fn prepared_commit_fail_stop_does_not_double_abort_during_host_unwind() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let handler = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let api = initialized_api();
        let mapping = snapshot(2, 39, 0x1000, 0x4000, MappingKind::AnonymousPrivate);
        let mut current = [mapping];
        state
            .register_range(
                &api,
                handler,
                mapping.range(),
                UffdRegisterMode::MISSING,
                &mut current,
            )
            .unwrap();
        let OptionalUffdPlan::Armed(token) = state
            .preflight_protect(0, page_range(0x2000, PAGE_SIZE_4K), |_, fragment| {
                Ok(Some(projected_fragment(mapping, fragment)))
            })
            .unwrap()
        else {
            panic!("protect split must arm a plan");
        };
        let stale = OptionalUffdPlan::Armed(UffdPlanToken {
            nonce: token.nonce + 1,
            ..token
        });

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            PreparedUffdMutation::new(&mut state, stale).commit();
        }));
        assert!(result.is_err());
        // The first invariant panic is caught without an RAII re-entry panic.
        // Continuing this deliberately corrupt state is test-only; consume
        // the original valid token to leave teardown accounting clean.
        state.abort_plan(OptionalUffdPlan::Armed(token));
    }

    #[test]
    fn detach_removes_handler_and_returns_wake_owner() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let handler = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        assert!(state.has_handlers());

        let detached = state.detach_handler(handler).unwrap();
        assert!(!detached.is_empty());
        assert!(!state.has_handlers());
        assert_eq!(state.pending(handler), Err(AxError::BadFileDescriptor));
        detached.finish();
    }

    #[test]
    fn policy_permission_gate_maps_to_eperm() {
        assert_eq!(
            uffd_policy_error(MmError::AccessDenied),
            AxError::OperationNotPermitted
        );
    }
}
