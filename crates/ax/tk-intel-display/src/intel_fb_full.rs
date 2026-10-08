// SPDX-License-Identifier: MIT
// Copyright © 2021 Intel Corporation
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.
//! Source-ordered framebuffer translation from Linux 7.2.3 `intel_fb.c`.
//! DRM framebuffer/GEM objects and allocation/registration helpers are narrow
//! traits; Intel format, modifier, layout, offset, and validation logic remains here.
#![allow(
    dead_code,
    clippy::too_many_arguments,
    clippy::needless_range_loop,
    non_upper_case_globals
)]

extern crate alloc;
use alloc::vec::Vec;
use core::cmp::max;

pub const MOD_INVALID: u64 = u64::MAX;
pub const MOD_LINEAR: u64 = 0;
// DRM_FORMAT_MOD_CODE(INTEL, value) = (INTEL << 56) | value.
pub const MOD_X: u64 = 0x0100_0000_0000_0001;
pub const MOD_Y: u64 = 0x0100_0000_0000_0002;
pub const MOD_YF: u64 = 0x0100_0000_0000_0003;
pub const MOD_Y_CCS: u64 = 0x0100_0000_0000_0004;
pub const MOD_YF_CCS: u64 = 0x0100_0000_0000_0005;
pub const MOD_Y12_RC: u64 = 0x0100_0000_0000_0006;
pub const MOD_Y12_MC: u64 = 0x0100_0000_0000_0007;
pub const MOD_Y12_RC_CC: u64 = 0x0100_0000_0000_0008;
pub const MOD_4: u64 = 0x0100_0000_0000_0009;
pub const MOD_4_DG2_RC: u64 = 0x0100_0000_0000_000a;
pub const MOD_4_DG2_MC: u64 = 0x0100_0000_0000_000b;
pub const MOD_4_DG2_RC_CC: u64 = 0x0100_0000_0000_000c;
pub const MOD_4_MTL_RC: u64 = 0x0100_0000_0000_000d;
pub const MOD_4_MTL_MC: u64 = 0x0100_0000_0000_000e;
pub const MOD_4_MTL_RC_CC: u64 = 0x0100_0000_0000_000f;
pub const MOD_4_LNL_CCS: u64 = 0x0100_0000_0000_0010;
pub const MOD_4_BMG_CCS: u64 = 0x0100_0000_0000_0011;
pub const I915_FORMAT_MOD_4_TILED_LNL_CCS: u64 = MOD_4_LNL_CCS;
pub const I915_FORMAT_MOD_4_TILED_BMG_CCS: u64 = MOD_4_BMG_CCS;
pub const I915_FORMAT_MOD_4_TILED_MTL_MC_CCS: u64 = MOD_4_MTL_MC;
pub const I915_FORMAT_MOD_4_TILED_MTL_RC_CCS: u64 = MOD_4_MTL_RC;
pub const I915_FORMAT_MOD_4_TILED_MTL_RC_CCS_CC: u64 = MOD_4_MTL_RC_CC;
pub const I915_FORMAT_MOD_4_TILED_DG2_MC_CCS: u64 = MOD_4_DG2_MC;
pub const I915_FORMAT_MOD_4_TILED_DG2_RC_CCS_CC: u64 = MOD_4_DG2_RC_CC;
pub const I915_FORMAT_MOD_4_TILED_DG2_RC_CCS: u64 = MOD_4_DG2_RC;
pub const I915_FORMAT_MOD_4_TILED: u64 = MOD_4;
pub const I915_FORMAT_MOD_Y_TILED_GEN12_MC_CCS: u64 = MOD_Y12_MC;
pub const I915_FORMAT_MOD_Y_TILED_GEN12_RC_CCS: u64 = MOD_Y12_RC;
pub const I915_FORMAT_MOD_Y_TILED_GEN12_RC_CCS_CC: u64 = MOD_Y12_RC_CC;
pub const I915_FORMAT_MOD_Yf_TILED_CCS: u64 = MOD_YF_CCS;
pub const I915_FORMAT_MOD_Y_TILED_CCS: u64 = MOD_Y_CCS;
pub const I915_FORMAT_MOD_Yf_TILED: u64 = MOD_YF;
pub const I915_FORMAT_MOD_Y_TILED: u64 = MOD_Y;
pub const I915_FORMAT_MOD_X_TILED: u64 = MOD_X;
pub const CAP_CCS_RC: u8 = 1 << 0;
pub const CAP_CCS_RC_CC: u8 = 1 << 1;
pub const CAP_CCS_MC: u8 = 1 << 2;
pub const CAP_TILING_X: u8 = 1 << 3;
pub const CAP_TILING_Y: u8 = 1 << 4;
pub const CAP_TILING_YF: u8 = 1 << 5;
pub const CAP_TILING_4: u8 = 1 << 6;
pub const CAP_NEED64K_PHYS: u8 = 1 << 7;
pub const CAP_CCS_MASK: u8 = CAP_CCS_RC | CAP_CCS_RC_CC | CAP_CCS_MC;
pub const CAP_TILING_MASK: u8 = CAP_TILING_X | CAP_TILING_Y | CAP_TILING_YF | CAP_TILING_4;
pub const TILING_NONE: u32 = 0;
pub const TILING_X: u32 = 1;
pub const TILING_Y: u32 = 2;
pub const ROTATE_0: u32 = 1;
pub const ROTATE_90: u32 = 2;
pub const ROTATE_180: u32 = 4;
pub const ROTATE_270: u32 = 8;

