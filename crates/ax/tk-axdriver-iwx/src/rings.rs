//! AX210 RX/TX descriptor-ring setup from OpenBSD iwx.
//!
//! Upstream: OpenBSD `sys/dev/pci/if_iwx.c` revision 1.230,
//! `iwx_alloc_rx_ring()`, `iwx_update_rx_desc()`, `iwx_reset_rx_ring()`,
//! `iwx_alloc_tx_ring()`, `iwx_reset_tx_ring()`, `iwx_clear_tx_desc()`, and
//! `iwx_tx_update_byte_tbl()`. ISC. Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

use alloc::vec::Vec;

use crate::{DmaAllocator, DmaError, DmaRegion};

pub const RX_MQ_RING_COUNT: usize = 512;
pub const RX_BUFFER_BYTES: usize = 4096;
pub const TX_RING_COUNT: usize = 256;
pub const GEN3_MAX_TFD_QUEUE_SIZE: usize = 65_536;
pub const TX_DESCRIPTOR_BYTES: usize = 256;
pub const TX_BUFFER_COUNT: usize = 25;
pub const TX_COMMAND_BYTES: usize = 324;
pub const TX_BC_TABLE_COUNT: usize = 1024;
pub const RX_TRANSFER_DESCRIPTOR_BYTES: usize = 16;
pub const RX_COMPLETION_DESCRIPTOR_BYTES: usize = 32;

const RX_DESCRIPTOR_ALIGNMENT: usize = 256;
const RX_STATUS_ALIGNMENT: usize = 16;
const TX_COMMAND_ALIGNMENT: usize = 64;
const TX_BC_ALIGNMENT: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RingError {
    Dma(DmaError),
    InvalidIndex,
    RingFull,
    TooManyBuffers,
    InvalidCompletion,
}

impl From<DmaError> for RingError {
    fn from(error: DmaError) -> Self {
        Self::Dma(error)
    }
}

/// AX210 RX completion metadata from the 32-byte completion descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RxCompletion {
    pub buffer_id: u16,
    pub fragmented: bool,
}

/// RX descriptor and buffer memory for the multi-queue default receive ring.
pub struct RxRing<R: DmaRegion> {
    pub free_descriptors: R,
    pub status: R,
    pub used_descriptors: R,
    pub buffers: Vec<R>,
    pub current: usize,
}

impl<R: DmaRegion> RxRing<R> {
    /// Read the firmware-owned write index in the 16-bit AX210 status word.
    pub fn hardware_index(&self) -> Result<usize, RingError> {
        let mut raw = [0; 2];
        self.status.read_at(0, &mut raw)?;
        Ok((u16::from_le_bytes(raw) as usize) & (RX_MQ_RING_COUNT - 1))
    }

    /// Decode a completion ring entry.
    // upstream: if_iwx.c iwx_rx_mpdu_mq()
    pub fn completion(&self, ring_index: usize) -> Result<RxCompletion, RingError> {
        if ring_index >= RX_MQ_RING_COUNT {
            return Err(RingError::InvalidIndex);
        }
        let mut desc = [0; RX_COMPLETION_DESCRIPTOR_BYTES];
        self.used_descriptors
            .read_at(ring_index * RX_COMPLETION_DESCRIPTOR_BYTES, &mut desc)?;
        Ok(RxCompletion {
            buffer_id: u16::from_le_bytes([desc[4], desc[5]]),
            fragmented: desc[6] & 1 != 0,
        })
    }

    /// Copy received bytes from the RBD buffer selected by `buffer_id`.
    // upstream: if_iwx.c iwx_rx_mpdu_mq()
    pub fn read_buffer(
        &self,
        buffer_id: u16,
        offset: usize,
        output: &mut [u8],
    ) -> Result<(), RingError> {
        let buffer = self
            .buffers
            .get(buffer_id as usize)
            .ok_or(RingError::InvalidIndex)?;
        buffer.read_at(offset, output)?;
        Ok(())
    }

