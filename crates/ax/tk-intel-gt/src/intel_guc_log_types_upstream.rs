// SPDX-License-Identifier: MIT
// Copyright © 2014-2019 Intel Corporation.
//
//! GuC log records and constants transcribed from Linux v7.2.3
//! `drivers/gpu/drm/i915/gt/uc/intel_guc_log.h`.

use crate::{
    intel_context_upstream::I915Vma,
    intel_engine_cs_upstream::{Mutex, WorkStruct},
};

pub const GUC_LOG_LEVEL_DISABLED: u32 = 0;
pub const GUC_LOG_LEVEL_NON_VERBOSE: u32 = 1;

#[allow(non_snake_case)]
pub const fn GUC_LOG_LEVEL_IS_ENABLED(level: u32) -> bool {
    level > GUC_LOG_LEVEL_DISABLED
}

#[allow(non_snake_case)]
pub const fn GUC_LOG_LEVEL_IS_VERBOSE(level: u32) -> bool {
    level > GUC_LOG_LEVEL_NON_VERBOSE
}

/// Equivalent of the source GNU statement-expression macro; evaluates its
/// argument once and maps non-verbose levels to zero.
#[allow(non_snake_case)]
pub const fn GUC_LOG_LEVEL_TO_VERBOSITY(level: u32) -> u32 {
    if GUC_LOG_LEVEL_IS_VERBOSE(level) {
        level - 2
    } else {
        0
    }
}

#[allow(non_snake_case)]
pub const fn GUC_VERBOSITY_TO_LOG_LEVEL(verbosity: u32) -> u32 {
    verbosity + 2
}

/// The included `intel_guc_fwif.h` defines GUC_LOG_VERBOSITY_MAX as 3.
pub const GUC_LOG_LEVEL_MAX: u32 = GUC_VERBOSITY_TO_LOG_LEVEL(3);

/// Anonymous enum `GUC_LOG_SECTIONS_*` in this header (C `int` representation).
#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GucLogSectionsValue {
    Crash   = 0,
    Debug   = 1,
    Capture = 2,
    Limit   = 3,
}

pub type GucLogSections = i32;
pub const GUC_LOG_SECTIONS_CRASH: GucLogSections = 0;
pub const GUC_LOG_SECTIONS_DEBUG: GucLogSections = 1;
pub const GUC_LOG_SECTIONS_CAPTURE: GucLogSections = 2;
pub const GUC_LOG_SECTIONS_LIMIT: GucLogSections = 3;

/// Anonymous allocation-settings record nested in `struct intel_guc_log`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IntelGucLogSizes {
    pub bytes: i32,
    pub units: i32,
    pub count: i32,
    pub flag: u32,
}

/// RelayFS state nested in `struct intel_guc_log`.
#[repr(C)]
pub struct IntelGucLogRelay {
    pub buf_in_use: bool,
    pub started: bool,
    pub flush_work: WorkStruct,
    /// `struct rchan *` from Linux relayfs.
    pub channel: *mut core::ffi::c_void,
    pub lock: Mutex,
    pub full_count: u32,
}

/// Logging counters nested in `struct intel_guc_log`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IntelGucLogStats {
    pub sampled_overflow: u32,
    pub overflow: u32,
    pub flush: u32,
}

/// `struct intel_guc_log`.
#[repr(C)]
pub struct IntelGucLog {
    pub level: u32,
    pub guc_lock: Mutex,
    pub sizes: [IntelGucLogSizes; GUC_LOG_SECTIONS_LIMIT as usize],
    pub sizes_initialised: bool,
    pub vma: *mut I915Vma,
    pub buf_addr: *mut core::ffi::c_void,
    pub relay: IntelGucLogRelay,
    /// `GUC_MAX_LOG_BUFFER` from the included `intel_guc_fwif.h` is 3.
    pub stats: [IntelGucLogStats; 3],
}

const _: [(); 16] = [(); core::mem::size_of::<IntelGucLogSizes>()];
const _: [(); 4] = [(); core::mem::align_of::<IntelGucLogSizes>()];
const _: [(); 12] = [(); core::mem::size_of::<IntelGucLogStats>()];
const _: [(); 4] = [(); core::mem::align_of::<IntelGucLogStats>()];
const _: [(); 24] = [(); core::mem::size_of::<Mutex>()];
const _: [(); 8] = [(); core::mem::align_of::<Mutex>()];
const _: [(); 32] = [(); core::mem::size_of::<WorkStruct>()];
const _: [(); 8] = [(); core::mem::align_of::<WorkStruct>()];

// x86_64 Linux v7.2.3. Mutex/worker layout follows the wt-dev configuration:
// CONFIG_LOCKDEP=n, CONFIG_DEBUG_MUTEXES=n and CONFIG_PREEMPT_RT=n.
const _: [(); 80] = [(); core::mem::size_of::<IntelGucLogRelay>()];
const _: [(); 8] = [(); core::mem::align_of::<IntelGucLogRelay>()];
const _: [(); 0] = [(); core::mem::offset_of!(IntelGucLogRelay, buf_in_use)];
const _: [(); 1] = [(); core::mem::offset_of!(IntelGucLogRelay, started)];
const _: [(); 8] = [(); core::mem::offset_of!(IntelGucLogRelay, flush_work)];
const _: [(); 40] = [(); core::mem::offset_of!(IntelGucLogRelay, channel)];
const _: [(); 48] = [(); core::mem::offset_of!(IntelGucLogRelay, lock)];
const _: [(); 72] = [(); core::mem::offset_of!(IntelGucLogRelay, full_count)];

// Existing imported layout evidence in guc_submission_upstream.rs records
// IntelGucLogLayout as 224 bytes with 8-byte alignment.
const _: [(); 224] = [(); core::mem::size_of::<IntelGucLog>()];
const _: [(); 8] = [(); core::mem::align_of::<IntelGucLog>()];
const _: [(); 0] = [(); core::mem::offset_of!(IntelGucLog, level)];
const _: [(); 8] = [(); core::mem::offset_of!(IntelGucLog, guc_lock)];
const _: [(); 32] = [(); core::mem::offset_of!(IntelGucLog, sizes)];
const _: [(); 80] = [(); core::mem::offset_of!(IntelGucLog, sizes_initialised)];
const _: [(); 88] = [(); core::mem::offset_of!(IntelGucLog, vma)];
const _: [(); 96] = [(); core::mem::offset_of!(IntelGucLog, buf_addr)];
const _: [(); 104] = [(); core::mem::offset_of!(IntelGucLog, relay)];
const _: [(); 184] = [(); core::mem::offset_of!(IntelGucLog, stats)];
