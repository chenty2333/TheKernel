// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_auth.c` (batch BI).
//!
//! Master and authentication management for DRM devices. Implements master
//! creation/destruction, current-master checks, magic generation and authentication,
//! and the SET_MASTER / DROP_MASTER ioctls.

#![allow(non_snake_case, dead_code, unsafe_code)]

use alloc::boxed::Box;
use tk_linux_kapi::assert::warn_on;
use tk_linux_kapi::config::{EBUSY, EINVAL, ENOMEM, GFP_KERNEL, CAP_SYS_ADMIN};
use tk_linux_kapi::kernel_services::capable;
use tk_linux_kapi::list::INIT_LIST_HEAD;
use tk_linux_kapi::locks::{spin_lock_raw, spin_unlock_raw};
use tk_linux_kapi::memory::{kfree, kref_init, kref_put};
#[cfg(not(test))]
use tk_linux_kapi::mutex::{mutex_lock, mutex_unlock};
#[cfg(test)]
unsafe fn mutex_lock<M>(_mutex: M) {}
#[cfg(test)]
unsafe fn mutex_unlock<M>(_mutex: M) {}
use tk_linux_kapi::types::Kref;
use tk_linux_kapi::idr::Idr;

use crate::drm_auth_types::DrmMaster;
use crate::drm_device_types::DrmDevice;
use crate::drm_file_types::DrmFile;
use crate::drm_lease_upstream::{drm_lease_destroy, drm_lease_owner, drm_lease_revoke};
use crate::drm_mode_object_upstream::{
    drm_core_check_feature, idr_alloc, idr_destroy, idr_find, idr_init, idr_init_base, idr_remove, idr_replace, DRIVER_MODESET,
};
use crate::uapi_drm_types::DrmAuth;

const EACCES: i32 = 13;

// upstream: drivers/gpu/drm/drm_auth.c drm_is_current_master_locked()
unsafe fn drm_is_current_master_locked(fpriv: *mut DrmFile) -> bool {
    unsafe {
        (*fpriv).is_master && !(*fpriv).master.is_null() && drm_lease_owner((*fpriv).master) == (*(*fpriv).minor).dev.as_ref().unwrap().master
    }
}

// upstream: drivers/gpu/drm/drm_auth.c drm_is_current_master()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_is_current_master(fpriv: *mut DrmFile) -> bool {
    if fpriv.is_null() {
        return false;
    }
    unsafe {
        spin_lock_raw(core::ptr::addr_of_mut!((*fpriv).master_lookup_lock));
        let ret = drm_is_current_master_locked(fpriv);
        spin_unlock_raw(core::ptr::addr_of_mut!((*fpriv).master_lookup_lock));
        ret
    }
}

// upstream: drivers/gpu/drm/drm_auth.c drm_getmagic()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_getmagic(
    dev: *mut DrmDevice,
    data: *mut core::ffi::c_void,
    file_priv: *mut DrmFile,
) -> i32 {
    let auth = data.cast::<DrmAuth>();
    let mut ret = 0;

    unsafe {
        mutex_lock(core::ptr::addr_of_mut!((*dev).master_mutex));
        if (*file_priv).magic == 0 {
            ret = idr_alloc(
                core::ptr::addr_of_mut!((*(*file_priv).master).magic_map),
                file_priv.cast(),
                1,
                0,
            );
            if ret >= 0 {
                (*file_priv).magic = ret as u32;
            }
        }
        (*auth).magic = (*file_priv).magic;
        mutex_unlock(core::ptr::addr_of_mut!((*dev).master_mutex));
    }

    if ret < 0 { ret } else { 0 }
}

// upstream: drivers/gpu/drm/drm_auth.c drm_authmagic()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_authmagic(
    dev: *mut DrmDevice,
    data: *mut core::ffi::c_void,
    file_priv: *mut DrmFile,
) -> i32 {
    let auth = data.cast::<DrmAuth>();

    unsafe {
        mutex_lock(core::ptr::addr_of_mut!((*dev).master_mutex));
        let file = idr_find(
            core::ptr::addr_of_mut!((*(*file_priv).master).magic_map),
            (*auth).magic as core::ffi::c_ulong,
        ).cast::<DrmFile>();
        if !file.is_null() {
            (*file).authenticated = true;
            idr_replace(
                core::ptr::addr_of_mut!((*(*file_priv).master).magic_map),
                core::ptr::null_mut(),
                (*auth).magic as i32,
            );
        }
        mutex_unlock(core::ptr::addr_of_mut!((*dev).master_mutex));

        if !file.is_null() { 0 } else { -EINVAL }
    }
}

