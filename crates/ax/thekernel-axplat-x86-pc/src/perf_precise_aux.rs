//! Panther Lake-only precise-sampling and AUX transport primitives.
//!
//! This deliberately is not a second perf policy engine.  It owns only the
//! machine state which is not architectural counter state: the preallocated
//! PEBS debug-store area, architectural LBR state, and Intel PT output.  The
//! kernel's perf core supplies already allocated buffers and performs record
//! publication in normal context.  In particular, none of the state below
//! allocates or takes a lock from NMI context.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use crate::pmu::{self, ProductClass};

const PT_CTL: u32 = 0x570;
const PT_STATUS: u32 = 0x571;
const PT_OUTPUT_BASE: u32 = 0x560;
const PT_OUTPUT_MASK_PTRS: u32 = 0x561;
const PT_CR3_MATCH: u32 = 0x572;
const PT_ADDR0_A: u32 = 0x580;
const PT_ADDR0_B: u32 = 0x581;
const DS_AREA: u32 = 0x600;
const PEBS_ENABLE: u32 = 0x3f1;
const DEBUGCTL: u32 = 0x1d9;
const LBR_SELECT: u32 = 0x1c8;
const LBR_FROM_0: u32 = 0x680;
const LBR_TO_0: u32 = 0x6c0;
const MAX_LBR_ENTRIES: usize = 32;

/// The one PEBS layout accepted by this product slice.  The byte offsets are
/// the fixed Panther Cove architectural basic record used for precise IP,
/// flags, data linear address and data source.  Unknown format values are
/// rejected rather than guessed at.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PebsFormat {
    PantherCoveBasic,
}

impl PebsFormat {
    pub const RECORD_BYTES: usize = 64;
    const fn from_perf_capabilities(value: u64) -> Option<Self> {
        // IA32_PERF_CAPABILITIES[15:8] is the PEBS record-format field.  The
        // Panther Cove basic record is format 4.  A format mismatch means we
        // cannot promise precise IP and must leave PEBS unavailable.
        match (value >> 8) & 0xff {
            4 => Some(Self::PantherCoveBasic),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PebsRecord {
    pub flags: u64,
    pub ip: u64,
    pub data_linear_address: u64,
    pub data_source: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Unsupported,
    InvalidBuffer,
    Busy,
    Stale,
}

/// Decode exactly one documented Panther Cove basic PEBS record.  This is
/// intentionally a pure operation so the NMI producer may copy a bounded
/// record to preallocated memory and defer decoding/publication.
pub fn decode_pebs_record(format: PebsFormat, bytes: &[u8]) -> Result<PebsRecord, Error> {
    if format != PebsFormat::PantherCoveBasic || bytes.len() != PebsFormat::RECORD_BYTES {
        return Err(Error::InvalidBuffer);
    }
    let word = |offset: usize| {
        let mut raw = [0u8; 8];
        raw.copy_from_slice(&bytes[offset..offset + 8]);
        u64::from_le_bytes(raw)
    };
    Ok(PebsRecord {
        flags: word(0),
        ip: word(8),
        data_linear_address: word(32),
        data_source: word(40),
    })
}

/// Precise-IP is admitted only when a committed Panther Lake PMU advertises
/// the one PEBS format above and the event is a PEBS-capable counter selected
/// by the PMU constraint solver.  Generic architectural PMUs never enter this
/// path.
pub fn precise_ip_admitted(pebs_counter_selected: bool) -> Result<PebsFormat, Error> {
    if !pebs_counter_selected {
        return Err(Error::Unsupported);
    }
    let snapshot = pmu::capability_snapshot().map_err(|_| Error::Unsupported)?;
    if snapshot.product != ProductClass::PantherLake {
        return Err(Error::Unsupported);
    }
    #[cfg(target_os = "none")]
    {
        let caps = unsafe { x86::msr::rdmsr(0x345) };
        PebsFormat::from_perf_capabilities(caps).ok_or(Error::Unsupported)
    }
    #[cfg(not(target_os = "none"))]
    Err(Error::Unsupported)
}

/// Preallocated debug-store buffer description.  The caller keeps the pages
/// pinned for the whole generation; this type never owns or frees memory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PebsBuffer {
    pub physical: u64,
    pub bytes: usize,
    /// Pinned, 64-byte-aligned IA32_DS_AREA descriptor page prepared by the
    /// kernel.  It is distinct from the PEBS output page so a close can
    /// restore the former DS owner before either allocation is released.
    pub ds_area_physical: u64,
    pub format: PebsFormat,
}

const MAX_PEBS_OWNERS: usize = 32;

/// One precise event's fixed NMI handoff.  It owns no memory: its buffer is
/// only a candidate for the CPU shared DS transport while it remains armed.
#[repr(C, align(64))]
struct PebsOwner {
    generation: AtomicU64,
    armed: AtomicBool,
    ready: AtomicBool,
    buffer: AtomicU64,
    ds_area: AtomicU64,
    bytes: AtomicU64,
    counter_bit: AtomicU64,
    words: [AtomicU64; PebsFormat::RECORD_BYTES / 8],
}
impl PebsOwner {
    const fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            armed: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            buffer: AtomicU64::new(0),
            ds_area: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            counter_bit: AtomicU64::new(0),
            words: [const { AtomicU64::new(0) }; PebsFormat::RECORD_BYTES / 8],
        }
    }
}

