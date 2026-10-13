// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! TEMPORARY definitions for callees owned by deferred optional features (OPT).
//! Replaced when optional feature drivers land (B9).

#![allow(non_snake_case, dead_code, unsafe_code)]

use crate::intel_display_core_types_upstream::IntelDisplay;

pub use crate::intel_opregion_upstream::{
    intel_opregion_headless_sku, intel_opregion_register, intel_opregion_unregister,
};

// upstream: intel_audio.c intel_audio_hooks_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_audio_hooks_init(_display: *mut IntelDisplay) {}

// upstream: intel_fdi.c intel_fdi_init_hook()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_fdi_init_hook(_display: *mut IntelDisplay) {}

// upstream: intel_psr.c intel_psr_dc5_dc6_wa_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_psr_dc5_dc6_wa_init(_display: *mut IntelDisplay) {}

// upstream: intel_fbc.c intel_fbc_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_fbc_init(_display: *mut IntelDisplay) {}

// upstream: intel_fdi.c intel_fdi_pll_freq_update()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_fdi_pll_freq_update(_display: *mut IntelDisplay) {}

// upstream: intel_dp_tunnel.c intel_dp_tunnel_mgr_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_tunnel_mgr_init(_display: *mut IntelDisplay) -> i32 {
    0
}

// upstream: intel_acpi.c intel_acpi_assign_connector_fwnodes()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_acpi_assign_connector_fwnodes(_display: *mut IntelDisplay) {}

// upstream: intel_hdcp.c intel_hdcp_component_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hdcp_component_init(_display: *mut IntelDisplay) {}

// upstream: intel_hdcp.c intel_hdcp_component_fini()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hdcp_component_fini(_display: *mut IntelDisplay) {}

// upstream: intel_flipq.c intel_flipq_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_flipq_init(_display: *mut IntelDisplay) {}

// upstream: intel_overlay.c intel_overlay_setup()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_overlay_setup(_display: *mut IntelDisplay) {}

// upstream: intel_overlay.c intel_overlay_cleanup()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_overlay_cleanup(_display: *mut IntelDisplay) {}

// upstream: intel_vga.c intel_vga_register()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_vga_register(_display: *mut IntelDisplay) {}

// upstream: intel_vga.c intel_vga_unregister()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_vga_unregister(_display: *mut IntelDisplay) {}

// upstream: intel_acpi.c intel_acpi_video_register()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_acpi_video_register(_display: *mut IntelDisplay) {}

// upstream: intel_audio.c intel_audio_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_audio_init(_display: *mut IntelDisplay) {}

// upstream: intel_audio.c intel_audio_register()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_audio_register(_display: *mut IntelDisplay) {}

// upstream: intel_audio.c intel_audio_deinit()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_audio_deinit(_display: *mut IntelDisplay) {}

// upstream: intel_display_debugfs.c intel_display_debugfs_register()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_debugfs_register(_display: *mut IntelDisplay) {}

// upstream: intel_acpi.c intel_register_dsm_handler()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_register_dsm_handler() {}

// upstream: intel_acpi.c intel_unregister_dsm_handler()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_unregister_dsm_handler() {}

// upstream: intel_dp_mst.c intel_dp_mst_suspend()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_mst_suspend(_display: *mut IntelDisplay) {}

// upstream: intel_dp_mst.c intel_dp_mst_resume()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_mst_resume(_display: *mut IntelDisplay) {}

// upstream: intel_vrr.c intel_vrr_send_push()/intel_vrr_check_push_sent()
// VRR is an explicit compile-out feature in the ADL-P bring-up configuration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_vrr_send_push(
    _dsb: *mut core::ffi::c_void,
    _state: *const crate::intel_display_types_upstream::IntelCrtcState,
) {
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_vrr_check_push_sent(
    _dsb: *mut core::ffi::c_void,
    _state: *const crate::intel_display_types_upstream::IntelCrtcState,
) {
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_vrr_is_push_sent(
    _state: *const crate::intel_display_types_upstream::IntelCrtcState,
) -> bool {
    false
}

// upstream: intel_dp_tunnel.c intel_dp_tunnel_mgr_cleanup()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_tunnel_mgr_cleanup(_display: *mut IntelDisplay) {}

// upstream: intel_fbc.c intel_fbc_cleanup()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_fbc_cleanup(_display: *mut IntelDisplay) {}

// upstream: intel_cx0_phy.c intel_cx0_pll_power_save_wa()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_cx0_pll_power_save_wa(_display: *mut IntelDisplay) {}
