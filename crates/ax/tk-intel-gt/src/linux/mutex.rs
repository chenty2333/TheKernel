// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Sleepable Linux mutex operations for the source kernel's x86_64/SMP
//! layout (`owner` at offset 0). The lock word stores the current task pointer
//! just like `mutex.owner`; contended callers sleep on a shared wait queue and
//! always recheck the particular mutex before acquiring it.

#![allow(unsafe_code)]

use core::{
    mem::size_of,
    sync::atomic::{AtomicUsize, Ordering},
};

use crate::{intel_engine_cs_upstream::Mutex, linux_config::CONFIG_DEBUG_MUTEXES};

static MUTEX_WAITERS: axtask::WaitQueue = axtask::WaitQueue::new();

const _: [(); 24] = [(); size_of::<Mutex>()];
const OWNER_OFFSET: usize = 0;
const _: [(); 0] = [(); OWNER_OFFSET];
const _: [(); 8] = [(); core::mem::align_of::<Mutex>()];

/// Linux `mutex_destroy()` is compiled away when DEBUG_MUTEXES is disabled.
#[inline]
pub fn mutex_destroy(_mutex: &mut Mutex) {
    if CONFIG_DEBUG_MUTEXES {
        panic!("DEBUG_MUTEXES requires the Linux mutex debugging backend");
    }
}

#[inline]
unsafe fn owner_word<'a>(mutex: *mut Mutex) -> &'a AtomicUsize {
    assert!(!mutex.is_null());
    // Linux v7.2.3 include/linux/mutex_types.h places atomic_long_t owner at
    // byte offset zero. Mutex is x86_64 c_ulong-sized and naturally aligned.
    let owner = unsafe { (mutex.cast::<u8>().add(OWNER_OFFSET)).cast::<usize>() };
    // SAFETY: the C layout and exclusive atomic-access contract above match
    // `atomic_long_t` storage.
    unsafe { AtomicUsize::from_ptr(owner) }
}

pub trait MutexPointer {
    fn mutex_ptr(self) -> *mut Mutex;
}

impl MutexPointer for *mut Mutex {
    fn mutex_ptr(self) -> *mut Mutex {
        self
    }
}

impl MutexPointer for *const Mutex {
    fn mutex_ptr(self) -> *mut Mutex {
        self.cast_mut()
    }
}

impl MutexPointer for &mut Mutex {
    fn mutex_ptr(self) -> *mut Mutex {
        self
    }
}

impl MutexPointer for &Mutex {
    fn mutex_ptr(self) -> *mut Mutex {
        self as *const Mutex as *mut Mutex
    }
}

fn current_owner() -> usize {
    assert!(
        axtask::can_block_current(),
        "Linux mutex operation requires a task context"
    );
    let task = axhal::percpu::current_task_ptr::<()>();
    assert!(!task.is_null(), "Linux mutex has no current task owner");
    task as usize
}

/// Initialize the source C-layout mutex. Caller must have exclusive access
/// and must not reinitialize a held mutex, matching Linux's contract.
pub unsafe fn mutex_init<M: MutexPointer>(mutex: M) {
    let mutex = mutex.mutex_ptr();
    assert!(!mutex.is_null());
    // CONFIG_LOCKDEP, CONFIG_DEBUG_MUTEXES and PREEMPT_RT are disabled; the
    // source mutex's owner, raw wait-lock, osq tail and first-waiter fields
    // therefore initialize to zero/null as in __mutex_init_generic().
    unsafe { core::ptr::write_bytes(mutex.cast::<u8>(), 0, size_of::<Mutex>()) };
}

/// Try to acquire without sleeping, preserving Linux's owner identity.
pub unsafe fn mutex_trylock<M: MutexPointer>(mutex: M) -> bool {
    let task = axhal::percpu::current_task_ptr::<()>();
    if task.is_null() {
        return false;
    }
    let owner = task as usize;
    let mutex = mutex.mutex_ptr();
    unsafe { owner_word(mutex) }
        .compare_exchange(0, owner, Ordering::Acquire, Ordering::Relaxed)
        .is_ok()
}

/// Acquire the mutex, sleeping on contention and rechecking after every wake.
pub unsafe fn mutex_lock<M: MutexPointer>(mutex: M) {
    let task = current_owner();
    let mutex = mutex.mutex_ptr();
    let owner = unsafe { owner_word(mutex) };
    loop {
        match owner.compare_exchange(0, task, Ordering::Acquire, Ordering::Relaxed) {
            Ok(_) => return,
            Err(held_by) if held_by == task => {
                panic!("recursive Linux mutex lock");
            }
            Err(_) => MUTEX_WAITERS
                .wait_until(|| owner.load(Ordering::Acquire) == 0)
                .expect("Linux mutex lost its task wait context"),
        }
    }
}

