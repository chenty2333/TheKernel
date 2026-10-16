// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// ADLP external SST DP path from drivers/gpu/drm/i915/display/intel_dp.c.
#![allow(non_snake_case, dead_code, unsafe_code, unsafe_op_in_unsafe_fn)]

use core::{ffi::c_void, mem::offset_of, ptr};

use tk_drm_upstream::{
    drm_connector_types::{
        CONNECTOR_STATUS_CONNECTED, CONNECTOR_STATUS_DISCONNECTED, CONNECTOR_STATUS_UNKNOWN,
        DrmConnector, DrmConnectorFuncs, DrmConnectorListIter, DrmConnectorState,
        DrmConnectorStatus,
    },
    drm_dp_helper_upstream as drm_dp,
    drm_modes_types::{
        DrmDisplayMode, DrmModeStatus, MODE_CLOCK_HIGH, MODE_CLOCK_LOW, MODE_H_ILLEGAL, MODE_OK,
    },
    drm_modeset_helper_vtables_types::DrmConnectorHelperFuncs,
    drm_modeset_lock_types::DrmModesetAcquireCtx,
    uapi_drm_consts::{DRM_MODE_FLAG_DBLCLK, DRM_MODE_FLAG_DBLSCAN, DRM_MODE_FLAG_INTERLACE},
};

use crate::{
    i915_external_placeholder_types_upstream::Irqreturn,
    intel_display_conversion_upstream::__drm_to_display,
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_limits_types_upstream::PORT_A,
    intel_display_power_types_upstream::{
        IntelDisplayPowerDomain, POWER_DOMAIN_AUX_A, POWER_DOMAIN_DISPLAY_CORE,
    },
    intel_display_types_upstream::{
        INTEL_OUTPUT_EDP, INTEL_OUTPUT_FORMAT_RGB, INTEL_OUTPUT_FORMAT_YCBCR420,
        INTEL_OUTPUT_FORMAT_YCBCR444, IntelAtomicState, IntelConnector, IntelCrtc, IntelCrtcState,
        IntelDigitalConnectorState, IntelDigitalPort, IntelDp, IntelDpLinkConfig, IntelEncoder,
    },
    intel_display_wa_upstream::{
        DISPLAY_VER, platform_alderlake_p, platform_alderlake_s, platform_dg1, platform_rocketlake,
    },
    intel_dp_types_upstream::LinkConfigLimits,
};

const DP_DSC_FEC_OVERHEAD_FACTOR: u32 = 1_028_530;
const DP_RATE_162: i32 = 162_000;
const DP_RATE_270: i32 = 270_000;
const DP_RATE_540: i32 = 540_000;
const DP_RATE_648: i32 = 648_000;
const DP_RATE_810: i32 = 810_000;
const DRM_CONNECTOR_POLL_HPD: u8 = 1 << 2;

static ICL_SOURCE_RATES: [i32; 10] = [
    DP_RATE_162,
    216_000,
    DP_RATE_270,
    324_000,
    432_000,
    DP_RATE_540,
    DP_RATE_648,
    DP_RATE_810,
    1_000_000,
    1_350_000,
];
static BXT_SOURCE_RATES: [i32; 7] = [
    DP_RATE_162,
    216_000,
    243_000,
    DP_RATE_270,
    324_000,
    432_000,
    DP_RATE_540,
];
static SKL_SOURCE_RATES: [i32; 6] = [
    DP_RATE_162,
    216_000,
    DP_RATE_270,
    324_000,
    432_000,
    DP_RATE_540,
];
static HSW_SOURCE_RATES: [i32; 3] = [DP_RATE_162, DP_RATE_270, DP_RATE_540];
static G4X_SOURCE_RATES: [i32; 2] = [DP_RATE_162, DP_RATE_270];

/// `intel_dp_needs_vsc_sdp()`: YCbCr 4:2:0 or BT.2020/SYCC colorimetry needs
/// the DP VSC SDP (DP 1.4a 2.2.4.3).
// upstream: intel_dp.c intel_dp_needs_vsc_sdp()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_needs_vsc_sdp(
    crtc_state: *const IntelCrtcState,
    conn_state: *const DrmConnectorState,
) -> bool {
    use tk_drm_upstream::drm_connector_types::{
        DRM_MODE_COLORIMETRY_BT2020_CYCC, DRM_MODE_COLORIMETRY_BT2020_RGB,
        DRM_MODE_COLORIMETRY_BT2020_YCC, DRM_MODE_COLORIMETRY_OPYCC_601,
        DRM_MODE_COLORIMETRY_SYCC_601,
    };
    unsafe {
        if (*crtc_state).output_format == INTEL_OUTPUT_FORMAT_YCBCR420 {
            return true;
        }
        matches!(
            (*conn_state).colorspace,
            DRM_MODE_COLORIMETRY_SYCC_601
                | DRM_MODE_COLORIMETRY_OPYCC_601
                | DRM_MODE_COLORIMETRY_BT2020_YCC
                | DRM_MODE_COLORIMETRY_BT2020_RGB
                | DRM_MODE_COLORIMETRY_BT2020_CYCC
        )
    }
}

#[inline]
unsafe fn dp_to_dig_port(dp: *mut IntelDp) -> *mut IntelDigitalPort {
    if dp.is_null() {
        return ptr::null_mut();
    }
    dp.cast::<u8>().sub(offset_of!(IntelDigitalPort, dp)).cast()
}

#[inline]
unsafe fn display_from_dp(dp: *mut IntelDp) -> *mut IntelDisplay {
    let dig_port = dp_to_dig_port(dp);
    if dig_port.is_null() {
        return ptr::null_mut();
    }
    __drm_to_display((*dig_port).base.base.dev)
}

#[inline]
unsafe fn display_from_encoder(encoder: *mut IntelEncoder) -> *mut IntelDisplay {
    if encoder.is_null() {
        return ptr::null_mut();
    }
    __drm_to_display((*encoder).base.dev)
}

#[inline]
unsafe fn display_from_connector(connector: *mut IntelConnector) -> *mut IntelDisplay {
    if connector.is_null() {
        return ptr::null_mut();
    }
    __drm_to_display((*connector).base.dev)
}

#[inline]
unsafe fn dp_from_connector(connector: *mut IntelConnector) -> *mut IntelDp {
    if connector.is_null() || (*connector).encoder.is_null() {
        return ptr::null_mut();
    }
    ptr::addr_of_mut!((*(*connector).encoder.cast::<IntelDigitalPort>()).dp)
}

#[inline]
unsafe fn dp_link_config_bits(config: *const IntelDpLinkConfig) -> u8 {
    (*config)._bitfields_0[0]
}

#[inline]
unsafe fn dp_link_config_set(config: *mut IntelDpLinkConfig, rate_idx: u8, lane_exp: u8) {
    (*config)._bitfields_0[0] = (rate_idx & 0x3f) | ((lane_exp & 0x3) << 6);
}

#[inline]
unsafe fn dp_link_config_rate_index(config: *const IntelDpLinkConfig) -> usize {
    (dp_link_config_bits(config) & 0x3f) as usize
}

