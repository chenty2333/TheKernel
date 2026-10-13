// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! ADL-P Type-C port state machine from Linux v7.2.3 `intel_tc.c`.
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::{
    ffi::{c_char, c_void},
    mem::{offset_of, size_of},
    ptr,
};

use tk_drm_upstream::{
    drm_atomic_types::DrmAtomicCommit, drm_device_types::DrmDevice,
    drm_modeset_lock_types::DrmModesetAcquireCtx,
};
use tk_linux_kapi::types::{DelayedWork, Mutex, WorkStruct};

use crate::{
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_limits_types_upstream::{PORT_TC1, PORT_TC4},
    intel_display_power_types_upstream::{
        IntelDisplayPowerDomain, POWER_DOMAIN_DISPLAY_CORE, POWER_DOMAIN_INVALID,
        POWER_DOMAIN_PORT_DDI_LANES_TC1, POWER_DOMAIN_TC_COLD_OFF,
    },
    intel_display_types_upstream::{
        IntelAtomicState, IntelCrtcState, IntelDigitalPort, IntelDp, IntelEncoder,
    },
    intel_display_wa_upstream::DISPLAY_VER,
    intel_dpll_mgr_types_upstream::{ICL_PORT_DPLL_DEFAULT, ICL_PORT_DPLL_MG_PHY, IclPortDpllId},
};

const TC_PORT_DISCONNECTED: i32 = 0;
const TC_PORT_TBT_ALT: i32 = 1;
const TC_PORT_DP_ALT: i32 = 2;
const TC_PORT_LEGACY: i32 = 3;

const PIN_ASSIGNMENT_NONE: i32 = 0;
const PIN_ASSIGNMENT_A: i32 = 1;
const PIN_ASSIGNMENT_B: i32 = 2;
const PIN_ASSIGNMENT_C: i32 = 3;
const PIN_ASSIGNMENT_D: i32 = 4;
const PIN_ASSIGNMENT_E: i32 = 5;
const PIN_ASSIGNMENT_F: i32 = 6;

const EDEADLK: i32 = tk_linux_kapi::config::EDEADLK;
const ENOMEM: i32 = tk_linux_kapi::config::ENOMEM;
const EINVAL: i32 = tk_linux_kapi::config::EINVAL;

/// Local C-file struct mirror (`struct intel_tc_port` in `intel_tc.c`).
/// CONFIG_DRM_I915_DEBUG_RUNTIME_PM is disabled by the configured build.
#[repr(C)]
struct IntelTcPort {
    dig_port: *mut IntelDigitalPort,
    phy_ops: *const IntelTcPhyOps,
    lock: Mutex,
    lock_wakeref: *mut c_void,
    disconnect_phy_work: DelayedWork,
    link_reset_work: DelayedWork,
    link_refcount: i32,
    legacy_port: bool,
    port_name: *const c_char,
    mode: i32,
    init_mode: i32,
    phy_fia: i32,
    pin_assignment: i32,
    phy_fia_idx: u8,
    max_lane_count: u8,
}

#[repr(C)]
struct IntelTcPhyOps {
    cold_off_domain: Option<unsafe extern "C" fn(*mut IntelTcPort) -> IntelDisplayPowerDomain>,
    hpd_live_status: Option<unsafe extern "C" fn(*mut IntelTcPort) -> u32>,
    is_ready: Option<unsafe extern "C" fn(*mut IntelTcPort) -> bool>,
    is_owned: Option<unsafe extern "C" fn(*mut IntelTcPort) -> bool>,
    get_hw_state: Option<unsafe extern "C" fn(*mut IntelTcPort)>,
    connect: Option<unsafe extern "C" fn(*mut IntelTcPort, i32) -> bool>,
    disconnect: Option<unsafe extern "C" fn(*mut IntelTcPort)>,
    init: Option<unsafe extern "C" fn(*mut IntelTcPort)>,
}

const _: () = assert!(size_of::<IntelTcPort>() == 264);
const _: () = assert!(offset_of!(IntelTcPort, lock) == 16);
const _: () = assert!(offset_of!(IntelTcPort, lock_wakeref) == 40);
const _: () = assert!(offset_of!(IntelTcPort, disconnect_phy_work) == 48);
const _: () = assert!(offset_of!(IntelTcPort, link_reset_work) == 136);
const _: () = assert!(offset_of!(IntelTcPort, link_refcount) == 224);
const _: () = assert!(offset_of!(IntelTcPort, legacy_port) == 228);
const _: () = assert!(offset_of!(IntelTcPort, port_name) == 232);
const _: () = assert!(offset_of!(IntelTcPort, mode) == 240);
const _: () = assert!(offset_of!(IntelTcPort, phy_fia_idx) == 256);
const _: () = assert!(size_of::<IntelTcPhyOps>() == 64);

static ADLP_TC_PHY_OPS: IntelTcPhyOps = IntelTcPhyOps {
    cold_off_domain: Some(adlp_tc_phy_cold_off_domain),
    hpd_live_status: Some(adlp_tc_phy_hpd_live_status),
    is_ready: Some(adlp_tc_phy_is_ready),
    is_owned: Some(adlp_tc_phy_is_owned),
    get_hw_state: Some(adlp_tc_phy_get_hw_state),
    connect: Some(adlp_tc_phy_connect),
    disconnect: Some(adlp_tc_phy_disconnect),
    init: Some(adlp_tc_phy_init),
};

unsafe extern "C" {
    fn drm_atomic_commit(state: *mut DrmAtomicCommit) -> i32;
    fn drm_atomic_commit_alloc(dev: *mut DrmDevice) -> *mut DrmAtomicCommit;
    fn drm_atomic_commit_put(state: *mut DrmAtomicCommit);
    fn drm_modeset_lock(
        lock: *mut tk_drm_upstream::drm_modeset_lock_types::DrmModesetLock,
        ctx: *mut DrmModesetAcquireCtx,
    ) -> i32;
    fn drm_modeset_acquire_init(ctx: *mut DrmModesetAcquireCtx, flags: u32);
    fn drm_modeset_acquire_fini(ctx: *mut DrmModesetAcquireCtx);
    fn drm_modeset_drop_locks(ctx: *mut DrmModesetAcquireCtx);
    fn drm_modeset_backoff(ctx: *mut DrmModesetAcquireCtx) -> i32;
    fn intel_ddi_port_pll_type(
        encoder: *mut IntelEncoder,
        crtc_state: *const IntelCrtcState,
    ) -> IclPortDpllId;
    fn intel_dp_mst_active_streams(dp: *mut IntelDp) -> i32;
    fn intel_dp_get_active_pipes(
        dp: *mut IntelDp,
        ctx: *mut DrmModesetAcquireCtx,
        pipe_mask: *mut u8,
    ) -> i32;
}

#[inline]
unsafe fn display_from_dig_port(dig_port: *mut IntelDigitalPort) -> *mut IntelDisplay {
    if dig_port.is_null() || unsafe { (*dig_port).base.base.dev.is_null() } {
        return ptr::null_mut();
    }
    let dev = unsafe { (*dig_port).base.base.dev };
    if unsafe { (*dev).dev_private.is_null() } {
        return ptr::null_mut();
    }
    let i915 = unsafe {
        (*dev)
            .dev_private
            .cast::<crate::linux_i915_private::DrmI915Private>()
    };
    unsafe { (*i915).display.cast::<IntelDisplay>() }
}

#[inline]
unsafe fn dig_port_from_encoder(encoder: *mut IntelEncoder) -> *mut IntelDigitalPort {
    if encoder.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        (encoder as *mut u8)
            .sub(offset_of!(IntelDigitalPort, base))
            .cast::<IntelDigitalPort>()
    }
}

#[inline]
unsafe fn tc_port_state(dig_port: *mut IntelDigitalPort) -> *mut IntelTcPort {
    if dig_port.is_null() {
        return ptr::null_mut();
    }
    unsafe { (*dig_port).tc.cast::<IntelTcPort>() }
}