/// CPU-wide shared DS transport. Intel exposes one IA32_DS_AREA per logical
/// CPU, so several PEBS counters must share it; owner slots retain separate
/// record handoffs and are selected by overflow counter bit/cookie.
#[repr(C, align(64))]
struct PebsCpuTransport {
    buffer: AtomicU64,
    ds_area: AtomicU64,
    bytes: AtomicU64,
    saved_ds_area: AtomicU64,
    saved_pebs_enable: AtomicU64,
    capture_cursor: AtomicU64,
    owners: [PebsOwner; MAX_PEBS_OWNERS],
}
impl PebsCpuTransport {
    const fn new() -> Self {
        Self {
            buffer: AtomicU64::new(0),
            ds_area: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            saved_ds_area: AtomicU64::new(0),
            saved_pebs_enable: AtomicU64::new(0),
            capture_cursor: AtomicU64::new(0),
            owners: [const { PebsOwner::new() }; MAX_PEBS_OWNERS],
        }
    }
    fn owner(&self, generation: u64, counter_bit: u64) -> Option<&PebsOwner> {
        self.owners.iter().find(|owner| {
            owner.armed.load(Ordering::Acquire)
                && owner.generation.load(Ordering::Acquire) == generation
                && owner.counter_bit.load(Ordering::Acquire) == counter_bit
        })
    }
    fn mask(&self) -> u64 {
        self.owners
            .iter()
            .filter(|owner| owner.armed.load(Ordering::Acquire))
            .fold(0, |mask, owner| {
                mask | owner.counter_bit.load(Ordering::Relaxed)
            })
    }
    fn replacement(&self) -> Option<&PebsOwner> {
        self.owners
            .iter()
            .find(|owner| owner.armed.load(Ordering::Acquire))
    }
}
static PEBS_NMI: [PebsCpuTransport; crate::config::plat::MAX_CPU_NUM] =
    [const { PebsCpuTransport::new() }; crate::config::plat::MAX_CPU_NUM];

pub fn arm_pebs_local(buffer: PebsBuffer, generation: u64, counter_bit: u64) -> Result<(), Error> {
    buffer.validate()?;
    precise_ip_admitted(true)?;
    let slot = &PEBS_NMI[current_cpu()];
    if counter_bit == 0 || !counter_bit.is_power_of_two() {
        return Err(Error::InvalidBuffer);
    }
    let owner = slot
        .owners
        .iter()
        .find(|owner| {
            owner.armed.load(Ordering::Acquire)
                && owner.generation.load(Ordering::Acquire) == generation
        })
        .or_else(|| {
            slot.owners.iter().find(|owner| {
                owner
                    .armed
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            })
        })
        .ok_or(Error::Busy)?;
    owner.ready.store(false, Ordering::Relaxed);
    owner.buffer.store(buffer.physical, Ordering::Relaxed);
    owner
        .ds_area
        .store(buffer.ds_area_physical, Ordering::Relaxed);
    owner.bytes.store(buffer.bytes as u64, Ordering::Relaxed);
    owner.counter_bit.store(counter_bit, Ordering::Relaxed);
    owner.generation.store(generation, Ordering::Release);
    #[cfg(target_os = "none")]
    unsafe {
        use axplat::mem::{PhysAddr, phys_to_virt};
        let first = slot.mask() == counter_bit;
        let (active_buffer, active_ds, active_bytes) = if first {
            (
                buffer.physical,
                buffer.ds_area_physical,
                buffer.bytes as u64,
            )
        } else {
            (
                slot.buffer.load(Ordering::Acquire),
                slot.ds_area.load(Ordering::Acquire),
                slot.bytes.load(Ordering::Acquire),
            )
        };
        let ds = phys_to_virt(PhysAddr::from_usize(active_ds as usize))
            .as_mut_ptr()
            .cast::<u64>();
        // Intel DS_AREA: BTS base/index/max/threshold followed by PEBS
        // base/index/max/threshold.  The kernel owns both pinned pages.
        core::ptr::write_volatile(ds.add(0), 0);
        core::ptr::write_volatile(ds.add(1), 0);
        core::ptr::write_volatile(ds.add(2), 0);
        core::ptr::write_volatile(ds.add(3), 0);
        if first {
            core::ptr::write_volatile(ds.add(4), active_buffer);
            core::ptr::write_volatile(ds.add(5), active_buffer);
            core::ptr::write_volatile(ds.add(6), active_buffer + active_bytes);
            core::ptr::write_volatile(ds.add(7), active_buffer + PebsFormat::RECORD_BYTES as u64);
            slot.buffer.store(active_buffer, Ordering::Relaxed);
            slot.ds_area.store(active_ds, Ordering::Relaxed);
            slot.bytes.store(active_bytes, Ordering::Relaxed);
            slot.saved_ds_area
                .store(x86::msr::rdmsr(DS_AREA), Ordering::Relaxed);
            slot.saved_pebs_enable
                .store(x86::msr::rdmsr(PEBS_ENABLE), Ordering::Relaxed);
            x86::msr::wrmsr(DS_AREA, active_ds);
        }
        x86::msr::wrmsr(
            PEBS_ENABLE,
            slot.saved_pebs_enable.load(Ordering::Relaxed) | slot.mask(),
        );
    }
    Ok(())
}

pub fn capture_pebs_nmi(generation: u64, counter_bit: u64) -> Result<(), Error> {
    let slot = &PEBS_NMI[current_cpu()];
    let owner = slot.owner(generation, counter_bit).ok_or(Error::Stale)?;
    #[cfg(target_os = "none")]
    unsafe {
        use axplat::mem::{PhysAddr, phys_to_virt};
        let base = slot.buffer.load(Ordering::Relaxed);
        let bytes = slot.bytes.load(Ordering::Relaxed);
        let ds = x86::msr::rdmsr(DS_AREA);
        if ds != slot.ds_area.load(Ordering::Relaxed) {
            return Err(Error::Stale);
        }
        let index = core::ptr::read_volatile(
            phys_to_virt(PhysAddr::from_usize(ds as usize))
                .as_ptr()
                .cast::<u64>()
                .add(5),
        );
        if index < base + PebsFormat::RECORD_BYTES as u64 || index > base + bytes {
            return Err(Error::Stale);
        }
        let cursor = slot.capture_cursor.swap(index, Ordering::AcqRel);
        let record_index =
            if cursor >= base + PebsFormat::RECORD_BYTES as u64 && cursor <= base + bytes {
                cursor - PebsFormat::RECORD_BYTES as u64
            } else {
                index - PebsFormat::RECORD_BYTES as u64
            };
        let record = phys_to_virt(PhysAddr::from_usize(record_index as usize))
            .as_ptr()
            .cast::<u64>();
        for (index, word) in owner.words.iter().enumerate() {
            word.store(
                core::ptr::read_volatile(record.add(index)),
                Ordering::Relaxed,
            );
        }
    }
    owner.ready.store(true, Ordering::Release);
    Ok(())
}

