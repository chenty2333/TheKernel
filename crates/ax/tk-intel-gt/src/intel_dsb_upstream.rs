// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
// Source: drivers/gpu/drm/i915/display/intel_dsb.c
#![allow(non_snake_case, dead_code, unsafe_code)]

use alloc::boxed::Box;
use core::{
    ffi::c_void,
    mem::{align_of, size_of},
    ptr,
    sync::atomic::{Ordering, fence},
};

use tk_drm_upstream::{
    drm_atomic_types::{DrmAtomicCommit, DrmCrtcsState},
    drm_crtc_types::{DrmCrtc, DrmCrtcState},
    drm_device_types::DrmDevice,
    drm_vblank_types::DrmPendingVblankEvent,
};

use crate::{
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_reg_defs_upstream::DISPLAY_VER,
    intel_display_regs_upstream::CHICKEN_MISC_2,
    intel_display_types_upstream::{IntelAtomicState, IntelCrtc, IntelCrtcState},
    intel_dsb_regs_upstream::*,
    intel_parent_upstream::{
        IntelDisplayDsbInterface, IntelDisplayRpmInterface, intel_display_parent,
    },
    intel_workarounds_types_upstream::I915RegT,
    linux::{
        locks::spin_lock,
        memory::{kfree, kmalloc_obj},
        primitives::udelay,
    },
    linux_config::GFP_KERNEL,
};

const CACHELINE_BYTES: u32 = 64;
const DSB_OPCODE_SHIFT: u32 = 24;
const DSB_OPCODE_NOOP: u32 = 0x0;
const DSB_OPCODE_MMIO_WRITE: u32 = 0x1;
const DSB_BYTE_EN_SHIFT: u32 = 20;
const DSB_REG_VALUE_MASK: u32 = 0x000f_ffff;
const DSB_OPCODE_WAIT_USEC: u32 = 0x2;
const DSB_OPCODE_WAIT_SCANLINE: u32 = 0x3;
const DSB_OPCODE_WAIT_VBLANKS: u32 = 0x4;
const DSB_OPCODE_WAIT_DSL_IN: u32 = 0x5;
const DSB_OPCODE_WAIT_DSL_OUT: u32 = 0x6;
const DSB_OPCODE_INTERRUPT: u32 = 0x7;
const DSB_OPCODE_INDEXED_WRITE: u32 = 0x9;
const DSB_OPCODE_POLL: u32 = 0xa;
const DSB_OPCODE_GOSUB: u32 = 0xc;
const DSB_SCANLINE_UPPER_SHIFT: u32 = 20;
const DSB_GOSUB_HEAD_SHIFT: u32 = 26;

/// DSB contexts are private to this implementation; only the parent-owned
/// backing buffer crosses the display boundary.  `submitted` records whether
/// hardware may still DMA from the backing map and therefore gates teardown.
#[repr(C)]
pub struct IntelDsb {
    id: u32,
    dsb_buf: *mut c_void,
    crtc: *mut IntelCrtc,
    display: *mut IntelDisplay,
    size: u32,
    free_pos: u32,
    ins: [u32; 2],
    ins_start_offset: u32,
    chicken: u32,
    hw_dewake_scanline: i32,
    submitted: bool,
    _pad: [u8; 3],
}

#[inline]
unsafe fn dsb_interface<'a>(display: *mut IntelDisplay) -> Option<&'a IntelDisplayDsbInterface> {
    if display.is_null() {
        return None;
    }
    let parent = unsafe { intel_display_parent(display) };
    if parent.is_null() || unsafe { (*parent).dsb.is_null() } {
        return None;
    }
    let ops = unsafe { &*(*parent).dsb };
    (ops.ggtt_offset.is_some()
        && ops.write.is_some()
        && ops.read.is_some()
        && ops.fill.is_some()
        && ops.create.is_some()
        && ops.cleanup.is_some()
        && ops.flush_map.is_some())
    .then_some(ops)
}

#[inline]
unsafe fn dsb_display_from_state(state: *mut IntelAtomicState) -> *mut IntelDisplay {
    if state.is_null() {
        return ptr::null_mut();
    }
    let drm = unsafe { (*state).base.dev };
    if drm.is_null() {
        return ptr::null_mut();
    }
    unsafe { crate::intel_display_conversion_upstream::__drm_to_display(drm) }
}

#[inline]
unsafe fn display_has_dsb(display: *mut IntelDisplay) -> bool {
    if display.is_null() {
        return false;
    }
    let info = unsafe { (*display).info.__device_info };
    // `intel_display_device.h`: cursor_needs_physical through has_dsb are
    // the first six one-bit fields in this source-defined packed byte.
    !info.is_null() && unsafe { (*info)._bitfields_46[0] & (1 << 5) != 0 }
}

#[inline]
unsafe fn dsb_ops_required<'a>(display: *mut IntelDisplay) -> Option<&'a IntelDisplayDsbInterface> {
    unsafe { dsb_interface(display) }
}

unsafe fn intel_display_rpm_ops(
    display: *mut IntelDisplay,
) -> Option<(
    unsafe extern "C" fn(*const DrmDevice) -> *mut c_void,
    unsafe extern "C" fn(*const DrmDevice, *mut c_void),
)> {
    let parent = unsafe { intel_display_parent(display) };
    if parent.is_null() || unsafe { (*parent).rpm.is_null() } {
        return None;
    }
    let rpm: &IntelDisplayRpmInterface = unsafe { &*(*parent).rpm };
    Some((rpm.get?, rpm.put?))
}

#[inline]
unsafe fn dsb_buffer_ggtt_offset(dsb: *mut IntelDsb) -> u32 {
    let Some(ops) = (unsafe { dsb_ops_required((*dsb).display) }) else {
        return 0;
    };
    unsafe { ops.ggtt_offset.unwrap_unchecked()((*dsb).dsb_buf) }
}

