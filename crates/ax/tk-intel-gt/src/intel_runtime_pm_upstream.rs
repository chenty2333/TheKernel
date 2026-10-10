// SPDX-License-Identifier: MIT
// Copyright © 2012-2014 Intel Corporation.
// Source-order translation from Linux v7.2.3 drivers/gpu/drm/i915/intel_runtime_pm.c.
// The source carries the MIT grant (no SPDX tag); its grant is retained here.
//
// This translation matches CONFIG_DRM_I915_DEBUG_RUNTIME_PM=n from the
// tracked target configuration. Debug-only alternatives and definitions are
// consequently not compiled, as in the source C preprocessor configuration.

#![allow(non_snake_case)]

use core::{
    ffi::c_void,
    mem::{align_of, offset_of, size_of},
};

use crate::{
    intel_context_types_upstream::IntelWakerefT,
    intel_context_upstream::RefTracker,
    intel_engine_cs_upstream::{AtomicT, ListHead, Spinlock},
    intel_uncore_types_upstream::IntelRuntimePm,
    intel_wakeref_types_upstream::{
        INTEL_WAKEREF_DEF, IntelWakerefAuto, intel_wakeref_auto_fini, intel_wakeref_auto_init,
    },
    linux::{
        gem::drm_device_device,
        i915::{IntelDeviceInfoOverlay, to_i915},
    },
    linux_i915_private::DrmI915Private,
    linux_memory::{atomic_add, atomic_dec, atomic_read, atomic_set},
};

/// Active `struct intel_runtime_pm` layout with CONFIG_DRM_I915_DEBUG_RUNTIME_PM=n.
/// This overlays the opaque runtime-PM prefix owned by `DrmI915Private`.
#[repr(C)]
pub struct IntelRuntimePmLayout {
    pub wakeref_count: AtomicT,
    pub kdev: *mut c_void,
    pub available: bool,
    pub no_wakeref_tracking: bool,
    pub lmem_userfault_lock: Spinlock,
    pub lmem_userfault_list: ListHead,
    pub userfault_wakeref: IntelWakerefAuto,
}

const _: [(); 104] = [(); size_of::<IntelRuntimePmLayout>()];
const _: [(); 8] = [(); align_of::<IntelRuntimePmLayout>()];

/// The target Linux PM-core entry points used by the C source.
unsafe extern "C" {
    fn pm_runtime_get_sync(dev: *mut c_void) -> i32;
    fn pm_runtime_get_if_active(dev: *mut c_void) -> i32;
    fn pm_runtime_get_if_in_use(dev: *mut c_void) -> i32;
    fn pm_runtime_get_noresume(dev: *mut c_void);
    fn pm_runtime_put_autosuspend(dev: *mut c_void) -> i32;
    fn pm_runtime_put(dev: *mut c_void) -> i32;
    fn pm_runtime_mark_last_busy(dev: *mut c_void);
    fn pm_runtime_set_autosuspend_delay(dev: *mut c_void, delay: i32);
    fn pm_runtime_dont_use_autosuspend(dev: *mut c_void);
    fn pm_runtime_use_autosuspend(dev: *mut c_void);
    fn pm_runtime_allow(dev: *mut c_void);
    fn pm_runtime_suspended(dev: *mut c_void) -> bool;
    fn dev_pm_set_driver_flags(dev: *mut c_void, flags: u32);
}

const INTEL_RPM_WAKELOCK_SHIFT: u32 = 16;
const INTEL_RPM_WAKELOCK_BIAS: i32 = 1 << INTEL_RPM_WAKELOCK_SHIFT;
const INTEL_RPM_RAW_WAKEREF_MASK: i32 = INTEL_RPM_WAKELOCK_BIAS - 1;
const DPM_FLAG_NO_DIRECT_COMPLETE: u32 = 1 << 0;

#[inline]
unsafe fn rpm_layout(rpm: *mut IntelRuntimePm) -> *mut IntelRuntimePmLayout {
    assert!(!rpm.is_null());
    rpm.cast()
}

// upstream: intel_runtime_pm.c rpm_to_i915()
#[inline]
unsafe fn rpm_to_i915(rpm: *mut IntelRuntimePm) -> *mut DrmI915Private {
    assert!(!rpm.is_null());
    unsafe {
        rpm.cast::<u8>()
            .sub(offset_of!(DrmI915Private, runtime_pm))
            .cast()
    }
}

// upstream: intel_runtime_pm.c init_intel_runtime_pm_wakeref()
unsafe fn init_intel_runtime_pm_wakeref(_rpm: *mut IntelRuntimePm) {}

