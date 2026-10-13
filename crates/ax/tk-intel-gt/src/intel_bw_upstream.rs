// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// Translation of the Alder Lake-P paths in Linux v7.2.3 intel_bw.c.
// The active target is display IP 13; other i915 generations are not enabled
// by TheKernel's display driver.
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::{mem::size_of, ptr};

use tk_drm_upstream::uapi_drm_consts::DRM_MODE_FLAG_INTERLACE;
use tk_linux_kapi::memory::{kfree, kzalloc_obj};

use crate::{
    intel_display_conversion_upstream::__drm_to_display,
    intel_display_core_types_upstream::{IntelBwInfo, IntelDisplay},
    intel_display_device_types_upstream::IntelDisplayPlatforms,
    intel_display_device_upstream::intel_display_device_present,
    intel_display_types_upstream::{
        IntelAtomicState, IntelCrtc, IntelCrtcState, IntelCrtcWmState, IntelCrtcWmStateAnon0x0Skl,
        SklPipeWm, SklPlaneWm,
    },
    intel_dram_types_upstream::{
        DramInfo, INTEL_DRAM_DDR4, INTEL_DRAM_DDR5, INTEL_DRAM_LPDDR4, INTEL_DRAM_LPDDR5,
    },
    intel_global_state_types_upstream::{IntelGlobalObj, IntelGlobalState, IntelGlobalStateFuncs},
    intel_parent_upstream::{intel_parent_pcode_read, intel_parent_pcode_request},
};

const ENOMEM: i32 = 12;
const EINVAL: i32 = 22;
const I915_MAX_PIPES: usize = 4;
const I915_MAX_QGV_POINTS: usize = 8;
const I915_MAX_PSF_GV_POINTS: usize = 3;
const I915_MAX_BW_GROUPS: usize = 6;
const PLANE_CURSOR: usize = 7;
const DEPROGBWPCLIMIT: u32 = 60;

const I915_SAGV_UNKNOWN: u32 = 0;
const I915_SAGV_DISABLED: u32 = 1;
const I915_SAGV_ENABLED: u32 = 2;
const I915_SAGV_NOT_CONTROLLED: u32 = 3;

const ICL_PCODE_MEM_SUBSYSYSTEM_INFO: u32 = 0xd;
const ICL_PCODE_MEM_SS_READ_QGV_POINT_INFO: u32 = 0x100 | (0 << 16);
const ADL_PCODE_MEM_SS_READ_PSF_GV_INFO: u32 = 0x200;
const ICL_PCODE_SAGV_DE_MEM_SS_CONFIG: u32 = 0xe;
const ICL_PCODE_REP_QGV_MASK: u32 = 0x3;
const ICL_PCODE_REP_QGV_SAFE: u32 = 0;
const ADLS_PCODE_REP_PSF_MASK: u32 = 0xc;
const ADLS_PCODE_REP_PSF_SAFE: u32 = 0;
const ICL_PCODE_REQ_QGV_PT_MASK: u32 = 0xff;
const ADLS_PCODE_REQ_PSF_PT_MASK: u32 = 0x700;

#[repr(C)]
pub struct IntelBwState {
    base: IntelGlobalState,
    pipe_sagv_reject: u8,
    qgv_point_peakbw: u16,
    qgv_points_mask: u16,
    data_rate: [u32; I915_MAX_PIPES],
    num_active_planes: [u8; I915_MAX_PIPES],
}

#[derive(Clone, Copy, Default)]
struct IntelQgvPoint {
    dclk: u16,
    t_rp: u16,
    t_rdpre: u16,
    t_rc: u16,
    t_ras: u16,
    t_rcd: u16,
}

#[derive(Clone, Copy, Default)]
struct IntelPsfGvPoint {
    clk: u8,
}

#[derive(Clone, Copy)]
struct IntelQgvInfo {
    points: [IntelQgvPoint; I915_MAX_QGV_POINTS],
    psf_points: [IntelPsfGvPoint; I915_MAX_PSF_GV_POINTS],
    num_points: usize,
    num_psf_points: usize,
    t_bl: u32,
    max_numchannels: u32,
    channel_width: u32,
    deinterleave: u32,
}

impl Default for IntelQgvInfo {
    fn default() -> Self {
        Self {
            points: [IntelQgvPoint::default(); I915_MAX_QGV_POINTS],
            psf_points: [IntelPsfGvPoint::default(); I915_MAX_PSF_GV_POINTS],
            num_points: 0,
            num_psf_points: 0,
            t_bl: 0,
            max_numchannels: 0,
            channel_width: 0,
            deinterleave: 0,
        }
    }
}

#[inline]
fn err_ptr<T>(err: i32) -> *mut T {
    err as isize as *mut T
}

#[inline]
fn is_err<T>(ptr: *const T) -> bool {
    (ptr as usize) >= (!4095usize)
}

#[inline]
unsafe fn display_from_atomic(state: *mut IntelAtomicState) -> *mut IntelDisplay {
    if state.is_null() {
        return ptr::null_mut();
    }
    unsafe { __drm_to_display((*state).base.dev) }
}

#[inline]
unsafe fn display_from_crtc(crtc: *mut IntelCrtc) -> *mut IntelDisplay {
    if crtc.is_null() {
        return ptr::null_mut();
    }
    unsafe { __drm_to_display((*crtc).base.dev) }
}

