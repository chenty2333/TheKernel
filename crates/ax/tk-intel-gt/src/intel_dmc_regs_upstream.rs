// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 display register definitions from `drivers/gpu/drm/i915/display/intel_dmc_regs.h`.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]
// Macro bodies keep C's parenthesization; the redundant parens are expected.
#![allow(unused_parens, unused_braces)]

use crate::intel_workarounds_types_upstream::I915RegT;
use crate::intel_display_reg_defs_upstream::*;
use crate::intel_display_limits_types_upstream::*;
use crate::intel_display_types_upstream::*;

/// `DMC_PROGRAM(addr, i)` (intel_dmc_regs.h)
pub const fn DMC_PROGRAM(addr: u32, i: u32) -> I915RegT {
    mmio(((addr) + ((i) * 4)))
}

/// `DMC_SSP_BASE_ADDR_GEN9` (intel_dmc_regs.h)
pub const DMC_SSP_BASE_ADDR_GEN9: u32 = 0x00002FC0;

/// `_PIPEDMC_CONTROL_A` (intel_dmc_regs.h)
pub const _PIPEDMC_CONTROL_A: u32 = 0x45250;

/// `_PIPEDMC_CONTROL_B` (intel_dmc_regs.h)
pub const _PIPEDMC_CONTROL_B: u32 = 0x45254;

/// `PIPEDMC_CONTROL(pipe)` (intel_dmc_regs.h)
pub const fn PIPEDMC_CONTROL(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPEDMC_CONTROL_A, _PIPEDMC_CONTROL_B)
}

/// `PIPEDMC_ENABLE` (intel_dmc_regs.h)
pub const PIPEDMC_ENABLE: u32 = reg_bit(0);

/// `MTL_PIPEDMC_CONTROL` (intel_dmc_regs.h)
pub const MTL_PIPEDMC_CONTROL: I915RegT = mmio(0x45250);

/// `PIPEDMC_ENABLE_MTL(pipe)` (intel_dmc_regs.h)
pub const fn PIPEDMC_ENABLE_MTL(pipe: u32) -> u32 {
    reg_bit(((((pipe) - (PIPE_A as u32))) * 4))
}

/// `_PIPEDMC_LOAD_HTP_A` (intel_dmc_regs.h)
pub const _PIPEDMC_LOAD_HTP_A: u32 = 0x5f000;

/// `_PIPEDMC_LOAD_HTP_B` (intel_dmc_regs.h)
pub const _PIPEDMC_LOAD_HTP_B: u32 = 0x5f400;

/// `PIPEDMC_LOAD_HTP(pipe)` (intel_dmc_regs.h)
pub const fn PIPEDMC_LOAD_HTP(pipe: u32) -> I915RegT {
    _MMIO_PIPE((pipe), _PIPEDMC_LOAD_HTP_A, _PIPEDMC_LOAD_HTP_B)
}

/// `_PIPEDMC_CTL_A` (intel_dmc_regs.h)
pub const _PIPEDMC_CTL_A: u32 = 0x5f064;

/// `_PIPEDMC_CTL_B` (intel_dmc_regs.h)
pub const _PIPEDMC_CTL_B: u32 = 0x5f464;

/// `PIPEDMC_CTL(pipe)` (intel_dmc_regs.h)
pub const fn PIPEDMC_CTL(pipe: u32) -> I915RegT {
    _MMIO_PIPE((pipe), _PIPEDMC_CTL_A, _PIPEDMC_CTL_B)
}

/// `PIPEDMC_HALT` (intel_dmc_regs.h)
pub const PIPEDMC_HALT: u32 = reg_bit(31);

/// `PIPEDMC_STEP` (intel_dmc_regs.h)
pub const PIPEDMC_STEP: u32 = reg_bit(27);

/// `PIPEDMC_CLOCKGATE` (intel_dmc_regs.h)
pub const PIPEDMC_CLOCKGATE: u32 = reg_bit(23);

/// `_PIPEDMC_STATUS_A` (intel_dmc_regs.h)
pub const _PIPEDMC_STATUS_A: u32 = 0x5f06c;

/// `_PIPEDMC_STATUS_B` (intel_dmc_regs.h)
pub const _PIPEDMC_STATUS_B: u32 = 0x5f46c;

/// `PIPEDMC_STATUS(pipe)` (intel_dmc_regs.h)
pub const fn PIPEDMC_STATUS(pipe: u32) -> I915RegT {
    _MMIO_PIPE((pipe), _PIPEDMC_STATUS_A, _PIPEDMC_STATUS_B)
}

/// `PIPEDMC_SSP` (intel_dmc_regs.h)
pub const PIPEDMC_SSP: u32 = reg_genmask(31, 16);

/// `PIPEDMC_INT_VECTOR_MASK` (intel_dmc_regs.h)
pub const PIPEDMC_INT_VECTOR_MASK: u32 = reg_genmask(15, 8);

/// `PIPEDMC_INT_VECTOR_SCANLINE_COMP_ERROR` (intel_dmc_regs.h)
pub const PIPEDMC_INT_VECTOR_SCANLINE_COMP_ERROR: u32 = reg_field_prep(PIPEDMC_INT_VECTOR_MASK, 0x1);

/// `PIPEDMC_INT_VECTOR_DC6V_FLIPQ_OVERLAP_ERROR` (intel_dmc_regs.h)
pub const PIPEDMC_INT_VECTOR_DC6V_FLIPQ_OVERLAP_ERROR: u32 = reg_field_prep(PIPEDMC_INT_VECTOR_MASK, 0x2);

/// `PIPEDMC_INT_VECTOR_FLIPQ_PROG_DONE` (intel_dmc_regs.h)
pub const PIPEDMC_INT_VECTOR_FLIPQ_PROG_DONE: u32 = reg_field_prep(PIPEDMC_INT_VECTOR_MASK, 0xff);

/// `PIPEDMC_EVT_PENDING` (intel_dmc_regs.h)
pub const PIPEDMC_EVT_PENDING: u32 = reg_genmask(7, 0);

/// `_PIPEDMC_FQ_CTRL_A` (intel_dmc_regs.h)
pub const _PIPEDMC_FQ_CTRL_A: u32 = 0x5f078;

/// `_PIPEDMC_FQ_CTRL_B` (intel_dmc_regs.h)
pub const _PIPEDMC_FQ_CTRL_B: u32 = 0x5f478;

/// `PIPEDMC_FQ_CTRL(pipe)` (intel_dmc_regs.h)
pub const fn PIPEDMC_FQ_CTRL(pipe: u32) -> I915RegT {
    _MMIO_PIPE((pipe), _PIPEDMC_FQ_CTRL_A, _PIPEDMC_FQ_CTRL_B)
}

