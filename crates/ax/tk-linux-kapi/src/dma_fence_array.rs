// SPDX-License-Identifier: GPL-2.0-only
// Copyright 2026 TheKernel contributors.
//! LinuxKPI dma-fence array: Linux 7.2.3 `drivers/dma-buf/dma-fence-array.c` and
//! `include/linux/dma-fence-array.h`. A fence array signals once every member
//! fence has signaled.

#![allow(unsafe_code)]

use alloc::vec::Vec;
use core::{
    mem::{ManuallyDrop, MaybeUninit},
    ptr,
    sync::atomic::{AtomicI32, Ordering},
};

use crate::{
    dma_fence::{
        DMA_FENCE_FLAG_ENABLE_SIGNAL_BIT, DMA_FENCE_FLAG_INLINE_LOCK_BIT, dma_fence_default_wait,
        dma_fence_get, dma_fence_put, dma_fence_set_deadline,
    },
    memory::kref_init,
    types::{AtomicT, DmaFence, DmaFenceCb, DmaFenceOps, DmaFenceTimestamp, IrqWork},
};

const DMA_FENCE_ARRAY_PENDING_ERROR: i32 = 1;

#[repr(C)]
struct DmaFenceArrayCallback {
    callback: DmaFenceCb,
    array: *mut DmaFenceArray,
}

#[repr(C)]
pub struct DmaFenceArray {
    pub base: DmaFence,
    num_fences: u32,
    num_pending: AtomicT,
    fences: *mut *mut DmaFence,
    work: IrqWork,
    callbacks: [MaybeUninit<DmaFenceArrayCallback>; 0],
}

unsafe fn dma_fence_container_init(
    fence: *mut DmaFence,
    ops: *const DmaFenceOps,
    context: u64,
    seqno: u64,
) {
    assert!(!fence.is_null() && !ops.is_null());
    unsafe {
        let lock = ptr::addr_of_mut!((*fence).lock.inline_lock);
        crate::locks::spin_lock_init(&mut *lock);
        (*fence).ops = ops.cast();
        (*fence).timestamp_union = DmaFenceTimestamp {
            cb_list: ManuallyDrop::new(crate::types::ListHead {
                next: ptr::null_mut(),
                prev: ptr::null_mut(),
            }),
        };
        crate::list::INIT_LIST_HEAD(
            ptr::addr_of_mut!((*fence).timestamp_union.cb_list).cast(),
        );
        (*fence).context = context;
        (*fence).seqno = seqno;
        (*fence).flags = 1 << DMA_FENCE_FLAG_INLINE_LOCK_BIT;
        kref_init(ptr::addr_of_mut!((*fence).refcount));
        (*fence).error = 0;
    }
}

#[unsafe(no_mangle)]
pub static dma_fence_array_ops: DmaFenceOps = DmaFenceOps {
    get_driver_name: Some(dma_fence_array_driver_name),
    get_timeline_name: Some(dma_fence_array_timeline_name),
    enable_signaling: Some(dma_fence_array_enable_signaling),
    signaled: Some(dma_fence_array_signaled),
    wait: Some(dma_fence_default_wait),
    release: Some(dma_fence_array_release),
    set_deadline: Some(dma_fence_array_set_deadline),
};

unsafe extern "C" fn dma_fence_array_driver_name(_: *mut DmaFence) -> *const core::ffi::c_char {
    c"dma_fence_array".as_ptr()
}

unsafe extern "C" fn dma_fence_array_timeline_name(_: *mut DmaFence) -> *const core::ffi::c_char {
    c"unbound".as_ptr()
}

/// Linux `dma_fence_array_first()` (include/linux/dma-fence-array.h): the first
/// member of an array, or `head` itself when it is not an array.
///
/// # Safety
/// `head` must be null or a live fence.
pub unsafe fn dma_fence_array_first(head: *mut DmaFence) -> *mut DmaFence {
    if head.is_null() {
        return ptr::null_mut();
    }
    if unsafe { (*head).ops } != ptr::addr_of!(dma_fence_array_ops).cast() {
        return head;
    }
    let array = head.cast::<DmaFenceArray>();
    unsafe { dma_fence_array_member(array, 0) }
}

