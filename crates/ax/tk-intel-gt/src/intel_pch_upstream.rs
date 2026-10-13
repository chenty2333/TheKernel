// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 PCH detection and clock gating initialization, translated from
//! `drivers/gpu/drm/i915/display/intel_pch.c`.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]

use crate::intel_de_upstream::{intel_de_read, intel_de_rmw, intel_de_write};
use crate::intel_display_core_types_upstream::IntelDisplay;
use crate::intel_display_device_upstream::intel_display_device_enabled;
use crate::intel_display_regs_upstream::*;
use crate::intel_display_utils_upstream::intel_display_run_as_guest;
use crate::intel_pch_types_upstream::*;
use crate::intel_quirks_upstream::PciDev;

const INTEL_PCH_DEVICE_ID_MASK: u16 = 0xff80;
const INTEL_PCH_IBX_DEVICE_ID_TYPE: u16 = 0x3b00;
const INTEL_PCH_CPT_DEVICE_ID_TYPE: u16 = 0x1c00;
const INTEL_PCH_PPT_DEVICE_ID_TYPE: u16 = 0x1e00;
const INTEL_PCH_LPT_DEVICE_ID_TYPE: u16 = 0x8c00;
const INTEL_PCH_LPT_LP_DEVICE_ID_TYPE: u16 = 0x9c00;
const INTEL_PCH_WPT_DEVICE_ID_TYPE: u16 = 0x8c80;
const INTEL_PCH_WPT_LP_DEVICE_ID_TYPE: u16 = 0x9c80;
const INTEL_PCH_SPT_DEVICE_ID_TYPE: u16 = 0xA100;
const INTEL_PCH_SPT_LP_DEVICE_ID_TYPE: u16 = 0x9D00;
const INTEL_PCH_KBP_DEVICE_ID_TYPE: u16 = 0xA280;
const INTEL_PCH_CNP_DEVICE_ID_TYPE: u16 = 0xA300;
const INTEL_PCH_CNP_LP_DEVICE_ID_TYPE: u16 = 0x9D80;
const INTEL_PCH_CMP_DEVICE_ID_TYPE: u16 = 0x0280;
const INTEL_PCH_CMP2_DEVICE_ID_TYPE: u16 = 0x0680;
const INTEL_PCH_CMP_V_DEVICE_ID_TYPE: u16 = 0xA380;
const INTEL_PCH_ICP_DEVICE_ID_TYPE: u16 = 0x3480;
const INTEL_PCH_ICP2_DEVICE_ID_TYPE: u16 = 0x3880;
const INTEL_PCH_MCC_DEVICE_ID_TYPE: u16 = 0x4B00;
const INTEL_PCH_TGP_DEVICE_ID_TYPE: u16 = 0xA080;
const INTEL_PCH_TGP2_DEVICE_ID_TYPE: u16 = 0x4380;
const INTEL_PCH_JSP_DEVICE_ID_TYPE: u16 = 0x4D80;
const INTEL_PCH_ADP_DEVICE_ID_TYPE: u16 = 0x7A80;
const INTEL_PCH_ADP2_DEVICE_ID_TYPE: u16 = 0x5180;
const INTEL_PCH_ADP3_DEVICE_ID_TYPE: u16 = 0x7A00;
const INTEL_PCH_ADP4_DEVICE_ID_TYPE: u16 = 0x5480;
const INTEL_PCH_P2X_DEVICE_ID_TYPE: u16 = 0x7100;
const INTEL_PCH_P3X_DEVICE_ID_TYPE: u16 = 0x7000;
const INTEL_PCH_QEMU_DEVICE_ID_TYPE: u16 = 0x2900;

const PCI_CLASS_BRIDGE_ISA: u32 = 0x0601;
const PCI_VENDOR_ID_INTEL: u16 = 0x8086;
const PCI_SUBVENDOR_ID_REDHAT_QUMRANET: u16 = 0x1af4;
const PCI_SUBDEVICE_ID_QEMU: u16 = 0x1100;

// Platform bit indices from intel_display_device_types_upstream.rs
const PLATFORM_IVYBRIDGE: u32 = 19;
const PLATFORM_HASWELL: u32 = 21;
const PLATFORM_HASWELL_ULT: u32 = 22;
const PLATFORM_BROADWELL: u32 = 24;
const PLATFORM_BROADWELL_ULT: u32 = 25;
const PLATFORM_SKYLAKE: u32 = 28;
const PLATFORM_BROXTON: u32 = 31;
const PLATFORM_KABYLAKE: u32 = 32;
const PLATFORM_GEMINILAKE: u32 = 35;
const PLATFORM_COFFEELAKE: u32 = 36;
const PLATFORM_COMETLAKE: u32 = 39;
const PLATFORM_ICELAKE: u32 = 42;
const PLATFORM_JASPERLAKE: u32 = 44;
const PLATFORM_ELKHARTLAKE: u32 = 45;
const PLATFORM_TIGERLAKE: u32 = 46;
const PLATFORM_ROCKETLAKE: u32 = 48;
const PLATFORM_DG1: u32 = 49;
const PLATFORM_ALDERLAKE_S: u32 = 50;
const PLATFORM_ALDERLAKE_P: u32 = 52;
const PLATFORM_DG2: u32 = 56;
const PLATFORM_METEORLAKE: u32 = 60;
const PLATFORM_BATTLEMAGE: u32 = 63;