pub fn take_pebs_record_local(
    generation: u64,
) -> Result<Option<[u8; PebsFormat::RECORD_BYTES]>, Error> {
    let slot = &PEBS_NMI[current_cpu()];
    let owner = slot
        .owners
        .iter()
        .find(|owner| {
            owner.armed.load(Ordering::Acquire)
                && owner.generation.load(Ordering::Acquire) == generation
        })
        .ok_or(Error::Stale)?;
    if !owner.ready.swap(false, Ordering::AcqRel) {
        return Ok(None);
    }
    let mut result = [0u8; PebsFormat::RECORD_BYTES];
    for (bytes, word) in result.as_chunks_mut::<8>().0.iter_mut().zip(owner.words.iter()) {
        bytes.copy_from_slice(&word.load(Ordering::Acquire).to_le_bytes());
    }
    Ok(Some(result))
}

pub fn disarm_pebs_local(generation: u64) -> Result<(), Error> {
    let slot = &PEBS_NMI[current_cpu()];
    let owner = slot
        .owners
        .iter()
        .find(|owner| {
            owner.generation.load(Ordering::Acquire) == generation
                && owner.armed.load(Ordering::Acquire)
        })
        .ok_or(Error::Stale)?;
    owner.armed.store(false, Ordering::Release);
    owner.ready.store(false, Ordering::Release);
    #[cfg(target_os = "none")]
    unsafe {
        let mask = slot.mask();
        if mask == 0 {
            x86::msr::wrmsr(PEBS_ENABLE, slot.saved_pebs_enable.load(Ordering::Relaxed));
            x86::msr::wrmsr(DS_AREA, slot.saved_ds_area.load(Ordering::Relaxed));
        } else {
            // If the departing owner supplied the active DS backing, transfer
            // it to a still-armed owner's pinned page before its mapping may
            // be released.  This is normal close context, never NMI.
            if slot.buffer.load(Ordering::Acquire) == owner.buffer.load(Ordering::Acquire)
                && let Some(replacement) = slot.replacement()
            {
                let buffer = replacement.buffer.load(Ordering::Relaxed);
                let ds_area = replacement.ds_area.load(Ordering::Relaxed);
                let bytes = replacement.bytes.load(Ordering::Relaxed);
                use axplat::mem::{PhysAddr, phys_to_virt};
                let ds = phys_to_virt(PhysAddr::from_usize(ds_area as usize))
                    .as_mut_ptr()
                    .cast::<u64>();
                core::ptr::write_volatile(ds.add(4), buffer);
                core::ptr::write_volatile(ds.add(5), buffer);
                core::ptr::write_volatile(ds.add(6), buffer + bytes);
                core::ptr::write_volatile(ds.add(7), buffer + PebsFormat::RECORD_BYTES as u64);
                slot.buffer.store(buffer, Ordering::Release);
                slot.ds_area.store(ds_area, Ordering::Release);
                slot.bytes.store(bytes, Ordering::Release);
                slot.capture_cursor.store(0, Ordering::Release);
                x86::msr::wrmsr(DS_AREA, ds_area);
            }
            x86::msr::wrmsr(
                PEBS_ENABLE,
                slot.saved_pebs_enable.load(Ordering::Relaxed) | mask,
            );
        }
    }
    Ok(())
}

impl PebsBuffer {
    pub fn validate(self) -> Result<(), Error> {
        if self.physical & 0x3f != 0
            || self.ds_area_physical & 0x3f != 0
            || self.bytes < PebsFormat::RECORD_BYTES
            || !self.bytes.is_multiple_of(PebsFormat::RECORD_BYTES)
        {
            return Err(Error::InvalidBuffer);
        }
        Ok(())
    }
}

/// A single architectural LBR is a CPU resource, never a per-task cache.
/// The generation prevents an old close/reconcile acknowledgement from
/// clearing a newer owner.
#[repr(C, align(64))]
struct LbrOwner {
    generation: AtomicU64,
    domain: AtomicU64,
    held: AtomicBool,
}
impl LbrOwner {
    const fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            domain: AtomicU64::new(0),
            held: AtomicBool::new(false),
        }
    }
}
static LBR_OWNERS: [LbrOwner; crate::config::plat::MAX_CPU_NUM] =
    [const { LbrOwner::new() }; crate::config::plat::MAX_CPU_NUM];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LbrToken {
    cpu: usize,
    generation: u64,
    domain: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LbrEntry {
    pub from: u64,
    pub to: u64,
}

pub fn acquire_lbr_local(domain: u64, generation: u64) -> Result<LbrToken, Error> {
    require_panther_lake()?;
    let cpu = current_cpu();
    let owner = &LBR_OWNERS[cpu];
    if owner
        .held
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(Error::Busy);
    }
    owner.domain.store(domain, Ordering::Release);
    owner.generation.store(generation, Ordering::Release);
    #[cfg(target_os = "none")]
    unsafe {
        x86::msr::wrmsr(DEBUGCTL, x86::msr::rdmsr(DEBUGCTL) | 1);
    }
    Ok(LbrToken {
        cpu,
        generation,
        domain,
    })
}

/// Snapshot architectural LBR entries in task context.  The caller owns the
/// singleton token, so the buffer cannot observe a foreign security domain.
pub fn read_lbr_local(token: LbrToken, out: &mut [LbrEntry]) -> Result<usize, Error> {
    if token.cpu != current_cpu() {
        return Err(Error::Stale);
    }
    let owner = &LBR_OWNERS[token.cpu];
    if !owner.held.load(Ordering::Acquire)
        || owner.generation.load(Ordering::Acquire) != token.generation
        || owner.domain.load(Ordering::Acquire) != token.domain
    {
        return Err(Error::Stale);
    }
    #[cfg(target_os = "none")]
    {
        let count = out.len().min(MAX_LBR_ENTRIES);
        for (index, entry) in out.iter_mut().take(count).enumerate() {
            unsafe {
                entry.from = x86::msr::rdmsr(LBR_FROM_0 + index as u32);
                entry.to = x86::msr::rdmsr(LBR_TO_0 + index as u32);
            }
        }
        Ok(count)
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = out;
        Err(Error::Unsupported)
    }
}