/// Linux `dma_fence_array_next()`: member `index`, or null past the end.
///
/// # Safety
/// `head` must be null or a live fence.
pub unsafe fn dma_fence_array_next(head: *mut DmaFence, index: u32) -> *mut DmaFence {
    if head.is_null() || unsafe { (*head).ops } != ptr::addr_of!(dma_fence_array_ops).cast() {
        return ptr::null_mut();
    }
    unsafe { dma_fence_array_member(head.cast(), index) }
}

unsafe fn dma_fence_array_member(array: *mut DmaFenceArray, index: u32) -> *mut DmaFence {
    if index >= unsafe { (*array).num_fences } {
        return ptr::null_mut();
    }
    unsafe { *(*array).fences.add(index as usize) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_array_create(
    num_fences: u32,
    fences: *mut *mut DmaFence,
    context: u64,
    seqno: u32,
) -> *mut DmaFenceArray {
    if num_fences == 0 || fences.is_null() {
        return ptr::null_mut();
    }
    let Some(callback_bytes) =
        (num_fences as usize).checked_mul(core::mem::size_of::<DmaFenceArrayCallback>())
    else {
        return ptr::null_mut();
    };
    let Some(bytes) = core::mem::size_of::<DmaFenceArray>().checked_add(callback_bytes) else {
        return ptr::null_mut();
    };
    let array = crate::memory::kzalloc(bytes, crate::config::GFP_KERNEL)
        .cast::<DmaFenceArray>();
    if array.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        (*array).num_fences = num_fences;
        (*array).num_pending.counter = num_fences as i32;
        (*array).fences = fences;
        crate::irq_work::init_irq_work(ptr::addr_of_mut!((*array).work), dma_fence_array_work);
        dma_fence_container_init(
            ptr::addr_of_mut!((*array).base),
            ptr::addr_of!(dma_fence_array_ops),
            context,
            u64::from(seqno),
        );
        (*array).base.error = DMA_FENCE_ARRAY_PENDING_ERROR;
    }
    array
}

/// Release the references in a failed owned-array construction.
///
/// # Safety
/// Every entry must be a live fence reference owned by this call.
unsafe fn put_all(fences: &[*mut DmaFence]) {
    for &fence in fences {
        // SAFETY: each entry is one owned reference transferred to this helper.
        unsafe { dma_fence_put(fence) };
    }
}

/// Build an array from a vector of owned fence references.
///
/// `dma_fence_array_create()` takes ownership of a kernel-allocated pointer
/// buffer. A Rust `Vec` buffer cannot be transferred to it: the vector would
/// free that buffer on return, while the array later frees it with `kfree`.
/// This helper copies the references into a matching allocation and either
/// transfers both buffer and references to the array, or releases all of them
/// on failure.
///
/// # Safety
/// Every entry must be a live fence reference owned by the caller.
pub unsafe fn dma_fence_array_create_owned(
    fences: Vec<*mut DmaFence>,
    context: u64,
    seqno: u32,
) -> *mut DmaFenceArray {
    let Ok(num_fences) = u32::try_from(fences.len()) else {
        // SAFETY: ownership of all references was transferred to this function.
        unsafe { put_all(&fences) };
        return ptr::null_mut();
    };
    if num_fences == 0 {
        return ptr::null_mut();
    }

    // SAFETY: allocates the kernel-owned pointer buffer required by the public
    // array constructor; a null allocation is handled before it is used.
    let storage = unsafe {
        crate::memory::kmalloc_array(
            fences.len(),
            core::mem::size_of::<*mut DmaFence>(),
            crate::config::GFP_KERNEL,
        )
        .cast::<*mut DmaFence>()
    };
    if storage.is_null() {
        // SAFETY: the caller transferred the references to this function.
        unsafe { put_all(&fences) };
        return ptr::null_mut();
    }
    // SAFETY: `storage` holds `fences.len()` pointers and is disjoint from the
    // still-live `Vec` allocation.
    unsafe { ptr::copy_nonoverlapping(fences.as_ptr(), storage, fences.len()) };

    // SAFETY: `storage` is a kernel allocation containing the owned references
    // required by the array constructor.
    let array = unsafe { dma_fence_array_create(num_fences, storage, context, seqno) };
    if array.is_null() {
        // SAFETY: array creation failed before consuming either allocation or
        // references, so this function still owns both.
        unsafe {
            crate::memory::kfree(storage.cast::<core::ffi::c_void>());
            put_all(&fences);
        }
    }
    array
}

unsafe extern "C" fn dma_fence_array_work(work: *mut IrqWork) {
    let array = unsafe {
        work.cast::<u8>()
            .sub(core::mem::offset_of!(DmaFenceArray, work))
            .cast::<DmaFenceArray>()
    };
    unsafe {
        dma_fence_array_clear_pending_error(array);
        crate::dma_fence::dma_fence_signal(ptr::addr_of_mut!((*array).base));
        dma_fence_put(ptr::addr_of_mut!((*array).base));
    }
}

unsafe extern "C" fn dma_fence_array_callback(fence: *mut DmaFence, callback: *mut DmaFenceCb) {
    let array_cb = unsafe { callback.cast::<DmaFenceArrayCallback>() };
    let array = unsafe { (*array_cb).array };
    unsafe {
        dma_fence_array_set_pending_error(array, (*fence).error);
        if crate::memory::atomic_sub_and_test(1, &mut (*array).num_pending) {
            crate::irq_work::irq_work_queue(ptr::addr_of_mut!((*array).work));
        } else {
            dma_fence_put(ptr::addr_of_mut!((*array).base));
        }
    }
}

unsafe extern "C" fn dma_fence_array_enable_signaling(fence: *mut DmaFence) -> bool {
    let array = fence.cast::<DmaFenceArray>();
    for index in 0..unsafe { (*array).num_fences } {
        let callback = unsafe {
            ptr::addr_of_mut!((*array).callbacks)
                .cast::<DmaFenceArrayCallback>()
                .add(index as usize)
        };
        unsafe {
            (*callback).array = array;
            dma_fence_get(ptr::addr_of_mut!((*array).base));
            let child = *(*array).fences.add(index as usize);
            let err = crate::dma_fence::dma_fence_add_callback(
                child,
                ptr::addr_of_mut!((*callback).callback),
                dma_fence_array_callback,
            );
            if err != 0 {
                dma_fence_array_set_pending_error(array, (*child).error);
                dma_fence_put(ptr::addr_of_mut!((*array).base));
                if crate::memory::atomic_sub_and_test(1, &mut (*array).num_pending) {
                    dma_fence_array_clear_pending_error(array);
                    return false;
                }
            }
        }
    }
    true
}

unsafe extern "C" fn dma_fence_array_signaled(fence: *mut DmaFence) -> bool {
    let array = fence.cast::<DmaFenceArray>();
    let mut pending = unsafe { crate::memory::atomic_read(&(*array).num_pending) };
    if crate::bits::test_bit(DMA_FENCE_FLAG_ENABLE_SIGNAL_BIT, unsafe { &(*fence).flags }) {
        if pending <= 0 {
            unsafe { dma_fence_array_clear_pending_error(array) };
            return true;
        }
        return false;
    }
    for index in 0..unsafe { (*array).num_fences } {
        let child = unsafe { *(*array).fences.add(index as usize) };
        if unsafe { crate::dma_fence::dma_fence_is_signaled(child) } {
            pending -= 1;
            if pending == 0 {
                unsafe { dma_fence_array_clear_pending_error(array) };
                return true;
            }
        }
    }
    false
}

unsafe fn dma_fence_array_set_pending_error(array: *mut DmaFenceArray, error: i32) {
    if error != 0 {
        unsafe {
            let err = AtomicI32::from_ptr(ptr::addr_of_mut!((*array).base.error));
            let _ = err.compare_exchange(
                DMA_FENCE_ARRAY_PENDING_ERROR,
                error,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
    }
}

unsafe fn dma_fence_array_clear_pending_error(array: *mut DmaFenceArray) {
    unsafe {
        let err = AtomicI32::from_ptr(ptr::addr_of_mut!((*array).base.error));
        let _ = err.compare_exchange(
            DMA_FENCE_ARRAY_PENDING_ERROR,
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

unsafe extern "C" fn dma_fence_array_release(fence: *mut DmaFence) {
    let array = fence.cast::<DmaFenceArray>();
    unsafe {
        for index in 0..(*array).num_fences {
            dma_fence_put(*(*array).fences.add(index as usize));
        }
        crate::memory::kfree((*array).fences);
        crate::dma_fence::dma_fence_free(fence);
    }
}

unsafe extern "C" fn dma_fence_array_set_deadline(fence: *mut DmaFence, deadline: i64) {
    let array = fence.cast::<DmaFenceArray>();
    unsafe {
        for index in 0..(*array).num_fences {
            dma_fence_set_deadline(*(*array).fences.add(index as usize), deadline);
        }
    }
}

const _: [(); 32] = [(); core::mem::size_of::<DmaFenceArrayCallback>()];
const _: [(); 112] = [(); core::mem::size_of::<DmaFenceArray>()];
const _: [(); 80] = [(); core::mem::offset_of!(DmaFenceArray, work)];
const _: [(); 112] = [(); core::mem::offset_of!(DmaFenceArray, callbacks)];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dma_fence::{
        dma_fence_init, dma_fence_is_signaled, dma_fence_put, dma_fence_signal,
    };

    static TEST_OPS: DmaFenceOps = DmaFenceOps {
        get_driver_name: None,
        get_timeline_name: None,
        enable_signaling: None,
        signaled: None,
        wait: None,
        release: None,
        set_deadline: None,
    };

    #[test]
    fn owned_array_copies_vec_storage_to_kernel_allocation() {
        let _scheduler = crate::test_support::scheduler_context();
        // SAFETY: `dma_fence_init` initializes this C-layout record before use.
        let mut fence = unsafe { core::mem::zeroed::<DmaFence>() };
        // SAFETY: the local fence and static ops table remain live throughout
        // the test; a null lock requests the embedded lock.
        unsafe {
            dma_fence_init(
                &mut fence,
                ptr::addr_of!(TEST_OPS),
                ptr::null_mut(),
                4,
                5,
            )
        };
        // SAFETY: the initialized local fence has one owned reference.
        let member = dma_fence_get(&mut fence);
        let members = alloc::vec![member];
        let vec_storage = members.as_ptr() as *mut *mut DmaFence;

        // SAFETY: `members` transfers its live fence reference to the array.
        let array = unsafe { dma_fence_array_create_owned(members, 6, 7) };
        assert!(!array.is_null());
        // The array must not retain a Vec allocation that is freed on return.
        assert_ne!(unsafe { (*array).fences }, vec_storage);
        assert_eq!(unsafe { *(*array).fences }, &mut fence as *mut DmaFence);
        assert_eq!(unsafe { (*array).num_fences }, 1);

        // Exercise the composite signaled hook, then release the array and
        // verify it drops the member reference and its owned pointer buffer.
        let base = unsafe { ptr::addr_of_mut!((*array).base) };
        // SAFETY: both fences are initialized and the array retains its member.
        assert!(!unsafe { dma_fence_is_signaled(base) });
        // SAFETY: the initialized member has an inline lock and no other owner
        // mutates it in this single-threaded test.
        unsafe { dma_fence_signal(&mut fence) };
        // SAFETY: the live array observes that its only member completed and
        // publishes its own completion before returning true.
        assert!(unsafe { dma_fence_is_signaled(base) });
        // SAFETY: the array's initial reference is still owned by this test;
        // its release hook frees the member buffer and drops the extra ref.
        unsafe { dma_fence_put(base) };
        assert_eq!(fence.refcount.refcount.refs.counter, 1);
    }
}
