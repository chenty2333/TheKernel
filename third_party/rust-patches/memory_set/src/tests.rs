use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use memory_addr::{MemoryAddr, VirtAddr, va_range};

use crate::{
    DeferredUnmapBackend, MappingBackend, MappingError, MappingLineage, MemoryArea, MemorySet,
};

const MAX_ADDR: usize = 0x10000;

type MockFlags = u8;
type MockPageTable = [MockFlags; MAX_ADDR];

#[derive(Clone)]
struct MockBackend;

#[derive(Clone)]
struct MergeBackend;

#[derive(Clone)]
struct FailingProtectBackend {
    preflight_calls: Arc<AtomicUsize>,
    protect_calls: Arc<AtomicUsize>,
    reject_start: usize,
}

#[derive(Clone)]
struct RejectingUnmapBackend {
    preflight_calls: Arc<AtomicUsize>,
    unmap_calls: Arc<AtomicUsize>,
    reject_start: usize,
}

#[derive(Clone)]
struct FailingCommitBackend;

#[derive(Clone)]
struct FailingProtectCommitBackend;

#[derive(Default)]
struct DeferredSignals {
    preflight_calls: Arc<AtomicUsize>,
    deferred_calls: Arc<AtomicUsize>,
    live_retirements: Arc<AtomicUsize>,
}

impl DeferredSignals {
    fn backend(&self, owner: Arc<()>, reject_start: Option<usize>) -> DeferredBackend {
        DeferredBackend {
            _area_owner: owner,
            preflight_calls: self.preflight_calls.clone(),
            deferred_calls: self.deferred_calls.clone(),
            live_retirements: self.live_retirements.clone(),
            reject_start,
            fail_commit_start: None,
        }
    }
}

#[derive(Clone)]
struct DeferredBackend {
    _area_owner: Arc<()>,
    preflight_calls: Arc<AtomicUsize>,
    deferred_calls: Arc<AtomicUsize>,
    live_retirements: Arc<AtomicUsize>,
    reject_start: Option<usize>,
    fail_commit_start: Option<usize>,
}

impl DeferredBackend {
    fn fail_commit_at(mut self, start: usize) -> Self {
        self.fail_commit_start = Some(start);
        self
    }
}

struct TrackedRetirement {
    live_retirements: Arc<AtomicUsize>,
}

impl TrackedRetirement {
    fn new(live_retirements: Arc<AtomicUsize>) -> Self {
        live_retirements.fetch_add(1, Ordering::Relaxed);
        Self { live_retirements }
    }
}

impl Drop for TrackedRetirement {
    fn drop(&mut self) {
        let previous = self.live_retirements.fetch_sub(1, Ordering::Relaxed);
        assert!(previous > 0);
    }
}

type MockMemorySet = MemorySet<MockBackend>;
type MergeMemorySet = MemorySet<MergeBackend>;

fn lineage(raw: u64) -> MappingLineage {
    MappingLineage::new(raw).unwrap()
}

fn tracked_area<B>(start: VirtAddr, size: usize, flags: MockFlags, backend: B) -> MemoryArea<B>
where
    B: MappingBackend<Addr = VirtAddr, Flags = MockFlags>,
{
    MemoryArea::new_with_lineage(
        start,
        size,
        flags,
        backend,
        lineage(start.as_usize() as u64 + 2),
    )
}

impl MappingBackend for MockBackend {
    type Addr = VirtAddr;
    type Flags = MockFlags;
    type PageTable = MockPageTable;

    fn map(&self, start: VirtAddr, size: usize, flags: MockFlags, pt: &mut MockPageTable) -> bool {
        for entry in pt.iter_mut().skip(start.as_usize()).take(size) {
            if *entry != 0 {
                return false;
            }
            *entry = flags;
        }
        true
    }

    fn preflight_unmap(&self, start: VirtAddr, size: usize, pt: &MockPageTable) -> bool {
        pt.iter()
            .skip(start.as_usize())
            .take(size)
            .all(|entry| *entry != 0)
    }

    fn unmap(&self, start: VirtAddr, size: usize, pt: &mut MockPageTable) -> bool {
        for entry in pt.iter_mut().skip(start.as_usize()).take(size) {
            if *entry == 0 {
                return false;
            }
            *entry = 0;
        }
        true
    }

    fn protect(
        &self,
        start: VirtAddr,
        size: usize,
        new_flags: MockFlags,
        pt: &mut MockPageTable,
    ) -> bool {
        for entry in pt.iter_mut().skip(start.as_usize()).take(size) {
            if *entry == 0 {
                return false;
            }
            *entry = new_flags;
        }
        true
    }
}

impl MappingBackend for MergeBackend {
    type Addr = VirtAddr;
    type Flags = MockFlags;
    type PageTable = MockPageTable;

    fn map(&self, start: VirtAddr, size: usize, flags: MockFlags, pt: &mut MockPageTable) -> bool {
        MockBackend.map(start, size, flags, pt)
    }

    fn preflight_unmap(&self, start: VirtAddr, size: usize, pt: &MockPageTable) -> bool {
        MockBackend.preflight_unmap(start, size, pt)
    }

    fn unmap(&self, start: VirtAddr, size: usize, pt: &mut MockPageTable) -> bool {
        MockBackend.unmap(start, size, pt)
    }

