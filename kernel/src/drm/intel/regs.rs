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
//!   attached, and this probe declares no writable register at all.
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
    /// One register of the GMBUS controller, the I2C master that carries DDC
    /// (and therefore EDID).  The probe's report does not read these; the
    /// protocol that gives them meaning lives in [`super::gmbus`].
    BusController,
    /// A register of the south display's hotplug-detect block.  Interpreted by
    /// [`super::hpd`], not by the probe.
    Hotplug,
    /// A display power well's control register.  Interpreted where a caller
    /// asks whether a well is on; this kernel reads one of these to explain a
    /// GMBUS failure and does not write any of them.
    PowerWell,
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
    /// A writable entry is a statement about this kernel, not about the
    /// hardware, and the first write to real hardware is an addition here
    /// rather than a loosening of the access rules: [`BUS`] is where the
    /// GMBUS transaction protocol and hotplug detection declare the handful of
    /// registers they program.
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
/// bit fields, sequences, everything a modeset needs --
/// is `docs/design/intel-display-registers.md`; this table is only what a
/// probe can read before it owns the device.
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

/// The GMBUS controller: Intel's I2C master for DDC, and therefore the way an
/// EDID is read.
///
/// The block sits in the south display window at `0xC0000`, the base a Gen12
/// part without a GMCH puts its PCH display registers at (`[I915]`
/// `display/intel_gmbus.c:871-879`; reference §9.1).  Offsets are `[I915]`
/// `display/intel_gmbus_regs.h:29-79`.
///
/// **Provenance, and it is weaker here than anywhere else in this file.**  Of
/// these registers only `GMBUS0` appears in any public Intel Gen12 register
/// volume; the reference says so explicitly (§9.2, and §13.1 item 2, which
/// records that the TGL, DG1 and RKL volumes were all searched).  The whole
/// transaction protocol therefore rests on `[I915]` alone, which is why every
/// field mask in [`super::gmbus`] carries its own citation and why this driver
/// prefers to poll a status register it can read back over anything it cannot.
///
/// Access is stated per register rather than inherited from the block:
///
/// * `GMBUS2` (status) and `GMBUS3` (data) are read-only *here* because this
///   driver never writes them -- the index cycle this driver uses carries the
///   index byte in `GMBUS1`, so `GMBUS3` is only ever read.  A later driver
///   that speaks DPCD over I2C needs `GMBUS3` writable, and that is an
///   addition to this table rather than a relaxation of it.
/// * `GMBUS4` (interrupt mask) is written only with zero: this kernel takes no
///   GMBUS interrupt, so the mask is cleared and completion is polled.
/// * `GMBUS5` (two-byte index) is cleared before a transaction.  See
///   [`super::gmbus`] for why clearing it is worth a write to a register no
///   public volume defines.
pub(crate) const BUS: &[Register] = &[
    GMBUS0,
    GMBUS1,
    GMBUS2,
    GMBUS3,
    GMBUS4,
    GMBUS5,
    SHOTPLUG_CTL_DDI,
    SDEISR,
    SOUTH_CHICKEN1,
    SHPD_FILTER_CNT,
    ICL_PWR_WELL_CTL_AUX2,
];

/// Clock/port select: the pin index and the bus rate.
pub(crate) const GMBUS0: Register =
    Register::read_write("GMBUS0", 0xc5100, Meaning::BusController, None);

/// Command and status: cycle type, byte count, slave address, direction.
pub(crate) const GMBUS1: Register =
    Register::read_write("GMBUS1", 0xc5104, Meaning::BusController, None);

/// Status: the bits a transaction is driven by.
pub(crate) const GMBUS2: Register =
    Register::read_only("GMBUS2", 0xc5108, Meaning::BusController, None);

/// The four-byte data buffer.
pub(crate) const GMBUS3: Register =
    Register::read_only("GMBUS3", 0xc510c, Meaning::BusController, None);

/// The interrupt mask.  Written with zero: completion is polled, not taken.
pub(crate) const GMBUS4: Register =
    Register::read_write("GMBUS4", 0xc5110, Meaning::BusController, None);

/// The two-byte index enable and value.
pub(crate) const GMBUS5: Register =
    Register::read_write("GMBUS5", 0xc5120, Meaning::BusController, None);

