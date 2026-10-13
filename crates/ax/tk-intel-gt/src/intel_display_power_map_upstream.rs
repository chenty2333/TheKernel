// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 display power map, translated from `drivers/gpu/drm/i915/display/intel_display_power_map.c`.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]

use core::ffi::{c_char, c_void};
use core::mem::ManuallyDrop;

use crate::intel_display_core_types_upstream::IntelDisplay;
use crate::intel_display_device_upstream::intel_display_device_enabled;
use crate::intel_display_power_types_upstream::*;
use crate::intel_display_power_well_types_upstream::*;
use crate::intel_display_power_well_upstream::*;
use crate::intel_display_regs_upstream::*;
use crate::linux_memory::{kfree, kzalloc_objs};

const fn hsw_inst(
    name: &'static [u8],
    domain_list: *const I915PowerDomainList,
    id: I915PowerWellId,
    idx: u8,
) -> I915PowerWellInstance {
    I915PowerWellInstance {
        name: name.as_ptr() as *mut c_char,
        domain_list: domain_list as *mut c_void,
        id,
        _anon_20_3: I915PowerWellInstanceAnon20x3 {
            hsw: ManuallyDrop::new(I915PowerWellInstanceAnon20x3Hsw { idx }),
        },
    }
}

const fn simple_inst(
    name: &'static [u8],
    domain_list: *const I915PowerDomainList,
    id: I915PowerWellId,
) -> I915PowerWellInstance {
    I915PowerWellInstance {
        name: name.as_ptr() as *mut c_char,
        domain_list: domain_list as *mut c_void,
        id,
        _anon_20_3: I915PowerWellInstanceAnon20x3 {
            hsw: ManuallyDrop::new(I915PowerWellInstanceAnon20x3Hsw { idx: 0 }),
        },
    }
}

const fn make_desc(
    ops: &'static I915PowerWellOps,
    instances: &'static I915PowerWellInstanceList,
    irq_pipe_mask: u8,
    always_on: bool,
    fixed_enable_delay: bool,
    has_fuses: bool,
    is_tc_tbt: bool,
    enable_timeout: u16,
) -> I915PowerWellDesc {
    let mut b = irq_pipe_mask & 0x0f;
    if always_on {
        b |= 0x10;
    }
    if fixed_enable_delay {
        b |= 0x20;
    }
    if has_fuses {
        b |= 0x40;
    }
    if is_tc_tbt {
        b |= 0x80;
    }
    I915PowerWellDesc {
        ops: ops as *const _ as *mut c_void,
        instances: instances as *const _ as *mut c_void,
        _bitfields_16: [b],
        _pad_17: [0],
        enable_timeout,
        _pad_20: [0; 4],
    }
}

// --- Power domain definitions for ADL-P / XELPD ---

static I9XX_PWDOMS_ALWAYS_ON: I915PowerDomainList = I915PowerDomainList {
    list: core::ptr::null(),
    count: 0,
};

static GLK_PWDOMS_DDI_IO_A_DOMS: [IntelDisplayPowerDomain; 1] = [POWER_DOMAIN_PORT_DDI_IO_A];
static GLK_PWDOMS_DDI_IO_A: I915PowerDomainList = I915PowerDomainList {
    list: GLK_PWDOMS_DDI_IO_A_DOMS.as_ptr(),
    count: 1,
};

static GLK_PWDOMS_DDI_IO_B_DOMS: [IntelDisplayPowerDomain; 1] = [POWER_DOMAIN_PORT_DDI_IO_B];
static GLK_PWDOMS_DDI_IO_B: I915PowerDomainList = I915PowerDomainList {
    list: GLK_PWDOMS_DDI_IO_B_DOMS.as_ptr(),
    count: 1,
};

static GLK_PWDOMS_DDI_IO_C_DOMS: [IntelDisplayPowerDomain; 1] = [POWER_DOMAIN_PORT_DDI_IO_C];
static GLK_PWDOMS_DDI_IO_C: I915PowerDomainList = I915PowerDomainList {
    list: GLK_PWDOMS_DDI_IO_C_DOMS.as_ptr(),
    count: 1,
};

static ICL_PWDOMS_DDI_IO_D_DOMS: [IntelDisplayPowerDomain; 1] = [POWER_DOMAIN_PORT_DDI_IO_D];
static ICL_PWDOMS_DDI_IO_D: I915PowerDomainList = I915PowerDomainList {
    list: ICL_PWDOMS_DDI_IO_D_DOMS.as_ptr(),
    count: 1,
};

static ICL_PWDOMS_DDI_IO_E_DOMS: [IntelDisplayPowerDomain; 1] = [POWER_DOMAIN_PORT_DDI_IO_E];
static ICL_PWDOMS_DDI_IO_E: I915PowerDomainList = I915PowerDomainList {
    list: ICL_PWDOMS_DDI_IO_E_DOMS.as_ptr(),
    count: 1,
};