#[inline]
unsafe fn display_from_crtc_state(state: *const IntelCrtcState) -> *mut IntelDisplay {
    if state.is_null() || unsafe { (*state).uapi.crtc.is_null() } {
        return ptr::null_mut();
    }
    unsafe { __drm_to_display((*(*state).uapi.crtc).dev) }
}

#[inline]
unsafe fn display_ver(display: *const IntelDisplay) -> u16 {
    unsafe { (*display).info.__runtime_info.ip.ver }
}

#[inline]
unsafe fn platform_bit(platforms: *const IntelDisplayPlatforms, bit: u32) -> bool {
    if platforms.is_null() || bit >= 128 {
        return false;
    }
    let word = unsafe { (*platforms)._anon_0_0.bitmap[(bit / 64) as usize] };
    word & (1u64 << (bit % 64)) != 0
}

#[inline]
unsafe fn is_adlp(display: *const IntelDisplay) -> bool {
    unsafe { platform_bit(ptr::addr_of!((*display).platform), 52) }
}

#[inline]
unsafe fn is_y_tile(display: *const IntelDisplay) -> bool {
    // DISPLAY_VER=13 uses the 4-tile DBUF only on DG2; ADL-P is Y-tiled.
    !unsafe { platform_bit(ptr::addr_of!((*display).platform), 56) }
}

#[inline]
fn display_info(_dram_info: &DramInfo) -> Option<(u32, u32)> {
    // Alder Lake-P SoC parameters and display-IP 12 parameters from Linux.
    Some((38, 20)) // deprogbwlimit (GB/s), derating (percent)
}

#[inline]
fn round_div_closest(numerator: u32, denominator: u32) -> u32 {
    (numerator + denominator / 2) / denominator
}

#[inline]
fn dclk_freq_mhz(ratio: u32) -> u16 {
    round_div_closest(ratio * 100, 6).min(u16::MAX as u32) as u16
}

#[inline]
fn dram_channels(dram: &DramInfo) -> u32 {
    u32::from(dram.num_channels.max(1))
}

unsafe fn read_qgv_point(
    display: *mut IntelDisplay,
    point: usize,
    result: &mut IntelQgvPoint,
) -> i32 {
    let mut val = 0;
    let mut val2 = 0;
    let mbox = ICL_PCODE_MEM_SUBSYSYSTEM_INFO | (1 << 8) | ((point as u32) << 16);
    let ret = unsafe { intel_parent_pcode_read(display, mbox, &mut val, &mut val2) };
    if ret != 0 {
        return ret;
    }
    result.dclk = dclk_freq_mhz(val & 0xffff);
    result.t_rp = ((val >> 16) & 0xff) as u16;
    result.t_rcd = ((val >> 24) & 0xff) as u16;
    result.t_rdpre = (val2 & 0xff) as u16;
    result.t_ras = ((val2 >> 8) & 0xff) as u16;
    result.t_rc = result.t_rp.saturating_add(result.t_ras);
    if result.dclk == 0 { -EINVAL } else { 0 }
}

unsafe fn read_psf_points(display: *mut IntelDisplay, points: &mut [IntelPsfGvPoint]) -> i32 {
    let mut val = 0;
    let ret = unsafe {
        intel_parent_pcode_read(
            display,
            ICL_PCODE_MEM_SUBSYSYSTEM_INFO | ADL_PCODE_MEM_SS_READ_PSF_GV_INFO,
            &mut val,
            ptr::null_mut(),
        )
    };
    if ret != 0 {
        return ret;
    }
    for point in points {
        point.clk = (val & 0xff) as u8;
        val >>= 8;
    }
    0
}

unsafe fn qgv_points_mask(display: *const IntelDisplay) -> u16 {
    let count = unsafe { (*display).bw.max[0].num_qgv_points }.min(8);
    let psf_count = unsafe { (*display).bw.max[0].num_psf_gv_points }.min(3);
    let qgv = if count == 0 { 0 } else { (1u32 << count) - 1 };
    let psf = if psf_count == 0 {
        0
    } else {
        (1u32 << psf_count) - 1
    };
    ((qgv & ICL_PCODE_REQ_QGV_PT_MASK) | ((psf << 8) & ADLS_PCODE_REQ_PSF_PT_MASK)) as u16
}

unsafe fn sagv_enabled_for_mask(display: *const IntelDisplay, points_mask: u16) -> bool {
    let qgv = (!(points_mask as u32))
        & (unsafe { qgv_points_mask(display) } as u32)
        & ICL_PCODE_REQ_QGV_PT_MASK;
    !qgv.is_power_of_two()
}

