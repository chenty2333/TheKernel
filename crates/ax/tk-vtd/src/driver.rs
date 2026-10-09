//! ACPI DMAR iteration, device-scope matching and driver lifecycle adapters
//! translated from FreeBSD sys/x86/iommu/intel_drv.c (snapshot c2b7fe4,
//! BSD-2-Clause). Copyright (c) 2013-2015 The FreeBSD Foundation; Konstantin
//! Belousov under Foundation sponsorship. ACPICA/PCI bus and device methods are
//! mapped to TheKernel's ACPI/PCI APIs. Retained license in LICENSES/.
use alloc::{string::String, vec::Vec};

use crate::{
    DmarTable, Error, OwnedScope, ReservedRegion, Unit,
    context::ContextInvalidator,
    dmar::{DMAR_BARRIER_RMRR, DmarUnit},
    quirks::{NorthbridgeIdentity, dmar_quirks_pre_use},
    reg::*,
    utils::{dmar_barrier_enter, dmar_barrier_exit},
};

const TABLE_HEADER_BYTES: usize = 48;
const DRHD: u16 = 0;
const RMRR: u16 = 1;
const RHSA: u16 = 3;
const SCOPE_ENDPOINT: u8 = 1;
const SCOPE_BRIDGE: u8 = 2;
const SCOPE_IOAPIC: u8 = 3;
const SCOPE_HPET: u8 = 4;

fn get_u16(bytes: &[u8], offset: usize) -> Result<u16, Error> {
    let part = bytes.get(offset..offset + 2).ok_or(Error::InvalidTable)?;
    Ok(u16::from_le_bytes([part[0], part[1]]))
}
fn get_u32(bytes: &[u8], offset: usize) -> Result<u32, Error> {
    let part = bytes.get(offset..offset + 4).ok_or(Error::InvalidTable)?;
    Ok(u32::from_le_bytes([part[0], part[1], part[2], part[3]]))
}
fn get_u64(bytes: &[u8], offset: usize) -> Result<u64, Error> {
    let part = bytes.get(offset..offset + 8).ok_or(Error::InvalidTable)?;
    Ok(u64::from_le_bytes(
        part.try_into().map_err(|_| Error::InvalidTable)?,
    ))
}

/// Iterate bounded ACPI DMAR substructures, preserving early-stop callback semantics.
/// upstream: intel_drv.c dmar_iterate_tbl()
pub fn dmar_iterate_tbl(
    table: &[u8],
    mut visit: impl FnMut(u16, usize, &[u8]) -> Result<bool, Error>,
) -> Result<(), Error> {
    if table.len() < TABLE_HEADER_BYTES || table.get(..4) != Some(b"DMAR") {
        return Err(Error::InvalidTable);
    }
    let declared = get_u32(table, 4)? as usize;
    if declared < TABLE_HEADER_BYTES || declared > table.len() {
        return Err(Error::InvalidTable);
    }
    let mut offset = TABLE_HEADER_BYTES;
    while offset < declared {
        let kind = get_u16(table, offset)?;
        let length = usize::from(get_u16(table, offset + 2)?);
        if length < 4 || offset.checked_add(length).is_none_or(|end| end > declared) {
            return Err(Error::InvalidStructure);
        }
        if !visit(kind, offset, &table[offset..offset + length])? {
            break;
        }
        offset += length;
    }
    Ok(())
}

/// Callback shape for selecting one DRHD record from ACPI's structure iterator.
/// upstream: intel_drv.c dmar_find_iter()
pub fn dmar_find_iter(table: &[u8], mut index: usize) -> Result<Option<&[u8]>, Error> {
    let mut found_range = None;
    dmar_iterate_tbl(table, |kind, offset, record| {
        if kind != DRHD {
            return Ok(true);
        }
        if index == 0 {
            found_range = Some((offset, offset + record.len()));
            return Ok(false);
        }
        index -= 1;
        Ok(true)
    })?;
    found_range
        .map(|(start, end)| table.get(start..end).ok_or(Error::InvalidTable))
        .transpose()
}

/// Select the Nth hardware-unit record.
/// upstream: intel_drv.c dmar_find_by_index()
pub fn dmar_find_by_index(table: &[u8], index: usize) -> Result<Option<&[u8]>, Error> {
    dmar_find_iter(table, index)
}

