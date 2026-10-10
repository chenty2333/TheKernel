// SPDX-License-Identifier: MIT
// Copyright © 2021 Intel Corporation.
// Source-order translation of Linux v7.2.3 drivers/gpu/drm/i915/gt/uc/intel_guc_slpc.c.

#![allow(unsafe_code, non_snake_case, non_camel_case_types)]

use core::{
    ffi::{c_int, c_ulong, c_void},
    mem::{offset_of, size_of},
    ptr,
};

use crate::{
    i915_request_types_upstream::DrmPrinter,
    i915_vma_api_upstream::{I915_VMA_RELEASE_MAP, i915_vma_unpin_and_release},
    intel_engine_api_upstream::drm_clflush_virt_range,
    intel_engine_cs_upstream::{IntelGt, WorkStruct},
    intel_gt_api_upstream::{guc_to_gt, guc_to_i915},
    intel_guc_ct_upstream::intel_guc_ct_send,
    intel_guc_fwif_types_upstream::SLPC_EVENT,
    intel_guc_slpc_types_upstream::{IntelGucSlpc, SLPC_RESET_TIMEOUT_MS},
    intel_guc_submission_types_upstream::intel_guc_submission_is_used,
    intel_guc_types_upstream::{IntelGuc, intel_guc_is_fw_running},
    intel_guc_upstream::{intel_guc_allocate_and_map_vma, intel_guc_ggtt_offset},
    intel_rps_types_upstream::{IntelRps, IntelRpsFreqCaps},
    intel_uncore_types_upstream::intel_uncore_rmw,
    intel_workarounds_types_upstream::I915RegT,
    linux::{
        i915::{GRAPHICS_VER_FULL, INTEL_INFO, IP_VER},
        memory::{atomic_dec_and_test, atomic_read, atomic_set},
        mutex::{mutex_init, mutex_lock, mutex_unlock},
        primitives::str_yes_no,
        workqueue::INIT_WORK_C,
    },
    linux_config::{EINVAL, EIO, ENODEV, EPROTO, PAGE_SIZE},
};

pub const SLPC_MAX_FREQ_MHZ: i32 = 4250;

const SLPC_MAX_OVERRIDE_PARAMETERS: u32 = 256;
const SLPC_MAX_PARAM: u8 = 32;
const SLPC_PAGE_SIZE_BYTES: usize = 4096;
const SLPC_GLOBAL_STATE_NOT_RUNNING: u32 = 0;
const SLPC_GLOBAL_STATE_INITIALIZING: u32 = 1;
const SLPC_GLOBAL_STATE_RESETTING: u32 = 2;
const SLPC_GLOBAL_STATE_RUNNING: u32 = 3;
const SLPC_GLOBAL_STATE_SHUTTING_DOWN: u32 = 4;
const SLPC_GLOBAL_STATE_ERROR: u32 = 5;
const SLPC_EVENT_RESET: u32 = 0;
const SLPC_EVENT_QUERY_TASK_STATE: u32 = 5;
const SLPC_EVENT_PARAMETER_SET: u32 = 6;
const GUC_ACTION_HOST2GUC_PC_SLPC_REQUEST: u32 = 0x3003;
const SLPC_PARAM_TASK_ENABLE_GTPERF: u8 = 0;
const SLPC_PARAM_TASK_DISABLE_GTPERF: u8 = 1;
const SLPC_PARAM_TASK_ENABLE_BALANCER: u8 = 2;
const SLPC_PARAM_TASK_DISABLE_BALANCER: u8 = 3;
const SLPC_PARAM_TASK_ENABLE_DCC: u8 = 4;
const SLPC_PARAM_TASK_DISABLE_DCC: u8 = 5;
const SLPC_PARAM_GLOBAL_MIN_GT_UNSLICE_FREQ_MHZ: u8 = 6;
const SLPC_PARAM_GLOBAL_MAX_GT_UNSLICE_FREQ_MHZ: u8 = 7;
const SLPC_PARAM_MEDIA_FF_RATIO_MODE: u8 = 24;
const SLPC_PARAM_STRATEGIES: u8 = 26;
const SLPC_PARAM_POWER_PROFILE: u8 = 27;
const SLPC_PARAM_IGNORE_EFFICIENT_FREQUENCY: u8 = 28;
const SLPC_MEDIA_RATIO_MODE_DYNAMIC_CONTROL: u32 = 0;
const SLPC_POWER_PROFILES_BASE: u32 = 0;
const SLPC_POWER_PROFILES_POWER_SAVING: u32 = 1;
const SLPC_OPTIMIZED_STRATEGY_COMPUTE: u32 = 1;
const SLPC_GTPERF_TASK_ENABLED: u32 = 1 << 0;
const SLPC_DCC_TASK_ENABLED: u32 = 1 << 11;
const SLPC_IN_DCC: u32 = 1 << 12;
const SLPC_BALANCER_ENABLED: u32 = 1 << 15;
const SLPC_IBC_TASK_ENABLED: u32 = 1 << 16;
const SLPC_BALANCER_IA_LMT_ENABLED: u32 = 1 << 17;
const SLPC_BALANCER_IA_LMT_ACTIVE: u32 = 1 << 18;
const SLPC_MAX_UNSLICE_FREQ_MASK: u32 = 0x0000_00ff;
const SLPC_MIN_UNSLICE_FREQ_MASK: u32 = 0x0000_ff00;
const GT_FREQUENCY_MULTIPLIER: u32 = 50;
const GEN9_FREQ_SCALER: u32 = 3;
const GEN6_PMINTRMSK: I915RegT = I915RegT { reg: 0xa168 };
const ARAT_EXPIRED_INTRMSK: u32 = 1 << 9;