#[inline]
unsafe fn platform_test(display: *const IntelDisplay, bit: u32) -> bool {
    if display.is_null() {
        return false;
    }
    let idx = (bit / 64) as usize;
    let b = bit % 64;
    ((*display).platform._anon_0_0.bitmap[idx] & (1u64 << b)) != 0
}

#[inline]
unsafe fn display_ver(display: *const IntelDisplay) -> u16 {
    if display.is_null() {
        0
    } else {
        (*display).info.__runtime_info.ip.ver
    }
}

#[inline]
unsafe fn has_display(display: *mut IntelDisplay) -> bool {
    intel_display_device_enabled(display)
}

unsafe extern "C" {
    fn pci_get_class(class: u32, from: *mut PciDev) -> *mut PciDev;
    fn pci_dev_put(dev: *mut PciDev);
}

// upstream: intel_pch.c intel_pch_fake_for_south_display()
unsafe fn intel_pch_fake_for_south_display(display: *mut IntelDisplay) -> IntelPch {
    let mut pch_type = PCH_NONE;

    if display_ver(display) >= 20 {
        pch_type = PCH_LNL;
    } else if platform_test(display, PLATFORM_BATTLEMAGE)
        || platform_test(display, PLATFORM_METEORLAKE)
    {
        pch_type = PCH_MTL;
    } else if platform_test(display, PLATFORM_DG2) {
        pch_type = PCH_DG2;
    } else if platform_test(display, PLATFORM_DG1) {
        pch_type = PCH_DG1;
    }

    pch_type
}

// upstream: intel_pch.c intel_pch_type()
unsafe fn intel_pch_type(display: *const IntelDisplay, id: u16) -> IntelPch {
    match id {
        INTEL_PCH_IBX_DEVICE_ID_TYPE => PCH_IBX,
        INTEL_PCH_CPT_DEVICE_ID_TYPE => PCH_CPT,
        INTEL_PCH_PPT_DEVICE_ID_TYPE => {
            // PPT is CPT compatible
            PCH_CPT
        }
        INTEL_PCH_LPT_DEVICE_ID_TYPE => PCH_LPT_H,
        INTEL_PCH_LPT_LP_DEVICE_ID_TYPE => PCH_LPT_LP,
        INTEL_PCH_WPT_DEVICE_ID_TYPE => {
            // WPT is LPT compatible
            PCH_LPT_H
        }
        INTEL_PCH_WPT_LP_DEVICE_ID_TYPE => {
            // WPT is LPT compatible
            PCH_LPT_LP
        }
        INTEL_PCH_SPT_DEVICE_ID_TYPE | INTEL_PCH_SPT_LP_DEVICE_ID_TYPE => PCH_SPT,
        INTEL_PCH_KBP_DEVICE_ID_TYPE => {
            // KBP is SPT compatible
            PCH_SPT
        }
        INTEL_PCH_CNP_DEVICE_ID_TYPE | INTEL_PCH_CNP_LP_DEVICE_ID_TYPE => PCH_CNP,
        INTEL_PCH_CMP_DEVICE_ID_TYPE | INTEL_PCH_CMP2_DEVICE_ID_TYPE => {
            // CMP is CNP compatible
            PCH_CNP
        }
        INTEL_PCH_CMP_V_DEVICE_ID_TYPE => {
            // CMP-V is based on KBP, which is SPT compatible
            PCH_SPT
        }
        INTEL_PCH_ICP_DEVICE_ID_TYPE | INTEL_PCH_ICP2_DEVICE_ID_TYPE => PCH_ICP,
        INTEL_PCH_MCC_DEVICE_ID_TYPE => {
            // MCC is TGP compatible
            PCH_TGP
        }
        INTEL_PCH_TGP_DEVICE_ID_TYPE | INTEL_PCH_TGP2_DEVICE_ID_TYPE => PCH_TGP,
        INTEL_PCH_JSP_DEVICE_ID_TYPE => {
            // JSP is ICP compatible
            PCH_ICP
        }
        INTEL_PCH_ADP_DEVICE_ID_TYPE
        | INTEL_PCH_ADP2_DEVICE_ID_TYPE
        | INTEL_PCH_ADP3_DEVICE_ID_TYPE
        | INTEL_PCH_ADP4_DEVICE_ID_TYPE => PCH_ADP,
        _ => PCH_NONE,
    }
}

