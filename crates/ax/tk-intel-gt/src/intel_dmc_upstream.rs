// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// Source: drivers/gpu/drm/i915/display/intel_dmc.c (Linux v7.2.3)

#![allow(non_snake_case, dead_code, unsafe_code)]

#[cfg(test)]
use alloc::vec;
use alloc::{
    alloc::{Layout, alloc, dealloc},
    vec::Vec,
};
use core::{
    ffi::{c_char, c_void},
    ptr,
};

use kernel_guard::BaseGuard;
use tk_linux_kapi::{
    firmware::{Firmware, tk_linux_firmware_release, tk_linux_firmware_request_nowarn},
    types::{ListHead, WorkStruct},
    workqueue::{INIT_WORK_C, flush_work, try_queue_work},
};

use crate::{
    intel_de_upstream::{
        intel_de_posting_read, intel_de_read, intel_de_rmw, intel_de_write, intel_de_write_fw,
    },
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_limits_types_upstream::{PIPE_A, Pipe},
    intel_display_power_types_upstream::POWER_DOMAIN_INIT,
    intel_display_power_upstream::{intel_display_power_get, intel_display_power_put},
    intel_display_reg_defs_upstream::reg_field_prep,
    intel_display_regs_upstream::{
        CLKGATE_DIS_PSL_EXT, DC_STATE_DEBUG, DC_STATE_DEBUG_MASK_CORES,
        DC_STATE_DEBUG_MASK_MEMORY_UP, PIPEDMC_GATING_DIS,
    },
    intel_display_types_upstream::{IntelCrtc, IntelCrtcState},
    intel_display_wa_upstream::{DISPLAY_VER, platform_alderlake_p},
    intel_dmc_regs_upstream::{
        ADLP_PIPE_MMIO_END, ADLP_PIPE_MMIO_START, DMC_EVENT_HANDLER_COUNT_GEN12, DMC_EVT_CTL,
        DMC_EVT_CTL_ENABLE, DMC_EVT_CTL_EVENT_ID_MASK, DMC_EVT_CTL_TYPE_EDGE_0_1,
        DMC_EVT_CTL_TYPE_MASK, DMC_EVT_HTP, DMC_FW_MAIN, DMC_MMIO_END_RANGE, DMC_MMIO_START_RANGE,
        DMC_PROGRAM, PIPEDMC_ADAPTIVE_DCB_ENABLE, PIPEDMC_BLOCK_PKGC_SW,
        PIPEDMC_BLOCK_PKGC_SW_BLOCK_PKGC_ALWAYS, PIPEDMC_CONTROL, PIPEDMC_DCB_CTL, PIPEDMC_ENABLE,
        PIPEDMC_INT_VECTOR_MASK, PIPEDMC_STATUS, TGL_MAIN_MMIO_END, TGL_MAIN_MMIO_START,
    },
    intel_workarounds_types_upstream::I915RegT,
    linux_i915_private::DrmI915Private,
};

const DMC_FW_MAX: usize = 5;
const DMC_V1_MAX_MMIO_COUNT: usize = 8;
const DMC_V3_MAX_MMIO_COUNT: usize = 20;
const PACKAGE_MAX_FW_INFO_ENTRIES: usize = 20;
const PACKAGE_V2_MAX_FW_INFO_ENTRIES: usize = 32;
const CSS_HEADER_SIZE: usize = 128;
const PACKAGE_HEADER_SIZE: usize = 16;
const DMC_V1_HEADER_SIZE: usize = 132;
const DMC_V3_HEADER_SIZE: usize = 260;
const DISPLAY_VER13_DMC_MAX_FW_SIZE: usize = 0x20000;
const DMC_V1_MMIO_START_RANGE: u32 = 0x80000;
const DMC_VERSION_OFFSET: usize = 88;
const ERR_ENOENT: i32 = -2;
const GFP_KERNEL: u32 = 0x0cc0;
const PIPEDMC_EVENT_VBLANK: u32 = 0x32;
const PIPEDMC_EVENT_ADAPTIVE_DCB_TRIGGER: u32 = 0x3d;

unsafe extern "C" {
    fn drmm_kzalloc(drm: *mut c_void, size: usize, flags: u32) -> *mut c_void;
    fn intel_parent_pcode_read(
        display: *mut IntelDisplay,
        mbox: u32,
        val: *mut u32,
        val1: *mut u32,
    ) -> i32;
}

struct DmcFirmwareInfo {
    present: bool,
    dmc_offset: u32,
    mmio_count: usize,
    mmioaddr: [I915RegT; DMC_V3_MAX_MMIO_COUNT],
    mmiodata: [u32; DMC_V3_MAX_MMIO_COUNT],
    start_mmioaddr: u32,
    payload: Vec<u32>,
}

impl DmcFirmwareInfo {
    fn empty() -> Self {
        Self {
            present: false,
            dmc_offset: 0,
            mmio_count: 0,
            mmioaddr: [I915RegT { reg: 0 }; DMC_V3_MAX_MMIO_COUNT],
            mmiodata: [0; DMC_V3_MAX_MMIO_COUNT],
            start_mmioaddr: 0,
            payload: Vec::new(),
        }
    }
}

