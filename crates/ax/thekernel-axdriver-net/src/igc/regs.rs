//! The register map: named registers, their access rules, and bounded access.
//!
//! A NIC is a block of 32-bit registers behind a PCI memory BAR.  Everything
//! about this part that is not in configuration space is a load or a store at
//! a fixed offset, so the job of this module is to make those loads and stores
//! named, typed, bounded, and honest about what the hardware is allowed to be
//! asked.
//!
//! Three properties are deliberate, and they are the same three the Intel
//! display probe's register module states for its own aperture, because this
//! project has one way of doing this and not two:
//!
//! * **A register is a named value, not an integer.**  A caller cannot form an
//!   offset by adding to a base, and it cannot write a register the table did
//!   not declare writable.  In this commit the table declares *no* writable
//!   register at all: the identify-only probe reads and nothing else, and that
//!   is a property a test can check rather than a promise.
//! * **Every register cites where its fact came from.**  Each entry carries
//!   the Linux `igc` symbol and line the offset and access came from, and the
//!   report prints it, so a reader with the source open can check the value
//!   against the code that uses it instead of guessing.
//! * **Access is bounded by the window that was actually mapped.**  A register
//!   outside the mapped window is refused rather than followed, so a table
//!   that disagrees with the aperture produces a report line instead of a
//!   fault.
//!
//! # Where the facts come from
//!
//! Linux v6.12 `drivers/net/ethernet/intel/igc/` (tag `v6.12`, commit
//! `adc218676eef25575469234709c2d87185ca223a`): `igc_regs.h` for offsets,
//! `igc_defines.h` for bit values and field masks, `igc_mac.c` and
//! `igc_nvm.c` for the sequences that read them.  Facts only; no code is
//! copied.  The two `pci.ids` facts (vendor id) live in [`super::ids`].

use core::sync::atomic::{Ordering, compiler_fence};

/// How many bytes of BAR0 this driver maps.
///
/// The window is not the BAR: it is the part of the BAR this driver maps, and
/// every access is checked against it.  It is sized to contain every register
/// the driver names in any phase — the highest is `IGC_TDBAL(3)` at `0x0e0c0`
/// for four transmit queues — with room to spare, and the probe refuses to run
/// at all when the firmware-assigned BAR is smaller than this.
///
/// The BAR's true size is not a fact any source available here establishes:
/// Linux's `igc` maps `pci_resource_len(pdev, 0)` (`igc_main.c:6990`) and
/// never states a size, and the register offsets it names reach `0x12594`
/// (`IGC_PTM_TDELAY`, `igc_regs.h:287`), so the only lower bound this project
/// can derive is "bigger than the registers someone uses".  A window of 64 KiB
/// is a chosen bound, not a measured one, and the probe reports the BAR size
/// beside it.
pub const WINDOW_BYTES: usize = 0x1_0000;

/// Whether a register may be written.
///
/// This is a statement about *this kernel*, not about the hardware: a register
/// can be writable in the architecture and still be declared read-only here,
/// because writing it before the driver understands it would change what a
/// device the firmware left running is doing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Access {
    /// This kernel reads it and never writes it.
    ReadOnly,
    /// The driver owns the write side.  A register only reaches this state
    /// when a phase of the driver needs it, so the first write to real
    /// hardware is an addition to the table rather than a change to the rules.
    ReadWrite,
}

impl Access {
    /// The word the report uses for this access.
    pub const fn describe(self) -> &'static str {
        match self {
            Self::ReadOnly => "ro",
            Self::ReadWrite => "rw",
        }
    }
}

/// What a register's value means, so a report can say something about it
/// without matching on a register's name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Meaning {
    /// `IGC_CTRL`: device control.
    DeviceControl,
    /// `IGC_STATUS`: device status, including link state and speed.
    DeviceStatus,
    /// `IGC_EECD`: NVM/flash control and status.
    NvmControl,
    /// `IGC_RAL(0)`: the low half of receive address 0.
    ReceiveAddressLow,
    /// `IGC_RAH(0)`: the high half of receive address 0, plus its valid bit.
    ReceiveAddressHigh,
}

impl Meaning {
    /// The word the report uses for this meaning.
    pub const fn describe(self) -> &'static str {
        match self {
            Self::DeviceControl => "device control",
            Self::DeviceStatus => "device status",
            Self::NvmControl => "NVM/flash control",
            Self::ReceiveAddressLow => "receive address 0, low half",
            Self::ReceiveAddressHigh => "receive address 0, high half",
        }
    }
}

