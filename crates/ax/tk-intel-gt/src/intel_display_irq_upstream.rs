// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// Translation of the Alder Lake-P / display-IP 13 path in Linux v7.2.3
// drivers/gpu/drm/i915/display/intel_display_irq.c.
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::{ffi::c_ulong, mem::offset_of, ptr};

use tk_drm_upstream::{
    drm_crtc_types::DrmCrtc,
    drm_mode_config_types::DrmModeConfig,
    drm_plane_types::DrmPlane,
    drm_vblank_types::DrmPendingVblankEvent,
    drm_vblank_upstream::{
        drm_crtc_handle_vblank, drm_crtc_send_vblank_event, drm_crtc_vblank_restore,
    },
};
use tk_linux_kapi::{
    locks::{
        spin_lock, spin_lock_init, spin_lock_irq, spin_lock_irqsave, spin_unlock, spin_unlock_irq,
        spin_unlock_irqrestore,
    },
    types::WorkStruct,
    workqueue::{INIT_WORK_C, queue_work, system_dfl_wq},
};

use crate::{
    intel_de_upstream::{intel_de_posting_read, intel_de_read, intel_de_rmw, intel_de_write},
    intel_display_core_types_upstream::{IntelDisplay, IntelDisplayIrq},
    intel_display_device_upstream::intel_display_device_present,
    intel_display_irq_types_upstream::IntelDisplayIrqState,
    intel_display_limits_types_upstream::{Pipe, PlaneId, Transcoder},
    intel_display_power_types_upstream::{
        IntelDisplayPowerDomain, POWER_DOMAIN_PIPE_A, POWER_DOMAIN_PIPE_B, POWER_DOMAIN_PIPE_C,
        POWER_DOMAIN_PIPE_D, POWER_DOMAIN_TRANSCODER_A, POWER_DOMAIN_TRANSCODER_B,
        POWER_DOMAIN_TRANSCODER_C, POWER_DOMAIN_TRANSCODER_D,
    },
    intel_display_reg_defs_upstream::{INTEL_IRQ_REGS, IntelIrqRegs},
    intel_display_regs_upstream::*,
    intel_display_rpm_upstream::{
        intel_display_rpm_assert_block, intel_display_rpm_assert_unblock,
    },
    intel_display_types_upstream::{IntelCrtc, IntelPlane, IntelPlaneError},
    intel_parent_upstream::{intel_parent_irq_enabled, intel_parent_irq_synchronize},
    intel_pch_types_upstream::{PCH_ICP, PCH_NOP},
    intel_psr_regs_upstream::{EDP_PSR_IIR, EDP_PSR_IMR, TRANS_PSR_IIR, TRANS_PSR_IMR},
    intel_workarounds_types_upstream::I915RegT,
};

const GEN8_DE_PCH_IRQ: u32 = 1 << 23;
const GEN8_DE_MISC_IRQ: u32 = 1 << 22;
const GEN8_DE_PORT_IRQ: u32 = 1 << 20;
const GEN11_DE_HPD_IRQ: u32 = 1 << 21;
const GEN8_DE_PIPE_IRQ_SHIFT: u32 = 16;
const GEN8_DE_EDP_PSR: u32 = 1 << 19;
const GEN11_GU_MISC_IRQ: u32 = 1 << 29;
const GEN11_GU_MISC_GSE: u32 = 1 << 27;
const GEN11_GU_MISC_IIR: I915RegT = I915RegT { reg: 0x444f8 };
const MAX_PIPES: usize = 4;
const MAX_PLANES: usize = 8;

