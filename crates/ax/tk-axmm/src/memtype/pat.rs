// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. Port of the cache-mode tables in
// Linux 7.2.3 arch/x86/mm/init.c (__cachemode2pte_tbl, __pte2cachemode_tbl,
// update_cache_mode_entry) and arch/x86/mm/pat/memtype.c (init_cache_modes).
//! Cache modes and their mapping to the PAT index carried by a PTE.
//!
//! A 4 KiB PTE selects its IA32_PAT slot with three bits: PWT (bit 3), PCD
//! (bit 4) and PAT (bit 7). The slot index is `PAT<<2 | PCD<<1 | PWT`.

use axhal::paging::MappingFlags;

/// Linux `_PAGE_CACHE_MODE_*`, with the same numeric values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum CacheMode {
    Wb = 0,
    Wc = 1,
    UcMinus = 2,
    Uc = 3,
    Wt = 4,
    Wp = 5,
}

impl CacheMode {
    /// Every cache mode, in Linux enum order.
    pub const ALL: [CacheMode; 6] = [
        CacheMode::Wb,
        CacheMode::Wc,
        CacheMode::UcMinus,
        CacheMode::Uc,
        CacheMode::Wt,
        CacheMode::Wp,
    ];

    const fn slot(self) -> usize {
        self as usize
    }
}

/// Architectural IA32_PAT memory-type codes (`X86_MEMTYPE_*`).
const X86_UC: u8 = 0;
const X86_WC: u8 = 1;
const X86_WT: u8 = 4;
const X86_WP: u8 = 5;
const X86_WB: u8 = 6;
const X86_UC_MINUS: u8 = 7;

/// PTE bit positions for a 4 KiB leaf.
pub const PTE_PWT: u16 = 1 << 3;
pub const PTE_PCD: u16 = 1 << 4;
pub const PTE_PAT: u16 = 1 << 7;

/// Linux `PAT_VALUE(p0..p7)`: pack eight architectural codes, slot 0 lowest.
pub const fn pat_value(types: [u8; 8]) -> u64 {
    let mut value = 0u64;
    let mut slot = 0;
    while slot < 8 {
        value |= (types[slot] as u64) << (slot * 8);
        slot += 1;
    }
    value
}

/// Linux full-PAT palette (`pat_bp_init`): WB, WC, UC-, UC, WB, WP, UC-, WT.
pub const PAT_LINUX_FULL: u64 = pat_value([
    X86_WB, X86_WC, X86_UC_MINUS, X86_UC, X86_WB, X86_WP, X86_UC_MINUS, X86_WT,
]);

/// Linux `PAT_VALUE(WB, WT, UC_MINUS, UC, WB, WT, UC_MINUS, UC)`, the value
/// that Linux uses to emulate PAT when the CPU has none.
pub const PAT_NO_PAT_EMULATION: u64 = pat_value([
    X86_WB, X86_WT, X86_UC_MINUS, X86_UC, X86_WB, X86_WT, X86_UC_MINUS, X86_UC,
]);

/// Map one architectural memory-type code to a cache mode. Reserved codes are
/// treated as WB, as `pat_get_cache_mode()` does.
const fn cache_mode_for_code(code: u8) -> CacheMode {
    match code {
        X86_UC => CacheMode::Uc,
        X86_WC => CacheMode::Wc,
        X86_WT => CacheMode::Wt,
        X86_WP => CacheMode::Wp,
        X86_UC_MINUS => CacheMode::UcMinus,
        _ => CacheMode::Wb,
    }
}

/// PTE-index bits (PAT, PCD, PWT as bits 2, 1, 0) to PTE bits.
pub const fn index_to_pte_bits(index: u8) -> u16 {
    (((index & 4) as u16) << (7 - 2))
        | (((index & 2) as u16) << (4 - 1))
        | (((index & 1) as u16) << 3)
}

/// The two Linux translation tables, as one value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CacheTables {
    /// `__cachemode2pte_tbl`: PAT index that encodes each cache mode.
    to_index: [u8; 6],
    /// `__pte2cachemode_tbl`: cache mode of each PAT index.
    from_index: [CacheMode; 8],
}

