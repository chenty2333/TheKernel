//! Hand-rolled ACPI table discovery for the x86 platform.
//!
//! There is deliberately no ACPI crate dependency here: the platform only
//! needs two tables, both of which are located the same way, so the RSDP scan
//! and root-table walk live in one place and are shared.
//!
//! * the MADT (whose APIC IDs define the CPU topology) is consumed by
//!   [`crate::cpu`];
//! * the MCFG (whose regions define where PCI configuration space is) is
//!   consumed by this module, which publishes the region the kernel uses so
//!   the PCI bus driver can ask for it at runtime.
//!
//! Every table lives in firmware-owned physical memory that is *not* part of
//! the runtime direct map, so all of this must happen from
//! [`crate::init::InitIfImpl::init_early`], while the temporary boot page table
//! still maps physical memory.  The two things retained afterwards — the APIC
//! ID map and the ECAM region — are plain owned scalars.
//!
//! Firmware is not trusted here.  A malformed table is never fatal, because
//! there is no way to report a fault this early: discovery degrades to the
//! configured fallback and the boot log records which path was taken.

pub mod mcfg;

use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};

use crate::config::devices::{PCI_BUS_END, PCI_ECAM_BASE};
use crate::cpu::{physical_bytes, read_u32, read_u64, table_length_and_bytes};
use mcfg::ConfigRegion;

/// Which ACPI root table an MCFG lookup walked.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RootTable {
    /// The XSDT (64-bit entries), used when the RSDP declares revision 2+.
    Xsdt,
    /// The RSDT (32-bit entries), the ACPI 1.0 root table.
    Rsdt,
    /// The RSDP was found, but neither root table was usable.
    Unusable,
}

/// Where the ECAM base the kernel will use came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EcamSource {
    /// The segment-0 region of a valid MCFG table.
    Mcfg,
    /// `[devices] pci-ecam-base` from the platform configuration.
    Configured,
}

impl EcamSource {
    /// Stable spelling used in the boot log.
    pub const fn as_str(self) -> &'static str {
        match self {
            EcamSource::Mcfg => "mcfg",
            EcamSource::Configured => "configured",
        }
    }

    /// Whether firmware supplied the value.
    ///
    /// `axhal` re-exports the platform's PCI facts as a plain boolean answer
    /// because hosted builds link no platform crate and therefore cannot name
    /// this type at all.
    pub const fn discovered(self) -> bool {
        matches!(self, EcamSource::Mcfg)
    }
}

/// Why the MCFG lookup did not yield a usable region (for the boot log only).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McfgStatus {
    /// A valid MCFG was parsed and one of its regions was selected.
    Selected,
    /// No RSDP was available at all, so no ACPI table could be located.
    NoRsdp,
    /// The RSDP was located but neither the XSDT nor the RSDT could be read.
    NoRootTable,
    /// Both root tables were read and neither names an MCFG.
    Absent,
    /// An MCFG was named but its bytes failed validation.
    Malformed,
    /// An MCFG was valid but declared no region for the segment the kernel
    /// uses.
    NoSegmentZero,
}

impl McfgStatus {
    /// Stable spelling used in the boot log.
    pub const fn as_str(self) -> &'static str {
        match self {
            McfgStatus::Selected => "selected",
            McfgStatus::NoRsdp => "absent:no-rsdp",
            McfgStatus::NoRootTable => "absent:no-root-table",
            McfgStatus::Absent => "absent:no-mcfg-table",
            McfgStatus::Malformed => "present:malformed",
            McfgStatus::NoSegmentZero => "present:no-segment-0",
        }
    }
}

/// The decision taken at boot: which ECAM base the kernel uses and why.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciEcam {
    /// Physical base address of the memory-mapped configuration window.
    pub base: u64,
    /// PCI segment group the window belongs to.  The kernel's `PciRoot`
    /// addresses segment 0 only, so this is 0 for every accepted region.
    pub segment_group: u16,
    /// First PCI bus the window describes, inclusive.
    pub bus_begin: u8,
    /// Last PCI bus the window describes, inclusive.
    pub bus_end: u8,
    /// Whether the window was discovered or configured.
    pub source: EcamSource,
}