// upstream: intel_runtime_pm.c track_intel_runtime_pm_wakeref()
unsafe fn track_intel_runtime_pm_wakeref(_rpm: *mut IntelRuntimePm) -> IntelWakerefT {
    INTEL_WAKEREF_DEF
}

// upstream: intel_runtime_pm.c untrack_intel_runtime_pm_wakeref()
unsafe fn untrack_intel_runtime_pm_wakeref(_rpm: *mut IntelRuntimePm, _wakeref: IntelWakerefT) {}

// upstream: intel_runtime_pm.c __intel_wakeref_dec_and_check_tracking()
unsafe fn __intel_wakeref_dec_and_check_tracking(rpm: *mut IntelRuntimePm) {
    unsafe { atomic_dec(&mut (*rpm_layout(rpm)).wakeref_count) };
}

// upstream: intel_runtime_pm.c untrack_all_intel_runtime_pm_wakerefs()
unsafe fn untrack_all_intel_runtime_pm_wakerefs(_rpm: *mut IntelRuntimePm) {}

// upstream: intel_runtime_pm.c intel_runtime_pm_acquire()
unsafe fn intel_runtime_pm_acquire(rpm: *mut IntelRuntimePm, wakelock: bool) {
    let count = unsafe { &mut (*rpm_layout(rpm)).wakeref_count };
    if wakelock {
        unsafe { atomic_add(1 + INTEL_RPM_WAKELOCK_BIAS, count) };
        unsafe { assert_rpm_wakelock_held(rpm) };
    } else {
        unsafe { atomic_add(1, count) };
        unsafe { assert_rpm_raw_wakeref_held(rpm) };
    }
}

// upstream: intel_runtime_pm.c intel_runtime_pm_release()
unsafe fn intel_runtime_pm_release(rpm: *mut IntelRuntimePm, wakelock: bool) {
    if wakelock {
        unsafe { assert_rpm_wakelock_held(rpm) };
        unsafe {
            atomic_add(
                -INTEL_RPM_WAKELOCK_BIAS,
                &mut (*rpm_layout(rpm)).wakeref_count,
            )
        };
    } else {
        unsafe { assert_rpm_raw_wakeref_held(rpm) };
    }
    unsafe { __intel_wakeref_dec_and_check_tracking(rpm) };
}

// upstream: intel_runtime_pm.c __intel_runtime_pm_get()
unsafe fn __intel_runtime_pm_get(rpm: *mut IntelRuntimePm, wakelock: bool) -> IntelWakerefT {
    let i915 = unsafe { rpm_to_i915(rpm) };
    let kdev = unsafe { (*rpm_layout(rpm)).kdev };
    let ret = unsafe { pm_runtime_get_sync(kdev) };
    if ret < 0 {
        crate::linux_assert::warn_on(true);
        let _ = i915;
    }
    unsafe { intel_runtime_pm_acquire(rpm, wakelock) };
    unsafe { track_intel_runtime_pm_wakeref(rpm) }
}

// upstream: intel_runtime_pm.c drm_to_rpm()
unsafe fn drm_to_rpm(drm: *const c_void) -> *mut IntelRuntimePm {
    let i915 = unsafe { to_i915(drm.cast_mut().cast()) };
    unsafe { core::ptr::addr_of_mut!((*i915).runtime_pm).cast() }
}

// upstream: intel_runtime_pm.c i915_display_rpm_get()
unsafe extern "C" fn i915_display_rpm_get(drm: *const c_void) -> *mut RefTracker {
    unsafe { intel_runtime_pm_get(drm_to_rpm(drm)) }
}

// upstream: intel_runtime_pm.c i915_display_rpm_get_raw()
unsafe extern "C" fn i915_display_rpm_get_raw(drm: *const c_void) -> *mut RefTracker {
    unsafe { intel_runtime_pm_get_raw(drm_to_rpm(drm)) }
}

// upstream: intel_runtime_pm.c i915_display_rpm_get_if_in_use()
unsafe extern "C" fn i915_display_rpm_get_if_in_use(drm: *const c_void) -> *mut RefTracker {
    unsafe { intel_runtime_pm_get_if_in_use(drm_to_rpm(drm)) }
}

// upstream: intel_runtime_pm.c i915_display_rpm_get_noresume()
unsafe extern "C" fn i915_display_rpm_get_noresume(drm: *const c_void) -> *mut RefTracker {
    unsafe { intel_runtime_pm_get_noresume(drm_to_rpm(drm)) }
}

