// SPDX-License-Identifier: MIT
// Copyright © 2014 Intel Corporation.
//
// Source-faithful Rust transcription of Linux 7.2.3
// drivers/gpu/drm/i915/gt/intel_lrc.c. This module is registered; upstream
// kernel GEM/RCU/register helpers remain explicit binding points, not
// replacements.

use core::{ffi::c_void, mem::size_of};

use crate::{
    i915_request_types_upstream::*,
    i915_scheduler_types_upstream::*,
    intel_context_types_upstream::*,
    intel_context_upstream::*,
    intel_engine_cs_upstream::{I915WaContextBatch as I915WaCtxBb, *},
    intel_engine_types_upstream::IntelEngineCs,
    intel_gt_types_upstream::IntelGt,
    intel_ring::{CACHELINE_BYTES, PAGE_SIZE},
    intel_timeline_types_upstream::IntelTimeline,
    linux_config::*,
    linux_list::*,
};

// upstream: intel_lrc.c set_offsets()
unsafe fn set_offsets(
    mut regs: *mut u32,
    mut data: *const u8,
    engine: *const IntelEngineCs,
    close: bool,
) {
    let base = (*engine).mmio_base;

    while *data != 0 {
        let mut count: u8;
        let flags: u8;

        if *data & (1 << 7) != 0 {
            count = *data & !(1 << 7);
            data = data.add(1);
            regs = regs.add(count as usize);
            continue;
        }

        count = *data & 0x3f;
        flags = *data >> 6;
        data = data.add(1);

        *regs = mi_load_register_imm(count as u32);
        if flags & POSTED != 0 {
            *regs |= MI_LRI_FORCE_POSTED;
        }
        if graphics_ver((*engine).i915) >= 11 {
            *regs |= MI_LRI_LRM_CS_MMIO;
        }
        regs = regs.add(1);

        GEM_BUG_ON!(count == 0);
        loop {
            let mut offset = 0u32;
            let mut v: u8;
            loop {
                v = *data;
                data = data.add(1);
                offset <<= 7;
                offset |= (v & !(1 << 7)) as u32;
                if v & (1 << 7) == 0 {
                    break;
                }
            }

            *regs = base + (offset << 2);
            regs = regs.add(2);
            count -= 1;
            if count == 0 {
                break;
            }
        }
    }

    if close {
        // Close the batch; used mainly by live_lrc_layout().
        *regs = MI_BATCH_BUFFER_END;
        if graphics_ver((*engine).i915) >= 11 {
            *regs |= 1;
        }
    }
}

const POSTED: u8 = 1;
const fn nop(count: u8) -> u8 {
    (1 << 7) | count
}
const fn lri(count: u8, flags: u8) -> u8 {
    assert!(count < (1 << 6));
    (flags << 6) | count
}
const fn reg(offset: u32) -> u8 {
    assert!(offset < 0x200);
    (offset >> 2) as u8
}
const fn reg16_hi(offset: u32) -> u8 {
    assert!(offset < 0x10000);
    ((offset >> 9) | (1 << 7)) as u8
}
const fn reg16_lo(offset: u32) -> u8 {
    ((offset >> 2) & 0x7f) as u8
}

static GEN8_XCS_OFFSETS: &[u8] = &[
    nop(1),
    lri(11, 0),
    reg16_hi(0x244),
    reg16_lo(0x244),
    reg(0x034),
    reg(0x030),
    reg(0x038),
    reg(0x03c),
    reg(0x168),
    reg(0x140),
    reg(0x110),
    reg(0x11c),
    reg(0x114),
    reg(0x118),
    nop(9),
    lri(9, 0),
    reg16_hi(0x3a8),
    reg16_lo(0x3a8),
    reg16_hi(0x28c),
    reg16_lo(0x28c),
    reg16_hi(0x288),
    reg16_lo(0x288),
    reg16_hi(0x284),
    reg16_lo(0x284),
    reg16_hi(0x280),
    reg16_lo(0x280),
    reg16_hi(0x27c),
    reg16_lo(0x27c),
    reg16_hi(0x278),
    reg16_lo(0x278),
    reg16_hi(0x274),
    reg16_lo(0x274),
    reg16_hi(0x270),
    reg16_lo(0x270),
    nop(13),
    lri(2, 0),
    reg16_hi(0x200),
    reg16_lo(0x200),
    reg(0x028),
    0,
];

static GEN9_XCS_OFFSETS: &[u8] = &[
    nop(1),
    lri(14, POSTED),
    reg16_hi(0x244),
    reg16_lo(0x244),
    reg(0x034),
    reg(0x030),
    reg(0x038),
    reg(0x03c),
    reg(0x168),
    reg(0x140),
    reg(0x110),
    reg(0x11c),
    reg(0x114),
    reg(0x118),
    reg(0x1c0),
    reg(0x1c4),
    reg(0x1c8),
    nop(3),
    lri(9, POSTED),
    reg16_hi(0x3a8),
    reg16_lo(0x3a8),
    reg16_hi(0x28c),
    reg16_lo(0x28c),
    reg16_hi(0x288),
    reg16_lo(0x288),
    reg16_hi(0x284),
    reg16_lo(0x284),
    reg16_hi(0x280),
    reg16_lo(0x280),
    reg16_hi(0x27c),
    reg16_lo(0x27c),
    reg16_hi(0x278),
    reg16_lo(0x278),
    reg16_hi(0x274),
    reg16_lo(0x274),
    reg16_hi(0x270),
    reg16_lo(0x270),
    nop(13),
    lri(1, POSTED),
    reg16_hi(0x200),
    reg16_lo(0x200),
    nop(13),
    lri(44, POSTED),
    reg(0x028),
    reg(0x09c),
    reg(0x0c0),
    reg(0x178),
    reg(0x17c),
    reg16_hi(0x358),
    reg16_lo(0x358),
    reg(0x170),
    reg(0x150),
    reg(0x154),
    reg(0x158),
    reg16_hi(0x41c),
    reg16_lo(0x41c),
    reg16_hi(0x600),
    reg16_lo(0x600),
    reg16_hi(0x604),
    reg16_lo(0x604),
    reg16_hi(0x608),
    reg16_lo(0x608),
    reg16_hi(0x60c),
    reg16_lo(0x60c),
    reg16_hi(0x610),
    reg16_lo(0x610),
    reg16_hi(0x614),
    reg16_lo(0x614),
    reg16_hi(0x618),
    reg16_lo(0x618),
    reg16_hi(0x61c),
    reg16_lo(0x61c),
    reg16_hi(0x620),
    reg16_lo(0x620),
    reg16_hi(0x624),
    reg16_lo(0x624),
    reg16_hi(0x628),
    reg16_lo(0x628),
    reg16_hi(0x62c),
    reg16_lo(0x62c),
    reg16_hi(0x630),
    reg16_lo(0x630),
    reg16_hi(0x634),
    reg16_lo(0x634),
    reg16_hi(0x638),
    reg16_lo(0x638),
    reg16_hi(0x63c),
    reg16_lo(0x63c),
    reg16_hi(0x640),
    reg16_lo(0x640),
    reg16_hi(0x644),
    reg16_lo(0x644),
    reg16_hi(0x648),
    reg16_lo(0x648),
    reg16_hi(0x64c),
    reg16_lo(0x64c),
    reg16_hi(0x650),
    reg16_lo(0x650),
    reg16_hi(0x654),
    reg16_lo(0x654),
    reg16_hi(0x658),
    reg16_lo(0x658),
    reg16_hi(0x65c),
    reg16_lo(0x65c),
    reg16_hi(0x660),
    reg16_lo(0x660),
    reg16_hi(0x664),
    reg16_lo(0x664),
    reg16_hi(0x668),
    reg16_lo(0x668),
    reg16_hi(0x66c),
    reg16_lo(0x66c),
    reg16_hi(0x670),
    reg16_lo(0x670),
    reg16_hi(0x674),
    reg16_lo(0x674),
    reg16_hi(0x678),
    reg16_lo(0x678),
    reg16_hi(0x67c),
    reg16_lo(0x67c),
    reg(0x068),
    0,
];

static GEN12_XCS_OFFSETS: &[u8] = &[
    nop(1),
    lri(13, POSTED),
    reg16_hi(0x244),
    reg16_lo(0x244),
    reg(0x034),
    reg(0x030),
    reg(0x038),
    reg(0x03c),
    reg(0x168),
    reg(0x140),
    reg(0x110),
    reg(0x1c0),
    reg(0x1c4),
    reg(0x1c8),
    reg(0x180),
    reg16_hi(0x2b4),
    reg16_lo(0x2b4),
    nop(5),
    lri(9, POSTED),
    reg16_hi(0x3a8),
    reg16_lo(0x3a8),
    reg16_hi(0x28c),
    reg16_lo(0x28c),
    reg16_hi(0x288),
    reg16_lo(0x288),
    reg16_hi(0x284),
    reg16_lo(0x284),
    reg16_hi(0x280),
    reg16_lo(0x280),
    reg16_hi(0x27c),
    reg16_lo(0x27c),
    reg16_hi(0x278),
    reg16_lo(0x278),
    reg16_hi(0x274),
    reg16_lo(0x274),
    reg16_hi(0x270),
    reg16_lo(0x270),
    0,
];

