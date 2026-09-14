//! The NIC: two descriptor rings, and the `NetDriverOps` implementation.
//!
//! This is the phase that makes the device usable by the network stack.  It
//! configures one transmit and one receive ring the way
//! `igc_configure_tx_ring` / `igc_configure_rx_ring` / `igc_configure` do
//! (`igc_main.c:625`, `:728`, `:4020`), fills the receive ring, and implements
//! the buffer handover the stack expects: the stack asks for a transmit buffer,
//! writes a frame into it, hands it back, and later hands receive buffers back
//! for the hardware to use again.
//!
//! # What is deliberately not here
//!
//! * **No interrupts.**  This driver polls.  The bring-up phase masked every
//!   interrupt source and nothing unmasks one, so [`NetDriverOps::can_receive`]
//!   and [`NetDriverOps::receive`] mean exactly what they say: a descriptor has
//!   been written back, or it has not yet.
//! * **No segmentation, no checksum offload, no VLAN insertion, no
//!   timestamping.**  A frame goes out exactly as the stack wrote it, with the
//!   MAC appending the Ethernet CRC because the descriptor asks for it
//!   (`DCMD_IFCS`).  On receive the CRC is stripped by the hardware
//!   (`RCTL.SECRC`), so the descriptor's length is the frame length and the
//!   driver does not adjust it.
//! * **One queue in each direction**, which is what `NetDriverOps` describes.
//!   The vendor driver runs one to four.
//! * **No flow control, no receive-filter programming, no RSS.**  The design
//!   note lists these with their consequences.
//!
//! # The invariant that matters
//!
//! Every buffer the driver hands to the network stack is remembered as *in
//! flight*, and a buffer can only come back once.  A pointer that is not from
//! this driver's own pool, or that names a buffer the driver did not hand out,
//! is refused with [`DevError::BadState`] instead of being turned into a
//! descriptor the hardware would then write through into memory the driver does
//! not own.  Tests drive both mistakes on purpose.
//!
//! One consequence is worth stating: a transmit buffer that the stack allocates
//! and then drops without transmitting is never reclaimed, because nothing
//! tells the driver about it.  The interface loses one of its `QS` slots when
//! that happens.  Linux has the same shape (`igc_tx_buffer` is reclaimed only
//! when its descriptor completes), and the network stack's transmit path always
//! either sends the buffer or reports the failure.

use alloc::{vec, vec::Vec};
use core::{marker::PhantomData, mem::ManuallyDrop, ptr::NonNull};

use axdriver_base::{BaseDriverOps, DevError, DevResult, DeviceType};

use super::{
    DMA_PAGE_BYTES, IgcBus, IgcHal, WindowBus,
    bringup::StationAddress,
    desc::{
        BufferPool, DESCRIPTOR_BYTES, DescriptorMemory, MAX_FRAME_BYTES, RX_BUFFER_BYTES,
        RX_HEADER_BYTES, RingError, RxRing, TxRing, tx_command_length, tx_offload_status,
    },
    regs::{
        self, QueueControl, ReceiveControl, RingBase, RingLength, SplitReceiveControl,
        TransmitControl,
    },
};
use crate::{EthernetAddress, NetBufPtr, NetDriverOps};

/// The device name the interface reports.
pub const DEVICE_NAME: &str = "igc";

/// The receive buffer size in bytes.
///
/// It is used in two places and in two different units, which is worth naming
/// once: `IGC_RLPML` takes a byte count, and `SRRCTL`'s packet-size field
/// takes kilobytes (`igc_base.h:97-99`), which
/// `SplitReceiveControl::one_buffer` divides down to.
const RX_PACKET_BYTES: u32 = RX_BUFFER_BYTES as u32;

/// Counters, for the report and for the tests.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Stats {
    /// Frames handed to the hardware.
    pub transmitted: u64,
    /// Bytes handed to the hardware.
    pub transmitted_bytes: u64,
    /// Frames taken from the receive ring.
    pub received: u64,
    /// Bytes taken from the receive ring.
    pub received_bytes: u64,
    /// Transmit buffers reclaimed after the hardware finished with them.
    pub tx_recycled: u64,
    /// Receive buffers handed back for re-arming.
    pub rx_recycled: u64,
    /// Receive descriptors discarded because they did not describe a usable
    /// frame.
    pub rx_dropped: u64,
    /// Receive descriptors whose length was set but whose done bit was not.
    pub rx_without_done: u64,
    /// Calls refused because a pointer did not belong to this driver.
    pub foreign_buffers: u64,
    /// Calls refused because the transmit ring had no room.
    pub tx_full: u64,
    /// Register writes the table allowed and the window refused, which can
    /// only mean the table and the mapping disagree.
    pub register_refused: u64,
}

impl Stats {
    /// The sentence a log line or a test uses.
    pub fn describe(&self) -> alloc::string::String {
        alloc::format!(
            "tx {} frames/{} bytes, rx {} frames/{} bytes, recycled tx {} rx {}, dropped {} \
             (length without done bit {}, foreign pointers {}, transmit ring full {}, register \
             writes refused {})",
            self.transmitted,
            self.transmitted_bytes,
            self.received,
            self.received_bytes,
            self.tx_recycled,
            self.rx_recycled,
            self.rx_dropped,
            self.rx_without_done,
            self.foreign_buffers,
            self.tx_full,
            self.register_refused,
        )
    }
}

/// One DMA allocation this driver owns, kept so it can be given back.
struct Allocation<H: IgcHal> {
    bus: u64,
    cpu: NonNull<u8>,
    pages: usize,
    _hal: PhantomData<H>,
}

impl<H: IgcHal> Drop for Allocation<H> {
    fn drop(&mut self) {
        // SAFETY: this owner is only dropped before publication to hardware,
        // or after the NIC has observed that bus mastering has stopped.
        unsafe { H::dma_dealloc(self.bus, self.cpu, self.pages) };
    }
}

