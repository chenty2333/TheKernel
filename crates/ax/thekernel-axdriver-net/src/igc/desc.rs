//! Descriptors and the two ring cursors.
//!
//! A descriptor is sixteen bytes of DMA memory the hardware reads (transmit)
//! or writes (receive).  Everything about the format is cited from the vendor
//! driver: the layouts are `igc_base.h` `union igc_adv_tx_desc` (`:14-25`) and
//! `union igc_adv_rx_desc` (`:57-86`), the transmit command bits are
//! `igc_base.h:36-52`, and the receive status bits are `igc_defines.h:304` and
//! `:366`.
//!
//! This module is where the parts of a NIC driver that *can* be proven on a
//! machine with no NIC live: the byte layout of a 16-byte structure, the
//! packing of a length and a set of flags into one word, and the cursor
//! arithmetic that wraps a ring without ever handing the hardware a descriptor
//! the driver still owns.  It has no MMIO, no allocation and no device.
//!
//! # The two conventions that are easy to get wrong
//!
//! * **One descriptor is always left unused.**  The vendor driver's
//!   `igc_desc_unused` (`igc.h:651-657`) computes the free space as
//!   `count + next_to_clean - next_to_use - 1`, and its own comment in
//!   `igc_configure` (`igc_main.c:4042-4045`) says why: leaving one descriptor
//!   unused is what makes "full" and "empty" distinguishable when head and
//!   tail are equal.  Both rings here have `count - 1` usable descriptors.
//! * **The tail register is one past the last descriptor handed over.**  In
//!   `igc_alloc_rx_buffers` (`igc_main.c:2229-2292`) the driver fills
//!   descriptors from `next_to_use`, then writes `i` -- the *incremented*
//!   index -- to the tail register, and `igc_configure_rx_ring`
//!   (`igc_main.c:625`) starts it at zero with an empty ring.  The same holds
//!   for transmit: `igc_tx_map` writes the index after the last descriptor of
//!   the frame.  So the hardware owns `[head, tail)` and the driver owns the
//!   rest.  This is an inference from the vendor code rather than a datasheet
//!   statement -- the datasheet is not available to this project -- and it is
//!   the convention the ring cursors below implement.
//!
//! # What the receive path treats as "written back"
//!
//! The vendor driver's hot path does not test the descriptor-done bit: it
//! reads the length and treats a non-zero length as "the hardware has written
//! this descriptor" (`igc_main.c:2601-2604`), using `IGC_RXD_STAT_EOP` for
//! end-of-packet and `IGC_RXD_STAT_DD` only in a diagnostic dump
//! (`igc_dump.c:271`).  This driver does the same, and additionally reports
//! whether the done bit was set, because a length without a done bit is worth
//! seeing if it ever happens.

use core::{
    ptr::NonNull,
    sync::atomic::{Ordering, compiler_fence},
};

use super::regs::bits;

/// The size of one descriptor, in bytes.  `igc_base.h:14-25` and `:57-86`:
/// four 32-bit words either way.
pub const DESCRIPTOR_BYTES: usize = 16;

/// The largest frame this driver handles.
///
/// The vendor driver's `max_frame_size` is `mtu + ETH_HLEN + ETH_FCS_LEN +
/// VLAN_HLEN` (`igc_main.c:4922`), which with the standard 1500-byte MTU is
/// 1522 bytes.  The largest frame that can actually arrive is 1518: the
/// hardware strips the four-byte FCS because `RCTL.SECRC` is set, and the
/// driver never subtracts it.
pub const MAX_FRAME_BYTES: usize = 1522;

/// The receive buffer size, `IGC_RXBUFFER_2048` (`igc.h:453`), which is what
/// `igc_configure_rx_ring` puts in the descriptor's buffer-size field
/// (`igc_main.c:625`).
pub const RX_BUFFER_BYTES: usize = 2048;

/// The header buffer size, `IGC_RX_HDR_LEN` = `IGC_RXBUFFER_256`
/// (`igc.h:457`).  In one-buffer mode the header buffer is not used, but the
/// field is still programmed, as the vendor driver programs it.
pub const RX_HEADER_BYTES: usize = 256;

/// `IGC_MAX_DATA_PER_TXD`, `BIT(IGC_MAX_TXD_PWR)` = 32768 (`igc.h:545-546`).
///
/// A frame larger than this would need more than one descriptor.  This driver
/// refuses such a frame rather than splitting it: the largest frame it accepts
/// is [`MAX_FRAME_BYTES`], so the case cannot arise, and a test pins that.
pub const TX_MAX_DATA_PER_DESCRIPTOR: usize = 1 << 15;

/// Which direction a ring belongs to, for the errors a ring can report.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RingDirection {
    /// The transmit ring.
    Transmit,
    /// The receive ring.
    Receive,
}

impl RingDirection {
    /// The word a report or an error uses.
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Transmit => "transmit",
            Self::Receive => "receive",
        }
    }
}

/// Why a ring operation could not be performed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RingError {
    /// The ring is full: the driver must wait for the hardware to catch up.
    Full(RingDirection),
    /// The ring is empty: there is nothing to take.
    Empty(RingDirection),
    /// The descriptor at this index is not one the hardware has written back.
    NotReady(RingDirection),
    /// The descriptor or the buffer it points at is not in the ring this
    /// driver allocated, which means a pointer was mixed up somewhere.
    ForeignBuffer(RingDirection),
    /// A descriptor's length field is larger than the buffer it names.
    OverlongFrame { length: usize, buffer: usize },
    /// A receive descriptor was written back without the end-of-packet bit.
    NotEndOfPacket,
}

impl RingError {
    /// The sentence the log and the report use.
    pub fn describe(&self) -> alloc::string::String {
        use alloc::format;
        match self {
            Self::Full(direction) => format!("the {} ring is full", direction.describe()),
            Self::Empty(direction) => format!("the {} ring is empty", direction.describe()),
            Self::NotReady(direction) => format!(
                "the {} descriptor has not been written back yet",
                direction.describe()
            ),
            Self::ForeignBuffer(direction) => format!(
                "the buffer handed back does not belong to the {} ring",
                direction.describe()
            ),
            Self::OverlongFrame { length, buffer } => format!(
                "a descriptor reports a {length} byte frame, which does not fit the {buffer} byte \
                 buffer it names"
            ),
            Self::NotEndOfPacket => alloc::string::String::from(
                "a receive descriptor was written back without the end-of-packet bit, so the \
                 frame spans descriptors and this driver's single-buffer receive path cannot \
                 assemble it",
            ),
        }
    }
}

