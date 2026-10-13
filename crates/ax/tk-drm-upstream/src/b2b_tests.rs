// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Host unit tests for the batch B2b ports:
//! - drm_atomic_helper
//! - drm_gem_framebuffer_helper
//! - drm_gem_atomic_helper

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::mem::zeroed;
use std::sync::Mutex;

use tk_linux_kapi::config::{EBUSY, EINVAL, ERANGE, IS_ERR};
use tk_linux_kapi::list::INIT_LIST_HEAD;

use crate::drm_atomic_helper_upstream::*;
use crate::drm_atomic_state_helper_upstream::*;
use crate::drm_atomic_upstream::*;
use crate::drm_atomic_types::DrmAtomicCommit;
use crate::drm_atomic_uapi_upstream::{
    drm_atomic_set_crtc_for_connector, drm_atomic_set_crtc_for_plane,
    drm_atomic_set_mode_for_crtc,
};
use crate::drm_connector_types::{DrmConnector, DrmConnectorFuncs};
use crate::drm_crtc_types::{DrmCrtc, DrmCrtcFuncs, DrmCrtcState};
use crate::drm_device_types::DrmDevice;
use crate::drm_encoder_types::DrmEncoder;
use crate::drm_file_upstream::{drm_file_alloc, drm_file_free};
use crate::drm_fourcc_types::DrmFormatInfo;
use crate::drm_framebuffer_types::{DrmFramebuffer, DrmFramebufferFuncs};
use crate::drm_framebuffer_upstream::{drm_framebuffer_cleanup, drm_framebuffer_init};
use crate::drm_gem_atomic_helper_upstream::*;
use crate::drm_gem_framebuffer_helper_upstream::*;
use crate::drm_gem_types::DrmGemObject;
use crate::drm_gem_upstream::{
    drm_gem_handle_create, drm_gem_handle_delete, drm_gem_object_init, drm_gem_object_release,
};
use crate::drm_modes_types::DrmDisplayMode;
use crate::drm_modeset_helper_vtables_types::{
    DrmConnectorHelperFuncs, DrmCrtcHelperFuncs, DrmEncoderHelperFuncs, DrmModeConfigHelperFuncs,
    DrmPlaneHelperFuncs,
};
use crate::drm_mode_config_types::DrmModeConfigFuncs;
use crate::drm_modeset_lock_types::DrmModesetAcquireCtx;
use crate::drm_modeset_lock_upstream::*;
use crate::drm_plane_types::{DrmPlane, DrmPlaneFuncs, DrmPlaneState, DRM_PLANE_TYPE_PRIMARY};
use crate::drm_rect_types::DrmRect;
use crate::test_support::TestDrm;
use crate::uapi_drm_consts::*;
use crate::uapi_drm_mode_types::DrmModeFbCmd2;

const DRIVER_MODESET: u32 = 1 << 1;
const DRIVER_ATOMIC: u32 = 1 << 4;
const DRIVER_GEM: u32 = 1 << 0;

use core::ffi::{c_char, c_int};
use tk_linux_kapi::kernel_services::TaskOps;

unsafe extern "C" fn test_current_comm() -> *const c_char {
    c"test".as_ptr()
}
unsafe extern "C" fn test_task_comm(_task: *const c_void, buf: *mut c_char) {
    unsafe { core::ptr::copy_nonoverlapping(c"test".as_ptr(), buf, 5) };
}
unsafe extern "C" fn test_task_pid_nr(_task: *const c_void) -> c_int { 1 }
unsafe extern "C" fn test_task_pid(_task: *mut c_void, _pid_type: c_int) -> *mut c_void { core::ptr::null_mut() }
unsafe extern "C" fn test_pid_nr(_pid: *const c_void) -> c_int { 1 }
unsafe extern "C" fn test_put_pid(_pid: *mut c_void) {}
unsafe extern "C" fn test_capable(_cap: c_int) -> bool { false }
unsafe extern "C" fn test_perfmon_capable() -> bool { false }

static TEST_TASK_OPS: TaskOps = TaskOps {
    current_comm: test_current_comm,
    task_comm: test_task_comm,
    task_pid_nr: test_task_pid_nr,
    task_pid: test_task_pid,
    pid_nr: test_pid_nr,
    put_pid: test_put_pid,
    capable: test_capable,
    perfmon_capable: test_perfmon_capable,
};

static MAIN_TASK_PTR: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

// The host harness reuses its worker thread across serial tests. B2b tests
// attach the scheduler's main task only for their own helper calls; do not
// leave that kernel-task identity in the host thread for later tests.
struct RestoreCurrentTask(*const ());

impl Drop for RestoreCurrentTask {
    fn drop(&mut self) {
        unsafe { axhal::percpu::set_current_task_ptr(self.0) };
    }
}