#[repr(C, packed)]
struct SlpcSharedDataHeader {
    size: u32,
    global_state: u32,
    display_data_addr: u32,
    _padding: [u8; 52],
}

#[repr(C, packed)]
struct SlpcTaskStateData {
    status: u32,
    freq: u32,
}

#[repr(C, packed)]
struct SlpcOverrideParams {
    bits: [u32; (SLPC_MAX_OVERRIDE_PARAMETERS / 32) as usize],
    values: [u32; SLPC_MAX_OVERRIDE_PARAMETERS as usize],
}

#[repr(C, packed)]
struct SlpcSharedDataLayout {
    header: SlpcSharedDataHeader,
    platform_info_pad: [u8; 64],
    task_state_data: SlpcTaskStateData,
    task_state_data_pad: [u8; 56],
    override_params: SlpcOverrideParams,
    override_params_pad: [u8; 32],
    shared_data_pad: [u8; 2816],
    reserved_mode_definition: [u8; SLPC_PAGE_SIZE_BYTES],
}

const _: [(); 8192] = [(); size_of::<SlpcSharedDataLayout>()];
const _: [(); 64] = [(); offset_of!(SlpcSharedDataLayout, platform_info_pad)];
const _: [(); 128] = [(); offset_of!(SlpcSharedDataLayout, task_state_data)];
const _: [(); 192] = [(); offset_of!(SlpcSharedDataLayout, override_params)];
const _: [(); 4096] = [(); offset_of!(SlpcSharedDataLayout, reserved_mode_definition)];

unsafe extern "C" {
    // These RPS entry points are the actual API declared by gt/intel_rps.h;
    // the C owner is not part of this SLPC source file.
    fn gen6_rps_get_freq_caps(rps: *mut IntelRps, caps: *mut IntelRpsFreqCaps);
    fn intel_gpu_freq(rps: *mut IntelRps, raw_freq: c_int) -> c_int;
}

#[inline]
unsafe fn slpc_data(slpc: *mut IntelGucSlpc) -> *mut SlpcSharedDataLayout {
    unsafe { (*slpc).vaddr.cast::<SlpcSharedDataLayout>() }
}

#[inline]
unsafe fn intel_guc_is_ready(guc: *mut IntelGuc) -> bool {
    unsafe { intel_guc_is_fw_running(guc) && (*guc).ct.enabled }
}

#[inline]
unsafe fn has_media_ratio_mode(i915: *mut crate::linux_i915_private::DrmI915Private) -> bool {
    let info = unsafe { INTEL_INFO(i915) };
    assert!(!info.is_null());
    unsafe { (*info).flags[2] & (1 << 5) != 0 }
}

#[inline]
fn div_round_closest(numerator: u32, denominator: u32) -> u32 {
    (numerator + denominator / 2) / denominator
}

/// `intel_guc_slpc_is_supported()` from intel_guc_slpc.h.
pub unsafe fn intel_guc_slpc_is_supported(guc: *mut IntelGuc) -> bool {
    assert!(!guc.is_null());
    unsafe { (*guc).slpc.supported }
}

/// `intel_guc_slpc_is_wanted()` from intel_guc_slpc.h.
pub unsafe fn intel_guc_slpc_is_wanted(guc: *mut IntelGuc) -> bool {
    assert!(!guc.is_null());
    unsafe { (*guc).slpc.selected }
}

/// `intel_guc_slpc_is_used()` from intel_guc_slpc.h.
pub unsafe fn intel_guc_slpc_is_used(guc: *mut IntelGuc) -> bool {
    assert!(!guc.is_null());
    unsafe { intel_guc_submission_is_used(guc) && intel_guc_slpc_is_wanted(guc) }
}

// upstream: intel_guc_slpc.c slpc_to_guc()
unsafe fn slpc_to_guc(slpc: *mut IntelGucSlpc) -> *mut IntelGuc {
    container_of!(slpc, IntelGuc, slpc)
}

// upstream: intel_guc_slpc.c slpc_to_gt()
unsafe fn slpc_to_gt(slpc: *mut IntelGucSlpc) -> *mut IntelGt {
    unsafe { guc_to_gt(slpc_to_guc(slpc)) }
}

// upstream: intel_guc_slpc.c slpc_to_i915()
unsafe fn slpc_to_i915(slpc: *mut IntelGucSlpc) -> *mut crate::linux_i915_private::DrmI915Private {
    unsafe { (*slpc_to_gt(slpc)).i915 }
}

// upstream: intel_guc_slpc.c __detect_slpc_supported()
unsafe fn __detect_slpc_supported(guc: *mut IntelGuc) -> bool {
    let i915 = unsafe { guc_to_i915(guc) };
    unsafe { (*guc).submission_supported && crate::linux::i915::GRAPHICS_VER(i915) >= 12 }
}

// upstream: intel_guc_slpc.c __guc_slpc_selected()
unsafe fn __guc_slpc_selected(guc: *mut IntelGuc) -> bool {
    if !unsafe { intel_guc_slpc_is_supported(guc) } {
        return false;
    }
    unsafe { (*guc).submission_selected }
}

// upstream: intel_guc_slpc.c intel_guc_slpc_init_early()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_slpc_init_early(slpc: *mut IntelGucSlpc) {
    let guc = unsafe { slpc_to_guc(slpc) };
    unsafe {
        (*slpc).supported = __detect_slpc_supported(guc);
        (*slpc).selected = __guc_slpc_selected(guc);
    }
}

