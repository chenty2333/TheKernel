// SPDX-License-Identifier: MIT
// Copyright © 2020-2021 Intel Corporation.
// Source: drivers/gpu/drm/i915/display/intel_fb.c
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::{
    ffi::{c_int, c_void},
    mem::{MaybeUninit, offset_of, size_of},
    ptr,
    sync::atomic::{AtomicI32, AtomicU8, Ordering},
};

use tk_drm_upstream::{
    drm_device_types::DrmDevice,
    drm_file_types::DrmFile,
    drm_fourcc_types::{DrmFormatInfo, DrmFormatInfoAnon6},
    drm_framebuffer_types::{DrmFramebuffer, DrmFramebufferFuncs},
    drm_gem_types::DrmGemObject,
    drm_plane_types::{DrmPlane, DrmPlaneState},
    uapi_drm_consts::*,
    uapi_drm_mode_types::DrmModeFbCmd2,
    uapi_drm_types::DrmClipRect,
};

use tk_linux_kapi::dma_resv::dma_resv_test_signaled;
use crate::{
    i915_gem_tiling_upstream::{I915_TILING_NONE, I915_TILING_X, I915_TILING_Y},
    i915_gtt_view_types_upstream::{
        I915_GTT_VIEW_NORMAL, I915_GTT_VIEW_REMAPPED, I915_GTT_VIEW_ROTATED,
    },
    intel_bo_upstream::{
        intel_bo_framebuffer_fini, intel_bo_framebuffer_init, intel_bo_framebuffer_lookup,
        intel_bo_is_tiled, intel_bo_is_userptr,
    },
    intel_context_upstream::{DmaFence, DmaFenceCb},
    intel_display_conversion_upstream::__drm_to_display,
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_reg_defs_upstream::DISPLAY_VER,
    intel_display_types_upstream::{
        I915ColorPlaneView, IntelFbView, IntelFramebuffer, IntelFramebufferAnon336x3, IntelPlane,
        IntelPlaneState,
    },
    intel_frontbuffer_types_upstream::IntelFrontbuffer,
    intel_parent_upstream::{
        intel_parent_dpt_create, intel_parent_dpt_destroy, intel_parent_frontbuffer_get,
        intel_parent_frontbuffer_put, intel_parent_frontbuffer_ref, intel_parent_has_auxccs,
        intel_parent_has_fenced_regions, intel_parent_panic_alloc,
    },
    intel_workarounds_types_upstream::I915RegT,
    linux::{
        gem::dma_resv_get_singleton,
        memory::{kfree, kmalloc_array, kmalloc_obj, kzalloc_obj},
        requests::dma_fence_put,
    },
    linux_config::{DMA_RESV_USAGE_KERNEL, EINVAL, ENODEV, ENOMEM, GFP_KERNEL},
};

pub const INTEL_PLANE_CAP_NONE: u8 = 0;
pub const INTEL_PLANE_CAP_CCS_RC: u8 = 1 << 0;
pub const INTEL_PLANE_CAP_CCS_RC_CC: u8 = 1 << 1;
pub const INTEL_PLANE_CAP_CCS_MC: u8 = 1 << 2;
pub const INTEL_PLANE_CAP_TILING_X: u8 = 1 << 3;
pub const INTEL_PLANE_CAP_TILING_Y: u8 = 1 << 4;
pub const INTEL_PLANE_CAP_TILING_YF: u8 = 1 << 5;
pub const INTEL_PLANE_CAP_TILING_4: u8 = 1 << 6;
pub const INTEL_PLANE_CAP_NEED64K_PHYS: u8 = 1 << 7;

pub const I915_FORMAT_MOD_X_TILED: u64 =
    tk_drm_upstream::uapi_drm_macros::fourcc_mod_code(DRM_FORMAT_MOD_VENDOR_INTEL, 1);
pub const I915_FORMAT_MOD_Y_TILED: u64 =
    tk_drm_upstream::uapi_drm_macros::fourcc_mod_code(DRM_FORMAT_MOD_VENDOR_INTEL, 2);
pub const I915_FORMAT_MOD_YF_TILED: u64 =
    tk_drm_upstream::uapi_drm_macros::fourcc_mod_code(DRM_FORMAT_MOD_VENDOR_INTEL, 3);
pub const I915_FORMAT_MOD_Y_TILED_CCS: u64 =
    tk_drm_upstream::uapi_drm_macros::fourcc_mod_code(DRM_FORMAT_MOD_VENDOR_INTEL, 4);
pub const I915_FORMAT_MOD_YF_TILED_CCS: u64 =
    tk_drm_upstream::uapi_drm_macros::fourcc_mod_code(DRM_FORMAT_MOD_VENDOR_INTEL, 5);
pub const I915_FORMAT_MOD_Y_TILED_GEN12_RC_CCS: u64 =
    tk_drm_upstream::uapi_drm_macros::fourcc_mod_code(DRM_FORMAT_MOD_VENDOR_INTEL, 6);
pub const I915_FORMAT_MOD_Y_TILED_GEN12_MC_CCS: u64 =
    tk_drm_upstream::uapi_drm_macros::fourcc_mod_code(DRM_FORMAT_MOD_VENDOR_INTEL, 7);
pub const I915_FORMAT_MOD_Y_TILED_GEN12_RC_CCS_CC: u64 =
    tk_drm_upstream::uapi_drm_macros::fourcc_mod_code(DRM_FORMAT_MOD_VENDOR_INTEL, 8);
pub const I915_FORMAT_MOD_4_TILED: u64 =
    tk_drm_upstream::uapi_drm_macros::fourcc_mod_code(DRM_FORMAT_MOD_VENDOR_INTEL, 9);
pub const I915_FORMAT_MOD_4_TILED_DG2_RC_CCS: u64 =
    tk_drm_upstream::uapi_drm_macros::fourcc_mod_code(DRM_FORMAT_MOD_VENDOR_INTEL, 10);
pub const I915_FORMAT_MOD_4_TILED_DG2_MC_CCS: u64 =
    tk_drm_upstream::uapi_drm_macros::fourcc_mod_code(DRM_FORMAT_MOD_VENDOR_INTEL, 11);
pub const I915_FORMAT_MOD_4_TILED_DG2_RC_CCS_CC: u64 =
    tk_drm_upstream::uapi_drm_macros::fourcc_mod_code(DRM_FORMAT_MOD_VENDOR_INTEL, 12);
pub const I915_FORMAT_MOD_4_TILED_MTL_RC_CCS: u64 =
    tk_drm_upstream::uapi_drm_macros::fourcc_mod_code(DRM_FORMAT_MOD_VENDOR_INTEL, 13);
pub const I915_FORMAT_MOD_4_TILED_MTL_MC_CCS: u64 =
    tk_drm_upstream::uapi_drm_macros::fourcc_mod_code(DRM_FORMAT_MOD_VENDOR_INTEL, 14);
pub const I915_FORMAT_MOD_4_TILED_MTL_RC_CCS_CC: u64 =
    tk_drm_upstream::uapi_drm_macros::fourcc_mod_code(DRM_FORMAT_MOD_VENDOR_INTEL, 15);
pub const I915_FORMAT_MOD_4_TILED_LNL_CCS: u64 =
    tk_drm_upstream::uapi_drm_macros::fourcc_mod_code(DRM_FORMAT_MOD_VENDOR_INTEL, 16);
pub const I915_FORMAT_MOD_4_TILED_BMG_CCS: u64 =
    tk_drm_upstream::uapi_drm_macros::fourcc_mod_code(DRM_FORMAT_MOD_VENDOR_INTEL, 17);

const INTEL_PLANE_CAP_CCS_MASK: u8 =
    INTEL_PLANE_CAP_CCS_RC | INTEL_PLANE_CAP_CCS_RC_CC | INTEL_PLANE_CAP_CCS_MC;
const INTEL_PLANE_CAP_TILING_MASK: u8 = INTEL_PLANE_CAP_TILING_X
    | INTEL_PLANE_CAP_TILING_Y
    | INTEL_PLANE_CAP_TILING_YF
    | INTEL_PLANE_CAP_TILING_4;

#[derive(Clone, Copy)]
struct ModifierDesc {
    modifier: u64,
    display_ver_from: u8,
    display_ver_until: u8,
    plane_caps: u8,
    cc_planes: u8,
    packed_aux_planes: u8,
    planar_aux_planes: u8,
    format_variant: i8,
}

const NO_FORMAT_OVERRIDE: i8 = -1;
const MODIFIERS: [ModifierDesc; 18] = [
    ModifierDesc {
        modifier: I915_FORMAT_MOD_4_TILED_LNL_CCS,
        display_ver_from: 20,
        display_ver_until: u8::MAX,
        plane_caps: INTEL_PLANE_CAP_TILING_4,
        cc_planes: 0,
        packed_aux_planes: 0,
        planar_aux_planes: 0,
        format_variant: NO_FORMAT_OVERRIDE,
    },
    ModifierDesc {
        modifier: I915_FORMAT_MOD_4_TILED_BMG_CCS,
        display_ver_from: 14,
        display_ver_until: u8::MAX,
        plane_caps: INTEL_PLANE_CAP_TILING_4 | INTEL_PLANE_CAP_NEED64K_PHYS,
        cc_planes: 0,
        packed_aux_planes: 0,
        planar_aux_planes: 0,
        format_variant: NO_FORMAT_OVERRIDE,
    },
    ModifierDesc {
        modifier: I915_FORMAT_MOD_4_TILED_MTL_MC_CCS,
        display_ver_from: 14,
        display_ver_until: 14,
        plane_caps: INTEL_PLANE_CAP_TILING_4 | INTEL_PLANE_CAP_CCS_MC,
        cc_planes: 0,
        packed_aux_planes: 1 << 1,
        planar_aux_planes: (1 << 2) | (1 << 3),
        format_variant: 1,
    },
    ModifierDesc {
        modifier: I915_FORMAT_MOD_4_TILED_MTL_RC_CCS,
        display_ver_from: 14,
        display_ver_until: 14,
        plane_caps: INTEL_PLANE_CAP_TILING_4 | INTEL_PLANE_CAP_CCS_RC,
        cc_planes: 0,
        packed_aux_planes: 1 << 1,
        planar_aux_planes: 0,
        format_variant: 1,
    },
    ModifierDesc {
        modifier: I915_FORMAT_MOD_4_TILED_MTL_RC_CCS_CC,
        display_ver_from: 14,
        display_ver_until: 14,
        plane_caps: INTEL_PLANE_CAP_TILING_4 | INTEL_PLANE_CAP_CCS_RC_CC,
        cc_planes: 1 << 2,
        packed_aux_planes: 1 << 1,
        planar_aux_planes: 0,
        format_variant: 2,
    },
    ModifierDesc {
        modifier: I915_FORMAT_MOD_4_TILED_DG2_MC_CCS,
        display_ver_from: 13,
        display_ver_until: 13,
        plane_caps: INTEL_PLANE_CAP_TILING_4 | INTEL_PLANE_CAP_CCS_MC,
        cc_planes: 0,
        packed_aux_planes: 0,
        planar_aux_planes: 0,
        format_variant: NO_FORMAT_OVERRIDE,
    },
    ModifierDesc {
        modifier: I915_FORMAT_MOD_4_TILED_DG2_RC_CCS_CC,
        display_ver_from: 13,
        display_ver_until: 13,
        plane_caps: INTEL_PLANE_CAP_TILING_4 | INTEL_PLANE_CAP_CCS_RC_CC,
        cc_planes: 1 << 1,
        packed_aux_planes: 0,
        planar_aux_planes: 0,
        format_variant: 3,
    },
    ModifierDesc {
        modifier: I915_FORMAT_MOD_4_TILED_DG2_RC_CCS,
        display_ver_from: 13,
        display_ver_until: 13,
        plane_caps: INTEL_PLANE_CAP_TILING_4 | INTEL_PLANE_CAP_CCS_RC,
        cc_planes: 0,
        packed_aux_planes: 0,
        planar_aux_planes: 0,
        format_variant: NO_FORMAT_OVERRIDE,
    },
    ModifierDesc {
        modifier: I915_FORMAT_MOD_4_TILED,
        display_ver_from: 13,
        display_ver_until: u8::MAX,
        plane_caps: INTEL_PLANE_CAP_TILING_4,
        cc_planes: 0,
        packed_aux_planes: 0,
        planar_aux_planes: 0,
        format_variant: NO_FORMAT_OVERRIDE,
    },
    ModifierDesc {
        modifier: I915_FORMAT_MOD_Y_TILED_GEN12_MC_CCS,
        display_ver_from: 12,
        display_ver_until: 13,
        plane_caps: INTEL_PLANE_CAP_TILING_Y | INTEL_PLANE_CAP_CCS_MC,
        cc_planes: 0,
        packed_aux_planes: 1 << 1,
        planar_aux_planes: (1 << 2) | (1 << 3),
        format_variant: 1,
    },
    ModifierDesc {
        modifier: I915_FORMAT_MOD_Y_TILED_GEN12_RC_CCS,
        display_ver_from: 12,
        display_ver_until: 13,
        plane_caps: INTEL_PLANE_CAP_TILING_Y | INTEL_PLANE_CAP_CCS_RC,
        cc_planes: 0,
        packed_aux_planes: 1 << 1,
        planar_aux_planes: 0,
        format_variant: 1,
    },
    ModifierDesc {
        modifier: I915_FORMAT_MOD_Y_TILED_GEN12_RC_CCS_CC,
        display_ver_from: 12,
        display_ver_until: 13,
        plane_caps: INTEL_PLANE_CAP_TILING_Y | INTEL_PLANE_CAP_CCS_RC_CC,
        cc_planes: 1 << 2,
        packed_aux_planes: 1 << 1,
        planar_aux_planes: 0,
        format_variant: 2,
    },
    ModifierDesc {
        modifier: I915_FORMAT_MOD_YF_TILED_CCS,
        display_ver_from: 9,
        display_ver_until: 11,
        plane_caps: INTEL_PLANE_CAP_TILING_YF | INTEL_PLANE_CAP_CCS_RC,
        cc_planes: 0,
        packed_aux_planes: 1 << 1,
        planar_aux_planes: 0,
        format_variant: 0,
    },
    ModifierDesc {
        modifier: I915_FORMAT_MOD_Y_TILED_CCS,
        display_ver_from: 9,
        display_ver_until: 11,
        plane_caps: INTEL_PLANE_CAP_TILING_Y | INTEL_PLANE_CAP_CCS_RC,
        cc_planes: 0,
        packed_aux_planes: 1 << 1,
        planar_aux_planes: 0,
        format_variant: 0,
    },
    ModifierDesc {
        modifier: I915_FORMAT_MOD_YF_TILED,
        display_ver_from: 9,
        display_ver_until: 11,
        plane_caps: INTEL_PLANE_CAP_TILING_YF,
        cc_planes: 0,
        packed_aux_planes: 0,
        planar_aux_planes: 0,
        format_variant: NO_FORMAT_OVERRIDE,
    },
    ModifierDesc {
        modifier: I915_FORMAT_MOD_Y_TILED,
        display_ver_from: 9,
        display_ver_until: 13,
        plane_caps: INTEL_PLANE_CAP_TILING_Y,
        cc_planes: 0,
        packed_aux_planes: 0,
        planar_aux_planes: 0,
        format_variant: NO_FORMAT_OVERRIDE,
    },
    ModifierDesc {
        modifier: I915_FORMAT_MOD_X_TILED,
        display_ver_from: 0,
        display_ver_until: 29,
        plane_caps: INTEL_PLANE_CAP_TILING_X,
        cc_planes: 0,
        packed_aux_planes: 0,
        planar_aux_planes: 0,
        format_variant: NO_FORMAT_OVERRIDE,
    },
    ModifierDesc {
        modifier: DRM_FORMAT_MOD_LINEAR,
        display_ver_from: 0,
        display_ver_until: u8::MAX,
        plane_caps: INTEL_PLANE_CAP_NONE,
        cc_planes: 0,
        packed_aux_planes: 0,
        planar_aux_planes: 0,
        format_variant: NO_FORMAT_OVERRIDE,
    },
];