/// Release the mutex, checking that only its owner unlocks it.
pub unsafe fn mutex_unlock<M: MutexPointer>(mutex: M) {
    let task = current_owner();
    let mutex = mutex.mutex_ptr();
    let owner = unsafe { owner_word(mutex) };
    assert_eq!(
        owner.swap(0, Ordering::Release),
        task,
        "Linux mutex unlocked by a non-owner"
    );
    MUTEX_WAITERS.notify_all(false);
}

#[inline]
pub unsafe fn mutex_is_locked<M: MutexPointer>(mutex: M) -> bool {
    let mutex = mutex.mutex_ptr();
    assert!(!mutex.is_null());
    let owner = unsafe { owner_word(mutex) };
    owner.load(Ordering::Acquire) != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_word_uses_the_first_c_ulong_and_locked_state() {
        let mut mutex = core::mem::MaybeUninit::<Mutex>::zeroed();
        let mutex_ptr = mutex.as_mut_ptr();
        // SAFETY: zeroed source mutex storage and exclusive local ownership.
        unsafe { mutex_init(mutex_ptr) };
        // SAFETY: the owner word is exactly the first field by Linux ABI.
        unsafe { owner_word(mutex_ptr) }.store(7, Ordering::Release);
        // SAFETY: valid local mutex storage.
        assert!(unsafe { mutex_is_locked(mutex_ptr) });
        // SAFETY: valid local mutex storage.
        unsafe { owner_word(mutex_ptr) }.store(0, Ordering::Release);
        // SAFETY: valid local mutex storage.
        assert!(!unsafe { mutex_is_locked(mutex_ptr) });
    }
}

/// Interruptible acquire: sleeps like [`mutex_lock`], and gives up with
/// `-EINTR` when the sleeping task is interrupted by a signal, as
/// `mutex_lock_interruptible()` does.
pub unsafe fn mutex_lock_interruptible_impl<M: MutexPointer>(mutex: M) -> i32 {
    let task = current_owner();
    let mutex = mutex.mutex_ptr();
    let owner = unsafe { owner_word(mutex) };
    loop {
        match owner.compare_exchange(0, task, Ordering::Acquire, Ordering::Relaxed) {
            Ok(_) => return 0,
            Err(held_by) if held_by == task => {
                panic!("recursive Linux mutex lock");
            }
            Err(_) => {
                // Re-check the signal state every tick, since a signal does
                // not notify the mutex waiters.
                if crate::linux::signal::signal_pending_current() {
                    return -crate::linux_config::EINTR;
                }
                let _ = MUTEX_WAITERS.wait_timeout_until(
                    core::time::Duration::from_millis(10),
                    || {
                        owner.load(Ordering::Acquire) == 0
                            || crate::linux::signal::signal_pending_current()
                    },
                );
            }
        }
    }
}

// C ABI names. The Rust helpers above keep their identifiers, so the exported
// symbols are attached with `export_name`.

/// Linux `mutex_unlock()`.
#[unsafe(export_name = "mutex_unlock")]
pub unsafe extern "C" fn c_mutex_unlock(lock: *mut Mutex) {
    unsafe { mutex_unlock(lock) };
}

/// Linux `mutex_destroy()`.
#[unsafe(export_name = "mutex_destroy")]
pub unsafe extern "C" fn c_mutex_destroy(lock: *mut Mutex) {
    mutex_destroy(unsafe { &mut *lock });
}

/// Linux `__mutex_init()`: lockdep and debug names are compiled out.
/// Linux `mutex_init()`: the C macro expands to `__mutex_init()` with the
/// debug name and key compiled out, so only the lock is initialized.
#[unsafe(export_name = "mutex_init")]
pub unsafe extern "C" fn c_mutex_init(lock: *mut core::ffi::c_void) {
    unsafe { mutex_init(lock.cast::<Mutex>()) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __mutex_init(
    lock: *mut Mutex,
    _name: *const u8,
    _key: *mut core::ffi::c_void,
) {
    unsafe { mutex_init(lock) };
}

/// Linux `mutex_lock_interruptible()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mutex_lock_interruptible(lock: *mut Mutex) -> i32 {
    unsafe { mutex_lock_interruptible_impl(lock) }
}

/// Linux `mutex_lock_interruptible_nested()`; the subclass only feeds lockdep.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mutex_lock_interruptible_nested(lock: *mut Mutex, _subclass: u32) -> i32 {
    unsafe { mutex_lock_interruptible_impl(lock) }
}
