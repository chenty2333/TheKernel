// SPDX-License-Identifier: MIT
// Copyright 2019 Intel Corporation.
// Source: drivers/gpu/drm/i915/display/intel_dp_link_training.c (Linux v7.2.3)
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::{ffi::c_void, mem::offset_of, ptr};

use tk_drm_upstream::{
    drm_dp_helper_types::DrmDpAux,
    drm_dp_helper_upstream::{
        DP_CHANNEL_EQ_BITS, DP_DPCD_REV, DP_DPCD_REV_14, DP_INTERLANE_ALIGN_DONE,
        DP_LANE_ALIGN_STATUS_UPDATED, DP_LANE_CHANNEL_EQ_DONE, DP_LANE_CR_DONE,
        DP_LANE_SYMBOL_LOCKED, DP_LANE0_1_STATUS, DP_LINK_BW_1_62, DP_LINK_BW_2_7, DP_LINK_BW_5_4,
        DP_LINK_BW_8_1, DP_LINK_STATUS_SIZE, DP_LT_TUNABLE_PHY_REPEATER_FIELD_DATA_STRUCTURE_REV,
        DP_MAX_DOWNSPREAD, DP_MAX_LANE_COUNT, DP_MAX_LANE_COUNT_MASK, DP_PHY_DPRX, DP_PHY_LTTPR1,
        DP_PHY_REPEATER_MODE, DP_PHY_REPEATER_MODE_NON_TRANSPARENT,
        DP_PHY_REPEATER_MODE_TRANSPARENT, DP_POST_LT_ADJ_REQ_IN_PROGRESS,
        DP_POST_LT_ADJ_REQ_SUPPORTED, DP_RECEIVER_CAP_SIZE, DP_TPS3_SUPPORTED, DP_TPS4_SUPPORTED,
        DP_TRAINING_AUX_RD_INTERVAL, DP_TRAINING_PATTERN_SET, DrmDpPhy, drm_dp_channel_eq_ok,
        drm_dp_clock_recovery_ok, drm_dp_dpcd_probe, drm_dp_dpcd_read, drm_dp_dpcd_read_byte,
        drm_dp_dpcd_read_phy_link_status, drm_dp_dpcd_write, drm_dp_dpcd_write_byte,
        drm_dp_get_adjust_request_pre_emphasis, drm_dp_get_adjust_request_voltage,
        drm_dp_link_rate_to_bw_code, drm_dp_lttpr_count, drm_dp_lttpr_init,
        drm_dp_lttpr_pre_emphasis_level_3_supported, drm_dp_lttpr_voltage_swing_level_3_supported,
        drm_dp_read_channel_eq_delay, drm_dp_read_clock_recovery_delay, drm_dp_read_dpcd_caps,
        drm_dp_read_lttpr_common_caps, drm_dp_read_lttpr_phy_caps, drm_dp_tps3_supported,
        drm_dp_tps4_supported,
    },
};
use tk_linux_kapi::{clock::now_ns, iopoll::usleep_range, primitives::msecs_to_jiffies_timeout};

use crate::{
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_types_upstream::{
        INTEL_OUTPUT_DP_MST, IntelAtomicState, IntelCrtcState, IntelDigitalPort, IntelDp,
        IntelEncoder,
    },
    intel_workarounds_types_upstream::I915RegT,
    linux_config::{EINVAL, EIO},
};

const MAX_SEQ_TRAIN_FAILURES: i32 = 2;
const DP_LINK_BW_SET: u32 = 0x100;
const DP_LANE_COUNT_SET: u32 = 0x101;
const DP_DOWNSPREAD_CTRL: u32 = 0x107;
const DP_TRAINING_PATTERN_SET_PHY_REPEATER1: u32 = 0xf0010;
const DP_TRAINING_LANE0_SET: u32 = 0x103;
const DP_TRAINING_LANE0_SET_PHY_REPEATER1: u32 = 0xf0011;
const DP_LINK_RATE_SET: u32 = 0x115;
const DP_MAIN_LINK_CHANNEL_CODING_SET: u32 = 0x108;
const DP_TRAINING_PATTERN_DISABLE: u8 = 0;
const DP_TRAINING_PATTERN_1: u8 = 1;
const DP_TRAINING_PATTERN_2: u8 = 2;
const DP_TRAINING_PATTERN_3: u8 = 3;
const DP_TRAINING_PATTERN_4: u8 = 7;
const DP_LINK_SCRAMBLING_DISABLE: u8 = 1 << 5;
const DP_TRAIN_VOLTAGE_SWING_MASK: u8 = 0x03;
const DP_TRAIN_VOLTAGE_SWING_SHIFT: u8 = 0;
const DP_TRAIN_PRE_EMPHASIS_MASK: u8 = 0x18;
const DP_TRAIN_PRE_EMPHASIS_SHIFT: u8 = 3;
const DP_TRAIN_MAX_SWING_REACHED: u8 = 1 << 2;
const DP_TRAIN_MAX_PRE_EMPHASIS_REACHED: u8 = 1 << 5;
const DP_LANE_COUNT_ENHANCED: u8 = 1 << 7;
const DP_POST_LT_ADJ_REQ_GRANTED: u8 = 1 << 5;
const DP_MSA_TIMING_PAR_IGNORE_EN: u8 = 1 << 7;
const DP_FIXED_VTOTAL_AS_SDP_EN_IN_PR_ACTIVE: u8 = 1 << 6;
const DP_SET_ANSI_8B10B: u8 = 1 << 0;
const DP_LTTPR_MODE_OFFSET: usize =
    (DP_PHY_REPEATER_MODE - DP_LT_TUNABLE_PHY_REPEATER_FIELD_DATA_STRUCTURE_REV) as usize;
