// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//
// External DDI path for ADL-P (display version 13), ported from
// drivers/gpu/drm/i915/display/intel_ddi.c in Linux v7.2.3.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unsafe_code)]

use core::{ffi::c_void, ptr};

use tk_drm_upstream::{
    drm_connector_types::{DrmConnector, DrmConnectorState},
    drm_device_types::DrmDevice,
    drm_encoder_types::{DrmEncoder, DrmEncoderFuncs},
};

use crate::{
    intel_combo_phy_regs_upstream::*,
    intel_ddi_buf_trans_types_upstream::IntelDdiBufTrans,
    intel_de_upstream::{
        intel_de_posting_read, intel_de_read, intel_de_rmw, intel_de_wait_for_clear_ms,
        intel_de_wait_for_set_ms, intel_de_write,
    },
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_limits_types_upstream::{PORT_NONE, Pipe, Port, TRANSCODER_EDP},
    intel_display_reg_defs_upstream::INVALID_MMIO_REG,
    intel_display_regs_upstream::*,
    intel_display_types_upstream::{
        INTEL_OUTPUT_DDI, INTEL_OUTPUT_DP, INTEL_OUTPUT_HDMI, IntelAtomicState, IntelConnector,
        IntelCrtc, IntelCrtcState, IntelDigitalPort, IntelDp, IntelEncoder, IntelHdmi,
    },
    intel_display_wa_upstream::DISPLAY_VER,
    intel_dkl_phy_regs_upstream::*,
    intel_dpll_mgr_types_upstream::{IntelDpll, IntelDpllHwState},
    intel_mg_phy_regs_upstream::{MG_DP_MODE_CFG_DP_X1_MODE, MG_DP_MODE_CFG_DP_X2_MODE},
    intel_workarounds_types_upstream::I915RegT,
};

const DP_SIGNAL_LEVELS: [u8; 10] = [0x00, 0x08, 0x10, 0x18, 0x01, 0x09, 0x11, 0x02, 0x0a, 0x03];
const MAX_DDI_SIGNAL_LEVELS: usize = DP_SIGNAL_LEVELS.len();
const DRM_MODE_ENCODER_TMDS: i32 = 2;
const DP_TRAIN_VOLTAGE_SWING_MASK: u8 = 0x03;
const DP_TRAIN_PRE_EMPHASIS_MASK: u8 = 0x18;
const DP_TRAIN_PRESET_MASK: u8 = 0x0f;
const TRANS_DDI_PORT_WIDTH_SHIFT: u32 = 1;
const DP_FEC_CONFIGURATION: u32 = 0x120;
const DP_FEC_STATUS: u32 = 0x280;
const DP_FEC_READY: u8 = 1 << 0;
const DP_FEC_DECODE_EN_DETECTED: u8 = 1 << 0;
const DP_FEC_DECODE_DIS_DETECTED: u8 = 1 << 1;

unsafe extern "C" {
    fn __drm_to_display(drm: *mut DrmDevice) -> *mut IntelDisplay;
    fn drm_encoder_init(
        drm: *mut DrmDevice,
        encoder: *mut DrmEncoder,
        funcs: *const DrmEncoderFuncs,
        encoder_type: i32,
        name: *const i8,
    ) -> i32;
    fn drm_encoder_cleanup(encoder: *mut DrmEncoder);
    fn intel_bios_encoder_port(data: *const c_void) -> Port;
    fn intel_bios_encoder_supports_dvi(data: *const c_void) -> bool;
    fn intel_bios_encoder_supports_hdmi(data: *const c_void) -> bool;
    fn intel_bios_encoder_supports_dp(data: *const c_void) -> bool;
    fn intel_bios_encoder_supports_typec_usb(data: *const c_void) -> bool;
    fn intel_bios_encoder_supports_tbt(data: *const c_void) -> bool;
    fn intel_bios_encoder_lane_reversal(data: *const c_void) -> bool;
    fn intel_bios_hdmi_level_shift(data: *const c_void) -> i32;
    fn intel_bios_encoder_is_dedicated_external(data: *const c_void) -> bool;
    fn intel_port_to_phy(display: *mut IntelDisplay, port: Port) -> u32;
    fn intel_encoder_to_phy(encoder: *mut IntelEncoder) -> u32;
    fn intel_encoder_is_tc(encoder: *const IntelEncoder) -> bool;
    fn intel_encoder_is_combo(encoder: *const IntelEncoder) -> bool;
    fn intel_encoder_to_tc(encoder: *const IntelEncoder) -> u32;
    fn intel_tc_port_in_tbt_alt_mode(port: *mut IntelDigitalPort) -> bool;
    fn intel_tc_port_in_legacy_mode(port: *mut IntelDigitalPort) -> bool;
    fn intel_tc_port_in_dp_alt_mode(port: *mut IntelDigitalPort) -> bool;
    fn intel_tc_port_get_link(port: *mut IntelDigitalPort, lanes: u8);
    fn intel_tc_port_put_link(port: *mut IntelDigitalPort);
    fn intel_tc_port_set_fia_lane_count(port: *mut IntelDigitalPort, lanes: u8);
    fn intel_tc_port_link_cancel_reset_work(port: *mut IntelDigitalPort);
    fn intel_display_power_ddi_lanes_domain(display: *mut IntelDisplay, port: Port) -> i32;
    fn intel_display_power_ddi_io_domain(display: *mut IntelDisplay, port: Port) -> i32;
    fn intel_display_power_get(display: *mut IntelDisplay, domain: i32) -> *mut c_void;
    fn intel_display_power_put(display: *mut IntelDisplay, domain: i32, wakeref: *mut c_void);
    fn intel_display_power_flush_work(display: *mut IntelDisplay);
    fn intel_crtc_has_type(state: *const IntelCrtcState, output_type: u32) -> bool;
    fn intel_crtc_has_dp_encoder(state: *const IntelCrtcState) -> bool;
    fn intel_ddi_buf_trans_init(encoder: *mut IntelEncoder);
    fn intel_connector_alloc() -> *mut IntelConnector;
    fn intel_connector_free(connector: *mut IntelConnector);
    fn intel_dp_init_connector(port: *mut IntelDigitalPort, connector: *mut IntelConnector)
    -> bool;
    fn intel_dp_cleanup_connector(port: *mut IntelDigitalPort, connector: *mut IntelConnector);
    fn intel_dp_encoder_flush_work(encoder: *mut DrmEncoder);
    fn intel_dp_test_phy(dp: *mut IntelDp) -> bool;
    fn intel_dp_check_link_state(dp: *mut IntelDp);
    fn intel_encoder_hotplug(encoder: *mut IntelEncoder, connector: *mut IntelConnector) -> u32;
    fn intel_dp_invalidate_source_oui(dp: *mut IntelDp);
    fn intel_hdmi_init_connector(
        port: *mut IntelDigitalPort,
        connector: *mut IntelConnector,
    ) -> bool;
    fn intel_dp_compute_config(
        encoder: *mut IntelEncoder,
        state: *mut IntelCrtcState,
        conn_state: *mut DrmConnectorState,
    ) -> i32;
    fn intel_dp_set_link_params(dp: *mut IntelDp, rate: i32, lanes: u8);
    fn intel_dp_needs_vsc_sdp(
        state: *const IntelCrtcState,
        conn_state: *const DrmConnectorState,
    ) -> bool;
    fn intel_dp_sink_enable_decompression(
        state: *mut IntelAtomicState,
        connector: *mut IntelConnector,
        crtc_state: *mut IntelCrtcState,
    );
    fn intel_dp_sink_disable_decompression(
        state: *mut IntelAtomicState,
        connector: *mut IntelConnector,
        crtc_state: *mut IntelCrtcState,
    );
    fn intel_dsc_dp_pps_write(encoder: *mut IntelEncoder, crtc_state: *mut IntelCrtcState);
    fn intel_hdmi_compute_has_hdmi_sink(
        encoder: *mut IntelEncoder,
        state: *const IntelCrtcState,
        conn_state: *const DrmConnectorState,
    ) -> bool;
    fn intel_hdmi_compute_config(
        encoder: *mut IntelEncoder,
        state: *mut IntelCrtcState,
        conn_state: *mut DrmConnectorState,
    ) -> i32;
    fn intel_hdmi_encoder_shutdown(encoder: *mut IntelEncoder);
    fn intel_hdmi_handle_sink_scrambling(
        encoder: *mut IntelEncoder,
        connector: *mut DrmConnector,
        high_tmds_clock_ratio: bool,
        scrambling: bool,
    ) -> bool;
    fn intel_hdmi_fastset_infoframes(
        encoder: *mut IntelEncoder,
        state: *const IntelCrtcState,
        conn_state: *const DrmConnectorState,
    );
    fn intel_hdmi_infoframes_enabled(
        encoder: *mut IntelEncoder,
        state: *const IntelCrtcState,
    ) -> u32;
    fn intel_dp_dual_mode_set_tmds_output(hdmi: *mut IntelHdmi, enable: bool);
    fn intel_dp_start_link_train(
        state: *mut IntelAtomicState,
        dp: *mut IntelDp,
        crtc_state: *mut IntelCrtcState,
    );
    fn intel_dp_stop_link_train(dp: *mut IntelDp, crtc_state: *mut IntelCrtcState);
    fn intel_dp_set_power(dp: *mut IntelDp, power: u8);
    fn intel_enable_transcoder(state: *const IntelCrtcState);
    fn intel_disable_transcoder(state: *const IntelCrtcState);
    fn intel_set_cpu_fifo_underrun_reporting(display: *mut IntelDisplay, pipe: Pipe, enable: bool);
    fn intel_hdmi_poll_for_scrambling_enable(
        state: *const IntelCrtcState,
        connector: *mut DrmConnector,
    );
    fn intel_dp_set_infoframes(
        encoder: *mut IntelEncoder,
        enable: bool,
        state: *mut IntelCrtcState,
        conn_state: *mut DrmConnectorState,
    );
    fn intel_dp_encoder_shutdown(encoder: *mut IntelEncoder);
    fn intel_dp_aux_ch(encoder: *mut IntelEncoder) -> i32;
    fn intel_dp_hpd_pulse(port: *mut IntelDigitalPort, long_hpd: bool) -> u32;
    fn intel_infoframe_init(port: *mut IntelDigitalPort);
    fn intel_tc_port_init(port: *mut IntelDigitalPort, legacy: bool) -> i32;
    fn intel_tc_port_cleanup(port: *mut IntelDigitalPort);
    fn intel_tc_port_connected(encoder: *mut IntelEncoder) -> bool;
    fn intel_tc_port_get_pin_assignment(port: *mut IntelDigitalPort) -> i32;
    fn intel_tc_port_init_mode(port: *mut IntelDigitalPort);
    fn intel_tc_port_link_reset(port: *mut IntelDigitalPort) -> bool;
    fn intel_tc_port_lock(port: *mut IntelDigitalPort);
    fn intel_tc_port_unlock(port: *mut IntelDigitalPort);
    fn intel_combo_phy_power_up_lanes(
        display: *mut IntelDisplay,
        phy: i32,
        is_dsi: bool,
        lane_count: i32,
        lane_reversal: bool,
    );
    fn intel_display_power_is_enabled(display: *mut IntelDisplay, domain: i32) -> bool;
    fn intel_dpll_get_hw_state(
        display: *mut IntelDisplay,
        pll: *mut IntelDpll,
        state: *mut IntelDpllHwState,
    ) -> bool;
    fn intel_dpll_get_freq(
        display: *mut IntelDisplay,
        pll: *const IntelDpll,
        state: *const IntelDpllHwState,
    ) -> i32;
    fn intel_dpll_update_active(
        state: *mut IntelAtomicState,
        crtc: *mut IntelCrtc,
        encoder: *mut IntelEncoder,
    );
    fn __intel_display_wa(
        display: *mut IntelDisplay,
        wa: crate::intel_display_wa_upstream::IntelDisplayWa,
        name: *const i8,
    ) -> bool;
    fn icl_set_active_port_dpll(state: *mut IntelCrtcState, id: u32);
    fn intel_display_power_aux_io_domain(display: *mut IntelDisplay, aux: i32) -> i32;
    fn intel_aux_power_domain(port: *mut IntelDigitalPort) -> i32;
    fn intel_psr_needs_aux_io_power(
        encoder: *mut IntelEncoder,
        state: *const IntelCrtcState,
    ) -> bool;
    fn intel_dp_sync_state(encoder: *mut IntelEncoder, state: *const IntelCrtcState);
    fn intel_tc_port_sanitize_mode(port: *mut IntelDigitalPort, state: *const IntelCrtcState);
    fn intel_get_dpll_by_id(display: *mut IntelDisplay, id: i32) -> *mut IntelDpll;
    fn icl_tc_port_to_pll_id(tc_port: i32) -> i32;
    fn intel_dkl_phy_write(display: *mut IntelDisplay, reg: IntelDklPhyReg, val: u32);
    fn intel_dkl_phy_read(display: *mut IntelDisplay, reg: IntelDklPhyReg) -> u32;
    fn intel_dkl_phy_rmw(display: *mut IntelDisplay, reg: IntelDklPhyReg, clear: u32, set: u32);
}

