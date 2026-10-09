// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
// Source-order ABI and inline-helper transcription from Linux v7.2.3
// drivers/gpu/drm/i915/intel_wakeref.h.

#![allow(non_snake_case)]

use core::{
    ffi::{c_char, c_ulong},
    sync::atomic::{AtomicI32, Ordering},
};

use crate::{
    intel_context_types_upstream::IntelWakerefT,
    intel_context_upstream::{RefTracker, RefcountT},
    intel_engine_cs_upstream::{AtomicT, DelayedWork, Mutex, Spinlock, TimerList},
    linux_i915_private::DrmI915Private,
    linux_print::DrmPrinter,
};

#[repr(C)]
pub struct IntelWakerefOps {
    pub get: Option<unsafe extern "C" fn(*mut IntelWakeref) -> i32>,
    pub put: Option<unsafe extern "C" fn(*mut IntelWakeref) -> i32>,
}

/// `struct intel_wakeref` for the non-debug-wakeref configuration. The
/// optional `debug: ref_tracker_dir` tail is omitted. The tracked
/// `linux/config.rs` does not expose `CONFIG_DRM_I915_DEBUG_WAKEREF`; the
/// assertions below therefore document the no-debug variant and this option
/// must be checked against the exact kernel auto.conf before enabling it.
#[repr(C)]
pub struct IntelWakeref {
    pub count: AtomicT,
    pub mutex: Mutex,
    pub wakeref: IntelWakerefT,
    pub i915: *mut DrmI915Private,
    pub ops: *const IntelWakerefOps,
    pub work: DelayedWork,
}

#[repr(C)]
pub struct LockClassKey {
    _empty: [u8; 0],
}

#[repr(C)]
pub struct IntelWakerefLockclass {
    pub mutex: LockClassKey,
    pub work: LockClassKey,
}

#[repr(C)]
pub struct IntelWakerefAuto {
    pub i915: *mut DrmI915Private,
    pub timer: TimerList,
    pub wakeref: IntelWakerefT,
    pub lock: Spinlock,
    pub count: RefcountT,
}

pub const INTEL_REFTRACK_DEAD_COUNT: u32 = 16;
pub const INTEL_REFTRACK_PRINT_LIMIT: u32 = 16;

pub const INTEL_WAKEREF_PUT_ASYNC: c_ulong = 1 << 0;
pub const INTEL_WAKEREF_PUT_DELAY_MASK: c_ulong = c_ulong::MAX & !1;
// Linux 7.2.3 GFP_NOWAIT = __GFP_KSWAPD_RECLAIM | __GFP_NOWARN.
const GFP_NOWAIT: u32 = (1 << 11) | (1 << 13);

/// `struct ref_tracker_dir` is opaque here; it is used only by pointer in the
/// genuine Linux ref-tracker API bindings below.
#[repr(C)]
pub struct RefTrackerDir {
    _opaque: [u8; 0],
}

// Out-of-line functions declared by intel_wakeref.h. These are bindings to
// real kernel implementations, not local fallback stubs.
unsafe extern "C" {
    pub fn __intel_wakeref_init(
        wf: *mut IntelWakeref,
        i915: *mut DrmI915Private,
        ops: *const IntelWakerefOps,
        key: *mut IntelWakerefLockclass,
        name: *const c_char,
    );
    pub fn __intel_wakeref_get_first(wf: *mut IntelWakeref) -> i32;
    pub fn __intel_wakeref_put_last(wf: *mut IntelWakeref, flags: c_ulong);
    pub fn intel_wakeref_wait_for_idle(wf: *mut IntelWakeref) -> i32;
    pub fn intel_ref_tracker_show(dir: *mut RefTrackerDir, printer: *mut DrmPrinter);
    pub fn intel_wakeref_auto(wf: *mut IntelWakerefAuto, timeout: c_ulong);
    pub fn intel_wakeref_auto_init(wf: *mut IntelWakerefAuto, i915: *mut DrmI915Private);
    pub fn intel_wakeref_auto_fini(wf: *mut IntelWakerefAuto);

    // `ref_tracker_alloc/free` are actual Linux APIs when CONFIG_REF_TRACKER
    // is enabled (the configuration which uses the debug-wakeref paths).
    fn ref_tracker_alloc(dir: *mut RefTrackerDir, trackerp: *mut *mut RefTracker, gfp: u32) -> i32;
    fn ref_tracker_free(dir: *mut RefTrackerDir, trackerp: *mut *mut RefTracker) -> i32;
}

/// `intel_wakeref_init()` macro equivalent when the caller supplies its
/// call-site static lock-class key explicitly.
///
/// # Safety
/// `wf`, `i915`, `ops`, `key`, and `name` must satisfy the source C API
/// lifetime and initialization requirements; `key` must be static storage.
pub unsafe fn intel_wakeref_init_with_key(
    wf: *mut IntelWakeref,
    i915: *mut DrmI915Private,
    ops: *const IntelWakerefOps,
    key: *mut IntelWakerefLockclass,
    name: *const c_char,
) {
    unsafe { __intel_wakeref_init(wf, i915, ops, key, name) };
}

