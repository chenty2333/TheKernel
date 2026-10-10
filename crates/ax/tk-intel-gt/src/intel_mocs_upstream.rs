// SPDX-License-Identifier: MIT
// Copyright © 2015 Intel Corporation.
// Translated from Linux v7.2.3 drivers/gpu/drm/i915/gt/intel_mocs.c.
// The complete MIT grant is retained in ../LICENSE-MIT.

#![allow(
    unsafe_code,
    non_snake_case,
    non_camel_case_types,
    non_upper_case_globals,
    dead_code
)]

use crate::{
    intel_engine_types_upstream::{IntelEngineCs, RENDER_CLASS},
    intel_gt_mcr_upstream::{
        intel_gt_mcr_lock, intel_gt_mcr_multicast_write_fw, intel_gt_mcr_unlock,
    },
    intel_gt_types_upstream::IntelGt,
    intel_uncore_types_upstream::{
        FORCEWAKE_ALL, IntelUncore, assert_forcewakes_active, intel_uncore_write_fw,
    },
    intel_workarounds_types_upstream::{I915McrRegT, I915RegT},
    linux::i915::{
        GRAPHICS_VER, GRAPHICS_VER_FULL, HAS_L3_CCS_READ, IP_VER, IS_DG1, IS_DG2, IS_DGFX,
        IS_GEN9_LP, IS_GFX_GT_IP_RANGE, IS_ROCKETLAKE, IS_TIGERLAKE, to_gt,
    },
    linux_i915_private::DrmI915Private,
};

const I915_MOCS_UNCACHED: usize = 0;
const I915_MOCS_PTE_INDEX: usize = 1;
const I915_MOCS_CACHED: usize = 2;
const GEN9_NUM_MOCS_ENTRIES: usize = 64;
const MTL_NUM_MOCS_ENTRIES: usize = 16;

const HAS_GLOBAL_MOCS: u32 = 1 << 0;
const HAS_ENGINE_MOCS: u32 = 1 << 1;
const HAS_RENDER_L3CC: u32 = 1 << 2;

const fn le_cacheability(value: u32) -> u32 {
    value
}
const fn le_tgt_cache(value: u32) -> u32 {
    value << 2
}
const fn le_lrum(value: u32) -> u32 {
    value << 4
}
const fn le_aom(value: u32) -> u32 {
    value << 6
}
const fn le_rsc(value: u32) -> u32 {
    value << 7
}
const fn le_scc(value: u32) -> u32 {
    value << 8
}
const fn le_pfm(value: u32) -> u32 {
    value << 11
}
const fn le_scf(value: u32) -> u32 {
    value << 14
}
const fn le_cos(value: u32) -> u32 {
    value << 15
}
const fn le_sse(value: u32) -> u32 {
    value << 17
}
const fn l4_cacheability(value: u32) -> u32 {
    value << 2
}
const fn ig_pat(value: u32) -> u32 {
    value << 8
}
const fn l3_esc(value: u16) -> u16 {
    value
}
const fn l3_scc(value: u16) -> u16 {
    value << 1
}
const fn l3_cacheability(value: u16) -> u16 {
    value << 4
}
const fn l3_glbgo(value: u16) -> u16 {
    value << 6
}
const fn l3_lkup(value: u16) -> u16 {
    value << 7
}

const LE_0_PAGETABLE: u32 = le_cacheability(0);
const LE_1_UC: u32 = le_cacheability(1);
const LE_2_WT: u32 = le_cacheability(2);
const LE_3_WB: u32 = le_cacheability(3);
const LE_TC_0_PAGETABLE: u32 = le_tgt_cache(0);
const LE_TC_1_LLC: u32 = le_tgt_cache(1);
const LE_TC_2_LLC_ELLC: u32 = le_tgt_cache(2);
const LE_TC_3_LLC_ELLC_ALT: u32 = le_tgt_cache(3);
const L3_0_DIRECT: u16 = l3_cacheability(0);
const L3_1_UC: u16 = l3_cacheability(1);
const L3_2_RESERVED: u16 = l3_cacheability(2);
const L3_3_WB: u16 = l3_cacheability(3);
const L4_0_WB: u32 = l4_cacheability(0);
const L4_1_WT: u32 = l4_cacheability(1);
const L4_2_RESERVED: u32 = l4_cacheability(2);
const L4_3_UC: u32 = l4_cacheability(3);

