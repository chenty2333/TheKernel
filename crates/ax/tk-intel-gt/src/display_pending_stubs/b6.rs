// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! TEMPORARY definitions for callees owned by batch B6 (DP, HDMI, GMBUS, hotplug, panel, PPS).
//! Replaced by intel_dp_upstream.rs, intel_hdmi_upstream.rs,
//! intel_gmbus_upstream.rs, intel_hotplug_upstream.rs, intel_pps_upstream.rs (B6).

#![allow(non_snake_case, dead_code, unsafe_code)]

use crate::intel_display_core_types_upstream::IntelDisplay;

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
