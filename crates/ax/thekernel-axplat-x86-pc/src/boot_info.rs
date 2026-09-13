//! Owned Multiboot handoff data.
//!
//! The platform entry point is reached before the runtime clears `.bss`, while
//! the Multiboot information block is owned by the bootloader and may be
//! reused as soon as normal memory initialization starts.  This module keeps
//! the raw entry record in initialized data, then copies every value needed by
//! the platform into one immutable owner before handing control to the rest of
//! the platform code.

use core::convert::TryFrom;

use axplat::mem::{PhysAddr, RawRange, phys_to_virt};
use lazyinit::LazyInit;

use crate::boot::{EarlyBootRecord, MULTIBOOT_BOOTLOADER_MAGIC, MULTIBOOT2_BOOTLOADER_MAGIC};

pub(crate) const MAX_REGIONS: usize = 16;
pub(crate) const MAX_MODULES: usize = 8;
const MAX_MB2_INFO_SIZE: usize = 16 * 1024 * 1024;
const MAX_RSDP_LENGTH: usize = 4096;
const MAX_MODULE_CMDLINE: usize = 256;
const PAGE_SIZE: usize = 4096;

const MB2_TAG_END: u32 = 0;
const MB2_TAG_MMAP: u32 = 6;
const MB2_TAG_MODULE: u32 = 3;
const MB2_TAG_ACPI_OLD: u32 = 14;
const MB2_TAG_ACPI_NEW: u32 = 15;

/// Upper bound on the number of Multiboot2 tags retained for diagnostics.
///
/// A bare-metal bring-up has to distinguish "the bootloader did not supply a
/// tag" from "the kernel ignored it": video, EFI and platform tags depend on
/// the firmware, the bootloader configuration and the machine, none of which
/// the kernel controls.  Retaining the declared headers makes that answer
/// available from a single boot instead of a bisect.  The bound is fixed
/// because the tag list belongs to the bootloader and the inventory must not
/// allocate before the heap exists.
pub(crate) const MAX_TAG_INVENTORY: usize = 32;

/// One Multiboot2 tag header exactly as the bootloader declared it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TagRecord {
    /// Multiboot2 tag type.
    pub(crate) tag_type: u32,
    /// Declared tag size in bytes, including the eight-byte tag header.
    pub(crate) size: u32,
}

const MB2_TAG_FRAMEBUFFER: u32 = 8;

/// Size of a framebuffer tag up to and including its `reserved` field.
///
/// The tag continues with a kind-dependent payload; only the fixed portion is
/// read unconditionally, so every accessor below is checked against this base
/// before it reads a field.
const MB2_FRAMEBUFFER_BASE_SIZE: usize = 32;

/// Framebuffer kind for palette-indexed pixels.
const MB2_FRAMEBUFFER_INDEXED: u8 = 0;
/// Framebuffer kind for direct RGB pixels.
const MB2_FRAMEBUFFER_RGB: u8 = 1;
/// Framebuffer kind for EGA text cells, which are not pixels at all.
const MB2_FRAMEBUFFER_TEXT: u8 = 2;

/// Bit position and width of one colour channel inside a pixel.
///
/// The position is the index of the channel's least significant bit, which is
/// what Multiboot2 reports.  Both values are retained rather than reduced to a
/// shift-and-mask pair because a caller drawing into the surface has to
/// scale an 8-bit channel into a field that is not necessarily eight bits wide.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ColorField {
    position: u8,
    size: u8,
}

impl ColorField {
    /// Index of the channel's least significant bit within a pixel.
    pub fn position(&self) -> u8 {
        self.position
    }

    /// Channel width in bits.
    pub fn size(&self) -> u8 {
        self.size
    }
}

/// A bootloader-supplied linear RGB framebuffer.
///
/// Only the direct-RGB kind becomes a `FramebufferInfo`.  An indexed surface
/// needs a palette this owner deliberately does not retain, and an EGA text
/// surface addresses character cells rather than pixels; both are reported as
/// a [`FramebufferRejection`] instead so the kernel can still boot without a
/// display.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FramebufferInfo {
    address: u64,
    pitch: u32,
    width: u32,
    height: u32,
    bpp: u8,
    red: ColorField,
    green: ColorField,
    blue: ColorField,
}

impl FramebufferInfo {
    /// Physical address of the first pixel of the first scan line.
    pub fn address(&self) -> u64 {
        self.address
    }

    /// Bytes between the starts of two consecutive scan lines.
    ///
    /// This is not necessarily `width * bpp / 8`: firmware frequently pads
    /// scan lines, and a driver that assumes a tight stride shears the image.
    pub fn pitch(&self) -> u32 {
        self.pitch
    }

    /// Visible width in pixels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Visible height in pixels.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Bits per pixel.
    pub fn bpp(&self) -> u8 {
        self.bpp
    }

    /// Red channel layout.
    pub fn red(&self) -> ColorField {
        self.red
    }

    /// Green channel layout.
    pub fn green(&self) -> ColorField {
        self.green
    }

    /// Blue channel layout.
    pub fn blue(&self) -> ColorField {
        self.blue
    }

    /// Bytes actually occupied by the visible surface.
    ///
    /// Returns `None` rather than wrapping when the extent cannot be
    /// represented, so a caller cannot derive a short length and map a window
    /// smaller than the pixels it is about to write.
    pub fn byte_len(&self) -> Option<usize> {
        let stride = usize::try_from(self.pitch).ok()?;
        let rows = usize::try_from(self.height).ok()?;
        stride.checked_mul(rows)
    }
}

/// Why a framebuffer tag did not produce a usable linear surface.
///
/// A missing or unusable framebuffer is not a boot failure: the machine has
/// simply lost its display.  The reason is retained because on hardware with
/// no serial port the display *is* the diagnostic channel, and "the tag was
/// absent" and "the tag was rejected" call for completely different fixes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FramebufferRejection {
    /// The tag is shorter than its fixed portion.
    Truncated,
    /// The declared kind is the palette-indexed one.
    Indexed,
    /// The declared kind is EGA text, which addresses character cells.
    Text,
    /// The declared kind is not one this kernel knows.
    UnknownKind(u8),
    /// The declared geometry, pitch, or colour layout contradicts itself.
    Inconsistent,
    /// The declared address is one no display can live at.
    ///
    /// A bootloader reports this when it filled in a mode description but
    /// never actually programmed a display, which is a bootloader
    /// configuration problem rather than a firmware one.
    UnusableAddress,
    /// The declared surface would lie inside memory the kernel may allocate.
    OverlapsUsableMemory,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BootProtocol {
    Multiboot1,
    Multiboot2,
}

/// The subset of the ACPI RSDP needed to locate the root table.
///
/// The full RSDP is validated while the bootloader-owned bytes are still
/// borrowed.  Only this fixed-size copy is retained, so no platform code ever
/// holds a reference into the Multiboot information block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AcpiRsdp {
    bytes: [u8; 36],
    length: usize,
}