/// One 32-bit register in the device aperture.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Register {
    name: &'static str,
    offset: u32,
    access: Access,
    meaning: Meaning,
    /// The Linux symbol and line this offset and access came from.
    source: &'static str,
}

impl Register {
    /// Declare a register this kernel reads and never writes.
    pub const fn read_only(
        name: &'static str,
        offset: u32,
        meaning: Meaning,
        source: &'static str,
    ) -> Self {
        Self::declare(name, offset, Access::ReadOnly, meaning, source)
    }

    /// Declare a register this kernel both reads and writes.
    ///
    /// Nothing uses this yet.  It exists so that the first write to real
    /// hardware is a stated addition to the table.
    pub const fn read_write(
        name: &'static str,
        offset: u32,
        meaning: Meaning,
        source: &'static str,
    ) -> Self {
        Self::declare(name, offset, Access::ReadWrite, meaning, source)
    }

    const fn declare(
        name: &'static str,
        offset: u32,
        access: Access,
        meaning: Meaning,
        source: &'static str,
    ) -> Self {
        assert!(
            offset.is_multiple_of(4),
            "device register offsets are dword aligned"
        );
        assert!(
            (offset as usize) + 4 <= WINDOW_BYTES,
            "a named register must lie inside the window this driver maps"
        );
        Self {
            name,
            offset,
            access,
            meaning,
            source,
        }
    }

    /// The register's name, as the Linux source spells it.
    pub const fn name(self) -> &'static str {
        self.name
    }

    /// The register's offset from the start of the aperture.
    pub const fn offset(self) -> u32 {
        self.offset
    }

    /// Whether this kernel may write it.
    pub const fn access(self) -> Access {
        self.access
    }

    /// What its value means.
    pub const fn meaning(self) -> Meaning {
        self.meaning
    }

    /// Where the fact came from.
    pub const fn source(self) -> &'static str {
        self.source
    }

    /// Whether this kernel may write it.
    pub const fn is_writable(self) -> bool {
        matches!(self.access, Access::ReadWrite)
    }

    /// Whether a window of `len` bytes contains this register whole.
    pub const fn fits_in(self, len: usize) -> bool {
        (self.offset as usize) + 4 <= len
    }
}

/// The registers this driver names, in report order.
///
/// This is the identify-only phase's set: the registers the part answers with
/// before anything has been programmed, chosen so that each one answers a
/// question rather than filling a table.  What the device ids claim is checked
/// against what these say, and a window that answers nothing is reported as a
/// window that answered nothing.
///
/// The registers a driver needs to reset the part, bring the link up and run
/// descriptor rings are deliberately absent: they arrive with the phases that
/// use them, so that the table is always exactly the set of registers this
/// kernel has a reason to touch.
///
/// One register a reader will miss is `IGC_GPHY_VERSION` (`igc_regs.h:17`,
/// offset `0x0001e`), which `igc_regs.h` lists in its "General Register
/// Descriptions" block with the comment "I225 gPHY Firmware Version".  It is
/// **not** an aperture register: its offset is not dword aligned, and the only
/// code that reads it, `igc_phy.c` `igc_read_phy_fw_version`, goes through
/// `igc_read_phy_reg_gpy` and then `igc_read_phy_reg_mdic` -- that is, it is a
/// register inside the integrated PHY, reached over the MDIC command register
/// at `0x00020`.  Reading it therefore requires *writing* MDIC to start the
/// transaction, which is exactly what this phase must not do.  The
/// dword-alignment assertion in [`Register::declare`] is what caught this: the
/// first version of this table listed `0x0001e` as an aperture register and
/// the compile-time check refused it.
pub const NAMED: &[Register] = &[
    Register::read_only(
        "IGC_CTRL",
        0x00000,
        Meaning::DeviceControl,
        "igc_regs.h:8 (IGC_CTRL, \"Device Control - RW\")",
    ),
    Register::read_only(
        "IGC_STATUS",
        0x00008,
        Meaning::DeviceStatus,
        "igc_regs.h:9 (IGC_STATUS, \"Device Status - RO\")",
    ),
    Register::read_only(
        "IGC_EECD",
        0x00010,
        Meaning::NvmControl,
        "igc_regs.h:10 (IGC_EECD, \"EEPROM/Flash Control - RW\")",
    ),
    Register::read_only(
        "IGC_RAL(0)",
        0x05400,
        Meaning::ReceiveAddressLow,
        "igc_regs.h:114 (IGC_RAL(_n)); read by igc_nvm.c:139 igc_read_mac_addr",
    ),
    Register::read_only(
        "IGC_RAH(0)",
        0x05404,
        Meaning::ReceiveAddressHigh,
        "igc_regs.h:115 (IGC_RAH(_n)); read by igc_nvm.c:138 igc_read_mac_addr",
    ),
];

