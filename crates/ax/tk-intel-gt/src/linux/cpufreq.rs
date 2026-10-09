// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Linux 7.2.3 CPU-frequency API bindings for the configured x86_64 target.
//! The policy offset and size are derived from the target kernel headers.

#![allow(unsafe_code)]

use core::ffi::c_uint;

#[repr(C)]
pub struct CpuFreqCpuInfo {
    pub max_freq: c_uint,
    pub min_freq: c_uint,
    pub transition_latency: c_uint,
}

#[repr(C, align(8))]
pub struct CpuFreqPolicy {
    _before_cpuinfo: [u8; 40],
    pub cpuinfo: CpuFreqCpuInfo,
    _after_cpuinfo: [u8; 716],
}

unsafe extern "C" {
    pub fn cpufreq_cpu_get(cpu: c_uint) -> *mut CpuFreqPolicy;
    pub fn cpufreq_cpu_put(policy: *mut CpuFreqPolicy);
}

// Linux v7.2.3 target oracle: CONFIG_CPU_FREQ=y, cpumask_var_t is pointer-
// backed, sizeof(struct cpufreq_policy)=768 and cpuinfo begins at byte 40.
const _: [(); 12] = [(); core::mem::size_of::<CpuFreqCpuInfo>()];
const _: [(); 768] = [(); core::mem::size_of::<CpuFreqPolicy>()];
const _: [(); 8] = [(); core::mem::align_of::<CpuFreqPolicy>()];
const _: [(); 40] = [(); core::mem::offset_of!(CpuFreqPolicy, cpuinfo)];
const _: [(); 0] = [(); core::mem::offset_of!(CpuFreqCpuInfo, max_freq)];