#[inline]
unsafe fn dsb_buffer_write(dsb: *mut IntelDsb, idx: u32, val: u32) {
    let Some(ops) = (unsafe { dsb_ops_required((*dsb).display) }) else {
        return;
    };
    unsafe { ops.write.unwrap_unchecked()((*dsb).dsb_buf, idx, val) };
}

#[inline]
unsafe fn dsb_buffer_read(dsb: *mut IntelDsb, idx: u32) -> u32 {
    let Some(ops) = (unsafe { dsb_ops_required((*dsb).display) }) else {
        return 0;
    };
    unsafe { ops.read.unwrap_unchecked()((*dsb).dsb_buf, idx) }
}

#[inline]
unsafe fn dsb_buffer_fill(dsb: *mut IntelDsb, idx: u32, val: u32, bytes: usize) {
    let Some(ops) = (unsafe { dsb_ops_required((*dsb).display) }) else {
        return;
    };
    unsafe { ops.fill.unwrap_unchecked()((*dsb).dsb_buf, idx, val, bytes) };
}

#[inline]
unsafe fn dsb_buffer_cleanup(dsb: *mut IntelDsb) -> bool {
    let Some(ops) = (unsafe { dsb_ops_required((*dsb).display) }) else {
        return false;
    };
    unsafe { ops.cleanup.unwrap_unchecked()((*dsb).dsb_buf) };
    true
}

#[inline]
unsafe fn dsb_buffer_flush_map(dsb: *mut IntelDsb) -> bool {
    let Some(ops) = (unsafe { dsb_ops_required((*dsb).display) }) else {
        return false;
    };
    fence(Ordering::Release);
    unsafe { ops.flush_map.unwrap_unchecked()((*dsb).dsb_buf) };
    true
}

#[inline]
fn align_up(value: u32, alignment: u32) -> Option<u32> {
    value
        .checked_add(alignment - 1)
        .map(|v| v & !(alignment - 1))
}

#[inline]
unsafe fn dsb_buffer_create(display: *mut IntelDisplay, bytes: usize) -> *mut c_void {
    let Some(ops) = (unsafe { dsb_ops_required(display) }) else {
        return ptr::null_mut();
    };
    let drm = unsafe { (*display).drm };
    if drm.is_null() {
        return ptr::null_mut();
    }
    unsafe { ops.create.unwrap_unchecked()(drm, bytes) }
}

#[inline]
fn new_crtc_state(state: *mut IntelAtomicState, crtc: *mut IntelCrtc) -> *mut IntelCrtcState {
    if state.is_null() || crtc.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        tk_drm_upstream::drm_atomic_upstream::drm_atomic_get_new_crtc_state(
            &mut (*state).base,
            &mut (*crtc).base,
        )
        .cast()
    }
}

#[inline]
fn old_crtc_state(state: *mut IntelAtomicState, crtc: *mut IntelCrtc) -> *mut IntelCrtcState {
    if state.is_null() || crtc.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        tk_drm_upstream::drm_atomic_upstream::drm_atomic_get_old_crtc_state(
            &mut (*state).base,
            &mut (*crtc).base,
        )
        .cast()
    }
}

#[inline]
fn crtc_needs_modeset(state: *const IntelCrtcState) -> bool {
    if state.is_null() {
        return false;
    }
    unsafe {
        (*state).uapi.mode_changed()
            || (*state).uapi.active_changed()
            || (*state).uapi.connectors_changed()
    }
}

#[inline]
fn pre_commit_crtc_state(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
) -> *mut IntelCrtcState {
    let old = old_crtc_state(state, crtc);
    let new = new_crtc_state(state, crtc);
    if old.is_null() || new.is_null() {
        return ptr::null_mut();
    }
    if crtc_needs_modeset(new) { new } else { old }
}

#[inline]
fn pre_commit_is_vrr_active(state: *mut IntelAtomicState, crtc: *mut IntelCrtc) -> bool {
    let old = old_crtc_state(state, crtc);
    let new = new_crtc_state(state, crtc);
    !old.is_null()
        && !new.is_null()
        && !crtc_needs_modeset(new)
        && unsafe { (*old).vrr.enable && (*new).vrr.enable }
}

#[inline]
fn intel_mode_vdisplay(mode: *const tk_drm_upstream::drm_modes_types::DrmDisplayMode) -> i32 {
    if mode.is_null() {
        return 0;
    }
    let mut value = unsafe { (*mode).crtc_vdisplay as i32 };
    if unsafe { (*mode).flags & tk_drm_upstream::uapi_drm_consts::DRM_MODE_FLAG_INTERLACE != 0 } {
        value = value.saturating_add(1) / 2;
    }
    value
}

#[inline]
fn intel_mode_vtotal(mode: *const tk_drm_upstream::drm_modes_types::DrmDisplayMode) -> i32 {
    if mode.is_null() {
        return 0;
    }
    let mut value = unsafe { (*mode).crtc_vtotal as i32 };
    if unsafe { (*mode).flags & tk_drm_upstream::uapi_drm_consts::DRM_MODE_FLAG_INTERLACE != 0 } {
        value /= 2;
    }
    value
}

#[inline]
fn intel_mode_vblank_start(mode: *const tk_drm_upstream::drm_modes_types::DrmDisplayMode) -> i32 {
    if mode.is_null() {
        return 0;
    }
    let mut value = unsafe { (*mode).crtc_vblank_start as i32 };
    if unsafe { (*mode).flags & tk_drm_upstream::uapi_drm_consts::DRM_MODE_FLAG_INTERLACE != 0 } {
        value = value.saturating_add(1) / 2;
    }
    value
}

#[inline]
fn intel_mode_vblank_delay(mode: *const tk_drm_upstream::drm_modes_types::DrmDisplayMode) -> i32 {
    intel_mode_vblank_start(mode) - intel_mode_vdisplay(mode)
}