    fn protect(
        &self,
        start: VirtAddr,
        size: usize,
        new_flags: MockFlags,
        pt: &mut MockPageTable,
    ) -> bool {
        MockBackend.protect(start, size, new_flags, pt)
    }

    fn can_merge(&self, _other: &Self) -> bool {
        true
    }
}

impl MappingBackend for FailingProtectBackend {
    type Addr = VirtAddr;
    type Flags = MockFlags;
    type PageTable = MockPageTable;

    fn map(&self, start: VirtAddr, size: usize, flags: MockFlags, pt: &mut MockPageTable) -> bool {
        MockBackend.map(start, size, flags, pt)
    }

    fn preflight_unmap(&self, start: VirtAddr, size: usize, pt: &MockPageTable) -> bool {
        MockBackend.preflight_unmap(start, size, pt)
    }

    fn unmap(&self, start: VirtAddr, size: usize, pt: &mut MockPageTable) -> bool {
        MockBackend.unmap(start, size, pt)
    }

    fn preflight_protect(
        &self,
        start: VirtAddr,
        size: usize,
        new_flags: MockFlags,
        pt: &MockPageTable,
    ) -> bool {
        self.preflight_calls.fetch_add(1, Ordering::Relaxed);
        start.as_usize() != self.reject_start
            && new_flags != 0
            && pt
                .iter()
                .skip(start.as_usize())
                .take(size)
                .all(|entry| *entry != 0)
    }

    fn protect(
        &self,
        start: VirtAddr,
        size: usize,
        new_flags: MockFlags,
        pt: &mut MockPageTable,
    ) -> bool {
        self.protect_calls.fetch_add(1, Ordering::Relaxed);
        MockBackend.protect(start, size, new_flags, pt)
    }
}

impl MappingBackend for RejectingUnmapBackend {
    type Addr = VirtAddr;
    type Flags = MockFlags;
    type PageTable = MockPageTable;

    fn map(&self, start: VirtAddr, size: usize, flags: MockFlags, pt: &mut MockPageTable) -> bool {
        MockBackend.map(start, size, flags, pt)
    }

    fn preflight_unmap(&self, start: VirtAddr, size: usize, pt: &MockPageTable) -> bool {
        self.preflight_calls.fetch_add(1, Ordering::Relaxed);
        start.as_usize() != self.reject_start && MockBackend.preflight_unmap(start, size, pt)
    }

    fn unmap(&self, start: VirtAddr, size: usize, pt: &mut MockPageTable) -> bool {
        self.unmap_calls.fetch_add(1, Ordering::Relaxed);
        MockBackend.unmap(start, size, pt)
    }

    fn protect(
        &self,
        start: VirtAddr,
        size: usize,
        new_flags: MockFlags,
        pt: &mut MockPageTable,
    ) -> bool {
        MockBackend.protect(start, size, new_flags, pt)
    }
}

impl MappingBackend for FailingCommitBackend {
    type Addr = VirtAddr;
    type Flags = MockFlags;
    type PageTable = MockPageTable;

    fn map(&self, start: VirtAddr, size: usize, flags: MockFlags, pt: &mut MockPageTable) -> bool {
        MockBackend.map(start, size, flags, pt)
    }

    fn unmap(&self, _start: VirtAddr, _size: usize, _pt: &mut MockPageTable) -> bool {
        false
    }

    fn protect(
        &self,
        start: VirtAddr,
        size: usize,
        new_flags: MockFlags,
        pt: &mut MockPageTable,
    ) -> bool {
        MockBackend.protect(start, size, new_flags, pt)
    }
}

impl MappingBackend for FailingProtectCommitBackend {
    type Addr = VirtAddr;
    type Flags = MockFlags;
    type PageTable = MockPageTable;

    fn map(&self, start: VirtAddr, size: usize, flags: MockFlags, pt: &mut MockPageTable) -> bool {
        MockBackend.map(start, size, flags, pt)
    }

    fn unmap(&self, start: VirtAddr, size: usize, pt: &mut MockPageTable) -> bool {
        MockBackend.unmap(start, size, pt)
    }

    fn preflight_protect(
        &self,
        _start: VirtAddr,
        _size: usize,
        _new_flags: MockFlags,
        _pt: &MockPageTable,
    ) -> bool {
        true
    }

    fn protect(
        &self,
        _start: VirtAddr,
        _size: usize,
        _new_flags: MockFlags,
        _pt: &mut MockPageTable,
    ) -> bool {
        false
    }
}

impl MappingBackend for DeferredBackend {
    type Addr = VirtAddr;
    type Flags = MockFlags;
    type PageTable = MockPageTable;

    fn map(&self, start: VirtAddr, size: usize, flags: MockFlags, pt: &mut MockPageTable) -> bool {
        MockBackend.map(start, size, flags, pt)
    }

    fn preflight_unmap(&self, start: VirtAddr, size: usize, pt: &MockPageTable) -> bool {
        self.preflight_calls.fetch_add(1, Ordering::Relaxed);
        self.reject_start != Some(start.as_usize()) && MockBackend.preflight_unmap(start, size, pt)
    }

    fn unmap(&self, start: VirtAddr, size: usize, pt: &mut MockPageTable) -> bool {
        MockBackend.unmap(start, size, pt)
    }

    fn protect(
        &self,
        start: VirtAddr,
        size: usize,
        new_flags: MockFlags,
        pt: &mut MockPageTable,
    ) -> bool {
        MockBackend.protect(start, size, new_flags, pt)
    }
}

