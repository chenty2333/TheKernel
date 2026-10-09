// SPDX-License-Identifier: MIT
// Translated from Linux v7.2.3 drivers/gpu/drm/i915/display/intel_dp_link_training.c.
// Copyright © 2008-2015 Intel Corporation.
// The upstream MIT grant is preserved in LICENSE-MIT.
//! Display-12/13 DisplayPort link training, expressed over explicit AUX/PHY callbacks.
//!
//! This module preserves the upstream protocol order, retry budgets, and fallback
//! decisions. The backend owns actual AUX/MMIO serialization and platform admission.

use alloc::vec::Vec;

pub const DP_RECEIVER_CAP_SIZE: usize = 0x0f;
pub const DP_LTTPR_COMMON_CAP_SIZE: usize = 8;
pub const DP_LTTPR_PHY_CAP_SIZE: usize = 3;
pub const DP_LINK_STATUS_SIZE: usize = 6;
pub const DP_MAX_LTTPR: usize = 8;

pub const DP_LT_TUNABLE_PHY_REPEATER_FIELD_DATA_STRUCTURE_REV: u32 = 0xf0000;
pub const DP_PHY_REPEATER_CNT: u32 = 0xf0002;
pub const DP_PHY_REPEATER_MODE: u32 = 0xf0003;
pub const DP_TRAINING_PATTERN_SET: u32 = 0x102;
pub const DP_TRAINING_LANE0_SET: u32 = 0x103;
pub const DP_LINK_BW_SET: u32 = 0x100;
pub const DP_LANE_COUNT_SET: u32 = 0x101;
pub const DP_DOWNSPREAD_CTRL: u32 = 0x107;
pub const DP_SUPPORTED_LINK_RATES: u32 = 0x010;
pub const DP_LINK_RATE_SET: u32 = 0x115;
pub const DP_SINK_STATUS: u32 = 0x205;
pub const DP_SDP_ERROR_DETECTION_CONFIGURATION: u32 = 0x121;

pub const DP_DPCD_REV: usize = 0;
pub const DP_DPCD_REV_14: u8 = 0x14;
pub const DP_POST_LT_ADJ_REQ_SUPPORTED: u8 = 1 << 5;
pub const DP_POST_LT_ADJ_REQ_GRANTED: u8 = 1 << 5;
pub const DP_LANE_COUNT_ENHANCED_FRAME_EN: u8 = 1 << 7;
pub const DP_INTRA_HOP_AUX_REPLY_INDICATION: u8 = 1 << 3;
pub const DP_PHY_REPEATER_MODE_TRANSPARENT: u8 = 0x55;
pub const DP_PHY_REPEATER_MODE_NON_TRANSPARENT: u8 = 0xaa;
pub const DP_MSA_TIMING_PAR_IGNORE_EN: u8 = 1 << 7;
pub const DP_FIXED_VTOTAL_AS_SDP_EN_IN_PR_ACTIVE: u8 = 1 << 6;
pub const DP_SET_ANSI_128B132B: u8 = 1 << 1;
pub const DP_TRAINING_PATTERN_DISABLE: u8 = 0;
pub const DP_TRAINING_PATTERN_1: u8 = 1;
pub const DP_TRAINING_PATTERN_2: u8 = 2;
pub const DP_TRAINING_PATTERN_3: u8 = 3;
pub const DP_TRAINING_PATTERN_4: u8 = 7;
pub const DP_TRAINING_PATTERN_2_CDS: u8 = 3;
pub const DP_LINK_SCRAMBLING_DISABLE: u8 = 1 << 5;
pub const DP_TRAIN_VOLTAGE_SWING_MASK: u8 = 0x03;
pub const DP_TRAIN_VOLTAGE_SWING_SHIFT: u8 = 0;
pub const DP_TRAIN_PRE_EMPHASIS_MASK: u8 = 0x0c;
pub const DP_TRAIN_PRE_EMPHASIS_SHIFT: u8 = 2;
pub const DP_TRAIN_MAX_SWING_REACHED: u8 = 1 << 2;
pub const DP_TRAIN_MAX_PRE_EMPHASIS_REACHED: u8 = 1 << 5;
pub const DP_TX_FFE_PRESET_VALUE_MASK: u8 = 0x0f;
pub const DP_TRAIN_VOLTAGE_SWING_LEVEL_0: u8 = 0;
pub const DP_TRAIN_VOLTAGE_SWING_LEVEL_1: u8 = 1;
pub const DP_TRAIN_VOLTAGE_SWING_LEVEL_2: u8 = 2;
pub const DP_TRAIN_VOLTAGE_SWING_LEVEL_3: u8 = 3;
pub const DP_TRAIN_PRE_EMPH_LEVEL_0: u8 = 0;
pub const DP_TRAIN_PRE_EMPH_LEVEL_1: u8 = 1 << DP_TRAIN_PRE_EMPHASIS_SHIFT;
pub const DP_TRAIN_PRE_EMPH_LEVEL_2: u8 = 2 << DP_TRAIN_PRE_EMPHASIS_SHIFT;
pub const DP_TRAIN_PRE_EMPH_LEVEL_3: u8 = 3 << DP_TRAIN_PRE_EMPHASIS_SHIFT;
pub const DP_SDP_CRC16_128B132B_EN: u8 = 1;
pub const MAX_SEQ_TRAIN_FAILURES: u8 = 2;

/// A DP receiver or one LTTPR PHY. LTTPR index zero is LTTPR1.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DpPhy {
    Dprx,
    Lttpr(u8),
}