/// The Intel i225/i226 NIC, with `QS` descriptors in each ring.
pub struct IgcNic<H: IgcHal, const QS: usize> {
    bus: WindowBus<H>,
    mac: [u8; 6],
    tx_memory: DescriptorMemory,
    tx_ring: TxRing,
    tx_pool: BufferPool,
    tx_free: Vec<usize>,
    /// Which slots are currently out with the *caller*, not with the ring.
    ///
    /// Set when [`IgcNic::alloc_tx_buffer`] hands a slot out and cleared the
    /// moment [`IgcNic::transmit`] commits it to the ring, so it answers one
    /// question: may this pointer be returned to the driver again?  A buffer
    /// that has been transmitted is the ring's until the done bit retires it,
    /// and a second `transmit` of the same pointer would enqueue one DMA slot
    /// twice and later free it twice, so that case has to be refused rather
    /// than tracked here.
    tx_in_flight: Vec<bool>,
    tx_owner: Vec<usize>,
    rx_memory: DescriptorMemory,
    rx_ring: RxRing,
    rx_pool: BufferPool,
    rx_owner: Vec<usize>,
    rx_free: Vec<usize>,
    rx_in_flight: Vec<bool>,
    stats: Stats,
    // A failed stop must retain DMA memory, not return live pages to the heap.
    allocations: ManuallyDrop<[Allocation<H>; 4]>,
}

// SAFETY: every field is either plain data, a bounded handle over DMA memory
// whose accesses are volatile and bounds-checked, or a value whose own safety
// comment says how it may be shared.  Everything that touches a ring or the
// register window goes through `&mut self`, so two threads cannot reach the
// device at once.
unsafe impl<H: IgcHal, const QS: usize> Sync for IgcNic<H, QS> {}
// SAFETY: as above.
unsafe impl<H: IgcHal, const QS: usize> Send for IgcNic<H, QS> {}

impl<H: IgcHal, const QS: usize> IgcNic<H, QS> {
    /// Take the device over: allocate the rings, program them, and fill the
    /// receive ring.
    ///
    /// The caller has already reset the part and read its station address; the
    /// address is passed in so that this function cannot be reached without
    /// one.
    pub fn init(bus: WindowBus<H>, station: &StationAddress) -> DevResult<Self> {
        // One descriptor is always left unused, so a ring of one descriptor
        // could never hand anything over.
        if QS < 2 {
            return Err(DevError::InvalidParam);
        }
        let Some(ring_length) = RingLength::new(QS, DESCRIPTOR_BYTES) else {
            return Err(DevError::InvalidParam);
        };

        // Four DMA regions: two descriptor rings and two buffer pools.  A
        // descriptor ring must be at least 128-byte aligned (its length must be
        // a multiple of 128, see `RingLength`) and each pool is carved into
        // fixed slots, so every
        // allocation is asked for page alignment and the alignment is checked
        // rather than assumed.
        // Array construction drops the preceding owners if a later allocation
        // fails. No address has been published to the device at this point.
        let allocations = [
            allocate::<H>(QS * DESCRIPTOR_BYTES, 4096)?,
            allocate::<H>(QS * DESCRIPTOR_BYTES, 4096)?,
            allocate::<H>(QS * RX_BUFFER_BYTES, 4096)?,
            allocate::<H>(QS * RX_BUFFER_BYTES, 4096)?,
        ];
        let [tx_descriptors, rx_descriptors, tx_buffers, rx_buffers] = &allocations;
        let tx_base = tx_descriptors.bus;
        let rx_base = rx_descriptors.bus;

        // SAFETY: every pointer below comes from `allocate`, which checks that
        // the region is live, non-null, aligned and long enough, and each value
        // is built over exactly the region its allocation describes.
        let tx_memory = unsafe { DescriptorMemory::new(tx_descriptors.cpu, QS) };
        let tx_pool =
            unsafe { BufferPool::new(tx_buffers.cpu, tx_buffers.bus, QS, RX_BUFFER_BYTES) };
        let rx_memory = unsafe { DescriptorMemory::new(rx_descriptors.cpu, QS) };
        let rx_pool =
            unsafe { BufferPool::new(rx_buffers.cpu, rx_buffers.bus, QS, RX_BUFFER_BYTES) };

        let mut nic = Self {
            bus,
            mac: station.bytes,
            tx_memory,
            tx_ring: TxRing::new(QS),
            tx_pool,
            tx_free: (0..QS).rev().collect(),
            tx_in_flight: vec![false; QS],
            tx_owner: vec![0; QS],
            rx_memory,
            rx_ring: RxRing::new(QS),
            rx_pool,
            rx_owner: vec![0; QS],
            rx_free: (0..QS).rev().collect(),
            rx_in_flight: vec![false; QS],
            stats: Stats::default(),
            allocations: ManuallyDrop::new(allocations),
        };
        nic.tx_memory.zero();
        nic.rx_memory.zero();
        // Fill the receive ring before anything can arrive, and hand it over.
        nic.fill_receive_ring();
        // Establish the owner before any fallible register write, and prepare
        // the descriptors before enabling the queues. Errors now use the same
        // DMA stop discipline as ordinary teardown.
        enable_mac(&mut nic.bus)?;
        configure_transmit(&mut nic.bus, tx_base, ring_length)?;
        configure_receive(&mut nic.bus, rx_base, ring_length)?;
        nic.write_receive_tail();
        Ok(nic)
    }

    /// The station address the hardware reported.
    pub const fn mac(&self) -> [u8; 6] {
        self.mac
    }

    /// The counters since this device was taken over.
    pub const fn stats(&self) -> &Stats {
        &self.stats
    }

    /// Whether the receive ring has a frame waiting.
    ///
    /// This reads one descriptor, which is the whole cost of the check, and it
    /// is what the stack's `can_receive` becomes.
    pub fn receive_pending(&self) -> bool {
        if self.rx_ring.outstanding() == 0 {
            return false;
        }
        self.rx_memory
            .rx_length(self.rx_ring.next_to_clean())
            .is_some_and(|length| length != 0)
    }

    /// Whether a frame can be handed to the hardware right now.
    pub fn transmit_ready(&self) -> bool {
        self.tx_ring.has_room() && !self.tx_free.is_empty()
    }

    /// Fill the receive ring from the free list, as far as it will go.
    ///
    /// Stops when the ring is full or when no buffer is free, which is what
    /// makes a late return harmless: the tail simply does not move.
    fn fill_receive_ring(&mut self) {
        while self.rx_ring.has_room() {
            let Some(slot) = self.rx_free.pop() else {
                break;
            };
            let Some(address) = self.rx_pool.bus_address(slot) else {
                self.rx_free.push(slot);
                break;
            };
            match self.rx_ring.fill(&mut self.rx_memory, address) {
                Ok(index) => self.rx_owner[index] = slot,
                Err(_) => {
                    self.rx_free.push(slot);
                    break;
                }
            }
        }
    }

    /// Write the receive tail register.
    ///
    /// This is the only place `IGC_RDT(0)` is written after init, and it
    /// happens after the descriptors are in place: the tail is what tells the
    /// hardware those descriptors are its.
    fn write_receive_tail(&mut self) {
        let tail = self.rx_ring.tail() as u32;
        if !self.bus.write(named("IGC_RDT(0)"), tail) {
            self.stats.register_refused += 1;
        }
    }

