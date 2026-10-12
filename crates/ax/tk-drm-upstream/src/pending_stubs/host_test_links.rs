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

use crate::drm_file_types::DrmFile;
use crate::drm_gem_types::DrmGemObject;
use tk_linux_kapi::types::{DmaFence, Spinlock};

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

#[cfg(test)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_handle_create(_file_priv: *mut DrmFile, _obj: *mut c_void, _handlep: *mut u32) -> i32 { -tk_linux_kapi::config::EOPNOTSUPP }

// upstream: drm_auth.c (no owner yet)
#[cfg(test)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_is_current_master(_file_priv: *mut DrmFile) -> bool {
    true
}

// upstream: drivers/dma-buf/dma-fence.c dma_fence_put()
#[cfg(test)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_put(_fence: *mut DmaFence) {}

#[cfg(test)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_dev_get(dev: *mut crate::drm_device_types::DrmDevice) -> *mut crate::drm_device_types::DrmDevice {
    dev
}

#[cfg(test)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_dev_put(_dev: *mut crate::drm_device_types::DrmDevice) {}

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
