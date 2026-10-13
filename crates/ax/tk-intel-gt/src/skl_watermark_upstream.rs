// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// Source: drivers/gpu/drm/i915/display/skl_watermark.c (Linux v7.2.3).
#![allow(
    non_snake_case,
    dead_code,
    unsafe_code,
    unsafe_op_in_unsafe_fn,
    clippy::missing_safety_doc
)]

use core::{mem, ptr};

use tk_drm_upstream::{drm_atomic_types::DrmAtomicCommit, drm_fourcc_types::DrmFormatInfo};

use crate::{
    intel_cdclk_upstream::intel_cdclk_state_set_joined_mbus,
    intel_cursor_regs_upstream::*,
    intel_de_upstream::{intel_de_read, intel_de_rmw, intel_de_write},
    intel_display_core_types_upstream::{IntelDisplay, IntelWmFuncs},
    intel_display_limits_types_upstream::{
        I915_MAX_PIPES, I915_MAX_PLANES, PLANE_CURSOR, Pipe, PlaneId,
    },
    intel_display_power_types_upstream::POWER_DOMAIN_PIPE_A,
    intel_display_regs_upstream::*,
    intel_display_types_upstream::{
        IntelAtomicState, IntelCrtc, IntelCrtcState, IntelCrtcWmStateAnon0x0Skl, IntelPlane,
        IntelPlaneState, SklPipeWm, SklWmLevel,
    },
    intel_display_wa_upstream::{
        DISPLAY_VER, DISPLAY_VERx100, platform_alderlake_p, platform_rocketlake,
    },
    intel_global_state_types_upstream::{IntelGlobalObj, IntelGlobalState, IntelGlobalStateFuncs},
    intel_parent_upstream::{
        intel_parent_pcode_read, intel_parent_pcode_request, intel_parent_pcode_write,
    },
    intel_wm_types_upstream::SklDdbEntry,
    linux_i915_private::DrmI915Private,
    skl_universal_plane_regs_upstream::*,
    skl_watermark_regs_upstream::*,
};

const DBUF_S1: u8 = 0;
const DBUF_S2: u8 = 1;
const DBUF_S3: u8 = 2;
const DBUF_S4: u8 = 3;
const DBUF_S1_S2: u8 = (1 << DBUF_S1) | (1 << DBUF_S2);
const DBUF_S3_S4: u8 = (1 << DBUF_S3) | (1 << DBUF_S4);
const DBUF_ALL: u8 = DBUF_S1_S2 | DBUF_S3_S4;
const SAGV_DISABLED: u32 = 1;
const SAGV_ENABLED: u32 = 2;
const SAGV_NOT_CONTROLLED: u32 = 3;
const GEN9_PCODE_SAGV_CONTROL: u32 = 0x21;
const GEN9_SAGV_ENABLE: u32 = 0x3;
const GEN9_SAGV_DISABLE: u32 = 0x0;
const GEN9_SAGV_IS_DISABLED: u32 = 0x1;
const GEN12_PCODE_READ_SAGV_BLOCK_TIME_US: u32 = 0x23;
const GEN9_PCODE_READ_MEM_LATENCY: u32 = 0x6;
const GEN9_MEM_LATENCY_LEVEL_0_4_MASK: u32 = 0x000000ff;
const GEN9_MEM_LATENCY_LEVEL_1_5_MASK: u32 = 0x0000ff00;
const GEN9_MEM_LATENCY_LEVEL_2_6_MASK: u32 = 0x00ff0000;
const GEN9_MEM_LATENCY_LEVEL_3_7_MASK: u32 = 0xff000000;
const DISP_IPC_ENABLE: u32 = 1 << 3;
const FP_ONE: u32 = 1 << 16;
const FP_MAX: u32 = u32::MAX;
const DRM_FORMAT_MOD_INTEL_X_TILED: u64 = (1u64 << 56) | 1;
const DRM_FORMAT_MOD_INTEL_Y_TILED: u64 = (1u64 << 56) | 2;
const DRM_FORMAT_MOD_INTEL_YF_TILED: u64 = (1u64 << 56) | 3;

#[repr(C)]
pub struct IntelDbufState {
    base: IntelGlobalState,
    ddb: [SklDdbEntry; I915_MAX_PIPES as usize],
    weight: [u32; I915_MAX_PIPES as usize],
    slices: [u8; I915_MAX_PIPES as usize],
    enabled_slices: u8,
    active_pipes: u8,
    mdclk_cdclk_ratio: u8,
    joined_mbus: bool,
}
const _: () = assert!(mem::size_of::<IntelDbufState>() == 72);

#[repr(C)]
struct DbufSliceConfEntry {
    active_pipes: u8,
    dbuf_mask: [u8; I915_MAX_PIPES as usize],
    join_mbus: bool,
}

#[derive(Clone, Copy, Default)]
struct SklWmParams {
    x_tiled: bool,
    y_tiled: bool,
    rc_surface: bool,
    width: u32,
    cpp: u8,
    plane_pixel_rate: u32,
    y_min_scanlines: u32,
    plane_bytes_per_line: u32,
    plane_blocks_per_line: u32,
    y_tile_minimum: u32,
    linetime_us: u32,
    dbuf_block_size: u32,
}

const fn slice_conf(active: u8, masks: [u8; 4], joined: bool) -> DbufSliceConfEntry {
    DbufSliceConfEntry {
        active_pipes: active,
        dbuf_mask: masks,
        join_mbus: joined,
    }
}

const PIPE_A_BIT: u8 = 1 << 0;
const PIPE_B_BIT: u8 = 1 << 1;
const PIPE_C_BIT: u8 = 1 << 2;
const PIPE_D_BIT: u8 = 1 << 3;
static ADLP_ALLOWED_DBUFS: [DbufSliceConfEntry; 18] = [
    slice_conf(PIPE_A_BIT, [DBUF_ALL, 0, 0, 0], true),
    slice_conf(PIPE_B_BIT, [0, DBUF_ALL, 0, 0], true),
    slice_conf(PIPE_A_BIT, [DBUF_S1_S2, 0, 0, 0], false),
    slice_conf(PIPE_B_BIT, [0, DBUF_S3_S4, 0, 0], false),
    slice_conf(
        PIPE_A_BIT | PIPE_B_BIT,
        [DBUF_S1_S2, DBUF_S3_S4, 0, 0],
        false,
    ),
    slice_conf(PIPE_C_BIT, [0, 0, DBUF_S3_S4, 0], false),
    slice_conf(
        PIPE_A_BIT | PIPE_C_BIT,
        [DBUF_S1_S2, 0, DBUF_S3_S4, 0],
        false,
    ),
    slice_conf(
        PIPE_B_BIT | PIPE_C_BIT,
        [0, DBUF_S3_S4, DBUF_S3_S4, 0],
        false,
    ),
    slice_conf(
        PIPE_A_BIT | PIPE_B_BIT | PIPE_C_BIT,
        [DBUF_S1_S2, DBUF_S3_S4, DBUF_S3_S4, 0],
        false,
    ),
    slice_conf(PIPE_D_BIT, [0, 0, 0, DBUF_S1_S2], false),
    slice_conf(
        PIPE_A_BIT | PIPE_D_BIT,
        [DBUF_S1_S2, 0, 0, DBUF_S1_S2],
        false,
    ),
    slice_conf(
        PIPE_B_BIT | PIPE_D_BIT,
        [0, DBUF_S3_S4, 0, DBUF_S1_S2],
        false,
    ),
    slice_conf(
        PIPE_A_BIT | PIPE_B_BIT | PIPE_D_BIT,
        [DBUF_S1_S2, DBUF_S3_S4, 0, DBUF_S1_S2],
        false,
    ),
    slice_conf(
        PIPE_C_BIT | PIPE_D_BIT,
        [0, 0, DBUF_S3_S4, DBUF_S1_S2],
        false,
    ),
    slice_conf(
        PIPE_A_BIT | PIPE_C_BIT | PIPE_D_BIT,
        [DBUF_S1_S2, 0, DBUF_S3_S4, DBUF_S1_S2],
        false,
    ),
    slice_conf(
        PIPE_B_BIT | PIPE_C_BIT | PIPE_D_BIT,
        [0, DBUF_S3_S4, DBUF_S3_S4, DBUF_S1_S2],
        false,
    ),
    slice_conf(
        PIPE_A_BIT | PIPE_B_BIT | PIPE_C_BIT | PIPE_D_BIT,
        [DBUF_S1_S2, DBUF_S3_S4, DBUF_S3_S4, DBUF_S1_S2],
        false,
    ),
    slice_conf(0, [0; 4], false),
];

static DBUF_GLOBAL_FUNCS: IntelGlobalStateFuncs = IntelGlobalStateFuncs {
    atomic_duplicate_state: Some(dbuf_duplicate_state),
    atomic_destroy_state: Some(dbuf_destroy_state),
};

unsafe fn display_from_atomic(state: *mut IntelAtomicState) -> *mut IntelDisplay {
    if state.is_null() || (*state).base.dev.is_null() || (*(*state).base.dev).dev_private.is_null()
    {
        return ptr::null_mut();
    }
    (*(*(*state).base.dev).dev_private.cast::<DrmI915Private>())
        .display
        .cast::<IntelDisplay>()
}

unsafe fn display_from_crtc_state(state: *const IntelCrtcState) -> *mut IntelDisplay {
    if state.is_null() || (*state).uapi.crtc.is_null() {
        return ptr::null_mut();
    }
    let crtc = (*state).uapi.crtc.cast::<IntelCrtc>();
    if (*crtc).base.dev.is_null() || (*(*crtc).base.dev).dev_private.is_null() {
        return ptr::null_mut();
    }
    (*(*(*crtc).base.dev).dev_private.cast::<DrmI915Private>())
        .display
        .cast::<IntelDisplay>()
}