// DRM FourCC values used by the modifier-specific format overrides.
pub const F_XRGB8888: u32 = 0x34325258;
pub const F_XBGR8888: u32 = 0x34324258;
pub const F_ARGB8888: u32 = 0x34325241;
pub const F_ABGR8888: u32 = 0x34324241;
pub const F_XRGB2101010: u32 = 0x30335258;
pub const F_XBGR2101010: u32 = 0x30334258;
pub const F_ARGB2101010: u32 = 0x30335241;
pub const F_ABGR2101010: u32 = 0x30334241;
pub const F_XRGB16161616F: u32 = 0x48345258;
pub const F_XBGR16161616F: u32 = 0x48344258;
pub const F_ARGB16161616F: u32 = 0x48345241;
pub const F_ABGR16161616F: u32 = 0x48344241;
pub const F_YUYV: u32 = 0x56595559;
pub const F_YVYU: u32 = 0x55595659;
pub const F_UYVY: u32 = 0x59565955;
pub const F_VYUY: u32 = 0x59555956;
pub const F_XYUV8888: u32 = 0x56555958;
pub const F_NV12: u32 = 0x3231564e;
pub const F_P010: u32 = 0x30313050;
pub const F_P012: u32 = 0x32313050;
pub const F_P016: u32 = 0x36313050;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FormatInfo {
    pub format: u32,
    pub depth: u8,
    pub num_planes: u8,
    pub cpp: [u8; 4],
    pub char_per_block: [u8; 4],
    pub block_w: [u8; 4],
    pub block_h: [u8; 4],
    pub hsub: u8,
    pub vsub: u8,
    pub has_alpha: bool,
    pub is_yuv: bool,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ModifierDesc {
    pub modifier: u64,
    pub display_ver_from: u8,
    pub display_ver_until: u8,
    pub plane_caps: u8,
    pub cc_planes: u8,
    pub packed_aux_planes: u8,
    pub planar_aux_planes: u8,
    pub formats: Option<&'static [FormatInfo]>,
}
impl ModifierDesc {
    const fn empty() -> Self {
        Self {
            modifier: 0,
            display_ver_from: 0,
            display_ver_until: 0,
            plane_caps: 0,
            cc_planes: 0,
            packed_aux_planes: 0,
            planar_aux_planes: 0,
            formats: None,
        }
    }
}
const fn fmt(
    format: u32,
    depth: u8,
    planes: u8,
    cpp: [u8; 4],
    hsub: u8,
    vsub: u8,
    alpha: bool,
    yuv: bool,
) -> FormatInfo {
    let bw = if format == F_NV12 {
        [1, 1, 4, 4]
    } else if format == F_P010 || format == F_P012 || format == F_P016 {
        [1, 1, 2, 2]
    } else if yuv {
        [1, 2, 1, 1]
    } else if cpp[0] >= 8 {
        [1, 1, 1, 1]
    } else if cpp[0] >= 4 {
        [1, 2, 1, 1]
    } else {
        [1, 1, 1, 1]
    };
    FormatInfo {
        format,
        depth,
        num_planes: planes,
        cpp,
        char_per_block: cpp,
        block_w: bw,
        block_h: [1; 4],
        hsub,
        vsub,
        has_alpha: alpha,
        is_yuv: yuv,
    }
}
const fn fmt_cc(format: u32, depth: u8, cpp0: u8, alpha: bool) -> FormatInfo {
    FormatInfo {
        format,
        depth,
        num_planes: 3,
        cpp: [cpp0, 1, 0, 0],
        char_per_block: [cpp0, 1, 0, 0],
        block_w: [1, 2, 0, 0],
        block_h: [1, 1, 0, 0],
        hsub: 1,
        vsub: 1,
        has_alpha: alpha,
        is_yuv: false,
    }
}
const fn fmt_flat_cc(format: u32, depth: u8, cpp0: u8, alpha: bool) -> FormatInfo {
    FormatInfo {
        format,
        depth,
        num_planes: 2,
        cpp: [cpp0, 0, 0, 0],
        char_per_block: [cpp0, 0, 0, 0],
        block_w: [1, 0, 0, 0],
        block_h: [1, 0, 0, 0],
        hsub: 1,
        vsub: 1,
        has_alpha: alpha,
        is_yuv: false,
    }
}
const SKL_CCS_FORMATS: [FormatInfo; 8] = [
    fmt(F_XRGB8888, 24, 2, [4, 1, 0, 0], 8, 16, false, false),
    fmt(F_XBGR8888, 24, 2, [4, 1, 0, 0], 8, 16, false, false),
    fmt(F_ARGB8888, 32, 2, [4, 1, 0, 0], 8, 16, true, false),
    fmt(F_ABGR8888, 32, 2, [4, 1, 0, 0], 8, 16, true, false),
    fmt(F_XRGB2101010, 30, 2, [4, 1, 0, 0], 8, 16, false, false),
    fmt(F_XBGR2101010, 30, 2, [4, 1, 0, 0], 8, 16, false, false),
    fmt(F_ARGB2101010, 30, 2, [4, 1, 0, 0], 8, 16, true, false),
    fmt(F_ABGR2101010, 30, 2, [4, 1, 0, 0], 8, 16, true, false),
];
const GEN12_CCS_FORMATS: [FormatInfo; 21] = [
    fmt(F_XRGB8888, 24, 2, [4, 1, 0, 0], 1, 1, false, false),
    fmt(F_XBGR8888, 24, 2, [4, 1, 0, 0], 1, 1, false, false),
    fmt(F_ARGB8888, 32, 2, [4, 1, 0, 0], 1, 1, true, false),
    fmt(F_ABGR8888, 32, 2, [4, 1, 0, 0], 1, 1, true, false),
    fmt(F_XRGB2101010, 30, 2, [4, 1, 0, 0], 1, 1, false, false),
    fmt(F_XBGR2101010, 30, 2, [4, 1, 0, 0], 1, 1, false, false),
    fmt(F_ARGB2101010, 30, 2, [4, 1, 0, 0], 1, 1, true, false),
    fmt(F_ABGR2101010, 30, 2, [4, 1, 0, 0], 1, 1, true, false),
    fmt(F_XRGB16161616F, 0, 2, [8, 1, 0, 0], 1, 1, false, false),
    fmt(F_XBGR16161616F, 0, 2, [8, 1, 0, 0], 1, 1, false, false),
    fmt(F_ARGB16161616F, 0, 2, [8, 1, 0, 0], 1, 1, true, false),
    fmt(F_ABGR16161616F, 0, 2, [8, 1, 0, 0], 1, 1, true, false),
    fmt(F_YUYV, 0, 2, [2, 1, 0, 0], 2, 1, false, true),
    fmt(F_YVYU, 0, 2, [2, 1, 0, 0], 2, 1, false, true),
    fmt(F_UYVY, 0, 2, [2, 1, 0, 0], 2, 1, false, true),
    fmt(F_VYUY, 0, 2, [2, 1, 0, 0], 2, 1, false, true),
    fmt(F_XYUV8888, 0, 2, [4, 1, 0, 0], 1, 1, false, true),
    fmt(F_NV12, 0, 4, [1, 2, 1, 1], 2, 2, false, true),
    fmt(F_P010, 0, 4, [2, 4, 1, 1], 2, 2, false, true),
    fmt(F_P012, 0, 4, [2, 4, 1, 1], 2, 2, false, true),
    fmt(F_P016, 0, 4, [2, 4, 1, 1], 2, 2, false, true),
];
const GEN12_CCS_CC_FORMATS: [FormatInfo; 12] = [
    fmt_cc(F_XRGB8888, 24, 4, false),
    fmt_cc(F_XBGR8888, 24, 4, false),
    fmt_cc(F_ARGB8888, 32, 4, true),
    fmt_cc(F_ABGR8888, 32, 4, true),
    fmt_cc(F_XRGB2101010, 30, 4, false),
    fmt_cc(F_XBGR2101010, 30, 4, false),
    fmt_cc(F_ARGB2101010, 30, 4, true),
    fmt_cc(F_ABGR2101010, 30, 4, true),
    fmt_cc(F_XRGB16161616F, 0, 8, false),
    fmt_cc(F_XBGR16161616F, 0, 8, false),
    fmt_cc(F_ARGB16161616F, 0, 8, true),
    fmt_cc(F_ABGR16161616F, 0, 8, true),
];
const GEN12_FLAT_CCS_CC_FORMATS: [FormatInfo; 12] = [
    fmt_flat_cc(F_XRGB8888, 24, 4, false),
    fmt_flat_cc(F_XBGR8888, 24, 4, false),
    fmt_flat_cc(F_ARGB8888, 32, 4, true),
    fmt_flat_cc(F_ABGR8888, 32, 4, true),
    fmt_flat_cc(F_XRGB2101010, 30, 4, false),
    fmt_flat_cc(F_XBGR2101010, 30, 4, false),
    fmt_flat_cc(F_ARGB2101010, 30, 4, true),
    fmt_flat_cc(F_ABGR2101010, 30, 4, true),
    fmt_flat_cc(F_XRGB16161616F, 0, 8, false),
    fmt_flat_cc(F_XBGR16161616F, 0, 8, false),
    fmt_flat_cc(F_ARGB16161616F, 0, 8, true),
    fmt_flat_cc(F_ABGR16161616F, 0, 8, true),
];

pub const INTEL_MODIFIERS: [ModifierDesc; 18] = [
    ModifierDesc {
        modifier: MOD_4_LNL_CCS,
        display_ver_from: 20,
        display_ver_until: 255,
        plane_caps: CAP_TILING_4,
        ..ModifierDesc::empty()
    },
    ModifierDesc {
        modifier: MOD_4_BMG_CCS,
        display_ver_from: 14,
        display_ver_until: 255,
        plane_caps: CAP_TILING_4 | CAP_NEED64K_PHYS,
        ..ModifierDesc::empty()
    },
    ModifierDesc {
        modifier: MOD_4_MTL_MC,
        display_ver_from: 14,
        display_ver_until: 14,
        plane_caps: CAP_TILING_4 | CAP_CCS_MC,
        planar_aux_planes: 0b1100,
        packed_aux_planes: 0b0010,
        formats: Some(&GEN12_CCS_FORMATS),
        ..ModifierDesc::empty()
    },
    ModifierDesc {
        modifier: MOD_4_MTL_RC,
        display_ver_from: 14,
        display_ver_until: 14,
        plane_caps: CAP_TILING_4 | CAP_CCS_RC,
        packed_aux_planes: 2,
        formats: Some(&GEN12_CCS_FORMATS),
        ..ModifierDesc::empty()
    },
    ModifierDesc {
        modifier: MOD_4_MTL_RC_CC,
        display_ver_from: 14,
        display_ver_until: 14,
        plane_caps: CAP_TILING_4 | CAP_CCS_RC_CC,
        cc_planes: 4,
        packed_aux_planes: 2,
        formats: Some(&GEN12_CCS_CC_FORMATS),
        ..ModifierDesc::empty()
    },
    ModifierDesc {
        modifier: MOD_4_DG2_MC,
        display_ver_from: 13,
        display_ver_until: 13,
        plane_caps: CAP_TILING_4 | CAP_CCS_MC,
        ..ModifierDesc::empty()
    },
    ModifierDesc {
        modifier: MOD_4_DG2_RC_CC,
        display_ver_from: 13,
        display_ver_until: 13,
        plane_caps: CAP_TILING_4 | CAP_CCS_RC_CC,
        cc_planes: 2,
        formats: Some(&GEN12_FLAT_CCS_CC_FORMATS),
        ..ModifierDesc::empty()
    },
    ModifierDesc {
        modifier: MOD_4_DG2_RC,
        display_ver_from: 13,
        display_ver_until: 13,
        plane_caps: CAP_TILING_4 | CAP_CCS_RC,
        ..ModifierDesc::empty()
    },
    ModifierDesc {
        modifier: MOD_4,
        display_ver_from: 13,
        display_ver_until: 255,
        plane_caps: CAP_TILING_4,
        ..ModifierDesc::empty()
    },
    ModifierDesc {
        modifier: MOD_Y12_MC,
        display_ver_from: 12,
        display_ver_until: 13,
        plane_caps: CAP_TILING_Y | CAP_CCS_MC,
        planar_aux_planes: 12,
        packed_aux_planes: 2,
        formats: Some(&GEN12_CCS_FORMATS),
        ..ModifierDesc::empty()
    },
    ModifierDesc {
        modifier: MOD_Y12_RC,
        display_ver_from: 12,
        display_ver_until: 13,
        plane_caps: CAP_TILING_Y | CAP_CCS_RC,
        packed_aux_planes: 2,
        formats: Some(&GEN12_CCS_FORMATS),
        ..ModifierDesc::empty()
    },
    ModifierDesc {
        modifier: MOD_Y12_RC_CC,
        display_ver_from: 12,
        display_ver_until: 13,
        plane_caps: CAP_TILING_Y | CAP_CCS_RC_CC,
        cc_planes: 4,
        packed_aux_planes: 2,
        formats: Some(&GEN12_CCS_CC_FORMATS),
        ..ModifierDesc::empty()
    },
    ModifierDesc {
        modifier: MOD_YF_CCS,
        display_ver_from: 9,
        display_ver_until: 11,
        plane_caps: CAP_TILING_YF | CAP_CCS_RC,
        packed_aux_planes: 2,
        formats: Some(&SKL_CCS_FORMATS),
        ..ModifierDesc::empty()
    },
    ModifierDesc {
        modifier: MOD_Y_CCS,
        display_ver_from: 9,
        display_ver_until: 11,
        plane_caps: CAP_TILING_Y | CAP_CCS_RC,
        packed_aux_planes: 2,
        formats: Some(&SKL_CCS_FORMATS),
        ..ModifierDesc::empty()
    },
    ModifierDesc {
        modifier: MOD_YF,
        display_ver_from: 9,
        display_ver_until: 11,
        plane_caps: CAP_TILING_YF,
        ..ModifierDesc::empty()
    },
    ModifierDesc {
        modifier: MOD_Y,
        display_ver_from: 9,
        display_ver_until: 13,
        plane_caps: CAP_TILING_Y,
        ..ModifierDesc::empty()
    },
    ModifierDesc {
        modifier: MOD_X,
        display_ver_from: 0,
        display_ver_until: 29,
        plane_caps: CAP_TILING_X,
        ..ModifierDesc::empty()
    },
    ModifierDesc {
        modifier: MOD_LINEAR,
        display_ver_from: 0,
        display_ver_until: 255,
        ..ModifierDesc::empty()
    },
];

// Linux framework objects are represented as plain input/output records. The
// callbacks below are restricted to DRM/GEM lifetime, logging, allocation,
// format capability enumeration, and DPT/frontbuffer operations.
pub trait FbHooks {
    fn warn(&mut self, _what: &'static str) {}
    fn debug(&mut self, _what: &'static str) {}
    fn modifier_allowed(&self, _format: u32, _modifier: u64) -> bool {
        true
    }
    fn min_alignment(&self, _format: u32, _modifier: u64) -> u32 {
        0
    }
    fn vtd_guard(&self, _format: u32, _modifier: u64) -> u32 {
        0
    }
    fn max_stride(&self, _format: u32, _modifier: u64, _rotation: u32) -> u32 {
        u32::MAX
    }
    fn plane_needs_physical(&self) -> bool {
        false
    }
    fn supports_auxccs(&self) -> bool {
        false
    }
    fn has_dpt(&self) -> bool {
        false
    }
    fn intel_plane_fb_max_stride(&self, _format: u32, _modifier: u64) -> u32 {
        u32::MAX
    }
    fn frontbuffer_bits(&self) -> u32 {
        0
    }
    fn reservation_signaled(&self) -> bool {
        true
    }
    fn reservation_singleton(&mut self) -> Result<bool, i32> {
        Ok(false)
    }
    fn alloc_fence_callback(&mut self) -> bool {
        true
    }
    fn frontbuffer_invalidate_dirty(&mut self) {}
    fn frontbuffer_queue_flush(&mut self) {}
    fn frontbuffer_flush_dirty(&mut self) {}
    fn add_fence_callback(&mut self) -> i32 {
        0
    }
    fn fence_put(&mut self) {}
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DisplayInfo {
    pub display_ver: u8,
    pub alderlake_p: bool,
    pub geminilake: bool,
    pub dgfx: bool,
    pub has_128b_y_tiling: bool,
    pub dpt_supported: bool,
    pub enable_dpt: bool,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Framebuffer {
    pub width: u32,
    pub height: u32,
    pub format: FormatInfo,
    pub modifier: u64,
    pub pitches: [u32; 4],
    pub offsets: [u32; 4],
    pub size: u64,
    pub id: u32,
    pub userptr: bool,
    pub gem_object: Option<u64>,
    pub object_tiled: bool,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlaneLayout {
    pub x: i32,
    pub y: i32,
    pub mapping_stride: u32,
    pub scanout_stride: u32,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RemapPlane {
    pub offset: u32,
    pub size: u32,
    pub src_stride: u16,
    pub dst_stride: u16,
    pub width: u16,
    pub height: u16,
    pub linear: bool,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FbView {
    pub color_plane: [PlaneLayout; 4],
    pub remapped: [RemapPlane; 4],
    pub plane_alignment: u32,
    pub kind: ViewKind,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ViewKind {
    #[default]
    Normal,
    Rotated,
    Remapped,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IntelFramebuffer {
    pub base: Framebuffer,
    pub normal_view: FbView,
    pub rotated_view: FbView,
    pub remapped_view: FbView,
    pub min_alignment: u32,
    pub vtd_guard: u32,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlaneState {
    pub fb: Framebuffer,
    pub view: FbView,
    pub rotation: u32,
    pub visible: bool,
    pub cursor: bool,
    pub fbc: bool,
    pub no_fbc_reason: bool,
    /// DRM source rectangle in signed 16.16 fixed-point pixels.
    pub src: FixedRect,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlaneCaps {
    pub plane_caps: u8,
    pub display_ver: u8,
    pub dgfx: bool,
    pub auxccs: bool,
    pub id: u8,
    pub modifiers: &'static [u64],
}
impl Default for PlaneCaps {
    fn default() -> Self {
        Self {
            plane_caps: 0,
            display_ver: 0,
            dgfx: false,
            auxccs: false,
            id: 0,
            modifiers: &[],
        }
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlaneViewDims {
    pub width: u32,
    pub height: u32,
    pub tile_width: u32,
    pub tile_height: u32,
}
/// `drm_rect` coordinates are 16.16 fixed point in plane state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FixedRect {
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
}
impl FixedRect {
    fn translate(&mut self, dx: i32, dy: i32) {
        self.x1 = self.x1.wrapping_add(dx);
        self.x2 = self.x2.wrapping_add(dx);
        self.y1 = self.y1.wrapping_add(dy);
        self.y2 = self.y2.wrapping_add(dy);
    }
    fn rotate_270(&mut self, width: i32, height: i32) {
        let tmp = *self;
        self.x1 = height.wrapping_sub(tmp.y2);
        self.x2 = height.wrapping_sub(tmp.y1);
        self.y1 = tmp.x1;
        self.y2 = tmp.x2;
        let _ = width; // drm_rect_rotate() does not use width for ROTATE_270.
    }
    pub fn width(self) -> i32 {
        self.x2.wrapping_sub(self.x1)
    }
    pub fn height(self) -> i32 {
        self.y2.wrapping_sub(self.y1)
    }
}
fn ceil_div(n: u32, d: u32) -> u32 {
    if d == 0 {
        0
    } else {
        // Linux DIV_ROUND_UP adds before dividing; unsigned overflow wraps.
        n.wrapping_add(d - 1) / d
    }
}
fn align(v: u32, a: u32) -> u32 {
    if a == 0 {
        v
    } else {
        // Match ALIGN's unsigned add-and-mask; these alignments are powers of 2.
        v.wrapping_add(a - 1) & !(a - 1)
    }
}
fn round_down(v: u32, a: u32) -> u32 {
    if a == 0 { v } else { (v / a).wrapping_mul(a) }
}
fn modifier_desc(modifier: u64) -> Option<&'static ModifierDesc> {
    INTEL_MODIFIERS.iter().find(|m| m.modifier == modifier)
}
fn desc_or_first(modifier: u64) -> &'static ModifierDesc {
    modifier_desc(modifier).unwrap_or(&INTEL_MODIFIERS[0])
}
fn format_override(md: &ModifierDesc, format: u32) -> Option<&'static FormatInfo> {
    md.formats?.iter().find(|f| f.format == format)
}
fn plane_caps_contain_any(caps: u8, mask: u8) -> bool {
    caps & mask != 0
}
fn plane_caps_contain_all(caps: u8, mask: u8) -> bool {
    caps & mask == mask
}

// upstream: intel_fb.c lookup_modifier_or_null()
pub fn lookup_modifier_or_null(modifier: u64) -> Option<&'static ModifierDesc> {
    modifier_desc(modifier)
}
// upstream: intel_fb.c lookup_modifier()
pub fn lookup_modifier(modifier: u64, hooks: &mut impl FbHooks) -> &'static ModifierDesc {
    if let Some(md) = modifier_desc(modifier) {
        md
    } else {
        hooks.warn("unknown modifier");
        &INTEL_MODIFIERS[0]
    }
}
// upstream: intel_fb.c lookup_format_info()
pub fn lookup_format_info(formats: &[FormatInfo], format: u32) -> Option<&FormatInfo> {
    formats.iter().find(|f| f.format == format)
}
// upstream: intel_fb.c intel_fb_modifier_to_tiling()
pub fn intel_fb_modifier_to_tiling(fb_modifier: u64) -> u32 {
    let md = match modifier_desc(fb_modifier) {
        Some(v) => v,
        None => return TILING_NONE,
    };
    match md.plane_caps & CAP_TILING_MASK {
        CAP_TILING_Y => TILING_Y,
        CAP_TILING_X => TILING_X,
        CAP_TILING_4 | CAP_TILING_YF | 0 => TILING_NONE,
        _ => TILING_NONE,
    }
}
// upstream: intel_fb.c intel_fb_get_format_info()
pub fn intel_fb_get_format_info(pixel_format: u32, modifier: u64) -> Option<&'static FormatInfo> {
    format_override(modifier_desc(modifier)?, pixel_format)
}
// upstream: intel_fb.c plane_caps_contain_any()
// (implemented as a private bit-test above; retained here as the source-order helper marker.)
// upstream: intel_fb.c plane_caps_contain_all()
// (implemented as a private bit-test above; retained here as the source-order helper marker.)
// upstream: intel_fb.c intel_fb_is_tiled_modifier()
pub fn intel_fb_is_tiled_modifier(modifier: u64, hooks: &mut impl FbHooks) -> bool {
    plane_caps_contain_any(lookup_modifier(modifier, hooks).plane_caps, CAP_TILING_MASK)
}
// upstream: intel_fb.c intel_fb_is_ccs_modifier()
pub fn intel_fb_is_ccs_modifier(modifier: u64, hooks: &mut impl FbHooks) -> bool {
    plane_caps_contain_any(lookup_modifier(modifier, hooks).plane_caps, CAP_CCS_MASK)
}
// upstream: intel_fb.c intel_fb_is_rc_ccs_cc_modifier()
pub fn intel_fb_is_rc_ccs_cc_modifier(modifier: u64, hooks: &mut impl FbHooks) -> bool {
    plane_caps_contain_any(lookup_modifier(modifier, hooks).plane_caps, CAP_CCS_RC_CC)
}
// upstream: intel_fb.c intel_fb_is_mc_ccs_modifier()
pub fn intel_fb_is_mc_ccs_modifier(modifier: u64, hooks: &mut impl FbHooks) -> bool {
    plane_caps_contain_any(lookup_modifier(modifier, hooks).plane_caps, CAP_CCS_MC)
}
// upstream: intel_fb.c intel_fb_needs_64k_phys()
pub fn intel_fb_needs_64k_phys(modifier: u64) -> bool {
    modifier_desc(modifier)
        .is_some_and(|md| plane_caps_contain_any(md.plane_caps, CAP_NEED64K_PHYS))
}
// upstream: intel_fb.c intel_fb_needs_cpu_access()
pub fn intel_fb_needs_cpu_access(fb: &Framebuffer, hooks: &mut impl FbHooks) -> bool {
    intel_fb_rc_ccs_cc_plane(fb, hooks) >= 0
}
// upstream: intel_fb.c intel_fb_is_tile4_modifier()
pub fn intel_fb_is_tile4_modifier(modifier: u64, hooks: &mut impl FbHooks) -> bool {
    plane_caps_contain_any(lookup_modifier(modifier, hooks).plane_caps, CAP_TILING_4)
}
// upstream: intel_fb.c check_modifier_display_ver_range()
pub fn check_modifier_display_ver_range(md: &ModifierDesc, from: u8, until: u8) -> bool {
    md.display_ver_from <= until && from <= md.display_ver_until
}
// upstream: intel_fb.c plane_has_modifier()
pub fn plane_has_modifier(
    display: &DisplayInfo,
    plane_caps: u8,
    md: &ModifierDesc,
    auxccs: bool,
) -> bool {
    if !(display.display_ver >= md.display_ver_from && display.display_ver <= md.display_ver_until)
        || !plane_caps_contain_all(plane_caps, md.plane_caps)
    {
        return false;
    }
    if plane_caps_contain_any(md.plane_caps, CAP_CCS_MASK) && auxccs != (md.packed_aux_planes != 0)
    {
        return false;
    }
    if md.modifier == MOD_4_BMG_CCS && (display.display_ver < 14 || !display.dgfx) {
        return false;
    }
    if md.modifier == MOD_4_LNL_CCS && (display.display_ver < 20 || display.dgfx) {
        return false;
    }
    true
}
// upstream: intel_fb.c intel_fb_plane_get_modifiers()
pub fn intel_fb_plane_get_modifiers(
    display: &DisplayInfo,
    plane_caps: u8,
    auxccs: bool,
    hooks: &mut impl FbHooks,
) -> Option<Vec<u64>> {
    let mut count = 1; // +1 for the DRM_FORMAT_MOD_INVALID terminator.
    for md in &INTEL_MODIFIERS {
        if plane_has_modifier(display, plane_caps, md, auxccs) {
            count += 1;
        }
    }
    let mut out = Vec::new();
    if out.try_reserve_exact(count).is_err() {
        hooks.warn("modifier list allocation failed");
        return None;
    }
    for md in &INTEL_MODIFIERS {
        if plane_has_modifier(display, plane_caps, md, auxccs) {
            out.push(md.modifier)
        }
    }
    out.push(MOD_INVALID);
    Some(out)
}
// upstream: intel_fb.c intel_fb_plane_supports_modifier()
pub fn intel_fb_plane_supports_modifier(plane: &PlaneCaps, modifier: u64) -> bool {
    plane.modifiers.contains(&modifier)
}
// upstream: intel_fb.c format_is_yuv_semiplanar()
pub fn format_is_yuv_semiplanar(md: &ModifierDesc, info: &FormatInfo) -> bool {
    if !info.is_yuv {
        return false;
    }
    if md.planar_aux_planes.count_ones() == 2 {
        info.num_planes == 4
    } else {
        info.num_planes == 2
    }
}
// upstream: intel_fb.c intel_format_info_is_yuv_semiplanar()
pub fn intel_format_info_is_yuv_semiplanar(
    info: &FormatInfo,
    modifier: u64,
    hooks: &mut impl FbHooks,
) -> bool {
    format_is_yuv_semiplanar(lookup_modifier(modifier, hooks), info)
}
// upstream: intel_fb.c ccs_aux_plane_mask()
pub fn ccs_aux_plane_mask(md: &ModifierDesc, format: &FormatInfo) -> u8 {
    if format_is_yuv_semiplanar(md, format) {
        md.planar_aux_planes
    } else {
        md.packed_aux_planes
    }
}
// upstream: intel_fb.c intel_fb_is_ccs_aux_plane()
pub fn intel_fb_is_ccs_aux_plane(
    fb: &Framebuffer,
    color_plane: i32,
    hooks: &mut impl FbHooks,
) -> bool {
    color_plane >= 0
        && ccs_aux_plane_mask(lookup_modifier(fb.modifier, hooks), &fb.format)
            & (1u8.checked_shl(color_plane as u32).unwrap_or(0))
            != 0
}
// upstream: intel_fb.c intel_fb_is_gen12_ccs_aux_plane()
pub fn intel_fb_is_gen12_ccs_aux_plane(
    fb: &Framebuffer,
    color_plane: i32,
    hooks: &mut impl FbHooks,
) -> bool {
    let md = lookup_modifier(fb.modifier, hooks);
    check_modifier_display_ver_range(md, 12, 14)
        && ccs_aux_plane_mask(md, &fb.format) & (1u8.checked_shl(color_plane as u32).unwrap_or(0))
            != 0
}
// upstream: intel_fb.c intel_fb_rc_ccs_cc_plane()
pub fn intel_fb_rc_ccs_cc_plane(fb: &Framebuffer, hooks: &mut impl FbHooks) -> i32 {
    let md = lookup_modifier(fb.modifier, hooks);
    if md.cc_planes == 0 {
        return -1;
    }
    if md.cc_planes.count_ones() > 1 {
        hooks.warn("multiple CCS clear-color planes")
    }
    md.cc_planes.trailing_zeros() as i32
}
// upstream: intel_fb.c is_gen12_ccs_cc_plane()
pub fn is_gen12_ccs_cc_plane(fb: &Framebuffer, color_plane: i32, hooks: &mut impl FbHooks) -> bool {
    intel_fb_rc_ccs_cc_plane(fb, hooks) == color_plane
}
// upstream: intel_fb.c is_surface_linear()
pub fn is_surface_linear(fb: &Framebuffer, color_plane: i32, hooks: &mut impl FbHooks) -> bool {
    fb.modifier == MOD_LINEAR
        || intel_fb_is_gen12_ccs_aux_plane(fb, color_plane, hooks)
        || is_gen12_ccs_cc_plane(fb, color_plane, hooks)
}
// upstream: intel_fb.c main_to_ccs_plane()
pub fn main_to_ccs_plane(fb: &Framebuffer, main_plane: i32, hooks: &mut impl FbHooks) -> i32 {
    if !intel_fb_is_ccs_modifier(fb.modifier, hooks)
        || (main_plane != 0 && main_plane >= fb.format.num_planes as i32 / 2)
    {
        hooks.warn("invalid main to CCS plane conversion")
    }
    fb.format.num_planes as i32 / 2 + main_plane
}
// upstream: intel_fb.c skl_ccs_to_main_plane()
pub fn skl_ccs_to_main_plane(fb: &Framebuffer, ccs_plane: i32, hooks: &mut impl FbHooks) -> i32 {
    if !intel_fb_is_ccs_modifier(fb.modifier, hooks) || ccs_plane < fb.format.num_planes as i32 / 2
    {
        hooks.warn("invalid CCS to main plane conversion")
    }
    if is_gen12_ccs_cc_plane(fb, ccs_plane, hooks) {
        0
    } else {
        ccs_plane - fb.format.num_planes as i32 / 2
    }
}
// upstream: intel_fb.c gen12_ccs_aux_stride()
pub fn gen12_ccs_aux_stride(
    fb: &Framebuffer,
    ccs_plane: i32,
    display: &DisplayInfo,
    hooks: &mut impl FbHooks,
) -> u32 {
    let main = skl_ccs_to_main_plane(fb, ccs_plane, hooks) as usize;
    ceil_div(
        fb.pitches[main],
        4u32.wrapping_mul(intel_tile_width_bytes(fb, main as i32, display, hooks)),
    )
    .wrapping_mul(64)
}
// upstream: intel_fb.c skl_main_to_aux_plane()
pub fn skl_main_to_aux_plane(
    fb: &Framebuffer,
    main_plane: i32,
    display: &DisplayInfo,
    hooks: &mut impl FbHooks,
) -> i32 {
    let md = lookup_modifier(fb.modifier, hooks);
    if md.packed_aux_planes | md.planar_aux_planes != 0 {
        main_to_ccs_plane(fb, main_plane, hooks)
    } else if display.display_ver < 11 && format_is_yuv_semiplanar(md, &fb.format) {
        1
    } else {
        0
    }
}
// upstream: intel_fb.c intel_tile_size()
pub fn intel_tile_size(display: &DisplayInfo) -> u32 {
    if display.display_ver == 2 { 2048 } else { 4096 }
}
// upstream: intel_fb.c intel_tile_width_bytes()
pub fn intel_tile_width_bytes(
    fb: &Framebuffer,
    color_plane: i32,
    display: &DisplayInfo,
    hooks: &mut impl FbHooks,
) -> u32 {
    let cpp = fb.format.cpp[color_plane as usize] as u32;
    match fb.modifier {
        MOD_LINEAR => intel_tile_size(display),
        MOD_X => {
            if display.display_ver == 2 {
                128
            } else {
                512
            }
        }
        MOD_4_BMG_CCS | MOD_4_LNL_CCS | MOD_4_DG2_RC | MOD_4_DG2_RC_CC | MOD_4_DG2_MC | MOD_4 => {
            128
        }
        MOD_Y_CCS => {
            if intel_fb_is_ccs_aux_plane(fb, color_plane, hooks) {
                128
            } else if display.has_128b_y_tiling {
                128
            } else {
                512
            }
        }
        MOD_4_MTL_RC | MOD_4_MTL_RC_CC | MOD_4_MTL_MC | MOD_Y12_RC | MOD_Y12_RC_CC | MOD_Y12_MC => {
            if intel_fb_is_ccs_aux_plane(fb, color_plane, hooks)
                || is_gen12_ccs_cc_plane(fb, color_plane, hooks)
            {
                64
            } else if display.has_128b_y_tiling {
                128
            } else {
                512
            }
        }
        MOD_Y => {
            if display.has_128b_y_tiling {
                128
            } else {
                512
            }
        }
        MOD_YF_CCS => {
            if intel_fb_is_ccs_aux_plane(fb, color_plane, hooks) {
                128
            } else {
                match cpp {
                    1 => 64,
                    2 | 4 => 128,
                    8 | 16 => 256,
                    _ => cpp,
                }
            }
        }
        MOD_YF => match cpp {
            1 => 64,
            2 | 4 => 128,
            8 | 16 => 256,
            _ => cpp,
        },
        _ => {
            hooks.warn("unknown framebuffer modifier tile width");
            cpp
        }
    }
}
// upstream: intel_fb.c intel_tile_height()
pub fn intel_tile_height(
    fb: &Framebuffer,
    color_plane: i32,
    display: &DisplayInfo,
    hooks: &mut impl FbHooks,
) -> u32 {
    intel_tile_size(display) / intel_tile_width_bytes(fb, color_plane, display, hooks)
}
// upstream: intel_fb.c intel_tile_dims()
pub fn intel_tile_dims(
    fb: &Framebuffer,
    color_plane: i32,
    display: &DisplayInfo,
    hooks: &mut impl FbHooks,
) -> (u32, u32) {
    let cpp = fb.format.cpp[color_plane as usize] as u32;
    (
        intel_tile_width_bytes(fb, color_plane, display, hooks) / cpp,
        intel_tile_height(fb, color_plane, display, hooks),
    )
}
// upstream: intel_fb.c intel_tile_block_dims()
pub fn intel_tile_block_dims(
    fb: &Framebuffer,
    color_plane: i32,
    display: &DisplayInfo,
    hooks: &mut impl FbHooks,
) -> (u32, u32) {
    let (w, mut h) = intel_tile_dims(fb, color_plane, display, hooks);
    if intel_fb_is_gen12_ccs_aux_plane(fb, color_plane, hooks) {
        h = 1
    }
    (w, h)
}
// upstream: intel_fb.c intel_fb_align_height()
pub fn intel_fb_align_height(
    fb: &Framebuffer,
    color_plane: i32,
    height: u32,
    display: &DisplayInfo,
    hooks: &mut impl FbHooks,
) -> u32 {
    align(height, intel_tile_height(fb, color_plane, display, hooks))
}
// upstream: intel_fb.c intel_fb_modifier_uses_dpt()
pub fn intel_fb_modifier_uses_dpt(
    display: &DisplayInfo,
    modifier: u64,
    _hooks: &impl FbHooks,
) -> bool {
    display.dpt_supported && modifier != MOD_LINEAR
}
// upstream: intel_fb.c intel_fb_uses_dpt()
pub fn intel_fb_uses_dpt(fb: &Framebuffer, display: &DisplayInfo, hooks: &impl FbHooks) -> bool {
    display.enable_dpt && intel_fb_modifier_uses_dpt(display, fb.modifier, hooks)
}
// upstream: intel_fb.c intel_fb_plane_get_subsampling()
pub fn intel_fb_plane_get_subsampling(
    fb: &Framebuffer,
    color_plane: i32,
    hooks: &mut impl FbHooks,
) -> (u32, u32) {
    if color_plane == 0 {
        return (1, 1);
    }
    if !intel_fb_is_gen12_ccs_aux_plane(fb, color_plane, hooks) {
        return (fb.format.hsub as u32, fb.format.vsub as u32);
    }
    let main = skl_ccs_to_main_plane(fb, color_plane, hooks) as usize;
    let bw = |p: usize| fb.format.block_w[p].max(1) as u32;
    let hsub = bw(color_plane as usize) / bw(main);
    let hsub = if main == 0 {
        hsub * fb.format.hsub as u32
    } else {
        hsub
    };
    (hsub, 32)
}
// upstream: intel_fb.c intel_fb_plane_dims()
pub fn intel_fb_plane_dims(
    fb: &IntelFramebuffer,
    color_plane: i32,
    hooks: &mut impl FbHooks,
) -> (u32, u32) {
    let main = if intel_fb_is_ccs_aux_plane(&fb.base, color_plane, hooks) {
        skl_ccs_to_main_plane(&fb.base, color_plane, hooks)
    } else {
        0
    };
    let (mh, mv) = intel_fb_plane_get_subsampling(&fb.base, main, hooks);
    let (h, v) = intel_fb_plane_get_subsampling(&fb.base, color_plane, hooks);
    (
        ceil_div(fb.base.width, mh * h),
        ceil_div(fb.base.height, mv * v),
    )
}
// upstream: intel_fb.c intel_adjust_tile_offset()
pub fn intel_adjust_tile_offset(
    x: &mut i32,
    y: &mut i32,
    tile_width: u32,
    tile_height: u32,
    tile_size: u32,
    pitch_tiles: u32,
    old_offset: u32,
    new_offset: u32,
    hooks: &mut impl FbHooks,
) -> u32 {
    if tile_size == 0 || pitch_tiles == 0 {
        return new_offset;
    }
    if old_offset & (tile_size - 1) != 0
        || new_offset & (tile_size - 1) != 0
        || new_offset > old_offset
    {
        hooks.warn("unaligned or increasing GTT tile offset adjustment");
    }
    let tiles = old_offset.wrapping_sub(new_offset) / tile_size;
    *y = (*y).wrapping_add((tiles / pitch_tiles).wrapping_mul(tile_height) as i32);
    *x = (*x).wrapping_add((tiles % pitch_tiles).wrapping_mul(tile_width) as i32);
    let pitch_pixels = pitch_tiles.wrapping_mul(tile_width);
    if pitch_pixels != 0 {
        *y = (*y).wrapping_add(((*x as u32 / pitch_pixels).wrapping_mul(tile_height)) as i32);
        *x = (*x as u32 % pitch_pixels) as i32
    }
    new_offset
}
// upstream: intel_fb.c intel_adjust_linear_offset()
pub fn intel_adjust_linear_offset(
    x: &mut i32,
    y: &mut i32,
    cpp: u32,
    pitch: u32,
    old_offset: u32,
    new_offset: u32,
) -> u32 {
    let offset = old_offset
        .wrapping_add((*y as u32).wrapping_mul(pitch))
        .wrapping_add((*x as u32).wrapping_mul(cpp));
    if pitch != 0 {
        let adjusted = offset.wrapping_sub(new_offset);
        *y = (adjusted / pitch) as i32;
        *x = (adjusted.wrapping_sub((*y as u32).wrapping_mul(pitch)) / cpp) as i32
    }
    new_offset
}
// upstream: intel_fb.c intel_adjust_aligned_offset()
pub fn intel_adjust_aligned_offset(
    x: &mut i32,
    y: &mut i32,
    fb: &Framebuffer,
    color_plane: i32,
    rotation: u32,
    pitch: u32,
    old_offset: u32,
    new_offset: u32,
    display: &DisplayInfo,
    hooks: &mut impl FbHooks,
) -> u32 {
    if !is_surface_linear(fb, color_plane, hooks) {
        let tile_size = intel_tile_size(display);
        let (mut tw, mut th) = intel_tile_dims(fb, color_plane, display, hooks);
        let pitch_tiles;
        if rotation & (ROTATE_90 | ROTATE_270) != 0 {
            pitch_tiles = pitch / th;
            core::mem::swap(&mut tw, &mut th)
        } else {
            pitch_tiles = pitch / tw.wrapping_mul(fb.format.cpp[color_plane as usize] as u32)
        }
        if new_offset > old_offset {
            hooks.warn("new aligned GTT offset exceeds original offset");
        }
        intel_adjust_tile_offset(
            x,
            y,
            tw,
            th,
            tile_size,
            pitch_tiles,
            old_offset,
            new_offset,
            hooks,
        )
    } else {
        intel_adjust_linear_offset(
            x,
            y,
            fb.format.cpp[color_plane as usize] as u32,
            pitch,
            old_offset,
            new_offset,
        )
    }
}
// upstream: intel_fb.c intel_plane_adjust_aligned_offset()
pub fn intel_plane_adjust_aligned_offset(
    x: &mut i32,
    y: &mut i32,
    state: &PlaneState,
    color_plane: i32,
    old_offset: u32,
    new_offset: u32,
    display: &DisplayInfo,
    hooks: &mut impl FbHooks,
) -> u32 {
    intel_adjust_aligned_offset(
        x,
        y,
        &state.fb,
        color_plane,
        state.rotation,
        state.view.color_plane[color_plane as usize].mapping_stride,
        old_offset,
        new_offset,
        display,
        hooks,
    )
}
// upstream: intel_fb.c intel_compute_aligned_offset()
pub fn intel_compute_aligned_offset(
    x: &mut i32,
    y: &mut i32,
    fb: &Framebuffer,
    color_plane: i32,
    pitch: u32,
    rotation: u32,
    alignment: u32,
    display: &DisplayInfo,
    hooks: &mut impl FbHooks,
) -> u32 {
    let cpp = fb.format.cpp[color_plane as usize] as u32;
    if !is_surface_linear(fb, color_plane, hooks) {
        let tile_size = intel_tile_size(display);
        let (mut tw, mut th) = intel_tile_dims(fb, color_plane, display, hooks);
        let pitch_tiles;
        if rotation & (ROTATE_90 | ROTATE_270) != 0 {
            pitch_tiles = pitch / th;
            core::mem::swap(&mut tw, &mut th)
        } else {
            pitch_tiles = pitch / tw.wrapping_mul(cpp)
        }
        let tile_rows = *y as u32 / th;
        *y = (*y as u32 % th) as i32;
        let tiles = *x as u32 / tw;
        *x = (*x as u32 % tw) as i32;
        let offset = tile_rows
            .wrapping_mul(pitch_tiles)
            .wrapping_add(tiles)
            .wrapping_mul(tile_size);
        let aligned = if alignment != 0 {
            round_down(offset, alignment)
        } else {
            offset
        };
        intel_adjust_tile_offset(x, y, tw, th, tile_size, pitch_tiles, offset, aligned, hooks)
    } else {
        let offset = (*y as u32)
            .wrapping_mul(pitch)
            .wrapping_add((*x as u32).wrapping_mul(cpp));
        let aligned = if alignment != 0 {
            round_down(offset, alignment);
            let rem = offset % alignment;
            *y = (rem / pitch) as i32;
            *x = (rem.wrapping_sub((*y as u32).wrapping_mul(pitch)) / cpp) as i32;
            round_down(offset, alignment)
        } else {
            *x = 0;
            *y = 0;
            offset
        };
        aligned
    }
}
// upstream: intel_fb.c intel_plane_compute_aligned_offset()
pub fn intel_plane_compute_aligned_offset(
    x: &mut i32,
    y: &mut i32,
    state: &PlaneState,
    color_plane: i32,
    alignment: u32,
    display: &DisplayInfo,
    hooks: &mut impl FbHooks,
) -> u32 {
    intel_compute_aligned_offset(
        x,
        y,
        &state.fb,
        color_plane,
        state.view.color_plane[color_plane as usize].mapping_stride,
        state.rotation,
        alignment,
        display,
        hooks,
    )
}
// upstream: intel_fb.c intel_fb_offset_to_xy()
pub fn intel_fb_offset_to_xy(
    fb: &Framebuffer,
    color_plane: i32,
    display: &DisplayInfo,
    hooks: &mut impl FbHooks,
) -> Result<(i32, i32), i32> {
    let alignment = if fb.modifier != MOD_LINEAR {
        intel_tile_size(display)
    } else {
        0
    };
    if alignment != 0 && fb.offsets[color_plane as usize] % alignment != 0 {
        hooks.debug("misaligned framebuffer plane offset");
        return Err(-22);
    }
    let (hsub, vsub) = intel_fb_plane_get_subsampling(fb, color_plane, hooks);
    let plane_height = ceil_div(fb.height, vsub);
    let height = align(
        plane_height,
        intel_tile_height(fb, color_plane, display, hooks),
    );
    if height
        .checked_mul(fb.pitches[color_plane as usize])
        .and_then(|v| v.checked_add(fb.offsets[color_plane as usize]))
        .is_none()
    {
        hooks.debug("framebuffer offset or pitch overflow");
        return Err(-34);
    }
    let _ = hsub;
    let (mut x, mut y) = (0, 0);
    intel_adjust_aligned_offset(
        &mut x,
        &mut y,
        fb,
        color_plane,
        ROTATE_0,
        fb.pitches[color_plane as usize],
        fb.offsets[color_plane as usize],
        0,
        display,
        hooks,
    );
    Ok((x, y))
}
// upstream: intel_fb.c intel_fb_check_ccs_xy()
pub fn intel_fb_check_ccs_xy(
    fb: &IntelFramebuffer,
    ccs_plane: i32,
    x: i32,
    y: i32,
    display: &DisplayInfo,
    hooks: &mut impl FbHooks,
) -> Result<(), i32> {
    if !intel_fb_is_ccs_aux_plane(&fb.base, ccs_plane, hooks) {
        return Ok(());
    }
    let (mut tw, mut th) = intel_tile_block_dims(&fb.base, ccs_plane, display, hooks);
    let (hsub, vsub) = intel_fb_plane_get_subsampling(&fb.base, ccs_plane, hooks);
    tw *= hsub;
    th *= vsub;
    let cx = (x as u32).wrapping_mul(hsub) % tw;
    let cy = (y as u32).wrapping_mul(vsub) % th;
    let main = skl_ccs_to_main_plane(&fb.base, ccs_plane, hooks) as usize;
    let mx = fb.normal_view.color_plane[main].x as u32 % tw;
    let my = fb.normal_view.color_plane[main].y as u32 % th;
    if mx != cx || my != cy {
        hooks.debug("CCS and main surface intra-tile coordinates differ");
        Err(-22)
    } else {
        Ok(())
    }
}
// upstream: intel_fb.c intel_plane_can_remap()
pub fn intel_plane_can_remap(
    state: &PlaneState,
    display: &DisplayInfo,
    hooks: &mut impl FbHooks,
) -> bool {
    if state.cursor
        || display.display_ver < 4
        || intel_fb_is_ccs_modifier(state.fb.modifier, hooks)
        || intel_fb_uses_dpt(&state.fb, display, hooks)
    {
        return false;
    }
    if state.fb.modifier == MOD_LINEAR {
        let align = intel_tile_size(display) - 1;
        for p in 0..state.fb.format.num_planes as usize {
            if state.fb.pitches[p] & align != 0 {
                return false;
            }
        }
    }
    true
}
// upstream: intel_fb.c intel_fb_needs_pot_stride_remap()
pub fn intel_fb_needs_pot_stride_remap(
    fb: &IntelFramebuffer,
    display: &DisplayInfo,
    hooks: &impl FbHooks,
) -> bool {
    (display.alderlake_p || display.display_ver >= 14)
        && intel_fb_uses_dpt(&fb.base, display, hooks)
}
// upstream: intel_fb.c intel_plane_uses_fence()
pub fn intel_plane_uses_fence(
    state: &PlaneState,
    _display: &DisplayInfo,
    needs_fence: bool,
) -> bool {
    needs_fence || (state.fbc && !state.no_fbc_reason && state.view.kind == ViewKind::Normal)
}
// upstream: intel_fb.c intel_fb_pitch()
pub fn intel_fb_pitch(
    fb: &IntelFramebuffer,
    color_plane: i32,
    rotation: u32,
    display: &DisplayInfo,
    hooks: &impl FbHooks,
) -> u32 {
    if rotation & (ROTATE_90 | ROTATE_270) != 0 {
        fb.rotated_view.color_plane[color_plane as usize].mapping_stride
    } else if intel_fb_needs_pot_stride_remap(fb, display, hooks) {
        fb.remapped_view.color_plane[color_plane as usize].mapping_stride
    } else {
        fb.normal_view.color_plane[color_plane as usize].mapping_stride
    }
}
// upstream: intel_fb.c intel_plane_needs_remap()
pub fn intel_plane_needs_remap(
    state: &PlaneState,
    fb: &IntelFramebuffer,
    display: &DisplayInfo,
    hooks: &mut impl FbHooks,
) -> bool {
    if !state.visible || !intel_plane_can_remap(state, display, hooks) {
        return false;
    }
    let stride = intel_fb_pitch(fb, 0, state.rotation, display, hooks);
    let max_stride = hooks.max_stride(state.fb.format.format, state.fb.modifier, state.rotation);
    stride > max_stride
}
// upstream: intel_fb.c convert_plane_offset_to_xy()
pub fn convert_plane_offset_to_xy(
    fb: &IntelFramebuffer,
    color_plane: i32,
    plane_width: u32,
    display: &DisplayInfo,
    hooks: &mut impl FbHooks,
) -> Result<(i32, i32), i32> {
    let (x, y) = intel_fb_offset_to_xy(&fb.base, color_plane, display, hooks).map_err(|e| {
        hooks.debug("bad framebuffer plane offset");
        e
    })?;
    intel_fb_check_ccs_xy(fb, color_plane, x, y, display, hooks)?;
    if color_plane == 0
        && fb.base.object_tiled
        && (x as u32)
            .wrapping_add(plane_width)
            .wrapping_mul(fb.base.format.cpp[0] as u32)
            > fb.base.pitches[0]
    {
        hooks.debug("framebuffer wraps across tiled fence boundary");
        return Err(-22);
    }
    Ok((x, y))
}
// upstream: intel_fb.c calc_plane_aligned_offset()
pub fn calc_plane_aligned_offset(
    fb: &IntelFramebuffer,
    color_plane: i32,
    x: &mut i32,
    y: &mut i32,
    display: &DisplayInfo,
    hooks: &mut impl FbHooks,
) -> u32 {
    let ts = intel_tile_size(display);
    intel_compute_aligned_offset(
        x,
        y,
        &fb.base,
        color_plane,
        fb.base.pitches[color_plane as usize],
        ROTATE_0,
        ts,
        display,
        hooks,
    ) / ts
}
// upstream: intel_fb.c init_plane_view_dims()
pub fn init_plane_view_dims(
    fb: &IntelFramebuffer,
    color_plane: i32,
    width: u32,
    height: u32,
    display: &DisplayInfo,
    hooks: &mut impl FbHooks,
) -> PlaneViewDims {
    let (tw, th) = intel_tile_dims(&fb.base, color_plane, display, hooks);
    PlaneViewDims {
        width,
        height,
        tile_width: tw,
        tile_height: th,
    }
}
// upstream: intel_fb.c plane_view_src_stride_tiles()
pub fn plane_view_src_stride_tiles(
    fb: &IntelFramebuffer,
    color_plane: i32,
    dims: &PlaneViewDims,
) -> u32 {
    ceil_div(
        fb.base.pitches[color_plane as usize],
        dims.tile_width
            .wrapping_mul(fb.base.format.cpp[color_plane as usize] as u32),
    )
}
// upstream: intel_fb.c plane_view_dst_stride_tiles()
pub fn plane_view_dst_stride_tiles(
    fb: &IntelFramebuffer,
    _color_plane: i32,
    pitch_tiles: u32,
    display: &DisplayInfo,
    hooks: &impl FbHooks,
) -> u32 {
    if intel_fb_needs_pot_stride_remap(fb, display, hooks) {
        pitch_tiles.max(8).checked_next_power_of_two().unwrap_or(0)
    } else {
        pitch_tiles
    }
}
fn assign_checked_u16(value: u32, hooks: &mut impl FbHooks) -> u16 {
    if value > u16::MAX as u32 {
        hooks.warn("framebuffer view field overflows u16")
    }
    value as u16
}
// upstream: intel_fb.c plane_view_scanout_stride()
pub fn plane_view_scanout_stride(
    fb: &IntelFramebuffer,
    color_plane: i32,
    tile_width: u32,
    src_stride_tiles: u32,
    dst_stride_tiles: u32,
    display: &DisplayInfo,
) -> u32 {
    let stride = if (display.alderlake_p || display.display_ver >= 14)
        && src_stride_tiles < dst_stride_tiles
    {
        src_stride_tiles
    } else {
        dst_stride_tiles
    };
    stride
        .wrapping_mul(tile_width)
        .wrapping_mul(fb.base.format.cpp[color_plane as usize] as u32)
}
// upstream: intel_fb.c plane_view_width_tiles()
pub fn plane_view_width_tiles(
    _fb: &IntelFramebuffer,
    _color_plane: i32,
    dims: &PlaneViewDims,
    x: i32,
) -> u32 {
    ceil_div((x as u32).wrapping_add(dims.width), dims.tile_width)
}
// upstream: intel_fb.c plane_view_height_tiles()
pub fn plane_view_height_tiles(
    _fb: &IntelFramebuffer,
    _color_plane: i32,
    dims: &PlaneViewDims,
    y: i32,
) -> u32 {
    ceil_div((y as u32).wrapping_add(dims.height), dims.tile_height)
}
// upstream: intel_fb.c plane_view_linear_tiles()
pub fn plane_view_linear_tiles(
    fb: &IntelFramebuffer,
    color_plane: i32,
    dims: &PlaneViewDims,
    x: i32,
    y: i32,
    display: &DisplayInfo,
) -> u32 {
    let size = (y as u32)
        .wrapping_add(dims.height)
        .wrapping_mul(fb.base.pitches[color_plane as usize])
        .wrapping_add((x as u32).wrapping_mul(fb.base.format.cpp[color_plane as usize] as u32));
    ceil_div(size, intel_tile_size(display))
}
// upstream: intel_fb.c calc_plane_remap_info()
pub fn calc_plane_remap_info(
    fb: &IntelFramebuffer,
    color_plane: i32,
    dims: &PlaneViewDims,
    obj_offset: u32,
    mut gtt_offset: u32,
    x: i32,
    y: i32,
    view: &mut FbView,
    display: &DisplayInfo,
    hooks: &mut impl FbHooks,
) -> u32 {
    let t = color_plane as usize;
    let mut info = RemapPlane {
        offset: obj_offset,
        ..RemapPlane::default()
    };
    let mut tw = dims.tile_width;
    let mut th = dims.tile_height;
    let ts = intel_tile_size(display);
    let mut size = 0u32;
    if obj_offset > 0x7fff_ffff {
        hooks.warn("framebuffer view offset overflows its 31-bit field")
    }
    info.offset = obj_offset & 0x7fff_ffff;
    if intel_fb_is_gen12_ccs_aux_plane(&fb.base, color_plane, hooks) {
        info.linear = true;
        info.size = plane_view_linear_tiles(fb, color_plane, dims, x, y, display)
    } else {
        info.linear = false;
        info.src_stride =
            assign_checked_u16(plane_view_src_stride_tiles(fb, color_plane, dims), hooks);
        info.width = assign_checked_u16(plane_view_width_tiles(fb, color_plane, dims, x), hooks);
        info.height = assign_checked_u16(plane_view_height_tiles(fb, color_plane, dims, y), hooks)
    }
    if view.kind == ViewKind::Rotated {
        info.dst_stride = assign_checked_u16(
            plane_view_dst_stride_tiles(fb, color_plane, info.height as u32, display, hooks),
            hooks,
        );
        let nx = x;
        let ny = y;
        view.color_plane[t].x = ((info.height as i32).wrapping_mul(th as i32))
            .wrapping_sub(ny.wrapping_add(dims.height as i32));
        view.color_plane[t].y = nx;
        view.color_plane[t].mapping_stride = (info.dst_stride as u32).wrapping_mul(th);
        view.color_plane[t].scanout_stride = view.color_plane[t].mapping_stride;
        size = size.wrapping_add((info.dst_stride as u32).wrapping_mul(info.width as u32));
        core::mem::swap(&mut tw, &mut th)
    } else {
        if view.plane_alignment != 0 {
            let aligned = align(gtt_offset, view.plane_alignment);
            size = size.wrapping_add(aligned.wrapping_sub(gtt_offset));
            gtt_offset = aligned;
        }
        view.color_plane[t].x = x;
        view.color_plane[t].y = y;
        if info.linear {
            view.color_plane[t].mapping_stride = fb.base.pitches[t];
            view.color_plane[t].scanout_stride = view.color_plane[t].mapping_stride;
            size = size.wrapping_add(info.size)
        } else {
            let mut dst = if intel_fb_needs_pot_stride_remap(fb, display, hooks)
                && intel_fb_is_ccs_modifier(fb.base.modifier, hooks)
            {
                info.src_stride as u32
            } else {
                info.width as u32
            };
            dst = plane_view_dst_stride_tiles(fb, color_plane, dst, display, hooks);
            info.dst_stride = assign_checked_u16(dst, hooks);
            view.color_plane[t].mapping_stride = (info.dst_stride as u32)
                .wrapping_mul(tw)
                .wrapping_mul(fb.base.format.cpp[t] as u32);
            view.color_plane[t].scanout_stride = plane_view_scanout_stride(
                fb,
                color_plane,
                tw,
                info.src_stride as u32,
                dst,
                display,
            );
            size = size.wrapping_add((info.dst_stride as u32).wrapping_mul(info.height as u32))
        }
    }
    if info.linear {
        intel_adjust_linear_offset(
            &mut view.color_plane[t].x,
            &mut view.color_plane[t].y,
            fb.base.format.cpp[t] as u32,
            view.color_plane[t].mapping_stride,
            gtt_offset.wrapping_mul(ts),
            0,
        );
    } else {
        intel_adjust_tile_offset(
            &mut view.color_plane[t].x,
            &mut view.color_plane[t].y,
            tw,
            th,
            ts,
            info.dst_stride as u32,
            gtt_offset.wrapping_mul(ts),
            0,
            hooks,
        );
    }
    view.remapped[t] = info;
    size
}
// upstream: intel_fb.c calc_plane_normal_size()
pub fn calc_plane_normal_size(
    fb: &IntelFramebuffer,
    color_plane: i32,
    dims: &PlaneViewDims,
    x: i32,
    y: i32,
    display: &DisplayInfo,
    hooks: &mut impl FbHooks,
) -> u32 {
    if is_surface_linear(&fb.base, color_plane, hooks) {
        plane_view_linear_tiles(fb, color_plane, dims, x, y, display)
    } else {
        plane_view_src_stride_tiles(fb, color_plane, dims)
            .wrapping_mul(plane_view_height_tiles(fb, color_plane, dims, y))
            .wrapping_add(u32::from(x != 0))
    }
}
// upstream: intel_fb.c intel_fb_view_init()
pub fn intel_fb_view_init(
    view: &mut FbView,
    kind: ViewKind,
    fb: &IntelFramebuffer,
    display: &DisplayInfo,
    hooks: &impl FbHooks,
) {
    *view = FbView {
        kind,
        ..FbView::default()
    };
    if kind == ViewKind::Remapped && intel_fb_needs_pot_stride_remap(fb, display, hooks) {
        view.plane_alignment = 2 * 1024 * 1024 / 4096
    }
}
// upstream: intel_fb.c intel_fb_supports_90_270_rotation()
pub fn intel_fb_supports_90_270_rotation(fb: &Framebuffer, display: &DisplayInfo) -> bool {
    display.display_ver < 13 && (fb.modifier == MOD_Y || fb.modifier == MOD_YF)
}
// upstream: intel_fb.c intel_fb_min_alignment()
pub fn intel_fb_min_alignment(fb: &Framebuffer, hooks: &impl FbHooks) -> u32 {
    if hooks.modifier_allowed(fb.format.format, fb.modifier) && !hooks.plane_needs_physical() {
        hooks.min_alignment(fb.format.format, fb.modifier)
    } else {
        0
    }
}
// upstream: intel_fb.c intel_fb_vtd_guard()
pub fn intel_fb_vtd_guard(fb: &Framebuffer, hooks: &impl FbHooks) -> u32 {
    if hooks.modifier_allowed(fb.format.format, fb.modifier) {
        hooks.vtd_guard(fb.format.format, fb.modifier)
    } else {
        0
    }
}
// upstream: intel_fb.c intel_fill_fb_info()
pub fn intel_fill_fb_info(
    fb: &mut IntelFramebuffer,
    display: &DisplayInfo,
    hooks: &mut impl FbHooks,
) -> Result<(), i32> {
    let n = fb.base.format.num_planes as usize;
    let tile = intel_tile_size(display);
    let mut max_size = 0u32;
    let (mut gtt_rotated, mut gtt_remapped) = (0u32, 0u32);
    let fb_snapshot = *fb;
    intel_fb_view_init(
        &mut fb.normal_view,
        ViewKind::Normal,
        &fb_snapshot,
        display,
        hooks,
    );
    let rotate = intel_fb_supports_90_270_rotation(&fb.base, display);
    let remap = intel_fb_needs_pot_stride_remap(fb, display, hooks);
    if rotate && remap {
        hooks.warn("rotated and POT-remapped views are mutually exclusive")
    }
    if rotate {
        intel_fb_view_init(
            &mut fb.rotated_view,
            ViewKind::Rotated,
            &fb_snapshot,
            display,
            hooks,
        )
    }
    if remap {
        intel_fb_view_init(
            &mut fb.remapped_view,
            ViewKind::Remapped,
            &fb_snapshot,
            display,
            hooks,
        )
    }
    for i in 0..n {
        let plane = i as i32;
        if is_gen12_ccs_cc_plane(&fb.base, plane, hooks) {
            let off = fb.base.offsets[i];
            if off % 64 != 0 {
                hooks.debug("misaligned clear-color plane offset");
                return Err(-22);
            }
            let end = off.checked_add(64).ok_or(-22)?;
            max_size = max(max_size, ceil_div(end, tile));
            continue;
        }
        let (width, height) = intel_fb_plane_dims(fb, plane, hooks);
        let (mut x, mut y) = convert_plane_offset_to_xy(fb, plane, width, display, hooks)?;
        let dims = init_plane_view_dims(fb, plane, width, height, display, hooks);
        fb.normal_view.color_plane[i] = PlaneLayout {
            x,
            y,
            mapping_stride: fb.base.pitches[i],
            scanout_stride: fb.base.pitches[i],
        };
        let offset = calc_plane_aligned_offset(fb, plane, &mut x, &mut y, display, hooks);
        if rotate {
            gtt_rotated = gtt_rotated.wrapping_add(calc_plane_remap_info(
                &fb_snapshot,
                plane,
                &dims,
                offset,
                gtt_rotated,
                x,
                y,
                &mut fb.rotated_view,
                display,
                hooks,
            ));
        }
        if remap {
            gtt_remapped = gtt_remapped.wrapping_add(calc_plane_remap_info(
                &fb_snapshot,
                plane,
                &dims,
                offset,
                gtt_remapped,
                x,
                y,
                &mut fb.remapped_view,
                display,
                hooks,
            ));
        }
        max_size = max(
            max_size,
            offset.wrapping_add(calc_plane_normal_size(
                fb, plane, &dims, x, y, display, hooks,
            )),
        );
    }
    if (max_size as u64) * (tile as u64) > fb.base.size {
        hooks.debug("framebuffer view exceeds backing object size");
        return Err(-22);
    }
    fb.min_alignment = intel_fb_min_alignment(&fb.base, hooks);
    fb.vtd_guard = intel_fb_vtd_guard(&fb.base, hooks);
    Ok(())
}
// upstream: intel_fb.c intel_fb_view_vtd_guard()
pub fn intel_fb_view_vtd_guard(
    fb: &IntelFramebuffer,
    view: &FbView,
    rotation: u32,
    display: &DisplayInfo,
    hooks: &mut impl FbHooks,
) -> u32 {
    let mut guard = fb.vtd_guard;
    if guard == 0 {
        return 0;
    }
    for p in 0..fb.base.format.num_planes as usize {
        if intel_fb_is_ccs_aux_plane(&fb.base, p as i32, hooks)
            || is_gen12_ccs_cc_plane(&fb.base, p as i32, hooks)
        {
            continue;
        }
        let stride = view.color_plane[p].mapping_stride;
        let tile = if rotation & (ROTATE_90 | ROTATE_270) != 0 {
            intel_tile_height(&fb.base, p as i32, display, hooks)
        } else {
            intel_tile_width_bytes(&fb.base, p as i32, display, hooks)
        };
        guard = max(guard, ceil_div(stride, tile))
    }
    guard
}
// upstream: intel_fb.c intel_plane_remap_gtt()
pub fn intel_plane_remap_gtt(
    state: &mut PlaneState,
    fb: &IntelFramebuffer,
    display: &DisplayInfo,
    hooks: &mut impl FbHooks,
) {
    let rotated = state.rotation & (ROTATE_90 | ROTATE_270) != 0;
    if intel_fb_is_ccs_modifier(state.fb.modifier, hooks) {
        hooks.warn("CCS framebuffer was passed to the remapping path");
    }
    intel_fb_view_init(
        &mut state.view,
        if rotated {
            ViewKind::Rotated
        } else {
            ViewKind::Remapped
        },
        fb,
        display,
        hooks,
    );
    let src_x = (state.src.x1 >> 16) as u32;
    let src_y = (state.src.y1 >> 16) as u32;
    let src_w = (state.src.width() >> 16) as u32;
    let src_h = (state.src.height() >> 16) as u32;
    // Match drm_rect_translate() followed by drm_rect_rotate(..., ROTATE_270).
    let dx = (src_x as i32).wrapping_shl(16).wrapping_neg();
    let dy = (src_y as i32).wrapping_shl(16).wrapping_neg();
    state.src.translate(dx, dy);
    if rotated {
        state.src.rotate_270(
            (src_w as i32).wrapping_shl(16),
            (src_h as i32).wrapping_shl(16),
        );
    }
    let mut gtt_offset = 0u32;
    for i in 0..state.fb.format.num_planes as usize {
        let hs = if i == 0 {
            1
        } else {
            state.fb.format.hsub as u32
        };
        let vs = if i == 0 {
            1
        } else {
            state.fb.format.vsub as u32
        };
        let x = (src_x / hs).wrapping_add(fb.normal_view.color_plane[i].x as u32);
        let y = (src_y / vs).wrapping_add(fb.normal_view.color_plane[i].y as u32);
        let w = src_w / hs;
        let h = src_h / vs;
        let dims = init_plane_view_dims(fb, i as i32, w, h, display, hooks);
        let (mut x, mut y) = (x as i32, y as i32);
        let offset = calc_plane_aligned_offset(fb, i as i32, &mut x, &mut y, display, hooks);
        let size = calc_plane_remap_info(
            fb,
            i as i32,
            &dims,
            offset,
            gtt_offset,
            x,
            y,
            &mut state.view,
            display,
            hooks,
        );
        gtt_offset = gtt_offset.wrapping_add(size);
    }
}
// upstream: intel_fb.c intel_rotation_info_size()
pub fn intel_rotation_info_size(view: &FbView) -> u32 {
    view.remapped.iter().fold(0u32, |size, p| {
        size.wrapping_add((p.dst_stride as u32).wrapping_mul(p.width as u32))
    })
}
// upstream: intel_fb.c intel_remapped_info_size()
pub fn intel_remapped_info_size(view: &FbView) -> u32 {
    let mut size = 0;
    for p in &view.remapped {
        let ps = if p.linear {
            p.size
        } else {
            (p.dst_stride as u32).wrapping_mul(p.height as u32)
        };
        if ps == 0 {
            continue;
        }
        if view.plane_alignment != 0 {
            size = align(size, view.plane_alignment)
        }
        size = size.wrapping_add(ps)
    }
    size
}
// upstream: intel_fb.c intel_fb_fill_view()
pub fn intel_fb_fill_view(
    fb: &IntelFramebuffer,
    rotation: u32,
    view: &mut FbView,
    display: &DisplayInfo,
    hooks: &impl FbHooks,
) {
    *view = if rotation & (ROTATE_90 | ROTATE_270) != 0 {
        fb.rotated_view
    } else if intel_fb_needs_pot_stride_remap(fb, display, hooks) {
        fb.remapped_view
    } else {
        fb.normal_view
    }
}
// upstream: intel_fb.c intel_fb_xy_to_linear()
pub fn intel_fb_xy_to_linear(x: i32, y: i32, state: &PlaneState, color_plane: i32) -> u32 {
    (y as u32)
        .wrapping_mul(state.view.color_plane[color_plane as usize].mapping_stride)
        .wrapping_add((x as u32).wrapping_mul(state.fb.format.cpp[color_plane as usize] as u32))
}
// upstream: intel_fb.c intel_add_fb_offsets()
pub fn intel_add_fb_offsets(x: &mut i32, y: &mut i32, state: &PlaneState, color_plane: i32) {
    *x += state.view.color_plane[color_plane as usize].x;
    *y += state.view.color_plane[color_plane as usize].y
}
// upstream: intel_fb.c intel_fb_max_stride()
pub fn intel_fb_max_stride(
    display: &DisplayInfo,
    info: &FormatInfo,
    modifier: u64,
    hooks: &impl FbHooks,
) -> u32 {
    if display.display_ver < 4
        || intel_fb_is_ccs_modifier(modifier, &mut NullHooks)
        || intel_fb_modifier_uses_dpt(display, modifier, hooks)
    {
        hooks.intel_plane_fb_max_stride(info.format, modifier)
    } else if display.display_ver >= 7 {
        256 * 1024
    } else {
        128 * 1024
    }
}
// upstream: intel_fb.c intel_fb_stride_alignment()
pub fn intel_fb_stride_alignment(
    fb: &Framebuffer,
    color_plane: i32,
    display: &DisplayInfo,
    hooks: &mut impl FbHooks,
) -> u32 {
    if is_surface_linear(fb, color_plane, hooks) {
        let max_stride = hooks.intel_plane_fb_max_stride(fb.format.format, fb.modifier);
        if fb.pitches[color_plane as usize] > max_stride
            && !intel_fb_is_ccs_modifier(fb.modifier, hooks)
        {
            intel_tile_size(display)
        } else {
            64
        }
    } else {
        let mut tw = intel_tile_width_bytes(fb, color_plane, display, hooks);
        if intel_fb_is_ccs_modifier(fb.modifier, hooks) {
            if display.display_ver >= 12 {
                tw *= 4
            } else if (display.display_ver == 9 || display.geminilake)
                && color_plane == 0
                && fb.width > 3840
            {
                tw *= 4
            }
        }
        tw
    }
}
// upstream: intel_fb.c intel_plane_check_stride()
pub fn intel_plane_check_stride(
    state: &PlaneState,
    display: &DisplayInfo,
    hooks: &mut impl FbHooks,
) -> Result<(), i32> {
    if intel_plane_can_remap(state, display, hooks) && !state.visible {
        return Ok(());
    }
    let stride = state.view.color_plane[0].mapping_stride;
    let max_stride = hooks.max_stride(state.fb.format.format, state.fb.modifier, state.rotation);
    if stride > max_stride {
        hooks.debug("plane stride exceeds maximum");
        Err(-22)
    } else {
        Ok(())
    }
}
// upstream: intel_fb.c intel_plane_compute_gtt()
pub fn intel_plane_compute_gtt(
    state: &mut PlaneState,
    fb: Option<&IntelFramebuffer>,
    display: &DisplayInfo,
    hooks: &mut impl FbHooks,
) -> Result<(), i32> {
    let fb = match fb {
        Some(fb) => fb,
        None => return Ok(()),
    };
    if intel_plane_needs_remap(state, fb, display, hooks) {
        intel_plane_remap_gtt(state, fb, display, hooks);
        return intel_plane_check_stride(state, display, hooks);
    }
    intel_fb_fill_view(fb, state.rotation, &mut state.view, display, hooks);
    if state.rotation & (ROTATE_90 | ROTATE_270) != 0 {
        state.src.rotate_270(
            (fb.base.width as i32).wrapping_shl(16),
            (fb.base.height as i32).wrapping_shl(16),
        );
    }
    intel_plane_check_stride(state, display, hooks)
}
// upstream: intel_fb.c intel_user_framebuffer_destroy()
pub fn intel_user_framebuffer_destroy(
    fb: &Framebuffer,
    display: &DisplayInfo,
    hooks: &mut impl FbLifecycle,
) {
    hooks.framebuffer_cleanup();
    if intel_fb_uses_dpt(fb, display, hooks) {
        hooks.dpt_destroy()
    }
    hooks.bo_framebuffer_fini();
    hooks.frontbuffer_put();
    hooks.free_panic()
}
// upstream: intel_fb.c intel_user_framebuffer_create_handle()
pub fn intel_user_framebuffer_create_handle(
    userptr: bool,
    hooks: &mut impl FbLifecycle,
) -> Result<u32, i32> {
    if userptr {
        hooks.debug("attempting to use a userptr for a framebuffer, denied");
        return Err(-22);
    }
    hooks.gem_handle_create()
}
// upstream: intel_fb.c intel_user_framebuffer_fence_wake()
pub fn intel_user_framebuffer_fence_wake(hooks: &mut impl FbLifecycle) {
    hooks.frontbuffer_queue_flush();
    hooks.free_fence_callback();
    hooks.fence_put()
}
// upstream: intel_fb.c intel_user_framebuffer_dirty()
pub fn intel_user_framebuffer_dirty(hooks: &mut impl FbLifecycle) -> Result<(), i32> {
    if hooks.frontbuffer_bits() == 0 {
        return Ok(());
    }
    if hooks.reservation_signaled() {
        hooks.frontbuffer_flush_dirty();
        return Ok(());
    }
    match hooks.reservation_singleton() {
        Err(e) => {
            hooks.frontbuffer_flush_dirty();
            return Err(e);
        }
        Ok(false) => {
            hooks.frontbuffer_flush_dirty();
            return Ok(());
        }
        Ok(true) => {}
    }
    if !hooks.alloc_fence_callback() {
        hooks.fence_put();
        hooks.frontbuffer_flush_dirty();
        return Err(-12);
    }
    hooks.frontbuffer_invalidate_dirty();
    let ret = hooks.add_fence_callback();
    if ret != 0 {
        intel_user_framebuffer_fence_wake(hooks);
        if ret == -2 {
            return Ok(());
        }
    }
    if ret == 0 { Ok(()) } else { Err(ret) }
}
pub const INTEL_FB_FUNC_NAMES: [&str; 3] = [
    "intel_user_framebuffer_destroy",
    "intel_user_framebuffer_create_handle",
    "intel_user_framebuffer_dirty",
];
// upstream: intel_fb.c intel_framebuffer_init()
pub fn intel_framebuffer_init(
    fb: &mut IntelFramebuffer,
    display: &DisplayInfo,
    mode: &mut FbMode,
    hooks: &mut impl FbLifecycle,
) -> Result<(), i32> {
    if !hooks.alloc_panic() {
        return Err(-12);
    }
    if !hooks.get_frontbuffer() {
        hooks.free_panic();
        return Err(-12);
    }
    if let Err(e) = hooks.bo_framebuffer_init(mode) {
        hooks.put_frontbuffer();
        hooks.free_panic();
        return Err(e);
    }
    if !hooks.any_plane_has_format(mode.format, mode.modifiers[0]) {
        hooks.debug("unsupported framebuffer format/modifier");
        hooks.bo_framebuffer_fini();
        hooks.put_frontbuffer();
        hooks.free_panic();
        return Err(-22);
    }
    let max_stride = intel_fb_max_stride(display, &mode.info, mode.modifiers[0], hooks);
    if mode.pitches[0] > max_stride {
        hooks.debug("framebuffer pitch exceeds maximum");
        hooks.bo_framebuffer_fini();
        hooks.put_frontbuffer();
        hooks.free_panic();
        return Err(-22);
    }
    if mode.offsets[0] != 0 {
        hooks.debug("plane zero offset must be zero");
        hooks.bo_framebuffer_fini();
        hooks.put_frontbuffer();
        hooks.free_panic();
        return Err(-22);
    }
    fb.base.width = mode.width;
    fb.base.height = mode.height;
    fb.base.format = mode.info;
    fb.base.modifier = mode.modifiers[0];
    fb.base.pitches = mode.pitches;
    fb.base.offsets = mode.offsets;
    fb.base.size = mode.object_size;
    fb.base.gem_object = mode.gem_object;
    fb.base.object_tiled = mode.object_tiled;
    for i in 0..fb.base.format.num_planes as usize {
        if mode.handles[i] != mode.handles[0] {
            hooks.debug("framebuffer planes use distinct GEM handles");
            hooks.bo_framebuffer_fini();
            hooks.put_frontbuffer();
            hooks.free_panic();
            return Err(-22);
        }
        let a = intel_fb_stride_alignment(&fb.base, i as i32, display, hooks);
        if a != 0 && fb.base.pitches[i] & (a - 1) != 0 {
            hooks.debug("framebuffer pitch alignment invalid");
            hooks.bo_framebuffer_fini();
            hooks.put_frontbuffer();
            hooks.free_panic();
            return Err(-22);
        }
        if intel_fb_is_gen12_ccs_aux_plane(&fb.base, i as i32, hooks) {
            let required = gen12_ccs_aux_stride(&fb.base, i as i32, display, hooks);
            if fb.base.pitches[i] != required {
                hooks.debug("CCS auxiliary pitch invalid");
                hooks.bo_framebuffer_fini();
                hooks.put_frontbuffer();
                hooks.free_panic();
                return Err(-22);
            }
        }
    }
    if let Err(e) = intel_fill_fb_info(fb, display, hooks) {
        hooks.bo_framebuffer_fini();
        hooks.put_frontbuffer();
        hooks.free_panic();
        return Err(e);
    }
    if intel_fb_uses_dpt(&fb.base, display, hooks) {
        let size = if intel_fb_needs_pot_stride_remap(fb, display, hooks) {
            intel_remapped_info_size(&fb.remapped_view)
        } else {
            0
        };
        if let Err(e) = hooks.dpt_create(size) {
            hooks.put_frontbuffer();
            hooks.free_panic();
            return Err(e);
        }
    }
    if let Err(e) = hooks.framebuffer_register() {
        if intel_fb_uses_dpt(&fb.base, display, hooks) {
            hooks.dpt_destroy();
        }
        hooks.bo_framebuffer_fini();
        hooks.put_frontbuffer();
        hooks.free_panic();
        return Err(e);
    }
    Ok(())
}
// upstream: intel_fb.c intel_user_framebuffer_create()
pub fn intel_user_framebuffer_create(
    mode: &mut FbMode,
    hooks: &mut impl FbLifecycle,
) -> Result<IntelFramebuffer, i32> {
    hooks.lookup_framebuffer(mode)?;
    let result = intel_framebuffer_create(mode, hooks);
    hooks.object_put();
    result
}
// upstream: intel_fb.c intel_framebuffer_alloc()
pub fn intel_framebuffer_alloc(hooks: &mut impl FbLifecycle) -> Option<IntelFramebuffer> {
    if hooks.allocate_framebuffer() {
        Some(IntelFramebuffer::default())
    } else {
        None
    }
}
// upstream: intel_fb.c intel_framebuffer_create()
pub fn intel_framebuffer_create(
    mode: &mut FbMode,
    hooks: &mut impl FbLifecycle,
) -> Result<IntelFramebuffer, i32> {
    let mut fb = intel_framebuffer_alloc(hooks).ok_or(-12)?;
    let display = mode.display;
    if let Err(e) = intel_framebuffer_init(&mut fb, &display, mode, hooks) {
        hooks.free_framebuffer();
        return Err(e);
    }
    Ok(fb)
}
// upstream: intel_fb.c intel_fb_bo()
pub fn intel_fb_bo(fb: Option<&Framebuffer>) -> Option<u64> {
    fb.and_then(|fb| fb.gem_object)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FbMode {
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub info: FormatInfo,
    pub modifiers: [u64; 4],
    pub pitches: [u32; 4],
    pub offsets: [u32; 4],
    pub handles: [u32; 4],
    pub object_size: u64,
    pub display: DisplayInfo,
    pub gem_object: Option<u64>,
    pub object_tiled: bool,
}
pub trait FbLifecycle: FbHooks {
    fn allocate_framebuffer(&mut self) -> bool {
        true
    }
    fn free_framebuffer(&mut self) {}
    fn alloc_panic(&mut self) -> bool {
        true
    }
    fn free_panic(&mut self) {}
    fn get_frontbuffer(&mut self) -> bool {
        true
    }
    fn put_frontbuffer(&mut self) {}
    fn bo_framebuffer_init(&mut self, _mode: &FbMode) -> Result<(), i32> {
        Ok(())
    }
    fn bo_framebuffer_fini(&mut self) {}
    fn any_plane_has_format(&self, _format: u32, _modifier: u64) -> bool {
        true
    }
    fn dpt_create(&mut self, _size: u32) -> Result<(), i32> {
        Ok(())
    }
    fn dpt_destroy(&mut self) {}
    fn framebuffer_register(&mut self) -> Result<(), i32> {
        Ok(())
    }
    fn lookup_framebuffer(&mut self, _mode: &FbMode) -> Result<(), i32> {
        Ok(())
    }
    fn object_put(&mut self) {}
    fn framebuffer_cleanup(&mut self) {}
    fn frontbuffer_put(&mut self) {}
    fn free_fence_callback(&mut self) {}
    fn gem_handle_create(&mut self) -> Result<u32, i32> {
        Ok(0)
    }
}
struct NullHooks;
impl FbHooks for NullHooks {}

#[cfg(test)]
mod tests {
    use super::{FixedRect, align, ceil_div, round_down};

    #[test]
    fn source_rect_rotation_keeps_16_16_fractional_bits() {
        let mut rect = FixedRect {
            x1: (100 << 16) + 1,
            y1: (200 << 16) + 2,
            x2: (400 << 16) + 3,
            y2: (500 << 16) + 4,
        };
        let src_w = rect.width() >> 16;
        let src_h = rect.height() >> 16;
        rect.translate(-(100_i32 << 16), -(200_i32 << 16));
        rect.rotate_270(src_w << 16, src_h << 16);
        assert_eq!(rect.x1, -4);
        assert_eq!(rect.x2, (300 << 16) - 2);
        assert_eq!(rect.y1, 1);
        assert_eq!(rect.y2, (300 << 16) + 3);
    }

    #[test]
    fn u32_helpers_follow_unsigned_c_macro_overflow() {
        assert_eq!(ceil_div(u32::MAX, 2), 0);
        assert_eq!(align(u32::MAX, 4096), 0);
        assert_eq!(round_down(u32::MAX, 4096), 0xffff_f000);
    }
}
