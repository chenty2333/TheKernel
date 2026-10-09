// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
// Source-order bindings from Linux v7.2.3
// drivers/gpu/drm/i915/gt/intel_rps_types.h.

use core::ffi::c_ulong;

use crate::{
    intel_engine_cs_upstream::{AtomicT, Mutex, TimerList, WorkStruct},
    linux::fields::KtimeT,
};

#[repr(C)]
pub struct IntelIps {
    pub last_count1: u64,
    pub last_time1: c_ulong,
    pub chipset_power: c_ulong,
    pub last_count2: u64,
    pub last_time2: u64,
    pub gfx_power: c_ulong,
    pub corr: u8,
    pub c: i32,
    pub m: i32,
}

#[repr(C)]
pub struct IntelRpsEi {
    pub ktime: KtimeT,
    pub render_c0: u32,
    pub media_c0: u32,
}

pub const INTEL_RPS_ENABLED: i32 = 0;
pub const INTEL_RPS_ACTIVE: i32 = 1;
pub const INTEL_RPS_INTERRUPTS: i32 = 2;
pub const INTEL_RPS_TIMER: i32 = 3;

#[repr(C)]
pub struct IntelRpsFreqCaps {
    pub rp0_freq: u8,
    pub rp1_freq: u8,
    pub min_freq: u8,
}

#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntelRpsPowerModeValue {
    LowPower  = 0,
    Between   = 1,
    HighPower = 2,
}

pub type IntelRpsPowerMode = i32;
pub const LOW_POWER: IntelRpsPowerMode = IntelRpsPowerModeValue::LowPower as i32;
pub const BETWEEN: IntelRpsPowerMode = IntelRpsPowerModeValue::Between as i32;
pub const HIGH_POWER: IntelRpsPowerMode = IntelRpsPowerModeValue::HighPower as i32;

#[repr(C)]
pub struct IntelRpsPower {
    pub mutex: Mutex,
    pub mode: IntelRpsPowerMode,
    pub interactive: u32,
    pub up_threshold: u8,
    pub down_threshold: u8,
}

#[repr(C)]
pub struct IntelRps {
    pub lock: Mutex,
    pub timer: TimerList,
    pub work: WorkStruct,
    pub flags: c_ulong,
    pub pm_timestamp: KtimeT,
    pub pm_interval: u32,
    pub pm_iir: u32,
    pub pm_intrmsk_mbz: u32,
    pub pm_events: u32,
    pub cur_freq: u8,
    pub last_freq: u8,
    pub min_freq_softlimit: u8,
    pub max_freq_softlimit: u8,
    pub max_freq: u8,
    pub min_freq: u8,
    pub boost_freq: u8,
    pub idle_freq: u8,
    pub efficient_freq: u8,
    pub rp1_freq: u8,
    pub rp0_freq: u8,
    pub gpll_ref_freq: u16,
    pub last_adj: i32,
    pub power: IntelRpsPower,
    pub num_waiters: AtomicT,
    pub boosts: u32,
    pub ei: IntelRpsEi,
    pub ips: IntelIps,
}

// x86_64 Linux v7.2.3, CONFIG_LOCKDEP=n, CONFIG_DEBUG_MUTEXES=n,
// CONFIG_PREEMPT_RT=n. These also pin the imported timer/work/mutex layouts.
const _: [(); 64] = [(); core::mem::size_of::<IntelIps>()];
const _: [(); 0] = [(); core::mem::offset_of!(IntelIps, last_count1)];
const _: [(); 8] = [(); core::mem::offset_of!(IntelIps, last_time1)];
const _: [(); 16] = [(); core::mem::offset_of!(IntelIps, chipset_power)];
const _: [(); 24] = [(); core::mem::offset_of!(IntelIps, last_count2)];
const _: [(); 32] = [(); core::mem::offset_of!(IntelIps, last_time2)];
const _: [(); 40] = [(); core::mem::offset_of!(IntelIps, gfx_power)];
const _: [(); 48] = [(); core::mem::offset_of!(IntelIps, corr)];
const _: [(); 52] = [(); core::mem::offset_of!(IntelIps, c)];
const _: [(); 56] = [(); core::mem::offset_of!(IntelIps, m)];