/// The descriptor memory of one ring.
///
/// The memory is DMA-coherent and is owned by this value for as long as it
/// lives; every access is a volatile 32-bit load or store, because the device
/// reads and writes the same bytes.  Every accessor bounds-checks its index:
/// an out-of-range descriptor is refused rather than followed, which costs one
/// comparison per descriptor and is worth it in the one place where a wrong
/// index is a write into somebody else's memory.
#[derive(Debug)]
pub struct DescriptorMemory {
    base: NonNull<u8>,
    count: usize,
}

// SAFETY: the memory is DMA-coherent memory owned by whoever holds this value,
// and every access is a volatile 32-bit load or store at a bounded offset, so
// moving the handle between threads moves no aliasing reference.
unsafe impl Send for DescriptorMemory {}
// SAFETY: as above; `&DescriptorMemory` only reaches the memory through
// volatile accesses, which are atomic with respect to the device and to other
// 32-bit accesses.
unsafe impl Sync for DescriptorMemory {}

impl DescriptorMemory {
    /// Take a ring over `count` descriptors of DMA memory at `base`.
    ///
    /// # Safety
    ///
    /// `base .. base + count * DESCRIPTOR_BYTES` must be a live, writable
    /// region of DMA-coherent memory that nothing else in the kernel aliases
    /// as ordinary data, and it must stay that way for as long as this value
    /// (or anything derived from it) is used.
    pub const unsafe fn new(base: NonNull<u8>, count: usize) -> Self {
        Self { base, count }
    }

    /// How many descriptors the ring holds.
    pub const fn count(&self) -> usize {
        self.count
    }

    /// A pointer to one descriptor's first byte, or `None` when `index` is not
    /// in the ring.
    fn word(&self, index: usize, word: usize) -> Option<NonNull<u32>> {
        if index >= self.count || word * 4 + 4 > DESCRIPTOR_BYTES {
            return None;
        }
        // SAFETY: the index and word were just checked to lie inside the
        // region this value promises is live DMA memory, and the arithmetic
        // cannot overflow because both factors are small and bounded.
        Some(unsafe {
            NonNull::new_unchecked(
                self.base
                    .as_ptr()
                    .add(index * DESCRIPTOR_BYTES + word * 4)
                    .cast::<u32>(),
            )
        })
    }

    /// Zero every descriptor.
    ///
    /// The vendor driver relies on `dma_alloc_coherent` having returned zeroed
    /// memory for everything except the first descriptor's length field
    /// (`igc_main.c:625`).  Doing it explicitly costs one pass at init and
    /// means the `hdr_addr` word of a single-buffer receive descriptor -- which
    /// the driver never writes again -- is known to be zero.
    pub fn zero(&mut self) {
        for index in 0..self.count {
            for word in 0..DESCRIPTOR_BYTES / 4 {
                if let Some(address) = self.word(index, word) {
                    // SAFETY: `word` returned a pointer inside the ring.
                    unsafe { core::ptr::write_volatile(address.as_ptr(), 0) };
                }
            }
        }
        compiler_fence(Ordering::SeqCst);
    }

    /// The first byte of the ring, for a test that plays the device's side.
    #[cfg(test)]
    pub(crate) fn test_base(&self) -> NonNull<u8> {
        self.base
    }

    /// Write a transmit descriptor's data fields.
    ///
    /// The order is the vendor driver's: the buffer address first, then the
    /// command word, then the offload word (`igc_tx_map`, `igc_main.c:1316`).
    /// The write-back `status` word is left alone; the hardware owns it.
    pub fn write_tx(
        &mut self,
        index: usize,
        buffer: u64,
        command_length: u32,
        offload: u32,
    ) -> bool {
        let (Some(low), Some(high), Some(command), Some(offload_word)) = (
            self.word(index, 0),
            self.word(index, 1),
            self.word(index, 2),
            self.word(index, 3),
        ) else {
            return false;
        };
        compiler_fence(Ordering::Release);
        // SAFETY: every pointer was bounds-checked above and the region is
        // live DMA memory by this value's contract.
        unsafe {
            core::ptr::write_volatile(low.as_ptr(), buffer as u32);
            core::ptr::write_volatile(high.as_ptr(), (buffer >> 32) as u32);
            core::ptr::write_volatile(command.as_ptr(), command_length);
            core::ptr::write_volatile(offload_word.as_ptr(), offload);
        }
        compiler_fence(Ordering::SeqCst);
        true
    }

    /// The transmit descriptor's write-back status word, or `None`.
    pub fn tx_status(&self, index: usize) -> Option<u32> {
        let address = self.word(index, 3)?;
        // SAFETY: bounds-checked above.
        let value = unsafe { core::ptr::read_volatile(address.as_ptr()) };
        compiler_fence(Ordering::Acquire);
        Some(value)
    }

    /// Whether the hardware has finished with this transmit descriptor.
    ///
    /// `IGC_TXD_STAT_DD` is bit 0 of the write-back status word
    /// (`igc_defines.h:315`).
    pub fn tx_done(&self, index: usize) -> bool {
        self.tx_status(index).is_some_and(|status| status & bits::TXD_STAT_DD != 0)
    }

