// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 display register definitions from `drivers/gpu/drm/i915/display/intel_display_reg_defs.h`.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]
// Macro bodies keep C's parenthesization; the redundant parens are expected.
#![allow(unused_parens, unused_braces)]



// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Display register helpers from `drivers/gpu/drm/i915/display/intel_display_reg_defs.h`
//! and the `include/drm/intel/{reg_bits,pick}.h` macros they rely on. The
//! `_PIPE`/`_PORT`/... wrappers below are generated from the header itself.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]

pub use crate::intel_gtt_api_upstream::{bit as reg_bit, genmask as reg_genmask, reg_field_prep};
pub use crate::intel_workarounds_types_upstream::I915RegT;
pub use crate::intel_display_core_types_upstream::IntelDisplay;

/// `_MMIO(r)`: wraps a register offset.
pub const fn mmio(reg: u32) -> I915RegT {
    I915RegT { reg }
}

/// `_MMIO(r)` alias for C macro name.
pub const fn _MMIO(reg: u32) -> I915RegT {
    mmio(reg)
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct IntelIrqRegs {
    pub imr: I915RegT,
    pub ier: I915RegT,
    pub iir: I915RegT,
}

pub const fn INTEL_IRQ_REGS(imr: I915RegT, ier: I915RegT, iir: I915RegT) -> IntelIrqRegs {
    IntelIrqRegs { imr, ier, iir }
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct IntelErrorRegs {
    pub emr: I915RegT,
    pub eir: I915RegT,
}

pub const fn INTEL_ERROR_REGS(emr: I915RegT, eir: I915RegT) -> IntelErrorRegs {
    IntelErrorRegs { emr, eir }
}

/// `INVALID_MMIO_REG` from i915_reg_defs.h.
pub const INVALID_MMIO_REG: I915RegT = mmio(0);

/// `REG_FIELD_GET(mask, val)`.
pub const fn reg_field_get(mask: u32, val: u32) -> u32 {
    (val & mask) >> mask.trailing_zeros()
}

/// `_PICK_EVEN(index, a, b)`; C unsigned wrap-around semantics.
pub const fn pick_even(index: u32, a: u32, b: u32) -> u32 {
    a.wrapping_add(index.wrapping_mul(b.wrapping_sub(a)))
}

/// `_PICK_EVEN(index, a, b)` alias for C macro name.
pub const fn _PICK_EVEN(index: u32, a: u32, b: u32) -> u32 {
    pick_even(index, a, b)
}

/// `_PICK_EVEN_2RANGES(index, c_index, a, b, c, d)`.
pub const fn pick_even_2ranges(index: u32, c_index: u32, a: u32, b: u32, c: u32, d: u32) -> u32 {
    if index < c_index {
        pick_even(index, a, b)
    } else {
        pick_even(index.wrapping_sub(c_index), c, d)
    }
}

/// `_PICK_EVEN_2RANGES(index, c_index, a, b, c, d)` alias for C macro name.
pub const fn _PICK_EVEN_2RANGES(index: u32, c_index: u32, a: u32, b: u32, c: u32, d: u32) -> u32 {
    pick_even_2ranges(index, c_index, a, b, c, d)
}

/// `_PICK(index, ...)`: indexed lookup into a constant list.
pub const fn pick(index: u32, values: &[u32]) -> u32 {
    values[index as usize]
}

/// `_PICK(index, ...)` alias for C macro name.
pub const fn _PICK(index: u32, values: &[u32]) -> u32 {
    pick(index, values)
}

/// `VLV_DISPLAY_BASE` (intel_display_reg_defs.h)
pub const VLV_DISPLAY_BASE: u32 = 0x180000;

/// `_PIPE(pipe, a, b)` (intel_display_reg_defs.h)
pub const fn _PIPE(pipe: u32, a: u32, b: u32) -> u32 {
    pick_even(pipe, a, b)
}

/// `_PLANE(plane, a, b)` (intel_display_reg_defs.h)
pub const fn _PLANE(plane: u32, a: u32, b: u32) -> u32 {
    pick_even(plane, a, b)
}

/// `_TRANS(tran, a, b)` (intel_display_reg_defs.h)
pub const fn _TRANS(tran: u32, a: u32, b: u32) -> u32 {
    pick_even(tran, a, b)
}

/// `_PORT(port, a, b)` (intel_display_reg_defs.h)
pub const fn _PORT(port: u32, a: u32, b: u32) -> u32 {
    pick_even(port, a, b)
}

/// `_PLL(pll, a, b)` (intel_display_reg_defs.h)
pub const fn _PLL(pll: u32, a: u32, b: u32) -> u32 {
    pick_even(pll, a, b)
}

/// `_PHY(phy, a, b)` (intel_display_reg_defs.h)
pub const fn _PHY(phy: u32, a: u32, b: u32) -> u32 {
    pick_even(phy, a, b)
}

/// `_MMIO_PIPE(pipe, a, b)` (intel_display_reg_defs.h)
pub const fn _MMIO_PIPE(pipe: u32, a: u32, b: u32) -> I915RegT {
    mmio(_PIPE(pipe, a, b))
}

/// `_MMIO_PLANE(plane, a, b)` (intel_display_reg_defs.h)
pub const fn _MMIO_PLANE(plane: u32, a: u32, b: u32) -> I915RegT {
    mmio(_PLANE(plane, a, b))
}

/// `_MMIO_TRANS(tran, a, b)` (intel_display_reg_defs.h)
pub const fn _MMIO_TRANS(tran: u32, a: u32, b: u32) -> I915RegT {
    mmio(_TRANS(tran, a, b))
}

/// `_MMIO_PORT(port, a, b)` (intel_display_reg_defs.h)
pub const fn _MMIO_PORT(port: u32, a: u32, b: u32) -> I915RegT {
    mmio(_PORT(port, a, b))
}

/// `_MMIO_PLL(pll, a, b)` (intel_display_reg_defs.h)
pub const fn _MMIO_PLL(pll: u32, a: u32, b: u32) -> I915RegT {
    mmio(_PLL(pll, a, b))
}

/// `_MMIO_PHY(phy, a, b)` (intel_display_reg_defs.h)
pub const fn _MMIO_PHY(phy: u32, a: u32, b: u32) -> I915RegT {
    mmio(_PHY(phy, a, b))
}

/// `_MMIO_BASE_PIPE3(base, pipe, a, b, c)` (intel_display_reg_defs.h)
pub const fn _MMIO_BASE_PIPE3(base: u32, pipe: u32, a: u32, b: u32, c: u32) -> I915RegT {
    mmio(((base) + pick_even_2ranges(pipe, 1, a, a, b, c)))
}

/// `_MMIO_BASE_PORT3(base, pipe, a, b, c)` (intel_display_reg_defs.h)
pub const fn _MMIO_BASE_PORT3(base: u32, pipe: u32, a: u32, b: u32, c: u32) -> I915RegT {
    mmio(((base) + pick_even_2ranges(pipe, 1, a, a, b, c)))
}

use crate::intel_display_device_types_upstream::{IntelDisplayDeviceInfo, IntelDisplayRuntimeInfo};
use crate::intel_display_limits_types_upstream::{PIPE_A, TRANSCODER_A};

#[inline]
pub unsafe fn DISPLAY_INFO(display: *const IntelDisplay) -> *mut IntelDisplayDeviceInfo {
    unsafe { (*display).info.__device_info }
}

#[inline]
pub unsafe fn DISPLAY_RUNTIME_INFO(display: *const IntelDisplay) -> *const IntelDisplayRuntimeInfo {
    unsafe { core::ptr::addr_of!((*display).info.__runtime_info) }
}

#[inline]
pub unsafe fn DISPLAY_VER(display: *const IntelDisplay) -> u16 {
    unsafe { (*display).info.__runtime_info.ip.ver }
}

#[inline]
pub unsafe fn DISPLAY_MMIO_BASE(display: *const IntelDisplay) -> u32 {
    let info = unsafe { DISPLAY_INFO(display) };
    unsafe { (*info).mmio_offset }
}

#[inline]
pub unsafe fn INTEL_DISPLAY_DEVICE_PIPE_OFFSET(display: *const IntelDisplay, pipe: u32) -> u32 {
    let info = unsafe { DISPLAY_INFO(display) };
    unsafe {
        (*info).pipe_offsets[pipe as usize]
            .wrapping_sub((*info).pipe_offsets[PIPE_A as usize])
            .wrapping_add((*info).mmio_offset)
    }
}

#[inline]
pub unsafe fn INTEL_DISPLAY_DEVICE_TRANS_OFFSET(display: *const IntelDisplay, trans: u32) -> u32 {
    let info = unsafe { DISPLAY_INFO(display) };
    unsafe {
        (*info).trans_offsets[trans as usize]
            .wrapping_sub((*info).trans_offsets[TRANSCODER_A as usize])
            .wrapping_add((*info).mmio_offset)
    }
}

#[inline]
pub unsafe fn INTEL_DISPLAY_DEVICE_CURSOR_OFFSET(display: *const IntelDisplay, pipe: u32) -> u32 {
    let info = unsafe { DISPLAY_INFO(display) };
    unsafe {
        (*info).cursor_offsets[pipe as usize]
            .wrapping_sub((*info).cursor_offsets[PIPE_A as usize])
            .wrapping_add((*info).mmio_offset)
    }
}

/// `_MMIO_PIPE2(display, pipe, reg)` (intel_display_reg_defs.h)
#[inline]
pub unsafe fn _MMIO_PIPE2(display: *const IntelDisplay, pipe: u32, reg: u32) -> I915RegT {
    mmio(unsafe { INTEL_DISPLAY_DEVICE_PIPE_OFFSET(display, pipe) }.wrapping_add(reg))
}

/// `_MMIO_TRANS2(display, trans, reg)` (intel_display_reg_defs.h)
#[inline]
pub unsafe fn _MMIO_TRANS2(display: *const IntelDisplay, trans: u32, reg: u32) -> I915RegT {
    mmio(unsafe { INTEL_DISPLAY_DEVICE_TRANS_OFFSET(display, trans) }.wrapping_add(reg))
}

/// `_MMIO_CURSOR2(display, pipe, reg)` (intel_display_reg_defs.h)
#[inline]
pub unsafe fn _MMIO_CURSOR2(display: *const IntelDisplay, pipe: u32, reg: u32) -> I915RegT {
    mmio(unsafe { INTEL_DISPLAY_DEVICE_CURSOR_OFFSET(display, pipe) }.wrapping_add(reg))
}
