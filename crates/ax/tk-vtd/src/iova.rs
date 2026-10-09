//! Shared-domain IOVA facade over the translated guest-address-space allocator.
use crate::{
    Error,
    gas::{ENTRY_MAP, GasConstraints, GasDomain},
    pgtbl::PAGE_SIZE,
};

pub struct IovaAllocator {
    gas: GasDomain,
}

impl IovaAllocator {
    pub fn new(start: u64, end_exclusive: u64) -> Result<Self, Error> {
        Ok(Self {
            gas: GasDomain::new_range(0, start, end_exclusive)?,
        })
    }

    pub fn allocate(&mut self, length: usize) -> Result<u64, Error> {
        let length = length
            .checked_add(PAGE_SIZE as usize - 1)
            .ok_or(Error::InvalidRange)?
            & !(PAGE_SIZE as usize - 1);
        if length == 0 {
            return Err(Error::InvalidRange);
        }
        let end = self.gas.end - 1;
        let entry = self.gas.iommu_gas_map(
            GasConstraints {
                size: length as u64,
                offset: 0,
                alignment: PAGE_SIZE,
                boundary: 0,
                low_address: end,
                high_address: end,
                can_split: false,
            },
            ENTRY_MAP,
            |_| Ok(()),
            |_| Ok(()),
        )?;
        Ok(entry.start)
    }

    pub fn release(&mut self, address: u64, length: usize) -> Result<(), Error> {
        let length = length
            .checked_add(PAGE_SIZE as usize - 1)
            .ok_or(Error::InvalidRange)?
            & !(PAGE_SIZE as usize - 1);
        if length == 0 || address & (PAGE_SIZE - 1) != 0 {
            return Err(Error::InvalidRange);
        }
        let entry = self
            .gas
            .entries
            .iter()
            .find(|entry| {
                entry.start == address
                    && entry.end - entry.start == length as u64
                    && entry.flags & ENTRY_MAP != 0
            })
            .cloned()
            .ok_or(Error::InvalidRange)?;
        let _ = self.gas.iommu_gas_free_space(entry.start)?;
        self.gas.iommu_gas_free_entry(entry)
    }

    pub fn allocated_pages(&self) -> usize {
        self.gas
            .entries
            .iter()
            .filter(|entry| entry.flags & ENTRY_MAP != 0)
            .map(|entry| ((entry.end - entry.start) / PAGE_SIZE) as usize)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligned_first_fit_reuses_iovas_and_preserves_guard_pages() {
        let mut iovas = IovaAllocator::new(0x1000, 0x9000).unwrap();
        let a = iovas.allocate(1).unwrap();
        let b = iovas.allocate(4097).unwrap();
        assert_eq!((a, b), (0x2000, 0x4000));
        iovas.release(a, 1).unwrap();
        assert_eq!(iovas.allocate(4096).unwrap(), a);
        assert_eq!(iovas.allocated_pages(), 3);
    }

    #[test]
    fn rejects_bad_ranges_double_release_and_guard_exhaustion() {
        assert_eq!(
            IovaAllocator::new(1, 0x1000).err(),
            Some(Error::InvalidRange)
        );
        let mut iovas = IovaAllocator::new(0x1000, 0x5000).unwrap();
        let address = iovas.allocate(4096).unwrap();
        assert_eq!(iovas.allocate(4096).err(), Some(Error::OutOfMemory));
        iovas.release(address, 4096).unwrap();
        assert_eq!(iovas.release(address, 4096), Err(Error::InvalidRange));
    }
}