    /// Point a receive descriptor at a buffer.
    ///
    /// The header-address word is written as zero: in one-buffer mode
    /// (`SRRCTL.DESCTYPE_ADV_ONEBUF`) there is no header buffer, and the
    /// vendor driver never writes the word at all, relying on the zeroed
    /// allocation.
    pub fn write_rx_buffer(&mut self, index: usize, buffer: u64) -> bool {
        let (Some(low), Some(high), Some(length)) =
            (self.word(index, 0), self.word(index, 1), self.word(index, 3))
        else {
            return false;
        };
        compiler_fence(Ordering::Release);
        // SAFETY: bounds-checked above.
        unsafe {
            core::ptr::write_volatile(low.as_ptr(), buffer as u32);
            core::ptr::write_volatile(high.as_ptr(), (buffer >> 32) as u32);
            // The write-back length field shares the fourth word with the VLAN
            // tag, and a stale length is indistinguishable from a fresh
            // packet: clearing it here is what makes "length != 0" mean "the
            // hardware just wrote this descriptor".  The vendor driver clears
            // it for the descriptor after the ones it fills
            // (`igc_main.c:2276`); clearing it per descriptor is the same
            // guarantee and also holds when buffers are handed back out of
            // order.
            core::ptr::write_volatile(length.as_ptr(), 0);
        }
        compiler_fence(Ordering::SeqCst);
        true
    }

    /// The receive descriptor's write-back length field, or `None`.
    ///
    /// This is the vendor driver's written-back test: a non-zero length means
    /// the hardware has finished with the descriptor (`igc_main.c:2601`).
    pub fn rx_length(&self, index: usize) -> Option<u16> {
        let address = self.word(index, 3)?;
        // The length is the low half of the fourth word.
        // SAFETY: bounds-checked above.
        let value = unsafe { core::ptr::read_volatile(address.as_ptr()) };
        compiler_fence(Ordering::Acquire);
        Some(value as u16)
    }

    /// The receive descriptor's write-back status and error word, or `None`.
    ///
    /// `IGC_RXD_STAT_EOP` and `IGC_RXD_STAT_DD` live in its low byte
    /// (`igc_defines.h:304`, `:366`).
    pub fn rx_status_error(&self, index: usize) -> Option<u32> {
        let address = self.word(index, 2)?;
        // SAFETY: bounds-checked above.
        let value = unsafe { core::ptr::read_volatile(address.as_ptr()) };
        compiler_fence(Ordering::Acquire);
        Some(value)
    }

    /// The receive descriptor's write-back VLAN tag, or `None`.
    ///
    /// This driver does not use it -- the VLAN tag is left in the frame, since
    /// `RCTL` is programmed without VLAN stripping -- but a report of what the
    /// hardware wrote is worth having when the numbers are being checked.
    pub fn rx_vlan(&self, index: usize) -> Option<u16> {
        let address = self.word(index, 3)?;
        // SAFETY: bounds-checked above.
        let value = unsafe { core::ptr::read_volatile(address.as_ptr()) };
        Some((value >> 16) as u16)
    }
}

/// What one receive descriptor says after the hardware has written it back.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReceivedFrame {
    /// The frame length the hardware reports, which excludes the Ethernet CRC
    /// because `RCTL.SECRC` is set.
    pub length: usize,
    /// `IGC_RXD_STAT_EOP`.
    pub end_of_packet: bool,
    /// `IGC_RXD_STAT_DD`.  Not required by the vendor driver's hot path; the
    /// driver reports it so that a length without it is visible.
    pub done: bool,
    /// The VLAN tag word the hardware left, if any.
    pub vlan: u16,
}

/// Decode a receive descriptor the hardware has written back.
///
/// Returns `Err(RingError::NotReady)` when the descriptor has not been written
/// back, which is the normal "nothing to receive" answer.
pub fn decode_received(memory: &DescriptorMemory, index: usize) -> Result<ReceivedFrame, RingError> {
    let length = memory
        .rx_length(index)
        .ok_or(RingError::NotReady(RingDirection::Receive))?;
    if length == 0 {
        return Err(RingError::NotReady(RingDirection::Receive));
    }
    let status = memory
        .rx_status_error(index)
        .ok_or(RingError::NotReady(RingDirection::Receive))?;
    let length = usize::from(length);
    if length > RX_BUFFER_BYTES {
        return Err(RingError::OverlongFrame {
            length,
            buffer: RX_BUFFER_BYTES,
        });
    }
    let frame = ReceivedFrame {
        length,
        end_of_packet: status & bits::RXD_STAT_EOP != 0,
        done: status & bits::RXD_STAT_DD != 0,
        vlan: memory.rx_vlan(index).unwrap_or(0),
    };
    if !frame.end_of_packet {
        return Err(RingError::NotEndOfPacket);
    }
    Ok(frame)
}

/// Pack a frame length and the transmit command bits into the
/// `cmd_type_len` word.
///
/// `igc_tx_cmd_type` (`igc_main.c:1259-1293`) builds
/// `DTYP_DATA | DCMD_DEXT | DCMD_IFCS` and `igc_tx_map` adds
/// `size | IGC_TXD_DCMD`, where `IGC_TXD_DCMD` is `DCMD_EOP | DCMD_RS`
/// (`igc.h:754`).  So a complete frame in one descriptor is
/// `0x2b30_0000 | length`:
///
/// * `DTYP_DATA` (0x0030_0000) -- an advanced data descriptor;
/// * `DCMD_DEXT` (0x2000_0000) -- the descriptor is an advanced one;
/// * `DCMD_IFCS` (0x0200_0000) -- insert the frame check sequence, which is
///   what makes the MAC append the Ethernet CRC;
/// * `DCMD_EOP` (0x0100_0000) -- end of packet;
/// * `DCMD_RS` (0x0800_0000) -- report status, which is what makes the
///   hardware write the done bit back so the buffer can be recycled.
///
/// The VLAN, TCP-segmentation and timestamp bits are deliberately not set:
/// this driver does no VLAN insertion, no segmentation offload and no
/// timestamping, so a frame carries exactly the bytes the stack wrote.
pub const fn tx_command_length(length: usize) -> u32 {
    (bits::ADVTXD_DTYP_DATA
        | bits::ADVTXD_DCMD_DEXT
        | bits::ADVTXD_DCMD_IFCS
        | bits::ADVTXD_DCMD_EOP
        | bits::ADVTXD_DCMD_RS)
        | (length as u32 & 0x0000_ffff)
}

/// Pack a frame length into the transmit descriptor's `olinfo_status` word.
///
/// `igc_tx_olinfo_status` (`igc_main.c:1295-1314`) puts the payload length at
/// `IGC_ADVTXD_PAYLEN_SHIFT` (`igc_base.h:52`) and adds checksum-insert bits
/// when the stack asked for them.  This driver never asks: the network stack's
/// packet context reports software checksums, so the two `POPTS` bits stay
/// clear and the hardware inserts nothing but the CRC.
pub const fn tx_offload_status(length: usize) -> u32 {
    (length as u32) << bits::ADVTXD_PAYLEN_SHIFT
}

