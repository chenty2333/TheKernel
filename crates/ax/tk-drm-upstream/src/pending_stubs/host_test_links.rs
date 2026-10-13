// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Host-test link stubs for C-ABI symbols whose real definitions live in
//! tk-intel-gt (`linux/dma_fence_core.rs`, `i915_gem_clflush_upstream.rs`,
//! `linux/drm_core.rs`). The kernel image links the tk-intel-gt definitions;
//! this crate's own test binary does not link tk-intel-gt, so these are
//! compiled only under `cfg(test)`. Delete them if the tests stop reaching the
//! symbols.

#![allow(non_snake_case, dead_code, unsafe_code)]
#![allow(clippy::missing_safety_doc)]

use core::ffi::c_void;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

pub static DMA_FENCE_INIT_COUNT: AtomicUsize = AtomicUsize::new(0);
pub static DMA_FENCE_GET_COUNT: AtomicUsize = AtomicUsize::new(0);
pub static DMA_FENCE_PUT_COUNT: AtomicUsize = AtomicUsize::new(0);

pub fn reset_dma_fence_counts() {
    DMA_FENCE_INIT_COUNT.store(0, Ordering::SeqCst);
    DMA_FENCE_GET_COUNT.store(0, Ordering::SeqCst);
    DMA_FENCE_PUT_COUNT.store(0, Ordering::SeqCst);
}

pub fn dma_fence_init_count() -> usize {
    DMA_FENCE_INIT_COUNT.load(Ordering::SeqCst)
}

pub fn dma_fence_get_count() -> usize {
    DMA_FENCE_GET_COUNT.load(Ordering::SeqCst)
}

pub fn dma_fence_put_count() -> usize {
    DMA_FENCE_PUT_COUNT.load(Ordering::SeqCst)
}

use tk_linux_kapi::config::GFP_KERNEL;
use tk_linux_kapi::memory::{kfree, kmalloc};
use tk_linux_kapi::types::{DmaFence, Spinlock};

use crate::drm_prime_upstream::{DmaBuf, DmaBufAttachment};

static FENCE_CONTEXT: AtomicU64 = AtomicU64::new(1);

// upstream: drivers/dma-buf/dma-fence.c dma_fence_context_alloc()
#[cfg(test)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_context_alloc(num: u32) -> u64 {
    FENCE_CONTEXT.fetch_add(num as u64, Ordering::SeqCst)
}

// upstream: drivers/dma-buf/dma-fence.c dma_fence_init()
#[cfg(test)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_init(
    fence: *mut DmaFence,
    ops: *const c_void,
    lock: *mut Spinlock,
    context: u64,
    seqno: u64,
) {
    if !fence.is_null() {
        DMA_FENCE_INIT_COUNT.fetch_add(1, Ordering::SeqCst);
        unsafe {
            (*fence).ops = ops;
            (*fence).lock.extern_lock = lock;
            (*fence).context = context;
            (*fence).seqno = seqno;
            (*fence).flags = 0;
            (*fence).error = 0;
            tk_linux_kapi::memory::kref_init(core::ptr::addr_of_mut!((*fence).refcount));
        }
    }
}

// upstream: drivers/dma-buf/dma-buf.c dma_buf_attach()
#[cfg(test)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_buf_attach(dmabuf: *mut DmaBuf, dev: *mut c_void) -> *mut DmaBufAttachment {
    unsafe {
        let attach = kmalloc(core::mem::size_of::<DmaBufAttachment>(), GFP_KERNEL).cast::<DmaBufAttachment>();
        if attach.is_null() {
            return (-12isize) as *mut DmaBufAttachment;
        }
        core::ptr::write_bytes(attach, 0, 1);
        (*attach).dmabuf = dmabuf;
        (*attach).dev = dev;
        if !dmabuf.is_null() {
            if let Some(ops) = (*dmabuf).ops.as_ref() {
                if let Some(att) = ops.attach {
                    att(dmabuf, attach);
                }
            }
        }
        attach
    }
}

// upstream: drivers/dma-buf/dma-buf.c dma_buf_detach()
#[cfg(test)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_buf_detach(dmabuf: *mut DmaBuf, attach: *mut DmaBufAttachment) {
    unsafe {
        if !dmabuf.is_null() && !attach.is_null() {
            if let Some(ops) = (*dmabuf).ops.as_ref() {
                if let Some(det) = ops.detach {
                    det(dmabuf, attach);
                }
            }
        }
        if !attach.is_null() {
            kfree(attach.cast::<c_void>());
        }
    }
}