#[repr(C)]
struct IntelDisplayIrqFuncs {
    reset: Option<unsafe extern "C" fn(*mut IntelDisplay)>,
    postinstall: Option<unsafe extern "C" fn(*mut IntelDisplay)>,
    ack: Option<unsafe extern "C" fn(*mut IntelDisplay, *mut IntelDisplayIrqState)>,
    handler: Option<unsafe extern "C" fn(*mut IntelDisplay, *const IntelDisplayIrqState) -> bool>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum IrqResetOp {
    Write(I915RegT, u32),
    PostingRead(I915RegT),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Gen11IrqStep {
    WriteDisplayCtl(u32),
    Dispatch(u32),
}

fn with_gen11_irq_masked(
    master_ctl: u32,
    mut write_display_ctl: impl FnMut(u32),
    mut dispatch: impl FnMut(u32),
) {
    write_display_ctl(0);
    dispatch(master_ctl);
    write_display_ctl(GEN11_DISPLAY_IRQ_ENABLE);
}

const fn irq_reset_ops(regs: IntelIrqRegs) -> [IrqResetOp; 7] {
    [
        IrqResetOp::Write(regs.imr, u32::MAX),
        IrqResetOp::PostingRead(regs.imr),
        IrqResetOp::Write(regs.ier, 0),
        IrqResetOp::Write(regs.iir, u32::MAX),
        IrqResetOp::PostingRead(regs.iir),
        IrqResetOp::Write(regs.iir, u32::MAX),
        IrqResetOp::PostingRead(regs.iir),
    ]
}

unsafe extern "C" {
    // These callbacks are owned by the dependent display batches. They are
    // wired only after those batches land; interrupt masks remain disabled
    // until the parent IRQ provider installs and enables the handler.
    fn intel_hotplug_irq_init(display: *mut IntelDisplay);
    fn gen11_hpd_irq_handler(display: *mut IntelDisplay, iir: u32);
    fn intel_dp_aux_irq_handler(display: *mut IntelDisplay);
    fn intel_gmbus_irq_handler(display: *mut IntelDisplay);
    fn icp_irq_handler(display: *mut IntelDisplay, pch_iir: u32);
    fn intel_pipedmc_irq_handler(display: *mut IntelDisplay, pipe: Pipe);
    fn intel_dsb_irq_handler(display: *mut IntelDisplay, pipe: Pipe, dsb_id: i32);
    fn intel_cpu_fifo_underrun_irq_handler(display: *mut IntelDisplay, pipe: Pipe);
    fn intel_opregion_asle_intr(display: *mut IntelDisplay);
}

#[inline]
unsafe fn display_ver(display: *const IntelDisplay) -> u16 {
    unsafe { (*display).info.__runtime_info.ip.ver }
}

#[inline]
unsafe fn pipe_mask(display: *const IntelDisplay) -> u8 {
    unsafe { (*display).info.__runtime_info.pipe_mask }
}

#[inline]
unsafe fn cpu_transcoder_mask(display: *const IntelDisplay) -> u8 {
    unsafe { (*display).info.__runtime_info.cpu_transcoder_mask }
}

#[inline]
unsafe fn pipe_power_domain(pipe: Pipe) -> IntelDisplayPowerDomain {
    match pipe {
        0 => POWER_DOMAIN_PIPE_A,
        1 => POWER_DOMAIN_PIPE_B,
        2 => POWER_DOMAIN_PIPE_C,
        3 => POWER_DOMAIN_PIPE_D,
        _ => POWER_DOMAIN_PIPE_A,
    }
}

#[inline]
unsafe fn transcoder_power_domain(transcoder: Transcoder) -> IntelDisplayPowerDomain {
    match transcoder {
        0 => POWER_DOMAIN_TRANSCODER_A,
        1 => POWER_DOMAIN_TRANSCODER_B,
        2 => POWER_DOMAIN_TRANSCODER_C,
        3 => POWER_DOMAIN_TRANSCODER_D,
        _ => POWER_DOMAIN_TRANSCODER_A,
    }
}

unsafe fn for_each_pipe(display: *mut IntelDisplay, mut f: impl FnMut(Pipe)) {
    if display.is_null() {
        return;
    }
    let mask = unsafe { pipe_mask(display) };
    for pipe in 0..MAX_PIPES {
        if mask & (1 << pipe) != 0 {
            f(pipe as Pipe);
        }
    }
}

unsafe fn find_crtc_for_pipe(display: *mut IntelDisplay, pipe: Pipe) -> *mut IntelCrtc {
    if display.is_null() {
        return ptr::null_mut();
    }
    let head = unsafe { ptr::addr_of_mut!((*display).pipe_list) };
    let mut node = unsafe { (*head).next };
    while !ptr::eq(node, head) {
        let crtc = unsafe {
            node.cast::<u8>()
                .sub(offset_of!(IntelCrtc, pipe_head))
                .cast::<IntelCrtc>()
        };
        if unsafe { (*crtc).pipe == pipe } {
            return crtc;
        }
        node = unsafe { (*node).next };
    }
    ptr::null_mut()
}

unsafe fn irq_reset(display: *mut IntelDisplay, regs: IntelIrqRegs) {
    for operation in irq_reset_ops(regs) {
        match operation {
            IrqResetOp::Write(reg, value) => unsafe { intel_de_write(display, reg, value) },
            IrqResetOp::PostingRead(reg) => unsafe { intel_de_posting_read(display, reg) },
        }
    }
}

unsafe fn assert_iir_is_zero(display: *mut IntelDisplay, reg: I915RegT) {
    if unsafe { intel_de_read(display, reg) } == 0 {
        return;
    }
    // A stale W1C status is cleared twice because the IIR may latch a second
    // event while the first one is acknowledged.
    unsafe { intel_de_write(display, reg, u32::MAX) };
    unsafe { intel_de_posting_read(display, reg) };
    unsafe { intel_de_write(display, reg, u32::MAX) };
    unsafe { intel_de_posting_read(display, reg) };
}

unsafe fn irq_init(display: *mut IntelDisplay, regs: IntelIrqRegs, imr: u32, ier: u32) {
    unsafe { assert_iir_is_zero(display, regs.iir) };
    unsafe { intel_de_write(display, regs.ier, ier) };
    unsafe { intel_de_write(display, regs.imr, imr) };
    unsafe { intel_de_posting_read(display, regs.imr) };
}

#[inline]
unsafe fn pch_type(display: *const IntelDisplay) -> i32 {
    unsafe { (*display).pch_type }
}

#[inline]
unsafe fn has_dsb(display: *const IntelDisplay) -> bool {
    let info = unsafe { (*display).info.__device_info };
    !info.is_null() && unsafe { (*info)._bitfields_46[0] & (1 << 5) != 0 }
}

#[inline]
fn gen8_pipe_irq(pipe: Pipe) -> u32 {
    1 << (GEN8_DE_PIPE_IRQ_SHIFT + pipe as u32)
}

fn gen8_de_pipe_flip_done_mask(display_ver: u16) -> u32 {
    if display_ver >= 9 {
        GEN9_PIPE_PLANE1_FLIP_DONE
    } else {
        GEN8_PIPE_PRIMARY_FLIP_DONE
    }
}

fn gen8_de_port_aux_mask(display_ver: u16) -> u32 {
    if display_ver >= 20 {
        0
    } else if display_ver >= 14 {
        TGL_DE_PORT_AUX_DDIA | TGL_DE_PORT_AUX_DDIB
    } else if display_ver >= 13 {
        TGL_DE_PORT_AUX_DDIA
            | TGL_DE_PORT_AUX_DDIB
            | TGL_DE_PORT_AUX_DDIC
            | XELPD_DE_PORT_AUX_DDID
            | XELPD_DE_PORT_AUX_DDIE
            | TGL_DE_PORT_AUX_USBC1
            | TGL_DE_PORT_AUX_USBC2
            | TGL_DE_PORT_AUX_USBC3
            | TGL_DE_PORT_AUX_USBC4
    } else if display_ver >= 12 {
        TGL_DE_PORT_AUX_DDIA
            | TGL_DE_PORT_AUX_DDIB
            | TGL_DE_PORT_AUX_DDIC
            | TGL_DE_PORT_AUX_USBC1
            | TGL_DE_PORT_AUX_USBC2
            | TGL_DE_PORT_AUX_USBC3
            | TGL_DE_PORT_AUX_USBC4
            | TGL_DE_PORT_AUX_USBC5
            | TGL_DE_PORT_AUX_USBC6
    } else {
        GEN8_AUX_CHANNEL_A | GEN9_AUX_CHANNEL_B | GEN9_AUX_CHANNEL_C | GEN9_AUX_CHANNEL_D
    }
}

fn gen8_de_pipe_fault_mask(display_ver: u16) -> u32 {
    if display_ver >= 13 {
        GEN12_PIPEDMC_FAULT
            | GEN9_PIPE_CURSOR_FAULT
            | GEN11_PIPE_PLANE5_FAULT
            | GEN9_PIPE_PLANE4_FAULT
            | GEN9_PIPE_PLANE3_FAULT
            | GEN9_PIPE_PLANE2_FAULT
            | GEN9_PIPE_PLANE1_FAULT
    } else if display_ver >= 12 {
        GEN12_PIPEDMC_FAULT
            | GEN9_PIPE_CURSOR_FAULT
            | GEN11_PIPE_PLANE5_FAULT
            | GEN9_PIPE_PLANE4_FAULT
            | GEN9_PIPE_PLANE3_FAULT
            | GEN9_PIPE_PLANE2_FAULT
            | GEN9_PIPE_PLANE1_FAULT
    } else if display_ver >= 11 {
        GEN9_PIPE_CURSOR_FAULT
            | GEN11_PIPE_PLANE5_FAULT
            | GEN9_PIPE_PLANE4_FAULT
            | GEN9_PIPE_PLANE3_FAULT
            | GEN9_PIPE_PLANE2_FAULT
            | GEN9_PIPE_PLANE1_FAULT
    } else {
        0
    }
}

unsafe fn gen11_display_irq_reset(display: *mut IntelDisplay) {
    unsafe { intel_de_write(display, GEN11_DISPLAY_INT_CTL, 0) };
    if unsafe { display_ver(display) } >= 12 {
        for trans in 0..4u32 {
            if unsafe { cpu_transcoder_mask(display) } & (1 << trans) == 0 {
                continue;
            }
            let domain = unsafe { transcoder_power_domain(trans as Transcoder) };
            if unsafe {
                crate::intel_display_power_upstream::intel_display_power_is_enabled(display, domain)
            } {
                unsafe { intel_de_write(display, TRANS_PSR_IMR(display, trans), u32::MAX) };
                unsafe { intel_de_write(display, TRANS_PSR_IIR(display, trans), u32::MAX) };
            }
        }
    } else {
        unsafe { intel_de_write(display, EDP_PSR_IMR, u32::MAX) };
        unsafe { intel_de_write(display, EDP_PSR_IIR, u32::MAX) };
    }
    unsafe {
        for_each_pipe(display, |pipe| {
            let domain = pipe_power_domain(pipe);
            if crate::intel_display_power_upstream::intel_display_power_is_enabled(display, domain)
            {
                irq_reset(display, GEN8_DE_PIPE_IRQ_REGS(pipe as u32));
            }
        });
        irq_reset(
            display,
            INTEL_IRQ_REGS(GEN8_DE_PORT_IMR, GEN8_DE_PORT_IER, GEN8_DE_PORT_IIR),
        );
        irq_reset(
            display,
            INTEL_IRQ_REGS(GEN8_DE_MISC_IMR, GEN8_DE_MISC_IER, GEN8_DE_MISC_IIR),
        );
        irq_reset(
            display,
            INTEL_IRQ_REGS(GEN11_DE_HPD_IMR, GEN11_DE_HPD_IER, GEN11_DE_HPD_IIR),
        );
        if pch_type(display) >= PCH_ICP {
            irq_reset(display, INTEL_IRQ_REGS(SDEIMR, SDEIER, SDEIIR));
        }
    }
}

unsafe fn gen8_de_irq_postinstall(display: *mut IntelDisplay) {
    if display.is_null() || !unsafe { intel_display_device_present(display) } {
        return;
    }
    if unsafe { pch_type(display) >= PCH_ICP && pch_type(display) != PCH_NOP } {
        unsafe { icp_irq_postinstall(display) };
    }

    let ver = unsafe { display_ver(display) };
    let mut pipe_masked = gen8_de_pipe_fault_mask(ver) | GEN8_PIPE_CDCLK_CRC_DONE;
    let port_masked = gen8_de_port_aux_mask(ver);
    // PSR is in the manifest's eDP-only deferred set. Do not enable its
    // interrupt without the matching PSR state machine in the kernel.
    let misc_enabled = 0;
    if unsafe { has_dsb(display) } {
        pipe_masked |= GEN12_DSB_INT(0) | GEN12_DSB_INT(1) | GEN12_DSB_INT(2);
    }
    let pipe_enables =
        pipe_masked | GEN8_PIPE_VBLANK | GEN8_PIPE_FIFO_UNDERRUN | gen8_de_pipe_flip_done_mask(ver);

    if ver >= 12 {
        for trans in 0..4u32 {
            if unsafe { cpu_transcoder_mask(display) } & (1 << trans) == 0 {
                continue;
            }
            let domain = unsafe { transcoder_power_domain(trans as Transcoder) };
            if unsafe {
                crate::intel_display_power_upstream::intel_display_power_is_enabled(display, domain)
            } {
                unsafe { assert_iir_is_zero(display, TRANS_PSR_IIR(display, trans)) };
            }
        }
    } else {
        unsafe { assert_iir_is_zero(display, EDP_PSR_IIR) };
    }

    unsafe {
        for_each_pipe(display, |pipe| {
            let index = pipe as usize;
            (*display).irq.de_pipe_imr_mask[index] = !pipe_masked;
            let domain = pipe_power_domain(pipe);
            if crate::intel_display_power_upstream::intel_display_power_is_enabled(display, domain)
            {
                irq_init(
                    display,
                    GEN8_DE_PIPE_IRQ_REGS(pipe as u32),
                    (*display).irq.de_pipe_imr_mask[index],
                    pipe_enables,
                );
            }
        });
        irq_init(
            display,
            INTEL_IRQ_REGS(GEN8_DE_PORT_IMR, GEN8_DE_PORT_IER, GEN8_DE_PORT_IIR),
            !port_masked,
            port_masked,
        );
        irq_init(
            display,
            INTEL_IRQ_REGS(GEN8_DE_MISC_IMR, GEN8_DE_MISC_IER, GEN8_DE_MISC_IIR),
            !misc_enabled,
            misc_enabled,
        );
        if (11..14).contains(&ver) {
            irq_init(
                display,
                INTEL_IRQ_REGS(GEN11_DE_HPD_IMR, GEN11_DE_HPD_IER, GEN11_DE_HPD_IIR),
                0xffff_ffff,
                GEN11_DE_TC_HOTPLUG_MASK | GEN11_DE_TBT_HOTPLUG_MASK,
            );
        }
    }
}

unsafe fn icp_irq_postinstall(display: *mut IntelDisplay) {
    unsafe {
        irq_init(
            display,
            INTEL_IRQ_REGS(SDEIMR, SDEIER, SDEIIR),
            !SDE_GMBUS_ICP,
            u32::MAX,
        )
    };
}

unsafe fn gen11_de_irq_postinstall(display: *mut IntelDisplay) {
    unsafe { gen8_de_irq_postinstall(display) };
    unsafe { intel_de_write(display, GEN11_DISPLAY_INT_CTL, GEN11_DISPLAY_IRQ_ENABLE) };
}

unsafe fn gen8_display_irq_reset(display: *mut IntelDisplay) {
    if display.is_null() || unsafe { display_ver(display) } >= 11 {
        return;
    }
    unsafe {
        intel_de_write(display, EDP_PSR_IMR, u32::MAX);
        intel_de_write(display, EDP_PSR_IIR, u32::MAX);
        for_each_pipe(display, |pipe| {
            let domain = pipe_power_domain(pipe);
            if crate::intel_display_power_upstream::intel_display_power_is_enabled(display, domain)
            {
                irq_reset(display, GEN8_DE_PIPE_IRQ_REGS(pipe as u32));
            }
        });
        irq_reset(
            display,
            INTEL_IRQ_REGS(GEN8_DE_PORT_IMR, GEN8_DE_PORT_IER, GEN8_DE_PORT_IIR),
        );
        irq_reset(
            display,
            INTEL_IRQ_REGS(GEN8_DE_MISC_IMR, GEN8_DE_MISC_IER, GEN8_DE_MISC_IIR),
        );
    }
}

unsafe extern "C" fn gen11_display_irq_handler(
    display: *mut IntelDisplay,
    _state: *const IntelDisplayIrqState,
) -> bool {
    if display.is_null() {
        return true;
    }
    unsafe { intel_display_rpm_assert_block(display) };
    let master_ctl = unsafe { intel_de_read(display, GEN11_DISPLAY_INT_CTL) };
    // Mask before reading/clearing child IIRs to prevent reentry. Restore only
    // after every child source has been acknowledged and dispatched.
    with_gen11_irq_masked(
        master_ctl,
        |value| unsafe { intel_de_write(display, GEN11_DISPLAY_INT_CTL, value) },
        |value| unsafe { gen8_de_irq_handler(display, value) },
    );
    unsafe { intel_display_rpm_assert_unblock(display) };
    true
}

unsafe extern "C" fn gen8_display_irq_handler(
    display: *mut IntelDisplay,
    state: *const IntelDisplayIrqState,
) -> bool {
    if display.is_null() || state.is_null() {
        return true;
    }
    unsafe { gen8_de_irq_handler(display, (*state).master_ctl) };
    true
}

unsafe fn gen8_read_and_ack_pch_irqs(display: *mut IntelDisplay) -> u32 {
    let pch_iir = unsafe { intel_de_read(display, SDEIIR) };
    if pch_iir != 0 {
        // ADL-P's PCH_ADP path has no PICA interrupt block; acknowledge the
        // PCH IIR once with its W1C value after snapshotting all sources.
        unsafe { intel_de_write(display, SDEIIR, pch_iir) };
    }
    pch_iir
}

unsafe fn find_plane(display: *mut IntelDisplay, pipe: Pipe, id: PlaneId) -> *mut IntelPlane {
    if display.is_null() || unsafe { (*display).drm.is_null() } {
        return ptr::null_mut();
    }
    let config: *mut DrmModeConfig = unsafe { ptr::addr_of_mut!((*(*display).drm).mode_config) };
    let head = unsafe { ptr::addr_of_mut!((*config).plane_list) };
    let mut node = unsafe { (*head).next };
    while !ptr::eq(node, head) {
        let plane = unsafe {
            node.cast::<u8>()
                .sub(offset_of!(DrmPlane, head))
                .cast::<IntelPlane>()
        };
        if unsafe { (*plane).pipe == pipe && (*plane).id == id } {
            return plane;
        }
        node = unsafe { (*node).next };
    }
    ptr::null_mut()
}

unsafe fn handle_plane_fault(
    display: *mut IntelDisplay,
    crtc: *mut IntelCrtc,
    id: PlaneId,
) -> bool {
    let plane = unsafe { find_plane(display, (*crtc).pipe, id) };
    if plane.is_null() {
        return false;
    }
    let capture = unsafe { (*plane).capture_error };
    if let Some(capture) = capture {
        let mut error = IntelPlaneError {
            ctl: 0,
            surf: 0,
            surflive: 0,
        };
        unsafe { capture(crtc, plane, &mut error) };
        axlog::error!(
            "i915 display plane fault: pipe={} plane={} ctl={:#x} surf={:#x} live={:#x}",
            unsafe { (*crtc).pipe },
            id,
            error.ctl,
            error.surf,
            error.surflive,
        );
        true
    } else {
        false
    }
}

unsafe fn pipe_fault_irq_handler(display: *mut IntelDisplay, pipe: Pipe, faults: u32) {
    let crtc = unsafe { find_crtc_for_pipe(display, pipe) };
    if crtc.is_null() {
        axlog::error!(
            "i915 display pipe fault with no CRTC: pipe={} faults={:#x}",
            pipe,
            faults
        );
        return;
    }
    let mut unhandled = faults;
    if unhandled & GEN12_PIPEDMC_FAULT != 0 {
        axlog::error!("i915 display PIPEDMC fault: pipe={}", pipe);
        unhandled &= !GEN12_PIPEDMC_FAULT;
    }
    for (bit, id) in [
        (GEN11_PIPE_PLANE5_FAULT, 4),
        (GEN9_PIPE_PLANE4_FAULT, 3),
        (GEN9_PIPE_PLANE3_FAULT, 2),
        (GEN9_PIPE_PLANE2_FAULT, 1),
        (GEN9_PIPE_PLANE1_FAULT, 0),
        (GEN9_PIPE_CURSOR_FAULT, 7),
    ] {
        if unhandled & bit == 0 {
            continue;
        }
        if unsafe { handle_plane_fault(display, crtc, id) } {
            unhandled &= !bit;
        }
    }
    if unhandled != 0 {
        axlog::error!(
            "i915 display unhandled plane faults: pipe={} faults={:#x}",
            pipe,
            unhandled
        );
    }
}

unsafe fn flip_done_handler(display: *mut IntelDisplay, pipe: Pipe) {
    let crtc = unsafe { find_crtc_for_pipe(display, pipe) };
    if crtc.is_null() {
        return;
    }
    let drm = unsafe { (*display).drm };
    if drm.is_null() {
        return;
    }
    unsafe { spin_lock(&mut (*drm).event_lock) };
    let event: *mut DrmPendingVblankEvent = unsafe { (*crtc).flip_done_event };
    if !event.is_null() {
        unsafe { drm_crtc_send_vblank_event(ptr::addr_of_mut!((*crtc).base), event) };
        unsafe { (*crtc).flip_done_event = ptr::null_mut() };
    }
    unsafe { spin_unlock(&mut (*drm).event_lock) };
}

unsafe fn gen8_de_misc_irq_handler(display: *mut IntelDisplay, iir: u32) {
    // On the ADL-P configuration, DSB and watermark-overlap bits are not in
    // this block. eDP PSR was intentionally left masked until its deferred
    // state machine is ported.
    if iir & GEN8_DE_EDP_PSR != 0 {
        axlog::warn!(
            "i915 display received disabled eDP PSR interrupt: {:#x}",
            iir
        );
    } else {
        axlog::warn!("i915 display unexpected DE misc interrupt: {:#x}", iir);
    }
    let _ = display;
}

unsafe fn gen8_de_irq_handler(display: *mut IntelDisplay, master_ctl: u32) {
    if display.is_null() {
        return;
    }
    if master_ctl & GEN8_DE_MISC_IRQ != 0 {
        let iir = unsafe { intel_de_read(display, GEN8_DE_MISC_IIR) };
        if iir != 0 {
            unsafe { intel_de_write(display, GEN8_DE_MISC_IIR, iir) };
            unsafe { gen8_de_misc_irq_handler(display, iir) };
        } else {
            axlog::error!("i915 display master interrupt lied (DE MISC)");
        }
    }
    if unsafe { display_ver(display) } >= 11 && master_ctl & GEN11_DE_HPD_IRQ != 0 {
        let iir = unsafe { intel_de_read(display, GEN11_DE_HPD_IIR) };
        if iir != 0 {
            unsafe { intel_de_write(display, GEN11_DE_HPD_IIR, iir) };
            unsafe { gen11_hpd_irq_handler(display, iir) };
        } else {
            axlog::error!("i915 display master interrupt lied (DE HPD)");
        }
    }
    if master_ctl & GEN8_DE_PORT_IRQ != 0 {
        let iir = unsafe { intel_de_read(display, GEN8_DE_PORT_IIR) };
        if iir != 0 {
            unsafe { intel_de_write(display, GEN8_DE_PORT_IIR, iir) };
            let aux_mask = gen8_de_port_aux_mask(unsafe { display_ver(display) });
            let mut handled = 0;
            if iir & aux_mask != 0 {
                unsafe { intel_dp_aux_irq_handler(display) };
                handled |= aux_mask;
            }
            // DSI TE sources are not enabled by this ADL-P port.
            if iir & (DSI0_TE | DSI1_TE) != 0 {
                axlog::warn!(
                    "i915 display received disabled DSI TE interrupt: {:#x}",
                    iir
                );
                handled |= DSI0_TE | DSI1_TE;
            }
            if iir & !handled != 0 {
                axlog::error!(
                    "i915 display unexpected DE port interrupt: {:#x}",
                    iir & !handled
                );
            }
        } else {
            axlog::error!("i915 display master interrupt lied (DE PORT)");
        }
    }
    unsafe {
        for_each_pipe(display, |pipe| {
            if master_ctl & gen8_pipe_irq(pipe) == 0 {
                return;
            }
            let iir = intel_de_read(display, GEN8_DE_PIPE_IIR(pipe as u32));
            if iir == 0 {
                axlog::error!("i915 display master interrupt lied (DE PIPE {})", pipe);
                return;
            }
            // W1C the complete snapshot before dispatching any callback, as
            // Linux does, so an event arriving during dispatch remains live.
            intel_de_write(display, GEN8_DE_PIPE_IIR(pipe as u32), iir);
            if iir & GEN8_PIPE_VBLANK != 0 {
                let crtc = find_crtc_for_pipe(display, pipe);
                if !crtc.is_null() {
                    let _ = drm_crtc_handle_vblank(ptr::addr_of_mut!((*crtc).base));
                }
            }
            if iir & gen8_de_pipe_flip_done_mask(display_ver(display)) != 0 {
                flip_done_handler(display, pipe);
            }
            if has_dsb(display) {
                for dsb in 0..3 {
                    if iir & GEN12_DSB_INT(dsb) != 0 {
                        intel_dsb_irq_handler(display, pipe, dsb as i32);
                    }
                }
            }
            if iir & GEN12_PIPEDMC_INTERRUPT != 0 && display_ver(display) >= 12 {
                intel_pipedmc_irq_handler(display, pipe);
            }
            if iir & GEN8_PIPE_FIFO_UNDERRUN != 0 {
                intel_cpu_fifo_underrun_irq_handler(display, pipe);
            }
            let faults = iir & gen8_de_pipe_fault_mask(display_ver(display));
            if faults != 0 {
                pipe_fault_irq_handler(display, pipe, faults);
            }
        });
    }
    if unsafe { pch_type(display) >= PCH_ICP && pch_type(display) != PCH_NOP }
        && master_ctl & GEN8_DE_PCH_IRQ != 0
    {
        let pch_iir = unsafe { gen8_read_and_ack_pch_irqs(display) };
        if pch_iir != 0 {
            unsafe { icp_irq_handler(display, pch_iir) };
        } else {
            axlog::warn!("i915 display master interrupt lied (SDE)");
        }
    }
}

unsafe extern "C" fn gen11_display_irq_reset_entry(display: *mut IntelDisplay) {
    if !display.is_null() && unsafe { intel_display_device_present(display) } {
        unsafe { gen11_display_irq_reset(display) };
    }
}

unsafe extern "C" fn gen8_display_irq_reset_entry(display: *mut IntelDisplay) {
    if !display.is_null() && unsafe { intel_display_device_present(display) } {
        unsafe { gen8_display_irq_reset(display) };
    }
}

unsafe extern "C" fn gen11_display_irq_postinstall_entry(display: *mut IntelDisplay) {
    if !display.is_null() && unsafe { intel_display_device_present(display) } {
        unsafe { gen11_de_irq_postinstall(display) };
    }
}

unsafe extern "C" fn gen8_display_irq_postinstall_entry(display: *mut IntelDisplay) {
    if !display.is_null() && unsafe { intel_display_device_present(display) } {
        unsafe { gen8_de_irq_postinstall(display) };
    }
}

static GEN11_DISPLAY_IRQ_FUNCS: IntelDisplayIrqFuncs = IntelDisplayIrqFuncs {
    reset: Some(gen11_display_irq_reset_entry),
    postinstall: Some(gen11_display_irq_postinstall_entry),
    ack: None,
    handler: Some(gen11_display_irq_handler),
};

static GEN8_DISPLAY_IRQ_FUNCS: IntelDisplayIrqFuncs = IntelDisplayIrqFuncs {
    reset: Some(gen8_display_irq_reset_entry),
    postinstall: Some(gen8_display_irq_postinstall_entry),
    ack: None,
    handler: Some(gen8_display_irq_handler),
};

// upstream: intel_display_irq.c intel_display_irq_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_irq_init(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    unsafe {
        spin_lock_init(&mut (*display).irq.lock);
        if !(*display).drm.is_null() {
            (*(*display).drm).vblank_disable_immediate = true;
        }
        intel_hotplug_irq_init(display);
        INIT_WORK_C(
            &mut (*display).irq.vblank_notify_work,
            intel_display_vblank_notify_work,
        );
        (*display).irq.funcs = if display_ver(display) >= 11 {
            ptr::addr_of!(GEN11_DISPLAY_IRQ_FUNCS).cast_mut().cast()
        } else if display_ver(display) >= 8 {
            ptr::addr_of!(GEN8_DISPLAY_IRQ_FUNCS).cast_mut().cast()
        } else {
            ptr::null_mut()
        };
    }
}