/// Private Linux `struct intel_dmc` state. It is stored behind the existing
/// opaque `display.dmc.dmc` pointer, keeping the shared ABI layout unchanged.
struct IntelDmc {
    work: WorkStruct,
    display: *mut IntelDisplay,
    fw_path: *const c_char,
    max_fw_size: usize,
    allow_fallback: bool,
    version: u32,
    dc5_start: u32,
    dc6_allowed_count: u32,
    info: [DmcFirmwareInfo; DMC_FW_MAX],
}

impl IntelDmc {
    fn new(
        display: *mut IntelDisplay,
        fw_path: *const c_char,
        max_fw_size: usize,
        allow_fallback: bool,
    ) -> *mut Self {
        let layout = Layout::new::<Self>();
        let allocation = unsafe { alloc(layout) }.cast::<Self>();
        if allocation.is_null() {
            return ptr::null_mut();
        }

        let work = WorkStruct {
            data: 0,
            entry: ListHead {
                next: ptr::null_mut(),
                prev: ptr::null_mut(),
            },
            function: None,
        };
        unsafe {
            allocation.write(Self {
                work,
                display,
                fw_path,
                max_fw_size,
                allow_fallback,
                version: 0,
                dc5_start: 0,
                dc6_allowed_count: 0,
                info: core::array::from_fn(|_| DmcFirmwareInfo::empty()),
            });
        }
        allocation
    }
}

unsafe fn display_to_dmc(display: *mut IntelDisplay) -> *mut IntelDmc {
    if display.is_null() {
        ptr::null_mut()
    } else {
        unsafe { (*display).dmc.dmc.cast() }
    }
}

unsafe fn has_dmc(display: *mut IntelDisplay) -> bool {
    !display.is_null() && unsafe { (*display).info.__runtime_info.has_dmc }
}

unsafe fn runtime_pm_get(display: *mut IntelDisplay) {
    if display.is_null() || !unsafe { (*display).dmc.wakeref.is_null() } {
        return;
    }
    unsafe {
        (*display).dmc.wakeref = intel_display_power_get(display, POWER_DOMAIN_INIT);
    }
}

unsafe fn runtime_pm_put(display: *mut IntelDisplay) {
    if display.is_null() || unsafe { (*display).drm.is_null() } {
        return;
    }
    let wakeref = unsafe { core::mem::replace(&mut (*display).dmc.wakeref, ptr::null_mut()) };
    if !wakeref.is_null() {
        unsafe { intel_display_power_put(display, POWER_DOMAIN_INIT, wakeref) };
    }
}

fn default_firmware(display: *mut IntelDisplay) -> Option<(*const c_char, usize)> {
    if unsafe { platform_alderlake_p(display) } {
        Some((c"i915/adlp_dmc.bin".as_ptr(), DISPLAY_VER13_DMC_MAX_FW_SIZE))
    } else {
        None
    }
}

unsafe fn firmware_override(display: *mut IntelDisplay) -> Option<*const c_char> {
    let path = unsafe { (*display).params.dmc_firmware_path };
    if path.is_null() || unsafe { *path == 0 } {
        None
    } else {
        Some(path.cast_const())
    }
}

unsafe fn is_disabled_by_param(display: *mut IntelDisplay) -> bool {
    let Some(path) = (unsafe { firmware_override(display) }) else {
        return false;
    };
    unsafe { core::ffi::CStr::from_ptr(path) }.to_bytes() == b"/dev/null"
}

// upstream: intel_dmc.c intel_dmc_has_payload()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_has_payload(display: *mut IntelDisplay) -> bool {
    let dmc = unsafe { display_to_dmc(display) };
    !dmc.is_null() && !unsafe { (*dmc).info[DMC_FW_MAIN as usize].payload.is_empty() }
}

fn dmc_event_ctl_disable(value: u32) -> u32 {
    (value & DMC_EVT_CTL_ENABLE)
        | reg_field_prep(DMC_EVT_CTL_TYPE_MASK, DMC_EVT_CTL_TYPE_EDGE_0_1)
        | reg_field_prep(DMC_EVT_CTL_EVENT_ID_MASK, 0)
}

unsafe fn is_event_ctl_reg(display: *mut IntelDisplay, dmc_id: u32, reg: I915RegT) -> bool {
    let start = unsafe { DMC_EVT_CTL(display, dmc_id, 0) }.reg;
    let end = unsafe { DMC_EVT_CTL(display, dmc_id, DMC_EVENT_HANDLER_COUNT_GEN12) }.reg;
    reg.reg >= start && reg.reg < end
}

unsafe fn disable_all_event_handlers(display: *mut IntelDisplay, dmc_id: u32) {
    if unsafe { DISPLAY_VER(display) } < 12 || !unsafe { has_dmc_id_fw(display, dmc_id) } {
        return;
    }
    for handler in 0..DMC_EVENT_HANDLER_COUNT_GEN12 {
        unsafe {
            intel_de_write(
                display,
                DMC_EVT_CTL(display, dmc_id, handler),
                dmc_event_ctl_disable(0),
            );
            intel_de_write(display, DMC_EVT_HTP(display, dmc_id, handler), 0);
        }
    }
}

unsafe fn has_dmc_id_fw(display: *mut IntelDisplay, dmc_id: u32) -> bool {
    let dmc = unsafe { display_to_dmc(display) };
    !dmc.is_null()
        && (dmc_id as usize) < DMC_FW_MAX
        && !unsafe { (*dmc).info[dmc_id as usize].payload.is_empty() }
}

