// SPDX-License-Identifier: MIT
// Copyright © 2025 Intel Corporation.
// Source-order translation of Linux v7.2.3 drivers/gpu/drm/i915/gem/i915_gem_object_frontbuffer.c.

#![allow(dead_code, non_camel_case_types, non_snake_case, unsafe_code)]

use core::{
    ffi::{c_int, c_ulong, c_void},
    mem::{offset_of, size_of},
    ptr,
    sync::atomic::{AtomicPtr, Ordering},
};

use crate::{
    i915_active_upstream::{i915_active_fini, i915_active_init},
    i915_gem_domain_upstream::i915_gem_object_flush_if_display,
    i915_gem_object_api_upstream::{i915_gem_object_get, i915_gem_object_put},
    i915_gem_object_types_upstream::{DrmI915GemObject, I915Frontbuffer, to_intel_bo},
    intel_context_types_upstream::I915Active,
    intel_context_upstream::{Kref, RcuHead},
    intel_engine_cs_upstream::{AtomicT, Spinlock, WorkStruct},
    linux::{
        gem::DrmGemObject,
        i915::to_i915,
        i915_private::{DrmI915Private, I915GemPrivate},
        locks::{spin_lock, spin_unlock},
        memory::{
            atomic_read, kfree, kmalloc_obj, kref_get, kref_get_unless_zero, kref_init,
            refcount_dec_and_test,
        },
        rcu::{call_rcu, rcu_read_lock, rcu_read_unlock},
    },
};

const GFP_KERNEL: u32 = crate::linux_config::GFP_KERNEL;
const I915_ACTIVE_RETIRE_SLEEPS: c_ulong = 1 << 0;
const ORIGIN_CS: c_int = 1;

#[repr(C)]
struct IntelFrontbuffer {
    display: *mut c_void,
    bits: AtomicT,
    flush_work: WorkStruct,
}

#[repr(C)]
struct I915FrontbufferLayout {
    base: IntelFrontbuffer,
    obj: *mut DrmI915GemObject,
    write: I915Active,
    rcu: RcuHead,
    ref_: Kref,
}

#[repr(C)]
pub struct IntelDisplayFrontbufferInterface {
    pub get: Option<unsafe extern "C" fn(*mut DrmGemObject) -> *mut IntelFrontbuffer>,
    pub ref_: Option<unsafe extern "C" fn(*mut IntelFrontbuffer)>,
    pub put: Option<unsafe extern "C" fn(*mut IntelFrontbuffer)>,
    pub flush_for_display: Option<unsafe extern "C" fn(*mut IntelFrontbuffer)>,
}

const _: [(); 48] = [(); size_of::<IntelFrontbuffer>()];
const _: [(); 0] = [(); offset_of!(I915FrontbufferLayout, base)];
const _: [(); 32] = [(); size_of::<I915GemPrivate>()];

unsafe extern "C" {
    // The display owner supplies the real frontbuffer tracking implementation.
    // These are not local stubs: display notifications must not be silently lost.
    fn intel_frontbuffer_init(front: *mut IntelFrontbuffer, drm: *mut c_void);
    fn intel_frontbuffer_fini(front: *mut IntelFrontbuffer);
    fn __intel_frontbuffer_flush(front: *mut IntelFrontbuffer, origin: c_int, bits: u32);
    fn __intel_frontbuffer_invalidate(front: *mut IntelFrontbuffer, origin: c_int, bits: u32);
}

#[inline]
unsafe fn frontbuffer_lock(i915: *mut DrmI915Private) -> *mut Spinlock {
    unsafe {
        i915.cast::<u8>()
            .add(offset_of!(DrmI915Private, gem) + size_of::<I915GemPrivate>())
            .cast::<Spinlock>()
    }
}

#[inline]
unsafe fn frontbuffer_deref_rcu(obj: *mut DrmI915GemObject) -> *mut I915FrontbufferLayout {
    let slot =
        unsafe { ptr::addr_of_mut!((*obj).frontbuffer).cast::<*mut I915FrontbufferLayout>() };
    unsafe { AtomicPtr::from_ptr(slot) }.load(Ordering::Acquire)
}

/// Header-inline `i915_gem_object_frontbuffer_lookup()` from
/// `i915_gem_object_frontbuffer.h`.
unsafe fn i915_gem_object_frontbuffer_lookup(
    obj: *mut DrmI915GemObject,
) -> *mut I915FrontbufferLayout {
    if unsafe { frontbuffer_deref_rcu(obj) }.is_null() {
        return ptr::null_mut();
    }

    rcu_read_lock();
    let front = loop {
        let front = unsafe { frontbuffer_deref_rcu(obj) };
        if front.is_null() {
            break front;
        }
        if !kref_get_unless_zero(unsafe { &mut (*front).ref_ }) {
            continue;
        }
        if front == unsafe { frontbuffer_deref_rcu(obj) } {
            break front;
        }
        unsafe { i915_gem_object_frontbuffer_put(front.cast::<I915Frontbuffer>()) };
    };
    rcu_read_unlock();
    front
}