/// `PIPEDMC_FQ_CTRL_ENABLE` (intel_dmc_regs.h)
pub const PIPEDMC_FQ_CTRL_ENABLE: u32 = reg_bit(31);

/// `PIPEDMC_FQ_CTRL_ASYNC` (intel_dmc_regs.h)
pub const PIPEDMC_FQ_CTRL_ASYNC: u32 = reg_bit(29);

/// `PIPEDMC_FQ_CTRL_PREEMPT` (intel_dmc_regs.h)
pub const PIPEDMC_FQ_CTRL_PREEMPT: u32 = reg_bit(0);

/// `_PIPEDMC_FQ_STATUS_A` (intel_dmc_regs.h)
pub const _PIPEDMC_FQ_STATUS_A: u32 = 0x5f098;

/// `_PIPEDMC_FQ_STATUS_B` (intel_dmc_regs.h)
pub const _PIPEDMC_FQ_STATUS_B: u32 = 0x5f498;

/// `PIPEDMC_FQ_STATUS(pipe)` (intel_dmc_regs.h)
pub const fn PIPEDMC_FQ_STATUS(pipe: u32) -> I915RegT {
    _MMIO_PIPE((pipe), _PIPEDMC_FQ_STATUS_A, _PIPEDMC_FQ_STATUS_B)
}

/// `PIPEDMC_FQ_STATUS_BUSY` (intel_dmc_regs.h)
pub const PIPEDMC_FQ_STATUS_BUSY: u32 = reg_bit(31);

/// `PIPEDMC_FQ_STATUS_W2_LIVE_STATUS` (intel_dmc_regs.h)
pub const PIPEDMC_FQ_STATUS_W2_LIVE_STATUS: u32 = reg_bit(1);

/// `PIPEDMC_FQ_STATUS_W1_LIVE_STATUS` (intel_dmc_regs.h)
pub const PIPEDMC_FQ_STATUS_W1_LIVE_STATUS: u32 = reg_bit(0);

/// `_PIPEDMC_FPQ_ATOMIC_TP_A` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ_ATOMIC_TP_A: u32 = 0x5f0a0;

/// `_PIPEDMC_FPQ_ATOMIC_TP_B` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ_ATOMIC_TP_B: u32 = 0x5f4a0;

/// `PIPEDMC_FPQ_ATOMIC_TP(pipe)` (intel_dmc_regs.h)
pub const fn PIPEDMC_FPQ_ATOMIC_TP(pipe: u32) -> I915RegT {
    _MMIO_PIPE((pipe), _PIPEDMC_FPQ_ATOMIC_TP_A, _PIPEDMC_FPQ_ATOMIC_TP_B)
}

/// `PIPEDMC_FPQ_PLANEQ_3_TP_MASK` (intel_dmc_regs.h)
pub const PIPEDMC_FPQ_PLANEQ_3_TP_MASK: u32 = reg_genmask(31, 26);

/// `PIPEDMC_FPQ_PLANEQ_3_TP(tail)` (intel_dmc_regs.h)
pub const fn PIPEDMC_FPQ_PLANEQ_3_TP(tail: u32) -> u32 {
    reg_field_prep(PIPEDMC_FPQ_PLANEQ_3_TP_MASK, (tail))
}

/// `PIPEDMC_FPQ_PLANEQ_2_TP_MASK` (intel_dmc_regs.h)
pub const PIPEDMC_FPQ_PLANEQ_2_TP_MASK: u32 = reg_genmask(24, 19);

/// `PIPEDMC_FPQ_PLANEQ_2_TP(tail)` (intel_dmc_regs.h)
pub const fn PIPEDMC_FPQ_PLANEQ_2_TP(tail: u32) -> u32 {
    reg_field_prep(PIPEDMC_FPQ_PLANEQ_2_TP_MASK, (tail))
}

/// `PIPEDMC_FPQ_PLANEQ_1_TP_MASK` (intel_dmc_regs.h)
pub const PIPEDMC_FPQ_PLANEQ_1_TP_MASK: u32 = reg_genmask(17, 12);

/// `PIPEDMC_FPQ_PLANEQ_1_TP(tail)` (intel_dmc_regs.h)
pub const fn PIPEDMC_FPQ_PLANEQ_1_TP(tail: u32) -> u32 {
    reg_field_prep(PIPEDMC_FPQ_PLANEQ_1_TP_MASK, (tail))
}

/// `PIPEDMC_FPQ_FASTQ_TP_MASK` (intel_dmc_regs.h)
pub const PIPEDMC_FPQ_FASTQ_TP_MASK: u32 = reg_genmask(10, 6);

/// `PIPEDMC_FPQ_FASTQ_TP(tail)` (intel_dmc_regs.h)
pub const fn PIPEDMC_FPQ_FASTQ_TP(tail: u32) -> u32 {
    reg_field_prep(PIPEDMC_FPQ_FASTQ_TP_MASK, (tail))
}

/// `PIPEDMC_FPQ_GENERALQ_TP_MASK` (intel_dmc_regs.h)
pub const PIPEDMC_FPQ_GENERALQ_TP_MASK: u32 = reg_genmask(4, 0);

/// `PIPEDMC_FPQ_GENERALQ_TP(tail)` (intel_dmc_regs.h)
pub const fn PIPEDMC_FPQ_GENERALQ_TP(tail: u32) -> u32 {
    reg_field_prep(PIPEDMC_FPQ_GENERALQ_TP_MASK, (tail))
}

/// `_PIPEDMC_FPQ_LINES_TO_W1_A` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ_LINES_TO_W1_A: u32 = 0x5f0a4;

/// `_PIPEDMC_FPQ_LINES_TO_W1_B` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ_LINES_TO_W1_B: u32 = 0x5f4a4;

/// `_PIPEDMC_FPQ_LINES_TO_W2_A` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ_LINES_TO_W2_A: u32 = 0x5f0a8;

/// `_PIPEDMC_FPQ_LINES_TO_W2_B` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ_LINES_TO_W2_B: u32 = 0x5f4a8;

/// `_PIPEDMC_SCANLINECMP_A` (intel_dmc_regs.h)
pub const _PIPEDMC_SCANLINECMP_A: u32 = 0x5f11c;

/// `_PIPEDMC_SCANLINECMP_B` (intel_dmc_regs.h)
pub const _PIPEDMC_SCANLINECMP_B: u32 = 0x5f51c;

/// `PIPEDMC_SCANLINECMP(pipe)` (intel_dmc_regs.h)
pub const fn PIPEDMC_SCANLINECMP(pipe: u32) -> I915RegT {
    _MMIO_PIPE((pipe), _PIPEDMC_SCANLINECMP_A, _PIPEDMC_SCANLINECMP_B)
}

