// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
// Linux v7.2.3 wound/wait mutex support used by the GEM reservation owner.
#![allow(unsafe_code)]

use core::{
    ffi::c_void,
    sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering},
};
use kernel_guard::NoPreemptIrqSave;

use crate::{
    i915_gem_ww_upstream::WwAcquireCtx,
    intel_engine_cs_upstream::Mutex,
    linux::mutex::{mutex_lock, mutex_trylock, mutex_unlock},
};

/// `struct ww_mutex` with the target's non-RT, non-debug x86_64 layout.
#[repr(C)]
pub struct WwMutex {
    pub base: Mutex,
    pub ctx: *mut WwAcquireCtx,
}

#[repr(C)]
struct WwClass {
    stamp: AtomicU64,
}

static RESERVATION_WW_CLASS: WwClass = WwClass {
    stamp: AtomicU64::new(0),
};
static WW_METADATA_LOCKED: AtomicBool = AtomicBool::new(false);

const _: [(); 32] = [(); core::mem::size_of::<WwMutex>()];
const _: [(); 0] = [(); core::mem::offset_of!(WwMutex, base)];
const _: [(); 24] = [(); core::mem::offset_of!(WwMutex, ctx)];

fn metadata_lock() -> NoPreemptIrqSave {
    let guard = NoPreemptIrqSave::new();
    while WW_METADATA_LOCKED
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    guard
}

fn metadata_unlock(guard: NoPreemptIrqSave) {
    WW_METADATA_LOCKED.store(false, Ordering::Release);
    drop(guard);
}

fn is_wounded(ctx: *mut WwAcquireCtx) -> bool {
    !ctx.is_null()
        && unsafe { AtomicU16::from_ptr(core::ptr::addr_of_mut!((*ctx).wounded)) }
            .load(Ordering::Acquire)
            != 0
}

/// Initialize a WW acquire context from the reservation class.
pub unsafe fn ww_acquire_init(ctx: *mut WwAcquireCtx) {
    assert!(!ctx.is_null());
    let task = axhal::percpu::current_task_ptr::<()>();
    assert!(!task.is_null(), "ww_acquire_init requires a current task");
    unsafe {
        (*ctx).task = task as *mut c_void;
        (*ctx).stamp = RESERVATION_WW_CLASS.stamp.fetch_add(1, Ordering::Relaxed) + 1;
        (*ctx).acquired = 0;
        (*ctx).wounded = 0;
        (*ctx).is_wait_die = 0;
    }
}

/// Mark the end of the acquire phase. The configured Linux build has
/// DEBUG_WW_MUTEXES and lockdep disabled, so the upstream inline is a no-op.
pub unsafe fn ww_acquire_done(ctx: *mut WwAcquireCtx) {
    assert!(!ctx.is_null());
}

/// Finish after all locks held by the context were released.
pub unsafe fn ww_acquire_fini(ctx: *mut WwAcquireCtx) {
    assert!(!ctx.is_null());
    assert_eq!(
        unsafe { (*ctx).acquired },
        0,
        "ww context finalized with held locks"
    );
}

unsafe fn publish_acquisition(lock: *mut WwMutex, ctx: *mut WwAcquireCtx) {
    let metadata_guard = metadata_lock();
    unsafe { (*lock).ctx = ctx };
    if !ctx.is_null() {
        unsafe {
            (*ctx).acquired = (*ctx)
                .acquired
                .checked_add(1)
                .expect("ww acquired counter overflow")
        };
    }
    metadata_unlock(metadata_guard);
}

/// Wound/wait acquisition. Older contexts wound younger owners; younger
/// contexts return `-EDEADLK` so the caller can release and back off.
pub unsafe fn ww_mutex_lock(lock: *mut WwMutex, ctx: *mut WwAcquireCtx) -> i32 {
    assert!(!lock.is_null());
    if is_wounded(ctx) {
        return -crate::linux_config::EDEADLK;
    }
    if unsafe { mutex_trylock(core::ptr::addr_of_mut!((*lock).base)) } {
        unsafe { publish_acquisition(lock, ctx) };
        return 0;
    }

    let metadata_guard = metadata_lock();
    let owner = unsafe { (*lock).ctx };
    if !ctx.is_null() && !owner.is_null() && owner != ctx {
        if unsafe { (*ctx).stamp } > unsafe { (*owner).stamp } {
            metadata_unlock(metadata_guard);
            return -crate::linux_config::EDEADLK;
        }
        unsafe { AtomicU16::from_ptr(core::ptr::addr_of_mut!((*owner).wounded)) }
            .store(1, Ordering::Release);
    }
    if !ctx.is_null() && owner == ctx {
        metadata_unlock(metadata_guard);
        return -crate::linux_config::EALREADY;
    }
    metadata_unlock(metadata_guard);

    unsafe { mutex_lock(core::ptr::addr_of_mut!((*lock).base)) };
    if is_wounded(ctx) {
        unsafe { mutex_unlock(core::ptr::addr_of_mut!((*lock).base)) };
        return -crate::linux_config::EDEADLK;
    }
    unsafe { publish_acquisition(lock, ctx) };
    0
}

/// Slow reacquisition after the caller has dropped all other WW locks.
pub unsafe fn ww_mutex_lock_slow(lock: *mut WwMutex, ctx: *mut WwAcquireCtx) {
    assert!(!lock.is_null());
    assert!(!is_wounded(ctx));
    unsafe { mutex_lock(core::ptr::addr_of_mut!((*lock).base)) };
    unsafe { publish_acquisition(lock, ctx) };
}

/// Nonblocking WW acquire.
pub unsafe fn ww_mutex_trylock(lock: *mut WwMutex, ctx: *mut WwAcquireCtx) -> bool {
    if lock.is_null() || is_wounded(ctx) {
        return false;
    }
    if !unsafe { mutex_trylock(core::ptr::addr_of_mut!((*lock).base)) } {
        return false;
    }
    unsafe { publish_acquisition(lock, ctx) };
    true
}

/// Release the stored context ownership and unlock the embedded Linux mutex.
pub unsafe fn ww_mutex_unlock(lock: *mut WwMutex) {
    assert!(!lock.is_null());
    let metadata_guard = metadata_lock();
    let ctx = unsafe { (*lock).ctx };
    unsafe { (*lock).ctx = core::ptr::null_mut() };
    if !ctx.is_null() {
        let acquired = unsafe { &mut (*ctx).acquired };
        assert!(*acquired > 0, "ww acquired count underflow");
        *acquired -= 1;
    }
    metadata_unlock(metadata_guard);
    unsafe { mutex_unlock(core::ptr::addr_of_mut!((*lock).base)) };
}
