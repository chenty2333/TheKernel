//! Truthful package-PMU publication and ownership.
//!
//! Intel has no architectural, model-independent list of uncore MSRs.  In
//! particular, CPUID leaf 0x0a describes only the core PMU.  This module
//! therefore publishes a box only after a PerfMon-Discovery record supplied
//! by the platform decoder has bounded its counter/configuration registers.
//! Unknown discovery revisions deliberately produce no PMUs and touch no
//! MSRs.  That conservative rule is important on VMs and on unvalidated
//! Intel products where an apparently familiar MSR can belong to firmware.

use core::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};

#[cfg(target_os = "none")]
use axplat::mem::{pa, phys_to_virt};

use kspin::SpinNoIrq;

use crate::{
    cpu,
    pmu::{self, Error, ProductClass},
};

/// Maximum boxes accepted from a single discovered Panther Lake package.
/// This is a storage bound, not a statement about the hardware box count.
pub const MAX_BOXES: usize = 16;
/// Counter slots are preallocated so a reconcile IPI never allocates.
pub const MAX_COUNTERS_PER_BOX: usize = 8;
const DISCOVERY_MSR: u32 = 0x201e;
const DISCOVERY_GLOBAL_WORDS: usize = 3;
const MAX_DISCOVERY_STRIDE_QWORDS: usize = 8;
const GENERIC_RAW_EVENT_MASK: u64 = 0x0000_00ff_ff04_ffff;
const MSR_RAPL_POWER_UNIT: u32 = 0x606;
const MSR_IA32_MPERF: u32 = 0x0e7;
const MSR_IA32_APERF: u32 = 0x0e8;
const MSR_PKG_ENERGY_STATUS: u32 = 0x611;
const MSR_PP0_ENERGY_STATUS: u32 = 0x639;
const MSR_CORE_C3_RESIDENCY: u32 = 0x3fc;
const MSR_CORE_C6_RESIDENCY: u32 = 0x3fd;
const MSR_CORE_C7_RESIDENCY: u32 = 0x3fe;
const MSR_PKG_C2_RESIDENCY: u32 = 0x60d;
const MSR_PKG_C3_RESIDENCY: u32 = 0x3f8;
const MSR_PKG_C6_RESIDENCY: u32 = 0x3f9;
const MSR_PKG_C7_RESIDENCY: u32 = 0x3fa;
const MSR_PKG_C8_RESIDENCY: u32 = 0x630;
const MSR_PKG_C9_RESIDENCY: u32 = 0x631;
const MSR_PKG_C10_RESIDENCY: u32 = 0x632;

/// The source class exposed through perf's dynamic PMU registry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum UncoreKind {
    Uncore = 0,
    Power = 1,
    CoreCstate = 2,
    PackageCstate = 3,
    Msr = 4,
}

/// Read-only PMU families whose model-specific register presence is accepted
/// only for the Panther Lake product whitelist.  Their numeric configs are
/// stable ABI selectors, not raw MSR numbers supplied by userspace.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ReadOnlyPmu {
    Msr = 0,
    Power = 1,
    CoreCstate = 2,
    PackageCstate = 3,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReadOnlyPmuSnapshot {
    pub pmu: ReadOnlyPmu,
    pub name: &'static str,
    /// Package PMUs must execute on this CPU; core residency is local to the
    /// CPU where it is read.
    pub package_scoped: bool,
    pub owner_cpu: usize,
    pub package_id: u32,
}

/// RAPL energy status counters are architecturally 32-bit and wrap.  This
/// helper is used by the perf core to account a read interval exactly once.
pub const fn wrapping_counter_delta(previous: u64, current: u64, width: u8) -> u64 {
    if width == 0 {
        return 0;
    }
    if width >= 64 {
        return current.wrapping_sub(previous);
    }
    current.wrapping_sub(previous) & ((1u64 << width) - 1)
}

/// Convert RAPL's `ENERGY_UNIT` exponent to a fixed-point joule multiplier.
/// Q32 avoids floating point in the PMU and preserves a raw counter ABI.
pub const fn rapl_energy_unit_q32(power_unit: u64) -> Option<u64> {
    let exponent = ((power_unit >> 8) & 0x1f) as u8;
    if exponent < 32 {
        Some(1u64 << (32 - exponent))
    } else {
        None
    }
}

/// The access mechanism encoded by the Intel generic discovery table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum AccessType {
    Msr = 0,
    Mmio = 1,
    Pci = 2,
}

impl AccessType {
    fn decode(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(Self::Msr),
            1 => Some(Self::Mmio),
            2 => Some(Self::Pci),
            _ => None,
        }
    }
}

/// A bounded decoder result.  The decoder must have established every range
/// against the discovery record before constructing this type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiscoveryBox {
    pub kind: UncoreKind,
    /// Linux-compatible event-source basename, for example `uncore_imc_0`.
    pub name: &'static str,
    /// Discovery's opaque type/id pair.  Linux names a generic box from this
    /// pair instead of guessing an IMC/IIO/etc. marketing name.
    pub box_type: u16,
    pub box_id: u16,
    pub access: AccessType,
    /// Access-method-specific box control address from the discovery table.
    pub control: u64,
    pub ctl_offset: u8,
    pub ctr_offset: u8,
    /// First event-select MSR; zero means a read-only energy/residency source.
    pub config_msr: u32,
    /// Bits the discovery record grants to this kernel.  Restoration uses an
    /// RMW of exactly these bits so firmware/another owner keeps all others.
    pub config_mask: u64,
    /// First counter/status MSR.
    pub counter_msr: u32,
    pub counters: u8,
    pub width: u8,
    /// Event selectors accepted by this box.  Zero for read-only sources.
    pub event_mask: u64,
    /// A package owner is the only CPU permitted to program/read the box.
    pub package_id: u32,
    pub owner_cpu: usize,
}

impl DiscoveryBox {
    const EMPTY: Self = Self {
        kind: UncoreKind::Uncore,
        name: "",
        box_type: 0,
        box_id: 0,
        access: AccessType::Msr,
        control: 0,
        ctl_offset: 0,
        ctr_offset: 0,
        config_msr: 0,
        config_mask: 0,
        counter_msr: 0,
        counters: 0,
        width: 0,
        event_mask: 0,
        package_id: 0,
        owner_cpu: 0,
    };