#[repr(C)]
#[derive(Clone, Copy)]
struct DrmI915MocsEntry {
    control_value: u32,
    l3cc_value: u16,
    used: u16,
}

const UNUSED_MOCS_ENTRY: DrmI915MocsEntry = DrmI915MocsEntry {
    control_value: 0,
    l3cc_value: 0,
    used: 0,
};

macro_rules! mocs_entry {
    ($table:ident, $index:expr, $control:expr, $l3cc:expr) => {
        $table[$index] = DrmI915MocsEntry {
            control_value: $control,
            l3cc_value: $l3cc,
            used: 1,
        };
    };
}

macro_rules! gen9_mocs_entries {
    ($table:ident) => {
        mocs_entry!(
            $table,
            I915_MOCS_UNCACHED,
            LE_1_UC | LE_TC_2_LLC_ELLC,
            L3_1_UC
        );
        mocs_entry!(
            $table,
            I915_MOCS_PTE_INDEX,
            LE_0_PAGETABLE | LE_TC_0_PAGETABLE | le_lrum(3),
            L3_3_WB
        );
    };
}

macro_rules! gen11_mocs_entries {
    ($table:ident) => {
        mocs_entry!($table, 2, LE_3_WB | LE_TC_1_LLC | le_lrum(3), L3_3_WB);
        mocs_entry!($table, 3, LE_1_UC | LE_TC_1_LLC, L3_1_UC);
        mocs_entry!($table, 4, LE_1_UC | LE_TC_1_LLC, L3_3_WB);
        mocs_entry!($table, 5, LE_3_WB | LE_TC_1_LLC | le_lrum(3), L3_1_UC);
        mocs_entry!($table, 6, LE_3_WB | LE_TC_1_LLC | le_lrum(1), L3_1_UC);
        mocs_entry!($table, 7, LE_3_WB | LE_TC_1_LLC | le_lrum(1), L3_3_WB);
        mocs_entry!($table, 8, LE_3_WB | LE_TC_1_LLC | le_lrum(2), L3_1_UC);
        mocs_entry!($table, 9, LE_3_WB | LE_TC_1_LLC | le_lrum(2), L3_3_WB);
        mocs_entry!(
            $table,
            10,
            LE_3_WB | LE_TC_1_LLC | le_lrum(3) | le_aom(1),
            L3_1_UC
        );
        mocs_entry!(
            $table,
            11,
            LE_3_WB | LE_TC_1_LLC | le_lrum(3) | le_aom(1),
            L3_3_WB
        );
        mocs_entry!(
            $table,
            12,
            LE_3_WB | LE_TC_1_LLC | le_lrum(1) | le_aom(1),
            L3_1_UC
        );
        mocs_entry!(
            $table,
            13,
            LE_3_WB | LE_TC_1_LLC | le_lrum(1) | le_aom(1),
            L3_3_WB
        );
        mocs_entry!(
            $table,
            14,
            LE_3_WB | LE_TC_1_LLC | le_lrum(2) | le_aom(1),
            L3_1_UC
        );
        mocs_entry!(
            $table,
            15,
            LE_3_WB | LE_TC_1_LLC | le_lrum(2) | le_aom(1),
            L3_3_WB
        );
        mocs_entry!($table, 16, LE_1_UC | LE_TC_1_LLC | le_scf(1), L3_1_UC);
        mocs_entry!($table, 17, LE_1_UC | LE_TC_1_LLC | le_scf(1), L3_3_WB);
        mocs_entry!(
            $table,
            18,
            LE_3_WB | LE_TC_1_LLC | le_lrum(3) | le_sse(3),
            L3_3_WB
        );
        mocs_entry!(
            $table,
            19,
            LE_3_WB | LE_TC_1_LLC | le_lrum(3) | le_scc(7),
            L3_3_WB
        );
        mocs_entry!(
            $table,
            20,
            LE_3_WB | LE_TC_1_LLC | le_lrum(3) | le_scc(3),
            L3_3_WB
        );
        mocs_entry!(
            $table,
            21,
            LE_3_WB | LE_TC_1_LLC | le_lrum(3) | le_scc(1),
            L3_3_WB
        );
        mocs_entry!(
            $table,
            22,
            LE_3_WB | LE_TC_1_LLC | le_lrum(3) | le_rsc(1) | le_scc(3),
            L3_3_WB
        );
        mocs_entry!(
            $table,
            23,
            LE_3_WB | LE_TC_1_LLC | le_lrum(3) | le_rsc(1) | le_scc(7),
            L3_3_WB
        );
        mocs_entry!($table, 62, LE_3_WB | LE_TC_1_LLC | le_lrum(3), L3_1_UC);
        mocs_entry!($table, 63, LE_3_WB | LE_TC_1_LLC | le_lrum(3), L3_1_UC);
    };
}

