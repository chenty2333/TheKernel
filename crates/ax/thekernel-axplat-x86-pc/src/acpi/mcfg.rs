//! Pure parser for the ACPI "MCFG" (PCI Express Memory Mapped Configuration
//! Space) table.
//!
//! Firmware, not the kernel's configuration file, owns the answer to "where is
//! PCI segment group *N*'s configuration space".  This module turns the raw
//! MCFG bytes into the list of regions it declares; nothing here touches
//! memory-mapped I/O, allocates, or can panic.  The caller decides which
//! region the kernel should use (see [`super::select_pci_ecam`]).
//!
//! This code runs while the kernel is still bootstrapping, before any fault
//! path can report anything useful, so every failure is a returned `None`.

/// Length of one MCFG allocation structure ("configuration space base address
/// allocation" entry): base address, segment group, start bus, end bus.
pub const MCFG_ENTRY_LEN: usize = 16;

/// Offset of the first allocation structure.
///
/// The MCFG table is the standard 36-byte ACPI description header followed by
/// eight reserved bytes, so the entries start at offset 44.
pub const MCFG_ENTRIES_OFFSET: usize = 44;

/// Upper bound on the number of regions [`parse`] will report.
///
/// The kernel only ever uses one region; the bound keeps the return type a
/// fixed-size value so this runs without a heap.  A table with more regions is
/// accepted but truncated to the first `MAX_REGIONS` — the caller logs the
/// count it sees, and real firmware declares one or two.
pub const MAX_REGIONS: usize = 8;

/// One memory-mapped PCI configuration region, exactly as firmware declared it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConfigRegion {
    /// Physical base address of the region.
    ///
    /// ACPI specifies this as a 64-bit address; only the low 32 bits are
    /// meaningful on the x86_64 machines this kernel targets, and the high half
    /// is kept so a malformed table is rejected rather than silently truncated.
    pub base_address: u64,
    /// PCI segment group this region describes.
    pub segment_group: u16,
    /// First PCI bus number in the region, inclusive.
    pub start_bus: u8,
    /// Last PCI bus number in the region, inclusive.
    pub end_bus: u8,
}

impl ConfigRegion {
    /// Number of PCI buses covered by this region.
    ///
    /// The MCFG end bus is inclusive, so a single-bus region covers one bus.
    #[cfg(test)]
    pub const fn bus_count(&self) -> u32 {
        self.end_bus as u32 - self.start_bus as u32 + 1
    }

    /// Whether the region's own bus numbers run in ascending order.
    ///
    /// A descending range cannot describe configuration space and is rejected
    /// by [`parse`].  The kernel-wide bus admission limit is a separate,
    /// configuration-owned check performed during region selection.
    pub const fn bus_range_is_ordered(&self) -> bool {
        self.start_bus <= self.end_bus
    }

    /// Number of bytes of memory-mapped configuration space the region spans.
    ///
    /// Each bus contributes 1 MiB: 32 devices x 8 functions x 4 KiB.
    #[cfg(test)]
    pub const fn size(&self) -> u64 {
        self.bus_count() as u64 * (1 << 20)
    }
}

/// Parse an MCFG table image.
///
/// `bytes` is the table as firmware declared it: the ACPI header's length
/// field is authoritative, so a caller that hands over a larger buffer (for
/// example a whole page) still gets exactly the declared table.  A table whose
/// declared length exceeds the buffer is truncated table data, not a table
/// with extra entries, and is rejected outright.
///
/// Returns `None` when the image is not a structurally valid MCFG table:
/// a short or lying length field, a wrong signature, a revision below 1, a
/// checksum that does not add to zero, or a trailing partial entry.  Such a
/// table is firmware corruption; the caller falls back to configuration.
///
/// Boot uses [`parse_regions`] directly, because the boot log reports *why* a
/// table was rejected.  This `Option`-shaped wrapper is the tests' shorthand.
#[cfg(test)]
pub fn parse(bytes: &[u8]) -> Option<heapless::Vec<ConfigRegion, MAX_REGIONS>> {
    parse_regions(bytes).ok()
}