    /// The slot a returned transmit buffer belongs to.
    fn transmit_slot(&mut self, buffer: &NetBufPtr) -> DevResult<usize> {
        let Some(slot) = self.tx_pool.slot_of(raw_pointer(buffer)) else {
            self.stats.foreign_buffers += 1;
            return Err(DevError::BadState);
        };
        if !self.tx_in_flight.get(slot).copied().unwrap_or(false) {
            // A buffer that is not out cannot come back.
            self.stats.foreign_buffers += 1;
            return Err(DevError::BadState);
        }
        Ok(slot)
    }

    /// The slot a returned receive buffer belongs to.
    fn receive_slot(&mut self, buffer: &NetBufPtr) -> DevResult<usize> {
        let Some(slot) = self.rx_pool.slot_of(raw_pointer(buffer)) else {
            self.stats.foreign_buffers += 1;
            return Err(DevError::BadState);
        };
        if !self.rx_in_flight.get(slot).copied().unwrap_or(false) {
            self.stats.foreign_buffers += 1;
            return Err(DevError::BadState);
        }
        Ok(slot)
    }

    /// Build the `NetBufPtr` the stack sees for one of this driver's slots.
    ///
    /// The raw pointer and the packet pointer are the same address: a slot's
    /// buffer starts at the frame, and the slot index is recovered from that
    /// address when the buffer comes back, so no per-packet bookkeeping object
    /// is needed.
    fn buffer_for(&self, pool: &BufferPool, slot: usize, length: usize) -> DevResult<NetBufPtr> {
        let Some(pointer) = pool.cpu_address(slot) else {
            return Err(DevError::BadState);
        };
        Ok(NetBufPtr::new(pointer, pointer, length))
    }

    /// Discard the descriptor at the receive ring's cursor and give its buffer
    /// back.
    ///
    /// A descriptor that reports an impossible frame still has to be reclaimed:
    /// leaving it would stall the ring, and the buffer it names is the driver's
    /// again either way.
    fn discard_receive_descriptor(&mut self) {
        let index = self.rx_ring.next_to_clean();
        let slot = self.rx_owner[index];
        self.rx_free.push(slot);
        let _ = self.rx_ring.discard();
        self.stats.rx_dropped += 1;
        // The buffer is the driver's again, so it goes straight back into the
        // ring at the tail: a dropped frame must not cost the ring a slot.
        let before = self.rx_ring.tail();
        self.fill_receive_ring();
        if self.rx_ring.tail() != before {
            self.write_receive_tail();
        }
    }
}

impl<H: IgcHal, const QS: usize> BaseDriverOps for IgcNic<H, QS> {
    fn device_name(&self) -> &str {
        DEVICE_NAME
    }

    fn device_type(&self) -> DeviceType {
        DeviceType::Net
    }
}

impl<H: IgcHal, const QS: usize> NetDriverOps for IgcNic<H, QS> {
    fn mac_address(&self) -> EthernetAddress {
        EthernetAddress(self.mac)
    }

    fn can_transmit(&self) -> bool {
        self.transmit_ready()
    }

    fn can_receive(&self) -> bool {
        self.receive_pending()
    }

    fn rx_queue_size(&self) -> usize {
        QS
    }

    fn tx_queue_size(&self) -> usize {
        QS
    }

    fn recycle_rx_buffer(&mut self, rx_buf: NetBufPtr) -> DevResult {
        let slot = self.receive_slot(&rx_buf)?;
        self.rx_in_flight[slot] = false;
        self.rx_free.push(slot);
        self.stats.rx_recycled += 1;
        // Refill from the tail and tell the hardware about it.  The tail is
        // written only when it moved, so a recycle that cannot be re-armed yet
        // costs nothing.
        let before = self.rx_ring.tail();
        self.fill_receive_ring();
        if self.rx_ring.tail() != before {
            self.write_receive_tail();
        }
        Ok(())
    }

    fn recycle_tx_buffers(&mut self) -> DevResult {
        // The hardware writes the done bit in ring order, so reclamation is a
        // walk from the driver's own cursor while the bit is set.
        while self.tx_ring.outstanding() > 0 {
            let index = self.tx_ring.next_to_clean();
            if !self.tx_memory.tx_done(index) {
                break;
            }
            let Ok(index) = self.tx_ring.release() else {
                break;
            };
            let slot = self.tx_owner[index];
            self.tx_in_flight[slot] = false;
            self.tx_free.push(slot);
            self.stats.tx_recycled += 1;
        }
        Ok(())
    }

    fn transmit(&mut self, tx_buf: NetBufPtr) -> DevResult {
        let length = tx_buf.packet_len();
        if length == 0 || length > MAX_FRAME_BYTES {
            self.stats.foreign_buffers += 1;
            return Err(DevError::InvalidParam);
        }
        let slot = self.transmit_slot(&tx_buf)?;
        let Some(address) = self.tx_pool.bus_address(slot) else {
            return Err(DevError::BadState);
        };
        let Ok(index) = self.tx_ring.take() else {
            self.stats.tx_full += 1;
            return Err(DevError::Again);
        };
        // The descriptor's three words, then the tail.  The tail write is what
        // hands the descriptor over, so it comes after the descriptor is
        // complete and after a fence, which `write_tx` performs.
        if !self.tx_memory.write_tx(
            index,
            address,
            tx_command_length(length),
            tx_offload_status(length),
        ) {
            return Err(DevError::BadState);
        }
        self.tx_owner[index] = slot;
        // Ownership has moved from the caller to the ring. A second return
        // of this pointer must not enqueue the same DMA slot again and later
        // insert it into the free list twice. Completion alone frees it.
        self.tx_in_flight[slot] = false;
        let tail = self.tx_ring.tail() as u32;
        if !self.bus.write(named("IGC_TDT(0)"), tail) {
            self.stats.register_refused += 1;
            return Err(DevError::BadState);
        }
        self.stats.transmitted += 1;
        self.stats.transmitted_bytes += length as u64;
        Ok(())
    }

    fn alloc_tx_buffer(&mut self, size: usize) -> DevResult<NetBufPtr> {
        if size == 0 || size > MAX_FRAME_BYTES {
            return Err(DevError::InvalidParam);
        }
        if !self.transmit_ready() {
            self.stats.tx_full += 1;
            return Err(DevError::Again);
        }
        let Some(slot) = self.tx_free.pop() else {
            self.stats.tx_full += 1;
            return Err(DevError::Again);
        };
        self.tx_in_flight[slot] = true;
        self.buffer_for(&self.tx_pool, slot, size)
    }

