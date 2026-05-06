use core::cell::Cell;

use memory_addr::VirtAddr;
use memory_set::{MemoryArea, MemorySet};

use super::Backend;

#[derive(Clone, Copy)]
struct VmaCacheSlot {
    generation: usize,
    start: usize,
    end: usize,
    ptr: usize,
}

impl VmaCacheSlot {
    const EMPTY: Self = Self {
        generation: 0,
        start: 0,
        end: 0,
        ptr: 0,
    };

    fn contains(self, generation: usize, addr: usize) -> bool {
        self.ptr != 0 && self.generation == generation && self.start <= addr && addr < self.end
    }
}

pub struct VmaRangeCache<const N: usize> {
    generation: Cell<usize>,
    clock: Cell<usize>,
    slots: Cell<[VmaCacheSlot; N]>,
}

impl<const N: usize> VmaRangeCache<N> {
    pub const fn new() -> Self {
        Self {
            generation: Cell::new(1),
            clock: Cell::new(0),
            slots: Cell::new([VmaCacheSlot::EMPTY; N]),
        }
    }

    pub fn invalidate(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        self.clock.set(0);
        self.slots.set([VmaCacheSlot::EMPTY; N]);
    }

    pub fn find<'a>(
        &self,
        areas: &'a MemorySet<Backend>,
        vaddr: VirtAddr,
    ) -> Option<&'a MemoryArea<Backend>> {
        let generation = self.generation.get();
        let addr = vaddr.as_usize();
        for slot in self.slots.get() {
            if slot.contains(generation, addr) {
                // SAFETY: cached pointers are inserted from `areas` while the
                // address space is locked. Every AddrSpace operation that can
                // mutate the MemorySet invalidates this cache before mutating,
                // so a matching generation means the BTreeMap nodes have not
                // been inserted, removed, split, merged, or rebalanced since
                // this pointer was captured.
                let area = unsafe { &*(slot.ptr as *const MemoryArea<Backend>) };
                debug_assert_eq!(area.start().as_usize(), slot.start);
                debug_assert_eq!(area.end().as_usize(), slot.end);
                debug_assert!(area.va_range().contains(vaddr));
                return Some(area);
            }
        }

        let area = areas.find(vaddr)?;
        self.insert(generation, area);
        Some(area)
    }

    fn insert(&self, generation: usize, area: &MemoryArea<Backend>) {
        if N == 0 {
            return;
        }

        let mut slots = self.slots.get();
        let idx = self.clock.get() % N;
        slots[idx] = VmaCacheSlot {
            generation,
            start: area.start().as_usize(),
            end: area.end().as_usize(),
            ptr: area as *const MemoryArea<Backend> as usize,
        };
        self.slots.set(slots);
        self.clock.set(self.clock.get().wrapping_add(1));
    }
}
