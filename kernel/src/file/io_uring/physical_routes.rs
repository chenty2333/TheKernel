//! Bounded completion routes, quarantine, and reset-owner custody.

use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PhysicalCompletionChildState {
    Empty,
    Reserved,
    Owner,
    /// A published effect whose driver report did not provide a unique,
    /// usable handle.  It remains reset/teardown custody and is never
    /// converted into an EIO CQE by the route lookup path.
    Quarantined,
}

#[derive(Clone, Copy)]
pub(super) struct PhysicalCompletionRouteChild {
    pub(super) handle: Option<u64>,
    pub(super) cookie: Option<u64>,
    pub(super) state: PhysicalCompletionChildState,
}

impl PhysicalCompletionRouteChild {
    pub(super) const fn empty() -> Self {
        Self {
            handle: None,
            cookie: None,
            state: PhysicalCompletionChildState::Empty,
        }
    }

    pub(super) const fn reserved() -> Self {
        Self {
            handle: None,
            cookie: None,
            state: PhysicalCompletionChildState::Reserved,
        }
    }

    pub(super) const fn quarantined(handle: Option<u64>, cookie: Option<u64>) -> Self {
        Self {
            handle,
            cookie,
            state: PhysicalCompletionChildState::Quarantined,
        }
    }

    pub(super) const fn owner(handle: u64, cookie: Option<u64>) -> Self {
        Self {
            handle: Some(handle),
            cookie,
            state: PhysicalCompletionChildState::Owner,
        }
    }
}

/// A single physical request group owns one ring/request identity and up to
/// sixteen lower descriptors. Keeping the Arc and request metadata here,
/// instead of in every child, makes the hot completion lookup bounded at the
/// fixed shared QD without multiplying the ownership footprint by extent
/// count.
pub(super) struct PhysicalCompletionRouteGroup {
    pub(super) ring: Option<Arc<IoUring>>,
    pub(super) request: Option<RequestId>,
    pub(super) slot: usize,
    /// Exact lower queue identity. Raw handles/cookies are only meaningful
    /// within this device namespace.
    pub(super) device_identity: usize,
    pub(super) generation: u64,
    /// A pending publication owns the logical ring/request identity but has
    /// no lower descriptor yet.  Keep it out of the child route set: handle
    /// lookup must never mistake a pending owner for a published request.
    pub(super) pending_publication: bool,
    pub(super) child_len: usize,
    pub(super) children: [PhysicalCompletionRouteChild; IO_URING_PHYSICAL_MAX_EXTENTS],
}

impl PhysicalCompletionRouteGroup {
    pub(super) fn reserved(device_identity: usize, generation: u64, child_len: usize) -> Self {
        let mut children =
            [const { PhysicalCompletionRouteChild::empty() }; IO_URING_PHYSICAL_MAX_EXTENTS];
        for child in &mut children[..child_len] {
            *child = PhysicalCompletionRouteChild::reserved();
        }
        Self {
            ring: None,
            request: None,
            slot: 0,
            device_identity,
            generation,
            pending_publication: false,
            child_len,
            children,
        }
    }

    pub(super) fn is_committed(&self) -> bool {
        self.ring.is_some() && self.request.is_some()
    }

    pub(super) fn has_custody(&self) -> bool {
        self.is_committed()
            && (self.pending_publication
                || self.children[..self.child_len].iter().any(|child| {
                    matches!(
                        child.state,
                        PhysicalCompletionChildState::Owner
                            | PhysicalCompletionChildState::Quarantined
                    )
                }))
    }

    pub(super) fn has_quarantined_child(&self) -> bool {
        self.children[..self.child_len]
            .iter()
            .any(|child| child.state == PhysicalCompletionChildState::Quarantined)
    }
}

#[derive(Clone, Copy)]
pub(super) struct QuarantinedPhysicalCompletion {
    pub(super) completion: PhysicalIoCompletion,
    pub(super) device_identity: usize,
    /// A completion observed while its handle route was not yet visible may
    /// have raced the submitter's route commit. Keep it replayable after the
    /// exact route becomes visible. Protocol failures observed for an
    /// already-owned route are diagnostic custody only and must not be fed
    /// back into the effect a second time.
    pub(super) replayable: bool,
}

/// Exact metadata for a pre-publication owner.  Pending publication is not a
/// lower route and therefore cannot be discovered through a raw completion
/// handle.  This fixed table is only metadata; the IssuedRequest and every
/// admission lease remain in the ring's matching logical slot.
pub(super) struct PhysicalCompletionPendingOwner {
    pub(super) device_identity: usize,
    pub(super) generation: u64,
    pub(super) ring: Arc<IoUring>,
    pub(super) request: RequestId,
    pub(super) slot: usize,
    pub(super) claimed: bool,
}

pub(super) struct PhysicalCompletionRouter {
    pub(super) groups: [Option<PhysicalCompletionRouteGroup>; PHYSICAL_COMPLETION_ROUTER_CAPACITY],
    pub(super) pending:
        [Option<PhysicalCompletionPendingOwner>; PHYSICAL_COMPLETION_ROUTER_CAPACITY],
    pub(super) quarantine:
        [Option<QuarantinedPhysicalCompletion>; PHYSICAL_COMPLETION_ROUTER_CAPACITY],
    pub(super) quarantine_len: usize,
    pub(super) work_count: usize,
    pub(super) pending_count: usize,
}

impl PhysicalCompletionRouter {
    pub(super) const fn new() -> Self {
        Self {
            groups: [const { None }; PHYSICAL_COMPLETION_ROUTER_CAPACITY],
            pending: [const { None }; PHYSICAL_COMPLETION_ROUTER_CAPACITY],
            quarantine: [const { None }; PHYSICAL_COMPLETION_ROUTER_CAPACITY],
            quarantine_len: 0,
            work_count: 0,
            pending_count: 0,
        }
    }
}