/// Clear branch history before a different security domain can execute.  This
/// is allocation-free and lock-free, so scheduler switch code may call it.
pub fn clear_lbr_on_domain_switch(previous: u64, next: u64) {
    if previous == next {
        return;
    }
    #[cfg(target_os = "none")]
    unsafe {
        x86::msr::wrmsr(DEBUGCTL, 0);
        x86::msr::wrmsr(LBR_SELECT, 0);
        for i in 0..MAX_LBR_ENTRIES as u32 {
            x86::msr::wrmsr(LBR_FROM_0 + i, 0);
            x86::msr::wrmsr(LBR_TO_0 + i, 0);
        }
    }
}

pub fn release_lbr_local(token: LbrToken) -> Result<(), Error> {
    if token.cpu != current_cpu() {
        return Err(Error::Stale);
    }
    let owner = &LBR_OWNERS[token.cpu];
    if owner.generation.load(Ordering::Acquire) != token.generation
        || owner.domain.load(Ordering::Acquire) != token.domain
    {
        return Err(Error::Stale);
    }
    clear_lbr_on_domain_switch(token.domain, 0);
    owner.held.store(false, Ordering::Release);
    Ok(())
}

/// A ToPA entry.  This small model exposes wrap arithmetic separately from
/// hardware programming, making overwrite/snapshot behavior testable without
/// a PT-capable CPU.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TopaLayout {
    pub data_bytes: usize,
    pub page_bytes: usize,
}
impl TopaLayout {
    pub fn validate(self) -> Result<(), Error> {
        if self.page_bytes != 4096 || self.data_bytes == 0 || !self.data_bytes.is_multiple_of(self.page_bytes)
        {
            Err(Error::InvalidBuffer)
        } else {
            Ok(())
        }
    }
    pub fn offset(self, raw: u64) -> Result<usize, Error> {
        self.validate()?;
        Ok((raw as usize) % self.data_bytes)
    }
    pub fn distance(self, tail: u64, head: u64) -> Result<usize, Error> {
        self.validate()?;
        Ok(((head.wrapping_sub(tail)) as usize).min(self.data_bytes))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuxMode {
    Snapshot,
    Overwrite,
}

/// Intel PT is preferred when CPUID advertises it.  BTS is retained only as a
/// debug-store fallback on the same Panther Lake gate; unrecognised processors
/// receive neither backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuxBackend {
    IntelPt,
    Bts,
}

// IA32_RTIT_CTL bits exposed by Linux's intel_pt PMU.  TraceEn, OS, USR,
// CR3Filter and ToPA are kernel-owned transport/state bits and are therefore
// deliberately absent from the userspace config mask.
const RTIT_CTL_TRACE_EN: u64 = 1 << 0;
const RTIT_CTL_CYC_EN: u64 = 1 << 1;
const RTIT_CTL_OS: u64 = 1 << 2;
const RTIT_CTL_USR: u64 = 1 << 3;
const RTIT_CTL_TOPA: u64 = 1 << 5;
const RTIT_CTL_MTC_EN: u64 = 1 << 9;
const RTIT_CTL_TSC_EN: u64 = 1 << 10;
const RTIT_CTL_DISRETC: u64 = 1 << 11;
const RTIT_CTL_PTW_EN: u64 = 1 << 12;
const RTIT_CTL_BRANCH_EN: u64 = 1 << 13;
const RTIT_CTL_MTC_RANGE: u64 = 0xf << 14;
const RTIT_CTL_CYC_THRESH: u64 = 0xf << 19;
const RTIT_CTL_PSB_FREQ: u64 = 0xf << 24;

// This is precisely Linux's intel_pt config surface, plus no invented
// config1/config2 extension.  Address filters are installed through
// PERF_EVENT_IOC_SET_FILTER and programmed as IA32_RTIT_ADDRn_A/B, not by
// reinterpreting config1/config2 which Linux reserves for PMU-specific use.
const PT_USER_CONFIG_MASK: u64 = RTIT_CTL_TRACE_EN
    | RTIT_CTL_CYC_EN
    | RTIT_CTL_MTC_EN
    | RTIT_CTL_TSC_EN
    | RTIT_CTL_DISRETC
    | RTIT_CTL_PTW_EN
    | RTIT_CTL_BRANCH_EN
    | RTIT_CTL_MTC_RANGE
    | RTIT_CTL_CYC_THRESH
    | RTIT_CTL_PSB_FREQ;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PtAddressFilter {
    /// Inclusive virtual address at which tracing starts.
    pub start: u64,
    /// Exclusive virtual address at which tracing stops.
    pub end: u64,
}

impl PtAddressFilter {
    pub fn validate(self) -> Result<(), Error> {
        if self.start >= self.end
            || !is_canonical_address(self.start)
            || !is_canonical_address(self.end - 1)
        {
            return Err(Error::InvalidBuffer);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PtConfig {
    /// Linux intel_pt's config word, excluding kernel-owned TraceEn/ToPA and
    /// privilege/context bits.
    pub config: u64,
    pub trace_user: bool,
    pub trace_kernel: bool,
    /// A numeric PERF_EVENT_IOC_SET_FILTER address range.  One range is the
    /// committed bounded implementation; a second range is rejected before
    /// programming rather than silently omitted.
    pub address_filter: Option<PtAddressFilter>,
}

impl PtConfig {
    pub fn validate(self) -> Result<(), Error> {
        require_panther_lake_pt()?;
        if !self.trace_user && !self.trace_kernel || self.config & !PT_USER_CONFIG_MASK != 0 {
            return Err(Error::InvalidBuffer);
        }
        // Linux keeps the legacy implicit BranchEn default unless bit 0
        // (passthrough) is present.  Supplying BranchEn without passthrough
        // is invalid, not a request we can guess about.
        if self.config & RTIT_CTL_TRACE_EN == 0 && self.config & RTIT_CTL_BRANCH_EN != 0 {
            return Err(Error::InvalidBuffer);
        }
        #[cfg(target_os = "none")]
        {
            use core::arch::x86_64::__cpuid_count;
            let caps0 = __cpuid_count(0x14, 0);
            let caps1 = __cpuid_count(0x14, 1);
            let requested_psb = (self.config & RTIT_CTL_PSB_FREQ) >> 24;
            let requested_cyc = (self.config & RTIT_CTL_CYC_THRESH) >> 19;
            let requested_mtc = (self.config & RTIT_CTL_MTC_RANGE) >> 14;
            if self.config & (RTIT_CTL_CYC_EN | RTIT_CTL_CYC_THRESH | RTIT_CTL_PSB_FREQ) != 0
                && (caps0.ebx & (1 << 1) == 0
                    || (requested_psb != 0 && caps1.ebx & (1 << (requested_psb + 16)) == 0)
                    || (requested_cyc != 0 && caps1.ebx & (1 << requested_cyc) == 0))
            {
                return Err(Error::Unsupported);
            }
            if self.config & (RTIT_CTL_MTC_EN | RTIT_CTL_MTC_RANGE) != 0
                && (caps0.ebx & (1 << 3) == 0
                    || caps1.eax & 0xffff_0000 == 0
                    || caps1.eax & (1 << (requested_mtc + 16)) == 0)
            {
                return Err(Error::Unsupported);
            }
            if self.config & RTIT_CTL_PTW_EN != 0 && caps0.ebx & (1 << 4) == 0 {
                return Err(Error::Unsupported);
            }
            if self.address_filter.is_some() && (caps0.ebx & (1 << 2) == 0 || caps1.eax & 0x7 == 0)
            {
                return Err(Error::Unsupported);
            }
        }
        if let Some(filter) = self.address_filter {
            filter.validate()?;
        }
        Ok(())
    }

    fn control(self) -> u64 {
        let mut ctl = self.config & PT_USER_CONFIG_MASK;
        if self.config & RTIT_CTL_TRACE_EN == 0 {
            ctl |= RTIT_CTL_BRANCH_EN;
        }
        if self.trace_user {
            ctl |= RTIT_CTL_USR;
        }
        if self.trace_kernel {
            ctl |= RTIT_CTL_OS;
        }
        ctl | RTIT_CTL_TRACE_EN | RTIT_CTL_TOPA
    }
}

/// Validate the actual Linux perf attr transport for Intel PT.  Linux's
/// intel_pt PMU uses only `config`; config1/config2 are not an alternate
/// address-filter ABI, so accepting them would silently broaden the trace.
pub fn validate_pt_attr(
    config: u64,
    config1: u64,
    config2: u64,
    trace_user: bool,
    trace_kernel: bool,
) -> Result<(), Error> {
    if config1 != 0 || config2 != 0 {
        return Err(Error::InvalidBuffer);
    }
    PtConfig {
        config,
        trace_user,
        trace_kernel,
        address_filter: None,
    }
    .validate()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BtsBuffer {
    pub physical: u64,
    pub bytes: usize,
}

/// A caller-preallocated debug-store descriptor and BTS output area.  The
/// descriptor is the architectural `IA32_DS_AREA` object (base/index/max/
/// threshold); it must remain pinned through the active generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BtsProgram {
    pub ds_area_physical: u64,
    pub buffer: BtsBuffer,
    pub generation: u64,
}

#[repr(C, align(64))]
struct BtsState {
    generation: AtomicU64,
    active: AtomicBool,
    saved_ds_area: AtomicU64,
    saved_debugctl: AtomicU64,
    buffer_base: AtomicU64,
    buffer_bytes: AtomicU64,
    ds_area: AtomicU64,
}
impl BtsState {
    const fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            active: AtomicBool::new(false),
            saved_ds_area: AtomicU64::new(0),
            saved_debugctl: AtomicU64::new(0),
            buffer_base: AtomicU64::new(0),
            buffer_bytes: AtomicU64::new(0),
            ds_area: AtomicU64::new(0),
        }
    }
}
static BTS: [BtsState; crate::config::plat::MAX_CPU_NUM] =
    [const { BtsState::new() }; crate::config::plat::MAX_CPU_NUM];

/// Enable BTS only when PT is absent.  The descriptor contents are prepared
/// by the kernel before this call; this local routine merely establishes and
/// later restores the MSR ownership boundary.
pub fn start_bts_local(program: BtsProgram) -> Result<(), Error> {
    if discover_aux_backend()? != AuxBackend::Bts {
        return Err(Error::Unsupported);
    }
    program.buffer.validate()?;
    if program.ds_area_physical & 0x3f != 0 {
        return Err(Error::InvalidBuffer);
    }
    let state = &BTS[current_cpu()];
    if state
        .active
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(Error::Busy);
    }
    state
        .generation
        .store(program.generation, Ordering::Release);
    state
        .buffer_base
        .store(program.buffer.physical, Ordering::Release);
    state
        .buffer_bytes
        .store(program.buffer.bytes as u64, Ordering::Release);
    state
        .ds_area
        .store(program.ds_area_physical, Ordering::Release);
    #[cfg(target_os = "none")]
    unsafe {
        use axplat::mem::{PhysAddr, phys_to_virt};
        let ds = phys_to_virt(PhysAddr::from_usize(program.ds_area_physical as usize))
            .as_mut_ptr()
            .cast::<u64>();
        // IA32_DS_AREA BTS base/index/max/threshold.  The output backing is
        // one physically-contiguous, pinned range supplied by the kernel.
        core::ptr::write_volatile(ds.add(0), program.buffer.physical);
        core::ptr::write_volatile(ds.add(1), program.buffer.physical);
        core::ptr::write_volatile(
            ds.add(2),
            program.buffer.physical + program.buffer.bytes as u64,
        );
        core::ptr::write_volatile(ds.add(3), program.buffer.physical + 24);
        state
            .saved_ds_area
            .store(x86::msr::rdmsr(DS_AREA), Ordering::Relaxed);
        state
            .saved_debugctl
            .store(x86::msr::rdmsr(DEBUGCTL), Ordering::Relaxed);
        x86::msr::wrmsr(
            DEBUGCTL,
            state.saved_debugctl.load(Ordering::Relaxed) & !(1 << 7),
        );
        x86::msr::wrmsr(DS_AREA, program.ds_area_physical);
        x86::msr::wrmsr(
            DEBUGCTL,
            state.saved_debugctl.load(Ordering::Relaxed) | (1 << 7),
        );
    }
    Ok(())
}

pub fn stop_bts_local(generation: u64) -> Result<AuxMetadata, Error> {
    let state = &BTS[current_cpu()];
    if !state.active.swap(false, Ordering::AcqRel)
        || state.generation.load(Ordering::Acquire) != generation
    {
        return Err(Error::Stale);
    }
    let base = state.buffer_base.load(Ordering::Acquire);
    let bytes = state.buffer_bytes.load(Ordering::Acquire);
    #[cfg(target_os = "none")]
    let index = unsafe {
        use axplat::mem::{PhysAddr, phys_to_virt};
        core::ptr::read_volatile(
            phys_to_virt(PhysAddr::from_usize(
                state.ds_area.load(Ordering::Relaxed) as usize
            ))
            .as_ptr()
            .cast::<u64>()
            .add(1),
        )
    };
    #[cfg(not(target_os = "none"))]
    let index = base;
    #[cfg(target_os = "none")]
    unsafe {
        x86::msr::wrmsr(DEBUGCTL, state.saved_debugctl.load(Ordering::Relaxed));
        x86::msr::wrmsr(DS_AREA, state.saved_ds_area.load(Ordering::Relaxed));
    }
    let size = index.saturating_sub(base).min(bytes) as usize;
    Ok(AuxMetadata {
        offset: 0,
        size,
        truncated: index > base + bytes,
        generation,
    })
}
impl BtsBuffer {
    pub fn validate(self) -> Result<(), Error> {
        if self.physical & 0x3f != 0 || self.bytes == 0 || !self.bytes.is_multiple_of(24) {
            Err(Error::InvalidBuffer)
        } else {
            Ok(())
        }
    }
}

pub fn choose_aux_backend(
    pt_advertised: bool,
    debug_store_advertised: bool,
) -> Result<AuxBackend, Error> {
    require_panther_lake()?;
    if pt_advertised {
        Ok(AuxBackend::IntelPt)
    } else if debug_store_advertised {
        Ok(AuxBackend::Bts)
    } else {
        Err(Error::Unsupported)
    }
}

/// Architectural LBR is a distinct CPUID leaf.  Do not infer support from a
/// generic PMU or from Debug Store support.
pub fn lbr_supported() -> bool {
    if require_panther_lake().is_err() {
        return false;
    }
    #[cfg(target_os = "none")]
    {
        use core::arch::x86_64::__cpuid_count;
        __cpuid_count(0, 0).eax >= 0x1c && __cpuid_count(0x1c, 0).eax != 0
    }
    #[cfg(not(target_os = "none"))]
    false
}

/// Discover the hardware transport rather than treating a requested AUX map
/// as evidence that PT exists.  CPUID.14 advertises Intel PT; BTS is used
/// only when PT is absent and debug-store support is explicitly advertised.
pub fn discover_aux_backend() -> Result<AuxBackend, Error> {
    #[cfg(target_os = "none")]
    {
        use core::arch::x86_64::__cpuid_count;

        let leaf0 = __cpuid_count(0, 0);
        let pt_advertised = leaf0.eax >= 0x14 && __cpuid_count(0x14, 0).eax != 0;
        let leaf1 = __cpuid_count(1, 0);
        let debug_store_advertised = leaf1.edx & (1 << 21) != 0;
        choose_aux_backend(pt_advertised, debug_store_advertised)
    }
    #[cfg(not(target_os = "none"))]
    Err(Error::Unsupported)
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuxMetadata {
    pub offset: u64,
    pub size: usize,
    pub truncated: bool,
    pub generation: u64,
}

#[repr(C, align(64))]
struct PtState {
    generation: AtomicU64,
    active: AtomicBool,
    saved_ctl: AtomicU64,
    saved_base: AtomicU64,
    saved_mask_ptrs: AtomicU64,
    saved_status: AtomicU64,
    saved_cr3_match: AtomicU64,
    saved_addr0_a: AtomicU64,
    saved_addr0_b: AtomicU64,
    data_bytes: AtomicU64,
    mode: AtomicU8,
}
impl PtState {
    const fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            active: AtomicBool::new(false),
            saved_ctl: AtomicU64::new(0),
            saved_base: AtomicU64::new(0),
            saved_mask_ptrs: AtomicU64::new(0),
            saved_status: AtomicU64::new(0),
            saved_cr3_match: AtomicU64::new(0),
            saved_addr0_a: AtomicU64::new(0),
            saved_addr0_b: AtomicU64::new(0),
            data_bytes: AtomicU64::new(0),
            mode: AtomicU8::new(0),
        }
    }
}
static PT: [PtState; crate::config::plat::MAX_CPU_NUM] =
    [const { PtState::new() }; crate::config::plat::MAX_CPU_NUM];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PtProgram {
    pub topa_physical: u64,
    pub layout: TopaLayout,
    pub mode: AuxMode,
    pub generation: u64,
    pub config: PtConfig,
}