#[inline]
unsafe fn encoder_is_tc(encoder: *mut IntelEncoder) -> bool {
    let dig_port = unsafe { dig_port_from_encoder(encoder) };
    !dig_port.is_null() && unsafe { !(*dig_port).dedicated_external && !(*dig_port).tc.is_null() }
}

#[inline]
unsafe fn tc_index(dig_port: *mut IntelDigitalPort) -> Option<i32> {
    if dig_port.is_null() {
        return None;
    }
    let port = unsafe { (*dig_port).base.port };
    let index = port - PORT_TC1;
    (0..=PORT_TC4 - PORT_TC1).contains(&index).then_some(index)
}

#[inline]
unsafe fn tc_port_power_domain(tc: *mut IntelTcPort) -> IntelDisplayPowerDomain {
    if tc.is_null() || unsafe { (*tc).dig_port.is_null() } {
        return POWER_DOMAIN_INVALID;
    }
    let Some(index) = (unsafe { tc_index((*tc).dig_port) }) else {
        return POWER_DOMAIN_INVALID;
    };
    POWER_DOMAIN_PORT_DDI_LANES_TC1 + index as u32
}

#[inline]
unsafe fn adlp_platform(display: *const IntelDisplay) -> bool {
    if display.is_null() {
        return false;
    }
    unsafe {
        let bits = ptr::addr_of!((*display).platform).cast::<u8>();
        *bits.add(52 / 8) & (1 << (52 % 8)) != 0
    }
}

#[inline]
fn tc_mode_bit(mode: i32) -> u32 {
    if (0..32).contains(&mode) {
        1u32 << mode
    } else {
        0
    }
}

fn tc_port_mode_name(mode: i32) -> &'static str {
    match mode {
        TC_PORT_DISCONNECTED => "disconnected",
        TC_PORT_TBT_ALT => "tbt-alt",
        TC_PORT_DP_ALT => "dp-alt",
        TC_PORT_LEGACY => "legacy",
        _ => "invalid",
    }
}

fn pin_assignment_name(pin: i32) -> char {
    if pin == PIN_ASSIGNMENT_NONE {
        '-'
    } else {
        char::from_u32(('A' as u32).saturating_add((pin - PIN_ASSIGNMENT_A) as u32)).unwrap_or('-')
    }
}

#[inline]
unsafe fn assert_display_core_power_enabled(tc: *mut IntelTcPort) {
    let display = unsafe { display_from_dig_port((*tc).dig_port) };
    let _ = tk_linux_kapi::assert::warn_on(unsafe {
        !crate::intel_display_power_upstream::intel_display_power_is_enabled(
            display,
            POWER_DOMAIN_DISPLAY_CORE,
        )
    });
}

#[inline]
unsafe fn assert_tc_port_power_enabled(tc: *mut IntelTcPort) {
    let display = unsafe { display_from_dig_port((*tc).dig_port) };
    let _ = tk_linux_kapi::assert::warn_on(unsafe {
        !crate::intel_display_power_upstream::intel_display_power_is_enabled(
            display,
            tc_port_power_domain(tc),
        )
    });
}

#[inline]
unsafe fn assert_tc_cold_blocked(tc: *mut IntelTcPort) {
    let _ = tk_linux_kapi::assert::warn_on(unsafe { (*tc).lock_wakeref.is_null() });
}

unsafe fn tc_phy_cold_off_domain(tc: *mut IntelTcPort) -> IntelDisplayPowerDomain {
    let dig_port = unsafe { (*tc).dig_port };
    let display = unsafe { display_from_dig_port(dig_port) };
    if unsafe { (*tc).mode == TC_PORT_TBT_ALT } {
        POWER_DOMAIN_TC_COLD_OFF
    } else {
        unsafe {
            crate::intel_display_power_upstream::intel_display_power_legacy_aux_domain(
                display,
                (*dig_port).aux_ch,
            )
        }
    }
}

unsafe fn tc_cold_block(tc: *mut IntelTcPort) {
    let display = unsafe { display_from_dig_port((*tc).dig_port) };
    let domain = unsafe { tc_phy_cold_off_domain(tc) };
    let wakeref =
        unsafe { crate::intel_display_power_upstream::intel_display_power_get(display, domain) };
    unsafe { (*tc).lock_wakeref = wakeref };
}

unsafe fn tc_cold_unblock(tc: *mut IntelTcPort, wakeref: *mut c_void) {
    if tc.is_null() || wakeref.is_null() {
        return;
    }
    let display = unsafe { display_from_dig_port((*tc).dig_port) };
    let domain = unsafe { tc_phy_cold_off_domain(tc) };
    unsafe {
        crate::intel_display_power_upstream::intel_display_power_put(display, domain, wakeref)
    };
}

unsafe fn tc_phy_load_fia_params(tc: *mut IntelTcPort) {
    if let Some(index) = unsafe { tc_index((*tc).dig_port) } {
        unsafe {
            (*tc).phy_fia = index / 2;
            (*tc).phy_fia_idx = (index % 2) as u8;
        }
    }
}

unsafe extern "C" fn adlp_tc_phy_cold_off_domain(tc: *mut IntelTcPort) -> IntelDisplayPowerDomain {
    unsafe { tc_phy_cold_off_domain(tc) }
}

unsafe extern "C" fn adlp_tc_phy_hpd_live_status(tc: *mut IntelTcPort) -> u32 {
    if tc.is_null() || unsafe { (*tc).dig_port.is_null() } {
        return 0;
    }
    let dig_port = unsafe { (*tc).dig_port };
    let display = unsafe { display_from_dig_port(dig_port) };
    if display.is_null() {
        return 0;
    }
    let hpd_pin = unsafe { (*dig_port).base.hpd_pin as usize };
    let hotplug = unsafe { ptr::addr_of_mut!((*display).hotplug) };
    if unsafe { (*hotplug).hpd.is_null() || (*hotplug).pch_hpd.is_null() } {
        return 0;
    }
    let cpu_isr_bit = unsafe { *(*hotplug).hpd.add(hpd_pin) };
    let pch_isr_bit = unsafe { *(*hotplug).pch_hpd.add(hpd_pin) };
    let wakeref = unsafe {
        crate::intel_display_power_upstream::intel_display_power_get(
            display,
            POWER_DOMAIN_DISPLAY_CORE,
        )
    };
    let cpu_isr = unsafe {
        crate::intel_de_upstream::intel_de_read(
            display,
            crate::intel_display_regs_upstream::GEN11_DE_HPD_ISR,
        )
    };
    let pch_isr = unsafe {
        crate::intel_de_upstream::intel_de_read(display, crate::intel_display_regs_upstream::SDEISR)
    };
    unsafe {
        crate::intel_display_power_upstream::intel_display_power_put(
            display,
            POWER_DOMAIN_DISPLAY_CORE,
            wakeref,
        )
    };
    let mut mask = 0;
    if cpu_isr & (cpu_isr_bit & crate::intel_display_regs_upstream::GEN11_DE_TC_HOTPLUG_MASK) != 0 {
        mask |= tc_mode_bit(TC_PORT_DP_ALT);
    }
    if cpu_isr & (cpu_isr_bit & crate::intel_display_regs_upstream::GEN11_DE_TBT_HOTPLUG_MASK) != 0
    {
        mask |= tc_mode_bit(TC_PORT_TBT_ALT);
    }
    if pch_isr & pch_isr_bit != 0 {
        mask |= tc_mode_bit(TC_PORT_LEGACY);
    }
    mask
}

