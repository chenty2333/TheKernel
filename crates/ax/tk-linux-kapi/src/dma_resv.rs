// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! LinuxKPI reservation objects: Linux 7.2.3 `drivers/dma-buf/dma-resv.c` and
//! `include/linux/dma-resv.h`. Each `dma_resv` keeps its fences in a native
//! sidecar; the iterator and wait helpers read it through `dma_resv_iter_next`.

#![allow(unsafe_code)]

use alloc::{alloc::{alloc, dealloc}, vec::Vec};
use core::{
    ffi::{c_int, c_long, c_void},
    mem::size_of,
    ptr,
    sync::atomic::{AtomicPtr, Ordering},
};

use crate::{
    config::{EINVAL, ENOMEM},
    dma_fence::{dma_fence_context_alloc, dma_fence_get, dma_fence_is_signaled, dma_fence_put, dma_fence_wait_timeout},
    dma_fence_array::dma_fence_array_create,
    types::{DmaFence, DmaResv},
};

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DmaResvIter {
    obj: *mut DmaResv,
    usage: u32,
    fence: *mut DmaFence,
    fence_usage: u32,
    index: u32,
    fences: *mut c_void,
    num_fences: u32,
    is_restarted: bool,
}

const _: [(); 48] = [(); size_of::<DmaResvIter>()];

impl DmaResvIter {
    fn new(resv: *mut DmaResv, usage: u32) -> Self {
        Self {
            obj: resv,
            usage,
            fence: ptr::null_mut(),
            fence_usage: 0,
            index: 0,
            fences: ptr::null_mut(),
            num_fences: 0,
            is_restarted: false,
        }
    }
}

#[repr(C)]
struct ReservedFence {
    fence: *mut DmaFence,
    usage: u32,
}

struct ReservationState {
    entries: spin::Mutex<Vec<ReservedFence>>,
}

unsafe fn alloc_object<T>(value: T) -> *mut T {
    let layout = core::alloc::Layout::new::<T>();
    let object = unsafe { alloc(layout) }.cast::<T>();
    if object.is_null() {
        return ptr::null_mut();
    }
    unsafe { object.write(value) };
    object
}

unsafe fn free_object<T>(object: *mut T) {
    if object.is_null() {
        return;
    }
    unsafe {
        ptr::drop_in_place(object);
        dealloc(object.cast(), core::alloc::Layout::new::<T>());
    }
}

unsafe fn reservation_state(
    resv: *mut DmaResv,
    create: bool,
) -> Result<*mut ReservationState, c_int> {
    if resv.is_null() {
        return Err(-EINVAL);
    }
    let slot = unsafe {
        AtomicPtr::from_ptr(ptr::addr_of_mut!((*resv).fences).cast::<*mut ReservationState>())
    };
    let current = slot.load(Ordering::Acquire);
    if !current.is_null() || !create {
        return Ok(current);
    }
    let state = unsafe {
        alloc_object(ReservationState {
            entries: spin::Mutex::new(Vec::new()),
        })
    };
    if state.is_null() {
        return Err(-ENOMEM);
    }
    match slot.compare_exchange(ptr::null_mut(), state, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => Ok(state),
        Err(existing) => {
            unsafe { free_object(state) };
            Ok(existing)
        }
    }
}

/// Reserve capacity in the native reservation list. The backing allocation is
/// real and all later fence insertion paths retain a reference.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_resv_reserve_fences(resv: *mut DmaResv, count: u32) -> c_int {
    let state = match unsafe { reservation_state(resv, true) } {
        Ok(state) => state,
        Err(error) => return error,
    };
    match unsafe { (*state).entries.lock().try_reserve(count as usize) } {
        Ok(()) => 0,
        Err(_) => -ENOMEM,
    }
}

/// Linux `dma_resv_add_fence()`: insert the fence, then mirror it into the
/// native alias graph through the installed native fence hooks.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_resv_add_fence(resv: *mut DmaResv, fence: *mut DmaFence, usage: u32) {
    unsafe { dma_resv_add_fence_native(resv, fence, usage) };
    if let Some(hooks) = crate::dma_fence_hooks::native_fence_hooks() {
        unsafe { (hooks.resv_alias_add)(resv.cast(), fence.cast(), usage) };
    }
}