/// Count DRHD unit records.
/// upstream: intel_drv.c dmar_count_iter()
pub fn dmar_count_iter(table: &[u8]) -> Result<usize, Error> {
    let mut count = 0usize;
    dmar_iterate_tbl(table, |kind, _, _| {
        if kind == DRHD {
            count = count.checked_add(1).ok_or(Error::InvalidRange)?;
        }
        Ok(true)
    })?;
    Ok(count)
}

/// Look up RHSA proximity affinity by the matching register base.
/// upstream: intel_drv.c dmar_rhsa_iter()
pub fn dmar_rhsa_iter(table: &[u8], register_base: u64) -> Result<Option<u32>, Error> {
    let mut domain = None;
    dmar_iterate_tbl(table, |kind, _, record| {
        if kind == RHSA && record.len() >= 20 && get_u64(record, 8)? == register_base {
            domain = Some(get_u32(record, 16)?);
        }
        Ok(true)
    })?;
    Ok(domain)
}

/// ACPI enable switch and table acquisition adapter.
/// upstream: intel_drv.c dmar_identify()
pub fn dmar_identify(table: Option<&[u8]>, enabled: bool) -> Result<Option<DmarTable>, Error> {
    if !enabled {
        return Ok(None);
    }
    let Some(bytes) = table else {
        return Ok(None);
    };
    Ok(Some(DmarTable::parse(bytes)?))
}