static SKL_MOCS_TABLE: [DrmI915MocsEntry; 64] = const {
    let mut table = [UNUSED_MOCS_ENTRY; 64];
    gen9_mocs_entries!(table);
    mocs_entry!(
        table,
        I915_MOCS_CACHED,
        LE_3_WB | LE_TC_2_LLC_ELLC | le_lrum(3),
        L3_3_WB
    );
    // MOCS 63 is used by L3 evictions and must permit coherent LLC traffic.
    mocs_entry!(table, 63, LE_3_WB | LE_TC_1_LLC | le_lrum(3), L3_1_UC);
    table
};

static BROXTON_MOCS_TABLE: [DrmI915MocsEntry; 3] = const {
    let mut table = [UNUSED_MOCS_ENTRY; 3];
    gen9_mocs_entries!(table);
    mocs_entry!(
        table,
        I915_MOCS_CACHED,
        LE_1_UC | LE_TC_2_LLC_ELLC | le_lrum(3),
        L3_3_WB
    );
    table
};

static TGL_MOCS_TABLE: [DrmI915MocsEntry; 64] = const {
    let mut table = [UNUSED_MOCS_ENTRY; 64];
    // TGL/RKL retain the historical PTE index for ABI compatibility.
    mocs_entry!(
        table,
        I915_MOCS_PTE_INDEX,
        LE_0_PAGETABLE | LE_TC_0_PAGETABLE,
        L3_1_UC
    );
    gen11_mocs_entries!(table);
    mocs_entry!(table, 48, LE_3_WB | LE_TC_1_LLC | le_lrum(3), L3_3_WB);
    mocs_entry!(table, 49, LE_1_UC | LE_TC_1_LLC, L3_3_WB);
    mocs_entry!(table, 50, LE_3_WB | LE_TC_1_LLC | le_lrum(3), L3_1_UC);
    mocs_entry!(table, 51, LE_1_UC | LE_TC_1_LLC, L3_1_UC);
    mocs_entry!(table, 60, LE_3_WB | LE_TC_1_LLC | le_lrum(3), L3_1_UC);
    mocs_entry!(table, 61, LE_1_UC | LE_TC_1_LLC, L3_3_WB);
    table
};