// upstream: intel_display_irq.c intel_display_irq_reset()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_irq_reset(display: *mut IntelDisplay) {
    if display.is_null() || !unsafe { intel_display_device_present(display) } {
        return;
    }
    let funcs = unsafe { (*display).irq.funcs.cast::<IntelDisplayIrqFuncs>() };
    if !funcs.is_null() {
        if let Some(reset) = unsafe { (*funcs).reset } {
            unsafe { reset(display) };
        }
    }
}

// upstream: intel_display_irq.c intel_display_irq_postinstall()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_irq_postinstall(display: *mut IntelDisplay) {
    if display.is_null() || !unsafe { intel_display_device_present(display) } {
        return;
    }
    let funcs = unsafe { (*display).irq.funcs.cast::<IntelDisplayIrqFuncs>() };
    if !funcs.is_null() {
        if let Some(postinstall) = unsafe { (*funcs).postinstall } {
            unsafe { postinstall(display) };
        }
    }
}

// upstream: intel_display_irq.c intel_display_irq_ack()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_irq_ack(
    display: *mut IntelDisplay,
    state: *mut IntelDisplayIrqState,
) {
    if display.is_null() || !unsafe { intel_display_device_present(display) } {
        return;
    }
    let funcs = unsafe { (*display).irq.funcs.cast::<IntelDisplayIrqFuncs>() };
    if !funcs.is_null() {
        if let Some(ack) = unsafe { (*funcs).ack } {
            unsafe { ack(display, state) };
        }
    }
}

