// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Explicit Linux runtime-PM binding points. The GT owner must install real
//! callbacks before opting into source-order i915 paths; an absent backend
//! refuses rather than pretending that forcewake/runtime-PM references exist.

use core::{
    ffi::{c_ulong, c_void},
    sync::atomic::{AtomicI32, AtomicPtr, Ordering, compiler_fence},
};

use crate::{
    intel_context_upstream::IntelWakerefHandle,
    intel_engine_cs_upstream::{IntelEngineCs, IntelGt, IntelWakeref, IntelWakerefOps, WorkStruct},
    linux_i915_private::DrmI915Private,
    linux_memory::{atomic_inc, atomic_read, kref_get_unless_zero},
};

#[repr(C)]
pub struct RuntimePmOps {
    pub runtime_get: unsafe fn(*mut c_void) -> IntelWakerefHandle,
    pub runtime_put: unsafe fn(*mut c_void, IntelWakerefHandle),
}

static RUNTIME_PM_OPS: AtomicPtr<RuntimePmOps> = AtomicPtr::new(core::ptr::null_mut());

pub fn install_runtime_pm_ops(ops: &'static RuntimePmOps) -> Result<(), &'static str> {
    RUNTIME_PM_OPS
        .compare_exchange(
            core::ptr::null_mut(),
            ops as *const RuntimePmOps as *mut RuntimePmOps,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .map(|_| ())
        .map_err(|_| "i915 runtime PM callbacks are already installed")
}

#[inline]
fn ops() -> &'static RuntimePmOps {
    let ptr = RUNTIME_PM_OPS.load(Ordering::Acquire);
    assert!(
        !ptr.is_null(),
        "upstream-gt invoked without a real runtime-PM backend"
    );
    unsafe { &*ptr }
}

pub trait RuntimePmPointer {
    fn runtime_pm_pointer(self) -> *mut c_void;
}
impl<T> RuntimePmPointer for *mut T {
    fn runtime_pm_pointer(self) -> *mut c_void {
        self.cast()
    }
}
impl<T> RuntimePmPointer for *const T {
    fn runtime_pm_pointer(self) -> *mut c_void {
        self.cast_mut().cast()
    }
}
impl<T> RuntimePmPointer for &mut T {
    fn runtime_pm_pointer(self) -> *mut c_void {
        (self as *mut T).cast()
    }
}
impl<T> RuntimePmPointer for &T {
    fn runtime_pm_pointer(self) -> *mut c_void {
        (self as *const T).cast_mut().cast()
    }
}

pub fn intel_runtime_pm_get<P: RuntimePmPointer>(rpm: P) -> IntelWakerefHandle {
    let rpm = rpm.runtime_pm_pointer();
    assert!(!rpm.is_null());
    unsafe { (ops().runtime_get)(rpm) }
}

pub fn intel_runtime_pm_put<P: RuntimePmPointer>(rpm: P, wakeref: IntelWakerefHandle) {
    let rpm = rpm.runtime_pm_pointer();
    assert!(!rpm.is_null() && !wakeref.is_null());
    unsafe { (ops().runtime_put)(rpm, wakeref) };
}

pub fn intel_gt_pm_get(gt: *mut IntelGt) -> IntelWakerefHandle {
    assert!(!gt.is_null());
    unsafe { intel_wakeref_get(core::ptr::addr_of_mut!((*gt).wakeref)) };
    crate::linux_config::ERR_PTR::<c_void>(-crate::linux_config::ENOENT)
        .cast::<crate::intel_context_upstream::RefTracker>()
}

pub fn intel_gt_pm_put(gt: *mut IntelGt, wakeref: IntelWakerefHandle) {
    assert!(!gt.is_null() && !wakeref.is_null());
    unsafe { intel_wakeref_put(core::ptr::addr_of_mut!((*gt).wakeref)) };
}

#[inline]
unsafe fn wakeref_count(wf: *mut IntelWakeref) -> &'static AtomicI32 {
    assert!(!wf.is_null());
    unsafe { &*core::ptr::addr_of_mut!((*wf).count.counter).cast::<AtomicI32>() }
}

#[inline]
unsafe fn wakeref_runtime_pm(wf: *mut IntelWakeref) -> *mut c_void {
    let i915 = unsafe { (*wf).i915 };
    assert!(!i915.is_null());
    unsafe { core::ptr::addr_of_mut!((*i915).runtime_pm).cast::<c_void>() }
}

#[inline]
unsafe fn wakeref_ops(wf: *mut IntelWakeref) -> &'static IntelWakerefOps {
    let ops = unsafe { (*wf).ops };
    assert!(!ops.is_null());
    unsafe { &*ops }
}

