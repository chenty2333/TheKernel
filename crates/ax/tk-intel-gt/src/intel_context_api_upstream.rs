// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
//
// Source-order transcription of active inline helpers in Linux 7.2.3
// drivers/gpu/drm/i915/gt/intel_context.h. The target configuration has
// CONFIG_LOCKDEP=n and CONFIG_DRM_I915_REPLAY_GPU_HANGS_API=n. Functions use
// the source-owned context/timeline layouts and existing LinuxKPI refcount,
// bitops, mutex, PM and time services.

#![allow(non_snake_case, non_camel_case_types, unsafe_op_in_unsafe_fn)]

use core::{
    ffi::{c_int, c_ulong},
    sync::atomic::{AtomicI32, Ordering},
};

use crate::{
    i915_gem_ww_upstream::I915GemWwCtx,
    intel_context_types_upstream::{
        CONTEXT_BANNED, CONTEXT_BARRIER_BIT, CONTEXT_CLOSED_BIT, CONTEXT_EXITING,
        CONTEXT_FORCE_SINGLE_SUBMISSION, CONTEXT_IS_PARKING, CONTEXT_NOPREEMPT, CONTEXT_OWN_STATE,
        CONTEXT_USE_SEMAPHORES, COPS_HAS_INFLIGHT_BIT, IntelContext,
    },
    intel_engine_cs_upstream::Mutex,
    intel_timeline_types_upstream::IntelTimeline,
    linux::{bits, memory, mutex, pm, primitives},
};

unsafe extern "C" {
    pub fn mutex_lock_interruptible(lock: *mut Mutex) -> c_int;
}

#[inline]
unsafe fn atomic_cmpxchg(
    value: *mut crate::intel_engine_cs_upstream::AtomicT,
    old: i32,
    new: i32,
) -> i32 {
    unsafe {
        AtomicI32::from_ptr(core::ptr::addr_of_mut!((*value).counter))
            .compare_exchange(old, new, Ordering::SeqCst, Ordering::SeqCst)
            .unwrap_or_else(|observed| observed)
    }
}

// upstream: intel_context.h intel_context_is_child()
pub unsafe fn intel_context_is_child(ce: *mut IntelContext) -> bool {
    unsafe { !(*ce).parallel.parent.is_null() }
}

// upstream: intel_context.h intel_context_is_parent()
pub unsafe fn intel_context_is_parent(ce: *mut IntelContext) -> bool {
    unsafe { (*ce).parallel.number_children != 0 }
}

// upstream: intel_context.h intel_context_to_parent()
pub unsafe fn intel_context_to_parent(ce: *mut IntelContext) -> *mut IntelContext {
    unsafe {
        if intel_context_is_child(ce) {
            let parent = (*ce).parallel.parent;
            GEM_BUG_ON!(!intel_context_is_pinned(parent));
            parent
        } else {
            ce
        }
    }
}

// upstream: intel_context.h intel_context_is_parallel()
pub unsafe fn intel_context_is_parallel(ce: *mut IntelContext) -> bool {
    unsafe { intel_context_is_child(ce) || intel_context_is_parent(ce) }
}

// upstream: intel_context.h intel_context_lock_pinned()
pub unsafe fn intel_context_lock_pinned(ce: *mut IntelContext) -> c_int {
    unsafe { mutex_lock_interruptible(core::ptr::addr_of_mut!((*ce).pin_mutex)) }
}

// upstream: intel_context.h intel_context_is_pinned()
pub unsafe fn intel_context_is_pinned(ce: *mut IntelContext) -> bool {
    memory::atomic_read(unsafe { &(*ce).pin_count }) != 0
}

// upstream: intel_context.h intel_context_cancel_request()
pub unsafe fn intel_context_cancel_request(
    ce: *mut IntelContext,
    rq: *mut crate::i915_request_types_upstream::I915Request,
) {
    unsafe {
        let cancel = (*(*ce).ops).cancel_request;
        GEM_BUG_ON!(cancel.is_none());
        cancel.unwrap()(ce, rq);
    }
}

// upstream: intel_context.h intel_context_unlock_pinned()
pub unsafe fn intel_context_unlock_pinned(ce: *mut IntelContext) {
    unsafe { mutex::mutex_unlock(&mut (*ce).pin_mutex) }
}

// upstream: intel_context.h intel_context_pin_if_active()
pub unsafe fn intel_context_pin_if_active(ce: *mut IntelContext) -> bool {
    memory::atomic_inc_not_zero(unsafe { &mut (*ce).pin_count })
}

// upstream: intel_context.h intel_context_pin()
pub unsafe fn intel_context_pin(ce: *mut IntelContext) -> c_int {
    if unsafe { intel_context_pin_if_active(ce) } {
        0
    } else {
        unsafe { crate::intel_context_upstream::__intel_context_do_pin(ce) }
    }
}

