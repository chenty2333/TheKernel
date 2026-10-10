// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
// Source-order Rust translation of Linux v7.2.3
// drivers/gpu/drm/i915/gt/intel_engine_heartbeat.c.

#![allow(unsafe_code, unsafe_op_in_unsafe_fn)]

use core::{
    ffi::{c_char, c_long, c_ulong},
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{
    i915_active_upstream::i915_request_add_active_barriers,
    i915_request_types_upstream::{I915_FENCE_FLAG_SENTINEL, I915Request},
    i915_request_upstream::{
        __i915_request_commit, __i915_request_create, __i915_request_queue_bh,
    },
    i915_scheduler_types_upstream::I915SchedAttr,
    intel_context_api_upstream::{
        intel_context_enter, intel_context_exit, mutex_lock_interruptible,
    },
    intel_context_types_upstream::IntelContext,
    intel_engine_api_upstream::{
        intel_clamp_heartbeat_interval_ms, intel_engine_flush_submission,
        intel_engine_has_heartbeat, intel_engine_has_preempt_reset, intel_engine_uses_guc,
    },
    intel_engine_cs_upstream::{IntelEngineCs, WorkStruct, intel_engine_dump},
    intel_engine_types_upstream::{IntelEngineId, IntelEngineMask, intel_engine_has_preemption},
    intel_gt_api_upstream::intel_gt_is_wedged,
    intel_gt_types_upstream::IntelGt,
    linux::{
        bits::set_bit,
        irq::{local_bh_disable, local_bh_enable},
        list::llist_empty,
        pm::{
            intel_engine_pm_get, intel_engine_pm_get_if_awake, intel_engine_pm_is_awake,
            intel_engine_pm_put,
        },
        primitives::{fetch_and_zero, jiffies, msecs_to_jiffies, time_after},
        print::{DRM_UT_DRIVER, drm_dbg_printer},
        requests::{i915_request_completed, i915_request_get, i915_request_put},
        sw_fence::i915_sw_fence_signaled,
        workqueue::{INIT_DELAYED_WORK, cancel_delayed_work, mod_delayed_work, system_highpri_wq},
    },
    linux_config::{
        __GFP_NOWARN, CONFIG_DRM_I915_DEBUG_GEM, CONFIG_DRM_I915_HEARTBEAT_INTERVAL, EINTR, ENODEV,
        GFP_KERNEL, GFP_NOWAIT, HZ,
    },
};

const I915_PRIORITY_MIN: i32 = -1024;
const I915_PRIORITY_HEARTBEAT: i32 = 1025;
const I915_PRIORITY_BARRIER: i32 = i32::MAX - 1;

// These are upstream out-of-line Linux/i915 C entry points whose owning
// source modules are not linked into this Rust crate. Keep their real C ABI
// declarations rather than replacing them with compatibility behavior.
unsafe extern "C" {
    fn round_jiffies_up_relative(j: c_ulong) -> c_ulong;
    fn intel_guc_find_hung_context(engine: *mut IntelEngineCs);
}

// upstream: intel_engine_heartbeat.c next_heartbeat()
fn next_heartbeat(engine: *mut IntelEngineCs) -> bool {
    let mut delay = unsafe { READ_ONCE!((*engine).props.heartbeat_interval_ms) as c_long };
    let rq = unsafe { (*engine).heartbeat.systole };

    // FIXME: The final period extension is disabled if the period has been
    // modified from the default. This is to prevent issues with certain
    // selftests which override the value and expect specific behaviour.
    // Once the selftests have been updated to either cope with variable
    // heartbeat periods (or to override the pre-emption timeout as well,
    // or just to add a selftest specific override of the extension), the
    // generic override can be removed.
    if !rq.is_null()
        && unsafe { (*rq).sched.attr.priority >= I915_PRIORITY_BARRIER }
        && delay as c_ulong == unsafe { (*engine).defaults.heartbeat_interval_ms }
    {
        // The final try is at the highest priority possible. Up until now
        // a pre-emption might not even have been attempted. So make sure
        // this last attempt allows enough time for a pre-emption to occur.
        let preempt_timeout = unsafe { READ_ONCE!((*engine).props.preempt_timeout_ms) };
        let mut longer = preempt_timeout.wrapping_mul(2) as c_long;
        longer = unsafe { intel_clamp_heartbeat_interval_ms(engine, longer as u64) } as c_long;
        if longer > delay {
            delay = longer;
        }
    }

    if delay == 0 {
        return false;
    }

    // `msecs_to_jiffies_timeout()` is the source inline helper from
    // i915_jiffies.h: min_t(unsigned long, MAX_JIFFY_OFFSET, j + 1).
    let max_jiffy_offset = ((c_long::MAX as u64) >> 1) - 1;
    let jiffies = msecs_to_jiffies(delay as u32);
    let mut ticks = jiffies.saturating_add(1).min(max_jiffy_offset) as c_ulong;
    if ticks >= HZ as c_ulong {
        ticks = unsafe { round_jiffies_up_relative(ticks) };
    }
    unsafe {
        mod_delayed_work(
            system_highpri_wq,
            core::ptr::addr_of_mut!((*engine).heartbeat.work),
            ticks.saturating_add(1) as u64,
        );
    }

    true
}

// upstream: intel_engine_heartbeat.c heartbeat_create()
fn heartbeat_create(ce: *mut IntelContext, gfp: u32) -> *mut I915Request {
    unsafe {
        intel_context_enter(ce);
        let rq = __i915_request_create(ce, gfp);
        intel_context_exit(ce);
        rq
    }
}

// upstream: intel_engine_heartbeat.c idle_pulse()
fn idle_pulse(engine: *mut IntelEngineCs, rq: *mut I915Request) {
    unsafe {
        (*engine).wakeref_serial = READ_ONCE!((*engine).serial).wrapping_add(1);
        i915_request_add_active_barriers(rq);
        if (*engine).heartbeat.systole.is_null() && intel_engine_has_heartbeat(engine) {
            (*engine).heartbeat.systole = i915_request_get(rq);
        }
    }
}

// upstream: intel_engine_heartbeat.c heartbeat_commit()
fn heartbeat_commit(rq: *mut I915Request, attr: *const I915SchedAttr) {
    let engine = unsafe { (*rq).engine };
    idle_pulse(engine, rq);

    unsafe {
        __i915_request_commit(rq);
        // __i915_request_queue() is file-local to i915_request.c. Translate
        // its exact call sequence here: optional scheduler update followed by
        // the LinuxKPI-owned queue bottom half under local_bh exclusion.
        if !attr.is_null() {
            let sched = (*engine).sched_engine;
            if let Some(schedule) = (*sched).schedule {
                schedule(rq, attr);
            }
        }
        local_bh_disable();
        __i915_request_queue_bh(rq);
        local_bh_enable();
    }
}

// upstream: intel_engine_heartbeat.c show_heartbeat()
fn show_heartbeat(rq: *const I915Request, engine: *mut IntelEngineCs) {
    let mut printer = unsafe {
        drm_dbg_printer(
            &mut (*(*engine).i915).drm,
            DRM_UT_DRIVER,
            b"heartbeat\0".as_ptr().cast(),
        )
    };

    if rq.is_null() {
        drm_printf!(&mut printer, "%s heartbeat not ticking\n", unsafe {
            (*engine).name.as_ptr()
        },);
        unsafe {
            intel_engine_dump(
                engine,
                &mut printer,
                core::ptr::null(),
            );
        }
    } else {
        drm_printf!(
            &mut printer,
            "%s heartbeat {seqno:%llx:%lld, prio:%d} not ticking\n",
            unsafe { (*engine).name.as_ptr() },
            unsafe { (*rq).fence.context },
            unsafe { (*rq).fence.seqno as i64 },
            unsafe { (*rq).sched.attr.priority },
        );
        unsafe {
            intel_engine_dump(
                engine,
                &mut printer,
                core::ptr::null(),
            );
        }
    }
}

// upstream: intel_engine_heartbeat.c reset_engine()
fn reset_engine(engine: *mut IntelEngineCs, rq: *mut I915Request) {
    if CONFIG_DRM_I915_DEBUG_GEM {
        show_heartbeat(rq, engine);
    }

    if unsafe { intel_engine_uses_guc(engine) } {
        // GuC itself is toast or GuC's hang detection
        // is disabled. Either way, need to find the
        // hang culprit manually.
        unsafe { intel_guc_find_hung_context(engine) };
    }

    unsafe {
        crate::intel_reset_upstream::intel_gt_handle_error_format(
            (*engine).gt,
            (*engine).mask,
            crate::linux::registers::I915_ERROR_CAPTURE as c_ulong,
            b"stopped heartbeat on %s\0".as_ptr().cast(),
            &[&(*engine).name.as_ptr() as &dyn crate::linux::print::CFormatArg],
        );
    }
}

// upstream: intel_engine_heartbeat.c heartbeat()
fn heartbeat(wrk: *mut WorkStruct) {
    let mut attr = I915SchedAttr {
        priority: I915_PRIORITY_MIN,
    };
    let engine = container_of!(wrk, IntelEngineCs, heartbeat.work.work);
    let ce = unsafe { (*engine).kernel_context };
    let mut rq: *mut I915Request;
    let mut serial: c_ulong;

    // Just in case everything has gone horribly wrong, give it a kick.
    unsafe { intel_engine_flush_submission(engine) };

    rq = unsafe { fetch_and_zero(&mut (*engine).heartbeat.systole) };
    if !rq.is_null() {
        if i915_request_completed(rq) {
            i915_request_put(rq);
        } else {
            unsafe { (*engine).heartbeat.systole = rq };
        }
    }

    if !unsafe { intel_engine_pm_get_if_awake(engine) } {
        return;
    }

    'out: {
        if unsafe { intel_gt_is_wedged((*engine).gt) } {
            break 'out;
        }

        let sched_engine = unsafe { (*engine).sched_engine };
        let disabled = unsafe { (*sched_engine).disabled.unwrap_unchecked() };
        if unsafe { disabled(sched_engine) } {
            reset_engine(engine, unsafe { (*engine).heartbeat.systole });
            break 'out;
        }

        if unsafe { !(*engine).heartbeat.systole.is_null() } {
            let delay = unsafe { READ_ONCE!((*engine).props.heartbeat_interval_ms) };

            // Safeguard against too-fast worker invocations.
            if !time_after(
                jiffies() as c_ulong,
                unsafe { (*rq).emitted_jiffies }
                    .wrapping_add(msecs_to_jiffies(delay as u32) as c_ulong),
            ) {
                break 'out;
            }

            if !unsafe { i915_sw_fence_signaled(&(*rq).submit) } {
                // Not yet submitted, system is stalled.
                //
                // This more often happens for ring submission,
                // where all contexts are funnelled into a common
                // ringbuffer. If one context is blocked on an
                // external fence, not only is it not submitted,
                // but all other contexts, including the kernel
                // context are stuck waiting for the signal.
            } else if unsafe {
                (*sched_engine).schedule.is_some()
                    && (*rq).sched.attr.priority < I915_PRIORITY_BARRIER
            } {
                // Gradually raise the priority of the heartbeat to
                // give high priority work [which presumably desires
                // low latency and no jitter] the chance to naturally
                // complete before being preempted.
                attr.priority = crate::linux::registers::I915_PRIORITY_NORMAL;
                if unsafe { (*rq).sched.attr.priority >= attr.priority } {
                    attr.priority = I915_PRIORITY_HEARTBEAT;
                }
                if unsafe { (*rq).sched.attr.priority >= attr.priority } {
                    attr.priority = I915_PRIORITY_BARRIER;
                }

                local_bh_disable();
                unsafe { (*sched_engine).schedule.unwrap_unchecked()(rq, &attr) };
                local_bh_enable();
            } else {
                reset_engine(engine, rq);
            }

            unsafe { (*rq).emitted_jiffies = jiffies() as c_ulong };
            break 'out;
        }

        serial = unsafe { READ_ONCE!((*engine).serial) };
        if unsafe { (*engine).wakeref_serial == serial } {
            break 'out;
        }

        if !unsafe { crate::linux::mutex::mutex_trylock(&mut (*(*ce).timeline).mutex) } {
            // Unable to lock the kernel timeline, is the engine stuck?
            let blocked = unsafe {
                AtomicU64::from_ptr(core::ptr::addr_of_mut!((*engine).heartbeat.blocked))
                    .swap(serial as u64, Ordering::SeqCst)
            };
            if blocked == serial as u64 {
                unsafe {
                    crate::intel_reset_upstream::intel_gt_handle_error_format(
                        (*engine).gt,
                        (*engine).mask,
                        crate::linux::registers::I915_ERROR_CAPTURE as c_ulong,
                        b"no heartbeat on %s\0".as_ptr().cast(),
                        &[&(*engine).name.as_ptr() as &dyn crate::linux::print::CFormatArg],
                    );
                }
            }
            break 'out;
        }

        rq = heartbeat_create(ce, GFP_NOWAIT | __GFP_NOWARN);
        if IS_ERR!(rq) {
            unsafe { crate::linux::mutex::mutex_unlock(&mut (*(*ce).timeline).mutex) };
            break 'out;
        }

        heartbeat_commit(rq, &attr);
        unsafe { crate::linux::mutex::mutex_unlock(&mut (*(*ce).timeline).mutex) };
    }

    if !unsafe { (*(*engine).i915).params.enable_hangcheck } || !next_heartbeat(engine) {
        rq = unsafe { fetch_and_zero(&mut (*engine).heartbeat.systole) };
        if !rq.is_null() {
            i915_request_put(rq);
        }
    }
    unsafe { intel_engine_pm_put(engine) };
}

