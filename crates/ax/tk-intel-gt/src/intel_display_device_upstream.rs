// SPDX-License-Identifier: MIT
// Copyright © 2023 Intel Corporation
// Source: drivers/gpu/drm/i915/display/intel_display_device.c and intel_display_device.h

#![allow(non_snake_case, dead_code, unsafe_code)]

use core::ffi::{c_char, c_void};
use crate::intel_de_upstream::intel_de_read;
use crate::intel_display_core_types_upstream::IntelDisplay;
use crate::intel_display_device_types_upstream::{
    IntelDisplayDeviceInfo, IntelDisplayDeviceInfoColor, IntelDisplayDeviceInfoDbuf,
    IntelDisplayIpVer, IntelDisplayPlatforms, IntelDisplayRuntimeInfo,
};
use crate::intel_display_params_upstream::{intel_display_params_copy, intel_display_params_free};
use crate::intel_display_regs_upstream::{
    ICL_DFSM_DMC_DISABLE, SKL_DFSM, SKL_DFSM_DISPLAY_HDCP_DISABLE, SKL_DFSM_PIPE_A_DISABLE,
    SKL_DFSM_PIPE_B_DISABLE, SKL_DFSM_PIPE_C_DISABLE, TGL_DFSM_PIPE_D_DISABLE,
    GLK_DFSM_DISPLAY_DSC_DISABLE,
};
use crate::intel_parent_upstream::IntelDisplayParentInterface;
use crate::intel_pciids_upstream::*;
use crate::intel_quirks_upstream::PciDev;
use tk_drm_upstream::drm_print_types::DrmPrinter;

// Step constants from include/drm/intel/step.h
pub const STEP_NONE: i32 = 0;
pub const STEP_A0: i32 = 1;
pub const STEP_A1: i32 = 2;
pub const STEP_A2: i32 = 3;
pub const STEP_B0: i32 = 5;
pub const STEP_B1: i32 = 6;
pub const STEP_C0: i32 = 9;
pub const STEP_D0: i32 = 13;
pub const STEP_D1: i32 = 14;
pub const STEP_E0: i32 = 17;
pub const STEP_F0: i32 = 21;
pub const STEP_G0: i32 = 25;
pub const STEP_H0: i32 = 29;
pub const STEP_I1: i32 = 34;
pub const STEP_J0: i32 = 37;
pub const STEP_FUTURE: i32 = 41;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SteppingDesc {
    pub map: *const i32,
    pub size: usize,
}

#[repr(C)]
pub struct SubplatformDesc {
    pub platforms: IntelDisplayPlatforms,
    pub name: *const c_char,
    pub pciidlist: *const u16,
    pub step_info: SteppingDesc,
}

#[repr(C)]
pub struct PlatformDesc {
    pub platforms: IntelDisplayPlatforms,
    pub name: *const c_char,
    pub subplatforms: *const SubplatformDesc,
    pub info: *const IntelDisplayDeviceInfo,
    pub step_info: SteppingDesc,
}

pub struct DisplayIdEntry {
    pub devid: u16,
    pub desc: *const PlatformDesc,
}

pub static NO_DISPLAY: IntelDisplayDeviceInfo = IntelDisplayDeviceInfo {
    __runtime_defaults: IntelDisplayRuntimeInfo {
        ip: IntelDisplayIpVer { ver: 0, rel: 0, step: 0 },
        _pad_6: [0; 2],
        step: 0,
        step_name: [0; 3],
        _pad_15: [0; 1],
        rawclk_freq: 0,
        pipe_mask: 0,
        cpu_transcoder_mask: 0,
        port_mask: 0,
        num_sprites: [0; 4],
        num_scalers: [0; 4],
        fbc_mask: 0,
        has_hdcp: false,
        has_dmc: false,
        has_dsc: false,
        edp_typec_support: false,
        has_dbuf_overlap_detection: false,
        _pad_38: [0; 2],
    },
    abox_mask: 0,
    _pad_41: [0; 1],
    dbuf: IntelDisplayDeviceInfoDbuf {
        size: 0,
        slice_mask: 0,
        _pad_3: [0; 1],
    },
    _bitfields_46: [0; 2],
    mmio_offset: 0,
    pipe_offsets: [0; 7],
    trans_offsets: [0; 7],
    cursor_offsets: [0; 4],
    color: IntelDisplayDeviceInfoColor {
        degamma_lut_size: 0,
        gamma_lut_size: 0,
        degamma_lut_tests: 0,
        gamma_lut_tests: 0,
    },
};

