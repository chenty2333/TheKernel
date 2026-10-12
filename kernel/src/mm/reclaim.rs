//! Anonymous-page LRU and reclaim.
//!
//! Ports the classic two-list model of Linux `mm/swap.c` (`lru_add`,
//! `folio_mark_accessed`, `folio_activate`), `mm/vmscan.c`
//! (`shrink_active_list`, `shrink_inactive_list`, `shrink_folio_list`,
//! `isolate_lru_folios`, `inactive_is_low`) and the rmap checks of
//! `mm/rmap.c` (`folio_referenced`, `try_to_unmap_one`).
//!
//! Reverse mapping: each tracked frame records the (address-space id, vaddr)
//! of the exclusive anonymous leaf that maps it. The record is a hint. Every
//! use revalidates it under the owner's address-space lock, which is the same
//! re-check an rmap walk performs on the PTE it found. Owners are resolved via
//! the live address-space registry in `swap.rs`.
//!
//! Owner locks are only trylocked, as Linux `folio_trylock()`: reclaim can run
//! from a fault that already holds the faulting address space, and a busy
//! owner is requeued rather than waited for.
//!
//! Lock order: the LRU lock is never held while an address-space lock is
//! taken; the fault path takes the address-space lock first and then the LRU
//! lock to add a page.

use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use axalloc::global_allocator;
use axerrno::{AxError, AxResult};
use axsync::Mutex;
use memory_addr::{PhysAddr, VirtAddr};
use spin::Lazy;

/// Linux `SWAP_CLUSTER_MAX`: the batch size of one scan step.
const SWAP_CLUSTER_MAX: usize = 32;
/// Linux `DEF_SWAPPINESS`.
const DEFAULT_SWAPPINESS: u32 = 60;
/// Linux `MAX_SWAPPINESS` (vm.swappiness accepts 0 through 200).
const MAX_SWAPPINESS: u32 = 200;
/// Pages a direct-reclaim attempt tries to free from a failing fault.
const DIRECT_RECLAIM_PAGES: usize = SWAP_CLUSTER_MAX;

/// Who asked for reclaim, which selects the `pgscan_*` / `pgsteal_*` counters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReclaimContext {
    Kswapd,
    Direct,
}

impl ReclaimContext {
    const fn index(self) -> usize {
        match self {
            Self::Kswapd => 0,
            Self::Direct => 1,
        }
    }
}

/// Outcome of trying to swap out one anonymous leaf.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VictimReclaim {
    /// The leaf was swapped out (or discarded for `MAP_DROPPABLE`).
    Reclaimed,
    /// No swap area is active; no anonymous leaf can be reclaimed.
    NoSwapArea,
    /// The leaf is not reclaimable right now.
    NotEligible,
}

/// Result of probing an owner for a frame's reference state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Probe {
    /// The reverse-map record no longer describes a reclaimable leaf.
    Gone,
    /// The owner is locked by someone else; try again later.
    Busy,
    /// The leaf was referenced since the last test (accessed bit cleared).
    Referenced,
    /// The leaf was not referenced.
    Idle,
}

#[derive(Clone, Copy, Debug)]
struct Frame {
    owner: u64,
    vaddr: usize,
    active: bool,
    referenced: bool,
    /// Position in its list; ages are ordered by insertion sequence.
    seq: u64,
}

#[derive(Default)]
struct AnonLru {
    frames: BTreeMap<usize, Frame>,
    inactive: BTreeMap<u64, usize>,
    active: BTreeMap<u64, usize>,
    next_seq: u64,
}

impl AnonLru {
    fn list_mut(&mut self, active: bool) -> &mut BTreeMap<u64, usize> {
        if active {
            &mut self.active
        } else {
            &mut self.inactive
        }
    }

    fn len(&self, active: bool) -> usize {
        if active {
            self.active.len()
        } else {
            self.inactive.len()
        }
    }

    fn detach(&mut self, paddr: usize) -> Option<Frame> {
        let frame = self.frames.remove(&paddr)?;
        self.list_mut(frame.active).remove(&frame.seq);
        Some(frame)
    }

    /// Appends `frame` to the tail of its list, the youngest end.
    fn attach(&mut self, paddr: usize, mut frame: Frame) {
        self.next_seq = self.next_seq.wrapping_add(1);
        frame.seq = self.next_seq;
        self.list_mut(frame.active).insert(frame.seq, paddr);
        self.frames.insert(paddr, frame);
    }

