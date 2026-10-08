//! Single-queue DMA rings and `NetDriverOps` for the e1000 MAC family.
//!
//! Translated from FreeBSD `sys/dev/e1000/{e1000_82540.c,e1000_nvm.c,em_txrx.c,if_em.c}`
//! and Intel `sys/dev/e1000/{e1000_regs.h,e1000_defines.h}` at
//! `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-2-Clause AND BSD-3-Clause).
//! Copyright (c) 2001-2024, Intel Corporation.
//! Copyright (c) 2016 Nicole Graziano <nicole@nextbsd.org>.
//! Copyright (c) 2017 Matthew Macy <mmacy@mattmacy.io>.
//! Copyright (c) 2024 Kevin Bowling <kbowling@FreeBSD.org>.

use alloc::{vec, vec::Vec};
use core::{
    marker::PhantomData,
    mem::size_of,
    ptr::{self, NonNull},
};

use axdriver_base::{BaseDriverOps, DevError, DevResult, DeviceType};

use super::{
    osdep::{E1000RegisterIo, pcim2pci_write},
    registers::*,
};
use crate::{EthernetAddress, NetBufPtr, NetDriverOps};

pub const RX_BUFFER_BYTES: usize = 2048;
pub const MAX_FRAME_BYTES: usize = 2048;
const PAGE_BYTES: usize = 4096;
const CTRL_RESET_POLLS: usize = 100_000;
const LINK_POLLS: usize = 100_000;

/// PCI/platform DMA services used by the shared e1000 register and ring logic.
pub trait E1000Hal: Send + Sync {
    fn dma_alloc(pages: usize) -> Option<(u64, NonNull<u8>)>;
    /// # Safety
    /// `bus`, `cpu` and `pages` must name a live allocation returned by `dma_alloc`.
    unsafe fn dma_dealloc(bus: u64, cpu: NonNull<u8>, pages: usize);
    fn delay_us(micros: u32);
}

struct DmaAllocation<H: E1000Hal> {
    bus: u64,
    cpu: NonNull<u8>,
    pages: usize,
    _hal: PhantomData<H>,
}

impl<H: E1000Hal> DmaAllocation<H> {
    fn new(pages: usize) -> DevResult<Self> {
        let (bus, cpu) = H::dma_alloc(pages).ok_or(DevError::NoMemory)?;
        if bus & 0xfff != 0 || cpu.as_ptr() as usize & 0xfff != 0 {
            // SAFETY: allocation ownership is returned immediately.
            unsafe { H::dma_dealloc(bus, cpu, pages) };
            return Err(DevError::InvalidParam);
        }
        // SAFETY: the platform contract promises a writable coherent extent.
        unsafe { ptr::write_bytes(cpu.as_ptr(), 0, pages * PAGE_BYTES) };
        Ok(Self {
            bus,
            cpu,
            pages,
            _hal: PhantomData,
        })
    }
}

impl<H: E1000Hal> Drop for DmaAllocation<H> {
    fn drop(&mut self) {
        // SAFETY: the NIC disables the data paths before this owner is dropped.
        unsafe { H::dma_dealloc(self.bus, self.cpu, self.pages) };
    }
}

struct BufferPool<H: E1000Hal> {
    memory: DmaAllocation<H>,
    slots: usize,
    bytes: usize,
}