static TGL_PWDOMS_DDI_IO_TC1_DOMS: [IntelDisplayPowerDomain; 1] = [POWER_DOMAIN_PORT_DDI_IO_TC1];
static TGL_PWDOMS_DDI_IO_TC1: I915PowerDomainList = I915PowerDomainList {
    list: TGL_PWDOMS_DDI_IO_TC1_DOMS.as_ptr(),
    count: 1,
};

static TGL_PWDOMS_DDI_IO_TC2_DOMS: [IntelDisplayPowerDomain; 1] = [POWER_DOMAIN_PORT_DDI_IO_TC2];
static TGL_PWDOMS_DDI_IO_TC2: I915PowerDomainList = I915PowerDomainList {
    list: TGL_PWDOMS_DDI_IO_TC2_DOMS.as_ptr(),
    count: 1,
};

static TGL_PWDOMS_DDI_IO_TC3_DOMS: [IntelDisplayPowerDomain; 1] = [POWER_DOMAIN_PORT_DDI_IO_TC3];
static TGL_PWDOMS_DDI_IO_TC3: I915PowerDomainList = I915PowerDomainList {
    list: TGL_PWDOMS_DDI_IO_TC3_DOMS.as_ptr(),
    count: 1,
};

static TGL_PWDOMS_DDI_IO_TC4_DOMS: [IntelDisplayPowerDomain; 1] = [POWER_DOMAIN_PORT_DDI_IO_TC4];
static TGL_PWDOMS_DDI_IO_TC4: I915PowerDomainList = I915PowerDomainList {
    list: TGL_PWDOMS_DDI_IO_TC4_DOMS.as_ptr(),
    count: 1,
};

static ICL_PWDOMS_AUX_A_DOMS: [IntelDisplayPowerDomain; 2] = [
    POWER_DOMAIN_AUX_IO_A,
    POWER_DOMAIN_AUX_A,
];
static ICL_PWDOMS_AUX_A: I915PowerDomainList = I915PowerDomainList {
    list: ICL_PWDOMS_AUX_A_DOMS.as_ptr(),
    count: 2,
};

static ICL_PWDOMS_AUX_B_DOMS: [IntelDisplayPowerDomain; 2] = [
    POWER_DOMAIN_AUX_IO_B,
    POWER_DOMAIN_AUX_B,
];
static ICL_PWDOMS_AUX_B: I915PowerDomainList = I915PowerDomainList {
    list: ICL_PWDOMS_AUX_B_DOMS.as_ptr(),
    count: 2,
};

static ICL_PWDOMS_AUX_C_DOMS: [IntelDisplayPowerDomain; 2] = [
    POWER_DOMAIN_AUX_IO_C,
    POWER_DOMAIN_AUX_C,
];
static ICL_PWDOMS_AUX_C: I915PowerDomainList = I915PowerDomainList {
    list: ICL_PWDOMS_AUX_C_DOMS.as_ptr(),
    count: 2,
};

static ICL_PWDOMS_AUX_D_DOMS: [IntelDisplayPowerDomain; 2] = [
    POWER_DOMAIN_AUX_IO_D,
    POWER_DOMAIN_AUX_D,
];
static ICL_PWDOMS_AUX_D: I915PowerDomainList = I915PowerDomainList {
    list: ICL_PWDOMS_AUX_D_DOMS.as_ptr(),
    count: 2,
};

static ICL_PWDOMS_AUX_E_DOMS: [IntelDisplayPowerDomain; 2] = [
    POWER_DOMAIN_AUX_IO_E,
    POWER_DOMAIN_AUX_E,
];
static ICL_PWDOMS_AUX_E: I915PowerDomainList = I915PowerDomainList {
    list: ICL_PWDOMS_AUX_E_DOMS.as_ptr(),
    count: 2,
};

static TGL_PWDOMS_AUX_USBC1_DOMS: [IntelDisplayPowerDomain; 1] = [POWER_DOMAIN_AUX_USBC1];
static TGL_PWDOMS_AUX_USBC1: I915PowerDomainList = I915PowerDomainList {
    list: TGL_PWDOMS_AUX_USBC1_DOMS.as_ptr(),
    count: 1,
};

static TGL_PWDOMS_AUX_USBC2_DOMS: [IntelDisplayPowerDomain; 1] = [POWER_DOMAIN_AUX_USBC2];
static TGL_PWDOMS_AUX_USBC2: I915PowerDomainList = I915PowerDomainList {
    list: TGL_PWDOMS_AUX_USBC2_DOMS.as_ptr(),
    count: 1,
};

static TGL_PWDOMS_AUX_USBC3_DOMS: [IntelDisplayPowerDomain; 1] = [POWER_DOMAIN_AUX_USBC3];
static TGL_PWDOMS_AUX_USBC3: I915PowerDomainList = I915PowerDomainList {
    list: TGL_PWDOMS_AUX_USBC3_DOMS.as_ptr(),
    count: 1,
};

static TGL_PWDOMS_AUX_USBC4_DOMS: [IntelDisplayPowerDomain; 1] = [POWER_DOMAIN_AUX_USBC4];
static TGL_PWDOMS_AUX_USBC4: I915PowerDomainList = I915PowerDomainList {
    list: TGL_PWDOMS_AUX_USBC4_DOMS.as_ptr(),
    count: 1,
};