unsafe fn read_qgv_info(display: *mut IntelDisplay, dram: &DramInfo, qi: &mut IntelQgvInfo) -> i32 {
    qi.num_points = (dram.num_qgv_points as usize).min(I915_MAX_QGV_POINTS);
    qi.num_psf_points = (dram.num_psf_gv_points as usize).min(I915_MAX_PSF_GV_POINTS);
    match dram.r#type {
        INTEL_DRAM_DDR4 => {
            qi.t_bl = if unsafe { is_y_tile(display) } { 8 } else { 4 };
            qi.max_numchannels = 2;
            qi.channel_width = 64;
            qi.deinterleave = if unsafe { is_y_tile(display) } { 1 } else { 2 };
        }
        INTEL_DRAM_DDR5 => {
            qi.t_bl = if unsafe { is_y_tile(display) } { 16 } else { 8 };
            qi.max_numchannels = 4;
            qi.channel_width = 32;
            qi.deinterleave = if unsafe { is_y_tile(display) } { 1 } else { 2 };
        }
        INTEL_DRAM_LPDDR4 | INTEL_DRAM_LPDDR5 => {
            qi.t_bl = 16;
            qi.max_numchannels = 8;
            qi.channel_width = 16;
            qi.deinterleave = if unsafe { is_y_tile(display) } { 2 } else { 4 };
        }
        _ => {
            // Linux falls back to a conservative LPDDR-like channel model on
            // display 12/13 for memory types without specific timing values.
            qi.t_bl = 16;
            qi.max_numchannels = 1;
        }
    }
    for index in 0..qi.num_points {
        let ret = unsafe { read_qgv_point(display, index, &mut qi.points[index]) };
        if ret != 0 {
            return ret;
        }
    }
    if qi.num_psf_points != 0
        && unsafe { read_psf_points(display, &mut qi.psf_points[..qi.num_psf_points]) } != 0
    {
        // Upstream continues with the QGV points if optional PSF data is not
        // readable; the display bandwidth tables remain conservative.
        qi.num_psf_points = 0;
    }
    0
}

fn max_dclk(qi: &IntelQgvInfo) -> u32 {
    qi.points[..qi.num_points]
        .iter()
        .map(|point| point.dclk as u32)
        .max()
        .unwrap_or(0)
}

fn peak_bw(num_channels: u32, channel_width: u32, dclk: u32) -> u32 {
    num_channels * (channel_width / 8) * dclk
}

fn adl_psf_bw(clock: u8) -> u32 {
    round_div_closest(64 * clock as u32 * 100, 6)
}

#[inline]
fn tgl_next_group_num_planes(ipqdepth: u32, clpchgroup: u32, clperchgroup: u32) -> u8 {
    if clpchgroup != 0 && clpchgroup < clperchgroup {
        (ipqdepth.saturating_sub(clpchgroup) / clpchgroup).min(u8::MAX as u32) as u8
    } else {
        0
    }
}

fn tgl_qgv_group_bw(
    point: IntelQgvPoint,
    clpchgroup: u32,
    t_bl: u32,
    channels: u32,
    max_bw: u32,
    derating: u32,
) -> u32 {
    if clpchgroup == 0 {
        return 0;
    }
    let burst_rows = (clpchgroup - 1).saturating_mul(t_bl).min(u16::MAX as u32) as u16;
    let ct = point.t_rc.max(
        point
            .t_rp
            .saturating_add(point.t_rcd)
            .saturating_add(burst_rows)
            .saturating_add(point.t_rdpre),
    );
    if ct == 0 {
        return 0;
    }
    let bw = (point.dclk as u32 * clpchgroup * 32 * channels) / ct as u32;
    max_bw.min(bw * (100 - derating.min(100)) / 100)
}

