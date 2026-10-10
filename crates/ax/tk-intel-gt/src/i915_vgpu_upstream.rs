// SPDX-License-Identifier: MIT
// Copyright © 2014 Intel Corporation
// Source: Linux v7.2.3 drivers/gpu/drm/i915/i915_vgpu.c (MIT).
#![allow(non_snake_case, unsafe_op_in_unsafe_fn)]

use core::ffi::{c_int, c_void};

use crate::{
    i915_probe_provider_upstream::early_gmd_read,
    intel_gtt_api_upstream::I915Ggtt,
    intel_uncore_types_upstream::{IntelUncore, intel_uncore_write},
    intel_workarounds_types_upstream::I915RegT,
    linux::i915::GRAPHICS_VER,
    linux_i915_private::DrmI915Private,
    linux_print::{CFormatArg, DrmLogLevel, drm_log_at, format_message},
};

// i915_pvinfo.h
const VGT_PVINFO_PAGE: u32 = 0x78000;
const VGT_MAGIC: u64 = 0x4776_5447_7654_4776;
const VGT_VERSION_MAJOR: u16 = 1;
const VGT_CAPS_FULL_PPGTT: u32 = 1 << 2;
const VGT_CAPS_HWSP_EMULATION: u32 = 1 << 3;
const VGT_CAPS_HUGE_GTT: u32 = 1 << 4;
const VGT_DRV_DISPLAY_READY: u32 = 1;
// struct vgt_if field offsets (packed): magic@0, version_major@8,
// vgt_caps@16, avail_rs@0x40, display_ready@0x804 (after rsv3/rsv4).
const VGTIF_VERSION_MAJOR: u32 = 8;
const VGTIF_VGT_CAPS: u32 = 16;
const VGTIF_DISPLAY_READY: u32 = 0x804;

// Byte offsets of `struct i915_virtual_gpu vgpu` inside `struct drm_i915_private`
// (x86_64 oracle, pahole on i915_layout_probe.o): lock@0, active@24, caps@28.
const I915_VGPU_OFFSET: usize = 2208;
const VGPU_LOCK_OFFSET: usize = I915_VGPU_OFFSET;
const VGPU_ACTIVE_OFFSET: usize = I915_VGPU_OFFSET + 24;
const VGPU_CAPS_OFFSET: usize = I915_VGPU_OFFSET + 28;

unsafe extern "C" {
    fn mutex_init(lock: *mut c_void);
}

/// `drm_info()` for this file, as in i915_driver_upstream.rs.
macro_rules! drm_info {
    ($device:expr, $format:expr $(, $argument:expr)* $(,)?) => {{
        let _ = $device;
        let __args: &[&dyn CFormatArg] = &[$(&($argument) as &dyn CFormatArg),*];
        let __message = format_message($format, __args);
        drm_log_at(DrmLogLevel::Info, "i915 DRM info", file!(), line!(), &__message);
    }};
}

#[inline]
unsafe fn vgpu_active_ptr(i915: *mut DrmI915Private) -> *mut bool {
    unsafe { (i915 as *mut u8).add(VGPU_ACTIVE_OFFSET).cast() }
}

#[inline]
unsafe fn vgpu_caps_ptr(i915: *mut DrmI915Private) -> *mut u32 {
    unsafe { (i915 as *mut u8).add(VGPU_CAPS_OFFSET).cast() }
}

/// Read one 32-bit word of the PVINFO page through BAR0 (before MMIO is
/// mapped). `None` means the BAR could not be mapped.
unsafe fn pvinfo_read(i915: *mut DrmI915Private, offset: u32) -> Option<u32> {
    let mut value = 0u32;
    if unsafe { early_gmd_read(i915, VGT_PVINFO_PAGE + offset, &mut value) } {
        Some(value)
    } else {
        None
    }
}

