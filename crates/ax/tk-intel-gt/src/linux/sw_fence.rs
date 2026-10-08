// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Linux 7.2.3 i915 software-fence primitives and scheduler traversal macros.
//!
//! `i915_scheduler_types.h` defines `for_each_waiter()` and
//! `for_each_signaler()` over intrusive dependency links in
//! `i915_request.sched`. The current context module keeps the enclosing
//! `I915Dependency` record opaque; this exact-layout view supplies the fields
//! needed by those source macros without changing that shared declaration.

#![allow(unsafe_code)]

use core::{
    ffi::c_ulong,
    sync::atomic::{AtomicI32, Ordering},
};

use crate::{
    intel_context_upstream::{I915Request, I915SchedNode, I915SwFence, WaitQueueHead},
    intel_engine_cs_upstream::ListHead,
};

/// Source-derived overlay of Linux v7.2.3 `struct i915_dependency`.
///
/// The translated context module currently uses an opaque 72-byte
/// `I915Dependency` storage declaration. This view has the same C layout and
/// exposes the exact pointer/list members used by scheduler traversal.
#[repr(C)]
pub struct I915DependencyLayout {
    pub signaler: *mut I915SchedNode,
    pub waiter: *mut I915SchedNode,
    pub signal_link: ListHead,
    pub wait_link: ListHead,
    pub dfs_link: ListHead,
    pub flags: c_ulong,
}

pub const I915_DEPENDENCY_ALLOC: c_ulong = 1 << 0;
pub const I915_DEPENDENCY_EXTERNAL: c_ulong = 1 << 1;
pub const I915_DEPENDENCY_WEAK: c_ulong = 1 << 2;

const _: [(); 72] = [(); core::mem::size_of::<I915DependencyLayout>()];
const _: [(); 8] = [(); core::mem::align_of::<I915DependencyLayout>()];
const _: [(); 0] = [(); core::mem::offset_of!(I915DependencyLayout, signaler)];
const _: [(); 8] = [(); core::mem::offset_of!(I915DependencyLayout, waiter)];
const _: [(); 16] = [(); core::mem::offset_of!(I915DependencyLayout, signal_link)];
const _: [(); 32] = [(); core::mem::offset_of!(I915DependencyLayout, wait_link)];
const _: [(); 48] = [(); core::mem::offset_of!(I915DependencyLayout, dfs_link)];
const _: [(); 64] = [(); core::mem::offset_of!(I915DependencyLayout, flags)];

// `i915_scheduler_types.h` embeds the scheduler node at this source-derived
// request offset. The two list anchors are then the leading 32 bytes of that
// node, as asserted by intel_context_upstream.rs.
const _: [(); 672] = [(); core::mem::size_of::<I915Request>()];
const _: [(); 304] = [(); core::mem::offset_of!(I915Request, sched)];
const _: [(); 304] = [(); core::mem::offset_of!(I915Request, sched)
    + core::mem::offset_of!(I915SchedNode, signalers_list)];
const _: [(); 320] = [(); core::mem::offset_of!(I915Request, sched)
    + core::mem::offset_of!(I915SchedNode, waiters_list)];

// The i915_sw_fence.h record is not itself traversed by these macros, but is
// embedded in i915_request and is part of the surrounding source layout. In
// the wt-dev x86_64 config, LOCKDEP and SW_FENCE_CHECK_DAG are disabled.
const _: [(); 40] = [(); core::mem::size_of::<I915SwFence>()];
const _: [(); 8] = [(); core::mem::align_of::<I915SwFence>()];
const _: [(); 0] = [(); core::mem::offset_of!(I915SwFence, wait)];
const _: [(); 24] = [(); core::mem::offset_of!(I915SwFence, fn_)];
const _: [(); 32] = [(); core::mem::offset_of!(I915SwFence, pending)];
const _: [(); 36] = [(); core::mem::offset_of!(I915SwFence, error)];
const _: [(); 24] = [(); core::mem::size_of::<WaitQueueHead>()];

/// Atomic view of `i915_sw_fence.pending` (`atomic_t` in Linux).
#[inline]
fn pending_atomic(fence: &I915SwFence) -> &AtomicI32 {
    let counter = core::ptr::addr_of!(fence.pending.counter).cast_mut();
    // SAFETY: `AtomicT` is the `repr(C)` Linux atomic_t layout (one aligned
    // i32), and this field is accessed atomically by the sw-fence API.
    unsafe { AtomicI32::from_ptr(counter) }
}

/// Atomic view used by Linux `cmpxchg(&fence->error, 0, error)`.
#[inline]
fn error_atomic(fence: &I915SwFence) -> &AtomicI32 {
    let error = core::ptr::addr_of!(fence.error).cast_mut();
    // SAFETY: `error` is an aligned i32 in the source-layout fence record;
    // all concurrent error updates use cmpxchg, so its atomic view is valid.
    unsafe { AtomicI32::from_ptr(error) }
}