static DG2_XCS_OFFSETS: &[u8] = &[
    nop(1),
    lri(15, POSTED),
    reg16_hi(0x244),
    reg16_lo(0x244),
    reg(0x034),
    reg(0x030),
    reg(0x038),
    reg(0x03c),
    reg(0x168),
    reg(0x140),
    reg(0x110),
    reg(0x1c0),
    reg(0x1c4),
    reg(0x1c8),
    reg(0x180),
    reg16_hi(0x2b4),
    reg16_lo(0x2b4),
    reg(0x120),
    reg(0x124),
    nop(1),
    lri(9, POSTED),
    reg16_hi(0x3a8),
    reg16_lo(0x3a8),
    reg16_hi(0x28c),
    reg16_lo(0x28c),
    reg16_hi(0x288),
    reg16_lo(0x288),
    reg16_hi(0x284),
    reg16_lo(0x284),
    reg16_hi(0x280),
    reg16_lo(0x280),
    reg16_hi(0x27c),
    reg16_lo(0x27c),
    reg16_hi(0x278),
    reg16_lo(0x278),
    reg16_hi(0x274),
    reg16_lo(0x274),
    reg16_hi(0x270),
    reg16_lo(0x270),
    0,
];

static GEN8_RCS_OFFSETS: &[u8] = &[
    nop(1),
    lri(14, POSTED),
    reg16_hi(0x244),
    reg16_lo(0x244),
    reg(0x034),
    reg(0x030),
    reg(0x038),
    reg(0x03c),
    reg(0x168),
    reg(0x140),
    reg(0x110),
    reg(0x11c),
    reg(0x114),
    reg(0x118),
    reg(0x1c0),
    reg(0x1c4),
    reg(0x1c8),
    nop(3),
    lri(9, POSTED),
    reg16_hi(0x3a8),
    reg16_lo(0x3a8),
    reg16_hi(0x28c),
    reg16_lo(0x28c),
    reg16_hi(0x288),
    reg16_lo(0x288),
    reg16_hi(0x284),
    reg16_lo(0x284),
    reg16_hi(0x280),
    reg16_lo(0x280),
    reg16_hi(0x27c),
    reg16_lo(0x27c),
    reg16_hi(0x278),
    reg16_lo(0x278),
    reg16_hi(0x274),
    reg16_lo(0x274),
    reg16_hi(0x270),
    reg16_lo(0x270),
    nop(13),
    lri(1, 0),
    reg(0x0c8),
    0,
];

static GEN9_RCS_OFFSETS: &[u8] = &[
    nop(1),
    lri(14, POSTED),
    reg16_hi(0x244),
    reg16_lo(0x244),
    reg(0x34),
    reg(0x30),
    reg(0x38),
    reg(0x3c),
    reg(0x168),
    reg(0x140),
    reg(0x110),
    reg(0x11c),
    reg(0x114),
    reg(0x118),
    reg(0x1c0),
    reg(0x1c4),
    reg(0x1c8),
    nop(3),
    lri(9, POSTED),
    reg16_hi(0x3a8),
    reg16_lo(0x3a8),
    reg16_hi(0x28c),
    reg16_lo(0x28c),
    reg16_hi(0x288),
    reg16_lo(0x288),
    reg16_hi(0x284),
    reg16_lo(0x284),
    reg16_hi(0x280),
    reg16_lo(0x280),
    reg16_hi(0x27c),
    reg16_lo(0x27c),
    reg16_hi(0x278),
    reg16_lo(0x278),
    reg16_hi(0x274),
    reg16_lo(0x274),
    reg16_hi(0x270),
    reg16_lo(0x270),
    nop(13),
    lri(1, 0),
    reg(0xc8),
    nop(13),
    lri(44, POSTED),
    reg(0x28),
    reg(0x9c),
    reg(0xc0),
    reg(0x178),
    reg(0x17c),
    reg16_hi(0x358),
    reg16_lo(0x358),
    reg(0x170),
    reg(0x150),
    reg(0x154),
    reg(0x158),
    reg16_hi(0x41c),
    reg16_lo(0x41c),
    reg16_hi(0x600),
    reg16_lo(0x600),
    reg16_hi(0x604),
    reg16_lo(0x604),
    reg16_hi(0x608),
    reg16_lo(0x608),
    reg16_hi(0x60c),
    reg16_lo(0x60c),
    reg16_hi(0x610),
    reg16_lo(0x610),
    reg16_hi(0x614),
    reg16_lo(0x614),
    reg16_hi(0x618),
    reg16_lo(0x618),
    reg16_hi(0x61c),
    reg16_lo(0x61c),
    reg16_hi(0x620),
    reg16_lo(0x620),
    reg16_hi(0x624),
    reg16_lo(0x624),
    reg16_hi(0x628),
    reg16_lo(0x628),
    reg16_hi(0x62c),
    reg16_lo(0x62c),
    reg16_hi(0x630),
    reg16_lo(0x630),
    reg16_hi(0x634),
    reg16_lo(0x634),
    reg16_hi(0x638),
    reg16_lo(0x638),
    reg16_hi(0x63c),
    reg16_lo(0x63c),
    reg16_hi(0x640),
    reg16_lo(0x640),
    reg16_hi(0x644),
    reg16_lo(0x644),
    reg16_hi(0x648),
    reg16_lo(0x648),
    reg16_hi(0x64c),
    reg16_lo(0x64c),
    reg16_hi(0x650),
    reg16_lo(0x650),
    reg16_hi(0x654),
    reg16_lo(0x654),
    reg16_hi(0x658),
    reg16_lo(0x658),
    reg16_hi(0x65c),
    reg16_lo(0x65c),
    reg16_hi(0x660),
    reg16_lo(0x660),
    reg16_hi(0x664),
    reg16_lo(0x664),
    reg16_hi(0x668),
    reg16_lo(0x668),
    reg16_hi(0x66c),
    reg16_lo(0x66c),
    reg16_hi(0x670),
    reg16_lo(0x670),
    reg16_hi(0x674),
    reg16_lo(0x674),
    reg16_hi(0x678),
    reg16_lo(0x678),
    reg16_hi(0x67c),
    reg16_lo(0x67c),
    reg(0x68),
    0,
];

static GEN11_RCS_OFFSETS: &[u8] = &[
    nop(1),
    lri(15, POSTED),
    reg16_hi(0x244),
    reg16_lo(0x244),
    reg(0x034),
    reg(0x030),
    reg(0x038),
    reg(0x03c),
    reg(0x168),
    reg(0x140),
    reg(0x110),
    reg(0x11c),
    reg(0x114),
    reg(0x118),
    reg(0x1c0),
    reg(0x1c4),
    reg(0x1c8),
    reg(0x180),
    nop(1),
    lri(9, POSTED),
    reg16_hi(0x3a8),
    reg16_lo(0x3a8),
    reg16_hi(0x28c),
    reg16_lo(0x28c),
    reg16_hi(0x288),
    reg16_lo(0x288),
    reg16_hi(0x284),
    reg16_lo(0x284),
    reg16_hi(0x280),
    reg16_lo(0x280),
    reg16_hi(0x27c),
    reg16_lo(0x27c),
    reg16_hi(0x278),
    reg16_lo(0x278),
    reg16_hi(0x274),
    reg16_lo(0x274),
    reg16_hi(0x270),
    reg16_lo(0x270),
    lri(1, POSTED),
    reg(0x1b0),
    nop(10),
    lri(1, 0),
    reg(0x0c8),
    0,
];

static GEN12_RCS_OFFSETS: &[u8] = &[
    nop(1),
    lri(13, POSTED),
    reg16_hi(0x244),
    reg16_lo(0x244),
    reg(0x034),
    reg(0x030),
    reg(0x038),
    reg(0x03c),
    reg(0x168),
    reg(0x140),
    reg(0x110),
    reg(0x1c0),
    reg(0x1c4),
    reg(0x1c8),
    reg(0x180),
    reg16_hi(0x2b4),
    reg16_lo(0x2b4),
    nop(5),
    lri(9, POSTED),
    reg16_hi(0x3a8),
    reg16_lo(0x3a8),
    reg16_hi(0x28c),
    reg16_lo(0x28c),
    reg16_hi(0x288),
    reg16_lo(0x288),
    reg16_hi(0x284),
    reg16_lo(0x284),
    reg16_hi(0x280),
    reg16_lo(0x280),
    reg16_hi(0x27c),
    reg16_lo(0x27c),
    reg16_hi(0x278),
    reg16_lo(0x278),
    reg16_hi(0x274),
    reg16_lo(0x274),
    reg16_hi(0x270),
    reg16_lo(0x270),
    lri(3, POSTED),
    reg(0x1b0),
    reg16_hi(0x5a8),
    reg16_lo(0x5a8),
    reg16_hi(0x5ac),
    reg16_lo(0x5ac),
    nop(6),
    lri(1, 0),
    reg(0x0c8),
    nop(3 + 9 + 1),
    lri(51, POSTED),
    reg16_hi(0x588),
    reg16_lo(0x588),
    reg16_hi(0x588),
    reg16_lo(0x588),
    reg16_hi(0x588),
    reg16_lo(0x588),
    reg16_hi(0x588),
    reg16_lo(0x588),
    reg16_hi(0x588),
    reg16_lo(0x588),
    reg16_hi(0x588),
    reg16_lo(0x588),
    reg(0x028),
    reg(0x09c),
    reg(0x0c0),
    reg(0x178),
    reg(0x17c),
    reg16_hi(0x358),
    reg16_lo(0x358),
    reg(0x170),
    reg(0x150),
    reg(0x154),
    reg(0x158),
    reg16_hi(0x41c),
    reg16_lo(0x41c),
    reg16_hi(0x600),
    reg16_lo(0x600),
    reg16_hi(0x604),
    reg16_lo(0x604),
    reg16_hi(0x608),
    reg16_lo(0x608),
    reg16_hi(0x60c),
    reg16_lo(0x60c),
    reg16_hi(0x610),
    reg16_lo(0x610),
    reg16_hi(0x614),
    reg16_lo(0x614),
    reg16_hi(0x618),
    reg16_lo(0x618),
    reg16_hi(0x61c),
    reg16_lo(0x61c),
    reg16_hi(0x620),
    reg16_lo(0x620),
    reg16_hi(0x624),
    reg16_lo(0x624),
    reg16_hi(0x628),
    reg16_lo(0x628),
    reg16_hi(0x62c),
    reg16_lo(0x62c),
    reg16_hi(0x630),
    reg16_lo(0x630),
    reg16_hi(0x634),
    reg16_lo(0x634),
    reg16_hi(0x638),
    reg16_lo(0x638),
    reg16_hi(0x63c),
    reg16_lo(0x63c),
    reg16_hi(0x640),
    reg16_lo(0x640),
    reg16_hi(0x644),
    reg16_lo(0x644),
    reg16_hi(0x648),
    reg16_lo(0x648),
    reg16_hi(0x64c),
    reg16_lo(0x64c),
    reg16_hi(0x650),
    reg16_lo(0x650),
    reg16_hi(0x654),
    reg16_lo(0x654),
    reg16_hi(0x658),
    reg16_lo(0x658),
    reg16_hi(0x65c),
    reg16_lo(0x65c),
    reg16_hi(0x660),
    reg16_lo(0x660),
    reg16_hi(0x664),
    reg16_lo(0x664),
    reg16_hi(0x668),
    reg16_lo(0x668),
    reg16_hi(0x66c),
    reg16_lo(0x66c),
    reg16_hi(0x670),
    reg16_lo(0x670),
    reg16_hi(0x674),
    reg16_lo(0x674),
    reg16_hi(0x678),
    reg16_lo(0x678),
    reg16_hi(0x67c),
    reg16_lo(0x67c),
    reg(0x068),
    reg(0x084),
    nop(1),
    0,
];

