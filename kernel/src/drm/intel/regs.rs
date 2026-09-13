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
    /// A register the display bring-up owns: a power well, a clock, a fuse that
    /// selects a clock's reference, a display buffer slice or a combo PHY.
    ///
    /// It carries no decoding here on purpose.  These registers are read and
    /// written by `power`, `clk` and `phy`, which interpret them and log what
    /// they observed at the point where they observed it; a second, thinner
    /// decoding in the probe's report would be a place for the two to disagree.
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

/// The register operations a bring-up sequence needs, as a trait.
///
/// [`RegisterWindow`] is the real implementation and is the only one that
/// touches hardware.  The sequences in `power`, `clk` and `phy` are written
/// against this trait so that a host test can drive them through a mock whose
/// status bits set, or fail to set, on command -- which is the only way to test
/// a handshake's success path, its timeout path and its rollback on a machine
/// that has no graphics device.
///
/// It is deliberately three methods: a sequence that needs more than reading
/// and writing a named register is a sequence that has stopped being checkable
/// against the reference by eye.
pub(crate) trait Registers {
    /// Read a 32-bit register, or `None` when it is outside the window.
    fn read(&self, register: Register) -> Option<u32>;

    /// Read a 64-bit register, or `None` when it is not 64 bits wide or does
    /// not fit in the window.
    fn read64(&self, register: Register) -> Option<u64>;

    /// Write a register, returning whether the write happened.  A read-only
    /// register is refused rather than silently skipped.
    fn write(&self, register: Register, value: u32) -> bool;
}

impl Registers for RegisterWindow {
    fn read(&self, register: Register) -> Option<u32> {
        RegisterWindow::read(*self, register)
    }

    fn read64(&self, register: Register) -> Option<u64> {
        RegisterWindow::read64(*self, register)
    }

    fn write(&self, register: Register, value: u32) -> bool {
        RegisterWindow::write(*self, register, value)
    }
}

/// What one poll of a status register is assumed to cost, in microseconds.
///
/// A poll is an uncached read of a device register, which on this platform is a
/// non-posted transaction of the order of a microsecond.  The number is an
/// estimate and it is used only to turn a documented timeout into a count --
/// see [`poll_attempts`].
pub(crate) const POLL_COST_US: u32 = 1;

/// How many polls a documented microsecond timeout is worth.
///
/// This is the one place where a timeout is expressed, and it is expressed as a
/// **count of reads rather than a clock reading**.  Two consequences are
/// deliberate:
///
/// * A sequence cannot hang because a clock is not running, and it behaves
///   identically on the target and in a host test.  This kernel's platform time
///   interface is registered only for `target_os = "none"`, so a poll loop that
///   read `monotonic_time` would not be testable at all -- and the handshake's
///   success path, its timeout path and its rollback are exactly what a host
///   test has to be able to drive.
/// * The bound stays traceable: a caller passes the microsecond figure the
///   reference gives for that status bit, and the count is that figure divided
///   by [`POLL_COST_US`].  A poll that fails means "the status did not appear
///   within N reads", which is what the error says.
///
/// What it is *not* is a precise timer.  A tighter bound than the reference's
/// would refuse hardware that is merely slow, so every caller passes the
/// longest figure any source gives for its status bit -- usually the vendor
/// driver's, which is deliberately more generous than the PRM's.
pub(crate) const fn poll_attempts(timeout_us: u32) -> u32 {
    let attempts = timeout_us / POLL_COST_US;
    if attempts == 0 { 1 } else { attempts }
}

