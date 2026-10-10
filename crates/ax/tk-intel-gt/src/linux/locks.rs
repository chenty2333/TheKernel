// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//
//! Runtime bindings for the Linux 7.2 x86 spin-lock API used by the i915 GT
//! translations.
//!
//! Linux source references (v7.2):
//! - `include/linux/spinlock_api_smp.h` (`__raw_spin_lock*` ordering):
//!   <https://github.com/torvalds/linux/blob/v7.2/include/linux/spinlock_api_smp.h>
//! - `arch/x86/include/asm/spinlock_types.h` and
//!   `include/asm-generic/qspinlock_types.h` (the four-byte x86 lock storage):
//!   <https://github.com/torvalds/linux/blob/v7.2/arch/x86/include/asm/spinlock_types.h>
//! - `arch/x86/include/asm/irqflags.h` (save/disable and IF-only restore):
//!   <https://github.com/torvalds/linux/blob/v7.2/arch/x86/include/asm/irqflags.h>
//!
//! The translated `Spinlock` is a four-byte, four-byte-aligned opaque C-layout
//! record in `intel_engine_cs_upstream.rs`, matching the wt-dev x86_64 source
//! kernel layout.  Its bytes hold one native atomic state word here.  The
//! `AtomicU32::from_ptr` API is specifically for operating atomically on an
//! initialized `u32` allocation through a pointer; initialization writes the
//! whole storage before concurrent use, and all subsequent accesses to that
//! word in this module are atomic.  The compile-time layout checks below keep
//! this binding from silently being used with a different lock layout.
//!
//! Preemption and IRQ state are not emulated.  They use the kernel's existing
//! `kernel_guard` runtime: `NoPreempt` acquire/release updates the current
//! task's preemption nesting, while `IrqSave` saves/restores the local IRQ
//! enable state.  `tk-intel-gt` must enable its optional `kernel_guard` dep for
//! this module. On host targets `kernel_guard` maps these guards to `NoOp`, as
//! there are no kernel IRQs or preemptible kernel tasks to control.
//!
//! This module deliberately does not bind Linux `struct mutex`: the current
//! translation models it as an opaque 24-byte C-layout object, while the
//! kernel's sleepable mutex runtime (`tk-axsync`) is a larger task-aware Rust
//! object with a wait-queue lifecycle. Reinterpreting those bytes as a spin
//! lock or allocating a sidecar would not preserve Linux mutex semantics.

#![allow(unsafe_code)]

use core::{
    ffi::c_ulong,
    mem::{align_of, size_of},
    sync::atomic::{AtomicU32, Ordering},
};

use kernel_guard::{BaseGuard, IrqSave, NoPreempt};

use crate::{intel_context_upstream::PinCookie, intel_engine_cs_upstream::Spinlock};

/// `lockdep_pin_lock()` is a no-op in the target's CONFIG_LOCKDEP=n build.
#[inline]
pub fn lockdep_pin_lock<T>(_lock: &mut T) -> PinCookie {
    PinCookie
}

/// `lockdep_unpin_lock()` is a no-op in the target's CONFIG_LOCKDEP=n build.
#[inline]
pub fn lockdep_unpin_lock<T>(_lock: &mut T, _cookie: PinCookie) {}

/// `lockdep_set_class_and_name()` compiles away with the source CONFIG_LOCKDEP=n.
#[inline]
pub fn lockdep_set_class_and_name<L, K, N>(_lock: *mut L, _key: *const K, _name: *const N) {}

#[cfg(all(target_os = "none", not(target_arch = "x86_64")))]
compile_error!("the translated i915 GT spinlock layout is the wt-dev x86_64 layout");

