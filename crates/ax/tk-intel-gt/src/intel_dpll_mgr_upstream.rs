// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// Source: drivers/gpu/drm/i915/display/intel_dpll_mgr.c (Linux v7.2.3).
#![allow(non_snake_case, dead_code, unsafe_code, unsafe_op_in_unsafe_fn)]

use core::{ffi::c_void, mem, ptr};

use tk_drm_upstream::drm_print_types::DrmPrinter;

use crate::{
    intel_de_upstream::{intel_de_posting_read, intel_de_read, intel_de_rmw, intel_de_write},
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_device_upstream::{STEP_A0, STEP_B0},
    intel_display_limits_types_upstream::I915_MAX_PIPES,
    intel_display_power_types_upstream::POWER_DOMAIN_DISPLAY_CORE,
    intel_display_reg_defs_upstream::INVALID_MMIO_REG,
    intel_display_regs_upstream::*,
    intel_display_types_upstream::{
        IntelAtomicState, IntelCrtc, IntelCrtcState, IntelDigitalPort, IntelDpMstEncoder,
        IntelEncoder,
    },
    intel_display_wa_upstream::{DISPLAY_VER, IS_DISPLAY_STEP, platform_alderlake_p},
    intel_dkl_phy_regs_upstream::*,
    intel_dpll_mgr_types_upstream::{
        DPLL_ID_ICL_DPLL0, DPLL_ID_ICL_DPLL1, DPLL_ID_ICL_MGPLL1, DPLL_ID_ICL_TBTPLL,
        DPLL_ID_PRIVATE, DpllInfo, ICL_PORT_DPLL_COUNT, ICL_PORT_DPLL_DEFAULT,
        ICL_PORT_DPLL_MG_PHY, IclDpllHwState, IntelDpll, IntelDpllHwState, IntelDpllId,
        IntelDpllState,
    },
    intel_mg_phy_regs_upstream::*,
    intel_workarounds_types_upstream::I915RegT,
    linux_i915_private::DrmI915Private,
};

#[repr(C)]
struct IntelDpllFuncs {
    enable: unsafe extern "C" fn(*mut IntelDisplay, *mut IntelDpll, *const IntelDpllHwState),
    disable: unsafe extern "C" fn(*mut IntelDisplay, *mut IntelDpll),
    get_hw_state:
        unsafe extern "C" fn(*mut IntelDisplay, *mut IntelDpll, *mut IntelDpllHwState) -> bool,
    get_freq:
        unsafe extern "C" fn(*mut IntelDisplay, *const IntelDpll, *const IntelDpllHwState) -> i32,
}

#[repr(C)]
struct IntelDpllMgr {
    dpll_info: *const DpllInfo,
    compute_dplls:
        unsafe extern "C" fn(*mut IntelAtomicState, *mut IntelCrtc, *mut IntelEncoder) -> i32,
    get_dplls:
        unsafe extern "C" fn(*mut IntelAtomicState, *mut IntelCrtc, *mut IntelEncoder) -> i32,
    put_dplls: unsafe extern "C" fn(*mut IntelAtomicState, *mut IntelCrtc),
    update_active_dpll:
        unsafe extern "C" fn(*mut IntelAtomicState, *mut IntelCrtc, *mut IntelEncoder),
    update_ref_clks: unsafe extern "C" fn(*mut IntelDisplay),
    dump_hw_state: unsafe extern "C" fn(*mut DrmPrinter, *const IntelDpllHwState),
    compare_hw_state:
        unsafe extern "C" fn(*const IntelDpllHwState, *const IntelDpllHwState) -> bool,
}

#[derive(Clone, Copy, Default)]
struct SklWrpllParams {
    dco_integer: u32,
    dco_fraction: u32,
    pdiv: u32,
    kdiv: u32,
    qdiv_mode: u32,
    qdiv_ratio: u32,
}

struct IclComboPllParams {
    clock: i32,
    wrpll: SklWrpllParams,
}

const DKL_PLL_BIAS_FRAC_EN_H: u32 = 1 << 30;
const MG_PLL_DIV0_FRACNEN_H: u32 = 1 << 30;
const MG_PLL_DIV1_FBPREDIV_SHIFT: u32 = 0;

// These immutable tables contain C-layout pointer fields matching i915's
// static descriptor tables; they are never mutated after initialization.
struct SyncDpllInfos([DpllInfo; 8]);
unsafe impl Sync for SyncDpllInfos {}

struct SyncDpllManager(IntelDpllMgr);
unsafe impl Sync for SyncDpllManager {}

static COMBO_PLL_FUNCS: IntelDpllFuncs = IntelDpllFuncs {
    enable: combo_pll_enable,
    disable: combo_pll_disable,
    get_hw_state: combo_pll_get_hw_state,
    get_freq: icl_ddi_combo_pll_get_freq,
};

static TBT_PLL_FUNCS: IntelDpllFuncs = IntelDpllFuncs {
    enable: icl_tbt_pll_enable,
    disable: icl_tbt_pll_disable,
    get_hw_state: icl_tbt_pll_get_hw_state,
    get_freq: icl_ddi_tbt_pll_get_freq,
};

static MG_PLL_FUNCS: IntelDpllFuncs = IntelDpllFuncs {
    enable: mg_pll_enable,
    disable: mg_pll_disable,
    get_hw_state: dkl_pll_get_hw_state,
    get_freq: icl_ddi_mg_pll_get_freq,
};

static ADLP_PLLS: SyncDpllInfos = SyncDpllInfos([
    DpllInfo {
        name: c"DPLL 0".as_ptr() as *mut i8,
        funcs: ptr::addr_of!(COMBO_PLL_FUNCS) as *mut c_void,
        id: DPLL_ID_ICL_DPLL0,
        power_domain: POWER_DOMAIN_DISPLAY_CORE,
        always_on: false,
        is_alt_port_dpll: false,
        _pad_26: [0; 6],
    },
    DpllInfo {
        name: c"DPLL 1".as_ptr() as *mut i8,
        funcs: ptr::addr_of!(COMBO_PLL_FUNCS) as *mut c_void,
        id: DPLL_ID_ICL_DPLL1,
        power_domain: POWER_DOMAIN_DISPLAY_CORE,
        always_on: false,
        is_alt_port_dpll: false,
        _pad_26: [0; 6],
    },
    DpllInfo {
        name: c"TBT PLL".as_ptr() as *mut i8,
        funcs: ptr::addr_of!(TBT_PLL_FUNCS) as *mut c_void,
        id: DPLL_ID_ICL_TBTPLL,
        power_domain: POWER_DOMAIN_DISPLAY_CORE,
        always_on: false,
        is_alt_port_dpll: true,
        _pad_26: [0; 6],
    },
    DpllInfo {
        name: c"TC PLL 1".as_ptr() as *mut i8,
        funcs: ptr::addr_of!(MG_PLL_FUNCS) as *mut c_void,
        id: DPLL_ID_ICL_MGPLL1,
        power_domain: POWER_DOMAIN_DISPLAY_CORE,
        always_on: false,
        is_alt_port_dpll: false,
        _pad_26: [0; 6],
    },
    DpllInfo {
        name: c"TC PLL 2".as_ptr() as *mut i8,
        funcs: ptr::addr_of!(MG_PLL_FUNCS) as *mut c_void,
        id: DPLL_ID_ICL_MGPLL1 + 1,
        power_domain: POWER_DOMAIN_DISPLAY_CORE,
        always_on: false,
        is_alt_port_dpll: false,
        _pad_26: [0; 6],
    },
    DpllInfo {
        name: c"TC PLL 3".as_ptr() as *mut i8,
        funcs: ptr::addr_of!(MG_PLL_FUNCS) as *mut c_void,
        id: DPLL_ID_ICL_MGPLL1 + 2,
        power_domain: POWER_DOMAIN_DISPLAY_CORE,
        always_on: false,
        is_alt_port_dpll: false,
        _pad_26: [0; 6],
    },
    DpllInfo {
        name: c"TC PLL 4".as_ptr() as *mut i8,
        funcs: ptr::addr_of!(MG_PLL_FUNCS) as *mut c_void,
        id: DPLL_ID_ICL_MGPLL1 + 3,
        power_domain: POWER_DOMAIN_DISPLAY_CORE,
        always_on: false,
        is_alt_port_dpll: false,
        _pad_26: [0; 6],
    },
    DpllInfo {
        name: ptr::null_mut(),
        funcs: ptr::null_mut(),
        id: DPLL_ID_PRIVATE,
        power_domain: POWER_DOMAIN_DISPLAY_CORE,
        always_on: false,
        is_alt_port_dpll: false,
        _pad_26: [0; 6],
    },
]);

static ADLP_PLL_MGR: SyncDpllManager = SyncDpllManager(IntelDpllMgr {
    dpll_info: ptr::addr_of!(ADLP_PLLS.0).cast::<DpllInfo>(),
    compute_dplls: icl_compute_dplls,
    get_dplls: icl_get_dplls,
    put_dplls: icl_put_dplls,
    update_active_dpll: icl_update_active_dpll,
    update_ref_clks: icl_update_dpll_ref_clks,
    dump_hw_state: icl_dump_hw_state,
    compare_hw_state: icl_compare_hw_state,
});

unsafe fn display_from_atomic(state: *mut IntelAtomicState) -> *mut IntelDisplay {
    if state.is_null() || (*state).base.dev.is_null() || (*(*state).base.dev).dev_private.is_null()
    {
        return ptr::null_mut();
    }
    (*(*(*state).base.dev).dev_private.cast::<DrmI915Private>())
        .display
        .cast::<IntelDisplay>()
}

unsafe fn display_from_crtc(crtc: *mut IntelCrtc) -> *mut IntelDisplay {
    if crtc.is_null() || (*crtc).base.dev.is_null() || (*(*crtc).base.dev).dev_private.is_null() {
        return ptr::null_mut();
    }
    (*(*(*crtc).base.dev).dev_private.cast::<DrmI915Private>())
        .display
        .cast::<IntelDisplay>()
}

unsafe fn display_from_crtc_state(crtc_state: *const IntelCrtcState) -> *mut IntelDisplay {
    if crtc_state.is_null() || (*crtc_state).uapi.crtc.is_null() {
        return ptr::null_mut();
    }
    display_from_crtc((*crtc_state).uapi.crtc.cast())
}

unsafe fn atomic_new_crtc_state(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
) -> *mut IntelCrtcState {
    tk_drm_upstream::drm_atomic_upstream::drm_atomic_get_new_crtc_state(
        ptr::addr_of!((*state).base),
        ptr::addr_of_mut!((*crtc).base),
    )
    .cast()
}

unsafe fn atomic_old_crtc_state(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
) -> *mut IntelCrtcState {
    tk_drm_upstream::drm_atomic_upstream::drm_atomic_get_old_crtc_state(
        ptr::addr_of!((*state).base),
        ptr::addr_of_mut!((*crtc).base),
    )
    .cast()
}

unsafe fn dpll_manager(display: *mut IntelDisplay) -> *const IntelDpllMgr {
    if display.is_null() {
        ptr::null()
    } else {
        (*display).dpll.mgr.cast()
    }
}