    fn valid(self) -> bool {
        !self.name.is_empty()
            && self.counters > 0
            && self.counters as usize <= MAX_COUNTERS_PER_BOX
            && self.width > 0
            && self.width <= 64
            && self.control != 0
            && self.counter_msr != 0
            && (self.config_msr == 0 || (self.event_mask != 0 && self.config_mask != 0))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UncorePmuSnapshot {
    pub kind: UncoreKind,
    pub name: &'static str,
    pub box_type: u16,
    pub box_id: u16,
    pub access: AccessType,
    pub cpus: usize,
    pub counters: u8,
    pub width: u8,
    pub event_mask: u64,
    pub package_id: u32,
}

#[derive(Clone, Copy)]
struct SavedCounter {
    config: u64,
    value: u64,
}
const EMPTY_SAVED: SavedCounter = SavedCounter {
    config: 0,
    value: 0,
};

/// A capability for exactly one discovered package counter.  It is deliberately
/// opaque to callers: the owner validates both monotonically changing words
/// before it will read, program, or restore a selector.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UncoreLease {
    pub box_type: u16,
    pub box_id: u16,
    pub counter: u8,
    generation: u64,
    cookie: u64,
}

#[derive(Clone, Copy)]
struct LeaseSlot {
    generation: u64,
    cookie: u64,
}
const EMPTY_LEASE: LeaseSlot = LeaseSlot {
    generation: 0,
    cookie: 0,
};

#[derive(Clone, Copy)]
struct BoxState {
    discovered: bool,
    descriptor: DiscoveryBox,
    baseline: [SavedCounter; MAX_COUNTERS_PER_BOX],
    leases: [LeaseSlot; MAX_COUNTERS_PER_BOX],
}
const EMPTY_BOX: BoxState = BoxState {
    discovered: false,
    descriptor: DiscoveryBox::EMPTY,
    baseline: [EMPTY_SAVED; MAX_COUNTERS_PER_BOX],
    leases: [EMPTY_LEASE; MAX_COUNTERS_PER_BOX],
};

static BOXES: SpinNoIrq<[BoxState; MAX_BOXES]> = SpinNoIrq::new([EMPTY_BOX; MAX_BOXES]);
static BOX_COUNT: AtomicUsize = AtomicUsize::new(0);
static NEXT_LEASE_WORD: AtomicU64 = AtomicU64::new(1);

/// Lock-free terminal projection of a validated discovery descriptor.
/// Normal operation keeps richer state behind `BOXES`; crash-kexec may have
/// interrupted that lock's owner and therefore consumes only these atomics.
struct CrashBox {
    access: AtomicU8,
    control: AtomicU64,
    ctl_offset: AtomicU8,
    config_msr: AtomicU64,
    config_mask: AtomicU64,
    counters: AtomicU8,
    owner_cpu: AtomicUsize,
}
impl CrashBox {
    const fn new() -> Self {
        Self {
            access: AtomicU8::new(0),
            control: AtomicU64::new(0),
            ctl_offset: AtomicU8::new(0),
            config_msr: AtomicU64::new(0),
            config_mask: AtomicU64::new(0),
            counters: AtomicU8::new(0),
            owner_cpu: AtomicUsize::new(usize::MAX),
        }
    }
    fn publish(&self, descriptor: DiscoveryBox) {
        self.access
            .store(descriptor.access as u8, Ordering::Relaxed);
        self.control.store(descriptor.control, Ordering::Relaxed);
        self.ctl_offset
            .store(descriptor.ctl_offset, Ordering::Relaxed);
        self.config_msr
            .store(descriptor.config_msr as u64, Ordering::Relaxed);
        self.config_mask
            .store(descriptor.config_mask, Ordering::Relaxed);
        self.counters.store(descriptor.counters, Ordering::Relaxed);
        self.owner_cpu
            .store(descriptor.owner_cpu, Ordering::Relaxed);
    }
}
static CRASH_BOXES: [CrashBox; MAX_BOXES] = [const { CrashBox::new() }; MAX_BOXES];

/// Per-package reconcile mailbox.  The owner consumes a generation only once;
/// a stale close/disable cannot reprogram a newer placement.
#[repr(C, align(64))]
struct ReconcileMailbox {
    generation: AtomicU64,
    acknowledged: AtomicU64,
    operation: AtomicU8,
    box_type: AtomicU64,
    box_id: AtomicU64,
    index: AtomicU8,
    config: AtomicU64,
    lease_generation: AtomicU64,
    lease_cookie: AtomicU64,
    result: AtomicU64,
}
impl ReconcileMailbox {
    const fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            acknowledged: AtomicU64::new(0),
            operation: AtomicU8::new(0),
            box_type: AtomicU64::new(0),
            box_id: AtomicU64::new(0),
            index: AtomicU8::new(0),
            config: AtomicU64::new(0),
            lease_generation: AtomicU64::new(0),
            lease_cookie: AtomicU64::new(0),
            result: AtomicU64::new(0),
        }
    }
}
const RECONCILE_NONE: u8 = 0;
const RECONCILE_PROGRAM: u8 = 1;
const RECONCILE_READ: u8 = 2;
const RECONCILE_STOP_RESTORE: u8 = 3;
const RECONCILE_RESERVE_PROGRAM: u8 = 4;
const RECONCILE_READ_LEASE: u8 = 5;
const RECONCILE_SETTLE_RELEASE: u8 = 6;

fn publish_payload(
    owner_cpu: usize,
    generation: u64,
    operation: u8,
    box_type: u16,
    box_id: u16,
    index: u8,
    config: u64,
    lease: Option<UncoreLease>,
) -> Result<(), Error> {
    let mailbox = RECONCILE.get(owner_cpu).ok_or(Error::Unsupported)?;
    if mailbox
        .operation
        .compare_exchange(RECONCILE_NONE, u8::MAX, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(Error::Busy);
    }
    mailbox.box_type.store(box_type as u64, Ordering::Relaxed);
    mailbox.box_id.store(box_id as u64, Ordering::Relaxed);
    mailbox.index.store(index, Ordering::Relaxed);
    mailbox.config.store(config, Ordering::Relaxed);
    mailbox
        .lease_generation
        .store(lease.map_or(0, |lease| lease.generation), Ordering::Relaxed);
    mailbox
        .lease_cookie
        .store(lease.map_or(0, |lease| lease.cookie), Ordering::Relaxed);
    mailbox.result.store(u64::MAX, Ordering::Relaxed);
    mailbox.operation.store(operation, Ordering::Relaxed);
    mailbox.generation.store(generation, Ordering::Release);
    Ok(())
}

pub fn publish_program(
    owner_cpu: usize,
    generation: u64,
    box_type: u16,
    box_id: u16,
    index: u8,
    config: u64,
) -> Result<(), Error> {
    publish_payload(
        owner_cpu,
        generation,
        RECONCILE_PROGRAM,
        box_type,
        box_id,
        index,
        config,
        None,
    )
}
pub fn publish_read(
    owner_cpu: usize,
    generation: u64,
    box_type: u16,
    box_id: u16,
    index: u8,
) -> Result<(), Error> {
    publish_payload(
        owner_cpu,
        generation,
        RECONCILE_READ,
        box_type,
        box_id,
        index,
        0,
        None,
    )
}
pub fn publish_stop_restore(owner_cpu: usize, generation: u64) -> Result<(), Error> {
    publish_payload(
        owner_cpu,
        generation,
        RECONCILE_STOP_RESTORE,
        0,
        0,
        0,
        0,
        None,
    )
}

pub fn publish_reserve_program(
    owner_cpu: usize,
    generation: u64,
    box_type: u16,
    box_id: u16,
    config: u64,
) -> Result<(), Error> {
    publish_payload(
        owner_cpu,
        generation,
        RECONCILE_RESERVE_PROGRAM,
        box_type,
        box_id,
        0,
        config,
        None,
    )
}

pub fn publish_read_lease(
    owner_cpu: usize,
    generation: u64,
    lease: UncoreLease,
) -> Result<(), Error> {
    publish_payload(
        owner_cpu,
        generation,
        RECONCILE_READ_LEASE,
        lease.box_type,
        lease.box_id,
        lease.counter,
        0,
        Some(lease),
    )
}

pub fn publish_settle_release(
    owner_cpu: usize,
    generation: u64,
    lease: UncoreLease,
) -> Result<(), Error> {
    publish_payload(
        owner_cpu,
        generation,
        RECONCILE_SETTLE_RELEASE,
        lease.box_type,
        lease.box_id,
        lease.counter,
        0,
        Some(lease),
    )
}
static RECONCILE: [ReconcileMailbox; crate::config::plat::MAX_CPU_NUM] =
    [const { ReconcileMailbox::new() }; crate::config::plat::MAX_CPU_NUM];