fn init_test_env() -> RestoreCurrentTask {
    crate::test_support::ensure_test_signal_provider();
    let previous = axhal::percpu::current_task_ptr::<()>();
    static SCHED_INIT: std::sync::Once = std::sync::Once::new();
    SCHED_INIT.call_once(|| {
        let _ = axtask::init_scheduler();
        let ptr = axhal::percpu::current_task_ptr::<()>() as usize;
        MAIN_TASK_PTR.store(ptr, core::sync::atomic::Ordering::SeqCst);
    });
    let ptr = MAIN_TASK_PTR.load(core::sync::atomic::Ordering::SeqCst);
    if ptr != 0 {
        unsafe { axhal::percpu::set_current_task_ptr(ptr as *const ()) };
    }
    let _ = tk_linux_kapi::kernel_services::install_task_ops(&TEST_TASK_OPS);
    RestoreCurrentTask(previous)
}




// ---------------------------------------------------------------------------
// Execution callback log for commit verification
// ---------------------------------------------------------------------------
static EXEC_LOG: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

fn clear_exec_log() {
    EXEC_LOG.lock().unwrap().clear();
}

fn log_exec(step: &'static str) {
    EXEC_LOG.lock().unwrap().push(step);
}

fn get_exec_log() -> Vec<&'static str> {
    EXEC_LOG.lock().unwrap().clone()
}

// ---------------------------------------------------------------------------
// Callbacks & Vtables
// ---------------------------------------------------------------------------
static CRTC_FUNCS: DrmCrtcFuncs = DrmCrtcFuncs {
    reset: Some(drm_atomic_helper_crtc_reset),
    cursor_set: None,
    cursor_set2: None,
    cursor_move: None,
    gamma_set: None,
    destroy: None,
    set_config: None,
    page_flip: None,
    page_flip_target: None,
    set_property: None,
    atomic_duplicate_state: Some(drm_atomic_helper_crtc_duplicate_state),
    atomic_destroy_state: Some(drm_atomic_helper_crtc_destroy_state),
    atomic_set_property: None,
    atomic_get_property: None,
    late_register: None,
    early_unregister: None,
    set_crc_source: None,
    verify_crc_source: None,
    get_crc_sources: None,
    atomic_print_state: None,
    get_vblank_counter: None,
    enable_vblank: None,
    disable_vblank: None,
    get_vblank_timestamp: None,
};

static PLANE_FUNCS: DrmPlaneFuncs = DrmPlaneFuncs {
    update_plane: None,
    disable_plane: None,
    destroy: None,
    reset: Some(drm_atomic_helper_plane_reset),
    set_property: None,
    atomic_duplicate_state: Some(drm_atomic_helper_plane_duplicate_state),
    atomic_destroy_state: Some(drm_atomic_helper_plane_destroy_state),
    atomic_set_property: None,
    atomic_get_property: None,
    late_register: None,
    early_unregister: None,
    atomic_print_state: None,
    format_mod_supported: None,
    format_mod_supported_async: None,
};

static CONNECTOR_FUNCS: DrmConnectorFuncs = DrmConnectorFuncs {
    dpms: None,
    reset: Some(drm_atomic_helper_connector_reset),
    detect: None,
    force: None,
    fill_modes: None,
    set_property: None,
    late_register: None,
    early_unregister: None,
    destroy: None,
    atomic_duplicate_state: Some(drm_atomic_helper_connector_duplicate_state),
    atomic_destroy_state: Some(drm_atomic_helper_connector_destroy_state),
    atomic_set_property: None,
    atomic_get_property: None,
    atomic_print_state: None,
    oob_hotplug_event: None,
    debugfs_init: None,
};

unsafe extern "C" fn test_crtc_atomic_disable(_crtc: *mut DrmCrtc, _state: *mut DrmAtomicCommit) {
    log_exec("atomic_disable");
}

unsafe extern "C" fn test_crtc_mode_set_nofb(_crtc: *mut DrmCrtc) {
    log_exec("mode_set_nofb");
}

unsafe extern "C" fn test_crtc_atomic_enable(_crtc: *mut DrmCrtc, _state: *mut DrmAtomicCommit) {
    log_exec("atomic_enable");
}

unsafe extern "C" fn test_plane_atomic_update(_plane: *mut DrmPlane, _state: *mut DrmAtomicCommit) {
    log_exec("atomic_update");
}

static CRTC_HELPER_FUNCS: DrmCrtcHelperFuncs = DrmCrtcHelperFuncs {
    dpms: None,
    prepare: None,
    commit: None,
    mode_valid: None,
    mode_fixup: None,
    mode_set: None,
    mode_set_nofb: Some(test_crtc_mode_set_nofb),
    mode_set_base: None,
    disable: None,
    atomic_check: None,
    atomic_begin: None,
    atomic_flush: None,
    atomic_enable: Some(test_crtc_atomic_enable),
    atomic_disable: Some(test_crtc_atomic_disable),
    get_scanout_position: None,
    handle_vblank_timeout: None,
};