    fn receive(&mut self) -> DevResult<NetBufPtr> {
        loop {
            match self.rx_ring.take(&self.rx_memory) {
                Ok((index, frame)) => {
                    if !frame.done {
                        self.stats.rx_without_done += 1;
                    }
                    let slot = self.rx_owner[index];
                    self.rx_in_flight[slot] = true;
                    self.stats.received += 1;
                    self.stats.received_bytes += frame.length as u64;
                    return self.buffer_for(&self.rx_pool, slot, frame.length);
                }
                Err(RingError::NotReady(_)) | Err(RingError::Empty(_)) => {
                    return Err(DevError::Again);
                }
                Err(_) => {
                    // A descriptor that cannot describe a usable frame is
                    // dropped and the ring moves on; the interface does not
                    // fail because one packet was malformed.
                    self.discard_receive_descriptor();
                }
            }
        }
    }
}

impl<H: IgcHal, const QS: usize> Drop for IgcNic<H, QS> {
    fn drop(&mut self) {
        // Stop both queues before the rings they point at go away, so the
        // hardware cannot be writing into memory that is about to be freed.
        let _ = self
            .bus
            .write(named("IGC_RXDCTL(0)"), QueueControl::disabled().raw());
        let _ = self
            .bus
            .write(named("IGC_TXDCTL(0)"), QueueControl::disabled().raw());
        // Posted queue-disable writes are not proof that outstanding DMA has
        // completed. Reuse the reset path's bounded PCIe-master handshake.
        if matches!(
            super::bringup::disable_pcie_master(&mut self.bus),
            Ok(Some(_))
        ) {
            // SAFETY: the device reported that it can no longer access these
            // pages. This is the sole destruction of these four owners.
            unsafe { ManuallyDrop::drop(&mut self.allocations) };
        } else {
            log::warn!("igc: DMA stop was not confirmed; retaining ring and packet memory");
        }
    }
}

#[cfg(test)]
impl<H: IgcHal, const QS: usize> IgcNic<H, QS> {
    /// The addresses of the four DMA regions, for a test that has to play the
    /// device's side of the conversation.
    pub(crate) fn test_regions(&self) -> (NonNull<u8>, NonNull<u8>, NonNull<u8>, NonNull<u8>) {
        (
            self.tx_memory.test_base(),
            self.rx_memory.test_base(),
            self.tx_pool.cpu_address(0).expect("slot 0"),
            self.rx_pool.cpu_address(0).expect("slot 0"),
        )
    }

    /// How many transmit slots are free, for a test that checks reclamation.
    pub(crate) fn test_free_tx_slots(&self) -> usize {
        self.tx_free.len()
    }

    /// How many receive slots are free.
    pub(crate) fn test_free_rx_slots(&self) -> usize {
        self.rx_free.len()
    }
}

/// The raw pointer inside a buffer the stack is handing back.
fn raw_pointer(buffer: &NetBufPtr) -> NonNull<u8> {
    NonNull::new(buffer.raw_ptr::<u8>()).expect("a buffer from this driver has a pointer")
}

/// A register by name, resolved at the call site so a typo is a panic in a
/// test rather than a wrong offset.
fn named(name: &str) -> regs::Register {
    regs::named(name).unwrap_or_else(|| panic!("{name} is not in the register table"))
}

/// Write a register, or fail.
fn write<B: IgcBus>(bus: &mut B, name: &str, value: u32) -> DevResult {
    let register = named(name);
    if bus.write(register, value) {
        Ok(())
    } else {
        Err(DevError::BadState)
    }
}

/// Read a register, or fail.
fn read<B: IgcBus>(bus: &mut B, name: &str) -> DevResult<u32> {
    bus.read(named(name)).ok_or(DevError::BadState)
}

/// Allocate one DMA region and remember it for the teardown path.
///
/// The region is allocated in whole pages, because that is the primitive the
/// platform offers, and the alignment the caller asked for is *checked* rather
/// than assumed: a page allocation satisfies every alignment up to the page
/// size, and a caller that needs more than that gets an error instead of a
/// ring the hardware would read from the wrong place.
fn allocate<H: IgcHal>(size: usize, align: usize) -> DevResult<Allocation<H>> {
    if align > DMA_PAGE_BYTES {
        return Err(DevError::InvalidParam);
    }
    let pages = size.div_ceil(DMA_PAGE_BYTES);
    let Some((bus, cpu)) = H::dma_alloc(pages) else {
        return Err(DevError::NoMemory);
    };
    if !(cpu.as_ptr() as usize).is_multiple_of(align) {
        // SAFETY: the allocation came from `H::dma_alloc` and is being given
        // straight back because it does not satisfy the alignment the rings
        // need.
        unsafe { H::dma_dealloc(bus, cpu, pages) };
        return Err(DevError::NoMemory);
    }
    Ok(Allocation {
        bus,
        cpu,
        pages,
        _hal: PhantomData,
    })
}

/// Program the receive and transmit control registers
/// (`igc_setup_rctl` and `igc_setup_tctl`, `igc_main.c:835`, `:882`), and the
/// long-packet bound.
fn enable_mac<H: IgcHal>(bus: &mut WindowBus<H>) -> DevResult {
    write(bus, "IGC_RCTL", ReceiveControl::setup_value().raw())?;
    // The long-packet bound is the size of the buffers this driver gives the
    // hardware, not the vendor driver's jumbo bound
    // (`MAX_JUMBO_FRAME_SIZE`, `igc_defines.h:147`): a receive limit larger
    // than the buffer a frame is written into is a limit this driver cannot
    // honour.
    write(bus, "IGC_RLPML", RX_PACKET_BYTES)?;
    let current = read(bus, "IGC_TCTL")?;
    write(bus, "IGC_TCTL", TransmitControl::setup_value(current).raw())?;
    Ok(())
}

/// Program the transmit ring, the way `igc_configure_tx_ring` does
/// (`igc_main.c:728-758`).
fn configure_transmit<H: IgcHal>(
    bus: &mut WindowBus<H>,
    descriptors: u64,
    length: RingLength,
) -> DevResult {
    write(bus, "IGC_TXDCTL(0)", QueueControl::disabled().raw())?;
    write(bus, "IGC_TDLEN(0)", length.bytes())?;
    let base = RingBase::new(descriptors);
    write(bus, "IGC_TDBAL(0)", base.low())?;
    write(bus, "IGC_TDBAH(0)", base.high())?;
    write(bus, "IGC_TDH(0)", 0)?;
    write(bus, "IGC_TDT(0)", 0)?;
    write(
        bus,
        "IGC_TXDCTL(0)",
        QueueControl::transmit_defaults().with_queue_enable().raw(),
    )?;
    Ok(())
}

