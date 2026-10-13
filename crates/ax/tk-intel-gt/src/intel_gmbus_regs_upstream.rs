// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 display register definitions from `drivers/gpu/drm/i915/display/intel_gmbus_regs.h`.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]
// Macro bodies keep C's parenthesization; the redundant parens are expected.
#![allow(unused_parens, unused_braces)]

use crate::intel_workarounds_types_upstream::I915RegT;
use crate::intel_display_reg_defs_upstream::*;

/// `GPIO_CLOCK_DIR_MASK` (intel_gmbus_regs.h)
pub const GPIO_CLOCK_DIR_MASK: u32 = ((1 << 0));

/// `GPIO_CLOCK_DIR_IN` (intel_gmbus_regs.h)
pub const GPIO_CLOCK_DIR_IN: u32 = ((0 << 1));

/// `GPIO_CLOCK_DIR_OUT` (intel_gmbus_regs.h)
pub const GPIO_CLOCK_DIR_OUT: u32 = ((1 << 1));

/// `GPIO_CLOCK_VAL_MASK` (intel_gmbus_regs.h)
pub const GPIO_CLOCK_VAL_MASK: u32 = ((1 << 2));

/// `GPIO_CLOCK_VAL_OUT` (intel_gmbus_regs.h)
pub const GPIO_CLOCK_VAL_OUT: u32 = ((1 << 3));

/// `GPIO_CLOCK_VAL_IN` (intel_gmbus_regs.h)
pub const GPIO_CLOCK_VAL_IN: u32 = ((1 << 4));

/// `GPIO_CLOCK_PULLUP_DISABLE` (intel_gmbus_regs.h)
pub const GPIO_CLOCK_PULLUP_DISABLE: u32 = ((1 << 5));

/// `GPIO_DATA_DIR_MASK` (intel_gmbus_regs.h)
pub const GPIO_DATA_DIR_MASK: u32 = ((1 << 8));

/// `GPIO_DATA_DIR_IN` (intel_gmbus_regs.h)
pub const GPIO_DATA_DIR_IN: u32 = ((0 << 9));

/// `GPIO_DATA_DIR_OUT` (intel_gmbus_regs.h)
pub const GPIO_DATA_DIR_OUT: u32 = ((1 << 9));

/// `GPIO_DATA_VAL_MASK` (intel_gmbus_regs.h)
pub const GPIO_DATA_VAL_MASK: u32 = ((1 << 10));

/// `GPIO_DATA_VAL_OUT` (intel_gmbus_regs.h)
pub const GPIO_DATA_VAL_OUT: u32 = ((1 << 11));

/// `GPIO_DATA_VAL_IN` (intel_gmbus_regs.h)
pub const GPIO_DATA_VAL_IN: u32 = ((1 << 12));

/// `GPIO_DATA_PULLUP_DISABLE` (intel_gmbus_regs.h)
pub const GPIO_DATA_PULLUP_DISABLE: u32 = ((1 << 13));

/// `GMBUS_AKSV_SELECT` (intel_gmbus_regs.h)
pub const GMBUS_AKSV_SELECT: u32 = ((1 << 11));

/// `GMBUS_RATE_100KHZ` (intel_gmbus_regs.h)
pub const GMBUS_RATE_100KHZ: u32 = ((0 << 8));

/// `GMBUS_RATE_50KHZ` (intel_gmbus_regs.h)
pub const GMBUS_RATE_50KHZ: u32 = ((1 << 8));

/// `GMBUS_RATE_400KHZ` (intel_gmbus_regs.h)
pub const GMBUS_RATE_400KHZ: u32 = ((2 << 8));

/// `GMBUS_RATE_1MHZ` (intel_gmbus_regs.h)
pub const GMBUS_RATE_1MHZ: u32 = ((3 << 8));

/// `GMBUS_HOLD_EXT` (intel_gmbus_regs.h)
pub const GMBUS_HOLD_EXT: u32 = ((1 << 7));

/// `GMBUS_BYTE_CNT_OVERRIDE` (intel_gmbus_regs.h)
pub const GMBUS_BYTE_CNT_OVERRIDE: u32 = ((1 << 6));

/// `GMBUS_SW_CLR_INT` (intel_gmbus_regs.h)
pub const GMBUS_SW_CLR_INT: u32 = ((1 << 31));

/// `GMBUS_SW_RDY` (intel_gmbus_regs.h)
pub const GMBUS_SW_RDY: u32 = ((1 << 30));

/// `GMBUS_ENT` (intel_gmbus_regs.h)
pub const GMBUS_ENT: u32 = ((1 << 29));

/// `GMBUS_CYCLE_NONE` (intel_gmbus_regs.h)
pub const GMBUS_CYCLE_NONE: u32 = ((0 << 25));

/// `GMBUS_CYCLE_WAIT` (intel_gmbus_regs.h)
pub const GMBUS_CYCLE_WAIT: u32 = ((1 << 25));

/// `GMBUS_CYCLE_INDEX` (intel_gmbus_regs.h)
pub const GMBUS_CYCLE_INDEX: u32 = ((2 << 25));

/// `GMBUS_CYCLE_STOP` (intel_gmbus_regs.h)
pub const GMBUS_CYCLE_STOP: u32 = ((4 << 25));

/// `GMBUS_BYTE_COUNT_SHIFT` (intel_gmbus_regs.h)
pub const GMBUS_BYTE_COUNT_SHIFT: u32 = 16;

/// `GMBUS_BYTE_COUNT_MAX` (intel_gmbus_regs.h)
pub const GMBUS_BYTE_COUNT_MAX: u32 = 256;