/// Set up the immutable, package-local discovery cache.  A caller must only
/// supply entries decoded from Intel PerfMon Discovery.  Invalid, duplicate,
/// multi-package, or non-Panther Lake input fails closed.
pub fn install_discovery_for_test_or_firmware(records: &[DiscoveryBox]) -> Result<(), Error> {
    let local = pmu::capability_snapshot()?;
    if local.product != ProductClass::PantherLake || records.is_empty() || records.len() > MAX_BOXES
    {
        return Err(Error::Unsupported);
    }
    let cpu_count = pmu::fleet_cpu_count()?;
    if records.iter().any(|record| {
        !record.valid()
            || record.owner_cpu >= cpu_count
            || cpu::topology_for_logical(record.owner_cpu)
                .map(|topology| topology.package_id != record.package_id)
                .unwrap_or(true)
    }) {
        return Err(Error::InvalidProgram);
    }
    let package = records[0].package_id;
    if records.iter().any(|record| record.package_id != package)
        || records.iter().enumerate().any(|(i, record)| {
            records[..i]
                .iter()
                .any(|prior| prior.box_type == record.box_type && prior.box_id == record.box_id)
        })
    {
        return Err(Error::Unsupported);
    }
    let mut boxes = BOXES.lock();
    if BOX_COUNT.load(Ordering::Acquire) != 0 {
        return Err(Error::Busy);
    }
    for (index, record) in records.iter().copied().enumerate() {
        boxes[index] = BoxState {
            discovered: true,
            descriptor: record,
            baseline: capture_baseline(record),
            leases: [EMPTY_LEASE; MAX_COUNTERS_PER_BOX],
        };
        CRASH_BOXES[index].publish(record);
    }
    BOX_COUNT.store(records.len(), Ordering::Release);
    Ok(())
}

/// Decode the three-word global header plus `max_units` unit records from an
/// Intel generic PerfMon-discovery table.  This is intentionally a pure
/// parser, so malformed firmware tables can be model-tested without issuing
/// an MSR, PCI, or MMIO operation.
pub fn decode_discovery_table(
    words: &[u64],
    package_id: u32,
    owner_cpu: usize,
) -> Result<([Option<DiscoveryBox>; MAX_BOXES], usize), Error> {
    if words.len() < DISCOVERY_GLOBAL_WORDS {
        return Err(Error::InvalidProgram);
    }
    let global = words[0];
    let stride_qwords = ((global >> 8) & 0xff) as usize;
    let max_units = ((global >> 16) & 0x3ff) as usize;
    let global_access = AccessType::decode((global >> 62) as u8).ok_or(Error::Unsupported)?;
    let global_ctl = words[1];
    if (global & 0xff) == 0
        || stride_qwords < DISCOVERY_GLOBAL_WORDS
        || max_units == 0
        || max_units > MAX_BOXES
        || global_ctl == 0
        || words[2] == u64::MAX
    {
        return Err(Error::InvalidProgram);
    }
    let required = (1usize)
        .checked_add(max_units)
        .and_then(|entries| entries.checked_mul(stride_qwords))
        .ok_or(Error::InvalidProgram)?;
    if required > words.len() {
        return Err(Error::InvalidProgram);
    }
    let mut decoded = [None; MAX_BOXES];
    let mut count = 0;
    for unit_index in 0..max_units {
        let at = (unit_index + 1) * stride_qwords;
        let table1 = words[at];
        let control = words[at + 1];
        let table3 = words[at + 2];
        if table1 == 0
            || table1 == u64::MAX
            || control == 0
            || control == u64::MAX
            || table3 == u64::MAX
        {
            continue;
        }
        let Some(access) = AccessType::decode((table1 >> 62) as u8) else {
            continue;
        };
        // Linux keys a discovery type by `box_type`; mixed access methods for
        // one type are unsafe, and the global access must agree with units.
        if access != global_access {
            continue;
        }
        let counters = (table1 & 0xff) as u8;
        let ctl_offset = ((table1 >> 8) & 0xff) as u32;
        let width = ((table1 >> 16) & 0xff) as u8;
        let ctr_offset = ((table1 >> 24) & 0xff) as u32;
        let box_type = (table3 & 0xffff) as u16;
        let box_id = ((table3 >> 16) & 0xffff) as u16;
        if counters == 0
            || counters as usize > MAX_COUNTERS_PER_BOX
            || width == 0
            || width > 64
            || ctl_offset == 0
            || ctr_offset == 0
            || box_type == 0
        {
            continue;
        }
        if !unit_access_is_bounded(access, control, ctl_offset, ctr_offset, counters) {
            continue;
        }
        let record = DiscoveryBox {
            kind: UncoreKind::Uncore,
            name: "uncore_discovery",
            box_type,
            box_id,
            access,
            control,
            ctl_offset: ctl_offset as u8,
            ctr_offset: ctr_offset as u8,
            config_msr: if access == AccessType::Msr {
                (control as u32)
                    .checked_add(ctl_offset)
                    .ok_or(Error::InvalidProgram)?
            } else {
                0
            },
            config_mask: GENERIC_RAW_EVENT_MASK,
            counter_msr: if access == AccessType::Msr {
                (control as u32)
                    .checked_add(ctr_offset)
                    .ok_or(Error::InvalidProgram)?
            } else {
                1
            },
            counters,
            width,
            event_mask: GENERIC_RAW_EVENT_MASK,
            package_id,
            owner_cpu,
        };
        if record.valid() {
            decoded[count] = Some(record);
            count += 1;
        }
    }
    Ok((decoded, count))
}

#[cfg(target_os = "none")]
fn discovery_table_is_mapped(base: usize, bytes: usize) -> bool {
    let end = match base.checked_add(bytes) {
        Some(end) => end,
        None => return false,
    };
    crate::config::devices::MMIO_RANGES
        .iter()
        .any(|&(range, size)| {
            range
                .checked_add(size)
                .is_some_and(|range_end| base >= range && end <= range_end)
        })
}

#[cfg(target_os = "none")]
fn pci_ecam_address(control: u64, register: usize, width: usize) -> Option<usize> {
    // control is exactly domain[30:28]:bus[27:20]:devfn[19:12]:box[11:0].
    if control & !0x7fff_ffff != 0 || ((control >> 28) & 7) != 0 {
        return None;
    }
    let bus = ((control >> 20) & 0xff) as usize;
    let devfn = ((control >> 12) & 0xff) as usize;
    let box_offset = (control & 0xfff) as usize;
    let config = box_offset.checked_add(register)?;
    if bus > crate::config::devices::PCI_BUS_END
        || config & 3 != 0
        || config.checked_add(width)? > 4096
    {
        return None;
    }
    let address = crate::config::devices::PCI_ECAM_BASE
        .checked_add(bus << 20)?
        .checked_add((devfn >> 3) << 15)?
        .checked_add((devfn & 7) << 12)?
        .checked_add(config)?;
    discovery_table_is_mapped(address, width).then_some(address)
}

fn unit_access_is_bounded(
    access: AccessType,
    control: u64,
    ctl_offset: u32,
    ctr_offset: u32,
    counters: u8,
) -> bool {
    let last = counters.saturating_sub(1) as u32;
    let config_last = ctl_offset.checked_add(last.saturating_mul(match access {
        AccessType::Msr => 1,
        AccessType::Mmio => 4,
        AccessType::Pci => 8,
    }));
    let counter_last = ctr_offset.checked_add(last.saturating_mul(match access {
        AccessType::Msr => 1,
        AccessType::Mmio | AccessType::Pci => 8,
    }));
    let (Some(config_last), Some(counter_last)) = (config_last, counter_last) else {
        return false;
    };
    match access {
        AccessType::Msr => {
            control <= u32::MAX as u64
                && control
                    .checked_add(counter_last as u64)
                    .is_some_and(|address| address <= u32::MAX as u64)
        }
        #[cfg(target_os = "none")]
        AccessType::Mmio => usize::try_from(control).ok().is_some_and(|base| {
            discovery_table_is_mapped(base, counter_last as usize + 8)
                && discovery_table_is_mapped(base, config_last as usize + 4)
        }),
        #[cfg(not(target_os = "none"))]
        AccessType::Mmio => false,
        #[cfg(target_os = "none")]
        AccessType::Pci => {
            pci_ecam_address(control, config_last as usize, 4).is_some()
                && pci_ecam_address(control, counter_last as usize, 8).is_some()
        }
        #[cfg(not(target_os = "none"))]
        AccessType::Pci => false,
    }
}

