// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors.
//! Linux v7.2.3 VirtIO GPU driver ioctls.
//!
//! User pointers are copied into owned buffers before queue submission. The
//! lower display transport retains command/DMA storage through terminal
//! completion; this layer retains referenced GEM objects for the same period.

use alloc::{string::ToString, sync::Arc, vec::Vec};
use core::{
    ffi::{c_int, c_void},
    mem::size_of,
    ptr,
};

use axdriver_display::{
    BlobMem, BlobResource, ContextInit, DevError, GpuBatch, GpuCompletionData, GpuFeatures,
    GpuQueue, RenderResource3D, RenderTransfer3D,
};
use tk_drm_upstream::{
    drm_file_types::DrmFile,
    drm_gem_shmem_helper_upstream::drm_gem_shmem_pin,
    drm_gem_types::DrmGemObject,
    drm_gem_upstream::{
        drm_gem_dumb_map_offset, drm_gem_handle_delete, drm_gem_object_lookup, drm_gem_object_put,
    },
    drm_ioctl_types::{DRM_RENDER_ALLOW, DrmIoctlDesc},
    drm_syncobj_upstream::{
        DrmSyncobj, drm_syncobj_add_point, drm_syncobj_chain_alloc, drm_syncobj_chain_free,
        drm_syncobj_fence_create, drm_syncobj_fence_put, drm_syncobj_fence_signal,
        drm_syncobj_find, drm_syncobj_find_fence, drm_syncobj_put, drm_syncobj_replace_fence,
        drm_syncobj_sync_file_create, drm_syncobj_sync_file_file, drm_syncobj_sync_file_fput,
        drm_syncobj_sync_file_get_fence,
    },
};
use tk_linux_kapi::{
    config::{EFAULT, EINVAL, EIO, ENODEV, ENOENT, ENOMEM, EOPNOTSUPP, GFP_KERNEL, PAGE_SIZE},
    memory::{kfree, kmalloc},
    mm_native::{copy_from_user, copy_to_user},
    types::DmaFence,
};

use crate::{
    device::{DriverPrivate, private_arc_from_device, with_device_engine, with_device_state},
    gem::{VirtioGpuObject, alloc_shmem_handle, create_classic_resource, physical_page_entries},
    transport::FenceStatus,
    uapi::*,
};

const WAIT_NS: u64 = 5_000_000_000;
const MAX_COMMAND: usize = 16 * 1024 * 1024;
const MAX_HANDLES: usize = 4096;
const BLOB_USE_MASK: u32 = VIRTGPU_BLOB_FLAG_USE_MAPPABLE
    | VIRTGPU_BLOB_FLAG_USE_SHAREABLE
    | VIRTGPU_BLOB_FLAG_USE_CROSS_DEVICE;

#[repr(C)]
struct FilePrivate {
    context_id: u32,
    context_created: bool,
    init: ContextInit,
    initialized: bool,
}

pub(crate) static VIRTGPU_IOCTLS: [DrmIoctlDesc; 11] = [
    desc(DRM_IOCTL_VIRTGPU_MAP, ioctl_map as *const ()),
    desc(DRM_IOCTL_VIRTGPU_EXECBUFFER, ioctl_execbuffer as *const ()),
    desc(DRM_IOCTL_VIRTGPU_GETPARAM, ioctl_getparam as *const ()),
    desc(
        DRM_IOCTL_VIRTGPU_RESOURCE_CREATE,
        ioctl_resource_create as *const (),
    ),
    desc(
        DRM_IOCTL_VIRTGPU_RESOURCE_INFO,
        ioctl_resource_info as *const (),
    ),
    desc(
        DRM_IOCTL_VIRTGPU_TRANSFER_FROM_HOST,
        ioctl_transfer_from_host as *const (),
    ),
    desc(
        DRM_IOCTL_VIRTGPU_TRANSFER_TO_HOST,
        ioctl_transfer_to_host as *const (),
    ),
    desc(DRM_IOCTL_VIRTGPU_WAIT, ioctl_wait as *const ()),
    desc(DRM_IOCTL_VIRTGPU_GET_CAPS, ioctl_get_caps as *const ()),
    desc(
        DRM_IOCTL_VIRTGPU_RESOURCE_CREATE_BLOB,
        ioctl_blob_create as *const (),
    ),
    desc(
        DRM_IOCTL_VIRTGPU_CONTEXT_INIT,
        ioctl_context_init as *const (),
    ),
];

const fn desc(cmd: u32, func: *const ()) -> DrmIoctlDesc {
    DrmIoctlDesc {
        cmd,
        flags: DRM_RENDER_ALLOW,
        func: func as *mut c_void,
        name: ptr::null_mut(),
    }
}

pub(crate) unsafe extern "C" fn virtgpu_open(
    _dev: *mut tk_drm_upstream::drm_device_types::DrmDevice,
    file: *mut DrmFile,
) -> i32 {
    if file.is_null() {
        return -EINVAL;
    }
    let private = unsafe { kmalloc(size_of::<FilePrivate>(), GFP_KERNEL) }.cast::<FilePrivate>();
    if private.is_null() {
        return -ENOMEM;
    }
    unsafe {
        ptr::write(
            private,
            FilePrivate {
                context_id: 0,
                context_created: false,
                init: ContextInit::default(),
                initialized: false,
            },
        );
        (*file).driver_priv = private.cast();
    }
    0
}

pub(crate) unsafe extern "C" fn virtgpu_postclose(
    dev: *mut tk_drm_upstream::drm_device_types::DrmDevice,
    file: *mut DrmFile,
) {
    if file.is_null() {
        return;
    }
    let private = unsafe { (*file).driver_priv.cast::<FilePrivate>() };
    if private.is_null() {
        return;
    }
    if unsafe { (*private).context_created } {
        let id = unsafe { (*private).context_id };
        let _ = unsafe {
            with_device_engine(dev, |engine, display| {
                if let Ok(fence) = engine.submit(
                    display,
                    GpuQueue::Control,
                    GpuBatch::DestroyContext { context: id },
                ) {
                    let _ = engine.wait(display, fence, WAIT_NS);
                    let _ = engine.reap(fence);
                }
            })
        };
    }
    unsafe { kfree(private.cast::<c_void>()) };
    unsafe { (*file).driver_priv = ptr::null_mut() };
}

fn file_private(file: *mut DrmFile) -> Result<*mut FilePrivate, i32> {
    if file.is_null() {
        return Err(-EINVAL);
    }
    let private = unsafe { (*file).driver_priv.cast::<FilePrivate>() };
    if private.is_null() {
        Err(-ENODEV)
    } else {
        Ok(private)
    }
}

pub(crate) unsafe extern "C" fn virtgpu_gem_open(
    object: *mut DrmGemObject,
    file: *mut DrmFile,
) -> i32 {
    if object.is_null() || file.is_null() {
        return -EINVAL;
    }
    let dev = unsafe { (*object).dev };
    let fpriv = match file_private(file) {
        Ok(private) => private,
        Err(error) => return error,
    };
    let Some(features) = (unsafe { with_device_engine(dev, |engine, _| engine.modern_features()) })
    else {
        return -ENODEV;
    };
    if features.contains(GpuFeatures::VIRGL_3D)
        && !features.contains(GpuFeatures::CONTEXT_INIT)
        && !unsafe { (*fpriv).context_created }
    {
        let error = ensure_context(dev, fpriv);
        if error != 0 {
            return error;
        }
    }
    context_resource_for_gem(dev, fpriv, object, true)
}

