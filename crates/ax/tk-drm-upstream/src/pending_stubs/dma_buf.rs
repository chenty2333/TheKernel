// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! TEMPORARY definitions for dma-buf core (drivers/dma-buf/).
//! Replaced by dma_buf_upstream.rs when the dma-buf batch lands.

#![allow(non_snake_case, dead_code, unsafe_code)]
#![allow(clippy::missing_safety_doc)]

use core::ffi::{c_int, c_void};
use core::sync::atomic::{AtomicPtr, Ordering};
use tk_linux_kapi::config::{EINVAL, ENOMEM, GFP_KERNEL};
use tk_linux_kapi::memory::{kfree, kmalloc};
use tk_linux_kapi::types::{File, SgTable};

use crate::drm_prime_upstream::{DmaBuf, DmaBufAttachment, DmaBufExportInfo};

const MAX_FD: usize = 256;
static DMABUF_FD_MAP: [AtomicPtr<DmaBuf>; MAX_FD] = [const { AtomicPtr::new(core::ptr::null_mut()) }; MAX_FD];

#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_buf_register_fd(fd: c_int, dmabuf: *mut DmaBuf) {
    if fd >= 0 && (fd as usize) < MAX_FD {
        DMABUF_FD_MAP[fd as usize].store(dmabuf, Ordering::SeqCst);
    }
}

// upstream: drivers/dma-buf/dma-buf.c dma_buf_export()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_buf_export(exp_info: *const DmaBufExportInfo) -> *mut DmaBuf {
    unsafe {
        if exp_info.is_null() {
            return (-EINVAL as isize) as *mut DmaBuf;
        }
        let dmabuf = kmalloc(core::mem::size_of::<DmaBuf>(), GFP_KERNEL).cast::<DmaBuf>();
        if dmabuf.is_null() {
            return (-ENOMEM as isize) as *mut DmaBuf;
        }
        core::ptr::write_bytes(dmabuf, 0, 1);
        (*dmabuf).size = (*exp_info).size;
        (*dmabuf).ops = (*exp_info).ops;
        (*dmabuf).r#priv = (*exp_info).r#priv;
        (*dmabuf).resv = (*exp_info).resv;
        (*dmabuf)._vmapping_pad = 1;

        let filp = kmalloc(core::mem::size_of::<File>(), GFP_KERNEL).cast::<File>();
        if !filp.is_null() {
            core::ptr::write_bytes(filp, 0, 1);
            (*filp).private_data = dmabuf.cast();
        }
        (*dmabuf).file = filp;

        dmabuf
    }
}

// upstream: drivers/dma-buf/dma-buf.c dma_buf_get()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_buf_get(fd: c_int) -> *mut DmaBuf {
    if fd >= 0 && (fd as usize) < MAX_FD {
        let ptr = DMABUF_FD_MAP[fd as usize].load(Ordering::SeqCst);
        if !ptr.is_null() {
            if (ptr as usize) >= 4096 {
                (*ptr)._vmapping_pad = (*ptr)._vmapping_pad.saturating_add(1);
            }
            return ptr;
        }
    }
    // upstream: drivers/dma-buf/dma-buf.c dma_buf_get() ERR_PTR(-EBADF)
    (-9isize) as *mut DmaBuf
}

// upstream: drivers/dma-buf/dma-buf.c dma_buf_map_attachment_unlocked()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_buf_map_attachment_unlocked(
    attach: *mut DmaBufAttachment,
    direction: c_int,
) -> *mut SgTable {
    unsafe {
        if attach.is_null() || (*attach).dmabuf.is_null() {
            return (-EINVAL as isize) as *mut SgTable;
        }
        let dmabuf = (*attach).dmabuf;
        if let Some(ops) = (*dmabuf).ops.as_ref() {
            if let Some(map) = ops.map_dma_buf {
                return map(attach, direction);
            }
        }
        // upstream: drivers/dma-buf/dma-buf.c missing map_dma_buf error fallback
        (-EINVAL as isize) as *mut SgTable
    }
}

// upstream: drivers/dma-buf/dma-buf.c dma_buf_unmap_attachment_unlocked()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_buf_unmap_attachment_unlocked(
    attach: *mut DmaBufAttachment,
    sgt: *mut SgTable,
    direction: c_int,
) {
    unsafe {
        if !attach.is_null() && !(*attach).dmabuf.is_null() {
            let dmabuf = (*attach).dmabuf;
            if let Some(ops) = (*dmabuf).ops.as_ref() {
                if let Some(unmap) = ops.unmap_dma_buf {
                    unmap(attach, sgt, direction);
                }
            }
        }
    }
}

// upstream: drivers/dma-buf/dma-buf.c dma_buf_begin_cpu_access()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_buf_begin_cpu_access(dmabuf: *mut DmaBuf, direction: c_int) -> c_int {
    unsafe {
        if dmabuf.is_null() {
            return -EINVAL;
        }
        if let Some(ops) = (*dmabuf).ops.as_ref() {
            if let Some(b) = ops.begin_cpu_access {
                return b(dmabuf, direction);
            }
        }
        0
    }
}

// upstream: drivers/dma-buf/dma-buf.c dma_buf_end_cpu_access()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_buf_end_cpu_access(dmabuf: *mut DmaBuf, direction: c_int) -> c_int {
    unsafe {
        if dmabuf.is_null() {
            return -EINVAL;
        }
        if let Some(ops) = (*dmabuf).ops.as_ref() {
            if let Some(e) = ops.end_cpu_access {
                return e(dmabuf, direction);
            }
        }
        0
    }
}

// upstream: drivers/dma-buf/dma-fence-chain.c dma_fence_chain_alloc()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_chain_alloc() -> *mut tk_linux_kapi::types::DmaFenceChain {
    tk_linux_kapi::memory::kzalloc(
        core::mem::size_of::<tk_linux_kapi::types::DmaFenceChain>(),
        GFP_KERNEL,
    )
    .cast()
}

// Production dma_fence_set_deadline is provided by tk-intel-gt.
#[cfg(test)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_set_deadline(fence: *mut tk_linux_kapi::types::DmaFence, deadline: i64) {
    if fence.is_null() { return; }
    let ops = unsafe { (*fence).ops.cast::<tk_linux_kapi::types::DmaFenceOps>() };
    if !ops.is_null() {
        if let Some(set_deadline) = unsafe { (*ops).set_deadline } {
            unsafe { set_deadline(fence, deadline) };
        }
    }
}
