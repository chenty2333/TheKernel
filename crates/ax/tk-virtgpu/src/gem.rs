// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors.
//! VirtIO GPU GEM/shmem objects and classic 2D resources.
//!
//! GEM's page allocator, mmap and PRIME shmem mechanics come from the landed
//! DRM shmem helper. This file adds only VirtIO resource identity/lifetime and
//! constructs the physical-page vectors consumed by the already existing
//! display transport (which owns DMA mapping and virtqueue descriptors).

use alloc::vec::Vec;
use core::{ffi::c_void, mem::size_of, ptr};

use axdriver_display::{DevError, DevResult, GpuBatch, GpuQueue};
use tk_drm_upstream::{
    drm_device_types::DrmDevice,
    drm_file_types::DrmFile,
    drm_gem_shmem_helper_types_upstream::DrmGemShmemObject,
    drm_gem_shmem_helper_upstream::{
        drm_gem_shmem_create, drm_gem_shmem_get_sg_table, drm_gem_shmem_object_free,
        drm_gem_shmem_object_get_sg_table, drm_gem_shmem_object_mmap, drm_gem_shmem_object_pin,
        drm_gem_shmem_object_print_info, drm_gem_shmem_object_unpin, drm_gem_shmem_object_vmap,
        drm_gem_shmem_object_vunmap, drm_gem_shmem_pin, drm_gem_shmem_prime_import_sg_table,
        drm_gem_shmem_unpin,
    },
    drm_gem_types::{DrmGemObject, DrmGemObjectFuncs},
    drm_prime_upstream::{DmaBuf, DmaBufAttachment, drm_gem_prime_export, drm_gem_prime_import},
};
use tk_linux_kapi::{
    config::{ERR_PTR, GFP_KERNEL, IS_ERR, PAGE_SIZE, PTR_ERR},
    memory::{kfree, kmalloc},
    scatterlist::{sg_free_table, sg_next, sg_page},
    shmem::try_page_to_phys,
    types::SgTable,
};

use crate::device::with_device_engine;

const RESOURCE_CLASSIC_2D: u32 = 1;
const FENCE_TIMEOUT_NS: u64 = 5_000_000_000;
pub(crate) const FENCE_PENDING: u32 = 1;
pub(crate) const FENCE_COMPLETE: u32 = 2;
pub(crate) const FENCE_FAILED: u32 = 3;

#[repr(C)]
pub(crate) struct VirtioGpuObject {
    pub base: DrmGemShmemObject,
    pub resource_id: u32,
    pub resource_kind: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub format: u32,
    pub stride: u32,
    pub pinned: bool,
    pub last_fence_id: u64,
    pub last_fence_status: u32,
    pub blob_mem: u32,
    pub blob_flags: u32,
    pub blob_hints: u32,
    pub resource_uuid: [u8; 16],
    pub is_blob: bool,
}

pub(crate) unsafe extern "C" fn virtgpu_gem_prime_import(
    dev: *mut DrmDevice,
    dma_buf: *mut c_void,
) -> *mut DrmGemObject {
    unsafe { drm_gem_prime_import(dev, dma_buf.cast::<DmaBuf>()) }
}

struct SyncGemFuncs(DrmGemObjectFuncs);
unsafe impl Sync for SyncGemFuncs {}

static VIRTGPU_GEM_FUNCS: SyncGemFuncs = SyncGemFuncs(DrmGemObjectFuncs {
    free: Some(virtgpu_gem_free),
    open: Some(crate::ioctl::virtgpu_gem_open),
    close: Some(crate::ioctl::virtgpu_gem_close),
    print_info: Some(drm_gem_shmem_object_print_info),
    export: Some(virtgpu_gem_export),
    pin: Some(drm_gem_shmem_object_pin),
    unpin: Some(drm_gem_shmem_object_unpin),
    get_sg_table: Some(drm_gem_shmem_object_get_sg_table),
    vmap: Some(drm_gem_shmem_object_vmap),
    vunmap: Some(drm_gem_shmem_object_vunmap),
    mmap: Some(drm_gem_shmem_object_mmap),
    evict: None,
    status: None,
    rss: None,
    vm_ops: ptr::null_mut(),
});

