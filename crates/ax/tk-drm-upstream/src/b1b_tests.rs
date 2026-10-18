// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Host unit tests for the batch B1b ports (drm_modes, drm_fourcc,
//! drm_framebuffer, drm_rect, drm_managed, drm_modeset_lock, drm_displayid,
//! drm_eld, drm_print).

use alloc::boxed::Box;
use core::ffi::c_void;
use core::mem::zeroed;
use core::sync::atomic::{AtomicUsize, Ordering};

use tk_linux_kapi::config::{EDEADLK, EINVAL, ENOSPC, ERANGE, IS_ERR};
use tk_linux_kapi::types::ListHead;

use crate::drm_device_types::DrmDevice;
use crate::drm_drv_types::DrmDriver;
use crate::drm_eld_upstream::drm_eld_sad_get;
use crate::drm_displayid_upstream::validate_displayid;
use crate::drm_file_types::DrmFile;
use crate::drm_fourcc_types::DrmFormatInfo;
use crate::drm_fourcc_upstream::*;
use crate::drm_framebuffer_types::DrmFramebuffer;
use crate::drm_framebuffer_upstream::*;
use crate::drm_managed_upstream::*;
use crate::drm_mode_config_types::DrmModeConfigFuncs;
use crate::drm_modes_types::{DrmDisplayMode, MODE_BAD, MODE_NO_420, MODE_OK};
use crate::drm_modes_upstream::*;
use crate::drm_modeset_helper_upstream::*;
use crate::drm_modeset_lock_types::{DrmModesetAcquireCtx, DrmModesetLock};
use crate::drm_modeset_lock_upstream::*;
use crate::drm_print_types::{DrmPrintIterator, DrmPrinter};
use crate::drm_rect_types::DrmRect;
use crate::drm_rect_upstream::*;
use crate::test_support::TestDrm;
use crate::uapi_drm_consts::*;
use crate::uapi_drm_mode_types::{DrmModeFbCmd2, DrmModeModeinfo};
use crate::drm_connector_types::DrmCmdlineMode;
use crate::drm_print_upstream::{__drm_printfn_coredump, __drm_puts_coredump};

const DRIVER_MODESET_FEATURE: u32 = 1 << 1;

// Fake per-task identities for the single-threaded ww_mutex tests. kapi
// mutexes take their owner from the per-CPU current-task pointer.
static TASK_A: u64 = 0xA;
static TASK_B: u64 = 0xB;

fn set_task(task: &u64) {
    unsafe { axhal::percpu::set_current_task_ptr(task as *const u64 as *const ()) };
}

/// Clears the fake current task when a test ends (also on panic), so later
/// tests in the same process see the default no-task state.
struct NoTaskOnDrop;

impl Drop for NoTaskOnDrop {
    fn drop(&mut self) {
        unsafe { axhal::percpu::set_current_task_ptr(core::ptr::null::<()>()) };
    }
}

fn mode_with(hd: u16, hss: u16, hse: u16, ht: u16, vd: u16, vss: u16, vse: u16, vt: u16, clock: i32) -> DrmDisplayMode {
    let mut m: DrmDisplayMode = unsafe { zeroed() };
    m.clock = clock;
    m.hdisplay = hd;
    m.hsync_start = hss;
    m.hsync_end = hse;
    m.htotal = ht;
    m.vdisplay = vd;
    m.vsync_start = vss;
    m.vsync_end = vse;
    m.vtotal = vt;
    m
}

fn name_of(m: &DrmDisplayMode) -> &str {
    let len = m.name.iter().position(|&c| c == 0).unwrap_or(32);
    let bytes = unsafe { core::slice::from_raw_parts(m.name.as_ptr() as *const u8, len) };
    core::str::from_utf8(bytes).unwrap()
}

// ---------------------------------------------------------------------------
// drm_modes.c
// ---------------------------------------------------------------------------