#[inline]
unsafe fn display_from_encoder(encoder: *mut IntelEncoder) -> *mut IntelDisplay {
    if encoder.is_null() || (*encoder).base.dev.is_null() {
        core::ptr::null_mut()
    } else {
        __drm_to_display((*encoder).base.dev)
    }
}

#[inline]
unsafe fn display_from_state(state: *const IntelCrtcState) -> *mut IntelDisplay {
    if state.is_null() || (*state).uapi.crtc.is_null() {
        core::ptr::null_mut()
    } else {
        __drm_to_display((*(*state).uapi.crtc).dev)
    }
}

#[inline]
unsafe fn dig_port(encoder: *mut IntelEncoder) -> *mut IntelDigitalPort {
    encoder.cast()
}

#[inline]
unsafe fn port_dp(encoder: *mut IntelEncoder) -> *mut IntelDp {
    &mut (*dig_port(encoder)).dp
}

#[inline]
unsafe fn output_is_hdmi(state: *const IntelCrtcState) -> bool {
    !state.is_null() && intel_crtc_has_type(state, INTEL_OUTPUT_HDMI)
}

fn encode_bpc(pipe_bpp: i32) -> u32 {
    match pipe_bpp {
        18 => TRANS_DDI_BPC_6,
        30 => TRANS_DDI_BPC_10,
        36 => TRANS_DDI_BPC_12,
        _ => TRANS_DDI_BPC_8,
    }
}

fn train_set_to_entry(signal: u8) -> usize {
    DP_SIGNAL_LEVELS
        .iter()
        .position(|entry| *entry == signal)
        .unwrap_or(0)
}

