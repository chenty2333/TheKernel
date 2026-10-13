// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
// Linux v7.2.3 wait/wound and wait/die mutex support (kernel/locking/ww_mutex.h,
// kernel/locking/mutex.c __ww_mutex_lock). Two class kinds exist, as in Linux:
// `RESERVATION_WW_CLASS` is DEFINE_WD_CLASS (drivers/dma-buf/dma-resv.c, used by
// the i915 GEM ww paths) and `CRTC_WW_CLASS` is DEFINE_WW_CLASS
// (drivers/gpu/drm/drm_modeset_lock.c).
#![allow(unsafe_code)]

use core::{
    ffi::c_void,
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU16, AtomicU64, Ordering},
};
use core::{ptr::null_mut, time::Duration};
use kernel_guard::NoPreemptIrqSave;

use crate::{
    mutex::{mutex_lock, mutex_trylock, mutex_unlock, mutex_wait_queue},
    types::Mutex,
    types::WwAcquireCtx,
};

/// `struct ww_mutex` with the target's non-RT, non-debug x86_64 layout.
#[repr(C)]
pub struct WwMutex {
    pub base: Mutex,
    pub ctx: *mut WwAcquireCtx,
}

/// `struct ww_class`: a stamp counter plus the wait-die flag.
#[repr(C)]
pub struct WwClass {
    stamp: AtomicU64,
    is_wait_die: u16,
}

/// `DEFINE_WD_CLASS(reservation_ww_class)` from drivers/dma-buf/dma-resv.c.
pub static RESERVATION_WW_CLASS: WwClass = WwClass {
    stamp: AtomicU64::new(0),
    is_wait_die: 1,
};

/// `DEFINE_WW_CLASS(crtc_ww_class)` from drivers/gpu/drm/drm_modeset_lock.c.
pub static CRTC_WW_CLASS: WwClass = WwClass {
    stamp: AtomicU64::new(0),
    is_wait_die: 0,
};

/// Linux `ww_mutex_lock()` waiters are kept on a per-lock list. The kernel
/// only needs the contexts that are currently blocked, so they are recorded in
/// an inline table plus dynamic overflow guarded by the metadata lock below.
const WW_MAX_WAITERS: usize = 128;

struct WwWaiterSlot {
    lock: AtomicPtr<WwMutex>,
    ctx: AtomicPtr<WwAcquireCtx>,
}

static WW_WAITERS: [WwWaiterSlot; WW_MAX_WAITERS] = [const {
    WwWaiterSlot {
        lock: AtomicPtr::new(null_mut()),
        ctx: AtomicPtr::new(null_mut()),
    }
}; WW_MAX_WAITERS];
struct WwOverflow(core::cell::UnsafeCell<Option<alloc::vec::Vec<WwWaiterSlot>>>);
unsafe impl Sync for WwOverflow {}
static WW_WAITERS_OVERFLOW: WwOverflow = WwOverflow(core::cell::UnsafeCell::new(None));
static WW_METADATA_LOCKED: AtomicBool = AtomicBool::new(false);

const _: [(); 32] = [(); core::mem::size_of::<WwMutex>()];
const _: [(); 0] = [(); core::mem::offset_of!(WwMutex, base)];
const _: [(); 24] = [(); core::mem::offset_of!(WwMutex, ctx)];

// Linux keeps `wait_lock` and the waiter list under the mutex's wait_lock; the
// Rust mutex has no wait_lock, so one short spin lock covers the ww metadata
// (`ww->ctx`, `ctx->acquired`, `ctx->wounded` updates and the waiter table).
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

fn wake_waiters(lock: *mut WwMutex) {
    if !lock.is_null() {
        crate::mutex::mutex_wait_queue_for(unsafe { core::ptr::addr_of_mut!((*lock).base) }).notify_all(false);
    } else {
        crate::mutex::mutex_wait_queue().notify_all(false);
    }
}

/// Linux `__ww_ctx_less(a, b)`: `a` is younger than `b` (wrap-aware stamp order).
fn ctx_younger(a: *mut WwAcquireCtx, b: *mut WwAcquireCtx) -> bool {
    (unsafe { (*a).stamp }.wrapping_sub(unsafe { (*b).stamp }) as i64) > 0
}

fn is_wounded(ctx: *mut WwAcquireCtx) -> bool {
    unsafe { AtomicU16::from_ptr(core::ptr::addr_of_mut!((*ctx).wounded)) }.load(Ordering::Acquire)
        != 0
}

fn is_wait_die(ctx: *mut WwAcquireCtx) -> bool {
    unsafe { (*ctx).is_wait_die != 0 }
}

