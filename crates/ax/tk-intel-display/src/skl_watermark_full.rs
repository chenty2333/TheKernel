// SPDX-License-Identifier: MIT
// Copyright © 2022 Intel Corporation.
// Faithful algorithmic translation of Linux 7.2.3 drivers/gpu/drm/i915/display/skl_watermark.c.
// DRM/atomic/debugfs/register interfaces are represented by WatermarkFramework.

use core::cmp::{max, min};

pub const WM_LEVELS: usize = 8;
pub const PLANES: usize = 6;
pub const U16_MAX: u16 = u16::MAX;
pub const FP_ONE: u32 = 1 << 16;
pub const FP_MAX: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SagvStatus {
    #[default]
    Unknown,
    Enabled,
    Disabled,
    NotControlled,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DisplayCaps {
    pub display_ver: u8,
    pub display_ver_fixed: u8,
    pub dg2: bool,
    pub skylake: bool,
    pub kabylake: bool,
    pub coffeelake: bool,
    pub cometlake: bool,
    pub sagv: bool,
    pub sagv_wm: bool,
    pub params_enable_sagv: bool,
    pub has_vrr: bool,
    pub has_4tile: bool,
    pub has_16gb_dimms: bool,
    pub alderlake_p: bool,
    pub wa_22010947358: bool,
    pub display_verx100: u16,
    pub has_mbus_joining: bool,
    pub has_hw_sagv_wm: bool,
}
impl DisplayCaps {
    pub fn ver(&self) -> u8 {
        self.display_ver
    }
    pub fn is_ver(&self, lo: u8, hi: u8) -> bool {
        self.display_ver >= lo && self.display_ver <= hi
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SagvInfo {
    pub status: SagvStatus,
    pub block_time_us: u32,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DbufState {
    pub ddb: [DdbEntry; 4],
    pub weight: [u32; 4],
    pub slices: [u8; 4],
    pub enabled_slices: u8,
    pub active_pipes: u8,
    pub mdclk_cdclk_ratio: u8,
    pub joined_mbus: bool,
    pub changed: bool,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DdbEntry {
    pub start: u16,
    pub end: u16,
}
impl DdbEntry {
    pub fn size(self) -> u16 {
        self.end.saturating_sub(self.start)
    }
    pub fn empty(self) -> bool {
        self.start == self.end
    }
    pub fn equal(self, other: Self) -> bool {
        self.start == other.start && self.end == other.end
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WmLevel {
    pub enable: bool,
    pub ignore_lines: bool,
    pub blocks: u32,
    pub lines: u32,
    pub min_ddb_alloc: u16,
    pub min_ddb_alloc_uv: u16,
    pub can_sagv: bool,
    pub auto_min_alloc_wm_enable: bool,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlaneWm {
    pub levels: [WmLevel; WM_LEVELS],
    pub trans_wm: WmLevel,
    pub sagv_wm0: WmLevel,
    pub sagv_trans_wm: WmLevel,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PipeWm {
    pub planes: [PlaneWm; PLANES],
    pub use_sagv_wm: bool,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WatermarkParams {
    pub x_tiled: bool,
    pub y_tiled: bool,
    pub rc_surface: bool,
    pub width: u32,
    pub cpp: u8,
    pub plane_pixel_rate: u32,
    pub y_min_scanlines: u32,
    pub plane_bytes_per_line: u32,
    pub plane_blocks_per_line: u32,
    pub y_tile_minimum: u32,
    pub linetime_us: u32,
    pub pipe_htotal: u32,
    pub dbuf_block_size: u32,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ModifierInfo {
    pub x_tiled: bool,
    pub tiled: bool,
    pub yf_tiled: bool,
    pub rc_surface: bool,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlaneWmInput {
    pub width: u32,
    pub cpp: u8,
    pub cpp_uv: u8,
    pub modifier: ModifierInfo,
    pub rotation_90_or_270: bool,
    pub pixel_rate: u32,
    pub pipe_htotal: u32,
    pub pan_x: u32,
    pub semiplanar: bool,
    pub yuv: bool,
    pub num_format_planes: u8,
    pub visible: bool,
    pub is_y_plane: bool,
    pub has_planar_linked_plane: bool,
    pub crtc_async_flip: bool,
    pub plane_async_flip: bool,
}

/// Boundary for DRM/atomic/debugfs/hardware services used by the upstream file.
/// Implementations supply behavior; policy and watermark arithmetic remain here.
pub trait WatermarkFramework {
    fn read32(&self, register: u32) -> u32 {
        let _ = register;
        0
    }
    fn read_sagv_latency_qclk(&self) -> u32 {
        0
    }
    fn pcode_read_sagv_block_time(&self) -> Result<u32, i32> {
        Err(-1)
    }
    fn pcode_write(&self, command: u32, value: u32) -> Result<(), i32> {
        let _ = (command, value);
        Err(-1)
    }
    fn pcode_request(
        &self,
        command: u32,
        request: u32,
        mask: u32,
        reply: u32,
        timeout: u32,
    ) -> Result<(), i32> {
        let _ = (command, request, mask, reply, timeout);
        Err(-1)
    }
    fn ipc_enabled(&self) -> bool {
        false
    }
    fn set_ipc_enabled(&self, _enabled: bool) {}
    fn log(&self, _message: &'static str, _value: u32) {}
    fn drm_warn(&self, _condition: bool) {}
}
pub trait DdbAtomicFramework {
    fn lock_global_state(&mut self) -> Result<(), i32>;
    fn set_crtc_absolute_ddb(&mut self, pipe: usize, ddb: DdbEntry) -> Result<(), i32>;
}
pub trait AtomicDdbFramework: DdbAtomicFramework + PlaneDdbFramework {
    fn serialize_global_state(&mut self) -> Result<(), i32>;
    fn set_joined_mbus(&mut self, joined: bool) -> Result<(), i32>;
    fn add_affected_planes(
        &mut self,
        pipe: usize,
        old: &PlaneDdbModel,
        new: &mut PlaneDdbModel,
    ) -> Result<(), i32>;
    fn add_wm_affected_plane(&mut self, _pipe: usize, _plane: usize) -> Result<(), i32> {
        Ok(())
    }
}
pub trait DbufRegisterIo {
    fn mbus_dbox_i_credit(&self, n: u32) -> u32;
    fn mbus_dbox_b2b_max(&self, n: u32) -> u32;
    fn mbus_dbox_b2b_delay(&self, n: u32) -> u32;
    fn mbus_dbox_regulate_b2b(&self) -> u32;
    fn mbus_dbox_a_credit(&self, n: u32) -> u32;
    fn mbus_dbox_b_credit(&self, n: u32) -> u32;
    fn mbus_dbox_bw_credit(&self, n: u32) -> u32;
    fn mbus_dbox_bw_8credits_mtl(&self) -> u32;
    fn mbus_dbox_bw_4credits_mtl(&self) -> u32;
    fn write_pipe_mbus_dbox_ctl(&self, pipe: usize, value: u32);
    fn update_mbus_ctl(&self, joined: bool, pipe: Option<usize>);
    fn update_mbus_translation_throttle(&self, ratio_minus_one: u32, xe3: bool);
    fn update_dbuf_min_tracker(&self, slice: usize, ratio_minus_one: u32, xe3: bool);
    fn update_dbuf_slices(&self, enabled: u8);
    fn wait_next_vblank(&self, pipe: usize);
    fn available_dbuf_slices(&self) -> u8 {
        S1 | S2 | S3 | S4
    }
}
pub trait PlaneDdbFramework {
    fn cursor_allocation(&self, active_pipes: u8) -> u16;
    fn warn(&self, condition: bool);
}
pub trait PrefillFramework: WatermarkFramework {
    fn init_prefill(&self);
    fn vblank_too_short(&self, latency_us: u32) -> bool;
}
pub trait DbufSanitizeIo {
    fn disable_visible_planes(&mut self);
    fn active_planes(&self) -> u32;
    fn warn(&self, condition: bool);
}
pub trait SagvCommitIo: WatermarkFramework {
    fn has_new_bw_state(&self) -> bool;
    fn bw_can_enable_sagv(&self) -> bool;
    fn icl_pre_plane_update(&mut self);
    fn icl_post_plane_update(&mut self);
}

// upstream: skl_watermark.c intel_enabled_dbuf_slices_mask()
pub fn intel_enabled_dbuf_slices_mask(io: &impl WmRegisterIo, num_slices: usize) -> u8 {
    let mut mask = 0;
    for slice in 0..num_slices.min(8) {
        if io.dbuf_powered(slice) {
            mask |= 1 << slice;
        }
    }
    mask
}
// upstream: skl_watermark.c skl_needs_memory_bw_wa()
pub fn skl_needs_memory_bw_wa(display: &DisplayCaps) -> bool {
    display.display_ver == 9
}
// upstream: skl_watermark.c intel_has_sagv()
pub fn intel_has_sagv(display: &DisplayCaps, sagv: &SagvInfo) -> bool {
    display.sagv && sagv.status != SagvStatus::NotControlled
}
// upstream: skl_watermark.c intel_sagv_block_time()
pub fn intel_sagv_block_time(io: &impl WatermarkFramework, display: &DisplayCaps) -> u32 {
    if display.display_ver >= 14 {
        io.read_sagv_latency_qclk()
    } else if display.display_ver >= 12 {
        match io.pcode_read_sagv_block_time() {
            Ok(v) => v,
            Err(_) => {
                io.log("Couldn't read SAGV block time!", 0);
                0
            }
        }
    } else if display.display_ver == 11 {
        10
    } else if display.sagv {
        30
    } else {
        0
    }
}
// upstream: skl_watermark.c intel_sagv_init()
pub fn intel_sagv_init(io: &impl WatermarkFramework, display: &DisplayCaps, sagv: &mut SagvInfo) {
    if !display.sagv {
        sagv.status = SagvStatus::NotControlled;
    }
    if display.display_ver < 11 {
        skl_sagv_disable(io, display, sagv);
    }
    io.drm_warn(sagv.status == SagvStatus::Unknown);
    sagv.block_time_us = intel_sagv_block_time(io, display);
    if sagv.block_time_us > u16::MAX as u32 {
        io.log("Excessive SAGV block time, ignoring", sagv.block_time_us);
        sagv.block_time_us = 0;
    }
    if !intel_has_sagv(display, sagv) {
        sagv.block_time_us = 0;
    }
}
// upstream: skl_watermark.c skl_sagv_enable()
pub fn skl_sagv_enable(io: &impl WatermarkFramework, display: &DisplayCaps, sagv: &mut SagvInfo) {
    if !intel_has_sagv(display, sagv) || sagv.status == SagvStatus::Enabled {
        return;
    }
    match io.pcode_write(0x21, 3) {
        Ok(()) => sagv.status = SagvStatus::Enabled,
        Err(-6) if display.skylake => {
            io.log("No SAGV found on system, ignoring", 0);
            sagv.status = SagvStatus::NotControlled;
        }
        Err(_) => io.log("Failed to enable SAGV", 0),
    }
}
// upstream: skl_watermark.c skl_sagv_disable()
pub fn skl_sagv_disable(io: &impl WatermarkFramework, display: &DisplayCaps, sagv: &mut SagvInfo) {
    if !intel_has_sagv(display, sagv) || sagv.status == SagvStatus::Disabled {
        return;
    }
    match io.pcode_request(0x21, 0, 1, 1, 1) {
        Ok(()) => sagv.status = SagvStatus::Disabled,
        Err(-6) if display.skylake => {
            io.log("No SAGV found on system, ignoring", 0);
            sagv.status = SagvStatus::NotControlled;
        }
        Err(e) => io.log("Failed to disable SAGV", e.unsigned_abs()),
    }
}
// upstream: skl_watermark.c skl_sagv_pre_plane_update()
pub fn skl_sagv_pre_plane_update(
    io: &mut impl SagvCommitIo,
    display: &DisplayCaps,
    sagv: &mut SagvInfo,
) {
    if !io.has_new_bw_state() {
        return;
    }
    if !io.bw_can_enable_sagv() {
        skl_sagv_disable(io, display, sagv);
    }
}
// upstream: skl_watermark.c skl_sagv_post_plane_update()
pub fn skl_sagv_post_plane_update(
    io: &mut impl SagvCommitIo,
    display: &DisplayCaps,
    sagv: &mut SagvInfo,
) {
    if !io.has_new_bw_state() {
        return;
    }
    if io.bw_can_enable_sagv() {
        skl_sagv_enable(io, display, sagv);
    }
}
// upstream: skl_watermark.c intel_sagv_pre_plane_update()
pub fn intel_sagv_pre_plane_update(
    io: &mut impl SagvCommitIo,
    display: &DisplayCaps,
    sagv: &mut SagvInfo,
) {
    if !intel_has_sagv(display, sagv) || !io.has_new_bw_state() {
        return;
    }
    if display.display_ver >= 11 {
        io.icl_pre_plane_update();
    } else if !io.bw_can_enable_sagv() {
        skl_sagv_disable(io, display, sagv);
    }
}
// upstream: skl_watermark.c intel_sagv_post_plane_update()
pub fn intel_sagv_post_plane_update(
    io: &mut impl SagvCommitIo,
    display: &DisplayCaps,
    sagv: &mut SagvInfo,
) {
    if !intel_has_sagv(display, sagv) || !io.has_new_bw_state() {
        return;
    }
    if display.display_ver >= 11 {
        io.icl_post_plane_update();
    } else if io.bw_can_enable_sagv() {
        skl_sagv_enable(io, display, sagv);
    }
}

#[inline]
fn div_round_up(n: u32, d: u32) -> u32 {
    if d == 0 {
        u32::MAX
    } else {
        n / d + u32::from(n % d != 0)
    }
}
#[inline]
fn fp_from_u32(n: u32) -> u32 {
    n.saturating_mul(FP_ONE)
}
#[inline]
fn fp_div_u32(n: u32, d: u32) -> u32 {
    if d == 0 {
        FP_MAX
    } else {
        ((n as u64) << 16)
            .checked_div(d as u64)
            .unwrap_or(u64::MAX)
            .min(u32::MAX as u64) as u32
    }
}
#[inline]
fn fp_mul_u32(n: u32, f: u32) -> u32 {
    ((n as u64 * f as u64) >> 16).min(u32::MAX as u64) as u32
}
#[inline]
fn fp_round_up(v: u32) -> u32 {
    (v >> 16) + u32::from(v & 0xffff != 0)
}
#[inline]
fn fp_div_round_up(a: u32, b: u32) -> u32 {
    if b == 0 {
        u32::MAX
    } else {
        let q = a / b;
        q + u32::from(a % b != 0)
    }
}
#[inline]
fn fp_mul_round_up_u32(n: u32, f: u32) -> u32 {
    let v = n as u64 * f as u64;
    ((v + 0xffff) >> 16).min(u32::MAX as u64) as u32
}

// upstream: skl_watermark.c skl_crtc_can_enable_sagv()
pub fn skl_crtc_can_enable_sagv(plane_wms: &[PlaneWm; PLANES], num_levels: usize) -> bool {
    let mut max_level = None;
    for p in plane_wms {
        if !p.levels[0].enable {
            continue;
        }
        let level = (0..num_levels.min(WM_LEVELS))
            .rev()
            .find(|&l| p.levels[l].enable)
            .unwrap_or(0);
        max_level = Some(max_level.map_or(level, |old: usize| min(old, level)));
    }
    let Some(level) = max_level else {
        return true;
    };
    plane_wms
        .iter()
        .all(|p| !p.levels[0].enable || p.levels[level].can_sagv)
}
// upstream: skl_watermark.c tgl_crtc_can_enable_sagv()
pub fn tgl_crtc_can_enable_sagv(plane_wms: &[PlaneWm; PLANES]) -> bool {
    plane_wms
        .iter()
        .all(|p| !p.levels[0].enable || p.sagv_wm0.enable)
}
// upstream: skl_watermark.c intel_crtc_can_enable_sagv()
pub fn intel_crtc_can_enable_sagv(
    display: &DisplayCaps,
    sagv: &SagvInfo,
    active: bool,
    interlaced: bool,
    inherited: bool,
    plane_wms: &[PlaneWm; PLANES],
    num_levels: usize,
) -> bool {
    if sagv.block_time_us == 0 || !display.params_enable_sagv || inherited {
        return false;
    }
    if !active {
        return true;
    }
    if interlaced {
        return false;
    }
    if display.sagv_wm {
        return tgl_crtc_can_enable_sagv(plane_wms);
    }
    skl_crtc_can_enable_sagv(plane_wms, num_levels)
}
// upstream: skl_watermark.c skl_ddb_entry_init()
pub fn skl_ddb_entry_init(entry: &mut DdbEntry, start: u16, end: u16) -> u16 {
    entry.start = start;
    entry.end = end;
    end
}
// upstream: skl_watermark.c intel_dbuf_slice_size()
pub fn intel_dbuf_slice_size(total_size: u16, display_slice_mask: u8) -> u16 {
    total_size / (display_slice_mask.count_ones() as u16)
}
// upstream: skl_watermark.c skl_ddb_entry_for_slices()
pub fn skl_ddb_entry_for_slices(slice_mask: u8, slice_size: u16, entry: &mut DdbEntry) {
    if slice_mask == 0 {
        entry.start = 0;
        entry.end = 0;
        return;
    }
    entry.start = (slice_mask.trailing_zeros() as u16) * slice_size;
    entry.end = (8 - slice_mask.leading_zeros() as u16) * slice_size;
}
// upstream: skl_watermark.c mbus_ddb_offset()
pub fn mbus_ddb_offset(slice_mask: u8, slice_size: u16) -> u16 {
    let normalized = if slice_mask & ((1 << 1) | (1 << 2)) != 0 {
        1 << 1
    } else if slice_mask & ((1 << 3) | (1 << 4)) != 0 {
        1 << 3
    } else {
        slice_mask
    };
    let mut ddb = DdbEntry::default();
    skl_ddb_entry_for_slices(normalized, slice_size, &mut ddb);
    ddb.start
}
// upstream: skl_watermark.c skl_ddb_dbuf_slice_mask()
pub fn skl_ddb_dbuf_slice_mask(entry: DdbEntry, slice_size: u16) -> u32 {
    if entry.size() == 0 || slice_size == 0 {
        return 0;
    }
    let mut start = entry.start / slice_size;
    let end = (entry.end - 1) / slice_size;
    let mut mask = 0;
    while start <= end {
        mask |= 1u32 << start;
        start += 1;
    }
    mask
}
// upstream: skl_watermark.c intel_crtc_ddb_weight()
pub fn intel_crtc_ddb_weight(active: bool, hdisplay: u32) -> u32 {
    if active { hdisplay } else { 0 }
}
// upstream: skl_watermark.c intel_crtc_dbuf_weights()
pub fn intel_crtc_dbuf_weights(state: &DbufState, for_pipe: usize) -> (u32, u32, u32) {
    let (mut start, mut end, mut total) = (0, 0, 0);
    for pipe in 0..state.weight.len() {
        let weight = state.weight[pipe];
        if state.slices[pipe] != state.slices[for_pipe] {
            continue;
        }
        total += weight;
        if pipe < for_pipe {
            start += weight;
            end += weight;
        } else if pipe == for_pipe {
            end += weight;
        }
    }
    (start, end, total)
}
// upstream: skl_watermark.c skl_crtc_allocate_ddb()
pub fn skl_crtc_allocate_ddb(
    io: &mut impl DdbAtomicFramework,
    old: &DbufState,
    new: &mut DbufState,
    pipe: usize,
    total_size: u16,
    display_slice_mask: u8,
) -> Result<(), i32> {
    let mut mbus_offset = 0u16;
    if new.weight[pipe] == 0 {
        skl_ddb_entry_init(&mut new.ddb[pipe], 0, 0);
    } else {
        let slice_mask = new.slices[pipe];
        let slice_size = intel_dbuf_slice_size(total_size, display_slice_mask);
        let mut slices = DdbEntry::default();
        skl_ddb_entry_for_slices(slice_mask, slice_size, &mut slices);
        mbus_offset = mbus_ddb_offset(slice_mask, slice_size);
        let ddb_range_size = slices.size() as u32;
        let (ws, we, wt) = intel_crtc_dbuf_weights(new, pipe);
        let start = ddb_range_size * ws / wt;
        let end = ddb_range_size * we / wt;
        skl_ddb_entry_init(
            &mut new.ddb[pipe],
            slices.start - mbus_offset as u16 + start as u16,
            slices.start - mbus_offset as u16 + end as u16,
        );
    }
    if old.slices[pipe] == new.slices[pipe] && old.ddb[pipe].equal(new.ddb[pipe]) {
        return Ok(());
    }
    io.lock_global_state()?;
    io.set_crtc_absolute_ddb(
        pipe,
        DdbEntry {
            start: mbus_offset + new.ddb[pipe].start,
            end: mbus_offset + new.ddb[pipe].end,
        },
    )
}
// upstream: skl_watermark.c skl_wm_latency()
pub fn skl_wm_latency(
    display: &DisplayCaps,
    wm_latency: &[u32; WM_LEVELS],
    level: usize,
    wp: Option<&WatermarkParams>,
    ipc: bool,
) -> u32 {
    let mut latency = wm_latency[level];
    if latency == 0 {
        return 0;
    }
    if (display.kabylake || display.coffeelake || display.cometlake) && ipc {
        latency += 4;
    }
    if skl_needs_memory_bw_wa(display) && wp.is_some_and(|p| p.x_tiled) {
        latency += 15;
    }
    latency
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlaneWmSelection {
    pub wm: WmLevel,
}
// upstream: skl_watermark.c skl_cursor_allocation()
pub fn skl_cursor_allocation(
    display: &DisplayCaps,
    cursor_width: u32,
    pixel_rate: u32,
    pipe_htotal: u32,
    num_active: u32,
    latencies: &[u32; WM_LEVELS],
    num_levels: usize,
) -> u16 {
    let modifier = ModifierInfo::default();
    let wp = match skl_compute_wm_params(
        display,
        cursor_width,
        4,
        modifier,
        false,
        pixel_rate,
        pipe_htotal,
        0,
        0,
        false,
    ) {
        Ok(v) => v,
        Err(_) => return if num_active == 1 { 32 } else { 8 },
    };
    let mut wm = WmLevel::default();
    let mut min_ddb_alloc = 0u16;
    for level in 0..num_levels.min(WM_LEVELS) {
        let latency = skl_wm_latency(display, latencies, level, Some(&wp), false);
        wm = skl_compute_plane_wm(display, 5, level, latency, &wp, wm, false, 0);
        if wm.min_ddb_alloc == U16_MAX {
            break;
        }
        min_ddb_alloc = wm.min_ddb_alloc;
    }
    max(if num_active == 1 { 32 } else { 8 }, min_ddb_alloc)
}
// upstream: skl_watermark.c skl_ddb_entry_init_from_hw()
pub fn skl_ddb_entry_init_from_hw(
    entry: &mut DdbEntry,
    reg: u32,
    start_mask: u32,
    end_mask: u32,
    end_shift: u8,
) {
    skl_ddb_entry_init(
        entry,
        (reg & start_mask) as u16,
        ((reg & end_mask) >> end_shift) as u16,
    );
    if entry.end != 0 {
        entry.end += 1;
    }
}
// upstream: skl_watermark.c skl_ddb_get_hw_plane_state()
pub fn skl_ddb_get_hw_plane_state(
    io: &impl WmRegisterIo,
    display: &DisplayCaps,
    pipe: usize,
    plane: usize,
) -> (DdbEntry, DdbEntry, u16, u16) {
    let mut ddb = DdbEntry::default();
    let ddb_y = DdbEntry::default();
    let mut min_ddb = 0;
    let mut interim = 0;
    let raw = if plane == 5 {
        io.cursor_ddb(pipe)
    } else {
        io.plane_ddb(pipe, plane)
    };
    skl_ddb_entry_init_from_hw(&mut ddb, raw, 0x1fff, 0x1fff_0000, 16);
    if plane != 5 && display.display_ver >= 30 {
        let val = io.plane_min_ddb(pipe, plane);
        min_ddb = ((val >> 16) & 0x1fff) as u16;
        interim = (val & 0x1fff) as u16;
    }
    (ddb, ddb_y, min_ddb, interim)
}
// upstream: skl_watermark.c skl_pipe_ddb_get_hw_state()
pub fn skl_pipe_ddb_get_hw_state(
    io: &impl WmRegisterIo,
    display: &DisplayCaps,
    pipe: usize,
) -> (
    [DdbEntry; PLANES],
    [DdbEntry; PLANES],
    [u16; PLANES],
    [u16; PLANES],
) {
    let (mut ddb, mut ddb_y, mut min_ddb, mut interim) = (
        [DdbEntry::default(); PLANES],
        [DdbEntry::default(); PLANES],
        [0; PLANES],
        [0; PLANES],
    );
    for plane in 0..PLANES {
        (ddb[plane], ddb_y[plane], min_ddb[plane], interim[plane]) =
            skl_ddb_get_hw_plane_state(io, display, pipe, plane);
    }
    (ddb, ddb_y, min_ddb, interim)
}
// upstream: skl_watermark.c check_mbus_joined()
pub fn check_mbus_joined(active_pipes: u8, configs: &[DbufSliceConfig]) -> bool {
    configs
        .iter()
        .take_while(|c| c.active_pipes != 0)
        .find(|c| c.active_pipes == active_pipes)
        .is_some_and(|c| c.join_mbus)
}
// upstream: skl_watermark.c adlp_check_mbus_joined()
pub fn adlp_check_mbus_joined(active_pipes: u8) -> bool {
    check_mbus_joined(active_pipes, ADLP_ALLOWED_DBUFS)
}
// upstream: skl_watermark.c compute_dbuf_slices()
pub fn compute_dbuf_slices(
    pipe: usize,
    active_pipes: u8,
    join_mbus: bool,
    configs: &[DbufSliceConfig],
) -> u8 {
    configs
        .iter()
        .take_while(|c| c.active_pipes != 0)
        .find(|c| c.active_pipes == active_pipes && c.join_mbus == join_mbus)
        .map_or(0, |c| c.dbuf_mask[pipe])
}
// upstream: skl_watermark.c icl_compute_dbuf_slices()
pub fn icl_compute_dbuf_slices(pipe: usize, active_pipes: u8, join_mbus: bool) -> u8 {
    compute_dbuf_slices(pipe, active_pipes, join_mbus, ICL_ALLOWED_DBUFS)
}
// upstream: skl_watermark.c tgl_compute_dbuf_slices()
pub fn tgl_compute_dbuf_slices(pipe: usize, active_pipes: u8, join_mbus: bool) -> u8 {
    compute_dbuf_slices(pipe, active_pipes, join_mbus, TGL_ALLOWED_DBUFS)
}
// upstream: skl_watermark.c adlp_compute_dbuf_slices()
pub fn adlp_compute_dbuf_slices(pipe: usize, active_pipes: u8, join_mbus: bool) -> u8 {
    compute_dbuf_slices(pipe, active_pipes, join_mbus, ADLP_ALLOWED_DBUFS)
}
// upstream: skl_watermark.c dg2_compute_dbuf_slices()
pub fn dg2_compute_dbuf_slices(pipe: usize, active_pipes: u8, join_mbus: bool) -> u8 {
    compute_dbuf_slices(pipe, active_pipes, join_mbus, DG2_ALLOWED_DBUFS)
}
// upstream: skl_watermark.c skl_compute_dbuf_slices()
pub fn skl_compute_dbuf_slices(
    display: &DisplayCaps,
    pipe: usize,
    active_pipes: u8,
    join_mbus: bool,
) -> u8 {
    if display.dg2 {
        dg2_compute_dbuf_slices(pipe, active_pipes, join_mbus)
    } else if display.display_ver >= 13 {
        adlp_compute_dbuf_slices(pipe, active_pipes, join_mbus)
    } else if display.display_ver == 12 {
        tgl_compute_dbuf_slices(pipe, active_pipes, join_mbus)
    } else if display.display_ver == 11 {
        icl_compute_dbuf_slices(pipe, active_pipes, join_mbus)
    } else if active_pipes & (1 << pipe) != 0 {
        1 << 1
    } else {
        0
    }
}
// upstream: skl_watermark.c use_minimal_wm0_only()
pub fn use_minimal_wm0_only(display: &DisplayCaps, async_crtc: bool, async_plane: bool) -> bool {
    display.is_ver(13, 20) && async_crtc && async_plane
}
// upstream: skl_watermark.c skl_plane_relative_data_rate()
pub fn skl_plane_relative_data_rate(
    display: &DisplayCaps,
    async_crtc: bool,
    async_plane: bool,
    width: u32,
    height: u32,
    cpp: u32,
) -> u32 {
    if use_minimal_wm0_only(display, async_crtc, async_plane) {
        0
    } else {
        width.wrapping_mul(height).wrapping_mul(cpp)
    }
}
// upstream: skl_watermark.c skl_total_relative_data_rate()
pub fn skl_total_relative_data_rate(
    data_rates: &[u64; PLANES],
    data_rates_y: &[u64; PLANES],
    display_ver: u8,
) -> u64 {
    let mut total = 0u64;
    for i in 0..PLANES - 1 {
        total += data_rates[i];
        if display_ver < 11 {
            total += data_rates_y[i];
        }
    }
    total
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlaneDdbIter {
    pub data_rate: u64,
    pub start: u16,
    pub size: u16,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaneDdbModel {
    pub active: bool,
    pub nv12_planes: u8,
    pub active_pipes: u8,
    pub num_levels: usize,
    pub rel_data_rate: [u64; PLANES],
    pub rel_data_rate_y: [u64; PLANES],
    pub raw_planes: [PlaneWm; PLANES],
    pub planes: [PlaneWm; PLANES],
    pub plane_ddb: [DdbEntry; PLANES],
    pub plane_ddb_y: [DdbEntry; PLANES],
    pub plane_min_ddb: [u16; PLANES],
    pub plane_interim_ddb: [u16; PLANES],
}
impl Default for PlaneDdbModel {
    fn default() -> Self {
        Self {
            active: false,
            nv12_planes: 0,
            active_pipes: 0,
            num_levels: WM_LEVELS,
            rel_data_rate: [0; PLANES],
            rel_data_rate_y: [0; PLANES],
            raw_planes: [PlaneWm::default(); PLANES],
            planes: [PlaneWm::default(); PLANES],
            plane_ddb: [DdbEntry::default(); PLANES],
            plane_ddb_y: [DdbEntry::default(); PLANES],
            plane_min_ddb: [0; PLANES],
            plane_interim_ddb: [0; PLANES],
        }
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DbufSliceConfig {
    pub active_pipes: u8,
    pub join_mbus: bool,
    pub dbuf_mask: [u8; 4],
}
const fn dbuf(active: u8, join: bool, masks: [u8; 4]) -> DbufSliceConfig {
    DbufSliceConfig {
        active_pipes: active,
        join_mbus: join,
        dbuf_mask: masks,
    }
}
const S1: u8 = 1 << 1;
const S2: u8 = 1 << 2;
const S3: u8 = 1 << 3;
const S4: u8 = 1 << 4;
const A: u8 = 1 << 0;
const B: u8 = 1 << 1;
const C: u8 = 1 << 2;
const D: u8 = 1 << 3;
// BSpec 12716 / 49255; order, including the ADLP joined-MBUS priority, is significant.
pub const ICL_ALLOWED_DBUFS: &[DbufSliceConfig] = &[
    dbuf(A, false, [S1, 0, 0, 0]),
    dbuf(B, false, [0, S1, 0, 0]),
    dbuf(A | B, false, [S1, S2, 0, 0]),
    dbuf(C, false, [0, 0, S2, 0]),
    dbuf(A | C, false, [S1, 0, S2, 0]),
    dbuf(B | C, false, [0, S1, S2, 0]),
    dbuf(A | B | C, false, [S1, S1, S2, 0]),
];
pub const TGL_ALLOWED_DBUFS: &[DbufSliceConfig] = &[
    dbuf(A, false, [S1 | S2, 0, 0, 0]),
    dbuf(B, false, [0, S1 | S2, 0, 0]),
    dbuf(A | B, false, [S2, S1, 0, 0]),
    dbuf(C, false, [0, 0, S1 | S2, 0]),
    dbuf(A | C, false, [S1, 0, S2, 0]),
    dbuf(B | C, false, [0, S1, S2, 0]),
    dbuf(A | B | C, false, [S1, S1, S2, 0]),
    dbuf(D, false, [0, 0, 0, S1 | S2]),
    dbuf(A | D, false, [S1, 0, 0, S2]),
    dbuf(B | D, false, [0, S1, 0, S2]),
    dbuf(A | B | D, false, [S1, S1, 0, S2]),
    dbuf(C | D, false, [0, 0, S1, S2]),
    dbuf(A | C | D, false, [S1, 0, S2, S2]),
    dbuf(B | C | D, false, [0, S1, S2, S2]),
    dbuf(A | B | C | D, false, [S1, S1, S2, S2]),
];
pub const DG2_ALLOWED_DBUFS: &[DbufSliceConfig] = &[
    dbuf(A, false, [S1 | S2, 0, 0, 0]),
    dbuf(B, false, [0, S1 | S2, 0, 0]),
    dbuf(A | B, false, [S1, S2, 0, 0]),
    dbuf(C, false, [0, 0, S3 | S4, 0]),
    dbuf(A | C, false, [S1 | S2, 0, S3 | S4, 0]),
    dbuf(B | C, false, [0, S1 | S2, S3 | S4, 0]),
    dbuf(A | B | C, false, [S1, S2, S3 | S4, 0]),
    dbuf(D, false, [0, 0, 0, S3 | S4]),
    dbuf(A | D, false, [S1 | S2, 0, 0, S3 | S4]),
    dbuf(B | D, false, [0, S1 | S2, 0, S3 | S4]),
    dbuf(A | B | D, false, [S1, S2, 0, S3 | S4]),
    dbuf(C | D, false, [0, 0, S3, S4]),
    dbuf(A | C | D, false, [S1 | S2, 0, S3, S4]),
    dbuf(B | C | D, false, [0, S1 | S2, S3, S4]),
    dbuf(A | B | C | D, false, [S1, S2, S3, S4]),
];
pub const ADLP_ALLOWED_DBUFS: &[DbufSliceConfig] = &[
    dbuf(A, true, [S1 | S2 | S3 | S4, 0, 0, 0]),
    dbuf(B, true, [0, S1 | S2 | S3 | S4, 0, 0]),
    dbuf(A, false, [S1 | S2, 0, 0, 0]),
    dbuf(B, false, [0, S3 | S4, 0, 0]),
    dbuf(A | B, false, [S1 | S2, S3 | S4, 0, 0]),
    dbuf(C, false, [0, 0, S3 | S4, 0]),
    dbuf(A | C, false, [S1 | S2, 0, S3 | S4, 0]),
    dbuf(B | C, false, [0, S3 | S4, S3 | S4, 0]),
    dbuf(A | B | C, false, [S1 | S2, S3 | S4, S3 | S4, 0]),
    dbuf(D, false, [0, 0, 0, S1 | S2]),
    dbuf(A | D, false, [S1 | S2, 0, 0, S1 | S2]),
    dbuf(B | D, false, [0, S3 | S4, 0, S1 | S2]),
    dbuf(A | B | D, false, [S1 | S2, S3 | S4, 0, S1 | S2]),
    dbuf(C | D, false, [0, 0, S3 | S4, S1 | S2]),
    dbuf(A | C | D, false, [S1 | S2, 0, S3 | S4, S1 | S2]),
    dbuf(B | C | D, false, [0, 0, S3 | S4, S1 | S2]),
    dbuf(A | B | C | D, false, [S1 | S2, 0, S3 | S4, S1 | S2]),
];
// upstream: skl_watermark.c skl_plane_wm_level()
pub fn skl_plane_wm_level(pipe_wm: &PipeWm, plane: usize, level: usize) -> WmLevel {
    let p = pipe_wm.planes[plane];
    if level == 0 && pipe_wm.use_sagv_wm {
        p.sagv_wm0
    } else {
        p.levels[level]
    }
}
// upstream: skl_watermark.c skl_plane_trans_wm()
pub fn skl_plane_trans_wm(pipe_wm: &PipeWm, plane: usize) -> WmLevel {
    let p = pipe_wm.planes[plane];
    if pipe_wm.use_sagv_wm {
        p.sagv_trans_wm
    } else {
        p.trans_wm
    }
}
// upstream: skl_watermark.c skl_check_wm_level()
pub fn skl_check_wm_level(wm: &mut WmLevel, ddb: DdbEntry) {
    if wm.min_ddb_alloc > ddb.size() {
        *wm = WmLevel::default();
    }
}
// upstream: skl_watermark.c skl_check_wm_level_nv12()
pub fn skl_check_wm_level_nv12(wm: &mut WmLevel, ddb_y: DdbEntry, ddb: DdbEntry) {
    if wm.min_ddb_alloc > ddb_y.size() || wm.min_ddb_alloc_uv > ddb.size() {
        *wm = WmLevel::default();
    }
}
// upstream: skl_watermark.c skl_need_wm_copy_wa()
pub fn skl_need_wm_copy_wa(level: usize, wm: &PlaneWm) -> bool {
    level > 0 && !wm.levels[level].enable
}

// upstream: skl_watermark.c _skl_allocate_plane_ddb()
pub fn _skl_allocate_plane_ddb(
    iter: &mut PlaneDdbIter,
    min_ddb_alloc: u16,
    ddb: &mut DdbEntry,
    data_rate: u64,
) {
    let mut extra = 0u16;
    if data_rate != 0 && iter.data_rate != 0 {
        extra = min(
            iter.size,
            ((iter.size as u64 * data_rate + iter.data_rate - 1) / iter.data_rate) as u16,
        );
        iter.size -= extra;
        iter.data_rate -= data_rate;
    }
    let size = min_ddb_alloc + extra;
    if size != 0 {
        iter.start = skl_ddb_entry_init(ddb, iter.start, iter.start + size);
    }
}
// upstream: skl_watermark.c skl_allocate_plane_ddb()
pub fn skl_allocate_plane_ddb(
    iter: &mut PlaneDdbIter,
    wm: WmLevel,
    ddb: &mut DdbEntry,
    data_rate: u64,
) {
    _skl_allocate_plane_ddb(iter, wm.min_ddb_alloc, ddb, data_rate);
}
// upstream: skl_watermark.c skl_allocate_plane_ddb_nv12()
pub fn skl_allocate_plane_ddb_nv12(
    iter: &mut PlaneDdbIter,
    wm: WmLevel,
    ddb_y: &mut DdbEntry,
    rate_y: u64,
    ddb: &mut DdbEntry,
    rate: u64,
) {
    _skl_allocate_plane_ddb(iter, wm.min_ddb_alloc, ddb_y, rate_y);
    _skl_allocate_plane_ddb(iter, wm.min_ddb_alloc_uv, ddb, rate);
}
// upstream: skl_watermark.c skl_crtc_allocate_plane_ddb()
pub fn skl_crtc_allocate_plane_ddb(
    io: &impl PlaneDdbFramework,
    display: &DisplayCaps,
    alloc: DdbEntry,
    model: &mut PlaneDdbModel,
) -> Result<(), i32> {
    model.plane_ddb = [DdbEntry::default(); PLANES];
    model.plane_ddb_y = [DdbEntry::default(); PLANES];
    model.plane_min_ddb = [0; PLANES];
    model.plane_interim_ddb = [0; PLANES];
    if !model.active {
        return Ok(());
    }
    let mut iter = PlaneDdbIter {
        data_rate: 0,
        start: alloc.start,
        size: alloc.size(),
    };
    if iter.size == 0 {
        return Ok(());
    }
    let cursor_size = io.cursor_allocation(model.active_pipes);
    iter.size -= cursor_size;
    skl_ddb_entry_init(&mut model.plane_ddb[5], alloc.end - cursor_size, alloc.end);
    iter.data_rate = skl_total_relative_data_rate(
        &model.rel_data_rate,
        &model.rel_data_rate_y,
        display.display_ver,
    );
    let mut blocks: u32;
    let mut level = model.num_levels as isize - 1;
    while level >= 0 {
        blocks = 0;
        for plane in 0..PLANES {
            let wm = model.planes[plane].levels[level as usize];
            if plane == 5 {
                if wm.min_ddb_alloc > model.plane_ddb[plane].size() {
                    io.warn(wm.min_ddb_alloc != U16_MAX);
                    blocks = u32::MAX;
                    break;
                }
                continue;
            }
            blocks = blocks
                .saturating_add(wm.min_ddb_alloc as u32)
                .saturating_add(wm.min_ddb_alloc_uv as u32);
        }
        if blocks <= iter.size as u32 {
            iter.size -= blocks as u16;
            break;
        }
        level -= 1;
    }
    if level < 0 {
        return Err(-22);
    }
    if iter.data_rate == 0 {
        iter.size = 0;
    }
    for plane in 0..PLANES {
        if plane == 5 {
            continue;
        }
        let wm = model.planes[plane].levels[level as usize];
        if display.display_ver < 11 && model.nv12_planes & (1 << plane) != 0 {
            skl_allocate_plane_ddb_nv12(
                &mut iter,
                wm,
                &mut model.plane_ddb_y[plane],
                model.rel_data_rate_y[plane],
                &mut model.plane_ddb[plane],
                model.rel_data_rate[plane],
            );
        } else {
            skl_allocate_plane_ddb(
                &mut iter,
                wm,
                &mut model.plane_ddb[plane],
                model.rel_data_rate[plane],
            );
        }
        if display.display_ver >= 30 {
            model.plane_min_ddb[plane] = model.planes[plane].levels[0].min_ddb_alloc;
            model.plane_interim_ddb[plane] = model.planes[plane].sagv_wm0.min_ddb_alloc;
        }
    }
    io.warn(iter.size != 0 || iter.data_rate != 0);
    for lev in (level + 1) as usize..model.num_levels.min(WM_LEVELS) {
        for plane in 0..PLANES {
            if display.display_ver < 11 && model.nv12_planes & (1 << plane) != 0 {
                skl_check_wm_level_nv12(
                    &mut model.planes[plane].levels[lev],
                    model.plane_ddb_y[plane],
                    model.plane_ddb[plane],
                );
            } else {
                skl_check_wm_level(&mut model.planes[plane].levels[lev], model.plane_ddb[plane]);
            }
            if skl_need_wm_copy_wa(lev, &model.planes[plane]) {
                let prev = model.planes[plane].levels[lev - 1];
                let wm = &mut model.planes[plane].levels[lev];
                wm.blocks = prev.blocks;
                wm.lines = prev.lines;
                wm.ignore_lines = prev.ignore_lines;
            }
        }
    }
    for plane in 0..PLANES {
        let ddb = model.plane_ddb[plane];
        let ddb_y = model.plane_ddb_y[plane];
        if display.display_ver < 11 && model.nv12_planes & (1 << plane) != 0 {
            skl_check_wm_level(&mut model.planes[plane].trans_wm, ddb_y);
        } else {
            io.warn(ddb_y.size() != 0);
            skl_check_wm_level(&mut model.planes[plane].trans_wm, ddb);
        }
        skl_check_wm_level(&mut model.planes[plane].sagv_wm0, ddb);
        if display.display_ver >= 30 {
            model.plane_interim_ddb[plane] = model.planes[plane].sagv_wm0.min_ddb_alloc;
        }
        skl_check_wm_level(&mut model.planes[plane].sagv_trans_wm, ddb);
    }
    Ok(())
}

// upstream: skl_watermark.c skl_wm_method1()
pub fn skl_wm_method1(
    display: &DisplayCaps,
    pixel_rate: u32,
    cpp: u8,
    latency: u32,
    block_size: u32,
) -> u32 {
    if latency == 0 {
        return FP_MAX;
    }
    let intermediate = latency.wrapping_mul(pixel_rate).wrapping_mul(cpp as u32);
    let mut ret = fp_div_u32(intermediate, 1000u32.wrapping_mul(block_size));
    if display.display_ver >= 10 {
        ret = ret.saturating_add(FP_ONE);
    }
    ret
}
// upstream: skl_watermark.c skl_wm_method2()
pub fn skl_wm_method2(pixel_rate: u32, htotal: u32, latency: u32, blocks_per_line: u32) -> u32 {
    if latency == 0 {
        return FP_MAX;
    }
    let intermediate = div_round_up(latency.wrapping_mul(pixel_rate), htotal.wrapping_mul(1000));
    fp_mul_u32(intermediate, blocks_per_line)
}
// upstream: skl_watermark.c skl_wm_linetime_us()
pub fn skl_wm_linetime_us(htotal: u32, pixel_rate: u32) -> u32 {
    div_round_up(htotal.wrapping_mul(1000), pixel_rate)
}

// upstream: skl_watermark.c skl_compute_wm_params()
pub fn skl_compute_wm_params(
    display: &DisplayCaps,
    width: u32,
    cpp: u8,
    modifier: ModifierInfo,
    rotation_90_or_270: bool,
    plane_pixel_rate: u32,
    pipe_htotal: u32,
    pan_x: u32,
    color_plane: u8,
    semiplanar: bool,
) -> Result<WatermarkParams, i32> {
    if color_plane == 1 && !semiplanar {
        return Err(-22);
    }
    let x_tiled = modifier.x_tiled;
    let y_tiled = !modifier.x_tiled && modifier.tiled;
    let width = if color_plane == 1 && semiplanar {
        width / 2
    } else {
        width
    };
    let dbuf_block_size = if display.display_ver >= 11 && modifier.yf_tiled && cpp == 1 {
        256
    } else {
        512
    };
    let y_min_scanlines = if rotation_90_or_270 {
        match cpp {
            1 => 16,
            2 => 8,
            4 => 4,
            _ => return Err(-22),
        }
    } else {
        4
    } * if skl_needs_memory_bw_wa(display) {
        2
    } else {
        1
    };
    let plane_bytes_per_line = width.wrapping_mul(cpp as u32);
    let mut interm_pbpl;
    let plane_blocks_per_line;
    if y_tiled {
        interm_pbpl = div_round_up(
            plane_bytes_per_line.wrapping_mul(y_min_scanlines),
            dbuf_block_size,
        );
        if display.display_ver >= 30 {
            interm_pbpl += u32::from(pan_x != 0);
        } else if display.display_ver >= 10 {
            interm_pbpl += 1;
        }
        plane_blocks_per_line = fp_div_u32(interm_pbpl, y_min_scanlines);
    } else {
        interm_pbpl = div_round_up(plane_bytes_per_line, dbuf_block_size);
        if !x_tiled || display.display_ver >= 10 {
            interm_pbpl += 1;
        }
        plane_blocks_per_line = fp_from_u32(interm_pbpl);
    }
    let y_tile_minimum = fp_mul_u32(y_min_scanlines, plane_blocks_per_line);
    Ok(WatermarkParams {
        x_tiled,
        y_tiled,
        rc_surface: modifier.rc_surface,
        width,
        cpp,
        plane_pixel_rate,
        y_min_scanlines,
        plane_bytes_per_line,
        plane_blocks_per_line,
        y_tile_minimum,
        linetime_us: skl_wm_linetime_us(pipe_htotal, plane_pixel_rate),
        pipe_htotal,
        dbuf_block_size,
    })
}
// upstream: skl_watermark.c skl_compute_plane_wm_params()
pub fn skl_compute_plane_wm_params(
    display: &DisplayCaps,
    input: &PlaneWmInput,
    color_plane: u8,
) -> Result<WatermarkParams, i32> {
    let cpp = if color_plane == 1 {
        input.cpp_uv
    } else {
        input.cpp
    };
    skl_compute_wm_params(
        display,
        input.width,
        cpp,
        input.modifier,
        input.rotation_90_or_270,
        input.pixel_rate,
        input.pipe_htotal,
        input.pan_x,
        color_plane,
        input.semiplanar,
    )
}

// upstream: skl_watermark.c skl_wm_has_lines()
pub fn skl_wm_has_lines(display: &DisplayCaps, level: usize) -> bool {
    display.display_ver >= 10 || level > 0
}
// upstream: skl_watermark.c skl_wm_max_lines()
pub fn skl_wm_max_lines(display: &DisplayCaps) -> u32 {
    if display.display_ver >= 13 { 255 } else { 31 }
}
// upstream: skl_watermark.c xe3_auto_min_alloc_capable()
pub fn xe3_auto_min_alloc_capable(display: &DisplayCaps, plane: usize, level: usize) -> bool {
    display.display_ver >= 30 && level == 0 && plane != 5
}

// upstream: skl_watermark.c skl_compute_plane_wm()
pub fn skl_compute_plane_wm(
    display: &DisplayCaps,
    plane: usize,
    level: usize,
    latency: u32,
    wp: &WatermarkParams,
    result_prev: WmLevel,
    minimal_wm0_only: bool,
    sagv_block_time_us: u32,
) -> WmLevel {
    let mut result = WmLevel::default();
    if latency == 0 || (minimal_wm0_only && level > 0) {
        result.min_ddb_alloc = U16_MAX;
        return result;
    }
    let method1 = skl_wm_method1(
        display,
        wp.plane_pixel_rate,
        wp.cpp,
        latency,
        wp.dbuf_block_size,
    );
    let method2 = skl_wm_method2(
        wp.plane_pixel_rate,
        wp.pipe_htotal,
        latency,
        wp.plane_blocks_per_line,
    );
    let selected = if wp.y_tiled {
        max(method2, wp.y_tile_minimum)
    } else if display.display_ver >= 35 {
        method2
    } else if (wp.cpp as u32 * wp.pipe_htotal / wp.dbuf_block_size < 1)
        && (wp.plane_bytes_per_line / wp.dbuf_block_size < 1)
    {
        method2
    } else if latency >= wp.linetime_us {
        if display.display_ver == 9 {
            min(method1, method2)
        } else {
            method2
        }
    } else {
        method1
    };
    let mut blocks = fp_round_up(selected);
    if display.display_ver < 30 {
        blocks = blocks.saturating_add(1);
    }
    if skl_wm_has_lines(display, level) {
        blocks = max(blocks, fp_round_up(wp.plane_blocks_per_line));
    }
    let mut lines = fp_div_round_up(selected, wp.plane_blocks_per_line);
    if display.display_ver == 9 {
        if level == 0 && wp.rc_surface {
            blocks = blocks.saturating_add(fp_round_up(wp.y_tile_minimum));
        }
        if (1..=7).contains(&level) {
            if wp.y_tiled {
                blocks = blocks.saturating_add(fp_round_up(wp.y_tile_minimum));
                lines = lines.saturating_add(wp.y_min_scanlines);
            } else {
                blocks = blocks.saturating_add(1);
            }
        }
    }
    blocks = max(blocks, result_prev.blocks);
    lines = max(lines, result_prev.lines);
    let min_ddb_alloc = if display.display_ver >= 11 {
        if wp.y_tiled {
            let extra = if lines % wp.y_min_scanlines == 0 {
                wp.y_min_scanlines
            } else {
                wp.y_min_scanlines * 2 - lines % wp.y_min_scanlines
            };
            fp_mul_round_up_u32(lines + extra, wp.plane_blocks_per_line)
        } else {
            blocks + div_round_up(blocks, 10)
        }
    } else {
        0
    };
    if !skl_wm_has_lines(display, level) {
        lines = 0;
    }
    if lines > skl_wm_max_lines(display) {
        result.min_ddb_alloc = U16_MAX;
        return result;
    }
    result.blocks = blocks;
    result.lines = lines;
    result.min_ddb_alloc = max(min_ddb_alloc, blocks)
        .saturating_add(1)
        .min(u16::MAX as u32) as u16;
    result.enable = true;
    result.auto_min_alloc_wm_enable = xe3_auto_min_alloc_capable(display, plane, level);
    if !display.sagv_wm && sagv_block_time_us != 0 {
        result.can_sagv = latency >= sagv_block_time_us;
    }
    result
}
// upstream: skl_watermark.c skl_compute_wm_levels()
pub fn skl_compute_wm_levels(
    display: &DisplayCaps,
    plane: usize,
    wp: &WatermarkParams,
    latencies: &[u32; WM_LEVELS],
    nlevels: usize,
    minimal_wm0_only: bool,
    sagv_block_time_us: u32,
) -> [WmLevel; WM_LEVELS] {
    let mut levels = [WmLevel::default(); WM_LEVELS];
    let mut prev = levels[0];
    for level in 0..nlevels.min(WM_LEVELS) {
        let result = skl_compute_plane_wm(
            display,
            plane,
            level,
            latencies[level],
            wp,
            prev,
            minimal_wm0_only,
            sagv_block_time_us,
        );
        levels[level] = result;
        prev = result;
    }
    levels
}

// upstream: skl_watermark.c tgl_compute_sagv_wm()
pub fn tgl_compute_sagv_wm(
    display: &DisplayCaps,
    plane: usize,
    wp: &WatermarkParams,
    levels: &[WmLevel; WM_LEVELS],
    latency_zero: u32,
    sagv_block_time_us: u32,
    minimal_wm0_only: bool,
) -> WmLevel {
    let latency = if sagv_block_time_us != 0 {
        sagv_block_time_us.saturating_add(latency_zero)
    } else {
        0
    };
    skl_compute_plane_wm(
        display,
        plane,
        0,
        latency,
        wp,
        levels[0],
        minimal_wm0_only,
        sagv_block_time_us,
    )
}
// upstream: skl_watermark.c skl_compute_transition_wm()
pub fn skl_compute_transition_wm(
    display: &DisplayCaps,
    ipc_enabled: bool,
    trans_wm: &mut WmLevel,
    wm0: WmLevel,
    wp: &WatermarkParams,
) {
    if !ipc_enabled || display.display_ver == 9 {
        return;
    }
    let trans_min = if display.display_ver >= 11 { 4u16 } else { 14 };
    let trans_amount = if display.display_ver == 10 { 0 } else { 10 };
    let trans_offset = trans_min + trans_amount;
    let wm0_blocks = (wm0.blocks as u16).wrapping_sub(1);
    let mut blocks = if wp.y_tiled {
        max(wm0_blocks, fp_mul_round_up_u32(2, wp.y_tile_minimum) as u16).wrapping_add(trans_offset)
    } else {
        wm0_blocks.wrapping_add(trans_offset)
    };
    blocks = blocks.wrapping_add(1);
    trans_wm.blocks = blocks as u32;
    trans_wm.min_ddb_alloc = max(wm0.min_ddb_alloc, blocks.wrapping_add(1));
    trans_wm.enable = true;
}
// upstream: skl_watermark.c skl_build_plane_wm_single()
pub fn skl_build_plane_wm_single(
    display: &DisplayCaps,
    input: &PlaneWmInput,
    plane: usize,
    color_plane: u8,
    num_levels: usize,
    latencies: &[u32; WM_LEVELS],
    ipc: bool,
    minimal_wm0_only: bool,
    sagv_block_time_us: u32,
    wm: &mut PlaneWm,
) -> Result<(), i32> {
    let params = skl_compute_plane_wm_params(display, input, color_plane)?;
    wm.levels = skl_compute_wm_levels(
        display,
        plane,
        &params,
        latencies,
        num_levels,
        minimal_wm0_only,
        sagv_block_time_us,
    );
    skl_compute_transition_wm(display, ipc, &mut wm.trans_wm, wm.levels[0], &params);
    if display.sagv_wm {
        wm.sagv_wm0 = tgl_compute_sagv_wm(
            display,
            plane,
            &params,
            &wm.levels,
            latencies[0],
            sagv_block_time_us,
            minimal_wm0_only,
        );
        skl_compute_transition_wm(display, ipc, &mut wm.sagv_trans_wm, wm.sagv_wm0, &params);
    }
    Ok(())
}
// upstream: skl_watermark.c skl_build_plane_wm_uv()
pub fn skl_build_plane_wm_uv(
    display: &DisplayCaps,
    input: &PlaneWmInput,
    plane: usize,
    num_levels: usize,
    latencies: &[u32; WM_LEVELS],
    sagv_block_time_us: u32,
    wm: &mut PlaneWm,
) -> Result<(), i32> {
    let params = skl_compute_plane_wm_params(display, input, 1)?;
    let levels = skl_compute_wm_levels(
        display,
        plane,
        &params,
        latencies,
        num_levels,
        false,
        sagv_block_time_us,
    );
    for level in 0..num_levels.min(WM_LEVELS) {
        wm.levels[level].min_ddb_alloc_uv = levels[level].min_ddb_alloc;
    }
    Ok(())
}
// upstream: skl_watermark.c skl_build_plane_wm()
pub fn skl_build_plane_wm(
    display: &DisplayCaps,
    input: &PlaneWmInput,
    plane: usize,
    num_levels: usize,
    latencies: &[u32; WM_LEVELS],
    ipc: bool,
    sagv_block_time_us: u32,
    wm: &mut PlaneWm,
) -> Result<(), i32> {
    *wm = PlaneWm::default();
    if !input.visible {
        return Ok(());
    }
    skl_build_plane_wm_single(
        display,
        input,
        plane,
        0,
        num_levels,
        latencies,
        ipc,
        use_minimal_wm0_only(display, input.crtc_async_flip, input.plane_async_flip),
        sagv_block_time_us,
        wm,
    )?;
    if input.yuv && input.num_format_planes > 1 {
        skl_build_plane_wm_uv(
            display,
            input,
            plane,
            num_levels,
            latencies,
            sagv_block_time_us,
            wm,
        )?;
    }
    Ok(())
}
// upstream: skl_watermark.c icl_build_plane_wm()
pub fn icl_build_plane_wm(
    display: &DisplayCaps,
    input: &PlaneWmInput,
    plane: usize,
    linked_plane: Option<usize>,
    num_levels: usize,
    latencies: &[u32; WM_LEVELS],
    ipc: bool,
    sagv_block_time_us: u32,
    wm: &mut PlaneWm,
) -> Result<(), i32> {
    if input.is_y_plane {
        return Ok(());
    }
    *wm = PlaneWm::default();
    if input.has_planar_linked_plane {
        if !input.yuv || input.num_format_planes == 1 {
            return Err(-22);
        }
        // The linked-plane and current-plane calls share the same format geometry,
        // but use the upstream primary/UV color-plane selectors respectively.
        let mut y_input = *input;
        y_input.is_y_plane = false;
        skl_build_plane_wm_single(
            display,
            &y_input,
            linked_plane.unwrap_or(plane),
            0,
            num_levels,
            latencies,
            ipc,
            use_minimal_wm0_only(display, input.crtc_async_flip, input.plane_async_flip),
            sagv_block_time_us,
            wm,
        )?;
        skl_build_plane_wm_single(
            display,
            input,
            plane,
            1,
            num_levels,
            latencies,
            ipc,
            use_minimal_wm0_only(display, input.crtc_async_flip, input.plane_async_flip),
            sagv_block_time_us,
            wm,
        )?;
    } else if input.visible {
        skl_build_plane_wm_single(
            display,
            input,
            plane,
            0,
            num_levels,
            latencies,
            ipc,
            use_minimal_wm0_only(display, input.crtc_async_flip, input.plane_async_flip),
            sagv_block_time_us,
            wm,
        )?;
    }
    Ok(())
}
// upstream: skl_watermark.c skl_wm0_prefill_lines_worst()
pub fn skl_wm0_prefill_lines_worst(
    display: &DisplayCaps,
    pipe_clock: u32,
    pipe_htotal: u32,
    pipe_hdisplay: u32,
    max_total_scale_16_16: u32,
    max_hscale_16_16: u32,
    latencies: &[u32; WM_LEVELS],
) -> u32 {
    let ceil_fixed = |v: u64| ((v + 0xffff) >> 16).min(u32::MAX as u64) as u32;
    let pixel_rate = ceil_fixed(pipe_clock as u64 * max_total_scale_16_16 as u64);
    let width = ceil_fixed(pipe_hdisplay as u64 * max_hscale_16_16 as u64);
    let modifier = ModifierInfo {
        tiled: true,
        ..ModifierInfo::default()
    };
    let Ok(wp) = skl_compute_wm_params(
        display,
        width,
        8,
        modifier,
        false,
        pixel_rate,
        pipe_htotal,
        1,
        0,
        false,
    ) else {
        return 0;
    };
    let latency = skl_wm_latency(display, latencies, 0, Some(&wp), false);
    let mut wm = WmLevel::default();
    wm = skl_compute_plane_wm(display, 0, 0, latency, &wp, wm, false, 0);
    if wm.min_ddb_alloc == U16_MAX {
        wm.lines = skl_wm_max_lines(display);
    }
    wm.lines << 16
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PkgcCrtc {
    pub pipe: usize,
    pub vrr_enabled: bool,
    pub linetime_us: u32,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PkgcState {
    pub disable: [bool; 4],
    pub linetime: [u32; 4],
}
// upstream: skl_watermark.c skl_max_wm0_lines()
pub fn skl_max_wm0_lines(planes: &[PlaneWm; PLANES]) -> u32 {
    planes.iter().map(|p| p.levels[0].lines).max().unwrap_or(0)
}
// upstream: skl_watermark.c skl_wm0_prefill_lines()
pub fn skl_wm0_prefill_lines(planes: &[PlaneWm; PLANES]) -> u32 {
    skl_max_wm0_lines(planes) << 16
}
// upstream: skl_watermark.c skl_max_wm_level_for_vblank()
pub fn skl_max_wm_level_for_vblank(
    io: &impl PrefillFramework,
    display: &DisplayCaps,
    latencies: &[u32; WM_LEVELS],
    num_levels: usize,
) -> Result<usize, i32> {
    for level in (0..num_levels.min(WM_LEVELS)).rev() {
        let mut latency = skl_wm_latency(display, latencies, level, None, io.ipc_enabled());
        if latency == 0 {
            continue;
        }
        if level == 0 {
            latency = 0;
        }
        if !io.vblank_too_short(latency) {
            return Ok(level);
        }
    }
    Err(-22)
}
// upstream: skl_watermark.c skl_wm_check_vblank()
pub fn skl_wm_check_vblank(
    io: &impl PrefillFramework,
    display: &DisplayCaps,
    active: bool,
    latencies: &[u32; WM_LEVELS],
    num_levels: usize,
    sagv_block_time_us: u32,
    planes: &mut [PlaneWm; PLANES],
) -> Result<bool, i32> {
    if !active {
        return Ok(false);
    }
    io.init_prefill();
    let level = skl_max_wm_level_for_vblank(io, display, latencies, num_levels)?;
    let disabled = level + 1 < num_levels;
    for plane in planes.iter_mut() {
        for l in level + 1..num_levels.min(WM_LEVELS) {
            plane.levels[l].enable = false;
        }
    }
    if display.sagv_wm && sagv_block_time_us != 0 && io.vblank_too_short(sagv_block_time_us) {
        for p in planes {
            p.sagv_wm0.enable = false;
            p.sagv_trans_wm.enable = false;
        }
    }
    Ok(disabled)
}
// upstream: skl_watermark.c skl_build_pipe_wm()
pub fn skl_build_pipe_wm(
    io: &impl PrefillFramework,
    display: &DisplayCaps,
    planes: &[(usize, PlaneWmInput)],
    num_levels: usize,
    latencies: &[u32; WM_LEVELS],
    ipc: bool,
    sagv_block_time_us: u32,
    active: bool,
    wm: &mut [PlaneWm; PLANES],
) -> Result<bool, i32> {
    for (id, input) in planes {
        if display.display_ver >= 11 {
            icl_build_plane_wm(
                display,
                input,
                *id,
                None,
                num_levels,
                latencies,
                ipc,
                sagv_block_time_us,
                &mut wm[*id],
            )?;
        } else {
            skl_build_plane_wm(
                display,
                input,
                *id,
                num_levels,
                latencies,
                ipc,
                sagv_block_time_us,
                &mut wm[*id],
            )?;
        }
    }
    skl_wm_check_vblank(
        io,
        display,
        active,
        latencies,
        num_levels,
        sagv_block_time_us,
        wm,
    )
}

// upstream: skl_watermark.c skl_wm_level_equals()
pub fn skl_wm_level_equals(a: WmLevel, b: WmLevel) -> bool {
    a.enable == b.enable
        && a.ignore_lines == b.ignore_lines
        && a.lines == b.lines
        && a.blocks == b.blocks
        && a.auto_min_alloc_wm_enable == b.auto_min_alloc_wm_enable
}
// upstream: skl_watermark.c skl_plane_wm_equals()
pub fn skl_plane_wm_equals(a: &PlaneWm, b: &PlaneWm, num_levels: usize) -> bool {
    for i in 0..num_levels.min(WM_LEVELS) {
        if !skl_wm_level_equals(a.levels[i], b.levels[i]) {
            return false;
        }
    }
    skl_wm_level_equals(a.trans_wm, b.trans_wm)
        && skl_wm_level_equals(a.sagv_wm0, b.sagv_wm0)
        && skl_wm_level_equals(a.sagv_trans_wm, b.sagv_trans_wm)
}
// upstream: skl_watermark.c skl_ddb_entries_overlap()
pub fn skl_ddb_entries_overlap(a: DdbEntry, b: DdbEntry) -> bool {
    a.start < b.end && b.start < a.end
}
// upstream: skl_watermark.c skl_ddb_entry_union()
pub fn skl_ddb_entry_union(a: &mut DdbEntry, b: DdbEntry) {
    if a.end != 0 && b.end != 0 {
        a.start = min(a.start, b.start);
        a.end = max(a.end, b.end);
    } else if b.end != 0 {
        *a = b;
    }
}
// upstream: skl_watermark.c skl_ddb_allocation_overlaps()
pub fn skl_ddb_allocation_overlaps(ddb: DdbEntry, entries: &[DdbEntry], ignore_idx: usize) -> bool {
    entries
        .iter()
        .enumerate()
        .any(|(i, e)| i != ignore_idx && skl_ddb_entries_overlap(ddb, *e))
}
// upstream: skl_watermark.c skl_ddb_add_affected_planes()
pub fn skl_ddb_add_affected_planes(crtc: &mut AtomicCrtcDdb) -> Result<(), i32> {
    for plane in 0..PLANES {
        if crtc.old_model.plane_ddb[plane].equal(crtc.new_model.plane_ddb[plane])
            && crtc.old_model.plane_ddb_y[plane].equal(crtc.new_model.plane_ddb_y[plane])
        {
            continue;
        }
        if crtc.do_async_flip {
            return Err(-22);
        }
        crtc.update_planes |= 1 << plane;
        crtc.async_flip_planes = 0;
        crtc.do_async_flip = false;
    }
    Ok(())
}
// upstream: skl_watermark.c intel_dbuf_enabled_slices()
pub fn intel_dbuf_enabled_slices(state: &DbufState) -> u8 {
    state.slices.iter().fold(1 << 1, |mask, s| mask | s)
}
// upstream: skl_watermark.c skl_compute_ddb()
pub fn skl_compute_ddb(
    io: &mut impl AtomicDdbFramework,
    display: &DisplayCaps,
    old: &DbufState,
    new: &mut DbufState,
    crtcs: &mut [AtomicCrtcDdb; 4],
    total_dbuf_size: u16,
    display_slice_mask: u8,
) -> Result<(), i32> {
    if !crtcs.iter().any(|c| c.new_in_state) {
        return Ok(());
    }
    let mut active = 0u8;
    for c in crtcs.iter() {
        if c.new_in_state {
            if c.active {
                active |= 1 << c.pipe;
            }
        } else {
            active |= old.active_pipes & (1 << c.pipe);
        }
    }
    new.active_pipes = active;
    if old.active_pipes != new.active_pipes {
        io.lock_global_state()?;
    }
    if display.has_mbus_joining {
        new.joined_mbus = adlp_check_mbus_joined(new.active_pipes);
        if old.joined_mbus != new.joined_mbus {
            io.set_joined_mbus(new.joined_mbus)?;
        }
    }
    for pipe in 0..4 {
        new.slices[pipe] =
            skl_compute_dbuf_slices(display, pipe, new.active_pipes, new.joined_mbus);
        if old.slices[pipe] != new.slices[pipe] {
            io.lock_global_state()?;
        }
    }
    new.enabled_slices = intel_dbuf_enabled_slices(new);
    if old.enabled_slices != new.enabled_slices || old.joined_mbus != new.joined_mbus {
        io.serialize_global_state()?;
        new.changed = true;
    }
    for c in crtcs.iter() {
        if c.new_in_state {
            new.weight[c.pipe] = intel_crtc_ddb_weight(c.active, c.hdisplay);
            if old.weight[c.pipe] != new.weight[c.pipe] {
                io.lock_global_state()?;
            }
        }
    }
    for pipe in 0..4 {
        skl_crtc_allocate_ddb(io, old, new, pipe, total_dbuf_size, display_slice_mask)?;
    }
    for c in crtcs.iter_mut() {
        if c.new_in_state {
            let alloc = new.ddb[c.pipe];
            skl_crtc_allocate_plane_ddb(io, display, alloc, &mut c.new_model)?;
            skl_ddb_add_affected_planes(c)?;
            io.add_affected_planes(c.pipe, &c.old_model, &mut c.new_model)?;
        }
    }
    Ok(())
}
pub trait AtomicWmFramework: AtomicDdbFramework + PrefillFramework {
    fn print_wm_changes(&self, _crtcs: &[AtomicCrtcDdb; 4]) {}
}
// upstream: skl_watermark.c enast()
pub fn enast(enable: bool) -> char {
    if enable { '*' } else { ' ' }
}
// upstream: skl_watermark.c skl_print_plane_ddb_changes()
pub fn skl_print_plane_ddb_changes(
    io: &impl WatermarkDebugLog,
    pipe: usize,
    plane: usize,
    old: DdbEntry,
    new: DdbEntry,
    name: &'static str,
) {
    io.ddb_change(pipe, plane, name, old, new);
}
// upstream: skl_watermark.c skl_print_plane_wm_changes()
pub fn skl_print_plane_wm_changes(
    io: &impl WatermarkDebugLog,
    display: &DisplayCaps,
    pipe: usize,
    plane: usize,
    old: PlaneWm,
    new: PlaneWm,
) {
    io.wm_change(pipe, plane, old, new, display.display_ver);
}
// upstream: skl_watermark.c skl_print_wm_changes()
pub fn skl_print_wm_changes(
    io: &impl WatermarkDebugLog,
    display: &DisplayCaps,
    crtcs: &[AtomicCrtcDdb; 4],
) {
    if !io.debug_enabled() {
        return;
    }
    for c in crtcs {
        if !c.new_in_state {
            continue;
        }
        for plane in 0..PLANES {
            let old = c.old_model.plane_ddb[plane];
            let new = c.new_model.plane_ddb[plane];
            if !old.equal(new) {
                skl_print_plane_ddb_changes(io, c.pipe, plane, old, new, "ddb");
            }
            if display.display_ver < 11 {
                let old = c.old_model.plane_ddb_y[plane];
                let new = c.new_model.plane_ddb_y[plane];
                if !old.equal(new) {
                    skl_print_plane_ddb_changes(io, c.pipe, plane, old, new, "ddb_y");
                }
            }
        }
        for plane in 0..PLANES {
            let old = c.optimal_old.planes[plane];
            let new = c.optimal_new.planes[plane];
            if !skl_plane_wm_equals(&old, &new, WM_LEVELS) {
                skl_print_plane_wm_changes(io, display, c.pipe, plane, old, new);
            }
        }
    }
}
// upstream: skl_watermark.c skl_plane_selected_wm_equals()
pub fn skl_plane_selected_wm_equals(
    display: &DisplayCaps,
    plane: usize,
    old: &PipeWm,
    new: &PipeWm,
    num_levels: usize,
) -> bool {
    for level in 0..num_levels.min(WM_LEVELS) {
        if !skl_wm_level_equals(
            skl_plane_wm_level(old, plane, level),
            skl_plane_wm_level(new, plane, level),
        ) {
            return false;
        }
    }
    if display.has_hw_sagv_wm
        && (!skl_wm_level_equals(old.planes[plane].sagv_wm0, new.planes[plane].sagv_wm0)
            || !skl_wm_level_equals(
                old.planes[plane].sagv_trans_wm,
                new.planes[plane].sagv_trans_wm,
            ))
    {
        return false;
    }
    skl_wm_level_equals(
        skl_plane_trans_wm(old, plane),
        skl_plane_trans_wm(new, plane),
    )
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AtomicCrtcDdb {
    pub pipe: usize,
    pub active: bool,
    pub hdisplay: u32,
    pub new_in_state: bool,
    pub inherited: bool,
    pub interlaced: bool,
    pub wm_level_disabled: bool,
    pub old_model: PlaneDdbModel,
    pub new_model: PlaneDdbModel,
    pub plane_inputs: [Option<(usize, PlaneWmInput)>; PLANES],
    pub ddb_absolute: DdbEntry,
    pub optimal_old: PipeWm,
    pub optimal_new: PipeWm,
    pub needs_modeset: bool,
    pub do_async_flip: bool,
    pub update_planes: u8,
    pub async_flip_planes: u8,
}
// upstream: skl_watermark.c skl_wm_add_affected_planes()
pub fn skl_wm_add_affected_planes(
    io: &mut impl AtomicDdbFramework,
    display: &DisplayCaps,
    crtc: &mut AtomicCrtcDdb,
    num_levels: usize,
) -> Result<(), i32> {
    for plane in 0..PLANES {
        if !crtc.needs_modeset
            && skl_plane_selected_wm_equals(
                display,
                plane,
                &crtc.optimal_old,
                &crtc.optimal_new,
                num_levels,
            )
        {
            continue;
        }
        if crtc.do_async_flip {
            return Err(-22);
        }
        io.add_wm_affected_plane(crtc.pipe, plane)?;
        crtc.update_planes |= 1 << plane;
        crtc.async_flip_planes = 0;
        crtc.do_async_flip = false;
    }
    Ok(())
}
// upstream: skl_watermark.c pkgc_max_linetime()
pub fn pkgc_max_linetime(
    state: &mut PkgcState,
    new_crtcs: &[PkgcCrtc],
    all_crtcs: &[PkgcCrtc],
) -> u32 {
    for c in new_crtcs {
        state.disable[c.pipe] = c.vrr_enabled;
        state.linetime[c.pipe] = div_round_up(c.linetime_us, 8);
    }
    let mut max_linetime = 0;
    for c in all_crtcs {
        if state.disable[c.pipe] {
            return 0;
        }
        max_linetime = max(max_linetime, state.linetime[c.pipe]);
    }
    max_linetime
}
// upstream: skl_watermark.c intel_program_dpkgc_latency()
pub fn intel_program_dpkgc_latency(
    io: &impl PkgcIo,
    display: &DisplayCaps,
    latencies: &[u32; WM_LEVELS],
    pkgc: &mut PkgcState,
    new_crtcs: &[PkgcCrtc],
    all_crtcs: &[PkgcCrtc],
    enable_flipq: bool,
) -> (u32, u32) {
    if display.display_ver < 20 {
        return (0, 0);
    }
    io.lock_wm_mutex();
    let mut latency = skl_watermark_max_latency(display, latencies, 1);
    let mut added = if enable_flipq {
        io.flipq_exec_time_us()
    } else {
        0
    };
    if latency != 0 && display.is_ver(20, 30) {
        latency = latency.saturating_add(added);
        added = 0;
    }
    let max_line = pkgc_max_linetime(pkgc, new_crtcs, all_crtcs);
    if max_line == 0 || latency == 0 {
        latency = 0x1fff;
        added = 0;
    } else {
        latency = div_round_up(latency, max_line) * max_line;
    }
    io.program_pkgc_latency(latency, added);
    io.unlock_wm_mutex();
    (latency, added)
}
// upstream: skl_watermark.c skl_compute_wm()
pub fn skl_compute_wm(
    io: &mut impl AtomicWmFramework,
    display: &DisplayCaps,
    sagv: &SagvInfo,
    latencies: &[u32; WM_LEVELS],
    num_levels: usize,
    ipc: bool,
    old_dbuf: &DbufState,
    new_dbuf: &mut DbufState,
    crtcs: &mut [AtomicCrtcDdb; 4],
    total_dbuf_size: u16,
    display_slice_mask: u8,
) -> Result<(), i32> {
    for crtc in crtcs.iter_mut().filter(|c| c.new_in_state) {
        let mut inputs = [(0usize, PlaneWmInput::default()); PLANES];
        let mut count = 0;
        for item in crtc.plane_inputs.iter().flatten() {
            inputs[count] = *item;
            count += 1;
        }
        let mut raw = [PlaneWm::default(); PLANES];
        for (plane, input) in &inputs[..count] {
            if display.display_ver >= 11 {
                icl_build_plane_wm(
                    display,
                    input,
                    *plane,
                    None,
                    num_levels,
                    latencies,
                    ipc,
                    sagv.block_time_us,
                    &mut raw[*plane],
                )?;
            } else {
                skl_build_plane_wm(
                    display,
                    input,
                    *plane,
                    num_levels,
                    latencies,
                    ipc,
                    sagv.block_time_us,
                    &mut raw[*plane],
                )?;
            }
        }
        crtc.new_model.raw_planes = raw;
        crtc.new_model.planes = raw;
        crtc.optimal_new.planes = raw;
        let disabled = skl_wm_check_vblank(
            io,
            display,
            crtc.active,
            latencies,
            num_levels,
            sagv.block_time_us,
            &mut crtc.optimal_new.planes,
        )?;
        crtc.wm_level_disabled = disabled;
        crtc.new_model.planes = crtc.optimal_new.planes;
    }
    skl_compute_ddb(
        io,
        display,
        old_dbuf,
        new_dbuf,
        crtcs,
        total_dbuf_size,
        display_slice_mask,
    )?;
    for crtc in crtcs.iter_mut().filter(|c| c.new_in_state) {
        crtc.optimal_new.planes = crtc.new_model.planes;
        crtc.optimal_new.use_sagv_wm = display.sagv_wm
            && !display.has_hw_sagv_wm
            && intel_crtc_can_enable_sagv(
                display,
                sagv,
                crtc.active,
                crtc.interlaced,
                crtc.inherited,
                &crtc.optimal_new.planes,
                num_levels,
            );
        skl_wm_add_affected_planes(io, display, crtc, num_levels)?;
    }
    io.print_wm_changes(crtcs);
    Ok(())
}
// upstream: skl_watermark.c skl_wm_level_from_reg_val()
pub fn skl_wm_level_from_reg_val(display: &DisplayCaps, raw: u32) -> WmLevel {
    WmLevel {
        enable: raw & (1 << 31) != 0,
        ignore_lines: raw & (1 << 30) != 0,
        blocks: raw & 0x1fff,
        lines: (raw >> 14) & 0x1fff,
        auto_min_alloc_wm_enable: display.display_ver >= 30 && raw & (1 << 29) != 0,
        ..WmLevel::default()
    }
}
// upstream: skl_watermark.c skl_pipe_wm_get_hw_state()
pub fn skl_pipe_wm_get_hw_state(
    io: &impl WmRegisterIo,
    display: &DisplayCaps,
    pipe: usize,
    num_levels: usize,
) -> PipeWm {
    let mut out = PipeWm::default();
    for plane in 0..PLANES {
        let wm = &mut out.planes[plane];
        for level in 0..num_levels.min(WM_LEVELS) {
            let raw = if plane == 5 {
                io.read_cursor_wm(pipe, level)
            } else {
                io.read_plane_wm(pipe, plane, level)
            };
            wm.levels[level] = skl_wm_level_from_reg_val(display, raw);
        }
        let raw = if plane == 5 {
            io.read_cursor_trans_wm(pipe)
        } else {
            io.read_plane_trans_wm(pipe, plane)
        };
        wm.trans_wm = skl_wm_level_from_reg_val(display, raw);
        if display.has_hw_sagv_wm {
            let raw = if plane == 5 {
                io.read_cursor_sagv_wm(pipe)
            } else {
                io.read_plane_sagv_wm(pipe, plane)
            };
            wm.sagv_wm0 = skl_wm_level_from_reg_val(display, raw);
            let raw = if plane == 5 {
                io.read_cursor_sagv_trans_wm(pipe)
            } else {
                io.read_plane_sagv_trans_wm(pipe, plane)
            };
            wm.sagv_trans_wm = skl_wm_level_from_reg_val(display, raw);
        } else if display.sagv_wm {
            wm.sagv_wm0 = wm.levels[0];
            wm.sagv_trans_wm = wm.trans_wm;
        }
    }
    out
}
// upstream: skl_watermark.c skl_wm_get_hw_state()
pub fn skl_wm_get_hw_state(
    io: &impl WmGlobalReadbackIo,
    display: &DisplayCaps,
    crtcs: &mut [CrtcWmReadback; 4],
    num_levels: usize,
) -> DbufState {
    let mut state = DbufState {
        joined_mbus: display.has_mbus_joining && io.joined_mbus(),
        mdclk_cdclk_ratio: io.mdclk_cdclk_ratio(),
        ..DbufState::default()
    };
    for c in crtcs.iter() {
        if c.active {
            state.active_pipes |= 1 << c.pipe;
        }
    }
    for c in crtcs.iter_mut() {
        let pipe = c.pipe;
        c.optimal = if c.active {
            skl_pipe_wm_get_hw_state(io, display, pipe, num_levels)
        } else {
            PipeWm::default()
        };
        c.raw = c.optimal;
        c.ddb = DdbEntry::default();
        c.plane_ddb = [DdbEntry::default(); PLANES];
        c.plane_ddb_y = [DdbEntry::default(); PLANES];
        if c.active {
            let (ddb, ddb_y, min, interim) = skl_pipe_ddb_get_hw_state(io, display, pipe);
            c.plane_ddb = ddb;
            c.plane_ddb_y = ddb_y;
            c.plane_min_ddb = min;
            c.plane_interim_ddb = interim;
            for p in 0..PLANES {
                skl_ddb_entry_union(&mut c.ddb, ddb[p]);
                skl_ddb_entry_union(&mut c.ddb, ddb_y[p]);
            }
        }
        state.ddb[pipe] = c.ddb;
        state.weight[pipe] = intel_crtc_ddb_weight(c.active, c.hdisplay);
        let slices = skl_compute_dbuf_slices(display, pipe, state.active_pipes, state.joined_mbus);
        let offset = mbus_ddb_offset(
            slices,
            intel_dbuf_slice_size(io.total_dbuf_size(), io.display_slice_mask()),
        );
        let absolute = DdbEntry {
            start: offset + c.ddb.start,
            end: offset + c.ddb.end,
        };
        c.ddb = absolute;
        state.slices[pipe] = skl_ddb_dbuf_slice_mask(
            absolute,
            intel_dbuf_slice_size(io.total_dbuf_size(), io.display_slice_mask()),
        ) as u8;
    }
    state.enabled_slices = io.enabled_slices();
    state
}
// upstream: skl_watermark.c skl_watermark_ipc_enabled()
pub fn skl_watermark_ipc_enabled(io: &impl WatermarkFramework) -> bool {
    io.ipc_enabled()
}
// upstream: skl_watermark.c skl_watermark_ipc_update()
pub fn skl_watermark_ipc_update(io: &impl WatermarkFramework, has_ipc: bool, enabled: bool) {
    if has_ipc {
        io.set_ipc_enabled(enabled);
    }
}
// upstream: skl_watermark.c skl_watermark_ipc_can_enable()
pub fn skl_watermark_ipc_can_enable(display: &DisplayCaps, dram_symmetric: bool) -> bool {
    if display.skylake {
        return false;
    }
    if display.kabylake || display.coffeelake || display.cometlake {
        return dram_symmetric;
    }
    true
}
// upstream: skl_watermark.c skl_watermark_ipc_init()
pub fn skl_watermark_ipc_init(
    io: &impl WatermarkFramework,
    has_ipc: bool,
    display: &DisplayCaps,
    dram_symmetric: bool,
) -> bool {
    if !has_ipc {
        return false;
    }
    let enabled = skl_watermark_ipc_can_enable(display, dram_symmetric);
    skl_watermark_ipc_update(io, has_ipc, enabled);
    enabled
}
// upstream: skl_watermark.c multiply_wm_latency()
pub fn multiply_wm_latency(latency: &mut [u32; WM_LEVELS], levels: usize, mult: u32) {
    for value in latency.iter_mut().take(levels.min(WM_LEVELS)) {
        *value = value.wrapping_mul(mult);
    }
}
// upstream: skl_watermark.c increase_wm_latency()
pub fn increase_wm_latency(latency: &mut [u32; WM_LEVELS], levels: usize, inc: u32) {
    increase_latency_values(latency, levels, inc);
}
// upstream: skl_watermark.c need_16gb_dimm_wa()
pub fn need_16gb_dimm_wa(display: &DisplayCaps) -> bool {
    (display.skylake
        || display.kabylake
        || display.coffeelake
        || display.cometlake
        || display.display_ver == 11)
        && display.has_16gb_dimms
}
// upstream: skl_watermark.c wm_read_latency()
pub fn wm_read_latency(display: &DisplayCaps) -> u32 {
    if display.display_ver >= 14 {
        6
    } else if display.display_ver >= 12 {
        3
    } else {
        2
    }
}
// upstream: skl_watermark.c sanitize_wm_latency()
pub fn sanitize_wm_latency(display: &DisplayCaps, latency: &mut [u32; WM_LEVELS], levels: usize) {
    let levels = levels.min(WM_LEVELS);
    if display.display_ver >= 35 && levels > 0 {
        latency[0] = 0;
    }
    let mut level = 1;
    while level < levels && latency[level] != 0 {
        level += 1;
    }
    for v in latency
        .iter_mut()
        .take(levels)
        .skip((level + 1).min(levels))
    {
        *v = 0;
    }
}
// upstream: skl_watermark.c make_wm_latency_monotonic()
pub fn make_wm_latency_monotonic(latency: &mut [u32; WM_LEVELS], levels: usize) {
    for i in 1..levels.min(WM_LEVELS) {
        if latency[i] == 0 {
            break;
        }
        latency[i] = max(latency[i], latency[i - 1]);
    }
}
pub fn increase_latency_values(latency: &mut [u32; WM_LEVELS], levels: usize, inc: u32) {
    if levels == 0 {
        return;
    }
    latency[0] = latency[0].wrapping_add(inc);
    for l in 1..levels.min(WM_LEVELS) {
        if latency[l] == 0 {
            break;
        }
        latency[l] = latency[l].wrapping_add(inc);
    }
}
// upstream: skl_watermark.c adjust_wm_latency()
pub fn adjust_wm_latency(display: &DisplayCaps, latency: &mut [u32; WM_LEVELS], levels: usize) {
    if display.dg2 {
        multiply_wm_latency(latency, levels, 2);
    }
    sanitize_wm_latency(display, latency, levels);
    make_wm_latency_monotonic(latency, levels);
    if latency[0] == 0 {
        increase_wm_latency(latency, levels, wm_read_latency(display));
    }
    if need_16gb_dimm_wa(display) {
        increase_wm_latency(latency, levels, 1);
    }
}
pub trait WmLatencyIo {
    fn read_mtl_latency_reg(&self, index: usize) -> u32;
    fn read_skl_latency_pcode(&self, index: u32) -> Result<u32, i32>;
}
pub trait WmRegisterIo {
    fn read_plane_wm(&self, pipe: usize, plane: usize, level: usize) -> u32;
    fn read_cursor_wm(&self, pipe: usize, level: usize) -> u32;
    fn read_plane_trans_wm(&self, pipe: usize, plane: usize) -> u32;
    fn read_cursor_trans_wm(&self, pipe: usize) -> u32;
    fn read_plane_sagv_wm(&self, pipe: usize, plane: usize) -> u32;
    fn read_cursor_sagv_wm(&self, pipe: usize) -> u32;
    fn read_plane_sagv_trans_wm(&self, pipe: usize, plane: usize) -> u32;
    fn read_cursor_sagv_trans_wm(&self, pipe: usize) -> u32;
    fn plane_ddb(&self, pipe: usize, plane: usize) -> u32;
    fn cursor_ddb(&self, pipe: usize) -> u32;
    fn plane_min_ddb(&self, pipe: usize, plane: usize) -> u32;
    fn dbuf_powered(&self, slice: usize) -> bool;
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CrtcWmReadback {
    pub pipe: usize,
    pub active: bool,
    pub hdisplay: u32,
    pub optimal: PipeWm,
    pub raw: PipeWm,
    pub ddb: DdbEntry,
    pub plane_ddb: [DdbEntry; PLANES],
    pub plane_ddb_y: [DdbEntry; PLANES],
    pub plane_min_ddb: [u16; PLANES],
    pub plane_interim_ddb: [u16; PLANES],
}
pub trait WmGlobalReadbackIo: WmRegisterIo {
    fn joined_mbus(&self) -> bool;
    fn mdclk_cdclk_ratio(&self) -> u8;
    fn enabled_slices(&self) -> u8;
    fn total_dbuf_size(&self) -> u16;
    fn display_slice_mask(&self) -> u8;
}
pub trait WmVerifyIo {
    fn report_wm_mismatch(
        &self,
        plane: usize,
        name: &'static str,
        expected: WmLevel,
        actual: WmLevel,
    );
    fn report_ddb_mismatch(
        &self,
        plane: usize,
        name: &'static str,
        expected: DdbEntry,
        actual: DdbEntry,
    );
    fn report_slice_mismatch(&self, _expected: u8, _actual: u8) {}
}
pub trait PkgcIo {
    fn flipq_exec_time_us(&self) -> u32;
    fn program_pkgc_latency(&self, latency: u32, added_wake_time: u32);
    fn lock_wm_mutex(&self) {}
    fn unlock_wm_mutex(&self) {}
}
pub trait DbufAtomicStateIo {
    fn get_dbuf_state(&mut self) -> Result<&mut DbufState, i32>;
}
pub trait WatermarkDebugfsIo {
    fn register_ipc_status(&mut self, mode: u16);
    fn register_sagv_status(&mut self, mode: u16);
}
pub trait WatermarkDebugLog {
    fn debug_enabled(&self) -> bool;
    fn ddb_change(
        &self,
        pipe: usize,
        plane: usize,
        name: &'static str,
        old: DdbEntry,
        new: DdbEntry,
    );
    fn wm_change(&self, pipe: usize, plane: usize, old: PlaneWm, new: PlaneWm, display_ver: u8);
}
pub trait WmInitIo: WmLatencyIo + WatermarkFramework {
    fn register_wm_functions(&self);
}
pub trait IpcStatusIo: WatermarkFramework {
    fn with_display_runtime_pm(&self, enable: bool) -> Result<(), i32>;
}
// upstream: skl_watermark.c mtl_read_wm_latency()
pub fn mtl_read_wm_latency(io: &impl WmLatencyIo, latency: &mut [u32; WM_LEVELS]) {
    for i in 0..3 {
        let val = io.read_mtl_latency_reg(i);
        latency[i * 2] = val & 0x1fff;
        latency[i * 2 + 1] = (val >> 16) & 0x1fff;
    }
}
// upstream: skl_watermark.c skl_read_wm_latency()
pub fn skl_read_wm_latency(
    io: &impl WmLatencyIo,
    latency: &mut [u32; WM_LEVELS],
) -> Result<(), i32> {
    let val = io.read_skl_latency_pcode(0)?;
    for i in 0..4 {
        latency[i] = ((val >> (i * 8)) & 0xff) as u32;
    }
    let val = io.read_skl_latency_pcode(1)?;
    for i in 0..4 {
        latency[i + 4] = ((val >> (i * 8)) & 0xff) as u32;
    }
    Ok(())
}
// upstream: skl_watermark.c skl_setup_wm_latency()
pub fn skl_setup_wm_latency(
    io: &impl WmLatencyIo,
    display: &DisplayCaps,
    latency: &mut [u32; WM_LEVELS],
) -> Result<usize, i32> {
    let levels = if display.sagv_wm { 6 } else { 8 };
    if display.display_ver >= 14 {
        mtl_read_wm_latency(io, latency);
    } else {
        skl_read_wm_latency(io, latency)?;
    }
    adjust_wm_latency(display, latency, levels);
    Ok(levels)
}
// upstream: skl_watermark.c intel_dbuf_duplicate_state()
pub fn intel_dbuf_duplicate_state(state: &DbufState) -> DbufState {
    *state
}
// upstream: skl_watermark.c intel_dbuf_destroy_state()
pub fn intel_dbuf_destroy_state(_state: DbufState) {}
// upstream: skl_watermark.c intel_atomic_get_dbuf_state()
pub fn intel_atomic_get_dbuf_state<'a>(
    io: &'a mut impl DbufAtomicStateIo,
) -> Result<&'a mut DbufState, i32> {
    io.get_dbuf_state()
}
// upstream: skl_watermark.c intel_dbuf_init()
pub fn intel_dbuf_init() -> DbufState {
    DbufState::default()
}
// upstream: skl_watermark.c xelpdp_is_only_pipe_per_dbuf_bank()
pub fn xelpdp_is_only_pipe_per_dbuf_bank(pipe: usize, active_pipes: u8) -> bool {
    let mask = match pipe {
        0 | 3 => active_pipes & (A | D),
        1 | 2 => active_pipes & (B | C),
        _ => return false,
    };
    mask.is_power_of_two()
}
// upstream: skl_watermark.c pipe_mbus_dbox_ctl()
pub fn pipe_mbus_dbox_ctl(
    io: &impl DbufRegisterIo,
    display: &DisplayCaps,
    pipe: usize,
    dbuf_state: &DbufState,
) -> u32 {
    let mut val = 0;
    if display.display_ver >= 14 {
        val |= io.mbus_dbox_i_credit(2);
    }
    if display.display_ver >= 12 {
        val |= io.mbus_dbox_b2b_max(16) | io.mbus_dbox_b2b_delay(1) | io.mbus_dbox_regulate_b2b();
    }
    if display.display_ver >= 14 {
        val |= io.mbus_dbox_a_credit(if dbuf_state.joined_mbus { 12 } else { 8 });
    } else if display.wa_22010947358 {
        val |= io.mbus_dbox_a_credit(if dbuf_state.joined_mbus { 6 } else { 4 });
    } else {
        val |= io.mbus_dbox_a_credit(2);
    }
    if display.display_ver >= 14 {
        val |= io.mbus_dbox_b_credit(0xa);
    } else if display.alderlake_p {
        val |= io.mbus_dbox_bw_credit(2) | io.mbus_dbox_b_credit(8);
    } else if display.display_ver >= 12 {
        val |= io.mbus_dbox_bw_credit(2) | io.mbus_dbox_b_credit(12);
    } else {
        val |= io.mbus_dbox_bw_credit(1) | io.mbus_dbox_b_credit(8);
    }
    if display.display_verx100 == 1400 {
        val |= if xelpdp_is_only_pipe_per_dbuf_bank(pipe, dbuf_state.active_pipes) {
            io.mbus_dbox_bw_8credits_mtl()
        } else {
            io.mbus_dbox_bw_4credits_mtl()
        };
    }
    val
}
// upstream: skl_watermark.c pipe_mbus_dbox_ctl_update()
pub fn pipe_mbus_dbox_ctl_update(
    io: &impl DbufRegisterIo,
    display: &DisplayCaps,
    dbuf_state: &DbufState,
) {
    for pipe in 0..4 {
        if dbuf_state.active_pipes & (1 << pipe) != 0 {
            io.write_pipe_mbus_dbox_ctl(pipe, pipe_mbus_dbox_ctl(io, display, pipe, dbuf_state));
        }
    }
}
// upstream: skl_watermark.c intel_mbus_dbox_update()
pub fn intel_mbus_dbox_update(
    io: &impl DbufRegisterIo,
    display: &DisplayCaps,
    old: &DbufState,
    new: &DbufState,
) {
    if display.display_ver < 11
        || (new.joined_mbus == old.joined_mbus && new.active_pipes == old.active_pipes)
    {
        return;
    }
    pipe_mbus_dbox_ctl_update(io, display, new);
}
// upstream: skl_watermark.c intel_dbuf_state_set_mdclk_cdclk_ratio()
pub fn intel_dbuf_state_set_mdclk_cdclk_ratio(
    io: &mut impl DdbAtomicFramework,
    state: &mut DbufState,
    ratio: u8,
) -> Result<(), i32> {
    state.mdclk_cdclk_ratio = ratio;
    io.lock_global_state()
}
// upstream: skl_watermark.c intel_dbuf_mdclk_cdclk_ratio_update()
pub fn intel_dbuf_mdclk_cdclk_ratio_update(
    io: &impl DbufRegisterIo,
    display: &DisplayCaps,
    ratio: u8,
    joined_mbus: bool,
) {
    if !display.has_mbus_joining {
        return;
    }
    let mut ratio = ratio as u32;
    if display.display_ver >= 35 {
        io.update_mbus_translation_throttle((ratio as u32).wrapping_sub(1), true);
    } else if display.display_ver >= 20 {
        io.update_mbus_translation_throttle((ratio as u32).wrapping_sub(1), false);
    }
    if joined_mbus {
        ratio *= 2;
    }
    for slice in 0..8 {
        if io.available_dbuf_slices() & (1 << slice) != 0 {
            io.update_dbuf_min_tracker(slice, ratio.wrapping_sub(1), display.display_ver >= 35);
        }
    }
}
// upstream: skl_watermark.c intel_dbuf_mdclk_min_tracker_update()
pub fn intel_dbuf_mdclk_min_tracker_update(
    io: &impl DbufRegisterIo,
    display: &DisplayCaps,
    old: &DbufState,
    new: &DbufState,
    cdclk_decreasing_later: bool,
) {
    let ratio = if cdclk_decreasing_later {
        old.mdclk_cdclk_ratio
    } else {
        new.mdclk_cdclk_ratio
    };
    intel_dbuf_mdclk_cdclk_ratio_update(io, display, ratio, new.joined_mbus);
}
// upstream: skl_watermark.c intel_mbus_joined_pipe()
pub fn intel_mbus_joined_pipe(
    state_active_pipes: u8,
    joined: bool,
    new_pipe_needs_modeset: bool,
) -> Option<usize> {
    if !joined || !state_active_pipes.is_power_of_two() {
        return None;
    }
    let pipe = state_active_pipes.trailing_zeros() as usize;
    if new_pipe_needs_modeset {
        None
    } else {
        Some(pipe)
    }
}
// upstream: skl_watermark.c mbus_ctl_join_update()
pub fn mbus_ctl_join_update(io: &impl DbufRegisterIo, dbuf_state: &DbufState, pipe: Option<usize>) {
    io.update_mbus_ctl(dbuf_state.joined_mbus, pipe);
}
// upstream: skl_watermark.c intel_dbuf_mbus_join_update()
pub fn intel_dbuf_mbus_join_update(
    io: &impl DbufRegisterIo,
    old: &DbufState,
    new: &DbufState,
    pipe: Option<usize>,
) {
    let _ = old;
    mbus_ctl_join_update(io, new, pipe);
}
// upstream: skl_watermark.c intel_dbuf_mbus_pre_ddb_update()
pub fn intel_dbuf_mbus_pre_ddb_update(
    io: &impl DbufRegisterIo,
    display: &DisplayCaps,
    old: &DbufState,
    new: &DbufState,
    pipe_needs_modeset: bool,
    cdclk_decreasing_later: bool,
) {
    if !old.joined_mbus && new.joined_mbus {
        let pipe = intel_mbus_joined_pipe(new.active_pipes, new.joined_mbus, pipe_needs_modeset);
        debug_assert!(new.changed);
        intel_dbuf_mbus_join_update(io, old, new, pipe);
        intel_mbus_dbox_update(io, display, old, new);
        intel_dbuf_mdclk_min_tracker_update(io, display, old, new, cdclk_decreasing_later);
    }
}
// upstream: skl_watermark.c intel_dbuf_mbus_post_ddb_update()
pub fn intel_dbuf_mbus_post_ddb_update(
    io: &impl DbufRegisterIo,
    display: &DisplayCaps,
    old: &DbufState,
    new: &DbufState,
    pipe_needs_modeset: bool,
    cdclk_decreasing_later: bool,
) {
    if old.joined_mbus && !new.joined_mbus {
        let pipe = intel_mbus_joined_pipe(old.active_pipes, old.joined_mbus, pipe_needs_modeset);
        debug_assert!(new.changed);
        intel_dbuf_mdclk_min_tracker_update(io, display, old, new, cdclk_decreasing_later);
        intel_mbus_dbox_update(io, display, old, new);
        intel_dbuf_mbus_join_update(io, old, new, pipe);
        if let Some(p) = pipe {
            io.wait_next_vblank(p);
        }
    } else if old.joined_mbus == new.joined_mbus && old.active_pipes != new.active_pipes {
        debug_assert!(new.changed);
        intel_dbuf_mdclk_min_tracker_update(io, display, old, new, cdclk_decreasing_later);
        intel_mbus_dbox_update(io, display, old, new);
    }
}
// upstream: skl_watermark.c intel_dbuf_pre_plane_update()
pub fn intel_dbuf_pre_plane_update(io: &impl DbufRegisterIo, old: &DbufState, new: &DbufState) {
    let old_slices = old.enabled_slices;
    let new_slices = old.enabled_slices | new.enabled_slices;
    if old_slices != new_slices {
        debug_assert!(new.changed);
        io.update_dbuf_slices(new_slices);
    }
}
// upstream: skl_watermark.c intel_dbuf_post_plane_update()
pub fn intel_dbuf_post_plane_update(io: &impl DbufRegisterIo, old: &DbufState, new: &DbufState) {
    let old_slices = old.enabled_slices | new.enabled_slices;
    let new_slices = new.enabled_slices;
    if old_slices != new_slices {
        debug_assert!(new.changed);
        io.update_dbuf_slices(new_slices);
    }
}
// upstream: skl_watermark.c intel_dbuf_num_enabled_slices()
pub fn intel_dbuf_num_enabled_slices(state: &DbufState) -> u32 {
    state.enabled_slices.count_ones()
}
// upstream: skl_watermark.c intel_dbuf_num_active_pipes()
pub fn intel_dbuf_num_active_pipes(state: &DbufState) -> u32 {
    state.active_pipes.count_ones()
}
// upstream: skl_watermark.c intel_dbuf_pmdemand_needs_update()
pub fn intel_dbuf_pmdemand_needs_update(
    display: &DisplayCaps,
    old: &DbufState,
    new: &DbufState,
) -> bool {
    new.active_pipes != old.active_pipes
        || (display.display_ver < 30 && new.enabled_slices != old.enabled_slices)
}

// upstream: skl_watermark.c skl_mbus_sanitize()
pub fn skl_mbus_sanitize(io: &impl DbufRegisterIo, display: &DisplayCaps, state: &mut DbufState) {
    if !display.has_mbus_joining || !state.joined_mbus || adlp_check_mbus_joined(state.active_pipes)
    {
        return;
    }
    state.joined_mbus = false;
    intel_dbuf_mdclk_cdclk_ratio_update(io, display, state.mdclk_cdclk_ratio, false);
    pipe_mbus_dbox_ctl_update(io, display, state);
    mbus_ctl_join_update(io, state, None);
}
// upstream: skl_watermark.c skl_dbuf_is_misconfigured()
pub fn skl_dbuf_is_misconfigured(
    display: &DisplayCaps,
    state: &DbufState,
    crtc_ddb_abs: &[DdbEntry; 4],
) -> bool {
    for pipe in 0..4 {
        let allowed = skl_compute_dbuf_slices(display, pipe, state.active_pipes, state.joined_mbus);
        if state.slices[pipe] & !allowed != 0
            || skl_ddb_allocation_overlaps(crtc_ddb_abs[pipe], crtc_ddb_abs, pipe)
        {
            return true;
        }
    }
    false
}
// upstream: skl_watermark.c skl_dbuf_sanitize()
pub fn skl_dbuf_sanitize(
    io: &mut impl DbufSanitizeIo,
    display: &DisplayCaps,
    state: &DbufState,
    crtc_ddb_abs: &mut [DdbEntry; 4],
) {
    if !skl_dbuf_is_misconfigured(display, state, crtc_ddb_abs) {
        return;
    }
    io.disable_visible_planes();
    io.warn(io.active_planes() != 0);
    *crtc_ddb_abs = [DdbEntry::default(); 4];
}
// upstream: skl_watermark.c skl_wm_sanitize()
pub fn skl_wm_sanitize(
    io: &mut impl DbufSanitizeIo,
    regs: &impl DbufRegisterIo,
    display: &DisplayCaps,
    state: &mut DbufState,
    crtc_ddb_abs: &mut [DdbEntry; 4],
) {
    skl_mbus_sanitize(regs, display, state);
    skl_dbuf_sanitize(io, display, state, crtc_ddb_abs);
}
// upstream: skl_watermark.c skl_wm_crtc_disable_noatomic()
pub fn skl_wm_crtc_disable_noatomic(
    display: &DisplayCaps,
    state: &mut DbufState,
    pipe: usize,
    ddb_abs: &mut DdbEntry,
) {
    if display.display_ver < 9 {
        return;
    }
    state.active_pipes &= !(1 << pipe);
    state.weight[pipe] = 0;
    state.slices[pipe] = 0;
    state.ddb[pipe] = DdbEntry::default();
    *ddb_abs = DdbEntry::default();
}
// upstream: skl_watermark.c skl_wm_plane_disable_noatomic()
pub fn skl_wm_plane_disable_noatomic(
    display: &DisplayCaps,
    model: &mut PlaneDdbModel,
    plane: usize,
) {
    if display.display_ver < 9 {
        return;
    }
    model.plane_ddb[plane] = DdbEntry::default();
    model.plane_ddb_y[plane] = DdbEntry::default();
    model.plane_min_ddb[plane] = 0;
    model.plane_interim_ddb[plane] = 0;
    model.raw_planes[plane] = PlaneWm::default();
    model.planes[plane] = PlaneWm::default();
}
// upstream: skl_watermark.c skl_wm_level_verify()
pub fn skl_wm_level_verify(
    io: &impl WmVerifyIo,
    plane: usize,
    name: &'static str,
    hw: WmLevel,
    sw: WmLevel,
) {
    if !skl_wm_level_equals(hw, sw) {
        io.report_wm_mismatch(plane, name, sw, hw);
    }
}
// upstream: skl_watermark.c skl_ddb_entry_verify()
pub fn skl_ddb_entry_verify(
    io: &impl WmVerifyIo,
    plane: usize,
    name: &'static str,
    hw: DdbEntry,
    sw: DdbEntry,
) {
    if !hw.equal(sw) {
        io.report_ddb_mismatch(plane, name, sw, hw);
    }
}
// upstream: skl_watermark.c intel_wm_state_verify()
pub fn intel_wm_state_verify(
    io: &impl WmVerifyIo,
    registers: &impl WmRegisterIo,
    display: &DisplayCaps,
    crtc: &CrtcWmReadback,
    sw: &PipeWm,
    plane_ddb: &[DdbEntry; PLANES],
    plane_ddb_y: &[DdbEntry; PLANES],
    enabled_slices: u8,
    num_levels: usize,
) {
    if display.display_ver < 9 || !crtc.active {
        return;
    }
    let hw = skl_pipe_wm_get_hw_state(registers, display, crtc.pipe, num_levels);
    let (hw_ddb, hw_ddb_y, ..) = skl_pipe_ddb_get_hw_state(registers, display, crtc.pipe);
    let actual_slices = intel_enabled_dbuf_slices_mask(registers, 8);
    if display.display_ver >= 11 && actual_slices != enabled_slices {
        io.report_slice_mismatch(enabled_slices, actual_slices);
    }
    for plane in 0..PLANES {
        for level in 0..num_levels.min(WM_LEVELS) {
            let name = if level == 0 {
                "WM0"
            } else if level == 1 {
                "WM1"
            } else if level == 2 {
                "WM2"
            } else if level == 3 {
                "WM3"
            } else if level == 4 {
                "WM4"
            } else if level == 5 {
                "WM5"
            } else if level == 6 {
                "WM6"
            } else {
                "WM7"
            };
            skl_wm_level_verify(
                io,
                plane,
                name,
                hw.planes[plane].levels[level],
                skl_plane_wm_level(sw, plane, level),
            );
        }
        skl_wm_level_verify(
            io,
            plane,
            "trans WM",
            hw.planes[plane].trans_wm,
            skl_plane_trans_wm(sw, plane),
        );
        if display.has_hw_sagv_wm {
            skl_wm_level_verify(
                io,
                plane,
                "SAGV WM",
                hw.planes[plane].sagv_wm0,
                sw.planes[plane].sagv_wm0,
            );
            skl_wm_level_verify(
                io,
                plane,
                "SAGV trans WM",
                hw.planes[plane].sagv_trans_wm,
                sw.planes[plane].sagv_trans_wm,
            );
        }
        skl_ddb_entry_verify(io, plane, "DDB", hw_ddb[plane], plane_ddb[plane]);
        skl_ddb_entry_verify(io, plane, "DDB Y", hw_ddb_y[plane], plane_ddb_y[plane]);
    }
}
// upstream: skl_watermark.c skl_wm_init()
pub fn skl_wm_init(
    io: &impl WmInitIo,
    display: &DisplayCaps,
    sagv: &mut SagvInfo,
    latencies: &mut [u32; WM_LEVELS],
) -> Result<usize, i32> {
    intel_sagv_init(io, display, sagv);
    let levels = skl_setup_wm_latency(io, display, latencies)?;
    io.register_wm_functions();
    Ok(levels)
}
// upstream: skl_watermark.c skl_watermark_ipc_status_show()
pub fn skl_watermark_ipc_status_show(ipc_enabled: bool) -> bool {
    ipc_enabled
}
// upstream: skl_watermark.c skl_watermark_ipc_status_open()
pub fn skl_watermark_ipc_status_open() -> bool {
    true
}
// upstream: skl_watermark.c skl_watermark_ipc_status_write()
pub fn skl_watermark_ipc_status_write(
    io: &impl IpcStatusIo,
    has_ipc: bool,
    current: &mut bool,
    input: &[u8],
) -> Result<usize, i32> {
    let text = core::str::from_utf8(input).map_err(|_| -22)?.trim();
    let enable = match text {
        "1" | "y" | "Y" | "yes" | "Yes" | "YES" | "true" | "True" | "TRUE" | "on" | "On" | "ON" => {
            true
        }
        "0" | "n" | "N" | "no" | "No" | "NO" | "false" | "False" | "FALSE" | "off" | "Off"
        | "OFF" => false,
        _ => return Err(-22),
    };
    io.with_display_runtime_pm(enable)?;
    *current = enable;
    if has_ipc {
        skl_watermark_ipc_update(io, has_ipc, enable);
    }
    Ok(input.len())
}
// upstream: skl_watermark.c intel_sagv_status_show()
pub fn intel_sagv_status_show(
    display: &DisplayCaps,
    sagv: &SagvInfo,
) -> (&'static str, bool, bool, u32) {
    let status = match sagv.status {
        SagvStatus::Unknown => "unknown",
        SagvStatus::Disabled => "disabled",
        SagvStatus::Enabled => "enabled",
        SagvStatus::NotControlled => "not controlled",
    };
    (
        status,
        intel_has_sagv(display, sagv),
        display.params_enable_sagv,
        sagv.block_time_us,
    )
}
// upstream: skl_watermark.c skl_watermark_debugfs_register()
pub fn skl_watermark_debugfs_register(
    io: &mut impl WatermarkDebugfsIo,
    has_ipc: bool,
    has_sagv: bool,
) {
    if has_ipc {
        io.register_ipc_status(0o644);
    }
    if has_sagv {
        io.register_sagv_status(0o444);
    }
}
// upstream: skl_watermark.c skl_watermark_max_latency()
pub fn skl_watermark_max_latency(
    display: &DisplayCaps,
    wm_latency: &[u32; WM_LEVELS],
    initial_wm_level: usize,
) -> u32 {
    for level in (initial_wm_level..WM_LEVELS).rev() {
        let latency = skl_wm_latency(display, wm_latency, level, None, false);
        if latency != 0 {
            return latency;
        }
    }
    0
}

#[cfg(test)]
mod source_primary_plane_tests {
    use super::*;

    struct PcodeLatency;
    impl WmLatencyIo for PcodeLatency {
        fn read_mtl_latency_reg(&self, _: usize) -> u32 {
            unreachable!("display-13 uses PCode latency reads")
        }

        fn read_skl_latency_pcode(&self, index: u32) -> Result<u32, i32> {
            match index {
                0 => Ok(0x0806_0402),
                1 => Ok(0x1412_100e),
                _ => Err(-22),
            }
        }
    }

    #[test]
    fn display13_primary_wm_uses_source_latency_levels_and_sagv_fields() {
        let display = DisplayCaps {
            display_ver: 13,
            display_ver_fixed: 13,
            alderlake_p: true,
            sagv: true,
            sagv_wm: true,
            has_hw_sagv_wm: true,
            ..DisplayCaps::default()
        };
        assert_eq!(skl_wm_max_lines(&display), 255);

        let input = PlaneWmInput {
            width: 1920,
            cpp: 4,
            pixel_rate: 148_500,
            pipe_htotal: 2200,
            num_format_planes: 1,
            visible: true,
            ..PlaneWmInput::default()
        };
        let mut latencies = [0u32; WM_LEVELS];
        let num_levels = skl_setup_wm_latency(&PcodeLatency, &display, &mut latencies).unwrap();
        assert_eq!(num_levels, 6);
        assert_eq!(&latencies[..6], &[2, 4, 6, 8, 14, 16]);

        let mut wm = PlaneWm::default();
        skl_build_plane_wm_single(
            &display, &input, 0, 0, num_levels, &latencies, false, false, 30, &mut wm,
        )
        .unwrap();

        assert!(wm.levels[0].enable);
        assert!(wm.levels[0].blocks > 0);
        assert!(wm.levels[0].lines > 0);
        assert!(wm.levels.iter().all(|level| level.lines <= 255));
        assert!(wm.sagv_wm0.enable);
        assert!(wm.levels[0].min_ddb_alloc <= 4096);
    }
}
