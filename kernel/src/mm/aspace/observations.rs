//! Non-mutating observations of the mm-owned software swap PTE registry.
use super::*;

fn swapped_bytes<T>(entries: &BTreeMap<VirtAddr, T>, start: VirtAddr, size: usize) -> usize {
    if size == 0 {
        return 0;
    }
    entries.range(start..start + size).count() * PAGE_SIZE_4K
}

impl AddrSpace {
    /// VMA ranges are page-aligned. Count owned software swap leaves rather
    /// than interpreting hardware table holes as swapped memory.
    pub(crate) fn swapped_bytes_in_range(&self, start: VirtAddr, size: usize) -> usize {
        swapped_bytes(&self.swapped, start, size)
    }
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