// upstream: intel_display_irq.c intel_display_irq_handler()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_irq_handler(
    display: *mut IntelDisplay,
    state: *const IntelDisplayIrqState,
) -> bool {
    if display.is_null() || !unsafe { intel_display_device_present(display) } {
        return true;
    }
    let funcs = unsafe { (*display).irq.funcs.cast::<IntelDisplayIrqFuncs>() };
    if funcs.is_null() {
        return true;
    }
    match unsafe { (*funcs).handler } {
        Some(handler) => unsafe { handler(display, state) },
        None => true,
    }
}

// upstream: intel_display_irq.c ibx_display_interrupt_update()
// The caller holds display.irq.lock; HPD detection changes these SDEIMR bits
// from both threaded and IRQ paths, and the parent interrupt must be active.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ibx_display_interrupt_update(
    display: *mut IntelDisplay,
    interrupt_mask: u32,
    enabled_irq_mask: u32,
) {
    if display.is_null() {
        return;
    }
    let old = unsafe { intel_de_read(display, SDEIMR) };
    let mut new = old & !interrupt_mask;
    new |= !enabled_irq_mask & interrupt_mask;
    if enabled_irq_mask & !interrupt_mask != 0
        || !unsafe { intel_parent_irq_enabled(display) }
    {
        return;
    }
    unsafe {
        intel_de_write(display, SDEIMR, new);
        intel_de_posting_read(display, SDEIMR);
    }
}

