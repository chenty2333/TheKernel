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
//! Configuration space uses the same runtime MCFG decision as the generic PCI
//! bus. Its discovered span is mapped as device memory; the configured MMIO
//! list is not an allowlist for firmware-discovered addresses.

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
    /// PCI Status, whose Capabilities List bit gates the linked list at `0x34`.
    pub(crate) const STATUS: u16 = 0x06;
    pub(crate) const REVISION_ID: u16 = 0x08;
    pub(crate) const PROG_IF: u16 = 0x09;
    pub(crate) const SUBCLASS: u16 = 0x0a;
    pub(crate) const CLASS: u16 = 0x0b;
    pub(crate) const HEADER_TYPE: u16 = 0x0e;
    pub(crate) const SUBSYSTEM_VENDOR_ID: u16 = 0x2c;
    pub(crate) const SUBSYSTEM_ID: u16 = 0x2e;
    /// First BAR slot; each slot is one dword.
    pub(crate) const BAR0: u16 = 0x10;
    /// `Interrupt Line`: the legacy IRQ the firmware routed this function to,
    /// or a value that says it routed none.
    pub(crate) const INTERRUPT_LINE: u16 = 0x3c;
    /// `Interrupt Pin`: which `INTx#` this function can assert.  Zero means it
    /// asserts none, which on a PCI Express function means it can only be
    /// reached by MSI.
    pub(crate) const INTERRUPT_PIN: u16 = 0x3d;
    /// `SNB_GMCH_CTRL`, which is not a standard header field: Intel reuses this
    /// dword of the function's own header for the graphics memory size, and on
    /// Gen8 and later bits `[7:6]` are the `GGMS` field that says how much page
    /// table the GGTT has.
    ///
    /// `[I915]` `include/drm/intel/i915_drm.h:49` (`#define SNB_GMCH_CTRL 0x50`),
    /// read by `gt/intel_ggtt.c:1228-1232` on every Gen8 and later part, so the
    /// target's Gen12 display function is one of them.
    pub(crate) const GMCH_CTL: u16 = 0x50;
}

/// `BDW_GMCH_GGMS_SHIFT`: where the `GGMS` field starts on Gen8 and later.
///
/// `[I915]` `include/drm/intel/i915_drm.h:54`.  The older `SNB` encoding puts
/// the same field at bit 8 (`:50-51`), which is why the shift is a constant
/// here rather than a literal at the read: `gen8_get_total_gtt_size()` is the
/// function this generation's probe calls (`gt/intel_ggtt.c:1230-1232` for
/// `GRAPHICS_VER >= 8`).
pub(crate) const GMCH_GGMS_SHIFT: u16 = 6;

/// `BDW_GMCH_GGMS_MASK`: the width of the `GGMS` field.
///
/// `[I915]` `include/drm/intel/i915_drm.h:55`.  Two bits is why the field names
/// four values and no more.
pub(crate) const GMCH_GGMS_MASK: u16 = 0x3;

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

/// The explicit write half used only after the native display path has taken
/// ownership. PCI probes remain read-only; implementations write just the
/// bounded 16/32-bit fields the MSI transaction names.
pub(crate) trait ConfigWriteSpace: ConfigSpace {
    fn write_u16(&mut self, bdf: Bdf, offset: u16, value: u16) -> Result<(), String>;
    fn write_u32(&mut self, bdf: Bdf, offset: u16, value: u32) -> Result<(), String>;
}

/// Standard MSI transaction shared by target ECAM and the host capability
/// model. MSI-X and an already-enabled MSI remain unowned and are refused.
pub(super) trait N305DisplayMsi: ConfigWriteSpace {
    fn prepare_n305_display_msi(
        &mut self,
        bdf: Bdf,
        message_address: u64,
        message_data: u32,
    ) -> Result<DisplayMsiBefore, DisplayMsiPrepareError> {
        self.prepare_n305_display_msi_inner(bdf, message_address, message_data)
            .map_err(|message| DisplayMsiPrepareError {
                rollback_verified: !message.contains("PCI MSI rollback unverified"),
                message,
            })
    }