/// `PIPEDMC_SCANLINECMP_EN` (intel_dmc_regs.h)
pub const PIPEDMC_SCANLINECMP_EN: u32 = reg_bit(31);

/// `PIPEDMC_SCANLINE_NUMBER` (intel_dmc_regs.h)
pub const PIPEDMC_SCANLINE_NUMBER: u32 = reg_genmask(20, 0);

/// `_PIPEDMC_SCANLINECMPLOWER_A` (intel_dmc_regs.h)
pub const _PIPEDMC_SCANLINECMPLOWER_A: u32 = 0x5f120;

/// `_PIPEDMC_SCANLINECMPLOWER_B` (intel_dmc_regs.h)
pub const _PIPEDMC_SCANLINECMPLOWER_B: u32 = 0x5f520;

/// `PIPEDMC_SCANLINECMPLOWER(pipe)` (intel_dmc_regs.h)
pub const fn PIPEDMC_SCANLINECMPLOWER(pipe: u32) -> I915RegT {
    _MMIO_PIPE((pipe), _PIPEDMC_SCANLINECMPLOWER_A, _PIPEDMC_SCANLINECMPLOWER_B)
}

/// `PIPEDMC_SCANLINEINRANGECMP_EN` (intel_dmc_regs.h)
pub const PIPEDMC_SCANLINEINRANGECMP_EN: u32 = reg_bit(31);

/// `PIPEDMC_SCANLINEOUTRANGECMP_EN` (intel_dmc_regs.h)
pub const PIPEDMC_SCANLINEOUTRANGECMP_EN: u32 = reg_bit(30);

/// `PIPEDMC_SCANLINE_LOWER_MASK` (intel_dmc_regs.h)
pub const PIPEDMC_SCANLINE_LOWER_MASK: u32 = reg_genmask(20, 0);

/// `PIPEDMC_SCANLINE_LOWER(scanline)` (intel_dmc_regs.h)
pub const fn PIPEDMC_SCANLINE_LOWER(scanline: u32) -> u32 {
    reg_field_prep(PIPEDMC_SCANLINE_LOWER_MASK, (scanline))
}

/// `_PIPEDMC_SCANLINECMPUPPER_A` (intel_dmc_regs.h)
pub const _PIPEDMC_SCANLINECMPUPPER_A: u32 = 0x5f124;

/// `_PIPEDMC_SCANLINECMPUPPER_B` (intel_dmc_regs.h)
pub const _PIPEDMC_SCANLINECMPUPPER_B: u32 = 0x5f524;

/// `PIPEDMC_SCANLINECMPUPPER(pipe)` (intel_dmc_regs.h)
pub const fn PIPEDMC_SCANLINECMPUPPER(pipe: u32) -> I915RegT {
    _MMIO_PIPE((pipe), _PIPEDMC_SCANLINECMPUPPER_A, _PIPEDMC_SCANLINECMPUPPER_B)
}

/// `PIPEDMC_SCANLINE_UPPER_MASK` (intel_dmc_regs.h)
pub const PIPEDMC_SCANLINE_UPPER_MASK: u32 = reg_genmask(20, 0);

/// `PIPEDMC_SCANLINE_UPPER(scanline)` (intel_dmc_regs.h)
pub const fn PIPEDMC_SCANLINE_UPPER(scanline: u32) -> u32 {
    reg_field_prep(PIPEDMC_SCANLINE_UPPER_MASK, (scanline))
}

/// `_MMIO_PIPEDMC_FPQ(pipe, fq_id, reg_fpq1_a, reg_fpq2_a, reg_fpq3_a, reg_fpq4_a, reg_fpq1_b, reg_fpq2_b, reg_fpq3_b, reg_fpq4_b)` (intel_dmc_regs.h)
pub const fn _MMIO_PIPEDMC_FPQ(pipe: u32, fq_id: u32, reg_fpq1_a: u32, reg_fpq2_a: u32, reg_fpq3_a: u32, reg_fpq4_a: u32, reg_fpq1_b: u32, reg_fpq2_b: u32, reg_fpq3_b: u32, reg_fpq4_b: u32) -> I915RegT {
    mmio(pick_even_2ranges((fq_id), (INTEL_FLIPQ_PLANE_3 as u32), _PIPE((pipe), (reg_fpq1_a), (reg_fpq1_b)), _PIPE((pipe), (reg_fpq2_a), (reg_fpq2_b)), _PIPE((pipe), (reg_fpq3_a), (reg_fpq3_b)), _PIPE((pipe), (reg_fpq4_a), (reg_fpq4_b))))
}

/// `_PIPEDMC_FPQ1_HP_A` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ1_HP_A: u32 = 0x5f128;

/// `_PIPEDMC_FPQ2_HP_A` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ2_HP_A: u32 = 0x5f138;

/// `_PIPEDMC_FPQ3_HP_A` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ3_HP_A: u32 = 0x5f168;

/// `_PIPEDMC_FPQ4_HP_A` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ4_HP_A: u32 = 0x5f174;

/// `_PIPEDMC_FPQ5_HP_A` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ5_HP_A: u32 = 0x5f180;

/// `_PIPEDMC_FPQ1_HP_B` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ1_HP_B: u32 = 0x5f528;

/// `_PIPEDMC_FPQ2_HP_B` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ2_HP_B: u32 = 0x5f538;

/// `_PIPEDMC_FPQ3_HP_B` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ3_HP_B: u32 = 0x5f568;

/// `_PIPEDMC_FPQ4_HP_B` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ4_HP_B: u32 = 0x5f574;

/// `_PIPEDMC_FPQ5_HP_B` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ5_HP_B: u32 = 0x5f580;

/// `PIPEDMC_FPQ_HP(pipe, fq_id)` (intel_dmc_regs.h)
pub const fn PIPEDMC_FPQ_HP(pipe: u32, fq_id: u32) -> I915RegT {
    _MMIO_PIPEDMC_FPQ((pipe), (fq_id), _PIPEDMC_FPQ1_HP_A, _PIPEDMC_FPQ2_HP_A, _PIPEDMC_FPQ3_HP_A, _PIPEDMC_FPQ4_HP_A, _PIPEDMC_FPQ1_HP_B, _PIPEDMC_FPQ2_HP_B, _PIPEDMC_FPQ3_HP_B, _PIPEDMC_FPQ4_HP_B)
}

/// `_PIPEDMC_FPQ1_TP_A` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ1_TP_A: u32 = 0x5f12c;

/// `_PIPEDMC_FPQ2_TP_A` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ2_TP_A: u32 = 0x5f13c;

/// `_PIPEDMC_FPQ3_TP_A` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ3_TP_A: u32 = 0x5f16c;