/// Context pointers of the waiters currently blocked on `lock`.
/// Caller holds the metadata lock.
fn waiters_on(lock: *mut WwMutex) -> alloc::vec::Vec<*mut WwAcquireCtx> {
    let mut list = alloc::vec::Vec::new();
    for slot in WW_WAITERS.iter() {
        if slot.lock.load(Ordering::Acquire) == lock {
            let ctx = slot.ctx.load(Ordering::Acquire);
            if !ctx.is_null() {
                list.push(ctx);
            }
        }
    }
    unsafe {
        let overflow_ref = &*WW_WAITERS_OVERFLOW.0.get();
        if let Some(overflow) = overflow_ref.as_ref() {
            for slot in overflow.iter() {
                if slot.lock.load(Ordering::Acquire) == lock {
                    let ctx = slot.ctx.load(Ordering::Acquire);
                    if !ctx.is_null() {
                        list.push(ctx);
                    }
                }
            }
        }
    }
    list
}

/// Caller holds the metadata lock.
fn register_waiter(lock: *mut WwMutex, ctx: *mut WwAcquireCtx) {
    for slot in WW_WAITERS.iter() {
        if slot.ctx.load(Ordering::Acquire).is_null() {
            slot.lock.store(lock, Ordering::Release);
            slot.ctx.store(ctx, Ordering::Release);
            return;
        }
    }
    unsafe {
        let overflow_mut = &mut *WW_WAITERS_OVERFLOW.0.get();
        let overflow = overflow_mut.get_or_insert_with(alloc::vec::Vec::new);
        overflow.push(WwWaiterSlot {
            lock: AtomicPtr::new(lock),
            ctx: AtomicPtr::new(ctx),
        });
    }
}

/// Caller holds the metadata lock.
fn unregister_waiter(ctx: *mut WwAcquireCtx) {
    for slot in WW_WAITERS.iter() {
        if slot.ctx.load(Ordering::Acquire) == ctx {
            slot.ctx.store(null_mut(), Ordering::Release);
            slot.lock.store(null_mut(), Ordering::Release);
            return;
        }
    }
    unsafe {
        let overflow_mut = &mut *WW_WAITERS_OVERFLOW.0.get();
        if let Some(overflow) = overflow_mut.as_mut() {
            if let Some(pos) = overflow.iter().position(|s| s.ctx.load(Ordering::Acquire) == ctx) {
                overflow.swap_remove(pos);
            }
        }
    }
}

/// Linux `__ww_mutex_check_kill()` as a predicate: true when `ctx` must back
/// off with -EDEADLK. Caller holds the metadata lock.
///
/// Wound-wait: `ctx` was wounded. Wait-die: `ctx` is younger than the current
/// holder or than a context already waiting on the lock. A context that holds
/// nothing never dies (`ctx->acquired == 0`).
fn must_die(lock: *mut WwMutex, ctx: *mut WwAcquireCtx) -> bool {
    if unsafe { (*ctx).acquired } == 0 {
        return false;
    }
    if !is_wait_die(ctx) {
        return is_wounded(ctx);
    }
    let hold = unsafe { (*lock).ctx };
    if !hold.is_null() && hold != ctx && ctx_younger(ctx, hold) {
        return true;
    }
    waiters_on(lock).iter().any(|&waiter| waiter != ctx && ctx_younger(ctx, waiter))
}

/// Initialize a WW acquire context for `class` (Linux `ww_acquire_init`).
pub unsafe fn ww_acquire_init(ctx: *mut WwAcquireCtx, class: &'static WwClass) {
    assert!(!ctx.is_null());
    let task = axhal::percpu::current_task_ptr::<()>();
    assert!(!task.is_null(), "ww_acquire_init requires a current task");
    unsafe {
        (*ctx).task = task as *mut c_void;
        (*ctx).stamp = class.stamp.fetch_add(1, Ordering::Relaxed) + 1;
        (*ctx).acquired = 0;
        (*ctx).wounded = 0;
        (*ctx).is_wait_die = class.is_wait_die;
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

/// Linux `ww_mutex_lock_acquired()` plus `ww_mutex_set_context_fastpath()`'s
/// waiter check: record ownership, then wound (wound-wait) or wake (wait-die)
/// the contexts already blocked on this lock.
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
        // __ww_mutex_check_waiters(): a waiter older than us that holds other
        // locks wounds us (wound-wait). Wait-die waiters that are now younger
        // than us re-run their predicate after the wake below.
        if !is_wait_die(ctx) {
            let wounds = waiters_on(lock).iter().any(|&waiter| {
                (unsafe { (*waiter).acquired }) > 0 && ctx_younger(ctx, waiter)
            });
            if wounds {
                unsafe { AtomicU16::from_ptr(core::ptr::addr_of_mut!((*ctx).wounded)) }
                    .store(1, Ordering::Release);
            }
        }
    }
    metadata_unlock(metadata_guard);
    wake_waiters(lock);
}