// `Spinlock` is the x86_64 LinuxKPI raw lock byte-for-byte layout: opaque [u8;
// 4] with `repr(C, align(4))`. The Linux v7.2 x86 raw lock is a qspinlock
// storage word (`arch/x86/include/asm/spinlock_types.h` includes
// `asm-generic/qspinlock_types.h`). This compatibility implementation uses
// that exact word as a simple acquire/release lock; no extra lock state is
// appended, so enclosing C offsets remain unchanged.
const _: [(); size_of::<AtomicU32>()] = [(); size_of::<Spinlock>()];
const _: [(); align_of::<AtomicU32>()] = [(); align_of::<Spinlock>()];

/// Convert the saved IF state to Linux's `unsigned long` flag storage.
///
/// The Linux x86 restore path tests only `X86_EFLAGS_IF`; the other saved
/// RFLAGS bits are not restored by `arch_local_irq_restore`, so the kernel
/// guard's IF-only token is sufficient and preserves the enabled/disabled
/// state exactly.
#[inline]
fn irq_save_flags() -> c_ulong {
    #[cfg(target_os = "none")]
    {
        IrqSave::acquire() as c_ulong
    }
    #[cfg(not(target_os = "none"))]
    {
        // In host tests there is no privileged IRQ state. `IrqSave` is the
        // kernel_guard `NoOp` alias on these targets.
        IrqSave::acquire();
        0
    }
}

#[inline]
fn irq_restore_flags(flags: c_ulong) {
    #[cfg(target_os = "none")]
    IrqSave::release(flags as usize);
    #[cfg(not(target_os = "none"))]
    {
        let _ = flags;
        IrqSave::release(());
    }
}

#[inline]
fn preempt_disable() {
    NoPreempt::acquire();
}

#[inline]
fn preempt_enable() {
    NoPreempt::release(());
}

#[inline]
unsafe fn raw_lock(lock: *mut Spinlock) {
    // SAFETY: forwarded from raw_lock's caller. The lock's exact size and
    // alignment are asserted above; initialization writes a valid unlocked
    // word, and only this atomic view accesses it after initialization.
    let word = unsafe { AtomicU32::from_ptr(lock.cast::<u32>()) };
    loop {
        if word
            .compare_exchange_weak(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return;
        }
        while word.load(Ordering::Relaxed) != 0 {
            core::hint::spin_loop();
        }
    }
}

#[inline]
unsafe fn raw_try_lock(lock: *mut Spinlock) -> bool {
    // SAFETY: forwarded from raw_try_lock's caller; see raw_lock.
    unsafe { AtomicU32::from_ptr(lock.cast::<u32>()) }
        .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
        .is_ok()
}

#[inline]
unsafe fn raw_unlock(lock: *mut Spinlock) {
    // SAFETY: forwarded from raw_unlock's caller; see raw_lock.
    unsafe { AtomicU32::from_ptr(lock.cast::<u32>()) }.store(0, Ordering::Release);
}

#[inline]
fn lock_ptr(lock: &mut Spinlock) -> *mut Spinlock {
    lock as *mut Spinlock
}

/// Initialize a Linux x86 raw spin lock before sharing it with other CPUs.
///
/// This corresponds to Linux `spin_lock_init()`: the four-byte raw lock word
/// is reset to its unlocked state. The caller must ensure no CPU currently
/// holds or is attempting to acquire this lock.
#[inline]
pub fn spin_lock_init(lock: &mut Spinlock) {
    // SAFETY: a mutable reference proves this storage is live and exclusively
    // borrowed during initialization; the compile-time assertions prove the
    // zeroing covers exactly the lock's ABI-sized storage.
    unsafe {
        core::ptr::write_bytes(lock_ptr(lock).cast::<u8>(), 0, size_of::<Spinlock>());
    }
}

/// Acquire a Linux-style spin lock, disabling preemption for the hold interval.
#[inline]
pub fn spin_lock(lock: &mut Spinlock) {
    preempt_disable();
    // SAFETY: the reference is live and correctly aligned; callers initialize
    // each lock before use and do not access its storage non-atomically.
    unsafe { raw_lock(lock_ptr(lock)) };
}