/// `_PIPEDMC_FPQ4_TP_A` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ4_TP_A: u32 = 0x5f178;

/// `_PIPEDMC_FPQ5_TP_A` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ5_TP_A: u32 = 0x5f184;

/// `_PIPEDMC_FPQ1_TP_B` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ1_TP_B: u32 = 0x5f52c;

/// `_PIPEDMC_FPQ2_TP_B` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ2_TP_B: u32 = 0x5f53c;

/// `_PIPEDMC_FPQ3_TP_B` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ3_TP_B: u32 = 0x5f56c;

/// `_PIPEDMC_FPQ4_TP_B` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ4_TP_B: u32 = 0x5f578;

/// `_PIPEDMC_FPQ5_TP_B` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ5_TP_B: u32 = 0x5f584;

/// `PIPEDMC_FPQ_TP(pipe, fq_id)` (intel_dmc_regs.h)
pub const fn PIPEDMC_FPQ_TP(pipe: u32, fq_id: u32) -> I915RegT {
    _MMIO_PIPEDMC_FPQ((pipe), (fq_id), _PIPEDMC_FPQ1_TP_A, _PIPEDMC_FPQ2_TP_A, _PIPEDMC_FPQ3_TP_A, _PIPEDMC_FPQ4_TP_A, _PIPEDMC_FPQ1_TP_B, _PIPEDMC_FPQ2_TP_B, _PIPEDMC_FPQ3_TP_B, _PIPEDMC_FPQ4_TP_B)
}

/// `_PIPEDMC_FPQ1_CHP_A` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ1_CHP_A: u32 = 0x5f130;

/// `_PIPEDMC_FPQ2_CHP_A` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ2_CHP_A: u32 = 0x5f140;

/// `_PIPEDMC_FPQ3_CHP_A` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ3_CHP_A: u32 = 0x5f170;

/// `_PIPEDMC_FPQ4_CHP_A` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ4_CHP_A: u32 = 0x5f17c;

/// `_PIPEDMC_FPQ5_CHP_A` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ5_CHP_A: u32 = 0x5f188;

/// `_PIPEDMC_FPQ1_CHP_B` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ1_CHP_B: u32 = 0x5f530;

/// `_PIPEDMC_FPQ2_CHP_B` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ2_CHP_B: u32 = 0x5f540;

/// `_PIPEDMC_FPQ3_CHP_B` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ3_CHP_B: u32 = 0x5f570;

/// `_PIPEDMC_FPQ4_CHP_B` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ4_CHP_B: u32 = 0x5f57c;

/// `_PIPEDMC_FPQ5_CHP_B` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ5_CHP_B: u32 = 0x5f588;

/// `PIPEDMC_FPQ_CHP(pipe, fq_id)` (intel_dmc_regs.h)
pub const fn PIPEDMC_FPQ_CHP(pipe: u32, fq_id: u32) -> I915RegT {
    _MMIO_PIPEDMC_FPQ((pipe), (fq_id), _PIPEDMC_FPQ1_CHP_A, _PIPEDMC_FPQ2_CHP_A, _PIPEDMC_FPQ3_CHP_A, _PIPEDMC_FPQ4_CHP_A, _PIPEDMC_FPQ1_CHP_B, _PIPEDMC_FPQ2_CHP_B, _PIPEDMC_FPQ3_CHP_B, _PIPEDMC_FPQ4_CHP_B)
}

/// `_PIPEDMC_FPQ_TS_A` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ_TS_A: u32 = 0x5f134;

/// `_PIPEDMC_FPQ_TS_B` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ_TS_B: u32 = 0x5f534;

/// `PIPEDMC_FPQ_TS(pipe)` (intel_dmc_regs.h)
pub const fn PIPEDMC_FPQ_TS(pipe: u32) -> I915RegT {
    _MMIO_PIPE((pipe), _PIPEDMC_FPQ_TS_A, _PIPEDMC_FPQ_TS_B)
}

/// `_PIPEDMC_SCANLINE_RO_A` (intel_dmc_regs.h)
pub const _PIPEDMC_SCANLINE_RO_A: u32 = 0x5f144;

/// `_PIPEDMC_SCANLINE_RO_B` (intel_dmc_regs.h)
pub const _PIPEDMC_SCANLINE_RO_B: u32 = 0x5f544;

/// `PIPEDMC_SCANLINE_RO(pipe)` (intel_dmc_regs.h)
pub const fn PIPEDMC_SCANLINE_RO(pipe: u32) -> I915RegT {
    _MMIO_PIPE((pipe), _PIPEDMC_SCANLINE_RO_A, _PIPEDMC_SCANLINE_RO_B)
}

/// `_PIPEDMC_FPQ_CTL1_A` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ_CTL1_A: u32 = 0x5f160;

/// `_PIPEDMC_FPQ_CTL1_B` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ_CTL1_B: u32 = 0x5f560;

/// `PIPEDMC_FPQ_CTL1(pipe)` (intel_dmc_regs.h)
pub const fn PIPEDMC_FPQ_CTL1(pipe: u32) -> I915RegT {
    _MMIO_PIPE((pipe), _PIPEDMC_FPQ_CTL1_A, _PIPEDMC_FPQ_CTL1_B)
}

/// `PIPEDMC_SW_DMC_WAKE` (intel_dmc_regs.h)
pub const PIPEDMC_SW_DMC_WAKE: u32 = reg_bit(0);

/// `_PIPEDMC_FPQ_CTL2_A` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ_CTL2_A: u32 = 0x5f164;

/// `_PIPEDMC_FPQ_CTL2_B` (intel_dmc_regs.h)
pub const _PIPEDMC_FPQ_CTL2_B: u32 = 0x5f564;

/// `PIPEDMC_FPQ_CTL2(pipe)` (intel_dmc_regs.h)
pub const fn PIPEDMC_FPQ_CTL2(pipe: u32) -> I915RegT {
    _MMIO_PIPE((pipe), _PIPEDMC_FPQ_CTL2_A, _PIPEDMC_FPQ_CTL2_B)
}

/// `PIPEDMC_DMC_INT_AT_DELAYED_VBLANK` (intel_dmc_regs.h)
pub const PIPEDMC_DMC_INT_AT_DELAYED_VBLANK: u32 = reg_bit(1);

/// `PIPEDMC_W1_DMC_WAKE` (intel_dmc_regs.h)
pub const PIPEDMC_W1_DMC_WAKE: u32 = reg_bit(0);

/// `_PIPEDMC_INTERRUPT_A` (intel_dmc_regs.h)
pub const _PIPEDMC_INTERRUPT_A: u32 = 0x5f190;

/// `_PIPEDMC_INTERRUPT_B` (intel_dmc_regs.h)
pub const _PIPEDMC_INTERRUPT_B: u32 = 0x5f590;

