//! CPU identity and topology discovery for the x86 platform.
//!
//! The platform interfaces expose a dense, logical CPU number to the rest of
//! the kernel.  APIC IDs are hardware identities and are deliberately kept in
//! a separate table: they are not required to start at zero or to be
//! contiguous (and x2APIC IDs may be wider than eight bits).

use core::sync::atomic::{AtomicU8, AtomicU32, AtomicUsize, Ordering};

use axplat::mem::{PhysAddr, phys_to_virt};
use raw_cpuid::{CpuId, TopologyType};

const APIC_ID_UNASSIGNED: u32 = u32::MAX;
const MAP_UNINITIALIZED: u8 = 0;
const MAP_INITIALIZING: u8 = 1;
const MAP_READY: u8 = 2;

const MAX_CPU_NUM: usize = crate::config::plat::MAX_CPU_NUM;

static APIC_ID_MAP: [AtomicU32; MAX_CPU_NUM] =
    [const { AtomicU32::new(APIC_ID_UNASSIGNED) }; MAX_CPU_NUM];
static APIC_ID_MAP_LEN: AtomicUsize = AtomicUsize::new(0);
static APIC_ID_MAP_STATE: AtomicU8 = AtomicU8::new(MAP_UNINITIALIZED);

/// A dense logical-to-hardware APIC ID map used while constructing the
/// immutable runtime map.  Keeping this logic independent of atomics also
/// makes the non-contiguous-ID contract directly testable on the host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CpuApicMap<const N: usize> {
    ids: [u32; N],
    len: usize,
}

impl<const N: usize> CpuApicMap<N> {
    const fn new() -> Self {
        Self {
            ids: [APIC_ID_UNASSIGNED; N],
            len: 0,
        }
    }

    fn insert(&mut self, apic_id: u32) -> bool {
        if self.ids[..self.len].contains(&apic_id) || self.len == N {
            return false;
        }
        self.ids[self.len] = apic_id;
        self.len += 1;
        true
    }

    fn logical_for(&self, apic_id: u32) -> Option<usize> {
        self.ids[..self.len]
            .iter()
            .position(|candidate| *candidate == apic_id)
    }

    fn apic_for(&self, logical_cpu_id: usize) -> Option<u32> {
        self.ids
            .get(logical_cpu_id)
            .copied()
            .filter(|_| logical_cpu_id < self.len)
    }

    fn len(&self) -> usize {
        self.len
    }
}

/// Build a dense logical map while preserving the complete hardware ID.
///
/// `current_apic_id` is inserted first, making the BSP logical CPU 0.  On
/// legacy xAPIC hardware, IDs wider than eight bits cannot be addressed by a
/// physical IPI and are therefore excluded from the bootable topology.  The
/// current processor is retained even in that error case so the BSP can boot
/// and report the unsupported topology without sending a wrapped destination.
fn build_apic_map<const N: usize>(
    current_apic_id: u32,
    candidates: &[u32],
    x2apic: bool,
) -> CpuApicMap<N> {
    let mut map = CpuApicMap::new();
    map.insert(current_apic_id);
    for &apic_id in candidates {
        if x2apic || apic_id <= u8::MAX as u32 {
            map.insert(apic_id);
        }
    }
    map
}

/// Return whether this processor advertises x2APIC support.
pub(crate) fn x2apic_supported() -> bool {
    CpuId::new()
        .get_feature_info()
        .is_some_and(|features| features.has_x2apic())
}