// upstream: intel_engine_heartbeat.c intel_engine_unpark_heartbeat()
pub fn intel_engine_unpark_heartbeat(engine: *mut IntelEngineCs) {
    if CONFIG_DRM_I915_HEARTBEAT_INTERVAL == 0 {
        return;
    }

    let _ = next_heartbeat(engine);
}

// upstream: intel_engine_heartbeat.c intel_engine_park_heartbeat()
pub fn intel_engine_park_heartbeat(engine: *mut IntelEngineCs) {
    if unsafe { cancel_delayed_work(core::ptr::addr_of_mut!((*engine).heartbeat.work)) } {
        let rq = unsafe { fetch_and_zero(&mut (*engine).heartbeat.systole) };
        if !rq.is_null() {
            i915_request_put(rq);
        }
    }
}

// upstream: intel_engine_heartbeat.c intel_gt_unpark_heartbeats()
pub fn intel_gt_unpark_heartbeats(gt: *mut IntelGt) {
    let mut engine: *mut IntelEngineCs = core::ptr::null_mut();
    let mut id: IntelEngineId = 0;

    for_each_engine!(engine, id, gt, {
        if unsafe { intel_engine_pm_is_awake(engine) } {
            intel_engine_unpark_heartbeat(engine);
        }
    });
}