/// Header-inline `intel_frontbuffer_flush()` from `display/intel_frontbuffer.h`.
#[inline]
unsafe fn intel_frontbuffer_flush(front: *mut IntelFrontbuffer, origin: c_int) {
    if front.is_null() {
        return;
    }
    let bits = atomic_read(unsafe { &(*front).bits }) as u32;
    if bits != 0 {
        unsafe { __intel_frontbuffer_flush(front, origin, bits) };
    }
}

/// Header-inline `intel_frontbuffer_invalidate()` from
/// `display/intel_frontbuffer.h`.
#[inline]
unsafe fn intel_frontbuffer_invalidate(front: *mut IntelFrontbuffer, origin: c_int) {
    if front.is_null() {
        return;
    }
    let bits = atomic_read(unsafe { &(*front).bits }) as u32;
    if bits != 0 {
        unsafe { __intel_frontbuffer_invalidate(front, origin, bits) };
    }
}

unsafe extern "C" fn frontbuffer_free_rcu(head: *mut RcuHead) {
    let front = container_of!(head, I915FrontbufferLayout, rcu);
    unsafe { kfree(front) };
}

unsafe fn kref_put_lock(
    reference: *mut Kref,
    lock: *mut Spinlock,
    release: unsafe extern "C" fn(*mut Kref),
) {
    if unsafe { refcount_dec_and_test(&mut (*reference).refcount) } {
        unsafe { spin_lock(&mut *lock) };
        unsafe { release(reference) };
    }
}

// upstream: gem/i915_gem_object_frontbuffer.c frontbuffer_active()
unsafe extern "C" fn frontbuffer_active(active: *mut I915Active) -> c_int {
    let front = container_of!(active, I915FrontbufferLayout, write);
    unsafe { kref_get(&mut (*front).ref_) };
    0
}

// upstream: gem/i915_gem_object_frontbuffer.c frontbuffer_retire()
unsafe extern "C" fn frontbuffer_retire(active: *mut I915Active) {
    let front = container_of!(active, I915FrontbufferLayout, write);
    unsafe { intel_frontbuffer_flush(&mut (*front).base, ORIGIN_CS) };
    unsafe { i915_gem_object_frontbuffer_put(front.cast::<I915Frontbuffer>()) };
}

// upstream: gem/i915_gem_object_frontbuffer.c i915_gem_object_frontbuffer_get()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_object_frontbuffer_get(
    obj: *mut DrmI915GemObject,
) -> *mut I915Frontbuffer {
    let gem = unsafe { crate::i915_gem_object_types_upstream::intel_bo_to_drm_bo(obj) };
    let i915 = unsafe { to_i915((*gem).dev) };
    let front = unsafe { i915_gem_object_frontbuffer_lookup(obj) };
    if !front.is_null() {
        return front.cast::<I915Frontbuffer>();
    }

    let front = kmalloc_obj::<I915FrontbufferLayout>(GFP_KERNEL);
    if front.is_null() {
        return ptr::null_mut();
    }

    unsafe { intel_frontbuffer_init(&mut (*front).base, ptr::addr_of_mut!((*i915).drm).cast()) };
    unsafe { kref_init(&mut (*front).ref_) };
    unsafe { i915_gem_object_get(obj) };
    unsafe { (*front).obj = obj };
    unsafe {
        i915_active_init(
            &mut (*front).write,
            frontbuffer_active,
            frontbuffer_retire,
            I915_ACTIVE_RETIRE_SLEEPS,
        )
    };

    let lock = unsafe { frontbuffer_lock(i915) };
    unsafe { spin_lock(&mut *lock) };
    let current = unsafe { frontbuffer_deref_rcu(obj) };
    let cur = if !current.is_null() {
        unsafe { kref_get(&mut (*current).ref_) };
        current
    } else {
        rcu_assign_pointer!(&mut (*obj).frontbuffer, front.cast::<I915Frontbuffer>());
        front
    };
    unsafe { spin_unlock(&mut *lock) };

    if cur != front {
        unsafe { i915_gem_object_put(obj) };
        unsafe { intel_frontbuffer_fini(&mut (*front).base) };
        unsafe { kfree(front) };
    }
    cur.cast::<I915Frontbuffer>()
}

// upstream: gem/i915_gem_object_frontbuffer.c i915_gem_object_frontbuffer_ref()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_object_frontbuffer_ref(front: *mut I915Frontbuffer) {
    unsafe { kref_get(&mut (*front.cast::<I915FrontbufferLayout>()).ref_) };
}

