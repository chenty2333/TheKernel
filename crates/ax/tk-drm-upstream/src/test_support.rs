// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Host-test helpers for the DRM core ports: a `struct drm_device` with a
//! `struct drm_driver` attached, set up the way `drm_dev_init()` leaves it
//! (managed-resource list initialised), so the real port code can run on it.

extern crate std;

use std::boxed::Box;

use tk_linux_kapi::list::INIT_LIST_HEAD;
use tk_linux_kapi::locks::spin_lock_init;

use crate::{
    drm_device_types::DrmDevice,
    drm_drv_types::DrmDriver,
    drm_mode_config_upstream::{drm_mode_config_cleanup, drmm_mode_config_init},
};

/// A host `drm_device` plus the `drm_driver` it points at. Both are boxed so
/// their addresses stay fixed for the lifetime of the test.
pub struct TestDrm {
    pub dev: Box<DrmDevice>,
    pub driver: Box<DrmDriver>,
}

impl TestDrm {
    /// A device whose driver advertises `driver_features` (`DRIVER_*` bits).
    pub fn new(driver_features: u32) -> Self {
        // SAFETY: DRM core structs are plain C data; an all-zero image is the
        // state `kzalloc()` hands the drivers before `drm_dev_init()`.
        let mut driver: Box<DrmDriver> = Box::new(unsafe { core::mem::zeroed() });
        driver.driver_features = driver_features;

        let mut dev: Box<DrmDevice> = Box::new(unsafe { core::mem::zeroed() });
        dev.driver = &mut *driver;
        dev.driver_features = driver_features;
        // drm_dev_init(): the managed-resource list starts empty and dev starts with refcount 1.
        unsafe {
            INIT_LIST_HEAD(core::ptr::addr_of_mut!(dev.managed.resources));
            spin_lock_init(&mut dev.managed.lock);
            tk_linux_kapi::memory::kref_init(core::ptr::addr_of_mut!(dev.r#ref));
        }
        Self { dev, driver }
    }

    /// The raw device pointer handed to the C-ABI ports.
    pub fn ptr(&mut self) -> *mut DrmDevice {
        &mut *self.dev
    }

    /// The object-registry part of `drmm_mode_config_init()` only: the object
    /// id registry, the property and blob lists, and their mutexes. Used by
    /// tests that need ids and properties without the standard property set.
    pub fn init_registry(&mut self) {
        let ptr = self.ptr();
        // SAFETY: `ptr` is the live device owned by `self`; each call below
        // initialises one field of the device's mode_config in place.
        unsafe {
            let mc = core::ptr::addr_of_mut!((*ptr).mode_config);
            tk_linux_kapi::mutex::mutex_init(core::ptr::addr_of_mut!((*mc).idr_mutex));
            tk_linux_kapi::mutex::mutex_init(core::ptr::addr_of_mut!((*mc).blob_lock));
            crate::drm_mode_object_upstream::idr_init(core::ptr::addr_of_mut!((*mc).object_idr));
            INIT_LIST_HEAD(core::ptr::addr_of_mut!((*mc).property_list));
            INIT_LIST_HEAD(core::ptr::addr_of_mut!((*mc).property_blob_list));
        }
    }

    /// `drmm_mode_config_init()`: standard properties, id registries and lists.
    pub fn init_mode_config(&mut self) -> i32 {
        let ptr = self.ptr();
        // SAFETY: `ptr` is the live device owned by `self`.
        unsafe { drmm_mode_config_init(ptr) }
    }

    /// `drm_mode_config_cleanup()`: the explicit teardown the tests call.
    pub fn cleanup_mode_config(&mut self) {
        let ptr = self.ptr();
        // SAFETY: as above.
        unsafe { drm_mode_config_cleanup(ptr) }
    }
}
