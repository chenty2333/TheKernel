//! Non-mutating observations of the mm-owned software swap PTE registry.
use super::*;

fn swapped_bytes<T>(entries: &BTreeMap<VirtAddr, T>, start: VirtAddr, size: usize) -> usize {
    if size == 0 {
        return 0;
    }
    entries.range(start..start + size).count() * PAGE_SIZE_4K
}

impl AddrSpace {
    /// Per-mm swap occupancy. CLONE_VM shares this registry; fork copies its
    /// PTE owners without multiplying the swap area's unique occupied slots.
    pub(crate) fn total_swapped_bytes(&self) -> usize {
        self.swapped.len() * PAGE_SIZE_4K
    }

    /// VMA ranges are page-aligned. Count owned software swap leaves rather
    /// than interpreting hardware table holes as swapped memory.
    pub(crate) fn swapped_bytes_in_range(&self, start: VirtAddr, size: usize) -> usize {
        swapped_bytes(&self.swapped, start, size)
    }

    /// Number of address-space owners of one resident leaf, for PSS.
    pub(crate) fn mapped_frame_owner_count(&self, paddr: PhysAddr, size: PageSize) -> usize {
        super::backend::cow_frame_owner_count(paddr, size)
    }

    /// Per-VMA policy facts that /proc/<pid>/smaps reports. The `[start,
    /// start + size)` range is one VMA; each set is consulted by overlap.
    pub(crate) fn proc_vma_observations(&self, start: VirtAddr, size: usize) -> ProcVmaObservations {
        let end = start + size;
        let overlaps_btree = |ranges: &BTreeMap<VirtAddr, VirtAddr>| {
            ranges
                .range(..end)
                .any(|(&range_start, &range_end)| range_end > start && range_start < end)
        };
        ProcVmaObservations {
            growsdown: self.growdown_starts.contains(&start),
            locked: size != 0 && self.locked_bytes_in_range(start, size) == size,
            dontdump: self
                .dontdump_ranges
                .iter()
                .any(|&(range_start, range_end)| range_end > start && range_start < end),
            dontfork: overlaps_btree(&self.dontfork_ranges),
            wipe_on_fork: overlaps_btree(&self.wipe_on_fork_ranges),
            lazyfree_bytes: self
                .madvise_free_pages
                .range(start..end)
                .count()
                * PAGE_SIZE_4K,
            thp_enabled: matches!(self.thp_disable_mode, ThpDisableMode::Enabled),
        }
    }
}

/// Procfs-visible policy observations for one VMA. `lazyfree_bytes` counts
/// the pages admitted by MADV_FREE that have not been reclaimed yet.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ProcVmaObservations {
    pub(crate) growsdown: bool,
    pub(crate) locked: bool,
    pub(crate) dontdump: bool,
    pub(crate) dontfork: bool,
    pub(crate) wipe_on_fork: bool,
    pub(crate) lazyfree_bytes: usize,
    pub(crate) thp_enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn swap_observation_counts_only_the_half_open_vma_range() {
        let entries: BTreeMap<_, _> = [0x1000usize, 0x2000, 0x4000, 0x5000]
            .into_iter()
            .map(|address| (VirtAddr::from(address), ()))
            .collect();
        assert_eq!(swapped_bytes(&entries, VirtAddr::from(0x2000), 0), 0);
        assert_eq!(
            swapped_bytes(&entries, VirtAddr::from(0x2000), 0x3000),
            8192
        );
        assert_eq!(swapped_bytes(&entries, VirtAddr::from(0x3000), 4096), 0);
        assert_eq!(swapped_bytes(&entries, VirtAddr::from(0x5000), 4096), 4096);
    }
}
