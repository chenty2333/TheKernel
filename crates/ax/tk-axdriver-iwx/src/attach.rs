//! DMA resource allocation in the OpenBSD iwx PCI attach path.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 (ISC).
//! Copyright (c) 2014, 2016 genua gmbh <info@genua.de>; Copyright (c) 2014
//! Fixup Software Ltd.; Copyright (c) 2017, 2019, 2020 Stefan Sperling
//! <stsp@openbsd.org>.

use alloc::vec::Vec;

use crate::{
    DeviceFamily, DmaAllocator, DmaError, DmaRegion, IctError, InterruptCauseTable, RingError,
    RxRing, TxRing, allocate_rx_ring, allocate_tx_ring_for_family,
};

pub const PCI_TX_QUEUE_COUNT: usize = 10;
pub const PRPH_INFO_BYTES: usize = 4096;
pub const ICT_BYTES: usize = 4096;
pub const ICT_ALIGNMENT: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachAllocationStage {
    ContextInfo,
    PrphScratch,
    PrphInfo,
    Ict,
    TxRing(u8),
    RxRing,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachAllocationError {
    Allocation(AttachAllocationStage, DmaError),
    Ring(AttachAllocationStage, RingError),
    InvalidAlignment,
}

/// DMA allocations retained for the complete lifetime of one PCI iwx instance.
pub struct IwxAttachResources<R: DmaRegion> {
    pub context_info: R,
    pub prph_scratch: Option<R>,
    pub prph_info: Option<R>,
    pub ict: InterruptCauseTable<R>,
    pub tx_queues: Vec<TxRing<R>>,
    pub rx_queue: RxRing<R>,
}

/// Allocate resources in the order used by iwx_attach(), with RAII rollback on failure.
// upstream: if_iwx.c iwx_attach()
pub fn allocate_attach_resources<A: DmaAllocator>(
    allocator: &mut A,
    family: DeviceFamily,
) -> Result<IwxAttachResources<A::Region>, AttachAllocationError> {
    let gen3 = family >= DeviceFamily::Ax210;
    let context_size = if gen3 {
        crate::GEN3_CONTEXT_BYTES
    } else {
        crate::GEN2_CONTEXT_BYTES
    };
    let context_info = zero_region(allocator, context_size, 0).map_err(|error| {
        AttachAllocationError::Allocation(AttachAllocationStage::ContextInfo, error)
    })?;
    let (prph_scratch, prph_info) = if gen3 {
        let scratch = zero_region(allocator, crate::PRPH_SCRATCH_BYTES, 0).map_err(|error| {
            AttachAllocationError::Allocation(AttachAllocationStage::PrphScratch, error)
        })?;
        let info = zero_region(allocator, PRPH_INFO_BYTES, 0).map_err(|error| {
            AttachAllocationError::Allocation(AttachAllocationStage::PrphInfo, error)
        })?;
        (Some(scratch), Some(info))
    } else {
        (None, None)
    };
    let ict_memory = zero_region(allocator, ICT_BYTES, ICT_ALIGNMENT)
        .map_err(|error| AttachAllocationError::Allocation(AttachAllocationStage::Ict, error))?;
    if ict_memory.device_address() & (ICT_ALIGNMENT as u64 - 1) != 0 {
        return Err(AttachAllocationError::InvalidAlignment);
    }
    let ict = InterruptCauseTable {
        memory: ict_memory,
        current: 0,
    };
    let mut tx_queues = Vec::new();
    tx_queues
        .try_reserve_exact(PCI_TX_QUEUE_COUNT)
        .map_err(|_| {
            AttachAllocationError::Allocation(
                AttachAllocationStage::TxRing(0),
                DmaError::AllocationFailed,
            )
        })?;
    for queue in 0..PCI_TX_QUEUE_COUNT {
        let ring =
            allocate_tx_ring_for_family(allocator, queue as u16, family).map_err(|error| {
                match error {
                    RingError::Dma(dma) => AttachAllocationError::Allocation(
                        AttachAllocationStage::TxRing(queue as u8),
                        dma,
                    ),
                    other => AttachAllocationError::Ring(
                        AttachAllocationStage::TxRing(queue as u8),
                        other,
                    ),
                }
            })?;
        tx_queues.push(ring);
    }
    let rx_queue = allocate_rx_ring(allocator).map_err(|error| match error {
        RingError::Dma(dma) => {
            AttachAllocationError::Allocation(AttachAllocationStage::RxRing, dma)
        }
        other => AttachAllocationError::Ring(AttachAllocationStage::RxRing, other),
    })?;
    Ok(IwxAttachResources {
        context_info,
        prph_scratch,
        prph_info,
        ict,
        tx_queues,
        rx_queue,
    })
}

fn zero_region<A: DmaAllocator>(
    allocator: &mut A,
    size: usize,
    alignment: usize,
) -> Result<A::Region, DmaError> {
    let mut region = if alignment == 0 {
        allocator.allocate(size)?
    } else {
        allocator.allocate_aligned(size, alignment)?
    };
    if region.capacity() < size {
        return Err(DmaError::RegionTooSmall);
    }
    let mut zeros = Vec::new();
    zeros
        .try_reserve_exact(size)
        .map_err(|_| DmaError::AllocationFailed)?;
    zeros.resize(size, 0);
    region.write_at(0, &zeros)?;
    Ok(region)
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use core::cell::Cell;

    use super::*;

    struct Region {
        address: u64,
        bytes: Vec<u8>,
    }
    impl DmaRegion for Region {
        fn device_address(&self) -> u64 {
            self.address
        }
        fn capacity(&self) -> usize {
            self.bytes.len()
        }
        fn write(&mut self, bytes: &[u8]) -> Result<(), DmaError> {
            self.write_at(0, bytes)
        }
        fn write_at(&mut self, offset: usize, bytes: &[u8]) -> Result<(), DmaError> {
            self.bytes
                .get_mut(offset..offset + bytes.len())
                .ok_or(DmaError::RegionTooSmall)?
                .copy_from_slice(bytes);
            Ok(())
        }
        fn read_at(&self, offset: usize, bytes: &mut [u8]) -> Result<(), DmaError> {
            bytes.copy_from_slice(
                self.bytes
                    .get(offset..offset + bytes.len())
                    .ok_or(DmaError::RegionTooSmall)?,
            );
            Ok(())
        }
    }
    struct Allocator {
        next: Cell<u64>,
    }
    impl DmaAllocator for Allocator {
        type Region = Region;
        fn allocate(&mut self, size: usize) -> Result<Region, DmaError> {
            let address = (self.next.get() + 4095) & !4095;
            self.next.set(address + size as u64 + 4096);
            Ok(Region {
                address,
                bytes: vec![0xff; size],
            })
        }
    }

    #[test]
    fn attach_allocates_common_then_gen3_dma_then_ten_tx_and_rx_queues() {
        let mut allocator = Allocator {
            next: Cell::new(0x1000),
        };
        let resources = allocate_attach_resources(&mut allocator, DeviceFamily::Ax210).unwrap();
        assert_eq!(resources.context_info.capacity(), crate::GEN3_CONTEXT_BYTES);
        assert_eq!(
            resources.prph_scratch.as_ref().unwrap().capacity(),
            crate::PRPH_SCRATCH_BYTES
        );
        assert_eq!(
            resources.prph_info.as_ref().unwrap().capacity(),
            PRPH_INFO_BYTES
        );
        assert_eq!(resources.ict.memory.capacity(), ICT_BYTES);
        assert_eq!(resources.tx_queues.len(), PCI_TX_QUEUE_COUNT);
        assert_eq!(resources.tx_queues[0].max_tfd_queue_size, 65_536);
        assert_eq!(
            resources.rx_queue.buffers.len(),
            crate::rings::RX_MQ_RING_COUNT
        );
        let mut old_allocator = Allocator {
            next: Cell::new(0x1000),
        };
        let old = allocate_attach_resources(&mut old_allocator, DeviceFamily::Family22000).unwrap();
        assert_eq!(old.context_info.capacity(), crate::GEN2_CONTEXT_BYTES);
        assert!(old.prph_info.is_none());
    }
}