// upstream: gem/i915_gem_object_frontbuffer.c frontbuffer_release()
unsafe extern "C" fn frontbuffer_release(reference: *mut Kref) {
    let front = container_of!(reference, I915FrontbufferLayout, ref_);
    let obj = unsafe { (*front).obj };
    let gem = unsafe { crate::i915_gem_object_types_upstream::intel_bo_to_drm_bo(obj) };
    let i915 = unsafe { to_i915((*gem).dev) };

    unsafe { crate::i915_vma_api_upstream::i915_ggtt_clear_scanout(obj) };
    unsafe {
        AtomicPtr::from_ptr(
            ptr::addr_of_mut!((*obj).frontbuffer).cast::<*mut I915FrontbufferLayout>(),
        )
        .store(ptr::null_mut(), Ordering::Release);
    }
    unsafe { spin_unlock(&mut *frontbuffer_lock(i915)) };

    unsafe { i915_active_fini(&mut (*front).write) };
    unsafe { i915_gem_object_put(obj) };
    unsafe { intel_frontbuffer_fini(&mut (*front).base) };
    call_rcu(&mut (*front).rcu, frontbuffer_free_rcu);
}

// upstream: gem/i915_gem_object_frontbuffer.c i915_gem_object_frontbuffer_put()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_object_frontbuffer_put(front: *mut I915Frontbuffer) {
    let front = front.cast::<I915FrontbufferLayout>();
    let obj = unsafe { (*front).obj };
    let gem = unsafe { crate::i915_gem_object_types_upstream::intel_bo_to_drm_bo(obj) };
    let i915 = unsafe { to_i915((*gem).dev) };
    unsafe {
        kref_put_lock(
            &mut (*front).ref_,
            frontbuffer_lock(i915),
            frontbuffer_release,
        )
    };
}

// upstream: gem/i915_gem_object_frontbuffer.c __i915_gem_object_frontbuffer_flush()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __i915_gem_object_frontbuffer_flush(
    obj: *mut DrmI915GemObject,
    origin: c_int,
) {
    let front = unsafe { i915_gem_object_frontbuffer_lookup(obj) };
    if !front.is_null() {
        unsafe { intel_frontbuffer_flush(&mut (*front).base, origin) };
        unsafe { i915_gem_object_frontbuffer_put(front.cast()) };
    }
}

// upstream: gem/i915_gem_object_frontbuffer.c __i915_gem_object_frontbuffer_invalidate()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __i915_gem_object_frontbuffer_invalidate(
    obj: *mut DrmI915GemObject,
    origin: c_int,
) {
    let front = unsafe { i915_gem_object_frontbuffer_lookup(obj) };
    if !front.is_null() {
        unsafe { intel_frontbuffer_invalidate(&mut (*front).base, origin) };
        unsafe { i915_gem_object_frontbuffer_put(front.cast()) };
    }
}

// upstream: gem/i915_gem_object_frontbuffer.c i915_frontbuffer_get()
unsafe extern "C" fn i915_frontbuffer_get(obj: *mut DrmGemObject) -> *mut IntelFrontbuffer {
    let obj = unsafe { to_intel_bo(obj) };
    let front = unsafe { i915_gem_object_frontbuffer_get(obj) };
    if front.is_null() {
        ptr::null_mut()
    } else {
        unsafe { ptr::addr_of_mut!((*front.cast::<I915FrontbufferLayout>()).base) }
    }
}

// upstream: gem/i915_gem_object_frontbuffer.c i915_frontbuffer_ref()
unsafe extern "C" fn i915_frontbuffer_ref(front: *mut IntelFrontbuffer) {
    let front = container_of!(front, I915FrontbufferLayout, base);
    unsafe { i915_gem_object_frontbuffer_ref(front.cast::<I915Frontbuffer>()) };
}

// upstream: gem/i915_gem_object_frontbuffer.c i915_frontbuffer_put()
unsafe extern "C" fn i915_frontbuffer_put(front: *mut IntelFrontbuffer) {
    let front = container_of!(front, I915FrontbufferLayout, base);
    unsafe { i915_gem_object_frontbuffer_put(front.cast::<I915Frontbuffer>()) };
}

// upstream: gem/i915_gem_object_frontbuffer.c i915_frontbuffer_flush_for_display()
unsafe extern "C" fn i915_frontbuffer_flush_for_display(front: *mut IntelFrontbuffer) {
    let front = container_of!(front, I915FrontbufferLayout, base);
    unsafe { i915_gem_object_flush_if_display((*front).obj) };
}

/// i915-owned callbacks exported to the actual display parent; display-side
/// tracking and notifications remain the display owner's responsibility.
#[unsafe(no_mangle)]
pub static i915_display_frontbuffer_interface: IntelDisplayFrontbufferInterface =
    IntelDisplayFrontbufferInterface {
        get: Some(i915_frontbuffer_get),
        ref_: Some(i915_frontbuffer_ref),
        put: Some(i915_frontbuffer_put),
        flush_for_display: Some(i915_frontbuffer_flush_for_display),
    };
