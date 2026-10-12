// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_drv.c` (batch BI).
//!
//! DRM core device allocation, initialization, registration, device-managed
//! lifetime, unplug semantics, minors, and drm_dev_enter/exit.

#![allow(non_snake_case, dead_code, unsafe_code)]

use core::ffi::{c_char, c_int, c_uint, c_ulong, c_void};
use core::sync::atomic::Ordering;
use alloc::boxed::Box;
use tk_linux_kapi::assert::warn_on;
pub use tk_linux_kapi::config::ENODEV;
use tk_linux_kapi::config::{EINVAL, ENOMEM, EOPNOTSUPP, GFP_KERNEL};
use tk_linux_kapi::list::{INIT_LIST_HEAD, list_add_tail, list_del};
use tk_linux_kapi::locks::{spin_lock, spin_unlock, spin_lock_init};
use tk_linux_kapi::memory::{kfree, kmalloc, kref_init, kref_put};
use tk_linux_kapi::mutex::mutex_init;
#[cfg(not(test))]
use tk_linux_kapi::mutex::{mutex_lock, mutex_unlock};
#[cfg(test)]
unsafe fn mutex_lock<M>(_mutex: M) {}
#[cfg(test)]
unsafe fn mutex_unlock<M>(_mutex: M) {}
use tk_linux_kapi::types::{AtomicT, Kref, Mutex, Spinlock, XArray};
use tk_linux_kapi::idr::Idr;
use tk_linux_kapi::xarray::XA_FLAGS_ALLOC;

use crate::drm_device_types::{DrmDevice, DrmDeviceManaged};
use crate::drm_drv_types::DrmDriver;
use crate::drm_file_types::DrmMinor;
use crate::drm_vma_manager_types::DrmVmaOffsetManager;
use crate::drm_vma_manager_upstream::{drm_vma_offset_manager_destroy, drm_vma_offset_manager_init};

pub const DRM_MINOR_PRIMARY: c_int = 0;
pub const DRM_MINOR_CONTROL: c_int = 1;
pub const DRM_MINOR_RENDER: c_int = 2;
pub const DRM_MINOR_ACCEL: c_int = 3;

#[repr(C)]
pub struct DrmWedgeTaskInfo {
    pub pid: i32,
    pub comm: [u8; 16],
}

// upstream: drivers/gpu/drm/drm_drv.c drm_minors_xa
#[unsafe(no_mangle)]
pub static mut drm_minors_xa: XArray = XArray::new(XA_FLAGS_ALLOC);


// upstream: drivers/gpu/drm/drm_drv.c drm_minor_get_xa()
unsafe fn drm_minor_get_xa(r#type: c_int) -> *mut XArray {
    if r#type == DRM_MINOR_PRIMARY || r#type == DRM_MINOR_RENDER {
        core::ptr::addr_of_mut!(drm_minors_xa)
    } else {
        core::ptr::null_mut()
    }
}

// upstream: drivers/gpu/drm/drm_drv.c drm_minor_get_slot()
unsafe fn drm_minor_get_slot(dev: *mut DrmDevice, r#type: c_int) -> *mut *mut DrmMinor {
    if dev.is_null() {
        return core::ptr::null_mut();
    }
    match r#type {
        DRM_MINOR_PRIMARY => core::ptr::addr_of_mut!((*dev).primary),
        DRM_MINOR_RENDER => core::ptr::addr_of_mut!((*dev).render),
        DRM_MINOR_ACCEL => core::ptr::addr_of_mut!((*dev).accel),
        _ => core::ptr::null_mut(),
    }
}

