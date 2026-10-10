// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../../LICENSE-MIT.
//! Linux SRCU embedded-record ABI and LinuxKPI epoch runtime.
//!
//! Linux v7.2.3 `include/linux/srcutree.h` defines `srcu_struct` when
//! CONFIG_TREE_SRCU=y. With CONFIG_LOCKDEP=n, `lockdep_map` is an empty C
//! record. The layout remains the source C ABI; its opaque pointer fields
//! reference this module's two-epoch reader counters rather than Linux's
//! internal per-CPU `srcu_data` allocation.

use core::{
    ffi::c_void,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use crate::linux_config::{ENOMEM, GFP_KERNEL};

#[repr(C)]
pub struct SrcuStruct {
    pub srcu_ctrp: *mut c_void,
    pub sda: *mut c_void,
    pub srcu_reader_flavor: u8,
    _padding: [u8; 7],
    pub srcu_sup: *mut c_void,
}

/// Runtime backing for an embedded SRCU record. Readers select one of two
/// counters; an expedited grace period flips the active counter and waits
/// only for readers that entered the old epoch. The serializer prevents two
/// grace periods from reopening an epoch before the first one has drained.
struct SrcuRuntime {
    active: AtomicUsize,
    readers: [AtomicUsize; 2],
    synchronize: AtomicBool,
}

impl SrcuRuntime {
    const fn new() -> Self {
        Self {
            active: AtomicUsize::new(0),
            readers: [AtomicUsize::new(0), AtomicUsize::new(0)],
            synchronize: AtomicBool::new(false),
        }
    }
}

unsafe fn runtime(srcu: *mut SrcuStruct) -> *mut SrcuRuntime {
    assert!(!srcu.is_null(), "SRCU operation on a null record");
    let state = unsafe { (*srcu).srcu_ctrp.cast::<SrcuRuntime>() };
    assert!(!state.is_null(), "SRCU operation before initialization");
    state
}

/// Initialize the runtime backing for a Linux-compatible `srcu_struct`.
/// The returned error follows `init_srcu_struct()` and leaves the record
/// untouched if its sleepable allocation cannot be satisfied.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn init_srcu_struct(srcu: *mut SrcuStruct) -> i32 {
    if srcu.is_null() {
        return -crate::linux_config::EINVAL;
    }
    let state = crate::linux_memory::kzalloc_obj_flags::<SrcuRuntime>(GFP_KERNEL);
    if state.is_null() {
        return -ENOMEM;
    }
    unsafe {
        core::ptr::write(state, SrcuRuntime::new());
        (*srcu).srcu_ctrp = state.cast();
        (*srcu).sda = core::ptr::null_mut();
        (*srcu).srcu_reader_flavor = 0;
        (*srcu)._padding = [0; 7];
        (*srcu).srcu_sup = state.cast();
    }
    0
}

/// Enter an SRCU read-side critical section and return its epoch token.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn srcu_read_lock(srcu: *mut SrcuStruct) -> i32 {
    let state = unsafe { &*runtime(srcu) };
    loop {
        let epoch = state.active.load(Ordering::Acquire) & 1;
        state.readers[epoch].fetch_add(1, Ordering::SeqCst);
        if state.active.load(Ordering::SeqCst) & 1 == epoch {
            return epoch as i32;
        }
        let old = state.readers[epoch].fetch_sub(1, Ordering::SeqCst);
        assert!(old != 0, "SRCU reader count underflow during retry");
    }
}

/// Leave the SRCU read-side critical section identified by `epoch`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn srcu_read_unlock(srcu: *mut SrcuStruct, epoch: i32) {
    assert!(epoch == 0 || epoch == 1, "invalid SRCU epoch token");
    let state = unsafe { &*runtime(srcu) };
    let old = state.readers[epoch as usize].fetch_sub(1, Ordering::SeqCst);
    assert!(old != 0, "SRCU reader count underflow");
}

/// Wait for pre-existing read-side critical sections to finish.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn synchronize_srcu_expedited(srcu: *mut SrcuStruct) {
    crate::linux_wait::might_sleep();
    assert!(
        axtask::can_block_current(),
        "SRCU grace period requires a sleepable task context"
    );
    let state = unsafe { &*runtime(srcu) };
    while state
        .synchronize
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        axtask::yield_now();
    }
    let old_epoch = state.active.fetch_xor(1, Ordering::SeqCst) & 1;
    while state.readers[old_epoch].load(Ordering::SeqCst) != 0 {
        axtask::yield_now();
    }
    state.synchronize.store(false, Ordering::Release);
}

/// Destroy SRCU backing after its readers have quiesced.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cleanup_srcu_struct(srcu: *mut SrcuStruct) {
    if srcu.is_null() || unsafe { (*srcu).srcu_ctrp.is_null() } {
        return;
    }
    unsafe { synchronize_srcu_expedited(srcu) };
    let state = unsafe { (*srcu).srcu_ctrp.cast::<SrcuRuntime>() };
    unsafe { crate::linux_memory::kfree(state) };
    unsafe {
        (*srcu).srcu_ctrp = core::ptr::null_mut();
        (*srcu).srcu_sup = core::ptr::null_mut();
    }
}

const _: [(); 32] = [(); core::mem::size_of::<SrcuStruct>()];
const _: [(); 8] = [(); core::mem::align_of::<SrcuStruct>()];
const _: [(); 0] = [(); core::mem::offset_of!(SrcuStruct, srcu_ctrp)];
const _: [(); 8] = [(); core::mem::offset_of!(SrcuStruct, sda)];
const _: [(); 16] = [(); core::mem::offset_of!(SrcuStruct, srcu_reader_flavor)];
const _: [(); 24] = [(); core::mem::offset_of!(SrcuStruct, srcu_sup)];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srcu_epoch_read_unlock_and_rotation() {
        let mut srcu = core::mem::MaybeUninit::<SrcuStruct>::zeroed();
        let srcu = srcu.as_mut_ptr();
        assert_eq!(unsafe { init_srcu_struct(srcu) }, 0);
        let state = unsafe { (*srcu).srcu_ctrp.cast::<SrcuRuntime>() };
        let old_epoch = unsafe { srcu_read_lock(srcu) };
        assert!(old_epoch == 0 || old_epoch == 1);
        assert_eq!(
            unsafe { &*state }.readers[old_epoch as usize].load(Ordering::SeqCst),
            1
        );
        unsafe { srcu_read_unlock(srcu, old_epoch) };
        assert_eq!(
            unsafe { &*state }.readers[old_epoch as usize].load(Ordering::SeqCst),
            0
        );
        unsafe { &*state }.active.fetch_xor(1, Ordering::SeqCst);
        let next_epoch = unsafe { srcu_read_lock(srcu) };
        assert_ne!(next_epoch, old_epoch);
        unsafe { srcu_read_unlock(srcu, next_epoch) };
        unsafe { crate::linux_memory::kfree(state) };
    }
}