    /// Re-arm an RBD descriptor after its Ethernet-frame bytes have been copied out.
    // upstream: if_iwx.c iwx_update_rx_desc()
    pub fn repost(&mut self, buffer_id: u16) -> Result<(), RingError> {
        let index = buffer_id as usize;
        if index >= self.buffers.len() {
            return Err(RingError::InvalidIndex);
        }
        let mut descriptor = [0u8; RX_TRANSFER_DESCRIPTOR_BYTES];
        descriptor[0..2].copy_from_slice(&buffer_id.to_le_bytes());
        descriptor[8..16].copy_from_slice(&self.buffers[index].device_address().to_le_bytes());
        self.free_descriptors
            .write_at(index * RX_TRANSFER_DESCRIPTOR_BYTES, &descriptor)?;
        Ok(())
    }

    /// Allocate and post a replacement RX buffer, returning the completed one
    /// to the caller for packet processing.
    // upstream: if_iwx.c iwx_rx_addbuf()
    pub fn refill_buffer<A: DmaAllocator<Region = R>>(
        &mut self,
        allocator: &mut A,
        buffer_id: u16,
    ) -> Result<R, RingError> {
        let index = usize::from(buffer_id);
        if index >= self.buffers.len() {
            return Err(RingError::InvalidIndex);
        }
        let replacement = zeroed_dma(allocator, RX_BUFFER_BYTES, 4096)?;
        let completed = core::mem::replace(&mut self.buffers[index], replacement);
        if let Err(error) = self.repost(buffer_id) {
            self.buffers[index] = completed;
            return Err(error);
        }
        Ok(completed)
    }

    /// Reset the hardware status cursor and re-publish every free RBD descriptor.
    // upstream: if_iwx.c iwx_reset_rx_ring()
    pub fn reset(&mut self) -> Result<(), RingError> {
        self.current = 0;
        self.status.write_at(0, &[0, 0])?;
        Ok(())
    }
}

/// Allocate the three AX210 receive rings and all 512 4-KiB receive buffers.
// upstream: if_iwx.c iwx_alloc_rx_ring()
pub fn allocate_rx_ring<A: DmaAllocator>(
    allocator: &mut A,
) -> Result<RxRing<A::Region>, RingError> {
    let free_size = RX_TRANSFER_DESCRIPTOR_BYTES * RX_MQ_RING_COUNT;
    let used_size = RX_COMPLETION_DESCRIPTOR_BYTES * RX_MQ_RING_COUNT;
    let mut free_descriptors = zeroed_dma(allocator, free_size, RX_DESCRIPTOR_ALIGNMENT)?;
    let status = zeroed_dma(allocator, 2, RX_STATUS_ALIGNMENT)?;
    let used_descriptors = zeroed_dma(allocator, used_size, RX_DESCRIPTOR_ALIGNMENT)?;
    let mut buffers = Vec::new();
    buffers
        .try_reserve_exact(RX_MQ_RING_COUNT)
        .map_err(|_| RingError::Dma(DmaError::AllocationFailed))?;
    for _ in 0..RX_MQ_RING_COUNT {
        buffers.push(allocator.allocate(RX_BUFFER_BYTES)?);
    }
    for (index, buffer) in buffers.iter().enumerate() {
        let mut descriptor = [0u8; RX_TRANSFER_DESCRIPTOR_BYTES];
        descriptor[..2].copy_from_slice(&(index as u16).to_le_bytes());
        descriptor[8..16].copy_from_slice(&buffer.device_address().to_le_bytes());
        free_descriptors.write_at(index * RX_TRANSFER_DESCRIPTOR_BYTES, &descriptor)?;
    }
    Ok(RxRing {
        free_descriptors,
        status,
        used_descriptors,
        buffers,
        current: 0,
    })
}

/// A physical memory segment referenced by a TFD transfer-buffer entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxSegment {
    pub address: u64,
    pub length: u16,
}

