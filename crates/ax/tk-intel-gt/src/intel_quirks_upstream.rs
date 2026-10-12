// SPDX-License-Identifier: MIT
// Copyright © 2018 Intel Corporation
// Source: drivers/gpu/drm/i915/display/intel_quirks.c and intel_quirks.h

#![allow(non_snake_case, dead_code, unsafe_code)]

use core::ffi::{c_char, c_void};
use crate::intel_display_core_types_upstream::IntelDisplay;
use crate::intel_display_types_upstream::IntelDp;

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

#[repr(C)]
pub struct DrmDpDpcdIdent {
    pub oui: [u8; 3],
    pub device_id: [u8; 6],
    pub hw_rev: u8,
    pub sw_major_rev: u8,
    pub sw_minor_rev: u8,
}

#[repr(C)]
pub struct DmiSystemId {
    pub callback: Option<unsafe extern "C" fn(*const DmiSystemId) -> i32>,
    pub ident: *const c_char,
    pub matches: [*const c_char; 8],
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

// upstream: intel_quirks.c intel_set_quirk()
#[inline]
unsafe fn intel_set_quirk(display: *mut IntelDisplay, quirk: IntelQuirkId) {
    if !display.is_null() {
        unsafe { (*display).quirks.mask |= 1u64 << (quirk as u32) };
    }
}

// upstream: intel_quirks.c intel_set_dpcd_quirk()
#[inline]
unsafe fn intel_set_dpcd_quirk(intel_dp: *mut IntelDp, quirk: IntelQuirkId) {
    if !intel_dp.is_null() {
        unsafe { (*intel_dp).quirks.mask |= 1u64 << (quirk as u32) };
    }
}

// upstream: intel_quirks.c quirk_ssc_force_disable()
unsafe fn quirk_ssc_force_disable(display: *mut IntelDisplay) {
    unsafe { intel_set_quirk(display, IntelQuirkId::QuirkLvdsSscDisable) };
}

// upstream: intel_quirks.c quirk_invert_brightness()
unsafe fn quirk_invert_brightness(display: *mut IntelDisplay) {
    unsafe { intel_set_quirk(display, IntelQuirkId::QuirkInvertBrightness) };
}

// upstream: intel_quirks.c quirk_backlight_present()
unsafe fn quirk_backlight_present(display: *mut IntelDisplay) {
    unsafe { intel_set_quirk(display, IntelQuirkId::QuirkBacklightPresent) };
}

// upstream: intel_quirks.c quirk_increase_t12_delay()
unsafe fn quirk_increase_t12_delay(display: *mut IntelDisplay) {
    unsafe { intel_set_quirk(display, IntelQuirkId::QuirkIncreaseT12Delay) };
}

// upstream: intel_quirks.c quirk_increase_ddi_disabled_time()
unsafe fn quirk_increase_ddi_disabled_time(display: *mut IntelDisplay) {
    unsafe { intel_set_quirk(display, IntelQuirkId::QuirkIncreaseDdiDisabledTime) };
}

// upstream: intel_quirks.c quirk_no_pps_backlight_power_hook()
unsafe fn quirk_no_pps_backlight_power_hook(display: *mut IntelDisplay) {
    unsafe { intel_set_quirk(display, IntelQuirkId::QuirkNoPpsBacklightPowerHook) };
}

// upstream: intel_quirks.c quirk_fw_sync_len()
unsafe fn quirk_fw_sync_len(intel_dp: *mut IntelDp) {
    unsafe { intel_set_dpcd_quirk(intel_dp, IntelQuirkId::QuirkFwSyncLen) };
}

// upstream: intel_quirks.c quirk_edp_limit_rate_hbr2()
unsafe fn quirk_edp_limit_rate_hbr2(display: *mut IntelDisplay) {
    unsafe { intel_set_quirk(display, IntelQuirkId::QuirkEdpLimitRateHbr2) };
}

// upstream: intel_quirks.c quirk_disable_edp_panel_replay()
unsafe fn quirk_disable_edp_panel_replay(intel_dp: *mut IntelDp) {
    unsafe { intel_set_dpcd_quirk(intel_dp, IntelQuirkId::QuirkDisableEdpPanelReplay) };
}

// upstream: intel_quirks.c quirk_disable_psr2()
unsafe fn quirk_disable_psr2(display: *mut IntelDisplay) {
    unsafe { intel_set_quirk(display, IntelQuirkId::QuirkDisablePsr2) };
}

// upstream: intel_quirks.c intel_dmi_reverse_brightness()
unsafe extern "C" fn intel_dmi_reverse_brightness(_id: *const DmiSystemId) -> i32 {
    1
}

// upstream: intel_quirks.c intel_dmi_no_pps_backlight()
unsafe extern "C" fn intel_dmi_no_pps_backlight(_id: *const DmiSystemId) -> i32 {
    1
}

struct IntelQuirk {
    device: i32,
    subsystem_vendor: i32,
    subsystem_device: i32,
    hook: unsafe fn(*mut IntelDisplay),
}

struct IntelDmiQuirk {
    hook: unsafe fn(*mut IntelDisplay),
    dmi_id_list: *const DmiSystemId,
}

struct IntelDpcdQuirk {
    device: i32,
    subsystem_vendor: i32,
    subsystem_device: i32,
    sink_oui: [u8; 3],
    sink_device_id: [u8; 6],
    hook: unsafe fn(*mut IntelDp),
}

unsafe impl Sync for IntelQuirk {}
unsafe impl Sync for IntelDmiQuirk {}
unsafe impl Sync for IntelDpcdQuirk {}
unsafe impl Sync for DmiSystemId {}

static DMI_REVERSE_BRIGHTNESS_LIST: [DmiSystemId; 3] = [
    DmiSystemId {
        callback: Some(intel_dmi_reverse_brightness),
        ident: c"NCR Corporation".as_ptr(),
        matches: [core::ptr::null(); 8],
    },
    DmiSystemId {
        callback: Some(intel_dmi_reverse_brightness),
        ident: c"Thundersoft TST178 tablet".as_ptr(),
        matches: [core::ptr::null(); 8],
    },
    DmiSystemId {
        callback: None,
        ident: core::ptr::null(),
        matches: [core::ptr::null(); 8],
    },
];

static DMI_NO_PPS_BACKLIGHT_LIST: [DmiSystemId; 3] = [
    DmiSystemId {
        callback: Some(intel_dmi_no_pps_backlight),
        ident: c"Google Lillipup sku524294".as_ptr(),
        matches: [core::ptr::null(); 8],
    },
    DmiSystemId {
        callback: Some(intel_dmi_no_pps_backlight),
        ident: c"Google Lillipup sku524295".as_ptr(),
        matches: [core::ptr::null(); 8],
    },
    DmiSystemId {
        callback: None,
        ident: core::ptr::null(),
        matches: [core::ptr::null(); 8],
    },
];

static INTEL_DMI_QUIRKS: &[IntelDmiQuirk] = &[
    IntelDmiQuirk {
        hook: quirk_invert_brightness,
        dmi_id_list: DMI_REVERSE_BRIGHTNESS_LIST.as_ptr(),
    },
    IntelDmiQuirk {
        hook: quirk_no_pps_backlight_power_hook,
        dmi_id_list: DMI_NO_PPS_BACKLIGHT_LIST.as_ptr(),
    },
];

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

static INTEL_DPCD_QUIRKS: &[IntelDpcdQuirk] = &[
    // Dell Precision 5490
    IntelDpcdQuirk {
        device: 0x7d55,
        subsystem_vendor: 0x1028,
        subsystem_device: 0x0cc7,
        sink_oui: [0x38, 0xec, 0x11],
        sink_device_id: [0; 6],
        hook: quirk_fw_sync_len,
    },
    // Dell XPS 14 DA14260
    IntelDpcdQuirk {
        device: DEVICE_ID_ANY,
        subsystem_vendor: 0x1028,
        subsystem_device: 0x0db9,
        sink_oui: [0x00, 0x22, 0xb9],
        sink_device_id: [0; 6],
        hook: quirk_disable_edp_panel_replay,
    },
    // Dell XPS 16 DA16260
    IntelDpcdQuirk {
        device: DEVICE_ID_ANY,
        subsystem_vendor: 0x1028,
        subsystem_device: 0x0dba,
        sink_oui: [0x00, 0x22, 0xb9],
        sink_device_id: [0; 6],
        hook: quirk_disable_edp_panel_replay,
    },
];

// upstream: intel_quirks.c intel_init_quirks()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_init_quirks(display: *mut IntelDisplay) {
    if display.is_null() || unsafe { (*display).drm.is_null() } {
        return;
    }
    let dev = unsafe { (*(*display).drm).dev };
    if dev.is_null() {
        return;
    }
    let d = dev.cast::<PciDev>();
    let device = unsafe { (*d).device as i32 };
    let sub_vendor = unsafe { (*d).subsystem_vendor as i32 };
    let sub_device = unsafe { (*d).subsystem_device as i32 };