/// Linux 7.2.3 `i915_sw_fence_signaled()`.
#[inline]
pub fn i915_sw_fence_signaled(fence: &I915SwFence) -> bool {
    pending_atomic(fence).load(Ordering::Relaxed) <= 0
}

/// Linux 7.2.3 `i915_sw_fence_done()`.
#[inline]
pub fn i915_sw_fence_done(fence: &I915SwFence) -> bool {
    pending_atomic(fence).load(Ordering::Relaxed) < 0
}

/// Linux 7.2.3 `i915_sw_fence_set_error_once()`.
#[inline]
pub fn i915_sw_fence_set_error_once(fence: &mut I915SwFence, error: i32) {
    if error != 0 {
        let _ = error_atomic(fence).compare_exchange(0, error, Ordering::SeqCst, Ordering::SeqCst);
    }
}

/// Linux 7.2.3 `i915_sw_fence_await()`.
///
/// Returns false once the fence has been committed (pending < 1); otherwise
/// atomically reserves one more signaler and returns true.
#[inline]
pub fn i915_sw_fence_await(fence: &mut I915SwFence) -> bool {
    let pending = pending_atomic(fence);
    let mut observed = pending.load(Ordering::Relaxed);
    loop {
        if observed < 1 {
            return false;
        }
        match pending.compare_exchange_weak(
            observed,
            observed.wrapping_add(1),
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => return true,
            Err(current) => observed = current,
        }
    }
}

/// Internal iteration guard: C `for` loops execute their increment expression
/// on both ordinary fallthrough and `continue`. Rust's scope drop gives the
/// same behavior while keeping the volatile next-link read after the body.
#[doc(hidden)]
pub struct AdvanceCursor<'a> {
    cursor: &'a mut *mut ListHead,
    current: *mut ListHead,
}

impl<'a> AdvanceCursor<'a> {
    #[doc(hidden)]
    pub fn new(cursor: &'a mut *mut ListHead, current: *mut ListHead) -> Self {
        Self { cursor, current }
    }
}

impl Drop for AdvanceCursor<'_> {
    fn drop(&mut self) {
        *self.cursor =
            unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*self.current).next)) };
    }
}

/// Expand an i915 `for_each_waiter(p, rq)` loop.
///
/// Like Linux's `list_for_each_entry_lockless`, this uses single volatile
/// reads for the list head/next links. Its caller must provide the same
/// lifetime/RCU or scheduler-lock protection as the source call site. The
/// block may use `continue` and `break` as in the C macro.
#[macro_export]
macro_rules! for_each_waiter {
    ($pos:ident, $request:expr, $body:block) => {{
        let __request = ($request) as *const $crate::intel_context_upstream::I915Request;
        let __head = unsafe {
            core::ptr::addr_of!((*__request).sched.waiters_list)
                as *const $crate::intel_engine_cs_upstream::ListHead
                as *mut $crate::intel_engine_cs_upstream::ListHead
        };
        let __offset = core::mem::offset_of!(
            $crate::linux::sw_fence::I915DependencyLayout,
            wait_link
        );
        let mut __cursor = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*__head).next)) };
        let mut $pos: *mut $crate::linux::sw_fence::I915DependencyLayout = core::ptr::null_mut();
        while !core::ptr::eq(__cursor, __head) {
            let __current = __cursor;
            $pos = __current
                .cast::<u8>()
                .wrapping_sub(__offset)
                .cast::<$crate::linux::sw_fence::I915DependencyLayout>();
            let __advance = $crate::linux::sw_fence::AdvanceCursor::new(
                &mut __cursor,
                __current,
            );
            $body
            drop(__advance);
        }
    }};
}

/// Expand an i915 `for_each_signaler(p, rq)` loop.
///
/// This is the RCU list form; callers must hold the source-mandated RCU
/// read-side section. List links are sampled with volatile single reads, as
/// `list_entry_rcu()` / `rcu_dereference_raw()` do for this x86_64 target.
#[macro_export]
macro_rules! for_each_signaler {
    ($pos:ident, $request:expr, $body:block) => {{
        let __request = ($request) as *const $crate::intel_context_upstream::I915Request;
        let __head = unsafe {
            core::ptr::addr_of!((*__request).sched.signalers_list)
                as *const $crate::intel_engine_cs_upstream::ListHead
                as *mut $crate::intel_engine_cs_upstream::ListHead
        };
        let __offset = core::mem::offset_of!(
            $crate::linux::sw_fence::I915DependencyLayout,
            signal_link
        );
        let mut __cursor = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*__head).next)) };
        let mut $pos: *mut $crate::linux::sw_fence::I915DependencyLayout = core::ptr::null_mut();
        while !core::ptr::eq(__cursor, __head) {
            let __current = __cursor;
            $pos = __current
                .cast::<u8>()
                .wrapping_sub(__offset)
                .cast::<$crate::linux::sw_fence::I915DependencyLayout>();
            let __advance = $crate::linux::sw_fence::AdvanceCursor::new(
                &mut __cursor,
                __current,
            );
            $body
            drop(__advance);
        }
    }};
}