pub static XE_LPD_DISPLAY: IntelDisplayDeviceInfo = IntelDisplayDeviceInfo {
    __runtime_defaults: IntelDisplayRuntimeInfo {
        ip: IntelDisplayIpVer { ver: 13, rel: 0, step: 0 },
        _pad_6: [0; 2],
        step: 0,
        step_name: [0; 3],
        _pad_15: [0; 1],
        rawclk_freq: 0,
        pipe_mask: 0x0f, // BIT(PIPE_A..D)
        cpu_transcoder_mask: 0x6f, // BIT(TRANSCODER_A..D, DSI_0, DSI_1)
        port_mask: 0x7b, // BIT(PORT_A, B, TC1..TC4)
        num_sprites: [4, 4, 4, 4],
        num_scalers: [2, 2, 2, 2],
        fbc_mask: 1, // BIT(INTEL_FBC_A)
        has_hdcp: true,
        has_dmc: true,
        has_dsc: true,
        edp_typec_support: false,
        has_dbuf_overlap_detection: false,
        _pad_38: [0; 2],
    },
    abox_mask: 3, // GENMASK(1, 0)
    _pad_41: [0; 1],
    dbuf: IntelDisplayDeviceInfoDbuf {
        size: 4096,
        slice_mask: 0x0f, // BIT(DBUF_S1..S4)
        _pad_3: [0; 1],
    },
    _bitfields_46: [
        // has_cdclk_crawl:1, has_ddi:1, has_dp_mst:1, has_dsb:1, has_fpga_dbg:1
        0x7a,
        // has_hotplug:1, has_ipc:1, has_psr:1
        0x15,
    ],
    mmio_offset: 0,
    pipe_offsets: [0x70000, 0x71000, 0x72000, 0x73000, 0, 0, 0],
    trans_offsets: [0x60000, 0x61000, 0x62000, 0x63000, 0x6b000, 0x6b800, 0],
    cursor_offsets: [0x70080, 0x71080, 0x72080, 0x73080],
    color: IntelDisplayDeviceInfoColor {
        degamma_lut_size: 129,
        gamma_lut_size: 1024,
        degamma_lut_tests: 3,
        gamma_lut_tests: 0,
    },
};

static ADL_P_STEPPINGS: [i32; 13] = [
    STEP_A0, 0, 0, 0,
    STEP_B0, 0, 0, 0,
    STEP_C0, 0, 0, 0,
    STEP_D0,
];

static ADL_P_ADL_N_STEPPINGS: [i32; 1] = [STEP_D0];
static ADL_P_RPL_PU_STEPPINGS: [i32; 5] = [0, 0, 0, 0, STEP_E0];

// Zero-terminated lists for subplatforms
static ADLP_ADLN_IDS_LIST: &[u16] = &[0x46D0, 0x46D1, 0x46D2, 0x46D3, 0x46D4, 0];
static ADLP_RPLP_IDS_LIST: &[u16] = &[0xA720, 0xA7A0, 0xA7A8, 0xA7AA, 0xA7AB, 0];
static ADLP_RPLU_IDS_LIST: &[u16] = &[0xA721, 0xA7A1, 0xA7A9, 0xA7AC, 0xA7AD, 0];

static ADL_P_SUBPLATFORMS: [SubplatformDesc; 4] = [
    SubplatformDesc {
        platforms: IntelDisplayPlatforms { _anon_0_0: crate::intel_display_device_types_upstream::IntelDisplayPlatformsAnon0x0 { bitmap: [1u64 << 53, 0] } },
        name: c"alderlake_n".as_ptr(),
        pciidlist: ADLP_ADLN_IDS_LIST.as_ptr(),
        step_info: SteppingDesc { map: ADL_P_ADL_N_STEPPINGS.as_ptr(), size: 1 },
    },
    SubplatformDesc {
        platforms: IntelDisplayPlatforms { _anon_0_0: crate::intel_display_device_types_upstream::IntelDisplayPlatformsAnon0x0 { bitmap: [1u64 << 54, 0] } },
        name: c"raptorlake_p".as_ptr(),
        pciidlist: ADLP_RPLP_IDS_LIST.as_ptr(),
        step_info: SteppingDesc { map: ADL_P_RPL_PU_STEPPINGS.as_ptr(), size: 5 },
    },
    SubplatformDesc {
        platforms: IntelDisplayPlatforms { _anon_0_0: crate::intel_display_device_types_upstream::IntelDisplayPlatformsAnon0x0 { bitmap: [1u64 << 55, 0] } },
        name: c"raptorlake_u".as_ptr(),
        pciidlist: ADLP_RPLU_IDS_LIST.as_ptr(),
        step_info: SteppingDesc { map: ADL_P_RPL_PU_STEPPINGS.as_ptr(), size: 5 },
    },
    SubplatformDesc {
        platforms: IntelDisplayPlatforms { _anon_0_0: crate::intel_display_device_types_upstream::IntelDisplayPlatformsAnon0x0 { bitmap: [0, 0] } },
        name: core::ptr::null(),
        pciidlist: core::ptr::null(),
        step_info: SteppingDesc { map: core::ptr::null(), size: 0 },
    },
];

