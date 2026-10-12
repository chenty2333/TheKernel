// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_lease.c` (batch BI).
//!
//! DRM display resource leasing. Allows DRM master to lease CRTCs, planes,
//! and connectors to lessee processes, granting them independent control
//! over subsets of display resources.

#![allow(non_snake_case, dead_code, unsafe_code)]

use tk_linux_kapi::config::{EINVAL, ENOMEM, EOPNOTSUPP};
use tk_linux_kapi::idr::{idr_get_next, Idr};
use tk_linux_kapi::list::{list_add_tail, list_del, list_empty, INIT_LIST_HEAD};
#[cfg(not(test))]
use tk_linux_kapi::mutex::{mutex_lock, mutex_unlock};
#[cfg(test)]
unsafe fn mutex_lock<M>(_mutex: M) {}
#[cfg(test)]
unsafe fn mutex_unlock<M>(_mutex: M) {}

use crate::drm_auth_types::DrmMaster;
use crate::drm_auth_upstream::{drm_file_get_master, drm_master_create, drm_master_get, drm_master_put};
use crate::drm_crtc_types::DrmCrtc;
use crate::drm_device_types::DrmDevice;
use crate::drm_file_types::DrmFile;
use crate::drm_mode_object_types::DrmModeObject;
use crate::drm_mode_object_upstream::{
    drm_core_check_feature, drm_mode_object_find, drm_mode_object_put,
    idr_alloc, idr_destroy, idr_find, idr_init, idr_remove, DRIVER_MODESET,
};
use crate::uapi_drm_consts::{DRM_MODE_OBJECT_CONNECTOR, DRM_MODE_OBJECT_CRTC, DRM_MODE_OBJECT_PLANE};
use crate::uapi_drm_mode_types::{DrmModeCreateLease, DrmModeGetLease, DrmModeListLessees, DrmModeRevokeLease};

const EACCES: i32 = 13;
const ENOENT: i32 = 2;

static mut DRM_LEASE_IDR_OBJECT: u64 = 1;

// upstream: drivers/gpu/drm/drm_lease.c drm_lease_owner()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_lease_owner(mut master: *mut DrmMaster) -> *mut DrmMaster {
    if master.is_null() {
        return core::ptr::null_mut();
    }
    unsafe {
        while !(*master).lessor.is_null() {
            master = (*master).lessor;
        }
        master
    }
}

// upstream: drivers/gpu/drm/drm_lease.c _drm_find_lessee()
unsafe fn _drm_find_lessee(master: *mut DrmMaster, lessee_id: i32) -> *mut DrmMaster {
    unsafe {
        let owner = drm_lease_owner(master);
        idr_find(core::ptr::addr_of_mut!((*owner).lessee_idr), lessee_id).cast()
    }
}

// upstream: drivers/gpu/drm/drm_lease.c _drm_lease_held_master()
unsafe fn _drm_lease_held_master(master: *mut DrmMaster, id: i32) -> bool {
    unsafe {
        if !(*master).lessor.is_null() {
            return !idr_find(core::ptr::addr_of_mut!((*master).leases), id).is_null();
        }
        true
    }
}

// upstream: drivers/gpu/drm/drm_lease.c _drm_has_leased()
unsafe fn _drm_has_leased(master: *mut DrmMaster, id: i32) -> bool {
    unsafe {
        let mut cur = (*master).lessees.next;
        while cur != core::ptr::addr_of_mut!((*master).lessees) {
            let lessee = cur.cast::<u8>().wrapping_sub(core::mem::offset_of!(DrmMaster, lessee_list)).cast::<DrmMaster>();
            if _drm_lease_held_master(lessee, id) {
                return true;
            }
            cur = (*cur).next;
        }
        false
    }
}

// upstream: drivers/gpu/drm/drm_lease.c _drm_lease_held()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn _drm_lease_held(file_priv: *mut DrmFile, id: i32) -> bool {
    if file_priv.is_null() {
        return true;
    }
    unsafe {
        let mut master = drm_file_get_master(file_priv);
        if master.is_null() {
            return true;
        }
        let ret = _drm_lease_held_master(master, id);
        drm_master_put(core::ptr::addr_of_mut!(master));
        ret
    }
}

