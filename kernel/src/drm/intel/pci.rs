//! The PCI side of the Intel display probe.
//!
//! An Intel display device is not handed to a probe callback by a bus driver;
//! it *is* a PCI function, and everything this kernel can know about it before
//! it touches a register comes out of that function's configuration header.
//! This module reads that header.
//!
//! **The probe reads configuration space and never writes it.**  That is a
//! policy, not an omission.  Sizing a BAR, assigning one, and enabling decode
//! all require writing the header, and each of them can disturb a device the
//! firmware is still driving -- on the target machine the firmware framebuffer
//! *is* the console, and it lives inside this GPU's aperture.  A probe that has
//! not yet identified the device has no business reprogramming it, so
//! [`ConfigSpace`] simply has no write method.  A driver that owns the device
//! can add one later as a deliberate, named act.
//!
//! Configuration space itself is reached through the platform's ECAM aperture,
//! the mechanism `axdriver`'s bus probe and the platform's uncore performance
//! monitor already use: the platform maps `axconfig::devices::PCI_ECAM_BASE`
//! as device memory while building the kernel address space, and a function's
//! header is a calculated offset into that window.  This module deliberately
//! computes that offset the same way rather than inventing a second route to
//! configuration space.

use alloc::{format, string::String, vec::Vec};
use core::fmt;

/// The PCI vendor identifier of Intel.
pub(crate) const VENDOR_INTEL: u16 = 0x8086;

/// What a configuration read returns for a function that is not there.
pub(crate) const NO_FUNCTION: u16 = 0xffff;

/// The display-controller base class.
pub(crate) const CLASS_DISPLAY: u8 = 0x03;

/// The VGA-compatible display subclass, which is what an integrated GPU
/// reports when it owns the boot framebuffer.
pub(crate) const SUBCLASS_VGA: u8 = 0x00;

/// The "other display controller" subclass, reported when firmware left the
/// VGA console somewhere else, or nowhere.
pub(crate) const SUBCLASS_OTHER_DISPLAY: u8 = 0x80;

/// The standard (type 0) header, the only one whose BARs live at 0x10..0x28.
pub(crate) const HEADER_TYPE_STANDARD: u8 = 0x00;

/// Number of 32-bit BAR slots in a type 0 header.
pub(crate) const BAR_SLOTS: usize = 6;

/// Configuration-space offsets the probe reads, all dword aligned.
pub(crate) mod offset {
    pub(crate) const VENDOR_ID: u16 = 0x00;
    pub(crate) const DEVICE_ID: u16 = 0x02;
    pub(crate) const COMMAND: u16 = 0x04;
    pub(crate) const REVISION_ID: u16 = 0x08;
    pub(crate) const PROG_IF: u16 = 0x09;
    pub(crate) const SUBCLASS: u16 = 0x0a;
    pub(crate) const CLASS: u16 = 0x0b;
    pub(crate) const HEADER_TYPE: u16 = 0x0e;
    pub(crate) const SUBSYSTEM_VENDOR_ID: u16 = 0x2c;
    pub(crate) const SUBSYSTEM_ID: u16 = 0x2e;
    /// First BAR slot; each slot is one dword.
    pub(crate) const BAR0: u16 = 0x10;
}

/// The configuration-space dword offset of BAR `slot`.
pub(crate) const fn bar_offset(slot: u8) -> u16 {
    offset::BAR0 + (slot as u16) * 4
}

/// A bus/device/function address on the single PCI segment this platform has.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct Bdf {
    pub(crate) bus: u8,
    pub(crate) device: u8,
    pub(crate) function: u8,
}

impl Bdf {
    pub(crate) const fn new(bus: u8, device: u8, function: u8) -> Self {
        Self {
            bus,
            device,
            function,
        }
    }

    /// The packed device/function byte ECAM places at bits 15:8 of a device's
    /// offset.
    pub(crate) const fn devfn(self) -> u8 {
        (self.device << 3) | self.function
    }
}

impl fmt::Display for Bdf {
    /// The `dddd:bb:dd.f` spelling every tool and every human uses.  The
    /// implied segment zero is written out because a log line is read out of
    /// context far more often than it is read next to `lspci`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "0000:{:02x}:{:02x}.{}",
            self.bus, self.device, self.function
        )
    }
}