static DG2_RCS_OFFSETS: &[u8] = &[
    nop(1),
    lri(15, POSTED),
    reg16_hi(0x244),
    reg16_lo(0x244),
    reg(0x034),
    reg(0x030),
    reg(0x038),
    reg(0x03c),
    reg(0x168),
    reg(0x140),
    reg(0x110),
    reg(0x1c0),
    reg(0x1c4),
    reg(0x1c8),
    reg(0x180),
    reg16_hi(0x2b4),
    reg16_lo(0x2b4),
    reg(0x120),
    reg(0x124),
    nop(1),
    lri(9, POSTED),
    reg16_hi(0x3a8),
    reg16_lo(0x3a8),
    reg16_hi(0x28c),
    reg16_lo(0x28c),
    reg16_hi(0x288),
    reg16_lo(0x288),
    reg16_hi(0x284),
    reg16_lo(0x284),
    reg16_hi(0x280),
    reg16_lo(0x280),
    reg16_hi(0x27c),
    reg16_lo(0x27c),
    reg16_hi(0x278),
    reg16_lo(0x278),
    reg16_hi(0x274),
    reg16_lo(0x274),
    reg16_hi(0x270),
    reg16_lo(0x270),
    lri(3, POSTED),
    reg(0x1b0),
    reg16_hi(0x5a8),
    reg16_lo(0x5a8),
    reg16_hi(0x5ac),
    reg16_lo(0x5ac),
    nop(6),
    lri(1, 0),
    reg(0x0c8),
    0,
];

static MTL_RCS_OFFSETS: &[u8] = &[
    nop(1),
    lri(15, POSTED),
    reg16_hi(0x244),
    reg16_lo(0x244),
    reg(0x034),
    reg(0x030),
    reg(0x038),
    reg(0x03c),
    reg(0x168),
    reg(0x140),
    reg(0x110),
    reg(0x1c0),
    reg(0x1c4),
    reg(0x1c8),
    reg(0x180),
    reg16_hi(0x2b4),
    reg16_lo(0x2b4),
    reg(0x120),
    reg(0x124),
    nop(1),
    lri(9, POSTED),
    reg16_hi(0x3a8),
    reg16_lo(0x3a8),
    reg16_hi(0x28c),
    reg16_lo(0x28c),
    reg16_hi(0x288),
    reg16_lo(0x288),
    reg16_hi(0x284),
    reg16_lo(0x284),
    reg16_hi(0x280),
    reg16_lo(0x280),
    reg16_hi(0x27c),
    reg16_lo(0x27c),
    reg16_hi(0x278),
    reg16_lo(0x278),
    reg16_hi(0x274),
    reg16_lo(0x274),
    reg16_hi(0x270),
    reg16_lo(0x270),
    nop(2),
    lri(2, POSTED),
    reg16_hi(0x5a8),
    reg16_lo(0x5a8),
    reg16_hi(0x5ac),
    reg16_lo(0x5ac),
    nop(6),
    lri(1, 0),
    reg(0x0c8),
    0,
];

// upstream: intel_lrc.c reg_offsets()
unsafe fn reg_offsets(engine: *const IntelEngineCs) -> *const u8 {
    // Gen12+ lists only program basic default-state registers and use relative
    // addressing to fix state between physical engines in a virtual engine.
    GEM_BUG_ON!(graphics_ver((*engine).i915) >= 12 && !intel_engine_has_relative_mmio(engine));

    if (*engine).flags & I915_ENGINE_HAS_RCS_REG_STATE != 0 {
        if graphics_ver_full((*engine).i915) >= IP_VER(12, 70) {
            MTL_RCS_OFFSETS.as_ptr()
        } else if graphics_ver_full((*engine).i915) >= IP_VER(12, 55) {
            DG2_RCS_OFFSETS.as_ptr()
        } else if graphics_ver((*engine).i915) >= 12 {
            GEN12_RCS_OFFSETS.as_ptr()
        } else if graphics_ver((*engine).i915) >= 11 {
            GEN11_RCS_OFFSETS.as_ptr()
        } else if graphics_ver((*engine).i915) >= 9 {
            GEN9_RCS_OFFSETS.as_ptr()
        } else {
            GEN8_RCS_OFFSETS.as_ptr()
        }
    } else if graphics_ver_full((*engine).i915) >= IP_VER(12, 55) {
        DG2_XCS_OFFSETS.as_ptr()
    } else if graphics_ver((*engine).i915) >= 12 {
        GEN12_XCS_OFFSETS.as_ptr()
    } else if graphics_ver((*engine).i915) >= 9 {
        GEN9_XCS_OFFSETS.as_ptr()
    } else {
        GEN8_XCS_OFFSETS.as_ptr()
    }
}

// upstream: intel_lrc.c lrc_ring_mi_mode()
unsafe fn lrc_ring_mi_mode(engine: *const IntelEngineCs) -> i32 {
    if graphics_ver_full((*engine).i915) >= IP_VER(12, 55) {
        0x70
    } else if graphics_ver((*engine).i915) >= 12 {
        0x60
    } else if graphics_ver((*engine).i915) >= 9 {
        0x54
    } else if (*engine).class == RENDER_CLASS {
        0x58
    } else {
        -1
    }
}

// upstream: intel_lrc.c lrc_ring_bb_offset()
unsafe fn lrc_ring_bb_offset(engine: *const IntelEngineCs) -> i32 {
    if graphics_ver_full((*engine).i915) >= IP_VER(12, 55) {
        0x80
    } else if graphics_ver((*engine).i915) >= 12 {
        0x70
    } else if graphics_ver((*engine).i915) >= 9 {
        0x64
    } else if graphics_ver((*engine).i915) >= 8 && (*engine).class == RENDER_CLASS {
        0xc4
    } else {
        -1
    }
}

// upstream: intel_lrc.c lrc_ring_gpr0()
unsafe fn lrc_ring_gpr0(engine: *const IntelEngineCs) -> i32 {
    if graphics_ver_full((*engine).i915) >= IP_VER(12, 55) {
        0x84
    } else if graphics_ver((*engine).i915) >= 12 {
        0x74
    } else if graphics_ver((*engine).i915) >= 9 {
        0x68
    } else if (*engine).class == RENDER_CLASS {
        0xd8
    } else {
        -1
    }
}

// upstream: intel_lrc.c lrc_ring_wa_bb_per_ctx()
unsafe fn lrc_ring_wa_bb_per_ctx(engine: *const IntelEngineCs) -> i32 {
    if graphics_ver((*engine).i915) >= 12 {
        0x12
    } else if graphics_ver((*engine).i915) >= 9 || (*engine).class == RENDER_CLASS {
        0x18
    } else {
        -1
    }
}

// upstream: intel_lrc.c lrc_ring_indirect_ptr()
unsafe fn lrc_ring_indirect_ptr(engine: *const IntelEngineCs) -> i32 {
    let x = lrc_ring_wa_bb_per_ctx(engine);
    if x < 0 {
        return x;
    }
    x + 2
}

// upstream: intel_lrc.c lrc_ring_indirect_offset()
unsafe fn lrc_ring_indirect_offset(engine: *const IntelEngineCs) -> i32 {
    let x = lrc_ring_indirect_ptr(engine);
    if x < 0 {
        return x;
    }
    x + 2
}

// upstream: intel_lrc.c lrc_ring_cmd_buf_cctl()
unsafe fn lrc_ring_cmd_buf_cctl(engine: *const IntelEngineCs) -> i32 {
    if graphics_ver_full((*engine).i915) >= IP_VER(12, 55) {
        // CSFE has a dummy CMD_BUF_CCTL slot to match the RCS image layout.
        0xc6
    } else if (*engine).class != RENDER_CLASS {
        -1
    } else if graphics_ver((*engine).i915) >= 12 {
        0xb6
    } else if graphics_ver((*engine).i915) >= 11 {
        0xaa
    } else {
        -1
    }
}

// upstream: intel_lrc.c lrc_ring_indirect_offset_default()
unsafe fn lrc_ring_indirect_offset_default(engine: *const IntelEngineCs) -> u32 {
    if graphics_ver((*engine).i915) >= 12 {
        GEN12_CTX_RCS_INDIRECT_CTX_OFFSET_DEFAULT
    } else if graphics_ver((*engine).i915) >= 11 {
        GEN11_CTX_RCS_INDIRECT_CTX_OFFSET_DEFAULT
    } else if graphics_ver((*engine).i915) >= 9 {
        GEN9_CTX_RCS_INDIRECT_CTX_OFFSET_DEFAULT
    } else if graphics_ver((*engine).i915) >= 8 {
        GEN8_CTX_RCS_INDIRECT_CTX_OFFSET_DEFAULT
    } else {
        GEM_BUG_ON!(graphics_ver((*engine).i915) < 8);
        0
    }
}