unsafe fn ddi_buf_trans(
    encoder: *mut IntelEncoder,
    state: *const IntelCrtcState,
    n_entries: &mut i32,
) -> *mut IntelDdiBufTrans {
    *n_entries = 0;
    if encoder.is_null() {
        return core::ptr::null_mut();
    }
    match (*encoder).get_buf_trans {
        Some(get) => get(encoder, state.cast_mut(), n_entries),
        None => core::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ddi_level(
    encoder: *mut IntelEncoder,
    state: *const IntelCrtcState,
    lane: i32,
) -> i32 {
    let mut n_entries = 0;
    let trans = ddi_buf_trans(encoder, state, &mut n_entries);
    if trans.is_null() || n_entries <= 0 || (*trans).entries.is_null() || (*trans).num_entries == 0
    {
        return 0;
    }

    let mut level = if output_is_hdmi(state) {
        let shift = intel_bios_hdmi_level_shift((*encoder).devdata);
        if shift < 0 {
            (*trans).hdmi_default_entry as i32
        } else {
            shift
        }
    } else {
        let dp = port_dp(encoder);
        let idx = lane.clamp(0, 3) as usize;
        train_set_to_entry(
            (*dp).train_set[idx] & (DP_TRAIN_VOLTAGE_SWING_MASK | DP_TRAIN_PRE_EMPHASIS_MASK),
        ) as i32
    };
    if level >= n_entries {
        level = n_entries - 1;
    }
    level.max(0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ddi_compute_min_voltage_level(state: *mut IntelCrtcState) {
    if state.is_null() {
        return;
    }
    let display = display_from_state(state);
    if display.is_null() {
        return;
    }
    if DISPLAY_VER(display) >= 12 {
        (*state).min_voltage_level = if (*state).port_clock > 594_000 { 2 } else { 0 };
    }
}

unsafe fn transcoder_func_ctl_value(
    encoder: *mut IntelEncoder,
    state: *const IntelCrtcState,
) -> u32 {
    let display = display_from_encoder(encoder);
    if encoder.is_null() || state.is_null() || display.is_null() {
        return 0;
    }
    let mut value = TRANS_DDI_FUNC_ENABLE | TGL_TRANS_DDI_SELECT_PORT((*encoder).port as u32);
    value |= encode_bpc((*state).pipe_bpp);
    if (*state).hw.adjusted_mode.flags & tk_drm_upstream::uapi_drm_consts::DRM_MODE_FLAG_PVSYNC != 0
    {
        value |= TRANS_DDI_PVSYNC;
    }
    if (*state).hw.adjusted_mode.flags & tk_drm_upstream::uapi_drm_consts::DRM_MODE_FLAG_PHSYNC != 0
    {
        value |= TRANS_DDI_PHSYNC;
    }

    if output_is_hdmi(state) {
        value |= if (*state).has_hdmi_sink {
            TRANS_DDI_MODE_SELECT_HDMI
        } else {
            TRANS_DDI_MODE_SELECT_DVI
        };
        if (*state).hdmi_scrambling {
            value |= TRANS_DDI_HDMI_SCRAMBLING;
        }
        if (*state).hdmi_high_tmds_clock_ratio {
            value |= TRANS_DDI_HIGH_TMDS_CHAR_RATE;
        }
    } else {
        value |= TRANS_DDI_MODE_SELECT_DP_SST | DDI_PORT_WIDTH((*state).lane_count as u32);
    }
    value
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ddi_enable_transcoder_func(
    encoder: *mut IntelEncoder,
    state: *const IntelCrtcState,
) {
    let display = display_from_state(state);
    if display.is_null() || encoder.is_null() || state.is_null() {
        return;
    }
    let tran = (*state).cpu_transcoder as u32;
    intel_de_write(
        display,
        TRANS_DDI_FUNC_CTL(display, tran),
        transcoder_func_ctl_value(encoder, state),
    );
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ddi_config_transcoder_func(
    encoder: *mut IntelEncoder,
    state: *const IntelCrtcState,
) {
    let display = display_from_state(state);
    if display.is_null() || encoder.is_null() || state.is_null() {
        return;
    }
    intel_de_write(
        display,
        TRANS_DDI_FUNC_CTL(display, (*state).cpu_transcoder as u32),
        transcoder_func_ctl_value(encoder, state) & !TRANS_DDI_FUNC_ENABLE,
    );
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ddi_disable_transcoder_func(state: *const IntelCrtcState) {
    let display = display_from_state(state);
    if display.is_null() || state.is_null() {
        return;
    }
    let tran = (*state).cpu_transcoder as u32;
    if DISPLAY_VER(display) >= 11 {
        intel_de_write(display, TRANS_DDI_FUNC_CTL2(display, tran), 0);
    }
    let reg = TRANS_DDI_FUNC_CTL(display, tran);
    let mut val = intel_de_read(display, reg) & !TRANS_DDI_FUNC_ENABLE;
    if DISPLAY_VER(display) >= 12 {
        val &= !(TGL_TRANS_DDI_PORT_MASK | TRANS_DDI_MODE_SELECT_MASK);
    }
    intel_de_write(display, reg, val);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ddi_enable_transcoder_clock(
    encoder: *mut IntelEncoder,
    state: *const IntelCrtcState,
) {
    let display = display_from_state(state);
    if display.is_null()
        || encoder.is_null()
        || state.is_null()
        || (*state).cpu_transcoder == TRANSCODER_EDP
    {
        return;
    }
    let phy = intel_encoder_to_phy(encoder);
    let select = if DISPLAY_VER(display) >= 13 {
        TGL_TRANS_CLK_SEL_PORT(phy)
    } else {
        TGL_TRANS_CLK_SEL_PORT((*encoder).port as u32)
    };
    intel_de_write(
        display,
        TRANS_CLK_SEL((*state).cpu_transcoder as u32),
        select,
    );
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ddi_disable_transcoder_clock(state: *const IntelCrtcState) {
    let display = display_from_state(state);
    if display.is_null() || state.is_null() || (*state).cpu_transcoder == TRANSCODER_EDP {
        return;
    }
    intel_de_write(
        display,
        TRANS_CLK_SEL((*state).cpu_transcoder as u32),
        TGL_TRANS_CLK_SEL_DISABLED,
    );
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ddi_get_clock(
    encoder: *mut IntelEncoder,
    state: *mut IntelCrtcState,
    pll: *mut IntelDpll,
) {
    let display = display_from_encoder(encoder);
    if display.is_null() || state.is_null() || pll.is_null() {
        return;
    }
    let id = (*encoder)
        .port_pll_type
        .map(|get_id| get_id(encoder, state))
        .unwrap_or(crate::intel_dpll_mgr_types_upstream::ICL_PORT_DPLL_DEFAULT);
    if id >= (*state).icl_port_dplls.len() as u32 {
        return;
    }
    let pd = &mut (*state).icl_port_dplls[id as usize];
    pd.pll = pll;
    let _active = intel_dpll_get_hw_state(display, pll, &mut pd.hw_state);
    icl_set_active_port_dpll(state, id);
    (*state).port_clock =
        intel_dpll_get_freq(display, (*state).intel_dpll, &(*state).dpll_hw_state);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ddi_port_pll_type(
    encoder: *mut IntelEncoder,
    state: *const IntelCrtcState,
) -> u32 {
    if encoder.is_null() || state.is_null() {
        return 0;
    }
    let pll = (*state).intel_dpll;
    if pll.is_null() || (*pll).info.is_null() {
        return 0;
    }
    if (*(*pll).info).id == crate::intel_dpll_mgr_types_upstream::DPLL_ID_ICL_TBTPLL {
        0
    } else {
        1
    }
}

unsafe extern "C" fn icl_ddi_tc_port_pll_type(
    encoder: *mut IntelEncoder,
    state: *mut IntelCrtcState,
) -> u32 {
    intel_ddi_port_pll_type(encoder, state)
}

unsafe fn ddi_lock_plls(display: *mut IntelDisplay) {
    tk_linux_kapi::mutex::mutex_lock(ptr::addr_of_mut!((*display).dpll.lock));
}

unsafe fn ddi_unlock_plls(display: *mut IntelDisplay) {
    tk_linux_kapi::mutex::mutex_unlock(ptr::addr_of_mut!((*display).dpll.lock));
}

unsafe fn icl_ddi_enable_clock_mux(
    display: *mut IntelDisplay,
    reg: I915RegT,
    sel_mask: u32,
    sel: u32,
    off: u32,
) {
    ddi_lock_plls(display);
    intel_de_rmw(display, reg, sel_mask, sel);
    // Linux requires selection and ungating in separate register writes.
    intel_de_rmw(display, reg, off, 0);
    ddi_unlock_plls(display);
}

unsafe fn icl_ddi_disable_clock_mux(display: *mut IntelDisplay, reg: I915RegT, off: u32) {
    ddi_lock_plls(display);
    intel_de_rmw(display, reg, 0, off);
    ddi_unlock_plls(display);
}

unsafe extern "C" fn icl_combo_enable_clock(
    encoder: *mut IntelEncoder,
    state: *mut IntelCrtcState,
) {
    if encoder.is_null() || state.is_null() || (*state).intel_dpll.is_null() {
        return;
    }
    let display = display_from_encoder(encoder);
    if display.is_null() {
        return;
    }
    let phy = intel_encoder_to_phy(encoder);
    let pll_id = (*(*(*state).intel_dpll).info).id as u32;
    icl_ddi_enable_clock_mux(
        display,
        ICL_DPCLKA_CFGCR0,
        ICL_DPCLKA_CFGCR0_DDI_CLK_SEL_MASK(phy),
        ICL_DPCLKA_CFGCR0_DDI_CLK_SEL(pll_id, phy),
        ICL_DPCLKA_CFGCR0_DDI_CLK_OFF(phy),
    );
}

unsafe extern "C" fn icl_combo_disable_clock(encoder: *mut IntelEncoder) {
    if encoder.is_null() {
        return;
    }
    let display = display_from_encoder(encoder);
    if display.is_null() {
        return;
    }
    let phy = intel_encoder_to_phy(encoder);
    icl_ddi_disable_clock_mux(
        display,
        ICL_DPCLKA_CFGCR0,
        ICL_DPCLKA_CFGCR0_DDI_CLK_OFF(phy),
    );
}

unsafe extern "C" fn icl_combo_is_clock_enabled(encoder: *mut IntelEncoder) -> bool {
    if encoder.is_null() {
        return false;
    }
    let display = display_from_encoder(encoder);
    !display.is_null()
        && intel_de_read(display, ICL_DPCLKA_CFGCR0)
            & ICL_DPCLKA_CFGCR0_DDI_CLK_OFF(intel_encoder_to_phy(encoder))
            == 0
}

fn tc_pll_to_ddi_clk_sel(pll_id: i32, port_clock: i32) -> u32 {
    if pll_id != crate::intel_dpll_mgr_types_upstream::DPLL_ID_ICL_TBTPLL {
        return DDI_CLK_SEL_MG;
    }
    match port_clock {
        162_000 => DDI_CLK_SEL_TBT_162,
        270_000 => DDI_CLK_SEL_TBT_270,
        540_000 => DDI_CLK_SEL_TBT_540,
        810_000 => DDI_CLK_SEL_TBT_810,
        _ => DDI_CLK_SEL_NONE,
    }
}

fn xelpd_hpd_pin(port: Port) -> u32 {
    if port >= crate::intel_display_limits_types_upstream::PORT_D_XELPD {
        (crate::intel_display_limits_types_upstream::HPD_PORT_D as i32 + port
            - crate::intel_display_limits_types_upstream::PORT_D_XELPD) as u32
    } else if port >= crate::intel_display_limits_types_upstream::PORT_TC1 {
        (crate::intel_display_limits_types_upstream::HPD_PORT_TC1 as i32 + port
            - crate::intel_display_limits_types_upstream::PORT_TC1) as u32
    } else {
        (crate::intel_display_limits_types_upstream::HPD_PORT_A as i32 + port
            - crate::intel_display_limits_types_upstream::PORT_A) as u32
    }
}

unsafe extern "C" fn icl_tc_enable_clock(encoder: *mut IntelEncoder, state: *mut IntelCrtcState) {
    if encoder.is_null() || state.is_null() || (*state).intel_dpll.is_null() {
        return;
    }
    let display = display_from_encoder(encoder);
    if display.is_null() {
        return;
    }
    let port = (*encoder).port;
    let tc = intel_encoder_to_tc(encoder);
    let pll_id = (*(*(*state).intel_dpll).info).id;
    intel_de_write(
        display,
        DDI_CLK_SEL(port as u32),
        tc_pll_to_ddi_clk_sel(pll_id, (*state).port_clock),
    );
    icl_ddi_enable_clock_mux(
        display,
        ICL_DPCLKA_CFGCR0,
        ICL_DPCLKA_CFGCR0_TC_CLK_OFF(tc),
        0,
        ICL_DPCLKA_CFGCR0_TC_CLK_OFF(tc),
    );
}

unsafe extern "C" fn icl_tc_disable_clock(encoder: *mut IntelEncoder) {
    if encoder.is_null() {
        return;
    }
    let display = display_from_encoder(encoder);
    if display.is_null() {
        return;
    }
    let tc = intel_encoder_to_tc(encoder);
    icl_ddi_disable_clock_mux(display, ICL_DPCLKA_CFGCR0, ICL_DPCLKA_CFGCR0_TC_CLK_OFF(tc));
    intel_de_write(
        display,
        DDI_CLK_SEL((*encoder).port as u32),
        DDI_CLK_SEL_NONE,
    );
}

unsafe extern "C" fn icl_tc_is_clock_enabled(encoder: *mut IntelEncoder) -> bool {
    if encoder.is_null() {
        return false;
    }
    let display = display_from_encoder(encoder);
    if display.is_null() {
        return false;
    }
    let port_val = intel_de_read(display, DDI_CLK_SEL((*encoder).port as u32)) & DDI_CLK_SEL_MASK;
    port_val != DDI_CLK_SEL_NONE
        && intel_de_read(display, ICL_DPCLKA_CFGCR0)
            & ICL_DPCLKA_CFGCR0_TC_CLK_OFF(intel_encoder_to_tc(encoder))
            == 0
}

unsafe fn icl_combo_get_pll(encoder: *mut IntelEncoder) -> *mut IntelDpll {
    let display = display_from_encoder(encoder);
    if display.is_null() || encoder.is_null() {
        return core::ptr::null_mut();
    }
    let phy = intel_encoder_to_phy(encoder);
    let raw = (intel_de_read(display, ICL_DPCLKA_CFGCR0) & ICL_DPCLKA_CFGCR0_DDI_CLK_SEL_MASK(phy))
        >> ICL_DPCLKA_CFGCR0_DDI_CLK_SEL_SHIFT(phy);
    if raw > 1 {
        return core::ptr::null_mut();
    }
    intel_get_dpll_by_id(display, raw as i32)
}

unsafe fn icl_tc_get_pll(encoder: *mut IntelEncoder) -> *mut IntelDpll {
    if encoder.is_null() {
        return core::ptr::null_mut();
    }
    let display = display_from_encoder(encoder);
    if display.is_null() {
        return core::ptr::null_mut();
    }
    let sel = intel_de_read(display, DDI_CLK_SEL((*encoder).port as u32)) & DDI_CLK_SEL_MASK;
    let id = if sel == DDI_CLK_SEL_MG {
        icl_tc_port_to_pll_id(intel_encoder_to_tc(encoder) as i32)
    } else if sel == DDI_CLK_SEL_TBT_162
        || sel == DDI_CLK_SEL_TBT_270
        || sel == DDI_CLK_SEL_TBT_540
        || sel == DDI_CLK_SEL_TBT_810
    {
        crate::intel_dpll_mgr_types_upstream::DPLL_ID_ICL_TBTPLL
    } else {
        return core::ptr::null_mut();
    };
    intel_get_dpll_by_id(display, id)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ddi_update_active_dpll(
    state: *mut IntelAtomicState,
    encoder: *mut IntelEncoder,
    crtc: *mut IntelCrtc,
) {
    if state.is_null() || encoder.is_null() || crtc.is_null() || !intel_encoder_is_tc(encoder) {
        return;
    }
    intel_dpll_update_active(state, crtc, encoder);
}

unsafe fn icl_program_mg_dp_mode(port: *mut IntelDigitalPort, state: *const IntelCrtcState) {
    let encoder = &mut (*port).base;
    let display = display_from_encoder(encoder);
    if display.is_null()
        || DISPLAY_VER(display) >= 14
        || !intel_encoder_is_tc(encoder)
        || intel_tc_port_in_tbt_alt_mode(port)
    {
        return;
    }

    let tc = intel_encoder_to_tc(encoder);
    let mut ln0 = intel_dkl_phy_read(display, DKL_DP_MODE(tc, 0));
    let mut ln1 = intel_dkl_phy_read(display, DKL_DP_MODE(tc, 1));
    ln0 &= !(MG_DP_MODE_CFG_DP_X1_MODE | MG_DP_MODE_CFG_DP_X2_MODE);
    ln1 &= !(MG_DP_MODE_CFG_DP_X1_MODE | MG_DP_MODE_CFG_DP_X2_MODE);
    let lanes = (*state).lane_count;
    let assignment = intel_tc_port_get_pin_assignment(port);
    match assignment {
        0 => {
            if !intel_tc_port_in_legacy_mode(port) {
                return;
            }
            if lanes == 1 {
                ln1 |= MG_DP_MODE_CFG_DP_X1_MODE;
            } else {
                ln0 |= MG_DP_MODE_CFG_DP_X2_MODE;
                ln1 |= MG_DP_MODE_CFG_DP_X2_MODE;
            }
        }
        1 => {
            if lanes == 4 {
                ln0 |= MG_DP_MODE_CFG_DP_X2_MODE;
                ln1 |= MG_DP_MODE_CFG_DP_X2_MODE;
            }
        }
        2 => {
            if lanes == 2 {
                ln0 |= MG_DP_MODE_CFG_DP_X2_MODE;
                ln1 |= MG_DP_MODE_CFG_DP_X2_MODE;
            }
        }
        3 | 4 | 5 | 6 => {
            if lanes == 1 {
                ln0 |= MG_DP_MODE_CFG_DP_X1_MODE;
                ln1 |= MG_DP_MODE_CFG_DP_X1_MODE;
            } else {
                ln0 |= MG_DP_MODE_CFG_DP_X2_MODE;
                ln1 |= MG_DP_MODE_CFG_DP_X2_MODE;
            }
        }
        _ => return,
    }
    intel_dkl_phy_write(display, DKL_DP_MODE(tc, 0), ln0);
    intel_dkl_phy_write(display, DKL_DP_MODE(tc, 1), ln1);
    // Wa_14018221282: ADL-P display 13 PHY mode writeback may be lost.
    for lane in 0..2 {
        let expected = if lane == 0 { ln0 } else { ln1 };
        let reg = DKL_DP_MODE(tc, lane);
        if intel_dkl_phy_read(display, reg) != expected {
            intel_dkl_phy_write(display, reg, expected);
        }
    }
}

unsafe fn ddi_dp_tp_ctl_reg(encoder: *mut IntelEncoder, state: *const IntelCrtcState) -> I915RegT {
    let display = display_from_encoder(encoder);
    TGL_DP_TP_CTL(display, (*state).cpu_transcoder as u32)
}

unsafe fn ddi_dp_tp_status_reg(
    encoder: *mut IntelEncoder,
    state: *const IntelCrtcState,
) -> I915RegT {
    let display = display_from_encoder(encoder);
    TGL_DP_TP_STATUS(display, (*state).cpu_transcoder as u32)
}

unsafe extern "C" fn intel_ddi_prepare_link_retrain(dp: *mut IntelDp, state: *mut IntelCrtcState) {
    if dp.is_null() || state.is_null() {
        return;
    }
    let port = (dp as *mut u8)
        .sub(core::mem::offset_of!(IntelDigitalPort, dp))
        .cast::<IntelDigitalPort>();
    let encoder = &mut (*port).base;
    let display = display_from_encoder(encoder);
    if display.is_null() {
        return;
    }
    let mut ctl = DP_TP_CTL_ENABLE | DP_TP_CTL_LINK_TRAIN_PAT1 | DP_TP_CTL_MODE_SST;
    if (*state).enhanced_framing {
        ctl |= DP_TP_CTL_ENHANCED_FRAME_ENABLE;
    }
    intel_de_write(display, ddi_dp_tp_ctl_reg(encoder, state), ctl);
    intel_de_posting_read(display, ddi_dp_tp_ctl_reg(encoder, state));
    if (*display).platform._anon_0_0.bitmap[0] & (1u64 << 52) != 0
        && intel_encoder_is_tc(encoder)
        && (intel_tc_port_in_dp_alt_mode(port) || intel_tc_port_in_legacy_mode(port))
    {
        let tc = intel_encoder_to_tc(encoder);
        for lane in 0..2 {
            intel_dkl_phy_rmw(
                display,
                DKL_PCS_DW5(tc, lane),
                DKL_PCS_DW5_CORE_SOFTRESET,
                0,
            );
        }
    }
    ddi_buf_enable(encoder, (*dp).DP);
    (*dp).DP |= DDI_BUF_CTL_ENABLE;
}

unsafe extern "C" fn intel_ddi_set_link_train(
    dp: *mut IntelDp,
    state: *mut IntelCrtcState,
    pattern: u8,
) {
    if dp.is_null() || state.is_null() {
        return;
    }
    let port = (dp as *mut u8)
        .sub(core::mem::offset_of!(IntelDigitalPort, dp))
        .cast::<IntelDigitalPort>();
    let encoder = &mut (*port).base;
    let display = display_from_encoder(encoder);
    if display.is_null() {
        return;
    }
    let value = match pattern {
        0 => DP_TP_CTL_LINK_TRAIN_NORMAL,
        1 => DP_TP_CTL_LINK_TRAIN_PAT1,
        2 => DP_TP_CTL_LINK_TRAIN_PAT2,
        3 => DP_TP_CTL_LINK_TRAIN_PAT3,
        4 => DP_TP_CTL_LINK_TRAIN_PAT4,
        _ => return,
    };
    intel_de_rmw(
        display,
        ddi_dp_tp_ctl_reg(encoder, state),
        DP_TP_CTL_LINK_TRAIN_MASK,
        value,
    );
}

unsafe extern "C" fn intel_ddi_set_idle_link_train(dp: *mut IntelDp, state: *mut IntelCrtcState) {
    if dp.is_null() || state.is_null() {
        return;
    }
    let port = (dp as *mut u8)
        .sub(core::mem::offset_of!(IntelDigitalPort, dp))
        .cast::<IntelDigitalPort>();
    let encoder = &mut (*port).base;
    let display = display_from_encoder(encoder);
    if display.is_null() {
        return;
    }
    let reg = ddi_dp_tp_ctl_reg(encoder, state);
    intel_de_rmw(
        display,
        reg,
        DP_TP_CTL_LINK_TRAIN_MASK,
        DP_TP_CTL_LINK_TRAIN_IDLE,
    );
    let _ = intel_de_wait_for_set_ms(
        display,
        ddi_dp_tp_status_reg(encoder, state),
        DP_TP_STATUS_IDLE_DONE,
        2,
    );
}

unsafe extern "C" fn intel_ddi_dp_voltage_max(dp: *mut IntelDp, state: *mut IntelCrtcState) -> u8 {
    if dp.is_null() {
        return 0;
    }
    let port = (dp as *mut u8)
        .sub(core::mem::offset_of!(IntelDigitalPort, dp))
        .cast::<IntelDigitalPort>();
    let encoder = &mut (*port).base;
    let mut n_entries = 0;
    let trans = ddi_buf_trans(encoder, state, &mut n_entries);
    if trans.is_null() || n_entries <= 0 || (*trans).entries.is_null() || (*trans).num_entries == 0
    {
        return 0;
    }
    DP_SIGNAL_LEVELS[(n_entries as usize).min(MAX_DDI_SIGNAL_LEVELS) - 1]
        & DP_TRAIN_VOLTAGE_SWING_MASK
}

unsafe extern "C" fn intel_ddi_dp_preemph_max(_dp: *mut IntelDp) -> u8 {
    3 << 3
}

unsafe fn ddi_init_dp_buf_reg(encoder: *mut IntelEncoder, state: *const IntelCrtcState) {
    let port = dig_port(encoder);
    let display = display_from_encoder(encoder);
    if port.is_null() || display.is_null() || state.is_null() {
        return;
    }
    let mut ctl = DDI_PORT_WIDTH((*state).lane_count as u32) | DDI_BUF_TRANS_SELECT(0);
    if (*port).lane_reversal {
        ctl |= DDI_BUF_PORT_REVERSAL;
    }
    if (*port).ddi_a_4_lanes {
        ctl |= DDI_A_4_LANES;
    }
    if (*display).platform._anon_0_0.bitmap[0] & (1u64 << 52) != 0 && intel_encoder_is_tc(encoder) {
        let rate_code = match (*state).port_clock {
            162_000 => 0,
            216_000 => 4,
            243_000 => 5,
            270_000 => 1,
            324_000 => 6,
            432_000 => 7,
            540_000 => 2,
            810_000 => 3,
            _ => 0,
        };
        ctl |= DDI_BUF_PHY_LINK_RATE(rate_code);
        if !intel_tc_port_in_tbt_alt_mode(port) {
            ctl |= DDI_BUF_CTL_TC_PHY_OWNERSHIP;
        }
    }
    (*port).dp.DP = ctl;
}

unsafe fn ddi_power_up_lanes(encoder: *mut IntelEncoder, state: *const IntelCrtcState) {
    if encoder.is_null() || state.is_null() || !intel_encoder_is_combo(encoder) {
        return;
    }
    let port = dig_port(encoder);
    intel_combo_phy_power_up_lanes(
        display_from_encoder(encoder),
        intel_encoder_to_phy(encoder) as i32,
        false,
        (*state).lane_count as i32,
        (*port).lane_reversal,
    );
}

unsafe fn intel_dp_sink_set_fec_ready(
    dp: *mut IntelDp,
    state: *const IntelCrtcState,
    enable: bool,
) {
    if dp.is_null() || state.is_null() || !(*state).fec_enable {
        return;
    }
    let value = if enable { DP_FEC_READY } else { 0 };
    let _ = tk_drm_upstream::drm_dp_helper_upstream::drm_dp_dpcd_writeb(
        ptr::addr_of_mut!((*dp).aux),
        DP_FEC_CONFIGURATION,
        value,
    );
    if enable {
        let _ = tk_drm_upstream::drm_dp_helper_upstream::drm_dp_dpcd_writeb(
            ptr::addr_of_mut!((*dp).aux),
            DP_FEC_STATUS,
            DP_FEC_DECODE_EN_DETECTED | DP_FEC_DECODE_DIS_DETECTED,
        );
    }
}

unsafe fn intel_ddi_enable_fec(encoder: *mut IntelEncoder, state: *const IntelCrtcState) {
    if encoder.is_null() || state.is_null() || !(*state).fec_enable {
        return;
    }
    let display = display_from_encoder(encoder);
    if !display.is_null() {
        intel_de_rmw(
            display,
            ddi_dp_tp_ctl_reg(encoder, state),
            0,
            DP_TP_CTL_FEC_ENABLE,
        );
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ddi_wait_for_fec_status(
    encoder: *mut IntelEncoder,
    state: *const IntelCrtcState,
    enabled: bool,
) -> i32 {
    if encoder.is_null() || state.is_null() || !(*state).fec_enable {
        return 0;
    }
    let display = display_from_encoder(encoder);
    if display.is_null() {
        return -22;
    }
    let status_reg = ddi_dp_tp_status_reg(encoder, state);
    let ret = if enabled {
        intel_de_wait_for_set_ms(display, status_reg, DP_TP_STATUS_FEC_ENABLE_LIVE, 1)
    } else {
        intel_de_wait_for_clear_ms(display, status_reg, DP_TP_STATUS_FEC_ENABLE_LIVE, 1)
    };
    if ret != 0 || !enabled {
        return ret;
    }

    // Linux waits up to 200 ms for the sink to report FEC decode enabled.
    let dp = port_dp(encoder);
    for _ in 0..20_000 {
        let mut fec_status = 0u8;
        let err = tk_drm_upstream::drm_dp_helper_upstream::drm_dp_dpcd_readb(
            ptr::addr_of_mut!((*dp).aux),
            DP_FEC_STATUS,
            &mut fec_status,
        );
        if err < 0 {
            return err as i32;
        }
        if err > 0 && fec_status & DP_FEC_DECODE_EN_DETECTED != 0 {
            return 0;
        }
        tk_linux_kapi::primitives::udelay(10);
    }
    -110 // -ETIMEDOUT
}

unsafe fn ddi_pre_enable_dp(
    state: *mut IntelAtomicState,
    encoder: *mut IntelEncoder,
    crtc_state: *mut IntelCrtcState,
    conn_state: *mut DrmConnectorState,
) {
    if intel_crtc_has_type(
        crtc_state,
        crate::intel_display_types_upstream::INTEL_OUTPUT_DP_MST,
    ) {
        return;
    }
    let port = dig_port(encoder);
    let display = display_from_encoder(encoder);
    if display.is_null() {
        return;
    }
    let dp = port_dp(encoder);
    intel_dp_set_link_params(dp, (*crtc_state).port_clock, (*crtc_state).lane_count);
    ddi_init_dp_buf_reg(encoder, crtc_state);
    intel_ddi_enable_clock(encoder, crtc_state);
    if !intel_tc_port_in_tbt_alt_mode(port) {
        ddi_io_get(port, display);
    }
    icl_program_mg_dp_mode(port, crtc_state);
    intel_ddi_enable_transcoder_clock(encoder, crtc_state);
    intel_ddi_config_transcoder_func(encoder, crtc_state);
    if let Some(set_levels) = (*encoder).set_signal_levels {
        set_levels(encoder, crtc_state);
    }
    ddi_power_up_lanes(encoder, crtc_state);
    intel_dp_set_power(dp, 1); // DP_SET_POWER_D0
    if !conn_state.is_null() && !(*conn_state).connector.is_null() {
        intel_dp_sink_enable_decompression(
            state,
            (*conn_state).connector.cast::<IntelConnector>(),
            crtc_state,
        );
    }
    intel_dp_sink_set_fec_ready(dp, crtc_state, true);
    intel_dp_start_link_train(state, dp, crtc_state);
    intel_dp_stop_link_train(dp, crtc_state);
    intel_ddi_enable_fec(encoder, crtc_state);
    intel_dsc_dp_pps_write(encoder, crtc_state);
    let _ = conn_state; // DP MST, DSC, and sink decompression are intentionally not enabled here.
}

unsafe extern "C" fn icl_combo_phy_set_signal_levels(
    encoder: *mut IntelEncoder,
    state: *mut IntelCrtcState,
) {
    let display = display_from_encoder(encoder);
    if display.is_null() || state.is_null() {
        return;
    }
    let phy = intel_encoder_to_phy(encoder);
    let mut n_entries = 0;
    let trans = ddi_buf_trans(encoder, state, &mut n_entries);
    if trans.is_null() || (*trans).entries.is_null() || (*trans).num_entries == 0 {
        return;
    }

    let pcs = ICL_PORT_PCS_DW1_LN(0, phy);
    let mut val = intel_de_read(display, pcs);
    if output_is_hdmi(state) {
        val &= !COMMON_KEEPER_EN;
    } else {
        val |= COMMON_KEEPER_EN;
    }
    intel_de_write(display, ICL_PORT_PCS_DW1_GRP(phy), val);

    for lane in 0..4u32 {
        let loadgen = (*state).port_clock <= 600_000
            && if (*state).lane_count == 4 {
                lane >= 1
            } else {
                lane == 1 || lane == 2
            };
        intel_de_rmw(
            display,
            ICL_PORT_TX_DW4_LN(lane, phy),
            LOADGEN_SELECT,
            if loadgen { LOADGEN_SELECT } else { 0 },
        );
    }
    intel_de_rmw(display, ICL_PORT_CL_DW5(phy), 0, SUS_CLOCK_CONFIG);

    let mut dw5 = intel_de_read(display, ICL_PORT_TX_DW5_LN(0, phy));
    dw5 &= !(SCALING_MODE_SEL_MASK
        | RTERM_SELECT_MASK
        | COEFF_POLARITY
        | CURSOR_PROGRAM
        | TAP2_DISABLE
        | TAP3_DISABLE);
    dw5 |= SCALING_MODE_SEL(2) | RTERM_SELECT(6) | TAP3_DISABLE;
    intel_de_write(display, ICL_PORT_TX_DW5_GRP(phy), dw5);

    let training = ICL_PORT_TX_DW5_LN(0, phy);
    let mut train = intel_de_read(display, training) & !TX_TRAINING_EN;
    intel_de_write(display, ICL_PORT_TX_DW5_GRP(phy), train);
    let trans = &*trans;
    for lane in 0..4u32 {
        let level = intel_ddi_level(encoder, state, lane as i32) as usize;
        let entry = &*(*trans)
            .entries
            .add(level.min((*trans).num_entries.saturating_sub(1) as usize));
        let icl = &*core::ptr::addr_of!((*entry).icl)
            .cast::<crate::intel_ddi_buf_trans_types_upstream::IclDdiBufTrans>();
        intel_de_rmw(
            display,
            ICL_PORT_TX_DW2_LN(lane, phy),
            SWING_SEL_UPPER_MASK | SWING_SEL_LOWER_MASK | RCOMP_SCALAR_MASK,
            SWING_SEL_UPPER(icl.dw2_swing_sel as u32)
                | SWING_SEL_LOWER(icl.dw2_swing_sel as u32)
                | RCOMP_SCALAR(0x98),
        );
        intel_de_rmw(
            display,
            ICL_PORT_TX_DW4_LN(lane, phy),
            POST_CURSOR_1_MASK | POST_CURSOR_2_MASK | CURSOR_COEFF_MASK,
            POST_CURSOR_1(icl.dw4_post_cursor_1 as u32)
                | POST_CURSOR_2(icl.dw4_post_cursor_2 as u32)
                | CURSOR_COEFF(icl.dw4_cursor_coeff as u32),
        );
        intel_de_rmw(
            display,
            ICL_PORT_TX_DW7_LN(lane, phy),
            N_SCALAR_MASK,
            N_SCALAR(icl.dw7_n_scalar as u32),
        );
    }
    train |= TX_TRAINING_EN;
    intel_de_write(display, ICL_PORT_TX_DW5_GRP(phy), train);
}

unsafe extern "C" fn tgl_dkl_phy_set_signal_levels(
    encoder: *mut IntelEncoder,
    state: *mut IntelCrtcState,
) {
    let display = display_from_encoder(encoder);
    if display.is_null() || state.is_null() {
        return;
    }
    let port = dig_port(encoder);
    if intel_tc_port_in_tbt_alt_mode(port) {
        return;
    }
    let mut n_entries = 0;
    let trans = ddi_buf_trans(encoder, state, &mut n_entries);
    if trans.is_null() || (*trans).entries.is_null() || (*trans).num_entries == 0 {
        return;
    }
    let tc = intel_encoder_to_tc(encoder);
    for lane in 0..2u32 {
        let reg0 = DKL_TX_DPCNTL0(tc, lane);
        let reg1 = DKL_TX_DPCNTL1(tc, lane);
        let reg2 = DKL_TX_DPCNTL2(tc, lane);
        if __intel_display_wa(
            display,
            crate::intel_display_wa_upstream::IntelDisplayWa::Wa16011342517,
            core::ptr::null(),
        ) {
            let disable_loadgen_sharing = (output_is_hdmi(state) && (*state).port_clock == 594_000)
                || (!output_is_hdmi(state) && (*state).port_clock == 162_000);
            intel_dkl_phy_rmw(
                display,
                reg2,
                LOADGEN_SHARING_PMD_DISABLE,
                if disable_loadgen_sharing {
                    LOADGEN_SHARING_PMD_DISABLE
                } else {
                    0
                },
            );
        }
        intel_dkl_phy_write(display, DKL_TX_PMD_LANE_SUS(tc, lane), 0);
        for (reg, ddi_lane) in [(reg0, lane as i32 * 2), (reg1, lane as i32 * 2 + 1)] {
            let level = intel_ddi_level(encoder, state, ddi_lane) as usize;
            let entry = &*(*trans)
                .entries
                .add(level.min((*trans).num_entries.saturating_sub(1) as usize));
            let dkl = &*core::ptr::addr_of!((*entry).dkl)
                .cast::<crate::intel_ddi_buf_trans_types_upstream::TglDklPhyDdiBufTrans>();
            intel_dkl_phy_rmw(
                display,
                reg,
                DKL_TX_PRESHOOT_COEFF_MASK
                    | DKL_TX_DE_EMPAHSIS_COEFF_MASK
                    | DKL_TX_VSWING_CONTROL_MASK,
                DKL_TX_PRESHOOT_COEFF(dkl.preshoot as u32)
                    | DKL_TX_DE_EMPHASIS_COEFF(dkl.de_emphasis as u32)
                    | DKL_TX_VSWING_CONTROL(dkl.vswing as u32),
            );
        }
        intel_dkl_phy_rmw(display, reg2, DKL_TX_DP20BITMODE, 0);
        if (*display).platform._anon_0_0.bitmap[0] & (1u64 << 52) != 0 {
            let loadgen = if output_is_hdmi(state) {
                if lane == 0 {
                    DKL_TX_DPCNTL2_CFG_LOADGENSELECT_TX1(0)
                        | DKL_TX_DPCNTL2_CFG_LOADGENSELECT_TX2(2)
                } else {
                    DKL_TX_DPCNTL2_CFG_LOADGENSELECT_TX1(3)
                        | DKL_TX_DPCNTL2_CFG_LOADGENSELECT_TX2(3)
                }
            } else {
                0
            };
            intel_dkl_phy_rmw(
                display,
                reg2,
                DKL_TX_DPCNTL2_CFG_LOADGENSELECT_TX1_MASK
                    | DKL_TX_DPCNTL2_CFG_LOADGENSELECT_TX2_MASK,
                loadgen,
            );
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ddi_compute_config(
    encoder: *mut IntelEncoder,
    state: *mut IntelCrtcState,
    conn_state: *mut DrmConnectorState,
) -> i32 {
    if encoder.is_null() || state.is_null() {
        return -22;
    }
    // ADL-P exports SST DP and HDMI from this module; MST stream encoders are
    // deliberately not advertised until their lifecycle and payload path land.
    if intel_crtc_has_type(
        state,
        crate::intel_display_types_upstream::INTEL_OUTPUT_DP_MST,
    ) {
        return -95;
    }
    let ret = if output_is_hdmi(state) {
        (*state).has_hdmi_sink = intel_hdmi_compute_has_hdmi_sink(encoder, state, conn_state);
        intel_hdmi_compute_config(encoder, state, conn_state)
    } else {
        intel_dp_compute_config(encoder, state, conn_state)
    };
    if ret == 0 {
        intel_ddi_compute_min_voltage_level(state);
    }
    ret
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ddi_get_config(
    encoder: *mut IntelEncoder,
    state: *mut IntelCrtcState,
) {
    if encoder.is_null() || state.is_null() {
        return;
    }
    let display = display_from_encoder(encoder);
    if display.is_null() {
        return;
    }
    let pll = if intel_encoder_is_tc(encoder) {
        icl_tc_get_pll(encoder)
    } else {
        icl_combo_get_pll(encoder)
    };
    if !pll.is_null() {
        intel_ddi_get_clock(encoder, state, pll);
    }
    let ctl = intel_de_read(
        display,
        TRANS_DDI_FUNC_CTL(display, (*state).cpu_transcoder as u32),
    );
    let mode = ctl & TRANS_DDI_MODE_SELECT_MASK;
    if ctl & TRANS_DDI_FUNC_ENABLE == 0 {
        return;
    }
    if ctl & TRANS_DDI_PHSYNC != 0 {
        (*state).hw.adjusted_mode.flags |= tk_drm_upstream::uapi_drm_consts::DRM_MODE_FLAG_PHSYNC;
    } else {
        (*state).hw.adjusted_mode.flags |= tk_drm_upstream::uapi_drm_consts::DRM_MODE_FLAG_NHSYNC;
    }
    if ctl & TRANS_DDI_PVSYNC != 0 {
        (*state).hw.adjusted_mode.flags |= tk_drm_upstream::uapi_drm_consts::DRM_MODE_FLAG_PVSYNC;
    } else {
        (*state).hw.adjusted_mode.flags |= tk_drm_upstream::uapi_drm_consts::DRM_MODE_FLAG_NVSYNC;
    }
    (*state).pipe_bpp = match ctl & TRANS_DDI_BPC_MASK {
        TRANS_DDI_BPC_6 => 18,
        TRANS_DDI_BPC_10 => 30,
        TRANS_DDI_BPC_12 => 36,
        _ => 24,
    };
    if mode == TRANS_DDI_MODE_SELECT_HDMI || mode == TRANS_DDI_MODE_SELECT_DVI {
        (*state).output_types = 1u32 << INTEL_OUTPUT_HDMI;
        (*state).has_hdmi_sink = mode == TRANS_DDI_MODE_SELECT_HDMI;
        (*state).hdmi_scrambling = ctl & TRANS_DDI_HDMI_SCRAMBLING != 0;
        (*state).hdmi_high_tmds_clock_ratio = ctl & TRANS_DDI_HIGH_TMDS_CHAR_RATE != 0;
        (*state).lane_count = 4;
        if mode == TRANS_DDI_MODE_SELECT_HDMI {
            (*state).infoframes.enable |= intel_hdmi_infoframes_enabled(encoder, state);
            (*state).has_infoframe = (*state).infoframes.enable != 0;
        }
    } else if mode == TRANS_DDI_MODE_SELECT_DP_SST {
        (*state).output_types = 1u32 << INTEL_OUTPUT_DP;
        (*state).lane_count =
            (((ctl & TRANS_DDI_PORT_WIDTH_MASK) >> TRANS_DDI_PORT_WIDTH_SHIFT) + 1) as u8;
        let dp_tp_ctl = intel_de_read(display, ddi_dp_tp_ctl_reg(encoder, state));
        (*state).enhanced_framing = dp_tp_ctl & DP_TP_CTL_ENHANCED_FRAME_ENABLE != 0;
        (*state).fec_enable = dp_tp_ctl & DP_TP_CTL_FEC_ENABLE != 0;
        (*state).infoframes.enable |= intel_hdmi_infoframes_enabled(encoder, state);
        (*state).has_infoframe = (*state).infoframes.enable != 0;
    }
    intel_ddi_compute_min_voltage_level(state);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ddi_get_hw_state(
    encoder: *mut IntelEncoder,
    pipe: *mut Pipe,
) -> bool {
    if encoder.is_null() || pipe.is_null() {
        return false;
    }
    let display = display_from_encoder(encoder);
    if display.is_null() {
        return false;
    }
    let buf = intel_de_read(display, DDI_BUF_CTL((*encoder).port as u32));
    if buf & DDI_BUF_CTL_ENABLE == 0 {
        return false;
    }
    for p in 0..4u32 {
        let reg = TRANS_DDI_FUNC_CTL(display, p);
        let ctl = intel_de_read(display, reg);
        let selected =
            ctl & TGL_TRANS_DDI_PORT_MASK == TGL_TRANS_DDI_SELECT_PORT((*encoder).port as u32);
        if selected && ctl & TRANS_DDI_FUNC_ENABLE != 0 {
            *pipe = p as Pipe;
            return true;
        }
    }
    false
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ddi_enable_clock(
    encoder: *mut IntelEncoder,
    state: *mut IntelCrtcState,
) {
    if encoder.is_null() || state.is_null() {
        return;
    }
    if let Some(enable) = (*encoder).enable_clock {
        enable(encoder, state);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ddi_disable_clock(encoder: *mut IntelEncoder) {
    if encoder.is_null() {
        return;
    }
    if let Some(disable) = (*encoder).disable_clock {
        disable(encoder);
    }
}

unsafe fn ddi_io_get(port: *mut IntelDigitalPort, display: *mut IntelDisplay) {
    if (*port).ddi_io_wakeref.is_null() {
        (*port).ddi_io_wakeref =
            intel_display_power_get(display, (*port).ddi_io_power_domain as i32);
    }
}

unsafe fn ddi_io_put(port: *mut IntelDigitalPort, display: *mut IntelDisplay) {
    let wf = core::mem::replace(&mut (*port).ddi_io_wakeref, core::ptr::null_mut());
    if !wf.is_null() {
        intel_display_power_put(display, (*port).ddi_io_power_domain as i32, wf);
    }
}

unsafe fn ddi_main_link_aux_domain(
    port: *mut IntelDigitalPort,
    state: *const IntelCrtcState,
) -> i32 {
    let encoder = &mut (*port).base;
    let display = display_from_encoder(encoder);
    if display.is_null() {
        return crate::intel_display_power_types_upstream::POWER_DOMAIN_INVALID as i32;
    }
    if intel_psr_needs_aux_io_power(encoder, state) {
        return intel_display_power_aux_io_domain(display, (*port).aux_ch) as i32;
    }
    if DISPLAY_VER(display) < 14
        && (intel_crtc_has_dp_encoder(state) || intel_encoder_is_tc(encoder))
    {
        return intel_aux_power_domain(port);
    }
    crate::intel_display_power_types_upstream::POWER_DOMAIN_INVALID as i32
}

unsafe fn ddi_main_link_aux_get(port: *mut IntelDigitalPort, state: *const IntelCrtcState) {
    if (*port).aux_wakeref.is_null() {
        let domain = ddi_main_link_aux_domain(port, state);
        if domain != crate::intel_display_power_types_upstream::POWER_DOMAIN_INVALID as i32 {
            (*port).aux_wakeref =
                intel_display_power_get(display_from_encoder(&mut (*port).base), domain);
        }
    }
}

unsafe fn ddi_main_link_aux_put(port: *mut IntelDigitalPort, state: *const IntelCrtcState) {
    let wf = core::mem::replace(&mut (*port).aux_wakeref, core::ptr::null_mut());
    if !wf.is_null() {
        intel_display_power_put(
            display_from_encoder(&mut (*port).base),
            ddi_main_link_aux_domain(port, state),
            wf,
        );
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ddi_get_power_domains(
    encoder: *mut IntelEncoder,
    state: *mut IntelCrtcState,
) {
    if encoder.is_null() || state.is_null() {
        return;
    }
    let port = dig_port(encoder);
    let display = display_from_encoder(encoder);
    if display.is_null() {
        return;
    }
    if !intel_tc_port_in_tbt_alt_mode(port) {
        ddi_io_get(port, display);
    }
    ddi_main_link_aux_get(port, state);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ddi_post_pll_disable(
    _state: *mut IntelAtomicState,
    encoder: *mut IntelEncoder,
    old_state: *mut IntelCrtcState,
    _old_conn: *mut DrmConnectorState,
) {
    if encoder.is_null() || old_state.is_null() {
        return;
    }
    let port = dig_port(encoder);
    ddi_main_link_aux_put(port, old_state);
    if intel_encoder_is_tc(encoder) {
        intel_tc_port_put_link(port);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ddi_pre_pll_enable(
    state: *mut IntelAtomicState,
    encoder: *mut IntelEncoder,
    crtc_state: *mut IntelCrtcState,
    _conn_state: *mut DrmConnectorState,
) {
    if state.is_null() || encoder.is_null() || crtc_state.is_null() {
        return;
    }
    let port = dig_port(encoder);
    if intel_encoder_is_tc(encoder) {
        intel_tc_port_get_link(port, (*crtc_state).lane_count);
        let icrtc = (*crtc_state).uapi.crtc.cast::<IntelCrtc>();
        intel_ddi_update_active_dpll(state, encoder, icrtc);
        if !intel_tc_port_in_tbt_alt_mode(port) {
            intel_tc_port_set_fia_lane_count(port, (*crtc_state).lane_count);
        }
    }
    ddi_main_link_aux_get(port, crtc_state);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ddi_pre_enable(
    state: *mut IntelAtomicState,
    encoder: *mut IntelEncoder,
    crtc_state: *mut IntelCrtcState,
    conn_state: *mut DrmConnectorState,
) {
    if state.is_null() || encoder.is_null() || crtc_state.is_null() {
        return;
    }
    let display = display_from_encoder(encoder);
    let port = dig_port(encoder);
    if display.is_null() {
        return;
    }
    let crtc = (*crtc_state).uapi.crtc.cast::<IntelCrtc>();
    intel_set_cpu_fifo_underrun_reporting(display, (*crtc).pipe, true);
    if output_is_hdmi(crtc_state) {
        intel_dp_dual_mode_set_tmds_output(&mut (*port).hdmi, true);
        intel_ddi_enable_clock(encoder, crtc_state);
        if !intel_tc_port_in_tbt_alt_mode(port) {
            ddi_io_get(port, display);
        }
        icl_program_mg_dp_mode(port, crtc_state);
        intel_ddi_enable_transcoder_clock(encoder, crtc_state);
        if let Some(set) = (*port).set_infoframes {
            set(encoder, (*crtc_state).has_infoframe, crtc_state, conn_state);
        }
    } else {
        ddi_pre_enable_dp(state, encoder, crtc_state, conn_state);
        intel_ddi_set_dp_msa(crtc_state, conn_state);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ddi_set_dp_msa(
    state: *const IntelCrtcState,
    conn_state: *const DrmConnectorState,
) {
    if state.is_null() || !intel_crtc_has_dp_encoder(state) {
        return;
    }
    let display = display_from_state(state);
    if display.is_null() {
        return;
    }
    let mut misc = 1u32; // DP_MSA_MISC_SYNC_CLOCK
    misc |= match (*state).pipe_bpp {
        18 => 0 << 5,
        24 => 1 << 5,
        30 => 2 << 5,
        36 => 3 << 5,
        _ => 1 << 5,
    };
    if (*state).limited_color_range
        && (*state).output_format == crate::intel_display_types_upstream::INTEL_OUTPUT_FORMAT_RGB
    {
        misc |= 1 << 3; // DP_MSA_MISC_COLOR_CEA_RGB
    }
    if (*state).output_format == crate::intel_display_types_upstream::INTEL_OUTPUT_FORMAT_YCBCR444 {
        misc |= (2 << 1) | (1 << 3) | (1 << 4); // BT.709 YCbCr 4:4:4
    }
    if !conn_state.is_null() && intel_dp_needs_vsc_sdp(state, conn_state) {
        misc |= 1 << 14;
    }
    intel_de_write(
        display,
        TRANS_MSA_MISC(display, (*state).cpu_transcoder as u32),
        misc,
    );
}

unsafe fn ddi_buf_enable(encoder: *mut IntelEncoder, ctl: u32) {
    let display = display_from_encoder(encoder);
    if display.is_null() {
        return;
    }
    let reg = DDI_BUF_CTL((*encoder).port as u32);
    intel_de_write(display, reg, ctl | DDI_BUF_CTL_ENABLE);
    intel_de_posting_read(display, reg);
}

unsafe fn ddi_buf_disable(encoder: *mut IntelEncoder, state: *const IntelCrtcState) {
    let display = display_from_encoder(encoder);
    if display.is_null() {
        return;
    }
    let reg = DDI_BUF_CTL((*encoder).port as u32);
    intel_de_rmw(display, reg, DDI_BUF_CTL_ENABLE, 0);
    if intel_crtc_has_dp_encoder(state) {
        let dp_ctl = ddi_dp_tp_ctl_reg(encoder, state);
        intel_de_rmw(display, dp_ctl, DP_TP_CTL_ENABLE | DP_TP_CTL_FEC_ENABLE, 0);
        intel_de_posting_read(display, dp_ctl);
    }
    // On display 13 the status bit is the authoritative idle condition.
    for _ in 0..1000 {
        if intel_de_read(display, reg) & DDI_BUF_IS_IDLE != 0 {
            break;
        }
        tk_linux_kapi::primitives::udelay(1);
    }
    let _ = intel_ddi_wait_for_fec_status(encoder, state, false);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ddi_enable(
    state: *mut IntelAtomicState,
    encoder: *mut IntelEncoder,
    crtc_state: *mut IntelCrtcState,
    conn_state: *mut DrmConnectorState,
) {
    if state.is_null() || encoder.is_null() || crtc_state.is_null() || conn_state.is_null() {
        return;
    }
    let display = display_from_encoder(encoder);
    let port = dig_port(encoder);
    if display.is_null() {
        return;
    }
    intel_ddi_enable_transcoder_func(encoder, crtc_state);
    intel_enable_transcoder(crtc_state);
    let _ = intel_ddi_wait_for_fec_status(encoder, crtc_state, true);
    if output_is_hdmi(crtc_state) {
        let connector = (*conn_state).connector;
        if !intel_hdmi_handle_sink_scrambling(
            encoder,
            connector,
            (*crtc_state).hdmi_high_tmds_clock_ratio,
            (*crtc_state).hdmi_scrambling,
        ) { /* sink failure is non-fatal, Linux logs */ }
        if let Some(set_levels) = (*encoder).set_signal_levels {
            set_levels(encoder, crtc_state);
        }
        let mut ctl = 0;
        if (*port).lane_reversal {
            ctl |= DDI_BUF_PORT_REVERSAL;
        }
        if (*port).ddi_a_4_lanes {
            ctl |= DDI_A_4_LANES;
        }
        if intel_encoder_is_tc(encoder)
            && (*display).platform._anon_0_0.bitmap[0] & (1u64 << 52) != 0
        {
            ctl |= DDI_BUF_CTL_TC_PHY_OWNERSHIP;
        }
        ddi_buf_enable(encoder, ctl);
        intel_hdmi_poll_for_scrambling_enable(crtc_state, connector);
    } else {
        (*port).dp.link.active = true;
        intel_dp_set_infoframes(encoder, true, crtc_state, conn_state);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ddi_disable(
    state: *mut IntelAtomicState,
    encoder: *mut IntelEncoder,
    old_state: *mut IntelCrtcState,
    old_conn: *mut DrmConnectorState,
) {
    if encoder.is_null() || old_state.is_null() || old_conn.is_null() {
        return;
    }
    let port = dig_port(encoder);
    intel_tc_port_link_cancel_reset_work(port);
    if output_is_hdmi(old_state) {
        let _ = intel_hdmi_handle_sink_scrambling(encoder, (*old_conn).connector, false, false);
    } else {
        if !(*old_conn).connector.is_null() {
            intel_dp_sink_disable_decompression(
                state,
                (*old_conn).connector.cast::<IntelConnector>(),
                old_state,
            );
        }
        (*port).dp.link.active = false;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ddi_post_disable(
    state: *mut IntelAtomicState,
    encoder: *mut IntelEncoder,
    old_state: *mut IntelCrtcState,
    old_conn: *mut DrmConnectorState,
) {
    if encoder.is_null() || old_state.is_null() {
        return;
    }
    let display = display_from_encoder(encoder);
    let port = dig_port(encoder);
    if display.is_null() {
        return;
    }
    if !old_conn.is_null() {
        if output_is_hdmi(old_state) {
            if let Some(set) = (*port).set_infoframes {
                set(encoder, false, old_state, old_conn);
            }
        } else {
            intel_dp_set_infoframes(encoder, false, old_state, old_conn);
        }
    }
    intel_disable_transcoder(old_state);
    intel_ddi_disable_transcoder_func(old_state);
    if !output_is_hdmi(old_state) {
        intel_dp_set_power(&mut (*port).dp, 2);
    } // DP_SET_POWER_D3
    ddi_buf_disable(encoder, old_state);
    if !output_is_hdmi(old_state) {
        intel_dp_sink_set_fec_ready(&mut (*port).dp, old_state, false);
    }
    intel_ddi_disable_transcoder_clock(old_state);
    ddi_io_put(port, display);
    intel_ddi_disable_clock(encoder);
    if output_is_hdmi(old_state) {
        intel_dp_dual_mode_set_tmds_output(&mut (*port).hdmi, false);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ddi_update_pipe(
    state: *mut IntelAtomicState,
    encoder: *mut IntelEncoder,
    crtc_state: *mut IntelCrtcState,
    conn_state: *mut DrmConnectorState,
) {
    if encoder.is_null() || crtc_state.is_null() || conn_state.is_null() {
        return;
    }
    if output_is_hdmi(crtc_state) {
        intel_hdmi_fastset_infoframes(encoder, crtc_state, conn_state);
    } else {
        intel_dp_set_infoframes(encoder, true, crtc_state, conn_state);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ddi_sync_state(
    encoder: *mut IntelEncoder,
    state: *mut IntelCrtcState,
) {
    if encoder.is_null() {
        return;
    }
    if intel_encoder_is_tc(encoder) {
        intel_tc_port_sanitize_mode(dig_port(encoder), state);
    }
    if !state.is_null() && intel_crtc_has_dp_encoder(state) {
        intel_dp_sync_state(encoder, state);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ddi_shutdown(encoder: *mut IntelEncoder) {
    if encoder.is_null() {
        return;
    }
    intel_dp_encoder_shutdown(encoder);
    intel_hdmi_encoder_shutdown(encoder);
}

unsafe fn intel_ddi_init_dp_connector(port: *mut IntelDigitalPort) -> i32 {
    let connector = intel_connector_alloc();
    if connector.is_null() {
        return -12;
    }
    let encoder = &mut (*port).base;
    (*port).dp.output_reg = DDI_BUF_CTL(encoder.port as u32);
    (*port).dp.prepare_link_retrain = Some(intel_ddi_prepare_link_retrain);
    (*port).dp.set_link_train = Some(intel_ddi_set_link_train);
    (*port).dp.set_idle_link_train = Some(intel_ddi_set_idle_link_train);
    (*port).dp.voltage_max = Some(intel_ddi_dp_voltage_max);
    (*port).dp.preemph_max = Some(intel_ddi_dp_preemph_max);
    (*port).aux_ch = intel_dp_aux_ch(encoder);
    if (*port).aux_ch == crate::intel_display_limits_types_upstream::AUX_CH_NONE {
        intel_connector_free(connector);
        return -22;
    }
    if !intel_dp_init_connector(port, connector) {
        intel_connector_free(connector);
        return -22;
    }
    0
}

unsafe fn intel_ddi_init_hdmi_connector(port: *mut IntelDigitalPort) -> i32 {
    let connector = intel_connector_alloc();
    if connector.is_null() {
        return 0;
    }
    (*port).hdmi.hdmi_reg = DDI_BUF_CTL((*port).base.port as u32);
    if !intel_hdmi_init_connector(port, connector) {
        (*port).hdmi.hdmi_reg = INVALID_MMIO_REG;
        intel_connector_free(connector);
    }
    0
}

unsafe extern "C" fn intel_ddi_compute_output_type(
    _encoder: *mut IntelEncoder,
    _state: *mut IntelCrtcState,
    conn_state: *mut DrmConnectorState,
) -> u32 {
    if conn_state.is_null() || (*conn_state).connector.is_null() {
        return INTEL_OUTPUT_DP;
    }
    if (*(*conn_state).connector).connector_type
        == tk_drm_upstream::uapi_drm_consts::DRM_MODE_CONNECTOR_HDMIA as i32
    {
        INTEL_OUTPUT_HDMI
    } else {
        INTEL_OUTPUT_DP
    }
}

unsafe extern "C" fn intel_ddi_encoder_reset(encoder: *mut DrmEncoder) {
    if encoder.is_null() {
        return;
    }
    let intel_encoder = encoder.cast::<IntelEncoder>();
    let port = dig_port(intel_encoder);
    (*port).dp.reset_link_params = true;
    intel_dp_invalidate_source_oui(&mut (*port).dp);
    if intel_encoder_is_tc(intel_encoder) {
        intel_tc_port_init_mode(port);
    }
}

unsafe extern "C" fn intel_ddi_encoder_destroy(encoder: *mut DrmEncoder) {
    if encoder.is_null() {
        return;
    }
    let intel_encoder = encoder.cast::<IntelEncoder>();
    let port = dig_port(intel_encoder);
    let display = display_from_encoder(intel_encoder);
    intel_dp_encoder_flush_work(encoder);
    if intel_encoder_is_tc(intel_encoder) {
        intel_tc_port_cleanup(port);
    }
    if !display.is_null() {
        intel_display_power_flush_work(display);
    }
    drm_encoder_cleanup(encoder);
    tk_linux_kapi::memory::kfree(port.cast::<c_void>());
}

unsafe extern "C" fn intel_ddi_encoder_late_register(encoder: *mut DrmEncoder) -> i32 {
    if encoder.is_null() {
        return -22;
    }
    let intel_encoder = encoder.cast::<IntelEncoder>();
    if intel_encoder_is_tc(intel_encoder) {
        intel_tc_port_link_reset(dig_port(intel_encoder));
    }
    0
}

unsafe extern "C" fn intel_ddi_tc_shutdown_complete(encoder: *mut IntelEncoder) {
    if !encoder.is_null() && intel_encoder_is_tc(encoder) {
        intel_tc_port_cleanup(dig_port(encoder));
    }
}

unsafe extern "C" fn intel_ddi_hotplug(
    encoder: *mut IntelEncoder,
    connector: *mut IntelConnector,
) -> u32 {
    if encoder.is_null() || connector.is_null() {
        return crate::intel_display_types_upstream::INTEL_HOTPLUG_UNCHANGED;
    }
    let port = dig_port(encoder);
    let dp = &mut (*port).dp;
    if intel_dp_test_phy(dp) {
        return crate::intel_display_types_upstream::INTEL_HOTPLUG_UNCHANGED;
    }
    let state = intel_encoder_hotplug(encoder, connector);
    if !intel_tc_port_link_reset(port)
        && (*connector).base.connector_type
            != tk_drm_upstream::uapi_drm_consts::DRM_MODE_CONNECTOR_HDMIA as i32
    {
        intel_dp_check_link_state(dp);
    }
    let retries = if intel_encoder_is_tc(encoder) { 5 } else { 1 };
    if state == crate::intel_display_types_upstream::INTEL_HOTPLUG_UNCHANGED
        && (*connector).hotplug_retries < retries
        && !(*dp).is_mst
    {
        crate::intel_display_types_upstream::INTEL_HOTPLUG_RETRY
    } else {
        state
    }
}

unsafe extern "C" fn intel_ddi_connected(encoder: *mut IntelEncoder) -> bool {
    if encoder.is_null() {
        return false;
    }
    let display = display_from_encoder(encoder);
    if display.is_null() || (*display).hotplug.hpd.is_null() || (*encoder).hpd_pin >= 15 {
        return false;
    }
    let hpd_bit = *(*display).hotplug.hpd.add((*encoder).hpd_pin as usize);
    intel_de_read(display, GEN8_DE_PORT_ISR) & hpd_bit != 0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ddi_port_pll_type_cb(
    encoder: *mut IntelEncoder,
    state: *mut IntelCrtcState,
) -> u32 {
    icl_ddi_tc_port_pll_type(encoder, state)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ddi_init(display: *mut IntelDisplay, devdata: *const c_void) {
    if display.is_null() || devdata.is_null() || DISPLAY_VER(display) != 13 {
        return;
    }
    let port = intel_bios_encoder_port(devdata);
    // Port A is the ADL-P eDP path and remains outside this external-DDI module.
    if port == PORT_NONE
        || port == crate::intel_display_limits_types_upstream::PORT_A
        || port < 0
        || port > 8
    {
        return;
    }
    let init_dp = intel_bios_encoder_supports_dp(devdata);
    let init_hdmi =
        intel_bios_encoder_supports_dvi(devdata) || intel_bios_encoder_supports_hdmi(devdata);
    if !init_dp && !init_hdmi {
        return;
    }

    let dig = tk_linux_kapi::memory::kzalloc_obj::<IntelDigitalPort>();
    if dig.is_null() {
        return;
    }
    let encoder = &mut (*dig).base;
    encoder.devdata = devdata.cast_mut();
    encoder.r#type = INTEL_OUTPUT_DDI;
    encoder.port = port;
    encoder.cloneable = 0;
    encoder.pipe_mask = 0xff;
    encoder.power_domain = intel_display_power_ddi_lanes_domain(display, port) as _;
    (*dig).lane_reversal = intel_bios_encoder_lane_reversal(devdata);
    (*dig).max_lanes = 4;
    (*dig).ddi_a_4_lanes = false;
    (*dig).dedicated_external = intel_bios_encoder_is_dedicated_external(devdata);
    (*dig).ddi_io_power_domain = intel_display_power_ddi_io_domain(display, port) as _;
    encoder.hpd_pin = xelpd_hpd_pin(port);
    encoder.compute_config = Some(intel_ddi_compute_config);
    encoder.compute_output_type = Some(intel_ddi_compute_output_type);
    encoder.hotplug = Some(intel_ddi_hotplug);
    encoder.pre_pll_enable = Some(intel_ddi_pre_pll_enable);
    encoder.pre_enable = Some(intel_ddi_pre_enable);
    encoder.enable = Some(intel_ddi_enable);
    encoder.disable = Some(intel_ddi_disable);
    encoder.post_disable = Some(intel_ddi_post_disable);
    encoder.post_pll_disable = Some(intel_ddi_post_pll_disable);
    encoder.update_pipe = Some(intel_ddi_update_pipe);
    encoder.get_hw_state = Some(intel_ddi_get_hw_state);
    encoder.get_config = Some(intel_ddi_get_config);
    encoder.sync_state = Some(intel_ddi_sync_state);
    encoder.shutdown = Some(intel_ddi_shutdown);
    encoder.shutdown_complete = Some(intel_ddi_tc_shutdown_complete);
    encoder.get_power_domains = Some(intel_ddi_get_power_domains);
    if intel_encoder_is_tc(encoder) {
        encoder.port_pll_type = Some(icl_ddi_tc_port_pll_type);
        encoder.set_signal_levels = Some(tgl_dkl_phy_set_signal_levels);
        encoder.enable_clock = Some(icl_tc_enable_clock);
        encoder.disable_clock = Some(icl_tc_disable_clock);
        encoder.is_clock_enabled = Some(icl_tc_is_clock_enabled);
        (*dig).connected = Some(intel_tc_port_connected);
        (*dig).lock = Some(intel_tc_port_lock);
        (*dig).unlock = Some(intel_tc_port_unlock);
    } else {
        encoder.port_pll_type = None;
        encoder.set_signal_levels = Some(icl_combo_phy_set_signal_levels);
        encoder.enable_clock = Some(icl_combo_enable_clock);
        encoder.disable_clock = Some(icl_combo_disable_clock);
        encoder.is_clock_enabled = Some(icl_combo_is_clock_enabled);
        (*dig).connected = Some(intel_ddi_connected);
    }
    intel_ddi_buf_trans_init(encoder);

    (*dig).hpd_pulse = Some(intel_dp_hpd_pulse);
    // intel_infoframe_init() installs the ADL-P HSW infoframe MMIO callbacks.
    intel_infoframe_init(dig);

    if intel_encoder_is_tc(encoder) {
        let mut legacy = !intel_bios_encoder_supports_typec_usb(devdata)
            && !intel_bios_encoder_supports_tbt(devdata);
        if !legacy && init_hdmi {
            legacy = !init_dp;
        }
        if intel_tc_port_init(dig, legacy) < 0 {
            tk_linux_kapi::memory::kfree(dig.cast::<c_void>());
            return;
        }
        encoder.suspend_complete = Some(intel_ddi_tc_shutdown_complete);
    }
    let name = b"DDI\0";
    if drm_encoder_init(
        (*display).drm,
        &mut encoder.base,
        &INTEL_DDI_FUNCS,
        DRM_MODE_ENCODER_TMDS,
        name.as_ptr().cast(),
    ) != 0
    {
        if intel_encoder_is_tc(encoder) {
            intel_tc_port_cleanup(dig);
        }
        tk_linux_kapi::memory::kfree(dig.cast::<c_void>());
        return;
    }
    if init_dp {
        if intel_ddi_init_dp_connector(dig) != 0 {
            drm_encoder_cleanup(&mut encoder.base);
            if intel_encoder_is_tc(encoder) {
                intel_tc_port_cleanup(dig);
            }
            tk_linux_kapi::memory::kfree(dig.cast::<c_void>());
            return;
        }
    }
    if init_hdmi {
        let _ = intel_ddi_init_hdmi_connector(dig);
    }
}

static INTEL_DDI_FUNCS: DrmEncoderFuncs = DrmEncoderFuncs {
    reset: Some(intel_ddi_encoder_reset),
    destroy: Some(intel_ddi_encoder_destroy),
    late_register: Some(intel_ddi_encoder_late_register),
    early_unregister: None,
    debugfs_init: None,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dp_signal_level_mapping_is_ordered_and_bounded() {
        assert_eq!(train_set_to_entry(0x00), 0);
        assert_eq!(train_set_to_entry(0x18), 3);
        assert_eq!(train_set_to_entry(0x02), 9);
        assert_eq!(train_set_to_entry(0xff), 0);
        assert_eq!(MAX_DDI_SIGNAL_LEVELS, 10);
    }

    #[test]
    fn transcoder_bpc_map_uses_display_depth_encodings() {
        assert_eq!(encode_bpc(18), TRANS_DDI_BPC_6);
        assert_eq!(encode_bpc(24), TRANS_DDI_BPC_8);
        assert_eq!(encode_bpc(30), TRANS_DDI_BPC_10);
        assert_eq!(encode_bpc(36), TRANS_DDI_BPC_12);
    }
}
