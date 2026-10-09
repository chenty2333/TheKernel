// SPDX-License-Identifier: MIT
// Copyright © 2016-2019 Intel Corporation
//
// Rust ABI mirror of Linux v7.2.3 drivers/gpu/drm/i915/gt/uc/intel_guc_ct.h.

use crate::{
    i915_scheduler_types_upstream::TaskletStruct,
    intel_context_upstream::{I915Vma, WaitQueueHead},
    intel_engine_cs_upstream::{AtomicT, ListHead, Spinlock, WorkStruct},
};

/// Forward declaration from `gt/uc/abi/guc_communication_ctb_abi.h`.
///
/// The CT header stores only a pointer to this descriptor; its wire layout is
/// therefore intentionally not duplicated in this host-side binding.
#[repr(C)]
pub struct GucCtBufferDesc {
    _opaque: [u8; 0],
}

/// `struct intel_guc_ct_buffer` from `intel_guc_ct.h`.
#[repr(C)]
pub struct IntelGucCtBuffer {
    pub lock: Spinlock,
    pub desc: *mut GucCtBufferDesc,
    pub cmds: *mut u32,
    pub size: u32,
    pub resv_space: u32,
    pub tail: u32,
    pub head: u32,
    pub space: AtomicT,
    pub broken: bool,
}

/// `struct intel_guc_ct` from `intel_guc_ct.h` for the checked-in kernel
/// configuration. `CONFIG_DRM_I915_DEBUG_GEM` is false in `linux/config.rs`,
/// and the config does not enable `CONFIG_DRM_I915_DEBUG`; consequently the
/// optional `lost_and_found` and dead-CT debug tail are absent here. Enabling
/// those C options changes the corresponding source layouts.
#[repr(C)]
pub struct IntelGucCt {
    pub vma: *mut I915Vma,
    pub enabled: bool,
    pub ctbs: IntelGucCtBuffers,
    pub receive_tasklet: TaskletStruct,
    pub wq: WaitQueueHead,
    pub requests: IntelGucCtRequests,
    pub stall_time: i64,
}

/// Anonymous `ctbs` member of `struct intel_guc_ct`.
#[repr(C)]
pub struct IntelGucCtBuffers {
    pub send: IntelGucCtBuffer,
    pub recv: IntelGucCtBuffer,
}

/// Anonymous `requests` member of `struct intel_guc_ct` for the non-debug-GEM
/// configuration.
#[repr(C)]
pub struct IntelGucCtRequests {
    pub last_fence: u16,
    pub lock: Spinlock,
    pub pending: ListHead,
    pub incoming: ListHead,
    pub worker: WorkStruct,
}

/// `INTEL_GUC_CT_SEND_NB`.
pub const INTEL_GUC_CT_SEND_NB: u32 = 1 << 31;
/// `INTEL_GUC_CT_SEND_G2H_DW_SHIFT`.
pub const INTEL_GUC_CT_SEND_G2H_DW_SHIFT: u32 = 0;
/// `INTEL_GUC_CT_SEND_G2H_DW_MASK`.
pub const INTEL_GUC_CT_SEND_G2H_DW_MASK: u32 = 0xff << INTEL_GUC_CT_SEND_G2H_DW_SHIFT;

// Source-layout checks for x86_64 with the source kernel's non-debug CT
// configuration and the LinuxKPI primitive mirrors imported above.
const _: [(); 48] = [(); core::mem::size_of::<IntelGucCtBuffer>()];
const _: [(); 8] = [(); core::mem::align_of::<IntelGucCtBuffer>()];
const _: [(); 0] = [(); core::mem::offset_of!(IntelGucCtBuffer, lock)];
const _: [(); 8] = [(); core::mem::offset_of!(IntelGucCtBuffer, desc)];
const _: [(); 16] = [(); core::mem::offset_of!(IntelGucCtBuffer, cmds)];
const _: [(); 24] = [(); core::mem::offset_of!(IntelGucCtBuffer, size)];
const _: [(); 28] = [(); core::mem::offset_of!(IntelGucCtBuffer, resv_space)];
const _: [(); 32] = [(); core::mem::offset_of!(IntelGucCtBuffer, tail)];
const _: [(); 36] = [(); core::mem::offset_of!(IntelGucCtBuffer, head)];
const _: [(); 40] = [(); core::mem::offset_of!(IntelGucCtBuffer, space)];
const _: [(); 44] = [(); core::mem::offset_of!(IntelGucCtBuffer, broken)];

const _: [(); 96] = [(); core::mem::size_of::<IntelGucCtBuffers>()];
const _: [(); 72] = [(); core::mem::size_of::<IntelGucCtRequests>()];
const _: [(); 256] = [(); core::mem::size_of::<IntelGucCt>()];
const _: [(); 0] = [(); core::mem::offset_of!(IntelGucCt, vma)];
const _: [(); 8] = [(); core::mem::offset_of!(IntelGucCt, enabled)];
const _: [(); 16] = [(); core::mem::offset_of!(IntelGucCt, ctbs)];
const _: [(); 112] = [(); core::mem::offset_of!(IntelGucCt, receive_tasklet)];
const _: [(); 152] = [(); core::mem::offset_of!(IntelGucCt, wq)];
const _: [(); 176] = [(); core::mem::offset_of!(IntelGucCt, requests)];
const _: [(); 248] = [(); core::mem::offset_of!(IntelGucCt, stall_time)];
