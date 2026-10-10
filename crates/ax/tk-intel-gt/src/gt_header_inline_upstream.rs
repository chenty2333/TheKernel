// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//
// C-ABI definitions for i915 header `static inline` helpers that the
// translated upstream code calls through `extern "C"`.  Bodies follow the
// header text; the Linux runtime-PM / request / signal primitives they wrap
// are the existing LinuxKPI-side Rust implementations.

#![allow(unsafe_op_in_unsafe_fn)]

use crate::{
    intel_engine_cs_upstream::IntelEngineCs,
    intel_guc_ct_types_upstream::IntelGucCt,
    intel_guc_types_upstream::IntelGuc,
    i915_request_types_upstream::I915Request,
    linux::{i915::intel_wa_list_free as wa_list_free_impl, pm, requests, signal, wait},
    linux_i915_private::{I915GpuError, i915_reset_count as reset_count_impl},
};

// upstream: i915_gpu_error.h i915_reset_count()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_reset_count(error: *const I915GpuError) -> u32 {
    reset_count_impl(error)
}

// upstream: gt/intel_engine_pm.h intel_engine_pm_get()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_engine_pm_get(engine: *mut IntelEngineCs) {
    pm::intel_engine_pm_get(engine)
}

// upstream: gt/intel_engine_pm.h intel_engine_pm_put()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_engine_pm_put(engine: *mut IntelEngineCs) {
    pm::intel_engine_pm_put(engine)
}

// upstream: gt/intel_engine_pm.h intel_engine_pm_put_delay()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_engine_pm_put_delay(engine: *mut IntelEngineCs, delay: u64) {
    pm::intel_engine_pm_put_delay(engine, delay)
}

// upstream: gt/intel_engine_pm.h intel_engine_pm_is_awake()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_engine_pm_is_awake(engine: *const IntelEngineCs) -> bool {
    pm::intel_engine_pm_is_awake(engine)
}

// upstream: i915_request.h i915_request_get()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_request_get(rq: *mut I915Request) -> *mut I915Request {
    requests::i915_request_get(rq)
}

// upstream: i915_request.h i915_request_get_rcu()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_request_get_rcu(rq: *mut I915Request) -> *mut I915Request {
    requests::i915_request_get_rcu(rq)
}

// upstream: i915_request.h i915_request_put()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_request_put(rq: *mut I915Request) {
    requests::i915_request_put(rq)
}

// upstream: gt/intel_workarounds.h intel_wa_list_free()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_wa_list_free(wal: *mut crate::intel_workarounds_types_upstream::I915WaList) {
    wa_list_free_impl(wal)
}

// upstream: gt/uc/intel_guc_ct.h intel_guc_ct_sanitize()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_ct_sanitize(ct: *mut IntelGucCt) {
    (*ct).enabled = false;
}

// upstream: gt/uc/intel_guc.h intel_guc_send_busy_loop()
// `might_sleep_if()` is a CONFIG_DEBUG_ATOMIC_SLEEP check only and is omitted.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_send_busy_loop(
    guc: *mut IntelGuc,
    action: *const u32,
    len: u32,
    g2h_len_dw: u32,
    loop_on_busy: bool,
) -> i32 {
    let not_atomic = !crate::linux::irq::in_atomic() && !crate::linux::locks::irqs_disabled();
    let mut sleep_period_ms: u32 = 1;
    loop {
        let words = core::slice::from_raw_parts(action, len as usize);
        let err = crate::guc_submission_upstream::intel_guc_send_nb(&mut *guc, words, g2h_len_dw);
        if err == -EBUSY && loop_on_busy {
            if not_atomic {
                if msleep_interruptible(sleep_period_ms) {
                    return -EINTR;
                }
                sleep_period_ms <<= 1;
            } else {
                core::hint::spin_loop();
            }
            continue;
        }
        return err;
    }
}

const EBUSY: i32 = 16;
const EINTR: i32 = 4;

// Sleeps for `ms` milliseconds; returns true when a signal is pending on the
// current task, which the caller maps to -EINTR as `msleep_interruptible()`
// does.  Signal wakeup mid-sleep is not modelled by the LinuxKPI wait layer.
unsafe fn msleep_interruptible(ms: u32) -> bool {
    if signal::signal_pending_current() {
        return true;
    }
    wait::msleep(ms);
    signal::signal_pending_current()
}