// upstream: drivers/gpu/drm/drm_lease.c drm_lease_held()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_lease_held(file_priv: *mut DrmFile, id: i32) -> bool {
    if file_priv.is_null() {
        return true;
    }
    unsafe {
        let mut master = drm_file_get_master(file_priv);
        if master.is_null() {
            return true;
        }
        if (*master).lessor.is_null() {
            drm_master_put(core::ptr::addr_of_mut!(master));
            return true;
        }
        mutex_lock(core::ptr::addr_of_mut!((*(*master).dev).mode_config.idr_mutex));
        let ret = _drm_lease_held_master(master, id);
        mutex_unlock(core::ptr::addr_of_mut!((*(*master).dev).mode_config.idr_mutex));
        drm_master_put(core::ptr::addr_of_mut!(master));
        ret
    }
}

// upstream: drivers/gpu/drm/drm_lease.c drm_lease_filter_crtcs()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_lease_filter_crtcs(file_priv: *mut DrmFile, crtcs_in: u32) -> u32 {
    if file_priv.is_null() {
        return crtcs_in;
    }
    unsafe {
        let mut master = drm_file_get_master(file_priv);
        if master.is_null() {
            return crtcs_in;
        }
        if (*master).lessor.is_null() {
            drm_master_put(core::ptr::addr_of_mut!(master));
            return crtcs_in;
        }

        let dev = (*master).dev;
        let mut count_in = 0;
        let mut count_out = 0;
        let mut crtcs_out = 0;

        mutex_lock(core::ptr::addr_of_mut!((*dev).mode_config.idr_mutex));
        let mut cur = (*dev).mode_config.crtc_list.next;
        while cur != core::ptr::addr_of_mut!((*dev).mode_config.crtc_list) {
            let crtc = cur.cast::<u8>().wrapping_sub(core::mem::offset_of!(DrmCrtc, head)).cast::<DrmCrtc>();
            if _drm_lease_held_master(master, (*crtc).base.id as i32) {
                let mask_in = 1u32 << count_in;
                if (crtcs_in & mask_in) != 0 {
                    let mask_out = 1u32 << count_out;
                    crtcs_out |= mask_out;
                }
                count_out += 1;
            }
            count_in += 1;
            cur = (*cur).next;
        }
        mutex_unlock(core::ptr::addr_of_mut!((*dev).mode_config.idr_mutex));

        drm_master_put(core::ptr::addr_of_mut!(master));
        crtcs_out
    }
}

// upstream: drivers/gpu/drm/drm_lease.c drm_lease_create()
unsafe fn drm_lease_create(lessor: *mut DrmMaster, leases: *mut Idr) -> *mut DrmMaster {
    unsafe {
        let dev = (*lessor).dev;
        let lessee = drm_master_create(dev);
        if lessee.is_null() {
            return core::ptr::null_mut();
        }

        mutex_lock(core::ptr::addr_of_mut!((*dev).mode_config.idr_mutex));
        let owner = drm_lease_owner(lessor);
        let ret = idr_alloc(
            core::ptr::addr_of_mut!((*owner).lessee_idr),
            lessee.cast(),
            1,
            0,
        );
        if ret < 0 {
            mutex_unlock(core::ptr::addr_of_mut!((*dev).mode_config.idr_mutex));
            let mut l = lessee;
            drm_master_put(core::ptr::addr_of_mut!(l));
            return core::ptr::null_mut();
        }

        (*lessee).lessee_id = ret;
        (*lessee).lessor = drm_master_get(lessor);
        core::ptr::swap(core::ptr::addr_of_mut!((*lessee).leases), leases);

        list_add_tail(
            core::ptr::addr_of_mut!((*lessee).lessee_list),
            core::ptr::addr_of_mut!((*lessor).lessees),
        );
        mutex_unlock(core::ptr::addr_of_mut!((*dev).mode_config.idr_mutex));

        lessee
    }
}

