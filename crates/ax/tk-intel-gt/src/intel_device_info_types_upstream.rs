// SPDX-License-Identifier: MIT
// Copyright © 2016 Intel Corporation.
// Translated from Linux v7.2.3 drivers/gpu/drm/i915/intel_device_info.h
// (enum intel_platform, subplatform bits, DEV_INFO_FOR_EACH_FLAG, struct
// intel_device_info, struct intel_driver_caps). The complete MIT grant is
// retained in ../LICENSE-MIT.
//
// Layout: `IntelDeviceInfo` is the x86_64 Linux 7.2.3 C layout (96 bytes),
// matching the byte overlay used by linux::i915::INTEL_INFO.

#![allow(unsafe_code, non_snake_case, dead_code)]

use core::{
    ffi::c_void,
    mem::{offset_of, size_of},
};

use crate::linux::i915::IntelRuntimeInfo;

/// `enum intel_platform` (intel_device_info.h). Values are C enum indices.
pub const INTEL_PLATFORM_UNINITIALIZED: u32 = 0;
pub const INTEL_I830: u32 = 1;
pub const INTEL_I845G: u32 = 2;
pub const INTEL_I85X: u32 = 3;
pub const INTEL_I865G: u32 = 4;
pub const INTEL_I915G: u32 = 5;
pub const INTEL_I915GM: u32 = 6;
pub const INTEL_I945G: u32 = 7;
pub const INTEL_I945GM: u32 = 8;
pub const INTEL_G33: u32 = 9;
pub const INTEL_PINEVIEW: u32 = 10;
pub const INTEL_I965G: u32 = 11;
pub const INTEL_I965GM: u32 = 12;
pub const INTEL_G45: u32 = 13;
pub const INTEL_GM45: u32 = 14;
pub const INTEL_IRONLAKE: u32 = 15;
pub const INTEL_SANDYBRIDGE: u32 = 16;
pub const INTEL_IVYBRIDGE: u32 = 17;
pub const INTEL_VALLEYVIEW: u32 = 18;
pub const INTEL_HASWELL: u32 = 19;
pub const INTEL_BROADWELL: u32 = 20;
pub const INTEL_CHERRYVIEW: u32 = 21;
pub const INTEL_SKYLAKE: u32 = 22;
pub const INTEL_BROXTON: u32 = 23;
pub const INTEL_KABYLAKE: u32 = 24;
pub const INTEL_GEMINILAKE: u32 = 25;
pub const INTEL_COFFEELAKE: u32 = 26;
pub const INTEL_COMETLAKE: u32 = 27;
pub const INTEL_ICELAKE: u32 = 28;
pub const INTEL_ELKHARTLAKE: u32 = 29;
pub const INTEL_JASPERLAKE: u32 = 30;
pub const INTEL_TIGERLAKE: u32 = 31;
pub const INTEL_ROCKETLAKE: u32 = 32;
pub const INTEL_DG1: u32 = 33;
pub const INTEL_ALDERLAKE_S: u32 = 34;
pub const INTEL_ALDERLAKE_P: u32 = 35;
pub const INTEL_DG2: u32 = 36;
pub const INTEL_METEORLAKE: u32 = 37;

pub const INTEL_SUBPLATFORM_BITS: u32 = 4;
pub const INTEL_SUBPLATFORM_MASK: u32 = (1 << INTEL_SUBPLATFORM_BITS) - 1;
pub const INTEL_SUBPLATFORM_ULT: u32 = 0;
pub const INTEL_SUBPLATFORM_ULX: u32 = 1;
pub const INTEL_SUBPLATFORM_PORTF: u32 = 0;
pub const INTEL_SUBPLATFORM_UY: u32 = 0;
pub const INTEL_SUBPLATFORM_G10: u32 = 0;
pub const INTEL_SUBPLATFORM_G11: u32 = 1;
pub const INTEL_SUBPLATFORM_G12: u32 = 2;
pub const INTEL_SUBPLATFORM_D: u32 = 3;
pub const INTEL_SUBPLATFORM_RPL: u32 = 0;
pub const INTEL_SUBPLATFORM_N: u32 = 1;
pub const INTEL_SUBPLATFORM_RPLU: u32 = 2;
pub const INTEL_SUBPLATFORM_ARL_H: u32 = 0;
pub const INTEL_SUBPLATFORM_ARL_U: u32 = 1;
pub const INTEL_SUBPLATFORM_ARL_S: u32 = 2;

/// `enum intel_ppgtt_type` (I915_GEM_PPGTT_* values).
pub const INTEL_PPGTT_NONE: i32 = 0;
pub const INTEL_PPGTT_ALIASING: i32 = 1;
pub const INTEL_PPGTT_FULL: i32 = 2;