/// Immutable metadata for a Multiboot module.
///
/// Module bytes remain owned by the bootloader handoff allocation. The
/// platform retains only this bounded copy of their physical extent and
/// command line; [`crate::boot_modules`] never borrows the Multiboot block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModuleInfo {
    start: usize,
    end: usize,
    command: [u8; MAX_MODULE_CMDLINE],
    command_len: usize,
}

impl ModuleInfo {
    /// Physical byte range occupied by this module.
    pub fn range(&self) -> (usize, usize) {
        (self.start, self.end)
    }

    /// The module command line, excluding its Multiboot NUL terminator.
    pub fn command(&self) -> &[u8] {
        &self.command[..self.command_len]
    }

    /// Whether this module was explicitly staged as the product rootfs.
    pub fn is_rootfs(&self) -> bool {
        self.command() == b"rootfs"
    }
}

impl AcpiRsdp {
    pub(crate) fn bytes(&self) -> &[u8; 36] {
        &self.bytes
    }

    #[allow(dead_code)]
    pub(crate) fn length(&self) -> usize {
        self.length
    }
}

/// The sole owner of boot protocol data after early handoff.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BootInfo {
    protocol: BootProtocol,
    info_paddr: usize,
    rsdp: Option<AcpiRsdp>,
    memory_regions: [RawRange; MAX_REGIONS],
    memory_region_count: usize,
    modules: [Option<ModuleInfo>; MAX_MODULES],
    module_count: usize,
    tags: [TagRecord; MAX_TAG_INVENTORY],
    tag_count: usize,
    tags_truncated: bool,
    framebuffer: Option<FramebufferInfo>,
    framebuffer_rejection: Option<FramebufferRejection>,
}

impl BootInfo {
    fn empty(protocol: BootProtocol, info_paddr: usize) -> Self {
        Self {
            protocol,
            info_paddr,
            rsdp: None,
            memory_regions: [(0, 0); MAX_REGIONS],
            memory_region_count: 0,
            modules: [None; MAX_MODULES],
            module_count: 0,
            tags: [TagRecord {
                tag_type: 0,
                size: 0,
            }; MAX_TAG_INVENTORY],
            tag_count: 0,
            tags_truncated: false,
            framebuffer: None,
            framebuffer_rejection: None,
        }
    }

    pub(crate) fn protocol(&self) -> BootProtocol {
        self.protocol
    }

    pub(crate) fn info_paddr(&self) -> usize {
        self.info_paddr
    }

    pub(crate) fn rsdp(&self) -> Option<&AcpiRsdp> {
        self.rsdp.as_ref()
    }

    pub(crate) fn memory_regions(&self) -> &[RawRange] {
        &self.memory_regions[..self.memory_region_count]
    }

    pub(crate) fn modules(&self) -> &[Option<ModuleInfo>] {
        &self.modules[..self.module_count]
    }

    /// Tag headers supplied by the bootloader, in the order they appeared.
    pub(crate) fn tags(&self) -> &[TagRecord] {
        &self.tags[..self.tag_count]
    }

    /// Whether the bootloader supplied more tags than the inventory retains.
    pub(crate) fn tags_truncated(&self) -> bool {
        self.tags_truncated
    }

    /// The linear RGB framebuffer the bootloader handed over, if any.
    pub fn framebuffer(&self) -> Option<&FramebufferInfo> {
        self.framebuffer.as_ref()
    }

    /// Why no usable framebuffer was retained, if the bootloader tried.
    pub(crate) fn framebuffer_rejection(&self) -> Option<FramebufferRejection> {
        self.framebuffer_rejection
    }
}

static BOOT_INFO: LazyInit<BootInfo> = LazyInit::new();

pub(crate) fn get() -> &'static BootInfo {
    BOOT_INFO
        .get()
        .expect("x86 boot handoff has not been finalized")
}

/// Finish the early handoff after `.bss` has been cleared.
pub(crate) fn finish_handoff() {
    let EarlyBootRecord { magic, info_paddr } = crate::boot::early_record();
    let protocol = match magic {
        MULTIBOOT_BOOTLOADER_MAGIC => BootProtocol::Multiboot1,
        MULTIBOOT2_BOOTLOADER_MAGIC => BootProtocol::Multiboot2,
        _ => panic!("unsupported x86 boot magic {magic:#x}"),
    };

    let owner = match protocol {
        BootProtocol::Multiboot1 => BootInfo::empty(protocol, info_paddr),
        BootProtocol::Multiboot2 => {
            let bytes = unsafe { multiboot2_info_bytes(info_paddr) }.unwrap_or_else(|| {
                panic!("invalid Multiboot2 information pointer {info_paddr:#x}")
            });
            parse_multiboot2_info(bytes, info_paddr)
                .unwrap_or_else(|error| panic!("invalid Multiboot2 information: {error:?}"))
        }
    };
    report_tag_inventory(&owner);
    BOOT_INFO.init_once(owner);
}

/// Report the bootloader-supplied tag inventory on the diagnostic channel.
///
/// This runs before the immutable owner is published so the evidence survives
/// a later handoff failure.  It writes to COM2 diagnostics rather than the
/// COM1 console: the console is the guest-visible TTY path, while this is
/// platform bring-up evidence, and on a machine without a working console the
/// diagnostic channel is the only one that can still be read.
fn report_tag_inventory(info: &BootInfo) {
    diagnostic_println!(
        "MB2 tag inventory: protocol={:?} count={} truncated={}",
        info.protocol(),
        info.tag_count,
        info.tags_truncated as u8
    );
    for record in info.tags() {
        diagnostic_println!("MB2 tag type={} size={}", record.tag_type, record.size);
    }
    report_framebuffer(info);
}

/// Report the retained framebuffer, or the reason the bootloader's was declined.
///
/// This is the single most load-bearing line of a serial-less bring-up: without
/// it, "the screen stayed black" has no distinguishable causes.
fn report_framebuffer(info: &BootInfo) {
    match (info.framebuffer(), info.framebuffer_rejection()) {
        (Some(fb), _) => diagnostic_println!(
            "MB2 framebuffer: addr={:#x} {}x{} bpp={} pitch={} len={:#x} rgb_bits={}/{}/{}",
            fb.address(),
            fb.width(),
            fb.height(),
            fb.bpp(),
            fb.pitch(),
            fb.byte_len().unwrap_or(0),
            fb.red().size(),
            fb.green().size(),
            fb.blue().size(),
        ),
        (None, Some(reason)) => {
            diagnostic_println!("MB2 framebuffer: declined reason={:?}", reason)
        }
        (None, None) => diagnostic_println!("MB2 framebuffer: absent"),
    }
}