#[cfg(target_os = "none")]
unsafe fn table_word(base: usize, index: usize) -> u64 {
    unsafe { core::ptr::read_volatile(phys_to_virt(pa!(base + index * 8)).as_ptr().cast::<u64>()) }
}

#[cfg(target_os = "none")]
unsafe fn mmio_read32(base: u64, offset: usize) -> u32 {
    unsafe { core::ptr::read_volatile(phys_to_virt(pa!(base as usize + offset)).as_ptr().cast()) }
}

#[cfg(target_os = "none")]
unsafe fn mmio_write32(base: u64, offset: usize, value: u32) {
    unsafe {
        core::ptr::write_volatile(
            phys_to_virt(pa!(base as usize + offset))
                .as_mut_ptr()
                .cast(),
            value,
        )
    }
}

#[cfg(target_os = "none")]
unsafe fn mmio_read64(base: u64, offset: usize) -> u64 {
    unsafe { core::ptr::read_volatile(phys_to_virt(pa!(base as usize + offset)).as_ptr().cast()) }
}

#[cfg(target_os = "none")]
unsafe fn mmio_write64(base: u64, offset: usize, value: u64) {
    unsafe {
        core::ptr::write_volatile(
            phys_to_virt(pa!(base as usize + offset))
                .as_mut_ptr()
                .cast(),
            value,
        )
    }
}

#[cfg(target_os = "none")]
unsafe fn pci_read32(control: u64, offset: usize) -> Option<u32> {
    let address = pci_ecam_address(control, offset, 4)?;
    Some(unsafe { core::ptr::read_volatile(phys_to_virt(pa!(address)).as_ptr().cast()) })
}

#[cfg(target_os = "none")]
unsafe fn pci_write32(control: u64, offset: usize, value: u32) -> bool {
    let Some(address) = pci_ecam_address(control, offset, 4) else {
        return false;
    };
    unsafe { core::ptr::write_volatile(phys_to_virt(pa!(address)).as_mut_ptr().cast(), value) };
    true
}

#[cfg(target_os = "none")]
unsafe fn pci_read64(control: u64, offset: usize) -> Option<u64> {
    let lo = unsafe { pci_read32(control, offset) }? as u64;
    let hi = unsafe { pci_read32(control, offset.checked_add(4)?) }? as u64;
    Some(lo | (hi << 32))
}

#[cfg(target_os = "none")]
unsafe fn pci_write64(control: u64, offset: usize, value: u64) -> bool {
    (unsafe { pci_write32(control, offset, value as u32) })
        && (unsafe { pci_write32(control, offset + 4, (value >> 32) as u32) })
}

/// Read the package discovery table from `MSR_UNCORE_PERFMON_GLOBAL_CTL`
/// (0x201e), validate its full MMIO span, then publish only generic MSR boxes
/// with bounded counter/configuration addresses.
pub fn discover_current() -> Result<usize, Error> {
    let snapshot = pmu::capability_snapshot()?;
    if snapshot.product != ProductClass::PantherLake {
        return Err(Error::Unsupported);
    }
    #[cfg(not(target_os = "none"))]
    return Err(Error::Unsupported);
    #[cfg(target_os = "none")]
    {
        let base = unsafe { x86::msr::rdmsr(DISCOVERY_MSR) } as usize;
        if base == 0
            || base & 7 != 0
            || !discovery_table_is_mapped(base, DISCOVERY_GLOBAL_WORDS * 8)
        {
            return Err(Error::Unsupported);
        }
        let global = unsafe { table_word(base, 0) };
        let stride = ((global >> 8) & 0xff) as usize;
        let units = ((global >> 16) & 0x3ff) as usize;
        let words = (1usize)
            .checked_add(units)
            .and_then(|entries| entries.checked_mul(stride))
            .ok_or(Error::InvalidProgram)?;
        let bytes = words.checked_mul(8).ok_or(Error::InvalidProgram)?;
        if units == 0
            || units > MAX_BOXES
            || !(DISCOVERY_GLOBAL_WORDS..=MAX_DISCOVERY_STRIDE_QWORDS).contains(&stride)
            || !discovery_table_is_mapped(base, bytes)
        {
            return Err(Error::InvalidProgram);
        }
        let mut table = [0u64; (MAX_BOXES + 1) * MAX_DISCOVERY_STRIDE_QWORDS];
        if words > table.len() {
            return Err(Error::InvalidProgram);
        }
        for (index, word) in table.iter_mut().take(words).enumerate() {
            *word = unsafe { table_word(base, index) };
        }
        let owner_cpu = cpu::current_logical_cpu_id();
        let package_id = cpu::topology_for_logical(owner_cpu)
            .ok_or(Error::Unsupported)?
            .package_id;
        let (decoded, count) = decode_discovery_table(&table[..words], package_id, owner_cpu)?;
        if count == 0 {
            return Err(Error::Unsupported);
        }
        let mut compact = [DiscoveryBox::EMPTY; MAX_BOXES];
        for (index, record) in decoded.into_iter().flatten().enumerate() {
            compact[index] = record;
        }
        install_discovery_for_test_or_firmware(&compact[..count])?;
        Ok(count)
    }
}

/// Returns only committed, hardware-described package PMUs.
pub fn discovered_pmus() -> impl Iterator<Item = UncorePmuSnapshot> {
    let count = BOX_COUNT.load(Ordering::Acquire).min(MAX_BOXES);
    let boxes = *BOXES.lock();
    let mut result = [None; MAX_BOXES];
    for (index, state) in boxes[..count].iter().enumerate() {
        if state.discovered {
            result[index] = Some(UncorePmuSnapshot {
                kind: state.descriptor.kind,
                name: state.descriptor.name,
                box_type: state.descriptor.box_type,
                box_id: state.descriptor.box_id,
                access: state.descriptor.access,
                cpus: state.descriptor.owner_cpu,
                counters: state.descriptor.counters,
                width: state.descriptor.width,
                event_mask: state.descriptor.event_mask,
                package_id: state.descriptor.package_id,
            });
        }
    }
    result.into_iter().flatten()
}

/// Enumerate only read-only MSR PMUs guaranteed by the committed Panther Lake
/// product contract.  No source is published for architectural-only Intel,
/// AMD, virtual PMUs, or an incomplete topology.
pub fn readonly_pmus() -> impl Iterator<Item = ReadOnlyPmuSnapshot> {
    let Ok(snapshot) = pmu::capability_snapshot() else {
        return [None; 4].into_iter().flatten();
    };
    if snapshot.product != ProductClass::PantherLake
        || snapshot.family != 6
        || snapshot.model != 0xcc
    {
        return [None; 4].into_iter().flatten();
    }
    let cpu = cpu::current_logical_cpu_id();
    let Some(topology) = cpu::topology_for_logical(cpu) else {
        return [None; 4].into_iter().flatten();
    };
    let owner = (0..pmu::fleet_cpu_count().unwrap_or(0))
        .find(|candidate| {
            cpu::topology_for_logical(*candidate)
                .is_some_and(|other| other.package_id == topology.package_id)
        })
        .unwrap_or(cpu);
    [
        Some(ReadOnlyPmuSnapshot {
            pmu: ReadOnlyPmu::Msr,
            name: "msr",
            package_scoped: false,
            owner_cpu: cpu,
            package_id: topology.package_id,
        }),
        Some(ReadOnlyPmuSnapshot {
            pmu: ReadOnlyPmu::Power,
            name: "power",
            package_scoped: true,
            owner_cpu: owner,
            package_id: topology.package_id,
        }),
        Some(ReadOnlyPmuSnapshot {
            pmu: ReadOnlyPmu::CoreCstate,
            name: "cstate_core",
            package_scoped: false,
            owner_cpu: cpu,
            package_id: topology.package_id,
        }),
        Some(ReadOnlyPmuSnapshot {
            pmu: ReadOnlyPmu::PackageCstate,
            name: "cstate_pkg",
            package_scoped: true,
            owner_cpu: owner,
            package_id: topology.package_id,
        }),
    ]
    .into_iter()
    .flatten()
}