// upstream: drivers/gpu/drm/drm_auth.c drm_master_create()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_master_create(dev: *mut DrmDevice) -> *mut DrmMaster {
    let master = Box::into_raw(Box::new(unsafe { core::mem::zeroed::<DrmMaster>() }));

    unsafe {
        (*master).dev = dev;
        kref_init(core::ptr::addr_of_mut!((*master).refcount));
        idr_init_base(core::ptr::addr_of_mut!((*master).magic_map), 1);
        INIT_LIST_HEAD(core::ptr::addr_of_mut!((*master).lessees));
        INIT_LIST_HEAD(core::ptr::addr_of_mut!((*master).lessee_list));
        idr_init(core::ptr::addr_of_mut!((*master).leases));
        idr_init_base(core::ptr::addr_of_mut!((*master).lessee_idr), 1);
    }

    master
}

// upstream: drivers/gpu/drm/drm_auth.c drm_set_master()
unsafe fn drm_set_master(dev: *mut DrmDevice, fpriv: *mut DrmFile, new_master: bool) {
    unsafe {
        (*dev).master = drm_master_get((*fpriv).master);
        if let Some(driver) = (*dev).driver.as_ref() {
            if let Some(master_set) = driver.master_set {
                master_set(dev, fpriv, new_master);
            }
        }
        (*fpriv).was_master = true;
    }
}

// upstream: drivers/gpu/drm/drm_auth.c drm_new_set_master()
unsafe fn drm_new_set_master(dev: *mut DrmDevice, fpriv: *mut DrmFile) -> i32 {
    unsafe {
        warn_on((*fpriv).is_master);
        let mut old_master = (*fpriv).master;
        let new_master = drm_master_create(dev);
        if new_master.is_null() {
            return -ENOMEM;
        }
        spin_lock_raw(core::ptr::addr_of_mut!((*fpriv).master_lookup_lock));
        (*fpriv).master = new_master;
        spin_unlock_raw(core::ptr::addr_of_mut!((*fpriv).master_lookup_lock));

        (*fpriv).is_master = true;
        (*fpriv).authenticated = true;

        drm_set_master(dev, fpriv, true);

        if !old_master.is_null() {
            drm_master_put(core::ptr::addr_of_mut!(old_master));
        }

        0
    }
}

// upstream: drivers/gpu/drm/drm_auth.c drm_master_check_perm()
unsafe fn drm_master_check_perm(_dev: *mut DrmDevice, file_priv: *mut DrmFile) -> i32 {
    unsafe {
        if (*file_priv).was_master {
            return 0;
        }
        if !capable(CAP_SYS_ADMIN) {
            return -EACCES;
        }
        0
    }
}

// upstream: drivers/gpu/drm/drm_auth.c drm_setmaster_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_setmaster_ioctl(
    dev: *mut DrmDevice,
    _data: *mut core::ffi::c_void,
    file_priv: *mut DrmFile,
) -> i32 {
    unsafe {
        mutex_lock(core::ptr::addr_of_mut!((*dev).master_mutex));
        let ret = drm_master_check_perm(dev, file_priv);
        if ret != 0 {
            mutex_unlock(core::ptr::addr_of_mut!((*dev).master_mutex));
            return ret;
        }

        if drm_is_current_master_locked(file_priv) {
            mutex_unlock(core::ptr::addr_of_mut!((*dev).master_mutex));
            return 0;
        }

        if !(*dev).master.is_null() {
            mutex_unlock(core::ptr::addr_of_mut!((*dev).master_mutex));
            return -EBUSY;
        }

        if (*file_priv).master.is_null() {
            mutex_unlock(core::ptr::addr_of_mut!((*dev).master_mutex));
            return -EINVAL;
        }

        if !(*file_priv).is_master {
            let res = drm_new_set_master(dev, file_priv);
            mutex_unlock(core::ptr::addr_of_mut!((*dev).master_mutex));
            return res;
        }

        if !(*(*file_priv).master).lessor.is_null() {
            mutex_unlock(core::ptr::addr_of_mut!((*dev).master_mutex));
            return -EINVAL;
        }

        drm_set_master(dev, file_priv, false);
        mutex_unlock(core::ptr::addr_of_mut!((*dev).master_mutex));
        0
    }
}