impl DeferredUnmapBackend for DeferredBackend {
    type Retirement = TrackedRetirement;

    fn unmap_deferred(
        &self,
        start: VirtAddr,
        size: usize,
        pt: &mut MockPageTable,
    ) -> Option<Self::Retirement> {
        self.deferred_calls.fetch_add(1, Ordering::Relaxed);
        if self.fail_commit_start == Some(start.as_usize()) {
            return None;
        }
        MockBackend
            .unmap(start, size, pt)
            .then(|| TrackedRetirement::new(self.live_retirements.clone()))
    }
}

macro_rules! assert_ok {
    ($expr:expr) => {
        assert!(($expr).is_ok())
    };
}

macro_rules! assert_err {
    ($expr:expr) => {
        assert!(($expr).is_err())
    };
    ($expr:expr, $err:ident) => {
        assert_eq!(($expr).err(), Some(MappingError::$err))
    };
}

fn dump_memory_set(set: &MockMemorySet) {
    use std::sync::Mutex;
    static DUMP_LOCK: Mutex<()> = Mutex::new(());

    let _lock = DUMP_LOCK.lock().unwrap();
    println!("Number of areas: {}", set.len());
    for area in set.iter() {
        println!("{area:?}");
    }
}

#[test]
fn test_map_unmap() {
    let mut set = MockMemorySet::new();
    let mut pt = [0; MAX_ADDR];

    // Map [0, 0x1000), [0x2000, 0x3000), [0x4000, 0x5000), ...
    for start in (0..MAX_ADDR).step_by(0x2000) {
        assert_ok!(set.map(
            tracked_area(start.into(), 0x1000, 1, MockBackend),
            &mut pt,
            false,
        ));
    }
    // Map [0x1000, 0x2000), [0x3000, 0x4000), [0x5000, 0x6000), ...
    for start in (0x1000..MAX_ADDR).step_by(0x2000) {
        assert_ok!(set.map(
            tracked_area(start.into(), 0x1000, 2, MockBackend),
            &mut pt,
            false,
        ));
    }
    dump_memory_set(&set);
    assert_eq!(set.len(), 16);
    for &e in &pt[0..MAX_ADDR] {
        assert!(e == 1 || e == 2);
    }

    // Found [0x4000, 0x5000), flags = 1.
    let area = set.find(0x4100.into()).unwrap();
    assert_eq!(area.start(), 0x4000.into());
    assert_eq!(area.end(), 0x5000.into());
    assert_eq!(area.flags(), 1);
    assert_eq!(pt[0x4200], 1);

    // The area [0x4000, 0x8000) is already mapped, map returns an error.
    assert_err!(
        set.map(
            tracked_area(0x4000.into(), 0x4000, 3, MockBackend),
            &mut pt,
            false
        ),
        AlreadyExists
    );
    // Unmap overlapped areas before adding the new mapping [0x4000, 0x8000).
    assert_ok!(set.map(
        tracked_area(0x4000.into(), 0x4000, 3, MockBackend),
        &mut pt,
        true
    ));
    dump_memory_set(&set);
    assert_eq!(set.len(), 13);

    // Found [0x4000, 0x8000), flags = 3.
    let area = set.find(0x4100.into()).unwrap();
    assert_eq!(area.start(), 0x4000.into());
    assert_eq!(area.end(), 0x8000.into());
    assert_eq!(area.flags(), 3);
    for &e in &pt[0x4000..0x8000] {
        assert_eq!(e, 3);
    }

    // Unmap areas in the middle.
    assert_ok!(set.unmap(0x4000.into(), 0x8000, &mut pt));
    assert_eq!(set.len(), 8);
    // Unmap the remaining areas, including the unmapped ranges.
    assert_ok!(set.unmap(0.into(), MAX_ADDR * 2, &mut pt));
    assert_eq!(set.len(), 0);
    for &e in &pt[0..MAX_ADDR] {
        assert_eq!(e, 0);
    }
}

#[test]
fn overlapping_iterator_includes_crossing_predecessor_and_stops_at_upper_bound() {
    let mut set = MockMemorySet::new();
    let mut pt = [0; MAX_ADDR];
    for start in [0x1000, 0x3000, 0x5000] {
        assert_ok!(set.map(
            tracked_area(start.into(), 0x1000, 1, MockBackend),
            &mut pt,
            false,
        ));
    }

    let starts: Vec<_> = set
        .iter_overlapping(va_range!(0x1800..0x4800))
        .map(|area| area.start().as_usize())
        .collect();
    assert_eq!(starts, [0x1000, 0x3000]);
    assert_eq!(set.iter_overlapping(va_range!(0x2000..0x3000)).count(), 0);
}