unsafe fn crtc_state_to_display(state: *const IntelCrtcState) -> (*mut IntelDisplay, Pipe) {
    if state.is_null() || unsafe { (*state).uapi.crtc.is_null() } {
        return (ptr::null_mut(), -1);
    }
    let crtc = unsafe { (*state).uapi.crtc.cast::<IntelCrtc>() };
    let drm = unsafe { (*crtc).base.dev };
    if drm.is_null() || unsafe { (*drm).dev_private.is_null() } {
        return (ptr::null_mut(), -1);
    }
    let i915 = unsafe { (*drm).dev_private.cast::<DrmI915Private>() };
    if i915.is_null() {
        return (ptr::null_mut(), -1);
    }
    (unsafe { (*i915).display.cast::<IntelDisplay>() }, unsafe {
        (*crtc).pipe
    })
}

unsafe fn dmc_configure_event(
    display: *mut IntelDisplay,
    dmc_id: u32,
    event_id: u32,
    enable: bool,
) {
    if !unsafe { has_dmc_id_fw(display, dmc_id) } {
        return;
    }
    let dmc = unsafe { display_to_dmc(display) };
    let info = unsafe { &(*dmc).info[dmc_id as usize] };
    for i in 0..info.mmio_count {
        let reg = info.mmioaddr[i];
        if !unsafe { is_event_ctl_reg(display, dmc_id, reg) }
            || ((info.mmiodata[i] & DMC_EVT_CTL_EVENT_ID_MASK) >> 8) != event_id
        {
            continue;
        }
        let value = if enable {
            info.mmiodata[i]
        } else {
            dmc_event_ctl_disable(info.mmiodata[i])
        };
        unsafe { intel_de_write(display, reg, value) };
    }
}

// upstream: intel_dmc.c intel_dmc_enable_pipe()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_enable_pipe(state: *const IntelCrtcState) {
    let (display, pipe) = unsafe { crtc_state_to_display(state) };
    if display.is_null() || pipe < PIPE_A || pipe >= PIPE_A + 4 {
        return;
    }
    let dmc_id = 1 + (pipe - PIPE_A) as u32;
    if !unsafe { has_dmc_id_fw(display, dmc_id) } {
        return;
    }

    // On ADL-P pipe C/D DMC state is not restored by the main DMC, so replay
    // its saved firmware init-MMIO sequence before enabling the pipe block.
    if pipe >= PIPE_A + 2 {
        unsafe { dmc_load_mmio(display, dmc_id) };
    }
    unsafe { intel_de_rmw(display, PIPEDMC_CONTROL(pipe as u32), 0, PIPEDMC_ENABLE) };
}

// upstream: intel_dmc.c intel_dmc_disable_pipe()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_disable_pipe(state: *const IntelCrtcState) {
    let (display, pipe) = unsafe { crtc_state_to_display(state) };
    if display.is_null() || pipe < PIPE_A || pipe >= PIPE_A + 4 {
        return;
    }
    let dmc_id = 1 + (pipe - PIPE_A) as u32;
    if !unsafe { has_dmc_id_fw(display, dmc_id) } {
        return;
    }
    unsafe {
        intel_de_rmw(display, PIPEDMC_CONTROL(pipe as u32), PIPEDMC_ENABLE, 0);
    }
}

// upstream: intel_dmc.c intel_dmc_block_pkgc()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_block_pkgc(display: *mut IntelDisplay, pipe: Pipe, block: bool) {
    if display.is_null() || pipe < PIPE_A || pipe >= PIPE_A + 4 {
        return;
    }
    let bit = if block {
        PIPEDMC_BLOCK_PKGC_SW_BLOCK_PKGC_ALWAYS
    } else {
        0
    };
    unsafe {
        intel_de_rmw(
            display,
            PIPEDMC_BLOCK_PKGC_SW(pipe as u32),
            PIPEDMC_BLOCK_PKGC_SW_BLOCK_PKGC_ALWAYS,
            bit,
        )
    };
}

// upstream: intel_dmc.c intel_dmc_configure_dc_balance_event()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_configure_dc_balance_event(
    display: *mut IntelDisplay,
    pipe: Pipe,
    enable: bool,
) {
    if display.is_null() || pipe < PIPE_A || pipe >= PIPE_A + 4 {
        return;
    }
    unsafe {
        dmc_configure_event(
            display,
            1 + (pipe - PIPE_A) as u32,
            PIPEDMC_EVENT_ADAPTIVE_DCB_TRIGGER,
            enable,
        )
    };
}

// upstream: intel_dmc.c intel_dmc_start_pkgc_exit_at_start_of_undelayed_vblank()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_start_pkgc_exit_at_start_of_undelayed_vblank(
    display: *mut IntelDisplay,
    pipe: Pipe,
    enable: bool,
) {
    if display.is_null() || pipe < PIPE_A || pipe >= PIPE_A + 4 {
        return;
    }
    unsafe {
        dmc_configure_event(
            display,
            1 + (pipe - PIPE_A) as u32,
            PIPEDMC_EVENT_VBLANK,
            enable,
        )
    };
}

