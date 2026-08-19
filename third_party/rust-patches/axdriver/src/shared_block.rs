use alloc::{
    string::{String, ToString},
    sync::Arc,
    vec,
    vec::Vec,
};
use core::{
    cmp::min,
    hint::spin_loop,
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    time::Duration,
};

use axsync::{Mutex, MutexGuard};
use axtask::WaitQueue;

use crate::{
    AxBlockDevice,
    prelude::{
        BaseDriverOps, BlockAsyncOp, BlockCompletion, BlockCompletionAvailability,
        BlockCompletionDrain, BlockCompletionNotifier, BlockCompletionOwner, BlockCompletionStatus,
        BlockCompletionTerminalNotifier, BlockDriverOps, BlockPhysicalCompletionRoute,
        BlockPhysicalRequest, BlockPhysicalSegment, BlockPhysicalSgOutcome, BlockQueueCaps,
        BlockQueueRequest, BlockRequestHandle, BlockResetOutcome, BlockSegment, BlockSubmitReport,
        DevError, DevResult, DeviceType,
    },
};

const COMPLETION_BATCH_CAPACITY: usize = 32;
const COMPLETION_MAILBOX_CAPACITY: usize = 128;
/// Number of independent physical completion route groups.  A reservation
/// consumes one whole group, even when it only needs one child.
const PHYSICAL_ROUTE_CAPACITY: usize = 32;
/// Maximum number of child requests in one physical route group.  This is the
/// lower route bound paired with the upper physical extent cap.
const PHYSICAL_ROUTE_CHILD_CAPACITY: usize = 16;
const COMPLETION_WAIT_SLICE: Duration = Duration::from_micros(100);

#[inline]
fn completion_progress_observed(observed: u64, current: u64, terminal: bool) -> bool {
    terminal || current != observed
}

#[inline]
fn sync_submit_unpublished_queue_full(report: &BlockSubmitReport, handles_empty: bool) -> bool {
    report.submitted == 0 && report.bytes == 0 && report.queue_full && handles_empty
}

#[inline]
fn sync_submit_queue_full_drain_progressed(drain: BlockCompletionDrain) -> bool {
    drain.completed != 0 || drain.continuation
}

#[inline]
fn completion_batch_has_physical(records: &[BlockCompletion]) -> bool {
    records
        .iter()
        .any(|record| record.owner == BlockCompletionOwner::Physical)
}

#[inline]
fn destination_drain_needs_followup(
    cached_hit: bool,
    lower_continuation: bool,
    destination_pending: bool,
) -> bool {
    cached_hit || lower_continuation || destination_pending
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum PhysicalRouteState {
    Reserved,
    Published,
    /// The exact waiter consumed the concrete completion, but the effect has
    /// not yet acknowledged the whole publication.  Keeping this slot in the
    /// table lets a later bounded exact wait authenticate the already-retired
    /// prefix without mistaking it for a stale/foreign handle.  Kernel routes
    /// are released immediately because the broker is their final owner.
    Completed,
    Quarantined,
}

#[derive(Clone, Copy)]
struct PhysicalRouteSlot {
    generation: u64,
    destination: BlockPhysicalCompletionRoute,
    state: PhysicalRouteState,
    handle: BlockRequestHandle,
    cookie: u64,
}

/// Exact waiter authorization captured while the route table still contains
/// the requested generation-bound identities.  It is deliberately borrowed
/// from the caller's fixed handle/cookie slices; no per-wait allocation or
/// current-generation lookup is needed when the completion is retired.
#[derive(Clone, Copy)]
struct ExactCompletionCapability<'a> {
    generation: u64,
    handles: &'a [BlockRequestHandle],
    cookies: &'a [u64],
}

impl ExactCompletionCapability<'_> {
    fn permits(&self, handle: BlockRequestHandle, cookie: u64) -> bool {
        self.handles
            .iter()
            .zip(self.cookies.iter().copied())
            .any(|(expected, expected_cookie)| {
                expected.raw == handle.raw && expected_cookie == cookie
            })
    }
}

#[derive(Clone, Copy)]
enum PhysicalRetirementCapability<'a> {
    /// A destination-aware lower drain captured this generation while it
    /// held the route table lock.
    Route { generation: u64 },
    /// An exact waiter authenticated both generation and identity before
    /// taking the mailbox record.
    Exact(ExactCompletionCapability<'a>),
}

impl PhysicalRetirementCapability<'_> {
    fn generation(self) -> u64 {
        match self {
            Self::Route { generation } => generation,
            Self::Exact(capability) => capability.generation,
        }
    }

    fn permits(self, record: BlockCompletion) -> bool {
        match self {
            Self::Route { .. } => true,
            Self::Exact(capability) => capability.permits(record.handle, record.cookie),
        }
    }
}

#[derive(Clone, Copy)]
struct PhysicalRouteGroup {
    children: [Option<PhysicalRouteSlot>; PHYSICAL_ROUTE_CHILD_CAPACITY],
    /// A malformed accepted prefix or a reset owns the whole group until the
    /// lower transport is explicitly quiesced.  Keeping this bit separate
    /// from child slots lets an unaccepted suffix remain distinguishable from
    /// a published child while still retaining group custody.
    quarantined: bool,
}

impl PhysicalRouteGroup {
    const fn new() -> Self {
        Self {
            children: [None; PHYSICAL_ROUTE_CHILD_CAPACITY],
            quarantined: false,
        }
    }

    fn is_free(&self) -> bool {
        !self.quarantined && self.children.iter().all(Option::is_none)
    }

    fn occupied(&self) -> bool {
        self.quarantined || self.children.iter().any(Option::is_some)
    }

    fn clear_if_empty(&mut self) {
        if self.children.iter().all(Option::is_none) {
            self.quarantined = false;
        }
    }
}

struct PhysicalRouteTable {
    groups: [PhysicalRouteGroup; PHYSICAL_ROUTE_CAPACITY],
}

impl PhysicalRouteTable {
    const fn new() -> Self {
        Self {
            groups: [PhysicalRouteGroup::new(); PHYSICAL_ROUTE_CAPACITY],
        }
    }

    fn reserve(
        &mut self,
        destination: BlockPhysicalCompletionRoute,
        generation: u64,
        count: usize,
    ) -> Result<u8, DevError> {
        if count == 0 || count > PHYSICAL_ROUTE_CHILD_CAPACITY {
            return Err(DevError::InvalidParam);
        }
        let Some((group_index, group)) = self
            .groups
            .iter_mut()
            .enumerate()
            .find(|(_, group)| group.is_free())
        else {
            return Err(DevError::ResourceBusy);
        };
        for child in group.children.iter_mut().take(count) {
            *child = Some(PhysicalRouteSlot {
                generation,
                destination,
                state: PhysicalRouteState::Reserved,
                handle: BlockRequestHandle { raw: 0 },
                cookie: 0,
            });
        }
        Ok(group_index as u8)
    }

    fn group(&self, group: u8) -> Option<&PhysicalRouteGroup> {
        self.groups.get(group as usize)
    }

    fn group_mut(&mut self, group: u8) -> Option<&mut PhysicalRouteGroup> {
        self.groups.get_mut(group as usize)
    }

    fn reservation_prefix_matches(
        &self,
        group: u8,
        generation: u64,
        destination: BlockPhysicalCompletionRoute,
        len: usize,
    ) -> bool {
        if len == 0 || len > PHYSICAL_ROUTE_CHILD_CAPACITY {
            return false;
        }
        self.group(group).is_some_and(|group| {
            group.children.iter().take(len).all(|slot| {
                slot.is_some_and(|slot| {
                    slot.generation == generation
                        && slot.destination == destination
                        && slot.state == PhysicalRouteState::Reserved
                        && slot.handle.raw == 0
                        && slot.cookie == 0
                })
            }) && group.children.iter().skip(len).all(Option::is_none)
        })
    }

    fn release_reserved(
        &mut self,
        group: u8,
        generation: u64,
        destination: BlockPhysicalCompletionRoute,
        start: usize,
        end: usize,
    ) -> bool {
        if start > end || end > PHYSICAL_ROUTE_CHILD_CAPACITY {
            return false;
        }
        let Some(group_data) = self.group(group) else {
            return false;
        };
        if !group_data
            .children
            .iter()
            .take(end)
            .skip(start)
            .all(|slot| {
                slot.is_some_and(|slot| {
                    slot.generation == generation
                        && slot.destination == destination
                        && slot.state == PhysicalRouteState::Reserved
                })
            })
        {
            return false;
        }
        let Some(group_data) = self.group_mut(group) else {
            return false;
        };
        for child in group_data.children.iter_mut().take(end).skip(start) {
            *child = None;
        }
        group_data.clear_if_empty();
        true
    }

    fn release_unpublished(
        &mut self,
        group: u8,
        generation: u64,
        destination: BlockPhysicalCompletionRoute,
        count: usize,
    ) -> bool {
        if !self.reservation_prefix_matches(group, generation, destination, count) {
            // A reset/quarantine changes Reserved to Quarantined and a reused
            // group carries a new generation.  In either case an old token
            // must not release the current owner's cells.
            return false;
        }
        let Some(group_data) = self.group_mut(group) else {
            return false;
        };
        for child in group_data.children.iter_mut().take(count) {
            *child = None;
        }
        group_data.clear_if_empty();
        true
    }