pub(super) fn physical_completion_device_group_capacity_available(
    router: &PhysicalCompletionRouter,
    device_identity: usize,
) -> bool {
    router
        .groups
        .iter()
        .flatten()
        .filter(|group| group.device_identity == device_identity)
        .count()
        < IO_URING_PHYSICAL_MAX_QD
        && router.groups.iter().any(Option::is_none)
}

pub(super) fn physical_completion_device_pending_capacity_available(
    router: &PhysicalCompletionRouter,
    device_identity: usize,
) -> bool {
    router
        .pending
        .iter()
        .flatten()
        .filter(|owner| owner.device_identity == device_identity)
        .count()
        < IO_URING_PHYSICAL_MAX_QD
        && router.pending.iter().any(Option::is_none)
}

pub(super) fn physical_completion_device_quarantine_capacity_available(
    router: &PhysicalCompletionRouter,
    device_identity: usize,
) -> bool {
    router
        .quarantine
        .iter()
        .flatten()
        .filter(|record| record.device_identity == device_identity)
        .count()
        < IO_URING_PHYSICAL_MAX_QD
        && router.quarantine.iter().any(Option::is_none)
}

pub(super) static PHYSICAL_COMPLETION_ROUTER: SpinNoIrq<PhysicalCompletionRouter> =
    SpinNoIrq::new(PhysicalCompletionRouter::new());
pub(super) static PHYSICAL_PUBLICATION_RETRY_CURSOR: AtomicUsize = AtomicUsize::new(0);
pub(super) const PHYSICAL_PUBLICATION_RETRY_BUDGET: usize = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PhysicalPublicationRetryDisposition {
    /// No pending owner remains and this pass did not publish new work.
    Quiescent,
    /// At least one pending owner became device-visible. Its freshly
    /// installed routes require an immediate bounded follow-up pass.
    Republished,
    /// Pending owners remain, but an existing published route owns the next
    /// real completion edge. Do not turn lower backpressure into a busy loop.
    WaitingForCompletion,
    /// Pending owners remain without any published route that can generate a
    /// future completion edge. A delayed task-context retry is the sole
    /// liveness source.
    PendingOnly,
}

pub(super) const fn physical_publication_retry_disposition(
    republished: bool,
    pending_remaining: bool,
    published_routes_remaining: bool,
) -> PhysicalPublicationRetryDisposition {
    if republished {
        PhysicalPublicationRetryDisposition::Republished
    } else if !pending_remaining {
        PhysicalPublicationRetryDisposition::Quiescent
    } else if published_routes_remaining {
        PhysicalPublicationRetryDisposition::WaitingForCompletion
    } else {
        PhysicalPublicationRetryDisposition::PendingOnly
    }
}

pub(super) fn register_physical_completion_pending_owner(
    ring: &Arc<IoUring>,
    request: RequestId,
    slot: usize,
    device_identity: usize,
    generation: u64,
) -> AxResult<()> {
    let mut router = PHYSICAL_COMPLETION_ROUTER.lock();
    if router.pending.iter().flatten().any(|owner| {
        owner.device_identity == device_identity
            && owner.generation == generation
            && owner.request == request
            && owner.slot == slot
            && Arc::ptr_eq(&owner.ring, ring)
    }) {
        return Err(AxError::BadState);
    }
    if !physical_completion_device_pending_capacity_available(&router, device_identity) {
        return Err(AxError::ResourceBusy);
    }
    let Some(entry) = router.pending.iter_mut().find(|entry| entry.is_none()) else {
        return Err(AxError::ResourceBusy);
    };
    *entry = Some(PhysicalCompletionPendingOwner {
        device_identity,
        generation,
        ring: Arc::clone(ring),
        request,
        slot,
        claimed: false,
    });
    router.pending_count = router
        .pending_count
        .checked_add(1)
        .ok_or(AxError::BadState)?;
    Ok(())
}

pub(super) fn clear_physical_completion_pending_owner(
    ring: &Arc<IoUring>,
    request: RequestId,
    slot: usize,
    device_identity: usize,
    generation: u64,
) -> bool {
    let mut router = PHYSICAL_COMPLETION_ROUTER.lock();
    clear_physical_completion_pending_owner_locked(
        &mut router,
        ring,
        request,
        slot,
        device_identity,
        generation,
    )
}

pub(super) fn set_physical_completion_pending_claim(
    ring: &Arc<IoUring>,
    request: RequestId,
    slot: usize,
    device_identity: usize,
    generation: u64,
    claimed: bool,
) -> AxResult<()> {
    let mut router = PHYSICAL_COMPLETION_ROUTER.lock();
    let Some(owner) = router.pending.iter_mut().flatten().find(|owner| {
        owner.device_identity == device_identity
            && owner.generation == generation
            && owner.request == request
            && owner.slot == slot
            && Arc::ptr_eq(&owner.ring, ring)
    }) else {
        return Err(AxError::BadState);
    };
    if owner.claimed && claimed {
        return Err(AxError::ResourceBusy);
    }
    owner.claimed = claimed;
    Ok(())
}

pub(super) fn clear_physical_completion_pending_owner_locked(
    router: &mut PhysicalCompletionRouter,
    ring: &Arc<IoUring>,
    request: RequestId,
    slot: usize,
    device_identity: usize,
    generation: u64,
) -> bool {
    let Some(index) = router.pending.iter().position(|entry| {
        entry.as_ref().is_some_and(|owner| {
            owner.device_identity == device_identity
                && owner.generation == generation
                && owner.request == request
                && owner.slot == slot
                && Arc::ptr_eq(&owner.ring, ring)
        })
    }) else {
        return false;
    };
    router.pending[index] = None;
    router.pending_count = router.pending_count.saturating_sub(1);
    true
}

