// SPDX-License-Identifier: MIT
// Copyright © 2021 Intel Corporation.
//
//! Source-order SLPC records and constants from Linux v7.2.3
//! `drivers/gpu/drm/i915/gt/uc/intel_guc_slpc_types.h`.

use crate::{
    intel_context_upstream::I915Vma,
    intel_engine_cs_upstream::{AtomicT, Mutex, WorkStruct},
};

/// SLPC shared data in the GuC-visible mapping; referenced only by pointer here.
#[repr(C)]
pub struct SlpcSharedData {
    _opaque: [u8; 0],
}

pub const SLPC_RESET_TIMEOUT_MS: i32 = 5;

/// `struct intel_guc_slpc`.
#[repr(C)]
pub struct IntelGucSlpc {
    pub vma: *mut I915Vma,
    pub vaddr: *mut SlpcSharedData,
    pub supported: bool,
    pub selected: bool,
    pub min_is_rpmax: bool,

    pub min_freq: u32,
    pub rp0_freq: u32,
    pub rp1_freq: u32,
    pub boost_freq: u32,

    pub min_freq_softlimit: u32,
    pub max_freq_softlimit: u32,
    pub ignore_eff_freq: bool,

    pub power_profile: u32,
    pub media_ratio_mode: u32,

    pub lock: Mutex,
    pub boost_work: WorkStruct,
    pub num_waiters: AtomicT,
    pub num_boosts: u32,
}

// x86_64 Linux v7.2.3; kernel mutex/workqueue records use the wt-dev config
// (CONFIG_LOCKDEP=n, CONFIG_DEBUG_MUTEXES=n, CONFIG_PREEMPT_RT=n).
const _: [(); 24] = [(); core::mem::size_of::<Mutex>()];
const _: [(); 8] = [(); core::mem::align_of::<Mutex>()];
const _: [(); 32] = [(); core::mem::size_of::<WorkStruct>()];
const _: [(); 8] = [(); core::mem::align_of::<WorkStruct>()];
const _: [(); 4] = [(); core::mem::size_of::<AtomicT>()];
const _: [(); 4] = [(); core::mem::align_of::<AtomicT>()];

// Existing IntelGucSlpcLayout in guc_submission_upstream.rs records the same
// Linux layout as 120 bytes. These assertions retain each field boundary.
const _: [(); 120] = [(); core::mem::size_of::<IntelGucSlpc>()];
const _: [(); 8] = [(); core::mem::align_of::<IntelGucSlpc>()];
const _: [(); 0] = [(); core::mem::offset_of!(IntelGucSlpc, vma)];
const _: [(); 8] = [(); core::mem::offset_of!(IntelGucSlpc, vaddr)];
const _: [(); 16] = [(); core::mem::offset_of!(IntelGucSlpc, supported)];
const _: [(); 17] = [(); core::mem::offset_of!(IntelGucSlpc, selected)];
const _: [(); 18] = [(); core::mem::offset_of!(IntelGucSlpc, min_is_rpmax)];
const _: [(); 20] = [(); core::mem::offset_of!(IntelGucSlpc, min_freq)];
const _: [(); 24] = [(); core::mem::offset_of!(IntelGucSlpc, rp0_freq)];
const _: [(); 28] = [(); core::mem::offset_of!(IntelGucSlpc, rp1_freq)];
const _: [(); 32] = [(); core::mem::offset_of!(IntelGucSlpc, boost_freq)];
const _: [(); 36] = [(); core::mem::offset_of!(IntelGucSlpc, min_freq_softlimit)];
const _: [(); 40] = [(); core::mem::offset_of!(IntelGucSlpc, max_freq_softlimit)];
const _: [(); 44] = [(); core::mem::offset_of!(IntelGucSlpc, ignore_eff_freq)];
const _: [(); 48] = [(); core::mem::offset_of!(IntelGucSlpc, power_profile)];
const _: [(); 52] = [(); core::mem::offset_of!(IntelGucSlpc, media_ratio_mode)];
const _: [(); 56] = [(); core::mem::offset_of!(IntelGucSlpc, lock)];
const _: [(); 80] = [(); core::mem::offset_of!(IntelGucSlpc, boost_work)];
const _: [(); 112] = [(); core::mem::offset_of!(IntelGucSlpc, num_waiters)];
const _: [(); 116] = [(); core::mem::offset_of!(IntelGucSlpc, num_boosts)];
