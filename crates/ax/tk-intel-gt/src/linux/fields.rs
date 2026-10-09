// SPDX-License-Identifier: MIT
// Copyright © 2026 TheKernel contributors.
// Linux v7.2.3 header-owned shared records used by the i915 GT translations.
//
// This file contains ABI descriptions only. It does not emulate the Linux
// synchronization, lifetime, allocation, or scheduling operations acting on
// these records.

use core::{
    ffi::{c_char, c_void},
    mem::{align_of, offset_of, size_of},
    ptr::NonNull,
};

// The LinuxKPI field prelude re-exports the canonical i915 header record; do
// not keep a second, subtly different breadcrumb layout here.
pub use crate::intel_breadcrumbs_types_upstream::{IntelBreadcrumbs, intel_breadcrumbs};
use crate::{
    intel_context_upstream::{DrmI915GemObject, IrqWork, Kref},
    intel_engine_cs_upstream::{
        AtomicT, IntelEngineCs, IntelEngineMask, ListHead, LlistHead, Spinlock,
    },
};

// Accessors for the packed bitfields at the exact source offsets in
// `DrmI915GemObjectPrefix`. They preserve adjacent PAT/coherency bits when
// updating `cache_dirty`, unlike treating the source bitfield as a byte.
#[inline]
pub unsafe fn i915_gem_object_cache_dirty(obj: *const DrmI915GemObject) -> bool {
    assert!(!obj.is_null());
    unsafe { (*obj).cache_state_bits & (1 << 9) != 0 }
}

#[inline]
pub unsafe fn i915_gem_object_set_cache_dirty(obj: *mut DrmI915GemObject, dirty: bool) {
    assert!(!obj.is_null());
    let bits = unsafe { &mut (*obj).cache_state_bits };
    if dirty {
        *bits |= 1 << 9;
    } else {
        *bits &= !(1 << 9);
    }
}

#[inline]
pub unsafe fn i915_gem_object_cache_coherent(obj: *const DrmI915GemObject) -> u32 {
    assert!(!obj.is_null());
    ((unsafe { (*obj).cache_state_bits } >> 7) & 0x3) as u32
}

#[inline]
pub unsafe fn i915_gem_object_is_dpt(obj: *const DrmI915GemObject) -> bool {
    assert!(!obj.is_null());
    unsafe { (*obj).cache_state_bits & (1 << 10) != 0 }
}

#[inline]
pub unsafe fn i915_gem_object_pat_set_by_user(obj: *const DrmI915GemObject) -> bool {
    assert!(!obj.is_null());
    unsafe { (*obj).cache_state_bits & (1 << 6) != 0 }
}

/// `i915_gem_object_is_framebuffer()` from gem/i915_gem_object.h.
pub unsafe fn i915_gem_object_is_framebuffer(obj: *const DrmI915GemObject) -> bool {
    assert!(!obj.is_null());
    let frontbuffer = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*obj).frontbuffer)) };
    !frontbuffer.is_null() || unsafe { i915_gem_object_is_dpt(obj) }
}

/// `i915_gem_object_get_tiling()` and `get_stride()` from gem/i915_gem_object.h.
pub unsafe fn i915_gem_object_get_tiling(obj: *const DrmI915GemObject) -> u32 {
    assert!(!obj.is_null());
    unsafe { (*obj).tiling_and_stride & 0x7f }
}

pub unsafe fn i915_gem_object_get_stride(obj: *const DrmI915GemObject) -> u32 {
    assert!(!obj.is_null());
    unsafe { (*obj).tiling_and_stride & !0x7f }
}

#[inline]
pub unsafe fn i915_gem_object_is_tiled(obj: *const DrmI915GemObject) -> bool {
    unsafe { i915_gem_object_get_tiling(obj) != 0 }
}

/// Source `i915_gem_object_has_cache_level()` fast check. A user-specified PAT
/// index deliberately bypasses cache-level comparison; otherwise compare the
/// asserted per-device cachelevel-to-PAT table.
pub unsafe fn i915_gem_object_has_cache_level(obj: *const DrmI915GemObject, level: u32) -> bool {
    assert!(!obj.is_null());
    if unsafe { i915_gem_object_pat_set_by_user(obj) } {
        return true;
    }
    let i915 = unsafe { crate::linux::i915::to_i915((&(*obj).base.base).dev) };
    let pat_index = unsafe { (*obj).cache_state_bits & 0x3f };
    pat_index == unsafe { crate::linux::i915::i915_gem_get_pat_index(i915, level) }
}