static PLANE_HELPER_FUNCS: DrmPlaneHelperFuncs = DrmPlaneHelperFuncs {
    prepare_fb: None,
    cleanup_fb: None,
    begin_fb_access: None,
    end_fb_access: None,
    atomic_check: None,
    atomic_update: Some(test_plane_atomic_update),
    atomic_enable: None,
    atomic_disable: None,
    atomic_async_check: None,
    atomic_async_update: None,
    get_scanout_buffer: None,
    panic_flush: None,
};

static CONNECTOR_HELPER_FUNCS: DrmConnectorHelperFuncs = DrmConnectorHelperFuncs {
    get_modes: None,
    detect_ctx: None,
    mode_valid: None,
    mode_valid_ctx: None,
    best_encoder: None,
    atomic_best_encoder: None,
    atomic_check: None,
    atomic_commit: None,
    prepare_writeback_job: None,
    cleanup_writeback_job: None,
    enable_hpd: None,
    disable_hpd: None,
};

static ENCODER_HELPER_FUNCS: DrmEncoderHelperFuncs = DrmEncoderHelperFuncs {
    dpms: None,
    mode_valid: None,
    mode_fixup: None,
    prepare: None,
    commit: None,
    mode_set: None,
    atomic_mode_set: None,
    detect: None,
    atomic_disable: None,
    atomic_enable: None,
    disable: None,
    enable: None,
    atomic_check: None,
};

static MODE_CONFIG_FUNCS: DrmModeConfigFuncs = DrmModeConfigFuncs {
    fb_create: None,
    get_format_info: None,
    mode_valid: None,
    atomic_check: Some(drm_atomic_helper_check),
    atomic_commit: Some(drm_atomic_helper_commit),
    atomic_state_alloc: None,
    atomic_state_clear: None,
    atomic_state_free: None,
};

static MODE_CONFIG_HELPER_FUNCS: DrmModeConfigHelperFuncs = DrmModeConfigHelperFuncs {
    atomic_commit_tail: Some(drm_atomic_helper_commit_tail_rpm),
    atomic_commit_setup: None,
};

static FB_FUNCS: DrmFramebufferFuncs = DrmFramebufferFuncs {
    destroy: Some(drm_gem_fb_destroy),
    create_handle: None,
    dirty: None,
};

static SUPPORTED_FORMATS: [u32; 1] = [DRM_FORMAT_XRGB8888];

// ---------------------------------------------------------------------------
// TestFixture: 1 CRTC, 1 Plane, 1 Connector, 1 Encoder
// ---------------------------------------------------------------------------
struct TestFixture {
    drm: TestDrm,
    crtc: Box<DrmCrtc>,
    plane: Box<DrmPlane>,
    connector: Box<DrmConnector>,
    encoder: Box<DrmEncoder>,
}

