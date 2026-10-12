// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! i915 display type definitions. Source headers:
//! `drivers/gpu/drm/i915/display/intel_pch.h`
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};
use core::mem::{align_of, size_of, ManuallyDrop};


pub type IntelPch = i32;
pub const PCH_NOP: IntelPch = -1;
pub const PCH_NONE: IntelPch = 0;
pub const PCH_IBX: IntelPch = 1;
pub const PCH_CPT: IntelPch = 2;
pub const PCH_LPT_H: IntelPch = 3;
pub const PCH_LPT_LP: IntelPch = 4;
pub const PCH_SPT: IntelPch = 5;
pub const PCH_CNP: IntelPch = 6;
pub const PCH_ICP: IntelPch = 7;
pub const PCH_TGP: IntelPch = 8;
pub const PCH_ADP: IntelPch = 9;
pub const PCH_DG1: IntelPch = 1024;
pub const PCH_DG2: IntelPch = 1025;
pub const PCH_MTL: IntelPch = 1026;
pub const PCH_LNL: IntelPch = 1027;