/// Read the complete hardware APIC ID exposed by CPUID topology leaves.
///
/// CPUID leaf 1's *initial local APIC ID* is only eight bits.  It is used only
/// as the final compatibility fallback for CPUs that expose no topology leaf;
/// it is never treated as a dense logical CPU ID.
pub(crate) fn hardware_apic_id() -> u32 {
    let cpuid = CpuId::new();

    if let Some(mut levels) = cpuid.get_extended_topology_info_v2()
        && let Some(level) = levels.find(|level| level.level_type() != TopologyType::Invalid)
    {
        return level.x2apic_id();
    }
    if let Some(mut levels) = cpuid.get_extended_topology_info()
        && let Some(level) = levels.find(|level| level.level_type() != TopologyType::Invalid)
    {
        return level.x2apic_id();
    }
    if let Some(topology) = cpuid.get_processor_topology_info() {
        return topology.x2apic_id();
    }
    cpuid
        .get_feature_info()
        .map_or(0, |features| features.initial_local_apic_id() as u32)
}

fn install_map(current_apic_id: u32) {
    let mut candidates = [APIC_ID_UNASSIGNED; MAX_CPU_NUM];
    let candidate_count = discover_madt_apic_ids(&mut candidates);
    let map: CpuApicMap<MAX_CPU_NUM> = build_apic_map(
        current_apic_id,
        &candidates[..candidate_count],
        x2apic_supported(),
    );

    for logical_cpu_id in 0..map.len() {
        APIC_ID_MAP[logical_cpu_id].store(
            map.apic_for(logical_cpu_id)
                .expect("APIC map length invariant"),
            Ordering::Relaxed,
        );
    }
    APIC_ID_MAP_LEN.store(map.len(), Ordering::Release);
}

/// Discover and publish the immutable CPU topology while the boot page table
/// still provides the temporary physical-memory mapping.
///
/// Multiboot2's ACPI RSDP is owned by [`crate::boot_info`], but the XSDT/RSDT
/// and MADT that it names remain firmware-owned physical memory.  The normal
/// runtime page table is rebuilt from usable memory ranges and deliberately
/// does not map ACPI reclaimable/NVS ranges.  Publish the APIC map before that
/// rebuild so all later callers use the owned ID snapshot and never dereference
/// firmware tables after the temporary mapping is gone.
pub(crate) fn init_topology() {
    ensure_map(hardware_apic_id());
}

fn ensure_map(current_apic_id: u32) {
    loop {
        match APIC_ID_MAP_STATE.load(Ordering::Acquire) {
            MAP_READY => return,
            MAP_UNINITIALIZED => {
                if APIC_ID_MAP_STATE
                    .compare_exchange(
                        MAP_UNINITIALIZED,
                        MAP_INITIALIZING,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    install_map(current_apic_id);
                    APIC_ID_MAP_STATE.store(MAP_READY, Ordering::Release);
                    return;
                }
            }
            MAP_INITIALIZING => core::hint::spin_loop(),
            _ => unreachable!("invalid APIC map state"),
        }
    }
}

/// Resolve the hardware identity of the current processor to its dense
/// logical CPU ID.  An AP not present in the immutable MADT-derived map is a
/// fatal topology error rather than an opportunity to guess or wrap an ID.
pub(crate) fn current_logical_cpu_id() -> usize {
    let apic_id = hardware_apic_id();
    ensure_map(apic_id);
    logical_cpu_id_for_apic(apic_id)
        .unwrap_or_else(|| panic!("current APIC ID {apic_id:#x} is absent from the CPU topology"))
}

pub(crate) fn logical_cpu_id_for_apic(apic_id: u32) -> Option<usize> {
    ensure_map(apic_id);
    let len = APIC_ID_MAP_LEN.load(Ordering::Acquire);
    APIC_ID_MAP[..len]
        .iter()
        .position(|candidate| candidate.load(Ordering::Acquire) == apic_id)
}

pub(crate) fn apic_id_for_logical(logical_cpu_id: usize) -> Option<u32> {
    ensure_map(hardware_apic_id());
    let len = APIC_ID_MAP_LEN.load(Ordering::Acquire);
    if logical_cpu_id >= len {
        return None;
    }
    Some(APIC_ID_MAP[logical_cpu_id].load(Ordering::Acquire))
}

