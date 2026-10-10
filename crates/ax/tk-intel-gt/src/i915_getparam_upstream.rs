// SPDX-License-Identifier: MIT
// Source-order translation of Linux v7.2.3 drivers/gpu/drm/i915/i915_getparam.c.

#![allow(dead_code, non_camel_case_types, non_snake_case, unsafe_code)]

use core::{
    ffi::{c_int, c_void},
    mem::{offset_of, size_of},
    ptr,
};

use crate::{
    i915_cmd_parser_upstream::i915_cmd_parser_get_version,
    i915_gem_mman_upstream::i915_gem_mmap_gtt_version,
    intel_engine_user_upstream::{intel_engine_lookup_user, intel_engines_has_context_isolation},
    intel_gt_types_upstream::IntelGt,
    intel_huc_upstream::intel_huc_check_status,
    intel_sseu_types_upstream::{SseuDevInfo, intel_sseu_get_hsw_subslices},
    intel_sseu_upstream::intel_sseu_subslice_total,
    intel_uc_types_upstream::intel_uc_uses_guc_submission,
    linux::{
        i915::{
            GRAPHICS_VER, GRAPHICS_VER_FULL, HAS_LLC, HAS_POOLED_EU, HAS_WT, INTEL_INFO, IP_VER,
            IntelRuntimeInfo, i915_pci_revision, to_gt, to_i915,
        },
        i915_private::DrmI915Private,
        mm_native::copy_to_user,
    },
    linux_config::{EFAULT, EINVAL, ENODEV},
};

// include/uapi/drm/i915_drm.h Linux 7.2.3 getparam values.
const I915_PARAM_IRQ_ACTIVE: c_int = 1;
const I915_PARAM_ALLOW_BATCHBUFFER: c_int = 2;
const I915_PARAM_LAST_DISPATCH: c_int = 3;
const I915_PARAM_CHIPSET_ID: c_int = 4;
const I915_PARAM_HAS_GEM: c_int = 5;
const I915_PARAM_NUM_FENCES_AVAIL: c_int = 6;
const I915_PARAM_HAS_OVERLAY: c_int = 7;
const I915_PARAM_HAS_PAGEFLIPPING: c_int = 8;
const I915_PARAM_HAS_EXECBUF2: c_int = 9;
const I915_PARAM_HAS_BSD: c_int = 10;
const I915_PARAM_HAS_BLT: c_int = 11;
const I915_PARAM_HAS_RELAXED_FENCING: c_int = 12;
const I915_PARAM_HAS_COHERENT_RINGS: c_int = 13;
const I915_PARAM_HAS_EXEC_CONSTANTS: c_int = 14;
const I915_PARAM_HAS_RELAXED_DELTA: c_int = 15;
const I915_PARAM_HAS_GEN7_SOL_RESET: c_int = 16;
const I915_PARAM_HAS_LLC: c_int = 17;
const I915_PARAM_HAS_ALIASING_PPGTT: c_int = 18;
const I915_PARAM_HAS_WAIT_TIMEOUT: c_int = 19;
const I915_PARAM_HAS_SEMAPHORES: c_int = 20;
const I915_PARAM_HAS_PRIME_VMAP_FLUSH: c_int = 21;
const I915_PARAM_HAS_VEBOX: c_int = 22;
const I915_PARAM_HAS_SECURE_BATCHES: c_int = 23;
const I915_PARAM_HAS_PINNED_BATCHES: c_int = 24;
const I915_PARAM_HAS_EXEC_NO_RELOC: c_int = 25;
const I915_PARAM_HAS_EXEC_HANDLE_LUT: c_int = 26;
const I915_PARAM_HAS_WT: c_int = 27;
const I915_PARAM_CMD_PARSER_VERSION: c_int = 28;
const I915_PARAM_HAS_COHERENT_PHYS_GTT: c_int = 29;
const I915_PARAM_MMAP_VERSION: c_int = 30;
const I915_PARAM_HAS_BSD2: c_int = 31;
const I915_PARAM_REVISION: c_int = 32;
const I915_PARAM_SUBSLICE_TOTAL: c_int = 33;
const I915_PARAM_EU_TOTAL: c_int = 34;
const I915_PARAM_HAS_GPU_RESET: c_int = 35;
const I915_PARAM_HAS_RESOURCE_STREAMER: c_int = 36;
const I915_PARAM_HAS_EXEC_SOFTPIN: c_int = 37;
const I915_PARAM_HAS_POOLED_EU: c_int = 38;
const I915_PARAM_MIN_EU_IN_POOL: c_int = 39;
const I915_PARAM_MMAP_GTT_VERSION: c_int = 40;
const I915_PARAM_HAS_SCHEDULER: c_int = 41;
const I915_PARAM_HUC_STATUS: c_int = 42;
const I915_PARAM_HAS_EXEC_ASYNC: c_int = 43;
const I915_PARAM_HAS_EXEC_FENCE: c_int = 44;
const I915_PARAM_HAS_EXEC_CAPTURE: c_int = 45;
const I915_PARAM_SLICE_MASK: c_int = 46;
const I915_PARAM_SUBSLICE_MASK: c_int = 47;
const I915_PARAM_HAS_EXEC_BATCH_FIRST: c_int = 48;
const I915_PARAM_HAS_EXEC_FENCE_ARRAY: c_int = 49;
const I915_PARAM_HAS_CONTEXT_ISOLATION: c_int = 50;
const I915_PARAM_CS_TIMESTAMP_FREQUENCY: c_int = 51;
const I915_PARAM_MMAP_GTT_COHERENT: c_int = 52;
const I915_PARAM_HAS_EXEC_SUBMIT_FENCE: c_int = 53;
const I915_PARAM_PERF_REVISION: c_int = 54;
const I915_PARAM_HAS_EXEC_TIMELINE_FENCES: c_int = 55;
const I915_PARAM_HAS_USERPTR_PROBE: c_int = 56;
const I915_PARAM_OA_TIMESTAMP_FREQUENCY: c_int = 57;
const I915_PARAM_PXP_STATUS: c_int = 58;
const I915_PARAM_HAS_CONTEXT_FREQ_HINT: c_int = 59;