/// One function's configuration space is at most 4 KiB, and ECAM gives each
/// function exactly one such window.
pub(crate) const CONFIG_WINDOW: usize = 4096;

/// The address of `offset` inside `bdf`'s configuration space, or `None` when
/// the request is not representable.
///
/// This is the whole of the ECAM addressing rule, and it is deliberately a
/// pure function: it is the part whose mistakes read wild memory, so the host
/// tests drive it through a table of cases instead of leaving it to be
/// exercised for the first time on hardware.
///
/// The layout is fixed by the PCI specification: bits 27:20 select the bus,
/// 19:15 the device, 14:12 the function, and 11:0 the dword inside the
/// function's 4 KiB window.  `bus_end` is the highest bus the platform
/// configures; a request beyond it is refused rather than aimed at address
/// space no bridge decodes.
pub(crate) fn config_address(
    ecam_base: usize,
    bus_end: u8,
    bdf: Bdf,
    offset: u16,
    width: usize,
) -> Option<usize> {
    if bdf.device > 31 || bdf.function > 7 || bdf.bus > bus_end {
        return None;
    }
    let offset = usize::from(offset);
    if offset & 3 != 0 || width == 0 || width > 4 {
        return None;
    }
    if offset.checked_add(width)? > CONFIG_WINDOW {
        return None;
    }
    ecam_base
        .checked_add(usize::from(bdf.bus) << 20)?
        .checked_add(usize::from(bdf.device) << 15)?
        .checked_add(usize::from(bdf.function) << 12)?
        .checked_add(offset)
}

/// A bus whose functions can be interrogated.
///
/// The trait has no write half on purpose; see the module comment.  It exists
/// so that the enumeration -- header decoding, filtering, the BAR walk -- is
/// one implementation that the host tests run against a synthetic bus and the
/// kernel runs against real ECAM.
pub(crate) trait ConfigSpace {
    /// Read the dword at `offset`, or `None` if the bus cannot be reached.
    fn read_u32(&self, bdf: Bdf, offset: u16) -> Option<u32>;

    /// Read the word at `offset`.  Configuration space is little endian, so a
    /// 16-bit register at a 2-byte aligned offset is one half of its dword --
    /// the low half at 0x00, 0x04, ..., and the high half at 0x02, 0x06, ...
    /// Implementations only need to provide [`ConfigSpace::read_u32`].
    fn read_u16(&self, bdf: Bdf, offset: u16) -> Option<u16> {
        if offset & 1 != 0 {
            return None;
        }
        let dword = self.read_u32(bdf, offset & !3)?;
        Some((dword >> ((offset & 2) * 8)) as u16)
    }

    /// Read the byte at `offset`; see [`ConfigSpace::read_u16`] for the byte
    /// order.
    fn read_u8(&self, bdf: Bdf, offset: u16) -> Option<u8> {
        let dword = self.read_u32(bdf, offset & !3)?;
        Some((dword >> ((offset & 3) * 8)) as u8)
    }
}

/// How wide a memory BAR's address is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BarWidth {
    Bits32,
    Bits64,
}

impl fmt::Display for BarWidth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bits32 => f.write_str("32-bit"),
            Self::Bits64 => f.write_str("64-bit"),
        }
    }
}

/// What one header slot means.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BarKind {
    /// The slot declares no address: the header reads it back as zero, or as
    /// all ones on a device that does not implement the slot.  There is no BAR
    /// here to map, and no size to report.
    Unimplemented,
    /// The second half of the 64-bit memory BAR declared in the previous slot.
    /// It carries address bits 63:32 and is never a BAR of its own, so it must
    /// not be reported, assigned, or counted as a separate aperture.
    UpperHalf,
    /// An I/O space BAR, whose port address is 4-byte aligned.
    Io { address: u32 },
    /// A memory space BAR.  `address` is zero when the header implements the
    /// BAR but nothing has assigned it an address yet, which is a different
    /// fact from "no BAR here".
    Memory {
        address: u64,
        width: BarWidth,
        prefetchable: bool,
    },
    /// The slot declares one of the two reserved BAR encodings, so nothing can
    /// be concluded from its contents.
    Reserved,
}