/// `__intel_wakeref_init()` from intel_wakeref.c. Call only during exclusive
/// object initialization before publishing the owning GT/engine.
pub unsafe fn intel_wakeref_init(
    wf: *mut IntelWakeref,
    i915: *mut DrmI915Private,
    ops: *const IntelWakerefOps,
) {
    assert!(!wf.is_null() && !i915.is_null() && !ops.is_null());
    unsafe {
        (*wf).i915 = i915;
        (*wf).ops = ops;
        (*wf).wakeref = core::ptr::null_mut();
        (*wf).count.counter = 0;
        crate::linux_mutex::mutex_init(&mut (*wf).mutex);
        crate::linux_workqueue::INIT_DELAYED_WORK(&mut (*wf).work, |work| {
            let wf = container_of!(work, IntelWakeref, work.work);
            wakeref_put_worker(wf);
        });
    }
}

/// `intel_wakeref_get()` from intel_wakeref.h / intel_wakeref.c.
pub unsafe fn intel_wakeref_get(wf: *mut IntelWakeref) -> i32 {
    assert!(!wf.is_null());
    let count = unsafe { wakeref_count(wf) };
    if count
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
            (n != 0).then(|| n.wrapping_add(1))
        })
        .is_ok()
    {
        return 0;
    }

    let rpm = unsafe { wakeref_runtime_pm(wf) };
    let mut runtime_ref: *mut c_void = intel_runtime_pm_get(rpm).cast();
    assert!(
        !runtime_ref.is_null(),
        "runtime PM backend returned no wakeref"
    );
    unsafe { crate::linux_mutex::mutex_lock(&mut (*wf).mutex) };
    let mut result = 0;
    if unsafe { wakeref_count(wf).load(Ordering::Relaxed) } == 0 {
        assert!(unsafe { (*wf).wakeref.is_null() }, "INTEL_WAKEREF_BUG_ON");
        unsafe { (*wf).wakeref = runtime_ref };
        runtime_ref = core::ptr::null_mut();
        let get = unsafe { wakeref_ops(wf).get }.expect("intel_wakeref_ops.get must be installed");
        result = unsafe { get(wf) };
        if result != 0 {
            runtime_ref = unsafe { core::ptr::replace(&mut (*wf).wakeref, core::ptr::null_mut()) };
        }
    }
    if result == 0 {
        compiler_fence(Ordering::Release);
        unsafe { atomic_inc(&mut (*wf).count) };
    }
    unsafe { crate::linux_mutex::mutex_unlock(&mut (*wf).mutex) };
    if !runtime_ref.is_null() {
        intel_runtime_pm_put(
            rpm,
            runtime_ref.cast::<crate::intel_context_upstream::RefTracker>(),
        );
    }
    result
}

unsafe fn wakeref_put_last(wf: *mut IntelWakeref) {
    let count = unsafe { wakeref_count(wf) };
    if count
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
            (n != 1).then(|| n.wrapping_sub(1))
        })
        .is_ok()
    {
        return;
    }

    unsafe { crate::linux_mutex::mutex_lock(&mut (*wf).mutex) };
    let previous = unsafe { wakeref_count(wf) }.fetch_sub(1, Ordering::Relaxed);
    let mut runtime_ref = core::ptr::null_mut();
    if previous == 1 {
        let put = unsafe { wakeref_ops(wf).put }.expect("intel_wakeref_ops.put must be installed");
        if unsafe { put(wf) } == 0 {
            runtime_ref = unsafe { core::ptr::replace(&mut (*wf).wakeref, core::ptr::null_mut()) };
            assert!(!runtime_ref.is_null(), "INTEL_WAKEREF_BUG_ON");
        }
    }
    unsafe { crate::linux_mutex::mutex_unlock(&mut (*wf).mutex) };
    if !runtime_ref.is_null() {
        let rpm = unsafe { wakeref_runtime_pm(wf) };
        intel_runtime_pm_put(
            rpm,
            runtime_ref.cast::<crate::intel_context_upstream::RefTracker>(),
        );
    }
}

fn wakeref_put_worker(wf: *mut IntelWakeref) {
    assert!(!wf.is_null());
    unsafe { wakeref_put_last(wf) };
}

/// `intel_wakeref_put()` from intel_wakeref.h.
pub unsafe fn intel_wakeref_put(wf: *mut IntelWakeref) {
    assert!(!wf.is_null());
    unsafe { wakeref_put_last(wf) };
}

