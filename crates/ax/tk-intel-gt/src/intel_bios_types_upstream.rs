// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! i915 display type definitions. Source headers:
//! `drivers/gpu/drm/i915/display/intel_bios.h`
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};
use core::mem::{align_of, size_of, ManuallyDrop};


pub type IntelBacklightType = u32;
pub const INTEL_BACKLIGHT_PMIC: IntelBacklightType = 0;
pub const INTEL_BACKLIGHT_LPSS: IntelBacklightType = 1;
pub const INTEL_BACKLIGHT_DISPLAY_DDI: IntelBacklightType = 2;
pub const INTEL_BACKLIGHT_DSI_DCS: IntelBacklightType = 3;
pub const INTEL_BACKLIGHT_PANEL_DRIVER_INTERFACE: IntelBacklightType = 4;
pub const INTEL_BACKLIGHT_VESA_EDP_AUX_INTERFACE: IntelBacklightType = 5;