/// AX210 transmit-ring memory and producer/consumer accounting.
pub struct TxRing<R: DmaRegion> {
    pub descriptors: R,
    pub byte_counts: R,
    pub commands: R,
    pub queue_id: u16,
    pub max_tfd_queue_size: usize,
    pub current: usize,
    pub current_hardware: usize,
    pub tail: usize,
    pub tail_hardware: usize,
    pub queued: usize,
}

impl<R: DmaRegion> TxRing<R> {
    /// Number of descriptors left while preserving one empty ring slot.
    pub fn available(&self) -> usize {
        TX_RING_COUNT - 1 - self.queued
    }

    /// Address of a preallocated `iwx_device_cmd` slot.
    pub fn command_address(&self, index: usize) -> Result<u64, RingError> {
        if index >= TX_RING_COUNT {
            return Err(RingError::InvalidIndex);
        }
        Ok(self.commands.device_address() + (index * TX_COMMAND_BYTES) as u64)
    }

    /// Build and publish one AX210 TFD and its Gen3 byte-count entry.
    // upstream: if_iwx.c iwx_tx()
    pub fn submit(&mut self, segments: &[TxSegment], byte_count: u16) -> Result<usize, RingError> {
        self.submit_inner(segments, Some(byte_count))
    }

    /// Submit a frame and ring the AX210/Gen3 HBUS hardware write-pointer.
    // upstream: if_iwx.c iwx_tx()
    pub fn submit_and_kick<B: crate::CsrAccess>(
        &mut self,
        registers: &mut crate::IwxRegisters<B>,
        segments: &[TxSegment],
        byte_count: u16,
    ) -> Result<usize, RingError> {
        let index = self.submit(segments, byte_count)?;
        registers.kick_tx_queue(self.queue_id, self.current_hardware);
        Ok(index)
    }

    /// Submit a command-queue TFD without changing the scheduler byte-count table.
    // upstream: if_iwx.c iwx_send_cmd() leaves command-queue byte-count entries alone
    pub fn submit_without_byte_count(
        &mut self,
        segments: &[TxSegment],
    ) -> Result<usize, RingError> {
        self.submit_inner(segments, None)
    }

    /// Submit a host command and notify firmware without touching the byte-count table.
    // upstream: if_iwx.c iwx_send_cmd()
    pub fn submit_command_and_kick<B: crate::CsrAccess>(
        &mut self,
        registers: &mut crate::IwxRegisters<B>,
        segments: &[TxSegment],
    ) -> Result<usize, RingError> {
        let index = self.submit_without_byte_count(segments)?;
        registers.kick_tx_queue(self.queue_id, self.current_hardware);
        Ok(index)
    }

    fn submit_inner(
        &mut self,
        segments: &[TxSegment],
        byte_count: Option<u16>,
    ) -> Result<usize, RingError> {
        if self.queued >= TX_RING_COUNT - 1 {
            return Err(RingError::RingFull);
        }
        if segments.is_empty() || segments.len() > TX_BUFFER_COUNT {
            return Err(RingError::TooManyBuffers);
        }
        let index = self.current;
        let mut tfd = [0u8; TX_DESCRIPTOR_BYTES];
        tfd[..2].copy_from_slice(&(segments.len() as u16).to_le_bytes());
        for (slot, segment) in segments.iter().enumerate() {
            let offset = 2 + slot * 10;
            tfd[offset..offset + 2].copy_from_slice(&segment.length.to_le_bytes());
            tfd[offset + 2..offset + 10].copy_from_slice(&segment.address.to_le_bytes());
        }
        self.descriptors
            .write_at(index * TX_DESCRIPTOR_BYTES, &tfd)?;
        if let Some(byte_count) = byte_count {
            let entry = tx_byte_count_entry(byte_count, segments.len())?;
            self.byte_counts.write_at(index * 2, &entry.to_le_bytes())?;
        }
        self.current = (self.current + 1) % TX_RING_COUNT;
        self.current_hardware = (self.current_hardware + 1) % self.max_tfd_queue_size;
        self.queued += 1;
        Ok(index)
    }