    /// Program one MSI message for the already-owned N305 display function,
    /// keeping MSI disabled until the caller has installed its source masks.
    /// MSI-X and a firmware-enabled MSI are unowned and therefore refused.
    fn prepare_n305_display_msi_inner(
        &mut self,
        bdf: Bdf,
        message_address: u64,
        message_data: u32,
    ) -> Result<DisplayMsiBefore, String> {
        const CAP_MSI: u8 = 0x05;
        const CAP_MSIX: u8 = 0x11;
        const MSI_ENABLE: u16 = 1;
        const MSI_MME_MASK: u16 = 0x70;
        const MSI_64BIT: u16 = 1 << 7;
        const MSI_PVM: u16 = 1 << 8;
        const MSIX_ENABLE: u16 = 1 << 15;
        const PCI_COMMAND_INTX_DISABLE: u16 = 1 << 10;

        let info = DeviceInfo::read(self, bdf)
            .ok_or_else(|| String::from("display MSI: N305 PCI header is unreadable"))?;
        if (info.vendor_id, info.device_id, info.revision) != (0x8086, 0x46d0, 0)
            || info.header_type & 0x7f != HEADER_TYPE_STANDARD
        {
            return Err(String::from(
                "display MSI: only the admitted N305 type-0 PCI function is supported",
            ));
        }
        let command = info.command;
        if command & 2 == 0 {
            return Err(String::from(
                "display MSI: PCI memory decode is not enabled by the firmware owner",
            ));
        }
        if self
            .read_u16(bdf, offset::STATUS)
            .is_none_or(|status| status & (1 << 4) == 0)
        {
            return Err(String::from(
                "display MSI: PCI capability list is absent or unreadable",
            ));
        }

        let mut current = self
            .read_u8(bdf, 0x34)
            .ok_or_else(|| String::from("display MSI: capability head is unreadable"))?;
        let mut seen = [0u8; 48];
        let mut count = 0usize;
        let mut msi = None;
        let mut msix_enabled = false;
        while current != 0 {
            if !(0x40..=0xfc).contains(&current) || current & 3 != 0 || count == seen.len() {
                return Err(String::from(
                    "display MSI: malformed or overlong PCI capability chain",
                ));
            }
            if seen[..count].contains(&current) {
                return Err(String::from("display MSI: PCI capability chain loops"));
            }
            seen[count] = current;
            count += 1;
            let header = self
                .read_u32(bdf, u16::from(current))
                .ok_or_else(|| String::from("display MSI: capability header is unreadable"))?;
            let id = header as u8;
            let next = (header >> 8) as u8;
            let capability = u16::from(current);
            let control = (header >> 16) as u16;
            match id {
                CAP_MSI if msi.is_none() => msi = Some((capability, control)),
                CAP_MSI => {
                    return Err(String::from(
                        "display MSI: duplicate MSI capabilities are refused",
                    ));
                }
                CAP_MSIX => msix_enabled |= control & MSIX_ENABLE != 0,
                _ => {}
            }
            current = next;
        }
        if msix_enabled {
            return Err(String::from(
                "display MSI: firmware-enabled MSI-X is unowned; refusing to change it",
            ));
        }
        let (capability, control) =
            msi.ok_or_else(|| String::from("display MSI: PCI MSI capability is absent"))?;
        if control & MSI_ENABLE != 0 {
            return Err(String::from(
                "display MSI: firmware MSI is already enabled and unowned",
            ));
        }

        let is_64bit = control & MSI_64BIT != 0;
        let has_pvm = control & MSI_PVM != 0;
        let address_low_offset = capability + 4;
        let (address_high_offset, data_offset) = if is_64bit {
            (Some(capability + 8), capability + 12)
        } else {
            (None, capability + 8)
        };
        let mask_offset = has_pvm.then_some(data_offset + 4);
        let capability_end = capability
            + if has_pvm {
                if is_64bit { 24 } else { 20 }
            } else if is_64bit {
                14
            } else {
                10
            };
        if capability_end > 0x100 {
            return Err(String::from(
                "display MSI: standard MSI layout extends past conventional PCI config space",
            ));
        }
        if seen[..count].iter().any(|other| {
            let other = u16::from(*other);
            other != capability && capability < other && other < capability_end
        }) {
            return Err(String::from(
                "display MSI: another capability header overlaps the writable MSI layout",
            ));
        }
        let before = DisplayMsiBefore {
            bdf,
            capability,
            control,
            command,
            address_low: self
                .read_u32(bdf, address_low_offset)
                .ok_or_else(|| String::from("display MSI: message address is unreadable"))?,
            address_high: address_high_offset
                .map(|offset| {
                    self.read_u32(bdf, offset)
                        .ok_or_else(|| String::from("display MSI: 64-bit address is unreadable"))
                })
                .transpose()?,
            data: self
                .read_u16(bdf, data_offset)
                .ok_or_else(|| String::from("display MSI: message data is unreadable"))?,
            vector_mask: mask_offset
                .map(|offset| {
                    self.read_u32(bdf, offset)
                        .ok_or_else(|| String::from("display MSI: per-vector mask is unreadable"))
                })
                .transpose()?,
        };
        if message_address > u64::from(u32::MAX) && !is_64bit {
            return Err(String::from(
                "display MSI: the allocated message address exceeds 32-bit MSI capability",
            ));
        }
        if message_data > u32::from(u16::MAX) {
            return Err(String::from(
                "display MSI: allocated message data exceeds the standard 16-bit field",
            ));
        }

        // Hold the message masked and MSI disabled until the display's source
        // masks, IIR before-image and global dispatch have been proved.
        let disabled_control = control & !(MSI_ENABLE | MSI_MME_MASK);
        let prepared = (|| {
            self.write_u16(bdf, capability + 2, disabled_control)?;
            self.write_u16(bdf, offset::COMMAND, command | PCI_COMMAND_INTX_DISABLE)?;
            self.write_u32(bdf, address_low_offset, message_address as u32)?;
            if let (Some(offset), Some(high)) = (address_high_offset, before.address_high) {
                self.write_u32(bdf, offset, (message_address >> 32) as u32)?;
                if self.read_u32(bdf, offset) != Some((message_address >> 32) as u32) {
                    return Err(String::from("display MSI: 64-bit address readback differs"));
                }
                let _ = high;
            }
            self.write_u16(bdf, data_offset, message_data as u16)?;
            if self.read_u16(bdf, capability + 2) != Some(disabled_control)
                || self.read_u16(bdf, offset::COMMAND) != Some(command | PCI_COMMAND_INTX_DISABLE)
                || self.read_u32(bdf, address_low_offset) != Some(message_address as u32)
                || self.read_u16(bdf, data_offset) != Some(message_data as u16)
            {
                return Err(String::from(
                    "display MSI: disabled message readback differs",
                ));
            }
            if let (Some(offset), Some(mask)) = (mask_offset, before.vector_mask) {
                let masked = mask | 1;
                self.write_u32(bdf, offset, masked)?;
                if self.read_u32(bdf, offset) != Some(masked) {
                    return Err(String::from(
                        "display MSI: cannot hold the single vector masked during setup",
                    ));
                }
            }
            Ok(())
        })();
        if let Err(error) = prepared {
            let restored = self.restore_n305_display_msi(&before);
            return Err(match restored {
                Ok(()) => format!("{error}; PCI MSI before-image restored"),
                Err(rollback) => format!("{error}; PCI MSI rollback unverified: {rollback}"),
            });
        }
        Ok(before)
    }