/// Read a fixed, read-only MSR event.  Power and package residency require
/// the package owner CPU; core residency must run locally.  Values are raw
/// counters, with the RAPL energy unit exposed separately through
/// [`rapl_energy_unit_q32`].
pub fn read_readonly_current(pmu_kind: ReadOnlyPmu, config: u64) -> Result<u64, Error> {
    #[cfg(not(target_os = "none"))]
    return Err(Error::Unsupported);
    #[cfg(target_os = "none")]
    {
        let cpu = cpu::current_logical_cpu_id();
        let source = readonly_pmus()
            .find(|source| source.pmu == pmu_kind)
            .ok_or(Error::Unsupported)?;
        if (source.package_scoped && source.owner_cpu != cpu)
            || (!source.package_scoped && source.owner_cpu != cpu)
        {
            return Err(Error::Migrated);
        }
        let msr = match (pmu_kind, config) {
            (ReadOnlyPmu::Msr, 0) => MSR_IA32_APERF,
            (ReadOnlyPmu::Msr, 1) => MSR_IA32_MPERF,
            (ReadOnlyPmu::Msr, 2) => return Ok(unsafe { core::arch::x86_64::_rdtsc() }),
            (ReadOnlyPmu::Power, 0) => MSR_PKG_ENERGY_STATUS,
            (ReadOnlyPmu::Power, 1) => MSR_PP0_ENERGY_STATUS,
            (ReadOnlyPmu::CoreCstate, 0) => MSR_CORE_C3_RESIDENCY,
            (ReadOnlyPmu::CoreCstate, 1) => MSR_CORE_C6_RESIDENCY,
            (ReadOnlyPmu::CoreCstate, 2) => MSR_CORE_C7_RESIDENCY,
            (ReadOnlyPmu::PackageCstate, 0) => MSR_PKG_C2_RESIDENCY,
            (ReadOnlyPmu::PackageCstate, 1) => MSR_PKG_C3_RESIDENCY,
            (ReadOnlyPmu::PackageCstate, 2) => MSR_PKG_C6_RESIDENCY,
            (ReadOnlyPmu::PackageCstate, 3) => MSR_PKG_C7_RESIDENCY,
            (ReadOnlyPmu::PackageCstate, 4) => MSR_PKG_C8_RESIDENCY,
            (ReadOnlyPmu::PackageCstate, 5) => MSR_PKG_C9_RESIDENCY,
            (ReadOnlyPmu::PackageCstate, 6) => MSR_PKG_C10_RESIDENCY,
            _ => return Err(Error::InvalidProgram),
        };
        Ok(unsafe { x86::msr::rdmsr(msr) })
    }
}

/// Read the immutable package RAPL unit for a published power-PMU owner.
///
/// `owner_cpu` is an identity token obtained from [`readonly_pmus`], not an
/// arbitrary target.  The first product release admits one package only, and
/// `MSR_RAPL_POWER_UNIT` is package-replicated, so the caller may read the
/// value while running on another CPU after proving that token names the
/// published package owner.  This keeps sysfs metadata available regardless
/// of the CPU on which the pseudo filesystem is mounted without widening the
/// actual energy-counter ownership rules.
pub fn rapl_power_unit_for_owner(owner_cpu: usize) -> Result<u64, Error> {
    #[cfg(not(target_os = "none"))]
    return Err(Error::Unsupported);
    #[cfg(target_os = "none")]
    {
        let power = readonly_pmus()
            .find(|source| source.pmu == ReadOnlyPmu::Power)
            .ok_or(Error::Unsupported)?;
        if !power_owner_is_published(power, owner_cpu) {
            return Err(Error::InvalidProgram);
        }
        Ok(unsafe { x86::msr::rdmsr(MSR_RAPL_POWER_UNIT) })
    }
}

fn power_owner_is_published(power: ReadOnlyPmuSnapshot, owner_cpu: usize) -> bool {
    power.pmu == ReadOnlyPmu::Power && power.package_scoped && power.owner_cpu == owner_cpu
}

/// Current-owner convenience wrapper for the code paths that already execute
/// on the power PMU owner CPU.
pub fn rapl_power_unit_current() -> Result<u64, Error> {
    let cpu = cpu::current_logical_cpu_id();
    rapl_power_unit_for_owner(cpu)
}

/// Publish a reconcile generation for the owning CPU.  A real perf owner
/// sends its existing `PerfReconcile` IPI after this publication.
pub fn publish_reconcile(owner_cpu: usize, generation: u64) -> Result<(), Error> {
    let mailbox = RECONCILE.get(owner_cpu).ok_or(Error::Unsupported)?;
    mailbox.generation.store(generation, Ordering::Release);
    Ok(())
}

/// Owner-side acknowledgement.  It is allocation-free and preserves all
/// foreign bits because this module restores/programs only whole registers it
/// previously saved from a discovery-bounded box.
pub fn reconcile_owner_current(generation: u64) -> Result<(), Error> {
    let cpu = cpu::current_logical_cpu_id();
    let mailbox = RECONCILE.get(cpu).ok_or(Error::Unsupported)?;
    if mailbox.generation.load(Ordering::Acquire) != generation {
        return Err(Error::Stale);
    }
    let result = match mailbox.operation.load(Ordering::Acquire) {
        RECONCILE_PROGRAM => program_raw_owner_current(
            mailbox.box_type.load(Ordering::Relaxed) as u16,
            mailbox.box_id.load(Ordering::Relaxed) as u16,
            mailbox.index.load(Ordering::Relaxed),
            mailbox.config.load(Ordering::Relaxed),
        )
        .map(|_| 0),
        RECONCILE_READ => read_counter_owner_current(
            mailbox.box_type.load(Ordering::Relaxed) as u16,
            mailbox.box_id.load(Ordering::Relaxed) as u16,
            mailbox.index.load(Ordering::Relaxed),
        ),
        RECONCILE_STOP_RESTORE => restore_owner_baseline_current().map(|_| 0),
        RECONCILE_RESERVE_PROGRAM => reserve_program_owner_current(
            mailbox.box_type.load(Ordering::Relaxed) as u16,
            mailbox.box_id.load(Ordering::Relaxed) as u16,
            mailbox.config.load(Ordering::Relaxed),
        )
        .map(|lease| {
            mailbox
                .lease_generation
                .store(lease.generation, Ordering::Relaxed);
            mailbox.lease_cookie.store(lease.cookie, Ordering::Relaxed);
            lease.counter as u64
        }),
        RECONCILE_READ_LEASE => read_lease_owner_current(UncoreLease {
            box_type: mailbox.box_type.load(Ordering::Relaxed) as u16,
            box_id: mailbox.box_id.load(Ordering::Relaxed) as u16,
            counter: mailbox.index.load(Ordering::Relaxed),
            generation: mailbox.lease_generation.load(Ordering::Relaxed),
            cookie: mailbox.lease_cookie.load(Ordering::Relaxed),
        }),
        RECONCILE_SETTLE_RELEASE => settle_release_owner_current(UncoreLease {
            box_type: mailbox.box_type.load(Ordering::Relaxed) as u16,
            box_id: mailbox.box_id.load(Ordering::Relaxed) as u16,
            counter: mailbox.index.load(Ordering::Relaxed),
            generation: mailbox.lease_generation.load(Ordering::Relaxed),
            cookie: mailbox.lease_cookie.load(Ordering::Relaxed),
        }),
        _ => Ok(0),
    };
    mailbox
        .result
        .store(result.unwrap_or(u64::MAX - 1), Ordering::Release);
    mailbox.operation.store(RECONCILE_NONE, Ordering::Release);
    mailbox.acknowledged.store(generation, Ordering::Release);
    Ok(())
}