/// `PIPEDMC_INTERRUPT(pipe)` (intel_dmc_regs.h)
pub const fn PIPEDMC_INTERRUPT(pipe: u32) -> I915RegT {
    _MMIO_PIPE((pipe), _PIPEDMC_INTERRUPT_A, _PIPEDMC_INTERRUPT_B)
}

/// `_PIPEDMC_INTERRUPT_MASK_A` (intel_dmc_regs.h)
pub const _PIPEDMC_INTERRUPT_MASK_A: u32 = 0x5f194;

/// `_PIPEDMC_INTERRUPT_MASK_B` (intel_dmc_regs.h)
pub const _PIPEDMC_INTERRUPT_MASK_B: u32 = 0x5f594;

/// `PIPEDMC_INTERRUPT_MASK(pipe)` (intel_dmc_regs.h)
pub const fn PIPEDMC_INTERRUPT_MASK(pipe: u32) -> I915RegT {
    _MMIO_PIPE((pipe), _PIPEDMC_INTERRUPT_MASK_A, _PIPEDMC_INTERRUPT_MASK_B)
}

/// `PIPEDMC_FLIPQ_PROG_DONE` (intel_dmc_regs.h)
pub const PIPEDMC_FLIPQ_PROG_DONE: u32 = reg_bit(3);

/// `PIPEDMC_ERROR` (intel_dmc_regs.h)
pub const PIPEDMC_ERROR: u32 = reg_bit(2);

/// `PIPEDMC_GTT_FAULT` (intel_dmc_regs.h)
pub const PIPEDMC_GTT_FAULT: u32 = reg_bit(1);

/// `PIPEDMC_ATS_FAULT` (intel_dmc_regs.h)
pub const PIPEDMC_ATS_FAULT: u32 = reg_bit(0);

/// `PIPEDMC_BLOCK_PKGC_SW_A` (intel_dmc_regs.h)
pub const PIPEDMC_BLOCK_PKGC_SW_A: u32 = 0x5f1d0;

/// `PIPEDMC_BLOCK_PKGC_SW_B` (intel_dmc_regs.h)
pub const PIPEDMC_BLOCK_PKGC_SW_B: u32 = 0x5F5d0;

/// `PIPEDMC_BLOCK_PKGC_SW(pipe)` (intel_dmc_regs.h)
pub const fn PIPEDMC_BLOCK_PKGC_SW(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, PIPEDMC_BLOCK_PKGC_SW_A, PIPEDMC_BLOCK_PKGC_SW_B)
}

/// `PIPEDMC_BLOCK_PKGC_SW_BLOCK_PKGC_ALWAYS` (intel_dmc_regs.h)
pub const PIPEDMC_BLOCK_PKGC_SW_BLOCK_PKGC_ALWAYS: u32 = reg_bit(31);

/// `PIPEDMC_BLOCK_PKGC_SW_BLOCK_PKGC_UNTIL_NEXT_FRAMESTART` (intel_dmc_regs.h)
pub const PIPEDMC_BLOCK_PKGC_SW_BLOCK_PKGC_UNTIL_NEXT_FRAMESTART: u32 = reg_bit(15);

/// `_ADLP_PIPEDMC_REG_MMIO_BASE_A` (intel_dmc_regs.h)
pub const _ADLP_PIPEDMC_REG_MMIO_BASE_A: u32 = 0x5f000;

/// `_TGL_PIPEDMC_REG_MMIO_BASE_A` (intel_dmc_regs.h)
pub const _TGL_PIPEDMC_REG_MMIO_BASE_A: u32 = 0x92000;

/// `DMC_EVENT_HANDLER_COUNT_GEN12` (intel_dmc_regs.h)
pub const DMC_EVENT_HANDLER_COUNT_GEN12: u32 = 8;

/// `_DMC_EVT_HTP_0` (intel_dmc_regs.h)
pub const _DMC_EVT_HTP_0: u32 = 0x8f004;

/// `_DMC_EVT_CTL_0` (intel_dmc_regs.h)
pub const _DMC_EVT_CTL_0: u32 = 0x8f034;

/// `DMC_EVT_CTL_ENABLE` (intel_dmc_regs.h)
pub const DMC_EVT_CTL_ENABLE: u32 = reg_bit(31);

/// `DMC_EVT_CTL_RECURRING` (intel_dmc_regs.h)
pub const DMC_EVT_CTL_RECURRING: u32 = reg_bit(30);

/// `DMC_EVT_CTL_TYPE_MASK` (intel_dmc_regs.h)
pub const DMC_EVT_CTL_TYPE_MASK: u32 = reg_genmask(17, 16);

/// `DMC_EVT_CTL_TYPE_LEVEL_0` (intel_dmc_regs.h)
pub const DMC_EVT_CTL_TYPE_LEVEL_0: u32 = 0;

/// `DMC_EVT_CTL_TYPE_LEVEL_1` (intel_dmc_regs.h)
pub const DMC_EVT_CTL_TYPE_LEVEL_1: u32 = 1;

/// `DMC_EVT_CTL_TYPE_EDGE_1_0` (intel_dmc_regs.h)
pub const DMC_EVT_CTL_TYPE_EDGE_1_0: u32 = 2;

/// `DMC_EVT_CTL_TYPE_EDGE_0_1` (intel_dmc_regs.h)
pub const DMC_EVT_CTL_TYPE_EDGE_0_1: u32 = 3;

/// `DMC_EVT_CTL_EVENT_ID_MASK` (intel_dmc_regs.h)
pub const DMC_EVT_CTL_EVENT_ID_MASK: u32 = reg_genmask(15, 8);

/// `DMC_HTP_ADDR_SKL` (intel_dmc_regs.h)
pub const DMC_HTP_ADDR_SKL: u32 = 0x00500034;

/// `DMC_SSP_BASE` (intel_dmc_regs.h)
pub const DMC_SSP_BASE: I915RegT = mmio(0x8F074);

/// `DMC_HTP_SKL` (intel_dmc_regs.h)
pub const DMC_HTP_SKL: I915RegT = mmio(0x8F004);

/// `DMC_LAST_WRITE` (intel_dmc_regs.h)
pub const DMC_LAST_WRITE: I915RegT = mmio(0x8F034);

/// `DMC_LAST_WRITE_VALUE` (intel_dmc_regs.h)
pub const DMC_LAST_WRITE_VALUE: u32 = 0xc003b400;

/// `DMC_MMIO_START_RANGE` (intel_dmc_regs.h)
pub const DMC_MMIO_START_RANGE: u32 = 0x80000;

/// `DMC_MMIO_END_RANGE` (intel_dmc_regs.h)
pub const DMC_MMIO_END_RANGE: u32 = 0x8FFFF;