impl DpPhy {
    const fn index(self) -> Option<usize> {
        match self {
            Self::Dprx => None,
            Self::Lttpr(i) => Some(i as usize),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinkTrainingError {
    Aux,
    Timeout,
    Invalid,
    Refused,
}

/// Terminal disposition from one invocation of the upstream link-training
/// state machine. In particular, a queued modeset retry is not a trained link.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinkTrainingOutcome {
    Trained,
    RetryDeferred,
    RetryScheduled,
    Disconnected,
    RetryDisabled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinkTrainingRetry {
    Disconnected,
    Scheduled,
    Unavailable,
}

/// Minimal CRTC/encoder state read by the upstream training code.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LinkTrainingCrtcState {
    pub port_clock: i32,
    pub lane_count: u8,
    pub enhanced_framing: bool,
    pub uhbr: bool,
    pub mst: bool,
    pub vrr_in_range: bool,
    pub adaptive_sync_sdp: bool,
    pub alpm_aux_less: bool,
    pub fixed_mode_clock: u32,
    pub fixed_mode_hdisplay: u32,
}

/// Link-wide state corresponding to the training-owned part of `struct intel_dp`.
#[derive(Clone, Debug)]
pub struct IntelDpLinkTraining {
    pub display_version: u8,
    pub geminilake: bool,
    pub broxton: bool,
    pub is_edp: bool,
    pub dpcd: [u8; DP_RECEIVER_CAP_SIZE],
    pub lttpr_common_caps: [u8; DP_LTTPR_COMMON_CAP_SIZE],
    pub lttpr_phy_caps: [[u8; DP_LTTPR_PHY_CAP_SIZE]; DP_MAX_LTTPR],
    pub train_set: [u8; 4],
    pub source_tps3: bool,
    pub source_tps4: bool,
    pub source_voltage_max: u8,
    pub source_preemph_max: u8,
    pub set_idle_link_train: bool,
    pub link_active: bool,
    pub link_rate: i32,
    pub lane_count: u8,
    pub force_rate: i32,
    pub force_lane_count: u8,
    pub max_rate: i32,
    pub max_lane_count: u8,
    pub force_train_failure: u8,
    pub force_retrain: u64,
    pub seq_train_failures: u8,
    pub retrain_disabled: bool,
    pub use_max_params: bool,
    pub hobl_active: bool,
    pub hobl_failed: bool,
    pub source_rates: Vec<i32>,
    pub common_rates: Vec<i32>,
    /// Upstream's ordered link-config table, represented as (rate, lanes).
    pub link_configs: Vec<(i32, u8)>,
}

impl Default for IntelDpLinkTraining {
    fn default() -> Self {
        Self {
            display_version: 12,
            geminilake: false,
            broxton: false,
            is_edp: false,
            dpcd: [0; DP_RECEIVER_CAP_SIZE],
            lttpr_common_caps: [0; DP_LTTPR_COMMON_CAP_SIZE],
            lttpr_phy_caps: [[0; DP_LTTPR_PHY_CAP_SIZE]; DP_MAX_LTTPR],
            train_set: [0; 4],
            source_tps3: false,
            source_tps4: false,
            source_voltage_max: DP_TRAIN_VOLTAGE_SWING_LEVEL_3,
            source_preemph_max: DP_TRAIN_PRE_EMPH_LEVEL_3,
            set_idle_link_train: false,
            link_active: false,
            link_rate: 0,
            lane_count: 0,
            force_rate: 0,
            force_lane_count: 0,
            max_rate: 0,
            max_lane_count: 0,
            force_train_failure: 0,
            force_retrain: 0,
            seq_train_failures: 0,
            retrain_disabled: false,
            use_max_params: false,
            hobl_active: false,
            hobl_failed: false,
            source_rates: Vec::new(),
            common_rates: Vec::new(),
            link_configs: Vec::new(),
        }
    }
}

/// Narrow boundary around DRM AUX helpers, PHY hooks, clocks, and hotplug work.
/// AUX transfer methods return transferred byte count; callers preserve upstream
/// exact-length checks. `now_ms()` and wait callbacks provide jiffies/sleep semantics.
pub trait LinkTrainingIo {
    fn dpcd_probe(&mut self, address: u32) -> Result<(), LinkTrainingError>;
    fn read_dpcd_caps(&mut self) -> Result<[u8; DP_RECEIVER_CAP_SIZE], LinkTrainingError>;
    fn read_lttpr_common_caps(
        &mut self,
    ) -> Result<[u8; DP_LTTPR_COMMON_CAP_SIZE], LinkTrainingError>;
    fn init_lttpr_non_transparent(&mut self, count: i32) -> Result<(), LinkTrainingError>;
    fn read_lttpr_phy_caps(
        &mut self,
        dpcd: &[u8; DP_RECEIVER_CAP_SIZE],
        phy: DpPhy,
    ) -> Result<[u8; DP_LTTPR_PHY_CAP_SIZE], LinkTrainingError>;
    fn dump_lttpr_desc(&mut self, phy: DpPhy);
    fn aux_read(&mut self, address: u32, data: &mut [u8]) -> Result<usize, LinkTrainingError>;
    fn aux_write(&mut self, address: u32, data: &[u8]) -> Result<usize, LinkTrainingError>;
    fn read_phy_link_status(
        &mut self,
        phy: DpPhy,
    ) -> Result<[u8; DP_LINK_STATUS_SIZE], LinkTrainingError>;
    fn read_link_status(&mut self) -> Result<[u8; DP_LINK_STATUS_SIZE], LinkTrainingError>;
    fn read_sink_status(&mut self) -> Result<u8, LinkTrainingError>;
    fn clock_recovery_delay(&mut self, phy: DpPhy, uhbr: bool) -> u32;
    fn channel_eq_delay(&mut self, phy: DpPhy, uhbr: bool) -> u32;
    fn uhbr_aux_rd_interval(&mut self) -> u32;
    fn dump_link_status(&mut self, _phy: DpPhy, _status: &[u8; DP_LINK_STATUS_SIZE]) {}
    fn source_supports_tps3(&self) -> bool {
        false
    }
    fn source_supports_tps4(&self) -> bool {
        false
    }
    fn edp_link_required(
        &mut self,
        link_rate: i32,
        lane_count: u8,
        mode_clock: u32,
        mode_hdisplay: u32,
        bpp_q4: u32,
        flags: u32,
    ) -> u64;
    fn max_link_data_rate(&mut self, link_rate: i32, lane_count: u8) -> u64;
    /// Program a source-side pattern and report PHY/MMIO refusal. A successful
    /// sink AUX write cannot make training pass if the transmitter rejected it.
    fn source_pattern(
        &mut self,
        state: &LinkTrainingCrtcState,
        pattern: u8,
    ) -> Result<(), LinkTrainingError>;
    /// Program source voltage/pre-emphasis and report PHY/MMIO refusal.
    fn source_signal_levels(
        &mut self,
        state: &LinkTrainingCrtcState,
        train_set: &[u8; 4],
    ) -> Result<(), LinkTrainingError>;
    fn prepare_link_retrain(&mut self, state: &LinkTrainingCrtcState);
    fn compute_rate(&mut self, port_clock: i32) -> (u8, u8);
    fn reload_supported_link_rates(&mut self);
    fn wait_us(&mut self, usec: u32);
    fn wait_range_us(&mut self, min_usec: u32, max_usec: u32);
    fn now_ms(&mut self) -> u64;
    fn connected(&mut self) -> bool;
    fn hpd_block(&mut self);
    fn hpd_unblock(&mut self);
    fn queue_link_check(&mut self, delay_ms: u32);
    fn set_idle_link_train(&mut self, state: &LinkTrainingCrtcState);
    fn queue_modeset_retry(&mut self, state: &LinkTrainingCrtcState);
    fn ignore_long_hpd(&mut self) -> bool {
        false
    }
    fn trigger_hpd_irq(&mut self) {}
}

// upstream: intel_dp_link_training.c intel_dp_reset_lttpr_common_caps()
pub fn intel_dp_reset_lttpr_common_caps(dp: &mut IntelDpLinkTraining) {
    dp.lttpr_common_caps = [0; DP_LTTPR_COMMON_CAP_SIZE];
}

// upstream: intel_dp_link_training.c intel_dp_reset_lttpr_count()
pub fn intel_dp_reset_lttpr_count(dp: &mut IntelDpLinkTraining) {
    let offset =
        (DP_PHY_REPEATER_CNT - DP_LT_TUNABLE_PHY_REPEATER_FIELD_DATA_STRUCTURE_REV) as usize;
    dp.lttpr_common_caps[offset] = 0;
}

// upstream: intel_dp_link_training.c intel_dp_lttpr_phy_caps()
pub fn intel_dp_lttpr_phy_caps(
    dp: &IntelDpLinkTraining,
    phy: DpPhy,
) -> Option<&[u8; DP_LTTPR_PHY_CAP_SIZE]> {
    phy.index().and_then(|i| dp.lttpr_phy_caps.get(i))
}

// upstream: intel_dp_link_training.c intel_dp_read_lttpr_phy_caps()
pub fn intel_dp_read_lttpr_phy_caps<I: LinkTrainingIo>(
    dp: &mut IntelDpLinkTraining,
    io: &mut I,
    dpcd: &[u8; DP_RECEIVER_CAP_SIZE],
    phy: DpPhy,
) {
    let Some(index) = phy.index() else { return };
    if let Ok(caps) = io.read_lttpr_phy_caps(dpcd, phy) {
        dp.lttpr_phy_caps[index] = caps;
    }
}

// upstream: intel_dp_link_training.c intel_dp_read_lttpr_common_caps()
pub fn intel_dp_read_lttpr_common_caps<I: LinkTrainingIo>(
    dp: &mut IntelDpLinkTraining,
    io: &mut I,
) -> bool {
    let Ok(caps) = io.read_lttpr_common_caps() else {
        intel_dp_reset_lttpr_common_caps(dp);
        return false;
    };
    dp.lttpr_common_caps = caps;
    if dp.lttpr_common_caps[0] < 0x14 {
        intel_dp_reset_lttpr_common_caps(dp);
        return false;
    }
    true
}

// upstream: intel_dp_link_training.c intel_dp_set_lttpr_transparent_mode()
pub fn intel_dp_set_lttpr_transparent_mode(dp: &mut IntelDpLinkTraining, enable: bool) -> bool {
    let offset =
        (DP_PHY_REPEATER_MODE - DP_LT_TUNABLE_PHY_REPEATER_FIELD_DATA_STRUCTURE_REV) as usize;
    dp.lttpr_common_caps[offset] = if enable {
        DP_PHY_REPEATER_MODE_TRANSPARENT
    } else {
        DP_PHY_REPEATER_MODE_NON_TRANSPARENT
    };
    true
}

// upstream: intel_dp_link_training.c intel_dp_lttpr_transparent_mode_enabled()
pub fn intel_dp_lttpr_transparent_mode_enabled(dp: &IntelDpLinkTraining) -> bool {
    let offset =
        (DP_PHY_REPEATER_MODE - DP_LT_TUNABLE_PHY_REPEATER_FIELD_DATA_STRUCTURE_REV) as usize;
    dp.lttpr_common_caps[offset] == DP_PHY_REPEATER_MODE_TRANSPARENT
}

fn lttpr_count(dp: &IntelDpLinkTraining) -> i32 {
    let count = dp.lttpr_common_caps
        [(DP_PHY_REPEATER_CNT - DP_LT_TUNABLE_PHY_REPEATER_FIELD_DATA_STRUCTURE_REV) as usize];
    match count.count_ones() {
        0 => 0,
        1 => 8 - (7 - count.leading_zeros() as i32),
        8 => -34, // -ERANGE: more LTTPRs than the supported maximum.
        _ => -22, // -EINVAL: invalid non-one-hot repeater count.
    }
}

// upstream: intel_dp_link_training.c intel_dp_init_lttpr_phys()
pub fn intel_dp_init_lttpr_phys<I: LinkTrainingIo>(
    dp: &mut IntelDpLinkTraining,
    io: &mut I,
    dpcd: &[u8; DP_RECEIVER_CAP_SIZE],
) -> i32 {
    if !intel_dp_read_lttpr_common_caps(dp, io) {
        return 0;
    }
    let count = lttpr_count(dp);
    // Do not explicitly set transparent mode when the common caps report no LTTPRs.
    if count == 0 {
        return 0;
    }
    if dp.link_active {
        if count < 0 || intel_dp_lttpr_transparent_mode_enabled(dp) {
            intel_dp_reset_lttpr_count(dp);
            return 0;
        }
        return count;
    }
    if io.init_lttpr_non_transparent(count).is_err() {
        intel_dp_set_lttpr_transparent_mode(dp, true);
        intel_dp_reset_lttpr_count(dp);
        return 0;
    }
    intel_dp_set_lttpr_transparent_mode(dp, false);
    let _ = dpcd;
    count
}

// upstream: intel_dp_link_training.c intel_dp_init_lttpr()
pub fn intel_dp_init_lttpr<I: LinkTrainingIo>(
    dp: &mut IntelDpLinkTraining,
    io: &mut I,
    dpcd: &[u8; DP_RECEIVER_CAP_SIZE],
) -> i32 {
    let count = intel_dp_init_lttpr_phys(dp, io, dpcd);
    for i in 0..count {
        let phy = DpPhy::Lttpr(i as u8);
        intel_dp_read_lttpr_phy_caps(dp, io, dpcd, phy);
        io.dump_lttpr_desc(phy);
    }
    count
}

// upstream: intel_dp_link_training.c intel_dp_read_dprx_caps()
pub fn intel_dp_read_dprx_caps<I: LinkTrainingIo>(
    dp: &IntelDpLinkTraining,
    io: &mut I,
    dpcd: &mut [u8; DP_RECEIVER_CAP_SIZE],
) -> Result<(), LinkTrainingError> {
    if dp.is_edp {
        return Ok(());
    }
    if dp.display_version >= 10
        && !dp.geminilake
        && io
            .dpcd_probe(DP_LT_TUNABLE_PHY_REPEATER_FIELD_DATA_STRUCTURE_REV)
            .is_err()
    {
        return Err(LinkTrainingError::Aux);
    }
    *dpcd = io.read_dpcd_caps().map_err(|_| LinkTrainingError::Aux)?;
    Ok(())
}

// upstream: intel_dp_link_training.c intel_dp_init_lttpr_and_dprx_caps()
pub fn intel_dp_init_lttpr_and_dprx_caps<I: LinkTrainingIo>(
    dp: &mut IntelDpLinkTraining,
    io: &mut I,
) -> Result<i32, LinkTrainingError> {
    let mut count = 0;
    if !dp.is_edp && dp.display_version >= 10 && !dp.geminilake {
        let mut caps = [0u8; DP_RECEIVER_CAP_SIZE];
        intel_dp_read_dprx_caps(dp, io, &mut caps)?;
        count = intel_dp_init_lttpr(dp, io, &caps);
    }
    dp.dpcd = match io.read_dpcd_caps() {
        Ok(caps) => caps,
        Err(_) => {
            intel_dp_reset_lttpr_common_caps(dp);
            return Err(LinkTrainingError::Aux);
        }
    };
    Ok(count)
}

// upstream: intel_dp_link_training.c dp_voltage_max()
pub const fn dp_voltage_max(preemph: u8) -> u8 {
    match preemph & DP_TRAIN_PRE_EMPHASIS_MASK {
        DP_TRAIN_PRE_EMPH_LEVEL_0 => DP_TRAIN_VOLTAGE_SWING_LEVEL_3,
        DP_TRAIN_PRE_EMPH_LEVEL_1 => DP_TRAIN_VOLTAGE_SWING_LEVEL_2,
        DP_TRAIN_PRE_EMPH_LEVEL_2 => DP_TRAIN_VOLTAGE_SWING_LEVEL_1,
        _ => DP_TRAIN_VOLTAGE_SWING_LEVEL_0,
    }
}

// upstream: intel_dp_link_training.c intel_dp_lttpr_voltage_max()
pub fn intel_dp_lttpr_voltage_max(dp: &IntelDpLinkTraining, phy: DpPhy) -> u8 {
    let tx_caps = intel_dp_lttpr_phy_caps(dp, phy)
        .map(|caps| caps[1])
        .unwrap_or(0);
    if tx_caps & 1 != 0 {
        DP_TRAIN_VOLTAGE_SWING_LEVEL_3
    } else {
        DP_TRAIN_VOLTAGE_SWING_LEVEL_2
    }
}

// upstream: intel_dp_link_training.c intel_dp_lttpr_preemph_max()
pub fn intel_dp_lttpr_preemph_max(dp: &IntelDpLinkTraining, phy: DpPhy) -> u8 {
    let tx_caps = intel_dp_lttpr_phy_caps(dp, phy)
        .map(|caps| caps[1])
        .unwrap_or(0);
    if tx_caps & 2 != 0 {
        DP_TRAIN_PRE_EMPH_LEVEL_3
    } else {
        DP_TRAIN_PRE_EMPH_LEVEL_2
    }
}

// upstream: intel_dp_link_training.c intel_dp_phy_is_downstream_of_source()
pub fn intel_dp_phy_is_downstream_of_source(dp: &IntelDpLinkTraining, phy: DpPhy) -> bool {
    let count = lttpr_count(dp);
    count <= 0 || phy == DpPhy::Lttpr((count - 1) as u8)
}

// upstream: intel_dp_link_training.c intel_dp_phy_voltage_max()
pub fn intel_dp_phy_voltage_max(dp: &IntelDpLinkTraining, phy: DpPhy) -> u8 {
    if intel_dp_phy_is_downstream_of_source(dp, phy) {
        dp.source_voltage_max
    } else {
        let upstream = match phy {
            DpPhy::Dprx => 0,
            DpPhy::Lttpr(i) => i.saturating_add(1),
        };
        intel_dp_lttpr_voltage_max(dp, DpPhy::Lttpr(upstream))
    }
}

// upstream: intel_dp_link_training.c intel_dp_phy_preemph_max()
pub fn intel_dp_phy_preemph_max(dp: &IntelDpLinkTraining, phy: DpPhy) -> u8 {
    if intel_dp_phy_is_downstream_of_source(dp, phy) {
        dp.source_preemph_max
    } else {
        let upstream = match phy {
            DpPhy::Dprx => 0,
            DpPhy::Lttpr(i) => i.saturating_add(1),
        };
        intel_dp_lttpr_preemph_max(dp, DpPhy::Lttpr(upstream))
    }
}

// upstream: intel_dp_link_training.c has_per_lane_signal_levels()
pub fn has_per_lane_signal_levels(dp: &IntelDpLinkTraining, phy: DpPhy) -> bool {
    !intel_dp_phy_is_downstream_of_source(dp, phy) || dp.display_version >= 10 || dp.broxton
}

// upstream: intel_dp_link_training.c intel_dp_get_lane_adjust_tx_ffe_preset()
pub fn intel_dp_get_lane_adjust_tx_ffe_preset(
    dp: &IntelDpLinkTraining,
    state: &LinkTrainingCrtcState,
    phy: DpPhy,
    link_status: &[u8; DP_LINK_STATUS_SIZE],
    lane: usize,
) -> u8 {
    if has_per_lane_signal_levels(dp, phy) {
        adjust_tx_ffe_preset(
            link_status,
            lane.min(state.lane_count.saturating_sub(1) as usize),
        )
    } else {
        (0..state.lane_count as usize)
            .map(|i| adjust_tx_ffe_preset(link_status, i))
            .max()
            .unwrap_or(0)
    }
}

// upstream: intel_dp_link_training.c intel_dp_get_lane_adjust_vswing_preemph()
pub fn intel_dp_get_lane_adjust_vswing_preemph(
    dp: &IntelDpLinkTraining,
    state: &LinkTrainingCrtcState,
    phy: DpPhy,
    link_status: &[u8; DP_LINK_STATUS_SIZE],
    lane: usize,
) -> u8 {
    let (mut v, mut p) = if has_per_lane_signal_levels(dp, phy) {
        let lane = lane.min(state.lane_count.saturating_sub(1) as usize);
        (
            adjust_request_voltage(link_status, lane),
            adjust_request_pre_emphasis(link_status, lane),
        )
    } else {
        (0, 0)
    };
    if !has_per_lane_signal_levels(dp, phy) {
        for lane in 0..state.lane_count as usize {
            v = v.max(adjust_request_voltage(link_status, lane));
            p = p.max(adjust_request_pre_emphasis(link_status, lane));
        }
    }
    let preemph_max = intel_dp_phy_preemph_max(dp, phy);
    if p >= preemph_max {
        p = preemph_max | DP_TRAIN_MAX_PRE_EMPHASIS_REACHED;
    }
    v = v.min(dp_voltage_max(p));
    let voltage_max = intel_dp_phy_voltage_max(dp, phy);
    if v >= voltage_max {
        v = voltage_max | DP_TRAIN_MAX_SWING_REACHED;
    }
    v | p
}

// upstream: intel_dp_link_training.c intel_dp_get_lane_adjust_train()
pub fn intel_dp_get_lane_adjust_train(
    dp: &IntelDpLinkTraining,
    state: &LinkTrainingCrtcState,
    phy: DpPhy,
    link_status: &[u8; DP_LINK_STATUS_SIZE],
    lane: usize,
) -> u8 {
    if state.uhbr {
        intel_dp_get_lane_adjust_tx_ffe_preset(dp, state, phy, link_status, lane)
    } else {
        intel_dp_get_lane_adjust_vswing_preemph(dp, state, phy, link_status, lane)
    }
}

fn adjust_nibble(status: &[u8; DP_LINK_STATUS_SIZE], lane: usize) -> u8 {
    status[4 + lane / 2] >> ((lane % 2) * 4) & 0x0f
}
fn adjust_request_voltage(status: &[u8; DP_LINK_STATUS_SIZE], lane: usize) -> u8 {
    adjust_nibble(status, lane) & DP_TRAIN_VOLTAGE_SWING_MASK
}
fn adjust_request_pre_emphasis(status: &[u8; DP_LINK_STATUS_SIZE], lane: usize) -> u8 {
    adjust_nibble(status, lane) & DP_TRAIN_PRE_EMPHASIS_MASK
}
fn adjust_tx_ffe_preset(status: &[u8; DP_LINK_STATUS_SIZE], lane: usize) -> u8 {
    adjust_nibble(status, lane) & DP_TX_FFE_PRESET_VALUE_MASK
}

// upstream: intel_dp_link_training.c intel_dp_get_adjust_train()
pub fn intel_dp_get_adjust_train(
    dp: &mut IntelDpLinkTraining,
    state: &LinkTrainingCrtcState,
    phy: DpPhy,
    link_status: &[u8; DP_LINK_STATUS_SIZE],
) -> bool {
    let mut changed = false;
    for lane in 0..4 {
        let new = intel_dp_get_lane_adjust_train(dp, state, phy, link_status, lane);
        if dp.train_set[lane] != new {
            dp.train_set[lane] = new;
            changed = true;
        }
    }
    changed
}

// upstream: intel_dp_link_training.c intel_dp_training_pattern_set_reg()
pub const fn intel_dp_training_pattern_set_reg(phy: DpPhy) -> u32 {
    match phy {
        DpPhy::Dprx => DP_TRAINING_PATTERN_SET,
        DpPhy::Lttpr(i) => 0xf0010 + (i as u32) * 0x50,
    }
}

// upstream: intel_dp_link_training.c intel_dp_set_link_train()
pub fn intel_dp_set_link_train<I: LinkTrainingIo>(
    dp: &mut IntelDpLinkTraining,
    io: &mut I,
    state: &LinkTrainingCrtcState,
    phy: DpPhy,
    pattern: u8,
) -> bool {
    if !intel_dp_program_link_training_pattern(dp, io, state, phy, pattern) {
        return false;
    }
    let mut buf = [0u8; 5];
    buf[0] = pattern;
    let lanes = (state.lane_count as usize).min(4);
    buf[1..1 + lanes].copy_from_slice(&dp.train_set[..lanes]);
    io.aux_write(intel_dp_training_pattern_set_reg(phy), &buf[..lanes + 1])
        .ok()
        == Some(lanes + 1)
}

// upstream: intel_dp_link_training.c dp_training_pattern_name()
pub const fn dp_training_pattern_name(pattern: u8) -> char {
    match pattern {
        DP_TRAINING_PATTERN_1 | DP_TRAINING_PATTERN_2 | DP_TRAINING_PATTERN_3 => {
            (b'0' + pattern) as char
        }
        DP_TRAINING_PATTERN_4 => '4',
        _ => '?',
    }
}

// upstream: intel_dp_link_training.c intel_dp_program_link_training_pattern()
pub fn intel_dp_program_link_training_pattern<I: LinkTrainingIo>(
    _dp: &mut IntelDpLinkTraining,
    io: &mut I,
    state: &LinkTrainingCrtcState,
    _phy: DpPhy,
    pattern: u8,
) -> bool {
    io.source_pattern(state, training_pattern_symbol(pattern))
        .is_ok()
}

const fn training_pattern_symbol(pattern: u8) -> u8 {
    pattern & !DP_LINK_SCRAMBLING_DISABLE
}

// upstream: intel_dp_link_training.c intel_dp_set_signal_levels()
pub fn intel_dp_set_signal_levels<I: LinkTrainingIo>(
    dp: &IntelDpLinkTraining,
    io: &mut I,
    state: &LinkTrainingCrtcState,
    phy: DpPhy,
) -> bool {
    if intel_dp_phy_is_downstream_of_source(dp, phy) {
        io.source_signal_levels(state, &dp.train_set).is_ok()
    } else {
        true
    }
}

// upstream: intel_dp_link_training.c intel_dp_reset_link_train()
pub fn intel_dp_reset_link_train<I: LinkTrainingIo>(
    dp: &mut IntelDpLinkTraining,
    io: &mut I,
    state: &LinkTrainingCrtcState,
    phy: DpPhy,
    pattern: u8,
) -> bool {
    dp.train_set = [0; 4];
    if !intel_dp_set_signal_levels(dp, io, state, phy) {
        return false;
    }
    intel_dp_set_link_train(dp, io, state, phy, pattern)
}

// upstream: intel_dp_link_training.c intel_dp_update_link_train()
pub fn intel_dp_update_link_train<I: LinkTrainingIo>(
    dp: &IntelDpLinkTraining,
    io: &mut I,
    state: &LinkTrainingCrtcState,
    phy: DpPhy,
) -> bool {
    if !intel_dp_set_signal_levels(dp, io, state, phy) {
        return false;
    }
    let address = match phy {
        DpPhy::Dprx => DP_TRAINING_LANE0_SET,
        DpPhy::Lttpr(i) => 0xf0011 + (i as u32) * 0x50,
    };
    let lanes = (state.lane_count as usize).min(4);
    io.aux_write(address, &dp.train_set[..lanes]).ok() == Some(lanes)
}

// upstream: intel_dp_link_training.c intel_dp_lane_max_tx_ffe_reached()
pub const fn intel_dp_lane_max_tx_ffe_reached(train_set_lane: u8) -> bool {
    train_set_lane & DP_TX_FFE_PRESET_VALUE_MASK == DP_TX_FFE_PRESET_VALUE_MASK
}

// upstream: intel_dp_link_training.c intel_dp_lane_max_vswing_reached()
pub const fn intel_dp_lane_max_vswing_reached(train_set_lane: u8) -> bool {
    let v = (train_set_lane & DP_TRAIN_VOLTAGE_SWING_MASK) >> DP_TRAIN_VOLTAGE_SWING_SHIFT;
    let p = (train_set_lane & DP_TRAIN_PRE_EMPHASIS_MASK) >> DP_TRAIN_PRE_EMPHASIS_SHIFT;
    train_set_lane & DP_TRAIN_MAX_SWING_REACHED != 0 && v + p == 3
}

// upstream: intel_dp_link_training.c intel_dp_link_max_vswing_reached()
pub fn intel_dp_link_max_vswing_reached(
    dp: &IntelDpLinkTraining,
    state: &LinkTrainingCrtcState,
) -> bool {
    (0..state.lane_count as usize).all(|lane| {
        if state.uhbr {
            intel_dp_lane_max_tx_ffe_reached(dp.train_set[lane])
        } else {
            intel_dp_lane_max_vswing_reached(dp.train_set[lane])
        }
    })
}

pub const DP_TPS3_SUPPORTED: u8 = 1 << 6;
pub const DP_TPS4_SUPPORTED: u8 = 1 << 7;
pub const DP_TRAINING_PATTERN_MASK_1_4: u8 = 0x0f;
pub const DP_LINK_STATUS_UPDATED: u8 = 1 << 0;
pub const DP_CHANNEL_EQ_BITS: u8 = 0x07;
pub const DP_CHANNEL_EQ_DONE: u8 = 1 << 1;
pub const DP_INTERLANE_ALIGN_DONE: u8 = 1 << 0;
pub const DP_POST_LT_ADJ_REQ_IN_PROGRESS: u8 = 1 << 1;
pub const DP_LANE_CHANNEL_EQ_DONE: u8 = 1 << 1;
pub const DP_LANE_SYMBOL_LOCKED: u8 = 1 << 2;
pub const DP_128B132B_DPRX_EQ_INTERLANE_ALIGN_DONE: u8 = 1 << 2;
pub const DP_128B132B_DPRX_CDS_INTERLANE_ALIGN_DONE: u8 = 1 << 3;
pub const DP_128B132B_LT_FAILED: u8 = 1 << 4;

fn clock_recovery_ok(status: &[u8; DP_LINK_STATUS_SIZE], lanes: u8) -> bool {
    (0..lanes as usize).all(|lane| lane_status(status, lane) & 1 != 0)
}
fn channel_eq_ok(status: &[u8; DP_LINK_STATUS_SIZE], lanes: u8) -> bool {
    clock_recovery_ok(status, lanes)
        && (0..lanes as usize)
            .all(|lane| lane_status(status, lane) & DP_CHANNEL_EQ_BITS == DP_CHANNEL_EQ_BITS)
        && status[2] & DP_INTERLANE_ALIGN_DONE != 0
}
fn lane_status(status: &[u8; DP_LINK_STATUS_SIZE], lane: usize) -> u8 {
    (status[lane / 2] >> ((lane % 2) * 4)) & 0x0f
}
fn post_lt_adj_req_in_progress(status: &[u8; DP_LINK_STATUS_SIZE]) -> bool {
    status[3] & DP_POST_LT_ADJ_REQ_IN_PROGRESS != 0
}
fn dp_128b132b_training_failed(status: &[u8; DP_LINK_STATUS_SIZE]) -> bool {
    status[2] & DP_128B132B_LT_FAILED != 0
}
fn dp_128b132b_lane_channel_eq_done(status: &[u8; DP_LINK_STATUS_SIZE], lanes: u8) -> bool {
    status[2] & DP_INTERLANE_ALIGN_DONE != 0
        && (0..lanes as usize).all(|lane| lane_status(status, lane) & DP_LANE_CHANNEL_EQ_DONE != 0)
}
fn dp_128b132b_eq_interlane_align_done(status: &[u8; DP_LINK_STATUS_SIZE]) -> bool {
    status[2] & DP_128B132B_DPRX_EQ_INTERLANE_ALIGN_DONE != 0
}
fn dp_128b132b_cds_interlane_align_done(status: &[u8; DP_LINK_STATUS_SIZE]) -> bool {
    status[2] & DP_128B132B_DPRX_CDS_INTERLANE_ALIGN_DONE != 0
}
fn dp_128b132b_lane_symbol_locked(status: &[u8; DP_LINK_STATUS_SIZE], lanes: u8) -> bool {
    (0..lanes as usize).all(|lane| lane_status(status, lane) & DP_LANE_SYMBOL_LOCKED != 0)
}

// upstream: intel_dp_link_training.c intel_dp_link_training_set_mode()
pub fn intel_dp_link_training_set_mode<I: LinkTrainingIo>(
    dp: &IntelDpLinkTraining,
    io: &mut I,
    link_rate: i32,
    is_vrr: bool,
    pr_with_as_sdp_enable: bool,
) -> Result<(), LinkTrainingError> {
    let mut link_config = [0u8; 2];
    if is_vrr {
        link_config[0] |= DP_MSA_TIMING_PAR_IGNORE_EN;
    }
    if pr_with_as_sdp_enable {
        link_config[0] |= DP_FIXED_VTOTAL_AS_SDP_EN_IN_PR_ACTIVE;
    }
    if link_rate >= 1_000_000 {
        link_config[1] = DP_SET_ANSI_128B132B;
    }
    let _ = dp;
    if io.aux_write(DP_DOWNSPREAD_CTRL, &link_config)? != link_config.len() {
        return Err(LinkTrainingError::Aux);
    }
    Ok(())
}

// upstream: intel_dp_link_training.c intel_dp_pr_with_as_sdp_enabled()
pub const fn intel_dp_pr_with_as_sdp_enabled(state: &LinkTrainingCrtcState) -> bool {
    state.alpm_aux_less && state.adaptive_sync_sdp
}

// upstream: intel_dp_link_training.c intel_dp_update_downspread_ctrl()
pub fn intel_dp_update_downspread_ctrl<I: LinkTrainingIo>(
    dp: &IntelDpLinkTraining,
    io: &mut I,
    state: &LinkTrainingCrtcState,
) -> Result<(), LinkTrainingError> {
    intel_dp_link_training_set_mode(
        dp,
        io,
        state.port_clock,
        state.vrr_in_range,
        intel_dp_pr_with_as_sdp_enabled(state),
    )
}

// upstream: intel_dp_link_training.c intel_dp_link_training_set_bw()
pub fn intel_dp_link_training_set_bw<I: LinkTrainingIo>(
    io: &mut I,
    link_bw: u8,
    rate_select: u8,
    lane_count: u8,
    enhanced_framing: bool,
    post_lt_adj_req: bool,
) -> Result<(), LinkTrainingError> {
    let mut lane_count = lane_count;
    if enhanced_framing {
        lane_count |= DP_LANE_COUNT_ENHANCED_FRAME_EN;
    }
    if post_lt_adj_req {
        lane_count |= DP_POST_LT_ADJ_REQ_GRANTED;
    }
    if link_bw != 0 {
        if io.aux_write(DP_LINK_BW_SET, &[link_bw, lane_count])? != 2 {
            return Err(LinkTrainingError::Aux);
        }
    } else {
        if io.aux_write(DP_LANE_COUNT_SET, &[lane_count])? != 1
            || io.aux_write(DP_LINK_RATE_SET, &[rate_select])? != 1
        {
            return Err(LinkTrainingError::Aux);
        }
    }
    Ok(())
}

// upstream: intel_dp_link_training.c intel_dp_training_pattern()
pub fn intel_dp_training_pattern<I: LinkTrainingIo>(
    dp: &IntelDpLinkTraining,
    io: &I,
    state: &LinkTrainingCrtcState,
    phy: DpPhy,
) -> u32 {
    if state.uhbr {
        return DP_TRAINING_PATTERN_2 as u32;
    }
    let source_tps4 = dp.source_tps4 || io.source_supports_tps4();
    let sink_tps4 = phy != DpPhy::Dprx || dp.dpcd[3] & DP_TPS4_SUPPORTED != 0;
    if source_tps4 && sink_tps4 {
        return DP_TRAINING_PATTERN_4 as u32;
    }
    let source_tps3 = dp.source_tps3 || io.source_supports_tps3();
    let sink_tps3 = phy != DpPhy::Dprx || dp.dpcd[2] & DP_TPS3_SUPPORTED != 0;
    if source_tps3 && sink_tps3 {
        return DP_TRAINING_PATTERN_3 as u32;
    }
    DP_TRAINING_PATTERN_2 as u32
}

// upstream: intel_dp_link_training.c intel_dp_use_post_lt_adj_req()
pub fn intel_dp_use_post_lt_adj_req<I: LinkTrainingIo>(
    dp: &IntelDpLinkTraining,
    io: &I,
    state: &LinkTrainingCrtcState,
) -> bool {
    dp.set_idle_link_train
        && dp.dpcd[2] & DP_POST_LT_ADJ_REQ_SUPPORTED != 0
        && intel_dp_training_pattern(dp, io, state, DpPhy::Dprx) != DP_TRAINING_PATTERN_4 as u32
}

// upstream: intel_dp_link_training.c intel_dp_update_link_bw_set()
pub fn intel_dp_update_link_bw_set<I: LinkTrainingIo>(
    dp: &IntelDpLinkTraining,
    io: &mut I,
    state: &LinkTrainingCrtcState,
    link_bw: u8,
    rate_select: u8,
) -> Result<(), LinkTrainingError> {
    let post_lt_adj_req = intel_dp_use_post_lt_adj_req(dp, io, state);
    intel_dp_link_training_set_bw(
        io,
        link_bw,
        rate_select,
        state.lane_count,
        state.enhanced_framing,
        post_lt_adj_req,
    )
}

// upstream: intel_dp_link_training.c intel_dp_prepare_link_train()
pub fn intel_dp_prepare_link_train<I: LinkTrainingIo>(
    dp: &IntelDpLinkTraining,
    io: &mut I,
    state: &LinkTrainingCrtcState,
) -> Result<(), LinkTrainingError> {
    io.prepare_link_retrain(state);
    let (link_bw, rate_select) = io.compute_rate(state.port_clock);
    if link_bw == 0 {
        io.reload_supported_link_rates();
    }
    intel_dp_update_downspread_ctrl(dp, io, state)?;
    intel_dp_update_link_bw_set(dp, io, state, link_bw, rate_select)
}

// upstream: intel_dp_link_training.c intel_dp_adjust_request_changed()
pub fn intel_dp_adjust_request_changed(
    state: &LinkTrainingCrtcState,
    old: &[u8; DP_LINK_STATUS_SIZE],
    new: &[u8; DP_LINK_STATUS_SIZE],
) -> bool {
    for lane in 0..state.lane_count as usize {
        let (old_req, new_req) = if state.uhbr {
            (
                adjust_tx_ffe_preset(old, lane),
                adjust_tx_ffe_preset(new, lane),
            )
        } else {
            (
                adjust_request_voltage(old, lane) | adjust_request_pre_emphasis(old, lane),
                adjust_request_voltage(new, lane) | adjust_request_pre_emphasis(new, lane),
            )
        };
        if old_req != new_req {
            return true;
        }
    }
    false
}

// upstream: intel_dp_link_training.c intel_dp_dump_link_status()
pub fn intel_dp_dump_link_status<I: LinkTrainingIo>(
    io: &mut I,
    phy: DpPhy,
    status: &[u8; DP_LINK_STATUS_SIZE],
) {
    io.dump_link_status(phy, status);
}

// upstream: intel_dp_link_training.c intel_dp_link_training_clock_recovery()
pub fn intel_dp_link_training_clock_recovery<I: LinkTrainingIo>(
    dp: &mut IntelDpLinkTraining,
    io: &mut I,
    state: &LinkTrainingCrtcState,
    phy: DpPhy,
) -> bool {
    let mut old_link_status = [0u8; DP_LINK_STATUS_SIZE];
    let mut voltage_tries = 1;
    let max_cr_tries = if dp.dpcd[DP_DPCD_REV] >= DP_DPCD_REV_14 {
        10
    } else {
        80
    };
    let delay_us = io.clock_recovery_delay(phy, state.uhbr);
    if !intel_dp_reset_link_train(
        dp,
        io,
        state,
        phy,
        DP_TRAINING_PATTERN_1 | DP_LINK_SCRAMBLING_DISABLE,
    ) {
        return false;
    }
    let mut max_vswing_reached = false;
    let mut last_status = [0u8; DP_LINK_STATUS_SIZE];
    for _cr_try in 0..max_cr_tries {
        io.wait_us(delay_us);
        let Ok(link_status) = io.read_phy_link_status(phy) else {
            return false;
        };
        last_status = link_status;
        if clock_recovery_ok(&link_status, state.lane_count) {
            return true;
        }
        if voltage_tries == 5 || max_vswing_reached {
            intel_dp_dump_link_status(io, phy, &link_status);
            return false;
        }
        intel_dp_get_adjust_train(dp, state, phy, &link_status);
        if !intel_dp_update_link_train(dp, io, state, phy) {
            return false;
        }
        if !intel_dp_adjust_request_changed(state, &old_link_status, &link_status) {
            voltage_tries += 1;
        } else {
            voltage_tries = 1;
        }
        old_link_status = link_status;
        if intel_dp_link_max_vswing_reached(dp, state) {
            max_vswing_reached = true;
        }
    }
    intel_dp_dump_link_status(io, phy, &last_status);
    false
}

// upstream: intel_dp_link_training.c intel_dp_link_training_channel_equalization()
pub fn intel_dp_link_training_channel_equalization<I: LinkTrainingIo>(
    dp: &mut IntelDpLinkTraining,
    io: &mut I,
    state: &LinkTrainingCrtcState,
    phy: DpPhy,
) -> bool {
    let delay_us = io.channel_eq_delay(phy, state.uhbr);
    let mut training_pattern = intel_dp_training_pattern(dp, io, state, phy) as u8;
    if training_pattern != DP_TRAINING_PATTERN_4 {
        training_pattern |= DP_LINK_SCRAMBLING_DISABLE;
    }
    if !intel_dp_set_link_train(dp, io, state, phy, training_pattern) {
        return false;
    }
    let mut channel_eq = false;
    let mut last_status = [0u8; DP_LINK_STATUS_SIZE];
    let mut tries = 0;
    while tries < 5 {
        io.wait_us(delay_us);
        let Ok(link_status) = io.read_phy_link_status(phy) else {
            break;
        };
        last_status = link_status;
        if !clock_recovery_ok(&link_status, state.lane_count) {
            intel_dp_dump_link_status(io, phy, &link_status);
            break;
        }
        if channel_eq_ok(&link_status, state.lane_count) {
            channel_eq = true;
            break;
        }
        intel_dp_get_adjust_train(dp, state, phy, &link_status);
        if !intel_dp_update_link_train(dp, io, state, phy) {
            break;
        }
        tries += 1;
    }
    if tries == 5 {
        intel_dp_dump_link_status(io, phy, &last_status);
    }
    channel_eq
}

// upstream: intel_dp_link_training.c intel_dp_post_lt_adj_req()
pub fn intel_dp_post_lt_adj_req<I: LinkTrainingIo>(
    dp: &mut IntelDpLinkTraining,
    io: &mut I,
    state: &LinkTrainingCrtcState,
) -> bool {
    if !intel_dp_use_post_lt_adj_req(dp, io, state) {
        return true;
    }
    let mut link_status = match io.read_phy_link_status(DpPhy::Dprx) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let mut deadline = io.now_ms().wrapping_add(200);
    let mut timeout = false;
    let mut success = false;
    let mut changes = 0;
    loop {
        if !clock_recovery_ok(&link_status, state.lane_count) {
            intel_dp_dump_link_status(io, DpPhy::Dprx, &link_status);
            break;
        }
        if !channel_eq_ok(&link_status, state.lane_count) {
            intel_dp_dump_link_status(io, DpPhy::Dprx, &link_status);
            break;
        }
        if !post_lt_adj_req_in_progress(&link_status) {
            success = true;
            intel_dp_dump_link_status(io, DpPhy::Dprx, &link_status);
            break;
        }
        if changes == 6 {
            success = true;
            intel_dp_dump_link_status(io, DpPhy::Dprx, &link_status);
            break;
        }
        if timeout {
            success = true;
            intel_dp_dump_link_status(io, DpPhy::Dprx, &link_status);
            break;
        }
        io.wait_us(5_000);
        link_status = match io.read_phy_link_status(DpPhy::Dprx) {
            Ok(s) => s,
            Err(_) => break,
        };
        if intel_dp_get_adjust_train(dp, state, DpPhy::Dprx, &link_status) {
            deadline = io.now_ms().wrapping_add(200);
            changes += 1;
            if !intel_dp_update_link_train(dp, io, state, DpPhy::Dprx) {
                break;
            }
        } else if io.now_ms() > deadline {
            timeout = true;
        }
    }
    success
}

// upstream: intel_dp_link_training.c intel_dp_stop_post_lt_adj_req()
pub fn intel_dp_stop_post_lt_adj_req<I: LinkTrainingIo>(
    dp: &IntelDpLinkTraining,
    io: &mut I,
    state: &LinkTrainingCrtcState,
) -> Result<(), LinkTrainingError> {
    if !intel_dp_use_post_lt_adj_req(dp, io, state) {
        return Ok(());
    }
    let mut lane_count = state.lane_count;
    if state.enhanced_framing {
        lane_count |= DP_LANE_COUNT_ENHANCED_FRAME_EN;
    }
    if io.aux_write(DP_LANE_COUNT_SET, &[lane_count])? != 1 {
        return Err(LinkTrainingError::Aux);
    }
    Ok(())
}

// upstream: intel_dp_link_training.c intel_dp_disable_dpcd_training_pattern()
pub fn intel_dp_disable_dpcd_training_pattern<I: LinkTrainingIo>(io: &mut I, phy: DpPhy) -> bool {
    io.aux_write(
        intel_dp_training_pattern_set_reg(phy),
        &[DP_TRAINING_PATTERN_DISABLE],
    )
    .ok()
        == Some(1)
}

fn finish_link_training<I: LinkTrainingIo>(io: &mut I, phy: DpPhy, passed: bool) -> bool {
    // Cleanup is mandatory even when training failed; avoid `passed && write()`
    // because short-circuit evaluation would leave the sink in training mode.
    let pattern_disabled = intel_dp_disable_dpcd_training_pattern(io, phy);
    passed && pattern_disabled
}

// upstream: intel_dp_link_training.c intel_dp_128b132b_intra_hop()
pub fn intel_dp_128b132b_intra_hop<I: LinkTrainingIo>(
    io: &mut I,
) -> Result<i32, LinkTrainingError> {
    let sink_status = io.read_sink_status()?;
    Ok(if sink_status & DP_INTRA_HOP_AUX_REPLY_INDICATION != 0 {
        1
    } else {
        0
    })
}

// upstream: intel_dp_link_training.c intel_dp_stop_link_train()
pub fn intel_dp_stop_link_train<I: LinkTrainingIo>(
    dp: &mut IntelDpLinkTraining,
    io: &mut I,
    state: &LinkTrainingCrtcState,
) {
    dp.link_active = true;
    intel_dp_program_link_training_pattern(dp, io, state, DpPhy::Dprx, DP_TRAINING_PATTERN_DISABLE);
    if state.uhbr {
        let deadline = io.now_ms().wrapping_add(500);
        loop {
            if matches!(intel_dp_128b132b_intra_hop(io), Ok(0)) {
                break;
            }
            if io.now_ms() >= deadline {
                break;
            }
            io.wait_us(500);
        }
    }
    io.hpd_unblock();
    if !io.ignore_long_hpd() && dp.seq_train_failures < MAX_SEQ_TRAIN_FAILURES {
        let delay_ms = if dp.seq_train_failures != 0 { 0 } else { 2_000 };
        io.queue_link_check(delay_ms);
    }
}

// upstream: intel_dp_link_training.c intel_dp_link_train_phy()
pub fn intel_dp_link_train_phy<I: LinkTrainingIo>(
    dp: &mut IntelDpLinkTraining,
    io: &mut I,
    state: &LinkTrainingCrtcState,
    phy: DpPhy,
) -> bool {
    if !intel_dp_link_training_clock_recovery(dp, io, state, phy) {
        return false;
    }
    intel_dp_link_training_channel_equalization(dp, io, state, phy)
}

// upstream: intel_dp_link_training.c intel_dp_can_link_train_fallback_for_edp()
pub fn intel_dp_can_link_train_fallback_for_edp<I: LinkTrainingIo>(
    state: &LinkTrainingCrtcState,
    io: &mut I,
    link_rate: i32,
    lane_count: u8,
) -> bool {
    let mode_rate = io.edp_link_required(
        link_rate,
        lane_count,
        state.fixed_mode_clock,
        state.fixed_mode_hdisplay,
        18 << 4,
        0,
    );
    let max_rate = io.max_link_data_rate(link_rate, lane_count);
    mode_rate <= max_rate
}

// upstream: intel_dp_link_training.c reduce_link_params_in_bw_order()
pub fn reduce_link_params_in_bw_order(
    dp: &IntelDpLinkTraining,
    state: &LinkTrainingCrtcState,
) -> Option<(i32, u8)> {
    let mut index = dp
        .link_configs
        .iter()
        .position(|&(rate, lanes)| rate == state.port_clock && lanes == state.lane_count)?
        as isize;
    index -= 1;
    while index >= 0 {
        let (rate, lanes) = dp.link_configs[index as usize];
        if (dp.force_rate != 0 && dp.force_rate != rate)
            || (dp.force_lane_count != 0 && dp.force_lane_count != lanes)
        {
            index -= 1;
            continue;
        }
        return Some((rate, lanes));
    }
    None
}

// upstream: intel_dp_link_training.c reduce_link_rate()
pub fn reduce_link_rate(dp: &IntelDpLinkTraining, current_rate: i32) -> Option<i32> {
    if dp.force_rate != 0 {
        return None;
    }
    let index = dp
        .common_rates
        .iter()
        .position(|&rate| rate == current_rate)?;
    if index == 0 {
        return None;
    }
    let new_rate = dp.common_rates[index - 1];
    if (current_rate >= 1_000_000) != (new_rate >= 1_000_000) {
        return None;
    }
    Some(new_rate)
}

// upstream: intel_dp_link_training.c reduce_lane_count()
pub fn reduce_lane_count(dp: &IntelDpLinkTraining, current_lane_count: u8) -> Option<u8> {
    if dp.force_lane_count != 0 || current_lane_count == 1 {
        return None;
    }
    Some(current_lane_count >> 1)
}

// upstream: intel_dp_link_training.c reduce_link_params_in_rate_lane_order()
pub fn reduce_link_params_in_rate_lane_order(
    dp: &IntelDpLinkTraining,
    state: &LinkTrainingCrtcState,
) -> Option<(i32, u8)> {
    let mut lane_count = state.lane_count;
    let link_rate = match reduce_link_rate(dp, state.port_clock) {
        Some(rate) => rate,
        None => {
            lane_count = reduce_lane_count(dp, state.lane_count)?;
            *dp.common_rates.last()?
        }
    };
    Some((link_rate, lane_count))
}

// upstream: intel_dp_link_training.c reduce_link_params()
pub fn reduce_link_params(
    dp: &IntelDpLinkTraining,
    state: &LinkTrainingCrtcState,
) -> Option<(i32, u8)> {
    if state.mst {
        reduce_link_params_in_bw_order(dp, state)
    } else {
        reduce_link_params_in_rate_lane_order(dp, state)
    }
}

// upstream: intel_dp_link_training.c intel_dp_get_link_train_fallback_values()
pub fn intel_dp_get_link_train_fallback_values<I: LinkTrainingIo>(
    dp: &mut IntelDpLinkTraining,
    io: &mut I,
    state: &LinkTrainingCrtcState,
) -> bool {
    if dp.is_edp && !dp.use_max_params {
        dp.use_max_params = true;
        return true;
    }
    let Some((new_link_rate, new_lane_count)) = reduce_link_params(dp, state) else {
        return false;
    };
    if dp.is_edp
        && !intel_dp_can_link_train_fallback_for_edp(state, io, new_link_rate, new_lane_count)
    {
        return true;
    }
    dp.max_rate = new_link_rate;
    dp.max_lane_count = new_lane_count;
    true
}

// upstream: intel_dp_link_training.c intel_dp_schedule_fallback_link_training()
pub fn intel_dp_schedule_fallback_link_training<I: LinkTrainingIo>(
    dp: &mut IntelDpLinkTraining,
    io: &mut I,
    state: &LinkTrainingCrtcState,
) -> LinkTrainingRetry {
    if !io.connected() {
        return LinkTrainingRetry::Disconnected;
    }
    if dp.hobl_active {
        dp.hobl_failed = true;
    } else if !intel_dp_get_link_train_fallback_values(dp, io, state) {
        return LinkTrainingRetry::Unavailable;
    }
    io.queue_modeset_retry(state);
    LinkTrainingRetry::Scheduled
}

// upstream: intel_dp_link_training.c intel_dp_link_train_all_phys()
pub fn intel_dp_link_train_all_phys<I: LinkTrainingIo>(
    dp: &mut IntelDpLinkTraining,
    io: &mut I,
    state: &LinkTrainingCrtcState,
    lttpr_count: i32,
) -> bool {
    let mut passed = true;
    for i in (0..lttpr_count).rev() {
        let phy = DpPhy::Lttpr(i as u8);
        let trained = intel_dp_link_train_phy(dp, io, state, phy);
        let pattern_disabled = intel_dp_disable_dpcd_training_pattern(io, phy);
        if !trained || !pattern_disabled {
            passed = false;
            break;
        }
    }
    if passed {
        passed = intel_dp_link_train_phy(dp, io, state, DpPhy::Dprx);
    }
    if !intel_dp_disable_dpcd_training_pattern(io, DpPhy::Dprx) {
        passed = false;
    }
    io.set_idle_link_train(state);
    if passed {
        passed = intel_dp_post_lt_adj_req(dp, io, state);
    }
    if intel_dp_stop_post_lt_adj_req(dp, io, state).is_err() {
        passed = false;
    }
    passed
}

// upstream: intel_dp_link_training.c intel_dp_128b132b_lane_eq()
pub fn intel_dp_128b132b_lane_eq<I: LinkTrainingIo>(
    dp: &mut IntelDpLinkTraining,
    io: &mut I,
    state: &LinkTrainingCrtcState,
) -> bool {
    let mut link_status;
    if !intel_dp_reset_link_train(dp, io, state, DpPhy::Dprx, DP_TRAINING_PATTERN_1) {
        return false;
    }
    let mut delay_us = io.uhbr_aux_rd_interval();
    link_status = match io.read_link_status() {
        Ok(s) => s,
        Err(_) => return false,
    };
    intel_dp_get_adjust_train(dp, state, DpPhy::Dprx, &link_status);
    if !intel_dp_update_link_train(dp, io, state, DpPhy::Dprx) {
        return false;
    }
    if !intel_dp_set_link_train(dp, io, state, DpPhy::Dprx, DP_TRAINING_PATTERN_2) {
        return false;
    }

    let deadline = io.now_ms().wrapping_add(450);
    let mut timeout = false;
    let mut try_count = 0;
    for try_index in 0..20 {
        try_count = try_index;
        io.wait_us(delay_us);
        link_status = match io.read_link_status() {
            Ok(s) => s,
            Err(_) => return false,
        };
        if dp_128b132b_training_failed(&link_status) {
            intel_dp_dump_link_status(io, DpPhy::Dprx, &link_status);
            return false;
        }
        if dp_128b132b_lane_channel_eq_done(&link_status, state.lane_count) {
            break;
        }
        if timeout {
            intel_dp_dump_link_status(io, DpPhy::Dprx, &link_status);
            return false;
        }
        if io.now_ms() > deadline {
            timeout = true;
        }
        delay_us = io.uhbr_aux_rd_interval();
        intel_dp_get_adjust_train(dp, state, DpPhy::Dprx, &link_status);
        if !intel_dp_update_link_train(dp, io, state, DpPhy::Dprx) {
            return false;
        }
        try_count = try_index + 1;
    }
    if try_count == 20 {
        intel_dp_dump_link_status(io, DpPhy::Dprx, &link_status);
        return false;
    }

    loop {
        if io.now_ms() > deadline {
            timeout = true;
        }
        link_status = match io.read_link_status() {
            Ok(s) => s,
            Err(_) => return false,
        };
        if dp_128b132b_training_failed(&link_status) {
            intel_dp_dump_link_status(io, DpPhy::Dprx, &link_status);
            return false;
        }
        if dp_128b132b_eq_interlane_align_done(&link_status) {
            break;
        }
        if timeout {
            intel_dp_dump_link_status(io, DpPhy::Dprx, &link_status);
            return false;
        }
        io.wait_range_us(2_000, 3_000);
    }
    true
}

// upstream: intel_dp_link_training.c intel_dp_128b132b_lane_cds()
pub fn intel_dp_128b132b_lane_cds<I: LinkTrainingIo>(
    io: &mut I,
    state: &LinkTrainingCrtcState,
    lttpr_count: i32,
) -> bool {
    if io
        .aux_write(DP_TRAINING_PATTERN_SET, &[DP_TRAINING_PATTERN_2_CDS])
        .ok()
        != Some(1)
    {
        return false;
    }
    let deadline = io.now_ms().wrapping_add(((lttpr_count + 1) * 20) as u64);
    loop {
        let timeout = io.now_ms() > deadline;
        io.wait_range_us(2_000, 3_000);
        let link_status = match io.read_link_status() {
            Ok(s) => s,
            Err(_) => return false,
        };
        if dp_128b132b_eq_interlane_align_done(&link_status)
            && dp_128b132b_cds_interlane_align_done(&link_status)
            && dp_128b132b_lane_symbol_locked(&link_status, state.lane_count)
        {
            return true;
        }
        if dp_128b132b_training_failed(&link_status) || timeout {
            intel_dp_dump_link_status(io, DpPhy::Dprx, &link_status);
            return false;
        }
    }
}

// upstream: intel_dp_link_training.c intel_dp_128b132b_link_train()
pub fn intel_dp_128b132b_link_train<I: LinkTrainingIo>(
    dp: &mut IntelDpLinkTraining,
    io: &mut I,
    state: &LinkTrainingCrtcState,
    lttpr_count: i32,
) -> bool {
    let deadline = io.now_ms().wrapping_add(500);
    let mut intra_hop_clear = false;
    loop {
        match intel_dp_128b132b_intra_hop(io) {
            Ok(0) => {
                intra_hop_clear = true;
                break;
            }
            Err(_) => break,
            _ => {}
        }
        if io.now_ms() >= deadline {
            break;
        }
        io.wait_us(500);
    }
    let passed = if intra_hop_clear {
        intel_dp_128b132b_lane_eq(dp, io, state)
            && intel_dp_128b132b_lane_cds(io, state, lttpr_count)
    } else {
        false
    };
    if !passed {
        let _ = intel_dp_program_link_training_pattern(
            dp,
            io,
            state,
            DpPhy::Dprx,
            DP_TRAINING_PATTERN_2,
        );
    }
    finish_link_training(io, DpPhy::Dprx, passed)
}

// upstream: intel_dp_link_training.c intel_dp_start_link_train()
pub fn intel_dp_start_link_train<I: LinkTrainingIo>(
    dp: &mut IntelDpLinkTraining,
    io: &mut I,
    state: &LinkTrainingCrtcState,
) -> Result<LinkTrainingOutcome, LinkTrainingError> {
    io.hpd_block();
    // A capability/AUX error is not equivalent to a sink with no repeaters.
    // Keep it visible to the caller instead of attempting training with an
    // invented zero-LTTPR topology.
    let mut lttpr_count = match intel_dp_init_lttpr_and_dprx_caps(dp, io) {
        Ok(count) => count,
        Err(error) => {
            io.hpd_unblock();
            return Err(error);
        }
    };
    if lttpr_count < 0 {
        lttpr_count = 0;
    }
    if let Err(error) = intel_dp_prepare_link_train(dp, io, state) {
        io.hpd_unblock();
        return Err(error);
    }
    let passed = if state.uhbr {
        intel_dp_128b132b_link_train(dp, io, state, lttpr_count)
    } else {
        intel_dp_link_train_all_phys(dp, io, state, lttpr_count)
    };
    if dp.force_train_failure != 0 {
        dp.force_train_failure -= 1;
    } else if passed {
        dp.seq_train_failures = 0;
        return Ok(LinkTrainingOutcome::Trained);
    }
    dp.seq_train_failures = dp.seq_train_failures.saturating_add(1);
    if io.ignore_long_hpd() {
        return Ok(LinkTrainingOutcome::RetryDeferred);
    }
    if dp.seq_train_failures < MAX_SEQ_TRAIN_FAILURES {
        return Ok(LinkTrainingOutcome::RetryDeferred);
    }
    match intel_dp_schedule_fallback_link_training(dp, io, state) {
        LinkTrainingRetry::Disconnected => return Ok(LinkTrainingOutcome::Disconnected),
        LinkTrainingRetry::Scheduled => return Ok(LinkTrainingOutcome::RetryScheduled),
        LinkTrainingRetry::Unavailable => {}
    }
    dp.retrain_disabled = true;
    Ok(LinkTrainingOutcome::RetryDisabled)
}

// upstream: intel_dp_link_training.c intel_dp_128b132b_sdp_crc16()
pub fn intel_dp_128b132b_sdp_crc16<I: LinkTrainingIo>(
    dp: &IntelDpLinkTraining,
    io: &mut I,
    state: &LinkTrainingCrtcState,
) {
    if !state.uhbr {
        return;
    }
    let _ = dp;
    let _ = io.aux_write(
        DP_SDP_ERROR_DETECTION_CONFIGURATION,
        &[DP_SDP_CRC16_128B132B_EN],
    );
}

fn parse_c_integer(text: &str) -> Result<i32, LinkTrainingError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(LinkTrainingError::Invalid);
    }
    let (negative, digits) = match text.as_bytes()[0] {
        b'-' => (true, &text[1..]),
        b'+' => (false, &text[1..]),
        _ => (false, text),
    };
    if digits.is_empty() {
        return Err(LinkTrainingError::Invalid);
    }
    let (radix, digits) = if digits.starts_with("0x") || digits.starts_with("0X") {
        (16, &digits[2..])
    } else if digits.len() > 1 && digits.starts_with('0') {
        (8, &digits[1..])
    } else {
        (10, digits)
    };
    let magnitude = i64::from_str_radix(digits, radix).map_err(|_| LinkTrainingError::Invalid)?;
    let value = if negative { -magnitude } else { magnitude };
    i32::try_from(value).map_err(|_| LinkTrainingError::Invalid)
}

