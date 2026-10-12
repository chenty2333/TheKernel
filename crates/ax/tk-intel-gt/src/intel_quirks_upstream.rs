// SPDX-License-Identifier: MIT
// Copyright © 2018 Intel Corporation
// Source: drivers/gpu/drm/i915/display/intel_quirks.c and intel_quirks.h

#![allow(non_snake_case, dead_code, unsafe_code)]

use core::ffi::c_void;
use crate::intel_display_core_types_upstream::IntelDisplay;

#[repr(C)]
pub struct PciDev {
    pub _pad_0: [u8; 56],
    pub devfn: u32,
    pub vendor: u16,
    pub device: u16,
    pub subsystem_vendor: u16,
    pub subsystem_device: u16,
    pub class: u32,
    pub revision: u8,
}

#[repr(u32)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum IntelQuirkId {
    QuirkBacklightPresent = 0,
    QuirkIncreaseDdiDisabledTime = 1,
    QuirkIncreaseT12Delay = 2,
    QuirkInvertBrightness = 3,
    QuirkLvdsSscDisable = 4,
    QuirkNoPpsBacklightPowerHook = 5,
    QuirkFwSyncLen = 6,
    QuirkEdpLimitRateHbr2 = 7,
    QuirkDisableEdpPanelReplay = 8,
    QuirkDisablePsr2 = 9,
}

const PCI_ANY_ID: i32 = -1;
const DEVICE_ID_ANY: i32 = 0;

#[inline]
unsafe fn intel_set_quirk(display: *mut IntelDisplay, quirk: IntelQuirkId) {
    if !display.is_null() {
        (*display).quirks.mask |= 1u64 << (quirk as u32);
    }
}

unsafe fn quirk_ssc_force_disable(display: *mut IntelDisplay) {
    intel_set_quirk(display, IntelQuirkId::QuirkLvdsSscDisable);
}

unsafe fn quirk_invert_brightness(display: *mut IntelDisplay) {
    intel_set_quirk(display, IntelQuirkId::QuirkInvertBrightness);
}

unsafe fn quirk_backlight_present(display: *mut IntelDisplay) {
    intel_set_quirk(display, IntelQuirkId::QuirkBacklightPresent);
}

unsafe fn quirk_increase_t12_delay(display: *mut IntelDisplay) {
    intel_set_quirk(display, IntelQuirkId::QuirkIncreaseT12Delay);
}

unsafe fn quirk_increase_ddi_disabled_time(display: *mut IntelDisplay) {
    intel_set_quirk(display, IntelQuirkId::QuirkIncreaseDdiDisabledTime);
}

unsafe fn quirk_no_pps_backlight_power_hook(display: *mut IntelDisplay) {
    intel_set_quirk(display, IntelQuirkId::QuirkNoPpsBacklightPowerHook);
}

unsafe fn quirk_edp_limit_rate_hbr2(display: *mut IntelDisplay) {
    intel_set_quirk(display, IntelQuirkId::QuirkEdpLimitRateHbr2);
}

unsafe fn quirk_disable_psr2(display: *mut IntelDisplay) {
    intel_set_quirk(display, IntelQuirkId::QuirkDisablePsr2);
}

struct IntelQuirk {
    device: i32,
    subsystem_vendor: i32,
    subsystem_device: i32,
    hook: unsafe fn(*mut IntelDisplay),
}