/// `intel_wakeref_get()`.
///
/// # Safety
/// `wf` must point to a live, initialized wakeref.
pub unsafe fn intel_wakeref_get(wf: *mut IntelWakeref) -> i32 {
    assert!(!wf.is_null());
    // might_sleep() is a source scheduling/debug annotation. This target
    // translation has no independent sleep-context tracker; the actual
    // sleeping get path is retained through its out-of-line C binding below.
    if !unsafe { crate::linux_memory::atomic_add_unless(&mut (*wf).count, 1, 0) } {
        unsafe { __intel_wakeref_get_first(wf) }
    } else {
        0
    }
}

/// `__intel_wakeref_get()`.
///
/// # Safety
/// `wf` must point to a live wakeref already held by the caller.
pub unsafe fn __intel_wakeref_get(wf: *mut IntelWakeref) {
    assert!(!wf.is_null());
    // In this target config CONFIG_DRM_I915_DEBUG is not enabled, so the C
    // INTEL_WAKEREF_BUG_ON expands to BUILD_BUG_ON_INVALID (no runtime test).
    unsafe { crate::linux_memory::atomic_inc(&mut (*wf).count) };
}

/// `intel_wakeref_get_if_active()`.
///
/// # Safety
/// `wf` must point to a live wakeref.
pub unsafe fn intel_wakeref_get_if_active(wf: *mut IntelWakeref) -> bool {
    assert!(!wf.is_null());
    unsafe { crate::linux_memory::atomic_add_unless(&mut (*wf).count, 1, 0) }
}

/// `intel_wakeref_might_get()`; lockdep is disabled in the source config, so
/// its annotation has no runtime effect in this translation.
pub unsafe fn intel_wakeref_might_get(wf: *mut IntelWakeref) {
    let _ = wf;
}

/// `__intel_wakeref_put()`.
///
/// # Safety
/// `wf` must point to a live wakeref held by the caller.
pub unsafe fn __intel_wakeref_put(wf: *mut IntelWakeref, flags: c_ulong) {
    assert!(!wf.is_null());
    // As above, the non-debug INTEL_WAKEREF_BUG_ON has no runtime check.
    if !unsafe { crate::linux_memory::atomic_add_unless(&mut (*wf).count, -1, 1) } {
        unsafe { __intel_wakeref_put_last(wf, flags) };
    }
}

/// `intel_wakeref_put()`.
///
/// # Safety
/// `wf` must point to a live wakeref held by the caller.
pub unsafe fn intel_wakeref_put(wf: *mut IntelWakeref) {
    unsafe { __intel_wakeref_put(wf, 0) };
}

/// `intel_wakeref_put_async()`.
///
/// # Safety
/// `wf` must point to a live wakeref held by the caller.
pub unsafe fn intel_wakeref_put_async(wf: *mut IntelWakeref) {
    unsafe { __intel_wakeref_put(wf, INTEL_WAKEREF_PUT_ASYNC) };
}

/// `intel_wakeref_put_delay()`.
///
/// # Safety
/// `wf` must point to a live wakeref held by the caller.
pub unsafe fn intel_wakeref_put_delay(wf: *mut IntelWakeref, delay: c_ulong) {
    let flags = INTEL_WAKEREF_PUT_ASYNC | ((delay << 1) & INTEL_WAKEREF_PUT_DELAY_MASK);
    unsafe { __intel_wakeref_put(wf, flags) };
}

/// `intel_wakeref_might_put()`; lockdep is disabled in the source config.
pub unsafe fn intel_wakeref_might_put(wf: *mut IntelWakeref) {
    let _ = wf;
}

/// `intel_wakeref_lock()`.
///
/// # Safety
/// `wf` must point to a live wakeref.
pub unsafe fn intel_wakeref_lock(wf: *mut IntelWakeref) {
    assert!(!wf.is_null());
    unsafe { crate::linux_mutex::mutex_lock(&mut (*wf).mutex) };
}

/// `intel_wakeref_unlock()`.
///
/// # Safety
/// `wf` must point to a live wakeref locked by the caller.
pub unsafe fn intel_wakeref_unlock(wf: *mut IntelWakeref) {
    assert!(!wf.is_null());
    unsafe { crate::linux_mutex::mutex_unlock(&mut (*wf).mutex) };
}

/// `intel_wakeref_unlock_wait()`.
///
/// # Safety
/// `wf` must point to a live wakeref.
pub unsafe fn intel_wakeref_unlock_wait(wf: *mut IntelWakeref) {
    assert!(!wf.is_null());
    unsafe {
        crate::linux_mutex::mutex_lock(&mut (*wf).mutex);
        crate::linux_mutex::mutex_unlock(&mut (*wf).mutex);
        crate::linux_workqueue::flush_delayed_work(&mut (*wf).work);
    }
}

