// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. Port of Linux 7.2.3
// arch/x86/mm/pat/memtype.c (memtype_reserve, memtype_free, lookup_memtype,
// reserve_ram_pages_type, free_ram_pages_type, pat_pfn_immune_to_uc_mtrr),
// arch/x86/mm/ioremap.c (memtype_reserve_io, memtype_free_io) and
// arch/x86/mm/pat/memtype.c:arch_phys_wc_add().
//!
//! Every physical range that is mapped with a non-default cache mode is
//! recorded here. Non-RAM ranges (device BARs, MMIO) go into an interval set.
//! RAM is tracked per page, as Linux does with page flags; this table stores
//! only pages whose mode is not WB, so the common case costs nothing.
//!
//! MTRR is not modelled. Linux consults MTRRs to decide what a WB request on
//! a non-RAM range really gets (`pat_x_mtrr_type`). With no MTRR information,
//! WB on non-RAM falls back to UC-, which is what the default MTRR type gives
//! on the platforms this kernel targets.

pub mod interval;
pub mod ioremap;
pub mod pat;
pub mod set_memory;

use alloc::collections::BTreeMap;

use axerrno::{AxError, AxResult};
use axhal::paging::MappingFlags;
use kspin::SpinNoIrq;

pub use self::pat::{CacheMode, CacheTables, PAT_LINUX_FULL};
use self::{interval::IntervalSet, pat::strip_cache};

/// Linux `ISA_START_ADDRESS` and `ISA_END_ADDRESS`: the legacy region is
/// always WB and is never tracked.
pub const ISA_START: u64 = 0xa0000;
pub const ISA_END: u64 = 0x100000;
const PAGE_SHIFT: u32 = 12;
const PAGE_SIZE: u64 = 1 << PAGE_SHIFT;

/// `-ENODEV`, returned by [`arch_phys_wc_add`] when PAT is not available and
/// no MTRR fallback exists.
pub const ENODEV: i32 = 19;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemtypeError {
    /// Bad range, or a request the architecture cannot express.
    Invalid,
    /// An existing reservation has a different type.
    Busy {
        tracked: CacheMode,
        requested: CacheMode,
    },
    /// The range is not tracked (free of an unknown range).
    NotFound,
}

impl From<MemtypeError> for AxError {
    fn from(error: MemtypeError) -> Self {
        match error {
            MemtypeError::Invalid => AxError::InvalidInput,
            MemtypeError::Busy { .. } => AxError::ResourceBusy,
            MemtypeError::NotFound => AxError::InvalidInput,
        }
    }
}

/// Linux `new_type == NULL` (`Exact`) or `new_type != NULL` (`Any`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReservePolicy {
    /// The reservation must be made with exactly the requested type.
    Exact,
    /// Any compatible existing type is accepted and returned.
    Any,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RangeKind {
    Ram,
    NonRam,
    Mixed,
}

/// Whether `[start, end)` is covered by the given RAM ranges (`(base, len)`),
/// as `pat_pagerange_is_ram()` decides it. The ISA region counts as non-RAM.
fn classify(ram: &[(usize, usize)], start: u64, end: u64) -> RangeKind {
    let start = start.max(ISA_END);
    if start >= end {
        return RangeKind::NonRam;
    }
    let mut covered = 0u64;
    let mut pieces: alloc::vec::Vec<(u64, u64)> = alloc::vec::Vec::new();
    for &(base, len) in ram {
        let (rs, re) = (base as u64, (base as u64).saturating_add(len as u64));
        let (s, e) = (rs.max(start), re.min(end));
        if s < e {
            pieces.push((s, e));
        }
    }
    pieces.sort_unstable();
    let mut cursor = start;
    for (s, e) in pieces {
        if s > cursor {
            break;
        }
        if e > cursor {
            covered += e - cursor;
            cursor = e;
        }
    }
    if covered == 0 {
        RangeKind::NonRam
    } else if cursor >= end {
        RangeKind::Ram
    } else {
        RangeKind::Mixed
    }
}

/// Whether `[start, end)` is in the untracked legacy ISA window.
fn is_untracked(start: u64, end: u64) -> bool {
    start >= ISA_START && end <= ISA_END
}