const I915_ENGINE_CLASS_COPY: u8 = 1;
const I915_ENGINE_CLASS_VIDEO: u8 = 2;
const I915_ENGINE_CLASS_VIDEO_ENHANCE: u8 = 3;
const I915_SCHEDULER_CAP_SEMAPHORES: u32 = 1 << 3;
const CAP_SYS_ADMIN: c_int = 21;

#[repr(C)]
struct DrmI915GetParam {
    param: c_int,
    value: *mut c_int,
}
const _: [(); 16] = [(); size_of::<DrmI915GetParam>()];
const _: [(); 8] = [(); offset_of!(DrmI915GetParam, value)];

#[repr(C)]
struct IntelDriverCapsView {
    scheduler: u32,
}
const _: [(); 0] = [(); offset_of!(IntelDriverCapsView, scheduler)];

unsafe extern "C" {
    fn capable(capability: c_int) -> bool;
    fn intel_overlay_available(display: *mut c_void) -> bool;
    fn intel_has_gpu_reset(gt: *const IntelGt) -> bool;
    fn intel_has_reset_engine(gt: *const IntelGt) -> bool;
    fn intel_pxp_get_readiness_status(pxp: *mut c_void, timeout_ms: c_int) -> c_int;
    fn i915_perf_ioctl_version(i915: *mut DrmI915Private) -> c_int;
    fn i915_perf_oa_timestamp_frequency(i915: *mut DrmI915Private) -> u32;
}

#[inline]
unsafe fn i915_scheduler_caps(i915: *mut DrmI915Private) -> u32 {
    // In Linux 7.2.3, intel_driver_caps immediately follows __runtime.
    let caps = unsafe {
        i915.cast::<u8>()
            .add(offset_of!(DrmI915Private, runtime) + size_of::<IntelRuntimeInfo>())
            .cast::<IntelDriverCapsView>()
    };
    unsafe { ptr::read_volatile(ptr::addr_of!((*caps).scheduler)) }
}

#[inline]
unsafe fn has_coherent_ggtt(i915: *mut DrmI915Private) -> bool {
    let info = unsafe { INTEL_INFO(i915) };
    assert!(
        !info.is_null(),
        "INTEL_INFO is unavailable after device setup"
    );
    unsafe { (*info).flags[4] & (1 << 1) != 0 }
}