    /// Enable one previously prepared MSI message. If the capability has a
    /// per-vector mask, it remains masked until [`Self::unmask_n305_display_msi`].
    fn activate_n305_display_msi(&mut self, before: &DisplayMsiBefore) -> Result<(), String> {
        const MSI_ENABLE: u16 = 1;
        const MSI_MME_MASK: u16 = 0x70;
        const PCI_COMMAND_INTX_DISABLE: u16 = 1 << 10;

        let control = self
            .read_u16(before.bdf, before.capability + 2)
            .ok_or_else(|| String::from("display MSI: control is unreadable at activation"))?;
        let enabled = (control & !MSI_MME_MASK) | MSI_ENABLE;
        self.write_u16(before.bdf, before.capability + 2, enabled)?;
        if self.read_u16(before.bdf, before.capability + 2) != Some(enabled)
            || self.read_u16(before.bdf, offset::COMMAND)
                != Some(before.command | PCI_COMMAND_INTX_DISABLE)
        {
            return Err(String::from(
                "display MSI enable/INTx-disable readback differs",
            ));
        }
        Ok(())
    }

    /// Release the prepared single message after the display master is live.
    fn unmask_n305_display_msi(&mut self, before: &DisplayMsiBefore) -> Result<(), String> {
        if before.control & (1 << 8) == 0 {
            return Ok(());
        }
        let is_64bit = before.control & (1 << 7) != 0;
        let data_offset = before.capability + if is_64bit { 12 } else { 8 };
        let mask_offset = data_offset + 4;
        let current = self
            .read_u32(before.bdf, mask_offset)
            .ok_or_else(|| String::from("display MSI: per-vector mask is unreadable"))?;
        let updated = current & !1;
        self.write_u32(before.bdf, mask_offset, updated)?;
        if self.read_u32(before.bdf, mask_offset) != Some(updated) {
            return Err(String::from(
                "display MSI per-vector unmask readback differs",
            ));
        }
        Ok(())
    }