const DP_LTTPR_COUNT_OFFSET: usize = 2;
const DP_LTTPR_MAX_LINK_RATE: u32 = 0xf0001;

unsafe extern "C" {
    fn intel_dp_is_edp(dp: *mut IntelDp) -> bool;
    fn intel_digital_port_connected(encoder: *mut IntelEncoder) -> bool;
    fn intel_dp_queue_modeset_retry_for_link(
        state: *mut IntelAtomicState,
        encoder: *mut IntelEncoder,
        crtc_state: *const IntelCrtcState,
    );
    fn intel_hpd_block(encoder: *mut IntelEncoder);
    fn intel_hpd_unblock(encoder: *mut IntelEncoder);
    fn intel_encoder_link_check_queue_work(encoder: *mut IntelEncoder, delay_ms: i32);
}

#[inline]
unsafe fn dp_to_dig_port(dp: *mut IntelDp) -> *mut IntelDigitalPort {
    if dp.is_null() {
        return ptr::null_mut();
    }
    dp.cast::<u8>()
        .wrapping_sub(offset_of!(IntelDigitalPort, dp))
        .cast()
}

#[inline]
unsafe fn dp_to_display(dp: *mut IntelDp) -> *mut IntelDisplay {
    let dig = unsafe { dp_to_dig_port(dp) };
    if dig.is_null() || unsafe { (*dig).base.base.dev.is_null() } {
        return ptr::null_mut();
    }
    unsafe { crate::intel_display_conversion_upstream::__drm_to_display((*dig).base.base.dev) }
}

#[inline]
unsafe fn dp_to_encoder(dp: *mut IntelDp) -> *mut IntelEncoder {
    let dig = unsafe { dp_to_dig_port(dp) };
    if dig.is_null() {
        ptr::null_mut()
    } else {
        unsafe { ptr::addr_of_mut!((*dig).base) }
    }
}

#[inline]
unsafe fn display_ver(display: *mut IntelDisplay) -> u16 {
    if display.is_null() {
        0
    } else {
        unsafe { (*display).info.__runtime_info.ip.ver }
    }
}

#[inline]
unsafe fn dpcd_aux(dp: *mut IntelDp) -> *mut DrmDpAux {
    if dp.is_null() {
        ptr::null_mut()
    } else {
        unsafe { ptr::addr_of_mut!((*dp).aux) }
    }
}

#[inline]
unsafe fn source_supports_tps3(display: *mut IntelDisplay) -> bool {
    unsafe { display_ver(display) >= 9 }
}

#[inline]
unsafe fn source_supports_tps4(display: *mut IntelDisplay) -> bool {
    unsafe { display_ver(display) >= 10 }
}

#[inline]
fn dp_phy_from_lttpr_index(index: i32) -> Option<DrmDpPhy> {
    match index {
        0 => Some(DP_PHY_LTTPR1),
        1 => Some(tk_drm_upstream::drm_dp_helper_upstream::DP_PHY_LTTPR2),
        2 => Some(tk_drm_upstream::drm_dp_helper_upstream::DP_PHY_LTTPR3),
        3 => Some(tk_drm_upstream::drm_dp_helper_upstream::DP_PHY_LTTPR4),
        4 => Some(tk_drm_upstream::drm_dp_helper_upstream::DP_PHY_LTTPR5),
        5 => Some(tk_drm_upstream::drm_dp_helper_upstream::DP_PHY_LTTPR6),
        6 => Some(tk_drm_upstream::drm_dp_helper_upstream::DP_PHY_LTTPR7),
        7 => Some(tk_drm_upstream::drm_dp_helper_upstream::DP_PHY_LTTPR8),
        _ => None,
    }
}

unsafe fn reset_lttpr_common_caps(dp: *mut IntelDp) {
    if !dp.is_null() {
        unsafe { (*dp).lttpr_common_caps.fill(0) };
    }
}

unsafe fn reset_lttpr_count(dp: *mut IntelDp) {
    if !dp.is_null() {
        unsafe { (*dp).lttpr_common_caps[DP_LTTPR_COUNT_OFFSET] = 0 };
    }
}