// upstream: i915_getparam.c i915_getparam_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_getparam_ioctl(
    dev: *mut c_void,
    data: *mut c_void,
    _file_priv: *mut c_void,
) -> c_int {
    let i915 = unsafe { to_i915(dev) };
    let display = unsafe { (*i915).display };
    let gt = unsafe { to_gt(i915) };
    let sseu: *const SseuDevInfo = unsafe { ptr::addr_of!((*gt).info.sseu) };
    let param = data.cast::<DrmI915GetParam>();
    let mut value: c_int = 0;

    match unsafe { (*param).param } {
        I915_PARAM_IRQ_ACTIVE
        | I915_PARAM_ALLOW_BATCHBUFFER
        | I915_PARAM_LAST_DISPATCH
        | I915_PARAM_HAS_EXEC_CONSTANTS => return -ENODEV,
        I915_PARAM_CHIPSET_ID => value = unsafe { (*i915).runtime.device_id as c_int },
        I915_PARAM_REVISION => {
            value = match unsafe { i915_pci_revision(i915) } {
                Ok(revision) => revision as c_int,
                Err(error) => return error,
            };
        }
        I915_PARAM_NUM_FENCES_AVAIL => {
            value = unsafe { (*(*gt).ggtt).num_fences as c_int };
        }
        I915_PARAM_HAS_OVERLAY => {
            value = unsafe { intel_overlay_available(display) as c_int };
        }
        I915_PARAM_HAS_BSD => {
            value = unsafe {
                (!intel_engine_lookup_user(i915, I915_ENGINE_CLASS_VIDEO, 0).is_null()) as c_int
            };
        }
        I915_PARAM_HAS_BLT => {
            value = unsafe {
                (!intel_engine_lookup_user(i915, I915_ENGINE_CLASS_COPY, 0).is_null()) as c_int
            };
        }
        I915_PARAM_HAS_VEBOX => {
            value = unsafe {
                (!intel_engine_lookup_user(i915, I915_ENGINE_CLASS_VIDEO_ENHANCE, 0).is_null())
                    as c_int
            };
        }
        I915_PARAM_HAS_BSD2 => {
            value = unsafe {
                (!intel_engine_lookup_user(i915, I915_ENGINE_CLASS_VIDEO, 1).is_null()) as c_int
            };
        }
        I915_PARAM_HAS_LLC => value = unsafe { HAS_LLC(i915) as c_int },
        I915_PARAM_HAS_WT => value = unsafe { HAS_WT(i915) as c_int },
        I915_PARAM_HAS_ALIASING_PPGTT => value = unsafe { (*i915).runtime.ppgtt_type },
        I915_PARAM_HAS_SEMAPHORES => {
            value = (unsafe { i915_scheduler_caps(i915) } & I915_SCHEDULER_CAP_SEMAPHORES != 0)
                as c_int;
        }
        I915_PARAM_HAS_SECURE_BATCHES => {
            value =
                (unsafe { GRAPHICS_VER(i915) } < 6 && unsafe { capable(CAP_SYS_ADMIN) }) as c_int;
        }
        I915_PARAM_CMD_PARSER_VERSION => {
            value = unsafe { i915_cmd_parser_get_version(i915) };
        }
        I915_PARAM_SUBSLICE_TOTAL => {
            value = unsafe { intel_sseu_subslice_total(sseu) as c_int };
            if value == 0 {
                return -ENODEV;
            }
        }
        I915_PARAM_EU_TOTAL => {
            value = unsafe { (*sseu).eu_total as c_int };
            if value == 0 {
                return -ENODEV;
            }
        }
        I915_PARAM_HAS_GPU_RESET => {
            value = (unsafe { (*i915).params.enable_hangcheck }
                && unsafe { intel_has_gpu_reset(gt) }) as c_int;
            if value != 0 && unsafe { intel_has_reset_engine(gt) } {
                value = 2;
            }
        }
        I915_PARAM_HAS_RESOURCE_STREAMER => value = 0,
        I915_PARAM_HAS_POOLED_EU => value = unsafe { HAS_POOLED_EU(i915) as c_int },
        I915_PARAM_MIN_EU_IN_POOL => value = unsafe { (*sseu).min_eu_in_pool as c_int },
        I915_PARAM_HUC_STATUS => {
            let media_gt = unsafe { (*i915).media_gt };
            value = if media_gt.is_null() {
                unsafe { intel_huc_check_status(ptr::addr_of_mut!((*gt).uc.huc)) }
            } else {
                unsafe { intel_huc_check_status(ptr::addr_of_mut!((*media_gt).uc.huc)) }
            };
            if value < 0 {
                return value;
            }
        }
        I915_PARAM_PXP_STATUS => {
            value = unsafe { intel_pxp_get_readiness_status((*i915).pxp, 0) };
            if value < 0 {
                return value;
            }
        }
        I915_PARAM_MMAP_GTT_VERSION => value = i915_gem_mmap_gtt_version(),
        I915_PARAM_HAS_SCHEDULER => value = unsafe { i915_scheduler_caps(i915) as c_int },
        I915_PARAM_MMAP_VERSION
        | I915_PARAM_HAS_GEM
        | I915_PARAM_HAS_PAGEFLIPPING
        | I915_PARAM_HAS_EXECBUF2
        | I915_PARAM_HAS_RELAXED_FENCING
        | I915_PARAM_HAS_COHERENT_RINGS
        | I915_PARAM_HAS_RELAXED_DELTA
        | I915_PARAM_HAS_GEN7_SOL_RESET
        | I915_PARAM_HAS_WAIT_TIMEOUT
        | I915_PARAM_HAS_PRIME_VMAP_FLUSH
        | I915_PARAM_HAS_PINNED_BATCHES
        | I915_PARAM_HAS_EXEC_NO_RELOC
        | I915_PARAM_HAS_EXEC_HANDLE_LUT
        | I915_PARAM_HAS_COHERENT_PHYS_GTT
        | I915_PARAM_HAS_EXEC_SOFTPIN
        | I915_PARAM_HAS_EXEC_ASYNC
        | I915_PARAM_HAS_EXEC_FENCE
        | I915_PARAM_HAS_EXEC_CAPTURE
        | I915_PARAM_HAS_EXEC_BATCH_FIRST
        | I915_PARAM_HAS_EXEC_FENCE_ARRAY
        | I915_PARAM_HAS_EXEC_SUBMIT_FENCE
        | I915_PARAM_HAS_EXEC_TIMELINE_FENCES
        | I915_PARAM_HAS_USERPTR_PROBE => value = 1,
        I915_PARAM_HAS_CONTEXT_FREQ_HINT => {
            if unsafe { intel_uc_uses_guc_submission(ptr::addr_of_mut!((*gt).uc)) } {
                value = 1;
            } else {
                return -EINVAL;
            }
        }
        I915_PARAM_HAS_CONTEXT_ISOLATION => {
            value = unsafe { intel_engines_has_context_isolation(i915) as c_int };
        }
        I915_PARAM_SLICE_MASK => {
            if unsafe { GRAPHICS_VER_FULL(i915) } >= IP_VER(12, 55) {
                return -EINVAL;
            }
            value = unsafe { (*sseu).slice_mask as c_int };
            if value == 0 {
                return -ENODEV;
            }
        }
        I915_PARAM_SUBSLICE_MASK => {
            if unsafe { GRAPHICS_VER_FULL(i915) } >= IP_VER(12, 55) {
                return -EINVAL;
            }
            value = unsafe { intel_sseu_get_hsw_subslices(sseu, 0) as c_int };
            if value == 0 {
                return -ENODEV;
            }
        }
        I915_PARAM_CS_TIMESTAMP_FREQUENCY => value = unsafe { (*gt).clock_frequency as c_int },
        I915_PARAM_MMAP_GTT_COHERENT => value = unsafe { has_coherent_ggtt(i915) as c_int },
        I915_PARAM_PERF_REVISION => value = unsafe { i915_perf_ioctl_version(i915) },
        I915_PARAM_OA_TIMESTAMP_FREQUENCY => {
            value = unsafe { i915_perf_oa_timestamp_frequency(i915) as c_int };
        }
        unknown => {
            drm_dbg!(unsafe { &(*i915).drm }, "Unknown parameter %d\n", unknown);
            return -EINVAL;
        }
    }

    if unsafe {
        copy_to_user(
            (*param).value.cast::<c_void>(),
            ptr::addr_of!(value).cast::<c_void>(),
            size_of::<c_int>(),
        )
    } != 0
    {
        return -EFAULT;
    }
    0
}