fn object_from_gem(obj: *mut DrmGemObject) -> *mut VirtioGpuObject {
    obj.cast()
}

fn shmem_from_object(obj: *mut VirtioGpuObject) -> *mut DrmGemShmemObject {
    obj.cast()
}

/// Driver hook used by the Linux shmem helper to allocate the extended object.
pub(crate) unsafe extern "C" fn virtgpu_gem_create_object(
    _dev: *mut DrmDevice,
    _size: usize,
) -> *mut DrmGemObject {
    let object =
        unsafe { kmalloc(size_of::<VirtioGpuObject>(), GFP_KERNEL) }.cast::<VirtioGpuObject>();
    if object.is_null() {
        return ERR_PTR::<DrmGemObject>(-tk_linux_kapi::config::ENOMEM);
    }
    unsafe { ptr::write_bytes(object.cast::<u8>(), 0, size_of::<VirtioGpuObject>()) };
    object.cast()
}

/// Driver hook for imported shmem PRIME buffers. GEM/shmem owns the imported
/// attachment and mapping; the VirtIO fields remain uncreated until a resource
/// operation explicitly requests a host resource.
pub(crate) unsafe extern "C" fn virtgpu_gem_prime_import_sg_table(
    dev: *mut DrmDevice,
    attach: *mut c_void,
    sgt: *mut SgTable,
) -> *mut DrmGemObject {
    let object =
        unsafe { drm_gem_shmem_prime_import_sg_table(dev, attach.cast::<DmaBufAttachment>(), sgt) };
    if !object.is_null() && !IS_ERR(object) {
        unsafe { (*object).funcs = ptr::addr_of!(VIRTGPU_GEM_FUNCS.0) as *mut _ };
    }
    object
}

unsafe extern "C" fn virtgpu_gem_export(obj: *mut DrmGemObject, flags: i32) -> *mut c_void {
    unsafe { drm_gem_prime_export(obj, flags).cast() }
}

/// Linux v7.2.3 `virtio_gpu_mode_dumb_create()`: allocate a linear XRGB8888
/// shmem GEM, create its 2D host resource, and return the DRM handle.
pub(crate) unsafe extern "C" fn virtgpu_dumb_create(
    file: *mut DrmFile,
    dev: *mut DrmDevice,
    args: *mut tk_drm_upstream::uapi_drm_mode_types::DrmModeCreateDumb,
) -> i32 {
    if file.is_null() || dev.is_null() || args.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    unsafe {
        let width = (*args).width;
        let height = (*args).height;
        if (*args).bpp != 32 || width == 0 || height == 0 || width > 8192 || height > 8192 {
            return -tk_linux_kapi::config::EINVAL;
        }
        let Some(pitch) = width.checked_mul(4) else {
            return -tk_linux_kapi::config::EINVAL;
        };
        let Some(raw_size) = (pitch as usize).checked_mul(height as usize) else {
            return -tk_linux_kapi::config::EINVAL;
        };
        let Some(size) = raw_size.checked_add(PAGE_SIZE - 1) else {
            return -tk_linux_kapi::config::EINVAL;
        };
        let size = size & !(PAGE_SIZE - 1);
        if size == 0 || size > u32::MAX as usize {
            return -tk_linux_kapi::config::EINVAL;
        }

        let shmem = drm_gem_shmem_create(dev, size);
        if shmem.is_null() || IS_ERR(shmem) {
            return if shmem.is_null() {
                -tk_linux_kapi::config::ENOMEM
            } else {
                PTR_ERR(shmem.cast::<c_void>())
            };
        }
        let object = shmem.cast::<VirtioGpuObject>();
        (*object).base.base.funcs = ptr::addr_of!(VIRTGPU_GEM_FUNCS.0) as *mut _;
        (*object).width = width;
        (*object).height = height;
        (*object).depth = 1;
        (*object).format = 2;
        (*object).stride = pitch;

        let ret = create_classic_resource(object, width, height);
        if ret != 0 {
            tk_drm_upstream::drm_gem_upstream::drm_gem_object_put(&mut (*object).base.base);
            return ret;
        }

        let mut handle = 0u32;
        let ret = tk_drm_upstream::drm_gem_upstream::drm_gem_handle_create(
            file,
            &mut (*object).base.base,
            &mut handle,
        );
        if ret != 0 {
            tk_drm_upstream::drm_gem_upstream::drm_gem_object_put(&mut (*object).base.base);
            return ret;
        }
        tk_drm_upstream::drm_gem_upstream::drm_gem_object_put(&mut (*object).base.base);
        (*args).handle = handle;
        (*args).pitch = pitch;
        (*args).size = size as u64;
        0
    }
}