    /// Retire descriptors up to (but excluding) the hardware producer index.
    // upstream: if_iwx.c iwx_txq_advance()
    pub fn advance_to(&mut self, hardware_index: usize) -> Result<Vec<usize>, RingError> {
        if hardware_index >= self.max_tfd_queue_size {
            return Err(RingError::InvalidIndex);
        }
        let mut completed = Vec::new();
        while self.tail_hardware != hardware_index {
            completed
                .try_reserve(1)
                .map_err(|_| RingError::Dma(DmaError::AllocationFailed))?;
            completed.push(self.tail);
            self.tail = (self.tail + 1) % TX_RING_COUNT;
            self.tail_hardware = (self.tail_hardware + 1) % self.max_tfd_queue_size;
            if self.queued > 0 {
                self.queued -= 1;
            }
        }
        Ok(completed)
    }

    /// Keep the bidirectional first TB and clear all other descriptor slots.
    // upstream: if_iwx.c iwx_clear_tx_desc()
    pub fn clear_descriptor(&mut self, index: usize) -> Result<(), RingError> {
        if index >= TX_RING_COUNT {
            return Err(RingError::InvalidIndex);
        }
        let mut descriptor = [0; TX_DESCRIPTOR_BYTES];
        self.descriptors
            .read_at(index * TX_DESCRIPTOR_BYTES, &mut descriptor)?;
        let count = (u16::from_le_bytes([descriptor[0], descriptor[1]]) & 0x1f) as usize;
        for slot in 1..count.min(TX_BUFFER_COUNT) {
            let offset = 2 + slot * 10;
            descriptor[offset..offset + 10].fill(0);
        }
        descriptor[0..2].copy_from_slice(&1u16.to_le_bytes());
        self.descriptors
            .write_at(index * TX_DESCRIPTOR_BYTES, &descriptor)?;
        self.byte_counts.write_at(index * 2, &0u16.to_le_bytes())?;
        Ok(())
    }

    /// Reset software producer/consumer state and clear device-visible tables.
    // upstream: if_iwx.c iwx_reset_tx_ring()
    pub fn reset(&mut self) -> Result<(), RingError> {
        self.current = 0;
        self.current_hardware = 0;
        self.tail = 0;
        self.tail_hardware = 0;
        self.queued = 0;
        zero_region(&mut self.descriptors)?;
        zero_region(&mut self.byte_counts)?;
        Ok(())
    }
}

/// Allocate AX210 TX descriptors, Gen3 byte-count table, and command slots.
// upstream: if_iwx.c iwx_alloc_tx_ring()
pub fn allocate_tx_ring<A: DmaAllocator>(
    allocator: &mut A,
    queue_id: u16,
) -> Result<TxRing<A::Region>, RingError> {
    allocate_tx_ring_for(allocator, queue_id, TX_RING_COUNT)
}

pub fn allocate_tx_ring_for<A: DmaAllocator>(
    allocator: &mut A,
    queue_id: u16,
    max_tfd_queue_size: usize,
) -> Result<TxRing<A::Region>, RingError> {
    if max_tfd_queue_size < TX_RING_COUNT || !max_tfd_queue_size.is_power_of_two() {
        return Err(RingError::InvalidIndex);
    }
    let descriptors = zeroed_dma(allocator, TX_RING_COUNT * TX_DESCRIPTOR_BYTES, 256)?;
    let byte_counts = zeroed_dma(allocator, TX_BC_TABLE_COUNT * 2, TX_BC_ALIGNMENT)?;
    let commands = zeroed_dma(
        allocator,
        TX_RING_COUNT * TX_COMMAND_BYTES,
        TX_COMMAND_ALIGNMENT,
    )?;
    Ok(TxRing {
        descriptors,
        byte_counts,
        commands,
        queue_id,
        max_tfd_queue_size,
        current: 0,
        current_hardware: 0,
        tail: 0,
        tail_hardware: 0,
        queued: 0,
    })
}

