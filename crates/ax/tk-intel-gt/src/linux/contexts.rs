// SPDX-License-Identifier: MIT
// Copyright © 2026 Intel Corporation and TheKernel contributors.
// Linux v7.2.3 drivers/gpu/drm/i915/gt/intel_context.h inline helpers.

#![allow(unsafe_code)]

use crate::{
    intel_context_upstream::{
        DmaFence, DmaFenceCb, I915ActiveFence, I915SchedEngine, IntelContext,
    },
    intel_engine_cs_upstream::IntelEngineCs,
    linux_memory::{atomic_inc, atomic_read, refcount_dec_and_test},
};

unsafe extern "C" fn i915_active_noop(_fence: *mut DmaFence, _cb: *mut DmaFenceCb) {}

pub unsafe fn init_active_fence(active: *mut I915ActiveFence) {
    assert!(!active.is_null());
    unsafe {
        (*active).fence = core::ptr::null_mut();
        (*active).cb.func = Some(i915_active_noop);
    }
}

/// C's inline `intel_context_*` helpers accept a context pointer. Rust
/// translations also use references for fields embedded directly in records.
pub trait IntelContextPtr {
    fn intel_context_ptr(self) -> *mut IntelContext;
}

impl IntelContextPtr for *mut IntelContext {
    fn intel_context_ptr(self) -> *mut IntelContext {
        self
    }
}
impl IntelContextPtr for *const IntelContext {
    fn intel_context_ptr(self) -> *mut IntelContext {
        self.cast_mut()
    }
}
impl IntelContextPtr for &IntelContext {
    fn intel_context_ptr(self) -> *mut IntelContext {
        (self as *const IntelContext).cast_mut()
    }
}
impl IntelContextPtr for &mut IntelContext {
    fn intel_context_ptr(self) -> *mut IntelContext {
        self
    }
}

/// Linux `intel_context_inflight()` removes the two low tag bits from the
/// RCU-published in-flight engine pointer.
pub unsafe fn intel_context_inflight<C: IntelContextPtr>(context: C) -> *mut IntelEngineCs {
    let context = context.intel_context_ptr();
    assert!(!context.is_null());
    let inflight = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*context).inflight)) };
    ((inflight as usize) & !3usize) as *mut IntelEngineCs
}

#[inline]
pub fn intel_context_is_child<C: IntelContextPtr>(context: C) -> bool {
    let context = context.intel_context_ptr();
    assert!(!context.is_null());
    unsafe { !(*context).parallel.parent.is_null() }
}

#[inline]
pub fn intel_context_is_parent<C: IntelContextPtr>(context: C) -> bool {
    let context = context.intel_context_ptr();
    assert!(!context.is_null());
    unsafe { (*context).parallel.number_children != 0 }
}

#[inline]
pub fn intel_context_is_parallel<C: IntelContextPtr>(context: C) -> bool {
    let context = context.intel_context_ptr();
    intel_context_is_child(context) || intel_context_is_parent(context)
}

#[inline]
pub fn intel_context_is_pinned<C: IntelContextPtr>(context: C) -> bool {
    let context = context.intel_context_ptr();
    assert!(!context.is_null());
    unsafe { atomic_read(&(*context).pin_count) != 0 }
}

/// `intel_context_to_parent()` from gt/intel_context.h. The caller holds the
/// parent pin that makes a child context's weak parent pointer stable.
pub fn intel_context_to_parent<C: IntelContextPtr>(context: C) -> *mut IntelContext {
    let context = context.intel_context_ptr();
    assert!(!context.is_null());
    if intel_context_is_child(context) {
        let parent = unsafe { (*context).parallel.parent };
        assert!(
            !parent.is_null() && intel_context_is_pinned(parent),
            "GEM_BUG_ON: child parent without pin"
        );
        parent
    } else {
        context
    }
}

/// `intel_context_sched_disable_unpin()` from gt/intel_context.h.
pub fn intel_context_sched_disable_unpin<C: IntelContextPtr>(context: C) {
    let context = context.intel_context_ptr();
    assert!(!context.is_null());
    unsafe { crate::intel_context_upstream::__intel_context_do_unpin(context, 2) };
}