/// Release a Linux-style spin lock and re-enable preemption.
#[inline]
pub fn spin_unlock(lock: &mut Spinlock) {
    // SAFETY: the reference is live and correctly aligned; caller must own the
    // lock, matching Linux spin_unlock's contract.
    unsafe { raw_unlock(lock_ptr(lock)) };
    preempt_enable();
}

/// Acquire a spin lock with local IRQs disabled, preserving their prior state.
#[inline]
pub fn spin_lock_irqsave(lock: &mut Spinlock, flags: &mut c_ulong) {
    // Linux v7.2's x86 generic raw-spin path orders these as irq-save then
    // preempt-disable, followed by the atomic lock acquisition.
    let saved = irq_save_flags();
    preempt_disable();
    // SAFETY: the reference is live and correctly aligned; see spin_lock.
    unsafe { raw_lock(lock_ptr(lock)) };
    *flags = saved;
}

/// Release a spin lock and restore the exact incoming local IRQ state.
#[inline]
pub fn spin_unlock_irqrestore(lock: &mut Spinlock, flags: c_ulong) {
    // Linux v7.2 restores IRQ state after unlocking and before preempt-enable.
    // SAFETY: the reference is live and correctly aligned; caller must own the
    // lock and pass the flags returned by the matching irqsave call.
    unsafe { raw_unlock(lock_ptr(lock)) };
    irq_restore_flags(flags);
    preempt_enable();
}

/// Acquire a spin lock after unconditionally disabling local IRQs.
#[inline]
pub fn spin_lock_irq(lock: &mut Spinlock) {
    local_irq_disable();
    preempt_disable();
    // SAFETY: the reference is live and correctly aligned; see spin_lock.
    unsafe { raw_lock(lock_ptr(lock)) };
}

/// Release a spin lock and unconditionally enable local IRQs.
///
/// Like Linux `spin_unlock_irq`, this does not restore a saved IRQ state; the
/// matching caller contract requires IRQs to be enabled at the end.
#[inline]
pub fn spin_unlock_irq(lock: &mut Spinlock) {
    // SAFETY: the reference is live and correctly aligned; caller must own the
    // lock, matching Linux spin_unlock_irq's contract.
    unsafe { raw_unlock(lock_ptr(lock)) };
    local_irq_enable();
    preempt_enable();
}

/// Try to acquire a spin lock without disabling local IRQs.
#[inline]
pub fn spin_trylock(lock: &mut Spinlock) -> bool {
    preempt_disable();
    // SAFETY: the reference is live and correctly aligned; see spin_lock.
    if unsafe { raw_try_lock(lock_ptr(lock)) } {
        true
    } else {
        preempt_enable();
        false
    }
}

/// Try to acquire a spin lock with IRQ-save semantics.
///
/// On failure, the IRQ state and preemption nesting are restored before
/// returning `false`, matching Linux `spin_trylock_irqsave`.
#[inline]
pub fn spin_trylock_irqsave(lock: &mut Spinlock, flags: &mut c_ulong) -> bool {
    let saved = irq_save_flags();
    preempt_disable();
    // SAFETY: the reference is live and correctly aligned; see spin_lock.
    if unsafe { raw_try_lock(lock_ptr(lock)) } {
        *flags = saved;
        true
    } else {
        preempt_enable();
        irq_restore_flags(saved);
        false
    }
}

/// Disable local interrupts without changing their saved state.
#[inline]
pub fn local_irq_disable() {
    // Acquiring IrqSave saves-and-disables. Linux local_irq_disable discards
    // the prior state rather than retaining a restore token.
    let _ = irq_save_flags();
}

/// Enable local interrupts unconditionally, as Linux local_irq_enable does.
#[inline]
pub fn local_irq_enable() {
    #[cfg(all(target_os = "none", target_arch = "x86_64"))]
    // SAFETY: the GT LinuxKPI binding is x86_64-only, and this is the
    // architectural operation used by Linux `arch_local_irq_enable`.
    unsafe {
        core::arch::asm!("sti", options(nostack));
    }
    #[cfg(not(target_os = "none"))]
    {}
}