/// The transmit ring's two cursors.
///
/// `next_to_clean` is the first descriptor the driver has not reclaimed;
/// `next_to_use` is the descriptor it will fill next.  The hardware owns
/// `[next_to_clean, next_to_use)`; the driver owns the rest, except for the
/// one descriptor that is always left unused.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TxRing {
    count: usize,
    next_to_clean: usize,
    next_to_use: usize,
}

impl TxRing {
    /// A ring of `count` descriptors, empty.
    pub const fn new(count: usize) -> Self {
        Self {
            count,
            next_to_clean: 0,
            next_to_use: 0,
        }
    }

    /// How many descriptors the ring holds.
    pub const fn count(&self) -> usize {
        self.count
    }

    /// The descriptor the driver will fill next.
    pub const fn next_to_use(&self) -> usize {
        self.next_to_use
    }

    /// The first descriptor the driver has not reclaimed.
    pub const fn next_to_clean(&self) -> usize {
        self.next_to_clean
    }

    /// How many descriptors are between the driver's cursor and its own
    /// reclamation cursor: the ones the hardware owns plus the ones it has
    /// finished with.
    pub const fn outstanding(&self) -> usize {
        (self.next_to_use + self.count - self.next_to_clean) % self.count
    }

    /// How many descriptors the driver may still hand over.
    ///
    /// This is the vendor driver's `igc_desc_unused` (`igc.h:651-657`),
    /// written as the distance around the ring: `count - 1 - outstanding`.
    /// The `count - 1` is the descriptor that is always left unused, which is
    /// what makes "full" and "empty" distinguishable when the two cursors are
    /// equal (`igc_main.c:4042-4045`).
    pub const fn unused(&self) -> usize {
        self.count - 1 - self.outstanding()
    }

    /// Whether a descriptor can be handed over.
    pub const fn has_room(&self) -> bool {
        self.unused() > 0
    }

    /// Take the next descriptor to fill, advancing the driver's cursor.
    ///
    /// Returns `Err(Full)` when the ring has no room, which is the caller's
    /// signal to wait for the hardware rather than to overwrite a descriptor
    /// the hardware still owns.
    pub fn take(&mut self) -> Result<usize, RingError> {
        if !self.has_room() {
            return Err(RingError::Full(RingDirection::Transmit));
        }
        let index = self.next_to_use;
        self.next_to_use = (self.next_to_use + 1) % self.count;
        Ok(index)
    }

    /// Give the descriptor at `next_to_clean` back to the driver.
    ///
    /// Returns the index that was released, or `Err(Empty)` when every
    /// descriptor is already the driver's.
    pub fn release(&mut self) -> Result<usize, RingError> {
        if self.next_to_clean == self.next_to_use {
            return Err(RingError::Empty(RingDirection::Transmit));
        }
        let index = self.next_to_clean;
        self.next_to_clean = (self.next_to_clean + 1) % self.count;
        Ok(index)
    }

    /// The value the tail register takes after the last handover.
    ///
    /// The tail is `next_to_use`: the hardware owns everything before it.
    pub const fn tail(&self) -> usize {
        self.next_to_use
    }

    /// Forget every handover, as if the ring had just been configured.
    pub fn reset(&mut self) {
        self.next_to_clean = 0;
        self.next_to_use = 0;
    }
}

/// The receive ring's two cursors.
///
/// The receive ring needs no more state than the transmit ring, provided the
/// driver fills it the way the vendor driver does: the descriptor to fill is
/// always `next_to_use`, and the free space is the ring distance to
/// `next_to_clean`.  A buffer handed back by the network stack is put on the
/// driver's free list and the *ring* is refilled from the tail, so a buffer
/// that comes back late simply means the tail does not move until it does --
/// rather than the tail moving over a descriptor whose buffer is still owned
/// by somebody else, which is the one mistake here that would corrupt memory
/// rather than a packet.
///
/// This mirrors `igc_alloc_rx_buffers` (`igc_main.c:2229-2292`): fill from
/// `next_to_use`, clear the length of the descriptor being armed, then write
/// the incremented index to the tail register.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RxRing {
    count: usize,
    next_to_clean: usize,
    next_to_use: usize,
}

impl RxRing {
    /// A ring of `count` descriptors with nothing armed.
    pub const fn new(count: usize) -> Self {
        Self {
            count,
            next_to_clean: 0,
            next_to_use: 0,
        }
    }

    /// How many descriptors the ring holds.
    pub const fn count(&self) -> usize {
        self.count
    }

    /// The descriptor the driver will read next.
    pub const fn next_to_clean(&self) -> usize {
        self.next_to_clean
    }

    /// The descriptor the driver will arm next, which is also the value the
    /// tail register holds.
    pub const fn tail(&self) -> usize {
        self.next_to_use
    }

    /// How many descriptors are between the tail and the driver's own cursor.
    pub const fn outstanding(&self) -> usize {
        (self.next_to_use + self.count - self.next_to_clean) % self.count
    }

    /// How many descriptors can still be armed, leaving one unused.
    pub const fn unused(&self) -> usize {
        self.count - 1 - self.outstanding()
    }

    /// Whether another descriptor can be armed.
    pub const fn has_room(&self) -> bool {
        self.unused() > 0
    }

    /// Arm the descriptor at the tail with a buffer and advance the tail.
    ///
    /// Returns the descriptor index that was armed.  `Err(Full)` means the
    /// ring holds as many buffers as it may, which is the caller's signal to
    /// stop until a buffer comes back.
    pub fn fill(&mut self, memory: &mut DescriptorMemory, buffer: u64) -> Result<usize, RingError> {
        if !self.has_room() {
            return Err(RingError::Full(RingDirection::Receive));
        }
        let index = self.next_to_use;
        if !memory.write_rx_buffer(index, buffer) {
            return Err(RingError::ForeignBuffer(RingDirection::Receive));
        }
        self.next_to_use = (self.next_to_use + 1) % self.count;
        Ok(index)
    }

