// SPDX-License-Identifier: MIT
// Copyright © 2014-2018 Intel Corporation.
//
//! Constants and macro-shaped Rust equivalents from Linux v7.2.3
//! `drivers/gpu/drm/i915/gt/intel_lrc_reg.h`.
//!
//! This file is intentionally separate from `intel_lrc_types_upstream.rs`;
//! the source header owns register-state indexes, CSB fields, and the PDP
//! assignment macros represented below.

pub const CTX_DESC_FORCE_RESTORE: u64 = 1u64 << 2;

// GEN8 to GEN12 Reg State Context dword indexes.
pub const CTX_CONTEXT_CONTROL: usize = 0x02 + 1;
pub const CTX_RING_HEAD: usize = 0x04 + 1;
pub const CTX_RING_TAIL: usize = 0x06 + 1;
pub const CTX_RING_START: usize = 0x08 + 1;
pub const CTX_RING_CTL: usize = 0x0a + 1;
pub const CTX_BB_STATE: usize = 0x10 + 1;
pub const CTX_TIMESTAMP: usize = 0x22 + 1;
pub const CTX_PDP3_UDW: usize = 0x24 + 1;
pub const CTX_PDP3_LDW: usize = 0x26 + 1;
pub const CTX_PDP2_UDW: usize = 0x28 + 1;
pub const CTX_PDP2_LDW: usize = 0x2a + 1;
pub const CTX_PDP1_UDW: usize = 0x2c + 1;
pub const CTX_PDP1_LDW: usize = 0x2e + 1;
pub const CTX_PDP0_UDW: usize = 0x30 + 1;
pub const CTX_PDP0_LDW: usize = 0x32 + 1;
pub const CTX_R_PWR_CLK_STATE: usize = 0x42 + 1;

pub const GEN9_CTX_RING_MI_MODE: u32 = 0x54;

// ASSIGN_CTX_PDP uses a literal PDP number just like the source token-pasting
// macro. `reg_state` and `ppgtt` are each evaluated once, in source order.
#[macro_export]
macro_rules! ASSIGN_CTX_PDP {
    ($ppgtt:expr, $reg_state:expr,0) => {{
        let reg_state__ = $reg_state;
        let addr__: u64 = i915_page_dir_dma_addr($ppgtt, 0);
        *reg_state__.add($crate::intel_lrc_reg_types_upstream::CTX_PDP0_UDW) =
            $crate::linux::bits::upper_32_bits(addr__);
        *reg_state__.add($crate::intel_lrc_reg_types_upstream::CTX_PDP0_LDW) =
            $crate::linux::bits::lower_32_bits(addr__);
    }};
    ($ppgtt:expr, $reg_state:expr,1) => {{
        let reg_state__ = $reg_state;
        let addr__: u64 = i915_page_dir_dma_addr($ppgtt, 1);
        *reg_state__.add($crate::intel_lrc_reg_types_upstream::CTX_PDP1_UDW) =
            $crate::linux::bits::upper_32_bits(addr__);
        *reg_state__.add($crate::intel_lrc_reg_types_upstream::CTX_PDP1_LDW) =
            $crate::linux::bits::lower_32_bits(addr__);
    }};
    ($ppgtt:expr, $reg_state:expr,2) => {{
        let reg_state__ = $reg_state;
        let addr__: u64 = i915_page_dir_dma_addr($ppgtt, 2);
        *reg_state__.add($crate::intel_lrc_reg_types_upstream::CTX_PDP2_UDW) =
            $crate::linux::bits::upper_32_bits(addr__);
        *reg_state__.add($crate::intel_lrc_reg_types_upstream::CTX_PDP2_LDW) =
            $crate::linux::bits::lower_32_bits(addr__);
    }};
    ($ppgtt:expr, $reg_state:expr,3) => {{
        let reg_state__ = $reg_state;
        let addr__: u64 = i915_page_dir_dma_addr($ppgtt, 3);
        *reg_state__.add($crate::intel_lrc_reg_types_upstream::CTX_PDP3_UDW) =
            $crate::linux::bits::upper_32_bits(addr__);
        *reg_state__.add($crate::intel_lrc_reg_types_upstream::CTX_PDP3_LDW) =
            $crate::linux::bits::lower_32_bits(addr__);
    }};
}

// ASSIGN_CTX_PML4 writes the root page directory address to PDP0, matching the
// C macro's reg_state initialization before px_dma(ppgtt->pd).
#[macro_export]
macro_rules! ASSIGN_CTX_PML4 {
    ($ppgtt:expr, $reg_state:expr) => {{
        let reg_state__ = $reg_state;
        let ppgtt__ = $ppgtt;
        let addr__: u64 = px_dma((*ppgtt__).pd);
        *reg_state__.add($crate::intel_lrc_reg_types_upstream::CTX_PDP0_UDW) =
            $crate::linux::bits::upper_32_bits(addr__);
        *reg_state__.add($crate::intel_lrc_reg_types_upstream::CTX_PDP0_LDW) =
            $crate::linux::bits::lower_32_bits(addr__);
    }};
}

pub const GEN8_CTX_RCS_INDIRECT_CTX_OFFSET_DEFAULT: u32 = 0x17;
pub const GEN9_CTX_RCS_INDIRECT_CTX_OFFSET_DEFAULT: u32 = 0x26;
pub const GEN10_CTX_RCS_INDIRECT_CTX_OFFSET_DEFAULT: u32 = 0x19;
pub const GEN11_CTX_RCS_INDIRECT_CTX_OFFSET_DEFAULT: u32 = 0x1a;
pub const GEN12_CTX_RCS_INDIRECT_CTX_OFFSET_DEFAULT: u32 = 0x0d;

pub const GEN8_EXECLISTS_STATUS_BUF: u32 = 0x370;
pub const GEN11_EXECLISTS_STATUS_BUF2: u32 = 0x3c0;

// Although CSB pointers expose three bits, values 6 and 7 are reserved.
pub const GEN8_CSB_ENTRIES: u32 = 6;
pub const GEN8_CSB_PTR_MASK: u32 = 0x7;
pub const GEN8_CSB_READ_PTR_MASK: u32 = GEN8_CSB_PTR_MASK << 8;
pub const GEN8_CSB_WRITE_PTR_MASK: u32 = GEN8_CSB_PTR_MASK << 0;

pub const GEN11_CSB_ENTRIES: u32 = 12;
pub const GEN11_CSB_PTR_MASK: u32 = 0xf;
pub const GEN11_CSB_READ_PTR_MASK: u32 = GEN11_CSB_PTR_MASK << 8;
pub const GEN11_CSB_WRITE_PTR_MASK: u32 = GEN11_CSB_PTR_MASK << 0;

pub const MAX_CONTEXT_HW_ID: u32 = 1 << 21; // exclusive
pub const GEN11_MAX_CONTEXT_HW_ID: u32 = 1 << 11; // exclusive
// Gen12 reserves ID 0x7ff for idle.
pub const GEN12_MAX_CONTEXT_HW_ID: u32 = GEN11_MAX_CONTEXT_HW_ID - 1;
// Xe_HP reserves ID 0xffff for an invalid context.
pub const XEHP_MAX_CONTEXT_HW_ID: u32 = 0xffff;