// upstream: intel_pch.c intel_pch_ibx_init_clock_gating()
unsafe fn intel_pch_ibx_init_clock_gating(display: *mut IntelDisplay) {
    intel_de_write(
        display,
        SOUTH_DSPCLK_GATE_D,
        PCH_DPLSUNIT_CLOCK_GATE_DISABLE,
    );
}

// upstream: intel_pch.c intel_pch_cpt_init_clock_gating()
unsafe fn intel_pch_cpt_init_clock_gating(display: *mut IntelDisplay) {
    intel_de_write(
        display,
        SOUTH_DSPCLK_GATE_D,
        PCH_DPLSUNIT_CLOCK_GATE_DISABLE
            | PCH_DPLUNIT_CLOCK_GATE_DISABLE
            | PCH_CPUNIT_CLOCK_GATE_DISABLE,
    );
    intel_de_rmw(display, SOUTH_CHICKEN2, 0, DPLS_EDP_PPS_FIX_DIS);

    for pipe in 0..3u32 {
        let mut val = intel_de_read(display, TRANS_CHICKEN2(pipe));
        val |= TRANS_CHICKEN2_TIMING_OVERRIDE;
        val &= !TRANS_CHICKEN2_FDI_POLARITY_REVERSED;
        val &= !TRANS_CHICKEN2_DISABLE_DEEP_COLOR_COUNTER;
        val &= !TRANS_CHICKEN2_DISABLE_DEEP_COLOR_MODESWITCH;
        intel_de_write(display, TRANS_CHICKEN2(pipe), val);
    }

    for pipe in 0..3u32 {
        intel_de_write(
            display,
            TRANS_CHICKEN1(pipe),
            TRANS_CHICKEN1_DP0UNIT_GC_DISABLE,
        );
    }
}

// upstream: intel_pch.c intel_pch_lpt_init_clock_gating()
unsafe fn intel_pch_lpt_init_clock_gating(display: *mut IntelDisplay) {
    if (*display).pch_type == PCH_LPT_LP {
        intel_de_rmw(
            display,
            SOUTH_DSPCLK_GATE_D,
            0,
            PCH_LP_PARTITION_LEVEL_DISABLE,
        );
    }

    intel_de_rmw(
        display,
        TRANS_CHICKEN1(0),
        0,
        TRANS_CHICKEN1_DP0UNIT_GC_DISABLE,
    );
}

// upstream: intel_pch.c intel_pch_cnp_init_clock_gating()
unsafe fn intel_pch_cnp_init_clock_gating(display: *mut IntelDisplay) {
    intel_de_rmw(display, SOUTH_DSPCLK_GATE_D, 0, CNP_PWM_CGE_GATING_DISABLE);
}

// upstream: intel_pch.c intel_pch_init_clock_gating()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pch_init_clock_gating(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    match (*display).pch_type {
        PCH_IBX => intel_pch_ibx_init_clock_gating(display),
        PCH_CPT => intel_pch_cpt_init_clock_gating(display),
        PCH_LPT_H | PCH_LPT_LP => intel_pch_lpt_init_clock_gating(display),
        PCH_CNP => intel_pch_cnp_init_clock_gating(display),
        _ => {}
    }
}

// upstream: intel_pch.c intel_is_virt_pch()
fn intel_is_virt_pch(id: u16, svendor: u16, sdevice: u16) -> bool {
    id == INTEL_PCH_P2X_DEVICE_ID_TYPE
        || id == INTEL_PCH_P3X_DEVICE_ID_TYPE
        || (id == INTEL_PCH_QEMU_DEVICE_ID_TYPE
            && svendor == PCI_SUBVENDOR_ID_REDHAT_QUMRANET
            && sdevice == PCI_SUBDEVICE_ID_QEMU)
}