#[test]
fn mode_vrefresh_and_name() {
    // 1920x1080 at 148.5 MHz with htotal 2200, vtotal 1125 is exactly 60 Hz.
    let mut m = mode_with(1920, 2008, 2052, 2200, 1080, 1084, 1089, 1125, 148_500);
    assert_eq!(unsafe { drm_mode_vrefresh(&m) }, 60);
    unsafe { drm_mode_set_name(&mut m) };
    assert_eq!(name_of(&m), "1920x1080");
    m.flags |= DRM_MODE_FLAG_INTERLACE;
    unsafe { drm_mode_set_name(&mut m) };
    assert_eq!(name_of(&m), "1920x1080i");
    // Zero totals short-circuit to 0.
    let z = mode_with(640, 0, 0, 0, 480, 0, 0, 0, 25_175);
    assert_eq!(unsafe { drm_mode_vrefresh(&z) }, 0);
}

#[test]
fn cvt_1920x1080_60_matches_linux_formula() {
    // Non-reduced CVT, no margins: the canonical `cvt 1920 1080 60` timing.
    let mut dev: TestDrm = TestDrm::new(0);
    let m = unsafe { drm_cvt_mode(dev.ptr(), 1920, 1080, 60, false, false, false) };
    assert!(!m.is_null());
    let m = unsafe { &*m };
    assert_eq!((m.hdisplay, m.hsync_start, m.hsync_end, m.htotal), (1920, 2048, 2248, 2576));
    assert_eq!((m.vdisplay, m.vsync_start, m.vsync_end, m.vtotal), (1080, 1083, 1088, 1120));
    // Linux drm_cvt_mode(): hperiod = 967000000 * 2 / (1083 * 2 * 60) = 14881 ns/line,
    // htotal 2576, clock = 2576 * 1000 * 1000 / 14881 = 173106 kHz. The CVT_CLOCK_STEP
    // rounding subtracts `drm_mode->clock % 250`, and that clock is still 0 (kzalloc).
    // xrandr's 173.00 MHz is its own 0.25 MHz rounding, not the kernel value.
    assert_eq!(m.clock, 173_106);
    assert_eq!(name_of(m), "1920x1080");
    assert_eq!(m.flags, DRM_MODE_FLAG_PVSYNC | DRM_MODE_FLAG_NHSYNC);
}

#[test]
fn gtf_1024x768_60_matches_linux_formula() {
    // Reference values from an independent transcription of
    // drm_gtf_mode_complex() (see the B1b report).
    let mut dev: TestDrm = TestDrm::new(0);
    let m = unsafe { drm_gtf_mode(dev.ptr(), 1024, 768, 60, false, 0) };
    assert!(!m.is_null());
    let m = unsafe { &*m };
    assert_eq!((m.hdisplay, m.hsync_start, m.hsync_end, m.htotal), (1024, 1080, 1184, 1344));
    assert_eq!((m.vdisplay, m.vsync_start, m.vsync_end, m.vtotal), (768, 769, 772, 795));
    assert_eq!(m.clock, 64_127);
    // drm_gtf_mode() passes the 600/80/128/40 coefficients, which selects the NHSYNC|PVSYNC flags.
    assert_eq!(m.flags, DRM_MODE_FLAG_NHSYNC | DRM_MODE_FLAG_PVSYNC);
    assert_eq!(unsafe { drm_mode_vrefresh(m) }, 60);
}

#[test]
fn duplicate_equal_and_clock_compare() {
    let mut dev: TestDrm = TestDrm::new(0);
    let src = mode_with(1024, 1080, 1184, 1344, 768, 769, 772, 795, 64_127);
    let dup = unsafe { drm_mode_duplicate(dev.ptr(), &src) };
    assert!(!dup.is_null());
    let d = unsafe { &*dup };
    assert!(unsafe { drm_mode_equal(&src, d) });
    assert_eq!(name_of(d), "");
    // A different clock breaks drm_mode_equal() but not the clock-less compare.
    let mut other = mode_with(1024, 1080, 1184, 1344, 768, 769, 772, 795, 64_127);
    other.clock = 65_000;
    assert!(!unsafe { drm_mode_equal(&src, &other) });
    assert!(unsafe { drm_mode_equal_no_clocks(&src, &other) });
    assert!(unsafe { drm_mode_match(&src, &other, DRM_MODE_MATCH_TIMINGS) });
    assert!(unsafe { drm_mode_match(core::ptr::null(), core::ptr::null(), 0) });
    assert!(!unsafe { drm_mode_match(&src, core::ptr::null(), 0) });
    unsafe { drm_mode_destroy(dev.ptr(), dup) };
}

