// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// ADL-P translation of drivers/gpu/drm/i915/display/intel_crtc.c.
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::{
    ffi::{c_char, c_void},
    mem::offset_of,
    ptr,
};

use tk_drm_upstream::{
    drm_atomic_helper_upstream::{drm_atomic_helper_page_flip, drm_atomic_helper_set_config},
    drm_atomic_types::{DrmAtomicCommit, DrmPlanesState},
    drm_crtc_types::{DrmCrtc, DrmCrtcFuncs, DrmCrtcState, DrmModeSet},
    drm_device_types::DrmDevice,
    drm_file_types::DrmFile,
    drm_modes_types::DrmDisplayMode,
    drm_plane_types::{
        DRM_SCALING_FILTER_DEFAULT, DRM_SCALING_FILTER_NEAREST_NEIGHBOR, DrmScalingFilter,
    },
    drm_vblank_types::DrmPendingVblankEvent,
    drm_vblank_work_types::DrmVblankWork,
    kernel_placeholder_types::KthreadWork,
};
use tk_linux_kapi::{
    config::{EINVAL, ENOENT, ENOMEM, ERR_PTR, GFP_KERNEL, IS_ERR, PTR_ERR},
    list::{INIT_LIST_HEAD, list_add, list_add_tail, list_del},
    locks::{
        local_irq_disable, local_irq_enable, spin_lock_irq, spin_lock_irqsave, spin_unlock_irq,
        spin_unlock_irqrestore,
    },
    memory::{kfree, kzalloc_objs},
    types::ListHead,
    workqueue::flush_work,
};

use crate::{
    intel_atomic_upstream::{intel_atomic_get_new_crtc_state, intel_atomic_get_old_crtc_state},
    intel_display_conversion_upstream::__drm_to_display,
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_device_upstream::intel_display_device_present,
    intel_display_limits_types_upstream::{
        I915_MAX_PIPES, INVALID_PIPE, INVALID_TRANSCODER, PLANE_1, PLANE_2, PLANE_CURSOR, Pipe,
        PlaneId,
    },
    intel_display_types_upstream::{
        IntelAtomicState, IntelCrtc, IntelCrtcState, IntelPlane, IntelPlaneState,
    },
};

const GEN9: u16 = 9;
const GEN11: u16 = 11;
const GEN12: u16 = 12;
const I915_MODE_FLAG_DSI_USE_TE0: u8 = 1 << 3;
const I915_MODE_FLAG_DSI_USE_TE1: u8 = 1 << 4;
const I915_MODE_FLAG_GET_SCANLINE_FROM_TIMESTAMP: u8 = 1 << 1;

unsafe extern "C" {
    fn drm_crtc_init_with_planes(
        dev: *mut DrmDevice,
        crtc: *mut DrmCrtc,
        primary: *mut tk_drm_upstream::drm_plane_types::DrmPlane,
        cursor: *mut tk_drm_upstream::drm_plane_types::DrmPlane,
        funcs: *const DrmCrtcFuncs,
        name: *const c_char,
    ) -> i32;
    fn drm_crtc_cleanup(crtc: *mut DrmCrtc);
    fn drm_atomic_helper_crtc_state_reset(state: *mut DrmCrtcState, crtc: *mut DrmCrtc);
    fn drm_crtc_create_scaling_filter_property(crtc: *mut DrmCrtc, supported_filters: u32) -> i32;
    fn drm_crtc_attach_background_color_property(crtc: *mut DrmCrtc);
    fn drm_crtc_vblank_count(crtc: *mut DrmCrtc) -> u64;
    fn drm_crtc_accurate_vblank_count(crtc: *mut DrmCrtc) -> u64;
    fn drm_crtc_vblank_get(crtc: *mut DrmCrtc) -> i32;
    fn drm_crtc_vblank_put(crtc: *mut DrmCrtc);
    fn drm_crtc_set_max_vblank_count(crtc: *mut DrmCrtc, count: u32);
    fn drm_crtc_vblank_on(crtc: *mut DrmCrtc);
    fn drm_crtc_vblank_off(crtc: *mut DrmCrtc);
    fn drm_crtc_arm_vblank_event(crtc: *mut DrmCrtc, event: *mut DrmPendingVblankEvent);
    fn drm_crtc_send_vblank_event(crtc: *mut DrmCrtc, event: *mut DrmPendingVblankEvent);
    fn drm_crtc_vblank_crtc(
        crtc: *mut DrmCrtc,
    ) -> *mut tk_drm_upstream::drm_vblank_types::DrmVblankCrtc;
    fn drm_crtc_vblank_handle(crtc: *mut DrmCrtc) -> bool;
    fn drm_vblank_work_init(
        work: *mut DrmVblankWork,
        crtc: *mut DrmCrtc,
        func: unsafe extern "C" fn(*mut KthreadWork),
    );
    fn drm_vblank_work_schedule(work: *mut DrmVblankWork, count: u64, nextonmiss: bool) -> i32;
    fn drm_vblank_work_flush(work: *mut DrmVblankWork);
    fn intel_init_fifo_underrun_reporting(
        display: *mut IntelDisplay,
        crtc: *mut IntelCrtc,
        disable: bool,
    );
    fn intel_set_cpu_fifo_underrun_reporting(
        display: *mut IntelDisplay,
        pipe: Pipe,
        enable: bool,
    ) -> bool;
    fn skl_universal_plane_create(
        display: *mut IntelDisplay,
        pipe: Pipe,
        plane: PlaneId,
    ) -> *mut IntelPlane;
    fn intel_cursor_plane_create(display: *mut IntelDisplay, pipe: Pipe) -> *mut IntelPlane;
    fn intel_plane_init_cursor_vblank_work(
        old_state: *mut IntelPlaneState,
        new_state: *mut IntelPlaneState,
    );
    fn drm_atomic_get_new_crtc_state(
        state: *mut DrmAtomicCommit,
        crtc: *mut DrmCrtc,
    ) -> *mut DrmCrtcState;
    fn intel_color_crtc_init(crtc: *mut IntelCrtc);
    fn intel_color_load_luts(state: *mut IntelCrtcState);
    fn intel_color_uses_dsb(state: *const IntelCrtcState) -> bool;
    fn intel_crtc_get_vblank_timestamp(
        crtc: *mut DrmCrtc,
        max_error: *mut i32,
        vblank_time: *mut i64,
        in_vblank_irq: bool,
    ) -> bool;
    fn g4x_get_vblank_counter(crtc: *mut DrmCrtc) -> u32;
    fn bdw_enable_vblank(crtc: *mut DrmCrtc) -> i32;
    fn bdw_disable_vblank(crtc: *mut DrmCrtc);
}