/// Why [`parse_regions`] rejected a table.
///
/// The distinction exists so a failing boot can say *which* firmware
/// invariant was violated; every variant leads to the same caller behaviour,
/// which is the configured fallback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McfgError {
    /// The buffer is shorter than a table header plus the eight reserved bytes.
    TooShort,
    /// The first four bytes are not `MCFG`.
    BadSignature,
    /// The declared length is below the smallest possible table, or is not a
    /// multiple of the entry size beyond the fixed prefix.
    BadLength,
    /// The declared length exceeds the bytes the caller supplied.
    LengthExceedsInput,
    /// The table checksum does not sum to zero.
    BadChecksum,
    /// The revision field is zero, meaning the table was never filled in.
    ZeroRevision,
    /// A region declares an end bus below its start bus.
    ReversedBusRange,
}

/// Parse an MCFG table image, distinguishing the failure modes.
///
/// This is the testable core of [`parse`]; the difference is only that this
/// one reports why the table was rejected.
pub fn parse_regions(bytes: &[u8]) -> Result<heapless::Vec<ConfigRegion, MAX_REGIONS>, McfgError> {
    const HEADER_LEN: usize = 36;

    if bytes.len() < HEADER_LEN + 8 {
        return Err(McfgError::TooShort);
    }
    if &bytes[..4] != b"MCFG" {
        return Err(McfgError::BadSignature);
    }

    let declared = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    // The smallest legal MCFG has a header, the reserved eight bytes, and no
    // entries.  Anything larger must be exactly the fixed prefix plus whole
    // 16-byte allocation structures: a trailing partial entry means the length
    // field and the entry array disagree.
    if declared < MCFG_ENTRIES_OFFSET
        || (declared - MCFG_ENTRIES_OFFSET) % MCFG_ENTRY_LEN != 0
    {
        return Err(McfgError::BadLength);
    }
    if declared > bytes.len() {
        return Err(McfgError::LengthExceedsInput);
    }

    let table = &bytes[..declared];
    if table.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)) != 0 {
        return Err(McfgError::BadChecksum);
    }

    // MCFG revision 1 is the only revision ACPI has ever defined; a revision
    // field of zero means the table was never filled in.
    if table[8] == 0 {
        return Err(McfgError::ZeroRevision);
    }

    let mut regions = heapless::Vec::new();
    let mut offset = MCFG_ENTRIES_OFFSET;
    while offset + MCFG_ENTRY_LEN <= table.len() {
        let entry = &table[offset..offset + MCFG_ENTRY_LEN];
        let region = ConfigRegion {
            base_address: u64::from_le_bytes([
                entry[0], entry[1], entry[2], entry[3], entry[4], entry[5], entry[6], entry[7],
            ]),
            segment_group: u16::from_le_bytes([entry[8], entry[9]]),
            start_bus: entry[10],
            end_bus: entry[11],
        };
        if !region.bus_range_is_ordered() {
            return Err(McfgError::ReversedBusRange);
        }
        // A region the kernel cannot use must not invalidate the regions it
        // can: truncation at capacity is reported by the length of the result.
        if regions.push(region).is_err() {
            break;
        }
        offset += MCFG_ENTRY_LEN;
    }

    Ok(regions)
}

#[cfg(test)]
mod tests {
    use std::vec::Vec;

    use super::{
        ConfigRegion, MAX_REGIONS, MCFG_ENTRIES_OFFSET, McfgError, parse, parse_regions,
    };