impl TestFixture {
    fn new() -> Self {
        let _task_guard = init_test_env();
        let mut drm = TestDrm::new(DRIVER_MODESET | DRIVER_ATOMIC | DRIVER_GEM);
        drm.init_registry();
        let dev = drm.ptr();

        unsafe {
            let mc = core::ptr::addr_of_mut!((*dev).mode_config);
            drm_modeset_lock_init(core::ptr::addr_of_mut!((*mc).connection_mutex));
            INIT_LIST_HEAD(core::ptr::addr_of_mut!((*mc).crtc_list));
            INIT_LIST_HEAD(core::ptr::addr_of_mut!((*mc).plane_list));
            INIT_LIST_HEAD(core::ptr::addr_of_mut!((*mc).connector_list));
            INIT_LIST_HEAD(core::ptr::addr_of_mut!((*mc).encoder_list));
            INIT_LIST_HEAD(core::ptr::addr_of_mut!((*mc).privobj_list));
            INIT_LIST_HEAD(core::ptr::addr_of_mut!((*mc).fb_list));
            INIT_LIST_HEAD(core::ptr::addr_of_mut!((*mc).property_blob_list));
            tk_linux_kapi::mutex::mutex_init(core::ptr::addr_of_mut!((*mc).blob_lock));
            tk_linux_kapi::mutex::mutex_init(core::ptr::addr_of_mut!((*mc).fb_lock));
            (*mc).num_crtc = 1;
            (*mc).num_total_plane = 1;
            (*mc).num_connector = 1;
            (*mc).num_encoder = 1;
            (*mc).funcs = &MODE_CONFIG_FUNCS as *const _ as *mut _;
            (*mc).helper_private = &MODE_CONFIG_HELPER_FUNCS as *const _ as *mut _;
            tk_linux_kapi::locks::spin_lock_init(&mut (*mc).connector_list_lock);
            tk_linux_kapi::mutex::mutex_init(core::ptr::addr_of_mut!((*dev).master_mutex));
            tk_linux_kapi::mutex::mutex_init(core::ptr::addr_of_mut!((*dev).filelist_mutex));
            INIT_LIST_HEAD(core::ptr::addr_of_mut!((*dev).filelist));

            let primary_minor = Box::into_raw(Box::new(zeroed::<crate::drm_file_types::DrmMinor>()));
            (*primary_minor).dev = dev;
            (*primary_minor).r#type = 0;
            (*dev).primary = primary_minor;
        }

        let mut crtc: Box<DrmCrtc> = Box::new(unsafe { zeroed() });
        crtc.dev = dev;
        crtc.funcs = &CRTC_FUNCS as *const _ as *mut _;
        crtc.helper_private = &CRTC_HELPER_FUNCS as *const _ as *mut _;
        crtc.index = 0;
        unsafe {
            INIT_LIST_HEAD(core::ptr::addr_of_mut!(crtc.commit_list));
            tk_linux_kapi::locks::spin_lock_init(&mut crtc.commit_lock);
            drm_modeset_lock_init(core::ptr::addr_of_mut!(crtc.mutex));
            drm_atomic_helper_crtc_reset(&mut *crtc);
            let mc = core::ptr::addr_of_mut!((*dev).mode_config);
            tk_linux_kapi::list::list_add_tail(
                core::ptr::addr_of_mut!(crtc.head),
                core::ptr::addr_of_mut!((*mc).crtc_list),
            );
        }

        let mut plane: Box<DrmPlane> = Box::new(unsafe { zeroed() });
        plane.dev = dev;
        plane.funcs = &PLANE_FUNCS as *const _ as *mut _;
        plane.helper_private = &PLANE_HELPER_FUNCS as *const _ as *mut _;
        plane.index = 0;
        plane.r#type = DRM_PLANE_TYPE_PRIMARY;
        plane.format_types = SUPPORTED_FORMATS.as_ptr() as *mut u32;
        plane.format_count = SUPPORTED_FORMATS.len() as u32;
        unsafe {
            drm_modeset_lock_init(core::ptr::addr_of_mut!(plane.mutex));
            drm_atomic_helper_plane_reset(&mut *plane);
            let mc = core::ptr::addr_of_mut!((*dev).mode_config);
            tk_linux_kapi::list::list_add_tail(
                core::ptr::addr_of_mut!(plane.head),
                core::ptr::addr_of_mut!((*mc).plane_list),
            );
        }
        crtc.primary = &mut *plane as *mut DrmPlane;

        let mut encoder: Box<DrmEncoder> = Box::new(unsafe { zeroed() });
        encoder.dev = dev;
        encoder.index = 0;
        encoder.possible_crtcs = 1 << 0;
        encoder.possible_clones = 1 << 0;
        encoder.helper_private = &ENCODER_HELPER_FUNCS as *const _ as *mut _;
        unsafe {
            let mc = core::ptr::addr_of_mut!((*dev).mode_config);
            tk_linux_kapi::list::list_add_tail(
                core::ptr::addr_of_mut!(encoder.head),
                core::ptr::addr_of_mut!((*mc).encoder_list),
            );
        }

        let mut connector: Box<DrmConnector> = Box::new(unsafe { zeroed() });
        connector.dev = dev;
        connector.funcs = &CONNECTOR_FUNCS as *const _ as *mut _;
        connector.helper_private = &CONNECTOR_HELPER_FUNCS as *const _ as *mut _;
        connector.index = 0;
        connector.possible_encoders = 1 << 0;
        unsafe {
            tk_linux_kapi::memory::kref_init(&mut connector.base.refcount);
            drm_atomic_helper_connector_reset(&mut *connector);
            let mc = core::ptr::addr_of_mut!((*dev).mode_config);
            tk_linux_kapi::list::list_add_tail(
                core::ptr::addr_of_mut!(connector.head),
                core::ptr::addr_of_mut!((*mc).connector_list),
            );
        }

        Self {
            drm,
            crtc,
            plane,
            connector,
            encoder,
        }
    }

    fn dev(&mut self) -> *mut DrmDevice {
        self.drm.ptr()
    }
}

impl Drop for TestFixture {
    fn drop(&mut self) {
        unsafe {
            let dev = self.dev();
            if !(*dev).primary.is_null() {
                drop(Box::from_raw((*dev).primary));
                (*dev).primary = core::ptr::null_mut();
            }
            if !self.crtc.state.is_null() {
                drm_atomic_helper_crtc_destroy_state(&mut *self.crtc, self.crtc.state);
                self.crtc.state = core::ptr::null_mut();
            }
            if !self.plane.state.is_null() {
                drm_atomic_helper_plane_destroy_state(&mut *self.plane, self.plane.state);
                self.plane.state = core::ptr::null_mut();
            }
            if !self.connector.state.is_null() {
                drm_atomic_helper_connector_destroy_state(&mut *self.connector, self.connector.state);
                self.connector.state = core::ptr::null_mut();
            }
        }
    }
}