    /// Removes the oldest entry of one list.
    fn pop_oldest(&mut self, active: bool) -> Option<(usize, Frame)> {
        let (&seq, &paddr) = self.list_mut(active).iter().next()?;
        self.list_mut(active).remove(&seq);
        let frame = self.frames.remove(&paddr)?;
        Some((paddr, frame))
    }
}

static LRU: Lazy<Mutex<AnonLru>> = Lazy::new(|| Mutex::new(AnonLru::default()));

static SWAPPINESS: AtomicU32 = AtomicU32::new(DEFAULT_SWAPPINESS);
static LRU_ADDS: AtomicU64 = AtomicU64::new(0);
static PGSCAN: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
static PGSTEAL: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
static PGACTIVATE: AtomicU64 = AtomicU64::new(0);
static PGDEACTIVATE: AtomicU64 = AtomicU64::new(0);

/// Snapshot of the reclaim counters exported through `/proc/vmstat`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReclaimCounters {
    pub pgscan_kswapd: u64,
    pub pgscan_direct: u64,
    pub pgsteal_kswapd: u64,
    pub pgsteal_direct: u64,
    pub pgactivate: u64,
    pub pgdeactivate: u64,
    pub lru_adds: u64,
    pub inactive_anon: u64,
    pub active_anon: u64,
}

pub fn reclaim_counters() -> ReclaimCounters {
    let (inactive_anon, active_anon) = {
        let lru = LRU.lock();
        (lru.len(false) as u64, lru.len(true) as u64)
    };
    ReclaimCounters {
        pgscan_kswapd: PGSCAN[ReclaimContext::Kswapd.index()].load(Ordering::Relaxed),
        pgscan_direct: PGSCAN[ReclaimContext::Direct.index()].load(Ordering::Relaxed),
        pgsteal_kswapd: PGSTEAL[ReclaimContext::Kswapd.index()].load(Ordering::Relaxed),
        pgsteal_direct: PGSTEAL[ReclaimContext::Direct.index()].load(Ordering::Relaxed),
        pgactivate: PGACTIVATE.load(Ordering::Relaxed),
        pgdeactivate: PGDEACTIVATE.load(Ordering::Relaxed),
        lru_adds: LRU_ADDS.load(Ordering::Relaxed),
        inactive_anon,
        active_anon,
    }
}

/// `vm.swappiness`: the relative weight of anonymous against file reclaim.
pub fn swappiness() -> u32 {
    SWAPPINESS.load(Ordering::Relaxed)
}

pub fn set_swappiness(value: u32) -> AxResult {
    if value > MAX_SWAPPINESS {
        return Err(AxError::InvalidInput);
    }
    SWAPPINESS.store(value, Ordering::Relaxed);
    Ok(())
}

/// Linux `lru_add()` for an anonymous leaf: a new page joins the inactive
/// list unreferenced. Re-adding the same (owner, vaddr) keeps its position.
pub(crate) fn lru_add_anonymous(paddr: PhysAddr, owner: u64, vaddr: VirtAddr) {
    let paddr = paddr.as_usize();
    let vaddr = vaddr.as_usize();
    let mut lru = LRU.lock();
    if let Some(existing) = lru.frames.get(&paddr)
        && existing.owner == owner
        && existing.vaddr == vaddr
    {
        return;
    }
    lru.detach(paddr);
    lru.attach(
        paddr,
        Frame {
            owner,
            vaddr,
            active: false,
            referenced: false,
            seq: 0,
        },
    );
    LRU_ADDS.fetch_add(1, Ordering::Relaxed);
}

/// Linux `folio_mark_accessed()`: a second reference to an inactive page
/// activates it; a first reference only marks it.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn mark_accessed(paddr: PhysAddr) {
    let paddr = paddr.as_usize();
    let mut lru = LRU.lock();
    let Some(frame) = lru.frames.get(&paddr).copied() else {
        return;
    };
    if frame.active || !frame.referenced {
        lru.frames.get_mut(&paddr).expect("tracked frame").referenced = true;
        return;
    }
    let Some(mut frame) = lru.detach(paddr) else {
        return;
    };
    frame.active = true;
    frame.referenced = false;
    lru.attach(paddr, frame);
    PGACTIVATE.fetch_add(1, Ordering::Relaxed);
}

/// Linux `inactive_is_low()` for the anonymous lists. The ratio is
/// `sqrt(10 * GiB)`, floored at one, as in `mm/vmscan.c`.
fn inactive_is_low(inactive: usize, active: usize, total_pages: usize) -> bool {
    let gib = (total_pages / (1 << (30 - 12))) as u64;
    let ratio = (10 * gib).isqrt().max(1) as usize;
    inactive.saturating_mul(ratio) < active
}