/// `intel_context_is_banned()` from gt/intel_context.h.
pub fn intel_context_is_banned<C: IntelContextPtr>(context: C) -> bool {
    let context = context.intel_context_ptr();
    assert!(!context.is_null());
    crate::linux::bits::test_bit(crate::intel_context_upstream::CONTEXT_BANNED, unsafe {
        &(*context).flags
    })
}

/// `i915_sched_engine_get()` from i915_scheduler.h.
pub unsafe fn i915_sched_engine_get(engine: *mut I915SchedEngine) -> *mut I915SchedEngine {
    assert!(!engine.is_null());
    let got = unsafe { crate::linux_memory::kref_get_unless_zero(&mut (*engine).r#ref) };
    assert!(got, "i915_sched_engine_get requires a live reference");
    engine
}

/// `i915_sched_engine_put()` from i915_scheduler.h.
pub unsafe fn i915_sched_engine_put(engine: *mut I915SchedEngine) {
    assert!(!engine.is_null());
    if crate::linux_memory::refcount_dec_and_test(unsafe { &mut (*engine).r#ref.refcount }) {
        let destroy = unsafe { (*engine).destroy };
        let Some(destroy) = destroy else {
            panic!("i915_sched_engine final reference has no destroy callback");
        };
        unsafe { destroy(&mut (*engine).r#ref) };
    }
}

/// `i915_sched_engine_is_empty()` from i915_scheduler.h.
pub unsafe fn i915_sched_engine_is_empty(engine: *const I915SchedEngine) -> bool {
    assert!(!engine.is_null());
    unsafe { (*engine).queue.root.node.is_null() }
}

/// Linux 7.2.3 `i915_sched_engine_reset_on_empty()`.
pub unsafe fn i915_sched_engine_reset_on_empty(engine: *mut I915SchedEngine) {
    assert!(!engine.is_null());
    if unsafe { i915_sched_engine_is_empty(engine) } {
        unsafe { (*engine).no_priolist = false };
    }
}

#[inline]
pub fn intel_context_is_barrier<C: IntelContextPtr>(context: C) -> bool {
    let context = context.intel_context_ptr();
    assert!(!context.is_null());
    unsafe { (*context).flags & (1 << 0) != 0 }
}

#[inline]
pub fn intel_context_is_closed<C: IntelContextPtr>(context: C) -> bool {
    let context = context.intel_context_ptr();
    assert!(!context.is_null());
    unsafe { (*context).flags & (1 << 4) != 0 }
}

#[inline]
pub fn intel_context_is_schedulable<C: IntelContextPtr>(context: C) -> bool {
    let context = context.intel_context_ptr();
    assert!(!context.is_null());
    unsafe { (*context).flags & ((1 << 13) | (1 << 6)) == 0 }
}

#[inline]
pub fn intel_context_get<C: IntelContextPtr>(context: C) -> *mut IntelContext {
    let context = context.intel_context_ptr();
    assert!(!context.is_null());
    unsafe {
        let reference = core::ptr::addr_of_mut!((*context).r#ref);
        let count = core::ptr::addr_of_mut!((*reference).refcount)
            .cast::<crate::intel_context_upstream::Kref>();
        atomic_inc(&mut (*count).refcount.refs);
    }
    context
}

pub fn intel_context_put<C: IntelContextPtr>(context: C) {
    let context = context.intel_context_ptr();
    assert!(!context.is_null());
    unsafe {
        let reference = core::ptr::addr_of_mut!((*context).r#ref);
        let count = core::ptr::addr_of_mut!((*reference).refcount)
            .cast::<crate::intel_context_upstream::Kref>();
        if refcount_dec_and_test(&mut (*count).refcount) {
            if let Some(destroy) = (*(*context).ops).destroy {
                destroy(count);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intel_engine_cs_upstream::AtomicT;

    #[test]
    fn context_header_predicates_follow_inline_i915_helpers() {
        let mut parent: IntelContext = unsafe { core::mem::zeroed() };
        parent.parallel.number_children = 2;
        assert!(intel_context_is_parent(&parent));
        assert!(!intel_context_is_child(&parent));
        parent.flags = 1;
        assert!(intel_context_is_barrier(&parent));
        parent.flags = 1 << 4;
        assert!(intel_context_is_closed(&parent));
        parent.pin_count = AtomicT { counter: 1 };
        assert!(intel_context_is_pinned(&parent));
    }
}