/// Poll a register until `mask` reads `value`, or the poll budget runs out.
///
/// Returns `Some(true)` when the register came back with the value, `Some(false)`
/// when the budget ran out, and `None` when the register could not be read at
/// all -- which a caller must not confuse with a register that read zero.
pub(crate) fn poll(
    regs: &impl Registers,
    register: Register,
    mask: u32,
    value: u32,
    timeout_us: u32,
) -> Option<bool> {
    for _ in 0..poll_attempts(timeout_us) {
        let readback = regs.read(register)?;
        if readback & mask == value {
            return Some(true);
        }
        core::hint::spin_loop();
    }
    Some(false)
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

// ---------------------------------------------------------------------------
// The registers the display power, clock and PHY bring-up owns.
//
// These are separate from `NAMED` for two reasons.  `NAMED` is what the boot
// probe reads, and the probe's claim is that it writes nothing; every register
// here is either writable or is a fuse whose value only means something to the
// bring-up path.  And a register belongs in this table only when a sequence in
// `power`, `clk` or `phy` names it, so the table is a complete inventory of what
// this kernel will program -- which is what a reviewer needs in order to check
// the sequences against the reference without reading four modules.
//
// Every offset below is cited to `docs/design/intel-display-registers.md` by
// section, and the section in turn cites the PRM or the Linux `drm/i915` tree.
// Where the two disagree, the comment says so.
// ---------------------------------------------------------------------------

/// `SKL_DFSM`, the display fuse register.
///
/// Which pipes are fused off, and whether DMC/DSC/HDCP/FBC exist at all.
/// Reference §3.4 and §12.1; `[I915]` `i915_reg.h:2858-2870`.
pub(crate) const SKL_DFSM: Register =
    Register::read_only("SKL_DFSM", 0x5_1000, Meaning::BringUp, None);

/// `SKL_DSSM`, which carries the CDCLK PLL reference frequency in bits `[31:29]`.
///
/// Reference §4.6 ("The reference clock") and §12.1; `[I915]` `i915_reg.h:2880-2884`.
/// This register selects both the CDCLK ratio table and, on the PRM's numbers,
/// the PG1 enable timeout, so it is read before either is used.
pub(crate) const SKL_DSSM: Register =
    Register::read_only("SKL_DSSM", 0x5_1004, Meaning::BringUp, None);

/// `SFUSE_STRAP`, the south display fuse strap.
///
/// Bit 8 is the raw-clock strap and bit 7 declares the SKU headless.
/// Reference §3.5 and §4.8; `[I915]` `i915_reg.h:4391-4399`.
pub(crate) const SFUSE_STRAP: Register =
    Register::read_only("SFUSE_STRAP", 0xC_2014, Meaning::BringUp, None);

/// `SKL_FUSE_STATUS`, the per-power-gate distribution status.
///
/// Reference §4.4; `[I915]` `i915_reg.h:3724-3738`.
pub(crate) const SKL_FUSE_STATUS: Register =
    Register::read_only("SKL_FUSE_STATUS", 0x4_2000, Meaning::BringUp, None);

/// `HSW_PWR_WELL_CTL1`, the firmware's power well request register.
///
/// Read-only here, and read only to diagnose a well that will not come up: the
/// four request registers are OR-ed by hardware, so a well that stays off while
/// this one has its request bit set was requested by the firmware all along.
/// Reference §4.2; `[I915]` `i915_reg.h:3626`.
pub(crate) const HSW_PWR_WELL_CTL1: Register =
    Register::read_only("HSW_PWR_WELL_CTL1", 0x4_5400, Meaning::BringUp, None);

/// `HSW_PWR_WELL_CTL2`, the driver's power well request register.
///
/// The only power well register this kernel writes.  Reference §4.2 and §4.4;
/// `[I915]` `i915_reg.h:3627`.
pub(crate) const HSW_PWR_WELL_CTL2: Register =
    Register::read_write("HSW_PWR_WELL_CTL2", 0x4_5404, Meaning::BringUp, None);

/// `HSW_PWR_WELL_CTL3`, the KVMR requester's register.  Read for diagnosis.
/// Reference §4.2; `[I915]` `i915_reg.h:3628`.
pub(crate) const HSW_PWR_WELL_CTL3: Register =
    Register::read_only("HSW_PWR_WELL_CTL3", 0x4_5408, Meaning::BringUp, None);

/// `HSW_PWR_WELL_CTL4`, the debug requester's register.  Read for diagnosis.
/// Reference §4.2; `[I915]` `i915_reg.h:3629`.
pub(crate) const HSW_PWR_WELL_CTL4: Register =
    Register::read_only("HSW_PWR_WELL_CTL4", 0x4_540C, Meaning::BringUp, None);

/// `ICL_PWR_WELL_CTL_AUX2`, the driver's AUX power well request register.
///
/// Declared because the AUX wells for a port have to be enabled before GMBUS or
/// AUX can use that pin pair (reference §11 phase 2.1), which is another
/// workstream's step; the well machinery in `power` is generic over the request
/// register so that step needs a name, not a new mechanism.
/// Reference §4.2; `[I915]` `i915_reg.h:3663`.
pub(crate) const ICL_PWR_WELL_CTL_AUX2: Register =
    Register::read_write("ICL_PWR_WELL_CTL_AUX2", 0x4_5444, Meaning::BringUp, None);

/// `ICL_PWR_WELL_CTL_DDI2`, the driver's DDI IO power well request register.
/// Reference §4.2 and §11 phase 5; `[I915]` `i915_reg.h:3691`.
pub(crate) const ICL_PWR_WELL_CTL_DDI2: Register =
    Register::read_write("ICL_PWR_WELL_CTL_DDI2", 0x4_5454, Meaning::BringUp, None);

/// `DC_STATE_EN`, the display C-state request.
///
/// Written to zero for the whole of a first bring-up: with DC states disabled
/// the display engine never hands power management to the DMC, which is what
/// makes a DMC-less kernel viable.  Reference §4.9 step 0 and §4.10;
/// `[I915]` `i915_reg.h:4364-4373`.
pub(crate) const DC_STATE_EN: Register =
    Register::read_write("DC_STATE_EN", 0x4_5504, Meaning::BringUp, None);

/// `DBUF_CTL_S0`; the four display buffer slice control registers.
///
/// The addresses are not monotonic in the slice number and the numbering is a
/// documented trap: `[I915]`'s slice index 0 (`DBUF_S1`) is `0x45008`, which the
/// hardware register name calls `S0`, while `0x44FE8` is both `DBUF_CTL_S1` to
/// the register name and `DBUF_S2` to the slice index.  This kernel names them
/// by register address as the reference does, and enables all four.
/// Reference §4.7; `[I915]` `skl_watermark_regs.h:54-68`.
pub(crate) const DBUF_CTL_S0: Register =
    Register::read_write("DBUF_CTL_S0", 0x4_5008, Meaning::BringUp, None);
pub(crate) const DBUF_CTL_S1: Register =
    Register::read_write("DBUF_CTL_S1", 0x4_4FE8, Meaning::BringUp, None);
pub(crate) const DBUF_CTL_S2: Register =
    Register::read_write("DBUF_CTL_S2", 0x4_4300, Meaning::BringUp, None);
pub(crate) const DBUF_CTL_S3: Register =
    Register::read_write("DBUF_CTL_S3", 0x4_4304, Meaning::BringUp, None);

/// `CDCLK_CTL`, the core display clock control register.
///
/// Reference §4.6; `[I915]` `i915_reg.h:4060-4082`.
pub(crate) const CDCLK_CTL: Register =
    Register::read_write("CDCLK_CTL", 0x4_6000, Meaning::BringUp, None);

/// `CDCLK_PLL_ENABLE`, the CDCLK PLL's enable register.
///
/// `[I915]` names this register `BXT_DE_PLL_ENABLE` and the reference calls it
/// by both names; on Gen11 and later the PLL ratio lives in this register rather
/// than in a separate control register.  Reference §4.6; `[I915]`
/// `i915_reg.h:4355-4364`.
pub(crate) const CDCLK_PLL_ENABLE: Register =
    Register::read_write("CDCLK_PLL_ENABLE", 0x4_6070, Meaning::BringUp, None);

/// `PCH_RAWCLK_FREQ`, the south display's raw clock frequency.
///
/// It must state the real crystal frequency before any south display function
/// is enabled; a wrong value mis-times GMBUS and hotplug de-glitching, which
/// presents as intermittent EDID failures rather than clean ones.
/// Reference §4.8; `[I915]` `i915_reg.h:3133-3143`.
pub(crate) const PCH_RAWCLK_FREQ: Register =
    Register::read_write("PCH_RAWCLK_FREQ", 0xC_6204, Meaning::BringUp, None);

/// `GEN8_CHICKEN_DCPR_1`, which carries `DISABLE_FLR_SRC` (bit 15).
///
/// Set before the `PW_1` request, per `Wa_16013190616` for ADL-P and its
/// subplatforms.  Reference §4.5 item 1; `[I915]` `i915_reg.h:2838-2845`.
pub(crate) const GEN8_CHICKEN_DCPR_1: Register =
    Register::read_write("GEN8_CHICKEN_DCPR_1", 0x4_6430, Meaning::BringUp, None);

/// `GEN11_CHICKEN_DCPR_2`, programmed after CDCLK and DBUF come up.
///
/// Cleared bits here are `Wa_14011508470`.  Reference §4.5 item 2 and §4.9
/// step 10; `[I915]` `i915_reg.h:2849-2853`.
pub(crate) const GEN11_CHICKEN_DCPR_2: Register =
    Register::read_write("GEN11_CHICKEN_DCPR_2", 0x4_6434, Meaning::BringUp, None);

/// `XELPD_DISPLAY_ERR_FATAL_MASK`, declared so that leaving it alone is a
/// decision rather than an omission.
///
/// `[I915]` writes all-ones here, masking every fatal display error
/// (`Wa_14011503030`).  This kernel does not: an error that is masked is an
/// error nobody sees, and a first bring-up wants the noise.  Reference §4.9
/// step 11; `[I915]` `i915_reg.h:2485`, `display/intel_display_power.c`
/// `icl_display_core_init`.
pub(crate) const XELPD_DISPLAY_ERR_FATAL_MASK: Register = Register::read_write(
    "XELPD_DISPLAY_ERR_FATAL_MASK",
    0x4_421C,
    Meaning::BringUp,
    None,
);

/// One combo PHY's register set.
///
/// The PHY blocks are structured as a base plus fixed sub-block offsets, so the
/// two instances present on `XE_LPD` differ only in the base:
///
/// | sub-block | offset within the PHY | register used here |
/// |---|---|---|
/// | `PORT_CL_DW*` | `4 * dw` | `DW5` |
/// | `PORT_COMP_DW*` | `0x100 + 4 * dw` | `DW0`, `DW1`, `DW3`, `DW8`, `DW9`, `DW10` |
/// | `PORT_TX_DW*` (group) | `0x680 + 4 * dw` | `DW8` |
/// | `PORT_TX_DW*` (lane 0) | `0x880 + 4 * dw` | `DW8`, read only |
/// | `PORT_PCS_DW*` (group) | `0x600 + 4 * dw` | `DW1` |
/// | `PORT_PCS_DW*` (lane 0) | `0x800 + 4 * dw` | `DW1`, read only |
///
/// The `PORT_TX_DW*` group base is a correction to the reference document: its
/// §8.2 table gives `+0x400` for the `PORT_TX` group, but the header it cites
/// for that table (`[I915]` `display/intel_combo_phy_regs.h:96-105`) defines
/// `_ICL_PORT_TX_GRP` as `0x680` and `_ICL_PORT_TX_LN(ln)` as
/// `0x880 + ln * 0x100`.  The `0x400` figure would address the `PORT_PCS`
/// region's tail and land on the wrong register entirely, so the offsets here
/// are the header's.  Recorded in `docs/design/intel-power.md`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ComboPhyRegisters {
    /// The name a log line uses: `A` or `B` on `XE_LPD`.
    pub(crate) port: &'static str,
    pub(crate) comp_dw0: Register,
    pub(crate) comp_dw1: Register,
    pub(crate) comp_dw3: Register,
    pub(crate) comp_dw8: Register,
    pub(crate) comp_dw9: Register,
    pub(crate) comp_dw10: Register,
    pub(crate) tx_dw8: Register,
    pub(crate) pcs_dw1: Register,
    /// Lane 0's `PORT_TX_DW8`, which is where the initialisation *reads* the
    /// value it then writes to the group register.
    pub(crate) tx_dw8_ln0: Register,
    /// Lane 0's `PORT_PCS_DW1`, read for the same reason.
    pub(crate) pcs_dw1_ln0: Register,
    pub(crate) cl_dw5: Register,
    pub(crate) phy_misc: Register,
}