unsafe fn dmc_mmiodata(display: *mut IntelDisplay, dmc_id: u32, i: usize) -> u32 {
    let dmc = unsafe { display_to_dmc(display) };
    if dmc.is_null() || dmc_id as usize >= DMC_FW_MAX {
        return 0;
    }
    let info = unsafe { &(*dmc).info[dmc_id as usize] };
    let value = info.mmiodata[i];
    // Linux disables every pipe-DMC event while keeping the enable bit as
    // required by the hardware. ADL-P does not use the Tiger Lake main-DMC
    // event workarounds.
    if dmc_id != DMC_FW_MAIN && unsafe { is_event_ctl_reg(display, dmc_id, info.mmioaddr[i]) } {
        dmc_event_ctl_disable(value)
    } else {
        value
    }
}

unsafe fn dmc_load_mmio(display: *mut IntelDisplay, dmc_id: u32) {
    let dmc = unsafe { display_to_dmc(display) };
    if dmc.is_null() || dmc_id as usize >= DMC_FW_MAX {
        return;
    }
    let info = unsafe { &(*dmc).info[dmc_id as usize] };
    for i in 0..info.mmio_count {
        let reg = info.mmioaddr[i];
        let value = unsafe { dmc_mmiodata(display, dmc_id, i) };
        unsafe { intel_de_write(display, reg, value) };
    }
}

unsafe fn dmc_load_one_program(display: *mut IntelDisplay, dmc_id: u32) {
    let dmc = unsafe { display_to_dmc(display) };
    if dmc.is_null() || dmc_id as usize >= DMC_FW_MAX {
        return;
    }
    unsafe { disable_all_event_handlers(display, dmc_id) };
    let info = unsafe { &(*dmc).info[dmc_id as usize] };

    // Linux keeps this firmware-register upload non-preemptible so another
    // task cannot interrupt the DMC program sequence.
    kernel_guard::NoPreempt::acquire();
    for (i, value) in info.payload.iter().copied().enumerate() {
        unsafe {
            intel_de_write_fw(display, DMC_PROGRAM(info.start_mmioaddr, i as u32), value);
        }
    }
    kernel_guard::NoPreempt::release(());

    unsafe { dmc_load_mmio(display, dmc_id) };
}

unsafe fn assert_dmc_loaded(display: *mut IntelDisplay, dmc_id: u32) {
    if !unsafe { has_dmc_id_fw(display, dmc_id) } {
        return;
    }
    let dmc = unsafe { display_to_dmc(display) };
    let info = unsafe { &(*dmc).info[dmc_id as usize] };
    let found = unsafe { intel_de_read(display, DMC_PROGRAM(info.start_mmioaddr, 0)) };
    if found != info.payload[0] {
        axlog::warn!(
            "i915 DMC {} program storage mismatch: expected {:#x}, got {:#x}",
            dmc_id,
            info.payload[0],
            found
        );
    }
    for i in 0..info.mmio_count {
        let found = unsafe { intel_de_read(display, info.mmioaddr[i]) };
        let expected = unsafe { dmc_mmiodata(display, dmc_id, i) };
        if found != expected {
            axlog::warn!(
                "i915 DMC {} MMIO {:#x} mismatch: expected {:#x}, got {:#x}",
                dmc_id,
                info.mmioaddr[i].reg,
                expected,
                found
            );
        }
    }
}

unsafe fn pipedmc_clock_gating_wa(display: *mut IntelDisplay, enable: bool) {
    if unsafe { DISPLAY_VER(display) } != 13 {
        return;
    }
    if enable {
        for pipe in 0..4 {
            unsafe { intel_de_rmw(display, CLKGATE_DIS_PSL_EXT(pipe), 0, PIPEDMC_GATING_DIS) };
        }
    } else {
        for pipe in 2..4 {
            unsafe { intel_de_rmw(display, CLKGATE_DIS_PSL_EXT(pipe), PIPEDMC_GATING_DIS, 0) };
        }
    }
}

unsafe fn set_dc_state_debugmask(display: *mut IntelDisplay) {
    unsafe {
        intel_de_rmw(
            display,
            DC_STATE_DEBUG,
            0,
            DC_STATE_DEBUG_MASK_CORES | DC_STATE_DEBUG_MASK_MEMORY_UP,
        );
        intel_de_posting_read(display, DC_STATE_DEBUG);
    }
}

// upstream: intel_dmc.c intel_dmc_load_program()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_load_program(display: *mut IntelDisplay) {
    if display.is_null() || !unsafe { intel_dmc_has_payload(display) } {
        return;
    }
    unsafe { pipedmc_clock_gating_wa(display, true) };
    for dmc_id in 0..DMC_FW_MAX as u32 {
        unsafe { dmc_load_one_program(display, dmc_id) };
        unsafe { assert_dmc_loaded(display, dmc_id) };
    }
    unsafe {
        (*display).power.domains.dc_state = 0;
        set_dc_state_debugmask(display);
        pipedmc_clock_gating_wa(display, false);
    }
}

// upstream: intel_dmc.c assert_main_dmc_loaded()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn assert_main_dmc_loaded(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    unsafe { assert_dmc_loaded(display, DMC_FW_MAIN) };
}

unsafe fn display_for_crtc(crtc: *mut IntelCrtc) -> *mut IntelDisplay {
    if crtc.is_null() || unsafe { (*crtc).base.dev.is_null() } {
        return ptr::null_mut();
    }
    let drm = unsafe { (*crtc).base.dev };
    if unsafe { (*drm).dev_private.is_null() } {
        return ptr::null_mut();
    }
    let i915 = unsafe { (*drm).dev_private.cast::<DrmI915Private>() };
    unsafe { (*i915).display.cast::<IntelDisplay>() }
}