fn make_mode(hdisplay: u16, vdisplay: u16) -> DrmDisplayMode {
    let mut mode: DrmDisplayMode = unsafe { zeroed() };
    mode.hdisplay = hdisplay;
    mode.vdisplay = vdisplay;
    mode.crtc_hdisplay = hdisplay;
    mode.crtc_vdisplay = vdisplay;
    mode
}

// ---------------------------------------------------------------------------
// 1. drm_atomic_helper_check_plane_state
// ---------------------------------------------------------------------------
#[test]
fn test_check_plane_state() {
    let _task_guard = init_test_env();

    let mut crtc: DrmCrtc = unsafe { zeroed() };
    let mut crtc_state: DrmCrtcState = unsafe { zeroed() };
    crtc_state.crtc = &mut crtc;
    crtc_state.enable = true;
    crtc_state.mode = make_mode(1920, 1080);

    let mut dummy_format: DrmFormatInfo = unsafe { zeroed() };
    dummy_format.format = DRM_FORMAT_XRGB8888;
    dummy_format.num_planes = 1;

    let mut dummy_fb: DrmFramebuffer = unsafe { zeroed() };
    dummy_fb.width = 1920;
    dummy_fb.height = 1080;
    dummy_fb.format = &mut dummy_format as *mut DrmFormatInfo;

    let mut plane_state: DrmPlaneState = unsafe { zeroed() };
    plane_state.crtc = &mut crtc;
    plane_state.fb = &mut dummy_fb;
    plane_state.rotation = DRM_MODE_ROTATE_0;

    // Normal full-screen plane within bounds
    plane_state.crtc_x = 0;
    plane_state.crtc_y = 0;
    plane_state.crtc_w = 1920;
    plane_state.crtc_h = 1080;
    plane_state.src_x = 0;
    plane_state.src_y = 0;
    plane_state.src_w = 1920 << 16;
    plane_state.src_h = 1080 << 16;

    let ret = unsafe {
        drm_atomic_helper_check_plane_state(
            &mut plane_state,
            &crtc_state,
            1 << 16,
            1 << 16,
            true,
            false,
        )
    };
    assert_eq!(ret, 0);
    assert!(plane_state.visible);
    assert_eq!(plane_state.dst.x1, 0);
    assert_eq!(plane_state.dst.y1, 0);
    assert_eq!(plane_state.dst.x2, 1920);
    assert_eq!(plane_state.dst.y2, 1080);

    // Plane positioned partially off-screen (clipping)
    plane_state.crtc_x = 1000;
    plane_state.crtc_y = 500;
    plane_state.crtc_w = 1920;
    plane_state.crtc_h = 1080;
    plane_state.src_x = 0;
    plane_state.src_y = 0;
    plane_state.src_w = 1920 << 16;
    plane_state.src_h = 1080 << 16;

    let ret = unsafe {
        drm_atomic_helper_check_plane_state(
            &mut plane_state,
            &crtc_state,
            1 << 16,
            1 << 16,
            true,
            false,
        )
    };
    assert_eq!(ret, 0);
    assert!(plane_state.visible);
    assert_eq!(plane_state.dst.x1, 1000);
    assert_eq!(plane_state.dst.x2, 1920); // clipped at CRTC edge
    assert_eq!(plane_state.dst.y1, 500);
    assert_eq!(plane_state.dst.y2, 1080);

    // Plane completely off-screen -> invisible
    plane_state.crtc_x = 2000;
    plane_state.crtc_y = 2000;
    plane_state.crtc_w = 100;
    plane_state.crtc_h = 100;
    plane_state.src_x = 0;
    plane_state.src_y = 0;
    plane_state.src_w = 100 << 16;
    plane_state.src_h = 100 << 16;

    let ret = unsafe {
        drm_atomic_helper_check_plane_state(
            &mut plane_state,
            &crtc_state,
            1 << 16,
            1 << 16,
            true,
            false,
        )
    };
    assert_eq!(ret, 0);
    assert!(!plane_state.visible);

    // Positioning not allowed (can_position = false)
    plane_state.src_w = 1920 << 16;
    plane_state.src_h = 1080 << 16;
    plane_state.crtc_x = 10;
    plane_state.crtc_y = 0;
    plane_state.crtc_w = 1920;
    plane_state.crtc_h = 1080;
    let ret = unsafe {
        drm_atomic_helper_check_plane_state(
            &mut plane_state,
            &crtc_state,
            1 << 16,
            1 << 16,
            false,
            false,
        )
    };
    assert_eq!(ret, -EINVAL);

    // Scaling limit exceeded: 2x downscale when max_scale is 1.0
    plane_state.crtc_x = 0;
    plane_state.crtc_y = 0;
    plane_state.crtc_w = 960;
    plane_state.crtc_h = 540;
    plane_state.src_x = 0;
    plane_state.src_y = 0;
    plane_state.src_w = 1920 << 16;
    plane_state.src_h = 1080 << 16;
    let ret = unsafe {
        drm_atomic_helper_check_plane_state(
            &mut plane_state,
            &crtc_state,
            1 << 16,
            1 << 16,
            true,
            false,
        )
    };
    assert_eq!(ret, -ERANGE);

    // Disabled CRTC with can_update_disabled = false
    crtc_state.enable = false;
    let ret = unsafe {
        drm_atomic_helper_check_plane_state(
            &mut plane_state,
            &crtc_state,
            1 << 16,
            2 << 16,
            true,
            false,
        )
    };
    assert_eq!(ret, -EINVAL);

    // Disabled CRTC with can_update_disabled = true
    let ret = unsafe {
        drm_atomic_helper_check_plane_state(
            &mut plane_state,
            &crtc_state,
            1 << 16,
            2 << 16,
            true,
            true,
        )
    };
    assert_eq!(ret, 0);
    assert!(!plane_state.visible);
}