// upstream: drivers/gpu/drm/drm_auth.c drm_drop_master()
unsafe fn drm_drop_master(dev: *mut DrmDevice, fpriv: *mut DrmFile) {
    unsafe {
        if let Some(driver) = (*dev).driver.as_ref() {
            if let Some(master_drop) = driver.master_drop {
                master_drop(dev, fpriv);
            }
        }
        drm_master_put(core::ptr::addr_of_mut!((*dev).master));
    }
}

// upstream: drivers/gpu/drm/drm_auth.c drm_dropmaster_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_dropmaster_ioctl(
    dev: *mut DrmDevice,
    _data: *mut core::ffi::c_void,
    file_priv: *mut DrmFile,
) -> i32 {
    unsafe {
        mutex_lock(core::ptr::addr_of_mut!((*dev).master_mutex));
        let ret = drm_master_check_perm(dev, file_priv);
        if ret != 0 {
            mutex_unlock(core::ptr::addr_of_mut!((*dev).master_mutex));
            return ret;
        }

        if !drm_is_current_master_locked(file_priv) {
            mutex_unlock(core::ptr::addr_of_mut!((*dev).master_mutex));
            return -EINVAL;
        }

        if (*dev).master.is_null() {
            mutex_unlock(core::ptr::addr_of_mut!((*dev).master_mutex));
            return -EINVAL;
        }

        if !(*(*file_priv).master).lessor.is_null() {
            mutex_unlock(core::ptr::addr_of_mut!((*dev).master_mutex));
            return -EINVAL;
        }

        drm_drop_master(dev, file_priv);
        mutex_unlock(core::ptr::addr_of_mut!((*dev).master_mutex));
        0
    }
}

// upstream: drivers/gpu/drm/drm_auth.c drm_master_open()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_master_open(file_priv: *mut DrmFile) -> i32 {
    unsafe {
        let dev = (*(*file_priv).minor).dev;
        let mut ret = 0;
        mutex_lock(core::ptr::addr_of_mut!((*dev).master_mutex));
        if (*dev).master.is_null() {
            ret = drm_new_set_master(dev, file_priv);
        } else {
            spin_lock_raw(core::ptr::addr_of_mut!((*file_priv).master_lookup_lock));
            (*file_priv).master = drm_master_get((*dev).master);
            spin_unlock_raw(core::ptr::addr_of_mut!((*file_priv).master_lookup_lock));
        }
        mutex_unlock(core::ptr::addr_of_mut!((*dev).master_mutex));
        ret
    }
}

// upstream: drivers/gpu/drm/drm_auth.c drm_master_release()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_master_release(file_priv: *mut DrmFile) {
    unsafe {
        let dev = (*(*file_priv).minor).dev;
        mutex_lock(core::ptr::addr_of_mut!((*dev).master_mutex));
        let master = (*file_priv).master;
        if (*file_priv).magic != 0 && !master.is_null() {
            idr_remove(core::ptr::addr_of_mut!((*master).magic_map), (*file_priv).magic as i32);
        }

        if drm_is_current_master_locked(file_priv) {
            if !(*dev).master.is_null() && (*dev).master == (*file_priv).master {
                drm_drop_master(dev, file_priv);
            }
        }

        if drm_core_check_feature(dev, DRIVER_MODESET) && (*file_priv).is_master && !master.is_null() {
            drm_lease_revoke(master);
        }

        if !(*file_priv).master.is_null() {
            drm_master_put(core::ptr::addr_of_mut!((*file_priv).master));
        }
        mutex_unlock(core::ptr::addr_of_mut!((*dev).master_mutex));
    }
}

// upstream: drivers/gpu/drm/drm_auth.c drm_master_get()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_master_get(master: *mut DrmMaster) -> *mut DrmMaster {
    if !master.is_null() {
        unsafe {
            tk_linux_kapi::memory::kref_get(core::ptr::addr_of_mut!((*master).refcount));
        }
    }
    master
}

// upstream: drivers/gpu/drm/drm_auth.c drm_file_get_master()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_file_get_master(file_priv: *mut DrmFile) -> *mut DrmMaster {
    if file_priv.is_null() {
        return core::ptr::null_mut();
    }
    unsafe {
        spin_lock_raw(core::ptr::addr_of_mut!((*file_priv).master_lookup_lock));
        let master = if !(*file_priv).master.is_null() {
            drm_master_get((*file_priv).master)
        } else {
            core::ptr::null_mut()
        };
        spin_unlock_raw(core::ptr::addr_of_mut!((*file_priv).master_lookup_lock));
        master
    }
}