// upstream: intel_dmc.c intel_pipedmc_irq_handler()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pipedmc_irq_handler(display: *mut IntelDisplay, pipe: Pipe) {
    if display.is_null() || pipe < PIPE_A || pipe >= PIPE_A + 4 {
        return;
    }
    let status = unsafe { intel_de_read(display, PIPEDMC_STATUS(pipe as u32)) };
    let int_vector = status & PIPEDMC_INT_VECTOR_MASK;
    if int_vector != 0 {
        axlog::error!(
            "i915 pipe-DMC interrupt vector on pipe {}: {:#x}",
            pipe,
            int_vector
        );
    }
}

// upstream: intel_dmc.c intel_pipedmc_enable_event()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pipedmc_enable_event(crtc: *mut IntelCrtc, event: u32) {
    let display = unsafe { display_for_crtc(crtc) };
    if display.is_null() {
        return;
    }
    let pipe = unsafe { (*crtc).pipe };
    if pipe < PIPE_A || pipe >= PIPE_A + 4 {
        return;
    }
    unsafe { dmc_configure_event(display, 1 + (pipe - PIPE_A) as u32, event, true) };
}

// upstream: intel_dmc.c intel_pipedmc_disable_event()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pipedmc_disable_event(crtc: *mut IntelCrtc, event: u32) {
    let display = unsafe { display_for_crtc(crtc) };
    if display.is_null() {
        return;
    }
    let pipe = unsafe { (*crtc).pipe };
    if pipe < PIPE_A || pipe >= PIPE_A + 4 {
        return;
    }
    unsafe { dmc_configure_event(display, 1 + (pipe - PIPE_A) as u32, event, false) };
}

// upstream: intel_dmc.c intel_pipedmc_start_mmioaddr()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pipedmc_start_mmioaddr(crtc: *mut IntelCrtc) -> u32 {
    let display = unsafe { display_for_crtc(crtc) };
    if display.is_null() {
        return 0;
    }
    let pipe = unsafe { (*crtc).pipe };
    if pipe < PIPE_A || pipe >= PIPE_A + 4 {
        return 0;
    }
    let dmc = unsafe { display_to_dmc(display) };
    if dmc.is_null() {
        return 0;
    }
    unsafe { (*dmc).info[(1 + pipe - PIPE_A) as usize].start_mmioaddr }
}

// upstream: intel_dmc.c intel_pipedmc_dcb_enable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pipedmc_dcb_enable(dsb: *mut c_void, crtc: *mut IntelCrtc) {
    let display = unsafe { display_for_crtc(crtc) };
    if display.is_null() {
        return;
    }
    let pipe = unsafe { (*crtc).pipe };
    if pipe < PIPE_A || pipe >= PIPE_A + 4 {
        return;
    }
    unsafe {
        crate::intel_de_upstream::intel_de_write_dsb(
            display,
            dsb,
            PIPEDMC_DCB_CTL(pipe as u32),
            PIPEDMC_ADAPTIVE_DCB_ENABLE,
        )
    };
}

// upstream: intel_dmc.c intel_pipedmc_dcb_disable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pipedmc_dcb_disable(dsb: *mut c_void, crtc: *mut IntelCrtc) {
    let display = unsafe { display_for_crtc(crtc) };
    if display.is_null() {
        return;
    }
    let pipe = unsafe { (*crtc).pipe };
    if pipe < PIPE_A || pipe >= PIPE_A + 4 {
        return;
    }
    unsafe {
        crate::intel_de_upstream::intel_de_write_dsb(display, dsb, PIPEDMC_DCB_CTL(pipe as u32), 0)
    };
}

// upstream: intel_dmc.c intel_dmc_disable_program()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_disable_program(display: *mut IntelDisplay) {
    if display.is_null() || !unsafe { intel_dmc_has_payload(display) } {
        return;
    }
    unsafe { pipedmc_clock_gating_wa(display, true) };
    for dmc_id in 0..DMC_FW_MAX as u32 {
        unsafe { disable_all_event_handlers(display, dmc_id) };
    }
    unsafe { pipedmc_clock_gating_wa(display, false) };
}

// upstream: intel_dmc.c intel_dmc_update_dc6_allowed_count()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_update_dc6_allowed_count(
    display: *mut IntelDisplay,
    start_tracking: bool,
) {
    if display.is_null() || unsafe { DISPLAY_VER(display) } < 14 {
        return;
    }
    let dmc = unsafe { display_to_dmc(display) };
    if dmc.is_null() {
        return;
    }
    let current = unsafe {
        intel_de_read(
            display,
            crate::intel_dmc_regs_upstream::DG1_DMC_DEBUG_DC5_COUNT,
        )
    };
    if !start_tracking {
        let delta = current.wrapping_sub(unsafe { (*dmc).dc5_start });
        unsafe { (*dmc).dc6_allowed_count = (*dmc).dc6_allowed_count.wrapping_add(delta) };
    }
    unsafe { (*dmc).dc5_start = current };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DmcParseError {
    Truncated,
    BadCssHeader,
    BadPackageHeader,
    BadDmcHeader,
    BadMmioCount,
    BadMmioAddress,
    PayloadTooLarge,
    MissingMainProgram,
    Allocation,
}

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    let word: [u8; 4] = bytes.get(offset..end)?.try_into().ok()?;
    Some(u32::from_le_bytes(word))
}

