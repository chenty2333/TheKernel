use alloc::vec::Vec;

use axerrno::{AxError, AxResult};
use axhal::paging::PageSize;
use kspin::SpinNoIrq;
use memory_addr::PhysAddr;

use super::dealloc_frame_now;

#[derive(Clone, Copy)]
struct PinnedFrame {
    pins: u32,
    pending_free: Option<PageSize>,
}

impl PinnedFrame {
    const fn new() -> Self {
        Self {
            pins: 1,
            pending_free: None,
        }
    }
}

#[derive(Clone, Copy)]
enum PinnedFrameSlot {
    Free {
        next: Option<usize>,
    },
    Occupied {
        paddr: PhysAddr,
        frame: PinnedFrame,
        next: Option<usize>,
    },
}

struct PinnedFrameTable {
    buckets: Vec<Option<usize>>,
    slots: Vec<PinnedFrameSlot>,
    free_head: Option<usize>,
    live: usize,
    #[cfg(test)]
    batch_metrics: PinBatchMetrics,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct PinBatchMetrics {
    pin_batches: usize,
    pin_pages_requested: usize,
    pin_pages_committed: usize,
    rollback_batches: usize,
    rollback_pages: usize,
    unpin_batches: usize,
    unpin_pages_requested: usize,
    unpin_pages_completed: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct UnpinBatchReport {
    missing: usize,
    first_missing: Option<PhysAddr>,
}

impl UnpinBatchReport {
    fn merge(&mut self, other: Self) {
        self.missing = self
            .missing
            .checked_add(other.missing)
            .expect("physical-pin missing-entry count overflow");
        if self.first_missing.is_none() {
            self.first_missing = other.first_missing;
        }
    }
}

const MAX_PINNED_FRAMES: usize = super::super::USER_IO_PIN_MAX_PAGES as usize;
const PIN_TABLE_SHARDS: usize = 64;
const PIN_METADATA_SLOTS: usize = MAX_PINNED_FRAMES * 2;
/// Bounds one IRQ-disabled registry transaction independently of the 64 MiB
/// policy quota. Larger logical batches remain all-or-none at the public API.
const PIN_TABLE_LOCK_CHUNK_PAGES: usize = 64;
const _: () = assert!(PIN_TABLE_SHARDS.is_power_of_two());
const _: () = assert!(MAX_PINNED_FRAMES >= PIN_TABLE_SHARDS);
const _: () = assert!(PIN_METADATA_SLOTS.is_multiple_of(PIN_TABLE_SHARDS));

static PINNED_FRAME_SHARDS: [SpinNoIrq<Option<PinnedFrameTable>>; PIN_TABLE_SHARDS] =
    [const { SpinNoIrq::new(None) }; PIN_TABLE_SHARDS];

fn physical_page_hash(paddr: PhysAddr) -> usize {
    let mut page = paddr.as_usize() >> 12;
    page ^= page >> 16;
    page = page.wrapping_mul(0x9e37_79b1);
    page ^ (page >> 13)
}

fn pin_shard_index(paddr: PhysAddr) -> usize {
    physical_page_hash(paddr) & (PIN_TABLE_SHARDS - 1)
}

const fn pin_shard_capacity(_: usize) -> usize {
    PIN_METADATA_SLOTS / PIN_TABLE_SHARDS
}

impl PinnedFrameTable {
    fn try_new(limit: usize) -> AxResult<Self> {
        let bucket_count = limit.checked_mul(2).ok_or(AxError::NoMemory)?;
        if bucket_count == 0 {
            return Err(AxError::NoMemory);
        }

        let mut buckets = Vec::new();
        buckets
            .try_reserve_exact(bucket_count)
            .map_err(|_| AxError::NoMemory)?;
        buckets.resize(bucket_count, None);

        let mut slots = Vec::new();
        slots
            .try_reserve_exact(limit)
            .map_err(|_| AxError::NoMemory)?;
        for index in 0..limit {
            slots.push(PinnedFrameSlot::Free {
                next: (index + 1 < limit).then_some(index + 1),
            });
        }
        Ok(Self {
            buckets,
            slots,
            free_head: Some(0),
            live: 0,
            #[cfg(test)]
            batch_metrics: PinBatchMetrics::default(),
        })
    }

    fn bucket_index(&self, paddr: PhysAddr) -> usize {
        // The low hash bits select a registry shard. Consume the remaining
        // bits here so one shard can still use its complete bucket array.
        (physical_page_hash(paddr) / PIN_TABLE_SHARDS) % self.buckets.len()
    }

    fn find_node(&self, paddr: PhysAddr) -> Option<(Option<usize>, usize)> {
        let mut previous = None;
        let mut cursor = self.buckets[self.bucket_index(paddr)];
        while let Some(index) = cursor {
            let PinnedFrameSlot::Occupied {
                paddr: current,
                next,
                ..
            } = self.slots[index]
            else {
                panic!("free physical-pin node is linked from a hash bucket");
            };
            if current == paddr {
                return Some((previous, index));
            }
            previous = Some(index);
            cursor = next;
        }
        None
    }

    fn pin_preallocated(&mut self, paddr: PhysAddr) -> AxResult<()> {
        if let Some((_, index)) = self.find_node(paddr) {
            let PinnedFrameSlot::Occupied { frame, .. } = &mut self.slots[index] else {
                unreachable!();
            };
            frame.pins = frame.pins.checked_add(1).ok_or(AxError::NoMemory)?;
            return Ok(());
        }
        if self.live >= self.slots.len() {
            return Err(AxError::NoMemory);
        }

        let index = self.free_head.ok_or(AxError::NoMemory)?;
        let PinnedFrameSlot::Free { next: next_free } = self.slots[index] else {
            panic!("physical-pin free list references an occupied node");
        };
        let bucket = self.bucket_index(paddr);
        self.free_head = next_free;
        self.slots[index] = PinnedFrameSlot::Occupied {
            paddr,
            frame: PinnedFrame::new(),
            next: self.buckets[bucket],
        };
        self.buckets[bucket] = Some(index);
        self.live += 1;
        Ok(())
    }

    /// Pins one already-owned physical-frame batch as an all-or-none table
    /// transaction.
    ///
    /// The caller owns the table lock for this complete operation. Every
    /// successful prefix is rolled back before an error is returned, so a
    /// finite-table or refcount failure never leaks a partial batch.
    fn pin_batch(&mut self, paddrs: &[PhysAddr]) -> AxResult<()> {
        #[cfg(test)]
        {
            self.batch_metrics.pin_batches += 1;
            self.batch_metrics.pin_pages_requested += paddrs.len();
        }

        for (pinned, &paddr) in paddrs.iter().enumerate() {
            if let Err(error) = self.pin_preallocated(paddr) {
                for &rollback in paddrs[..pinned].iter().rev() {
                    let pending_free = self
                        .unpin(rollback)
                        .expect("physical-pin batch rollback lost its pinned prefix");
                    assert!(
                        pending_free.is_none(),
                        "physical-pin batch rollback released a pre-existing deferred free"
                    );
                }
                #[cfg(test)]
                {
                    self.batch_metrics.rollback_batches += 1;
                    self.batch_metrics.rollback_pages += pinned;
                }
                return Err(error);
            }
        }

        #[cfg(test)]
        {
            self.batch_metrics.pin_pages_committed += paddrs.len();
        }
        Ok(())
    }

    fn unpin(&mut self, paddr: PhysAddr) -> AxResult<Option<PageSize>> {
        let (previous, index) = self.find_node(paddr).ok_or(AxError::BadState)?;
        let (next, pending_free) = {
            let PinnedFrameSlot::Occupied { frame, next, .. } = &mut self.slots[index] else {
                unreachable!();
            };
            if frame.pins == 0 {
                return Err(AxError::BadState);
            }
            frame.pins -= 1;
            if frame.pins != 0 {
                return Ok(None);
            }
            (*next, frame.pending_free)
        };

        if let Some(previous) = previous {
            let PinnedFrameSlot::Occupied {
                next: previous_next,
                ..
            } = &mut self.slots[previous]
            else {
                panic!("physical-pin chain predecessor became free");
            };
            *previous_next = next;
        } else {
            let bucket = self.bucket_index(paddr);
            self.buckets[bucket] = next;
        }
        self.slots[index] = PinnedFrameSlot::Free {
            next: self.free_head,
        };
        self.free_head = Some(index);
        self.live = self
            .live
            .checked_sub(1)
            .expect("physical-pin count underflow");
        Ok(pending_free)
    }

    /// Releases a complete RAII batch while the caller owns the table lock.
    /// Deferred frees are appended to caller-preallocated storage and therefore
    /// never allocate inside the IRQ-disabled critical section.
    fn unpin_batch(
        &mut self,
        paddrs: &[PhysAddr],
        deferred_frees: &mut Vec<(PhysAddr, PageSize)>,
    ) -> UnpinBatchReport {
        debug_assert!(deferred_frees.capacity() - deferred_frees.len() >= paddrs.len());
        #[cfg(test)]
        {
            self.batch_metrics.unpin_batches += 1;
            self.batch_metrics.unpin_pages_requested += paddrs.len();
        }

        let mut report = UnpinBatchReport::default();
        for &paddr in paddrs {
            match self.unpin(paddr) {
                Ok(Some(page_size)) => deferred_frees.push((paddr, page_size)),
                Ok(None) => {}
                Err(_) => {
                    report.missing += 1;
                    report.first_missing.get_or_insert(paddr);
                    continue;
                }
            }
            #[cfg(test)]
            {
                self.batch_metrics.unpin_pages_completed += 1;
            }
        }
        report
    }

    fn defer_deallocation(&mut self, paddr: PhysAddr, page_size: PageSize) -> bool {
        let Some((_, index)) = self.find_node(paddr) else {
            return false;
        };
        let PinnedFrameSlot::Occupied { frame, .. } = &mut self.slots[index] else {
            unreachable!();
        };
        if let Some(existing) = frame.pending_free {
            assert_eq!(existing, page_size, "pinned frame free size changed");
        } else {
            frame.pending_free = Some(page_size);
        }
        true
    }
}

/// Installs one fully allocated shard table with only the pointer/owner move
/// performed under the IRQ-safe shard lock. A racing loser's complete table is
/// returned to the caller so its large vectors are always dropped after the
/// lock guard has gone away.
fn install_prepared_pin_shard(
    shard_index: usize,
    prepared: PinnedFrameTable,
) -> Option<PinnedFrameTable> {
    let mut shard =
        crate::mm::lock_mm_diagnosed!(PINNED_FRAME_SHARDS[shard_index], PhysPinRegistryShard);
    if shard.is_none() {
        *shard = Some(prepared);
        None
    } else {
        Some(prepared)
    }
}

fn prepare_physical_pin_registry_with(
    mut allocate: impl FnMut(usize) -> AxResult<PinnedFrameTable>,
) -> AxResult<()> {
    // `shard_index` is an operand, not just a subscript: the allocator and the
    // installer both receive it so a partially installed prefix stays
    // identifiable. Iterating the shard array directly would lose that.
    #[allow(clippy::needless_range_loop)]
    for shard_index in 0..PIN_TABLE_SHARDS {
        if crate::mm::lock_mm_diagnosed!(PINNED_FRAME_SHARDS[shard_index], PhysPinRegistryShard)
            .is_some()
        {
            continue;
        }

        // Allocation and complete table initialization happen with no shard
        // lock held. Successfully installed earlier shards deliberately remain
        // live if this allocation fails, making the bounded prefix retryable.
        let prepared = allocate(shard_index)?;
        let racing_loser = install_prepared_pin_shard(shard_index, prepared);
        // `install_prepared_pin_shard` has already released IRQ exclusion.
        drop(racing_loser);
    }
    Ok(())
}

/// Prepares every fixed physical-pin registry shard outside the address-space
/// critical section.
///
/// The operation is bounded to [`PIN_TABLE_SHARDS`] fixed tables. Allocation
/// failure preserves the already initialized prefix, and a later call safely
/// resumes at the first missing shard. Concurrent callers may allocate a
/// redundant table, but the racing loser is returned and freed outside the
/// shard lock.
pub(crate) fn prepare_physical_pin_registry() -> AxResult<()> {
    prepare_physical_pin_registry_with(|shard_index| {
        PinnedFrameTable::try_new(pin_shard_capacity(shard_index))
    })
}

/// Preallocated owner for one all-or-none physical-frame publication.
///
/// Call [`prepare_physical_pin_registry`] once for the operation and
/// [`Self::try_new`] for each batch before taking the address-space lock.
/// Afterwards, [`Self::push`] cannot allocate, and [`Self::publish`] either
/// transfers both vectors into [`PhysicalFramePins`] with `mem::take` or leaves
/// them owned by this value for the caller to drop after releasing its lock.
pub(crate) struct PreparedPhysicalFramePins {
    paddrs: Vec<PhysAddr>,
    deferred_frees: Vec<(PhysAddr, PageSize)>,
    max_pages: usize,
    publication_failed: bool,
}

impl PreparedPhysicalFramePins {
    pub(crate) fn try_new(max_pages: usize) -> AxResult<Self> {
        if max_pages > MAX_PINNED_FRAMES {
            return Err(AxError::InvalidInput);
        }

        let mut paddrs = Vec::new();
        paddrs
            .try_reserve_exact(max_pages)
            .map_err(|_| AxError::NoMemory)?;
        let mut deferred_frees = Vec::new();
        deferred_frees
            .try_reserve_exact(max_pages)
            .map_err(|_| AxError::NoMemory)?;
        Ok(Self {
            paddrs,
            deferred_frees,
            max_pages,
            publication_failed: false,
        })
    }

    /// Appends one frame without allocating. `false` is a bounded admission
    /// failure and leaves the preparation unchanged.
    pub(crate) fn push(&mut self, paddr: PhysAddr) -> bool {
        if self.publication_failed || self.paddrs.len() >= self.max_pages {
            return false;
        }
        debug_assert!(self.paddrs.len() < self.paddrs.capacity());
        self.paddrs.push(paddr);
        true
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.paddrs.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.paddrs.len()
    }

    /// Publishes only while the caller holds the system-wide logical page
    /// charge. No allocation or large owner destruction occurs in this call.
    /// The charge is the exact aggregate bound; the fixed 2x shard metadata is
    /// a second, mechanism-level admission. Pathological single-shard pressure
    /// therefore fails atomically and lets the adapter preserve semantics with
    /// its copy fallback instead of growing metadata in an IRQ-off path.
    pub(crate) fn publish(
        &mut self,
        _system_charge: &super::super::UserIoSystemPinCharge,
    ) -> AxResult<PhysicalFramePins> {
        self.publish_admitted()
    }

    fn publish_admitted(&mut self) -> AxResult<PhysicalFramePins> {
        debug_assert!(self.paddrs.len() <= self.max_pages);
        if self.publication_failed {
            return Err(AxError::BadState);
        }
        debug_assert!(self.deferred_frees.is_empty());
        if self.paddrs.is_empty() {
            return Ok(self.take_published());
        }

        // Group equal shards in place. Physical address order is not part of
        // the RAII contract; exact duplicate multiplicity is.
        self.paddrs
            .sort_unstable_by_key(|&paddr| pin_shard_index(paddr));
        let mut pinned = 0usize;
        while pinned < self.paddrs.len() {
            let shard_index = pin_shard_index(self.paddrs[pinned]);
            let mut shard_end = pinned + 1;
            while shard_end < self.paddrs.len()
                && pin_shard_index(self.paddrs[shard_end]) == shard_index
            {
                shard_end += 1;
            }
            while pinned < shard_end {
                let end = shard_end.min(pinned.saturating_add(PIN_TABLE_LOCK_CHUNK_PAGES));
                if let Err(error) = pin_shard_chunk(shard_index, &self.paddrs[pinned..end]) {
                    let report =
                        unpin_frame_chunks(&self.paddrs[..pinned], &mut self.deferred_frees)
                            .expect("prepared physical-pin registry lost an initialized shard");
                    assert_eq!(
                        report,
                        UnpinBatchReport::default(),
                        "physical-pin batch rollback lost a committed chunk"
                    );
                    // Retain deferred frees and both large vectors in this
                    // preparation. The caller still owns `&mut self`, exits its
                    // AddrSpace guard, and only then drops the failed batch.
                    self.publication_failed = true;
                    return Err(error);
                }
                pinned = end;
            }
        }

        Ok(self.take_published())
    }

    fn take_published(&mut self) -> PhysicalFramePins {
        self.max_pages = 0;
        PhysicalFramePins {
            paddrs: core::mem::take(&mut self.paddrs),
            deferred_frees: core::mem::take(&mut self.deferred_frees),
        }
    }
}

impl Drop for PreparedPhysicalFramePins {
    fn drop(&mut self) {
        for (paddr, page_size) in self.deferred_frees.drain(..) {
            dealloc_frame_now(paddr, page_size);
        }
    }
}

/// One all-or-none physical-frame pin batch.
///
/// Publication and final release acquire the registry once per bounded chunk,
/// so a large policy-level batch cannot turn into an unbounded IRQ-disabled
/// critical section. The second vector reserves enough storage before
/// publication to carry every possible deferred free out of those sections.
pub(crate) struct PhysicalFramePins {
    paddrs: Vec<PhysAddr>,
    deferred_frees: Vec<(PhysAddr, PageSize)>,
}

impl PhysicalFramePins {
    pub(crate) fn len(&self) -> usize {
        self.paddrs.len()
    }
}

impl Drop for PhysicalFramePins {
    fn drop(&mut self) {
        if self.paddrs.is_empty() {
            return;
        }

        let Some(report) = unpin_frame_chunks(&self.paddrs, &mut self.deferred_frees) else {
            warn!("PhysicalFramePins::drop: a pin registry shard is uninitialized");
            return;
        };
        if report.missing != 0 {
            warn!(
                "PhysicalFramePins::drop: {} missing pinned frame entries; first={:?}",
                report.missing, report.first_missing
            );
        }
        for (paddr, page_size) in self.deferred_frees.drain(..) {
            dealloc_frame_now(paddr, page_size);
        }
    }
}

fn unpin_frame_chunks(
    paddrs: &[PhysAddr],
    deferred_frees: &mut Vec<(PhysAddr, PageSize)>,
) -> Option<UnpinBatchReport> {
    let mut report = UnpinBatchReport::default();
    let mut shard_end = paddrs.len();
    while shard_end != 0 {
        let shard_index = pin_shard_index(paddrs[shard_end - 1]);
        let mut shard_start = shard_end - 1;
        while shard_start != 0 && pin_shard_index(paddrs[shard_start - 1]) == shard_index {
            shard_start -= 1;
        }

        for chunk in paddrs[shard_start..shard_end].rchunks(PIN_TABLE_LOCK_CHUNK_PAGES) {
            let chunk_report = {
                let mut table = crate::mm::lock_mm_diagnosed!(
                    PINNED_FRAME_SHARDS[shard_index],
                    PhysPinReleaseShard
                );
                let table = table.as_mut()?;
                table.unpin_batch(chunk, deferred_frees)
            };
            report.merge(chunk_report);
        }
        shard_end = shard_start;
    }
    Some(report)
}

fn pin_shard_chunk(shard_index: usize, paddrs: &[PhysAddr]) -> AxResult<()> {
    debug_assert!(!paddrs.is_empty());
    debug_assert!(paddrs.len() <= PIN_TABLE_LOCK_CHUNK_PAGES);
    debug_assert!(
        paddrs
            .iter()
            .all(|&paddr| pin_shard_index(paddr) == shard_index)
    );

    let mut table =
        crate::mm::lock_mm_diagnosed!(PINNED_FRAME_SHARDS[shard_index], PhysPinPublishShard);
    table.as_mut().ok_or(AxError::BadState)?.pin_batch(paddrs)
}

fn prepare_batch_from_paddrs(paddrs: Vec<PhysAddr>) -> AxResult<PreparedPhysicalFramePins> {
    prepare_physical_pin_registry()?;
    let mut prepared = PreparedPhysicalFramePins::try_new(paddrs.len())?;
    for paddr in paddrs {
        assert!(
            prepared.push(paddr),
            "sized physical-pin preparation rejected its own input"
        );
    }
    Ok(prepared)
}

#[cfg(test)]
fn pin_frames_admitted(paddrs: Vec<PhysAddr>) -> AxResult<PhysicalFramePins> {
    let mut prepared = prepare_batch_from_paddrs(paddrs)?;
    prepared.publish_admitted()
}

pub(crate) fn defer_frame_dealloc_if_pinned(paddr: PhysAddr, page_size: PageSize) -> bool {
    crate::mm::lock_mm_diagnosed!(
        PINNED_FRAME_SHARDS[pin_shard_index(paddr)],
        PhysPinDeallocProbeShard
    )
    .as_mut()
    .is_some_and(|table| table.defer_deallocation(paddr, page_size))
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::sync::Mutex as StdMutex;

    use super::*;

    static GLOBAL_REGISTRY_TEST_SERIAL: StdMutex<()> = StdMutex::new(());

    fn table_with_capacity(capacity: usize) -> PinnedFrameTable {
        PinnedFrameTable::try_new(capacity).unwrap()
    }

    fn reset_pin_registry() {
        for shard in &PINNED_FRAME_SHARDS {
            let table = { shard.lock().take() };
            // Keep test teardown faithful to the production ownership rule:
            // the table's large vectors are released after the shard unlocks.
            drop(table);
        }
    }

    fn paddrs_for_shard(shard_index: usize, count: usize, first_page: usize) -> Vec<PhysAddr> {
        let mut paddrs = Vec::new();
        paddrs.try_reserve_exact(count).unwrap();
        let mut page = first_page;
        while paddrs.len() < count {
            let paddr = PhysAddr::from(page.checked_mul(0x1000).unwrap());
            if pin_shard_index(paddr) == shard_index {
                paddrs.push(paddr);
            }
            page = page.checked_add(1).unwrap();
        }
        paddrs
    }

    fn assert_table_invariants(table: &PinnedFrameTable) {
        let mut seen = alloc::vec![false; table.slots.len()];
        let mut occupied = 0usize;
        for &head in &table.buckets {
            let mut cursor = head;
            while let Some(index) = cursor {
                assert!(!seen[index], "physical-pin node is linked more than once");
                seen[index] = true;
                let PinnedFrameSlot::Occupied { next, .. } = table.slots[index] else {
                    panic!("bucket chain reached a free physical-pin node");
                };
                occupied += 1;
                cursor = next;
            }
        }
        assert_eq!(occupied, table.live);

        let mut free = 0usize;
        let mut cursor = table.free_head;
        while let Some(index) = cursor {
            assert!(!seen[index], "physical-pin node is both live and free");
            seen[index] = true;
            let PinnedFrameSlot::Free { next } = table.slots[index] else {
                panic!("free list reached an occupied physical-pin node");
            };
            free += 1;
            cursor = next;
        }
        assert_eq!(free + occupied, table.slots.len());
        assert!(seen.into_iter().all(|visited| visited));
    }

    #[test]
    fn registry_prepare_failure_preserves_prefix_and_retry_skips_it() {
        let _serial = GLOBAL_REGISTRY_TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        const FAILING_SHARD: usize = 17;
        reset_pin_registry();

        let mut first_attempt = Vec::new();
        let result = prepare_physical_pin_registry_with(|shard_index| {
            // The allocation factory is the structural boundary: the target
            // shard is demonstrably unlocked whenever a table is built.
            assert!(PINNED_FRAME_SHARDS[shard_index].try_lock().is_some());
            first_attempt.push(shard_index);
            if shard_index == FAILING_SHARD {
                return Err(AxError::NoMemory);
            }
            PinnedFrameTable::try_new(pin_shard_capacity(shard_index))
        });
        assert_eq!(result, Err(AxError::NoMemory));
        assert_eq!(first_attempt, (0..=FAILING_SHARD).collect::<Vec<_>>());
        for (index, shard) in PINNED_FRAME_SHARDS.iter().enumerate() {
            assert_eq!(shard.lock().is_some(), index < FAILING_SHARD);
        }

        let mut retry = Vec::new();
        prepare_physical_pin_registry_with(|shard_index| {
            assert!(PINNED_FRAME_SHARDS[shard_index].try_lock().is_some());
            retry.push(shard_index);
            PinnedFrameTable::try_new(pin_shard_capacity(shard_index))
        })
        .unwrap();
        assert_eq!(retry, (FAILING_SHARD..PIN_TABLE_SHARDS).collect::<Vec<_>>());
        assert!(
            PINNED_FRAME_SHARDS
                .iter()
                .all(|shard| shard.lock().is_some())
        );
        reset_pin_registry();
    }

    #[test]
    fn racing_registry_loser_is_returned_after_the_shard_unlocks() {
        let _serial = GLOBAL_REGISTRY_TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        const SHARD: usize = 29;
        reset_pin_registry();

        assert!(install_prepared_pin_shard(SHARD, table_with_capacity(1)).is_none());
        let loser = install_prepared_pin_shard(SHARD, table_with_capacity(1))
            .expect("second registry initializer must retain its redundant owner");
        assert!(PINNED_FRAME_SHARDS[SHARD].try_lock().is_some());
        drop(loser);
        reset_pin_registry();
    }

    #[test]
    fn prepared_batch_push_is_bounded_and_publish_transfers_both_vectors() {
        let _serial = GLOBAL_REGISTRY_TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        reset_pin_registry();
        prepare_physical_pin_registry().unwrap();
        let mut prepared = PreparedPhysicalFramePins::try_new(3).unwrap();
        let paddr_ptr = prepared.paddrs.as_ptr();
        let paddr_capacity = prepared.paddrs.capacity();
        let deferred_ptr = prepared.deferred_frees.as_ptr();
        let deferred_capacity = prepared.deferred_frees.capacity();

        assert!(prepared.push(PhysAddr::from(0x10_0000)));
        assert!(prepared.push(PhysAddr::from(0x20_0000)));
        assert!(prepared.push(PhysAddr::from(0x30_0000)));
        assert!(!prepared.push(PhysAddr::from(0x40_0000)));
        assert_eq!(prepared.paddrs.as_ptr(), paddr_ptr);
        assert_eq!(prepared.paddrs.capacity(), paddr_capacity);

        let pins = prepared.publish_admitted().unwrap();
        assert_eq!(pins.paddrs.as_ptr(), paddr_ptr);
        assert_eq!(pins.paddrs.capacity(), paddr_capacity);
        assert_eq!(pins.deferred_frees.as_ptr(), deferred_ptr);
        assert_eq!(pins.deferred_frees.capacity(), deferred_capacity);
        assert_eq!(prepared.max_pages, 0);
        assert_eq!(prepared.paddrs.capacity(), 0);
        assert_eq!(prepared.deferred_frees.capacity(), 0);

        drop(pins);
        reset_pin_registry();
    }

    #[test]
    fn preallocated_table_is_bounded_and_duplicate_pins_share_one_slot() {
        let first = PhysAddr::from(0x1000);
        let second = PhysAddr::from(0x2000);
        let mut table = table_with_capacity(1);

        table.pin_preallocated(first).unwrap();
        table.pin_preallocated(first).unwrap();
        assert_eq!(table.live, 1);
        assert_eq!(table.pin_preallocated(second), Err(AxError::NoMemory));
        assert_eq!(table.unpin(first), Ok(None));
        assert_eq!(table.live, 1);
        assert_eq!(table.unpin(first), Ok(None));
        assert_eq!(table.live, 0);
        assert_table_invariants(&table);
    }

    #[test]
    fn deferred_free_is_returned_by_the_final_unpin() {
        let address = PhysAddr::from(0x3000);
        let mut table = table_with_capacity(1);
        table.pin_preallocated(address).unwrap();
        assert!(table.defer_deallocation(address, PageSize::Size4K));
        assert_eq!(table.unpin(address), Ok(Some(PageSize::Size4K)));
        assert_table_invariants(&table);
    }

    #[test]
    fn multi_page_batch_uses_one_pin_and_one_unpin_transaction() {
        const PAGES: usize = 8;
        let paddrs = (0..PAGES)
            .map(|index| PhysAddr::from(0x10_0000 + index * 0x1000))
            .collect::<Vec<_>>();
        let mut table = table_with_capacity(PAGES);

        table.pin_batch(&paddrs).unwrap();
        assert_eq!(table.live, PAGES);
        assert_eq!(
            table.batch_metrics,
            PinBatchMetrics {
                pin_batches: 1,
                pin_pages_requested: PAGES,
                pin_pages_committed: PAGES,
                ..PinBatchMetrics::default()
            }
        );

        let mut deferred = Vec::new();
        deferred.try_reserve_exact(PAGES).unwrap();
        assert_eq!(
            table.unpin_batch(&paddrs, &mut deferred),
            UnpinBatchReport::default()
        );
        assert!(deferred.is_empty());
        assert_eq!(table.live, 0);
        assert_eq!(table.batch_metrics.pin_batches, 1);
        assert_eq!(table.batch_metrics.unpin_batches, 1);
        assert_eq!(table.batch_metrics.unpin_pages_requested, PAGES);
        assert_eq!(table.batch_metrics.unpin_pages_completed, PAGES);
        assert_table_invariants(&table);
    }

    #[test]
    fn large_public_batch_is_split_into_bounded_registry_transactions() {
        let _serial = GLOBAL_REGISTRY_TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        const PAGES: usize = PIN_TABLE_LOCK_CHUNK_PAGES * 2 + 1;
        const SHARD: usize = 7;
        reset_pin_registry();
        let paddrs = paddrs_for_shard(SHARD, PAGES, 0x180);

        let pins = pin_frames_admitted(paddrs).unwrap();
        {
            let table = PINNED_FRAME_SHARDS[SHARD].lock();
            let table = table.as_ref().unwrap();
            assert_eq!(table.live, PAGES);
            assert_eq!(table.batch_metrics.pin_batches, 3);
            assert_eq!(table.batch_metrics.pin_pages_requested, PAGES);
            assert_eq!(table.batch_metrics.pin_pages_committed, PAGES);
        }

        drop(pins);
        {
            let mut table = PINNED_FRAME_SHARDS[SHARD].lock();
            let table = table.as_mut().unwrap();
            assert_eq!(table.live, 0);
            assert_eq!(table.batch_metrics.unpin_batches, 3);
            assert_eq!(table.batch_metrics.unpin_pages_requested, PAGES);
            assert_eq!(table.batch_metrics.unpin_pages_completed, PAGES);
            assert_table_invariants(table);
        }
        reset_pin_registry();
    }

    #[test]
    fn failed_later_chunk_rolls_back_every_committed_chunk() {
        let _serial = GLOBAL_REGISTRY_TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        const PAGES: usize = PIN_TABLE_LOCK_CHUNK_PAGES + 2;
        const SHARD: usize = 11;
        reset_pin_registry();
        let paddrs = paddrs_for_shard(SHARD, PAGES, 0x280);
        *PINNED_FRAME_SHARDS[SHARD].lock() = Some(table_with_capacity(PAGES - 1));

        assert_eq!(pin_frames_admitted(paddrs).err(), Some(AxError::NoMemory));
        {
            let table = PINNED_FRAME_SHARDS[SHARD].lock();
            let table = table.as_ref().unwrap();
            assert_eq!(table.live, 0);
            assert_eq!(table.batch_metrics.pin_batches, 2);
            assert_eq!(table.batch_metrics.rollback_batches, 1);
            assert_eq!(table.batch_metrics.rollback_pages, 1);
            assert_eq!(table.batch_metrics.unpin_batches, 1);
            assert_eq!(
                table.batch_metrics.unpin_pages_requested,
                PIN_TABLE_LOCK_CHUNK_PAGES
            );
            assert_eq!(
                table.batch_metrics.unpin_pages_completed,
                PIN_TABLE_LOCK_CHUNK_PAGES
            );
            assert_table_invariants(table);
        }
        reset_pin_registry();
    }

    #[test]
    fn shard_capacities_preserve_the_exact_metadata_reserve() {
        let total = (0..PIN_TABLE_SHARDS).map(pin_shard_capacity).sum::<usize>();
        assert_eq!(total, PIN_METADATA_SLOTS);
        assert!((0..PIN_TABLE_SHARDS).all(|index| pin_shard_capacity(index) != 0));
    }

    #[test]
    fn exact_system_limit_contiguous_batch_survives_shard_skew() {
        let _serial = GLOBAL_REGISTRY_TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        reset_pin_registry();
        let paddrs = (0..MAX_PINNED_FRAMES)
            .map(|page| PhysAddr::from((page + 1) * 0x1000))
            .collect::<Vec<_>>();

        let pins = pin_frames_admitted(paddrs).unwrap();
        let mut total_live = 0usize;
        for (index, shard) in PINNED_FRAME_SHARDS.iter().enumerate() {
            let table = shard.lock();
            let table = table.as_ref().unwrap();
            assert!(table.live <= pin_shard_capacity(index));
            total_live += table.live;
        }
        assert_eq!(total_live, MAX_PINNED_FRAMES);

        drop(pins);
        assert!(PINNED_FRAME_SHARDS.iter().all(|shard| {
            let table = shard.lock();
            table.as_ref().is_some_and(|table| table.live == 0)
        }));
        reset_pin_registry();
    }

    #[test]
    fn pathological_single_shard_pressure_fails_atomically_at_its_bound() {
        let _serial = GLOBAL_REGISTRY_TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        const SHARD: usize = 23;
        reset_pin_registry();
        let paddrs = paddrs_for_shard(SHARD, pin_shard_capacity(SHARD) + 1, 0x580);
        let mut prepared = prepare_batch_from_paddrs(paddrs).unwrap();
        let paddr_ptr = prepared.paddrs.as_ptr();
        let paddr_capacity = prepared.paddrs.capacity();
        let deferred_ptr = prepared.deferred_frees.as_ptr();
        let deferred_capacity = prepared.deferred_frees.capacity();

        // Pathological skew is a bounded mechanism rejection. Publication
        // rolls back atomically and retains both large owners so the adapter
        // can unlock AddrSpace, drop this preparation, and use its copy path.
        assert_eq!(prepared.publish_admitted().err(), Some(AxError::NoMemory));
        assert_eq!(prepared.paddrs.as_ptr(), paddr_ptr);
        assert_eq!(prepared.paddrs.capacity(), paddr_capacity);
        assert_eq!(prepared.deferred_frees.as_ptr(), deferred_ptr);
        assert_eq!(prepared.deferred_frees.capacity(), deferred_capacity);
        assert!(prepared.publication_failed);
        assert_eq!(prepared.publish_admitted().err(), Some(AxError::BadState));
        {
            let table = PINNED_FRAME_SHARDS[SHARD].lock();
            let table = table.as_ref().unwrap();
            assert_eq!(table.live, 0);
            assert_table_invariants(table);
        }
        drop(prepared);
        reset_pin_registry();
    }

    #[test]
    fn distinct_shards_have_independent_irq_safe_locks() {
        let _serial = GLOBAL_REGISTRY_TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        const FIRST: usize = 5;
        const SECOND: usize = 37;
        reset_pin_registry();
        *PINNED_FRAME_SHARDS[FIRST].lock() = Some(table_with_capacity(1));
        *PINNED_FRAME_SHARDS[SECOND].lock() = Some(table_with_capacity(1));

        let first = PINNED_FRAME_SHARDS[FIRST].lock();
        assert!(PINNED_FRAME_SHARDS[SECOND].try_lock().is_some());
        drop(first);
        reset_pin_registry();
    }

    #[test]
    fn failed_shard_rolls_back_every_previously_committed_shard() {
        let _serial = GLOBAL_REGISTRY_TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        const FIRST: usize = 3;
        const FAILING: usize = 19;
        const FIRST_PAGES: usize = 5;
        reset_pin_registry();
        let mut paddrs = paddrs_for_shard(FIRST, FIRST_PAGES, 0x380);
        paddrs.extend(paddrs_for_shard(FAILING, 2, 0x480));
        *PINNED_FRAME_SHARDS[FAILING].lock() = Some(table_with_capacity(1));

        assert_eq!(pin_frames_admitted(paddrs).err(), Some(AxError::NoMemory));
        {
            let table = PINNED_FRAME_SHARDS[FIRST].lock();
            let table = table.as_ref().unwrap();
            assert_eq!(table.live, 0);
            assert_eq!(table.batch_metrics.pin_batches, 1);
            assert_eq!(table.batch_metrics.unpin_batches, 1);
            assert_eq!(table.batch_metrics.unpin_pages_completed, FIRST_PAGES);
            assert_table_invariants(table);
        }
        {
            let table = PINNED_FRAME_SHARDS[FAILING].lock();
            let table = table.as_ref().unwrap();
            assert_eq!(table.live, 0);
            assert_eq!(table.batch_metrics.pin_batches, 1);
            assert_eq!(table.batch_metrics.rollback_batches, 1);
            assert_eq!(table.batch_metrics.rollback_pages, 1);
            assert_eq!(table.batch_metrics.unpin_batches, 0);
            assert_table_invariants(table);
        }
        reset_pin_registry();
    }

    #[test]
    fn failed_batch_rolls_back_its_complete_pinned_prefix() {
        let paddrs = [
            PhysAddr::from(0x20_0000),
            PhysAddr::from(0x21_0000),
            PhysAddr::from(0x22_0000),
        ];
        let mut table = table_with_capacity(2);

        assert_eq!(table.pin_batch(&paddrs), Err(AxError::NoMemory));
        assert_eq!(table.live, 0);
        assert_eq!(table.batch_metrics.pin_batches, 1);
        assert_eq!(table.batch_metrics.pin_pages_requested, paddrs.len());
        assert_eq!(table.batch_metrics.pin_pages_committed, 0);
        assert_eq!(table.batch_metrics.rollback_batches, 1);
        assert_eq!(table.batch_metrics.rollback_pages, 2);
        assert_table_invariants(&table);

        table.pin_batch(&paddrs[..2]).unwrap();
        assert_eq!(table.live, 2);
        assert_table_invariants(&table);
    }

    #[test]
    fn batch_release_preserves_duplicate_refcounts_and_defers_each_frame_once() {
        let first = PhysAddr::from(0x30_0000);
        let second = PhysAddr::from(0x31_0000);
        let paddrs = [first, first, second];
        let mut table = table_with_capacity(2);
        table.pin_batch(&paddrs).unwrap();
        assert_eq!(table.live, 2);
        assert!(table.defer_deallocation(first, PageSize::Size4K));
        assert!(table.defer_deallocation(second, PageSize::Size2M));

        let mut deferred = Vec::new();
        deferred.try_reserve_exact(paddrs.len()).unwrap();
        assert_eq!(
            table.unpin_batch(&paddrs, &mut deferred),
            UnpinBatchReport::default()
        );
        assert_eq!(
            deferred,
            alloc::vec![(first, PageSize::Size4K), (second, PageSize::Size2M)]
        );
        assert_eq!(table.live, 0);
        assert_table_invariants(&table);
    }

    #[test]
    fn collision_chain_unlinks_head_middle_and_tail() {
        let mut table = table_with_capacity(4);
        let target_bucket = table.bucket_index(PhysAddr::from(0x1000));
        let mut colliders = [PhysAddr::from(0); 3];
        let mut found = 0usize;
        for page in 1..1024 {
            let address = PhysAddr::from(page * 0x1000);
            if table.bucket_index(address) == target_bucket {
                colliders[found] = address;
                found += 1;
                if found == colliders.len() {
                    break;
                }
            }
        }
        assert_eq!(found, colliders.len());

        for address in colliders {
            table.pin_preallocated(address).unwrap();
        }
        assert_table_invariants(&table);
        table.unpin(colliders[1]).unwrap();
        assert!(table.find_node(colliders[0]).is_some());
        assert!(table.find_node(colliders[2]).is_some());
        assert_table_invariants(&table);
        table.unpin(colliders[2]).unwrap();
        assert!(table.find_node(colliders[0]).is_some());
        assert_table_invariants(&table);
        table.unpin(colliders[0]).unwrap();
        assert_table_invariants(&table);
    }

    #[test]
    fn full_capacity_is_reusable_across_long_running_churn() {
        const CAPACITY: usize = 16;
        let mut table = table_with_capacity(CAPACITY);

        for round in 0..128 {
            let base = 0x10_0000 + round * 0x40_0000;
            for index in 0..CAPACITY {
                table
                    .pin_preallocated(PhysAddr::from(base + index * 0x1000))
                    .unwrap();
            }
            assert_eq!(table.live, CAPACITY);
            assert_table_invariants(&table);

            for index in (0..CAPACITY).step_by(2) {
                table.unpin(PhysAddr::from(base + index * 0x1000)).unwrap();
            }
            for index in 0..CAPACITY / 2 {
                table
                    .pin_preallocated(PhysAddr::from(base + 0x20_0000 + index * 0x1000))
                    .unwrap();
            }
            assert_eq!(table.live, CAPACITY);
            assert_table_invariants(&table);

            for index in (1..CAPACITY).step_by(2) {
                table.unpin(PhysAddr::from(base + index * 0x1000)).unwrap();
            }
            for index in 0..CAPACITY / 2 {
                table
                    .unpin(PhysAddr::from(base + 0x20_0000 + index * 0x1000))
                    .unwrap();
            }
            assert_eq!(table.live, 0);
            assert_table_invariants(&table);
        }
    }
}
