// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// Translation of drivers/gpu/drm/i915/display/intel_vblank.c for ADL-P.
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::{ffi::c_void, mem::offset_of, ptr};

use tk_drm_upstream::{
    drm_atomic_types::DrmAtomicCommit, drm_crtc_types::DrmCrtc, drm_modes_types::DrmDisplayMode,
    drm_vblank_types::DrmVblankCrtc,
};
use tk_linux_kapi::{
    locks::{
        local_irq_disable, local_irq_enable, spin_lock, spin_lock_irqsave, spin_unlock,
        spin_unlock_irqrestore,
    },
    primitives::{ktime_get, msecs_to_jiffies_timeout, udelay},
    types::WaitQueueEntry,
    wait::{TASK_UNINTERRUPTIBLE, finish_wait, prepare_to_wait, schedule_timeout},
};

use crate::{
    intel_atomic_upstream::{
        intel_atomic_get_new_crtc_state, intel_atomic_get_old_crtc_state, intel_crtc_needs_modeset,
    },
    intel_de_upstream::{intel_de_read, intel_de_read_fw, intel_de_read64_2x32_volatile},
    intel_display_conversion_upstream::__drm_to_display,
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_device_upstream::intel_display_device_present,
    intel_display_limits_types_upstream::Pipe,
    intel_display_regs_upstream::{
        IVB_TIMESTAMP_CTR, PIPE_FRAME_LOW_SHIFT, PIPE_FRMCOUNT_G4X, PIPE_FRMTMSTMP,
        PIPE_PIXEL_MASK, PIPE_PIXEL_SHIFT, PIPEDSL, PIPEDSL_LINE_MASK, PIPEFRAME, PIPEFRAMEPIXEL,
    },
    intel_display_types_upstream::{
        INTEL_OUTPUT_HDMI, IntelAtomicState, IntelCrtc, IntelCrtcState,
    },
    intel_uncore_types_upstream::IntelUncore,
    linux_i915_private::DrmI915Private,
};

const I915_MODE_FLAG_GET_SCANLINE_FROM_TIMESTAMP: u8 = 1 << 1;
const I915_MODE_FLAG_VRR: u8 = 1 << 6;
const I915_MODE_FLAG_USE_SCANLINE_COUNTER: u8 = 1 << 2;
const VBLANK_EVASION_TIME_US: i32 = 100;

#[repr(C)]
pub struct IntelVblankEvadeCtx {
    pub crtc: *mut IntelCrtc,
    pub min: i32,
    pub max: i32,
    pub vblank_start: i32,
    pub need_vlv_dsi_wa: bool,
}

#[inline]
unsafe fn display_from_crtc(crtc: *mut IntelCrtc) -> *mut IntelDisplay {
    if crtc.is_null() {
        return ptr::null_mut();
    }
    unsafe { __drm_to_display((*crtc).base.dev) }
}

#[inline]
unsafe fn display_from_drm(crtc: *mut DrmCrtc) -> *mut IntelDisplay {
    if crtc.is_null() {
        return ptr::null_mut();
    }
    unsafe { __drm_to_display((*crtc).dev) }
}

#[inline]
unsafe fn uncore(display: *mut IntelDisplay) -> *mut IntelUncore {
    if display.is_null()
        || unsafe { (*display).drm.is_null() || (*(*display).drm).dev_private.is_null() }
    {
        return ptr::null_mut();
    }
    let i915 = unsafe { (*(*display).drm).dev_private.cast::<DrmI915Private>() };
    unsafe { ptr::addr_of_mut!((*i915).uncore).cast::<IntelUncore>() }
}

#[inline]
unsafe fn vblank_crtc(crtc: *mut DrmCrtc) -> *mut DrmVblankCrtc {
    unsafe { tk_drm_upstream::drm_vblank_upstream::drm_crtc_vblank_crtc(crtc) }
}