/// Probe policy: ACPICA owns real ACPI devices; this class is a synthetic PCI-adjacent unit.
/// upstream: intel_drv.c dmar_probe()
pub fn dmar_probe(has_acpi_device_handle: bool) -> Result<&'static str, Error> {
    if has_acpi_device_handle {
        Err(Error::NoDevice)
    } else {
        Ok("DMA remap")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DriverState {
    pub units: Vec<Unit>,
    pub running: bool,
}

/// Attach each parsed unit; failed attach unwinds already-created units in reverse order.
/// upstream: intel_drv.c dmar_attach()
pub fn dmar_attach(
    table: &DmarTable,
    mut attach_unit: impl FnMut(&Unit) -> Result<(), Error>,
    mut release_unit: impl FnMut(&Unit),
) -> Result<DriverState, Error> {
    let mut attached = Vec::new();
    attached
        .try_reserve_exact(table.units.len())
        .map_err(|_| Error::OutOfMemory)?;
    for unit in &table.units {
        if let Err(error) = attach_unit(unit) {
            for old in attached.iter().rev() {
                release_unit(old);
            }
            return Err(error);
        }
        attached.push(unit.clone());
    }
    Ok(DriverState {
        units: attached,
        running: true,
    })
}

/// Release per-unit resources in upstream teardown order, abstracting OS resource handles.
/// upstream: intel_drv.c dmar_release_resources()
pub fn dmar_release_resources(state: &mut DriverState, mut release_unit: impl FnMut(&Unit)) {
    for unit in state.units.iter().rev() {
        release_unit(unit);
    }
    state.units.clear();
    state.running = false;
}

/// Interrupt remap child callback; the x86 APIC/PCI routing details are native hooks.
/// upstream: intel_drv.c dmar_remap_intr()
pub fn dmar_remap_intr(
    irq: u32,
    registered_irqs: &[u32],
    mut remap: impl FnMut(u32) -> Result<(), Error>,
) -> Result<(), Error> {
    if registered_irqs.contains(&irq) {
        remap(irq)
    } else {
        Err(Error::NoDevice)
    }
}

/// Render key capabilities in a concise device log format.
/// upstream: intel_drv.c dmar_print_caps()
pub fn dmar_print_caps(unit: &Unit, version: u32, cap: u64, ecap: u64) -> String {
    alloc::format!(
        "regs@{:#x}, ver={}.{}, seg={}, include_all={}, cap={:#x}, ecap={:#x}, ndoms={}, \
         sagaw={:#x}, mgaw={}, fro={}, nfr={}, sps={:#x}, iro={}, qi={}",
        unit.register_base,
        DMAR_MAJOR_VER(version as u64),
        DMAR_MINOR_VER(version as u64),
        unit.segment,
        unit.include_all,
        cap,
        ecap,
        DMAR_CAP_ND(cap),
        DMAR_CAP_SAGAW(cap),
        DMAR_CAP_MGAW(cap),
        DMAR_CAP_FRO(cap),
        DMAR_CAP_NFR(cap),
        DMAR_CAP_SPS(cap),
        DMAR_ECAP_IRO(ecap),
        ecap & DMAR_ECAP_QI != 0
    )
}

/// The driver is intentionally not detachable while the active DMA backend owns it.
/// upstream: intel_drv.c dmar_detach()
pub fn dmar_detach(_state: &DriverState) -> Result<(), Error> {
    Err(Error::Unsupported)
}
/// Suspend is a no-op in the upstream implementation.
/// upstream: intel_drv.c dmar_suspend()
pub fn dmar_suspend(_state: &DriverState) -> Result<(), Error> {
    Ok(())
}
/// Resume is a no-op in the upstream implementation.
/// upstream: intel_drv.c dmar_resume()
pub fn dmar_resume(_state: &DriverState) -> Result<(), Error> {
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciPathEntry {
    pub device: u8,
    pub function: u8,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PciPath {
    pub segment: u16,
    pub root_bus: u8,
    /// Root-to-leaf PCI device/function path.
    pub path: Vec<PciPathEntry>,
}

/// upstream: intel_drv.c dmar_dev_depth()
pub fn dmar_dev_depth(path: &PciPath) -> usize {
    path.path.len()
}
/// upstream: intel_drv.c dmar_dev_path()
pub fn dmar_dev_path(path: &PciPath) -> (u8, Vec<PciPathEntry>) {
    (path.root_bus, path.path.clone())
}

/// Compare root bus and path prefix; endpoints require equal depth, bridges match descendants.
/// upstream: intel_drv.c dmar_match_pathes()
pub fn dmar_match_pathes(
    scope_bus: u8,
    scope_path: &[PciPathEntry],
    device_bus: u8,
    device_path: &[PciPathEntry],
    scope_type: u8,
) -> bool {
    if scope_bus != device_bus {
        return false;
    }
    if scope_type == SCOPE_ENDPOINT && scope_path.len() != device_path.len() {
        return false;
    }
    if scope_type != SCOPE_ENDPOINT && scope_type != SCOPE_BRIDGE {
        return false;
    }
    let depth = scope_path.len().min(device_path.len());
    scope_path[..depth] == device_path[..depth]
}

/// Decode the byte-packed ACPI PCI path and match it to the endpoint/bridge route.
/// upstream: intel_drv.c dmar_match_devscope()
pub fn dmar_match_devscope(scope: &OwnedScope, path: &PciPath) -> Result<bool, Error> {
    if scope.path.is_empty() || scope.path.len() % 2 != 0 {
        return Err(Error::InvalidStructure);
    }
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(scope.path.len() / 2)
        .map_err(|_| Error::OutOfMemory)?;
    for pair in scope.path.chunks_exact(2) {
        if pair[0] > 31 || pair[1] > 7 {
            return Err(Error::InvalidStructure);
        }
        entries.push(PciPathEntry {
            device: pair[0],
            function: pair[1],
        });
    }
    Ok(dmar_match_pathes(
        scope.start_bus,
        &entries,
        path.root_bus,
        &path.path,
        scope.scope_type,
    ))
}

/// Match a DRHD's segment and first explicit path, then use INCLUDE_ALL as fallback.
/// upstream: intel_drv.c dmar_match_by_path()
pub fn dmar_match_by_path<'a>(
    unit: &'a Unit,
    path: &PciPath,
) -> Result<Option<&'static str>, Error> {
    if unit.segment != path.segment {
        return Ok(None);
    }
    if unit.include_all {
        return Ok(Some("INCLUDE_ALL"));
    }
    for scope in &unit.scopes {
        if dmar_match_devscope(scope, path)? {
            return Ok(Some("specific match"));
        }
    }
    Ok(None)
}

/// Find the first explicit DRHD scope, then fall back to a matching INCLUDE_ALL unit.
/// upstream: intel_drv.c dmar_find_by_scope()
pub fn dmar_find_by_scope<'a>(
    table: &'a DmarTable,
    path: &PciPath,
) -> Result<Option<&'a Unit>, Error> {
    for unit in &table.units {
        if unit.segment == path.segment
            && !unit.include_all
            && dmar_match_by_path(unit, path)?.is_some()
        {
            return Ok(Some(unit));
        }
    }
    Ok(table
        .units
        .iter()
        .find(|unit| unit.segment == path.segment && unit.include_all))
}