static ICL_PWDOMS_AUX_TBT1_DOMS: [IntelDisplayPowerDomain; 1] = [POWER_DOMAIN_AUX_TBT1];
static ICL_PWDOMS_AUX_TBT1: I915PowerDomainList = I915PowerDomainList {
    list: ICL_PWDOMS_AUX_TBT1_DOMS.as_ptr(),
    count: 1,
};

static ICL_PWDOMS_AUX_TBT2_DOMS: [IntelDisplayPowerDomain; 1] = [POWER_DOMAIN_AUX_TBT2];
static ICL_PWDOMS_AUX_TBT2: I915PowerDomainList = I915PowerDomainList {
    list: ICL_PWDOMS_AUX_TBT2_DOMS.as_ptr(),
    count: 1,
};

static ICL_PWDOMS_AUX_TBT3_DOMS: [IntelDisplayPowerDomain; 1] = [POWER_DOMAIN_AUX_TBT3];
static ICL_PWDOMS_AUX_TBT3: I915PowerDomainList = I915PowerDomainList {
    list: ICL_PWDOMS_AUX_TBT3_DOMS.as_ptr(),
    count: 1,
};

static ICL_PWDOMS_AUX_TBT4_DOMS: [IntelDisplayPowerDomain; 1] = [POWER_DOMAIN_AUX_TBT4];
static ICL_PWDOMS_AUX_TBT4: I915PowerDomainList = I915PowerDomainList {
    list: ICL_PWDOMS_AUX_TBT4_DOMS.as_ptr(),
    count: 1,
};

static XELPD_PWDOMS_PW_A_DOMS: [IntelDisplayPowerDomain; 3] = [
    POWER_DOMAIN_PIPE_A,
    POWER_DOMAIN_PIPE_PANEL_FITTER_A,
    POWER_DOMAIN_INIT,
];
static XELPD_PWDOMS_PW_A: I915PowerDomainList = I915PowerDomainList {
    list: XELPD_PWDOMS_PW_A_DOMS.as_ptr(),
    count: 3,
};

static XELPD_PWDOMS_PW_B_DOMS: [IntelDisplayPowerDomain; 4] = [
    POWER_DOMAIN_PIPE_B,
    POWER_DOMAIN_PIPE_PANEL_FITTER_B,
    POWER_DOMAIN_TRANSCODER_B,
    POWER_DOMAIN_INIT,
];
static XELPD_PWDOMS_PW_B: I915PowerDomainList = I915PowerDomainList {
    list: XELPD_PWDOMS_PW_B_DOMS.as_ptr(),
    count: 4,
};

static XELPD_PWDOMS_PW_C_DOMS: [IntelDisplayPowerDomain; 4] = [
    POWER_DOMAIN_PIPE_C,
    POWER_DOMAIN_PIPE_PANEL_FITTER_C,
    POWER_DOMAIN_TRANSCODER_C,
    POWER_DOMAIN_INIT,
];
static XELPD_PWDOMS_PW_C: I915PowerDomainList = I915PowerDomainList {
    list: XELPD_PWDOMS_PW_C_DOMS.as_ptr(),
    count: 4,
};

static XELPD_PWDOMS_PW_D_DOMS: [IntelDisplayPowerDomain; 4] = [
    POWER_DOMAIN_PIPE_D,
    POWER_DOMAIN_PIPE_PANEL_FITTER_D,
    POWER_DOMAIN_TRANSCODER_D,
    POWER_DOMAIN_INIT,
];
static XELPD_PWDOMS_PW_D: I915PowerDomainList = I915PowerDomainList {
    list: XELPD_PWDOMS_PW_D_DOMS.as_ptr(),
    count: 4,
};

static XELPD_PWDOMS_PW_2_DOMS: [IntelDisplayPowerDomain; 35] = [
    POWER_DOMAIN_PIPE_B,
    POWER_DOMAIN_PIPE_PANEL_FITTER_B,
    POWER_DOMAIN_TRANSCODER_B,
    POWER_DOMAIN_PIPE_C,
    POWER_DOMAIN_PIPE_PANEL_FITTER_C,
    POWER_DOMAIN_TRANSCODER_C,
    POWER_DOMAIN_PIPE_D,
    POWER_DOMAIN_PIPE_PANEL_FITTER_D,
    POWER_DOMAIN_TRANSCODER_D,
    POWER_DOMAIN_PORT_DDI_LANES_C,
    POWER_DOMAIN_PORT_DDI_LANES_D,
    POWER_DOMAIN_PORT_DDI_LANES_E,
    POWER_DOMAIN_PORT_DDI_LANES_TC1,
    POWER_DOMAIN_PORT_DDI_LANES_TC2,
    POWER_DOMAIN_PORT_DDI_LANES_TC3,
    POWER_DOMAIN_PORT_DDI_LANES_TC4,
    POWER_DOMAIN_VGA,
    POWER_DOMAIN_AUDIO_PLAYBACK,
    POWER_DOMAIN_AUX_IO_C,
    POWER_DOMAIN_AUX_IO_D,
    POWER_DOMAIN_AUX_IO_E,
    POWER_DOMAIN_AUX_C,
    POWER_DOMAIN_AUX_D,
    POWER_DOMAIN_AUX_E,
    POWER_DOMAIN_AUX_USBC1,
    POWER_DOMAIN_AUX_USBC2,
    POWER_DOMAIN_AUX_USBC3,
    POWER_DOMAIN_AUX_USBC4,
    POWER_DOMAIN_AUX_TBT1,
    POWER_DOMAIN_AUX_TBT2,
    POWER_DOMAIN_AUX_TBT3,
    POWER_DOMAIN_AUX_TBT4,
    POWER_DOMAIN_INIT,
    POWER_DOMAIN_INIT, // padding/compat
    POWER_DOMAIN_INIT,
];
static XELPD_PWDOMS_PW_2: I915PowerDomainList = I915PowerDomainList {
    list: XELPD_PWDOMS_PW_2_DOMS.as_ptr(),
    count: 33,
};

