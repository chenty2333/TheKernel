// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. Port of Linux 7.2.3
// arch/x86/mm/ioremap.c: __ioremap_caller() RAM refusal (__ioremap_check_ram)
// and the memtype reservation that precedes the page-table update.
//! Physical-range reservation for `ioremap*` mappings.

use axerrno::{AxError, AxResult};
use axhal::paging::MappingFlags;

use super::{CacheMode, CacheTables, MemtypeState, ReservePolicy, overlaps_ram, pat_enabled, platform_ram, tables};

/// A reserved, page-aligned physical range and the PTE cache selection the
/// caller must install for it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IoremapReservation {
    pub start: u64,
    pub end: u64,
    /// The mode actually granted, which may be a compatible existing one.
    pub mode: CacheMode,
    /// Cache bits to OR into the leaf flags (`UNCACHED`, `WRITE_COMBINING` or none).
    pub flags: MappingFlags,
}

/// `__ioremap_caller` for the physical side: refuse system RAM, reserve the
/// memtype for the page-aligned extent, and derive the PTE cache bits.
pub fn reserve_with(
    state: &mut MemtypeState,
    ram: &[(usize, usize)],
    pat: bool,
    tables: &CacheTables,
    paddr: u64,
    size: u64,
    req: CacheMode,
) -> AxResult<IoremapReservation> {
    if size == 0 {
        return Err(AxError::InvalidInput);
    }
    let start = paddr & !0xfff;
    let end = paddr
        .checked_add(size)
        .and_then(|end| end.checked_add(0xfff))
        .ok_or(AxError::InvalidInput)?
        & !0xfff;
    if end <= start {
        return Err(AxError::InvalidInput);
    }
    // Linux refuses RAM for every ioremap flavour; use the RAM-side APIs.
    if overlaps_ram(ram, start, end) {
        return Err(AxError::InvalidInput);
    }
    let mode = state
        .reserve(ram, pat, start, end, req, ReservePolicy::Any)
        .map_err(AxError::from)?;
    match tables.flags_for(mode) {
        Some(flags) => Ok(IoremapReservation {
            start,
            end,
            mode,
            flags,
        }),
        None => {
            let _ = state.free(ram, pat, start, end);
            Err(AxError::InvalidInput)
        }
    }
}

/// Global `ioremap` reservation. Call [`free`] when the mapping is torn down.
pub fn reserve(paddr: u64, size: u64, req: CacheMode) -> AxResult<IoremapReservation> {
    let ram = platform_ram();
    let pat = pat_enabled();
    let tables = tables();
    let mut state = super::STATE.lock();
    reserve_with(&mut state, ram, pat, &tables, paddr, size, req)
}

/// Release a reservation made by [`reserve`] (Linux `memtype_free_io`).
pub fn free(reservation: &IoremapReservation) -> AxResult {
    super::memtype_free_io(reservation.start, reservation.end)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAM: &[(usize, usize)] = &[(0x0010_0000, 0x0010_0000)];

    #[test]
    fn ram_is_refused_for_every_mode() {
        let mut s = MemtypeState::new();
        let t = CacheTables::from_pat(super::super::PAT_LINUX_FULL);
        for req in [CacheMode::Wc, CacheMode::UcMinus, CacheMode::Uc] {
            assert_eq!(
                reserve_with(&mut s, RAM, true, &t, 0x0010_0000, 0x1000, req),
                Err(AxError::InvalidInput)
            );
        }
        // A range that only ends inside RAM is still RAM.
        assert_eq!(
            reserve_with(&mut s, RAM, true, &t, 0x001f_f000, 0x2000, CacheMode::Wc),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn wc_reservation_gets_pat_one_and_extends_to_pages() {
        let mut s = MemtypeState::new();
        let t = CacheTables::from_pat(super::super::PAT_LINUX_FULL);
        let r = reserve_with(&mut s, RAM, true, &t, 0xf000_0234, 0x2000, CacheMode::Wc).unwrap();
        assert_eq!((r.start, r.end), (0xf000_0000, 0xf000_3000));
        assert_eq!(r.mode, CacheMode::Wc);
        assert_eq!(r.flags, MappingFlags::WRITE_COMBINING);
    }

    #[test]
    fn existing_uc_minus_is_granted_to_a_wc_request() {
        let mut s = MemtypeState::new();
        let t = CacheTables::from_pat(super::super::PAT_LINUX_FULL);
        reserve_with(&mut s, RAM, true, &t, 0xf000_0000, 0x1000, CacheMode::UcMinus).unwrap();
        let r = reserve_with(&mut s, RAM, true, &t, 0xf000_0000, 0x1000, CacheMode::Wc).unwrap();
        assert_eq!(r.mode, CacheMode::UcMinus);
        assert_eq!(r.flags, MappingFlags::UNCACHED);
    }

    #[test]
    fn without_pat_wc_is_uc_minus_on_the_pte() {
        let mut s = MemtypeState::new();
        let t = CacheTables::without_pat();
        let r = reserve_with(&mut s, RAM, false, &t, 0xf000_0000, 0x1000, CacheMode::Wc).unwrap();
        assert_eq!(r.mode, CacheMode::Wc);
        assert_eq!(r.flags, MappingFlags::UNCACHED);
    }

    #[test]
    fn wt_without_a_pat_bit_encoding_is_refused_and_rolled_back() {
        let mut s = MemtypeState::new();
        let t = CacheTables::from_pat(super::super::PAT_LINUX_FULL);
        assert_eq!(
            reserve_with(&mut s, RAM, true, &t, 0xf000_0000, 0x1000, CacheMode::Wt),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            s.lookup(&[], true, 0xf000_0000),
            CacheMode::UcMinus,
            "the refused reservation must not stay recorded"
        );
    }

    #[test]
    fn zero_and_overflowing_sizes_are_rejected() {
        let mut s = MemtypeState::new();
        let t = CacheTables::from_pat(super::super::PAT_LINUX_FULL);
        assert_eq!(
            reserve_with(&mut s, RAM, true, &t, 0xf000_0000, 0, CacheMode::Wc),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            reserve_with(&mut s, RAM, true, &t, u64::MAX - 1, 8, CacheMode::Wc),
            Err(AxError::InvalidInput)
        );
    }
}