/// Device lookup adapter; logging/property publication maps to the PCI provider.
/// upstream: intel_drv.c dmar_find()
pub fn dmar_find<'a>(
    table: &'a DmarTable,
    path: &PciPath,
    mut publish: impl FnMut(&Unit, &str),
) -> Result<Option<&'a Unit>, Error> {
    let Some(unit) = dmar_find_by_scope(table, path)? else {
        return Ok(None);
    };
    let match_kind = dmar_match_by_path(unit, path)?.unwrap_or("specific match");
    publish(unit, match_kind);
    Ok(Some(unit))
}

/// x86 IOMMU callback wrapper around PCI scope routing.
/// upstream: intel_drv.c dmar_find_method()
pub fn dmar_find_method<'a>(
    table: &'a DmarTable,
    path: &PciPath,
    publish: impl FnMut(&Unit, &str),
) -> Result<Option<&'a Unit>, Error> {
    dmar_find(table, path, publish)
}

/// Find an IOAPIC/HPET scope by enumeration ID, optionally returning its source ID.
/// upstream: intel_drv.c dmar_find_nonpci()
pub fn dmar_find_nonpci(
    table: &DmarTable,
    enumeration_id: u8,
    scope_type: u8,
) -> Option<(&Unit, Option<u16>)> {
    for unit in &table.units {
        for scope in &unit.scopes {
            if scope.scope_type != scope_type || scope.enumeration_id != enumeration_id {
                continue;
            }
            let rid = (scope.path.len() == 2).then(|| {
                u16::from(scope.start_bus) << 8
                    | u16::from(scope.path[0]) << 3
                    | u16::from(scope.path[1])
            });
            return Some((unit, rid));
        }
    }
    None
}
/// upstream: intel_drv.c dmar_find_hpet()
pub fn dmar_find_hpet(table: &DmarTable, uid: u8) -> Option<(&Unit, Option<u16>)> {
    dmar_find_nonpci(table, uid, SCOPE_HPET)
}
/// upstream: intel_drv.c dmar_find_ioapic()
pub fn dmar_find_ioapic(table: &DmarTable, apic_id: u8) -> Option<(&Unit, Option<u16>)> {
    dmar_find_nonpci(table, apic_id, SCOPE_IOAPIC)
}

/// Iterate scope-qualified reserved-memory records for one PCI path.
/// upstream: intel_drv.c dmar_rmrr_iter()
pub fn dmar_rmrr_iter<'a>(
    table: &'a DmarTable,
    path: &PciPath,
) -> Result<Vec<&'a ReservedRegion>, Error> {
    let mut regions = Vec::new();
    for region in &table.reserved_regions {
        if region.segment != path.segment {
            continue;
        }
        let mut matched = false;
        for scope in &region.scopes {
            if dmar_match_devscope(scope, path)? {
                matched = true;
                break;
            }
        }
        if matched {
            regions.try_reserve(1).map_err(|_| Error::OutOfMemory)?;
            regions.push(region);
        }
    }
    Ok(regions)
}
/// Public RMRR parser adapter for context initialization.
/// upstream: intel_drv.c dmar_dev_parse_rmrr()
pub fn dmar_dev_parse_rmrr<'a>(
    table: &'a DmarTable,
    path: &PciPath,
) -> Result<Vec<&'a ReservedRegion>, Error> {
    dmar_rmrr_iter(table, path)
}