/// `intel_wakeref_put_async()` from intel_wakeref.h.
pub unsafe fn intel_wakeref_put_async(wf: *mut IntelWakeref) {
    unsafe { intel_wakeref_put_delay(wf, 0) };
}

/// `intel_wakeref_put_delay()` from intel_wakeref.h; delay is in jiffies.
pub unsafe fn intel_wakeref_put_delay(wf: *mut IntelWakeref, delay: u64) {
    assert!(!wf.is_null());
    let count = unsafe { wakeref_count(wf) };
    if count
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
            (n != 1).then(|| n.wrapping_sub(1))
        })
        .is_ok()
    {
        return;
    }
    let i915 = unsafe { (*wf).i915 };
    assert!(!i915.is_null() && !unsafe { (*i915).unordered_wq }.is_null());
    let queued = unsafe {
        crate::linux_workqueue::mod_delayed_work((*i915).unordered_wq, &mut (*wf).work, delay)
    };
    assert!(queued, "wakeref async release work was not queued");
}

/// `intel_wakeref_get_if_active()` from intel_wakeref.h.
pub unsafe fn intel_wakeref_get_if_active(wf: *mut IntelWakeref) -> bool {
    assert!(!wf.is_null());
    let count = unsafe { wakeref_count(wf) };
    let mut old = count.load(Ordering::Relaxed);
    loop {
        if old == 0 {
            return false;
        }
        match count.compare_exchange_weak(
            old,
            old.wrapping_add(1),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return true,
            Err(current) => old = current,
        }
    }
}

/// `intel_wakeref_wait_for_idle()` from intel_wakeref.c. The target wait
/// backend lacks `wait_var_event`; poll the same count/token predicate while
/// yielding, then take/release the mutex to serialize against final put.
pub unsafe fn intel_wakeref_wait_for_idle(wf: *mut IntelWakeref) -> i32 {
    assert!(!wf.is_null());
    while unsafe { wakeref_count(wf).load(Ordering::Acquire) } != 0 {
        axtask::yield_now();
    }
    unsafe { crate::linux_mutex::mutex_lock(&mut (*wf).mutex) };
    unsafe { crate::linux_mutex::mutex_unlock(&mut (*wf).mutex) };
    0
}

/// CONFIG_LOCKDEP is disabled in the target wt-dev configuration; Linux's
/// `might_lock()` annotation therefore compiles to no runtime operation.
pub unsafe fn intel_engine_pm_might_get(engine: *mut IntelEngineCs) {
    let _ = engine;
}

/// `intel_engine_pm_get_if_awake()` from gt/intel_engine_pm.h.
pub unsafe fn intel_engine_pm_get_if_awake(engine: *mut IntelEngineCs) -> bool {
    assert!(!engine.is_null());
    unsafe { intel_wakeref_get_if_active(core::ptr::addr_of_mut!((*engine).wakeref)) }
}

/// `intel_engine_pm_put_async()` from gt/intel_engine_pm.h.
pub unsafe fn intel_engine_pm_put_async(engine: *mut IntelEngineCs) {
    assert!(!engine.is_null());
    unsafe { intel_wakeref_put_async(core::ptr::addr_of_mut!((*engine).wakeref)) };
}

/// `intel_engine_pm_put_delay()` from gt/intel_engine_pm.h.
pub unsafe fn intel_engine_pm_put_delay(engine: *mut IntelEngineCs, delay: u64) {
    assert!(!engine.is_null());
    unsafe { intel_wakeref_put_delay(core::ptr::addr_of_mut!((*engine).wakeref), delay) };
}

/// `intel_engine_pm_might_put()` is lockdep-only in the target config.
pub unsafe fn intel_engine_pm_might_put(engine: *mut IntelEngineCs) {
    let _ = engine;
}

/// `intel_wakeref_is_active()` from intel_wakeref.h.
pub unsafe fn intel_wakeref_is_active(wf: *const IntelWakeref) -> bool {
    assert!(!wf.is_null());
    unsafe { (*wf).count.counter > 0 }
}

/// `intel_engine_pm_get()` from gt/intel_engine_pm.h.
pub unsafe fn intel_engine_pm_get(engine: *mut IntelEngineCs) {
    assert!(!engine.is_null());
    let _ = unsafe { intel_wakeref_get(core::ptr::addr_of_mut!((*engine).wakeref)) };
}

/// `intel_engine_pm_put()` from gt/intel_engine_pm.h.
pub unsafe fn intel_engine_pm_put(engine: *mut IntelEngineCs) {
    assert!(!engine.is_null());
    unsafe { intel_wakeref_put(core::ptr::addr_of_mut!((*engine).wakeref)) };
}