/// Program PT only with a caller-preallocated ToPA table.  The caller is
/// responsible for constructing legal ToPA entries and keeping both table and
/// output pages pinned.  This function contains no allocation or locks.
pub fn start_pt_local(program: PtProgram) -> Result<(), Error> {
    require_panther_lake_pt()?;
    program.layout.validate()?;
    program.config.validate()?;
    if program.topa_physical & 0xfff != 0 {
        return Err(Error::InvalidBuffer);
    }
    let state = &PT[current_cpu()];
    if state
        .active
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(Error::Busy);
    }
    state
        .generation
        .store(program.generation, Ordering::Release);
    state.mode.store(
        matches!(program.mode, AuxMode::Overwrite) as u8,
        Ordering::Release,
    );
    state
        .data_bytes
        .store(program.layout.data_bytes as u64, Ordering::Release);
    #[cfg(target_os = "none")]
    unsafe {
        state
            .saved_ctl
            .store(x86::msr::rdmsr(PT_CTL), Ordering::Relaxed);
        state
            .saved_base
            .store(x86::msr::rdmsr(PT_OUTPUT_BASE), Ordering::Relaxed);
        state
            .saved_mask_ptrs
            .store(x86::msr::rdmsr(PT_OUTPUT_MASK_PTRS), Ordering::Relaxed);
        state
            .saved_status
            .store(x86::msr::rdmsr(PT_STATUS), Ordering::Relaxed);
        state
            .saved_cr3_match
            .store(x86::msr::rdmsr(PT_CR3_MATCH), Ordering::Relaxed);
        state
            .saved_addr0_a
            .store(x86::msr::rdmsr(PT_ADDR0_A), Ordering::Relaxed);
        state
            .saved_addr0_b
            .store(x86::msr::rdmsr(PT_ADDR0_B), Ordering::Relaxed);
        x86::msr::wrmsr(PT_CTL, 0);
        x86::msr::wrmsr(PT_OUTPUT_BASE, program.topa_physical);
        // ToPA is selected in RTIT_CTL; OUTPUT_MASK_PTRS starts at offset 0.
        x86::msr::wrmsr(PT_OUTPUT_MASK_PTRS, 0);
        x86::msr::wrmsr(PT_STATUS, 0);
        let mut ctl = program.config.control();
        if let Some(filter) = program.config.address_filter {
            x86::msr::wrmsr(PT_ADDR0_A, filter.start);
            x86::msr::wrmsr(PT_ADDR0_B, filter.end);
            // One hardware FILTER range: trace only while RIP is inside the
            // programmed interval.  The range count was checked above.
            ctl |= 1 << 32;
        }
        x86::msr::wrmsr(PT_CTL, ctl);
    }
    Ok(())
}