/// Number of CPUs that can be addressed by this platform implementation.
///
/// A missing or malformed ACPI topology intentionally reports one CPU (the
/// BSP) instead of manufacturing a contiguous APIC-ID list.
pub(crate) fn cpu_num() -> usize {
    ensure_map(hardware_apic_id());
    APIC_ID_MAP_LEN.load(Ordering::Acquire)
}

/// Validate that the APIC ID observed after local-APIC initialization is the
/// same complete ID used while entering the runtime.
pub(crate) fn assert_current_apic_id(apic_id: u32) {
    let expected = hardware_apic_id();
    assert_eq!(
        expected, apic_id,
        "local APIC ID changed during initialization: CPUID={expected:#x}, LAPIC={apic_id:#x}"
    );
    assert!(
        logical_cpu_id_for_apic(apic_id).is_some(),
        "local APIC ID {apic_id:#x} is absent from the CPU topology"
    );
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let bytes = bytes.get(offset..offset.checked_add(2)?)?;
    Some(u16::from_le_bytes([bytes[0], bytes[1]]))
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

unsafe fn physical_bytes(address: u64, length: usize) -> Option<&'static [u8]> {
    let address = usize::try_from(address).ok()?;
    address.checked_add(length)?;
    let ptr = phys_to_virt(PhysAddr::from_usize(address)).as_ptr();
    Some(unsafe { core::slice::from_raw_parts(ptr, length) })
}

fn checksum_is_valid(bytes: &[u8]) -> bool {
    bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)) == 0
}

fn find_rsdp_in_range(start: u64, end: u64) -> Option<u64> {
    let mut address = start;
    while address.checked_add(20)? <= end {
        let signature = unsafe { physical_bytes(address, 20)? };
        if &signature[..8] == b"RSD PTR " && checksum_is_valid(signature) {
            let revision = signature[15];
            if revision < 2 {
                return Some(address);
            }
            let length = read_u32(unsafe { physical_bytes(address, 36)? }, 20)? as usize;
            if (20..=4096).contains(&length)
                && checksum_is_valid(unsafe { physical_bytes(address, length)? })
            {
                return Some(address);
            }
        }
        address = address.checked_add(16)?;
    }
    None
}

fn find_rsdp() -> Option<u64> {
    let ebda_segment = read_u16(unsafe { physical_bytes(0x40e, 2)? }, 0)? as u64;
    find_rsdp_in_range(ebda_segment << 4, (ebda_segment << 4).checked_add(1024)?)
        .or_else(|| find_rsdp_in_range(0xe0000, 0x100000))
}

fn table_length_and_bytes(address: u64) -> Option<(usize, &'static [u8])> {
    let header = unsafe { physical_bytes(address, 36)? };
    let length = read_u32(header, 4)? as usize;
    if !(36..=1024 * 1024).contains(&length) {
        return None;
    }
    let table = unsafe { physical_bytes(address, length)? };
    checksum_is_valid(table).then_some((length, table))
}

fn find_madt_in_root(root_address: u64, entry_width: usize) -> Option<u64> {
    let (root_length, root) = table_length_and_bytes(root_address)?;
    if root_length < 36 || entry_width == 0 {
        return None;
    }
    let mut offset: usize = 36;
    while offset.checked_add(entry_width)? <= root_length {
        let table_address = if entry_width == 4 {
            read_u32(root, offset)? as u64
        } else {
            read_u64(root, offset)?
        };
        if let Some((_, table)) = table_length_and_bytes(table_address)
            && &table[..4] == b"APIC"
        {
            return Some(table_address);
        }
        offset = offset.checked_add(entry_width)?;
    }
    None
}

fn find_madt_from_rsdp(rsdp: &[u8]) -> Option<&'static [u8]> {
    let revision = rsdp[15];
    let xsdt_address = read_u64(rsdp, 24).unwrap_or(0);
    let rsdt_address = read_u32(rsdp, 16)? as u64;

    if revision >= 2
        && xsdt_address != 0
        && let Some(madt_address) = find_madt_in_root(xsdt_address, 8)
    {
        let (_, table) = table_length_and_bytes(madt_address)?;
        return Some(table);
    }
    let madt_address = find_madt_in_root(rsdt_address, 4)?;
    let (_, table) = table_length_and_bytes(madt_address)?;
    Some(table)
}