/// Insert a fence into the reservation and retain its ownership reference.
pub unsafe fn dma_resv_add_fence_native(resv: *mut DmaResv, fence: *mut DmaFence, usage: u32) {
    assert!(!fence.is_null(), "dma_resv_add_fence received a null fence");
    let state = unsafe { reservation_state(resv, true) }
        .unwrap_or_else(|error| panic!("dma_resv_add_fence failed: {error}"));
    let mut entries = unsafe { (*state).entries.lock() };
    if entries.len() == entries.capacity() {
        entries
            .try_reserve(1)
            .expect("dma_resv_add_fence requires successful prior reservation");
    }
    dma_fence_get(fence);
    entries.push(ReservedFence { fence, usage });
    drop(entries);
}

/// Iterate reservation fences with Linux's usage ordering and a live cursor
/// reference, released by the caller after iteration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_resv_iter_next(cursor: *mut DmaResvIter) -> *mut DmaFence {
    if cursor.is_null() {
        return ptr::null_mut();
    }
    let state = match unsafe { reservation_state((*cursor).obj, false) } {
        Ok(state) if !state.is_null() => state,
        _ => return ptr::null_mut(),
    };
    let entries = unsafe { (*state).entries.lock() };
    let usage = unsafe { (*cursor).usage };
    let start = unsafe { (*cursor).index as usize };
    let next = entries
        .iter()
        .enumerate()
        .skip(start)
        .find(|(_, entry)| entry.usage <= usage);
    let Some((index, entry)) = next else {
        return ptr::null_mut();
    };
    let new_fence = dma_fence_get(entry.fence);
    let old_fence = unsafe { (*cursor).fence };
    if !old_fence.is_null() {
        unsafe { dma_fence_put(old_fence) };
    }
    unsafe {
        (*cursor).fence = new_fence;
        (*cursor).fence_usage = entry.usage;
        (*cursor).index = index.saturating_add(1) as u32;
        (*cursor).fences = state.cast();
        (*cursor).num_fences = entries.len() as u32;
        (*cursor).is_restarted = false;
    }
    new_fence
}

/// Tear down the reservation sidecar and release all retained fence refs.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_resv_fini(resv: *mut DmaResv) {
    if resv.is_null() {
        return;
    }
    let state = unsafe {
        AtomicPtr::from_ptr(ptr::addr_of_mut!((*resv).fences).cast::<*mut ReservationState>())
            .swap(ptr::null_mut(), Ordering::AcqRel)
    };
    if state.is_null() {
        return;
    }
    for entry in unsafe { (*state).entries.lock() }.iter() {
        unsafe { dma_fence_put(entry.fence) };
    }
    unsafe { free_object(state) };
}

/// Initialize a reservation sidecar when a framework owner initializes its
/// `dma_resv` record, including its native WW mutex.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_resv_init(resv: *mut DmaResv) {
    if resv.is_null() {
        return;
    }
    unsafe {
        crate::mutex::mutex_init(ptr::addr_of_mut!((*resv).lock.base));
        (*resv).lock.ctx = ptr::null_mut();
    }
    let slot = unsafe {
        AtomicPtr::from_ptr(ptr::addr_of_mut!((*resv).fences).cast::<*mut ReservationState>())
    };
    slot.store(ptr::null_mut(), Ordering::Release);
}

/// Release the reference the reservation cursor holds on its current fence.
unsafe fn resv_iter_end(cursor: &mut DmaResvIter) {
    if !cursor.fence.is_null() {
        unsafe { dma_fence_put(cursor.fence) };
        cursor.fence = core::ptr::null_mut();
    }
}

/// Linux `dma_resv_test_signaled()`: true when every fence at or below
/// `usage` has signaled.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_resv_test_signaled(resv: *mut c_void, usage: c_int) -> bool {
    assert!(!resv.is_null());
    let mut cursor = DmaResvIter::new(resv.cast(), usage as u32);
    let mut all = true;
    loop {
        // The cursor keeps the returned fence referenced until the next step.
        let fence = unsafe { dma_resv_iter_next(&mut cursor) };
        if fence.is_null() {
            break;
        }
        if !unsafe { dma_fence_is_signaled(fence) } {
            all = false;
            break;
        }
    }
    unsafe { resv_iter_end(&mut cursor) };
    all
}

