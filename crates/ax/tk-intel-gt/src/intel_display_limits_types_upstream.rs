// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! i915 display type definitions. Source headers:
//! `drivers/gpu/drm/i915/display/intel_display_limits.h`
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};
use core::mem::{align_of, size_of, ManuallyDrop};


pub type Port = i32;
pub const PORT_NONE: Port = -1;
pub const PORT_A: Port = 0;
pub const PORT_B: Port = 1;
pub const PORT_C: Port = 2;
pub const PORT_D: Port = 3;
pub const PORT_E: Port = 4;
pub const PORT_F: Port = 5;
pub const PORT_G: Port = 6;
pub const PORT_H: Port = 7;
pub const PORT_I: Port = 8;
pub const PORT_TC1: Port = 3;
pub const PORT_TC2: Port = 4;
pub const PORT_TC3: Port = 5;
pub const PORT_TC4: Port = 6;
pub const PORT_TC5: Port = 7;
pub const PORT_TC6: Port = 8;
pub const PORT_D_XELPD: Port = 7;
pub const PORT_E_XELPD: Port = 8;
pub const I915_MAX_PORTS: Port = 9;

pub type Pipe = i32;
pub const INVALID_PIPE: Pipe = -1;
pub const PIPE_A: Pipe = 0;
pub const PIPE_B: Pipe = 1;
pub const PIPE_C: Pipe = 2;
pub const PIPE_D: Pipe = 3;
pub const _PIPE_EDP: Pipe = 4;
pub const I915_MAX_PIPES: Pipe = 4;

pub type HpdPin = u32;
pub const HPD_NONE: HpdPin = 0;
pub const HPD_TV: HpdPin = 0;
pub const HPD_CRT: HpdPin = 1;
pub const HPD_SDVO_B: HpdPin = 2;
pub const HPD_SDVO_C: HpdPin = 3;
pub const HPD_PORT_A: HpdPin = 4;
pub const HPD_PORT_B: HpdPin = 5;
pub const HPD_PORT_C: HpdPin = 6;
pub const HPD_PORT_D: HpdPin = 7;
pub const HPD_PORT_E: HpdPin = 8;
pub const HPD_PORT_TC1: HpdPin = 9;
pub const HPD_PORT_TC2: HpdPin = 10;
pub const HPD_PORT_TC3: HpdPin = 11;
pub const HPD_PORT_TC4: HpdPin = 12;
pub const HPD_PORT_TC5: HpdPin = 13;
pub const HPD_PORT_TC6: HpdPin = 14;
pub const HPD_NUM_PINS: HpdPin = 15;

pub type Transcoder = i32;
pub const INVALID_TRANSCODER: Transcoder = -1;
pub const TRANSCODER_A: Transcoder = 0;
pub const TRANSCODER_B: Transcoder = 1;
pub const TRANSCODER_C: Transcoder = 2;
pub const TRANSCODER_D: Transcoder = 3;
pub const TRANSCODER_EDP: Transcoder = 4;
pub const TRANSCODER_DSI_0: Transcoder = 5;
pub const TRANSCODER_DSI_1: Transcoder = 6;
pub const TRANSCODER_DSI_A: Transcoder = 5;
pub const TRANSCODER_DSI_C: Transcoder = 6;
pub const I915_MAX_TRANSCODERS: Transcoder = 7;

pub type I9xxPlaneId = u32;
pub const PLANE_A: I9xxPlaneId = 0;
pub const PLANE_B: I9xxPlaneId = 1;
pub const PLANE_C: I9xxPlaneId = 2;

pub type PlaneId = u32;
pub const PLANE_1: PlaneId = 0;
pub const PLANE_2: PlaneId = 1;
pub const PLANE_3: PlaneId = 2;
pub const PLANE_4: PlaneId = 3;
pub const PLANE_5: PlaneId = 4;
pub const PLANE_6: PlaneId = 5;
pub const PLANE_7: PlaneId = 6;
pub const PLANE_CURSOR: PlaneId = 7;
pub const I915_MAX_PLANES: PlaneId = 8;
pub const PLANE_PRIMARY: PlaneId = 0;
pub const PLANE_SPRITE0: PlaneId = 1;
pub const PLANE_SPRITE1: PlaneId = 2;

pub type AuxCh = i32;
pub const AUX_CH_NONE: AuxCh = -1;
pub const AUX_CH_A: AuxCh = 0;
pub const AUX_CH_B: AuxCh = 1;
pub const AUX_CH_C: AuxCh = 2;
pub const AUX_CH_D: AuxCh = 3;
pub const AUX_CH_E: AuxCh = 4;
pub const AUX_CH_F: AuxCh = 5;
pub const AUX_CH_G: AuxCh = 6;
pub const AUX_CH_H: AuxCh = 7;
pub const AUX_CH_I: AuxCh = 8;
pub const AUX_CH_USBC1: AuxCh = 3;
pub const AUX_CH_USBC2: AuxCh = 4;
pub const AUX_CH_USBC3: AuxCh = 5;
pub const AUX_CH_USBC4: AuxCh = 6;
pub const AUX_CH_USBC5: AuxCh = 7;
pub const AUX_CH_USBC6: AuxCh = 8;
pub const AUX_CH_D_XELPD: AuxCh = 7;
pub const AUX_CH_E_XELPD: AuxCh = 8;

pub type IntelColorBlock = u32;
pub const INTEL_PLANE_CB_PRE_CSC_LUT: IntelColorBlock = 0;
pub const INTEL_PLANE_CB_CSC: IntelColorBlock = 1;
pub const INTEL_PLANE_CB_POST_CSC_LUT: IntelColorBlock = 2;
pub const INTEL_PLANE_CB_3DLUT: IntelColorBlock = 3;
pub const INTEL_CB_MAX: IntelColorBlock = 4;