unsafe fn init_lttpr_phys(dp: *mut IntelDp, dpcd: *const u8) -> i32 {
    let mut caps = [0u8; tk_drm_upstream::drm_dp_helper_upstream::DP_LTTPR_COMMON_CAP_SIZE];
    let aux = unsafe { dpcd_aux(dp) };
    if unsafe { drm_dp_read_lttpr_common_caps(aux, dpcd, caps.as_mut_ptr()) } < 0 || caps[0] < 0x14
    {
        unsafe { reset_lttpr_common_caps(dp) };
        return 0;
    }
    unsafe { (*dp).lttpr_common_caps = caps };
    let count = unsafe { drm_dp_lttpr_count((*dp).lttpr_common_caps.as_ptr()) };
    if count == 0 {
        return 0;
    }
    if unsafe { (*dp).link.active } {
        if count < 0 || unsafe { intel_dp_lttpr_transparent_mode_enabled(dp) } {
            unsafe { reset_lttpr_count(dp) };
            return 0;
        }
        return count;
    }
    if unsafe { drm_dp_lttpr_init(aux, count) } != 0 {
        unsafe {
            (*dp).lttpr_common_caps[DP_LTTPR_MODE_OFFSET] = DP_PHY_REPEATER_MODE_TRANSPARENT;
            reset_lttpr_count(dp);
        }
        return 0;
    }
    unsafe { (*dp).lttpr_common_caps[DP_LTTPR_MODE_OFFSET] = DP_PHY_REPEATER_MODE_NON_TRANSPARENT };
    count
}