impl ComboPhyRegisters {
    /// Every register of this PHY, for the table's own consistency test.
    pub(crate) const fn all(self) -> [Register; 12] {
        [
            self.comp_dw0,
            self.comp_dw1,
            self.comp_dw3,
            self.comp_dw8,
            self.comp_dw9,
            self.comp_dw10,
            self.tx_dw8,
            self.pcs_dw1,
            self.tx_dw8_ln0,
            self.pcs_dw1_ln0,
            self.cl_dw5,
            self.phy_misc,
        ]
    }
}

/// Combo PHY A, base `0x162000`, the compensation source for PHY B.
/// Reference §8.2 and §8.3; `[I915]` `intel_combo_phy_regs.h:11,17-21`.
pub(crate) const COMBO_PHY_A: ComboPhyRegisters = ComboPhyRegisters {
    port: "A",
    comp_dw0: Register::read_write("PORT_COMP_DW0(A)", 0x16_2100, Meaning::BringUp, None),
    comp_dw1: Register::read_write("PORT_COMP_DW1(A)", 0x16_2104, Meaning::BringUp, None),
    comp_dw3: Register::read_only("PORT_COMP_DW3(A)", 0x16_210C, Meaning::BringUp, None),
    comp_dw8: Register::read_write("PORT_COMP_DW8(A)", 0x16_2120, Meaning::BringUp, None),
    comp_dw9: Register::read_write("PORT_COMP_DW9(A)", 0x16_2124, Meaning::BringUp, None),
    comp_dw10: Register::read_write("PORT_COMP_DW10(A)", 0x16_2128, Meaning::BringUp, None),
    tx_dw8: Register::read_write("PORT_TX_DW8(A)", 0x16_26A0, Meaning::BringUp, None),
    pcs_dw1: Register::read_write("PORT_PCS_DW1(A)", 0x16_2604, Meaning::BringUp, None),
    tx_dw8_ln0: Register::read_only("PORT_TX_DW8_LN0(A)", 0x16_28A0, Meaning::BringUp, None),
    pcs_dw1_ln0: Register::read_only("PORT_PCS_DW1_LN0(A)", 0x16_2804, Meaning::BringUp, None),
    cl_dw5: Register::read_write("PORT_CL_DW5(A)", 0x16_2014, Meaning::BringUp, None),
    phy_misc: Register::read_write("ICL_PHY_MISC(A)", 0x6_4C00, Meaning::BringUp, None),
};