fn find_madt() -> Option<&'static [u8]> {
    // Multiboot2 ACPI tags are copied into the owned boot record before any
    // topology discovery.  If an owned RSDP exists, use it exclusively; a
    // failed root/MADT lookup must not silently switch to an unrelated legacy
    // scan.  MB1 and malformed/absent MB2 ACPI data use the established scan.
    if let Some(rsdp) = crate::boot_info::get().rsdp() {
        return find_madt_from_rsdp(rsdp.bytes());
    }

    let rsdp_address = find_rsdp()?;
    let rsdp = unsafe { physical_bytes(rsdp_address, 36)? };
    find_madt_from_rsdp(rsdp)
}

fn discover_madt_apic_ids<const N: usize>(out: &mut [u32; N]) -> usize {
    let Some(madt) = find_madt() else {
        return 0;
    };
    if madt.len() < 44 {
        return 0;
    }

    let mut count = 0;
    let mut offset: usize = 44;
    while let Some(end) = offset.checked_add(2) {
        if end > madt.len() {
            break;
        }
        let entry_type = madt[offset];
        let entry_length = madt[offset + 1] as usize;
        if entry_length < 2 {
            break;
        }
        let Some(entry_end) = offset.checked_add(entry_length) else {
            break;
        };
        if entry_end > madt.len() {
            break;
        }

        let (apic_id, flags) = match entry_type {
            // Processor Local APIC: ACPI processor ID, APIC ID, flags.
            0 if entry_length >= 8 => (madt[offset + 3] as u32, read_u32(madt, offset + 4)),
            // Processor Local x2APIC: reserved, x2APIC ID, flags, UID.
            9 if entry_length >= 16 => (
                read_u32(madt, offset + 4).unwrap_or(0),
                read_u32(madt, offset + 8),
            ),
            _ => (0, None),
        };
        if let Some(flags) = flags
            && flags & 1 != 0
            && !out[..count].contains(&apic_id)
            && count < N
        {
            out[count] = apic_id;
            count += 1;
        }
        offset = entry_end;
    }
    count
}

#[cfg(test)]
mod tests {
    use super::{CpuApicMap, build_apic_map};

    #[test]
    fn logical_map_preserves_non_contiguous_full_ids() {
        let map = build_apic_map::<4>(0x120, &[0x120, 0x20, 0x220, 0x20], true);
        assert_eq!(map.len(), 3);
        assert_eq!(map.apic_for(0), Some(0x120));
        assert_eq!(map.apic_for(1), Some(0x20));
        assert_eq!(map.apic_for(2), Some(0x220));
        assert_eq!(map.logical_for(0x220), Some(2));
        assert_eq!(map.logical_for(0x221), None);
    }

    #[test]
    fn xapic_map_rejects_unaddressable_extended_ids() {
        let map = build_apic_map::<4>(0x20, &[0x20, 0x220, 0xff], false);
        assert_eq!(map.len(), 2);
        assert_eq!(map.apic_for(1), Some(0xff));
        assert_eq!(map.logical_for(0x220), None);
    }

    #[test]
    fn map_capacity_is_a_hard_boundary() {
        let map = build_apic_map::<2>(7, &[9, 11, 13], true);
        assert_eq!(map.len(), 2);
        assert_eq!(map.apic_for(0), Some(7));
        assert_eq!(map.apic_for(1), Some(9));
        assert_eq!(map.apic_for(2), None);
    }

    #[test]
    fn duplicate_hardware_ids_do_not_create_logical_aliases() {
        let map = build_apic_map::<4>(7, &[7, 7, 9], true);
        assert_eq!(
            map,
            CpuApicMap {
                ids: [7, 9, u32::MAX, u32::MAX],
                len: 2
            }
        );
    }
}