/// Return the exact lease produced by a completed reserve/program mailbox.
pub fn reconcile_lease_result(owner_cpu: usize, generation: u64) -> Result<UncoreLease, Error> {
    let mailbox = RECONCILE.get(owner_cpu).ok_or(Error::Unsupported)?;
    if mailbox.acknowledged.load(Ordering::Acquire) != generation {
        return Err(Error::Stale);
    }
    let counter = reconcile_result(owner_cpu, generation)? as u8;
    Ok(UncoreLease {
        box_type: mailbox.box_type.load(Ordering::Acquire) as u16,
        box_id: mailbox.box_id.load(Ordering::Acquire) as u16,
        counter,
        generation: mailbox.lease_generation.load(Ordering::Acquire),
        cookie: mailbox.lease_cookie.load(Ordering::Acquire),
    })
}

pub fn reconcile_result(owner_cpu: usize, generation: u64) -> Result<u64, Error> {
    let mailbox = RECONCILE.get(owner_cpu).ok_or(Error::Unsupported)?;
    if mailbox.acknowledged.load(Ordering::Acquire) != generation {
        return Err(Error::Stale);
    }
    let result = mailbox.result.load(Ordering::Acquire);
    if result == u64::MAX - 1 {
        Err(Error::Unsupported)
    } else {
        Ok(result)
    }
}

pub fn reconcile_acknowledged(owner_cpu: usize) -> u64 {
    RECONCILE
        .get(owner_cpu)
        .map(|mailbox| mailbox.acknowledged.load(Ordering::Acquire))
        .unwrap_or(0)
}
pub fn reconcile_generation(owner_cpu: usize) -> u64 {
    RECONCILE
        .get(owner_cpu)
        .map(|mailbox| mailbox.generation.load(Ordering::Acquire))
        .unwrap_or(0)
}

/// Cancel an IPI payload that could not be sent.  This is intentionally
/// generation-qualified so an old caller cannot clear a newer owner request.
pub fn cancel_reconcile(owner_cpu: usize, generation: u64) {
    let Some(mailbox) = RECONCILE.get(owner_cpu) else {
        return;
    };
    if mailbox.generation.load(Ordering::Acquire) == generation
        && mailbox.acknowledged.load(Ordering::Acquire) != generation
    {
        mailbox.operation.store(RECONCILE_NONE, Ordering::Release);
    }
}

pub fn owner_cpu(box_type: u16, box_id: u16) -> Result<usize, Error> {
    BOXES
        .lock()
        .iter()
        .find(|state| {
            state.discovered
                && state.descriptor.box_type == box_type
                && state.descriptor.box_id == box_id
        })
        .map(|state| state.descriptor.owner_cpu)
        .ok_or(Error::Unsupported)
}

fn lease_is_current(lease: UncoreLease) -> Result<(), Error> {
    let boxes = BOXES.lock();
    let state = boxes
        .iter()
        .find(|state| {
            state.discovered
                && state.descriptor.box_type == lease.box_type
                && state.descriptor.box_id == lease.box_id
        })
        .ok_or(Error::Unsupported)?;
    if lease.counter >= state.descriptor.counters {
        return Err(Error::InvalidProgram);
    }
    let slot = state.leases[lease.counter as usize];
    if slot.generation != lease.generation || slot.cookie != lease.cookie || slot.generation == 0 {
        return Err(Error::Stale);
    }
    Ok(())
}

/// Reserve and program one free counter in a discovered box.  A lease never
/// aliases another event, even when both are controlled through the same
/// package-owner IPI mailbox.
pub fn reserve_program_owner_current(
    box_type: u16,
    box_id: u16,
    raw_config: u64,
) -> Result<UncoreLease, Error> {
    let cpu = cpu::current_logical_cpu_id();
    let lease = {
        let mut boxes = BOXES.lock();
        let state = boxes
            .iter_mut()
            .find(|state| {
                state.discovered
                    && state.descriptor.box_type == box_type
                    && state.descriptor.box_id == box_id
            })
            .ok_or(Error::Unsupported)?;
        if state.descriptor.owner_cpu != cpu {
            return Err(Error::Migrated);
        }
        if raw_config & !state.descriptor.event_mask != 0 {
            return Err(Error::InvalidProgram);
        }
        let counter = state.leases[..state.descriptor.counters as usize]
            .iter()
            .position(|slot| slot.generation == 0)
            .ok_or(Error::Busy)? as u8;
        let generation = NEXT_LEASE_WORD.fetch_add(1, Ordering::Relaxed).max(1);
        let cookie = NEXT_LEASE_WORD.fetch_add(1, Ordering::Relaxed).max(1);
        state.leases[counter as usize] = LeaseSlot { generation, cookie };
        UncoreLease {
            box_type,
            box_id,
            counter,
            generation,
            cookie,
        }
    };
    if let Err(error) = program_raw_owner_current(box_type, box_id, lease.counter, raw_config) {
        let mut boxes = BOXES.lock();
        if let Some(state) = boxes.iter_mut().find(|state| {
            state.discovered
                && state.descriptor.box_type == box_type
                && state.descriptor.box_id == box_id
        })
            && state.leases[lease.counter as usize].generation == lease.generation
        {
            state.leases[lease.counter as usize] = EMPTY_LEASE;
        }
        return Err(error);
    }
    Ok(lease)
}

pub fn read_lease_owner_current(lease: UncoreLease) -> Result<u64, Error> {
    lease_is_current(lease)?;
    read_counter_owner_current(lease.box_type, lease.box_id, lease.counter)
}

/// Settle exactly one owner lease, restore only its selector/counter baseline,
/// then make the counter available to another flexible group.
pub fn settle_release_owner_current(lease: UncoreLease) -> Result<u64, Error> {
    let value = read_lease_owner_current(lease)?;
    restore_counter_baseline_current(lease)?;
    let mut boxes = BOXES.lock();
    let state = boxes
        .iter_mut()
        .find(|state| {
            state.discovered
                && state.descriptor.box_type == lease.box_type
                && state.descriptor.box_id == lease.box_id
        })
        .ok_or(Error::Unsupported)?;
    if state.leases[lease.counter as usize].generation != lease.generation
        || state.leases[lease.counter as usize].cookie != lease.cookie
    {
        return Err(Error::Stale);
    }
    state.leases[lease.counter as usize] = EMPTY_LEASE;
    Ok(value)
}