/// Combo PHY B, base `0x06C000`, a compensation sink.
/// Reference §8.2 and §8.3; `[I915]` `intel_combo_phy_regs.h:12`.
pub(crate) const COMBO_PHY_B: ComboPhyRegisters = ComboPhyRegisters {
    port: "B",
    comp_dw0: Register::read_write("PORT_COMP_DW0(B)", 0x6_C100, Meaning::BringUp, None),
    comp_dw1: Register::read_write("PORT_COMP_DW1(B)", 0x6_C104, Meaning::BringUp, None),
    comp_dw3: Register::read_only("PORT_COMP_DW3(B)", 0x6_C10C, Meaning::BringUp, None),
    comp_dw8: Register::read_write("PORT_COMP_DW8(B)", 0x6_C120, Meaning::BringUp, None),
    comp_dw9: Register::read_write("PORT_COMP_DW9(B)", 0x6_C124, Meaning::BringUp, None),
    comp_dw10: Register::read_write("PORT_COMP_DW10(B)", 0x6_C128, Meaning::BringUp, None),
    tx_dw8: Register::read_write("PORT_TX_DW8(B)", 0x6_C6A0, Meaning::BringUp, None),
    pcs_dw1: Register::read_write("PORT_PCS_DW1(B)", 0x6_C604, Meaning::BringUp, None),
    tx_dw8_ln0: Register::read_only("PORT_TX_DW8_LN0(B)", 0x6_C8A0, Meaning::BringUp, None),
    pcs_dw1_ln0: Register::read_only("PORT_PCS_DW1_LN0(B)", 0x6_C804, Meaning::BringUp, None),
    cl_dw5: Register::read_write("PORT_CL_DW5(B)", 0x6_C014, Meaning::BringUp, None),
    phy_misc: Register::read_write("ICL_PHY_MISC(B)", 0x6_4C04, Meaning::BringUp, None),
};