/// The highest offset any named register occupies, exclusive.
///
/// The probe compares this against the BAR the firmware assigned: a BAR too
/// small to contain the registers the driver names is a device this driver
/// must not touch.
pub const NAMED_SPAN: usize = {
    let mut highest = 0;
    let mut index = 0;
    while index < NAMED.len() {
        let end = NAMED[index].offset() as usize + 4;
        if end > highest {
            highest = end;
        }
        index += 1;
    }
    highest
};

/// Every named register lies inside the mapped window, checked at compile
/// time: a register that does not cannot be compiled.
const _: () = assert!(NAMED_SPAN <= WINDOW_BYTES);

/// The named register called `name`, or `None`.
pub fn named(name: &str) -> Option<Register> {
    NAMED.iter().copied().find(|register| register.name == name)
}

/// Bit values, named as the Linux source names them.
///
/// Each carries the symbol and line it came from; the tests check the values
/// against the comments, so a transcription slip is a test failure rather than
/// a device that behaves oddly.
pub mod bits {
    /// `IGC_CTRL_RST` — global reset (`igc_defines.h:132`).
    pub const CTRL_RST: u32 = 0x0400_0000;
    /// `IGC_CTRL_PHY_RST` — PHY reset (`igc_defines.h:134`).
    pub const CTRL_PHY_RST: u32 = 0x8000_0000;
    /// `IGC_CTRL_SLU` — set link up (`igc_defines.h:135`).
    pub const CTRL_SLU: u32 = 0x0000_0040;
    /// `IGC_CTRL_FRCSPD` — force speed (`igc_defines.h:136`).
    pub const CTRL_FRCSPD: u32 = 0x0000_0800;
    /// `IGC_CTRL_FRCDPX` — force duplex (`igc_defines.h:137`).
    pub const CTRL_FRCDPX: u32 = 0x0000_1000;
    /// `IGC_CTRL_GIO_MASTER_DISABLE` (`igc_defines.h:97`).
    pub const CTRL_GIO_MASTER_DISABLE: u32 = 0x0000_0004;

    /// `IGC_STATUS_FD` — full duplex (`igc_defines.h:223`).
    pub const STATUS_FD: u32 = 0x0000_0001;
    /// `IGC_STATUS_LU` — link up (`igc_defines.h:224`).
    pub const STATUS_LU: u32 = 0x0000_0002;
    /// `IGC_STATUS_FUNC_MASK` — PCI function (`igc_defines.h:225`).
    pub const STATUS_FUNC_MASK: u32 = 0x0000_000c;
    /// `IGC_STATUS_FUNC_SHIFT` (`igc_defines.h:226`).
    pub const STATUS_FUNC_SHIFT: u32 = 2;
    /// `IGC_STATUS_TXOFF` — transmission paused (`igc_defines.h:227`).
    pub const STATUS_TXOFF: u32 = 0x0000_0010;
    /// `IGC_STATUS_SPEED_100` — 100 Mb/s (`igc_defines.h:228`).
    pub const STATUS_SPEED_100: u32 = 0x0000_0040;
    /// `IGC_STATUS_SPEED_1000` — 1000 Mb/s (`igc_defines.h:229`).
    pub const STATUS_SPEED_1000: u32 = 0x0000_0080;
    /// `IGC_STATUS_SPEED_2500` — the extra bit that makes it 2.5 Gb/s
    /// (`igc_defines.h:230`).
    pub const STATUS_SPEED_2500: u32 = 0x0040_0000;

    /// `IGC_STATUS_GIO_MASTER_ENABLE` (`igc_defines.h:99`).
    pub const STATUS_GIO_MASTER_ENABLE: u32 = 0x0008_0000;