/// Parse an MB2 information block from a physical address, without retaining
/// a borrow into it.
unsafe fn multiboot2_info_bytes(info_paddr: usize) -> Option<&'static [u8]> {
    if info_paddr & 7 != 0 {
        return None;
    }
    let header = unsafe { physical_bytes(info_paddr, 16)? };
    let total_size = read_u32(header, 0)? as usize;
    if !(16..=MAX_MB2_INFO_SIZE).contains(&total_size) || total_size & 7 != 0 {
        return None;
    }
    unsafe { physical_bytes(info_paddr, total_size) }
}

unsafe fn physical_bytes(address: usize, length: usize) -> Option<&'static [u8]> {
    address.checked_add(length)?;
    let ptr = phys_to_virt(PhysAddr::from_usize(address)).as_ptr();
    Some(unsafe { core::slice::from_raw_parts(ptr, length) })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ParseError {
    InfoTooSmall,
    InfoTooLarge,
    InfoNotAligned,
    ReservedHeader,
    TagHeaderTruncated,
    TagTooSmall,
    TagOutOfBounds,
    TagAlignment,
    EndTagMissing,
    EndTagMalformed,
    DuplicateMemoryMap,
    MemoryMapMissing,
    MemoryMapMalformed,
    MemoryMapCapacity,
    MemoryRangeOverflow,
    MemoryRangeConversion,
    ModuleTagMalformed,
    ModuleRangeOverflow,
    ModuleRangeAlignment,
    ModuleRangeOverlap,
    ModuleRangeOutsideMemory,
    ModuleCapacity,
    ModuleCommandLine,
    AcpiTagTruncated,
    AcpiTagMalformed,
    AcpiSignature,
    AcpiChecksum,
    AcpiLength,
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let bytes = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let bytes = bytes.get(offset..offset.checked_add(8)?)?;
    Some(u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]))
}

fn checksum_is_valid(bytes: &[u8]) -> bool {
    bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)) == 0
}

fn align_tag_size(size: usize) -> Option<usize> {
    size.checked_add(7).map(|size| size & !7)
}

fn parse_acpi_tag(payload: &[u8], tag_type: u32) -> Result<AcpiRsdp, ParseError> {
    let minimum = if tag_type == MB2_TAG_ACPI_NEW { 36 } else { 20 };
    if payload.len() < minimum {
        return Err(ParseError::AcpiTagTruncated);
    }
    if &payload[..8] != b"RSD PTR " {
        return Err(ParseError::AcpiSignature);
    }
    if !checksum_is_valid(&payload[..20]) {
        return Err(ParseError::AcpiChecksum);
    }

    let revision = payload[15];
    if tag_type == MB2_TAG_ACPI_NEW && revision < 2 {
        return Err(ParseError::AcpiTagMalformed);
    }
    let length = if revision >= 2 {
        if payload.len() < 36 {
            return Err(ParseError::AcpiLength);
        }
        let length = read_u32(payload, 20).ok_or(ParseError::AcpiLength)? as usize;
        if !(36..=MAX_RSDP_LENGTH).contains(&length) || length > payload.len() {
            return Err(ParseError::AcpiLength);
        }
        if !checksum_is_valid(&payload[..length]) {
            return Err(ParseError::AcpiChecksum);
        }
        length
    } else {
        20
    };

    let mut bytes = [0; 36];
    bytes[..36.min(payload.len())].copy_from_slice(&payload[..36.min(payload.len())]);
    Ok(AcpiRsdp { bytes, length })
}

fn parse_memory_map(
    tag: &[u8],
    regions: &mut [RawRange; MAX_REGIONS],
) -> Result<usize, ParseError> {
    if tag.len() < 16 {
        return Err(ParseError::MemoryMapMalformed);
    }
    let entry_size = read_u32(tag, 8).ok_or(ParseError::MemoryMapMalformed)? as usize;
    if entry_size < 24 || entry_size & 7 != 0 {
        return Err(ParseError::MemoryMapMalformed);
    }
    let payload_len = tag
        .len()
        .checked_sub(16)
        .ok_or(ParseError::MemoryMapMalformed)?;
    if payload_len % entry_size != 0 {
        return Err(ParseError::MemoryMapMalformed);
    }

    let mut count = 0;
    let mut offset = 16;
    while offset < tag.len() {
        let entry = tag
            .get(
                offset
                    ..offset
                        .checked_add(entry_size)
                        .ok_or(ParseError::MemoryMapMalformed)?,
            )
            .ok_or(ParseError::MemoryMapMalformed)?;
        let base = read_u64(entry, 0).ok_or(ParseError::MemoryMapMalformed)?;
        let length = read_u64(entry, 8).ok_or(ParseError::MemoryMapMalformed)?;
        let kind = read_u32(entry, 16).ok_or(ParseError::MemoryMapMalformed)?;
        if kind == 1 && length != 0 {
            let base = usize::try_from(base).map_err(|_| ParseError::MemoryRangeConversion)?;
            let length = usize::try_from(length).map_err(|_| ParseError::MemoryRangeConversion)?;
            base.checked_add(length)
                .ok_or(ParseError::MemoryRangeOverflow)?;
            if count == regions.len() {
                return Err(ParseError::MemoryMapCapacity);
            }
            regions[count] = (base, length);
            count += 1;
        }
        offset += entry_size;
    }
    Ok(count)
}

fn parse_module(tag: &[u8]) -> Result<ModuleInfo, ParseError> {
    if tag.len() < 17 {
        return Err(ParseError::ModuleTagMalformed);
    }
    let start = read_u32(tag, 8).ok_or(ParseError::ModuleTagMalformed)? as usize;
    let end = read_u32(tag, 12).ok_or(ParseError::ModuleTagMalformed)? as usize;
    if start >= end {
        return Err(ParseError::ModuleRangeOverflow);
    }
    if start & (PAGE_SIZE - 1) != 0 || end & (PAGE_SIZE - 1) != 0 {
        return Err(ParseError::ModuleRangeAlignment);
    }
    let command = &tag[16..];
    if command.last() != Some(&0) || command.len() - 1 > MAX_MODULE_CMDLINE {
        return Err(ParseError::ModuleCommandLine);
    }
    if command[..command.len() - 1].contains(&0) {
        return Err(ParseError::ModuleCommandLine);
    }
    let mut owned_command = [0; MAX_MODULE_CMDLINE];
    let command_len = command.len() - 1;
    owned_command[..command_len].copy_from_slice(&command[..command_len]);
    Ok(ModuleInfo {
        start,
        end,
        command: owned_command,
        command_len,
    })
}

fn module_is_available(module: ModuleInfo, regions: &[RawRange]) -> bool {
    regions.iter().any(|&(start, length)| {
        start
            .checked_add(length)
            .is_some_and(|end| start <= module.start && module.end <= end)
    })
}