// upstream: intel_display_irq.c bdw_update_port_irq()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bdw_update_port_irq(
    display: *mut IntelDisplay,
    interrupt_mask: u32,
    enabled_irq_mask: u32,
) {
    if display.is_null() || enabled_irq_mask & !interrupt_mask != 0 {
        return;
    }
    if !unsafe { intel_parent_irq_enabled(display) } {
        return;
    }
    let old = unsafe { intel_de_read(display, GEN8_DE_PORT_IMR) };
    let mut new = old & !interrupt_mask;
    new |= !enabled_irq_mask & interrupt_mask;
    if new != old {
        unsafe { intel_de_write(display, GEN8_DE_PORT_IMR, new) };
        unsafe { intel_de_posting_read(display, GEN8_DE_PORT_IMR) };
    }
}

// upstream: intel_display_irq.c bdw_enable_pipe_irq()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bdw_enable_pipe_irq(display: *mut IntelDisplay, pipe: Pipe, bits: u32) {
    unsafe { bdw_update_pipe_irq(display, pipe, bits, bits) };
}

// upstream: intel_display_irq.c bdw_disable_pipe_irq()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bdw_disable_pipe_irq(display: *mut IntelDisplay, pipe: Pipe, bits: u32) {
    unsafe { bdw_update_pipe_irq(display, pipe, bits, 0) };
}