    /// Restore the exact previously disabled MSI capability and PCI command.
    /// This is called only with the Intel display master disabled.
    fn restore_n305_display_msi(&mut self, before: &DisplayMsiBefore) -> Result<(), String> {
        const MSI_ENABLE: u16 = 1;
        const MSI_64BIT: u16 = 1 << 7;
        const MSI_PVM: u16 = 1 << 8;
        let disabled = before.control & !MSI_ENABLE;
        self.write_u16(before.bdf, before.capability + 2, disabled)?;
        self.write_u32(before.bdf, before.capability + 4, before.address_low)?;
        let data_offset = if before.control & MSI_64BIT != 0 {
            if let Some(high) = before.address_high {
                self.write_u32(before.bdf, before.capability + 8, high)?;
            }
            before.capability + 12
        } else {
            before.capability + 8
        };
        if let Some(mask) = before.vector_mask {
            self.write_u32(before.bdf, data_offset + 4, mask)?;
        }
        self.write_u16(before.bdf, data_offset, before.data)?;
        self.write_u16(before.bdf, before.capability + 2, before.control)?;
        self.write_u16(before.bdf, offset::COMMAND, before.command)?;
        if self.read_u16(before.bdf, before.capability + 2) != Some(before.control)
            || self.read_u16(before.bdf, offset::COMMAND) != Some(before.command)
            || self.read_u32(before.bdf, before.capability + 4) != Some(before.address_low)
            || self.read_u16(before.bdf, data_offset) != Some(before.data)
        {
            return Err(String::from(
                "PCI MSI capability before-image readback differs",
            ));
        }
        if let Some(high) = before.address_high
            && self.read_u32(before.bdf, before.capability + 8) != Some(high)
        {
            return Err(String::from("PCI MSI high address before-image differs"));
        }
        if let Some(mask) = before.vector_mask
            && (before.control & MSI_PVM != 0)
            && self.read_u32(before.bdf, data_offset + 4) != Some(mask)
        {
            return Err(String::from("PCI MSI vector mask before-image differs"));
        }
        Ok(())
    }
}