pub(super) fn physical_completion_pending_owner_snapshot(
    index: usize,
) -> Option<(Arc<IoUring>, RequestId, usize, usize, u64)> {
    let router = PHYSICAL_COMPLETION_ROUTER.lock();
    let owner = router.pending.get(index)?.as_ref()?;
    Some((
        Arc::clone(&owner.ring),
        owner.request,
        owner.slot,
        owner.device_identity,
        owner.generation,
    ))
}

pub(super) fn physical_completion_pending_owner_count_for_device(device_identity: usize) -> usize {
    PHYSICAL_COMPLETION_ROUTER
        .lock()
        .pending
        .iter()
        .flatten()
        .filter(|owner| owner.device_identity == device_identity)
        .count()
}

/// A route reservation is made before the vendor effect is published.  One
/// reservation occupies exactly one fixed request group, regardless of its
/// extent count. It is deliberately separate from the per-ring QD
/// reservation: completion ownership is device-global, so two rings must
/// compete for the same fixed group table for that device even when each ring
/// still has local QD credit. Different devices receive independent bounded
/// namespaces so one fail-closed quarantine cannot consume a sibling's
/// admission capacity.
pub(super) struct PhysicalCompletionRouteReservation {
    pub(super) group: usize,
    pub(super) len: usize,
    pub(super) device_identity: usize,
    pub(super) work_reserved: bool,
    pub(super) committed: bool,
}

impl PhysicalCompletionRouteReservation {
    pub(super) fn new(count: usize) -> AxResult<Self> {
        Self::new_for_device(count, physical_completion_default_identity())
    }

    pub(super) fn new_for_device(count: usize, device_identity: usize) -> AxResult<Self> {
        if count == 0 || count > IO_URING_PHYSICAL_MAX_EXTENTS {
            return Err(AxError::BadState);
        }
        let mut router = PHYSICAL_COMPLETION_ROUTER.lock();
        if !physical_completion_device_group_capacity_available(&router, device_identity) {
            return Err(AxError::ResourceBusy);
        }
        let Some(group) = router.groups.iter().position(Option::is_none) else {
            return Err(AxError::ResourceBusy);
        };
        let generation =
            physical_completion_generation_for(device_identity).ok_or(AxError::BadState)?;
        router.groups[group] = Some(PhysicalCompletionRouteGroup::reserved(
            device_identity,
            generation,
            count,
        ));
        router.work_count += 1;
        Ok(Self {
            group,
            len: count,
            device_identity,
            work_reserved: true,
            committed: false,
        })
    }

    pub(super) fn activate_locked(
        &mut self,
        router: &mut PhysicalCompletionRouter,
        ring: &Arc<IoUring>,
        request: RequestId,
        slot: usize,
        publication: Option<PhysicalIoPublication>,
    ) -> bool {
        let mut handles = [None; IO_URING_PHYSICAL_MAX_EXTENTS];
        let mut cookies = [None; IO_URING_PHYSICAL_MAX_EXTENTS];
        let force_quarantine = publication.is_none_or(|publication| publication.count() == 0);
        let accepted = publication.map_or(0, |publication| {
            let count = publication.count();
            for index in 0..count.min(IO_URING_PHYSICAL_MAX_EXTENTS) {
                handles[index] = publication.handle(index);
                cookies[index] = publication.cookie(index);
            }
            count
        });
        self.activate_handles_locked(
            router,
            ring,
            request,
            slot,
            &handles,
            &cookies,
            accepted,
            force_quarantine,
        )
    }

    /// Installs only the handles that the lower publication actually accepted.
    /// A terminal short batch is still a valid publication for its accepted
    /// prefix; every reserved suffix is a pre-publication rollback and must
    /// not become reset custody.  A missing/duplicate handle in the accepted
    /// prefix remains quarantine custody because it cannot be retired by an
    /// exact completion.
    pub(super) fn activate_handles_locked(
        &mut self,
        router: &mut PhysicalCompletionRouter,
        ring: &Arc<IoUring>,
        request: RequestId,
        slot: usize,
        handles: &[Option<u64>; IO_URING_PHYSICAL_MAX_EXTENTS],
        cookies: &[Option<u64>; IO_URING_PHYSICAL_MAX_EXTENTS],
        accepted: usize,
        force_quarantine: bool,
    ) -> bool {
        let mut quarantined = force_quarantine || accepted > self.len;
        if !quarantined {
            for index in 0..accepted {
                let Some(handle) = handles[index] else {
                    quarantined = true;
                    break;
                };
                if handle == 0
                    || handles[..index]
                        .iter()
                        .flatten()
                        .any(|existing| *existing == handle)
                    || router
                        .groups
                        .iter()
                        .enumerate()
                        .any(|(group_index, group)| {
                            group_index != self.group
                                && group.as_ref().is_some_and(|group| {
                                    group.device_identity == self.device_identity
                                        && group.children[..group.child_len].iter().any(|child| {
                                            matches!(
                                                child.state,
                                                PhysicalCompletionChildState::Owner
                                                    | PhysicalCompletionChildState::Quarantined
                                            ) && child.handle == Some(handle)
                                        })
                                })
                        })
                {
                    quarantined = true;
                    break;
                }
            }
        }

        let Some(group) = router.groups.get_mut(self.group).and_then(Option::as_mut) else {
            // A reservation must normally remain installed until this commit
            // point.  If an internal owner was already removed, preserve the
            // fail-stop result rather than exposing a route without a group.
            self.committed = true;
            self.work_reserved = false;
            return true;
        };
        group.ring = Some(Arc::clone(ring));
        group.request = Some(request);
        group.slot = slot;
        if quarantined {
            // Any malformed accepted prefix makes the complete reserved
            // group reset custody.  The lower owner may have accepted an
            // extent beyond the reported prefix, so retaining every reserved
            // child is safer than releasing a suffix whose ownership is
            // ambiguous.  A valid short prefix is handled below and alone
            // releases its never-published suffix.
            for index in 0..self.len {
                let handle = (index < accepted).then_some(handles[index]).flatten();
                let cookie = (index < accepted).then_some(cookies[index]).flatten();
                group.children[index] = PhysicalCompletionRouteChild::quarantined(handle, cookie);
            }
        } else {
            for index in 0..self.len {
                group.children[index] = if index < accepted {
                    PhysicalCompletionRouteChild::owner(
                        handles[index].expect("validated physical completion handle"),
                        cookies[index],
                    )
                } else {
                    // The lower device never owned this suffix.  Roll its
                    // upper child reservation back exactly as a legal short
                    // publication; the operation's one group charge remains
                    // for the accepted prefix until it settles.
                    PhysicalCompletionRouteChild::empty()
                };
            }
        }
        self.committed = true;
        self.work_reserved = false;
        quarantined
    }