unsafe fn bdw_update_pipe_irq(
    display: *mut IntelDisplay,
    pipe: Pipe,
    interrupt_mask: u32,
    enabled_irq_mask: u32,
) {
    if display.is_null()
        || !(0..MAX_PIPES as i32).contains(&pipe)
        || enabled_irq_mask & !interrupt_mask != 0
        || !unsafe { intel_parent_irq_enabled(display) }
    {
        return;
    }
    let mask_slot = unsafe { ptr::addr_of_mut!((*display).irq.de_pipe_imr_mask[pipe as usize]) };
    let old = unsafe { *mask_slot };
    let mut new = old & !interrupt_mask;
    new |= !enabled_irq_mask & interrupt_mask;
    if new != old {
        unsafe { *mask_slot = new };
        let reg = GEN8_DE_PIPE_IMR(pipe as u32);
        unsafe { intel_de_write(display, reg, new) };
        unsafe { intel_de_posting_read(display, reg) };
    }
}

// upstream: intel_display_irq.c gen8_irq_power_well_post_enable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen8_irq_power_well_post_enable(
    display: *mut IntelDisplay,
    pipe_mask: u8,
) {
    if display.is_null() {
        return;
    }
    unsafe { spin_lock_irq(&mut (*display).irq.lock) };
    if !unsafe { intel_parent_irq_enabled(display) } {
        unsafe { spin_unlock_irq(&mut (*display).irq.lock) };
        return;
    }
    let extra = GEN8_PIPE_VBLANK
        | GEN8_PIPE_FIFO_UNDERRUN
        | gen8_de_pipe_flip_done_mask(unsafe { display_ver(display) });
    unsafe {
        for_each_pipe(display, |pipe| {
            if pipe_mask & (1 << pipe) == 0 {
                return;
            }
            let imr = (*display).irq.de_pipe_imr_mask[pipe as usize];
            irq_init(
                display,
                GEN8_DE_PIPE_IRQ_REGS(pipe as u32),
                imr,
                !imr | extra,
            );
        });
        spin_unlock_irq(&mut (*display).irq.lock);
    }
}