pub(crate) unsafe extern "C" fn virtgpu_gem_close(object: *mut DrmGemObject, file: *mut DrmFile) {
    if object.is_null() || file.is_null() {
        return;
    }
    let dev = unsafe { (*object).dev };
    let fpriv = match file_private(file) {
        Ok(private) => private,
        Err(_) => return,
    };
    let _ = context_resource_for_gem(dev, fpriv, object, false);
}

fn context_resource_for_gem(
    dev: *mut tk_drm_upstream::drm_device_types::DrmDevice,
    fpriv: *mut FilePrivate,
    object: *mut DrmGemObject,
    attach: bool,
) -> i32 {
    let Some(result) = (unsafe {
        with_device_state(dev, |state| {
            if !(*fpriv).context_created {
                return Ok(());
            }
            let resource = (*(object.cast::<VirtioGpuObject>())).resource_id;
            if resource == 0 {
                return Ok(());
            }
            let batch = if attach {
                GpuBatch::AttachResource {
                    context: (*fpriv).context_id,
                    resource,
                }
            } else {
                GpuBatch::DetachResource {
                    context: (*fpriv).context_id,
                    resource,
                }
            };
            let fence = state
                .engine
                .submit(&mut *state.display, GpuQueue::Control, batch)
                .map_err(dev_errno)?;
            state
                .engine
                .wait(&mut *state.display, fence, WAIT_NS)
                .map_err(dev_errno)?;
            let _ = state.engine.reap(fence);
            Ok(())
        })
    }) else {
        return -ENODEV;
    };
    result.err().unwrap_or(0)
}

fn context_resource_ioctl(
    dev: *mut tk_drm_upstream::drm_device_types::DrmDevice,
    context: u32,
    resource: u32,
    attach: bool,
) -> i32 {
    let Some(result) = (unsafe {
        with_device_state(dev, |state| {
            let batch = if attach {
                GpuBatch::AttachResource { context, resource }
            } else {
                GpuBatch::DetachResource { context, resource }
            };
            let fence = state
                .engine
                .submit(&mut *state.display, GpuQueue::Control, batch)
                .map_err(dev_errno)?;
            state
                .engine
                .wait(&mut *state.display, fence, WAIT_NS)
                .map_err(dev_errno)?;
            let _ = state.engine.reap(fence);
            Ok(())
        })
    }) else {
        return -ENODEV;
    };
    result.err().unwrap_or(0)
}

unsafe extern "C" fn ioctl_map(
    dev: *mut tk_drm_upstream::drm_device_types::DrmDevice,
    data: *mut c_void,
    file: *mut DrmFile,
) -> i32 {
    if data.is_null() || file.is_null() || dev.is_null() {
        return -EINVAL;
    }
    let arg = data.cast::<DrmVirtgpuMap>();
    unsafe { drm_gem_dumb_map_offset(file, dev, (*arg).handle, ptr::addr_of_mut!((*arg).offset)) }
}

unsafe extern "C" fn ioctl_getparam(
    dev: *mut tk_drm_upstream::drm_device_types::DrmDevice,
    data: *mut c_void,
    _file: *mut DrmFile,
) -> i32 {
    if data.is_null() || dev.is_null() {
        return -EINVAL;
    }
    let arg = data.cast::<DrmVirtgpuGetparam>();
    let result = unsafe {
        with_device_engine(dev, |engine, _| {
            let features = engine.modern_features();
            Ok(match (*arg).param {
                VIRTGPU_PARAM_3D_FEATURES => {
                    Some(i32::from(features.contains(GpuFeatures::VIRGL_3D)))
                }
                VIRTGPU_PARAM_CAPSET_QUERY_FIX => Some(1),
                VIRTGPU_PARAM_RESOURCE_BLOB => {
                    Some(i32::from(features.contains(GpuFeatures::RESOURCE_BLOB)))
                }
                VIRTGPU_PARAM_HOST_VISIBLE => {
                    Some(i32::from(features.contains(GpuFeatures::HOST_VISIBLE)))
                }
                VIRTGPU_PARAM_CROSS_DEVICE => {
                    Some(i32::from(features.contains(GpuFeatures::RESOURCE_UUID)))
                }
                VIRTGPU_PARAM_CONTEXT_INIT => {
                    Some(i32::from(features.contains(GpuFeatures::CONTEXT_INIT)))
                }
                VIRTGPU_PARAM_SUPPORTED_CAPSET_IDS => Some(engine.capset_mask() as i32),
                VIRTGPU_PARAM_EXPLICIT_DEBUG_NAME => {
                    Some(i32::from(features.contains(GpuFeatures::CONTEXT_INIT)))
                }
                VIRTGPU_PARAM_BLOB_ALIGNMENT => None,
                _ => return Err(-EINVAL),
            })
        })
    };
    let Some(result) = result else {
        return -ENODEV;
    };
    match result {
        Err(e) => e,
        Ok(None) => -ENOENT,
        Ok(Some(value)) => {
            let address = unsafe { (*arg).value } as usize as *mut c_void;
            if address.is_null()
                || unsafe { copy_to_user(address, ptr::addr_of!(value).cast(), size_of::<i32>()) }
                    != 0
            {
                -EFAULT
            } else {
                0
            }
        }
    }
}

