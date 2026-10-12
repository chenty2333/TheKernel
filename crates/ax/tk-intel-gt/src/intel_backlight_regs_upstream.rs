// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 display register definitions from `drivers/gpu/drm/i915/display/intel_backlight_regs.h`.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]
// Macro bodies keep C's parenthesization; the redundant parens are expected.
#![allow(unused_parens, unused_braces)]

use crate::intel_workarounds_types_upstream::I915RegT;
use crate::intel_display_reg_defs_upstream::*;

/// `_VLV_BLC_PWM_CTL2_A` (intel_backlight_regs.h)
pub const _VLV_BLC_PWM_CTL2_A: u32 = ((VLV_DISPLAY_BASE + 0x61250));

/// `_VLV_BLC_PWM_CTL2_B` (intel_backlight_regs.h)
pub const _VLV_BLC_PWM_CTL2_B: u32 = ((VLV_DISPLAY_BASE + 0x61350));

/// `VLV_BLC_PWM_CTL2(pipe)` (intel_backlight_regs.h)
pub const fn VLV_BLC_PWM_CTL2(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _VLV_BLC_PWM_CTL2_A, _VLV_BLC_PWM_CTL2_B)
}

/// `_VLV_BLC_PWM_CTL_A` (intel_backlight_regs.h)
pub const _VLV_BLC_PWM_CTL_A: u32 = ((VLV_DISPLAY_BASE + 0x61254));

/// `_VLV_BLC_PWM_CTL_B` (intel_backlight_regs.h)
pub const _VLV_BLC_PWM_CTL_B: u32 = ((VLV_DISPLAY_BASE + 0x61354));

/// `VLV_BLC_PWM_CTL(pipe)` (intel_backlight_regs.h)
pub const fn VLV_BLC_PWM_CTL(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _VLV_BLC_PWM_CTL_A, _VLV_BLC_PWM_CTL_B)
}

/// `_VLV_BLC_HIST_CTL_A` (intel_backlight_regs.h)
pub const _VLV_BLC_HIST_CTL_A: u32 = ((VLV_DISPLAY_BASE + 0x61260));

/// `_VLV_BLC_HIST_CTL_B` (intel_backlight_regs.h)
pub const _VLV_BLC_HIST_CTL_B: u32 = ((VLV_DISPLAY_BASE + 0x61360));

/// `VLV_BLC_HIST_CTL(pipe)` (intel_backlight_regs.h)
pub const fn VLV_BLC_HIST_CTL(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _VLV_BLC_HIST_CTL_A, _VLV_BLC_HIST_CTL_B)
}

/// `BLC_PWM_CTL2` (intel_backlight_regs.h)
pub const BLC_PWM_CTL2: I915RegT = mmio(0x61250);

/// `BLM_PWM_ENABLE` (intel_backlight_regs.h)
pub const BLM_PWM_ENABLE: u32 = ((1 << 31));

/// `BLM_COMBINATION_MODE` (intel_backlight_regs.h)
pub const BLM_COMBINATION_MODE: u32 = ((1 << 30));

/// `BLM_PIPE_SELECT` (intel_backlight_regs.h)
pub const BLM_PIPE_SELECT: u32 = ((1 << 29));

/// `BLM_PIPE_SELECT_IVB` (intel_backlight_regs.h)
pub const BLM_PIPE_SELECT_IVB: u32 = ((3 << 29));

/// `BLM_PIPE_A` (intel_backlight_regs.h)
pub const BLM_PIPE_A: u32 = ((0 << 29));

/// `BLM_PIPE_B` (intel_backlight_regs.h)
pub const BLM_PIPE_B: u32 = ((1 << 29));

/// `BLM_PIPE_C` (intel_backlight_regs.h)
pub const BLM_PIPE_C: u32 = ((2 << 29));

/// `BLM_TRANSCODER_A` (intel_backlight_regs.h)
pub const BLM_TRANSCODER_A: u32 = BLM_PIPE_A;

/// `BLM_TRANSCODER_B` (intel_backlight_regs.h)
pub const BLM_TRANSCODER_B: u32 = BLM_PIPE_B;

/// `BLM_TRANSCODER_C` (intel_backlight_regs.h)
pub const BLM_TRANSCODER_C: u32 = BLM_PIPE_C;

/// `BLM_TRANSCODER_EDP` (intel_backlight_regs.h)
pub const BLM_TRANSCODER_EDP: u32 = ((3 << 29));

/// `BLM_PIPE(pipe)` (intel_backlight_regs.h)
pub const fn BLM_PIPE(pipe: u32) -> u32 {
    (((pipe) << 29))
}

/// `BLM_POLARITY_I965` (intel_backlight_regs.h)
pub const BLM_POLARITY_I965: u32 = ((1 << 28));