// upstream: intel_guc_slpc.c slpc_mem_set_param()
unsafe fn slpc_mem_set_param(data: *mut SlpcSharedDataLayout, id: u32, value: u32) {
    GEM_BUG_ON!(id >= SLPC_MAX_OVERRIDE_PARAMETERS);
    unsafe {
        let bits = ptr::addr_of_mut!((*data).override_params.bits)
            .cast::<u32>()
            .add((id >> 5) as usize);
        ptr::write_unaligned(bits, ptr::read_unaligned(bits) | (1 << (id % 32)));
        let values = ptr::addr_of_mut!((*data).override_params.values)
            .cast::<u32>()
            .add(id as usize);
        ptr::write_unaligned(values, value);
    }
}

// upstream: intel_guc_slpc.c slpc_mem_set_enabled()
unsafe fn slpc_mem_set_enabled(data: *mut SlpcSharedDataLayout, enable_id: u8, disable_id: u8) {
    unsafe {
        slpc_mem_set_param(data, enable_id as u32, 1);
        slpc_mem_set_param(data, disable_id as u32, 0);
    }
}

// upstream: intel_guc_slpc.c slpc_mem_set_disabled()
unsafe fn slpc_mem_set_disabled(data: *mut SlpcSharedDataLayout, enable_id: u8, disable_id: u8) {
    unsafe {
        slpc_mem_set_param(data, disable_id as u32, 1);
        slpc_mem_set_param(data, enable_id as u32, 0);
    }
}

// upstream: intel_guc_slpc.c slpc_get_state()
unsafe fn slpc_get_state(slpc: *mut IntelGucSlpc) -> u32 {
    GEM_BUG_ON!(unsafe { (*slpc).vma.is_null() });
    let data = unsafe { slpc_data(slpc) };
    unsafe {
        drm_clflush_virt_range(data.cast::<c_void>(), size_of::<u32>() as c_ulong);
        ptr::read_unaligned(ptr::addr_of!((*data).header.global_state))
    }
}

// upstream: intel_guc_slpc.c guc_action_slpc_set_param_nb()
unsafe fn guc_action_slpc_set_param_nb(guc: *mut IntelGuc, id: u8, value: u32) -> c_int {
    let request = [
        GUC_ACTION_HOST2GUC_PC_SLPC_REQUEST,
        SLPC_EVENT(SLPC_EVENT_PARAMETER_SET, 2),
        id as u32,
        value,
    ];
    let ret = unsafe {
        intel_guc_ct_send(
            ptr::addr_of_mut!((*guc).ct),
            request.as_ptr(),
            request.len() as u32,
            ptr::null_mut(),
            0,
            crate::guc_ct::CT_SEND_NB,
        )
    };
    if ret > 0 { -EPROTO } else { ret }
}

// upstream: intel_guc_slpc.c slpc_set_param_nb()
unsafe fn slpc_set_param_nb(slpc: *mut IntelGucSlpc, id: u8, value: u32) -> c_int {
    let guc = unsafe { slpc_to_guc(slpc) };
    GEM_BUG_ON!(id as u32 >= SLPC_MAX_PARAM as u32);
    unsafe { guc_action_slpc_set_param_nb(guc, id, value) }
}

// upstream: intel_guc_slpc.c guc_action_slpc_set_param()
unsafe fn guc_action_slpc_set_param(guc: *mut IntelGuc, id: u8, value: u32) -> c_int {
    let request = [
        GUC_ACTION_HOST2GUC_PC_SLPC_REQUEST,
        SLPC_EVENT(SLPC_EVENT_PARAMETER_SET, 2),
        id as u32,
        value,
    ];
    let ret = unsafe {
        intel_guc_ct_send(
            ptr::addr_of_mut!((*guc).ct),
            request.as_ptr(),
            request.len() as u32,
            ptr::null_mut(),
            0,
            0,
        )
    };
    if ret > 0 { -EPROTO } else { ret }
}

// upstream: intel_guc_slpc.c slpc_is_running()
unsafe fn slpc_is_running(slpc: *mut IntelGucSlpc) -> bool {
    unsafe { slpc_get_state(slpc) == SLPC_GLOBAL_STATE_RUNNING }
}

// upstream: intel_guc_slpc.c guc_action_slpc_query()
unsafe fn guc_action_slpc_query(guc: *mut IntelGuc, offset: u32) -> c_int {
    let request = [
        GUC_ACTION_HOST2GUC_PC_SLPC_REQUEST,
        SLPC_EVENT(SLPC_EVENT_QUERY_TASK_STATE, 2),
        offset,
        0,
    ];
    let ret = unsafe {
        intel_guc_ct_send(
            ptr::addr_of_mut!((*guc).ct),
            request.as_ptr(),
            request.len() as u32,
            ptr::null_mut(),
            0,
            0,
        )
    };
    if ret > 0 { -EPROTO } else { ret }
}

// upstream: intel_guc_slpc.c slpc_query_task_state()
unsafe fn slpc_query_task_state(slpc: *mut IntelGucSlpc) -> c_int {
    let guc = unsafe { slpc_to_guc(slpc) };
    let offset = unsafe { intel_guc_ggtt_offset(guc, (*slpc).vma) };
    let ret = unsafe { guc_action_slpc_query(guc, offset) };
    if ret != 0 {
        guc_probe_error!(
            guc,
            "Failed to query task state: %pe\n",
            crate::linux_config::ERR_PTR::<c_void>(ret),
        );
    }
    unsafe {
        drm_clflush_virt_range(
            (*slpc).vaddr.cast::<c_void>(),
            SLPC_PAGE_SIZE_BYTES as c_ulong,
        )
    };
    ret
}

