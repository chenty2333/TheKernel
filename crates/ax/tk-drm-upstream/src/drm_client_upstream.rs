// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_client.c` (batch B2c).

#![allow(non_snake_case, non_upper_case_globals, dead_code, unsafe_code)]
#![allow(clippy::missing_safety_doc)]

use core::ffi::c_char;

use tk_linux_kapi::config::{EINVAL, ENOENT, ENOMEM, ERR_PTR};
use tk_linux_kapi::list::{list_add_tail, list_del};
use tk_linux_kapi::memory::{kfree, kzalloc_obj};
#[cfg(not(test))]
use tk_linux_kapi::mutex::{mutex_lock, mutex_unlock};
#[cfg(test)]
unsafe fn mutex_lock<M>(_mutex: M) {}
#[cfg(test)]
unsafe fn mutex_unlock<M>(_mutex: M) {}

use crate::drm_client_types::{DrmClientBuffer, DrmClientDev};
use crate::drm_device_types::DrmDevice;
use crate::drm_file_types::DrmMinor;
use crate::drm_framebuffer_types::DrmFramebufferFuncs;
use crate::drm_gem_upstream::{
    drm_gem_object_lookup, drm_gem_object_put, drm_gem_vmap, drm_gem_vunmap,
    drm_gem_vmap_locked, drm_gem_vunmap_locked, drm_gem_lock, drm_gem_unlock,
};
use crate::drm_rect_types::DrmRect;
use crate::iosys_map_types::IosysMap;
use crate::uapi_drm_mode_types::DrmModeFbCmd2;

// drm_client.c: client_open()
unsafe fn drm_client_open(client: *mut DrmClientDev) -> i32 {
    let dev = unsafe { (*client).dev };
    let minor: *mut DrmMinor = unsafe { (*dev).primary };
    if minor.is_null() {
        return -EINVAL;
    }
    let file = unsafe { crate::drm_file_upstream::drm_file_alloc(minor) };
    if file.is_null() {
        return -ENOMEM;
    }
    unsafe {
        mutex_lock(core::ptr::addr_of_mut!((*dev).filelist_mutex));
        list_add_tail(
            core::ptr::addr_of_mut!((*file).lhead),
            core::ptr::addr_of_mut!((*dev).filelist_internal),
        );
        mutex_unlock(core::ptr::addr_of_mut!((*dev).filelist_mutex));
        (*client).file = file;
    }
    0
}

// drm_client.c: client_close()
unsafe fn drm_client_close(client: *mut DrmClientDev) {
    let dev = unsafe { (*client).dev };
    let file = unsafe { (*client).file };
    if file.is_null() {
        return;
    }
    unsafe {
        mutex_lock(core::ptr::addr_of_mut!((*dev).filelist_mutex));
        list_del(core::ptr::addr_of_mut!((*file).lhead));
        mutex_unlock(core::ptr::addr_of_mut!((*dev).filelist_mutex));
        crate::drm_file_upstream::drm_file_free(file);
        (*client).file = core::ptr::null_mut();
    }
}

// upstream: drm_client.c drm_client_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_client_init(
    dev: *mut DrmDevice,
    client: *mut DrmClientDev,
    name: *const c_char,
    funcs: *const crate::drm_client_types::DrmClientFuncs,
) -> i32 {
    if dev.is_null() || client.is_null() || name.is_null() {
        return -EINVAL;
    }
    unsafe {
        if !crate::drm_mode_object_upstream::drm_core_check_feature(
            dev,
            crate::drm_mode_object_upstream::DRIVER_MODESET,
        ) || (*dev).driver.is_null()
            || (*(*dev).driver).dumb_create.is_none()
        {
            return -tk_linux_kapi::config::EOPNOTSUPP;
        }
        (*client).dev = dev;
        (*client).name = name.cast_mut();
        (*client).funcs = funcs.cast_mut();

        let ret = crate::drm_client_modeset_upstream::drm_client_modeset_create(client);
        if ret != 0 {
            return ret;
        }
        let ret = drm_client_open(client);
        if ret != 0 {
            crate::drm_client_modeset_upstream::drm_client_modeset_free(client);
            return ret;
        }
        crate::drm_drv_upstream::drm_dev_get(dev);
        0
    }
}

// upstream: drm_client.c drm_client_register()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_client_register(client: *mut DrmClientDev) {
    if client.is_null() || unsafe { (*client).dev.is_null() } {
        return;
    }
    let dev = unsafe { (*client).dev };
    unsafe {
        mutex_lock(core::ptr::addr_of_mut!((*dev).clientlist_mutex));
        list_add_tail(
            core::ptr::addr_of_mut!((*client).list),
            core::ptr::addr_of_mut!((*dev).clientlist),
        );
        // Linux adds the client before the synchronous initial hotplug. This
        // ordering prevents a concurrent event from being lost.
        if let Some(hotplug) = (*client).funcs.as_ref().and_then(|f| f.hotplug) {
            let ret = hotplug(client);
            if ret != 0 {
                tk_linux_kapi::print::drm_log_at(
                    tk_linux_kapi::print::DrmLogLevel::Debug,
                    "drm",
                    file!(),
                    line!(),
                    &alloc::format!("client hotplug ret={ret}"),
                );
            }
        }
        mutex_unlock(core::ptr::addr_of_mut!((*dev).clientlist_mutex));
    }
}