pub static ADL_P_DESC: PlatformDesc = PlatformDesc {
    platforms: IntelDisplayPlatforms { _anon_0_0: crate::intel_display_device_types_upstream::IntelDisplayPlatformsAnon0x0 { bitmap: [(1u64 << 52) | (1u64 << 1), 0] } },
    name: c"alderlake_p".as_ptr(),
    subplatforms: ADL_P_SUBPLATFORMS.as_ptr(),
    info: core::ptr::addr_of!(XE_LPD_DISPLAY),
    step_info: SteppingDesc { map: ADL_P_STEPPINGS.as_ptr(), size: 13 },
};

pub static ADL_S_DESC: PlatformDesc = PlatformDesc {
    platforms: IntelDisplayPlatforms { _anon_0_0: crate::intel_display_device_types_upstream::IntelDisplayPlatformsAnon0x0 { bitmap: [1u64 << 50, 0] } },
    name: c"alderlake_s".as_ptr(),
    subplatforms: core::ptr::null(),
    info: core::ptr::null(),
    step_info: SteppingDesc { map: core::ptr::null(), size: 0 },
};

pub static TGL_DESC: PlatformDesc = PlatformDesc {
    platforms: IntelDisplayPlatforms { _anon_0_0: crate::intel_display_device_types_upstream::IntelDisplayPlatformsAnon0x0 { bitmap: [1u64 << 46, 0] } },
    name: c"tigerlake".as_ptr(),
    subplatforms: core::ptr::null(),
    info: core::ptr::null(),
    step_info: SteppingDesc { map: core::ptr::null(), size: 0 },
};

pub static RKL_DESC: PlatformDesc = PlatformDesc {
    platforms: IntelDisplayPlatforms { _anon_0_0: crate::intel_display_device_types_upstream::IntelDisplayPlatformsAnon0x0 { bitmap: [1u64 << 48, 0] } },
    name: c"rocketlake".as_ptr(),
    subplatforms: core::ptr::null(),
    info: core::ptr::null(),
    step_info: SteppingDesc { map: core::ptr::null(), size: 0 },
};

pub static DG1_DESC: PlatformDesc = PlatformDesc {
    platforms: IntelDisplayPlatforms { _anon_0_0: crate::intel_display_device_types_upstream::IntelDisplayPlatformsAnon0x0 { bitmap: [(1u64 << 49) | (1u64 << 2), 0] } },
    name: c"dg1".as_ptr(),
    subplatforms: core::ptr::null(),
    info: core::ptr::null(),
    step_info: SteppingDesc { map: core::ptr::null(), size: 0 },
};

pub static DG2_DESC: PlatformDesc = PlatformDesc {
    platforms: IntelDisplayPlatforms { _anon_0_0: crate::intel_display_device_types_upstream::IntelDisplayPlatformsAnon0x0 { bitmap: [(1u64 << 56) | (1u64 << 2), 0] } },
    name: c"dg2".as_ptr(),
    subplatforms: core::ptr::null(),
    info: core::ptr::null(),
    step_info: SteppingDesc { map: core::ptr::null(), size: 0 },
};

pub static MTL_DESC: PlatformDesc = PlatformDesc {
    platforms: IntelDisplayPlatforms { _anon_0_0: crate::intel_display_device_types_upstream::IntelDisplayPlatformsAnon0x0 { bitmap: [1u64 << 60, 0] } },
    name: c"meteorlake".as_ptr(),
    subplatforms: core::ptr::null(),
    info: core::ptr::null(),
    step_info: SteppingDesc { map: core::ptr::null(), size: 0 },
};

