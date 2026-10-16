// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Kernel-side hooks for the two dma-fence entry points whose alias and
//! timeline state is owned by the native DRM stack rather than by LinuxKPI:
//! `dma_resv_add_fence()` mirrors each insertion into the native alias graph,
//! and `dma_fence_chain_find_seqno()` offers a timeline point to the native
//! syncobj fences first.
//!
//! Until installed, the alias mirror does nothing and the timeline probe
//! declines (returns 1), so LinuxKPI's ordinary chain walk runs.

#![allow(unsafe_code)]

use core::{
    ffi::{c_int, c_void},
    sync::atomic::{AtomicPtr, Ordering},
};

/// Native DRM-stack callbacks. Pointers are the native stack's own objects.
#[repr(C)]
pub struct NativeFenceHooks {
    /// Mirror a reservation insertion into the native alias graph.
    pub resv_alias_add: unsafe extern "C" fn(resv: *mut c_void, fence: *mut c_void, usage: u32),
    /// Resolve a timeline point on a native syncobj fence. Returns 1 when the
    /// fence is not native, so the caller's chain walk handles it; otherwise
    /// 0 or a negative errno, with `*fence` replaced by the resolved fence.
    pub timeline_fence_find: unsafe extern "C" fn(fence: *mut *mut c_void, point: u64) -> c_int,
}

static NATIVE_FENCE_HOOKS: AtomicPtr<NativeFenceHooks> = AtomicPtr::new(core::ptr::null_mut());

/// Install the native fence hooks. They can be installed once.
pub fn install_native_fence_hooks(hooks: &'static NativeFenceHooks) -> Result<(), &'static str> {
    NATIVE_FENCE_HOOKS
        .compare_exchange(
            core::ptr::null_mut(),
            core::ptr::from_ref(hooks).cast_mut(),
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .map(|_| ())
        .map_err(|_| "native fence hooks already installed")
}

/// The installed hooks, if any.
pub(crate) fn native_fence_hooks() -> Option<&'static NativeFenceHooks> {
    let hooks = NATIVE_FENCE_HOOKS.load(Ordering::Acquire);
    // SAFETY: only `install_native_fence_hooks` stores a non-null `'static` reference.
    unsafe { hooks.as_ref() }
}
