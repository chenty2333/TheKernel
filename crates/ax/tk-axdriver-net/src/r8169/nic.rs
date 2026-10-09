//! Device-owned DMA storage and checked buffer loans. See the design note for
//! warm-PHY assumptions and why an unconfirmed reset retains the allocation.
use core::{marker::PhantomData, mem::ManuallyDrop, ptr::NonNull};

use super::{
    Hal, bringup,
    desc::{self as d, Descriptor},
    ids::Chip,
    probe,
    regs::{
        self as r, Bus,
        Width::{Byte, Word},
    },
};
use crate::{
    BaseDriverOps, DevError, DevResult, DeviceType, EthernetAddress, NetBufPtr, NetDriverOps,
};
const PAGE: usize = 4096;
const FREE: u8 = 0;
const CALLER: u8 = 1;
const DMA: u8 = 2;
struct Allocation<H: Hal> {
    address: u64,
    pointer: NonNull<u8>,
    pages: usize,
    hal: PhantomData<H>,
}
impl<H: Hal> Allocation<H> {
    fn new(bytes: usize) -> DevResult<Self> {
        let pages = bytes.div_ceil(PAGE);
        let (address, pointer) = H::allocate(pages).ok_or(DevError::NoMemory)?;
        let allocation = Self {
            address,
            pointer,
            pages,
            hal: PhantomData,
        };
        if address & (PAGE as u64 - 1) != 0
            || pointer.as_ptr() as usize & (PAGE - 1) != 0
            || address.checked_add((pages * PAGE) as u64).is_none()
        {
            return Err(DevError::BadState);
        }
        // SAFETY: fresh allocation, not yet reachable by DMA or borrowers.
        unsafe {
            pointer.as_ptr().write_bytes(0, pages * PAGE);
        }
        Ok(allocation)
    }
    fn slot(&self, index: usize) -> NonNull<u8> {
        // SAFETY: caller bounds index to its ring size and the allocation holds
        // one BUFFER-byte slot per descriptor.
        unsafe { NonNull::new_unchecked(self.pointer.as_ptr().add(index * d::BUFFER)) }
    }
    fn index(&self, buffer: &NetBufPtr, count: usize) -> Option<usize> {
        let pointer = buffer.raw_ptr::<u8>() as usize;
        let offset = pointer.checked_sub(self.pointer.as_ptr() as usize)?;
        if !offset.is_multiple_of(d::BUFFER)
            || offset / d::BUFFER >= count
            || buffer.packet_ptr().as_ptr() as usize != pointer
        {
            return None;
        }
        Some(offset / d::BUFFER)
    }
}
impl<H: Hal> Drop for Allocation<H> {
    fn drop(&mut self) {
        unsafe {
            H::deallocate(self.address, self.pointer, self.pages);
        }
    }
}