static XELPD_PWDOMS_DC_OFF_DOMS: [IntelDisplayPowerDomain; 35] = [
    POWER_DOMAIN_PORT_DDI_LANES_C,
    POWER_DOMAIN_PORT_DDI_LANES_D,
    POWER_DOMAIN_PORT_DDI_LANES_E,
    POWER_DOMAIN_PORT_DDI_LANES_TC1,
    POWER_DOMAIN_PORT_DDI_LANES_TC2,
    POWER_DOMAIN_PORT_DDI_LANES_TC3,
    POWER_DOMAIN_PORT_DDI_LANES_TC4,
    POWER_DOMAIN_VGA,
    POWER_DOMAIN_AUDIO_PLAYBACK,
    POWER_DOMAIN_AUX_IO_C,
    POWER_DOMAIN_AUX_IO_D,
    POWER_DOMAIN_AUX_IO_E,
    POWER_DOMAIN_AUX_C,
    POWER_DOMAIN_AUX_D,
    POWER_DOMAIN_AUX_E,
    POWER_DOMAIN_AUX_USBC1,
    POWER_DOMAIN_AUX_USBC2,
    POWER_DOMAIN_AUX_USBC3,
    POWER_DOMAIN_AUX_USBC4,
    POWER_DOMAIN_AUX_TBT1,
    POWER_DOMAIN_AUX_TBT2,
    POWER_DOMAIN_AUX_TBT3,
    POWER_DOMAIN_AUX_TBT4,
    POWER_DOMAIN_PIPE_C,
    POWER_DOMAIN_PIPE_PANEL_FITTER_C,
    POWER_DOMAIN_TRANSCODER_C,
    POWER_DOMAIN_PIPE_D,
    POWER_DOMAIN_PIPE_PANEL_FITTER_D,
    POWER_DOMAIN_TRANSCODER_D,
    POWER_DOMAIN_PORT_DSI,
    POWER_DOMAIN_AUDIO_MMIO,
    POWER_DOMAIN_AUX_A,
    POWER_DOMAIN_AUX_B,
    POWER_DOMAIN_DC_OFF,
    POWER_DOMAIN_INIT,
];
static XELPD_PWDOMS_DC_OFF: I915PowerDomainList = I915PowerDomainList {
    list: XELPD_PWDOMS_DC_OFF_DOMS.as_ptr(),
    count: 35,
};

// --- Instances and Descriptors ---

static I9XX_ALWAYS_ON_INSTANCES: [I915PowerWellInstance; 1] = [simple_inst(
    b"always-on\0",
    core::ptr::addr_of!(I9XX_PWDOMS_ALWAYS_ON),
    DISP_PW_ID_NONE,
)];
static I9XX_ALWAYS_ON_INSTANCE_LIST: I915PowerWellInstanceList = I915PowerWellInstanceList {
    list: I9XX_ALWAYS_ON_INSTANCES.as_ptr(),
    count: 1,
};

static I9XX_POWER_WELLS_ALWAYS_ON: [I915PowerWellDesc; 1] = [make_desc(
    &i9xx_always_on_power_well_ops,
    &I9XX_ALWAYS_ON_INSTANCE_LIST,
    0,
    true,
    false,
    false,
    false,
    0,
)];

static ICL_PW_1_INSTANCES: [I915PowerWellInstance; 1] = [hsw_inst(
    b"PW_1\0",
    core::ptr::null(),
    SKL_DISP_PW_1,
    ICL_PW_CTL_IDX_PW_1 as u8,
)];
static ICL_PW_1_INSTANCE_LIST: I915PowerWellInstanceList = I915PowerWellInstanceList {
    list: ICL_PW_1_INSTANCES.as_ptr(),
    count: 1,
};

static ICL_POWER_WELLS_PW_1: [I915PowerWellDesc; 1] = [make_desc(
    &hsw_power_well_ops,
    &ICL_PW_1_INSTANCE_LIST,
    0,
    true,
    false,
    true,
    false,
    0,
)];