// Full display IDs table for platform lookup
static INTEL_DISPLAY_IDS: &[DisplayIdEntry] = &[
    // ADL-P
    DisplayIdEntry { devid: 0x46A0, desc: core::ptr::addr_of!(ADL_P_DESC) },
    DisplayIdEntry { devid: 0x46A1, desc: core::ptr::addr_of!(ADL_P_DESC) },
    DisplayIdEntry { devid: 0x46A2, desc: core::ptr::addr_of!(ADL_P_DESC) },
    DisplayIdEntry { devid: 0x46A3, desc: core::ptr::addr_of!(ADL_P_DESC) },
    DisplayIdEntry { devid: 0x46A6, desc: core::ptr::addr_of!(ADL_P_DESC) },
    DisplayIdEntry { devid: 0x46A8, desc: core::ptr::addr_of!(ADL_P_DESC) },
    DisplayIdEntry { devid: 0x46AA, desc: core::ptr::addr_of!(ADL_P_DESC) },
    DisplayIdEntry { devid: 0x462A, desc: core::ptr::addr_of!(ADL_P_DESC) },
    DisplayIdEntry { devid: 0x4626, desc: core::ptr::addr_of!(ADL_P_DESC) },
    DisplayIdEntry { devid: 0x4628, desc: core::ptr::addr_of!(ADL_P_DESC) },
    DisplayIdEntry { devid: 0x46B0, desc: core::ptr::addr_of!(ADL_P_DESC) },
    DisplayIdEntry { devid: 0x46B1, desc: core::ptr::addr_of!(ADL_P_DESC) },
    DisplayIdEntry { devid: 0x46B2, desc: core::ptr::addr_of!(ADL_P_DESC) },
    DisplayIdEntry { devid: 0x46B3, desc: core::ptr::addr_of!(ADL_P_DESC) },
    DisplayIdEntry { devid: 0x46C0, desc: core::ptr::addr_of!(ADL_P_DESC) },
    DisplayIdEntry { devid: 0x46C1, desc: core::ptr::addr_of!(ADL_P_DESC) },
    DisplayIdEntry { devid: 0x46C2, desc: core::ptr::addr_of!(ADL_P_DESC) },
    DisplayIdEntry { devid: 0x46C3, desc: core::ptr::addr_of!(ADL_P_DESC) },
    // ADL-N
    DisplayIdEntry { devid: 0x46D0, desc: core::ptr::addr_of!(ADL_P_DESC) },
    DisplayIdEntry { devid: 0x46D1, desc: core::ptr::addr_of!(ADL_P_DESC) },
    DisplayIdEntry { devid: 0x46D2, desc: core::ptr::addr_of!(ADL_P_DESC) },
    DisplayIdEntry { devid: 0x46D3, desc: core::ptr::addr_of!(ADL_P_DESC) },
    DisplayIdEntry { devid: 0x46D4, desc: core::ptr::addr_of!(ADL_P_DESC) },
    // RPL-U
    DisplayIdEntry { devid: 0xA721, desc: core::ptr::addr_of!(ADL_P_DESC) },
    DisplayIdEntry { devid: 0xA7A1, desc: core::ptr::addr_of!(ADL_P_DESC) },
    DisplayIdEntry { devid: 0xA7A9, desc: core::ptr::addr_of!(ADL_P_DESC) },
    DisplayIdEntry { devid: 0xA7AC, desc: core::ptr::addr_of!(ADL_P_DESC) },
    DisplayIdEntry { devid: 0xA7AD, desc: core::ptr::addr_of!(ADL_P_DESC) },
    // RPL-P
    DisplayIdEntry { devid: 0xA720, desc: core::ptr::addr_of!(ADL_P_DESC) },
    DisplayIdEntry { devid: 0xA7A0, desc: core::ptr::addr_of!(ADL_P_DESC) },
    DisplayIdEntry { devid: 0xA7A8, desc: core::ptr::addr_of!(ADL_P_DESC) },
    DisplayIdEntry { devid: 0xA7AA, desc: core::ptr::addr_of!(ADL_P_DESC) },
    DisplayIdEntry { devid: 0xA7AB, desc: core::ptr::addr_of!(ADL_P_DESC) },
    // ADL-S / RPL-S
    DisplayIdEntry { devid: 0x4680, desc: core::ptr::addr_of!(ADL_S_DESC) },
    DisplayIdEntry { devid: 0x4682, desc: core::ptr::addr_of!(ADL_S_DESC) },
    DisplayIdEntry { devid: 0x4688, desc: core::ptr::addr_of!(ADL_S_DESC) },
    DisplayIdEntry { devid: 0x468A, desc: core::ptr::addr_of!(ADL_S_DESC) },
    DisplayIdEntry { devid: 0x468B, desc: core::ptr::addr_of!(ADL_S_DESC) },
    DisplayIdEntry { devid: 0x4690, desc: core::ptr::addr_of!(ADL_S_DESC) },
    DisplayIdEntry { devid: 0x4692, desc: core::ptr::addr_of!(ADL_S_DESC) },
    DisplayIdEntry { devid: 0x4693, desc: core::ptr::addr_of!(ADL_S_DESC) },
    DisplayIdEntry { devid: 0xA780, desc: core::ptr::addr_of!(ADL_S_DESC) },
    DisplayIdEntry { devid: 0xA781, desc: core::ptr::addr_of!(ADL_S_DESC) },
    DisplayIdEntry { devid: 0xA782, desc: core::ptr::addr_of!(ADL_S_DESC) },
    DisplayIdEntry { devid: 0xA783, desc: core::ptr::addr_of!(ADL_S_DESC) },
    DisplayIdEntry { devid: 0xA788, desc: core::ptr::addr_of!(ADL_S_DESC) },
    DisplayIdEntry { devid: 0xA789, desc: core::ptr::addr_of!(ADL_S_DESC) },
    DisplayIdEntry { devid: 0xA78A, desc: core::ptr::addr_of!(ADL_S_DESC) },
    DisplayIdEntry { devid: 0xA78B, desc: core::ptr::addr_of!(ADL_S_DESC) },
    // TGL
    DisplayIdEntry { devid: 0x9A60, desc: core::ptr::addr_of!(TGL_DESC) },
    DisplayIdEntry { devid: 0x9A68, desc: core::ptr::addr_of!(TGL_DESC) },
    DisplayIdEntry { devid: 0x9A70, desc: core::ptr::addr_of!(TGL_DESC) },
    DisplayIdEntry { devid: 0x9A40, desc: core::ptr::addr_of!(TGL_DESC) },
    DisplayIdEntry { devid: 0x9A49, desc: core::ptr::addr_of!(TGL_DESC) },
    DisplayIdEntry { devid: 0x9A59, desc: core::ptr::addr_of!(TGL_DESC) },
    DisplayIdEntry { devid: 0x9A78, desc: core::ptr::addr_of!(TGL_DESC) },
    DisplayIdEntry { devid: 0x9AC0, desc: core::ptr::addr_of!(TGL_DESC) },
    DisplayIdEntry { devid: 0x9AC9, desc: core::ptr::addr_of!(TGL_DESC) },
    DisplayIdEntry { devid: 0x9AD9, desc: core::ptr::addr_of!(TGL_DESC) },
    DisplayIdEntry { devid: 0x9AF8, desc: core::ptr::addr_of!(TGL_DESC) },
    // RKL
    DisplayIdEntry { devid: 0x4C80, desc: core::ptr::addr_of!(RKL_DESC) },
    DisplayIdEntry { devid: 0x4C8A, desc: core::ptr::addr_of!(RKL_DESC) },
    DisplayIdEntry { devid: 0x4C8B, desc: core::ptr::addr_of!(RKL_DESC) },
    DisplayIdEntry { devid: 0x4C8C, desc: core::ptr::addr_of!(RKL_DESC) },
    DisplayIdEntry { devid: 0x4C90, desc: core::ptr::addr_of!(RKL_DESC) },
    DisplayIdEntry { devid: 0x4C9A, desc: core::ptr::addr_of!(RKL_DESC) },
    // DG1
    DisplayIdEntry { devid: 0x4905, desc: core::ptr::addr_of!(DG1_DESC) },
    DisplayIdEntry { devid: 0x4906, desc: core::ptr::addr_of!(DG1_DESC) },
    DisplayIdEntry { devid: 0x4907, desc: core::ptr::addr_of!(DG1_DESC) },
    DisplayIdEntry { devid: 0x4908, desc: core::ptr::addr_of!(DG1_DESC) },
    DisplayIdEntry { devid: 0x4909, desc: core::ptr::addr_of!(DG1_DESC) },
    // DG2
    DisplayIdEntry { devid: 0x56A0, desc: core::ptr::addr_of!(DG2_DESC) },
    DisplayIdEntry { devid: 0x56A1, desc: core::ptr::addr_of!(DG2_DESC) },
    DisplayIdEntry { devid: 0x56A2, desc: core::ptr::addr_of!(DG2_DESC) },
    DisplayIdEntry { devid: 0x56BE, desc: core::ptr::addr_of!(DG2_DESC) },
    DisplayIdEntry { devid: 0x56BF, desc: core::ptr::addr_of!(DG2_DESC) },
    DisplayIdEntry { devid: 0x5690, desc: core::ptr::addr_of!(DG2_DESC) },
    DisplayIdEntry { devid: 0x5691, desc: core::ptr::addr_of!(DG2_DESC) },
    DisplayIdEntry { devid: 0x5692, desc: core::ptr::addr_of!(DG2_DESC) },
    DisplayIdEntry { devid: 0x56A5, desc: core::ptr::addr_of!(DG2_DESC) },
    DisplayIdEntry { devid: 0x56A6, desc: core::ptr::addr_of!(DG2_DESC) },
    DisplayIdEntry { devid: 0x56B0, desc: core::ptr::addr_of!(DG2_DESC) },
    DisplayIdEntry { devid: 0x56B1, desc: core::ptr::addr_of!(DG2_DESC) },
    DisplayIdEntry { devid: 0x56BA, desc: core::ptr::addr_of!(DG2_DESC) },
    DisplayIdEntry { devid: 0x56BB, desc: core::ptr::addr_of!(DG2_DESC) },
    DisplayIdEntry { devid: 0x56BC, desc: core::ptr::addr_of!(DG2_DESC) },
    DisplayIdEntry { devid: 0x56BD, desc: core::ptr::addr_of!(DG2_DESC) },
    DisplayIdEntry { devid: 0x5693, desc: core::ptr::addr_of!(DG2_DESC) },
    DisplayIdEntry { devid: 0x5694, desc: core::ptr::addr_of!(DG2_DESC) },
    DisplayIdEntry { devid: 0x5695, desc: core::ptr::addr_of!(DG2_DESC) },
    DisplayIdEntry { devid: 0x56A3, desc: core::ptr::addr_of!(DG2_DESC) },
    DisplayIdEntry { devid: 0x56A4, desc: core::ptr::addr_of!(DG2_DESC) },
    DisplayIdEntry { devid: 0x56B2, desc: core::ptr::addr_of!(DG2_DESC) },
    DisplayIdEntry { devid: 0x56B3, desc: core::ptr::addr_of!(DG2_DESC) },
    DisplayIdEntry { devid: 0x5696, desc: core::ptr::addr_of!(DG2_DESC) },
    DisplayIdEntry { devid: 0x5697, desc: core::ptr::addr_of!(DG2_DESC) },
    // MTL / ARL
    DisplayIdEntry { devid: 0x7D51, desc: core::ptr::addr_of!(MTL_DESC) },
    DisplayIdEntry { devid: 0x7DD1, desc: core::ptr::addr_of!(MTL_DESC) },
    DisplayIdEntry { devid: 0x7D41, desc: core::ptr::addr_of!(MTL_DESC) },
    DisplayIdEntry { devid: 0x7D67, desc: core::ptr::addr_of!(MTL_DESC) },
    DisplayIdEntry { devid: 0xB640, desc: core::ptr::addr_of!(MTL_DESC) },
    DisplayIdEntry { devid: 0x7D40, desc: core::ptr::addr_of!(MTL_DESC) },
    DisplayIdEntry { devid: 0x7D45, desc: core::ptr::addr_of!(MTL_DESC) },
    DisplayIdEntry { devid: 0x7D55, desc: core::ptr::addr_of!(MTL_DESC) },
    DisplayIdEntry { devid: 0x7D60, desc: core::ptr::addr_of!(MTL_DESC) },
    DisplayIdEntry { devid: 0x7DD5, desc: core::ptr::addr_of!(MTL_DESC) },
];