/// `BLM_PHASE_IN_INTERUPT_STATUS` (intel_backlight_regs.h)
pub const BLM_PHASE_IN_INTERUPT_STATUS: u32 = ((1 << 26));

/// `BLM_PHASE_IN_ENABLE` (intel_backlight_regs.h)
pub const BLM_PHASE_IN_ENABLE: u32 = ((1 << 25));

/// `BLM_PHASE_IN_INTERUPT_ENABL` (intel_backlight_regs.h)
pub const BLM_PHASE_IN_INTERUPT_ENABL: u32 = ((1 << 24));

/// `BLM_PHASE_IN_TIME_BASE_SHIFT` (intel_backlight_regs.h)
pub const BLM_PHASE_IN_TIME_BASE_SHIFT: u32 = (16);

/// `BLM_PHASE_IN_TIME_BASE_MASK` (intel_backlight_regs.h)
pub const BLM_PHASE_IN_TIME_BASE_MASK: u32 = ((0xff << 16));

/// `BLM_PHASE_IN_COUNT_SHIFT` (intel_backlight_regs.h)
pub const BLM_PHASE_IN_COUNT_SHIFT: u32 = (8);

/// `BLM_PHASE_IN_COUNT_MASK` (intel_backlight_regs.h)
pub const BLM_PHASE_IN_COUNT_MASK: u32 = ((0xff << 8));

/// `BLM_PHASE_IN_INCR_SHIFT` (intel_backlight_regs.h)
pub const BLM_PHASE_IN_INCR_SHIFT: u32 = (0);

/// `BLM_PHASE_IN_INCR_MASK` (intel_backlight_regs.h)
pub const BLM_PHASE_IN_INCR_MASK: u32 = ((0xff << 0));

/// `BLC_PWM_CTL` (intel_backlight_regs.h)
pub const BLC_PWM_CTL: I915RegT = mmio(0x61254);

/// `BACKLIGHT_MODULATION_FREQ_SHIFT` (intel_backlight_regs.h)
pub const BACKLIGHT_MODULATION_FREQ_SHIFT: u32 = (17);

/// `BACKLIGHT_MODULATION_FREQ_MASK` (intel_backlight_regs.h)
pub const BACKLIGHT_MODULATION_FREQ_MASK: u32 = ((0x7fff << 17));

/// `BLM_LEGACY_MODE` (intel_backlight_regs.h)
pub const BLM_LEGACY_MODE: u32 = ((1 << 16));

/// `BACKLIGHT_DUTY_CYCLE_SHIFT` (intel_backlight_regs.h)
pub const BACKLIGHT_DUTY_CYCLE_SHIFT: u32 = (0);

/// `BACKLIGHT_DUTY_CYCLE_MASK` (intel_backlight_regs.h)
pub const BACKLIGHT_DUTY_CYCLE_MASK: u32 = (0xffff);

/// `BACKLIGHT_DUTY_CYCLE_MASK_PNV` (intel_backlight_regs.h)
pub const BACKLIGHT_DUTY_CYCLE_MASK_PNV: u32 = (0xfffe);

/// `BLM_POLARITY_PNV` (intel_backlight_regs.h)
pub const BLM_POLARITY_PNV: u32 = ((1 << 0));

/// `BLC_HIST_CTL` (intel_backlight_regs.h)
pub const BLC_HIST_CTL: I915RegT = mmio(0x61260);

/// `BLM_HISTOGRAM_ENABLE` (intel_backlight_regs.h)
pub const BLM_HISTOGRAM_ENABLE: u32 = ((1 << 31));

/// `BLC_PWM_CPU_CTL2` (intel_backlight_regs.h)
pub const BLC_PWM_CPU_CTL2: I915RegT = mmio(0x48250);

/// `BLC_PWM_CPU_CTL` (intel_backlight_regs.h)
pub const BLC_PWM_CPU_CTL: I915RegT = mmio(0x48254);

/// `HSW_BLC_PWM2_CTL` (intel_backlight_regs.h)
pub const HSW_BLC_PWM2_CTL: I915RegT = mmio(0x48350);

/// `BLC_PWM_PCH_CTL1` (intel_backlight_regs.h)
pub const BLC_PWM_PCH_CTL1: I915RegT = mmio(0xc8250);

/// `BLM_PCH_PWM_ENABLE` (intel_backlight_regs.h)
pub const BLM_PCH_PWM_ENABLE: u32 = ((1 << 31));

/// `BLM_PCH_OVERRIDE_ENABLE` (intel_backlight_regs.h)
pub const BLM_PCH_OVERRIDE_ENABLE: u32 = ((1 << 30));

/// `BLM_PCH_POLARITY` (intel_backlight_regs.h)
pub const BLM_PCH_POLARITY: u32 = ((1 << 29));

/// `BLC_PWM_PCH_CTL2` (intel_backlight_regs.h)
pub const BLC_PWM_PCH_CTL2: I915RegT = mmio(0xc8254);