// upstream: intel_dp_link_training.c parse_link_rate()
pub fn parse_link_rate(dp: &IntelDpLinkTraining, input: &[u8]) -> Result<i32, LinkTrainingError> {
    let text = core::str::from_utf8(input)
        .map_err(|_| LinkTrainingError::Invalid)?
        .trim();
    if text == "auto" {
        return Ok(0);
    }
    let rate = parse_c_integer(text)?;
    if !dp.source_rates.contains(&rate) {
        return Err(LinkTrainingError::Invalid);
    }
    Ok(rate)
}

// upstream: intel_dp_link_training.c parse_lane_count()
pub fn parse_lane_count(input: &[u8]) -> Result<u8, LinkTrainingError> {
    let text = core::str::from_utf8(input)
        .map_err(|_| LinkTrainingError::Invalid)?
        .trim();
    if text == "auto" {
        return Ok(0);
    }
    match parse_c_integer(text)? {
        1 => Ok(1),
        2 => Ok(2),
        4 => Ok(4),
        _ => Err(LinkTrainingError::Invalid),
    }
}

// The upstream show/store and debugfs-create wrappers are intentionally outside
// this protocol module: they require DRM debugfs, seq_file, user-copy, and the
// connection-modeset mutex. The two pure input parsers above remain translated.

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct SourcePhyFailureIo {
        aux_writes: usize,
        short_aux_write: bool,
        fail_caps_read: bool,
        hpd_blocks: usize,
        hpd_unblocks: usize,
    }

    impl LinkTrainingIo for SourcePhyFailureIo {
        fn dpcd_probe(&mut self, _: u32) -> Result<(), LinkTrainingError> {
            Ok(())
        }
        fn read_dpcd_caps(&mut self) -> Result<[u8; DP_RECEIVER_CAP_SIZE], LinkTrainingError> {
            if self.fail_caps_read {
                Err(LinkTrainingError::Aux)
            } else {
                Ok([0; DP_RECEIVER_CAP_SIZE])
            }
        }
        fn read_lttpr_common_caps(
            &mut self,
        ) -> Result<[u8; DP_LTTPR_COMMON_CAP_SIZE], LinkTrainingError> {
            Ok([0; DP_LTTPR_COMMON_CAP_SIZE])
        }
        fn init_lttpr_non_transparent(&mut self, _: i32) -> Result<(), LinkTrainingError> {
            Ok(())
        }
        fn read_lttpr_phy_caps(
            &mut self,
            _: &[u8; DP_RECEIVER_CAP_SIZE],
            _: DpPhy,
        ) -> Result<[u8; DP_LTTPR_PHY_CAP_SIZE], LinkTrainingError> {
            Ok([0; DP_LTTPR_PHY_CAP_SIZE])
        }
        fn dump_lttpr_desc(&mut self, _: DpPhy) {}
        fn aux_read(&mut self, _: u32, _: &mut [u8]) -> Result<usize, LinkTrainingError> {
            Ok(0)
        }
        fn aux_write(&mut self, _: u32, data: &[u8]) -> Result<usize, LinkTrainingError> {
            self.aux_writes += 1;
            Ok(data.len() - usize::from(self.short_aux_write))
        }
        fn read_phy_link_status(
            &mut self,
            _: DpPhy,
        ) -> Result<[u8; DP_LINK_STATUS_SIZE], LinkTrainingError> {
            Ok([0; DP_LINK_STATUS_SIZE])
        }
        fn read_link_status(&mut self) -> Result<[u8; DP_LINK_STATUS_SIZE], LinkTrainingError> {
            Ok([0; DP_LINK_STATUS_SIZE])
        }
        fn read_sink_status(&mut self) -> Result<u8, LinkTrainingError> {
            Ok(0)
        }
        fn clock_recovery_delay(&mut self, _: DpPhy, _: bool) -> u32 {
            0
        }
        fn channel_eq_delay(&mut self, _: DpPhy, _: bool) -> u32 {
            0
        }
        fn uhbr_aux_rd_interval(&mut self) -> u32 {
            0
        }
        fn edp_link_required(&mut self, _: i32, _: u8, _: u32, _: u32, _: u32, _: u32) -> u64 {
            0
        }
        fn max_link_data_rate(&mut self, _: i32, _: u8) -> u64 {
            0
        }
        fn source_pattern(
            &mut self,
            _: &LinkTrainingCrtcState,
            _: u8,
        ) -> Result<(), LinkTrainingError> {
            Err(LinkTrainingError::Refused)
        }
        fn source_signal_levels(
            &mut self,
            _: &LinkTrainingCrtcState,
            _: &[u8; 4],
        ) -> Result<(), LinkTrainingError> {
            Ok(())
        }
        fn prepare_link_retrain(&mut self, _: &LinkTrainingCrtcState) {}
        fn compute_rate(&mut self, _: i32) -> (u8, u8) {
            (0, 0)
        }
        fn reload_supported_link_rates(&mut self) {}
        fn wait_us(&mut self, _: u32) {}
        fn wait_range_us(&mut self, _: u32, _: u32) {}
        fn now_ms(&mut self) -> u64 {
            0
        }
        fn connected(&mut self) -> bool {
            true
        }
        fn hpd_block(&mut self) {
            self.hpd_blocks += 1;
        }
        fn hpd_unblock(&mut self) {
            self.hpd_unblocks += 1;
        }
        fn queue_link_check(&mut self, _: u32) {}
        fn set_idle_link_train(&mut self, _: &LinkTrainingCrtcState) {}
        fn queue_modeset_retry(&mut self, _: &LinkTrainingCrtcState) {}
    }

    #[test]
    fn parses_auto_and_only_advertised_source_rates() {
        let mut dp = IntelDpLinkTraining::default();
        dp.source_rates = alloc::vec![162_000, 270_000, 540_000];
        assert_eq!(parse_link_rate(&dp, b"auto\n"), Ok(0));
        assert_eq!(parse_link_rate(&dp, b"540000"), Ok(540_000));
        assert_eq!(
            parse_link_rate(&dp, b"810000"),
            Err(LinkTrainingError::Invalid)
        );
        assert_eq!(
            parse_link_rate(&dp, b"junk"),
            Err(LinkTrainingError::Invalid)
        );
    }

    #[test]
    fn parses_supported_lane_counts_and_preserves_auto() {
        assert_eq!(parse_lane_count(b"auto"), Ok(0));
        for (bytes, lanes) in [(b"1".as_slice(), 1), (b"2", 2), (b"4", 4)] {
            assert_eq!(parse_lane_count(bytes), Ok(lanes));
        }
        assert_eq!(parse_lane_count(b"3"), Err(LinkTrainingError::Invalid));
    }

    #[test]
    fn status_checks_use_lane_nibbles_and_alignment_bits() {
        let status = [0x77, 0x77, DP_INTERLANE_ALIGN_DONE, 0, 0, 0];
        assert!(clock_recovery_ok(&status, 4));
        assert!(channel_eq_ok(&status, 4));
        assert!(!channel_eq_ok(&[0x77, 0x77, 0, 0, 0, 0], 4));
        assert!(dp_128b132b_training_failed(&[
            0,
            0,
            DP_128B132B_LT_FAILED,
            0,
            0,
            0
        ]));
    }

    #[test]
    fn source_pattern_refusal_prevents_sink_aux_training_write() {
        let mut dp = IntelDpLinkTraining::default();
        let state = LinkTrainingCrtcState {
            lane_count: 4,
            ..Default::default()
        };
        let mut io = SourcePhyFailureIo::default();

        assert!(!intel_dp_set_link_train(
            &mut dp,
            &mut io,
            &state,
            DpPhy::Dprx,
            DP_TRAINING_PATTERN_1,
        ));
        assert_eq!(io.aux_writes, 0);
    }

    #[test]
    fn short_aux_bandwidth_write_refuses_link_training_prepare() {
        let mut io = SourcePhyFailureIo {
            short_aux_write: true,
            ..Default::default()
        };
        assert_eq!(
            intel_dp_link_training_set_bw(&mut io, 0x14, 0, 4, false, false),
            Err(LinkTrainingError::Aux)
        );
        assert_eq!(io.aux_writes, 1);
    }

    #[test]
    fn short_downspread_write_stops_before_bandwidth_programming() {
        let mut io = SourcePhyFailureIo {
            short_aux_write: true,
            ..Default::default()
        };
        assert_eq!(
            intel_dp_prepare_link_train(
                &IntelDpLinkTraining::default(),
                &mut io,
                &LinkTrainingCrtcState::default(),
            ),
            Err(LinkTrainingError::Aux)
        );
        // The initial downspread write is short; link bandwidth/lane count
        // programming must not proceed after that failed prerequisite.
        assert_eq!(io.aux_writes, 1);
    }

    #[test]
    fn failed_post_adjustment_cleanup_is_not_reported_as_success() {
        let mut io = SourcePhyFailureIo {
            short_aux_write: true,
            ..Default::default()
        };
        let mut dp = IntelDpLinkTraining::default();
        dp.set_idle_link_train = true;
        dp.dpcd[2] = DP_POST_LT_ADJ_REQ_SUPPORTED;
        let state = LinkTrainingCrtcState {
            lane_count: 4,
            ..Default::default()
        };

        assert_eq!(
            intel_dp_stop_post_lt_adj_req(&dp, &mut io, &state),
            Err(LinkTrainingError::Aux)
        );
    }

    #[test]
    fn training_pattern_disable_requires_complete_aux_write() {
        let mut io = SourcePhyFailureIo {
            short_aux_write: true,
            ..Default::default()
        };
        assert!(!intel_dp_disable_dpcd_training_pattern(
            &mut io,
            DpPhy::Dprx
        ));
        assert_eq!(io.aux_writes, 1);
    }

    #[test]
    fn failed_training_still_attempts_pattern_disable() {
        let mut io = SourcePhyFailureIo::default();
        assert!(!finish_link_training(&mut io, DpPhy::Dprx, false));
        assert_eq!(io.aux_writes, 1);
    }

    #[test]
    fn capability_read_error_stops_before_training_writes() {
        let mut io = SourcePhyFailureIo {
            fail_caps_read: true,
            ..Default::default()
        };
        let mut dp = IntelDpLinkTraining::default();
        assert_eq!(
            intel_dp_start_link_train(&mut dp, &mut io, &LinkTrainingCrtcState::default()),
            Err(LinkTrainingError::Aux)
        );
        assert_eq!(io.aux_writes, 0);
        assert_eq!(io.hpd_blocks, 1);
        assert_eq!(io.hpd_unblocks, 1);
    }

    #[test]
    fn setup_aux_error_unblocks_hpd_before_returning() {
        let mut io = SourcePhyFailureIo {
            short_aux_write: true,
            ..Default::default()
        };
        let mut dp = IntelDpLinkTraining::default();
        assert_eq!(
            intel_dp_start_link_train(&mut dp, &mut io, &LinkTrainingCrtcState::default()),
            Err(LinkTrainingError::Aux)
        );
        assert_eq!(io.hpd_unblocks, 1);
        assert_eq!(io.hpd_blocks, 1);
    }
}