/// Program the receive ring, the way `igc_configure_rx_ring` does
/// (`igc_main.c:625-702`).
fn configure_receive<H: IgcHal>(
    bus: &mut WindowBus<H>,
    descriptors: u64,
    length: RingLength,
) -> DevResult {
    write(bus, "IGC_RXDCTL(0)", QueueControl::disabled().raw())?;
    let base = RingBase::new(descriptors);
    write(bus, "IGC_RDBAL(0)", base.low())?;
    write(bus, "IGC_RDBAH(0)", base.high())?;
    write(bus, "IGC_RDLEN(0)", length.bytes())?;
    write(bus, "IGC_RDH(0)", 0)?;
    write(bus, "IGC_RDT(0)", 0)?;
    write(
        bus,
        "IGC_SRRCTL(0)",
        SplitReceiveControl::one_buffer(RX_PACKET_BYTES, RX_HEADER_BYTES as u32).raw(),
    )?;
    write(
        bus,
        "IGC_RXDCTL(0)",
        QueueControl::receive_defaults().with_queue_enable().raw(),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;
    use crate::igc::{
        desc::RX_BUFFER_BYTES,
        fake::{FakeBus, FakeHal},
        regs::{self as regs, RegisterWindow, WINDOW_BYTES, bits},
    };

    /// A small ring: eight descriptors is the smallest size whose byte length
    /// is a multiple of 128, the multiple the vendor driver's own
    /// `REQ_*_DESCRIPTOR_MULTIPLE` constants imply (`igc_defines.h:9-11`).
    const QS: usize = 8;

    /// A locally administered unicast address.
    const MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];

    fn station() -> StationAddress {
        StationAddress {
            bytes: MAC,
            low: u32::from_le_bytes([MAC[0], MAC[1], MAC[2], MAC[3]]),
            high: 0x8000_u32 | u32::from(MAC[4]) | (u32::from(MAC[5]) << 8),
            address_valid: true,
        }
    }

    /// The device's aperture, the driver, and the plumbing a test needs to
    /// play the device's side of the conversation.
    ///
    /// The aperture is ordinary memory: a `RegisterWindow` over a buffer is
    /// the same code path the real driver uses, which is the point of testing
    /// it this way.  DMA memory comes from [`FakeHal`], whose allocations are
    /// page-aligned and zeroed, exactly like the platform allocator's.
    struct Harness {
        nic: IgcNic<FakeHal, QS>,
        // Fields drop in declaration order: the NIC writes queue registers
        // during Drop, so its register window must still be allocated then.
        aperture: vec::Vec<u32>,
    }

    impl Harness {
        fn new() -> Self {
            FakeHal::reset();
            FakeHal::take_allocation_count();
            let mut aperture = vec![0u32; WINDOW_BYTES / 4];
            // SAFETY: the buffer is live, is `WINDOW_BYTES` long and 4-byte
            // aligned, and outlives the driver built over it because it is
            // owned by this struct.
            let bus = unsafe {
                WindowBus::<FakeHal>::new(RegisterWindow::from_mapped(
                    aperture.as_mut_ptr() as usize,
                    WINDOW_BYTES,
                ))
            };
            let nic = IgcNic::<FakeHal, QS>::init(bus, &station()).expect("the rings come up");
            Self { aperture, nic }
        }

        /// A register's current value, by name.
        fn register(&self, name: &str) -> u32 {
            self.aperture[regs::named(name).expect("named").offset() as usize / 4]
        }

        /// The four DMA regions, in the order `init` allocates them.
        fn regions(&self) -> (NonNull<u8>, NonNull<u8>, NonNull<u8>, NonNull<u8>) {
            self.nic.test_regions()
        }

        /// One 32-bit word of a region.
        fn word(&self, region: NonNull<u8>, offset: usize) -> u32 {
            // SAFETY: the test only asks for offsets inside the allocation the
            // driver made, which is live for as long as the driver is.
            unsafe { core::ptr::read_volatile(region.as_ptr().add(offset).cast::<u32>()) }
        }

        /// A descriptor's 64-bit buffer address.
        fn descriptor_address(&self, region: NonNull<u8>, index: usize) -> u64 {
            let offset = index * DESCRIPTOR_BYTES;
            u64::from(self.word(region, offset)) | (u64::from(self.word(region, offset + 4)) << 32)
        }

        /// Write a transmit descriptor's write-back status word, which is what
        /// the hardware does when it finishes with a frame.
        fn complete_transmit(&mut self, index: usize) {
            let (tx_descriptors, ..) = self.regions();
            // SAFETY: the offset is inside the transmit descriptor ring.
            unsafe {
                core::ptr::write_volatile(
                    tx_descriptors
                        .as_ptr()
                        .add(index * DESCRIPTOR_BYTES + 12)
                        .cast::<u32>(),
                    bits::TXD_STAT_DD,
                );
            }
        }

        /// Write a receive descriptor's write-back words, which is what the
        /// hardware does when a frame arrives.
        fn receive_frame(&mut self, index: usize, length: u16, status: u32) {
            let (_, rx_descriptors, ..) = self.regions();
            let offset = index * DESCRIPTOR_BYTES;
            // SAFETY: the offsets are inside the receive descriptor ring:
            // `status_error` is the third word and `length` the low half of
            // the fourth (`igc_base.h:57-86`).
            unsafe {
                core::ptr::write_volatile(
                    rx_descriptors.as_ptr().add(offset + 8).cast::<u32>(),
                    status,
                );
                core::ptr::write_volatile(
                    rx_descriptors.as_ptr().add(offset + 12).cast::<u32>(),
                    u32::from(length),
                );
            }
        }

        /// Put bytes into a buffer the driver handed out.
        fn write_bytes(&self, buffer: &mut NetBufPtr, bytes: &[u8]) {
            buffer.packet_mut()[..bytes.len()].copy_from_slice(bytes);
        }
    }

    #[test]
    fn init_programs_both_rings_the_way_the_vendor_driver_does() {
        let harness = Harness::new();
        let (tx_descriptors, rx_descriptors, ..) = harness.regions();
        let ring_bytes = (QS * DESCRIPTOR_BYTES) as u32;

        // The register writes igc_configure_tx_ring and
        // igc_configure_rx_ring make.
        assert_eq!(harness.register("IGC_TDLEN(0)"), ring_bytes);
        assert_eq!(harness.register("IGC_RDLEN(0)"), ring_bytes);
        assert_eq!(harness.register("IGC_TDH(0)"), 0);
        assert_eq!(harness.register("IGC_RDH(0)"), 0);
        assert_eq!(
            harness.register("IGC_TDBAL(0)"),
            tx_descriptors.as_ptr() as u32,
            "the transmit ring base is the transmit descriptor region",
        );
        assert_eq!(
            harness.register("IGC_RDBAL(0)"),
            rx_descriptors.as_ptr() as u32,
        );
        assert_eq!(
            harness.register("IGC_TXDCTL(0)"),
            QueueControl::transmit_defaults().with_queue_enable().raw(),
        );
        assert_eq!(
            harness.register("IGC_RXDCTL(0)"),
            QueueControl::receive_defaults().with_queue_enable().raw(),
        );
        // The receive control value, and the long-packet bound the driver
        // chose: its own buffer size, not the vendor's jumbo bound.
        assert_eq!(harness.register("IGC_RCTL"), 0x0400_8022);
        assert_eq!(harness.register("IGC_RLPML"), RX_BUFFER_BYTES as u32);
        assert_eq!(harness.register("IGC_TCTL"), 0x0100_00fa);
        assert_eq!(
            harness.register("IGC_SRRCTL(0)"),
            (4 << 8) | 2 | (1 << 25),
            "BSIZEHDR(256) | BSIZEPKT(2 KiB) | DESCTYPE_ADV_ONEBUF",
        );
        // The tail: one short of the ring, which is what hands the filled
        // descriptors to the hardware.
        assert_eq!(harness.register("IGC_RDT(0)"), (QS - 1) as u32);
        assert_eq!(harness.register("IGC_TDT(0)"), 0);
    }

    #[test]
    fn init_fills_the_receive_ring_with_its_own_buffers_and_leaves_one_descriptor_unused() {
        let harness = Harness::new();
        let (_, rx_descriptors, _, rx_buffers) = harness.regions();
        for index in 0..QS - 1 {
            let expected = rx_buffers.as_ptr() as u64 + (index * RX_BUFFER_BYTES) as u64;
            assert_eq!(
                harness.descriptor_address(rx_descriptors, index),
                expected,
                "descriptor {index}",
            );
            // The length field is clear, so a stale value cannot look like a
            // frame.
            assert_eq!(
                harness.word(rx_descriptors, index * DESCRIPTOR_BYTES + 12),
                0
            );
        }
        // The descriptor the tail stops before is untouched: it is the one the
        // ring always leaves unused.
        assert_eq!(harness.descriptor_address(rx_descriptors, QS - 1), 0);
        assert_eq!(harness.nic.test_free_rx_slots(), 1);
    }

    #[test]
    fn a_transmitted_frame_lands_in_a_descriptor_with_the_vendors_encoding() {
        let mut harness = Harness::new();
        let (tx_descriptors, _, tx_buffers, _) = harness.regions();
        let frame: vec::Vec<u8> = (0..60u8).collect();

        let mut buffer = harness.nic.alloc_tx_buffer(frame.len()).expect("a buffer");
        harness.write_bytes(&mut buffer, &frame);
        harness.nic.transmit(buffer).expect("the frame goes out");

        // Word for word, as igc_tx_map writes it.
        assert_eq!(
            harness.descriptor_address(tx_descriptors, 0),
            tx_buffers.as_ptr() as u64,
        );
        assert_eq!(
            harness.word(tx_descriptors, 8),
            0x2b30_0000 | 60,
            "DTYP_DATA | DEXT | IFCS | EOP | RS | length",
        );
        assert_eq!(harness.word(tx_descriptors, 12), 60 << 14, "PAYLEN");
        // The tail was written, which is what hands the descriptor over.
        assert_eq!(harness.register("IGC_TDT(0)"), 1);
        assert_eq!(harness.nic.stats().transmitted, 1);
        assert_eq!(harness.nic.stats().transmitted_bytes, 60);
    }

    #[test]
    fn a_transmit_buffer_comes_back_only_after_the_hardware_is_done_with_it() {
        let mut harness = Harness::new();
        let free_before = harness.nic.test_free_tx_slots();
        let buffer = harness.nic.alloc_tx_buffer(64).expect("a buffer");
        assert_eq!(harness.nic.test_free_tx_slots(), free_before - 1);
        harness.nic.transmit(buffer).expect("the frame goes out");
        // The hardware has not written the done bit: nothing is reclaimed and
        // the slot is not available again.
        harness.nic.recycle_tx_buffers().expect("nothing to do");
        assert_eq!(harness.nic.test_free_tx_slots(), free_before - 1);
        assert_eq!(harness.nic.stats().tx_recycled, 0);
        // The hardware finishes.
        harness.complete_transmit(0);
        harness.nic.recycle_tx_buffers().expect("reclaim");
        assert_eq!(harness.nic.test_free_tx_slots(), free_before);
        assert_eq!(harness.nic.stats().tx_recycled, 1);
    }

    #[test]
    fn a_received_frame_is_handed_over_with_its_length_and_its_bytes() {
        let mut harness = Harness::new();
        let (_, _, _, rx_buffers) = harness.regions();
        let payload: vec::Vec<u8> = (0..64u8).collect();
        // The device writes the frame into the buffer descriptor 0 points at,
        // then writes the descriptor back.
        // SAFETY: the first receive buffer is live and 64 bytes fit in it.
        unsafe {
            core::ptr::copy_nonoverlapping(payload.as_ptr(), rx_buffers.as_ptr(), payload.len());
        }
        harness.receive_frame(0, 64, bits::RXD_STAT_DD | bits::RXD_STAT_EOP);

        assert!(harness.nic.can_receive());
        let received = harness.nic.receive().expect("a frame");
        assert_eq!(received.packet_len(), 64);
        assert_eq!(received.packet(), &payload[..]);
        assert_eq!(harness.nic.stats().received, 1);
        // The length is used exactly as the hardware reported it: `RCTL.SECRC`
        // means it excludes the CRC, so the driver must not adjust it.
        assert_eq!(harness.nic.stats().received_bytes, 64);
        // With one frame taken, the next read finds nothing.
        assert!(!harness.nic.can_receive());
        assert!(matches!(harness.nic.receive(), Err(DevError::Again)));
    }

    #[test]
    fn a_recycled_receive_buffer_is_armed_in_the_next_descriptor_and_the_tail_wraps() {
        let mut harness = Harness::new();
        let (_, rx_descriptors, _, rx_buffers) = harness.regions();
        harness.receive_frame(0, 60, bits::RXD_STAT_DD | bits::RXD_STAT_EOP);
        let received = harness.nic.receive().expect("a frame");
        // One slot is always free: the descriptor the ring leaves unused has
        // no buffer in it.
        assert_eq!(harness.nic.test_free_rx_slots(), 1);
        assert_eq!(harness.register("IGC_RDT(0)"), (QS - 1) as u32);

        harness
            .nic
            .recycle_rx_buffer(received)
            .expect("the buffer goes back");
        // The recycled buffer was armed in descriptor 7 -- the tail -- and the
        // tail wrapped to zero, so the hardware now owns 1..7.
        assert_eq!(harness.register("IGC_RDT(0)"), 0);
        assert_eq!(
            harness.descriptor_address(rx_descriptors, QS - 1),
            rx_buffers.as_ptr() as u64,
            "the first slot's buffer, now in the descriptor the tail reached",
        );
        assert_eq!(harness.nic.test_free_rx_slots(), 1);
        assert_eq!(harness.nic.stats().rx_recycled, 1);
    }

    #[test]
    fn a_foreign_pointer_is_refused_rather_than_turned_into_a_descriptor() {
        let mut harness = Harness::new();
        // A buffer that is not from this driver's pool at all.
        let foreign = FakeHal::foreign_allocation(RX_BUFFER_BYTES, 4096);
        assert!(matches!(
            harness
                .nic
                .recycle_rx_buffer(NetBufPtr::new(foreign, foreign, 64)),
            Err(DevError::BadState)
        ));
        assert!(matches!(
            harness.nic.transmit(NetBufPtr::new(foreign, foreign, 64)),
            Err(DevError::BadState)
        ));
        assert_eq!(harness.nic.stats().foreign_buffers, 2);
        // SAFETY: the foreign allocation is still owned by this test.
        unsafe {
            std::alloc::dealloc(
                foreign.as_ptr(),
                std::alloc::Layout::from_size_align(RX_BUFFER_BYTES, 4096).unwrap(),
            )
        };
    }

    #[test]
    fn a_buffer_can_only_come_back_once() {
        let mut harness = Harness::new();
        harness.receive_frame(0, 60, bits::RXD_STAT_DD | bits::RXD_STAT_EOP);
        let received = harness.nic.receive().expect("a frame");
        let raw = received.raw_ptr::<u8>();
        let second = NetBufPtr::new(
            NonNull::new(raw).expect("a pointer"),
            NonNull::new(raw).expect("a pointer"),
            60,
        );
        harness
            .nic
            .recycle_rx_buffer(received)
            .expect("the first return is the real one");
        // The same buffer a second time: it is not in flight any more, so it
        // is refused instead of being armed twice.
        assert!(matches!(
            harness.nic.recycle_rx_buffer(second),
            Err(DevError::BadState)
        ));
        assert_eq!(harness.nic.stats().foreign_buffers, 1);
    }

    #[test]
    fn a_transmit_buffer_cannot_be_queued_twice_before_completion() {
        let mut harness = Harness::new();
        let buffer = harness.nic.alloc_tx_buffer(64).unwrap();
        let pointer = NonNull::new(buffer.raw_ptr::<u8>()).unwrap();
        let duplicate = NetBufPtr::new(pointer, pointer, 64);
        harness.nic.transmit(buffer).unwrap();
        assert!(matches!(
            harness.nic.transmit(duplicate),
            Err(DevError::BadState)
        ));
        assert_eq!(harness.register("IGC_TDT(0)"), 1);
        harness.complete_transmit(0);
        harness.nic.recycle_tx_buffers().unwrap();
        assert_eq!(harness.nic.test_free_tx_slots(), QS);
    }

    #[test]
    fn a_frame_larger_than_the_driver_handles_is_refused() {
        let mut harness = Harness::new();
        assert!(matches!(
            harness.nic.alloc_tx_buffer(MAX_FRAME_BYTES + 1),
            Err(DevError::InvalidParam)
        ));
        assert!(matches!(
            harness.nic.alloc_tx_buffer(0),
            Err(DevError::InvalidParam)
        ));
        // The largest frame the vendor driver's arithmetic allows is accepted.
        assert!(harness.nic.alloc_tx_buffer(MAX_FRAME_BYTES).is_ok());
    }

    #[test]
    fn the_transmit_ring_reports_full_rather_than_overwriting_a_descriptor() {
        let mut harness = Harness::new();
        // One descriptor is always left unused, so the ring takes QS - 1
        // frames and then says so.
        for index in 0..QS - 1 {
            let buffer = harness
                .nic
                .alloc_tx_buffer(64)
                .unwrap_or_else(|error| panic!("frame {index}: {error:?}"));
            harness.nic.transmit(buffer).expect("the frame goes out");
        }
        assert!(!harness.nic.can_transmit());
        assert!(matches!(
            harness.nic.alloc_tx_buffer(64),
            Err(DevError::Again)
        ));
        assert_eq!(harness.nic.stats().tx_full, 1);
        // The tail register never moved past the last descriptor it may hold.
        assert_eq!(harness.register("IGC_TDT(0)"), (QS - 1) as u32);
    }

    #[test]
    fn a_malformed_receive_descriptor_is_dropped_and_the_ring_moves_on() {
        let mut harness = Harness::new();
        let (_, rx_descriptors, ..) = harness.regions();
        // A length with no end-of-packet bit: this driver's single-buffer
        // receive path cannot assemble a split packet, so the descriptor is
        // dropped rather than handed to the stack as a fragment.
        harness.receive_frame(0, 64, bits::RXD_STAT_DD);
        assert!(matches!(harness.nic.receive(), Err(DevError::Again)));
        assert_eq!(harness.nic.stats().rx_dropped, 1);
        assert!(!harness.nic.can_receive());
        // The descriptor was given back to the ring: its buffer is armed in
        // the descriptor at the tail, and the tail moved.
        assert_eq!(harness.nic.stats().rx_recycled, 0);
        assert_eq!(
            harness.descriptor_address(rx_descriptors, QS - 1),
            harness.regions().3.as_ptr() as u64,
        );
        assert_eq!(harness.register("IGC_RDT(0)"), 0);
    }

    #[test]
    fn a_length_without_the_done_bit_is_still_received_and_counted() {
        // The vendor driver's hot path treats a non-zero length as "written
        // back" and never looks at the done bit; this driver does the same and
        // reports the difference, so that a part which does not set it is
        // visible rather than a ring that never delivers anything.
        let mut harness = Harness::new();
        harness.receive_frame(0, 60, bits::RXD_STAT_EOP);
        let received = harness.nic.receive().expect("a frame");
        assert_eq!(received.packet_len(), 60);
        assert_eq!(harness.nic.stats().rx_without_done, 1);
    }

    #[test]
    fn the_net_driver_ops_report_the_device_the_stack_expects() {
        let harness = Harness::new();
        assert_eq!(harness.nic.device_name(), DEVICE_NAME);
        assert_eq!(harness.nic.device_type(), DeviceType::Net);
        assert_eq!(harness.nic.mac_address().0, MAC);
        assert_eq!(harness.nic.rx_queue_size(), QS);
        assert_eq!(harness.nic.tx_queue_size(), QS);
        // The device is ready to transmit and has nothing to receive.
        assert!(harness.nic.can_transmit());
        assert!(!harness.nic.can_receive());
        // No interrupts: this driver polls, and the stack asks the platform
        // for an IRQ number only if there is one.
        assert_eq!(harness.nic.irq_num(), None);
    }

    #[test]
    fn dropping_the_device_stops_both_queues_and_gives_every_allocation_back() {
        let mut aperture = vec![0u32; WINDOW_BYTES / 4];
        FakeHal::reset();
        FakeHal::take_allocation_count();
        {
            // SAFETY: as in `Harness::new`.
            let bus = unsafe {
                WindowBus::<FakeHal>::new(RegisterWindow::from_mapped(
                    aperture.as_mut_ptr() as usize,
                    WINDOW_BYTES,
                ))
            };
            let nic = IgcNic::<FakeHal, QS>::init(bus, &station()).expect("the rings come up");
            assert_eq!(FakeHal::live_allocations(), 4);
            drop(nic);
        }
        // The queues were stopped before their rings went away, and every
        // allocation was given back.
        let aperture_words =
            |name: &str| aperture[regs::named(name).unwrap().offset() as usize / 4];
        assert_eq!(aperture_words("IGC_RXDCTL(0)"), 0);
        assert_eq!(aperture_words("IGC_TXDCTL(0)"), 0);
        assert_eq!(FakeHal::live_allocations(), 0, "nothing leaked");
    }

    #[test]
    fn a_ring_of_one_descriptor_is_refused() {
        // One descriptor is always left unused, so a ring of one could never
        // hand anything over.
        FakeHal::reset();
        FakeHal::take_allocation_count();
        let mut aperture = vec![0u32; WINDOW_BYTES / 4];
        // SAFETY: as in `Harness::new`.
        let bus = unsafe {
            WindowBus::<FakeHal>::new(RegisterWindow::from_mapped(
                aperture.as_mut_ptr() as usize,
                WINDOW_BYTES,
            ))
        };
        assert!(matches!(
            IgcNic::<FakeHal, 1>::init(bus, &station()),
            Err(DevError::InvalidParam)
        ));
        assert_eq!(FakeHal::live_allocations(), 0, "nothing was leaked");
    }

    #[test]
    fn partial_dma_allocation_failure_releases_every_unpublished_region() {
        struct FailAfter<const N: usize>;
        impl<const N: usize> IgcHal for FailAfter<N> {
            fn busy_wait_us(us: u32) {
                FakeHal::busy_wait_us(us);
            }
            fn dma_alloc(pages: usize) -> Option<(u64, NonNull<u8>)> {
                if FakeHal::live_allocations() == N {
                    None
                } else {
                    FakeHal::dma_alloc(pages)
                }
            }
            unsafe fn dma_dealloc(bus: u64, cpu: NonNull<u8>, pages: usize) {
                unsafe { FakeHal::dma_dealloc(bus, cpu, pages) };
            }
        }
        fn check<const N: usize>() {
            assert_eq!(FakeHal::live_allocations(), 0);
            let mut aperture = vec![0u32; WINDOW_BYTES / 4];
            // SAFETY: aperture remains live until init and any teardown finish.
            let bus = unsafe {
                WindowBus::<FailAfter<N>>::new(RegisterWindow::from_mapped(
                    aperture.as_mut_ptr() as usize,
                    WINDOW_BYTES,
                ))
            };
            assert!(matches!(
                IgcNic::<FailAfter<N>, QS>::init(bus, &station()),
                Err(DevError::NoMemory)
            ));
            assert_eq!(
                FakeHal::live_allocations(),
                0,
                "allocation failure after {N} successes"
            );
            assert!(
                aperture.iter().all(|word| *word == 0),
                "nothing was programmed"
            );
        }
        check::<0>();
        check::<1>();
        check::<2>();
        check::<3>();
    }

    #[test]
    fn a_dma_engine_that_does_not_stop_keeps_its_allocations() {
        let mut harness = Harness::new();
        let allocations: Vec<_> = harness
            .nic
            .allocations
            .iter()
            .map(|a| (a.bus, a.cpu, a.pages))
            .collect();
        harness.aperture[named("IGC_STATUS").offset() as usize / 4] =
            bits::STATUS_GIO_MASTER_ENABLE;
        drop(harness);
        assert_eq!(FakeHal::live_allocations(), 4);
        assert_eq!(
            FakeHal::delay_count(),
            bits::MASTER_DISABLE_TIMEOUT as usize
        );
        // No device exists in this test. Release the deliberately retained
        // buffers after observing the failure policy, so the test does not leak.
        for (bus, cpu, pages) in allocations {
            unsafe { FakeHal::dma_dealloc(bus, cpu, pages) };
        }
        assert_eq!(FakeHal::live_allocations(), 0);
    }

    #[test]
    fn the_stats_sentence_names_every_counter() {
        let harness = Harness::new();
        let text = harness.nic.stats().describe();
        for field in [
            "tx ",
            "rx ",
            "recycled",
            "dropped",
            "foreign pointers",
            "transmit ring full",
            "register writes refused",
        ] {
            assert!(text.contains(field), "{field} is missing from {text}");
        }
    }

    #[test]
    fn the_fake_bus_and_the_real_window_agree_about_what_may_be_written() {
        // A small check that the harness is honest: the driver's writes all
        // went through a window over ordinary memory, and the two registers it
        // must not be able to write are still read-only in the table.
        let harness = Harness::new();
        assert!(regs::named("IGC_STATUS").unwrap().is_readable());
        assert!(!regs::named("IGC_STATUS").unwrap().is_writable());
        let _ = FakeBus::new();
        assert!(
            harness.register("IGC_CTRL") == 0,
            "the driver never writes CTRL here"
        );
    }
}