static ICL_MOCS_TABLE: [DrmI915MocsEntry; 64] = const {
    let mut table = [UNUSED_MOCS_ENTRY; 64];
    mocs_entry!(table, I915_MOCS_UNCACHED, LE_1_UC | LE_TC_1_LLC, L3_1_UC);
    mocs_entry!(
        table,
        I915_MOCS_PTE_INDEX,
        LE_0_PAGETABLE | LE_TC_0_PAGETABLE,
        L3_3_WB
    );
    gen11_mocs_entries!(table);
    table
};

static DG1_MOCS_TABLE: [DrmI915MocsEntry; 64] = const {
    let mut table = [UNUSED_MOCS_ENTRY; 64];
    mocs_entry!(table, 1, 0, L3_1_UC);
    mocs_entry!(table, 5, 0, L3_3_WB);
    mocs_entry!(table, 6, 0, l3_esc(1) | l3_scc(1) | L3_3_WB);
    mocs_entry!(table, 7, 0, l3_esc(1) | l3_scc(3) | L3_3_WB);
    mocs_entry!(table, 8, 0, l3_esc(1) | l3_scc(7) | L3_3_WB);
    mocs_entry!(table, 48, 0, L3_3_WB);
    mocs_entry!(table, 49, 0, L3_1_UC);
    mocs_entry!(table, 60, 0, L3_1_UC);
    mocs_entry!(table, 61, 0, L3_1_UC);
    mocs_entry!(table, 62, 0, L3_1_UC);
    mocs_entry!(table, 63, 0, L3_1_UC);
    table
};

static GEN12_MOCS_TABLE: [DrmI915MocsEntry; 64] = const {
    let mut table = [UNUSED_MOCS_ENTRY; 64];
    gen11_mocs_entries!(table);
    mocs_entry!(table, 48, LE_3_WB | LE_TC_1_LLC | le_lrum(3), L3_3_WB);
    mocs_entry!(table, 49, LE_1_UC | LE_TC_1_LLC, L3_3_WB);
    mocs_entry!(table, 50, LE_3_WB | LE_TC_1_LLC | le_lrum(3), L3_1_UC);
    mocs_entry!(table, 51, LE_1_UC | LE_TC_1_LLC, L3_1_UC);
    mocs_entry!(table, 60, LE_3_WB | LE_TC_1_LLC | le_lrum(3), L3_1_UC);
    mocs_entry!(table, 61, LE_1_UC | LE_TC_1_LLC, L3_3_WB);
    table
};

static DG2_MOCS_TABLE: [DrmI915MocsEntry; 4] = const {
    let mut table = [UNUSED_MOCS_ENTRY; 4];
    mocs_entry!(table, 0, 0, L3_1_UC | l3_lkup(1));
    mocs_entry!(table, 1, 0, L3_1_UC | l3_glbgo(1) | l3_lkup(1));
    mocs_entry!(table, 2, 0, L3_1_UC | l3_glbgo(1));
    mocs_entry!(table, 3, 0, L3_3_WB | l3_lkup(1));
    table
};

static MTL_MOCS_TABLE: [DrmI915MocsEntry; 16] = const {
    let mut table = [UNUSED_MOCS_ENTRY; 16];
    mocs_entry!(table, 0, ig_pat(0), l3_lkup(1) | L3_3_WB);
    mocs_entry!(table, 1, ig_pat(1), l3_lkup(1) | L3_3_WB);
    mocs_entry!(table, 2, ig_pat(1), l3_lkup(1) | L3_1_UC);
    mocs_entry!(table, 3, ig_pat(1) | L4_3_UC, l3_lkup(1) | L3_1_UC);
    mocs_entry!(table, 4, ig_pat(1), l3_lkup(1) | l3_glbgo(1) | L3_1_UC);
    mocs_entry!(
        table,
        5,
        ig_pat(1) | L4_3_UC,
        l3_lkup(1) | l3_glbgo(1) | L3_1_UC
    );
    mocs_entry!(table, 6, ig_pat(1), L3_1_UC);
    mocs_entry!(table, 7, ig_pat(1) | L4_3_UC, L3_1_UC);
    mocs_entry!(table, 8, ig_pat(1), l3_glbgo(1) | L3_1_UC);
    mocs_entry!(table, 9, ig_pat(1) | L4_3_UC, l3_glbgo(1) | L3_1_UC);
    mocs_entry!(table, 14, ig_pat(1) | L4_1_WT, l3_lkup(1) | L3_3_WB);
    mocs_entry!(table, 15, ig_pat(1), l3_glbgo(1) | L3_1_UC);
    table
};