// upstream: intel_runtime_pm.c i915_display_rpm_put()
unsafe extern "C" fn i915_display_rpm_put(drm: *const c_void, wakeref: *mut RefTracker) {
    // In this no-debug configuration the header's inline put delegates to
    // intel_runtime_pm_put_unchecked().
    let _ = wakeref;
    unsafe { intel_runtime_pm_put_unchecked(drm_to_rpm(drm)) };
}

// upstream: intel_runtime_pm.c i915_display_rpm_put_raw()
unsafe extern "C" fn i915_display_rpm_put_raw(drm: *const c_void, wakeref: *mut RefTracker) {
    unsafe { intel_runtime_pm_put_raw(drm_to_rpm(drm), wakeref) };
}

// upstream: intel_runtime_pm.c i915_display_rpm_put_unchecked()
unsafe extern "C" fn i915_display_rpm_put_unchecked(drm: *const c_void) {
    unsafe { intel_runtime_pm_put_unchecked(drm_to_rpm(drm)) };
}

// upstream: intel_runtime_pm.c i915_display_rpm_suspended()
unsafe extern "C" fn i915_display_rpm_suspended(drm: *const c_void) -> bool {
    unsafe { intel_runtime_pm_suspended(drm_to_rpm(drm)) }
}

// upstream: intel_runtime_pm.c i915_display_rpm_assert_held()
unsafe extern "C" fn i915_display_rpm_assert_held(drm: *const c_void) {
    unsafe { assert_rpm_wakelock_held(drm_to_rpm(drm)) };
}

// upstream: intel_runtime_pm.c i915_display_rpm_assert_block()
unsafe extern "C" fn i915_display_rpm_assert_block(drm: *const c_void) {
    unsafe { disable_rpm_wakeref_asserts(drm_to_rpm(drm)) };
}

// upstream: intel_runtime_pm.c i915_display_rpm_assert_unblock()
unsafe extern "C" fn i915_display_rpm_assert_unblock(drm: *const c_void) {
    unsafe { enable_rpm_wakeref_asserts(drm_to_rpm(drm)) };
}

#[repr(C)]
pub struct IntelDisplayRpmInterface {
    pub get: Option<unsafe extern "C" fn(*const c_void) -> *mut RefTracker>,
    pub get_raw: Option<unsafe extern "C" fn(*const c_void) -> *mut RefTracker>,
    pub get_if_in_use: Option<unsafe extern "C" fn(*const c_void) -> *mut RefTracker>,
    pub get_noresume: Option<unsafe extern "C" fn(*const c_void) -> *mut RefTracker>,
    pub put: Option<unsafe extern "C" fn(*const c_void, *mut RefTracker)>,
    pub put_raw: Option<unsafe extern "C" fn(*const c_void, *mut RefTracker)>,
    pub put_unchecked: Option<unsafe extern "C" fn(*const c_void)>,
    pub suspended: Option<unsafe extern "C" fn(*const c_void) -> bool>,
    pub assert_held: Option<unsafe extern "C" fn(*const c_void)>,
    pub assert_block: Option<unsafe extern "C" fn(*const c_void)>,
    pub assert_unblock: Option<unsafe extern "C" fn(*const c_void)>,
}

#[unsafe(no_mangle)]
pub static i915_display_rpm_interface: IntelDisplayRpmInterface = IntelDisplayRpmInterface {
    get: Some(i915_display_rpm_get),
    get_raw: Some(i915_display_rpm_get_raw),
    get_if_in_use: Some(i915_display_rpm_get_if_in_use),
    get_noresume: Some(i915_display_rpm_get_noresume),
    put: Some(i915_display_rpm_put),
    put_raw: Some(i915_display_rpm_put_raw),
    put_unchecked: Some(i915_display_rpm_put_unchecked),
    suspended: Some(i915_display_rpm_suspended),
    assert_held: Some(i915_display_rpm_assert_held),
    assert_block: Some(i915_display_rpm_assert_block),
    assert_unblock: Some(i915_display_rpm_assert_unblock),
};

// upstream: intel_runtime_pm.c intel_runtime_pm_get_raw()
pub unsafe fn intel_runtime_pm_get_raw(rpm: *mut IntelRuntimePm) -> IntelWakerefT {
    unsafe { __intel_runtime_pm_get(rpm, false) }
}

// upstream: intel_runtime_pm.c intel_runtime_pm_get()
pub unsafe fn intel_runtime_pm_get(rpm: *mut IntelRuntimePm) -> IntelWakerefT {
    unsafe { __intel_runtime_pm_get(rpm, true) }
}