#[test]
fn crtcinfo_halves_interlaced_and_doubles_doublescan() {
    let mut m = mode_with(720, 740, 800, 880, 480, 490, 492, 525, 13_500);
    m.flags = DRM_MODE_FLAG_INTERLACE;
    unsafe { drm_mode_set_crtcinfo(&mut m, CRTC_INTERLACE_HALVE_V as i32) };
    assert_eq!(m.crtc_vdisplay, 240);
    assert_eq!(m.crtc_vtotal, 262);
    assert_eq!(m.crtc_hdisplay, 720);
    let mut hv = (0, 0);
    let mut n = mode_with(640, 656, 752, 800, 480, 490, 492, 525, 25_175);
    unsafe { drm_mode_get_hv_timing(&n, &mut hv.0, &mut hv.1) };
    assert_eq!(hv, (640, 480));
    n.vscan = 2;
    unsafe { drm_mode_set_crtcinfo(&mut n, 0) };
    assert_eq!(n.crtc_vdisplay, 960);
}

#[test]
fn cmdline_parsing_named_res_options_and_flags() {
    let mut dev: TestDrm = TestDrm::new(0);
    let _ = &mut dev;
    let parse = |s: &core::ffi::CStr| {
        let mut cmd: DrmCmdlineMode = unsafe { zeroed() };
        let ok = unsafe { drm_mode_parse_command_line_for_connector(s.as_ptr(), core::ptr::null(), &mut cmd) };
        (ok, cmd)
    };

    let (ok, cmd) = parse(c"1024x768@60");
    assert!(ok);
    assert_eq!((cmd.xres, cmd.yres, cmd.refresh, cmd.refresh_specified), (1024, 768, 60, true));

    let (ok, cmd) = parse(c"1366x768M");
    assert!(ok && cmd.cvt);

    let (ok, cmd) = parse(c"NTSC");
    assert!(ok);
    assert_eq!((cmd.xres, cmd.yres, cmd.interlace, cmd.tv_mode_specified), (720, 480, true, true));

    let (ok, cmd) = parse(c"1024x768,rotate=90");
    assert!(ok);
    assert_eq!(cmd.rotation_reflection, DRM_MODE_ROTATE_90);

    let (ok, cmd) = parse(c"1024x768,rotate=45");
    assert!(!ok);
    let _ = cmd;

    let (ok, cmd) = parse(c"e");
    assert!(ok);
    assert_eq!(cmd.force, crate::drm_connector_types::DRM_FORCE_ON);

    let (ok, _) = parse(c"1024x768-24");
    assert!(ok);
    let (ok, _) = parse(c"");
    assert!(!ok);
}

#[test]
fn mode_sort_prefers_preferred_then_resolution() {
    let mut list = ListHead { next: core::ptr::null_mut(), prev: core::ptr::null_mut() };
    let lp = &mut list as *mut ListHead;
    unsafe {
        (*lp).next = lp;
        (*lp).prev = lp;
    }
    let mut a = Box::new(mode_with(640, 656, 752, 800, 480, 490, 492, 525, 25_175));
    let mut b = Box::new(mode_with(1024, 1080, 1184, 1344, 768, 769, 772, 795, 64_127));
    let mut c = Box::new(mode_with(800, 832, 912, 1024, 600, 601, 604, 622, 38_184));
    c.r#type = DRM_MODE_TYPE_PREFERRED as u8;
    for m in [&mut a, &mut b, &mut c] {
        m.status = MODE_OK;
        unsafe { tk_linux_kapi::list::list_add_tail(core::ptr::addr_of_mut!(m.head), lp) };
    }
    unsafe { drm_mode_sort(lp) };
    // Expected order: preferred 800x600 first, then 1024x768, then 640x480.
    let mut order = [0usize; 3];
    let mut n = 0;
    let mut link = unsafe { (*lp).next };
    while link != lp {
        let mode = unsafe { link.cast::<u8>().sub(core::mem::offset_of!(DrmDisplayMode, head)) as *mut DrmDisplayMode };
        order[n] = unsafe { (*mode).hdisplay as usize };
        n += 1;
        link = unsafe { (*link).next };
    }
    assert_eq!(order, [800, 1024, 640]);
}