const SKL_CCS_FORMATS: [u32; 8] = [
    DRM_FORMAT_XRGB8888,
    DRM_FORMAT_XBGR8888,
    DRM_FORMAT_ARGB8888,
    DRM_FORMAT_ABGR8888,
    DRM_FORMAT_XRGB2101010,
    DRM_FORMAT_XBGR2101010,
    DRM_FORMAT_ARGB2101010,
    DRM_FORMAT_ABGR2101010,
];
const GEN12_CCS_FORMATS: [u32; 22] = [
    DRM_FORMAT_XRGB8888,
    DRM_FORMAT_XBGR8888,
    DRM_FORMAT_ARGB8888,
    DRM_FORMAT_ABGR8888,
    DRM_FORMAT_XRGB2101010,
    DRM_FORMAT_XBGR2101010,
    DRM_FORMAT_ARGB2101010,
    DRM_FORMAT_ABGR2101010,
    DRM_FORMAT_XRGB16161616F,
    DRM_FORMAT_XBGR16161616F,
    DRM_FORMAT_ARGB16161616F,
    DRM_FORMAT_ABGR16161616F,
    DRM_FORMAT_YUYV,
    DRM_FORMAT_YVYU,
    DRM_FORMAT_UYVY,
    DRM_FORMAT_VYUY,
    DRM_FORMAT_XYUV8888,
    DRM_FORMAT_NV12,
    DRM_FORMAT_P010,
    DRM_FORMAT_P012,
    DRM_FORMAT_P016,
    DRM_FORMAT_INVALID,
];
const GEN12_CC_FORMATS: [u32; 12] = [
    DRM_FORMAT_XRGB8888,
    DRM_FORMAT_XBGR8888,
    DRM_FORMAT_ARGB8888,
    DRM_FORMAT_ABGR8888,
    DRM_FORMAT_XRGB2101010,
    DRM_FORMAT_XBGR2101010,
    DRM_FORMAT_ARGB2101010,
    DRM_FORMAT_ABGR2101010,
    DRM_FORMAT_XRGB16161616F,
    DRM_FORMAT_XBGR16161616F,
    DRM_FORMAT_ARGB16161616F,
    DRM_FORMAT_ABGR16161616F,
];
const FORMAT_CACHE_LEN: usize = 8 + 21 + 12 + 12;
static FORMAT_CACHE_READY: AtomicU8 = AtomicU8::new(0);
static mut FORMAT_CACHE: MaybeUninit<[DrmFormatInfo; FORMAT_CACHE_LEN]> = MaybeUninit::uninit();

#[inline]
fn format_list(variant: i8) -> (&'static [u32], usize) {
    match variant {
        0 => (&SKL_CCS_FORMATS, 0),
        1 => (&GEN12_CCS_FORMATS[..21], 8),
        2 => (&GEN12_CC_FORMATS, 29),
        3 => (&GEN12_CC_FORMATS, 41),
        _ => (&[], 0),
    }
}

unsafe fn initialize_format_override(dst: *mut DrmFormatInfo, format: u32, variant: i8) {
    let source = tk_drm_upstream::drm_fourcc_upstream::drm_format_info(format);
    if source.is_null() {
        return;
    }
    unsafe { ptr::copy_nonoverlapping(source, dst, 1) };
    let cpp0 = unsafe { (*dst)._anon_6.cpp[0] };
    let mut cpp = [0u8; 4];
    let mut block_w = [0u8; 4];
    let mut block_h = [0u8; 4];
    let mut hsub = 1;
    let mut vsub = 1;
    let mut planes = 2;

    if variant == 0 {
        cpp = [cpp0, 1, 0, 0];
        hsub = 8;
        vsub = 16;
    } else if matches!(
        format,
        DRM_FORMAT_NV12 | DRM_FORMAT_P010 | DRM_FORMAT_P012 | DRM_FORMAT_P016
    ) {
        planes = 4;
        hsub = 2;
        vsub = 2;
        if format == DRM_FORMAT_NV12 {
            cpp = [1, 2, 1, 1];
            block_w = [1, 1, 4, 4];
        } else {
            cpp = [2, 4, 1, 1];
            block_w = [1, 1, 2, 2];
        }
        block_h = [1, 1, 1, 1];
    } else if matches!(
        format,
        DRM_FORMAT_YUYV | DRM_FORMAT_YVYU | DRM_FORMAT_UYVY | DRM_FORMAT_VYUY
    ) {
        cpp = [2, 1, 0, 0];
        block_w = [1, 2, 0, 0];
        block_h = [1, 1, 0, 0];
        hsub = 2;
        unsafe { (*dst).is_yuv = true };
    } else if format == DRM_FORMAT_XYUV8888 {
        cpp = [4, 1, 0, 0];
        block_w = [1, 2, 0, 0];
        block_h = [1, 1, 0, 0];
        unsafe { (*dst).is_yuv = true };
    } else if variant == 3 {
        planes = 2;
        cpp = [cpp0, 0, 0, 0];
        block_w = [1, 0, 0, 0];
        block_h = [1, 0, 0, 0];
    } else {
        // RGB and FP16 CCS formats. Plane 1 carries two pixels per block
        // for 32-bit formats and one for FP16, as defined in drm_fourcc.h.
        cpp = [cpp0, 1, 0, 0];
        block_w = [1, if cpp0 == 8 { 1 } else { 2 }, 0, 0];
        block_h = [1, 1, 0, 0];
        if variant == 2 {
            planes = 3;
            cpp[2] = 0;
            block_w[2] = 0;
            block_h[2] = 0;
        }
    }

    unsafe {
        (*dst).num_planes = planes;
        (*dst)._anon_6 = DrmFormatInfoAnon6 {
            char_per_block: cpp,
        };
        (*dst).block_w = block_w;
        (*dst).block_h = block_h;
        (*dst).hsub = hsub;
        (*dst).vsub = vsub;
    }
}

fn ensure_format_override_cache() {
    if FORMAT_CACHE_READY.load(Ordering::Acquire) == 2 {
        return;
    }
    if FORMAT_CACHE_READY
        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        let mut cache = MaybeUninit::<[DrmFormatInfo; FORMAT_CACHE_LEN]>::zeroed();
        let base = cache.as_mut_ptr().cast::<DrmFormatInfo>();
        for variant in 0..4 {
            let (formats, offset) = format_list(variant);
            for (index, format) in formats.iter().copied().enumerate() {
                unsafe {
                    initialize_format_override(base.add(offset + index), format, variant as i8)
                };
            }
        }
        unsafe { ptr::write(ptr::addr_of_mut!(FORMAT_CACHE), cache) };
        FORMAT_CACHE_READY.store(2, Ordering::Release);
    } else {
        while FORMAT_CACHE_READY.load(Ordering::Acquire) != 2 {
            core::hint::spin_loop();
        }
    }
}

unsafe fn format_override(format: u32, variant: i8) -> *const DrmFormatInfo {
    let (formats, offset) = format_list(variant);
    let Some(index) = formats.iter().position(|candidate| *candidate == format) else {
        return ptr::null();
    };
    ensure_format_override_cache();
    let cache = ptr::addr_of!(FORMAT_CACHE).cast::<DrmFormatInfo>();
    let info = unsafe { cache.add(offset + index) };
    if unsafe { (*info).format == format } {
        info
    } else {
        ptr::null()
    }
}

#[inline]
fn modifier_desc_or_null(modifier: u64) -> Option<&'static ModifierDesc> {
    MODIFIERS
        .iter()
        .find(|descriptor| descriptor.modifier == modifier)
}

#[inline]
fn modifier_desc(modifier: u64) -> &'static ModifierDesc {
    modifier_desc_or_null(modifier).unwrap_or_else(|| {
        let _ = tk_linux_kapi::assert::warn_on(true);
        &MODIFIERS[0]
    })
}

#[inline]
fn plane_caps_any(caps: u8, mask: u8) -> bool {
    caps & mask != 0
}

#[inline]
fn plane_caps_all(caps: u8, mask: u8) -> bool {
    caps & mask == mask
}

#[inline]
fn display_platform_bit(display: *const IntelDisplay, bit: usize) -> bool {
    if display.is_null() || bit >= 72 {
        return false;
    }
    unsafe {
        let platform = ptr::addr_of!((*display).platform).cast::<u8>();
        let byte = platform.add(bit / 8).read();
        byte & (1 << (bit & 7)) != 0
    }
}

#[inline]
fn modifier_display_range(desc: &ModifierDesc, ver: u8) -> bool {
    desc.display_ver_from <= ver && ver <= desc.display_ver_until
}