// upstream: drm_client.c drm_client_release()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_client_release(client: *mut DrmClientDev) {
    if client.is_null() {
        return;
    }
    unsafe {
        let dev = (*client).dev;
        if dev.is_null() {
            return;
        }
        crate::drm_client_modeset_upstream::drm_client_modeset_free(client);
        drm_client_close(client);
        if let Some(free) = (*client).funcs.as_ref().and_then(|f| f.free) {
            free(client);
        }
        crate::drm_drv_upstream::drm_dev_put(dev);
    }
}

// upstream: drm_client.c drm_client_buffer_delete()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_client_buffer_delete(buffer: *mut DrmClientBuffer) {
    if buffer.is_null() {
        return;
    }
    unsafe {
        let fb = (*buffer).fb;
        let client = (*buffer).client;
        let gem = if fb.is_null() { (*buffer).gem } else { (*fb).obj[0] };
        if !gem.is_null() {
            drm_gem_vunmap(gem, core::ptr::addr_of_mut!((*buffer).map).cast());
        }
        if !fb.is_null() && !client.is_null() {
            let ret = crate::drm_framebuffer_upstream::drm_mode_rmfb(
                (*client).dev,
                (*fb).base.id,
                (*client).file,
            );
            if ret != 0 {
                tk_linux_kapi::print::drm_log_at(
                    tk_linux_kapi::print::DrmLogLevel::Error,
                    "drm",
                    file!(),
                    line!(),
                    &alloc::format!("Error removing FB:{} ({ret})", (*fb).base.id),
                );
            }
        }
        if !(*buffer).gem.is_null() {
            drm_gem_object_put((*buffer).gem);
        }
        kfree(buffer);
    }
}

// upstream: drm_client.c drm_client_buffer_create()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_client_buffer_create(
    client: *mut DrmClientDev,
    width: u32,
    height: u32,
    format: u32,
    handle: u32,
    pitch: u32,
) -> *mut DrmClientBuffer {
    if client.is_null() || unsafe { (*client).dev.is_null() || (*client).file.is_null() } {
        return ERR_PTR::<DrmClientBuffer>(-EINVAL);
    }
    let buffer = kzalloc_obj::<DrmClientBuffer>();
    if buffer.is_null() {
        return ERR_PTR::<DrmClientBuffer>(-ENOMEM);
    }
    unsafe { (*buffer).client = client };

    let obj = unsafe { drm_gem_object_lookup((*client).file, handle) };
    if obj.is_null() {
        unsafe { kfree(buffer) };
        return ERR_PTR::<DrmClientBuffer>(-ENOENT);
    }

    let mut request = DrmModeFbCmd2 {
        fb_id: 0,
        width,
        height,
        pixel_format: format,
        flags: 0,
        handles: [handle, 0, 0, 0],
        pitches: [pitch, 0, 0, 0],
        offsets: [0; 4],
        modifier: [0; 4],
    };
    let dev = unsafe { (*client).dev };
    let file = unsafe { (*client).file };
    let ret = unsafe {
        crate::drm_framebuffer_upstream::drm_mode_addfb2(
            dev,
            (&mut request as *mut DrmModeFbCmd2).cast(),
            file,
        )
    };
    if ret != 0 {
        unsafe {
            drm_gem_object_put(obj);
            kfree(buffer);
        }
        return ERR_PTR::<DrmClientBuffer>(ret);
    }

    let fb = unsafe { crate::drm_framebuffer_upstream::drm_framebuffer_lookup(dev, file, request.fb_id) };
    if fb.is_null() {
        unsafe {
            crate::drm_framebuffer_upstream::drm_mode_rmfb(dev, request.fb_id, file);
            drm_gem_object_put(obj);
            kfree(buffer);
        }
        return ERR_PTR::<DrmClientBuffer>(-ENOENT);
    }
    unsafe {
        crate::drm_framebuffer_upstream::drm_framebuffer_put(fb);
        copy_comm((*fb).comm.as_mut_ptr(), (*client).name);
        (*buffer).gem = obj;
        (*buffer).fb = fb;
    }
    buffer
}

unsafe fn copy_comm(dst: *mut c_char, src: *const c_char) {
    if dst.is_null() || src.is_null() {
        return;
    }
    unsafe {
        for i in 0..16 {
            let ch = *src.add(i);
            *dst.add(i) = ch;
            if ch == 0 {
                break;
            }
        }
        *dst.add(15) = 0;
    }
}