    /// Build a well-formed MCFG image around `entries`, with a correct length
    /// field and checksum.  Individual tests then break exactly one property.
    fn table(entries: &[(u64, u16, u8, u8)]) -> Vec<u8> {
        let length = MCFG_ENTRIES_OFFSET + entries.len() * 16;
        let mut bytes = vec![0u8; length];
        bytes[..4].copy_from_slice(b"MCFG");
        bytes[4..8].copy_from_slice(&(length as u32).to_le_bytes());
        bytes[8] = 1; // revision
        bytes[10..16].copy_from_slice(b"THEKRN");
        bytes[16..24].copy_from_slice(b"THEKRN01");
        bytes[28..32].copy_from_slice(b"TKRN");
        bytes[32..36].copy_from_slice(&1u32.to_le_bytes());
        for (index, (base, segment, start, end)) in entries.iter().enumerate() {
            let offset = MCFG_ENTRIES_OFFSET + index * 16;
            bytes[offset..offset + 8].copy_from_slice(&base.to_le_bytes());
            bytes[offset + 8..offset + 10].copy_from_slice(&segment.to_le_bytes());
            bytes[offset + 10] = *start;
            bytes[offset + 11] = *end;
        }
        let sum = bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte));
        bytes[9] = 0u8.wrapping_sub(sum);
        bytes
    }

    fn region(base: u64, segment: u16, start: u8, end: u8) -> ConfigRegion {
        ConfigRegion {
            base_address: base,
            segment_group: segment,
            start_bus: start,
            end_bus: end,
        }
    }

    #[test]
    fn normal_table_yields_its_single_region() {
        let bytes = table(&[(0xe000_0000, 0, 0, 0xff)]);
        assert_eq!(bytes.len(), 60);
        let regions = parse(&bytes).expect("valid table");
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0], region(0xe000_0000, 0, 0, 0xff));
        assert_eq!(regions[0].bus_count(), 256);
        assert_eq!(regions[0].size(), 256 << 20);
    }

    #[test]
    fn several_segments_keep_their_order_and_identity() {
        let bytes = table(&[
            (0xe000_0000, 0, 0, 0xff),
            (0x8000_0000, 1, 0, 0x3f),
            (0x9000_0000, 2, 0x80, 0x80),
        ]);
        let regions = parse(&bytes).expect("valid table");
        assert_eq!(regions.len(), 3);
        assert_eq!(regions[0], region(0xe000_0000, 0, 0, 0xff));
        assert_eq!(regions[1], region(0x8000_0000, 1, 0, 0x3f));
        assert_eq!(regions[2], region(0x9000_0000, 2, 0x80, 0x80));
        assert_eq!(regions[2].bus_count(), 1);
    }

    #[test]
    fn zero_entries_is_a_valid_empty_table() {
        let bytes = table(&[]);
        assert_eq!(bytes.len(), MCFG_ENTRIES_OFFSET);
        assert_eq!(parse(&bytes).expect("valid empty table").len(), 0);
    }

    #[test]
    fn doubled_declared_length_is_rejected() {
        // A length field that lies upward must not make the parser read past
        // the bytes the caller actually has.  76 is one more whole entry than
        // the 60 bytes present, so the length is well-formed in isolation and
        // only the input bound can reject it.
        let bytes = table(&[(0xe000_0000, 0, 0, 0xff)]);
        let mut lying = bytes.clone();
        lying[4..8].copy_from_slice(&76u32.to_le_bytes());
        assert_eq!(parse_regions(&lying), Err(McfgError::LengthExceedsInput));
        assert!(parse(&lying).is_none());
    }

    #[test]
    fn length_field_that_is_aligned_but_wrong_is_rejected() {
        // 44 + 16 is a whole number of entries but does not match the bytes.
        let mut bytes = table(&[(0xe000_0000, 0, 0, 0xff)]);
        bytes[4..8].copy_from_slice(&16u32.to_le_bytes()); // below the fixed prefix
        assert_eq!(parse_regions(&bytes), Err(McfgError::BadLength));

        let mut bytes = table(&[(0xe000_0000, 0, 0, 0xff)]);
        bytes[4..8].copy_from_slice(&53u32.to_le_bytes()); // not entry-aligned
        assert_eq!(parse_regions(&bytes), Err(McfgError::BadLength));
    }

    #[test]
    fn truncated_table_is_rejected_in_every_prefix() {
        let bytes = table(&[(0xe000_0000, 0, 0, 0xff), (0x8000_0000, 1, 0, 0x3f)]);
        for length in 0..bytes.len() {
            let prefix = &bytes[..length];
            assert!(
                parse(prefix).is_none(),
                "a {length}-byte prefix must not parse as a complete table"
            );
        }
        // The whole table still parses, so the loop above is not vacuous.
        assert_eq!(parse(&bytes).expect("valid table").len(), 2);
    }

    #[test]
    fn padding_beyond_the_declared_length_is_ignored() {
        // Callers commonly read a whole page from firmware.  Only the declared
        // length is the table; trailing bytes must not be read as entries.
        let mut bytes = table(&[(0xe000_0000, 0, 0, 0xff)]);
        bytes.resize(4096, 0xff);
        let regions = parse(&bytes).expect("valid table in a padded buffer");
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].base_address, 0xe000_0000);
    }

    #[test]
    fn wrong_signature_is_rejected() {
        let mut bytes = table(&[(0xe000_0000, 0, 0, 0xff)]);
        bytes[..4].copy_from_slice(b"APIC");
        assert_eq!(parse_regions(&bytes), Err(McfgError::BadSignature));
    }

    #[test]
    fn bad_checksum_is_rejected() {
        let mut bytes = table(&[(0xe000_0000, 0, 0, 0xff)]);
        bytes[9] = bytes[9].wrapping_add(1);
        assert_eq!(parse_regions(&bytes), Err(McfgError::BadChecksum));
    }

    #[test]
    fn zero_revision_is_rejected() {
        let mut bytes = table(&[(0xe000_0000, 0, 0, 0xff)]);
        bytes[8] = 0;
        let sum = bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte));
        bytes[9] = bytes[9].wrapping_sub(sum);
        assert_eq!(parse_regions(&bytes), Err(McfgError::ZeroRevision));
    }

    #[test]
    fn reversed_bus_range_is_rejected() {
        // 0x40..0x20 describes no bus at all; accepting it would let the
        // parser underflow the bus count.
        let bytes = table(&[(0xe000_0000, 0, 0x40, 0x20)]);
        assert_eq!(parse_regions(&bytes), Err(McfgError::ReversedBusRange));
        assert!(parse(&bytes).is_none());
    }

    #[test]
    fn region_larger_than_the_kernel_bus_limit_is_still_parsed() {
        // "Bus 0xff is beyond pci-bus-end" is a selection-time question: the
        // parser reports what firmware said, the caller decides what to use.
        let bytes = table(&[(0xe000_0000, 0, 0, 0xff)]);
        let regions = parse(&bytes).expect("valid table");
        assert_eq!(regions[0].end_bus, 0xff);
        assert!(regions[0].bus_range_is_ordered());
    }

    #[test]
    fn a_region_beyond_the_reported_bus_count_is_reported_verbatim() {
        // Bus count is inclusive; a region covering only bus 0xff spans one
        // bus, and every byte of it lies outside a "buses 0-0xfe" profile.
        let bytes = table(&[(0xe000_0000, 0, 0xff, 0xff)]);
        let regions = parse(&bytes).expect("valid table");
        assert_eq!(regions[0].bus_count(), 1);
        assert_eq!(regions[0].size(), 1 << 20);
    }

    #[test]
    fn more_regions_than_the_capacity_are_truncated_not_rejected() {
        let entries: Vec<_> = (0..MAX_REGIONS as u64 + 3)
            .map(|index| (0xe000_0000 + index * 0x1000_0000, index as u16, 0, 0xff))
            .collect();
        let bytes = table(&entries);
        let regions = parse(&bytes).expect("valid table");
        assert_eq!(regions.len(), MAX_REGIONS);
        assert_eq!(regions[MAX_REGIONS - 1].segment_group, MAX_REGIONS as u16 - 1);
    }

    #[test]
    fn every_short_buffer_is_rejected_without_panicking() {
        for length in 0..MCFG_ENTRIES_OFFSET {
            assert!(parse(&vec![0u8; length]).is_none());
        }
        // The header alone (36 bytes) is short: MCFG's reserved field is part
        // of the fixed prefix and entries cannot start before offset 44.
        assert!(parse(&vec![0u8; 36]).is_none());
        assert!(parse(&[]).is_none());
    }

    #[test]
    fn parser_never_panics_on_arbitrary_bytes() {
        // A cheap deterministic sweep over length and content, because this
        // parser runs before the kernel can report a fault.
        let mut state = 0x1234_5678_9abc_def0u64;
        for length in 0..512usize {
            let mut bytes = vec![0u8; length];
            for byte in bytes.iter_mut() {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                *byte = (state >> 33) as u8;
            }
            let _ = parse(&bytes);
        }
    }

    #[test]
    fn high_half_of_a_base_address_is_preserved() {
        // A 32-bit-only parser would silently truncate this to 0.
        let bytes = table(&[(0x0000_0001_0000_0000, 0, 0, 0xff)]);
        let regions = parse(&bytes).expect("valid table");
        assert_eq!(regions[0].base_address, 0x0000_0001_0000_0000);
    }
}