unsafe fn tgl_get_bw_info(
    display: *mut IntelDisplay,
    dram: &DramInfo,
    qi: &IntelQgvInfo,
    soc_params: (u32, u32),
) {
    let mut channels = dram_channels(dram);
    let mut deinterleave = qi.deinterleave;
    if (dram.r#type == INTEL_DRAM_LPDDR4 || dram.r#type == INTEL_DRAM_LPDDR5)
        && unsafe { display_ver(display) } < 14
    {
        channels *= 2;
    }
    if channels < qi.max_numchannels && unsafe { display_ver(display) } >= 12 {
        deinterleave = (deinterleave / 2).max(1);
    }
    channels = channels.min(qi.max_numchannels.max(1));

    let peakbw = peak_bw(channels, qi.channel_width, max_dclk(qi));
    let maxdebw = (soc_params.0 * 1000).min(peakbw * DEPROGBWPCLIMIT / 100);
    let displayrtids = 256u32;
    let ipqdepth = 16u32.min(displayrtids / channels.max(1));
    let clperchgroup = 4 * (8 / channels.max(1)) * deinterleave;
    for group in 0..I915_MAX_BW_GROUPS {
        let clpchgroup = (16 * deinterleave / channels.max(1)) << group;
        if group < I915_MAX_BW_GROUPS - 1 {
            unsafe {
                (*display).bw.max[group + 1].num_planes =
                    tgl_next_group_num_planes(ipqdepth, clpchgroup, clperchgroup)
            };
        }
        unsafe { (*display).bw.max[group].num_qgv_points = qi.num_points as u8 };
        unsafe { (*display).bw.max[group].num_psf_gv_points = qi.num_psf_points as u8 };
        for qgv in 0..qi.num_points {
            let point = qi.points[qgv];
            let info: &mut IntelBwInfo = unsafe { &mut (*display).bw.max[group] };
            info.deratedbw[qgv] =
                tgl_qgv_group_bw(point, clpchgroup, qi.t_bl, channels, maxdebw, soc_params.1);
            info.peakbw[qgv] = peak_bw(channels, qi.channel_width, point.dclk as u32);
        }
        for psf in 0..qi.num_psf_points {
            unsafe { (*display).bw.max[group].psf_bw[psf] = adl_psf_bw(qi.psf_points[psf].clk) };
        }
    }
    unsafe {
        (*display).sagv.status = if qi.num_points == 1 {
            I915_SAGV_NOT_CONTROLLED
        } else {
            I915_SAGV_ENABLED
        };
    }
}

unsafe fn num_qgv_available(display: *const IntelDisplay) -> usize {
    unsafe { (*display).bw.max[0].num_qgv_points.min(8) as usize }
}

unsafe fn num_psf_available(display: *const IntelDisplay) -> usize {
    unsafe { (*display).bw.max[0].num_psf_gv_points.min(3) as usize }
}

unsafe fn max_bw_index(display: *const IntelDisplay, num_planes: u32, qgv: usize) -> Option<usize> {
    for i in (0..I915_MAX_BW_GROUPS).rev() {
        let info = unsafe { &(*display).bw.max[i] };
        if qgv >= info.num_qgv_points as usize {
            return None;
        }
        if num_planes <= info.num_planes as u32 {
            return Some(i);
        }
    }
    Some(0)
}

unsafe fn qgv_bw(display: *const IntelDisplay, num_planes: u32, qgv: usize) -> u32 {
    match unsafe { max_bw_index(display, num_planes, qgv) } {
        Some(index) => unsafe { (*display).bw.max[index].deratedbw[qgv] },
        None => 0,
    }
}

unsafe fn max_qgv_point_mask(display: *const IntelDisplay, active_planes: u32) -> u16 {
    let mut best_mask = 0u16;
    let mut best_bw = 0u32;
    for point in 0..unsafe { num_qgv_available(display) } {
        let bw = unsafe { qgv_bw(display, active_planes, point) };
        if bw > best_bw {
            best_bw = bw;
            best_mask = 1 << point;
        }
    }
    best_mask
}

unsafe fn max_psf_point_mask(display: *const IntelDisplay) -> u16 {
    let mut mask = 0u16;
    let mut best_bw = 0u32;
    for point in 0..unsafe { num_psf_available(display) } {
        let bw = unsafe { (*display).bw.max[0].psf_bw[point] };
        if bw > best_bw {
            best_bw = bw;
            mask = 1 << point;
        } else if bw == best_bw {
            mask |= 1 << point;
        }
    }
    mask
}

unsafe fn prepare_qgv_points_mask(display: *const IntelDisplay, qgv: u16, psf: u16) -> u16 {
    prepare_points_mask(qgv, psf, unsafe { qgv_points_mask(display) })
}

#[inline]
fn prepare_points_mask(qgv: u16, psf: u16, available: u16) -> u16 {
    let requested =
        (qgv as u32 & ICL_PCODE_REQ_QGV_PT_MASK) | ((psf as u32) << 8 & ADLS_PCODE_REQ_PSF_PT_MASK);
    !(requested as u16) & available
}

unsafe fn restrict_qgv_points(display: *mut IntelDisplay, mask: u16) -> i32 {
    let ret = unsafe {
        intel_parent_pcode_request(
            display,
            ICL_PCODE_SAGV_DE_MEM_SS_CONFIG,
            mask as u32,
            ICL_PCODE_REP_QGV_MASK | ADLS_PCODE_REP_PSF_MASK,
            ICL_PCODE_REP_QGV_SAFE | ADLS_PCODE_REP_PSF_SAFE,
            1,
        )
    };
    if ret < 0 {
        return ret;
    }
    unsafe {
        (*display).sagv.status = if sagv_enabled_for_mask(display, mask) {
            I915_SAGV_ENABLED
        } else {
            I915_SAGV_DISABLED
        };
    }
    0
}

unsafe fn force_disable_sagv(display: *mut IntelDisplay, state: *mut IntelBwState) {
    let qgv = unsafe { max_qgv_point_mask(display, 0) };
    let psf = unsafe { max_psf_point_mask(display) };
    let mask = unsafe { prepare_qgv_points_mask(display, qgv, psf) };
    unsafe { (*state).qgv_points_mask = mask };
    let _ = unsafe { restrict_qgv_points(display, mask) };
}

unsafe fn for_each_changed_crtc(
    state: *mut IntelAtomicState,
    mut f: impl FnMut(*mut IntelCrtc, *mut IntelCrtcState, *mut IntelCrtcState) -> i32,
) -> i32 {
    if state.is_null() || unsafe { (*state).base.dev.is_null() } {
        return -ENOMEM;
    }
    let drm = unsafe { (*state).base.dev };
    let count = unsafe { (*drm).num_crtcs as usize };
    let entries = unsafe { (*state).base.crtcs };
    if entries.is_null() {
        return 0;
    }
    for index in 0..count {
        let entry = unsafe { &*entries.add(index) };
        if entry.ptr.is_null() {
            continue;
        }
        let crtc = entry.ptr.cast::<IntelCrtc>();
        let ret = f(
            crtc,
            entry.old_state.cast::<IntelCrtcState>(),
            entry.new_state.cast::<IntelCrtcState>(),
        );
        if ret != 0 {
            return ret;
        }
    }
    0
}

unsafe fn crtc_data_rate(state: *const IntelCrtcState) -> u32 {
    if state.is_null() {
        return 0;
    }
    let mut total = 0u32;
    for plane in 0..8 {
        if plane == PLANE_CURSOR {
            continue;
        }
        total = total.saturating_add(unsafe { (*state).data_rate[plane] });
        let display = unsafe { display_from_crtc_state(state) };
        if !display.is_null() && unsafe { display_ver(display) } < 11 {
            total = total.saturating_add(unsafe { (*state).data_rate_y[plane] });
        }
    }
    total
}

unsafe fn crtc_active_planes(state: *const IntelCrtcState) -> u32 {
    if state.is_null() {
        0
    } else {
        (unsafe { (*state).active_planes } & !(1 << PLANE_CURSOR)).count_ones()
    }
}

unsafe fn can_enable_sagv(crtc_state: *const IntelCrtcState) -> bool {
    if crtc_state.is_null() {
        return false;
    }
    let display = unsafe { display_from_crtc_state(crtc_state) };
    if display.is_null()
        || unsafe { (*display).sagv.block_time_us == 0 }
        || !unsafe { (*display).params.enable_sagv }
        || unsafe { (*crtc_state).inherited }
    {
        return false;
    }
    if !unsafe { (*crtc_state).hw.active } {
        return true;
    }
    if unsafe { (*crtc_state).hw.pipe_mode.flags & DRM_MODE_FLAG_INTERLACE != 0 } {
        return false;
    }
    let crtc_ptr = unsafe { (*crtc_state).uapi.crtc };
    if crtc_ptr.is_null() {
        return false;
    }
    let crtc = crtc_ptr.cast::<IntelCrtc>();
    for plane in 0..8 {
        if unsafe { (*crtc).plane_ids_mask } & (1 << plane) == 0 {
            continue;
        }
        let offset = core::mem::offset_of!(IntelCrtcState, wm)
            + core::mem::offset_of!(IntelCrtcWmState, _anon_0_0)
            + core::mem::offset_of!(IntelCrtcWmStateAnon0x0Skl, optimal)
            + core::mem::offset_of!(SklPipeWm, planes)
            + plane * size_of::<SklPlaneWm>();
        let wm_ptr = unsafe { crtc_state.cast::<u8>().add(offset).cast::<SklPlaneWm>() };
        let wm = unsafe { &*wm_ptr };
        if wm.wm[0].enable && !wm.sagv.wm0.enable {
            return false;
        }
    }
    true
}

unsafe fn bw_num_active_planes(display: *const IntelDisplay, state: *const IntelBwState) -> u32 {
    let pipes = unsafe { (*display).info.__runtime_info.pipe_mask };
    let mut count = 0u32;
    for pipe in 0..I915_MAX_PIPES {
        if pipes & (1 << pipe) != 0 {
            count += unsafe { (*state).num_active_planes[pipe] as u32 };
        }
    }
    count
}

unsafe fn bw_data_rate(display: *const IntelDisplay, state: *const IntelBwState) -> u32 {
    let pipes = unsafe { (*display).info.__runtime_info.pipe_mask };
    let mut rate = 0u32;
    for pipe in 0..I915_MAX_PIPES {
        if pipes & (1 << pipe) != 0 {
            rate = rate.saturating_add(unsafe { (*state).data_rate[pipe] });
        }
    }
    if unsafe { display_ver(display) } >= 13
        && unsafe {
            crate::intel_display_utils_upstream::intel_display_vtd_active(
                display as *mut IntelDisplay,
            )
        }
    {
        rate = rate.saturating_mul(105).saturating_add(99) / 100;
    }
    rate
}

unsafe fn get_bw_state(state: *mut IntelAtomicState) -> *mut IntelBwState {
    let display = unsafe { display_from_atomic(state) };
    if display.is_null() {
        return err_ptr(-ENOMEM);
    }
    let base = unsafe {
        crate::intel_global_state_upstream::intel_atomic_get_global_obj_state(
            state,
            ptr::addr_of_mut!((*display).bw.obj),
        )
    };
    if is_err(base) {
        base.cast()
    } else {
        base.cast()
    }
}

unsafe fn old_bw_state(state: *mut IntelAtomicState) -> *mut IntelBwState {
    let display = unsafe { display_from_atomic(state) };
    if display.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        crate::intel_global_state_upstream::intel_atomic_get_old_global_obj_state(
            state,
            ptr::addr_of_mut!((*display).bw.obj),
        )
        .cast()
    }
}