#[repr(C)]
struct DrmI915MocsTable {
    size: u32,
    n_entries: u32,
    table: *const DrmI915MocsEntry,
    uc_index: u8,
    wb_index: u8,
    unused_entries_index: u8,
}

impl Default for DrmI915MocsTable {
    fn default() -> Self {
        Self {
            size: 0,
            n_entries: 0,
            table: core::ptr::null(),
            uc_index: 0,
            wb_index: 0,
            unused_entries_index: I915_MOCS_PTE_INDEX as u8,
        }
    }
}

const MOCS_OFFSET: [u32; 19] = [
    0xc800, 0xcc00, 0, 0, 0, 0, 0, 0, 0, 0, 0xc900, 0xca00, 0x10000, 0, 0, 0, 0, 0, 0xcb00,
];

unsafe fn has_global_mocs_registers(i915: *const DrmI915Private) -> bool {
    let info = unsafe { crate::linux::i915::INTEL_INFO(i915) };
    !info.is_null() && (unsafe { (*info).flags[1] } & (1 << 1)) != 0
}

unsafe fn is_gen9_bc(i915: *const DrmI915Private) -> bool {
    unsafe { GRAPHICS_VER(i915) == 9 && !IS_GEN9_LP(i915) }
}

// upstream: intel_mocs.c has_l3cc()
fn has_l3cc(_i915: *const DrmI915Private) -> bool {
    true
}

// upstream: intel_mocs.c has_global_mocs()
unsafe fn has_global_mocs(i915: *const DrmI915Private) -> bool {
    unsafe { has_global_mocs_registers(i915) }
}

// upstream: intel_mocs.c has_mocs()
unsafe fn has_mocs(i915: *const DrmI915Private) -> bool {
    !unsafe { IS_DGFX(i915) }
}