#[test]
fn mode_prune_removes_rejected_modes() {
    let mut dev: TestDrm = TestDrm::new(0);
    let mut list = ListHead { next: core::ptr::null_mut(), prev: core::ptr::null_mut() };
    let lp = &mut list as *mut ListHead;
    unsafe {
        (*lp).next = lp;
        (*lp).prev = lp;
    }
    let ok = unsafe { drm_mode_duplicate(dev.ptr(), &mode_with(640, 656, 752, 800, 480, 490, 492, 525, 25_175)) };
    let bad = unsafe { drm_mode_duplicate(dev.ptr(), &mode_with(641, 656, 752, 800, 480, 490, 492, 525, 25_175)) };
    unsafe {
        (*ok).status = MODE_OK;
        (*bad).status = MODE_BAD;
        tk_linux_kapi::list::list_add_tail(core::ptr::addr_of_mut!((*ok).head), lp);
        tk_linux_kapi::list::list_add_tail(core::ptr::addr_of_mut!((*bad).head), lp);
        drm_mode_prune_invalid(dev.ptr(), lp, false);
    }
    let remaining = unsafe { tk_linux_kapi::list::list_count_nodes(lp) };
    assert_eq!(remaining, 1);
    assert_eq!(unsafe { (*(*lp).next).next } as *mut ListHead, lp);
    unsafe { drm_mode_destroy(dev.ptr(), ok) };
}

#[test]
fn mode_status_names_and_validation() {
    let name = |s| unsafe { core::ffi::CStr::from_ptr(drm_get_mode_status_name(s)) }.to_str().unwrap();
    assert_eq!(name(MODE_BAD), "BAD");
    assert_eq!(name(MODE_NO_420), "NO_420");
    assert_eq!(name(MODE_OK), "OK");

    let m = mode_with(640, 656, 752, 800, 480, 490, 492, 525, 25_175);
    assert_eq!(unsafe { drm_mode_validate_size(&m, 800, 600) }, MODE_OK);
    assert_eq!(unsafe { drm_mode_validate_size(&m, 600, 600) }, crate::drm_modes_types::MODE_VIRTUAL_X);
    let bad = mode_with(640, 600, 752, 800, 480, 490, 492, 525, 25_175);
    let mut dev: TestDrm = TestDrm::new(0);
    dev.dev.mode_config.funcs = Box::leak(Box::new(unsafe { zeroed::<DrmModeConfigFuncs>() })) as *mut DrmModeConfigFuncs;
    assert_eq!(unsafe { drm_mode_validate_driver(dev.ptr(), &bad) }, crate::drm_modes_types::MODE_H_ILLEGAL);
    assert_eq!(unsafe { drm_mode_validate_driver(dev.ptr(), &m) }, MODE_OK);
}

#[test]
fn umode_round_trip_preserves_timing() {
    let mut dev: TestDrm = TestDrm::new(0);
    dev.dev.mode_config.funcs = Box::leak(Box::new(unsafe { zeroed::<DrmModeConfigFuncs>() })) as *mut DrmModeConfigFuncs;
    let cvt = unsafe { drm_cvt_mode(dev.ptr(), 1920, 1080, 60, false, false, false) };
    let mut umode: DrmModeModeinfo = unsafe { zeroed() };
    unsafe { drm_mode_convert_to_umode(&mut umode, cvt) };
    assert_eq!(umode.hdisplay, 1920);
    assert_eq!(umode.vrefresh, 60);
    assert_ne!(umode.flags & DRM_MODE_FLAG_PIC_AR_NONE, u32::MAX);
    let mut out: DrmDisplayMode = unsafe { zeroed() };
    let ret = unsafe { drm_mode_convert_umode(dev.ptr(), &mut out, &umode) };
    assert_eq!(ret, 0);
    assert_eq!((out.hsync_start, out.htotal, out.vtotal), (2048, 2576, 1120));
    assert_eq!(out.status, MODE_OK);
    let mut too_big = umode;
    too_big.clock = i32::MAX as u32 + 1;
    assert_eq!(unsafe { drm_mode_convert_umode(dev.ptr(), &mut out, &too_big) }, -ERANGE);
}

// ---------------------------------------------------------------------------
// drm_rect.c
// ---------------------------------------------------------------------------