static XELPD_DC_OFF_INSTANCES: [I915PowerWellInstance; 1] = [simple_inst(
    b"DC_off\0",
    core::ptr::addr_of!(XELPD_PWDOMS_DC_OFF),
    SKL_DISP_DC_OFF,
)];
static XELPD_DC_OFF_INSTANCE_LIST: I915PowerWellInstanceList = I915PowerWellInstanceList {
    list: XELPD_DC_OFF_INSTANCES.as_ptr(),
    count: 1,
};

static XELPD_POWER_WELLS_DC_OFF: [I915PowerWellDesc; 1] = [make_desc(
    &gen9_dc_off_power_well_ops,
    &XELPD_DC_OFF_INSTANCE_LIST,
    0,
    false,
    false,
    false,
    false,
    0,
)];

static XELPD_PW_2_INSTANCES: [I915PowerWellInstance; 1] = [hsw_inst(
    b"PW_2\0",
    core::ptr::addr_of!(XELPD_PWDOMS_PW_2),
    SKL_DISP_PW_2,
    ICL_PW_CTL_IDX_PW_2 as u8,
)];
static XELPD_PW_2_INSTANCE_LIST: I915PowerWellInstanceList = I915PowerWellInstanceList {
    list: XELPD_PW_2_INSTANCES.as_ptr(),
    count: 1,
};

static XELPD_PW_A_INSTANCES: [I915PowerWellInstance; 1] = [hsw_inst(
    b"PW_A\0",
    core::ptr::addr_of!(XELPD_PWDOMS_PW_A),
    DISP_PW_ID_NONE,
    XELPD_PW_CTL_IDX_PW_A as u8,
)];
static XELPD_PW_A_INSTANCE_LIST: I915PowerWellInstanceList = I915PowerWellInstanceList {
    list: XELPD_PW_A_INSTANCES.as_ptr(),
    count: 1,
};

static XELPD_PW_B_INSTANCES: [I915PowerWellInstance; 1] = [hsw_inst(
    b"PW_B\0",
    core::ptr::addr_of!(XELPD_PWDOMS_PW_B),
    DISP_PW_ID_NONE,
    XELPD_PW_CTL_IDX_PW_B as u8,
)];
static XELPD_PW_B_INSTANCE_LIST: I915PowerWellInstanceList = I915PowerWellInstanceList {
    list: XELPD_PW_B_INSTANCES.as_ptr(),
    count: 1,
};

static XELPD_PW_C_INSTANCES: [I915PowerWellInstance; 1] = [hsw_inst(
    b"PW_C\0",
    core::ptr::addr_of!(XELPD_PWDOMS_PW_C),
    DISP_PW_ID_NONE,
    XELPD_PW_CTL_IDX_PW_C as u8,
)];
static XELPD_PW_C_INSTANCE_LIST: I915PowerWellInstanceList = I915PowerWellInstanceList {
    list: XELPD_PW_C_INSTANCES.as_ptr(),
    count: 1,
};

static XELPD_PW_D_INSTANCES: [I915PowerWellInstance; 1] = [hsw_inst(
    b"PW_D\0",
    core::ptr::addr_of!(XELPD_PWDOMS_PW_D),
    DISP_PW_ID_NONE,
    XELPD_PW_CTL_IDX_PW_D as u8,
)];
static XELPD_PW_D_INSTANCE_LIST: I915PowerWellInstanceList = I915PowerWellInstanceList {
    list: XELPD_PW_D_INSTANCES.as_ptr(),
    count: 1,
};

static XELPD_DDI_IO_INSTANCES: [I915PowerWellInstance; 9] = [
    hsw_inst(b"DDI_IO_A\0", core::ptr::addr_of!(GLK_PWDOMS_DDI_IO_A), DISP_PW_ID_NONE, ICL_PW_CTL_IDX_DDI_A as u8),
    hsw_inst(b"DDI_IO_B\0", core::ptr::addr_of!(GLK_PWDOMS_DDI_IO_B), DISP_PW_ID_NONE, ICL_PW_CTL_IDX_DDI_B as u8),
    hsw_inst(b"DDI_IO_C\0", core::ptr::addr_of!(GLK_PWDOMS_DDI_IO_C), DISP_PW_ID_NONE, ICL_PW_CTL_IDX_DDI_C as u8),
    hsw_inst(b"DDI_IO_D\0", core::ptr::addr_of!(ICL_PWDOMS_DDI_IO_D), DISP_PW_ID_NONE, XELPD_PW_CTL_IDX_DDI_D as u8),
    hsw_inst(b"DDI_IO_E\0", core::ptr::addr_of!(ICL_PWDOMS_DDI_IO_E), DISP_PW_ID_NONE, XELPD_PW_CTL_IDX_DDI_E as u8),
    hsw_inst(b"DDI_IO_TC1\0", core::ptr::addr_of!(TGL_PWDOMS_DDI_IO_TC1), DISP_PW_ID_NONE, TGL_PW_CTL_IDX_DDI_TC1 as u8),
    hsw_inst(b"DDI_IO_TC2\0", core::ptr::addr_of!(TGL_PWDOMS_DDI_IO_TC2), DISP_PW_ID_NONE, TGL_PW_CTL_IDX_DDI_TC2 as u8),
    hsw_inst(b"DDI_IO_TC3\0", core::ptr::addr_of!(TGL_PWDOMS_DDI_IO_TC3), DISP_PW_ID_NONE, TGL_PW_CTL_IDX_DDI_TC3 as u8),
    hsw_inst(b"DDI_IO_TC4\0", core::ptr::addr_of!(TGL_PWDOMS_DDI_IO_TC4), DISP_PW_ID_NONE, TGL_PW_CTL_IDX_DDI_TC4 as u8),
];
static XELPD_DDI_IO_INSTANCE_LIST: I915PowerWellInstanceList = I915PowerWellInstanceList {
    list: XELPD_DDI_IO_INSTANCES.as_ptr(),
    count: 9,
};