#[test]
fn test_unmap_split() {
    let mut set = MockMemorySet::new();
    let mut pt = [0; MAX_ADDR];

    // Map [0, 0x1000), [0x2000, 0x3000), [0x4000, 0x5000), ...
    for start in (0..MAX_ADDR).step_by(0x2000) {
        assert_ok!(set.map(
            tracked_area(start.into(), 0x1000, 1, MockBackend),
            &mut pt,
            false,
        ));
    }
    assert_eq!(set.len(), 8);

    // Unmap [0xc00, 0x2400), [0x2c00, 0x4400), [0x4c00, 0x6400), ...
    // The areas are shrinked at the left and right boundaries.
    for start in (0..MAX_ADDR).step_by(0x2000) {
        assert_ok!(set.unmap((start + 0xc00).into(), 0x1800, &mut pt));
    }
    dump_memory_set(&set);
    assert_eq!(set.len(), 8);

    for area in set.iter() {
        if area.start().as_usize() == 0 {
            assert_eq!(area.size(), 0xc00);
        } else {
            assert_eq!(area.start().align_offset_4k(), 0x400);
            assert_eq!(area.end().align_offset_4k(), 0xc00);
            assert_eq!(area.size(), 0x800);
        }
        for &e in &pt[area.start().as_usize()..area.end().as_usize()] {
            assert_eq!(e, 1);
        }
    }

    // Unmap [0x800, 0x900), [0x2800, 0x2900), [0x4800, 0x4900), ...
    // The areas are split into two areas.
    for start in (0..MAX_ADDR).step_by(0x2000) {
        assert_ok!(set.unmap((start + 0x800).into(), 0x100, &mut pt));
    }
    dump_memory_set(&set);
    assert_eq!(set.len(), 16);

    for area in set.iter() {
        let off = area.start().align_offset_4k();
        if off == 0 {
            assert_eq!(area.size(), 0x800);
        } else if off == 0x400 {
            assert_eq!(area.size(), 0x400);
        } else if off == 0x900 {
            assert_eq!(area.size(), 0x300);
        } else {
            unreachable!();
        }
        for &e in &pt[area.start().as_usize()..area.end().as_usize()] {
            assert_eq!(e, 1);
        }
    }
    let mut iter = set.iter();
    while let Some(area) = iter.next() {
        if let Some(next) = iter.next() {
            for &e in &pt[area.end().as_usize()..next.start().as_usize()] {
                assert_eq!(e, 0);
            }
        }
    }
    drop(iter);

    // Unmap all areas.
    assert_ok!(set.unmap(0.into(), MAX_ADDR, &mut pt));
    assert_eq!(set.len(), 0);
    for &e in &pt[0..MAX_ADDR] {
        assert_eq!(e, 0);
    }
}

#[test]
fn unmap_preflights_every_backend_before_mutating_any_area_or_pte() {
    let preflight_calls = Arc::new(AtomicUsize::new(0));
    let unmap_calls = Arc::new(AtomicUsize::new(0));
    let backend = RejectingUnmapBackend {
        preflight_calls: preflight_calls.clone(),
        unmap_calls: unmap_calls.clone(),
        reject_start: 0x3000,
    };
    let mut set = MemorySet::new();
    let mut pt = [0; MAX_ADDR];
    assert_ok!(set.map(
        tracked_area(0x1000.into(), 0x1000, 1, backend.clone()),
        &mut pt,
        false,
    ));
    assert_ok!(set.map(
        tracked_area(0x3000.into(), 0x1000, 2, backend),
        &mut pt,
        false,
    ));
    let pt_before = pt;

    assert_err!(set.unmap(0x1000.into(), 0x3000, &mut pt), BadState);

    assert_eq!(preflight_calls.load(Ordering::Relaxed), 2);
    assert_eq!(unmap_calls.load(Ordering::Relaxed), 0);
    assert_eq!(set.len(), 2);
    assert_eq!(set.find(0x1000.into()).unwrap().flags(), 1);
    assert_eq!(set.find(0x3000.into()).unwrap().flags(), 2);
    assert_eq!(pt, pt_before);
}

#[test]
fn bounded_map_rejects_before_mapping_or_growing_the_area_tree() {
    let mut set = MockMemorySet::new();
    let mut pt = [0; MAX_ADDR];
    assert_ok!(set.map_with_limit(
        tracked_area(0x1000.into(), 0x1000, 1, MockBackend),
        &mut pt,
        false,
        1,
    ));
    let pt_before = pt;

    assert_err!(
        set.map_with_limit(
            tracked_area(0x3000.into(), 0x1000, 2, MockBackend),
            &mut pt,
            false,
            1,
        ),
        NoMemory
    );

    assert_eq!(set.len(), 1);
    assert!(set.find(0x3000.into()).is_none());
    assert_eq!(pt, pt_before);
}

#[test]
fn bounded_unmap_rejects_middle_split_before_area_or_pte_mutation() {
    let mut set = MockMemorySet::new();
    let mut pt = [0; MAX_ADDR];
    assert_ok!(set.map(
        tracked_area(0x1000.into(), 0x3000, 1, MockBackend),
        &mut pt,
        false,
    ));
    let pt_before = pt;

    assert_err!(
        set.unmap_with_limit(0x2000.into(), 0x1000, &mut pt, 1),
        NoMemory
    );

    assert_eq!(set.len(), 1);
    let area = set.find(0x1000.into()).unwrap();
    assert_eq!(area.start(), 0x1000.into());
    assert_eq!(area.end(), 0x4000.into());
    assert_eq!(pt, pt_before);

    assert_ok!(set.unmap_with_limit(0x2000.into(), 0x1000, &mut pt, 2,));
    assert_eq!(set.len(), 2);
}