/// Create a plain linear resource for a GEM object. Page addresses are
/// produced from the helper-pinned physical pages; DMA mapping and descriptor
/// ownership remain wholly in the lower display transport.
pub(crate) fn create_classic_resource(
    object: *mut VirtioGpuObject,
    width: u32,
    height: u32,
) -> i32 {
    if object.is_null() || width == 0 || height == 0 {
        return -tk_linux_kapi::config::EINVAL;
    }
    unsafe {
        if (*object).resource_id != 0 {
            return 0;
        }
        let ret = drm_gem_shmem_pin(shmem_from_object(object));
        if ret != 0 {
            return ret;
        }
        (*object).pinned = true;
    }

    let entries = match physical_page_entries(object) {
        Ok(entries) => entries,
        Err(error) => {
            unsafe {
                drm_gem_shmem_unpin(shmem_from_object(object));
                (*object).pinned = false;
            }
            return dev_error_errno(error);
        }
    };
    let drm = unsafe { (*object).base.base.dev };
    let Some(result) = (unsafe {
        with_device_engine(drm, |engine, display| {
            let create = engine
                .submit(
                    display,
                    GpuQueue::Control,
                    GpuBatch::Create2d {
                        width,
                        height,
                        format: if unsafe { (*object).format == 0 } {
                            2
                        } else {
                            unsafe { (*object).format }
                        },
                        entries: Vec::new(),
                    },
                )
                .map_err(dev_error_errno)?;
            let created = engine
                .wait(display, create, FENCE_TIMEOUT_NS)
                .map_err(dev_error_errno)?;
            let resource_id = created.resource_id.ok_or(-tk_linux_kapi::config::EIO)?;
            let _ = engine.reap(create);

            let attach = match engine.submit(
                display,
                GpuQueue::Control,
                GpuBatch::AttachBacking {
                    resource: resource_id,
                    entries,
                },
            ) {
                Ok(fence) => fence,
                Err(error) => {
                    if let Ok(unref) = engine.submit(
                        display,
                        GpuQueue::Control,
                        GpuBatch::UnrefResource {
                            resource: resource_id,
                        },
                    ) {
                        let _ = engine.wait(display, unref, FENCE_TIMEOUT_NS);
                        let _ = engine.reap(unref);
                    }
                    return Err(dev_error_errno(error));
                }
            };
            if let Err(error) = engine.wait(display, attach, FENCE_TIMEOUT_NS) {
                let _ = engine.reap(attach);
                let unref = engine.submit(
                    display,
                    GpuQueue::Control,
                    GpuBatch::UnrefResource {
                        resource: resource_id,
                    },
                );
                if let Ok(unref) = unref {
                    let _ = engine.wait(display, unref, FENCE_TIMEOUT_NS);
                    let _ = engine.reap(unref);
                }
                return Err(dev_error_errno(error));
            }
            let _ = engine.reap(attach);
            Ok(resource_id)
        })
    }) else {
        unsafe {
            drm_gem_shmem_unpin(shmem_from_object(object));
            (*object).pinned = false;
        }
        return -tk_linux_kapi::config::ENODEV;
    };
    let resource_id = match result {
        Ok(resource_id) => resource_id,
        Err(error) => {
            unsafe {
                drm_gem_shmem_unpin(shmem_from_object(object));
                (*object).pinned = false;
            }
            return error;
        }
    };

    unsafe {
        (*object).resource_id = resource_id;
        (*object).resource_kind = RESOURCE_CLASSIC_2D;
        (*object).width = width;
        (*object).height = height;
        (*object).depth = 1;
        if (*object).format == 0 {
            (*object).format = 2;
        }
        (*object).stride = width.saturating_mul(4);
    }
    0
}