    for q in INTEL_QUIRKS.iter() {
        if (q.device == DEVICE_ID_ANY || q.device == device)
            && (q.subsystem_vendor == PCI_ANY_ID || q.subsystem_vendor == sub_vendor)
            && (q.subsystem_device == PCI_ANY_ID || q.subsystem_device == sub_device)
        {
            unsafe { (q.hook)(display) };
        }
    }

    for q in INTEL_DMI_QUIRKS.iter() {
        if unsafe { crate::display_pending_stubs::kapi_display::dmi_check_system(q.dmi_id_list.cast()) } != 0 {
            unsafe { (q.hook)(display) };
        }
    }
}

unsafe fn dp_to_dig_port(intel_dp: *mut IntelDp) -> *mut crate::intel_display_types_upstream::IntelDigitalPort {
    (intel_dp.cast::<u8>().sub(504)).cast()
}

unsafe fn to_intel_display_from_dp(intel_dp: *mut IntelDp) -> *mut IntelDisplay {
    if intel_dp.is_null() {
        return core::ptr::null_mut();
    }
    let dig = unsafe { dp_to_dig_port(intel_dp) };
    let dev = unsafe { (*dig).base.base.dev };
    if dev.is_null() {
        core::ptr::null_mut()
    } else {
        unsafe { (*dev).dev_private.cast() }
    }
}