unsafe extern "C" fn ioctl_resource_create(
    dev: *mut tk_drm_upstream::drm_device_types::DrmDevice,
    data: *mut c_void,
    file: *mut DrmFile,
) -> i32 {
    if data.is_null() || file.is_null() || dev.is_null() {
        return -EINVAL;
    }
    let arg = unsafe { &mut *data.cast::<DrmVirtgpuResourceCreate>() };
    let size = if arg.size == 0 {
        PAGE_SIZE
    } else {
        arg.size as usize
    };
    if size > u32::MAX as usize
        || arg.width == 0
        || arg.height == 0
        || arg.width > 8192
        || arg.height > 8192
    {
        return -EINVAL;
    }
    let Some(virgl) = (unsafe {
        with_device_engine(dev, |engine, _| {
            engine.modern_features().contains(GpuFeatures::VIRGL_3D)
        })
    }) else {
        return -ENODEV;
    };
    if !virgl
        && (arg.target != 2
            || arg.depth > 1
            || arg.array_size > 1
            || arg.last_level > 1
            || arg.nr_samples > 1)
    {
        return -EINVAL;
    }
    let min_2d_size = if virgl {
        None
    } else {
        let Some(minimum) = (arg.width as usize)
            .checked_mul(arg.height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
        else {
            return -EINVAL;
        };
        if arg.bo_handle == 0 && size < minimum {
            return -EINVAL;
        }
        Some(minimum)
    };
    let fpriv = if virgl {
        match file_private(file) {
            Ok(p) => {
                let ret = ensure_context(dev, p);
                if ret != 0 {
                    return ret;
                }
                Some(p)
            }
            Err(e) => return e,
        }
    } else {
        None
    };
    let (object, handle, new_object) = if arg.bo_handle == 0 {
        match alloc_shmem_handle(file, dev, size) {
            Ok((o, h)) => (o, h, true),
            Err(e) => return e,
        }
    } else {
        let ptr = unsafe { drm_gem_object_lookup(file, arg.bo_handle) };
        if ptr.is_null() {
            return -ENOENT;
        }
        let obj = ptr.cast::<VirtioGpuObject>();
        if unsafe { (*obj).resource_id != 0 } {
            unsafe {
                drm_gem_object_put(ptr);
            }
            return -tk_linux_kapi::config::EBUSY;
        }
        if min_2d_size.is_some_and(|minimum| unsafe { (*obj).base.base.size < minimum }) {
            unsafe {
                drm_gem_object_put(ptr);
            }
            return -EINVAL;
        }
        (obj, arg.bo_handle, false)
    };
    unsafe {
        (*object).format = arg.format;
    }
    let ret = if virgl {
        let pinned = unsafe { (*object).pinned };
        if !pinned {
            let pin = unsafe { drm_gem_shmem_pin(&mut (*object).base) };
            if pin != 0 {
                if new_object {
                    unsafe {
                        drm_gem_handle_delete(file, handle);
                    }
                }
                unsafe {
                    drm_gem_object_put(&mut (*object).base.base);
                }
                return pin;
            }
            unsafe {
                (*object).pinned = true;
            }
        }
        match physical_page_entries(object) {
            Err(e) => dev_errno(e),
            Ok(entries) => {
                let resource = RenderResource3D {
                    target: arg.target,
                    format: arg.format,
                    bind: arg.bind,
                    width: arg.width,
                    height: arg.height,
                    depth: arg.depth,
                    array_size: arg.array_size,
                    last_level: arg.last_level,
                    nr_samples: arg.nr_samples,
                    flags: arg.flags,
                };
                submit_create_3d(dev, fpriv.unwrap(), object, entries, resource)
            }
        }
    } else {
        create_classic_resource(object, arg.width, arg.height)
    };
    if ret != 0 {
        if new_object {
            unsafe {
                drm_gem_handle_delete(file, handle);
            }
        }
        unsafe {
            drm_gem_object_put(&mut (*object).base.base);
        }
        return ret;
    }
    unsafe {
        (*object).width = arg.width;
        (*object).height = arg.height;
        (*object).depth = if virgl { arg.depth } else { 1 };
        if !virgl {
            (*object).stride = arg.stride;
        }
        if new_object {
            arg.bo_handle = handle;
        }
        arg.res_handle = (*object).resource_id;
        drm_gem_object_put(&mut (*object).base.base);
    }
    0
}

fn ensure_context(
    dev: *mut tk_drm_upstream::drm_device_types::DrmDevice,
    fpriv: *mut FilePrivate,
) -> i32 {
    if fpriv.is_null() {
        return -EINVAL;
    }
    let Some(result) = (unsafe {
        with_device_state(dev, |state| {
            if (*fpriv).context_created {
                return Ok(());
            }
            let name = b"thekernel".to_vec();
            let id = state
                .engine
                .submit(
                    &mut *state.display,
                    GpuQueue::Control,
                    GpuBatch::CreateContext {
                        name,
                        init: if (*fpriv).initialized {
                            (*fpriv).init
                        } else {
                            ContextInit::default()
                        },
                    },
                )
                .map_err(dev_errno)?;
            let snapshot = state
                .engine
                .wait(&mut *state.display, id, WAIT_NS)
                .map_err(dev_errno)?;
            let context = snapshot.context_id.ok_or(-EIO)?;
            let _ = state.engine.reap(id);
            (*fpriv).context_id = context;
            (*fpriv).context_created = true;
            Ok(())
        })
    }) else {
        return -ENODEV;
    };
    result.err().unwrap_or(0)
}

fn submit_create_3d(
    dev: *mut tk_drm_upstream::drm_device_types::DrmDevice,
    fpriv: *mut FilePrivate,
    object: *mut VirtioGpuObject,
    entries: Vec<(u64, u32)>,
    resource: RenderResource3D,
) -> i32 {
    let context = unsafe { (*fpriv).context_id };
    let Some(result) = (unsafe {
        with_device_state(dev, |state| {
            let create = state
                .engine
                .submit(
                    &mut *state.display,
                    GpuQueue::Control,
                    GpuBatch::CreateResource3d { resource },
                )
                .map_err(dev_errno)?;
            let created = state
                .engine
                .wait(&mut *state.display, create, WAIT_NS)
                .map_err(dev_errno)?;
            let id = created.resource_id.ok_or(-EIO)?;
            unsafe {
                (*object).resource_id = id;
                (*object).resource_kind = 3;
            }
            let _ = state.engine.reap(create);
            let attach = state
                .engine
                .submit(
                    &mut *state.display,
                    GpuQueue::Control,
                    GpuBatch::AttachBacking {
                        resource: id,
                        entries,
                    },
                )
                .map_err(dev_errno)?;
            if let Err(e) = state.engine.wait(&mut *state.display, attach, WAIT_NS) {
                let _ = state.engine.reap(attach);
                return Err(dev_errno(e));
            }
            let _ = state.engine.reap(attach);
            let context_fence = state
                .engine
                .submit(
                    &mut *state.display,
                    GpuQueue::Control,
                    GpuBatch::AttachResource {
                        context,
                        resource: id,
                    },
                )
                .map_err(dev_errno)?;
            if let Err(error) = state
                .engine
                .wait(&mut *state.display, context_fence, WAIT_NS)
            {
                let _ = state.engine.reap(context_fence);
                return Err(dev_errno(error));
            }
            let _ = state.engine.reap(context_fence);
            Ok(id)
        })
    }) else {
        return -ENODEV;
    };
    match result {
        Ok(id) => unsafe {
            (*object).resource_id = id;
            (*object).resource_kind = 3;
            (*object).pinned = true;
            0
        },
        Err(e) => e,
    }
}

unsafe extern "C" fn ioctl_resource_info(
    _dev: *mut tk_drm_upstream::drm_device_types::DrmDevice,
    data: *mut c_void,
    file: *mut DrmFile,
) -> i32 {
    if data.is_null() || file.is_null() {
        return -EINVAL;
    }
    let arg = unsafe { &mut *data.cast::<DrmVirtgpuResourceInfo>() };
    let object = unsafe { drm_gem_object_lookup(file, arg.bo_handle) };
    if object.is_null() {
        return -ENOENT;
    }
    let gpu = object.cast::<VirtioGpuObject>();
    unsafe {
        arg.size = (*object).size.min(u32::MAX as usize) as u32;
        arg.res_handle = (*gpu).resource_id;
        arg.blob_mem = (*gpu).blob_mem;
        drm_gem_object_put(object);
    }
    0
}

unsafe extern "C" fn ioctl_transfer_to_host(
    dev: *mut tk_drm_upstream::drm_device_types::DrmDevice,
    data: *mut c_void,
    file: *mut DrmFile,
) -> i32 {
    transfer_ioctl(dev, data.cast::<DrmVirtgpu3dTransferToHost>(), file, true)
}
unsafe extern "C" fn ioctl_transfer_from_host(
    dev: *mut tk_drm_upstream::drm_device_types::DrmDevice,
    data: *mut c_void,
    file: *mut DrmFile,
) -> i32 {
    transfer_ioctl(
        dev,
        data.cast::<DrmVirtgpu3dTransferFromHost>(),
        file,
        false,
    )
}

unsafe fn transfer_ioctl(
    dev: *mut tk_drm_upstream::drm_device_types::DrmDevice,
    arg: *mut DrmVirtgpu3dTransferToHost,
    file: *mut DrmFile,
    to_host: bool,
) -> i32 {
    if arg.is_null() || file.is_null() {
        return -EINVAL;
    }
    let args = unsafe { *arg };
    let object = unsafe { drm_gem_object_lookup(file, args.bo_handle) };
    if object.is_null() {
        return -ENOENT;
    }
    let gpu = object.cast::<VirtioGpuObject>();
    let resource = unsafe { (*gpu).resource_id };
    let width = unsafe { (*gpu).width };
    let height = unsafe { (*gpu).height };
    let depth = unsafe { (*gpu).depth };
    let b = args.box_;
    if resource == 0
        || b.w == 0
        || b.h == 0
        || b.d == 0
        || b.x.checked_add(b.w).is_none_or(|x| width != 0 && x > width)
        || b.y
            .checked_add(b.h)
            .is_none_or(|y| height != 0 && y > height)
        || b.z.checked_add(b.d).is_none_or(|z| depth != 0 && z > depth)
    {
        unsafe {
            drm_gem_object_put(object);
        }
        return -EINVAL;
    }
    let is_guest_only_blob = unsafe { (*gpu).is_blob && (*gpu).blob_mem == VIRTGPU_BLOB_MEM_GUEST };
    if is_guest_only_blob {
        unsafe {
            drm_gem_object_put(object);
        }
        return -EINVAL;
    }
    let Some(virgl) = (unsafe {
        with_device_engine(dev, |engine, _| {
            engine.modern_features().contains(GpuFeatures::VIRGL_3D)
        })
    }) else {
        unsafe {
            drm_gem_object_put(object);
        }
        return -ENODEV;
    };
    if !to_host && !virgl {
        unsafe {
            drm_gem_object_put(object);
        }
        return -EOPNOTSUPP;
    }
    if !virgl && (b.z != 0 || b.d != 1) {
        unsafe {
            drm_gem_object_put(object);
        }
        return -EINVAL;
    }
    let is_host3d_blob = unsafe { (*gpu).is_blob && (*gpu).blob_mem != VIRTGPU_BLOB_MEM_GUEST };
    if virgl && !is_host3d_blob && (args.stride != 0 || args.layer_stride != 0) {
        unsafe {
            drm_gem_object_put(object);
        }
        return -EINVAL;
    }
    let fpriv = match file_private(file) {
        Ok(p) => p,
        Err(e) => {
            unsafe {
                drm_gem_object_put(object);
            }
            return e;
        }
    };
    if virgl {
        let r = ensure_context(dev, fpriv);
        if r != 0 {
            unsafe {
                drm_gem_object_put(object);
            }
            return r;
        }
    }
    let transfer = RenderTransfer3D {
        x: b.x,
        y: b.y,
        z: b.z,
        width: b.w,
        height: b.h,
        depth: b.d,
        offset: args.offset as u64,
        level: args.level,
        stride: args.stride,
        layer_stride: args.layer_stride,
    };
    let Some(result) = (unsafe {
        with_device_state(dev, |state| {
            // Reserve the tracking slot before submitting: after submission the
            // lookup reference must be retained until its terminal fence.
            state.pending_refs.try_reserve(1).map_err(|_| -ENOMEM)?;
            let mut refs = Vec::new();
            refs.try_reserve_exact(1).map_err(|_| -ENOMEM)?;
            refs.push(object as usize);
            let fence = if virgl {
                state
                    .engine
                    .submit(
                        &mut *state.display,
                        GpuQueue::Control,
                        GpuBatch::Transfer3d {
                            context: (*fpriv).context_id,
                            resource,
                            transfer,
                            to_host,
                        },
                    )
                    .map_err(dev_errno)?
            } else {
                state
                    .engine
                    .submit(
                        &mut *state.display,
                        GpuQueue::Control,
                        GpuBatch::Transfer2dRect {
                            resource,
                            x: b.x,
                            y: b.y,
                            width: b.w,
                            height: b.h,
                            offset: args.offset,
                        },
                    )
                    .map_err(dev_errno)?
            };
            state.pending_refs.push((fence, refs));
            (*(gpu)).last_fence_id = fence;
            (*(gpu)).last_fence_status = crate::gem::FENCE_PENDING;
            Ok(())
        })
    }) else {
        unsafe {
            drm_gem_object_put(object);
        }
        return -ENODEV;
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            unsafe {
                drm_gem_object_put(object);
            }
            e
        }
    }
}

