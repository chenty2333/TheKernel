// SPDX-License-Identifier: MIT
// Copyright © 2026 TheKernel contributors.
// Linux v7.2.3 drivers/gpu/drm/i915/i915_drv.h configuration-specific overlay.
// Directly accessed members use the x86_64 wt-dev C layout probe; untouched
// configuration-sensitive regions remain opaque byte ranges.

use core::{
    ffi::{c_char, c_ulong, c_void},
    mem::{align_of, offset_of, size_of},
};

use crate::{
    intel_engine_cs_upstream::{AtomicT, IntelEngineCs, IntelGt, Spinlock},
    linux::i915::IntelRuntimeInfo,
    linux_memory::{atomic_inc, atomic_read},
};

/// The opaque storage of the leading `struct drm_device` in the target
/// x86_64 wt-dev configuration. No drm_device members are accessed through
/// this representation; its size/alignment preserve the following offsets.
#[repr(C, align(8))]
pub struct DrmDevicePrefix {
    _bytes: [u8; DRM_DEVICE_SIZE],
}

pub const DRM_DEVICE_SIZE: usize = 1552;

/// Complete `struct i915_params` member sequence from i915_params.h.
#[repr(C)]
pub struct I915Params {
    pub modeset: i32,
    pub enable_guc: i32,
    pub guc_log_level: i32,
    pub guc_firmware_path: *mut c_char,
    pub huc_firmware_path: *mut c_char,
    pub gsc_firmware_path: *mut c_char,
    pub memtest: bool,
    pub mmio_debug: i32,
    pub reset: u32,
    pub force_probe: *mut c_char,
    pub request_timeout_ms: u32,
    pub lmem_size: u32,
    pub lmem_bar_size: u32,
    pub enable_hangcheck: bool,
    pub error_capture: bool,
    pub enable_gvt: bool,
    pub enable_debug_only_api: bool,
}

/// Linux `struct i915_gpu_error` layout used by the reset-counter inline.
#[repr(C)]
pub struct I915GpuError {
    pub lock: Spinlock,
    pub first_error: *mut c_void,
    pub reset_count: AtomicT,
    pub reset_engine_count: [AtomicT; 5],
}

/// Only the aligned prefix of the embedded `intel_runtime_pm` is modeled;
/// callers pass its address to an installed opaque runtime-PM backend.
#[repr(C, align(8))]
pub struct IntelRuntimePmPrefix {
    _opaque: [u8; 8],
}

/// Source-derived Linux 7.2.3 `drm_i915_private` ABI overlay for the x86_64
/// wt-dev configuration. Members not directly accessed by this port are
/// represented as opaque byte ranges sized from the target C layout probe.
/// Regenerate the asserted offsets when the Linux config/compiler changes.
#[repr(C)]
pub struct DrmI915Private {
    pub drm: DrmDevicePrefix,
    pub display: *mut c_void,
    pub do_release: bool,
    _align_params: [u8; 7],
    pub params: I915Params,
    pub info: *const c_void,
    pub runtime: IntelRuntimeInfo,
    _before_unordered_wq: [u8; 852],
    pub unordered_wq: *mut c_void,
    _before_gem_quirks: [u8; 8],
    pub gem_quirks: c_ulong,
    _before_edram_size: [u8; 544],
    pub edram_size_mb: u32,
    _align_gpu_error: [u8; 4],
    pub gpu_error: I915GpuError,
    pub suspend_count: u32,
    pub vlv_s0ix_state: *mut c_void,
    pub runtime_pm: IntelRuntimePmPrefix,
    _before_gt: [u8; 344],
    pub gt: [*mut IntelGt; I915_MAX_GT],
    pub sysfs_gt: *mut c_void,
    pub media_gt: *mut IntelGt,
    _configuration_sensitive_tail: [u8; 2368],
}

pub const I915_MAX_GT: usize = 2;

/// Source inline helper from i915_gpu_error.h.
#[inline]
pub fn i915_increase_reset_engine_count(error: &mut I915GpuError, engine: *const IntelEngineCs) {
    assert!(!engine.is_null());
    let class = unsafe { (*engine).class as usize };
    assert!(class < error.reset_engine_count.len());
    atomic_inc(&mut error.reset_engine_count[class]);
}

/// `i915_reset_count()` from i915_gpu_error.h.
pub unsafe fn i915_reset_count(error: *const I915GpuError) -> u32 {
    assert!(!error.is_null());
    atomic_read(unsafe { &(*error).reset_count }) as u32
}

/// `i915_reset_engine_count()` from i915_gpu_error.h.
pub unsafe fn i915_reset_engine_count(
    error: *const I915GpuError,
    engine: *const IntelEngineCs,
) -> u32 {
    assert!(!error.is_null() && !engine.is_null());
    let class = unsafe { (*engine).class as usize };
    assert!(
        class < 5,
        "engine class is outside the source reset counter array"
    );
    atomic_read(unsafe { &(*error).reset_engine_count[class] }) as u32
}

const _: [(); 1552] = [(); size_of::<DrmDevicePrefix>()];
const _: [(); 8] = [(); align_of::<DrmDevicePrefix>()];

const _: [(); 80] = [(); size_of::<I915Params>()];
const _: [(); 8] = [(); align_of::<I915Params>()];
const _: [(); 2560] = [(); offset_of!(DrmI915Private, gem_quirks)];
const _: [(); 3112] = [(); offset_of!(DrmI915Private, edram_size_mb)];
const _: [(); 3120] = [(); offset_of!(DrmI915Private, gpu_error)];
const _: [(); 4] = [(); offset_of!(I915Params, enable_guc)];
const _: [(); 8] = [(); offset_of!(I915Params, guc_log_level)];
const _: [(); 16] = [(); offset_of!(I915Params, guc_firmware_path)];
const _: [(); 40] = [(); offset_of!(I915Params, memtest)];
const _: [(); 44] = [(); offset_of!(I915Params, mmio_debug)];
const _: [(); 48] = [(); offset_of!(I915Params, reset)];
const _: [(); 56] = [(); offset_of!(I915Params, force_probe)];
const _: [(); 64] = [(); offset_of!(I915Params, request_timeout_ms)];
const _: [(); 76] = [(); offset_of!(I915Params, enable_hangcheck)];

const _: [(); 0] = [(); offset_of!(DrmI915Private, drm)];
const _: [(); 1552] = [(); offset_of!(DrmI915Private, display)];
const _: [(); 1560] = [(); offset_of!(DrmI915Private, do_release)];
const _: [(); 1568] = [(); offset_of!(DrmI915Private, params)];
const _: [(); 1572] = [(); offset_of!(DrmI915Private, params.enable_guc)];
const _: [(); 1648] = [(); offset_of!(DrmI915Private, info)];
const _: [(); 1656] = [(); offset_of!(DrmI915Private, runtime)];
const _: [(); 2544] = [(); offset_of!(DrmI915Private, unordered_wq)];
const _: [(); 3120] = [(); offset_of!(DrmI915Private, gpu_error)];
const _: [(); 3176] = [(); offset_of!(DrmI915Private, runtime_pm)];
const _: [(); 3528] = [(); offset_of!(DrmI915Private, gt)];
const _: [(); 5928] = [(); size_of::<DrmI915Private>()];
const _: [(); 40] = [(); size_of::<I915GpuError>()];
const _: [(); 8] = [(); size_of::<IntelRuntimePmPrefix>()];