/// Stop one generation and return the data-ring-independent AUX descriptor.
/// A stale close cannot stop an event newly placed on the same CPU.
pub fn stop_pt_local(generation: u64, tail: u64) -> Result<AuxMetadata, Error> {
    let state = &PT[current_cpu()];
    if !state.active.load(Ordering::Acquire)
        || state.generation.load(Ordering::Acquire) != generation
    {
        return Err(Error::Stale);
    }
    #[cfg(target_os = "none")]
    let raw = unsafe { x86::msr::rdmsr(PT_OUTPUT_MASK_PTRS) };
    #[cfg(not(target_os = "none"))]
    let raw = 0;
    state.active.store(false, Ordering::Release);
    #[cfg(target_os = "none")]
    unsafe {
        x86::msr::wrmsr(PT_CTL, state.saved_ctl.load(Ordering::Relaxed));
        x86::msr::wrmsr(PT_OUTPUT_BASE, state.saved_base.load(Ordering::Relaxed));
        x86::msr::wrmsr(
            PT_OUTPUT_MASK_PTRS,
            state.saved_mask_ptrs.load(Ordering::Relaxed),
        );
        x86::msr::wrmsr(PT_STATUS, state.saved_status.load(Ordering::Relaxed));
        x86::msr::wrmsr(PT_CR3_MATCH, state.saved_cr3_match.load(Ordering::Relaxed));
        x86::msr::wrmsr(PT_ADDR0_A, state.saved_addr0_a.load(Ordering::Relaxed));
        x86::msr::wrmsr(PT_ADDR0_B, state.saved_addr0_b.load(Ordering::Relaxed));
    }
    let layout = TopaLayout {
        data_bytes: state.data_bytes.load(Ordering::Acquire) as usize,
        page_bytes: 4096,
    };
    let head = layout.offset(raw & u32::MAX as u64)? as u64;
    Ok(AuxMetadata {
        offset: head,
        size: layout.distance(tail, head)?,
        truncated: state.mode.load(Ordering::Acquire) != 0,
        generation,
    })
}