// upstream: drivers/dma-buf/dma-buf.c dma_buf_put()
#[cfg(test)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_buf_put(dmabuf: *mut DmaBuf) {
    unsafe {
        if !dmabuf.is_null() && (dmabuf as usize) >= 4096 {
            if (*dmabuf)._vmapping_pad > 1 {
                (*dmabuf)._vmapping_pad -= 1;
                return;
            }
            if let Some(ops) = (*dmabuf).ops.as_ref() {
                if let Some(rel) = ops.release {
                    rel(dmabuf);
                }
            }
            if !(*dmabuf).file.is_null() {
                kfree((*dmabuf).file.cast::<c_void>());
            }
            kfree(dmabuf.cast::<c_void>());
        }
    }
}

// upstream: drivers/dma-buf/dma-fence.c dma_fence_get()
#[cfg(test)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_get(fence: *mut DmaFence) -> *mut DmaFence {
    if !fence.is_null() {
        DMA_FENCE_GET_COUNT.fetch_add(1, Ordering::SeqCst);
        unsafe {
            tk_linux_kapi::memory::kref_get(core::ptr::addr_of_mut!((*fence).refcount));
        }
    }
    fence
}

unsafe extern "C" fn host_test_fence_release(_kref: *mut tk_linux_kapi::types::Kref) {}

// upstream: drivers/dma-buf/dma-fence.c dma_fence_put()
#[cfg(test)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_put(fence: *mut DmaFence) {
    if !fence.is_null() {
        DMA_FENCE_PUT_COUNT.fetch_add(1, Ordering::SeqCst);
        unsafe {
            tk_linux_kapi::memory::kref_put(
                core::ptr::addr_of_mut!((*fence).refcount),
                host_test_fence_release,
            );
        }
    }
}

#[cfg(test)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sync_file_create(_fence: *mut DmaFence) -> *mut c_void {
    core::ptr::null_mut()
}

#[cfg(test)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sync_file_get_fence(_fd: i32) -> *mut DmaFence {
    core::ptr::null_mut()
}

#[cfg(test)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_resv_get_singleton(
    _resv: *mut crate::dma_resv_types::DmaResv,
    _usage: i32,
    fence: *mut *mut DmaFence,
) -> i32 {
    if !fence.is_null() {
        unsafe { *fence = core::ptr::null_mut() };
    }
    0
}

#[cfg(test)]
#[unsafe(no_mangle)]
pub static dma_fence_chain_ops: u8 = 42;

#[cfg(test)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_wait(_fence: *mut tk_linux_kapi::types::DmaFence, _intr: bool) -> i32 {
    0
}

// The ioctl table retains syncobj entry points in this isolated component test
// binary. Their fence/VFS services belong to the kernel-linked tests, not this
// fixture. Trap if reached: linking this table must not falsely validate them.
macro_rules! kernel_only_test_service {
    ($name:ident($($arg:ident: $ty:ty),* $(,)?) -> $ret:ty) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name($($arg: $ty),*) -> $ret {
            $(let _ = $arg;)*
            panic!(concat!(stringify!($name), " requires the kernel-linked test harness"));
        }
    };
}

kernel_only_test_service!(dma_fence_chain_find_seqno(fence: *mut *mut DmaFence, point: u64) -> i32);
kernel_only_test_service!(dma_fence_is_signaled(fence: *mut DmaFence) -> bool);
kernel_only_test_service!(dma_fence_add_callback(
    fence: *mut DmaFence,
    callback: *mut tk_linux_kapi::types::DmaFenceCb,
    function: unsafe extern "C" fn(*mut DmaFence, *mut tk_linux_kapi::types::DmaFenceCb)
) -> i32);
kernel_only_test_service!(dma_fence_chain_walk(fence: *mut DmaFence) -> *mut DmaFence);
kernel_only_test_service!(dma_fence_array_create(count: u32, fences: *mut *mut DmaFence, context: u64, seqno: u32) -> *mut DmaFence);
kernel_only_test_service!(dma_fence_signal(fence: *mut DmaFence) -> i32);
kernel_only_test_service!(dma_fence_free(fence: *mut DmaFence) -> ());
kernel_only_test_service!(dma_fence_default_wait(fence: *mut DmaFence, interruptible: bool, timeout: core::ffi::c_long) -> core::ffi::c_long);
kernel_only_test_service!(thekernel_eventfd_signal(ctx: *mut c_void, count: u64) -> i32);
kernel_only_test_service!(thekernel_eventfd_ctx_put(ctx: *mut c_void) -> ());
kernel_only_test_service!(thekernel_eventfd_ctx_fdget(fd: i32) -> *mut c_void);
kernel_only_test_service!(thekernel_drm_syncobj_import_fd(fd: i32) -> *mut c_void);
kernel_only_test_service!(thekernel_drm_syncobj_export_fd(syncobj: *mut c_void) -> i32);
