//! Typed access to the graphics MMIO aperture.
//!
//! A graphics device is a block of 32-bit registers behind a PCI memory BAR.
//! Everything about it that is not in configuration space is a load or a store
//! at a fixed offset, so this module's job is to make those loads and stores
//! typed, bounded, and honest about ordering.
//!
//! Three properties are deliberate:
//!
//! * **A register is a named value, not an integer.**  A caller cannot form an
//!   offset by adding to a base, and it cannot write a register the table did
//!   not declare writable.  Reading a register this kernel has not enabled is
//!   harmless and expected; writing one is a deliberate act with a name
//!   attached, and the boot probe declares no writable register at all.
//! * **Every named register is inside a band that needs no forcewake.**  Most
//!   of the register aperture is power gated: a read of a gated register
//!   without the matching forcewake returns zero, which is indistinguishable
//!   from a register that is legitimately zero and would make a probe's
//!   findings meaningless.  The bands below are the ones a Gen12 part decodes
//!   without the driver holding forcewake for a power well, and the offset
//!   check is a compile-time assertion, so a register added outside them
//!   cannot be compiled.
//! * **Ordering is stated, not assumed.**  x86_64 orders uncached loads and
//!   stores against each other in hardware, so no `mfence` belongs on this
//!   path; what the architecture does *not* give is a compiler barrier, and a
//!   posted write is not guaranteed to have reached the device when the store
//!   retires.  Both facts are written down here rather than left for a reader
//!   to rediscover.

use core::sync::atomic::{Ordering, compiler_fence};

#[cfg(target_os = "none")]
use axerrno::{AxError, AxResult};

use super::id::Quirk;

// The table holds the access machinery and the handful of registers the boot
// probe reads (`NAMED`). Display register tables were removed with the native
// display stack; the i915 port reads the registers it needs through its own
// uncore.

/// The part of the register aperture this kernel maps.
///
/// On Gen12 Xe-LP this is exactly the whole MMIO register window: `GTTMMADR`
/// is a 16 MiB BAR of which the first 2 MiB is registers, the next 6 MiB is
/// reserved and the last 8 MiB is the global page table.  Mapping the register
/// window rather than the BAR means the probe cannot wander into the page table
/// even if its own model of the aperture is wrong.
pub(crate) const PROBE_WINDOW: usize = 0x20_0000;

/// Offsets a Gen12 part decodes without forcewake held for a power well, as
/// `(first, last)` inclusive dword ranges.
///
/// Most of the aperture is power gated: a read of a gated register without the
/// matching forcewake returns zero, which is indistinguishable from a register
/// that is legitimately zero and would make everything the probe concluded
/// meaningless.  The block from 0x0 to 0xaff is reserved -- there is no
/// register there at all -- so it is excluded even though it is not gated.
pub(crate) const FORCEWAKE_FREE_BANDS: &[(u32, u32)] = &[
    // The always-on block, which includes the graphics identity register.
    (0x0b00, 0x1fff),
    (0x8160, 0x81ff),
    (0x9560, 0x97ff),
    (0xd000, 0xd7ff),
    (0x2_4000, 0x2_417f),
    // The uncore block: display registers, the interrupt block, and the
    // graphics control registers this probe reads.
    (0x4_0000, 0x1b_ffff),
];

/// Whether `offset` is inside a band that needs no forcewake.
pub(crate) const fn is_forcewake_free(offset: u32) -> bool {
    let mut index = 0;
    while index < FORCEWAKE_FREE_BANDS.len() {
        let (first, last) = FORCEWAKE_FREE_BANDS[index];
        if offset >= first && offset <= last {
            return true;
        }
        index += 1;
    }
    false
}