/// Whether a physical range avoids every region of usable RAM.
///
/// The surface is written through, not merely read, so a range that shares
/// bytes with allocatable memory would corrupt unrelated allocations once the
/// allocator starts handing that memory out.
fn range_is_outside_memory(start: u64, length: u64, regions: &[RawRange]) -> bool {
    let Some(end) = start.checked_add(length) else {
        return false;
    };
    !regions.iter().any(|&(region_start, region_length)| {
        let region_start = region_start as u64;
        region_start
            .checked_add(region_length as u64)
            .is_some_and(|region_end| start < region_end && region_start < end)
    })
}

/// Validate a framebuffer tag and retain it when it describes a usable surface.
///
/// Every field is checked for self-consistency before it is trusted, because
/// this range is about to become write-only kernel memory: a plausible but
/// wrong `pitch` or `height` turns console output into arbitrary writes.  The
/// checks therefore reject only contradictions the tag cannot survive on its
/// own terms.
///
/// One contradiction is deliberately *not* checked here.  Whether the surface
/// overlaps memory the kernel may allocate is a property of the memory map
/// rather than of the tag, and the map may not have been seen yet when this
/// tag arrives.  [`parse_multiboot2_info`] applies that check once the whole
/// block has been read.
///
/// A tag that fails any check is *declined* rather than treated as a boot
/// error, so losing the display never costs the machine its ability to boot.
fn parse_framebuffer(tag: &[u8]) -> Result<FramebufferInfo, FramebufferRejection> {
    if tag.len() < MB2_FRAMEBUFFER_BASE_SIZE {
        return Err(FramebufferRejection::Truncated);
    }
    let address = read_u64(tag, 8).ok_or(FramebufferRejection::Truncated)?;
    let pitch = read_u32(tag, 16).ok_or(FramebufferRejection::Truncated)?;
    let width = read_u32(tag, 20).ok_or(FramebufferRejection::Truncated)?;
    let height = read_u32(tag, 24).ok_or(FramebufferRejection::Truncated)?;
    let bpp = *tag.get(28).ok_or(FramebufferRejection::Truncated)?;
    let kind = *tag.get(29).ok_or(FramebufferRejection::Truncated)?;

    match kind {
        MB2_FRAMEBUFFER_RGB => {}
        MB2_FRAMEBUFFER_INDEXED => return Err(FramebufferRejection::Indexed),
        MB2_FRAMEBUFFER_TEXT => return Err(FramebufferRejection::Text),
        other => return Err(FramebufferRejection::UnknownKind(other)),
    }

    // An RGB tag always carries six colour-layout bytes.  Their absence means
    // the tag contradicts itself; the channel order must not be guessed,
    // because guessing it wrong swaps red and blue on every pixel.
    let layout = tag
        .get(MB2_FRAMEBUFFER_BASE_SIZE..MB2_FRAMEBUFFER_BASE_SIZE + 6)
        .ok_or(FramebufferRejection::Truncated)?;
    let red = ColorField {
        position: layout[0],
        size: layout[1],
    };
    let green = ColorField {
        position: layout[2],
        size: layout[3],
    };
    let blue = ColorField {
        position: layout[4],
        size: layout[5],
    };

    if width == 0 || height == 0 {
        return Err(FramebufferRejection::Inconsistent);
    }
    // A bootloader that describes a mode but never programmed a display
    // reports a zero base.  Homing the surface at physical zero would write
    // console output over the real-mode interrupt vector table.
    if address == 0 {
        return Err(FramebufferRejection::UnusableAddress);
    }
    // The pixel sizes a scanout can be driven with.  15 and 16 share a pixel
    // size but not a layout, so they stay distinct.
    if !matches!(bpp, 15 | 16 | 24 | 32) {
        return Err(FramebufferRejection::Inconsistent);
    }
    for field in [red, green, blue] {
        if field.size == 0 || u16::from(field.position) + u16::from(field.size) > u16::from(bpp) {
            return Err(FramebufferRejection::Inconsistent);
        }
    }

    // A scan line must hold its pixels.  Firmware routinely pads scan lines,
    // so this is a lower bound: requiring equality would reject valid modes.
    let minimum_pitch = (u64::from(width) * u64::from(bpp)).div_ceil(8);
    if u64::from(pitch) < minimum_pitch {
        return Err(FramebufferRejection::Inconsistent);
    }
    if address
        .checked_add(u64::from(pitch) * u64::from(height))
        .is_none()
    {
        return Err(FramebufferRejection::Inconsistent);
    }

    Ok(FramebufferInfo {
        address,
        pitch,
        width,
        height,
        bpp,
        red,
        green,
        blue,
    })
}