// upstream: drivers/gpu/drm/drm_drv.c drm_minor_alloc_release()
unsafe fn drm_minor_alloc_release(dev: *mut DrmDevice, data: *mut c_void) {
    let minor = data.cast::<DrmMinor>();
    if !minor.is_null() {
        let xa = drm_minor_get_xa((*minor).r#type);
        if !xa.is_null() {
            tk_linux_kapi::xarray::xa_erase(xa, (*minor).index as c_ulong);
        }
    }
}

// upstream: drivers/gpu/drm/drm_drv.c drm_minor_alloc()
unsafe fn drm_minor_alloc(dev: *mut DrmDevice, r#type: c_int) -> i32 {
    let minor = kmalloc(core::mem::size_of::<DrmMinor>(), GFP_KERNEL).cast::<DrmMinor>();
    if minor.is_null() {
        return -ENOMEM;
    }
    core::ptr::write_bytes(minor, 0, 1);
    (*minor).r#type = r#type;
    (*minor).dev = dev;
    (*minor).index = if r#type == DRM_MINOR_RENDER { 128 } else { 0 };

    let slot = drm_minor_get_slot(dev, r#type);
    if !slot.is_null() {
        *slot = minor;
    }
    0
}

// upstream: drivers/gpu/drm/drm_drv.c drm_minor_register()
unsafe fn drm_minor_register(dev: *mut DrmDevice, r#type: c_int) -> i32 {
    let slot = drm_minor_get_slot(dev, r#type);
    if slot.is_null() || (*slot).is_null() {
        return 0;
    }
    let minor = *slot;
    let xa = drm_minor_get_xa(r#type);
    if !xa.is_null() {
        tk_linux_kapi::xarray::xa_store(xa, (*minor).index as c_ulong, minor.cast(), GFP_KERNEL);
    }
    0
}

// upstream: drivers/gpu/drm/drm_drv.c drm_minor_unregister()
unsafe fn drm_minor_unregister(dev: *mut DrmDevice, r#type: c_int) {
    let slot = drm_minor_get_slot(dev, r#type);
    if slot.is_null() || (*slot).is_null() {
        return;
    }
    let minor = *slot;
    let xa = drm_minor_get_xa(r#type);
    if !xa.is_null() {
        tk_linux_kapi::xarray::xa_erase(xa, (*minor).index as c_ulong);
    }
}

// upstream: drivers/gpu/drm/drm_drv.c drm_minor_acquire()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_minor_acquire(minor_xa: *mut XArray, minor_id: c_uint) -> *mut DrmMinor {
    if minor_xa.is_null() {
        return (-ENODEV as isize) as *mut DrmMinor;
    }
    let minor = tk_linux_kapi::xarray::xa_load::<XArray, DrmMinor>(&mut *minor_xa, minor_id);
    if minor.is_null() {
        return (-ENODEV as isize) as *mut DrmMinor;
    }
    if (*(*minor).dev).unplugged {
        return (-ENODEV as isize) as *mut DrmMinor;
    }
    drm_dev_get((*minor).dev);
    minor
}

// upstream: drivers/gpu/drm/drm_drv.c drm_minor_release()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_minor_release(minor: *mut DrmMinor) {
    if !minor.is_null() && !(*minor).dev.is_null() {
        drm_dev_put((*minor).dev);
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

// upstream: drivers/gpu/drm/drm_drv.c drm_get_wedge_recovery()
unsafe fn drm_get_wedge_recovery(opt: c_uint) -> *const c_char {
    match 1u32 << opt {
        1 => c"none".as_ptr(),
        2 => c"rebind".as_ptr(),
        4 => c"bus-reset".as_ptr(),
        8 => c"vendor-specific".as_ptr(),
        _ => core::ptr::null(),
    }
}

// upstream: drivers/gpu/drm/drm_drv.c drm_dev_wedged_event()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_dev_wedged_event(
    _dev: *mut DrmDevice,
    _method: c_ulong,
    _info: *mut DrmWedgeTaskInfo,
) -> i32 {
    0
}

// upstream: drivers/gpu/drm/drm_drv.c drm_fs_init_fs_context()
unsafe fn drm_fs_init_fs_context(_fc: *mut c_void) -> i32 {
    0
}

// upstream: drivers/gpu/drm/drm_drv.c drm_fs_inode_new()
unsafe fn drm_fs_inode_new() -> *mut c_void {
    let ptr = Box::into_raw(Box::new(0u64));
    ptr.cast()
}

// upstream: drivers/gpu/drm/drm_drv.c drm_fs_inode_free()
unsafe fn drm_fs_inode_free(inode: *mut c_void) {
    if !inode.is_null() {
        drop(Box::from_raw(inode.cast::<u64>()));
    }
}

unsafe fn idr_init_base(idr: *mut Idr, base: u32) {
    unsafe {
        crate::drm_mode_object_upstream::idr_init(idr);
        (*idr).idr_base = base;
        (*idr).idr_next = base;
    }
}

// upstream: drivers/gpu/drm/drm_drv.c drm_dev_init_release()
unsafe fn drm_dev_init_release(_dev: *mut DrmDevice, _res: *mut c_void) {}

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
        let vma_mgr = kmalloc(core::mem::size_of::<DrmVmaOffsetManager>(), GFP_KERNEL).cast::<DrmVmaOffsetManager>();
        core::ptr::write_bytes(vma_mgr, 0, 1);
        drm_vma_offset_manager_init(vma_mgr, 0x10000000, 0xE0000000);
        (*dev).vma_offset_manager = vma_mgr;

        drm_minor_alloc(dev, DRM_MINOR_PRIMARY);
        drm_minor_alloc(dev, DRM_MINOR_RENDER);

        (*dev).anon_inode = drm_fs_inode_new().cast();

        if ((*dev).driver_features & (1 << 0)) != 0 {
            crate::drm_gem_upstream::drm_gem_init(dev);
        }

        0
    }
}

// upstream: drivers/gpu/drm/drm_drv.c devm_drm_dev_init_release()
unsafe fn devm_drm_dev_init_release(data: *mut c_void) {
    drm_dev_put(data.cast());
}

// upstream: drivers/gpu/drm/drm_drv.c devm_drm_dev_init()
unsafe fn devm_drm_dev_init(
    parent: *mut c_void,
    dev: *mut DrmDevice,
    driver: *const DrmDriver,
) -> i32 {
    drm_dev_init(dev, driver, parent)
}

// upstream: drivers/gpu/drm/drm_drv.c __devm_drm_dev_alloc()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __devm_drm_dev_alloc(
    parent: *mut c_void,
    driver: *const DrmDriver,
    size: usize,
    offset: usize,
) -> *mut c_void {
    let container = kmalloc(size, GFP_KERNEL);
    if container.is_null() {
        return (-ENOMEM as isize) as *mut c_void;
    }
    core::ptr::write_bytes(container, 0, size);
    let dev = container.cast::<u8>().add(offset).cast::<DrmDevice>();
    let ret = drm_dev_init(dev, driver, parent);
    if ret != 0 {
        kfree(container);
        return (ret as isize) as *mut c_void;
    }
    (*dev).managed.final_kfree = container;
    container
}

// upstream: drivers/gpu/drm/drm_drv.c __drm_dev_alloc()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __drm_dev_alloc(
    parent: *mut c_void,
    driver: *const DrmDriver,
    size: usize,
    offset: usize,
) -> *mut c_void {
    let container = kmalloc(size, GFP_KERNEL);
    if container.is_null() {
        return (-ENOMEM as isize) as *mut c_void;
    }
    core::ptr::write_bytes(container, 0, size);
    let dev = container.cast::<u8>().add(offset).cast::<DrmDevice>();
    let ret = drm_dev_init(dev, driver, parent);
    if ret != 0 {
        kfree(container);
        return (ret as isize) as *mut c_void;
    }
    (*dev).managed.final_kfree = container;
    container
}

// upstream: drivers/gpu/drm/drm_drv.c drm_dev_alloc()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_dev_alloc(
    driver: *const DrmDriver,
    parent: *mut c_void,
) -> *mut DrmDevice {
    let ptr = __drm_dev_alloc(parent, driver, core::mem::size_of::<DrmDevice>(), 0);
    if (ptr as isize) < 0 && (ptr as isize) > -4096 {
        return core::ptr::null_mut();
    }
    ptr.cast()
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
            kfree((*dev).vma_offset_manager.cast::<c_void>());
            (*dev).vma_offset_manager = core::ptr::null_mut();
        }
        if !(*dev).primary.is_null() {
            kfree((*dev).primary.cast::<c_void>());
            (*dev).primary = core::ptr::null_mut();
        }
        if !(*dev).render.is_null() {
            kfree((*dev).render.cast::<c_void>());
            (*dev).render = core::ptr::null_mut();
        }
        if !(*dev).unique.is_null() {
            kfree((*dev).unique.cast::<c_void>());
        }
        if !(*dev).anon_inode.is_null() {
            drm_fs_inode_free((*dev).anon_inode.cast());
        }
        let final_kfree = (*dev).managed.final_kfree;
        if !final_kfree.is_null() {
            kfree(final_kfree);
        } else {
            kfree(dev.cast::<c_void>());
        }
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