#[test]
fn bounded_local_unmap_preserves_a_large_vma_prefix_and_split_peak() {
    let mut set = MockMemorySet::new();
    let mut pt = [0; MAX_ADDR];

    // Keep a large, unrelated prefix below the locally modified VMA. The
    // fragment admission path should only inspect the target overlap, while
    // the total prefix still contributes to the conservative peak count.
    for start in (0x100..0x8100).step_by(0x100) {
        assert_ok!(set.map(
            tracked_area(start.into(), 0x80, 1, MockBackend),
            &mut pt,
            false,
        ));
    }
    assert_ok!(set.map(
        tracked_area(0xe000.into(), 0x1000, 2, MockBackend),
        &mut pt,
        false,
    ));
    assert_ok!(set.map(
        tracked_area(0xf800.into(), 0x100, 3, MockBackend),
        &mut pt,
        false,
    ));
    let original_len = set.len();
    assert_eq!(original_len, 130);
    let pt_before = pt;

    assert_err!(
        set.unmap_with_limit(0xe400.into(), 0x400, &mut pt, original_len),
        NoMemory
    );
    assert_eq!(set.len(), original_len);
    assert_eq!(pt, pt_before);

    assert_ok!(set.unmap_with_limit(0xe400.into(), 0x400, &mut pt, original_len + 1,));
    assert_eq!(set.len(), original_len + 1);
    assert_eq!(set.find(0x100.into()).unwrap().flags(), 1);
    assert_eq!(set.find(0xe100.into()).unwrap().end(), 0xe400.into());
    assert_eq!(set.find(0xe900.into()).unwrap().start(), 0xe800.into());
    assert_eq!(set.find(0xf800.into()).unwrap().flags(), 3);
    assert!(pt[0xe400..0xe800].iter().all(|&flags| flags == 0));
}

#[test]
fn clear_preflights_every_backend_before_mutating_any_pte() {
    let preflight_calls = Arc::new(AtomicUsize::new(0));
    let unmap_calls = Arc::new(AtomicUsize::new(0));
    let backend = RejectingUnmapBackend {
        preflight_calls: preflight_calls.clone(),
        unmap_calls: unmap_calls.clone(),
        reject_start: 0x3000,
    };
    let mut set = MemorySet::new();
    let mut pt = [0; MAX_ADDR];
    assert_ok!(set.map(
        tracked_area(0x1000.into(), 0x1000, 1, backend.clone()),
        &mut pt,
        false,
    ));
    assert_ok!(set.map(
        tracked_area(0x3000.into(), 0x1000, 2, backend),
        &mut pt,
        false,
    ));
    let pt_before = pt;

    assert_err!(set.clear(&mut pt), BadState);

    assert_eq!(preflight_calls.load(Ordering::Relaxed), 2);
    assert_eq!(unmap_calls.load(Ordering::Relaxed), 0);
    assert_eq!(set.len(), 2);
    assert_eq!(pt, pt_before);
}

#[test]
fn deferred_unmap_holds_backend_retirement_and_complete_area_until_release() {
    let signals = DeferredSignals::default();
    let owner = Arc::new(());
    let mut set = MemorySet::new();
    let mut pt = [0; MAX_ADDR];
    assert_ok!(set.map(
        tracked_area(
            0x1000.into(),
            0x1000,
            1,
            signals.backend(owner.clone(), None),
        ),
        &mut pt,
        false,
    ));
    assert_eq!(Arc::strong_count(&owner), 2);

    let retirement = set.unmap_deferred(0x1000.into(), 0x1000, &mut pt).unwrap();

    assert!(set.is_empty());
    assert_eq!(retirement.backend_retirements().len(), 1);
    assert_eq!(retirement.retired_areas().len(), 1);
    assert_eq!(signals.live_retirements.load(Ordering::Relaxed), 1);
    assert_eq!(Arc::strong_count(&owner), 2);

    // The caller performs its translation fence before this explicit release.
    retirement.release();
    assert_eq!(signals.live_retirements.load(Ordering::Relaxed), 0);
    assert_eq!(Arc::strong_count(&owner), 1);
}

#[test]
fn deferred_partial_unmap_retains_its_backend_token() {
    let signals = DeferredSignals::default();
    let owner = Arc::new(());
    let mut set = MemorySet::new();
    let mut pt = [0; MAX_ADDR];
    assert_ok!(set.map(
        tracked_area(
            0x1000.into(),
            0x3000,
            1,
            signals.backend(owner.clone(), None),
        ),
        &mut pt,
        false,
    ));

    let retirement = set.unmap_deferred(0x2000.into(), 0x1000, &mut pt).unwrap();

    assert_eq!(set.len(), 2);
    assert_eq!(retirement.backend_retirements().len(), 1);
    assert!(retirement.retired_areas().is_empty());
    assert_eq!(signals.live_retirements.load(Ordering::Relaxed), 1);
    assert!(pt[0x2000..0x3000].iter().all(|&flags| flags == 0));

    retirement.release();
    assert_eq!(signals.live_retirements.load(Ordering::Relaxed), 0);
    assert_eq!(Arc::strong_count(&owner), 3);
}