/// Whether a register may be written.
///
/// This is a statement about *this kernel*, not about the hardware: a register
/// can be writable in the architecture and still be declared read-only here,
/// because writing it today would change what a device the kernel does not own
/// is doing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Access {
    /// The kernel reads it and never writes it.
    ReadOnly,
    /// A later driver phase owns the write side.  A register only reaches this
    /// state when that phase needs it, so the first write to real hardware is
    /// an addition to the table rather than a change to the access rules.
    ReadWrite,
}

/// What a register's value means, so that a report can say something about it
/// without the report having to match on a register's name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Meaning {
    /// The graphics IP identity: architecture, release and stepping.
    GraphicsIp,
    /// The display IP identity, in the same encoding as [`Meaning::GraphicsIp`]
    /// but for the display engine, which is versioned separately.
    DisplayIp,
    /// The state of the graphics interrupt block.
    InterruptState,
    /// The graphics control register: how much memory the firmware gave the
    /// graphics device and how it is addressed.
    GraphicsControl,
    /// The physical base of the memory the firmware set aside for the graphics
    /// device.
    StolenMemoryBase,
    /// The physical base of the global page table.
    GttBase,
    /// One register of a display bus controller: GMBUS I2C/DDC or a DP AUX
    /// channel. The probe does not read these, and nothing in this build drives
    /// them: there is no display.
    BusController,
    /// A register of the south display's hotplug-detect block: the per-DDI
    /// enable, the latched detect field, the live connect state and the board
    /// polarity bit.  Not interpreted by the probe, and not driven in this build.
    Hotplug,
    /// A display power well's request or state register.  Not driven in this
    /// build: the GT_IRQ reference is a plain count (see `power.rs`).
    PowerWell,
    /// A display bring-up register that no variant above describes: a clock, a
    /// fuse that selects a clock's reference, a display buffer slice, a combo
    /// PHY register, a workaround or an error mask.
    ///
    /// It carries no decoding here on purpose.  These registers are read and
    /// written by the code that owns their meaning (the power and GT paths),
    /// which interprets them and logs what it observed at the point where it
    /// observed it; a second, thinner decoding in the probe's report would be a
    /// place for the two to disagree.
    BringUp,
}

/// How wide a register's value is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Width {
    Bits32,
    Bits64,
}

impl Width {
    pub(crate) const fn bytes(self) -> usize {
        match self {
            Self::Bits32 => 4,
            Self::Bits64 => 8,
        }
    }
}

/// One 32- or 64-bit register in the graphics aperture.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Register {
    name: &'static str,
    offset: u32,
    width: Width,
    access: Access,
    meaning: Meaning,
    /// The device behaviour that has to hold for this register to exist.  A
    /// register that only some parts implement is not read on the parts that
    /// do not, and the report says which quirk was missing.
    required_quirk: Option<Quirk>,
}

impl Register {
    /// Declare a 32-bit register this kernel reads and never writes.
    ///
    /// The offset is checked where the register is written down: it must be
    /// dword aligned, inside a band that needs no forcewake, and inside the
    /// window the probe maps.  A mistake is therefore a compile error rather
    /// than a read of a register that answers zero for the wrong reason.
    pub(crate) const fn read_only(
        name: &'static str,
        offset: u32,
        meaning: Meaning,
        required_quirk: Option<Quirk>,
    ) -> Self {
        Self::declare(
            name,
            offset,
            Width::Bits32,
            Access::ReadOnly,
            meaning,
            required_quirk,
        )
    }

    /// Declare a 64-bit register this kernel reads and never writes.
    ///
    /// A 64-bit register occupies two consecutive dwords; the second one is
    /// read at `offset + 4` and must still be inside the forcewake-free band
    /// and the mapped window, which the offset check verifies for both halves.
    pub(crate) const fn read_only_64(
        name: &'static str,
        offset: u32,
        meaning: Meaning,
        required_quirk: Option<Quirk>,
    ) -> Self {
        Self::declare(
            name,
            offset,
            Width::Bits64,
            Access::ReadOnly,
            meaning,
            required_quirk,
        )
    }