unsafe fn new_bw_state(state: *mut IntelAtomicState) -> *mut IntelBwState {
    let display = unsafe { display_from_atomic(state) };
    if display.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        crate::intel_global_state_upstream::intel_atomic_get_new_global_obj_state(
            state,
            ptr::addr_of_mut!((*display).bw.obj),
        )
        .cast()
    }
}

unsafe fn max_qgv_mask_for_state(display: *const IntelDisplay, active_planes: u32) -> u16 {
    let mut mask = 0u16;
    let mut max_bw = 0u32;
    for qgv in 0..unsafe { num_qgv_available(display) } {
        let bw = unsafe { qgv_bw(display, active_planes, qgv) };
        if bw > max_bw {
            max_bw = bw;
            mask = 1 << qgv;
        }
    }
    mask
}

unsafe fn find_qgv_points(
    display: *mut IntelDisplay,
    old_bw: *const IntelBwState,
    new_bw: *mut IntelBwState,
) -> i32 {
    let ret = unsafe {
        crate::intel_global_state_upstream::intel_atomic_lock_global_state(ptr::addr_of_mut!(
            (*new_bw).base
        ))
    };
    if ret != 0 {
        return ret;
    }
    let rate = unsafe { bw_data_rate(display, new_bw) }.div_ceil(1000);
    let active_planes = unsafe { bw_num_active_planes(display, new_bw) };
    let mut qgv_points = 0u16;
    let mut psf_points = 0u16;
    for point in 0..unsafe { num_qgv_available(display) } {
        if unsafe { qgv_bw(display, active_planes, point) } >= rate {
            qgv_points |= 1 << point;
        }
    }
    for point in 0..unsafe { num_psf_available(display) } {
        if unsafe { (*display).bw.max[0].psf_bw[point] >= rate } {
            psf_points |= 1 << point;
        }
    }
    if qgv_points == 0 || (unsafe { num_psf_available(display) } != 0 && psf_points == 0) {
        return -EINVAL;
    }
    let can_sagv = unsafe { (*new_bw).pipe_sagv_reject == 0 };
    if !can_sagv {
        qgv_points = unsafe { max_qgv_mask_for_state(display, active_planes) };
    }
    let mask = unsafe { prepare_qgv_points_mask(display, qgv_points, psf_points) };
    unsafe { (*new_bw).qgv_points_mask = mask };
    if !old_bw.is_null() && unsafe { (*old_bw).qgv_points_mask != mask } {
        return unsafe {
            crate::intel_global_state_upstream::intel_atomic_serialize_global_state(
                ptr::addr_of_mut!((*new_bw).base),
            )
        };
    }
    0
}