// upstream: intel_guc_slpc.c slpc_set_param()
unsafe fn slpc_set_param(slpc: *mut IntelGucSlpc, id: u8, value: u32) -> c_int {
    let guc = unsafe { slpc_to_guc(slpc) };
    GEM_BUG_ON!(id as u32 >= SLPC_MAX_PARAM as u32);
    let ret = unsafe { guc_action_slpc_set_param(guc, id, value) };
    if ret != 0 {
        guc_probe_error!(
            guc,
            "Failed to set param %d to %u: %pe\n",
            id,
            value,
            crate::linux_config::ERR_PTR::<c_void>(ret),
        );
    }
    ret
}

// upstream: intel_guc_slpc.c slpc_force_min_freq()
unsafe fn slpc_force_min_freq(slpc: *mut IntelGucSlpc, freq: u32) -> c_int {
    let guc = unsafe { slpc_to_guc(slpc) };
    let i915 = unsafe { slpc_to_i915(slpc) };
    lockdep_assert_held!(unsafe { &(*slpc).lock });
    if !unsafe { intel_guc_is_ready(guc) } {
        return -ENODEV;
    }

    let rpm = unsafe { ptr::addr_of_mut!((*i915).runtime_pm) };
    let mut ret = -ENODEV;
    with_intel_runtime_pm!(rpm, wakeref, {
        ret = unsafe { slpc_set_param_nb(slpc, SLPC_PARAM_GLOBAL_MIN_GT_UNSLICE_FREQ_MHZ, freq) };
        if ret != 0 {
            guc_notice!(
                guc,
                "Failed to send set_param for min freq(%d): %pe\n",
                freq,
                crate::linux_config::ERR_PTR::<c_void>(ret),
            );
        }
    });
    ret
}

// upstream: intel_guc_slpc.c slpc_boost_work()
unsafe extern "C" fn slpc_boost_work(work: *mut WorkStruct) {
    let slpc = container_of!(work, IntelGucSlpc, boost_work);
    unsafe { mutex_lock(&mut (*slpc).lock) };
    if atomic_read(unsafe { &(*slpc).num_waiters }) != 0 {
        let ret = unsafe { slpc_force_min_freq(slpc, (*slpc).boost_freq) };
        if ret == 0 {
            unsafe { (*slpc).num_boosts += 1 };
        }
    }
    unsafe { mutex_unlock(&mut (*slpc).lock) };
}

// upstream: intel_guc_slpc.c intel_guc_slpc_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_slpc_init(slpc: *mut IntelGucSlpc) -> c_int {
    let guc = unsafe { slpc_to_guc(slpc) };
    let size = ((size_of::<SlpcSharedDataLayout>() + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)) as u32;
    GEM_BUG_ON!(!unsafe { (*slpc).vma }.is_null());

    let err = unsafe {
        intel_guc_allocate_and_map_vma(
            guc,
            size,
            ptr::addr_of_mut!((*slpc).vma),
            ptr::addr_of_mut!((*slpc).vaddr).cast::<*mut c_void>(),
        )
    };
    if err != 0 {
        guc_probe_error!(
            guc,
            "Failed to allocate SLPC struct: %pe\n",
            crate::linux_config::ERR_PTR::<c_void>(err),
        );
        return err;
    }

    unsafe {
        (*slpc).max_freq_softlimit = 0;
        (*slpc).min_freq_softlimit = 0;
        (*slpc).ignore_eff_freq = false;
        (*slpc).min_is_rpmax = false;
        (*slpc).boost_freq = 0;
        atomic_set(&mut (*slpc).num_waiters, 0);
        (*slpc).num_boosts = 0;
        (*slpc).media_ratio_mode = SLPC_MEDIA_RATIO_MODE_DYNAMIC_CONTROL;
        (*slpc).power_profile = SLPC_POWER_PROFILES_BASE;
        mutex_init(&mut (*slpc).lock);
        INIT_WORK_C(&mut (*slpc).boost_work, slpc_boost_work);
    }
    0
}

// upstream: intel_guc_slpc.c slpc_global_state_to_string()
unsafe fn slpc_global_state_to_string(state: u32) -> &'static str {
    match state {
        SLPC_GLOBAL_STATE_NOT_RUNNING => "not running",
        SLPC_GLOBAL_STATE_INITIALIZING => "initializing",
        SLPC_GLOBAL_STATE_RESETTING => "resetting",
        SLPC_GLOBAL_STATE_RUNNING => "running",
        SLPC_GLOBAL_STATE_SHUTTING_DOWN => "shutting down",
        SLPC_GLOBAL_STATE_ERROR => "error",
        _ => "unknown",
    }
}

// upstream: intel_guc_slpc.c slpc_get_state_string()
unsafe fn slpc_get_state_string(slpc: *mut IntelGucSlpc) -> &'static str {
    unsafe { slpc_global_state_to_string(slpc_get_state(slpc)) }
}

// upstream: intel_guc_slpc.c guc_action_slpc_reset()
unsafe fn guc_action_slpc_reset(guc: *mut IntelGuc, offset: u32) -> c_int {
    let request = [
        GUC_ACTION_HOST2GUC_PC_SLPC_REQUEST,
        SLPC_EVENT(SLPC_EVENT_RESET, 2),
        offset,
        0,
    ];
    let ret = unsafe {
        intel_guc_ct_send(
            ptr::addr_of_mut!((*guc).ct),
            request.as_ptr(),
            request.len() as u32,
            ptr::null_mut(),
            0,
            0,
        )
    };
    if ret > 0 { -EPROTO } else { ret }
}