/// DDI hotplug control, one four-bit field per DDI (`[I915]`
/// `i915_reg.h:3078-3085`; reference §9.5).
///
/// The address is a reuse: on older platforms `0xc4030` is `PCH_PORT_HOTPLUG`
/// with a different field layout, including three `BXT_DDI*_HPD_INVERT` bits
/// at 27/11/3 (`[I915]` `i915_reg.h:3023-3064`).  On ADL-N the split
/// `SHOTPLUG_CTL_DDI`/`SHOTPLUG_CTL_TC` layout applies, which is what
/// [`super::hpd`] programs.
pub(crate) const SHOTPLUG_CTL_DDI: Register =
    Register::read_write("SHOTPLUG_CTL_DDI", 0xc4030, Meaning::Hotplug, None);

/// South display interrupt status: where the *live* connect state of a DDI is
/// read, per the PRM's advice quoted in reference §9.4.
///
/// Read-only here, and that is not a formality: the same register is
/// write-one-to-clear, so a stray write would discard the very state a caller
/// came to read.
pub(crate) const SDEISR: Register = Register::read_only("SDEISR", 0xc4000, Meaning::Hotplug, None);

/// The south display's first "chicken" register.
///
/// The reference records it as `[GAP]`: the DG1 PRM requires `0xC2000[18:15] =
/// 1111b` for board hotplug inversion but did not say what the register is
/// called.  It is `SOUTH_CHICKEN1` (`[I915]` `i915_reg.h:3358`), and the field
/// is not one four-bit value but one bit per DDI -- `INVERT_DDIA_HPD` through
/// `INVERT_DDID_HPD` at bits 15..18 (`i915_reg.h:3367-3370`) -- which is what
/// lets a board invert one port instead of all four.
pub(crate) const SOUTH_CHICKEN1: Register =
    Register::read_write("SOUTH_CHICKEN1", 0xc2000, Meaning::Hotplug, None);

/// Hotplug pulse filter (`[I915]` `i915_reg.h:3092`; reference §9.5).
///
/// Read-only: the filter shapes interrupt *pulses*, and this workstream
/// deliberately enables detection without taking an interrupt, so it has no
/// business changing it.
pub(crate) const SHPD_FILTER_CNT: Register =
    Register::read_only("SHPD_FILTER_CNT", 0xc4038, Meaning::Hotplug, None);