/// Select the hardware write-pointer modulus from the device generation.
pub fn allocate_tx_ring_for_family<A: DmaAllocator>(
    allocator: &mut A,
    queue_id: u16,
    family: crate::DeviceFamily,
) -> Result<TxRing<A::Region>, RingError> {
    let queue_size = if family >= crate::DeviceFamily::Ax210 {
        GEN3_MAX_TFD_QUEUE_SIZE
    } else {
        TX_RING_COUNT
    };
    allocate_tx_ring_for(allocator, queue_id, queue_size)
}

/// Compute the Gen3 scheduler entry, including the TFD fetch-chunk count.
// upstream: if_iwx.c iwx_tx_update_byte_tbl()
pub fn tx_byte_count_entry(byte_count: u16, num_tbs: usize) -> Result<u16, RingError> {
    if num_tbs > TX_BUFFER_COUNT {
        return Err(RingError::TooManyBuffers);
    }
    let descriptor_size = 2usize
        .checked_add(num_tbs.checked_mul(10).ok_or(RingError::TooManyBuffers)?)
        .ok_or(RingError::TooManyBuffers)?;
    let chunks = descriptor_size.div_ceil(64).saturating_sub(1);
    if chunks > 3 {
        return Err(RingError::TooManyBuffers);
    }
    Ok(byte_count | ((chunks as u16) << 14))
}

fn zeroed_dma<A: DmaAllocator>(
    allocator: &mut A,
    size: usize,
    alignment: usize,
) -> Result<A::Region, RingError> {
    let mut region = allocator.allocate_aligned(size, alignment)?;
    if region.capacity() < size {
        return Err(RingError::Dma(DmaError::RegionTooSmall));
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|_| RingError::Dma(DmaError::AllocationFailed))?;
    bytes.resize(size, 0);
    region.write_at(0, &bytes)?;
    Ok(region)
}