/// The combo PHYs `XE_LPD` has, in the order they must be initialised.
///
/// PHY A is the compensation source and PHY B its sink, so A comes first: a
/// sink initialised while its source is not produces wrong termination, which
/// does not announce itself until a link fails to train.
/// Reference §8.1 and §8.3 ("Comp source / sink"); `[I915]`
/// `display/intel_combo_phy.c:189-215` (`phy_is_master`) and
/// `intel_combo_phy_regs.h:11-21`, whose `C`/`D`/`E` instances are documented
/// there as EHL's, RKL's and ADL-S's respectively.
pub(crate) const COMBO_PHYS: &[ComboPhyRegisters] = &[COMBO_PHY_A, COMBO_PHY_B];

/// Every register in this table, for the consistency test below.
pub(crate) const POWER_AND_CLOCK_REGISTERS: &[Register] = &[
    SKL_DFSM,
    SKL_DSSM,
    SFUSE_STRAP,
    SKL_FUSE_STATUS,
    HSW_PWR_WELL_CTL1,
    HSW_PWR_WELL_CTL2,
    HSW_PWR_WELL_CTL3,
    HSW_PWR_WELL_CTL4,
    ICL_PWR_WELL_CTL_AUX2,
    ICL_PWR_WELL_CTL_DDI2,
    DC_STATE_EN,
    DBUF_CTL_S0,
    DBUF_CTL_S1,
    DBUF_CTL_S2,
    DBUF_CTL_S3,
    CDCLK_CTL,
    CDCLK_PLL_ENABLE,
    PCH_RAWCLK_FREQ,
    GEN8_CHICKEN_DCPR_1,
    GEN11_CHICKEN_DCPR_2,
    XELPD_DISPLAY_ERR_FATAL_MASK,
];

