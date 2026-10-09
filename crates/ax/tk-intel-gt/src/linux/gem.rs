// SPDX-License-Identifier: MIT
// Copyright © 2026 TheKernel contributors.
// Linux v7.2.3 DRM/i915 GEM shared layout records.

use core::{
    ffi::c_void,
    mem::{align_of, offset_of, size_of},
};

use crate::{
    i915_gem_context_types_upstream::I915GemContext,
    i915_gem_ww_upstream::WwAcquireCtx,
    intel_context_upstream::DrmGemObjectBaseLayout,
    intel_engine_cs_upstream::ListHead,
    linux::ww_mutex::{
        WwMutex, ww_mutex_lock, ww_mutex_lock_slow, ww_mutex_trylock, ww_mutex_unlock,
    },
};

/// The C type name used by DRM/i915 call sites for the 480-byte base union.
/// Accessed `drm_gem_object` fields use the Linux 7.2.3 x86_64 record layout;
/// opaque storage preserves the larger `ttm_buffer_object` union size.
pub type DrmGemObject = DrmGemObjectBaseLayout;

/// Linux v7.2.3 `struct dma_resv` for CONFIG_PREEMPT_RT=n and LOCKDEP=n.
#[repr(C)]
pub struct DmaResv {
    pub lock: WwMutex,
    pub fences: *mut c_void,
}

const _: [(); 40] = [(); size_of::<DmaResv>()];
const _: [(); 0] = [(); offset_of!(DmaResv, lock)];
const _: [(); 32] = [(); offset_of!(DmaResv, fences)];

/// `dma_resv_lock()` source-order API. The current task model has no POSIX
/// signal delivery; interruptible callers otherwise use the same wait path.
pub unsafe fn dma_resv_lock(
    resv: *mut c_void,
    ctx: *mut WwAcquireCtx,
    _intr: bool,
    try_only: bool,
) -> i32 {
    let lock = unsafe { core::ptr::addr_of_mut!((*resv.cast::<DmaResv>()).lock) };
    if try_only {
        if unsafe { ww_mutex_trylock(lock, ctx) } {
            0
        } else {
            -crate::linux_config::EBUSY
        }
    } else {
        unsafe { ww_mutex_lock(lock, ctx) }
    }
}

/// `dma_resv_lock_slow()` after the WW caller has dropped all other locks.
pub unsafe fn dma_resv_lock_slow(resv: *mut c_void, ctx: *mut WwAcquireCtx) {
    let lock = unsafe { core::ptr::addr_of_mut!((*resv.cast::<DmaResv>()).lock) };
    unsafe { ww_mutex_lock_slow(lock, ctx) };
}

/// Interruptible source API. Signals are not part of the active task model.
pub unsafe fn dma_resv_lock_slow_interruptible(resv: *mut c_void, ctx: *mut WwAcquireCtx) -> i32 {
    unsafe { dma_resv_lock_slow(resv, ctx) };
    0
}

/// `dma_resv_unlock()` releases the underlying WW mutex.
pub unsafe fn dma_resv_unlock(resv: *mut c_void) {
    let lock = unsafe { core::ptr::addr_of_mut!((*resv.cast::<DmaResv>()).lock) };
    unsafe { ww_mutex_unlock(lock) };
}

/// The `struct drm_device` member used by `drm_dev_to_dev()`.
///
/// Linux v7.2.3 places `dev` after `if_version` and the embedded `kref`;
/// both fields occupy eight bytes in the configured x86_64 ABI.
#[repr(C)]
pub struct DrmDevice {
    _if_version: i32,
    _pad: u32,
    _ref: crate::intel_context_upstream::Kref,
    pub dev: *mut core::ffi::c_void,
}

#[inline]
pub unsafe fn drm_device_device(drm: *mut core::ffi::c_void) -> *mut core::ffi::c_void {
    unsafe { (*drm.cast::<DrmDevice>()).dev }
}

/// Opaque TTM arm of `drm_i915_gem_object.base`; target-configured x86_64
/// storage is 480 bytes and the i915 owner does not access TTM-private fields.
#[repr(C, align(8))]
pub struct TtmBufferObjectLayout {
    /// The DRM GEM base is the leading TTM member; the remaining TTM-private
    /// payload is not accessed by this GT/GEM translation.
    pub base: DrmGemObjectBaseLayout,
}
const _: [(); 480] = [(); size_of::<TtmBufferObjectLayout>()];
const _: [(); 8] = [(); align_of::<TtmBufferObjectLayout>()];

/// Source-layout prefix through `drm_file::driver_priv`, the member used by
/// GEM handle close paths. Later DRM file fields are not accessed here.
#[repr(C)]
pub struct DrmFile {
    _prefix: [u8; 136],
    pub driver_priv: *mut core::ffi::c_void,
}

const _: [(); 136] = [(); offset_of!(DrmFile, driver_priv)];

/// `struct i915_lut_handle` from `gem/i915_gem_object_types.h`.
#[repr(C)]
pub struct I915LutHandle {
    pub obj_link: ListHead,
    pub ctx: *mut I915GemContext,
    pub handle: u32,
}

const _: [(); 480] = [(); size_of::<DrmGemObject>()];
const _: [(); 8] = [(); align_of::<DrmGemObject>()];
const _: [(); 8] = [(); offset_of!(DrmGemObject, dev)];
const _: [(); 24] = [(); size_of::<DrmDevice>()];
const _: [(); 16] = [(); offset_of!(DrmDevice, dev)];

const _: [(); 32] = [(); size_of::<I915LutHandle>()];
const _: [(); 8] = [(); align_of::<I915LutHandle>()];
const _: [(); 0] = [(); offset_of!(I915LutHandle, obj_link)];
const _: [(); 16] = [(); offset_of!(I915LutHandle, ctx)];
const _: [(); 24] = [(); offset_of!(I915LutHandle, handle)];