/// Query x86's IF bit without changing interrupt state.
#[inline]
pub fn irqs_disabled() -> bool {
    #[cfg(all(target_os = "none", target_arch = "x86_64"))]
    {
        let flags: usize;
        // SAFETY: reading RFLAGS does not modify architectural state.
        unsafe { core::arch::asm!("pushfq", "pop {}", out(reg) flags) };
        flags & (1 << 9) == 0
    }
    #[cfg(not(all(target_os = "none", target_arch = "x86_64")))]
    {
        false
    }
}

/// Acquire a spin lock through a Linux-style raw pointer.
///
/// # Safety
/// `lock` must be a non-null, aligned pointer to a live lock initialized by
/// `spin_lock_init`; it must remain valid for the duration of the call.
#[inline]
pub unsafe fn spin_lock_raw(lock: *mut Spinlock) {
    preempt_disable();
    // SAFETY: required by this function's contract.
    unsafe { raw_lock(lock) };
}

/// Release a spin lock through a Linux-style raw pointer.
///
/// # Safety
/// `lock` must meet `spin_lock_raw`'s pointer requirements and be held by the
/// current execution context.
#[inline]
pub unsafe fn spin_unlock_raw(lock: *mut Spinlock) {
    // SAFETY: required by this function's contract.
    unsafe { raw_unlock(lock) };
    preempt_enable();
}

/// Acquire a spin lock with IRQ-save semantics through a raw pointer.
///
/// # Safety
/// `lock` must meet `spin_lock_raw`'s pointer requirements.
#[inline]
pub unsafe fn spin_lock_irqsave_raw(lock: *mut Spinlock, flags: &mut c_ulong) {
    let saved = irq_save_flags();
    preempt_disable();
    // SAFETY: required by this function's contract.
    unsafe { raw_lock(lock) };
    *flags = saved;
}

/// Release a spin lock with IRQ restoration through a raw pointer.
///
/// # Safety
/// `lock` must meet `spin_lock_raw`'s requirements, be held by the current
/// context, and `flags` must come from its matching irqsave operation.
#[inline]
pub unsafe fn spin_unlock_irqrestore_raw(lock: *mut Spinlock, flags: c_ulong) {
    // SAFETY: required by this function's contract.
    unsafe { raw_unlock(lock) };
    irq_restore_flags(flags);
    preempt_enable();
}

/// Acquire a spin lock with IRQ-disable semantics through a raw pointer.
///
/// # Safety
/// `lock` must meet `spin_lock_raw`'s pointer requirements.
#[inline]
pub unsafe fn spin_lock_irq_raw(lock: *mut Spinlock) {
    local_irq_disable();
    preempt_disable();
    // SAFETY: required by this function's contract.
    unsafe { raw_lock(lock) };
}

/// Release a spin lock and unconditionally enable IRQs through a raw pointer.
///
/// # Safety
/// `lock` must meet `spin_lock_raw`'s requirements and be held by the current
/// context. Callers must require IRQs to be enabled after unlock.
#[inline]
pub unsafe fn spin_unlock_irq_raw(lock: *mut Spinlock) {
    // SAFETY: required by this function's contract.
    unsafe { raw_unlock(lock) };
    local_irq_enable();
    preempt_enable();
}