impl PciEcam {
    /// The configured fallback, used verbatim when firmware says nothing.
    pub const fn configured() -> Self {
        Self {
            base: PCI_ECAM_BASE as u64,
            segment_group: 0,
            bus_begin: 0,
            bus_end: PCI_BUS_END as u8,
            source: EcamSource::Configured,
        }
    }
}

/// Select the ECAM region the kernel should use.
///
/// Selection is deterministic and deliberately narrow:
///
/// 1. only **segment group 0** is eligible, because the PCI root this kernel
///    builds is a CAM/ECAM walk of one segment starting at bus 0 and has no
///    notion of a segment number in a bus-device-function address;
/// 2. among those, the **first** region in table order wins.  ACPI gives no
///    precedence rule, so "first" is the only choice that does not invent one;
///    firmware lists the region it wants used first, and a machine with two
///    segment-0 regions (a firmware bug) must still boot deterministically;
/// 3. a region that starts above bus 0 is still accepted, but the kernel's
///    bus scan then begins at bus 0, outside the described window.  Such a
///    region is rejected instead, because the scan would read configuration
///    space that firmware did not describe.
///
/// Returns `None` when no region is eligible, which makes the caller keep the
/// configured base.
pub fn select_pci_ecam(regions: &[ConfigRegion]) -> Option<PciEcam> {
    let region = regions
        .iter()
        .find(|region| region.segment_group == 0 && region.start_bus == 0)?;
    Some(PciEcam {
        base: region.base_address,
        segment_group: region.segment_group,
        bus_begin: region.start_bus,
        bus_end: region.end_bus,
        source: EcamSource::Mcfg,
    })
}

/// The MCFG lookup outcome, with everything the boot log needs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Discovery {
    /// What was found and what was decided.
    pub status: McfgStatus,
    /// Which root table the lookup walked.
    pub root: RootTable,
    /// Number of regions the MCFG declared (before selection).
    pub region_count: usize,
    /// The bus range of the region that was *not* selected first, if any.
    ///
    /// Reported so a machine with several regions is diagnosable from one
    /// boot: `first_other` is the bus range of the first rejected region.
    pub first_other: Option<(u16, u8, u8)>,
    /// The region the kernel will use, if firmware supplied one.
    pub selected: Option<ConfigRegion>,
}

impl Discovery {
    const fn unavailable(status: McfgStatus, root: RootTable) -> Self {
        Self {
            status,
            root,
            region_count: 0,
            first_other: None,
            selected: None,
        }
    }
}

/// Outcome of looking for the MCFG table.
enum McfgLookup {
    /// The table's bytes and the root table that named it.
    Found {
        bytes: &'static [u8],
        root: RootTable,
    },
    /// No usable MCFG bytes, with the reason.
    Unavailable { status: McfgStatus, root: RootTable },
}

/// The ECAM decision, published by [`init_early`] and read afterwards.
static ECAM_BASE: AtomicU64 = AtomicU64::new(0);
static ECAM_SEGMENT: AtomicU32 = AtomicU32::new(0);
static ECAM_BUS_BEGIN: AtomicU32 = AtomicU32::new(0);
static ECAM_BUS_END: AtomicU32 = AtomicU32::new(0);
/// 0 = not published, 1 = discovered from MCFG, 2 = configured fallback.
static ECAM_SOURCE: AtomicU8 = AtomicU8::new(0);

const SOURCE_UNPUBLISHED: u8 = 0;
const SOURCE_DISCOVERED: u8 = 1;
const SOURCE_CONFIGURED: u8 = 2;

/// Outcome of the boot-time ACPI lookup, retained for the later `info!` line.
static DISCOVERY_STATUS: AtomicU8 = AtomicU8::new(0);
static DISCOVERY_ROOT: AtomicU8 = AtomicU8::new(0);
static DISCOVERY_REGIONS: AtomicU32 = AtomicU32::new(0);
static DISCOVERY_FIRST_OTHER: AtomicU64 = AtomicU64::new(u64::MAX);