// upstream: intel_engine_heartbeat.c intel_gt_park_heartbeats()
pub fn intel_gt_park_heartbeats(gt: *mut IntelGt) {
    let mut engine: *mut IntelEngineCs = core::ptr::null_mut();
    let mut id: IntelEngineId = 0;

    for_each_engine!(engine, id, gt, {
        intel_engine_park_heartbeat(engine);
    });
}

// upstream: intel_engine_heartbeat.c intel_engine_init_heartbeat()
pub fn intel_engine_init_heartbeat(engine: *mut IntelEngineCs) {
    unsafe {
        INIT_DELAYED_WORK(&mut (*engine).heartbeat.work, |work| {
            heartbeat(work as *mut WorkStruct)
        });
    }
}

// upstream: intel_engine_heartbeat.c __intel_engine_pulse()
fn __intel_engine_pulse(engine: *mut IntelEngineCs) -> i32 {
    let attr = I915SchedAttr {
        priority: I915_PRIORITY_BARRIER,
    };
    let ce = unsafe { (*engine).kernel_context };
    let rq: *mut I915Request;

    lockdep_assert_held!(unsafe { &(*(*ce).timeline).mutex });
    GEM_BUG_ON!(!unsafe { intel_engine_has_preemption(engine) });
    GEM_BUG_ON!(!unsafe { intel_engine_pm_is_awake(engine) });

    rq = heartbeat_create(ce, GFP_NOWAIT | __GFP_NOWARN);
    if IS_ERR!(rq) {
        return PTR_ERR!(rq);
    }

    unsafe { set_bit(I915_FENCE_FLAG_SENTINEL, &mut (*rq).fence.flags) };

    heartbeat_commit(rq, &attr);
    GEM_BUG_ON!(unsafe { (*rq).sched.attr.priority < I915_PRIORITY_BARRIER });

    // Ensure the forced pulse gets a full period to execute.
    let _ = next_heartbeat(engine);

    0
}