#[inline]
fn intel_usecs_to_scanlines(
    mode: *const tk_drm_upstream::drm_modes_types::DrmDisplayMode,
    usecs: i32,
) -> i32 {
    if mode.is_null() || unsafe { (*mode).crtc_htotal == 0 } {
        return 1;
    }
    let num = (usecs.max(0) as u64).saturating_mul(unsafe { (*mode).crtc_clock.max(0) as u64 });
    ((num + 1000 * unsafe { (*mode).crtc_htotal as u64 } - 1)
        / (1000 * unsafe { (*mode).crtc_htotal as u64 })) as i32
}

#[inline]
fn intel_scanlines_to_usecs(
    mode: *const tk_drm_upstream::drm_modes_types::DrmDisplayMode,
    lines: i32,
) -> i32 {
    if mode.is_null() || unsafe { (*mode).crtc_clock <= 0 } {
        return 1;
    }
    let num = (lines.max(0) as u64)
        .saturating_mul(unsafe { (*mode).crtc_htotal as u64 })
        .saturating_mul(1000);
    ((num + unsafe { (*mode).crtc_clock as u64 } - 1) / unsafe { (*mode).crtc_clock as u64 }) as i32
}

#[inline]
fn dsb_vtotal(state: *mut IntelAtomicState, crtc: *mut IntelCrtc) -> i32 {
    let atomic_state = state;
    let crtc_state = pre_commit_crtc_state(state, crtc);
    if crtc_state.is_null() {
        return 0;
    }
    let mode = unsafe { ptr::addr_of!((*crtc_state).hw.adjusted_mode) };
    if pre_commit_is_vrr_active(atomic_state, crtc) && unsafe { (*crtc_state).vrr.vmax != 0 } {
        unsafe { (*crtc_state).vrr.vmax as i32 }
    } else {
        intel_mode_vtotal(mode)
    }
}

unsafe fn dsb_wm_max_latency(display: *mut IntelDisplay, initial_level: usize) -> u32 {
    if display.is_null() {
        return 0;
    }
    let num_levels = unsafe { (*display).wm.num_levels as usize }.min(8);
    for level in (initial_level.min(num_levels)..num_levels).rev() {
        let latency = unsafe { (*display).wm.skl_latency[level] } as u32;
        if latency != 0 {
            return latency;
        }
    }
    0
}

fn dsb_dewake_scanline_start(state: *mut IntelAtomicState, crtc: *mut IntelCrtc) -> i32 {
    let crtc_state = pre_commit_crtc_state(state, crtc);
    if crtc_state.is_null() {
        return 0;
    }
    let display =
        unsafe { crate::intel_display_conversion_upstream::__drm_to_display((*crtc).base.dev) };
    let mode = unsafe { ptr::addr_of!((*crtc_state).hw.adjusted_mode) };
    intel_mode_vdisplay(mode)
        - intel_usecs_to_scanlines(mode, unsafe { dsb_wm_max_latency(display, 0) as i32 })
}

fn dsb_dewake_scanline_end(state: *mut IntelAtomicState, crtc: *mut IntelCrtc) -> i32 {
    let crtc_state = pre_commit_crtc_state(state, crtc);
    if crtc_state.is_null() {
        return 0;
    }
    intel_mode_vdisplay(unsafe { ptr::addr_of!((*crtc_state).hw.adjusted_mode) })
}

fn dsb_scanline_to_hw(state: *mut IntelAtomicState, crtc: *mut IntelCrtc, scanline: i32) -> i32 {
    let vtotal = dsb_vtotal(state, crtc);
    if vtotal <= 0 || crtc.is_null() {
        return 0;
    }
    (scanline + vtotal - unsafe { (*crtc).scanline_offset }).rem_euclid(vtotal)
}

