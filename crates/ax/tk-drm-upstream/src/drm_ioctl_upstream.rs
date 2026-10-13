// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_ioctl.c` (batch BI).
//!
//! DRM core ioctl table with flags/permissions, dispatching and copy in/out
//! sizing rules, drm_ioctl_permit, and core ioctl implementations:
//! VERSION, GET_CAP, SET_CLIENT_CAP, GET_UNIQUE, SET_VERSION, GET_CLIENT, GET_STATS.

#![allow(non_snake_case, dead_code, unsafe_code)]

use core::ffi::{c_char, c_int, c_uint, c_ulong, c_void};
use alloc::boxed::Box;
use tk_linux_kapi::assert::warn_on;
use tk_linux_kapi::config::{EFAULT, EINVAL, ENODEV, ENOMEM, EOPNOTSUPP, GFP_KERNEL, CAP_SYS_ADMIN};
use tk_linux_kapi::kernel_services::capable;
use tk_linux_kapi::memory::{kfree, kmalloc};
#[cfg(not(test))]
use tk_linux_kapi::mutex::{mutex_lock, mutex_unlock};
#[cfg(test)]
unsafe fn mutex_lock<M>(_mutex: M) {}
#[cfg(test)]
unsafe fn mutex_unlock<M>(_mutex: M) {}
use tk_linux_kapi::types::File;

use crate::drm_auth_upstream::{drm_authmagic, drm_dropmaster_ioctl, drm_getmagic, drm_is_current_master, drm_setmaster_ioctl};
use crate::drm_device_types::DrmDevice;
use crate::drm_file_types::DrmFile;
use crate::drm_gem_upstream::{drm_gem_close_ioctl, drm_gem_flink_ioctl, drm_gem_open_ioctl};
use crate::drm_ioctl_types::{DrmIoctlDesc, DRM_AUTH, DRM_MASTER, DRM_RENDER_ALLOW, DRM_ROOT_ONLY};
use crate::drm_lease_upstream::{drm_mode_create_lease_ioctl, drm_mode_get_lease_ioctl, drm_mode_list_lessees_ioctl, drm_mode_revoke_lease_ioctl};
use crate::drm_mode_object_upstream::{drm_core_check_feature, DRIVER_ATOMIC, DRIVER_MODESET};
use crate::drm_prime_upstream::{drm_prime_fd_to_handle_ioctl, drm_prime_handle_to_fd_ioctl};
use crate::uapi_drm_consts::*;
use crate::uapi_drm_types::{
    DrmClient, DrmGetCap, DrmSetClientCap, DrmSetClientName, DrmSetVersion, DrmStats, DrmUnique, DrmVersion,
};

const EACCES: i32 = 13;
const ENOTTY: i32 = 25;
pub const DRIVER_SYNCOBJ: u32 = 1 << 5;
pub const DRIVER_SYNCOBJ_TIMELINE: u32 = 1 << 6;

type DrmIoctlFn = unsafe extern "C" fn(*mut DrmDevice, *mut c_void, *mut DrmFile) -> i32;

// upstream: drivers/gpu/drm/drm_ioctl.c drm_noop()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_noop(
    _dev: *mut DrmDevice,
    _data: *mut c_void,
    _file_priv: *mut DrmFile,
) -> i32 {
    0
}

// upstream: drivers/gpu/drm/drm_ioctl.c drm_invalid_op()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_invalid_op(
    _dev: *mut DrmDevice,
    _data: *mut c_void,
    _file_priv: *mut DrmFile,
) -> i32 {
    -EINVAL
}

// upstream: drivers/gpu/drm/drm_ioctl.c drm_copy_field()
unsafe fn drm_copy_field(buf: *mut c_char, buf_len: *mut usize, value: *const c_char) -> i32 {
    if value.is_null() {
        if !buf_len.is_null() {
            unsafe {
                *buf_len = 0;
            }
        }
        return 0;
    }
    let mut len = 0;
    unsafe {
        while *value.add(len) != 0 {
            len += 1;
        }
        if !buf_len.is_null() {
            let copy_len = (*buf_len).min(len);
            if !buf.is_null() && copy_len > 0 {
                core::ptr::copy_nonoverlapping(value, buf, copy_len);
            }
            *buf_len = len;
        }
    }
    0
}