fn total_pages() -> usize {
    let allocator = global_allocator();
    allocator
        .used_pages()
        .saturating_add(allocator.available_pages())
}

fn requeue(paddr: usize, frame: Frame) {
    LRU.lock().attach(paddr, frame);
}

fn activate(paddr: usize, mut frame: Frame) {
    frame.active = true;
    frame.referenced = false;
    requeue(paddr, frame);
    PGACTIVATE.fetch_add(1, Ordering::Relaxed);
}

/// Resolves the owner of `frame` and tests (and clears) its accessed bit.
/// Owners are trylocked so that reclaim never blocks behind a fault.
fn probe(paddr: usize, frame: &Frame) -> Probe {
    let Some(owner) = super::swap::live_address_space(frame.owner) else {
        return Probe::Gone;
    };
    let Some(mut aspace) = owner.try_lock() else {
        return Probe::Busy;
    };
    let page = VirtAddr::from(frame.vaddr);
    if !aspace.anonymous_frame_is(page, PhysAddr::from(paddr)) {
        return Probe::Gone;
    }
    match aspace.test_and_clear_young(page) {
        Ok(true) => Probe::Referenced,
        Ok(false) => Probe::Idle,
        Err(_) => Probe::Gone,
    }
}

/// Linux `shrink_active_list()` for one batch: referenced active pages rotate
/// to the young end, unreferenced ones are deactivated.
fn shrink_active_list(batch: usize) -> usize {
    let mut examined = 0;
    while examined < batch {
        let popped = LRU.lock().pop_oldest(true);
        let Some((paddr, mut frame)) = popped else {
            break;
        };
        examined += 1;
        if frame.referenced {
            frame.referenced = false;
            requeue(paddr, frame);
            continue;
        }
        match probe(paddr, &frame) {
            Probe::Gone => {}
            Probe::Busy | Probe::Referenced => requeue(paddr, frame),
            Probe::Idle => {
                frame.active = false;
                requeue(paddr, frame);
                PGDEACTIVATE.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    examined
}

/// Linux `shrink_folio_list()` for one inactive victim.
fn try_reclaim_victim(paddr: usize, frame: &Frame) -> Result<VictimReclaim, Probe> {
    let Some(owner) = super::swap::live_address_space(frame.owner) else {
        return Err(Probe::Gone);
    };
    let Some(mut aspace) = owner.try_lock() else {
        return Err(Probe::Busy);
    };
    let page = VirtAddr::from(frame.vaddr);
    if !aspace.anonymous_frame_is(page, PhysAddr::from(paddr)) {
        return Err(Probe::Gone);
    }
    match aspace.test_and_clear_young(page) {
        Ok(true) => return Err(Probe::Referenced),
        Ok(false) => {}
        Err(_) => return Err(Probe::Gone),
    }
    match aspace.reclaim_resident_anonymous_page(page) {
        Ok(VictimReclaim::Reclaimed) => Ok(VictimReclaim::Reclaimed),
        Ok(VictimReclaim::NoSwapArea) => Ok(VictimReclaim::NoSwapArea),
        Ok(VictimReclaim::NotEligible) | Err(_) => Ok(VictimReclaim::NotEligible),
    }
}

/// Reclaims up to `target` anonymous pages by swapping out cold leaves from
/// the oldest end of the inactive list. Returns the number reclaimed.
///
/// A referenced victim is activated (Linux `folio_referenced` keeps it
/// resident), a busy owner is requeued, and a leaf the pageout path refuses
/// is dropped from the list until its next fault re-tracks it.
pub(crate) fn reclaim_anonymous(target: usize, context: ReclaimContext) -> usize {
    if target == 0 {
        return 0;
    }
    let total = total_pages();
    // Linux `nr_to_scan`: one call examines each page on the inactive list at
    // most once, so a page activated here is not re-tested in the same call.
    let (mut budget, _) = {
        let lru = LRU.lock();
        (lru.len(false), lru.len(true))
    };
    let mut reclaimed = 0;
    while reclaimed < target && budget > 0 {
        let (inactive, active) = {
            let lru = LRU.lock();
            (lru.len(false), lru.len(true))
        };
        if inactive_is_low(inactive, active, total) {
            shrink_active_list(SWAP_CLUSTER_MAX);
        }
        let popped = LRU.lock().pop_oldest(false);
        let Some((paddr, mut frame)) = popped else {
            // Nothing idle is left; refill the inactive list from the active
            // list for the next caller.
            shrink_active_list(SWAP_CLUSTER_MAX);
            break;
        };
        budget -= 1;
        PGSCAN[context.index()].fetch_add(1, Ordering::Relaxed);
        if frame.referenced {
            frame.referenced = false;
            activate(paddr, frame);
            continue;
        }
        match try_reclaim_victim(paddr, &frame) {
            Ok(VictimReclaim::Reclaimed) => {
                reclaimed += 1;
                PGSTEAL[context.index()].fetch_add(1, Ordering::Relaxed);
            }
            Ok(VictimReclaim::NotEligible) => {}
            Ok(VictimReclaim::NoSwapArea) => {
                requeue(paddr, frame);
                break;
            }
            Err(Probe::Referenced) => activate(paddr, frame),
            Err(Probe::Busy) => requeue(paddr, frame),
            Err(_) => {}
        }
    }
    reclaimed
}

/// Direct reclaim from a failing anonymous population. Returns the number of
/// pages freed; a non-zero result means the caller should retry.
pub(crate) fn direct_reclaim_anonymous() -> usize {
    reclaim_anonymous(DIRECT_RECLAIM_PAGES, ReclaimContext::Direct)
}

#[cfg(test)]
pub(crate) fn lru_is_active_for_test(paddr: PhysAddr) -> Option<bool> {
    LRU.lock()
        .frames
        .get(&paddr.as_usize())
        .map(|frame| frame.active)
}

#[cfg(test)]
pub(crate) fn lru_contains_for_test(paddr: PhysAddr) -> bool {
    LRU.lock().frames.contains_key(&paddr.as_usize())
}

#[cfg(test)]
mod tests {
    use alloc::{sync::Arc, vec::Vec};
    use core::sync::atomic::{AtomicU32, AtomicUsize};

    use axfs::OpenOptions;
    use axhal::paging::MappingFlags;
    use axfs_ng_vfs::{FsName, Location, Mountpoint, NodePermission, NodeType};
    use axsync::Mutex;
    use memory_addr::{PAGE_SIZE_4K, VirtAddr, VirtAddrRange};
    use mmu_notifier::{Event, Range, Subscriber};
    use page_table_multiarch::PageSize;

    use super::*;
    use crate::{
        mm::{AddrSpace, Backend, swap},
        pseudofs::tmp::MemoryFs,
    };

    const PAGE: usize = PAGE_SIZE_4K;
    const SWAP_PAGE: usize = 4096;

    /// Records invalidations delivered to one interval subscriber.
    struct Probe {
        hits: AtomicUsize,
        last_event: AtomicU32,
    }

    impl Subscriber for Probe {
        fn invalidate(&self, range: &Range, seq: u64) -> bool {
            assert_eq!(seq & 1, 1);
            self.last_event.store(range.event as u32, Ordering::Release);
            self.hits.fetch_add(1, Ordering::AcqRel);
            true
        }

        fn seq(&self) -> u64 {
            0
        }

        fn set_seq(&self, _seq: u64) {}
    }

    /// A valid one-area swap file, as `swap::tests` builds for `swapon`.
    fn make_swap_file() -> Location {
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let location = mount
            .root_location()
            .create(
                FsName::new(b"reclaim-swap"),
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open_loc(location.clone())
            .unwrap()
            .into_file()
            .unwrap();
        file.set_len((SWAP_PAGE * 9) as u64).unwrap();
        let mut header = [0u8; SWAP_PAGE];
        header[1024..1028].copy_from_slice(&1u32.to_le_bytes());
        header[1028..1032].copy_from_slice(&8u32.to_le_bytes());
        header[SWAP_PAGE - 10..].copy_from_slice(b"SWAPSPACE2");
        assert_eq!(file.write_at(&header[..], 0).unwrap(), SWAP_PAGE);
        drop(file);
        location
    }

    fn cold_and_hot_space(base: VirtAddr) -> Arc<Mutex<AddrSpace>> {
        let mut aspace = AddrSpace::new_empty(base, 0x20000).unwrap();
        let flags = MappingFlags::USER
            | MappingFlags::READ
            | MappingFlags::WRITE;
        aspace
            .map(
                base,
                PAGE * 8,
                flags,
                true,
                Backend::new_alloc(base, PageSize::Size4K),
            )
            .unwrap();
        let aspace = Arc::new(Mutex::new(aspace));
        swap::register_address_space(&aspace);
        {
            let mut guard = aspace.lock();
            // Pages 0..4 go cold (accessed bit clear), 4..8 stay hot.
            guard
                .cold_resident_pages(VirtAddrRange::new(base, base + 4 * PAGE))
                .unwrap();
            for index in 4..8 {
                guard.set_accessed_for_test(base + index * PAGE);
            }
        }
        aspace
    }

    fn frame_of(aspace: &Arc<Mutex<AddrSpace>>, page: VirtAddr) -> PhysAddr {
        let guard = aspace.lock();
        guard.pt_query_for_test(page)
    }

    #[test]
    fn kswapd_reclaims_cold_anonymous_pages_and_activates_referenced_ones() {
        let _context = crate::test_support::scheduler_test_context();
        let location = make_swap_file();
        swap::activate(location.clone(), 0).unwrap();
        let base = VirtAddr::from(0x1000);
        let aspace = cold_and_hot_space(base);
        let key = aspace.lock().address_space_id().get();
        let frames: Vec<PhysAddr> = (0..8).map(|i| frame_of(&aspace, base + i * PAGE)).collect();
        let before = reclaim_counters();

        // Notifier subscribers over the cold half must see the reclaim unmap.
        let probe = Arc::new(Probe {
            hits: AtomicUsize::new(0),
            last_event: AtomicU32::new(0),
        });
        mmu_notifier::subscribe(key, 0x7a11, base.as_usize(), base.as_usize() + 4 * PAGE - 1, probe.clone())
            .unwrap();

        // The reclaimer runs without the address-space lock held.
        let reclaimed = reclaim_anonymous(8, ReclaimContext::Kswapd);
        assert_eq!(reclaimed, 4, "only the four cold pages may be swapped out");

        {
            let guard = aspace.lock();
            for index in 0..4 {
                let page = base + index * PAGE;
                assert!(guard.is_swapped_for_test(page), "cold page {index} not swapped");
                assert!(!lru_contains_for_test(frames[index]));
            }
            for index in 4..8 {
                let page = base + index * PAGE;
                assert!(!guard.is_swapped_for_test(page), "hot page {index} was reclaimed");
                // Still tracked: activated, and possibly deactivated again by
                // `inactive_is_low` once its accessed bit was consumed.
                assert!(lru_contains_for_test(frames[index]));
            }
        }
        assert!(probe.hits.load(Ordering::Acquire) >= 4);
        assert_eq!(probe.last_event.load(Ordering::Acquire), Event::Clear as u32);
        mmu_notifier::unsubscribe(key, 0x7a11);

        let after = reclaim_counters();
        assert_eq!(after.pgsteal_kswapd - before.pgsteal_kswapd, 4);
        assert!(after.pgscan_kswapd - before.pgscan_kswapd >= 8);
        assert!(after.pgactivate - before.pgactivate >= 4);

        // A swapped-out page faults back in and is tracked again.
        let page = base;
        assert!(aspace.lock().handle_page_fault(page, MappingFlags::READ));
        assert!(lru_contains_for_test(frame_of(&aspace, page)));

        swap::deactivate(&location).unwrap();
    }

    #[test]
    fn mark_accessed_activates_on_second_reference() {
        let frame = PhysAddr::from(0xdead_0000usize);
        lru_add_anonymous(frame, u64::MAX - 1, VirtAddr::from(0x4000));
        assert_eq!(lru_is_active_for_test(frame), Some(false));
        mark_accessed(frame);
        assert_eq!(lru_is_active_for_test(frame), Some(false));
        mark_accessed(frame);
        assert_eq!(lru_is_active_for_test(frame), Some(true));
        // Adding the same owner and vaddr again keeps the active position.
        lru_add_anonymous(frame, u64::MAX - 1, VirtAddr::from(0x4000));
        assert_eq!(lru_is_active_for_test(frame), Some(true));
    }

    #[test]
    fn inactive_is_low_follows_linux_ratio() {
        // Below 1 GiB the ratio is one: active may not exceed inactive.
        assert!(!inactive_is_low(4, 4, 1 << 17));
        assert!(inactive_is_low(3, 4, 1 << 17));
        // At 1 GiB the ratio is sqrt(10) = 3.
        assert!(inactive_is_low(2, 7, 1 << 18));
        assert!(!inactive_is_low(2, 6, 1 << 18));
        // At 40 GiB the ratio is sqrt(400) = 20.
        assert!(inactive_is_low(1, 21, 40 << 18));
        assert!(!inactive_is_low(1, 20, 40 << 18));
    }

    #[test]
    fn swappiness_is_bounded_like_vm_swappiness() {
        assert_eq!(swappiness(), 60);
        assert!(set_swappiness(200).is_ok());
        assert!(set_swappiness(201).is_err());
        assert!(set_swappiness(60).is_ok());
    }
}
