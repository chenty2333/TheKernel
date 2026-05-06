use alloc::vec::Vec;
use core::cell::{Cell, RefCell};

use axhal::paging::MappingFlags;
use memory_addr::{VirtAddr, VirtAddrRange};
use memory_set::MemorySet;

use super::Backend;

#[derive(Clone)]
pub struct VmaSnapshot {
    start: VirtAddr,
    end: VirtAddr,
    flags: MappingFlags,
    backend: Backend,
}

impl VmaSnapshot {
    pub const fn start(&self) -> VirtAddr {
        self.start
    }

    pub const fn end(&self) -> VirtAddr {
        self.end
    }

    pub const fn flags(&self) -> MappingFlags {
        self.flags
    }

    pub const fn backend(&self) -> &Backend {
        &self.backend
    }
}

#[derive(Clone)]
struct VmaCacheSlot {
    generation: usize,
    range: VirtAddrRange,
    snapshot: VmaSnapshot,
}

impl VmaCacheSlot {
    fn matches(&self, generation: usize, vaddr: VirtAddr) -> bool {
        self.generation == generation && self.range.contains(vaddr)
    }
}

pub struct VmaRangeCache<const N: usize> {
    generation: Cell<usize>,
    clock: Cell<usize>,
    slots: RefCell<Vec<VmaCacheSlot>>,
}

impl<const N: usize> VmaRangeCache<N> {
    pub const fn new() -> Self {
        Self {
            generation: Cell::new(1),
            clock: Cell::new(0),
            slots: RefCell::new(Vec::new()),
        }
    }

    pub fn invalidate(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        self.clock.set(0);
        self.slots.borrow_mut().clear();
    }

    pub fn find_snapshot(&self, areas: &MemorySet<Backend>, vaddr: VirtAddr) -> Option<VmaSnapshot> {
        let generation = self.generation.get();
        {
            let slots = self.slots.borrow();
            if let Some(slot) = slots.iter().find(|slot| slot.matches(generation, vaddr)) {
                return Some(slot.snapshot.clone());
            }
        }

        let area = areas.find(vaddr)?;
        let snapshot = VmaSnapshot {
            start: area.start(),
            end: area.end(),
            flags: area.flags(),
            backend: area.backend().clone(),
        };
        self.insert(generation, snapshot.clone());
        Some(snapshot)
    }

    fn insert(&self, generation: usize, snapshot: VmaSnapshot) {
        if N == 0 {
            return;
        }

        let mut slots = self.slots.borrow_mut();
        let slot = VmaCacheSlot {
            generation,
            range: VirtAddrRange::new(snapshot.start, snapshot.end),
            snapshot,
        };
        if slots.len() < N {
            slots.push(slot);
        } else {
            let idx = self.clock.get() % N;
            slots[idx] = slot;
        }
        self.clock.set(self.clock.get().wrapping_add(1));
    }
}
