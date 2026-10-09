// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
// Source-order translation from Linux v7.2.3
// drivers/gpu/drm/i915/gt/intel_llc.c.
//
// Lower-owner APIs not present in this checkout are deliberately referenced
// by their source names below, rather than replaced with guessed layouts or
// synthetic implementations. They are: cpufreq_cpu_get/put and the
// cpufreq_policy.cpuinfo.max_freq owner, asm/x86 tsc_khz, the two
// intel_rps_get_*_raw_freq helpers, the DCLK and GEN6_PCODE_* register
// definitions, and snb_pcode_write (intel_pcode.h's 1 ms timeout wrapper).

#![allow(unsafe_code)]
#![allow(non_snake_case)]

use core::mem::MaybeUninit;

use crate::{
    intel_gt_types_upstream::IntelGt,
    intel_llc_types_upstream::IntelLlc,
    intel_rps_upstream::{intel_rps_get_max_raw_freq, intel_rps_get_min_raw_freq},
    intel_rps_types_upstream::IntelRps,
    intel_uncore_types_upstream::intel_uncore_read,
    intel_workarounds_types_upstream::I915RegT,
    linux::cpufreq::{cpufreq_cpu_get, cpufreq_cpu_put, CpuFreqPolicy},
    linux::{
        i915::{GRAPHICS_VER, HAS_LLC, IS_DGFX, IS_HASWELL},
        primitives::max,
    },
};

// arch/x86/include/asm/tsc.h declares the configured kernel's global counter.
unsafe extern "C" {
    static tsc_khz: u32;
}

// Header constants from include/drm/intel/mchbar_regs.h and
// include/drm/intel/intel_pcode_regs.h in Linux v7.2.3.
const DCLK: I915RegT = I915RegT { reg: 0x145e04 };
const GEN6_PCODE_WRITE_MIN_FREQ_TABLE: u32 = 0x8;
const GEN6_PCODE_FREQ_IA_RATIO_SHIFT: u32 = 8;
const GEN6_PCODE_FREQ_RING_RATIO_SHIFT: u32 = 16;

// `intel_pcode.h` defines snb_pcode_write() as this 1 ms wrapper.
unsafe extern "C" {
    fn snb_pcode_write_timeout(
        uncore: *mut crate::intel_uncore_types_upstream::IntelUncore,
        mbox: u32,
        val: u32,
        timeout_ms: i32,
    ) -> i32;
}

unsafe fn snb_pcode_write(
    uncore: *mut crate::intel_uncore_types_upstream::IntelUncore,
    mbox: u32,
    val: u32,
) -> i32 {
    unsafe { snb_pcode_write_timeout(uncore, mbox, val, 1) }
}

#[repr(C)]
struct IaConstants {
    min_gpu_freq: u32,
    max_gpu_freq: u32,

    min_ring_freq: u32,
    max_ia_freq: u32,
}

// upstream: intel_llc.c llc_to_gt()
unsafe fn llc_to_gt(llc: *mut IntelLlc) -> *mut IntelGt {
    container_of!(llc, IntelGt, llc)
}

// upstream: intel_llc.c cpu_max_MHz()
fn cpu_max_MHz() -> u32 {
    let policy: *mut CpuFreqPolicy = unsafe { cpufreq_cpu_get(0) };
    let max_khz = if !policy.is_null() {
        let max_khz = unsafe { (*policy).cpuinfo.max_freq };
        unsafe { cpufreq_cpu_put(policy) };
        max_khz
    } else {
        // Default to measured freq if none found, PCU will ensure we
        // don't go over
        unsafe { tsc_khz }
    };

    max_khz / 1000
}

// upstream: intel_llc.c get_ia_constants()
unsafe fn get_ia_constants(llc: *mut IntelLlc, consts: *mut IaConstants) -> bool {
    let gt = unsafe { llc_to_gt(llc) };
    let i915 = unsafe { (*gt).i915 };
    let rps = unsafe { core::ptr::addr_of_mut!((*gt).rps) };

    if !unsafe { HAS_LLC(i915) } || unsafe { IS_DGFX(i915) } {
        return false;
    }

    unsafe { (*consts).max_ia_freq = cpu_max_MHz() };

    unsafe {
        (*consts).min_ring_freq = intel_uncore_read((*gt).uncore, DCLK) & 0xf;
    }
    // Convert DDR frequency from units of 266.6MHz to bandwidth.
    let ddr_freq = unsafe { (*consts).min_ring_freq };
    // Exact overflow-avoiding expansion of Linux mult_frac(ddr_freq, 8, 3).
    unsafe {
        (*consts).min_ring_freq = (ddr_freq / 3) * 8 + (ddr_freq % 3) * 8 / 3;
        (*consts).min_gpu_freq = intel_rps_get_min_raw_freq(rps);
        (*consts).max_gpu_freq = intel_rps_get_max_raw_freq(rps);
    }

    true
}