unsafe extern "C" fn adlp_tc_phy_is_ready(tc: *mut IntelTcPort) -> bool {
    if tc.is_null() || unsafe { (*tc).dig_port.is_null() } {
        return false;
    }
    let display = unsafe { display_from_dig_port((*tc).dig_port) };
    let Some(index) = (unsafe { tc_index((*tc).dig_port) }) else {
        return false;
    };
    let wakeref = unsafe {
        crate::intel_display_power_upstream::intel_display_power_get(
            display,
            POWER_DOMAIN_DISPLAY_CORE,
        )
    };
    unsafe { assert_display_core_power_enabled(tc) };
    let value = unsafe {
        crate::intel_de_upstream::intel_de_read(
            display,
            crate::intel_display_regs_upstream::TCSS_DDI_STATUS(index as u32),
        )
    };
    unsafe {
        crate::intel_display_power_upstream::intel_display_power_put(
            display,
            POWER_DOMAIN_DISPLAY_CORE,
            wakeref,
        )
    };
    value != u32::MAX && value & crate::intel_display_regs_upstream::TCSS_DDI_STATUS_READY != 0
}

unsafe fn adlp_tc_phy_take_ownership(tc: *mut IntelTcPort, take: bool) -> bool {
    if tc.is_null() || unsafe { (*tc).dig_port.is_null() } {
        return false;
    }
    let display = unsafe { display_from_dig_port((*tc).dig_port) };
    let port = unsafe { (*(*tc).dig_port).base.port };
    unsafe { assert_tc_port_power_enabled(tc) };
    unsafe {
        crate::intel_de_upstream::intel_de_rmw(
            display,
            crate::intel_display_regs_upstream::DDI_BUF_CTL(port as u32),
            crate::intel_display_regs_upstream::DDI_BUF_CTL_TC_PHY_OWNERSHIP,
            if take {
                crate::intel_display_regs_upstream::DDI_BUF_CTL_TC_PHY_OWNERSHIP
            } else {
                0
            },
        );
    }
    true
}

unsafe extern "C" fn adlp_tc_phy_is_owned(tc: *mut IntelTcPort) -> bool {
    if tc.is_null() || unsafe { (*tc).dig_port.is_null() } {
        return false;
    }
    let display = unsafe { display_from_dig_port((*tc).dig_port) };
    let port = unsafe { (*(*tc).dig_port).base.port };
    unsafe { assert_tc_port_power_enabled(tc) };
    unsafe {
        crate::intel_de_upstream::intel_de_read(
            display,
            crate::intel_display_regs_upstream::DDI_BUF_CTL(port as u32),
        ) & crate::intel_display_regs_upstream::DDI_BUF_CTL_TC_PHY_OWNERSHIP
            != 0
    }
}

unsafe fn get_lane_mask(tc: *mut IntelTcPort) -> u32 {
    if tc.is_null() || unsafe { (*tc).dig_port.is_null() } {
        return 0;
    }
    let display = unsafe { display_from_dig_port((*tc).dig_port) };
    unsafe { assert_tc_cold_blocked(tc) };
    let reg = crate::intel_display_regs_upstream::PORT_TX_DFLEXDPSP((*tc).phy_fia as u32);
    let wakeref = unsafe {
        crate::intel_display_power_upstream::intel_display_power_get(
            display,
            POWER_DOMAIN_DISPLAY_CORE,
        )
    };
    unsafe { assert_display_core_power_enabled(tc) };
    let value = unsafe { crate::intel_de_upstream::intel_de_read(display, reg) };
    unsafe {
        crate::intel_display_power_upstream::intel_display_power_put(
            display,
            POWER_DOMAIN_DISPLAY_CORE,
            wakeref,
        )
    };
    if value == u32::MAX {
        return 0;
    }
    let mask =
        crate::intel_display_regs_upstream::DP_LANE_ASSIGNMENT_MASK((*tc).phy_fia_idx as u32);
    let shift =
        crate::intel_display_regs_upstream::DP_LANE_ASSIGNMENT_SHIFT((*tc).phy_fia_idx as u32);
    (value & mask) >> shift
}

unsafe fn get_pin_assignment(tc: *mut IntelTcPort) -> i32 {
    if tc.is_null() || unsafe { (*tc).dig_port.is_null() } {
        return PIN_ASSIGNMENT_NONE;
    }
    if unsafe { (*tc).mode == TC_PORT_TBT_ALT } {
        return PIN_ASSIGNMENT_NONE;
    }
    let display = unsafe { display_from_dig_port((*tc).dig_port) };
    let reg = crate::intel_display_regs_upstream::PORT_TX_DFLEXPA1((*tc).phy_fia as u32);
    let mask = crate::intel_display_regs_upstream::DP_PIN_ASSIGNMENT_MASK((*tc).phy_fia_idx as u32);
    let wakeref = unsafe {
        crate::intel_display_power_upstream::intel_display_power_get(
            display,
            POWER_DOMAIN_DISPLAY_CORE,
        )
    };
    unsafe { assert_display_core_power_enabled(tc) };
    let value = unsafe { crate::intel_de_upstream::intel_de_read(display, reg) };
    unsafe {
        crate::intel_display_power_upstream::intel_display_power_put(
            display,
            POWER_DOMAIN_DISPLAY_CORE,
            wakeref,
        )
    };
    if value == u32::MAX || mask == 0 {
        return PIN_ASSIGNMENT_NONE;
    }
    let pin = ((value & mask) >> mask.trailing_zeros()) as i32;
    match pin {
        PIN_ASSIGNMENT_NONE | PIN_ASSIGNMENT_C | PIN_ASSIGNMENT_D | PIN_ASSIGNMENT_E => pin,
        PIN_ASSIGNMENT_A | PIN_ASSIGNMENT_B | PIN_ASSIGNMENT_F
            if unsafe { DISPLAY_VER(display) <= 11 } =>
        {
            pin
        }
        _ => PIN_ASSIGNMENT_NONE,
    }
}

unsafe fn get_max_lane_count(tc: *mut IntelTcPort) -> i32 {
    if tc.is_null() {
        return 0;
    }
    if unsafe { (*tc).mode != TC_PORT_DP_ALT } {
        return 4;
    }
    lane_mask_to_count(unsafe { get_lane_mask(tc) })
}

fn lane_mask_to_count(lane_mask: u32) -> i32 {
    match lane_mask {
        0x1 | 0x2 | 0x4 | 0x8 => 1,
        0x3 | 0xc => 2,
        0xf => 4,
        _ => 1,
    }
}

unsafe fn read_pin_configuration(tc: *mut IntelTcPort) {
    unsafe {
        (*tc).pin_assignment = get_pin_assignment(tc);
        (*tc).max_lane_count = get_max_lane_count(tc) as u8;
    }
}

unsafe extern "C" fn adlp_tc_phy_get_hw_state(tc: *mut IntelTcPort) {
    if tc.is_null() || unsafe { (*tc).dig_port.is_null() } {
        return;
    }
    let display = unsafe { display_from_dig_port((*tc).dig_port) };
    let domain = unsafe { tc_port_power_domain(tc) };
    let port_wakeref =
        unsafe { crate::intel_display_power_upstream::intel_display_power_get(display, domain) };
    unsafe {
        (*tc).mode = tc_phy_get_current_mode(tc);
        if (*tc).mode != TC_PORT_DISCONNECTED {
            tc_cold_block(tc);
            read_pin_configuration(tc);
        }
        crate::intel_display_power_upstream::intel_display_power_put(display, domain, port_wakeref);
    }
}

unsafe fn tc_phy_verify_legacy_or_dp_alt_mode(tc: *mut IntelTcPort, required_lanes: i32) -> bool {
    let max_lanes = unsafe { get_max_lane_count(tc) };
    match unsafe { (*tc).mode } {
        TC_PORT_LEGACY => max_lanes == 4,
        TC_PORT_DP_ALT => {
            if unsafe { adlp_tc_phy_hpd_live_status(tc) } & tc_mode_bit(TC_PORT_DP_ALT) == 0 {
                return false;
            }
            if max_lanes < required_lanes {
                return false;
            }
            true
        }
        _ => false,
    }
}