/// Try to acquire a spin lock with IRQ-save semantics through a raw pointer.
///
/// # Safety
/// `lock` must meet `spin_lock_raw`'s pointer requirements.
#[inline]
pub unsafe fn spin_trylock_irqsave_raw(lock: *mut Spinlock, flags: &mut c_ulong) -> bool {
    let saved = irq_save_flags();
    preempt_disable();
    // SAFETY: required by this function's contract.
    if unsafe { raw_try_lock(lock) } {
        *flags = saved;
        true
    } else {
        preempt_enable();
        irq_restore_flags(saved);
        false
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;
    use core::{
        cell::UnsafeCell,
        mem::MaybeUninit,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use std::sync::Arc;

    use super::*;

    struct SharedSpinlock(UnsafeCell<Spinlock>);

    // SAFETY: all accesses after initialization go through this module's raw
    // atomic lock functions; the storage itself is never read non-atomically.
    unsafe impl Sync for SharedSpinlock {}

    impl SharedSpinlock {
        fn new() -> Self {
            let mut storage = MaybeUninit::<Spinlock>::zeroed();
            // SAFETY: zero is a valid initialized byte pattern for the opaque
            // four-byte lock storage; initialization is exclusive here.
            unsafe { spin_lock_init(storage.assume_init_mut()) };
            // SAFETY: spin_lock_init initialized every byte of the storage.
            Self(UnsafeCell::new(unsafe { storage.assume_init() }))
        }

        fn ptr(&self) -> *mut Spinlock {
            self.0.get()
        }
    }

    #[test]
    fn spin_lock_is_mutually_exclusive_across_threads() {
        const THREADS: usize = 6;
        const ITERATIONS: usize = 4_000;

        let lock = Arc::new(SharedSpinlock::new());
        let inside = Arc::new(AtomicBool::new(false));
        let overlap = Arc::new(AtomicUsize::new(0));
        let counter = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();

        for _ in 0..THREADS {
            let lock = Arc::clone(&lock);
            let inside = Arc::clone(&inside);
            let overlap = Arc::clone(&overlap);
            let counter = Arc::clone(&counter);
            workers.push(std::thread::spawn(move || {
                for _ in 0..ITERATIONS {
                    // SAFETY: SharedSpinlock initializes once and only exposes
                    // this valid pointer to the actual lock operations.
                    unsafe { spin_lock_raw(lock.ptr()) };
                    if inside.swap(true, Ordering::SeqCst) {
                        overlap.fetch_add(1, Ordering::Relaxed);
                    }
                    counter.fetch_add(1, Ordering::Relaxed);
                    inside.store(false, Ordering::SeqCst);
                    // SAFETY: this thread holds the lock acquired above.
                    unsafe { spin_unlock_raw(lock.ptr()) };
                }
            }));
        }

        for worker in workers {
            worker.join().expect("spinlock worker panicked");
        }
        assert_eq!(overlap.load(Ordering::Relaxed), 0);
        assert_eq!(counter.load(Ordering::Relaxed), THREADS * ITERATIONS);
    }

    #[test]
    fn irqsave_trylock_failure_restores_host_context() {
        let mut storage = MaybeUninit::<Spinlock>::zeroed();
        // SAFETY: zeroed bytes are valid storage and no concurrent users exist.
        unsafe { spin_lock_init(storage.assume_init_mut()) };
        // SAFETY: initialized above and local to this test.
        let mut lock = unsafe { storage.assume_init() };
        let mut flags: c_ulong = 0;

        assert!(spin_trylock_irqsave(&mut lock, &mut flags));
        assert!(!spin_trylock_irqsave(&mut lock, &mut flags));
        spin_unlock_irqrestore(&mut lock, flags);
    }
}

/// Native reader/writer lock in the four-byte, zero-initialized rwlock storage.
/// All LinuxKPI users of that storage use this implementation, never mix it
/// with Linux qrwlock instructions. Reader guards retain native preemption
/// exclusion until read_unlock, as required for DRM lookup atomic sections.
pub unsafe fn read_lock(lock: *mut core::sync::atomic::AtomicU32) {
    NoPreempt::acquire();
    loop {
        let old = unsafe {(*lock).load(Ordering::Relaxed)};
        if old & (1 << 31) == 0 && old < (1 << 31) - 1
            && unsafe {(*lock).compare_exchange_weak(old,old+1,Ordering::Acquire,Ordering::Relaxed)}.is_ok() {break;}
        core::hint::spin_loop();
    }
}
pub unsafe fn read_unlock(lock: *mut core::sync::atomic::AtomicU32) {
    let old=unsafe {(*lock).fetch_sub(1,Ordering::Release)};
    assert!(old!=0 && old&(1<<31)==0);
    NoPreempt::release(());
}
pub unsafe fn write_lock(lock:*mut core::sync::atomic::AtomicU32) {
    NoPreempt::acquire();
    while unsafe {(*lock).compare_exchange_weak(0,1<<31,Ordering::Acquire,Ordering::Relaxed)}.is_err(){core::hint::spin_loop();}
}
pub unsafe fn write_unlock(lock:*mut core::sync::atomic::AtomicU32) {
    assert_eq!(unsafe {(*lock).swap(0,Ordering::Release)},1<<31);
    NoPreempt::release(());
}

// ---------------------------------------------------------------------------
// C ABI entry points used by the source-order i915 translations.
//
// Lock-debugging subclasses are ignored because CONFIG_LOCKDEP is disabled, so
// the `_nested` variants are the plain operations. Rust helpers that already
// carry the same name keep their identifiers; the C symbol is attached with
// `export_name`.
// ---------------------------------------------------------------------------

/// Linux `spin_unlock()`.
#[unsafe(export_name = "spin_unlock")]
pub unsafe extern "C" fn c_spin_unlock(lock: *mut Spinlock) {
    unsafe { spin_unlock_raw(lock) };
}

/// Linux `spin_lock_nested()`; the subclass only feeds lockdep.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn spin_lock_nested(lock: *mut Spinlock, _subclass: u32) {
    unsafe { spin_lock_raw(lock) };
}

/// Linux `spin_lock_irq()`.
#[unsafe(export_name = "spin_lock_irq")]
pub unsafe extern "C" fn c_spin_lock_irq(lock: *mut Spinlock) {
    unsafe { spin_lock_irq_raw(lock) };
}

/// Linux `spin_unlock_irq()`.
#[unsafe(export_name = "spin_unlock_irq")]
pub unsafe extern "C" fn c_spin_unlock_irq(lock: *mut Spinlock) {
    unsafe { spin_unlock_irq_raw(lock) };
}

/// Linux `spin_lock_irqsave()`.
#[unsafe(export_name = "spin_lock_irqsave")]
pub unsafe extern "C" fn c_spin_lock_irqsave(lock: *mut Spinlock, flags: *mut c_ulong) {
    unsafe { spin_lock_irqsave_raw(lock, &mut *flags) };
}

/// Linux `spin_lock_irqsave_nested()`; the subclass only feeds lockdep.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn spin_lock_irqsave_nested(
    lock: *mut Spinlock,
    flags: *mut c_ulong,
    _subclass: u32,
) {
    unsafe { spin_lock_irqsave_raw(lock, &mut *flags) };
}

/// Linux `spin_unlock_irqrestore()`.
#[unsafe(export_name = "spin_unlock_irqrestore")]
pub unsafe extern "C" fn c_spin_unlock_irqrestore(lock: *mut Spinlock, flags: c_ulong) {
    unsafe { spin_unlock_irqrestore_raw(lock, flags) };
}

/// Linux `cpu_relax()`: a spin-wait hint with no architectural side effects.
#[unsafe(no_mangle)]
pub extern "C" fn cpu_relax() {
    core::hint::spin_loop();
}

/// Linux `__cond_resched_lock()`: when a reschedule is pending, drop the
/// spinlock, yield, retake the lock, and return 1; otherwise return 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __cond_resched_lock(lock: *mut Spinlock) -> i32 {
    if !crate::linux::wait::need_resched() {
        return 0;
    }
    unsafe { spin_unlock_raw(lock) };
    crate::linux::wait::cond_resched();
    unsafe { spin_lock_raw(lock) };
    1
}