// ---------------------------------------------------------------------------
// 2. drm_atomic_helper_check_modeset + check_planes
// ---------------------------------------------------------------------------
#[test]
fn test_check_modeset_and_check_planes() {
    let _task_guard = init_test_env();

    let mut fixture = TestFixture::new();
    let dev = fixture.dev();

    let mut ctx: DrmModesetAcquireCtx = unsafe { zeroed() };
    unsafe { drm_modeset_acquire_init(&mut ctx, 0) };

    unsafe {
        let state = drm_atomic_commit_alloc(dev);
        assert!(!state.is_null());
        (*state).acquire_ctx = &mut ctx;

        let crtc_state = drm_atomic_get_crtc_state(state, &mut *fixture.crtc);
        assert!(!IS_ERR(crtc_state));
        (*crtc_state).enable = true;
        (*crtc_state).active = true;
        (*crtc_state).mode = make_mode(1920, 1080);

        let conn_state = drm_atomic_get_connector_state(state, &mut *fixture.connector);
        assert!(!IS_ERR(conn_state));
        let ret = drm_atomic_set_crtc_for_connector(conn_state, &mut *fixture.crtc);
        assert_eq!(ret, 0);

        let plane_state = drm_atomic_get_plane_state(state, &mut *fixture.plane);
        assert!(!IS_ERR(plane_state));
        let ret = drm_atomic_set_crtc_for_plane(plane_state, &mut *fixture.crtc);
        assert_eq!(ret, 0);

        // First check: new configuration
        let ret = drm_atomic_helper_check_modeset(dev, state);
        assert_eq!(ret, 0);
        assert!((*crtc_state).mode_changed());
        assert!((*crtc_state).active_changed());
        assert!((*crtc_state).connectors_changed());

        let ret = drm_atomic_helper_check_planes(dev, state);
        assert_eq!(ret, 0);

        // Commit this state to establish baseline
        let ret = drm_atomic_helper_swap_state(state, true);
        assert_eq!(ret, 0);
        drm_atomic_commit_put(state);

        // Second check: duplicate and keep identical mode
        let state2 = drm_atomic_commit_alloc(dev);
        (*state2).acquire_ctx = &mut ctx;
        let crtc_state2 = drm_atomic_get_crtc_state(state2, &mut *fixture.crtc);
        let conn_state2 = drm_atomic_get_connector_state(state2, &mut *fixture.connector);

        let ret = drm_atomic_helper_check_modeset(dev, state2);
        assert_eq!(ret, 0);
        assert!(!(*crtc_state2).mode_changed());
        assert!(!(*crtc_state2).active_changed());
        assert!(!(*crtc_state2).connectors_changed());

        // Now modify mode -> mode_changed becomes true
        (*crtc_state2).mode = make_mode(1280, 720);
        let ret = drm_atomic_helper_check_modeset(dev, state2);
        assert_eq!(ret, 0);
        assert!((*crtc_state2).mode_changed());

        // Modify link status -> connectors_changed becomes true
        (*conn_state2).link_status = DRM_MODE_LINK_STATUS_BAD as u32;
        let ret = drm_atomic_helper_check_modeset(dev, state2);
        assert_eq!(ret, 0);
        assert!((*crtc_state2).connectors_changed());

        drm_atomic_commit_put(state2);
        drm_modeset_drop_locks(&mut ctx);
        drm_modeset_acquire_fini(&mut ctx);
    }
}