#[test]
fn deferred_clear_retains_every_complete_area_owner() {
    let signals = DeferredSignals::default();
    let owner = Arc::new(());
    let backend = signals.backend(owner.clone(), None);
    let mut set = MemorySet::new();
    let mut pt = [0; MAX_ADDR];
    assert_ok!(set.map(
        tracked_area(0x1000.into(), 0x1000, 1, backend.clone()),
        &mut pt,
        false,
    ));
    assert_ok!(set.map(
        tracked_area(0x3000.into(), 0x1000, 2, backend),
        &mut pt,
        false,
    ));
    assert_eq!(Arc::strong_count(&owner), 3);

    let retirement = set.clear_deferred(&mut pt).unwrap();

    assert!(set.is_empty());
    assert_eq!(retirement.backend_retirements().len(), 2);
    assert_eq!(retirement.retired_areas().len(), 2);
    assert_eq!(signals.live_retirements.load(Ordering::Relaxed), 2);
    assert_eq!(Arc::strong_count(&owner), 3);

    retirement.release();
    assert_eq!(signals.live_retirements.load(Ordering::Relaxed), 0);
    assert_eq!(Arc::strong_count(&owner), 1);
}

#[test]
fn deferred_clear_preflight_failure_has_no_token_or_side_effect() {
    let signals = DeferredSignals::default();
    let owner = Arc::new(());
    let backend = signals.backend(owner.clone(), Some(0x3000));
    let mut set = MemorySet::new();
    let mut pt = [0; MAX_ADDR];
    assert_ok!(set.map(
        tracked_area(0x1000.into(), 0x1000, 1, backend.clone()),
        &mut pt,
        false,
    ));
    assert_ok!(set.map(
        tracked_area(0x3000.into(), 0x1000, 2, backend),
        &mut pt,
        false,
    ));
    let pt_before = pt;

    assert_err!(set.clear_deferred(&mut pt), BadState);

    assert_eq!(signals.preflight_calls.load(Ordering::Relaxed), 2);
    assert_eq!(signals.deferred_calls.load(Ordering::Relaxed), 0);
    assert_eq!(signals.live_retirements.load(Ordering::Relaxed), 0);
    assert_eq!(set.len(), 2);
    assert_eq!(Arc::strong_count(&owner), 3);
    assert_eq!(pt, pt_before);
}

#[test]
fn deferred_commit_invariant_failure_leaks_prior_retirement_and_area_owner() {
    let signals = DeferredSignals::default();
    let owner = Arc::new(());
    let mut set = MemorySet::new();
    let mut pt = [0; MAX_ADDR];
    assert_ok!(set.map(
        tracked_area(
            0x1000.into(),
            0x1000,
            1,
            signals.backend(owner.clone(), None),
        ),
        &mut pt,
        false,
    ));
    assert_ok!(set.map(
        tracked_area(
            0x3000.into(),
            0x1000,
            2,
            signals.backend(owner.clone(), None).fail_commit_at(0x3000),
        ),
        &mut pt,
        false,
    ));

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = set.unmap_deferred(0x1000.into(), 0x3000, &mut pt);
    }));

    assert!(outcome.is_err());
    assert_eq!(signals.deferred_calls.load(Ordering::Relaxed), 2);
    assert_eq!(signals.live_retirements.load(Ordering::Relaxed), 1);
    assert_eq!(set.len(), 1);
    assert_eq!(Arc::strong_count(&owner), 3);
    assert!(pt[0x1000..0x2000].iter().all(|&flags| flags == 0));
    assert!(pt[0x3000..0x4000].iter().all(|&flags| flags == 2));
}

#[test]
fn backend_failure_after_successful_preflight_is_fail_stop() {
    let mut set = MemorySet::new();
    let mut pt = [0; MAX_ADDR];
    assert_ok!(set.map(
        tracked_area(0x1000.into(), 0x1000, 1, FailingCommitBackend),
        &mut pt,
        false,
    ));

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = set.unmap(0x1000.into(), 0x1000, &mut pt);
    }));

    assert!(outcome.is_err());
}