// upstream: intel_lrc.c lrc_setup_bb_per_ctx()
unsafe fn lrc_setup_bb_per_ctx(
    regs: *mut u32,
    engine: *const IntelEngineCs,
    ctx_bb_ggtt_addr: u32,
) {
    let loc = lrc_ring_wa_bb_per_ctx(engine);
    GEM_BUG_ON!(loc == -1);
    *regs.add((loc + 1) as usize) = ctx_bb_ggtt_addr | PER_CTX_BB_FORCE | PER_CTX_BB_VALID;
}

// upstream: intel_lrc.c lrc_setup_indirect_ctx()
unsafe fn lrc_setup_indirect_ctx(
    regs: *mut u32,
    engine: *const IntelEngineCs,
    ctx_bb_ggtt_addr: u32,
    size: u32,
) {
    GEM_BUG_ON!(size == 0);
    GEM_BUG_ON!(!IS_ALIGNED(size, CACHELINE_BYTES));
    let ptr = lrc_ring_indirect_ptr(engine);
    GEM_BUG_ON!(ptr == -1);
    *regs.add((ptr + 1) as usize) = ctx_bb_ggtt_addr | (size / CACHELINE_BYTES);

    let offset = lrc_ring_indirect_offset(engine);
    GEM_BUG_ON!(offset == -1);
    *regs.add((offset + 1) as usize) = lrc_ring_indirect_offset_default(engine) << 6;
}

// upstream: intel_lrc.c ctx_needs_runalone()
unsafe fn ctx_needs_runalone(ce: *const IntelContext) -> bool {
    let mut ctx_is_protected = false;
    // Wa_14019159160 - Case 2. Protected PXP contexts need LRC run-alone mode.
    if graphics_ver_full((*(*ce).engine).i915) >= IP_VER(12, 70)
        && ((*(*ce).engine).class == COMPUTE_CLASS || (*(*ce).engine).class == RENDER_CLASS)
    {
        rcu_read_lock();
        let gem_ctx = rcu_dereference((*ce).gem_context);
        if !gem_ctx.is_null() {
            ctx_is_protected = (*gem_ctx).uses_protected_content;
        }
        rcu_read_unlock();
    }
    ctx_is_protected
}

// upstream: intel_lrc.c init_common_regs()
unsafe fn init_common_regs(
    regs: *mut u32,
    ce: *const IntelContext,
    engine: *const IntelEngineCs,
    inhibit: bool,
) {
    let mut ctl = REG_MASKED_FIELD_ENABLE!(CTX_CTRL_INHIBIT_SYN_CTX_SWITCH);
    ctl |= REG_MASKED_FIELD_DISABLE!(CTX_CTRL_ENGINE_CTX_RESTORE_INHIBIT);
    if inhibit {
        ctl |= CTX_CTRL_ENGINE_CTX_RESTORE_INHIBIT;
    }
    if graphics_ver((*engine).i915) < 11 {
        ctl |= REG_MASKED_FIELD_DISABLE!(CTX_CTRL_ENGINE_CTX_SAVE_INHIBIT | CTX_CTRL_RS_CTX_ENABLE);
    }
    // Wa_14019159160 - Case 2.
    if ctx_needs_runalone(ce) {
        ctl |= REG_MASKED_FIELD_ENABLE!(GEN12_CTX_CTRL_RUNALONE_MODE);
    }
    *regs.add(CTX_CONTEXT_CONTROL) = ctl;

    *regs.add(CTX_TIMESTAMP) = (*ce).stats.runtime.last;
    let loc = lrc_ring_bb_offset(engine);
    if loc != -1 {
        *regs.add((loc + 1) as usize) = 0;
    }
}

// upstream: intel_lrc.c init_wa_bb_regs()
unsafe fn init_wa_bb_regs(regs: *mut u32, engine: *const IntelEngineCs) {
    let wa_ctx = &(*engine).wa_ctx;
    if wa_ctx.per_ctx.size != 0 {
        let ggtt_offset = i915_ggtt_offset(wa_ctx.vma);
        let loc = lrc_ring_wa_bb_per_ctx(engine);
        GEM_BUG_ON!(loc == -1);
        *regs.add((loc + 1) as usize) = (ggtt_offset + wa_ctx.per_ctx.offset) | 0x01;
    }
    if wa_ctx.indirect_ctx.size != 0 {
        lrc_setup_indirect_ctx(
            regs,
            engine,
            i915_ggtt_offset(wa_ctx.vma) + wa_ctx.indirect_ctx.offset,
            wa_ctx.indirect_ctx.size,
        );
    }
}

// upstream: intel_lrc.c init_ppgtt_regs()
unsafe fn init_ppgtt_regs(regs: *mut u32, ppgtt: *const I915Ppgtt) {
    if i915_vm_is_4lvl(&(*ppgtt).vm) {
        // 64-bit PPGTT (48-bit canonical): PDP0_DESCRIPTOR holds PML4 base.
        ASSIGN_CTX_PML4!(ppgtt, regs);
    } else {
        ASSIGN_CTX_PDP!(ppgtt, regs, 3);
        ASSIGN_CTX_PDP!(ppgtt, regs, 2);
        ASSIGN_CTX_PDP!(ppgtt, regs, 1);
        ASSIGN_CTX_PDP!(ppgtt, regs, 0);
    }
}

// upstream: intel_lrc.c vm_alias()
unsafe fn vm_alias(vm: *mut I915AddressSpace) -> *mut I915Ppgtt {
    if i915_is_ggtt(vm) {
        i915_vm_to_ggtt(vm).alias
    } else {
        i915_vm_to_ppgtt(vm)
    }
}

// upstream: intel_lrc.c __reset_stop_ring()
unsafe fn __reset_stop_ring(regs: *mut u32, engine: *const IntelEngineCs) {
    let x = lrc_ring_mi_mode(engine);
    if x != -1 {
        *regs.add((x + 1) as usize) &= !STOP_RING;
        *regs.add((x + 1) as usize) |= STOP_RING << 16;
    }
}

// upstream: intel_lrc.c __lrc_init_regs()
unsafe fn __lrc_init_regs(
    regs: *mut u32,
    ce: *const IntelContext,
    engine: *const IntelEngineCs,
    inhibit: bool,
) {
    // A context is a batch of MI_LOAD_REGISTER_IMM commands and register/value
    // pairs.  Only first-restore values are initialized; GPU saves recreate it.
    // Keep this consistent with virtual_update_register_offsets().
    if inhibit {
        memset(regs.cast::<c_void>(), 0, PAGE_SIZE);
    }
    set_offsets(regs, reg_offsets(engine), engine, inhibit);
    init_common_regs(regs, ce, engine, inhibit);
    init_ppgtt_regs(regs, vm_alias((*ce).vm));
    init_wa_bb_regs(regs, engine);
    __reset_stop_ring(regs, engine);
}

// upstream: intel_lrc.c lrc_init_regs()
pub(crate) unsafe fn lrc_init_regs(
    ce: *const IntelContext,
    engine: *const IntelEngineCs,
    inhibit: bool,
) {
    __lrc_init_regs((*ce).lrc_reg_state, ce, engine, inhibit);
}

// upstream: intel_lrc.c lrc_reset_regs()
pub(crate) unsafe fn lrc_reset_regs(ce: *const IntelContext, engine: *const IntelEngineCs) {
    __reset_stop_ring((*ce).lrc_reg_state, engine);
}

// upstream: intel_lrc.c set_redzone()
unsafe fn set_redzone(mut vaddr: *mut c_void, engine: *const IntelEngineCs) {
    if !IS_ENABLED!(CONFIG_DRM_I915_DEBUG_GEM) {
        return;
    }
    vaddr = vaddr
        .cast::<u8>()
        .add((*engine).context_size as usize)
        .cast::<c_void>();
    memset(vaddr, CONTEXT_REDZONE as i32, I915_GTT_PAGE_SIZE);
}

// upstream: intel_lrc.c check_redzone()
unsafe fn check_redzone(mut vaddr: *const c_void, engine: *const IntelEngineCs) {
    if !IS_ENABLED!(CONFIG_DRM_I915_DEBUG_GEM) {
        return;
    }
    vaddr = vaddr
        .cast::<u8>()
        .add((*engine).context_size as usize)
        .cast::<c_void>();
    if !memchr_inv(vaddr, CONTEXT_REDZONE as i32, I915_GTT_PAGE_SIZE).is_null() {
        drm_err_once!(
            &(*(*engine).i915).drm,
            "%s context redzone overwritten!\n",
            (*engine).name,
        );
    }
}

// upstream: intel_lrc.c context_wa_bb_offset()
unsafe fn context_wa_bb_offset(ce: *const IntelContext) -> u32 {
    PAGE_SIZE as u32 * (*ce).wa_bb_page
}

// upstream: intel_lrc.c context_wabb()
unsafe fn context_wabb(ce: *const IntelContext, per_ctx: bool) -> *mut u32 {
    GEM_BUG_ON!((*ce).wa_bb_page == 0);
    let mut ptr = (*ce).lrc_reg_state.cast::<u8>();
    ptr = ptr.sub(LRC_STATE_OFFSET); // back to start of context image
    ptr = ptr.add(context_wa_bb_offset(ce) as usize);
    ptr = ptr.add(if per_ctx { PAGE_SIZE } else { 0 });
    ptr.cast::<u32>()
}