/// `DMC_V1_MMIO_START_RANGE` (intel_dmc_regs.h)
pub const DMC_V1_MMIO_START_RANGE: u32 = 0x80000;

/// `TGL_MAIN_MMIO_START` (intel_dmc_regs.h)
pub const TGL_MAIN_MMIO_START: u32 = 0x8F000;

/// `TGL_MAIN_MMIO_END` (intel_dmc_regs.h)
pub const TGL_MAIN_MMIO_END: u32 = 0x8FFFF;

/// `_TGL_PIPEA_MMIO_START` (intel_dmc_regs.h)
pub const _TGL_PIPEA_MMIO_START: u32 = 0x92000;

/// `_TGL_PIPEA_MMIO_END` (intel_dmc_regs.h)
pub const _TGL_PIPEA_MMIO_END: u32 = 0x93FFF;

/// `_TGL_PIPEB_MMIO_START` (intel_dmc_regs.h)
pub const _TGL_PIPEB_MMIO_START: u32 = 0x96000;

/// `_TGL_PIPEB_MMIO_END` (intel_dmc_regs.h)
pub const _TGL_PIPEB_MMIO_END: u32 = 0x97FFF;

/// `ADLP_PIPE_MMIO_START` (intel_dmc_regs.h)
pub const ADLP_PIPE_MMIO_START: u32 = 0x5F000;

/// `ADLP_PIPE_MMIO_END` (intel_dmc_regs.h)
pub const ADLP_PIPE_MMIO_END: u32 = 0x5FFFF;

/// `TGL_PIPE_MMIO_START(dmc_id)` (intel_dmc_regs.h)
pub const fn TGL_PIPE_MMIO_START(dmc_id: u32) -> u32 {
    pick_even((((dmc_id) - 1)), _TGL_PIPEA_MMIO_START, _TGL_PIPEB_MMIO_START)
}

/// `TGL_PIPE_MMIO_END(dmc_id)` (intel_dmc_regs.h)
pub const fn TGL_PIPE_MMIO_END(dmc_id: u32) -> u32 {
    pick_even((((dmc_id) - 1)), _TGL_PIPEA_MMIO_END, _TGL_PIPEB_MMIO_END)
}

/// `SKL_DMC_DC3_DC5_COUNT` (intel_dmc_regs.h)
pub const SKL_DMC_DC3_DC5_COUNT: I915RegT = mmio(0x80030);

/// `SKL_DMC_DC5_DC6_COUNT` (intel_dmc_regs.h)
pub const SKL_DMC_DC5_DC6_COUNT: I915RegT = mmio(0x8002C);

/// `BXT_DMC_DC3_DC5_COUNT` (intel_dmc_regs.h)
pub const BXT_DMC_DC3_DC5_COUNT: I915RegT = mmio(0x80038);

/// `TGL_DMC_DEBUG_DC5_COUNT` (intel_dmc_regs.h)
pub const TGL_DMC_DEBUG_DC5_COUNT: I915RegT = mmio(0x101084);

/// `TGL_DMC_DEBUG_DC6_COUNT` (intel_dmc_regs.h)
pub const TGL_DMC_DEBUG_DC6_COUNT: I915RegT = mmio(0x101088);

/// `DG1_DMC_DEBUG_DC5_COUNT` (intel_dmc_regs.h)
pub const DG1_DMC_DEBUG_DC5_COUNT: I915RegT = mmio(0x134154);

/// `TGL_DMC_DEBUG3` (intel_dmc_regs.h)
pub const TGL_DMC_DEBUG3: I915RegT = mmio(0x101090);

/// `DG1_DMC_DEBUG3` (intel_dmc_regs.h)
pub const DG1_DMC_DEBUG3: I915RegT = mmio(0x13415c);

/// `DMC_WAKELOCK_CFG` (intel_dmc_regs.h)
pub const DMC_WAKELOCK_CFG: I915RegT = mmio(0x8F1B0);

/// `DMC_WAKELOCK_CFG_ENABLE` (intel_dmc_regs.h)
pub const DMC_WAKELOCK_CFG_ENABLE: u32 = reg_bit(31);

/// `DMC_WAKELOCK1_CTL` (intel_dmc_regs.h)
pub const DMC_WAKELOCK1_CTL: I915RegT = mmio(0x8F140);

/// `DMC_WAKELOCK_CTL_REQ` (intel_dmc_regs.h)
pub const DMC_WAKELOCK_CTL_REQ: u32 = reg_bit(31);

/// `DMC_WAKELOCK_CTL_ACK` (intel_dmc_regs.h)
pub const DMC_WAKELOCK_CTL_ACK: u32 = reg_bit(15);

/// `DMC_FQ_W2_PTS_CFG_SEL` (intel_dmc_regs.h)
pub const DMC_FQ_W2_PTS_CFG_SEL: I915RegT = mmio(0x8f240);

/// `PIPE_D_DMC_W2_PTS_CONFIG_SELECT_MASK` (intel_dmc_regs.h)
pub const PIPE_D_DMC_W2_PTS_CONFIG_SELECT_MASK: u32 = reg_genmask(26, 24);

/// `PIPE_D_DMC_W2_PTS_CONFIG_SELECT(pipe)` (intel_dmc_regs.h)
pub const fn PIPE_D_DMC_W2_PTS_CONFIG_SELECT(pipe: u32) -> u32 {
    reg_field_prep(PIPE_D_DMC_W2_PTS_CONFIG_SELECT_MASK, (pipe))
}

/// `PIPE_C_DMC_W2_PTS_CONFIG_SELECT_MASK` (intel_dmc_regs.h)
pub const PIPE_C_DMC_W2_PTS_CONFIG_SELECT_MASK: u32 = reg_genmask(18, 16);

/// `PIPE_C_DMC_W2_PTS_CONFIG_SELECT(pipe)` (intel_dmc_regs.h)
pub const fn PIPE_C_DMC_W2_PTS_CONFIG_SELECT(pipe: u32) -> u32 {
    reg_field_prep(PIPE_C_DMC_W2_PTS_CONFIG_SELECT_MASK, (pipe))
}

/// `PIPE_B_DMC_W2_PTS_CONFIG_SELECT_MASK` (intel_dmc_regs.h)
pub const PIPE_B_DMC_W2_PTS_CONFIG_SELECT_MASK: u32 = reg_genmask(10, 8);

/// `PIPE_B_DMC_W2_PTS_CONFIG_SELECT(pipe)` (intel_dmc_regs.h)
pub const fn PIPE_B_DMC_W2_PTS_CONFIG_SELECT(pipe: u32) -> u32 {
    reg_field_prep(PIPE_B_DMC_W2_PTS_CONFIG_SELECT_MASK, (pipe))
}

