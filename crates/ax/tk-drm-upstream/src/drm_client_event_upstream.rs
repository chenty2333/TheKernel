// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_client_event.c` (batch B2c).

#![allow(non_snake_case, non_upper_case_globals, dead_code, unsafe_code)]
#![allow(clippy::missing_safety_doc)]

use core::mem::offset_of;

use tk_linux_kapi::list::list_del;
#[cfg(not(test))]
use tk_linux_kapi::mutex::{mutex_lock, mutex_unlock};
#[cfg(test)]
unsafe fn mutex_lock<M>(_mutex: M) {}
#[cfg(test)]
unsafe fn mutex_unlock<M>(_mutex: M) {}

use crate::drm_client_types::{DrmClientDev, DrmClientFuncs};
use crate::drm_device_types::DrmDevice;
use crate::drm_mode_object_upstream::{drm_core_check_feature, DRIVER_MODESET};

#[inline]
unsafe fn client_from_node(node: *mut tk_linux_kapi::types::ListHead) -> *mut DrmClientDev {
    unsafe { node.cast::<u8>().sub(offset_of!(DrmClientDev, list)).cast() }
}

#[inline]
unsafe fn next_client(
    node: *mut tk_linux_kapi::types::ListHead,
    head: *mut tk_linux_kapi::types::ListHead,
) -> *mut DrmClientDev {
    if node.is_null() || core::ptr::eq(node, head) {
        return core::ptr::null_mut();
    }
    unsafe { client_from_node(node) }
}

unsafe fn drm_client_hotplug(client: *mut DrmClientDev) {
    if client.is_null() {
        return;
    }
    unsafe {
        let funcs: *mut DrmClientFuncs = (*client).funcs;
        if funcs.is_null() || (*funcs).hotplug.is_none() || (*client).hotplug_failed {
            return;
        }
        if (*client).suspended {
            (*client).hotplug_pending = true;
            return;
        }
        (*client).hotplug_pending = false;
        let ret = (*funcs).hotplug.unwrap()(client);
        tk_linux_kapi::print::drm_log_at(
            tk_linux_kapi::print::DrmLogLevel::Debug,
            "drm",
            file!(),
            line!(),
            &alloc::format!("client hotplug ret={ret}"),
        );
        if ret != 0 {
            (*client).hotplug_failed = true;
        }
    }
}

// upstream: drm_client_event.c drm_client_dev_unregister()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_client_dev_unregister(dev: *mut DrmDevice) {
    if dev.is_null() || !unsafe { drm_core_check_feature(dev, DRIVER_MODESET) } {
        return;
    }
    unsafe {
        let head = core::ptr::addr_of_mut!((*dev).clientlist);
        mutex_lock(core::ptr::addr_of_mut!((*dev).clientlist_mutex));
        let mut node = (*head).next;
        while !core::ptr::eq(node, head) {
            let next = (*node).next;
            let client = next_client(node, head);
            list_del(node);
            // The callback consumes and may free the client; use `next`, not
            // client state, after the call.
            if let Some(unregister) = (*client).funcs.as_ref().and_then(|f| f.unregister) {
                unregister(client);
            } else {
                crate::drm_client_upstream::drm_client_release(client);
            }
            node = next;
        }
        mutex_unlock(core::ptr::addr_of_mut!((*dev).clientlist_mutex));
    }
}

// upstream: drm_client_event.c drm_client_dev_hotplug()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_client_dev_hotplug(dev: *mut DrmDevice) {
    if dev.is_null() || !unsafe { drm_core_check_feature(dev, DRIVER_MODESET) } {
        return;
    }
    unsafe {
        if (*dev).mode_config.num_connector == 0 {
            return;
        }
        let head = core::ptr::addr_of_mut!((*dev).clientlist);
        mutex_lock(core::ptr::addr_of_mut!((*dev).clientlist_mutex));
        let mut node = (*head).next;
        while !core::ptr::eq(node, head) {
            let client = next_client(node, head);
            drm_client_hotplug(client);
            node = (*node).next;
        }
        mutex_unlock(core::ptr::addr_of_mut!((*dev).clientlist_mutex));
    }
}

// upstream: drm_client_event.c drm_client_dev_restore()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_client_dev_restore(dev: *mut DrmDevice, force: bool) {
    if dev.is_null() || !unsafe { drm_core_check_feature(dev, DRIVER_MODESET) } {
        return;
    }
    unsafe {
        let head = core::ptr::addr_of_mut!((*dev).clientlist);
        mutex_lock(core::ptr::addr_of_mut!((*dev).clientlist_mutex));
        let mut node = (*head).next;
        while !core::ptr::eq(node, head) {
            let client = next_client(node, head);
            if let Some(restore) = (*client).funcs.as_ref().and_then(|f| f.restore) {
                if restore(client, force) == 0 {
                    break;
                }
            }
            node = (*node).next;
        }
        mutex_unlock(core::ptr::addr_of_mut!((*dev).clientlist_mutex));
    }
}

unsafe fn drm_client_suspend(client: *mut DrmClientDev) -> i32 {
    unsafe {
        if (*client).suspended {
            return 0;
        }
        let ret = (*client)
            .funcs
            .as_ref()
            .and_then(|f| f.suspend)
            .map_or(0, |suspend| suspend(client));
        (*client).suspended = true;
        ret
    }
}

// upstream: drm_client_event.c drm_client_dev_suspend()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_client_dev_suspend(dev: *mut DrmDevice) {
    if dev.is_null() {
        return;
    }
    unsafe {
        let head = core::ptr::addr_of_mut!((*dev).clientlist);
        mutex_lock(core::ptr::addr_of_mut!((*dev).clientlist_mutex));
        let mut node = (*head).next;
        while !core::ptr::eq(node, head) {
            let client = next_client(node, head);
            if !(*client).suspended {
                let _ = drm_client_suspend(client);
            }
            node = (*node).next;
        }
        mutex_unlock(core::ptr::addr_of_mut!((*dev).clientlist_mutex));
    }
}

unsafe fn drm_client_resume(client: *mut DrmClientDev) -> i32 {
    unsafe {
        if !(*client).suspended {
            return 0;
        }
        let ret = (*client)
            .funcs
            .as_ref()
            .and_then(|f| f.resume)
            .map_or(0, |resume| resume(client));
        (*client).suspended = false;
        if (*client).hotplug_pending {
            drm_client_hotplug(client);
        }
        ret
    }
}

// upstream: drm_client_event.c drm_client_dev_resume()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_client_dev_resume(dev: *mut DrmDevice) {
    if dev.is_null() {
        return;
    }
    unsafe {
        let head = core::ptr::addr_of_mut!((*dev).clientlist);
        mutex_lock(core::ptr::addr_of_mut!((*dev).clientlist_mutex));
        let mut node = (*head).next;
        while !core::ptr::eq(node, head) {
            let client = next_client(node, head);
            if (*client).suspended {
                let _ = drm_client_resume(client);
            }
            node = (*node).next;
        }
        mutex_unlock(core::ptr::addr_of_mut!((*dev).clientlist_mutex));
    }
}