/// Uncontended or contended ww acquisition with `ctx == NULL` is a plain
/// `mutex_lock()`; with a context the Linux `__mutex_lock_common()` flow runs.
pub unsafe fn ww_mutex_lock(lock: *mut WwMutex, ctx: *mut WwAcquireCtx) -> i32 {
    unsafe { ww_mutex_lock_inner(lock, ctx, false) }
}

/// Interruptible wound/wait acquisition; `-EINTR` is returned when a signal
/// interrupts the sleep, leaving the lock unacquired.
unsafe fn ww_mutex_lock_inner(lock: *mut WwMutex, ctx: *mut WwAcquireCtx, interruptible: bool) -> i32 {
    assert!(!lock.is_null());
    if ctx.is_null() {
        if interruptible {
            return unsafe {
                crate::mutex::mutex_lock_interruptible_impl(core::ptr::addr_of_mut!((*lock).base))
            };
        }
        unsafe { mutex_lock(core::ptr::addr_of_mut!((*lock).base)) };
        return 0;
    }
    // Linux __mutex_lock_common(): the EALREADY check precedes the wounded reset.
    if unsafe { (*lock).ctx } == ctx {
        return -crate::config::EALREADY;
    }
    if unsafe { (*ctx).acquired } == 0 {
        unsafe { (*ctx).wounded = 0 };
    }
    if unsafe { mutex_trylock(core::ptr::addr_of_mut!((*lock).base)) } {
        unsafe { publish_acquisition(lock, ctx) };
        return 0;
    }

    // Linux add_waiter phase, then the first __ww_mutex_check_kill().
    let metadata_guard = metadata_lock();
    if !is_wait_die(ctx) {
        // Wound-wait: an older context wounds a younger holder when we hold locks.
        let hold = unsafe { (*lock).ctx };
        if !hold.is_null() && hold != ctx && unsafe { (*ctx).acquired } > 0 && ctx_younger(hold, ctx) {
            unsafe { AtomicU16::from_ptr(core::ptr::addr_of_mut!((*hold).wounded)) }
                .store(1, Ordering::Release);
        }
    }
    register_waiter(lock, ctx);
    if must_die(lock, ctx) {
        unregister_waiter(ctx);
        metadata_unlock(metadata_guard);
        wake_waiters(lock);
        return -crate::config::EDEADLK;
    }
    metadata_unlock(metadata_guard);
    wake_waiters(lock);

    let wq = crate::mutex::mutex_wait_queue_for(core::ptr::addr_of_mut!((*lock).base));
    loop {
        // Linux re-runs the trylock before any kill check, so a lock that was
        // just released is taken even by a wounded context.
        if unsafe { mutex_trylock(core::ptr::addr_of_mut!((*lock).base)) } {
            break;
        }
        let dies = || {
            let guard = metadata_lock();
            let dead = must_die(lock, ctx);
            metadata_unlock(guard);
            dead
        };
        if interruptible {
            if crate::signal::signal_pending_current() {
                let guard = metadata_lock();
                unregister_waiter(ctx);
                metadata_unlock(guard);
                wake_waiters(lock);
                return -crate::config::EINTR;
            }
            let _ = wq.wait_timeout_until(Duration::from_millis(10), || {
                crate::signal::signal_pending_current() || dies() || unsafe { lock_is_free(lock) }
            });
        } else {
            wq
                .wait_until(|| dies() || unsafe { lock_is_free(lock) })
                .expect("ww_mutex lost its task wait context");
        }
        if !unsafe { lock_is_free(lock) } && dies() {
            let guard = metadata_lock();
            unregister_waiter(ctx);
            metadata_unlock(guard);
            wake_waiters(lock);
            return -crate::config::EDEADLK;
        }
    }

    let guard = metadata_lock();
    unregister_waiter(ctx);
    metadata_unlock(guard);
    unsafe { publish_acquisition(lock, ctx) };
    0
}

/// True when the embedded Linux mutex has no owner.
unsafe fn lock_is_free(lock: *mut WwMutex) -> bool {
    !unsafe { crate::mutex::mutex_is_locked(core::ptr::addr_of_mut!((*lock).base)) }
}

/// Linux `ww_mutex_is_locked()`: the embedded base mutex is held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ww_mutex_is_locked(lock: *mut WwMutex) -> bool {
    unsafe { crate::mutex::mutex_is_locked(core::ptr::addr_of_mut!((*lock).base)) }
}

/// Slow reacquisition after the caller has dropped all other WW locks.
/// Linux: `ww_mutex_lock(lock, ctx)` with `acquired == 0`, so it cannot fail.
pub unsafe fn ww_mutex_lock_slow(lock: *mut WwMutex, ctx: *mut WwAcquireCtx) {
    let ret = unsafe { ww_mutex_lock_inner(lock, ctx, false) };
    assert_eq!(ret, 0, "ww_mutex_lock_slow with held locks");
}