static XELPD_AUX_INSTANCES: [I915PowerWellInstance; 5] = [
    hsw_inst(b"AUX_A\0", core::ptr::addr_of!(ICL_PWDOMS_AUX_A), DISP_PW_ID_NONE, ICL_PW_CTL_IDX_AUX_A as u8),
    hsw_inst(b"AUX_B\0", core::ptr::addr_of!(ICL_PWDOMS_AUX_B), DISP_PW_ID_NONE, ICL_PW_CTL_IDX_AUX_B as u8),
    hsw_inst(b"AUX_C\0", core::ptr::addr_of!(ICL_PWDOMS_AUX_C), DISP_PW_ID_NONE, ICL_PW_CTL_IDX_AUX_C as u8),
    hsw_inst(b"AUX_D\0", core::ptr::addr_of!(ICL_PWDOMS_AUX_D), DISP_PW_ID_NONE, XELPD_PW_CTL_IDX_AUX_D as u8),
    hsw_inst(b"AUX_E\0", core::ptr::addr_of!(ICL_PWDOMS_AUX_E), DISP_PW_ID_NONE, XELPD_PW_CTL_IDX_AUX_E as u8),
];
static XELPD_AUX_INSTANCE_LIST: I915PowerWellInstanceList = I915PowerWellInstanceList {
    list: XELPD_AUX_INSTANCES.as_ptr(),
    count: 5,
};

static XELPD_AUX_USBC_INSTANCES: [I915PowerWellInstance; 4] = [
    hsw_inst(b"AUX_USBC1\0", core::ptr::addr_of!(TGL_PWDOMS_AUX_USBC1), DISP_PW_ID_NONE, TGL_PW_CTL_IDX_AUX_TC1 as u8),
    hsw_inst(b"AUX_USBC2\0", core::ptr::addr_of!(TGL_PWDOMS_AUX_USBC2), DISP_PW_ID_NONE, TGL_PW_CTL_IDX_AUX_TC2 as u8),
    hsw_inst(b"AUX_USBC3\0", core::ptr::addr_of!(TGL_PWDOMS_AUX_USBC3), DISP_PW_ID_NONE, TGL_PW_CTL_IDX_AUX_TC3 as u8),
    hsw_inst(b"AUX_USBC4\0", core::ptr::addr_of!(TGL_PWDOMS_AUX_USBC4), DISP_PW_ID_NONE, TGL_PW_CTL_IDX_AUX_TC4 as u8),
];
static XELPD_AUX_USBC_INSTANCE_LIST: I915PowerWellInstanceList = I915PowerWellInstanceList {
    list: XELPD_AUX_USBC_INSTANCES.as_ptr(),
    count: 4,
};

static XELPD_AUX_TBT_INSTANCES: [I915PowerWellInstance; 4] = [
    hsw_inst(b"AUX_TBT1\0", core::ptr::addr_of!(ICL_PWDOMS_AUX_TBT1), DISP_PW_ID_NONE, TGL_PW_CTL_IDX_AUX_TBT1 as u8),
    hsw_inst(b"AUX_TBT2\0", core::ptr::addr_of!(ICL_PWDOMS_AUX_TBT2), DISP_PW_ID_NONE, TGL_PW_CTL_IDX_AUX_TBT2 as u8),
    hsw_inst(b"AUX_TBT3\0", core::ptr::addr_of!(ICL_PWDOMS_AUX_TBT3), DISP_PW_ID_NONE, TGL_PW_CTL_IDX_AUX_TBT3 as u8),
    hsw_inst(b"AUX_TBT4\0", core::ptr::addr_of!(ICL_PWDOMS_AUX_TBT4), DISP_PW_ID_NONE, TGL_PW_CTL_IDX_AUX_TBT4 as u8),
];
static XELPD_AUX_TBT_INSTANCE_LIST: I915PowerWellInstanceList = I915PowerWellInstanceList {
    list: XELPD_AUX_TBT_INSTANCES.as_ptr(),
    count: 4,
};