// upstream: intel_runtime_pm.c __intel_runtime_pm_get_if_active()
unsafe fn __intel_runtime_pm_get_if_active(
    rpm: *mut IntelRuntimePm,
    ignore_usecount: bool,
) -> IntelWakerefT {
    if crate::linux_config::CONFIG_PM {
        let kdev = unsafe { (*rpm_layout(rpm)).kdev };
        let active = if ignore_usecount {
            unsafe { pm_runtime_get_if_active(kdev) }
        } else {
            unsafe { pm_runtime_get_if_in_use(kdev) }
        };
        if active <= 0 {
            return core::ptr::null_mut();
        }
    }
    unsafe { intel_runtime_pm_acquire(rpm, true) };
    unsafe { track_intel_runtime_pm_wakeref(rpm) }
}

// upstream: intel_runtime_pm.c intel_runtime_pm_get_if_in_use()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_runtime_pm_get_if_in_use(rpm: *mut IntelRuntimePm) -> IntelWakerefT {
    unsafe { __intel_runtime_pm_get_if_active(rpm, false) }
}

// upstream: intel_runtime_pm.c intel_runtime_pm_get_if_active()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_runtime_pm_get_if_active(rpm: *mut IntelRuntimePm) -> IntelWakerefT {
    unsafe { __intel_runtime_pm_get_if_active(rpm, true) }
}

// upstream: intel_runtime_pm.c intel_runtime_pm_get_noresume()
pub unsafe fn intel_runtime_pm_get_noresume(rpm: *mut IntelRuntimePm) -> IntelWakerefT {
    unsafe { assert_rpm_raw_wakeref_held(rpm) };
    unsafe { pm_runtime_get_noresume((*rpm_layout(rpm)).kdev) };
    unsafe { intel_runtime_pm_acquire(rpm, true) };
    unsafe { track_intel_runtime_pm_wakeref(rpm) }
}

// upstream: intel_runtime_pm.c __intel_runtime_pm_put()
unsafe fn __intel_runtime_pm_put(rpm: *mut IntelRuntimePm, wref: IntelWakerefT, wakelock: bool) {
    let kdev = unsafe { (*rpm_layout(rpm)).kdev };
    unsafe { untrack_intel_runtime_pm_wakeref(rpm, wref) };
    unsafe { intel_runtime_pm_release(rpm, wakelock) };
    unsafe { pm_runtime_mark_last_busy(kdev) };
    unsafe { pm_runtime_put_autosuspend(kdev) };
}

// upstream: intel_runtime_pm.c intel_runtime_pm_put_raw()
pub unsafe fn intel_runtime_pm_put_raw(rpm: *mut IntelRuntimePm, wref: IntelWakerefT) {
    unsafe { __intel_runtime_pm_put(rpm, wref, false) };
}

// upstream: intel_runtime_pm.c intel_runtime_pm_put_unchecked()
pub unsafe fn intel_runtime_pm_put_unchecked(rpm: *mut IntelRuntimePm) {
    unsafe { __intel_runtime_pm_put(rpm, INTEL_WAKEREF_DEF, true) };
}

// upstream: intel_runtime_pm.h intel_runtime_pm_put() (CONFIG_DRM_I915_DEBUG_RUNTIME_PM=n)
#[inline]
pub unsafe fn intel_runtime_pm_put(rpm: *mut IntelRuntimePm, _wref: IntelWakerefT) {
    unsafe { intel_runtime_pm_put_unchecked(rpm) };
}

// upstream: intel_runtime_pm.c intel_runtime_pm_enable()
pub unsafe fn intel_runtime_pm_enable(rpm: *mut IntelRuntimePm) {
    let i915 = unsafe { rpm_to_i915(rpm) };
    let kdev = unsafe { (*rpm_layout(rpm)).kdev };
    unsafe { dev_pm_set_driver_flags(kdev, DPM_FLAG_NO_DIRECT_COMPLETE) };
    unsafe { pm_runtime_set_autosuspend_delay(kdev, 10000) };
    unsafe { pm_runtime_mark_last_busy(kdev) };
    if !unsafe { (*rpm_layout(rpm)).available } {
        unsafe { pm_runtime_dont_use_autosuspend(kdev) };
        let ret = unsafe { pm_runtime_get_sync(kdev) };
        if ret < 0 {
            let _ = i915;
            crate::linux_assert::warn_on(true);
        }
    } else {
        unsafe { pm_runtime_use_autosuspend(kdev) };
    }
    if !unsafe { crate::linux::i915::IS_DGFX(i915) } {
        unsafe { pm_runtime_allow(kdev) };
    }
    unsafe { pm_runtime_put_autosuspend(kdev) };
}