#[cfg(test)]
pub(crate) mod mock {
    //! A register window a host test can drive.
    //!
    //! The bring-up sequences are the part of this driver that cannot be run on
    //! the target machine yet, and their interesting behaviour is not the writes
    //! but what they do when a status bit does not appear, when a register is
    //! not there, or when the device drops a write.  Those are exactly the cases
    //! a real aperture never produces on demand, so the sequences are written
    //! against [`Registers`] and the tests drive them through this.
    //!
    //! A word that has not been set reads zero, as it does on a real aperture
    //! whose power well is down; that is deliberate, because "reads zero" is the
    //! failure mode the reference document spends the most words on.

    use alloc::{boxed::Box, collections::BTreeMap, vec::Vec};
    use core::cell::RefCell;

    use super::{Register, Registers};

    /// How a register's stored value is derived from the value written to it.
    type WriteHook = Box<dyn Fn(u32) -> u32>;

    /// How a register's value is derived when it is read.
    type ReadHook = Box<dyn Fn(u32) -> u32>;

    /// A mock aperture.
    pub(crate) struct MockRegisters {
        words: RefCell<BTreeMap<u32, u32>>,
        write_hooks: RefCell<BTreeMap<u32, WriteHook>>,
        read_hooks: RefCell<BTreeMap<u32, ReadHook>>,
        refused: RefCell<Vec<&'static str>>,
        hidden: RefCell<Vec<&'static str>>,
        log: RefCell<Vec<(&'static str, u32)>>,
    }

    impl MockRegisters {
        pub(crate) fn new() -> Self {
            Self {
                words: RefCell::new(BTreeMap::new()),
                write_hooks: RefCell::new(BTreeMap::new()),
                read_hooks: RefCell::new(BTreeMap::new()),
                refused: RefCell::new(Vec::new()),
                hidden: RefCell::new(Vec::new()),
                log: RefCell::new(Vec::new()),
            }
        }

        /// Set a register's value, as firmware would have left it.
        pub(crate) fn set(&self, register: Register, value: u32) {
            self.words.borrow_mut().insert(register.offset(), value);
        }

        /// Make writes to `register` derive the stored value from the written
        /// one: the status bit that follows a request bit, or a lock bit that
        /// follows an enable bit.
        pub(crate) fn derive(&self, register: Register, hook: impl Fn(u32) -> u32 + 'static) {
            self.write_hooks
                .borrow_mut()
                .insert(register.offset(), Box::new(hook));
        }

        /// Make reads of `register` derive their answer, which is how a
        /// handshake that takes several polls is modelled.
        pub(crate) fn on_read(&self, register: Register, hook: impl Fn(u32) -> u32 + 'static) {
            self.read_hooks
                .borrow_mut()
                .insert(register.offset(), Box::new(hook));
        }

        /// Make writes to `register` fail, as a read-only register or a
        /// register outside the window would.
        pub(crate) fn refuse(&self, register: Register) {
            self.refused.borrow_mut().push(register.name());
        }

        /// Make reads of `register` fail, as a register outside the mapped
        /// window does.
        pub(crate) fn hide(&self, register: Register) {
            self.hidden.borrow_mut().push(register.name());
        }

        /// Every write that happened, in order, as `(name, value)`.
        pub(crate) fn writes(&self) -> Vec<(&'static str, u32)> {
            self.log.borrow().clone()
        }

        /// How many times a register was written.
        pub(crate) fn write_count(&self, register: Register) -> usize {
            self.log
                .borrow()
                .iter()
                .filter(|(name, _)| *name == register.name())
                .count()
        }
    }

    impl Registers for MockRegisters {
        fn read(&self, register: Register) -> Option<u32> {
            if self.hidden.borrow().contains(&register.name()) {
                return None;
            }
            let stored = self
                .words
                .borrow()
                .get(&register.offset())
                .copied()
                .unwrap_or(0);
            match self.read_hooks.borrow().get(&register.offset()) {
                Some(hook) => Some(hook(stored)),
                None => Some(stored),
            }
        }

        fn read64(&self, register: Register) -> Option<u64> {
            let low = self.read(register)? as u64;
            let high = self
                .words
                .borrow()
                .get(&(register.offset() + 4))
                .copied()
                .unwrap_or(0) as u64;
            Some(low | (high << 32))
        }

        fn write(&self, register: Register, value: u32) -> bool {
            if self.refused.borrow().contains(&register.name()) {
                return false;
            }
            self.log.borrow_mut().push((register.name(), value));
            let stored = match self.write_hooks.borrow().get(&register.offset()) {
                Some(hook) => hook(value),
                None => value,
            };
            self.words.borrow_mut().insert(register.offset(), stored);
            true
        }
    }
}

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