/// One BAR slot exactly as the header presents it.
///
/// The raw dwords and the decoded meaning are both kept.  The raw value is
/// what a human compares against `lspci` and against the identity table; the
/// decoded value is what the probe decides from.  A disagreement between the
/// two is exactly the kind of thing the report exists to surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Bar {
    /// Slot index, 0..[`BAR_SLOTS`].
    pub(crate) slot: u8,
    /// The low dword of the slot, as read back.
    pub(crate) low: u32,
    /// The high dword, present only for a 64-bit memory BAR and for the upper
    /// half slot that follows it.
    pub(crate) high: Option<u32>,
    pub(crate) kind: BarKind,
}

/// Whether the slot whose low dword is `low` is a 64-bit memory BAR, and so
/// whether the next slot belongs to it rather than being a BAR of its own.
///
/// Bit 0 clear selects memory space and bits 2:1 equal to `0b10` select a
/// 64-bit address.  A slot reading all ones declares nothing here and consumes
/// no successor, which the `0b111` mask handles by construction.
pub(crate) const fn declares_upper_half(low: u32) -> bool {
    low & 0b111 == 0b100
}

impl Bar {
    /// An unimplemented slot, which is also what an array is initialised with.
    pub(crate) const fn absent(slot: u8) -> Self {
        Self {
            slot,
            low: 0,
            high: None,
            kind: BarKind::Unimplemented,
        }
    }

    /// The upper half of the 64-bit memory BAR in slot `slot - 1`.
    pub(crate) const fn upper_half(slot: u8, low: u32) -> Self {
        Self {
            slot,
            low,
            high: Some(low),
            kind: BarKind::UpperHalf,
        }
    }

    /// Decode a slot that is not the upper half of another BAR.
    ///
    /// `high` must be the successor dword exactly when
    /// [`declares_upper_half`] holds for `low`; it is ignored otherwise.
    pub(crate) fn decode(slot: u8, low: u32, high: Option<u32>) -> Self {
        let kind = if low == 0 || low == u32::MAX {
            // A slot that reads back as zero declares no BAR at all, and a
            // device that does not implement a slot reads it back as all ones.
            // Neither may be decoded as an address.
            BarKind::Unimplemented
        } else if low & 1 == 1 {
            BarKind::Io {
                address: low & !0x3,
            }
        } else {
            match (low >> 1) & 0b11 {
                0b00 => BarKind::Memory {
                    address: u64::from(low & !0xf),
                    width: BarWidth::Bits32,
                    prefetchable: low & 0b1000 != 0,
                },
                0b10 => BarKind::Memory {
                    address: (u64::from(high.unwrap_or(0)) << 32) | u64::from(low & !0xf),
                    width: BarWidth::Bits64,
                    prefetchable: low & 0b1000 != 0,
                },
                _ => BarKind::Reserved,
            }
        };
        let high = match kind {
            BarKind::Memory {
                width: BarWidth::Bits64,
                ..
            } => high,
            _ => None,
        };
        Self {
            slot,
            low,
            high,
            kind,
        }
    }

    /// Whether this slot carries a BAR of its own, as opposed to being absent
    /// or being the second half of its predecessor.
    pub(crate) const fn is_own_bar(&self) -> bool {
        !matches!(self.kind, BarKind::UpperHalf | BarKind::Unimplemented)
    }

    /// The address this BAR decodes, when it decodes one.
    pub(crate) const fn address(&self) -> Option<u64> {
        match self.kind {
            BarKind::Io { address } => Some(address as u64),
            BarKind::Memory { address, .. } => Some(address),
            BarKind::Unimplemented | BarKind::UpperHalf | BarKind::Reserved => None,
        }
    }

    /// The width of a memory BAR, when this slot is one.
    pub(crate) const fn width(&self) -> Option<BarWidth> {
        match self.kind {
            BarKind::Memory { width, .. } => Some(width),
            _ => None,
        }
    }

    /// Whether this BAR lives in memory space, which is the only space an
    /// aperture can be mapped from.
    pub(crate) const fn is_memory(&self) -> bool {
        matches!(self.kind, BarKind::Memory { .. })
    }