/// Whether any byte of `[start, end)` is system RAM. Used by ioremap to refuse
/// RAM (`__ioremap_check_ram`), where a partially covered range also counts.
pub fn overlaps_ram(ram: &[(usize, usize)], start: u64, end: u64) -> bool {
    ram.iter().any(|&(base, len)| {
        let (rs, re) = (base as u64, (base as u64).saturating_add(len as u64));
        rs < end && re > start
    })
}

/// The tracking state: non-RAM intervals and non-WB RAM pages.
#[derive(Clone, Debug)]
pub struct MemtypeState {
    non_ram: IntervalSet<CacheMode>,
    /// pfn -> mode for RAM pages whose mode is not WB.
    ram: BTreeMap<u64, CacheMode>,
}

impl Default for MemtypeState {
    fn default() -> Self {
        Self::new()
    }
}

impl MemtypeState {
    pub const fn new() -> Self {
        Self {
            non_ram: IntervalSet::new(),
            ram: BTreeMap::new(),
        }
    }

    fn ram_mode(&self, pfn: u64) -> CacheMode {
        self.ram.get(&pfn).copied().unwrap_or(CacheMode::Wb)
    }

    /// `reserve_ram_pages_type`: every page must currently be WB.
    fn reserve_ram(
        &mut self,
        start: u64,
        end: u64,
        req: CacheMode,
    ) -> Result<CacheMode, MemtypeError> {
        if req == CacheMode::Wp {
            return Err(MemtypeError::Invalid);
        }
        // Strong UC is not supported for RAM; Linux downgrades it to UC-.
        let req = if req == CacheMode::Uc {
            CacheMode::UcMinus
        } else {
            req
        };
        for pfn in (start >> PAGE_SHIFT)..(end >> PAGE_SHIFT) {
            let tracked = self.ram_mode(pfn);
            if tracked != CacheMode::Wb {
                return Err(MemtypeError::Busy {
                    tracked,
                    requested: req,
                });
            }
        }
        for pfn in (start >> PAGE_SHIFT)..(end >> PAGE_SHIFT) {
            if req == CacheMode::Wb {
                self.ram.remove(&pfn);
            } else {
                self.ram.insert(pfn, req);
            }
        }
        Ok(req)
    }

    /// `memtype_reserve`. `ram` is the platform RAM map, `pat` is whether PAT
    /// is programmed on every CPU. Without PAT the request is returned as is,
    /// exactly as Linux does; the caller must then use [`CacheTables::without_pat`].
    pub fn reserve(
        &mut self,
        ram: &[(usize, usize)],
        pat: bool,
        start: u64,
        end: u64,
        req: CacheMode,
        policy: ReservePolicy,
    ) -> Result<CacheMode, MemtypeError> {
        if start >= end {
            return Err(MemtypeError::Invalid);
        }
        if !pat {
            return Ok(req);
        }
        if is_untracked(start, end) {
            return Ok(CacheMode::Wb);
        }
        // Without MTRR information a WB request on non-RAM is UC-.
        let actual = if req == CacheMode::Wb {
            CacheMode::UcMinus
        } else {
            req
        };
        match classify(ram, start, end) {
            RangeKind::Mixed => Err(MemtypeError::Invalid),
            RangeKind::Ram => self.reserve_ram(start, end, req),
            RangeKind::NonRam => {
                let overlapping = self.non_ram.overlapping(start, end);
                let result = match (overlapping.first(), policy) {
                    (None, _) => actual,
                    (Some(first), ReservePolicy::Any) => first.value,
                    (Some(first), ReservePolicy::Exact) => {
                        if first.value != actual {
                            return Err(MemtypeError::Busy {
                                tracked: first.value,
                                requested: actual,
                            });
                        }
                        actual
                    }
                };
                // Linux checks every overlapping entry against the type it
                // settled on, so mixed overlaps are always refused.
                for entry in &overlapping {
                    if entry.value != result {
                        return Err(MemtypeError::Busy {
                            tracked: entry.value,
                            requested: result,
                        });
                    }
                }
                self.non_ram.insert(start, end, result);
                Ok(result)
            }
        }
    }

    /// `memtype_free`.
    pub fn free(
        &mut self,
        ram: &[(usize, usize)],
        pat: bool,
        start: u64,
        end: u64,
    ) -> Result<(), MemtypeError> {
        if !pat || is_untracked(start, end) {
            return Ok(());
        }
        match classify(ram, start, end) {
            RangeKind::Mixed => Err(MemtypeError::Invalid),
            RangeKind::Ram => {
                for pfn in (start >> PAGE_SHIFT)..(end >> PAGE_SHIFT) {
                    self.ram.remove(&pfn);
                }
                Ok(())
            }
            RangeKind::NonRam => self
                .non_ram
                .remove_exact(start, end)
                .map(|_| ())
                .ok_or(MemtypeError::NotFound),
        }
    }