// upstream: intel_lrc.c lrc_init_state()
pub(crate) unsafe fn lrc_init_state(
    ce: *mut IntelContext,
    engine: *mut IntelEngineCs,
    state: *mut c_void,
) {
    let mut inhibit = true;
    set_redzone(state, engine);

    if !(*ce).default_state.is_null() {
        shmem_read((*ce).default_state, 0, state, (*engine).context_size);
        __set_bit(CONTEXT_VALID_BIT, &mut (*ce).flags);
        inhibit = false;
    }

    // Clear ppHWSP (including per-context counters).
    memset(state, 0, PAGE_SIZE);
    // Clear indirect WA and storage.
    if (*ce).wa_bb_page != 0 {
        memset(
            state
                .cast::<u8>()
                .add(context_wa_bb_offset(ce) as usize)
                .cast::<c_void>(),
            0,
            PAGE_SIZE,
        );
    }
    // The second page holds registers set before the first execution.
    __lrc_init_regs(
        state.cast::<u8>().add(LRC_STATE_OFFSET).cast::<u32>(),
        ce,
        engine,
        inhibit,
    );
}

// upstream: intel_lrc.c lrc_indirect_bb()
unsafe fn lrc_indirect_bb(ce: *const IntelContext) -> u32 {
    i915_ggtt_offset((*ce).state) + context_wa_bb_offset(ce)
}

// upstream: intel_lrc.c setup_predicate_disable_wa()
unsafe fn setup_predicate_disable_wa(ce: *const IntelContext, mut cs: *mut u32) -> *mut u32 {
    // If predication is active, this will be noop'ed.
    *cs = MI_STORE_DWORD_IMM_GEN4 | MI_USE_GGTT | (4 - 2);
    cs = cs.add(1);
    *cs = lrc_indirect_bb(ce) + DG2_PREDICATE_RESULT_WA;
    cs = cs.add(1);
    *cs = 0;
    cs = cs.add(1);
    *cs = 0;
    cs = cs.add(1); // No predication

    // Predicated end only terminates if SET_PREDICATE_RESULT:0 is clear.
    *cs = MI_BATCH_BUFFER_END | BIT!(15);
    cs = cs.add(1);
    *cs = MI_SET_PREDICATE | MI_SET_PREDICATE_DISABLE;
    cs = cs.add(1);

    // Instructions are no longer predicated (disabled), so proceed.
    *cs = MI_STORE_DWORD_IMM_GEN4 | MI_USE_GGTT | (4 - 2);
    cs = cs.add(1);
    *cs = lrc_indirect_bb(ce) + DG2_PREDICATE_RESULT_WA;
    cs = cs.add(1);
    *cs = 0;
    cs = cs.add(1);
    *cs = 1;
    cs = cs.add(1); // enable predication before the next BB

    *cs = MI_BATCH_BUFFER_END;
    cs = cs.add(1);
    GEM_BUG_ON!(offset_in_page(cs) > DG2_PREDICATE_RESULT_WA);
    cs
}

// upstream: intel_lrc.c __lrc_alloc_state()
unsafe fn __lrc_alloc_state(ce: *mut IntelContext, engine: *mut IntelEngineCs) -> *mut I915Vma {
    let mut context_size = round_up((*engine).context_size, I915_GTT_PAGE_SIZE);
    if IS_ENABLED!(CONFIG_DRM_I915_DEBUG_GEM) {
        context_size += I915_GTT_PAGE_SIZE; // for redzone
    }
    if graphics_ver((*engine).i915) >= 12 {
        (*ce).wa_bb_page = context_size / PAGE_SIZE;
        // INDIRECT_CTX and PER_CTX_BB need separate pages.
        context_size += PAGE_SIZE * 2;
    }
    if intel_context_is_parent(ce) && intel_engine_uses_guc(engine) {
        (*ce).parallel.guc.parent_page = context_size / PAGE_SIZE;
        context_size += PARENT_SCRATCH_SIZE;
    }

    let mut obj =
        i915_gem_object_create_lmem((*engine).i915, context_size, I915_BO_ALLOC_PM_VOLATILE);
    if IS_ERR(obj) {
        obj = i915_gem_object_create_shmem((*engine).i915, context_size);
        if IS_ERR(obj) {
            return ERR_CAST(obj);
        }

        // Wa_22016122933: Media 13.0 shared memory is WC on CPU and UC/PAT2 on GPU.
        if intel_gt_needs_wa_22016122933((*engine).gt) {
            i915_gem_object_set_cache_coherency(obj, I915_CACHE_NONE);
        }
    }

    let vma = i915_vma_instance(obj, &(*(*(*engine).gt).ggtt).vm, core::ptr::null_mut());
    if IS_ERR(vma) {
        i915_gem_object_put(obj);
        return vma;
    }
    vma
}

// upstream: intel_lrc.c pinned_timeline()
unsafe fn pinned_timeline(ce: *mut IntelContext, engine: *mut IntelEngineCs) -> *mut IntelTimeline {
    let tl = fetch_and_zero(&mut (*ce).timeline);
    intel_timeline_create_from_engine(engine, page_unmask_bits(tl))
}

// upstream: intel_lrc.c lrc_alloc()
pub(crate) unsafe fn lrc_alloc(ce: *mut IntelContext, engine: *mut IntelEngineCs) -> i32 {
    GEM_BUG_ON!(!(*ce).state.is_null());
    if !intel_context_has_own_state(ce) {
        (*ce).default_state = (*engine).default_state;
    }

    let vma = __lrc_alloc_state(ce, engine);
    if IS_ERR(vma) {
        return PTR_ERR(vma);
    }

    let ring = intel_engine_create_ring(engine, (*ce).ring_size);
    if IS_ERR(ring) {
        let err = PTR_ERR(ring);
        i915_vma_put(vma);
        return err;
    }

    if !page_mask_bits((*ce).timeline).is_null() {
        // Existing pinned timeline is retained.
    } else {
        let tl = if unlikely(!(*ce).timeline.is_null()) {
            // Static global HWSP for kernel context; dynamic cacheline otherwise.
            pinned_timeline(ce, engine)
        } else {
            intel_timeline_create((*(*engine).gt))
        };
        if IS_ERR(tl) {
            let err = PTR_ERR(tl);
            intel_ring_put(ring);
            i915_vma_put(vma);
            return err;
        }
        (*ce).timeline = tl;
    }

    (*ce).ring = ring;
    (*ce).state = vma;
    0
}

// upstream: intel_lrc.c lrc_reset()
pub(crate) unsafe fn lrc_reset(ce: *mut IntelContext) {
    GEM_BUG_ON!(!intel_context_is_pinned(ce));
    intel_ring_reset((*ce).ring, (*(*ce).ring).emit);
    // Scrub away the garbage.
    lrc_init_regs(ce, (*ce).engine, true);
    (*ce).lrc.lrca = lrc_update_regs(ce, (*ce).engine, (*(*ce).ring).tail);
}

// upstream: intel_lrc.c lrc_pre_pin()
pub(crate) unsafe fn lrc_pre_pin(
    ce: *mut IntelContext,
    _engine: *mut IntelEngineCs,
    _ww: *mut I915GemWwCtx,
    vaddr: *mut *mut c_void,
) -> i32 {
    GEM_BUG_ON!((*ce).state.is_null());
    GEM_BUG_ON!(!i915_vma_is_pinned((*ce).state));
    *vaddr = i915_gem_object_pin_map(
        (*(*ce).state).obj,
        intel_gt_coherent_map_type((*(*ce).engine).gt, (*(*ce).state).obj, false)
            | I915_MAP_OVERRIDE,
    );
    PTR_ERR_OR_ZERO(*vaddr)
}

// upstream: intel_lrc.c lrc_pin()
pub(crate) unsafe fn lrc_pin(
    ce: *mut IntelContext,
    engine: *mut IntelEngineCs,
    vaddr: *mut c_void,
) -> i32 {
    (*ce).lrc_reg_state = vaddr.cast::<u8>().add(LRC_STATE_OFFSET).cast::<u32>();
    if !__test_and_set_bit(CONTEXT_INIT_BIT, &mut (*ce).flags) {
        lrc_init_state(ce, engine, vaddr);
    }
    (*ce).lrc.lrca = lrc_update_regs(ce, engine, (*(*ce).ring).tail);
    0
}

// upstream: intel_lrc.c lrc_unpin()
pub(crate) unsafe fn lrc_unpin(ce: *mut IntelContext) {
    if unlikely(!(*ce).parallel.last_rq.is_null()) {
        i915_request_put((*ce).parallel.last_rq);
        (*ce).parallel.last_rq = core::ptr::null_mut();
    }
    check_redzone(
        (*ce)
            .lrc_reg_state
            .cast::<u8>()
            .sub(LRC_STATE_OFFSET)
            .cast::<c_void>(),
        (*ce).engine,
    );
}

// upstream: intel_lrc.c lrc_post_unpin()
pub(crate) unsafe fn lrc_post_unpin(ce: *mut IntelContext) {
    i915_gem_object_unpin_map((*(*ce).state).obj);
}

// upstream: intel_lrc.c lrc_fini()
pub(crate) unsafe fn lrc_fini(ce: *mut IntelContext) {
    if (*ce).state.is_null() {
        return;
    }
    intel_ring_put(fetch_and_zero(&mut (*ce).ring));
    i915_vma_put(fetch_and_zero(&mut (*ce).state));
}

// upstream: intel_lrc.c lrc_destroy()
unsafe fn lrc_destroy(kref: *mut Kref) {
    let ce = container_of!(kref, IntelContext, ref_);
    GEM_BUG_ON!(!i915_active_is_idle(&mut (*ce).active));
    GEM_BUG_ON!(intel_context_is_pinned(ce));
    lrc_fini(ce);
    intel_context_fini(ce);
    intel_context_free(ce);
}