unsafe fn atomic_get_dpll_states(state: *mut IntelAtomicState) -> *mut IntelDpllState {
    let display = display_from_atomic(state);
    if display.is_null() {
        return ptr::null_mut();
    }
    if !(*state).dpll_set {
        (*state).dpll_set = true;
        for index in 0..(*display).dpll.num_dpll.max(0) as usize {
            ptr::copy_nonoverlapping(
                ptr::addr_of!((*display).dpll.dplls[index].state),
                ptr::addr_of_mut!((*state).dpll_state[index]),
                1,
            );
        }
    }
    ptr::addr_of_mut!((*state).dpll_state).cast()
}

unsafe fn dpll_funcs(pll: *const IntelDpll) -> *const IntelDpllFuncs {
    if pll.is_null() || (*pll).info.is_null() {
        ptr::null()
    } else {
        (*(*pll).info).funcs.cast()
    }
}

unsafe fn hw_state_icl(state: *const IntelDpllHwState) -> *const IclDpllHwState {
    ptr::addr_of!((*state)._anon_0_0.icl).cast()
}

unsafe fn hw_state_icl_mut(state: *mut IntelDpllHwState) -> *mut IclDpllHwState {
    ptr::addr_of_mut!((*state)._anon_0_0.icl).cast()
}