    /// Declare a register this kernel both reads and writes.
    ///
    /// Nothing uses this yet.  It exists so that the first write to real
    /// hardware is a stated addition to the table.
    pub(crate) const fn read_write(
        name: &'static str,
        offset: u32,
        meaning: Meaning,
        required_quirk: Option<Quirk>,
    ) -> Self {
        Self::declare(
            name,
            offset,
            Width::Bits32,
            Access::ReadWrite,
            meaning,
            required_quirk,
        )
    }

    const fn declare(
        name: &'static str,
        offset: u32,
        width: Width,
        access: Access,
        meaning: Meaning,
        required_quirk: Option<Quirk>,
    ) -> Self {
        assert!(
            offset.is_multiple_of(4),
            "graphics register offsets are dword aligned"
        );
        assert!(
            is_forcewake_free(offset),
            "graphics probe registers must live where no forcewake is needed"
        );
        assert!(
            is_forcewake_free(offset + width.bytes() as u32 - 4),
            "every dword of a graphics probe register must lie in a forcewake-free band"
        );
        assert!(
            (offset as usize) + width.bytes() <= PROBE_WINDOW,
            "graphics probe registers must live inside the mapped window"
        );
        Self {
            name,
            offset,
            width,
            access,
            meaning,
            required_quirk,
        }
    }

    pub(crate) const fn name(self) -> &'static str {
        self.name
    }

    pub(crate) const fn offset(self) -> u32 {
        self.offset
    }

    pub(crate) const fn width(self) -> Width {
        self.width
    }

    pub(crate) const fn access(self) -> Access {
        self.access
    }

    pub(crate) const fn meaning(self) -> Meaning {
        self.meaning
    }

    pub(crate) const fn required_quirk(self) -> Option<Quirk> {
        self.required_quirk
    }

    pub(crate) const fn is_writable(self) -> bool {
        matches!(self.access, Access::ReadWrite)
    }

    /// Whether a window of `len` bytes contains this register whole.
    pub(crate) const fn fits_in(self, len: usize) -> bool {
        (self.offset() as usize) + self.width.bytes() <= len
    }
}

/// A mapped window of the graphics aperture.
///
/// The window is not the BAR; it is the part of the BAR this kernel mapped,
/// and every access is checked against it.  A register outside it is refused
/// rather than followed, so a register table that disagrees with the mapped
/// aperture produces a report line instead of a fault.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RegisterWindow {
    base: usize,
    len: usize,
}

impl RegisterWindow {
    /// Take a window over an aperture that is already mapped.
    ///
    /// # Safety
    ///
    /// `base .. base + len` must be a live region of at least `len` bytes,
    /// mapped as device memory (or, in a test, as ordinary memory standing in
    /// for it) for as long as this value is used, and it must be aligned well
    /// enough for the 32-bit accesses the register table performs.  The host
    /// tests use this to point a window at an ordinary buffer, which is the
    /// only way to exercise volatile register access on a machine that has no
    /// graphics device.
    pub(crate) const unsafe fn from_mapped(base: usize, len: usize) -> Self {
        Self { base, len }
    }

    /// Map a physical aperture as uncached device memory and take a window
    /// over it.
    ///
    /// `iomap` is the facility the firmware framebuffer also uses: it maps the
    /// physical range at its direct-map address with the device bit set, and
    /// it tolerates a range the platform already mapped.  The mapping
    /// deliberately outlives this value -- it belongs to the kernel address
    /// space -- because a later driver phase will want the same registers
    /// without mapping them twice.
    #[cfg(target_os = "none")]
    pub(crate) fn map(physical: u64, size: usize) -> AxResult<Self> {
        let address = usize::try_from(physical).map_err(|_| AxError::InvalidInput)?;
        if address == 0 || size == 0 || !size.is_multiple_of(4) {
            return Err(AxError::InvalidInput);
        }
        let base = axmm::iomap(axhal::mem::PhysAddr::from_usize(address), size)?;
        Ok(Self {
            base: base.as_usize(),
            len: size,
        })
    }