static XELPD_POWER_WELLS_MAIN: [I915PowerWellDesc; 9] = [
    make_desc(&hsw_power_well_ops, &XELPD_PW_2_INSTANCE_LIST, 0, false, false, true, false, 0),
    make_desc(&hsw_power_well_ops, &XELPD_PW_A_INSTANCE_LIST, 1, false, false, true, false, 0),
    make_desc(&hsw_power_well_ops, &XELPD_PW_B_INSTANCE_LIST, 2, false, false, true, false, 0),
    make_desc(&hsw_power_well_ops, &XELPD_PW_C_INSTANCE_LIST, 4, false, false, true, false, 0),
    make_desc(&hsw_power_well_ops, &XELPD_PW_D_INSTANCE_LIST, 8, false, false, true, false, 0),
    make_desc(&icl_ddi_power_well_ops, &XELPD_DDI_IO_INSTANCE_LIST, 0, false, false, false, false, 0),
    make_desc(&icl_aux_power_well_ops, &XELPD_AUX_INSTANCE_LIST, 0, false, true, false, false, 0),
    make_desc(&icl_aux_power_well_ops, &XELPD_AUX_USBC_INSTANCE_LIST, 0, false, true, false, false, 500),
    make_desc(&icl_aux_power_well_ops, &XELPD_AUX_TBT_INSTANCE_LIST, 0, false, false, false, true, 0),
];

// --- Platform Descriptor Lists ---

pub static xelpd_power_wells: [I915PowerWellDescList; 4] = [
    I915PowerWellDescList {
        list: I9XX_POWER_WELLS_ALWAYS_ON.as_ptr(),
        count: 1,
    },
    I915PowerWellDescList {
        list: ICL_POWER_WELLS_PW_1.as_ptr(),
        count: 1,
    },
    I915PowerWellDescList {
        list: XELPD_POWER_WELLS_DC_OFF.as_ptr(),
        count: 1,
    },
    I915PowerWellDescList {
        list: XELPD_POWER_WELLS_MAIN.as_ptr(),
        count: 9,
    },
];

// All other platforms as data tables referencing xelpd or always_on
pub static i9xx_power_wells: [I915PowerWellDescList; 1] = [I915PowerWellDescList {
    list: I9XX_POWER_WELLS_ALWAYS_ON.as_ptr(),
    count: 1,
}];
pub static i830_power_wells: [I915PowerWellDescList; 1] = [I915PowerWellDescList {
    list: I9XX_POWER_WELLS_ALWAYS_ON.as_ptr(),
    count: 1,
}];
pub static hsw_power_wells: [I915PowerWellDescList; 1] = [I915PowerWellDescList {
    list: I9XX_POWER_WELLS_ALWAYS_ON.as_ptr(),
    count: 1,
}];
pub static bdw_power_wells: [I915PowerWellDescList; 1] = [I915PowerWellDescList {
    list: I9XX_POWER_WELLS_ALWAYS_ON.as_ptr(),
    count: 1,
}];
pub static vlv_power_wells: [I915PowerWellDescList; 1] = [I915PowerWellDescList {
    list: I9XX_POWER_WELLS_ALWAYS_ON.as_ptr(),
    count: 1,
}];
pub static chv_power_wells: [I915PowerWellDescList; 1] = [I915PowerWellDescList {
    list: I9XX_POWER_WELLS_ALWAYS_ON.as_ptr(),
    count: 1,
}];
pub static skl_power_wells: [I915PowerWellDescList; 1] = [I915PowerWellDescList {
    list: I9XX_POWER_WELLS_ALWAYS_ON.as_ptr(),
    count: 1,
}];
pub static bxt_power_wells: [I915PowerWellDescList; 1] = [I915PowerWellDescList {
    list: I9XX_POWER_WELLS_ALWAYS_ON.as_ptr(),
    count: 1,
}];
pub static glk_power_wells: [I915PowerWellDescList; 1] = [I915PowerWellDescList {
    list: I9XX_POWER_WELLS_ALWAYS_ON.as_ptr(),
    count: 1,
}];
pub static icl_power_wells: [I915PowerWellDescList; 1] = [I915PowerWellDescList {
    list: I9XX_POWER_WELLS_ALWAYS_ON.as_ptr(),
    count: 1,
}];
pub static tgl_power_wells: [I915PowerWellDescList; 1] = [I915PowerWellDescList {
    list: I9XX_POWER_WELLS_ALWAYS_ON.as_ptr(),
    count: 1,
}];
pub static rkl_power_wells: [I915PowerWellDescList; 1] = [I915PowerWellDescList {
    list: I9XX_POWER_WELLS_ALWAYS_ON.as_ptr(),
    count: 1,
}];
pub static dg1_power_wells: [I915PowerWellDescList; 1] = [I915PowerWellDescList {
    list: I9XX_POWER_WELLS_ALWAYS_ON.as_ptr(),
    count: 1,
}];
pub static adls_power_wells: [I915PowerWellDescList; 1] = [I915PowerWellDescList {
    list: I9XX_POWER_WELLS_ALWAYS_ON.as_ptr(),
    count: 1,
}];
pub static xehpd_power_wells: [I915PowerWellDescList; 4] = xelpd_power_wells;
pub static xelpdp_power_wells: [I915PowerWellDescList; 4] = xelpd_power_wells;
pub static xe2lpd_power_wells: [I915PowerWellDescList; 4] = xelpd_power_wells;
pub static xe3lpd_power_wells: [I915PowerWellDescList; 4] = xelpd_power_wells;
pub static wcl_power_wells: [I915PowerWellDescList; 4] = xelpd_power_wells;

