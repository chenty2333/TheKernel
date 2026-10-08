// SPDX-License-Identifier: MIT
// Copyright © 2022 Intel Corporation.
// Source-order bindings from Linux v7.2.3
// drivers/gpu/drm/i915/gt/uc/intel_gsc_uc.h.

use core::ffi::c_void;

use crate::{
    intel_context_types_upstream::IntelContext,
    intel_context_upstream::I915Vma,
    intel_engine_cs_upstream::{Mutex, WorkStruct},
    intel_uc_fw_types_upstream::{
        __intel_uc_fw_status, INTEL_UC_FIRMWARE_AVAILABLE, INTEL_UC_FIRMWARE_DISABLED,
        INTEL_UC_FIRMWARE_NOT_SUPPORTED, INTEL_UC_FIRMWARE_SELECTED, IntelUcFw, IntelUcFwVersion,
        intel_uc_fw_is_available,
    },
    linux::workqueue::WorkqueueStruct,
    linux_print::DrmPrinter,
};

/// Forward-declared by `intel_gsc_uc.h`; stored only as a pointer.
#[repr(C)]
pub struct I915GscProxyComponent {
    _opaque: [u8; 0],
}

/// Anonymous proxy subrecord in `struct intel_gsc_uc`.
#[repr(C)]
pub struct IntelGscUcProxy {
    pub component: *mut I915GscProxyComponent,
    pub component_added: bool,
    pub vma: *mut I915Vma,
    pub to_gsc: *mut c_void,
    pub to_csme: *mut c_void,
    pub mutex: Mutex,
}

/// `struct intel_gsc_uc`.
#[repr(C)]
pub struct IntelGscUc {
    pub fw: IntelUcFw,
    pub release: IntelUcFwVersion,
    pub security_version: u32,
    pub local: *mut I915Vma,
    pub local_vaddr: *mut c_void,
    pub ce: *mut IntelContext,
    pub wq: *mut WorkqueueStruct,
    pub work: WorkStruct,
    pub gsc_work_actions: u32,
    pub proxy: IntelGscUcProxy,
}

pub const GSC_ACTION_FW_LOAD: u32 = 1 << 0;
pub const GSC_ACTION_SW_PROXY: u32 = 1 << 1;

// Out-of-line functions declared by intel_gsc_uc.h.
unsafe extern "C" {
    pub fn intel_gsc_uc_init_early(gsc: *mut IntelGscUc);
    pub fn intel_gsc_uc_init(gsc: *mut IntelGscUc) -> i32;
    pub fn intel_gsc_uc_fini(gsc: *mut IntelGscUc);
    pub fn intel_gsc_uc_suspend(gsc: *mut IntelGscUc);
    pub fn intel_gsc_uc_resume(gsc: *mut IntelGscUc);
    pub fn intel_gsc_uc_flush_work(gsc: *mut IntelGscUc);
    pub fn intel_gsc_uc_load_start(gsc: *mut IntelGscUc);
    pub fn intel_gsc_uc_load_status(gsc: *mut IntelGscUc, printer: *mut DrmPrinter);
}

/// `intel_gsc_uc_is_supported()` from intel_gsc_uc.h.
///
/// # Safety
/// `gsc` must point to a live GSC firmware owner.
pub unsafe fn intel_gsc_uc_is_supported(gsc: *mut IntelGscUc) -> bool {
    let fw = unsafe { core::ptr::addr_of!((*gsc).fw) };
    unsafe { __intel_uc_fw_status(fw) != INTEL_UC_FIRMWARE_NOT_SUPPORTED }
}

/// `intel_gsc_uc_is_wanted()` from intel_gsc_uc.h.
///
/// # Safety
/// `gsc` must point to a live GSC firmware owner.
pub unsafe fn intel_gsc_uc_is_wanted(gsc: *mut IntelGscUc) -> bool {
    let fw = unsafe { core::ptr::addr_of!((*gsc).fw) };
    unsafe { __intel_uc_fw_status(fw) > INTEL_UC_FIRMWARE_DISABLED }
}

/// `intel_gsc_uc_is_used()` from intel_gsc_uc.h.
///
/// # Safety
/// `gsc` must point to a live GSC firmware owner.
pub unsafe fn intel_gsc_uc_is_used(gsc: *mut IntelGscUc) -> bool {
    let fw = unsafe { core::ptr::addr_of!((*gsc).fw) };
    gem_bug_on!(unsafe { __intel_uc_fw_status(fw) } == INTEL_UC_FIRMWARE_SELECTED);
    unsafe { intel_uc_fw_is_available(fw) }
}

// Target ABI: x86_64 Linux v7.2.3, CONFIG_DRM_I915_CAPTURE_ERROR=y,
// CONFIG_LOCKDEP=n, CONFIG_DEBUG_MUTEXES=n, CONFIG_PREEMPT_RT=n.
const _: [(); 64] = [(); core::mem::size_of::<IntelGscUcProxy>()];
const _: [(); 8] = [(); core::mem::align_of::<IntelGscUcProxy>()];
const _: [(); 0] = [(); core::mem::offset_of!(IntelGscUcProxy, component)];
const _: [(); 8] = [(); core::mem::offset_of!(IntelGscUcProxy, component_added)];
const _: [(); 16] = [(); core::mem::offset_of!(IntelGscUcProxy, vma)];
const _: [(); 24] = [(); core::mem::offset_of!(IntelGscUcProxy, to_gsc)];
const _: [(); 32] = [(); core::mem::offset_of!(IntelGscUcProxy, to_csme)];
const _: [(); 40] = [(); core::mem::offset_of!(IntelGscUcProxy, mutex)];

const _: [(); 0] = [(); core::mem::offset_of!(IntelGscUc, fw)];
const _: [(); 416] = [(); core::mem::offset_of!(IntelGscUc, release)];
const _: [(); 432] = [(); core::mem::offset_of!(IntelGscUc, security_version)];
const _: [(); 440] = [(); core::mem::offset_of!(IntelGscUc, local)];
const _: [(); 448] = [(); core::mem::offset_of!(IntelGscUc, local_vaddr)];
const _: [(); 456] = [(); core::mem::offset_of!(IntelGscUc, ce)];
const _: [(); 464] = [(); core::mem::offset_of!(IntelGscUc, wq)];
const _: [(); 472] = [(); core::mem::offset_of!(IntelGscUc, work)];
const _: [(); 504] = [(); core::mem::offset_of!(IntelGscUc, gsc_work_actions)];
const _: [(); 512] = [(); core::mem::offset_of!(IntelGscUc, proxy)];
const _: [(); 576] = [(); core::mem::size_of::<IntelGscUc>()];