    // legacy implementation removed
    // group.children[..group.child_len].iter().any(|child| {
    // matches!(
    // child.state,
    // PhysicalCompletionChildState::Owner
    // | PhysicalCompletionChildState::Quarantined
    // ) && child.handle == Some(handle)
    // })
    // })

    pub(super) fn activate(
        mut self,
        ring: &Arc<IoUring>,
        request: RequestId,
        slot: usize,
        publication: Option<PhysicalIoPublication>,
    ) {
        let quarantined = self.activate_locked(
            &mut PHYSICAL_COMPLETION_ROUTER.lock(),
            ring,
            request,
            slot,
            publication,
        );
        if quarantined {
            record_io_uring_physical_quarantine();
        }
    }

    #[cfg(test)]
    pub(super) fn activate_test(
        self,
        ring: &Arc<IoUring>,
        request: RequestId,
        slot: usize,
        handle: u64,
    ) {
        self.activate_test_with_cookie(ring, request, slot, handle, None);
    }

    #[cfg(test)]
    pub(super) fn activate_test_with_cookie(
        mut self,
        ring: &Arc<IoUring>,
        request: RequestId,
        slot: usize,
        handle: u64,
        cookie: Option<u64>,
    ) {
        let mut handles = [None; IO_URING_PHYSICAL_MAX_EXTENTS];
        let mut cookies = [None; IO_URING_PHYSICAL_MAX_EXTENTS];
        handles[0] = Some(handle);
        cookies[0] = cookie;
        let quarantined = self.activate_handles_locked(
            &mut PHYSICAL_COMPLETION_ROUTER.lock(),
            ring,
            request,
            slot,
            &handles,
            &cookies,
            1,
            false,
        );
        if quarantined {
            record_io_uring_physical_quarantine();
        }
    }

    #[cfg(test)]
    pub(super) fn activate_test_with_handles(
        mut self,
        ring: &Arc<IoUring>,
        request: RequestId,
        slot: usize,
        handles: &[Option<u64>],
    ) -> bool {
        let mut accepted_handles = [None; IO_URING_PHYSICAL_MAX_EXTENTS];
        let accepted = handles.len();
        let copied = accepted.min(accepted_handles.len());
        accepted_handles[..copied].copy_from_slice(&handles[..copied]);
        let cookies = [None; IO_URING_PHYSICAL_MAX_EXTENTS];
        let quarantined = self.activate_handles_locked(
            &mut PHYSICAL_COMPLETION_ROUTER.lock(),
            ring,
            request,
            slot,
            &accepted_handles,
            &cookies,
            accepted,
            false,
        );
        if quarantined {
            record_io_uring_physical_quarantine();
        }
        quarantined
    }
}

impl Drop for PhysicalCompletionRouteReservation {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let mut router = PHYSICAL_COMPLETION_ROUTER.lock();
        if self.work_reserved {
            router.groups[self.group] = None;
            router.work_count = router.work_count.saturating_sub(1);
        }
    }
}

pub(super) fn physical_completion_route_count() -> usize {
    physical_completion_route_count_for_device(physical_completion_default_identity())
}

pub(super) fn physical_completion_route_count_for_device(device_identity: usize) -> usize {
    let router = PHYSICAL_COMPLETION_ROUTER.lock();
    router
        .groups
        .iter()
        .flatten()
        .filter(|group| group.device_identity == device_identity)
        .map(|group| {
            group.children[..group.child_len]
                .iter()
                .filter(|child| child.state == PhysicalCompletionChildState::Owner)
                .count()
        })
        .sum()
}

pub(super) fn physical_completion_custody_count() -> usize {
    physical_completion_custody_count_for_device(physical_completion_default_identity())
}

pub(super) fn physical_completion_custody_count_for_device(device_identity: usize) -> usize {
    let router = PHYSICAL_COMPLETION_ROUTER.lock();
    router
        .groups
        .iter()
        .flatten()
        .filter(|group| group.device_identity == device_identity)
        .filter(|group| group.has_custody())
        .count()
        + router
            .pending
            .iter()
            .flatten()
            .filter(|owner| owner.device_identity == device_identity)
            .count()
}

pub(super) fn physical_completion_has_quarantined_route() -> bool {
    physical_completion_has_quarantined_route_for_device(physical_completion_default_identity())
}