fn valid_mmio_range(dmc_id: usize, header_ver: u8) -> (u32, u32) {
    if header_ver == 1 {
        (DMC_MMIO_START_RANGE, DMC_MMIO_END_RANGE)
    } else if dmc_id == DMC_FW_MAIN as usize {
        (TGL_MAIN_MMIO_START, TGL_MAIN_MMIO_END)
    } else {
        (ADLP_PIPE_MMIO_START, ADLP_PIPE_MMIO_END)
    }
}

fn stepping_matches(fw_step: u8, fw_substep: u8, step: u8, substep: u8) -> bool {
    (fw_substep == b'*' && step == fw_step)
        || (step == fw_step && substep == fw_substep)
        || (step == b'*' && substep == fw_substep)
        || (fw_step == b'*' && fw_substep == b'*')
}

fn dmc_header(
    bytes: &[u8],
    dmc_id: usize,
    max_fw_size: usize,
) -> Result<DmcFirmwareInfo, DmcParseError> {
    if bytes.len() < 24 {
        return Err(DmcParseError::Truncated);
    }
    let header_ver = *bytes.get(5).ok_or(DmcParseError::Truncated)?;
    let header_len = *bytes.get(4).ok_or(DmcParseError::Truncated)? as usize;
    let fw_size = u32_at(bytes, 12).ok_or(DmcParseError::Truncated)? as usize;

    let (fixed_size, mmio_count_offset, mmioaddr_offset, mmiodata_offset, start_mmioaddr) =
        match header_ver {
            1 => (DMC_V1_HEADER_SIZE, 24, 28, 60, DMC_V1_MMIO_START_RANGE),
            3 => (
                DMC_V3_HEADER_SIZE,
                96,
                100,
                180,
                u32_at(bytes, 24).ok_or(DmcParseError::Truncated)?,
            ),
            _ => return Err(DmcParseError::BadDmcHeader),
        };
    if bytes.len() < fixed_size {
        return Err(DmcParseError::Truncated);
    }
    let header_len_bytes = if header_ver == 1 {
        header_len
    } else {
        header_len * 4
    };
    if header_len_bytes != fixed_size {
        return Err(DmcParseError::BadDmcHeader);
    }

    let mmio_count = u32_at(bytes, mmio_count_offset).ok_or(DmcParseError::Truncated)? as usize;
    let mmio_limit = if header_ver == 1 {
        DMC_V1_MAX_MMIO_COUNT
    } else {
        DMC_V3_MAX_MMIO_COUNT
    };
    if mmio_count > mmio_limit {
        return Err(DmcParseError::BadMmioCount);
    }
    let (start, end) = valid_mmio_range(dmc_id, header_ver);
    let mut info = DmcFirmwareInfo::empty();
    for i in 0..mmio_count {
        let address = u32_at(bytes, mmioaddr_offset + i * 4).ok_or(DmcParseError::Truncated)?;
        if address < start || address > end {
            return Err(DmcParseError::BadMmioAddress);
        }
        info.mmioaddr[i] = I915RegT { reg: address };
        info.mmiodata[i] =
            u32_at(bytes, mmiodata_offset + i * 4).ok_or(DmcParseError::Truncated)?;
    }

    let payload_size = fw_size.checked_mul(4).ok_or(DmcParseError::BadDmcHeader)?;
    let payload_end = header_len_bytes
        .checked_add(payload_size)
        .ok_or(DmcParseError::Truncated)?;
    if bytes.len() < payload_end {
        return Err(DmcParseError::Truncated);
    }
    if payload_size > max_fw_size {
        return Err(DmcParseError::PayloadTooLarge);
    }
    if start_mmioaddr & 3 != 0 {
        return Err(DmcParseError::BadDmcHeader);
    }
    info.present = true;
    info.mmio_count = mmio_count;
    info.start_mmioaddr = start_mmioaddr;
    info.payload
        .try_reserve_exact(fw_size)
        .map_err(|_| DmcParseError::Allocation)?;
    for i in 0..fw_size {
        info.payload
            .push(u32_at(bytes, header_len_bytes + i * 4).ok_or(DmcParseError::Truncated)?);
    }
    Ok(info)
}

struct ParsedDmcFirmware {
    version: u32,
    info: [DmcFirmwareInfo; DMC_FW_MAX],
}

