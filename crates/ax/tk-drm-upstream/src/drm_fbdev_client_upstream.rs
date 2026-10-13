// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/clients/drm_fbdev_client.c` (batch B2c).

#![allow(non_snake_case, non_upper_case_globals, dead_code, unsafe_code)]
#![allow(clippy::missing_safety_doc)]

use core::ffi::c_void;

use tk_linux_kapi::config::{EINVAL, ENOMEM};
use tk_linux_kapi::memory::{kfree, kzalloc_obj};
use tk_linux_kapi::print::{drm_log_at, DrmLogLevel};

use crate::drm_client_types::{DrmClientDev, DrmClientFuncs};
use crate::drm_device_types::DrmDevice;
use crate::drm_fb_helper_types::DrmFbHelper;
use crate::drm_fourcc_types::DrmFormatInfo;

#[inline]
unsafe fn drm_fb_helper_from_client(client: *mut DrmClientDev) -> *mut DrmFbHelper {
    client.cast()
}

unsafe extern "C" fn drm_fbdev_client_free(client: *mut DrmClientDev) {
    if client.is_null() {
        return;
    }
    unsafe {
        let helper = drm_fb_helper_from_client(client);
        crate::drm_fb_helper_upstream::drm_fb_helper_unprepare(helper);
        kfree(helper);
    }
}

unsafe extern "C" fn drm_fbdev_client_unregister(client: *mut DrmClientDev) {
    if client.is_null() {
        return;
    }
    unsafe {
        let helper = drm_fb_helper_from_client(client);
        if !(*helper).info.is_null() {
            crate::drm_fb_helper_upstream::drm_fb_helper_unregister_info(helper);
        } else {
            // Partial setup has no fb_info and therefore no unregister path;
            // release the client and every resource allocated during init.
            crate::drm_client_upstream::drm_client_release(client);
        }
    }
}

unsafe extern "C" fn drm_fbdev_client_restore(client: *mut DrmClientDev, force: bool) -> i32 {
    if client.is_null() {
        return -EINVAL;
    }
    unsafe {
        crate::drm_fb_helper_upstream::drm_fb_helper_restore_fbdev_mode_unlocked(
            drm_fb_helper_from_client(client),
            force,
        );
    }
    0
}

unsafe extern "C" fn drm_fbdev_client_hotplug(client: *mut DrmClientDev) -> i32 {
    if client.is_null() || unsafe { (*client).dev.is_null() } {
        return -EINVAL;
    }
    unsafe {
        let helper = drm_fb_helper_from_client(client);
        let dev = (*client).dev;
        if !(*dev).fb_helper.is_null() {
            return crate::drm_fb_helper_upstream::drm_fb_helper_hotplug_event((*dev).fb_helper);
        }

        let mut ret = crate::drm_fb_helper_upstream::drm_fb_helper_init(dev, helper);
        if ret != 0 {
            return fbdev_setup_error(dev, ret);
        }
        if !crate::drm_color_mgmt_upstream::drm_drv_uses_atomic_modeset(dev) {
            crate::drm_client_modeset_upstream::drm_helper_disable_unused_functions(dev);
        }
        ret = crate::drm_fb_helper_upstream::drm_fb_helper_initial_config(helper);
        if ret != 0 {
            crate::drm_fb_helper_upstream::drm_fb_helper_fini(helper);
            return fbdev_setup_error(dev, ret);
        }
        0
    }
}

unsafe fn fbdev_setup_error(dev: *mut DrmDevice, ret: i32) -> i32 {
    unsafe {
        drm_log_at(
            DrmLogLevel::Error,
            "drm",
            file!(),
            line!(),
            &alloc::format!("fbdev: Failed to setup emulation (ret={ret})"),
        );
        let _ = dev;
    }
    ret
}

unsafe extern "C" fn drm_fbdev_client_suspend(client: *mut DrmClientDev) -> i32 {
    if client.is_null() {
        return -EINVAL;
    }
    unsafe {
        crate::drm_fb_helper_upstream::drm_fb_helper_set_suspend_unlocked(
            drm_fb_helper_from_client(client),
            true,
        );
    }
    0
}

unsafe extern "C" fn drm_fbdev_client_resume(client: *mut DrmClientDev) -> i32 {
    if client.is_null() {
        return -EINVAL;
    }
    unsafe {
        crate::drm_fb_helper_upstream::drm_fb_helper_set_suspend_unlocked(
            drm_fb_helper_from_client(client),
            false,
        );
    }
    0
}

static mut DRM_FBDEV_CLIENT_FUNCS: DrmClientFuncs = DrmClientFuncs {
    owner: core::ptr::null_mut::<c_void>(),
    free: Some(drm_fbdev_client_free),
    unregister: Some(drm_fbdev_client_unregister),
    restore: Some(drm_fbdev_client_restore),
    hotplug: Some(drm_fbdev_client_hotplug),
    suspend: Some(drm_fbdev_client_suspend),
    resume: Some(drm_fbdev_client_resume),
};

// upstream: clients/drm_fbdev_client.c drm_fbdev_client_setup()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_fbdev_client_setup(
    dev: *mut DrmDevice,
    format: *const DrmFormatInfo,
) -> i32 {
    if dev.is_null() {
        return -EINVAL;
    }

    let color_mode = unsafe {
        if !format.is_null() {
            let bpp = crate::drm_fourcc_upstream::drm_format_info_bpp(format, 0);
            if bpp == 16 {
                (*format).depth as u32
            } else {
                bpp
            }
        } else {
            match (*dev).mode_config.preferred_depth {
                0 | 24 => 32,
                depth => depth,
            }
        }
    };

    unsafe {
        if !(*dev).registered || !(*dev).fb_helper.is_null() {
            drm_log_at(
                DrmLogLevel::Warn,
                "drm",
                file!(),
                line!(),
                "fbdev setup called before registration or more than once",
            );
        }
    }
    let helper = kzalloc_obj::<DrmFbHelper>();
    if helper.is_null() {
        return -ENOMEM;
    }

    unsafe {
        crate::drm_fb_helper_upstream::drm_fb_helper_prepare(
            dev,
            helper,
            color_mode,
            core::ptr::null_mut(),
        );
        let ret = crate::drm_client_upstream::drm_client_init(
            dev,
            core::ptr::addr_of_mut!((*helper).client),
            c"fbdev".as_ptr(),
            core::ptr::addr_of_mut!(DRM_FBDEV_CLIENT_FUNCS),
        );
        if ret != 0 {
            crate::drm_fb_helper_upstream::drm_fb_helper_unprepare(helper);
            kfree(helper);
            drm_log_at(
                DrmLogLevel::Error,
                "drm",
                file!(),
                line!(),
                &alloc::format!("Failed to register client: {ret}"),
            );
            return ret;
        }
        crate::drm_client_upstream::drm_client_register(core::ptr::addr_of_mut!((*helper).client));
    }
    0
}