// upstream: intel_lrc.c gen12_emit_timestamp_wa()
unsafe fn gen12_emit_timestamp_wa(ce: *const IntelContext, mut cs: *mut u32) -> *mut u32 {
    *cs = MI_LOAD_REGISTER_MEM_GEN8 | MI_SRM_LRM_GLOBAL_GTT | MI_LRI_LRM_CS_MMIO;
    cs = cs.add(1);
    *cs = i915_mmio_reg_offset(GEN8_RING_CS_GPR(0, 0));
    cs = cs.add(1);
    *cs = i915_ggtt_offset((*ce).state)
        + LRC_STATE_OFFSET as u32
        + CTX_TIMESTAMP as u32 * size_of::<u32>() as u32;
    cs = cs.add(1);
    *cs = 0;
    cs = cs.add(1);
    *cs = MI_LOAD_REGISTER_REG | MI_LRR_SOURCE_CS_MMIO | MI_LRI_LRM_CS_MMIO;
    cs = cs.add(1);
    *cs = i915_mmio_reg_offset(GEN8_RING_CS_GPR(0, 0));
    cs = cs.add(1);
    *cs = i915_mmio_reg_offset(RING_CTX_TIMESTAMP(0));
    cs = cs.add(1);
    *cs = MI_LOAD_REGISTER_REG | MI_LRR_SOURCE_CS_MMIO | MI_LRI_LRM_CS_MMIO;
    cs = cs.add(1);
    *cs = i915_mmio_reg_offset(GEN8_RING_CS_GPR(0, 0));
    cs = cs.add(1);
    *cs = i915_mmio_reg_offset(RING_CTX_TIMESTAMP(0));
    cs = cs.add(1);
    cs
}

// upstream: intel_lrc.c gen12_emit_restore_scratch()
unsafe fn gen12_emit_restore_scratch(ce: *const IntelContext, mut cs: *mut u32) -> *mut u32 {
    GEM_BUG_ON!(lrc_ring_gpr0((*ce).engine) == -1);
    *cs = MI_LOAD_REGISTER_MEM_GEN8 | MI_SRM_LRM_GLOBAL_GTT | MI_LRI_LRM_CS_MMIO;
    cs = cs.add(1);
    *cs = i915_mmio_reg_offset(GEN8_RING_CS_GPR(0, 0));
    cs = cs.add(1);
    *cs = i915_ggtt_offset((*ce).state)
        + LRC_STATE_OFFSET as u32
        + (lrc_ring_gpr0((*ce).engine) as u32 + 1) * size_of::<u32>() as u32;
    cs = cs.add(1);
    *cs = 0;
    cs = cs.add(1);
    cs
}

// upstream: intel_lrc.c gen12_emit_cmd_buf_wa()
unsafe fn gen12_emit_cmd_buf_wa(ce: *const IntelContext, mut cs: *mut u32) -> *mut u32 {
    GEM_BUG_ON!(lrc_ring_cmd_buf_cctl((*ce).engine) == -1);
    *cs = MI_LOAD_REGISTER_MEM_GEN8 | MI_SRM_LRM_GLOBAL_GTT | MI_LRI_LRM_CS_MMIO;
    cs = cs.add(1);
    *cs = i915_mmio_reg_offset(GEN8_RING_CS_GPR(0, 0));
    cs = cs.add(1);
    *cs = i915_ggtt_offset((*ce).state)
        + LRC_STATE_OFFSET as u32
        + (lrc_ring_cmd_buf_cctl((*ce).engine) as u32 + 1) * size_of::<u32>() as u32;
    cs = cs.add(1);
    *cs = 0;
    cs = cs.add(1);
    *cs = MI_LOAD_REGISTER_REG | MI_LRR_SOURCE_CS_MMIO | MI_LRI_LRM_CS_MMIO;
    cs = cs.add(1);
    *cs = i915_mmio_reg_offset(GEN8_RING_CS_GPR(0, 0));
    cs = cs.add(1);
    *cs = i915_mmio_reg_offset(RING_CMD_BUF_CCTL(0));
    cs = cs.add(1);
    cs
}

// upstream: intel_lrc.c dg2_emit_draw_watermark_setting()
unsafe fn dg2_emit_draw_watermark_setting(mut cs: *mut u32) -> *mut u32 {
    *cs = MI_LOAD_REGISTER_IMM(1);
    cs = cs.add(1);
    *cs = i915_mmio_reg_offset(DRAW_WATERMARK);
    cs = cs.add(1);
    *cs = REG_FIELD_PREP(VERT_WM_VAL, 0x3ff);
    cs = cs.add(1);
    cs
}

// upstream: intel_lrc.c gen12_invalidate_state_cache()
unsafe fn gen12_invalidate_state_cache(mut cs: *mut u32) -> *mut u32 {
    *cs = MI_LOAD_REGISTER_IMM(1);
    cs = cs.add(1);
    *cs = i915_mmio_reg_offset(GEN12_CS_DEBUG_MODE2);
    cs = cs.add(1);
    *cs = REG_MASKED_FIELD_ENABLE!(INSTRUCTION_STATE_CACHE_INVALIDATE);
    cs = cs.add(1);
    cs
}

// upstream: intel_lrc.c gen12_emit_indirect_ctx_rcs()
unsafe fn gen12_emit_indirect_ctx_rcs(ce: *const IntelContext, mut cs: *mut u32) -> *mut u32 {
    cs = gen12_emit_timestamp_wa(ce, cs);
    cs = gen12_emit_cmd_buf_wa(ce, cs);
    cs = gen12_emit_restore_scratch(ce, cs);
    // Wa_16013000631:dg2
    if IS_DG2_G11((*(*ce).engine).i915) {
        cs = gen8_emit_pipe_control(cs, PIPE_CONTROL_INSTRUCTION_CACHE_INVALIDATE, 0);
    }
    cs = gen12_emit_aux_table_inv((*ce).engine, cs);
    // Wa_18022495364
    if IS_GFX_GT_IP_RANGE((*(*ce).engine).gt, IP_VER(12, 0), IP_VER(12, 10)) {
        cs = gen12_invalidate_state_cache(cs);
    }
    // Wa_16014892111
    if IS_GFX_GT_IP_STEP((*(*ce).engine).gt, IP_VER(12, 70), STEP_A0, STEP_B0)
        || IS_GFX_GT_IP_STEP((*(*ce).engine).gt, IP_VER(12, 71), STEP_A0, STEP_B0)
        || IS_DG2((*(*ce).engine).i915)
    {
        cs = dg2_emit_draw_watermark_setting(cs);
    }
    cs
}

// upstream: intel_lrc.c gen12_emit_indirect_ctx_xcs()
unsafe fn gen12_emit_indirect_ctx_xcs(ce: *const IntelContext, mut cs: *mut u32) -> *mut u32 {
    cs = gen12_emit_timestamp_wa(ce, cs);
    cs = gen12_emit_restore_scratch(ce, cs);
    // Wa_16013000631:dg2
    if IS_DG2_G11((*(*ce).engine).i915) && (*(*ce).engine).class == COMPUTE_CLASS {
        cs = gen8_emit_pipe_control(cs, PIPE_CONTROL_INSTRUCTION_CACHE_INVALIDATE, 0);
    }
    gen12_emit_aux_table_inv((*ce).engine, cs)
}

// upstream: intel_lrc.c xehp_emit_fastcolor_blt_wabb()
unsafe fn xehp_emit_fastcolor_blt_wabb(ce: *const IntelContext, mut cs: *mut u32) -> *mut u32 {
    let gt = (*(*ce).engine).gt;
    let mocs = (*gt).mocs.uc_index << 1;
    // Wa_16018031267 / Wa_16018063123: emit four zero-byte-write fast-color subblits.
    *cs = XY_FAST_COLOR_BLT_CMD | (16 - 2);
    cs = cs.add(1);
    *cs = FIELD_PREP(XY_FAST_COLOR_BLT_MOCS_MASK, mocs) | 0x3f;
    cs = cs.add(1);
    *cs = 0;
    cs = cs.add(1);
    *cs = (4 << 16) | 1;
    cs = cs.add(1);
    *cs = lower_32_bits(i915_vma_offset((*(*ce).vm).rsvd.vma));
    cs = cs.add(1);
    *cs = upper_32_bits(i915_vma_offset((*(*ce).vm).rsvd.vma));
    cs = cs.add(1);
    *cs = 0;
    cs = cs.add(1);
    *cs = 0;
    cs = cs.add(1);
    *cs = 0;
    cs = cs.add(1);
    *cs = 0;
    cs = cs.add(1);
    *cs = 0;
    cs = cs.add(1);
    *cs = 0;
    cs = cs.add(1);
    *cs = 0x20004004;
    cs = cs.add(1);
    *cs = 0x10;
    cs = cs.add(1);
    *cs = 0;
    cs = cs.add(1);
    cs
}

// upstream: intel_lrc.c xehp_emit_per_ctx_bb()
unsafe fn xehp_emit_per_ctx_bb(ce: *const IntelContext, mut cs: *mut u32) -> *mut u32 {
    // Wa_16018031267, Wa_16018063123.
    if NEEDS_FASTCOLOR_BLT_WABB((*(*ce).engine)) {
        cs = xehp_emit_fastcolor_blt_wabb(ce, cs);
    }
    cs
}

// upstream: intel_lrc.c setup_per_ctx_bb()
unsafe fn setup_per_ctx_bb(
    ce: *const IntelContext,
    engine: *const IntelEngineCs,
    emit: unsafe fn(*const IntelContext, *mut u32) -> *mut u32,
) {
    // Place PER_CTX_BB on the next page after INDIRECT_CTX.
    let start = context_wabb(ce, true);
    let mut cs = emit(ce, start);
    // PER_CTX_BB must manually terminate.
    *cs = MI_BATCH_BUFFER_END;
    cs = cs.add(1);
    GEM_BUG_ON!(cs.offset_from(start) as usize > I915_GTT_PAGE_SIZE / size_of::<u32>());
    lrc_setup_bb_per_ctx(
        (*ce).lrc_reg_state,
        engine,
        lrc_indirect_bb(ce) + PAGE_SIZE as u32,
    );
}

