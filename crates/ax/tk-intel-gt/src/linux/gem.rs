// SPDX-License-Identifier: MIT
// Copyright © 2026 TheKernel contributors.
// Linux v7.2.3 DRM/i915 GEM shared layout records.

use core::mem::{align_of, offset_of, size_of};

use crate::{
    intel_context_upstream::{DrmGemObjectBaseLayout, I915GemContext},
    intel_engine_cs_upstream::ListHead,
};

/// The C type name used by DRM/i915 call sites for the 480-byte base union.
/// Accessed `drm_gem_object` fields use the Linux 7.2.3 x86_64 record layout;
/// opaque storage preserves the larger `ttm_buffer_object` union size.
pub type DrmGemObject = DrmGemObjectBaseLayout;

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

const _: [(); 32] = [(); size_of::<I915LutHandle>()];
const _: [(); 8] = [(); align_of::<I915LutHandle>()];
const _: [(); 0] = [(); offset_of!(I915LutHandle, obj_link)];
const _: [(); 16] = [(); offset_of!(I915LutHandle, ctx)];
const _: [(); 24] = [(); offset_of!(I915LutHandle, handle)];