pub trait PciPathResolver {
    fn find_device(&mut self, segment: u16, bus: u8, device: u8, function: u8) -> Option<u64>;
    fn secondary_bus(&mut self, device: u64) -> Option<u8>;
}
/// Resolve the ACPI PCI hierarchy to a concrete PCI function and requester ID.
/// upstream: intel_drv.c dmar_path_dev()
pub fn dmar_path_dev(
    segment: u16,
    root_bus: u8,
    path: &[PciPathEntry],
    resolver: &mut impl PciPathResolver,
) -> Option<(u64, u16)> {
    let mut bus = root_bus;
    let mut requester_bus = root_bus;
    let mut device_handle = None;
    for (index, entry) in path.iter().enumerate() {
        requester_bus = bus;
        let current = resolver.find_device(segment, bus, entry.device, entry.function)?;
        if index + 1 < path.len() {
            bus = resolver.secondary_bus(current)?;
        }
        device_handle = Some(current);
    }
    let last = path.last()?;
    let rid =
        u16::from(requester_bus) << 8 | u16::from(last.device) << 3 | u16::from(last.function);
    Some((device_handle?, rid))
}

/// Process scope-qualified RMRRs and instantiate only contexts owned by this unit.
/// upstream: intel_drv.c dmar_inst_rmrr_iter()
pub fn dmar_inst_rmrr_iter(
    unit_index: usize,
    table: &DmarTable,
    resolver: &mut impl PciPathResolver,
    mut unit_for_path: impl FnMut(&PciPath) -> Result<Option<usize>, Error>,
    mut instantiate: impl FnMut(usize, u16, ReservedRegion) -> Result<(), Error>,
) -> Result<(), Error> {
    for region in &table.reserved_regions {
        for scope in &region.scopes {
            if scope.scope_type != SCOPE_ENDPOINT {
                continue;
            }
            let mut path = Vec::new();
            path.try_reserve_exact(scope.path.len() / 2)
                .map_err(|_| Error::OutOfMemory)?;
            for pair in scope.path.chunks_exact(2) {
                path.push(PciPathEntry {
                    device: pair[0],
                    function: pair[1],
                });
            }
            let pci_path = PciPath {
                segment: region.segment,
                root_bus: scope.start_bus,
                path,
            };
            let found_unit = unit_for_path(&pci_path)?;
            if found_unit != Some(unit_index) {
                continue;
            }
            let rid = dmar_path_dev(region.segment, scope.start_bus, &pci_path.path, resolver)
                .map(|(_, rid)| rid)
                .unwrap_or_else(|| {
                    let final_node = pci_path.path.last().copied().unwrap_or(PciPathEntry {
                        device: 0,
                        function: 0,
                    });
                    u16::from(scope.start_bus) << 8
                        | u16::from(final_node.device) << 3
                        | u16::from(final_node.function)
                });
            instantiate(unit_index, rid, region.clone())?;
        }
    }
    Ok(())
}

/// RMRR one-time barrier plus protected-region/TE activation orchestration.
/// upstream: intel_drv.c dmar_instantiate_rmrr_ctxs()
pub fn dmar_instantiate_rmrr_ctxs<I: ContextInvalidator>(
    unit: &mut DmarUnit,
    barrier_flags: &core::sync::atomic::AtomicU32,
    contexts_present: bool,
    io: &mut I,
    mut instantiate: impl FnMut() -> Result<(), Error>,
    mut disable_protected: impl FnMut() -> Result<(), Error>,
    mut enable_translation: impl FnMut() -> Result<(), Error>,
) -> Result<bool, Error> {
    if !dmar_barrier_enter(barrier_flags, DMAR_BARRIER_RMRR)? {
        return Ok(false);
    }
    instantiate()?;
    if contexts_present {
        let _ = &unit; // retained as the source unit identity for logging/ordering
        disable_protected()?;
        enable_translation()?;
    }
    dmar_barrier_exit(barrier_flags, DMAR_BARRIER_RMRR)?;
    let _ = io;
    Ok(true)
}

/// Return ENXIO-equivalent when no DMAR driver is running.
/// upstream: intel_drv.c dmar_is_running()
pub fn dmar_is_running(running: bool) -> Result<(), Error> {
    if running {
        Ok(())
    } else {
        Err(Error::NoDevice)
    }
}