/// `intel_engine_pm_is_awake()` from gt/intel_engine_pm.h.
pub unsafe fn intel_engine_pm_is_awake(engine: *const IntelEngineCs) -> bool {
    assert!(!engine.is_null());
    unsafe { intel_wakeref_is_active(core::ptr::addr_of!((*engine).wakeref)) }
}

/// `intel_gt_pm_put_async_untracked()` from gt/intel_gt_pm.h.
pub unsafe fn intel_gt_pm_put_async_untracked(gt: *mut IntelGt) {
    assert!(!gt.is_null());
    unsafe { intel_wakeref_put_async(core::ptr::addr_of_mut!((*gt).wakeref)) };
}

/// `intel_gt_pm_put_async()` from gt/intel_gt_pm.h is the tracked form; the
/// wt-dev configuration disables wakeref tracking, so it shares the same
/// asynchronous reference decrement as the untracked helper.
pub unsafe fn intel_gt_pm_put_async(gt: *mut IntelGt, wakeref: IntelWakerefHandle) {
    assert!(!gt.is_null() && !wakeref.is_null());
    unsafe { intel_wakeref_put_async(core::ptr::addr_of_mut!((*gt).wakeref)) };
}

/// `intel_gt_pm_get_untracked()` from gt/intel_gt_pm.h.
pub unsafe fn intel_gt_pm_get_untracked(gt: *mut IntelGt) {
    assert!(!gt.is_null());
    let _ = unsafe { intel_wakeref_get(core::ptr::addr_of_mut!((*gt).wakeref)) };
}

/// `__intel_gt_pm_get()` from gt/intel_gt_pm.h.
pub unsafe fn __intel_gt_pm_get(gt: *mut IntelGt) {
    assert!(!gt.is_null());
    let wf = unsafe { core::ptr::addr_of_mut!((*gt).wakeref) };
    assert!(unsafe { wakeref_count(wf).load(Ordering::Relaxed) } > 0);
    unsafe { atomic_inc(&mut (*wf).count) };
}

/// `intel_gt_pm_put_untracked()` from gt/intel_gt_pm.h.
pub unsafe fn intel_gt_pm_put_untracked(gt: *mut IntelGt) {
    assert!(!gt.is_null());
    unsafe { intel_wakeref_put(core::ptr::addr_of_mut!((*gt).wakeref)) };
}

/// `intel_gt_pm_get_if_awake()` from gt/intel_gt_pm.h.
pub unsafe fn intel_gt_pm_get_if_awake(gt: *mut IntelGt) -> IntelWakerefHandle {
    assert!(!gt.is_null());
    if unsafe { intel_wakeref_get_if_active(core::ptr::addr_of_mut!((*gt).wakeref)) } {
        crate::linux_config::ERR_PTR::<c_void>(-crate::linux_config::ENOENT)
            .cast::<crate::intel_context_upstream::RefTracker>()
    } else {
        core::ptr::null_mut()
    }
}

/// `intel_gt_pm_is_awake()` from gt/intel_gt_pm.h.
pub unsafe fn intel_gt_pm_is_awake(gt: *const IntelGt) -> bool {
    assert!(!gt.is_null());
    unsafe { (*gt).wakeref.count.counter > 0 }
}

/// `intel_gt_pm_might_get/put()` are lockdep-only in the target config.
pub unsafe fn intel_gt_pm_might_get(gt: *mut IntelGt) {
    let _ = gt;
}

pub unsafe fn intel_gt_pm_might_put(gt: *mut IntelGt) {
    let _ = gt;
}

pub(crate) fn runtime_wakeref_is_acquired(value: IntelWakerefHandle) -> bool {
    !value.is_null()
}

macro_rules! with_intel_runtime_pm {
    ($rpm:expr, $wakeref:ident, $body:block) => {{
        let __rpm = $rpm;
        let mut $wakeref = $crate::linux_pm::intel_runtime_pm_get(__rpm);
        if $crate::linux_pm::runtime_wakeref_is_acquired($wakeref) {
            $body
            $crate::linux_pm::intel_runtime_pm_put(__rpm, $wakeref);
            $wakeref = core::ptr::null_mut();
        }
    }};
}

macro_rules! with_intel_gt_pm {
    ($gt:expr, $wakeref:ident, $body:block) => {{
        let __gt = $gt;
        let mut $wakeref = $crate::linux_pm::intel_gt_pm_get(__gt);
        if $crate::linux_pm::runtime_wakeref_is_acquired($wakeref) {
            $body
            $crate::linux_pm::intel_gt_pm_put(__gt, $wakeref);
            $wakeref = core::ptr::null_mut();
        }
    }};
}