// upstream: drivers/gpu/drm/drm_lease.c drm_lease_destroy()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_lease_destroy(master: *mut DrmMaster) {
    if master.is_null() {
        return;
    }
    unsafe {
        let mut lessor = (*master).lessor;
        if lessor.is_null() {
            return;
        }
        mutex_lock(core::ptr::addr_of_mut!((*(*master).dev).mode_config.idr_mutex));
        let owner = drm_lease_owner(lessor);
        idr_remove(core::ptr::addr_of_mut!((*owner).lessee_idr), (*master).lessee_id);
        list_del(core::ptr::addr_of_mut!((*master).lessee_list));
        mutex_unlock(core::ptr::addr_of_mut!((*(*master).dev).mode_config.idr_mutex));

        drm_master_put(core::ptr::addr_of_mut!(lessor));
        (*master).lessor = core::ptr::null_mut();
    }
}

// upstream: drivers/gpu/drm/drm_lease.c _drm_lease_revoke()
unsafe fn _drm_lease_revoke(top: *mut DrmMaster) {
    unsafe {
        let mut cur = (*top).lessees.next;
        while cur != core::ptr::addr_of_mut!((*top).lessees) {
            let lessee = cur.cast::<u8>().wrapping_sub(core::mem::offset_of!(DrmMaster, lessee_list)).cast::<DrmMaster>();
            _drm_lease_revoke(lessee);
            cur = (*cur).next;
        }
        let leases = core::ptr::addr_of_mut!((*top).leases);
        idr_destroy(leases);
        idr_init(leases);
    }
}

// upstream: drivers/gpu/drm/drm_lease.c drm_lease_revoke()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_lease_revoke(top: *mut DrmMaster) {
    if top.is_null() {
        return;
    }
    unsafe {
        mutex_lock(core::ptr::addr_of_mut!((*(*top).dev).mode_config.idr_mutex));
        _drm_lease_revoke(top);
        mutex_unlock(core::ptr::addr_of_mut!((*(*top).dev).mode_config.idr_mutex));
    }
}

// upstream: drivers/gpu/drm/drm_lease.c validate_lease()
unsafe fn validate_lease(
    _dev: *mut DrmDevice,
    object_count: i32,
    objects: *mut *mut DrmModeObject,
    _universal_planes: bool,
) -> i32 {
    let mut has_crtc = false;
    let mut has_connector = false;

    for i in 0..object_count as usize {
        let obj = unsafe { *objects.add(i) };
        if obj.is_null() {
            continue;
        }
        match unsafe { (*obj).r#type } {
            DRM_MODE_OBJECT_CRTC => has_crtc = true,
            DRM_MODE_OBJECT_CONNECTOR => has_connector = true,
            DRM_MODE_OBJECT_PLANE => {},
            _ => return -EINVAL,
        }
    }
    if object_count > 0 && (!has_crtc || !has_connector) {
        return -EINVAL;
    }
    0
}

// upstream: drivers/gpu/drm/drm_lease.c fill_object_idr()
unsafe fn fill_object_idr(
    dev: *mut DrmDevice,
    lessor_priv: *mut DrmFile,
    leases: *mut Idr,
    object_count: usize,
    object_ids: *const u32,
) -> i32 {
    unsafe {
        let mut objects: alloc::vec::Vec<*mut DrmModeObject> = alloc::vec::Vec::with_capacity(object_count);
        for o in 0..object_count {
            let object_id = *object_ids.add(o);
            let obj = drm_mode_object_find(dev, lessor_priv, object_id, 0);
            if obj.is_null() {
                for &done_obj in &objects {
                    if !done_obj.is_null() {
                        drm_mode_object_put(done_obj);
                    }
                }
                return -ENOENT;
            }
            if !_drm_lease_held_master((*lessor_priv).master, object_id as i32) ||
               _drm_has_leased((*lessor_priv).master, object_id as i32) {
                drm_mode_object_put(obj);
                for &done_obj in &objects {
                    if !done_obj.is_null() {
                        drm_mode_object_put(done_obj);
                    }
                }
                return -EACCES;
            }
            objects.push(obj);
        }

        let ret = validate_lease(dev, object_count as i32, objects.as_mut_ptr(), (*lessor_priv).universal_planes);
        if ret != 0 {
            for &done_obj in &objects {
                drm_mode_object_put(done_obj);
            }
            return ret;
        }

        for (o, &_obj) in objects.iter().enumerate() {
            let object_id = *object_ids.add(o);
            let ret = idr_alloc(leases, core::ptr::addr_of_mut!(DRM_LEASE_IDR_OBJECT).cast(), object_id as i32, object_id as i32 + 1);
            if ret < 0 {
                for &done_obj in &objects {
                    drm_mode_object_put(done_obj);
                }
                return ret;
            }
        }

        for &done_obj in &objects {
            drm_mode_object_put(done_obj);
        }
        0
    }
}

