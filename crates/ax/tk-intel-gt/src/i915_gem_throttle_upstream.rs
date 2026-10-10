// SPDX-License-Identifier: MIT
// Copyright © 2014-2016 Intel Corporation.
// Source-order translation of Linux v7.2.3 drivers/gpu/drm/i915/gem/i915_gem_throttle.c.

#![allow(dead_code, non_camel_case_types, non_snake_case, unsafe_code)]

use core::{
    ffi::{c_int, c_long, c_ulong, c_void},
    mem::offset_of,
};

use crate::{
    i915_gem_context_types_upstream::{I915GemContext, I915GemEnginesIter},
    i915_gem_context_upstream::{i915_gem_context_put, i915_gem_engines_iter_next},
    i915_request_types_upstream::I915Request,
    i915_request_upstream::i915_request_wait,
    intel_context_types_upstream::IntelContext,
    intel_engine_cs_upstream::Mutex,
    intel_timeline_types_upstream::IntelTimeline,
    linux::{
        gem::DrmFile,
        i915::to_i915,
        memory::kref_get_unless_zero,
        mutex::{mutex_lock, mutex_unlock},
        primitives::{jiffies, msecs_to_jiffies, time_after},
        rcu::{rcu_read_lock, rcu_read_unlock},
        requests::{i915_request_completed, i915_request_get, i915_request_put},
        xarray::XArray,
    },
    linux_config::MAX_SCHEDULE_TIMEOUT,
    linux_i915_private::DrmI915Private,
};

const I915_WAIT_INTERRUPTIBLE: u32 = 1 << 0;

#[repr(C)]
struct I915FilePrivateView {
    i915: *mut DrmI915Private,
    _file_or_rcu: [u8; 16],
    _proto_context_lock: Mutex,
    _proto_context_xa: XArray,
    context_xa: XArray,
}

const _: [(); 0] = [(); offset_of!(I915FilePrivateView, i915)];
const _: [(); 8] = [(); offset_of!(I915FilePrivateView, _file_or_rcu)];

unsafe extern "C" {
    fn intel_gt_terminally_wedged(gt: *mut crate::intel_gt_types_upstream::IntelGt) -> c_int;
}

// upstream: i915_gem_throttle.c i915_gem_throttle_ioctl()
pub unsafe fn i915_gem_throttle_ioctl(
    dev: *mut c_void,
    _data: *mut c_void,
    file: *mut DrmFile,
) -> c_int {
    if dev.is_null() || file.is_null() {
        return -crate::linux_config::EINVAL;
    }
    let recent_enough = jiffies().wrapping_sub(msecs_to_jiffies(20u32) as c_ulong);
    let file_priv = unsafe { (*file).driver_priv.cast::<I915FilePrivateView>() };
    if file_priv.is_null() {
        return -crate::linux_config::EINVAL;
    }
    let i915 = unsafe { to_i915(dev) };
    let gt = unsafe { crate::linux::i915::to_gt(i915) };
    let mut ret = unsafe { intel_gt_terminally_wedged(gt) } as c_long;
    if ret != 0 {
        return ret as c_int;
    }

    unsafe { rcu_read_lock() };
    let mut idx = 0 as c_ulong;
    let mut context = core::ptr::null_mut::<I915GemContext>();
    xa_for_each!(unsafe { &mut (*file_priv).context_xa }, idx, context, {
        let context: *mut I915GemContext = context;
        if !unsafe { kref_get_unless_zero(&mut (*context).r#ref) } {
            continue;
        }
        unsafe { rcu_read_unlock() };

        let ctx = context;
        let engines_mutex = unsafe { core::ptr::addr_of_mut!((*ctx).engines_mutex) };
        unsafe { mutex_lock(engines_mutex) };
        let engines = unsafe { (*ctx).engines };
        let mut iter = I915GemEnginesIter { idx: 0, engines };
        loop {
            let ce = unsafe { i915_gem_engines_iter_next(&mut iter) };
            if ce.is_null() {
                break;
            }
            let ce: *mut IntelContext = ce;
            let timeline: *mut IntelTimeline = unsafe { (*ce).timeline };
            if timeline.is_null() {
                continue;
            }

            let timeline_mutex = unsafe { core::ptr::addr_of_mut!((*timeline).mutex) };
            unsafe { mutex_lock(timeline_mutex) };
            let mut rq = core::ptr::null_mut::<I915Request>();
            let mut target = core::ptr::null_mut::<I915Request>();
            list_for_each_entry_reverse!(
                rq,
                unsafe { core::ptr::addr_of_mut!((*timeline).requests) },
                link,
                {
                    if i915_request_completed(unsafe { &*rq }) {
                        break;
                    }
                    if time_after(unsafe { (*rq).emitted_jiffies }, recent_enough) {
                        continue;
                    }
                    target = unsafe { i915_request_get(rq) };
                    break;
                }
            );
            unsafe { mutex_unlock(timeline_mutex) };

            if !target.is_null() {
                ret = unsafe {
                    i915_request_wait(
                        target,
                        I915_WAIT_INTERRUPTIBLE,
                        MAX_SCHEDULE_TIMEOUT as c_long,
                    )
                };
                unsafe { i915_request_put(target) };
                if ret < 0 {
                    break;
                }
            }
        }
        unsafe { mutex_unlock(engines_mutex) };
        unsafe { i915_gem_context_put(ctx) };
        unsafe { rcu_read_lock() };
    });
    unsafe { rcu_read_unlock() };

    if ret < 0 { ret as c_int } else { 0 }
}