    /// `IGC_EECD_AUTO_RD` — NVM auto-read done (`igc_defines.h:187`).
    pub const EECD_AUTO_RD: u32 = 0x0000_0200;
    /// `IGC_EECD_SIZE_EX_MASK` — NVM size, in the extended encoding
    /// (`igc_defines.h:193`).
    pub const EECD_SIZE_EX_MASK: u32 = 0x0000_7800;
    /// `IGC_EECD_SIZE_EX_SHIFT` (`igc_defines.h:194`).
    pub const EECD_SIZE_EX_SHIFT: u32 = 11;
    /// `IGC_EECD_FLASH_DETECTED_I225` (`igc_defines.h:197`).
    pub const EECD_FLASH_DETECTED_I225: u32 = 0x0008_0000;

    /// `IGC_RAH_AV` — receive address valid (`igc_defines.h:114`).
    pub const RAH_AV: u32 = 0x8000_0000;
    /// `IGC_RAH_RAH_MASK` — the upper two bytes of the address
    /// (`igc_defines.h:108`).
    pub const RAH_ADDR_MASK: u32 = 0x0000_ffff;
}

/// Link speed, as the part reports it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Speed {
    /// 10 Mb/s.
    Mbit10,
    /// 100 Mb/s.
    Mbit100,
    /// 1000 Mb/s.
    Mbit1000,
    /// 2500 Mb/s.
    Mbit2500,
}

impl Speed {
    /// The speed in Mb/s.
    pub const fn mbps(self) -> u16 {
        match self {
            Self::Mbit10 => 10,
            Self::Mbit100 => 100,
            Self::Mbit1000 => 1000,
            Self::Mbit2500 => 2500,
        }
    }

    /// How the report spells it.
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Mbit10 => "10 Mb/s",
            Self::Mbit100 => "100 Mb/s",
            Self::Mbit1000 => "1000 Mb/s",
            Self::Mbit2500 => "2500 Mb/s",
        }
    }
}

/// `IGC_STATUS`: link state, speed, duplex and function number.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceStatus(u32);

impl DeviceStatus {
    /// Interpret a raw register value.
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// The raw value.
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// `IGC_STATUS_LU`: the MAC reports link.
    pub const fn link_up(self) -> bool {
        self.0 & bits::STATUS_LU != 0
    }

    /// `IGC_STATUS_FD`: full duplex when set.
    pub const fn full_duplex(self) -> bool {
        self.0 & bits::STATUS_FD != 0
    }

    /// `IGC_STATUS_TXOFF`: the transmitter is paused.
    pub const fn transmit_paused(self) -> bool {
        self.0 & bits::STATUS_TXOFF != 0
    }

    /// `IGC_STATUS_FUNC`: which PCI function this is.
    ///
    /// `igc_base.c:165` reads it the same way when it sets the LAN id.
    pub const fn function_id(self) -> u8 {
        ((self.0 & bits::STATUS_FUNC_MASK) >> bits::STATUS_FUNC_SHIFT) as u8
    }

    /// The link speed, decoded exactly as `igc_mac.c:681-712`
    /// (`igc_get_speed_and_duplex_copper`) decodes it.
    ///
    /// The encoding is not monotonic: `IGC_STATUS_SPEED_1000` means *1 Gb/s or
    /// faster*, and `IGC_STATUS_SPEED_2500` is an extra bit that distinguishes
    /// 2.5 Gb/s from 1 Gb/s.  A `SPEED_2500` without `SPEED_1000` therefore
    /// decodes as 10 Mb/s — which is what the vendor driver does, and the test
    /// below pins that behaviour rather than inventing a nicer one.
    pub const fn speed(self) -> Speed {
        if self.0 & bits::STATUS_SPEED_1000 != 0 {
            if self.0 & bits::STATUS_SPEED_2500 != 0 {
                Speed::Mbit2500
            } else {
                Speed::Mbit1000
            }
        } else if self.0 & bits::STATUS_SPEED_100 != 0 {
            Speed::Mbit100
        } else {
            Speed::Mbit10
        }
    }
}

/// `IGC_CTRL`: device control.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceControl(u32);

impl DeviceControl {
    /// Interpret a raw register value.
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// The raw value.
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// `IGC_CTRL_SLU` is set, so the MAC is driving the link up.
    pub const fn set_link_up(self) -> bool {
        self.0 & bits::CTRL_SLU != 0
    }

    /// `IGC_CTRL_RST` is set, so a global reset is in progress or pending.
    pub const fn reset_asserted(self) -> bool {
        self.0 & bits::CTRL_RST != 0
    }