    /// Every register of every combo PHY, as one list.
    fn every_combo_phy_register() -> vec::Vec<Register> {
        COMBO_PHYS
            .iter()
            .flat_map(|phy| phy.all())
            .collect::<vec::Vec<_>>()
    }

    #[test]
    fn every_bring_up_register_is_inside_a_window_that_needs_no_forcewake() {
        // The same three properties the probe's own table has to satisfy, for
        // the same reason: a register outside a band answers zero for the wrong
        // reason, and a register outside the window has no address at all.  A
        // bring-up register that fails this is a bug in the table, and the
        // assertion in `Register::declare` is what makes it a compile error --
        // this test covers the `all()` lists, which `declare` cannot see.
        let registers = POWER_AND_CLOCK_REGISTERS
            .iter()
            .copied()
            .chain(every_combo_phy_register());
        for register in registers {
            assert_eq!(
                register.offset() % 4,
                0,
                "{} is not dword aligned",
                register.name()
            );
            assert_eq!(register.width(), Width::Bits32, "{}", register.name());
            assert!(
                is_forcewake_free(register.offset()),
                "{} is outside the forcewake-free bands",
                register.name()
            );
            assert!(
                register.fits_in(PROBE_WINDOW),
                "{} is outside the mapped window",
                register.name()
            );
        }
    }

    #[test]
    fn the_writable_registers_are_exactly_the_ones_the_bring_up_programs() {
        // Freezing the write set is the point of this test: a register that
        // becomes writable without a sequence that needs it is the first step
        // towards a driver that pokes at hardware nobody has reasoned about.
        let writable: vec::Vec<&str> = POWER_AND_CLOCK_REGISTERS
            .iter()
            .chain(every_combo_phy_register().iter())
            .filter(|register| register.is_writable())
            .map(|register| register.name())
            .collect();
        assert_eq!(
            writable,
            vec![
                "HSW_PWR_WELL_CTL2",
                "ICL_PWR_WELL_CTL_AUX2",
                "ICL_PWR_WELL_CTL_DDI2",
                "DC_STATE_EN",
                "DBUF_CTL_S0",
                "DBUF_CTL_S1",
                "DBUF_CTL_S2",
                "DBUF_CTL_S3",
                "CDCLK_CTL",
                "CDCLK_PLL_ENABLE",
                "PCH_RAWCLK_FREQ",
                "GEN8_CHICKEN_DCPR_1",
                "GEN11_CHICKEN_DCPR_2",
                "XELPD_DISPLAY_ERR_FATAL_MASK",
                "PORT_COMP_DW0(A)",
                "PORT_COMP_DW1(A)",
                "PORT_COMP_DW8(A)",
                "PORT_COMP_DW9(A)",
                "PORT_COMP_DW10(A)",
                "PORT_TX_DW8(A)",
                "PORT_PCS_DW1(A)",
                "PORT_CL_DW5(A)",
                "ICL_PHY_MISC(A)",
                "PORT_COMP_DW0(B)",
                "PORT_COMP_DW1(B)",
                "PORT_COMP_DW8(B)",
                "PORT_COMP_DW9(B)",
                "PORT_COMP_DW10(B)",
                "PORT_TX_DW8(B)",
                "PORT_PCS_DW1(B)",
                "PORT_CL_DW5(B)",
                "ICL_PHY_MISC(B)",
            ]
        );
        // The probe reads fuses; the bring-up must never write one, and the
        // probe must never write at all.
        for register in NAMED {
            assert!(
                !register.is_writable(),
                "{} is a probe register and must stay read-only",
                register.name()
            );
        }
        for register in [SKL_DFSM, SKL_DSSM, SFUSE_STRAP, SKL_FUSE_STATUS] {
            assert!(
                !register.is_writable(),
                "{} is a fuse or a strap: read it, never write it",
                register.name()
            );
        }
        let read_only: vec::Vec<&str> = POWER_AND_CLOCK_REGISTERS
            .iter()
            .chain(every_combo_phy_register().iter())
            .filter(|register| !register.is_writable())
            .map(|register| register.name())
            .collect();
        assert_eq!(
            read_only,
            vec![
                "SKL_DFSM",
                "SKL_DSSM",
                "SFUSE_STRAP",
                "SKL_FUSE_STATUS",
                "HSW_PWR_WELL_CTL1",
                "HSW_PWR_WELL_CTL3",
                "HSW_PWR_WELL_CTL4",
                "PORT_COMP_DW3(A)",
                "PORT_TX_DW8_LN0(A)",
                "PORT_PCS_DW1_LN0(A)",
                "PORT_COMP_DW3(B)",
                "PORT_TX_DW8_LN0(B)",
                "PORT_PCS_DW1_LN0(B)",
            ]
        );
    }