/// Terminal kexec hook.  It never waits and only restores registers captured
/// by `start_pt_local`; callers rendezvous CPUs before transferring control.
pub fn quiesce_pt_for_kexec_local() {
    let state = &PT[current_cpu()];
    if !state.active.swap(false, Ordering::AcqRel) {
        return;
    }
    #[cfg(target_os = "none")]
    unsafe {
        x86::msr::wrmsr(PT_CTL, state.saved_ctl.load(Ordering::Relaxed));
        x86::msr::wrmsr(PT_OUTPUT_BASE, state.saved_base.load(Ordering::Relaxed));
        x86::msr::wrmsr(
            PT_OUTPUT_MASK_PTRS,
            state.saved_mask_ptrs.load(Ordering::Relaxed),
        );
        x86::msr::wrmsr(PT_STATUS, state.saved_status.load(Ordering::Relaxed));
        x86::msr::wrmsr(PT_CR3_MATCH, state.saved_cr3_match.load(Ordering::Relaxed));
        x86::msr::wrmsr(PT_ADDR0_A, state.saved_addr0_a.load(Ordering::Relaxed));
        x86::msr::wrmsr(PT_ADDR0_B, state.saved_addr0_b.load(Ordering::Relaxed));
    }
    state.generation.fetch_add(1, Ordering::AcqRel);
}

/// Restore the debug-store ownership baseline before the kexec rendezvous
/// acknowledgement.  As with PT, this is terminal and never waits.
pub fn quiesce_bts_for_kexec_local() {
    let state = &BTS[current_cpu()];
    if !state.active.swap(false, Ordering::AcqRel) {
        return;
    }
    #[cfg(target_os = "none")]
    unsafe {
        x86::msr::wrmsr(DEBUGCTL, state.saved_debugctl.load(Ordering::Relaxed));
        x86::msr::wrmsr(DS_AREA, state.saved_ds_area.load(Ordering::Relaxed));
    }
    state.generation.fetch_add(1, Ordering::AcqRel);
}