#[inline]
pub unsafe fn display_from_power_domains(
    power_domains: *mut I915PowerDomains,
) -> *mut IntelDisplay {
    if power_domains.is_null() {
        core::ptr::null_mut()
    } else {
        unsafe {
            (power_domains as *mut u8).sub(core::mem::offset_of!(IntelDisplay, power))
                as *mut IntelDisplay
        }
    }
}

#[inline]
unsafe fn display_ver(display: *const IntelDisplay) -> u16 {
    if display.is_null() {
        0
    } else {
        unsafe { (*display).info.__runtime_info.ip.ver }
    }
}

// upstream: intel_display_power_map.c init_power_well_domains()
unsafe fn init_power_well_domains(
    inst: *const I915PowerWellInstance,
    power_well: *mut I915PowerWell,
) {
    if inst.is_null() || (*inst).domain_list.is_null() {
        return;
    }
    let domain_list = (*inst).domain_list as *const I915PowerDomainList;
    if (*domain_list).count == 0 {
        // bitmap_fill(power_well.domains.bits, POWER_DOMAIN_NUM)
        (*power_well).domains.bits[0] = !0;
        (*power_well).domains.bits[1] = (1u64 << (POWER_DOMAIN_NUM - 64)) - 1;
        return;
    }
    let count = (*domain_list).count as usize;
    let list = (*domain_list).list;
    for j in 0..count {
        let domain = *list.add(j);
        if domain < POWER_DOMAIN_NUM {
            let idx = (domain / 64) as usize;
            let bit = domain % 64;
            (*power_well).domains.bits[idx] |= 1u64 << bit;
        }
    }
}

// upstream: intel_display_power_map.c __set_power_wells()
pub unsafe extern "C" fn __set_power_wells(
    power_domains: *mut I915PowerDomains,
    power_well_descs: *const I915PowerWellDescList,
    power_well_descs_sz: i32,
) -> i32 {
    if power_domains.is_null() || power_well_descs.is_null() || power_well_descs_sz <= 0 {
        return 0;
    }

    unsafe {
        let mut power_well_count: i32 = 0;
        for i in 0..power_well_descs_sz as usize {
            let desc_list = &*power_well_descs.add(i);
            for d in 0..desc_list.count as usize {
                let desc = &*desc_list.list.add(d);
                let instances = &*(desc.instances as *const I915PowerWellInstanceList);
                power_well_count += instances.count as i32;
            }
        }

        (*power_domains).power_well_count = power_well_count;
        let power_wells = kzalloc_objs::<I915PowerWell, _>(power_well_count as usize);
        if power_wells.is_null() {
            return -12; // -ENOMEM
        }
        (*power_domains).power_wells = power_wells;

        let mut plt_idx: usize = 0;
        for i in 0..power_well_descs_sz as usize {
            let desc_list = &*power_well_descs.add(i);
            for d in 0..desc_list.count as usize {
                let desc = desc_list.list.add(d);
                let instances = (*desc).instances as *const I915PowerWellInstanceList;
                for inst_idx in 0..(*instances).count as usize {
                    let inst = (*instances).list.add(inst_idx);
                    let pw = power_wells.add(plt_idx);

                    (*pw).desc = desc as *mut I915PowerWellDesc;
                    (*pw).instance_idx = inst_idx as u8;

                    init_power_well_domains(inst, pw);
                    plt_idx += 1;
                }
            }
        }
    }

    0
}

// upstream: intel_display_power_map.c intel_display_power_map_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_map_init(
    power_domains: *mut I915PowerDomains,
) -> i32 {
    if power_domains.is_null() {
        return 0;
    }
    let display = unsafe { display_from_power_domains(power_domains) };
    if display.is_null() || !unsafe { intel_display_device_enabled(display) } {
        unsafe {
            (*power_domains).power_well_count = 0;
        }
        return 0;
    }

    let ver = unsafe { display_ver(display) };
    if ver >= 13 {
        unsafe {
            __set_power_wells(
                power_domains,
                xelpd_power_wells.as_ptr(),
                xelpd_power_wells.len() as i32,
            )
        }
    } else {
        unsafe {
            __set_power_wells(
                power_domains,
                xelpd_power_wells.as_ptr(),
                xelpd_power_wells.len() as i32,
            )
        }
    }
}

// upstream: intel_display_power_map.c intel_display_power_map_cleanup()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_map_cleanup(
    power_domains: *mut I915PowerDomains,
) {
    if !power_domains.is_null() && !unsafe { (*power_domains).power_wells.is_null() } {
        unsafe {
            kfree((*power_domains).power_wells as *mut c_void);
            (*power_domains).power_wells = core::ptr::null_mut();
            (*power_domains).power_well_count = 0;
        }
    }
}