// ---------------------------------------------------------------------------
// 3. Blocking drm_atomic_helper_commit callback order and crtc commit cleanup
// ---------------------------------------------------------------------------
#[test]
fn test_blocking_commit_callback_order_and_cleanup() {
    let _task_guard = init_test_env();
    clear_exec_log();

    let mut fixture = TestFixture::new();
    let dev = fixture.dev();

    let mut ctx: DrmModesetAcquireCtx = unsafe { zeroed() };
    unsafe { drm_modeset_acquire_init(&mut ctx, 0) };

    unsafe {
        let state = drm_atomic_commit_alloc(dev);
        (*state).acquire_ctx = &mut ctx;

        let crtc_state = drm_atomic_get_crtc_state(state, &mut *fixture.crtc);
        (*crtc_state).enable = true;
        (*crtc_state).active = true;
        (*crtc_state).mode = make_mode(1920, 1080);

        let conn_state = drm_atomic_get_connector_state(state, &mut *fixture.connector);
        drm_atomic_set_crtc_for_connector(conn_state, &mut *fixture.connector as *mut _ as *mut _);
        drm_atomic_set_crtc_for_connector(conn_state, &mut *fixture.crtc);

        let plane_state = drm_atomic_get_plane_state(state, &mut *fixture.plane);
        drm_atomic_set_crtc_for_plane(plane_state, &mut *fixture.crtc);

        let ret = drm_atomic_helper_check(dev, state);
        assert_eq!(ret, 0);

        // Perform full blocking commit
        let ret = drm_atomic_helper_commit(dev, state, false);
        assert_eq!(ret, 0);

        let log = get_exec_log();
        // Expected order: modeset_disables (noop on initial enable),
        // mode_set_nofb, atomic_enable, atomic_update
        assert!(log.contains(&"mode_set_nofb"));
        assert!(log.contains(&"atomic_enable"));
        assert!(log.contains(&"atomic_update"));

        let idx_mode = log.iter().position(|&s| s == "mode_set_nofb").unwrap();
        let idx_enable = log.iter().position(|&s| s == "atomic_enable").unwrap();
        let idx_update = log.iter().position(|&s| s == "atomic_update").unwrap();
        assert!(idx_mode < idx_enable);
        assert!(idx_enable < idx_update);

        // Verify CRTC commit list is clean
        let head = core::ptr::addr_of_mut!(fixture.crtc.commit_list);
        assert_eq!((*head).next, head);

        drm_atomic_commit_put(state);
        drm_modeset_drop_locks(&mut ctx);
        drm_modeset_acquire_fini(&mut ctx);
    }
}

// ---------------------------------------------------------------------------
// 4. Nonblocking commit dependencies
// ---------------------------------------------------------------------------
#[test]
fn test_nonblocking_commit_dependencies() {
    let _task_guard = init_test_env();

    let mut fixture = TestFixture::new();
    let dev = fixture.dev();

    let mut ctx: DrmModesetAcquireCtx = unsafe { zeroed() };
    unsafe { drm_modeset_acquire_init(&mut ctx, 0) };

    unsafe {
        // First commit: setup commit
        let state1 = drm_atomic_commit_alloc(dev);
        (*state1).acquire_ctx = &mut ctx;
        let crtc_state1 = drm_atomic_get_crtc_state(state1, &mut *fixture.crtc);
        (*crtc_state1).enable = true;
        (*crtc_state1).active = true;

        let ret = drm_atomic_helper_setup_commit(state1, true);
        assert_eq!(ret, 0);
        assert!(!(*crtc_state1).commit.is_null());
        let commit1 = (*crtc_state1).commit;

        // Swap state: moves commit1 to crtc.commit_list
        let ret = drm_atomic_helper_swap_state(state1, true);
        assert_eq!(ret, 0);

        // Second commit: depends on first
        let state2 = drm_atomic_commit_alloc(dev);
        (*state2).acquire_ctx = &mut ctx;
        let crtc_state2 = drm_atomic_get_crtc_state(state2, &mut *fixture.crtc);
        (*crtc_state2).enable = true;

        // Since commit1 has not signaled flip_done, stall_checks with nonblock=true fails with -EBUSY
        let ret = drm_atomic_helper_setup_commit(state2, true);
        assert_eq!(ret, -EBUSY);

        // Signal flip_done on commit1
        (*commit1).flip_done.complete_all();

        // Now setup_commit succeeds!
        let ret = drm_atomic_helper_setup_commit(state2, true);
        assert_eq!(ret, 0);

        // Complete hw_done and cleanup_done for both commits
        (*commit1).hw_done.complete_all();
        (*commit1).cleanup_done.complete_all();
        drm_atomic_helper_commit_cleanup_done(state1);

        if !(*crtc_state2).commit.is_null() {
            let commit2 = (*crtc_state2).commit;
            (*commit2).hw_done.complete_all();
            (*commit2).flip_done.complete_all();
            (*commit2).cleanup_done.complete_all();
            drm_atomic_helper_commit_cleanup_done(state2);
        }

        drm_atomic_commit_put(state1);
        drm_atomic_commit_put(state2);
        drm_modeset_drop_locks(&mut ctx);
        drm_modeset_acquire_fini(&mut ctx);
    }
}

