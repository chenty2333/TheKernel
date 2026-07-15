use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use memory_addr::{MemoryAddr, VirtAddr, va_range};

use crate::{MappingBackend, MappingError, MemoryArea, MemorySet};

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

type MockMemorySet = MemorySet<MockBackend>;
type MergeMemorySet = MemorySet<MergeBackend>;

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
        println!("{:?}", area);
    }
}

#[test]
fn test_map_unmap() {
    let mut set = MockMemorySet::new();
    let mut pt = [0; MAX_ADDR];

    // Map [0, 0x1000), [0x2000, 0x3000), [0x4000, 0x5000), ...
    for start in (0..MAX_ADDR).step_by(0x2000) {
        assert_ok!(set.map(
            MemoryArea::new(start.into(), 0x1000, 1, MockBackend),
            &mut pt,
            false,
        ));
    }
    // Map [0x1000, 0x2000), [0x3000, 0x4000), [0x5000, 0x6000), ...
    for start in (0x1000..MAX_ADDR).step_by(0x2000) {
        assert_ok!(set.map(
            MemoryArea::new(start.into(), 0x1000, 2, MockBackend),
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
            MemoryArea::new(0x4000.into(), 0x4000, 3, MockBackend),
            &mut pt,
            false
        ),
        AlreadyExists
    );
    // Unmap overlapped areas before adding the new mapping [0x4000, 0x8000).
    assert_ok!(set.map(
        MemoryArea::new(0x4000.into(), 0x4000, 3, MockBackend),
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
fn test_unmap_split() {
    let mut set = MockMemorySet::new();
    let mut pt = [0; MAX_ADDR];

    // Map [0, 0x1000), [0x2000, 0x3000), [0x4000, 0x5000), ...
    for start in (0..MAX_ADDR).step_by(0x2000) {
        assert_ok!(set.map(
            MemoryArea::new(start.into(), 0x1000, 1, MockBackend),
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
        MemoryArea::new(0x1000.into(), 0x1000, 1, backend.clone()),
        &mut pt,
        false,
    ));
    assert_ok!(set.map(
        MemoryArea::new(0x3000.into(), 0x1000, 2, backend),
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
        MemoryArea::new(0x1000.into(), 0x1000, 1, backend.clone()),
        &mut pt,
        false,
    ));
    assert_ok!(set.map(
        MemoryArea::new(0x3000.into(), 0x1000, 2, backend),
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
fn backend_failure_after_successful_preflight_is_fail_stop() {
    let mut set = MemorySet::new();
    let mut pt = [0; MAX_ADDR];
    assert_ok!(set.map(
        MemoryArea::new(0x1000.into(), 0x1000, 1, FailingCommitBackend),
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
            MemoryArea::new(start.into(), 0x1000, 0x7, MockBackend),
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
        MemoryArea::new(0x1000.into(), 0x1000, 1, backend.clone()),
        &mut pt,
        false,
    ));
    assert_ok!(set.map(
        MemoryArea::new(0x3000.into(), 0x1000, 2, backend),
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
fn protect_backend_failure_after_successful_preflight_is_fail_stop() {
    let mut set = MemorySet::new();
    let mut pt = [0; MAX_ADDR];
    assert_ok!(set.map(
        MemoryArea::new(0x1000.into(), 0x1000, 1, FailingProtectCommitBackend),
        &mut pt,
        false,
    ));

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = set.protect(0x1800.into(), 0x400, |_| Some(7), &mut pt);
    }));

    assert!(outcome.is_err());
}

#[test]
fn test_map_merge_adjacent() {
    let mut set = MergeMemorySet::new();
    let mut pt = [0; MAX_ADDR];

    assert_ok!(set.map(
        MemoryArea::new(0x1000.into(), 0x1000, 0x3, MergeBackend),
        &mut pt,
        false,
    ));
    assert_ok!(set.map(
        MemoryArea::new(0x2000.into(), 0x1000, 0x3, MergeBackend),
        &mut pt,
        false,
    ));
    assert_eq!(set.len(), 1);
    let merged = set.find(0x1800.into()).unwrap();
    assert_eq!(merged.start(), 0x1000.into());
    assert_eq!(merged.end(), 0x3000.into());

    assert_ok!(set.unmap(0x1800.into(), 0x800, &mut pt));
    assert_eq!(set.len(), 2);

    assert_ok!(set.map(
        MemoryArea::new(0x1800.into(), 0x800, 0x3, MergeBackend),
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
            MemoryArea::new(start.into(), 0x1000, 1, MockBackend),
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
            MemoryArea::new(start.into(), 0x1000, 1, MockBackend),
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