#[inline]
unsafe fn dp_link_config_lane_exp(config: *const IntelDpLinkConfig) -> u8 {
    dp_link_config_bits(config) >> 6
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_is_edp(dp: *mut IntelDp) -> bool {
    !dp.is_null() && (*dp_to_dig_port(dp)).base.r#type == INTEL_OUTPUT_EDP
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_is_uhbr(crtc_state: *const IntelCrtcState) -> bool {
    !crtc_state.is_null() && drm_dp::drm_dp_is_uhbr_rate((*crtc_state).port_clock)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_link_symbol_size(rate: i32) -> i32 {
    if drm_dp::drm_dp_is_uhbr_rate(rate) {
        32
    } else {
        10
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_link_symbol_clock(rate: i32) -> i32 {
    (rate * 10 + intel_dp_link_symbol_size(rate) / 2) / intel_dp_link_symbol_size(rate)
}

unsafe fn max_dprx_rate(dp: *mut IntelDp) -> i32 {
    drm_dp::drm_dp_bw_code_to_link_rate((*dp).dpcd[drm_dp::DP_MAX_LINK_RATE as usize])
}

unsafe fn max_dprx_lane_count(dp: *mut IntelDp) -> i32 {
    drm_dp::drm_dp_max_lane_count((*dp).dpcd.as_ptr()) as i32
}

unsafe fn default_sink_rates(dp: *mut IntelDp) {
    (*dp).sink_rates[0] = DP_RATE_162;
    (*dp).num_sink_rates = 1;
}

unsafe fn set_dpcd_sink_rates(dp: *mut IntelDp) {
    // ADLP's source PHY is limited to 810 MHz (8b/10b), but preserve the
    // complete standardized sink-rate list so common-rate intersection is
    // based on the sink's real DPCD capability.
    const RATES: [i32; 4] = [DP_RATE_162, DP_RATE_270, DP_RATE_540, DP_RATE_810];
    let mut max_rate = max_dprx_rate(dp);
    let max_lttpr_rate = drm_dp::drm_dp_lttpr_max_link_rate((*dp).lttpr_common_caps.as_ptr());
    if max_lttpr_rate != 0 {
        max_rate = max_rate.min(max_lttpr_rate);
    }
    let mut count = 0usize;
    for rate in RATES {
        if rate > max_rate {
            break;
        }
        (*dp).sink_rates[count] = rate;
        count += 1;
    }
    (*dp).num_sink_rates = count as i32;
}

unsafe fn set_sink_rates(dp: *mut IntelDp) {
    set_dpcd_sink_rates(dp);
    if (*dp).num_sink_rates == 0 {
        // Invalid zero-rate DPCD: use the spec's conservative RBR fallback.
        default_sink_rates(dp);
    }
}

unsafe fn set_max_sink_lane_count(dp: *mut IntelDp) {
    let lanes = max_dprx_lane_count(dp);
    (*dp).max_sink_lane_count = match lanes {
        1 | 2 | 4 => lanes,
        _ => 1,
    };
}

unsafe fn rate_limit_len(rates: *const i32, len: usize, max_rate: i32) -> usize {
    if rates.is_null() || max_rate <= 0 {
        return len;
    }
    let mut i = len;
    while i > 0 {
        if *rates.add(i - 1) <= max_rate {
            return i;
        }
        i -= 1;
    }
    0
}

unsafe fn set_source_rates(dp: *mut IntelDp) {
    let display = display_from_dp(dp);
    let dig_port = dp_to_dig_port(dp);
    let encoder = &mut (*dig_port).base;
    let (rates, len, mut max_rate): (*const i32, usize, i32) = if DISPLAY_VER(display) >= 11 {
        let cap = if platform_alderlake_p(display)
            || platform_alderlake_s(display)
            || platform_dg1(display)
            || platform_rocketlake(display)
        {
            810_000
        } else if intel_encoder_is_combo(encoder) && !intel_dp_is_edp(dp) {
            540_000
        } else {
            810_000
        };
        (ICL_SOURCE_RATES.as_ptr(), ICL_SOURCE_RATES.len(), cap)
    } else if DISPLAY_VER(display) >= 9 && DISPLAY_VER(display) < 11 {
        (BXT_SOURCE_RATES.as_ptr(), BXT_SOURCE_RATES.len(), 0)
    } else if DISPLAY_VER(display) >= 7 {
        (HSW_SOURCE_RATES.as_ptr(), HSW_SOURCE_RATES.len(), 0)
    } else {
        (G4X_SOURCE_RATES.as_ptr(), G4X_SOURCE_RATES.len(), 0)
    };
    let vbt_max_rate =
        crate::intel_bios_upstream::intel_bios_dp_max_link_rate(encoder.devdata.cast());
    if max_rate > 0 && vbt_max_rate > 0 {
        max_rate = max_rate.min(vbt_max_rate);
    } else if vbt_max_rate > 0 {
        max_rate = vbt_max_rate;
    }
    let len = rate_limit_len(rates, len, max_rate);
    (*dp).source_rates = rates.cast_mut();
    (*dp).num_source_rates = len as i32;
}

unsafe fn max_source_lane_count(dig_port: *mut IntelDigitalPort) -> i32 {
    let vbt_max =
        crate::intel_bios_upstream::intel_bios_dp_max_lane_count((*dig_port).base.devdata.cast());
    let max_lanes = (*dig_port).max_lanes as i32;
    if vbt_max > 0 {
        max_lanes.min(vbt_max)
    } else {
        max_lanes
    }
}

unsafe fn set_max_common_lane_count(dp: *mut IntelDp) -> bool {
    let dig_port = dp_to_dig_port(dp);
    let old = (*dp).max_common_lane_count;
    let mut sink_max = (*dp).max_sink_lane_count;
    let lttpr_max = drm_dp::drm_dp_lttpr_max_lane_count((*dp).lttpr_common_caps.as_ptr());
    if lttpr_max != 0 {
        sink_max = sink_max.min(lttpr_max);
    }
    let tc_max = intel_tc_port_max_lane_count(dig_port);
    (*dp).max_common_lane_count = max_source_lane_count(dig_port).min(sink_max).min(tc_max);
    (*dp).max_common_lane_count != old
}

unsafe fn intersect_rates(
    source: *const i32,
    source_len: usize,
    sink: *const i32,
    sink_len: usize,
    common: *mut i32,
) -> usize {
    let (mut i, mut j, mut k) = (0usize, 0usize, 0usize);
    while i < source_len && j < sink_len && k < 8 {
        let src = *source.add(i);
        let snk = *sink.add(j);
        if src == snk {
            *common.add(k) = src;
            i += 1;
            j += 1;
            k += 1;
        } else if src < snk {
            i += 1;
        } else {
            j += 1;
        }
    }
    k
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_common_rate(dp: *mut IntelDp, index: i32) -> i32 {
    if dp.is_null() || index < 0 || index >= (*dp).num_common_rates {
        return DP_RATE_162;
    }
    (*dp).common_rates[index as usize]
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_max_common_rate(dp: *mut IntelDp) -> i32 {
    if dp.is_null() {
        return 0;
    }
    intel_dp_common_rate(dp, (*dp).num_common_rates - 1)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_max_source_lane_count(dig_port: *mut IntelDigitalPort) -> i32 {
    if dig_port.is_null() {
        0
    } else {
        max_source_lane_count(dig_port)
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_max_common_lane_count(dp: *mut IntelDp) -> i32 {
    if dp.is_null() {
        0
    } else {
        (*dp).max_common_lane_count
    }
}

unsafe fn link_config_rate(dp: *mut IntelDp, config: *const IntelDpLinkConfig) -> i32 {
    intel_dp_common_rate(dp, dp_link_config_rate_index(config) as i32)
}

unsafe fn link_config_lane_count(config: *const IntelDpLinkConfig) -> i32 {
    1 << dp_link_config_lane_exp(config)
}

unsafe fn link_config_bw(dp: *mut IntelDp, config: *const IntelDpLinkConfig) -> i32 {
    drm_dp::drm_dp_max_dprx_data_rate(link_config_rate(dp, config), link_config_lane_count(config))
}

unsafe fn link_config_init(dp: *mut IntelDp) {
    let max_lanes = (*dp).max_common_lane_count;
    if !matches!(max_lanes, 1 | 2 | 4) {
        (*dp).link.num_configs = 0;
        return;
    }
    let lane_levels = match max_lanes {
        1 => 1,
        2 => 2,
        4 => 3,
        _ => 0,
    };
    let rates = (*dp).num_common_rates.max(0) as usize;
    let count = rates
        .saturating_mul(lane_levels)
        .min((*dp).link.configs.len());
    (*dp).link.num_configs = count as i32;
    let mut idx = 0usize;
    for rate in 0..rates {
        for lane_exp in 0..lane_levels {
            dp_link_config_set(&mut (*dp).link.configs[idx], rate as u8, lane_exp as u8);
            idx += 1;
        }
    }
    // Linux's sort_r() comparator orders by usable bandwidth, then link rate.
    for i in 1..count {
        let key = dp_link_config_bits(&(*dp).link.configs[i]);
        let mut j = i;
        while j > 0 {
            let previous = &(*dp).link.configs[j - 1];
            let previous_bw = link_config_bw(dp, previous);
            let key_rate = (*dp).common_rates[(key & 0x3f) as usize];
            let key_bw = drm_dp::drm_dp_max_dprx_data_rate(key_rate, 1 << (key >> 6));
            if previous_bw < key_bw
                || (previous_bw == key_bw && link_config_rate(dp, previous) <= key_rate)
            {
                break;
            }
            let previous_bits = dp_link_config_bits(previous);
            (*dp).link.configs[j]._bitfields_0[0] = previous_bits;
            j -= 1;
        }
        (*dp).link.configs[j]._bitfields_0[0] = key;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_link_config_get(
    dp: *mut IntelDp,
    index: i32,
    link_rate: *mut i32,
    lane_count: *mut i32,
) {
    if dp.is_null() || link_rate.is_null() || lane_count.is_null() {
        return;
    }
    let idx = if index < 0 || index >= (*dp).link.num_configs {
        0
    } else {
        index as usize
    };
    *link_rate = link_config_rate(dp, &(*dp).link.configs[idx]);
    *lane_count = link_config_lane_count(&(*dp).link.configs[idx]);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_rate_index(rates: *const i32, len: i32, rate: i32) -> i32 {
    if rates.is_null() || len <= 0 {
        return -1;
    }
    for idx in 0..len as usize {
        if *rates.add(idx) == rate {
            return idx as i32;
        }
    }
    -1
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_link_config_index(
    dp: *mut IntelDp,
    link_rate: i32,
    lane_count: i32,
) -> i32 {
    if dp.is_null() || !matches!(lane_count, 1 | 2 | 4) {
        return -1;
    }
    let rate_idx = intel_dp_rate_index(
        (*dp).common_rates.as_ptr(),
        (*dp).num_common_rates,
        link_rate,
    );
    if rate_idx < 0 {
        return -1;
    }
    let exp = lane_count.trailing_zeros() as u8;
    for idx in 0..(*dp).link.num_configs.max(0) as usize {
        let config = &(*dp).link.configs[idx];
        if dp_link_config_rate_index(config) == rate_idx as usize
            && dp_link_config_lane_exp(config) == exp
        {
            return idx as i32;
        }
    }
    -1
}

unsafe fn set_common_rates(dp: *mut IntelDp) -> bool {
    let old_count = (*dp).num_common_rates.max(0) as usize;
    let mut old = [0i32; 8];
    let common = core::slice::from_raw_parts(ptr::addr_of!((*dp).common_rates).cast::<i32>(), 8);
    old[..old_count.min(8)].copy_from_slice(&common[..old_count.min(8)]);
    let count = intersect_rates(
        (*dp).source_rates,
        (*dp).num_source_rates.max(0) as usize,
        (*dp).sink_rates.as_ptr(),
        (*dp).num_sink_rates.max(0) as usize,
        (*dp).common_rates.as_mut_ptr(),
    );
    if count == 0 {
        (*dp).common_rates[0] = DP_RATE_162;
        (*dp).num_common_rates = 1;
    } else {
        (*dp).num_common_rates = count as i32;
    }
    let new_count = (*dp).num_common_rates as usize;
    let common = core::slice::from_raw_parts(ptr::addr_of!((*dp).common_rates).cast::<i32>(), 8);
    old_count != new_count || old[..old_count.min(8)] != common[..old_count.min(8)]
}

unsafe fn set_common_link_params(dp: *mut IntelDp) -> bool {
    let mut changed = set_common_rates(dp);
    changed |= set_max_common_lane_count(dp);
    link_config_init(dp);
    changed
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_max_lane_count(dp: *mut IntelDp) -> i32 {
    if dp.is_null() {
        return 1;
    }
    let forced = (*dp).link.force_lane_count;
    let value = if forced != 0 {
        forced.clamp(1, (*dp).max_common_lane_count.max(1))
    } else {
        (*dp).link.max_lane_count
    };
    if matches!(value, 1 | 2 | 4) { value } else { 1 }
}

unsafe fn min_lane_count(dp: *mut IntelDp) -> i32 {
    if (*dp).link.force_lane_count != 0 {
        (*dp)
            .link
            .force_lane_count
            .clamp(1, (*dp).max_common_lane_count.max(1))
    } else {
        1
    }
}

unsafe fn common_len_rate_limit(dp: *mut IntelDp, max_rate: i32) -> usize {
    rate_limit_len(
        (*dp).common_rates.as_ptr(),
        (*dp).num_common_rates.max(0) as usize,
        max_rate,
    )
}

unsafe fn min_link_rate(dp: *mut IntelDp) -> i32 {
    if (*dp).link.force_rate != 0 {
        let len = common_len_rate_limit(dp, (*dp).link.force_rate);
        return intel_dp_common_rate(dp, len.saturating_sub(1) as i32);
    }
    intel_dp_common_rate(dp, 0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_max_link_rate(dp: *mut IntelDp) -> i32 {
    if dp.is_null() {
        return 0;
    }
    let len = if (*dp).link.force_rate != 0 {
        common_len_rate_limit(dp, (*dp).link.force_rate)
    } else {
        common_len_rate_limit(dp, (*dp).link.max_rate)
    };
    intel_dp_common_rate(dp, len.saturating_sub(1) as i32)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_rate_select(dp: *mut IntelDp, rate: i32) -> i32 {
    if dp.is_null() {
        return 0;
    }
    let index = intel_dp_rate_index((*dp).sink_rates.as_ptr(), (*dp).num_sink_rates, rate);
    index.max(0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_compute_rate(
    dp: *mut IntelDp,
    port_clock: i32,
    link_bw: *mut u8,
    rate_select: *mut u8,
) {
    if dp.is_null() || link_bw.is_null() || rate_select.is_null() {
        return;
    }
    if (*dp).use_rate_select {
        *link_bw = 0;
        *rate_select = intel_dp_rate_select(dp, port_clock).clamp(0, u8::MAX as i32) as u8;
    } else {
        *link_bw = drm_dp::drm_dp_link_rate_to_bw_code(port_clock);
        *rate_select = 0;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_link_params_valid(
    dp: *mut IntelDp,
    link_rate: i32,
    lane_count: u8,
) -> bool {
    if dp.is_null() || link_rate == 0 || link_rate > (*dp).link.max_rate {
        return false;
    }
    lane_count != 0 && lane_count as i32 <= intel_dp_max_lane_count(dp)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_mode_to_fec_clock(mode_clock: u32) -> u32 {
    ((mode_clock as u64 * DP_DSC_FEC_OVERHEAD_FACTOR as u64) / 1_000_000) as u32
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_bw_fec_overhead(fec_enabled: bool) -> i32 {
    if fec_enabled {
        DP_DSC_FEC_OVERHEAD_FACTOR as i32
    } else {
        1_000_000
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_link_bw_overhead(
    link_clock: i32,
    lane_count: i32,
    hdisplay: i32,
    dsc_slice_count: i32,
    bpp_x16: i32,
    mut flags: core::ffi::c_ulong,
) -> i32 {
    if drm_dp::drm_dp_is_uhbr_rate(link_clock) {
        flags |= drm_dp::DRM_DP_BW_OVERHEAD_UHBR;
    }
    if dsc_slice_count != 0 {
        flags |= drm_dp::DRM_DP_BW_OVERHEAD_DSC;
    }
    let overhead =
        drm_dp::drm_dp_bw_overhead(lane_count, hdisplay, dsc_slice_count, bpp_x16, flags);
    overhead.max(intel_dp_bw_fec_overhead(
        flags & drm_dp::DRM_DP_BW_OVERHEAD_FEC != 0,
    ))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_effective_data_rate(
    pixel_clock: i32,
    bpp_x16: i32,
    bw_overhead: i32,
) -> i32 {
    let den = 1_000_000i64 * 16 * 8;
    let num = pixel_clock as i64 * bpp_x16 as i64 * bw_overhead as i64;
    ((num + den - 1) / den) as i32
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_link_required(
    link_clock: i32,
    lane_count: i32,
    mode_clock: i32,
    mode_hdisplay: i32,
    link_bpp_x16: i32,
    bw_overhead_flags: core::ffi::c_ulong,
) -> i32 {
    let overhead = intel_dp_link_bw_overhead(
        link_clock,
        lane_count,
        mode_hdisplay,
        0,
        link_bpp_x16,
        bw_overhead_flags,
    );
    intel_dp_effective_data_rate(mode_clock, link_bpp_x16, overhead)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_max_link_data_rate(
    dp: *mut IntelDp,
    max_rate: i32,
    max_lanes: i32,
) -> i32 {
    if dp.is_null() {
        return 0;
    }
    drm_dp::drm_dp_max_dprx_data_rate(max_rate, max_lanes)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_min_bpp(output_format: u32) -> i32 {
    if output_format == INTEL_OUTPUT_FORMAT_RGB {
        18
    } else {
        24
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_output_format_link_bpp_x16(
    output_format: u32,
    mut pipe_bpp: i32,
) -> i32 {
    if output_format == INTEL_OUTPUT_FORMAT_YCBCR420 {
        pipe_bpp /= 2;
    }
    pipe_bpp << 4
}

unsafe fn source_can_output(dp: *mut IntelDp, format: u32) -> bool {
    let display = display_from_dp(dp);
    match format {
        INTEL_OUTPUT_FORMAT_RGB => true,
        INTEL_OUTPUT_FORMAT_YCBCR444 => DISPLAY_VER(display) >= 5,
        INTEL_OUTPUT_FORMAT_YCBCR420 => DISPLAY_VER(display) >= 11,
        _ => false,
    }
}

unsafe fn dfp_can_convert_from_rgb(dp: *mut IntelDp, sink_format: u32) -> bool {
    if !drm_dp::drm_dp_is_branch((*dp).dpcd.as_ptr()) {
        return false;
    }
    match sink_format {
        INTEL_OUTPUT_FORMAT_YCBCR444 => (*dp).dfp.rgb_to_ycbcr,
        INTEL_OUTPUT_FORMAT_YCBCR420 => (*dp).dfp.rgb_to_ycbcr && (*dp).dfp.ycbcr_444_to_420,
        _ => false,
    }
}

unsafe fn dfp_can_convert_from_ycbcr444(dp: *mut IntelDp, sink_format: u32) -> bool {
    drm_dp::drm_dp_is_branch((*dp).dpcd.as_ptr())
        && sink_format == INTEL_OUTPUT_FORMAT_YCBCR420
        && (*dp).dfp.ycbcr_444_to_420
}

unsafe fn dfp_can_convert(dp: *mut IntelDp, output_format: u32, sink_format: u32) -> bool {
    match output_format {
        INTEL_OUTPUT_FORMAT_RGB => dfp_can_convert_from_rgb(dp, sink_format),
        INTEL_OUTPUT_FORMAT_YCBCR444 => dfp_can_convert_from_ycbcr444(dp, sink_format),
        _ => false,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_output_format(
    connector: *mut IntelConnector,
    sink_format: u32,
) -> u32 {
    if connector.is_null() {
        return INTEL_OUTPUT_FORMAT_RGB;
    }
    let dp = dp_from_connector(connector);
    if sink_format == INTEL_OUTPUT_FORMAT_RGB || dfp_can_convert_from_rgb(dp, sink_format) {
        INTEL_OUTPUT_FORMAT_RGB
    } else if sink_format == INTEL_OUTPUT_FORMAT_YCBCR444
        || dfp_can_convert_from_ycbcr444(dp, sink_format)
    {
        INTEL_OUTPUT_FORMAT_YCBCR444
    } else {
        INTEL_OUTPUT_FORMAT_YCBCR420
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_limited_color_range(
    crtc_state: *const IntelCrtcState,
    conn_state: *const DrmConnectorState,
) -> bool {
    if crtc_state.is_null() || conn_state.is_null() {
        return false;
    }
    if (*crtc_state).output_format != INTEL_OUTPUT_FORMAT_RGB {
        return false;
    }
    let state = conn_state.cast::<IntelDigitalConnectorState>();
    if (*state).broadcast_rgb == 0 {
        (*crtc_state).pipe_bpp != 18
            && tk_drm_upstream::drm_edid_upstream::drm_default_rgb_quant_range(ptr::addr_of!(
                (*crtc_state).hw.adjusted_mode
            )) == tk_drm_upstream::hdmi_types::HDMI_QUANTIZATION_RANGE_LIMITED
    } else {
        (*state).broadcast_rgb == 2
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_source_supports_tps3(display: *mut IntelDisplay) -> bool {
    DISPLAY_VER(display) >= 9
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_source_supports_tps4(display: *mut IntelDisplay) -> bool {
    DISPLAY_VER(display) >= 10
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_supports_fec(
    dp: *mut IntelDp,
    connector: *const IntelConnector,
    crtc_state: *const IntelCrtcState,
) -> bool {
    if dp.is_null() || connector.is_null() || crtc_state.is_null() {
        return false;
    }
    DISPLAY_VER(display_from_dp(dp)) >= 12
        && drm_dp::drm_dp_sink_supports_fec((*connector).dp.fec_capability)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_max_hdisplay_per_pipe(display: *mut IntelDisplay) -> i32 {
    if DISPLAY_VER(display) >= 30 {
        6144
    } else {
        5120
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_dotclk_valid(
    display: *mut IntelDisplay,
    target_clock: i32,
    _htotal: i32,
    dsc_slice_count: i32,
    num_joined_pipes: i32,
) -> bool {
    if display.is_null() || num_joined_pipes <= 0 {
        return false;
    }
    if dsc_slice_count != 0 {
        // DSC is deliberately not enabled by the ADLP SST implementation.
        return false;
    }
    let max_dotclock = intel_max_uncompressed_dotclock(display);
    target_clock <= max_dotclock.saturating_mul(num_joined_pipes)
}

unsafe fn sink_format_valid(
    connector: *mut IntelConnector,
    mode: *const DrmDisplayMode,
    sink_format: u32,
) -> DrmModeStatus {
    let dp = dp_from_connector(connector);
    let info = ptr::addr_of!((*connector).base.display_info);
    if sink_format == INTEL_OUTPUT_FORMAT_YCBCR420 {
        if !(*connector).base.ycbcr_420_allowed
            || !tk_drm_upstream::drm_modes_upstream::drm_mode_is_420(info, mode)
        {
            return tk_drm_upstream::drm_modes_types::MODE_NO_420;
        }
        if (*dp).dfp.min_tmds_clock != 0 && !(*info).is_hdmi {
            return tk_drm_upstream::drm_modes_types::MODE_NO_420;
        }
        return MODE_OK;
    }
    if sink_format == INTEL_OUTPUT_FORMAT_RGB {
        MODE_OK
    } else {
        tk_drm_upstream::drm_modes_types::MODE_BAD
    }
}

unsafe fn tmds_clock_valid(
    dp: *mut IntelDp,
    clock: i32,
    bpc: i32,
    sink_format: u32,
    respect_downstream_limits: bool,
) -> DrmModeStatus {
    if !respect_downstream_limits {
        return MODE_OK;
    }
    let connector = (*dp).attached_connector;
    let mut max_tmds = (*dp).dfp.max_tmds_clock;
    if max_tmds != 0 && (*connector).base.display_info.max_tmds_clock != 0 {
        max_tmds = max_tmds.min((*connector).base.display_info.max_tmds_clock);
    }
    let tmds_clock = intel_hdmi_tmds_clock(clock, bpc, sink_format);
    if (*dp).dfp.min_tmds_clock != 0 && tmds_clock < (*dp).dfp.min_tmds_clock {
        return MODE_CLOCK_LOW;
    }
    if max_tmds != 0 && tmds_clock > max_tmds {
        return MODE_CLOCK_HIGH;
    }
    MODE_OK
}

unsafe fn mode_valid_downstream(
    connector: *mut IntelConnector,
    _mode: *const DrmDisplayMode,
    target_clock: i32,
    sink_format: u32,
) -> DrmModeStatus {
    let dp = dp_from_connector(connector);
    if (*dp).dfp.max_dotclock != 0 && target_clock > (*dp).dfp.max_dotclock {
        return MODE_CLOCK_HIGH;
    }
    tmds_clock_valid(dp, target_clock, 8, sink_format, true)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_compute_config_limits(
    dp: *mut IntelDp,
    conn_state: *mut DrmConnectorState,
    crtc_state: *mut IntelCrtcState,
    respect_downstream_limits: bool,
    dsc: bool,
    limits: *mut LinkConfigLimits,
) -> bool {
    if dp.is_null() || conn_state.is_null() || crtc_state.is_null() || limits.is_null() || dsc {
        return false;
    }
    let connector = (*conn_state).connector.cast::<IntelConnector>();
    if connector.is_null() {
        return false;
    }
    let max_rate = intel_dp_max_link_rate(dp);
    (*limits).min_rate = min_link_rate(dp).min(max_rate);
    (*limits).max_rate = max_rate;
    (*limits).min_lane_count = min_lane_count(dp);
    (*limits).max_lane_count = intel_dp_max_lane_count(dp);
    (*limits).pipe.min_bpp = intel_dp_min_bpp((*crtc_state).output_format);
    let mut pipe_max = (*crtc_state).max_pipe_bpp;
    if (*dp).dfp.max_bpc != 0 {
        pipe_max = pipe_max.min((*dp).dfp.max_bpc as i32 * 3);
    }
    if respect_downstream_limits && (*dp).dfp.min_tmds_clock != 0 {
        // The DP-to-HDMI converter's TMDS limits are reflected by max_bpc;
        // an infeasible timing is still checked after selecting the mode.
        pipe_max = pipe_max.min(36);
    }
    if (*limits).pipe.min_bpp <= 0 || (*limits).pipe.min_bpp > pipe_max {
        return false;
    }
    (*limits).pipe.max_bpp = (*crtc_state)
        .pipe_bpp
        .clamp((*limits).pipe.min_bpp, pipe_max);
    let max_link_bpp = (*crtc_state)
        .max_link_bpp_x16
        .min((*limits).pipe.max_bpp << 4);
    let max_link_bpp = max_link_bpp / (6 << 4) * (6 << 4);
    (*limits).link.min_bpp_x16 = (*limits).pipe.min_bpp << 4;
    (*limits).link.max_bpp_x16 = max_link_bpp;

    if (*dp).use_max_params {
        (*limits).min_lane_count = (*limits).max_lane_count;
        (*limits).min_rate = (*limits).max_rate;
    }
    (*limits).link.min_bpp_x16 > 0
        && (*limits).link.min_bpp_x16 <= (*limits).link.max_bpp_x16
        && (*limits).min_lane_count > 0
        && (*limits).min_rate > 0
}

unsafe fn compute_link_config_wide(
    dp: *mut IntelDp,
    crtc_state: *mut IntelCrtcState,
    limits: *const LinkConfigLimits,
) -> i32 {
    let mode = ptr::addr_of!((*crtc_state).hw.adjusted_mode);
    let clock = (*mode).crtc_clock;
    let mut bpp = ((*limits).link.max_bpp_x16 >> 4).max(0);
    let min_bpp = ((*limits).link.min_bpp_x16 >> 4).max(0);
    while bpp >= min_bpp {
        let link_bpp_x16 = intel_dp_output_format_link_bpp_x16((*crtc_state).output_format, bpp);
        for rate_idx in 0..(*dp).num_common_rates.max(0) {
            let rate = intel_dp_common_rate(dp, rate_idx);
            if rate < (*limits).min_rate || rate > (*limits).max_rate {
                continue;
            }
            let mut lanes = (*limits).min_lane_count;
            while lanes <= (*limits).max_lane_count {
                let required = intel_dp_link_required(
                    rate,
                    lanes,
                    clock,
                    (*mode).hdisplay as i32,
                    link_bpp_x16,
                    0,
                );
                let available = intel_dp_max_link_data_rate(dp, rate, lanes);
                if required <= available {
                    (*crtc_state).lane_count = lanes.clamp(1, u8::MAX as i32) as u8;
                    (*crtc_state).pipe_bpp = bpp;
                    (*crtc_state).port_clock = rate;
                    return 0;
                }
                lanes = lanes.saturating_mul(2);
            }
        }
        bpp -= 6;
    }
    -tk_linux_kapi::config::EINVAL
}

unsafe fn compute_link_config(
    encoder: *mut IntelEncoder,
    crtc_state: *mut IntelCrtcState,
    conn_state: *mut DrmConnectorState,
    respect_downstream_limits: bool,
) -> i32 {
    let dp = encoder.cast::<IntelDigitalPort>();
    let dp = ptr::addr_of_mut!((*dp).dp);
    let connector = (*conn_state).connector.cast::<IntelConnector>();
    if (*crtc_state).fec_enable && !intel_dp_supports_fec(dp, connector, crtc_state) {
        return -tk_linux_kapi::config::EINVAL;
    }
    (*crtc_state).joiner_pipes = 0;
    let mut limits = LinkConfigLimits {
        min_rate: 0,
        max_rate: 0,
        min_lane_count: 0,
        max_lane_count: 0,
        pipe: crate::intel_dp_types_upstream::LinkConfigLimitsPipe {
            min_bpp: 0,
            max_bpp: 0,
        },
        link: crate::intel_dp_types_upstream::LinkConfigLimitsLink {
            min_bpp_x16: 0,
            max_bpp_x16: 0,
        },
    };
    if !intel_dp_compute_config_limits(
        dp,
        conn_state,
        crtc_state,
        respect_downstream_limits,
        false,
        &mut limits,
    ) {
        return -tk_linux_kapi::config::EINVAL;
    }
    compute_link_config_wide(dp, crtc_state, &limits)
}

unsafe fn compute_output_format(
    encoder: *mut IntelEncoder,
    crtc_state: *mut IntelCrtcState,
    conn_state: *mut DrmConnectorState,
    respect_downstream_limits: bool,
    sink_format: u32,
) -> i32 {
    let connector = (*conn_state).connector.cast::<IntelConnector>();
    let mode = ptr::addr_of!((*crtc_state).hw.adjusted_mode);
    if sink_format_valid(connector, mode, sink_format) != MODE_OK {
        return -tk_linux_kapi::config::EINVAL;
    }
    (*crtc_state).sink_format = sink_format;
    (*crtc_state).output_format = intel_dp_output_format(connector, sink_format);
    compute_link_config(encoder, crtc_state, conn_state, respect_downstream_limits)
}

unsafe fn compute_formats(
    encoder: *mut IntelEncoder,
    crtc_state: *mut IntelCrtcState,
    conn_state: *mut DrmConnectorState,
    respect_downstream_limits: bool,
) -> i32 {
    let connector = (*conn_state).connector.cast::<IntelConnector>();
    let info = ptr::addr_of!((*connector).base.display_info);
    let mode = ptr::addr_of!((*crtc_state).hw.adjusted_mode);
    if tk_drm_upstream::drm_modes_upstream::drm_mode_is_420_only(info, mode) {
        let ret = compute_output_format(
            encoder,
            crtc_state,
            conn_state,
            respect_downstream_limits,
            INTEL_OUTPUT_FORMAT_YCBCR420,
        );
        if ret != 0 && !respect_downstream_limits {
            return compute_output_format(
                encoder,
                crtc_state,
                conn_state,
                false,
                INTEL_OUTPUT_FORMAT_RGB,
            );
        }
        ret
    } else {
        let ret = compute_output_format(
            encoder,
            crtc_state,
            conn_state,
            respect_downstream_limits,
            INTEL_OUTPUT_FORMAT_RGB,
        );
        if ret != 0 && tk_drm_upstream::drm_modes_upstream::drm_mode_is_420_also(info, mode) {
            return compute_output_format(
                encoder,
                crtc_state,
                conn_state,
                respect_downstream_limits,
                INTEL_OUTPUT_FORMAT_YCBCR420,
            );
        }
        ret
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_compute_config(
    encoder: *mut IntelEncoder,
    crtc_state: *mut IntelCrtcState,
    conn_state: *mut DrmConnectorState,
) -> i32 {
    if encoder.is_null() || crtc_state.is_null() || conn_state.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let display = display_from_encoder(encoder);
    let connector = (*conn_state).connector.cast::<IntelConnector>();
    if display.is_null() || connector.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    // DSC is not part of this external SST implementation. Never accept a
    // DSC state while producing an uncompressed link configuration.
    if (*crtc_state).dsc.compression_enable {
        return -tk_linux_kapi::config::EOPNOTSUPP;
    }
    let mode = ptr::addr_of_mut!((*crtc_state).hw.adjusted_mode);
    if (*mode).flags & DRM_MODE_FLAG_DBLSCAN as u32 != 0
        || (!(*connector).base.interlace_allowed
            && (*mode).flags & DRM_MODE_FLAG_INTERLACE as u32 != 0)
        || (*mode).flags & DRM_MODE_FLAG_DBLCLK as u32 != 0
    {
        return -tk_linux_kapi::config::EINVAL;
    }

    let mut ret = compute_formats(encoder, crtc_state, conn_state, true);
    if ret != 0 {
        ret = compute_formats(encoder, crtc_state, conn_state, false);
    }
    if ret != 0 {
        return ret;
    }
    ret = intel_pfit_compute_config(crtc_state, conn_state);
    if ret != 0 {
        return ret;
    }
    (*crtc_state).limited_color_range = intel_dp_limited_color_range(crtc_state, conn_state);
    (*crtc_state).enhanced_framing =
        drm_dp::drm_dp_enhanced_frame_cap((*encoder.cast::<IntelDigitalPort>()).dp.dpcd.as_ptr());

    let link_bpp_x16 =
        intel_dp_output_format_link_bpp_x16((*crtc_state).output_format, (*crtc_state).pipe_bpp);
    intel_link_compute_m_n(
        link_bpp_x16.clamp(0, u16::MAX as i32) as u16,
        (*crtc_state).lane_count as i32,
        (*mode).crtc_clock,
        (*crtc_state).port_clock,
        intel_dp_bw_fec_overhead((*crtc_state).fec_enable),
        ptr::addr_of_mut!((*crtc_state).dp_m_n),
    );
    intel_dp_compute_min_hblank(crtc_state, conn_state)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_compute_min_hblank(
    crtc_state: *mut IntelCrtcState,
    conn_state: *const DrmConnectorState,
) -> i32 {
    if crtc_state.is_null() || conn_state.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let connector = (*conn_state).connector.cast::<IntelConnector>();
    let display = display_from_connector(connector);
    if display.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    if DISPLAY_VER(display) < 30 || !intel_dp_is_uhbr(crtc_state) {
        (*crtc_state).min_hblank = 0;
        return 0;
    }
    if (*crtc_state).dsc.compression_enable {
        return -tk_linux_kapi::config::EOPNOTSUPP;
    }
    let mode = ptr::addr_of!((*crtc_state).hw.adjusted_mode);
    let hdisplay = (*mode).hdisplay as i32;
    if hdisplay <= 0 {
        return -tk_linux_kapi::config::EINVAL;
    }
    let link_bpp_x16 =
        intel_dp_output_format_link_bpp_x16((*crtc_state).output_format, (*crtc_state).pipe_bpp);
    let hactive_sym_cycles =
        drm_dp::drm_dp_link_symbol_cycles(4, hdisplay, 0, link_bpp_x16, 32, false);
    let htotal_sym_cycles = (*mode).htotal as i32 * hactive_sym_cycles / hdisplay;
    let min_hblank = (htotal_sym_cycles - hactive_sym_cycles).max(3) - 2;
    (*crtc_state).min_hblank = min_hblank.min(3);
    0
}

unsafe fn hdisplay_bad(display: *mut IntelDisplay, hdisplay: i32) -> bool {
    hdisplay == 4096 && DISPLAY_VER(display) < 8
}

unsafe fn mode_valid_format(
    connector: *mut IntelConnector,
    mode: *const DrmDisplayMode,
    target_clock: i32,
    sink_format: u32,
) -> DrmModeStatus {
    let display = display_from_connector(connector);
    let dp = dp_from_connector(connector);
    let status = sink_format_valid(connector, mode, sink_format);
    if status != MODE_OK {
        return status;
    }
    let output_format = intel_dp_output_format(connector, sink_format);
    if !source_can_output(dp, output_format) {
        return tk_drm_upstream::drm_modes_types::MODE_BAD;
    }
    let max_rate = intel_dp_max_link_rate(dp);
    let max_lanes = intel_dp_max_lane_count(dp);
    let max_data = intel_dp_max_link_data_rate(dp, max_rate, max_lanes);
    let min_pipe_bpp = intel_dp_min_bpp(output_format);
    let link_bpp_x16 = intel_dp_output_format_link_bpp_x16(output_format, min_pipe_bpp);
    let mode_rate = intel_dp_link_required(
        max_rate,
        max_lanes,
        target_clock,
        (*mode).hdisplay as i32,
        link_bpp_x16,
        0,
    );
    if mode_rate > max_data {
        return MODE_CLOCK_HIGH;
    }
    let status = intel_pfit_mode_valid(display, mode, output_format, 1);
    if status != MODE_OK {
        return status;
    }
    let status = intel_mode_valid_max_plane_size(display, mode, 1);
    if status != MODE_OK {
        return status;
    }
    if !intel_dp_dotclk_valid(display, target_clock, (*mode).htotal as i32, 0, 1) {
        return MODE_CLOCK_HIGH;
    }
    mode_valid_downstream(connector, mode, target_clock, sink_format)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_mode_valid(
    drm_connector: *mut DrmConnector,
    mode: *mut DrmDisplayMode,
) -> DrmModeStatus {
    if drm_connector.is_null() || mode.is_null() {
        return MODE_H_ILLEGAL;
    }
    let connector = drm_connector.cast::<IntelConnector>();
    let display = display_from_connector(connector);
    let status = intel_cpu_transcoder_mode_valid(display, mode);
    if status != MODE_OK {
        return status;
    }
    if (*mode).flags & DRM_MODE_FLAG_DBLCLK as u32 != 0 {
        return MODE_H_ILLEGAL;
    }
    if (*mode).clock < 10_000 {
        return MODE_CLOCK_LOW;
    }
    if hdisplay_bad(display, (*mode).hdisplay as i32) {
        return MODE_H_ILLEGAL;
    }
    let info = ptr::addr_of!((*connector).base.display_info);
    if tk_drm_upstream::drm_modes_upstream::drm_mode_is_420_only(info, mode) {
        mode_valid_format(connector, mode, (*mode).clock, INTEL_OUTPUT_FORMAT_YCBCR420)
    } else {
        let status = mode_valid_format(connector, mode, (*mode).clock, INTEL_OUTPUT_FORMAT_RGB);
        if status != MODE_OK
            && tk_drm_upstream::drm_modes_upstream::drm_mode_is_420_also(info, mode)
        {
            mode_valid_format(connector, mode, (*mode).clock, INTEL_OUTPUT_FORMAT_YCBCR420)
        } else {
            status
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_set_link_params(
    dp: *mut IntelDp,
    link_rate: i32,
    lane_count: i32,
) {
    if dp.is_null() {
        return;
    }
    (*dp).train_set = [0; 4];
    (*dp).link.active = false;
    (*dp).needs_modeset_retry = false;
    (*dp).link_rate = link_rate;
    (*dp).lane_count = lane_count.clamp(0, u8::MAX as i32) as u8;
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_reset_link_params(dp: *mut IntelDp) {
    if dp.is_null() {
        return;
    }
    (*dp).link.max_lane_count = intel_dp_max_common_lane_count(dp);
    (*dp).link.max_rate = intel_dp_max_common_rate(dp);
    (*dp).link.mst_probed_lane_count = 0;
    (*dp).link.mst_probed_rate = 0;
    (*dp).link.retrain_disabled = false;
    (*dp).link.seq_train_failures = 0;
}

unsafe fn has_sink_count(dp: *mut IntelDp) -> bool {
    !dp.is_null()
        && !(*dp).attached_connector.is_null()
        && drm_dp::drm_dp_read_sink_count_cap(
            &mut (*(*dp).attached_connector).base,
            (*dp).dpcd.as_ptr(),
            ptr::addr_of!((*dp).desc),
        )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_update_sink_caps(dp: *mut IntelDp) {
    if dp.is_null() {
        return;
    }
    set_sink_rates(dp);
    set_max_sink_lane_count(dp);
    if set_common_link_params(dp) && !(*dp).reset_link_params {
        (*dp).reset_link_params = true;
    }
}

unsafe fn intel_dp_get_dpcd(dp: *mut IntelDp) -> bool {
    if dp.is_null() || intel_dp_init_lttpr_and_dprx_caps(dp) < 0 {
        return false;
    }
    let aux = ptr::addr_of_mut!((*dp).aux);
    let branch = drm_dp::drm_dp_is_branch((*dp).dpcd.as_ptr());
    let _ = drm_dp::drm_dp_read_desc(aux, ptr::addr_of_mut!((*dp).desc), branch);
    crate::intel_quirks_upstream::intel_init_dpcd_quirks(
        dp,
        ptr::addr_of!((*dp).desc.ident).cast(),
    );
    intel_dp_update_sink_caps(dp);

    if has_sink_count(dp) {
        let count = drm_dp::drm_dp_read_sink_count(aux);
        if count < 0 {
            return false;
        }
        (*dp).sink_count = count as u8;
        if count == 0 {
            return false;
        }
    }

    drm_dp::drm_dp_read_downstream_info(
        aux,
        (*dp).dpcd.as_ptr(),
        (*dp).downstream_ports.as_mut_ptr(),
    ) == 0
}

unsafe fn intel_dp_get_edid(dp: *mut IntelDp) -> *const c_void {
    let connector = (*dp).attached_connector;
    if connector.is_null() {
        return ptr::null();
    }
    let edid = tk_drm_upstream::drm_edid_upstream::drm_edid_read_ddc(
        &mut (*connector).base,
        ptr::addr_of_mut!((*dp).aux.ddc).cast(),
    );
    edid.cast()
}

unsafe fn intel_dp_unset_edid(dp: *mut IntelDp) {
    let connector = (*dp).attached_connector;
    if connector.is_null() {
        return;
    }
    if !(*connector).detect_edid.is_null() {
        tk_drm_upstream::drm_edid_upstream::drm_edid_free((*connector).detect_edid.cast());
        (*connector).detect_edid = ptr::null_mut();
    }
    (*dp).dfp.max_bpc = 0;
    (*dp).dfp.max_dotclock = 0;
    (*dp).dfp.min_tmds_clock = 0;
    (*dp).dfp.max_tmds_clock = 0;
    (*dp).dfp.pcon_max_frl_bw = 0;
    (*dp).dfp.ycbcr_444_to_420 = false;
    (*connector).base.ycbcr_420_allowed = false;
}

unsafe fn intel_dp_update_dfp(dp: *mut IntelDp) {
    let connector = (*dp).attached_connector;
    let edid = (*connector).detect_edid.cast();
    (*dp).dfp.max_bpc = drm_dp::drm_dp_downstream_max_bpc(
        (*dp).dpcd.as_ptr(),
        (*dp).downstream_ports.as_ptr(),
        edid,
    ) as u8;
    (*dp).dfp.max_dotclock = drm_dp::drm_dp_downstream_max_dotclock(
        (*dp).dpcd.as_ptr(),
        (*dp).downstream_ports.as_ptr(),
    );
    (*dp).dfp.min_tmds_clock = drm_dp::drm_dp_downstream_min_tmds_clock(
        (*dp).dpcd.as_ptr(),
        (*dp).downstream_ports.as_ptr(),
        edid,
    );
    (*dp).dfp.max_tmds_clock = drm_dp::drm_dp_downstream_max_tmds_clock(
        (*dp).dpcd.as_ptr(),
        (*dp).downstream_ports.as_ptr(),
        edid,
    );
    // PCON FRL/DSC support is outside the ordinary SST path.
    (*dp).dfp.pcon_max_frl_bw = 0;
}

unsafe fn intel_dp_update_420(dp: *mut IntelDp) {
    let connector = (*dp).attached_connector;
    let dpcd = (*dp).dpcd.as_ptr();
    let ports = (*dp).downstream_ports.as_ptr();
    let branch = drm_dp::drm_dp_is_branch(dpcd);
    (*dp).dfp.ycbcr420_passthrough = drm_dp::drm_dp_downstream_420_passthrough(dpcd, ports);
    (*dp).dfp.ycbcr_444_to_420 = drm_dp::drm_dp_downstream_444_to_420_conversion(dpcd, ports);
    (*dp).dfp.rgb_to_ycbcr = drm_dp::drm_dp_downstream_rgb_to_ycbcr_conversion(
        dpcd,
        ports,
        1 << 6, // DP_DS_HDMI_BT709_RGB_YCBCR_CONV
    );
    let source_420 = source_can_output(dp, INTEL_OUTPUT_FORMAT_YCBCR420);
    let source_rgb = source_can_output(dp, INTEL_OUTPUT_FORMAT_RGB);
    let source_444 = source_can_output(dp, INTEL_OUTPUT_FORMAT_YCBCR444);
    (*connector).base.ycbcr_420_allowed = (source_420
        && (!branch || (*dp).dfp.ycbcr420_passthrough))
        || (source_rgb && (*dp).dfp.rgb_to_ycbcr && (*dp).dfp.ycbcr_444_to_420)
        || (source_444 && (*dp).dfp.ycbcr_444_to_420);
}

unsafe fn intel_dp_set_edid(dp: *mut IntelDp) {
    let connector = (*dp).attached_connector;
    intel_dp_unset_edid(dp);
    (*connector).detect_edid = intel_dp_get_edid(dp) as *mut c_void;
    tk_drm_upstream::drm_edid_upstream::drm_edid_connector_update(
        &mut (*connector).base,
        (*connector).detect_edid.cast(),
    );
    intel_dp_update_dfp(dp);
    intel_dp_update_420(dp);
}

unsafe fn intel_dp_detect_dpcd(dp: *mut IntelDp) -> DrmConnectorStatus {
    (*dp).downstream_port_changed = false;
    if !intel_dp_get_dpcd(dp) {
        return CONNECTOR_STATUS_DISCONNECTED;
    }

    if !drm_dp::drm_dp_is_branch((*dp).dpcd.as_ptr()) {
        return CONNECTOR_STATUS_CONNECTED;
    }
    if has_sink_count(dp) && (*dp).downstream_ports[0] & DP_DS_PORT_HPD != 0 {
        return if (*dp).sink_count != 0 {
            CONNECTOR_STATUS_CONNECTED
        } else {
            CONNECTOR_STATUS_DISCONNECTED
        };
    }
    if tk_drm_upstream::drm_edid_upstream::drm_probe_ddc(ptr::addr_of_mut!((*dp).aux.ddc).cast()) {
        return CONNECTOR_STATUS_CONNECTED;
    }

    let rev = (*dp).dpcd[drm_dp::DP_DPCD_REV as usize];
    let port_type = if rev >= 0x11 {
        (*dp).downstream_ports[0] & drm_dp::DP_DS_PORT_TYPE_MASK
    } else {
        (*dp).dpcd[drm_dp::DP_DOWNSTREAMPORT_PRESENT as usize] & drm_dp::DP_DWN_STRM_PORT_TYPE_MASK
    };
    if (rev >= 0x11
        && matches!(
            port_type,
            drm_dp::DP_DS_PORT_TYPE_VGA | drm_dp::DP_DS_PORT_TYPE_NON_EDID
        ))
        || (rev < 0x11
            && matches!(
                port_type,
                drm_dp::DP_DWN_STRM_PORT_TYPE_ANALOG | drm_dp::DP_DWN_STRM_PORT_TYPE_OTHER
            ))
    {
        CONNECTOR_STATUS_UNKNOWN
    } else {
        CONNECTOR_STATUS_DISCONNECTED
    }
}

unsafe extern "C" fn intel_dp_get_modes(connector: *mut DrmConnector) -> i32 {
    if connector.is_null() {
        return 0;
    }
    let intel_connector = connector.cast::<IntelConnector>();
    let dp = dp_from_connector(intel_connector);
    let mut count = tk_drm_upstream::drm_edid_upstream::drm_edid_connector_add_modes(connector);
    if count == 0 && (*intel_connector).detect_edid.is_null() && !dp.is_null() {
        let dev = (*connector).dev;
        let mode = drm_dp::drm_dp_downstream_mode(
            dev,
            (*dp).dpcd.as_ptr(),
            (*dp).downstream_ports.as_ptr(),
        );
        if !mode.is_null() {
            tk_drm_upstream::drm_modes_upstream::drm_mode_probed_add(connector, mode);
            count += 1;
        }
    }
    count
}

unsafe extern "C" fn intel_dp_detect(
    connector: *mut DrmConnector,
    _ctx: *mut DrmModesetAcquireCtx,
    _force: bool,
) -> i32 {
    if connector.is_null() {
        return CONNECTOR_STATUS_DISCONNECTED as i32;
    }
    let intel_connector = connector.cast::<IntelConnector>();
    let dp = dp_from_connector(intel_connector);
    let encoder = (*intel_connector).encoder;
    let display = display_from_connector(intel_connector);
    if dp.is_null() || encoder.is_null() || display.is_null() {
        return CONNECTOR_STATUS_DISCONNECTED as i32;
    }
    if !crate::intel_display_device_upstream::intel_display_device_enabled(display) {
        return CONNECTOR_STATUS_DISCONNECTED as i32;
    }
    if !crate::intel_display_driver_upstream::intel_display_driver_check_access(display) {
        return (*connector).status as i32;
    }

    intel_dp_flush_connector_commits(intel_connector);
    let status = if intel_digital_port_connected(encoder) {
        intel_dp_detect_dpcd(dp)
    } else {
        CONNECTOR_STATUS_DISCONNECTED
    };
    if status == CONNECTOR_STATUS_CONNECTED {
        intel_dp_init_source_oui(dp);
        if (*dp).reset_link_params {
            intel_dp_reset_link_params(dp);
            (*dp).reset_link_params = false;
        }
        intel_dp_check_link_state(dp);
        (*dp).aux.i2c_nack_count = 0;
        (*dp).aux.i2c_defer_count = 0;
        intel_dp_set_edid(dp);
    } else {
        intel_dp_unset_edid(dp);
    }
    intel_dp_dpcd_set_probe(dp, false);
    drm_dp::drm_dp_set_subconnector_property(
        connector,
        status,
        (*dp).dpcd.as_ptr(),
        (*dp).downstream_ports.as_ptr(),
    );
    status as i32
}

unsafe extern "C" fn intel_dp_force(connector: *mut DrmConnector) {
    if connector.is_null() {
        return;
    }
    let dp = dp_from_connector(connector.cast());
    if dp.is_null()
        || !crate::intel_display_driver_upstream::intel_display_driver_check_access(
            display_from_connector(connector.cast()),
        )
    {
        return;
    }
    intel_dp_unset_edid(dp);
    if (*connector).status == CONNECTOR_STATUS_CONNECTED {
        intel_dp_set_edid(dp);
    }
    intel_dp_dpcd_set_probe(dp, false);
}

unsafe extern "C" fn intel_dp_connector_atomic_check(
    connector: *mut DrmConnector,
    state: *mut tk_drm_upstream::drm_atomic_types::DrmAtomicCommit,
) -> i32 {
    crate::intel_atomic_upstream::intel_digital_connector_atomic_check(connector, state)
}

unsafe extern "C" fn intel_dp_connector_atomic_get_property(
    connector: *mut DrmConnector,
    state: *mut DrmConnectorState,
    property: *mut tk_drm_upstream::drm_property_types::DrmProperty,
    val: *mut u64,
) -> i32 {
    crate::intel_atomic_upstream::intel_digital_connector_atomic_get_property(
        connector, state, property, val,
    )
}

unsafe extern "C" fn intel_dp_connector_sync_state(
    connector: *mut IntelConnector,
    _crtc_state: *mut IntelCrtcState,
) {
    if !connector.is_null() {
        // The SST path never enables DSC decompression.
        (*connector).dp._bitfields_2745[0] &= !(1 << 2);
    }
}

unsafe extern "C" fn intel_dp_connector_register(connector: *mut DrmConnector) -> i32 {
    if connector.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let intel_connector = connector.cast::<IntelConnector>();
    let dp = dp_from_connector(intel_connector);
    if dp.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let ret = crate::intel_connector_upstream::intel_connector_register(connector);
    if ret != 0 {
        return ret;
    }
    (*dp).aux.dev = (*connector).kdev;
    drm_dp::drm_dp_aux_register(&mut (*dp).aux)
}

unsafe extern "C" fn intel_dp_connector_unregister(connector: *mut DrmConnector) {
    if connector.is_null() {
        return;
    }
    let dp = dp_from_connector(connector.cast());
    if !dp.is_null() {
        drm_dp::drm_dp_aux_unregister(&mut (*dp).aux);
    }
    crate::intel_connector_upstream::intel_connector_unregister(connector);
}

static DP_CONNECTOR_FUNCS: DrmConnectorFuncs = DrmConnectorFuncs {
    dpms: None,
    reset: None,
    detect: None,
    force: Some(intel_dp_force),
    fill_modes: Some(tk_drm_upstream::drm_probe_helper_upstream::drm_helper_probe_single_connector_modes),
    set_property: None,
    late_register: Some(intel_dp_connector_register),
    early_unregister: Some(intel_dp_connector_unregister),
    destroy: Some(crate::intel_connector_upstream::intel_connector_destroy),
    atomic_duplicate_state: Some(crate::intel_atomic_upstream::intel_digital_connector_duplicate_state),
    atomic_destroy_state: Some(tk_drm_upstream::drm_atomic_state_helper_upstream::drm_atomic_helper_connector_destroy_state),
    atomic_set_property: Some(crate::intel_atomic_upstream::intel_digital_connector_atomic_set_property),
    atomic_get_property: Some(intel_dp_connector_atomic_get_property),
    atomic_print_state: None,
    oob_hotplug_event: None,
    debugfs_init: None,
};

static DP_CONNECTOR_HELPER_FUNCS: DrmConnectorHelperFuncs = DrmConnectorHelperFuncs {
    get_modes: Some(intel_dp_get_modes),
    detect_ctx: Some(intel_dp_detect),
    mode_valid: Some(intel_dp_mode_valid),
    mode_valid_ctx: None,
    best_encoder: None,
    atomic_best_encoder: None,
    atomic_check: Some(intel_dp_connector_atomic_check),
    atomic_commit: None,
    prepare_writeback_job: None,
    cleanup_writeback_job: None,
    enable_hpd: None,
    disable_hpd: None,
};

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_is_port_edp(display: *mut IntelDisplay, port: i32) -> bool {
    if display.is_null() || DISPLAY_VER(display) < 5 {
        return false;
    }
    if DISPLAY_VER(display) < 9 && port == PORT_A {
        return true;
    }
    let devdata = crate::intel_bios_upstream::intel_bios_encoder_data_lookup(display, port);
    crate::intel_bios_upstream::intel_bios_encoder_supports_edp(devdata)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_init_connector(
    dig_port: *mut IntelDigitalPort,
    connector: *mut IntelConnector,
) -> bool {
    if dig_port.is_null() || connector.is_null() || (*dig_port).max_lanes < 1 {
        return false;
    }
    let dp = ptr::addr_of_mut!((*dig_port).dp);
    let display = display_from_dp(dp);
    if display.is_null()
        || (*dig_port).base.r#type == INTEL_OUTPUT_EDP
        || intel_dp_is_port_edp(display, (*dig_port).base.port)
    {
        return false;
    }

    (*dp).reset_link_params = true;
    (*dp).DP = crate::intel_de_upstream::intel_de_read(display, (*dp).output_reg);
    (*dp).attached_connector = connector;
    default_sink_rates(dp);
    (*dp).max_sink_lane_count = 1;
    intel_dp_aux_init(dp);
    (*connector).dp.dsc_decompression_aux = &mut (*dp).aux;
    let ret = tk_drm_upstream::drm_connector_upstream::drm_connector_init_with_ddc(
        (*dig_port).base.base.dev,
        &mut (*connector).base,
        &DP_CONNECTOR_FUNCS,
        tk_drm_upstream::uapi_drm_consts::DRM_MODE_CONNECTOR_DisplayPort as i32,
        ptr::addr_of_mut!((*dp).aux.ddc).cast(),
    );
    if ret != 0 {
        intel_dp_aux_fini(dp);
        return false;
    }
    drm_connector_helper_add(&mut (*connector).base, &DP_CONNECTOR_HELPER_FUNCS);
    let device_info = (*display).info.__device_info;
    let has_gmch = !device_info.is_null() && (*device_info)._bitfields_46[0] & (1 << 7) != 0;
    (*connector).base.interlace_allowed = !has_gmch && DISPLAY_VER(display) < 12;
    (*connector).polled = DRM_CONNECTOR_POLL_HPD;
    (*connector).base.polled = DRM_CONNECTOR_POLL_HPD;
    crate::intel_connector_upstream::intel_connector_attach_encoder(
        connector,
        &mut (*dig_port).base,
    );
    (*connector).get_hw_state = Some(crate::intel_connector_upstream::intel_connector_get_hw_state);
    (*connector).sync_state = Some(intel_dp_connector_sync_state);
    set_source_rates(dp);
    set_common_link_params(dp);
    intel_dp_reset_link_params(dp);
    tk_drm_upstream::drm_connector_upstream::drm_connector_attach_dp_subconnector_property(
        &mut (*connector).base,
    );
    let _ = tk_drm_upstream::drm_connector_upstream::drm_connector_attach_max_bpc_property(
        &mut (*connector).base,
        6,
        12,
    );
    crate::intel_connector_upstream::intel_attach_broadcast_rgb_property(&mut (*connector).base);
    crate::intel_connector_upstream::intel_attach_dp_colorspace_property(&mut (*connector).base);
    (*dp).frl.is_trained = false;
    (*dp).frl.trained_rate_gbps = 0;
    true
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_cleanup_connector(
    dig_port: *mut IntelDigitalPort,
    connector: *mut IntelConnector,
) {
    if dig_port.is_null() || connector.is_null() {
        return;
    }
    let dp = ptr::addr_of_mut!((*dig_port).dp);
    crate::intel_display_power_upstream::intel_display_power_flush_work(display_from_dp(dp));
    intel_dp_aux_fini(dp);
    tk_drm_upstream::drm_connector_upstream::drm_connector_cleanup(&mut (*connector).base);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_encoder_suspend(encoder: *mut IntelEncoder) {
    // This external SST implementation does not create eDP PPS or USB4 DP
    // tunnel state. The corresponding Linux suspend helpers are therefore
    // intentionally absent from this scoped path.
    let _ = encoder;
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_encoder_shutdown(encoder: *mut IntelEncoder) {
    let _ = encoder;
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_encoder_flush_work(
    encoder: *mut tk_drm_upstream::drm_encoder_types::DrmEncoder,
) {
    if encoder.is_null() {
        return;
    }
    let intel_encoder = encoder.cast::<IntelEncoder>();
    let dp = ptr::addr_of_mut!((*intel_encoder.cast::<IntelDigitalPort>()).dp);
    crate::intel_encoder_upstream::intel_encoder_link_check_flush_work(intel_encoder);
    intel_dp_aux_fini(dp);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_compute_config_late(
    _encoder: *mut IntelEncoder,
    crtc_state: *mut IntelCrtcState,
    _conn_state: *mut DrmConnectorState,
) -> i32 {
    if crtc_state.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    if (*crtc_state).dsc.compression_enable {
        return -tk_linux_kapi::config::EOPNOTSUPP;
    }
    let guardband = crate::intel_vblank_upstream::intel_crtc_vblank_length(crtc_state);
    let min_guardband = intel_dp_sdp_min_guardband(crtc_state, false);
    if guardband < min_guardband {
        return -tk_linux_kapi::config::EINVAL;
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_sdp_min_guardband(
    crtc_state: *const IntelCrtcState,
    assume_all_enabled: bool,
) -> i32 {
    if crtc_state.is_null() {
        return 0;
    }
    let mut guardband = 0;
    // intel_hdmi_infoframe_enable() maps gamut metadata to bit 1 and
    // Adaptive-Sync SDP to bit 3 in infoframes.enable.
    if assume_all_enabled || (*crtc_state).infoframes.enable & (1 << 1) != 0 {
        guardband = guardband.max(8); // HDMI_PACKET_TYPE_GAMUT_METADATA
    }
    if assume_all_enabled || (*crtc_state).dsc.compression_enable {
        guardband = guardband.max(7); // DP_SDP_PPS
    }
    if (*crtc_state).infoframes.enable & (1 << 3) != 0 {
        guardband = guardband.max((*crtc_state).vrr.vsync_start as i32 + 1);
    }
    guardband
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_mst_active_streams(dp: *mut IntelDp) -> i32 {
    if dp.is_null() {
        0
    } else {
        (*dp).mst.active_streams
    }
}

unsafe extern "C" {
    fn drm_connector_helper_add(
        connector: *mut DrmConnector,
        funcs: *const DrmConnectorHelperFuncs,
    );
    fn intel_encoder_is_combo(encoder: *mut IntelEncoder) -> bool;
    fn intel_tc_port_max_lane_count(dig_port: *mut IntelDigitalPort) -> i32;
    fn intel_tc_port_in_tbt_alt_mode(dig_port: *mut IntelDigitalPort) -> bool;
    fn intel_dp_aux_init(dp: *mut IntelDp);
    fn intel_dp_aux_fini(dp: *mut IntelDp);
    fn intel_dp_init_lttpr_and_dprx_caps(dp: *mut IntelDp) -> i32;
    fn intel_dp_read_dprx_caps(dp: *mut IntelDp, dpcd: *mut u8) -> i32;
    fn intel_tc_port_handles_hpd_glitches(dig_port: *mut IntelDigitalPort) -> bool;
    fn intel_cpu_transcoder_mode_valid(
        display: *mut IntelDisplay,
        mode: *const DrmDisplayMode,
    ) -> DrmModeStatus;
    fn intel_mode_valid_max_plane_size(
        display: *mut IntelDisplay,
        mode: *const DrmDisplayMode,
        num_joined_pipes: i32,
    ) -> DrmModeStatus;
    fn intel_max_uncompressed_dotclock(display: *mut IntelDisplay) -> i32;
    fn intel_pfit_mode_valid(
        display: *mut IntelDisplay,
        mode: *const DrmDisplayMode,
        output_format: u32,
        num_joined_pipes: i32,
    ) -> DrmModeStatus;
    fn intel_pfit_compute_config(
        crtc_state: *mut IntelCrtcState,
        conn_state: *const DrmConnectorState,
    ) -> i32;
    fn intel_link_compute_m_n(
        bpp_x16: u16,
        lane_count: i32,
        pixel_clock: i32,
        link_clock: i32,
        bw_overhead: i32,
        mn: *mut crate::intel_display_types_upstream::IntelLinkMN,
    );
    fn intel_hdmi_tmds_clock(clock: i32, bpc: i32, sink_format: u32) -> i32;
    fn intel_modeset_commit_pipes(
        display: *mut IntelDisplay,
        pipe_mask: u8,
        ctx: *mut DrmModesetAcquireCtx,
    ) -> i32;
}

unsafe fn dp_has_connector(dp: *mut IntelDp, state: *const DrmConnectorState) -> bool {
    if dp.is_null() || state.is_null() {
        return false;
    }
    let dig_port = dp_to_dig_port(dp);
    ptr::addr_of!((*dig_port).base.base).cast_mut() == (*state).best_encoder
}

unsafe fn wait_for_connector_hw_done(state: *const DrmConnectorState) {
    if state.is_null() || (*state).commit.is_null() {
        return;
    }
    let timeout = tk_linux_kapi::primitives::msecs_to_jiffies(5_000u32) as i64;
    let _ = (*(*state).commit).hw_done.wait_timeout(timeout);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_flush_connector_commits(connector: *mut IntelConnector) {
    if !connector.is_null() {
        wait_for_connector_hw_done((*connector).base.state);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_get_active_pipes(
    dp: *mut IntelDp,
    ctx: *mut DrmModesetAcquireCtx,
    pipe_mask: *mut u8,
) -> i32 {
    if dp.is_null() || pipe_mask.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    *pipe_mask = 0;
    let display = display_from_dp(dp);
    if display.is_null() || (*display).drm.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }

    let mut iter = DrmConnectorListIter {
        dev: ptr::null_mut(),
        conn: ptr::null_mut(),
    };
    tk_drm_upstream::drm_connector_upstream::drm_connector_list_iter_begin(
        (*display).drm,
        &mut iter,
    );
    let mut ret = 0;
    loop {
        let connector =
            tk_drm_upstream::drm_connector_upstream::drm_connector_list_iter_next(&mut iter);
        if connector.is_null() {
            break;
        }
        let conn_state = (*connector).state;
        if conn_state.is_null() || !dp_has_connector(dp, conn_state) || (*conn_state).crtc.is_null()
        {
            continue;
        }
        let crtc = (*conn_state).crtc.cast::<IntelCrtc>();
        ret = tk_drm_upstream::drm_modeset_lock_upstream::drm_modeset_lock(
            ptr::addr_of_mut!((*crtc).base.mutex),
            ctx,
        );
        if ret != 0 {
            break;
        }
        let crtc_state = (*crtc).base.state.cast::<IntelCrtcState>();
        if crtc_state.is_null() || !(*crtc_state).hw.active {
            continue;
        }
        wait_for_connector_hw_done(conn_state);
        *pipe_mask |= 1u8 << ((*crtc).pipe as u32);
    }
    tk_drm_upstream::drm_connector_upstream::drm_connector_list_iter_end(&mut iter);
    ret
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_digital_port_lock(encoder: *mut IntelEncoder) {
    if encoder.is_null() {
        return;
    }
    let dig_port = encoder.cast::<IntelDigitalPort>();
    if let Some(lock) = (*dig_port).lock {
        lock(dig_port);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_digital_port_unlock(encoder: *mut IntelEncoder) {
    if encoder.is_null() {
        return;
    }
    let dig_port = encoder.cast::<IntelDigitalPort>();
    if let Some(unlock) = (*dig_port).unlock {
        unlock(dig_port);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_digital_port_connected_locked(encoder: *mut IntelEncoder) -> bool {
    if encoder.is_null() {
        return false;
    }
    let dig_port = encoder.cast::<IntelDigitalPort>();
    let display = display_from_encoder(encoder);
    if display.is_null() {
        return false;
    }
    let wakeref = crate::intel_display_power_upstream::intel_display_power_get(
        display,
        POWER_DOMAIN_DISPLAY_CORE,
    );
    let glitch_free = intel_tc_port_handles_hpd_glitches(dig_port);
    let mut connected = false;
    let _ = tk_linux_kapi::poll_timeout_us!(
        {
            connected = (*dig_port)
                .connected
                .map(|is_connected| is_connected(encoder))
                .unwrap_or(false);
        },
        connected || glitch_free,
        30,
        4_000,
        false
    );
    crate::intel_display_power_upstream::intel_display_power_put(
        display,
        POWER_DOMAIN_DISPLAY_CORE,
        wakeref,
    );
    connected
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_digital_port_connected(encoder: *mut IntelEncoder) -> bool {
    intel_digital_port_lock(encoder);
    let connected = intel_digital_port_connected_locked(encoder);
    intel_digital_port_unlock(encoder);
    connected
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_aux_power_domain(
    dig_port: *mut IntelDigitalPort,
) -> IntelDisplayPowerDomain {
    if dig_port.is_null() {
        return POWER_DOMAIN_AUX_A;
    }
    let display = display_from_encoder(&mut (*dig_port).base);
    if display.is_null() {
        return POWER_DOMAIN_AUX_A;
    }
    if intel_tc_port_in_tbt_alt_mode(dig_port) {
        crate::intel_display_power_upstream::intel_display_power_tbt_aux_domain(
            display,
            (*dig_port).aux_ch,
        )
    } else {
        crate::intel_display_power_upstream::intel_display_power_legacy_aux_domain(
            display,
            (*dig_port).aux_ch,
        )
    }
}

const DP_SOURCE_OUI: u32 = 0x300;
const DP_SET_POWER_D0: u8 = 1;
const DP_SET_POWER_D3: u8 = 2;
const DP_DS_PORT_HPD: u8 = 1 << 3;
const DP_DEVICE_SERVICE_IRQ_VECTOR_ESI0: u32 = 0x2003;
const DP_LINK_SERVICE_IRQ_VECTOR_ESI0: u32 = 0x2005;
const DP_AUTOMATED_TEST_REQUEST: u8 = 1 << 1;
const DP_CP_IRQ: u8 = 1 << 2;
const DP_SINK_SPECIFIC_IRQ: u8 = 1 << 6;
const RX_CAP_CHANGED: u8 = 1 << 0;
const LINK_STATUS_CHANGED: u8 = 1 << 1;
const HDMI_LINK_STATUS_CHANGED: u8 = 1 << 3;
const CONNECTED_OFF_ENTRY_REQUESTED: u8 = 1 << 4;
const DP_TUNNELING_IRQ: u8 = 1 << 5;
const DP_DOWNSTREAM_PORT_STATUS_CHANGED: u8 = 1 << 6;
const IRQ_NONE: Irqreturn = 0;
const IRQ_HANDLED: Irqreturn = 1;

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_dpcd_set_probe(dp: *mut IntelDp, force_on_external: bool) {
    if dp.is_null() {
        return;
    }
    let connector = (*dp).attached_connector;
    let enable = if intel_dp_is_edp(dp) {
        false
    } else if force_on_external {
        true
    } else if (*dp).is_mst || connector.is_null() {
        false
    } else {
        tk_drm_upstream::drm_edid_upstream::drm_edid_has_quirk(
            &mut (*connector).base,
            tk_drm_upstream::drm_edid_upstream::DRM_EDID_QUIRK_DP_DPCD_PROBE,
        )
    };
    drm_dp::drm_dp_dpcd_set_probe(&mut (*dp).aux, enable);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_invalidate_source_oui(dp: *mut IntelDp) {
    if !dp.is_null() {
        (*dp).oui_valid = false;
    }
}

unsafe fn intel_dp_init_source_oui(dp: *mut IntelDp) {
    if dp.is_null() || (*dp).oui_valid {
        return;
    }
    (*dp).oui_valid = true;
    let mut oui = [0u8; 3];
    let expected = [0x00, 0xaa, 0x01];
    let _ = drm_dp::drm_dp_dpcd_read(
        &mut (*dp).aux,
        DP_SOURCE_OUI,
        oui.as_mut_ptr().cast(),
        oui.len(),
    );
    if oui == expected {
        (*dp).last_oui_write = tk_linux_kapi::primitives::jiffies();
        return;
    }
    if drm_dp::drm_dp_dpcd_write(
        &mut (*dp).aux,
        DP_SOURCE_OUI,
        expected.as_ptr().cast_mut().cast(),
        expected.len(),
    ) < 0
    {
        (*dp).oui_valid = false;
    }
    (*dp).last_oui_write = tk_linux_kapi::primitives::jiffies();
}

unsafe fn downstream_hpd_needs_d0(dp: *mut IntelDp) -> bool {
    (*dp).dpcd[drm_dp::DP_DPCD_REV as usize] == 0x11
        && drm_dp::drm_dp_is_branch((*dp).dpcd.as_ptr())
        && ((*dp).downstream_ports[0] & DP_DS_PORT_HPD) != 0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_set_power(dp: *mut IntelDp, mode: u8) {
    if dp.is_null() || (*dp).dpcd[drm_dp::DP_DPCD_REV as usize] < 0x11 {
        return;
    }
    if mode != DP_SET_POWER_D0 {
        if downstream_hpd_needs_d0(dp) {
            return;
        }
        let _ = drm_dp::drm_dp_dpcd_writeb(&mut (*dp).aux, drm_dp::DP_SET_POWER, mode);
        return;
    }

    intel_dp_init_source_oui(dp);
    let mut ret = -tk_linux_kapi::config::EIO as isize;
    for _ in 0..3 {
        ret = drm_dp::drm_dp_dpcd_writeb(&mut (*dp).aux, drm_dp::DP_SET_POWER, mode);
        if ret == 1 {
            break;
        }
        tk_linux_kapi::wait::msleep(1);
    }
    let _ = ret;
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_sync_state(
    encoder: *mut IntelEncoder,
    crtc_state: *const IntelCrtcState,
) {
    if encoder.is_null() {
        return;
    }
    let dp = ptr::addr_of_mut!((*encoder.cast::<IntelDigitalPort>()).dp);
    if !crtc_state.is_null() && (*dp).dpcd[drm_dp::DP_DPCD_REV as usize] == 0 {
        let _ = intel_dp_get_dpcd(dp);
    }
    if !crtc_state.is_null() {
        intel_dp_reset_link_params(dp);
        intel_dp_set_link_params(
            dp,
            (*crtc_state).port_clock,
            (*crtc_state).lane_count as i32,
        );
        (*dp).link.active = true;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_initial_fastset_check(
    encoder: *mut IntelEncoder,
    crtc_state: *mut IntelCrtcState,
) -> bool {
    if encoder.is_null() || crtc_state.is_null() {
        return false;
    }
    let dp = ptr::addr_of_mut!((*encoder.cast::<IntelDigitalPort>()).dp);
    let mut fastset = true;
    if intel_dp_rate_index(
        (*dp).source_rates,
        (*dp).num_source_rates,
        (*crtc_state).port_clock,
    ) < 0
    {
        (*crtc_state).uapi.set_connectors_changed(true);
        fastset = false;
    }
    if (*crtc_state).dsc.compression_enable {
        (*crtc_state).uapi.set_mode_changed(true);
        fastset = false;
    }
    fastset
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_queue_modeset_retry_for_link(
    _state: *mut IntelAtomicState,
    encoder: *mut IntelEncoder,
    _crtc_state: *const IntelCrtcState,
) {
    if encoder.is_null() {
        return;
    }
    let dp = ptr::addr_of_mut!((*encoder.cast::<IntelDigitalPort>()).dp);
    if (*dp).needs_modeset_retry {
        return;
    }
    (*dp).needs_modeset_retry = true;
    if !(*dp).attached_connector.is_null() {
        crate::intel_connector_upstream::intel_connector_queue_modeset_retry_work(
            (*dp).attached_connector,
        );
    }
}

unsafe fn read_link_status(dp: *mut IntelDp, link_status: *mut u8) -> i32 {
    if dp.is_null() || link_status.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    ptr::write_bytes(link_status, 0, drm_dp::DP_LINK_STATUS_SIZE);
    let ret =
        drm_dp::drm_dp_dpcd_read_phy_link_status(&mut (*dp).aux, drm_dp::DP_PHY_DPRX, link_status);
    if ret != 0 {
        return ret;
    }
    if *link_status.add((drm_dp::DP_LANE_ALIGN_STATUS_UPDATED - drm_dp::DP_LANE0_1_STATUS) as usize)
        & DP_DOWNSTREAM_PORT_STATUS_CHANGED
        != 0
    {
        (*dp).downstream_port_changed = true;
    }
    0
}

unsafe fn needs_link_retrain(dp: *mut IntelDp) -> bool {
    if dp.is_null() || !(*dp).link.active {
        return false;
    }
    if (*dp).link.force_retrain {
        return true;
    }
    let mut status = [0u8; drm_dp::DP_LINK_STATUS_SIZE];
    if read_link_status(dp, status.as_mut_ptr()) < 0 {
        return false;
    }
    if !intel_dp_link_params_valid(dp, (*dp).link_rate, (*dp).lane_count) {
        return false;
    }
    if (*dp).link.retrain_disabled {
        return false;
    }
    if (*dp).link.seq_train_failures != 0 {
        return true;
    }
    !drm_dp::drm_dp_clock_recovery_ok(status.as_ptr(), (*dp).lane_count as i32)
        || !drm_dp::drm_dp_channel_eq_ok(status.as_ptr(), (*dp).lane_count as i32)
}

unsafe fn dp_is_connected(dp: *mut IntelDp) -> bool {
    !dp.is_null()
        && !(*dp).attached_connector.is_null()
        && (*(*dp).attached_connector).base.status == CONNECTOR_STATUS_CONNECTED
}

unsafe fn retrain_link(encoder: *mut IntelEncoder, ctx: *mut DrmModesetAcquireCtx) -> i32 {
    if encoder.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let dp = ptr::addr_of_mut!((*encoder.cast::<IntelDigitalPort>()).dp);
    if !dp_is_connected(dp) {
        return 0;
    }
    let display = display_from_encoder(encoder);
    let ret = tk_drm_upstream::drm_modeset_lock_upstream::drm_modeset_lock(
        ptr::addr_of_mut!((*(*display).drm).mode_config.connection_mutex),
        ctx,
    );
    if ret != 0 {
        return ret;
    }
    if !needs_link_retrain(dp) {
        return 0;
    }
    let mut pipe_mask = 0u8;
    let ret = intel_dp_get_active_pipes(dp, ctx, &mut pipe_mask);
    if ret != 0 || pipe_mask == 0 {
        return ret;
    }
    if !needs_link_retrain(dp) {
        return 0;
    }
    let ret = intel_modeset_commit_pipes(display, pipe_mask, ctx);
    if ret != -tk_linux_kapi::config::EDEADLK {
        (*dp).link.force_retrain = false;
    }
    ret
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_link_check(encoder: *mut IntelEncoder) {
    if encoder.is_null() {
        return;
    }
    let mut ctx: DrmModesetAcquireCtx = core::mem::zeroed();
    let mut ret = 0;
    crate::intel_modeset_lock_upstream::_intel_modeset_lock_begin(
        &mut ctx,
        ptr::null_mut(),
        0,
        &mut ret,
    );
    while crate::intel_modeset_lock_upstream::_intel_modeset_lock_loop(&mut ret) {
        ret = retrain_link(encoder, &mut ctx);
        crate::intel_modeset_lock_upstream::_intel_modeset_lock_end(
            &mut ctx,
            ptr::null_mut(),
            &mut ret,
        );
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_check_link_state(dp: *mut IntelDp) {
    if dp.is_null() || !dp_is_connected(dp) || !needs_link_retrain(dp) {
        return;
    }
    let encoder = &mut (*dp_to_dig_port(dp)).base;
    crate::intel_encoder_upstream::intel_encoder_link_check_queue_work(encoder, 0);
}

unsafe fn get_and_ack_sink_irq_esi_sst(dp: *mut IntelDp, esi: *mut u8) -> bool {
    if dp.is_null() || esi.is_null() {
        return false;
    }
    ptr::write_bytes(esi, 0, 4);
    if drm_dp::drm_dp_dpcd_read_data(&mut (*dp).aux, drm_dp::DP_SINK_COUNT, esi.cast(), 2) != 0 {
        return false;
    }
    if (*dp).dpcd[drm_dp::DP_DPCD_REV as usize] >= 0x12
        && drm_dp::drm_dp_dpcd_read_byte(
            &mut (*dp).aux,
            DP_LINK_SERVICE_IRQ_VECTOR_ESI0,
            esi.add(3),
        ) != 0
    {
        return false;
    }
    *esi.add(1) &= DP_AUTOMATED_TEST_REQUEST | DP_CP_IRQ | DP_SINK_SPECIFIC_IRQ;
    *esi.add(3) &= RX_CAP_CHANGED
        | LINK_STATUS_CHANGED
        | HDMI_LINK_STATUS_CHANGED
        | CONNECTED_OFF_ENTRY_REQUESTED
        | DP_TUNNELING_IRQ;
    if *esi.add(1) == 0 && *esi.add(2) == 0 && *esi.add(3) == 0 {
        return true;
    }
    if drm_dp::drm_dp_dpcd_write_byte(
        &mut (*dp).aux,
        DP_DEVICE_SERVICE_IRQ_VECTOR_ESI0,
        *esi.add(1),
    ) != 0
    {
        return false;
    }
    (*dp).dpcd[drm_dp::DP_DPCD_REV as usize] < 0x12
        || drm_dp::drm_dp_dpcd_write_byte(
            &mut (*dp).aux,
            DP_LINK_SERVICE_IRQ_VECTOR_ESI0,
            *esi.add(3),
        ) == 0
}

unsafe fn short_pulse(dp: *mut IntelDp) -> bool {
    let mut esi = [0u8; 4];
    if !get_and_ack_sink_irq_esi_sst(dp, esi.as_mut_ptr()) {
        return false;
    }
    if has_sink_count(dp) {
        let sink_count = ((esi[0] & 0x80) >> 1) | (esi[0] & 0x3f);
        if sink_count != (*dp).sink_count {
            return false;
        }
    }
    if esi[3] & RX_CAP_CHANGED != 0 {
        (*dp).reset_link_params = true;
        return false;
    }
    // Force checking link status even if DPCD < 1.2, per the SST IRQ path.
    if esi[3] & LINK_STATUS_CHANGED != 0 || (*dp).dpcd[drm_dp::DP_DPCD_REV as usize] < 0x12 {
        intel_dp_check_link_state(dp);
    }
    if (*dp).downstream_port_changed {
        (*dp).downstream_port_changed = false;
        return false;
    }
    true
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_hpd_pulse(
    dig_port: *mut IntelDigitalPort,
    long_hpd: bool,
) -> Irqreturn {
    if dig_port.is_null() {
        return IRQ_NONE;
    }
    let dp = ptr::addr_of_mut!((*dig_port).dp);
    if long_hpd {
        intel_dp_dpcd_set_probe(dp, true);
        let mut dpcd = [0u8; drm_dp::DP_RECEIVER_CAP_SIZE];
        let _ = intel_dp_read_dprx_caps(dp, dpcd.as_mut_ptr());
        (*dp).reset_link_params = true;
        intel_dp_invalidate_source_oui(dp);
        return IRQ_NONE;
    }
    if short_pulse(dp) {
        IRQ_HANDLED
    } else {
        IRQ_NONE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_coding_rate_and_clock_units() {
        unsafe {
            assert_eq!(intel_dp_link_symbol_size(DP_RATE_810), 10);
            assert_eq!(intel_dp_link_symbol_size(1_000_000), 32);
            assert_eq!(intel_dp_link_symbol_clock(DP_RATE_162), DP_RATE_162);
            assert_eq!(intel_dp_link_symbol_clock(1_000_000), 312_500);
        }
    }

    #[test]
    fn sst_bpp_and_fec_bandwidth_math() {
        unsafe {
            assert_eq!(intel_dp_min_bpp(INTEL_OUTPUT_FORMAT_RGB), 18);
            assert_eq!(intel_dp_min_bpp(INTEL_OUTPUT_FORMAT_YCBCR420), 24);
            assert_eq!(
                intel_dp_output_format_link_bpp_x16(INTEL_OUTPUT_FORMAT_RGB, 24),
                24 << 4
            );
            assert_eq!(
                intel_dp_output_format_link_bpp_x16(INTEL_OUTPUT_FORMAT_YCBCR420, 24),
                12 << 4
            );
            assert_eq!(intel_dp_bw_fec_overhead(false), 1_000_000);
            assert_eq!(intel_dp_bw_fec_overhead(true), 1_028_530);
            assert_eq!(intel_dp_mode_to_fec_clock(148_500), 152_736);
            assert_eq!(
                intel_dp_effective_data_rate(148_500, 24 << 4, 1_000_000),
                445_500
            );
        }
    }

    #[test]
    fn common_rate_lookup_rejects_missing_rates() {
        let rates = [DP_RATE_162, DP_RATE_270, DP_RATE_540];
        unsafe {
            assert_eq!(
                intel_dp_rate_index(rates.as_ptr(), rates.len() as i32, DP_RATE_270),
                1
            );
            assert_eq!(
                intel_dp_rate_index(rates.as_ptr(), rates.len() as i32, DP_RATE_648),
                -1
            );
            assert_eq!(intel_dp_rate_index(rates.as_ptr(), 0, DP_RATE_162), -1);
        }
    }

    #[test]
    fn sdp_guardband_covers_enabled_packet_lines() {
        let mut state: IntelCrtcState = unsafe { core::mem::zeroed() };
        unsafe {
            state.infoframes.enable = 1 << 1; // gamut metadata
            assert_eq!(intel_dp_sdp_min_guardband(&state, false), 8);
            state.infoframes.enable = 1 << 3; // Adaptive-Sync SDP
            state.vrr.vsync_start = 4;
            assert_eq!(intel_dp_sdp_min_guardband(&state, false), 5);
            assert_eq!(intel_dp_sdp_min_guardband(&state, true), 8);
        }
    }
}

// ---- intel_dp.c: DSC decompression sequencing and MST/tunnel state hooks ----

/// `DP_DSC_ENABLE` (drm_dp.h).
const DP_DSC_ENABLE: u32 = 0x160;
/// `DP_DECOMPRESSION_EN` (drm_dp.h).
const DP_DECOMPRESSION_EN: u8 = 1 << 0;
/// `DP_DSC_PASSTHROUGH_EN` (drm_dp.h).
const DP_DSC_PASSTHROUGH_EN: u8 = 1 << 1;

/// `connector->dp.dsc_decompression_enabled` (bit 2 of the connector bitfields).
#[inline]
unsafe fn dsc_decompression_enabled(connector: *const IntelConnector) -> bool {
    unsafe { (*connector).dp._bitfields_2745[0] & (1 << 2) != 0 }
}

#[inline]
unsafe fn set_dsc_decompression_enabled(connector: *mut IntelConnector, enabled: bool) {
    unsafe {
        let bits = &mut (*connector).dp._bitfields_2745[0];
        if enabled {
            *bits |= 1 << 2;
        } else {
            *bits &= !(1 << 2);
        }
    }
}

/// `to_intel_display(state)`.
#[inline]
pub(crate) unsafe fn display_of_state(state: *const IntelAtomicState) -> *mut IntelDisplay {
    unsafe { crate::intel_display_conversion_upstream::__drm_to_display((*state).base.dev) }
}

/// `write_dsc_decompression_flag()`: read-modify-write one DSC enable bit.
// upstream: intel_dp.c write_dsc_decompression_flag()
unsafe fn write_dsc_decompression_flag(
    aux: *mut tk_drm_upstream::drm_dp_helper_types::DrmDpAux,
    flag: u8,
    set: bool,
) -> isize {
    unsafe {
        let mut val: u8 = 0;
        let err = tk_drm_upstream::drm_dp_helper_upstream::drm_dp_dpcd_readb(aux, DP_DSC_ENABLE, &mut val);
        if err < 0 {
            return err;
        }
        if set {
            val |= flag;
        } else {
            val &= !flag;
        }
        tk_drm_upstream::drm_dp_helper_upstream::drm_dp_dpcd_writeb(aux, DP_DSC_ENABLE, val)
    }
}

/// `intel_dp_sink_set_dsc_decompression()`.
// upstream: intel_dp.c intel_dp_sink_set_dsc_decompression()
unsafe fn intel_dp_sink_set_dsc_decompression(connector: *mut IntelConnector, enable: bool) {
    unsafe {
        // A failed DPCD write is only logged in Linux (drm_dbg_kms).
        let _ = write_dsc_decompression_flag((*connector).dp.dsc_decompression_aux, DP_DECOMPRESSION_EN, enable);
    }
}

/// `intel_dp_sink_set_dsc_passthrough()`.
// upstream: intel_dp.c intel_dp_sink_set_dsc_passthrough()
unsafe fn intel_dp_sink_set_dsc_passthrough(connector: *const IntelConnector, enable: bool) {
    unsafe {
        // The passthrough AUX lives on an MST port; this build never creates one
        // (intel_dp_mst_init() is not ported), so the AUX is NULL and Linux returns.
        if !(*connector).mst.port.is_null() {
            return;
        }
        let _ = enable;
    }
}

/// `intel_dp_dsc_aux_ref_count()`: how many connectors in `state` share this
/// connector's DSC decompression AUX.
// upstream: intel_dp.c intel_dp_dsc_aux_ref_count()
unsafe fn intel_dp_dsc_aux_ref_count(
    state: *mut IntelAtomicState,
    connector: *const IntelConnector,
    for_get_ref: bool,
) -> i32 {
    unsafe {
        if (*connector).mst.dp.is_null() {
            return if dsc_decompression_enabled(connector) { 1 } else { 0 };
        }

        let mut ref_count = 0;
        let count = (*state).base.num_connector.max(0) as usize;
        for index in 0..count {
            let entry = (*state).base.connectors.add(index);
            let old_conn_state = (*entry).old_state;
            let new_conn_state = (*entry).new_state;
            let iter = (*entry).ptr.cast::<IntelConnector>();
            if iter.is_null() || old_conn_state.is_null() || new_conn_state.is_null() {
                continue;
            }
            if (*iter).mst.dp != (*connector).mst.dp {
                continue;
            }
            if !dsc_decompression_enabled(iter) {
                continue;
            }
            let _ = (for_get_ref && (*new_conn_state).crtc.is_null())
                || (!for_get_ref && (*old_conn_state).crtc.is_null());
            if (*iter).dp.dsc_decompression_aux == (*connector).dp.dsc_decompression_aux {
                ref_count += 1;
            }
        }
        ref_count
    }
}

/// `intel_dp_dsc_aux_get_ref()`.
// upstream: intel_dp.c intel_dp_dsc_aux_get_ref()
unsafe fn intel_dp_dsc_aux_get_ref(state: *mut IntelAtomicState, connector: *mut IntelConnector) -> bool {
    unsafe {
        let ret = intel_dp_dsc_aux_ref_count(state, connector, true) == 0;
        set_dsc_decompression_enabled(connector, true);
        ret
    }
}

/// `intel_dp_dsc_aux_put_ref()`.
// upstream: intel_dp.c intel_dp_dsc_aux_put_ref()
unsafe fn intel_dp_dsc_aux_put_ref(state: *mut IntelAtomicState, connector: *mut IntelConnector) -> bool {
    unsafe {
        set_dsc_decompression_enabled(connector, false);
        intel_dp_dsc_aux_ref_count(state, connector, false) == 0
    }
}

/// `intel_dp_sink_enable_decompression()`.
// upstream: intel_dp.c intel_dp_sink_enable_decompression()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_sink_enable_decompression(
    state: *mut IntelAtomicState,
    connector: *mut IntelConnector,
    new_crtc_state: *const IntelCrtcState,
) {
    unsafe {
        if !(*new_crtc_state).dsc.compression_enable {
            return;
        }
        let display = display_of_state(state);
        let bad = (*connector).dp.dsc_decompression_aux.is_null() || dsc_decompression_enabled(connector);
        if drm_WARN_ON!((*display).drm, bad) {
            return;
        }
        if !intel_dp_dsc_aux_get_ref(state, connector) {
            return;
        }
        intel_dp_sink_set_dsc_passthrough(connector, true);
        intel_dp_sink_set_dsc_decompression(connector, true);
    }
}

/// `intel_dp_sink_disable_decompression()`.
// upstream: intel_dp.c intel_dp_sink_disable_decompression()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_sink_disable_decompression(
    state: *mut IntelAtomicState,
    connector: *mut IntelConnector,
    old_crtc_state: *const IntelCrtcState,
) {
    unsafe {
        if !(*old_crtc_state).dsc.compression_enable {
            return;
        }
        let display = display_of_state(state);
        let bad = (*connector).dp.dsc_decompression_aux.is_null() || !dsc_decompression_enabled(connector);
        if drm_WARN_ON!((*display).drm, bad) {
            return;
        }
        if !intel_dp_dsc_aux_put_ref(state, connector) {
            return;
        }
        intel_dp_sink_set_dsc_decompression(connector, false);
        intel_dp_sink_set_dsc_passthrough(connector, false);
    }
}

/// `intel_dp_mst_add_topology_state_for_connector()`.
///
/// The MST topology manager (`drm_dp_mst_topology_mgr`) and its atomic state are
/// not ported, and `intel_dp_mst_init()` never runs in this build, so
/// `connector->mst.dp` is always NULL and Linux returns 0 on the first check.
/// The MST branch is therefore unreachable and reports `-EOPNOTSUPP`.
// upstream: intel_dp_mst.c intel_dp_mst_add_topology_state_for_connector()
unsafe fn intel_dp_mst_add_topology_state_for_connector(
    _state: *mut IntelAtomicState,
    connector: *mut IntelConnector,
    _crtc: *mut IntelCrtc,
) -> i32 {
    unsafe {
        if (*connector).mst.dp.is_null() {
            return 0;
        }
    }
    -tk_linux_kapi::config::EOPNOTSUPP
}

/// `intel_dp_mst_add_topology_state_for_crtc()`.
// upstream: intel_dp_mst.c intel_dp_mst_add_topology_state_for_crtc()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_mst_add_topology_state_for_crtc(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
) -> i32 {
    unsafe {
        let count = (*state).base.num_connector.max(0) as usize;
        for index in 0..count {
            let entry = (*state).base.connectors.add(index);
            let conn_state = (*entry).new_state;
            if conn_state.is_null() {
                continue;
            }
            if (*conn_state).crtc != core::ptr::addr_of_mut!((*crtc).base) {
                continue;
            }
            let connector = (*entry).ptr.cast::<IntelConnector>();
            let ret = intel_dp_mst_add_topology_state_for_connector(state, connector, crtc);
            if ret != 0 {
                return ret;
            }
        }
        0
    }
}

/// `intel_dp_tunnel_atomic_add_state_for_crtc()`.
///
/// Tunnels are created only by the DP tunnel manager, which is not ported, so
/// `dp_tunnel_ref.tunnel` is always NULL and Linux returns 0. The non-NULL
/// branch (`drm_dp_tunnel_atomic_get_state()`) is unreachable and reports `-EOPNOTSUPP`.
// upstream: intel_dp_tunnel.c intel_dp_tunnel_atomic_add_state_for_crtc()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_tunnel_atomic_add_state_for_crtc(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
) -> i32 {
    unsafe {
        let new_crtc_state = crate::intel_atomic_upstream::intel_atomic_get_new_crtc_state(state, crtc);
        if (*new_crtc_state).dp_tunnel_ref.tunnel.is_null() {
            return 0;
        }
    }
    -tk_linux_kapi::config::EOPNOTSUPP
}

// ---- intel_dp.c: DP secondary-data packets and infoframe enable ----

/// `DP_SDP_VSC` (drm_dp.h).
const DP_SDP_VSC_TYPE: u32 = 0x07;
/// `DP_SDP_ADAPTIVE_SYNC` (drm_dp.h).
const DP_SDP_ADAPTIVE_SYNC_TYPE: u32 = 0x22;
/// `HDMI_PACKET_TYPE_GAMUT_METADATA` (linux/hdmi.h).
const HDMI_PACKET_TYPE_GAMUT_METADATA: u32 = 0x0a;
/// `HDMI_INFOFRAME_HEADER_SIZE` + `HDMI_DRM_INFOFRAME_SIZE`.
const HDMI_DRM_INFOFRAME_TOTAL: usize = 4 + 26;
const ENOSPC: isize = 28;

/// `intel_dp_as_sdp_pack()`.
// upstream: intel_dp.c intel_dp_as_sdp_pack()
unsafe fn intel_dp_as_sdp_pack(
    as_sdp: *const tk_drm_upstream::drm_dp_helper_types::DrmDpAsSdp,
    sdp: *mut tk_drm_upstream::drm_dp_types::DpSdp,
    size: usize,
) -> isize {
    unsafe {
        let length = core::mem::size_of::<tk_drm_upstream::drm_dp_types::DpSdp>();
        if size < length {
            return -ENOSPC;
        }
        core::ptr::write_bytes(sdp.cast::<u8>(), 0, size);

        let sdp = &mut *sdp;
        let as_sdp = &*as_sdp;
        sdp.sdp_header.HB0 = 0;
        sdp.sdp_header.HB1 = as_sdp.sdp_type;
        sdp.sdp_header.HB2 = as_sdp.revision;
        sdp.sdp_header.HB3 = as_sdp.length;
        sdp.db[0] = as_sdp.mode as u8;
        sdp.db[1] = (as_sdp.vtotal & 0xFF) as u8;
        sdp.db[2] = ((as_sdp.vtotal >> 8) & 0xFF) as u8;
        sdp.db[3] = (as_sdp.target_rr & 0xFF) as u8;
        sdp.db[4] = ((as_sdp.target_rr >> 8) & 0x3) as u8;
        if as_sdp.target_rr_divider {
            sdp.db[4] |= 0x20;
        }
        sdp.db[7] = (as_sdp.coasting_vtotal & 0xFF) as u8;
        sdp.db[8] = ((as_sdp.coasting_vtotal >> 8) & 0xFF) as u8;
        length as isize
    }
}

/// `intel_dp_hdr_metadata_infoframe_sdp_pack()`: HDR static metadata as a DP SDP.
// upstream: intel_dp.c intel_dp_hdr_metadata_infoframe_sdp_pack()
unsafe fn intel_dp_hdr_metadata_infoframe_sdp_pack(
    drm_infoframe: *const tk_drm_upstream::hdmi_types::HdmiDrmInfoframe,
    sdp: *mut tk_drm_upstream::drm_dp_types::DpSdp,
    size: usize,
) -> isize {
    use tk_drm_upstream::drm_dp_types::DpSdp;
    unsafe {
        let length = core::mem::size_of::<DpSdp>();
        if size < length {
            return -ENOSPC;
        }
        core::ptr::write_bytes(sdp.cast::<u8>(), 0, size);

        let mut buf = [0u8; HDMI_DRM_INFOFRAME_TOTAL];
        let len = tk_drm_upstream::hdmi_upstream::hdmi_drm_infoframe_pack_only(
            drm_infoframe,
            buf.as_mut_ptr().cast(),
            buf.len(),
        );
        if len < 0 {
            return -ENOSPC;
        }
        if len as usize != HDMI_DRM_INFOFRAME_TOTAL {
            return -ENOSPC;
        }

        let sdp = &mut *sdp;
        let frame = &*drm_infoframe;
        // Secondary-data packet ID 00h, then the non-audio infoframe type.
        sdp.sdp_header.HB0 = 0;
        sdp.sdp_header.HB1 = frame.r#type as u8;
        // Length of (data byte count - 1) and the DP SDP revision.
        sdp.sdp_header.HB2 = 0x1D;
        sdp.sdp_header.HB3 = 0x13 << 2;
        // CTA header: infoframe version and length, then the 26-byte payload.
        sdp.db[0] = frame.version;
        sdp.db[1] = frame.length;
        sdp.db[2..2 + 26].copy_from_slice(&buf[4..4 + 26]);

        // sizeof(dp_sdp_header) + 2 + HDMI_DRM_INFOFRAME_SIZE
        (4 + 2 + 26) as isize
    }
}

/// `intel_write_dp_sdp()`: pack one SDP and hand it to the port's infoframe writer.
// upstream: intel_dp.c intel_write_dp_sdp()
unsafe fn intel_write_dp_sdp(encoder: *mut IntelEncoder, crtc_state: *mut IntelCrtcState, sdp_type: u32) {
    use tk_drm_upstream::drm_dp_types::DpSdp;
    unsafe {
        let display = crate::intel_display_conversion_upstream::__drm_to_display((*encoder).base.dev);
        let dig_port = encoder.cast::<u8>().sub(core::mem::offset_of!(IntelDigitalPort, base)).cast::<IntelDigitalPort>();
        let mut sdp: DpSdp = core::mem::zeroed();
        let sdp_size = core::mem::size_of::<DpSdp>();

        if (*crtc_state).infoframes.enable & crate::intel_hdmi_upstream::intel_hdmi_infoframe_enable(sdp_type) == 0 {
            return;
        }

        let len: isize = match sdp_type {
            DP_SDP_VSC_TYPE => tk_drm_upstream::drm_dp_helper_upstream::drm_dp_vsc_sdp_pack(
                core::ptr::addr_of!((*crtc_state).infoframes.vsc),
                &mut sdp,
            ),
            HDMI_PACKET_TYPE_GAMUT_METADATA => intel_dp_hdr_metadata_infoframe_sdp_pack(
                core::ptr::addr_of!((*crtc_state).infoframes.drm.drm).cast(),
                &mut sdp,
                sdp_size,
            ),
            DP_SDP_ADAPTIVE_SYNC_TYPE => intel_dp_as_sdp_pack(
                core::ptr::addr_of!((*crtc_state).infoframes.as_sdp),
                &mut sdp,
                sdp_size,
            ),
            _ => return,
        };

        if drm_WARN_ON!((*display).drm, len < 0) {
            return;
        }

        let write = (*dig_port).write_infoframe.expect("write_infoframe");
        write(
            encoder,
            crtc_state,
            sdp_type,
            (&mut sdp as *mut DpSdp).cast(),
            len,
        );
    }
}

/// `intel_dp_set_infoframes()`: enable or disable the DIP packets of a DP port.
// upstream: intel_dp.c intel_dp_set_infoframes()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_set_infoframes(
    encoder: *mut IntelEncoder,
    enable: bool,
    crtc_state: *const IntelCrtcState,
    _conn_state: *const DrmConnectorState,
) {
    use crate::intel_de_upstream::{intel_de_posting_read, intel_de_read, intel_de_write};
    use crate::intel_display_regs_upstream::*;
    unsafe {
        let display = crate::intel_display_conversion_upstream::__drm_to_display((*encoder).base.dev);
        let reg = HSW_TVIDEO_DIP_CTL(display, (*crtc_state).cpu_transcoder as u32);
        let mut dip_enable = VIDEO_DIP_ENABLE_AVI_HSW
            | VIDEO_DIP_ENABLE_GCP_HSW
            | VIDEO_DIP_ENABLE_VS_HSW
            | VIDEO_DIP_ENABLE_GMP_HSW
            | VIDEO_DIP_ENABLE_SPD_HSW
            | VIDEO_DIP_ENABLE_DRM_GLK;

        // HAS_AS_SDP(display): DISPLAY_VER >= 13.
        let has_as_sdp = crate::intel_display_reg_defs_upstream::DISPLAY_VER(display) >= 13;
        if has_as_sdp {
            dip_enable |= VIDEO_DIP_ENABLE_AS_ADL;
        }

        let mut val = intel_de_read(display, reg) & !dip_enable;

        // TODO in Linux: sanitize DSC enabling wrt. intel_dsc_dp_pps_write().
        let has_dsc = (*display).info.__runtime_info.has_dsc;
        if !enable && has_dsc {
            val &= !VDIP_ENABLE_PPS;
        }

        // Disable VSC DIP when disabling SDP or when there is no PSR.
        if !enable || !(*crtc_state).has_psr {
            val &= !VIDEO_DIP_ENABLE_VSC_HSW;
        }

        intel_de_write(display, reg, val);
        intel_de_posting_read(display, reg);

        if !enable {
            return;
        }

        intel_write_dp_sdp(encoder, crtc_state as *mut IntelCrtcState, DP_SDP_VSC_TYPE);
        intel_write_dp_sdp(encoder, crtc_state as *mut IntelCrtcState, DP_SDP_ADAPTIVE_SYNC_TYPE);
        intel_write_dp_sdp(encoder, crtc_state as *mut IntelCrtcState, HDMI_PACKET_TYPE_GAMUT_METADATA);
    }
}

// ---- intel_dp_test.c: DP PHY compliance test ----

const DP_LINK_QUAL_PATTERN_DISABLE: u8 = 0;
const DP_LINK_QUAL_PATTERN_D10_2: u8 = 1;
const DP_LINK_QUAL_PATTERN_ERROR_RATE: u8 = 2;
const DP_LINK_QUAL_PATTERN_PRBS7: u8 = 3;
const DP_LINK_QUAL_PATTERN_80BIT_CUSTOM: u8 = 4;
const DP_LINK_QUAL_PATTERN_CP2520_PAT_1: u8 = 5;
const DP_LINK_QUAL_PATTERN_CP2520_PAT_3: u8 = 7;
/// `DP_TEST_LINK_PHY_TEST_PATTERN` (drm_dp.h).
const DP_TEST_LINK_PHY_TEST_PATTERN: u64 = 1 << 3;

/// `intel_dp_has_connector()`.
// upstream: intel_dp.c intel_dp_has_connector()
pub(crate) unsafe fn intel_dp_has_connector(
    intel_dp: *const IntelDp,
    conn_state: *const DrmConnectorState,
) -> bool {
    unsafe {
        let best = (*conn_state).best_encoder;
        if best.is_null() {
            return false;
        }
        let dig = dp_to_dig_port(intel_dp as *mut IntelDp);
        if best == core::ptr::addr_of_mut!((*dig).base.base) {
            return true;
        }
        for pipe in 0..crate::intel_display_limits_types_upstream::I915_MAX_PIPES as usize {
            let stream = (*intel_dp).mst.stream_encoders[pipe];
            if stream.is_null() {
                continue;
            }
            if best == core::ptr::addr_of_mut!((*stream).base.base) {
                return true;
            }
        }
        false
    }
}

/// `dp_tp_ctl_reg()` (intel_ddi.c).
// upstream: intel_ddi.c dp_tp_ctl_reg()
unsafe fn dp_tp_ctl_reg(
    encoder: *mut IntelEncoder,
    crtc_state: *const IntelCrtcState,
) -> crate::intel_workarounds_types_upstream::I915RegT {
    use crate::intel_display_regs_upstream::{DP_TP_CTL, TGL_DP_TP_CTL};
    unsafe {
        let display = crate::intel_display_conversion_upstream::__drm_to_display((*encoder).base.dev);
        if crate::intel_display_reg_defs_upstream::DISPLAY_VER(display) >= 12 {
            let transcoder = if crate::intel_atomic_upstream::intel_crtc_has_type(
                crtc_state,
                crate::intel_display_types_upstream::INTEL_OUTPUT_DP_MST,
            ) {
                (*crtc_state).mst_master_transcoder
            } else {
                (*crtc_state).cpu_transcoder
            };
            TGL_DP_TP_CTL(display, transcoder as u32)
        } else {
            DP_TP_CTL((*encoder).port as u32)
        }
    }
}

/// `intel_dp_phy_pattern_update()`: program the PHY test pattern requested by the sink.
// upstream: intel_dp_test.c intel_dp_phy_pattern_update()
unsafe fn intel_dp_phy_pattern_update(intel_dp: *mut IntelDp, crtc_state: *const IntelCrtcState) {
    use crate::intel_de_upstream::{intel_de_rmw, intel_de_write};
    use crate::intel_display_regs_upstream::*;
    unsafe {
        let dig = dp_to_dig_port(intel_dp);
        let display = crate::intel_display_conversion_upstream::__drm_to_display((*dig).base.base.dev);
        let data = &(*intel_dp).compliance.test_data.phytest;
        let crtc = (*crtc_state).uapi.crtc.cast::<IntelCrtc>();
        let encoder = core::ptr::addr_of_mut!((*dig).base);
        let pipe = (*crtc).pipe as u32;

        match data.phy_pattern {
            DP_LINK_QUAL_PATTERN_DISABLE => {
                intel_de_write(display, DDI_DP_COMP_CTL(pipe), 0x0);
                if crate::intel_display_reg_defs_upstream::DISPLAY_VER(display) >= 10 {
                    intel_de_rmw(
                        display,
                        dp_tp_ctl_reg(encoder, crtc_state),
                        DP_TP_CTL_TRAIN_PAT4_SEL_MASK | DP_TP_CTL_LINK_TRAIN_MASK,
                        DP_TP_CTL_LINK_TRAIN_NORMAL,
                    );
                }
            }
            DP_LINK_QUAL_PATTERN_D10_2 => {
                intel_de_write(display, DDI_DP_COMP_CTL(pipe), DDI_DP_COMP_CTL_ENABLE | DDI_DP_COMP_CTL_D10_2);
            }
            DP_LINK_QUAL_PATTERN_ERROR_RATE => {
                intel_de_write(
                    display,
                    DDI_DP_COMP_CTL(pipe),
                    DDI_DP_COMP_CTL_ENABLE | DDI_DP_COMP_CTL_SCRAMBLED_0,
                );
            }
            DP_LINK_QUAL_PATTERN_PRBS7 => {
                intel_de_write(display, DDI_DP_COMP_CTL(pipe), DDI_DP_COMP_CTL_ENABLE | DDI_DP_COMP_CTL_PRBS7);
            }
            DP_LINK_QUAL_PATTERN_80BIT_CUSTOM => {
                intel_de_write(display, DDI_DP_COMP_PAT(pipe, 0), 0x3e0f83e0);
                intel_de_write(display, DDI_DP_COMP_PAT(pipe, 1), 0x0f83e0f8);
                intel_de_write(display, DDI_DP_COMP_PAT(pipe, 2), 0x0000f83e);
                intel_de_write(
                    display,
                    DDI_DP_COMP_CTL(pipe),
                    DDI_DP_COMP_CTL_ENABLE | DDI_DP_COMP_CTL_CUSTOM80,
                );
            }
            DP_LINK_QUAL_PATTERN_CP2520_PAT_1 => {
                let pattern_val: u32 = 0xFB;
                intel_de_write(
                    display,
                    DDI_DP_COMP_CTL(pipe),
                    DDI_DP_COMP_CTL_ENABLE | DDI_DP_COMP_CTL_HBR2 | pattern_val,
                );
            }
            DP_LINK_QUAL_PATTERN_CP2520_PAT_3 => {
                if crate::intel_display_reg_defs_upstream::DISPLAY_VER(display) < 10 {
                    // "Platform does not support TPS4"
                    return;
                }
                intel_de_write(display, DDI_DP_COMP_CTL(pipe), 0x0);
                intel_de_rmw(
                    display,
                    dp_tp_ctl_reg(encoder, crtc_state),
                    DP_TP_CTL_TRAIN_PAT4_SEL_MASK | DP_TP_CTL_LINK_TRAIN_MASK,
                    DP_TP_CTL_TRAIN_PAT4_SEL_TP4A | DP_TP_CTL_LINK_TRAIN_PAT4,
                );
            }
            _ => {
                // "Invalid Phy Test Pattern"
            }
        }
    }
}

/// `intel_dp_process_phy_request()`: answer a PHY test request for one CRTC.
// upstream: intel_dp_test.c intel_dp_process_phy_request()
unsafe fn intel_dp_process_phy_request(intel_dp: *mut IntelDp, crtc_state: *const IntelCrtcState) {
    use tk_drm_upstream::drm_dp_helper_upstream::{
        drm_dp_dpcd_read_phy_link_status, drm_dp_dpcd_write, drm_dp_set_phy_test_pattern,
        DP_DPCD_REV, DP_PHY_DPRX,
    };
    unsafe {
        let mut link_status = [0u8; 6];
        if drm_dp_dpcd_read_phy_link_status(core::ptr::addr_of_mut!((*intel_dp).aux), DP_PHY_DPRX, link_status.as_mut_ptr()) < 0 {
            // "failed to get link status"
            return;
        }

        crate::intel_dp_link_training_upstream::intel_dp_get_adjust_train(
            intel_dp,
            crtc_state,
            DP_PHY_DPRX,
            link_status.as_ptr(),
        );
        crate::intel_dp_link_training_upstream::intel_dp_set_signal_levels(intel_dp, crtc_state, DP_PHY_DPRX);

        intel_dp_phy_pattern_update(intel_dp, crtc_state);

        drm_dp_dpcd_write(
            core::ptr::addr_of_mut!((*intel_dp).aux),
            0x103,
            (*intel_dp).train_set.as_mut_ptr().cast(),
            (*crtc_state).lane_count as usize,
        );

        let data = core::ptr::addr_of_mut!((*intel_dp).compliance.test_data.phytest);
        drm_dp_set_phy_test_pattern(
            core::ptr::addr_of_mut!((*intel_dp).aux),
            data,
            (*intel_dp).dpcd[DP_DPCD_REV as usize],
        );
    }
}

/// `intel_dp_prep_phy_test()`: lock the CRTCs of the connectors on this port
/// and collect the pipes that are active and not mid-commit.
// upstream: intel_dp_test.c intel_dp_prep_phy_test()
unsafe fn intel_dp_prep_phy_test(
    intel_dp: *mut IntelDp,
    ctx: *mut tk_drm_upstream::drm_modeset_lock_types::DrmModesetAcquireCtx,
    pipe_mask: *mut u8,
) -> i32 {
    use tk_drm_upstream::{
        drm_connector_upstream::{drm_connector_list_iter_begin, drm_connector_list_iter_end, drm_connector_list_iter_next},
        drm_connector_types::DrmConnectorListIter,
        drm_modeset_lock_upstream::drm_modeset_lock,
    };
    unsafe {
        let dig = dp_to_dig_port(intel_dp);
        let display = crate::intel_display_conversion_upstream::__drm_to_display((*dig).base.base.dev);
        let mut ret = 0;
        *pipe_mask = 0;

        let mut conn_iter = core::mem::zeroed::<DrmConnectorListIter>();
        drm_connector_list_iter_begin((*display).drm, &mut conn_iter);
        loop {
            let connector = drm_connector_list_iter_next(&mut conn_iter);
            if connector.is_null() {
                break;
            }
            let connector = connector.cast::<IntelConnector>();
            let conn_state = (*connector).base.state;

            if !intel_dp_has_connector(intel_dp, conn_state) {
                continue;
            }

            let crtc = (*conn_state).crtc.cast::<IntelCrtc>();
            if crtc.is_null() {
                continue;
            }

            ret = drm_modeset_lock(core::ptr::addr_of_mut!((*crtc).base.mutex), ctx);
            if ret != 0 {
                break;
            }

            let crtc_state = (*crtc).base.state.cast::<IntelCrtcState>();
            let bad = !crate::intel_dp_upstream::intel_crtc_has_dp_encoder_local(crtc_state);
            drm_WARN_ON!((*display).drm, bad);

            if !(*crtc_state).hw.active {
                continue;
            }

            let commit = (*conn_state).commit;
            if !commit.is_null() && !(*commit).hw_done.try_wait() {
                continue;
            }

            *pipe_mask |= 1u8 << (*crtc).pipe;
        }
        drm_connector_list_iter_end(&mut conn_iter);
        ret
    }
}

#[inline]
unsafe fn intel_crtc_has_dp_encoder_local(crtc_state: *const IntelCrtcState) -> bool {
    unsafe { crate::intel_atomic_upstream::intel_crtc_has_dp_encoder(crtc_state) }
}

/// `intel_dp_do_phy_test()`.
// upstream: intel_dp_test.c intel_dp_do_phy_test()
unsafe fn intel_dp_do_phy_test(
    encoder: *mut IntelEncoder,
    ctx: *mut tk_drm_upstream::drm_modeset_lock_types::DrmModesetAcquireCtx,
) -> i32 {
    use tk_drm_upstream::drm_modeset_lock_upstream::drm_modeset_lock;
    unsafe {
        let display = crate::intel_display_conversion_upstream::__drm_to_display((*encoder).base.dev);
        let intel_dp = crate::intel_dp_upstream::enc_to_intel_dp_local(encoder);

        let ret = drm_modeset_lock(
            core::ptr::addr_of_mut!((*(*display).drm).mode_config.connection_mutex),
            ctx,
        );
        if ret != 0 {
            return ret;
        }

        let mut pipe_mask: u8 = 0;
        let ret = intel_dp_prep_phy_test(intel_dp, ctx, &mut pipe_mask);
        if ret != 0 {
            return ret;
        }
        if pipe_mask == 0 {
            return 0;
        }

        for pipe in 0..crate::intel_display_limits_types_upstream::I915_MAX_PIPES as usize {
            if pipe_mask & (1 << pipe) == 0 {
                continue;
            }
            let crtc = crate::intel_crtc_upstream::intel_crtc_for_pipe(display, pipe as i32);
            let crtc_state = (*crtc).base.state.cast::<IntelCrtcState>();

            // Test on the MST master transcoder only.
            if crate::intel_display_reg_defs_upstream::DISPLAY_VER(display) >= 12
                && crate::intel_atomic_upstream::intel_crtc_has_type(
                    crtc_state,
                    crate::intel_display_types_upstream::INTEL_OUTPUT_DP_MST,
                )
            {
                // intel_dp_mst_is_master_trans(): MST is not ported; a DP_MST
                // CRTC cannot exist in this build, so the branch is unreachable.
                return -tk_linux_kapi::config::EOPNOTSUPP;
            }

            intel_dp_process_phy_request(intel_dp, crtc_state);
            break;
        }
        0
    }
}

/// `enc_to_intel_dp()` for the local digital-port layout.
#[inline]
unsafe fn enc_to_intel_dp_local(encoder: *mut IntelEncoder) -> *mut IntelDp {
    unsafe {
        encoder
            .cast::<u8>()
            .add(core::mem::offset_of!(IntelDigitalPort, dp))
            .sub(core::mem::offset_of!(IntelDigitalPort, base))
            .cast::<IntelDp>()
    }
}

/// `intel_dp_test_phy()`: run the PHY compliance test if the sink requested it.
// upstream: intel_dp_test.c intel_dp_test_phy()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_test_phy(intel_dp: *mut IntelDp) -> bool {
    use tk_drm_upstream::drm_modeset_lock_upstream::{drm_modeset_acquire_fini, drm_modeset_acquire_init, drm_modeset_backoff, drm_modeset_drop_locks};
    unsafe {
        if !(*intel_dp).compliance.test_active
            || (*intel_dp).compliance.test_type != DP_TEST_LINK_PHY_TEST_PATTERN
        {
            return false;
        }

        let dig = dp_to_dig_port(intel_dp);
        let encoder = core::ptr::addr_of_mut!((*dig).base);
        let mut ctx = core::mem::zeroed::<tk_drm_upstream::drm_modeset_lock_types::DrmModesetAcquireCtx>();

        drm_modeset_acquire_init(&mut ctx, 0);
        let ret = loop {
            let ret = intel_dp_do_phy_test(encoder, &mut ctx);
            if ret == -tk_linux_kapi::config::EDEADLK {
                drm_modeset_backoff(&mut ctx);
                continue;
            }
            break ret;
        };
        drm_modeset_drop_locks(&mut ctx);
        drm_modeset_acquire_fini(&mut ctx);

        if ret != 0 {
            tk_linux_kapi::drm_warn!((*encoder).base.dev, "Acquiring modeset locks failed with {}", ret);
        }
        true
    }
}