    /// `lookup_memtype` for one page.
    pub fn lookup(&self, ram: &[(usize, usize)], pat: bool, paddr: u64) -> CacheMode {
        if !pat || is_untracked(paddr, paddr + PAGE_SIZE) {
            return CacheMode::Wb;
        }
        match classify(ram, paddr, paddr + PAGE_SIZE) {
            RangeKind::Ram => self.ram_mode(paddr >> PAGE_SHIFT),
            _ => self
                .non_ram
                .first_overlapping(paddr, paddr + PAGE_SIZE)
                .map(|e| e.value)
                .unwrap_or(CacheMode::UcMinus),
        }
    }
}

/// `pat_pfn_immune_to_uc_mtrr`: UC, UC- and WC cannot be overridden by an
/// MTRR, so they are safe to use for MMIO (no MTRR is modelled here).
pub fn pfn_immune_to_uc_mtrr(mode: CacheMode) -> bool {
    matches!(mode, CacheMode::Uc | CacheMode::UcMinus | CacheMode::Wc)
}

pub(crate) static STATE: SpinNoIrq<MemtypeState> = SpinNoIrq::new(MemtypeState::new());

/// Whether PAT is programmed on every startup CPU.
pub fn pat_enabled() -> bool {
    axhal::boot::intel_cpu_mmap_ready()
}

/// Cache-mode tables that the page-table encoder must use right now.
pub fn tables() -> CacheTables {
    if pat_enabled() {
        CacheTables::from_pat(PAT_LINUX_FULL)
    } else {
        CacheTables::without_pat()
    }
}

pub(crate) fn platform_ram() -> &'static [(usize, usize)] {
    axhal::mem::phys_ram_ranges()
}

fn state_reserve(
    start: u64,
    end: u64,
    req: CacheMode,
    policy: ReservePolicy,
) -> AxResult<CacheMode> {
    let ram = platform_ram();
    let mut state = STATE.lock();
    Ok(state.reserve(ram, pat_enabled(), start, end, req, policy)?)
}

/// Linux `memtype_reserve` with `new_type == NULL`.
pub fn memtype_reserve(start: u64, end: u64, req: CacheMode) -> AxResult<CacheMode> {
    state_reserve(start, end, req, ReservePolicy::Exact)
}

/// Linux `memtype_free`.
pub fn memtype_free(start: u64, end: u64) -> AxResult {
    let ram = platform_ram();
    let mut state = STATE.lock();
    Ok(state.free(ram, pat_enabled(), start, end)?)
}

/// Linux `memtype_reserve_io`: reserve `[start, end)` and return the type
/// actually granted, which may be a compatible existing type.
pub fn memtype_reserve_io(start: u64, end: u64, req: CacheMode) -> AxResult<CacheMode> {
    state_reserve(start, end, req, ReservePolicy::Any)
}

/// Linux `memtype_free_io`.
pub fn memtype_free_io(start: u64, end: u64) -> AxResult {
    memtype_free(start, end)
}

/// Linux `lookup_memtype` for one page.
pub fn lookup_memtype(paddr: u64) -> CacheMode {
    let ram = platform_ram();
    STATE.lock().lookup(ram, pat_enabled(), paddr)
}

/// Linux `arch_phys_wc_add`: with PAT, write-combining needs no MTRR.
pub fn arch_phys_wc_add(_base: u64, _size: u64) -> i32 {
    if pat_enabled() { 0 } else { -ENODEV }
}

/// `pgprot_writecombine`: the PTE cache selection for WC in the active palette.
pub fn pgprot_writecombine(prot: MappingFlags) -> MappingFlags {
    strip_cache(prot) | tables().flags_for(CacheMode::Wc).unwrap_or(MappingFlags::UNCACHED)
}

/// `pgprot_noncached`: the PTE cache selection for UC.
pub fn pgprot_noncached(prot: MappingFlags) -> MappingFlags {
    strip_cache(prot) | tables().flags_for(CacheMode::Uc).unwrap_or(MappingFlags::UNCACHED)
}

