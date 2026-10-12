// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_drv.c` (batch BI).
//!
//! DRM core device allocation, initialization, registration, device-managed
//! lifetime, unplug semantics, minors, and drm_dev_enter/exit.

#![allow(non_snake_case, dead_code, unsafe_code)]

use core::ffi::{c_char, c_int, c_void};
use alloc::boxed::Box;
use tk_linux_kapi::assert::warn_on;
use tk_linux_kapi::config::{EINVAL, ENODEV, ENOMEM, GFP_KERNEL};
use tk_linux_kapi::list::{INIT_LIST_HEAD, list_add_tail, list_del};
use tk_linux_kapi::locks::{spin_lock, spin_unlock, spin_lock_init};
use tk_linux_kapi::memory::{kfree, kref_init, kref_put};
use tk_linux_kapi::mutex::mutex_init;
#[cfg(not(test))]
use tk_linux_kapi::mutex::{mutex_lock, mutex_unlock};
#[cfg(test)]
unsafe fn mutex_lock<M>(_mutex: M) {}
#[cfg(test)]
unsafe fn mutex_unlock<M>(_mutex: M) {}
use tk_linux_kapi::types::{AtomicT, Kref, Mutex, Spinlock};
use tk_linux_kapi::idr::Idr;

use crate::drm_device_types::{DrmDevice, DrmDeviceManaged};
use crate::drm_drv_types::DrmDriver;
use crate::drm_file_types::DrmMinor;
use crate::drm_vma_manager_types::DrmVmaOffsetManager;
use crate::drm_vma_manager_upstream::{drm_vma_offset_manager_destroy, drm_vma_offset_manager_init};

unsafe fn idr_init_base(idr: *mut Idr, base: u32) {
    unsafe {
        crate::drm_mode_object_upstream::idr_init(idr);
        (*idr).idr_base = base;
        (*idr).idr_next = base;
    }
}

// upstream: drivers/gpu/drm/drm_drv.c drm_dev_enter()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_dev_enter(dev: *mut DrmDevice, idx: *mut c_int) -> bool {
    if dev.is_null() {
        return false;
    }
    unsafe {
        if !idx.is_null() {
            *idx = 0;
        }
        !(*dev).unplugged
    }
}

// upstream: drivers/gpu/drm/drm_drv.c drm_dev_exit()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_dev_exit(_idx: c_int) {}

// upstream: drivers/gpu/drm/drm_drv.c drm_dev_unplug()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_dev_unplug(dev: *mut DrmDevice) {
    if dev.is_null() {
        return;
    }
    unsafe {
        (*dev).unplugged = true;
    }
}

// upstream: drivers/gpu/drm/drm_drv.c drm_dev_set_dma_dev()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_dev_set_dma_dev(dev: *mut DrmDevice, dma_dev: *mut c_void) {
    if dev.is_null() {
        return;
    }
    unsafe {
        (*dev).dma_dev = dma_dev;
    }
}