/// Linux `dma_resv_wait_timeout()`: wait for every fence at or below `usage`.
/// Returns the remaining jiffies (at least 1), or 0 when the wait timed out.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_resv_wait_timeout(
    resv: *mut c_void,
    usage: c_int,
    intr: bool,
    timeout: c_long,
) -> c_long {
    assert!(!resv.is_null());
    let mut cursor = DmaResvIter::new(resv.cast(), usage as u32);
    let mut remaining = timeout;
    loop {
        let fence = unsafe { dma_resv_iter_next(&mut cursor) };
        if fence.is_null() {
            break;
        }
        let left = unsafe { dma_fence_wait_timeout(fence, intr, remaining) };
        if left == 0 {
            unsafe { resv_iter_end(&mut cursor) };
            return 0;
        }
        remaining = left;
    }
    unsafe { resv_iter_end(&mut cursor) };
    if remaining != 0 { remaining } else { 1 }
}

/// Linux `dma_resv_get_singleton()`: one fence that stands for every fence at
/// or below `usage`. No fence gives NULL, one fence is returned by reference,
/// and several are gathered into a fence array that signals when all do.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_resv_get_singleton(
    resv: *mut c_void,
    usage: c_int,
    fence_out: *mut *mut DmaFence,
) -> c_int {
    assert!(!resv.is_null() && !fence_out.is_null());
    let mut cursor = DmaResvIter::new(resv.cast(), usage as u32);
    // Every gathered fence takes its own reference; the cursor's reference is
    // released once iteration ends.
    let mut fences: alloc::vec::Vec<*mut DmaFence> = alloc::vec::Vec::new();
    loop {
        let fence = unsafe { dma_resv_iter_next(&mut cursor) };
        if fence.is_null() {
            break;
        }
        fences.push(unsafe { dma_fence_get(fence) });
    }
    unsafe { resv_iter_end(&mut cursor) };
    let result = match fences.len() {
        0 => core::ptr::null_mut(),
        1 => fences[0],
        n => {
            let array = unsafe {
                dma_fence_array_create(n as u32, fences.as_mut_ptr(), dma_fence_context_alloc(1), 1)
            };
            array.cast::<DmaFence>()
        }
    };
    unsafe { *fence_out = result };
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::AtomicUsize;

    use crate::{
        config::ENOENT,
        dma_fence::{dma_fence_add_callback, dma_fence_init, dma_fence_is_signaled, dma_fence_signal},
        locks::spin_lock_init,
        types::{DmaFenceCb, DmaFenceOps, Spinlock},
    };
    const DMA_RESV_USAGE_KERNEL: u32 = 0;
    static CALLBACKS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn completed(_: *mut DmaFence, _: *mut DmaFenceCb) {
        CALLBACKS.fetch_add(1, Ordering::Relaxed);
    }

    #[test]
    fn fence_callbacks_and_reservation_references_have_real_lifetimes() {
        unsafe {
            let mut fence: DmaFence = core::mem::zeroed();
            let mut lock: Spinlock = core::mem::zeroed();
            let ops: DmaFenceOps = core::mem::zeroed();
            spin_lock_init(&mut lock);
            dma_fence_init(&mut fence, &ops, &mut lock, 1, 2);
            let mut callback: DmaFenceCb = core::mem::zeroed();
            assert_eq!(
                dma_fence_add_callback(&mut fence, &mut callback, completed),
                0
            );
            assert!(!dma_fence_is_signaled(&mut fence));
            dma_fence_signal(&mut fence);
            dma_fence_signal(&mut fence);
            assert!(dma_fence_is_signaled(&mut fence));
            assert_eq!(CALLBACKS.load(Ordering::Relaxed), 1);
            assert_eq!(
                dma_fence_add_callback(&mut fence, &mut callback, completed),
                -ENOENT
            );
            // Also exercises the empty callback list and timestamp union switch.
            let mut empty: DmaFence = core::mem::zeroed();
            dma_fence_init(&mut empty, &ops, &mut lock, 1, 3);
            dma_fence_signal(&mut empty);
            let mut resv: DmaResv = core::mem::zeroed();
            dma_resv_init(&mut resv);
            assert_eq!(dma_resv_reserve_fences(&mut resv, 1), 0);
            dma_resv_add_fence_native(&mut resv, &mut fence, DMA_RESV_USAGE_KERNEL);
            let mut cursor: DmaResvIter = core::mem::zeroed();
            cursor.obj = &mut resv;
            cursor.usage = DMA_RESV_USAGE_KERNEL;
            assert_eq!(dma_resv_iter_next(&mut cursor), &mut fence as *mut _);
            assert!(dma_resv_iter_next(&mut cursor).is_null());
            dma_fence_put(cursor.fence);
            dma_resv_fini(&mut resv);
            assert_eq!(fence.refcount.refcount.refs.counter, 1);
        }
    }
}