// upstream: intel_guc_slpc.c slpc_reset()
unsafe fn slpc_reset(slpc: *mut IntelGucSlpc) -> c_int {
    let guc = unsafe { slpc_to_guc(slpc) };
    let offset = unsafe { intel_guc_ggtt_offset(guc, (*slpc).vma) };
    let ret = unsafe { guc_action_slpc_reset(guc, offset) };
    if ret < 0 {
        guc_probe_error!(
            guc,
            "SLPC reset action failed: %pe\n",
            crate::linux_config::ERR_PTR::<c_void>(ret),
        );
        return ret;
    }

    if ret == 0 && wait_for!(unsafe { slpc_is_running(slpc) }, SLPC_RESET_TIMEOUT_MS) {
        guc_probe_error!(guc, "SLPC not enabled! State = %s\n", unsafe {
            slpc_get_state_string(slpc)
        },);
        return -EIO;
    }
    0
}

// upstream: intel_guc_slpc.c slpc_decode_min_freq()
unsafe fn slpc_decode_min_freq(slpc: *mut IntelGucSlpc) -> u32 {
    GEM_BUG_ON!(unsafe { (*slpc).vma.is_null() });
    let data = unsafe { slpc_data(slpc) };
    let freq = unsafe { ptr::read_unaligned(ptr::addr_of!((*data).task_state_data.freq)) };
    let raw = (freq & SLPC_MIN_UNSLICE_FREQ_MASK) >> 8;
    div_round_closest(raw * GT_FREQUENCY_MULTIPLIER, GEN9_FREQ_SCALER)
}

// upstream: intel_guc_slpc.c slpc_decode_max_freq()
unsafe fn slpc_decode_max_freq(slpc: *mut IntelGucSlpc) -> u32 {
    GEM_BUG_ON!(unsafe { (*slpc).vma.is_null() });
    let data = unsafe { slpc_data(slpc) };
    let freq = unsafe { ptr::read_unaligned(ptr::addr_of!((*data).task_state_data.freq)) };
    let raw = freq & SLPC_MAX_UNSLICE_FREQ_MASK;
    div_round_closest(raw * GT_FREQUENCY_MULTIPLIER, GEN9_FREQ_SCALER)
}

// upstream: intel_guc_slpc.c slpc_shared_data_reset()
unsafe fn slpc_shared_data_reset(slpc: *mut IntelGucSlpc) {
    let i915 = unsafe { slpc_to_i915(slpc) };
    let data = unsafe { slpc_data(slpc) };
    unsafe { ptr::write_bytes(data.cast::<u8>(), 0, size_of::<SlpcSharedDataLayout>()) };
    unsafe {
        ptr::write_unaligned(
            ptr::addr_of_mut!((*data).header.size),
            size_of::<SlpcSharedDataLayout>() as u32,
        );
        slpc_mem_set_enabled(
            data,
            SLPC_PARAM_TASK_ENABLE_GTPERF,
            SLPC_PARAM_TASK_DISABLE_GTPERF,
        );
    }

    if unsafe { GRAPHICS_VER_FULL(i915) } < IP_VER(12, 70) {
        unsafe {
            slpc_mem_set_disabled(
                data,
                SLPC_PARAM_TASK_ENABLE_BALANCER,
                SLPC_PARAM_TASK_DISABLE_BALANCER,
            );
            slpc_mem_set_disabled(
                data,
                SLPC_PARAM_TASK_ENABLE_DCC,
                SLPC_PARAM_TASK_DISABLE_DCC,
            );
        }
    }
}

// upstream: intel_guc_slpc.c intel_guc_slpc_set_max_freq()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_slpc_set_max_freq(slpc: *mut IntelGucSlpc, value: u32) -> c_int {
    if value < unsafe { (*slpc).min_freq }
        || value > unsafe { (*slpc).rp0_freq }
        || value < unsafe { (*slpc).min_freq_softlimit }
    {
        return -EINVAL;
    }

    let i915 = unsafe { slpc_to_i915(slpc) };
    let rpm = unsafe { ptr::addr_of_mut!((*i915).runtime_pm) };
    let mut ret = -ENODEV;
    with_intel_runtime_pm!(rpm, wakeref, {
        ret = unsafe { slpc_set_param(slpc, SLPC_PARAM_GLOBAL_MAX_GT_UNSLICE_FREQ_MHZ, value) };
        if ret != 0 {
            ret = -EIO;
        }
    });

    if ret == 0 {
        unsafe { (*slpc).max_freq_softlimit = value };
    }
    ret
}

// upstream: intel_guc_slpc.c intel_guc_slpc_get_max_freq()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_slpc_get_max_freq(
    slpc: *mut IntelGucSlpc,
    value: *mut u32,
) -> c_int {
    let i915 = unsafe { slpc_to_i915(slpc) };
    let rpm = unsafe { ptr::addr_of_mut!((*i915).runtime_pm) };
    let mut ret = -ENODEV;
    with_intel_runtime_pm!(rpm, wakeref, {
        ret = unsafe { slpc_query_task_state(slpc) };
        if ret == 0 {
            unsafe { ptr::write(value, slpc_decode_max_freq(slpc)) };
        }
    });
    ret
}

