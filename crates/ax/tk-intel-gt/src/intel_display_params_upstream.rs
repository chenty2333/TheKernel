// SPDX-License-Identifier: MIT
// Copyright © 2023 Intel Corporation
// Source: drivers/gpu/drm/i915/display/intel_display_params.c

#![allow(non_snake_case, dead_code, unsafe_code)]

use core::ffi::c_char;
use crate::intel_display_params_types_upstream::IntelDisplayParams;
use tk_drm_upstream::drm_print_types::DrmPrinter;

// upstream: intel_display_params.c intel_display_modparams
#[unsafe(no_mangle)]
pub static mut intel_display_modparams: IntelDisplayParams = IntelDisplayParams {
    dmc_firmware_path: core::ptr::null_mut(),
    vbt_firmware: core::ptr::null_mut(),
    lvds_channel_mode: 0,
    panel_use_ssc: -1,
    vbt_sdvo_panel_type: -1,
    enable_dc: -1,
    enable_dpt: true,
    enable_dsb: true,
    enable_flipq: false,
    enable_sagv: true,
    disable_power_well: -1,
    enable_ips: true,
    _pad_41: [0; 3],
    invert_brightness: 0,
    edp_vswing: 0,
    enable_dpcd_backlight: -1,
    load_detect_test: false,
    force_reset_modeset_test: false,
    disable_display: false,
    verbose_state_checks: true,
    nuclear_pageflip: false,
    enable_dp_mst: true,
    _pad_62: [0; 2],
    enable_fbc: -1,
    enable_psr: -1,
    enable_panel_replay: -1,
    psr_safest_params: false,
    enable_psr2_sel_fetch: true,
    _pad_78: [0; 2],
    enable_dmc_wl: -1,
    _pad_84: [0; 4],
};

unsafe fn kstrdup_c(src: *const c_char) -> *mut c_char {
    if src.is_null() {
        return core::ptr::null_mut();
    }
    let mut len = 0usize;
    while *src.add(len) != 0 {
        len += 1;
    }
    let mem = tk_linux_kapi::memory::kmalloc(
        len + 1,
        tk_linux_kapi::config::GFP_ATOMIC,
    ).cast::<c_char>();
    if !mem.is_null() {
        core::ptr::copy_nonoverlapping(src, mem, len + 1);
    }
    mem
}

// upstream: intel_display_params.c intel_display_params_dump()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_params_dump(
    params: *const IntelDisplayParams,
    driver_name: *const c_char,
    p: *mut DrmPrinter,
) {
    if params.is_null() || p.is_null() {
        return;
    }
    // We can print key params if printer is present
    let _ = driver_name;
}

// upstream: intel_display_params.c intel_display_params_copy()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_params_copy(dest: *mut IntelDisplayParams) {
    if dest.is_null() {
        return;
    }
    core::ptr::copy_nonoverlapping(core::ptr::addr_of!(intel_display_modparams), dest, 1);
    if !intel_display_modparams.dmc_firmware_path.is_null() {
        (*dest).dmc_firmware_path = kstrdup_c(intel_display_modparams.dmc_firmware_path);
    }
    if !intel_display_modparams.vbt_firmware.is_null() {
        (*dest).vbt_firmware = kstrdup_c(intel_display_modparams.vbt_firmware);
    }
}

// upstream: intel_display_params.c intel_display_params_free()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_params_free(params: *mut IntelDisplayParams) {
    if params.is_null() {
        return;
    }
    if !(*params).dmc_firmware_path.is_null() {
        tk_linux_kapi::memory::kfree((*params).dmc_firmware_path);
        (*params).dmc_firmware_path = core::ptr::null_mut();
    }
    if !(*params).vbt_firmware.is_null() {
        tk_linux_kapi::memory::kfree((*params).vbt_firmware);
        (*params).vbt_firmware = core::ptr::null_mut();
    }
}