// upstream: intel_runtime_pm.c intel_runtime_pm_disable()
pub unsafe fn intel_runtime_pm_disable(rpm: *mut IntelRuntimePm) {
    let i915 = unsafe { rpm_to_i915(rpm) };
    let kdev = unsafe { (*rpm_layout(rpm)).kdev };
    if unsafe { pm_runtime_get_sync(kdev) } < 0 {
        let _ = i915;
        crate::linux_assert::warn_on(true);
    }
    unsafe { pm_runtime_dont_use_autosuspend(kdev) };
    if !unsafe { (*rpm_layout(rpm)).available } {
        unsafe { pm_runtime_put(kdev) };
    }
}

// upstream: intel_runtime_pm.c intel_runtime_pm_driver_release()
pub unsafe fn intel_runtime_pm_driver_release(rpm: *mut IntelRuntimePm) {
    let i915 = unsafe { rpm_to_i915(rpm) };
    let count = unsafe { atomic_read(&(*rpm_layout(rpm)).wakeref_count) };
    unsafe { intel_wakeref_auto_fini(&mut (*rpm_layout(rpm)).userfault_wakeref) };
    if count != 0 {
        let _ = i915;
        crate::linux_assert::warn_on(true);
    }
}

// upstream: intel_runtime_pm.c intel_runtime_pm_driver_last_release()
pub unsafe fn intel_runtime_pm_driver_last_release(rpm: *mut IntelRuntimePm) {
    unsafe { intel_runtime_pm_driver_release(rpm) };
    unsafe { untrack_all_intel_runtime_pm_wakerefs(rpm) };
}

// upstream: intel_runtime_pm.c intel_runtime_pm_init_early()
pub unsafe fn intel_runtime_pm_init_early(rpm: *mut IntelRuntimePm) {
    let i915 = unsafe { rpm_to_i915(rpm) };
    let kdev = unsafe { drm_device_device(core::ptr::addr_of_mut!((*i915).drm).cast()) };
    let layout = unsafe { rpm_layout(rpm) };
    unsafe {
        (*layout).kdev = kdev;
        let info = (*i915).info.cast::<IntelDeviceInfoOverlay>();
        assert!(!info.is_null());
        (*layout).available = (*info).flags[3] & (1 << 7) != 0;
        atomic_set(&mut (*layout).wakeref_count, 0);
        init_intel_runtime_pm_wakeref(rpm);
        (*layout).lmem_userfault_list.next = core::ptr::addr_of_mut!((*layout).lmem_userfault_list);
        (*layout).lmem_userfault_list.prev = core::ptr::addr_of_mut!((*layout).lmem_userfault_list);
        crate::linux::locks::spin_lock_init(&mut (*layout).lmem_userfault_lock);
        intel_wakeref_auto_init(&mut (*layout).userfault_wakeref, i915);
    }
}

#[inline]
unsafe fn intel_runtime_pm_suspended(rpm: *mut IntelRuntimePm) -> bool {
    unsafe { pm_runtime_suspended((*rpm_layout(rpm)).kdev) }
}

#[inline]
pub(crate) unsafe fn assert_rpm_device_not_suspended(rpm: *mut IntelRuntimePm) {
    crate::linux_assert::warn_on(unsafe { intel_runtime_pm_suspended(rpm) });
}

#[inline]
pub(crate) unsafe fn assert_rpm_raw_wakeref_held(rpm: *mut IntelRuntimePm) {
    unsafe { assert_rpm_device_not_suspended(rpm) };
    let count = unsafe { atomic_read(&(*rpm_layout(rpm)).wakeref_count) };
    crate::linux_assert::warn_on(count & INTEL_RPM_RAW_WAKEREF_MASK == 0);
}

#[inline]
pub(crate) unsafe fn assert_rpm_wakelock_held(rpm: *mut IntelRuntimePm) {
    unsafe { assert_rpm_raw_wakeref_held(rpm) };
    let count = unsafe { atomic_read(&(*rpm_layout(rpm)).wakeref_count) };
    crate::linux_assert::warn_on(count >> INTEL_RPM_WAKELOCK_SHIFT == 0);
}

#[inline]
pub(crate) unsafe fn disable_rpm_wakeref_asserts(rpm: *mut IntelRuntimePm) {
    unsafe {
        atomic_add(
            INTEL_RPM_WAKELOCK_BIAS + 1,
            &mut (*rpm_layout(rpm)).wakeref_count,
        )
    };
}

#[inline]
pub(crate) unsafe fn enable_rpm_wakeref_asserts(rpm: *mut IntelRuntimePm) {
    unsafe {
        atomic_add(
            -(INTEL_RPM_WAKELOCK_BIAS + 1),
            &mut (*rpm_layout(rpm)).wakeref_count,
        )
    };
}