// upstream: intel_guc_slpc.c intel_guc_slpc_set_ignore_eff_freq()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_slpc_set_ignore_eff_freq(
    slpc: *mut IntelGucSlpc,
    value: bool,
) -> c_int {
    let i915 = unsafe { slpc_to_i915(slpc) };
    unsafe { mutex_lock(&mut (*slpc).lock) };
    let rpm = unsafe { ptr::addr_of_mut!((*i915).runtime_pm) };
    let wakeref = crate::linux_pm::intel_runtime_pm_get(rpm);
    if wakeref.is_null() {
        unsafe { mutex_unlock(&mut (*slpc).lock) };
        return -ENODEV;
    }

    let mut ret =
        unsafe { slpc_set_param(slpc, SLPC_PARAM_IGNORE_EFFICIENT_FREQUENCY, value as u32) };
    if ret != 0 {
        guc_probe_error!(
            unsafe { slpc_to_guc(slpc) },
            "Failed to set efficient freq(%d): %pe\n",
            value,
            crate::linux_config::ERR_PTR::<c_void>(ret),
        );
    } else {
        unsafe { (*slpc).ignore_eff_freq = value };
        if value {
            ret = unsafe {
                slpc_set_param(
                    slpc,
                    SLPC_PARAM_GLOBAL_MIN_GT_UNSLICE_FREQ_MHZ,
                    (*slpc).min_freq,
                )
            };
        }
    }

    crate::linux_pm::intel_runtime_pm_put(rpm, wakeref);
    unsafe { mutex_unlock(&mut (*slpc).lock) };
    ret
}

// upstream: intel_guc_slpc.c intel_guc_slpc_set_min_freq()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_slpc_set_min_freq(slpc: *mut IntelGucSlpc, value: u32) -> c_int {
    if value < unsafe { (*slpc).min_freq }
        || value > unsafe { (*slpc).rp0_freq }
        || value > unsafe { (*slpc).max_freq_softlimit }
    {
        return -EINVAL;
    }

    let i915 = unsafe { slpc_to_i915(slpc) };
    unsafe { mutex_lock(&mut (*slpc).lock) };
    let rpm = unsafe { ptr::addr_of_mut!((*i915).runtime_pm) };
    let wakeref = crate::linux_pm::intel_runtime_pm_get(rpm);
    if wakeref.is_null() {
        unsafe { mutex_unlock(&mut (*slpc).lock) };
        return -ENODEV;
    }

    let mut ret = unsafe { slpc_set_param(slpc, SLPC_PARAM_GLOBAL_MIN_GT_UNSLICE_FREQ_MHZ, value) };
    if ret == 0 {
        unsafe { (*slpc).min_freq_softlimit = value };
    }
    crate::linux_pm::intel_runtime_pm_put(rpm, wakeref);
    unsafe { mutex_unlock(&mut (*slpc).lock) };

    if ret != 0 {
        ret = -EIO;
    }
    ret
}

// upstream: intel_guc_slpc.c intel_guc_slpc_get_min_freq()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_slpc_get_min_freq(
    slpc: *mut IntelGucSlpc,
    value: *mut u32,
) -> c_int {
    let i915 = unsafe { slpc_to_i915(slpc) };
    let rpm = unsafe { ptr::addr_of_mut!((*i915).runtime_pm) };
    let mut ret = -ENODEV;
    with_intel_runtime_pm!(rpm, wakeref, {
        ret = unsafe { slpc_query_task_state(slpc) };
        if ret == 0 {
            unsafe { ptr::write(value, slpc_decode_min_freq(slpc)) };
        }
    });
    ret
}

// upstream: intel_guc_slpc.c intel_guc_slpc_set_strategy()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_slpc_set_strategy(slpc: *mut IntelGucSlpc, value: u32) -> c_int {
    let i915 = unsafe { slpc_to_i915(slpc) };
    let rpm = unsafe { ptr::addr_of_mut!((*i915).runtime_pm) };
    let mut ret = -ENODEV;
    with_intel_runtime_pm!(rpm, wakeref, {
        ret = unsafe { slpc_set_param(slpc, SLPC_PARAM_STRATEGIES, value) };
    });
    ret
}

// upstream: intel_guc_slpc.c intel_guc_slpc_set_media_ratio_mode()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_slpc_set_media_ratio_mode(
    slpc: *mut IntelGucSlpc,
    value: u32,
) -> c_int {
    let i915 = unsafe { slpc_to_i915(slpc) };
    if !unsafe { has_media_ratio_mode(i915) } {
        return -ENODEV;
    }
    let rpm = unsafe { ptr::addr_of_mut!((*i915).runtime_pm) };
    let mut ret = -ENODEV;
    with_intel_runtime_pm!(rpm, wakeref, {
        ret = unsafe { slpc_set_param(slpc, SLPC_PARAM_MEDIA_FF_RATIO_MODE, value) };
    });
    ret
}

// upstream: intel_guc_slpc.c intel_guc_slpc_set_power_profile()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_slpc_set_power_profile(
    slpc: *mut IntelGucSlpc,
    value: u32,
) -> c_int {
    if value > SLPC_POWER_PROFILES_POWER_SAVING {
        return -EINVAL;
    }

    let i915 = unsafe { slpc_to_i915(slpc) };
    unsafe { mutex_lock(&mut (*slpc).lock) };
    let rpm = unsafe { ptr::addr_of_mut!((*i915).runtime_pm) };
    let wakeref = crate::linux_pm::intel_runtime_pm_get(rpm);
    if wakeref.is_null() {
        unsafe { mutex_unlock(&mut (*slpc).lock) };
        return -ENODEV;
    }

    let ret = unsafe { slpc_set_param(slpc, SLPC_PARAM_POWER_PROFILE, value) };
    if ret != 0 {
        guc_err!(
            unsafe { slpc_to_guc(slpc) },
            "Failed to set power profile to %d: %pe\n",
            value,
            crate::linux_config::ERR_PTR::<c_void>(ret),
        );
    } else {
        unsafe { (*slpc).power_profile = value };
    }
    crate::linux_pm::intel_runtime_pm_put(rpm, wakeref);
    unsafe { mutex_unlock(&mut (*slpc).lock) };
    ret
}