// upstream: intel_quirks.c intel_init_dpcd_quirks()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_init_dpcd_quirks(
    intel_dp: *mut IntelDp,
    ident: *const DrmDpDpcdIdent,
) {
    if intel_dp.is_null() || ident.is_null() {
        return;
    }
    let display = unsafe { to_intel_display_from_dp(intel_dp) };
    let d = if !display.is_null() && !unsafe { (*display).drm.is_null() } && !unsafe { (*(*display).drm).dev.is_null() } {
        unsafe { (*(*display).drm).dev.cast::<PciDev>() }
    } else {
        core::ptr::null()
    };

    let (device, sub_vendor, sub_device) = if !d.is_null() {
        unsafe { ((*d).device as i32, (*d).subsystem_vendor as i32, (*d).subsystem_device as i32) }
    } else {
        (0, 0, 0)
    };

    let ident_oui = unsafe { (*ident).oui };
    let ident_dev = unsafe { (*ident).device_id };

    for q in INTEL_DPCD_QUIRKS.iter() {
        if (q.device == DEVICE_ID_ANY || q.device == device)
            && (q.subsystem_vendor == PCI_ANY_ID || q.subsystem_vendor == sub_vendor)
            && (q.subsystem_device == PCI_ANY_ID || q.subsystem_device == sub_device)
            && q.sink_oui == ident_oui
            && (q.sink_device_id == ident_dev || q.sink_device_id == [0; 6])
        {
            unsafe { (q.hook)(intel_dp) };
        }
    }
}

// upstream: intel_quirks.c intel_has_quirk()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_has_quirk(display: *mut IntelDisplay, quirk: IntelQuirkId) -> bool {
    if display.is_null() {
        false
    } else {
        unsafe { ((*display).quirks.mask & (1u64 << (quirk as u32))) != 0 }
    }
}

// upstream: intel_quirks.c intel_has_dpcd_quirk()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_has_dpcd_quirk(
    intel_dp: *mut IntelDp,
    quirk: IntelQuirkId,
) -> bool {
    if intel_dp.is_null() {
        false
    } else {
        unsafe { ((*intel_dp).quirks.mask & (1u64 << (quirk as u32))) != 0 }
    }
}