impl<T: ConfigWriteSpace> N305DisplayMsi for T {}

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
    /// `Interrupt Line`, `0x3c`.  Read-only here, like every other byte of the
    /// header: assigning this line is a write, and this probe does not write.
    pub(crate) interrupt_line: u8,
    /// `Interrupt Pin`, `0x3d`.
    pub(crate) interrupt_pin: u8,
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
        // The last two bytes of the type 0 header's first half.  They are read
        // because they are the answer to the first question a reader asks about
        // this device's interrupt: whether any legacy INTx route exists at all.
        // On this part the display function's interrupt is an MSI and this
        // kernel has no MSI support, so "is there an INTx to fall back on" is
        // not an academic question -- and it is these two bytes, not an
        // inference from the device id.
        let interrupt_line = config.read_u8(bdf, offset::INTERRUPT_LINE)?;
        let interrupt_pin = config.read_u8(bdf, offset::INTERRUPT_PIN)?;

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
            interrupt_line,
            interrupt_pin,
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
             subsystem {:#06x}:{:#06x} header {:#04x} command {:#06x} interrupt pin {:#04x} line \
             {:#04x}",
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
            self.interrupt_pin,
            self.interrupt_line,
        )
    }

    /// What the two interrupt bytes say, in words, for a report line.
    ///
    /// Both are read-only observations of what the firmware left in the header.
    /// `Interrupt Pin` is which `INTx#` the function can assert -- zero means
    /// it asserts none, so only MSI can reach it -- and `Interrupt Line` is the
    /// legacy IRQ the firmware routed that pin to, where zero and `0xff` are
    /// the two values that conventionally mean "none".  A reader who is asking
    /// whether the MSI gap can be worked around needs exactly these bytes, and
    /// a raw pair of hex numbers does not answer the question by itself.
    pub(crate) fn describe_interrupt(&self) -> String {
        let pin = match self.interrupt_pin {
            0 => String::from("no INTx pin, so only MSI can reach this function"),
            1 => String::from("INTA#"),
            2 => String::from("INTB#"),
            3 => String::from("INTC#"),
            4 => String::from("INTD#"),
            other => format!("{other:#04x}, which is not one of the four INTx pins"),
        };
        let line = match self.interrupt_line {
            0x00 => String::from("no legacy IRQ is programmed in the header"),
            0xff => String::from("0xff, which is what firmware leaves when it assigned none"),
            value => format!("IRQ {value}"),
        };
        format!(
            "interrupt pin {:#04x} ({pin}), interrupt line {:#04x} ({line})",
            self.interrupt_pin, self.interrupt_line,
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

/// A read-only view of the runtime segment-zero ECAM mapping.
#[cfg(target_os = "none")]
pub(crate) struct Ecam {
    physical: usize,
    mapped: usize,
    bus_end: u8,
}

/// Before-image for the N305 display function's standard MSI capability.
///
/// The IRQ owner keeps this only while it installs its single message; when
/// setup refuses after a partial config write, every changed field can be
/// restored with MSI disabled first. MSI-X is not enabled or modified here.
#[derive(Clone, Copy, Debug)]
pub(super) struct DisplayMsiBefore {
    bdf: Bdf,
    capability: u16,
    control: u16,
    command: u16,
    address_low: u32,
    address_high: Option<u32>,
    data: u16,
    vector_mask: Option<u32>,
}

#[derive(Clone, Debug)]
pub(super) struct DisplayMsiPrepareError {
    pub(super) message: String,
    /// True when no write was attempted or the partial MSI before-image was
    /// fully restored. False retains the vector/window for delayed-message
    /// safety even though the root master is still disabled.
    pub(super) rollback_verified: bool,
}

#[cfg(target_os = "none")]
impl Ecam {
    pub(crate) fn platform() -> Option<Self> {
        let physical = axhal::pci::ecam_base();
        let (bus_begin, bus_end) = axhal::pci::ecam_bus_range();
        let size = ecam_span(physical, bus_begin, bus_end)?;
        if axhal::pci::ecam_segment() != 0 {
            return None;
        }
        match axmm::iomap(axhal::mem::PhysAddr::from_usize(physical), size) {
            Ok(mapped) => Some(Self {
                physical,
                mapped: mapped.as_usize(),
                bus_end,
            }),
            Err(error) => {
                axlog::warn!(
                    "intel-gpu: ECAM mapping failed at {physical:#x}: {error:?}; firmware console \
                     retained"
                );
                None
            }
        }
    }

    /// Sole fresh-GT owner only, after DMA/VM admission. PCI COMMAND is a
    /// 16-bit write: never echo adjacent STATUS write-one-to-clear bits.
    pub(super) fn enable_n305_bus_master(&self, bdf: Bdf) -> Option<bool> {
        let info = DeviceInfo::read(self, bdf)?;
        if (info.vendor_id, info.device_id, info.revision) != (0x8086, 0x46d0, 0)
            || info.command & 2 == 0
        {
            return None;
        }
        if info.command & 4 != 0 {
            return Some(false);
        }
        let address = config_address(self.mapped, self.bus_end, bdf, offset::COMMAND, 2)?;
        // SAFETY: checked exact-N305 COMMAND word in this live ECAM mapping.
        // Caller holds exclusive opt-in GT ownership; no other PCI bit changes.
        unsafe { core::ptr::write_volatile(address as *mut u16, info.command | 4) };
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
        (self.read_u16(bdf, offset::COMMAND)? == info.command | 4).then_some(true)
    }

    pub(crate) const fn base(&self) -> u64 {
        self.physical as u64
    }
    pub(crate) const fn bus_end(&self) -> u8 {
        self.bus_end
    }
}

/// Only segment-zero windows beginning at bus zero are currently supported.
fn ecam_span(base: usize, bus_begin: u8, bus_end: u8) -> Option<usize> {
    if base == 0 || base & 0xfffff != 0 || bus_begin != 0 {
        return None;
    }
    let size = (usize::from(bus_end) + 1) << 20;
    base.checked_add(size)?;
    Some(size)
}

#[cfg(target_os = "none")]
impl ConfigSpace for Ecam {
    fn read_u32(&self, bdf: Bdf, offset: u16) -> Option<u32> {
        let address = config_address(self.mapped, self.bus_end, bdf, offset, 4)?;
        // SAFETY: platform() mapped this exact runtime window; config_address
        // admits only aligned dwords inside its buses and function windows.
        Some(unsafe { core::ptr::read_volatile(address as *const u32) })
    }
}

#[cfg(target_os = "none")]
impl ConfigWriteSpace for Ecam {
    fn write_u16(&mut self, bdf: Bdf, offset: u16, value: u16) -> Result<(), String> {
        let address = config_address(self.mapped, self.bus_end, bdf, offset, 2)
            .ok_or_else(|| String::from("PCI MSI 16-bit write is out of ECAM bounds"))?;
        // SAFETY: platform() mapped this exact ECAM range and the aligned
        // offset was bounded above. Only the named MSI/COMMAND word changes.
        unsafe { core::ptr::write_volatile(address as *mut u16, value) };
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    fn write_u32(&mut self, bdf: Bdf, offset: u16, value: u32) -> Result<(), String> {
        let address = config_address(self.mapped, self.bus_end, bdf, offset, 4)
            .ok_or_else(|| String::from("PCI MSI 32-bit write is out of ECAM bounds"))?;
        // SAFETY: platform() mapped this exact ECAM range and the aligned
        // offset was bounded above. Only an MSI message field/mask changes.
        unsafe { core::ptr::write_volatile(address as *mut u32, value) };
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloc::{collections::BTreeMap, string::String, vec};
    use core::cell::Cell;

    use super::*;
    use crate::drm::intel::testbus::{FakeBus, Header};

    struct MsiModel {
        bdf: Bdf,
        words: BTreeMap<(Bdf, u16), u32>,
        fail_once: Option<u16>,
        write_count: usize,
        fail_before_write: Option<usize>,
        fail_after_write: Option<usize>,
        corrupt_read_once: Cell<Option<(u16, usize)>>,
    }

    impl MsiModel {
        fn n305(control: u16, next: u8) -> Self {
            let bdf = Bdf::new(0, 2, 0);
            let mut model = Self {
                bdf,
                words: BTreeMap::new(),
                fail_once: None,
                write_count: 0,
                fail_before_write: None,
                fail_after_write: None,
                corrupt_read_once: Cell::new(None),
            };
            for (offset, value) in [
                (0x00, 0x46d0_8086),
                (0x04, (0x0010u32 << 16) | 0x0006),
                (0x08, 0x0300_0000),
                (0x0c, 0),
                (0x2c, 0),
                (0x3c, 0x0000_010b),
                (0x34, 0x40),
                (
                    0x40,
                    u32::from(control) << 16 | (u32::from(next) << 8) | 0x05,
                ),
                (0x44, 0xaabb_ccdd),
                (0x48, 0x1122_3344),
                (0x4c, 0x0000_0067),
                (0x50, 0xa5a5_a5a4),
                (0x54, 0),
            ] {
                model.words.insert((bdf, offset), value);
            }
            for slot in 0..BAR_SLOTS {
                model.words.insert((bdf, bar_offset(slot as u8)), 0);
            }
            model
        }

        fn get(&self, offset: u16) -> u32 {
            self.words.get(&(self.bdf, offset)).copied().unwrap_or(0)
        }
        fn fail_once_at(&mut self, offset: u16) {
            self.fail_once = Some(offset);
        }
        fn insert(&mut self, offset: u16, value: u32) {
            self.words.insert((self.bdf, offset), value);
        }

        fn inject_write(&mut self, operation: usize, landed: bool) {
            if landed {
                self.fail_after_write = Some(operation);
            } else {
                self.fail_before_write = Some(operation);
            }
        }

        fn begin_write(&mut self) -> Result<bool, String> {
            self.write_count += 1;
            if self.fail_before_write == Some(self.write_count) {
                self.fail_before_write = None;
                return Err(String::from("injected pre-write failure"));
            }
            Ok(self.fail_after_write == Some(self.write_count))
        }

        fn finish_write(&mut self, landed_failure: bool) -> Result<(), String> {
            if landed_failure {
                self.fail_after_write = None;
                Err(String::from("injected landed-write failure"))
            } else {
                Ok(())
            }
        }
    }

    impl ConfigSpace for MsiModel {
        fn read_u32(&self, bdf: Bdf, offset: u16) -> Option<u32> {
            if bdf != self.bdf || offset >= CONFIG_WINDOW as u16 || !offset.is_multiple_of(4) {
                return None;
            }
            if self
                .corrupt_read_once
                .get()
                .is_some_and(|(register, after_write)| {
                    register == offset && self.write_count >= after_write
                })
            {
                self.corrupt_read_once.set(None);
                Some(self.get(offset) ^ 1)
            } else {
                Some(self.get(offset))
            }
        }
    }

    impl ConfigWriteSpace for MsiModel {
        fn write_u16(&mut self, bdf: Bdf, offset: u16, value: u16) -> Result<(), String> {
            if bdf != self.bdf || offset >= CONFIG_WINDOW as u16 || !offset.is_multiple_of(2) {
                return Err(String::from("invalid fake 16-bit config write"));
            }
            if self.fail_once == Some(offset) {
                self.fail_once = None;
                return Err(String::from("injected fake 16-bit config write failure"));
            }
            let landed_failure = self.begin_write()?;
            let aligned = offset & !3;
            let old = self.get(aligned);
            let shift = (offset & 2) * 8;
            let mask = u32::from(u16::MAX) << shift;
            self.insert(aligned, (old & !mask) | (u32::from(value) << shift));
            self.finish_write(landed_failure)
        }

        fn write_u32(&mut self, bdf: Bdf, offset: u16, value: u32) -> Result<(), String> {
            if bdf != self.bdf || offset >= CONFIG_WINDOW as u16 || !offset.is_multiple_of(4) {
                return Err(String::from("invalid fake 32-bit config write"));
            }
            if self.fail_once == Some(offset) {
                self.fail_once = None;
                return Err(String::from("injected fake 32-bit config write failure"));
            }
            let landed_failure = self.begin_write()?;
            self.insert(offset, value);
            self.finish_write(landed_failure)
        }
    }

    fn msi_control(model: &MsiModel) -> u16 {
        model.read_u16(model.bdf, 0x42).unwrap()
    }

    #[test]
    fn n305_msi_64bit_pvm_message_activation_and_exact_rollback() {
        let mut model = MsiModel::n305((1 << 7) | (1 << 8), 0);
        model.insert(0x48, 0x1357_2468);
        model.insert(0x4c, 0x0000_0067);
        model.insert(0x50, 0xa5a5_a5a4);
        let original = model.words.clone();
        let before = model
            .prepare_n305_display_msi(model.bdf, 0x1234_5678_9abc_def0, 0x45)
            .unwrap();
        assert_eq!(model.read_u32(model.bdf, 0x44), Some(0x9abc_def0));
        assert_eq!(model.read_u32(model.bdf, 0x48), Some(0x1234_5678));
        assert_eq!(model.read_u16(model.bdf, 0x4c), Some(0x45));
        assert_eq!(model.read_u32(model.bdf, 0x50), Some(0xa5a5_a5a5));
        assert_eq!(model.read_u16(model.bdf, offset::COMMAND), Some(0x0406));
        assert_eq!(model.read_u16(model.bdf, offset::STATUS), Some(0x0010));
        assert_eq!(
            msi_control(&model) & 1,
            0,
            "MSI stays disabled until activate"
        );

        model.activate_n305_display_msi(&before).unwrap();
        assert_eq!(msi_control(&model), (1 << 8) | (1 << 7) | 1);
        assert_eq!(model.read_u32(model.bdf, 0x50), Some(0xa5a5_a5a5));
        model.unmask_n305_display_msi(&before).unwrap();
        assert_eq!(model.read_u32(model.bdf, 0x50), Some(0xa5a5_a5a4));
        model.restore_n305_display_msi(&before).unwrap();
        assert_eq!(model.words, original);
    }

    #[test]
    fn n305_msi_32bit_data_offset_and_multiple_message_enable_are_bounded() {
        let mut model = MsiModel::n305(0b110, 0); // MMC says four, MME must remain one vector.
        model.insert(0x48, 0x0000_0067);
        let before = model
            .prepare_n305_display_msi(model.bdf, 0xfee0_0000, 0x31)
            .unwrap();
        assert_eq!(model.read_u32(model.bdf, 0x44), Some(0xfee0_0000));
        assert_eq!(model.read_u16(model.bdf, 0x48), Some(0x31));
        assert_eq!(msi_control(&model), 0b110);
        model.activate_n305_display_msi(&before).unwrap();
        assert_eq!(msi_control(&model), 0b111);
        model.restore_n305_display_msi(&before).unwrap();
    }

    #[test]
    fn n305_msi_partial_write_failure_restores_the_full_before_image() {
        let mut model = MsiModel::n305((1 << 7) | (1 << 8), 0);
        model.insert(0x4c, 0x0000_0067);
        let original = model.words.clone();
        model.fail_once_at(0x48);
        let error = model
            .prepare_n305_display_msi(model.bdf, 0x1234_5678_9abc_def0, 0x45)
            .unwrap_err();
        assert!(error.rollback_verified, "{}", error.message);
        assert!(
            error.message.contains("before-image restored"),
            "{}",
            error.message
        );
        assert_eq!(model.words, original);
    }

    #[test]
    fn n305_msi_landed_write_failures_restore_each_prepare_prefix() {
        // 64-bit MSI with PVM: capability control, PCI COMMAND, low/high
        // address, message data, and vector mask are six distinct writes.
        for landed_write in 1..=6 {
            let mut model = MsiModel::n305((1 << 7) | (1 << 8), 0);
            let original = model.words.clone();
            model.inject_write(landed_write, true);
            let error = model
                .prepare_n305_display_msi(model.bdf, 0x1234_5678_9abc_def0, 0x45)
                .unwrap_err();
            assert!(
                error.rollback_verified,
                "write {landed_write}: {}",
                error.message
            );
            assert_eq!(
                model.words, original,
                "landed write {landed_write} must restore the complete before-image"
            );
        }
    }

    #[test]
    fn n305_msi_landed_activation_and_unmask_failures_restore_before_image() {
        for fail_activation in [true, false] {
            let mut model = MsiModel::n305((1 << 7) | (1 << 8), 0);
            let original = model.words.clone();
            let before = model
                .prepare_n305_display_msi(model.bdf, 0x1234_5678_9abc_def0, 0x45)
                .unwrap();
            if fail_activation {
                model.inject_write(model.write_count + 1, true);
                assert!(model.activate_n305_display_msi(&before).is_err());
            } else {
                model.activate_n305_display_msi(&before).unwrap();
                model.inject_write(model.write_count + 1, true);
                assert!(model.unmask_n305_display_msi(&before).is_err());
            }
            // The IRQ installer's rollback path owns restoration after these
            // post-prepare failures, while the global display master is off.
            model.restore_n305_display_msi(&before).unwrap();
            assert_eq!(model.words, original);
        }
    }

    #[test]
    fn n305_msi_readback_failure_restores_and_unverified_rollback_is_reported() {
        let mut readback = MsiModel::n305((1 << 7) | (1 << 8), 0);
        let original = readback.words.clone();
        readback.corrupt_read_once.set(Some((0x4c, 5)));
        let error = readback
            .prepare_n305_display_msi(readback.bdf, 0x1234_5678_9abc_def0, 0x45)
            .unwrap_err();
        assert!(error.rollback_verified, "{}", error.message);
        assert!(
            error.message.contains("readback differs"),
            "{}",
            error.message
        );
        assert_eq!(readback.words, original);

        let mut rollback = MsiModel::n305((1 << 7) | (1 << 8), 0);
        rollback.inject_write(1, true);
        rollback.inject_write(2, false);
        let error = rollback
            .prepare_n305_display_msi(rollback.bdf, 0x1234_5678_9abc_def0, 0x45)
            .unwrap_err();
        assert!(!error.rollback_verified, "{}", error.message);
        assert!(error.message.contains("PCI MSI rollback unverified"));
    }

    #[test]
    fn n305_msi_refuses_enabled_unowned_vectors_and_overlapping_capabilities() {
        let mut already_enabled = MsiModel::n305(1, 0);
        assert!(
            already_enabled
                .prepare_n305_display_msi(already_enabled.bdf, 0xfee0_0000, 0x20)
                .is_err()
        );

        let mut msix = MsiModel::n305(0, 0x60);
        msix.insert(0x60, 0x8000_0000 | 0x11);
        assert!(
            msix.prepare_n305_display_msi(msix.bdf, 0xfee0_0000, 0x20)
                .is_err()
        );

        let mut overlap = MsiModel::n305((1 << 7) | (1 << 8), 0x50);
        overlap.insert(0x50, 0x11);
        assert!(
            overlap
                .prepare_n305_display_msi(overlap.bdf, 0x1234_5678_9abc_def0, 0x45)
                .is_err()
        );

        let mut short_cap = MsiModel::n305((1 << 7) | (1 << 8), 0);
        short_cap.insert(0x34, 0xfc);
        short_cap.insert(0xfc, (u32::from((1u16 << 7) | (1u16 << 8)) << 16) | 0x05);
        assert!(
            short_cap
                .prepare_n305_display_msi(short_cap.bdf, 0x1234_5678_9abc_def0, 0x45)
                .is_err()
        );
    }

    #[test]
    fn runtime_ecam_does_not_require_the_old_configured_mmio_range() {
        assert_eq!(ecam_span(0xc000_0000, 0, 255), Some(256 << 20));
        assert_eq!(ecam_span(0xc000_0000, 0, 63), Some(64 << 20));
        assert_eq!(ecam_span(0, 0, 255), None);
        assert_eq!(ecam_span(0xc000_0001, 0, 255), None);
        assert_eq!(ecam_span(0xc000_0000, 1, 255), None);
        assert_eq!(ecam_span(usize::MAX & !0xfffff, 0, 255), None);
    }

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
    fn the_interrupt_line_and_pin_are_read_and_said_in_words() {
        // The two bytes at 0x3c and 0x3d, which are the answer to "is there a
        // legacy INTx route on this function at all".  They are read from the
        // same dword, one byte apart, and neither one changes any decision the
        // probe makes: the identity and the window are the same either way.
        let bus = FakeBus::new(vec![
            Header::new(Bdf::new(0, 2, 0), VENDOR_INTEL, 0x46d0)
                .class(CLASS_DISPLAY, SUBCLASS_VGA)
                .interrupt(0x0b, 1),
            Header::new(Bdf::new(0, 2, 1), VENDOR_INTEL, 0x46d1)
                .class(CLASS_DISPLAY, SUBCLASS_VGA)
                .interrupt(0xff, 0),
        ]);
        let routing = DeviceInfo::read(&bus, Bdf::new(0, 2, 0)).unwrap();
        assert_eq!(routing.interrupt_line, 0x0b);
        assert_eq!(routing.interrupt_pin, 1);
        assert!(
            routing
                .describe_identity()
                .contains("interrupt pin 0x01 line 0x0b")
        );
        let described = routing.describe_interrupt();
        assert!(described.contains("INTA#"), "{described}");
        assert!(described.contains("IRQ 11"), "{described}");

        // A function that asserts no pin can only be reached by MSI, and a
        // header firmware left unassigned reads as unassigned rather than as
        // IRQ 255.
        let msi_only = DeviceInfo::read(&bus, Bdf::new(0, 2, 1)).unwrap();
        assert!(msi_only.is_intel_display());
        let described = msi_only.describe_interrupt();
        assert!(described.contains("only MSI can reach"), "{described}");
        assert!(!described.contains("IRQ 255"), "{described}");
        // The probe's decision is untouched by either value.
        assert_eq!(
            DeviceInfo::read(&bus, Bdf::new(0, 2, 0))
                .unwrap()
                .has_standard_header(),
            msi_only.has_standard_header()
        );
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