/// Physical base address of the memory-mapped PCI configuration window the
/// kernel uses.
///
/// Before [`init_early`] has run this reports the configured value, so a
/// caller that asks too early gets the documented fallback rather than
/// address 0.
pub fn pci_ecam_base() -> usize {
    match ECAM_SOURCE.load(Ordering::Acquire) {
        SOURCE_DISCOVERED | SOURCE_CONFIGURED => ECAM_BASE.load(Ordering::Relaxed) as usize,
        _ => PCI_ECAM_BASE,
    }
}

/// Whether the published ECAM base came from firmware or configuration.
///
/// Returns the configured fallback marker before [`init_early`] has run.
pub fn ecam_source() -> EcamSource {
    match ECAM_SOURCE.load(Ordering::Acquire) {
        SOURCE_DISCOVERED => EcamSource::Mcfg,
        _ => EcamSource::Configured,
    }
}

/// Segment group of the published ECAM window.
pub fn pci_ecam_segment() -> u16 {
    ECAM_SEGMENT.load(Ordering::Relaxed) as u16
}

/// Bus range of the published ECAM window, inclusive.
pub fn pci_ecam_bus_range() -> (u8, u8) {
    (
        ECAM_BUS_BEGIN.load(Ordering::Relaxed) as u8,
        ECAM_BUS_END.load(Ordering::Relaxed) as u8,
    )
}

/// The two root tables an RSDP can name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RootTables {
    /// ACPI revision; the XSDT is only usable from revision 2 on.
    pub revision: u8,
    /// Physical address of the XSDT, or 0 when the RSDP has none.
    pub xsdt: u64,
    /// Physical address of the RSDT, or 0 when the RSDP has none.
    pub rsdt: u64,
}

/// Read the root-table pointers out of an RSDP image.
///
/// Shared by MADT and MCFG discovery so a change to how the RSDP is decoded
/// cannot apply to one table and not the other.  The caller supplies at least
/// the 36 bytes an ACPI 2.0 RSDP occupies; a shorter buffer yields zeroed
/// pointers rather than a panic.
pub(crate) fn root_tables(rsdp: &[u8]) -> RootTables {
    RootTables {
        revision: rsdp.get(15).copied().unwrap_or(0),
        xsdt: read_u64(rsdp, 24).unwrap_or(0),
        rsdt: read_u32(rsdp, 16).map(u64::from).unwrap_or(0),
    }
}

/// Locate the MCFG table through the same RSDP path the MADT uses, then
/// publish the ECAM region the kernel will use.
///
/// Must be called from the primary CPU's early initialization, while the
/// temporary boot page table maps firmware memory.  Never panics: an absent or
/// malformed table leaves the configured fallback in place.
pub(crate) fn init_early() {
    let (discovery, chosen) = match find_mcfg() {
        McfgLookup::Found { bytes, root } => match mcfg::parse_regions(bytes) {
            Ok(regions) => {
                let selected = select_pci_ecam(&regions);
                let discovery = Discovery {
                    status: if selected.is_some() {
                        McfgStatus::Selected
                    } else {
                        McfgStatus::NoSegmentZero
                    },
                    root,
                    region_count: regions.len(),
                    // The first region the kernel did *not* choose, so one
                    // boot is enough to see what else firmware offered.
                    first_other: regions
                        .iter()
                        .find(|region| match selected {
                            Some(ecam) => {
                                region.segment_group != ecam.segment_group
                                    || region.start_bus != ecam.bus_begin
                            }
                            None => true,
                        })
                        .map(|region| (region.segment_group, region.start_bus, region.end_bus)),
                    selected: selected.map(|ecam| ConfigRegion {
                        base_address: ecam.base,
                        segment_group: ecam.segment_group,
                        start_bus: ecam.bus_begin,
                        end_bus: ecam.bus_end,
                    }),
                };
                (discovery, selected.unwrap_or_else(PciEcam::configured))
            }
            Err(_error) => {
                let discovery = Discovery::unavailable(McfgStatus::Malformed, root);
                (discovery, PciEcam::configured())
            }
        },
        McfgLookup::Unavailable { status, root } => {
            let discovery = Discovery::unavailable(status, root);
            (discovery, PciEcam::configured())
        }
    };

    publish(&chosen);
    record(&discovery);
}

