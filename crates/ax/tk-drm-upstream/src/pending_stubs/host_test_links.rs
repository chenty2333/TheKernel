// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Host-test link stubs for C-ABI symbols whose real definitions live in
//! tk-intel-gt (`linux/drm_core.rs`, the DRM-core provider seam) or in
//! tk-linux-kapi (the dma-fence core, which this crate's test binary links for
//! real). The kernel image links the real definitions; these are compiled only
//! under `cfg(test)`. Delete them if the tests stop reaching the symbols.

#![allow(non_snake_case, dead_code, unsafe_code)]
#![allow(clippy::missing_safety_doc)]

use core::ffi::c_void;

use tk_linux_kapi::config::GFP_KERNEL;
use tk_linux_kapi::memory::{kfree, kmalloc};
use tk_linux_kapi::types::DmaFence;

use crate::drm_prime_upstream::{DmaBuf, DmaBufAttachment};

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

unsafe extern "C" fn host_test_fence_release(_kref: *mut tk_linux_kapi::types::Kref) {}

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
kernel_only_test_service!(thekernel_eventfd_signal(ctx: *mut c_void, count: u64) -> i32);
kernel_only_test_service!(thekernel_eventfd_ctx_put(ctx: *mut c_void) -> ());
kernel_only_test_service!(thekernel_eventfd_ctx_fdget(fd: i32) -> *mut c_void);
kernel_only_test_service!(thekernel_drm_syncobj_import_fd(fd: i32) -> *mut c_void);
kernel_only_test_service!(thekernel_drm_syncobj_export_fd(syncobj: *mut c_void) -> i32);