// upstream: drm_client.c drm_client_buffer_vmap_local()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_client_buffer_vmap_local(
    buffer: *mut DrmClientBuffer,
    map_copy: *mut IosysMap,
) -> i32 {
    if buffer.is_null() || map_copy.is_null() || unsafe { (*buffer).fb.is_null() } {
        return -EINVAL;
    }
    unsafe {
        let gem = (*(*buffer).fb).obj[0];
        drm_gem_lock(gem);
        let ret = drm_gem_vmap_locked(gem, core::ptr::addr_of_mut!((*buffer).map).cast());
        if ret != 0 {
            drm_gem_unlock(gem);
            return ret;
        }
        core::ptr::copy_nonoverlapping(core::ptr::addr_of!((*buffer).map), map_copy, 1);
    }
    0
}

// upstream: drm_client.c drm_client_buffer_vunmap_local()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_client_buffer_vunmap_local(buffer: *mut DrmClientBuffer) {
    if buffer.is_null() || unsafe { (*buffer).fb.is_null() } {
        return;
    }
    unsafe {
        let gem = (*(*buffer).fb).obj[0];
        drm_gem_vunmap_locked(gem, core::ptr::addr_of_mut!((*buffer).map).cast());
        drm_gem_unlock(gem);
    }
}

// upstream: drm_client.c drm_client_buffer_vmap()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_client_buffer_vmap(
    buffer: *mut DrmClientBuffer,
    map_copy: *mut IosysMap,
) -> i32 {
    if buffer.is_null() || map_copy.is_null() || unsafe { (*buffer).fb.is_null() } {
        return -EINVAL;
    }
    unsafe {
        let ret = drm_gem_vmap(
            (*(*buffer).fb).obj[0],
            core::ptr::addr_of_mut!((*buffer).map).cast(),
        );
        if ret != 0 {
            return ret;
        }
        core::ptr::copy_nonoverlapping(core::ptr::addr_of!((*buffer).map), map_copy, 1);
    }
    0
}

// upstream: drm_client.c drm_client_buffer_vunmap()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_client_buffer_vunmap(buffer: *mut DrmClientBuffer) {
    if buffer.is_null() || unsafe { (*buffer).fb.is_null() } {
        return;
    }
    unsafe {
        drm_gem_vunmap(
            (*(*buffer).fb).obj[0],
            core::ptr::addr_of_mut!((*buffer).map).cast(),
        );
    }
}

// upstream: drm_client.c drm_client_buffer_create_dumb()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_client_buffer_create_dumb(
    client: *mut DrmClientDev,
    width: u32,
    height: u32,
    format: u32,
) -> *mut DrmClientBuffer {
    if client.is_null() || unsafe { (*client).dev.is_null() } {
        return ERR_PTR::<DrmClientBuffer>(-EINVAL);
    }
    let info = unsafe { crate::drm_fourcc_upstream::drm_format_info(format) };
    if info.is_null() {
        return ERR_PTR::<DrmClientBuffer>(-EINVAL);
    }
    let mut args = crate::uapi_drm_mode_types::DrmModeCreateDumb {
        height,
        width,
        bpp: unsafe { crate::drm_fourcc_upstream::drm_format_info_bpp(info, 0) },
        flags: 0,
        handle: 0,
        pitch: 0,
        size: 0,
    };
    let dev = unsafe { (*client).dev };
    let file = unsafe { (*client).file };
    let ret = unsafe {
        crate::drm_dumb_buffers_upstream::drm_mode_create_dumb(
            dev,
            (&mut args as *mut crate::uapi_drm_mode_types::DrmModeCreateDumb).cast(),
            file,
        )
    };
    if ret != 0 {
        return ERR_PTR::<DrmClientBuffer>(ret);
    }
    let buffer = unsafe { drm_client_buffer_create(client, width, height, format, args.handle, args.pitch) };
    if tk_linux_kapi::config::IS_ERR(buffer) {
        unsafe {
            crate::drm_dumb_buffers_upstream::drm_mode_destroy_dumb(dev, args.handle, file);
        }
        return buffer;
    }
    unsafe { crate::drm_dumb_buffers_upstream::drm_mode_destroy_dumb(dev, args.handle, file) };
    buffer
}

// upstream: drm_client.c drm_client_buffer_flush()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_client_buffer_flush(
    buffer: *mut DrmClientBuffer,
    rect: *mut DrmRect,
) -> i32 {
    if buffer.is_null() || unsafe { (*buffer).fb.is_null() || (*(*buffer).fb).funcs.is_null() } {
        return 0;
    }
    unsafe {
        let funcs: *mut DrmFramebufferFuncs = (*(*buffer).fb).funcs;
        let Some(dirty) = (*funcs).dirty else { return 0 };
        if rect.is_null() {
            return dirty((*buffer).fb, (*(*buffer).client).file, 0, 0, core::ptr::null_mut(), 0);
        }
        let clip = crate::uapi_drm_types::DrmClipRect {
            x1: (*rect).x1 as u16,
            y1: (*rect).y1 as u16,
            x2: (*rect).x2 as u16,
            y2: (*rect).y2 as u16,
        };
        dirty(
            (*buffer).fb,
            (*(*buffer).client).file,
            0,
            0,
            (&clip as *const crate::uapi_drm_types::DrmClipRect).cast_mut(),
            1,
        )
    }
}