unsafe extern "C" fn ioctl_wait(
    dev: *mut tk_drm_upstream::drm_device_types::DrmDevice,
    data: *mut c_void,
    file: *mut DrmFile,
) -> i32 {
    if data.is_null() || file.is_null() {
        return -EINVAL;
    }
    let arg = unsafe { &*data.cast::<DrmVirtgpu3dWait>() };
    let object = unsafe { drm_gem_object_lookup(file, arg.handle) };
    if object.is_null() {
        return -ENOENT;
    }
    let Some(result) = (unsafe {
        with_device_state(dev, |state| {
            let gpu = object.cast::<VirtioGpuObject>();
            let fence = (*gpu).last_fence_id;
            if fence == 0 || (*gpu).last_fence_status == crate::gem::FENCE_COMPLETE {
                return Ok(());
            }
            if (*gpu).last_fence_status == crate::gem::FENCE_FAILED {
                return Err(-EIO);
            }
            if (*gpu).last_fence_status != crate::gem::FENCE_PENDING {
                return Err(-EIO);
            }
            if arg.flags & VIRTGPU_WAIT_NOWAIT != 0 {
                let queue = state.engine.fence(fence).ok_or(-EIO)?.queue;
                let _ = state.engine.drain(&mut *state.display, queue);
                match state.engine.fence(fence).map(|f| f.status) {
                    Some(FenceStatus::Pending) => Err(-tk_linux_kapi::config::EBUSY),
                    Some(FenceStatus::Complete) => Ok(()),
                    _ => Err(-EIO),
                }
            } else {
                state
                    .engine
                    .wait(&mut *state.display, fence, WAIT_NS)
                    .map_err(dev_errno)?;
                Ok(())
            }
        })
    }) else {
        unsafe {
            drm_gem_object_put(object);
        }
        return -ENODEV;
    };
    let ret = result.err().unwrap_or(0);
    unsafe {
        drm_gem_object_put(object);
    }
    ret
}

unsafe extern "C" fn ioctl_get_caps(
    dev: *mut tk_drm_upstream::drm_device_types::DrmDevice,
    data: *mut c_void,
    _file: *mut DrmFile,
) -> i32 {
    if data.is_null() {
        return -EINVAL;
    }
    let arg = unsafe { &*data.cast::<DrmVirtgpuGetCaps>() };
    if arg.size == 0 || arg.size > MAX_COMMAND as u32 || arg.addr == 0 {
        return -EINVAL;
    }
    let Some(result) = (unsafe {
        with_device_engine(dev, |engine, display| {
            let Some((_, info)) = engine.capset(arg.cap_set_id) else {
                return Err(-EINVAL);
            };
            if arg.cap_set_ver > info.max_version || info.max_size as usize > MAX_COMMAND {
                return Err(-EINVAL);
            }
            let bytes = info.max_size as usize;
            let fence = engine
                .submit(
                    display,
                    GpuQueue::Control,
                    GpuBatch::Capset {
                        id: arg.cap_set_id,
                        version: arg.cap_set_ver,
                        bytes,
                    },
                )
                .map_err(dev_errno)?;
            engine.wait(display, fence, WAIT_NS).map_err(dev_errno)?;
            let data = engine.take_completion_data(fence).map_err(dev_errno)?;
            let _ = engine.reap(fence);
            let GpuCompletionData::Capset(bytes) = data else {
                return Err(-EIO);
            };
            Ok(bytes)
        })
    }) else {
        return -ENODEV;
    };
    match result {
        Err(e) => e,
        Ok(bytes) => {
            let len = bytes.len().min(arg.size as usize);
            if len != 0
                && unsafe {
                    copy_to_user(arg.addr as usize as *mut c_void, bytes.as_ptr().cast(), len)
                } != 0
            {
                -EFAULT
            } else {
                0
            }
        }
    }
}