    /// Take the descriptor at `next_to_clean`, if the hardware wrote it back.
    ///
    /// Returns `Err(NotReady)` for the ordinary "no frame waiting" case, which
    /// the caller distinguishes from the other errors.
    pub fn take(&mut self, memory: &DescriptorMemory) -> Result<(usize, ReceivedFrame), RingError> {
        if self.outstanding() == 0 {
            return Err(RingError::Empty(RingDirection::Receive));
        }
        let index = self.next_to_clean;
        let frame = decode_received(memory, index)?;
        self.next_to_clean = (self.next_to_clean + 1) % self.count;
        Ok((index, frame))
    }

    /// Drop the descriptor at `next_to_clean` without decoding it.
    ///
    /// A descriptor that describes a frame this driver cannot use still has to
    /// be reclaimed: leaving the cursor on it would stall the ring for ever,
    /// and the buffer it names is the driver's again either way.
    pub fn discard(&mut self) -> Result<usize, RingError> {
        if self.outstanding() == 0 {
            return Err(RingError::Empty(RingDirection::Receive));
        }
        let index = self.next_to_clean;
        self.next_to_clean = (self.next_to_clean + 1) % self.count;
        Ok(index)
    }

    /// Forget every handover, as if the ring had just been configured.
    pub fn reset(&mut self) {
        self.next_to_clean = 0;
        self.next_to_use = 0;
    }
}

/// A DMA-coherent buffer region split into equal slots.
///
/// The receive and transmit rings each get one of these: a single
/// allocation of `slots * slot_bytes` bytes, carved into slots whose bus
/// address is `bus_base + index * slot_bytes`.  A pointer into the region is
/// how a [`super::NetBufPtr`](crate::NetBufPtr) finds its way back to the slot
/// it came from, without a per-packet allocation to keep the mapping in.
#[derive(Clone, Copy, Debug)]
pub struct BufferPool {
    cpu_base: NonNull<u8>,
    bus_base: u64,
    slots: usize,
    slot_bytes: usize,
}

// SAFETY: the pool is a description of a region of DMA-coherent memory -- a
// base address, a bus address and a geometry -- and every access it offers
// either computes an address or performs an initialised read through
// `read_volatile`.  It owns nothing and frees nothing, so moving or sharing
// the description cannot alias anything that Rust's aliasing rules protect.
unsafe impl Send for BufferPool {}
// SAFETY: as above.  The slots are written by the network stack through the
// pointers it is handed, one buffer at a time and never two at once: a slot is
// only handed out once, and cannot be handed out again until it comes back.
unsafe impl Sync for BufferPool {}

impl BufferPool {
    /// Take a pool over an allocation that is already mapped.
    ///
    /// # Safety
    ///
    /// `cpu_base .. cpu_base + slots * slot_bytes` must be a live, writable
    /// region of DMA-coherent memory whose bus address is `bus_base`, and
    /// nothing else in the kernel may alias it for as long as this value or
    /// any pointer derived from it is used.
    pub const unsafe fn new(
        cpu_base: NonNull<u8>,
        bus_base: u64,
        slots: usize,
        slot_bytes: usize,
    ) -> Self {
        Self {
            cpu_base,
            bus_base,
            slots,
            slot_bytes,
        }
    }

    /// How many slots the pool holds.
    pub const fn slots(&self) -> usize {
        self.slots
    }

    /// How many bytes each slot holds.
    pub const fn slot_bytes(&self) -> usize {
        self.slot_bytes
    }

    /// The bus address of a slot's buffer, or `None` for a slot that is not in
    /// the pool.
    pub fn bus_address(&self, index: usize) -> Option<u64> {
        if index >= self.slots {
            return None;
        }
        Some(self.bus_base + (index * self.slot_bytes) as u64)
    }

    /// The CPU address of a slot's buffer, or `None`.
    pub fn cpu_address(&self, index: usize) -> Option<NonNull<u8>> {
        if index >= self.slots {
            return None;
        }
        // SAFETY: the index was bounds-checked, so the offset is inside the
        // region this value promises is live.
        Some(unsafe { NonNull::new_unchecked(self.cpu_base.as_ptr().add(index * self.slot_bytes)) })
    }