pub(crate) fn physical_page_entries(object: *mut VirtioGpuObject) -> DevResult<Vec<(u64, u32)>> {
    if object.is_null() {
        return Err(DevError::InvalidParam);
    }
    unsafe {
        let shmem = shmem_from_object(object);
        let sgt = drm_gem_shmem_get_sg_table(shmem);
        if sgt.is_null() || IS_ERR(sgt) {
            return Err(if sgt.is_null() {
                DevError::NoMemory
            } else {
                DevError::Unsupported
            });
        }
        let count = (*sgt).nents as usize;
        let mut entries = Vec::new();
        entries.try_reserve_exact(count).map_err(|_| {
            sg_free_table(sgt);
            kfree(sgt.cast::<c_void>());
            DevError::NoMemory
        })?;
        let mut sg = (*sgt).sgl;
        for _ in 0..count {
            if sg.is_null() {
                sg_free_table(sgt);
                kfree(sgt.cast::<c_void>());
                return Err(DevError::BadState);
            }
            let page = sg_page(sg);
            let Some(physical) = try_page_to_phys(page) else {
                sg_free_table(sgt);
                kfree(sgt.cast::<c_void>());
                return Err(DevError::BadState);
            };
            let Some(address) = physical.checked_add((*sg).offset as usize) else {
                sg_free_table(sgt);
                kfree(sgt.cast::<c_void>());
                return Err(DevError::InvalidParam);
            };
            entries.push((address as u64, (*sg).length));
            sg = sg_next(sg);
        }
        sg_free_table(sgt);
        kfree(sgt.cast::<c_void>());
        Ok(entries)
    }
}

/// Allocate an extended shmem GEM and install a DRM handle, leaving the
/// initial GEM reference with the caller so the resource ioctl can safely
/// fill its output fields before dropping it.
pub(crate) fn alloc_shmem_handle(
    file: *mut DrmFile,
    dev: *mut DrmDevice,
    size: usize,
) -> Result<(*mut VirtioGpuObject, u32), i32> {
    if file.is_null() || dev.is_null() || size == 0 {
        return Err(-tk_linux_kapi::config::EINVAL);
    }
    let shmem = unsafe { drm_gem_shmem_create(dev, size) };
    if shmem.is_null() || IS_ERR(shmem) {
        return Err(if shmem.is_null() {
            -tk_linux_kapi::config::ENOMEM
        } else {
            unsafe { PTR_ERR(shmem.cast::<c_void>()) }
        });
    }
    let object = shmem.cast::<VirtioGpuObject>();
    unsafe { (*object).base.base.funcs = ptr::addr_of!(VIRTGPU_GEM_FUNCS.0) as *mut _ };
    let mut handle = 0;
    let ret = unsafe {
        tk_drm_upstream::drm_gem_upstream::drm_gem_handle_create(
            file,
            &mut (*object).base.base,
            &mut handle,
        )
    };
    if ret != 0 {
        unsafe { tk_drm_upstream::drm_gem_upstream::drm_gem_object_put(&mut (*object).base.base) };
        return Err(ret);
    }
    Ok((object, handle))
}