static INTEL_QUIRKS: &[IntelQuirk] = &[
    // Lenovo U160 cannot use SSC on LVDS
    IntelQuirk { device: 0x0046, subsystem_vendor: 0x17aa, subsystem_device: 0x3920, hook: quirk_ssc_force_disable },
    // Sony Vaio Y cannot use SSC on LVDS
    IntelQuirk { device: 0x0046, subsystem_vendor: 0x104d, subsystem_device: 0x9076, hook: quirk_ssc_force_disable },
    // Acer Aspire 5734Z must invert backlight brightness
    IntelQuirk { device: 0x2a42, subsystem_vendor: 0x1025, subsystem_device: 0x0459, hook: quirk_invert_brightness },
    // Acer/eMachines G725
    IntelQuirk { device: 0x2a42, subsystem_vendor: 0x1025, subsystem_device: 0x0210, hook: quirk_invert_brightness },
    // Acer/eMachines e725
    IntelQuirk { device: 0x2a42, subsystem_vendor: 0x1025, subsystem_device: 0x0212, hook: quirk_invert_brightness },
    // Acer/Packard Bell NCL20
    IntelQuirk { device: 0x2a42, subsystem_vendor: 0x1025, subsystem_device: 0x034b, hook: quirk_invert_brightness },
    // Acer Aspire 4736Z
    IntelQuirk { device: 0x2a42, subsystem_vendor: 0x1025, subsystem_device: 0x0260, hook: quirk_invert_brightness },
    // Acer Aspire 5336
    IntelQuirk { device: 0x2a42, subsystem_vendor: 0x1025, subsystem_device: 0x048a, hook: quirk_invert_brightness },
    // Acer C720 and C720P Chromebooks (Celeron 2955U) have backlights
    IntelQuirk { device: 0x0a06, subsystem_vendor: 0x1025, subsystem_device: 0x0a11, hook: quirk_backlight_present },
    // Acer C720 Chromebook (Core i3 4005U)
    IntelQuirk { device: 0x0a16, subsystem_vendor: 0x1025, subsystem_device: 0x0a11, hook: quirk_backlight_present },
    // Apple Macbook 2,1 (Core 2 T7400)
    IntelQuirk { device: 0x27a2, subsystem_vendor: 0x8086, subsystem_device: 0x7270, hook: quirk_backlight_present },
    // Apple Macbook 4,1
    IntelQuirk { device: 0x2a02, subsystem_vendor: 0x106b, subsystem_device: 0x00a1, hook: quirk_backlight_present },
    // Toshiba CB35 Chromebook (Celeron 2955U)
    IntelQuirk { device: 0x0a06, subsystem_vendor: 0x1179, subsystem_device: 0x0a88, hook: quirk_backlight_present },
    // HP Chromebook 14 (Celeron 2955U)
    IntelQuirk { device: 0x0a06, subsystem_vendor: 0x103c, subsystem_device: 0x21ed, hook: quirk_backlight_present },
    // Dell Chromebook 11
    IntelQuirk { device: 0x0a06, subsystem_vendor: 0x1028, subsystem_device: 0x0a35, hook: quirk_backlight_present },
    // Dell Chromebook 11 (2015 version)
    IntelQuirk { device: 0x0a16, subsystem_vendor: 0x1028, subsystem_device: 0x0a35, hook: quirk_backlight_present },
    // Toshiba Satellite P50-C-18C
    IntelQuirk { device: 0x191B, subsystem_vendor: 0x1179, subsystem_device: 0xF840, hook: quirk_increase_t12_delay },
    // GeminiLake NUC
    IntelQuirk { device: 0x3185, subsystem_vendor: 0x8086, subsystem_device: 0x2072, hook: quirk_increase_ddi_disabled_time },
    IntelQuirk { device: 0x3184, subsystem_vendor: 0x8086, subsystem_device: 0x2072, hook: quirk_increase_ddi_disabled_time },
    // ASRock ITX
    IntelQuirk { device: 0x3185, subsystem_vendor: 0x1849, subsystem_device: 0x2212, hook: quirk_increase_ddi_disabled_time },
    IntelQuirk { device: 0x3184, subsystem_vendor: 0x1849, subsystem_device: 0x2212, hook: quirk_increase_ddi_disabled_time },
    // ECS Liva Q2
    IntelQuirk { device: 0x3185, subsystem_vendor: 0x1019, subsystem_device: 0xa94d, hook: quirk_increase_ddi_disabled_time },
    IntelQuirk { device: 0x3184, subsystem_vendor: 0x1019, subsystem_device: 0xa94d, hook: quirk_increase_ddi_disabled_time },
    // HP Notebook - 14-r206nv
    IntelQuirk { device: 0x0f31, subsystem_vendor: 0x103c, subsystem_device: 0x220f, hook: quirk_invert_brightness },
    // Dell XPS 13 7390 2-in-1
    IntelQuirk { device: 0x8a52, subsystem_vendor: 0x1028, subsystem_device: 0x08b0, hook: quirk_edp_limit_rate_hbr2 },
    // Xiaomi Book Pro 14 2026
    IntelQuirk { device: 0xb081, subsystem_vendor: 0x1d72, subsystem_device: 0x2424, hook: quirk_disable_psr2 },
];

// upstream: intel_quirks.c intel_init_quirks()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_init_quirks(display: *mut IntelDisplay) {
    if display.is_null() || (*display).drm.is_null() {
        return;
    }
    let dev = (*(*display).drm).dev;
    if dev.is_null() {
        return;
    }
    let d = dev.cast::<PciDev>();
    let device = (*d).device as i32;
    let sub_vendor = (*d).subsystem_vendor as i32;
    let sub_device = (*d).subsystem_device as i32;

    for q in INTEL_QUIRKS.iter() {
        if (q.device == DEVICE_ID_ANY || q.device == device)
            && (q.subsystem_vendor == PCI_ANY_ID || q.subsystem_vendor == sub_vendor)
            && (q.subsystem_device == PCI_ANY_ID || q.subsystem_device == sub_device)
        {
            (q.hook)(display);
        }
    }
}

// upstream: intel_quirks.c intel_init_dpcd_quirks()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_init_dpcd_quirks(
    _intel_dp: *mut c_void,
    _ident: *const c_void,
) {
}

// upstream: intel_quirks.c intel_has_quirk()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_has_quirk(display: *mut IntelDisplay, quirk: IntelQuirkId) -> bool {
    if display.is_null() {
        false
    } else {
        ((*display).quirks.mask & (1u64 << (quirk as u32))) != 0
    }
}

// upstream: intel_quirks.c intel_has_dpcd_quirk()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_has_dpcd_quirk(
    _intel_dp: *mut c_void,
    _quirk: IntelQuirkId,
) -> bool {
    false
}
