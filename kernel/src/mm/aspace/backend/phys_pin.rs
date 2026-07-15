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
}

static PINNED_FRAMES: SpinNoIrq<Option<PinnedFrameTable>> = SpinNoIrq::new(None);

const MAX_PINNED_FRAMES: usize = super::super::USER_IO_PIN_MAX_PAGES as usize;

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
        })
    }

    fn bucket_index(&self, paddr: PhysAddr) -> usize {
        let mut page = paddr.as_usize() >> 12;
        page ^= page >> 16;
        page = page.wrapping_mul(0x9e37_79b1);
        page ^= page >> 13;
        page % self.buckets.len()
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

fn ensure_pin_table_capacity() -> AxResult<()> {
    if PINNED_FRAMES.lock().is_some() {
        return Ok(());
    }

    // Allocate outside the IRQ-disabled table lock. Competing first users may
    // prepare redundant storage, but only one installs it and no live entry is
    // ever moved through a fallible allocation path.
    let prepared = PinnedFrameTable::try_new(MAX_PINNED_FRAMES)?;
    let mut table = PINNED_FRAMES.lock();
    if table.is_none() {
        *table = Some(prepared);
    }
    Ok(())
}

pub(crate) struct PhysicalFramePin {
    paddr: PhysAddr,
}

impl Drop for PhysicalFramePin {
    fn drop(&mut self) {
        let pending_free = {
            let mut table = PINNED_FRAMES.lock();
            let Some(table) = table.as_mut() else {
                warn!("PhysicalFramePin::drop: pin table is uninitialized");
                return;
            };
            let Ok(pending_free) = table.unpin(self.paddr) else {
                warn!(
                    "PhysicalFramePin::drop: missing pinned frame entry for {:?}",
                    self.paddr
                );
                return;
            };
            pending_free
        };

        if let Some(page_size) = pending_free {
            dealloc_frame_now(self.paddr, page_size);
        }
    }
}

pub(crate) fn pin_frame(paddr: PhysAddr) -> AxResult<PhysicalFramePin> {
    ensure_pin_table_capacity()?;
    let mut table = PINNED_FRAMES.lock();
    table
        .as_mut()
        .expect("initialized physical pin table")
        .pin_preallocated(paddr)?;
    Ok(PhysicalFramePin { paddr })
}

pub(crate) fn defer_frame_dealloc_if_pinned(paddr: PhysAddr, page_size: PageSize) -> bool {
    PINNED_FRAMES
        .lock()
        .as_mut()
        .is_some_and(|table| table.defer_deallocation(paddr, page_size))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table_with_capacity(capacity: usize) -> PinnedFrameTable {
        PinnedFrameTable::try_new(capacity).unwrap()
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