/// `PIPE_A_DMC_W2_PTS_CONFIG_SELECT_MASK` (intel_dmc_regs.h)
pub const PIPE_A_DMC_W2_PTS_CONFIG_SELECT_MASK: u32 = reg_genmask(2, 0);

/// `PIPE_A_DMC_W2_PTS_CONFIG_SELECT(pipe)` (intel_dmc_regs.h)
pub const fn PIPE_A_DMC_W2_PTS_CONFIG_SELECT(pipe: u32) -> u32 {
    reg_field_prep(PIPE_A_DMC_W2_PTS_CONFIG_SELECT_MASK, (pipe))
}

/// `PIPEDMC_FQ_RAM(start_mmioaddr, i)` (intel_dmc_regs.h)
pub const fn PIPEDMC_FQ_RAM(start_mmioaddr: u32, i: u32) -> I915RegT {
    mmio(((start_mmioaddr) + ((i) * 4)))
}

/// `LNL_FQ_INTERRUPT` (intel_dmc_regs.h)
pub const LNL_FQ_INTERRUPT: u32 = reg_bit(31);

/// `LNL_FQ_DSB_ID_MASK` (intel_dmc_regs.h)
pub const LNL_FQ_DSB_ID_MASK: u32 = reg_genmask(30, 29);

/// `LNL_FQ_DSB_ID(dsb_id)` (intel_dmc_regs.h)
pub const fn LNL_FQ_DSB_ID(dsb_id: u32) -> u32 {
    reg_field_prep(LNL_FQ_DSB_ID_MASK, (dsb_id))
}

/// `LNL_FQ_EXECUTED` (intel_dmc_regs.h)
pub const LNL_FQ_EXECUTED: u32 = reg_bit(28);

/// `LNL_FQ_DSB_SIZE_MASK` (intel_dmc_regs.h)
pub const LNL_FQ_DSB_SIZE_MASK: u32 = reg_genmask(15, 0);

/// `LNL_FQ_DSB_SIZE(size)` (intel_dmc_regs.h)
pub const fn LNL_FQ_DSB_SIZE(size: u32) -> u32 {
    reg_field_prep(LNL_FQ_DSB_SIZE_MASK, (size))
}

/// `PTL_FQ_INTERRUPT` (intel_dmc_regs.h)
pub const PTL_FQ_INTERRUPT: u32 = reg_bit(31);

/// `PTL_FQ_NEED_PUSH` (intel_dmc_regs.h)
pub const PTL_FQ_NEED_PUSH: u32 = reg_bit(30);

/// `PTL_FQ_BLOCK_PUSH` (intel_dmc_regs.h)
pub const PTL_FQ_BLOCK_PUSH: u32 = reg_bit(29);

/// `PTL_FQ_EXECUTED` (intel_dmc_regs.h)
pub const PTL_FQ_EXECUTED: u32 = reg_bit(28);

/// `PTL_FQ_DSB_ID_MASK` (intel_dmc_regs.h)
pub const PTL_FQ_DSB_ID_MASK: u32 = reg_genmask(25, 24);

/// `PTL_FQ_DSB_ID(dsb_id)` (intel_dmc_regs.h)
pub const fn PTL_FQ_DSB_ID(dsb_id: u32) -> u32 {
    reg_field_prep(PTL_FQ_DSB_ID_MASK, (dsb_id))
}

/// `PTL_FQ_DSB_SIZE_MASK` (intel_dmc_regs.h)
pub const PTL_FQ_DSB_SIZE_MASK: u32 = reg_genmask(15, 0);

/// `PTL_FQ_DSB_SIZE(size)` (intel_dmc_regs.h)
pub const fn PTL_FQ_DSB_SIZE(size: u32) -> u32 {
    reg_field_prep(PTL_FQ_DSB_SIZE_MASK, (size))
}

/// `PTL_PIPEDMC_EXEC_TIME_LINES(start_mmioaddr)` (intel_dmc_regs.h)
pub const fn PTL_PIPEDMC_EXEC_TIME_LINES(start_mmioaddr: u32) -> I915RegT {
    mmio(((start_mmioaddr) + 0x6b8))
}

/// `PTL_PIPEDMC_END_OF_EXEC_GB(start_mmioaddr)` (intel_dmc_regs.h)
pub const fn PTL_PIPEDMC_END_OF_EXEC_GB(start_mmioaddr: u32) -> I915RegT {
    mmio(((start_mmioaddr) + 0x6c0))
}

/// `_PIPEDMC_DCB_CTL_A` (intel_dmc_regs.h)
pub const _PIPEDMC_DCB_CTL_A: u32 = 0x5f1a0;

/// `_PIPEDMC_DCB_CTL_B` (intel_dmc_regs.h)
pub const _PIPEDMC_DCB_CTL_B: u32 = 0x5f5a0;

/// `PIPEDMC_DCB_CTL(pipe)` (intel_dmc_regs.h)
pub const fn PIPEDMC_DCB_CTL(pipe: u32) -> I915RegT {
    _MMIO_PIPE((pipe), _PIPEDMC_DCB_CTL_A, _PIPEDMC_DCB_CTL_B)
}

/// `PIPEDMC_ADAPTIVE_DCB_ENABLE` (intel_dmc_regs.h)
pub const PIPEDMC_ADAPTIVE_DCB_ENABLE: u32 = reg_bit(31);

/// `_PIPEDMC_DCB_VBLANK_A` (intel_dmc_regs.h)
pub const _PIPEDMC_DCB_VBLANK_A: u32 = 0x5f1bc;

/// `_PIPEDMC_DCB_VBLANK_B` (intel_dmc_regs.h)
pub const _PIPEDMC_DCB_VBLANK_B: u32 = 0x5f5bc;

/// `PIPEDMC_DCB_VBLANK(pipe)` (intel_dmc_regs.h)
pub const fn PIPEDMC_DCB_VBLANK(pipe: u32) -> I915RegT {
    _MMIO_PIPE((pipe), _PIPEDMC_DCB_VBLANK_A, _PIPEDMC_DCB_VBLANK_B)
}

/// `_PIPEDMC_DCB_SLOPE_A` (intel_dmc_regs.h)
pub const _PIPEDMC_DCB_SLOPE_A: u32 = 0x5f1b8;

/// `_PIPEDMC_DCB_SLOPE_B` (intel_dmc_regs.h)
pub const _PIPEDMC_DCB_SLOPE_B: u32 = 0x5f5b8;