unsafe impl Sync for SteppingDesc {}
unsafe impl Sync for SubplatformDesc {}
unsafe impl Sync for PlatformDesc {}
unsafe impl Sync for DisplayIdEntry {}

// upstream: intel_display_device.c has_no_display()
unsafe fn has_no_display(pdev: *mut PciDev) -> bool {
    if pdev.is_null() {
        return false;
    }
    // IVB Q server transcode without display: vendor 0x8086, dev 0x016a, subvendor 0x152d, subdevice 0x8990
    unsafe {
        (*pdev).vendor == 0x8086
            && (*pdev).device == 0x016a
            && (*pdev).subsystem_vendor == 0x152d
            && (*pdev).subsystem_device == 0x8990
    }
}

// upstream: intel_display_device.c probe_gmdid_display()
// skipped: not display 13 (GMD ID is used on Xe_LPD+ ver >= 14)
unsafe fn probe_gmdid_display(
    _display: *mut IntelDisplay,
    _ip_ver: *mut IntelDisplayIpVer,
) -> *const IntelDisplayDeviceInfo {
    core::ptr::null()
}

// upstream: intel_display_device.c find_platform_desc()
pub unsafe fn find_platform_desc(pdev: *mut PciDev) -> *const PlatformDesc {
    if pdev.is_null() {
        return core::ptr::null();
    }
    let devid = unsafe { (*pdev).device };
    for entry in INTEL_DISPLAY_IDS {
        if entry.devid == devid {
            return entry.desc;
        }
    }
    core::ptr::null()
}