/// One terminal entry point used by the kernel's kexec stop path.
pub fn quiesce_aux_for_kexec_local() {
    quiesce_pt_for_kexec_local();
    quiesce_bts_for_kexec_local();
    clear_lbr_on_domain_switch(1, 0);
    let slot = &PEBS_NMI[current_cpu()];
    for owner in &slot.owners {
        owner.armed.store(false, Ordering::Release);
        owner.ready.store(false, Ordering::Release);
        owner.generation.fetch_add(1, Ordering::AcqRel);
    }
    #[cfg(target_os = "none")]
    unsafe {
        x86::msr::wrmsr(PEBS_ENABLE, slot.saved_pebs_enable.load(Ordering::Relaxed));
        x86::msr::wrmsr(DS_AREA, slot.saved_ds_area.load(Ordering::Relaxed));
    }
}

/// Lock-free, baseline-independent AUX shutdown for crash-kexec.
///
/// The ordinary kexec hook restores saved owners and therefore trusts the
/// per-event state.  A panic cannot make that assumption: disable producers
/// directly after checking only architectural CPUID presence, then invalidate
/// the preallocated NMI hand-off records with atomic stores.
#[cfg(target_os = "none")]
pub fn crash_quiesce_aux_current() {
    use core::arch::x86_64::__cpuid_count;

    let vendor = __cpuid_count(0, 0);
    if [vendor.ebx, vendor.edx, vendor.ecx] != [0x756e6547, 0x49656e69, 0x6c65746e] {
        return;
    }
    let maximum = vendor.eax;
    let leaf1 = __cpuid_count(1, 0);
    unsafe {
        if maximum >= 0x14 && __cpuid_count(0x14, 0).eax != 0 {
            x86::msr::wrmsr(PT_CTL, 0);
        }
        if leaf1.edx & (1 << 21) != 0 {
            x86::msr::wrmsr(PEBS_ENABLE, 0);
            x86::msr::wrmsr(DEBUGCTL, 0);
            x86::msr::wrmsr(DS_AREA, 0);
        }
    }
    let cpu = current_cpu();
    PT[cpu].active.store(false, Ordering::Release);
    PT[cpu].generation.fetch_add(1, Ordering::AcqRel);
    BTS[cpu].active.store(false, Ordering::Release);
    BTS[cpu].generation.fetch_add(1, Ordering::AcqRel);
    let slot = &PEBS_NMI[cpu];
    for owner in &slot.owners {
        owner.armed.store(false, Ordering::Release);
        owner.ready.store(false, Ordering::Release);
        owner.generation.fetch_add(1, Ordering::AcqRel);
    }
}

fn require_panther_lake() -> Result<(), Error> {
    match pmu::capability_snapshot() {
        Ok(snapshot) if snapshot.product == ProductClass::PantherLake => Ok(()),
        _ => Err(Error::Unsupported),
    }
}
fn require_panther_lake_pt() -> Result<(), Error> {
    require_panther_lake()?;
    #[cfg(target_os = "none")]
    {
        use core::arch::x86_64::__cpuid_count;
        if __cpuid_count(0, 0).eax >= 0x14 && __cpuid_count(0x14, 0).eax != 0 {
            return Ok(());
        }
    }
    Err(Error::Unsupported)
}

/// Intel PT records canonical linear instruction pointers.  Restrict the
/// first product to the universally valid 48-bit subset even on an LA57 CPU;
/// this avoids programming an address-range MSR that an online 48-bit CPU
/// would reject during fleet operation.
const fn is_canonical_address(address: u64) -> bool {
    let upper48 = address >> 48;
    upper48 == 0 || upper48 == 0xffff
}
fn current_cpu() -> usize {
    #[cfg(target_os = "none")]
    {
        crate::cpu::current_logical_cpu_id()
    }
    #[cfg(not(target_os = "none"))]
    {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pebs_decode_requires_exact_documented_record() {
        let mut raw = [0u8; 64];
        raw[8..16].copy_from_slice(&0x1234u64.to_le_bytes());
        raw[32..40].copy_from_slice(&7u64.to_le_bytes());
        assert_eq!(
            decode_pebs_record(PebsFormat::PantherCoveBasic, &raw)
                .unwrap()
                .ip,
            0x1234
        );
        assert_eq!(
            decode_pebs_record(PebsFormat::PantherCoveBasic, &raw[..63]),
            Err(Error::InvalidBuffer)
        );
    }
    #[test]
    fn topa_wrap_and_overflow_math() {
        let layout = TopaLayout {
            data_bytes: 8192,
            page_bytes: 4096,
        };
        assert_eq!(layout.offset(8200).unwrap(), 8);
        assert_eq!(layout.distance(8180, 8200).unwrap(), 20);
        assert_eq!(layout.distance(0, 9000).unwrap(), 8192);
    }
    #[test]
    fn topa_rejects_non_page_geometry() {
        assert_eq!(
            TopaLayout {
                data_bytes: 4097,
                page_bytes: 4096
            }
            .validate(),
            Err(Error::InvalidBuffer)
        );
    }
    #[test]
    fn lbr_domain_transition_only_clears_across_domains() {
        clear_lbr_on_domain_switch(4, 4);
        clear_lbr_on_domain_switch(4, 5);
    }
    #[test]
    fn pebs_format_gate_does_not_guess() {
        assert_eq!(
            PebsFormat::from_perf_capabilities(4 << 8),
            Some(PebsFormat::PantherCoveBasic)
        );
        assert_eq!(PebsFormat::from_perf_capabilities(3 << 8), None);
    }
    #[test]
    fn pebs_nmi_handoff_rejects_stale_generation() {
        let slot = &PEBS_NMI[0];
        let owner = &slot.owners[0];
        owner.generation.store(9, Ordering::Release);
        owner.counter_bit.store(1, Ordering::Release);
        owner.armed.store(true, Ordering::Release);
        owner.ready.store(false, Ordering::Release);
        assert_eq!(capture_pebs_nmi(8, 1), Err(Error::Stale));
        capture_pebs_nmi(9, 1).unwrap();
        assert!(take_pebs_record_local(9).unwrap().is_some());
        owner.armed.store(false, Ordering::Release);
    }
    #[test]
    fn bts_buffer_requires_aligned_complete_records() {
        assert!(
            BtsBuffer {
                physical: 0x40,
                bytes: 24
            }
            .validate()
            .is_ok()
        );
        assert!(
            BtsBuffer {
                physical: 1,
                bytes: 24
            }
            .validate()
            .is_err()
        );
    }
}
