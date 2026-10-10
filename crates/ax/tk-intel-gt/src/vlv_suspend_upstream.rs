// SPDX-License-Identifier: MIT
// Copyright © 2014 Intel Corporation
// Source: Linux v7.2.3 drivers/gpu/drm/i915/vlv_suspend.c (MIT, 482 lines).
//
// The file implements the Valleyview/Cherryview S0ix suspend sequence, and its
// entry points are called unconditionally from the probe and PM paths. Each
// entry point keeps the upstream early return for platforms that are not
// VLV/CHV. The VLV/CHV branch is not translated (the file is larger than the
// unsupported-hardware budget), so it fails closed with a panic.
#![allow(non_snake_case, unsafe_op_in_unsafe_fn)]

use core::ffi::{c_int, c_void};

use crate::{
    linux::i915::{IS_CHERRYVIEW, IS_VALLEYVIEW},
    linux_i915_private::DrmI915Private,
};

unsafe extern "C" {
    fn kfree(ptr: *mut c_void);
}

// upstream: vlv_suspend.c vlv_suspend_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vlv_suspend_init(i915: *mut DrmI915Private) -> c_int {
    if !unsafe { IS_VALLEYVIEW(i915) } {
        return 0;
    }
    panic!("vlv_suspend_init({i915:p}): VLV S0ix state is not supported by TheKernel");
}

// upstream: vlv_suspend.c vlv_suspend_cleanup()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vlv_suspend_cleanup(i915: *mut DrmI915Private) {
    unsafe {
        let state = core::ptr::addr_of_mut!((*i915).vlv_s0ix_state);
        if (*state).is_null() {
            return;
        }
        kfree(*state);
        *state = core::ptr::null_mut();
    }
}

// upstream: vlv_suspend.c vlv_suspend_complete()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vlv_suspend_complete(dev_priv: *mut DrmI915Private) -> c_int {
    if !unsafe { IS_VALLEYVIEW(dev_priv) || IS_CHERRYVIEW(dev_priv) } {
        return 0;
    }
    panic!("vlv_suspend_complete({dev_priv:p}): VLV/CHV S0ix suspend is not supported by TheKernel");
}

// upstream: vlv_suspend.c vlv_resume_prepare()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vlv_resume_prepare(dev_priv: *mut DrmI915Private, rpm_resume: bool) -> c_int {
    let _ = rpm_resume;
    if !unsafe { IS_VALLEYVIEW(dev_priv) || IS_CHERRYVIEW(dev_priv) } {
        return 0;
    }
    panic!("vlv_resume_prepare({dev_priv:p}): VLV/CHV S0ix resume is not supported by TheKernel");
}