/// `GEN9_GMBUS_BYTE_COUNT_MAX` (intel_gmbus_regs.h)
pub const GEN9_GMBUS_BYTE_COUNT_MAX: u32 = 511;

/// `GMBUS_SLAVE_INDEX_SHIFT` (intel_gmbus_regs.h)
pub const GMBUS_SLAVE_INDEX_SHIFT: u32 = 8;

/// `GMBUS_SLAVE_ADDR_SHIFT` (intel_gmbus_regs.h)
pub const GMBUS_SLAVE_ADDR_SHIFT: u32 = 1;

/// `GMBUS_SLAVE_READ` (intel_gmbus_regs.h)
pub const GMBUS_SLAVE_READ: u32 = ((1 << 0));

/// `GMBUS_SLAVE_WRITE` (intel_gmbus_regs.h)
pub const GMBUS_SLAVE_WRITE: u32 = ((0 << 0));

/// `GMBUS_INUSE` (intel_gmbus_regs.h)
pub const GMBUS_INUSE: u32 = ((1 << 15));

/// `GMBUS_HW_WAIT_PHASE` (intel_gmbus_regs.h)
pub const GMBUS_HW_WAIT_PHASE: u32 = ((1 << 14));

/// `GMBUS_STALL_TIMEOUT` (intel_gmbus_regs.h)
pub const GMBUS_STALL_TIMEOUT: u32 = ((1 << 13));

/// `GMBUS_INT` (intel_gmbus_regs.h)
pub const GMBUS_INT: u32 = ((1 << 12));

/// `GMBUS_HW_RDY` (intel_gmbus_regs.h)
pub const GMBUS_HW_RDY: u32 = ((1 << 11));

/// `GMBUS_SATOER` (intel_gmbus_regs.h)
pub const GMBUS_SATOER: u32 = ((1 << 10));

/// `GMBUS_ACTIVE` (intel_gmbus_regs.h)
pub const GMBUS_ACTIVE: u32 = ((1 << 9));

/// `GMBUS_SLAVE_TIMEOUT_EN` (intel_gmbus_regs.h)
pub const GMBUS_SLAVE_TIMEOUT_EN: u32 = ((1 << 4));

/// `GMBUS_NAK_EN` (intel_gmbus_regs.h)
pub const GMBUS_NAK_EN: u32 = ((1 << 3));

/// `GMBUS_IDLE_EN` (intel_gmbus_regs.h)
pub const GMBUS_IDLE_EN: u32 = ((1 << 2));

/// `GMBUS_HW_WAIT_EN` (intel_gmbus_regs.h)
pub const GMBUS_HW_WAIT_EN: u32 = ((1 << 1));

/// `GMBUS_HW_RDY_EN` (intel_gmbus_regs.h)
pub const GMBUS_HW_RDY_EN: u32 = ((1 << 0));

/// `GMBUS_2BYTE_INDEX_EN` (intel_gmbus_regs.h)
pub const GMBUS_2BYTE_INDEX_EN: u32 = ((1 << 31));

pub const PCH_DISPLAY_BASE: u32 = 0xc0000;

pub unsafe fn HAS_PCH_SPLIT(display: *const IntelDisplay) -> bool {
    (*display).pch_type != 0
}

pub unsafe fn __GMBUS_MMIO_BASE(display: *const IntelDisplay) -> u32 {
    if unsafe { DISPLAY_VER(display) } >= 5 {
        if unsafe { HAS_PCH_SPLIT(display) } {
            PCH_DISPLAY_BASE
        } else {
            0
        }
    } else {
        unsafe { DISPLAY_MMIO_BASE(display) }
    }
}

/// `GPIO(__display, gpio)` (intel_gmbus_regs.h)
pub unsafe fn GPIO(display: *const IntelDisplay, gpio: u32) -> I915RegT {
    mmio(unsafe { __GMBUS_MMIO_BASE(display) } + 0x5010 + 4 * gpio)
}

/// `GMBUS0(__display)` (intel_gmbus_regs.h)
pub unsafe fn GMBUS0(display: *const IntelDisplay) -> I915RegT {
    mmio(unsafe { __GMBUS_MMIO_BASE(display) } + 0x5100)
}

/// `GMBUS1(__display)` (intel_gmbus_regs.h)
pub unsafe fn GMBUS1(display: *const IntelDisplay) -> I915RegT {
    mmio(unsafe { __GMBUS_MMIO_BASE(display) } + 0x5104)
}

/// `GMBUS2(__display)` (intel_gmbus_regs.h)
pub unsafe fn GMBUS2(display: *const IntelDisplay) -> I915RegT {
    mmio(unsafe { __GMBUS_MMIO_BASE(display) } + 0x5108)
}

/// `GMBUS3(__display)` (intel_gmbus_regs.h)
pub unsafe fn GMBUS3(display: *const IntelDisplay) -> I915RegT {
    mmio(unsafe { __GMBUS_MMIO_BASE(display) } + 0x510c)
}

/// `GMBUS4(__display)` (intel_gmbus_regs.h)
pub unsafe fn GMBUS4(display: *const IntelDisplay) -> I915RegT {
    mmio(unsafe { __GMBUS_MMIO_BASE(display) } + 0x5110)
}

/// `GMBUS5(__display)` (intel_gmbus_regs.h)
pub unsafe fn GMBUS5(display: *const IntelDisplay) -> I915RegT {
    mmio(unsafe { __GMBUS_MMIO_BASE(display) } + 0x5120)
}