// upstream: intel_context.h intel_context_pin_ww()
pub unsafe fn intel_context_pin_ww(ce: *mut IntelContext, ww: *mut I915GemWwCtx) -> c_int {
    if unsafe { intel_context_pin_if_active(ce) } {
        0
    } else {
        unsafe { crate::intel_context_upstream::__intel_context_do_pin_ww(ce, ww) }
    }
}

// upstream: intel_context.h __intel_context_pin()
pub unsafe fn __intel_context_pin(ce: *mut IntelContext) {
    unsafe {
        GEM_BUG_ON!(!intel_context_is_pinned(ce));
        memory::atomic_inc(&mut (*ce).pin_count);
    }
}

// upstream: intel_context.h intel_context_sched_disable_unpin()
pub unsafe fn intel_context_sched_disable_unpin(ce: *mut IntelContext) {
    unsafe { crate::intel_context_upstream::__intel_context_do_unpin(ce, 2) }
}

// upstream: intel_context.h intel_context_unpin()
pub unsafe fn intel_context_unpin(ce: *mut IntelContext) {
    unsafe {
        let sched_disable = (*(*ce).ops).sched_disable;
        if sched_disable.is_none() {
            crate::intel_context_upstream::__intel_context_do_unpin(ce, 1);
        } else {
            loop {
                if memory::atomic_add_unless(&mut (*ce).pin_count, -1, 1) {
                    break;
                }
                if atomic_cmpxchg(core::ptr::addr_of_mut!((*ce).pin_count), 1, 2) == 1 {
                    sched_disable.unwrap()(ce);
                    break;
                }
            }
        }
    }
}

// upstream: intel_context.h intel_context_enter()
pub unsafe fn intel_context_enter(ce: *mut IntelContext) {
    unsafe {
        crate::linux::assertion::lockdep_assert_held(&(*(*ce).timeline).mutex);
        let old = (*ce).active_count;
        (*ce).active_count = old.wrapping_add(1);
        if old != 0 {
            return;
        }
        (*(*ce).ops).enter.unwrap()(ce);
        (*ce).wakeref = pm::intel_gt_pm_get((*(*ce).vm).gt);
    }
}

// upstream: intel_context.h intel_context_mark_active()
pub unsafe fn intel_context_mark_active(ce: *mut IntelContext) {
    unsafe {
        if !bits::test_bit(CONTEXT_IS_PARKING, &(*ce).flags) {
            crate::linux::assertion::lockdep_assert_held(&(*(*ce).timeline).mutex);
        }
        (*ce).active_count = (*ce).active_count.wrapping_add(1);
    }
}

// upstream: intel_context.h intel_context_exit()
pub unsafe fn intel_context_exit(ce: *mut IntelContext) {
    unsafe {
        crate::linux::assertion::lockdep_assert_held(&(*(*ce).timeline).mutex);
        GEM_BUG_ON!((*ce).active_count == 0);
        (*ce).active_count -= 1;
        if (*ce).active_count != 0 {
            return;
        }
        pm::intel_gt_pm_put_async((*(*ce).vm).gt, (*ce).wakeref);
        (*(*ce).ops).exit.unwrap()(ce);
    }
}