fn parse_firmware(
    bytes: &[u8],
    stepping: (u8, u8),
    max_fw_size: usize,
) -> Result<ParsedDmcFirmware, DmcParseError> {
    if bytes.len() < CSS_HEADER_SIZE {
        return Err(DmcParseError::Truncated);
    }
    let css_len = u32_at(bytes, 4).ok_or(DmcParseError::Truncated)? as usize;
    if css_len.checked_mul(4) != Some(CSS_HEADER_SIZE) {
        return Err(DmcParseError::BadCssHeader);
    }
    let version = u32_at(bytes, DMC_VERSION_OFFSET).ok_or(DmcParseError::Truncated)?;
    let pkg = &bytes[CSS_HEADER_SIZE..];
    if pkg.len() < PACKAGE_HEADER_SIZE {
        return Err(DmcParseError::Truncated);
    }
    let pkg_ver = pkg[1];
    let max_entries = match pkg_ver {
        1 => PACKAGE_MAX_FW_INFO_ENTRIES,
        2 => PACKAGE_V2_MAX_FW_INFO_ENTRIES,
        _ => return Err(DmcParseError::BadPackageHeader),
    };
    let package_size = PACKAGE_HEADER_SIZE + max_entries * 16;
    if pkg.len() < package_size || (pkg[0] as usize) * 4 != package_size {
        return Err(DmcParseError::BadPackageHeader);
    }
    let count = (u32_at(pkg, 12).ok_or(DmcParseError::Truncated)? as usize).min(max_entries);
    let mut info: [DmcFirmwareInfo; DMC_FW_MAX] =
        core::array::from_fn(|_| DmcFirmwareInfo::empty());
    let step = if stepping.0 == 0 { b'*' } else { stepping.0 };
    let substep = if stepping.1 == 0 { b'*' } else { stepping.1 };

    // Upstream firmware tables put the most specific stepping entries first;
    // only the first matching image for each DMC id is retained.
    for entry in 0..count {
        let entry_offset = PACKAGE_HEADER_SIZE + entry * 16;
        let dmc_id = if pkg_ver == 1 {
            0
        } else {
            pkg[entry_offset + 1] as usize
        };
        if dmc_id >= DMC_FW_MAX || info[dmc_id].present {
            continue;
        }
        if !stepping_matches(pkg[entry_offset + 2], pkg[entry_offset + 3], step, substep) {
            continue;
        }
        info[dmc_id].present = true;
        info[dmc_id].dmc_offset = u32_at(pkg, entry_offset + 4).ok_or(DmcParseError::Truncated)?;
    }

    let dmc_image_base = CSS_HEADER_SIZE + package_size;
    for dmc_id in 0..DMC_FW_MAX {
        if !info[dmc_id].present {
            continue;
        }
        let offset_bytes = (info[dmc_id].dmc_offset as usize)
            .checked_mul(4)
            .and_then(|offset| dmc_image_base.checked_add(offset))
            .ok_or(DmcParseError::Truncated)?;
        let Some(image) = bytes.get(offset_bytes..) else {
            continue;
        };
        match dmc_header(image, dmc_id, max_fw_size) {
            Ok(parsed) => info[dmc_id] = parsed,
            Err(_) if dmc_id != DMC_FW_MAIN as usize => continue,
            Err(error) => return Err(error),
        }
    }

    if info[DMC_FW_MAIN as usize].payload.is_empty() {
        return Err(DmcParseError::MissingMainProgram);
    }
    Ok(ParsedDmcFirmware { version, info })
}

unsafe extern "C" fn dmc_load_work(work: *mut WorkStruct) {
    if work.is_null() {
        return;
    }
    let dmc = work.cast::<IntelDmc>();
    let display = unsafe { (*dmc).display };
    if display.is_null() || unsafe { (*display).drm.is_null() } {
        return;
    }

    let mut firmware: *const Firmware = ptr::null();
    let mut ret = unsafe {
        tk_linux_firmware_request_nowarn(&mut firmware, (*dmc).fw_path, (*(*display).drm).dev)
    };
    if ret == ERR_ENOENT && unsafe { (*dmc).allow_fallback } {
        let fallback = c"i915/adlp_dmc_ver2_16.bin";
        ret = unsafe {
            tk_linux_firmware_request_nowarn(
                &mut firmware,
                fallback.as_ptr(),
                (*(*display).drm).dev,
            )
        };
    }
    if ret != 0 || firmware.is_null() {
        return;
    }

    let parsed = unsafe {
        let fw = &*firmware;
        if fw.data.is_null() || fw.size == 0 {
            Err(DmcParseError::Truncated)
        } else {
            let data = core::slice::from_raw_parts(fw.data, fw.size);
            let step_name = (*display).info.__runtime_info.step_name;
            parse_firmware(
                data,
                (step_name[0] as u8, step_name[1] as u8),
                (*dmc).max_fw_size,
            )
        }
    };
    if let Ok(parsed) = parsed {
        unsafe {
            (*dmc).version = parsed.version;
            (*dmc).info = parsed.info;
            intel_dmc_load_program(display);
            runtime_pm_put(display);
        }
    }
    unsafe { tk_linux_firmware_release(firmware) };
}

// upstream: intel_dmc.c intel_dmc_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_init(display: *mut IntelDisplay) {
    if display.is_null() || !unsafe { has_dmc(display) } {
        return;
    }
    if !unsafe { (*display).dmc.dmc.is_null() } {
        return;
    }

    // The GT probe is deferred until rootfs-ready, so the LinuxKPI firmware
    // request in the work callback is not replaced by a boot-time fake success.
    unsafe { runtime_pm_get(display) };
    let override_path = unsafe { firmware_override(display) };
    if unsafe { is_disabled_by_param(display) } {
        return;
    }
    let default = default_firmware(display);
    let (default_path, max_fw_size) = default.unwrap_or((ptr::null(), 0));
    let fw_path = override_path.unwrap_or(default_path);
    if fw_path.is_null() {
        return;
    }

    let dmc = IntelDmc::new(display, fw_path, max_fw_size, override_path.is_none());
    if dmc.is_null() {
        return;
    }
    unsafe {
        (*display).dmc.dmc = dmc.cast();
        INIT_WORK_C(&mut (*dmc).work, dmc_load_work);
        if (*display).wq.unordered.is_null() {
            (*display).dmc.dmc = ptr::null_mut();
            drop_dmc(dmc);
            return;
        }
        if try_queue_work((*display).wq.unordered, &mut (*dmc).work).is_err() {
            (*display).dmc.dmc = ptr::null_mut();
            drop_dmc(dmc);
        }
    }
}