// upstream: drivers/gpu/drm/drm_lease.c drm_mode_create_lease_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_create_lease_ioctl(
    dev: *mut DrmDevice,
    data: *mut core::ffi::c_void,
    lessor_priv: *mut DrmFile,
) -> i32 {
    let cl = data.cast::<DrmModeCreateLease>();
    unsafe {
        if !drm_core_check_feature(dev, DRIVER_MODESET) {
            return -EOPNOTSUPP;
        }
        let mut lessor = drm_file_get_master(lessor_priv);
        if lessor.is_null() {
            return -EINVAL;
        }
        if !(*lessor).lessor.is_null() {
            drm_master_put(core::ptr::addr_of_mut!(lessor));
            return -EINVAL;
        }

        let object_count = (*cl).object_count as usize;
        let mut leases: Idr = core::mem::zeroed();
        idr_init(core::ptr::addr_of_mut!(leases));

        if object_count != 0 {
            let object_ids = (*cl).object_ids as *const u32;
            let ret = fill_object_idr(dev, lessor_priv, core::ptr::addr_of_mut!(leases), object_count, object_ids);
            if ret != 0 {
                idr_destroy(core::ptr::addr_of_mut!(leases));
                drm_master_put(core::ptr::addr_of_mut!(lessor));
                return ret;
            }
        }

        let lessee = drm_lease_create(lessor, core::ptr::addr_of_mut!(leases));
        if lessee.is_null() {
            idr_destroy(core::ptr::addr_of_mut!(leases));
            drm_master_put(core::ptr::addr_of_mut!(lessor));
            return -ENOMEM;
        }

        (*cl).lessee_id = (*lessee).lessee_id as u32;
        (*cl).fd = 0;

        drm_master_put(core::ptr::addr_of_mut!(lessor));
        0
    }
}

// upstream: drivers/gpu/drm/drm_lease.c drm_mode_list_lessees_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_list_lessees_ioctl(
    dev: *mut DrmDevice,
    data: *mut core::ffi::c_void,
    lessor_priv: *mut DrmFile,
) -> i32 {
    let arg = data.cast::<DrmModeListLessees>();
    unsafe {
        if !drm_core_check_feature(dev, DRIVER_MODESET) {
            return -EOPNOTSUPP;
        }
        let mut lessor = drm_file_get_master(lessor_priv);
        if lessor.is_null() {
            return -EINVAL;
        }

        mutex_lock(core::ptr::addr_of_mut!((*dev).mode_config.idr_mutex));
        let mut count = 0u32;
        let mut cur = (*lessor).lessees.next;
        let lessee_ids = (*arg).lessees_ptr as *mut u32;
        let count_lessees = (*arg).count_lessees;

        while cur != core::ptr::addr_of_mut!((*lessor).lessees) {
            let lessee = cur.cast::<u8>().wrapping_sub(core::mem::offset_of!(DrmMaster, lessee_list)).cast::<DrmMaster>();
            let mut dummy = 0;
            let first = idr_get_next(core::ptr::addr_of_mut!((*lessee).leases).cast(), &mut dummy);
            if !first.is_null() {
                if count_lessees > count && !lessee_ids.is_null() {
                    *lessee_ids.add(count as usize) = (*lessee).lessee_id as u32;
                }
                count += 1;
            }
            cur = (*cur).next;
        }
        (*arg).count_lessees = count;
        mutex_unlock(core::ptr::addr_of_mut!((*dev).mode_config.idr_mutex));

        drm_master_put(core::ptr::addr_of_mut!(lessor));
        0
    }
}

