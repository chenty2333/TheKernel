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

use crate::{
    intel_context_upstream::{IrqWork, Kref},
    intel_engine_cs_upstream::{
        AtomicT, IntelEngineCs, IntelEngineMask, ListHead, LlistHead, Spinlock,
    },
};

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

/// `struct intel_breadcrumbs` from `gt/intel_breadcrumbs_types.h`.
///
/// The target configuration has 64-bit pointers, `CONFIG_PREEMPT_RT=n`, and
/// lockdep/debug spinlock fields disabled. `irq_armed` is represented with a
/// nullable-pointer niche so its `Option` state occupies the same single
/// pointer word as upstream `intel_wakeref_t`.
#[repr(C)]
pub struct IntelBreadcrumbs {
    pub ref_: Kref,
    pub active: AtomicT,

    pub signalers_lock: Spinlock,
    pub signalers: ListHead,
    pub signaled_requests: LlistHead,
    pub signaler_active: AtomicT,

    pub irq_lock: Spinlock,
    pub irq_work: IrqWork,
    pub irq_enabled: u32,
    pub irq_armed: IntelWakerefT,

    pub engine_mask: IntelEngineMaskT,
    pub irq_engine: *mut IntelEngineCs,
    pub irq_enable: Option<unsafe fn(*mut Self) -> bool>,
    pub irq_disable: Option<unsafe fn(*mut Self)>,
}

/// Linux source code spells the struct tag `intel_breadcrumbs`; several
/// translated C sites retain that tag spelling.
#[allow(non_camel_case_types)]
pub type intel_breadcrumbs = IntelBreadcrumbs;

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

/// Values of `enum i915_request_state` from `i915_request.h`.
#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum I915RequestState {
    Unknown  = 0,
    Complete = 1,
    Pending  = 2,
    Queued   = 3,
    Active   = 4,
}

pub const I915_REQUEST_UNKNOWN: I915RequestState = I915RequestState::Unknown;
pub const I915_REQUEST_COMPLETE: I915RequestState = I915RequestState::Complete;
pub const I915_REQUEST_PENDING: I915RequestState = I915RequestState::Pending;
pub const I915_REQUEST_QUEUED: I915RequestState = I915RequestState::Queued;
pub const I915_REQUEST_ACTIVE: I915RequestState = I915RequestState::Active;

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

const _: [(); 128] = [(); size_of::<IntelBreadcrumbs>()];
const _: [(); 8] = [(); align_of::<IntelBreadcrumbs>()];
const _: [(); 0] = [(); offset_of!(IntelBreadcrumbs, r#ref)];
const _: [(); 4] = [(); offset_of!(IntelBreadcrumbs, active)];
const _: [(); 8] = [(); offset_of!(IntelBreadcrumbs, signalers_lock)];
const _: [(); 16] = [(); offset_of!(IntelBreadcrumbs, signalers)];
const _: [(); 32] = [(); offset_of!(IntelBreadcrumbs, signaled_requests)];
const _: [(); 40] = [(); offset_of!(IntelBreadcrumbs, signaler_active)];
const _: [(); 44] = [(); offset_of!(IntelBreadcrumbs, irq_lock)];
const _: [(); 48] = [(); offset_of!(IntelBreadcrumbs, irq_work)];
const _: [(); 80] = [(); offset_of!(IntelBreadcrumbs, irq_enabled)];
const _: [(); 88] = [(); offset_of!(IntelBreadcrumbs, irq_armed)];
const _: [(); 96] = [(); offset_of!(IntelBreadcrumbs, engine_mask)];
const _: [(); 104] = [(); offset_of!(IntelBreadcrumbs, irq_engine)];
const _: [(); 112] = [(); offset_of!(IntelBreadcrumbs, irq_enable)];
const _: [(); 120] = [(); offset_of!(IntelBreadcrumbs, irq_disable)];

const _: [(); 1552] = [(); size_of::<IntelInstdone>()];
const _: [(); 16] = [(); offset_of!(IntelInstdone, sampler)];
const _: [(); 528] = [(); offset_of!(IntelInstdone, row)];
const _: [(); 1040] = [(); offset_of!(IntelInstdone, geom_svg)];