/// Read one package counter on the preselected owner CPU.  Uncore never
/// supports task/cgroup attribution or sampling: callers must have admitted a
/// system-wide or owner-CPU counting event before reaching this API.
pub fn read_counter_owner_current(box_type: u16, box_id: u16, index: u8) -> Result<u64, Error> {
    #[cfg(not(target_os = "none"))]
    return Err(Error::Unsupported);
    #[cfg(target_os = "none")]
    {
        let cpu = cpu::current_logical_cpu_id();
        let boxes = BOXES.lock();
        let state = boxes
            .iter()
            .find(|state| {
                state.discovered
                    && state.descriptor.box_type == box_type
                    && state.descriptor.box_id == box_id
            })
            .ok_or(Error::Unsupported)?;
        if state.descriptor.owner_cpu != cpu || index >= state.descriptor.counters {
            return Err(Error::Migrated);
        }
        let offset = state.descriptor.ctr_offset as usize
            + index as usize
                * match state.descriptor.access {
                    AccessType::Msr => 1,
                    AccessType::Mmio | AccessType::Pci => 8,
                };
        match state.descriptor.access {
            AccessType::Msr => {
                Ok(unsafe { x86::msr::rdmsr(state.descriptor.counter_msr + index as u32) })
            }
            AccessType::Mmio => Ok(unsafe { mmio_read64(state.descriptor.control, offset) }),
            AccessType::Pci => {
                unsafe { pci_read64(state.descriptor.control, offset) }.ok_or(Error::Unsupported)
            }
        }
    }
}

/// Program one raw generic-un-core selector on the owner CPU.  The table has
/// already bounded the transport; this only accepts Linux's generic raw bits
/// and leaves all unrelated control bits untouched.
pub fn program_raw_owner_current(
    box_type: u16,
    box_id: u16,
    index: u8,
    raw_config: u64,
) -> Result<(), Error> {
    #[cfg(not(target_os = "none"))]
    return Err(Error::Unsupported);
    #[cfg(target_os = "none")]
    {
        let cpu = cpu::current_logical_cpu_id();
        let boxes = BOXES.lock();
        let state = boxes
            .iter()
            .find(|state| {
                state.discovered
                    && state.descriptor.box_type == box_type
                    && state.descriptor.box_id == box_id
            })
            .ok_or(Error::Unsupported)?;
        if state.descriptor.owner_cpu != cpu || index >= state.descriptor.counters {
            return Err(Error::Migrated);
        }
        if raw_config & !state.descriptor.event_mask != 0 {
            return Err(Error::InvalidProgram);
        }
        let offset = state.descriptor.ctl_offset as usize
            + index as usize
                * match state.descriptor.access {
                    AccessType::Msr => 1,
                    AccessType::Mmio => 4,
                    AccessType::Pci => 8,
                };
        match state.descriptor.access {
            AccessType::Msr => unsafe {
                let address = state.descriptor.config_msr + index as u32;
                let current = x86::msr::rdmsr(address);
                x86::msr::wrmsr(
                    address,
                    (current & !state.descriptor.config_mask) | raw_config,
                );
            },
            AccessType::Mmio => unsafe {
                let current = mmio_read32(state.descriptor.control, offset) as u64;
                mmio_write32(
                    state.descriptor.control,
                    offset,
                    ((current & !state.descriptor.config_mask) | raw_config) as u32,
                );
            },
            AccessType::Pci => unsafe {
                let current =
                    pci_read32(state.descriptor.control, offset).ok_or(Error::Unsupported)? as u64;
                if !pci_write32(
                    state.descriptor.control,
                    offset,
                    ((current & !state.descriptor.config_mask) | raw_config) as u32,
                ) {
                    return Err(Error::Unsupported);
                }
            },
        }
        Ok(())
    }
}

/// Restore one counter without modifying any selector bits outside the
/// discovery-granted mask.  Normal perf teardown uses this exact operation;
/// the owner-wide variant below is reserved for terminal kexec recovery.
fn restore_counter_baseline_current(lease: UncoreLease) -> Result<(), Error> {
    #[cfg(not(target_os = "none"))]
    {
        let _ = lease;
        return Err(Error::Unsupported);
    }
    #[cfg(target_os = "none")]
    {
        let cpu = cpu::current_logical_cpu_id();
        let (descriptor, saved) = {
            let boxes = BOXES.lock();
            let state = boxes
                .iter()
                .find(|state| {
                    state.discovered
                        && state.descriptor.box_type == lease.box_type
                        && state.descriptor.box_id == lease.box_id
                })
                .ok_or(Error::Unsupported)?;
            if state.descriptor.owner_cpu != cpu || lease.counter >= state.descriptor.counters {
                return Err(Error::Migrated);
            }
            (state.descriptor, state.baseline[lease.counter as usize])
        };
        let index = lease.counter as usize;
        let config_offset = descriptor.ctl_offset as usize
            + index
                * match descriptor.access {
                    AccessType::Msr => 1,
                    AccessType::Mmio => 4,
                    AccessType::Pci => 8,
                };
        let counter_offset = descriptor.ctr_offset as usize
            + index
                * match descriptor.access {
                    AccessType::Msr => 1,
                    AccessType::Mmio | AccessType::Pci => 8,
                };
        let config = saved.config & descriptor.config_mask;
        match descriptor.access {
            AccessType::Msr => unsafe {
                let address = descriptor.config_msr + lease.counter as u32;
                let current = x86::msr::rdmsr(address);
                x86::msr::wrmsr(address, (current & !descriptor.config_mask) | config);
                x86::msr::wrmsr(descriptor.counter_msr + lease.counter as u32, saved.value);
            },
            AccessType::Mmio => unsafe {
                let current = mmio_read32(descriptor.control, config_offset) as u64;
                mmio_write32(
                    descriptor.control,
                    config_offset,
                    ((current & !descriptor.config_mask) | config) as u32,
                );
                mmio_write64(descriptor.control, counter_offset, saved.value);
            },
            AccessType::Pci => unsafe {
                let current =
                    pci_read32(descriptor.control, config_offset).ok_or(Error::Unsupported)? as u64;
                if !pci_write32(
                    descriptor.control,
                    config_offset,
                    ((current & !descriptor.config_mask) | config) as u32,
                ) || !pci_write64(descriptor.control, counter_offset, saved.value)
                {
                    return Err(Error::Unsupported);
                }
            },
        }
        Ok(())
    }
}

/// Restore every counter/configuration pair claimed by this package owner.
/// The caller must run this before the owner's terminal kexec ACK.
pub fn restore_owner_baseline_current() -> Result<(), Error> {
    #[cfg(target_os = "none")]
    {
        let cpu = cpu::current_logical_cpu_id();
        let boxes = BOXES.lock();
        for state in boxes
            .iter()
            .filter(|state| state.discovered && state.descriptor.owner_cpu == cpu)
        {
            for index in 0..state.descriptor.counters as usize {
                let config_offset = state.descriptor.ctl_offset as usize
                    + index
                        * match state.descriptor.access {
                            AccessType::Msr => 1,
                            AccessType::Mmio => 4,
                            AccessType::Pci => 8,
                        };
                let counter_offset = state.descriptor.ctr_offset as usize
                    + index
                        * match state.descriptor.access {
                            AccessType::Msr => 1,
                            AccessType::Mmio | AccessType::Pci => 8,
                        };
                let config = state.baseline[index].config & state.descriptor.config_mask;
                match state.descriptor.access {
                    AccessType::Msr => unsafe {
                        let current = x86::msr::rdmsr(state.descriptor.config_msr + index as u32);
                        x86::msr::wrmsr(
                            state.descriptor.config_msr + index as u32,
                            (current & !state.descriptor.config_mask) | config,
                        );
                        x86::msr::wrmsr(
                            state.descriptor.counter_msr + index as u32,
                            state.baseline[index].value,
                        );
                    },
                    AccessType::Mmio => unsafe {
                        let current = mmio_read32(state.descriptor.control, config_offset) as u64;
                        mmio_write32(
                            state.descriptor.control,
                            config_offset,
                            ((current & !state.descriptor.config_mask) | config) as u32,
                        );
                        mmio_write64(
                            state.descriptor.control,
                            counter_offset,
                            state.baseline[index].value,
                        );
                    },
                    AccessType::Pci => unsafe {
                        let current = pci_read32(state.descriptor.control, config_offset)
                            .ok_or(Error::Unsupported)?
                            as u64;
                        if !pci_write32(
                            state.descriptor.control,
                            config_offset,
                            ((current & !state.descriptor.config_mask) | config) as u32,
                        ) || !pci_write64(
                            state.descriptor.control,
                            counter_offset,
                            state.baseline[index].value,
                        ) {
                            return Err(Error::Unsupported);
                        }
                    },
                }
            }
        }
    }
    Ok(())
}