impl CacheTables {
    /// Linux static defaults before any PAT is loaded: WC, WT and WP fall
    /// back to UC- (PCD only), and UC- is PCD only.
    const DEFAULT_TO_INDEX: [u8; 6] = [0, 2, 2, 3, 2, 2];
    /// `__pte2cachemode_tbl` for the PWT/PCD/PAT index space without PAT.
    const DEFAULT_FROM_INDEX: [CacheMode; 8] = [
        CacheMode::Wb,
        CacheMode::UcMinus,
        CacheMode::UcMinus,
        CacheMode::Uc,
        CacheMode::Wb,
        CacheMode::UcMinus,
        CacheMode::UcMinus,
        CacheMode::Uc,
    ];

    /// Tables for a PAT MSR value, as `init_cache_modes()` builds them: the
    /// lowest slot holding a mode wins, and absent modes keep the defaults.
    pub const fn from_pat(pat: u64) -> Self {
        let mut to_index = Self::DEFAULT_TO_INDEX;
        let mut from_index = Self::DEFAULT_FROM_INDEX;
        let mut slot = 7usize;
        loop {
            let code = ((pat >> (slot * 8)) & 0x7) as u8;
            let mode = cache_mode_for_code(code);
            from_index[slot] = mode;
            to_index[mode.slot()] = slot as u8;
            if slot == 0 {
                break;
            }
            slot -= 1;
        }
        Self {
            to_index,
            from_index,
        }
    }

    /// Tables the kernel uses when PAT is not programmed on this platform.
    pub const fn without_pat() -> Self {
        Self::from_pat(PAT_NO_PAT_EMULATION)
    }

    /// PAT index that encodes `mode`.
    pub const fn index_of(&self, mode: CacheMode) -> u8 {
        self.to_index[mode.slot()]
    }

    /// Cache mode selected by PAT index `index` (0..8).
    pub const fn mode_of(&self, index: u8) -> CacheMode {
        self.from_index[(index & 7) as usize]
    }

    /// PTE cache bits (PWT/PCD/PAT) that encode `mode` in a 4 KiB leaf.
    pub const fn pte_bits(&self, mode: CacheMode) -> u16 {
        index_to_pte_bits(self.index_of(mode))
    }

    /// Recover the cache mode from the PTE cache bits of a 4 KiB leaf.
    pub const fn mode_of_pte_bits(&self, bits: u16) -> CacheMode {
        let index = (((bits & PTE_PAT) >> 5) | ((bits & PTE_PCD) >> 3) | ((bits & PTE_PWT) >> 3))
            as u8;
        self.mode_of(index)
    }

    /// The `MappingFlags` that select `mode` in this kernel's PTE encoding.
    ///
    /// `MappingFlags` expresses PWT (WRITE_COMBINING), PCD|PWT (UNCACHED) and
    /// no bits (WB). A mode whose index needs PCD alone (UC-) is carried as
    /// UNCACHED, which is strictly stronger, as Linux's `ioremap` default
    /// does on hardware without a UC- flag. Modes that need the PAT bit
    /// (WT, WP in the full palette) have no encoding here and return `None`.
    pub const fn flags_for(&self, mode: CacheMode) -> Option<MappingFlags> {
        match self.index_of(mode) {
            0 => Some(MappingFlags::empty()),
            1 => Some(MappingFlags::WRITE_COMBINING),
            2 | 3 => Some(MappingFlags::UNCACHED),
            _ => None,
        }
    }

    /// Recover the cache mode that a `MappingFlags` cache selection encodes.
    pub fn mode_of_flags(&self, flags: MappingFlags) -> CacheMode {
        if flags.contains(MappingFlags::UNCACHED) {
            self.mode_of(3)
        } else if flags.contains(MappingFlags::WRITE_COMBINING) {
            self.mode_of(1)
        } else {
            self.mode_of(0)
        }
    }
}

/// The cache bits of `MappingFlags`, which [`strip_cache`] clears.
pub const CACHE_FLAGS: MappingFlags =
    MappingFlags::UNCACHED.union(MappingFlags::WRITE_COMBINING).union(MappingFlags::DEVICE);

