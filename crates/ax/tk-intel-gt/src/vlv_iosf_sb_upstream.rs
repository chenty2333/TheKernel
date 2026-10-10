// SPDX-License-Identifier: MIT
// Copyright © 2013-2021 Intel Corporation
// Linux v7.2.3 drivers/gpu/drm/i915/vlv_iosf_sb.c declares the VLV/CHV
// sideband-mailbox entry points. The file exists only for Valleyview and
// Cherryview (IOSF sideband PUNIT access), which TheKernel does not support,
// and it is larger than the translation budget for unsupported hardware. The
// definitions below therefore fail closed: every caller on these paths is a
// VLV/CHV-only code path that has no way to continue without the sideband.

#![allow(unsafe_code, non_snake_case, dead_code)]

use core::{ffi::c_ulong, ffi::c_void};

// upstream: vlv_iosf_sb.c vlv_iosf_sb_get()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vlv_iosf_sb_get(_drm: *mut c_void, _unit_mask: c_ulong) {
    panic!("vlv_iosf_sb_get: VLV/CHV 不受 TheKernel 支持");
}

// upstream: vlv_iosf_sb.c vlv_iosf_sb_put()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vlv_iosf_sb_put(_drm: *mut c_void, _unit_mask: c_ulong) {
    panic!("vlv_iosf_sb_put: VLV/CHV 不受 TheKernel 支持");
}

// upstream: vlv_iosf_sb.c vlv_iosf_sb_read()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vlv_iosf_sb_read(_drm: *mut c_void, _unit: u32, _addr: u32) -> u32 {
    panic!("vlv_iosf_sb_read: VLV/CHV 不受 TheKernel 支持");
}

// upstream: vlv_iosf_sb.c vlv_iosf_sb_write()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vlv_iosf_sb_write(_drm: *mut c_void, _unit: u32, _addr: u32, _val: u32) -> i32 {
    panic!("vlv_iosf_sb_write: VLV/CHV 不受 TheKernel 支持");
}

// upstream: vlv_iosf_sb.c vlv_iosf_sb_init()
// The mutex and the PM QoS request are created only for VLV/CHV. On other
// platforms the upstream function does nothing, which is the faithful path.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vlv_iosf_sb_init(i915: *mut crate::linux_i915_private::DrmI915Private) {
    if unsafe { crate::linux::i915::IS_VALLEYVIEW(i915) || crate::linux::i915::IS_CHERRYVIEW(i915) } {
        panic!("vlv_iosf_sb_init({i915:p}): VLV/CHV 不受 TheKernel 支持");
    }
}

// upstream: vlv_iosf_sb.c vlv_iosf_sb_fini()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vlv_iosf_sb_fini(i915: *mut crate::linux_i915_private::DrmI915Private) {
    if unsafe { crate::linux::i915::IS_VALLEYVIEW(i915) || crate::linux::i915::IS_CHERRYVIEW(i915) } {
        panic!("vlv_iosf_sb_fini({i915:p}): VLV/CHV 不受 TheKernel 支持");
    }
}