// upstream: intel_context.h intel_context_get()
pub unsafe fn intel_context_get(ce: *mut IntelContext) -> *mut IntelContext {
    unsafe { memory::kref_get(core::ptr::addr_of_mut!((*ce).r#ref.refcount).cast()) };
    ce
}

// upstream: intel_context.h intel_context_put()
pub unsafe fn intel_context_put(ce: *mut IntelContext) {
    unsafe {
        let release = (*(*ce).ops).destroy.unwrap();
        memory::kref_put(
            core::ptr::addr_of_mut!((*ce).r#ref.refcount).cast(),
            release,
        );
    }
}

// upstream: intel_context.h intel_context_timeline_lock()
pub unsafe fn intel_context_timeline_lock(ce: *mut IntelContext) -> *mut IntelTimeline {
    unsafe {
        let tl = (*ce).timeline;
        let err = if intel_context_is_parent(ce) {
            // CONFIG_LOCKDEP=n: mutex_lock_interruptible_nested() maps to the
            // ordinary interruptible lock, discarding its subclass argument.
            mutex_lock_interruptible(core::ptr::addr_of_mut!((*tl).mutex))
        } else if intel_context_is_child(ce) {
            mutex_lock_interruptible(core::ptr::addr_of_mut!((*tl).mutex))
        } else {
            mutex_lock_interruptible(core::ptr::addr_of_mut!((*tl).mutex))
        };
        if err != 0 {
            crate::linux_config::ERR_PTR(err)
        } else {
            tl
        }
    }
}

// upstream: intel_context.h intel_context_timeline_unlock()
pub unsafe fn intel_context_timeline_unlock(tl: *mut IntelTimeline) {
    unsafe { mutex::mutex_unlock(&mut (*tl).mutex) }
}

// upstream: intel_context.h intel_context_is_barrier()
pub unsafe fn intel_context_is_barrier(ce: *const IntelContext) -> bool {
    bits::test_bit(CONTEXT_BARRIER_BIT, unsafe { &(*ce).flags })
}

// upstream: intel_context.h intel_context_close()
pub unsafe fn intel_context_close(ce: *mut IntelContext) {
    unsafe {
        bits::set_bit(CONTEXT_CLOSED_BIT, &mut (*ce).flags);
        if let Some(close) = (*(*ce).ops).close {
            close(ce);
        }
    }
}

// upstream: intel_context.h intel_context_is_closed()
pub unsafe fn intel_context_is_closed(ce: *const IntelContext) -> bool {
    bits::test_bit(CONTEXT_CLOSED_BIT, unsafe { &(*ce).flags })
}

// upstream: intel_context.h intel_context_has_inflight()
pub unsafe fn intel_context_has_inflight(ce: *const IntelContext) -> bool {
    bits::test_bit(COPS_HAS_INFLIGHT_BIT, unsafe { &(*(*ce).ops).flags })
}

// upstream: intel_context.h intel_context_use_semaphores()
pub unsafe fn intel_context_use_semaphores(ce: *const IntelContext) -> bool {
    bits::test_bit(CONTEXT_USE_SEMAPHORES, unsafe { &(*ce).flags })
}

// upstream: intel_context.h intel_context_set_use_semaphores()
pub unsafe fn intel_context_set_use_semaphores(ce: *mut IntelContext) {
    bits::set_bit(CONTEXT_USE_SEMAPHORES, unsafe { &mut (*ce).flags });
}

// upstream: intel_context.h intel_context_clear_use_semaphores()
pub unsafe fn intel_context_clear_use_semaphores(ce: *mut IntelContext) {
    bits::clear_bit(CONTEXT_USE_SEMAPHORES, unsafe { &mut (*ce).flags });
}

// upstream: intel_context.h intel_context_is_banned()
pub unsafe fn intel_context_is_banned(ce: *const IntelContext) -> bool {
    bits::test_bit(CONTEXT_BANNED, unsafe { &(*ce).flags })
}

// upstream: intel_context.h intel_context_set_banned()
pub unsafe fn intel_context_set_banned(ce: *mut IntelContext) -> bool {
    bits::test_and_set_bit(CONTEXT_BANNED, unsafe { &mut (*ce).flags })
}

// upstream: intel_context.h intel_context_is_schedulable()
pub unsafe fn intel_context_is_schedulable(ce: *const IntelContext) -> bool {
    !bits::test_bit(CONTEXT_EXITING, unsafe { &(*ce).flags })
        && !bits::test_bit(CONTEXT_BANNED, unsafe { &(*ce).flags })
}

// upstream: intel_context.h intel_context_is_exiting()
pub unsafe fn intel_context_is_exiting(ce: *const IntelContext) -> bool {
    bits::test_bit(CONTEXT_EXITING, unsafe { &(*ce).flags })
}

// upstream: intel_context.h intel_context_set_exiting()
pub unsafe fn intel_context_set_exiting(ce: *mut IntelContext) -> bool {
    bits::test_and_set_bit(CONTEXT_EXITING, unsafe { &mut (*ce).flags })
}

// upstream: intel_context.h intel_context_force_single_submission()
pub unsafe fn intel_context_force_single_submission(ce: *const IntelContext) -> bool {
    bits::test_bit(CONTEXT_FORCE_SINGLE_SUBMISSION, unsafe { &(*ce).flags })
}

// upstream: intel_context.h intel_context_set_single_submission()
pub unsafe fn intel_context_set_single_submission(ce: *mut IntelContext) {
    bits::__set_bit(CONTEXT_FORCE_SINGLE_SUBMISSION, unsafe { &mut (*ce).flags });
}

// upstream: intel_context.h intel_context_nopreempt()
pub unsafe fn intel_context_nopreempt(ce: *const IntelContext) -> bool {
    bits::test_bit(CONTEXT_NOPREEMPT, unsafe { &(*ce).flags })
}

// upstream: intel_context.h intel_context_set_nopreempt()
pub unsafe fn intel_context_set_nopreempt(ce: *mut IntelContext) {
    bits::set_bit(CONTEXT_NOPREEMPT, unsafe { &mut (*ce).flags });
}

// upstream: intel_context.h intel_context_clear_nopreempt()
pub unsafe fn intel_context_clear_nopreempt(ce: *mut IntelContext) {
    bits::clear_bit(CONTEXT_NOPREEMPT, unsafe { &mut (*ce).flags });
}

// upstream: intel_context.h intel_context_has_own_state()
pub unsafe fn intel_context_has_own_state(_ce: *const IntelContext) -> bool {
    false
}

// upstream: intel_context.h intel_context_set_own_state()
pub unsafe fn intel_context_set_own_state(_ce: *mut IntelContext) -> bool {
    true
}

// upstream: intel_context.h intel_context_clock()
pub fn intel_context_clock() -> u64 {
    primitives::ktime_get_raw_fast_ns()
}