// upstream: drivers/gpu/drm/drm_drv.c drm_dev_init()
unsafe fn drm_dev_init(
    dev: *mut DrmDevice,
    driver: *const DrmDriver,
    parent: *mut c_void,
) -> i32 {
    unsafe {
        kref_init(core::ptr::addr_of_mut!((*dev).r#ref));
        (*dev).dev = parent;
        (*dev).dma_dev = parent;
        (*dev).driver = driver.cast_mut();
        (*dev).driver_features = if !driver.is_null() { (*driver).driver_features } else { 0 };

        INIT_LIST_HEAD(core::ptr::addr_of_mut!((*dev).managed.resources));
        spin_lock_init(&mut (*dev).managed.lock);

        mutex_init(core::ptr::addr_of_mut!((*dev).master_mutex));
        mutex_init(core::ptr::addr_of_mut!((*dev).filelist_mutex));
        INIT_LIST_HEAD(core::ptr::addr_of_mut!((*dev).filelist));
        INIT_LIST_HEAD(core::ptr::addr_of_mut!((*dev).filelist_internal));
        mutex_init(core::ptr::addr_of_mut!((*dev).clientlist_mutex));
        INIT_LIST_HEAD(core::ptr::addr_of_mut!((*dev).clientlist));
        INIT_LIST_HEAD(core::ptr::addr_of_mut!((*dev).client_sysrq_list));

        spin_lock_init(&mut (*dev).event_lock);
        spin_lock_init(&mut (*dev).vblank_time_lock);
        spin_lock_init(&mut (*dev).vbl_lock);
        INIT_LIST_HEAD(core::ptr::addr_of_mut!((*dev).vblank_event_list));

        mutex_init(core::ptr::addr_of_mut!((*dev).object_name_lock));
        idr_init_base(core::ptr::addr_of_mut!((*dev).object_name_idr), 1);
        mutex_init(core::ptr::addr_of_mut!((*dev).gem_lru_mutex));

        // allocate vma_offset_manager
        let vma_mgr = Box::into_raw(Box::new(DrmVmaOffsetManager {
            vm_lock: core::mem::zeroed(),
            vm_addr_space_mm: core::mem::zeroed(),
        }));
        // DRM_FILE_PAGE_OFFSET_START = 0x10000000, SIZE = 0xE0000000
        drm_vma_offset_manager_init(vma_mgr, 0x10000000, 0xE0000000);
        (*dev).vma_offset_manager = vma_mgr;

        // primary and render minors
        let primary = Box::into_raw(Box::new(DrmMinor {
            index: 0,
            r#type: 0,
            kdev: core::ptr::null_mut(),
            dev,
            debugfs_symlink: core::ptr::null_mut(),
            debugfs_root: core::ptr::null_mut(),
        }));
        let render = Box::into_raw(Box::new(DrmMinor {
            index: 128,
            r#type: 2,
            kdev: core::ptr::null_mut(),
            dev,
            debugfs_symlink: core::ptr::null_mut(),
            debugfs_root: core::ptr::null_mut(),
        }));
        (*dev).primary = primary;
        (*dev).render = render;

        0
    }
}

// upstream: drivers/gpu/drm/drm_drv.c drm_dev_alloc()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_dev_alloc(
    driver: *const DrmDriver,
    parent: *mut c_void,
) -> *mut DrmDevice {
    let dev = Box::into_raw(Box::new(unsafe { core::mem::zeroed::<DrmDevice>() }));
    let ret = unsafe { drm_dev_init(dev, driver, parent) };
    if ret != 0 {
        unsafe { drop(Box::from_raw(dev)) };
        return core::ptr::null_mut();
    }
    dev
}

// upstream: drivers/gpu/drm/drm_drv.c drm_dev_release()
unsafe extern "C" fn drm_dev_release(refcount: *mut Kref) {
    let dev = refcount.cast::<u8>().wrapping_sub(core::mem::offset_of!(DrmDevice, r#ref)).cast::<DrmDevice>();
    unsafe {
        if let Some(driver) = (*dev).driver.as_ref() {
            if let Some(release) = driver.release {
                release(dev);
            }
        }
        if !(*dev).vma_offset_manager.is_null() {
            drm_vma_offset_manager_destroy((*dev).vma_offset_manager);
            drop(Box::from_raw((*dev).vma_offset_manager));
            (*dev).vma_offset_manager = core::ptr::null_mut();
        }
        if !(*dev).primary.is_null() {
            drop(Box::from_raw((*dev).primary));
            (*dev).primary = core::ptr::null_mut();
        }
        if !(*dev).render.is_null() {
            drop(Box::from_raw((*dev).render));
            (*dev).render = core::ptr::null_mut();
        }
        if !(*dev).unique.is_null() {
            kfree((*dev).unique);
        }
        drop(Box::from_raw(dev));
    }
}

// upstream: drivers/gpu/drm/drm_drv.c drm_dev_get()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_dev_get(dev: *mut DrmDevice) -> *mut DrmDevice {
    if !dev.is_null() {
        unsafe {
            tk_linux_kapi::memory::kref_get(core::ptr::addr_of_mut!((*dev).r#ref));
        }
    }
    dev
}

// upstream: drivers/gpu/drm/drm_drv.c drm_dev_put()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_dev_put(dev: *mut DrmDevice) {
    if !dev.is_null() {
        unsafe {
            kref_put(core::ptr::addr_of_mut!((*dev).r#ref), drm_dev_release);
        }
    }
}

// upstream: drivers/gpu/drm/drm_drv.c drm_put_dev()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_put_dev(dev: *mut DrmDevice) {
    unsafe { drm_dev_put(dev); }
}

// upstream: drivers/gpu/drm/drm_drv.c drm_dev_register()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_dev_register(dev: *mut DrmDevice, _flags: core::ffi::c_ulong) -> i32 {
    if dev.is_null() {
        return -EINVAL;
    }
    unsafe {
        (*dev).registered = true;
        0
    }
}

// upstream: drivers/gpu/drm/drm_drv.c drm_dev_unregister()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_dev_unregister(dev: *mut DrmDevice) {
    if dev.is_null() {
        return;
    }
    unsafe {
        (*dev).registered = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicBool, Ordering};

    static RELEASE_CALLED: AtomicBool = AtomicBool::new(false);
    unsafe extern "C" fn test_driver_release(_dev: *mut DrmDevice) {
        RELEASE_CALLED.store(true, Ordering::SeqCst);
    }

    #[test]
    fn test_device_lifecycle_and_enter_exit() {
        let mut driver: DrmDriver = unsafe { core::mem::zeroed() };
        driver.driver_features = 0;
        driver.release = Some(test_driver_release);
        RELEASE_CALLED.store(false, Ordering::SeqCst);

        unsafe {
            let dev = drm_dev_alloc(&driver, core::ptr::null_mut());
            assert!(!dev.is_null());
            assert!(!(*dev).primary.is_null());
            assert!(!(*dev).render.is_null());
            assert!(!(*dev).vma_offset_manager.is_null());

            // Test register
            assert_eq!(drm_dev_register(dev, 0), 0);
            assert!((*dev).registered);

            // Test enter/exit before unplug
            let mut idx: c_int = -1;
            assert!(drm_dev_enter(dev, &mut idx));
            assert_eq!(idx, 0);
            drm_dev_exit(idx);

            // Test unplug
            drm_dev_unplug(dev);
            assert!((*dev).unplugged);
            assert!(!drm_dev_enter(dev, &mut idx));

            // Test unregister
            drm_dev_unregister(dev);
            assert!(!(*dev).registered);

            // Test refcounting & release
            drm_dev_get(dev);
            drm_dev_put(dev);
            assert!(!RELEASE_CALLED.load(Ordering::SeqCst));

            drm_dev_put(dev);
            assert!(RELEASE_CALLED.load(Ordering::SeqCst));
        }
    }
}