#[test]
fn rect_intersect_clip_scale_and_rotate() {
    let mut r1 = DrmRect { x1: 0, y1: 0, x2: 100, y2: 100 };
    let r2 = DrmRect { x1: 50, y1: 50, x2: 150, y2: 150 };
    assert!(unsafe { drm_rect_intersect(&mut r1, &r2) });
    assert_eq!((r1.x1, r1.y1, r1.x2, r1.y2), (50, 50, 100, 100));
    let mut far = DrmRect { x1: 0, y1: 0, x2: 10, y2: 10 };
    assert!(!unsafe { drm_rect_intersect(&mut far, &DrmRect { x1: 20, y1: 20, x2: 30, y2: 30 }) });

    // Clip the left edge of a 2:1 downscale; src and dst both shrink.
    let mut src = DrmRect { x1: 0, y1: 0, x2: 100, y2: 100 };
    let mut dst = DrmRect { x1: 0, y1: 0, x2: 50, y2: 50 };
    let clip = DrmRect { x1: 25, y1: 0, x2: 50, y2: 50 };
    assert!(unsafe { drm_rect_clip_scaled(&mut src, &mut dst, &clip) });
    assert_eq!((dst.x1, dst.x2), (25, 50));
    assert_eq!((src.x1, src.x2), (50, 100));

    let s = DrmRect { x1: 0, y1: 0, x2: 100, y2: 100 };
    let d = DrmRect { x1: 0, y1: 0, x2: 50, y2: 50 };
    assert_eq!(unsafe { drm_rect_calc_hscale(&s, &d, 1, 4) }, 2);
    assert_eq!(unsafe { drm_rect_calc_vscale(&s, &d, 1, 1) }, -ERANGE);

    let mut r = DrmRect { x1: 0, y1: 0, x2: 10, y2: 20 };
    unsafe { drm_rect_rotate(&mut r, 100, 50, DRM_MODE_ROTATE_90) };
    assert_eq!((r.x1, r.y1, r.x2, r.y2), (0, 90, 20, 100));
    let mut back = r;
    unsafe { drm_rect_rotate_inv(&mut back, 100, 50, DRM_MODE_ROTATE_90) };
    assert_eq!((back.x1, back.y1, back.x2, back.y2), (0, 0, 10, 20));
}

// ---------------------------------------------------------------------------
// drm_fourcc.c
// ---------------------------------------------------------------------------

#[test]
fn fourcc_lookup_legacy_and_plane_sizes() {
    let xrgb = __drm_format_info(DRM_FORMAT_XRGB8888);
    assert!(!xrgb.is_null());
    let x = unsafe { &*xrgb };
    assert_eq!((x.depth, x.num_planes), (24, 1));
    assert_eq!(unsafe { x._anon_6.char_per_block[0] }, 4);
    assert!(!x.has_alpha);

    let argb = unsafe { &*__drm_format_info(DRM_FORMAT_ARGB8888) };
    assert!(argb.has_alpha);

    let nv12 = __drm_format_info(DRM_FORMAT_NV12);
    assert!(!nv12.is_null());
    let nv12 = unsafe { &*nv12 };
    assert_eq!(nv12.num_planes, 2);
    assert!(nv12.is_yuv);
    assert_eq!(unsafe { drm_format_info_min_pitch(nv12, 0, 1920) }, 1920);
    // Chroma plane: width is subsampled by hsub before the pitch is computed.
    assert_eq!(unsafe { drm_format_info_min_pitch(nv12, 1, 960) }, 1920);
    assert_eq!(unsafe { drm_format_info_min_pitch(nv12, 1, 1920) }, 3840);
    assert_eq!(unsafe { drm_format_info_block_width(nv12, 1) }, 1);

    assert_eq!(unsafe { drm_format_info_bpp(nv12, 0) }, 8);
    assert_eq!(unsafe { drm_format_info_bpp(x, 0) }, 32);
    assert_eq!(unsafe { drm_format_info_bpp(x, 1) }, 0);

    assert_eq!(drm_mode_legacy_fb_format(32, 24), DRM_FORMAT_XRGB8888);
    assert_eq!(drm_mode_legacy_fb_format(16, 16), DRM_FORMAT_RGB565);
    assert_eq!(drm_mode_legacy_fb_format(16, 15), DRM_FORMAT_XRGB1555);
    assert_eq!(drm_mode_legacy_fb_format(7, 7), DRM_FORMAT_INVALID);
    assert!(__drm_format_info(0xdead_beef).is_null());
}