/// Opaque forward declaration of `struct i915_gpu_coredump`.
#[repr(C)]
pub struct I915GpuCoredump {
    _opaque: [u8; 0],
}

/// `intel_wakeref_t` from `drivers/gpu/drm/i915/intel_wakeref.h`.
///
/// It is a nullable pointer to `struct ref_tracker`, not the embedded
/// `struct intel_wakeref` used by `intel_engine_cs`.
pub type IntelWakerefT = Option<NonNull<c_void>>;

/// `ktime_t` from `include/linux/ktime.h` on the source x86_64 ABI.
pub type KtimeT = i64;

/// Rust spelling used for the same source `ktime_t` value by the engine file.
pub type Ktime = KtimeT;

/// `intel_engine_mask_t` from `gt/intel_engine_types.h`.
pub type IntelEngineMaskT = IntelEngineMask;

/// `struct intel_instdone` from `gt/intel_engine_types.h`; the bounds follow
/// `GEN_MAX_GSLICES == 16` and `I915_MAX_SUBSLICES == 8` in `gt/intel_sseu.h`.
#[repr(C)]
pub struct IntelInstdone {
    pub instdone: u32,
    pub slice_common: u32,
    pub slice_common_extra: [u32; 2],
    pub sampler: [[u32; 8]; 16],
    pub row: [[u32; 8]; 16],
    pub geom_svg: [[u32; 8]; 16],
}

// The i915 request state enum and exported values are owned by the Linux 7.2.3
// `i915_request.h` binding; do not maintain a duplicate enum here.
pub use crate::i915_request_types_upstream::{
    I915_REQUEST_ACTIVE, I915_REQUEST_COMPLETE, I915_REQUEST_PENDING, I915_REQUEST_QUEUED,
    I915_REQUEST_UNKNOWN, I915RequestState,
};

/// `struct lock_class_key` with `CONFIG_LOCKDEP=n` in the task's Linux config.
/// Linux declares this as an empty C extension struct in that configuration;
/// its Rust representation is consequently zero-sized and is never embedded
/// in any runtime record here.
#[repr(C)]
pub struct LockClassKey {}

/// `va_format` from include/linux/kernel.h, used by DRM printer callbacks.
#[repr(C)]
pub struct VaFormat {
    pub fmt: *const c_char,
    pub va: *mut VaList,
}

/// Opaque C `va_list` storage; it is passed through to the configured printer
/// callback but never inspected or formatted by Rust.
#[repr(C)]
pub struct VaList {
    _opaque: [u8; 0],
}

/// `struct drm_printer` from include/drm/drm_print.h. Its callbacks and opaque
/// argument are the Linux ABI boundary; keep Rust-owned buffers out of this
/// record so source pointers retain the exact C layout.
#[repr(C)]
pub struct DrmPrinter {
    pub printfn: Option<unsafe extern "C" fn(*mut DrmPrinter, *mut VaFormat)>,
    pub puts: Option<unsafe extern "C" fn(*mut DrmPrinter, *const c_char)>,
    pub arg: *mut c_void,
    pub origin: *const c_void,
    pub prefix: *const c_char,
    pub line: DrmPrinterLine,
}

#[repr(C)]
pub struct DrmPrinterLine {
    pub series: u32,
    pub counter: u32,
}

const _: [(); 48] = [(); size_of::<DrmPrinter>()];
const _: [(); 40] = [(); offset_of!(DrmPrinter, line)];
const _: [(); 8] = [(); size_of::<DrmPrinterLine>()];

/// Forward-declared in `i915_gpu_error.h`; the execlists translation only
/// passes/tests this opaque pointer and does not access its private fields.
#[repr(C)]
pub struct IntelEngineCaptureVma {}

const _: [(); 1552] = [(); size_of::<IntelInstdone>()];
const _: [(); 16] = [(); offset_of!(IntelInstdone, sampler)];
const _: [(); 528] = [(); offset_of!(IntelInstdone, row)];
const _: [(); 1040] = [(); offset_of!(IntelInstdone, geom_svg)];
