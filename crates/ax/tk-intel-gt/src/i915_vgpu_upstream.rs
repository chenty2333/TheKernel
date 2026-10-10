// SPDX-License-Identifier: MIT
// Copyright © 2009-2011 Intel Corporation.
// Source-order translation of Linux 7.2.3 drivers/gpu/drm/i915/i915_vgpu.c.
#![allow(non_snake_case, unsafe_op_in_unsafe_fn)]

use core::ffi::c_int;

use crate::{intel_gtt_api_upstream::I915Ggtt, linux_i915_private::DrmI915Private};

// Linux `dev_priv->vgpu.active`. TheKernel never runs as a GVT-g guest: the
// PVINFO detection (`intel_vgpu_detect()`) requires a BAR0 provider that the
// kernel owner does not install, so the state stays at its native-hardware
// value `false`. Any guest-mode path below fails closed rather than ballooning.
const VGPU_ACTIVE: bool = false;
// Linux `dev_priv->vgpu.caps`; zero on native hardware.
const VGPU_CAPS: u32 = 0;
const VGT_CAPS_HUGE_GTT: u32 = 1 << 2;

// upstream: i915_vgpu.c intel_vgpu_active()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_vgpu_active(_dev_priv: *mut DrmI915Private) -> bool {
    VGPU_ACTIVE
}

// upstream: i915_vgpu.c intel_vgpu_has_huge_gtt()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_vgpu_has_huge_gtt(_dev_priv: *mut DrmI915Private) -> bool {
    VGPU_CAPS & VGT_CAPS_HUGE_GTT != 0
}

// upstream: i915_vgpu.c intel_vgt_deballoon()
//
// The ballooned ranges (`bl_info`) are only created on a vGPU guest. Native
// hardware returns at the `intel_vgpu_active()` check; the guest path cannot
// be reached because `intel_vgt_balloon()` fails closed first.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_vgt_deballoon(ggtt: *mut I915Ggtt) {
    if !VGPU_ACTIVE {
        return;
    }
    panic!("intel_vgt_deballoon({ggtt:p}): vGPU ballooning (GVT-g guest) is not supported by TheKernel");
}

// upstream: i915_vgpu.c intel_vgt_balloon()
//
// Linux returns 0 when `intel_vgpu_active()` is false, which is the native
// case. The guest balloon path needs PVINFO resource data that TheKernel does
// not provide, so it fails closed instead of reserving unknown GGTT ranges.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_vgt_balloon(ggtt: *mut I915Ggtt) -> c_int {
    if !VGPU_ACTIVE {
        return 0;
    }
    panic!("intel_vgt_balloon({ggtt:p}): vGPU ballooning (GVT-g guest) is not supported by TheKernel");
}