// upstream: drivers/gpu/drm/drm_lease.c drm_mode_get_lease_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_get_lease_ioctl(
    dev: *mut DrmDevice,
    data: *mut core::ffi::c_void,
    lessee_priv: *mut DrmFile,
) -> i32 {
    let arg = data.cast::<DrmModeGetLease>();
    unsafe {
        if !drm_core_check_feature(dev, DRIVER_MODESET) {
            return -EOPNOTSUPP;
        }
        let mut lessee = drm_file_get_master(lessee_priv);
        if lessee.is_null() {
            return -EINVAL;
        }

        mutex_lock(core::ptr::addr_of_mut!((*dev).mode_config.idr_mutex));
        let object_idr = if (*lessee).lessor.is_null() {
            core::ptr::addr_of_mut!((*dev).mode_config.object_idr)
        } else {
            core::ptr::addr_of_mut!((*lessee).leases)
        };

        let mut count = 0u32;
        let object_ids = (*arg).objects_ptr as *mut u32;
        let count_objects = (*arg).count_objects;

        let mut next = 0;
        loop {
            let entry = idr_get_next(object_idr.cast(), &mut next);
            if entry.is_null() {
                break;
            }
            if count_objects > count && !object_ids.is_null() {
                *object_ids.add(count as usize) = next as u32;
            }
            count += 1;
            next += 1;
        }

        (*arg).count_objects = count;
        mutex_unlock(core::ptr::addr_of_mut!((*dev).mode_config.idr_mutex));
        drm_master_put(core::ptr::addr_of_mut!(lessee));
        0
    }
}

// upstream: drivers/gpu/drm/drm_lease.c drm_mode_revoke_lease_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_revoke_lease_ioctl(
    dev: *mut DrmDevice,
    data: *mut core::ffi::c_void,
    lessor_priv: *mut DrmFile,
) -> i32 {
    let arg = data.cast::<DrmModeRevokeLease>();
    unsafe {
        if !drm_core_check_feature(dev, DRIVER_MODESET) {
            return -EOPNOTSUPP;
        }
        let mut lessor = drm_file_get_master(lessor_priv);
        if lessor.is_null() {
            return -EINVAL;
        }

        mutex_lock(core::ptr::addr_of_mut!((*dev).mode_config.idr_mutex));
        let lessee = _drm_find_lessee(lessor, (*arg).lessee_id as i32);
        if lessee.is_null() {
            mutex_unlock(core::ptr::addr_of_mut!((*dev).mode_config.idr_mutex));
            drm_master_put(core::ptr::addr_of_mut!(lessor));
            return -ENOENT;
        }
        if (*lessee).lessor != lessor {
            mutex_unlock(core::ptr::addr_of_mut!((*dev).mode_config.idr_mutex));
            drm_master_put(core::ptr::addr_of_mut!(lessor));
            return -EACCES;
        }

        _drm_lease_revoke(lessee);
        mutex_unlock(core::ptr::addr_of_mut!((*dev).mode_config.idr_mutex));
        drm_master_put(core::ptr::addr_of_mut!(lessor));
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drm_drv_types::DrmDriver;
    use crate::drm_drv_upstream::{drm_dev_alloc, drm_dev_put};
    use crate::drm_file_upstream::{drm_file_alloc, drm_file_free};

    #[test]
    fn test_lease_owner_and_held() {
        unsafe {
            let mut driver: DrmDriver = core::mem::zeroed();
            let dev = drm_dev_alloc(&driver, core::ptr::null_mut());
            let file_priv = drm_file_alloc((*dev).primary);
            let master = (*file_priv).master;

            // An unleased master is its own lease owner
            assert_eq!(drm_lease_owner(master), master);

            // An unleased master holds all objects
            assert!(drm_lease_held(file_priv, 42));

            drm_file_free(file_priv);
            drm_dev_put(dev);
        }
    }
}