unsafe fn check_sagv_mask(state: *mut IntelAtomicState) -> i32 {
    let display = unsafe { display_from_atomic(state) };
    if display.is_null() {
        return -ENOMEM;
    }
    let mut new_bw: *mut IntelBwState = ptr::null_mut();
    let mut old_bw: *mut IntelBwState = ptr::null_mut();
    let ret = unsafe {
        for_each_changed_crtc(state, |crtc, old_crtc, new_crtc| {
            if can_enable_sagv(old_crtc) == can_enable_sagv(new_crtc) {
                return 0;
            }
            if new_bw.is_null() {
                new_bw = get_bw_state(state);
                if is_err(new_bw) {
                    return new_bw as isize as i32;
                }
                old_bw = old_bw_state(state);
            }
            let pipe = (*crtc).pipe;
            if !(0..I915_MAX_PIPES as i32).contains(&pipe) {
                return -EINVAL;
            }
            if can_enable_sagv(new_crtc) {
                (*new_bw).pipe_sagv_reject &= !(1 << pipe);
            } else {
                (*new_bw).pipe_sagv_reject |= 1 << pipe;
            }
            0
        })
    };
    if ret != 0 {
        return ret;
    }
    if new_bw.is_null() || old_bw.is_null() {
        return 0;
    }
    let new_can = unsafe { (*new_bw).pipe_sagv_reject == 0 };
    let old_can = unsafe { (*old_bw).pipe_sagv_reject == 0 };
    if new_can != old_can {
        unsafe {
            crate::intel_global_state_upstream::intel_atomic_serialize_global_state(
                ptr::addr_of_mut!((*new_bw).base),
            )
        }
    } else if unsafe { (*new_bw).pipe_sagv_reject != (*old_bw).pipe_sagv_reject } {
        unsafe {
            crate::intel_global_state_upstream::intel_atomic_lock_global_state(ptr::addr_of_mut!(
                (*new_bw).base
            ))
        }
    } else {
        0
    }
}

// upstream: intel_bw.c to_intel_bw_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn to_intel_bw_state(obj_state: *mut IntelGlobalState) -> *mut IntelBwState {
    obj_state.cast::<IntelBwState>()
}

// upstream: intel_bw.c intel_atomic_get_old_bw_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_get_old_bw_state(
    state: *mut IntelAtomicState,
) -> *mut IntelBwState {
    unsafe { old_bw_state(state) }
}

// upstream: intel_bw.c intel_atomic_get_new_bw_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_get_new_bw_state(
    state: *mut IntelAtomicState,
) -> *mut IntelBwState {
    unsafe { new_bw_state(state) }
}

// upstream: intel_bw.c intel_atomic_get_bw_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_get_bw_state(
    state: *mut IntelAtomicState,
) -> *mut IntelBwState {
    unsafe { get_bw_state(state) }
}

// upstream: intel_bw.c intel_bw_init_hw()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bw_init_hw(display: *mut IntelDisplay) {
    if display.is_null()
        || !unsafe { intel_display_device_present(display) }
        || !unsafe { is_adlp(display) }
    {
        return;
    }
    let dram = unsafe { (*display).dram.info };
    if dram.is_null() {
        axlog::warn!("i915: display bandwidth initialization has no DRAM information");
        return;
    }
    let dram = unsafe { &*dram };
    let params = match display_info(dram) {
        Some(params) => params,
        None => return,
    };
    let mut qi = IntelQgvInfo::default();
    let ret = unsafe { read_qgv_info(display, dram, &mut qi) };
    if ret != 0 {
        axlog::warn!("i915: could not read ADL-P QGV points ({})", ret);
        return;
    }
    let channels = dram_channels(dram);
    if qi.max_numchannels != 0 && channels > qi.max_numchannels {
        axlog::warn!("i915: ADL-P DRAM channel count exceeds QGV model");
    }
    unsafe { tgl_get_bw_info(display, dram, &qi, params) };
}

