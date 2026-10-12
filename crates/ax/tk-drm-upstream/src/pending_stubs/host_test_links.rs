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
use core::sync::atomic::{AtomicU64, Ordering};

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
    unsafe {
        (*fence).ops = ops;
        (*fence).lock.extern_lock = lock;
        (*fence).context = context;
        (*fence).seqno = seqno;
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
        if !dmabuf.is_null() {
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