/// `PIPEDMC_DCB_SLOPE(pipe)` (intel_dmc_regs.h)
pub const fn PIPEDMC_DCB_SLOPE(pipe: u32) -> I915RegT {
    _MMIO_PIPE((pipe), _PIPEDMC_DCB_SLOPE_A, _PIPEDMC_DCB_SLOPE_B)
}

/// `_PIPEDMC_DCB_GUARDBAND_A` (intel_dmc_regs.h)
pub const _PIPEDMC_DCB_GUARDBAND_A: u32 = 0x5f1b4;

/// `_PIPEDMC_DCB_GUARDBAND_B` (intel_dmc_regs.h)
pub const _PIPEDMC_DCB_GUARDBAND_B: u32 = 0x5f5b4;

/// `PIPEDMC_DCB_GUARDBAND(pipe)` (intel_dmc_regs.h)
pub const fn PIPEDMC_DCB_GUARDBAND(pipe: u32) -> I915RegT {
    _MMIO_PIPE((pipe), _PIPEDMC_DCB_GUARDBAND_A, _PIPEDMC_DCB_GUARDBAND_B)
}

/// `_PIPEDMC_DCB_MAX_INCREASE_A` (intel_dmc_regs.h)
pub const _PIPEDMC_DCB_MAX_INCREASE_A: u32 = 0x5f1ac;

/// `_PIPEDMC_DCB_MAX_INCREASE_B` (intel_dmc_regs.h)
pub const _PIPEDMC_DCB_MAX_INCREASE_B: u32 = 0x5f5ac;

/// `PIPEDMC_DCB_MAX_INCREASE(pipe)` (intel_dmc_regs.h)
pub const fn PIPEDMC_DCB_MAX_INCREASE(pipe: u32) -> I915RegT {
    _MMIO_PIPE((pipe), _PIPEDMC_DCB_MAX_INCREASE_A, _PIPEDMC_DCB_MAX_INCREASE_B)
}

/// `_PIPEDMC_DCB_MAX_DECREASE_A` (intel_dmc_regs.h)
pub const _PIPEDMC_DCB_MAX_DECREASE_A: u32 = 0x5f1b0;

/// `_PIPEDMC_DCB_MAX_DECREASE_B` (intel_dmc_regs.h)
pub const _PIPEDMC_DCB_MAX_DECREASE_B: u32 = 0x5f5b0;

/// `PIPEDMC_DCB_MAX_DECREASE(pipe)` (intel_dmc_regs.h)
pub const fn PIPEDMC_DCB_MAX_DECREASE(pipe: u32) -> I915RegT {
    _MMIO_PIPE((pipe), _PIPEDMC_DCB_MAX_DECREASE_A, _PIPEDMC_DCB_MAX_DECREASE_B)
}

/// `_PIPEDMC_DCB_VMIN_A` (intel_dmc_regs.h)
pub const _PIPEDMC_DCB_VMIN_A: u32 = 0x5f1a4;

/// `_PIPEDMC_DCB_VMIN_B` (intel_dmc_regs.h)
pub const _PIPEDMC_DCB_VMIN_B: u32 = 0x5f5a4;

/// `PIPEDMC_DCB_VMIN(pipe)` (intel_dmc_regs.h)
pub const fn PIPEDMC_DCB_VMIN(pipe: u32) -> I915RegT {
    _MMIO_PIPE((pipe), _PIPEDMC_DCB_VMIN_A, _PIPEDMC_DCB_VMIN_B)
}

/// `_PIPEDMC_DCB_VMAX_A` (intel_dmc_regs.h)
pub const _PIPEDMC_DCB_VMAX_A: u32 = 0x5f1a8;

/// `_PIPEDMC_DCB_VMAX_B` (intel_dmc_regs.h)
pub const _PIPEDMC_DCB_VMAX_B: u32 = 0x5f5a8;

/// `PIPEDMC_DCB_VMAX(pipe)` (intel_dmc_regs.h)
pub const fn PIPEDMC_DCB_VMAX(pipe: u32) -> I915RegT {
    _MMIO_PIPE((pipe), _PIPEDMC_DCB_VMAX_A, _PIPEDMC_DCB_VMAX_B)
}

/// `_PIPEDMC_DCB_DEBUG_A` (intel_dmc_regs.h)
pub const _PIPEDMC_DCB_DEBUG_A: u32 = 0x5f1c0;

/// `_PIPEDMC_DCB_DEBUG_B` (intel_dmc_regs.h)
pub const _PIPEDMC_DCB_DEBUG_B: u32 = 0x5f5c0;

/// `PIPEDMC_DCB_DEBUG(pipe)` (intel_dmc_regs.h)
pub const fn PIPEDMC_DCB_DEBUG(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPEDMC_DCB_DEBUG_A, _PIPEDMC_DCB_DEBUG_B)
}

/// `_PIPEDMC_EVT_CTL_3_A` (intel_dmc_regs.h)
pub const _PIPEDMC_EVT_CTL_3_A: u32 = 0x5f040;

/// `_PIPEDMC_EVT_CTL_3_B` (intel_dmc_regs.h)
pub const _PIPEDMC_EVT_CTL_3_B: u32 = 0x5f440;

/// `PIPEDMC_EVT_CTL_3(pipe)` (intel_dmc_regs.h)
pub const fn PIPEDMC_EVT_CTL_3(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPEDMC_EVT_CTL_3_A, _PIPEDMC_EVT_CTL_3_B)
}

/// `_PIPEDMC_DCB_FLIP_COUNT_A` (intel_dmc_regs.h)
pub const _PIPEDMC_DCB_FLIP_COUNT_A: u32 = 0x906a4;

/// `_PIPEDMC_DCB_FLIP_COUNT_B` (intel_dmc_regs.h)
pub const _PIPEDMC_DCB_FLIP_COUNT_B: u32 = 0x986a4;

/// `PIPEDMC_DCB_FLIP_COUNT(pipe)` (intel_dmc_regs.h)
pub const fn PIPEDMC_DCB_FLIP_COUNT(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPEDMC_DCB_FLIP_COUNT_A, _PIPEDMC_DCB_FLIP_COUNT_B)
}

/// `_PIPEDMC_DCB_BALANCE_RESET_A` (intel_dmc_regs.h)
pub const _PIPEDMC_DCB_BALANCE_RESET_A: u32 = 0x906a8;

/// `_PIPEDMC_DCB_BALANCE_RESET_B` (intel_dmc_regs.h)
pub const _PIPEDMC_DCB_BALANCE_RESET_B: u32 = 0x986a8;

/// `PIPEDMC_DCB_BALANCE_RESET(pipe)` (intel_dmc_regs.h)
pub const fn PIPEDMC_DCB_BALANCE_RESET(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPEDMC_DCB_BALANCE_RESET_A, _PIPEDMC_DCB_BALANCE_RESET_B)
}