// upstream: intel_display_device.c find_subplatform_desc()
pub unsafe fn find_subplatform_desc(
    pdev: *mut PciDev,
    desc: *const PlatformDesc,
) -> *const SubplatformDesc {
    if desc.is_null() || unsafe { (*desc).subplatforms.is_null() } || pdev.is_null() {
        return core::ptr::null();
    }
    let devid = unsafe { (*pdev).device };
    let mut sp = unsafe { (*desc).subplatforms };
    while !unsafe { (*sp).pciidlist.is_null() } {
        let mut id = unsafe { (*sp).pciidlist };
        while unsafe { *id != 0 } {
            if unsafe { *id } == devid {
                return sp;
            }
            id = unsafe { id.add(1) };
        }
        sp = unsafe { sp.add(1) };
    }
    core::ptr::null()
}

// upstream: intel_display_device.c get_pre_gmdid_step()
pub unsafe fn get_pre_gmdid_step(
    _display: *mut IntelDisplay,
    main: *const SteppingDesc,
    sub: *const SteppingDesc,
) -> i32 {
    let mut map = unsafe { (*main).map };
    let mut size = unsafe { (*main).size };
    let revision = 0usize;

    if !sub.is_null() && !unsafe { (*sub).map.is_null() } && unsafe { (*sub).size } > 0 {
        map = unsafe { (*sub).map };
        size = unsafe { (*sub).size };
    }

    if map.is_null() || size == 0 {
        return STEP_NONE;
    }

    if revision < size && unsafe { *map.add(revision) } != STEP_NONE {
        return unsafe { *map.add(revision) };
    }

    let mut rev = revision;
    while rev < size && unsafe { *map.add(rev) } == STEP_NONE {
        rev += 1;
    }
    if rev < size {
        unsafe { *map.add(rev) }
    } else {
        STEP_FUTURE
    }
}