unsafe extern "C" fn virtgpu_gem_free(obj: *mut DrmGemObject) {
    if obj.is_null() {
        return;
    }
    let object = object_from_gem(obj);
    let resource_id = unsafe { (*object).resource_id };
    if resource_id != 0 {
        let drm = unsafe { (*obj).dev };
        let _ = unsafe {
            with_device_engine(drm, |engine, display| {
                if let Ok(fence) = engine.submit(
                    display,
                    GpuQueue::Control,
                    GpuBatch::DetachResourceEverywhere {
                        resource: resource_id,
                    },
                ) {
                    let _ = engine.wait(display, fence, FENCE_TIMEOUT_NS);
                    let _ = engine.reap(fence);
                }
                let fence_id = engine.submit(
                    display,
                    GpuQueue::Control,
                    GpuBatch::UnrefResource {
                        resource: resource_id,
                    },
                );
                if let Ok(fence_id) = fence_id {
                    let _ = engine.wait(display, fence_id, FENCE_TIMEOUT_NS);
                    let _ = engine.reap(fence_id);
                } else {
                    engine.reset(display, GpuQueue::Control);
                }
            })
        };
    }
    unsafe {
        if (*object).pinned {
            drm_gem_shmem_unpin(shmem_from_object(object));
            (*object).pinned = false;
        }
        drm_gem_shmem_object_free(obj);
    }
}

/// Helper-compatible GEM allocator used by CREATE and framebuffer paths.
pub(crate) fn create_gem_handle(
    file: *mut DrmFile,
    dev: *mut DrmDevice,
    width: u32,
    height: u32,
) -> Result<(u32, u32), i32> {
    if file.is_null() || dev.is_null() || width == 0 || height == 0 || width > 8192 || height > 8192
    {
        return Err(-tk_linux_kapi::config::EINVAL);
    }
    let pitch = width.checked_mul(4).ok_or(-tk_linux_kapi::config::EINVAL)?;
    let size = (pitch as usize)
        .checked_mul(height as usize)
        .and_then(|size| size.checked_add(PAGE_SIZE - 1))
        .map(|size| size & !(PAGE_SIZE - 1))
        .ok_or(-tk_linux_kapi::config::EINVAL)?;
    let shmem = unsafe { drm_gem_shmem_create(dev, size) };
    if shmem.is_null() || IS_ERR(shmem) {
        return Err(if shmem.is_null() {
            -tk_linux_kapi::config::ENOMEM
        } else {
            unsafe { PTR_ERR(shmem.cast::<c_void>()) }
        });
    }
    let object = shmem.cast::<VirtioGpuObject>();
    unsafe {
        (*object).base.base.funcs = ptr::addr_of!(VIRTGPU_GEM_FUNCS.0) as *mut _;
        (*object).width = width;
        (*object).height = height;
        (*object).depth = 1;
        (*object).format = 2;
        (*object).stride = pitch;
    }
    let ret = create_classic_resource(object, width, height);
    if ret != 0 {
        unsafe { tk_drm_upstream::drm_gem_upstream::drm_gem_object_put(&mut (*object).base.base) };
        return Err(ret);
    }
    let mut handle = 0;
    let ret = unsafe {
        tk_drm_upstream::drm_gem_upstream::drm_gem_handle_create(
            file,
            &mut (*object).base.base,
            &mut handle,
        )
    };
    if ret != 0 {
        unsafe { tk_drm_upstream::drm_gem_upstream::drm_gem_object_put(&mut (*object).base.base) };
        return Err(ret);
    }
    let resource_id = unsafe { (*object).resource_id };
    unsafe { tk_drm_upstream::drm_gem_upstream::drm_gem_object_put(&mut (*object).base.base) };
    Ok((handle, resource_id))
}

fn dev_error_errno(error: DevError) -> i32 {
    match error {
        DevError::NoMemory => -tk_linux_kapi::config::ENOMEM,
        DevError::InvalidParam => -tk_linux_kapi::config::EINVAL,
        DevError::Unsupported => -tk_linux_kapi::config::EOPNOTSUPP,
        DevError::Again => -tk_linux_kapi::config::EAGAIN,
        DevError::ResourceBusy => -tk_linux_kapi::config::EBUSY,
        DevError::BadState | DevError::Io => -tk_linux_kapi::config::EIO,
        DevError::AlreadyExists => -tk_linux_kapi::config::EEXIST,
    }
}