/// `track_pfn_remap`: reserve a physical range for a remap into user space.
/// Returns the granted mode; the caller builds its PTEs with
/// [`pgprot_for_mode`].
pub fn track_pfn_remap(paddr: u64, size: u64, req: CacheMode) -> AxResult<CacheMode> {
    if size == 0 {
        return Err(AxError::InvalidInput);
    }
    let end = paddr
        .checked_add(size)
        .ok_or(AxError::InvalidInput)?
        .next_multiple_of(PAGE_SIZE);
    memtype_reserve_io(paddr & !(PAGE_SIZE - 1), end, req)
}

/// `untrack_pfn`.
pub fn untrack_pfn(paddr: u64, size: u64) -> AxResult {
    if size == 0 {
        return Ok(());
    }
    let end = paddr.saturating_add(size).next_multiple_of(PAGE_SIZE);
    memtype_free_io(paddr & !(PAGE_SIZE - 1), end)
}

/// `track_pfn_insert`: the PTE cache selection that matches the recorded type
/// of `paddr`, so a `vmf_insert_pfn` cannot alias a different mode.
pub fn track_pfn_insert(paddr: u64, prot: MappingFlags) -> MappingFlags {
    let mode = lookup_memtype(paddr);
    strip_cache(prot) | tables().flags_for(mode).unwrap_or(MappingFlags::UNCACHED)
}