// upstream: intel_display_device.c display_platforms_num_bits()
pub fn display_platforms_num_bits() -> u32 {
    128
}

// upstream: intel_display_device.c display_platforms_weight()
pub unsafe fn display_platforms_weight(p: *const IntelDisplayPlatforms) -> u32 {
    if p.is_null() {
        0
    } else {
        unsafe {
            let b0 = (*p)._anon_0_0.bitmap[0].count_ones();
            let b1 = (*p)._anon_0_0.bitmap[1].count_ones();
            b0 + b1
        }
    }
}

// upstream: intel_display_device.c display_platforms_or()
pub unsafe fn display_platforms_or(
    dst: *mut IntelDisplayPlatforms,
    src: *const IntelDisplayPlatforms,
) {
    if !dst.is_null() && !src.is_null() {
        unsafe {
            (*dst)._anon_0_0.bitmap[0] |= (*src)._anon_0_0.bitmap[0];
            (*dst)._anon_0_0.bitmap[1] |= (*src)._anon_0_0.bitmap[1];
        }
    }
}

// upstream: intel_display_device.c initialize_step()
pub unsafe fn initialize_step(display: *mut IntelDisplay, step: i32) {
    if display.is_null() {
        return;
    }
    let runtime = unsafe { core::ptr::addr_of_mut!((*display).info.__runtime_info) };
    unsafe { (*runtime).step = step };
    let name: &[u8; 3] = match step {
        STEP_A0 => b"A0\0",
        STEP_A1 => b"A1\0",
        STEP_A2 => b"A2\0",
        STEP_B0 => b"B0\0",
        STEP_B1 => b"B1\0",
        STEP_C0 => b"C0\0",
        STEP_D0 => b"D0\0",
        STEP_D1 => b"D1\0",
        STEP_E0 => b"E0\0",
        STEP_F0 => b"F0\0",
        STEP_G0 => b"G0\0",
        STEP_H0 => b"H0\0",
        _ => b"\0\0\0",
    };
    unsafe {
        (*runtime).step_name[0] = name[0] as c_char;
        (*runtime).step_name[1] = name[1] as c_char;
        (*runtime).step_name[2] = name[2] as c_char;
    }
}

// upstream: intel_display_device.c step_name()
pub unsafe fn step_name(runtime: *const IntelDisplayRuntimeInfo) -> *const c_char {
    if runtime.is_null() || unsafe { (*runtime).step_name[0] == 0 } {
        c"N/A".as_ptr()
    } else {
        unsafe { (*runtime).step_name.as_ptr() }
    }
}

// upstream: intel_display_device.c intel_display_device_probe()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_device_probe(
    pdev: *mut PciDev,
    parent: *const IntelDisplayParentInterface,
) -> *mut IntelDisplay {
    let display = unsafe {
        tk_linux_kapi::memory::kzalloc(
            core::mem::size_of::<IntelDisplay>(),
            tk_linux_kapi::config::GFP_KERNEL,
        )
        .cast::<IntelDisplay>()
    };

    if display.is_null() {
        return core::ptr::null_mut();
    }

    unsafe {
        (*display).parent = parent.cast_mut().cast();
        intel_display_params_copy(core::ptr::addr_of_mut!((*display).params));
    }

    if pdev.is_null() || unsafe { has_no_display(pdev) } {
        unsafe { (*display).info.__device_info = core::ptr::addr_of!(NO_DISPLAY) as *mut _ };
        return display;
    }

    let desc = unsafe { find_platform_desc(pdev) };
    if desc.is_null() {
        unsafe { (*display).info.__device_info = core::ptr::addr_of!(NO_DISPLAY) as *mut _ };
        return display;
    }

    let mut info = unsafe { (*desc).info };
    let mut ip_ver = IntelDisplayIpVer { ver: 0, rel: 0, step: 0 };
    if info.is_null() {
        info = unsafe { probe_gmdid_display(display, core::ptr::addr_of_mut!(ip_ver)) };
    }
    if info.is_null() {
        unsafe { (*display).info.__device_info = core::ptr::addr_of!(NO_DISPLAY) as *mut _ };
        return display;
    }

    unsafe {
        (*display).info.__device_info = info.cast_mut();
        core::ptr::copy_nonoverlapping(
            core::ptr::addr_of!((*info).__runtime_defaults),
            core::ptr::addr_of_mut!((*display).info.__runtime_info),
            1,
        );

        core::ptr::copy_nonoverlapping(
            core::ptr::addr_of!((*desc).platforms),
            core::ptr::addr_of_mut!((*display).platform),
            1,
        );
    }

    let subdesc = unsafe { find_subplatform_desc(pdev, desc) };
    if !subdesc.is_null() {
        unsafe {
            display_platforms_or(
                core::ptr::addr_of_mut!((*display).platform),
                core::ptr::addr_of!((*subdesc).platforms),
            );
        }
    }

    let step = if ip_ver.ver != 0 || ip_ver.rel != 0 || ip_ver.step != 0 {
        let step_offset = ip_ver.step as i32;
        unsafe { (*display).info.__runtime_info.ip = ip_ver };
        STEP_A0 + step_offset
    } else {
        unsafe {
            get_pre_gmdid_step(
                display,
                core::ptr::addr_of!((*desc).step_info),
                if !subdesc.is_null() { core::ptr::addr_of!((*subdesc).step_info) } else { core::ptr::null() },
            )
        }
    };

    unsafe { initialize_step(display, step) };

    display
}