unsafe extern "C" fn adlp_tc_phy_connect(tc: *mut IntelTcPort, required_lanes: i32) -> bool {
    if tc.is_null() || unsafe { (*tc).dig_port.is_null() } {
        return false;
    }
    if unsafe { (*tc).mode == TC_PORT_TBT_ALT } {
        unsafe {
            tc_cold_block(tc);
            read_pin_configuration(tc);
        }
        return true;
    }

    let display = unsafe { display_from_dig_port((*tc).dig_port) };
    let domain = unsafe { tc_port_power_domain(tc) };
    let port_wakeref =
        unsafe { crate::intel_display_power_upstream::intel_display_power_get(display, domain) };
    let taken = unsafe { adlp_tc_phy_take_ownership(tc, true) };
    if !taken && unsafe { (*tc).mode != TC_PORT_LEGACY } {
        unsafe {
            crate::intel_display_power_upstream::intel_display_power_put(
                display,
                domain,
                port_wakeref,
            );
        }
        return false;
    }
    let ready = unsafe { adlp_tc_phy_is_ready(tc) };
    if !ready && unsafe { (*tc).mode != TC_PORT_LEGACY } {
        unsafe {
            adlp_tc_phy_take_ownership(tc, false);
            crate::intel_display_power_upstream::intel_display_power_put(
                display,
                domain,
                port_wakeref,
            );
        }
        return false;
    }

    unsafe {
        tc_cold_block(tc);
        read_pin_configuration(tc);
    }
    if !unsafe { tc_phy_verify_legacy_or_dp_alt_mode(tc, required_lanes) } {
        let wakeref =
            unsafe { ptr::replace(ptr::addr_of_mut!((*tc).lock_wakeref), ptr::null_mut()) };
        unsafe {
            tc_cold_unblock(tc, wakeref);
            adlp_tc_phy_take_ownership(tc, false);
            crate::intel_display_power_upstream::intel_display_power_put(
                display,
                domain,
                port_wakeref,
            );
        }
        return false;
    }
    unsafe {
        crate::intel_display_power_upstream::intel_display_power_put(display, domain, port_wakeref)
    };
    true
}

unsafe extern "C" fn adlp_tc_phy_disconnect(tc: *mut IntelTcPort) {
    if tc.is_null() || unsafe { (*tc).dig_port.is_null() } {
        return;
    }
    let display = unsafe { display_from_dig_port((*tc).dig_port) };
    let domain = unsafe { tc_port_power_domain(tc) };
    let port_wakeref =
        unsafe { crate::intel_display_power_upstream::intel_display_power_get(display, domain) };
    let cold_wakeref =
        unsafe { ptr::replace(ptr::addr_of_mut!((*tc).lock_wakeref), ptr::null_mut()) };
    unsafe { tc_cold_unblock(tc, cold_wakeref) };
    match unsafe { (*tc).mode } {
        TC_PORT_LEGACY | TC_PORT_DP_ALT => {
            let _ = unsafe { adlp_tc_phy_take_ownership(tc, false) };
        }
        TC_PORT_TBT_ALT => {}
        _ => {}
    }
    unsafe {
        crate::intel_display_power_upstream::intel_display_power_put(display, domain, port_wakeref)
    };
}

unsafe extern "C" fn adlp_tc_phy_init(tc: *mut IntelTcPort) {
    unsafe { tc_phy_load_fia_params(tc) };
}

unsafe fn tc_phy_hpd_live_status(tc: *mut IntelTcPort) -> u32 {
    if tc.is_null() {
        return 0;
    }
    let ops = unsafe { (*tc).phy_ops };
    if ops.is_null() {
        return 0;
    }
    let mask = unsafe { (*ops).hpd_live_status.map_or(0, |f| f(tc)) };
    let _ = tk_linux_kapi::assert::warn_on(mask.count_ones() > 1);
    mask
}

unsafe fn tc_phy_is_ready(tc: *mut IntelTcPort) -> bool {
    let ops = unsafe { (*tc).phy_ops };
    !ops.is_null() && unsafe { (*ops).is_ready.is_some_and(|f| f(tc)) }
}

unsafe fn tc_phy_is_owned(tc: *mut IntelTcPort) -> bool {
    let ops = unsafe { (*tc).phy_ops };
    !ops.is_null() && unsafe { (*ops).is_owned.is_some_and(|f| f(tc)) }
}

unsafe fn tc_phy_get_hw_state(tc: *mut IntelTcPort) {
    let ops = unsafe { (*tc).phy_ops };
    if !ops.is_null() {
        if let Some(f) = unsafe { (*ops).get_hw_state } {
            unsafe { f(tc) };
        }
    }
}

#[inline]
fn hpd_mask_to_tc_mode(mask: u32) -> i32 {
    if mask == 0 {
        TC_PORT_DISCONNECTED
    } else {
        31 - mask.leading_zeros() as i32
    }
}

unsafe fn tc_phy_hpd_live_mode(tc: *mut IntelTcPort) -> i32 {
    hpd_mask_to_tc_mode(unsafe { tc_phy_hpd_live_status(tc) })
}

unsafe fn default_tc_mode(tc: *mut IntelTcPort) -> i32 {
    if unsafe { (*tc).legacy_port } {
        TC_PORT_LEGACY
    } else {
        TC_PORT_TBT_ALT
    }
}

unsafe fn hpd_mask_to_target_mode(tc: *mut IntelTcPort, mask: u32) -> i32 {
    let mode = hpd_mask_to_tc_mode(mask);
    if mode != TC_PORT_DISCONNECTED {
        mode
    } else {
        unsafe { default_tc_mode(tc) }
    }
}

unsafe fn tc_phy_get_target_mode(tc: *mut IntelTcPort) -> i32 {
    unsafe { hpd_mask_to_target_mode(tc, tc_phy_hpd_live_status(tc)) }
}

unsafe fn tc_phy_owned_by_display(tc: *mut IntelTcPort, ready: bool, owned: bool) -> bool {
    let display = unsafe { display_from_dig_port((*tc).dig_port) };
    if unsafe { DISPLAY_VER(display) < 20 } {
        let _ = tk_linux_kapi::assert::warn_on(owned && !ready);
        ready && owned
    } else {
        owned
    }
}

unsafe fn get_tc_mode_in_phy_owned_state(tc: *mut IntelTcPort, live_mode: i32) -> i32 {
    match live_mode {
        TC_PORT_LEGACY | TC_PORT_DP_ALT => live_mode,
        TC_PORT_TBT_ALT | TC_PORT_DISCONNECTED => {
            if unsafe { (*tc).legacy_port } {
                TC_PORT_LEGACY
            } else {
                TC_PORT_DP_ALT
            }
        }
        _ => {
            if unsafe { (*tc).legacy_port } {
                TC_PORT_LEGACY
            } else {
                TC_PORT_DP_ALT
            }
        }
    }
}

unsafe fn get_tc_mode_in_phy_not_owned_state(tc: *mut IntelTcPort, live_mode: i32) -> i32 {
    match live_mode {
        TC_PORT_LEGACY => TC_PORT_DISCONNECTED,
        TC_PORT_DP_ALT | TC_PORT_TBT_ALT => TC_PORT_TBT_ALT,
        TC_PORT_DISCONNECTED => {
            if unsafe { (*tc).legacy_port } {
                TC_PORT_DISCONNECTED
            } else {
                TC_PORT_TBT_ALT
            }
        }
        _ => TC_PORT_DISCONNECTED,
    }
}

unsafe fn tc_phy_wait_for_ready(tc: *mut IntelTcPort) -> bool {
    let mut ready = false;
    let ret = tk_linux_kapi::poll_timeout_us!(
        ready = unsafe { tc_phy_is_ready(tc) },
        ready,
        1_000,
        500_000,
        false,
    );
    ret == 0
}