// upstream: intel_engine_heartbeat.c set_heartbeat()
fn set_heartbeat(engine: *mut IntelEngineCs, delay: c_ulong) -> c_ulong {
    let old = unsafe {
        AtomicU64::from_ptr(core::ptr::addr_of_mut!(
            (*engine).props.heartbeat_interval_ms
        ))
        .swap(delay as u64, Ordering::SeqCst) as c_ulong
    };

    if delay != 0 {
        intel_engine_unpark_heartbeat(engine);
    } else {
        intel_engine_park_heartbeat(engine);
    }

    old
}

// upstream: intel_engine_heartbeat.c intel_engine_set_heartbeat()
pub fn intel_engine_set_heartbeat(engine: *mut IntelEngineCs, delay: c_ulong) -> i32 {
    let ce = unsafe { (*engine).kernel_context };
    let mut err = 0;

    if delay == 0 && !unsafe { intel_engine_has_preempt_reset(engine) } {
        return -ENODEV;
    }

    // FIXME: Remove together with equally marked hack in next_heartbeat.
    let preempt_timeout = unsafe { (*engine).props.preempt_timeout_ms };
    if delay != unsafe { (*engine).defaults.heartbeat_interval_ms }
        && delay < preempt_timeout.wrapping_mul(2)
    {
        if unsafe { intel_engine_uses_guc(engine) } {
            drm_notice!(
                unsafe { &mut (*(*engine).i915).drm },
                "%s heartbeat interval adjusted to a non-default value which may downgrade \
                 individual engine resets to full GPU resets!\n",
                unsafe { (*engine).name.as_ptr() },
            );
        } else {
            drm_notice!(
                unsafe { &mut (*(*engine).i915).drm },
                "%s heartbeat interval adjusted to a non-default value which may cause engine \
                 resets to target innocent contexts!\n",
                unsafe { (*engine).name.as_ptr() },
            );
        }
    }

    unsafe { intel_engine_pm_get(engine) };

    err = unsafe { mutex_lock_interruptible(&mut (*(*ce).timeline).mutex) };
    if err != 0 {
        unsafe { intel_engine_pm_put(engine) };
        return err;
    }

    if delay != unsafe { (*engine).props.heartbeat_interval_ms } {
        let saved = set_heartbeat(engine, delay);

        // recheck current execution
        if unsafe { intel_engine_has_preemption(engine) } {
            err = __intel_engine_pulse(engine);
            if err != 0 {
                let _ = set_heartbeat(engine, saved);
            }
        }
    }

    unsafe { crate::linux::mutex::mutex_unlock(&mut (*(*ce).timeline).mutex) };
    unsafe { intel_engine_pm_put(engine) };
    err
}