    /// What a human needs to see for this slot, without the size, which only
    /// the identity table can supply.
    pub(crate) fn describe(&self) -> String {
        use crate::drm::intel::hex;

        match self.kind {
            BarKind::Unimplemented => format!(
                "BAR{} unimplemented (raw {})",
                self.slot,
                hex(self.low as u64, 8)
            ),
            BarKind::UpperHalf => format!(
                "BAR{} upper half of BAR{} (raw {})",
                self.slot,
                self.slot.saturating_sub(1),
                hex(self.low as u64, 8)
            ),
            BarKind::Io { address } => format!(
                "BAR{} i/o port at {} (raw {})",
                self.slot,
                hex(address as u64, 8),
                hex(self.low as u64, 8)
            ),
            BarKind::Memory {
                address,
                width,
                prefetchable,
            } => format!(
                "BAR{} memory {}{} at {} (raw {}{})",
                self.slot,
                width,
                if prefetchable { " prefetchable" } else { "" },
                hex(address, 16),
                hex(self.low as u64, 8),
                match self.high {
                    Some(high) => format!(", high {}", hex(high as u64, 8)),
                    None => String::new(),
                }
            ),
            BarKind::Reserved => format!(
                "BAR{} reserved encoding (raw {})",
                self.slot,
                hex(self.low as u64, 8)
            ),
        }
    }
}

impl fmt::Display for Bar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.describe())
    }
}

/// Everything the probe reads out of one function's configuration header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DeviceInfo {
    pub(crate) bdf: Bdf,
    pub(crate) vendor_id: u16,
    pub(crate) device_id: u16,
    pub(crate) revision: u8,
    pub(crate) prog_if: u8,
    pub(crate) subclass: u8,
    pub(crate) class: u8,
    pub(crate) header_type: u8,
    pub(crate) subsystem_vendor_id: u16,
    pub(crate) subsystem_id: u16,
    pub(crate) command: u16,
    pub(crate) bars: [Bar; BAR_SLOTS],
}

impl DeviceInfo {
    /// Read one function's header, or `None` when no function answers there.
    ///
    /// A read that fails part way through -- a bus the platform cannot reach,
    /// or an address the arithmetic refuses -- is reported as an absent
    /// function.  A probe cannot distinguish "no device" from "no way to ask",
    /// and a half-populated device would be worse than none at all.
    pub(crate) fn read<C: ConfigSpace + ?Sized>(config: &C, bdf: Bdf) -> Option<Self> {
        let vendor_id = config.read_u16(bdf, offset::VENDOR_ID)?;
        if vendor_id == NO_FUNCTION {
            return None;
        }
        let device_id = config.read_u16(bdf, offset::DEVICE_ID)?;
        let command = config.read_u16(bdf, offset::COMMAND)?;
        let revision = config.read_u8(bdf, offset::REVISION_ID)?;
        let prog_if = config.read_u8(bdf, offset::PROG_IF)?;
        let subclass = config.read_u8(bdf, offset::SUBCLASS)?;
        let class = config.read_u8(bdf, offset::CLASS)?;
        let header_type = config.read_u8(bdf, offset::HEADER_TYPE)?;
        let subsystem_vendor_id = config.read_u16(bdf, offset::SUBSYSTEM_VENDOR_ID)?;
        let subsystem_id = config.read_u16(bdf, offset::SUBSYSTEM_ID)?;

        let mut bars = [Bar::absent(0); BAR_SLOTS];
        for (slot, bar) in bars.iter_mut().enumerate() {
            *bar = Bar::absent(slot as u8);
        }
        // Only a type 0 header puts BARs at 0x10.  Reading them out of a
        // bridge's header would decode bus numbers and windows as apertures,
        // so a non-standard header leaves every slot unimplemented and is
        // reported by its header type instead.
        if header_type & 0x7f == HEADER_TYPE_STANDARD {
            let mut slot = 0;
            while slot < BAR_SLOTS {
                let low = config.read_u32(bdf, bar_offset(slot as u8))?;
                if declares_upper_half(low) && slot + 1 < BAR_SLOTS {
                    let high = config.read_u32(bdf, bar_offset(slot as u8 + 1))?;
                    bars[slot] = Bar::decode(slot as u8, low, Some(high));
                    bars[slot + 1] = Bar::upper_half(slot as u8 + 1, high);
                    slot += 2;
                } else {
                    bars[slot] = Bar::decode(slot as u8, low, None);
                    slot += 1;
                }
            }
        }

        Some(Self {
            bdf,
            vendor_id,
            device_id,
            revision,
            prog_if,
            subclass,
            class,
            header_type,
            subsystem_vendor_id,
            subsystem_id,
            command,
            bars,
        })
    }