unsafe extern "C" fn ioctl_blob_create(
    dev: *mut tk_drm_upstream::drm_device_types::DrmDevice,
    data: *mut c_void,
    file: *mut DrmFile,
) -> i32 {
    if data.is_null() || file.is_null() {
        return -EINVAL;
    }
    let arg = unsafe { &mut *data.cast::<DrmVirtgpuResourceCreateBlob>() };
    if arg.size == 0 || arg.blob_flags & !BLOB_USE_MASK != 0 || arg.cmd_size as usize > MAX_COMMAND
    {
        return -EINVAL;
    }
    let mem = match arg.blob_mem {
        VIRTGPU_BLOB_MEM_GUEST => BlobMem::Guest,
        VIRTGPU_BLOB_MEM_HOST3D => BlobMem::Host3d,
        VIRTGPU_BLOB_MEM_HOST3D_GUEST => BlobMem::Host3dGuest,
        _ => return -EINVAL,
    };
    let Some(features) = (unsafe { with_device_engine(dev, |e, _| e.modern_features()) }) else {
        return -ENODEV;
    };
    if !features.contains(GpuFeatures::RESOURCE_BLOB)
        || (mem != BlobMem::Guest && !features.contains(GpuFeatures::VIRGL_3D))
        || (arg.blob_flags & VIRTGPU_BLOB_FLAG_USE_CROSS_DEVICE != 0
            && !features.contains(GpuFeatures::RESOURCE_UUID))
    {
        return -EOPNOTSUPP;
    }
    if (mem == BlobMem::Guest && (arg.blob_id != 0 || arg.cmd_size != 0))
        || (mem != BlobMem::Guest && arg.cmd_size % 4 != 0)
    {
        return -EINVAL;
    }
    let fpriv = match file_private(file) {
        Ok(private) => private,
        Err(error) => return error,
    };
    if features.contains(GpuFeatures::VIRGL_3D) {
        let ret = ensure_context(dev, fpriv);
        if ret != 0 {
            return ret;
        }
    }
    let size = match usize::try_from(arg.size)
        .ok()
        .and_then(|s| s.checked_add(PAGE_SIZE - 1))
        .map(|s| s & !(PAGE_SIZE - 1))
    {
        Some(s) if s <= u32::MAX as usize => s,
        _ => return -EINVAL,
    };
    let (object, handle) = match alloc_shmem_handle(file, dev, size) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let entries = if mem == BlobMem::Guest || mem == BlobMem::Host3dGuest {
        let ret = unsafe { drm_gem_shmem_pin(&mut (*object).base) };
        if ret != 0 {
            drop_allocated_handle(file, object, handle);
            return ret;
        }
        unsafe {
            (*object).pinned = true;
        }
        match physical_page_entries(object) {
            Ok(v) => v,
            Err(e) => {
                drop_allocated_handle(file, object, handle);
                return dev_errno(e);
            }
        }
    } else {
        Vec::new()
    };
    let command = if arg.cmd_size == 0 {
        Vec::new()
    } else {
        if arg.cmd == 0 {
            drop_allocated_handle(file, object, handle);
            return -EFAULT;
        }
        let mut cmd = Vec::new();
        if cmd.try_reserve_exact(arg.cmd_size as usize).is_err() {
            drop_allocated_handle(file, object, handle);
            return -ENOMEM;
        }
        cmd.resize(arg.cmd_size as usize, 0);
        if unsafe {
            copy_from_user(
                cmd.as_mut_ptr().cast(),
                arg.cmd as usize as *const c_void,
                cmd.len(),
            )
        } != 0
        {
            drop_allocated_handle(file, object, handle);
            return -EFAULT;
        }
        cmd
    };
    let batch = GpuBatch::CreateBlob {
        resource: BlobResource {
            mem,
            flags: arg.blob_flags,
            size: arg.size,
            blob_id: arg.blob_id,
        },
        entries,
    };
    let Some(result) = (unsafe {
        with_device_state(dev, |state| {
            if !command.is_empty() {
                let fpriv = file_private(file).map_err(|e| e)?;
                ensure_context_locked(state, fpriv)?;
                let submit = state
                    .engine
                    .submit(
                        &mut *state.display,
                        GpuQueue::Control,
                        GpuBatch::Submit3d {
                            context: (*fpriv).context_id,
                            ring_idx: None,
                            commands: command,
                            resources: Vec::new(),
                        },
                    )
                    .map_err(dev_errno)?;
                state
                    .engine
                    .wait(&mut *state.display, submit, WAIT_NS)
                    .map_err(dev_errno)?;
                let _ = state.engine.reap(submit);
            }
            let fence = state
                .engine
                .submit(&mut *state.display, GpuQueue::Control, batch)
                .map_err(dev_errno)?;
            let created = state
                .engine
                .wait(&mut *state.display, fence, WAIT_NS)
                .map_err(dev_errno)?;
            let id = created.resource_id.ok_or(-EIO)?;
            let _ = state.engine.reap(fence);
            unsafe {
                (*object).resource_id = id;
            }
            let uuid = if arg.blob_flags & VIRTGPU_BLOB_FLAG_USE_CROSS_DEVICE != 0 {
                let fence = state
                    .engine
                    .submit(
                        &mut *state.display,
                        GpuQueue::Control,
                        GpuBatch::AssignUuid { resource: id },
                    )
                    .map_err(dev_errno)?;
                state
                    .engine
                    .wait(&mut *state.display, fence, WAIT_NS)
                    .map_err(dev_errno)?;
                let data = state
                    .engine
                    .take_completion_data(fence)
                    .map_err(dev_errno)?;
                let _ = state.engine.reap(fence);
                let GpuCompletionData::Uuid(uuid) = data else {
                    return Err(-EIO);
                };
                Some(uuid)
            } else {
                None
            };
            Ok((id, uuid))
        })
    }) else {
        drop_allocated_handle(file, object, handle);
        return -ENODEV;
    };
    match result {
        Ok((id, uuid)) => {
            if unsafe { (*fpriv).context_created } {
                let ret = context_resource_ioctl(dev, unsafe { (*fpriv).context_id }, id, true);
                if ret != 0 {
                    drop_allocated_handle(file, object, handle);
                    return ret;
                }
            }
            unsafe {
                (*object).resource_id = id;
                (*object).is_blob = true;
                (*object).blob_mem = arg.blob_mem;
                (*object).blob_flags = arg.blob_flags;
                if let Some(uuid) = uuid {
                    (*object).resource_uuid = uuid;
                }
                arg.bo_handle = handle;
                arg.res_handle = id;
                drm_gem_object_put(&mut (*object).base.base);
            }
            0
        }
        Err(error) => {
            drop_allocated_handle(file, object, handle);
            error
        }
    }
}