// upstream: intel_guc_slpc.c intel_guc_pm_intrmsk_enable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_pm_intrmsk_enable(gt: *mut IntelGt) {
    let pm_intrmsk_mbz = ARAT_EXPIRED_INTRMSK;
    unsafe { intel_uncore_rmw((*gt).uncore, GEN6_PMINTRMSK, pm_intrmsk_mbz, 0) };
}

// upstream: intel_guc_slpc.c slpc_set_softlimits()
unsafe fn slpc_set_softlimits(slpc: *mut IntelGucSlpc) -> c_int {
    let gt = unsafe { slpc_to_gt(slpc) };
    let mut ret = 0;
    if unsafe { (*slpc).max_freq_softlimit == 0 } {
        unsafe {
            (*slpc).max_freq_softlimit = (*slpc).rp0_freq;
            (*gt).defaults.max_freq = (*slpc).max_freq_softlimit;
        }
    } else if unsafe { (*slpc).max_freq_softlimit != (*slpc).rp0_freq } {
        ret = unsafe { intel_guc_slpc_set_max_freq(slpc, (*slpc).max_freq_softlimit) };
    }
    if ret != 0 {
        return ret;
    }

    if unsafe { (*slpc).min_freq_softlimit == 0 } {
        unsafe {
            (*slpc).min_freq_softlimit = (*slpc).min_freq;
            (*gt).defaults.min_freq = (*slpc).min_freq_softlimit;
        }
    } else {
        return unsafe { intel_guc_slpc_set_min_freq(slpc, (*slpc).min_freq_softlimit) };
    }
    0
}

// upstream: intel_guc_slpc.c is_slpc_min_freq_rpmax()
unsafe fn is_slpc_min_freq_rpmax(slpc: *mut IntelGucSlpc) -> bool {
    let mut slpc_min_freq = 0u32;
    let ret = unsafe { intel_guc_slpc_get_min_freq(slpc, &mut slpc_min_freq) };
    if ret != 0 {
        guc_err!(
            unsafe { slpc_to_guc(slpc) },
            "Failed to get min freq: %pe\n",
            crate::linux_config::ERR_PTR::<c_void>(ret),
        );
        return false;
    }
    slpc_min_freq == SLPC_MAX_FREQ_MHZ as u32
}

// upstream: intel_guc_slpc.c update_server_min_softlimit()
unsafe fn update_server_min_softlimit(slpc: *mut IntelGucSlpc) {
    if unsafe { (*slpc).min_freq_softlimit == 0 } && unsafe { is_slpc_min_freq_rpmax(slpc) } {
        let gt = unsafe { slpc_to_gt(slpc) };
        unsafe {
            (*slpc).min_is_rpmax = true;
            (*slpc).min_freq_softlimit = (*slpc).rp0_freq;
            (*gt).defaults.min_freq = (*slpc).min_freq_softlimit;
        }
    }
}

// upstream: intel_guc_slpc.c slpc_use_fused_rp0()
unsafe fn slpc_use_fused_rp0(slpc: *mut IntelGucSlpc) -> c_int {
    unsafe {
        slpc_set_param(
            slpc,
            SLPC_PARAM_GLOBAL_MAX_GT_UNSLICE_FREQ_MHZ,
            (*slpc).rp0_freq,
        )
    }
}

// upstream: intel_guc_slpc.c slpc_get_rp_values()
unsafe fn slpc_get_rp_values(slpc: *mut IntelGucSlpc) {
    let gt = unsafe { slpc_to_gt(slpc) };
    let rps = unsafe { ptr::addr_of_mut!((*gt).rps) };
    let mut caps: IntelRpsFreqCaps = unsafe { core::mem::zeroed() };
    unsafe { gen6_rps_get_freq_caps(rps, &mut caps) };
    unsafe {
        (*slpc).rp0_freq = intel_gpu_freq(rps, caps.rp0_freq as c_int) as u32;
        (*slpc).rp1_freq = intel_gpu_freq(rps, caps.rp1_freq as c_int) as u32;
        (*slpc).min_freq = intel_gpu_freq(rps, caps.min_freq as c_int) as u32;
        if (*slpc).boost_freq == 0 {
            (*slpc).boost_freq = (*slpc).rp0_freq;
        }
    }
}

// upstream: intel_guc_slpc.c intel_guc_slpc_enable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_slpc_enable(slpc: *mut IntelGucSlpc) -> c_int {
    let guc = unsafe { slpc_to_guc(slpc) };
    GEM_BUG_ON!(unsafe { (*slpc).vma.is_null() });
    unsafe { slpc_shared_data_reset(slpc) };

    let mut ret = unsafe { slpc_reset(slpc) };
    if ret < 0 {
        guc_probe_error!(
            guc,
            "SLPC Reset event returned: %pe\n",
            crate::linux_config::ERR_PTR::<c_void>(ret),
        );
        return ret;
    }

    ret = unsafe { slpc_query_task_state(slpc) };
    if ret < 0 {
        return ret;
    }

    unsafe { intel_guc_pm_intrmsk_enable(slpc_to_gt(slpc)) };
    unsafe { slpc_get_rp_values(slpc) };
    unsafe { update_server_min_softlimit(slpc) };

    ret = unsafe { slpc_use_fused_rp0(slpc) };
    if ret != 0 {
        guc_probe_error!(
            guc,
            "Failed to set SLPC max to RP0: %pe\n",
            crate::linux_config::ERR_PTR::<c_void>(ret),
        );
        return ret;
    }

    let _ = unsafe { intel_guc_slpc_set_ignore_eff_freq(slpc, (*slpc).ignore_eff_freq) };
    ret = unsafe { slpc_set_softlimits(slpc) };
    if ret != 0 {
        guc_probe_error!(
            guc,
            "Failed to set SLPC softlimits: %pe\n",
            crate::linux_config::ERR_PTR::<c_void>(ret),
        );
        return ret;
    }

    let _ = unsafe { intel_guc_slpc_set_media_ratio_mode(slpc, (*slpc).media_ratio_mode) };
    let _ = unsafe { intel_guc_slpc_set_strategy(slpc, SLPC_OPTIMIZED_STRATEGY_COMPUTE) };
    ret = unsafe { intel_guc_slpc_set_power_profile(slpc, (*slpc).power_profile) };
    if ret != 0 {
        guc_probe_error!(
            guc,
            "Failed to set SLPC power profile: %pe\n",
            crate::linux_config::ERR_PTR::<c_void>(ret),
        );
        return ret;
    }
    0
}