    /// Whether this function is an Intel display device, the only thing the
    /// probe is looking for.
    pub(crate) const fn is_intel_display(&self) -> bool {
        self.vendor_id == VENDOR_INTEL && self.class == CLASS_DISPLAY
    }

    /// Whether this header is the standard type whose BARs were decoded.
    pub(crate) const fn has_standard_header(&self) -> bool {
        self.header_type & 0x7f == HEADER_TYPE_STANDARD
    }

    /// The memory BAR in `slot`, when the function declares one there.
    pub(crate) fn memory_bar(&self, slot: u8) -> Option<&Bar> {
        self.bars
            .get(usize::from(slot))
            .filter(|bar| bar.is_memory() && bar.slot == slot)
    }

    /// The BARs this function actually declares, in slot order.
    pub(crate) fn declared_bars(&self) -> impl Iterator<Item = &Bar> {
        self.bars.iter().filter(|bar| bar.is_own_bar())
    }

    /// One line of identity, in the order a human writes it down.
    pub(crate) fn describe_identity(&self) -> String {
        format!(
            "{} vendor {:#06x} device {:#06x} revision {:#04x} class {:#04x}:{:#04x}:{:#04x} \
             subsystem {:#06x}:{:#06x} header {:#04x} command {:#06x}",
            self.bdf,
            self.vendor_id,
            self.device_id,
            self.revision,
            self.class,
            self.subclass,
            self.prog_if,
            self.subsystem_vendor_id,
            self.subsystem_id,
            self.header_type,
            self.command,
        )
    }
}

/// The functions a walk found, in the order it found them.
#[derive(Clone, Debug, Default)]
pub(crate) struct BusScan {
    /// Every function that answered, whether or not it is interesting.
    pub(crate) functions: Vec<DeviceInfo>,
}

impl BusScan {
    /// Functions whose vendor is Intel, whatever their class.
    pub(crate) fn intel_functions(&self) -> impl Iterator<Item = &DeviceInfo> {
        self.functions
            .iter()
            .filter(|info| info.vendor_id == VENDOR_INTEL)
    }

    /// Intel display devices: the candidates the probe can act on.
    pub(crate) fn intel_displays(&self) -> impl Iterator<Item = &DeviceInfo> {
        self.functions.iter().filter(|info| info.is_intel_display())
    }

    /// Walk every bus and function the platform configures.
    ///
    /// The walk follows the standard multi-function rule: a device whose
    /// function 0 is absent has no other functions either, so the remaining
    /// seven are not probed.  That bounds a 256-bus ECAM walk to 8192 probes
    /// instead of 65 536, and it is also how the specification says a bus is
    /// built.
    ///
    /// `bus_end` bounds the walk to the buses the platform declares.  An ECAM
    /// window is 256 buses wide whether or not bridges decode them, so walking
    /// the whole window would report functions on buses that do not exist.
    pub(crate) fn walk<C: ConfigSpace + ?Sized>(config: &C, bus_end: u8) -> Self {
        let mut functions = Vec::new();
        for bus in 0..=bus_end {
            for device in 0..32 {
                let Some(function_zero) = DeviceInfo::read(config, Bdf::new(bus, device, 0)) else {
                    continue;
                };
                let multifunction = function_zero.header_type & 0x80 != 0;
                functions.push(function_zero);
                if !multifunction {
                    continue;
                }
                for function in 1..8 {
                    if let Some(info) = DeviceInfo::read(config, Bdf::new(bus, device, function)) {
                        functions.push(info);
                    }
                }
            }
        }
        Self { functions }
    }
}

/// Configuration space reached through the platform's ECAM aperture.
///
/// The platform maps every range it lists in `mmio-ranges` as device memory
/// while building the kernel address space, so the aperture is reached through
/// the direct map at its physical address -- the same route `axdriver`'s bus
/// probe takes.  The range check before each access is not redundant with
/// that: it is what keeps a mistyped bus number from reading address space the
/// platform never declared.
#[cfg(target_os = "none")]
pub(crate) struct Ecam {
    base: usize,
    bus_end: u8,
}