// upstream: intel_mocs.c get_mocs_settings()
unsafe fn get_mocs_settings(i915: *mut DrmI915Private, table: &mut DrmI915MocsTable) -> u32 {
    *table = DrmI915MocsTable::default();
    if unsafe { IS_GFX_GT_IP_RANGE(to_gt(i915), IP_VER(12, 70), IP_VER(12, 74)) } {
        table.size = MTL_MOCS_TABLE.len() as u32;
        table.table = MTL_MOCS_TABLE.as_ptr();
        table.n_entries = MTL_NUM_MOCS_ENTRIES as u32;
        table.uc_index = 9;
        table.unused_entries_index = 1;
    } else if unsafe { IS_DG2(i915) } {
        table.size = DG2_MOCS_TABLE.len() as u32;
        table.table = DG2_MOCS_TABLE.as_ptr();
        table.uc_index = 1;
        table.n_entries = GEN9_NUM_MOCS_ENTRIES as u32;
        table.unused_entries_index = 3;
    } else if unsafe { IS_DG1(i915) } {
        table.size = DG1_MOCS_TABLE.len() as u32;
        table.table = DG1_MOCS_TABLE.as_ptr();
        table.uc_index = 1;
        table.n_entries = GEN9_NUM_MOCS_ENTRIES as u32;
        table.unused_entries_index = 5;
    } else if unsafe { IS_TIGERLAKE(i915) || IS_ROCKETLAKE(i915) } {
        table.size = TGL_MOCS_TABLE.len() as u32;
        table.table = TGL_MOCS_TABLE.as_ptr();
        table.n_entries = GEN9_NUM_MOCS_ENTRIES as u32;
        table.uc_index = 3;
    } else if unsafe { GRAPHICS_VER(i915) >= 12 } {
        table.size = GEN12_MOCS_TABLE.len() as u32;
        table.table = GEN12_MOCS_TABLE.as_ptr();
        table.n_entries = GEN9_NUM_MOCS_ENTRIES as u32;
        table.uc_index = 3;
        table.unused_entries_index = 2;
    } else if unsafe { GRAPHICS_VER(i915) == 11 } {
        table.size = ICL_MOCS_TABLE.len() as u32;
        table.table = ICL_MOCS_TABLE.as_ptr();
        table.n_entries = GEN9_NUM_MOCS_ENTRIES as u32;
    } else if unsafe { is_gen9_bc(i915) } {
        table.size = SKL_MOCS_TABLE.len() as u32;
        table.n_entries = GEN9_NUM_MOCS_ENTRIES as u32;
        table.table = SKL_MOCS_TABLE.as_ptr();
    } else if unsafe { IS_GEN9_LP(i915) } {
        table.size = BROXTON_MOCS_TABLE.len() as u32;
        table.n_entries = GEN9_NUM_MOCS_ENTRIES as u32;
        table.table = BROXTON_MOCS_TABLE.as_ptr();
    } else {
        if unsafe { GRAPHICS_VER(i915) >= 9 } {
            WARN_ONCE!(true, "Platform that should have a MOCS table does not.");
        }
        return 0;
    }

    if GEM_DEBUG_WARN_ON!(table.size > table.n_entries) {
        return 0;
    }

    // WaDisableSkipCaching:skl,bxt,kbl,glk
    if unsafe { GRAPHICS_VER(i915) == 9 } {
        let mut i = 0;
        while i < table.size as usize {
            let entry = unsafe { *table.table.add(i) };
            if GEM_DEBUG_WARN_ON!(entry.l3cc_value & (l3_esc(1) | l3_scc(0x7)) != 0) {
                return 0;
            }
            i += 1;
        }
    }

    let mut flags = 0;
    if unsafe { has_mocs(i915) } {
        if unsafe { has_global_mocs(i915) } {
            flags |= HAS_GLOBAL_MOCS;
        } else {
            flags |= HAS_ENGINE_MOCS;
        }
    }
    if has_l3cc(i915) {
        flags |= HAS_RENDER_L3CC;
    }
    flags
}

// upstream: intel_mocs.c get_entry_control()
unsafe fn get_entry_control(table: &DrmI915MocsTable, index: usize) -> u32 {
    if index < table.size as usize {
        let entry = unsafe { *table.table.add(index) };
        if entry.used != 0 {
            return entry.control_value;
        }
    }
    unsafe { (*table.table.add(table.unused_entries_index as usize)).control_value }
}

// upstream: intel_mocs.c __init_mocs_table()
unsafe fn __init_mocs_table(uncore: *mut IntelUncore, table: &DrmI915MocsTable, addr: u32) {
    WARN_ONCE!(
        table.unused_entries_index == 0,
        "Unused entries index should have been defined"
    );
    let mut i = 0;
    while i < table.n_entries as usize {
        let value = unsafe { get_entry_control(table, i) };
        unsafe {
            intel_uncore_write_fw(
                uncore,
                I915RegT {
                    reg: addr.wrapping_add((i as u32).wrapping_mul(4)),
                },
                value,
            );
        }
        i += 1;
    }
}

// upstream: intel_mocs.c mocs_offset()
unsafe fn mocs_offset(engine: *const IntelEngineCs) -> u32 {
    let id = unsafe { (*engine).id };
    GEM_BUG_ON!(id < 0 || id as usize >= MOCS_OFFSET.len());
    MOCS_OFFSET[id as usize]
}