/// Render the ACPI PCI path in FreeBSD diagnostic form.
/// upstream: intel_drv.c dmar_print_path()
pub fn dmar_print_path(bus: u8, path: &[PciPathEntry]) -> String {
    let mut rendered = alloc::format!("[{bus}, ");
    for (index, entry) in path.iter().enumerate() {
        if index != 0 {
            rendered.push_str(", ");
        }
        rendered.push_str(&alloc::format!("({}, {})", entry.device, entry.function));
    }
    rendered.push(']');
    rendered
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct X86UnitCommon {
    pub unit_number: u32,
    pub qi_active: bool,
    pub queue_tail: u32,
    pub queue_available: u32,
}
pub struct DriverUnit {
    pub dmar: DmarUnit,
    pub x86: X86UnitCommon,
}
/// Return the x86 common record embedded in a unit.
/// upstream: intel_drv.c dmar_get_x86_common()
pub fn dmar_get_x86_common(unit: &mut DriverUnit) -> &mut X86UnitCommon {
    &mut unit.x86
}

/// Unit pre-instantiation orchestration calls pre-use quirks before RMRR contexts.
/// upstream: intel_drv.c dmar_unit_pre_instantiate_ctx()
pub fn dmar_unit_pre_instantiate_ctx(
    unit: &mut DmarUnit,
    flags: &core::sync::atomic::AtomicU32,
    northbridge: Option<NorthbridgeIdentity>,
    report: impl FnMut(&'static str),
    mut instantiate_rmrr: impl FnMut() -> Result<(), Error>,
) -> Result<(), Error> {
    let _ = dmar_quirks_pre_use(unit, flags, northbridge, report)?;
    instantiate_rmrr()
}

/// Install the Intel backend only when CPUID reports Intel.
/// upstream: intel_drv.c x86_iommu_set_intel()
pub fn x86_iommu_set_intel(cpu_vendor: &[u8; 12]) -> bool {
    cpu_vendor == b"GenuineIntel"
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    fn empty_table() -> Vec<u8> {
        let mut bytes = vec![0u8; TABLE_HEADER_BYTES];
        bytes[..4].copy_from_slice(b"DMAR");
        bytes[36] = 47;
        let size = bytes.len() as u32;
        bytes[4..8].copy_from_slice(&size.to_le_bytes());
        bytes
    }
    fn drhd(segment: u16, include_all: bool, scopes: &[u8]) -> Vec<u8> {
        let length = 16 + scopes.len();
        let mut record = vec![0u8; length];
        record[..2].copy_from_slice(&DRHD.to_le_bytes());
        record[2..4].copy_from_slice(&(length as u16).to_le_bytes());
        record[4] = u8::from(include_all);
        record[6..8].copy_from_slice(&segment.to_le_bytes());
        record[8..16].copy_from_slice(&0xfed9_0000u64.to_le_bytes());
        record[16..].copy_from_slice(scopes);
        record
    }

    #[test]
    fn table_iteration_finds_and_counts_drhd_and_rhsa_records() {
        let mut bytes = empty_table();
        bytes.extend_from_slice(&drhd(0, true, &[]));
        let len = bytes.len() as u32;
        bytes[4..8].copy_from_slice(&len.to_le_bytes());
        assert_eq!(dmar_count_iter(&bytes), Ok(1));
        assert!(dmar_find_iter(&bytes, 0).unwrap().is_some());
        assert!(dmar_find_by_index(&bytes, 1).unwrap().is_none());
        assert_eq!(dmar_identify(Some(&bytes), false).unwrap(), None);
        assert!(dmar_identify(Some(&bytes), true).unwrap().is_some());
    }

    #[test]
    fn rhsa_record_is_attached_to_its_matching_unit_register_base() {
        let mut bytes = empty_table();
        bytes.extend_from_slice(&drhd(0, true, &[]));
        let mut rhsa = vec![0u8; 20];
        rhsa[..2].copy_from_slice(&RHSA.to_le_bytes());
        rhsa[2..4].copy_from_slice(&20u16.to_le_bytes());
        rhsa[8..16].copy_from_slice(&0xfed9_0000u64.to_le_bytes());
        rhsa[16..20].copy_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&rhsa);
        let size = bytes.len() as u32;
        bytes[4..8].copy_from_slice(&size.to_le_bytes());
        let table = DmarTable::parse(&bytes).unwrap();
        assert_eq!(dmar_rhsa_iter(&bytes, 0xfed9_0000), Ok(Some(3)));
        assert_eq!(table.units[0].proximity_domain, Some(3));
    }

    #[test]
    fn path_match_requires_exact_endpoint_and_prefix_bridge() {
        let device = PciPath {
            segment: 0,
            root_bus: 0,
            path: vec![
                PciPathEntry {
                    device: 1,
                    function: 0,
                },
                PciPathEntry {
                    device: 2,
                    function: 3,
                },
            ],
        };
        assert!(dmar_match_pathes(
            0,
            &device.path,
            0,
            &device.path,
            SCOPE_ENDPOINT
        ));
        assert!(!dmar_match_pathes(
            0,
            &device.path[..1],
            0,
            &device.path,
            SCOPE_ENDPOINT
        ));
        assert!(dmar_match_pathes(
            0,
            &device.path[..1],
            0,
            &device.path,
            SCOPE_BRIDGE
        ));
    }

    #[test]
    fn include_all_is_fallback_to_more_specific_scope() {
        let mut table_bytes = empty_table();
        let scope = [1, 8, 0, 0, 1, 1, 2, 0];
        table_bytes.extend_from_slice(&drhd(0, true, &[]));
        table_bytes.extend_from_slice(&drhd(0, false, &scope));
        let size = table_bytes.len() as u32;
        table_bytes[4..8].copy_from_slice(&size.to_le_bytes());
        let table = DmarTable::parse(&table_bytes).unwrap();
        let path = PciPath {
            segment: 0,
            root_bus: 1,
            path: vec![PciPathEntry {
                device: 2,
                function: 0,
            }],
        };
        assert_eq!(
            dmar_find_by_scope(&table, &path)
                .unwrap()
                .unwrap()
                .include_all,
            false
        );
        assert_eq!(dmar_print_path(1, &path.path), "[1, (2, 0)]");
    }

    struct FakePciPath;
    impl PciPathResolver for FakePciPath {
        fn find_device(&mut self, segment: u16, bus: u8, device: u8, function: u8) -> Option<u64> {
            match (segment, bus, device, function) {
                (0, 0, 1, 0) => Some(0x10),
                (0, 1, 2, 0) => Some(0x20),
                _ => None,
            }
        }
        fn secondary_bus(&mut self, device: u64) -> Option<u8> {
            (device == 0x10).then_some(1)
        }
    }

    #[test]
    fn pci_path_resolution_returns_leaf_requester_bus_and_devfn() {
        let path = [
            PciPathEntry {
                device: 1,
                function: 0,
            },
            PciPathEntry {
                device: 2,
                function: 0,
            },
        ];
        assert_eq!(
            dmar_dev_depth(&PciPath {
                segment: 0,
                root_bus: 0,
                path: path.to_vec()
            }),
            2
        );
        assert_eq!(
            dmar_path_dev(0, 0, &path, &mut FakePciPath),
            Some((0x20, 0x0110))
        );
    }

    #[test]
    fn rmrr_iteration_requires_segment_and_scope_path_match() {
        let mut bytes = empty_table();
        bytes.extend_from_slice(&drhd(0, true, &[]));
        let scope = [1, 8, 0, 0, 1, 1, 2, 0];
        let mut rmrr = vec![0u8; 24 + scope.len()];
        let rmrr_len = rmrr.len() as u16;
        rmrr[..2].copy_from_slice(&RMRR.to_le_bytes());
        rmrr[2..4].copy_from_slice(&rmrr_len.to_le_bytes());
        rmrr[6..8].copy_from_slice(&0u16.to_le_bytes());
        rmrr[8..16].copy_from_slice(&0x2000u64.to_le_bytes());
        rmrr[16..24].copy_from_slice(&0x2fffu64.to_le_bytes());
        rmrr[24..].copy_from_slice(&scope);
        bytes.extend_from_slice(&rmrr);
        let size = bytes.len() as u32;
        bytes[4..8].copy_from_slice(&size.to_le_bytes());
        let table = DmarTable::parse(&bytes).unwrap();
        let path = PciPath {
            segment: 0,
            root_bus: 1,
            path: vec![PciPathEntry {
                device: 2,
                function: 0,
            }],
        };
        let entries = dmar_dev_parse_rmrr(&table, &path).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            (entries[0].base, entries[0].end_inclusive),
            (0x2000, 0x2fff)
        );
    }
}