unsafe extern "C" {
    fn intel_crtc_has_type(crtc_state: *const IntelCrtcState, output_type: u32) -> bool;
    fn intel_get_crtc_new_encoder(
        state: *mut IntelAtomicState,
        crtc_state: *const IntelCrtcState,
    ) -> *mut IntelEncoder;
    fn intel_encoder_is_combo(encoder: *mut IntelEncoder) -> bool;
    fn intel_encoder_is_tc(encoder: *mut IntelEncoder) -> bool;
    fn intel_encoder_to_tc(encoder: *mut IntelEncoder) -> i32;
    fn intel_tc_port_in_dp_alt_mode(port: *mut IntelDigitalPort) -> bool;
    fn intel_tc_port_in_legacy_mode(port: *mut IntelDigitalPort) -> bool;
    fn intel_hti_dpll_mask(display: *mut IntelDisplay) -> u32;
    fn intel_crtc_joined_pipe_mask(crtc_state: *const IntelCrtcState) -> u8;
    fn intel_atomic_get_new_crtc_state(
        state: *mut IntelAtomicState,
        crtc: *mut IntelCrtc,
    ) -> *mut IntelCrtcState;
    fn intel_atomic_get_old_crtc_state(
        state: *mut IntelAtomicState,
        crtc: *mut IntelCrtc,
    ) -> *mut IntelCrtcState;
    fn intel_dkl_phy_read(display: *mut IntelDisplay, reg: IntelDklPhyReg) -> u32;
    fn intel_dkl_phy_write(display: *mut IntelDisplay, reg: IntelDklPhyReg, value: u32);
    fn intel_dkl_phy_rmw(display: *mut IntelDisplay, reg: IntelDklPhyReg, clear: u32, set: u32);
    fn intel_dkl_phy_posting_read(display: *mut IntelDisplay, reg: IntelDklPhyReg);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dpll_init(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    tk_linux_kapi::mutex::mutex_init(ptr::addr_of_mut!((*display).dpll.lock));
    (*display).dpll.num_dpll = 0;
    (*display).dpll.mgr = ptr::null_mut();

    if !platform_alderlake_p(display) {
        return;
    }

    let mut count = 0usize;
    while count < ADLP_PLLS.0.len() && !ADLP_PLLS.0[count].name.is_null() {
        (*display).dpll.dplls[count].info = ptr::addr_of!(ADLP_PLLS.0[count]) as *mut DpllInfo;
        (*display).dpll.dplls[count].index = count as u8;
        count += 1;
    }
    (*display).dpll.num_dpll = count as i32;
    (*display).dpll.mgr = ptr::addr_of!(ADLP_PLL_MGR.0) as *mut c_void;
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dpll_update_ref_clks(display: *mut IntelDisplay) {
    let manager = dpll_manager(display);
    if !manager.is_null() {
        ((*manager).update_ref_clks)(display);
    }
}

unsafe extern "C" fn icl_update_dpll_ref_clks(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    (*display).dpll.ref_clks.nssc = (*display).cdclk.hw.r#ref as i32;
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_get_dpll_by_id(
    display: *mut IntelDisplay,
    id: IntelDpllId,
) -> *mut IntelDpll {
    if display.is_null() {
        return ptr::null_mut();
    }
    for index in 0..(*display).dpll.num_dpll.max(0) as usize {
        let pll = ptr::addr_of_mut!((*display).dpll.dplls[index]);
        if !(*pll).info.is_null() && (*(*pll).info).id == id {
            return pll;
        }
    }
    ptr::null_mut()
}

unsafe fn intel_dpll_mask_all(display: *mut IntelDisplay) -> u32 {
    let mut mask = 0u32;
    if display.is_null() {
        return mask;
    }
    for index in 0..(*display).dpll.num_dpll.max(0) as usize {
        let info = (*display).dpll.dplls[index].info;
        if info.is_null() || (*info).id < 0 || (*info).id >= 32 {
            continue;
        }
        mask |= 1u32 << (*info).id;
    }
    mask
}

unsafe fn dpll_state_for_id(
    display: *mut IntelDisplay,
    dpll_states: *mut IntelDpllState,
    id: IntelDpllId,
) -> (*mut IntelDpll, *mut IntelDpllState) {
    let pll = intel_get_dpll_by_id(display, id);
    if pll.is_null() {
        return (ptr::null_mut(), ptr::null_mut());
    }
    (pll, dpll_states.add((*pll).index as usize))
}

unsafe fn intel_find_dpll(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
    hw_state: *const IntelDpllHwState,
    mask: u32,
) -> *mut IntelDpll {
    let display = display_from_atomic(state);
    if display.is_null() || crtc.is_null() || hw_state.is_null() {
        return ptr::null_mut();
    }
    let all = intel_dpll_mask_all(display);
    if mask & !all != 0 {
        return ptr::null_mut();
    }
    let states = atomic_get_dpll_states(state);
    if states.is_null() {
        return ptr::null_mut();
    }
    let mut unused: *mut IntelDpll = ptr::null_mut();
    for id in 0..32 {
        if mask & (1u32 << id) == 0 {
            continue;
        }
        let (pll, dpll_state) = dpll_state_for_id(display, states, id as i32);
        if pll.is_null() || dpll_state.is_null() {
            continue;
        }
        if (*dpll_state).pipe_mask == 0 {
            if unused.is_null() {
                unused = pll;
            }
            continue;
        }
        if hw_states_equal(
            hw_state,
            ptr::addr_of!((*dpll_state).hw_state),
            mem::size_of::<IntelDpllHwState>(),
        ) {
            return pll;
        }
    }
    unused
}

unsafe fn hw_states_equal(
    a: *const IntelDpllHwState,
    b: *const IntelDpllHwState,
    size: usize,
) -> bool {
    core::slice::from_raw_parts(a.cast::<u8>(), size)
        == core::slice::from_raw_parts(b.cast::<u8>(), size)
}

unsafe fn intel_dpll_crtc_get(
    crtc: *mut IntelCrtc,
    pll: *const IntelDpll,
    dpll_state: *mut IntelDpllState,
) {
    if crtc.is_null()
        || pll.is_null()
        || dpll_state.is_null()
        || (*crtc).pipe < 0
        || (*crtc).pipe >= I915_MAX_PIPES
    {
        return;
    }
    (*dpll_state).pipe_mask |= 1u8 << (*crtc).pipe;
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dpll_crtc_put(
    crtc: *const IntelCrtc,
    _pll: *const IntelDpll,
    dpll_state: *mut IntelDpllState,
) {
    if crtc.is_null() || dpll_state.is_null() || (*crtc).pipe < 0 || (*crtc).pipe >= I915_MAX_PIPES
    {
        return;
    }
    (*dpll_state).pipe_mask &= !(1u8 << (*crtc).pipe);
}

unsafe fn intel_reference_dpll(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
    pll: *mut IntelDpll,
    hw_state: *const IntelDpllHwState,
) -> i32 {
    if pll.is_null() || hw_state.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let states = atomic_get_dpll_states(state);
    if states.is_null() {
        return -tk_linux_kapi::config::ENOMEM;
    }
    let dpll_state = states.add((*pll).index as usize);
    if (*dpll_state).pipe_mask == 0 {
        ptr::copy_nonoverlapping(hw_state, ptr::addr_of_mut!((*dpll_state).hw_state), 1);
    }
    intel_dpll_crtc_get(crtc, pll, dpll_state);
    0
}

unsafe fn intel_unreference_dpll(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
    pll: *mut IntelDpll,
) {
    if pll.is_null() {
        return;
    }
    let states = atomic_get_dpll_states(state);
    if !states.is_null() {
        intel_dpll_crtc_put(crtc, pll, states.add((*pll).index as usize));
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dpll_compute(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
    encoder: *mut IntelEncoder,
) -> i32 {
    let display = display_from_atomic(state);
    let manager = dpll_manager(display);
    if manager.is_null() || encoder.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    ((*manager).compute_dplls)(state, crtc, encoder)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dpll_reserve(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
    encoder: *mut IntelEncoder,
) -> i32 {
    let display = display_from_atomic(state);
    let manager = dpll_manager(display);
    if manager.is_null() || encoder.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    ((*manager).get_dplls)(state, crtc, encoder)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dpll_release(state: *mut IntelAtomicState, crtc: *mut IntelCrtc) {
    let display = display_from_atomic(state);
    let manager = dpll_manager(display);
    if !manager.is_null() {
        ((*manager).put_dplls)(state, crtc);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dpll_update_active(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
    encoder: *mut IntelEncoder,
) {
    let display = display_from_atomic(state);
    let manager = dpll_manager(display);
    if !manager.is_null() && !encoder.is_null() {
        ((*manager).update_active_dpll)(state, crtc, encoder);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dpll_get_freq(
    display: *mut IntelDisplay,
    pll: *const IntelDpll,
    hw_state: *const IntelDpllHwState,
) -> i32 {
    let funcs = dpll_funcs(pll);
    if display.is_null() || funcs.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    ((*funcs).get_freq)(display, pll, hw_state)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dpll_get_hw_state(
    display: *mut IntelDisplay,
    pll: *mut IntelDpll,
    hw_state: *mut IntelDpllHwState,
) -> bool {
    let funcs = dpll_funcs(pll);
    !display.is_null() && !funcs.is_null() && ((*funcs).get_hw_state)(display, pll, hw_state)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn icl_set_active_port_dpll(
    crtc_state: *mut IntelCrtcState,
    port_dpll_id: u32,
) {
    if crtc_state.is_null() || port_dpll_id >= ICL_PORT_DPLL_COUNT {
        return;
    }
    let port_dpll = ptr::addr_of!((*crtc_state).icl_port_dplls[port_dpll_id as usize]);
    (*crtc_state).intel_dpll = (*port_dpll).pll;
    ptr::copy_nonoverlapping(
        ptr::addr_of!((*port_dpll).hw_state),
        ptr::addr_of_mut!((*crtc_state).dpll_hw_state),
        1,
    );
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn icl_tc_port_to_pll_id(tc_port: i32) -> IntelDpllId {
    tc_port - 0 + DPLL_ID_ICL_MGPLL1 // TC_PORT_1 is zero in intel_display.h.
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dpll_is_combophy(id: IntelDpllId) -> bool {
    id == DPLL_ID_ICL_DPLL0
        || id == DPLL_ID_ICL_DPLL1
        || (DPLL_ID_ICL_MGPLL1..=DPLL_ID_ICL_MGPLL1 + 3).contains(&id)
}

unsafe extern "C" fn icl_update_active_dpll(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
    encoder: *mut IntelEncoder,
) {
    let crtc_state = atomic_new_crtc_state(state, crtc);
    if crtc_state.is_null() || encoder.is_null() {
        return;
    }
    let dig_port = if (*encoder).r#type == crate::intel_display_types_upstream::INTEL_OUTPUT_DP_MST
    {
        (*encoder.cast::<IntelDpMstEncoder>()).primary
    } else if matches!(
        (*encoder).r#type,
        crate::intel_display_types_upstream::INTEL_OUTPUT_DDI
            | crate::intel_display_types_upstream::INTEL_OUTPUT_DP
            | crate::intel_display_types_upstream::INTEL_OUTPUT_EDP
            | crate::intel_display_types_upstream::INTEL_OUTPUT_HDMI
    ) {
        encoder.cast::<IntelDigitalPort>()
    } else {
        ptr::null_mut()
    };
    let is_tc = !dig_port.is_null()
        && (intel_tc_port_in_dp_alt_mode(dig_port) || intel_tc_port_in_legacy_mode(dig_port));
    let active = if is_tc {
        ICL_PORT_DPLL_MG_PHY
    } else {
        ICL_PORT_DPLL_DEFAULT
    };
    icl_set_active_port_dpll(crtc_state, active);
}

unsafe extern "C" fn icl_compute_combo_phy_dpll(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
) -> i32 {
    let display = display_from_atomic(state);
    let crtc_state = atomic_new_crtc_state(state, crtc);
    if display.is_null() || crtc_state.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let port_dpll = ptr::addr_of_mut!((*crtc_state).icl_port_dplls[ICL_PORT_DPLL_DEFAULT as usize]);
    let mut params = SklWrpllParams::default();
    let ret = if intel_crtc_has_type(
        crtc_state,
        crate::intel_display_types_upstream::INTEL_OUTPUT_HDMI,
    ) {
        icl_calc_wrpll(crtc_state, &mut params)
    } else {
        icl_calc_dp_combo_pll(display, (*crtc_state).port_clock, &mut params)
    };
    if ret != 0 {
        return ret;
    }
    icl_calc_dpll_state(display, &params, ptr::addr_of_mut!((*port_dpll).hw_state));
    icl_set_active_port_dpll(crtc_state, ICL_PORT_DPLL_DEFAULT);
    (*crtc_state).port_clock =
        icl_ddi_combo_pll_get_freq(display, ptr::null(), ptr::addr_of!((*port_dpll).hw_state));
    0
}

unsafe extern "C" fn icl_compute_tc_phy_dplls(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
    encoder: *mut IntelEncoder,
) -> i32 {
    let display = display_from_atomic(state);
    let crtc_state = atomic_new_crtc_state(state, crtc);
    if display.is_null() || crtc_state.is_null() || encoder.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let mut tbt = SklWrpllParams::default();
    let ret = icl_calc_tbt_pll(display, &mut tbt);
    if ret != 0 {
        return ret;
    }
    let default_dpll =
        ptr::addr_of_mut!((*crtc_state).icl_port_dplls[ICL_PORT_DPLL_DEFAULT as usize]);
    icl_calc_dpll_state(display, &tbt, ptr::addr_of_mut!((*default_dpll).hw_state));

    let mg_dpll = ptr::addr_of_mut!((*crtc_state).icl_port_dplls[ICL_PORT_DPLL_MG_PHY as usize]);
    let ret = icl_calc_mg_pll_state(crtc_state, ptr::addr_of_mut!((*mg_dpll).hw_state));
    if ret != 0 {
        return ret;
    }
    icl_update_active_dpll(state, crtc, encoder);
    (*crtc_state).port_clock =
        icl_ddi_mg_pll_get_freq(display, ptr::null(), ptr::addr_of!((*mg_dpll).hw_state));
    0
}

unsafe extern "C" fn icl_compute_dplls(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
    encoder: *mut IntelEncoder,
) -> i32 {
    if encoder.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    if intel_encoder_is_combo(encoder) {
        icl_compute_combo_phy_dpll(state, crtc)
    } else if intel_encoder_is_tc(encoder) {
        icl_compute_tc_phy_dplls(state, crtc, encoder)
    } else {
        -tk_linux_kapi::config::EINVAL
    }
}

unsafe extern "C" fn icl_get_combo_phy_dpll(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
    encoder: *mut IntelEncoder,
) -> i32 {
    let display = display_from_atomic(state);
    let crtc_state = atomic_new_crtc_state(state, crtc);
    if display.is_null() || crtc_state.is_null() || encoder.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let port_dpll = ptr::addr_of_mut!((*crtc_state).icl_port_dplls[ICL_PORT_DPLL_DEFAULT as usize]);
    let mask =
        ((1u32 << DPLL_ID_ICL_DPLL0) | (1u32 << DPLL_ID_ICL_DPLL1)) & !intel_hti_dpll_mask(display);
    let pll = intel_find_dpll(state, crtc, ptr::addr_of!((*port_dpll).hw_state), mask);
    if pll.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    (*port_dpll).pll = pll;
    let ret = intel_reference_dpll(state, crtc, pll, ptr::addr_of!((*port_dpll).hw_state));
    if ret != 0 {
        (*port_dpll).pll = ptr::null_mut();
        return ret;
    }
    icl_update_active_dpll(state, crtc, encoder);
    0
}

unsafe extern "C" fn icl_get_tc_phy_dplls(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
    encoder: *mut IntelEncoder,
) -> i32 {
    let display = display_from_atomic(state);
    let crtc_state = atomic_new_crtc_state(state, crtc);
    if display.is_null() || crtc_state.is_null() || encoder.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let default_dpll =
        ptr::addr_of_mut!((*crtc_state).icl_port_dplls[ICL_PORT_DPLL_DEFAULT as usize]);
    let tbt = intel_find_dpll(
        state,
        crtc,
        ptr::addr_of!((*default_dpll).hw_state),
        1u32 << DPLL_ID_ICL_TBTPLL,
    );
    if tbt.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    (*default_dpll).pll = tbt;
    let ret = intel_reference_dpll(state, crtc, tbt, ptr::addr_of!((*default_dpll).hw_state));
    if ret != 0 {
        (*default_dpll).pll = ptr::null_mut();
        return ret;
    }

    let tc_port = intel_encoder_to_tc(encoder);
    if !(0..4).contains(&tc_port) {
        intel_unreference_dpll(state, crtc, tbt);
        (*default_dpll).pll = ptr::null_mut();
        return -tk_linux_kapi::config::EINVAL;
    }
    let pll_id = icl_tc_port_to_pll_id(tc_port);
    let mg_dpll = ptr::addr_of_mut!((*crtc_state).icl_port_dplls[ICL_PORT_DPLL_MG_PHY as usize]);
    let mg = intel_find_dpll(
        state,
        crtc,
        ptr::addr_of!((*mg_dpll).hw_state),
        1u32 << pll_id,
    );
    if mg.is_null() {
        intel_unreference_dpll(state, crtc, tbt);
        (*default_dpll).pll = ptr::null_mut();
        return -tk_linux_kapi::config::EINVAL;
    }
    (*mg_dpll).pll = mg;
    let ret = intel_reference_dpll(state, crtc, mg, ptr::addr_of!((*mg_dpll).hw_state));
    if ret != 0 {
        (*mg_dpll).pll = ptr::null_mut();
        intel_unreference_dpll(state, crtc, tbt);
        (*default_dpll).pll = ptr::null_mut();
        return ret;
    }
    icl_update_active_dpll(state, crtc, encoder);
    0
}

unsafe extern "C" fn icl_get_dplls(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
    encoder: *mut IntelEncoder,
) -> i32 {
    if encoder.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    if intel_encoder_is_combo(encoder) {
        icl_get_combo_phy_dpll(state, crtc, encoder)
    } else if intel_encoder_is_tc(encoder) {
        icl_get_tc_phy_dplls(state, crtc, encoder)
    } else {
        -tk_linux_kapi::config::EINVAL
    }
}

unsafe extern "C" fn icl_put_dplls(state: *mut IntelAtomicState, crtc: *mut IntelCrtc) {
    let old = atomic_old_crtc_state(state, crtc);
    let new = atomic_new_crtc_state(state, crtc);
    if old.is_null() || new.is_null() {
        return;
    }
    (*new).intel_dpll = ptr::null_mut();
    for id in [ICL_PORT_DPLL_DEFAULT, ICL_PORT_DPLL_MG_PHY] {
        let old_port = ptr::addr_of!((*old).icl_port_dplls[id as usize]);
        let new_port = ptr::addr_of_mut!((*new).icl_port_dplls[id as usize]);
        (*new_port).pll = ptr::null_mut();
        if !(*old_port).pll.is_null() {
            intel_unreference_dpll(state, crtc, (*old_port).pll);
        }
    }
}

fn icl_wrpll_get_multipliers(bestdiv: i32) -> Option<(i32, i32, i32)> {
    if bestdiv % 2 == 0 {
        if bestdiv == 2 {
            Some((2, 1, 1))
        } else if bestdiv % 4 == 0 {
            Some((2, bestdiv / 4, 2))
        } else if bestdiv % 6 == 0 {
            Some((3, bestdiv / 6, 2))
        } else if bestdiv % 5 == 0 {
            Some((5, bestdiv / 10, 2))
        } else if bestdiv % 14 == 0 {
            Some((7, bestdiv / 14, 2))
        } else {
            None
        }
    } else if matches!(bestdiv, 3 | 5 | 7) {
        Some((bestdiv, 1, 1))
    } else if matches!(bestdiv, 9 | 15 | 21) {
        Some((bestdiv / 3, 1, 3))
    } else {
        None
    }
}

fn icl_wrpll_params_populate(
    params: &mut SklWrpllParams,
    dco_freq: u32,
    ref_freq: u32,
    pdiv: i32,
    qdiv: i32,
    kdiv: i32,
) -> bool {
    let pdiv_bits = match pdiv {
        2 => 1,
        3 => 2,
        5 => 4,
        7 => 8,
        _ => return false,
    };
    let kdiv_bits = match kdiv {
        1 => 1,
        2 => 2,
        3 => 4,
        _ => return false,
    };
    if ref_freq == 0 || (kdiv != 2 && qdiv != 1) {
        return false;
    }
    params.pdiv = pdiv_bits;
    params.kdiv = kdiv_bits;
    params.qdiv_ratio = qdiv as u32;
    params.qdiv_mode = u32::from(qdiv != 1);
    let dco = (((dco_freq as u64) << 15) / ref_freq as u64) as u32;
    params.dco_integer = dco >> 15;
    params.dco_fraction = dco & 0x7fff;
    true
}

unsafe fn icl_wrpll_ref_clock(display: *mut IntelDisplay) -> u32 {
    let reference = (*display).dpll.ref_clks.nssc.max(0) as u32;
    if reference == 38400 { 19200 } else { reference }
}

unsafe fn ehl_combo_pll_div_frac_wa_needed(display: *mut IntelDisplay) -> bool {
    (DISPLAY_VER(display) >= 12) && (*display).dpll.ref_clks.nssc == 38400
}

static ICL_DP_COMBO_PLL_24MHZ: [IclComboPllParams; 8] = [
    IclComboPllParams {
        clock: 540000,
        wrpll: SklWrpllParams {
            dco_integer: 0x151,
            dco_fraction: 0x4000,
            pdiv: 0x2,
            kdiv: 1,
            qdiv_mode: 0,
            qdiv_ratio: 0,
        },
    },
    IclComboPllParams {
        clock: 270000,
        wrpll: SklWrpllParams {
            dco_integer: 0x151,
            dco_fraction: 0x4000,
            pdiv: 0x2,
            kdiv: 2,
            qdiv_mode: 0,
            qdiv_ratio: 0,
        },
    },
    IclComboPllParams {
        clock: 162000,
        wrpll: SklWrpllParams {
            dco_integer: 0x151,
            dco_fraction: 0x4000,
            pdiv: 0x4,
            kdiv: 2,
            qdiv_mode: 0,
            qdiv_ratio: 0,
        },
    },
    IclComboPllParams {
        clock: 324000,
        wrpll: SklWrpllParams {
            dco_integer: 0x151,
            dco_fraction: 0x4000,
            pdiv: 0x4,
            kdiv: 1,
            qdiv_mode: 0,
            qdiv_ratio: 0,
        },
    },
    IclComboPllParams {
        clock: 216000,
        wrpll: SklWrpllParams {
            dco_integer: 0x168,
            dco_fraction: 0,
            pdiv: 0x1,
            kdiv: 2,
            qdiv_mode: 1,
            qdiv_ratio: 2,
        },
    },
    IclComboPllParams {
        clock: 432000,
        wrpll: SklWrpllParams {
            dco_integer: 0x168,
            dco_fraction: 0,
            pdiv: 0x1,
            kdiv: 2,
            qdiv_mode: 0,
            qdiv_ratio: 0,
        },
    },
    IclComboPllParams {
        clock: 648000,
        wrpll: SklWrpllParams {
            dco_integer: 0x195,
            dco_fraction: 0,
            pdiv: 0x2,
            kdiv: 1,
            qdiv_mode: 0,
            qdiv_ratio: 0,
        },
    },
    IclComboPllParams {
        clock: 810000,
        wrpll: SklWrpllParams {
            dco_integer: 0x151,
            dco_fraction: 0x4000,
            pdiv: 0x1,
            kdiv: 1,
            qdiv_mode: 0,
            qdiv_ratio: 0,
        },
    },
];

static ICL_DP_COMBO_PLL_19_2MHZ: [IclComboPllParams; 8] = [
    IclComboPllParams {
        clock: 540000,
        wrpll: SklWrpllParams {
            dco_integer: 0x1a5,
            dco_fraction: 0x7000,
            pdiv: 0x2,
            kdiv: 1,
            qdiv_mode: 0,
            qdiv_ratio: 0,
        },
    },
    IclComboPllParams {
        clock: 270000,
        wrpll: SklWrpllParams {
            dco_integer: 0x1a5,
            dco_fraction: 0x7000,
            pdiv: 0x2,
            kdiv: 2,
            qdiv_mode: 0,
            qdiv_ratio: 0,
        },
    },
    IclComboPllParams {
        clock: 162000,
        wrpll: SklWrpllParams {
            dco_integer: 0x1a5,
            dco_fraction: 0x7000,
            pdiv: 0x4,
            kdiv: 2,
            qdiv_mode: 0,
            qdiv_ratio: 0,
        },
    },
    IclComboPllParams {
        clock: 324000,
        wrpll: SklWrpllParams {
            dco_integer: 0x1a5,
            dco_fraction: 0x7000,
            pdiv: 0x4,
            kdiv: 1,
            qdiv_mode: 0,
            qdiv_ratio: 0,
        },
    },
    IclComboPllParams {
        clock: 216000,
        wrpll: SklWrpllParams {
            dco_integer: 0x1c2,
            dco_fraction: 0,
            pdiv: 0x1,
            kdiv: 2,
            qdiv_mode: 1,
            qdiv_ratio: 2,
        },
    },
    IclComboPllParams {
        clock: 432000,
        wrpll: SklWrpllParams {
            dco_integer: 0x1c2,
            dco_fraction: 0,
            pdiv: 0x1,
            kdiv: 2,
            qdiv_mode: 0,
            qdiv_ratio: 0,
        },
    },
    IclComboPllParams {
        clock: 648000,
        wrpll: SklWrpllParams {
            dco_integer: 0x1fa,
            dco_fraction: 0x2000,
            pdiv: 0x2,
            kdiv: 1,
            qdiv_mode: 0,
            qdiv_ratio: 0,
        },
    },
    IclComboPllParams {
        clock: 810000,
        wrpll: SklWrpllParams {
            dco_integer: 0x1a5,
            dco_fraction: 0x7000,
            pdiv: 0x1,
            kdiv: 1,
            qdiv_mode: 0,
            qdiv_ratio: 0,
        },
    },
];

unsafe fn icl_calc_dp_combo_pll(
    display: *mut IntelDisplay,
    port_clock: i32,
    params: &mut SklWrpllParams,
) -> i32 {
    match icl_dp_combo_pll_params((*display).dpll.ref_clks.nssc, port_clock) {
        Some(found) => {
            *params = found;
            0
        }
        None => -tk_linux_kapi::config::EINVAL,
    }
}

fn icl_dp_combo_pll_params(ref_clock: i32, port_clock: i32) -> Option<SklWrpllParams> {
    let table = if ref_clock == 24000 {
        &ICL_DP_COMBO_PLL_24MHZ
    } else {
        &ICL_DP_COMBO_PLL_19_2MHZ
    };
    table
        .iter()
        .find(|entry| entry.clock == port_clock)
        .map(|entry| entry.wrpll)
}

unsafe fn icl_calc_tbt_pll(display: *mut IntelDisplay, params: &mut SklWrpllParams) -> i32 {
    *params = if (*display).dpll.ref_clks.nssc == 24000 {
        SklWrpllParams {
            dco_integer: 0x43,
            dco_fraction: 0x4000,
            pdiv: 0,
            kdiv: 0,
            qdiv_mode: 0,
            qdiv_ratio: 0,
        }
    } else {
        SklWrpllParams {
            dco_integer: 0x54,
            dco_fraction: 0x3000,
            pdiv: 0,
            kdiv: 0,
            qdiv_mode: 0,
            qdiv_ratio: 0,
        }
    };
    0
}

unsafe fn icl_calc_wrpll(crtc_state: *const IntelCrtcState, params: &mut SklWrpllParams) -> i32 {
    let display = display_from_crtc_state(crtc_state);
    if display.is_null() || (*crtc_state).port_clock <= 0 {
        return -tk_linux_kapi::config::EINVAL;
    }
    let ref_clock = icl_wrpll_ref_clock(display);
    if ref_clock == 0 {
        return -tk_linux_kapi::config::EINVAL;
    }
    const DIVIDERS: [i32; 46] = [
        2, 4, 6, 8, 10, 12, 14, 16, 18, 20, 24, 28, 30, 32, 36, 40, 42, 44, 48, 50, 52, 54, 56, 60,
        64, 66, 68, 70, 72, 76, 78, 80, 84, 88, 90, 92, 96, 98, 100, 102, 3, 5, 7, 9, 15, 21,
    ];
    let afe_clock = (*crtc_state).port_clock as u32 * 5;
    let mut best = None;
    for divider in DIVIDERS {
        let dco = afe_clock as u64 * divider as u64;
        if !(7_998_000..=10_000_000).contains(&dco) {
            continue;
        }
        let center = dco.abs_diff(8_999_000);
        if best.map_or(true, |(_, best_center)| center < best_center) {
            best = Some((divider, center));
        }
    }
    let Some((best_div, _)) = best else {
        return -tk_linux_kapi::config::EINVAL;
    };
    let Some((pdiv, qdiv, kdiv)) = icl_wrpll_get_multipliers(best_div) else {
        return -tk_linux_kapi::config::EINVAL;
    };
    if !icl_wrpll_params_populate(
        params,
        afe_clock.saturating_mul(best_div as u32),
        ref_clock,
        pdiv,
        qdiv,
        kdiv,
    ) {
        return -tk_linux_kapi::config::EINVAL;
    }
    0
}

unsafe fn icl_calc_dpll_state(
    display: *mut IntelDisplay,
    params: &SklWrpllParams,
    hw_state: *mut IntelDpllHwState,
) {
    if display.is_null() || hw_state.is_null() {
        return;
    }
    let icl = hw_state_icl_mut(hw_state);
    let mut fraction = params.dco_fraction;
    if ehl_combo_pll_div_frac_wa_needed(display) {
        fraction = (fraction + 1) / 2;
    }
    (*icl).cfgcr0 = DPLL_CFGCR0_DCO_FRACTION(fraction) | params.dco_integer;
    (*icl).cfgcr1 = DPLL_CFGCR1_QDIV_RATIO(params.qdiv_ratio)
        | DPLL_CFGCR1_QDIV_MODE(params.qdiv_mode)
        | DPLL_CFGCR1_KDIV(params.kdiv)
        | DPLL_CFGCR1_PDIV(params.pdiv)
        | if DISPLAY_VER(display) >= 12 {
            TGL_DPLL_CFGCR1_CFSELOVRD_NORMAL_XTAL
        } else {
            DPLL_CFGCR1_CENTRAL_FREQ_8400
        };
    if (*display).vbt.override_afc_startup {
        (*icl).div0 = TGL_DPLL0_DIV0_AFC_STARTUP((*display).vbt.override_afc_startup_val as u32);
    }
}

unsafe extern "C" fn icl_ddi_combo_pll_get_freq(
    display: *mut IntelDisplay,
    _pll: *const IntelDpll,
    hw_state: *const IntelDpllHwState,
) -> i32 {
    if display.is_null() || hw_state.is_null() {
        return 0;
    }
    let hw = &*hw_state_icl(hw_state);
    let ref_clock = icl_wrpll_ref_clock(display);
    let pdiv = match hw.cfgcr1 & DPLL_CFGCR1_PDIV_MASK {
        DPLL_CFGCR1_PDIV_2 => 2,
        DPLL_CFGCR1_PDIV_3 => 3,
        DPLL_CFGCR1_PDIV_5 => 5,
        DPLL_CFGCR1_PDIV_7 => 7,
        _ => return 0,
    };
    let kdiv = match hw.cfgcr1 & DPLL_CFGCR1_KDIV_MASK {
        DPLL_CFGCR1_KDIV_1 => 1,
        DPLL_CFGCR1_KDIV_2 => 2,
        DPLL_CFGCR1_KDIV_3 => 3,
        _ => return 0,
    };
    let qdiv = if hw.cfgcr1 & DPLL_CFGCR1_QDIV_MODE(1) != 0 {
        (hw.cfgcr1 & DPLL_CFGCR1_QDIV_RATIO_MASK) >> DPLL_CFGCR1_QDIV_RATIO_SHIFT
    } else {
        1
    };
    if qdiv == 0 {
        return 0;
    }
    let mut frac = (hw.cfgcr0 & DPLL_CFGCR0_DCO_FRACTION_MASK) >> DPLL_CFGCR0_DCO_FRACTION_SHIFT;
    if ehl_combo_pll_div_frac_wa_needed(display) {
        frac *= 2;
    }
    let dco = (hw.cfgcr0 & DPLL_CFGCR0_DCO_INTEGER_MASK) * ref_clock
        + ((frac as u64 * ref_clock as u64) / 0x8000) as u32;
    (dco / (pdiv * qdiv * kdiv * 5)) as i32
}

unsafe fn icl_mg_pll_find_divisors(
    clock_khz: i32,
    is_dp: bool,
    use_ssc: bool,
    target_dco_khz: &mut u32,
    hw_state: &mut IclDpllHwState,
    is_dkl: bool,
) -> bool {
    const DIV1_VALS: [i32; 4] = [7, 5, 3, 2];
    let dco_min = if is_dp {
        8_100_000
    } else if use_ssc {
        8_000_000
    } else {
        7_992_000
    };
    let dco_max = if is_dp { 8_100_000 } else { 10_000_000 };
    if clock_khz <= 0 {
        return false;
    }
    for div1 in DIV1_VALS {
        for div2 in (1..=10).rev() {
            let dco = div1 * div2 * clock_khz * 5;
            if dco < dco_min || dco > dco_max {
                continue;
            }
            let (a_divratio, tline_drv) = if div2 >= 2 {
                (if is_dp { 10 } else { 5 }, if is_dkl { 1 } else { 2 })
            } else {
                (5, 0)
            };
            let hsdiv = match div1 {
                2 => MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_2,
                3 => MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_3,
                5 => MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_5,
                7 => MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_7,
                _ => continue,
            };
            *target_dco_khz = dco as u32;
            hw_state.mg_refclkin_ctl = MG_REFCLKIN_CTL_OD_2_MUX(1);
            hw_state.mg_clktop2_coreclkctl1 = MG_CLKTOP2_CORECLKCTL1_A_DIVRATIO(a_divratio);
            hw_state.mg_clktop2_hsclkctl = MG_CLKTOP2_HSCLKCTL_TLINEDRV_CLKSEL(tline_drv)
                | MG_CLKTOP2_HSCLKCTL_CORE_INPUTSEL(if is_dp { 0 } else { 1 })
                | hsdiv
                | MG_CLKTOP2_HSCLKCTL_DSDIV_RATIO(div2 as u32);
            return true;
        }
    }
    false
}

unsafe fn icl_calc_mg_pll_state(
    crtc_state: *const IntelCrtcState,
    dpll_hw_state: *mut IntelDpllHwState,
) -> i32 {
    if crtc_state.is_null() || dpll_hw_state.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let display = display_from_crtc_state(crtc_state);
    if display.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let hw = hw_state_icl_mut(dpll_hw_state);
    let refclk = (*display).dpll.ref_clks.nssc;
    let clock = (*crtc_state).port_clock;
    let is_dp = !intel_crtc_has_type(
        crtc_state,
        crate::intel_display_types_upstream::INTEL_OUTPUT_HDMI,
    );
    let is_dkl = DISPLAY_VER(display) >= 12;
    let mut dco = 0u32;
    if !icl_mg_pll_find_divisors(clock, is_dp, false, &mut dco, &mut *hw, is_dkl) {
        return -tk_linux_kapi::config::EINVAL;
    }

    let mut m1 = 2u32;
    let mut m2_int = dco / ((refclk as u32).saturating_mul(m1));
    if m2_int > 255 {
        if !is_dkl {
            m1 = 4;
            m2_int = dco / ((refclk as u32).saturating_mul(m1));
        }
        if m2_int > 255 {
            return -tk_linux_kapi::config::EINVAL;
        }
    }
    let denominator = (refclk as u32).saturating_mul(m1);
    if refclk <= 0 || denominator == 0 {
        return -tk_linux_kapi::config::EINVAL;
    }
    let m2_rem = dco % denominator;
    let m2_frac = ((m2_rem as u64) << 22) / denominator as u64;
    let (iref_ndiv, iref_trim, iref_pulse_w) = match refclk {
        19200 => (1u32, 28u32, 1u32),
        24000 => (1, 25, 2),
        38400 => (2, 28, 1),
        _ => return -tk_linux_kapi::config::EINVAL,
    };
    let tdc_targetcnt = (2u64 * 1000 * 100000 * 10 / (132 * refclk as u64) + 5) / 10;
    let feedfwgain = if m2_rem > 0 {
        (m1 as u64 * 1_000_000 * 100 / (dco as u64 * 3 / 10)) as u32
    } else {
        0
    };
    let (prop_coeff, int_coeff) = if dco >= 9_000_000 { (5, 10) } else { (4, 8) };
    let ssc_stepsize = 0u64;
    let ssc_steplen = 0u64;
    let ssc_steplog = 4u64;

    if is_dkl {
        (*hw).mg_pll_div0 = DKL_PLL_DIV0_INTEG_COEFF(int_coeff)
            | DKL_PLL_DIV0_PROP_COEFF(prop_coeff)
            | DKL_PLL_DIV0_FBPREDIV(m1)
            | DKL_PLL_DIV0_FBDIV_INT(m2_int);
        if (*display).vbt.override_afc_startup {
            (*hw).mg_pll_div0 |=
                DKL_PLL_DIV0_AFC_STARTUP((*display).vbt.override_afc_startup_val as u32);
        }
        (*hw).mg_pll_div1 =
            DKL_PLL_DIV1_IREF_TRIM(iref_trim) | DKL_PLL_DIV1_TDC_TARGET_CNT(tdc_targetcnt as u32);
        (*hw).mg_pll_ssc = DKL_PLL_SSC_IREF_NDIV_RATIO(iref_ndiv)
            | DKL_PLL_SSC_STEP_LEN(ssc_steplen as u32)
            | DKL_PLL_SSC_STEP_NUM(ssc_steplog as u32);
        (*hw).mg_pll_bias = if m2_frac != 0 {
            DKL_PLL_BIAS_FRAC_EN_H
        } else {
            0
        } | DKL_PLL_BIAS_FBDIV_FRAC(m2_frac as u32);
        (*hw).mg_pll_tdc_coldst_bias =
            DKL_PLL_TDC_SSC_STEP_SIZE(ssc_stepsize as u32) | DKL_PLL_TDC_FEED_FWD_GAIN(feedfwgain);
    } else {
        (*hw).mg_pll_div0 = if m2_rem > 0 { MG_PLL_DIV0_FRACNEN_H } else { 0 }
            | MG_PLL_DIV0_FBDIV_FRAC(m2_frac as u32)
            | MG_PLL_DIV0_FBDIV_INT(m2_int);
        (*hw).mg_pll_div1 = MG_PLL_DIV1_IREF_NDIVRATIO(iref_ndiv)
            | MG_PLL_DIV1_DITHER_DIV_2
            | MG_PLL_DIV1_NDIVRATIO(1)
            | MG_PLL_DIV1_FBPREDIV(m1);
        (*hw).mg_pll_lf = MG_PLL_LF_TDCTARGETCNT(tdc_targetcnt as u32)
            | MG_PLL_LF_AFCCNTSEL_512
            | MG_PLL_LF_GAINCTRL(1)
            | MG_PLL_LF_INT_COEFF(int_coeff)
            | MG_PLL_LF_PROP_COEFF(prop_coeff);
        (*hw).mg_pll_frac_lock = MG_PLL_FRAC_LOCK_TRUELOCK_CRIT_32
            | MG_PLL_FRAC_LOCK_EARLYLOCK_CRIT_32
            | MG_PLL_FRAC_LOCK_LOCKTHRESH(10)
            | MG_PLL_FRAC_LOCK_DCODITHEREN
            | MG_PLL_FRAC_LOCK_FEEDFWRDGAIN(feedfwgain);
        if m2_rem > 0 {
            (*hw).mg_pll_frac_lock |= MG_PLL_FRAC_LOCK_FEEDFWRDCAL_EN;
        }
        (*hw).mg_pll_ssc = MG_PLL_SSC_TYPE(2)
            | MG_PLL_SSC_STEPLENGTH(ssc_steplen as u32)
            | MG_PLL_SSC_STEPNUM(ssc_steplog as u32)
            | MG_PLL_SSC_FLLEN
            | MG_PLL_SSC_STEPSIZE(ssc_stepsize as u32);
        (*hw).mg_pll_tdc_coldst_bias = MG_PLL_TDC_COLDST_COLDSTART
            | MG_PLL_TDC_COLDST_IREFINT_EN
            | MG_PLL_TDC_COLDST_REFBIAS_START_PULSE_W(iref_pulse_w)
            | MG_PLL_TDC_TDCOVCCORR_EN
            | MG_PLL_TDC_TDCSEL(3);
        (*hw).mg_pll_bias = MG_PLL_BIAS_BIAS_GB_SEL(3)
            | MG_PLL_BIAS_INIT_DCOAMP(0x3f)
            | MG_PLL_BIAS_BIAS_BONUS(10)
            | MG_PLL_BIAS_BIASCAL_EN
            | MG_PLL_BIAS_CTRIM(12)
            | MG_PLL_BIAS_VREF_RDAC(4)
            | MG_PLL_BIAS_IREFTRIM(iref_trim);
        if refclk == 38400 {
            (*hw).mg_pll_tdc_coldst_bias_mask = MG_PLL_TDC_COLDST_COLDSTART;
            (*hw).mg_pll_bias_mask = 0;
        } else {
            (*hw).mg_pll_tdc_coldst_bias_mask = u32::MAX;
            (*hw).mg_pll_bias_mask = u32::MAX;
        }
        (*hw).mg_pll_tdc_coldst_bias &= (*hw).mg_pll_tdc_coldst_bias_mask;
        (*hw).mg_pll_bias &= (*hw).mg_pll_bias_mask;
    }
    0
}

unsafe extern "C" fn icl_ddi_mg_pll_get_freq(
    display: *mut IntelDisplay,
    _pll: *const IntelDpll,
    dpll_hw_state: *const IntelDpllHwState,
) -> i32 {
    if display.is_null() || dpll_hw_state.is_null() {
        return 0;
    }
    let hw = &*hw_state_icl(dpll_hw_state);
    let (m1, m2_int, m2_frac) = if DISPLAY_VER(display) >= 12 {
        let m1 = (hw.mg_pll_div0 & DKL_PLL_DIV0_FBPREDIV_MASK) >> DKL_PLL_DIV0_FBPREDIV_SHIFT;
        let m2 = hw.mg_pll_div0 & DKL_PLL_DIV0_FBDIV_INT_MASK;
        let frac = if hw.mg_pll_bias & DKL_PLL_BIAS_FRAC_EN_H != 0 {
            (hw.mg_pll_bias & DKL_PLL_BIAS_FBDIV_FRAC_MASK) >> DKL_PLL_BIAS_FBDIV_SHIFT
        } else {
            0
        };
        (m1, m2, frac)
    } else {
        let m1 = (hw.mg_pll_div1 & MG_PLL_DIV1_FBPREDIV_MASK) >> MG_PLL_DIV1_FBPREDIV_SHIFT;
        let m2 = hw.mg_pll_div0 & MG_PLL_DIV0_FBDIV_INT_MASK;
        let frac = if hw.mg_pll_div0 & MG_PLL_DIV0_FRACNEN_H != 0 {
            (hw.mg_pll_div0 & MG_PLL_DIV0_FBDIV_FRAC_MASK) >> MG_PLL_DIV0_FBDIV_FRAC_SHIFT
        } else {
            0
        };
        (m1, m2, frac)
    };
    let div1 = match hw.mg_clktop2_hsclkctl & MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_MASK {
        MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_2 => 2u32,
        MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_3 => 3,
        MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_5 => 5,
        MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_7 => 7,
        _ => return 0,
    };
    let mut div2 = (hw.mg_clktop2_hsclkctl & MG_CLKTOP2_HSCLKCTL_DSDIV_RATIO_MASK)
        >> MG_CLKTOP2_HSCLKCTL_DSDIV_RATIO_SHIFT;
    if div2 == 0 {
        div2 = 1;
    }
    let ref_clock = (*display).dpll.ref_clks.nssc.max(0) as u64;
    let tmp =
        m1 as u64 * m2_int as u64 * ref_clock + ((m1 as u64 * m2_frac as u64 * ref_clock) >> 22);
    (tmp / (5 * div1 as u64 * div2 as u64)) as i32
}

unsafe fn intel_combo_pll_enable_reg(
    display: *mut IntelDisplay,
    pll: *const IntelDpll,
) -> I915RegT {
    if display.is_null() || pll.is_null() || (*pll).info.is_null() {
        return INVALID_MMIO_REG;
    }
    ICL_DPLL_ENABLE((*(*pll).info).id as u32)
}

unsafe fn intel_tc_pll_enable_reg(_display: *mut IntelDisplay, pll: *const IntelDpll) -> I915RegT {
    if pll.is_null() || (*pll).info.is_null() {
        return INVALID_MMIO_REG;
    }
    let tc_port = ((*(*pll).info).id - DPLL_ID_ICL_MGPLL1) as u32;
    ADLP_PORTTC_PLL_ENABLE(tc_port)
}

unsafe fn icl_pll_get_hw_state(
    display: *mut IntelDisplay,
    pll: *mut IntelDpll,
    hw_state: *mut IntelDpllHwState,
    enable_reg: I915RegT,
) -> bool {
    if display.is_null() || pll.is_null() || hw_state.is_null() {
        return false;
    }
    let wakeref = crate::intel_display_power_upstream::intel_display_power_get_if_enabled(
        display,
        POWER_DOMAIN_DISPLAY_CORE,
    );
    if wakeref.is_null() {
        return false;
    }
    let mut found = false;
    if intel_de_read(display, enable_reg) & PLL_ENABLE != 0 {
        let id = (*(*pll).info).id as u32;
        let icl = hw_state_icl_mut(hw_state);
        if DISPLAY_VER(display) >= 12 {
            (*icl).cfgcr0 = intel_de_read(display, TGL_DPLL_CFGCR0(id));
            (*icl).cfgcr1 = intel_de_read(display, TGL_DPLL_CFGCR1(id));
            if (*display).vbt.override_afc_startup {
                (*icl).div0 =
                    intel_de_read(display, TGL_DPLL0_DIV0(id)) & TGL_DPLL0_DIV0_AFC_STARTUP_MASK;
            }
        } else {
            (*icl).cfgcr0 = intel_de_read(display, ICL_DPLL_CFGCR0(id));
            (*icl).cfgcr1 = intel_de_read(display, ICL_DPLL_CFGCR1(id));
        }
        found = true;
    }
    crate::intel_display_power_upstream::intel_display_power_put(
        display,
        POWER_DOMAIN_DISPLAY_CORE,
        wakeref,
    );
    found
}

unsafe extern "C" fn combo_pll_get_hw_state(
    display: *mut IntelDisplay,
    pll: *mut IntelDpll,
    hw_state: *mut IntelDpllHwState,
) -> bool {
    let enable_reg = intel_combo_pll_enable_reg(display, pll);
    icl_pll_get_hw_state(display, pll, hw_state, enable_reg)
}

unsafe extern "C" fn icl_tbt_pll_get_hw_state(
    display: *mut IntelDisplay,
    pll: *mut IntelDpll,
    hw_state: *mut IntelDpllHwState,
) -> bool {
    icl_pll_get_hw_state(display, pll, hw_state, TBT_PLL_ENABLE)
}

unsafe extern "C" fn dkl_pll_get_hw_state(
    display: *mut IntelDisplay,
    pll: *mut IntelDpll,
    dpll_hw_state: *mut IntelDpllHwState,
) -> bool {
    if display.is_null() || pll.is_null() || dpll_hw_state.is_null() || (*pll).info.is_null() {
        return false;
    }
    let wakeref = crate::intel_display_power_upstream::intel_display_power_get_if_enabled(
        display,
        POWER_DOMAIN_DISPLAY_CORE,
    );
    if wakeref.is_null() {
        return false;
    }
    let enable_reg = intel_tc_pll_enable_reg(display, pll);
    let mut found = false;
    if intel_de_read(display, enable_reg) & PLL_ENABLE != 0 {
        let tc_port = ((*(*pll).info).id - DPLL_ID_ICL_MGPLL1) as u32;
        let hw = hw_state_icl_mut(dpll_hw_state);
        (*hw).mg_refclkin_ctl =
            intel_dkl_phy_read(display, DKL_REFCLKIN_CTL(tc_port)) & MG_REFCLKIN_CTL_OD_2_MUX_MASK;
        (*hw).mg_clktop2_hsclkctl = intel_dkl_phy_read(display, DKL_CLKTOP2_HSCLKCTL(tc_port))
            & (MG_CLKTOP2_HSCLKCTL_TLINEDRV_CLKSEL_MASK
                | MG_CLKTOP2_HSCLKCTL_CORE_INPUTSEL_MASK
                | MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_MASK
                | MG_CLKTOP2_HSCLKCTL_DSDIV_RATIO_MASK);
        (*hw).mg_clktop2_coreclkctl1 =
            intel_dkl_phy_read(display, DKL_CLKTOP2_CORECLKCTL1(tc_port))
                & MG_CLKTOP2_CORECLKCTL1_A_DIVRATIO_MASK;
        (*hw).mg_pll_div0 = intel_dkl_phy_read(display, DKL_PLL_DIV0(tc_port));
        let mut mask = DKL_PLL_DIV0_MASK;
        if (*display).vbt.override_afc_startup {
            mask |= DKL_PLL_DIV0_AFC_STARTUP_MASK;
        }
        (*hw).mg_pll_div0 &= mask;
        (*hw).mg_pll_div1 = intel_dkl_phy_read(display, DKL_PLL_DIV1(tc_port))
            & (DKL_PLL_DIV1_IREF_TRIM_MASK | DKL_PLL_DIV1_TDC_TARGET_CNT_MASK);
        (*hw).mg_pll_ssc = intel_dkl_phy_read(display, DKL_PLL_SSC(tc_port))
            & (DKL_PLL_SSC_IREF_NDIV_RATIO_MASK
                | DKL_PLL_SSC_STEP_LEN_MASK
                | DKL_PLL_SSC_STEP_NUM_MASK
                | DKL_PLL_SSC_EN);
        (*hw).mg_pll_bias = intel_dkl_phy_read(display, DKL_PLL_BIAS(tc_port))
            & (DKL_PLL_BIAS_FRAC_EN_H | DKL_PLL_BIAS_FBDIV_FRAC_MASK);
        (*hw).mg_pll_tdc_coldst_bias =
            intel_dkl_phy_read(display, DKL_PLL_TDC_COLDST_BIAS(tc_port))
                & (DKL_PLL_TDC_SSC_STEP_SIZE_MASK | DKL_PLL_TDC_FEED_FWD_GAIN_MASK);
        found = true;
    }
    crate::intel_display_power_upstream::intel_display_power_put(
        display,
        POWER_DOMAIN_DISPLAY_CORE,
        wakeref,
    );
    found
}

unsafe fn icl_dpll_write(
    display: *mut IntelDisplay,
    pll: *mut IntelDpll,
    hw_state: *const IclDpllHwState,
) {
    if display.is_null() || pll.is_null() || hw_state.is_null() || (*pll).info.is_null() {
        return;
    }
    let id = (*(*pll).info).id as u32;
    let cfgcr0 = if DISPLAY_VER(display) >= 12 {
        TGL_DPLL_CFGCR0(id)
    } else {
        ICL_DPLL_CFGCR0(id)
    };
    let cfgcr1 = if DISPLAY_VER(display) >= 12 {
        TGL_DPLL_CFGCR1(id)
    } else {
        ICL_DPLL_CFGCR1(id)
    };
    intel_de_write(display, cfgcr0, (*hw_state).cfgcr0);
    intel_de_write(display, cfgcr1, (*hw_state).cfgcr1);
    if DISPLAY_VER(display) >= 12 && (*display).vbt.override_afc_startup {
        intel_de_rmw(
            display,
            TGL_DPLL0_DIV0(id),
            TGL_DPLL0_DIV0_AFC_STARTUP_MASK,
            (*hw_state).div0,
        );
    }
    intel_de_posting_read(display, cfgcr1);
}

unsafe fn dkl_pll_write(
    display: *mut IntelDisplay,
    pll: *mut IntelDpll,
    hw: *const IclDpllHwState,
) {
    if display.is_null() || pll.is_null() || hw.is_null() || (*pll).info.is_null() {
        return;
    }
    let tc_port = ((*(*pll).info).id - DPLL_ID_ICL_MGPLL1) as u32;
    let mut val = intel_dkl_phy_read(display, DKL_REFCLKIN_CTL(tc_port));
    intel_dkl_phy_write(
        display,
        DKL_REFCLKIN_CTL(tc_port),
        (val & !MG_REFCLKIN_CTL_OD_2_MUX_MASK) | (*hw).mg_refclkin_ctl,
    );
    val = intel_dkl_phy_read(display, DKL_CLKTOP2_CORECLKCTL1(tc_port));
    intel_dkl_phy_write(
        display,
        DKL_CLKTOP2_CORECLKCTL1(tc_port),
        (val & !MG_CLKTOP2_CORECLKCTL1_A_DIVRATIO_MASK) | (*hw).mg_clktop2_coreclkctl1,
    );
    val = intel_dkl_phy_read(display, DKL_CLKTOP2_HSCLKCTL(tc_port));
    let mask = MG_CLKTOP2_HSCLKCTL_TLINEDRV_CLKSEL_MASK
        | MG_CLKTOP2_HSCLKCTL_CORE_INPUTSEL_MASK
        | MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_MASK
        | MG_CLKTOP2_HSCLKCTL_DSDIV_RATIO_MASK;
    intel_dkl_phy_write(
        display,
        DKL_CLKTOP2_HSCLKCTL(tc_port),
        (val & !mask) | (*hw).mg_clktop2_hsclkctl,
    );
    val = DKL_PLL_DIV0_MASK;
    if (*display).vbt.override_afc_startup {
        val |= DKL_PLL_DIV0_AFC_STARTUP_MASK;
    }
    intel_dkl_phy_rmw(display, DKL_PLL_DIV0(tc_port), val, (*hw).mg_pll_div0);
    val = intel_dkl_phy_read(display, DKL_PLL_DIV1(tc_port));
    val = (val & !(DKL_PLL_DIV1_IREF_TRIM_MASK | DKL_PLL_DIV1_TDC_TARGET_CNT_MASK))
        | (*hw).mg_pll_div1;
    intel_dkl_phy_write(display, DKL_PLL_DIV1(tc_port), val);
    val = intel_dkl_phy_read(display, DKL_PLL_SSC(tc_port));
    let mask = DKL_PLL_SSC_IREF_NDIV_RATIO_MASK
        | DKL_PLL_SSC_STEP_LEN_MASK
        | DKL_PLL_SSC_STEP_NUM_MASK
        | DKL_PLL_SSC_EN;
    intel_dkl_phy_write(
        display,
        DKL_PLL_SSC(tc_port),
        (val & !mask) | (*hw).mg_pll_ssc,
    );
    val = intel_dkl_phy_read(display, DKL_PLL_BIAS(tc_port));
    let mask = DKL_PLL_BIAS_FRAC_EN_H | DKL_PLL_BIAS_FBDIV_FRAC_MASK;
    intel_dkl_phy_write(
        display,
        DKL_PLL_BIAS(tc_port),
        (val & !mask) | (*hw).mg_pll_bias,
    );
    val = intel_dkl_phy_read(display, DKL_PLL_TDC_COLDST_BIAS(tc_port));
    let mask = DKL_PLL_TDC_SSC_STEP_SIZE_MASK | DKL_PLL_TDC_FEED_FWD_GAIN_MASK;
    intel_dkl_phy_write(
        display,
        DKL_PLL_TDC_COLDST_BIAS(tc_port),
        (val & !mask) | (*hw).mg_pll_tdc_coldst_bias,
    );
    intel_dkl_phy_posting_read(display, DKL_PLL_TDC_COLDST_BIAS(tc_port));
}

unsafe fn icl_pll_power_enable(display: *mut IntelDisplay, pll: *mut IntelDpll, reg: I915RegT) {
    intel_de_rmw(display, reg, 0, PLL_POWER_ENABLE);
    if crate::intel_de_upstream::intel_de_wait_for_set_ms(display, reg, PLL_POWER_STATE, 1) != 0 {
        drm_err!(
            &(*display).drm,
            "PLL {} power did not enable\n",
            (*(*pll).info).id
        );
    }
}

unsafe fn icl_pll_enable(display: *mut IntelDisplay, pll: *mut IntelDpll, reg: I915RegT) {
    intel_de_rmw(display, reg, 0, PLL_ENABLE);
    if crate::intel_de_upstream::intel_de_wait_for_set_ms(display, reg, PLL_LOCK, 1) != 0 {
        drm_err!(
            &(*display).drm,
            "PLL {} failed to lock\n",
            (*(*pll).info).id
        );
    }
}

unsafe fn adlp_cmtg_clock_gating_wa(display: *mut IntelDisplay, pll: *mut IntelDpll) {
    if !platform_alderlake_p(display)
        || !IS_DISPLAY_STEP(display, STEP_A0, STEP_B0)
        || pll.is_null()
        || (*pll).info.is_null()
        || (*(*pll).info).id != DPLL_ID_ICL_DPLL0
    {
        return;
    }
    let old = intel_de_read(display, TRANS_CMTG_CHICKEN);
    let _ = crate::intel_de_upstream::intel_de_rmw(
        display,
        TRANS_CMTG_CHICKEN,
        u32::MAX,
        DISABLE_DPT_CLK_GATING,
    );
    if old & !DISABLE_DPT_CLK_GATING != 0 {
        drm_warn!(
            &(*display).drm,
            "unexpected flags in TRANS_CMTG_CHICKEN: {:#x}\n",
            old
        );
    }
}

unsafe extern "C" fn combo_pll_enable(
    display: *mut IntelDisplay,
    pll: *mut IntelDpll,
    hw_state: *const IntelDpllHwState,
) {
    if display.is_null() || pll.is_null() || hw_state.is_null() {
        return;
    }
    let reg = intel_combo_pll_enable_reg(display, pll);
    icl_pll_power_enable(display, pll, reg);
    icl_dpll_write(display, pll, hw_state_icl(hw_state));
    icl_pll_enable(display, pll, reg);
    adlp_cmtg_clock_gating_wa(display, pll);
}

unsafe extern "C" fn icl_tbt_pll_enable(
    display: *mut IntelDisplay,
    pll: *mut IntelDpll,
    hw_state: *const IntelDpllHwState,
) {
    if display.is_null() || pll.is_null() || hw_state.is_null() {
        return;
    }
    icl_pll_power_enable(display, pll, TBT_PLL_ENABLE);
    icl_dpll_write(display, pll, hw_state_icl(hw_state));
    icl_pll_enable(display, pll, TBT_PLL_ENABLE);
}

unsafe extern "C" fn mg_pll_enable(
    display: *mut IntelDisplay,
    pll: *mut IntelDpll,
    hw_state: *const IntelDpllHwState,
) {
    if display.is_null() || pll.is_null() || hw_state.is_null() {
        return;
    }
    let reg = intel_tc_pll_enable_reg(display, pll);
    icl_pll_power_enable(display, pll, reg);
    if DISPLAY_VER(display) >= 12 {
        dkl_pll_write(display, pll, hw_state_icl(hw_state));
    }
    icl_pll_enable(display, pll, reg);
}

unsafe fn icl_pll_disable(display: *mut IntelDisplay, pll: *mut IntelDpll, reg: I915RegT) {
    intel_de_rmw(display, reg, PLL_ENABLE, 0);
    if crate::intel_de_upstream::intel_de_wait_for_clear_ms(display, reg, PLL_LOCK, 1) != 0 {
        drm_err!(
            &(*display).drm,
            "PLL {} remained locked during disable\n",
            (*(*pll).info).id
        );
    }
    intel_de_rmw(display, reg, PLL_POWER_ENABLE, 0);
    if crate::intel_de_upstream::intel_de_wait_for_clear_ms(display, reg, PLL_POWER_STATE, 1) != 0 {
        drm_err!(
            &(*display).drm,
            "PLL {} power did not disable\n",
            (*(*pll).info).id
        );
    }
}

unsafe extern "C" fn combo_pll_disable(display: *mut IntelDisplay, pll: *mut IntelDpll) {
    if !display.is_null() && !pll.is_null() {
        icl_pll_disable(display, pll, intel_combo_pll_enable_reg(display, pll));
    }
}

unsafe extern "C" fn icl_tbt_pll_disable(display: *mut IntelDisplay, pll: *mut IntelDpll) {
    if !display.is_null() && !pll.is_null() {
        icl_pll_disable(display, pll, TBT_PLL_ENABLE);
    }
}

unsafe extern "C" fn mg_pll_disable(display: *mut IntelDisplay, pll: *mut IntelDpll) {
    if !display.is_null() && !pll.is_null() {
        icl_pll_disable(display, pll, intel_tc_pll_enable_reg(display, pll));
    }
}

unsafe extern "C" fn icl_ddi_tbt_pll_get_freq(
    _display: *mut IntelDisplay,
    _pll: *const IntelDpll,
    _hw_state: *const IntelDpllHwState,
) -> i32 {
    0 // TBT PLL exposes multiple clocks; DDI selects the active output.
}

unsafe fn intel_enable_shared_dpll(display: *mut IntelDisplay, pll: *mut IntelDpll) {
    if (*(*pll).info).power_domain != 0 {
        (*pll).wakeref = crate::intel_display_power_upstream::intel_display_power_get(
            display,
            (*(*pll).info).power_domain,
        );
    }
    let funcs = dpll_funcs(pll);
    if !funcs.is_null() {
        ((*funcs).enable)(display, pll, ptr::addr_of!((*pll).state.hw_state));
    }
    (*pll).on = true;
}

unsafe fn intel_disable_shared_dpll(display: *mut IntelDisplay, pll: *mut IntelDpll) {
    let funcs = dpll_funcs(pll);
    if !funcs.is_null() {
        ((*funcs).disable)(display, pll);
    }
    (*pll).on = false;
    if (*(*pll).info).power_domain != 0 {
        crate::intel_display_power_upstream::intel_display_power_put(
            display,
            (*(*pll).info).power_domain,
            (*pll).wakeref,
        );
        (*pll).wakeref = ptr::null_mut();
    }
}

unsafe fn assert_dpll(display: *mut IntelDisplay, pll: *mut IntelDpll, expected: bool) {
    if display.is_null() || pll.is_null() {
        return;
    }
    let mut hw: IntelDpllHwState = mem::zeroed();
    let actual = intel_dpll_get_hw_state(display, pll, &mut hw);
    if actual != expected {
        drm_warn!(
            &(*display).drm,
            "PLL {} state assertion failed (expected {}, actual {})\n",
            (*(*pll).info).id,
            expected,
            actual
        );
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dpll_enable(crtc_state: *const IntelCrtcState) {
    if crtc_state.is_null() || (*crtc_state).uapi.crtc.is_null() {
        return;
    }
    let crtc = (*crtc_state).uapi.crtc.cast::<IntelCrtc>();
    let display = display_from_crtc(crtc);
    let pll = (*crtc_state).intel_dpll;
    if display.is_null() || pll.is_null() {
        return;
    }
    let pipe_mask = intel_crtc_joined_pipe_mask(crtc_state);
    tk_linux_kapi::mutex::mutex_lock(ptr::addr_of_mut!((*display).dpll.lock));
    let old_mask = (*pll).active_mask;
    if (*pll).state.pipe_mask & pipe_mask == 0 || (*pll).active_mask & pipe_mask != 0 {
        drm_warn!(
            &(*display).drm,
            "invalid PLL enable reference for pipe {}\n",
            (*crtc).pipe
        );
        tk_linux_kapi::mutex::mutex_unlock(ptr::addr_of_mut!((*display).dpll.lock));
        return;
    }
    (*pll).active_mask |= pipe_mask;
    if old_mask != 0 {
        if !(*pll).on {
            drm_warn!(
                &(*display).drm,
                "shared PLL {} was unexpectedly off\n",
                (*(*pll).info).id
            );
        } else {
            assert_dpll(display, pll, true);
        }
    } else if (*pll).on {
        drm_warn!(
            &(*display).drm,
            "PLL {} was already on without active users\n",
            (*(*pll).info).id
        );
    } else {
        intel_enable_shared_dpll(display, pll);
    }
    tk_linux_kapi::mutex::mutex_unlock(ptr::addr_of_mut!((*display).dpll.lock));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dpll_disable(crtc_state: *const IntelCrtcState) {
    if crtc_state.is_null() || (*crtc_state).uapi.crtc.is_null() {
        return;
    }
    let crtc = (*crtc_state).uapi.crtc.cast::<IntelCrtc>();
    let display = display_from_crtc(crtc);
    let pll = (*crtc_state).intel_dpll;
    if display.is_null() || pll.is_null() || DISPLAY_VER(display) < 5 {
        return;
    }
    let pipe_mask = intel_crtc_joined_pipe_mask(crtc_state);
    tk_linux_kapi::mutex::mutex_lock(ptr::addr_of_mut!((*display).dpll.lock));
    if (*pll).active_mask & pipe_mask == 0 {
        drm_warn!(
            &(*display).drm,
            "PLL {} not active for pipe {}\n",
            (*(*pll).info).id,
            (*crtc).pipe
        );
    } else {
        assert_dpll(display, pll, true);
        (*pll).active_mask &= !pipe_mask;
        if (*pll).active_mask == 0 {
            intel_disable_shared_dpll(display, pll);
        }
    }
    tk_linux_kapi::mutex::mutex_unlock(ptr::addr_of_mut!((*display).dpll.lock));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dpll_swap_state(state: *mut IntelAtomicState) {
    let display = display_from_atomic(state);
    if display.is_null() || !(*state).dpll_set {
        return;
    }
    for index in 0..(*display).dpll.num_dpll.max(0) as usize {
        ptr::swap(
            ptr::addr_of_mut!((*display).dpll.dplls[index].state),
            ptr::addr_of_mut!((*state).dpll_state[index]),
        );
    }
}

unsafe fn for_each_display_crtc(display: *mut IntelDisplay, mut f: impl FnMut(*mut IntelCrtc)) {
    if display.is_null() {
        return;
    }
    let head = ptr::addr_of_mut!((*display).pipe_list);
    let mut node = (*head).next;
    while !node.is_null() && node != head {
        let offset = mem::offset_of!(IntelCrtc, pipe_head);
        f(node.cast::<u8>().sub(offset).cast::<IntelCrtc>());
        node = (*node).next;
    }
}

unsafe fn readout_dpll_hw_state(display: *mut IntelDisplay, pll: *mut IntelDpll) {
    (*pll).on = intel_dpll_get_hw_state(display, pll, ptr::addr_of_mut!((*pll).state.hw_state));
    if (*pll).on && !(*pll).info.is_null() && (*(*pll).info).power_domain != 0 {
        (*pll).wakeref = crate::intel_display_power_upstream::intel_display_power_get(
            display,
            (*(*pll).info).power_domain,
        );
    }
    (*pll).state.pipe_mask = 0;
    for_each_display_crtc(display, |crtc| unsafe {
        let crtc_state = (*crtc).base.state.cast::<IntelCrtcState>();
        if !crtc_state.is_null() && (*crtc_state).hw.active && (*crtc_state).intel_dpll == pll {
            intel_dpll_crtc_get(crtc, pll, ptr::addr_of_mut!((*pll).state));
        }
    });
    (*pll).active_mask = (*pll).state.pipe_mask;
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dpll_readout_hw_state(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    for index in 0..(*display).dpll.num_dpll.max(0) as usize {
        readout_dpll_hw_state(display, ptr::addr_of_mut!((*display).dpll.dplls[index]));
    }
}

unsafe fn sanitize_dpll_state(display: *mut IntelDisplay, pll: *mut IntelDpll) {
    if !(*pll).on {
        return;
    }
    adlp_cmtg_clock_gating_wa(display, pll);
    if (*pll).active_mask == 0 {
        drm_dbg!(
            &(*display).drm,
            "{} enabled but unused, disabling\n",
            (*(*pll).info).id
        );
        intel_disable_shared_dpll(display, pll);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dpll_sanitize_state(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    for index in 0..(*display).dpll.num_dpll.max(0) as usize {
        sanitize_dpll_state(display, ptr::addr_of_mut!((*display).dpll.dplls[index]));
    }
}

unsafe extern "C" fn icl_dump_hw_state(
    printer: *mut DrmPrinter,
    hw_state: *const IntelDpllHwState,
) {
    if printer.is_null() || hw_state.is_null() {
        return;
    }
    let hw = &*hw_state_icl(hw_state);
    let message = alloc::format!(
        "icl dpll cfgcr0={:#x} cfgcr1={:#x} div0={:#x} mg_refclkin_ctl={:#x} mg_coreclk={:#x} \
         mg_hsclk={:#x} mg_div0={:#x} mg_div1={:#x} mg_lf={:#x} mg_frac_lock={:#x} mg_ssc={:#x} \
         mg_bias={:#x} mg_tdc={:#x}\n",
        hw.cfgcr0,
        hw.cfgcr1,
        hw.div0,
        hw.mg_refclkin_ctl,
        hw.mg_clktop2_coreclkctl1,
        hw.mg_clktop2_hsclkctl,
        hw.mg_pll_div0,
        hw.mg_pll_div1,
        hw.mg_pll_lf,
        hw.mg_pll_frac_lock,
        hw.mg_pll_ssc,
        hw.mg_pll_bias,
        hw.mg_pll_tdc_coldst_bias
    );
    tk_drm_upstream::drm_print_upstream::drm_printf(printer, &message);
}

unsafe extern "C" fn icl_compare_hw_state(
    a: *const IntelDpllHwState,
    b: *const IntelDpllHwState,
) -> bool {
    if a.is_null() || b.is_null() {
        return false;
    }
    let a = &*hw_state_icl(a);
    let b = &*hw_state_icl(b);
    a.cfgcr0 == b.cfgcr0
        && a.cfgcr1 == b.cfgcr1
        && a.div0 == b.div0
        && a.mg_refclkin_ctl == b.mg_refclkin_ctl
        && a.mg_clktop2_coreclkctl1 == b.mg_clktop2_coreclkctl1
        && a.mg_clktop2_hsclkctl == b.mg_clktop2_hsclkctl
        && a.mg_pll_div0 == b.mg_pll_div0
        && a.mg_pll_div1 == b.mg_pll_div1
        && a.mg_pll_lf == b.mg_pll_lf
        && a.mg_pll_frac_lock == b.mg_pll_frac_lock
        && a.mg_pll_ssc == b.mg_pll_ssc
        && a.mg_pll_bias == b.mg_pll_bias
        && a.mg_pll_tdc_coldst_bias == b.mg_pll_tdc_coldst_bias
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dpll_dump_hw_state(
    _display: *mut IntelDisplay,
    printer: *mut DrmPrinter,
    hw_state: *const IntelDpllHwState,
) {
    if !dpll_manager(_display).is_null() {
        icl_dump_hw_state(printer, hw_state);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dpll_compare_hw_state(
    display: *mut IntelDisplay,
    a: *const IntelDpllHwState,
    b: *const IntelDpllHwState,
) -> bool {
    if display.is_null() {
        return false;
    }
    if !dpll_manager(display).is_null() {
        icl_compare_hw_state(a, b)
    } else {
        hw_states_equal(a, b, mem::size_of::<IntelDpllHwState>())
    }
}

unsafe fn verify_single_dpll_state(
    display: *mut IntelDisplay,
    pll: *mut IntelDpll,
    crtc: *mut IntelCrtc,
    new_crtc_state: *const IntelCrtcState,
) {
    if display.is_null() || pll.is_null() || (*pll).info.is_null() {
        return;
    }
    let mut hw_state: IntelDpllHwState = mem::zeroed();
    let hw_on = intel_dpll_get_hw_state(display, pll, &mut hw_state);
    if !(*(*pll).info).always_on {
        if !(*pll).on && (*pll).active_mask != 0 {
            drm_warn!(
                &(*display).drm,
                "PLL {} active but not tracked on\n",
                (*(*pll).info).id
            );
        }
        if (*pll).on && (*pll).active_mask == 0 {
            drm_warn!(
                &(*display).drm,
                "PLL {} tracked on without active users\n",
                (*(*pll).info).id
            );
        }
        if (*pll).on != hw_on {
            drm_warn!(
                &(*display).drm,
                "PLL {} on-state mismatch tracked={} hw={}\n",
                (*(*pll).info).id,
                (*pll).on,
                hw_on
            );
        }
    }
    if crtc.is_null() {
        if (*pll).active_mask & !(*pll).state.pipe_mask != 0 {
            drm_warn!(
                &(*display).drm,
                "PLL {} active users exceed references\n",
                (*(*pll).info).id
            );
        }
        return;
    }
    if new_crtc_state.is_null() || (*crtc).pipe < 0 || (*crtc).pipe >= I915_MAX_PIPES {
        return;
    }
    let bit = 1u8 << (*crtc).pipe;
    if (*new_crtc_state).hw.active != ((*pll).active_mask & bit != 0) {
        drm_warn!(
            &(*display).drm,
            "PLL {} active mask mismatch for pipe {}\n",
            (*(*pll).info).id,
            (*crtc).pipe
        );
    }
    if (*pll).state.pipe_mask & bit == 0 {
        drm_warn!(
            &(*display).drm,
            "PLL {} missing pipe {} reference\n",
            (*(*pll).info).id,
            (*crtc).pipe
        );
    }
    if (*pll).on && !icl_compare_hw_state(ptr::addr_of!((*pll).state.hw_state), &hw_state) {
        drm_warn!(
            &(*display).drm,
            "PLL {} hardware state mismatch\n",
            (*(*pll).info).id
        );
    }
}

unsafe fn has_alt_port_dpll(old: *mut IntelDpll, new: *mut IntelDpll) -> bool {
    !old.is_null()
        && !new.is_null()
        && old != new
        && ((*(*old).info).is_alt_port_dpll || (*(*new).info).is_alt_port_dpll)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dpll_state_verify(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
) {
    let display = display_from_atomic(state);
    if display.is_null() || crtc.is_null() {
        return;
    }
    let old = atomic_old_crtc_state(state, crtc);
    let new = atomic_new_crtc_state(state, crtc);
    if new.is_null() {
        return;
    }
    if !(*new).intel_dpll.is_null() {
        verify_single_dpll_state(display, (*new).intel_dpll, crtc, new);
    }
    if !old.is_null() && !(*old).intel_dpll.is_null() && (*old).intel_dpll != (*new).intel_dpll {
        let pll = (*old).intel_dpll;
        let bit = if (*crtc).pipe >= 0 && (*crtc).pipe < I915_MAX_PIPES {
            1u8 << (*crtc).pipe
        } else {
            0
        };
        if (*pll).active_mask & bit != 0 {
            drm_warn!(
                &(*display).drm,
                "old PLL {} remained active for pipe {}\n",
                (*(*pll).info).id,
                (*crtc).pipe
            );
        }
        if !has_alt_port_dpll(pll, (*new).intel_dpll) && (*pll).state.pipe_mask & bit != 0 {
            drm_warn!(
                &(*display).drm,
                "old PLL {} retained a pipe reference\n",
                (*(*pll).info).id
            );
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dpll_verify_disabled(state: *mut IntelAtomicState) {
    let display = display_from_atomic(state);
    if display.is_null() {
        return;
    }
    for index in 0..(*display).dpll.num_dpll.max(0) as usize {
        verify_single_dpll_state(
            display,
            ptr::addr_of_mut!((*display).dpll.dplls[index]),
            ptr::null_mut(),
            ptr::null(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::icl_dp_combo_pll_params;

    #[test]
    fn dp_combo_pll_selects_refclock_table_and_rejects_unknown_clock() {
        let ref24 = icl_dp_combo_pll_params(24000, 540000).unwrap();
        assert_eq!(ref24.dco_integer, 0x151);
        assert_eq!(ref24.dco_fraction, 0x4000);
        assert_eq!(ref24.pdiv, 0x2);
        assert_eq!(ref24.kdiv, 1);

        let ref19_2 = icl_dp_combo_pll_params(19200, 540000).unwrap();
        assert_eq!(ref19_2.dco_integer, 0x1a5);
        assert_eq!(ref19_2.dco_fraction, 0x7000);
        assert_eq!(ref19_2.pdiv, 0x2);
        assert_eq!(ref19_2.kdiv, 1);

        assert!(icl_dp_combo_pll_params(24000, 123456).is_none());
    }
}
