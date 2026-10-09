// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
// Source-order type transcription of Linux 7.2.3
// drivers/gpu/drm/i915/gt/intel_breadcrumbs_types.h.
//
// This independent header binding does not alias the earlier LinuxKPI
// `IntelBreadcrumbs` record. Root integration must switch breadcrumb and
// engine field imports together.

use crate::{
    intel_context_types_upstream::intel_wakeref_t,
    intel_context_upstream::{IrqWork, Kref},
    intel_engine_cs_upstream::{
        AtomicT, IntelEngineCs, IntelEngineMask, ListHead, LlistHead, Spinlock,
    },
};

/// `struct intel_breadcrumbs`.
#[repr(C)]
pub struct IntelBreadcrumbs {
    pub r#ref: Kref,
    pub active: AtomicT,
    pub signalers_lock: Spinlock,
    pub signalers: ListHead,
    pub signaled_requests: LlistHead,
    pub signaler_active: AtomicT,
    pub irq_lock: Spinlock,
    pub irq_work: IrqWork,
    pub irq_enabled: u32,
    pub irq_armed: intel_wakeref_t,
    pub engine_mask: IntelEngineMask,
    pub irq_engine: *mut IntelEngineCs,
    pub irq_enable: Option<unsafe extern "C" fn(*mut IntelBreadcrumbs) -> bool>,
    pub irq_disable: Option<unsafe extern "C" fn(*mut IntelBreadcrumbs)>,
}

#[allow(non_camel_case_types)]
pub type intel_breadcrumbs = IntelBreadcrumbs;

// Linux 7.2.3 x86_64, non-RT spinlock / 64-bit pointer layout.
const _: [(); 128] = [(); core::mem::size_of::<IntelBreadcrumbs>()];
const _: [(); 8] = [(); core::mem::align_of::<IntelBreadcrumbs>()];
const _: [(); 0] = [(); core::mem::offset_of!(IntelBreadcrumbs, r#ref)];
const _: [(); 4] = [(); core::mem::offset_of!(IntelBreadcrumbs, active)];
const _: [(); 8] = [(); core::mem::offset_of!(IntelBreadcrumbs, signalers_lock)];
const _: [(); 16] = [(); core::mem::offset_of!(IntelBreadcrumbs, signalers)];
const _: [(); 32] = [(); core::mem::offset_of!(IntelBreadcrumbs, signaled_requests)];
const _: [(); 40] = [(); core::mem::offset_of!(IntelBreadcrumbs, signaler_active)];
const _: [(); 44] = [(); core::mem::offset_of!(IntelBreadcrumbs, irq_lock)];
const _: [(); 48] = [(); core::mem::offset_of!(IntelBreadcrumbs, irq_work)];
const _: [(); 80] = [(); core::mem::offset_of!(IntelBreadcrumbs, irq_enabled)];
const _: [(); 88] = [(); core::mem::offset_of!(IntelBreadcrumbs, irq_armed)];
const _: [(); 96] = [(); core::mem::offset_of!(IntelBreadcrumbs, engine_mask)];
const _: [(); 104] = [(); core::mem::offset_of!(IntelBreadcrumbs, irq_engine)];
const _: [(); 112] = [(); core::mem::offset_of!(IntelBreadcrumbs, irq_enable)];
const _: [(); 120] = [(); core::mem::offset_of!(IntelBreadcrumbs, irq_disable)];