const _: [(); 16] = [(); core::mem::size_of::<IntelRpsEi>()];
const _: [(); 3] = [(); core::mem::size_of::<IntelRpsFreqCaps>()];
const _: [(); 40] = [(); core::mem::size_of::<IntelRpsPower>()];
const _: [(); 8] = [(); core::mem::align_of::<IntelRpsPower>()];
const _: [(); 0] = [(); core::mem::offset_of!(IntelRpsPower, mutex)];
const _: [(); 24] = [(); core::mem::offset_of!(IntelRpsPower, mode)];
const _: [(); 28] = [(); core::mem::offset_of!(IntelRpsPower, interactive)];
const _: [(); 32] = [(); core::mem::offset_of!(IntelRpsPower, up_threshold)];
const _: [(); 33] = [(); core::mem::offset_of!(IntelRpsPower, down_threshold)];
const _: [(); 152] = [(); core::mem::offset_of!(IntelRps, power)];
const _: [(); 280] = [(); core::mem::size_of::<IntelRps>()];
const _: [(); 8] = [(); core::mem::align_of::<IntelRps>()];
const _: [(); 0] = [(); core::mem::offset_of!(IntelRps, lock)];
const _: [(); 24] = [(); core::mem::offset_of!(IntelRps, timer)];
const _: [(); 64] = [(); core::mem::offset_of!(IntelRps, work)];
const _: [(); 96] = [(); core::mem::offset_of!(IntelRps, flags)];
const _: [(); 104] = [(); core::mem::offset_of!(IntelRps, pm_timestamp)];
const _: [(); 112] = [(); core::mem::offset_of!(IntelRps, pm_interval)];
const _: [(); 116] = [(); core::mem::offset_of!(IntelRps, pm_iir)];
const _: [(); 120] = [(); core::mem::offset_of!(IntelRps, pm_intrmsk_mbz)];
const _: [(); 124] = [(); core::mem::offset_of!(IntelRps, pm_events)];
const _: [(); 128] = [(); core::mem::offset_of!(IntelRps, cur_freq)];
const _: [(); 129] = [(); core::mem::offset_of!(IntelRps, last_freq)];
const _: [(); 130] = [(); core::mem::offset_of!(IntelRps, min_freq_softlimit)];
const _: [(); 131] = [(); core::mem::offset_of!(IntelRps, max_freq_softlimit)];
const _: [(); 132] = [(); core::mem::offset_of!(IntelRps, max_freq)];
const _: [(); 133] = [(); core::mem::offset_of!(IntelRps, min_freq)];
const _: [(); 134] = [(); core::mem::offset_of!(IntelRps, boost_freq)];
const _: [(); 135] = [(); core::mem::offset_of!(IntelRps, idle_freq)];
const _: [(); 136] = [(); core::mem::offset_of!(IntelRps, efficient_freq)];
const _: [(); 137] = [(); core::mem::offset_of!(IntelRps, rp1_freq)];
const _: [(); 138] = [(); core::mem::offset_of!(IntelRps, rp0_freq)];
const _: [(); 140] = [(); core::mem::offset_of!(IntelRps, gpll_ref_freq)];
const _: [(); 144] = [(); core::mem::offset_of!(IntelRps, last_adj)];
const _: [(); 192] = [(); core::mem::offset_of!(IntelRps, num_waiters)];
const _: [(); 196] = [(); core::mem::offset_of!(IntelRps, boosts)];
const _: [(); 200] = [(); core::mem::offset_of!(IntelRps, ei)];
const _: [(); 216] = [(); core::mem::offset_of!(IntelRps, ips)];
