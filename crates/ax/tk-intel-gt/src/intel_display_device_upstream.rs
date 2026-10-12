// SPDX-License-Identifier: MIT
// Copyright © 2023 Intel Corporation
// Source: drivers/gpu/drm/i915/display/intel_display_device.c and intel_display_device.h

#![allow(non_snake_case, dead_code, unsafe_code)]

use crate::intel_de_upstream::intel_de_read;
use crate::intel_display_core_types_upstream::IntelDisplay;
use crate::intel_display_device_types_upstream::{
    IntelDisplayDeviceInfo, IntelDisplayDeviceInfoColor, IntelDisplayDeviceInfoDbuf,
    IntelDisplayIpVer, IntelDisplayRuntimeInfo,
};
use crate::intel_display_params_upstream::{intel_display_params_copy, intel_display_params_free};
use crate::intel_display_regs_upstream::{
    ICL_DFSM_DMC_DISABLE, SKL_DFSM, SKL_DFSM_DISPLAY_HDCP_DISABLE, SKL_DFSM_PIPE_A_DISABLE,
    SKL_DFSM_PIPE_B_DISABLE, SKL_DFSM_PIPE_C_DISABLE, TGL_DFSM_PIPE_D_DISABLE,
};
use crate::intel_parent_upstream::IntelDisplayParentInterface;
use crate::intel_pciids_upstream::{PCI_ADLN_IDS, PCI_ADLP_IDS};
use crate::intel_quirks_upstream::PciDev;
use tk_drm_upstream::drm_print_types::DrmPrinter;

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

fn is_adlp_pci_id(id: u16) -> bool {
    PCI_ADLP_IDS.contains(&id)
}

fn is_adln_pci_id(id: u16) -> bool {
    PCI_ADLN_IDS.contains(&id)
}

// upstream: intel_display_device.c intel_display_device_probe()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_device_probe(
    pdev: *mut PciDev,
    parent: *const IntelDisplayParentInterface,
) -> *mut IntelDisplay {
    let display = tk_linux_kapi::memory::kzalloc(
        core::mem::size_of::<IntelDisplay>(),
        tk_linux_kapi::config::GFP_KERNEL,
    )
    .cast::<IntelDisplay>();

    if display.is_null() {
        return core::ptr::null_mut();
    }

    (*display).parent = parent.cast_mut().cast();
    intel_display_params_copy(core::ptr::addr_of_mut!((*display).params));

    if pdev.is_null() {
        (*display).info.__device_info = core::ptr::addr_of!(NO_DISPLAY) as *mut _;
        return display;
    }

    let dev_id = (*pdev).device;
    let is_adlp = is_adlp_pci_id(dev_id);
    let is_adln = is_adln_pci_id(dev_id);

    if is_adlp || is_adln {
        (*display).info.__device_info = core::ptr::addr_of!(XE_LPD_DISPLAY) as *mut _;
        core::ptr::copy_nonoverlapping(
            core::ptr::addr_of!(XE_LPD_DISPLAY.__runtime_defaults),
            core::ptr::addr_of_mut!((*display).info.__runtime_info),
            1,
        );

        // Platform flags: alderlake_p bit 52, mobile bit 1
        (*display).platform._anon_0_0.bitmap[0] |= (1u64 << 52) | (1u64 << 1);
        if is_adln {
            // alderlake_p_alderlake_n bit 53
            (*display).platform._anon_0_0.bitmap[0] |= 1u64 << 53;
        }
    } else {
        // Platform not display 13 / ADL-P
        (*display).info.__device_info = core::ptr::addr_of!(NO_DISPLAY) as *mut _;
    }

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

// upstream: intel_display_device.c intel_display_device_info_runtime_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_device_info_runtime_init(display: *mut IntelDisplay) {
    if display.is_null() || !intel_display_device_present(display) {
        return;
    }

    let runtime = core::ptr::addr_of_mut!((*display).info.__runtime_info);
    let ver = (*runtime).ip.ver;

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

        if (10..=12).contains(&ver) && (dfsm & crate::intel_display_regs_upstream::GLK_DFSM_DISPLAY_DSC_DISABLE) != 0 {
            (*runtime).has_dsc = false;
        }

        if (*runtime).pipe_mask == 0 {
            // All pipes fused off
            core::ptr::write_bytes(runtime.cast::<u8>(), 0, core::mem::size_of::<IntelDisplayRuntimeInfo>());
            (*display).info.__device_info = core::ptr::addr_of!(NO_DISPLAY) as *mut _;
            return;
        }
    }

    (*runtime).rawclk_freq = crate::display_pending_stubs::b3e::intel_read_rawclk(display);
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

// upstream: intel_display_device.c intel_display_device_info_print()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_device_info_print(
    _info: *const IntelDisplayDeviceInfo,
    _runtime: *const IntelDisplayRuntimeInfo,
    _p: *mut DrmPrinter,
) {
}