// upstream: intel_guc_slpc.c intel_guc_slpc_set_boost_freq()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_slpc_set_boost_freq(
    slpc: *mut IntelGucSlpc,
    value: u32,
) -> c_int {
    if value < unsafe { (*slpc).min_freq } || value > unsafe { (*slpc).rp0_freq } {
        return -EINVAL;
    }
    unsafe { mutex_lock(&mut (*slpc).lock) };
    let mut ret = 0;
    if unsafe { (*slpc).boost_freq != value } {
        if atomic_read(unsafe { &(*slpc).num_waiters }) != 0 {
            ret = unsafe { slpc_force_min_freq(slpc, value) };
            if ret != 0 {
                ret = -EIO;
                unsafe { mutex_unlock(&mut (*slpc).lock) };
                return ret;
            }
        }
        unsafe { (*slpc).boost_freq = value };
    }
    unsafe { mutex_unlock(&mut (*slpc).lock) };
    ret
}

// upstream: intel_guc_slpc.c intel_guc_slpc_dec_waiters()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_slpc_dec_waiters(slpc: *mut IntelGucSlpc) {
    unsafe { mutex_lock(&mut (*slpc).lock) };
    if atomic_dec_and_test(unsafe { &mut (*slpc).num_waiters }) {
        let _ = unsafe { slpc_force_min_freq(slpc, (*slpc).min_freq_softlimit) };
    }
    unsafe { mutex_unlock(&mut (*slpc).lock) };
}

// upstream: intel_guc_slpc.c intel_guc_slpc_print_info()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_slpc_print_info(
    slpc: *mut IntelGucSlpc,
    printer: *mut DrmPrinter,
) -> c_int {
    GEM_BUG_ON!(unsafe { (*slpc).vma.is_null() });
    let i915 = unsafe { slpc_to_i915(slpc) };
    let data = unsafe { slpc_data(slpc) };
    let task = unsafe { ptr::addr_of!((*data).task_state_data) };
    let rpm = unsafe { ptr::addr_of_mut!((*i915).runtime_pm) };
    let mut ret = -ENODEV;
    with_intel_runtime_pm!(rpm, wakeref, {
        ret = unsafe { slpc_query_task_state(slpc) };
        if ret == 0 {
            let status = unsafe { ptr::read_unaligned(ptr::addr_of!((*task).status)) };
            drm_printf!(printer, "\tSLPC state: %s\n", unsafe {
                slpc_get_state_string(slpc)
            });
            drm_printf!(
                printer,
                "\tGTPERF task active: %s\n",
                str_yes_no(status & SLPC_GTPERF_TASK_ENABLED != 0)
            );
            drm_printf!(
                printer,
                "\tDCC enabled: %s\n",
                str_yes_no(status & SLPC_DCC_TASK_ENABLED != 0)
            );
            drm_printf!(
                printer,
                "\tDCC in: %s\n",
                str_yes_no(status & SLPC_IN_DCC != 0)
            );
            drm_printf!(
                printer,
                "\tBalancer enabled: %s\n",
                str_yes_no(status & SLPC_BALANCER_ENABLED != 0)
            );
            drm_printf!(
                printer,
                "\tIBC enabled: %s\n",
                str_yes_no(status & SLPC_IBC_TASK_ENABLED != 0)
            );
            drm_printf!(
                printer,
                "\tBalancer IA LMT enabled: %s\n",
                str_yes_no(status & SLPC_BALANCER_IA_LMT_ENABLED != 0)
            );
            drm_printf!(
                printer,
                "\tBalancer IA LMT active: %s\n",
                str_yes_no(status & SLPC_BALANCER_IA_LMT_ACTIVE != 0)
            );
            drm_printf!(printer, "\tMax freq: %u MHz\n", unsafe {
                slpc_decode_max_freq(slpc)
            });
            drm_printf!(printer, "\tMin freq: %u MHz\n", unsafe {
                slpc_decode_min_freq(slpc)
            });
            drm_printf!(printer, "\twaitboosts: %u\n", unsafe { (*slpc).num_boosts });
            drm_printf!(
                printer,
                "\tBoosts outstanding: %u\n",
                atomic_read(unsafe { &(*slpc).num_waiters }),
            );
        }
    });
    ret
}

// upstream: intel_guc_slpc.c intel_guc_slpc_fini()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_slpc_fini(slpc: *mut IntelGucSlpc) {
    if unsafe { (*slpc).vma.is_null() } {
        return;
    }
    unsafe { i915_vma_unpin_and_release(ptr::addr_of_mut!((*slpc).vma), I915_VMA_RELEASE_MAP) };
}