    fn mark_published(
        &mut self,
        group_index: u8,
        generation: u64,
        destination: BlockPhysicalCompletionRoute,
        requests: &[BlockPhysicalRequest<'_>],
    ) -> bool {
        if requests.is_empty() || requests.len() > PHYSICAL_ROUTE_CHILD_CAPACITY {
            return false;
        }
        let Some(group) = self.group(group_index) else {
            return false;
        };
        for (child, request) in group.children.iter().take(requests.len()).zip(requests) {
            let Some(slot) = child.as_ref() else {
                return false;
            };
            if slot.generation != generation
                || slot.destination != destination
                || slot.state != PhysicalRouteState::Reserved
                || request.handle.is_none()
                || request.handle.is_some_and(|handle| handle.raw == 0)
                || request.cookie.is_none_or(|cookie| cookie == 0)
            {
                return false;
            }
        }
        for (offset, request) in requests.iter().enumerate() {
            let Some(handle) = request.handle else {
                return false;
            };
            let Some(cookie) = request.cookie else {
                return false;
            };
            if requests[..offset].iter().any(|previous| {
                previous.handle.is_some_and(|other| other.raw == handle.raw)
                    || previous.cookie == Some(cookie)
            }) {
                // A duplicate raw identity/cookie cannot be demultiplexed;
                // the caller must retain every accepted owner for reset.
                return false;
            }
            // A lower driver must never recycle a raw handle or cookie while
            // the prior route is still in custody.  Checking the complete
            // fixed table (including Completed exact prefixes) keeps the
            // destination demuxable across concurrent effects.
            if self.groups.iter().enumerate().any(|(index, other_group)| {
                index != group_index as usize
                    && other_group
                        .children
                        .iter()
                        .flatten()
                        .any(|slot| slot.handle.raw == handle.raw || slot.cookie == cookie)
            }) {
                return false;
            }
        }
        let group = self.group_mut(group_index).expect("route checked");
        for (slot, request) in group.children.iter_mut().take(requests.len()).zip(requests) {
            let slot = slot.as_mut().expect("route checked");
            slot.state = PhysicalRouteState::Published;
            slot.handle = request.handle.expect("route checked");
            slot.cookie = request.cookie.expect("route checked");
        }
        true
    }

    fn mark_quarantined(&mut self) {
        for group in &mut self.groups {
            if !group.occupied() {
                continue;
            }
            group.quarantined = true;
            for slot in group.children.iter_mut().flatten() {
                slot.state = PhysicalRouteState::Quarantined;
            }
        }
    }

    fn mark_group_quarantined(&mut self, group_index: u8) {
        if let Some(group) = self.group_mut(group_index) {
            group.quarantined = true;
            for slot in group.children.iter_mut().flatten() {
                slot.state = PhysicalRouteState::Quarantined;
            }
        }
    }

    fn clear(&mut self) {
        self.groups.fill(PhysicalRouteGroup::new());
    }

    fn find(&self, generation: u64, raw: u64) -> Option<(u8, usize, PhysicalRouteSlot)> {
        if raw == 0 {
            return None;
        }
        self.groups
            .iter()
            .enumerate()
            .find_map(|(group, group_data)| {
                group_data
                    .children
                    .iter()
                    .enumerate()
                    .find_map(|(child, slot)| {
                        let slot = (*slot)?;
                        (slot.generation == generation && slot.handle.raw == raw).then_some((
                            group as u8,
                            child,
                            slot,
                        ))
                    })
            })
    }

    fn matches_exact(&self, generation: u64, raw: u64, cookie: u64) -> bool {
        self.find(generation, raw).is_some_and(|(_, _, slot)| {
            slot.destination == BlockPhysicalCompletionRoute::Exact
                && matches!(
                    slot.state,
                    PhysicalRouteState::Published | PhysicalRouteState::Completed
                )
                && slot.cookie == cookie
        })
    }

    fn matches_route(
        &self,
        generation: u64,
        raw: u64,
        cookie: u64,
        destination: BlockPhysicalCompletionRoute,
    ) -> Option<usize> {
        self.find(generation, raw).and_then(|(_, child, slot)| {
            (slot.destination == destination
                && slot.state == PhysicalRouteState::Published
                && slot.cookie == cookie)
                .then_some(child)
        })
    }

    fn completion_is_known(&self, generation: u64, raw: u64, cookie: u64) -> bool {
        self.find(generation, raw).is_some_and(|(_, _, slot)| {
            slot.state == PhysicalRouteState::Published && slot.cookie == cookie
        })
    }

    fn release_completion(&mut self, generation: u64, raw: u64, cookie: u64) -> bool {
        let Some((group_index, child_index, slot)) = self.find(generation, raw) else {
            return false;
        };
        if slot.state != PhysicalRouteState::Published || slot.cookie != cookie {
            return false;
        }
        if slot.destination == BlockPhysicalCompletionRoute::Exact {
            // Keep exact ownership until the waiter observes the complete
            // effect publication.  This is what makes partial exact drains
            // safe without a second per-effect hash/mailbox.
            self.groups[group_index as usize].children[child_index]
                .as_mut()
                .expect("route checked")
                .state = PhysicalRouteState::Completed;
        } else {
            self.groups[group_index as usize].children[child_index] = None;
            self.groups[group_index as usize].clear_if_empty();
        }
        true
    }

    fn exact_all_completed(
        &self,
        generation: u64,
        handles: &[BlockRequestHandle],
        cookies: &[u64],
    ) -> bool {
        handles.len() == cookies.len()
            && handles
                .iter()
                .zip(cookies.iter().copied())
                .all(|(handle, cookie)| {
                    self.find(generation, handle.raw)
                        .is_some_and(|(_, _, slot)| {
                            slot.destination == BlockPhysicalCompletionRoute::Exact
                                && slot.state == PhysicalRouteState::Completed
                                && slot.cookie == cookie
                        })
                })
    }

    fn release_exact_completed(
        &mut self,
        generation: u64,
        handles: &[BlockRequestHandle],
        cookies: &[u64],
    ) -> bool {
        if handles.is_empty()
            || handles.len() != cookies.len()
            || handles.iter().enumerate().any(|(index, handle)| {
                handles[..index]
                    .iter()
                    .any(|previous| previous.raw == handle.raw)
                    || cookies[..index]
                        .iter()
                        .any(|previous| *previous == cookies[index])
            })
            || !self.exact_all_completed(generation, handles, cookies)
        {
            return false;
        }
        let mut group_index = None;
        for (handle, cookie) in handles.iter().zip(cookies.iter().copied()) {
            let Some((group, _, slot)) = self.find(generation, handle.raw) else {
                return false;
            };
            if slot.cookie != cookie || slot.state != PhysicalRouteState::Completed {
                return false;
            }
            if let Some(previous) = group_index {
                if previous != group {
                    return false;
                }
            } else {
                group_index = Some(group);
            }
        }
        let Some(group_index) = group_index else {
            return false;
        };
        let group = &self.groups[group_index as usize];
        // A group is acknowledged only when the supplied identities cover
        // every accepted child.  This prevents a caller that consumed a
        // prefix from releasing that prefix while siblings remain in route
        // custody.
        let active = group.children.iter().flatten().count();
        if active != handles.len()
            || group.children.iter().flatten().any(|slot| {
                slot.destination != BlockPhysicalCompletionRoute::Exact
                    || slot.state != PhysicalRouteState::Completed
            })
        {
            return false;
        }
        self.groups[group_index as usize] = PhysicalRouteGroup::new();
        true
    }

    fn count(&self, generation: u64, destination: Option<BlockPhysicalCompletionRoute>) -> usize {
        self.groups
            .iter()
            .flat_map(|group| group.children.iter().flatten())
            .filter(|slot| {
                slot.generation == generation
                    && slot.state == PhysicalRouteState::Published
                    && destination.is_none_or(|wanted| slot.destination == wanted)
            })
            .count()
    }

    fn occupied(&self) -> bool {
        self.groups.iter().any(PhysicalRouteGroup::occupied)
    }

    /// Returns whether any route for `destination` still owns a publication
    /// slot in this generation. Unlike [`Self::count`], this includes a
    /// pre-publication reservation, so policy checks treat a destination as
    /// owned for the whole prepare/publish sequence.
    fn has_destination(&self, generation: u64, destination: BlockPhysicalCompletionRoute) -> bool {
        self.groups.iter().any(|group| {
            group.children.iter().flatten().any(|slot| {
                slot.generation == generation
                    && slot.destination == destination
                    && matches!(
                        slot.state,
                        PhysicalRouteState::Reserved
                            | PhysicalRouteState::Published
                            | PhysicalRouteState::Completed
                    )
            })
        })
    }
}

/// Fixed-capacity records retained by the single shared completion owner.
/// The ring cursors are hot; owner/status payload is kept in the same bounded
/// slab so no allocation or per-completion hash lookup occurs on the drain
/// path.  Physical records are removed by owner class, ordinary records by
/// exact raw handle.
struct CompletionMailbox {
    records: [Option<BlockCompletion>; COMPLETION_MAILBOX_CAPACITY],
    head: usize,
    len: usize,
}

impl CompletionMailbox {
    const fn new() -> Self {
        Self {
            records: [None; COMPLETION_MAILBOX_CAPACITY],
            head: 0,
            len: 0,
        }
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn contains_physical(&self) -> bool {
        (0..self.len).any(|offset| {
            self.records[(self.head + offset) % COMPLETION_MAILBOX_CAPACITY]
                .is_some_and(|record| record.owner == BlockCompletionOwner::Physical)
        })
    }

    fn contains_quarantined(&self) -> bool {
        (0..self.len).any(|offset| {
            self.records[(self.head + offset) % COMPLETION_MAILBOX_CAPACITY]
                .is_some_and(|record| record.status == BlockCompletionStatus::Quarantined)
        })
    }

    fn handle_has_invalid_identity(&self, raw: u64, expected_owner: BlockCompletionOwner) -> bool {
        (0..self.len).any(|offset| {
            self.records[(self.head + offset) % COMPLETION_MAILBOX_CAPACITY].is_some_and(|record| {
                record.handle.raw == raw
                    && (record.owner != expected_owner
                        || record.cookie == 0
                        || record.status == BlockCompletionStatus::Quarantined)
            })
        })
    }

    fn push(&mut self, record: BlockCompletion) -> bool {
        if self.len == COMPLETION_MAILBOX_CAPACITY {
            return false;
        }
        let index = (self.head + self.len) % COMPLETION_MAILBOX_CAPACITY;
        self.records[index] = Some(record);
        self.len += 1;
        true
    }

    fn remove_offset(&mut self, offset: usize) -> Option<BlockCompletion> {
        if offset >= self.len {
            return None;
        }
        let index = (self.head + offset) % COMPLETION_MAILBOX_CAPACITY;
        let record = self.records[index].take();
        for shift in offset..self.len.saturating_sub(1) {
            let from = (self.head + shift + 1) % COMPLETION_MAILBOX_CAPACITY;
            let to = (self.head + shift) % COMPLETION_MAILBOX_CAPACITY;
            self.records[to] = self.records[from].take();
        }
        let tail = (self.head + self.len.saturating_sub(1)) % COMPLETION_MAILBOX_CAPACITY;
        self.records[tail] = None;
        self.len -= 1;
        if self.len == 0 {
            self.head = 0;
        }
        record
    }

    fn take_physical_matching(
        &mut self,
        output: &mut [BlockCompletion],
        mut matches: impl FnMut(&BlockCompletion) -> bool,
    ) -> usize {
        let original_len = self.len;
        let mut kept = 0usize;
        let mut written = 0usize;
        for offset in 0..original_len {
            let source = (self.head + offset) % COMPLETION_MAILBOX_CAPACITY;
            let Some(record) = self.records[source].take() else {
                continue;
            };
            if record.owner == BlockCompletionOwner::Physical
                && matches(&record)
                && written < output.len()
            {
                output[written] = record;
                written += 1;
            } else {
                let destination = (self.head + kept) % COMPLETION_MAILBOX_CAPACITY;
                self.records[destination] = Some(record);
                kept += 1;
            }
        }
        for offset in kept..original_len {
            let index = (self.head + offset) % COMPLETION_MAILBOX_CAPACITY;
            self.records[index] = None;
        }
        self.len = kept;
        if self.len == 0 {
            self.head = 0;
        }
        written
    }

    fn contains_ordinary(&self) -> bool {
        (0..self.len).any(|offset| {
            self.records[(self.head + offset) % COMPLETION_MAILBOX_CAPACITY]
                .is_some_and(|record| record.owner != BlockCompletionOwner::Physical)
        })
    }

    fn take_ordinary(&mut self, output: &mut [BlockCompletion]) -> usize {
        self.take_matching(output, false)
    }

    /// Extract a bounded owner class in one compaction pass.  The previous
    /// remove-one-at-a-time implementation repeatedly shifted the mailbox
    /// tail for mixed ordinary/physical batches; keeping the hot cursor
    /// traversal linear makes a 32-record drain predictable without adding a
    /// lock-free data structure.
    fn take_matching(&mut self, output: &mut [BlockCompletion], physical: bool) -> usize {
        let original_len = self.len;
        let mut kept = 0usize;
        let mut written = 0usize;
        for offset in 0..original_len {
            let source = (self.head + offset) % COMPLETION_MAILBOX_CAPACITY;
            let Some(record) = self.records[source].take() else {
                continue;
            };
            let matches = (record.owner == BlockCompletionOwner::Physical) == physical;
            if matches && written < output.len() {
                output[written] = record;
                written += 1;
            } else {
                let destination = (self.head + kept) % COMPLETION_MAILBOX_CAPACITY;
                self.records[destination] = Some(record);
                kept += 1;
            }
        }
        for offset in kept..original_len {
            let index = (self.head + offset) % COMPLETION_MAILBOX_CAPACITY;
            self.records[index] = None;
        }
        self.len = kept;
        if self.len == 0 {
            self.head = 0;
        }
        written
    }

    fn take_handle(&mut self, raw: u64) -> Option<BlockCompletion> {
        let offset = (0..self.len).find(|offset| {
            self.records[(self.head + *offset) % COMPLETION_MAILBOX_CAPACITY]
                .is_some_and(|record| record.handle.raw == raw)
        })?;
        self.remove_offset(offset)
    }

    fn contains_handle(&self, raw: u64) -> bool {
        (0..self.len).any(|offset| {
            self.records[(self.head + offset) % COMPLETION_MAILBOX_CAPACITY]
                .is_some_and(|record| record.handle.raw == raw)
        })
    }

    /// Removes only records whose raw handle and expected completion cookie
    /// belong to this waiter.  Foreign physical records stay in the mailbox
    /// even when they precede the requested handle in the lower FIFO.  A
    /// matching raw handle with a malformed owner/cookie is left retained and
    /// reported as quarantine-required instead of being retired as EIO.
    fn take_handles_exact(
        &mut self,
        handles: &[BlockRequestHandle],
        cookies: &[u64],
        output: &mut [BlockCompletion],
    ) -> Result<usize, ()> {
        if handles.len() != cookies.len() {
            return Err(());
        }
        // Validate every matching record before changing mailbox ownership.
        // If a duplicate or cookie-mismatched record is present, retain the
        // complete mailbox (including otherwise valid siblings) for the
        // reset/quarantine owner rather than consuming credits partially.
        for offset in 0..self.len {
            let Some(record) = self.records[(self.head + offset) % COMPLETION_MAILBOX_CAPACITY]
            else {
                continue;
            };
            let requested = handles
                .iter()
                .zip(cookies.iter().copied())
                .find(|(handle, _)| handle.raw == record.handle.raw);
            if let Some((_, cookie)) = requested
                && (record.owner != BlockCompletionOwner::Physical
                    || record.cookie == 0
                    || record.cookie != cookie
                    || record.status == BlockCompletionStatus::Quarantined)
            {
                return Err(());
            }
        }
        // One publication produces one completion per raw handle.  A second
        // matching record is a duplicate used/token observation and must stay
        // quarantined rather than being retired on a later retry.
        for handle in handles {
            let matches = (0..self.len)
                .filter(|offset| {
                    self.records[(self.head + *offset) % COMPLETION_MAILBOX_CAPACITY]
                        .is_some_and(|record| record.handle.raw == handle.raw)
                })
                .count();
            if matches > 1 {
                return Err(());
            }
        }
        // A publication itself must carry unique raw identities.  Reject a
        // malformed effect before consuming even one valid sibling; otherwise
        // a duplicate input handle could retire one lower owner and leave the
        // second owner impossible to route.
        for (index, handle) in handles.iter().enumerate() {
            if handles[..index]
                .iter()
                .any(|previous| previous.raw == handle.raw)
            {
                return Err(());
            }
        }
        let original_len = self.len;
        let mut kept = 0usize;
        let mut written = 0usize;
        for offset in 0..original_len {
            let source = (self.head + offset) % COMPLETION_MAILBOX_CAPACITY;
            let Some(record) = self.records[source].take() else {
                continue;
            };
            let requested = handles
                .iter()
                .zip(cookies.iter().copied())
                .find(|(handle, _)| handle.raw == record.handle.raw);
            let valid = requested.is_some_and(|(_, cookie)| {
                record.owner == BlockCompletionOwner::Physical
                    && record.cookie != 0
                    && record.cookie == cookie
            });
            if requested.is_some() && valid && written < output.len() {
                output[written] = record;
                written += 1;
            } else {
                let destination = (self.head + kept) % COMPLETION_MAILBOX_CAPACITY;
                self.records[destination] = Some(record);
                kept += 1;
            }
        }
        for offset in kept..original_len {
            let index = (self.head + offset) % COMPLETION_MAILBOX_CAPACITY;
            self.records[index] = None;
        }
        self.len = kept;
        if self.len == 0 {
            self.head = 0;
        }
        Ok(written)
    }

    fn take_ordinary_count(&mut self, budget: usize) -> usize {
        let mut retired = 0;
        while retired < budget {
            let Some(offset) = (0..self.len).find(|offset| {
                self.records[(self.head + *offset) % COMPLETION_MAILBOX_CAPACITY]
                    .is_some_and(|record| record.owner != BlockCompletionOwner::Physical)
            }) else {
                break;
            };
            let _ = self.remove_offset(offset);
            retired += 1;
        }
        retired
    }
}

struct SharedBlockDeviceInner {
    device: Mutex<AxBlockDevice>,
    name: String,
    device_type: DeviceType,
    irq: Option<usize>,
    completions: Mutex<CompletionMailbox>,
    /// Fixed route groups/children are reserved before publication. A
    /// completion worker may therefore consume a used-ring entry without
    /// stealing an exact synchronous effect; it only removes records for its
    /// destination after the route table has authenticated raw handle/cookie.
    completion_routes: Mutex<PhysicalRouteTable>,
    completion_waiters: WaitQueue,
    /// Wake/progress generation changes for every IRQ and mailbox event.
    completion_generation: AtomicU64,
    /// Transport generation changes only across reset/reinitialization and
    /// is the identity bound to physical route reservations.
    completion_transport_generation: AtomicU64,
    completion_owner: AtomicBool,
    /// Number of published requests whose concrete completion has not yet
    /// been consumed by a mailbox/ordinary owner.  Keeping this separate
    /// from the mailbox length reserves room for lower-ring completions that
    /// have not been drained yet; otherwise cached records plus in-flight
    /// records could overflow the fixed slab.
    completion_credits: AtomicUsize,
    physical_pending: AtomicUsize,
    completion_quarantined: AtomicBool,
    /// A successful reset may retire the lower queue permanently.  This is a
    /// separate terminal state from quarantine: no notifier is reinstalled
    /// and no submission is admitted until transport reinitialization creates
    /// a new queue owner.
    completion_retired: AtomicBool,
    /// Set once a device-global worker has been installed.  The route table
    /// remains the sole used-ring demultiplexer for the device thereafter.
    completion_broker_installed: AtomicBool,
    completion_terminal_notifier: AtomicUsize,
    completion_terminal_context: AtomicUsize,
    completion_terminal_readers: AtomicUsize,
    /// Optional upper-owner wake bridge. The lower notifier remains owned by
    /// this wrapper; this second endpoint only publishes a bounded progress
    /// edge to a task-context owner (for example a multi-device io_uring
    /// worker). Its context is installed by a process-lifetime slot, so a
    /// late IRQ cannot dereference a freed ring or mount object.
    completion_progress_notifier: AtomicUsize,
    completion_progress_context: AtomicUsize,
}

fn notify_upper_completion_progress(inner: &SharedBlockDeviceInner) {
    let notifier = inner.completion_progress_notifier.load(Ordering::Acquire);
    let progress_context = inner.completion_progress_context.load(Ordering::Acquire);
    if notifier != 0 && progress_context != 0 {
        // SAFETY: the endpoint is published only with a function pointer and
        // a process-lifetime context. Uninstall clears the function before
        // clearing its context, so a racing IRQ either calls the old valid
        // pair or observes a closed endpoint.
        let notifier = unsafe { core::mem::transmute::<usize, BlockCompletionNotifier>(notifier) };
        notifier(progress_context);
    }
}

fn shared_completion_notify(context: usize) {
    if context == 0 {
        return;
    }
    // SAFETY: SharedBlockDevice installs this callback only with the stable
    // Arc allocation address and unregisters it before the inner value drops.
    let inner = unsafe { &*(context as *const SharedBlockDeviceInner) };
    inner.completion_generation.fetch_add(1, Ordering::AcqRel);
    inner.completion_waiters.notify_many(usize::MAX, false);
    notify_upper_completion_progress(inner);
}

struct CompletionOwnerGuard<'a> {
    inner: &'a SharedBlockDeviceInner,
}

impl Drop for SharedBlockDeviceInner {
    fn drop(&mut self) {
        // Arc ownership guarantees no user can call into this wrapper after
        // the final clone is being dropped. Clear the callback before the
        // allocation becomes invalid so a late IRQ can only observe a null
        // endpoint.
        let mut device = self.device.lock();
        let _ = device.install_completion_notifier(None, 0);
    }
}

impl Drop for CompletionOwnerGuard<'_> {
    fn drop(&mut self) {
        self.inner.completion_owner.store(false, Ordering::Release);
        self.inner.completion_waiters.notify_many(usize::MAX, false);
    }
}

/// A cloneable, serialized handle to one block device.
///
/// Filesystems and raw block-device files can share this handle without
/// duplicating descriptor queues or bypassing driver synchronization.
#[derive(Clone)]
pub struct SharedBlockDevice {
    inner: Arc<SharedBlockDeviceInner>,
}

/// Restricted guard for the shared device's ordinary metadata/flush access.
///
/// The raw [`AxBlockDevice`] guard is deliberately not exposed: once the
/// shared completion broker is installed, a caller must not invoke a lower
/// drain, fence, IRQ, or wait method while holding the device mutex. Those
/// operations would bypass the mailbox/credit ledger and could deadlock the
/// sole completion owner. Each operation below acquires the route/owner/device
/// locks in the broker's order, so a non-blocking filesystem setup can still
/// use the lower synchronous poller while the broker is idle.
pub struct SharedBlockDeviceGuard<'a> {
    device: &'a SharedBlockDevice,
}

impl SharedBlockDeviceGuard<'_> {
    /// Reads through the idle device's legacy synchronous owner. Once any
    /// route/credit/mailbox custody exists, the operation uses the typed
    /// shared owner instead; that path never falls back after publication.
    pub fn read_block(&mut self, block_id: u64, buf: &mut [u8]) -> DevResult {
        if let Some(result) = self
            .device
            .try_legacy_sync(|device| BlockDriverOps::read_block(device, block_id, buf))
        {
            return result;
        }
        self.device.read_block_owned(block_id, buf)
    }

    /// Write counterpart to [`Self::read_block`].
    pub fn write_block(&mut self, block_id: u64, buf: &[u8]) -> DevResult {
        if let Some(result) = self
            .device
            .try_legacy_sync(|device| BlockDriverOps::write_block(device, block_id, buf))
        {
            return result;
        }
        self.device.write_block_owned(block_id, buf)
    }

    /// Vectored ordinary read counterpart.
    pub fn read_block_vectored(&mut self, block_id: u64, bufs: &mut [&mut [u8]]) -> DevResult {
        if let Some(result) = self
            .device
            .try_legacy_sync(|device| BlockDriverOps::read_block_vectored(device, block_id, bufs))
        {
            return result;
        }
        self.device.read_block_vectored_owned(block_id, bufs)
    }

    /// Vectored ordinary write counterpart.
    pub fn write_block_vectored(&mut self, block_id: u64, bufs: &[&[u8]]) -> DevResult {
        if let Some(result) = self
            .device
            .try_legacy_sync(|device| BlockDriverOps::write_block_vectored(device, block_id, bufs))
        {
            return result;
        }
        self.device.write_block_vectored_owned(block_id, bufs)
    }

    /// Flushes through the idle legacy owner or the typed shared owner.
    pub fn flush(&mut self) -> DevResult {
        if let Some(result) = self.device.try_legacy_sync(BlockDriverOps::flush) {
            return result;
        }
        {
            let mut shared = self.device.clone();
            return BlockDriverOps::flush(&mut shared);
        }
    }
}

/// A bounded physical route reservation.
///
/// The reservation owns one whole route group and a prefix of its child
/// cells. Once submission succeeds, the group is owned by the device mailbox
/// until its route-specific terminal acknowledgement or a typed
/// reset/quarantine state. Dropping an uncommitted reservation rolls back only
/// still-reserved child cells; it can never roll back a published descriptor.
pub struct BlockPhysicalRouteReservation {
    device: SharedBlockDevice,
    destination: BlockPhysicalCompletionRoute,
    generation: u64,
    group: u8,
    len: usize,
    committed: bool,
}

impl BlockPhysicalRouteReservation {
    /// Route destination selected before descriptor publication.
    pub fn destination(&self) -> BlockPhysicalCompletionRoute {
        self.destination
    }

    /// Completion generation bound to this reservation.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Number of physical requests reserved by this token.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether descriptor publication committed the route group.
    pub fn is_committed(&self) -> bool {
        self.committed
    }
}

impl Drop for BlockPhysicalRouteReservation {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let _ = self
            .device
            .inner
            .completion_routes
            .lock()
            .release_unpublished(self.group, self.generation, self.destination, self.len);
    }
}

impl SharedBlockDevice {
    pub fn new(device: AxBlockDevice) -> Self {
        let name = device.device_name().to_string();
        let device_type = device.device_type();
        let irq = device.irq_num();
        let inner = Arc::new(SharedBlockDeviceInner {
            device: Mutex::new(device),
            name,
            device_type,
            irq,
            completions: Mutex::new(CompletionMailbox::new()),
            completion_routes: Mutex::new(PhysicalRouteTable::new()),
            completion_waiters: WaitQueue::new(),
            completion_generation: AtomicU64::new(0),
            completion_transport_generation: AtomicU64::new(0),
            completion_owner: AtomicBool::new(false),
            completion_credits: AtomicUsize::new(0),
            physical_pending: AtomicUsize::new(0),
            completion_quarantined: AtomicBool::new(false),
            completion_retired: AtomicBool::new(false),
            completion_broker_installed: AtomicBool::new(false),
            completion_terminal_notifier: AtomicUsize::new(0),
            completion_terminal_context: AtomicUsize::new(0),
            completion_terminal_readers: AtomicUsize::new(0),
            completion_progress_notifier: AtomicUsize::new(0),
            completion_progress_context: AtomicUsize::new(0),
        });
        // Install the wake bridge before the device can be published to a
        // registry.  The callback only advances a generation and wakes tasks;
        // all queue inspection remains in bounded task-context drains.
        {
            let mut device = inner.device.lock();
            let _ = device.install_completion_notifier(
                Some(shared_completion_notify as BlockCompletionNotifier),
                Arc::as_ptr(&inner) as usize,
            );
        }
        Self { inner }
    }