/// Write the decision into the published statics.
fn publish(chosen: &PciEcam) {
    ECAM_BASE.store(chosen.base, Ordering::Relaxed);
    ECAM_SEGMENT.store(chosen.segment_group as u32, Ordering::Relaxed);
    ECAM_BUS_BEGIN.store(chosen.bus_begin as u32, Ordering::Relaxed);
    ECAM_BUS_END.store(chosen.bus_end as u32, Ordering::Relaxed);
    ECAM_SOURCE.store(
        match chosen.source {
            EcamSource::Mcfg => SOURCE_DISCOVERED,
            EcamSource::Configured => SOURCE_CONFIGURED,
        },
        Ordering::Release,
    );
}

/// Retain the lookup outcome for the later, logging-enabled report.
fn record(discovery: &Discovery) {
    DISCOVERY_STATUS.store(
        match discovery.status {
            McfgStatus::Selected => 1,
            McfgStatus::NoRsdp => 2,
            McfgStatus::NoRootTable => 3,
            McfgStatus::Absent => 4,
            McfgStatus::Malformed => 5,
            McfgStatus::NoSegmentZero => 6,
        },
        Ordering::Relaxed,
    );
    DISCOVERY_ROOT.store(
        match discovery.root {
            RootTable::Xsdt => 1,
            RootTable::Rsdt => 2,
            RootTable::Unusable => 0,
        },
        Ordering::Relaxed,
    );
    DISCOVERY_REGIONS.store(discovery.region_count as u32, Ordering::Relaxed);
    DISCOVERY_FIRST_OTHER.store(
        match discovery.first_other {
            Some((segment, begin, end)) => {
                (segment as u64) << 16 | (begin as u64) << 8 | end as u64
            }
            None => u64::MAX,
        },
        Ordering::Relaxed,
    );
}

/// Report the ECAM decision at `info!`.
///
/// Called from `init_later`, after the logger exists and after the runtime page
/// table has replaced the temporary boot mapping, so the physical address
/// printed here is the address the PCI bus driver will actually use.
pub(crate) fn report() {
    let status = match DISCOVERY_STATUS.load(Ordering::Relaxed) {
        1 => McfgStatus::Selected,
        2 => McfgStatus::NoRsdp,
        3 => McfgStatus::NoRootTable,
        4 => McfgStatus::Absent,
        5 => McfgStatus::Malformed,
        6 => McfgStatus::NoSegmentZero,
        _ => McfgStatus::NoRsdp,
    };
    let root = match DISCOVERY_ROOT.load(Ordering::Relaxed) {
        1 => RootTable::Xsdt,
        2 => RootTable::Rsdt,
        _ => RootTable::Unusable,
    };
    let root = match root {
        RootTable::Xsdt => "xsdt",
        RootTable::Rsdt => "rsdt",
        RootTable::Unusable => "none",
    };
    let first_other = match DISCOVERY_FIRST_OTHER.load(Ordering::Relaxed) {
        u64::MAX => None,
        packed => Some((
            (packed >> 16) as u16,
            ((packed >> 8) & 0xff) as u8,
            (packed & 0xff) as u8,
        )),
    };
    let (bus_begin, bus_end) = pci_ecam_bus_range();

    info!(
        "pci-ecam: base={:#x} segment={} bus={:#04x}-{:#04x} source={} \
         mcfg={} root={} regions={}",
        pci_ecam_base(),
        pci_ecam_segment(),
        bus_begin,
        bus_end,
        ecam_source().as_str(),
        status.as_str(),
        root,
        DISCOVERY_REGIONS.load(Ordering::Relaxed),
    );
    if let Some((segment, begin, end)) = first_other {
        info!(
            "pci-ecam: first unused MCFG region segment={segment} bus={begin:#04x}-{end:#04x} \
             (segment 0 starting at bus 0 is preferred)"
        );
    }
    if ecam_source() == EcamSource::Configured {
        info!(
            "pci-ecam: {:#x} is the configured [devices] pci-ecam-base fallback, not a \
             firmware discovery",
            pci_ecam_base()
        );
    }
}