/// Nonblocking WW acquire.
pub unsafe fn ww_mutex_trylock(lock: *mut WwMutex, ctx: *mut WwAcquireCtx) -> bool {
    if lock.is_null() {
        return false;
    }
    if ctx.is_null() {
        return unsafe { mutex_trylock(core::ptr::addr_of_mut!((*lock).base)) };
    }
    if unsafe { (*ctx).acquired } == 0 {
        unsafe { (*ctx).wounded = 0 };
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

/// Linux `ww_mutex_lock_interruptible()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ww_mutex_lock_interruptible(lock: *mut c_void, ctx: *mut c_void) -> i32 {
    unsafe { ww_mutex_lock_inner(lock.cast::<WwMutex>(), ctx.cast::<WwAcquireCtx>(), true) }
}

/// Linux `ww_mutex_unlock()`; the Rust helper keeps its identifier.
#[unsafe(export_name = "ww_mutex_unlock")]
pub unsafe extern "C" fn c_ww_mutex_unlock(lock: *mut c_void) {
    unsafe { ww_mutex_unlock(lock.cast::<WwMutex>()) };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(stamp: u64, acquired: u32, wait_die: u16) -> WwAcquireCtx {
        WwAcquireCtx {
            task: core::ptr::null_mut(),
            stamp,
            acquired,
            wounded: 0,
            is_wait_die: wait_die,
        }
    }

    fn lock_held_by(holder: *mut WwAcquireCtx) -> WwMutex {
        WwMutex {
            // SAFETY: an all-zero Linux mutex is the unlocked state.
            base: unsafe { core::mem::zeroed() },
            ctx: holder,
        }
    }

    #[test]
    fn wait_die_younger_context_dies_against_older_holder() {
        let mut old = ctx(1, 1, 1);
        let mut young = ctx(2, 1, 1);
        let mut lock = lock_held_by(&mut old);
        assert!(must_die(&mut lock, &mut young));
        assert!(!must_die(&mut lock, &mut old));
    }

    #[test]
    fn wait_die_context_holding_nothing_never_dies() {
        let mut old = ctx(1, 1, 1);
        let mut young = ctx(2, 0, 1);
        let mut lock = lock_held_by(&mut old);
        assert!(!must_die(&mut lock, &mut young));
    }

    #[test]
    fn wait_die_younger_context_dies_against_older_waiter() {
        let mut holder = ctx(1, 0, 1);
        let mut old_waiter = ctx(2, 1, 1);
        let mut young = ctx(3, 1, 1);
        let mut lock = lock_held_by(core::ptr::null_mut());
        let lock_ptr: *mut WwMutex = &mut lock;
        let _ = &mut holder;
        register_waiter(lock_ptr, &mut old_waiter);
        assert!(must_die(lock_ptr, &mut young));
        assert!(!must_die(lock_ptr, &mut old_waiter));
        unregister_waiter(&mut old_waiter);
        assert!(!must_die(lock_ptr, &mut young));
    }

    #[test]
    fn wound_wait_dies_only_when_wounded_and_holding() {
        let mut holder = ctx(2, 1, 0);
        let mut lock = lock_held_by(core::ptr::null_mut());
        let lock_ptr: *mut WwMutex = &mut lock;
        assert!(!must_die(lock_ptr, &mut holder));
        holder.wounded = 1;
        assert!(must_die(lock_ptr, &mut holder));
        let mut empty = ctx(3, 0, 0);
        empty.wounded = 1;
        assert!(!must_die(lock_ptr, &mut empty));
    }

    #[test]
    fn reservation_and_crtc_classes_have_linux_kinds() {
        assert_eq!(RESERVATION_WW_CLASS.is_wait_die, 1);
        assert_eq!(CRTC_WW_CLASS.is_wait_die, 0);
    }

    #[test]
    fn test_ww_mutex_waiters_overflow() {
        let mut lock = lock_held_by(core::ptr::null_mut());
        let lock_ptr: *mut WwMutex = &mut lock;
        let mut ctxs: alloc::vec::Vec<WwAcquireCtx> = alloc::vec::Vec::new();
        for i in 0..140 {
            ctxs.push(ctx(i as u64 + 10, 1, 1));
        }
        for c in ctxs.iter_mut() {
            register_waiter(lock_ptr, c);
        }
        let list = waiters_on(lock_ptr);
        assert_eq!(list.len(), 140);
        for c in ctxs.iter_mut() {
            unregister_waiter(c);
        }
        let list_after = waiters_on(lock_ptr);
        assert_eq!(list_after.len(), 0);
    }
}