// upstream: intel_engine_heartbeat.c intel_engine_pulse()
#[unsafe(no_mangle)]
pub extern "C" fn intel_engine_pulse(engine: *mut IntelEngineCs) -> i32 {
    let ce = unsafe { (*engine).kernel_context };
    let mut err: i32;

    if !unsafe { intel_engine_has_preemption(engine) } {
        return -ENODEV;
    }

    if !unsafe { intel_engine_pm_get_if_awake(engine) } {
        return 0;
    }

    err = -EINTR;
    if unsafe { mutex_lock_interruptible(&mut (*(*ce).timeline).mutex) } == 0 {
        err = __intel_engine_pulse(engine);
        unsafe { crate::linux::mutex::mutex_unlock(&mut (*(*ce).timeline).mutex) };
    }

    unsafe { intel_engine_flush_submission(engine) };
    unsafe { intel_engine_pm_put(engine) };
    err
}

// upstream: intel_engine_heartbeat.c intel_engine_flush_barriers()
#[unsafe(no_mangle)]
pub extern "C" fn intel_engine_flush_barriers(engine: *mut IntelEngineCs) -> i32 {
    let attr = I915SchedAttr {
        priority: I915_PRIORITY_MIN,
    };
    let ce = unsafe { (*engine).kernel_context };
    let rq: *mut I915Request;
    let mut err: i32;

    if unsafe { llist_empty(core::ptr::addr_of!((*engine).barrier_tasks)) } {
        return 0;
    }

    if !unsafe { intel_engine_pm_get_if_awake(engine) } {
        return 0;
    }

    err = unsafe { mutex_lock_interruptible(&mut (*(*ce).timeline).mutex) };
    if err != 0 {
        err = -EINTR;
        unsafe { intel_engine_pm_put(engine) };
        return err;
    }

    rq = heartbeat_create(ce, GFP_KERNEL);
    if IS_ERR!(rq) {
        err = PTR_ERR!(rq);
    } else {
        heartbeat_commit(rq, &attr);
        err = 0;
    }

    unsafe { crate::linux::mutex::mutex_unlock(&mut (*(*ce).timeline).mutex) };
    unsafe { intel_engine_pm_put(engine) };
    err
}
