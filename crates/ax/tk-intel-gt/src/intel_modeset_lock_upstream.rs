// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// Translation of drivers/gpu/drm/i915/display/intel_modeset_lock.c.
#![allow(non_snake_case, dead_code, unsafe_code)]

use tk_drm_upstream::drm_modeset_lock_types::DrmModesetAcquireCtx;

use crate::intel_display_types_upstream::IntelAtomicState;

unsafe extern "C" {
    fn drm_modeset_acquire_init(ctx: *mut DrmModesetAcquireCtx, flags: u32);
    fn drm_modeset_acquire_fini(ctx: *mut DrmModesetAcquireCtx);
    fn drm_modeset_drop_locks(ctx: *mut DrmModesetAcquireCtx);
    fn drm_modeset_backoff(ctx: *mut DrmModesetAcquireCtx) -> i32;
    fn drm_atomic_commit_clear(state: *mut tk_drm_upstream::drm_atomic_types::DrmAtomicCommit);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn _intel_modeset_lock_begin(
    ctx: *mut DrmModesetAcquireCtx,
    state: *mut IntelAtomicState,
    flags: u32,
    ret: *mut i32,
) {
    if ctx.is_null() || ret.is_null() {
        return;
    }
    unsafe { drm_modeset_acquire_init(ctx, flags) };
    if !state.is_null() {
        unsafe { (*state).base.acquire_ctx = ctx };
    }
    unsafe { *ret = -tk_linux_kapi::config::EDEADLK };
}

#[inline]
fn lock_loop(ret: &mut i32) -> bool {
    if *ret == -tk_linux_kapi::config::EDEADLK {
        *ret = 0;
        true
    } else {
        false
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn _intel_modeset_lock_loop(ret: *mut i32) -> bool {
    if ret.is_null() {
        return false;
    }
    unsafe { lock_loop(&mut *ret) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn _intel_modeset_lock_end(
    ctx: *mut DrmModesetAcquireCtx,
    state: *mut IntelAtomicState,
    ret: *mut i32,
) {
    if ctx.is_null() || ret.is_null() {
        return;
    }
    if unsafe { *ret == -tk_linux_kapi::config::EDEADLK } {
        if !state.is_null() {
            unsafe { drm_atomic_commit_clear(&mut (*state).base) };
        }
        let backoff = unsafe { drm_modeset_backoff(ctx) };
        if backoff == 0 {
            unsafe { *ret = -tk_linux_kapi::config::EDEADLK };
            return;
        }
        unsafe { *ret = backoff };
    }
    unsafe {
        drm_modeset_drop_locks(ctx);
        drm_modeset_acquire_fini(ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::lock_loop;

    #[test]
    fn modeset_retry_only_restarts_after_deadlock() {
        let mut ret = -tk_linux_kapi::config::EDEADLK;
        assert!(lock_loop(&mut ret));
        assert_eq!(ret, 0);
        assert!(!lock_loop(&mut ret));
        assert_eq!(ret, 0);
        ret = -tk_linux_kapi::config::EINVAL;
        assert!(!lock_loop(&mut ret));
        assert_eq!(ret, -tk_linux_kapi::config::EINVAL);
    }
}