fn zero_region<R: DmaRegion>(region: &mut R) -> Result<(), RingError> {
    let size = region.capacity();
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|_| RingError::Dma(DmaError::AllocationFailed))?;
    bytes.resize(size, 0);
    region.write_at(0, &bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
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
            self.bytes.copy_from_slice(bytes);
            Ok(())
        }
        fn write_at(&mut self, offset: usize, bytes: &[u8]) -> Result<(), DmaError> {
            let end = offset
                .checked_add(bytes.len())
                .ok_or(DmaError::RegionTooSmall)?;
            self.bytes
                .get_mut(offset..end)
                .ok_or(DmaError::RegionTooSmall)?
                .copy_from_slice(bytes);
            Ok(())
        }
        fn read_at(&self, offset: usize, bytes: &mut [u8]) -> Result<(), DmaError> {
            let end = offset
                .checked_add(bytes.len())
                .ok_or(DmaError::RegionTooSmall)?;
            bytes.copy_from_slice(
                self.bytes
                    .get(offset..end)
                    .ok_or(DmaError::RegionTooSmall)?,
            );
            Ok(())
        }
    }
    struct Alloc(Cell<u64>);
    impl DmaAllocator for Alloc {
        type Region = Region;
        fn allocate(&mut self, size: usize) -> Result<Region, DmaError> {
            let address = (self.0.get() + 4095) & !4095;
            self.0.set(address + size as u64);
            Ok(Region {
                address,
                bytes: vec![0; size],
            })
        }
    }

    #[test]
    fn ax210_rx_ring_uses_expected_alignment_and_rbid_addresses() {
        let mut alloc = Alloc(Cell::new(0x100000));
        let mut ring = allocate_rx_ring(&mut alloc).unwrap();
        assert_eq!(ring.free_descriptors.device_address() % 256, 0);
        assert_eq!(ring.status.device_address() % 16, 0);
        assert_eq!(ring.used_descriptors.device_address() % 256, 0);
        let completion = ring.completion(0).unwrap();
        assert_eq!(completion.buffer_id, 0);
        ring.repost(511).unwrap();
        let mut desc = [0; 16];
        ring.free_descriptors.read_at(511 * 16, &mut desc).unwrap();
        assert_eq!(u16::from_le_bytes([desc[0], desc[1]]), 511);
        assert_eq!(
            u64::from_le_bytes(desc[8..16].try_into().unwrap()),
            ring.buffers[511].device_address()
        );
        let old_address = ring.buffers[511].device_address();
        let completed = ring.refill_buffer(&mut alloc, 511).unwrap();
        assert_eq!(completed.device_address(), old_address);
        assert_ne!(ring.buffers[511].device_address(), old_address);
        ring.free_descriptors.read_at(511 * 16, &mut desc).unwrap();
        assert_eq!(
            u64::from_le_bytes(desc[8..16].try_into().unwrap()),
            ring.buffers[511].device_address()
        );
    }

    #[test]
    fn tx_ring_tfd_bytecount_ring_full_and_completion_follow_hardware_indices() {
        let mut alloc = Alloc(Cell::new(0x100000));
        let mut ring = allocate_tx_ring(&mut alloc, 0).unwrap();
        let index = ring
            .submit(
                &[TxSegment {
                    address: 0xfeed_0000,
                    length: 100,
                }],
                100,
            )
            .unwrap();
        assert_eq!(index, 0);
        let mut tfd = [0; 256];
        ring.descriptors.read_at(0, &mut tfd).unwrap();
        assert_eq!(u16::from_le_bytes([tfd[0], tfd[1]]), 1);
        assert_eq!(
            u64::from_le_bytes(tfd[4..12].try_into().unwrap()),
            0xfeed_0000
        );
        assert_eq!(ring.advance_to(1).unwrap(), [0]);
        for _ in 0..(TX_RING_COUNT - 1) {
            ring.submit(
                &[TxSegment {
                    address: 0xfeed_0000,
                    length: 100,
                }],
                100,
            )
            .unwrap();
        }
        assert_eq!(ring.available(), 0);
        assert_eq!(
            ring.submit(
                &[TxSegment {
                    address: 0,
                    length: 1
                }],
                1
            ),
            Err(RingError::RingFull)
        );
        assert_eq!(ring.advance_to(0).unwrap().len(), TX_RING_COUNT - 1);
        assert_eq!(ring.available(), TX_RING_COUNT - 1);
        ring.clear_descriptor(0).unwrap();
        assert_eq!(tx_byte_count_entry(100, 1).unwrap(), 100);
        assert_eq!(tx_byte_count_entry(100, 7).unwrap(), 100 | (1 << 14));
        assert!(tx_byte_count_entry(100, 25).is_ok());
        assert_eq!(tx_byte_count_entry(100, 26), Err(RingError::TooManyBuffers));
    }

    #[test]
    fn gen3_hardware_pointer_wrap_uses_full_queue_size() {
        let mut alloc = Alloc(Cell::new(0x1000));
        let mut ring =
            allocate_tx_ring_for_family(&mut alloc, 7, crate::DeviceFamily::Ax210).unwrap();
        ring.current_hardware = GEN3_MAX_TFD_QUEUE_SIZE - 1;
        ring.submit(
            &[TxSegment {
                address: 0x2000,
                length: 8,
            }],
            8,
        )
        .unwrap();
        assert_eq!(ring.current_hardware, 0);
        ring.tail_hardware = GEN3_MAX_TFD_QUEUE_SIZE - 1;
        assert_eq!(ring.advance_to(0).unwrap(), [0]);
        assert_eq!(ring.tail_hardware, 0);
    }
}