fn drop_allocated_handle(file: *mut DrmFile, object: *mut VirtioGpuObject, handle: u32) {
    unsafe {
        let _ = drm_gem_handle_delete(file, handle);
        drm_gem_object_put(&mut (*object).base.base);
    }
}

fn ensure_context_locked(
    state: &mut crate::device::DriverState,
    fpriv: *mut FilePrivate,
) -> Result<(), i32> {
    if unsafe { (*fpriv).context_created } {
        return Ok(());
    }
    let fence = state
        .engine
        .submit(
            &mut *state.display,
            GpuQueue::Control,
            GpuBatch::CreateContext {
                name: b"thekernel".to_vec(),
                init: if unsafe { (*fpriv).initialized } {
                    unsafe { (*fpriv).init }
                } else {
                    ContextInit::default()
                },
            },
        )
        .map_err(dev_errno)?;
    let snapshot = state
        .engine
        .wait(&mut *state.display, fence, WAIT_NS)
        .map_err(dev_errno)?;
    let id = snapshot.context_id.ok_or(-EIO)?;
    let _ = state.engine.reap(fence);
    unsafe {
        (*fpriv).context_id = id;
        (*fpriv).context_created = true;
    }
    Ok(())
}

unsafe extern "C" fn ioctl_context_init(
    dev: *mut tk_drm_upstream::drm_device_types::DrmDevice,
    data: *mut c_void,
    file: *mut DrmFile,
) -> i32 {
    if data.is_null() || file.is_null() {
        return -EINVAL;
    }
    let args = unsafe { *data.cast::<DrmVirtgpuContextInit>() };
    if args.num_params > 4 || (args.num_params != 0 && args.ctx_set_params == 0) {
        return -EINVAL;
    }
    let mut params = [DrmVirtgpuContextSetParam::default(); 4];
    if args.num_params != 0
        && unsafe {
            copy_from_user(
                params.as_mut_ptr().cast(),
                args.ctx_set_params as usize as *const c_void,
                args.num_params as usize * size_of::<DrmVirtgpuContextSetParam>(),
            )
        } != 0
    {
        return -EFAULT;
    }
    let Some(features) = (unsafe { with_device_engine(dev, |e, _| e.modern_features()) }) else {
        return -ENODEV;
    };
    if !features.contains(GpuFeatures::CONTEXT_INIT) || !features.contains(GpuFeatures::VIRGL_3D) {
        return -EOPNOTSUPP;
    }
    let Ok(privp) = file_private(file) else {
        return -ENODEV;
    };
    let mut init = ContextInit::default();
    let mut seen = 0u8;
    for p in params.iter().take(args.num_params as usize) {
        let bit = match p.param {
            VIRTGPU_CONTEXT_PARAM_CAPSET_ID => 1,
            VIRTGPU_CONTEXT_PARAM_NUM_RINGS => 2,
            VIRTGPU_CONTEXT_PARAM_POLL_RINGS_MASK => 4,
            VIRTGPU_CONTEXT_PARAM_DEBUG_NAME => 8,
            _ => return -EINVAL,
        };
        if seen & bit != 0 {
            return -EINVAL;
        }
        seen |= bit;
        match p.param {
            VIRTGPU_CONTEXT_PARAM_CAPSET_ID
                if p.value <= 63
                    && unsafe {
                        with_device_engine(dev, |e, _| e.capset_mask() & (1u64 << p.value) != 0)
                    }
                    .unwrap_or(false) =>
            {
                init.capset_id = p.value as u32
            }
            VIRTGPU_CONTEXT_PARAM_NUM_RINGS if p.value <= 64 => init.num_rings = p.value as u32,
            VIRTGPU_CONTEXT_PARAM_POLL_RINGS_MASK => init.poll_rings_mask = p.value,
            VIRTGPU_CONTEXT_PARAM_DEBUG_NAME => {
                let src = p.value as usize as *const u8;
                if src.is_null() {
                    return -EFAULT;
                }
                let mut bytes = [0u8; 64];
                let mut len = 0usize;
                while len < 64 {
                    let source = (src as usize).wrapping_add(len) as *const c_void;
                    if unsafe { copy_from_user(bytes.as_mut_ptr().add(len).cast(), source, 1) } != 0
                    {
                        return -EFAULT;
                    }
                    if bytes[len] == 0 {
                        break;
                    }
                    len += 1;
                }
                init.debug_name = bytes;
                init.debug_name_len = len as u8;
            }
            _ => return -EINVAL,
        }
    }
    let valid_mask = if init.num_rings == 64 {
        u64::MAX
    } else if init.num_rings == 0 {
        0
    } else {
        (1u64 << init.num_rings) - 1
    };
    if init.poll_rings_mask & !valid_mask != 0 {
        return -EINVAL;
    }
    let Some(result) = (unsafe {
        with_device_state(dev, |state| {
            if (*privp).context_created {
                return Err(-tk_linux_kapi::config::EEXIST);
            }
            (*privp).init = init;
            (*privp).initialized = true;
            if let Err(error) = ensure_context_locked(state, privp) {
                (*privp).init = ContextInit::default();
                (*privp).initialized = false;
                return Err(error);
            }
            Ok(())
        })
    }) else {
        return -ENODEV;
    };
    result.err().unwrap_or(0)
}

struct ExecOutputSyncobj {
    object: *mut DrmSyncobj,
    chain: *mut tk_linux_kapi::types::DmaFenceChain,
    point: u64,
}

struct ExecInputSyncobjs {
    reset_objects: Vec<*mut DrmSyncobj>,
}

impl Drop for ExecInputSyncobjs {
    fn drop(&mut self) {
        for object in self.reset_objects.drain(..) {
            unsafe {
                drm_syncobj_replace_fence(object, ptr::null_mut());
                drm_syncobj_put(object);
            }
        }
    }
}

struct ExecOutputSyncobjs {
    entries: Vec<ExecOutputSyncobj>,
}

impl ExecOutputSyncobjs {
    fn publish(&mut self, fence: *mut DmaFence) {
        for entry in &mut self.entries {
            unsafe {
                if entry.point == 0 {
                    drm_syncobj_replace_fence(entry.object, fence);
                } else {
                    drm_syncobj_add_point(entry.object, entry.chain, fence, entry.point);
                    entry.chain = ptr::null_mut();
                }
            }
        }
    }
}

impl Drop for ExecOutputSyncobjs {
    fn drop(&mut self) {
        for entry in self.entries.drain(..) {
            unsafe {
                if !entry.chain.is_null() {
                    drm_syncobj_chain_free(entry.chain);
                }
                drm_syncobj_put(entry.object);
            }
        }
    }
}

struct ExecFenceResources {
    fence: *mut DmaFence,
    sync_file: *mut c_void,
    fd: c_int,
}

impl ExecFenceResources {
    fn new(fence: *mut DmaFence) -> Self {
        Self {
            fence,
            sync_file: ptr::null_mut(),
            fd: -1,
        }
    }

    fn publish_fd(&mut self, args: &mut DrmVirtgpuExecbuffer) -> i32 {
        if self.sync_file.is_null() || self.fd < 0 {
            return -EINVAL;
        }
        let file = unsafe { drm_syncobj_sync_file_file(self.sync_file) };
        if file.is_null() {
            return -EINVAL;
        }
        unsafe { tk_linux_kapi::kernel_services::fd_install(self.fd, file) };
        args.fence_fd = self.fd;
        self.fd = -1;
        self.sync_file = ptr::null_mut();
        0
    }