// upstream: intel_display_irq.c gen8_irq_power_well_pre_disable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen8_irq_power_well_pre_disable(
    display: *mut IntelDisplay,
    pipe_mask: u8,
) {
    if display.is_null() {
        return;
    }
    unsafe { spin_lock_irq(&mut (*display).irq.lock) };
    if !unsafe { intel_parent_irq_enabled(display) } {
        unsafe { spin_unlock_irq(&mut (*display).irq.lock) };
        return;
    }
    unsafe {
        for_each_pipe(display, |pipe| {
            if pipe_mask & (1 << pipe) != 0 {
                irq_reset(display, GEN8_DE_PIPE_IRQ_REGS(pipe as u32));
            }
        });
        spin_unlock_irq(&mut (*display).irq.lock);
        intel_parent_irq_synchronize(display);
    }
}

// upstream: intel_display_irq.c gen11_gu_misc_irq_ack()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen11_gu_misc_irq_ack(display: *mut IntelDisplay, master_ctl: u32) -> u32 {
    if display.is_null() || master_ctl & GEN11_GU_MISC_IRQ == 0 {
        return 0;
    }
    unsafe { intel_display_rpm_assert_block(display) };
    let iir = unsafe { intel_de_read(display, GEN11_GU_MISC_IIR) };
    if iir != 0 {
        unsafe { intel_de_write(display, GEN11_GU_MISC_IIR, iir) };
    }
    unsafe { intel_display_rpm_assert_unblock(display) };
    iir
}

// upstream: intel_display_irq.c gen11_gu_misc_irq_handler()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen11_gu_misc_irq_handler(display: *mut IntelDisplay, iir: u32) {
    if !display.is_null() && iir & GEN11_GU_MISC_GSE != 0 {
        unsafe { intel_opregion_asle_intr(display) };
    }
}

unsafe extern "C" fn intel_display_vblank_notify_work(work: *mut WorkStruct) {
    if work.is_null() {
        return;
    }
    let display = unsafe {
        work.cast::<u8>()
            .sub(
                offset_of!(IntelDisplay, irq)
                    + offset_of!(
                        crate::intel_display_core_types_upstream::IntelDisplayIrq,
                        vblank_notify_work
                    ),
            )
            .cast::<IntelDisplay>()
    };
    let enabled_count =
        unsafe { ptr::read_volatile(ptr::addr_of!((*display).irq.vblank_enable_count)) };
    let enabled = enabled_count != 0;
    let enabled_before = unsafe { (*display).irq.vblank_status_last_notified };
    if enabled_before != enabled {
        // The eDP PSR state machine is deliberately deferred; no PSR source
        // is unmasked by the ADL-P IRQ setup above.
        unsafe { (*display).irq.vblank_status_last_notified = enabled };
    }
}

unsafe fn bdw_update_vblank_irq(display: *mut IntelDisplay, pipe: Pipe, enable: bool) {
    if enable {
        unsafe { bdw_enable_pipe_irq(display, pipe, GEN8_PIPE_VBLANK) };
    } else {
        unsafe { bdw_disable_pipe_irq(display, pipe, GEN8_PIPE_VBLANK) };
    }
}