pub struct RtlNic<H: Hal, B: Bus, const N: usize> {
    bus: B,
    mac: [u8; 6],
    chip: Chip,
    tx_desc: ManuallyDrop<Allocation<H>>,
    rx_desc: ManuallyDrop<Allocation<H>>,
    tx_data: ManuallyDrop<Allocation<H>>,
    rx_data: ManuallyDrop<Allocation<H>>,
    tx_lengths: [usize; N],
    tx_loan: [u8; N],
    rx_loan: [bool; N],
    tx_slots: [usize; N],
    tx_head: usize,
    tx_tail: usize,
    tx_used: usize,
    rx_head: usize,
    counters: super::health::Counters,
    monitor: super::health::Monitor,
    firmware_stage: &'static str,
    // A failed firmware operation after MAC reset must not leave the stack
    // publishing fresh DMA descriptors to an uninitialized device. This is
    // software admission, not a claim that hardware DMA has stopped.
    data_path_ready: bool,
}
// DMA pointers denote exclusive allocations; &mut self serializes CPU access.
unsafe impl<H: Hal, B: Bus, const N: usize> Send for RtlNic<H, B, N> {}
unsafe impl<H: Hal, B: Bus + Sync, const N: usize> Sync for RtlNic<H, B, N> {}
impl<H: Hal, B: Bus, const N: usize> RtlNic<H, B, N> {
    pub fn new(mut bus: B) -> DevResult<Self> {
        if N < 2 || N > 256 || !N.is_power_of_two() {
            return Err(DevError::InvalidParam);
        }
        let (chip, mac) = probe::identify(&mut bus)?;
        // No published addresses until all four allocations have succeeded.
        let tx_desc = Allocation::new(N * size_of::<Descriptor>())?;
        let rx_desc = Allocation::new(N * size_of::<Descriptor>())?;
        let tx_data = Allocation::new(N * d::BUFFER)?;
        let rx_data = Allocation::new(N * d::BUFFER)?;
        let mut nic = Self {
            bus,
            mac,
            chip,
            tx_desc: ManuallyDrop::new(tx_desc),
            rx_desc: ManuallyDrop::new(rx_desc),
            tx_data: ManuallyDrop::new(tx_data),
            rx_data: ManuallyDrop::new(rx_data),
            tx_lengths: [0; N],
            tx_loan: [FREE; N],
            rx_loan: [false; N],
            tx_slots: [0; N],
            tx_head: 0,
            tx_tail: 0,
            tx_used: 0,
            rx_head: 0,
            counters: super::health::Counters::default(),
            monitor: super::health::Monitor::default(),
            firmware_stage: "warm-PXE-no-firmware-attempt",
            data_path_ready: false,
        };
        bringup::reset(&mut nic.bus, chip)?;
        for index in 0..N {
            nic.arm_rx(index);
        }
        bringup::program(&mut nic.bus, chip, nic.tx_desc.address, nic.rx_desc.address)?;
        nic.data_path_ready = true;
        Ok(nic)
    }
    /// Read-only status snapshot; PHY OCP reads do not write PHY values and
    /// IntrStatus is not acknowledged/cleared here. No recovery/reset is done.
    pub fn snapshot(&mut self) -> super::health::Snapshot {
        let (mask, status, width) = self.chip.irq();
        let command = self.bus.read(r::COMMAND, Byte);
        let intr_status = self.bus.read(status, width);
        let intr_mask = self.bus.read(mask, width);
        let phy_status = self.bus.read(r::PHY_STATUS, Byte);
        let (bmcr, bmsr) = if self.chip == Chip::Rtl8168H {
            let bmcr = super::indirect::phy_read(&mut self.bus, 0xa400).ok();
            let _ = super::indirect::phy_read(&mut self.bus, 0xa402); // latched-low link observation
            (bmcr, super::indirect::phy_read(&mut self.bus, 0xa402).ok())
        } else {
            (None, None)
        };
        super::health::Snapshot {
            command,
            intr_status,
            intr_mask,
            phy_status,
            bmcr,
            bmsr,
            tx_head: self.tx_head,
            tx_tail: self.tx_tail,
            tx_used: self.tx_used,
            rx_head: self.rx_head,
            tx_head_status: unsafe { d::status(self.descriptor(false, self.tx_head)) },
            rx_head_status: unsafe { d::status(self.descriptor(true, self.rx_head)) },
            rx_loans: self.rx_loan.iter().filter(|v| **v).count(),
            counters: self.counters,
            firmware_stage: self.firmware_stage,
        }
    }
    fn diagnostic_tick(&mut self) {
        if self.chip != Chip::Rtl8168H {
            return;
        }
        let Some(now) = self.bus.now_millis() else {
            return;
        };
        let owned = self.tx_used != 0
            && unsafe { d::status(self.descriptor(false, self.tx_head)) } & d::OWN != 0;
        if let Some(reason) = self.monitor.observe(now, self.counters, owned) {
            let state = self.snapshot();
            log::warn!(
                "\x013RTL8168_HEALTH reason={} ms={} ChipCmd={:#04x} IntrStatus={:#06x} \
                 IntrMask={:#06x} PHYstatus={:#04x} BMCR={:?} BMSR={:?} tx_head={} tx_tail={} \
                 tx_used={} tx_desc={:#010x} rx_head={} rx_desc={:#010x} rx_loans={} tx={} \
                 tx_reaped={} rx={} rx_bad={} fw_stage={} (observation, no reset)",
                reason.as_str(),
                now,
                state.command,
                state.intr_status,
                state.intr_mask,
                state.phy_status,
                state.bmcr,
                state.bmsr,
                state.tx_head,
                state.tx_tail,
                state.tx_used,
                state.tx_head_status,
                state.rx_head,
                state.rx_head_status,
                state.rx_loans,
                state.counters.tx,
                state.counters.tx_reaped,
                state.counters.rx,
                state.counters.rx_bad,
                state.firmware_stage
            );
        }
    }
    pub fn link_up(&mut self) -> bool {
        self.bus.read(r::PHY_STATUS, Byte) & r::LINK != 0
    }
    fn descriptor(&self, rx: bool, index: usize) -> *mut Descriptor {
        let allocation = if rx { &self.rx_desc } else { &self.tx_desc };
        // SAFETY: all ring users bound index < N; allocation holds N entries.
        unsafe { allocation.pointer.as_ptr().cast::<Descriptor>().add(index) }
    }
    fn arm_rx(&mut self, index: usize) {
        let end = if index == N - 1 { d::END } else { 0 };
        unsafe {
            d::publish(
                self.descriptor(true, index),
                self.rx_data.address + (index * d::BUFFER) as u64,
                d::OWN | end | d::BUFFER as u32,
            );
        }
    }
}
impl<H: Hal, B: Bus, const N: usize> Drop for RtlNic<H, B, N> {
    fn drop(&mut self) {
        if bringup::reset(&mut self.bus, self.chip).is_err()
            || self.rx_loan.contains(&true)
            || self.tx_loan.contains(&CALLER)
        {
            log::warn!("r8169: DMA stop or packet-loan release unconfirmed; retaining DMA memory");
            return;
        }
        // SAFETY: reset completed, so no DMA can access these allocations;
        // all CPU packet loans were returned. They are dropped exactly once.
        unsafe {
            ManuallyDrop::drop(&mut self.tx_desc);
            ManuallyDrop::drop(&mut self.rx_desc);
            ManuallyDrop::drop(&mut self.tx_data);
            ManuallyDrop::drop(&mut self.rx_data);
        }
    }
}
impl<H: Hal, B: Bus, const N: usize> BaseDriverOps for RtlNic<H, B, N> {
    fn irq_num(&self) -> Option<usize> {
        self.bus.irq_num()
    }
    fn device_name(&self) -> &str {
        self.chip.name()
    }
    fn device_type(&self) -> DeviceType {
        DeviceType::Net
    }
}
impl<H: Hal, B: Bus, const N: usize> NetDriverOps for RtlNic<H, B, N> {
    fn rx_poll_interval_micros(&self) -> Option<u64> {
        Some(10_000)
    }
    fn firmware_path(&self) -> Option<&'static str> {
        (self.chip == Chip::Rtl8168H).then_some("/lib/firmware/rtl_nic/rtl8168h-2.fw")
    }
    fn load_firmware(&mut self, bytes: &[u8]) -> DevResult {
        if self.chip != Chip::Rtl8168H {
            return Err(DevError::Unsupported);
        }
        self.firmware_stage = "firmware-parse";
        let result = (|| {
            let firmware =
                super::health::stage(self.firmware_stage, super::firmware::Firmware::parse(bytes))?;
            self.firmware_stage = "firmware-quiescence";
            if self.tx_used != 0 || self.rx_loan.contains(&true) || self.tx_loan.contains(&CALLER) {
                return super::health::stage(self.firmware_stage, Err(DevError::ResourceBusy));
            }
            self.firmware_stage = "firmware-MAC-reset";
            self.data_path_ready = false;
            super::health::stage(
                self.firmware_stage,
                bringup::reset(&mut self.bus, self.chip),
            )?;
            self.firmware_stage = "firmware-execute";
            super::health::stage(self.firmware_stage, firmware.execute(&mut self.bus))?;
            self.firmware_stage = "PHY-calibration-autoneg";
            super::health::stage(
                self.firmware_stage,
                super::h8168::configure_phy(&mut self.bus),
            )?;
            self.firmware_stage = "firmware-ring-rearm";
            for index in 0..N {
                self.arm_rx(index);
                // MAC reset restarts descriptor fetch at slot zero. Every
                // loan was checked quiescent before reset; do not retain a
                // software TX cursor pointing past the hardware cursor.
                // SAFETY: reset succeeded, TX borrowers are absent, and the
                // descriptor remains CPU-owned (OWN is clear).
                unsafe {
                    d::publish(
                        self.descriptor(false, index),
                        0,
                        if index == N - 1 { d::END } else { 0 },
                    );
                }
            }
            self.tx_lengths = [0; N];
            self.tx_loan = [FREE; N];
            self.tx_slots = [0; N];
            self.tx_head = 0;
            self.tx_tail = 0;
            self.tx_used = 0;
            self.rx_head = 0;
            self.firmware_stage = "firmware-MAC-program";
            super::health::stage(
                self.firmware_stage,
                bringup::program(
                    &mut self.bus,
                    self.chip,
                    self.tx_desc.address,
                    self.rx_desc.address,
                ),
            )?;
            self.firmware_stage = "ready-after-firmware";
            self.data_path_ready = true;
            Ok(())
        })();
        let snapshot = self.snapshot();
        if result.is_err() {
            log::warn!(
                "\x013RTL8168_FIRMWARE_FAILED stage={} data_path_ready={} state={snapshot:?}; no \
                 automatic recovery",
                self.firmware_stage,
                self.data_path_ready
            );
        } else {
            log::info!(
                "RTL8168_FIRMWARE_READY state={snapshot:?}; verify link/DHCP, not just this log"
            );
        }
        result
    }

    fn mac_address(&self) -> EthernetAddress {
        EthernetAddress(self.mac)
    }
    fn can_transmit(&self) -> bool {
        self.data_path_ready && self.tx_used < N - 1 && self.tx_loan.contains(&FREE)
    }
    fn can_receive(&self) -> bool {
        self.data_path_ready
            && !self.rx_loan[self.rx_head]
            && unsafe { d::status(self.descriptor(true, self.rx_head)) } & d::OWN == 0
    }
    fn rx_queue_size(&self) -> usize {
        N
    }
    fn tx_queue_size(&self) -> usize {
        N - 1
    }
    fn alloc_tx_buffer(&mut self, size: usize) -> DevResult<NetBufPtr> {
        if !self.data_path_ready {
            return Err(DevError::BadState);
        }
        if size == 0 || size > d::MAX_FRAME {
            return Err(DevError::InvalidParam);
        }
        let _ = self.recycle_tx_buffers();
        let index = self
            .tx_loan
            .iter()
            .position(|state| *state == FREE)
            .ok_or(DevError::Again)?;
        self.tx_loan[index] = CALLER;
        self.tx_lengths[index] = size;
        let pointer = self.tx_data.slot(index);
        // Zero padding before the stack writes its requested bytes; short
        // Ethernet frames must not disclose a previous packet's tail.
        unsafe {
            pointer.as_ptr().write_bytes(0, 60.max(size));
        }
        Ok(NetBufPtr::new(pointer, pointer, size))
    }
    fn transmit(&mut self, buffer: NetBufPtr) -> DevResult {
        if !self.data_path_ready {
            return Err(DevError::BadState);
        }
        let index = self.tx_data.index(&buffer, N).ok_or(DevError::BadState)?;
        if buffer.packet_len() != self.tx_lengths[index]
            || self.tx_loan[index] != CALLER
            || buffer.packet_len() == 0
            || buffer.packet_len() > d::MAX_FRAME
        {
            return Err(DevError::BadState);
        }
        self.recycle_tx_buffers()?;
        if self.tx_used == N - 1 {
            return Err(DevError::Again);
        }
        let slot = self.tx_tail;
        let end = if slot == N - 1 { d::END } else { 0 };
        self.tx_loan[index] = DMA;
        self.tx_slots[slot] = index;
        unsafe {
            d::publish(
                self.descriptor(false, slot),
                self.tx_data.address + (index * d::BUFFER) as u64,
                d::OWN | d::FIRST | d::LAST | end | buffer.packet_len().max(60) as u32,
            );
        }
        self.tx_tail = (slot + 1) % N;
        self.tx_used += 1;
        self.counters.tx = self.counters.tx.saturating_add(1);
        match self.chip {
            Chip::Rtl8125B => self.bus.write(r::TX_POLL, Word, 1),
            Chip::Rtl8168H => self.bus.write(0x38, Byte, 1 << 6),
        }
        Ok(())
    }
    fn recycle_tx_buffers(&mut self) -> DevResult {
        while self.tx_used > 0 {
            if unsafe { d::status(self.descriptor(false, self.tx_head)) } & d::OWN != 0 {
                break;
            }
            self.tx_loan[self.tx_slots[self.tx_head]] = FREE;
            self.tx_head = (self.tx_head + 1) % N;
            self.tx_used -= 1;
            self.counters.tx_reaped = self.counters.tx_reaped.saturating_add(1);
        }
        Ok(())
    }
    fn receive(&mut self) -> DevResult<NetBufPtr> {
        if !self.data_path_ready {
            return Err(DevError::BadState);
        }
        self.diagnostic_tick();
        for _ in 0..N {
            let index = self.rx_head;
            if self.rx_loan[index] {
                return Err(DevError::Again);
            }
            let status = unsafe { d::status(self.descriptor(true, index)) };
            if status & d::OWN != 0 {
                return Err(DevError::Again);
            }
            self.rx_head = (index + 1) % N;
            let Some(length) = d::receive_length(status) else {
                self.counters.rx_bad = self.counters.rx_bad.saturating_add(1);
                self.arm_rx(index);
                continue;
            };
            self.rx_loan[index] = true;
            self.counters.rx = self.counters.rx.saturating_add(1);
            let pointer = self.rx_data.slot(index);
            return Ok(NetBufPtr::new(pointer, pointer, length));
        }
        Err(DevError::Again)
    }
    fn recycle_rx_buffer(&mut self, buffer: NetBufPtr) -> DevResult {
        let index = self.rx_data.index(&buffer, N).ok_or(DevError::BadState)?;
        if !self.rx_loan[index] {
            return Err(DevError::BadState);
        }
        self.rx_loan[index] = false;
        self.arm_rx(index);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        super::fake::{FakeBus, FakeHal},
        *,
    };
    #[test]
    fn health_snapshot_preserves_irq_status_mac_and_descriptor_ownership() {
        let mut nic = RtlNic::<FakeHal, _, 4>::new(FakeBus::h8168()).unwrap();
        nic.bus.set_register(0x3e, 0x0027);
        nic.bus.set_register(0x6c, 0xc3);
        let prior = nic.bus.writes.len();
        let state = nic.snapshot();
        assert_eq!(state.intr_status, 0x0027);
        assert_eq!(state.command, r::RX_TX_ENABLE);
        assert_eq!(state.phy_status, 0xc3);
        assert_eq!(state.rx_head, 0);
        assert_ne!(state.rx_head_status & d::OWN, 0);
        assert_eq!(state.tx_used, 0);
        assert!(
            nic.bus.writes[prior..]
                .iter()
                .all(|&(port, _, value)| port == 0xb8 && value & (1 << 31) == 0)
        );
        assert_eq!(nic.snapshot().intr_status, 0x0027);
        assert!(nic.load_firmware(&[1]).is_err());
        assert_eq!(nic.firmware_stage, "firmware-parse");
        assert_eq!(nic.snapshot().command, r::RX_TX_ENABLE); // bad header did not pretend to reset/recover
    }
    fn minimal_firmware() -> std::vec::Vec<u8> {
        [0x801f_0a43u32, 0x8010_0055]
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect()
    }

    #[test]
    fn firmware_parse_and_busy_failures_preserve_the_running_warm_path() {
        let mut nic = RtlNic::<FakeHal, _, 4>::new(FakeBus::h8168()).unwrap();
        assert!(nic.can_transmit());
        assert!(nic.load_firmware(&[1]).is_err());
        assert!(nic.can_transmit());
        let packet = nic.alloc_tx_buffer(42).unwrap();
        assert!(matches!(
            nic.load_firmware(&minimal_firmware()),
            Err(DevError::ResourceBusy)
        ));
        assert!(nic.can_transmit());
        nic.transmit(packet).unwrap();
    }

    #[test]
    fn firmware_failures_after_reset_fence_new_dma_until_explicit_success() {
        for stuck_reset in [false, true] {
            let mut nic = RtlNic::<FakeHal, _, 4>::new(FakeBus::h8168()).unwrap();
            nic.bus.stuck_reset = stuck_reset;
            nic.bus.stuck_indirect = !stuck_reset;
            assert!(nic.load_firmware(&minimal_firmware()).is_err());
            assert!(!nic.can_transmit());
            assert!(!nic.can_receive());
            let writes = nic.bus.writes.len();
            assert!(matches!(nic.alloc_tx_buffer(42), Err(DevError::BadState)));
            assert!(matches!(nic.receive(), Err(DevError::BadState)));
            assert_eq!(nic.bus.writes.len(), writes);
            assert_eq!(nic.tx_used, 0);
            nic.bus.stuck_reset = false;
            nic.bus.stuck_indirect = false;
            nic.load_firmware(&minimal_firmware()).unwrap();
            assert!(nic.can_transmit());
            assert_eq!(nic.firmware_stage, "ready-after-firmware");
        }
    }

    #[test]
    fn firmware_reset_restarts_both_hardware_and_software_tx_at_slot_zero() {
        let mut nic = RtlNic::<FakeHal, _, 4>::new(FakeBus::h8168()).unwrap();
        let packet = nic.alloc_tx_buffer(42).unwrap();
        nic.transmit(packet).unwrap();
        // SAFETY: fake hardware completes the sole outstanding descriptor.
        unsafe {
            (*nic.descriptor(false, 0)).options &= !d::OWN;
        }
        nic.recycle_tx_buffers().unwrap();
        assert_eq!((nic.tx_head, nic.tx_tail, nic.tx_used), (1, 1, 0));
        nic.load_firmware(&minimal_firmware()).unwrap();
        assert_eq!((nic.tx_head, nic.tx_tail, nic.tx_used), (0, 0, 0));
        // SAFETY: the fake NIC owns the live four-entry ring with no device DMA.
        let statuses = unsafe {
            (
                d::status(nic.descriptor(false, 0)),
                d::status(nic.descriptor(false, 3)),
            )
        };
        assert_eq!(statuses, (0, d::END));
        let packet = nic.alloc_tx_buffer(42).unwrap();
        nic.transmit(packet).unwrap();
        // SAFETY: descriptor zero is live and only this fake NIC accesses it.
        let status = unsafe { d::status(nic.descriptor(false, 0)) };
        assert_ne!(status & d::OWN, 0);
        assert_eq!((nic.tx_head, nic.tx_tail, nic.tx_used), (0, 1, 1));
    }

    #[test]
    fn failed_allocations_release_unpublished_prefix_but_failed_reset_retains_dma() {
        let (a, f) = FakeHal::counts();
        FakeHal::fail_after(2);
        assert!(matches!(
            RtlNic::<FakeHal, _, 4>::new(FakeBus::new()),
            Err(DevError::NoMemory)
        ));
        assert_eq!(FakeHal::counts(), (a + 2, f + 2));
        let (a, f) = FakeHal::counts();
        let mut bus = FakeBus::new();
        bus.stuck_reset = true;
        assert!(matches!(
            RtlNic::<FakeHal, _, 4>::new(bus),
            Err(DevError::Io)
        ));
        assert_eq!(FakeHal::counts(), (a + 4, f));
        let (a, f) = FakeHal::counts();
        drop(RtlNic::<FakeHal, _, 4>::new(FakeBus::new()).unwrap());
        assert_eq!(FakeHal::counts(), (a + 4, f + 4));
    }

    #[test]
    fn rings_recycle_wrap_and_refuse_duplicate_loans() {
        let mut nic = RtlNic::<FakeHal, _, 4>::new(FakeBus::new()).unwrap();
        for _ in 0..10 {
            let buffer = nic.alloc_tx_buffer(42).unwrap();
            let pointer = NonNull::new(buffer.raw_ptr::<u8>()).unwrap();
            let duplicate = NetBufPtr::new(pointer, pointer, 42);
            let slot = nic.tx_tail;
            nic.transmit(buffer).unwrap();
            assert!(matches!(nic.transmit(duplicate), Err(DevError::BadState)));
            let descriptor = nic.descriptor(false, slot);
            assert_eq!(unsafe { (*descriptor).options } & 0x3fff, 60);
            unsafe {
                (*descriptor).options &= !d::OWN;
            }
            nic.recycle_tx_buffers().unwrap();
            assert_eq!(nic.tx_used, 0);
        }
        let index = nic.rx_head;
        unsafe {
            (*nic.descriptor(true, index)).options = d::FIRST | d::LAST | 64;
        }
        let buffer = nic.receive().unwrap();
        assert_eq!(buffer.packet_len(), 60);
        let pointer = NonNull::new(buffer.raw_ptr::<u8>()).unwrap();
        nic.recycle_rx_buffer(buffer).unwrap();
        assert!(matches!(
            nic.recycle_rx_buffer(NetBufPtr::new(pointer, pointer, 60)),
            Err(DevError::BadState)
        ));
        assert_ne!(
            unsafe { (*nic.descriptor(true, index)).options } & d::OWN,
            0
        );
    }
    #[test]
    fn h8168_packets_use_the_shared_ring_and_byte_doorbell() {
        let mut nic = RtlNic::<FakeHal, _, 4>::new(FakeBus::h8168()).unwrap();
        assert_eq!(nic.device_name(), "rtl8168");
        for _ in 0..12 {
            let packet = nic.alloc_tx_buffer(42).unwrap();
            unsafe {
                packet.packet_ptr().as_ptr().write_bytes(0xa5, 42);
            }
            let slot = nic.tx_tail;
            nic.transmit(packet).unwrap();
            assert_eq!(nic.bus.writes.last(), Some(&(0x38, Byte, 0x40)));
            unsafe {
                let data = (*nic.descriptor(false, slot)).address as *const u8;
                assert_eq!(core::slice::from_raw_parts(data, 42), &[0xa5; 42]);
                assert_eq!(core::slice::from_raw_parts(data.add(42), 18), &[0; 18]);
                (*nic.descriptor(false, slot)).options &= !d::OWN;
            }
            nic.recycle_tx_buffers().unwrap();
            let rx = nic.rx_head;
            unsafe {
                nic.rx_data.slot(rx).as_ptr().write_bytes(0x5a, 60);
                (*nic.descriptor(true, rx)).options = d::FIRST | d::LAST | 64;
            }
            let packet = nic.receive().unwrap();
            assert_eq!(packet.packet_len(), 60);
            assert_eq!(
                unsafe { core::slice::from_raw_parts(packet.packet_ptr().as_ptr(), 60) },
                &[0x5a; 60]
            );
            nic.recycle_rx_buffer(packet).unwrap();
        }
    }
    #[test]
    fn malformed_receive_is_rearmed_and_ring_full_is_bounded() {
        let mut nic = RtlNic::<FakeHal, _, 4>::new(FakeBus::new()).unwrap();
        unsafe {
            (*nic.descriptor(true, 0)).options = d::ERROR | d::FIRST | d::LAST | 64;
        }
        assert!(matches!(nic.receive(), Err(DevError::Again)));
        assert_ne!(unsafe { (*nic.descriptor(true, 0)).options } & d::OWN, 0);
        for _ in 0..3 {
            let buffer = nic.alloc_tx_buffer(100).unwrap();
            nic.transmit(buffer).unwrap();
        }
        assert!(!nic.can_transmit());
        assert_eq!(nic.tx_used, 3);
    }
}