/// `_BXT_BLC_PWM_CTL1` (intel_backlight_regs.h)
pub const _BXT_BLC_PWM_CTL1: u32 = 0xC8250;

/// `BXT_BLC_PWM_ENABLE` (intel_backlight_regs.h)
pub const BXT_BLC_PWM_ENABLE: u32 = ((1 << 31));

/// `BXT_BLC_PWM_POLARITY` (intel_backlight_regs.h)
pub const BXT_BLC_PWM_POLARITY: u32 = ((1 << 29));

/// `_BXT_BLC_PWM_FREQ1` (intel_backlight_regs.h)
pub const _BXT_BLC_PWM_FREQ1: u32 = 0xC8254;

/// `_BXT_BLC_PWM_DUTY1` (intel_backlight_regs.h)
pub const _BXT_BLC_PWM_DUTY1: u32 = 0xC8258;

/// `_BXT_BLC_PWM_CTL2` (intel_backlight_regs.h)
pub const _BXT_BLC_PWM_CTL2: u32 = 0xC8350;

/// `_BXT_BLC_PWM_FREQ2` (intel_backlight_regs.h)
pub const _BXT_BLC_PWM_FREQ2: u32 = 0xC8354;

/// `_BXT_BLC_PWM_DUTY2` (intel_backlight_regs.h)
pub const _BXT_BLC_PWM_DUTY2: u32 = 0xC8358;

/// `BXT_BLC_PWM_CTL(controller)` (intel_backlight_regs.h)
pub const fn BXT_BLC_PWM_CTL(controller: u32) -> I915RegT {
    _MMIO_PIPE(controller, _BXT_BLC_PWM_CTL1, _BXT_BLC_PWM_CTL2)
}

/// `BXT_BLC_PWM_FREQ(controller)` (intel_backlight_regs.h)
pub const fn BXT_BLC_PWM_FREQ(controller: u32) -> I915RegT {
    _MMIO_PIPE(controller, _BXT_BLC_PWM_FREQ1, _BXT_BLC_PWM_FREQ2)
}

/// `BXT_BLC_PWM_DUTY(controller)` (intel_backlight_regs.h)
pub const fn BXT_BLC_PWM_DUTY(controller: u32) -> I915RegT {
    _MMIO_PIPE(controller, _BXT_BLC_PWM_DUTY1, _BXT_BLC_PWM_DUTY2)
}

/// `UTIL_PIN_CTL` (intel_backlight_regs.h)
pub const UTIL_PIN_CTL: I915RegT = mmio(0x48400);

/// `UTIL_PIN_ENABLE` (intel_backlight_regs.h)
pub const UTIL_PIN_ENABLE: u32 = ((1 << 31));

/// `UTIL_PIN_PIPE_MASK` (intel_backlight_regs.h)
pub const UTIL_PIN_PIPE_MASK: u32 = ((3 << 29));

/// `UTIL_PIN_PIPE(x)` (intel_backlight_regs.h)
pub const fn UTIL_PIN_PIPE(x: u32) -> u32 {
    (((x) << 29))
}

/// `UTIL_PIN_MODE_MASK` (intel_backlight_regs.h)
pub const UTIL_PIN_MODE_MASK: u32 = ((0xf << 24));

/// `UTIL_PIN_MODE_DATA` (intel_backlight_regs.h)
pub const UTIL_PIN_MODE_DATA: u32 = ((0 << 24));

/// `UTIL_PIN_MODE_PWM` (intel_backlight_regs.h)
pub const UTIL_PIN_MODE_PWM: u32 = ((1 << 24));

/// `UTIL_PIN_MODE_VBLANK` (intel_backlight_regs.h)
pub const UTIL_PIN_MODE_VBLANK: u32 = ((4 << 24));

/// `UTIL_PIN_MODE_VSYNC` (intel_backlight_regs.h)
pub const UTIL_PIN_MODE_VSYNC: u32 = ((5 << 24));

/// `UTIL_PIN_MODE_EYE_LEVEL` (intel_backlight_regs.h)
pub const UTIL_PIN_MODE_EYE_LEVEL: u32 = ((8 << 24));

/// `UTIL_PIN_OUTPUT_DATA` (intel_backlight_regs.h)
pub const UTIL_PIN_OUTPUT_DATA: u32 = ((1 << 23));

/// `UTIL_PIN_POLARITY` (intel_backlight_regs.h)
pub const UTIL_PIN_POLARITY: u32 = ((1 << 22));

/// `UTIL_PIN_DIRECTION_INPUT` (intel_backlight_regs.h)
pub const UTIL_PIN_DIRECTION_INPUT: u32 = ((1 << 19));

/// `UTIL_PIN_INPUT_DATA` (intel_backlight_regs.h)
pub const UTIL_PIN_INPUT_DATA: u32 = ((1 << 16));