    /// The length of the mapped window in bytes.
    pub(crate) const fn len(self) -> usize {
        self.len
    }

    /// The virtual address the window starts at.
    pub(crate) const fn base(self) -> usize {
        self.base
    }

    /// Where `register` lives, or `None` when it is outside this window.
    pub(crate) const fn address_of(self, register: Register) -> Option<usize> {
        if !register.fits_in(self.len) {
            return None;
        }
        Some(self.base + register.offset() as usize)
    }

    /// Read a register, or `None` when it lies outside the mapped window.
    ///
    /// Reading a register whose engine is powered down, or one this part does
    /// not implement, is a normal read: it returns whatever the bus returns
    /// and changes nothing.  That is why a probe can read first and interpret
    /// afterwards -- and why the register table, not the read, is where the
    /// safety argument lives.
    pub(crate) fn read(self, register: Register) -> Option<u32> {
        self.read_word(self.address_of(register)?)
    }

    /// Read a 64-bit register, or `None` when this register is not 64 bits
    /// wide or does not fit in the window.
    ///
    /// The two halves are read as separate 32-bit accesses.  That is not a
    /// compromise: the pair is not atomic against the device either, and a
    /// base address the firmware has already programmed does not change under
    /// a probe.
    pub(crate) fn read64(self, register: Register) -> Option<u64> {
        if register.width() != Width::Bits64 || !register.fits_in(self.len) {
            return None;
        }
        let low = self.read_word(self.base + register.offset() as usize)? as u64;
        let high = self.read_word(self.base + register.offset() as usize + 4)? as u64;
        Some(low | (high << 32))
    }

    /// Read one assembled dword from an address inside the window.
    fn read_word(self, address: usize) -> Option<u32> {
        if address < self.base || address + size_of::<u32>() > self.base + self.len {
            return None;
        }
        // SAFETY: the address was just checked to lie inside a window this
        // value promises is mapped device memory, and a 32-bit register access
        // is naturally aligned because the register table is dword aligned.
        // Nothing else in the kernel aliases it: the aperture belongs to this
        // device and is mapped uncached, so a read cannot be served from a
        // stale cache line.
        let value = unsafe { core::ptr::read_volatile(address as *const u32) };
        // The load may not be moved past a later volatile access, and this
        // fence states that whatever the caller does next happens after the
        // device answered.  x86_64 needs no processor fence here: uncached
        // accesses are strongly ordered against each other.
        compiler_fence(Ordering::Acquire);
        Some(value)
    }

    /// Write a register, returning whether the write happened.
    ///
    /// A read-only register, or one outside the window, is refused; the caller
    /// gets `false` rather than a silent no-op.
    ///
    /// The write is posted: it may still be in flight when this returns.  A
    /// driver that needs the device to have seen it reads the register back,
    /// which is the ordering primitive this architecture actually offers --
    /// there is no completion signal for an MMIO store, and an `mfence` would
    /// not create one.
    pub(crate) fn write(self, register: Register, value: u32) -> bool {
        if !register.is_writable() {
            return false;
        }
        let Some(address) = self.address_of(register) else {
            return false;
        };
        // Keep the value from being computed after the store, and the store
        // from being sunk past whatever the caller does next.
        compiler_fence(Ordering::Release);
        // SAFETY: as for `read`, and the register table is the only source of
        // addresses, so the store is to a dword-aligned register inside the
        // mapped aperture.
        unsafe { core::ptr::write_volatile(address as *mut u32, value) };
        compiler_fence(Ordering::SeqCst);
        true
    }
}

