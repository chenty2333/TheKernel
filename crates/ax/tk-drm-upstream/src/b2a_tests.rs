// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Host unit tests for the batch B2a ports (drm_atomic, drm_atomic_uapi,
//! drm_atomic_state_helper).

use alloc::boxed::Box;
use core::ffi::c_void;
use core::mem::zeroed;

use tk_linux_kapi::config::{EINVAL, ENOENT, IS_ERR};
use tk_linux_kapi::list::INIT_LIST_HEAD;

use crate::drm_atomic_state_helper_upstream::*;
use crate::drm_atomic_types::{DrmPrivateObj, DrmPrivateState, DrmPrivateStateFuncs};
use crate::drm_atomic_uapi_upstream::*;
use crate::drm_atomic_upstream::*;
use crate::drm_connector_types::{DrmConnector, DrmConnectorFuncs};
use crate::drm_crtc_types::{DrmCrtc, DrmCrtcFuncs};
use crate::drm_device_types::DrmDevice;
use crate::drm_file_types::DrmFile;
use crate::drm_modeset_lock_types::DrmModesetAcquireCtx;
use crate::drm_modeset_lock_upstream::*;
use crate::drm_plane_types::{DrmPlane, DrmPlaneFuncs};
use crate::drm_property_types::DrmProperty;
use crate::test_support::TestDrm;
use crate::uapi_drm_consts::*;
use crate::uapi_drm_mode_types::DrmModeAtomic;

const DRIVER_MODESET: u32 = 1 << 1;
const DRIVER_ATOMIC: u32 = 1 << 4;

static TASK_A: u64 = 0xA;

fn set_task(task: &u64) {
    unsafe { axhal::percpu::set_current_task_ptr(task as *const u64 as *const ()) };
}

struct NoTaskOnDrop;

impl Drop for NoTaskOnDrop {
    fn drop(&mut self) {
        unsafe { axhal::percpu::set_current_task_ptr(core::ptr::null::<()>()) };
    }
}

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

struct TestFixture {
    drm: TestDrm,
    crtc: Box<DrmCrtc>,
    plane: Box<DrmPlane>,
    connector: Box<DrmConnector>,
}

impl TestFixture {
    fn new() -> Self {
        let mut drm = TestDrm::new(DRIVER_MODESET | DRIVER_ATOMIC);
        drm.init_registry();
        let dev = drm.ptr();

        unsafe {
            let mc = core::ptr::addr_of_mut!((*dev).mode_config);
            drm_modeset_lock_init(core::ptr::addr_of_mut!((*mc).connection_mutex));
            INIT_LIST_HEAD(core::ptr::addr_of_mut!((*mc).crtc_list));
            INIT_LIST_HEAD(core::ptr::addr_of_mut!((*mc).plane_list));
            INIT_LIST_HEAD(core::ptr::addr_of_mut!((*mc).connector_list));
            INIT_LIST_HEAD(core::ptr::addr_of_mut!((*mc).privobj_list));
            (*mc).num_crtc = 1;
            (*mc).num_total_plane = 1;
            (*mc).num_connector = 1;
        }

        let mut crtc: Box<DrmCrtc> = Box::new(unsafe { zeroed() });
        crtc.dev = dev;
        crtc.funcs = &CRTC_FUNCS as *const _ as *mut _;
        unsafe {
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
        unsafe {
            drm_modeset_lock_init(core::ptr::addr_of_mut!(plane.mutex));
            drm_atomic_helper_plane_reset(&mut *plane);
            let mc = core::ptr::addr_of_mut!((*dev).mode_config);
            tk_linux_kapi::list::list_add_tail(
                core::ptr::addr_of_mut!(plane.head),
                core::ptr::addr_of_mut!((*mc).plane_list),
            );
        }

        let mut connector: Box<DrmConnector> = Box::new(unsafe { zeroed() });
        connector.dev = dev;
        connector.funcs = &CONNECTOR_FUNCS as *const _ as *mut _;
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
        }
    }

    fn dev(&mut self) -> *mut DrmDevice {
        self.drm.ptr()
    }
}