// ---------------------------------------------------------------------------
// drm_framebuffer.c
// ---------------------------------------------------------------------------

#[test]
fn framebuffer_src_coords_and_addfb2_validation() {
    let mut fb: DrmFramebuffer = unsafe { zeroed() };
    fb.width = 100;
    fb.height = 100;
    assert_eq!(unsafe { drm_framebuffer_check_src_coords(0, 0, 100 << 16, 100 << 16, &fb) }, 0);
    assert_eq!(unsafe { drm_framebuffer_check_src_coords(0, 0, 101 << 16, 100 << 16, &fb) }, -ENOSPC);

    let mut dev: TestDrm = TestDrm::new(DRIVER_MODESET_FEATURE);
    dev.init_registry();
    dev.dev.mode_config.funcs = Box::leak(Box::new(unsafe { zeroed::<DrmModeConfigFuncs>() })) as *mut DrmModeConfigFuncs;
    dev.dev.mode_config.max_width = 4096;
    dev.dev.mode_config.max_height = 4096;
    let mut file: DrmFile = unsafe { zeroed() };

    let base = || DrmModeFbCmd2 {
        fb_id: 0,
        width: 640,
        height: 480,
        pixel_format: DRM_FORMAT_XRGB8888,
        flags: 0,
        handles: [1, 0, 0, 0],
        pitches: [2560, 0, 0, 0],
        offsets: [0; 4],
        modifier: [0; 4],
    };
    let copy_cmd = |r: &DrmModeFbCmd2| DrmModeFbCmd2 { fb_id: r.fb_id, width: r.width, height: r.height, pixel_format: r.pixel_format, flags: r.flags, handles: r.handles, pitches: r.pitches, offsets: r.offsets, modifier: r.modifier };
    let run = |r: &DrmModeFbCmd2, dev: &mut TestDrm, file: &mut DrmFile| {
        let mut cmd = copy_cmd(r);
        unsafe { drm_mode_addfb2(dev.ptr(), &mut cmd as *mut _ as *mut c_void, file) }
    };

    let mut zero_w = base();
    zero_w.width = 0;
    assert_eq!(run(&zero_w, &mut dev, &mut file), -EINVAL);

    let mut bad_flags = base();
    bad_flags.flags = 0x100;
    assert_eq!(run(&bad_flags, &mut dev, &mut file), -EINVAL);

    let mut bad_format = base();
    bad_format.pixel_format = 0xdead_beef;
    assert_eq!(run(&bad_format, &mut dev, &mut file), -EINVAL);

    let mut no_handle = base();
    no_handle.handles = [0; 4];
    assert_eq!(run(&no_handle, &mut dev, &mut file), -EINVAL);

    let mut short_pitch = base();
    short_pitch.pitches[0] = 100;
    assert_eq!(run(&short_pitch, &mut dev, &mut file), -EINVAL);

    let mut stray_plane = base();
    stray_plane.flags = DRM_MODE_FB_MODIFIERS;
    stray_plane.handles = [1, 2, 0, 0];
    assert_eq!(run(&stray_plane, &mut dev, &mut file), -EINVAL);

    // Lookup of an id that was never registered.
    assert!(unsafe { drm_framebuffer_lookup(dev.ptr(), &mut file, 42) }.is_null());
}

// ---------------------------------------------------------------------------
// drm_managed.c
// ---------------------------------------------------------------------------

static RELEASE_LOG: [AtomicUsize; 8] = [const { AtomicUsize::new(0) }; 8];
static RELEASE_COUNT: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn record_release(_dev: *mut DrmDevice, data: *mut c_void) {
    let n = RELEASE_COUNT.fetch_add(1, Ordering::SeqCst);
    RELEASE_LOG[n].store(data as usize, Ordering::SeqCst);
}