    /// `IGC_CTRL_GIO_MASTER_DISABLE` is set.
    pub const fn master_disabled(self) -> bool {
        self.0 & bits::CTRL_GIO_MASTER_DISABLE != 0
    }
}

/// `IGC_EECD`: NVM/flash control and status.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NvmControl(u32);

impl NvmControl {
    /// Interpret a raw register value.
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// The raw value.
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// `IGC_EECD_AUTO_RD`: the hardware has finished reading the NVM into the
    /// receive-address registers.
    ///
    /// `igc_mac.c:650-674` (`igc_get_auto_rd_done`) polls this bit after a
    /// reset; it is the only signal this driver has that the MAC address in
    /// `IGC_RAL(0)`/`IGC_RAH(0)` is the one the NVM holds.
    pub const fn auto_read_done(self) -> bool {
        self.0 & bits::EECD_AUTO_RD != 0
    }

    /// `IGC_EECD_FLASH_DETECTED_I225`: flash, rather than an EEPROM, is
    /// attached.  `igc_i225.c:459` (`igc_get_flash_presence_i225`) reads it.
    pub const fn flash_detected(self) -> bool {
        self.0 & bits::EECD_FLASH_DETECTED_I225 != 0
    }

    /// `IGC_EECD_SIZE_EX_MASK`: the NVM size field.
    ///
    /// What the field's value *means* is not established here: `igc_base.c`
    /// derives a word size from it inside `igc_init_nvm_params_base`, but the
    /// driver never needs the size, so this returns the raw field rather than
    /// a byte count this project cannot source.
    pub const fn size_field(self) -> u16 {
        ((self.0 & bits::EECD_SIZE_EX_MASK) >> bits::EECD_SIZE_EX_SHIFT) as u16
    }
}

/// `IGC_RAH(0)`: the upper half of receive address 0.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReceiveAddressHigh(u32);

impl ReceiveAddressHigh {
    /// Interpret a raw register value.
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// The raw value.
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// `IGC_RAH_AV`: the receive address in this register pair is valid.
    pub const fn address_valid(self) -> bool {
        self.0 & bits::RAH_AV != 0
    }

    /// The upper two bytes of the address.  `igc_nvm.c:144-145` takes them the
    /// same way.
    pub const fn address_high(self) -> u16 {
        (self.0 & bits::RAH_ADDR_MASK) as u16
    }
}

/// The six bytes of a receive address, assembled the way `igc_nvm.c`
/// `igc_read_mac_addr` assembles them: the low register holds bytes 0..3
/// little-endian, the high register holds bytes 4..5.
pub const fn assemble_receive_address(low: u32, high: ReceiveAddressHigh) -> [u8; 6] {
    let mut address = [0u8; 6];
    address[0] = low as u8;
    address[1] = (low >> 8) as u8;
    address[2] = (low >> 16) as u8;
    address[3] = (low >> 24) as u8;
    let upper = high.address_high();
    address[4] = upper as u8;
    address[5] = (upper >> 8) as u8;
    address
}

/// A mapped window of the device aperture.
///
/// The window is not the BAR; it is the part of the BAR this driver mapped,
/// and every access is checked against it.
#[derive(Clone, Copy, Debug)]
pub struct RegisterWindow {
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
    /// such device.
    pub const unsafe fn from_mapped(base: usize, len: usize) -> Self {
        Self { base, len }
    }

    /// The length of the mapped window in bytes.
    pub const fn len(self) -> usize {
        self.len
    }

    /// The address the window starts at.
    pub const fn base(self) -> usize {
        self.base
    }

    /// Whether the window contains `register` whole.
    pub const fn contains(self, register: Register) -> bool {
        register.fits_in(self.len)
    }