pub(super) fn physical_completion_has_quarantined_route_for_device(device_identity: usize) -> bool {
    PHYSICAL_COMPLETION_ROUTER
        .lock()
        .groups
        .iter()
        .flatten()
        .filter(|group| group.device_identity == device_identity)
        .any(PhysicalCompletionRouteGroup::has_quarantined_child)
}

pub(super) fn lookup_physical_completion_route_for_device(
    device_identity: usize,
    handle: u64,
) -> Option<(Arc<IoUring>, usize)> {
    let router = PHYSICAL_COMPLETION_ROUTER.lock();
    for group in router.groups.iter().flatten() {
        if group.ring.is_none() || group.device_identity != device_identity {
            continue;
        }
        if group.children[..group.child_len].iter().any(|child| {
            child.state == PhysicalCompletionChildState::Owner && child.handle == Some(handle)
        }) {
            return Some((
                Arc::clone(group.ring.as_ref().expect("committed route ring")),
                group.slot,
            ));
        }
    }
    None
}

pub(super) fn lookup_physical_completion_route(handle: u64) -> Option<(Arc<IoUring>, usize)> {
    lookup_physical_completion_route_for_device(physical_completion_default_identity(), handle)
}

pub(super) fn lookup_physical_completion_route_identity(
    handle: u64,
) -> Option<(Arc<IoUring>, RequestId, u64)> {
    lookup_physical_completion_route_identity_for_device(
        physical_completion_default_identity(),
        handle,
    )
    .map(|(ring, request, generation, _slot)| (ring, request, generation))
}

pub(super) fn lookup_physical_completion_route_identity_for_device(
    device_identity: usize,
    handle: u64,
) -> Option<(Arc<IoUring>, RequestId, u64, usize)> {
    let router = PHYSICAL_COMPLETION_ROUTER.lock();
    for group in router.groups.iter().flatten() {
        let (Some(ring), Some(request)) = (group.ring.as_ref(), group.request) else {
            continue;
        };
        if group.device_identity != device_identity {
            continue;
        }
        if group.children[..group.child_len].iter().any(|child| {
            child.state == PhysicalCompletionChildState::Owner && child.handle == Some(handle)
        }) {
            return Some((Arc::clone(ring), request, group.generation, group.slot));
        }
    }
    None
}

/// Releases the complete route set for one exact request only after its
/// completion handle has matched.  Ring/worker slots are deliberately not an
/// identity: a stale completion can race a new request that reuses both.
pub(super) fn release_physical_completion_routes(
    ring: &Arc<IoUring>,
    request: RequestId,
    handle: Option<u64>,
) -> bool {
    release_physical_completion_routes_for_device(
        physical_completion_default_identity(),
        ring,
        request,
        handle,
    )
}

pub(super) fn release_physical_completion_routes_for_device(
    device_identity: usize,
    ring: &Arc<IoUring>,
    request: RequestId,
    handle: Option<u64>,
) -> bool {
    let mut router = PHYSICAL_COMPLETION_ROUTER.lock();
    // Require an exact handle match before clearing the request's complete
    // extent set.  In particular, an old cleanup with a reused worker slot
    // cannot observe a different generation and decrement its QD charge.
    let Some(group_index) = router.groups.iter().position(|group| {
        let Some(group) = group.as_ref() else {
            return false;
        };
        let (Some(owner), Some(route_request)) = (group.ring.as_ref(), group.request) else {
            return false;
        };
        if group.device_identity != device_identity {
            return false;
        }
        Arc::ptr_eq(owner, ring)
            && route_request == request
            && group.children[..group.child_len].iter().any(|child| {
                matches!(
                    child.state,
                    PhysicalCompletionChildState::Owner | PhysicalCompletionChildState::Quarantined
                ) && handle.is_none_or(|expected| child.handle == Some(expected))
            })
    }) else {
        return false;
    };
    // The matching route proves that this request still owns one global work
    // charge.  Refuse a corrupted zero count before clearing the routes;
    // saturating here would hide a double release and strand the remaining
    // reset/close accounting.
    if router.work_count == 0 {
        return false;
    }
    router.groups[group_index] = None;
    router.work_count -= 1;
    true
}

pub(super) struct PhysicalCompletionResetOwner {
    pub(super) device_identity: usize,
    pub(super) ring: Arc<IoUring>,
    pub(super) request: RequestId,
    pub(super) slot: usize,
    pub(super) generation: u64,
}

pub(super) struct PhysicalCompletionResetOwners {
    pub(super) owners: [Option<PhysicalCompletionResetOwner>; IO_URING_PHYSICAL_MAX_QD],
    pub(super) len: usize,
}

impl PhysicalCompletionResetOwners {
    pub(super) const fn new() -> Self {
        Self {
            owners: [const { None }; IO_URING_PHYSICAL_MAX_QD],
            len: 0,
        }
    }
}

pub(super) struct PhysicalCompletionResetWork {
    pub(super) owner: PhysicalCompletionResetOwner,
    pub(super) work: PhysicalIoWork,
}

pub(super) struct PhysicalCompletionResetWorks {
    pub(super) works: [Option<PhysicalCompletionResetWork>; IO_URING_PHYSICAL_MAX_QD],
    pub(super) len: usize,
}

impl PhysicalCompletionResetWorks {
    pub(super) const fn new() -> Self {
        Self {
            works: [const { None }; IO_URING_PHYSICAL_MAX_QD],
            len: 0,
        }
    }
}