impl<H: E1000Hal> BufferPool<H> {
    fn new(slots: usize, bytes: usize) -> DevResult<Self> {
        let total = slots.checked_mul(bytes).ok_or(DevError::InvalidParam)?;
        let pages = total.div_ceil(PAGE_BYTES);
        Ok(Self {
            memory: DmaAllocation::new(pages)?,
            slots,
            bytes,
        })
    }
    fn bus(&self, slot: usize) -> Option<u64> {
        (slot < self.slots).then(|| self.memory.bus + (slot * self.bytes) as u64)
    }
    fn cpu(&self, slot: usize) -> Option<NonNull<u8>> {
        if slot >= self.slots {
            return None;
        }
        // SAFETY: slot is in the allocation's validated bounds.
        NonNull::new(unsafe { self.memory.cpu.as_ptr().add(slot * self.bytes) })
    }
    fn slot_of(&self, pointer: NonNull<u8>) -> Option<usize> {
        let base = self.memory.cpu.as_ptr() as usize;
        let address = pointer.as_ptr() as usize;
        let offset = address.checked_sub(base)?;
        (offset < self.slots * self.bytes && offset % self.bytes == 0)
            .then_some(offset / self.bytes)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RxDescriptor {
    address: u64,
    length: u16,
    checksum: u16,
    status: u8,
    errors: u8,
    special: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct TxDescriptor {
    address: u64,
    length: u16,
    checksum_offset: u8,
    command: u8,
    status: u8,
    checksum_start: u8,
    special: u16,
}

const _: () = assert!(size_of::<RxDescriptor>() == 16);
const _: () = assert!(size_of::<TxDescriptor>() == 16);

/// One e1000-compatible 8254x/e1000e/igb controller with single descriptor queues.
pub struct E1000Nic<H: E1000Hal, const QS: usize = 128> {
    mmio: NonNull<u8>,
    mmio_size: usize,
    mac: [u8; 6],
    advanced_queues: bool,
    pcim2pci: bool,
    rx_desc: DmaAllocation<H>,
    tx_desc: DmaAllocation<H>,
    rx_pool: BufferPool<H>,
    tx_pool: BufferPool<H>,
    rx_free: Vec<usize>,
    tx_free: Vec<usize>,
    rx_owner: Vec<usize>,
    tx_owner: Vec<usize>,
    rx_in_flight: Vec<bool>,
    tx_in_flight: Vec<bool>,
    rx_head: usize,
    rx_tail: usize,
    tx_head: usize,
    tx_tail: usize,
    _hal: PhantomData<H>,
}

// SAFETY: mutable operations are serialized by `&mut self`; shared methods
// only read immutable MAC metadata or volatile device completion state.
unsafe impl<H: E1000Hal, const QS: usize> Send for E1000Nic<H, QS> {}
// SAFETY: descriptor memory is accessed through volatile pointers, and no
// Rust reference aliases memory that the device may update asynchronously.
unsafe impl<H: E1000Hal, const QS: usize> Sync for E1000Nic<H, QS> {}

impl<H: E1000Hal, const QS: usize> E1000Nic<H, QS> {
    /// Adapt the FreeBSD attach order to TheKernel's MMIO, DMA and netdev interfaces.
    // upstream: if_em.c em_attach_pre()
    pub fn new(
        mmio: NonNull<u8>,
        mmio_size: usize,
        advanced_queues: bool,
        pcim2pci: bool,
    ) -> DevResult<Self> {
        if QS < 8 || !QS.is_power_of_two() || QS > u16::MAX as usize || mmio_size < 0x6000 {
            return Err(DevError::InvalidParam);
        }
        reset_hardware::<H>(mmio, mmio_size)?;
        // Reset clears queue state. Enable auto-speed detection and request link.
        let ctrl = reg_read(mmio, mmio_size, E1000_CTRL)?;
        reg_write(
            mmio,
            mmio_size,
            E1000_CTRL,
            ctrl | E1000_CTRL_ASDE | E1000_CTRL_SLU,
        )?;
        let mac = read_station_address(mmio, mmio_size)?;
        let rx_desc =
            DmaAllocation::<H>::new((QS * size_of::<RxDescriptor>()).div_ceil(PAGE_BYTES))?;
        let tx_desc =
            DmaAllocation::<H>::new((QS * size_of::<TxDescriptor>()).div_ceil(PAGE_BYTES))?;
        let rx_pool = BufferPool::<H>::new(QS, RX_BUFFER_BYTES)?;
        let tx_pool = BufferPool::<H>::new(QS, RX_BUFFER_BYTES)?;
        let mut nic = Self {
            mmio,
            mmio_size,
            mac,
            advanced_queues,
            pcim2pci,
            rx_desc,
            tx_desc,
            rx_pool,
            tx_pool,
            rx_free: Vec::new(),
            tx_free: (0..QS).rev().collect(),
            rx_owner: vec![0; QS],
            tx_owner: vec![0; QS],
            rx_in_flight: vec![false; QS],
            tx_in_flight: vec![false; QS],
            rx_head: 0,
            rx_tail: QS - 1,
            tx_head: 0,
            tx_tail: 0,
            _hal: PhantomData,
        };
        for slot in 0..QS {
            let address = nic.rx_pool.bus(slot).ok_or(DevError::BadState)?;
            nic.write_rx_descriptor(slot, address)?;
            nic.rx_owner[slot] = slot;
        }
        nic.initialize_transmit_unit()?;
        nic.initialize_receive_unit()?;
        // Keep the interface available if no cable is attached; link can come up later.
        for _ in 0..LINK_POLLS {
            if nic.read(E1000_STATUS)? & E1000_STATUS_LU != 0 {
                break;
            }
            H::delay_us(10);
        }
        Ok(nic)
    }

    fn read(&self, offset: u32) -> DevResult<u32> {
        if (offset as usize)
            .checked_add(4)
            .is_none_or(|end| end > self.mmio_size)
        {
            return Err(DevError::InvalidParam);
        }
        // SAFETY: the PCI BAR is mapped and the register offset is in bounds.
        Ok(unsafe { ptr::read_volatile(self.mmio.as_ptr().add(offset as usize).cast::<u32>()) })
    }

    fn write(&mut self, offset: u32, value: u32) -> DevResult {
        if self.pcim2pci && (offset == tx_desc_tail(0) || offset == rx_desc_tail(0)) {
            return pcim2pci_write(self, offset, value);
        }
        self.write_raw(offset, value)
    }

    fn write_raw(&mut self, offset: u32, value: u32) -> DevResult {
        if (offset as usize)
            .checked_add(4)
            .is_none_or(|end| end > self.mmio_size)
        {
            return Err(DevError::InvalidParam);
        }
        // SAFETY: the PCI BAR is mapped and the register offset is in bounds.
        unsafe {
            ptr::write_volatile(self.mmio.as_ptr().add(offset as usize).cast::<u32>(), value)
        };
        Ok(())
    }

    fn rx_descriptor(&self, index: usize) -> DevResult<RxDescriptor> {
        if index >= QS {
            return Err(DevError::InvalidParam);
        }
        // SAFETY: descriptor memory is 4 KiB aligned and each element is 16 bytes.
        Ok(unsafe {
            ptr::read_volatile(self.rx_desc.cpu.as_ptr().cast::<RxDescriptor>().add(index))
        })
    }

    // upstream: em_txrx.c em_isc_rxd_refill() descriptor initialization
    fn write_rx_descriptor(&mut self, index: usize, address: u64) -> DevResult {
        if index >= QS {
            return Err(DevError::InvalidParam);
        }
        let descriptor = RxDescriptor {
            address,
            ..RxDescriptor::default()
        };
        // SAFETY: descriptor memory is owned, aligned and this slot is not in use by the NIC.
        unsafe {
            ptr::write_volatile(
                self.rx_desc.cpu.as_ptr().cast::<RxDescriptor>().add(index),
                descriptor,
            )
        };
        Ok(())
    }

    // upstream: em_txrx.c em_isc_txd_encap() one-buffer descriptor path
    fn write_tx_descriptor(&mut self, index: usize, address: u64, length: usize) -> DevResult {
        if index >= QS || length == 0 || length > MAX_FRAME_BYTES {
            return Err(DevError::InvalidParam);
        }
        let descriptor = encode_tx_descriptor(address, length)?;
        // SAFETY: descriptor memory is owned, aligned and this slot is software-owned.
        unsafe {
            ptr::write_volatile(
                self.tx_desc.cpu.as_ptr().cast::<TxDescriptor>().add(index),
                descriptor,
            )
        };
        Ok(())
    }

    fn tx_descriptor(&self, index: usize) -> DevResult<TxDescriptor> {
        if index >= QS {
            return Err(DevError::InvalidParam);
        }
        // SAFETY: descriptor memory is 4 KiB aligned and each element is 16 bytes.
        Ok(unsafe {
            ptr::read_volatile(self.tx_desc.cpu.as_ptr().cast::<TxDescriptor>().add(index))
        })
    }

    // upstream: if_em.c em_initialize_transmit_unit()
    fn initialize_transmit_unit(&mut self) -> DevResult {
        let tx_bus = self.tx_desc.bus;
        self.write(tx_desc_base_low(0), tx_bus as u32)?;
        self.write(tx_desc_base_high(0), (tx_bus >> 32) as u32)?;
        self.write(tx_desc_length(0), (QS * size_of::<TxDescriptor>()) as u32)?;
        self.write(tx_desc_head(0), 0)?;
        self.write(tx_desc_tail(0), 0)?;
        self.write(E1000_TIPG, 0x0060_200a)?;
        self.write(
            E1000_TCTL,
            E1000_TCTL_EN | E1000_TCTL_PSP | 0x0000_0f00 | 0x003f_0000,
        )
    }

    // upstream: if_em.c em_initialize_receive_unit()
    fn initialize_receive_unit(&mut self) -> DevResult {
        let rx_bus = self.rx_desc.bus;
        self.write(rx_desc_base_low(0), rx_bus as u32)?;
        self.write(rx_desc_base_high(0), (rx_bus >> 32) as u32)?;
        self.write(rx_desc_length(0), (QS * size_of::<RxDescriptor>()) as u32)?;
        self.write(rx_desc_head(0), 0)?;
        self.write(rx_desc_tail(0), (QS - 1) as u32)?;
        self.write(
            E1000_RCTL,
            E1000_RCTL_EN | E1000_RCTL_BAM | E1000_RCTL_SECRC,
        )
    }

    fn rx_pool_slot(&self, buffer: &NetBufPtr) -> DevResult<usize> {
        let slot = self
            .rx_pool
            .slot_of(raw_pointer(buffer))
            .ok_or(DevError::BadState)?;
        if !self.rx_in_flight[slot] {
            return Err(DevError::BadState);
        }
        Ok(slot)
    }

    fn tx_pool_slot(&self, buffer: &NetBufPtr) -> DevResult<usize> {
        let slot = self
            .tx_pool
            .slot_of(raw_pointer(buffer))
            .ok_or(DevError::BadState)?;
        if !self.tx_in_flight[slot] {
            return Err(DevError::BadState);
        }
        Ok(slot)
    }

    // upstream: em_txrx.c em_isc_rxd_refill() / em_isc_rxd_flush()
    fn fill_rx(&mut self) -> DevResult {
        while let Some(slot) = self.rx_free.pop() {
            let index = (self.rx_tail + 1) % QS;
            let address = self.rx_pool.bus(slot).ok_or(DevError::BadState)?;
            self.write_rx_descriptor(index, address)?;
            self.rx_owner[index] = slot;
            self.rx_tail = index;
            core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
            self.write(rx_desc_tail(0), index as u32)?;
        }
        Ok(())
    }

    // upstream: em_txrx.c em_isc_txd_credits_update()
    fn reclaim_tx(&mut self) {
        while self.tx_head != self.tx_tail {
            let Ok(descriptor) = self.tx_descriptor(self.tx_head) else {
                break;
            };
            if descriptor.status & E1000_TXD_STAT_DD as u8 == 0 {
                break;
            }
            let slot = self.tx_owner[self.tx_head];
            self.tx_in_flight[slot] = false;
            self.tx_free.push(slot);
            self.tx_head = (self.tx_head + 1) % QS;
        }
    }

    fn buffer_for(pool: &BufferPool<H>, slot: usize, length: usize) -> DevResult<NetBufPtr> {
        let pointer = pool.cpu(slot).ok_or(DevError::BadState)?;
        Ok(NetBufPtr::new(pointer, pointer, length))
    }
}

fn encode_tx_descriptor(address: u64, length: usize) -> DevResult<TxDescriptor> {
    if length == 0 || length > MAX_FRAME_BYTES {
        return Err(DevError::InvalidParam);
    }
    Ok(TxDescriptor {
        address,
        length: length as u16,
        command: ((E1000_TXD_CMD_EOP | E1000_TXD_CMD_IFCS | E1000_TXD_CMD_RS) >> 24) as u8,
        ..TxDescriptor::default()
    })
}

fn raw_pointer(buffer: &NetBufPtr) -> NonNull<u8> {
    buffer.packet_ptr()
}

// upstream: e1000_82540.c e1000_reset_hw_82540()
fn reset_hardware<H: E1000Hal>(mmio: NonNull<u8>, size: usize) -> DevResult {
    reg_write(mmio, size, E1000_IMC, u32::MAX)?;
    reg_write(mmio, size, E1000_RCTL, 0)?;
    reg_write(mmio, size, E1000_TCTL, E1000_TCTL_PSP)?;
    let _flush = reg_read(mmio, size, E1000_STATUS)?;
    H::delay_us(10_000);
    let ctrl = reg_read(mmio, size, E1000_CTRL)?;
    reg_write(mmio, size, E1000_CTRL, ctrl | E1000_CTRL_RST)?;
    H::delay_us(5_000);
    let mut reset_done = false;
    for _ in 0..CTRL_RESET_POLLS {
        if reg_read(mmio, size, E1000_CTRL)? & E1000_CTRL_RST == 0 {
            reset_done = true;
            break;
        }
        H::delay_us(10);
    }
    if !reset_done {
        return Err(DevError::Again);
    }
    let manc = reg_read(mmio, size, E1000_MANC)? & !E1000_MANC_ARP_EN;
    reg_write(mmio, size, E1000_MANC, manc)?;
    reg_write(mmio, size, E1000_IMC, u32::MAX)?;
    let _clear_interrupts = reg_read(mmio, size, E1000_ICR)?;
    Ok(())
}

// upstream: e1000_nvm.c e1000_read_mac_addr_generic()
fn read_station_address(mmio: NonNull<u8>, size: usize) -> DevResult<[u8; 6]> {
    let low = reg_read(mmio, size, E1000_RA)?;
    let high = reg_read(mmio, size, E1000_RA + 4)?;
    if high & E1000_RAH_AV == 0 {
        return Err(DevError::BadState);
    }
    let address = [
        low as u8,
        (low >> 8) as u8,
        (low >> 16) as u8,
        (low >> 24) as u8,
        high as u8,
        (high >> 8) as u8,
    ];
    if address == [0; 6] || address[0] & 1 != 0 {
        return Err(DevError::BadState);
    }
    Ok(address)
}

fn reg_read(mmio: NonNull<u8>, size: usize, offset: u32) -> DevResult<u32> {
    let offset = offset as usize;
    if offset.checked_add(4).is_none_or(|end| end > size) {
        return Err(DevError::InvalidParam);
    }
    // SAFETY: caller supplies an active, mapped PCI BAR and the register is in bounds.
    Ok(unsafe { ptr::read_volatile(mmio.as_ptr().add(offset).cast::<u32>()) })
}

fn reg_write(mmio: NonNull<u8>, size: usize, offset: u32, value: u32) -> DevResult {
    let offset = offset as usize;
    if offset.checked_add(4).is_none_or(|end| end > size) {
        return Err(DevError::InvalidParam);
    }
    // SAFETY: caller supplies an active, mapped PCI BAR and the register is in bounds.
    unsafe { ptr::write_volatile(mmio.as_ptr().add(offset).cast::<u32>(), value) };
    Ok(())
}

impl<H: E1000Hal, const QS: usize> Drop for E1000Nic<H, QS> {
    fn drop(&mut self) {
        let _ = self.write(E1000_RCTL, 0);
        let _ = self.write(E1000_TCTL, 0);
        if let Ok(ctrl) = self.read(E1000_CTRL) {
            let _ = self.write(E1000_CTRL, ctrl | E1000_CTRL_RST);
        }
        for _ in 0..CTRL_RESET_POLLS {
            if self
                .read(E1000_CTRL)
                .is_ok_and(|ctrl| ctrl & E1000_CTRL_RST == 0)
            {
                break;
            }
            H::delay_us(10);
        }
    }
}

impl<H: E1000Hal, const QS: usize> BaseDriverOps for E1000Nic<H, QS> {
    fn device_name(&self) -> &str {
        "e1000"
    }
    fn device_type(&self) -> DeviceType {
        DeviceType::Net
    }
}

impl<H: E1000Hal, const QS: usize> NetDriverOps for E1000Nic<H, QS> {
    fn mac_address(&self) -> EthernetAddress {
        EthernetAddress(self.mac)
    }
    fn can_transmit(&self) -> bool {
        let next = (self.tx_tail + 1) % QS;
        next != self.tx_head && !self.tx_free.is_empty()
    }
    fn can_receive(&self) -> bool {
        self.rx_descriptor(self.rx_head)
            .is_ok_and(|desc| desc.status & E1000_RXD_STAT_DD as u8 != 0)
    }
    fn rx_queue_size(&self) -> usize {
        QS
    }
    fn tx_queue_size(&self) -> usize {
        QS
    }
    fn recycle_rx_buffer(&mut self, rx_buf: NetBufPtr) -> DevResult {
        let slot = self.rx_pool_slot(&rx_buf)?;
        self.rx_in_flight[slot] = false;
        self.rx_free.push(slot);
        self.fill_rx()
    }
    fn recycle_tx_buffers(&mut self) -> DevResult {
        self.reclaim_tx();
        Ok(())
    }
    // upstream: em_txrx.c em_isc_txd_encap() / em_isc_txd_flush()
    fn transmit(&mut self, tx_buf: NetBufPtr) -> DevResult {
        self.reclaim_tx();
        let slot = self.tx_pool_slot(&tx_buf)?;
        let length = tx_buf.packet_len();
        if length == 0 || length > MAX_FRAME_BYTES {
            return Err(DevError::InvalidParam);
        }
        let next = (self.tx_tail + 1) % QS;
        if next == self.tx_head {
            return Err(DevError::Again);
        }
        self.write_tx_descriptor(
            self.tx_tail,
            self.tx_pool.bus(slot).ok_or(DevError::BadState)?,
            length,
        )?;
        self.tx_owner[self.tx_tail] = slot;
        core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
        self.tx_tail = next;
        self.write(tx_desc_tail(0), next as u32)
    }
    // upstream: em_txrx.c em_isc_rxd_pkt_get() single-fragment path
    fn receive(&mut self) -> DevResult<NetBufPtr> {
        let descriptor = self.rx_descriptor(self.rx_head)?;
        if descriptor.status & E1000_RXD_STAT_DD as u8 == 0 {
            return Err(DevError::Again);
        }
        let index = self.rx_head;
        self.rx_head = (self.rx_head + 1) % QS;
        let slot = self.rx_owner[index];
        if descriptor.errors != 0
            || descriptor.status & E1000_RXD_STAT_EOP as u8 == 0
            || descriptor.length == 0
            || descriptor.length as usize > MAX_FRAME_BYTES
        {
            self.rx_free.push(slot);
            self.fill_rx()?;
            return Err(DevError::Io);
        }
        self.rx_in_flight[slot] = true;
        Self::buffer_for(&self.rx_pool, slot, descriptor.length as usize)
    }
    fn alloc_tx_buffer(&mut self, size: usize) -> DevResult<NetBufPtr> {
        if size == 0 || size > MAX_FRAME_BYTES {
            return Err(DevError::InvalidParam);
        }
        self.reclaim_tx();
        let slot = self.tx_free.pop().ok_or(DevError::Again)?;
        self.tx_in_flight[slot] = true;
        Self::buffer_for(&self.tx_pool, slot, size)
    }
}

impl<H: E1000Hal, const QS: usize> E1000RegisterIo for E1000Nic<H, QS> {
    fn read_register(&mut self, register: u32) -> DevResult<u32> {
        self.read(register)
    }

    fn write_register(&mut self, register: u32, value: u32) -> DevResult {
        self.write_raw(register, value)
    }

    fn delay_us(&mut self, micros: u32) {
        H::delay_us(micros);
    }

    fn invalid_tail_write(&mut self, direction: &'static str) {
        log::error!("e1000: invalid {direction} tail write; queue disabled");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_tx_descriptor_encodes_address_length_and_completion_request() {
        let descriptor = encode_tx_descriptor(0x1234_5678_9abc_def0, 1514).unwrap();
        assert_eq!(descriptor.address, 0x1234_5678_9abc_def0);
        assert_eq!(descriptor.length, 1514);
        assert_eq!(
            descriptor.command,
            ((E1000_TXD_CMD_EOP | E1000_TXD_CMD_IFCS | E1000_TXD_CMD_RS) >> 24) as u8
        );
        assert!(matches!(
            encode_tx_descriptor(0, 0),
            Err(DevError::InvalidParam)
        ));
        assert!(matches!(
            encode_tx_descriptor(0, MAX_FRAME_BYTES + 1),
            Err(DevError::InvalidParam)
        ));
    }
}