/// Locate the MCFG table bytes, reusing the RSDP path that also feeds MADT
/// discovery.
///
/// The owned Multiboot2 RSDP is preferred exactly as [`crate::cpu`] prefers
/// it: a failure to find the table through it must not silently switch to an
/// unrelated legacy scan.
fn find_mcfg() -> McfgLookup {
    if let Some(rsdp) = crate::boot_info::get().rsdp() {
        return mcfg_from_rsdp(rsdp.bytes());
    }

    let Some(rsdp_address) = crate::cpu::find_rsdp() else {
        return McfgLookup::Unavailable {
            status: McfgStatus::NoRsdp,
            root: RootTable::Unusable,
        };
    };
    let Some(rsdp) = (unsafe { physical_bytes(rsdp_address, 36) }) else {
        return McfgLookup::Unavailable {
            status: McfgStatus::NoRsdp,
            root: RootTable::Unusable,
        };
    };
    mcfg_from_rsdp(rsdp)
}

/// Walk the root table the RSDP names and return the MCFG's bytes.
fn mcfg_from_rsdp(rsdp: &[u8]) -> McfgLookup {
    let RootTables {
        revision,
        xsdt: xsdt_address,
        rsdt: rsdt_address,
    } = root_tables(rsdp);

    // ACPI 2.0+ prefers the XSDT.  Fall back to the RSDT when the XSDT is
    // absent or unusable, exactly as MADT discovery does: a machine with a
    // revision-2 RSDP and no usable XSDT still has a usable RSDT.
    let mut saw_readable_root = false;
    if revision >= 2
        && xsdt_address != 0
        && let Some(result) = check_root(xsdt_address, 8, RootTable::Xsdt, &mut saw_readable_root)
    {
        return result;
    }
    if rsdt_address != 0
        && let Some(result) = check_root(rsdt_address, 4, RootTable::Rsdt, &mut saw_readable_root)
    {
        return result;
    }

    McfgLookup::Unavailable {
        status: if saw_readable_root {
            McfgStatus::Absent
        } else {
            McfgStatus::NoRootTable
        },
        root: RootTable::Unusable,
    }
}

/// Consult one root table, returning `Some` only when it settles the lookup.
///
/// `Ok(None)` from the walk means "this root is fine but names no MCFG", which
/// is not an answer on its own: the other root table may still name one.
fn check_root(
    root_address: u64,
    entry_width: usize,
    root: RootTable,
    saw_readable_root: &mut bool,
) -> Option<McfgLookup> {
    match find_in_root(root_address, entry_width) {
        RootWalk::Found(address) => Some(match table_length_and_bytes(address) {
            Some((_, table)) => McfgLookup::Found { bytes: table, root },
            // The MCFG's own checksum or length is bad.  Say "present but
            // malformed" rather than "absent": the two need different fixes.
            None => McfgLookup::Unavailable {
                status: McfgStatus::Malformed,
                root,
            },
        }),
        RootWalk::NoTable => {
            *saw_readable_root = true;
            None
        }
        RootWalk::Unreadable => None,
    }
}

/// What walking one root table produced.
enum RootWalk {
    /// The root names an MCFG at this physical address.
    Found(u64),
    /// The root is valid but names no MCFG.
    NoTable,
    /// The root itself could not be read.
    Unreadable,
}

/// Search one root table for the MCFG signature.
fn find_in_root(root_address: u64, entry_width: usize) -> RootWalk {
    if entry_width != 4 && entry_width != 8 {
        return RootWalk::Unreadable;
    }
    let Some((root_length, root)) = table_length_and_bytes(root_address) else {
        return RootWalk::Unreadable;
    };
    if root_length < 36 {
        return RootWalk::Unreadable;
    }
    let mut offset: usize = 36;
    while offset + entry_width <= root_length {
        let table_address = if entry_width == 4 {
            read_u32(root, offset).map(u64::from)
        } else {
            read_u64(root, offset)
        };
        // A single entry that cannot be read is skipped, not fatal: the root
        // table is still a usable index and a later entry may be the MCFG.
        if let Some(table_address) = table_address
            && let Some((_, table)) = table_length_and_bytes(table_address)
            && &table[..4] == b"MCFG"
        {
            return RootWalk::Found(table_address);
        }
        offset += entry_width;
    }
    RootWalk::NoTable
}