/// Parse a complete Multiboot2 information block into an owned `BootInfo`.
///
/// This function is intentionally independent of the physical-memory access
/// helper so host tests can exercise every boundary and malformed-tag case.
fn parse_multiboot2_info(bytes: &[u8], info_paddr: usize) -> Result<BootInfo, ParseError> {
    if info_paddr & 7 != 0 {
        return Err(ParseError::InfoNotAligned);
    }
    if bytes.len() < 8 {
        return Err(ParseError::InfoTooSmall);
    }
    let total_size = read_u32(bytes, 0).ok_or(ParseError::InfoTooSmall)? as usize;
    if total_size < 16 {
        return Err(ParseError::InfoTooSmall);
    }
    if total_size > MAX_MB2_INFO_SIZE {
        return Err(ParseError::InfoTooLarge);
    }
    if total_size & 7 != 0 {
        return Err(ParseError::InfoNotAligned);
    }
    if total_size > bytes.len() {
        return Err(ParseError::InfoTooSmall);
    }
    let bytes = &bytes[..total_size];
    if read_u32(bytes, 4) != Some(0) {
        return Err(ParseError::ReservedHeader);
    }

    let mut owner = BootInfo::empty(BootProtocol::Multiboot2, info_paddr);
    let mut cursor = 8;
    let mut saw_end = false;
    let mut saw_mmap = false;
    let mut acpi_old = None;
    let mut acpi_new = None;

    while cursor < total_size {
        if cursor & 7 != 0 {
            return Err(ParseError::TagAlignment);
        }
        let remaining = total_size - cursor;
        if remaining < 8 {
            return Err(ParseError::TagHeaderTruncated);
        }
        let tag_type = read_u32(bytes, cursor).ok_or(ParseError::TagHeaderTruncated)?;
        let tag_size = read_u32(bytes, cursor + 4).ok_or(ParseError::TagHeaderTruncated)? as usize;
        if tag_size < 8 {
            return Err(ParseError::TagTooSmall);
        }
        if tag_size > remaining {
            return Err(ParseError::TagOutOfBounds);
        }
        let aligned_size = align_tag_size(tag_size).ok_or(ParseError::TagAlignment)?;
        if aligned_size > remaining {
            return Err(ParseError::TagAlignment);
        }
        let tag = &bytes[cursor..cursor + tag_size];

        // Record every tag the bootloader declared, including ones this
        // platform does not interpret.  The inventory is the only evidence
        // available when a boot on new hardware behaves differently from the
        // reference machine, and it costs one store per tag.
        if owner.tag_count < MAX_TAG_INVENTORY {
            owner.tags[owner.tag_count] = TagRecord {
                tag_type,
                size: tag_size as u32,
            };
            owner.tag_count += 1;
        } else {
            owner.tags_truncated = true;
        }

        match tag_type {
            MB2_TAG_END => {
                if tag_size != 8 || read_u32(tag, 0) != Some(0) || cursor + 8 != total_size {
                    return Err(ParseError::EndTagMalformed);
                }
                saw_end = true;
                break;
            }
            MB2_TAG_MMAP => {
                if saw_mmap {
                    return Err(ParseError::DuplicateMemoryMap);
                }
                saw_mmap = true;
                owner.memory_region_count = parse_memory_map(tag, &mut owner.memory_regions)?;
            }
            MB2_TAG_MODULE => {
                if owner.module_count == MAX_MODULES {
                    return Err(ParseError::ModuleCapacity);
                }
                let module = parse_module(tag)?;
                if owner
                    .modules()
                    .iter()
                    .flatten()
                    .any(|existing| module.start < existing.end && existing.start < module.end)
                {
                    return Err(ParseError::ModuleRangeOverlap);
                }
                owner.modules[owner.module_count] = Some(module);
                owner.module_count += 1;
            }
            MB2_TAG_ACPI_OLD | MB2_TAG_ACPI_NEW => {
                // Firmware occasionally leaves a stale/partial ACPI tag next
                // to a valid one.  Keep validating each candidate, but let a
                // valid new tag win over a valid old tag without allowing a
                // malformed candidate to discard the other valid copy.
                if let Ok(parsed) = parse_acpi_tag(&tag[8..], tag_type) {
                    if tag_type == MB2_TAG_ACPI_NEW {
                        acpi_new = Some(parsed);
                    } else {
                        acpi_old = Some(parsed);
                    }
                }
            }
            // First tag wins.  A bootloader has no reason to describe two
            // framebuffers, and if it does, the one it listed first is the one
            // it also programmed the display for.  A later tag falls through
            // to the catch-all arm instead of overwriting the first verdict.
            MB2_TAG_FRAMEBUFFER
                if owner.framebuffer.is_none() && owner.framebuffer_rejection.is_none() =>
            {
                match parse_framebuffer(tag) {
                    Ok(info) => owner.framebuffer = Some(info),
                    Err(reason) => owner.framebuffer_rejection = Some(reason),
                }
            }
            _ => {}
        }

        cursor += aligned_size;
    }

    if !saw_end {
        return Err(ParseError::EndTagMissing);
    }
    if !saw_mmap || owner.memory_region_count == 0 {
        return Err(ParseError::MemoryMapMissing);
    }
    for module in owner.modules() {
        let module = module.expect("module slots below module_count are initialized");
        if !module_is_available(module, owner.memory_regions()) {
            return Err(ParseError::ModuleRangeOutsideMemory);
        }
    }
    // The framebuffer tag is not required to follow the memory map, so the
    // overlap rule is applied once the whole block has been read rather than
    // inside the parse arm.  A surface that shares bytes with allocatable
    // memory is refused rather than used, because the console writes through
    // it and would otherwise corrupt whatever the allocator places there.
    let framebuffer_conflict = owner.framebuffer.is_some_and(|framebuffer| {
        framebuffer.byte_len().is_none_or(|length| {
            !range_is_outside_memory(framebuffer.address(), length as u64, owner.memory_regions())
        })
    });
    if framebuffer_conflict {
        owner.framebuffer = None;
        owner.framebuffer_rejection = Some(FramebufferRejection::OverlapsUsableMemory);
    }
    owner.rsdp = acpi_new.or(acpi_old);
    Ok(owner)
}

#[cfg(test)]
mod tests {
    use super::{
        AcpiRsdp, BootProtocol, FramebufferRejection, MAX_MODULES, MAX_REGIONS, MAX_TAG_INVENTORY,
        ParseError, TagRecord, parse_multiboot2_info,
    };