/// Captures one exact ring/request identity for every published route that
/// must be retired by a global device reset.  The storage is fixed-capacity:
/// after the lower reset proves `Quiesced`, retirement cannot fail because a
/// recovery allocation ran out of memory.
pub(super) fn collect_physical_completion_owners_for_device(
    device_identity: usize,
) -> AxResult<PhysicalCompletionResetOwners> {
    let router = PHYSICAL_COMPLETION_ROUTER.lock();
    let mut owners = PhysicalCompletionResetOwners::new();
    for group in router.groups.iter().flatten() {
        if group.device_identity != device_identity {
            continue;
        }
        let (Some(ring), Some(request)) = (group.ring.as_ref(), group.request) else {
            continue;
        };
        if !group.has_custody() {
            continue;
        }
        if owners.len == IO_URING_PHYSICAL_MAX_QD {
            return Err(AxError::BadState);
        }
        owners.owners[owners.len] = Some(PhysicalCompletionResetOwner {
            device_identity,
            ring: Arc::clone(ring),
            request,
            slot: group.slot,
            generation: group.generation,
        });
        owners.len += 1;
    }
    for pending in router.pending.iter().flatten() {
        if pending.device_identity != device_identity {
            continue;
        }
        if owners.len == IO_URING_PHYSICAL_MAX_QD {
            return Err(AxError::BadState);
        }
        owners.owners[owners.len] = Some(PhysicalCompletionResetOwner {
            device_identity,
            ring: Arc::clone(&pending.ring),
            request: pending.request,
            slot: pending.slot,
            generation: pending.generation,
        });
        owners.len += 1;
    }
    Ok(owners)
}

pub(super) fn collect_physical_completion_owners() -> AxResult<PhysicalCompletionResetOwners> {
    collect_physical_completion_owners_for_device(physical_completion_default_identity())
}

pub(super) fn clear_physical_completion_quarantine() {
    let mut router = PHYSICAL_COMPLETION_ROUTER.lock();
    router.quarantine.fill(None);
    router.quarantine_len = 0;
}

pub(super) fn clear_physical_completion_quarantine_for_device(device_identity: usize) {
    let mut router = PHYSICAL_COMPLETION_ROUTER.lock();
    let mut removed = 0;
    for index in 0..router.quarantine.len() {
        if router.quarantine[index]
            .as_ref()
            .is_some_and(|entry| entry.device_identity == device_identity)
        {
            router.quarantine[index] = None;
            removed += 1;
        }
    }
    router.quarantine_len = router.quarantine_len.saturating_sub(removed);
}

pub(super) fn route_matches_reset_owner(
    group: &PhysicalCompletionRouteGroup,
    owner: &PhysicalCompletionResetOwner,
) -> bool {
    group.has_custody()
        && group.device_identity == owner.device_identity
        && group.request == Some(owner.request)
        && group.generation == owner.generation
        && group
            .ring
            .as_ref()
            .is_some_and(|ring| Arc::ptr_eq(ring, &owner.ring))
}

pub(super) fn pending_matches_reset_owner(
    pending: &PhysicalCompletionPendingOwner,
    owner: &PhysicalCompletionResetOwner,
) -> bool {
    pending.device_identity == owner.device_identity
        && pending.generation == owner.generation
        && pending.request == owner.request
        && pending.slot == owner.slot
        && Arc::ptr_eq(&pending.ring, &owner.ring)
}

/// Removes every route for the reset owner set as one router transaction.
/// All identities are validated before any route is cleared, so a malformed
/// owner set cannot partially decrement the global work charge.
pub(super) fn release_physical_completion_owner_set(
    owners: &PhysicalCompletionResetOwners,
) -> AxResult<()> {
    let mut router = PHYSICAL_COMPLETION_ROUTER.lock();
    let mut group_releases = 0usize;
    let mut pending_releases = 0usize;
    for owner in owners.owners[..owners.len].iter().flatten() {
        let group_match = router
            .groups
            .iter()
            .flatten()
            .any(|group| route_matches_reset_owner(group, owner));
        let pending_match = router
            .pending
            .iter()
            .flatten()
            .any(|pending| pending_matches_reset_owner(pending, owner));
        if !group_match && !pending_match {
            return Err(AxError::BadState);
        }
        group_releases += usize::from(group_match);
        pending_releases += usize::from(pending_match);
    }
    if router.work_count < group_releases || router.pending_count < pending_releases {
        return Err(AxError::BadState);
    }
    for group in &mut router.groups {
        if group.as_ref().is_some_and(|group| {
            owners.owners[..owners.len]
                .iter()
                .flatten()
                .any(|owner| route_matches_reset_owner(group, owner))
        }) {
            *group = None;
        }
    }
    for pending in &mut router.pending {
        if pending.as_ref().is_some_and(|pending| {
            owners.owners[..owners.len]
                .iter()
                .flatten()
                .any(|owner| pending_matches_reset_owner(pending, owner))
        }) {
            *pending = None;
        }
    }
    router.work_count -= group_releases;
    router.pending_count -= pending_releases;
    Ok(())
}

pub(super) fn restore_physical_completion_reset_works(mut works: PhysicalCompletionResetWorks) {
    for entry in works.works[..works.len].iter_mut() {
        let Some(entry) = entry.take() else {
            continue;
        };
        let _ = entry.owner.ring.retain_physical_worker_work(entry.work);
    }
}