    /// Read a register, or `None` when it lies outside the mapped window.
    ///
    /// Reading a register this part does not implement, or one whose engine is
    /// not clocked, is a normal read: it returns whatever the bus returns and
    /// changes nothing.  That is why a probe can read first and interpret
    /// afterwards -- and why the register table, not the read, is where the
    /// safety argument lives.
    pub fn read(self, register: Register) -> Option<u32> {
        let address = register.offset() as usize;
        if address + 4 > self.len {
            return None;
        }
        // SAFETY: the offset was just checked to lie inside a window this
        // value promises is mapped device memory, and a 32-bit register access
        // is naturally aligned because the register table is dword aligned.
        // Nothing else in the kernel aliases it: the aperture belongs to this
        // device and is mapped uncached, so a read cannot be served from a
        // stale cache line.
        let value = unsafe { core::ptr::read_volatile((self.base + address) as *const u32) };
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
    pub fn write(self, register: Register, value: u32) -> bool {
        if !register.is_writable() {
            return false;
        }
        let address = register.offset() as usize;
        if address + 4 > self.len {
            return false;
        }
        // Keep the value from being computed after the store, and the store
        // from being sunk past whatever the caller does next.
        compiler_fence(Ordering::Release);
        // SAFETY: as for `read`, and the register table is the only source of
        // offsets, so the store is to a dword-aligned register inside the
        // mapped aperture.
        unsafe { core::ptr::write_volatile((self.base + address) as *mut u32, value) };
        compiler_fence(Ordering::SeqCst);
        true
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    /// A buffer standing in for an aperture, sized exactly like the window the
    /// driver maps.  Device memory is ordinary memory with rules, so the
    /// volatile access path can be tested against it as long as the window is
    /// built to point at it.
    struct Scratch {
        words: vec::Vec<u32>,
    }

    impl Scratch {
        fn new() -> Self {
            Self {
                words: vec![0; WINDOW_BYTES / 4],
            }
        }

        fn window(&mut self) -> RegisterWindow {
            // SAFETY: `words` is a live, 4-byte aligned buffer of exactly
            // `WINDOW_BYTES` bytes that outlives both the window and this
            // borrow, and the windows built over it are the only way the
            // buffer is reached once the borrow ends.
            unsafe { RegisterWindow::from_mapped(self.words.as_mut_ptr() as usize, WINDOW_BYTES) }
        }
    }

    #[test]
    fn every_named_register_is_inside_the_mapped_window() {
        for register in NAMED {
            assert!(
                register.fits_in(WINDOW_BYTES),
                "{} does not fit in the window",
                register.name()
            );
        }
        assert_eq!(NAMED_SPAN, 0x05408, "the highest named register end");
    }

    #[test]
    fn every_named_register_is_dword_aligned_which_is_how_the_phy_register_was_caught() {
        // `IGC_GPHY_VERSION` is listed in igc_regs.h beside these and is *not*
        // an aperture register: its offset 0x1e is not dword aligned, and it is
        // read through MDIC by igc_phy.c igc_read_phy_fw_version.  The
        // assertion in `Register::declare` is what turned that into a compile
        // error rather than a probe that read a PHY register as if it were an
        // aperture register.
        assert!(
            !(0x0001eu32).is_multiple_of(4),
            "0x1e is the PHY register's offset, and it is not an aperture offset"
        );
        for register in NAMED {
            assert!(
                register.offset().is_multiple_of(4),
                "{} is not dword aligned",
                register.name()
            );
        }
    }

    #[test]
    fn the_identify_only_table_declares_no_writable_register() {
        // This is the property that makes "identify, do not program" checkable
        // rather than promised: with no writable entry, `write` refuses every
        // register in the table.
        for register in NAMED {
            assert_eq!(
                register.access(),
                Access::ReadOnly,
                "{} is writable in a phase that must not write",
                register.name()
            );
            assert!(!register.is_writable());
        }
    }

    #[test]
    fn a_read_returns_what_the_aperture_holds() {
        let mut scratch = Scratch::new();
        let window = scratch.window();
        scratch.words[0x00008 / 4] = 0xdead_beef;
        scratch.words[0x05400 / 4] = 0x3322_1100;
        assert_eq!(window.read(named("IGC_STATUS").unwrap()), Some(0xdead_beef));
        assert_eq!(window.read(named("IGC_RAL(0)").unwrap()), Some(0x3322_1100));
    }

    #[test]
    fn a_read_of_a_register_outside_the_window_is_refused() {
        let mut scratch = Scratch::new();
        let start = scratch.words.as_mut_ptr() as usize;
        // A window that stops one dword short of the status register.
        let window = unsafe { RegisterWindow::from_mapped(start, 0x8) };
        assert!(window.read(named("IGC_CTRL").unwrap()).is_some());
        assert_eq!(window.read(named("IGC_STATUS").unwrap()), None);
        assert_eq!(window.read(named("IGC_RAL(0)").unwrap()), None);
        assert!(!window.contains(named("IGC_EECD").unwrap()));
        assert!(!window.write(named("IGC_CTRL").unwrap(), 1));
    }

    #[test]
    fn the_table_names_the_registers_the_linux_driver_names_at_the_same_offsets() {
        // Each pair is (name, offset, the Linux symbol and the line the value
        // is defined on).  If a transcription is wrong, this fails.
        let expected: &[(&str, u32, &str)] = &[
            ("IGC_CTRL", 0x00000, "igc_regs.h:8"),
            ("IGC_STATUS", 0x00008, "igc_regs.h:9"),
            ("IGC_EECD", 0x00010, "igc_regs.h:10"),
            ("IGC_RAL(0)", 0x05400, "igc_regs.h:114"),
            ("IGC_RAH(0)", 0x05404, "igc_regs.h:115"),
        ];
        assert_eq!(expected.len(), NAMED.len());
        for (name, offset, source) in expected {
            let register = named(name).unwrap_or_else(|| panic!("{name} is not in the table"));
            assert_eq!(register.offset(), *offset, "{name}");
            assert!(
                register.source().starts_with(source),
                "{name} cites {:?}, expected it to start with {source}",
                register.source()
            );
        }
    }

    #[test]
    fn the_bit_values_match_the_linux_defines() {
        // igc_defines.h, by line.
        assert_eq!(bits::CTRL_GIO_MASTER_DISABLE, 0x0000_0004); // :97
        assert_eq!(bits::CTRL_RST, 0x0400_0000); // :132
        assert_eq!(bits::CTRL_PHY_RST, 0x8000_0000); // :134
        assert_eq!(bits::CTRL_SLU, 0x0000_0040); // :135
        assert_eq!(bits::CTRL_FRCSPD, 0x0000_0800); // :136
        assert_eq!(bits::CTRL_FRCDPX, 0x0000_1000); // :137
        assert_eq!(bits::STATUS_FD, 0x0000_0001); // :223
        assert_eq!(bits::STATUS_LU, 0x0000_0002); // :224
        assert_eq!(bits::STATUS_FUNC_MASK, 0x0000_000c); // :225
        assert_eq!(bits::STATUS_FUNC_SHIFT, 2); // :226
        assert_eq!(bits::STATUS_TXOFF, 0x0000_0010); // :227
        assert_eq!(bits::STATUS_SPEED_100, 0x0000_0040); // :228
        assert_eq!(bits::STATUS_SPEED_1000, 0x0000_0080); // :229
        assert_eq!(bits::STATUS_SPEED_2500, 0x0040_0000); // :230
        assert_eq!(bits::STATUS_GIO_MASTER_ENABLE, 0x0008_0000); // :99
        assert_eq!(bits::EECD_AUTO_RD, 0x0000_0200); // :187
        assert_eq!(bits::EECD_SIZE_EX_MASK, 0x0000_7800); // :193
        assert_eq!(bits::EECD_SIZE_EX_SHIFT, 11); // :194
        assert_eq!(bits::EECD_FLASH_DETECTED_I225, 0x0008_0000); // :197
        assert_eq!(bits::RAH_AV, 0x8000_0000); // :114
        assert_eq!(bits::RAH_ADDR_MASK, 0x0000_ffff); // :108
    }

    #[test]
    fn the_speed_field_decodes_the_way_the_vendor_driver_decodes_it() {
        // igc_mac.c igc_get_speed_and_duplex_copper.
        assert_eq!(DeviceStatus::new(0).speed(), Speed::Mbit10);
        assert_eq!(
            DeviceStatus::new(bits::STATUS_SPEED_100).speed(),
            Speed::Mbit100
        );
        assert_eq!(
            DeviceStatus::new(bits::STATUS_SPEED_1000).speed(),
            Speed::Mbit1000
        );
        assert_eq!(
            DeviceStatus::new(bits::STATUS_SPEED_1000 | bits::STATUS_SPEED_2500).speed(),
            Speed::Mbit2500
        );
        // Both low bits set: 1 Gb/s wins, because the vendor driver tests
        // SPEED_1000 first.
        assert_eq!(
            DeviceStatus::new(bits::STATUS_SPEED_100 | bits::STATUS_SPEED_1000).speed(),
            Speed::Mbit1000
        );
        // The odd one: the 2.5 Gb/s discriminator without the 1 Gb/s bit is
        // 10 Mb/s in this encoding, and this test pins that rather than
        // silently "fixing" it into something the hardware does not promise.
        assert_eq!(
            DeviceStatus::new(bits::STATUS_SPEED_2500).speed(),
            Speed::Mbit10
        );
    }

    #[test]
    fn the_status_fields_decode() {
        let status = DeviceStatus::new(
            bits::STATUS_LU | bits::STATUS_FD | bits::STATUS_SPEED_1000 | (2 << 2),
        );
        assert!(status.link_up());
        assert!(status.full_duplex());
        assert!(!status.transmit_paused());
        assert_eq!(status.function_id(), 2);
        assert_eq!(status.speed(), Speed::Mbit1000);
        assert_eq!(status.speed().mbps(), 1000);

        let half_duplex_down = DeviceStatus::new((3 << 2) | bits::STATUS_TXOFF);
        assert!(!half_duplex_down.link_up());
        assert!(!half_duplex_down.full_duplex());
        assert!(half_duplex_down.transmit_paused());
        assert_eq!(half_duplex_down.function_id(), 3);
    }

    #[test]
    fn the_control_and_nvm_fields_decode() {
        let control = DeviceControl::new(bits::CTRL_SLU | bits::CTRL_RST);
        assert!(control.set_link_up());
        assert!(control.reset_asserted());
        assert!(!control.master_disabled());

        let eecd = NvmControl::new(bits::EECD_AUTO_RD | bits::EECD_FLASH_DETECTED_I225 | (5 << 11));
        assert!(eecd.auto_read_done());
        assert!(eecd.flash_detected());
        assert_eq!(eecd.size_field(), 5);

        let bare = NvmControl::new(0);
        assert!(!bare.auto_read_done());
        assert!(!bare.flash_detected());
        assert_eq!(bare.size_field(), 0);
    }

    #[test]
    fn the_receive_address_assembles_little_endian_low_then_high() {
        // igc_nvm.c igc_read_mac_addr: byte 0 is the low byte of RAL, byte 5
        // is the high byte of RAH's low half.
        let low = 0x3322_1100u32;
        let high = ReceiveAddressHigh::new(0x8000_0000 | 0x5544);
        assert_eq!(
            assemble_receive_address(low, high),
            [0x00, 0x11, 0x22, 0x33, 0x44, 0x55]
        );
        assert!(high.address_valid());
        assert_eq!(high.address_high(), 0x5544);

        // The valid bit is not part of the address, and a zero address with
        // the valid bit clear must not become a valid one.
        let invalid = ReceiveAddressHigh::new(0);
        assert!(!invalid.address_valid());
        assert_eq!(assemble_receive_address(0, invalid), [0; 6]);
    }

    #[test]
    fn a_write_to_a_read_only_register_is_refused_and_leaves_the_aperture_alone() {
        let mut scratch = Scratch::new();
        let window = scratch.window();
        scratch.words[0x00000 / 4] = 0x1234_5678;
        assert!(!window.write(named("IGC_CTRL").unwrap(), 0xffff_ffff));
        assert_eq!(window.read(named("IGC_CTRL").unwrap()), Some(0x1234_5678));
    }

    #[test]
    fn a_read_write_register_can_be_written_and_read_back() {
        // No such register is in this phase's table; the window's write path
        // is exercised with a declared-writable entry so that the rule it
        // enforces is the table's `Access`, not the presence of a register.
        let probe = Register::read_write("IGC_TEST", 0x00020, Meaning::NvmControl, "test");
        let mut scratch = Scratch::new();
        let window = scratch.window();
        assert!(window.write(probe, 0x5a5a_5a5a));
        assert_eq!(window.read(probe), Some(0x5a5a_5a5a));
        // And the compile-time check rejects a register outside the window.
        assert!(!probe.fits_in(0x10));
    }

    #[test]
    fn a_register_is_named_by_its_linux_spelling() {
        assert_eq!(named("IGC_STATUS").unwrap().meaning(), Meaning::DeviceStatus);
        assert_eq!(named("IGC_RAL(0)").unwrap().meaning(), Meaning::ReceiveAddressLow);
        assert_eq!(named("IGC_RAH(0)").unwrap().meaning(), Meaning::ReceiveAddressHigh);
        assert!(named("IGC_RCTL").is_none(), "not named in this phase");
        assert!(named("igc_status").is_none(), "names are exact");
    }
}