#[test]
fn drmm_actions_run_in_reverse_registration_order() {
    RELEASE_COUNT.store(0, Ordering::SeqCst);
    let mut dev: TestDrm = TestDrm::new(0);
    let p = dev.ptr();
    let name = c"action".as_ptr();
    unsafe {
        assert_eq!(__drmm_add_action(p, Some(record_release), 0x10 as *mut c_void, name), 0);
        assert_eq!(__drmm_add_action(p, Some(record_release), 0x20 as *mut c_void, name), 0);
        assert_eq!(__drmm_add_action(p, Some(record_release), 0x30 as *mut c_void, name), 0);
        // drmm_release_action() removes a matching action without running a
        // second time at drm_managed_release().
        drmm_release_action(p, Some(record_release), 0x20 as *mut c_void);
        drm_managed_release(p);
    }
    let order: [usize; 2] = [RELEASE_LOG[0].load(Ordering::SeqCst), RELEASE_LOG[1].load(Ordering::SeqCst)];
    // The 0x20 action ran in drmm_release_action(); the rest unwind LIFO.
    assert_eq!(RELEASE_COUNT.load(Ordering::SeqCst), 3);
    assert_eq!(order, [0x20, 0x30]);
    assert_eq!(RELEASE_LOG[2].load(Ordering::SeqCst), 0x10);
}

#[test]
fn drmm_kmalloc_and_kstrdup_are_tracked() {
    let mut dev: TestDrm = TestDrm::new(0);
    let p = dev.ptr();
    unsafe {
        // This KAPI has no non-sleeping heap pool: honor the caller's flags
        // rather than silently changing a NOWAIT request to GFP_KERNEL.
        assert!(drmm_kmalloc(p, 16, 0).is_null());
        let buf = drmm_kmalloc(p, 16, tk_linux_kapi::config::GFP_KERNEL) as *mut u8;
        assert!(!buf.is_null());
        *buf = 7;
        drmm_kfree(p, buf as *mut c_void);
        let s = drmm_kstrdup(p, c"hi".as_ptr(), tk_linux_kapi::config::GFP_KERNEL);
        assert_eq!(core::ffi::CStr::from_ptr(s).to_bytes(), b"hi");
        drm_managed_release(p);
    }
}

// ---------------------------------------------------------------------------
// drm_modeset_lock.c (ww_mutex wound/wait and -EDEADLK backoff)
// ---------------------------------------------------------------------------

#[test]
fn modeset_lock_deadlock_backoff() {
    let _no_task = NoTaskOnDrop;
    let mut dev: TestDrm = TestDrm::new(0);
    let mut l1: DrmModesetLock = unsafe { zeroed() };
    let mut l2: DrmModesetLock = unsafe { zeroed() };
    unsafe {
        drm_modeset_lock_init(&mut l1);
        drm_modeset_lock_init(&mut l2);
    }
    let mut ctx_a: DrmModesetAcquireCtx = unsafe { zeroed() };
    let mut ctx_b: DrmModesetAcquireCtx = unsafe { zeroed() };

    unsafe {
        set_task(&TASK_A);
        drm_modeset_acquire_init(&mut ctx_a, 0);
        set_task(&TASK_B);
        drm_modeset_acquire_init(&mut ctx_b, 0);
        // A (older) takes L1; B (younger) takes L2.
        set_task(&TASK_A);
        assert_eq!(drm_modeset_lock(&mut l1, &mut ctx_a), 0);
        set_task(&TASK_B);
        assert_eq!(drm_modeset_lock(&mut l2, &mut ctx_b), 0);
        // B asks for L1, which A holds. B has been wounded by A, so the
        // request fails with -EDEADLK instead of blocking.
        ctx_b.ww_ctx.wounded = 1;
        assert_eq!(drm_modeset_lock(&mut l1, &mut ctx_b), -EDEADLK);
        assert!(!ctx_b.contended.is_null());
        // Back off: drop B's locks, let A release L1, then slow-lock L1.
        drm_modeset_drop_locks(&mut ctx_b);
        set_task(&TASK_A);
        drm_modeset_drop_locks(&mut ctx_a);
        set_task(&TASK_B);
        assert_eq!(drm_modeset_backoff(&mut ctx_b), 0);
        assert!(ctx_b.contended.is_null());
        assert_eq!(ctx_b.ww_ctx.acquired, 1);
        drm_modeset_drop_locks(&mut ctx_b);
        drm_modeset_acquire_fini(&mut ctx_b);
        set_task(&TASK_A);
        drm_modeset_acquire_fini(&mut ctx_a);
    }
    let _ = &mut dev;
}