impl Drop for TestFixture {
    fn drop(&mut self) {
        unsafe {
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

#[test]
fn allocate_a_commit() {
    let mut fixture = TestFixture::new();
    let dev = fixture.dev();

    unsafe {
        let commit = drm_atomic_commit_alloc(dev);
        assert!(!commit.is_null());
        assert_eq!((*commit).dev, dev);
        assert_eq!((*commit).r#ref.refcount.refs.counter, 1);

        drm_atomic_commit_get(commit);
        assert_eq!((*commit).r#ref.refcount.refs.counter, 2);

        drm_atomic_commit_put(commit);
        assert_eq!((*commit).r#ref.refcount.refs.counter, 1);

        drm_atomic_commit_put(commit);
    }
}

#[test]
fn get_states_and_check_refcounts() {
    let _no_task = NoTaskOnDrop;
    set_task(&TASK_A);

    let mut fixture = TestFixture::new();
    let dev = fixture.dev();

    let mut ctx: DrmModesetAcquireCtx = unsafe { zeroed() };
    unsafe {
        drm_modeset_acquire_init(&mut ctx, 0);

        let commit = drm_atomic_commit_alloc(dev);
        assert!(!commit.is_null());
        (*commit).acquire_ctx = &mut ctx;

        let crtc_state = drm_atomic_get_crtc_state(commit, &mut *fixture.crtc);
        assert!(!IS_ERR(crtc_state));
        assert!(!crtc_state.is_null());
        assert_eq!((*crtc_state).crtc, fixture.crtc.as_mut() as *mut _);

        let old_crtc = drm_atomic_get_old_crtc_state(commit, &mut *fixture.crtc);
        let new_crtc = drm_atomic_get_new_crtc_state(commit, &mut *fixture.crtc);
        assert_eq!(old_crtc, fixture.crtc.state);
        assert_eq!(new_crtc, crtc_state);

        let plane_state = drm_atomic_get_plane_state(commit, &mut *fixture.plane);
        assert!(!IS_ERR(plane_state));
        assert!(!plane_state.is_null());
        assert_eq!((*plane_state).plane, fixture.plane.as_mut() as *mut _);

        let old_plane = drm_atomic_get_old_plane_state(commit, &mut *fixture.plane);
        let new_plane = drm_atomic_get_new_plane_state(commit, &mut *fixture.plane);
        assert_eq!(old_plane, fixture.plane.state);
        assert_eq!(new_plane, plane_state);

        let conn_state = drm_atomic_get_connector_state(commit, &mut *fixture.connector);
        assert!(!IS_ERR(conn_state));
        assert!(!conn_state.is_null());
        assert_eq!((*conn_state).connector, fixture.connector.as_mut() as *mut _);

        let old_conn = drm_atomic_get_old_connector_state(commit, &mut *fixture.connector);
        let new_conn = drm_atomic_get_new_connector_state(commit, &mut *fixture.connector);
        assert_eq!(old_conn, fixture.connector.state);
        assert_eq!(new_conn, conn_state);

        // Calling get second time should return the cached new_state.
        assert_eq!(drm_atomic_get_crtc_state(commit, &mut *fixture.crtc), crtc_state);
        assert_eq!(drm_atomic_get_plane_state(commit, &mut *fixture.plane), plane_state);
        assert_eq!(drm_atomic_get_connector_state(commit, &mut *fixture.connector), conn_state);

        // Put commit cleans up duplicated states and drops locks
        drm_atomic_commit_put(commit);
        drm_modeset_drop_locks(&mut ctx);
        drm_modeset_acquire_fini(&mut ctx);
    }
}

#[test]
fn check_only_rejects_invalid_plane_on_disabled_crtc() {
    let _no_task = NoTaskOnDrop;
    set_task(&TASK_A);

    let mut fixture = TestFixture::new();
    let dev = fixture.dev();

    let mut ctx: DrmModesetAcquireCtx = unsafe { zeroed() };
    unsafe {
        drm_modeset_acquire_init(&mut ctx, 0);

        let commit = drm_atomic_commit_alloc(dev);
        (*commit).acquire_ctx = &mut ctx;

        let crtc_state = drm_atomic_get_crtc_state(commit, &mut *fixture.crtc);
        let plane_state = drm_atomic_get_plane_state(commit, &mut *fixture.plane);
        assert!(!IS_ERR(crtc_state));
        assert!(!IS_ERR(plane_state));

        // Set plane to point to CRTC, but without an FB
        (*plane_state).crtc = &mut *fixture.crtc;
        (*plane_state).fb = core::ptr::null_mut();

        // Invalid: crtc set on plane but no FB
        let ret = drm_atomic_check_only(commit);
        assert_eq!(ret, -EINVAL);

        // Set crtc active while enable is false -> invalid crtc state
        (*plane_state).crtc = core::ptr::null_mut();
        (*crtc_state).enable = false;
        (*crtc_state).active = true;
        let ret2 = drm_atomic_check_only(commit);
        assert_eq!(ret2, -EINVAL);

        drm_atomic_commit_put(commit);
        drm_modeset_drop_locks(&mut ctx);
        drm_modeset_acquire_fini(&mut ctx);
    }
}

#[test]
fn test_only_via_ioctl_path() {
    let _no_task = NoTaskOnDrop;
    set_task(&TASK_A);

    let mut fixture = TestFixture::new();
    let dev = fixture.dev();

    let mut obj_id: u32 = 0;
    let mut prop_id: u32 = 99999; // Non-existent property
    let mut count_props: u32 = 1;
    let mut val: u64 = 1;

    let mut arg = DrmModeAtomic {
        flags: DRM_MODE_ATOMIC_TEST_ONLY,
        count_objs: 1,
        objs_ptr: core::ptr::addr_of_mut!(obj_id) as u64,
        count_props_ptr: core::ptr::addr_of_mut!(count_props) as u64,
        props_ptr: core::ptr::addr_of_mut!(prop_id) as u64,
        prop_values_ptr: core::ptr::addr_of_mut!(val) as u64,
        reserved: 0,
        user_data: 0,
    };

    let mut file: DrmFile = unsafe { zeroed() };
    file.atomic = true;

    unsafe {
        let ret = drm_mode_atomic_ioctl(dev, &mut arg as *mut _ as *mut c_void, &mut file);
        // Non-existent object 0 or property 99999 should be rejected with -ENOENT
        assert_eq!(ret, -ENOENT);
    }
}

#[test]
fn property_set_get_roundtrip() {
    let _no_task = NoTaskOnDrop;
    set_task(&TASK_A);

    let mut fixture = TestFixture::new();
    let dev = fixture.dev();

    let mut prop: DrmProperty = unsafe { zeroed() };
    unsafe {
        let mc = core::ptr::addr_of_mut!((*dev).mode_config);
        (*mc).prop_crtc_id = &mut prop;
    }

    let commit = unsafe { drm_atomic_commit_alloc(dev) };
    let mut ctx: DrmModesetAcquireCtx = unsafe { zeroed() };

    unsafe {
        drm_modeset_acquire_init(&mut ctx, 0);
        (*commit).acquire_ctx = &mut ctx;

        let plane_state = drm_atomic_get_plane_state(commit, &mut *fixture.plane);
        assert!(!IS_ERR(plane_state));

        // Use drm_atomic_plane_set_property helper logic: set crtc to NULL
        let ret = drm_atomic_set_crtc_for_plane(plane_state, core::ptr::null_mut());
        assert_eq!(ret, 0);
        assert!((*plane_state).crtc.is_null());

        let ret2 = drm_atomic_set_crtc_for_plane(plane_state, &mut *fixture.crtc);
        assert_eq!(ret2, 0);
        assert_eq!((*plane_state).crtc, fixture.crtc.as_mut() as *mut _);

        drm_atomic_commit_put(commit);
        drm_modeset_drop_locks(&mut ctx);
        drm_modeset_acquire_fini(&mut ctx);
    }
}

use core::sync::atomic::{AtomicUsize, Ordering};

static PRIV_DESTROY_COUNT: AtomicUsize = AtomicUsize::new(0);
static PRIV_DUP_COUNT: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn priv_duplicate_state(_obj: *mut DrmPrivateObj) -> *mut DrmPrivateState {
    PRIV_DUP_COUNT.fetch_add(1, Ordering::SeqCst);
    let state: Box<DrmPrivateState> = Box::new(unsafe { zeroed() });
    Box::into_raw(state)
}

unsafe extern "C" fn priv_destroy_state(_obj: *mut DrmPrivateObj, state: *mut DrmPrivateState) {
    PRIV_DESTROY_COUNT.fetch_add(1, Ordering::SeqCst);
    if !state.is_null() {
        unsafe { drop(Box::from_raw(state)) };
    }
}

static PRIV_FUNCS: DrmPrivateStateFuncs = DrmPrivateStateFuncs {
    atomic_duplicate_state: Some(priv_duplicate_state),
    atomic_destroy_state: Some(priv_destroy_state),
    atomic_print_state: None,
    atomic_create_state: None,
};

#[test]
fn private_object_state_lifecycle() {
    let _no_task = NoTaskOnDrop;
    set_task(&TASK_A);

    let mut fixture = TestFixture::new();
    let dev = fixture.dev();

    unsafe {
        PRIV_DESTROY_COUNT.store(0, Ordering::SeqCst);
        PRIV_DUP_COUNT.store(0, Ordering::SeqCst);

        let mut privobj: Box<DrmPrivateObj> = Box::new(zeroed());
        let ret = drm_atomic_private_obj_init(dev, &mut *privobj, &PRIV_FUNCS);
        assert_eq!(ret, 0);

        let initial_state: Box<DrmPrivateState> = Box::new(zeroed());
        privobj.state = Box::into_raw(initial_state);

        let mut ctx: DrmModesetAcquireCtx = zeroed();
        drm_modeset_acquire_init(&mut ctx, 0);

        let commit = drm_atomic_commit_alloc(dev);
        (*commit).acquire_ctx = &mut ctx;

        let state = drm_atomic_get_private_obj_state(commit, &mut *privobj);
        assert!(!IS_ERR(state));
        assert!(!state.is_null());
        assert_eq!(PRIV_DUP_COUNT.load(Ordering::SeqCst), 1);

        let old_state = drm_atomic_get_old_private_obj_state(commit, &mut *privobj);
        let new_state = drm_atomic_get_new_private_obj_state(commit, &mut *privobj);
        assert_eq!(old_state, privobj.state);
        assert_eq!(new_state, state);

        drm_atomic_commit_put(commit);
        assert_eq!(PRIV_DESTROY_COUNT.load(Ordering::SeqCst), 1);

        drm_modeset_drop_locks(&mut ctx);
        drm_modeset_acquire_fini(&mut ctx);

        drm_atomic_private_obj_fini(&mut *privobj);
        assert_eq!(PRIV_DESTROY_COUNT.load(Ordering::SeqCst), 2);
    }
}

#[test]
fn add_affected_connectors_test() {
    let _no_task = NoTaskOnDrop;
    set_task(&TASK_A);

    let mut fixture = TestFixture::new();
    let dev = fixture.dev();

    let mut ctx: DrmModesetAcquireCtx = unsafe { zeroed() };
    unsafe {
        drm_modeset_acquire_init(&mut ctx, 0);

        let commit = drm_atomic_commit_alloc(dev);
        (*commit).acquire_ctx = &mut ctx;

        let crtc_state = drm_atomic_get_crtc_state(commit, &mut *fixture.crtc);
        assert!(!IS_ERR(crtc_state));

        // Connect connector to crtc
        fixture.connector.index = 0;
        (*crtc_state).connector_mask = 1 << 0;

        let ret = drm_atomic_add_affected_connectors(commit, &mut *fixture.crtc);
        assert_eq!(ret, 0);

        // Connector should now be present in commit
        let conn_state = drm_atomic_get_new_connector_state(commit, &mut *fixture.connector);
        assert!(!conn_state.is_null());

        drm_atomic_commit_put(commit);
        drm_modeset_drop_locks(&mut ctx);
        drm_modeset_acquire_fini(&mut ctx);
    }
}