// upstream: drivers/gpu/drm/drm_drv.c drmm_cg_unregister_region()
unsafe fn drmm_cg_unregister_region(_dev: *mut DrmDevice, _arg: *mut c_void) {}

// upstream: drivers/gpu/drm/drm_drv.c drmm_cgroup_register_region()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drmm_cgroup_register_region(
    _dev: *mut DrmDevice,
    _region_name: *const c_char,
    _size: u64,
) -> *mut c_void {
    core::ptr::null_mut()
}

// upstream: drivers/gpu/drm/drm_drv.c create_compat_control_link()
unsafe fn create_compat_control_link(_dev: *mut DrmDevice) -> i32 {
    0
}

// upstream: drivers/gpu/drm/drm_drv.c remove_compat_control_link()
unsafe fn remove_compat_control_link(_dev: *mut DrmDevice) {}

// upstream: drivers/gpu/drm/drm_drv.c drm_dev_register()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_dev_register(dev: *mut DrmDevice, flags: c_ulong) -> i32 {
    if dev.is_null() {
        return -EINVAL;
    }
    unsafe {
        let driver = (*dev).driver;
        if driver.is_null() {
            return -EINVAL;
        }
        if (*driver).load.is_none() {
            crate::drm_mode_config_upstream::drm_mode_config_validate(dev);
        }

        let need_global = crate::drm_file_upstream::drm_dev_needs_global_mutex(dev);
        if need_global {
            mutex_lock(core::ptr::addr_of_mut!(crate::drm_file_upstream::drm_global_mutex));
        }

        let mut ret = drm_minor_register(dev, DRM_MINOR_RENDER);
        if ret != 0 {
            if need_global {
                mutex_unlock(core::ptr::addr_of_mut!(crate::drm_file_upstream::drm_global_mutex));
            }
            return ret;
        }
        ret = drm_minor_register(dev, DRM_MINOR_PRIMARY);
        if ret != 0 {
            drm_minor_unregister(dev, DRM_MINOR_RENDER);
            if need_global {
                mutex_unlock(core::ptr::addr_of_mut!(crate::drm_file_upstream::drm_global_mutex));
            }
            return ret;
        }

        create_compat_control_link(dev);
        (*dev).registered = true;

        if let Some(load) = (*driver).load {
            ret = load(dev, flags as u64);
            if ret != 0 {
                remove_compat_control_link(dev);
                drm_minor_unregister(dev, DRM_MINOR_PRIMARY);
                drm_minor_unregister(dev, DRM_MINOR_RENDER);
                if need_global {
                    mutex_unlock(core::ptr::addr_of_mut!(crate::drm_file_upstream::drm_global_mutex));
                }
                return ret;
            }
        }

        if ((*dev).driver_features & (1 << 1)) != 0 {
            ret = crate::drm_mode_config_upstream::drm_modeset_register_all(dev);
            if ret != 0 {
                if let Some(unload) = (*driver).unload {
                    unload(dev);
                }
                remove_compat_control_link(dev);
                drm_minor_unregister(dev, DRM_MINOR_PRIMARY);
                drm_minor_unregister(dev, DRM_MINOR_RENDER);
                if need_global {
                    mutex_unlock(core::ptr::addr_of_mut!(crate::drm_file_upstream::drm_global_mutex));
                }
                return ret;
            }
        }

        crate::pending_stubs::drm_client_probe::drm_panic_register(dev);
        crate::pending_stubs::drm_client_probe::drm_client_sysrq_register(dev);

        if need_global {
            mutex_unlock(core::ptr::addr_of_mut!(crate::drm_file_upstream::drm_global_mutex));
        }
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
        crate::pending_stubs::drm_client_probe::drm_client_sysrq_unregister(dev);
        crate::pending_stubs::drm_client_probe::drm_panic_unregister(dev);
        crate::pending_stubs::drm_client_probe::drm_client_dev_unregister(dev);

        if ((*dev).driver_features & (1 << 1)) != 0 {
            crate::drm_mode_config_upstream::drm_modeset_unregister_all(dev);
        }

        if let Some(driver) = (*dev).driver.as_ref() {
            if let Some(unload) = driver.unload {
                unload(dev);
            }
        }

        remove_compat_control_link(dev);
        drm_minor_unregister(dev, DRM_MINOR_ACCEL);
        drm_minor_unregister(dev, DRM_MINOR_PRIMARY);
        drm_minor_unregister(dev, DRM_MINOR_RENDER);
    }
}

// upstream: drivers/gpu/drm/drm_drv.c drm_stub_open()
unsafe fn drm_stub_open(_inode: *mut c_void, _filp: *mut c_void) -> i32 {
    -ENODEV
}

// upstream: drivers/gpu/drm/drm_drv.c drm_core_exit()
unsafe fn drm_core_exit() {}

// upstream: drivers/gpu/drm/drm_drv.c drm_core_init()
unsafe fn drm_core_init() -> i32 {
    0
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

            // Test minor acquire/release
            let minor = drm_minor_acquire(core::ptr::addr_of_mut!(drm_minors_xa), 0);
            assert!(!minor.is_null());
            assert_eq!((*minor).dev, dev);
            drm_minor_release(minor);

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