    fn take_fence(&mut self) -> *mut DmaFence {
        core::mem::replace(&mut self.fence, ptr::null_mut())
    }
}

impl Drop for ExecFenceResources {
    fn drop(&mut self) {
        unsafe {
            if !self.sync_file.is_null() {
                // A provider may have installed a native mirror task which
                // waits for this fence, so error-signal before dropping the
                // unpublished wrapper.
                if !self.fence.is_null() {
                    drm_syncobj_fence_signal(self.fence, -EIO);
                }
                let file = drm_syncobj_sync_file_file(self.sync_file);
                drm_syncobj_sync_file_fput(file);
            }
            if self.fd >= 0 {
                tk_linux_kapi::kernel_services::put_unused_fd(self.fd);
            }
            if !self.fence.is_null() {
                drm_syncobj_fence_put(self.fence);
            }
        }
    }
}

fn copy_exec_syncobj_descs(
    pointer: u64,
    stride: u32,
    count: u32,
) -> Result<Vec<DrmVirtgpuExecbufferSyncobj>, i32> {
    if count as usize > MAX_HANDLES {
        return Err(-EINVAL);
    }
    if count != 0 && pointer == 0 {
        return Err(-EFAULT);
    }
    let mut descriptors = Vec::new();
    descriptors
        .try_reserve_exact(count as usize)
        .map_err(|_| -ENOMEM)?;
    let copy_len = (stride as usize).min(size_of::<DrmVirtgpuExecbufferSyncobj>());
    for index in 0..count as usize {
        let mut descriptor = DrmVirtgpuExecbufferSyncobj::default();
        if copy_len != 0 {
            let offset = (index as u64)
                .checked_mul(stride as u64)
                .and_then(|offset| pointer.checked_add(offset))
                .ok_or(-EINVAL)?;
            if unsafe {
                copy_from_user(
                    (&mut descriptor as *mut DrmVirtgpuExecbufferSyncobj).cast(),
                    offset as usize as *const c_void,
                    copy_len,
                )
            } != 0
            {
                return Err(-EFAULT);
            }
        }
        descriptors.push(descriptor);
    }
    Ok(descriptors)
}

fn parse_exec_input_syncobjs(
    file: *mut DrmFile,
    args: &DrmVirtgpuExecbuffer,
) -> Result<Vec<*mut DrmSyncobj>, i32> {
    let descriptors =
        copy_exec_syncobj_descs(args.in_syncobjs, args.syncobj_stride, args.num_in_syncobjs)?;
    let mut reset_objects = Vec::new();
    reset_objects
        .try_reserve_exact(descriptors.len())
        .map_err(|_| -ENOMEM)?;
    for descriptor in descriptors {
        if descriptor.flags & !VIRTGPU_EXECBUF_SYNCOBJ_FLAGS != 0 {
            for object in reset_objects {
                unsafe { drm_syncobj_put(object) };
            }
            return Err(-EINVAL);
        }
        let mut fence = ptr::null_mut();
        let ret = unsafe {
            drm_syncobj_find_fence(file, descriptor.handle, descriptor.point, 0, &mut fence)
        };
        if ret != 0 {
            for object in reset_objects {
                unsafe { drm_syncobj_put(object) };
            }
            return Err(ret);
        }
        let wait =
            unsafe { tk_drm_upstream::drm_syncobj_upstream::drm_syncobj_wait_fence(fence, true) };
        unsafe { drm_syncobj_fence_put(fence) };
        if wait != 0 {
            for object in reset_objects {
                unsafe { drm_syncobj_put(object) };
            }
            return Err(wait);
        }
        if descriptor.flags & VIRTGPU_EXECBUF_SYNCOBJ_RESET != 0 {
            let object = unsafe { drm_syncobj_find(file, descriptor.handle) };
            if object.is_null() {
                for object in reset_objects {
                    unsafe { drm_syncobj_put(object) };
                }
                return Err(-ENOENT);
            }
            reset_objects.push(object);
        }
    }
    Ok(reset_objects)
}

fn parse_exec_output_syncobjs(
    file: *mut DrmFile,
    args: &DrmVirtgpuExecbuffer,
) -> Result<Vec<ExecOutputSyncobj>, i32> {
    let descriptors = copy_exec_syncobj_descs(
        args.out_syncobjs,
        args.syncobj_stride,
        args.num_out_syncobjs,
    )?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(descriptors.len())
        .map_err(|_| -ENOMEM)?;
    for descriptor in descriptors {
        if descriptor.flags != 0 {
            release_exec_output_syncobjs(output);
            return Err(-EINVAL);
        }
        let object = unsafe { drm_syncobj_find(file, descriptor.handle) };
        if object.is_null() {
            release_exec_output_syncobjs(output);
            return Err(-ENOENT);
        }
        let chain = if descriptor.point == 0 {
            ptr::null_mut()
        } else {
            let chain = drm_syncobj_chain_alloc();
            if chain.is_null() {
                unsafe { drm_syncobj_put(object) };
                release_exec_output_syncobjs(output);
                return Err(-ENOMEM);
            }
            chain
        };
        output.push(ExecOutputSyncobj {
            object,
            chain,
            point: descriptor.point,
        });
    }
    Ok(output)
}

fn release_exec_output_syncobjs(output: Vec<ExecOutputSyncobj>) {
    for entry in output {
        unsafe {
            if !entry.chain.is_null() {
                drm_syncobj_chain_free(entry.chain);
            }
            drm_syncobj_put(entry.object);
        }
    }
}

fn finish_submission(private: Arc<DriverPrivate>, fence_id: u64, fence: usize) {
    let status = private.wait_submission_completion(fence_id);
    let error = if matches!(status, Some(Ok(()))) {
        0
    } else {
        -EIO
    };
    unsafe {
        drm_syncobj_fence_signal(fence as *mut DmaFence, error);
        drm_syncobj_fence_put(fence as *mut DmaFence);
    }
}

fn queue_submission_completion(private: Arc<DriverPrivate>, fence_id: u64, fence: *mut DmaFence) {
    let fallback = private.clone();
    let fence_owner = fence as usize;
    if axtask::spawn_with_name(
        move || finish_submission(private, fence_id, fence_owner),
        "virtgpu-fence".to_string(),
    )
    .is_err()
    {
        // The command is already owned by the lower queue. If a worker cannot
        // be admitted, synchronously retire it rather than leak GEM/fence refs.
        finish_submission(fallback, fence_id, fence_owner);
    }
}