// upstream: intel_display_irq.c bdw_enable_vblank()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bdw_enable_vblank(crtc: *mut DrmCrtc) -> i32 {
    if crtc.is_null() {
        return -22;
    }
    let display =
        unsafe { crate::intel_display_conversion_upstream::__drm_to_display((*crtc).dev) };
    let intel_crtc = crtc.cast::<IntelCrtc>();
    if display.is_null() {
        return -22;
    }
    let pipe = unsafe { (*intel_crtc).pipe };
    let mut flags: c_ulong = 0;
    unsafe { spin_lock_irqsave(&mut (*display).irq.lock, &mut flags) };
    if unsafe { (*intel_crtc).vblank_psr_notify } {
        let old = unsafe { (*display).irq.vblank_enable_count };
        unsafe { (*display).irq.vblank_enable_count = old.wrapping_add(1) };
        if old == 0 {
            unsafe { queue_work(system_dfl_wq, &mut (*display).irq.vblank_notify_work) };
        }
    }
    unsafe { bdw_update_vblank_irq(display, pipe, true) };
    unsafe { spin_unlock_irqrestore(&mut (*display).irq.lock, flags) };
    let info = unsafe { (*display).info.__device_info };
    if !info.is_null() && unsafe { (*info)._bitfields_46[1] & (1 << 4) != 0 } {
        unsafe { drm_crtc_vblank_restore(crtc) };
    }
    0
}

// upstream: intel_display_irq.c bdw_disable_vblank()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bdw_disable_vblank(crtc: *mut DrmCrtc) {
    if crtc.is_null() {
        return;
    }
    let display =
        unsafe { crate::intel_display_conversion_upstream::__drm_to_display((*crtc).dev) };
    let intel_crtc = crtc.cast::<IntelCrtc>();
    if display.is_null() {
        return;
    }
    let pipe = unsafe { (*intel_crtc).pipe };
    let mut flags: c_ulong = 0;
    unsafe { spin_lock_irqsave(&mut (*display).irq.lock, &mut flags) };
    unsafe { bdw_update_vblank_irq(display, pipe, false) };
    unsafe { spin_unlock_irqrestore(&mut (*display).irq.lock, flags) };
    if unsafe { (*intel_crtc).vblank_psr_notify } {
        let old = unsafe { (*display).irq.vblank_enable_count };
        let new = old.wrapping_sub(1);
        unsafe { (*display).irq.vblank_enable_count = new };
        if new == 0 {
            unsafe { queue_work(system_dfl_wq, &mut (*display).irq.vblank_notify_work) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        GEN8_DE_MISC_IRQ, GEN8_DE_PORT_IRQ, GEN11_DISPLAY_IRQ_ENABLE, Gen11IrqStep, IrqResetOp,
        gen8_de_pipe_flip_done_mask, gen8_de_port_aux_mask, gen8_pipe_irq, irq_reset_ops,
        with_gen11_irq_masked,
    };
    use crate::{
        intel_display_reg_defs_upstream::INTEL_IRQ_REGS,
        intel_display_regs_upstream::{GEN8_DE_PORT_IER, GEN8_DE_PORT_IIR, GEN8_DE_PORT_IMR},
        intel_workarounds_types_upstream::I915RegT,
    };

    #[test]
    fn gen13_irq_masks_match_port_and_pipe_topology() {
        let expected_aux = (1 << 0)
            | (1 << 1)
            | (1 << 2)
            | (1 << 12)
            | (1 << 13)
            | (1 << 8)
            | (1 << 9)
            | (1 << 10)
            | (1 << 11);
        assert_eq!(gen8_de_port_aux_mask(13), expected_aux);
        assert_eq!(gen8_de_pipe_flip_done_mask(13), 1 << 3);
        assert_eq!(gen8_pipe_irq(0), 1 << 16);
        assert_eq!(gen8_pipe_irq(3), 1 << 19);
        assert_eq!(GEN8_DE_PORT_IRQ, 1 << 20);
        assert_eq!(GEN8_DE_MISC_IRQ, 1 << 22);
    }

    #[test]
    fn irq_reset_clears_masks_then_disables_enables_then_double_acks_iir() {
        let regs = INTEL_IRQ_REGS(GEN8_DE_PORT_IMR, GEN8_DE_PORT_IER, GEN8_DE_PORT_IIR);
        let ops = irq_reset_ops(regs);
        assert_eq!(ops.len(), 7);
        assert!(
            matches!(ops[0], IrqResetOp::Write(reg, u32::MAX) if reg.reg == GEN8_DE_PORT_IMR.reg)
        );
        assert!(matches!(ops[1], IrqResetOp::PostingRead(reg) if reg.reg == GEN8_DE_PORT_IMR.reg));
        assert!(matches!(ops[2], IrqResetOp::Write(reg, 0) if reg.reg == GEN8_DE_PORT_IER.reg));
        assert!(
            matches!(ops[3], IrqResetOp::Write(reg, u32::MAX) if reg.reg == GEN8_DE_PORT_IIR.reg)
        );
        assert!(matches!(ops[4], IrqResetOp::PostingRead(reg) if reg.reg == GEN8_DE_PORT_IIR.reg));
        assert!(
            matches!(ops[5], IrqResetOp::Write(reg, u32::MAX) if reg.reg == GEN8_DE_PORT_IIR.reg)
        );
        assert!(matches!(ops[6], IrqResetOp::PostingRead(reg) if reg.reg == GEN8_DE_PORT_IIR.reg));
    }

    #[test]
    fn gen11_master_is_disabled_before_child_ack_and_reenabled_after_dispatch() {
        let steps = std::cell::RefCell::new(std::vec::Vec::new());
        with_gen11_irq_masked(
            0x0055_a0a0,
            |value| {
                steps
                    .borrow_mut()
                    .push(Gen11IrqStep::WriteDisplayCtl(value))
            },
            |value| steps.borrow_mut().push(Gen11IrqStep::Dispatch(value)),
        );
        assert_eq!(
            *steps.borrow(),
            [
                Gen11IrqStep::WriteDisplayCtl(0),
                Gen11IrqStep::Dispatch(0x0055_a0a0),
                Gen11IrqStep::WriteDisplayCtl(GEN11_DISPLAY_IRQ_ENABLE),
            ]
        );
    }
}