/// The registers a probe reads, in report order.
///
/// Every entry is read-only and answerable without forcewake.  They are chosen
/// so that a log line answers a question: which graphics IP is this, which
/// display IP, is the interrupt block there, how much memory does the firmware
/// say the device has and where does it start.  The deep register reference --
/// bit fields, sequences, everything a display port would need -- is
/// `docs/design/intel-display-registers.md`; this table is only what a probe
/// can read before it owns the device.
///
/// Three registers a reader might expect are deliberately absent:
///
/// * The GT interrupt dword registers (`0x190018`/`0x19001c`) *lock* when read.
///   The vendor driver's own comment records that a read has to be undone with
///   a write before anyone else can use the register, which makes reading one
///   the opposite of a harmless probe.
/// * The per-bank interrupt identity registers (`0x190060 + 4n`) only report
///   the bank the selector register (`0x190070 + 4n`) was last set to, and
///   setting the selector is a write.  A read without that write would report
///   whatever the firmware left selected, which is a fact about the firmware
///   rather than about this device.
/// * Anything in the first 2 KiB of the aperture.  There is no register there;
///   the range is reserved.
pub(crate) const NAMED: &[Register] = &[
    Register::read_only(
        "GMD_ID",
        0x0d8c,
        Meaning::GraphicsIp,
        Some(Quirk::GmdIdImplemented),
    ),
    Register::read_only(
        "GMD_ID_DISPLAY",
        0x5_10a0,
        Meaning::DisplayIp,
        Some(Quirk::GmdIdImplemented),
    ),
    Register::read_only("GFX_MSTR_IRQ", 0x19_0010, Meaning::InterruptState, None),
    Register::read_only("GGC", 0x10_8040, Meaning::GraphicsControl, None),
    Register::read_only_64("DSMBASE", 0x10_80c0, Meaning::StolenMemoryBase, None),
    Register::read_only_64("GSMBASE", 0x10_8100, Meaning::GttBase, None),
];

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// A window over ordinary memory: the registers are plain words there.
    fn window_over(words: &mut [u32]) -> RegisterWindow {
        // SAFETY: the caller keeps `words` alive for the window's use.
        unsafe { RegisterWindow::from_mapped(words.as_mut_ptr() as usize, words.len() * 4) }
    }

    const SCRATCH: Register = Register::read_write("SCRATCH", 0xb00, Meaning::BringUp, None);
    const STATUS: Register = Register::read_only("STATUS", 0xb04, Meaning::BringUp, None);

    #[test]
    fn a_register_reads_and_writes_the_word_it_addresses() {
        let mut words = vec![0u32; 0xb000 / 4];
        let window = window_over(&mut words);
        assert!(window.write(SCRATCH, 0xdead_beef));
        assert_eq!(window.read(SCRATCH), Some(0xdead_beef));
        drop(window);
        assert_eq!(words[0xb00 / 4], 0xdead_beef);
    }

    #[test]
    fn a_read_only_register_refuses_to_be_written() {
        let mut words = vec![0u32; 0xb000 / 4];
        let window = window_over(&mut words);
        assert!(!window.write(STATUS, 1));
    }

    #[test]
    fn a_register_outside_the_window_is_refused_rather_than_followed() {
        let mut words = vec![0u32; 0x800 / 4];
        let window = window_over(&mut words);
        assert_eq!(window.read(SCRATCH), None);
    }

    #[test]
    fn every_named_register_is_addressable_and_read_only() {
        for register in NAMED {
            assert!(!register.is_writable(), "{} is writable", register.name());
            assert!(register.offset() % 4 == 0, "{} is not dword aligned", register.name());
        }
    }

    #[test]
    fn a_64bit_register_reads_both_of_its_dwords() {
        const WIDE: Register = Register::read_only_64("WIDE", 0xb00, Meaning::BringUp, None);
        let mut words = vec![0u32; 0xb000 / 4];
        words[0xb00 / 4] = 0x1111_2222;
        words[0xb04 / 4] = 0x3333_4444;
        let window = window_over(&mut words);
        assert_eq!(window.read64(WIDE), Some(0x3333_4444_1111_2222));
    }
}
