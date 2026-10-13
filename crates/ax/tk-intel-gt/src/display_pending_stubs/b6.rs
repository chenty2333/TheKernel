// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! TEMPORARY definitions for callees owned by batch B6 (DP, HDMI, GMBUS, hotplug, panel, PPS).
//! Replaced by intel_dp_upstream.rs, intel_hdmi_upstream.rs,
//! intel_gmbus_upstream.rs, intel_hotplug_upstream.rs, intel_pps_upstream.rs (B6).

#![allow(non_snake_case, dead_code, unsafe_code)]

use crate::intel_display_core_types_upstream::IntelDisplay;
// B6a: external single-stream DisplayPort runtime.  Keep these re-exports in
// the batch integration namespace while the remaining B6 modules land.
pub use crate::intel_dp_upstream::{
    intel_aux_power_domain, intel_digital_port_connected, intel_digital_port_connected_locked,
    intel_digital_port_lock, intel_digital_port_unlock, intel_dp_bw_fec_overhead,
    intel_dp_check_link_state, intel_dp_cleanup_connector, intel_dp_common_rate,
    intel_dp_compute_config, intel_dp_compute_config_late, intel_dp_compute_config_limits,
    intel_dp_compute_min_hblank, intel_dp_compute_rate, intel_dp_dpcd_set_probe,
    intel_dp_effective_data_rate, intel_dp_encoder_flush_work, intel_dp_encoder_shutdown,
    intel_dp_encoder_suspend, intel_dp_get_active_pipes, intel_dp_hpd_pulse,
    intel_dp_initial_fastset_check, intel_dp_invalidate_source_oui, intel_dp_is_edp,
    intel_dp_is_port_edp, intel_dp_is_uhbr, intel_dp_limited_color_range,
    intel_dp_link_bw_overhead, intel_dp_link_check, intel_dp_link_config_get,
    intel_dp_link_config_index, intel_dp_link_params_valid, intel_dp_link_required,
    intel_dp_link_symbol_clock, intel_dp_link_symbol_size, intel_dp_max_common_lane_count,
    intel_dp_max_common_rate, intel_dp_max_hdisplay_per_pipe, intel_dp_max_lane_count,
    intel_dp_max_link_data_rate, intel_dp_max_link_rate, intel_dp_max_source_lane_count,
    intel_dp_min_bpp, intel_dp_mode_to_fec_clock, intel_dp_mode_valid, intel_dp_mst_active_streams,
    intel_dp_output_format, intel_dp_output_format_link_bpp_x16,
    intel_dp_queue_modeset_retry_for_link, intel_dp_rate_index, intel_dp_rate_select,
    intel_dp_reset_link_params, intel_dp_sdp_min_guardband, intel_dp_set_link_params,
    intel_dp_set_power, intel_dp_source_supports_tps3, intel_dp_source_supports_tps4,
    intel_dp_supports_fec, intel_dp_sync_state, intel_dp_update_sink_caps,
};

// upstream: intel_hotplug.c intel_hpd_enable_detection_work()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hpd_enable_detection_work(_display: *mut IntelDisplay) {}

// upstream: intel_hotplug.c intel_hpd_disable_detection_work()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hpd_disable_detection_work(_display: *mut IntelDisplay) {}

// upstream: intel_panel.c intel_panel_sanitize_ssc()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_panel_sanitize_ssc(_display: *mut IntelDisplay) {}

// upstream: intel_pps.c intel_pps_setup()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pps_setup(_display: *mut IntelDisplay) {}

// upstream: intel_hotplug.c intel_hpd_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hpd_init(_display: *mut IntelDisplay) {}

// upstream: intel_hotplug.c intel_hpd_poll_disable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hpd_poll_disable(_display: *mut IntelDisplay) {}

// upstream: intel_hotplug.c intel_hpd_poll_fini()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hpd_poll_fini(_display: *mut IntelDisplay) {}

// upstream: intel_pps.c intel_pps_unlock_regs_wa()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pps_unlock_regs_wa(_display: *mut IntelDisplay) {}