/// Completes the upper reset protocol only after the lower device has
/// returned a quiescent outcome (`Quiesced` or `Retired`).  Every route and
/// ring work owner is still held while reset runs; once quiescence is proven,
/// each exact request receives a typed reset failure and its route/work charge
/// is released exactly once.
pub(super) fn retire_physical_completion_after_reset_for_device(
    device_identity: usize,
    outcome: axdriver::prelude::BlockResetOutcome,
) -> AxResult<()> {
    let proof = PhysicalIoResetProof::from_lower_reset(outcome).ok_or(AxError::BadState)?;
    let owners = collect_physical_completion_owners_for_device(device_identity)?;
    // Validate the route/work pairing before mutating either table.  A
    // missing ring owner is an internal custody violation; leaving routes in
    // place is safer than decrementing the global charge and stranding the
    // unmatched ring work on a retry.
    let mut works = PhysicalCompletionResetWorks::new();
    for owner in owners.owners[..owners.len].iter().flatten() {
        if !owner.ring.has_physical_worker_request_for_device(
            device_identity,
            owner.request,
            owner.slot,
            owner.generation,
        ) {
            restore_physical_completion_reset_works(works);
            return Err(AxError::BadState);
        }
        let Some(work) = owner.ring.take_physical_worker_for_reset_for_device(
            device_identity,
            owner.request,
            owner.slot,
            owner.generation,
        ) else {
            restore_physical_completion_reset_works(works);
            return Err(AxError::BadState);
        };
        works.works[works.len] = Some(PhysicalCompletionResetWork {
            owner: PhysicalCompletionResetOwner {
                device_identity,
                ring: Arc::clone(&owner.ring),
                request: owner.request,
                slot: owner.slot,
                generation: owner.generation,
            },
            work,
        });
        works.len += 1;
    }
    if let Err(error) = release_physical_completion_owner_set(&owners) {
        restore_physical_completion_reset_works(works);
        return Err(error);
    }
    for entry in works.works[..works.len].iter_mut() {
        let Some(entry) = entry.take() else {
            continue;
        };
        entry
            .owner
            .ring
            .finish_physical_worker_after_reset(entry.work, proof);
    }
    clear_physical_completion_quarantine_for_device(device_identity);
    if physical_completion_work_count_for_device(device_identity) == 0 {
        Ok(())
    } else {
        Err(AxError::BadState)
    }
}

pub(super) fn retire_physical_completion_after_reset(
    outcome: axdriver::prelude::BlockResetOutcome,
) -> AxResult<()> {
    retire_physical_completion_after_reset_for_device(
        physical_completion_default_identity(),
        outcome,
    )
}

pub(super) fn quarantine_physical_completion_routes(
    ring: &Arc<IoUring>,
    request: RequestId,
    handle: Option<u64>,
) {
    quarantine_physical_completion_routes_for_device(
        physical_completion_default_identity(),
        ring,
        request,
        handle,
    )
}

pub(super) fn quarantine_physical_completion_routes_for_device(
    device_identity: usize,
    ring: &Arc<IoUring>,
    request: RequestId,
    handle: Option<u64>,
) {
    let mut router = PHYSICAL_COMPLETION_ROUTER.lock();
    for group in router.groups.iter_mut().flatten() {
        let Some(owner) = group.ring.as_ref() else {
            continue;
        };
        if group.device_identity != device_identity
            || group.request != Some(request)
            || !Arc::ptr_eq(owner, ring)
        {
            continue;
        }
        for child in &mut group.children[..group.child_len] {
            if child.state == PhysicalCompletionChildState::Owner
                && handle.is_none_or(|expected| child.handle == Some(expected))
            {
                child.state = PhysicalCompletionChildState::Quarantined;
            }
        }
    }
}

pub(super) fn quarantine_physical_completion(
    completion: PhysicalIoCompletion,
    replayable: bool,
) -> AxResult<()> {
    quarantine_physical_completion_for_device(
        physical_completion_default_identity(),
        completion,
        replayable,
    )
}

pub(super) fn quarantine_physical_completion_for_device(
    device_identity: usize,
    completion: PhysicalIoCompletion,
    replayable: bool,
) -> AxResult<()> {
    let mut router = PHYSICAL_COMPLETION_ROUTER.lock();
    if !physical_completion_device_quarantine_capacity_available(&router, device_identity) {
        // The effect route remains installed and its owner remains pinned;
        // callers must take a typed reset/quarantine path instead of turning
        // this bounded-storage failure into an I/O error or fallback.
        return Err(AxError::ResourceBusy);
    }
    let Some(index) = router.quarantine.iter().position(Option::is_none) else {
        // The effect route remains installed and its owner remains pinned;
        // callers must take a typed reset/quarantine path instead of turning
        // this bounded-storage failure into an I/O error or fallback.
        return Err(AxError::ResourceBusy);
    };
    router.quarantine[index] = Some(QuarantinedPhysicalCompletion {
        completion,
        device_identity,
        replayable,
    });
    router.quarantine_len += 1;
    // A completion that beats the Reserved -> Owner route commit is an
    // expected publication race, not a safety quarantine.  Keep it in the
    // same bounded custody slab for exact handle/cookie replay, but reserve
    // the externally visible quarantine counter for observations that can no
    // longer be replayed safely.
    if !replayable {
        record_io_uring_physical_quarantine();
    }
    Ok(())
}

/// Removes only completion records that were observed before their exact
/// route became visible. Protocol failures from an already-owned route stay
/// in bounded custody and are never replayed into the effect a second time.
pub(super) fn take_replayable_physical_completions(output: &mut [PhysicalIoCompletion]) -> usize {
    take_replayable_physical_completions_for_device(physical_completion_default_identity(), output)
}