unsafe fn drop_dmc(dmc: *mut IntelDmc) {
    if dmc.is_null() {
        return;
    }
    unsafe {
        ptr::drop_in_place(dmc);
        dealloc(dmc.cast(), Layout::new::<IntelDmc>());
    }
}

// upstream: intel_dmc.c intel_dmc_suspend()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_suspend(display: *mut IntelDisplay) {
    if display.is_null() || !unsafe { has_dmc(display) } {
        return;
    }
    let dmc = unsafe { display_to_dmc(display) };
    if !dmc.is_null() {
        unsafe { flush_work(&mut (*dmc).work) };
    }
    if !unsafe { intel_dmc_has_payload(display) } {
        unsafe { runtime_pm_put(display) };
    }
}

// upstream: intel_dmc.c intel_dmc_wait_fw_load()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_wait_fw_load(display: *mut IntelDisplay) {
    if display.is_null() || !unsafe { has_dmc(display) } {
        return;
    }
    let dmc = unsafe { display_to_dmc(display) };
    if !dmc.is_null() {
        unsafe { flush_work(&mut (*dmc).work) };
    }
}

// upstream: intel_dmc.c intel_dmc_resume()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_resume(display: *mut IntelDisplay) {
    if display.is_null() || !unsafe { has_dmc(display) } {
        return;
    }
    if !unsafe { intel_dmc_has_payload(display) } {
        unsafe { runtime_pm_get(display) };
    }
}

// upstream: intel_dmc.c intel_dmc_fini()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_fini(display: *mut IntelDisplay) {
    if display.is_null() || !unsafe { has_dmc(display) } {
        return;
    }
    unsafe { intel_dmc_suspend(display) };
    let dmc = unsafe { display_to_dmc(display) };
    if !dmc.is_null() {
        unsafe {
            (*display).dmc.dmc = ptr::null_mut();
            drop_dmc(dmc);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn adlp_fixture(mmio_addr: u32) -> Vec<u8> {
        let package_size = PACKAGE_HEADER_SIZE + PACKAGE_V2_MAX_FW_INFO_ENTRIES * 16;
        let dmc_offset = CSS_HEADER_SIZE + package_size;
        let mut bytes = vec![0u8; dmc_offset + DMC_V3_HEADER_SIZE + 4];
        put_u32(&mut bytes, 4, (CSS_HEADER_SIZE / 4) as u32);
        put_u32(&mut bytes, DMC_VERSION_OFFSET, 0x0002_0016);
        bytes[CSS_HEADER_SIZE] = (package_size / 4) as u8;
        bytes[CSS_HEADER_SIZE + 1] = 2;
        put_u32(&mut bytes, CSS_HEADER_SIZE + 12, 1);
        let fw_info = CSS_HEADER_SIZE + PACKAGE_HEADER_SIZE;
        bytes[fw_info + 1] = 0; // DMC_FW_MAIN
        bytes[fw_info + 2] = b'*';
        bytes[fw_info + 3] = b'*';
        put_u32(&mut bytes, fw_info + 4, 0);

        put_u32(&mut bytes, dmc_offset, 0x4040_3e3e);
        bytes[dmc_offset + 4] = (DMC_V3_HEADER_SIZE / 4) as u8;
        bytes[dmc_offset + 5] = 3;
        put_u32(&mut bytes, dmc_offset + 12, 1);
        put_u32(&mut bytes, dmc_offset + 24, 0x8f000);
        put_u32(&mut bytes, dmc_offset + 96, 1);
        put_u32(&mut bytes, dmc_offset + 100, mmio_addr);
        put_u32(&mut bytes, dmc_offset + 180, 0x1234);
        put_u32(&mut bytes, dmc_offset + DMC_V3_HEADER_SIZE, 0xfeed_beef);
        bytes
    }

    #[test]
    fn parses_adlp_css_package_v3_and_program_payload() {
        let parsed = parse_firmware(&adlp_fixture(0x8f100), (b'A', b'0'), 0x20000).unwrap();
        assert_eq!(parsed.version, 0x0002_0016);
        let main = &parsed.info[DMC_FW_MAIN as usize];
        assert_eq!(main.mmio_count, 1);
        assert_eq!(main.mmioaddr[0].reg, 0x8f100);
        assert_eq!(main.mmiodata[0], 0x1234);
        assert_eq!(main.start_mmioaddr, 0x8f000);
        assert_eq!(main.payload, [0xfeed_beef]);
    }

    #[test]
    fn rejects_truncated_firmware_and_out_of_range_init_mmio() {
        let mut truncated = adlp_fixture(0x8f100);
        truncated.pop();
        assert!(matches!(
            parse_firmware(&truncated, (b'A', b'0'), 0x20000),
            Err(DmcParseError::Truncated)
        ));
        assert!(matches!(
            parse_firmware(&adlp_fixture(0x70000), (b'A', b'0'), 0x20000),
            Err(DmcParseError::BadMmioAddress)
        ));
    }

    #[test]
    fn stepping_table_selects_matching_image() {
        assert!(stepping_matches(b'A', b'*', b'A', b'0'));
        assert!(stepping_matches(b'*', b'*', b'B', b'1'));
        assert!(!stepping_matches(b'B', b'0', b'A', b'0'));
    }
}
