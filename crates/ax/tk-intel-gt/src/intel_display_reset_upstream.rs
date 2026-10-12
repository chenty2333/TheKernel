// SPDX-License-Identifier: MIT
// Copyright © 2023 Intel Corporation
// Source: drivers/gpu/drm/i915/display/intel_display_reset.c and intel_display_reset.h

#![allow(non_snake_case, dead_code, unsafe_code)]

use core::ffi::c_void;
use crate::intel_display_core_types_upstream::IntelDisplay;
use crate::intel_display_device_upstream::intel_display_device_present;
use tk_drm_upstream::drm_atomic_types::DrmAtomicCommit;
use tk_drm_upstream::drm_modeset_lock_types::DrmModesetAcquireCtx;

#[inline]
fn is_err<T>(ptr: *const T) -> bool {
    let addr = ptr as usize;
    addr >= (!4095usize)
}

// upstream: intel_display_reset.c intel_display_reset_supported()
// Note: Symbol collision with `crate::linux::platform::intel_display_reset_supported`.
// Kept live in platform.rs to preserve the runtime boundary. This function provides the functional upstream translation.
pub unsafe extern "C" fn intel_display_reset_supported(display: *mut IntelDisplay) -> bool {
    !display.is_null() && unsafe { intel_display_device_present(display) }
}

// upstream: intel_display_reset.c intel_display_reset_test()
// Note: Symbol collision with `crate::linux::platform::intel_display_reset_test`.
pub unsafe extern "C" fn intel_display_reset_test(display: *mut IntelDisplay) -> bool {
    if display.is_null() || !unsafe { intel_display_device_present(display) } {
        false
    } else {
        unsafe { (*display).params.force_reset_modeset_test }
    }
}

// upstream: intel_display_reset.c intel_display_reset_prepare()
// Note: Symbol collision with `crate::linux::platform::intel_display_reset_prepare`.
pub unsafe extern "C" fn intel_display_reset_prepare(display: *mut IntelDisplay) {
    if display.is_null() || unsafe { (*display).drm.is_null() } {
        return;
    }
    let ctx = unsafe { core::ptr::addr_of_mut!((*display).restore.reset_ctx) };
    let mut ret: i32;

    unsafe {
        tk_linux_kapi::mutex::mutex_lock(core::ptr::addr_of_mut!((*(*display).drm).mode_config.mutex));
        tk_drm_upstream::drm_modeset_lock_upstream::drm_modeset_acquire_init(ctx, 0);

        loop {
            ret = tk_drm_upstream::drm_modeset_lock_upstream::drm_modeset_lock_all_ctx((*display).drm, ctx);
            if ret != -tk_linux_kapi::config::EDEADLK {
                break;
            }
            tk_drm_upstream::drm_modeset_lock_upstream::drm_modeset_backoff(ctx);
        }

        let state = crate::display_pending_stubs::kapi_display::drm_atomic_helper_duplicate_state((*display).drm, ctx);
        if is_err(state) {
            return;
        }

        ret = crate::display_pending_stubs::kapi_display::drm_atomic_helper_disable_all((*display).drm, ctx);
        if ret != 0 {
            crate::display_pending_stubs::kapi_display::drm_atomic_commit_put(state);
            return;
        }

        (*display).reset.count += 1;
        (*display).restore.modeset_state = state;
    }
}

// upstream: intel_display_reset.c intel_display_reset_finish()
// Note: Symbol collision with `crate::linux::platform::intel_display_reset_finish`.
pub unsafe extern "C" fn intel_display_reset_finish(display: *mut IntelDisplay, test_only: bool) {
    if display.is_null() || unsafe { (*display).drm.is_null() } {
        return;
    }
    let ctx = unsafe { core::ptr::addr_of_mut!((*display).restore.reset_ctx) };
    let state = unsafe { (*display).restore.modeset_state };
    unsafe { (*display).restore.modeset_state = core::ptr::null_mut() };

    if state.is_null() {
        // Nothing to restore
    } else if test_only {
        unsafe {
            let _ = crate::display_pending_stubs::kapi_display::drm_atomic_helper_commit_duplicated_state(state, ctx);
            crate::display_pending_stubs::kapi_display::drm_atomic_commit_put(state);
        }
    } else {
        unsafe {
            crate::display_pending_stubs::b6::intel_pps_unlock_regs_wa(display);
            crate::intel_display_driver_upstream::intel_display_driver_init_hw(display);
            crate::intel_clock_gating_upstream::intel_clock_gating_init((*display).drm.cast());
            crate::display_pending_stubs::opt::intel_cx0_pll_power_save_wa(display);
            crate::display_pending_stubs::b6::intel_hpd_init(display);

            let _ = crate::intel_display_driver_upstream::__intel_display_driver_resume(display, state, ctx);

            crate::display_pending_stubs::b6::intel_hpd_poll_disable(display);
            crate::display_pending_stubs::kapi_display::drm_atomic_commit_put(state);
        }
    }

    unsafe {
        tk_drm_upstream::drm_modeset_lock_upstream::drm_modeset_drop_locks(ctx);
        tk_drm_upstream::drm_modeset_lock_upstream::drm_modeset_acquire_fini(ctx);
        tk_linux_kapi::mutex::mutex_unlock(core::ptr::addr_of_mut!((*(*display).drm).mode_config.mutex));
    }
}

// upstream: intel_display_reset.c intel_display_reset_debugfs_register()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_reset_debugfs_register(_display: *mut IntelDisplay) {
    // skipped: debugfs disabled in config
}