#[test]
fn test_protect() {
    let mut set = MockMemorySet::new();
    let mut pt = [0; MAX_ADDR];
    let update_flags = |new_flags: MockFlags| {
        move |old_flags: MockFlags| -> Option<MockFlags> {
            if (old_flags & 0x7) == (new_flags & 0x7) {
                return None;
            }
            let flags = (new_flags & 0x7) | (old_flags & !0x7);
            Some(flags)
        }
    };

    // Map [0, 0x1000), [0x2000, 0x3000), [0x4000, 0x5000), ...
    for start in (0..MAX_ADDR).step_by(0x2000) {
        assert_ok!(set.map(
            tracked_area(start.into(), 0x1000, 0x7, MockBackend),
            &mut pt,
            false,
        ));
    }
    assert_eq!(set.len(), 8);

    // Protect [0xc00, 0x2400), [0x2c00, 0x4400), [0x4c00, 0x6400), ...
    // The areas are split into two areas.
    for start in (0..MAX_ADDR).step_by(0x2000) {
        assert_ok!(set.protect((start + 0xc00).into(), 0x1800, update_flags(0x1), &mut pt));
    }
    dump_memory_set(&set);
    assert_eq!(set.len(), 23);

    for area in set.iter() {
        let off = area.start().align_offset_4k();
        if area.start().as_usize() == 0 {
            assert_eq!(area.size(), 0xc00);
            assert_eq!(area.flags(), 0x7);
        } else if off == 0 {
            assert_eq!(area.size(), 0x400);
            assert_eq!(area.flags(), 0x1);
        } else if off == 0x400 {
            assert_eq!(area.size(), 0x800);
            assert_eq!(area.flags(), 0x7);
        } else if off == 0xc00 {
            assert_eq!(area.size(), 0x400);
            assert_eq!(area.flags(), 0x1);
        }
    }

    // Protect [0x800, 0x900), [0x2800, 0x2900), [0x4800, 0x4900), ...
    // The areas are split into three areas.
    for start in (0..MAX_ADDR).step_by(0x2000) {
        assert_ok!(set.protect((start + 0x800).into(), 0x100, update_flags(0x13), &mut pt));
    }
    dump_memory_set(&set);
    assert_eq!(set.len(), 39);

    for area in set.iter() {
        let off = area.start().align_offset_4k();
        if area.start().as_usize() == 0 {
            assert_eq!(area.size(), 0x800);
            assert_eq!(area.flags(), 0x7);
        } else if off == 0 {
            assert_eq!(area.size(), 0x400);
            assert_eq!(area.flags(), 0x1);
        } else if off == 0x400 {
            assert_eq!(area.size(), 0x400);
            assert_eq!(area.flags(), 0x7);
        } else if off == 0x800 {
            assert_eq!(area.size(), 0x100);
            assert_eq!(area.flags(), 0x3);
        } else if off == 0x900 {
            assert_eq!(area.size(), 0x300);
            assert_eq!(area.flags(), 0x7);
        } else if off == 0xc00 {
            assert_eq!(area.size(), 0x400);
            assert_eq!(area.flags(), 0x1);
        }
    }

    // Test skip [0x880, 0x900), [0x2880, 0x2900), [0x4880, 0x4900), ...
    for start in (0..MAX_ADDR).step_by(0x2000) {
        assert_ok!(set.protect((start + 0x880).into(), 0x80, update_flags(0x3), &mut pt));
    }
    assert_eq!(set.len(), 39);

    // Unmap all areas.
    assert_ok!(set.unmap(0.into(), MAX_ADDR, &mut pt));
    assert_eq!(set.len(), 0);
    for &e in &pt[0..MAX_ADDR] {
        assert_eq!(e, 0);
    }
}

#[test]
fn protect_preflights_every_backend_before_splitting_or_mutating_pte() {
    let preflight_calls = Arc::new(AtomicUsize::new(0));
    let protect_calls = Arc::new(AtomicUsize::new(0));
    let backend = FailingProtectBackend {
        preflight_calls: preflight_calls.clone(),
        protect_calls: protect_calls.clone(),
        reject_start: 0x3000,
    };
    let mut set = MemorySet::new();
    let mut pt = [0; MAX_ADDR];

    assert_ok!(set.map(
        tracked_area(0x1000.into(), 0x1000, 1, backend.clone()),
        &mut pt,
        false,
    ));
    assert_ok!(set.map(
        tracked_area(0x3000.into(), 0x1000, 2, backend),
        &mut pt,
        false,
    ));

    assert_err!(
        set.protect(0x1800.into(), 0x2000, |_| Some(7), &mut pt),
        BadState
    );

    let areas: Vec<_> = set
        .iter()
        .map(|area| (area.start(), area.end(), area.flags()))
        .collect();
    assert_eq!(
        areas,
        vec![
            (VirtAddr::from(0x1000), VirtAddr::from(0x2000), 1),
            (VirtAddr::from(0x3000), VirtAddr::from(0x4000), 2),
        ]
    );
    assert!(pt[0x1000..0x2000].iter().all(|&flags| flags == 1));
    assert!(pt[0x3000..0x4000].iter().all(|&flags| flags == 2));
    assert_eq!(preflight_calls.load(Ordering::Relaxed), 2);
    assert_eq!(protect_calls.load(Ordering::Relaxed), 0);
}

#[test]
fn bounded_protect_rejects_all_required_splits_before_pte_mutation() {
    let mut set = MockMemorySet::new();
    let mut pt = [0; MAX_ADDR];
    assert_ok!(set.map(
        tracked_area(0x1000.into(), 0x3000, 1, MockBackend),
        &mut pt,
        false,
    ));
    let pt_before = pt;

    assert_err!(
        set.protect_with_limit(0x2000.into(), 0x1000, |_| Some(3), &mut pt, 2),
        NoMemory
    );

    assert_eq!(set.len(), 1);
    let area = set.find(0x1000.into()).unwrap();
    assert_eq!(area.start(), 0x1000.into());
    assert_eq!(area.end(), 0x4000.into());
    assert_eq!(area.flags(), 1);
    assert_eq!(pt, pt_before);

    assert_ok!(set.protect_with_limit(0x2000.into(), 0x1000, |_| Some(3), &mut pt, 3,));
    assert_eq!(set.len(), 3);
}

#[test]
fn protect_backend_failure_after_successful_preflight_is_fail_stop() {
    let mut set = MemorySet::new();
    let mut pt = [0; MAX_ADDR];
    assert_ok!(set.map(
        tracked_area(0x1000.into(), 0x1000, 1, FailingProtectCommitBackend),
        &mut pt,
        false,
    ));

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = set.protect(0x1800.into(), 0x400, |_| Some(7), &mut pt);
    }));

    assert!(outcome.is_err());
}

#[test]
fn tracked_lineage_cannot_alias_the_legacy_sentinel() {
    assert!(MappingLineage::new(0).is_none());
    assert!(MappingLineage::new(MappingLineage::UNTRACKED.get()).is_none());
    assert_eq!(MappingLineage::UNTRACKED.get(), 1);
    assert_eq!(lineage(2).get(), 2);
}