unsafe fn dsb_chicken(state: *mut IntelAtomicState, crtc: *mut IntelCrtc) -> u32 {
    if pre_commit_is_vrr_active(state, crtc) {
        DSB_CTRL_WAIT_SAFE_WINDOW
            | DSB_CTRL_NO_WAIT_VBLANK
            | DSB_INST_WAIT_SAFE_WINDOW
            | DSB_INST_NO_WAIT_VBLANK
    } else {
        0
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dsb_size(dsb: *mut IntelDsb) -> u32 {
    if dsb.is_null() {
        0
    } else {
        unsafe { (*dsb).free_pos.saturating_mul(4) }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dsb_head(dsb: *mut IntelDsb) -> u32 {
    if dsb.is_null() || unsafe { (*dsb).dsb_buf.is_null() } {
        return 0;
    }
    unsafe { dsb_buffer_ggtt_offset(dsb) }
}

#[inline]
unsafe fn intel_dsb_tail(dsb: *mut IntelDsb) -> u32 {
    unsafe { intel_dsb_head(dsb).wrapping_add(intel_dsb_size(dsb)) }
}

#[inline]
unsafe fn assert_dsb_has_room(dsb: *mut IntelDsb) -> bool {
    !dsb.is_null() && unsafe { (*dsb).size >= 2 && (*dsb).free_pos <= (*dsb).size - 2 }
}

#[inline]
unsafe fn assert_dsb_tail_is_aligned(dsb: *mut IntelDsb) -> bool {
    !dsb.is_null() && unsafe { ((*dsb).free_pos * 4) & (CACHELINE_BYTES - 1) == 0 }
}

unsafe fn intel_dsb_ins_align(dsb: *mut IntelDsb) {
    if dsb.is_null() {
        return;
    }
    unsafe { (*dsb).free_pos = ((*dsb).free_pos + 1) & !1 };
}

unsafe fn intel_dsb_emit(dsb: *mut IntelDsb, ldw: u32, udw: u32) {
    if !unsafe { assert_dsb_has_room(dsb) } {
        let _ = tk_linux_kapi::assert::warn_on(true);
        return;
    }
    unsafe { intel_dsb_ins_align(dsb) };
    if !unsafe { assert_dsb_has_room(dsb) } {
        let _ = tk_linux_kapi::assert::warn_on(true);
        return;
    }
    let pos = unsafe { (*dsb).free_pos };
    unsafe {
        (*dsb).ins_start_offset = pos;
        (*dsb).ins = [ldw, udw];
        dsb_buffer_write(dsb, pos, ldw);
        dsb_buffer_write(dsb, pos + 1, udw);
        (*dsb).free_pos = pos + 2;
    }
}

unsafe fn intel_dsb_prev_ins_is_write(dsb: *mut IntelDsb, opcode: u32, reg: I915RegT) -> bool {
    if dsb.is_null() || unsafe { (*dsb).free_pos == 0 } {
        return false;
    }
    let ins = unsafe { (*dsb).ins };
    (ins[1] & !DSB_REG_VALUE_MASK) == opcode && (ins[1] & DSB_REG_VALUE_MASK) == reg.reg
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dsb_reg_write_indexed(dsb: *mut IntelDsb, reg: I915RegT, val: u32) {
    if !unsafe {
        intel_dsb_prev_ins_is_write(dsb, DSB_OPCODE_INDEXED_WRITE << DSB_OPCODE_SHIFT, reg)
    } {
        unsafe {
            intel_dsb_emit(
                dsb,
                0,
                (DSB_OPCODE_INDEXED_WRITE << DSB_OPCODE_SHIFT) | reg.reg,
            )
        };
    }
    if !unsafe { assert_dsb_has_room(dsb) } {
        let _ = tk_linux_kapi::assert::warn_on(true);
        return;
    }
    unsafe {
        (*dsb).ins[0] = (*dsb).ins[0].saturating_add(1);
        dsb_buffer_write(dsb, (*dsb).ins_start_offset, (*dsb).ins[0]);
        dsb_buffer_write(dsb, (*dsb).free_pos, val);
        (*dsb).free_pos += 1;
        if (*dsb).free_pos & 1 != 0 {
            dsb_buffer_write(dsb, (*dsb).free_pos, 0);
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dsb_reg_write(dsb: *mut IntelDsb, reg: I915RegT, val: u32) {
    unsafe {
        intel_dsb_emit(
            dsb,
            val,
            (DSB_OPCODE_MMIO_WRITE << DSB_OPCODE_SHIFT) | (0xf << DSB_BYTE_EN_SHIFT) | reg.reg,
        )
    };
}

#[inline]
fn intel_dsb_mask_to_byte_en(mask: u32) -> u32 {
    (((mask & 0xff00_0000 != 0) as u32) << 3)
        | (((mask & 0x00ff_0000 != 0) as u32) << 2)
        | (((mask & 0x0000_ff00 != 0) as u32) << 1)
        | ((mask & 0x0000_00ff != 0) as u32)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dsb_reg_write_masked(
    dsb: *mut IntelDsb,
    reg: I915RegT,
    mask: u32,
    val: u32,
) {
    unsafe {
        intel_dsb_emit(
            dsb,
            val,
            (DSB_OPCODE_MMIO_WRITE << DSB_OPCODE_SHIFT)
                | (intel_dsb_mask_to_byte_en(mask) << DSB_BYTE_EN_SHIFT)
                | reg.reg,
        )
    };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dsb_noop(dsb: *mut IntelDsb, count: i32) {
    for _ in 0..count.max(0) {
        unsafe { intel_dsb_emit(dsb, 0, DSB_OPCODE_NOOP << DSB_OPCODE_SHIFT) };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dsb_nonpost_start(dsb: *mut IntelDsb) {
    if dsb.is_null() || unsafe { (*dsb).crtc.is_null() } {
        return;
    }
    let pipe = unsafe { (*(*dsb).crtc).pipe as u32 };
    unsafe {
        intel_dsb_reg_write_masked(
            dsb,
            DSB_CTRL(pipe, (*dsb).id),
            DSB_NON_POSTED,
            DSB_NON_POSTED,
        );
        intel_dsb_noop(dsb, 4);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dsb_nonpost_end(dsb: *mut IntelDsb) {
    if dsb.is_null() || unsafe { (*dsb).crtc.is_null() } {
        return;
    }
    let pipe = unsafe { (*(*dsb).crtc).pipe as u32 };
    unsafe {
        intel_dsb_reg_write_masked(dsb, DSB_CTRL(pipe, (*dsb).id), DSB_NON_POSTED, 0);
        intel_dsb_noop(dsb, 4);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dsb_interrupt(dsb: *mut IntelDsb) {
    unsafe { intel_dsb_emit(dsb, 0, DSB_OPCODE_INTERRUPT << DSB_OPCODE_SHIFT) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dsb_wait_usec(dsb: *mut IntelDsb, count: i32) {
    unsafe {
        intel_dsb_emit(
            dsb,
            count.saturating_add(1) as u32,
            DSB_OPCODE_WAIT_USEC << DSB_OPCODE_SHIFT,
        )
    };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dsb_wait_vblanks(dsb: *mut IntelDsb, count: i32) {
    unsafe {
        intel_dsb_emit(
            dsb,
            count as u32,
            DSB_OPCODE_WAIT_VBLANKS << DSB_OPCODE_SHIFT,
        )
    };
}

unsafe fn intel_dsb_emit_wait_dsl(dsb: *mut IntelDsb, opcode: u32, lower: i32, upper: i32) {
    let window = ((upper as u64) << DSB_SCANLINE_UPPER_SHIFT) | (lower as u32 as u64);
    unsafe {
        intel_dsb_emit(
            dsb,
            window as u32,
            (opcode << DSB_OPCODE_SHIFT) | (window >> 32) as u32,
        )
    };
}

unsafe fn intel_dsb_wait_dsl(
    state: *mut IntelAtomicState,
    dsb: *mut IntelDsb,
    lower_in: i32,
    upper_in: i32,
    lower_out: i32,
    upper_out: i32,
) {
    let crtc = unsafe { (*dsb).crtc };
    let lower_in = dsb_scanline_to_hw(state, crtc, lower_in);
    let upper_in = dsb_scanline_to_hw(state, crtc, upper_in);
    let lower_out = dsb_scanline_to_hw(state, crtc, lower_out);
    let upper_out = dsb_scanline_to_hw(state, crtc, upper_out);
    if upper_in >= lower_in {
        unsafe { intel_dsb_emit_wait_dsl(dsb, DSB_OPCODE_WAIT_DSL_IN, lower_in, upper_in) };
    } else if upper_out >= lower_out {
        unsafe { intel_dsb_emit_wait_dsl(dsb, DSB_OPCODE_WAIT_DSL_OUT, lower_out, upper_out) };
    } else {
        let _ = tk_linux_kapi::assert::warn_on(true);
    }
}

unsafe fn assert_dsl_ok(state: *mut IntelAtomicState, dsb: *mut IntelDsb, start: i32, end: i32) {
    let crtc = unsafe { (*dsb).crtc };
    let vtotal = dsb_vtotal(state, crtc);
    if vtotal > 0 {
        let _ =
            tk_linux_kapi::assert::warn_on((end - start + vtotal).rem_euclid(vtotal) == vtotal - 1);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dsb_wait_scanline_in(
    state: *mut IntelAtomicState,
    dsb: *mut IntelDsb,
    start: i32,
    end: i32,
) {
    unsafe {
        assert_dsl_ok(state, dsb, start, end);
        intel_dsb_wait_dsl(state, dsb, start, end, end + 1, start - 1);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dsb_wait_scanline_out(
    state: *mut IntelAtomicState,
    dsb: *mut IntelDsb,
    start: i32,
    end: i32,
) {
    unsafe {
        assert_dsl_ok(state, dsb, start, end);
        intel_dsb_wait_dsl(state, dsb, end + 1, start - 1, start, end);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dsb_poll(
    dsb: *mut IntelDsb,
    reg: I915RegT,
    mask: u32,
    val: u32,
    wait_us: i32,
    count: i32,
) {
    if dsb.is_null() || unsafe { (*dsb).crtc.is_null() } {
        return;
    }
    let pipe = unsafe { (*(*dsb).crtc).pipe as u32 };
    unsafe {
        intel_dsb_reg_write(dsb, DSB_POLLMASK(pipe, (*dsb).id), mask);
        intel_dsb_reg_write(
            dsb,
            DSB_POLLFUNC(pipe, (*dsb).id),
            DSB_POLL_ENABLE
                | DSB_POLL_WAIT(wait_us.max(0) as u32)
                | DSB_POLL_COUNT(count.max(0) as u32),
        );
        intel_dsb_noop(dsb, 5);
        intel_dsb_emit(dsb, val, (DSB_OPCODE_POLL << DSB_OPCODE_SHIFT) | reg.reg);
    }
}

unsafe fn intel_dsb_align_tail(dsb: *mut IntelDsb) -> bool {
    if dsb.is_null() {
        return false;
    }
    unsafe { intel_dsb_ins_align(dsb) };
    let tail = unsafe { (*dsb).free_pos.saturating_mul(4) };
    let Some(aligned_tail) = align_up(tail, CACHELINE_BYTES) else {
        return false;
    };
    if aligned_tail > unsafe { (*dsb).size.saturating_mul(4) } {
        return false;
    }
    if aligned_tail > tail {
        unsafe { dsb_buffer_fill(dsb, (*dsb).free_pos, 0, (aligned_tail - tail) as usize) };
    }
    unsafe { (*dsb).free_pos = aligned_tail / 4 };
    true
}

unsafe fn intel_dsb_gosub_align(dsb: *mut IntelDsb) -> bool {
    if dsb.is_null() {
        return false;
    }
    unsafe { intel_dsb_ins_align(dsb) };
    let tail = unsafe { (*dsb).free_pos.saturating_mul(4) };
    let Some(aligned_tail) = align_up(tail, CACHELINE_BYTES) else {
        return false;
    };
    // Wa_16024917128: do not place GOSUB in the final two QW slots.
    if aligned_tail - tail <= 16 {
        if aligned_tail > unsafe { (*dsb).size.saturating_mul(4) } {
            return false;
        }
        unsafe { dsb_buffer_fill(dsb, (*dsb).free_pos, 0, (aligned_tail - tail) as usize) };
        unsafe { (*dsb).free_pos = aligned_tail / 4 };
    }
    unsafe { assert_dsb_has_room(dsb) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dsb_gosub(dsb: *mut IntelDsb, sub_dsb: *mut IntelDsb) {
    if dsb.is_null() || sub_dsb.is_null() {
        return;
    }
    if unsafe { (*dsb).id != (*sub_dsb).id || !assert_dsb_tail_is_aligned(sub_dsb) } {
        let _ = tk_linux_kapi::assert::warn_on(true);
        return;
    }
    if !unsafe { intel_dsb_gosub_align(dsb) } {
        return;
    }
    let head = unsafe { intel_dsb_head(sub_dsb) };
    let tail = unsafe { intel_dsb_tail(sub_dsb) };
    let head_tail = (((head >> 6) as u64) << DSB_GOSUB_HEAD_SHIFT) | ((tail >> 6) as u64);
    unsafe {
        intel_dsb_emit(
            dsb,
            head_tail as u32,
            (DSB_OPCODE_GOSUB << DSB_OPCODE_SHIFT) | (head_tail >> 32) as u32,
        );
        intel_dsb_align_tail(dsb);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dsb_gosub_finish(dsb: *mut IntelDsb) {
    if unsafe { !intel_dsb_align_tail(dsb) } {
        return;
    }
    unsafe { intel_dsb_noop(dsb, 8) };
    let _ = unsafe { dsb_buffer_flush_map(dsb) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dsb_finish(dsb: *mut IntelDsb) {
    if unsafe { !intel_dsb_align_tail(dsb) } {
        let _ = tk_linux_kapi::assert::warn_on(true);
        return;
    }
    let _ = unsafe { dsb_buffer_flush_map(dsb) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dsb_exec_time_us() -> i32 {
    80 + 20
}

unsafe fn dsb_error_int_status(display: *mut IntelDisplay) -> u32 {
    let mut errors = DSB_GTT_FAULT_INT_STATUS | DSB_RSPTIMEOUT_INT_STATUS | DSB_POLL_ERR_INT_STATUS;
    if !display.is_null() && unsafe { DISPLAY_VER(display) >= 14 } {
        errors |= DSB_ATS_FAULT_INT_STATUS;
    }
    if !display.is_null() && unsafe { DISPLAY_VER(display) >= 30 } {
        errors |= DSB_GOSUB_INT_STATUS;
    }
    errors
}

unsafe fn dsb_error_int_en(display: *mut IntelDisplay) -> u32 {
    let mut errors = DSB_GTT_FAULT_INT_EN | DSB_RSPTIMEOUT_INT_EN | DSB_POLL_ERR_INT_EN;
    if !display.is_null() && unsafe { DISPLAY_VER(display) >= 14 } {
        errors |= DSB_ATS_FAULT_INT_EN;
    }
    errors
}

unsafe fn is_dsb_busy(display: *mut IntelDisplay, pipe: u32, id: u32) -> bool {
    !display.is_null()
        && unsafe {
            crate::intel_de_upstream::intel_de_read_fw(display, DSB_CTRL(pipe, id))
                & DSB_STATUS_BUSY
                != 0
        }
}

/// Poll for completion and then disable/reset the engine.  On timeout, halt
/// and poll once more; callers must retain the backing map if the busy bit
/// remains set because the engine may still be reading it.
unsafe fn dsb_wait_quiesced(dsb: *mut IntelDsb) -> bool {
    if dsb.is_null() {
        return true;
    }
    if unsafe { (*dsb).display.is_null() || (*dsb).crtc.is_null() } {
        return unsafe { (*dsb).dsb_buf.is_null() && !(*dsb).submitted };
    }
    let display = unsafe { (*dsb).display };
    let pipe = unsafe { (*(*dsb).crtc).pipe as u32 };
    let id = unsafe { (*dsb).id };
    let mut idle = false;
    for _ in 0..10 {
        if !unsafe { is_dsb_busy(display, pipe, id) } {
            idle = true;
            break;
        }
        udelay(100);
    }
    if !idle {
        unsafe {
            crate::intel_de_upstream::intel_de_write_fw(
                display,
                DSB_CTRL(pipe, id),
                DSB_ENABLE | DSB_HALT,
            )
        };
        for _ in 0..10 {
            if !unsafe { is_dsb_busy(display, pipe, id) } {
                idle = true;
                break;
            }
            udelay(100);
        }
    }
    if !idle {
        return false;
    }

    unsafe { crate::intel_de_upstream::intel_de_write_fw(display, DSB_CTRL(pipe, id), 0) };
    if unsafe { is_dsb_busy(display, pipe, id) } {
        return false;
    }
    let status = unsafe { dsb_error_int_status(display) | DSB_PROG_INT_STATUS };
    unsafe {
        crate::intel_de_upstream::intel_de_write_fw(display, DSB_INTERRUPT(pipe, id), status)
    };
    unsafe {
        (*dsb).submitted = false;
        (*dsb).free_pos = 0;
        (*dsb).ins_start_offset = 0;
        (*dsb).ins = [0; 2];
    }
    true
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dsb_commit(dsb: *mut IntelDsb) {
    if dsb.is_null() || unsafe { (*dsb).display.is_null() || (*dsb).crtc.is_null() } {
        return;
    }
    if !unsafe { assert_dsb_tail_is_aligned(dsb) } {
        let _ = tk_linux_kapi::assert::warn_on(true);
        return;
    }
    let display = unsafe { (*dsb).display };
    let pipe = unsafe { (*(*dsb).crtc).pipe as u32 };
    let id = unsafe { (*dsb).id };
    if unsafe { is_dsb_busy(display, pipe, id) } {
        let _ = tk_linux_kapi::assert::warn_on(true);
        return;
    }
    let head = unsafe { intel_dsb_head(dsb) };
    let tail = unsafe { intel_dsb_tail(dsb) };
    if head & (CACHELINE_BYTES - 1) != 0 || tail < head || tail & (CACHELINE_BYTES - 1) != 0 {
        let _ = tk_linux_kapi::assert::warn_on(true);
        return;
    }

    unsafe {
        crate::intel_de_upstream::intel_de_write_fw(display, DSB_CTRL(pipe, id), DSB_ENABLE);
        crate::intel_de_upstream::intel_de_write_fw(display, DSB_CHICKEN(pipe, id), (*dsb).chicken);
        crate::intel_de_upstream::intel_de_write_fw(
            display,
            DSB_INTERRUPT(pipe, id),
            dsb_error_int_status(display)
                | DSB_PROG_INT_STATUS
                | dsb_error_int_en(display)
                | DSB_PROG_INT_EN,
        );
        crate::intel_de_upstream::intel_de_write_fw(display, DSB_PMCTRL(pipe, id), 0);
        crate::intel_de_upstream::intel_de_write_fw(display, DSB_HEAD(pipe, id), head);
        crate::intel_de_upstream::intel_de_write_fw(display, DSB_TAIL(pipe, id), tail);
        (*dsb).submitted = true;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dsb_wait(dsb: *mut IntelDsb) {
    if dsb.is_null() {
        return;
    }
    if !unsafe { dsb_wait_quiesced(dsb) } {
        // Preserve the DSB object and parent-owned map: freeing either while
        // hardware remains busy would allow DMA into reclaimed memory.
        let _ = tk_linux_kapi::assert::warn_on(true);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dsb_prepare(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
    dsb_id: u32,
    max_cmds: u32,
) -> *mut IntelDsb {
    if state.is_null() || crtc.is_null() || dsb_id >= 3 || max_cmds == 0 {
        return ptr::null_mut();
    }
    let display = unsafe { dsb_display_from_state(state) };
    if display.is_null()
        || unsafe { (*crtc).base.dev.is_null() }
        || unsafe {
            crate::intel_display_conversion_upstream::__drm_to_display((*crtc).base.dev) != display
        }
        || !unsafe { display_has_dsb(display) }
        || !unsafe { (*display).params.enable_dsb }
        || new_crtc_state(state, crtc).is_null()
        || old_crtc_state(state, crtc).is_null()
    {
        return ptr::null_mut();
    }
    let Some(ops) = (unsafe { dsb_ops_required(display) }) else {
        return ptr::null_mut();
    };
    let Some(create) = ops.create else {
        return ptr::null_mut();
    };
    let Some(cleanup) = ops.cleanup else {
        return ptr::null_mut();
    };
    let Some(rpm) = (unsafe { intel_display_rpm_ops(display) }) else {
        return ptr::null_mut();
    };

    // Linux allocates roughly one qword per instruction and rounds the map to
    // complete cachelines. Refuse overflow rather than undersize the DMA map.
    let Some(bytes) = max_cmds
        .checked_mul(8)
        .and_then(|size| align_up(size, CACHELINE_BYTES))
    else {
        return ptr::null_mut();
    };
    let dsb = unsafe { kmalloc_obj::<IntelDsb>(GFP_KERNEL) };
    if dsb.is_null() {
        return ptr::null_mut();
    }
    unsafe { ptr::write_bytes(dsb, 0, 1) };

    let wakeref = unsafe { (rpm.0)((*display).drm) };
    if wakeref.is_null() {
        unsafe { kfree(dsb.cast::<c_void>()) };
        return ptr::null_mut();
    }
    let buffer = unsafe { create(unsafe { (*display).drm }, bytes as usize) };
    if buffer.is_null() || tk_linux_kapi::config::IS_ERR(buffer) {
        unsafe {
            (rpm.1)((*display).drm, wakeref);
            kfree(dsb.cast::<c_void>());
        }
        return ptr::null_mut();
    }

    let dword_size = bytes / 4;
    unsafe {
        (*dsb).id = dsb_id;
        (*dsb).dsb_buf = buffer;
        (*dsb).crtc = crtc;
        (*dsb).display = display;
        (*dsb).size = dword_size;
        (*dsb).chicken = dsb_chicken(state, crtc);
        let scanline = dsb_scanline_to_hw(state, crtc, dsb_dewake_scanline_start(state, crtc));
        (*dsb).hw_dewake_scanline = scanline;
        (rpm.1)((*display).drm, wakeref);
    }
    let offset = unsafe { ops.ggtt_offset.unwrap_unchecked()(buffer) };
    if offset & (CACHELINE_BYTES - 1) != 0 || offset.checked_add(bytes).is_none() {
        unsafe {
            cleanup(buffer);
            kfree(dsb.cast::<c_void>());
        }
        return ptr::null_mut();
    }
    dsb
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dsb_cleanup(dsb: *mut IntelDsb) {
    if dsb.is_null() {
        return;
    }
    let needs_quiesce = unsafe {
        (*dsb).submitted
            || (!(*dsb).display.is_null()
                && !(*dsb).crtc.is_null()
                && is_dsb_busy((*dsb).display, (*(*dsb).crtc).pipe as u32, (*dsb).id))
    };
    if needs_quiesce && !unsafe { dsb_wait_quiesced(dsb) } {
        // Deliberately keep the DMA map and context allocated when hardware
        // cannot be stopped/observed idle. A leak is safer than a DMA UAF.
        return;
    }
    if unsafe { !dsb_buffer_cleanup(dsb) } {
        return;
    }
    unsafe { kfree(dsb.cast::<c_void>()) };
}

unsafe fn intel_dsb_wait_scanline_window(
    state: *mut IntelAtomicState,
    dsb: *mut IntelDsb,
    start: i32,
    end: i32,
) {
    unsafe { intel_dsb_wait_scanline_out(state, dsb, start, end) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dsb_vblank_evade(state: *mut IntelAtomicState, dsb: *mut IntelDsb) {
    if state.is_null() || dsb.is_null() {
        return;
    }
    let crtc = unsafe { (*dsb).crtc };
    let crtc_state = pre_commit_crtc_state(state, crtc);
    if crtc_state.is_null() {
        return;
    }
    let mode = unsafe { ptr::addr_of!((*crtc_state).hw.adjusted_mode) };
    let arm_latency = intel_usecs_to_scanlines(mode, 20);

    if pre_commit_is_vrr_active(state, crtc) {
        let vmin_start = unsafe { (*crtc_state).vrr.vmin as i32 };
        let vmax_start = unsafe { (*crtc_state).vrr.vmax as i32 };
        let delay = unsafe { (*crtc_state).set_context_latency as i32 };
        if vmin_start > 0 {
            unsafe {
                intel_dsb_wait_scanline_window(
                    state,
                    dsb,
                    vmin_start - delay - arm_latency,
                    vmin_start,
                )
            };
        }
        if vmax_start > 0 {
            unsafe {
                intel_dsb_wait_scanline_window(
                    state,
                    dsb,
                    vmax_start - delay - arm_latency,
                    vmax_start,
                )
            };
        }
    } else {
        let end = intel_mode_vblank_start(mode);
        let start = end - intel_mode_vblank_delay(mode) - arm_latency;
        unsafe { intel_dsb_wait_scanline_window(state, dsb, start, end) };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dsb_wait_for_delayed_vblank(
    state: *mut IntelAtomicState,
    dsb: *mut IntelDsb,
) {
    if state.is_null() || dsb.is_null() {
        return;
    }
    let crtc = unsafe { (*dsb).crtc };
    let crtc_state = pre_commit_crtc_state(state, crtc);
    if crtc_state.is_null() {
        return;
    }
    let mode = unsafe { ptr::addr_of!((*crtc_state).hw.adjusted_mode) };
    let wait_scanlines = if pre_commit_is_vrr_active(state, crtc) {
        // TRANS_PUSH can be observed one scanline after it was written.
        unsafe { (*crtc_state).set_context_latency as i32 + 1 }
    } else {
        intel_mode_vblank_delay(mode)
    };
    unsafe { intel_dsb_wait_usec(dsb, intel_scanlines_to_usecs(mode, wait_scanlines)) };
}

unsafe fn intel_dsb_chain_internal(
    state: *mut IntelAtomicState,
    dsb: *mut IntelDsb,
    chained: *mut IntelDsb,
    ctrl: u32,
) {
    if state.is_null() || dsb.is_null() || chained.is_null() {
        return;
    }
    let crtc = unsafe { (*dsb).crtc };
    if crtc.is_null() || unsafe { (*dsb).id == (*chained).id } {
        let _ = tk_linux_kapi::assert::warn_on(true);
        return;
    }
    if unsafe { !assert_dsb_tail_is_aligned(chained) } {
        return;
    }
    let pipe = unsafe { (*crtc).pipe as u32 };
    let chained_id = unsafe { (*chained).id };
    let id = unsafe { (*dsb).id };
    let display = unsafe { (*dsb).display };
    unsafe {
        intel_dsb_reg_write(dsb, DSB_CTRL(pipe, chained_id), ctrl | DSB_ENABLE);
        intel_dsb_reg_write(dsb, DSB_CHICKEN(pipe, chained_id), dsb_chicken(state, crtc));
        intel_dsb_reg_write(
            dsb,
            DSB_INTERRUPT(pipe, chained_id),
            dsb_error_int_status(display)
                | DSB_PROG_INT_STATUS
                | dsb_error_int_en(display)
                | DSB_PROG_INT_EN,
        );
        if ctrl & DSB_WAIT_FOR_VBLANK != 0 {
            let scanline = dsb_scanline_to_hw(state, crtc, dsb_dewake_scanline_start(state, crtc));
            intel_dsb_reg_write(
                dsb,
                DSB_PMCTRL(pipe, chained_id),
                DSB_ENABLE_DEWAKE | DSB_SCANLINE_FOR_DEWAKE(scanline as u32),
            );
        } else {
            intel_dsb_reg_write(dsb, DSB_PMCTRL(pipe, chained_id), 0);
        }
        intel_dsb_reg_write(dsb, DSB_HEAD(pipe, chained_id), intel_dsb_head(chained));
        intel_dsb_reg_write(dsb, DSB_TAIL(pipe, chained_id), intel_dsb_tail(chained));
        if ctrl & DSB_WAIT_FOR_VBLANK != 0 {
            intel_dsb_reg_write_masked(
                dsb,
                DSB_PMCTRL_2(pipe, id),
                DSB_FORCE_DEWAKE,
                DSB_FORCE_DEWAKE,
            );
            intel_dsb_wait_scanline_out(
                state,
                dsb,
                dsb_dewake_scanline_start(state, crtc),
                dsb_dewake_scanline_end(state, crtc),
            );
            intel_dsb_reg_write_masked(dsb, DSB_PMCTRL_2(pipe, id), DSB_FORCE_DEWAKE, 0);
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dsb_chain(
    state: *mut IntelAtomicState,
    dsb: *mut IntelDsb,
    chained_dsb: *mut IntelDsb,
    wait_for_vblank: bool,
) {
    unsafe {
        intel_dsb_chain_internal(
            state,
            dsb,
            chained_dsb,
            if wait_for_vblank {
                DSB_WAIT_FOR_VBLANK
            } else {
                0
            },
        )
    };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dsb_irq_handler(display: *mut IntelDisplay, pipe: i32, dsb_id: u32) {
    if display.is_null() || pipe < 0 || dsb_id >= 3 {
        return;
    }
    let pipe = pipe as u32;
    let tmp =
        unsafe { crate::intel_de_upstream::intel_de_read_fw(display, DSB_INTERRUPT(pipe, dsb_id)) };
    unsafe {
        crate::intel_de_upstream::intel_de_write_fw(display, DSB_INTERRUPT(pipe, dsb_id), tmp)
    };
    if tmp & DSB_PROG_INT_STATUS == 0 {
        return;
    }
    let crtc =
        unsafe { crate::display_pending_stubs::b4::intel_crtc_for_pipe(display, pipe as i32) };
    if crtc.is_null() {
        return;
    }
    let drm = unsafe { (*display).drm };
    if drm.is_null() {
        return;
    }
    unsafe { spin_lock(&mut (*drm).event_lock) };
    let event = unsafe { (*crtc).dsb_event };
    if !event.is_null() {
        unsafe {
            tk_drm_upstream::drm_vblank_upstream::drm_crtc_accurate_vblank_count(&mut (*crtc).base);
            tk_drm_upstream::drm_vblank_upstream::drm_crtc_send_vblank_event(
                &mut (*crtc).base,
                event,
            );
            (*crtc).dsb_event = ptr::null_mut::<DrmPendingVblankEvent>();
        }
    }
    unsafe { tk_linux_kapi::locks::spin_unlock(&mut (*drm).event_lock) };
}