    #[test]
    fn a_combo_phys_registers_are_at_the_documented_sub_block_offsets() {
        // The PHY blocks are a base plus fixed sub-block offsets, so the offset
        // of every register can be recomputed from the rule rather than
        // eyeballed from a table.  This is the test that would have caught the
        // reference document's `PORT_TX_DW*` group base: it gives `+0x400`,
        // while the header it cites gives `+0x680`, and the two cannot both
        // address `PORT_TX_DW8`.
        const COMP: u32 = 0x100;
        const TX_GRP: u32 = 0x680;
        const TX_LN0: u32 = 0x880;
        const PCS_GRP: u32 = 0x600;
        const PCS_LN0: u32 = 0x800;
        for (phy, base) in [(COMBO_PHY_A, 0x16_2000), (COMBO_PHY_B, 0x6_C000)] {
            let name = phy.port;
            assert_eq!(
                phy.comp_dw0.offset(),
                base + COMP + 4 * 0,
                "COMP_DW0({name})"
            );
            assert_eq!(
                phy.comp_dw1.offset(),
                base + COMP + 4 * 1,
                "COMP_DW1({name})"
            );
            assert_eq!(
                phy.comp_dw3.offset(),
                base + COMP + 4 * 3,
                "COMP_DW3({name})"
            );
            assert_eq!(
                phy.comp_dw8.offset(),
                base + COMP + 4 * 8,
                "COMP_DW8({name})"
            );
            assert_eq!(
                phy.comp_dw9.offset(),
                base + COMP + 4 * 9,
                "COMP_DW9({name})"
            );
            assert_eq!(
                phy.comp_dw10.offset(),
                base + COMP + 4 * 10,
                "COMP_DW10({name})"
            );
            assert_eq!(phy.tx_dw8.offset(), base + TX_GRP + 4 * 8, "TX_DW8({name})");
            assert_eq!(
                phy.pcs_dw1.offset(),
                base + PCS_GRP + 4 * 1,
                "PCS_DW1({name})"
            );
            // The lane 0 registers are read and never written: the
            // initialisation takes its starting value from lane 0 and writes
            // the result to the group register, which is what `[I915]`
            // `icl_combo_phys_init` does (`display/intel_combo_phy.c:350-359`).
            // A group write and a lane write are different facts, so the
            // distinction is kept rather than collapsed into one register.
            assert_eq!(
                phy.tx_dw8_ln0.offset(),
                base + TX_LN0 + 4 * 8,
                "TX_DW8_LN0({name})"
            );
            assert_eq!(
                phy.pcs_dw1_ln0.offset(),
                base + PCS_LN0 + 4 * 1,
                "PCS_DW1_LN0({name})"
            );
            assert!(!phy.tx_dw8_ln0.is_writable(), "TX_DW8_LN0({name})");
            assert!(!phy.pcs_dw1_ln0.is_writable(), "PCS_DW1_LN0({name})");
            assert_eq!(phy.cl_dw5.offset(), base + 4 * 5, "CL_DW5({name})");
        }
        // `ICL_PHY_MISC` is the exception: it lives in the DDI block, not in
        // the PHY block.  Reference §8.2; `[I915]` `i915_reg.h:4458-4465`.
        assert_eq!(COMBO_PHY_A.phy_misc.offset(), 0x6_4C00);
        assert_eq!(COMBO_PHY_B.phy_misc.offset(), 0x6_4C04);
        // The two PHY register sets must not overlap: an aliased register would
        // mean one PHY's init silently rewrote the other's.
        let mut offsets: vec::Vec<u32> = every_combo_phy_register()
            .iter()
            .map(|register| register.offset())
            .collect();
        let before = offsets.len();
        offsets.sort_unstable();
        offsets.dedup();
        assert_eq!(offsets.len(), before, "two PHY registers share an offset");
    }

    #[test]
    fn the_register_mock_stands_in_for_a_window() {
        // The bring-up sequences are written against `Registers`, so the trait
        // has to be usable through a plain window as well as through a mock.
        let _guard = scheduler_test_context();
        let mut scratch = Scratch::new();
        let window = scratch.window();
        let register: &dyn Registers = &window;
        assert!(register.write(DBUF_CTL_S0, 0x8000_0000));
        assert_eq!(register.read(DBUF_CTL_S0), Some(0x8000_0000));
        // A read-only register is refused through the trait exactly as it is
        // through the window.
        assert!(!register.write(SKL_DFSM, 0));
        assert_eq!(register.read64(SKL_DFSM), None);
    }
}