// upstream: drivers/gpu/drm/drm_auth.c drm_master_destroy()
unsafe extern "C" fn drm_master_destroy(kref: *mut Kref) {
    let master = kref.cast::<DrmMaster>();
    unsafe {
        let dev = (*master).dev;
        if drm_core_check_feature(dev, DRIVER_MODESET) {
            drm_lease_destroy(master);
        }
        idr_destroy(core::ptr::addr_of_mut!((*master).magic_map));
        idr_destroy(core::ptr::addr_of_mut!((*master).leases));
        idr_destroy(core::ptr::addr_of_mut!((*master).lessee_idr));

        if !(*master).unique.is_null() {
            kfree((*master).unique);
        }
        drop(Box::from_raw(master));
    }
}

// upstream: drivers/gpu/drm/drm_auth.c drm_master_put()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_master_put(master: *mut *mut DrmMaster) {
    if !master.is_null() && !unsafe { (*master).is_null() } {
        unsafe {
            kref_put(core::ptr::addr_of_mut!((**master).refcount), drm_master_destroy);
            *master = core::ptr::null_mut();
        }
    }
}

// upstream: drivers/gpu/drm/drm_auth.c drm_master_internal_acquire()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_master_internal_acquire(dev: *mut DrmDevice) -> bool {
    if dev.is_null() {
        return false;
    }
    unsafe {
        mutex_lock(core::ptr::addr_of_mut!((*dev).master_mutex));
        if !(*dev).master.is_null() {
            mutex_unlock(core::ptr::addr_of_mut!((*dev).master_mutex));
            return false;
        }
        true
    }
}

// upstream: drivers/gpu/drm/drm_auth.c drm_master_internal_release()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_master_internal_release(dev: *mut DrmDevice) {
    if !dev.is_null() {
        unsafe {
            mutex_unlock(core::ptr::addr_of_mut!((*dev).master_mutex));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drm_drv_types::DrmDriver;
    use crate::drm_drv_upstream::{drm_dev_alloc, drm_dev_put};
    use crate::drm_file_upstream::{drm_file_alloc, drm_file_free};

    #[test]
    fn test_master_set_drop_and_magic() {
        unsafe {
            let mut driver: DrmDriver = core::mem::zeroed();
            let dev = drm_dev_alloc(&driver, core::ptr::null_mut());
            assert!(!dev.is_null());

            // 1. First file opened becomes master
            let file1 = drm_file_alloc((*dev).primary);
            assert!(!file1.is_null());
            assert!(drm_is_current_master(file1));
            assert!((*file1).authenticated);

            // 2. Second file opened is not master, and not authenticated
            let file2 = drm_file_alloc((*dev).primary);
            assert!(!file2.is_null());
            assert!(!drm_is_current_master(file2));
            assert!(!(*file2).authenticated);

            // 3. Magic authentication
            let mut auth_req = DrmAuth { magic: 0 };
            assert_eq!(drm_getmagic(dev, &mut auth_req as *mut _ as *mut _, file2), 0);
            assert!(auth_req.magic > 0);

            // Master (file1) authenticates file2's magic
            assert_eq!(drm_authmagic(dev, &mut auth_req as *mut _ as *mut _, file1), 0);
            assert!((*file2).authenticated);

            // Re-authenticating same magic returns -EINVAL (already consumed)
            assert_eq!(drm_authmagic(dev, &mut auth_req as *mut _ as *mut _, file1), -EINVAL);

            // 4. Drop master
            // file2 is not master and lacks CAP_SYS_ADMIN; dropping fails with -EACCES
            assert_eq!(drm_dropmaster_ioctl(dev, core::ptr::null_mut(), file2), -EACCES);

            // file1 drops master
            assert_eq!(drm_dropmaster_ioctl(dev, core::ptr::null_mut(), file1), 0);
            assert!(!drm_is_current_master(file1));
            assert!((*dev).master.is_null());

            // 5. Set master
            // file1 was master, so it can reclaim master via setmaster_ioctl
            assert_eq!(drm_setmaster_ioctl(dev, core::ptr::null_mut(), file1), 0);
            assert!(drm_is_current_master(file1));

            // file2 lacks permission so it cannot set master (-EACCES)
            assert_eq!(drm_setmaster_ioctl(dev, core::ptr::null_mut(), file2), -EACCES);

            // With was_master set, file2 has permission but fails because file1 is already master (-EBUSY)
            (*file2).was_master = true;
            assert_eq!(drm_setmaster_ioctl(dev, core::ptr::null_mut(), file2), -EBUSY);

            drm_file_free(file2);
            drm_file_free(file1);
            drm_dev_put(dev);
        }
    }
}