unsafe fn display_from_plane(plane: *mut IntelPlane) -> *mut IntelDisplay {
    if plane.is_null() || (*plane).base.dev.is_null() || (*(*plane).base.dev).dev_private.is_null()
    {
        return ptr::null_mut();
    }
    (*(*(*plane).base.dev).dev_private.cast::<DrmI915Private>())
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

unsafe fn skl_wm_state_ptr(state: *mut IntelCrtcState) -> *mut IntelCrtcWmStateAnon0x0Skl {
    ptr::addr_of_mut!((*state).wm._anon_0_0.skl).cast()
}

unsafe fn skl_wm_state_const_ptr(
    state: *const IntelCrtcState,
) -> *const IntelCrtcWmStateAnon0x0Skl {
    ptr::addr_of!((*state).wm._anon_0_0.skl).cast()
}

unsafe extern "C" {
    fn intel_atomic_get_global_obj_state(
        state: *mut IntelAtomicState,
        obj: *mut IntelGlobalObj,
    ) -> *mut IntelGlobalState;
    fn intel_atomic_get_old_global_obj_state(
        state: *mut IntelAtomicState,
        obj: *mut IntelGlobalObj,
    ) -> *mut IntelGlobalState;
    fn intel_atomic_get_new_global_obj_state(
        state: *mut IntelAtomicState,
        obj: *mut IntelGlobalObj,
    ) -> *mut IntelGlobalState;
    fn intel_atomic_lock_global_state(state: *mut IntelGlobalState) -> i32;
    fn intel_atomic_serialize_global_state(state: *mut IntelGlobalState) -> i32;
    fn intel_atomic_get_crtc_state(
        state: *mut DrmAtomicCommit,
        crtc: *mut IntelCrtc,
    ) -> *mut IntelCrtcState;
    fn intel_crtc_get_plane(crtc: *mut IntelCrtc, plane_id: PlaneId) -> *mut IntelPlane;
    fn intel_crtc_for_pipe(display: *mut IntelDisplay, pipe: Pipe) -> *mut IntelCrtc;
    fn intel_crtc_wait_for_next_vblank(crtc: *mut IntelCrtc);
    fn intel_plane_disable_noatomic(crtc: *mut IntelCrtc, plane: *mut IntelPlane);
    fn intel_calc_active_pipes(state: *mut IntelAtomicState, old: u8) -> u8;
    fn intel_plane_pixel_rate(
        state: *const IntelCrtcState,
        plane_state: *const IntelPlaneState,
    ) -> u32;
    fn intel_plane_data_rate(
        state: *const IntelCrtcState,
        plane_state: *const IntelPlaneState,
        color_plane: i32,
    ) -> u32;
    fn intel_fb_is_tiled_modifier(modifier: u64) -> bool;
    fn intel_fb_is_ccs_modifier(modifier: u64) -> bool;
    fn intel_format_info_is_yuv_semiplanar(format: *const DrmFormatInfo, modifier: u64) -> bool;
    fn drm_get_format_info(
        drm: *mut tk_drm_upstream::drm_device_types::DrmDevice,
        format: u32,
        modifier: u64,
    ) -> *const DrmFormatInfo;
    fn skl_prefill_init(
        ctx: *mut crate::skl_prefill_types_upstream::SklPrefillCtx,
        state: *const IntelCrtcState,
    );
    fn skl_prefill_vblank_too_short(
        ctx: *const crate::skl_prefill_types_upstream::SklPrefillCtx,
        state: *const IntelCrtcState,
        latency_us: u32,
    ) -> bool;
    fn skl_scaler_max_total_scale(state: *const IntelCrtcState) -> u32;
    fn skl_scaler_max_hscale(state: *const IntelCrtcState) -> u32;
}

unsafe extern "C" fn dbuf_duplicate_state(obj: *mut IntelGlobalObj) -> *mut IntelGlobalState {
    if obj.is_null() || (*obj).state.is_null() {
        return ptr::null_mut();
    }
    let source = (*obj).state.cast::<IntelDbufState>();
    let state =
        tk_linux_kapi::memory::kmalloc_obj::<IntelDbufState>(tk_linux_kapi::config::GFP_KERNEL);
    if state.is_null() {
        return ptr::null_mut();
    }
    ptr::copy_nonoverlapping(source, state, 1);
    (*state).base.state = ptr::null_mut();
    (*state).base.commit = ptr::null_mut();
    (*state).base.changed = false;
    (*state).base.serialized = false;
    ptr::addr_of_mut!((*state).base)
}

unsafe extern "C" fn dbuf_destroy_state(_obj: *mut IntelGlobalObj, state: *mut IntelGlobalState) {
    if !state.is_null() {
        tk_linux_kapi::memory::kfree(state.cast::<IntelDbufState>());
    }
}

unsafe fn old_dbuf_state(state: *mut IntelAtomicState) -> *const IntelDbufState {
    let display = display_from_atomic(state);
    if display.is_null() {
        ptr::null()
    } else {
        intel_atomic_get_old_global_obj_state(state, ptr::addr_of_mut!((*display).dbuf.obj)).cast()
    }
}

unsafe fn new_dbuf_state(state: *mut IntelAtomicState) -> *mut IntelDbufState {
    let display = display_from_atomic(state);
    if display.is_null() {
        ptr::null_mut()
    } else {
        intel_atomic_get_new_global_obj_state(state, ptr::addr_of_mut!((*display).dbuf.obj)).cast()
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_get_dbuf_state(
    state: *mut IntelAtomicState,
) -> *mut IntelDbufState {
    let display = display_from_atomic(state);
    if display.is_null() {
        return tk_linux_kapi::config::ERR_PTR(-tk_linux_kapi::config::EINVAL);
    }
    let global = intel_atomic_get_global_obj_state(state, ptr::addr_of_mut!((*display).dbuf.obj));
    if tk_linux_kapi::config::IS_ERR(global) {
        global.cast()
    } else {
        global.cast()
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dbuf_init(display: *mut IntelDisplay) -> i32 {
    if display.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let state = tk_linux_kapi::memory::kzalloc_obj::<IntelDbufState>();
    if state.is_null() {
        return -tk_linux_kapi::config::ENOMEM;
    }
    crate::intel_global_state_upstream::intel_atomic_global_obj_init(
        display,
        ptr::addr_of_mut!((*display).dbuf.obj),
        ptr::addr_of_mut!((*state).base),
        ptr::addr_of!(DBUF_GLOBAL_FUNCS),
    );
    0
}

fn ddb_entry_size(entry: &SklDdbEntry) -> u16 {
    entry.end.saturating_sub(entry.start)
}

fn ddb_entry_equal(a: &SklDdbEntry, b: &SklDdbEntry) -> bool {
    a.start == b.start && a.end == b.end
}

fn ddb_entry_for_slices(slice_size: u16, slice_mask: u8) -> Option<SklDdbEntry> {
    if slice_mask == 0 || slice_size == 0 {
        return Some(SklDdbEntry { start: 0, end: 0 });
    }
    let start_slice = slice_mask.trailing_zeros() as u16;
    let end_slice = (8 - slice_mask.leading_zeros()) as u16;
    let start = start_slice.checked_mul(slice_size)?;
    let end = end_slice.checked_mul(slice_size)?;
    (start < end).then_some(SklDdbEntry { start, end })
}

fn adlp_slice_mask_for(pipe: u8, active_pipes: u8, join_mbus: bool) -> u8 {
    for conf in ADLP_ALLOWED_DBUFS.iter() {
        if conf.active_pipes == active_pipes && conf.join_mbus == join_mbus {
            return conf.dbuf_mask.get(pipe as usize).copied().unwrap_or(0);
        }
    }
    0
}

fn adlp_should_join_mbus(active_pipes: u8) -> bool {
    ADLP_ALLOWED_DBUFS
        .iter()
        .find(|conf| conf.active_pipes == active_pipes && conf.active_pipes != 0)
        .map(|conf| conf.join_mbus)
        .unwrap_or(false)
}

unsafe fn display_dbuf_slice_size(display: *mut IntelDisplay) -> Option<u16> {
    if display.is_null() {
        return None;
    }
    let info = (*display).info.__device_info;
    if info.is_null() {
        return None;
    }
    let slice_count = (*info).dbuf.slice_mask.count_ones();
    if slice_count == 0 {
        return None;
    }
    Some((*info).dbuf.size / slice_count as u16)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_ddb_dbuf_slice_mask(
    display: *mut IntelDisplay,
    entry: *const SklDdbEntry,
) -> u32 {
    if display.is_null() || entry.is_null() || ddb_entry_size(&*entry) == 0 {
        return 0;
    }
    let Some(slice_size) = display_dbuf_slice_size(display) else {
        return 0;
    };
    let first = ((*entry).start / slice_size) as u8;
    let last = (((*entry).end - 1) / slice_size) as u8;
    if first >= 8 || last >= 8 {
        return 0;
    }
    let mut mask = 0u8;
    for slice in first..=last {
        mask |= 1 << slice;
    }
    mask as u32
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_enabled_dbuf_slices_mask(display: *mut IntelDisplay) -> u8 {
    let Some(info) = (!display.is_null())
        .then(|| (*display).info.__device_info)
        .filter(|p| !p.is_null())
    else {
        return 0;
    };
    let mut enabled = 0;
    for slice in 0..4u32 {
        if (*info).dbuf.slice_mask & (1 << slice) != 0
            && intel_de_read(display, DBUF_CTL_S(slice)) & DBUF_POWER_STATE != 0
        {
            enabled |= 1 << slice;
        }
    }
    enabled
}

unsafe fn display_has_ipc(display: *mut IntelDisplay) -> bool {
    if display.is_null() || (*display).info.__device_info.is_null() {
        return false;
    }
    ((*(*display).info.__device_info)._bitfields_46[1] & (1 << 2)) != 0
}

unsafe fn has_sagv(display: *mut IntelDisplay) -> bool {
    !display.is_null() && DISPLAY_VER(display) >= 9 && (*display).sagv.status != SAGV_NOT_CONTROLLED
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_has_sagv(display: *mut IntelDisplay) -> bool {
    has_sagv(display)
}

unsafe fn sagv_block_time(display: *mut IntelDisplay) -> u32 {
    if DISPLAY_VER(display) >= 14 {
        let val = intel_de_read(display, MTL_LATENCY_SAGV);
        crate::linux::registers::REG_FIELD_GET(MTL_LATENCY_QCLK_SAGV, val)
    } else if DISPLAY_VER(display) >= 12 {
        let mut val = 0;
        if intel_parent_pcode_read(
            display,
            GEN12_PCODE_READ_SAGV_BLOCK_TIME_US,
            &mut val,
            ptr::null_mut(),
        ) != 0
        {
            0
        } else {
            val
        }
    } else if DISPLAY_VER(display) == 11 {
        10
    } else if has_sagv(display) {
        30
    } else {
        0
    }
}

unsafe fn skl_sagv_disable(display: *mut IntelDisplay) {
    if !has_sagv(display) || (*display).sagv.status == SAGV_DISABLED {
        return;
    }
    let ret = intel_parent_pcode_request(
        display,
        GEN9_PCODE_SAGV_CONTROL,
        GEN9_SAGV_DISABLE,
        GEN9_SAGV_IS_DISABLED,
        GEN9_SAGV_IS_DISABLED,
        1,
    );
    if ret == 0 {
        (*display).sagv.status = SAGV_DISABLED;
    }
}

unsafe fn intel_sagv_init(display: *mut IntelDisplay) {
    if !has_sagv(display) {
        (*display).sagv.status = SAGV_NOT_CONTROLLED;
    }
    if DISPLAY_VER(display) < 11 {
        skl_sagv_disable(display);
    }
    let mut block = sagv_block_time(display);
    if block > u16::MAX as u32 {
        block = 0;
    }
    if !has_sagv(display) {
        block = 0;
    }
    (*display).sagv.block_time_us = block;
}

unsafe fn has_sagv_wm(display: *mut IntelDisplay) -> bool {
    DISPLAY_VER(display) >= 12 && !platform_rocketlake(display)
}

unsafe fn has_hw_sagv_wm(display: *mut IntelDisplay) -> bool {
    DISPLAY_VER(display) >= 13 && !platform_dgfx(display)
}

unsafe fn platform_dgfx(display: *mut IntelDisplay) -> bool {
    !display.is_null() && ((*display).platform._anon_0_0.bitmap[0] & (1u64 << 2)) != 0
}

unsafe fn skl_sagv_pre_plane_update(state: *mut IntelAtomicState) {
    let display = display_from_atomic(state);
    if display.is_null() {
        return;
    }
    let bw = crate::intel_bw_upstream::intel_atomic_get_new_bw_state(state);
    if !bw.is_null() && !crate::intel_bw_upstream::intel_bw_can_enable_sagv(display, bw) {
        skl_sagv_disable(display);
    }
}

unsafe fn skl_sagv_post_plane_update(state: *mut IntelAtomicState) {
    let display = display_from_atomic(state);
    if display.is_null() {
        return;
    }
    let bw = crate::intel_bw_upstream::intel_atomic_get_new_bw_state(state);
    if !bw.is_null() && crate::intel_bw_upstream::intel_bw_can_enable_sagv(display, bw) {
        skl_sagv_enable(display);
    }
}

unsafe fn skl_sagv_enable(display: *mut IntelDisplay) {
    if !has_sagv(display) || (*display).sagv.status == SAGV_ENABLED {
        return;
    }
    if intel_parent_pcode_write(display, GEN9_PCODE_SAGV_CONTROL, GEN9_SAGV_ENABLE) == 0 {
        (*display).sagv.status = SAGV_ENABLED;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_sagv_pre_plane_update(state: *mut IntelAtomicState) {
    let display = display_from_atomic(state);
    if display.is_null() || !has_sagv(display) || DISPLAY_VER(display) >= 14 {
        return;
    }
    if DISPLAY_VER(display) >= 11 {
        crate::intel_bw_upstream::icl_sagv_pre_plane_update(state);
    } else {
        skl_sagv_pre_plane_update(state);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_sagv_post_plane_update(state: *mut IntelAtomicState) {
    let display = display_from_atomic(state);
    if display.is_null() || !has_sagv(display) || DISPLAY_VER(display) >= 14 {
        return;
    }
    if DISPLAY_VER(display) >= 11 {
        crate::intel_bw_upstream::icl_sagv_post_plane_update(state);
    } else {
        skl_sagv_post_plane_update(state);
    }
}

unsafe fn plane_on_crtc(crtc: *mut IntelCrtc, plane_id: PlaneId) -> *mut IntelPlane {
    if crtc.is_null() || plane_id >= I915_MAX_PLANES {
        return ptr::null_mut();
    }
    if (*crtc).plane_ids_mask & (1u8 << plane_id) == 0 {
        return ptr::null_mut();
    }
    intel_crtc_get_plane(crtc, plane_id)
}

unsafe fn tgl_crtc_can_enable_sagv(crtc_state: *const IntelCrtcState) -> bool {
    if crtc_state.is_null() || (*crtc_state).uapi.crtc.is_null() {
        return false;
    }
    let crtc = (*crtc_state).uapi.crtc.cast::<IntelCrtc>();
    for plane_id in 0..I915_MAX_PLANES {
        if let Some(wm) = plane_wm(crtc_state, plane_id).as_ref() {
            if wm.wm[0].enable && !wm.sagv.wm0.enable {
                return false;
            }
        }
    }
    let _ = crtc;
    true
}

unsafe fn skl_crtc_can_enable_sagv(crtc_state: *const IntelCrtcState) -> bool {
    if crtc_state.is_null() || (*crtc_state).uapi.crtc.is_null() {
        return false;
    }
    let display = display_from_crtc_state(crtc_state);
    let crtc = (*crtc_state).uapi.crtc.cast::<IntelCrtc>();
    let mut max_level = i32::MAX;
    for plane_id in 0..I915_MAX_PLANES {
        if !plane_on_crtc(crtc, plane_id).is_null() {
            let wm = plane_wm(crtc_state, plane_id);
            if wm.is_null() || !(*wm).wm[0].enable {
                continue;
            }
            let mut level = (*display).wm.num_levels as i32 - 1;
            while level > 0 && !(*wm).wm[level as usize].enable {
                level -= 1;
            }
            if !(*wm).wm[level as usize].enable {
                return false;
            }
            max_level = max_level.min(level);
        }
    }
    if max_level == i32::MAX {
        return true;
    }
    for plane_id in 0..I915_MAX_PLANES {
        if plane_on_crtc(crtc, plane_id).is_null() {
            continue;
        }
        let wm = plane_wm(crtc_state, plane_id);
        if !wm.is_null() && (*wm).wm[0].enable && !(*wm).wm[max_level as usize].can_sagv {
            return false;
        }
    }
    true
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_crtc_can_enable_sagv(crtc_state: *const IntelCrtcState) -> bool {
    let display = display_from_crtc_state(crtc_state);
    if display.is_null()
        || !has_sagv(display)
        || (*display).sagv.block_time_us == 0
        || !(*display).params.enable_sagv
        || (*crtc_state).inherited
    {
        return false;
    }
    if !(*crtc_state).hw.active {
        return true;
    }
    if (*crtc_state).hw.pipe_mode.flags & tk_drm_upstream::uapi_drm_consts::DRM_MODE_FLAG_INTERLACE
        != 0
    {
        return false;
    }
    if has_sagv_wm(display) {
        tgl_crtc_can_enable_sagv(crtc_state)
    } else {
        skl_crtc_can_enable_sagv(crtc_state)
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_watermark_ipc_enabled(display: *mut IntelDisplay) -> bool {
    !display.is_null() && (*display).wm.ipc_enabled
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_watermark_ipc_update(display: *mut IntelDisplay) {
    if display.is_null() || !display_has_ipc(display) {
        return;
    }
    intel_de_rmw(
        display,
        DISP_ARB_CTL2,
        DISP_IPC_ENABLE,
        if (*display).wm.ipc_enabled {
            DISP_IPC_ENABLE
        } else {
            0
        },
    );
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_watermark_ipc_init(display: *mut IntelDisplay) {
    if display.is_null() || !display_has_ipc(display) {
        return;
    }
    (*display).wm.ipc_enabled = true;
    skl_watermark_ipc_update(display);
}

unsafe fn wm_latency(display: *mut IntelDisplay, level: i32) -> u32 {
    if display.is_null() || level < 0 || level >= (*display).wm.num_levels as i32 {
        return 0;
    }
    (*display).wm.skl_latency[level as usize] as u32
}

unsafe fn sanitize_wm_latency(display: *mut IntelDisplay) {
    let num_levels = (*display).wm.num_levels as usize;
    for level in 1..num_levels {
        if (*display).wm.skl_latency[level] == 0 {
            for tail in level + 1..num_levels {
                (*display).wm.skl_latency[tail] = 0;
            }
            break;
        }
    }
}

unsafe fn adjust_wm_latency(display: *mut IntelDisplay) {
    sanitize_wm_latency(display);
    for level in 1..(*display).wm.num_levels as usize {
        if (*display).wm.skl_latency[level] == 0 {
            break;
        }
        (*display).wm.skl_latency[level] =
            (*display).wm.skl_latency[level].max((*display).wm.skl_latency[level - 1]);
    }
    if (*display).wm.skl_latency[0] == 0 {
        let read_latency = if DISPLAY_VER(display) >= 14 {
            6
        } else if DISPLAY_VER(display) >= 12 {
            3
        } else {
            2
        };
        (*display).wm.skl_latency[0] = read_latency;
        for level in 1..(*display).wm.num_levels as usize {
            if (*display).wm.skl_latency[level] == 0 {
                break;
            }
            (*display).wm.skl_latency[level] =
                (*display).wm.skl_latency[level].saturating_add(read_latency);
        }
    }
}

unsafe fn skl_read_wm_latency(display: *mut IntelDisplay) {
    let mut val = 0;
    if intel_parent_pcode_read(
        display,
        GEN9_PCODE_READ_MEM_LATENCY,
        &mut val,
        ptr::null_mut(),
    ) != 0
    {
        return;
    }
    (*display).wm.skl_latency[0] =
        crate::linux::registers::REG_FIELD_GET(GEN9_MEM_LATENCY_LEVEL_0_4_MASK, val) as u16;
    (*display).wm.skl_latency[1] =
        crate::linux::registers::REG_FIELD_GET(GEN9_MEM_LATENCY_LEVEL_1_5_MASK, val) as u16;
    (*display).wm.skl_latency[2] =
        crate::linux::registers::REG_FIELD_GET(GEN9_MEM_LATENCY_LEVEL_2_6_MASK, val) as u16;
    (*display).wm.skl_latency[3] =
        crate::linux::registers::REG_FIELD_GET(GEN9_MEM_LATENCY_LEVEL_3_7_MASK, val) as u16;
    val = 1;
    if intel_parent_pcode_read(
        display,
        GEN9_PCODE_READ_MEM_LATENCY,
        &mut val,
        ptr::null_mut(),
    ) != 0
    {
        return;
    }
    (*display).wm.skl_latency[4] =
        crate::linux::registers::REG_FIELD_GET(GEN9_MEM_LATENCY_LEVEL_0_4_MASK, val) as u16;
    (*display).wm.skl_latency[5] =
        crate::linux::registers::REG_FIELD_GET(GEN9_MEM_LATENCY_LEVEL_1_5_MASK, val) as u16;
    (*display).wm.skl_latency[6] =
        crate::linux::registers::REG_FIELD_GET(GEN9_MEM_LATENCY_LEVEL_2_6_MASK, val) as u16;
    (*display).wm.skl_latency[7] =
        crate::linux::registers::REG_FIELD_GET(GEN9_MEM_LATENCY_LEVEL_3_7_MASK, val) as u16;
}

unsafe fn skl_setup_wm_latency(display: *mut IntelDisplay) {
    (*display).wm.num_levels = if has_hw_sagv_wm(display) { 6 } else { 8 };
    if DISPLAY_VER(display) >= 14 {
        for (level, reg) in [
            MTL_LATENCY_LP0_LP1,
            MTL_LATENCY_LP2_LP3,
            MTL_LATENCY_LP4_LP5,
        ]
        .into_iter()
        .enumerate()
        {
            let val = intel_de_read(display, reg);
            (*display).wm.skl_latency[level * 2] =
                crate::linux::registers::REG_FIELD_GET(MTL_LATENCY_LEVEL_EVEN_MASK, val) as u16;
            if level * 2 + 1 < 8 {
                (*display).wm.skl_latency[level * 2 + 1] =
                    crate::linux::registers::REG_FIELD_GET(MTL_LATENCY_LEVEL_ODD_MASK, val) as u16;
            }
        }
    } else {
        skl_read_wm_latency(display);
    }
    adjust_wm_latency(display);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_watermark_max_latency(
    display: *mut IntelDisplay,
    initial_wm_level: i32,
) -> u32 {
    if display.is_null() || initial_wm_level < 0 {
        return 0;
    }
    for level in (initial_wm_level..(*display).wm.num_levels as i32).rev() {
        let latency = wm_latency(display, level);
        if latency != 0 {
            return latency;
        }
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_wm_init(display: *mut IntelDisplay) {
    if display.is_null() || !platform_alderlake_p(display) {
        return;
    }
    intel_sagv_init(display);
    skl_setup_wm_latency(display);
    (*display).wm.funcs =
        ptr::addr_of!(crate::intel_wm_upstream::SKL_WM_FUNCS) as *mut IntelWmFuncs;
}

fn fixed16_from_u32(value: u32) -> u32 {
    value.wrapping_shl(16)
}
fn fixed16_to_u32_ceil(value: u32) -> u32 {
    value.saturating_add(FP_ONE - 1) / FP_ONE
}
fn fixed16_div_ceil(value: u32, divisor: u32) -> u32 {
    if divisor == 0 {
        FP_MAX
    } else {
        (((value as u64) << 16).saturating_add(divisor as u64 - 1) / divisor as u64)
            .min(FP_MAX as u64) as u32
    }
}
fn fixed16_div_round_up(value: u32, divisor: u32) -> u32 {
    if divisor == 0 {
        u32::MAX
    } else {
        ((value as u64).saturating_add(divisor as u64 - 1) / divisor as u64).min(u32::MAX as u64)
            as u32
    }
}
fn fixed16_mul_u32(value: u32, fixed: u32) -> u32 {
    (value as u64)
        .saturating_mul(fixed as u64)
        .min(FP_MAX as u64) as u32
}
fn fixed16_mul_u32_ceil(value: u32, fixed: u32) -> u32 {
    (((value as u64).saturating_mul(fixed as u64)).saturating_add(FP_ONE as u64 - 1)
        / FP_ONE as u64)
        .min(u32::MAX as u64) as u32
}
fn fixed16_add_u32(value: u32, add: u32) -> u32 {
    value.saturating_add(fixed16_from_u32(add))
}

unsafe fn plane_wm(
    state: *const IntelCrtcState,
    plane_id: PlaneId,
) -> *const crate::intel_display_types_upstream::SklPlaneWm {
    if state.is_null() || plane_id >= I915_MAX_PLANES {
        return ptr::null();
    }
    ptr::addr_of!((*skl_wm_state_const_ptr(state)).optimal.planes[plane_id as usize])
}

unsafe fn plane_wm_mut(
    state: *mut IntelCrtcState,
    plane_id: PlaneId,
) -> *mut crate::intel_display_types_upstream::SklPlaneWm {
    if state.is_null() || plane_id >= I915_MAX_PLANES {
        return ptr::null_mut();
    }
    ptr::addr_of_mut!((*skl_wm_state_ptr(state)).optimal.planes[plane_id as usize])
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_plane_wm_level(
    pipe_wm: *const SklPipeWm,
    plane_id: PlaneId,
    level: i32,
) -> *const SklWmLevel {
    if pipe_wm.is_null() || plane_id >= I915_MAX_PLANES || !(0..8).contains(&level) {
        return ptr::null();
    }
    let plane = &(*pipe_wm).planes[plane_id as usize];
    if level == 0 && (*pipe_wm).use_sagv_wm {
        ptr::addr_of!(plane.sagv.wm0)
    } else {
        ptr::addr_of!(plane.wm[level as usize])
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_plane_trans_wm(
    pipe_wm: *const SklPipeWm,
    plane_id: PlaneId,
) -> *const SklWmLevel {
    if pipe_wm.is_null() || plane_id >= I915_MAX_PLANES {
        return ptr::null();
    }
    let plane = &(*pipe_wm).planes[plane_id as usize];
    if (*pipe_wm).use_sagv_wm {
        ptr::addr_of!(plane.sagv.trans_wm)
    } else {
        ptr::addr_of!(plane.trans_wm)
    }
}

fn wm_level_equals(a: &SklWmLevel, b: &SklWmLevel) -> bool {
    a.enable == b.enable
        && a.ignore_lines == b.ignore_lines
        && a.lines == b.lines
        && a.blocks == b.blocks
        && a.auto_min_alloc_wm_enable == b.auto_min_alloc_wm_enable
}

fn ddb_entries_overlap(a: &SklDdbEntry, b: &SklDdbEntry) -> bool {
    a.start < a.end && b.start < b.end && a.start < b.end && b.start < a.end
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_ddb_allocation_overlaps(
    ddb: *const SklDdbEntry,
    entries: *const SklDdbEntry,
    num_entries: i32,
    ignore_idx: i32,
) -> bool {
    if ddb.is_null() || entries.is_null() || num_entries <= 0 {
        return false;
    }
    for idx in 0..num_entries as usize {
        if idx as i32 != ignore_idx && ddb_entries_overlap(&*ddb, &*entries.add(idx)) {
            return true;
        }
    }
    false
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_plane_relative_data_rate(
    crtc_state: *const IntelCrtcState,
    plane: *mut IntelPlane,
    width: i32,
    height: i32,
    cpp: i32,
) -> u32 {
    if crtc_state.is_null() || plane.is_null() || width <= 0 || height <= 0 || cpp <= 0 {
        return 0;
    }
    let display = display_from_crtc_state(crtc_state);
    if !display.is_null()
        && DISPLAY_VER(display) >= 13
        && DISPLAY_VER(display) <= 20
        && (*crtc_state).uapi.async_flip
        && (*plane).async_flip.is_some()
    {
        return 0;
    }
    (width as u32)
        .saturating_mul(height as u32)
        .saturating_mul(cpp as u32)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_wm0_prefill_lines(crtc_state: *const IntelCrtcState) -> u32 {
    if crtc_state.is_null() || (*crtc_state).uapi.crtc.is_null() {
        return 0;
    }
    let crtc = (*crtc_state).uapi.crtc.cast::<IntelCrtc>();
    let wm = skl_wm_state_const_ptr(crtc_state);
    let mut lines = 0u32;
    for plane in 0..I915_MAX_PLANES {
        if (*crtc).plane_ids_mask & (1u8 << plane) == 0 {
            continue;
        }
        lines = lines.max((*wm).optimal.planes[plane as usize].wm[0].lines as u32);
    }
    lines << 16
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_wm0_prefill_lines_worst(crtc_state: *const IntelCrtcState) -> u32 {
    if crtc_state.is_null() || (*crtc_state).uapi.crtc.is_null() {
        return 0;
    }
    let display = display_from_crtc_state(crtc_state);
    if display.is_null() {
        return 0;
    }
    let crtc = (*crtc_state).uapi.crtc.cast::<IntelCrtc>();
    let plane = (*crtc).base.primary.cast::<IntelPlane>();
    if plane.is_null() {
        return 0;
    }

    let modifier = DRM_FORMAT_MOD_INTEL_Y_TILED;
    let format = drm_get_format_info(
        (*display).drm,
        tk_drm_upstream::uapi_drm_consts::DRM_FORMAT_XBGR16161616F,
        modifier,
    );
    if format.is_null() {
        return 0;
    }

    let scale = skl_scaler_max_total_scale(crtc_state) as u64;
    let pixel_rate = (scale
        .saturating_mul((*crtc_state).hw.pipe_mode.crtc_clock.max(0) as u64)
        .saturating_add(0xffff)
        >> 16)
        .min(u32::MAX as u64) as u32;
    let width = (skl_scaler_max_hscale(crtc_state) as u64)
        .saturating_mul((*crtc_state).hw.pipe_mode.crtc_hdisplay.max(0) as u64)
        .saturating_add(0xffff)
        >> 16;
    let params = match compute_wm_params(
        crtc_state,
        width.min(i32::MAX as u64) as i32,
        format,
        modifier,
        tk_drm_upstream::uapi_drm_consts::DRM_MODE_ROTATE_0,
        pixel_rate,
        0,
        1,
    ) {
        Ok(params) => params,
        Err(_) => return 0,
    };
    let latency = wm_latency_for_params(display, 0, &params);
    let mut level = SklWmLevel {
        min_ddb_alloc: 0,
        min_ddb_alloc_uv: 0,
        blocks: 0,
        lines: 0,
        enable: false,
        ignore_lines: false,
        auto_min_alloc_wm_enable: false,
        can_sagv: false,
        _pad_11: [0; 1],
    };
    compute_plane_wm(crtc_state, plane, 0, latency, &params, 0, 0, &mut level);
    if level.min_ddb_alloc == u16::MAX {
        level.lines = 255;
    }
    (level.lines as u32) << 16
}

unsafe fn plane_wm_selected_equals(
    display: *mut IntelDisplay,
    plane_id: PlaneId,
    old: *const SklPipeWm,
    new: *const SklPipeWm,
) -> bool {
    for level in 0..(*display).wm.num_levels as i32 {
        let a = skl_plane_wm_level(old, plane_id, level);
        let b = skl_plane_wm_level(new, plane_id, level);
        if a.is_null() || b.is_null() || !wm_level_equals(&*a, &*b) {
            return false;
        }
    }
    if has_hw_sagv_wm(display) {
        let a = &(*old).planes[plane_id as usize];
        let b = &(*new).planes[plane_id as usize];
        if !wm_level_equals(&a.sagv.wm0, &b.sagv.wm0)
            || !wm_level_equals(&a.sagv.trans_wm, &b.sagv.trans_wm)
        {
            return false;
        }
    }
    let a = skl_plane_trans_wm(old, plane_id);
    let b = skl_plane_trans_wm(new, plane_id);
    !a.is_null() && !b.is_null() && wm_level_equals(&*a, &*b)
}

unsafe fn wm_latency_for_params(
    display: *mut IntelDisplay,
    level: i32,
    wp: *const SklWmParams,
) -> u32 {
    let mut latency = wm_latency(display, level);
    if latency == 0 {
        return 0;
    }
    if !wp.is_null() && DISPLAY_VER(display) == 9 && (*wp).x_tiled {
        latency += 15;
    }
    latency
}

unsafe fn wm_method1(
    display: *mut IntelDisplay,
    pixel_rate: u32,
    cpp: u8,
    latency: u32,
    block_size: u32,
) -> u32 {
    if latency == 0 || block_size == 0 {
        return FP_MAX;
    }
    let numerator = latency
        .saturating_mul(pixel_rate)
        .saturating_mul(cpp as u32);
    let mut result = fixed16_div_ceil(numerator, 1000u32.saturating_mul(block_size));
    if DISPLAY_VER(display) >= 10 {
        result = fixed16_add_u32(result, 1);
    }
    result
}

unsafe fn wm_method2(
    pixel_rate: u32,
    pipe_htotal: u32,
    latency: u32,
    plane_blocks_per_line: u32,
) -> u32 {
    if latency == 0 || pipe_htotal == 0 {
        return FP_MAX;
    }
    let numerator = latency.saturating_mul(pixel_rate);
    let intermediate = numerator.saturating_add(pipe_htotal.saturating_mul(1000).saturating_sub(1))
        / pipe_htotal.saturating_mul(1000).max(1);
    fixed16_mul_u32(intermediate, plane_blocks_per_line)
}

unsafe fn compute_wm_params(
    crtc_state: *const IntelCrtcState,
    width: i32,
    format: *const DrmFormatInfo,
    modifier: u64,
    rotation: u32,
    plane_pixel_rate: u32,
    color_plane: usize,
    pan_x: u32,
) -> Result<SklWmParams, i32> {
    if crtc_state.is_null() || format.is_null() || color_plane >= 4 {
        return Err(-tk_linux_kapi::config::EINVAL);
    }
    let display = display_from_crtc_state(crtc_state);
    if display.is_null() {
        return Err(-tk_linux_kapi::config::EINVAL);
    }
    let semiplanar = intel_format_info_is_yuv_semiplanar(format, modifier);
    if color_plane == 1 && !semiplanar {
        return Err(-tk_linux_kapi::config::EINVAL);
    }
    let mut wp = SklWmParams::default();
    wp.x_tiled = modifier == DRM_FORMAT_MOD_INTEL_X_TILED;
    wp.y_tiled = !wp.x_tiled && intel_fb_is_tiled_modifier(modifier);
    wp.rc_surface = intel_fb_is_ccs_modifier(modifier);
    wp.width = if color_plane == 1 && semiplanar {
        width.max(0) as u32 / 2
    } else {
        width.max(0) as u32
    };
    wp.cpp = (*format)._anon_6.cpp[color_plane];
    if wp.cpp == 0 {
        return Err(-tk_linux_kapi::config::EINVAL);
    }
    wp.plane_pixel_rate = plane_pixel_rate;
    wp.dbuf_block_size =
        if DISPLAY_VER(display) >= 11 && modifier == DRM_FORMAT_MOD_INTEL_YF_TILED && wp.cpp == 1 {
            256
        } else {
            512
        };
    wp.y_min_scanlines = if rotation
        & (tk_drm_upstream::uapi_drm_consts::DRM_MODE_ROTATE_90
            | tk_drm_upstream::uapi_drm_consts::DRM_MODE_ROTATE_270)
        != 0
    {
        match wp.cpp {
            1 => 16,
            2 => 8,
            4 => 4,
            _ => return Err(-tk_linux_kapi::config::EINVAL),
        }
    } else {
        4
    };
    if DISPLAY_VER(display) == 9 {
        wp.y_min_scanlines *= 2;
    }
    wp.plane_bytes_per_line = wp.width.saturating_mul(wp.cpp as u32);
    if wp.y_tiled {
        let mut blocks = wp
            .plane_bytes_per_line
            .saturating_mul(wp.y_min_scanlines)
            .saturating_add(wp.dbuf_block_size - 1)
            / wp.dbuf_block_size;
        if DISPLAY_VER(display) >= 30 {
            blocks += u32::from(pan_x != 0);
        } else if DISPLAY_VER(display) >= 10 {
            blocks += 1;
        }
        wp.plane_blocks_per_line = fixed16_div_ceil(blocks, wp.y_min_scanlines);
    } else {
        let mut blocks = wp
            .plane_bytes_per_line
            .saturating_add(wp.dbuf_block_size - 1)
            / wp.dbuf_block_size;
        if !wp.x_tiled || DISPLAY_VER(display) >= 10 {
            blocks += 1;
        }
        wp.plane_blocks_per_line = fixed16_from_u32(blocks);
    }
    wp.y_tile_minimum = fixed16_mul_u32(wp.y_min_scanlines, wp.plane_blocks_per_line);
    let htotal = (*crtc_state).hw.pipe_mode.crtc_htotal as u32;
    wp.linetime_us = if plane_pixel_rate == 0 {
        u32::MAX
    } else {
        htotal
            .saturating_mul(1000)
            .saturating_add(plane_pixel_rate - 1)
            / plane_pixel_rate
    };
    Ok(wp)
}

unsafe fn compute_plane_wm(
    crtc_state: *const IntelCrtcState,
    plane: *mut IntelPlane,
    level: i32,
    latency: u32,
    wp: &SklWmParams,
    prev_blocks: u16,
    prev_lines: u8,
    result: &mut SklWmLevel,
) {
    let display = display_from_crtc_state(crtc_state);
    if display.is_null() || plane.is_null() {
        result.min_ddb_alloc = u16::MAX;
        return;
    }
    let minimal_wm0_only = DISPLAY_VER(display) >= 13
        && DISPLAY_VER(display) <= 20
        && (*crtc_state).uapi.async_flip
        && (*plane).async_flip.is_some();
    if latency == 0 || (minimal_wm0_only && level > 0) {
        result.min_ddb_alloc = u16::MAX;
        return;
    }
    let method1 = wm_method1(
        display,
        wp.plane_pixel_rate,
        wp.cpp,
        latency,
        wp.dbuf_block_size,
    );
    let method2 = wm_method2(
        wp.plane_pixel_rate,
        (*crtc_state).hw.pipe_mode.crtc_htotal as u32,
        latency,
        wp.plane_blocks_per_line,
    );
    let selected = if wp.y_tiled {
        method2.max(wp.y_tile_minimum)
    } else if DISPLAY_VER(display) >= 35 {
        method2
    } else {
        let narrow_by_rate = (wp.cpp as u32 * (*crtc_state).hw.pipe_mode.crtc_htotal as u32
            / wp.dbuf_block_size
            < 1)
            && (wp.plane_bytes_per_line / wp.dbuf_block_size < 1);
        if narrow_by_rate {
            method2
        } else if latency >= wp.linetime_us {
            if DISPLAY_VER(display) == 9 {
                method1.min(method2)
            } else {
                method2
            }
        } else {
            method1
        }
    };
    let mut blocks = fixed16_to_u32_ceil(selected);
    if DISPLAY_VER(display) < 30 {
        blocks = blocks.saturating_add(1);
    }
    let has_lines = DISPLAY_VER(display) >= 10;
    if has_lines {
        blocks = blocks.max(fixed16_to_u32_ceil(wp.plane_blocks_per_line));
    }
    let mut lines = fixed16_div_round_up(selected, wp.plane_blocks_per_line);
    if DISPLAY_VER(display) == 9 {
        if level == 0 && wp.rc_surface {
            blocks = blocks.saturating_add(fixed16_to_u32_ceil(wp.y_tile_minimum));
        }
        if (1..=7).contains(&level) {
            if wp.y_tiled {
                blocks = blocks.saturating_add(fixed16_to_u32_ceil(wp.y_tile_minimum));
                lines = lines.saturating_add(wp.y_min_scanlines);
            } else {
                blocks = blocks.saturating_add(1);
            }
        }
    }
    blocks = blocks.max(prev_blocks as u32);
    lines = lines.max(prev_lines as u32);
    let mut min_ddb_alloc = 0u32;
    if DISPLAY_VER(display) >= 11 {
        if wp.y_tiled {
            let remainder = lines % wp.y_min_scanlines;
            let extra_lines = if remainder == 0 {
                wp.y_min_scanlines
            } else {
                wp.y_min_scanlines * 2 - remainder
            };
            min_ddb_alloc =
                fixed16_mul_u32_ceil(lines.saturating_add(extra_lines), wp.plane_blocks_per_line);
        } else {
            min_ddb_alloc = blocks.saturating_add(blocks.saturating_add(9) / 10);
        }
    }
    if !has_lines {
        lines = 0;
    }
    let max_lines = if DISPLAY_VER(display) >= 13 { 255 } else { 31 };
    if lines > max_lines {
        result.min_ddb_alloc = u16::MAX;
        return;
    }
    result.blocks = blocks.min(u16::MAX as u32) as u16;
    result.lines = lines.min(u8::MAX as u32) as u8;
    result.min_ddb_alloc = min_ddb_alloc
        .max(blocks)
        .saturating_add(1)
        .min(u16::MAX as u32) as u16;
    result.enable = true;
    result.auto_min_alloc_wm_enable = false;
    result.can_sagv = !has_sagv_wm(display)
        && (*display).sagv.block_time_us != 0
        && latency >= (*display).sagv.block_time_us;
}

unsafe fn compute_wm_levels(
    crtc_state: *const IntelCrtcState,
    plane: *mut IntelPlane,
    params: &SklWmParams,
    levels: &mut [SklWmLevel; 8],
) {
    let display = display_from_crtc_state(crtc_state);
    if display.is_null() {
        return;
    }
    for level in 0..(*display).wm.num_levels as usize {
        let latency = wm_latency_for_params(display, level as i32, params);
        let (prev_blocks, prev_lines) = if level == 0 {
            (0, 0)
        } else {
            (levels[level - 1].blocks, levels[level - 1].lines)
        };
        compute_plane_wm(
            crtc_state,
            plane,
            level as i32,
            latency,
            params,
            prev_blocks,
            prev_lines,
            &mut levels[level],
        );
    }
}

unsafe fn plane_state_wm_params(
    crtc_state: *const IntelCrtcState,
    plane_state: *const IntelPlaneState,
    color_plane: usize,
) -> Result<SklWmParams, i32> {
    if plane_state.is_null() || (*plane_state).hw.fb.is_null() {
        return Err(-tk_linux_kapi::config::EINVAL);
    }
    let fb = (*plane_state).hw.fb;
    let format = (*fb).format;
    let width_fixed = (*plane_state)
        .uapi
        .src
        .x2
        .saturating_sub((*plane_state).uapi.src.x1);
    let width = width_fixed >> 16;
    let rate = intel_plane_pixel_rate(crtc_state, plane_state);
    compute_wm_params(
        crtc_state,
        width,
        format,
        (*fb).modifier,
        (*plane_state).hw.rotation,
        rate,
        color_plane,
        (*plane_state).uapi.src.x1 as u32,
    )
}

unsafe fn build_plane_wm_single(
    crtc_state: *mut IntelCrtcState,
    plane_state: *const IntelPlaneState,
    plane: *mut IntelPlane,
    color_plane: usize,
) -> i32 {
    let display = display_from_crtc_state(crtc_state);
    if display.is_null() || plane_state.is_null() || plane.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let wm = plane_wm_mut(crtc_state, (*plane).id);
    if wm.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let params = match plane_state_wm_params(crtc_state, plane_state, color_plane) {
        Ok(params) => params,
        Err(err) => return err,
    };
    compute_wm_levels(crtc_state, plane, &params, &mut (*wm).wm);
    compute_transition_wm(display, &mut (*wm).trans_wm, &(*wm).wm[0], &params);
    if has_sagv_wm(display) {
        let mut latency = 0;
        if (*display).sagv.block_time_us != 0 {
            latency = (*display)
                .sagv
                .block_time_us
                .saturating_add(wm_latency_for_params(display, 0, &params));
        }
        compute_plane_wm(
            crtc_state,
            plane,
            0,
            latency,
            &params,
            (*wm).wm[0].blocks,
            (*wm).wm[0].lines,
            &mut (*wm).sagv.wm0,
        );
        compute_transition_wm(display, &mut (*wm).sagv.trans_wm, &(*wm).sagv.wm0, &params);
    }
    0
}

unsafe fn build_plane_wm_skl(
    crtc_state: *mut IntelCrtcState,
    plane_state: *const IntelPlaneState,
) -> i32 {
    if plane_state.is_null() || (*plane_state).uapi.plane.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let plane = (*plane_state).uapi.plane.cast::<IntelPlane>();
    let wm = plane_wm_mut(crtc_state, (*plane).id);
    if wm.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    ptr::write_bytes(wm, 0, 1);
    if !crate::intel_wm_upstream::intel_wm_plane_visible(crtc_state, plane_state) {
        return 0;
    }
    let ret = build_plane_wm_single(crtc_state, plane_state, plane, 0);
    if ret != 0 {
        return ret;
    }
    let fb = (*plane_state).hw.fb;
    if !fb.is_null()
        && !(*fb).format.is_null()
        && (*(*fb).format).is_yuv
        && (*(*fb).format).num_planes > 1
    {
        let params = match plane_state_wm_params(crtc_state, plane_state, 1) {
            Ok(params) => params,
            Err(err) => return err,
        };
        let mut uv_wm: [SklWmLevel; 8] = core::array::from_fn(|_| SklWmLevel {
            min_ddb_alloc: 0,
            min_ddb_alloc_uv: 0,
            blocks: 0,
            lines: 0,
            enable: false,
            ignore_lines: false,
            auto_min_alloc_wm_enable: false,
            can_sagv: false,
            _pad_11: [0; 1],
        });
        compute_wm_levels(crtc_state, plane, &params, &mut uv_wm);
        let display = display_from_crtc_state(crtc_state);
        for level in 0..(*display).wm.num_levels as usize {
            (*wm).wm[level].min_ddb_alloc_uv = uv_wm[level].min_ddb_alloc;
        }
    }
    0
}

unsafe fn build_plane_wm_icl(
    crtc_state: *mut IntelCrtcState,
    plane_state: *const IntelPlaneState,
) -> i32 {
    if plane_state.is_null() || (*plane_state).uapi.plane.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let display = display_from_crtc_state(crtc_state);
    let plane = (*plane_state).uapi.plane.cast::<IntelPlane>();
    if (*plane_state).is_y_plane {
        return 0;
    }
    let wm = plane_wm_mut(crtc_state, (*plane).id);
    if wm.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    ptr::write_bytes(wm, 0, 1);
    if !(*plane_state).planar_linked_plane.is_null() {
        let linked = (*plane_state).planar_linked_plane;
        let fb = (*plane_state).hw.fb;
        if !crate::intel_wm_upstream::intel_wm_plane_visible(crtc_state, plane_state)
            || fb.is_null()
            || (*fb).format.is_null()
            || !(*(*fb).format).is_yuv
            || (*(*fb).format).num_planes <= 1
        {
            return -tk_linux_kapi::config::EINVAL;
        }
        let ret = build_plane_wm_single(crtc_state, plane_state, linked, 0);
        if ret != 0 {
            return ret;
        }
        build_plane_wm_single(crtc_state, plane_state, plane, 1)
    } else if crate::intel_wm_upstream::intel_wm_plane_visible(crtc_state, plane_state) {
        build_plane_wm_single(crtc_state, plane_state, plane, 0)
    } else {
        0
    }
}

unsafe fn compute_transition_wm(
    display: *mut IntelDisplay,
    trans: &mut SklWmLevel,
    wm0: &SklWmLevel,
    params: &SklWmParams,
) {
    if !skl_watermark_ipc_enabled(display) || DISPLAY_VER(display) == 9 {
        return;
    }
    let trans_min = if DISPLAY_VER(display) >= 11 { 4u32 } else { 14 };
    let trans_amount = if DISPLAY_VER(display) == 10 { 0 } else { 10 };
    let wm0_blocks = wm0.blocks.saturating_sub(1) as u32;
    let blocks = if params.y_tiled {
        wm0_blocks.max(fixed16_mul_u32_ceil(2, params.y_tile_minimum)) + trans_min + trans_amount
    } else {
        wm0_blocks + trans_min + trans_amount
    } + 1;
    trans.blocks = blocks.min(u16::MAX as u32) as u16;
    trans.min_ddb_alloc = (wm0.min_ddb_alloc as u32)
        .max(blocks + 1)
        .min(u16::MAX as u32) as u16;
    trans.enable = true;
}

unsafe fn build_pipe_wm(state: *mut IntelAtomicState, crtc: *mut IntelCrtc) -> i32 {
    let display = display_from_atomic(state);
    let crtc_state = atomic_new_crtc_state(state, crtc);
    if display.is_null() || crtc_state.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let dev = (*state).base.dev;
    if dev.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let num_planes = (*dev).mode_config.num_total_plane.max(0) as usize;
    for index in 0..num_planes {
        let entry = (*state).base.planes.add(index);
        if (*entry).new_state.is_null() || (*entry).ptr.is_null() {
            continue;
        }
        let plane = (*entry).ptr.cast::<IntelPlane>();
        if (*plane).pipe != (*crtc).pipe {
            continue;
        }
        let plane_state = (*entry).new_state.cast::<IntelPlaneState>();
        let ret = if DISPLAY_VER(display) >= 11 {
            build_plane_wm_icl(crtc_state, plane_state)
        } else {
            build_plane_wm_skl(crtc_state, plane_state)
        };
        if ret != 0 {
            return ret;
        }
    }
    ptr::copy_nonoverlapping(
        ptr::addr_of!((*skl_wm_state_ptr(crtc_state)).raw),
        ptr::addr_of_mut!((*skl_wm_state_ptr(crtc_state)).optimal),
        1,
    );
    check_wm_vblank(crtc_state)
}

unsafe fn check_wm_vblank(crtc_state: *mut IntelCrtcState) -> i32 {
    let display = display_from_crtc_state(crtc_state);
    if display.is_null() || !(*crtc_state).hw.active {
        return 0;
    }
    let mut ctx: crate::skl_prefill_types_upstream::SklPrefillCtx = mem::zeroed();
    skl_prefill_init(&mut ctx, crtc_state);
    let crtc = (*crtc_state).uapi.crtc.cast::<IntelCrtc>();
    let mut selected = -1;
    for level in (0..(*display).wm.num_levels as i32).rev() {
        let mut latency = wm_latency(display, level);
        if latency == 0 {
            continue;
        }
        if level == 0 {
            latency = 0;
        }
        if !skl_prefill_vblank_too_short(&ctx, crtc_state, latency) {
            selected = level;
            break;
        }
    }
    if selected < 0 {
        return -tk_linux_kapi::config::EINVAL;
    }
    (*crtc_state).wm_level_disabled = selected < (*display).wm.num_levels as i32 - 1;
    for level in selected + 1..(*display).wm.num_levels as i32 {
        for plane_id in 0..I915_MAX_PLANES as usize {
            if (*crtc).plane_ids_mask & (1 << plane_id) != 0 {
                (*skl_wm_state_ptr(crtc_state)).optimal.planes[plane_id].wm[level as usize]
                    .enable = false;
            }
        }
    }
    if has_sagv_wm(display)
        && (*display).sagv.block_time_us != 0
        && skl_prefill_vblank_too_short(&ctx, crtc_state, (*display).sagv.block_time_us)
    {
        for plane_id in 0..I915_MAX_PLANES as usize {
            if (*crtc).plane_ids_mask & (1 << plane_id) != 0 {
                let wm = &mut (*skl_wm_state_ptr(crtc_state)).optimal.planes[plane_id];
                wm.sagv.wm0.enable = false;
                wm.sagv.trans_wm.enable = false;
            }
        }
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_compute_wm(state: *mut IntelAtomicState) -> i32 {
    let display = display_from_atomic(state);
    if display.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let mut error = 0;
    for_each_new_crtc(state, |crtc, _| unsafe {
        if error == 0 {
            error = build_pipe_wm(state, crtc);
        }
    });
    if error != 0 {
        return error;
    }
    error = compute_ddb(state);
    if error != 0 {
        return error;
    }
    for_each_new_crtc(state, |crtc, crtc_state| unsafe {
        if error != 0 {
            return;
        }
        (*skl_wm_state_ptr(crtc_state)).optimal.use_sagv_wm = has_sagv_wm(display)
            && !has_hw_sagv_wm(display)
            && intel_crtc_can_enable_sagv(crtc_state);
        error = skl_wm_add_affected_planes(state, crtc);
    });
    error
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

unsafe fn for_each_new_crtc(
    state: *mut IntelAtomicState,
    mut f: impl FnMut(*mut IntelCrtc, *mut IntelCrtcState),
) {
    if state.is_null() || (*state).base.crtcs.is_null() || (*state).base.dev.is_null() {
        return;
    }
    let count = (*(*state).base.dev).mode_config.num_crtc.max(0) as usize;
    for index in 0..count {
        let entry = (*state).base.crtcs.add(index);
        if !(*entry).ptr.is_null() && !(*entry).new_state.is_null() {
            f(
                (*entry).ptr.cast::<IntelCrtc>(),
                (*entry).new_state.cast::<IntelCrtcState>(),
            );
        }
    }
}

unsafe fn intel_crtc_ddb_weight(crtc_state: *const IntelCrtcState) -> u32 {
    if crtc_state.is_null() || !(*crtc_state).hw.active {
        return 0;
    }
    let mut hdisplay = 0;
    let mut vdisplay = 0;
    tk_drm_upstream::drm_modes_upstream::drm_mode_get_hv_timing(
        ptr::addr_of!((*crtc_state).hw.pipe_mode),
        &mut hdisplay,
        &mut vdisplay,
    );
    hdisplay.max(0) as u32
}

unsafe fn crtc_weights_for_slice_set(
    dbuf: *const IntelDbufState,
    pipe: usize,
    out_start: &mut u32,
    out_end: &mut u32,
    out_total: &mut u32,
) {
    *out_start = 0;
    *out_end = 0;
    *out_total = 0;
    let wanted_slices = (*dbuf).slices[pipe];
    for current in 0..I915_MAX_PIPES as usize {
        if (*dbuf).slices[current] != wanted_slices {
            continue;
        }
        let weight = (*dbuf).weight[current];
        *out_total = out_total.saturating_add(weight);
        if current < pipe {
            *out_start = out_start.saturating_add(weight);
            *out_end = out_end.saturating_add(weight);
        } else if current == pipe {
            *out_end = out_end.saturating_add(weight);
        }
    }
}

unsafe fn allocate_crtc_ddb(state: *mut IntelAtomicState, crtc: *mut IntelCrtc) -> i32 {
    let display = display_from_atomic(state);
    if display.is_null() || crtc.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let pipe = (*crtc).pipe;
    if pipe < 0 || pipe >= I915_MAX_PIPES {
        return -tk_linux_kapi::config::EINVAL;
    }
    let pipe_idx = pipe as usize;
    let old = old_dbuf_state(state);
    let new = intel_atomic_get_dbuf_state(state);
    if old.is_null() || tk_linux_kapi::config::IS_ERR(new) {
        return -tk_linux_kapi::config::EINVAL;
    }
    let mut mbus_offset = 0u16;
    let mut new_ddb = SklDdbEntry { start: 0, end: 0 };

    if (*new).weight[pipe_idx] != 0 {
        let Some(slice_size) = display_dbuf_slice_size(display) else {
            return -tk_linux_kapi::config::EINVAL;
        };
        let Some(slice_ddb) = ddb_entry_for_slices(slice_size, (*new).slices[pipe_idx]) else {
            return -tk_linux_kapi::config::EINVAL;
        };
        let slice_set = (*new).slices[pipe_idx];
        let offset_mask = if slice_set & DBUF_S1_S2 != 0 {
            1 << DBUF_S1
        } else if slice_set & DBUF_S3_S4 != 0 {
            1 << DBUF_S3
        } else {
            0
        };
        let Some(offset_ddb) = ddb_entry_for_slices(slice_size, offset_mask) else {
            return -tk_linux_kapi::config::EINVAL;
        };
        mbus_offset = offset_ddb.start;
        let range = ddb_entry_size(&slice_ddb) as u32;
        let mut weight_start = 0;
        let mut weight_end = 0;
        let mut weight_total = 0;
        crtc_weights_for_slice_set(
            new,
            pipe_idx,
            &mut weight_start,
            &mut weight_end,
            &mut weight_total,
        );
        if weight_total == 0 {
            return -tk_linux_kapi::config::EINVAL;
        }
        let start = range.saturating_mul(weight_start) / weight_total;
        let end = range.saturating_mul(weight_end) / weight_total;
        let abs_start = (slice_ddb.start as u32)
            .saturating_sub(mbus_offset as u32)
            .saturating_add(start);
        let abs_end = (slice_ddb.start as u32)
            .saturating_sub(mbus_offset as u32)
            .saturating_add(end);
        if abs_end > u16::MAX as u32 || abs_start > abs_end {
            return -tk_linux_kapi::config::EINVAL;
        }
        new_ddb = SklDdbEntry {
            start: abs_start as u16,
            end: abs_end as u16,
        };
    }

    let changed = (*old).slices[pipe_idx] != (*new).slices[pipe_idx]
        || !ddb_entry_equal(&(*old).ddb[pipe_idx], &new_ddb);
    (*new).ddb[pipe_idx].start = new_ddb.start;
    (*new).ddb[pipe_idx].end = new_ddb.end;
    if !changed {
        return 0;
    }
    let ret = intel_atomic_lock_global_state(ptr::addr_of_mut!((*new).base));
    if ret != 0 {
        return ret;
    }
    let crtc_state = intel_atomic_get_crtc_state(ptr::addr_of_mut!((*state).base), crtc);
    if tk_linux_kapi::config::IS_ERR(crtc_state) {
        return tk_linux_kapi::config::PTR_ERR(crtc_state);
    }
    if crtc_state.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    (*skl_wm_state_ptr(crtc_state)).ddb = SklDdbEntry {
        start: mbus_offset.saturating_add(new_ddb.start),
        end: mbus_offset.saturating_add(new_ddb.end),
    };
    0
}

unsafe fn compute_ddb(state: *mut IntelAtomicState) -> i32 {
    let display = display_from_atomic(state);
    if display.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let mut first_new: Option<(*mut IntelCrtc, *mut IntelCrtcState)> = None;
    for_each_new_crtc(state, |crtc, crtc_state| {
        if first_new.is_none() {
            first_new = Some((crtc, crtc_state));
        }
    });
    if first_new.is_none() {
        return 0;
    }
    let old = old_dbuf_state(state);
    let new = intel_atomic_get_dbuf_state(state);
    if old.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    if tk_linux_kapi::config::IS_ERR(new) {
        return tk_linux_kapi::config::PTR_ERR(new);
    }
    if new.is_null() {
        return -tk_linux_kapi::config::ENOMEM;
    }

    (*new).active_pipes = intel_calc_active_pipes(state, (*old).active_pipes);
    if (*old).active_pipes != (*new).active_pipes {
        let ret = intel_atomic_lock_global_state(ptr::addr_of_mut!((*new).base));
        if ret != 0 {
            return ret;
        }
    }

    if has_mbus_joining(display) {
        (*new).joined_mbus = adlp_should_join_mbus((*new).active_pipes);
        if (*old).joined_mbus != (*new).joined_mbus {
            let ret = intel_cdclk_state_set_joined_mbus(state, (*new).joined_mbus);
            if ret != 0 {
                return ret;
            }
        }
    }

    let mut slice_error = 0;
    for_each_display_crtc(display, |crtc| unsafe {
        if slice_error != 0 {
            return;
        }
        let pipe = (*crtc).pipe;
        if pipe < 0 || pipe >= I915_MAX_PIPES {
            slice_error = -tk_linux_kapi::config::EINVAL;
            return;
        }
        let mask = if DISPLAY_VER(display) >= 13 {
            adlp_slice_mask_for(pipe as u8, (*new).active_pipes, (*new).joined_mbus)
        } else {
            0
        };
        let idx = pipe as usize;
        if (*new).slices[idx] != mask {
            (*new).slices[idx] = mask;
            slice_error = intel_atomic_lock_global_state(ptr::addr_of_mut!((*new).base));
        }
    });
    if slice_error != 0 {
        return slice_error;
    }

    (*new).enabled_slices = dbuf_enabled_slices(new);
    if (*old).enabled_slices != (*new).enabled_slices || (*old).joined_mbus != (*new).joined_mbus {
        let ret = intel_atomic_serialize_global_state(ptr::addr_of_mut!((*new).base));
        if ret != 0 {
            return ret;
        }
    }

    for_each_new_crtc(state, |crtc, crtc_state| unsafe {
        let pipe = (*crtc).pipe;
        if pipe >= 0 && pipe < I915_MAX_PIPES {
            let idx = pipe as usize;
            let weight = intel_crtc_ddb_weight(crtc_state);
            if (*old).weight[idx] != weight {
                (*new).weight[idx] = weight;
                let ret = intel_atomic_lock_global_state(ptr::addr_of_mut!((*new).base));
                if ret != 0 {
                    slice_error = ret;
                }
            }
        }
    });
    if slice_error != 0 {
        return slice_error;
    }

    let mut error = 0;
    for_each_display_crtc(display, |crtc| unsafe {
        if error == 0 {
            error = allocate_crtc_ddb(state, crtc);
        }
    });
    if error != 0 {
        return error;
    }

    for_each_new_crtc(state, |crtc, _| unsafe {
        if error != 0 {
            return;
        }
        error = allocate_plane_ddb(state, crtc);
        if error == 0 {
            error = skl_ddb_add_affected_planes(state, crtc);
        }
    });
    error
}

unsafe fn has_mbus_joining(display: *mut IntelDisplay) -> bool {
    platform_alderlake_p(display) || DISPLAY_VER(display) >= 14
}

unsafe fn dbuf_enabled_slices(state: *const IntelDbufState) -> u8 {
    let mut enabled = 1 << DBUF_S1;
    for mask in (*state).slices {
        enabled |= mask;
    }
    enabled
}

unsafe fn total_plane_data_rate(state: *const IntelCrtcState, display: *mut IntelDisplay) -> u64 {
    let mut rate = 0u64;
    for plane_id in 0..I915_MAX_PLANES as usize {
        if plane_id == PLANE_CURSOR as usize {
            continue;
        }
        rate = rate.saturating_add((*state).rel_data_rate[plane_id]);
        if DISPLAY_VER(display) < 11 {
            rate = rate.saturating_add((*state).rel_data_rate_y[plane_id]);
        }
    }
    rate
}

unsafe fn cursor_allocation(crtc_state: *const IntelCrtcState, num_active: u32) -> u16 {
    let display = display_from_crtc_state(crtc_state);
    if display.is_null() || (*crtc_state).uapi.crtc.is_null() {
        return 0;
    }
    let crtc = (*crtc_state).uapi.crtc.cast::<IntelCrtc>();
    let plane = plane_on_crtc(crtc, PLANE_CURSOR);
    if plane.is_null() {
        return 0;
    }
    let format = drm_get_format_info(
        (*display).drm,
        tk_drm_upstream::uapi_drm_consts::DRM_FORMAT_ARGB8888,
        0,
    );
    if format.is_null() {
        return 0;
    }
    let width = (*(*display).drm).mode_config.cursor_width;
    let pixel_rate = (*crtc_state).pixel_rate;
    let params = match compute_wm_params(
        crtc_state,
        width as i32,
        format,
        0,
        tk_drm_upstream::uapi_drm_consts::DRM_MODE_ROTATE_0,
        pixel_rate,
        0,
        0,
    ) {
        Ok(params) => params,
        Err(_) => return 0,
    };
    let mut wm = SklWmLevel {
        min_ddb_alloc: 0,
        min_ddb_alloc_uv: 0,
        blocks: 0,
        lines: 0,
        enable: false,
        ignore_lines: false,
        auto_min_alloc_wm_enable: false,
        can_sagv: false,
        _pad_11: [0; 1],
    };
    let mut min_alloc = 0u16;
    for level in 0..(*display).wm.num_levels as usize {
        let latency = wm_latency_for_params(display, level as i32, &params);
        let previous = (wm.blocks, wm.lines);
        compute_plane_wm(
            crtc_state,
            plane,
            level as i32,
            latency,
            &params,
            previous.0,
            previous.1,
            &mut wm,
        );
        if wm.min_ddb_alloc == u16::MAX {
            break;
        }
        min_alloc = wm.min_ddb_alloc;
    }
    min_alloc.max(if num_active == 1 { 32 } else { 8 })
}

fn allocate_weighted_plane_ranges(
    start: u16,
    available: u16,
    minima: &[u16; I915_MAX_PLANES as usize],
    rates: &[u64; I915_MAX_PLANES as usize],
    plane_mask: u8,
) -> Option<[(u16, u16); I915_MAX_PLANES as usize]> {
    let mut total_min = 0u32;
    let mut total_rate = 0u64;
    for plane in 0..I915_MAX_PLANES as usize {
        if plane == PLANE_CURSOR as usize || plane_mask & (1 << plane) == 0 {
            continue;
        }
        total_min = total_min.checked_add(minima[plane] as u32)?;
        total_rate = total_rate.saturating_add(rates[plane]);
    }
    if total_min > available as u32 {
        return None;
    }
    let mut remaining = available as u32 - total_min;
    if total_rate == 0 {
        remaining = 0;
    }
    let mut rate_left = total_rate;
    let mut cursor = start as u32;
    let mut ranges = [(0u16, 0u16); I915_MAX_PLANES as usize];
    for plane in 0..I915_MAX_PLANES as usize {
        if plane == PLANE_CURSOR as usize || plane_mask & (1 << plane) == 0 {
            continue;
        }
        let rate = rates[plane];
        let extra = if rate != 0 && rate_left != 0 {
            (remaining as u64)
                .saturating_mul(rate)
                .saturating_add(rate_left - 1)
                / rate_left
        } else {
            0
        }
        .min(remaining as u64) as u32;
        remaining -= extra;
        rate_left = rate_left.saturating_sub(rate);
        let size = (minima[plane] as u32).saturating_add(extra);
        let end = cursor.checked_add(size)?;
        if end > u16::MAX as u32 {
            return None;
        }
        if size != 0 {
            ranges[plane] = (cursor as u16, end as u16);
        }
        cursor = end;
    }
    Some(ranges)
}

fn disable_wm_level(level: &mut SklWmLevel) {
    *level = SklWmLevel {
        min_ddb_alloc: 0,
        min_ddb_alloc_uv: 0,
        blocks: 0,
        lines: 0,
        enable: false,
        ignore_lines: false,
        auto_min_alloc_wm_enable: false,
        can_sagv: false,
        _pad_11: [0; 1],
    };
}

unsafe fn allocate_plane_ddb(state: *mut IntelAtomicState, crtc: *mut IntelCrtc) -> i32 {
    let display = display_from_atomic(state);
    let dbuf = new_dbuf_state(state);
    let crtc_state = atomic_new_crtc_state(state, crtc);
    if display.is_null() || dbuf.is_null() || crtc_state.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let pipe = (*crtc).pipe;
    if pipe < 0 || pipe >= I915_MAX_PIPES {
        return -tk_linux_kapi::config::EINVAL;
    }
    let pipe_idx = pipe as usize;
    let alloc = &(*dbuf).ddb[pipe_idx];
    let alloc_size = ddb_entry_size(alloc);
    let wm_skl = ptr::addr_of_mut!((*skl_wm_state_ptr(crtc_state)));
    ptr::write_bytes(ptr::addr_of_mut!((*wm_skl).plane_ddb), 0, 1);
    ptr::write_bytes(ptr::addr_of_mut!((*wm_skl).plane_ddb_y), 0, 1);
    ptr::write_bytes(ptr::addr_of_mut!((*wm_skl).plane_min_ddb), 0, 1);
    ptr::write_bytes(ptr::addr_of_mut!((*wm_skl).plane_interim_ddb), 0, 1);
    if !(*crtc_state).hw.active || alloc_size == 0 {
        return 0;
    }
    let num_active = (*dbuf).active_pipes.count_ones();
    let cursor_size = cursor_allocation(crtc_state, num_active);
    if cursor_size > alloc_size {
        return -tk_linux_kapi::config::EINVAL;
    }
    let work_size = alloc_size - cursor_size;
    let cursor = plane_wm(crtc_state, PLANE_CURSOR);
    if cursor.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    (*wm_skl).plane_ddb[PLANE_CURSOR as usize] = SklDdbEntry {
        start: alloc.end - cursor_size,
        end: alloc.end,
    };

    let mut minima = [0u16; I915_MAX_PLANES as usize];
    let mut rates = [0u64; I915_MAX_PLANES as usize];
    let mut chosen_level = None;
    let mut required = 0u32;
    for level in (0..(*display).wm.num_levels as usize).rev() {
        required = 0;
        let mut impossible = false;
        for plane_id in 0..I915_MAX_PLANES {
            if (*crtc).plane_ids_mask & (1u8 << plane_id) == 0 {
                continue;
            }
            let wm = plane_wm(crtc_state, plane_id);
            if wm.is_null() {
                continue;
            }
            if plane_id == PLANE_CURSOR {
                if (*wm).wm[level].min_ddb_alloc > cursor_size {
                    impossible = true;
                    break;
                }
            } else {
                required = required.saturating_add((*wm).wm[level].min_ddb_alloc as u32);
                required = required.saturating_add((*wm).wm[level].min_ddb_alloc_uv as u32);
                if required > work_size as u32 {
                    impossible = true;
                    break;
                }
            }
        }
        if !impossible && required <= work_size as u32 {
            chosen_level = Some(level);
            break;
        }
    }
    let Some(level) = chosen_level else {
        return -tk_linux_kapi::config::EINVAL;
    };
    for plane_id in 0..I915_MAX_PLANES as usize {
        if plane_id == PLANE_CURSOR as usize || (*crtc).plane_ids_mask & (1u8 << plane_id) == 0 {
            continue;
        }
        let wm = plane_wm(crtc_state, plane_id as PlaneId);
        if wm.is_null() {
            continue;
        }
        minima[plane_id] = (*wm).wm[level]
            .min_ddb_alloc
            .saturating_add((*wm).wm[level].min_ddb_alloc_uv);
        rates[plane_id] = (*crtc_state).rel_data_rate[plane_id]
            .saturating_add((*crtc_state).rel_data_rate_y[plane_id]);
    }
    let mut plane_mask = (*crtc).plane_ids_mask;
    plane_mask &= !(1u8 << PLANE_CURSOR);
    let ranges =
        match allocate_weighted_plane_ranges(alloc.start, work_size, &minima, &rates, plane_mask) {
            Some(ranges) => ranges,
            None => return -tk_linux_kapi::config::EINVAL,
        };
    for plane_id in 0..I915_MAX_PLANES as usize {
        if plane_id == PLANE_CURSOR as usize {
            continue;
        }
        let (start, end) = ranges[plane_id];
        (*wm_skl).plane_ddb[plane_id] = SklDdbEntry { start, end };
    }

    for upper in level + 1..(*display).wm.num_levels as usize {
        for plane_id in 0..I915_MAX_PLANES as usize {
            if (*crtc).plane_ids_mask & (1u8 << plane_id) == 0 {
                continue;
            }
            let ddb = &(*wm_skl).plane_ddb[plane_id];
            let wm = plane_wm_mut(crtc_state, plane_id as PlaneId);
            if wm.is_null() {
                continue;
            }
            if (*wm).wm[upper].min_ddb_alloc > ddb_entry_size(ddb) {
                disable_wm_level(&mut (*wm).wm[upper]);
            }
            if upper > 0 && !(*wm).wm[upper].enable {
                (*wm).wm[upper].blocks = (*wm).wm[upper - 1].blocks;
                (*wm).wm[upper].lines = (*wm).wm[upper - 1].lines;
                (*wm).wm[upper].ignore_lines = (*wm).wm[upper - 1].ignore_lines;
            }
        }
    }
    for plane_id in 0..I915_MAX_PLANES as usize {
        if (*crtc).plane_ids_mask & (1u8 << plane_id) == 0 {
            continue;
        }
        let ddb = &(*wm_skl).plane_ddb[plane_id];
        let wm = plane_wm_mut(crtc_state, plane_id as PlaneId);
        if wm.is_null() {
            continue;
        }
        if (*wm).trans_wm.min_ddb_alloc > ddb_entry_size(ddb) {
            disable_wm_level(&mut (*wm).trans_wm);
        }
        if (*wm).sagv.wm0.min_ddb_alloc > ddb_entry_size(ddb) {
            disable_wm_level(&mut (*wm).sagv.wm0);
        }
        if (*wm).sagv.trans_wm.min_ddb_alloc > ddb_entry_size(ddb) {
            disable_wm_level(&mut (*wm).sagv.trans_wm);
        }
    }
    0
}

unsafe fn skl_ddb_add_affected_planes(state: *mut IntelAtomicState, crtc: *mut IntelCrtc) -> i32 {
    let display = display_from_atomic(state);
    let old = atomic_old_crtc_state(state, crtc);
    let new = atomic_new_crtc_state(state, crtc);
    if display.is_null() || old.is_null() || new.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    for plane_id in 0..I915_MAX_PLANES {
        let plane = plane_on_crtc(crtc, plane_id);
        if plane.is_null() {
            continue;
        }
        let old_skl = ptr::addr_of!((*skl_wm_state_ptr(old)));
        let new_skl = ptr::addr_of!((*skl_wm_state_ptr(new)));
        let idx = plane_id as usize;
        if ddb_entry_equal(&(*old_skl).plane_ddb[idx], &(*new_skl).plane_ddb[idx])
            && ddb_entry_equal(&(*old_skl).plane_ddb_y[idx], &(*new_skl).plane_ddb_y[idx])
        {
            continue;
        }
        if (*new).do_async_flip {
            return -tk_linux_kapi::config::EINVAL;
        }
        let plane_state = tk_drm_upstream::drm_atomic_upstream::drm_atomic_get_plane_state(
            ptr::addr_of_mut!((*state).base),
            ptr::addr_of_mut!((*plane).base),
        );
        if tk_linux_kapi::config::IS_ERR(plane_state) {
            return tk_linux_kapi::config::PTR_ERR(plane_state);
        }
        if plane_state.is_null() {
            return -tk_linux_kapi::config::ENOMEM;
        }
        (*new).update_planes |= 1u8 << plane_id;
        (*new).async_flip_planes = 0;
        (*new).do_async_flip = false;
    }
    0
}

unsafe fn skl_wm_add_affected_planes(state: *mut IntelAtomicState, crtc: *mut IntelCrtc) -> i32 {
    let display = display_from_atomic(state);
    let old = atomic_old_crtc_state(state, crtc);
    let new = atomic_new_crtc_state(state, crtc);
    if display.is_null() || old.is_null() || new.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let old_wm = ptr::addr_of!((*skl_wm_state_ptr(old)).optimal);
    let new_wm = ptr::addr_of!((*skl_wm_state_ptr(new)).optimal);
    for plane_id in 0..I915_MAX_PLANES {
        let plane = plane_on_crtc(crtc, plane_id);
        if plane.is_null() {
            continue;
        }
        if !crtc_state_needs_modeset(new)
            && plane_wm_selected_equals(display, plane_id, old_wm, new_wm)
        {
            continue;
        }
        if (*new).do_async_flip {
            return -tk_linux_kapi::config::EINVAL;
        }
        let plane_state = tk_drm_upstream::drm_atomic_upstream::drm_atomic_get_plane_state(
            ptr::addr_of_mut!((*state).base),
            ptr::addr_of_mut!((*plane).base),
        );
        if tk_linux_kapi::config::IS_ERR(plane_state) {
            return tk_linux_kapi::config::PTR_ERR(plane_state);
        }
        if plane_state.is_null() {
            return -tk_linux_kapi::config::ENOMEM;
        }
        (*new).update_planes |= 1u8 << plane_id;
        (*new).async_flip_planes = 0;
        (*new).do_async_flip = false;
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dbuf_num_enabled_slices(state: *const IntelDbufState) -> i32 {
    if state.is_null() {
        0
    } else {
        (*state).enabled_slices.count_ones() as i32
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dbuf_num_active_pipes(state: *const IntelDbufState) -> i32 {
    if state.is_null() {
        0
    } else {
        (*state).active_pipes.count_ones() as i32
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dbuf_state_set_mdclk_cdclk_ratio(
    state: *mut IntelAtomicState,
    ratio: i32,
) -> i32 {
    if ratio <= 0 || ratio > u8::MAX as i32 {
        return -tk_linux_kapi::config::EINVAL;
    }
    let dbuf = intel_atomic_get_dbuf_state(state);
    if tk_linux_kapi::config::IS_ERR(dbuf) {
        return tk_linux_kapi::config::PTR_ERR(dbuf);
    }
    if dbuf.is_null() {
        return -tk_linux_kapi::config::ENOMEM;
    }
    (*dbuf).mdclk_cdclk_ratio = ratio as u8;
    intel_atomic_lock_global_state(ptr::addr_of_mut!((*dbuf).base))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dbuf_mdclk_cdclk_ratio_update(
    display: *mut IntelDisplay,
    ratio: i32,
    joined_mbus: bool,
) {
    if display.is_null() || !has_mbus_joining(display) || ratio <= 0 {
        return;
    }
    if DISPLAY_VER(display) >= 35 {
        intel_de_rmw(
            display,
            MBUS_CTL,
            XE3P_MBUS_TRANSLATION_THROTTLE_MIN_MASK,
            XE3P_MBUS_TRANSLATION_THROTTLE_MIN((ratio - 1) as u32),
        );
    } else if DISPLAY_VER(display) >= 20 {
        intel_de_rmw(
            display,
            MBUS_CTL,
            MBUS_TRANSLATION_THROTTLE_MIN_MASK,
            MBUS_TRANSLATION_THROTTLE_MIN((ratio - 1) as u32),
        );
    }
    let service_ratio = if joined_mbus {
        ratio.saturating_mul(2)
    } else {
        ratio
    };
    let info = (*display).info.__device_info;
    if info.is_null() {
        return;
    }
    for slice in 0..4u32 {
        if (*info).dbuf.slice_mask & (1 << slice) == 0 {
            continue;
        }
        let mask = if DISPLAY_VER(display) >= 35 {
            XE3P_DBUF_MIN_TRACKER_STATE_SERVICE_MASK
        } else {
            DBUF_MIN_TRACKER_STATE_SERVICE_MASK
        };
        let val = if DISPLAY_VER(display) >= 35 {
            XE3P_DBUF_MIN_TRACKER_STATE_SERVICE((service_ratio - 1) as u32)
        } else {
            DBUF_MIN_TRACKER_STATE_SERVICE((service_ratio - 1) as u32)
        };
        intel_de_rmw(display, DBUF_CTL_S(slice), mask, val);
    }
}

unsafe fn mbus_dbox_ctl(crtc: *mut IntelCrtc, dbuf: *const IntelDbufState) -> u32 {
    let display = display_from_atomic((*dbuf).base.state.cast::<IntelAtomicState>());
    if display.is_null() || crtc.is_null() {
        return 0;
    }
    let mut val = 0;
    if DISPLAY_VER(display) >= 14 {
        val |= MBUS_DBOX_I_CREDIT(2);
    }
    if DISPLAY_VER(display) >= 12 {
        val |= MBUS_DBOX_B2B_TRANSACTIONS_MAX(16)
            | MBUS_DBOX_B2B_TRANSACTIONS_DELAY(1)
            | MBUS_DBOX_REGULATE_B2B_TRANSACTIONS_EN;
    }
    if DISPLAY_VER(display) >= 14 {
        val |= MBUS_DBOX_A_CREDIT(if (*dbuf).joined_mbus { 12 } else { 8 });
    } else if crate::intel_display_wa_upstream::__intel_display_wa(
        display,
        crate::intel_display_wa_upstream::IntelDisplayWa::Wa22010947358,
        ptr::null(),
    ) {
        val |= MBUS_DBOX_A_CREDIT(if (*dbuf).joined_mbus { 6 } else { 4 });
    } else {
        val |= MBUS_DBOX_A_CREDIT(2);
    }
    if DISPLAY_VER(display) >= 14 {
        val |= MBUS_DBOX_B_CREDIT(10);
    } else if platform_alderlake_p(display) {
        val |= MBUS_DBOX_BW_CREDIT(2) | MBUS_DBOX_B_CREDIT(8);
    } else if DISPLAY_VER(display) >= 12 {
        val |= MBUS_DBOX_BW_CREDIT(2) | MBUS_DBOX_B_CREDIT(12);
    } else {
        val |= MBUS_DBOX_BW_CREDIT(1) | MBUS_DBOX_B_CREDIT(8);
    }
    if DISPLAY_VERx100(display) == 1400 {
        let bank_mask = if (*crtc).pipe == 0 || (*crtc).pipe == 3 {
            (1 << 0) | (1 << 3)
        } else if (*crtc).pipe == 1 || (*crtc).pipe == 2 {
            (1 << 1) | (1 << 2)
        } else {
            0
        };
        if ((*dbuf).active_pipes & bank_mask).count_ones() == 1 {
            val |= MBUS_DBOX_BW_8CREDITS_MTL;
        } else {
            val |= MBUS_DBOX_BW_4CREDITS_MTL;
        }
    }
    val
}

unsafe fn pipe_mbus_dbox_ctl_update(display: *mut IntelDisplay, state: *const IntelDbufState) {
    for_each_display_crtc(display, |crtc| unsafe {
        if (*crtc).pipe >= 0
            && (*crtc).pipe < I915_MAX_PIPES
            && (*state).active_pipes & (1u8 << (*crtc).pipe) != 0
        {
            intel_de_write(
                display,
                PIPE_MBUS_DBOX_CTL((*crtc).pipe as u32),
                mbus_dbox_ctl(crtc, state),
            );
        }
    });
}

unsafe fn intel_mbus_dbox_update(state: *mut IntelAtomicState) {
    let display = display_from_atomic(state);
    let old = old_dbuf_state(state);
    let new = new_dbuf_state(state);
    if display.is_null() || old.is_null() || new.is_null() || DISPLAY_VER(display) < 11 {
        return;
    }
    if (*new).joined_mbus != (*old).joined_mbus || (*new).active_pipes != (*old).active_pipes {
        pipe_mbus_dbox_ctl_update(display, new);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dbuf_pre_plane_update(state: *mut IntelAtomicState) {
    let display = display_from_atomic(state);
    let old = old_dbuf_state(state);
    let new = new_dbuf_state(state);
    if display.is_null() || old.is_null() || new.is_null() {
        return;
    }
    let union = (*old).enabled_slices | (*new).enabled_slices;
    if union != (*old).enabled_slices {
        crate::intel_display_power_upstream::gen9_dbuf_slices_update(display, union);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dbuf_post_plane_update(state: *mut IntelAtomicState) {
    let display = display_from_atomic(state);
    let old = old_dbuf_state(state);
    let new = new_dbuf_state(state);
    if display.is_null() || old.is_null() || new.is_null() {
        return;
    }
    let union = (*old).enabled_slices | (*new).enabled_slices;
    if union != (*new).enabled_slices {
        crate::intel_display_power_upstream::gen9_dbuf_slices_update(
            display,
            (*new).enabled_slices,
        );
    }
}

unsafe fn mdclk_min_tracker_update(state: *mut IntelAtomicState) {
    let display = display_from_atomic(state);
    let old = old_dbuf_state(state);
    let new = new_dbuf_state(state);
    if display.is_null() || old.is_null() || new.is_null() {
        return;
    }
    let ratio = if crate::intel_cdclk_upstream::intel_cdclk_is_decreasing_later(state) {
        (*old).mdclk_cdclk_ratio
    } else {
        (*new).mdclk_cdclk_ratio
    };
    intel_dbuf_mdclk_cdclk_ratio_update(display, ratio as i32, (*new).joined_mbus);
}

unsafe fn crtc_state_needs_modeset(state: *const IntelCrtcState) -> bool {
    !state.is_null()
        && ((*state).uapi.mode_changed()
            || (*state).uapi.active_changed()
            || (*state).uapi.connectors_changed())
}

unsafe fn mbus_joined_pipe(state: *mut IntelAtomicState, dbuf: *const IntelDbufState) -> Pipe {
    let display = display_from_atomic(state);
    if display.is_null() || (*dbuf).active_pipes.count_ones() != 1 {
        return crate::intel_display_limits_types_upstream::INVALID_PIPE;
    }
    let pipe = (*dbuf).active_pipes.trailing_zeros() as Pipe;
    let crtc = intel_crtc_for_pipe(display, pipe);
    if crtc.is_null() {
        return crate::intel_display_limits_types_upstream::INVALID_PIPE;
    }
    let crtc_state = intel_atomic_get_crtc_state(ptr::addr_of_mut!((*state).base), crtc);
    if crtc_state.is_null()
        || tk_linux_kapi::config::IS_ERR(crtc_state)
        || crtc_state_needs_modeset(crtc_state)
    {
        crate::intel_display_limits_types_upstream::INVALID_PIPE
    } else {
        pipe
    }
}

unsafe fn mbus_ctl_join_update(
    display: *mut IntelDisplay,
    state: *const IntelDbufState,
    pipe: Pipe,
) {
    let mut val = if (*state).joined_mbus {
        MBUS_HASHING_MODE_1x4 | MBUS_JOIN
    } else {
        MBUS_HASHING_MODE_2x2
    };
    if pipe != crate::intel_display_limits_types_upstream::INVALID_PIPE {
        val |= MBUS_JOIN_PIPE_SELECT(pipe as u32);
    } else {
        val |= MBUS_JOIN_PIPE_SELECT_NONE;
    }
    intel_de_rmw(
        display,
        MBUS_CTL,
        MBUS_HASHING_MODE_MASK | MBUS_JOIN | MBUS_JOIN_PIPE_SELECT_MASK,
        val,
    );
}

unsafe fn dbuf_mbus_join_update(state: *mut IntelAtomicState, pipe: Pipe) {
    let display = display_from_atomic(state);
    let new = new_dbuf_state(state);
    if !display.is_null() && !new.is_null() {
        mbus_ctl_join_update(display, new, pipe);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dbuf_mbus_pre_ddb_update(state: *mut IntelAtomicState) {
    let old = old_dbuf_state(state);
    let new = new_dbuf_state(state);
    if old.is_null() || new.is_null() || !(!(*old).joined_mbus && (*new).joined_mbus) {
        return;
    }
    let pipe = mbus_joined_pipe(state, new);
    dbuf_mbus_join_update(state, pipe);
    intel_mbus_dbox_update(state);
    mdclk_min_tracker_update(state);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dbuf_mbus_post_ddb_update(state: *mut IntelAtomicState) {
    let display = display_from_atomic(state);
    let old = old_dbuf_state(state);
    let new = new_dbuf_state(state);
    if display.is_null() || old.is_null() || new.is_null() {
        return;
    }
    if (*old).joined_mbus && !(*new).joined_mbus {
        let pipe = mbus_joined_pipe(state, old);
        mdclk_min_tracker_update(state);
        intel_mbus_dbox_update(state);
        mbus_ctl_join_update(display, new, pipe);
        if pipe != crate::intel_display_limits_types_upstream::INVALID_PIPE {
            let crtc = intel_crtc_for_pipe(display, pipe);
            if !crtc.is_null() {
                intel_crtc_wait_for_next_vblank(crtc);
            }
        }
    } else if (*old).joined_mbus == (*new).joined_mbus && (*old).active_pipes != (*new).active_pipes
    {
        mdclk_min_tracker_update(state);
        intel_mbus_dbox_update(state);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dbuf_pmdemand_needs_update(state: *mut IntelAtomicState) -> bool {
    let display = display_from_atomic(state);
    let old = old_dbuf_state(state);
    let new = new_dbuf_state(state);
    !display.is_null()
        && !old.is_null()
        && !new.is_null()
        && ((*new).active_pipes != (*old).active_pipes
            || (DISPLAY_VER(display) < 30 && (*new).enabled_slices != (*old).enabled_slices))
}

unsafe fn wm_level_from_reg(display: *mut IntelDisplay, value: u32, level: &mut SklWmLevel) {
    level.enable = value & PLANE_WM_EN != 0;
    level.ignore_lines = value & PLANE_WM_IGNORE_LINES != 0;
    level.blocks = crate::linux::registers::REG_FIELD_GET(PLANE_WM_BLOCKS_MASK, value) as u16;
    level.lines = crate::linux::registers::REG_FIELD_GET(PLANE_WM_LINES_MASK, value) as u8;
    level.auto_min_alloc_wm_enable =
        DISPLAY_VER(display) >= 30 && value & PLANE_WM_AUTO_MIN_ALLOC_EN != 0;
}

unsafe fn pipe_wm_get_hw_state(crtc: *mut IntelCrtc, out: *mut SklPipeWm) {
    let display = display_from_crtc(crtc);
    if display.is_null() || out.is_null() {
        return;
    }
    let pipe = (*crtc).pipe as u32;
    for plane_id in 0..I915_MAX_PLANES {
        if (*crtc).plane_ids_mask & (1u8 << plane_id) == 0 {
            continue;
        }
        let wm = &mut (*out).planes[plane_id as usize];
        for level in 0..(*display).wm.num_levels as u32 {
            let val = if plane_id == PLANE_CURSOR {
                intel_de_read(display, CUR_WM(pipe, level))
            } else {
                intel_de_read(display, PLANE_WM(pipe, plane_id, level))
            };
            wm_level_from_reg(display, val, &mut wm.wm[level as usize]);
        }
        let trans = if plane_id == PLANE_CURSOR {
            intel_de_read(display, CUR_WM_TRANS(pipe))
        } else {
            intel_de_read(display, PLANE_WM_TRANS(pipe, plane_id))
        };
        wm_level_from_reg(display, trans, &mut wm.trans_wm);
        if has_hw_sagv_wm(display) {
            let sagv = if plane_id == PLANE_CURSOR {
                intel_de_read(display, CUR_WM_SAGV(pipe))
            } else {
                intel_de_read(display, PLANE_WM_SAGV(pipe, plane_id))
            };
            wm_level_from_reg(display, sagv, &mut wm.sagv.wm0);
            let trans_sagv = if plane_id == PLANE_CURSOR {
                intel_de_read(display, CUR_WM_SAGV_TRANS(pipe))
            } else {
                intel_de_read(display, PLANE_WM_SAGV_TRANS(pipe, plane_id))
            };
            wm_level_from_reg(display, trans_sagv, &mut wm.sagv.trans_wm);
        } else if has_sagv_wm(display) {
            ptr::copy_nonoverlapping(ptr::addr_of!(wm.wm[0]), ptr::addr_of_mut!(wm.sagv.wm0), 1);
            ptr::copy_nonoverlapping(
                ptr::addr_of!(wm.trans_wm),
                ptr::addr_of_mut!(wm.sagv.trans_wm),
                1,
            );
        }
    }
}

unsafe fn ddb_entry_from_hw(entry: &mut SklDdbEntry, reg: u32) {
    let start = crate::linux::registers::REG_FIELD_GET(PLANE_BUF_START_MASK, reg) as u16;
    let mut end = crate::linux::registers::REG_FIELD_GET(PLANE_BUF_END_MASK, reg) as u16;
    if end != 0 {
        end = end.saturating_add(1);
    }
    entry.start = start;
    entry.end = end;
}

unsafe fn ddb_get_hw_plane_state(
    display: *mut IntelDisplay,
    pipe: Pipe,
    plane_id: PlaneId,
    ddb: &mut SklDdbEntry,
    ddb_y: &mut SklDdbEntry,
    min_ddb: &mut u16,
    interim_ddb: &mut u16,
) {
    if plane_id == PLANE_CURSOR {
        ddb_entry_from_hw(ddb, intel_de_read(display, CUR_BUF_CFG(pipe as u32)));
        return;
    }
    ddb_entry_from_hw(
        ddb,
        intel_de_read(display, PLANE_BUF_CFG(pipe as u32, plane_id)),
    );
    if DISPLAY_VER(display) >= 30 {
        let val = intel_de_read(display, PLANE_MIN_BUF_CFG(pipe as u32, plane_id));
        *min_ddb = crate::linux::registers::REG_FIELD_GET(PLANE_MIN_DBUF_BLOCKS_MASK, val) as u16;
        *interim_ddb =
            crate::linux::registers::REG_FIELD_GET(PLANE_INTERIM_DBUF_BLOCKS_MASK, val) as u16;
    }
    if DISPLAY_VER(display) < 11 {
        ddb_entry_from_hw(
            ddb_y,
            intel_de_read(display, PLANE_NV12_BUF_CFG(pipe as u32, plane_id)),
        );
    }
}

unsafe fn ddb_entry_union(a: &mut SklDdbEntry, b: &SklDdbEntry) {
    if a.end != 0 && b.end != 0 {
        a.start = a.start.min(b.start);
        a.end = a.end.max(b.end);
    } else if b.end != 0 {
        a.start = b.start;
        a.end = b.end;
    }
}

unsafe fn pipe_ddb_get_hw_state(
    crtc: *mut IntelCrtc,
    ddb: *mut [SklDdbEntry; I915_MAX_PLANES as usize],
    ddb_y: *mut [SklDdbEntry; I915_MAX_PLANES as usize],
    min_ddb: *mut [u16; I915_MAX_PLANES as usize],
    interim_ddb: *mut [u16; I915_MAX_PLANES as usize],
) {
    let display = display_from_crtc(crtc);
    if display.is_null() {
        return;
    }
    let pipe = (*crtc).pipe;
    if pipe < 0 || pipe >= I915_MAX_PIPES {
        return;
    }
    let domain = POWER_DOMAIN_PIPE_A + pipe as u32;
    let wakeref =
        crate::intel_display_power_upstream::intel_display_power_get_if_enabled(display, domain);
    if wakeref.is_null() {
        return;
    }
    for plane_id in 0..I915_MAX_PLANES {
        if (*crtc).plane_ids_mask & (1u8 << plane_id) == 0 {
            continue;
        }
        ddb_get_hw_plane_state(
            display,
            pipe,
            plane_id,
            &mut (*ddb)[plane_id as usize],
            &mut (*ddb_y)[plane_id as usize],
            &mut (*min_ddb)[plane_id as usize],
            &mut (*interim_ddb)[plane_id as usize],
        );
    }
    crate::intel_display_power_upstream::intel_display_power_put(display, domain, wakeref);
}

unsafe fn mbus_ddb_offset(display: *mut IntelDisplay, slices: u8) -> u16 {
    let base_slice = if slices & DBUF_S1_S2 != 0 {
        1 << DBUF_S1
    } else if slices & DBUF_S3_S4 != 0 {
        1 << DBUF_S3
    } else {
        0
    };
    ddb_entry_for_slices(display_dbuf_slice_size(display).unwrap_or(0), base_slice)
        .map(|entry| entry.start)
        .unwrap_or(0)
}

fn ddb_slice_mask_for_active_pipe(pipe: Pipe, active_pipes: u8, joined_mbus: bool) -> u8 {
    if pipe < 0 || pipe >= I915_MAX_PIPES {
        return 0;
    }
    adlp_slice_mask_for(pipe as u8, active_pipes, joined_mbus)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_wm_get_hw_state(display: *mut IntelDisplay) {
    if display.is_null() || (*display).dbuf.obj.state.is_null() {
        return;
    }
    let dbuf = (*display).dbuf.obj.state.cast::<IntelDbufState>();
    if has_mbus_joining(display) {
        (*dbuf).joined_mbus = intel_de_read(display, MBUS_CTL) & MBUS_JOIN != 0;
    }
    (*dbuf).mdclk_cdclk_ratio = crate::intel_cdclk_upstream::intel_mdclk_cdclk_ratio(
        display,
        ptr::addr_of!((*display).cdclk.hw),
    )
    .max(1) as u8;
    (*dbuf).active_pipes = 0;
    for_each_display_crtc(display, |crtc| unsafe {
        let crtc_state = (*crtc).base.state.cast::<IntelCrtcState>();
        let pipe = (*crtc).pipe;
        if crtc_state.is_null() || pipe < 0 || pipe >= I915_MAX_PIPES {
            return;
        }
        let idx = pipe as usize;
        ptr::write_bytes(
            ptr::addr_of_mut!((*skl_wm_state_ptr(crtc_state)).optimal),
            0,
            1,
        );
        if (*crtc_state).hw.active {
            pipe_wm_get_hw_state(
                crtc,
                ptr::addr_of_mut!((*skl_wm_state_ptr(crtc_state)).optimal),
            );
            (*dbuf).active_pipes |= 1u8 << pipe;
        }
        ptr::copy_nonoverlapping(
            ptr::addr_of!((*skl_wm_state_ptr(crtc_state)).optimal),
            ptr::addr_of_mut!((*skl_wm_state_ptr(crtc_state)).raw),
            1,
        );
        (*dbuf).ddb[idx].start = 0;
        (*dbuf).ddb[idx].end = 0;
        if (*crtc_state).hw.active {
            for plane_id in 0..I915_MAX_PLANES {
                if (*crtc).plane_ids_mask & (1u8 << plane_id) == 0 {
                    continue;
                }
                ddb_get_hw_plane_state(
                    display,
                    pipe,
                    plane_id,
                    &mut (*skl_wm_state_ptr(crtc_state)).plane_ddb[plane_id as usize],
                    &mut (*skl_wm_state_ptr(crtc_state)).plane_ddb_y[plane_id as usize],
                    &mut (*skl_wm_state_ptr(crtc_state)).plane_min_ddb[plane_id as usize],
                    &mut (*skl_wm_state_ptr(crtc_state)).plane_interim_ddb[plane_id as usize],
                );
                ddb_entry_union(
                    &mut (*dbuf).ddb[idx],
                    &(*skl_wm_state_ptr(crtc_state)).plane_ddb[plane_id as usize],
                );
                ddb_entry_union(
                    &mut (*dbuf).ddb[idx],
                    &(*skl_wm_state_ptr(crtc_state)).plane_ddb_y[plane_id as usize],
                );
            }
        }
        (*dbuf).weight[idx] = intel_crtc_ddb_weight(crtc_state);
        let slices =
            ddb_slice_mask_for_active_pipe(pipe, (*dbuf).active_pipes, (*dbuf).joined_mbus);
        let offset = mbus_ddb_offset(display, slices);
        (*skl_wm_state_ptr(crtc_state)).ddb.start = offset.saturating_add((*dbuf).ddb[idx].start);
        (*skl_wm_state_ptr(crtc_state)).ddb.end = offset.saturating_add((*dbuf).ddb[idx].end);
        (*dbuf).slices[idx] =
            skl_ddb_dbuf_slice_mask(display, ptr::addr_of!((*skl_wm_state_ptr(crtc_state)).ddb))
                as u8;
    });
    (*dbuf).enabled_slices = (*display).dbuf.enabled_slices;
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_wm_crtc_disable_noatomic(crtc: *mut IntelCrtc) {
    let display = display_from_crtc(crtc);
    if display.is_null()
        || DISPLAY_VER(display) < 9
        || (*crtc).pipe < 0
        || (*crtc).pipe >= I915_MAX_PIPES
    {
        return;
    }
    let crtc_state = (*crtc).base.state.cast::<IntelCrtcState>();
    let dbuf = (*display).dbuf.obj.state.cast::<IntelDbufState>();
    if crtc_state.is_null() || dbuf.is_null() {
        return;
    }
    let pipe = (*crtc).pipe as usize;
    (*dbuf).active_pipes &= !(1u8 << (*crtc).pipe);
    (*dbuf).weight[pipe] = 0;
    (*dbuf).slices[pipe] = 0;
    (*dbuf).ddb[pipe].start = 0;
    (*dbuf).ddb[pipe].end = 0;
    (*skl_wm_state_ptr(crtc_state)).ddb.start = 0;
    (*skl_wm_state_ptr(crtc_state)).ddb.end = 0;
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_wm_plane_disable_noatomic(
    crtc: *mut IntelCrtc,
    plane: *mut IntelPlane,
) {
    if crtc.is_null() || plane.is_null() {
        return;
    }
    let display = display_from_crtc(crtc);
    let crtc_state = (*crtc).base.state.cast::<IntelCrtcState>();
    let plane_id = (*plane).id;
    if display.is_null()
        || DISPLAY_VER(display) < 9
        || crtc_state.is_null()
        || plane_id >= I915_MAX_PLANES
    {
        return;
    }
    let idx = plane_id as usize;
    (*skl_wm_state_ptr(crtc_state)).plane_ddb[idx].start = 0;
    (*skl_wm_state_ptr(crtc_state)).plane_ddb[idx].end = 0;
    (*skl_wm_state_ptr(crtc_state)).plane_ddb_y[idx].start = 0;
    (*skl_wm_state_ptr(crtc_state)).plane_ddb_y[idx].end = 0;
    (*skl_wm_state_ptr(crtc_state)).plane_min_ddb[idx] = 0;
    (*skl_wm_state_ptr(crtc_state)).plane_interim_ddb[idx] = 0;
    ptr::write_bytes(
        ptr::addr_of_mut!((*skl_wm_state_ptr(crtc_state)).raw.planes[idx]),
        0,
        1,
    );
    ptr::write_bytes(
        ptr::addr_of_mut!((*skl_wm_state_ptr(crtc_state)).optimal.planes[idx]),
        0,
        1,
    );
}

unsafe fn dbuf_is_misconfigured(display: *mut IntelDisplay, dbuf: *const IntelDbufState) -> bool {
    let mut entries: [(u16, u16); I915_MAX_PIPES as usize] = [(0, 0); I915_MAX_PIPES as usize];
    for_each_display_crtc(display, |crtc| unsafe {
        let pipe = (*crtc).pipe;
        let state = (*crtc).base.state.cast::<IntelCrtcState>();
        if pipe >= 0 && pipe < I915_MAX_PIPES && !state.is_null() {
            entries[pipe as usize] = (
                (*skl_wm_state_ptr(state)).ddb.start,
                (*skl_wm_state_ptr(state)).ddb.end,
            );
        }
    });
    let mut invalid = false;
    for_each_display_crtc(display, |crtc| unsafe {
        if invalid {
            return;
        }
        let pipe = (*crtc).pipe;
        if pipe < 0 || pipe >= I915_MAX_PIPES {
            invalid = true;
            return;
        }
        let state = (*crtc).base.state.cast::<IntelCrtcState>();
        if state.is_null() {
            return;
        }
        let allowed =
            ddb_slice_mask_for_active_pipe(pipe, (*dbuf).active_pipes, (*dbuf).joined_mbus);
        if (*dbuf).slices[pipe as usize] & !allowed != 0 {
            invalid = true;
            return;
        }
        let (start, end) = entries[pipe as usize];
        for other in 0..I915_MAX_PIPES as usize {
            if other == pipe as usize {
                continue;
            }
            let (other_start, other_end) = entries[other];
            if start < other_end && other_start < end {
                invalid = true;
                break;
            }
        }
    });
    invalid
}

unsafe fn mbus_sanitize(display: *mut IntelDisplay) {
    let dbuf = (*display).dbuf.obj.state.cast::<IntelDbufState>();
    if dbuf.is_null()
        || !has_mbus_joining(display)
        || !(*dbuf).joined_mbus
        || adlp_should_join_mbus((*dbuf).active_pipes)
    {
        return;
    }
    (*dbuf).joined_mbus = false;
    intel_dbuf_mdclk_cdclk_ratio_update(display, (*dbuf).mdclk_cdclk_ratio as i32, false);
    pipe_mbus_dbox_ctl_update(display, dbuf);
    mbus_ctl_join_update(
        display,
        dbuf,
        crate::intel_display_limits_types_upstream::INVALID_PIPE,
    );
}

unsafe fn dbuf_sanitize(display: *mut IntelDisplay) {
    let dbuf = (*display).dbuf.obj.state.cast::<IntelDbufState>();
    if dbuf.is_null() || !dbuf_is_misconfigured(display, dbuf) {
        return;
    }
    for_each_display_crtc(display, |crtc| unsafe {
        let state = (*crtc).base.state.cast::<IntelCrtcState>();
        if state.is_null() {
            return;
        }
        let primary = (*crtc).base.primary.cast::<IntelPlane>();
        if primary.is_null() {
            return;
        }
        let plane_state = (*primary).base.state.cast::<IntelPlaneState>();
        if !plane_state.is_null() && (*plane_state).uapi.visible {
            intel_plane_disable_noatomic(crtc, primary);
        }
        (*skl_wm_state_ptr(state)).ddb.start = 0;
        (*skl_wm_state_ptr(state)).ddb.end = 0;
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_wm_sanitize(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    mbus_sanitize(display);
    dbuf_sanitize(display);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_wm_state_verify(state: *mut IntelAtomicState, crtc: *mut IntelCrtc) {
    let display = display_from_atomic(state);
    let crtc_state = atomic_new_crtc_state(state, crtc);
    if display.is_null() || crtc.is_null() || crtc_state.is_null() || !(*crtc_state).hw.active {
        return;
    }
    let mut hw_wm: SklPipeWm = mem::zeroed();
    pipe_wm_get_hw_state(crtc, &mut hw_wm);
    let mut hw_ddb: [SklDdbEntry; I915_MAX_PLANES as usize] =
        core::array::from_fn(|_| SklDdbEntry { start: 0, end: 0 });
    let mut hw_ddb_y: [SklDdbEntry; I915_MAX_PLANES as usize] =
        core::array::from_fn(|_| SklDdbEntry { start: 0, end: 0 });
    let mut min_ddb = [0u16; I915_MAX_PLANES as usize];
    let mut interim_ddb = [0u16; I915_MAX_PLANES as usize];
    pipe_ddb_get_hw_state(
        crtc,
        &mut hw_ddb,
        &mut hw_ddb_y,
        &mut min_ddb,
        &mut interim_ddb,
    );
    let hw_enabled_slices = intel_enabled_dbuf_slices_mask(display);
    if DISPLAY_VER(display) >= 11 && hw_enabled_slices != (*display).dbuf.enabled_slices {
        drm_err!(
            &(*display).drm,
            "DBUF slices mismatch expected={} got={}\n",
            (*display).dbuf.enabled_slices,
            hw_enabled_slices
        );
    }
    let sw = ptr::addr_of!((*skl_wm_state_ptr(crtc_state)).optimal);
    for plane_id in 0..I915_MAX_PLANES {
        if (*crtc).plane_ids_mask & (1u8 << plane_id) == 0 {
            continue;
        }
        let hw_plane = &hw_wm.planes[plane_id as usize];
        let sw_plane = &(*sw).planes[plane_id as usize];
        for level in 0..(*display).wm.num_levels as usize {
            let expected = skl_plane_wm_level(sw, plane_id, level as i32);
            if !expected.is_null() && !wm_level_equals(&hw_plane.wm[level], &*expected) {
                drm_err!(
                    &(*display).drm,
                    "watermark mismatch pipe={} plane={} level={}\n",
                    (*crtc).pipe,
                    plane_id,
                    level
                );
            }
        }
        let expected_trans = skl_plane_trans_wm(sw, plane_id);
        if !expected_trans.is_null() && !wm_level_equals(&hw_plane.trans_wm, &*expected_trans) {
            drm_err!(
                &(*display).drm,
                "transition watermark mismatch pipe={} plane={}\n",
                (*crtc).pipe,
                plane_id
            );
        }
        if has_hw_sagv_wm(display)
            && (!wm_level_equals(&hw_plane.sagv.wm0, &sw_plane.sagv.wm0)
                || !wm_level_equals(&hw_plane.sagv.trans_wm, &sw_plane.sagv.trans_wm))
        {
            drm_err!(
                &(*display).drm,
                "SAGV watermark mismatch pipe={} plane={}\n",
                (*crtc).pipe,
                plane_id
            );
        }
        if !ddb_entry_equal(
            &hw_ddb[plane_id as usize],
            &(*skl_wm_state_ptr(crtc_state)).plane_ddb[plane_id as usize],
        ) {
            drm_err!(
                &(*display).drm,
                "DDB mismatch pipe={} plane={}\n",
                (*crtc).pipe,
                plane_id
            );
        }
        if !ddb_entry_equal(
            &hw_ddb_y[plane_id as usize],
            &(*skl_wm_state_ptr(crtc_state)).plane_ddb_y[plane_id as usize],
        ) {
            drm_err!(
                &(*display).drm,
                "DDB Y mismatch pipe={} plane={}\n",
                (*crtc).pipe,
                plane_id
            );
        }
    }
    let _ = (min_ddb, interim_ddb);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ddb_ranges_use_half_open_intervals() {
        let left = SklDdbEntry { start: 4, end: 12 };
        let adjacent = SklDdbEntry { start: 12, end: 20 };
        let overlap = SklDdbEntry { start: 11, end: 15 };
        let empty = SklDdbEntry { start: 8, end: 8 };

        assert!(!ddb_entries_overlap(&left, &adjacent));
        assert!(ddb_entries_overlap(&left, &overlap));
        assert!(!ddb_entries_overlap(&left, &empty));
    }

    #[test]
    fn adlp_slice_assignments_match_pipe_table() {
        assert!(adlp_should_join_mbus(PIPE_A_BIT));
        assert_eq!(adlp_slice_mask_for(0, PIPE_A_BIT, false), DBUF_S1_S2);
        assert_eq!(adlp_slice_mask_for(1, PIPE_B_BIT, false), DBUF_S3_S4);
        assert_eq!(
            adlp_slice_mask_for(0, PIPE_A_BIT | PIPE_B_BIT, false),
            DBUF_S1_S2,
        );
        assert_eq!(
            adlp_slice_mask_for(1, PIPE_A_BIT | PIPE_B_BIT, false),
            DBUF_S3_S4,
        );
        assert_eq!(adlp_slice_mask_for(2, PIPE_C_BIT, false), DBUF_S3_S4);
        assert_eq!(adlp_slice_mask_for(3, PIPE_D_BIT, false), DBUF_S1_S2);
        assert!(!adlp_should_join_mbus(PIPE_A_BIT | PIPE_B_BIT));
    }

    #[test]
    fn weighted_ddb_allocation_preserves_minima_and_exact_range() {
        let mut minima = [0u16; I915_MAX_PLANES as usize];
        let mut rates = [0u64; I915_MAX_PLANES as usize];
        let first = crate::intel_display_limits_types_upstream::PLANE_PRIMARY as usize;
        let second = 1usize;
        minima[first] = 4;
        minima[second] = 6;
        rates[first] = 1;
        rates[second] = 3;
        let mask = (1u8 << first) | (1u8 << second);
        let ranges = allocate_weighted_plane_ranges(100, 31, &minima, &rates, mask).unwrap();

        assert_eq!(ranges[first], (100, 110));
        assert_eq!(ranges[second], (110, 131));
        assert!(allocate_weighted_plane_ranges(100, 9, &minima, &rates, mask).is_none());
    }
}