#[test]
fn legacy_areas_merge_but_distinct_tracked_lineages_do_not() {
    let mut legacy = MergeMemorySet::new();
    let mut legacy_pt = [0; MAX_ADDR];
    assert_ok!(legacy.map(
        MemoryArea::new(0x1000.into(), 0x1000, 0x3, MergeBackend),
        &mut legacy_pt,
        false,
    ));
    assert_ok!(legacy.map(
        MemoryArea::new(0x2000.into(), 0x1000, 0x3, MergeBackend),
        &mut legacy_pt,
        false,
    ));
    assert_eq!(legacy.len(), 1);
    assert_eq!(
        legacy.find(0x1000.into()).unwrap().lineage(),
        MappingLineage::UNTRACKED
    );

    let mut tracked = MergeMemorySet::new();
    let mut tracked_pt = [0; MAX_ADDR];
    assert_ok!(tracked.map(
        MemoryArea::new_with_lineage(0x1000.into(), 0x1000, 0x3, MergeBackend, lineage(2),),
        &mut tracked_pt,
        false,
    ));
    assert_ok!(tracked.map(
        MemoryArea::new_with_lineage(0x2000.into(), 0x1000, 0x3, MergeBackend, lineage(3),),
        &mut tracked_pt,
        false,
    ));
    assert_eq!(tracked.len(), 2);
}

#[test]
fn test_map_merge_adjacent() {
    let mut set = MergeMemorySet::new();
    let mut pt = [0; MAX_ADDR];

    assert_ok!(set.map(
        MemoryArea::new_with_lineage(0x1000.into(), 0x1000, 0x3, MergeBackend, lineage(0x55),),
        &mut pt,
        false,
    ));
    assert_ok!(set.map(
        MemoryArea::new_with_lineage(0x2000.into(), 0x1000, 0x3, MergeBackend, lineage(0x55),),
        &mut pt,
        false,
    ));
    assert_eq!(set.len(), 1);
    let merged = set.find(0x1800.into()).unwrap();
    assert_eq!(merged.start(), 0x1000.into());
    assert_eq!(merged.end(), 0x3000.into());

    assert_ok!(set.unmap(0x1800.into(), 0x800, &mut pt));
    assert_eq!(set.len(), 2);
    assert!(set.iter().all(|area| area.lineage() == lineage(0x55)));

    assert_ok!(set.map(
        MemoryArea::new_with_lineage(0x1800.into(), 0x800, 0x3, MergeBackend, lineage(0x55),),
        &mut pt,
        false,
    ));
    assert_eq!(set.len(), 1);
    let merged = set.find(0x1c00.into()).unwrap();
    assert_eq!(merged.start(), 0x1000.into());
    assert_eq!(merged.end(), 0x3000.into());
}

#[test]
fn test_find_free_area() {
    let mut set = MockMemorySet::new();
    let mut pt = [0; MAX_ADDR];

    // Map [0, 0x1000), [0x2000, 0x3000), ..., [0xe000, 0xf000)
    for start in (0..MAX_ADDR).step_by(0x2000) {
        assert_ok!(set.map(
            tracked_area(start.into(), 0x1000, 1, MockBackend),
            &mut pt,
            false,
        ));
    }

    let addr = set.find_free_area(0.into(), 0x1000, va_range!(0..MAX_ADDR), 1);
    assert_eq!(addr, Some(0x1000.into()));

    let addr = set.find_free_area(0x800.into(), 0x800, va_range!(0..MAX_ADDR), 0x800);
    assert_eq!(addr, Some(0x1000.into()));

    let addr = set.find_free_area(0x1800.into(), 0x800, va_range!(0..MAX_ADDR), 0x800);
    assert_eq!(addr, Some(0x1800.into()));

    let addr = set.find_free_area(0x1800.into(), 0x1000, va_range!(0..MAX_ADDR), 0x1000);
    assert_eq!(addr, Some(0x3000.into()));

    let addr = set.find_free_area(0x2000.into(), 0x1000, va_range!(0..MAX_ADDR), 0x1000);
    assert_eq!(addr, Some(0x3000.into()));

    let addr = set.find_free_area(0xf000.into(), 0x1000, va_range!(0..MAX_ADDR), 0x1000);
    assert_eq!(addr, Some(0xf000.into()));

    let addr = set.find_free_area(0xf001.into(), 0x1000, va_range!(0..MAX_ADDR), 0x1000);
    assert_eq!(addr, None);
}

#[test]
fn test_find_append_area() {
    let mut set = MockMemorySet::new();
    let mut pt = [0; MAX_ADDR];

    assert_eq!(
        set.find_append_area(0x1000, va_range!(0x4000..0x8000), 0x1000),
        Some(0x4000.into())
    );

    for start in (0..MAX_ADDR).step_by(0x2000) {
        assert_ok!(set.map(
            tracked_area(start.into(), 0x1000, 1, MockBackend),
            &mut pt,
            false,
        ));
    }

    assert_eq!(
        set.find_append_area(0x1000, va_range!(0..MAX_ADDR), 0x1000),
        Some(0xf000.into())
    );
    assert_eq!(
        set.find_append_area(0x1000, va_range!(0..0xf000), 0x1000),
        None
    );
}