// upstream: intel_llc.c calc_ia_freq()
unsafe fn calc_ia_freq(
    llc: *mut IntelLlc,
    gpu_freq: u32,
    consts: *const IaConstants,
    out_ia_freq: *mut u32,
    out_ring_freq: *mut u32,
) {
    let i915 = unsafe { (*llc_to_gt(llc)).i915 };
    // C computes this unsigned subtraction and converts the result to `int`.
    let diff = unsafe { (*consts).max_gpu_freq.wrapping_sub(gpu_freq) as i32 };
    let mut ia_freq = 0u32;
    let mut ring_freq = 0u32;

    if unsafe { GRAPHICS_VER(i915) } >= 9 {
        // ring_freq = 2 * GT. ring_freq is in 100MHz units.
        ring_freq = gpu_freq;
    } else if unsafe { GRAPHICS_VER(i915) } >= 8 {
        // max(2 * GT, DDR): GT is in 50MHz units.
        ring_freq = max(unsafe { (*consts).min_ring_freq }, gpu_freq);
    } else if unsafe { IS_HASWELL(i915) } {
        // Exact overflow-avoiding expansion of Linux mult_frac(gpu_freq,5,4).
        let scaled_gpu_freq = (gpu_freq / 4) * 5 + (gpu_freq % 4) * 5 / 4;
        ring_freq = max(unsafe { (*consts).min_ring_freq }, scaled_gpu_freq);
        // leave ia_freq as the default, chosen by cpufreq
    } else {
        const MIN_FREQ: u32 = 15;
        const SCALE: i32 = 180;

        // On older processors, there is no separate ring clock domain, so
        // in order to boost the bandwidth of the ring, upclock the CPU.
        if gpu_freq < MIN_FREQ {
            ia_freq = 800;
        } else {
            ia_freq = unsafe { (*consts).max_ia_freq } - (diff * SCALE / 2) as u32;
        }
        // Positive inputs, preserving DIV_ROUND_CLOSEST(ia_freq, 100).
        ia_freq = (ia_freq + 50) / 100;
    }

    unsafe {
        *out_ia_freq = ia_freq;
        *out_ring_freq = ring_freq;
    }
}

// upstream: intel_llc.c gen6_update_ring_freq()
unsafe fn gen6_update_ring_freq(llc: *mut IntelLlc) {
    let mut consts = MaybeUninit::<IaConstants>::uninit();
    if !unsafe { get_ia_constants(llc, consts.as_mut_ptr()) } {
        return;
    }
    let consts = unsafe { consts.assume_init() };

    // Prevent accidentally entering the table loop without a descending range.
    if consts.max_gpu_freq <= consts.min_gpu_freq {
        return;
    }

    // For each potential GPU frequency, load a ring frequency we'd like to
    // use for memory access. Specify the IA frequency as PCU's reference.
    let mut gpu_freq = consts.max_gpu_freq;
    while gpu_freq >= consts.min_gpu_freq {
        let mut ia_freq = 0;
        let mut ring_freq = 0;

        unsafe {
            calc_ia_freq(llc, gpu_freq, &consts, &mut ia_freq, &mut ring_freq);
            let gt = llc_to_gt(llc);
            snb_pcode_write(
                (*gt).uncore,
                GEN6_PCODE_WRITE_MIN_FREQ_TABLE,
                (ia_freq << GEN6_PCODE_FREQ_IA_RATIO_SHIFT)
                    | (ring_freq << GEN6_PCODE_FREQ_RING_RATIO_SHIFT)
                    | gpu_freq,
            );
        }

        // The C loop variable is unsigned; retain its wrap semantics.
        gpu_freq = gpu_freq.wrapping_sub(1);
    }
}

// upstream: intel_llc.c intel_llc_enable()
pub unsafe fn intel_llc_enable(llc: *mut IntelLlc) {
    unsafe { gen6_update_ring_freq(llc) };
}

// upstream: intel_llc.c intel_llc_disable()
pub unsafe fn intel_llc_disable(_llc: *mut IntelLlc) {
    // Currently there is no HW configuration to be done to disable.
}