// upstream: intel_display_device.c intel_display_device_remove()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_device_remove(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    intel_display_params_free(core::ptr::addr_of_mut!((*display).params));
    tk_linux_kapi::memory::kfree(display);
}

// upstream: intel_display_device.c __intel_display_device_info_runtime_init()
pub unsafe fn __intel_display_device_info_runtime_init(display: *mut IntelDisplay) {
    let runtime = core::ptr::addr_of_mut!((*display).info.__runtime_info);
    let ver = (*runtime).ip.ver;

    // Wa_14011765242: adl-s A0,A1 scalers
    if ver >= 11 {
        (*runtime).num_scalers = [2, 2, 2, 2];
    } else if ver >= 9 {
        (*runtime).num_scalers = [2, 2, 1, 0];
    }

    if ver >= 13 {
        (*runtime).num_sprites = [4, 4, 4, 4];
    } else if ver >= 11 {
        (*runtime).num_sprites = [6, 6, 6, 6];
    }

    if ver >= 9 {
        let dfsm = intel_de_read(display, SKL_DFSM);

        if (dfsm & SKL_DFSM_PIPE_A_DISABLE) != 0 {
            (*runtime).pipe_mask &= !(1 << 0);
            (*runtime).cpu_transcoder_mask &= !(1 << 0);
            (*runtime).fbc_mask &= !1;
        }
        if (dfsm & SKL_DFSM_PIPE_B_DISABLE) != 0 {
            (*runtime).pipe_mask &= !(1 << 1);
            (*runtime).cpu_transcoder_mask &= !(1 << 1);
            (*runtime).fbc_mask &= !(1 << 1);
        }
        if (dfsm & SKL_DFSM_PIPE_C_DISABLE) != 0 {
            (*runtime).pipe_mask &= !(1 << 2);
            (*runtime).cpu_transcoder_mask &= !(1 << 2);
            (*runtime).fbc_mask &= !(1 << 2);
        }
        if ver >= 12 && (dfsm & TGL_DFSM_PIPE_D_DISABLE) != 0 {
            (*runtime).pipe_mask &= !(1 << 3);
            (*runtime).cpu_transcoder_mask &= !(1 << 3);
            (*runtime).fbc_mask &= !(1 << 3);
        }

        if (dfsm & SKL_DFSM_DISPLAY_HDCP_DISABLE) != 0 {
            (*runtime).has_hdcp = false;
        }

        if ver >= 11 && (dfsm & ICL_DFSM_DMC_DISABLE) != 0 {
            (*runtime).has_dmc = false;
        }

        if (10..=12).contains(&ver) && (dfsm & GLK_DFSM_DISPLAY_DSC_DISABLE) != 0 {
            (*runtime).has_dsc = false;
        }

        if (*runtime).pipe_mask == 0 {
            core::ptr::write_bytes(runtime.cast::<u8>(), 0, core::mem::size_of::<IntelDisplayRuntimeInfo>());
            (*display).info.__device_info = core::ptr::addr_of!(NO_DISPLAY) as *mut _;
            return;
        }
    }

    (*runtime).rawclk_freq = crate::display_pending_stubs::b3e::intel_read_rawclk(display);
}

// upstream: intel_display_device.c intel_display_device_info_runtime_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_device_info_runtime_init(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    if intel_display_device_present(display) {
        __intel_display_device_info_runtime_init(display);
    }
    if !intel_display_device_present(display) {
        (*display).info.__device_info = core::ptr::addr_of!(NO_DISPLAY) as *mut _;
    }
}

// upstream: intel_display_device.c intel_display_device_info_print()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_device_info_print(
    _info: *const IntelDisplayDeviceInfo,
    _runtime: *const IntelDisplayRuntimeInfo,
    _p: *mut DrmPrinter,
) {
}

// upstream: intel_display_device.c intel_display_device_present()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_device_present(display: *mut IntelDisplay) -> bool {
    if display.is_null() {
        false
    } else {
        let info = (*display).info.__device_info;
        !info.is_null() && info != (core::ptr::addr_of!(NO_DISPLAY) as *mut _)
    }
}

// upstream: intel_display_device.c intel_display_device_enabled()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_device_enabled(display: *mut IntelDisplay) -> bool {
    if display.is_null() {
        false
    } else {
        !(*display).params.disable_display && intel_display_device_present(display)
    }
}