unsafe extern "C" fn bw_state_duplicate(obj: *mut IntelGlobalObj) -> *mut IntelGlobalState {
    if obj.is_null() || unsafe { (*obj).state.is_null() } {
        return ptr::null_mut();
    }
    let state = kzalloc_obj::<IntelBwState>();
    if state.is_null() {
        return ptr::null_mut();
    }
    unsafe { ptr::copy_nonoverlapping((*obj).state.cast::<IntelBwState>(), state, 1) };
    state.cast()
}

unsafe extern "C" fn bw_state_destroy(_obj: *mut IntelGlobalObj, state: *mut IntelGlobalState) {
    unsafe { kfree(state) };
}

static INTEL_BW_FUNCS: IntelGlobalStateFuncs = IntelGlobalStateFuncs {
    atomic_duplicate_state: Some(bw_state_duplicate),
    atomic_destroy_state: Some(bw_state_destroy),
};

// upstream: intel_bw.c intel_bw_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bw_init(display: *mut IntelDisplay) -> i32 {
    if display.is_null() {
        return -ENOMEM;
    }
    let state = kzalloc_obj::<IntelBwState>();
    if state.is_null() {
        return -ENOMEM;
    }
    unsafe {
        crate::intel_global_state_upstream::intel_atomic_global_obj_init(
            display,
            ptr::addr_of_mut!((*display).bw.obj),
            ptr::addr_of_mut!((*state).base),
            ptr::addr_of!(INTEL_BW_FUNCS),
        )
    };
    if unsafe { is_adlp(display) } {
        unsafe { force_disable_sagv(display, state) };
    }
    0
}

// upstream: intel_bw.c intel_bw_atomic_check()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bw_atomic_check(state: *mut IntelAtomicState) -> i32 {
    if state.is_null() {
        return -ENOMEM;
    }
    let display = unsafe { display_from_atomic(state) };
    if display.is_null() || unsafe { display_ver(display) } < 9 {
        return 0;
    }
    let ret = unsafe { check_sagv_mask(state) };
    if ret != 0 || unsafe { display_ver(display) } < 11 {
        return ret;
    }

    let mut changed = false;
    let ret = unsafe {
        for_each_changed_crtc(state, |crtc, old_crtc, new_crtc| {
            let old_rate = crtc_data_rate(old_crtc);
            let new_rate = crtc_data_rate(new_crtc);
            let old_planes = crtc_active_planes(old_crtc);
            let new_planes = crtc_active_planes(new_crtc);
            if old_rate == new_rate && old_planes == new_planes {
                return 0;
            }
            let new_bw = get_bw_state(state);
            if is_err(new_bw) {
                return new_bw as isize as i32;
            }
            let pipe = (*crtc).pipe;
            if !(0..I915_MAX_PIPES as i32).contains(&pipe) {
                return -EINVAL;
            }
            (*new_bw).data_rate[pipe as usize] = new_rate;
            (*new_bw).num_active_planes[pipe as usize] = new_planes as u8;
            changed = true;
            0
        })
    };
    if ret != 0 {
        return ret;
    }
    let old_bw = unsafe { old_bw_state(state) };
    let new_bw = unsafe { new_bw_state(state) };
    if !new_bw.is_null()
        && !old_bw.is_null()
        && unsafe { ((*new_bw).pipe_sagv_reject == 0) != ((*old_bw).pipe_sagv_reject == 0) }
    {
        changed = true;
    }
    if !changed {
        return 0;
    }
    if new_bw.is_null() || old_bw.is_null() {
        return -EINVAL;
    }
    unsafe { find_qgv_points(display, old_bw, new_bw) }
}

// upstream: intel_bw.c intel_bw_update_hw_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bw_update_hw_state(display: *mut IntelDisplay) {
    if display.is_null() || unsafe { display_ver(display) } < 9 {
        return;
    }
    let state = unsafe { (*display).bw.obj.state.cast::<IntelBwState>() };
    if state.is_null() {
        return;
    }
    unsafe { (*state).pipe_sagv_reject = 0 };
    let head = unsafe { ptr::addr_of_mut!((*display).pipe_list) };
    let mut pos = unsafe { (*head).next };
    while !ptr::eq(pos, head) {
        let next = unsafe { (*pos).next };
        let crtc = unsafe {
            pos.cast::<u8>()
                .sub(core::mem::offset_of!(IntelCrtc, pipe_head))
                .cast::<IntelCrtc>()
        };
        let pipe = unsafe { (*crtc).pipe };
        if (0..I915_MAX_PIPES as i32).contains(&pipe) {
            let crtc_state = unsafe { (*crtc).base.state.cast::<IntelCrtcState>() };
            if unsafe { display_ver(display) } >= 11 {
                unsafe {
                    (*state).data_rate[pipe as usize] = crtc_data_rate(crtc_state);
                    (*state).num_active_planes[pipe as usize] =
                        crtc_active_planes(crtc_state) as u8;
                }
            }
            unsafe { (*state).pipe_sagv_reject |= 1 << pipe };
        }
        pos = next;
    }
}