// upstream: drivers/gpu/drm/drm_ioctl.c drm_version()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_version(
    dev: *mut DrmDevice,
    data: *mut c_void,
    _file_priv: *mut DrmFile,
) -> i32 {
    let v = data.cast::<DrmVersion>();
    if dev.is_null() || data.is_null() {
        return -EINVAL;
    }
    unsafe {
        if let Some(driver) = (*dev).driver.as_ref() {
            (*v).version_major = driver.major;
            (*v).version_minor = driver.minor;
            (*v).version_patchlevel = driver.patchlevel;
            let mut name_len = (*v).name_len;
            drm_copy_field((*v).name, &mut name_len, driver.name);
            (*v).name_len = name_len;
            let mut desc_len = (*v).desc_len;
            drm_copy_field((*v).desc, &mut desc_len, driver.desc);
            (*v).desc_len = desc_len;
            let mut date_len = (*v).date_len;
            let date_str = b"0\0".as_ptr().cast();
            drm_copy_field((*v).date, &mut date_len, date_str);
            (*v).date_len = date_len;
        }
        0
    }
}

// upstream: drivers/gpu/drm/drm_ioctl.c drm_getunique()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_getunique(
    dev: *mut DrmDevice,
    data: *mut c_void,
    file_priv: *mut DrmFile,
) -> i32 {
    let u = data.cast::<DrmUnique>();
    if dev.is_null() || data.is_null() || file_priv.is_null() {
        return -EINVAL;
    }
    unsafe {
        mutex_lock(core::ptr::addr_of_mut!((*dev).master_mutex));
        let master = (*file_priv).master;
        if master.is_null() {
            mutex_unlock(core::ptr::addr_of_mut!((*dev).master_mutex));
            return -EINVAL;
        }
        let master_len = (*master).unique_len as usize;
        if (*u).unique_len >= master_len && !(*master).unique.is_null() && !(*u).unique.is_null() {
            core::ptr::copy_nonoverlapping((*master).unique, (*u).unique, master_len);
        }
        (*u).unique_len = master_len;
        mutex_unlock(core::ptr::addr_of_mut!((*dev).master_mutex));
        0
    }
}

// upstream: drivers/gpu/drm/drm_ioctl.c drm_getclient()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_getclient(
    _dev: *mut DrmDevice,
    data: *mut c_void,
    file_priv: *mut DrmFile,
) -> i32 {
    let client = data.cast::<DrmClient>();
    if data.is_null() || file_priv.is_null() {
        return -EINVAL;
    }
    unsafe {
        if (*client).idx == 0 {
            (*client).auth = if (*file_priv).authenticated { 1 } else { 0 };
            (*client).pid = 0;
            (*client).uid = 0;
            (*client).magic = 0;
            (*client).iocs = 0;
            0
        } else {
            -EINVAL
        }
    }
}

// upstream: drivers/gpu/drm/drm_ioctl.c drm_getstats()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_getstats(
    _dev: *mut DrmDevice,
    data: *mut c_void,
    _file_priv: *mut DrmFile,
) -> i32 {
    if data.is_null() {
        return -EINVAL;
    }
    unsafe {
        core::ptr::write_bytes(data.cast::<u8>(), 0, core::mem::size_of::<DrmStats>());
        0
    }
}

// upstream: drivers/gpu/drm/drm_ioctl.c drm_getcap()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_getcap(
    dev: *mut DrmDevice,
    data: *mut c_void,
    _file_priv: *mut DrmFile,
) -> i32 {
    let req = data.cast::<DrmGetCap>();
    if dev.is_null() || data.is_null() {
        return -EINVAL;
    }
    unsafe {
        (*req).value = 0;
        match (*req).capability as u32 {
            DRM_CAP_TIMESTAMP_MONOTONIC => {
                (*req).value = 1;
                0
            }
            DRM_CAP_PRIME => {
                (*req).value = (DRM_PRIME_CAP_IMPORT | DRM_PRIME_CAP_EXPORT) as u64;
                0
            }
            DRM_CAP_SYNCOBJ => {
                (*req).value = if drm_core_check_feature(dev, DRIVER_SYNCOBJ) { 1 } else { 0 };
                0
            }
            DRM_CAP_SYNCOBJ_TIMELINE => {
                (*req).value = if drm_core_check_feature(dev, DRIVER_SYNCOBJ_TIMELINE) { 1 } else { 0 };
                0
            }
            _ => {
                if !drm_core_check_feature(dev, DRIVER_MODESET) {
                    return -EOPNOTSUPP;
                }
                match (*req).capability as u32 {
                    DRM_CAP_DUMB_BUFFER => {
                        if let Some(driver) = (*dev).driver.as_ref() {
                            if driver.dumb_create.is_some() {
                                (*req).value = 1;
                            }
                        }
                        0
                    }
                    DRM_CAP_VBLANK_HIGH_CRTC => {
                        (*req).value = 1;
                        0
                    }
                    DRM_CAP_CURSOR_WIDTH => {
                        (*req).value = if (*dev).mode_config.cursor_width != 0 {
                            (*dev).mode_config.cursor_width as u64
                        } else {
                            64
                        };
                        0
                    }
                    DRM_CAP_CURSOR_HEIGHT => {
                        (*req).value = if (*dev).mode_config.cursor_height != 0 {
                            (*dev).mode_config.cursor_height as u64
                        } else {
                            64
                        };
                        0
                    }
                    DRM_CAP_ADDFB2_MODIFIERS => {
                        (*req).value = if (*dev).mode_config.fb_modifiers_not_supported { 0 } else { 1 };
                        0
                    }
                    DRM_CAP_CRTC_IN_VBLANK_EVENT => {
                        (*req).value = 1;
                        0
                    }
                    _ => -EINVAL,
                }
            }
        }
    }
}