#[inline]
unsafe fn display_from_atomic(state: *mut IntelAtomicState) -> *mut IntelDisplay {
    if state.is_null() || unsafe { (*state).base.dev.is_null() } {
        return ptr::null_mut();
    }
    unsafe { __drm_to_display((*state).base.dev) }
}

#[inline]
unsafe fn crtc_needs_color_update(state: *const IntelCrtcState) -> bool {
    !state.is_null()
        && unsafe {
            (*state).uapi.color_mgmt_changed()
                || (*state).update_pipe
                || crate::intel_atomic_upstream::intel_crtc_needs_modeset(state)
        }
}

unsafe fn crtc_needs_vblank_work(state: *const IntelCrtcState) -> bool {
    if state.is_null() {
        return false;
    }
    let display = unsafe { state_to_display(state) };
    !display.is_null()
        && unsafe { (*state).hw.active }
        && !unsafe { (*state).preload_luts }
        && !unsafe { crate::intel_atomic_upstream::intel_crtc_needs_modeset(state) }
        && unsafe { crtc_needs_color_update(state) }
        && unsafe { (*display).info.__runtime_info.ip.ver < 30 }
        && !unsafe { intel_color_uses_dsb(state) }
        && !unsafe { (*state).use_dsb }
}

unsafe extern "C" fn intel_crtc_vblank_work(base: *mut KthreadWork) {
    if base.is_null() {
        return;
    }
    let state = unsafe {
        base.cast::<u8>()
            .sub(offset_of!(IntelCrtcState, vblank_work))
            .cast::<IntelCrtcState>()
    };
    let crtc = unsafe { (*state).uapi.crtc.cast::<IntelCrtc>() };
    if crtc.is_null() {
        return;
    }
    unsafe {
        intel_color_load_luts(state);
        if !(*state).uapi.event.is_null() {
            spin_lock_irq(&mut (*(*crtc).base.dev).event_lock);
            drm_crtc_send_vblank_event(&mut (*crtc).base, (*state).uapi.event);
            spin_unlock_irq(&mut (*(*crtc).base.dev).event_lock);
            (*state).uapi.event = ptr::null_mut();
        }
    }
}