// upstream: intel_bw.c intel_bw_crtc_disable_noatomic()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bw_crtc_disable_noatomic(crtc: *mut IntelCrtc) {
    let display = unsafe { display_from_crtc(crtc) };
    if display.is_null() || unsafe { display_ver(display) } < 9 {
        return;
    }
    let state = unsafe { (*display).bw.obj.state.cast::<IntelBwState>() };
    let pipe = unsafe { (*crtc).pipe };
    if !state.is_null() && (0..I915_MAX_PIPES as i32).contains(&pipe) {
        unsafe {
            (*state).data_rate[pipe as usize] = 0;
            (*state).num_active_planes[pipe as usize] = 0;
        }
    }
}

// upstream: intel_bw.c intel_bw_pmdemand_needs_update()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bw_pmdemand_needs_update(state: *mut IntelAtomicState) -> bool {
    let new = unsafe { new_bw_state(state) };
    let old = unsafe { old_bw_state(state) };
    !new.is_null()
        && !old.is_null()
        && unsafe { (*new).qgv_point_peakbw != (*old).qgv_point_peakbw }
}

// upstream: intel_bw.c intel_bw_can_enable_sagv()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bw_can_enable_sagv(
    _display: *mut IntelDisplay,
    bw_state: *const IntelBwState,
) -> bool {
    !bw_state.is_null() && unsafe { (*bw_state).pipe_sagv_reject == 0 }
}

// upstream: intel_bw.c icl_sagv_pre_plane_update()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn icl_sagv_pre_plane_update(state: *mut IntelAtomicState) {
    let display = unsafe { display_from_atomic(state) };
    let old = unsafe { old_bw_state(state) };
    let new = unsafe { new_bw_state(state) };
    if display.is_null() || old.is_null() || new.is_null() {
        return;
    }
    let old_mask = unsafe { (*old).qgv_points_mask };
    let new_mask = old_mask | unsafe { (*new).qgv_points_mask };
    if old_mask != new_mask {
        let _ = unsafe { restrict_qgv_points(display, new_mask) };
    }
}

// upstream: intel_bw.c icl_sagv_post_plane_update()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn icl_sagv_post_plane_update(state: *mut IntelAtomicState) {
    let display = unsafe { display_from_atomic(state) };
    let old = unsafe { old_bw_state(state) };
    let new = unsafe { new_bw_state(state) };
    if display.is_null() || old.is_null() || new.is_null() {
        return;
    }
    let old_mask = unsafe { (*old).qgv_points_mask | (*new).qgv_points_mask };
    let new_mask = unsafe { (*new).qgv_points_mask };
    if old_mask != new_mask {
        let _ = unsafe { restrict_qgv_points(display, new_mask) };
    }
}

// upstream: intel_bw.c intel_bw_qgv_point_peakbw()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bw_qgv_point_peakbw(state: *const IntelBwState) -> i32 {
    if state.is_null() {
        0
    } else {
        unsafe { (*state).qgv_point_peakbw as i32 }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        IntelQgvInfo, IntelQgvPoint, dclk_freq_mhz, max_dclk, peak_bw, prepare_points_mask,
        round_div_closest, tgl_next_group_num_planes, tgl_qgv_group_bw,
    };

    #[test]
    fn dclk_conversion_rounds_to_nearest_sixteenth_mhz_step() {
        assert_eq!(dclk_freq_mhz(6), 100);
        assert_eq!(dclk_freq_mhz(7), 117);
        assert_eq!(round_div_closest(65, 6), 11);
    }

    #[test]
    fn peak_bandwidth_is_channels_times_bytes_per_channel_times_dclk() {
        assert_eq!(peak_bw(2, 64, 1600), 25_600);
    }

    #[test]
    fn maximum_qgv_clock_is_selected_from_reported_points_only() {
        let qi = IntelQgvInfo {
            points: [
                IntelQgvPoint {
                    dclk: 800,
                    ..Default::default()
                },
                IntelQgvPoint {
                    dclk: 1600,
                    ..Default::default()
                },
                IntelQgvPoint {
                    dclk: 1200,
                    ..Default::default()
                },
                IntelQgvPoint::default(),
                IntelQgvPoint::default(),
                IntelQgvPoint::default(),
                IntelQgvPoint::default(),
                IntelQgvPoint::default(),
            ],
            num_points: 3,
            ..Default::default()
        };
        assert_eq!(max_dclk(&qi), 1600);
    }

    #[test]
    fn qgv_point_masks_complement_supported_points_only() {
        assert_eq!(prepare_points_mask(0b0010, 0b101, 0x0507), 0x0005);
    }

    #[test]
    fn tgl_plane_group_and_derated_bandwidth_follow_the_source_equations() {
        assert_eq!(tgl_next_group_num_planes(16, 2, 16), 7);
        assert_eq!(tgl_next_group_num_planes(16, 16, 16), 0);
        let point = IntelQgvPoint {
            dclk: 1600,
            t_rp: 10,
            t_rdpre: 10,
            t_rc: 40,
            t_ras: 30,
            t_rcd: 10,
        };
        assert_eq!(tgl_qgv_group_bw(point, 32, 8, 2, 50_000, 20), 9_429);
        assert_eq!(tgl_qgv_group_bw(point, 32, 8, 2, 8_000, 20), 8_000);
    }
}
