//! First-fit page-aligned IOVA allocator for shared VT-d domains.
use alloc::collections::BTreeMap;

use crate::Error;

pub struct IovaAllocator {
    start: u64,
    end: u64,
    allocations: BTreeMap<u64, usize>,
}

impl IovaAllocator {
    pub fn new(start: u64, end_exclusive: u64) -> Result<Self, Error> {
        if start >= end_exclusive || start & 0xfff != 0 || end_exclusive & 0xfff != 0 {
            return Err(Error::InvalidRange);
        }
        Ok(Self {
            start,
            end: end_exclusive,
            allocations: BTreeMap::new(),
        })
    }

    pub fn allocate(&mut self, length: usize) -> Result<u64, Error> {
        let length = length.checked_add(0xfff).ok_or(Error::InvalidRange)? & !0xfff;
        if length == 0 {
            return Err(Error::InvalidRange);
        }
        let mut candidate = self.start;
        for (&base, &allocated) in &self.allocations {
            if candidate
                .checked_add(length as u64)
                .is_none_or(|end| end > base)
            {
                candidate = base
                    .checked_add(allocated as u64)
                    .ok_or(Error::InvalidRange)?;
            } else {
                break;
            }
        }
        if candidate
            .checked_add(length as u64)
            .is_none_or(|end| end > self.end)
        {
            return Err(Error::OutOfMemory);
        }
        if self.allocations.insert(candidate, length).is_some() {
            return Err(Error::MapFailed);
        }
        Ok(candidate)
    }

    pub fn release(&mut self, address: u64, length: usize) -> Result<(), Error> {
        let length = length.checked_add(0xfff).ok_or(Error::InvalidRange)? & !0xfff;
        if address & 0xfff != 0 || self.allocations.get(&address) != Some(&length) {
            return Err(Error::InvalidRange);
        }
        self.allocations.remove(&address);
        Ok(())
    }

    pub fn allocated_pages(&self) -> usize {
        self.allocations.values().map(|length| length / 4096).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligned_first_fit_reuses_unmapped_iovas() {
        let mut iovas = IovaAllocator::new(0x1000, 0x9000).unwrap();
        let a = iovas.allocate(1).unwrap();
        let b = iovas.allocate(4097).unwrap();
        assert_eq!((a, b), (0x1000, 0x2000));
        iovas.release(a, 1).unwrap();
        assert_eq!(iovas.allocate(4096).unwrap(), a);
        assert_eq!(iovas.allocated_pages(), 3);
    }

    #[test]
    fn rejects_bad_ranges_and_double_release() {
        assert_eq!(
            IovaAllocator::new(1, 0x1000).err(),
            Some(Error::InvalidRange)
        );
        let mut iovas = IovaAllocator::new(0x1000, 0x2000).unwrap();
        let address = iovas.allocate(4096).unwrap();
        assert_eq!(iovas.allocate(4096).err(), Some(Error::OutOfMemory));
        iovas.release(address, 4096).unwrap();
        assert_eq!(iovas.release(address, 4096), Err(Error::InvalidRange));
    }
}