/// `intel_wakeref_is_active()`.
///
/// # Safety
/// `wf` must point to a live wakeref.
pub unsafe fn intel_wakeref_is_active(wf: *const IntelWakeref) -> bool {
    assert!(!wf.is_null());
    !unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*wf).wakeref)) }.is_null()
}

/// `__intel_wakeref_defer_park()`.
///
/// # Safety
/// `wf` must point to a live wakeref locked by the caller.
pub unsafe fn __intel_wakeref_defer_park(wf: *mut IntelWakeref) {
    assert!(!wf.is_null());
    if crate::linux_config::CONFIG_LOCKDEP {
        crate::linux_assert::lockdep_assert_held(unsafe { &(*wf).mutex });
    }
    // As above, the non-debug INTEL_WAKEREF_BUG_ON has no runtime check.
    let counter = unsafe { &*core::ptr::addr_of!((*wf).count.counter).cast::<AtomicI32>() };
    counter.store(1, Ordering::Release);
}

/// `intel_ref_tracker_alloc()`.
///
/// # Safety
/// `dir` must point to a live tracker directory when reference tracking is
/// enabled; otherwise this follows the Linux API's configuration stub.
pub unsafe fn intel_ref_tracker_alloc(dir: *mut RefTrackerDir) -> IntelWakerefT {
    let mut user = core::ptr::null_mut();
    unsafe { ref_tracker_alloc(dir, &mut user, GFP_NOWAIT) };
    if user.is_null() {
        INTEL_WAKEREF_DEF
    } else {
        user
    }
}

/// `intel_ref_tracker_free()`.
///
/// # Safety
/// `dir` must point to a live tracker directory when tracking is enabled;
/// `wakeref` must be a value returned by `intel_ref_tracker_alloc`.
pub unsafe fn intel_ref_tracker_free(dir: *mut RefTrackerDir, mut wakeref: IntelWakerefT) {
    if wakeref == INTEL_WAKEREF_DEF {
        wakeref = core::ptr::null_mut();
    }
    if is_err(wakeref) {
        let _ = crate::linux_assert::warn_on(true);
        return;
    }
    unsafe { ref_tracker_free(dir, &mut wakeref) };
}

/// `intel_wakeref_track()` with CONFIG_DRM_I915_DEBUG_WAKEREF disabled.
///
/// # Safety
/// `wf` must point to a live wakeref.
pub unsafe fn intel_wakeref_track(wf: *mut IntelWakeref) -> IntelWakerefT {
    INTEL_WAKEREF_DEF
}

/// `intel_wakeref_untrack()` with CONFIG_DRM_I915_DEBUG_WAKEREF disabled.
///
/// # Safety
/// `wf` must point to a live wakeref.
pub unsafe fn intel_wakeref_untrack(wf: *mut IntelWakeref, handle: IntelWakerefT) {
    let _ = handle;
}

pub const INTEL_WAKEREF_DEF: IntelWakerefT = (-2isize) as *mut RefTracker;

#[inline]
fn is_err<T>(pointer: *mut T) -> bool {
    pointer as usize >= usize::MAX - 4094
}

// Exact x86_64 layouts for the non-LOCKDEP, non-debug-wakeref source config.
const _: [(); 16] = [(); core::mem::size_of::<IntelWakerefOps>()];
const _: [(); 0] = [(); core::mem::offset_of!(IntelWakerefOps, get)];
const _: [(); 8] = [(); core::mem::offset_of!(IntelWakerefOps, put)];
const _: [(); 144] = [(); core::mem::size_of::<IntelWakeref>()];
const _: [(); 0] = [(); core::mem::offset_of!(IntelWakeref, count)];
const _: [(); 8] = [(); core::mem::offset_of!(IntelWakeref, mutex)];
const _: [(); 32] = [(); core::mem::offset_of!(IntelWakeref, wakeref)];
const _: [(); 40] = [(); core::mem::offset_of!(IntelWakeref, i915)];
const _: [(); 48] = [(); core::mem::offset_of!(IntelWakeref, ops)];
const _: [(); 56] = [(); core::mem::offset_of!(IntelWakeref, work)];
const _: [(); 0] = [(); core::mem::size_of::<IntelWakerefLockclass>()];
const _: [(); 0] = [(); core::mem::offset_of!(IntelWakerefLockclass, mutex)];
const _: [(); 0] = [(); core::mem::offset_of!(IntelWakerefLockclass, work)];
const _: [(); 8] = [(); core::mem::align_of::<IntelWakerefAuto>()];
const _: [(); 64] = [(); core::mem::size_of::<IntelWakerefAuto>()];
const _: [(); 0] = [(); core::mem::offset_of!(IntelWakerefAuto, i915)];
const _: [(); 8] = [(); core::mem::offset_of!(IntelWakerefAuto, timer)];
const _: [(); 48] = [(); core::mem::offset_of!(IntelWakerefAuto, wakeref)];
const _: [(); 56] = [(); core::mem::offset_of!(IntelWakerefAuto, lock)];
const _: [(); 60] = [(); core::mem::offset_of!(IntelWakerefAuto, count)];