// upstream: intel_lrc.c setup_indirect_ctx_bb()
unsafe fn setup_indirect_ctx_bb(
    ce: *const IntelContext,
    engine: *const IntelEngineCs,
    emit: unsafe fn(*const IntelContext, *mut u32) -> *mut u32,
) {
    let start = context_wabb(ce, false);
    let mut cs = emit(ce, start);
    GEM_BUG_ON!(cs.offset_from(start) as usize > I915_GTT_PAGE_SIZE / size_of::<u32>());
    while (cs as usize) % CACHELINE_BYTES != 0 {
        *cs = MI_NOOP;
        cs = cs.add(1);
    }
    GEM_BUG_ON!(cs.offset_from(start) as usize > DG2_PREDICATE_RESULT_BB / size_of::<u32>());
    setup_predicate_disable_wa(ce, start.add(DG2_PREDICATE_RESULT_BB / size_of::<u32>()));
    lrc_setup_indirect_ctx(
        (*ce).lrc_reg_state,
        engine,
        lrc_indirect_bb(ce),
        cs.offset_from(start) as u32 * size_of::<u32>() as u32,
    );
}

// upstream: intel_lrc.c lrc_descriptor()
unsafe fn lrc_descriptor(ce: *const IntelContext) -> u32 {
    let mut desc = INTEL_LEGACY_32B_CONTEXT;
    if i915_vm_is_4lvl((*ce).vm) {
        desc = INTEL_LEGACY_64B_CONTEXT;
    }
    desc <<= GEN8_CTX_ADDRESSING_MODE_SHIFT;
    desc |= GEN8_CTX_VALID | GEN8_CTX_PRIVILEGE;
    if graphics_ver((*(*ce).vm).i915) == 8 {
        desc |= GEN8_CTX_L3LLC_COHERENT;
    }
    i915_ggtt_offset((*ce).state) | desc
}

// upstream: intel_lrc.c lrc_update_regs()
pub(crate) unsafe fn lrc_update_regs(
    ce: *const IntelContext,
    engine: *const IntelEngineCs,
    head: u32,
) -> u32 {
    let ring = (*ce).ring;
    let regs = (*ce).lrc_reg_state;
    GEM_BUG_ON!(!intel_ring_offset_valid(ring, head));
    GEM_BUG_ON!(!intel_ring_offset_valid(ring, (*ring).tail));

    *regs.add(CTX_RING_START) = i915_ggtt_offset((*ring).vma);
    *regs.add(CTX_RING_HEAD) = head;
    *regs.add(CTX_RING_TAIL) = (*ring).tail;
    *regs.add(CTX_RING_CTL) = RING_CTL_SIZE((*ring).size) | RING_VALID;

    // RPCS.
    if (*engine).class == RENDER_CLASS {
        *regs.add(CTX_R_PWR_CLK_STATE) = intel_sseu_make_rpcs((*engine).gt, &(*ce).sseu);
        i915_oa_init_reg_state(ce, engine);
    }

    if (*ce).wa_bb_page != 0 {
        let mut emit =
            gen12_emit_indirect_ctx_xcs as unsafe fn(*const IntelContext, *mut u32) -> *mut u32;
        if (*(*ce).engine).class == RENDER_CLASS {
            emit = gen12_emit_indirect_ctx_rcs;
        }
        // Mutually exclusive with global indirect BB.
        GEM_BUG_ON!((*engine).wa_ctx.indirect_ctx.size != 0);
        setup_indirect_ctx_bb(ce, engine, emit);
        setup_per_ctx_bb(ce, engine, xehp_emit_per_ctx_bb);
    }
    lrc_descriptor(ce) | CTX_DESC_FORCE_RESTORE
}

// upstream: intel_lrc.c lrc_update_offsets()
pub(crate) unsafe fn lrc_update_offsets(ce: *mut IntelContext, engine: *const IntelEngineCs) {
    set_offsets((*ce).lrc_reg_state, reg_offsets(engine), engine, false);
}

// upstream: intel_lrc.c lrc_check_regs()
pub(crate) unsafe fn lrc_check_regs(
    ce: *const IntelContext,
    engine: *const IntelEngineCs,
    when: *const i8,
) {
    let ring = (*ce).ring;
    let regs = (*ce).lrc_reg_state;
    let mut valid = true;
    let mut x: i32;

    if *regs.add(CTX_RING_START) != i915_ggtt_offset((*ring).vma) {
        pr_err!(
            "%s: context submitted with incorrect RING_START [%08x], expected %08x\n",
            (*engine).name,
            *regs.add(CTX_RING_START),
            i915_ggtt_offset((*ring).vma),
        );
        *regs.add(CTX_RING_START) = i915_ggtt_offset((*ring).vma);
        valid = false;
    }

    if (*regs.add(CTX_RING_CTL) & !(RING_WAIT | RING_WAIT_SEMAPHORE))
        != (RING_CTL_SIZE((*ring).size) | RING_VALID)
    {
        pr_err!(
            "%s: context submitted with incorrect RING_CTL [%08x], expected %08x\n",
            (*engine).name,
            *regs.add(CTX_RING_CTL),
            (RING_CTL_SIZE((*ring).size) | RING_VALID) as u32,
        );
        *regs.add(CTX_RING_CTL) = RING_CTL_SIZE((*ring).size) | RING_VALID;
        valid = false;
    }

    x = lrc_ring_mi_mode(engine);
    if x != -1 && *regs.add((x + 1) as usize) & (*regs.add((x + 1) as usize) >> 16) & STOP_RING != 0
    {
        pr_err!(
            "%s: context submitted with STOP_RING [%08x] in RING_MI_MODE\n",
            (*engine).name,
            *regs.add((x + 1) as usize),
        );
        *regs.add((x + 1) as usize) &= !STOP_RING;
        *regs.add((x + 1) as usize) |= STOP_RING << 16;
        valid = false;
    }
    WARN_ONCE!(!valid, "Invalid lrc state found {} submission", when);
}

// upstream: intel_lrc.c gen8_emit_flush_coherentl3_wa()
unsafe fn gen8_emit_flush_coherentl3_wa(
    engine: *mut IntelEngineCs,
    mut batch: *mut u32,
) -> *mut u32 {
    // NB no one else is allowed to scribble over scratch + 256!
    *batch = MI_STORE_REGISTER_MEM_GEN8 | MI_SRM_LRM_GLOBAL_GTT;
    batch = batch.add(1);
    *batch = i915_mmio_reg_offset(GEN8_L3SQCREG4);
    batch = batch.add(1);
    *batch = intel_gt_scratch_offset((*engine).gt, INTEL_GT_SCRATCH_FIELD_COHERENTL3_WA);
    batch = batch.add(1);
    *batch = 0;
    batch = batch.add(1);

    *batch = MI_LOAD_REGISTER_IMM(1);
    batch = batch.add(1);
    *batch = i915_mmio_reg_offset(GEN8_L3SQCREG4);
    batch = batch.add(1);
    *batch = 0x40400000 | GEN8_LQSC_FLUSH_COHERENT_LINES;
    batch = batch.add(1);
    batch = gen8_emit_pipe_control(
        batch,
        PIPE_CONTROL_CS_STALL | PIPE_CONTROL_DC_FLUSH_ENABLE,
        0,
    );

    *batch = MI_LOAD_REGISTER_MEM_GEN8 | MI_SRM_LRM_GLOBAL_GTT;
    batch = batch.add(1);
    *batch = i915_mmio_reg_offset(GEN8_L3SQCREG4);
    batch = batch.add(1);
    *batch = intel_gt_scratch_offset((*engine).gt, INTEL_GT_SCRATCH_FIELD_COHERENTL3_WA);
    batch = batch.add(1);
    *batch = 0;
    batch = batch.add(1);
    batch
}

// upstream: intel_lrc.c gen8_init_indirectctx_bb()
unsafe fn gen8_init_indirectctx_bb(engine: *mut IntelEngineCs, mut batch: *mut u32) -> *mut u32 {
    // WaDisableCtxRestoreArbitration:bdw,chv.
    *batch = MI_ARB_ON_OFF | MI_ARB_DISABLE;
    batch = batch.add(1);
    // WaFlushCoherentL3CacheLinesAtContextSwitch:bdw.
    if IS_BROADWELL((*engine).i915) {
        batch = gen8_emit_flush_coherentl3_wa(engine, batch);
    }
    // WaClearSlmSpaceAtContextSwitch:bdw,chv; scratch is at 128 bytes offset.
    batch = gen8_emit_pipe_control(
        batch,
        PIPE_CONTROL_FLUSH_L3
            | PIPE_CONTROL_STORE_DATA_INDEX
            | PIPE_CONTROL_CS_STALL
            | PIPE_CONTROL_QW_WRITE,
        LRC_PPHWSP_SCRATCH_ADDR,
    );
    *batch = MI_ARB_ON_OFF | MI_ARB_ENABLE;
    batch = batch.add(1);
    // Pad to end of cacheline.
    while (batch as usize % CACHELINE_BYTES != 0) {
        *batch = MI_NOOP;
        batch = batch.add(1);
    }
    // No MI_BATCH_BUFFER_END: execution is length-limited in CTX_RCS_INDIRECT_CTX.
    batch
}

struct Lri {
    reg: I915Reg,
    value: u32,
}