/// Bit indexes of `DEV_INFO_FOR_EACH_FLAG`, in declaration order (each is a
/// one-bit field of `struct intel_device_info`, LSB first on x86_64 gcc).
pub const DEV_INFO_FLAG_IS_MOBILE: u32 = 0;
pub const DEV_INFO_FLAG_REQUIRE_FORCE_PROBE: u32 = 1;
pub const DEV_INFO_FLAG_IS_DGFX: u32 = 2;
pub const DEV_INFO_FLAG_HAS_64BIT_RELOC: u32 = 3;
pub const DEV_INFO_FLAG_HAS_64K_PAGES: u32 = 4;
pub const DEV_INFO_FLAG_GPU_RESET_CLOBBERS_DISPLAY: u32 = 5;
pub const DEV_INFO_FLAG_HAS_RESET_ENGINE: u32 = 6;
pub const DEV_INFO_FLAG_HAS_3D_PIPELINE: u32 = 7;
pub const DEV_INFO_FLAG_HAS_FLAT_CCS: u32 = 8;
pub const DEV_INFO_FLAG_HAS_GLOBAL_MOCS: u32 = 9;
pub const DEV_INFO_FLAG_HAS_GMD_ID: u32 = 10;
pub const DEV_INFO_FLAG_HAS_GT_UC: u32 = 11;
pub const DEV_INFO_FLAG_HAS_HECI_PXP: u32 = 12;
pub const DEV_INFO_FLAG_HAS_HECI_GSCFI: u32 = 13;
pub const DEV_INFO_FLAG_HAS_GUC_DEPRIVILEGE: u32 = 14;
pub const DEV_INFO_FLAG_HAS_GUC_TLB_INVALIDATION: u32 = 15;
pub const DEV_INFO_FLAG_HAS_L3_CCS_READ: u32 = 16;
pub const DEV_INFO_FLAG_HAS_L3_DPF: u32 = 17;
pub const DEV_INFO_FLAG_HAS_LLC: u32 = 18;
pub const DEV_INFO_FLAG_HAS_LOGICAL_RING_CONTEXTS: u32 = 19;
pub const DEV_INFO_FLAG_HAS_LOGICAL_RING_ELSQ: u32 = 20;
pub const DEV_INFO_FLAG_HAS_MEDIA_RATIO_MODE: u32 = 21;
pub const DEV_INFO_FLAG_HAS_MSLICE_STEERING: u32 = 22;
pub const DEV_INFO_FLAG_HAS_OA_BPC_REPORTING: u32 = 23;
pub const DEV_INFO_FLAG_HAS_OA_SLICE_CONTRIB_LIMITS: u32 = 24;
pub const DEV_INFO_FLAG_HAS_OAM: u32 = 25;
pub const DEV_INFO_FLAG_HAS_ONE_EU_PER_FUSE_BIT: u32 = 26;
pub const DEV_INFO_FLAG_HAS_PXP: u32 = 27;
pub const DEV_INFO_FLAG_HAS_RC6: u32 = 28;
pub const DEV_INFO_FLAG_HAS_RC6P: u32 = 29;
pub const DEV_INFO_FLAG_HAS_RPS: u32 = 30;
pub const DEV_INFO_FLAG_HAS_RUNTIME_PM: u32 = 31;
pub const DEV_INFO_FLAG_HAS_SNOOP: u32 = 32;
pub const DEV_INFO_FLAG_HAS_COHERENT_GGTT: u32 = 33;
pub const DEV_INFO_FLAG_TUNING_THREAD_RR_AFTER_DEP: u32 = 34;
pub const DEV_INFO_FLAG_UNFENCED_NEEDS_ALIGNMENT: u32 = 35;
pub const DEV_INFO_FLAG_HWS_NEEDS_PHYSICAL: u32 = 36;

/// `struct intel_device_info` (intel_device_info.h), x86_64 Linux 7.2.3 layout.
#[repr(C, align(8))]
pub struct IntelDeviceInfo {
    pub platform: u32,
    pub dma_mask_size: u32,
    pub extra_gt_list: *const c_void,
    pub gt: u8,
    _pad_gt: [u8; 3],
    pub platform_engine_mask: u32,
    pub memory_regions: u32,
    /// Packed `DEV_INFO_FOR_EACH_FLAG` bitfields (37 bits in 5 bytes).
    pub flags: [u8; 5],
    _pad_flags: [u8; 3],
    /// Initial runtime info (`__runtime`); copied by driver_create.
    pub runtime: IntelRuntimeInfo,
    pub cachelevel_to_pat: [u32; 4],
    pub max_pat_index: u32,
    _tail: [u8; 4],
}

const _: [(); 96] = [(); size_of::<IntelDeviceInfo>()];
const _: [(); 8] = [(); core::mem::align_of::<IntelDeviceInfo>()];
const _: [(); 16] = [(); offset_of!(IntelDeviceInfo, gt)];
const _: [(); 20] = [(); offset_of!(IntelDeviceInfo, platform_engine_mask)];
const _: [(); 24] = [(); offset_of!(IntelDeviceInfo, memory_regions)];
const _: [(); 28] = [(); offset_of!(IntelDeviceInfo, flags)];
const _: [(); 36] = [(); offset_of!(IntelDeviceInfo, runtime)];
const _: [(); 72] = [(); offset_of!(IntelDeviceInfo, cachelevel_to_pat)];
const _: [(); 88] = [(); offset_of!(IntelDeviceInfo, max_pat_index)];


impl IntelDeviceInfo {
    /// Reads one `DEV_INFO_FOR_EACH_FLAG` bit by its declaration index.
    #[inline]
    pub const fn flag(&self, bit: u32) -> bool {
        (self.flags[(bit / 8) as usize] >> (bit % 8)) & 1 != 0
    }
}

/// Builds the packed flag bytes for a static device-info initializer; the
/// argument list is the `DEV_INFO_FLAG_*` indexes that the C designated
/// initializer sets to 1.
pub const fn dev_info_flags(bits: &[u32]) -> [u8; 5] {
    let mut out = [0u8; 5];
    let mut i = 0;
    while i < bits.len() {
        out[(bits[i] / 8) as usize] |= 1 << (bits[i] % 8);
        i += 1;
    }
    out
}

/// `struct intel_driver_caps` (intel_device_info.h).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct IntelDriverCaps {
    pub scheduler: u32,
    pub has_logical_contexts: bool,
}

// SAFETY: device-info statics are read-only after const evaluation; the only
// pointer member (`extra_gt_list`) refers to another immutable static table.
unsafe impl Sync for IntelDeviceInfo {}

impl IntelDeviceInfo {
    /// All-zero initializer for the static designated-initializer tables in
    /// i915_pci.c (C zero-initializes every member not named in the table).
    pub const fn zeroed() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