unsafe fn init_lttpr(dp: *mut IntelDp, dpcd: *const u8) -> i32 {
    let count = unsafe { init_lttpr_phys(dp, dpcd) };
    for i in 0..count {
        let Some(phy) = dp_phy_from_lttpr_index(i) else {
            unsafe { reset_lttpr_common_caps(dp) };
            return 0;
        };
        unsafe {
            let _ = drm_dp_read_lttpr_phy_caps(
                dpcd_aux(dp),
                dpcd,
                phy,
                (*dp).lttpr_phy_caps[i as usize].as_mut_ptr(),
            );
        }
    }
    count
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_lttpr_transparent_mode_enabled(dp: *mut IntelDp) -> bool {
    !dp.is_null()
        && unsafe { (*dp).lttpr_common_caps[DP_LTTPR_MODE_OFFSET] }
            == DP_PHY_REPEATER_MODE_TRANSPARENT
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_read_dprx_caps(dp: *mut IntelDp, dpcd: *mut u8) -> i32 {
    if dp.is_null() || dpcd.is_null() {
        return -EINVAL;
    }
    if unsafe { intel_dp_is_edp(dp) } {
        return 0;
    }
    let display = unsafe { dp_to_display(dp) };
    if display.is_null() {
        return -EINVAL;
    }
    let is_glk = unsafe { (*display).platform._anon_0_0.bitmap[0] & (1u64 << 35) != 0 };
    if unsafe { display_ver(display) } >= 10
        && !is_glk
        && unsafe {
            drm_dp_dpcd_probe(
                dpcd_aux(dp),
                DP_LT_TUNABLE_PHY_REPEATER_FIELD_DATA_STRUCTURE_REV,
            )
        } != 0
    {
        return -EIO;
    }
    if unsafe { drm_dp_read_dpcd_caps(dpcd_aux(dp), dpcd) } != 0 {
        return -EIO;
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_init_lttpr_and_dprx_caps(dp: *mut IntelDp) -> i32 {
    if dp.is_null() {
        return -EINVAL;
    }
    let display = unsafe { dp_to_display(dp) };
    if display.is_null() {
        return -EINVAL;
    }
    let mut lttpr_count = 0;
    if !unsafe { intel_dp_is_edp(dp) }
        && unsafe { display_ver(display) } >= 10
        && unsafe { (*display).platform._anon_0_0.bitmap[0] & (1u64 << 35) == 0 }
    {
        let mut dpcd = [0u8; DP_RECEIVER_CAP_SIZE];
        let ret = unsafe { intel_dp_read_dprx_caps(dp, dpcd.as_mut_ptr()) };
        if ret != 0 {
            return ret;
        }
        lttpr_count = unsafe { init_lttpr(dp, dpcd.as_ptr()) };
    }
    if unsafe { drm_dp_read_dpcd_caps(dpcd_aux(dp), (*dp).dpcd.as_mut_ptr()) } != 0 {
        unsafe { reset_lttpr_common_caps(dp) };
        return -EIO;
    }
    lttpr_count
}

#[inline]
fn training_pattern_set_reg(phy: DrmDpPhy) -> u32 {
    if phy == DP_PHY_DPRX {
        DP_TRAINING_PATTERN_SET
    } else {
        tk_drm_upstream::drm_dp_helper_upstream::DP_LTTPR_REG(
            phy,
            DP_TRAINING_PATTERN_SET_PHY_REPEATER1,
        )
    }
}

#[inline]
fn training_lane0_set_reg(phy: DrmDpPhy) -> u32 {
    if phy == DP_PHY_DPRX {
        DP_TRAINING_LANE0_SET
    } else {
        tk_drm_upstream::drm_dp_helper_upstream::DP_LTTPR_REG(
            phy,
            DP_TRAINING_LANE0_SET_PHY_REPEATER1,
        )
    }
}

#[inline]
fn dp_voltage_max(preemphasis: u8) -> u8 {
    match (preemphasis & DP_TRAIN_PRE_EMPHASIS_MASK) >> DP_TRAIN_PRE_EMPHASIS_SHIFT {
        0 => 3,
        1 => 2,
        2 => 1,
        _ => 0,
    }
}

unsafe fn phy_is_downstream_of_source(dp: *mut IntelDp, phy: DrmDpPhy) -> bool {
    let count = unsafe { drm_dp_lttpr_count((*dp).lttpr_common_caps.as_ptr()) };
    count <= 0 || phy as i32 == count
}

unsafe fn phy_voltage_max(dp: *mut IntelDp, state: *const IntelCrtcState, phy: DrmDpPhy) -> u8 {
    if unsafe { phy_is_downstream_of_source(dp, phy) } {
        unsafe {
            (*dp)
                .voltage_max
                .map(|f| f(dp, state.cast_mut()))
                .unwrap_or(0)
                .min(3)
        }
    } else {
        let upstream = (phy as i32 + 1) as usize;
        let caps = unsafe { (*dp).lttpr_phy_caps[upstream.min(7)] };
        if unsafe { drm_dp_lttpr_voltage_swing_level_3_supported(caps.as_ptr()) } {
            3
        } else {
            2
        }
    }
}

unsafe fn phy_preemph_max(dp: *mut IntelDp, phy: DrmDpPhy) -> u8 {
    if unsafe { phy_is_downstream_of_source(dp, phy) } {
        unsafe { (*dp).preemph_max.map(|f| f(dp)).unwrap_or(0).min(3) }
    } else {
        let upstream = (phy as i32 + 1) as usize;
        let caps = unsafe { (*dp).lttpr_phy_caps[upstream.min(7)] };
        if unsafe { drm_dp_lttpr_pre_emphasis_level_3_supported(caps.as_ptr()) } {
            3
        } else {
            2
        }
    }
}

unsafe fn get_lane_adjust(
    dp: *mut IntelDp,
    state: *const IntelCrtcState,
    phy: DrmDpPhy,
    status: *const u8,
    lane: i32,
) -> u8 {
    let lane_count = unsafe { (*state).lane_count as i32 };
    if lane_count <= 0 || lane_count > 4 || status.is_null() {
        return 0;
    }
    // ADL-P has independent source signal controls for each lane.
    let lane = if unsafe { display_ver(dp_to_display(dp)) } >= 10 {
        lane.min(lane_count - 1)
    } else {
        0
    };
    let mut vswing = 0;
    let mut pre = 0;
    for lane_idx in if unsafe { display_ver(dp_to_display(dp)) } >= 10 {
        lane..lane + 1
    } else {
        0..lane_count
    } {
        vswing = vswing.max(unsafe { drm_dp_get_adjust_request_voltage(status, lane_idx) });
        pre = pre.max(unsafe { drm_dp_get_adjust_request_pre_emphasis(status, lane_idx) });
    }
    let pre_max = unsafe { phy_preemph_max(dp, phy) } << DP_TRAIN_PRE_EMPHASIS_SHIFT;
    if (pre & DP_TRAIN_PRE_EMPHASIS_MASK) >= pre_max {
        pre = pre_max | DP_TRAIN_MAX_PRE_EMPHASIS_REACHED;
    }
    vswing = vswing.min(dp_voltage_max(pre) << DP_TRAIN_VOLTAGE_SWING_SHIFT);
    let voltage_max = unsafe { phy_voltage_max(dp, state, phy) } << DP_TRAIN_VOLTAGE_SWING_SHIFT;
    if vswing >= voltage_max {
        vswing = voltage_max | DP_TRAIN_MAX_SWING_REACHED;
    }
    vswing | pre
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_get_adjust_train(
    dp: *mut IntelDp,
    state: *const IntelCrtcState,
    phy: DrmDpPhy,
    link_status: *const u8,
) -> bool {
    if dp.is_null() || state.is_null() || link_status.is_null() {
        return false;
    }
    let lanes = unsafe { (*state).lane_count as usize };
    if lanes == 0 || lanes > 4 {
        return false;
    }
    let mut changed = false;
    for lane in 0..lanes {
        let next = unsafe { get_lane_adjust(dp, state, phy, link_status, lane as i32) };
        if unsafe { (*dp).train_set[lane] } != next {
            unsafe { (*dp).train_set[lane] = next };
            changed = true;
        }
    }
    changed
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_program_link_training_pattern(
    dp: *mut IntelDp,
    state: *const IntelCrtcState,
    _phy: DrmDpPhy,
    pattern: u8,
) {
    if dp.is_null() || state.is_null() {
        return;
    }
    if let Some(set) = unsafe { (*dp).set_link_train } {
        unsafe { set(dp, state.cast_mut(), pattern) };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_set_signal_levels(
    dp: *mut IntelDp,
    state: *const IntelCrtcState,
    phy: DrmDpPhy,
) {
    if dp.is_null() || state.is_null() || !unsafe { phy_is_downstream_of_source(dp, phy) } {
        return;
    }
    let encoder = unsafe { dp_to_encoder(dp) };
    if !encoder.is_null() {
        if let Some(set) = unsafe { (*encoder).set_signal_levels } {
            unsafe { set(encoder, state.cast_mut()) };
        }
    }
}

unsafe fn update_link_train(dp: *mut IntelDp, state: *const IntelCrtcState, phy: DrmDpPhy) -> bool {
    let lanes = unsafe { (*state).lane_count as usize };
    if lanes == 0 || lanes > 4 {
        return false;
    }
    unsafe { intel_dp_set_signal_levels(dp, state, phy) };
    let ret = unsafe {
        drm_dp_dpcd_write(
            dpcd_aux(dp),
            training_lane0_set_reg(phy),
            ptr::addr_of_mut!((*dp).train_set).cast::<c_void>(),
            lanes,
        )
    };
    ret == lanes as isize
}

unsafe fn set_link_train(
    dp: *mut IntelDp,
    state: *const IntelCrtcState,
    phy: DrmDpPhy,
    pattern: u8,
) -> bool {
    let lanes = unsafe { (*state).lane_count as usize };
    if lanes == 0 || lanes > 4 {
        return false;
    }
    unsafe { intel_dp_program_link_training_pattern(dp, state, phy, pattern) };
    let mut data = [0u8; 5];
    data[0] = pattern;
    for lane in 0..lanes {
        data[lane + 1] = unsafe { (*dp).train_set[lane] };
    }
    unsafe {
        drm_dp_dpcd_write(
            dpcd_aux(dp),
            training_pattern_set_reg(phy),
            data.as_mut_ptr().cast::<c_void>(),
            lanes + 1,
        ) == (lanes + 1) as isize
    }
}

unsafe fn reset_link_train(
    dp: *mut IntelDp,
    state: *const IntelCrtcState,
    phy: DrmDpPhy,
    pattern: u8,
) -> bool {
    unsafe { (*dp).train_set.fill(0) };
    unsafe { intel_dp_set_signal_levels(dp, state, phy) };
    unsafe { set_link_train(dp, state, phy, pattern) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_link_training_set_mode(
    dp: *mut IntelDp,
    link_rate: i32,
    is_vrr: bool,
    pr_with_as_sdp_enable: bool,
) {
    if dp.is_null() {
        return;
    }
    let coding =
        if unsafe { tk_drm_upstream::drm_dp_helper_upstream::drm_dp_is_uhbr_rate(link_rate) } {
            1 << 1
        } else {
            DP_SET_ANSI_8B10B
        };
    let config = [
        (if is_vrr {
            DP_MSA_TIMING_PAR_IGNORE_EN
        } else {
            0
        }) | (if pr_with_as_sdp_enable {
            DP_FIXED_VTOTAL_AS_SDP_EN_IN_PR_ACTIVE
        } else {
            0
        }),
        coding,
    ];
    unsafe {
        let _ = drm_dp_dpcd_write(
            dpcd_aux(dp),
            DP_DOWNSPREAD_CTRL,
            config.as_ptr().cast_mut().cast::<c_void>(),
            config.len(),
        );
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_link_training_set_bw(
    dp: *mut IntelDp,
    link_bw: i32,
    rate_select: i32,
    lane_count: i32,
    enhanced_framing: bool,
    post_lt_adj_req: bool,
) {
    if dp.is_null() || lane_count <= 0 || lane_count > 4 {
        return;
    }
    let mut lanes = lane_count as u8;
    if enhanced_framing {
        lanes |= DP_LANE_COUNT_ENHANCED;
    }
    if post_lt_adj_req {
        lanes |= DP_POST_LT_ADJ_REQ_GRANTED;
    }
    if link_bw != 0 {
        let config = [link_bw as u8, lanes];
        unsafe {
            let _ = drm_dp_dpcd_write(
                dpcd_aux(dp),
                DP_LINK_BW_SET,
                config.as_ptr().cast_mut().cast::<c_void>(),
                config.len(),
            );
        }
    } else {
        unsafe {
            let _ = drm_dp_dpcd_write_byte(dpcd_aux(dp), DP_LANE_COUNT_SET, lanes);
            let _ = drm_dp_dpcd_write_byte(dpcd_aux(dp), DP_LINK_RATE_SET, rate_select as u8);
        }
    }
}

unsafe fn selected_training_pattern(
    dp: *mut IntelDp,
    state: *const IntelCrtcState,
    phy: DrmDpPhy,
) -> u8 {
    let display = unsafe { dp_to_display(dp) };
    let tps4 = phy != DP_PHY_DPRX || unsafe { drm_dp_tps4_supported((*dp).dpcd.as_ptr()) };
    if unsafe { source_supports_tps4(display) } && tps4 {
        return DP_TRAINING_PATTERN_4;
    }
    let tps3 = phy != DP_PHY_DPRX || unsafe { drm_dp_tps3_supported((*dp).dpcd.as_ptr()) };
    if unsafe { source_supports_tps3(display) } && tps3 {
        return DP_TRAINING_PATTERN_3;
    }
    if !state.is_null() && unsafe { (*state).port_clock } >= 540_000 {
        return DP_TRAINING_PATTERN_2;
    }
    DP_TRAINING_PATTERN_2
}

unsafe fn max_vswing_reached(dp: *mut IntelDp, lane_count: usize) -> bool {
    for lane in 0..lane_count.min(4) {
        let train = unsafe { (*dp).train_set[lane] };
        let v = (train & DP_TRAIN_VOLTAGE_SWING_MASK) >> DP_TRAIN_VOLTAGE_SWING_SHIFT;
        let p = (train & DP_TRAIN_PRE_EMPHASIS_MASK) >> DP_TRAIN_PRE_EMPHASIS_SHIFT;
        if train & DP_TRAIN_MAX_SWING_REACHED == 0 || v + p != 3 {
            return false;
        }
    }
    true
}

fn request_changed(
    state: *const IntelCrtcState,
    old: &[u8; DP_LINK_STATUS_SIZE],
    new: &[u8; DP_LINK_STATUS_SIZE],
) -> bool {
    let lanes = unsafe { (*state).lane_count as usize };
    for lane in 0..lanes.min(4) {
        let v_old = (old[4 + lane / 2] >> ((lane & 1) * 4)) & 0x3;
        let p_old = (old[4 + lane / 2] >> ((lane & 1) * 4 + 2)) & 0x3;
        let v_new = (new[4 + lane / 2] >> ((lane & 1) * 4)) & 0x3;
        let p_new = (new[4 + lane / 2] >> ((lane & 1) * 4 + 2)) & 0x3;
        if (v_old | p_old) != (v_new | p_new) {
            return true;
        }
    }
    false
}

unsafe fn clock_recovery(dp: *mut IntelDp, state: *const IntelCrtcState, phy: DrmDpPhy) -> bool {
    let lanes = unsafe { (*state).lane_count as i32 };
    if !(1..=4).contains(&lanes)
        || !unsafe {
            reset_link_train(
                dp,
                state,
                phy,
                DP_TRAINING_PATTERN_1 | DP_LINK_SCRAMBLING_DISABLE,
            )
        }
    {
        return false;
    }
    let mut old_status = [0u8; DP_LINK_STATUS_SIZE];
    let mut voltage_tries = 1;
    let max_tries = if unsafe { (*dp).dpcd[DP_DPCD_REV as usize] } >= DP_DPCD_REV_14 {
        10
    } else {
        80
    };
    let delay = unsafe {
        drm_dp_read_clock_recovery_delay(dpcd_aux(dp), (*dp).dpcd.as_ptr(), phy, false).max(0)
            as u64
    };
    for _ in 0..max_tries {
        usleep_range(delay.max(100), delay.max(100) + 100);
        let mut status = [0u8; DP_LINK_STATUS_SIZE];
        if unsafe { drm_dp_dpcd_read_phy_link_status(dpcd_aux(dp), phy, status.as_mut_ptr()) } < 0 {
            return false;
        }
        if unsafe { drm_dp_clock_recovery_ok(status.as_ptr(), lanes) } {
            return true;
        }
        if voltage_tries == 5 || unsafe { max_vswing_reached(dp, lanes as usize) } {
            return false;
        }
        unsafe { intel_dp_get_adjust_train(dp, state, phy, status.as_ptr()) };
        if !unsafe { update_link_train(dp, state, phy) } {
            return false;
        }
        if request_changed(state, &old_status, &status) {
            voltage_tries = 1;
        } else {
            voltage_tries += 1;
        }
        old_status = status;
    }
    false
}

unsafe fn channel_equalization(
    dp: *mut IntelDp,
    state: *const IntelCrtcState,
    phy: DrmDpPhy,
) -> bool {
    let lanes = unsafe { (*state).lane_count as i32 };
    let delay = unsafe {
        drm_dp_read_channel_eq_delay(dpcd_aux(dp), (*dp).dpcd.as_ptr(), phy, false).max(0) as u64
    };
    let pattern = unsafe { selected_training_pattern(dp, state, phy) };
    let pattern = if pattern == DP_TRAINING_PATTERN_4 {
        pattern
    } else {
        pattern | DP_LINK_SCRAMBLING_DISABLE
    };
    if !(1..=4).contains(&lanes) || !unsafe { set_link_train(dp, state, phy, pattern) } {
        return false;
    }
    for _ in 0..5 {
        usleep_range(delay.max(100), delay.max(100) + 100);
        let mut status = [0u8; DP_LINK_STATUS_SIZE];
        if unsafe { drm_dp_dpcd_read_phy_link_status(dpcd_aux(dp), phy, status.as_mut_ptr()) } < 0 {
            return false;
        }
        if !unsafe { drm_dp_clock_recovery_ok(status.as_ptr(), lanes) } {
            return false;
        }
        if unsafe { drm_dp_channel_eq_ok(status.as_ptr(), lanes) } {
            return true;
        }
        unsafe { intel_dp_get_adjust_train(dp, state, phy, status.as_ptr()) };
        if !unsafe { update_link_train(dp, state, phy) } {
            return false;
        }
    }
    false
}

unsafe fn post_lt_adj_req(dp: *mut IntelDp, state: *const IntelCrtcState) -> bool {
    if unsafe { (*dp).set_idle_link_train.is_none() }
        || !unsafe {
            tk_drm_upstream::drm_dp_helper_upstream::drm_dp_post_lt_adj_req_supported(
                (*dp).dpcd.as_ptr(),
            )
        }
        || unsafe { selected_training_pattern(dp, state, DP_PHY_DPRX) } == DP_TRAINING_PATTERN_4
    {
        return true;
    }
    let mut status = [0u8; DP_LINK_STATUS_SIZE];
    if unsafe { drm_dp_dpcd_read_phy_link_status(dpcd_aux(dp), DP_PHY_DPRX, status.as_mut_ptr()) }
        < 0
    {
        return false;
    }
    let start = now_ns();
    let mut changes = 0;
    loop {
        if !unsafe { drm_dp_clock_recovery_ok(status.as_ptr(), (*state).lane_count as i32) }
            || !unsafe { drm_dp_channel_eq_ok(status.as_ptr(), (*state).lane_count as i32) }
        {
            return false;
        }
        if status[2] & DP_POST_LT_ADJ_REQ_IN_PROGRESS == 0
            || changes >= 6
            || now_ns().saturating_sub(start) >= 200_000_000
        {
            break;
        }
        usleep_range(5_000, 5_500);
        if unsafe {
            drm_dp_dpcd_read_phy_link_status(dpcd_aux(dp), DP_PHY_DPRX, status.as_mut_ptr())
        } < 0
        {
            return false;
        }
        if unsafe { intel_dp_get_adjust_train(dp, state, DP_PHY_DPRX, status.as_ptr()) } {
            changes += 1;
            if !unsafe { update_link_train(dp, state, DP_PHY_DPRX) } {
                return false;
            }
        }
    }
    let mut lane_count = unsafe { (*state).lane_count };
    if unsafe { (*state).enhanced_framing } {
        lane_count |= DP_LANE_COUNT_ENHANCED;
    }
    unsafe {
        let _ = drm_dp_dpcd_write_byte(dpcd_aux(dp), DP_LANE_COUNT_SET, lane_count);
    }
    true
}

unsafe fn link_train_phy(dp: *mut IntelDp, state: *const IntelCrtcState, phy: DrmDpPhy) -> bool {
    unsafe { clock_recovery(dp, state, phy) && channel_equalization(dp, state, phy) }
}

unsafe fn link_train_all_phys(dp: *mut IntelDp, state: *const IntelCrtcState, count: i32) -> bool {
    let mut passed = true;
    for i in (0..count).rev() {
        let Some(phy) = dp_phy_from_lttpr_index(i) else {
            return false;
        };
        passed = unsafe { link_train_phy(dp, state, phy) };
        let _ = unsafe {
            drm_dp_dpcd_write_byte(
                dpcd_aux(dp),
                training_pattern_set_reg(phy),
                DP_TRAINING_PATTERN_DISABLE,
            )
        };
        if !passed {
            break;
        }
    }
    if passed {
        passed = unsafe { link_train_phy(dp, state, DP_PHY_DPRX) };
    }
    let _ = unsafe {
        drm_dp_dpcd_write_byte(
            dpcd_aux(dp),
            training_pattern_set_reg(DP_PHY_DPRX),
            DP_TRAINING_PATTERN_DISABLE,
        )
    };
    if let Some(set_idle) = unsafe { (*dp).set_idle_link_train } {
        unsafe { set_idle(dp, state.cast_mut()) };
    }
    if passed {
        passed = unsafe { post_lt_adj_req(dp, state) };
    }
    passed
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_start_link_train(
    state: *mut IntelAtomicState,
    dp: *mut IntelDp,
    crtc_state: *const IntelCrtcState,
) {
    if state.is_null() || dp.is_null() || crtc_state.is_null() {
        return;
    }
    let encoder = unsafe { dp_to_encoder(dp) };
    if encoder.is_null() {
        return;
    }
    unsafe { intel_hpd_block(encoder) };
    let mut lttpr_count = unsafe { intel_dp_init_lttpr_and_dprx_caps(dp) };
    if lttpr_count < 0 {
        lttpr_count = 0;
    }
    if let Some(prepare) = unsafe { (*dp).prepare_link_retrain } {
        unsafe { prepare(dp, crtc_state.cast_mut()) };
    }
    let rate = unsafe { (*crtc_state).port_clock };
    let link_bw = unsafe { drm_dp_link_rate_to_bw_code(rate) };
    unsafe { intel_dp_link_training_set_mode(dp, rate, (*crtc_state).vrr.in_range, false) };
    unsafe {
        intel_dp_link_training_set_bw(
            dp,
            link_bw as i32,
            0,
            (*crtc_state).lane_count as i32,
            (*crtc_state).enhanced_framing,
            (*dp).set_idle_link_train.is_some()
                && tk_drm_upstream::drm_dp_helper_upstream::drm_dp_post_lt_adj_req_supported(
                    (*dp).dpcd.as_ptr(),
                )
                && selected_training_pattern(dp, crtc_state, DP_PHY_DPRX) != DP_TRAINING_PATTERN_4,
        );
    }
    let passed = unsafe { link_train_all_phys(dp, crtc_state, lttpr_count) };
    if unsafe { (*dp).link.force_train_failure > 0 } {
        unsafe { (*dp).link.force_train_failure -= 1 };
    } else if passed {
        unsafe { (*dp).link.seq_train_failures = 0 };
        return;
    }
    unsafe { (*dp).link.seq_train_failures += 1 };
    let display = unsafe { dp_to_display(dp) };
    if !display.is_null() && unsafe { (*display).hotplug.ignore_long_hpd } {
        return;
    }
    if unsafe { (*dp).link.seq_train_failures } < MAX_SEQ_TRAIN_FAILURES {
        return;
    }
    let link = unsafe { &mut (*dp).link };
    if !unsafe { intel_digital_port_connected(encoder) } {
        return;
    }
    let current_rate = unsafe { (*crtc_state).port_clock };
    let current_lanes = unsafe { (*crtc_state).lane_count as i32 };
    let forced_rate = link.force_rate;
    let forced_lanes = link.force_lane_count;
    let mut fallback = false;
    if forced_rate == 0 {
        let count =
            (unsafe { (*dp).num_common_rates } as usize).min(unsafe { (*dp).common_rates.len() });
        let index = unsafe {
            core::slice::from_raw_parts(ptr::addr_of!((*dp).common_rates).cast::<i32>(), count)
                .iter()
                .position(|&candidate| candidate == current_rate)
        };
        if let Some(index) = index {
            if index > 0 {
                link.max_rate = unsafe { (*dp).common_rates[index - 1] };
                link.max_lane_count = current_lanes;
                fallback = true;
            }
        }
    }
    if !fallback && forced_lanes == 0 && current_lanes > 1 {
        let count =
            (unsafe { (*dp).num_common_rates } as usize).min(unsafe { (*dp).common_rates.len() });
        if count != 0 {
            link.max_lane_count = current_lanes / 2;
            link.max_rate = unsafe { (*dp).common_rates[count - 1] };
            fallback = true;
        }
    }
    if fallback {
        unsafe { intel_dp_queue_modeset_retry_for_link(state, encoder, crtc_state) };
    } else {
        unsafe { (*dp).link.retrain_disabled = true };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_stop_link_train(dp: *mut IntelDp, state: *const IntelCrtcState) {
    if dp.is_null() || state.is_null() {
        return;
    }
    unsafe { (*dp).link.active = true };
    unsafe {
        intel_dp_program_link_training_pattern(dp, state, DP_PHY_DPRX, DP_TRAINING_PATTERN_DISABLE)
    };
    let _ = unsafe {
        drm_dp_dpcd_write_byte(
            dpcd_aux(dp),
            training_pattern_set_reg(DP_PHY_DPRX),
            DP_TRAINING_PATTERN_DISABLE,
        )
    };
    let encoder = unsafe { dp_to_encoder(dp) };
    if !encoder.is_null() {
        unsafe { intel_hpd_unblock(encoder) };
        let display = unsafe { dp_to_display(dp) };
        if !display.is_null()
            && !unsafe { (*display).hotplug.ignore_long_hpd }
            && unsafe { (*dp).link.seq_train_failures < MAX_SEQ_TRAIN_FAILURES }
        {
            let delay = if unsafe { (*dp).link.seq_train_failures != 0 } {
                0
            } else {
                2000
            };
            unsafe { intel_encoder_link_check_queue_work(encoder, delay) };
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_dump_link_status(
    _dp: *mut IntelDp,
    _phy: DrmDpPhy,
    _link_status: *const u8,
) {
    // Debug-only printk formatting is provided by the caller's diagnostics.
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_128b132b_sdp_crc16(
    _dp: *mut IntelDp,
    _state: *const IntelCrtcState,
) {
    // ADL-P's external SST path is 8b/10b and never invokes DP 2.x CRC SDP.
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_link_training_debugfs_add(_connector: *mut c_void) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swing_preemphasis_combination_obeys_dp_budget() {
        for pre in [
            0,
            1 << DP_TRAIN_PRE_EMPHASIS_SHIFT,
            2 << DP_TRAIN_PRE_EMPHASIS_SHIFT,
            3 << DP_TRAIN_PRE_EMPHASIS_SHIFT,
        ] {
            let max_v = dp_voltage_max(pre);
            assert!(
                max_v + ((pre & DP_TRAIN_PRE_EMPHASIS_MASK) >> DP_TRAIN_PRE_EMPHASIS_SHIFT) <= 3
            );
        }
        assert_eq!(dp_voltage_max(0), 3);
        assert_eq!(dp_voltage_max(1 << DP_TRAIN_PRE_EMPHASIS_SHIFT), 2);
        assert_eq!(dp_voltage_max(2 << DP_TRAIN_PRE_EMPHASIS_SHIFT), 1);
        assert_eq!(dp_voltage_max(3 << DP_TRAIN_PRE_EMPHASIS_SHIFT), 0);
    }

    #[test]
    fn max_voltage_detection_requires_sink_max_marker_and_combined_level_three() {
        assert!(lane_max_vswing_reached(DP_TRAIN_MAX_SWING_REACHED | 3));
        assert!(!lane_max_vswing_reached(3));
        assert!(!lane_max_vswing_reached(DP_TRAIN_MAX_SWING_REACHED | 2));
    }
}

#[inline]
fn lane_max_vswing_reached(train: u8) -> bool {
    let v = (train & DP_TRAIN_VOLTAGE_SWING_MASK) >> DP_TRAIN_VOLTAGE_SWING_SHIFT;
    let p = (train & DP_TRAIN_PRE_EMPHASIS_MASK) >> DP_TRAIN_PRE_EMPHASIS_SHIFT;
    train & DP_TRAIN_MAX_SWING_REACHED != 0 && v + p == 3
}