    /// Returns a restricted ordinary-device guard.  Completion and reset
    /// methods are intentionally unavailable through this value.
    pub fn lock(&self) -> SharedBlockDeviceGuard<'_> {
        SharedBlockDeviceGuard { device: self }
    }

    /// Stable identity of this exact shared queue owner.
    ///
    /// Clones of one [`SharedBlockDevice`] preserve the same identity, while
    /// independently discovered devices cannot alias it. The value is only
    /// an opaque demultiplexing token; authorization never uses a device
    /// name or pathname.
    #[inline]
    pub fn identity_token(&self) -> usize {
        Arc::as_ptr(&self.inner) as usize
    }

    /// Installs an allocation-free progress callback used by an upper task
    /// owner. The callback is invoked after the lower IRQ bridge publishes a
    /// progress generation and must not drain the queue itself.
    pub fn install_completion_progress_notifier(
        &self,
        notifier: Option<BlockCompletionNotifier>,
        context: usize,
    ) -> DevResult {
        match notifier {
            Some(notifier) if context != 0 => {
                self.inner
                    .completion_progress_context
                    .store(context, Ordering::Release);
                self.inner
                    .completion_progress_notifier
                    .store(notifier as usize, Ordering::Release);
                Ok(())
            }
            None if context == 0 => {
                self.inner
                    .completion_progress_notifier
                    .store(0, Ordering::Release);
                self.inner
                    .completion_progress_context
                    .store(0, Ordering::Release);
                Ok(())
            }
            _ => Err(DevError::InvalidParam),
        }
    }

    fn lock_raw(&self) -> MutexGuard<'_, AxBlockDevice> {
        self.inner.device.lock()
    }

    /// Runs one ordinary synchronous operation through the lower driver's
    /// legacy owner while the device has no published completion custody.
    /// Filesystem setup can run with task blocking disabled; publishing an
    /// ordinary descriptor and then waiting on the shared mailbox would
    /// return `Again` even though the transport itself can synchronously poll
    /// it. The idle legacy path therefore remains available after broker
    /// installation, but only while every route/physical/credit/mailbox
    /// custody check is empty.
    ///
    /// The sole completion owner is claimed before the route lock, then the
    /// route lock is acquired before the lower mutex and held through the
    /// operation. This is the publication order used by physical routes and
    /// broker installation: no drain or descriptor publication can race the
    /// pre-publication legacy decision. Once custody exists, this returns
    /// `None` and callers use the typed split-phase path; it never falls back
    /// after a descriptor was published.
    fn try_legacy_sync<R>(
        &self,
        operation: impl FnOnce(&mut AxBlockDevice) -> DevResult<R>,
    ) -> Option<DevResult<R>> {
        if self.completion_unavailable() {
            return Some(Err(DevError::BadState));
        }
        // Before broker installation there is no competing shared drain
        // owner; avoid touching the task wait queue during early filesystem
        // bootstrap (which may run with local IRQs disabled). Once the broker
        // is live, claim its sole owner before taking the route/device locks.
        let _owner = if self.physical_completion_broker_installed() {
            match self.claim_completion_owner() {
                Ok(owner) => Some(owner),
                Err(DevError::Again) => return None,
                Err(error) => return Some(Err(error)),
            }
        } else {
            None
        };
        let routes = self.inner.completion_routes.lock();
        if routes.occupied() {
            return None;
        }
        let mut device = self.inner.device.lock();
        if self.completion_unavailable() {
            return Some(Err(DevError::BadState));
        }
        if self.inner.physical_pending.load(Ordering::Acquire) != 0
            || self.inner.completion_credits.load(Ordering::Acquire) != 0
        {
            return None;
        }
        let mailbox = self.inner.completions.lock();
        if mailbox.len != 0 {
            return None;
        }
        drop(mailbox);
        Some(operation(&mut device))
    }

    fn notify_progress(&self) {
        self.inner
            .completion_generation
            .fetch_add(1, Ordering::AcqRel);
        self.inner.completion_waiters.notify_many(usize::MAX, false);
    }

    /// Installs the typed reset/generation notification consumed by the
    /// device-global kernel broker.  The context must remain valid until a
    /// matching `None` uninstall; reset invokes this callback only after the
    /// lower queue has selected its typed state.
    pub fn install_completion_terminal_notifier(
        &self,
        notifier: Option<BlockCompletionTerminalNotifier>,
        context: usize,
    ) -> DevResult {
        let callback = match notifier {
            Some(notifier) if context != 0 => notifier as usize,
            None if context == 0 => 0,
            Some(_) => return Err(DevError::InvalidParam),
            None => return Err(DevError::InvalidParam),
        };
        if callback == 0 {
            self.inner
                .completion_terminal_notifier
                .store(0, Ordering::Release);
            while self
                .inner
                .completion_terminal_readers
                .load(Ordering::Acquire)
                != 0
            {
                spin_loop();
            }
            self.inner
                .completion_terminal_context
                .store(0, Ordering::Release);
        } else {
            // Replace the pair as one quiescent publication.  Storing a new
            // context while an old callback is still visible could let a
            // racing reset load a mixed function/context pair; briefly close
            // the endpoint and wait for readers just as the uninstall path
            // does.
            self.inner
                .completion_terminal_notifier
                .store(0, Ordering::Release);
            while self
                .inner
                .completion_terminal_readers
                .load(Ordering::Acquire)
                != 0
            {
                spin_loop();
            }
            self.inner
                .completion_terminal_context
                .store(context, Ordering::Release);
            self.inner
                .completion_terminal_notifier
                .store(callback, Ordering::Release);
        }
        Ok(())
    }

    fn notify_terminal(&self) {
        self.inner
            .completion_terminal_readers
            .fetch_add(1, Ordering::AcqRel);
        let callback = self
            .inner
            .completion_terminal_notifier
            .load(Ordering::Acquire);
        let context = self
            .inner
            .completion_terminal_context
            .load(Ordering::Acquire);
        if callback == 0 || context == 0 {
            self.inner
                .completion_terminal_readers
                .fetch_sub(1, Ordering::Release);
            return;
        }
        // SAFETY: installation accepts only a function pointer and a
        // non-zero caller-owned context; reset invokes the pointer in task
        // context before any caller is allowed to release that context.
        let callback =
            unsafe { core::mem::transmute::<usize, BlockCompletionTerminalNotifier>(callback) };
        callback(context, self.completion_availability());
        self.inner
            .completion_terminal_readers
            .fetch_sub(1, Ordering::Release);
    }

    #[inline]
    fn completion_unavailable(&self) -> bool {
        self.inner.completion_quarantined.load(Ordering::Acquire)
            || self.inner.completion_retired.load(Ordering::Acquire)
    }

    /// Returns the live/terminal completion state and its generation.
    pub fn completion_availability(&self) -> BlockCompletionAvailability {
        let generation = self
            .inner
            .completion_transport_generation
            .load(Ordering::Acquire);
        if self.inner.completion_retired.load(Ordering::Acquire) {
            BlockCompletionAvailability::Retired { generation }
        } else if self.inner.completion_quarantined.load(Ordering::Acquire) {
            BlockCompletionAvailability::Quarantined { generation }
        } else {
            BlockCompletionAvailability::Live { generation }
        }
    }

    /// Returns the generation currently accepted for new route reservations.
    pub fn completion_generation(&self) -> u64 {
        self.inner
            .completion_transport_generation
            .load(Ordering::Acquire)
    }

    /// Installs the device-global route owner exactly once for this transport
    /// generation.  The owner is established before any kernel route should
    /// publish a descriptor; later synchronous effects use the same bounded
    /// broker rather than competing with a second used-ring consumer.
    pub fn install_physical_completion_broker(&self) -> DevResult<u64> {
        if self.completion_unavailable() {
            return Err(DevError::BadState);
        }
        let routes = self.inner.completion_routes.lock();
        // Serialize broker publication with the only pre-broker raw fallback
        // paths. A caller may already hold the lower mutex while finishing
        // one short synchronous operation; waiting here means the broker is
        // not marked live until that operation has left the device, rather
        // than racing the flag check and becoming a second queue owner.
        let _device = self.inner.device.lock();
        let generation = self.completion_generation();
        if self.inner.physical_pending.load(Ordering::Acquire) != 0
            // A completed exact prefix is still owner custody until the
            // effect acknowledges the whole publication.  Do not install or
            // replace the global destination policy while any terminal route
            // slot remains, even if its physical-pending counter already
            // reached zero.
            || routes.occupied()
            || self.mailbox_has_physical()
        {
            // Switching the destination policy after publication would leave
            // an in-flight exact owner without a single established drain
            // owner. Reinitialize/quiesce first instead of stealing it.
            return Err(DevError::ResourceBusy);
        }
        self.inner
            .completion_broker_installed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| DevError::ResourceBusy)?;
        drop(routes);
        self.notify_progress();
        Ok(generation)
    }

    /// Whether a device-global completion owner has been installed.
    pub fn physical_completion_broker_installed(&self) -> bool {
        self.inner
            .completion_broker_installed
            .load(Ordering::Acquire)
    }

    /// Number of published routes for one destination in the current
    /// generation.  This is a bounded, fixed-table query used by a worker to
    /// decide whether it must continue draining even when no kernel route is
    /// currently ready.
    pub fn physical_completion_route_count(
        &self,
        destination: Option<BlockPhysicalCompletionRoute>,
    ) -> usize {
        self.inner
            .completion_routes
            .lock()
            .count(self.completion_generation(), destination)
    }

    /// Returns whether any lower physical owner still needs a completion
    /// drain or mailbox wait.  It intentionally includes exact routes so a
    /// global worker can remain the sole used-ring consumer while a kernel
    /// route is temporarily absent.
    pub fn physical_completion_work_pending(&self) -> bool {
        // Keep the route/mailbox lock order identical to the broker drain
        // (`routes` then `mailbox`).  The previous mailbox-first ordering
        // could deadlock against `take_routed_physical` while two task
        // contexts observed progress concurrently.
        self.inner.physical_pending.load(Ordering::Acquire) != 0
            || self.physical_completion_route_count(None) != 0
            || self.mailbox_has_physical()
    }

    /// Reserves one fixed route group and a bounded child prefix before
    /// descriptor publication.
    pub fn reserve_physical_completion_routes(
        &self,
        destination: BlockPhysicalCompletionRoute,
        count: usize,
    ) -> DevResult<BlockPhysicalRouteReservation> {
        if self.completion_unavailable() {
            return Err(DevError::BadState);
        }
        let generation = self.completion_generation();
        let group = {
            let mut routes = self.inner.completion_routes.lock();
            if destination == BlockPhysicalCompletionRoute::Kernel
                && !self.physical_completion_broker_installed()
            {
                // A kernel route without an installed device owner would
                // create a second used-ring consumer. Require the owner
                // while holding the same route lock used by installation.
                return Err(DevError::BadState);
            }
            routes.reserve(destination, generation, count)?
        };
        // Reset may race the reservation after the availability check.  The
        // generation check keeps this token pre-publication and harmless;
        // reset marks its group terminal instead of releasing it as if no
        // route had ever existed.
        if self.completion_unavailable() || self.completion_generation() != generation {
            // No descriptor can be visible yet: the reservation token was
            // never returned to the caller.  Release these pre-publication
            // group even when reset won the race between the route lock and
            // the generation check; otherwise a retired device would leak
            // reserved child ownership forever.
            self.inner.completion_routes.lock().release_unpublished(
                group,
                generation,
                destination,
                count,
            );
            return Err(DevError::BadState);
        }
        Ok(BlockPhysicalRouteReservation {
            device: self.clone(),
            destination,
            generation,
            group,
            len: count,
            committed: false,
        })
    }

    fn reserve_completion_credits(&self, requested: usize) -> DevResult {
        let credits = self.inner.completion_credits.load(Ordering::Acquire);
        if requested > COMPLETION_MAILBOX_CAPACITY.saturating_sub(credits) {
            return Err(DevError::ResourceBusy);
        }
        Ok(())
    }

    fn publish_completion_credits(&self, submitted: usize) {
        let previous = self
            .inner
            .completion_credits
            .fetch_add(submitted, Ordering::AcqRel);
        if previous > COMPLETION_MAILBOX_CAPACITY.saturating_sub(submitted) {
            // Publication accounting itself became impossible.  Do not try
            // to release an owner whose exact queue identity is no longer
            // provable; the explicit reset path retains/quarantines it.
            self.inner
                .completion_quarantined
                .store(true, Ordering::Release);
            self.notify_progress();
        }
    }

    fn consume_completion_credits(&self, completed: usize) -> DevResult {
        let previous = self
            .inner
            .completion_credits
            .fetch_sub(completed, Ordering::AcqRel);
        if previous < completed {
            self.inner.completion_credits.store(0, Ordering::Release);
            self.inner
                .completion_quarantined
                .store(true, Ordering::Release);
            self.notify_progress();
            return Err(DevError::BadState);
        }
        Ok(())
    }

    fn claim_completion_owner(&self) -> DevResult<CompletionOwnerGuard<'_>> {
        loop {
            if self
                .inner
                .completion_owner
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Ok(CompletionOwnerGuard { inner: &self.inner });
            }
            if !axtask::can_block_current() {
                return Err(DevError::Again);
            }
            let generation = self.inner.completion_generation.load(Ordering::Acquire);
            let _ = self
                .inner
                .completion_waiters
                .wait_timeout_until(COMPLETION_WAIT_SLICE, || {
                    !self.inner.completion_owner.load(Ordering::Acquire)
                        || self.inner.completion_generation.load(Ordering::Acquire) != generation
                });
        }
    }

    fn mailbox_has_physical(&self) -> bool {
        self.inner.completions.lock().contains_physical()
    }

    /// Performs one bounded lower drain while holding the route table.  Route
    /// publication uses the same `routes -> device -> mailbox` lock order;
    /// keeping the route guard across the lower call closes the check-then-
    /// drain window in which an exact owner could otherwise be published
    /// after a generic waiter inspected the table.
    fn drain_device_once_locked(
        &self,
        routes: &mut PhysicalRouteTable,
    ) -> DevResult<(BlockCompletionDrain, bool)> {
        if self.completion_unavailable() {
            return Err(DevError::BadState);
        }
        let mut output = [BlockCompletion {
            handle: BlockRequestHandle { raw: 0 },
            owner: BlockCompletionOwner::Ordinary,
            cookie: 0,
            status: BlockCompletionStatus::Quarantined,
            bytes: 0,
        }; COMPLETION_BATCH_CAPACITY];
        let drain = {
            let mut device = self.inner.device.lock();
            let mailbox = self.inner.completions.lock();
            let available = COMPLETION_MAILBOX_CAPACITY.saturating_sub(mailbox.len);
            if available == 0 {
                // Completion credits include lower-ring requests that have
                // not reached this mailbox yet. With admission enforcing the
                // same fixed bound, a full mailbox cannot coexist with
                // another valid lower completion; leave the ring untouched
                // until an owner consumes a cached record.
                return Ok((BlockCompletionDrain::default(), false));
            }
            let output_len = output.len().min(available);
            device.drain_async_completions(&mut output[..output_len])
        };
        let drain = match drain {
            Ok(drain) => drain,
            Err(error @ DevError::BadState) => {
                // Lower malformed/duplicate used entries are typed
                // quarantine, not ordinary EIO. Preserve the flag until an
                // explicit reset proves quiescence; all waiters are woken so
                // they cannot remain asleep behind the poisoned head.
                self.inner
                    .completion_quarantined
                    .store(true, Ordering::Release);
                routes.mark_quarantined();
                self.notify_progress();
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        if drain.completed > output.len() {
            self.inner
                .completion_quarantined
                .store(true, Ordering::Release);
            routes.mark_quarantined();
            self.notify_progress();
            return Err(DevError::BadState);
        }
        if drain.completed != 0 {
            let mailbox_overflow = {
                let mut mailbox = self.inner.completions.lock();
                let mut overflow = false;
                for record in output.iter().copied().take(drain.completed) {
                    if !mailbox.push(record) {
                        // The lower queue exceeded the fixed ownership slab.
                        // Keep all existing records and require reset; never
                        // release a published owner merely to surface EIO.
                        overflow = true;
                        break;
                    }
                }
                overflow
            };
            if mailbox_overflow {
                self.inner
                    .completion_quarantined
                    .store(true, Ordering::Release);
                routes.mark_quarantined();
                self.notify_progress();
                return Err(DevError::BadState);
            }
            // Route identity is authenticated after the bounded lower drain
            // but before any waiter can retire a record.  A stale/duplicate
            // physical completion remains in the mailbox as raw status while
            // every route is moved to typed quarantine custody.
            let generation = self.completion_generation();
            let records = &output[..drain.completed];
            let duplicate_identity = records.iter().enumerate().any(|(offset, record)| {
                records[..offset].iter().any(|previous| {
                    previous.handle.raw == record.handle.raw || previous.cookie == record.cookie
                })
            });
            let routes_valid = !duplicate_identity
                && records.iter().copied().all(|record| {
                    // A live queue must never publish zero/terminal identity
                    // fields.  This check applies to ordinary and legacy
                    // records too: otherwise a malformed ordinary record
                    // could be consumed while a physical route with the
                    // same raw handle remains permanently published.
                    if record.handle.raw == 0
                        || record.cookie == 0
                        || record.status == BlockCompletionStatus::Quarantined
                    {
                        return false;
                    }
                    if record.owner == BlockCompletionOwner::Physical {
                        routes.completion_is_known(generation, record.handle.raw, record.cookie)
                    } else {
                        // A lower owner classification must not collide with
                        // any current physical route identity.  Treat a raw
                        // collision as a malformed completion instead of
                        // letting an ordinary waiter steal the DMA owner.
                        routes.find(generation, record.handle.raw).is_none()
                    }
                });
            if !routes_valid {
                routes.mark_quarantined();
                self.inner
                    .completion_quarantined
                    .store(true, Ordering::Release);
                // The batch may contain otherwise valid siblings, but the
                // single lower owner is no longer demuxable as a whole. Do
                // not let an ordinary or physical waiter retire any prefix
                // after an unknown/duplicate raw identity was observed;
                // leave every record in mailbox custody for reset.
                self.notify_progress();
                return Err(DevError::BadState);
            }
            self.notify_progress();
            return Ok((drain, completion_batch_has_physical(records)));
        }
        Ok((drain, false))
    }

    fn drain_device_once(&self) -> DevResult<(BlockCompletionDrain, u64)> {
        let mut routes = self.inner.completion_routes.lock();
        let generation = self.completion_generation();
        let drained = self.drain_device_once_locked(&mut routes);
        drop(routes);
        let (drain, physical_published) = drained?;
        if physical_published {
            notify_upper_completion_progress(&self.inner);
        }
        Ok((drain, generation))
    }

    fn wait_for_progress(&self) -> DevResult {
        if !axtask::can_block_current() {
            return Err(DevError::Again);
        }
        let generation = self.inner.completion_generation.load(Ordering::Acquire);
        let _ = self
            .inner
            .completion_waiters
            .wait_timeout_until(COMPLETION_WAIT_SLICE, || {
                self.inner.completion_generation.load(Ordering::Acquire) != generation
                    || self.mailbox_has_physical()
                    || self.completion_unavailable()
            });
        Ok(())
    }

    fn wait_for_generation_progress(&self) -> DevResult {
        if !axtask::can_block_current() {
            return Err(DevError::Again);
        }
        let generation = self.inner.completion_generation.load(Ordering::Acquire);
        let _ = self
            .inner
            .completion_waiters
            .wait_timeout_until(COMPLETION_WAIT_SLICE, || {
                self.inner.completion_generation.load(Ordering::Acquire) != generation
                    || self.completion_unavailable()
            });
        Ok(())
    }

    /// Waits for progress relative to a snapshot taken before a submission
    /// attempt.  The caller checks the generation once before registering its
    /// listener and `wait_timeout_until` checks it again after registration;
    /// that two-sided check closes the completion/subscribe lost-wake window.
    /// A reset/quarantine is terminal for the current owner and must not turn
    /// into an unbounded retry loop.
    fn wait_for_generation_progress_since(&self, observed: u64) -> DevResult {
        let terminal = self.completion_unavailable();
        let current = self.inner.completion_generation.load(Ordering::Acquire);
        if completion_progress_observed(observed, current, terminal) {
            return if terminal {
                Err(DevError::BadState)
            } else {
                Ok(())
            };
        }
        if !axtask::can_block_current() {
            return Err(DevError::Again);
        }
        let _ = self
            .inner
            .completion_waiters
            .wait_timeout_until(COMPLETION_WAIT_SLICE, || {
                completion_progress_observed(
                    observed,
                    self.inner.completion_generation.load(Ordering::Acquire),
                    self.completion_unavailable(),
                )
            });
        if self.completion_unavailable() {
            return Err(DevError::BadState);
        }
        Ok(())
    }

    fn wait_for_exact_progress(&self, handles: &[BlockRequestHandle]) -> DevResult {
        if !axtask::can_block_current() {
            return Err(DevError::Again);
        }
        let generation = self.inner.completion_generation.load(Ordering::Acquire);
        let _ = self
            .inner
            .completion_waiters
            .wait_timeout_until(COMPLETION_WAIT_SLICE, || {
                self.inner.completion_generation.load(Ordering::Acquire) != generation
                    || self.completion_unavailable()
                    || handles
                        .iter()
                        .any(|handle| self.inner.completions.lock().contains_handle(handle.raw))
            });
        Ok(())
    }

    fn wait_for_handle_progress(&self, handle: BlockRequestHandle) -> DevResult {
        if !axtask::can_block_current() {
            return Err(DevError::Again);
        }
        let generation = self.inner.completion_generation.load(Ordering::Acquire);
        let _ = self
            .inner
            .completion_waiters
            .wait_timeout_until(COMPLETION_WAIT_SLICE, || {
                self.inner.completion_generation.load(Ordering::Acquire) != generation
                    || self.completion_unavailable()
                    || self.inner.completions.lock().contains_handle(handle.raw)
            });
        Ok(())
    }

    fn take_routed_physical_locked(
        &self,
        routes: &PhysicalRouteTable,
        generation: u64,
        destination: BlockPhysicalCompletionRoute,
        output: &mut [BlockCompletion],
    ) -> usize {
        let mut mailbox = self.inner.completions.lock();
        mailbox.take_physical_matching(output, |record| {
            routes
                .matches_route(generation, record.handle.raw, record.cookie, destination)
                .is_some()
        })
    }

    fn routed_physical_pending_locked(
        &self,
        routes: &PhysicalRouteTable,
        generation: u64,
        destination: BlockPhysicalCompletionRoute,
    ) -> bool {
        let mailbox = self.inner.completions.lock();
        (0..mailbox.len).any(|offset| {
            mailbox.records[(mailbox.head + offset) % COMPLETION_MAILBOX_CAPACITY].is_some_and(
                |record| {
                    record.owner == BlockCompletionOwner::Physical
                        && routes
                            .matches_route(
                                generation,
                                record.handle.raw,
                                record.cookie,
                                destination,
                            )
                            .is_some()
                },
            )
        })
    }

    /// Drains one bounded lower batch for exactly one physical destination.
    /// The route guard spans both the cached-mailbox extraction and the lower
    /// used-ring drain, so a route publication cannot slip between the
    /// destination check and the extraction.  Callers own the completion
    /// owner while invoking this helper; publication and draining therefore
    /// share both the route lock and the device owner sequence.
    fn drain_destination_once(
        &self,
        destination: BlockPhysicalCompletionRoute,
        output: &mut [BlockCompletion],
    ) -> DevResult<(BlockCompletionDrain, u64)> {
        let mut routes = self.inner.completion_routes.lock();
        let generation = self.completion_generation();
        let mut lower = BlockCompletionDrain::default();
        let mut completed =
            self.take_routed_physical_locked(&routes, generation, destination, output);
        let cached_hit = completed != 0;
        if completed == 0 {
            let (drain, _physical_published) = self.drain_device_once_locked(&mut routes)?;
            lower = drain;
            completed = self.take_routed_physical_locked(&routes, generation, destination, output);
        }
        // A published route is not itself a continuation: while its device
        // request is still in flight there is no used-ring work to make
        // progress on, so reporting the route count here would turn the
        // worker's bounded pass into a yield/busy loop.  Only an actual lower
        // continuation or an already-cached completion for this destination
        // warrants another immediate pass.
        let continuation = destination_drain_needs_followup(
            cached_hit,
            lower.continuation,
            self.routed_physical_pending_locked(&routes, generation, destination),
        );
        Ok((
            BlockCompletionDrain {
                completed,
                continuation,
            },
            generation,
        ))
    }

    fn retire_routed_physical(
        &self,
        capability: PhysicalRetirementCapability<'_>,
        records: &[BlockCompletion],
        completed: usize,
    ) -> DevResult<()> {
        if completed == 0 || completed > records.len() {
            return Err(DevError::InvalidParam);
        }

        // Keep the route lock across authentication and counter retirement.
        // Reset takes this lock before clearing/reusing route groups, so an
        // old exact capability cannot validate against one generation and
        // then decrement credits/pending for a reused owner.
        let generation = capability.generation();
        let mut routes = self.inner.completion_routes.lock();
        // Reset advances the transport generation before taking route
        // custody.  Once that edge is visible, an old drain/waiter must not
        // inspect or mutate the newly reused table at all; in particular, a
        // wrong record from the old pass must not quarantine the new owner.
        if self.completion_generation() != generation {
            return Err(DevError::BadState);
        }
        let mut invalid_current_owner = false;
        for (offset, record) in records.iter().copied().take(completed).enumerate() {
            if record.owner != BlockCompletionOwner::Physical || !capability.permits(record) {
                invalid_current_owner = true;
                continue;
            }
            if records[..offset].iter().take(completed).any(|previous| {
                previous.handle.raw == record.handle.raw || previous.cookie == record.cookie
            }) {
                // A lower batch must contain one terminal observation per
                // published identity.  Validate the whole batch before
                // decrementing credits or releasing its first slot; a
                // duplicate is a malformed transport result, not a second
                // successful retirement.
                invalid_current_owner = true;
                continue;
            }
            if !routes.completion_is_known(generation, record.handle.raw, record.cookie) {
                // The generation was checked above, so a missing or
                // non-published slot here is a malformed current owner, not
                // a stale waiter.  Keep the queue fail-closed and preserve
                // every owner for reset/quarantine.
                invalid_current_owner = true;
            }
        }
        if invalid_current_owner {
            routes.mark_quarantined();
            self.inner
                .completion_quarantined
                .store(true, Ordering::Release);
            self.notify_progress();
            return Err(DevError::BadState);
        }

        self.consume_completion_credits(completed)?;
        let mut underflow = false;
        let retired = self.inner.physical_pending.try_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |pending| {
                if pending < completed {
                    underflow = true;
                    None
                } else {
                    Some(pending - completed)
                }
            },
        );
        if retired.is_err() || underflow {
            routes.mark_quarantined();
            self.inner
                .completion_quarantined
                .store(true, Ordering::Release);
            self.notify_progress();
            return Err(DevError::BadState);
        }
        for record in records.iter().copied().take(completed) {
            if !routes.release_completion(generation, record.handle.raw, record.cookie) {
                routes.mark_quarantined();
                self.inner
                    .completion_quarantined
                    .store(true, Ordering::Release);
                self.notify_progress();
                return Err(DevError::BadState);
            }
        }
        // Mailbox retirement can remove the last exact route while the
        // broker worker is asleep waiting for progress. Wake it explicitly;
        // an IRQ is not required after a task-local exact consumer makes
        // progress.
        self.notify_progress();
        Ok(())
    }

    fn release_exact_routes_if_complete(&self, capability: ExactCompletionCapability<'_>) {
        let mut routes = self.inner.completion_routes.lock();
        if routes.release_exact_completed(
            capability.generation,
            capability.handles,
            capability.cookies,
        ) {
            drop(routes);
            // A new route may be admitted now that the effect has explicitly
            // consumed its complete publication.  Wake the global worker if
            // it was asleep on a full fixed route table.
            self.notify_progress();
        }
    }

    fn wait_exact_from_broker(
        &self,
        capability: ExactCompletionCapability<'_>,
        output: &mut [BlockCompletion],
    ) -> DevResult<BlockCompletionDrain> {
        loop {
            if self.completion_unavailable() {
                return Err(DevError::BadState);
            }
            let taken = {
                // Keep route custody through mailbox extraction.  Reset uses
                // the same route -> mailbox order, so a waiter cannot validate
                // one generation and then take a reused raw/cookie record.
                let mut routes = self.inner.completion_routes.lock();
                let exact_routes = capability
                    .handles
                    .iter()
                    .zip(capability.cookies.iter().copied())
                    .all(|(handle, cookie)| {
                        routes.matches_exact(capability.generation, handle.raw, cookie)
                    });
                if !exact_routes {
                    // The caller cannot safely wait on an unreserved or stale
                    // identity.  Published siblings remain in route custody
                    // for reset/quarantine rather than being released as EIO.
                    let old_route_present = capability
                        .handles
                        .iter()
                        .any(|handle| routes.find(capability.generation, handle.raw).is_some());
                    if old_route_present {
                        routes.mark_quarantined();
                        self.inner
                            .completion_quarantined
                            .store(true, Ordering::Release);
                        self.notify_progress();
                    }
                    return Err(DevError::BadState);
                }
                let mut mailbox = self.inner.completions.lock();
                if mailbox.contains_quarantined() {
                    self.inner
                        .completion_quarantined
                        .store(true, Ordering::Release);
                    self.notify_progress();
                    return Err(DevError::BadState);
                }
                mailbox.take_handles_exact(capability.handles, capability.cookies, output)
            };
            let completed = match taken {
                Ok(completed) => completed,
                Err(()) => {
                    self.inner.completion_routes.lock().mark_quarantined();
                    self.inner
                        .completion_quarantined
                        .store(true, Ordering::Release);
                    self.notify_progress();
                    return Err(DevError::BadState);
                }
            };
            if completed != 0 {
                self.retire_routed_physical(
                    PhysicalRetirementCapability::Exact(capability),
                    output,
                    completed,
                )?;
                self.release_exact_routes_if_complete(capability);
                let continuation = capability
                    .handles
                    .iter()
                    .any(|handle| self.inner.completions.lock().contains_handle(handle.raw));
                return Ok(BlockCompletionDrain {
                    completed,
                    continuation,
                });
            }
            self.wait_for_exact_progress(capability.handles)?;
        }
    }

    /// Drains only kernel-destination physical routes while retaining exact
    /// synchronous records in the mailbox.  This is the broker entry point
    /// for the device-global worker; it is the only method that should call
    /// the lower mixed used-ring drain once the broker is installed.
    pub fn wait_kernel_physical_completions(
        &self,
        output: &mut [BlockCompletion],
    ) -> DevResult<BlockCompletionDrain> {
        if output.is_empty() {
            if self.completion_unavailable() {
                return Err(DevError::BadState);
            }
            return Ok(BlockCompletionDrain::default());
        }
        if !self.physical_completion_broker_installed() {
            return Err(DevError::Unsupported);
        }
        loop {
            // The completion owner protects only this bounded lower-ring
            // pass.  It must be released before sleeping for an exact-only
            // route, otherwise ordinary drains could be blocked forever by
            // a physical worker that has no Kernel record to return.
            let _owner = self.claim_completion_owner()?;
            if self.completion_unavailable() {
                return Err(DevError::BadState);
            }
            if self.inner.completions.lock().contains_quarantined() {
                self.inner
                    .completion_quarantined
                    .store(true, Ordering::Release);
                self.notify_progress();
                return Err(DevError::BadState);
            }
            // Mixed drain is the one and only lower used-ring consumer after
            // broker installation. It may cache exact records, but the
            // destination-aware helper returns only this Kernel route. The
            // route lock remains held across the lower drain and extraction,
            // so an exact publication cannot turn into a generic steal.
            let (drain, generation) =
                self.drain_destination_once(BlockPhysicalCompletionRoute::Kernel, output)?;
            let completed = drain.completed;
            if completed != 0 {
                self.retire_routed_physical(
                    PhysicalRetirementCapability::Route { generation },
                    output,
                    completed,
                )?;
                return Ok(drain);
            }
            if self.physical_completion_route_count(Some(BlockPhysicalCompletionRoute::Kernel)) == 0
            {
                // Exact routes may still be in flight. Their completion has
                // been retained in the mailbox (or is awaiting the next IRQ)
                // while this broker remains the sole lower-ring owner. Keep
                // the owner asleep on the generation queue until either a
                // kernel route appears or all routes have retired; this is a
                // real wait, not a retry/busy-poll loop.
                if self.physical_completion_route_count(None) != 0 {
                    if drain.continuation {
                        drop(_owner);
                        axtask::yield_now();
                    } else {
                        drop(_owner);
                        self.wait_for_generation_progress()?;
                    }
                    continue;
                }
                return Err(DevError::Again);
            }
            if self.inner.physical_pending.load(Ordering::Acquire) == 0 {
                return Err(DevError::Again);
            }
            if drain.continuation {
                drop(_owner);
                axtask::yield_now();
            } else {
                drop(_owner);
                self.wait_for_generation_progress()?;
            }
        }
    }

    /// Device-global physical completion owner.  It never holds the shared
    /// device mutex while waiting: each pass locks only for a bounded drain,
    /// then sleeps on the owner-local wait queue. Ordinary and physical heads
    /// therefore share one FIFO owner and cannot deadlock each other.
    pub fn wait_any_physical_completion(
        &self,
        output: &mut [BlockCompletion],
    ) -> DevResult<BlockCompletionDrain> {
        if self.physical_completion_broker_installed() {
            return self.wait_kernel_physical_completions(output);
        }
        if output.is_empty() {
            if self.completion_unavailable() {
                return Err(DevError::BadState);
            }
            return Ok(BlockCompletionDrain::default());
        }
        loop {
            // Claim the lower-ring owner only for the finite mixed-drain
            // pass.  A generic waiter must not hold it while sleeping, or an
            // ordinary waiter could never drain its own head.
            let _owner = self.claim_completion_owner()?;
            if self.completion_unavailable() {
                return Err(DevError::BadState);
            }
            if self.inner.completions.lock().contains_quarantined() {
                self.inner
                    .completion_quarantined
                    .store(true, Ordering::Release);
                self.notify_progress();
                return Err(DevError::BadState);
            }
            // There is no Kernel route before broker installation, but use
            // the same destination-aware path as the broker rather than a
            // raw physical mailbox drain.  An exact reservation can appear
            // after the caller's previous observation; the route lock spans
            // this check, lower drain, and extraction, so that completion
            // remains exact-owner custody.
            let (drain, generation) =
                self.drain_destination_once(BlockPhysicalCompletionRoute::Kernel, output)?;
            let completed = drain.completed;
            if completed != 0 {
                self.retire_routed_physical(
                    PhysicalRetirementCapability::Route { generation },
                    output,
                    completed,
                )?;
                return Ok(drain);
            }
            // The route check is deliberately after the destination-aware
            // pass.  It is only a policy result (the exact owner must use its
            // own waiter); it is never used to authorize a later unfiltered
            // drain.
            let exact_route_present = self.inner.completion_routes.lock().has_destination(
                self.completion_generation(),
                BlockPhysicalCompletionRoute::Exact,
            );
            if exact_route_present {
                return Err(DevError::Unsupported);
            }
            if self.inner.physical_pending.load(Ordering::Acquire) == 0 {
                return Err(DevError::Again);
            }
            if drain.continuation {
                drop(_owner);
                axtask::yield_now();
                continue;
            }
            drop(_owner);
            self.wait_for_progress()?;
        }
    }

    /// Performs one bounded, non-blocking drain for the device-global kernel
    /// physical route. This is the multi-device worker primitive: a worker
    /// scans each fixed device slot without sleeping on one idle device and
    /// relies on the progress notifier to wake the next pass.
    pub fn drain_physical_completions(
        &self,
        output: &mut [BlockCompletion],
    ) -> DevResult<BlockCompletionDrain> {
        if output.is_empty() {
            return Ok(BlockCompletionDrain::default());
        }
        if !self.physical_completion_broker_installed() {
            return Err(DevError::Unsupported);
        }
        let _owner = self.claim_completion_owner()?;
        if self.completion_unavailable() {
            return Err(DevError::BadState);
        }
        if self.inner.completions.lock().contains_quarantined() {
            self.inner
                .completion_quarantined
                .store(true, Ordering::Release);
            self.notify_progress();
            return Err(DevError::BadState);
        }
        let (drain, generation) =
            self.drain_destination_once(BlockPhysicalCompletionRoute::Kernel, output)?;
        if drain.completed != 0 {
            self.retire_routed_physical(
                PhysicalRetirementCapability::Route { generation },
                output,
                drain.completed,
            )?;
        }
        Ok(BlockCompletionDrain {
            completed: drain.completed,
            continuation: drain.continuation,
        })
    }

    /// Waits for exact physical handles while acting as the mandatory
    /// device-local any-drain owner.  Every bounded drain consumes mixed
    /// ordinary/physical used entries into the mailbox, so a foreign physical
    /// or ordinary head cannot block this waiter.  Only records whose raw
    /// handle *and expected cookie* match this effect are removed; all foreign
    /// records remain available for their own exact waiter/broker.
    pub fn wait_physical_completions_exact(
        &self,
        handles: &[BlockRequestHandle],
        cookies: &[u64],
        output: &mut [BlockCompletion],
    ) -> DevResult<BlockCompletionDrain> {
        if handles.is_empty() || output.is_empty() || handles.len() != cookies.len() {
            return if handles.is_empty() || handles.len() != cookies.len() {
                Err(DevError::InvalidParam)
            } else if self.completion_unavailable() {
                Err(DevError::BadState)
            } else {
                Ok(BlockCompletionDrain::default())
            };
        }
        if handles.iter().any(|handle| handle.raw == 0) || cookies.iter().any(|cookie| *cookie == 0)
        {
            self.inner
                .completion_quarantined
                .store(true, Ordering::Release);
            self.inner.completion_routes.lock().mark_quarantined();
            self.notify_progress();
            return Err(DevError::BadState);
        }
        // Bind this waiter to the transport generation observed at entry.
        // Re-reading the current generation on a later loop iteration would
        // let reset plus raw/cookie reuse turn the old wait into a new owner.
        let capability = ExactCompletionCapability {
            generation: self.completion_generation(),
            handles,
            cookies,
        };
        if self.physical_completion_broker_installed() {
            // The global broker owns every lower used-ring drain.  This
            // waiter only scans its mailbox and sleeps on the same generation
            // queue; it cannot steal a kernel route or block the broker while
            // holding the shared device mutex.
            return self.wait_exact_from_broker(capability, output);
        }
        loop {
            let _owner = self.claim_completion_owner()?;
            if self.completion_unavailable() {
                return Err(DevError::BadState);
            }
            let take = {
                // Keep route authentication and mailbox extraction under the
                // route -> mailbox lock order.  This closes the reset/reuse
                // window between an exact validation and taking the record.
                let mut routes = self.inner.completion_routes.lock();
                let exact_routes = capability
                    .handles
                    .iter()
                    .zip(capability.cookies.iter().copied())
                    .all(|(handle, cookie)| {
                        routes.matches_exact(capability.generation, handle.raw, cookie)
                    });
                if !exact_routes {
                    let old_route_present = capability
                        .handles
                        .iter()
                        .any(|handle| routes.find(capability.generation, handle.raw).is_some());
                    if old_route_present {
                        routes.mark_quarantined();
                        self.inner
                            .completion_quarantined
                            .store(true, Ordering::Release);
                        self.notify_progress();
                    }
                    return Err(DevError::BadState);
                }
                let mut mailbox = self.inner.completions.lock();
                if mailbox.contains_quarantined() {
                    // A malformed/unknown record for another effect poisons
                    // the same lower queue. Do not consume this effect's
                    // otherwise valid sibling while reset custody is needed.
                    self.inner
                        .completion_quarantined
                        .store(true, Ordering::Release);
                    self.notify_progress();
                    return Err(DevError::BadState);
                }
                mailbox.take_handles_exact(capability.handles, capability.cookies, output)
            };
            let completed = match take {
                Ok(completed) => completed,
                Err(()) => {
                    self.inner.completion_routes.lock().mark_quarantined();
                    self.inner
                        .completion_quarantined
                        .store(true, Ordering::Release);
                    self.notify_progress();
                    return Err(DevError::BadState);
                }
            };
            if completed != 0 {
                self.retire_routed_physical(
                    PhysicalRetirementCapability::Exact(capability),
                    output,
                    completed,
                )?;
                self.release_exact_routes_if_complete(capability);
                let continuation = capability
                    .handles
                    .iter()
                    .any(|handle| self.inner.completions.lock().contains_handle(handle.raw));
                return Ok(BlockCompletionDrain {
                    completed,
                    continuation,
                });
            }
            if self.completion_unavailable() {
                return Err(DevError::BadState);
            }

            // This is deliberately the mixed drain, not the lower
            // physical-only/handle FIFO helper.  It retires no caller owner;
            // records are copied into the fixed mailbox for exact routing.
            let (drain, _drain_generation) = self.drain_device_once()?;
            let take = {
                let mut routes = self.inner.completion_routes.lock();
                let exact_routes = capability
                    .handles
                    .iter()
                    .zip(capability.cookies.iter().copied())
                    .all(|(handle, cookie)| {
                        routes.matches_exact(capability.generation, handle.raw, cookie)
                    });
                if !exact_routes {
                    let old_route_present = capability
                        .handles
                        .iter()
                        .any(|handle| routes.find(capability.generation, handle.raw).is_some());
                    if old_route_present {
                        routes.mark_quarantined();
                        self.inner
                            .completion_quarantined
                            .store(true, Ordering::Release);
                        self.notify_progress();
                    }
                    return Err(DevError::BadState);
                }
                let mut mailbox = self.inner.completions.lock();
                mailbox.take_handles_exact(capability.handles, capability.cookies, output)
            };
            let completed = match take {
                Ok(completed) => completed,
                Err(()) => {
                    self.inner.completion_routes.lock().mark_quarantined();
                    self.inner
                        .completion_quarantined
                        .store(true, Ordering::Release);
                    self.notify_progress();
                    return Err(DevError::BadState);
                }
            };
            if completed != 0 {
                self.retire_routed_physical(
                    PhysicalRetirementCapability::Exact(capability),
                    output,
                    completed,
                )?;
                self.release_exact_routes_if_complete(capability);
                let continuation = drain.continuation
                    || capability
                        .handles
                        .iter()
                        .any(|handle| self.inner.completions.lock().contains_handle(handle.raw));
                return Ok(BlockCompletionDrain {
                    completed,
                    continuation,
                });
            }
            if self.completion_unavailable() {
                return Err(DevError::BadState);
            }
            if self.inner.physical_pending.load(Ordering::Acquire) == 0 {
                // An accepted exact owner disappeared without a matching
                // record.  Treat this as a lost identity/reset condition, not
                // as a successful empty drain or an ordinary EIO.
                self.inner
                    .completion_quarantined
                    .store(true, Ordering::Release);
                self.notify_progress();
                return Err(DevError::BadState);
            }
            if drain.continuation {
                drop(_owner);
                axtask::yield_now();
            } else {
                drop(_owner);
                self.wait_for_exact_progress(capability.handles)?;
            }
        }
    }

    fn wait_handle(
        &self,
        handle: BlockRequestHandle,
        expected_owner: BlockCompletionOwner,
    ) -> DevResult {
        loop {
            let _owner = self.claim_completion_owner()?;
            if self.completion_unavailable() {
                return Err(DevError::BadState);
            }
            if self
                .inner
                .completions
                .lock()
                .handle_has_invalid_identity(handle.raw, expected_owner)
            {
                self.inner
                    .completion_quarantined
                    .store(true, Ordering::Release);
                self.notify_progress();
                return Err(DevError::BadState);
            }
            if let Some(record) = self.inner.completions.lock().take_handle(handle.raw) {
                self.consume_completion_credits(1)?;
                if record.handle.raw != handle.raw || record.cookie == 0 {
                    self.inner
                        .completion_quarantined
                        .store(true, Ordering::Release);
                    return Err(DevError::BadState);
                }
                if record.owner == BlockCompletionOwner::Physical {
                    let _ = self.inner.physical_pending.try_update(
                        Ordering::AcqRel,
                        Ordering::Acquire,
                        |pending| Some(pending.saturating_sub(1)),
                    );
                }
                self.notify_progress();
                return match record.status {
                    BlockCompletionStatus::Success => Ok(()),
                    BlockCompletionStatus::DeviceError(_) => Err(DevError::Io),
                    BlockCompletionStatus::Quarantined => Err(DevError::BadState),
                };
            }
            if self.completion_unavailable() {
                return Err(DevError::BadState);
            }
            let (drain, _drain_generation) = self.drain_device_once()?;
            if self
                .inner
                .completions
                .lock()
                .handle_has_invalid_identity(handle.raw, expected_owner)
            {
                self.inner
                    .completion_quarantined
                    .store(true, Ordering::Release);
                self.notify_progress();
                return Err(DevError::BadState);
            }
            if let Some(record) = self.inner.completions.lock().take_handle(handle.raw) {
                self.consume_completion_credits(1)?;
                if record.handle.raw != handle.raw || record.cookie == 0 {
                    self.inner
                        .completion_quarantined
                        .store(true, Ordering::Release);
                    return Err(DevError::BadState);
                }
                if record.owner == BlockCompletionOwner::Physical {
                    let _ = self.inner.physical_pending.try_update(
                        Ordering::AcqRel,
                        Ordering::Acquire,
                        |pending| Some(pending.saturating_sub(1)),
                    );
                }
                self.notify_progress();
                return match record.status {
                    BlockCompletionStatus::Success => Ok(()),
                    BlockCompletionStatus::DeviceError(_) => Err(DevError::Io),
                    BlockCompletionStatus::Quarantined => Err(DevError::BadState),
                };
            }
            if drain.continuation {
                drop(_owner);
                axtask::yield_now();
                continue;
            }
            drop(_owner);
            self.wait_for_handle_progress(handle)?;
        }
    }

    /// Exact ordinary/physical handle wait without retaining the outer mutex.
    pub fn wait_async_all_owned(&self, handles: &[BlockRequestHandle]) -> DevResult {
        let mut first_error = None;
        for handle in handles.iter().copied() {
            if let Err(error) = self.wait_handle(handle, BlockCompletionOwner::Ordinary) {
                // A device-status error retires exactly one handle but must
                // not prevent the remaining submitted handles from reaching
                // their own exact terminal observation. Typed quarantine,
                // malformed identity, and transport errors invalidate the
                // shared owner proof and therefore stop immediately.
                if matches!(error, DevError::Io) {
                    first_error.get_or_insert(error);
                } else {
                    return Err(error);
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    unsafe fn physical_sg_owned(
        &self,
        block_id: u64,
        op: BlockAsyncOp,
        segments: &[BlockPhysicalSegment],
    ) -> DevResult<BlockPhysicalSgOutcome> {
        if self.completion_unavailable() {
            return Err(DevError::BadState);
        }
        if segments.is_empty() {
            return Err(DevError::InvalidParam);
        }
        let mut request = BlockPhysicalRequest {
            block_id,
            op,
            segments,
            handle: None,
            cookie: None,
        };
        // Keep the reservation visible to this adapter so a post-publication
        // malformed identity can be returned as typed custody transfer.  The
        // caller must never drop its pin merely because a lower handle was
        // malformed after the descriptor became visible.
        let mut reservation =
            match self.reserve_physical_completion_routes(BlockPhysicalCompletionRoute::Exact, 1) {
                Ok(reservation) => reservation,
                // No descriptor exists yet when the fixed route table cannot
                // admit this request.  Preserve the physical-SG fallback result
                // instead of leaking a route-capacity error into the caller.
                Err(DevError::ResourceBusy | DevError::Unsupported | DevError::NoMemory) => {
                    return Ok(BlockPhysicalSgOutcome::NotSubmitted);
                }
                Err(error) => return Err(error),
            };
        // SAFETY: the caller's physical pin/direction contract is forwarded
        // unchanged through the pre-publication exact route reservation.
        let report = unsafe {
            self.submit_physical_batch_reserved(
                &mut reservation,
                core::slice::from_mut(&mut request),
            )
        };
        let report = match report {
            Ok(report) => report,
            Err(DevError::Unsupported | DevError::ResourceBusy | DevError::NoMemory) => {
                return Ok(BlockPhysicalSgOutcome::NotSubmitted);
            }
            Err(_) if reservation.is_committed() => {
                // Publication has happened; the reservation owns the exact
                // lower route even though its identity is no longer usable.
                // Preserve it for reset/quarantine instead of returning an
                // ordinary error that permits the DMA owner to be dropped.
                return Ok(BlockPhysicalSgOutcome::Quarantined);
            }
            Err(error) => return Err(error),
        };
        if report.submitted == 0 {
            return Ok(BlockPhysicalSgOutcome::NotSubmitted);
        }
        if report.submitted != 1 {
            self.inner
                .completion_quarantined
                .store(true, Ordering::Release);
            return Ok(BlockPhysicalSgOutcome::Quarantined);
        }
        let (Some(handle), Some(cookie)) = (request.handle, request.cookie) else {
            self.inner
                .completion_quarantined
                .store(true, Ordering::Release);
            return Ok(BlockPhysicalSgOutcome::Quarantined);
        };
        if handle.raw == 0 || cookie == 0 {
            self.inner
                .completion_quarantined
                .store(true, Ordering::Release);
            return Ok(BlockPhysicalSgOutcome::Quarantined);
        }
        self.notify_progress();
        let mut completion = [BlockCompletion {
            handle: BlockRequestHandle { raw: 0 },
            owner: BlockCompletionOwner::Physical,
            cookie: 0,
            status: BlockCompletionStatus::Quarantined,
            bytes: 0,
        }];
        let drain = match self.wait_physical_completions_exact(
            core::slice::from_ref(&handle),
            core::slice::from_ref(&cookie),
            &mut completion,
        ) {
            Ok(drain) => drain,
            // Any other post-publication result leaves ownership uncertain
            // (including reset, lost wake, or a non-blocking caller).  Keep
            // the queue fail-closed and return a typed custody transfer, not
            // an error that lets the caller drop its DMA pin.
            Err(error) => {
                self.inner
                    .completion_quarantined
                    .store(true, Ordering::Release);
                self.notify_progress();
                let _ = error;
                return Ok(BlockPhysicalSgOutcome::Quarantined);
            }
        };
        if drain.completed != 1
            || completion[0].handle.raw != handle.raw
            || completion[0].cookie != cookie
            || completion[0].owner != BlockCompletionOwner::Physical
        {
            self.inner
                .completion_quarantined
                .store(true, Ordering::Release);
            self.notify_progress();
            return Ok(BlockPhysicalSgOutcome::Quarantined);
        }
        match completion[0].status {
            BlockCompletionStatus::Success => Ok(BlockPhysicalSgOutcome::Completed),
            // A concrete device status retires this exact owner and is safe
            // to report as an ordinary logical failure.
            BlockCompletionStatus::DeviceError(_) => Err(DevError::Io),
            BlockCompletionStatus::Quarantined => {
                self.inner
                    .completion_quarantined
                    .store(true, Ordering::Release);
                self.notify_progress();
                Ok(BlockPhysicalSgOutcome::Quarantined)
            }
        }
    }

    fn submit_sync_batch_owned(
        &self,
        requests: &mut [BlockQueueRequest<'_>],
    ) -> DevResult<Option<BlockSubmitReport>> {
        if requests.is_empty() {
            return Ok(Some(BlockSubmitReport::default()));
        }
        // A batch larger than the fixed completion-credit slab can never make
        // progress as one publication.  It is a permanent admission error,
        // not backpressure to sleep and retry.
        if requests.len() > COMPLETION_MAILBOX_CAPACITY {
            return Err(DevError::ResourceBusy);
        }

        loop {
            // Snapshot before taking either publication lock.  If a physical
            // completion races this attempt, the generation check below
            // retries without sleeping; otherwise the wait queue's
            // pre/post-listener checks make the edge race-safe.
            let observed_generation = self.inner.completion_generation.load(Ordering::Acquire);
            let mut credit_backpressure = false;
            let report = {
                // Keep both locks only for the finite publication operation.
                // The mailbox credit prevents a completed lower slot from
                // being retired when there is no bounded owner storage left.
                let mut device = self.inner.device.lock();
                let _mailbox = self.inner.completions.lock();
                if self.completion_unavailable() {
                    return Err(DevError::BadState);
                }
                if let Err(error) = self.reserve_completion_credits(requests.len()) {
                    if matches!(error, DevError::ResourceBusy) {
                        credit_backpressure = true;
                        None
                    } else {
                        return Err(error);
                    }
                } else {
                    let report = device.submit_sync_batch(requests);
                    if let Ok(report) = report {
                        if report.submitted <= requests.len() {
                            self.publish_completion_credits(report.submitted);
                        }
                    }
                    Some(report)
                }
            };

            if credit_backpressure {
                self.wait_for_generation_progress_since(observed_generation)?;
                continue;
            }
            let Some(report) = report else {
                unreachable!("sync submission attempt must produce a result");
            };
            let report = match report {
                Ok(report) if report.submitted == 0 => {
                    if report.bytes != 0 {
                        self.inner
                            .completion_quarantined
                            .store(true, Ordering::Release);
                        self.notify_progress();
                        return Err(DevError::BadState);
                    }
                    if report.queue_full {
                        // Only an entirely unpublished prefix may be retried.
                        // A lower driver that leaves any handle behind has
                        // violated the admission boundary; quarantine it
                        // instead of submitting the same owner twice.
                        if !sync_submit_unpublished_queue_full(
                            &report,
                            requests.iter().all(|request| request.handle.is_none()),
                        ) {
                            self.inner
                                .completion_quarantined
                                .store(true, Ordering::Release);
                            self.notify_progress();
                            return Err(DevError::BadState);
                        }
                        // The lower queue may already have used entries while
                        // this task is the only upper completion worker. The
                        // publication locks are out of scope here, so claim
                        // the shared owner for one route-authenticated mixed
                        // drain. It only moves records into the mailbox;
                        // physical records remain in physical-owner custody.
                        let (drain, _generation) = {
                            let _owner = self.claim_completion_owner()?;
                            self.drain_device_once()?
                        };
                        if sync_submit_queue_full_drain_progressed(drain) {
                            continue;
                        }
                        // No lower progress was available. Do not retry on a
                        // fixed timer or busy-poll; retain the existing
                        // generation wait and its lost-wake protection.
                        self.wait_for_generation_progress_since(observed_generation)?;
                        continue;
                    }
                    return Ok(None);
                }
                Ok(report) => report,
                Err(DevError::Unsupported) => return Ok(None),
                Err(error) => return Err(error),
            };
            if report.submitted > requests.len() {
                self.inner
                    .completion_quarantined
                    .store(true, Ordering::Release);
                self.notify_progress();
                return Err(DevError::BadState);
            }
            if requests
                .iter()
                .take(report.submitted)
                .any(|request| request.handle.is_none())
            {
                // Publication already happened, so there is no safe
                // fallback. Keep the lower owner quarantined until an
                // explicit reset path.
                self.inner
                    .completion_quarantined
                    .store(true, Ordering::Release);
                self.notify_progress();
                return Err(DevError::BadState);
            }
            return Ok(Some(report));
        }
    }

    fn read_block_owned(&self, block_id: u64, buf: &mut [u8]) -> DevResult {
        if buf.is_empty() {
            return Ok(());
        }
        if let Some(result) =
            self.try_legacy_sync(|device| BlockDriverOps::read_block(device, block_id, buf))
        {
            return result;
        }
        let segment = BlockSegment::from_read_buf(buf);
        let mut request = BlockQueueRequest {
            op: BlockAsyncOp::Read,
            block_id,
            segments: core::slice::from_ref(&segment),
            handle: None,
        };
        if let Some(report) = self.submit_sync_batch_owned(core::slice::from_mut(&mut request))? {
            if report.submitted != 1 || report.bytes != buf.len() {
                self.inner
                    .completion_quarantined
                    .store(true, Ordering::Release);
                self.notify_progress();
                return Err(DevError::BadState);
            }
            let handle = request.handle.ok_or(DevError::BadState)?;
            return self.wait_async_all_owned(core::slice::from_ref(&handle));
        }
        self.try_legacy_sync(|device| BlockDriverOps::read_block(device, block_id, buf))
            .unwrap_or(Err(DevError::BadState))
    }

    fn write_block_owned(&self, block_id: u64, buf: &[u8]) -> DevResult {
        if buf.is_empty() {
            return Ok(());
        }
        if let Some(result) =
            self.try_legacy_sync(|device| BlockDriverOps::write_block(device, block_id, buf))
        {
            return result;
        }
        let segment = BlockSegment::from_write_buf(buf);
        let mut request = BlockQueueRequest {
            op: BlockAsyncOp::Write,
            block_id,
            segments: core::slice::from_ref(&segment),
            handle: None,
        };
        if let Some(report) = self.submit_sync_batch_owned(core::slice::from_mut(&mut request))? {
            if report.submitted != 1 || report.bytes != buf.len() {
                self.inner
                    .completion_quarantined
                    .store(true, Ordering::Release);
                self.notify_progress();
                return Err(DevError::BadState);
            }
            let handle = request.handle.ok_or(DevError::BadState)?;
            return self.wait_async_all_owned(core::slice::from_ref(&handle));
        }
        self.try_legacy_sync(|device| BlockDriverOps::write_block(device, block_id, buf))
            .unwrap_or(Err(DevError::BadState))
    }

    fn read_block_vectored_owned(&self, block_id: u64, bufs: &mut [&mut [u8]]) -> DevResult {
        let mut segments = Vec::with_capacity(bufs.len());
        for buf in bufs.iter_mut() {
            if !buf.is_empty() {
                segments.push(BlockSegment::from_read_buf(buf));
            }
        }
        let bytes = segments
            .iter()
            .try_fold(0usize, |total, segment| total.checked_add(segment.len))
            .ok_or(DevError::InvalidParam)?;
        if bytes == 0 {
            return Ok(());
        }
        if let Some(result) = self
            .try_legacy_sync(|device| BlockDriverOps::read_block_vectored(device, block_id, bufs))
        {
            return result;
        }
        let mut request = BlockQueueRequest {
            op: BlockAsyncOp::Read,
            block_id,
            segments: &segments,
            handle: None,
        };
        if let Some(report) = self.submit_sync_batch_owned(core::slice::from_mut(&mut request))? {
            if report.submitted != 1 || report.bytes != bytes {
                self.inner
                    .completion_quarantined
                    .store(true, Ordering::Release);
                self.notify_progress();
                return Err(DevError::BadState);
            }
            let handle = request.handle.ok_or(DevError::BadState)?;
            return self.wait_async_all_owned(core::slice::from_ref(&handle));
        }
        self.try_legacy_sync(|device| BlockDriverOps::read_block_vectored(device, block_id, bufs))
            .unwrap_or(Err(DevError::BadState))
    }

    fn write_block_vectored_owned(&self, block_id: u64, bufs: &[&[u8]]) -> DevResult {
        let mut segments = Vec::with_capacity(bufs.len());
        for buf in bufs.iter().copied() {
            if !buf.is_empty() {
                segments.push(BlockSegment::from_write_buf(buf));
            }
        }
        let bytes = segments
            .iter()
            .try_fold(0usize, |total, segment| total.checked_add(segment.len))
            .ok_or(DevError::InvalidParam)?;
        if bytes == 0 {
            return Ok(());
        }
        if let Some(result) = self
            .try_legacy_sync(|device| BlockDriverOps::write_block_vectored(device, block_id, bufs))
        {
            return result;
        }
        let mut request = BlockQueueRequest {
            op: BlockAsyncOp::Write,
            block_id,
            segments: &segments,
            handle: None,
        };
        if let Some(report) = self.submit_sync_batch_owned(core::slice::from_mut(&mut request))? {
            if report.submitted != 1 || report.bytes != bytes {
                self.inner
                    .completion_quarantined
                    .store(true, Ordering::Release);
                self.notify_progress();
                return Err(DevError::BadState);
            }
            let handle = request.handle.ok_or(DevError::BadState)?;
            return self.wait_async_all_owned(core::slice::from_ref(&handle));
        }
        self.try_legacy_sync(|device| BlockDriverOps::write_block_vectored(device, block_id, bufs))
            .unwrap_or(Err(DevError::BadState))
    }

    /// Direct physical read that publishes under the finite device lock and
    /// waits through the shared completion owner.
    pub unsafe fn read_block_physical_sg(
        &self,
        block_id: u64,
        segments: &[BlockPhysicalSegment],
    ) -> DevResult<BlockPhysicalSgOutcome> {
        // SAFETY: forwarded to `physical_sg_owned` with the same caller
        // lifetime and DMA-direction contract.
        unsafe { self.physical_sg_owned(block_id, BlockAsyncOp::Read, segments) }
    }

    /// Direct physical write counterpart to
    /// [`Self::read_block_physical_sg`].
    pub unsafe fn write_block_physical_sg(
        &self,
        block_id: u64,
        segments: &[BlockPhysicalSegment],
    ) -> DevResult<BlockPhysicalSgOutcome> {
        // SAFETY: see the read wrapper above.
        unsafe { self.physical_sg_owned(block_id, BlockAsyncOp::Write, segments) }
    }

    /// Publishes a physical batch through a route reserved before the first
    /// descriptor becomes visible.  Kernel routes are consumed by the global
    /// worker; exact routes are consumed only by their handle/cookie waiter.
    /// The route table is fixed-capacity and never performs a per-completion
    /// allocation or hash lookup.
    pub unsafe fn submit_physical_batch_reserved(
        &self,
        reservation: &mut BlockPhysicalRouteReservation,
        requests: &mut [BlockPhysicalRequest<'_>],
    ) -> DevResult<BlockSubmitReport> {
        if !Arc::ptr_eq(&self.inner, &reservation.device.inner)
            || reservation.committed
            || requests.len() != reservation.len
            || requests.is_empty()
        {
            return Err(DevError::InvalidParam);
        }
        if self.completion_unavailable() || self.completion_generation() != reservation.generation {
            return Err(DevError::BadState);
        }
        // Hold the route table through the finite lower publication so an
        // IRQ/task drain cannot observe a still-Reserved slot after the
        // descriptor has become visible.
        let mut routes = self.inner.completion_routes.lock();
        if !routes.reservation_prefix_matches(
            reservation.group,
            reservation.generation,
            reservation.destination,
            reservation.len,
        ) {
            return Err(DevError::BadState);
        }

        let lower_result = {
            let mut device = self.inner.device.lock();
            let _mailbox = self.inner.completions.lock();
            if self.completion_unavailable()
                || self.completion_generation() != reservation.generation
            {
                return Err(DevError::BadState);
            }
            self.reserve_completion_credits(requests.len())?;
            // SAFETY: the caller's pin/lifetime contract covers the returned
            // handles, exactly as required by the lower driver trait.
            let lower = unsafe { device.submit_physical_batch(requests) };
            match lower {
                Ok(report) => {
                    if report.submitted <= requests.len() && report.submitted != 0 {
                        self.inner
                            .physical_pending
                            .fetch_add(report.submitted, Ordering::AcqRel);
                        self.publish_completion_credits(report.submitted);
                    }
                    Ok(report)
                }
                Err(error) => Err(error),
            }
        };

        let report = match lower_result {
            Ok(report) => report,
            Err(
                error @ (DevError::Again
                | DevError::InvalidParam
                | DevError::ResourceBusy
                | DevError::Unsupported
                | DevError::NoMemory),
            ) => {
                // These errors are admission/validation failures under the
                // BlockDriverOps contract: an error return cannot retain a
                // published descriptor. Release only the still-Reserved
                // child cells, leaving fallback/error selection to the caller.
                if !routes.release_reserved(
                    reservation.group,
                    reservation.generation,
                    reservation.destination,
                    0,
                    reservation.len,
                ) {
                    routes.mark_group_quarantined(reservation.group);
                    reservation.committed = true;
                    self.inner
                        .completion_quarantined
                        .store(true, Ordering::Release);
                    self.notify_progress();
                    return Err(DevError::BadState);
                }
                return Err(error);
            }
            Err(error) => {
                // A non-admission error cannot prove that the lower driver
                // did not publish. Keep all reserved owner identities under
                // typed quarantine and never let Drop turn them into a
                // fallback release.
                routes.mark_group_quarantined(reservation.group);
                reservation.committed = true;
                self.inner
                    .completion_quarantined
                    .store(true, Ordering::Release);
                self.notify_progress();
                return Err(error);
            }
        };

        if report.submitted == 0 {
            if report.bytes != 0 {
                routes.mark_group_quarantined(reservation.group);
                reservation.committed = true;
                self.inner
                    .completion_quarantined
                    .store(true, Ordering::Release);
                self.notify_progress();
                return Err(DevError::BadState);
            }
            if !routes.release_reserved(
                reservation.group,
                reservation.generation,
                reservation.destination,
                0,
                reservation.len,
            ) {
                routes.mark_group_quarantined(reservation.group);
                reservation.committed = true;
                self.inner
                    .completion_quarantined
                    .store(true, Ordering::Release);
                self.notify_progress();
                return Err(DevError::BadState);
            }
            return Ok(report);
        }

        let accepted = report.submitted.min(requests.len());
        let malformed = report.submitted > requests.len()
            || requests[..accepted].iter().any(|request| {
                request.handle.is_none_or(|handle| handle.raw == 0)
                    || request.cookie.is_none_or(|c| c == 0)
            });
        if malformed {
            routes.mark_group_quarantined(reservation.group);
            reservation.committed = true;
            self.inner
                .completion_quarantined
                .store(true, Ordering::Release);
            self.notify_progress();
            return Err(DevError::BadState);
        }

        let published = routes.mark_published(
            reservation.group,
            reservation.generation,
            reservation.destination,
            &requests[..accepted],
        );
        if !published {
            routes.mark_group_quarantined(reservation.group);
            reservation.committed = true;
            self.inner
                .completion_quarantined
                .store(true, Ordering::Release);
            self.notify_progress();
            return Err(DevError::BadState);
        }
        if accepted < reservation.len {
            if !routes.release_reserved(
                reservation.group,
                reservation.generation,
                reservation.destination,
                accepted,
                reservation.len,
            ) {
                routes.mark_group_quarantined(reservation.group);
                reservation.committed = true;
                self.inner
                    .completion_quarantined
                    .store(true, Ordering::Release);
                self.notify_progress();
                return Err(DevError::BadState);
            }
        }
        reservation.committed = true;
        drop(routes);
        self.notify_progress();
        Ok(report)
    }

    /// Prepares and publishes an exact physical batch.  This is the normal
    /// ext4 synchronous path; it shares the device-global broker when one is
    /// installed and never starts a competing used-ring drain.
    pub unsafe fn submit_physical_batch_exact(
        &self,
        requests: &mut [BlockPhysicalRequest<'_>],
    ) -> DevResult<BlockSubmitReport> {
        if requests.is_empty() {
            return Ok(BlockSubmitReport::default());
        }
        let mut reservation = self.reserve_physical_completion_routes(
            BlockPhysicalCompletionRoute::Exact,
            requests.len(),
        )?;
        // SAFETY: forwarded under the reservation's original pin/lifetime
        // contract; publication cannot fall back after the lower call.
        unsafe { self.submit_physical_batch_reserved(&mut reservation, requests) }
    }

    /// Prepares and publishes a route owned by the global asynchronous
    /// worker.  The worker must install the broker before calling this API.
    pub unsafe fn submit_physical_batch_kernel(
        &self,
        requests: &mut [BlockPhysicalRequest<'_>],
    ) -> DevResult<BlockSubmitReport> {
        if requests.is_empty() {
            return Ok(BlockSubmitReport::default());
        }
        let mut reservation = self.reserve_physical_completion_routes(
            BlockPhysicalCompletionRoute::Kernel,
            requests.len(),
        )?;
        // SAFETY: see `submit_physical_batch_exact`.
        unsafe { self.submit_physical_batch_reserved(&mut reservation, requests) }
    }

    /// Backwards-compatible exact route for lower callers that do not need
    /// to name the destination explicitly.  It still reserves the route
    /// before publication; callers cannot create an unowned physical request.
    pub unsafe fn submit_physical_batch(
        &self,
        requests: &mut [BlockPhysicalRequest<'_>],
    ) -> DevResult<BlockSubmitReport> {
        // SAFETY: this wrapper uses the same exact route and pin contract.
        unsafe { self.submit_physical_batch_exact(requests) }
    }

    fn drain_owned(&self, output: &mut [BlockCompletion]) -> DevResult<BlockCompletionDrain> {
        if output.is_empty() {
            return Ok(BlockCompletionDrain::default());
        }
        let _owner = self.claim_completion_owner()?;
        loop {
            if self.completion_unavailable() {
                return Err(DevError::BadState);
            }
            if self.inner.completions.lock().contains_quarantined() {
                self.inner
                    .completion_quarantined
                    .store(true, Ordering::Release);
                self.notify_progress();
                return Err(DevError::BadState);
            }
            let completed = self.inner.completions.lock().take_ordinary(output);
            if completed != 0 {
                self.consume_completion_credits(completed)?;
                self.notify_progress();
                return Ok(BlockCompletionDrain {
                    completed,
                    continuation: self.inner.completions.lock().contains_ordinary(),
                });
            }
            let (drain, _drain_generation) = self.drain_device_once()?;
            let completed = self.inner.completions.lock().take_ordinary(output);
            if completed != 0 {
                self.consume_completion_credits(completed)?;
                self.notify_progress();
                return Ok(BlockCompletionDrain {
                    completed,
                    continuation: drain.continuation
                        && self.inner.completions.lock().contains_ordinary(),
                });
            }
            if !drain.continuation || !self.inner.completions.lock().contains_ordinary() {
                // A physical-only mailbox/used-ring head belongs to the
                // physical owner. Ordinary polling must not spin forever on
                // that class merely because the shared lower drain made
                // progress.
                return Ok(BlockCompletionDrain {
                    completed: 0,
                    continuation: false,
                });
            }
            axtask::yield_now();
        }
    }

    pub fn byte_len(&self) -> u64 {
        self.num_blocks().saturating_mul(self.block_size() as u64)
    }

    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> DevResult<usize> {
        if buf.is_empty() || offset >= self.byte_len() {
            return Ok(0);
        }
        let len = min(buf.len() as u64, self.byte_len() - offset) as usize;
        read_at_owned(self, offset, &mut buf[..len])?;
        Ok(len)
    }

    pub fn write_at(&self, offset: u64, buf: &[u8]) -> DevResult<usize> {
        if buf.is_empty() || offset >= self.byte_len() {
            return Ok(0);
        }
        let len = min(buf.len() as u64, self.byte_len() - offset) as usize;
        write_at_owned(self, offset, &buf[..len])?;
        Ok(len)
    }
}

fn read_at_owned(device: &SharedBlockDevice, offset: u64, buf: &mut [u8]) -> DevResult {
    let block_size = device.block_size();
    if block_size == 0 {
        return Err(DevError::InvalidParam);
    }

    let mut done = 0;
    let mut block = offset / block_size as u64;
    let block_offset = offset as usize % block_size;
    if block_offset != 0 {
        let mut scratch = vec![0; block_size];
        device.read_block_owned(block, &mut scratch)?;
        let copied = min(buf.len(), block_size - block_offset);
        buf[..copied].copy_from_slice(&scratch[block_offset..block_offset + copied]);
        done += copied;
        block += 1;
    }

    let full_bytes = (buf.len() - done) / block_size * block_size;
    if full_bytes != 0 {
        device.read_block_owned(block, &mut buf[done..done + full_bytes])?;
        done += full_bytes;
        block += (full_bytes / block_size) as u64;
    }

    if done != buf.len() {
        let mut scratch = vec![0; block_size];
        device.read_block_owned(block, &mut scratch)?;
        let tail = buf.len() - done;
        buf[done..].copy_from_slice(&scratch[..tail]);
    }
    Ok(())
}

fn write_at_owned(device: &SharedBlockDevice, offset: u64, buf: &[u8]) -> DevResult {
    let block_size = device.block_size();
    if block_size == 0 {
        return Err(DevError::InvalidParam);
    }

    let mut done = 0;
    let mut block = offset / block_size as u64;
    let block_offset = offset as usize % block_size;
    if block_offset != 0 {
        let mut scratch = vec![0; block_size];
        device.read_block_owned(block, &mut scratch)?;
        let copied = min(buf.len(), block_size - block_offset);
        scratch[block_offset..block_offset + copied].copy_from_slice(&buf[..copied]);
        device.write_block_owned(block, &scratch)?;
        done += copied;
        block += 1;
    }

    let full_bytes = (buf.len() - done) / block_size * block_size;
    if full_bytes != 0 {
        device.write_block_owned(block, &buf[done..done + full_bytes])?;
        done += full_bytes;
        block += (full_bytes / block_size) as u64;
    }

    if done != buf.len() {
        let mut scratch = vec![0; block_size];
        device.read_block_owned(block, &mut scratch)?;
        let tail = buf.len() - done;
        scratch[..tail].copy_from_slice(&buf[done..]);
        device.write_block_owned(block, &scratch)?;
    }
    Ok(())
}

impl BaseDriverOps for SharedBlockDevice {
    fn device_name(&self) -> &str {
        &self.inner.name
    }

    fn device_type(&self) -> DeviceType {
        self.inner.device_type
    }

    fn irq_num(&self) -> Option<usize> {
        self.inner.irq
    }
}

impl BlockDriverOps for SharedBlockDevice {
    fn num_blocks(&self) -> u64 {
        self.lock_raw().num_blocks()
    }

    fn block_size(&self) -> usize {
        self.lock_raw().block_size()
    }

    fn read_block(&mut self, block_id: u64, buf: &mut [u8]) -> DevResult {
        self.read_block_owned(block_id, buf)
    }

    fn read_block_vectored(&mut self, block_id: u64, bufs: &mut [&mut [u8]]) -> DevResult {
        self.read_block_vectored_owned(block_id, bufs)
    }

    fn write_block(&mut self, block_id: u64, buf: &[u8]) -> DevResult {
        self.write_block_owned(block_id, buf)
    }

    fn write_block_vectored(&mut self, block_id: u64, bufs: &[&[u8]]) -> DevResult {
        self.write_block_vectored_owned(block_id, bufs)
    }

    /// # Safety
    ///
    /// The caller must keep the physical segments pinned and valid, avoid
    /// concurrent CPU access, and obey the device-write direction of a read.
    unsafe fn read_block_physical_sg(
        &mut self,
        block_id: u64,
        segments: &[crate::prelude::BlockPhysicalSegment],
    ) -> DevResult<axdriver_block::BlockPhysicalSgOutcome> {
        // SAFETY: the helper publishes under the lock but waits through the
        // device-global owner after releasing it.
        unsafe { self.physical_sg_owned(block_id, BlockAsyncOp::Read, segments) }
    }

    /// # Safety
    ///
    /// The caller must keep the physical segments pinned and valid, avoid
    /// concurrent CPU access, and obey the device-read direction of a write.
    unsafe fn write_block_physical_sg(
        &mut self,
        block_id: u64,
        segments: &[crate::prelude::BlockPhysicalSegment],
    ) -> DevResult<axdriver_block::BlockPhysicalSgOutcome> {
        // SAFETY: see the read path above.
        unsafe { self.physical_sg_owned(block_id, BlockAsyncOp::Write, segments) }
    }

    fn flush(&mut self) -> DevResult {
        if let Some(result) = self.try_legacy_sync(BlockDriverOps::flush) {
            return result;
        }
        let segments: [BlockSegment; 0] = [];
        let mut request = BlockQueueRequest {
            op: BlockAsyncOp::Flush,
            block_id: 0,
            segments: &segments,
            handle: None,
        };
        if let Some(report) = self.submit_sync_batch_owned(core::slice::from_mut(&mut request))? {
            if report.submitted != 1 || report.bytes != 0 {
                self.inner
                    .completion_quarantined
                    .store(true, Ordering::Release);
                self.notify_progress();
                return Err(DevError::BadState);
            }
            let handle = request.handle.ok_or(DevError::BadState)?;
            return self.wait_async_all_owned(core::slice::from_ref(&handle));
        }
        self.try_legacy_sync(BlockDriverOps::flush)
            .unwrap_or(Err(DevError::BadState))
    }

    fn async_queue_caps(&self) -> Option<BlockQueueCaps> {
        if self.completion_unavailable() {
            return None;
        }
        self.lock_raw().async_queue_caps()
    }

    fn submit_async_batch(
        &mut self,
        requests: &mut [BlockQueueRequest<'_>],
    ) -> DevResult<BlockSubmitReport> {
        if requests.is_empty() {
            return Ok(BlockSubmitReport::default());
        }
        let report = {
            let mut device = self.inner.device.lock();
            let _mailbox = self.inner.completions.lock();
            if self.completion_unavailable() {
                return Err(DevError::BadState);
            }
            if self.reserve_completion_credits(requests.len()).is_err() {
                return Err(DevError::ResourceBusy);
            }
            let report = device.submit_async_batch(requests)?;
            if report.submitted <= requests.len() {
                self.publish_completion_credits(report.submitted);
            }
            report
        };
        if report.submitted > requests.len()
            || (report.submitted == 0 && report.bytes != 0)
            || requests
                .iter()
                .take(report.submitted)
                .any(|request| request.handle.is_none())
        {
            self.inner
                .completion_quarantined
                .store(true, Ordering::Release);
            self.notify_progress();
            return Err(DevError::BadState);
        }
        Ok(report)
    }

    fn submit_sync_batch(
        &mut self,
        requests: &mut [BlockQueueRequest<'_>],
    ) -> DevResult<BlockSubmitReport> {
        SharedBlockDevice::submit_sync_batch_owned(self, requests)?.ok_or(DevError::Unsupported)
    }

    unsafe fn submit_physical_batch(
        &mut self,
        requests: &mut [BlockPhysicalRequest<'_>],
    ) -> DevResult<BlockSubmitReport> {
        // SAFETY: the shared lock serializes queue ownership while preserving
        // the caller's physical pin and lifetime contract.
        // SAFETY: this trait bridge preserves the caller's physical pin
        // contract and uses the finite publication helper.
        unsafe { SharedBlockDevice::submit_physical_batch(self, requests) }
    }

    fn drain_async_completions(
        &mut self,
        output: &mut [BlockCompletion],
    ) -> DevResult<BlockCompletionDrain> {
        self.drain_owned(output)
    }

    fn wait_any_physical_completion(
        &mut self,
        output: &mut [BlockCompletion],
    ) -> DevResult<BlockCompletionDrain> {
        SharedBlockDevice::wait_any_physical_completion(self, output)
    }

    fn reset_device(&mut self) -> DevResult<BlockResetOutcome> {
        // Cancel any in-flight completion owner before touching lower queue
        // state. The owner observes this typed quarantine marker, wakes, and
        // drops its guard; reset then becomes the unique device owner for the
        // finite quiescence proof and mailbox retirement.
        self.inner
            .completion_quarantined
            .store(true, Ordering::Release);
        self.inner.completion_routes.lock().mark_quarantined();
        self.inner
            .completion_transport_generation
            .fetch_add(1, Ordering::AcqRel);
        self.notify_progress();
        // Close both delivery paths before waiting for the unique drain
        // owner.  Reset can be requested from a non-blocking context; in
        // that case the early `Again` return must still leave late IRQs
        // unable to manufacture progress for the cancelled generation.
        let (notifier_was_installed, irq_was_enabled) = {
            let mut device = self.inner.device.lock();
            let irq_was_enabled = device.is_irq_enabled();
            let _ = device.disable_irq();
            let notifier_was_installed = device.install_completion_notifier(None, 0).is_ok();
            (notifier_was_installed, irq_was_enabled)
        };
        let _owner = self.claim_completion_owner()?;

        // Close the callback endpoint and wait for any in-flight callback
        // reader before resetting the lower queue.  A callback that raced
        // the IRQ-disable edge is therefore completed while the Arc context
        // is still live, and a late IRQ after reset cannot create a fresh
        // completion generation.  Reinstall only after a proven quiescence;
        // a quarantined device stays closed until an explicit recovery path.
        let outcome = self.lock_raw().reset_device();
        self.inner
            .completion_generation
            .fetch_add(1, Ordering::AcqRel);
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                // IRQ delivery is already closed and the generation has
                // already been cancelled. Preserve the quarantine marker and
                // wake every waiter even when the lower reset itself fails.
                self.inner.completion_waiters.notify_many(usize::MAX, false);
                self.notify_terminal();
                return Err(error);
            }
        };
        match outcome {
            BlockResetOutcome::Quiesced => {
                let reenabled = {
                    let mut device = self.inner.device.lock();
                    let notifier_ok = !notifier_was_installed
                        || device
                            .install_completion_notifier(
                                Some(shared_completion_notify as BlockCompletionNotifier),
                                Arc::as_ptr(&self.inner) as usize,
                            )
                            .is_ok();
                    let irq_ok =
                        !irq_was_enabled || BlockDriverOps::enable_irq(&mut *device).is_ok();
                    notifier_ok && irq_ok
                };
                if !reenabled {
                    self.inner
                        .completion_quarantined
                        .store(true, Ordering::Release);
                    self.notify_progress();
                    self.notify_terminal();
                    self.inner.completion_waiters.notify_many(usize::MAX, false);
                    return Err(DevError::BadState);
                }
                self.inner.physical_pending.store(0, Ordering::Release);
                self.inner.completion_credits.store(0, Ordering::Release);
                {
                    let mut mailbox = self.inner.completions.lock();
                    mailbox.head = 0;
                    mailbox.len = 0;
                }
                self.inner.completion_routes.lock().clear();
                self.inner
                    .completion_quarantined
                    .store(false, Ordering::Release);
            }
            BlockResetOutcome::Retired => {
                // Quiescence is proven, but the lower queue has been
                // dismantled permanently.  Keep cached completion records
                // and ownership credits intact for any broker/quarantine
                // consumer; clearing them here would silently release an
                // effect identity.  No callback is reinstalled and every
                // future submission/wait fails until transport reinit.
                self.inner.completion_retired.store(true, Ordering::Release);
                self.inner
                    .completion_quarantined
                    .store(true, Ordering::Release);
            }
            BlockResetOutcome::Quarantined => {
                self.inner
                    .completion_quarantined
                    .store(true, Ordering::Release);
            }
        }
        self.notify_terminal();
        self.inner.completion_waiters.notify_many(usize::MAX, false);
        Ok(outcome)
    }

    fn poll_async_complete(&mut self, budget: usize) -> DevResult<usize> {
        if budget == 0 {
            return Ok(0);
        }
        // Count-only polling is a legacy ordinary-owner API.  Keep one fixed
        // broker pass per invocation; in particular, never loop while a
        // physical-only stream keeps making the lower drain report a
        // continuation.  Callers that need continuation metadata use
        // `drain_async_completions`, whose typed result carries the bounded
        // continuation bit.
        let budget = budget.min(COMPLETION_BATCH_CAPACITY);
        let _owner = self.claim_completion_owner()?;
        if self.inner.completions.lock().contains_quarantined() {
            self.inner
                .completion_quarantined
                .store(true, Ordering::Release);
            self.notify_progress();
            return Err(DevError::BadState);
        }
        let mut retired = self.inner.completions.lock().take_ordinary_count(budget);
        self.consume_completion_credits(retired)?;
        if retired != 0 {
            self.notify_progress();
        }
        if retired == budget {
            return Ok(retired);
        }

        // A physical-only mailbox is already owned by the typed broker (or
        // by the exact effect route).  Do not touch the lower used ring from
        // this legacy count-only surface: a continuous physical stream must
        // not turn an ordinary poll into a second drain loop.  The broker's
        // bounded pass will wake ordinary owners when the mixed head moves.
        let physical_only = {
            let mailbox = self.inner.completions.lock();
            mailbox.contains_physical() && !mailbox.contains_ordinary()
        };
        if physical_only {
            return Ok(retired);
        }

        // One bounded mixed drain is enough to make ordinary progress.  If
        // its head is physical-only, the exact/device-global physical owner
        // retains that record and this count-only API returns immediately.
        let (_drain, _drain_generation) = self.drain_device_once()?;
        if self.inner.completions.lock().contains_quarantined() {
            self.inner
                .completion_quarantined
                .store(true, Ordering::Release);
            self.notify_progress();
            return Err(DevError::BadState);
        }
        let take = self
            .inner
            .completions
            .lock()
            .take_ordinary_count(budget.saturating_sub(retired));
        self.consume_completion_credits(take)?;
        retired = retired.saturating_add(take);
        if take != 0 {
            self.notify_progress();
        }
        // A physical-only continuation is intentionally left to the typed
        // drain worker; this count-only surface returns after this pass.
        Ok(retired)
    }

    fn wait_async_all(&mut self, handles: &[BlockRequestHandle]) -> DevResult {
        self.wait_async_all_owned(handles)
    }

    fn enable_irq(&mut self) -> DevResult {
        BlockDriverOps::enable_irq(&mut *self.lock_raw())
    }

    fn disable_irq(&mut self) -> DevResult {
        BlockDriverOps::disable_irq(&mut *self.lock_raw())
    }

    fn is_irq_enabled(&self) -> bool {
        self.lock_raw().is_irq_enabled()
    }

    fn handle_irq(&mut self) -> DevResult<usize> {
        let handled = self.lock_raw().handle_irq()?;
        self.notify_progress();
        Ok(handled)
    }

    fn fence_async(&mut self) -> DevResult {
        // A fence is a queue operation, not a completion consumer. Once the
        // broker is installed it remains the sole used-ring owner, so claim
        // that owner for the finite idle-state check and for the lower fence
        // itself. An active/unknown route or cached completion must still
        // fail closed: the lower fence could otherwise consume an identity
        // that belongs to the typed mailbox.
        if self.completion_unavailable() {
            return Err(DevError::BadState);
        }
        let _owner = self.claim_completion_owner()?;
        let routes = self.inner.completion_routes.lock();
        let mut device = self.inner.device.lock();
        if self.completion_unavailable()
            || self.inner.physical_pending.load(Ordering::Acquire) != 0
            || self.inner.completion_credits.load(Ordering::Acquire) != 0
            || routes.occupied()
        {
            return Err(DevError::BadState);
        }
        let mailbox = self.inner.completions.lock();
        if mailbox.len != 0 {
            return Err(DevError::BadState);
        }
        drop(mailbox);
        // Keep the route and device locks through this idle operation so a
        // physical reservation cannot race the proof that no completion
        // custody exists. The lower fence has no tracked request to wait for
        // in this state and therefore cannot block the broker's owner.
        device.fence_async()
    }
}

#[cfg(test)]
mod completion_mailbox_tests {
    use super::*;

    fn record(raw: u64, owner: BlockCompletionOwner) -> BlockCompletion {
        BlockCompletion {
            handle: BlockRequestHandle { raw },
            owner,
            cookie: raw.wrapping_add(0x1000),
            status: BlockCompletionStatus::Success,
            bytes: raw as u32,
        }
    }

    #[test]
    fn sync_backpressure_requires_progress_and_an_unpublished_prefix() {
        // A completion racing the failed attempt must win over sleeping; a
        // terminal state must win over retrying.  These are the two sides of
        // the pre/post-listener generation protocol.
        assert!(completion_progress_observed(7, 8, false));
        assert!(completion_progress_observed(7, 7, true));
        assert!(!completion_progress_observed(7, 7, false));

        let report = BlockSubmitReport {
            submitted: 0,
            bytes: 0,
            queue_full: true,
        };
        assert!(sync_submit_unpublished_queue_full(&report, true));
        assert!(!sync_submit_unpublished_queue_full(&report, false));
        assert!(!sync_submit_unpublished_queue_full(
            &BlockSubmitReport {
                submitted: 1,
                ..report
            },
            true,
        ));

        assert!(sync_submit_queue_full_drain_progressed(
            BlockCompletionDrain {
                completed: 1,
                continuation: false,
            }
        ));
        assert!(sync_submit_queue_full_drain_progressed(
            BlockCompletionDrain {
                completed: 0,
                continuation: true,
            }
        ));
        assert!(!sync_submit_queue_full_drain_progressed(
            BlockCompletionDrain::default()
        ));
    }

    #[test]
    fn mixed_owner_drain_keeps_physical_records_for_physical_owner() {
        let mut mailbox = CompletionMailbox::new();
        assert!(mailbox.push(record(0x10, BlockCompletionOwner::Ordinary)));
        assert!(mailbox.push(record(0x20, BlockCompletionOwner::Physical)));
        assert!(mailbox.push(record(0x30, BlockCompletionOwner::Legacy)));

        let mut ordinary = [record(0, BlockCompletionOwner::Ordinary); 2];
        assert_eq!(mailbox.take_ordinary(&mut ordinary), 2);
        assert_eq!(ordinary[0].handle.raw, 0x10);
        assert_eq!(ordinary[1].handle.raw, 0x30);
        assert!(mailbox.contains_physical());

        let mut physical = [record(0, BlockCompletionOwner::Physical); 1];
        assert_eq!(mailbox.take_matching(&mut physical, true), 1);
        assert_eq!(physical[0].handle.raw, 0x20);
        assert!(mailbox.is_empty());
    }

    #[test]
    fn mixed_drain_progress_edge_requires_a_physical_record() {
        let ordinary = [record(0x10, BlockCompletionOwner::Ordinary)];
        assert!(!completion_batch_has_physical(&ordinary));

        let mixed = [
            record(0x10, BlockCompletionOwner::Ordinary),
            record(0x20, BlockCompletionOwner::Physical),
        ];
        assert!(completion_batch_has_physical(&mixed));
    }

    #[test]
    fn destination_cached_hit_reprobes_before_destination_quiescence() {
        let mut mailbox = CompletionMailbox::new();
        assert!(mailbox.push(record(0x10, BlockCompletionOwner::Ordinary)));
        assert!(mailbox.push(record(0x20, BlockCompletionOwner::Physical)));

        let mut output = [record(0, BlockCompletionOwner::Physical); 1];
        let cached = mailbox.take_physical_matching(&mut output, |_| true);
        assert_eq!(cached, 1);
        assert!(mailbox.contains_ordinary());
        assert!(!mailbox.contains_physical());

        // Taking a cached destination record skipped the lower probe, so the
        // next bounded pass must run even though this destination is empty.
        assert!(destination_drain_needs_followup(
            cached != 0,
            false,
            mailbox.contains_physical(),
        ));
        // The follow-up pass probes the lower queue. With no lower or
        // destination work left, the destination worker reaches quiescence;
        // an ordinary mailbox record does not keep this route alive.
        assert!(!destination_drain_needs_followup(
            false,
            false,
            mailbox.contains_physical(),
        ));
    }

    #[test]
    fn exact_raw_handle_lookup_is_out_of_order_safe() {
        let mut mailbox = CompletionMailbox::new();
        assert!(mailbox.push(record(0x41, BlockCompletionOwner::Physical)));
        assert!(mailbox.push(record(0x83, BlockCompletionOwner::Physical)));

        let matched = mailbox.take_handle(0x83).expect("raw handle must match");
        assert_eq!(matched.handle.raw, 0x83);
        let mut output = [record(0, BlockCompletionOwner::Physical); 1];
        assert_eq!(mailbox.take_matching(&mut output, true), 1);
        assert_eq!(output[0].handle.raw, 0x41);
    }

    #[test]
    fn exact_effect_drain_keeps_foreign_head_and_out_of_order_effects() {
        let mut mailbox = CompletionMailbox::new();
        let first = record(0x41, BlockCompletionOwner::Physical);
        let second = record(0x83, BlockCompletionOwner::Physical);
        assert!(mailbox.push(first));
        assert!(mailbox.push(second));

        let handles = [BlockRequestHandle { raw: 0x83 }];
        let cookies = [second.cookie];
        let mut output = [record(0, BlockCompletionOwner::Physical); 1];
        assert_eq!(
            mailbox
                .take_handles_exact(&handles, &cookies, &mut output)
                .unwrap(),
            1
        );
        assert_eq!(output[0].handle.raw, 0x83);
        assert!(mailbox.contains_handle(first.handle.raw));

        let handles = [BlockRequestHandle { raw: 0x41 }];
        let cookies = [first.cookie];
        assert_eq!(
            mailbox
                .take_handles_exact(&handles, &cookies, &mut output)
                .unwrap(),
            1
        );
        assert_eq!(output[0].handle.raw, 0x41);
        assert!(mailbox.is_empty());
    }

    #[test]
    fn exact_effect_drain_quarantines_cookie_mismatch_without_releasing_owner() {
        let mut mailbox = CompletionMailbox::new();
        let expected = record(0x55, BlockCompletionOwner::Physical);
        assert!(mailbox.push(expected));
        let handles = [expected.handle];
        let cookies = [expected.cookie.wrapping_add(1)];
        let mut output = [record(0, BlockCompletionOwner::Physical); 1];
        assert!(
            mailbox
                .take_handles_exact(&handles, &cookies, &mut output)
                .is_err()
        );
        assert!(mailbox.contains_handle(expected.handle.raw));
        assert_eq!(mailbox.len, 1);
    }

    #[test]
    fn exact_effect_drain_quarantines_duplicate_raw_completion() {
        let mut mailbox = CompletionMailbox::new();
        let duplicate = record(0x66, BlockCompletionOwner::Physical);
        assert!(mailbox.push(duplicate));
        assert!(mailbox.push(duplicate));
        let handles = [duplicate.handle];
        let cookies = [duplicate.cookie];
        let mut output = [record(0, BlockCompletionOwner::Physical); 1];
        assert!(
            mailbox
                .take_handles_exact(&handles, &cookies, &mut output)
                .is_err()
        );
        assert_eq!(mailbox.len, 2);
    }

    #[test]
    fn exact_effect_drain_skips_ordinary_fifo_head() {
        let mut mailbox = CompletionMailbox::new();
        let ordinary = record(0x71, BlockCompletionOwner::Ordinary);
        let physical = record(0x72, BlockCompletionOwner::Physical);
        assert!(mailbox.push(ordinary));
        assert!(mailbox.push(physical));
        let handles = [physical.handle];
        let cookies = [physical.cookie];
        let mut output = [record(0, BlockCompletionOwner::Physical); 1];
        assert_eq!(
            mailbox
                .take_handles_exact(&handles, &cookies, &mut output)
                .unwrap(),
            1
        );
        assert_eq!(output[0].handle.raw, physical.handle.raw);
        assert!(mailbox.contains_handle(ordinary.handle.raw));
    }

    #[test]
    fn bounded_mixed_drain_compacts_once_and_preserves_fifo_per_owner() {
        let mut mailbox = CompletionMailbox::new();
        assert!(mailbox.push(record(0x10, BlockCompletionOwner::Ordinary)));
        assert!(mailbox.push(record(0x20, BlockCompletionOwner::Physical)));
        assert!(mailbox.push(record(0x30, BlockCompletionOwner::Ordinary)));
        assert!(mailbox.push(record(0x40, BlockCompletionOwner::Physical)));

        let mut physical = [record(0, BlockCompletionOwner::Physical); 1];
        assert_eq!(mailbox.take_matching(&mut physical, true), 1);
        assert_eq!(physical[0].handle.raw, 0x20);
        assert!(mailbox.contains_physical());

        let mut ordinary = [record(0, BlockCompletionOwner::Ordinary); 2];
        assert_eq!(mailbox.take_ordinary(&mut ordinary), 2);
        assert_eq!(ordinary[0].handle.raw, 0x10);
        assert_eq!(ordinary[1].handle.raw, 0x30);

        assert_eq!(mailbox.take_matching(&mut physical, true), 1);
        assert_eq!(physical[0].handle.raw, 0x40);
        assert!(mailbox.is_empty());
    }

    fn physical_requests<'a, const N: usize>(
        segments: &'a [BlockPhysicalSegment],
        raw_base: u64,
    ) -> [BlockPhysicalRequest<'a>; N] {
        core::array::from_fn(|index| {
            let raw = raw_base + index as u64;
            BlockPhysicalRequest {
                block_id: index as u64,
                op: BlockAsyncOp::Read,
                segments,
                handle: Some(BlockRequestHandle { raw }),
                cookie: Some(raw + 0x1000),
            }
        })
    }

    #[test]
    fn route_groups_have_32x16_capacity_and_reject_the_33rd_group() {
        let mut routes = PhysicalRouteTable::new();
        let mut groups = [0u8; PHYSICAL_ROUTE_CAPACITY];
        for group in &mut groups {
            *group = routes
                .reserve(
                    BlockPhysicalCompletionRoute::Exact,
                    7,
                    PHYSICAL_ROUTE_CHILD_CAPACITY,
                )
                .unwrap();
        }
        assert_eq!(groups, core::array::from_fn(|index| index as u8));
        assert!(matches!(
            routes.reserve(BlockPhysicalCompletionRoute::Exact, 7, 1),
            Err(DevError::ResourceBusy)
        ));
        assert!(matches!(
            routes.reserve(
                BlockPhysicalCompletionRoute::Exact,
                7,
                PHYSICAL_ROUTE_CHILD_CAPACITY + 1
            ),
            Err(DevError::InvalidParam)
        ));

        let mut child_limit = PhysicalRouteTable::new();
        assert!(matches!(
            child_limit.reserve(
                BlockPhysicalCompletionRoute::Exact,
                7,
                PHYSICAL_ROUTE_CHILD_CAPACITY + 1,
            ),
            Err(DevError::InvalidParam)
        ));
    }

    #[test]
    fn two_groups_demux_interleaved_out_of_order_children() {
        let mut routes = PhysicalRouteTable::new();
        let exact_group = routes
            .reserve(BlockPhysicalCompletionRoute::Exact, 7, 2)
            .unwrap();
        let kernel_group = routes
            .reserve(BlockPhysicalCompletionRoute::Kernel, 7, 2)
            .unwrap();
        let segments: [BlockPhysicalSegment; 0] = [];
        let mut exact = physical_requests::<2>(&segments, 0x10);
        let mut kernel = physical_requests::<2>(&segments, 0x20);
        assert!(
            routes.mark_published(exact_group, 7, BlockPhysicalCompletionRoute::Exact, &exact,)
        );
        assert!(routes.mark_published(
            kernel_group,
            7,
            BlockPhysicalCompletionRoute::Kernel,
            &kernel,
        ));

        let mut mailbox = CompletionMailbox::new();
        let kernel_second = record(0x21, BlockCompletionOwner::Physical);
        let exact_first = record(0x10, BlockCompletionOwner::Physical);
        let kernel_first = record(0x20, BlockCompletionOwner::Physical);
        let exact_second = record(0x11, BlockCompletionOwner::Physical);
        assert!(mailbox.push(kernel_second));
        assert!(mailbox.push(exact_first));
        assert!(mailbox.push(kernel_first));
        assert!(mailbox.push(exact_second));
        let mut output = [record(0, BlockCompletionOwner::Physical); 2];
        let completed = mailbox.take_physical_matching(&mut output, |completion| {
            routes
                .matches_route(
                    7,
                    completion.handle.raw,
                    completion.cookie,
                    BlockPhysicalCompletionRoute::Kernel,
                )
                .is_some()
        });
        assert_eq!(completed, 2);
        assert_eq!(output[0], kernel_second);
        assert_eq!(output[1], kernel_first);
        assert!(mailbox.contains_handle(exact_first.handle.raw));
        assert!(mailbox.contains_handle(exact_second.handle.raw));
        assert!(routes.release_completion(7, 0x21, kernel_second.cookie));
        assert!(routes.occupied());
        assert!(routes.release_completion(7, 0x20, kernel_first.cookie));
        assert!(routes.occupied());
        assert!(routes.matches_exact(7, 0x10, exact_first.cookie));
        let _ = (&mut exact, &mut kernel);
    }

    #[test]
    fn exact_sixteen_child_prefix_stays_until_group_ack() {
        let mut routes = PhysicalRouteTable::new();
        let group = routes
            .reserve(
                BlockPhysicalCompletionRoute::Exact,
                13,
                PHYSICAL_ROUTE_CHILD_CAPACITY,
            )
            .unwrap();
        let segments: [BlockPhysicalSegment; 0] = [];
        let requests = physical_requests::<PHYSICAL_ROUTE_CHILD_CAPACITY>(&segments, 0x50);
        assert!(routes.mark_published(group, 13, BlockPhysicalCompletionRoute::Exact, &requests,));
        let handles: [BlockRequestHandle; PHYSICAL_ROUTE_CHILD_CAPACITY] =
            core::array::from_fn(|index| requests[index].handle.unwrap());
        let cookies: [u64; PHYSICAL_ROUTE_CHILD_CAPACITY] =
            core::array::from_fn(|index| requests[index].cookie.unwrap());

        for index in 0..7 {
            assert!(routes.release_completion(13, handles[index].raw, cookies[index]));
            assert!(routes.matches_exact(13, handles[index].raw, cookies[index]));
        }
        assert!(!routes.release_exact_completed(13, &handles[..7], &cookies[..7],));
        assert!(routes.occupied());
        for index in 7..PHYSICAL_ROUTE_CHILD_CAPACITY {
            assert!(routes.release_completion(13, handles[index].raw, cookies[index]));
        }
        assert!(routes.release_exact_completed(13, &handles, &cookies));
        assert!(!routes.occupied());
    }

    #[test]
    fn partial_sixteen_child_acceptance_releases_unaccepted_suffix() {
        let mut routes = PhysicalRouteTable::new();
        let group = routes
            .reserve(
                BlockPhysicalCompletionRoute::Kernel,
                17,
                PHYSICAL_ROUTE_CHILD_CAPACITY,
            )
            .unwrap();
        let segments: [BlockPhysicalSegment; 0] = [];
        let requests = physical_requests::<PHYSICAL_ROUTE_CHILD_CAPACITY>(&segments, 0x70);
        assert!(routes.mark_published(
            group,
            17,
            BlockPhysicalCompletionRoute::Kernel,
            &requests[..7],
        ));
        routes.release_reserved(
            group,
            17,
            BlockPhysicalCompletionRoute::Kernel,
            7,
            PHYSICAL_ROUTE_CHILD_CAPACITY,
        );
        assert_eq!(
            routes.count(17, Some(BlockPhysicalCompletionRoute::Kernel)),
            7
        );
        assert!(
            routes.group(group).unwrap().children[7..]
                .iter()
                .all(Option::is_none)
        );
        for request in requests.iter().take(7) {
            let handle = request.handle.unwrap();
            assert!(routes.release_completion(17, handle.raw, request.cookie.unwrap()));
        }
        assert!(!routes.occupied());
    }

    #[test]
    fn stale_generation_wrong_cookie_and_duplicate_quarantine_group_owner() {
        let mut routes = PhysicalRouteTable::new();
        let segments: [BlockPhysicalSegment; 0] = [];
        let first_group = routes
            .reserve(BlockPhysicalCompletionRoute::Exact, 19, 1)
            .unwrap();
        let first = physical_requests::<1>(&segments, 0x90);
        assert!(routes.mark_published(
            first_group,
            19,
            BlockPhysicalCompletionRoute::Exact,
            &first,
        ));
        assert!(!routes.matches_exact(20, 0x90, 0x1090));
        assert!(!routes.matches_exact(19, 0x90, 0x2090));
        routes.mark_group_quarantined(first_group);
        assert!(routes.occupied());
        assert!(!routes.matches_exact(19, 0x90, 0x1090));

        let second_group = routes
            .reserve(BlockPhysicalCompletionRoute::Kernel, 19, 1)
            .unwrap();
        let second = physical_requests::<1>(&segments, 0xa0);
        assert!(routes.mark_published(
            second_group,
            19,
            BlockPhysicalCompletionRoute::Kernel,
            &second,
        ));
        let duplicate_group = routes
            .reserve(BlockPhysicalCompletionRoute::Kernel, 19, 1)
            .unwrap();
        let duplicate = [BlockPhysicalRequest {
            block_id: 1,
            op: BlockAsyncOp::Read,
            segments: &segments,
            handle: Some(BlockRequestHandle { raw: 0x90 }),
            cookie: Some(0x3090),
        }];
        assert!(!routes.mark_published(
            duplicate_group,
            19,
            BlockPhysicalCompletionRoute::Kernel,
            &duplicate,
        ));
        routes.mark_group_quarantined(duplicate_group);
        assert!(routes.group(duplicate_group).unwrap().quarantined);
        let _ = (&first, &second);
    }

    #[test]
    fn old_exact_capability_cannot_release_reused_raw_cookie_owner() {
        let mut routes = PhysicalRouteTable::new();
        let segments: [BlockPhysicalSegment; 0] = [];
        let old_generation = 23;
        let new_generation = old_generation + 1;
        let group = routes
            .reserve(BlockPhysicalCompletionRoute::Exact, old_generation, 1)
            .unwrap();
        let old = physical_requests::<1>(&segments, 0xc0);
        assert!(routes.mark_published(
            group,
            old_generation,
            BlockPhysicalCompletionRoute::Exact,
            &old,
        ));
        let handles = [old[0].handle.unwrap()];
        let cookies = [old[0].cookie.unwrap()];
        let capability = ExactCompletionCapability {
            generation: old_generation,
            handles: &handles,
            cookies: &cookies,
        };

        // A quiescing reset clears the old table, then the next transport
        // generation reuses the same fixed group and raw/cookie identity.
        routes.clear();
        let reused_group = routes
            .reserve(BlockPhysicalCompletionRoute::Exact, new_generation, 1)
            .unwrap();
        assert_eq!(reused_group, group);
        let reused = physical_requests::<1>(&segments, 0xc0);
        assert!(routes.mark_published(
            reused_group,
            new_generation,
            BlockPhysicalCompletionRoute::Exact,
            &reused,
        ));

        assert!(!routes.release_completion(capability.generation, handles[0].raw, cookies[0],));
        assert!(routes.matches_exact(new_generation, handles[0].raw, cookies[0]));
    }

    #[test]
    fn old_reservation_generation_cannot_drop_reused_group() {
        let mut routes = PhysicalRouteTable::new();
        let old_generation = 31;
        let new_generation = old_generation + 1;
        let group = routes
            .reserve(BlockPhysicalCompletionRoute::Exact, old_generation, 1)
            .unwrap();

        routes.clear();
        let reused_group = routes
            .reserve(BlockPhysicalCompletionRoute::Exact, new_generation, 1)
            .unwrap();
        assert_eq!(reused_group, group);

        // This is the exact operation performed by an old uncommitted
        // reservation's Drop. Generation mismatch must make it a no-op.
        assert!(!routes.release_unpublished(
            group,
            old_generation,
            BlockPhysicalCompletionRoute::Exact,
            1,
        ));
        assert!(routes.reservation_prefix_matches(
            reused_group,
            new_generation,
            BlockPhysicalCompletionRoute::Exact,
            1,
        ));
    }
}

#[cfg(all(test, block_dev = "ramdisk"))]
mod tests {
    use alloc::vec::Vec;

    use axdriver_block::ramdisk::RamDisk;

    use super::*;

    fn patterned_device(bytes: usize) -> (SharedBlockDevice, Vec<u8>) {
        let contents = (0..bytes)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let device = SharedBlockDevice::new(RamDisk::from(contents.as_slice()));
        (device, contents)
    }

    fn one_physical_request<'a>(
        segments: &'a [BlockPhysicalSegment],
        raw: u64,
    ) -> BlockPhysicalRequest<'a> {
        BlockPhysicalRequest {
            block_id: 0,
            op: BlockAsyncOp::Read,
            segments,
            handle: Some(BlockRequestHandle { raw }),
            cookie: Some(raw + 0x1000),
        }
    }

    #[test]
    fn unaligned_read_crosses_blocks_and_stops_at_eof() {
        let (device, contents) = patterned_device(1024);
        let mut crossing = [0u8; 8];
        assert_eq!(device.read_at(509, &mut crossing).unwrap(), crossing.len());
        assert_eq!(&crossing, &contents[509..517]);

        let mut eof = [0xa5; 16];
        assert_eq!(device.read_at(1020, &mut eof).unwrap(), 4);
        assert_eq!(&eof[..4], &contents[1020..]);
        assert_eq!(&eof[4..], &[0xa5; 12]);
        assert_eq!(device.read_at(1024, &mut eof).unwrap(), 0);
    }

    #[test]
    fn unaligned_write_preserves_neighbors_and_stops_at_eof() {
        let (device, mut expected) = patterned_device(1024);
        let crossing = [0xf1, 0xf2, 0xf3, 0xf4, 0xf5];
        assert_eq!(device.write_at(510, &crossing).unwrap(), crossing.len());
        expected[510..515].copy_from_slice(&crossing);

        let tail = [0xe1, 0xe2, 0xe3, 0xe4];
        assert_eq!(device.write_at(1022, &tail).unwrap(), 2);
        expected[1022..].copy_from_slice(&tail[..2]);

        let mut actual = vec![0u8; expected.len()];
        assert_eq!(device.read_at(0, &mut actual).unwrap(), actual.len());
        assert_eq!(actual, expected);
        assert_eq!(device.write_at(1024, &tail).unwrap(), 0);
    }

    #[test]
    fn block_flush_is_forwarded() {
        let (mut device, _) = patterned_device(512);
        BlockDriverOps::flush(&mut device).unwrap();
    }

    #[test]
    fn nonblocking_bootstrap_guard_uses_legacy_owner_before_publish() {
        let (device, contents) = patterned_device(512);
        // Model the filesystem-init context where can_block_current() is
        // false: this path must not publish a split-phase request and then
        // ask the completion wait queue for progress.  The lower legacy
        // operation is selected while the route/device locks exclude a
        // concurrent publication.
        let mut read = [0u8; 512];
        let result = device.try_legacy_sync(|raw| BlockDriverOps::read_block(raw, 0, &mut read));
        assert!(matches!(result, Some(Ok(()))));
        assert_eq!(read.as_slice(), contents.as_slice());

        // An installed-but-idle broker still permits the same pre-publication
        // legacy owner. A physical route reservation, however, is custody
        // even before its descriptor is published and must force the typed
        // route path.
        assert!(matches!(device.install_physical_completion_broker(), Ok(0)));
        assert!(matches!(device.try_legacy_sync(|_| Ok(())), Some(Ok(()))));
        let reservation = device
            .reserve_physical_completion_routes(BlockPhysicalCompletionRoute::Exact, 1)
            .unwrap();
        assert!(device.try_legacy_sync(|_| Ok(())).is_none());
        drop(reservation);
    }

    #[test]
    fn bootstrap_guard_supports_vectored_legacy_io() {
        let (device, contents) = patterned_device(1024);
        let mut first = [0u8; 512];
        let mut second = [0u8; 512];
        let mut bufs: [&mut [u8]; 2] = [&mut first, &mut second];
        device.lock().read_block_vectored(0, &mut bufs).unwrap();
        assert_eq!(first.as_slice(), &contents[..512]);
        assert_eq!(second.as_slice(), &contents[512..]);

        let replacement_a = [0xa5u8; 512];
        let replacement_b = [0x5au8; 512];
        let bufs: [&[u8]; 2] = [&replacement_a, &replacement_b];
        device.lock().write_block_vectored(0, &bufs).unwrap();
        let mut actual = vec![0u8; 1024];
        assert_eq!(device.read_at(0, &mut actual).unwrap(), actual.len());
        assert_eq!(&actual[..512], &replacement_a);
        assert_eq!(&actual[512..], &replacement_b);
    }

    #[test]
    fn idle_broker_fence_preserves_lower_capability_and_seals_fallback() {
        let (device, _) = patterned_device(512);
        assert!(matches!(device.install_physical_completion_broker(), Ok(0)));

        let mut fenced = device.clone();
        // The broker is idle: it must not manufacture BadState merely
        // because it is installed. RamDisk has no lower fence capability, so
        // the typed transport error is the expected result.
        assert!(matches!(
            BlockDriverOps::fence_async(&mut fenced),
            Err(DevError::Unsupported)
        ));
        // An idle broker still has one safe completion owner: the restricted
        // guard may run the lower synchronous operation while holding the
        // route/owner/device exclusion. It must not force an unsupported
        // split-phase path merely because the broker flag is set.
        assert!(device.lock().flush().is_ok());
    }

    #[test]
    fn count_poll_yields_immediately_for_physical_only_mailbox() {
        let (mut device, _) = patterned_device(512);
        assert!(device.inner.completions.lock().push(BlockCompletion {
            handle: BlockRequestHandle { raw: 0x91 },
            owner: BlockCompletionOwner::Physical,
            cookie: 0x1091,
            status: BlockCompletionStatus::Success,
            bytes: 512,
        }));

        assert!(matches!(
            BlockDriverOps::poll_async_complete(&mut device, usize::MAX),
            Ok(0)
        ));
        assert!(device.inner.completions.lock().contains_physical());
    }

    #[test]
    fn old_exact_waiter_cannot_retire_reused_raw_cookie_owner() {
        let (device, _) = patterned_device(512);
        let old_generation = device.completion_generation();
        let new_generation = old_generation + 1;
        let segments: [BlockPhysicalSegment; 0] = [];
        let old = [one_physical_request(&segments, 0xd0)];
        let old_group = {
            let mut routes = device.inner.completion_routes.lock();
            let group = routes
                .reserve(BlockPhysicalCompletionRoute::Exact, old_generation, 1)
                .unwrap();
            assert!(routes.mark_published(
                group,
                old_generation,
                BlockPhysicalCompletionRoute::Exact,
                &old,
            ));
            group
        };
        let handles = [old[0].handle.unwrap()];
        let cookies = [old[0].cookie.unwrap()];
        let capability = ExactCompletionCapability {
            generation: old_generation,
            handles: &handles,
            cookies: &cookies,
        };

        // Model reset/quiesce followed by a same-group, same-identity reuse.
        device
            .inner
            .completion_transport_generation
            .store(new_generation, Ordering::Release);
        let reused = [one_physical_request(&segments, 0xd0)];
        {
            let mut routes = device.inner.completion_routes.lock();
            routes.clear();
            let reused_group = routes
                .reserve(BlockPhysicalCompletionRoute::Exact, new_generation, 1)
                .unwrap();
            assert_eq!(reused_group, old_group);
            assert!(routes.mark_published(
                reused_group,
                new_generation,
                BlockPhysicalCompletionRoute::Exact,
                &reused,
            ));
        }
        device.inner.physical_pending.store(1, Ordering::Release);
        device.inner.completion_credits.store(1, Ordering::Release);

        let record = BlockCompletion {
            handle: handles[0],
            owner: BlockCompletionOwner::Physical,
            cookie: cookies[0],
            status: BlockCompletionStatus::Success,
            bytes: 512,
        };
        device
            .inner
            .completion_broker_installed
            .store(true, Ordering::Release);
        assert!(device.inner.completions.lock().push(record));
        let mut output = [record; 1];
        assert!(matches!(
            device.wait_exact_from_broker(capability, &mut output),
            Err(DevError::BadState)
        ));
        assert!(
            device
                .inner
                .completions
                .lock()
                .contains_handle(record.handle.raw)
        );
        assert!(matches!(
            device.retire_routed_physical(
                PhysicalRetirementCapability::Exact(capability),
                &[record],
                1,
            ),
            Err(DevError::BadState)
        ));
        assert_eq!(device.inner.physical_pending.load(Ordering::Acquire), 1);
        assert_eq!(device.inner.completion_credits.load(Ordering::Acquire), 1);
        assert!(device.inner.completion_routes.lock().matches_exact(
            new_generation,
            handles[0].raw,
            cookies[0],
        ));
    }

    #[test]
    fn old_reservation_drop_cannot_release_reused_group() {
        let (device, _) = patterned_device(512);
        let old = device
            .reserve_physical_completion_routes(BlockPhysicalCompletionRoute::Exact, 1)
            .unwrap();
        let old_generation = old.generation();
        let old_group = old.group;
        let new_generation = old_generation + 1;

        // Model reset/quiesce and reopen the same fixed group for the next
        // generation while the old token remains uncommitted.
        device
            .inner
            .completion_transport_generation
            .store(new_generation, Ordering::Release);
        device.inner.completion_routes.lock().clear();
        let new = device
            .reserve_physical_completion_routes(BlockPhysicalCompletionRoute::Exact, 1)
            .unwrap();
        assert_eq!(new.group, old_group);

        drop(old);
        assert!(
            device
                .inner
                .completion_routes
                .lock()
                .reservation_prefix_matches(
                    new.group,
                    new_generation,
                    BlockPhysicalCompletionRoute::Exact,
                    1,
                )
        );
        drop(new);
    }
}