unsafe extern "C" fn ioctl_execbuffer(
    dev: *mut tk_drm_upstream::drm_device_types::DrmDevice,
    data: *mut c_void,
    file: *mut DrmFile,
) -> i32 {
    if data.is_null() || file.is_null() {
        return -EINVAL;
    }
    let mut args = unsafe { *data.cast::<DrmVirtgpuExecbuffer>() };
    if args.flags & !VIRTGPU_EXECBUF_FLAGS != 0 {
        return -EINVAL;
    }
    if args.size == 0
        || args.size as usize > MAX_COMMAND
        || args.num_bo_handles as usize > MAX_HANDLES
        || args.command == 0
        || (args.num_bo_handles != 0 && args.bo_handles == 0)
        || (args.flags & VIRTGPU_EXECBUF_FENCE_FD_IN != 0 && args.fence_fd < 0)
    {
        return -EINVAL;
    }
    let fpriv = match file_private(file) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let Some(virgl) = (unsafe {
        with_device_engine(dev, |e, _| {
            e.modern_features().contains(GpuFeatures::VIRGL_3D)
        })
    }) else {
        return -ENODEV;
    };
    if !virgl {
        return -EOPNOTSUPP;
    }
    let ret = ensure_context(dev, fpriv);
    if ret != 0 {
        return ret;
    }

    let Some(private) = (unsafe { private_arc_from_device(dev) }) else {
        return -ENODEV;
    };
    let fence = drm_syncobj_fence_create();
    if fence.is_null() {
        return -ENOMEM;
    }
    let mut fence_resources = ExecFenceResources::new(fence);
    if args.flags & VIRTGPU_EXECBUF_FENCE_FD_OUT != 0 {
        let fd = unsafe { tk_linux_kapi::kernel_services::get_unused_fd_flags(0x80000) };
        if fd < 0 {
            return fd;
        }
        fence_resources.fd = fd;
        let sync_file = unsafe { drm_syncobj_sync_file_create(fence_resources.fence) };
        if sync_file.is_null() {
            return -ENOMEM;
        }
        fence_resources.sync_file = sync_file;
    }

    let mut command = Vec::new();
    if command.try_reserve_exact(args.size as usize).is_err() {
        return -ENOMEM;
    }
    command.resize(args.size as usize, 0);
    if args.size != 0
        && unsafe {
            copy_from_user(
                command.as_mut_ptr().cast(),
                args.command as usize as *const c_void,
                args.size as usize,
            )
        } != 0
    {
        return -EFAULT;
    }

    let mut handles = Vec::new();
    if handles
        .try_reserve_exact(args.num_bo_handles as usize)
        .is_err()
    {
        return -ENOMEM;
    }
    handles.resize(args.num_bo_handles as usize, 0);
    if !handles.is_empty()
        && unsafe {
            copy_from_user(
                handles.as_mut_ptr().cast(),
                args.bo_handles as usize as *const c_void,
                handles.len() * 4,
            )
        } != 0
    {
        return -EFAULT;
    }
    let mut objects = Vec::new();
    let mut resources = Vec::new();
    if objects.try_reserve_exact(handles.len()).is_err()
        || resources.try_reserve_exact(handles.len()).is_err()
    {
        return -ENOMEM;
    }
    for handle in handles {
        let obj = unsafe { drm_gem_object_lookup(file, handle) };
        if obj.is_null() {
            for p in objects {
                unsafe {
                    drm_gem_object_put(p);
                }
            }
            return -ENOENT;
        }
        let rid = unsafe { (*(obj.cast::<VirtioGpuObject>())).resource_id };
        if rid == 0 {
            unsafe {
                drm_gem_object_put(obj);
            }
            for p in objects {
                unsafe {
                    drm_gem_object_put(p);
                }
            }
            return -EINVAL;
        }
        resources.push(rid);
        objects.push(obj);
    }
    let mut ptrs = Vec::new();
    if ptrs.try_reserve_exact(objects.len()).is_err() {
        for p in objects {
            unsafe { drm_gem_object_put(p) }
        }
        return -ENOMEM;
    }
    ptrs.extend(objects.iter().map(|p| *p as usize));

    let output_entries = match parse_exec_output_syncobjs(file, &args) {
        Ok(output) => output,
        Err(error) => {
            for object in objects {
                unsafe { drm_gem_object_put(object) }
            }
            return error;
        }
    };
    let mut output_syncobjs = ExecOutputSyncobjs {
        entries: output_entries,
    };

    // The upstream driver resolves syncobj dependencies before submitting a
    // command and only resets requested input syncobjs once dependency parsing
    // succeeded. The RAII owner therefore resets on every path after this call.
    let input_syncobjs = match parse_exec_input_syncobjs(file, &args) {
        Ok(objects) => ExecInputSyncobjs {
            reset_objects: objects,
        },
        Err(error) => {
            for object in objects {
                unsafe { drm_gem_object_put(object) }
            }
            return error;
        }
    };

    if args.flags & VIRTGPU_EXECBUF_FENCE_FD_IN != 0 {
        let fence = unsafe { drm_syncobj_sync_file_get_fence(args.fence_fd) };
        if fence.is_null() {
            for object in objects {
                unsafe { drm_gem_object_put(object) }
            }
            return -EINVAL;
        }
        let wait =
            unsafe { tk_drm_upstream::drm_syncobj_upstream::drm_syncobj_wait_fence(fence, true) };
        unsafe { drm_syncobj_fence_put(fence) };
        if wait != 0 {
            for object in objects {
                unsafe { drm_gem_object_put(object) }
            }
            return wait;
        }
    }
    let Some(result) = (unsafe {
        with_device_state(dev, |state| {
            state.pending_refs.try_reserve(1).map_err(|_| -ENOMEM)?;
            let rings = if (*fpriv).init.num_rings == 0 {
                1
            } else {
                (*fpriv).init.num_rings
            };
            let ring = if args.flags & VIRTGPU_EXECBUF_RING_IDX != 0 {
                if args.ring_idx >= rings {
                    return Err(-EINVAL);
                }
                Some(args.ring_idx)
            } else {
                None
            };
            let fence = state
                .engine
                .submit(
                    &mut *state.display,
                    GpuQueue::Control,
                    GpuBatch::Submit3d {
                        context: (*fpriv).context_id,
                        ring_idx: ring,
                        commands: command,
                        resources,
                    },
                )
                .map_err(dev_errno)?;
            for p in &objects {
                unsafe {
                    (*(p.cast::<VirtioGpuObject>())).last_fence_id = fence;
                    (*(p.cast::<VirtioGpuObject>())).last_fence_status = crate::gem::FENCE_PENDING;
                }
            }
            state.pending_refs.push((fence, ptrs));
            Ok(fence)
        })
    }) else {
        for p in objects {
            unsafe { drm_gem_object_put(p) }
        }
        return -ENODEV;
    };
    let fence_id = match result {
        Ok(fence_id) => fence_id,
        Err(error) => {
            for p in objects {
                unsafe { drm_gem_object_put(p) }
            }
            return error;
        }
    };

    output_syncobjs.publish(fence_resources.fence);
    if args.flags & VIRTGPU_EXECBUF_FENCE_FD_OUT != 0 {
        let ret = fence_resources.publish_fd(&mut args);
        if ret != 0 {
            // `fd_install` itself is infallible; this is only a defensive
            // check for a malformed sync-file provider response. The command
            // has already been accepted, so still retire it asynchronously.
            queue_submission_completion(private.clone(), fence_id, fence_resources.take_fence());
            return ret;
        }
    }
    drop(input_syncobjs);
    let completion_fence = fence_resources.take_fence();
    queue_submission_completion(private, fence_id, completion_fence);
    unsafe {
        *data.cast::<DrmVirtgpuExecbuffer>() = args;
    }
    0
}

fn dev_errno(error: DevError) -> i32 {
    match error {
        DevError::NoMemory => -ENOMEM,
        DevError::InvalidParam => -EINVAL,
        DevError::Unsupported => -EOPNOTSUPP,
        DevError::Again => -tk_linux_kapi::config::EAGAIN,
        DevError::ResourceBusy => -tk_linux_kapi::config::EBUSY,
        _ => -EIO,
    }
}