#[cfg(test)]
mod tests {
    use super::mcfg::ConfigRegion;
    use super::{EcamSource, McfgStatus, PciEcam, RootTable, select_pci_ecam};

    fn region(base: u64, segment: u16, start: u8, end: u8) -> ConfigRegion {
        ConfigRegion {
            base_address: base,
            segment_group: segment,
            start_bus: start,
            end_bus: end,
        }
    }

    #[test]
    fn segment_zero_region_is_selected_over_other_segments() {
        let regions = [
            region(0x8000_0000, 1, 0, 0xff),
            region(0xe000_0000, 0, 0, 0xff),
            region(0x9000_0000, 2, 0, 0xff),
        ];
        let selected = select_pci_ecam(&regions).expect("segment 0 is present");
        assert_eq!(selected.base, 0xe000_0000);
        assert_eq!(selected.segment_group, 0);
        assert_eq!(selected.bus_begin, 0);
        assert_eq!(selected.bus_end, 0xff);
        assert_eq!(selected.source, EcamSource::Mcfg);
    }

    #[test]
    fn the_first_segment_zero_region_wins() {
        // Two segment-0 regions are a firmware bug.  The kernel must still
        // pick the same one on every boot, so it takes table order.
        let regions = [
            region(0xe000_0000, 0, 0, 0x7f),
            region(0xf000_0000, 0, 0, 0xff),
        ];
        let selected = select_pci_ecam(&regions).expect("segment 0 is present");
        assert_eq!(selected.base, 0xe000_0000);
        assert_eq!(selected.bus_end, 0x7f);
    }

    #[test]
    fn a_bus_range_starting_above_zero_is_rejected() {
        // This is the "config space outside the region it describes" case:
        // the kernel scans from bus 0, which this window does not cover.
        let regions = [region(0xe000_0000, 0, 0x10, 0xff)];
        assert_eq!(select_pci_ecam(&regions), None);
    }

    #[test]
    fn a_table_with_only_other_segments_yields_no_discovery() {
        let regions = [region(0x8000_0000, 1, 0, 0xff), region(0x9000_0000, 3, 0, 0x3f)];
        assert_eq!(select_pci_ecam(&regions), None);
    }

    #[test]
    fn an_empty_region_list_yields_no_discovery() {
        assert_eq!(select_pci_ecam(&[]), None);
    }

    #[test]
    fn fallback_reports_the_configured_base_and_says_so() {
        // The platform crate reaches its configuration through the generated
        // `crate::config` module, not through an `axconfig` dependency.
        let fallback = PciEcam::configured();
        assert_eq!(fallback.base, crate::config::devices::PCI_ECAM_BASE as u64);
        assert_eq!(
            fallback.bus_end,
            crate::config::devices::PCI_BUS_END as u8
        );
        assert_eq!(fallback.source, EcamSource::Configured);
        // The published accessors must agree with the parsed fallback before
        // early initialization has run.
        assert_eq!(
            super::pci_ecam_base(),
            crate::config::devices::PCI_ECAM_BASE
        );
        assert_eq!(super::ecam_source(), EcamSource::Configured);
        assert!(!super::ecam_source().discovered());
        assert!(EcamSource::Mcfg.discovered());
    }

    #[test]
    fn a_single_bus_region_still_selects_when_it_covers_bus_zero() {
        let regions = [region(0xfe00_0000, 0, 0, 0x00)];
        let selected = select_pci_ecam(&regions).expect("bus 0 is covered");
        assert_eq!(selected.bus_begin, 0);
        assert_eq!(selected.bus_end, 0);
    }

    #[test]
    fn source_and_status_spellings_are_stable() {
        // These strings are part of the boot log contract used to tell a
        // discovery from a fallback.
        assert_eq!(EcamSource::Mcfg.as_str(), "mcfg");
        assert_eq!(EcamSource::Configured.as_str(), "configured");
        assert_eq!(McfgStatus::Selected.as_str(), "selected");
        assert_eq!(McfgStatus::Absent.as_str(), "absent:no-mcfg-table");
        assert_eq!(McfgStatus::Malformed.as_str(), "present:malformed");
        assert_eq!(RootTable::Xsdt, RootTable::Xsdt);
    }
}