/// The AUX/DDC power well request/state register (`[I915]` `i915_reg.h:3663`;
/// reference §4.2, §11 phase 2.1).
///
/// Read-only to this kernel on purpose.  Enabling a power well belongs to the
/// power workstream; this module reads the state bit so that "GMBUS was NAKed
/// on every address" can be reported as the named condition the reference says
/// it almost always is -- the well for that pin pair being down -- instead of
/// as a timeout.
pub(crate) const ICL_PWR_WELL_CTL_AUX2: Register =
    Register::read_only("ICL_PWR_WELL_CTL_AUX2", 0x4_5444, Meaning::PowerWell, None);

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;
    use crate::test_support::scheduler_test_context;

    /// A buffer standing in for an aperture, sized exactly like the window the
    /// probe maps.  Device memory is ordinary memory with rules, so the
    /// volatile access path can be tested against it as long as the window is
    /// built to point at it.
    struct Scratch {
        words: vec::Vec<u32>,
    }

    impl Scratch {
        fn new() -> Self {
            Self {
                words: vec![0; PROBE_WINDOW / 4],
            }
        }

        fn window(&mut self) -> RegisterWindow {
            // SAFETY: `words` is a live, 4-byte aligned buffer of exactly
            // `PROBE_WINDOW` bytes that outlives both the window and this
            // borrow, and the windows built over it are the only way the
            // buffer is reached once the borrow ends.
            unsafe { RegisterWindow::from_mapped(self.words.as_mut_ptr() as usize, PROBE_WINDOW) }
        }
    }

    /// The named register with this name, or a panic naming the table.
    fn named(name: &str) -> Register {
        *NAMED
            .iter()
            .find(|register| register.name() == name)
            .unwrap_or_else(|| panic!("{name} is not in the register table"))
    }

    #[test]
    fn a_register_reads_and_writes_the_word_it_addresses() {
        let _guard = scheduler_test_context();
        let mut scratch = Scratch::new();
        let window = scratch.window();
        let register = Register::read_write("TEST", 0x1000, Meaning::InterruptState, None);
        assert_eq!(window.address_of(register), Some(window.base() + 0x1000));
        assert!(window.write(register, 0xdead_beef));
        assert_eq!(window.read(register), Some(0xdead_beef));
        // The word after it was not disturbed: the offset arithmetic, not a
        // rounded index, is what selected the address.
        let neighbour = Register::read_only("NEIGHBOUR", 0x1004, Meaning::InterruptState, None);
        assert_eq!(window.read(neighbour), Some(0));
        assert_eq!(scratch.words[0x1000 / 4], 0xdead_beef);
        assert_eq!(scratch.words[0x1004 / 4], 0);
    }

    #[test]
    fn a_read_only_register_refuses_to_be_written() {
        let _guard = scheduler_test_context();
        let mut scratch = Scratch::new();
        let window = scratch.window();
        let register = named("GFX_MSTR_IRQ");
        assert!(!register.is_writable());
        scratch.words[register.offset() as usize / 4] = 0x8000_0000;
        assert!(!window.write(register, 0xffff_ffff));
        // The refusal is a refusal, not a silent write.
        assert_eq!(window.read(register), Some(0x8000_0000));
    }

    #[test]
    fn a_register_outside_the_window_is_refused_rather_than_followed() {
        let _guard = scheduler_test_context();
        let mut scratch = Scratch::new();
        // A window smaller than the buffer: the registers below are inside the
        // buffer but outside the window, so they must have no address.
        let small =
            unsafe { RegisterWindow::from_mapped(scratch.words.as_mut_ptr() as usize, 0x1000) };
        assert_eq!(
            small.address_of(named("GFX_MSTR_IRQ")),
            None,
            "an interrupt register is not in the first 8 KiB"
        );
        assert_eq!(small.read(named("GFX_MSTR_IRQ")), None);
        let outside = Register::read_write("OUTSIDE", 0x1800, Meaning::InterruptState, None);
        assert_eq!(small.address_of(outside), None);
        assert!(!small.write(outside, 1));
        // The last dword of a window is addressable and the one after it is
        // not, so the bound is the window's own length rather than the page
        // the mapping was rounded to.
        let last = Register::read_write("LAST", 0x0ffc, Meaning::InterruptState, None);
        assert!(small.address_of(last).is_some());
        assert!(small.write(last, 7));
        assert_eq!(small.read(last), Some(7));
        assert_eq!(scratch.words[0x0ffc / 4], 7);
        let past = Register::read_only("PAST", 0x1800, Meaning::InterruptState, None);
        assert_eq!(small.address_of(past), None);
        assert_eq!(small.read(past), None);
    }

    #[test]
    fn every_named_register_is_addressable_and_read_only() {
        for register in NAMED {
            assert_eq!(register.offset() % 4, 0, "{}", register.name());
            assert!(
                is_forcewake_free(register.offset()),
                "{} is outside the forcewake-free bands",
                register.name()
            );
            assert!(
                is_forcewake_free(register.offset() + register.width().bytes() as u32 - 4),
                "{} has a second dword outside the forcewake-free bands",
                register.name()
            );
            // The window the probe maps must contain the register whole, or
            // the probe could not read it on the target machine.
            assert!(
                register.fits_in(PROBE_WINDOW),
                "{} is outside the mapped window",
                register.name()
            );
            assert!(
                !register.is_writable(),
                "{} must be read-only: nothing in this kernel may write a register yet",
                register.name()
            );
        }
    }

    #[test]
    fn a_64bit_register_reads_both_of_its_dwords() {
        let _guard = scheduler_test_context();
        let mut scratch = Scratch::new();
        let window = scratch.window();
        let register = Register::read_only_64("TEST64", 0x1000, Meaning::GttBase, None);
        assert_eq!(register.width().bytes(), 8);
        assert!(register.fits_in(PROBE_WINDOW));
        // A stored base address reads back as one value, low dword first.
        scratch.words[0x1000 / 4] = 0x8000_1000;
        scratch.words[0x1004 / 4] = 0x0000_0001;
        assert_eq!(window.read64(register), Some(0x0000_0001_8000_1000));
        // A 32-bit register has no 64-bit read, and a 64-bit register has no
        // 32-bit reading that pretends to be complete.
        assert_eq!(window.read64(named("GGC")), None);
        assert_eq!(
            window.read(register),
            Some(0x8000_1000),
            "the low dword of a 64-bit register is still its low dword"
        );
        // A window that holds only the low half cannot answer for the whole
        // register.
        let half = unsafe { RegisterWindow::from_mapped(window.base(), 0x1004) };
        assert_eq!(half.read64(register), None);
    }

    #[test]
    fn the_forcewake_bands_are_the_ones_the_table_relies_on() {
        // The bands, and the gaps between them, are the whole safety argument
        // for reading anything at all.
        assert!(is_forcewake_free(0x0b00));
        assert!(is_forcewake_free(0x0d8c), "the graphics identity register");
        assert!(is_forcewake_free(0x1fff));
        assert!(!is_forcewake_free(0x0aff), "reserved, not for a probe");
        assert!(!is_forcewake_free(0x0200), "reserved, not for a probe");
        assert!(!is_forcewake_free(0x2000), "render domain: needs forcewake");
        assert!(!is_forcewake_free(0x3_ffff));
        assert!(is_forcewake_free(0x4_0000));
        assert!(
            is_forcewake_free(0x10_8040),
            "the graphics control register"
        );
        assert!(is_forcewake_free(0x19_0010), "the interrupt block");
        assert!(is_forcewake_free(0x1b_ffff));
        assert!(!is_forcewake_free(0x1c_0000), "past the uncore block");
        // The topology fuse mirrors are *not* in a band: they are in the GT
        // forcewake domain, so a probe without forcewake must not read them
        // however tempting they look.
        assert!(!is_forcewake_free(0x9138));
        assert!(!is_forcewake_free(0x913c));
        // The window the probe maps covers every band it may read, with room
        // to spare: the last dword of the window is deliberately outside every
        // band, so a register can never sit against the window's edge.
        for &(_, last) in FORCEWAKE_FREE_BANDS {
            assert!(
                (last as usize) + 4 <= PROBE_WINDOW,
                "a band ends past the mapped window"
            );
        }
        assert!(PROBE_WINDOW as u32 > 0x1b_ffff);
        assert!(!is_forcewake_free(PROBE_WINDOW as u32 - 4));
    }

    #[test]
    fn every_bus_register_is_addressable_and_classified() {
        // The same check the probe's own table gets, for the same reason: a
        // register outside the mapped window or outside a forcewake-free band
        // would answer nonsense on the target machine, and a mistake here is a
        // compile-time-adjacent failure rather than a hardware experiment.
        for register in BUS {
            assert_eq!(register.offset() % 4, 0, "{}", register.name());
            assert!(
                is_forcewake_free(register.offset()),
                "{} is outside the forcewake-free bands",
                register.name()
            );
            assert!(
                is_forcewake_free(register.offset() + register.width().bytes() as u32 - 4),
                "{} has a second dword outside the forcewake-free bands",
                register.name()
            );
            assert!(
                register.fits_in(PROBE_WINDOW),
                "{} is outside the mapped window",
                register.name()
            );
        }
        // The window the probe maps must cover the whole south display block
        // these registers live in, or nothing here could be reached on the
        // target machine.
        assert!(PROBE_WINDOW as u32 > 0xc5120 + 4, "the GMBUS block");
        assert!(PROBE_WINDOW as u32 > 0x4_5444 + 4, "the AUX power well");
    }

    #[test]
    fn writing_the_bus_is_an_explicit_list() {
        // Writable is a statement about this kernel.  This test is the list:
        // if a register becomes writable, that is a deliberate change to what
        // the kernel may do to real hardware, and it should fail here first.
        let writable: alloc::vec::Vec<&str> = BUS
            .iter()
            .filter(|register| register.is_writable())
            .map(|register| register.name())
            .collect();
        assert_eq!(
            writable,
            alloc::vec![
                "GMBUS0",           // pin select and rate
                "GMBUS1",           // the transaction itself
                "GMBUS4",           // interrupt mask, cleared to zero
                "GMBUS5",           // two-byte index, cleared to zero
                "SHOTPLUG_CTL_DDI", // hotplug enable
                "SOUTH_CHICKEN1",   // board HPD inversion, when asked for
            ]
        );
        // The status, data and power-well registers are read-only, and so is
        // the interrupt status: a write to SDEISR would clear the very state a
        // caller reads it for.
        for name in [
            "GMBUS2",
            "GMBUS3",
            "SDEISR",
            "SHPD_FILTER_CNT",
            "ICL_PWR_WELL_CTL_AUX2",
        ] {
            let register = BUS
                .iter()
                .find(|register| register.name() == name)
                .unwrap_or_else(|| panic!("{name} is not in the bus table"));
            assert!(!register.is_writable(), "{name} must stay read-only");
        }
    }
}