// upstream: intel_mocs.c init_mocs_table()
unsafe fn init_mocs_table(engine: *mut IntelEngineCs, table: &DrmI915MocsTable) {
    let addr = unsafe { mocs_offset(engine) };
    unsafe { __init_mocs_table((*engine).uncore, table, addr) };
}

// upstream: intel_mocs.c get_entry_l3cc()
unsafe fn get_entry_l3cc(table: &DrmI915MocsTable, index: usize) -> u16 {
    if index < table.size as usize {
        let entry = unsafe { *table.table.add(index) };
        if entry.used != 0 {
            return entry.l3cc_value;
        }
    }
    unsafe { (*table.table.add(table.unused_entries_index as usize)).l3cc_value }
}

// upstream: intel_mocs.c l3cc_combine()
fn l3cc_combine(low: u16, high: u16) -> u32 {
    low as u32 | ((high as u32) << 16)
}

// upstream: intel_mocs.c init_l3cc_table()
unsafe fn init_l3cc_table(gt: *mut IntelGt, table: &DrmI915MocsTable) {
    let mut flags = 0usize;
    unsafe { intel_gt_mcr_lock(gt, &mut flags) };
    let mut i = 0;
    while i < (table.n_entries as usize + 1) / 2 {
        let low = unsafe { get_entry_l3cc(table, 2 * i) };
        let high = unsafe { get_entry_l3cc(table, 2 * i + 1) };
        let value = l3cc_combine(low, high);
        let offset = 0xb020u32.wrapping_add((i as u32).wrapping_mul(4));
        if unsafe { GRAPHICS_VER_FULL((*gt).i915) >= IP_VER(12, 55) } {
            unsafe {
                intel_gt_mcr_multicast_write_fw(gt, I915McrRegT { reg: offset }, value);
            }
        } else {
            unsafe { intel_uncore_write_fw((*gt).uncore, I915RegT { reg: offset }, value) };
        }
        i += 1;
    }
    unsafe { intel_gt_mcr_unlock(gt, flags) };
}

// upstream: intel_mocs.c intel_mocs_init_engine()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_mocs_init_engine(engine: *mut IntelEngineCs) {
    unsafe { assert_forcewakes_active((*engine).uncore, FORCEWAKE_ALL) };
    let mut table = DrmI915MocsTable::default();
    let flags = unsafe { get_mocs_settings((*engine).i915, &mut table) };
    if flags == 0 {
        return;
    }
    if flags & HAS_ENGINE_MOCS != 0 {
        unsafe { init_mocs_table(engine, &table) };
    }
    if flags & HAS_RENDER_L3CC != 0 && unsafe { (*engine).class as i32 == RENDER_CLASS } {
        unsafe { init_l3cc_table((*engine).gt, &table) };
    }
}

// upstream: intel_mocs.c global_mocs_offset()
fn global_mocs_offset() -> u32 {
    crate::linux::i915::i915_mmio_reg_offset(I915RegT { reg: 0x4000 })
}

// upstream: intel_mocs.c intel_set_mocs_index()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_set_mocs_index(gt: *mut IntelGt) {
    let mut table = DrmI915MocsTable::default();
    unsafe { get_mocs_settings((*gt).i915, &mut table) };
    unsafe {
        (*gt).mocs.uc_index = table.uc_index;
        if HAS_L3_CCS_READ((*gt).i915) {
            (*gt).mocs.wb_index = table.wb_index;
        }
    }
}

// upstream: intel_mocs.c intel_mocs_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_mocs_init(gt: *mut IntelGt) {
    let mut table = DrmI915MocsTable::default();
    let flags = unsafe { get_mocs_settings((*gt).i915, &mut table) };
    if flags & HAS_GLOBAL_MOCS != 0 {
        let addr = global_mocs_offset();
        unsafe { __init_mocs_table((*gt).uncore, &table, addr) };
    }
    if flags & HAS_RENDER_L3CC != 0 {
        unsafe { init_l3cc_table(gt, &table) };
    }
}