    fn push_u32(bytes: &mut std::vec::Vec<u8>, value: u32) {
        bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn push_tag(bytes: &mut std::vec::Vec<u8>, tag_type: u32, payload: &[u8]) {
        let size = 8 + payload.len();
        push_u32(bytes, tag_type);
        push_u32(bytes, size as u32);
        bytes.extend_from_slice(payload);
        while bytes.len() & 7 != 0 {
            bytes.push(0);
        }
    }

    fn set_total_size(bytes: &mut std::vec::Vec<u8>) {
        let size = bytes.len() as u32;
        bytes[..4].copy_from_slice(&size.to_le_bytes());
    }

    fn valid_rsdp(seed: u8) -> [u8; 36] {
        let mut rsdp = [0; 36];
        rsdp[..8].copy_from_slice(b"RSD PTR ");
        rsdp[9] = seed;
        rsdp[15] = 2;
        rsdp[20..24].copy_from_slice(&36u32.to_le_bytes());
        let checksum20 = (0u8).wrapping_sub(
            rsdp[..20]
                .iter()
                .fold(0u8, |sum, byte| sum.wrapping_add(*byte)),
        );
        rsdp[8] = checksum20;
        let checksum36 =
            (0u8).wrapping_sub(rsdp.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)));
        rsdp[32] = checksum36;
        rsdp
    }

    fn valid_mmap_tag_payload() -> [u8; 32] {
        let mut payload = [0; 32];
        payload[..4].copy_from_slice(&24u32.to_le_bytes());
        payload[8..16].copy_from_slice(&0u64.to_le_bytes());
        payload[16..24].copy_from_slice(&0x0800_0000u64.to_le_bytes());
        payload[24..28].copy_from_slice(&1u32.to_le_bytes());
        payload
    }

    fn valid_info() -> std::vec::Vec<u8> {
        let mut bytes = vec![0; 8];
        push_tag(&mut bytes, 6, &valid_mmap_tag_payload());
        push_tag(&mut bytes, 15, &valid_rsdp(2));
        push_tag(&mut bytes, 0, &[]);
        set_total_size(&mut bytes);
        bytes
    }

    fn module_payload(start: u32, end: u32, command: &[u8]) -> std::vec::Vec<u8> {
        let mut payload = std::vec::Vec::new();
        push_u32(&mut payload, start);
        push_u32(&mut payload, end);
        payload.extend_from_slice(command);
        payload.push(0);
        payload
    }

    #[test]
    fn parser_copies_mmap_and_new_rsdp() {
        let mut bytes = valid_info();
        let info = parse_multiboot2_info(&bytes, 0x1000).unwrap();
        // The Multiboot information block (and its ACPI tag) is only borrowed
        // during parsing.  The platform must retain the copied RSDP rather
        // than relying on the bootloader-owned bytes after the handoff.
        bytes.fill(0);
        assert_eq!(info.protocol(), BootProtocol::Multiboot2);
        assert_eq!(info.info_paddr(), 0x1000);
        assert_eq!(info.memory_regions(), &[(0, 0x0800_0000)]);
        assert_eq!(info.rsdp().unwrap().bytes()[9], 2);
    }

    #[test]
    fn parser_owns_page_aligned_rootfs_module() {
        let mut bytes = vec![0; 8];
        push_tag(&mut bytes, 6, &valid_mmap_tag_payload());
        push_tag(
            &mut bytes,
            3,
            &module_payload(0x0200_0000, 0x0220_0000, b"rootfs"),
        );
        push_tag(&mut bytes, 0, &[]);
        set_total_size(&mut bytes);

        let info = parse_multiboot2_info(&bytes, 0x1000).unwrap();
        bytes.fill(0);
        let module = info.modules()[0].unwrap();
        assert_eq!(module.range(), (0x0200_0000, 0x0220_0000));
        assert_eq!(module.command(), b"rootfs");
        assert!(module.is_rootfs());
    }

    #[test]
    fn parser_rejects_misaligned_or_unavailable_modules() {
        let mut bytes = vec![0; 8];
        push_tag(&mut bytes, 6, &valid_mmap_tag_payload());
        push_tag(&mut bytes, 3, &module_payload(0x2001, 0x3000, b"rootfs"));
        push_tag(&mut bytes, 0, &[]);
        set_total_size(&mut bytes);
        assert_eq!(
            parse_multiboot2_info(&bytes, 0x1000),
            Err(ParseError::ModuleRangeAlignment)
        );

        let mut bytes = vec![0; 8];
        push_tag(&mut bytes, 6, &valid_mmap_tag_payload());
        push_tag(
            &mut bytes,
            3,
            &module_payload(0x0800_0000, 0x0800_1000, b"rootfs"),
        );
        push_tag(&mut bytes, 0, &[]);
        set_total_size(&mut bytes);
        assert_eq!(
            parse_multiboot2_info(&bytes, 0x1000),
            Err(ParseError::ModuleRangeOutsideMemory)
        );

        let mut bytes = vec![0; 8];
        push_tag(&mut bytes, 6, &valid_mmap_tag_payload());
        push_tag(
            &mut bytes,
            3,
            &module_payload(0x0100_0000, 0x0100_2000, b"first"),
        );
        push_tag(
            &mut bytes,
            3,
            &module_payload(0x0100_1000, 0x0100_3000, b"second"),
        );
        push_tag(&mut bytes, 0, &[]);
        set_total_size(&mut bytes);
        assert_eq!(
            parse_multiboot2_info(&bytes, 0x1000),
            Err(ParseError::ModuleRangeOverlap)
        );
    }

    #[test]
    fn parser_rejects_module_capacity_and_malformed_command_line() {
        let mut bytes = vec![0; 8];
        push_tag(&mut bytes, 6, &valid_mmap_tag_payload());
        for index in 0..=MAX_MODULES {
            let start = 0x0100_0000 + (index as u32) * 0x1000;
            push_tag(
                &mut bytes,
                3,
                &module_payload(start, start + 0x1000, b"rootfs"),
            );
        }
        push_tag(&mut bytes, 0, &[]);
        set_total_size(&mut bytes);
        assert_eq!(
            parse_multiboot2_info(&bytes, 0x1000),
            Err(ParseError::ModuleCapacity)
        );

        let mut bytes = vec![0; 8];
        push_tag(&mut bytes, 6, &valid_mmap_tag_payload());
        let mut malformed = module_payload(0x0100_0000, 0x0100_1000, b"rootfs");
        malformed.pop();
        push_tag(&mut bytes, 3, &malformed);
        push_tag(&mut bytes, 0, &[]);
        set_total_size(&mut bytes);
        assert_eq!(
            parse_multiboot2_info(&bytes, 0x1000),
            Err(ParseError::ModuleCommandLine)
        );
    }

    #[test]
    fn new_acpi_tag_wins_over_old_tag() {
        let mut bytes = vec![0; 8];
        push_tag(&mut bytes, 6, &valid_mmap_tag_payload());
        push_tag(&mut bytes, 14, &valid_rsdp(1));
        push_tag(&mut bytes, 15, &valid_rsdp(2));
        push_tag(&mut bytes, 0, &[]);
        set_total_size(&mut bytes);

        let info = parse_multiboot2_info(&bytes, 0x1000).unwrap();
        assert_eq!(info.rsdp().unwrap().bytes()[9], 2);
    }

    #[test]
    fn malformed_new_acpi_tag_falls_back_to_valid_old_tag() {
        let mut bytes = vec![0; 8];
        push_tag(&mut bytes, 6, &valid_mmap_tag_payload());
        push_tag(&mut bytes, 14, &valid_rsdp(1));
        let mut malformed_new = valid_rsdp(2);
        malformed_new[8] ^= 1;
        push_tag(&mut bytes, 15, &malformed_new);
        push_tag(&mut bytes, 0, &[]);
        set_total_size(&mut bytes);

        let info = parse_multiboot2_info(&bytes, 0x1000).unwrap();
        assert_eq!(info.rsdp().unwrap().bytes()[9], 1);
    }

    #[test]
    fn malformed_and_missing_memory_maps_are_rejected() {
        let mut bytes = vec![0; 8];
        push_tag(&mut bytes, 15, &valid_rsdp(2));
        push_tag(&mut bytes, 0, &[]);
        set_total_size(&mut bytes);
        assert_eq!(
            parse_multiboot2_info(&bytes, 0x1000),
            Err(ParseError::MemoryMapMissing)
        );

        let mut bytes = vec![0; 8];
        let mut payload = valid_mmap_tag_payload();
        payload[..4].copy_from_slice(&23u32.to_le_bytes());
        push_tag(&mut bytes, 6, &payload);
        push_tag(&mut bytes, 0, &[]);
        set_total_size(&mut bytes);
        assert_eq!(
            parse_multiboot2_info(&bytes, 0x1000),
            Err(ParseError::MemoryMapMalformed)
        );
    }

    #[test]
    fn zero_length_available_entries_are_skipped() {
        let mut payload = vec![0; 8 + 24 * 2];
        payload[..4].copy_from_slice(&24u32.to_le_bytes());
        let second = 8 + 24;
        payload[second..second + 8].copy_from_slice(&0x2000u64.to_le_bytes());
        payload[second + 8..second + 16].copy_from_slice(&0x2000u64.to_le_bytes());
        payload[second + 16..second + 20].copy_from_slice(&1u32.to_le_bytes());
        let mut bytes = vec![0; 8];
        push_tag(&mut bytes, 6, &payload);
        push_tag(&mut bytes, 0, &[]);
        set_total_size(&mut bytes);

        let info = parse_multiboot2_info(&bytes, 0x1000).unwrap();
        assert_eq!(info.memory_regions(), &[(0x2000, 0x2000)]);
    }

    #[test]
    fn extended_mmap_entries_must_be_eight_byte_aligned_and_are_supported() {
        let mut payload = vec![0; 8 + 32];
        payload[..4].copy_from_slice(&32u32.to_le_bytes());
        payload[8..16].copy_from_slice(&0x3000u64.to_le_bytes());
        payload[16..24].copy_from_slice(&0x3000u64.to_le_bytes());
        payload[24..28].copy_from_slice(&1u32.to_le_bytes());
        let mut bytes = vec![0; 8];
        push_tag(&mut bytes, 6, &payload);
        push_tag(&mut bytes, 0, &[]);
        set_total_size(&mut bytes);

        let info = parse_multiboot2_info(&bytes, 0x1000).unwrap();
        assert_eq!(info.memory_regions(), &[(0x3000, 0x3000)]);

        let mut malformed = bytes;
        malformed[16..20].copy_from_slice(&31u32.to_le_bytes());
        assert_eq!(
            parse_multiboot2_info(&malformed, 0x1000),
            Err(ParseError::MemoryMapMalformed)
        );
    }

    #[test]
    fn end_tag_must_be_the_last_aligned_tag() {
        let mut bytes = valid_info();
        bytes.extend_from_slice(&[0; 8]);
        set_total_size(&mut bytes);
        assert_eq!(
            parse_multiboot2_info(&bytes, 0x1000),
            Err(ParseError::EndTagMalformed)
        );
    }

    #[test]
    fn mmap_capacity_is_fatal_instead_of_truncating() {
        let mut payload = vec![0; 8 + 24 * (MAX_REGIONS + 1)];
        payload[..4].copy_from_slice(&24u32.to_le_bytes());
        for index in 0..=MAX_REGIONS {
            let offset = 8 + index * 24;
            payload[offset..offset + 8].copy_from_slice(&((index * 0x1000) as u64).to_le_bytes());
            payload[offset + 8..offset + 16].copy_from_slice(&0x1000u64.to_le_bytes());
            payload[offset + 16..offset + 20].copy_from_slice(&1u32.to_le_bytes());
        }
        let mut bytes = vec![0; 8];
        push_tag(&mut bytes, 6, &payload);
        push_tag(&mut bytes, 0, &[]);
        set_total_size(&mut bytes);
        assert_eq!(
            parse_multiboot2_info(&bytes, 0x1000),
            Err(ParseError::MemoryMapCapacity)
        );
    }

    #[test]
    fn rsdp_copy_is_fixed_size_and_owned() {
        let rsdp = AcpiRsdp {
            bytes: valid_rsdp(7),
            length: 36,
        };
        let mut bytes = *rsdp.bytes();
        bytes[9] = 9;
        assert_eq!(bytes[9], 9);
        assert_eq!(rsdp.bytes()[9], 7);
    }

    #[test]
    fn tag_inventory_records_every_declared_header_in_order() {
        let mut bytes = vec![0; 8];
        push_tag(&mut bytes, 6, &valid_mmap_tag_payload());
        // A tag this platform does not interpret must still be inventoried:
        // "the bootloader never sent one" and "the kernel ignored it" are
        // different bring-up answers, and only the inventory distinguishes
        // them.
        push_tag(&mut bytes, 8, &[0; 32]);
        push_tag(&mut bytes, 15, &valid_rsdp(2));
        push_tag(&mut bytes, 0, &[]);
        set_total_size(&mut bytes);

        let info = parse_multiboot2_info(&bytes, 0x1000).unwrap();
        assert!(!info.tags_truncated());
        assert_eq!(
            info.tags(),
            &[
                TagRecord {
                    tag_type: 6,
                    size: 40
                },
                TagRecord {
                    tag_type: 8,
                    size: 40
                },
                TagRecord {
                    tag_type: 15,
                    size: 44
                },
                TagRecord {
                    tag_type: 0,
                    size: 8
                },
            ]
        );
    }

    #[test]
    fn tag_inventory_truncates_without_failing_the_handoff() {
        let mut bytes = vec![0; 8];
        push_tag(&mut bytes, 6, &valid_mmap_tag_payload());
        for _ in 0..MAX_TAG_INVENTORY {
            push_tag(&mut bytes, 8, &[0; 8]);
        }
        push_tag(&mut bytes, 0, &[]);
        set_total_size(&mut bytes);

        let info = parse_multiboot2_info(&bytes, 0x1000).unwrap();
        assert!(info.tags_truncated());
        assert_eq!(info.tags().len(), MAX_TAG_INVENTORY);
        // The memory map is parsed from its own tag rather than from the
        // inventory, so overflowing a diagnostic bound must not change any
        // data the platform actually consumes.
        assert_eq!(info.memory_regions(), &[(0, 0x0800_0000)]);
    }

    /// Build a framebuffer tag payload.  `push_tag` adds the eight-byte header,
    /// so a payload of 30 bytes yields the 38-byte tag GRUB emits for RGB.
    fn framebuffer_payload(
        address: u64,
        pitch: u32,
        width: u32,
        height: u32,
        bpp: u8,
        kind: u8,
        layout: [u8; 6],
    ) -> std::vec::Vec<u8> {
        let mut payload = std::vec::Vec::new();
        payload.extend_from_slice(&address.to_le_bytes());
        payload.extend_from_slice(&pitch.to_le_bytes());
        payload.extend_from_slice(&width.to_le_bytes());
        payload.extend_from_slice(&height.to_le_bytes());
        payload.push(bpp);
        payload.push(kind);
        payload.extend_from_slice(&[0, 0]);
        payload.extend_from_slice(&layout);
        payload
    }

    /// Red at bit 16, green at bit 8, blue at bit 0: the 32-bit layout the
    /// reference QEMU firmware reports.
    const RGB_888: [u8; 6] = [16, 8, 8, 8, 0, 8];

    fn info_with_framebuffer(tag: &[u8]) -> std::vec::Vec<u8> {
        let mut bytes = vec![0; 8];
        push_tag(&mut bytes, 6, &valid_mmap_tag_payload());
        push_tag(&mut bytes, 8, tag);
        push_tag(&mut bytes, 0, &[]);
        set_total_size(&mut bytes);
        bytes
    }

    #[test]
    fn rgb_framebuffer_is_retained_with_padded_pitch() {
        // Pitch exceeds the tight stride, which is the normal case: firmware
        // pads scan lines, and a driver that recomputed the stride would shear
        // the image.
        let tag = framebuffer_payload(0x8000_0000, 4096, 800, 600, 32, 1, RGB_888);
        assert_eq!(tag.len(), 30);
        let info = parse_multiboot2_info(&info_with_framebuffer(&tag), 0x1000).unwrap();
        let framebuffer = info.framebuffer().expect("RGB tag must be retained");
        assert_eq!(framebuffer.address(), 0x8000_0000);
        assert_eq!(framebuffer.pitch(), 4096);
        assert_eq!(framebuffer.width(), 800);
        assert_eq!(framebuffer.height(), 600);
        assert_eq!(framebuffer.bpp(), 32);
        assert_eq!(framebuffer.red().position(), 16);
        assert_eq!(framebuffer.blue().position(), 0);
        assert_eq!(framebuffer.byte_len(), Some(4096 * 600));
        assert_eq!(info.framebuffer_rejection(), None);
    }

    #[test]
    fn non_rgb_framebuffers_are_declined_without_failing_the_boot() {
        // An indexed surface needs a palette this owner does not retain, and an
        // EGA text surface addresses character cells.  Neither may be treated
        // as a boot error: on a machine whose only output is the display,
        // aborting would be strictly worse than starting without one.
        for (kind, expected) in [
            (0u8, FramebufferRejection::Indexed),
            (2u8, FramebufferRejection::Text),
            (7u8, FramebufferRejection::UnknownKind(7)),
        ] {
            let tag = framebuffer_payload(0x8000_0000, 4096, 800, 600, 32, kind, RGB_888);
            let info = parse_multiboot2_info(&info_with_framebuffer(&tag), 0x1000).unwrap();
            assert!(info.framebuffer().is_none(), "kind {kind} must be declined");
            assert_eq!(info.framebuffer_rejection(), Some(expected));
            // The rest of the handoff must be unaffected by the decline.
            assert_eq!(info.memory_regions(), &[(0, 0x0800_0000)]);
        }
    }

    #[test]
    fn inconsistent_framebuffer_geometry_is_declined() {
        // Each case is self-contradictory, so trusting it would turn console
        // output into writes outside the surface the firmware described.
        let cases = [
            // Zero height.
            framebuffer_payload(0x8000_0000, 4096, 800, 0, 32, 1, RGB_888),
            // Pitch below the tight stride for 800 pixels at 32 bpp.
            framebuffer_payload(0x8000_0000, 3199, 800, 600, 32, 1, RGB_888),
            // Pixel size no scanout uses.
            framebuffer_payload(0x8000_0000, 4096, 800, 600, 12, 1, RGB_888),
            // Zero-width colour channel.
            framebuffer_payload(0x8000_0000, 4096, 800, 600, 32, 1, [16, 0, 8, 8, 0, 8]),
            // Red field runs past the end of the pixel.
            framebuffer_payload(0x8000_0000, 4096, 800, 600, 32, 1, [28, 8, 8, 8, 0, 8]),
        ];
        for tag in cases {
            let info = parse_multiboot2_info(&info_with_framebuffer(&tag), 0x1000).unwrap();
            assert!(info.framebuffer().is_none());
            assert_eq!(
                info.framebuffer_rejection(),
                Some(FramebufferRejection::Inconsistent)
            );
        }
    }

    #[test]
    fn truncated_rgb_framebuffer_is_declined_rather_than_guessed() {
        // Drop the colour layout, leaving the 24-byte fixed portion.  Channel
        // order must never be assumed: a wrong guess swaps red and blue on
        // every pixel while looking entirely plausible.
        let mut tag = framebuffer_payload(0x8000_0000, 4096, 800, 600, 32, 1, RGB_888);
        tag.truncate(24);
        let info = parse_multiboot2_info(&info_with_framebuffer(&tag), 0x1000).unwrap();
        assert!(info.framebuffer().is_none());
        assert_eq!(
            info.framebuffer_rejection(),
            Some(FramebufferRejection::Truncated)
        );
    }

    #[test]
    fn absent_framebuffer_is_reported_as_absent_not_as_a_rejection() {
        // The two outcomes need different fixes, so they must stay
        // distinguishable in the bring-up log.
        let info = parse_multiboot2_info(&valid_info(), 0x1000).unwrap();
        assert!(info.framebuffer().is_none());
        assert_eq!(info.framebuffer_rejection(), None);
    }

    #[test]
    fn zero_address_framebuffer_is_declined() {
        // Observed from the reference QEMU firmware under a headless display
        // topology: a well-formed mode description whose base was never
        // programmed.  Accepting it would home the console at physical zero,
        // over the real-mode interrupt vector table.
        let tag = framebuffer_payload(0, 3200, 800, 600, 32, 1, RGB_888);
        let info = parse_multiboot2_info(&info_with_framebuffer(&tag), 0x1000).unwrap();
        assert!(info.framebuffer().is_none());
        assert_eq!(
            info.framebuffer_rejection(),
            Some(FramebufferRejection::UnusableAddress)
        );
    }

    #[test]
    fn framebuffer_overlapping_usable_memory_is_declined() {
        // The surface is written through rather than merely read, so sharing
        // bytes with allocatable memory would corrupt whatever the allocator
        // later places there.
        let tag = framebuffer_payload(0x0001_0000, 3200, 800, 600, 32, 1, RGB_888);
        let info = parse_multiboot2_info(&info_with_framebuffer(&tag), 0x1000).unwrap();
        assert!(info.framebuffer().is_none());
        assert_eq!(
            info.framebuffer_rejection(),
            Some(FramebufferRejection::OverlapsUsableMemory)
        );
    }

    #[test]
    fn framebuffer_adjacent_to_usable_memory_is_accepted() {
        // The rule is overlap, not proximity.  A surface that begins exactly
        // where usable RAM ends is where firmware is expected to put it, and
        // rejecting it would cost the display on a correctly configured boot.
        let tag = framebuffer_payload(0x0800_0000, 3200, 800, 600, 32, 1, RGB_888);
        let info = parse_multiboot2_info(&info_with_framebuffer(&tag), 0x1000).unwrap();
        assert_eq!(info.framebuffer().map(|fb| fb.address()), Some(0x0800_0000));
        assert_eq!(info.framebuffer_rejection(), None);
    }

    #[test]
    fn first_framebuffer_tag_wins() {        let mut bytes = vec![0; 8];
        push_tag(&mut bytes, 6, &valid_mmap_tag_payload());
        push_tag(
            &mut bytes,
            8,
            &framebuffer_payload(0x8000_0000, 4096, 800, 600, 32, 1, RGB_888),
        );
        push_tag(
            &mut bytes,
            8,
            &framebuffer_payload(0x9000_0000, 8192, 1920, 1080, 32, 1, RGB_888),
        );
        push_tag(&mut bytes, 0, &[]);
        set_total_size(&mut bytes);

        let info = parse_multiboot2_info(&bytes, 0x1000).unwrap();
        let framebuffer = info.framebuffer().unwrap();
        assert_eq!(framebuffer.address(), 0x8000_0000);
        assert_eq!(framebuffer.width(), 800);
    }
}