pub(super) fn take_replayable_physical_completions_for_device(
    device_identity: usize,
    output: &mut [PhysicalIoCompletion],
) -> usize {
    let mut router = PHYSICAL_COMPLETION_ROUTER.lock();
    let mut count = 0;
    for index in 0..router.quarantine.len() {
        if count == output.len() {
            break;
        }
        let Some(record) = router.quarantine[index] else {
            continue;
        };
        if !record.replayable {
            continue;
        }
        if record.device_identity != device_identity {
            continue;
        }
        let mut matches_owner = false;
        let mut wrong_cookie_owner = false;
        for group in router.groups.iter_mut().flatten() {
            if group.device_identity != device_identity {
                continue;
            }
            for child in &mut group.children[..group.child_len] {
                if child.state != PhysicalCompletionChildState::Owner
                    || child.handle != Some(record.completion.handle)
                {
                    continue;
                }
                if child
                    .cookie
                    .is_none_or(|expected| expected == record.completion.cookie)
                {
                    matches_owner = true;
                    continue;
                }
                // The raw handle was reused with a different publication
                // cookie. Retain the stale record as diagnostic custody and
                // quarantine the new child so a lower reset, not a replay,
                // resolves the ABA.
                wrong_cookie_owner = true;
                child.state = PhysicalCompletionChildState::Quarantined;
            }
        }
        if matches_owner {
            output[count] = record.completion;
            count += 1;
            router.quarantine[index] = None;
            router.quarantine_len = router.quarantine_len.saturating_sub(1);
        } else if wrong_cookie_owner
            && let Some(record) = router.quarantine[index].as_mut()
            && record.replayable
        {
            record.replayable = false;
            // The apparent early completion has now been proven to
            // belong to an older publication of the reused raw
            // handle.  Count the transition exactly once when its
            // cookie mismatch turns replay custody into fail-stop
            // quarantine.
            record_io_uring_physical_quarantine();
        }
    }
    count
}

#[inline]
pub(super) fn retained_completion_needs_quarantine(reason: PhysicalIoPendingReason) -> bool {
    !matches!(reason, PhysicalIoPendingReason::MissingCompletion { .. })
}

pub(super) fn route_physical_completion(
    completion: PhysicalIoCompletion,
) -> AxResult<PhysicalIoCompletionDisposition> {
    route_physical_completion_for_device(physical_completion_default_identity(), completion)
}

pub(super) fn route_physical_completion_for_device(
    device_identity: usize,
    completion: PhysicalIoCompletion,
) -> AxResult<PhysicalIoCompletionDisposition> {
    let Some((ring, request, generation, slot)) =
        lookup_physical_completion_route_identity_for_device(device_identity, completion.handle)
    else {
        // This may be a completion racing the submitter's Reserved -> Owner
        // route commit. Keep it replayable; a later task-context pass will
        // match the exact handle after the owner is atomically installed.
        quarantine_physical_completion_for_device(device_identity, completion, true)?;
        return Ok(PhysicalIoCompletionDisposition::Unknown);
    };
    if generation != physical_completion_generation_for(device_identity).unwrap_or(0) {
        // The lower transport generation changed after publication. Keep the
        // ring/work owner in reset custody; a stale late IRQ is never allowed
        // to settle a handle that a replacement transport may reuse.
        quarantine_physical_completion_routes_for_device(
            device_identity,
            &ring,
            request,
            Some(completion.handle),
        );
        quarantine_physical_completion_for_device(device_identity, completion, false)?;
        return Ok(PhysicalIoCompletionDisposition::Unknown);
    }
    ring.consume_physical_completion_for_device_at_slot(device_identity, slot, completion)
}

pub(super) fn map_block_completion_error(error: DevError) -> AxError {
    match error {
        DevError::AlreadyExists => AxError::AlreadyExists,
        DevError::Again => AxError::WouldBlock,
        DevError::BadState => AxError::BadState,
        DevError::InvalidParam => AxError::InvalidInput,
        DevError::Io => AxError::Io,
        DevError::NoMemory => AxError::NoMemory,
        DevError::ResourceBusy => AxError::ResourceBusy,
        DevError::Unsupported => AxError::OperationNotSupported,
    }
}

pub(super) fn convert_block_completion(
    completion: BlockCompletion,
) -> AxResult<PhysicalIoCompletion> {
    if completion.owner != BlockCompletionOwner::Physical
        || completion.handle.raw == 0
        || completion.cookie == 0
    {
        // A lower owner/type violation is a reset/quarantine condition, never
        // a synthetic EIO completion. Published effect owners stay installed
        // until an explicit device reset proves quiescence.
        return Err(AxError::BadState);
    }
    let success = match completion.status {
        BlockCompletionStatus::Success => true,
        BlockCompletionStatus::DeviceError(_) => false,
        BlockCompletionStatus::Quarantined => return Err(AxError::BadState),
    };
    Ok(PhysicalIoCompletion {
        handle: completion.handle.raw,
        cookie: completion.cookie,
        bytes: completion.bytes as usize,
        success,
    })
}

/// Keeps every lower record that was removed before a generation/type error
/// became visible.  Returning the first typed error makes the caller enter
/// the device reset path, while the bounded upper quarantine retains exact
/// handle/cookie custody instead of silently losing an earlier record in the
/// same batch.
pub(super) fn quarantine_drained_block_completions(
    records: &[BlockCompletion],
    count: usize,
) -> AxError {
    quarantine_drained_block_completions_for_device(
        physical_completion_default_identity(),
        records,
        count,
    )
}

pub(super) fn quarantine_drained_block_completions_for_device(
    device_identity: usize,
    records: &[BlockCompletion],
    count: usize,
) -> AxError {
    let mut first_error = None;
    for record in records.iter().copied().take(count.min(records.len())) {
        match convert_block_completion(record) {
            Ok(completion) => {
                if let Err(error) =
                    quarantine_physical_completion_for_device(device_identity, completion, false)
                {
                    first_error.get_or_insert(error);
                }
            }
            Err(error) => {
                first_error.get_or_insert(error);
                record_io_uring_physical_quarantine();
            }
        }
    }
    first_error.unwrap_or(AxError::BadState)
}