#[inline]
fn div_round_up_u64(value: u64, divisor: u64) -> u64 {
    if divisor == 0 {
        return 0;
    }
    value / divisor + u64::from(value % divisor != 0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_mode_vdisplay(mode: *const DrmDisplayMode) -> i32 {
    if mode.is_null() {
        return 0;
    }
    let value = unsafe { (*mode).crtc_vdisplay as i32 };
    if unsafe { (*mode).flags & tk_drm_upstream::uapi_drm_consts::DRM_MODE_FLAG_INTERLACE != 0 } {
        (value + 1) / 2
    } else {
        value
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_mode_vblank_start(mode: *const DrmDisplayMode) -> i32 {
    if mode.is_null() {
        return 0;
    }
    let value = unsafe { (*mode).crtc_vblank_start as i32 };
    if unsafe { (*mode).flags & tk_drm_upstream::uapi_drm_consts::DRM_MODE_FLAG_INTERLACE != 0 } {
        (value + 1) / 2
    } else {
        value
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_mode_vblank_end(mode: *const DrmDisplayMode) -> i32 {
    if mode.is_null() {
        return 0;
    }
    let value = unsafe { (*mode).crtc_vblank_end as i32 };
    if unsafe { (*mode).flags & tk_drm_upstream::uapi_drm_consts::DRM_MODE_FLAG_INTERLACE != 0 } {
        value / 2
    } else {
        value
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_mode_vtotal(mode: *const DrmDisplayMode) -> i32 {
    if mode.is_null() {
        return 0;
    }
    let value = unsafe { (*mode).crtc_vtotal as i32 };
    if unsafe { (*mode).flags & tk_drm_upstream::uapi_drm_consts::DRM_MODE_FLAG_INTERLACE != 0 } {
        value / 2
    } else {
        value
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_mode_vblank_delay(mode: *const DrmDisplayMode) -> i32 {
    unsafe { intel_mode_vblank_start(mode) - intel_mode_vdisplay(mode) }
}

unsafe fn crtc_scanlines_since_frame_timestamp(crtc: *mut IntelCrtc) -> u32 {
    let display = unsafe { display_from_crtc(crtc) };
    let vblank = unsafe { vblank_crtc(&mut (*crtc).base) };
    if display.is_null() || vblank.is_null() {
        return 0;
    }
    let mode = unsafe { &(*vblank).hwmode };
    let htotal = mode.crtc_htotal as u32;
    let clock = mode.crtc_clock as u32;
    if htotal == 0 {
        return 0;
    }
    let mut prev;
    let mut current = 0u32;
    loop {
        prev = unsafe { intel_de_read_fw(display, PIPE_FRMTMSTMP((*crtc).pipe as u32)) };
        current = unsafe { intel_de_read_fw(display, IVB_TIMESTAMP_CTR) };
        let post = unsafe { intel_de_read_fw(display, PIPE_FRMTMSTMP((*crtc).pipe as u32)) };
        if post == prev {
            break;
        }
    }
    ((current.wrapping_sub(prev) as u64 * clock as u64) / (1000 * htotal as u64)) as u32
}

unsafe fn get_crtc_scanline_from_timestamp(crtc: *mut IntelCrtc) -> u32 {
    let vblank = unsafe { vblank_crtc(&mut (*crtc).base) };
    if vblank.is_null() {
        return 0;
    }
    let mode = unsafe { &(*vblank).hwmode };
    let vtotal = unsafe { intel_mode_vtotal(mode) }.max(1) as u32;
    let start = mode.crtc_vblank_start as u32;
    (unsafe { crtc_scanlines_since_frame_timestamp(crtc) }.min(vtotal - 1) + start) % vtotal
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_crtc_scanline_offset(state: *const IntelCrtcState) -> i32 {
    if state.is_null() {
        return 0;
    }
    let display = unsafe { display_from_drm((*state).uapi.crtc) };
    if display.is_null() {
        return 0;
    }
    let ver = unsafe { (*display).info.__runtime_info.ip.ver };
    if ver >= 9 {
        if unsafe { (*state).output_types & (1u32 << INTEL_OUTPUT_HDMI) != 0 } {
            2
        } else {
            1
        }
    } else if ver >= 3
        || unsafe { (*display).platform._anon_0_0.bitmap[0] & ((1 << 20) | (1 << 27)) != 0 }
    {
        1
    } else {
        -1
    }
}

unsafe fn get_crtc_scanline(crtc: *mut IntelCrtc) -> i32 {
    if crtc.is_null() || !unsafe { (*crtc).active } {
        return 0;
    }
    let display = unsafe { display_from_crtc(crtc) };
    let vb = unsafe { vblank_crtc(&mut (*crtc).base) };
    if display.is_null() || vb.is_null() {
        return 0;
    }
    if unsafe { (*crtc).mode_flags & I915_MODE_FLAG_GET_SCANLINE_FROM_TIMESTAMP != 0 } {
        return unsafe { get_crtc_scanline_from_timestamp(crtc) as i32 };
    }
    let vtotal = unsafe { intel_mode_vtotal(&(*vb).hwmode) };
    if vtotal <= 0 {
        return 0;
    }
    let reg = unsafe { PIPEDSL(display, (*crtc).pipe as u32) };
    let mut position = unsafe { intel_de_read_fw(display, reg) & PIPEDSL_LINE_MASK } as i32;
    // The DDI scanline register can transiently report 0 at the vblank edge.
    if unsafe { (*display).info.__runtime_info.ip.ver >= 8 } && position == 0 {
        for _ in 0..100 {
            udelay(1);
            let next = unsafe { intel_de_read_fw(display, reg) & PIPEDSL_LINE_MASK } as i32;
            if next != position {
                position = next;
                break;
            }
        }
    }
    (position + vtotal + unsafe { (*crtc).scanline_offset }) % vtotal
}

unsafe fn vblank_uncore_lock(display: *mut IntelDisplay, flags: &mut u64) -> *mut IntelUncore {
    let uncore = unsafe { uncore(display) };
    if !uncore.is_null() {
        unsafe { spin_lock_irqsave(&mut (*uncore).lock, flags) };
    }
    uncore
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_get_crtc_scanline(crtc: *mut IntelCrtc) -> i32 {
    if crtc.is_null() {
        return 0;
    }
    let display = unsafe { display_from_crtc(crtc) };
    let mut flags = 0u64;
    let uncore = unsafe { vblank_uncore_lock(display, &mut flags) };
    let position = unsafe { get_crtc_scanline(crtc) };
    if !uncore.is_null() {
        unsafe { spin_unlock_irqrestore(&mut (*uncore).lock, flags) };
    }
    position
}

unsafe extern "C" fn i915_get_crtc_scanoutpos(
    base: *mut DrmCrtc,
    _in_vblank_irq: bool,
    vpos: *mut i32,
    hpos: *mut i32,
    stime: *mut i64,
    etime: *mut i64,
    mode: *mut DrmDisplayMode,
) -> bool {
    if base.is_null() || mode.is_null() || unsafe { (*mode).crtc_clock == 0 } {
        return false;
    }
    let display = unsafe { display_from_drm(base) };
    let crtc = base.cast::<IntelCrtc>();
    if display.is_null() {
        return false;
    }
    let htotal = unsafe { (*mode).crtc_htotal as i32 };
    if htotal == 0 {
        return false;
    }
    let vtotal = unsafe { intel_mode_vtotal(mode) };
    if vtotal == 0 {
        return false;
    }
    let vbl_start = unsafe { intel_mode_vblank_start(mode) };
    let vbl_end = unsafe { intel_mode_vblank_end(mode) };
    let mut flags = 0u64;
    let uncore = unsafe { vblank_uncore_lock(display, &mut flags) };
    if !stime.is_null() {
        unsafe { *stime = ktime_get() };
    }
    let scanline_counter = unsafe { (*display).info.__runtime_info.ip.ver >= 5 }
        || unsafe { (*crtc).mode_flags & I915_MODE_FLAG_USE_SCANLINE_COUNTER != 0 };
    let mut pos = if scanline_counter {
        unsafe { get_crtc_scanline(crtc) }
    } else {
        let value = unsafe {
            intel_de_read_fw(display, PIPEFRAMEPIXEL(display, (*crtc).pipe as u32))
                & PIPE_PIXEL_MASK
        } >> PIPE_PIXEL_SHIFT;
        let frame_total = vtotal.saturating_mul(htotal);
        ((value as i32 + htotal - (*mode).crtc_hsync_start as i32) % frame_total)
            .min(frame_total - 1)
    };
    if !etime.is_null() {
        unsafe { *etime = ktime_get() };
    }
    if !uncore.is_null() {
        unsafe { spin_unlock_irqrestore(&mut (*uncore).lock, flags) };
    }
    if pos >= vbl_start {
        pos -= vbl_end;
    } else {
        pos += vtotal - vbl_end;
    }
    let vertical = if scanline_counter { pos } else { pos / htotal };
    if !vpos.is_null() {
        unsafe { *vpos = vertical };
    }
    if !hpos.is_null() {
        unsafe {
            *hpos = if scanline_counter {
                0
            } else {
                pos - vertical.saturating_mul(htotal)
            }
        };
    }
    true
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_crtc_get_vblank_timestamp(
    base: *mut DrmCrtc,
    max_error: *mut i32,
    vblank_time: *mut i64,
    in_vblank_irq: bool,
) -> bool {
    unsafe {
        tk_drm_upstream::drm_vblank_upstream::drm_crtc_vblank_helper_get_vblank_timestamp_internal(
            base,
            max_error,
            vblank_time,
            in_vblank_irq,
            Some(i915_get_crtc_scanoutpos),
        )
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_get_vblank_counter(base: *mut DrmCrtc) -> u32 {
    if base.is_null() {
        return 0;
    }
    let display = unsafe { display_from_drm(base) };
    let vblank = unsafe { vblank_crtc(base) };
    if display.is_null() || vblank.is_null() || unsafe { (*vblank).max_vblank_count == 0 } {
        return 0;
    }
    let mode = unsafe { &(*vblank).hwmode };
    let pipe = unsafe { (*base.cast::<IntelCrtc>()).pipe as u32 };
    let htotal = mode.crtc_htotal as u32;
    if htotal == 0 {
        return 0;
    }
    let mut vblank_start = unsafe { intel_mode_vblank_start(mode) } as u32 * htotal;
    vblank_start = vblank_start.wrapping_sub(htotal - mode.crtc_hsync_start as u32);
    let frame = unsafe {
        intel_de_read64_2x32_volatile(
            display,
            PIPEFRAMEPIXEL(display, pipe),
            PIPEFRAME(display, pipe),
        )
    };
    let pixel = frame as u32 & PIPE_PIXEL_MASK;
    let count = (frame >> PIPE_FRAME_LOW_SHIFT) & 0x00ff_ffff;
    (count as u32).wrapping_add(u32::from(pixel >= vblank_start)) & 0x00ff_ffff
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn g4x_get_vblank_counter(base: *mut DrmCrtc) -> u32 {
    if base.is_null() {
        return 0;
    }
    let display = unsafe { display_from_drm(base) };
    let vblank = unsafe { vblank_crtc(base) };
    if display.is_null() || vblank.is_null() || unsafe { (*vblank).max_vblank_count == 0 } {
        return 0;
    }
    unsafe {
        intel_de_read(
            display,
            PIPE_FRMCOUNT_G4X(display, (*base.cast::<IntelCrtc>()).pipe as u32),
        )
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_wait_for_pipe_scanline_stopped(crtc: *mut IntelCrtc) {
    wait_for_pipe_scanline_moving(crtc, false);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_wait_for_pipe_scanline_moving(crtc: *mut IntelCrtc) {
    wait_for_pipe_scanline_moving(crtc, true);
}

unsafe fn wait_for_pipe_scanline_moving(crtc: *mut IntelCrtc, should_move: bool) {
    if crtc.is_null() {
        return;
    }
    let display = unsafe { display_from_crtc(crtc) };
    if display.is_null() {
        return;
    }
    let pipe = unsafe { (*crtc).pipe as u32 };
    let mut moving = false;
    let ret = tk_linux_kapi::poll_timeout_us!(
        {
            let line1 =
                unsafe { intel_de_read(display, PIPEDSL(display, pipe)) } & PIPEDSL_LINE_MASK;
            tk_linux_kapi::wait::msleep(5);
            let line2 =
                unsafe { intel_de_read(display, PIPEDSL(display, pipe)) } & PIPEDSL_LINE_MASK;
            moving = line1 != line2;
        },
        moving == should_move,
        500,
        100_000,
        false
    );
    let _ = ret;
}

unsafe fn active_timings(
    mode: &mut DrmDisplayMode,
    state: *const IntelCrtcState,
    vrr_enable: bool,
    vmax_start: &mut i32,
) {
    unsafe {
        tk_drm_upstream::drm_modes_upstream::drm_mode_init(
            mode,
            ptr::addr_of!((*state).hw.adjusted_mode),
        )
    };
    *vmax_start = 0;
    if vrr_enable && unsafe { (*state).vrr.enable } {
        mode.crtc_vtotal = unsafe { (*state).vrr.vmax };
        mode.crtc_vblank_end = unsafe { (*state).vrr.vmax };
        mode.crtc_vblank_start =
            unsafe { (*state).vrr.vmin.saturating_sub((*state).vrr.guardband) };
        *vmax_start = unsafe { (*state).vrr.vmax.saturating_sub((*state).vrr.guardband) as i32 };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_crtc_update_active_timings(
    state: *const IntelCrtcState,
    vrr_enable: bool,
) {
    if state.is_null() || unsafe { (*state).uapi.crtc.is_null() } {
        return;
    }
    let display = unsafe { display_from_drm((*state).uapi.crtc) };
    let crtc = unsafe { (*state).uapi.crtc.cast::<IntelCrtc>() };
    if display.is_null() {
        return;
    }
    let mut mode = unsafe { core::mem::zeroed::<DrmDisplayMode>() };
    let mut vmax_start = 0;
    unsafe { active_timings(&mut mode, state, vrr_enable, &mut vmax_start) };
    let drm = unsafe { (*display).drm };
    let mut flags = 0u64;
    if !drm.is_null() {
        unsafe { spin_lock_irqsave(&mut (*drm).vblank_time_lock, &mut flags) };
    }
    let uncore = unsafe { uncore(display) };
    if !uncore.is_null() {
        unsafe { spin_lock(&mut (*uncore).lock) };
    }
    unsafe {
        tk_drm_upstream::drm_vblank_upstream::drm_calc_timestamping_constants(
            &mut (*crtc).base,
            &mode,
        );
        (*crtc).vmax_vblank_start = vmax_start as u16;
        (*crtc).mode_flags = if vrr_enable {
            (*state).mode_flags
        } else {
            (*state).mode_flags & !I915_MODE_FLAG_VRR
        };
        (*crtc).scanline_offset = intel_crtc_scanline_offset(state);
    }
    if !uncore.is_null() {
        unsafe { spin_unlock(&mut (*uncore).lock) };
    }
    if !drm.is_null() {
        unsafe { spin_unlock_irqrestore(&mut (*drm).vblank_time_lock, flags) };
    }
}

#[inline]
unsafe fn pre_commit_state<'a>(
    old: *const IntelCrtcState,
    new: *const IntelCrtcState,
) -> *const IntelCrtcState {
    if unsafe { intel_crtc_needs_modeset(new) } {
        new
    } else {
        old
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pre_commit_crtc_state(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
) -> *const IntelCrtcState {
    if state.is_null() || crtc.is_null() {
        return ptr::null();
    }
    let old = unsafe { intel_atomic_get_old_crtc_state(state, crtc) };
    let new = unsafe { intel_atomic_get_new_crtc_state(state, crtc) };
    unsafe { pre_commit_state(old, new) }
}

unsafe fn vrr_vblank_start(state: *const IntelCrtcState) -> i32 {
    let pushed = false; // VRR push register is not enabled in this subset.
    let start = unsafe {
        if pushed {
            (*state).vrr.vmin
        } else {
            (*state).vrr.vmax
        }
    } as i32;
    start - unsafe { (*state).vrr.guardband as i32 }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_vblank_evade_init(
    old_state: *const IntelCrtcState,
    new_state: *const IntelCrtcState,
    evade: *mut IntelVblankEvadeCtx,
) {
    if old_state.is_null() || new_state.is_null() || evade.is_null() {
        return;
    }
    let crtc = unsafe { (*new_state).uapi.crtc.cast::<IntelCrtc>() };
    if crtc.is_null() {
        return;
    }
    let selected = unsafe { pre_commit_state(old_state, new_state) };
    let mode = unsafe { &(*selected).hw.adjusted_mode };
    let vrr = unsafe { (*crtc).mode_flags & I915_MODE_FLAG_VRR != 0 };
    let start = if vrr {
        unsafe { vrr_vblank_start(selected) }
    } else {
        unsafe { intel_mode_vblank_start(mode) }
    };
    let delay = if vrr {
        unsafe { (*selected).set_context_latency as i32 }
    } else {
        unsafe { intel_mode_vblank_delay(mode) }
    };
    let mut min = start
        - ((VBLANK_EVASION_TIME_US * mode.crtc_clock as i32 + 1000 * mode.crtc_htotal as i32 - 1)
            / (1000 * (mode.crtc_htotal as i32).max(1)));
    if unsafe { (*selected).use_dsb || (*selected).update_m_n || (*selected).update_lrr } {
        min -= delay;
    }
    unsafe {
        (*evade).crtc = crtc;
        (*evade).vblank_start = start;
        (*evade).min = min;
        (*evade).max = start - 1;
        (*evade).need_vlv_dsi_wa = false;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_vblank_evade(evade: *mut IntelVblankEvadeCtx) -> i32 {
    if evade.is_null() {
        return 0;
    }
    let crtc = unsafe { (*evade).crtc };
    if crtc.is_null() {
        return 0;
    }
    let mut timeout = msecs_to_jiffies_timeout(1) as i64;
    let base = unsafe { &mut (*crtc).base };
    let wq = unsafe { tk_drm_upstream::drm_vblank_upstream::drm_crtc_vblank_waitqueue(base) };
    if wq.is_null() {
        return 0;
    }
    let mut wait = tk_linux_kapi::DEFINE_WAIT!();
    loop {
        unsafe {
            prepare_to_wait(
                wq,
                &mut wait as *mut WaitQueueEntry,
                TASK_UNINTERRUPTIBLE as i32,
            )
        };
        let scanline = unsafe { get_crtc_scanline(crtc) };
        if scanline < unsafe { (*evade).min } || scanline > unsafe { (*evade).max } {
            break;
        }
        if timeout == 0 {
            break;
        }
        local_irq_enable();
        timeout = schedule_timeout(timeout);
        local_irq_disable();
    }
    unsafe { finish_wait(wq, &mut wait as *mut WaitQueueEntry) };
    unsafe { get_crtc_scanline(crtc) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_crtc_vblank_length(state: *const IntelCrtcState) -> i32 {
    if state.is_null() {
        return 0;
    }
    unsafe {
        if (*state).vrr.enable {
            (*state).vrr.guardband as i32
        } else {
            (*state).hw.adjusted_mode.crtc_vtotal as i32
                - (*state).hw.adjusted_mode.crtc_vblank_start as i32
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interlaced_vblank_boundaries_round_start_but_truncate_end() {
        let mut mode = unsafe { core::mem::zeroed::<DrmDisplayMode>() };
        mode.flags = tk_drm_upstream::uapi_drm_consts::DRM_MODE_FLAG_INTERLACE;
        mode.crtc_vdisplay = 1081;
        mode.crtc_vblank_start = 1083;
        mode.crtc_vblank_end = 1125;
        mode.crtc_vtotal = 1125;
        assert_eq!(unsafe { intel_mode_vdisplay(&mode) }, 541);
        assert_eq!(unsafe { intel_mode_vblank_start(&mode) }, 542);
        assert_eq!(unsafe { intel_mode_vblank_end(&mode) }, 562);
        assert_eq!(unsafe { intel_mode_vtotal(&mode) }, 562);
    }

    #[test]
    fn vblank_length_prefers_vrr_guardband() {
        let mut state = unsafe { core::mem::zeroed::<IntelCrtcState>() };
        state.hw.adjusted_mode.crtc_vtotal = 1125;
        state.hw.adjusted_mode.crtc_vblank_start = 1080;
        assert_eq!(unsafe { intel_crtc_vblank_length(&state) }, 45);
        state.vrr.enable = true;
        state.vrr.guardband = 4;
        assert_eq!(unsafe { intel_crtc_vblank_length(&state) }, 4);
    }
}