#[cfg(target_os = "none")]
impl Ecam {
    /// The platform's ECAM window, or `None` when the configuration does not
    /// describe one.
    pub(crate) fn platform() -> Option<Self> {
        let base = axconfig::devices::PCI_ECAM_BASE;
        let bus_end = u8::try_from(axconfig::devices::PCI_BUS_END).ok()?;
        if base == 0 {
            return None;
        }
        Some(Self { base, bus_end })
    }

    /// The physical base of the aperture, for a log line.
    pub(crate) const fn base(&self) -> u64 {
        self.base as u64
    }

    /// The highest bus the platform declares.
    pub(crate) const fn bus_end(&self) -> u8 {
        self.bus_end
    }
}

/// Whether `address .. address + width` lies inside a range the platform
/// declared as device memory.
#[cfg(target_os = "none")]
fn declared_mmio(address: usize, width: usize) -> bool {
    let Some(end) = address.checked_add(width) else {
        return false;
    };
    axhal::mem::mmio_ranges().iter().any(|&(start, size)| {
        start
            .checked_add(size)
            .is_some_and(|range_end| address >= start && end <= range_end)
    })
}

#[cfg(target_os = "none")]
impl ConfigSpace for Ecam {
    fn read_u32(&self, bdf: Bdf, offset: u16) -> Option<u32> {
        let address = config_address(self.base, self.bus_end, bdf, offset, 4)?;
        if !declared_mmio(address, 4) {
            return None;
        }
        // SAFETY: `address` was just checked to lie inside a range the
        // platform declared as device memory, and the platform maps every such
        // range into the kernel address space before any driver runs.  A
        // configuration-space read has no side effects and needs no device
        // state, which is why it is the one access this probe makes before it
        // knows what it is talking to.
        let pointer = axhal::mem::phys_to_virt(axhal::mem::PhysAddr::from_usize(address))
            .as_ptr()
            .cast::<u32>();
        Some(unsafe { core::ptr::read_volatile(pointer) })
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;
    use crate::drm::intel::testbus::{FakeBus, Header};

    #[test]
    fn config_address_places_a_function_in_its_ecam_window() {
        let base = 0xe000_0000;
        // Bus 0, device 2, function 0, offset 0: the integrated GPU's usual
        // address, two 32 KiB device blocks into the window.
        assert_eq!(
            config_address(base, 0xff, Bdf::new(0, 2, 0), 0, 4),
            Some(base + (2 << 15))
        );
        // A bus is a 1 MiB step and a function a 4 KiB step inside a device.
        assert_eq!(
            config_address(base, 0xff, Bdf::new(1, 0, 0), 0, 4),
            Some(base + (1 << 20))
        );
        assert_eq!(
            config_address(base, 0xff, Bdf::new(0, 0, 7), 0, 4),
            Some(base + (7 << 12))
        );
        assert_eq!(
            config_address(base, 0xff, Bdf::new(0, 31, 7), 0x2c, 4),
            Some(base + (31 << 15) + (7 << 12) + 0x2c)
        );
    }

    #[test]
    fn config_address_refuses_what_the_window_does_not_contain() {
        let base = 0xe000_0000;
        // A bus the platform does not declare must not be aimed at.
        assert_eq!(config_address(base, 0x3f, Bdf::new(0x40, 0, 0), 0, 4), None);
        // Device and function numbers that cannot be encoded.
        assert_eq!(config_address(base, 0xff, Bdf::new(0, 32, 0), 0, 4), None);
        assert_eq!(config_address(base, 0xff, Bdf::new(0, 0, 8), 0, 4), None);
        // A function's window is 4 KiB and an access may not leave it.
        assert_eq!(
            config_address(base, 0xff, Bdf::new(0, 0, 0), 0xffc, 4),
            Some(base + 0xffc)
        );
        assert_eq!(
            config_address(base, 0xff, Bdf::new(0, 0, 0), 0xffd, 4),
            None
        );
        assert_eq!(
            config_address(base, 0xff, Bdf::new(0, 0, 0), 0x1000, 4),
            None
        );
        // Only aligned 1-, 2- and 4-byte accesses exist.
        assert_eq!(config_address(base, 0xff, Bdf::new(0, 0, 0), 2, 2), None);
        assert_eq!(config_address(base, 0xff, Bdf::new(0, 0, 0), 0, 8), None);
        assert_eq!(config_address(base, 0xff, Bdf::new(0, 0, 0), 0, 0), None);
        // The arithmetic refuses to wrap rather than landing back inside.
        assert_eq!(
            config_address(usize::MAX - 8, 0xff, Bdf::new(1, 0, 0), 0, 4),
            None
        );
    }

    #[test]
    fn bar_decode_reads_the_encodings_the_specification_defines() {
        let bar = Bar::decode(0, 0x8000_0000, None);
        assert_eq!(
            bar.kind,
            BarKind::Memory {
                address: 0x8000_0000,
                width: BarWidth::Bits32,
                prefetchable: false,
            }
        );
        assert!(!declares_upper_half(0x8000_0000));
        // The same BAR, prefetchable: bit 3 is the only difference.
        assert_eq!(
            Bar::decode(0, 0x8000_0008, None).kind,
            BarKind::Memory {
                address: 0x8000_0000,
                width: BarWidth::Bits32,
                prefetchable: true,
            }
        );
        // A 64-bit memory BAR declares its width in bits 2:1, and its address
        // is the pair of dwords with the low four bits stripped.
        let low = 0x0000_0004;
        assert!(declares_upper_half(low));
        let bar = Bar::decode(0, low, Some(0x0000_0060));
        assert_eq!(
            bar.kind,
            BarKind::Memory {
                address: 0x0000_0060_0000_0000,
                width: BarWidth::Bits64,
                prefetchable: false,
            }
        );
        assert_eq!(bar.high, Some(0x0000_0060));
        assert_eq!(bar.width(), Some(BarWidth::Bits64));
        // An I/O BAR keeps its port address and drops the flag bits.
        assert_eq!(
            Bar::decode(4, 0x0000_c001, None).kind,
            BarKind::Io { address: 0xc000 }
        );
        assert!(!Bar::decode(4, 0x0000_c001, None).is_memory());
        // The reserved encodings are reported rather than guessed at.
        assert_eq!(Bar::decode(1, 0x0000_0006, None).kind, BarKind::Reserved);
        assert_eq!(Bar::decode(1, 0x0000_000e, None).kind, BarKind::Reserved);
    }

    #[test]
    fn bar_decode_distinguishes_unimplemented_from_unassigned() {
        // A slot that reads back as zero declares no BAR, and consumes no
        // successor: a following 64-bit BAR is still a BAR of its own.
        let bar = Bar::decode(0, 0x0000_0000, None);
        assert_eq!(bar.kind, BarKind::Unimplemented);
        assert!(!bar.is_own_bar());
        assert!(!declares_upper_half(0x0000_0000));
        // All ones is what a device that does not implement a slot reads back.
        // It must not become an I/O BAR at 0xffff_fffc.
        assert_eq!(Bar::decode(5, u32::MAX, None).kind, BarKind::Unimplemented);
        assert_eq!(Bar::decode(5, u32::MAX, None).address(), None);
        // An implemented but unassigned BAR declares its width and reports
        // address zero, which is a different fact from "no BAR here".
        let bar = Bar::decode(2, 0x0000_0004, Some(0));
        assert_eq!(
            bar.kind,
            BarKind::Memory {
                address: 0,
                width: BarWidth::Bits64,
                prefetchable: false,
            }
        );
        assert!(bar.is_own_bar());
        assert_eq!(bar.address(), Some(0));
    }

    #[test]
    fn bar_description_names_type_width_and_address() {
        assert_eq!(
            Bar::decode(0, 0x0000_0004, Some(0x6000)).describe(),
            "BAR0 memory 64-bit at 0x0000_6000_0000_0000 (raw 0x0000_0004, high 0x0000_6000)"
        );
        assert_eq!(
            Bar::decode(2, 0x9000_0008, None).describe(),
            "BAR2 memory 32-bit prefetchable at 0x0000_0000_9000_0000 (raw 0x9000_0008)"
        );
        assert_eq!(
            Bar::decode(4, 0x0000_c001, None).describe(),
            "BAR4 i/o port at 0x0000_c000 (raw 0x0000_c001)"
        );
        assert_eq!(
            Bar::decode(5, 0x0000_0000, None).describe(),
            "BAR5 unimplemented (raw 0x0000_0000)"
        );
        assert_eq!(
            Bar::upper_half(1, 0x0000_0060).describe(),
            "BAR1 upper half of BAR0 (raw 0x0000_0060)"
        );
    }

    #[test]
    fn a_64bit_bar_consumes_its_successor_slot() {
        // BAR0 is a 64-bit memory BAR, BAR2 a 32-bit one, and the rest read
        // back as zero.
        let bus = FakeBus::new(vec![
            Header::new(Bdf::new(0, 2, 0), VENDOR_INTEL, 0x46d0)
                .class(CLASS_DISPLAY, SUBCLASS_VGA)
                .bars([0x0000_0004, 0x0000_0060, 0x9000_0008, 0, 0, 0]),
        ]);
        let info = DeviceInfo::read(&bus, Bdf::new(0, 2, 0)).unwrap();
        assert_eq!(
            info.bars[0].kind,
            BarKind::Memory {
                address: 0x0000_0060_0000_0000,
                width: BarWidth::Bits64,
                prefetchable: false,
            }
        );
        assert_eq!(info.bars[1].kind, BarKind::UpperHalf);
        assert!(!info.bars[1].is_own_bar());
        assert_eq!(info.bars[2].address(), Some(0x9000_0000));
        assert_eq!(info.bars[3].kind, BarKind::Unimplemented);
        // Three slots carry something of their own: BAR0, BAR2 and the
        // reserved-free remainder is empty, so two BARs plus nothing.
        let declared: Vec<u8> = info.declared_bars().map(|bar| bar.slot).collect();
        assert_eq!(declared, vec![0, 2]);
        assert!(info.is_intel_display());
        assert!(info.has_standard_header());
        assert_eq!(
            info.memory_bar(0).unwrap().address(),
            Some(0x0000_0060_0000_0000)
        );
        assert!(info.memory_bar(1).is_none(), "an upper half is not a BAR");
    }

    #[test]
    fn a_bridge_header_has_no_apertures_to_decode() {
        // Header type 1 puts bus numbers and windows at the BAR offsets; the
        // probe must refuse to read them as apertures.
        let mut header = Header::new(Bdf::new(0, 1, 0), VENDOR_INTEL, 0x1234)
            .class(0x06, 0x04)
            .bars([0x0010_1001, 0x0000_3030, 0x0000_2020, 0x0000_4040, 0, 0]);
        header.header_type = 0x01;
        let bus = FakeBus::new(vec![header]);
        let info = DeviceInfo::read(&bus, Bdf::new(0, 1, 0)).unwrap();
        assert!(!info.has_standard_header());
        assert_eq!(info.declared_bars().count(), 0);
        assert!(!info.is_intel_display());
    }

    #[test]
    fn an_absent_function_reads_as_nothing_at_all() {
        let bus = FakeBus::new(vec![]);
        assert!(DeviceInfo::read(&bus, Bdf::new(0, 2, 0)).is_none());
    }

    #[test]
    fn the_walk_follows_the_multifunction_rule_and_stops_at_bus_end() {
        let bus = FakeBus::new(vec![
            // Device 0 has two functions and declares itself multi-function.
            Header::new(Bdf::new(0, 0, 0), VENDOR_INTEL, 0x29c0)
                .class(0x06, 0x00)
                .multifunction(),
            Header::new(Bdf::new(0, 0, 1), VENDOR_INTEL, 0x29c1)
                .class(0x06, 0x00)
                .multifunction(),
            // Device 1 has no function 0, so its function 3 is never probed.
            Header::new(Bdf::new(0, 1, 3), VENDOR_INTEL, 0x1234)
                .class(0x02, 0x00)
                .multifunction(),
            // A function on a bus beyond the declared end is out of reach.
            Header::new(Bdf::new(2, 0, 0), VENDOR_INTEL, 0x46d0).class(CLASS_DISPLAY, SUBCLASS_VGA),
        ]);
        let scan = BusScan::walk(&bus, 1);
        let seen: Vec<Bdf> = scan.functions.iter().map(|info| info.bdf).collect();
        assert_eq!(seen, vec![Bdf::new(0, 0, 0), Bdf::new(0, 0, 1)]);
        // Only the last bus-declared device counted; the GPU on bus 2 is
        // outside the platform's declared bus range.
        assert_eq!(scan.intel_functions().count(), 2);
        assert_eq!(scan.intel_displays().count(), 0);
    }
}