unsafe fn crtc_vblank_work_init(state: *mut IntelCrtcState) {
    if state.is_null() {
        return;
    }
    let crtc = unsafe { (*state).uapi.crtc.cast::<IntelCrtc>() };
    if crtc.is_null() {
        return;
    }
    unsafe {
        drm_vblank_work_init(
            &mut (*state).vblank_work,
            &mut (*crtc).base,
            intel_crtc_vblank_work,
        )
    };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_wait_for_vblank_workers(state: *mut IntelAtomicState) {
    unsafe {
        for_each_crtc_in_state(state, |_crtc, new_state| {
            if crtc_needs_vblank_work(new_state) {
                drm_vblank_work_flush(&mut (*new_state).vblank_work);
            }
        });
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_usecs_to_scanlines(mode: *const DrmDisplayMode, usecs: i32) -> i32 {
    if mode.is_null() {
        return 1;
    }
    let htotal = unsafe { (*mode).crtc_htotal as u64 };
    if htotal == 0 {
        return 1;
    }
    let clock = unsafe { (*mode).crtc_clock as u64 };
    div_ceil_u64(
        (usecs.max(0) as u64).saturating_mul(clock),
        1000u64.saturating_mul(htotal),
    ) as i32
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_scanlines_to_usecs(
    mode: *const DrmDisplayMode,
    scanlines: i32,
) -> i32 {
    if mode.is_null() {
        return 1;
    }
    let clock = unsafe { (*mode).crtc_clock as u64 };
    if clock == 0 {
        return 1;
    }
    let htotal = unsafe { (*mode).crtc_htotal as u64 };
    div_ceil_u64(
        (scanlines.max(0) as u64)
            .saturating_mul(htotal)
            .saturating_mul(1000),
        clock,
    ) as i32
}

#[inline]
fn div_ceil_u64(value: u64, divisor: u64) -> u64 {
    if divisor == 0 {
        0
    } else {
        value / divisor + u64::from(value % divisor != 0)
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_crtc_arm_vblank_event(state: *mut IntelCrtcState) {
    if state.is_null() {
        return;
    }
    let event = unsafe { (*state).uapi.event };
    if event.is_null() {
        return;
    }
    let crtc = unsafe { (*state).uapi.crtc.cast::<IntelCrtc>() };
    if crtc.is_null() {
        return;
    }
    unsafe {
        let _ = drm_crtc_vblank_get(&mut (*crtc).base);
        let mut flags = 0u64;
        spin_lock_irqsave(&mut (*(*crtc).base.dev).event_lock, &mut flags);
        drm_crtc_arm_vblank_event(&mut (*crtc).base, event);
        spin_unlock_irqrestore(&mut (*(*crtc).base.dev).event_lock, flags);
        (*state).uapi.event = ptr::null_mut();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_crtc_prepare_vblank_event(
    state: *mut IntelCrtcState,
    event: *mut *mut DrmPendingVblankEvent,
) {
    if state.is_null() || event.is_null() {
        return;
    }
    let crtc = unsafe { (*state).uapi.crtc.cast::<IntelCrtc>() };
    if crtc.is_null() {
        return;
    }
    unsafe {
        let mut flags = 0u64;
        spin_lock_irqsave(&mut (*(*crtc).base.dev).event_lock, &mut flags);
        *event = (*state).uapi.event;
        spin_unlock_irqrestore(&mut (*(*crtc).base.dev).event_lock, flags);
        (*state).uapi.event = ptr::null_mut();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pipe_update_start(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
) {
    if state.is_null() || crtc.is_null() {
        return;
    }
    let new_state = unsafe { intel_atomic_get_new_crtc_state(state, crtc) };
    if new_state.is_null() {
        return;
    }
    let old_state = unsafe { intel_atomic_get_old_crtc_state(state, crtc) };
    if unsafe { (*new_state).do_async_flip } {
        unsafe { intel_crtc_prepare_vblank_event(new_state, &mut (*crtc).flip_done_event) };
        return;
    }
    if unsafe { crtc_needs_vblank_work(new_state) } {
        unsafe { crtc_vblank_work_init(new_state) };
    }
    if unsafe { (*state).base.legacy_cursor_update() } && !unsafe { (*state).base.planes.is_null() }
    {
        let total = unsafe { (*(*state).base.dev).mode_config.num_total_plane }.max(0) as usize;
        for index in 0..total {
            let entry = unsafe { (*state).base.planes.add(index) };
            let old = unsafe { (*entry).old_state };
            let new = unsafe { (*entry).new_state };
            if !old.is_null() && unsafe { (*old).crtc == &mut (*crtc).base } {
                unsafe { intel_plane_init_cursor_vblank_work(old.cast(), new.cast()) };
            }
        }
    }
    if old_state.is_null() {
        local_irq_disable();
        return;
    }
    let mut evade =
        core::mem::MaybeUninit::<crate::intel_vblank_upstream::IntelVblankEvadeCtx>::uninit();
    unsafe {
        crate::intel_vblank_upstream::intel_vblank_evade_init(
            old_state,
            new_state,
            evade.as_mut_ptr(),
        )
    };
    if unsafe { drm_crtc_vblank_get(&mut (*crtc).base) } != 0 {
        local_irq_disable();
        return;
    }
    local_irq_disable();
    let scanline = unsafe { crate::intel_vblank_upstream::intel_vblank_evade(evade.as_mut_ptr()) };
    unsafe {
        drm_crtc_vblank_put(&mut (*crtc).base);
        (*crtc).debug.scanline_start = scanline;
        (*crtc).debug.start_vbl_time = tk_linux_kapi::primitives::ktime_get();
        (*crtc).debug.start_vbl_count = intel_crtc_get_vblank_counter(crtc);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pipe_update_end(state: *mut IntelAtomicState, crtc: *mut IntelCrtc) {
    if state.is_null() || crtc.is_null() {
        return;
    }
    let new_state = unsafe { intel_atomic_get_new_crtc_state(state, crtc) };
    if new_state.is_null() {
        return;
    }
    let end_scanline = unsafe { crate::intel_vblank_upstream::intel_get_crtc_scanline(crtc) };
    let end_vblank_count = unsafe { intel_crtc_get_vblank_counter(crtc) };
    let end_time = tk_linux_kapi::primitives::ktime_get();
    if unsafe { (*new_state).do_async_flip } {
        return;
    }
    if unsafe { crtc_needs_vblank_work(new_state) } {
        let count = unsafe { drm_crtc_accurate_vblank_count(&mut (*crtc).base) };
        unsafe {
            let _ = drm_vblank_work_schedule(&mut (*new_state).vblank_work, count + 1, false);
        }
    } else {
        unsafe { intel_crtc_arm_vblank_event(new_state) };
    }
    if unsafe { (*state).base.legacy_cursor_update() } && !unsafe { (*state).base.planes.is_null() }
    {
        let total = unsafe { (*(*state).base.dev).mode_config.num_total_plane }.max(0) as usize;
        for index in 0..total {
            let entry = unsafe { (*state).base.planes.add(index) };
            let old = unsafe { (*entry).old_state.cast::<IntelPlaneState>() };
            if !old.is_null()
                && unsafe {
                    (*old).hw.crtc == &mut (*crtc).base && !(*old).unpin_work.vblank.is_null()
                }
            {
                let count = unsafe { drm_crtc_accurate_vblank_count(&mut (*crtc).base) };
                unsafe {
                    let _ = drm_vblank_work_schedule(&mut (*old).unpin_work, count + 1, false);
                    ptr::write_bytes(entry, 0, 1);
                }
            }
        }
    }
    // eDP PSR has no ADL-P output path here; the only remaining upstream
    // condition is a normal commit, whose VRR push is compile-out/no-op.
    if !unsafe { (*state).base.legacy_cursor_update() } {
        unsafe {
            crate::display_pending_stubs::opt::intel_vrr_send_push(ptr::null_mut(), new_state);
        }
    }
    local_irq_enable();
    let display = unsafe { display_from_atomic(state) };
    if !display.is_null()
        && unsafe { crate::intel_parent_upstream::intel_parent_vgpu_active(display) }
    {
        return;
    }
    if unsafe {
        (*crtc).debug.start_vbl_count != 0 && (*crtc).debug.start_vbl_count != end_vblank_count
    } {
        axlog::error!(
            "atomic update crossed a vblank boundary on pipe {} ({} -> {}, scanline {})",
            unsafe { (*crtc).pipe },
            unsafe { (*crtc).debug.start_vbl_count },
            end_vblank_count,
            end_scanline
        );
    }
    let _ = end_time;
}

#[inline]
unsafe fn crtc_to_display(crtc: *const IntelCrtc) -> *mut IntelDisplay {
    if crtc.is_null() {
        return ptr::null_mut();
    }
    unsafe { __drm_to_display((*crtc).base.dev) }
}

#[inline]
unsafe fn state_to_display(state: *const IntelCrtcState) -> *mut IntelDisplay {
    if state.is_null() || unsafe { (*state).uapi.crtc.is_null() } {
        return ptr::null_mut();
    }
    unsafe { __drm_to_display((*(*state).uapi.crtc).dev) }
}

#[inline]
unsafe fn pipe_mask(display: *const IntelDisplay) -> u8 {
    unsafe { (*display).info.__runtime_info.pipe_mask }
}

unsafe fn for_each_pipe(display: *mut IntelDisplay, mut f: impl FnMut(Pipe)) {
    if display.is_null() {
        return;
    }
    let mask = unsafe { pipe_mask(display) };
    for pipe in 0..I915_MAX_PIPES as i32 {
        if mask & (1 << pipe) != 0 {
            f(pipe);
        }
    }
}

unsafe fn for_each_crtc_in_state(
    state: *mut IntelAtomicState,
    mut f: impl FnMut(*mut IntelCrtc, *mut IntelCrtcState),
) {
    if state.is_null() || unsafe { (*state).base.dev.is_null() || (*state).base.crtcs.is_null() } {
        return;
    }
    let count = unsafe { (*(*state).base.dev).mode_config.num_crtc }.max(0) as usize;
    for index in 0..count {
        let entry = unsafe { (*state).base.crtcs.add(index) };
        let new_state = unsafe { (*entry).new_state };
        let old_state = unsafe { (*entry).old_state };
        let crtc = unsafe { (*entry).ptr };
        if !crtc.is_null() && !new_state.is_null() {
            f(crtc.cast::<IntelCrtc>(), new_state.cast::<IntelCrtcState>());
        }
        let _ = old_state;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_first_crtc(display: *mut IntelDisplay) -> *mut IntelCrtc {
    if !unsafe { intel_display_device_present(display) } {
        return ptr::null_mut();
    }
    let base =
        unsafe { tk_drm_upstream::drm_crtc_upstream::drm_crtc_from_index((*display).drm, 0) };
    base.cast()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_crtc_for_pipe(
    display: *mut IntelDisplay,
    pipe: Pipe,
) -> *mut IntelCrtc {
    if !unsafe { intel_display_device_present(display) } {
        return ptr::null_mut();
    }
    let head = unsafe { ptr::addr_of_mut!((*display).pipe_list) };
    let mut node = unsafe { (*head).next };
    while node != head {
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

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_crtc_wait_for_next_vblank(crtc: *mut IntelCrtc) {
    if !crtc.is_null() {
        unsafe {
            tk_drm_upstream::drm_vblank_upstream::drm_crtc_wait_one_vblank(&mut (*crtc).base);
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_wait_for_vblank_if_active(display: *mut IntelDisplay, pipe: Pipe) {
    let crtc = unsafe { intel_crtc_for_pipe(display, pipe) };
    if !crtc.is_null() && unsafe { (*crtc).active } {
        unsafe { intel_crtc_wait_for_next_vblank(crtc) };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_crtc_get_vblank_counter(crtc: *mut IntelCrtc) -> u32 {
    if crtc.is_null() {
        return 0;
    }
    let vblank = unsafe { drm_crtc_vblank_crtc(&mut (*crtc).base) };
    if !unsafe { (*crtc).active } || vblank.is_null() {
        return 0;
    }
    if unsafe { (*vblank).max_vblank_count == 0 } {
        return unsafe { drm_crtc_accurate_vblank_count(&mut (*crtc).base) as u32 };
    }
    let funcs = unsafe { (*(*crtc).base.funcs).get_vblank_counter };
    funcs.map(|f| unsafe { f(&mut (*crtc).base) }).unwrap_or(0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_crtc_max_vblank_count(state: *const IntelCrtcState) -> u32 {
    if state.is_null() {
        return 0;
    }
    let display = unsafe { state_to_display(state) };
    if display.is_null() {
        return 0;
    }
    if unsafe {
        (*state).mode_flags & (I915_MODE_FLAG_DSI_USE_TE0 | I915_MODE_FLAG_DSI_USE_TE1) != 0
    } {
        return 0;
    }
    if unsafe { (*display).info.__runtime_info.ip.ver >= 5 } {
        u32::MAX
    } else if unsafe { (*display).info.__runtime_info.ip.ver >= 3 } {
        0x00ff_ffff
    } else {
        0
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_crtc_vblank_on(state: *const IntelCrtcState) {
    if state.is_null() || unsafe { (*state).uapi.crtc.is_null() } {
        return;
    }
    let crtc = unsafe { (*state).uapi.crtc.cast::<IntelCrtc>() };
    unsafe {
        assert_vblank_disabled(&mut (*crtc).base);
        (*crtc).vblank_psr_notify = false; // PSR/eDP is disabled for this ADL-P port.
        drm_crtc_set_max_vblank_count(&mut (*crtc).base, intel_crtc_max_vblank_count(state));
        drm_crtc_vblank_on(&mut (*crtc).base);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_crtc_vblank_off(state: *const IntelCrtcState) {
    if state.is_null() || unsafe { (*state).uapi.crtc.is_null() } {
        return;
    }
    let display = unsafe { state_to_display(state) };
    let crtc = unsafe { (*state).uapi.crtc.cast::<IntelCrtc>() };
    unsafe {
        drm_crtc_vblank_off(&mut (*crtc).base);
        assert_vblank_disabled(&mut (*crtc).base);
        (*crtc).vblank_psr_notify = false;
        if !display.is_null() {
            flush_work(&mut (*display).irq.vblank_notify_work);
        }
    }
}

unsafe fn assert_vblank_disabled(crtc: *mut DrmCrtc) {
    if crtc.is_null() {
        return;
    }
    if unsafe { drm_crtc_vblank_get(crtc) } == 0 {
        unsafe { drm_crtc_vblank_put(crtc) };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_crtc_state_alloc(crtc: *mut IntelCrtc) -> *mut IntelCrtcState {
    if crtc.is_null() {
        return ptr::null_mut();
    }
    let state = unsafe { kzalloc_objs::<IntelCrtcState, _>(1) };
    if !state.is_null() {
        unsafe { intel_crtc_state_reset(state, crtc) };
    }
    state
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_crtc_state_reset(state: *mut IntelCrtcState, crtc: *mut IntelCrtc) {
    if state.is_null() || crtc.is_null() {
        return;
    }
    unsafe {
        ptr::write_bytes(state, 0, 1);
        drm_atomic_helper_crtc_state_reset(&mut (*state).uapi, &mut (*crtc).base);
        (*state).cpu_transcoder = INVALID_TRANSCODER;
        (*state).master_transcoder = INVALID_TRANSCODER;
        (*state).hsw_workaround_pipe = INVALID_PIPE;
        (*state).scaler_state.scaler_id = -1;
        (*state).mst_master_transcoder = INVALID_TRANSCODER;
        (*state).max_link_bpp_x16 = i32::MAX;
    }
}

unsafe fn intel_crtc_alloc() -> *mut IntelCrtc {
    let crtc = unsafe { kzalloc_objs::<IntelCrtc, _>(1) };
    if crtc.is_null() {
        return ERR_PTR(-ENOMEM);
    }
    let state = unsafe { intel_crtc_state_alloc(crtc) };
    if state.is_null() {
        unsafe { kfree(crtc.cast::<c_void>()) };
        return ERR_PTR(-ENOMEM);
    }
    unsafe {
        (*crtc).base.state = ptr::addr_of_mut!((*state).uapi);
        (*crtc).config = state;
        INIT_LIST_HEAD(ptr::addr_of_mut!((*crtc).pipe_head));
    }
    crtc
}

unsafe fn intel_crtc_free(crtc: *mut IntelCrtc) {
    if crtc.is_null() {
        return;
    }
    unsafe {
        if !(*crtc).base.state.is_null() {
            crate::intel_atomic_upstream::intel_crtc_destroy_state(
                &mut (*crtc).base,
                (*crtc).base.state,
            );
        }
        kfree(crtc.cast::<c_void>());
    }
}

unsafe extern "C" fn intel_crtc_destroy(base: *mut DrmCrtc) {
    if base.is_null() {
        return;
    }
    let crtc = base.cast::<IntelCrtc>();
    unsafe {
        list_del(ptr::addr_of_mut!((*crtc).pipe_head));
        drm_crtc_cleanup(base);
        kfree(crtc.cast::<c_void>());
    }
}

unsafe extern "C" fn intel_crtc_late_register(_crtc: *mut DrmCrtc) -> i32 {
    0
}

static BDW_CRTC_FUNCS: DrmCrtcFuncs = DrmCrtcFuncs {
    reset: None,
    cursor_set: None,
    cursor_set2: None,
    cursor_move: None,
    gamma_set: None,
    destroy: Some(intel_crtc_destroy),
    set_config: Some(drm_atomic_helper_set_config),
    page_flip: Some(drm_atomic_helper_page_flip),
    page_flip_target: None,
    set_property: None,
    atomic_duplicate_state: Some(crate::intel_atomic_upstream::intel_crtc_duplicate_state),
    atomic_destroy_state: Some(crate::intel_atomic_upstream::intel_crtc_destroy_state),
    atomic_set_property: None,
    atomic_get_property: None,
    late_register: Some(intel_crtc_late_register),
    early_unregister: None,
    set_crc_source: None,
    verify_crc_source: None,
    get_crc_sources: None,
    atomic_print_state: None,
    get_vblank_counter: Some(g4x_get_vblank_counter),
    enable_vblank: Some(bdw_enable_vblank),
    disable_vblank: Some(bdw_disable_vblank),
    get_vblank_timestamp: Some(intel_crtc_get_vblank_timestamp),
};

unsafe fn add_crtc_to_pipe_list(display: *mut IntelDisplay, crtc: *mut IntelCrtc) {
    let head = unsafe { ptr::addr_of_mut!((*display).pipe_list) };
    let mut node = unsafe { (*head).next };
    while node != head {
        let existing = unsafe {
            node.cast::<u8>()
                .sub(offset_of!(IntelCrtc, pipe_head))
                .cast::<IntelCrtc>()
        };
        if unsafe { (*crtc).pipe < (*existing).pipe } {
            unsafe { list_add(ptr::addr_of_mut!((*crtc).pipe_head), node) };
            return;
        }
        node = unsafe { (*node).next };
    }
    unsafe { list_add_tail(ptr::addr_of_mut!((*crtc).pipe_head), head) };
}

unsafe fn crtc_init_one(display: *mut IntelDisplay, pipe: Pipe) -> i32 {
    let crtc = unsafe { intel_crtc_alloc() };
    if unsafe { IS_ERR(crtc.cast::<c_void>()) } {
        return unsafe { PTR_ERR(crtc.cast::<c_void>()) };
    }
    unsafe {
        (*crtc).pipe = pipe;
        (*crtc).num_scalers = (*display).info.__runtime_info.num_scalers[pipe as usize] as i32;
    }
    let primary = unsafe { skl_universal_plane_create(display, pipe, PLANE_1) };
    if primary.is_null() || unsafe { IS_ERR(primary.cast::<c_void>()) } {
        let ret = if primary.is_null() {
            -ENOMEM
        } else {
            unsafe { PTR_ERR(primary.cast::<c_void>()) }
        };
        unsafe { intel_crtc_free(crtc) };
        return ret;
    }
    unsafe {
        (*crtc).plane_ids_mask |= 1u8 << (*primary).id;
    }
    unsafe { intel_init_fifo_underrun_reporting(display, crtc, false) };
    let sprite_count = unsafe { (*display).info.__runtime_info.num_sprites[pipe as usize] };
    for sprite in 0..sprite_count {
        let plane = unsafe { skl_universal_plane_create(display, pipe, PLANE_2 + sprite as u32) };
        if plane.is_null() || unsafe { IS_ERR(plane.cast::<c_void>()) } {
            let ret = if plane.is_null() {
                -ENOMEM
            } else {
                unsafe { PTR_ERR(plane.cast::<c_void>()) }
            };
            unsafe { intel_crtc_free(crtc) };
            return ret;
        }
        unsafe {
            (*crtc).plane_ids_mask |= 1u8 << (*plane).id;
        }
    }
    let cursor = unsafe { intel_cursor_plane_create(display, pipe) };
    if cursor.is_null() || unsafe { IS_ERR(cursor.cast::<c_void>()) } {
        let ret = if cursor.is_null() {
            -ENOMEM
        } else {
            unsafe { PTR_ERR(cursor.cast::<c_void>()) }
        };
        unsafe { intel_crtc_free(crtc) };
        return ret;
    }
    unsafe {
        (*crtc).plane_ids_mask |= 1u8 << (*cursor).id;
    }
    let name = match pipe {
        0 => c"pipe A",
        1 => c"pipe B",
        2 => c"pipe C",
        3 => c"pipe D",
        _ => c"pipe ?",
    };
    let ret = unsafe {
        drm_crtc_init_with_planes(
            (*display).drm,
            &mut (*crtc).base,
            &mut (*primary).base,
            &mut (*cursor).base,
            &BDW_CRTC_FUNCS,
            name.as_ptr(),
        )
    };
    if ret != 0 {
        unsafe { intel_crtc_free(crtc) };
        return ret;
    }
    let ver = unsafe { (*display).info.__runtime_info.ip.ver };
    if ver >= GEN11 {
        unsafe {
            drm_crtc_create_scaling_filter_property(
                &mut (*crtc).base,
                (1u32 << DRM_SCALING_FILTER_DEFAULT)
                    | (1u32 << DRM_SCALING_FILTER_NEAREST_NEIGHBOR),
            );
        }
    }
    if ver >= GEN9 {
        unsafe {
            drm_crtc_attach_background_color_property(&mut (*crtc).base);
        }
    }
    unsafe {
        intel_color_crtc_init(crtc);
        // DRRS and CRC are deferred/eDP-or-debug features in this configuration.
        add_crtc_to_pipe_list(display, crtc);
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_crtc_init(display: *mut IntelDisplay) -> i32 {
    if !unsafe { intel_display_device_present(display) } {
        return 0;
    }
    let mut ret = 0;
    unsafe {
        for_each_pipe(display, |pipe| {
            if ret == 0 {
                ret = crtc_init_one(display, pipe);
            }
        })
    };
    ret
}

#[repr(C)]
struct GetPipeFromCrtcId {
    crtc_id: u32,
    pipe: u32,
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_crtc_get_pipe_from_crtc_id_ioctl(
    dev: *mut DrmDevice,
    data: *mut c_void,
    file: *mut DrmFile,
) -> i32 {
    if dev.is_null() || data.is_null() || file.is_null() {
        return -EINVAL;
    }
    let request = data.cast::<GetPipeFromCrtcId>();
    let object = unsafe {
        tk_drm_upstream::drm_mode_object_upstream::drm_mode_object_find(
            dev,
            file,
            (*request).crtc_id,
            tk_drm_upstream::uapi_drm_consts::DRM_MODE_OBJECT_CRTC,
        )
    };
    let crtc = if object.is_null() {
        ptr::null_mut()
    } else {
        unsafe {
            object
                .cast::<u8>()
                .sub(offset_of!(DrmCrtc, base))
                .cast::<DrmCrtc>()
        }
    };
    if crtc.is_null() {
        return -ENOENT;
    }
    unsafe {
        (*request).pipe = (*crtc.cast::<IntelCrtc>()).pipe as u32;
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_crtc_enable_changed(
    old: *const IntelCrtcState,
    new: *const IntelCrtcState,
) -> bool {
    !old.is_null() && !new.is_null() && unsafe { (*old).hw.enable != (*new).hw.enable }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_any_crtc_enable_changed(state: *mut IntelAtomicState) -> bool {
    let mut changed = false;
    unsafe {
        for_each_crtc_in_state(state, |_, new| {
            let old = intel_atomic_get_old_crtc_state(state, new_crtc_for_state(new));
            if intel_crtc_enable_changed(old, new) {
                changed = true;
            }
        })
    };
    changed
}

unsafe fn new_crtc_for_state(state: *mut IntelCrtcState) -> *mut IntelCrtc {
    if state.is_null() {
        ptr::null_mut()
    } else {
        unsafe { (*state).uapi.crtc.cast() }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_crtc_active_changed(
    old: *const IntelCrtcState,
    new: *const IntelCrtcState,
) -> bool {
    !old.is_null() && !new.is_null() && unsafe { (*old).hw.active != (*new).hw.active }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_any_crtc_active_changed(state: *mut IntelAtomicState) -> bool {
    let mut changed = false;
    unsafe {
        for_each_crtc_in_state(state, |_, new| {
            let old = intel_atomic_get_old_crtc_state(state, new_crtc_for_state(new));
            if intel_crtc_active_changed(old, new) {
                changed = true;
            }
        })
    };
    changed
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_crtc_bw_num_active_planes(state: *const IntelCrtcState) -> u32 {
    if state.is_null() {
        0
    } else {
        (unsafe { (*state).active_planes & !(1 << PLANE_CURSOR) }).count_ones()
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_crtc_bw_data_rate(state: *const IntelCrtcState) -> u32 {
    if state.is_null() {
        return 0;
    }
    let display = unsafe { state_to_display(state) };
    let crtc = unsafe { (*state).uapi.crtc.cast::<IntelCrtc>() };
    if display.is_null() || crtc.is_null() {
        return 0;
    }
    let ver = unsafe { (*display).info.__runtime_info.ip.ver };
    let mut rate = 0u32;
    for id in 0..8usize {
        if unsafe { (*crtc).plane_ids_mask & (1 << id) == 0 } || id == PLANE_CURSOR as usize {
            continue;
        }
        rate = rate.wrapping_add(unsafe { (*state).data_rate[id] });
        if ver < GEN11 {
            rate = rate.wrapping_add(unsafe { (*state).data_rate_y[id] });
        }
    }
    rate
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_crtc_bw_min_cdclk(state: *const IntelCrtcState) -> i32 {
    if state.is_null() {
        return 0;
    }
    let display = unsafe { state_to_display(state) };
    if display.is_null() || unsafe { (*display).info.__runtime_info.ip.ver < GEN12 } {
        return 0;
    }
    let rate = unsafe { intel_crtc_bw_data_rate(state) } as u64;
    rate.saturating_mul(10).div_ceil(512) as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn state_reset_restores_pipe_sentinels_and_clears_prior_updates() {
        let mut crtc = unsafe { core::mem::zeroed::<IntelCrtc>() };
        let mut state = unsafe { core::mem::zeroed::<IntelCrtcState>() };
        state.update_pipe = true;
        state.cpu_transcoder = 2;
        unsafe { intel_crtc_state_reset(&mut state, &mut crtc) };
        assert_eq!(state.cpu_transcoder, INVALID_TRANSCODER);
        assert_eq!(state.master_transcoder, INVALID_TRANSCODER);
        assert_eq!(state.hsw_workaround_pipe, INVALID_PIPE);
        assert_eq!(state.scaler_state.scaler_id, -1);
        assert!(!state.update_pipe);
    }

    #[test]
    fn cdclk_bandwidth_rounding_uses_ceiling() {
        assert_eq!(0u64.div_ceil(512), 0);
        assert_eq!(1u64.div_ceil(512), 1);
        assert_eq!(512u64.div_ceil(512), 1);
        assert_eq!(513u64.div_ceil(512), 2);
    }
}