// upstream: i915_vgpu.c intel_vgpu_detect()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_vgpu_detect(dev_priv: *mut DrmI915Private) {
    unsafe {
        if GRAPHICS_VER(dev_priv) < 6 {
            return;
        }

        let Some(magic_lo) = pvinfo_read(dev_priv, 0) else {
            drm_err!(
                &(*dev_priv).drm,
                "failed to map MMIO bar to check for VGT\n"
            );
            return;
        };
        let Some(magic_hi) = pvinfo_read(dev_priv, 4) else {
            drm_err!(
                &(*dev_priv).drm,
                "failed to map MMIO bar to check for VGT\n"
            );
            return;
        };
        let magic = u64::from(magic_lo) | (u64::from(magic_hi) << 32);
        if magic != VGT_MAGIC {
            return;
        }

        // version_major is the low half of the 32-bit word at offset 8.
        let Some(version_word) = pvinfo_read(dev_priv, VGTIF_VERSION_MAJOR) else {
            drm_err!(
                &(*dev_priv).drm,
                "failed to map MMIO bar to check for VGT\n"
            );
            return;
        };
        let version_major = version_word as u16;
        if version_major < VGT_VERSION_MAJOR {
            drm_info!(&(*dev_priv).drm, "VGT interface version mismatch!\n");
            return;
        }

        let Some(caps) = pvinfo_read(dev_priv, VGTIF_VGT_CAPS) else {
            drm_err!(
                &(*dev_priv).drm,
                "failed to map MMIO bar to check for VGT\n"
            );
            return;
        };
        *vgpu_caps_ptr(dev_priv) = caps;
        *vgpu_active_ptr(dev_priv) = true;
        mutex_init((dev_priv as *mut u8).add(VGPU_LOCK_OFFSET).cast());

        drm_info!(
            &(*dev_priv).drm,
            "Virtual GPU for Intel GVT-g detected.\n"
        );
    }
}

// upstream: i915_vgpu.c intel_vgpu_register()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_vgpu_register(i915: *mut DrmI915Private) {
    unsafe {
        if intel_vgpu_active(i915) {
            let uncore = core::ptr::addr_of!((*i915).uncore).cast::<IntelUncore>().cast_mut();
            intel_uncore_write(
                uncore,
                I915RegT {
                    reg: VGT_PVINFO_PAGE + VGTIF_DISPLAY_READY,
                },
                VGT_DRV_DISPLAY_READY,
            );
        }
    }
}

// upstream: i915_vgpu.c intel_vgpu_active()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_vgpu_active(dev_priv: *mut DrmI915Private) -> bool {
    unsafe { *vgpu_active_ptr(dev_priv) }
}

// upstream: i915_vgpu.c intel_vgpu_has_full_ppgtt()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_vgpu_has_full_ppgtt(dev_priv: *mut DrmI915Private) -> bool {
    unsafe { *vgpu_caps_ptr(dev_priv) & VGT_CAPS_FULL_PPGTT != 0 }
}

// upstream: i915_vgpu.c intel_vgpu_has_hwsp_emulation()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_vgpu_has_hwsp_emulation(dev_priv: *mut DrmI915Private) -> bool {
    unsafe { *vgpu_caps_ptr(dev_priv) & VGT_CAPS_HWSP_EMULATION != 0 }
}

// upstream: i915_vgpu.c intel_vgpu_has_huge_gtt()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_vgpu_has_huge_gtt(dev_priv: *mut DrmI915Private) -> bool {
    unsafe { *vgpu_caps_ptr(dev_priv) & VGT_CAPS_HUGE_GTT != 0 }
}

// upstream: i915_vgpu.c intel_vgt_deballoon()
// Ballooning only runs for a GVT-g guest. TheKernel does not implement that
// guest role, so an active vGPU here is refused rather than ballooned.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_vgt_deballoon(ggtt: *mut I915Ggtt) {
    if !unsafe { intel_vgpu_active_from_ggtt(ggtt) } {
        return;
    }
    panic!("intel_vgt_deballoon({ggtt:p}): vGPU ballooning (GVT-g guest) is not supported by TheKernel");
}

// upstream: i915_vgpu.c intel_vgt_balloon()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_vgt_balloon(ggtt: *mut I915Ggtt) -> c_int {
    if !unsafe { intel_vgpu_active_from_ggtt(ggtt) } {
        return 0;
    }
    panic!("intel_vgt_balloon({ggtt:p}): vGPU ballooning (GVT-g guest) is not supported by TheKernel");
}

// `ggtt->vm.i915`, as read by intel_vgt_balloon()/intel_vgt_deballoon().
unsafe fn intel_vgpu_active_from_ggtt(ggtt: *mut I915Ggtt) -> bool {
    unsafe { intel_vgpu_active((*ggtt).vm.i915) }
}