    /// Which slot a pointer into the pool names, or `None` when it points
    /// outside the pool or at a slot boundary that is not one.
    ///
    /// This is how a buffer that came back from the network stack is turned
    /// into a slot index: anything that is not exactly a slot start is
    /// refused, so a stale or foreign pointer becomes a reported error rather
    /// than a write into the wrong buffer.
    pub fn slot_of(&self, pointer: NonNull<u8>) -> Option<usize> {
        let base = self.cpu_base.as_ptr() as usize;
        let address = pointer.as_ptr() as usize;
        if address < base {
            return None;
        }
        let offset = address - base;
        let limit = self.slots * self.slot_bytes;
        if offset >= limit || !offset.is_multiple_of(self.slot_bytes) {
            return None;
        }
        Some(offset / self.slot_bytes)
    }
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};

    use super::*;

    /// Descriptor memory standing in for DMA memory, plus the allocation that
    /// keeps it alive.
    struct Scratch {
        words: Vec<u32>,
    }

    impl Scratch {
        fn new(descriptors: usize) -> Self {
            Self {
                words: vec![0; descriptors * DESCRIPTOR_BYTES / 4],
            }
        }

        fn memory(&mut self) -> DescriptorMemory {
            let pointers = descriptors(&mut self.words);
            // SAFETY: the buffer is live, is exactly `descriptors * 16` bytes
            // long, and outlives the returned value.
            unsafe { DescriptorMemory::new(pointers, self.words.len() * 4 / DESCRIPTOR_BYTES) }
        }
    }

    fn descriptors(words: &mut [u32]) -> NonNull<u8> {
        NonNull::new(words.as_mut_ptr().cast::<u8>()).expect("a live buffer")
    }

    #[test]
    fn a_transmit_descriptor_packs_the_address_the_length_and_the_command_bits() {
        let mut scratch = Scratch::new(4);
        let mut memory = scratch.memory();
        assert!(memory.write_tx(
            1,
            0x0000_0001_f7a0_4000,
            tx_command_length(1514),
            tx_offload_status(1514),
        ));
        // Word for word, in the layout igc_base.h:14-25 gives.
        assert_eq!(scratch.words[4], 0xf7a0_4000, "buffer address, low dword");
        assert_eq!(scratch.words[5], 0x0000_0001, "buffer address, high dword");
        assert_eq!(
            scratch.words[6],
            0x2b30_0000 | 1514,
            "DTYP_DATA | DEXT | IFCS | EOP | RS | length",
        );
        assert_eq!(scratch.words[7], 1514 << 14, "PAYLEN in olinfo_status");
        // The write-back status word is the *same* word as `olinfo_status`:
        // the hardware overwrites it when it finishes with the descriptor.
        // Before that, reading it returns what the driver wrote, which is why
        // the done test is bit 0 rather than "the word changed" -- and why
        // nothing this driver writes can ever look like a completed
        // descriptor: `PAYLEN` starts at bit 14.
        assert_eq!(memory.tx_status(1), Some(1514 << 14));
        assert!(!memory.tx_done(1));
        assert_eq!((1514u32 << 14) & bits::TXD_STAT_DD, 0);
        // The hardware writes the done bit and the driver sees it.
        scratch.words[1 * 4 + 3] = bits::TXD_STAT_DD;
        assert!(memory.tx_done(1));
    }

    #[test]
    fn the_transmit_command_bits_are_exactly_the_vendor_drivers() {
        assert_eq!(tx_command_length(0), 0x2b30_0000);
        // Each bit, named, so a reader can check the sum.
        assert_eq!(tx_command_length(0) & bits::ADVTXD_DTYP_DATA, 0x0030_0000);
        assert_eq!(tx_command_length(0) & bits::ADVTXD_DCMD_DEXT, 0x2000_0000);
        assert_eq!(tx_command_length(0) & bits::ADVTXD_DCMD_IFCS, 0x0200_0000);
        assert_eq!(tx_command_length(0) & bits::ADVTXD_DCMD_EOP, 0x0100_0000);
        assert_eq!(tx_command_length(0) & bits::ADVTXD_DCMD_RS, 0x0800_0000);
        // The bits this driver must never set, because it implements none of
        // those features.
        assert_eq!(tx_command_length(1514) & bits::ADVTXD_DCMD_VLE, 0);
        assert_eq!(tx_command_length(1514) & bits::ADVTXD_DCMD_TSE, 0);
        assert_eq!(tx_command_length(1514) & bits::ADVTXD_MAC_TSTAMP, 0);
        // The length is the low 16 bits.
        assert_eq!(tx_command_length(1514) & 0xffff, 1514);
        assert_eq!(tx_offload_status(1514), 1514 << 14);
        // The checksum-insert bits stay clear: no offload is requested.
        assert_eq!(tx_offload_status(1514) & (bits::TXD_POPTS_IXSM << 8), 0);
        assert_eq!(tx_offload_status(1514) & (bits::TXD_POPTS_TXSM << 8), 0);
    }

    #[test]
    fn the_largest_frame_fits_one_descriptor_and_one_buffer() {
        assert!(MAX_FRAME_BYTES <= TX_MAX_DATA_PER_DESCRIPTOR);
        assert!(MAX_FRAME_BYTES <= RX_BUFFER_BYTES);
        // The vendor bound: mtu 1500 + Ethernet header + FCS + VLAN tag.
        assert_eq!(MAX_FRAME_BYTES, 1500 + 14 + 4 + 4);
    }

    #[test]
    fn a_transmit_descriptor_reports_done_only_when_the_hardware_says_so() {
        let mut scratch = Scratch::new(4);
        let memory = scratch.memory();
        assert!(!memory.tx_done(2));
        scratch.words[2 * 4 + 3] = bits::TXD_STAT_DD;
        assert!(memory.tx_done(2));
        // Other status bits are not the done bit.
        scratch.words[2 * 4 + 3] = 0x0000_0002;
        assert!(!memory.tx_done(2));
    }

    #[test]
    fn a_receive_descriptor_is_armed_with_a_buffer_and_the_length_is_cleared() {
        let mut scratch = Scratch::new(4);
        let mut memory = scratch.memory();
        // A stale length from a previous packet must not survive arming.
        scratch.words[3 * 4 + 3] = 0x0000_0600;
        assert!(memory.write_rx_buffer(3, 0x0000_0002_0000_0800));
        assert_eq!(scratch.words[3 * 4], 0x0000_0800);
        assert_eq!(scratch.words[3 * 4 + 1], 0x0000_0002);
        assert_eq!(scratch.words[3 * 4 + 2], 0, "hdr_addr is zero: one buffer");
        assert_eq!(scratch.words[3 * 4 + 3], 0, "the write-back length is clear");
        assert_eq!(memory.rx_length(3), Some(0));
        assert_eq!(memory.rx_vlan(3), Some(0));
    }

    #[test]
    fn a_written_back_receive_descriptor_decodes_the_way_the_vendor_driver_reads_it() {
        let mut scratch = Scratch::new(4);
        let mut memory = scratch.memory();
        memory.write_rx_buffer(1, 0x1000);
        // The hardware writes: status_error at word 2, length and VLAN in
        // word 3.
        scratch.words[1 * 4 + 2] = bits::RXD_STAT_DD | bits::RXD_STAT_EOP;
        scratch.words[1 * 4 + 3] = 1514 | (0x0064 << 16);
        let frame = decode_received(&memory, 1).expect("a written-back descriptor");
        assert_eq!(frame.length, 1514);
        assert!(frame.end_of_packet);
        assert!(frame.done);
        assert_eq!(frame.vlan, 0x0064);
    }

    #[test]
    fn a_descriptor_the_hardware_has_not_written_is_not_a_frame() {
        let mut scratch = Scratch::new(4);
        let memory = scratch.memory();
        assert_eq!(
            decode_received(&memory, 0),
            Err(RingError::NotReady(RingDirection::Receive)),
        );
        // A length but no end-of-packet bit is a reported error, not a frame:
        // this driver's single-buffer path cannot assemble a split packet.
        scratch.words[2] = bits::RXD_STAT_DD;
        scratch.words[3] = 60;
        assert_eq!(decode_received(&memory, 0), Err(RingError::NotEndOfPacket));
        // A length larger than the buffer it names is also refused.
        scratch.words[3] = (RX_BUFFER_BYTES + 1) as u32;
        assert_eq!(
            decode_received(&memory, 0),
            Err(RingError::OverlongFrame {
                length: RX_BUFFER_BYTES + 1,
                buffer: RX_BUFFER_BYTES,
            }),
        );
    }

    #[test]
    fn an_out_of_range_descriptor_index_is_refused_by_every_accessor() {
        let mut scratch = Scratch::new(4);
        let mut memory = scratch.memory();
        assert_eq!(memory.rx_length(4), None);
        assert_eq!(memory.rx_status_error(4), None);
        assert_eq!(memory.tx_status(4), None);
        assert!(!memory.tx_done(usize::MAX));
        assert!(!memory.write_tx(9, 0, 0, 0));
        assert!(!memory.write_rx_buffer(9, 0));
        // And nothing outside the ring was touched.
        assert!(scratch.words.iter().all(|word| *word == 0));
    }

    #[test]
    fn the_transmit_ring_leaves_one_descriptor_unused() {
        let mut ring = TxRing::new(4);
        assert_eq!(ring.unused(), 3);
        let mut taken = Vec::new();
        for _ in 0..3 {
            taken.push(ring.take().expect("room for three of four"));
        }
        assert_eq!(taken, [0, 1, 2]);
        assert_eq!(ring.tail(), 3, "the tail is one past the last handover");
        assert_eq!(ring.outstanding(), 3);
        assert!(!ring.has_room());
        assert_eq!(ring.take(), Err(RingError::Full(RingDirection::Transmit)));
        // The hardware finishes with them in order, so the driver reclaims
        // them in order and each reclamation makes room for one more.
        for expected in [0, 1, 2] {
            assert_eq!(ring.release(), Ok(expected));
        }
        assert_eq!(
            ring.release(),
            Err(RingError::Empty(RingDirection::Transmit)),
            "nothing is outstanding",
        );
        assert_eq!(ring.outstanding(), 0);
        assert_eq!(ring.unused(), 3);
    }

    #[test]
    fn the_transmit_ring_wraps_its_indices() {
        let mut ring = TxRing::new(4);
        for _ in 0..3 {
            ring.take().unwrap();
        }
        assert_eq!(ring.release(), Ok(0));
        assert_eq!(ring.unused(), 1, "reclaiming one descriptor frees one");
        // One more descriptor: index 3, which wraps the cursor to 0.
        assert_eq!(ring.take(), Ok(3));
        assert_eq!(ring.tail(), 0, "the tail wrapped");
        assert_eq!(ring.unused(), 0);
        assert_eq!(ring.release(), Ok(1));
        assert_eq!(ring.take(), Ok(0), "the index wrapped back to zero");
        assert_eq!(ring.next_to_use(), 1);
        assert_eq!(ring.next_to_clean(), 2);
        // Free space is `count - 1 - outstanding`, and outstanding is the
        // distance from the driver's cursor to the tail, which is the same
        // answer the vendor driver's `igc_desc_unused` gives.  Three
        // descriptors are in the hardware's hands: 2, 3 and 0.
        assert_eq!(ring.outstanding(), 3);
        assert_eq!(ring.unused(), 0);
        // The invariant holds at every step of a long run, wrapping twice.
        for _ in 0..20 {
            if ring.has_room() {
                let index = ring.take().expect("room");
                assert!(index < 4);
            }
            if ring.outstanding() > 0 {
                ring.release().expect("something outstanding");
            }
            assert_eq!(ring.unused(), 3 - ring.outstanding());
        }
    }

    #[test]
    fn the_receive_ring_fills_from_the_tail_and_leaves_one_descriptor_unused() {
        let mut scratch = Scratch::new(4);
        let mut memory = scratch.memory();
        let mut ring = RxRing::new(4);
        // A freshly configured ring fills to one short of its size, which is
        // what igc_alloc_rx_buffers does with igc_desc_unused.
        for slot in 0..3u64 {
            assert_eq!(ring.fill(&mut memory, 0x1000 * (slot + 1)), Ok(slot as usize));
        }
        assert_eq!(ring.tail(), 3);
        assert_eq!(ring.outstanding(), 3);
        assert_eq!(ring.unused(), 0);
        assert_eq!(
            ring.fill(&mut memory, 0x9000),
            Err(RingError::Full(RingDirection::Receive))
        );
        // The buffers really are in the descriptors the hardware will read.
        assert_eq!(scratch.words[0], 0x1000);
        assert_eq!(scratch.words[4], 0x2000);
        assert_eq!(scratch.words[8], 0x3000);
        assert_eq!(scratch.words[12], 0, "the unused descriptor is untouched");
    }

    #[test]
    fn a_recycled_buffer_goes_back_in_at_the_tail_not_at_its_old_descriptor() {
        let mut scratch = Scratch::new(4);
        let mut memory = scratch.memory();
        let mut ring = RxRing::new(4);
        for slot in 0..3u64 {
            ring.fill(&mut memory, 0x1000 * (slot + 1)).unwrap();
        }
        // The hardware writes descriptor 0 and the driver takes it.
        scratch.words[2] = bits::RXD_STAT_DD | bits::RXD_STAT_EOP;
        scratch.words[3] = 64;
        let (index, frame) = ring.take(&memory).expect("a frame in descriptor 0");
        assert_eq!(index, 0);
        assert_eq!(frame.length, 64);
        assert_eq!(ring.next_to_clean(), 1);
        assert_eq!(ring.outstanding(), 2);
        // The buffer comes back: it is armed in descriptor 3 -- the tail --
        // and the tail wraps to zero, so the hardware now owns 1, 2 and 3.
        assert_eq!(ring.fill(&mut memory, 0x4000), Ok(3));
        assert_eq!(ring.tail(), 0, "the tail wrapped");
        assert_eq!(ring.outstanding(), 3);
        assert_eq!(scratch.words[12], 0x4000);
        // Descriptor 0 is the driver's now, and it still holds the
        // hardware's write-back: the buffer address and the length of the
        // frame that was just taken.  That is harmless -- the hardware will
        // not touch a descriptor the driver owns -- and it is exactly why
        // arming a descriptor clears its length field, which the next step
        // shows.
        assert_eq!(scratch.words[3], 64);
        // Take descriptor 1, hand its buffer back, and arm descriptor 0 with
        // the recycled buffer.
        scratch.words[1 * 4 + 2] = bits::RXD_STAT_EOP;
        scratch.words[1 * 4 + 3] = 60;
        ring.take(&memory).expect("the second frame");
        assert_eq!(ring.fill(&mut memory, 0x7000), Ok(0));
        assert_eq!(scratch.words[0], 0x7000, "the recycled buffer is in place");
        assert_eq!(scratch.words[3], 0, "and the stale length is gone");
    }

    #[test]
    fn the_receive_ring_takes_descriptors_in_order_and_only_when_written_back() {
        let mut scratch = Scratch::new(4);
        let mut memory = scratch.memory();
        let mut ring = RxRing::new(4);
        for slot in 0..3u64 {
            ring.fill(&mut memory, 0x1000 * (slot + 1)).unwrap();
        }
        // Nothing written back yet.
        assert_eq!(
            ring.take(&memory),
            Err(RingError::NotReady(RingDirection::Receive))
        );
        // The hardware writes descriptor 0.
        scratch.words[2] = bits::RXD_STAT_DD | bits::RXD_STAT_EOP;
        scratch.words[3] = 64;
        let (index, frame) = ring.take(&memory).expect("a frame");
        assert_eq!(index, 0);
        assert_eq!(frame.length, 64);
        assert_eq!(ring.next_to_clean(), 1);
        // Descriptor 1 is not written back: the ring does not skip it.
        assert_eq!(
            ring.take(&memory),
            Err(RingError::NotReady(RingDirection::Receive))
        );
        assert_eq!(ring.next_to_clean(), 1);
    }

    #[test]
    fn a_receive_ring_whose_descriptors_are_all_the_drivers_is_empty() {
        let mut scratch = Scratch::new(4);
        let memory = scratch.memory();
        let mut ring = RxRing::new(4);
        // Nothing armed: the driver owns every descriptor.
        assert_eq!(
            ring.take(&memory),
            Err(RingError::Empty(RingDirection::Receive))
        );
    }

    #[test]
    fn the_receive_ring_wraps_and_its_free_space_stays_the_distance_around_it() {
        let mut scratch = Scratch::new(4);
        let mut memory = scratch.memory();
        let mut ring = RxRing::new(4);
        for slot in 0..3u64 {
            ring.fill(&mut memory, 0x1000 * (slot + 1)).unwrap();
        }
        // Consume two without handing anything back: the ring fills up with
        // descriptors the driver owns, and the tail cannot move.
        for index in 0..2 {
            scratch.words[index * 4 + 2] = bits::RXD_STAT_EOP;
            scratch.words[index * 4 + 3] = 64;
            ring.take(&memory).unwrap();
        }
        assert_eq!(ring.next_to_clean(), 2);
        assert_eq!(ring.tail(), 3);
        assert_eq!(ring.outstanding(), 1, "only descriptor 2 is still armed");
        assert_eq!(ring.unused(), 2);
        assert!(ring.has_room());
        // One buffer comes back: it goes into descriptor 3 and the tail
        // reaches the driver's cursor, which leaves one descriptor unused.
        assert_eq!(ring.fill(&mut memory, 0x5000), Ok(3));
        assert_eq!(ring.tail(), 0);
        assert_eq!(ring.outstanding(), 2);
        assert_eq!(ring.unused(), 1);
        // The second buffer comes back into descriptor 0, which wraps the
        // tail to 1, and the ring is full again.
        assert_eq!(ring.fill(&mut memory, 0x6000), Ok(0));
        assert_eq!(ring.tail(), 1);
        assert_eq!(ring.outstanding(), 3);
        assert_eq!(ring.unused(), 0);
        assert!(!ring.has_room());
    }

    #[test]
    fn the_buffer_pool_maps_slots_to_bus_and_cpu_addresses_and_back() {
        let mut region = vec![0u8; 8 * 2048];
        let base = NonNull::new(region.as_mut_ptr()).unwrap();
        // SAFETY: the region is live for the length of this test.
        let pool = unsafe { BufferPool::new(base, 0x0000_0001_0000_0000, 8, 2048) };
        assert_eq!(pool.slots(), 8);
        assert_eq!(pool.slot_bytes(), 2048);
        assert_eq!(pool.bus_address(0), Some(0x0000_0001_0000_0000));
        assert_eq!(pool.bus_address(3), Some(0x0000_0001_0000_1800));
        assert_eq!(pool.bus_address(8), None, "one past the end");
        assert_eq!(pool.cpu_address(0), Some(base));
        assert_eq!(pool.cpu_address(8), None);

        // Every slot round-trips, and a pointer that is not a slot start is
        // refused rather than rounded.
        for index in 0..8 {
            let pointer = pool.cpu_address(index).unwrap();
            assert_eq!(pool.slot_of(pointer), Some(index));
        }
        let inside = NonNull::new(unsafe { base.as_ptr().add(1) }).unwrap();
        assert_eq!(pool.slot_of(inside), None, "not a slot boundary");
        let past = NonNull::new(unsafe { base.as_ptr().add(8 * 2048) }).unwrap();
        assert_eq!(pool.slot_of(past), None, "one past the end");
        let before = NonNull::new(region.as_ptr().wrapping_sub(1) as *mut u8).unwrap();
        assert_eq!(pool.slot_of(before), None, "before the pool");
    }

    #[test]
    fn the_ring_errors_say_what_went_wrong() {
        assert!(RingError::Full(RingDirection::Transmit)
            .describe()
            .contains("transmit ring is full"));
        assert!(RingError::ForeignBuffer(RingDirection::Receive)
            .describe()
            .contains("does not belong to the receive ring"));
        assert!(RingError::NotEndOfPacket
            .describe()
            .contains("without the end-of-packet bit"));
        assert!(RingError::OverlongFrame {
            length: 9000,
            buffer: 2048
        }
        .describe()
        .contains("a 9000 byte frame"));
    }
}