unsafe fn tc_phy_get_current_mode(tc: *mut IntelTcPort) -> i32 {
    let live_mode = unsafe { tc_phy_hpd_live_mode(tc) };
    if unsafe { (*tc).legacy_port } {
        let _ = unsafe { tc_phy_wait_for_ready(tc) };
    }
    let ready = unsafe { tc_phy_is_ready(tc) };
    let owned = unsafe { tc_phy_is_owned(tc) };
    if unsafe { tc_phy_owned_by_display(tc, ready, owned) } {
        let _ = tk_linux_kapi::assert::warn_on(live_mode == TC_PORT_TBT_ALT);
        unsafe { get_tc_mode_in_phy_owned_state(tc, live_mode) }
    } else {
        unsafe { get_tc_mode_in_phy_not_owned_state(tc, live_mode) }
    }
}

unsafe fn tc_phy_get_target_mode_name(tc: *mut IntelTcPort) -> i32 {
    unsafe { tc_phy_get_target_mode(tc) }
}

unsafe fn tc_port_fixup_legacy_flag(tc: *mut IntelTcPort, live_status: u32) {
    if tc.is_null() || live_status.count_ones() != 1 {
        return;
    }
    let valid = if unsafe { (*tc).legacy_port } {
        tc_mode_bit(TC_PORT_LEGACY)
    } else {
        tc_mode_bit(TC_PORT_DP_ALT) | tc_mode_bit(TC_PORT_TBT_ALT)
    };
    if live_status & !valid == 0 {
        return;
    }
    unsafe { (*tc).legacy_port = !(*tc).legacy_port };
}

unsafe fn tc_phy_connect(tc: *mut IntelTcPort, required_lanes: i32) -> bool {
    if tc.is_null() {
        return false;
    }
    let live_status = unsafe { tc_phy_hpd_live_status(tc) };
    unsafe {
        tc_port_fixup_legacy_flag(tc, live_status);
        (*tc).mode = hpd_mask_to_target_mode(tc, live_status);
    }
    let ops = unsafe { (*tc).phy_ops };
    if ops.is_null() || unsafe { (*ops).connect.is_none() } {
        return false;
    }
    let connect = unsafe { (*ops).connect.unwrap() };
    let mut connected = unsafe { connect(tc, required_lanes) };
    let default_mode = unsafe { default_tc_mode(tc) };
    if !connected && unsafe { (*tc).mode != default_mode } {
        unsafe { (*tc).mode = default_mode };
        connected = unsafe { connect(tc, required_lanes) };
    }
    let _ = tk_linux_kapi::assert::warn_on(!connected);
    connected
}

unsafe fn tc_phy_disconnect(tc: *mut IntelTcPort) {
    if tc.is_null() || unsafe { (*tc).mode == TC_PORT_DISCONNECTED } {
        return;
    }
    let ops = unsafe { (*tc).phy_ops };
    if !ops.is_null() {
        if let Some(disconnect) = unsafe { (*ops).disconnect } {
            unsafe { disconnect(tc) };
        }
    }
    unsafe { (*tc).mode = TC_PORT_DISCONNECTED };
}

unsafe fn tc_phy_init(tc: *mut IntelTcPort) {
    if tc.is_null() {
        return;
    }
    tk_linux_kapi::mutex::mutex_lock(unsafe { ptr::addr_of_mut!((*tc).lock) });
    let ops = unsafe { (*tc).phy_ops };
    if !ops.is_null() {
        if let Some(init) = unsafe { (*ops).init } {
            unsafe { init(tc) };
        }
    }
    tk_linux_kapi::mutex::mutex_unlock(unsafe { ptr::addr_of_mut!((*tc).lock) });
}

unsafe fn tc_port_is_enabled(tc: *mut IntelTcPort) -> bool {
    if tc.is_null() || unsafe { (*tc).dig_port.is_null() } {
        return false;
    }
    let display = unsafe { display_from_dig_port((*tc).dig_port) };
    let port = unsafe { (*(*tc).dig_port).base.port };
    unsafe { assert_tc_port_power_enabled(tc) };
    unsafe {
        crate::intel_de_upstream::intel_de_read(
            display,
            crate::intel_display_regs_upstream::DDI_BUF_CTL(port as u32),
        ) & crate::intel_display_regs_upstream::DDI_BUF_CTL_ENABLE
            != 0
    }
}

unsafe fn intel_tc_port_reset_mode(
    tc: *mut IntelTcPort,
    required_lanes: i32,
    force_disconnect: bool,
) {
    if tc.is_null() || unsafe { (*tc).dig_port.is_null() } {
        return;
    }
    let display = unsafe { display_from_dig_port((*tc).dig_port) };
    unsafe { crate::intel_display_power_upstream::intel_display_power_flush_work(display) };
    unsafe {
        tc_phy_disconnect(tc);
        if !force_disconnect {
            let _ = tc_phy_connect(tc, required_lanes);
        }
    }
}

unsafe fn intel_tc_port_needs_reset(tc: *mut IntelTcPort) -> bool {
    !tc.is_null() && unsafe { tc_phy_get_target_mode(tc) != (*tc).mode }
}

unsafe fn intel_tc_port_update_mode(
    tc: *mut IntelTcPort,
    required_lanes: i32,
    force_disconnect: bool,
) {
    if force_disconnect || unsafe { intel_tc_port_needs_reset(tc) } {
        unsafe { intel_tc_port_reset_mode(tc, required_lanes, force_disconnect) };
    }
}

unsafe fn __intel_tc_port_get_link(tc: *mut IntelTcPort) {
    if !tc.is_null() {
        unsafe { (*tc).link_refcount = (*tc).link_refcount.saturating_add(1) };
    }
}

unsafe fn __intel_tc_port_put_link(tc: *mut IntelTcPort) {
    if !tc.is_null() {
        unsafe { (*tc).link_refcount = (*tc).link_refcount.saturating_sub(1) };
    }
}

unsafe fn tc_phy_is_connected(tc: *mut IntelTcPort, pll_type: IclPortDpllId) -> bool {
    let ready = unsafe { tc_phy_is_ready(tc) };
    let owned = unsafe { tc_phy_is_owned(tc) };
    let by_display = unsafe { tc_phy_owned_by_display(tc, ready, owned) };
    if by_display {
        pll_type == ICL_PORT_DPLL_MG_PHY
    } else {
        pll_type == ICL_PORT_DPLL_DEFAULT
    }
}

unsafe fn tc_phy_get_target_mode_for_reset(tc: *mut IntelTcPort) -> i32 {
    unsafe { tc_phy_get_target_mode(tc) }
}

unsafe fn __intel_tc_port_link_needs_reset(tc: *mut IntelTcPort) -> bool {
    if tc.is_null() {
        return false;
    }
    tk_linux_kapi::mutex::mutex_lock(unsafe { ptr::addr_of_mut!((*tc).lock) });
    let needs = unsafe {
        (*tc).link_refcount != 0
            && (*tc).mode == TC_PORT_DP_ALT
            && tc_phy_get_target_mode_for_reset(tc) != (*tc).mode
    };
    tk_linux_kapi::mutex::mutex_unlock(unsafe { ptr::addr_of_mut!((*tc).lock) });
    needs
}