#[test]
fn modeset_lock_uncontended_trylock_path() {
    let _no_task = NoTaskOnDrop;
    let mut l1: DrmModesetLock = unsafe { zeroed() };
    unsafe { drm_modeset_lock_init(&mut l1) };
    let mut ctx: DrmModesetAcquireCtx = unsafe { zeroed() };
    unsafe {
        set_task(&TASK_A);
        drm_modeset_acquire_init(&mut ctx, 0);
        assert_eq!(drm_modeset_lock(&mut l1, &mut ctx), 0);
        // A second lock by the same context is -EALREADY, reported as success.
        assert_eq!(drm_modeset_lock(&mut l1, &mut ctx), 0);
        drm_modeset_drop_locks(&mut ctx);
        drm_modeset_acquire_fini(&mut ctx);
    }
    let _ = EDEADLK;
}

// ---------------------------------------------------------------------------
// drm_displayid.c and drm_eld.c
// ---------------------------------------------------------------------------

#[test]
fn displayid_block_checksum_validation() {
    // Header: rev 0x12, bytes 4, prod_id 0, ext_count 0; then 4 payload bytes
    // and a checksum byte chosen so the whole section sums to zero.
    let mut blob = [0x12u8, 4, 0, 0, 0x01, 0x02, 0x03, 0x04, 0];
    let sum: u8 = blob[..8].iter().fold(0u8, |a, b| a.wrapping_add(*b));
    blob[8] = 0u8.wrapping_sub(sum);
    let good = unsafe { validate_displayid(blob.as_ptr(), blob.len() as i32, 0, false) };
    assert!(!IS_ERR(good));
    assert_eq!(unsafe { (*good).rev }, 0x12);

    let mut bad = blob;
    bad[5] ^= 0x40;
    assert!(IS_ERR(unsafe { validate_displayid(bad.as_ptr(), bad.len() as i32, 0, false) }));
    // The quirk path ignores the checksum.
    assert!(!IS_ERR(unsafe { validate_displayid(bad.as_ptr(), bad.len() as i32, 0, true) }));
    // Too short for the declared payload.
    assert!(IS_ERR(unsafe { validate_displayid(blob.as_ptr(), 6, 0, true) }));
}

#[test]
fn eld_sad_index_bounds() {
    // ELD: MNL (byte 4) = 2, SAD count (byte 5, bits 4..7) = 2.
    let mut eld = [0u8; 64];
    eld[4] = 2;
    eld[5] = 2 << 4;
    let mut sad = crate::drm_edid_types::CeaSad { format: 0, channels: 0, freq: 0, byte2: 0 };
    assert_eq!(unsafe { drm_eld_sad_get(eld.as_ptr(), 0, &mut sad) }, 0);
    assert_eq!(unsafe { drm_eld_sad_get(eld.as_ptr(), 2, &mut sad) }, -EINVAL);
}

// ---------------------------------------------------------------------------
// drm_print.c
// ---------------------------------------------------------------------------

#[test]
fn coredump_printer_window_copies_text() {
    let mut buf = [0u8; 8];
    let mut it = DrmPrintIterator {
        data: buf.as_mut_ptr() as *mut c_void,
        start: 0,
        remain: 8,
        offset: 0,
    };
    let mut printer: DrmPrinter = unsafe { zeroed() };
    printer.arg = &mut it as *mut DrmPrintIterator as *mut c_void;
    unsafe {
        __drm_puts_coredump(&mut printer, c"hello".as_ptr());
        __drm_puts_coredump(&mut printer, c" world".as_ptr());
    }
    assert_eq!(&buf, b"hello wo");
    assert_eq!((it.offset, it.remain), (8, 0));

    // printfn callback with a formatted &str payload.
    let mut buf2 = [0u8; 8];
    let mut it2 = DrmPrintIterator { data: buf2.as_mut_ptr() as *mut c_void, start: 0, remain: 8, offset: 0 };
    printer.arg = &mut it2 as *mut DrmPrintIterator as *mut c_void;
    let mut text: &str = "abc";
    unsafe { __drm_printfn_coredump(&mut printer, &mut text as *mut &str as *mut c_void) };
    assert_eq!(&buf2[..3], b"abc");
    assert_eq!(it2.offset, 3);
}