fn plane_has_modifier(display: *mut IntelDisplay, caps: u8, desc: &ModifierDesc) -> bool {
    if display.is_null()
        || !modifier_display_range(desc, unsafe { DISPLAY_VER(display) } as u8)
        || !plane_caps_all(caps, desc.plane_caps)
    {
        return false;
    }
    if plane_caps_any(desc.plane_caps, INTEL_PLANE_CAP_CCS_MASK)
        && unsafe { intel_parent_has_auxccs(display) } != (desc.packed_aux_planes != 0)
    {
        return false;
    }
    if desc.modifier == I915_FORMAT_MOD_4_TILED_BMG_CCS
        && (unsafe { DISPLAY_VER(display) } < 14 || !display_platform_bit(display, 2))
    {
        return false;
    }
    if desc.modifier == I915_FORMAT_MOD_4_TILED_LNL_CCS
        && (unsafe { DISPLAY_VER(display) } < 20 || display_platform_bit(display, 2))
    {
        return false;
    }
    true
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_fb_modifier_to_tiling(modifier: u64) -> u32 {
    let Some(desc) = modifier_desc_or_null(modifier) else {
        return I915_TILING_NONE;
    };
    match desc.plane_caps & INTEL_PLANE_CAP_TILING_MASK {
        INTEL_PLANE_CAP_TILING_Y => I915_TILING_Y,
        INTEL_PLANE_CAP_TILING_X => I915_TILING_X,
        _ => I915_TILING_NONE,
    }
}

#[derive(Clone, Copy)]
struct FbPlaneViewDims {
    width: u32,
    height: u32,
    tile_width: u32,
    tile_height: u32,
}

unsafe fn fb_plane_dims(
    fb: *const IntelFramebuffer,
    color_plane: usize,
) -> Result<(u32, u32), i32> {
    if fb.is_null() || unsafe { (*fb).base.format.is_null() } {
        return Err(-tk_linux_kapi::config::EINVAL);
    }
    let base = unsafe { ptr::addr_of!((*fb).base) };
    let main_plane = if unsafe { intel_fb_is_ccs_aux_plane(base, color_plane as i32) } {
        unsafe { skl_ccs_to_main_plane(base, color_plane as i32) }.max(0) as usize
    } else {
        0
    };
    let mut main_hsub = 1i32;
    let mut main_vsub = 1i32;
    let mut hsub = 1i32;
    let mut vsub = 1i32;
    unsafe {
        intel_fb_plane_get_subsampling(&mut main_hsub, &mut main_vsub, base, main_plane as i32);
        intel_fb_plane_get_subsampling(&mut hsub, &mut vsub, base, color_plane as i32);
    }
    let divisor_w = (main_hsub.max(1) as u32).saturating_mul(hsub.max(1) as u32);
    let divisor_h = (main_vsub.max(1) as u32).saturating_mul(vsub.max(1) as u32);
    let width = div_round_up(unsafe { (*base).width }, divisor_w);
    let height = div_round_up(unsafe { (*base).height }, divisor_h);
    Ok((width, height))
}

unsafe fn fb_check_ccs_xy(fb: *const DrmFramebuffer, color_plane: usize, x: u32, y: u32) -> i32 {
    if !unsafe { intel_fb_is_ccs_aux_plane(fb, color_plane as i32) } {
        return 0;
    }
    let (mut tile_width, mut tile_height) =
        unsafe { intel_tile_block_dims(fb, color_plane as i32) };
    let mut hsub = 1i32;
    let mut vsub = 1i32;
    unsafe { intel_fb_plane_get_subsampling(&mut hsub, &mut vsub, fb, color_plane as i32) };
    tile_width = tile_width.saturating_mul(hsub.max(1) as u32);
    tile_height = tile_height.saturating_mul(vsub.max(1) as u32);
    if tile_width == 0 || tile_height == 0 {
        return -EINVAL;
    }
    let ccs_x = x.saturating_mul(hsub.max(1) as u32) % tile_width;
    let ccs_y = y.saturating_mul(vsub.max(1) as u32) % tile_height;
    let main_plane = unsafe { skl_ccs_to_main_plane(fb, color_plane as i32) }.max(0) as usize;
    let intel_fb = fb.cast::<IntelFramebuffer>();
    let main_x = unsafe { (*intel_fb).normal_view.color_plane[main_plane].x };
    let main_y = unsafe { (*intel_fb).normal_view.color_plane[main_plane].y };
    if main_x % tile_width != ccs_x || main_y % tile_height != ccs_y {
        -EINVAL
    } else {
        0
    }
}

unsafe fn init_fb_view(view: *mut IntelFbView, view_type: u32, remap_alignment: u32) {
    if view.is_null() {
        return;
    }
    unsafe {
        ptr::write_bytes(view.cast::<u8>(), 0, size_of::<IntelFbView>());
        let gtt = view.cast::<u8>().add(offset_of!(IntelFbView, gtt));
        ptr::write_unaligned(gtt.cast::<u32>(), view_type);
        if view_type == I915_GTT_VIEW_REMAPPED {
            let alignment = gtt.add(
                offset_of!(crate::i915_gtt_view_types_upstream::I915GttView, _anon_4_1)
                    + offset_of!(
                        crate::i915_gtt_view_types_upstream::IntelRemappedInfo,
                        plane_alignment
                    ),
            );
            ptr::write_unaligned(alignment.cast::<u32>(), remap_alignment);
        }
    }
}

unsafe fn set_remapped_plane(
    view: *mut IntelFbView,
    plane: usize,
    offset_tiles: u32,
    linear: bool,
    width_tiles: u32,
    height_tiles: u32,
    src_stride_tiles: u32,
    dst_stride_tiles: u32,
    linear_size: u32,
) {
    if view.is_null() || plane >= 4 {
        return;
    }
    let entry = unsafe {
        view.cast::<u8>().add(
            offset_of!(IntelFbView, gtt)
                + offset_of!(crate::i915_gtt_view_types_upstream::I915GttView, _anon_4_1)
                + offset_of!(
                    crate::i915_gtt_view_types_upstream::IntelRemappedInfo,
                    plane
                )
                + plane * size_of::<crate::i915_gtt_view_types_upstream::IntelRemappedPlaneInfo>(),
        )
    };
    let offset = offset_tiles & 0x7fff_ffff;
    let mut bits = offset.to_le_bytes();
    if linear {
        bits[3] |= 0x80;
    }
    unsafe {
        ptr::copy_nonoverlapping(bits.as_ptr(), entry, 4);
        let union = if linear {
            crate::i915_gtt_view_types_upstream::IntelRemappedPlaneInfoAnon4x2 { size: linear_size }
        } else {
            crate::i915_gtt_view_types_upstream::IntelRemappedPlaneInfoAnon4x2 {
                _anon_4_0: core::mem::ManuallyDrop::new(
                    crate::i915_gtt_view_types_upstream::IntelRemappedPlaneInfoAnon4x2Anon4x0 {
                        width: width_tiles.min(u16::MAX as u32) as u16,
                        height: height_tiles.min(u16::MAX as u32) as u16,
                        src_stride: src_stride_tiles.min(u16::MAX as u32) as u16,
                        dst_stride: dst_stride_tiles.min(u16::MAX as u32) as u16,
                    },
                ),
            }
        };
        ptr::write_unaligned(
            entry
                .add(4)
                .cast::<crate::i915_gtt_view_types_upstream::IntelRemappedPlaneInfoAnon4x2>(),
            union,
        );
    }
}

unsafe fn remapped_plane_size(
    info: *const crate::i915_gtt_view_types_upstream::IntelRemappedInfo,
    plane: usize,
) -> u32 {
    if info.is_null() || plane >= 4 {
        return 0;
    }
    let entry = unsafe {
        info.cast::<u8>().add(
            offset_of!(
                crate::i915_gtt_view_types_upstream::IntelRemappedInfo,
                plane
            ) + plane * size_of::<crate::i915_gtt_view_types_upstream::IntelRemappedPlaneInfo>(),
        )
    };
    let bits = unsafe { ptr::read_unaligned(entry.cast::<u32>()) };
    let linear = bits & 0x8000_0000 != 0;
    if linear {
        unsafe { ptr::read_unaligned(entry.add(4).cast::<u32>()) }
    } else {
        let info = unsafe {
            ptr::read_unaligned(entry.add(4).cast::<crate::i915_gtt_view_types_upstream::IntelRemappedPlaneInfoAnon4x2Anon4x0>())
        };
        (info.dst_stride as u32).saturating_mul(info.height as u32)
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rotation_info_size(
    info: *const crate::i915_gtt_view_types_upstream::IntelRotationInfo,
) -> u32 {
    if info.is_null() {
        return 0;
    }
    (0..2).map(|p| unsafe {
        let entry = info.cast::<u8>().add(offset_of!(crate::i915_gtt_view_types_upstream::IntelRotationInfo, plane)
            + p * size_of::<crate::i915_gtt_view_types_upstream::IntelRemappedPlaneInfo>());
        let info = ptr::read_unaligned(entry.add(4).cast::<crate::i915_gtt_view_types_upstream::IntelRemappedPlaneInfoAnon4x2Anon4x0>());
        (info.dst_stride as u32).saturating_mul(info.width as u32)
    }).fold(0u32, u32::saturating_add)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_remapped_info_size(
    info: *const crate::i915_gtt_view_types_upstream::IntelRemappedInfo,
) -> u32 {
    if info.is_null() {
        return 0;
    }
    let alignment_addr = unsafe {
        info.cast::<u8>().add(offset_of!(
            crate::i915_gtt_view_types_upstream::IntelRemappedInfo,
            plane_alignment
        ))
    };
    let alignment = unsafe { ptr::read_unaligned(alignment_addr.cast::<u32>()) };
    let mut size = 0u32;
    for plane in 0..4 {
        let amount = unsafe { remapped_plane_size(info, plane) };
        if amount == 0 {
            continue;
        }
        if alignment != 0 {
            size = div_round_up(size, alignment).saturating_mul(alignment);
        }
        size = size.saturating_add(amount);
    }
    size
}

fn plane_view_dst_stride_tiles(fb: *const IntelFramebuffer, pitch_tiles: u32) -> u32 {
    if unsafe { intel_fb_needs_pot_stride_remap(fb) } {
        pitch_tiles
            .max(8)
            .checked_next_power_of_two()
            .unwrap_or(u32::MAX)
    } else {
        pitch_tiles
    }
}

unsafe fn encode_plane_view(
    fb: *mut IntelFramebuffer,
    view: *mut IntelFbView,
    p: usize,
    dims: FbPlaneViewDims,
    offset_tiles: u32,
    gtt_offset_tiles: u32,
    mut x: u32,
    mut y: u32,
) -> u32 {
    let base = unsafe { ptr::addr_of!((*fb).base) };
    let cpp = unsafe { cpp_for(base, p) };
    let tile_size = unsafe { intel_tile_size(display_from_fb(base)) };
    let linear = unsafe { is_gen12_ccs_aux_plane(base, p as i32) };
    let src_stride_tiles = div_round_up(
        unsafe { (*base).pitches[p] },
        dims.tile_width.saturating_mul(cpp),
    );
    let width_tiles = div_round_up(x.saturating_add(dims.width), dims.tile_width);
    let height_tiles = div_round_up(y.saturating_add(dims.height), dims.tile_height);
    let remap_offset = if unsafe { intel_fb_needs_pot_stride_remap(fb) } {
        div_round_up(gtt_offset_tiles, 512).saturating_mul(512)
    } else {
        gtt_offset_tiles
    };
    let alignment_pad = remap_offset.saturating_sub(gtt_offset_tiles);
    let dst_stride = if linear {
        0
    } else if unsafe {
        intel_fb_needs_pot_stride_remap(fb) && intel_fb_is_ccs_modifier((*base).modifier)
    } {
        src_stride_tiles
    } else {
        plane_view_dst_stride_tiles(
            fb,
            if unsafe { intel_fb_needs_pot_stride_remap(fb) } {
                width_tiles
            } else {
                src_stride_tiles
            },
        )
    };
    let remap_size = if linear {
        let bytes = (y.saturating_add(dims.height))
            .saturating_mul(unsafe { (*base).pitches[p] })
            .saturating_add(x.saturating_mul(cpp));
        div_round_up(bytes, tile_size)
    } else {
        dst_stride.saturating_mul(height_tiles)
    };
    let cp = unsafe { ptr::addr_of_mut!((*view).color_plane[p]) };
    unsafe {
        (*cp).x = x;
        (*cp).y = y;
        (*cp).mapping_stride = if linear {
            (*base).pitches[p]
        } else {
            dst_stride
                .saturating_mul(dims.tile_width)
                .saturating_mul(cpp)
        };
        (*cp).scanout_stride = if linear {
            (*base).pitches[p]
        } else {
            let scanout_tiles =
                if intel_fb_needs_pot_stride_remap(fb) && src_stride_tiles < dst_stride {
                    src_stride_tiles
                } else {
                    dst_stride
                };
            scanout_tiles
                .saturating_mul(dims.tile_width)
                .saturating_mul(cpp)
        };
    }
    unsafe {
        set_remapped_plane(
            view,
            p,
            offset_tiles,
            linear,
            width_tiles,
            height_tiles,
            src_stride_tiles,
            dst_stride,
            remap_size,
        )
    };
    if remap_offset != gtt_offset_tiles {
        if linear {
            unsafe {
                adjust_linear_offset(
                    &mut x,
                    &mut y,
                    cpp,
                    (*base).pitches[p],
                    remap_offset * tile_size,
                    0,
                )
            };
        } else {
            unsafe {
                adjust_tile_offset(
                    &mut x,
                    &mut y,
                    dims.tile_width,
                    dims.tile_height,
                    tile_size,
                    dst_stride,
                    remap_offset * tile_size,
                    0,
                )
            };
        }
        unsafe {
            (*cp).x = x;
            (*cp).y = y;
        }
    }
    alignment_pad.saturating_add(remap_size)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_fill_fb_info(
    display: *mut IntelDisplay,
    fb: *mut IntelFramebuffer,
) -> i32 {
    if display.is_null() || fb.is_null() || unsafe { (*fb).base.format.is_null() } {
        return -tk_linux_kapi::config::EINVAL;
    }
    let base = unsafe { ptr::addr_of_mut!((*fb).base) };
    let obj = unsafe { (*base).obj[0] };
    if obj.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let tile_size = unsafe { intel_tile_size(display) };
    let remap = unsafe { intel_fb_needs_pot_stride_remap(fb) };
    unsafe {
        init_fb_view(
            ptr::addr_of_mut!((*fb).normal_view),
            I915_GTT_VIEW_NORMAL,
            0,
        );
        if remap {
            let remapped = ptr::addr_of_mut!((*fb)._anon_336_3.remapped_view).cast::<IntelFbView>();
            init_fb_view(remapped, I915_GTT_VIEW_REMAPPED, 512);
        }
    }
    let num_planes = unsafe { (*(*base).format).num_planes.min(4) as usize };
    let mut max_tiles = 0u32;
    let mut remapped_offset = 0u32;
    for p in 0..num_planes {
        if unsafe { is_gen12_ccs_cc_plane(base, p as i32) } {
            let offset = unsafe { (*base).offsets[p] };
            let Some(end) = offset.checked_add(64) else {
                return -tk_linux_kapi::config::EINVAL;
            };
            if offset & 63 != 0 {
                return -tk_linux_kapi::config::EINVAL;
            }
            max_tiles = max_tiles.max(div_round_up(end, tile_size));
            continue;
        }
        let Ok((width, height)) = (unsafe { fb_plane_dims(fb, p) }) else {
            return -tk_linux_kapi::config::EINVAL;
        };
        let Ok((mut x, mut y)) = (unsafe { fb_offset_to_xy(base, p) }) else {
            return -tk_linux_kapi::config::EINVAL;
        };
        let cpp = unsafe { cpp_for(base, p) };
        let (tile_width, tile_height) = unsafe { intel_tile_dims(base, p as i32) };
        if cpp == 0 || tile_width == 0 || tile_height == 0 {
            return -tk_linux_kapi::config::EINVAL;
        }
        unsafe {
            (*fb).normal_view.color_plane[p] = I915ColorPlaneView {
                offset: 0,
                x,
                y,
                mapping_stride: (*base).pitches[p],
                scanout_stride: (*base).pitches[p],
            };
        }
        if unsafe { fb_check_ccs_xy(base, p, x, y) } != 0 {
            return -tk_linux_kapi::config::EINVAL;
        }
        if !unsafe { is_surface_linear(base, p as i32) }
            && p == 0
            && x.saturating_add(width).saturating_mul(cpp) > unsafe { (*base).pitches[p] }
        {
            return -tk_linux_kapi::config::EINVAL;
        }
        let aligned = unsafe {
            compute_aligned_offset(base, p, &mut x, &mut y, (*base).pitches[p], 0, tile_size)
        };
        let offset_tiles = aligned / tile_size;
        let dims = FbPlaneViewDims {
            width,
            height,
            tile_width,
            tile_height,
        };
        let Some(end_y) = y.checked_add(height) else {
            return -tk_linux_kapi::config::ERANGE;
        };
        let size = if unsafe { is_surface_linear(base, p as i32) } {
            let Some(bytes) = end_y
                .checked_mul(unsafe { (*base).pitches[p] })
                .and_then(|bytes| {
                    x.checked_mul(cpp)
                        .and_then(|x_bytes| bytes.checked_add(x_bytes))
                })
            else {
                return -tk_linux_kapi::config::ERANGE;
            };
            div_round_up(bytes, tile_size)
        } else {
            let Some(tile_row_bytes) = tile_width.checked_mul(cpp).filter(|bytes| *bytes != 0)
            else {
                return -tk_linux_kapi::config::ERANGE;
            };
            let Some(size) = div_round_up(unsafe { (*base).pitches[p] }, tile_row_bytes)
                .checked_mul(div_round_up(end_y, tile_height))
                .and_then(|size| size.checked_add(u32::from(x != 0)))
            else {
                return -tk_linux_kapi::config::ERANGE;
            };
            size
        };
        let Some(end_tiles) = offset_tiles.checked_add(size) else {
            return -tk_linux_kapi::config::ERANGE;
        };
        max_tiles = max_tiles.max(end_tiles);
        if remap {
            let remapped_view =
                unsafe { ptr::addr_of_mut!((*fb)._anon_336_3.remapped_view).cast::<IntelFbView>() };
            let plane_remap_size = unsafe {
                encode_plane_view(
                    fb,
                    remapped_view,
                    p,
                    dims,
                    offset_tiles,
                    remapped_offset,
                    x,
                    y,
                )
            };
            let Some(next_remapped_offset) = remapped_offset.checked_add(plane_remap_size) else {
                return -tk_linux_kapi::config::ERANGE;
            };
            remapped_offset = next_remapped_offset;
        }
    }
    if let Err(error) = fb_required_bytes(max_tiles, tile_size, unsafe { (*obj).size }) {
        return error;
    }
    unsafe {
        (*fb).min_alignment = fb_min_alignment(base);
        (*fb).vtd_guard = fb_vtd_guard(base);
    }
    0
}

unsafe fn fb_min_alignment(fb: *const DrmFramebuffer) -> u32 {
    if fb.is_null() {
        return 0;
    }
    // The supported plane set is supplied by the Intel display setup; use the
    // per-plane callback for its DRM plane list, while rejecting absent callbacks.
    let display = unsafe { display_from_fb(fb) };
    if display.is_null() {
        return 0;
    }
    let drm = unsafe { (*display).drm };
    if drm.is_null() {
        return 0;
    }
    let mut best = 0u32;
    let format = unsafe { (*fb).format };
    if format.is_null() {
        return 0;
    }
    // `drm->mode_config.plane_list` is the authoritative registry.
    let head = unsafe { ptr::addr_of!((*drm).mode_config.plane_list) };
    let mut node = unsafe { (*head).next };
    while !node.is_null() && node != head.cast_mut() {
        let drm_plane = unsafe {
            node.cast::<u8>()
                .sub(offset_of!(DrmPlane, head))
                .cast::<DrmPlane>()
        };
        if !unsafe {
            tk_drm_upstream::drm_plane_upstream::drm_plane_has_format(
                drm_plane,
                (*format).format,
                (*fb).modifier,
            )
        } {
            node = unsafe { (*node).next };
            continue;
        }
        let plane = drm_plane.cast::<IntelPlane>();
        let device_info = unsafe { (*display).info.__device_info };
        if !device_info.is_null()
            && unsafe { (*plane).id == crate::intel_display_limits_types_upstream::PLANE_CURSOR }
            && unsafe { (*device_info)._bitfields_46[0] & 1 != 0 }
        {
            node = unsafe { (*node).next };
            continue;
        }
        let cb = unsafe { (*plane).min_alignment };
        if let Some(cb) = cb {
            let align = unsafe { cb(plane, fb.cast_mut(), 0) };
            if align.is_power_of_two() {
                best = best.max(align);
            }
        }
        node = unsafe { (*node).next };
    }
    best
}

unsafe fn fb_vtd_guard(fb: *const DrmFramebuffer) -> u32 {
    if fb.is_null() {
        return 0;
    }
    let display = unsafe { display_from_fb(fb) };
    if display.is_null() || unsafe { (*display).drm.is_null() } {
        return 0;
    }
    let drm = unsafe { (*display).drm };
    let head = unsafe { ptr::addr_of!((*drm).mode_config.plane_list) };
    let mut node = unsafe { (*head).next };
    let mut guard = 0u32;
    while !node.is_null() && node != head.cast_mut() {
        let base = unsafe {
            node.cast::<u8>()
                .sub(offset_of!(DrmPlane, head))
                .cast::<DrmPlane>()
        };
        let format = unsafe { (*fb).format };
        if format.is_null()
            || !unsafe {
                tk_drm_upstream::drm_plane_upstream::drm_plane_has_format(
                    base,
                    (*format).format,
                    (*fb).modifier,
                )
            }
        {
            node = unsafe { (*node).next };
            continue;
        }
        let plane = base.cast::<IntelPlane>();
        guard = guard.max(unsafe { (*plane).vtd_guard } as u32);
        node = unsafe { (*node).next };
    }
    guard
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_fb_get_format_info(
    format: u32,
    modifier: u64,
) -> *mut DrmFormatInfo {
    let Some(desc) = modifier_desc_or_null(modifier) else {
        return ptr::null_mut();
    };
    if desc.format_variant < 0 {
        return ptr::null_mut();
    }
    unsafe { format_override(format, desc.format_variant).cast_mut() }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_fb_is_tiled_modifier(modifier: u64) -> bool {
    plane_caps_any(
        modifier_desc(modifier).plane_caps,
        INTEL_PLANE_CAP_TILING_MASK,
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_fb_is_ccs_modifier(modifier: u64) -> bool {
    plane_caps_any(modifier_desc(modifier).plane_caps, INTEL_PLANE_CAP_CCS_MASK)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_fb_is_rc_ccs_cc_modifier(modifier: u64) -> bool {
    plane_caps_any(
        modifier_desc(modifier).plane_caps,
        INTEL_PLANE_CAP_CCS_RC_CC,
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_fb_is_mc_ccs_modifier(modifier: u64) -> bool {
    plane_caps_any(modifier_desc(modifier).plane_caps, INTEL_PLANE_CAP_CCS_MC)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_fb_needs_64k_phys(modifier: u64) -> bool {
    modifier_desc_or_null(modifier)
        .is_some_and(|desc| plane_caps_any(desc.plane_caps, INTEL_PLANE_CAP_NEED64K_PHYS))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_fb_is_tile4_modifier(modifier: u64) -> bool {
    plane_caps_any(modifier_desc(modifier).plane_caps, INTEL_PLANE_CAP_TILING_4)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_fb_plane_get_modifiers(
    display: *mut IntelDisplay,
    plane_caps: u8,
) -> *mut u64 {
    if display.is_null() {
        return ptr::null_mut();
    }
    let count = MODIFIERS
        .iter()
        .filter(|desc| plane_has_modifier(display, plane_caps, desc))
        .count()
        .saturating_add(1);
    let list = unsafe { kmalloc_array(count, size_of::<u64>(), GFP_KERNEL).cast::<u64>() };
    if list.is_null() {
        return ptr::null_mut();
    }
    let mut index = 0;
    for desc in &MODIFIERS {
        if plane_has_modifier(display, plane_caps, desc) {
            unsafe { *list.add(index) = desc.modifier };
            index += 1;
        }
    }
    unsafe { *list.add(index) = DRM_FORMAT_MOD_INVALID };
    list
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_fb_plane_supports_modifier(
    plane: *mut IntelPlane,
    modifier: u64,
) -> bool {
    if plane.is_null() {
        return false;
    }
    let base = unsafe { ptr::addr_of!((*plane).base) };
    let count = unsafe { (*base).modifier_count as usize };
    let modifiers = unsafe { (*base).modifiers };
    !modifiers.is_null() && (0..count).any(|index| unsafe { *modifiers.add(index) == modifier })
}

fn format_is_yuv_semiplanar(desc: &ModifierDesc, info: *const DrmFormatInfo) -> bool {
    if info.is_null() || !unsafe { (*info).is_yuv } {
        return false;
    }
    if desc.planar_aux_planes.count_ones() == 2 {
        unsafe { (*info).num_planes == 4 }
    } else {
        unsafe { (*info).num_planes == 2 }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_format_info_is_yuv_semiplanar(
    info: *const DrmFormatInfo,
    modifier: u64,
) -> bool {
    format_is_yuv_semiplanar(modifier_desc(modifier), info)
}

fn ccs_aux_plane_mask(desc: &ModifierDesc, format: *const DrmFormatInfo) -> u8 {
    if format_is_yuv_semiplanar(desc, format) {
        desc.planar_aux_planes
    } else {
        desc.packed_aux_planes
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_fb_is_ccs_aux_plane(
    fb: *const DrmFramebuffer,
    color_plane: i32,
) -> bool {
    if fb.is_null() || !(0..4).contains(&color_plane) {
        return false;
    }
    let desc = modifier_desc(unsafe { (*fb).modifier });
    ccs_aux_plane_mask(desc, unsafe { (*fb).format }) & (1 << color_plane) != 0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_fb_rc_ccs_cc_plane(fb: *const DrmFramebuffer) -> i32 {
    if fb.is_null() {
        return -1;
    }
    let bits = modifier_desc(unsafe { (*fb).modifier }).cc_planes;
    if bits == 0 {
        -1
    } else {
        bits.trailing_zeros() as i32
    }
}

fn is_gen12_ccs_aux_plane(fb: *const DrmFramebuffer, color_plane: i32) -> bool {
    if fb.is_null() || !(0..4).contains(&color_plane) {
        return false;
    }
    let desc = modifier_desc(unsafe { (*fb).modifier });
    desc.display_ver_from <= 14
        && desc.display_ver_until >= 12
        && ccs_aux_plane_mask(desc, unsafe { (*fb).format }) & (1 << color_plane) != 0
}

fn is_gen12_ccs_cc_plane(fb: *const DrmFramebuffer, color_plane: i32) -> bool {
    unsafe { intel_fb_rc_ccs_cc_plane(fb) == color_plane }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn is_surface_linear(fb: *const DrmFramebuffer, color_plane: i32) -> bool {
    !fb.is_null()
        && (unsafe { (*fb).modifier == DRM_FORMAT_MOD_LINEAR }
            || is_gen12_ccs_aux_plane(fb, color_plane)
            || is_gen12_ccs_cc_plane(fb, color_plane))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn main_to_ccs_plane(fb: *const DrmFramebuffer, main_plane: i32) -> i32 {
    if fb.is_null() || unsafe { (*fb).format.is_null() } {
        return 0;
    }
    let num_planes = unsafe { (*(*fb).format).num_planes as i32 };
    let _ = tk_linux_kapi::assert::warn_on(
        !unsafe { intel_fb_is_ccs_modifier((*fb).modifier) }
            || (main_plane != 0 && main_plane >= num_planes / 2),
    );
    num_planes / 2 + main_plane
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_ccs_to_main_plane(fb: *const DrmFramebuffer, ccs_plane: i32) -> i32 {
    if fb.is_null() || unsafe { (*fb).format.is_null() } {
        return 0;
    }
    let num_planes = unsafe { (*(*fb).format).num_planes as i32 };
    let _ = tk_linux_kapi::assert::warn_on(
        !unsafe { intel_fb_is_ccs_modifier((*fb).modifier) } || ccs_plane < num_planes / 2,
    );
    if unsafe { intel_fb_rc_ccs_cc_plane(fb) == ccs_plane } {
        0
    } else {
        ccs_plane - num_planes / 2
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_main_to_aux_plane(fb: *const DrmFramebuffer, main_plane: i32) -> i32 {
    if fb.is_null() || unsafe { (*fb).format.is_null() } {
        return 0;
    }
    let desc = modifier_desc(unsafe { (*fb).modifier });
    if desc.packed_aux_planes | desc.planar_aux_planes != 0 {
        unsafe { main_to_ccs_plane(fb, main_plane) }
    } else if unsafe { DISPLAY_VER(__drm_to_display((*fb).dev)) } < 11
        && format_is_yuv_semiplanar(desc, unsafe { (*fb).format })
    {
        1
    } else {
        0
    }
}

#[inline]
unsafe fn display_from_fb(fb: *const DrmFramebuffer) -> *mut IntelDisplay {
    if fb.is_null() {
        return ptr::null_mut();
    }
    unsafe { __drm_to_display((*fb).dev) }
}

#[inline]
unsafe fn format_cpp(info: *const DrmFormatInfo, plane: usize) -> u32 {
    if info.is_null() || plane >= 4 {
        return 0;
    }
    unsafe { (*info)._anon_6.cpp[plane] as u32 }
}

#[inline]
unsafe fn intel_fb_is_gen12_ccs_cc_plane(fb: *const DrmFramebuffer, plane: i32) -> bool {
    unsafe { intel_fb_rc_ccs_cc_plane(fb) == plane }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_fb_needs_cpu_access(fb: *const DrmFramebuffer) -> bool {
    unsafe { intel_fb_rc_ccs_cc_plane(fb) >= 0 }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_fb_modifier_uses_dpt(
    display: *mut IntelDisplay,
    modifier: u64,
) -> bool {
    !display.is_null() && unsafe { DISPLAY_VER(display) >= 13 } && modifier != DRM_FORMAT_MOD_LINEAR
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_fb_uses_dpt(fb: *const DrmFramebuffer) -> bool {
    if fb.is_null() {
        return false;
    }
    let display = unsafe { display_from_fb(fb) };
    !display.is_null()
        && unsafe { (*display).params.enable_dpt }
        && unsafe { intel_fb_modifier_uses_dpt(display, (*fb).modifier) }
}

// upstream: intel_fb.c intel_plane_uses_fence()
// Resolves the extern declared in intel_plane_upstream.rs.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_plane_uses_fence(plane_state: *const IntelPlaneState) -> bool {
    unsafe {
        let plane = (*plane_state).uapi.plane;
        let display = __drm_to_display((*plane).dev);
        // to_intel_plane(): `base` is the first member of IntelPlane.
        let intel_plane = plane.cast::<IntelPlane>();
        crate::intel_plane_upstream::intel_plane_needs_fence(display)
            || (!(*intel_plane).fbc.is_null()
                && (*plane_state).no_fbc_reason.is_null()
                // i915_gtt_view_is_normal(&plane_state->view.gtt)
                && (*plane_state).view.gtt.r#type == I915_GTT_VIEW_NORMAL)
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_fb_plane_get_subsampling(
    hsub: *mut i32,
    vsub: *mut i32,
    fb: *const DrmFramebuffer,
    color_plane: i32,
) {
    if hsub.is_null() || vsub.is_null() || fb.is_null() || unsafe { (*fb).format.is_null() } {
        return;
    }
    if color_plane == 0 {
        unsafe {
            *hsub = 1;
            *vsub = 1;
        }
        return;
    }
    if !is_gen12_ccs_aux_plane(fb, color_plane) {
        unsafe {
            *hsub = (*(*fb).format).hsub as i32;
            *vsub = (*(*fb).format).vsub as i32;
        }
        return;
    }
    let main_plane = unsafe { skl_ccs_to_main_plane(fb, color_plane) };
    let main_block = unsafe {
        tk_drm_upstream::drm_fourcc_upstream::drm_format_info_block_width((*fb).format, main_plane)
    };
    let ccs_block = unsafe {
        tk_drm_upstream::drm_fourcc_upstream::drm_format_info_block_width((*fb).format, color_plane)
    };
    let mut horizontal = if main_block == 0 {
        1
    } else {
        ccs_block / main_block
    } as i32;
    if main_plane == 0 {
        horizontal *= unsafe { (*(*fb).format).hsub as i32 };
    }
    unsafe {
        *hsub = horizontal;
        *vsub = 32;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_tile_size(display: *mut IntelDisplay) -> u32 {
    if !display.is_null() && unsafe { DISPLAY_VER(display) == 2 } {
        2048
    } else {
        4096
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_tile_width_bytes(
    fb: *const DrmFramebuffer,
    color_plane: i32,
) -> u32 {
    if fb.is_null() || unsafe { (*fb).format.is_null() } || color_plane < 0 || color_plane >= 4 {
        return 0;
    }
    let display = unsafe { display_from_fb(fb) };
    let modifier = unsafe { (*fb).modifier };
    let cpp = unsafe { format_cpp((*fb).format, color_plane as usize) };
    match modifier {
        DRM_FORMAT_MOD_LINEAR => unsafe { intel_tile_size(display) },
        I915_FORMAT_MOD_X_TILED => {
            if unsafe { DISPLAY_VER(display) == 2 } {
                128
            } else {
                512
            }
        }
        I915_FORMAT_MOD_4_TILED_BMG_CCS
        | I915_FORMAT_MOD_4_TILED_LNL_CCS
        | I915_FORMAT_MOD_4_TILED_DG2_RC_CCS
        | I915_FORMAT_MOD_4_TILED_DG2_RC_CCS_CC
        | I915_FORMAT_MOD_4_TILED_DG2_MC_CCS
        | I915_FORMAT_MOD_4_TILED => 128,
        I915_FORMAT_MOD_Y_TILED_CCS if unsafe { intel_fb_is_ccs_aux_plane(fb, color_plane) } => 128,
        I915_FORMAT_MOD_4_TILED_MTL_RC_CCS
        | I915_FORMAT_MOD_4_TILED_MTL_RC_CCS_CC
        | I915_FORMAT_MOD_4_TILED_MTL_MC_CCS
        | I915_FORMAT_MOD_Y_TILED_GEN12_RC_CCS
        | I915_FORMAT_MOD_Y_TILED_GEN12_RC_CCS_CC
        | I915_FORMAT_MOD_Y_TILED_GEN12_MC_CCS
            if unsafe {
                intel_fb_is_ccs_aux_plane(fb, color_plane)
                    || intel_fb_is_gen12_ccs_cc_plane(fb, color_plane)
            } =>
        {
            64
        }
        I915_FORMAT_MOD_4_TILED_MTL_RC_CCS
        | I915_FORMAT_MOD_4_TILED_MTL_RC_CCS_CC
        | I915_FORMAT_MOD_4_TILED_MTL_MC_CCS
        | I915_FORMAT_MOD_Y_TILED_GEN12_RC_CCS
        | I915_FORMAT_MOD_Y_TILED_GEN12_RC_CCS_CC
        | I915_FORMAT_MOD_Y_TILED_GEN12_MC_CCS
        | I915_FORMAT_MOD_Y_TILED => {
            if !display_platform_bit(display, 7) && !display_platform_bit(display, 8) {
                128
            } else {
                512
            }
        }
        I915_FORMAT_MOD_YF_TILED_CCS if unsafe { intel_fb_is_ccs_aux_plane(fb, color_plane) } => {
            128
        }
        I915_FORMAT_MOD_YF_TILED_CCS | I915_FORMAT_MOD_YF_TILED => match cpp {
            1 => 64,
            2 | 4 => 128,
            8 | 16 => 256,
            _ => cpp,
        },
        _ => cpp,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_tile_height(fb: *const DrmFramebuffer, color_plane: i32) -> u32 {
    let display = unsafe { display_from_fb(fb) };
    let width = unsafe { intel_tile_width_bytes(fb, color_plane) };
    if width == 0 {
        0
    } else {
        (unsafe { intel_tile_size(display) }) / width
    }
}

#[inline]
unsafe fn intel_tile_dims(fb: *const DrmFramebuffer, color_plane: i32) -> (u32, u32) {
    let width_bytes = unsafe { intel_tile_width_bytes(fb, color_plane) };
    let cpp = unsafe { format_cpp((*fb).format, color_plane.max(0) as usize) };
    if cpp == 0 {
        return (0, 0);
    }
    (width_bytes / cpp, unsafe {
        intel_tile_height(fb, color_plane)
    })
}

#[inline]
unsafe fn intel_tile_block_dims(fb: *const DrmFramebuffer, color_plane: i32) -> (u32, u32) {
    let (width, mut height) = unsafe { intel_tile_dims(fb, color_plane) };
    if is_gen12_ccs_aux_plane(fb, color_plane) {
        height = 1;
    }
    (width, height)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_fb_align_height(
    fb: *const DrmFramebuffer,
    color_plane: i32,
    height: u32,
) -> u32 {
    let tile_height = unsafe { intel_tile_height(fb, color_plane) };
    if tile_height == 0 {
        height
    } else {
        height.saturating_add(tile_height - 1) / tile_height * tile_height
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_fb_supports_90_270_rotation(fb: *const IntelFramebuffer) -> bool {
    if fb.is_null() {
        return false;
    }
    let display = unsafe { display_from_fb(ptr::addr_of!((*fb).base)) };
    if unsafe { DISPLAY_VER(display) >= 13 } {
        return false;
    }
    let modifier = unsafe { (*fb).base.modifier };
    modifier == I915_FORMAT_MOD_Y_TILED || modifier == I915_FORMAT_MOD_YF_TILED
}

#[inline]
fn div_round_up(value: u32, divisor: u32) -> u32 {
    if divisor == 0 {
        0
    } else {
        value / divisor + u32::from(value % divisor != 0)
    }
}

fn fb_required_bytes(max_tiles: u32, tile_size: u32, object_size: usize) -> Result<usize, i32> {
    let required = (max_tiles as usize)
        .checked_mul(tile_size as usize)
        .ok_or(-tk_linux_kapi::config::ERANGE)?;
    if required > object_size {
        Err(-tk_linux_kapi::config::EINVAL)
    } else {
        Ok(required)
    }
}

#[inline]
unsafe fn cpp_for(fb: *const DrmFramebuffer, plane: usize) -> u32 {
    if fb.is_null() || plane >= 4 || unsafe { (*fb).format.is_null() } {
        0
    } else {
        unsafe { format_cpp((*fb).format, plane) }
    }
}

#[inline]
unsafe fn is_rotation_90_270(rotation: u32) -> bool {
    rotation & (DRM_MODE_ROTATE_90 | DRM_MODE_ROTATE_270) != 0
}

unsafe fn adjust_linear_offset(
    x: &mut u32,
    y: &mut u32,
    cpp: u32,
    pitch: u32,
    old: u32,
    new: u32,
) -> u32 {
    if cpp == 0 || pitch == 0 || new > old {
        return new;
    }
    let Some(offset) = old
        .checked_add(y.saturating_mul(pitch))
        .and_then(|v| v.checked_add(x.saturating_mul(cpp)))
    else {
        return new;
    };
    let relative = offset - new;
    *y = relative / pitch;
    *x = (relative - *y * pitch) / cpp;
    new
}

unsafe fn adjust_tile_offset(
    x: &mut u32,
    y: &mut u32,
    tile_width: u32,
    tile_height: u32,
    tile_size: u32,
    pitch_tiles: u32,
    old: u32,
    new: u32,
) -> u32 {
    if tile_width == 0 || tile_height == 0 || tile_size == 0 || pitch_tiles == 0 || new > old {
        return new;
    }
    let tiles = (old - new) / tile_size;
    *y = y.saturating_add((tiles / pitch_tiles).saturating_mul(tile_height));
    *x = x.saturating_add((tiles % pitch_tiles).saturating_mul(tile_width));
    let pitch_pixels = pitch_tiles.saturating_mul(tile_width);
    if pitch_pixels != 0 {
        *y = y.saturating_add((*x / pitch_pixels).saturating_mul(tile_height));
        *x %= pitch_pixels;
    }
    new
}

/// Translate an object byte offset into the x/y position within the original
/// framebuffer. Non-linear surfaces are required to start on a GTT tile.
unsafe fn fb_offset_to_xy(fb: *const DrmFramebuffer, plane: usize) -> Result<(u32, u32), i32> {
    if fb.is_null() || plane >= 4 {
        return Err(-tk_linux_kapi::config::EINVAL);
    }
    let display = unsafe { display_from_fb(fb) };
    let tile_size = unsafe { intel_tile_size(display) };
    let offset = unsafe { (*fb).offsets[plane] };
    let pitch = unsafe { (*fb).pitches[plane] };
    let linear = unsafe { is_surface_linear(fb, plane as i32) };
    if !linear && (tile_size == 0 || offset % tile_size != 0) {
        return Err(-tk_linux_kapi::config::EINVAL);
    }
    let (mut x, mut y) = (0, 0);
    let height = unsafe { (*fb).height };
    let vsub = if plane == 0 {
        1
    } else {
        unsafe { (*(*fb).format).vsub as u32 }.max(1)
    };
    let plane_height = div_round_up(height, vsub);
    let aligned_height = unsafe { intel_fb_align_height(fb, plane as i32, plane_height) };
    if aligned_height
        .checked_mul(pitch)
        .and_then(|v| v.checked_add(offset))
        .is_none()
    {
        return Err(-tk_linux_kapi::config::ERANGE);
    }
    if linear {
        let cpp = unsafe { cpp_for(fb, plane) };
        if pitch == 0 || cpp == 0 {
            return Err(-tk_linux_kapi::config::EINVAL);
        }
        y = offset / pitch;
        x = (offset % pitch) / cpp;
    } else {
        let (tile_width, tile_height) = unsafe { intel_tile_dims(fb, plane as i32) };
        let cpp = unsafe { cpp_for(fb, plane) };
        let row_bytes = tile_width
            .checked_mul(cpp)
            .filter(|v| *v != 0)
            .ok_or(-tk_linux_kapi::config::EINVAL)?;
        let pitch_tiles = pitch / row_bytes;
        if pitch_tiles == 0 {
            return Err(-tk_linux_kapi::config::EINVAL);
        }
        let tile_index = offset / tile_size;
        y = (tile_index / pitch_tiles).saturating_mul(tile_height);
        x = (tile_index % pitch_tiles).saturating_mul(tile_width);
    }
    Ok((x, y))
}

unsafe fn compute_aligned_offset(
    fb: *const DrmFramebuffer,
    plane: usize,
    x: &mut u32,
    y: &mut u32,
    pitch: u32,
    rotation: u32,
    alignment: u32,
) -> u32 {
    if fb.is_null() || plane >= 4 {
        return 0;
    }
    let display = unsafe { display_from_fb(fb) };
    let cpp = unsafe { cpp_for(fb, plane) };
    if cpp == 0 || pitch == 0 {
        return 0;
    }
    if unsafe { is_surface_linear(fb, plane as i32) } {
        let Some(offset) = y
            .checked_mul(pitch)
            .and_then(|v| x.checked_mul(cpp).and_then(|xbytes| v.checked_add(xbytes)))
        else {
            return 0;
        };
        let aligned = if alignment == 0 {
            offset
        } else {
            offset & !(alignment - 1)
        };
        if alignment == 0 {
            *x = 0;
            *y = 0;
        } else {
            let rem = offset - aligned;
            *y = rem / pitch;
            *x = (rem - *y * pitch) / cpp;
        }
        return aligned;
    }
    let tile_size = unsafe { intel_tile_size(display) };
    let (mut tile_width, mut tile_height) = unsafe { intel_tile_dims(fb, plane as i32) };
    let pitch_tiles = if unsafe { is_rotation_90_270(rotation) } {
        if tile_height == 0 {
            0
        } else {
            pitch / tile_height
        }
    } else {
        pitch / tile_width.saturating_mul(cpp)
    };
    if unsafe { is_rotation_90_270(rotation) } {
        core::mem::swap(&mut tile_width, &mut tile_height);
    }
    if tile_width == 0 || tile_height == 0 || pitch_tiles == 0 || tile_size == 0 {
        return 0;
    }
    let tile_rows = *y / tile_height;
    *y %= tile_height;
    let tiles = *x / tile_width;
    *x %= tile_width;
    let offset =
        (tile_rows.saturating_mul(pitch_tiles).saturating_add(tiles)).saturating_mul(tile_size);
    let aligned = if alignment == 0 {
        offset
    } else {
        offset & !(alignment - 1)
    };
    unsafe {
        adjust_tile_offset(
            x,
            y,
            tile_width,
            tile_height,
            tile_size,
            pitch_tiles,
            offset,
            aligned,
        )
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_plane_adjust_aligned_offset(
    x: *mut i32,
    y: *mut i32,
    plane_state: *const IntelPlaneState,
    color_plane: i32,
    old_offset: u32,
    new_offset: u32,
) -> u32 {
    if x.is_null() || y.is_null() || plane_state.is_null() || color_plane < 0 || color_plane >= 4 {
        return new_offset;
    }
    let fb = unsafe { (*plane_state).hw.fb };
    if fb.is_null() {
        return new_offset;
    }
    let mut px = unsafe { (*x).max(0) as u32 };
    let mut py = unsafe { (*y).max(0) as u32 };
    let pitch = unsafe { (*plane_state).view.color_plane[color_plane as usize].mapping_stride };
    let rotation = unsafe { (*plane_state).hw.rotation };
    let adjusted = if unsafe { is_surface_linear(fb, color_plane) } {
        unsafe {
            adjust_linear_offset(
                &mut px,
                &mut py,
                cpp_for(fb, color_plane as usize),
                pitch,
                old_offset,
                new_offset,
            )
        }
    } else {
        let display = unsafe { display_from_fb(fb) };
        let (mut tw, mut th) = unsafe { intel_tile_dims(fb, color_plane) };
        let cpp = unsafe { cpp_for(fb, color_plane as usize) };
        let pt = if unsafe { is_rotation_90_270(rotation) } {
            if th == 0 { 0 } else { pitch / th }
        } else {
            pitch / tw.saturating_mul(cpp)
        };
        if unsafe { is_rotation_90_270(rotation) } {
            core::mem::swap(&mut tw, &mut th);
        }
        unsafe {
            adjust_tile_offset(
                &mut px,
                &mut py,
                tw,
                th,
                intel_tile_size(display),
                pt,
                old_offset,
                new_offset,
            )
        }
    };
    unsafe {
        *x = px.min(i32::MAX as u32) as i32;
        *y = py.min(i32::MAX as u32) as i32;
    }
    adjusted
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_plane_compute_aligned_offset(
    x: *mut i32,
    y: *mut i32,
    plane_state: *const IntelPlaneState,
    color_plane: i32,
) -> u32 {
    if x.is_null() || y.is_null() || plane_state.is_null() || color_plane < 0 || color_plane >= 4 {
        return 0;
    }
    let fb = unsafe { (*plane_state).hw.fb };
    if fb.is_null() {
        return 0;
    }
    let plane = unsafe { (*plane_state).uapi.plane.cast::<IntelPlane>() };
    if plane.is_null() {
        return 0;
    }
    let min_alignment = unsafe { (*plane).min_alignment };
    let alignment = min_alignment
        .map(|f| unsafe { f(plane, fb, color_plane) })
        .unwrap_or(0);
    let mut px = unsafe { (*x).max(0) as u32 };
    let mut py = unsafe { (*y).max(0) as u32 };
    let pitch = unsafe { (*plane_state).view.color_plane[color_plane as usize].mapping_stride };
    let offset = unsafe {
        compute_aligned_offset(
            fb,
            color_plane as usize,
            &mut px,
            &mut py,
            pitch,
            (*plane_state).hw.rotation,
            alignment,
        )
    };
    unsafe {
        *x = px.min(i32::MAX as u32) as i32;
        *y = py.min(i32::MAX as u32) as i32;
    }
    offset
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_fb_xy_to_linear(
    x: i32,
    y: i32,
    plane_state: *const IntelPlaneState,
    color_plane: i32,
) -> u32 {
    if plane_state.is_null() || color_plane < 0 || color_plane >= 4 {
        return 0;
    }
    let fb = unsafe { (*plane_state).hw.fb };
    let cpp = unsafe { cpp_for(fb, color_plane as usize) };
    let pitch = unsafe { (*plane_state).view.color_plane[color_plane as usize].mapping_stride };
    (x.max(0) as u32)
        .saturating_mul(cpp)
        .saturating_add((y.max(0) as u32).saturating_mul(pitch))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_add_fb_offsets(
    x: *mut i32,
    y: *mut i32,
    plane_state: *const IntelPlaneState,
    color_plane: i32,
) {
    if x.is_null() || y.is_null() || plane_state.is_null() || color_plane < 0 || color_plane >= 4 {
        return;
    }
    let x_offset = unsafe { (*plane_state).view.color_plane[color_plane as usize].x };
    let y_offset = unsafe { (*plane_state).view.color_plane[color_plane as usize].y };
    unsafe {
        *x = x
            .read()
            .saturating_add(x_offset.min(i32::MAX as u32) as i32);
        *y = y
            .read()
            .saturating_add(y_offset.min(i32::MAX as u32) as i32);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_fb_fill_view(
    fb: *const IntelFramebuffer,
    rotation: u32,
    view: *mut IntelFbView,
) {
    if fb.is_null() || view.is_null() {
        return;
    }
    let src = if unsafe { is_rotation_90_270(rotation) } {
        ptr::addr_of!((*fb)._anon_336_3.rotated_view).cast::<IntelFbView>()
    } else if unsafe { intel_fb_needs_pot_stride_remap(fb) } {
        ptr::addr_of!((*fb)._anon_336_3.remapped_view).cast::<IntelFbView>()
    } else {
        ptr::addr_of!((*fb).normal_view)
    };
    unsafe { ptr::copy_nonoverlapping(src, view, 1) };
}

unsafe fn intel_fb_needs_pot_stride_remap(fb: *const IntelFramebuffer) -> bool {
    if fb.is_null() {
        return false;
    }
    let display = unsafe { display_from_fb(ptr::addr_of!((*fb).base)) };
    let platform = unsafe { display_platform_bit(display, 52) }; // alderlake_p platform capability
    (platform || unsafe { DISPLAY_VER(display) >= 14 })
        && unsafe { intel_fb_uses_dpt(ptr::addr_of!((*fb).base)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_fb_view_vtd_guard(
    fb: *const DrmFramebuffer,
    view: *const IntelFbView,
    rotation: u32,
) -> u32 {
    if fb.is_null() || view.is_null() {
        return 0;
    }
    let intel_fb = fb.cast::<IntelFramebuffer>();
    let mut guard = unsafe { (*intel_fb).vtd_guard } as u32;
    if guard == 0 {
        return 0;
    }
    let count = unsafe { (*(*fb).format).num_planes.min(4) as usize };
    for p in 0..count {
        if unsafe {
            intel_fb_is_ccs_aux_plane(fb, p as i32) || intel_fb_rc_ccs_cc_plane(fb) == p as i32
        } {
            continue;
        }
        let stride = unsafe { (*view).color_plane[p].mapping_stride };
        let tile = if unsafe { is_rotation_90_270(rotation) } {
            unsafe { intel_tile_height(fb, p as i32) }
        } else {
            unsafe { intel_tile_width_bytes(fb, p as i32) }
        };
        if tile != 0 {
            guard = guard.max(div_round_up(stride, tile));
        }
    }
    guard
}

unsafe fn plane_check_stride(plane_state: *const IntelPlaneState) -> i32 {
    if plane_state.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let fb = unsafe { (*plane_state).hw.fb };
    let plane = unsafe { (*plane_state).uapi.plane.cast::<IntelPlane>() };
    if fb.is_null() || plane.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let Some(max_stride) = (unsafe { (*plane).max_stride }) else {
        return -tk_linux_kapi::config::EINVAL;
    };
    let format = unsafe { (*fb).format };
    if format.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let rotation = unsafe { (*plane_state).hw.rotation };
    let stride = unsafe { (*plane_state).view.color_plane[0].mapping_stride };
    let maximum = unsafe { max_stride(plane, format, (*fb).modifier, rotation) };
    if maximum == 0 || stride > maximum {
        -tk_linux_kapi::config::EINVAL
    } else {
        0
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_plane_compute_gtt(plane_state: *mut IntelPlaneState) -> i32 {
    if plane_state.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let fb = unsafe { (*plane_state).hw.fb };
    if fb.is_null() {
        return 0;
    }
    let intel_fb = fb.cast::<IntelFramebuffer>();
    let display = unsafe { display_from_fb(fb) };
    let rotation = unsafe { (*plane_state).hw.rotation };
    if unsafe { is_rotation_90_270(rotation) }
        && !unsafe { intel_fb_supports_90_270_rotation(intel_fb) }
    {
        return -tk_linux_kapi::config::EINVAL;
    }
    // ADL-P DPT mappings are built with the framebuffer and selected here;
    // upstream disables arbitrary viewport remapping for DPT-backed planes.
    unsafe {
        intel_fb_fill_view(intel_fb, rotation, ptr::addr_of_mut!((*plane_state).view));
    }
    if unsafe { is_rotation_90_270(rotation) } {
        if unsafe { DISPLAY_VER(display) >= 13 } {
            return -tk_linux_kapi::config::EINVAL;
        }
        let width = unsafe { (*fb).width };
        let height = unsafe { (*fb).height };
        unsafe {
            tk_drm_upstream::drm_rect_upstream::drm_rect_rotate(
                ptr::addr_of_mut!((*plane_state).uapi.src),
                width.min(i32::MAX as u32) as i32,
                height.min(i32::MAX as u32) as i32,
                DRM_MODE_ROTATE_270,
            );
        }
    }
    unsafe { plane_check_stride(plane_state) }
}

const DMA_RESV_USAGE_READ: u32 = DMA_RESV_USAGE_KERNEL + 2;

#[inline]
unsafe fn frontbuffer_has_bits(front: *const IntelFrontbuffer) -> bool {
    if front.is_null() {
        return false;
    }
    let bits = unsafe { ptr::addr_of!((*front).bits.counter).cast::<AtomicI32>() };
    unsafe { &*bits }.load(Ordering::Relaxed) != 0
}

#[repr(C)]
struct FrontbufferFenceCb {
    base: DmaFenceCb,
    front: *mut IntelFrontbuffer,
}

unsafe extern "C" fn intel_user_framebuffer_fence_wake(
    fence: *mut DmaFence,
    callback: *mut DmaFenceCb,
) {
    if fence.is_null() || callback.is_null() {
        return;
    }
    let cb = unsafe {
        callback
            .cast::<u8>()
            .sub(offset_of!(FrontbufferFenceCb, base))
            .cast::<FrontbufferFenceCb>()
    };
    let front = unsafe { (*cb).front };
    if !front.is_null() {
        let display = unsafe { (*front).display };
        unsafe {
            crate::intel_frontbuffer_upstream::intel_frontbuffer_queue_flush(front);
            intel_parent_frontbuffer_put(display, front.cast());
        }
    }
    unsafe {
        dma_fence_put(fence);
        kfree(cb.cast::<c_void>());
    }
}

unsafe extern "C" fn intel_user_framebuffer_dirty(
    fb: *mut DrmFramebuffer,
    _file: *mut DrmFile,
    _flags: u32,
    _color: u32,
    _clips: *mut DrmClipRect,
    _num_clips: u32,
) -> i32 {
    if fb.is_null() {
        return -EINVAL;
    }
    let intel_fb = fb.cast::<IntelFramebuffer>();
    let front = unsafe { (*intel_fb).frontbuffer };
    if !unsafe { frontbuffer_has_bits(front) } {
        return 0;
    }
    let obj = unsafe { (*fb).obj[0] };
    if obj.is_null() {
        unsafe {
            crate::intel_frontbuffer_upstream::intel_frontbuffer_flush(
                front,
                crate::intel_frontbuffer_upstream::ORIGIN_DIRTYFB,
            )
        };
        return -EINVAL;
    }
    let resv = unsafe { (*obj).resv };
    if resv.is_null() || unsafe { dma_resv_test_signaled(resv.cast(), DMA_RESV_USAGE_READ as i32) }
    {
        unsafe {
            crate::intel_frontbuffer_upstream::intel_frontbuffer_flush(
                front,
                crate::intel_frontbuffer_upstream::ORIGIN_DIRTYFB,
            )
        };
        return 0;
    }

    let mut fence = ptr::null_mut();
    let ret = unsafe { dma_resv_get_singleton(resv.cast(), DMA_RESV_USAGE_READ, &mut fence) };
    if ret != 0 || fence.is_null() {
        unsafe {
            crate::intel_frontbuffer_upstream::intel_frontbuffer_flush(
                front,
                crate::intel_frontbuffer_upstream::ORIGIN_DIRTYFB,
            )
        };
        return ret;
    }
    let cb = unsafe { kzalloc_obj::<FrontbufferFenceCb>() };
    if cb.is_null() {
        unsafe {
            dma_fence_put(fence);
            crate::intel_frontbuffer_upstream::intel_frontbuffer_flush(
                front,
                crate::intel_frontbuffer_upstream::ORIGIN_DIRTYFB,
            );
        }
        return -ENOMEM;
    }
    unsafe {
        (*cb).front = front;
        intel_parent_frontbuffer_ref((*front).display, front.cast());
    }
    if !unsafe {
        crate::intel_frontbuffer_upstream::intel_frontbuffer_invalidate(
            front,
            crate::intel_frontbuffer_upstream::ORIGIN_DIRTYFB,
        )
    } {
        unsafe {
            intel_parent_frontbuffer_put((*front).display, front.cast());
            dma_fence_put(fence);
            kfree(cb.cast::<c_void>());
        }
        return 0;
    }
    let ret = unsafe {
        tk_linux_kapi::dma_fence::dma_fence_add_callback(
            fence,
            ptr::addr_of_mut!((*cb).base),
            intel_user_framebuffer_fence_wake,
        )
    };
    if ret != 0 {
        unsafe { intel_user_framebuffer_fence_wake(fence, ptr::addr_of_mut!((*cb).base)) };
        return if ret == -tk_linux_kapi::config::ENOENT {
            0
        } else {
            ret
        };
    }
    0
}

unsafe extern "C" fn intel_user_framebuffer_create_handle(
    fb: *mut DrmFramebuffer,
    file: *mut DrmFile,
    handle: *mut u32,
) -> i32 {
    if fb.is_null() || file.is_null() || handle.is_null() {
        return -EINVAL;
    }
    let obj = unsafe { (*fb).obj[0] };
    if obj.is_null() || unsafe { intel_bo_is_userptr(obj.cast()) } {
        return -EINVAL;
    }
    unsafe { tk_drm_upstream::drm_gem_upstream::drm_gem_handle_create(file, obj, handle) }
}

unsafe extern "C" fn intel_user_framebuffer_destroy(fb: *mut DrmFramebuffer) {
    if fb.is_null() {
        return;
    }
    let intel_fb = fb.cast::<IntelFramebuffer>();
    let display = unsafe { display_from_fb(fb) };
    unsafe {
        tk_drm_upstream::drm_framebuffer_upstream::drm_framebuffer_cleanup(fb);
        if intel_fb_uses_dpt(fb) {
            intel_parent_dpt_destroy(display, (*intel_fb).dpt);
        }
        intel_bo_framebuffer_fini((*fb).obj[0].cast());
        intel_parent_frontbuffer_put(display, (*intel_fb).frontbuffer.cast());
        kfree((*intel_fb).panic);
        kfree(intel_fb.cast::<c_void>());
    }
}

static INTEL_FRAMEBUFFER_FUNCS: DrmFramebufferFuncs = DrmFramebufferFuncs {
    destroy: Some(intel_user_framebuffer_destroy),
    create_handle: Some(intel_user_framebuffer_create_handle),
    dirty: Some(intel_user_framebuffer_dirty),
};

unsafe fn plane_max_stride(
    display: *mut IntelDisplay,
    info: *mut DrmFormatInfo,
    modifier: u64,
) -> u32 {
    if display.is_null() || info.is_null() || unsafe { (*display).drm.is_null() } {
        return 0;
    }
    let drm = unsafe { (*display).drm };
    let head = unsafe { ptr::addr_of!((*drm).mode_config.plane_list) };
    let mut node = unsafe { (*head).next };
    let mut maximum = 0u32;
    while !node.is_null() && node != head.cast_mut() {
        let drm_plane = unsafe {
            node.cast::<u8>()
                .sub(offset_of!(DrmPlane, head))
                .cast::<DrmPlane>()
        };
        if unsafe {
            tk_drm_upstream::drm_plane_upstream::drm_plane_has_format(
                drm_plane,
                (*info).format,
                modifier,
            )
        } {
            let plane = drm_plane.cast::<IntelPlane>();
            if let Some(max_stride) = unsafe { (*plane).max_stride } {
                maximum =
                    maximum.max(unsafe { max_stride(plane, info, modifier, DRM_MODE_ROTATE_0) });
            }
        }
        node = unsafe { (*node).next };
    }
    maximum
}

unsafe fn intel_fb_max_stride(
    display: *mut IntelDisplay,
    info: *mut DrmFormatInfo,
    modifier: u64,
) -> u32 {
    if display.is_null() {
        return 0;
    }
    if unsafe {
        DISPLAY_VER(display) < 4
            || intel_fb_is_ccs_modifier(modifier)
            || intel_fb_modifier_uses_dpt(display, modifier)
    } {
        unsafe { plane_max_stride(display, info, modifier) }
    } else if unsafe { DISPLAY_VER(display) >= 7 } {
        256 * 1024
    } else {
        128 * 1024
    }
}

unsafe fn fb_stride_alignment(fb: *const DrmFramebuffer, color_plane: usize) -> u32 {
    let display = unsafe { display_from_fb(fb) };
    if unsafe { is_surface_linear(fb, color_plane as i32) } {
        let max_stride = unsafe { intel_fb_max_stride(display, (*fb).format, (*fb).modifier) };
        if unsafe {
            (*fb).pitches[color_plane] > max_stride && !intel_fb_is_ccs_modifier((*fb).modifier)
        } {
            unsafe { intel_tile_size(display) }
        } else {
            64
        }
    } else {
        let mut tile_width = unsafe { intel_tile_width_bytes(fb, color_plane as i32) };
        if unsafe { intel_fb_is_ccs_modifier((*fb).modifier) && DISPLAY_VER(display) >= 12 } {
            tile_width = tile_width.saturating_mul(4);
        }
        tile_width
    }
}

unsafe fn gen12_ccs_aux_stride(fb: *const DrmFramebuffer, color_plane: i32) -> u32 {
    let main_plane = unsafe { skl_ccs_to_main_plane(fb, color_plane) }.max(0) as usize;
    let main_stride = unsafe { (*fb).pitches[main_plane] };
    let tile_width = unsafe { intel_tile_width_bytes(fb, main_plane as i32) };
    if tile_width == 0 {
        0
    } else {
        div_round_up(main_stride, tile_width.saturating_mul(4)).saturating_mul(64)
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_framebuffer_alloc() -> *mut IntelFramebuffer {
    unsafe { kzalloc_obj::<IntelFramebuffer>() }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_fb_bo(fb: *const DrmFramebuffer) -> *mut DrmGemObject {
    if fb.is_null() {
        ptr::null_mut()
    } else {
        unsafe { (*fb).obj[0] }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_framebuffer_init(
    intel_fb: *mut IntelFramebuffer,
    obj: *mut DrmGemObject,
    info: *const DrmFormatInfo,
    mode_cmd: *mut DrmModeFbCmd2,
) -> i32 {
    if intel_fb.is_null() || obj.is_null() || info.is_null() || mode_cmd.is_null() {
        return -EINVAL;
    }
    let display = unsafe { __drm_to_display((*obj).dev) };
    if display.is_null() {
        return -ENODEV;
    }
    let fb = unsafe { ptr::addr_of_mut!((*intel_fb).base) };
    let panic = unsafe { intel_parent_panic_alloc(display) };
    if panic.is_null() {
        return -ENOMEM;
    }
    unsafe {
        (*intel_fb).panic = panic;
    }
    let front = unsafe { intel_parent_frontbuffer_get(display, obj.cast()) };
    if front.is_null() {
        unsafe {
            kfree(panic);
            (*intel_fb).panic = ptr::null_mut();
        }
        return -ENOMEM;
    }
    unsafe {
        (*intel_fb).frontbuffer = front.cast();
    }
    let ret = unsafe { intel_bo_framebuffer_init(obj.cast(), mode_cmd.cast()) };
    if ret != 0 {
        unsafe {
            intel_parent_frontbuffer_put(display, front);
            kfree(panic);
            (*intel_fb).panic = ptr::null_mut();
            (*intel_fb).frontbuffer = ptr::null_mut();
        }
        return ret;
    }
    let pixel_format = unsafe { (*mode_cmd).pixel_format };
    let modifier = unsafe { (*mode_cmd).modifier[0] };
    if !unsafe {
        tk_drm_upstream::drm_plane_upstream::drm_any_plane_has_format(
            (*display).drm,
            pixel_format,
            modifier,
        )
    } {
        unsafe {
            intel_bo_framebuffer_fini(obj.cast());
            intel_parent_frontbuffer_put(display, front);
            kfree(panic);
            (*intel_fb).panic = ptr::null_mut();
            (*intel_fb).frontbuffer = ptr::null_mut();
        }
        return -EINVAL;
    }
    let max_stride = unsafe { intel_fb_max_stride(display, info.cast_mut(), modifier) };
    if max_stride == 0
        || unsafe { (*mode_cmd).pitches[0] > max_stride }
        || unsafe { (*mode_cmd).offsets[0] != 0 }
    {
        unsafe {
            intel_bo_framebuffer_fini(obj.cast());
            intel_parent_frontbuffer_put(display, front);
            kfree(panic);
            (*intel_fb).panic = ptr::null_mut();
            (*intel_fb).frontbuffer = ptr::null_mut();
        }
        return -EINVAL;
    }
    unsafe {
        tk_drm_upstream::drm_modeset_helper_upstream::drm_helper_mode_fill_fb_struct(
            (*display).drm,
            fb,
            info,
            mode_cmd,
        )
    };
    let num_planes = unsafe { (*info).num_planes.min(4) as usize };
    for plane in 0..num_planes {
        if unsafe { (*mode_cmd).handles[plane] != (*mode_cmd).handles[0] } {
            unsafe {
                intel_bo_framebuffer_fini(obj.cast());
                intel_parent_frontbuffer_put(display, front);
                kfree(panic);
                (*intel_fb).panic = ptr::null_mut();
                (*intel_fb).frontbuffer = ptr::null_mut();
            }
            return -EINVAL;
        }
        let alignment = unsafe { fb_stride_alignment(fb, plane) };
        if alignment == 0
            || alignment.is_power_of_two() == false
            || unsafe { (*fb).pitches[plane] & (alignment - 1) != 0 }
        {
            unsafe {
                intel_bo_framebuffer_fini(obj.cast());
                intel_parent_frontbuffer_put(display, front);
                kfree(panic);
                (*intel_fb).panic = ptr::null_mut();
                (*intel_fb).frontbuffer = ptr::null_mut();
            }
            return -EINVAL;
        }
        if unsafe { is_gen12_ccs_aux_plane(fb, plane as i32) }
            && unsafe { (*fb).pitches[plane] != gen12_ccs_aux_stride(fb, plane as i32) }
        {
            unsafe {
                intel_bo_framebuffer_fini(obj.cast());
                intel_parent_frontbuffer_put(display, front);
                kfree(panic);
                (*intel_fb).panic = ptr::null_mut();
                (*intel_fb).frontbuffer = ptr::null_mut();
            }
            return -EINVAL;
        }
        unsafe {
            (*fb).obj[plane] = obj;
        }
    }
    let ret = unsafe { intel_fill_fb_info(display, intel_fb) };
    if ret != 0 {
        unsafe {
            intel_bo_framebuffer_fini(obj.cast());
            intel_parent_frontbuffer_put(display, front);
            kfree(panic);
            (*intel_fb).panic = ptr::null_mut();
            (*intel_fb).frontbuffer = ptr::null_mut();
        }
        return ret;
    }
    if unsafe { intel_fb_uses_dpt(fb) } {
        let mut size = 0usize;
        if unsafe { intel_fb_needs_pot_stride_remap(intel_fb) } {
            let info_ptr = unsafe { fb_remapped_info(intel_fb) };
            size = unsafe { intel_remapped_info_size(info_ptr) as usize };
        }
        let dpt = unsafe { intel_parent_dpt_create(display, obj.cast(), size) };
        if dpt.is_null() || unsafe { tk_linux_kapi::config::IS_ERR(dpt) } {
            let error = if dpt.is_null() {
                -ENODEV
            } else {
                unsafe { tk_linux_kapi::config::PTR_ERR(dpt) }
            };
            unsafe {
                intel_bo_framebuffer_fini(obj.cast());
                intel_parent_frontbuffer_put(display, front);
                kfree(panic);
                (*intel_fb).panic = ptr::null_mut();
                (*intel_fb).frontbuffer = ptr::null_mut();
            }
            return error;
        }
        unsafe {
            (*intel_fb).dpt = dpt;
        }
    }
    let ret = unsafe {
        tk_drm_upstream::drm_framebuffer_upstream::drm_framebuffer_init(
            (*display).drm,
            fb,
            &INTEL_FRAMEBUFFER_FUNCS,
        )
    };
    if ret != 0 {
        if unsafe { intel_fb_uses_dpt(fb) } {
            unsafe {
                intel_parent_dpt_destroy(display, (*intel_fb).dpt);
            }
        }
        unsafe {
            intel_bo_framebuffer_fini(obj.cast());
            intel_parent_frontbuffer_put(display, front);
            kfree(panic);
            (*intel_fb).panic = ptr::null_mut();
            (*intel_fb).frontbuffer = ptr::null_mut();
            (*intel_fb).dpt = ptr::null_mut();
        }
        return ret;
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_framebuffer_create(
    obj: *mut DrmGemObject,
    info: *const DrmFormatInfo,
    mode_cmd: *mut DrmModeFbCmd2,
) -> *mut DrmFramebuffer {
    let intel_fb = unsafe { intel_framebuffer_alloc() };
    if intel_fb.is_null() {
        return tk_linux_kapi::config::ERR_PTR(-ENOMEM);
    }
    let ret = unsafe { intel_framebuffer_init(intel_fb, obj, info, mode_cmd) };
    if ret != 0 {
        unsafe {
            kfree(intel_fb.cast::<c_void>());
        }
        tk_linux_kapi::config::ERR_PTR(ret)
    } else {
        unsafe { ptr::addr_of_mut!((*intel_fb).base) }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_user_framebuffer_create(
    dev: *mut DrmDevice,
    filp: *mut DrmFile,
    info: *mut DrmFormatInfo,
    user_mode_cmd: *mut DrmModeFbCmd2,
) -> *mut DrmFramebuffer {
    if dev.is_null() || filp.is_null() || info.is_null() || user_mode_cmd.is_null() {
        return tk_linux_kapi::config::ERR_PTR(-EINVAL);
    }
    let display = unsafe { __drm_to_display(dev) };
    let obj = unsafe { intel_bo_framebuffer_lookup(display, filp.cast(), user_mode_cmd.cast()) }
        .cast::<DrmGemObject>();
    if obj.is_null() || unsafe { tk_linux_kapi::config::IS_ERR(obj) } {
        return obj.cast();
    }
    let mode_cmd = unsafe { ptr::read(user_mode_cmd) };
    let fb = unsafe { intel_framebuffer_create(obj, info, ptr::addr_of!(mode_cmd).cast_mut()) };
    unsafe {
        tk_drm_upstream::drm_gem_upstream::drm_gem_object_put(obj);
    }
    fb
}

unsafe fn view_as_remapped_info(
    view: *const IntelFbView,
) -> *const crate::i915_gtt_view_types_upstream::IntelRemappedInfo {
    if view.is_null() {
        return ptr::null();
    }
    unsafe {
        view.cast::<u8>()
            .add(
                offset_of!(IntelFbView, gtt)
                    + offset_of!(crate::i915_gtt_view_types_upstream::I915GttView, _anon_4_1),
            )
            .cast::<crate::i915_gtt_view_types_upstream::IntelRemappedInfo>()
    }
}

unsafe fn fb_remapped_info(
    fb: *const IntelFramebuffer,
) -> *const crate::i915_gtt_view_types_upstream::IntelRemappedInfo {
    if fb.is_null() {
        return ptr::null();
    }
    unsafe {
        fb.cast::<u8>()
            .add(
                offset_of!(IntelFramebuffer, _anon_336_3)
                    + offset_of!(IntelFramebufferAnon336x3, remapped_view)
                    + offset_of!(IntelFbView, gtt)
                    + offset_of!(crate::i915_gtt_view_types_upstream::I915GttView, _anon_4_1),
            )
            .cast::<crate::i915_gtt_view_types_upstream::IntelRemappedInfo>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framebuffer_object_extent_accepts_exact_end_and_rejects_truncation() {
        assert_eq!(fb_required_bytes(4, 4096, 16 * 1024), Ok(16 * 1024));
        assert_eq!(
            fb_required_bytes(4, 4096, 16 * 1024 - 1),
            Err(-tk_linux_kapi::config::EINVAL)
        );
        assert_eq!(fb_required_bytes(0, 4096, 0), Ok(0));
    }

    #[test]
    fn dpt_remapped_size_aligns_each_plane_to_2m_tiles() {
        let mut view = unsafe { MaybeUninit::<IntelFbView>::zeroed().assume_init() };
        unsafe {
            init_fb_view(&mut view, I915_GTT_VIEW_REMAPPED, 512);
            set_remapped_plane(&mut view, 0, 0, false, 8, 2, 4, 4, 0);
            set_remapped_plane(&mut view, 1, 0, true, 0, 0, 0, 0, 3);
        }
        let info = unsafe { view_as_remapped_info(ptr::addr_of!(view)) };
        assert_eq!(unsafe { intel_remapped_info_size(info) }, 512 + 3);
    }

    #[test]
    fn gen12_modifier_format_overrides_preserve_plane_geometry() {
        unsafe {
            let rgb =
                intel_fb_get_format_info(DRM_FORMAT_XRGB8888, I915_FORMAT_MOD_Y_TILED_GEN12_RC_CCS);
            assert!(!rgb.is_null());
            assert_eq!((*rgb).num_planes, 2);
            assert_eq!((*rgb).block_w[1], 2);
            assert_eq!((*rgb).block_h[1], 1);
            assert_eq!((*rgb).vsub, 1);

            let nv12 =
                intel_fb_get_format_info(DRM_FORMAT_NV12, I915_FORMAT_MOD_Y_TILED_GEN12_RC_CCS);
            assert!(!nv12.is_null());
            assert_eq!((*nv12).num_planes, 4);
            assert_eq!((*nv12).block_w[2], 4);
            assert_eq!((*nv12).hsub, 2);
            assert_eq!((*nv12).vsub, 2);
        }
    }
}