#[inline]
unsafe fn tc_port_in_mode(dig_port: *mut IntelDigitalPort, mode: i32) -> bool {
    if dig_port.is_null() || !unsafe { encoder_is_tc(ptr::addr_of_mut!((*dig_port).base)) } {
        return false;
    }
    let tc = unsafe { tc_port_state(dig_port) };
    !tc.is_null() && unsafe { (*tc).mode == mode }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_tc_port_in_tbt_alt_mode(dig_port: *mut IntelDigitalPort) -> bool {
    unsafe { tc_port_in_mode(dig_port, TC_PORT_TBT_ALT) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_tc_port_in_dp_alt_mode(dig_port: *mut IntelDigitalPort) -> bool {
    unsafe { tc_port_in_mode(dig_port, TC_PORT_DP_ALT) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_tc_port_in_legacy_mode(dig_port: *mut IntelDigitalPort) -> bool {
    unsafe { tc_port_in_mode(dig_port, TC_PORT_LEGACY) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_tc_port_handles_hpd_glitches(
    dig_port: *mut IntelDigitalPort,
) -> bool {
    !dig_port.is_null()
        && unsafe { encoder_is_tc(ptr::addr_of_mut!((*dig_port).base)) }
        && unsafe { !(*tc_port_state(dig_port)).legacy_port }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_tc_cold_requires_aux_pw(dig_port: *mut IntelDigitalPort) -> bool {
    if dig_port.is_null() || !unsafe { encoder_is_tc(ptr::addr_of_mut!((*dig_port).base)) } {
        return false;
    }
    let tc = unsafe { tc_port_state(dig_port) };
    let display = unsafe { display_from_dig_port(dig_port) };
    unsafe {
        tc_phy_cold_off_domain(tc)
            == crate::intel_display_power_upstream::intel_display_power_legacy_aux_domain(
                display,
                (*dig_port).aux_ch,
            )
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_tc_port_max_lane_count(dig_port: *mut IntelDigitalPort) -> i32 {
    if dig_port.is_null() || !unsafe { encoder_is_tc(ptr::addr_of_mut!((*dig_port).base)) } {
        return 4;
    }
    unsafe { (*tc_port_state(dig_port)).max_lane_count as i32 }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_tc_port_get_pin_assignment(dig_port: *mut IntelDigitalPort) -> i32 {
    if dig_port.is_null() || !unsafe { encoder_is_tc(ptr::addr_of_mut!((*dig_port).base)) } {
        return PIN_ASSIGNMENT_NONE;
    }
    unsafe { (*tc_port_state(dig_port)).pin_assignment }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_tc_port_set_fia_lane_count(
    dig_port: *mut IntelDigitalPort,
    required_lanes: i32,
) {
    if dig_port.is_null() || !unsafe { encoder_is_tc(ptr::addr_of_mut!((*dig_port).base)) } {
        return;
    }
    let tc = unsafe { tc_port_state(dig_port) };
    let display = unsafe { display_from_dig_port(dig_port) };
    if display.is_null() || unsafe { DISPLAY_VER(display) >= 14 } {
        return;
    }
    let lane_reversal = unsafe { (*dig_port).lane_reversal };
    let _ =
        tk_linux_kapi::assert::warn_on(lane_reversal && unsafe { (*tc).mode != TC_PORT_LEGACY });
    unsafe { assert_tc_cold_blocked(tc) };
    let idx = unsafe { (*tc).phy_fia_idx as u32 };
    let reg =
        crate::intel_mg_phy_regs_upstream::PORT_TX_DFLEXDPMLE1(unsafe { (*tc).phy_fia as u32 });
    let mask = crate::intel_mg_phy_regs_upstream::DFLEXDPMLE1_DPMLETC_MASK(idx);
    let lane_value = match required_lanes {
        1 if lane_reversal => crate::intel_mg_phy_regs_upstream::DFLEXDPMLE1_DPMLETC_ML3(idx),
        1 => crate::intel_mg_phy_regs_upstream::DFLEXDPMLE1_DPMLETC_ML0(idx),
        2 if lane_reversal => crate::intel_mg_phy_regs_upstream::DFLEXDPMLE1_DPMLETC_ML3_2(idx),
        2 => crate::intel_mg_phy_regs_upstream::DFLEXDPMLE1_DPMLETC_ML1_0(idx),
        4 => crate::intel_mg_phy_regs_upstream::DFLEXDPMLE1_DPMLETC_ML3_0(idx),
        _ => {
            let _ = tk_linux_kapi::assert::warn_on(true);
            return;
        }
    };
    let wakeref = unsafe {
        crate::intel_display_power_upstream::intel_display_power_get(
            display,
            POWER_DOMAIN_DISPLAY_CORE,
        )
    };
    unsafe { assert_display_core_power_enabled(tc) };
    let mut value = unsafe { crate::intel_de_upstream::intel_de_read(display, reg) } & !mask;
    value |= lane_value;
    unsafe { crate::intel_de_upstream::intel_de_write(display, reg, value) };
    unsafe {
        crate::intel_display_power_upstream::intel_display_power_put(
            display,
            POWER_DOMAIN_DISPLAY_CORE,
            wakeref,
        )
    };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_tc_port_init_mode(dig_port: *mut IntelDigitalPort) {
    if dig_port.is_null() {
        return;
    }
    let tc = unsafe { tc_port_state(dig_port) };
    if tc.is_null() {
        return;
    }
    let lock = unsafe { ptr::addr_of_mut!((*tc).lock) };
    unsafe { tk_linux_kapi::mutex::mutex_lock(lock) };
    let _ = tk_linux_kapi::assert::warn_on(unsafe {
        (*tc).mode != TC_PORT_DISCONNECTED
            || !(*tc).lock_wakeref.is_null()
            || (*tc).link_refcount != 0
    });
    unsafe { tc_phy_get_hw_state(tc) };
    unsafe { (*tc).init_mode = (*tc).mode };
    let mut update = !unsafe { tc_port_is_enabled(tc) };
    if !update && unsafe { (*tc).mode == TC_PORT_DISCONNECTED } {
        let _ = tk_linux_kapi::assert::warn_on(!unsafe { (*tc).legacy_port });
        update = true;
    }
    if update {
        unsafe { intel_tc_port_update_mode(tc, 1, false) };
    }
    unsafe {
        __intel_tc_port_get_link(tc);
        tk_linux_kapi::mutex::mutex_unlock(lock);
    }
}

unsafe fn tc_port_has_active_streams(
    tc: *mut IntelTcPort,
    crtc_state: *const IntelCrtcState,
) -> bool {
    if tc.is_null() || unsafe { (*tc).dig_port.is_null() } {
        return false;
    }
    let dig_port = unsafe { (*tc).dig_port };
    let mut active = false;
    let mut pll_type = ICL_PORT_DPLL_DEFAULT;
    if unsafe { (*dig_port).dp.is_mst } {
        active = unsafe { intel_dp_mst_active_streams(ptr::addr_of_mut!((*dig_port).dp)) > 0 };
    } else if !crtc_state.is_null() && unsafe { (*crtc_state).hw.active } {
        pll_type = unsafe { intel_ddi_port_pll_type(&mut (*dig_port).base, crtc_state) };
        active = true;
    }
    if active && !unsafe { tc_phy_is_connected(tc, pll_type) } {
        let _ = tk_linux_kapi::assert::warn_on(true);
    }
    active
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_tc_port_sanitize_mode(
    dig_port: *mut IntelDigitalPort,
    crtc_state: *const IntelCrtcState,
) {
    if dig_port.is_null() {
        return;
    }
    let tc = unsafe { tc_port_state(dig_port) };
    if tc.is_null() {
        return;
    }
    let lock = unsafe { ptr::addr_of_mut!((*tc).lock) };
    unsafe { tk_linux_kapi::mutex::mutex_lock(lock) };
    let _ = tk_linux_kapi::assert::warn_on(unsafe { (*tc).link_refcount != 1 });
    if !unsafe { tc_port_has_active_streams(tc, crtc_state) } {
        unsafe {
            tc_phy_disconnect(tc);
            __intel_tc_port_put_link(tc);
        }
    }
    unsafe { tk_linux_kapi::mutex::mutex_unlock(lock) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_tc_port_connected(encoder: *mut IntelEncoder) -> bool {
    if encoder.is_null() || !unsafe { encoder_is_tc(encoder) } {
        return false;
    }
    let dig_port = unsafe { dig_port_from_encoder(encoder) };
    let tc = unsafe { tc_port_state(dig_port) };
    let _ = tk_linux_kapi::assert::warn_on(!unsafe { intel_tc_port_ref_held(dig_port) });
    let mut mask = u32::MAX;
    if unsafe { (*tc).mode != TC_PORT_DISCONNECTED } {
        mask = tc_mode_bit(unsafe { (*tc).mode });
    }
    unsafe { tc_phy_hpd_live_status(tc) & mask != 0 }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_tc_port_link_needs_reset(dig_port: *mut IntelDigitalPort) -> bool {
    if dig_port.is_null()
        || unsafe { (*dig_port).dedicated_external }
        || unsafe { tc_index(dig_port).is_none() }
    {
        return false;
    }
    unsafe { __intel_tc_port_link_needs_reset(tc_port_state(dig_port)) }
}

unsafe fn reset_link_commit(
    tc: *mut IntelTcPort,
    state: *mut IntelAtomicState,
    ctx: *mut DrmModesetAcquireCtx,
) -> i32 {
    let display = unsafe { display_from_dig_port((*tc).dig_port) };
    let dev = unsafe { (*display).drm };
    let dig_port = unsafe { (*tc).dig_port };
    let dp = ptr::addr_of_mut!((*dig_port).dp);
    let ret =
        unsafe { drm_modeset_lock(ptr::addr_of_mut!((*dev).mode_config.connection_mutex), ctx) };
    if ret != 0 {
        return ret;
    }
    let mut pipe_mask = 0u8;
    let ret = unsafe { intel_dp_get_active_pipes(dp, ctx, &mut pipe_mask) };
    if ret != 0 || pipe_mask == 0 {
        return ret;
    }
    for pipe in 0..8 {
        if pipe_mask & (1 << pipe) == 0 {
            continue;
        }
        let crtc = unsafe { crate::intel_crtc_upstream::intel_crtc_for_pipe(display, pipe as i32) };
        if crtc.is_null() {
            continue;
        }
        let crtc_state = unsafe {
            crate::intel_atomic_upstream::intel_atomic_get_crtc_state(&mut (*state).base, crtc)
        };
        if unsafe { tk_linux_kapi::config::IS_ERR(crtc_state) } {
            return unsafe { tk_linux_kapi::config::PTR_ERR(crtc_state) as i32 };
        }
        unsafe { (*crtc_state).uapi.set_connectors_changed(true) };
    }
    if unsafe { !__intel_tc_port_link_needs_reset(tc) } {
        return 0;
    }
    unsafe { drm_atomic_commit(&mut (*state).base) }
}

unsafe fn reset_link(tc: *mut IntelTcPort) -> i32 {
    let display = unsafe { display_from_dig_port((*tc).dig_port) };
    let dev = unsafe { (*display).drm };
    let base = unsafe { drm_atomic_commit_alloc(dev) };
    if base.is_null() {
        return -ENOMEM;
    }
    let state = base.cast::<IntelAtomicState>();
    unsafe { (*state).internal = true };
    let mut ctx: DrmModesetAcquireCtx = unsafe { core::mem::zeroed() };
    unsafe { drm_modeset_acquire_init(&mut ctx, 0) };
    unsafe { (*state).base.acquire_ctx = &mut ctx };
    let mut ret = 0;
    loop {
        ret = unsafe { reset_link_commit(tc, state, &mut ctx) };
        if ret != -EDEADLK {
            break;
        }
        unsafe { crate::intel_atomic_upstream::intel_atomic_state_clear(&mut (*state).base) };
        let backoff = unsafe { drm_modeset_backoff(&mut ctx) };
        if backoff != 0 {
            ret = backoff;
            break;
        }
    }
    unsafe {
        drm_modeset_drop_locks(&mut ctx);
        drm_modeset_acquire_fini(&mut ctx);
        drm_atomic_commit_put(&mut (*state).base);
    }
    ret
}

unsafe extern "C" fn intel_tc_port_link_reset_work(work: *mut WorkStruct) {
    if work.is_null() {
        return;
    }
    let delayed = unsafe {
        work.cast::<u8>()
            .sub(offset_of!(DelayedWork, work))
            .cast::<DelayedWork>()
    };
    let tc = unsafe {
        delayed
            .cast::<u8>()
            .sub(offset_of!(IntelTcPort, link_reset_work))
            .cast::<IntelTcPort>()
    };
    if unsafe { !__intel_tc_port_link_needs_reset(tc) } {
        return;
    }
    let display = unsafe { display_from_dig_port((*tc).dig_port) };
    if display.is_null() || unsafe { (*display).drm.is_null() } {
        return;
    }
    let mutex = unsafe { ptr::addr_of_mut!((*(*display).drm).mode_config.mutex) };
    tk_linux_kapi::mutex::mutex_lock(mutex);
    let ret = unsafe { reset_link(tc) };
    let _ = tk_linux_kapi::assert::warn_on(ret != 0);
    tk_linux_kapi::mutex::mutex_unlock(mutex);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_tc_port_link_reset(dig_port: *mut IntelDigitalPort) -> bool {
    if !unsafe { intel_tc_port_link_needs_reset(dig_port) } {
        return false;
    }
    let tc = unsafe { tc_port_state(dig_port) };
    unsafe {
        tk_linux_kapi::workqueue::queue_delayed_work(
            tk_linux_kapi::workqueue::system_dfl_wq,
            ptr::addr_of_mut!((*tc).link_reset_work),
            tk_linux_kapi::primitives::msecs_to_jiffies(2_000u32),
        );
    }
    true
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_tc_port_link_cancel_reset_work(dig_port: *mut IntelDigitalPort) {
    if dig_port.is_null() || !unsafe { encoder_is_tc(ptr::addr_of_mut!((*dig_port).base)) } {
        return;
    }
    let tc = unsafe { tc_port_state(dig_port) };
    unsafe { tk_linux_kapi::workqueue::cancel_delayed_work(&mut (*tc).link_reset_work) };
}

unsafe fn intel_tc_port_lock_internal(tc: *mut IntelTcPort, required_lanes: i32) {
    if tc.is_null() {
        return;
    }
    let lock = unsafe { ptr::addr_of_mut!((*tc).lock) };
    unsafe { tk_linux_kapi::mutex::mutex_lock(lock) };
    unsafe {
        tk_linux_kapi::workqueue::cancel_delayed_work(&mut (*tc).disconnect_phy_work);
        if (*tc).link_refcount == 0 {
            intel_tc_port_update_mode(tc, required_lanes, false);
        }
    }
    let _ = tk_linux_kapi::assert::warn_on(unsafe {
        (*tc).mode == TC_PORT_DISCONNECTED
            || ((*tc).mode != TC_PORT_TBT_ALT && !tc_phy_is_owned(tc))
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_tc_port_lock(dig_port: *mut IntelDigitalPort) {
    if dig_port.is_null() {
        return;
    }
    unsafe { intel_tc_port_lock_internal(tc_port_state(dig_port), 1) };
}

unsafe extern "C" fn intel_tc_port_disconnect_phy_work(work: *mut WorkStruct) {
    if work.is_null() {
        return;
    }
    let delayed = unsafe {
        work.cast::<u8>()
            .sub(offset_of!(DelayedWork, work))
            .cast::<DelayedWork>()
    };
    let tc = unsafe {
        delayed
            .cast::<u8>()
            .sub(offset_of!(IntelTcPort, disconnect_phy_work))
            .cast::<IntelTcPort>()
    };
    let lock = unsafe { ptr::addr_of_mut!((*tc).lock) };
    unsafe { tk_linux_kapi::mutex::mutex_lock(lock) };
    if unsafe { (*tc).link_refcount == 0 } {
        unsafe { intel_tc_port_update_mode(tc, 1, true) };
    }
    unsafe { tk_linux_kapi::mutex::mutex_unlock(lock) };
}

unsafe fn intel_tc_port_flush_work(dig_port: *mut IntelDigitalPort) {
    if dig_port.is_null() {
        return;
    }
    let tc = unsafe { tc_port_state(dig_port) };
    if !tc.is_null() {
        unsafe { tk_linux_kapi::workqueue::flush_delayed_work(&mut (*tc).disconnect_phy_work) };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_tc_port_suspend(dig_port: *mut IntelDigitalPort) {
    if dig_port.is_null() {
        return;
    }
    let tc = unsafe { tc_port_state(dig_port) };
    if tc.is_null() {
        return;
    }
    unsafe { tk_linux_kapi::workqueue::cancel_delayed_work_sync(&mut (*tc).link_reset_work) };
    unsafe { intel_tc_port_flush_work(dig_port) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_tc_port_unlock(dig_port: *mut IntelDigitalPort) {
    if dig_port.is_null() {
        return;
    }
    let tc = unsafe { tc_port_state(dig_port) };
    if tc.is_null() {
        return;
    }
    if unsafe { (*tc).link_refcount == 0 && (*tc).mode != TC_PORT_DISCONNECTED } {
        unsafe {
            tk_linux_kapi::workqueue::queue_delayed_work(
                tk_linux_kapi::workqueue::system_dfl_wq,
                ptr::addr_of_mut!((*tc).disconnect_phy_work),
                tk_linux_kapi::primitives::msecs_to_jiffies(1_000u32),
            );
        }
    }
    unsafe { tk_linux_kapi::mutex::mutex_unlock(ptr::addr_of_mut!((*tc).lock)) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_tc_port_ref_held(dig_port: *mut IntelDigitalPort) -> bool {
    if dig_port.is_null() {
        return false;
    }
    let tc = unsafe { tc_port_state(dig_port) };
    !tc.is_null()
        && (unsafe { tk_linux_kapi::mutex::mutex_is_locked(ptr::addr_of_mut!((*tc).lock)) }
            || unsafe { (*tc).link_refcount != 0 })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_tc_port_get_link(
    dig_port: *mut IntelDigitalPort,
    required_lanes: i32,
) {
    if dig_port.is_null() {
        return;
    }
    let tc = unsafe { tc_port_state(dig_port) };
    if tc.is_null() {
        return;
    }
    unsafe {
        intel_tc_port_lock_internal(tc, required_lanes);
        __intel_tc_port_get_link(tc);
        intel_tc_port_unlock(dig_port);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_tc_port_put_link(dig_port: *mut IntelDigitalPort) {
    if dig_port.is_null() {
        return;
    }
    let tc = unsafe { tc_port_state(dig_port) };
    if tc.is_null() {
        return;
    }
    unsafe {
        intel_tc_port_lock_internal(tc, 1);
        __intel_tc_port_put_link(tc);
        intel_tc_port_unlock(dig_port);
        intel_tc_port_flush_work(dig_port);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_tc_info(
    printer: *mut tk_drm_upstream::drm_print_types::DrmPrinter,
    dig_port: *mut IntelDigitalPort,
) {
    if printer.is_null() || dig_port.is_null() {
        return;
    }
    let tc = unsafe { tc_port_state(dig_port) };
    if tc.is_null() {
        return;
    }
    unsafe { intel_tc_port_lock_internal(tc, 1) };
    unsafe {
        tk_drm_upstream::drm_print_upstream::drm_printf(
            printer,
            &alloc::format!(
                "\tTC Port {}: mode: {}, pin assignment: {}, max lanes: {}\n",
                core::ffi::CStr::from_ptr((*tc).port_name).to_string_lossy(),
                tc_port_mode_name((*tc).mode),
                pin_assignment_name((*tc).pin_assignment),
                (*tc).max_lane_count,
            ),
        );
        intel_tc_port_unlock(dig_port);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_tc_port_init(
    dig_port: *mut IntelDigitalPort,
    is_legacy: bool,
) -> i32 {
    if dig_port.is_null() || !unsafe { encoder_is_tc(ptr::addr_of_mut!((*dig_port).base)) } {
        return -EINVAL;
    }
    let display = unsafe { display_from_dig_port(dig_port) };
    if display.is_null()
        || unsafe { DISPLAY_VER(display) != 13 || !adlp_platform(display) }
        || !unsafe { (*dig_port).tc.is_null() }
    {
        return -EINVAL;
    }
    let Some(index) = (unsafe { tc_index(dig_port) }) else {
        return -EINVAL;
    };
    let tc = tk_linux_kapi::memory::kzalloc_obj::<IntelTcPort>();
    if tc.is_null() {
        return -ENOMEM;
    }
    let letter = b'D' + index as u8;
    let name = tk_linux_kapi::memory::kmalloc(7, tk_linux_kapi::config::GFP_KERNEL).cast::<u8>();
    if name.is_null() {
        unsafe { tk_linux_kapi::memory::kfree(tc) };
        return -ENOMEM;
    }
    unsafe {
        name.write(letter);
        name.add(1).write(b'/');
        name.add(2).write(b'T');
        name.add(3).write(b'C');
        name.add(4).write(b'#');
        name.add(5).write(b'1' + index as u8);
        name.add(6).write(0);
        (*tc).dig_port = dig_port;
        (*tc).phy_ops = &ADLP_TC_PHY_OPS;
        (*tc).port_name = name.cast();
        (*tc).legacy_port = is_legacy;
        (*tc).mode = TC_PORT_DISCONNECTED;
        (*tc).link_refcount = 0;
        (*dig_port).tc = tc.cast::<c_void>();
        tk_linux_kapi::mutex::mutex_init(ptr::addr_of_mut!((*tc).lock));
        tk_linux_kapi::workqueue::INIT_DELAYED_WORK(
            &mut (*tc).disconnect_phy_work,
            |work| unsafe { intel_tc_port_disconnect_phy_work(work as *mut WorkStruct) },
        );
        tk_linux_kapi::workqueue::INIT_DELAYED_WORK(&mut (*tc).link_reset_work, |work| unsafe {
            intel_tc_port_link_reset_work(work as *mut WorkStruct)
        });
    }
    unsafe { tc_phy_init(tc) };
    unsafe { intel_tc_port_init_mode(dig_port) };
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_tc_port_cleanup(dig_port: *mut IntelDigitalPort) {
    if dig_port.is_null() {
        return;
    }
    let tc = unsafe { tc_port_state(dig_port) };
    if tc.is_null() {
        return;
    }
    unsafe {
        intel_tc_port_suspend(dig_port);
        tk_linux_kapi::memory::kfree((*tc).port_name.cast_mut());
        (*dig_port).tc = ptr::null_mut();
        tk_linux_kapi::memory::kfree(tc);
    }
}

#[cfg(test)]
mod tc_mode_tests {
    use super::*;

    #[test]
    fn live_hpd_mode_uses_mode_bit_ordinals() {
        assert_eq!(
            hpd_mask_to_tc_mode(tc_mode_bit(TC_PORT_TBT_ALT)),
            TC_PORT_TBT_ALT
        );
        assert_eq!(
            hpd_mask_to_tc_mode(tc_mode_bit(TC_PORT_DP_ALT)),
            TC_PORT_DP_ALT
        );
        assert_eq!(
            hpd_mask_to_tc_mode(tc_mode_bit(TC_PORT_LEGACY)),
            TC_PORT_LEGACY
        );
        assert_eq!(hpd_mask_to_tc_mode(0), TC_PORT_DISCONNECTED);
    }

    #[test]
    fn lane_masks_map_to_expected_dp_lane_counts() {
        assert_eq!(lane_mask_to_count(0x1), 1);
        assert_eq!(lane_mask_to_count(0x8), 1);
        assert_eq!(lane_mask_to_count(0x3), 2);
        assert_eq!(lane_mask_to_count(0xc), 2);
        assert_eq!(lane_mask_to_count(0xf), 4);
        assert_eq!(lane_mask_to_count(0), 1);
    }
}