/// Disable every discovered writable package counter owned by this CPU
/// without taking `BOXES` or any transport lock.  Discovery publishes the
/// descriptor atomics before the release-store to `BOX_COUNT`, making this
/// safe for a panic IPI even if normal uncore placement was interrupted.
pub fn crash_quiesce_owner_current() {
    #[cfg(target_os = "none")]
    {
        let cpu = cpu::current_logical_cpu_id();
        let count = BOX_COUNT.load(Ordering::Acquire).min(MAX_BOXES);
        for descriptor in &CRASH_BOXES[..count] {
            if descriptor.owner_cpu.load(Ordering::Relaxed) != cpu {
                continue;
            }
            let mask = descriptor.config_mask.load(Ordering::Relaxed);
            let control = descriptor.control.load(Ordering::Relaxed);
            let base = descriptor.config_msr.load(Ordering::Relaxed) as u32;
            let offset = descriptor.ctl_offset.load(Ordering::Relaxed) as usize;
            let counters = descriptor.counters.load(Ordering::Relaxed) as usize;
            let access = descriptor.access.load(Ordering::Relaxed);
            for index in 0..counters.min(MAX_COUNTERS_PER_BOX) {
                match access {
                    x if x == AccessType::Msr as u8 => unsafe {
                        let register = base + index as u32;
                        let current = x86::msr::rdmsr(register);
                        x86::msr::wrmsr(register, current & !mask);
                    },
                    x if x == AccessType::Mmio as u8 => unsafe {
                        let register = offset + index * 4;
                        let current = mmio_read32(control, register) as u64;
                        mmio_write32(control, register, (current & !mask) as u32);
                    },
                    x if x == AccessType::Pci as u8 => unsafe {
                        let register = offset + index * 8;
                        if let Some(current) = pci_read32(control, register) {
                            let _ = pci_write32(control, register, (current as u64 & !mask) as u32);
                        }
                    },
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_rejects_unbounded_or_multipackage_records() {
        let empty = DiscoveryBox::EMPTY;
        assert!(!empty.valid());
        let mut valid = DiscoveryBox {
            kind: UncoreKind::Uncore,
            name: "uncore_test",
            box_type: 1,
            box_id: 0,
            access: AccessType::Msr,
            control: 0x700,
            ctl_offset: 0,
            ctr_offset: 0,
            config_msr: 0x700,
            config_mask: 0xffff,
            counter_msr: 0x710,
            counters: 2,
            width: 48,
            event_mask: 0xff,
            package_id: 0,
            owner_cpu: 0,
        };
        assert!(valid.valid());
        valid.counters = (MAX_COUNTERS_PER_BOX + 1) as u8;
        assert!(!valid.valid());
    }

    #[test]
    fn reconcile_is_generation_exact() {
        let mailbox = ReconcileMailbox::new();
        mailbox.generation.store(8, Ordering::Release);
        assert_ne!(mailbox.generation.load(Ordering::Acquire), 7);
        mailbox.acknowledged.store(8, Ordering::Release);
        assert_eq!(mailbox.acknowledged.load(Ordering::Acquire), 8);
    }

    #[test]
    fn generic_discovery_decodes_only_bounded_msr_units() {
        // global: type=1, stride=3 qwords, one unit, MSR access.
        let global = 1 | (3 << 8) | (1 << 16);
        // one counter, ctl +8, 48-bit counter, ctr +16, MSR access.
        let unit = 1 | (8 << 8) | (48 << 16) | (16 << 24);
        let table = [global, 0x700, 0, unit, 0x700, 0x2a];
        let (decoded, count) = decode_discovery_table(&table, 0, 0).unwrap();
        assert_eq!(count, 1);
        let box0 = decoded[0].unwrap();
        assert_eq!(box0.config_msr, 0x708);
        assert_eq!(box0.counter_msr, 0x710);
        assert_eq!(box0.box_type, 0x2a);
    }

    #[test]
    fn unknown_or_non_msr_discovery_unit_is_not_published() {
        let global = 1 | (3 << 8) | (1 << 16);
        let mmio_unit = 1 | (8 << 8) | (48 << 16) | (16 << 24) | (1u64 << 62);
        let table = [global, 0x700, 0, mmio_unit, 0x700, 1];
        assert_eq!(decode_discovery_table(&table, 0, 0).unwrap().1, 0);
    }

    #[test]
    fn read_only_counter_wrap_and_rapl_unit_are_exact() {
        assert_eq!(wrapping_counter_delta(0xffff_fffe, 3, 32), 5);
        assert_eq!(wrapping_counter_delta(u64::MAX, 1, 64), 2);
        assert_eq!(rapl_energy_unit_q32(14 << 8), Some(1 << 18));
        // ENERGY_UNIT is a five-bit field. Bit 13 belongs to the next
        // POWER_UNIT field, so this raw MSR value has energy exponent zero
        // and must not be rejected as an invented sixth exponent bit.
        assert_eq!(rapl_energy_unit_q32(32 << 8), Some(1 << 32));
    }

    #[test]
    fn rapl_unit_rejects_an_owner_not_published_for_power() {
        let power = ReadOnlyPmuSnapshot {
            pmu: ReadOnlyPmu::Power,
            name: "power",
            package_scoped: true,
            owner_cpu: 2,
            package_id: 0,
        };
        assert!(power_owner_is_published(power, 2));
        assert!(!power_owner_is_published(power, 1));
        assert!(!power_owner_is_published(
            ReadOnlyPmuSnapshot {
                pmu: ReadOnlyPmu::CoreCstate,
                ..power
            },
            2,
        ));
    }
}
#[cfg(target_os = "none")]
fn capture_baseline(record: DiscoveryBox) -> [SavedCounter; MAX_COUNTERS_PER_BOX] {
    let mut baseline = [EMPTY_SAVED; MAX_COUNTERS_PER_BOX];
    for (index, saved) in baseline
        .iter_mut()
        .enumerate()
        .take(record.counters as usize)
    {
        let config_offset = record.ctl_offset as usize
            + index
                * match record.access {
                    AccessType::Msr => 1,
                    AccessType::Mmio => 4,
                    AccessType::Pci => 8,
                };
        let counter_offset = record.ctr_offset as usize
            + index
                * match record.access {
                    AccessType::Msr => 1,
                    AccessType::Mmio | AccessType::Pci => 8,
                };
        // All addresses originate from a fully bounded discovery record; no
        // probing or speculative access to model-specific state occurs here.
        match record.access {
            AccessType::Msr => {
                saved.value = unsafe { x86::msr::rdmsr(record.counter_msr + index as u32) };
                saved.config = unsafe { x86::msr::rdmsr(record.config_msr + index as u32) };
            }
            AccessType::Mmio => {
                saved.value = unsafe { mmio_read64(record.control, counter_offset) };
                saved.config = unsafe { mmio_read32(record.control, config_offset) } as u64;
            }
            AccessType::Pci => {
                saved.value = unsafe { pci_read64(record.control, counter_offset) }.unwrap_or(0);
                saved.config =
                    unsafe { pci_read32(record.control, config_offset) }.unwrap_or(0) as u64;
            }
        }
    }
    baseline
}

#[cfg(not(target_os = "none"))]
const fn capture_baseline(_: DiscoveryBox) -> [SavedCounter; MAX_COUNTERS_PER_BOX] {
    [EMPTY_SAVED; MAX_COUNTERS_PER_BOX]
}