// upstream: drivers/gpu/drm/drm_ioctl.c drm_setclientcap()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_setclientcap(
    dev: *mut DrmDevice,
    data: *mut c_void,
    file_priv: *mut DrmFile,
) -> i32 {
    let req = data.cast::<DrmSetClientCap>();
    if dev.is_null() || data.is_null() || file_priv.is_null() {
        return -EINVAL;
    }
    unsafe {
        if !drm_core_check_feature(dev, DRIVER_MODESET) {
            return -EOPNOTSUPP;
        }
        match (*req).capability as u32 {
            DRM_CLIENT_CAP_STEREO_3D => {
                if (*req).value > 1 { return -EINVAL; }
                (*file_priv).stereo_allowed = (*req).value != 0;
            }
            DRM_CLIENT_CAP_UNIVERSAL_PLANES => {
                if (*req).value > 1 { return -EINVAL; }
                (*file_priv).universal_planes = (*req).value != 0;
            }
            DRM_CLIENT_CAP_ATOMIC => {
                if !drm_core_check_feature(dev, DRIVER_ATOMIC) {
                    return -EOPNOTSUPP;
                }
                if (*req).value > 2 { return -EINVAL; }
                (*file_priv).atomic = (*req).value != 0;
                (*file_priv).universal_planes = (*req).value != 0;
                (*file_priv).aspect_ratio_allowed = (*req).value != 0;
            }
            DRM_CLIENT_CAP_ASPECT_RATIO => {
                if (*req).value > 1 { return -EINVAL; }
                (*file_priv).aspect_ratio_allowed = (*req).value != 0;
            }
            DRM_CLIENT_CAP_WRITEBACK_CONNECTORS => {
                if !(*file_priv).atomic { return -EINVAL; }
                if (*req).value > 1 { return -EINVAL; }
                (*file_priv).writeback_connectors = (*req).value != 0;
            }
            _ => return -EINVAL,
        }
        0
    }
}

// upstream: drivers/gpu/drm/drm_ioctl.c drm_setversion()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_setversion(
    dev: *mut DrmDevice,
    data: *mut c_void,
    _file_priv: *mut DrmFile,
) -> i32 {
    let sv = data.cast::<DrmSetVersion>();
    if dev.is_null() || data.is_null() {
        return -EINVAL;
    }
    unsafe {
        mutex_lock(core::ptr::addr_of_mut!((*dev).master_mutex));
        let mut ret = 0;
        if (*sv).drm_di_major != -1 {
            if (*sv).drm_di_major != 1 || (*sv).drm_di_minor < 0 || (*sv).drm_di_minor > 4 {
                ret = -EINVAL;
            }
        }
        (*sv).drm_di_major = 1;
        (*sv).drm_di_minor = 4;
        if let Some(driver) = (*dev).driver.as_ref() {
            (*sv).drm_dd_major = driver.major;
            (*sv).drm_dd_minor = driver.minor;
        }
        mutex_unlock(core::ptr::addr_of_mut!((*dev).master_mutex));
        ret
    }
}

// upstream: drivers/gpu/drm/drm_ioctl.c drm_set_client_name()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_set_client_name(
    _dev: *mut DrmDevice,
    _data: *mut c_void,
    _file_priv: *mut DrmFile,
) -> i32 {
    0
}

// upstream: drivers/gpu/drm/drm_ioctl.c drm_ioctl_permit()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_ioctl_permit(flags: u32, file_priv: *mut DrmFile) -> i32 {
    if file_priv.is_null() {
        return -EINVAL;
    }
    unsafe {
        if (flags & DRM_ROOT_ONLY != 0) && !capable(CAP_SYS_ADMIN) {
            return -EACCES;
        }
        let is_render = (*(*file_priv).minor).r#type == 2;
        if (flags & DRM_AUTH != 0) && !is_render && !(*file_priv).authenticated {
            return -EACCES;
        }
        if (flags & DRM_MASTER != 0) && !drm_is_current_master(file_priv) {
            return -EACCES;
        }
        if (flags & DRM_RENDER_ALLOW == 0) && is_render {
            return -EACCES;
        }
        0
    }
}