// ---------------------------------------------------------------------------
// 5. drm_atomic_helper_suspend / resume roundtrip state
// ---------------------------------------------------------------------------
#[test]
fn test_suspend_resume_roundtrip() {
    let _task_guard = init_test_env();

    let mut fixture = TestFixture::new();
    let dev = fixture.dev();

    let mut ctx: DrmModesetAcquireCtx = unsafe { zeroed() };
    unsafe { drm_modeset_acquire_init(&mut ctx, 0) };

    unsafe {
        // Initial state: CRTC is enabled and active
        let state = drm_atomic_commit_alloc(dev);
        (*state).acquire_ctx = &mut ctx;
        let crtc_state = drm_atomic_get_crtc_state(state, &mut *fixture.crtc);
        let mode = make_mode(1920, 1080);
        let ret = drm_atomic_set_mode_for_crtc(crtc_state, &mode);
        assert_eq!(ret, 0);
        (*crtc_state).active = true;
        let conn_state = drm_atomic_get_connector_state(state, &mut *fixture.connector);
        drm_atomic_set_crtc_for_connector(conn_state, &mut *fixture.crtc);

        let ret = drm_atomic_helper_swap_state(state, true);
        assert_eq!(ret, 0);
        drm_atomic_commit_put(state);

        assert!((*fixture.crtc.state).enable);
        assert!((*fixture.crtc.state).active);

        drm_modeset_drop_locks(&mut ctx);
        drm_modeset_acquire_fini(&mut ctx);

        // Suspend device
        let saved_state = drm_atomic_helper_suspend(dev);
        if IS_ERR(saved_state) {
            panic!("saved_state error: {}", tk_linux_kapi::config::PTR_ERR(saved_state));
        }
        assert!(!saved_state.is_null());

        // After suspend, CRTC must be disabled
        assert!(!(*fixture.crtc.state).enable);
        assert!(!(*fixture.crtc.state).active);

        // Resume device
        let ret = drm_atomic_helper_resume(dev, saved_state);
        assert_eq!(ret, 0);

        // After resume, CRTC must be enabled and active again
        assert!((*fixture.crtc.state).enable);
        assert!((*fixture.crtc.state).active);
    }
}

// ---------------------------------------------------------------------------
// 6. GEM framebuffer helpers & prepare_fb
// ---------------------------------------------------------------------------
#[test]
fn test_gem_fb_helpers_and_prepare_fb() {
    let _task_guard = init_test_env();

    let mut fixture = TestFixture::new();
    let dev = fixture.dev();

    unsafe {
        let file_priv = drm_file_alloc((*dev).primary);
        assert!(!file_priv.is_null());

        // Allocate mock GEM object
        let mut gem_obj: Box<DrmGemObject> = Box::new(zeroed());
        let ret = drm_gem_object_init(dev, &mut *gem_obj, 64 * 64 * 4);
        assert_eq!(ret, 0);

        let mut handle = 0u32;
        let ret = drm_gem_handle_create(file_priv, &mut *gem_obj, &mut handle);
        assert_eq!(ret, 0);
        assert_ne!(handle, 0);

        let mut mode_cmd: DrmModeFbCmd2 = zeroed();
        mode_cmd.width = 64;
        mode_cmd.height = 64;
        mode_cmd.pixel_format = DRM_FORMAT_XRGB8888;
        mode_cmd.handles[0] = handle;
        mode_cmd.pitches[0] = 64 * 4;

        let info = crate::drm_fourcc_upstream::drm_format_info(DRM_FORMAT_XRGB8888);
        assert!(!info.is_null());

        // Create GEM framebuffer
        let fb = drm_gem_fb_create_with_funcs(dev, file_priv, info, &mode_cmd, &FB_FUNCS);
        assert!(!IS_ERR(fb));
        assert!(!fb.is_null());

        // Lookup GEM object from fb
        let looked_up_gem = drm_gem_fb_get_obj(fb, 0);
        assert_eq!(looked_up_gem, &mut *gem_obj as *mut DrmGemObject);

        // Test drm_gem_plane_helper_prepare_fb
        let mut plane_state: DrmPlaneState = zeroed();
        plane_state.plane = &mut *fixture.plane;
        plane_state.fb = fb;

        let ret = drm_gem_plane_helper_prepare_fb(&mut *fixture.plane, &mut plane_state);
        assert_eq!(ret, 0);

        // Cleanup
        drm_framebuffer_cleanup(fb);
        tk_linux_kapi::memory::kfree(fb.cast::<core::ffi::c_void>());

        assert_eq!(drm_gem_handle_delete(file_priv, handle), 0);
        drm_file_free(file_priv);
        drm_gem_object_release(&mut *gem_obj);
    }
}