/// Clear every cache-attribute bit from `flags`.
pub fn strip_cache(flags: MappingFlags) -> MappingFlags {
    flags & !CACHE_FLAGS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_palette_matches_linux_pat_value() {
        // PAT_VALUE(WB, WC, UC_MINUS, UC, WB, WP, UC_MINUS, WT)
        assert_eq!(PAT_LINUX_FULL, 0x0407_0506_0007_0106);
        assert_eq!(PAT_NO_PAT_EMULATION, 0x0007_0406_0007_0406);
    }

    #[test]
    fn full_palette_tables_follow_init_cache_modes() {
        let t = CacheTables::from_pat(PAT_LINUX_FULL);
        assert_eq!(t.mode_of(0), CacheMode::Wb);
        assert_eq!(t.mode_of(1), CacheMode::Wc);
        assert_eq!(t.mode_of(2), CacheMode::UcMinus);
        assert_eq!(t.mode_of(3), CacheMode::Uc);
        assert_eq!(t.mode_of(4), CacheMode::Wb);
        assert_eq!(t.mode_of(5), CacheMode::Wp);
        assert_eq!(t.mode_of(6), CacheMode::UcMinus);
        assert_eq!(t.mode_of(7), CacheMode::Wt);
        assert_eq!(t.index_of(CacheMode::Wb), 0);
        assert_eq!(t.index_of(CacheMode::Wc), 1);
        assert_eq!(t.index_of(CacheMode::UcMinus), 2);
        assert_eq!(t.index_of(CacheMode::Uc), 3);
        assert_eq!(t.index_of(CacheMode::Wt), 7);
        assert_eq!(t.index_of(CacheMode::Wp), 5);
    }

    #[test]
    fn lowest_slot_wins_when_a_mode_is_duplicated() {
        // Slot 0 and slot 4 both WB; slot 2 and 6 both UC-.
        let t = CacheTables::from_pat(PAT_NO_PAT_EMULATION);
        assert_eq!(t.index_of(CacheMode::Wb), 0);
        assert_eq!(t.index_of(CacheMode::UcMinus), 2);
        // WT lives in slot 1 of the emulated table, not in its slot 5.
        assert_eq!(t.index_of(CacheMode::Wt), 1);
        // WC is absent, so it keeps the static default of UC- (PCD).
        assert_eq!(t.index_of(CacheMode::Wc), 2);
        assert_eq!(t.index_of(CacheMode::Wp), 2);
    }

    #[test]
    fn index_bits_match_pte_pwt_pcd_pat_positions() {
        assert_eq!(index_to_pte_bits(0), 0);
        assert_eq!(index_to_pte_bits(1), PTE_PWT);
        assert_eq!(index_to_pte_bits(2), PTE_PCD);
        assert_eq!(index_to_pte_bits(3), PTE_PCD | PTE_PWT);
        assert_eq!(index_to_pte_bits(4), PTE_PAT);
    }

    #[test]
    fn pte_bits_round_trip_through_the_palette() {
        let t = CacheTables::from_pat(PAT_LINUX_FULL);
        for mode in CacheMode::ALL {
            assert_eq!(t.mode_of_pte_bits(t.pte_bits(mode)), mode, "{mode:?}");
        }
    }

    #[test]
    fn linux_full_palette_is_consistent_with_mapping_flags_encoding() {
        let t = CacheTables::from_pat(PAT_LINUX_FULL);
        assert_eq!(t.flags_for(CacheMode::Wb), Some(MappingFlags::empty()));
        assert_eq!(t.flags_for(CacheMode::Wc), Some(MappingFlags::WRITE_COMBINING));
        assert_eq!(t.flags_for(CacheMode::Uc), Some(MappingFlags::UNCACHED));
        assert_eq!(t.flags_for(CacheMode::UcMinus), Some(MappingFlags::UNCACHED));
        // WT needs the PAT bit, which MappingFlags cannot express.
        assert_eq!(t.flags_for(CacheMode::Wt), None);
        assert_eq!(t.mode_of_flags(MappingFlags::WRITE_COMBINING), CacheMode::Wc);
        assert_eq!(t.mode_of_flags(MappingFlags::UNCACHED), CacheMode::Uc);
    }

    #[test]
    fn without_pat_write_combining_degrades_to_uc_minus() {
        let t = CacheTables::without_pat();
        assert_eq!(t.index_of(CacheMode::Wc), 2);
        assert_eq!(t.flags_for(CacheMode::Wc), Some(MappingFlags::UNCACHED));
        assert_eq!(t.index_of(CacheMode::Wt), 1);
    }

    #[test]
    fn strip_cache_keeps_permissions_and_pkeys() {
        let flags = MappingFlags::READ | MappingFlags::WRITE | MappingFlags::WRITE_COMBINING;
        assert_eq!(strip_cache(flags), MappingFlags::READ | MappingFlags::WRITE);
    }
}