// Core ioctl dispatcher lookup
macro_rules! ioctl_entry {
    ($cmd:ident, $func:ident, $flags:expr) => {
        DrmIoctlDesc {
            cmd: $cmd,
            flags: $flags,
            func: $func as *mut c_void,
            name: core::ptr::null_mut(),
        }
    };
}

unsafe extern "C" {
    fn drm_wait_vblank_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_mode_getresources(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_mode_getplane_res(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_mode_getcrtc(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_mode_setcrtc(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_mode_getplane(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_mode_setplane(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_mode_cursor_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_mode_cursor2_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_mode_gamma_get_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_mode_gamma_set_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_mode_getencoder(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_mode_getconnector(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_mode_getproperty_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_connector_property_set_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_mode_getblob_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_mode_createblob_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_mode_destroyblob_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_mode_getfb(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_mode_getfb2_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_mode_addfb_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_mode_addfb2_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_mode_rmfb_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_mode_closefb_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_mode_page_flip_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_mode_dirtyfb_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_mode_create_dumb_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_mode_mmap_dumb_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_mode_destroy_dumb_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_mode_obj_get_properties_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_mode_obj_set_property_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_mode_atomic_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_syncobj_create_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_syncobj_destroy_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_syncobj_handle_to_fd_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_syncobj_fd_to_handle_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_syncobj_transfer_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_syncobj_wait_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_syncobj_timeline_wait_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_syncobj_eventfd_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_syncobj_reset_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_syncobj_signal_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_syncobj_timeline_signal_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_syncobj_query_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_crtc_get_sequence_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
    fn drm_crtc_queue_sequence_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32;
}

// upstream: drivers/gpu/drm/drm_ioctl.c drm_ioctls
pub static DRM_IOCTLS: &[DrmIoctlDesc] = &[
    ioctl_entry!(DRM_IOCTL_VERSION, drm_version, DRM_RENDER_ALLOW),
    ioctl_entry!(DRM_IOCTL_GET_UNIQUE, drm_getunique, 0),
    ioctl_entry!(DRM_IOCTL_GET_MAGIC, drm_getmagic, 0),
    ioctl_entry!(DRM_IOCTL_GET_CLIENT, drm_getclient, 0),
    ioctl_entry!(DRM_IOCTL_GET_STATS, drm_getstats, 0),
    ioctl_entry!(DRM_IOCTL_GET_CAP, drm_getcap, DRM_RENDER_ALLOW),
    ioctl_entry!(DRM_IOCTL_SET_CLIENT_CAP, drm_setclientcap, 0),
    ioctl_entry!(DRM_IOCTL_SET_VERSION, drm_setversion, DRM_MASTER),
    ioctl_entry!(DRM_IOCTL_SET_UNIQUE, drm_invalid_op, DRM_AUTH | DRM_MASTER | DRM_ROOT_ONLY),
    ioctl_entry!(DRM_IOCTL_BLOCK, drm_noop, DRM_AUTH | DRM_MASTER | DRM_ROOT_ONLY),
    ioctl_entry!(DRM_IOCTL_UNBLOCK, drm_noop, DRM_AUTH | DRM_MASTER | DRM_ROOT_ONLY),
    ioctl_entry!(DRM_IOCTL_AUTH_MAGIC, drm_authmagic, DRM_MASTER),
    ioctl_entry!(DRM_IOCTL_SET_MASTER, drm_setmaster_ioctl, 0),
    ioctl_entry!(DRM_IOCTL_DROP_MASTER, drm_dropmaster_ioctl, 0),
    ioctl_entry!(DRM_IOCTL_ADD_DRAW, drm_noop, DRM_AUTH | DRM_MASTER | DRM_ROOT_ONLY),
    ioctl_entry!(DRM_IOCTL_RM_DRAW, drm_noop, DRM_AUTH | DRM_MASTER | DRM_ROOT_ONLY),
    ioctl_entry!(DRM_IOCTL_FINISH, drm_noop, DRM_AUTH),
    ioctl_entry!(DRM_IOCTL_WAIT_VBLANK, drm_wait_vblank_ioctl, 0),
    ioctl_entry!(DRM_IOCTL_UPDATE_DRAW, drm_noop, DRM_AUTH | DRM_MASTER | DRM_ROOT_ONLY),
    ioctl_entry!(DRM_IOCTL_GEM_CLOSE, drm_gem_close_ioctl, DRM_RENDER_ALLOW),
    ioctl_entry!(DRM_IOCTL_GEM_FLINK, drm_gem_flink_ioctl, DRM_AUTH),
    ioctl_entry!(DRM_IOCTL_GEM_OPEN, drm_gem_open_ioctl, DRM_AUTH),
    ioctl_entry!(DRM_IOCTL_GEM_CHANGE_HANDLE, drm_invalid_op, DRM_RENDER_ALLOW),
    ioctl_entry!(DRM_IOCTL_MODE_GETRESOURCES, drm_mode_getresources, 0),
    ioctl_entry!(DRM_IOCTL_PRIME_HANDLE_TO_FD, drm_prime_handle_to_fd_ioctl, DRM_RENDER_ALLOW),
    ioctl_entry!(DRM_IOCTL_PRIME_FD_TO_HANDLE, drm_prime_fd_to_handle_ioctl, DRM_RENDER_ALLOW),
    ioctl_entry!(DRM_IOCTL_SET_CLIENT_NAME, drm_set_client_name, DRM_RENDER_ALLOW),
    ioctl_entry!(DRM_IOCTL_MODE_GETPLANERESOURCES, drm_mode_getplane_res, 0),
    ioctl_entry!(DRM_IOCTL_MODE_GETCRTC, drm_mode_getcrtc, 0),
    ioctl_entry!(DRM_IOCTL_MODE_SETCRTC, drm_mode_setcrtc, DRM_MASTER),
    ioctl_entry!(DRM_IOCTL_MODE_GETPLANE, drm_mode_getplane, 0),
    ioctl_entry!(DRM_IOCTL_MODE_SETPLANE, drm_mode_setplane, DRM_MASTER),
    ioctl_entry!(DRM_IOCTL_MODE_CURSOR, drm_mode_cursor_ioctl, DRM_MASTER),
    ioctl_entry!(DRM_IOCTL_MODE_GETGAMMA, drm_mode_gamma_get_ioctl, 0),
    ioctl_entry!(DRM_IOCTL_MODE_SETGAMMA, drm_mode_gamma_set_ioctl, DRM_MASTER),
    ioctl_entry!(DRM_IOCTL_MODE_GETENCODER, drm_mode_getencoder, 0),
    ioctl_entry!(DRM_IOCTL_MODE_GETCONNECTOR, drm_mode_getconnector, 0),
    ioctl_entry!(DRM_IOCTL_MODE_ATTACHMODE, drm_noop, DRM_MASTER),
    ioctl_entry!(DRM_IOCTL_MODE_DETACHMODE, drm_noop, DRM_MASTER),
    ioctl_entry!(DRM_IOCTL_MODE_GETPROPERTY, drm_mode_getproperty_ioctl, 0),
    ioctl_entry!(DRM_IOCTL_MODE_SETPROPERTY, drm_connector_property_set_ioctl, DRM_MASTER),
    ioctl_entry!(DRM_IOCTL_MODE_GETPROPBLOB, drm_mode_getblob_ioctl, 0),
    ioctl_entry!(DRM_IOCTL_MODE_GETFB, drm_mode_getfb, 0),
    ioctl_entry!(DRM_IOCTL_MODE_GETFB2, drm_mode_getfb2_ioctl, 0),
    ioctl_entry!(DRM_IOCTL_MODE_ADDFB, drm_mode_addfb_ioctl, 0),
    ioctl_entry!(DRM_IOCTL_MODE_ADDFB2, drm_mode_addfb2_ioctl, 0),
    ioctl_entry!(DRM_IOCTL_MODE_RMFB, drm_mode_rmfb_ioctl, 0),
    ioctl_entry!(DRM_IOCTL_MODE_CLOSEFB, drm_mode_closefb_ioctl, 0),
    ioctl_entry!(DRM_IOCTL_MODE_PAGE_FLIP, drm_mode_page_flip_ioctl, DRM_MASTER),
    ioctl_entry!(DRM_IOCTL_MODE_DIRTYFB, drm_mode_dirtyfb_ioctl, DRM_MASTER),
    ioctl_entry!(DRM_IOCTL_MODE_CREATE_DUMB, drm_mode_create_dumb_ioctl, 0),
    ioctl_entry!(DRM_IOCTL_MODE_MAP_DUMB, drm_mode_mmap_dumb_ioctl, 0),
    ioctl_entry!(DRM_IOCTL_MODE_DESTROY_DUMB, drm_mode_destroy_dumb_ioctl, 0),
    ioctl_entry!(DRM_IOCTL_MODE_OBJ_GETPROPERTIES, drm_mode_obj_get_properties_ioctl, 0),
    ioctl_entry!(DRM_IOCTL_MODE_OBJ_SETPROPERTY, drm_mode_obj_set_property_ioctl, DRM_MASTER),
    ioctl_entry!(DRM_IOCTL_MODE_CURSOR2, drm_mode_cursor2_ioctl, DRM_MASTER),
    ioctl_entry!(DRM_IOCTL_MODE_ATOMIC, drm_mode_atomic_ioctl, DRM_MASTER),
    ioctl_entry!(DRM_IOCTL_MODE_CREATEPROPBLOB, drm_mode_createblob_ioctl, 0),
    ioctl_entry!(DRM_IOCTL_MODE_DESTROYPROPBLOB, drm_mode_destroyblob_ioctl, 0),
    ioctl_entry!(DRM_IOCTL_SYNCOBJ_CREATE, drm_syncobj_create_ioctl, DRM_RENDER_ALLOW),
    ioctl_entry!(DRM_IOCTL_SYNCOBJ_DESTROY, drm_syncobj_destroy_ioctl, DRM_RENDER_ALLOW),
    ioctl_entry!(DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD, drm_syncobj_handle_to_fd_ioctl, DRM_RENDER_ALLOW),
    ioctl_entry!(DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE, drm_syncobj_fd_to_handle_ioctl, DRM_RENDER_ALLOW),
    ioctl_entry!(DRM_IOCTL_SYNCOBJ_TRANSFER, drm_syncobj_transfer_ioctl, DRM_RENDER_ALLOW),
    ioctl_entry!(DRM_IOCTL_SYNCOBJ_WAIT, drm_syncobj_wait_ioctl, DRM_RENDER_ALLOW),
    ioctl_entry!(DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT, drm_syncobj_timeline_wait_ioctl, DRM_RENDER_ALLOW),
    ioctl_entry!(DRM_IOCTL_SYNCOBJ_EVENTFD, drm_syncobj_eventfd_ioctl, DRM_RENDER_ALLOW),
    ioctl_entry!(DRM_IOCTL_SYNCOBJ_RESET, drm_syncobj_reset_ioctl, DRM_RENDER_ALLOW),
    ioctl_entry!(DRM_IOCTL_SYNCOBJ_SIGNAL, drm_syncobj_signal_ioctl, DRM_RENDER_ALLOW),
    ioctl_entry!(DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL, drm_syncobj_timeline_signal_ioctl, DRM_RENDER_ALLOW),
    ioctl_entry!(DRM_IOCTL_SYNCOBJ_QUERY, drm_syncobj_query_ioctl, DRM_RENDER_ALLOW),
    ioctl_entry!(DRM_IOCTL_CRTC_GET_SEQUENCE, drm_crtc_get_sequence_ioctl, 0),
    ioctl_entry!(DRM_IOCTL_CRTC_QUEUE_SEQUENCE, drm_crtc_queue_sequence_ioctl, 0),
    ioctl_entry!(DRM_IOCTL_MODE_CREATE_LEASE, drm_mode_create_lease_ioctl, DRM_MASTER),
    ioctl_entry!(DRM_IOCTL_MODE_LIST_LESSEES, drm_mode_list_lessees_ioctl, DRM_MASTER),
    ioctl_entry!(DRM_IOCTL_MODE_GET_LEASE, drm_mode_get_lease_ioctl, DRM_MASTER),
    ioctl_entry!(DRM_IOCTL_MODE_REVOKE_LEASE, drm_mode_revoke_lease_ioctl, DRM_MASTER),
];

// upstream: drivers/gpu/drm/drm_ioctl.c drm_ioctl_kernel()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_ioctl_kernel(
    filp: *mut File,
    func: DrmIoctlFn,
    kdata: *mut c_void,
    flags: u32,
) -> i32 {
    if filp.is_null() {
        return -EINVAL;
    }
    unsafe {
        let file_priv = (*filp).private_data.cast::<DrmFile>();
        if file_priv.is_null() {
            return -EINVAL;
        }
        let dev = (*(*file_priv).minor).dev;
        if (*dev).unplugged {
            return -ENODEV;
        }
        let ret = drm_ioctl_permit(flags, file_priv);
        if ret != 0 {
            return ret;
        }
        func(dev, kdata, file_priv)
    }
}

// upstream: drivers/gpu/drm/drm_ioctl.c drm_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_ioctl(
    filp: *mut File,
    cmd: c_uint,
    arg: c_ulong,
) -> i32 {
    if filp.is_null() {
        return -EINVAL;
    }
    unsafe {
        let file_priv = (*filp).private_data.cast::<DrmFile>();
        if file_priv.is_null() {
            return -EINVAL;
        }
        let dev = (*(*file_priv).minor).dev;
        if (*dev).unplugged {
            return -ENODEV;
        }

        let nr = cmd & 0xff;
        let is_driver = nr >= 0x40 && nr < 0xa0;

        let desc: *const DrmIoctlDesc = if is_driver {
            let idx = (nr - 0x40) as usize;
            if let Some(driver) = (*dev).driver.as_ref() {
                if idx < driver.num_ioctls as usize && !driver.ioctls.is_null() {
                    driver.ioctls.add(idx)
                } else {
                    return -EINVAL;
                }
            } else {
                return -EINVAL;
            }
        } else {
            let mut found: *const DrmIoctlDesc = core::ptr::null();
            for item in DRM_IOCTLS.iter() {
                if (item.cmd & 0xff) == nr {
                    found = item;
                    break;
                }
            }
            if found.is_null() {
                return -EINVAL;
            }
            found
        };

        let func: DrmIoctlFn = core::mem::transmute((*desc).func);
        let drv_size = ((*desc).cmd >> 16) & 0x3fff;
        let user_size = (cmd >> 16) & 0x3fff;
        let ksize = drv_size.max(user_size) as usize;

        let mut stack_buf = [0u8; 128];
        let kdata = if ksize <= stack_buf.len() {
            stack_buf.as_mut_ptr()
        } else {
            kmalloc(ksize, GFP_KERNEL).cast::<u8>()
        };
        if kdata.is_null() {
            return -ENOMEM;
        }

        if user_size > 0 && arg != 0 {
            core::ptr::copy_nonoverlapping(arg as *const u8, kdata, user_size.min(drv_size) as usize);
            if ksize > (user_size as usize) {
                core::ptr::write_bytes(kdata.add(user_size as usize), 0, ksize - user_size as usize);
            }
        } else {
            core::ptr::write_bytes(kdata, 0, ksize);
        }

        let ret = drm_ioctl_kernel(filp, func, kdata.cast(), (*desc).flags);

        if ret == 0 && user_size > 0 && arg != 0 {
            core::ptr::copy_nonoverlapping(kdata, arg as *mut u8, user_size.min(drv_size) as usize);
        }

        if kdata != stack_buf.as_mut_ptr() {
            kfree(kdata);
        }

        ret
    }
}

// upstream: drivers/gpu/drm/drm_ioctl.c drm_ioctl_flags()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_ioctl_flags(nr: c_uint, flags: *mut c_uint) -> bool {
    if nr >= DRM_COMMAND_BASE as c_uint && nr < DRM_COMMAND_END as c_uint {
        return false;
    }
    if nr as usize >= DRM_IOCTLS.len() {
        return false;
    }
    if !flags.is_null() {
        unsafe { *flags = DRM_IOCTLS[nr as usize].flags };
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drm_drv_types::DrmDriver;
    use crate::drm_drv_upstream::{drm_dev_alloc, drm_dev_put};
    use crate::drm_file_upstream::{drm_file_alloc, drm_file_free};
    use crate::uapi_drm_consts::DRM_IOCTL_VERSION;
    use crate::uapi_drm_types::DrmVersion;

    #[test]
    fn test_ioctl_permit_flags() {
        unsafe {
            let mut driver: DrmDriver = core::mem::zeroed();
            let dev = drm_dev_alloc(&driver, core::ptr::null_mut());
            assert!(!dev.is_null());

            let primary_file = drm_file_alloc((*dev).primary);
            assert!(!primary_file.is_null());

            let render_file = drm_file_alloc((*dev).render);
            assert!(!render_file.is_null());

            // 1. DRM_ROOT_ONLY: without CAP_SYS_ADMIN provider, returns -EACCES
            assert_eq!(drm_ioctl_permit(DRM_ROOT_ONLY, primary_file), -EACCES);

            // 2. DRM_AUTH on primary file:
            (*primary_file).authenticated = false;
            assert_eq!(drm_ioctl_permit(DRM_AUTH, primary_file), -EACCES);
            (*primary_file).authenticated = true;
            assert_eq!(drm_ioctl_permit(DRM_AUTH, primary_file), 0);

            // 3. DRM_MASTER on primary file:
            // Since primary_file was first opened, it is master
            assert_eq!(drm_ioctl_permit(DRM_MASTER, primary_file), 0);
            (*primary_file).is_master = false;
            assert_eq!(drm_ioctl_permit(DRM_MASTER, primary_file), -EACCES);

            // 4. Render node permissions:
            // Without DRM_RENDER_ALLOW -> -EACCES
            assert_eq!(drm_ioctl_permit(0, render_file), -EACCES);
            // With DRM_RENDER_ALLOW -> 0
            assert_eq!(drm_ioctl_permit(DRM_RENDER_ALLOW, render_file), 0);
            // With DRM_RENDER_ALLOW | DRM_AUTH -> bypasses AUTH check on render node
            (*render_file).authenticated = false;
            assert_eq!(drm_ioctl_permit(DRM_RENDER_ALLOW | DRM_AUTH, render_file), 0);

            drm_file_free(render_file);
            drm_file_free(primary_file);
            drm_dev_put(dev);
        }
    }

    #[repr(C)]
    struct DriverTestData {
        a: u64,
        b: u64,
    }

    use core::sync::atomic::{AtomicU64, Ordering};
    static RECEIVED_A: AtomicU64 = AtomicU64::new(0);
    static RECEIVED_B: AtomicU64 = AtomicU64::new(0);

    unsafe extern "C" fn test_driver_ioctl(
        _dev: *mut DrmDevice,
        data: *mut c_void,
        _file_priv: *mut DrmFile,
    ) -> i32 {
        unsafe {
            let d = data.cast::<DriverTestData>();
            RECEIVED_A.store((*d).a, Ordering::SeqCst);
            RECEIVED_B.store((*d).b, Ordering::SeqCst);
            (*d).a = 0x1111;
            (*d).b = 0x2222;
            0
        }
    }

    #[test]
    fn test_ioctl_dispatch_sizing_and_unknown() {
        unsafe {
            let mut driver: DrmDriver = core::mem::zeroed();
            driver.major = 1;
            driver.minor = 2;
            driver.patchlevel = 3;
            driver.name = b"testdrv\0".as_ptr().cast_mut().cast();
            driver.desc = b"Test Driver\0".as_ptr().cast_mut().cast();

            // Setup custom driver ioctl at nr 0x40 (DRM_COMMAND_BASE)
            let drv_desc = DrmIoctlDesc {
                cmd: (size_of::<DriverTestData>() as u32) << 16 | 0x40,
                flags: DRM_RENDER_ALLOW,
                func: test_driver_ioctl as *mut c_void,
                name: core::ptr::null_mut(),
            };
            driver.ioctls = (&drv_desc as *const DrmIoctlDesc).cast_mut();
            driver.num_ioctls = 1;

            let dev = drm_dev_alloc(&driver, core::ptr::null_mut());
            let file_priv = drm_file_alloc((*dev).primary);

            let mut filp_struct: File = core::mem::zeroed();
            filp_struct.private_data = file_priv.cast();
            let filp = &mut filp_struct as *mut File;

            // 1. Unknown ioctl: nr 0xff (unknown core ioctl) -> -EINVAL
            assert_eq!(drm_ioctl(filp, 0x0000_00ff, 0), -EINVAL);

            // 2. Standard core ioctl: DRM_IOCTL_VERSION
            let mut version_data: DrmVersion = core::mem::zeroed();
            let ret = drm_ioctl(filp, DRM_IOCTL_VERSION, &mut version_data as *mut DrmVersion as usize as u64);
            assert_eq!(ret, 0);
            assert_eq!(version_data.version_major, 1);
            assert_eq!(version_data.version_minor, 2);
            assert_eq!(version_data.version_patchlevel, 3);

            // 3. Zero-extension rule: user passes 8 bytes (user_size = 8 < drv_size = 16)
            #[repr(C, align(8))]
            struct AlignedBuf<const N: usize>([u8; N]);

            let mut user_buf_8 = AlignedBuf([0xaa; 8]);
            let cmd_user_8 = (8u32 << 16) | 0x40;
            RECEIVED_A.store(0, Ordering::SeqCst);
            RECEIVED_B.store(0, Ordering::SeqCst);
            let ret = drm_ioctl(filp, cmd_user_8, user_buf_8.0.as_mut_ptr() as usize as u64);
            assert_eq!(ret, 0);
            assert_eq!(RECEIVED_A.load(Ordering::SeqCst), 0xaaaaaaaaaaaaaaaa);
            assert_eq!(RECEIVED_B.load(Ordering::SeqCst), 0); // zero-extended!

            // 4. Truncation rule: user passes 24 bytes (user_size = 24 > drv_size = 16)
            let mut user_buf_24 = AlignedBuf([0u8; 24]);
            let d_in = user_buf_24.0.as_mut_ptr().cast::<DriverTestData>();
            (*d_in).a = 0x5555;
            (*d_in).b = 0x6666;
            let cmd_user_24 = (24u32 << 16) | 0x40;
            let ret = drm_ioctl(filp, cmd_user_24, user_buf_24.0.as_mut_ptr() as usize as u64);
            assert_eq!(ret, 0);
            assert_eq!(RECEIVED_A.load(Ordering::SeqCst), 0x5555);
            assert_eq!(RECEIVED_B.load(Ordering::SeqCst), 0x6666);
            // Copied back 16 bytes:
            assert_eq!((*d_in).a, 0x1111);
            assert_eq!((*d_in).b, 0x2222);
            // Trailing 8 bytes untouched:
            assert_eq!(user_buf_24.0[16], 0);

            drm_file_free(file_priv);
            drm_dev_put(dev);
        }
    }
}