// upstream: intel_pch.c intel_virt_detect_pch()
unsafe fn intel_virt_detect_pch(
    display: *const IntelDisplay,
    pch_id: *mut u16,
    pch_type: *mut IntelPch,
) {
    let mut id: u16 = 0;

    if platform_test(display, PLATFORM_ALDERLAKE_S) || platform_test(display, PLATFORM_ALDERLAKE_P)
    {
        id = INTEL_PCH_ADP_DEVICE_ID_TYPE;
    } else if platform_test(display, PLATFORM_TIGERLAKE)
        || platform_test(display, PLATFORM_ROCKETLAKE)
    {
        id = INTEL_PCH_TGP_DEVICE_ID_TYPE;
    } else if platform_test(display, PLATFORM_JASPERLAKE)
        || platform_test(display, PLATFORM_ELKHARTLAKE)
    {
        id = INTEL_PCH_MCC_DEVICE_ID_TYPE;
    } else if platform_test(display, PLATFORM_ICELAKE) {
        id = INTEL_PCH_ICP_DEVICE_ID_TYPE;
    } else if platform_test(display, PLATFORM_COFFEELAKE)
        || platform_test(display, PLATFORM_COMETLAKE)
    {
        id = INTEL_PCH_CNP_DEVICE_ID_TYPE;
    } else if platform_test(display, PLATFORM_KABYLAKE) || platform_test(display, PLATFORM_SKYLAKE)
    {
        id = INTEL_PCH_SPT_DEVICE_ID_TYPE;
    } else if platform_test(display, PLATFORM_HASWELL_ULT)
        || platform_test(display, PLATFORM_BROADWELL_ULT)
    {
        id = INTEL_PCH_LPT_LP_DEVICE_ID_TYPE;
    } else if platform_test(display, PLATFORM_HASWELL)
        || platform_test(display, PLATFORM_BROADWELL)
    {
        id = INTEL_PCH_LPT_DEVICE_ID_TYPE;
    } else if display_ver(display) == 6 || platform_test(display, PLATFORM_IVYBRIDGE) {
        id = INTEL_PCH_CPT_DEVICE_ID_TYPE;
    } else if display_ver(display) == 5 {
        id = INTEL_PCH_IBX_DEVICE_ID_TYPE;
    }

    *pch_type = intel_pch_type(display, id);

    if id != 0 && *pch_type == PCH_NONE {
        id = 0;
    }

    *pch_id = id;
}

#[inline]
unsafe fn pch_vendor(pch: *mut PciDev) -> u16 {
    let mock = pch.cast::<tk_linux_kapi::pci::MockPciDevice>();
    if (*mock).vendor != 0 && (*mock).vendor <= 0xffff {
        (*mock).vendor as u16
    } else {
        (*pch).vendor
    }
}

#[inline]
unsafe fn pch_device(pch: *mut PciDev) -> u16 {
    let mock = pch.cast::<tk_linux_kapi::pci::MockPciDevice>();
    if (*mock).vendor != 0 && (*mock).vendor <= 0xffff {
        (*mock).device as u16
    } else {
        (*pch).device
    }
}

#[inline]
unsafe fn pch_subsystem_vendor(pch: *mut PciDev) -> u16 {
    let mock = pch.cast::<tk_linux_kapi::pci::MockPciDevice>();
    if (*mock).vendor != 0 && (*mock).vendor <= 0xffff {
        (*mock).subvendor as u16
    } else {
        (*pch).subsystem_vendor
    }
}

#[inline]
unsafe fn pch_subsystem_device(pch: *mut PciDev) -> u16 {
    let mock = pch.cast::<tk_linux_kapi::pci::MockPciDevice>();
    if (*mock).vendor != 0 && (*mock).vendor <= 0xffff {
        (*mock).subdevice as u16
    } else {
        (*pch).subsystem_device
    }
}

// upstream: intel_pch.c intel_pch_detect()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pch_detect(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }

    let fake_type = intel_pch_fake_for_south_display(display);
    if fake_type != PCH_NONE {
        (*display).pch_type = fake_type;
        return;
    }

    let mut pch: *mut PciDev = core::ptr::null_mut();
    let mut id: u16 = 0;
    let mut detected_type = PCH_NONE;

    loop {
        pch = pci_get_class(PCI_CLASS_BRIDGE_ISA << 8, pch);
        if pch.is_null() {
            break;
        }

        if pch_vendor(pch) != PCI_VENDOR_ID_INTEL {
            continue;
        }

        id = pch_device(pch) & INTEL_PCH_DEVICE_ID_MASK;
        detected_type = intel_pch_type(display, id);
        if detected_type != PCH_NONE {
            (*display).pch_type = detected_type;
            break;
        } else if intel_is_virt_pch(id, pch_subsystem_vendor(pch), pch_subsystem_device(pch)) {
            intel_virt_detect_pch(display, &mut id, &mut detected_type);
            (*display).pch_type = detected_type;
            break;
        }
    }

    if !pch.is_null() && !has_display(display) {
        (*display).pch_type = PCH_NOP;
    } else if pch.is_null() {
        if intel_display_run_as_guest(display) && has_display(display) {
            intel_virt_detect_pch(display, &mut id, &mut detected_type);
            (*display).pch_type = detected_type;
        }
    }

    pci_dev_put(pch);
}