/// PTE cache bits for `mode` in the active palette, or `None` if the palette
/// cannot express it.
pub fn pgprot_for_mode(prot: MappingFlags, mode: CacheMode) -> Option<MappingFlags> {
    tables().flags_for(mode).map(|f| strip_cache(prot) | f)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAM: &[(usize, usize)] = &[(0x0010_0000, 0x0010_0000), (0x2000_0000, 0x1000_0000)];

    #[test]
    fn classify_distinguishes_ram_gaps_and_isa() {
        assert_eq!(classify(RAM, 0x0010_0000, 0x0020_0000), RangeKind::Ram);
        assert_eq!(classify(RAM, 0x0020_0000, 0x0030_0000), RangeKind::NonRam);
        assert_eq!(classify(RAM, 0x0010_0000, 0x0030_0000), RangeKind::Mixed);
        assert_eq!(classify(RAM, 0, ISA_END), RangeKind::NonRam);
        assert_eq!(classify(&[(0, 0x0020_0000)], 0, 0x0010_0000), RangeKind::NonRam);
        assert_eq!(classify(&[(0, 0x40_0000), (0x40_0000, 0x40_0000)], 0x20_0000, 0x60_0000), RangeKind::Ram);
    }

    #[test]
    fn ram_reserve_requires_wb_and_round_trips() {
        let mut s = MemtypeState::new();
        let (start, end) = (0x2000_0000, 0x2000_4000);
        assert_eq!(
            s.reserve(RAM, true, start, end, CacheMode::Wc, ReservePolicy::Exact),
            Ok(CacheMode::Wc)
        );
        assert_eq!(s.lookup(RAM, true, start + 0x1000), CacheMode::Wc);
        assert_eq!(
            s.reserve(RAM, true, start, end, CacheMode::Uc, ReservePolicy::Exact),
            Err(MemtypeError::Busy {
                tracked: CacheMode::Wc,
                requested: CacheMode::UcMinus,
            })
        );
        assert_eq!(
            s.reserve(RAM, true, start, end, CacheMode::UcMinus, ReservePolicy::Exact),
            Err(MemtypeError::Busy {
                tracked: CacheMode::Wc,
                requested: CacheMode::UcMinus,
            })
        );
        s.free(RAM, true, start, end).unwrap();
        assert_eq!(s.lookup(RAM, true, start), CacheMode::Wb);
    }

    #[test]
    fn ram_refuses_wp_and_downgrades_uc() {
        let mut s = MemtypeState::new();
        assert_eq!(
            s.reserve(RAM, true, 0x2000_0000, 0x2000_1000, CacheMode::Wp, ReservePolicy::Any),
            Err(MemtypeError::Invalid)
        );
        assert_eq!(
            s.reserve(RAM, true, 0x2000_0000, 0x2000_1000, CacheMode::Uc, ReservePolicy::Any),
            Ok(CacheMode::UcMinus)
        );
    }

    #[test]
    fn mixed_ram_and_gap_is_refused() {
        let mut s = MemtypeState::new();
        assert_eq!(
            s.reserve(RAM, true, 0x001f_f000, 0x0020_1000, CacheMode::Wc, ReservePolicy::Exact),
            Err(MemtypeError::Invalid)
        );
        assert_eq!(s.free(RAM, true, 0x001f_f000, 0x0020_1000), Err(MemtypeError::Invalid));
    }

    #[test]
    fn non_ram_exact_conflict_and_any_policy() {
        let mut s = MemtypeState::new();
        let (a, b) = (0xf000_0000u64, 0xf000_4000u64);
        assert_eq!(
            s.reserve(RAM, true, a, b, CacheMode::Wc, ReservePolicy::Exact),
            Ok(CacheMode::Wc)
        );
        assert_eq!(
            s.reserve(RAM, true, a, b, CacheMode::Uc, ReservePolicy::Exact),
            Err(MemtypeError::Busy {
                tracked: CacheMode::Wc,
                requested: CacheMode::Uc,
            })
        );
        // Linux: an "any" request is granted the existing compatible type.
        assert_eq!(
            s.reserve(RAM, true, a, b, CacheMode::Uc, ReservePolicy::Any),
            Ok(CacheMode::Wc)
        );
        assert_eq!(
            s.reserve(RAM, true, a + 0x1000, b + 0x1000, CacheMode::Wc, ReservePolicy::Exact),
            Ok(CacheMode::Wc)
        );
    }

    #[test]
    fn overlapping_reservations_must_agree_and_free_is_exact() {
        let mut s = MemtypeState::new();
        let (a, b) = (0xf000_0000u64, 0xf000_4000u64);
        s.reserve(RAM, true, a, b, CacheMode::Wc, ReservePolicy::Exact).unwrap();
        s.reserve(RAM, true, a, b, CacheMode::Wc, ReservePolicy::Exact).unwrap();
        assert_eq!(
            s.reserve(RAM, true, a + 0x2000, b + 0x4000, CacheMode::UcMinus, ReservePolicy::Exact),
            Err(MemtypeError::Busy {
                tracked: CacheMode::Wc,
                requested: CacheMode::UcMinus,
            })
        );
        assert_eq!(s.lookup(RAM, true, a), CacheMode::Wc);
        assert_eq!(s.free(RAM, true, a, b + 1), Err(MemtypeError::NotFound));
        s.free(RAM, true, a, b).unwrap();
        assert_eq!(s.lookup(RAM, true, a), CacheMode::Wc);
        s.free(RAM, true, a, b).unwrap();
        assert_eq!(s.lookup(RAM, true, a), CacheMode::UcMinus);
    }

    #[test]
    fn wb_on_non_ram_is_uc_minus_and_isa_is_untracked() {
        let mut s = MemtypeState::new();
        assert_eq!(
            s.reserve(RAM, true, 0xf100_0000, 0xf100_1000, CacheMode::Wb, ReservePolicy::Exact),
            Ok(CacheMode::UcMinus)
        );
        assert_eq!(
            s.reserve(RAM, true, 0xa0000, 0xa1000, CacheMode::Uc, ReservePolicy::Exact),
            Ok(CacheMode::Wb)
        );
        assert_eq!(s.lookup(RAM, true, 0xa0000), CacheMode::Wb);
    }

    #[test]
    fn without_pat_requests_pass_through_untracked() {
        let mut s = MemtypeState::new();
        assert_eq!(
            s.reserve(RAM, false, 0xf000_0000, 0xf000_1000, CacheMode::Wc, ReservePolicy::Exact),
            Ok(CacheMode::Wc)
        );
        assert!(s.non_ram.is_empty());
    }

    #[test]
    fn invalid_ranges_and_overlap_helper() {
        let mut s = MemtypeState::new();
        assert_eq!(
            s.reserve(RAM, true, 0x1000, 0x1000, CacheMode::Wc, ReservePolicy::Exact),
            Err(MemtypeError::Invalid)
        );
        assert!(overlaps_ram(RAM, 0x0010_0000, 0x0010_1000));
        assert!(!overlaps_ram(RAM, 0x0020_0000, 0x0020_1000));
        assert!(overlaps_ram(RAM, 0x000f_f000, 0x0010_1000));
        assert!(pfn_immune_to_uc_mtrr(CacheMode::Wc));
        assert!(!pfn_immune_to_uc_mtrr(CacheMode::Wb));
    }
}