// upstream: intel_lrc.c emit_lri()
unsafe fn emit_lri(mut batch: *mut u32, mut lri: *const Lri, mut count: u32) -> *mut u32 {
    GEM_BUG_ON!(count == 0 || count > 63);
    *batch = MI_LOAD_REGISTER_IMM(count);
    batch = batch.add(1);
    loop {
        *batch = i915_mmio_reg_offset((*lri).reg);
        batch = batch.add(1);
        *batch = (*lri).value;
        batch = batch.add(1);
        lri = lri.add(1);
        count -= 1;
        if count == 0 {
            break;
        }
    }
    *batch = MI_NOOP;
    batch = batch.add(1);
    batch
}

// upstream: intel_lrc.c gen9_init_indirectctx_bb()
unsafe fn gen9_init_indirectctx_bb(engine: *mut IntelEngineCs, mut batch: *mut u32) -> *mut u32 {
    static LRI_ENTRIES: &[Lri] = &[
        // WaDisableGatherAtSetShaderCommonSlice:skl,bxt,kbl,glk.
        Lri {
            reg: COMMON_SLICE_CHICKEN2,
            value: REG_MASKED_FIELD_DISABLE!(GEN9_DISABLE_GATHER_AT_SET_SHADER_COMMON_SLICE),
        },
        // BSpec: 11391.
        Lri {
            reg: FF_SLICE_CHICKEN,
            value: REG_MASKED_FIELD_ENABLE!(FF_SLICE_CHICKEN_CL_PROVOKING_VERTEX_FIX),
        },
        // BSpec: 11299.
        Lri {
            reg: _3D_CHICKEN3,
            value: REG_MASKED_FIELD_ENABLE!(_3D_CHICKEN_SF_PROVOKING_VERTEX_FIX),
        },
    ];

    *batch = MI_ARB_ON_OFF | MI_ARB_DISABLE;
    batch = batch.add(1);
    // WaFlushCoherentL3CacheLinesAtContextSwitch:skl,bxt,glk.
    batch = gen8_emit_flush_coherentl3_wa(engine, batch);
    // WaClearSlmSpaceAtContextSwitch:skl,bxt,kbl,glk,cfl.
    batch = gen8_emit_pipe_control(
        batch,
        PIPE_CONTROL_FLUSH_L3
            | PIPE_CONTROL_STORE_DATA_INDEX
            | PIPE_CONTROL_CS_STALL
            | PIPE_CONTROL_QW_WRITE,
        LRC_PPHWSP_SCRATCH_ADDR,
    );
    batch = emit_lri(batch, LRI_ENTRIES.as_ptr(), LRI_ENTRIES.len() as u32);

    // WaMediaPoolStateCmdInWABB:bxt,glk.
    if HAS_POOLED_EU((*engine).i915) {
        // EU pool configuration is established with golden context. Hardware
        // ignores disabled-subslice bits and uses the appropriate configuration.
        *batch = GEN9_MEDIA_POOL_STATE;
        batch = batch.add(1);
        *batch = GEN9_MEDIA_POOL_ENABLE;
        batch = batch.add(1);
        *batch = 0x00777000;
        batch = batch.add(1);
        *batch = 0;
        batch = batch.add(1);
        *batch = 0;
        batch = batch.add(1);
        *batch = 0;
        batch = batch.add(1);
    }
    *batch = MI_ARB_ON_OFF | MI_ARB_ENABLE;
    batch = batch.add(1);
    // Pad to end of cacheline.
    while batch as usize % CACHELINE_BYTES != 0 {
        *batch = MI_NOOP;
        batch = batch.add(1);
    }
    batch
}

// upstream: intel_lrc.c lrc_create_wa_ctx()
unsafe fn lrc_create_wa_ctx(engine: *mut IntelEngineCs) -> i32 {
    let obj = i915_gem_object_create_shmem((*engine).i915, CTX_WA_BB_SIZE);
    if IS_ERR(obj) {
        return PTR_ERR(obj);
    }
    let vma = i915_vma_instance(obj, &(*(*(*engine).gt).ggtt).vm, core::ptr::null_mut());
    if IS_ERR(vma) {
        let err = PTR_ERR(vma);
        i915_gem_object_put(obj);
        return err;
    }
    (*engine).wa_ctx.vma = vma;
    0
}

// upstream: intel_lrc.c lrc_fini_wa_ctx()
pub(crate) unsafe fn lrc_fini_wa_ctx(engine: *mut IntelEngineCs) {
    i915_vma_unpin_and_release(&mut (*engine).wa_ctx.vma, 0);
}

type WaBbFunc = unsafe fn(*mut IntelEngineCs, *mut u32) -> *mut u32;

// upstream: intel_lrc.c lrc_init_wa_ctx()
pub(crate) unsafe fn lrc_init_wa_ctx(engine: *mut IntelEngineCs) {
    let wa_ctx = &mut (*engine).wa_ctx;
    let wa_bb = [
        &mut wa_ctx.indirect_ctx as *mut I915WaCtxBb,
        &mut wa_ctx.per_ctx as *mut I915WaCtxBb,
    ];
    let mut wa_bb_fn: [Option<WaBbFunc>; 2] = [None, None];
    let mut ww = I915GemWwCtx::zeroed();
    let mut batch: *mut c_void;
    let mut batch_ptr: *mut u8;
    let mut i: usize;
    let mut err: i32;

    if graphics_ver((*engine).i915) >= 11 || (*engine).flags & I915_ENGINE_HAS_RCS_REG_STATE == 0 {
        return;
    }

    if graphics_ver((*engine).i915) == 9 {
        wa_bb_fn[0] = Some(gen9_init_indirectctx_bb);
        wa_bb_fn[1] = None;
    } else if graphics_ver((*engine).i915) == 8 {
        wa_bb_fn[0] = Some(gen8_init_indirectctx_bb);
        wa_bb_fn[1] = None;
    }

    err = lrc_create_wa_ctx(engine);
    if err != 0 {
        // Continue despite rare WA batch allocation failures; GPU use is not prevented.
        drm_err!(
            &(*(*engine).i915).drm,
            "Ignoring context switch w/a allocation error:%d\n",
            err,
        );
        return;
    }
    if (*engine).wa_ctx.vma.is_null() {
        return;
    }

    i915_gem_ww_ctx_init(&mut ww, true);
    'retry: loop {
        err = i915_gem_object_lock((*(*wa_ctx).vma).obj, &mut ww);
        if err == 0 {
            err = i915_ggtt_pin((*wa_ctx).vma, &mut ww, 0, PIN_HIGH);
        }
        if err == 0 {
            batch = i915_gem_object_pin_map((*(*wa_ctx).vma).obj, I915_MAP_WB);
            if IS_ERR(batch) {
                err = PTR_ERR(batch);
                i915_vma_unpin((*wa_ctx).vma);
            } else {
                // Emit the two WA batches, recording their offsets and sizes.
                batch_ptr = batch.cast::<u8>();
                for i in 0..wa_bb.len() {
                    let bb = &mut *wa_bb[i];
                    bb.offset = batch_ptr.offset_from(batch.cast::<u8>()) as u32;
                    if GEM_DEBUG_WARN_ON!(!IS_ALIGNED(bb.offset, CACHELINE_BYTES)) {
                        err = -EINVAL;
                        break;
                    }
                    if let Some(emit) = wa_bb_fn[i] {
                        batch_ptr = emit(engine, batch_ptr.cast::<u32>()).cast::<u8>();
                    }
                    bb.size = batch_ptr.offset_from(batch.cast::<u8>()) as u32 - bb.offset;
                }
                GEM_BUG_ON!(batch_ptr.offset_from(batch.cast::<u8>()) as usize > CTX_WA_BB_SIZE);
                __i915_gem_object_flush_map(
                    (*(*wa_ctx).vma).obj,
                    0,
                    batch_ptr.offset_from(batch.cast::<u8>()) as usize,
                );
                __i915_gem_object_release_map((*(*wa_ctx).vma).obj);
            }
        }

        if err == -EDEADLK {
            err = i915_gem_ww_ctx_backoff(&mut ww);
            if err == 0 {
                continue 'retry;
            }
        }
        i915_gem_ww_ctx_fini(&mut ww);

        if err != 0 {
            i915_vma_put((*engine).wa_ctx.vma);
            // Clear all flags to prevent further use.
            memset(
                wa_ctx as *mut I915CtxWorkarounds as *mut c_void,
                0,
                size_of::<I915CtxWorkarounds>(),
            );
        }
        break;
    }
}

// upstream: intel_lrc.c st_runtime_underflow()
unsafe fn st_runtime_underflow(stats: *mut IntelContextStats, dt: i32) {
    if IS_ENABLED!(CONFIG_DRM_I915_SELFTEST) {
        (*stats).runtime.num_underflow += 1;
        (*stats).runtime.max_underflow =
            core::cmp::max((*stats).runtime.max_underflow, (-dt) as u32);
    }
}

// upstream: intel_lrc.c lrc_get_runtime()
unsafe fn lrc_get_runtime(ce: *const IntelContext) -> u32 {
    // ppHWSP[16] excludes context-switch cost; context image includes save cost.
    READ_ONCE!(*(*ce).lrc_reg_state.add(CTX_TIMESTAMP))
}

// upstream: intel_lrc.c lrc_update_runtime()
pub(crate) unsafe fn lrc_update_runtime(ce: *mut IntelContext) {
    let stats = &mut (*ce).stats;
    let old = stats.runtime.last;
    stats.runtime.last = lrc_get_runtime(ce);
    let dt = stats.runtime.last.wrapping_sub(old) as i32;
    if dt == 0 {
        return;
    }
    if unlikely(dt < 0) {
        CE_TRACE!(
            ce,
            "runtime underflow: last=%u, new=%u, delta=%d\n",
            old,
            stats.runtime.last,
            dt
        );
        st_runtime_underflow(stats, dt);
        return;
    }
    ewma_runtime_add(&mut stats.runtime.avg, dt as u32);
    stats.runtime.total += dt as u64;
}
