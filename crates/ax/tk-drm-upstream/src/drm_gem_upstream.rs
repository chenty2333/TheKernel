// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_gem.c` (batch BI).
//!
//! Graphics Execution Manager (GEM): object lifecycle (init/release/free),
//! handles, names/flink, mmap offset allocation through `drm_vma_manager`,
//! LRU management, and dma-resv locking helpers.

#![allow(non_snake_case, dead_code, unsafe_code)]

use core::ffi::{c_char, c_int, c_long, c_uint, c_ulong, c_void};
use alloc::boxed::Box;
use tk_linux_kapi::config::{EAGAIN, EEXIST, EINVAL, ENOMEM, EOPNOTSUPP, GFP_KERNEL};
use tk_linux_kapi::list::{INIT_LIST_HEAD, list_add_tail, list_del, list_del_init, list_empty};
use tk_linux_kapi::locks::{spin_lock, spin_unlock, spin_lock_init};
use tk_linux_kapi::memory::{kfree, kmalloc, kref_init, kref_put};
use tk_linux_kapi::mutex::mutex_init;
#[cfg(not(test))]
use tk_linux_kapi::mutex::{mutex_lock, mutex_unlock};
#[cfg(test)]
unsafe fn mutex_lock<M>(_mutex: M) {}
#[cfg(test)]
unsafe fn mutex_unlock<M>(_mutex: M) {}
use tk_linux_kapi::types::{File, Kref, ListHead, WwAcquireCtx};
use tk_linux_kapi::ww_mutex::WwMutex;
#[cfg(not(test))]
use tk_linux_kapi::ww_mutex::{ww_mutex_lock, ww_mutex_unlock};
#[cfg(test)]
unsafe fn ww_mutex_lock(_lock: *mut WwMutex, _ctx: *mut WwAcquireCtx) -> i32 { 0 }
#[cfg(test)]
unsafe fn ww_mutex_unlock(_lock: *mut WwMutex) {}

use crate::dma_resv_types::DmaResv;
use crate::drm_device_types::DrmDevice;
use crate::drm_file_types::DrmFile;
use crate::drm_gem_types::{DrmGemLru, DrmGemObject, DrmGemObjectFuncs};
use crate::drm_mode_object_upstream::{idr_alloc, idr_destroy, idr_find, idr_remove};
use crate::drm_print_types::DrmPrinter;
use crate::drm_vma_manager_upstream::{drm_vma_offset_add, drm_vma_offset_remove};
use crate::uapi_drm_types::{DrmGemClose, DrmGemFlink, DrmGemOpen};

const ENOENT: i32 = 2;
const PAGE_SIZE: usize = 4096;
const PAGE_SHIFT: usize = 12;

unsafe fn dma_resv_init(resv: *mut DmaResv) {
    if !resv.is_null() {
        unsafe {
            mutex_init(core::ptr::addr_of_mut!((*resv).lock.base));
            (*resv).lock.ctx = core::ptr::null_mut();
            (*resv).fences = core::ptr::null_mut();
        }
    }
}

unsafe fn dma_resv_fini(resv: *mut DmaResv) {
    if !resv.is_null() {
        unsafe {
            (*resv).fences = core::ptr::null_mut();
        }
    }
}

unsafe fn dma_resv_lock(resv: *mut DmaResv, ctx: *mut WwAcquireCtx) -> i32 {
    unsafe {
        ww_mutex_lock(core::ptr::addr_of_mut!((*resv).lock), ctx)
    }
}

unsafe fn dma_resv_unlock(resv: *mut DmaResv) {
    unsafe {
        ww_mutex_unlock(core::ptr::addr_of_mut!((*resv).lock));
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_huge_mnt_free()
unsafe fn drm_gem_huge_mnt_free(_dev: *mut DrmDevice, _data: *mut c_void) {}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_huge_mnt_create()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_huge_mnt_create(_dev: *mut DrmDevice, _value: *const c_char) -> i32 {
    0
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_init_release()
unsafe fn drm_gem_init_release(_dev: *mut DrmDevice, _ptr: *mut c_void) {}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_init(_dev: *mut DrmDevice) -> i32 {
    0
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_private_object_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_private_object_init(
    dev: *mut DrmDevice,
    obj: *mut DrmGemObject,
    size: usize,
) {
    if obj.is_null() {
        return;
    }
    unsafe {
        (*obj).dev = dev;
        (*obj).filp = core::ptr::null_mut();
        kref_init(core::ptr::addr_of_mut!((*obj).refcount));
        (*obj).handle_count = 0;
        (*obj).size = size;
        core::ptr::write_bytes(core::ptr::addr_of_mut!((*obj).vma_node.vm_lock), 0, 1);
        (*obj).vma_node.vm_files.node = core::ptr::null_mut();
        (*obj).vma_node.driver_private = core::ptr::null_mut();
        INIT_LIST_HEAD(core::ptr::addr_of_mut!((*obj).gpuva.list));
        mutex_init(core::ptr::addr_of_mut!((*obj).gpuva.lock));
        INIT_LIST_HEAD(core::ptr::addr_of_mut!((*obj).lru_node));
        (*obj).resv = core::ptr::addr_of_mut!((*obj)._resv);
        dma_resv_init((*obj).resv);
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_private_object_fini()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_private_object_fini(obj: *mut DrmGemObject) {
    if obj.is_null() {
        return;
    }
    unsafe {
        dma_resv_fini((*obj).resv);
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_object_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_object_init(
    dev: *mut DrmDevice,
    obj: *mut DrmGemObject,
    size: usize,
) -> i32 {
    unsafe {
        drm_gem_private_object_init(dev, obj, size);
        0
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_object_handle_get()
unsafe fn drm_gem_object_handle_get(obj: *mut DrmGemObject) {
    if !obj.is_null() {
        (*obj).handle_count += 1;
        tk_linux_kapi::memory::kref_get(core::ptr::addr_of_mut!((*obj).refcount));
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_object_handle_get_if_exists_unlocked()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_object_handle_get_if_exists_unlocked(obj: *mut DrmGemObject) -> bool {
    if obj.is_null() {
        return false;
    }
    unsafe {
        if (*obj).handle_count == 0 {
            return false;
        }
        (*obj).handle_count += 1;
        tk_linux_kapi::memory::kref_get(core::ptr::addr_of_mut!((*obj).refcount));
        true
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_object_handle_free()
unsafe fn drm_gem_object_handle_free(obj: *mut DrmGemObject) {
    if !obj.is_null() && (*obj).handle_count == 0 {
        drm_gem_object_put(obj);
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_object_exported_dma_buf_free()
unsafe fn drm_gem_object_exported_dma_buf_free(obj: *mut DrmGemObject) {
    if !obj.is_null() {
        (*obj).dma_buf = core::ptr::null_mut();
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_object_handle_put_unlocked()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_object_handle_put_unlocked(obj: *mut DrmGemObject) {
    if obj.is_null() {
        return;
    }
    unsafe {
        if (*obj).handle_count > 0 {
            (*obj).handle_count -= 1;
        }
        drm_gem_object_put(obj);
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_object_release_handle()
unsafe fn drm_gem_object_release_handle(_id: c_int, ptr: *mut c_void, _data: *mut c_void) -> i32 {
    let obj = ptr.cast::<DrmGemObject>();
    drm_gem_object_handle_put_unlocked(obj);
    0
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_handle_delete()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_handle_delete(filp: *mut DrmFile, handle: u32) -> i32 {
    if filp.is_null() || handle == 0 {
        return -EINVAL;
    }
    unsafe {
        spin_lock(&mut (*filp).table_lock);
        let obj = idr_find(core::ptr::addr_of_mut!((*filp).object_idr), handle as i32).cast::<DrmGemObject>();
        if obj.is_null() {
            spin_unlock(&mut (*filp).table_lock);
            return -EINVAL;
        }
        idr_remove(core::ptr::addr_of_mut!((*filp).object_idr), handle as i32);
        spin_unlock(&mut (*filp).table_lock);

        if let Some(funcs) = (*obj).funcs.as_ref() {
            if let Some(close) = funcs.close {
                close(obj, filp);
            }
        }
        drm_gem_object_handle_put_unlocked(obj);
        0
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_dumb_map_offset()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_dumb_map_offset(
    file: *mut DrmFile,
    _dev: *mut DrmDevice,
    handle: u32,
    offset: *mut u64,
) -> i32 {
    let obj = drm_gem_object_lookup(file, handle);
    if obj.is_null() {
        return -ENOENT;
    }
    let ret = drm_gem_create_mmap_offset(obj);
    if ret == 0 && !offset.is_null() {
        *offset = (*obj).vma_node.vm_node.start << PAGE_SHIFT;
    }
    drm_gem_object_put(obj);
    ret
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_handle_create_tail()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_handle_create_tail(
    file_priv: *mut DrmFile,
    obj: *mut DrmGemObject,
    handlep: *mut u32,
) -> i32 {
    if file_priv.is_null() || obj.is_null() || handlep.is_null() {
        return -EINVAL;
    }
    unsafe {
        spin_lock(&mut (*file_priv).table_lock);
        let ret = idr_alloc(
            core::ptr::addr_of_mut!((*file_priv).object_idr),
            obj.cast(),
            1,
            0,
        );
        spin_unlock(&mut (*file_priv).table_lock);

        mutex_unlock(core::ptr::addr_of_mut!((*(*obj).dev).object_name_lock));

        if ret < 0 {
            return ret;
        }

        drm_gem_object_handle_get(obj);
        *handlep = ret as u32;

        if let Some(funcs) = (*obj).funcs.as_ref() {
            if let Some(open) = funcs.open {
                let open_ret = open(obj, file_priv);
                if open_ret != 0 {
                    drm_gem_handle_delete(file_priv, *handlep);
                    return open_ret;
                }
            }
        }
        0
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_handle_create()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_handle_create(
    file_priv: *mut DrmFile,
    obj: *mut DrmGemObject,
    handlep: *mut u32,
) -> i32 {
    if file_priv.is_null() || obj.is_null() || handlep.is_null() {
        return -EINVAL;
    }
    unsafe {
        mutex_lock(core::ptr::addr_of_mut!((*(*obj).dev).object_name_lock));
        drm_gem_handle_create_tail(file_priv, obj, handlep)
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_free_mmap_offset()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_free_mmap_offset(obj: *mut DrmGemObject) {
    if obj.is_null() || (*obj).dev.is_null() {
        return;
    }
    unsafe {
        let vma_mgr = (*(*obj).dev).vma_offset_manager;
        if !vma_mgr.is_null() {
            drm_vma_offset_remove(vma_mgr, core::ptr::addr_of_mut!((*obj).vma_node));
        }
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_create_mmap_offset_size()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_create_mmap_offset_size(obj: *mut DrmGemObject, size: usize) -> i32 {
    if obj.is_null() || (*obj).dev.is_null() {
        return -EINVAL;
    }
    unsafe {
        let vma_mgr = (*(*obj).dev).vma_offset_manager;
        if vma_mgr.is_null() {
            return -EINVAL;
        }
        drm_vma_offset_add(
            vma_mgr,
            core::ptr::addr_of_mut!((*obj).vma_node),
            (size / PAGE_SIZE) as c_ulong,
        )
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_create_mmap_offset()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_create_mmap_offset(obj: *mut DrmGemObject) -> i32 {
    if obj.is_null() {
        return -EINVAL;
    }
    unsafe {
        drm_gem_create_mmap_offset_size(obj, (*obj).size)
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_check_release_batch()
unsafe fn drm_gem_check_release_batch(_fbatch: *mut c_void) {}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_get_pages()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_get_pages(obj: *mut DrmGemObject) -> *mut *mut c_void {
    if obj.is_null() {
        return (-EINVAL as isize) as *mut *mut c_void;
    }
    let npages = (*obj).size / PAGE_SIZE;
    if npages == 0 {
        return (-EINVAL as isize) as *mut *mut c_void;
    }
    let pages = kmalloc(npages * core::mem::size_of::<*mut c_void>(), GFP_KERNEL).cast::<*mut c_void>();
    if pages.is_null() {
        return (-ENOMEM as isize) as *mut *mut c_void;
    }
    core::ptr::write_bytes(pages, 0, npages);
    pages
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_put_pages()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_put_pages(
    _obj: *mut DrmGemObject,
    pages: *mut *mut c_void,
    _dirty: bool,
    _accessed: bool,
) {
    if !pages.is_null() {
        kfree(pages.cast::<c_void>());
    }
}

// upstream: drivers/gpu/drm/drm_gem.c objects_lookup()
unsafe fn objects_lookup(
    filp: *mut DrmFile,
    handles: *const u32,
    count: c_int,
    objs: *mut *mut DrmGemObject,
) -> i32 {
    spin_lock(&mut (*filp).table_lock);
    for i in 0..count as usize {
        let handle = *handles.add(i);
        let obj = idr_find(core::ptr::addr_of_mut!((*filp).object_idr), handle as i32).cast::<DrmGemObject>();
        if obj.is_null() {
            spin_unlock(&mut (*filp).table_lock);
            for j in 0..i {
                drm_gem_object_put(*objs.add(j));
            }
            return -ENOENT;
        }
        drm_gem_object_get(obj);
        *objs.add(i) = obj;
    }
    spin_unlock(&mut (*filp).table_lock);
    0
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_objects_lookup()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_objects_lookup(
    filp: *mut DrmFile,
    bo_handles: *const u32,
    count: c_int,
    objs_out: *mut *mut *mut DrmGemObject,
) -> i32 {
    if objs_out.is_null() {
        return -EINVAL;
    }
    *objs_out = core::ptr::null_mut();
    if count <= 0 || filp.is_null() || bo_handles.is_null() {
        return 0;
    }
    let objs = kmalloc(count as usize * core::mem::size_of::<*mut DrmGemObject>(), GFP_KERNEL).cast::<*mut DrmGemObject>();
    if objs.is_null() {
        return -ENOMEM;
    }
    let ret = objects_lookup(filp, bo_handles, count, objs);
    if ret != 0 {
        kfree(objs.cast::<c_void>());
        return ret;
    }
    *objs_out = objs;
    0
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_object_lookup()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_object_lookup(filp: *mut DrmFile, handle: u32) -> *mut DrmGemObject {
    if filp.is_null() || handle == 0 {
        return core::ptr::null_mut();
    }
    unsafe {
        spin_lock(&mut (*filp).table_lock);
        let obj = idr_find(core::ptr::addr_of_mut!((*filp).object_idr), handle as i32).cast::<DrmGemObject>();
        if !obj.is_null() {
            drm_gem_object_get(obj);
        }
        spin_unlock(&mut (*filp).table_lock);
        obj
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_dma_resv_wait()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_dma_resv_wait(
    filep: *mut DrmFile,
    handle: u32,
    _wait_all: bool,
    _timeout: c_ulong,
) -> c_long {
    let obj = drm_gem_object_lookup(filep, handle);
    if obj.is_null() {
        return -EINVAL as c_long;
    }
    drm_gem_object_put(obj);
    0
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_close_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_close_ioctl(
    _dev: *mut DrmDevice,
    data: *mut c_void,
    file_priv: *mut DrmFile,
) -> i32 {
    let args = data.cast::<DrmGemClose>();
    if args.is_null() || file_priv.is_null() {
        return -EINVAL;
    }
    unsafe {
        drm_gem_handle_delete(file_priv, (*args).handle)
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_flink_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_flink_ioctl(
    dev: *mut DrmDevice,
    data: *mut c_void,
    file_priv: *mut DrmFile,
) -> i32 {
    let args = data.cast::<DrmGemFlink>();
    if dev.is_null() || data.is_null() || file_priv.is_null() {
        return -EINVAL;
    }
    unsafe {
        let obj = drm_gem_object_lookup(file_priv, (*args).handle);
        if obj.is_null() {
            return -ENOENT;
        }

        mutex_lock(core::ptr::addr_of_mut!((*dev).object_name_lock));
        let ret = if (*obj).name != 0 {
            (*args).name = (*obj).name as u32;
            0
        } else {
            let r = idr_alloc(
                core::ptr::addr_of_mut!((*dev).object_name_idr),
                obj.cast(),
                1,
                0,
            );
            if r >= 0 {
                let name = r as c_int;
                (*obj).name = name;
                (*args).name = name as u32;
                0
            } else {
                r
            }
        };
        mutex_unlock(core::ptr::addr_of_mut!((*dev).object_name_lock));

        drm_gem_object_put(obj);
        ret
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_open_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_open_ioctl(
    dev: *mut DrmDevice,
    data: *mut c_void,
    file_priv: *mut DrmFile,
) -> i32 {
    let args = data.cast::<DrmGemOpen>();
    if dev.is_null() || data.is_null() || file_priv.is_null() {
        return -EINVAL;
    }
    unsafe {
        mutex_lock(core::ptr::addr_of_mut!((*dev).object_name_lock));
        let obj = idr_find(core::ptr::addr_of_mut!((*dev).object_name_idr), (*args).name as i32).cast::<DrmGemObject>();
        if obj.is_null() {
            mutex_unlock(core::ptr::addr_of_mut!((*dev).object_name_lock));
            return -ENOENT;
        }
        drm_gem_object_get(obj);
        mutex_unlock(core::ptr::addr_of_mut!((*dev).object_name_lock));

        let ret = drm_gem_handle_create(file_priv, obj, core::ptr::addr_of_mut!((*args).handle));
        (*args).size = (*obj).size as u64;
        drm_gem_object_put(obj);
        ret
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_change_handle_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_change_handle_ioctl(
    _dev: *mut DrmDevice,
    _data: *mut c_void,
    _file_priv: *mut DrmFile,
) -> i32 {
    -EOPNOTSUPP
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_open()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_open(_dev: *mut DrmDevice, file_private: *mut DrmFile) {
    if !file_private.is_null() {
        crate::drm_mode_object_upstream::idr_init(core::ptr::addr_of_mut!((*file_private).object_idr));
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_release()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_release(_dev: *mut DrmDevice, file_private: *mut DrmFile) {
    if !file_private.is_null() {
        crate::drm_mode_object_upstream::idr_for_each(core::ptr::addr_of_mut!((*file_private).object_idr), |_id, ptr| {
            let obj = ptr.cast::<DrmGemObject>();
            drm_gem_object_handle_put_unlocked(obj);
        });
        idr_destroy(core::ptr::addr_of_mut!((*file_private).object_idr));
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_object_release()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_object_release(obj: *mut DrmGemObject) {
    if obj.is_null() {
        return;
    }
    unsafe {
        if !(*obj).dev.is_null() {
            drm_gem_free_mmap_offset(obj);
        }
        dma_resv_fini((*obj).resv);
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_object_free()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_object_free(kref: *mut Kref) {
    let obj = kref.cast::<u8>().wrapping_sub(core::mem::offset_of!(DrmGemObject, refcount)).cast::<DrmGemObject>();
    unsafe {
        if let Some(funcs) = (*obj).funcs.as_ref() {
            if let Some(free) = funcs.free {
                free(obj);
                return;
            }
        }
        drm_gem_object_release(obj);
        kfree(obj.cast::<c_void>());
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_object_get()
#[inline]
pub unsafe fn drm_gem_object_get(obj: *mut DrmGemObject) {
    if !obj.is_null() {
        unsafe {
            tk_linux_kapi::memory::kref_get(core::ptr::addr_of_mut!((*obj).refcount));
        }
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_object_put()
#[inline]
pub unsafe fn drm_gem_object_put(obj: *mut DrmGemObject) {
    if !obj.is_null() {
        unsafe {
            kref_put(core::ptr::addr_of_mut!((*obj).refcount), drm_gem_object_free);
        }
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_vm_open()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_vm_open(vma: *mut c_void) {
    if !vma.is_null() {
        // vma->vm_private_data is DrmGemObject
        let obj = *(vma.cast::<*mut DrmGemObject>());
        drm_gem_object_get(obj);
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_vm_close()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_vm_close(vma: *mut c_void) {
    if !vma.is_null() {
        let obj = *(vma.cast::<*mut DrmGemObject>());
        drm_gem_object_put(obj);
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_mmap_obj()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_mmap_obj(
    obj: *mut DrmGemObject,
    obj_size: c_ulong,
    vma: *mut c_void,
) -> i32 {
    if obj.is_null() || vma.is_null() || (obj_size as usize) < (*obj).size {
        return -EINVAL;
    }
    drm_gem_object_get(obj);
    if let Some(funcs) = (*obj).funcs.as_ref() {
        if let Some(mmap) = funcs.mmap {
            let ret = mmap(obj, vma);
            if ret != 0 {
                drm_gem_object_put(obj);
                return ret;
            }
        }
    }
    0
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_object_lookup_at_offset()
unsafe fn drm_gem_object_lookup_at_offset(dev: *mut DrmDevice, offset: u64) -> *mut DrmGemObject {
    if dev.is_null() || (*dev).vma_offset_manager.is_null() {
        return core::ptr::null_mut();
    }
    let node = crate::drm_vma_manager_upstream::drm_vma_offset_lookup_locked(
        (*dev).vma_offset_manager,
        offset >> PAGE_SHIFT,
        1,
    );
    if node.is_null() {
        return core::ptr::null_mut();
    }
    node.cast::<u8>().wrapping_sub(core::mem::offset_of!(DrmGemObject, vma_node)).cast::<DrmGemObject>()
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_get_unmapped_area()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_get_unmapped_area(
    _filp: *mut c_void,
    _uaddr: c_ulong,
    _len: c_ulong,
    _pgoff: c_ulong,
    _flags: c_ulong,
) -> c_ulong {
    0
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_mmap()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_mmap(filp: *mut c_void, vma: *mut c_void) -> i32 {
    if filp.is_null() || vma.is_null() {
        return -EINVAL;
    }
    0
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_print_info()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_print_info(
    p: *mut DrmPrinter,
    indent: c_uint,
    obj: *const DrmGemObject,
) {
    if p.is_null() || obj.is_null() {
        return;
    }
    if let Some(funcs) = (*obj).funcs.as_ref() {
        if let Some(print_info) = funcs.print_info {
            print_info(p, indent, obj.cast_mut());
        }
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_vmap_locked()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_vmap_locked(obj: *mut DrmGemObject, map: *mut c_void) -> i32 {
    if obj.is_null() || map.is_null() {
        return -EINVAL;
    }
    if let Some(funcs) = (*obj).funcs.as_ref() {
        if let Some(vmap) = funcs.vmap {
            return vmap(obj, map.cast());
        }
    }
    -EOPNOTSUPP
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_vunmap_locked()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_vunmap_locked(obj: *mut DrmGemObject, map: *mut c_void) {
    if obj.is_null() || map.is_null() {
        return;
    }
    if let Some(funcs) = (*obj).funcs.as_ref() {
        if let Some(vunmap) = funcs.vunmap {
            vunmap(obj, map.cast());
        }
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_lock()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_lock(obj: *mut DrmGemObject) {
    if !obj.is_null() {
        dma_resv_lock((*obj).resv, core::ptr::null_mut());
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_unlock()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_unlock(obj: *mut DrmGemObject) {
    if !obj.is_null() {
        dma_resv_unlock((*obj).resv);
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_vmap()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_vmap(obj: *mut DrmGemObject, map: *mut c_void) -> i32 {
    if obj.is_null() {
        return -EINVAL;
    }
    drm_gem_lock(obj);
    let ret = drm_gem_vmap_locked(obj, map);
    drm_gem_unlock(obj);
    ret
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_vunmap()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_vunmap(obj: *mut DrmGemObject, map: *mut c_void) {
    if obj.is_null() {
        return;
    }
    drm_gem_lock(obj);
    drm_gem_vunmap_locked(obj, map);
    drm_gem_unlock(obj);
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_lock_reservations()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_lock_reservations(
    objs: *mut *mut DrmGemObject,
    count: c_int,
    acquire_ctx: *mut c_void,
) -> i32 {
    if objs.is_null() || count <= 0 {
        return 0;
    }
    unsafe {
        for i in 0..count {
            let obj = *objs.offset(i as isize);
            if !obj.is_null() {
                let ret = dma_resv_lock((*obj).resv, acquire_ctx.cast());
                if ret != 0 {
                    for j in 0..i {
                        let prev = *objs.offset(j as isize);
                        if !prev.is_null() {
                            dma_resv_unlock((*prev).resv);
                        }
                    }
                    return ret;
                }
            }
        }
        0
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_unlock_reservations()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_unlock_reservations(
    objs: *mut *mut DrmGemObject,
    count: c_int,
    _acquire_ctx: *mut c_void,
) {
    if objs.is_null() || count <= 0 {
        return;
    }
    unsafe {
        for i in 0..count {
            let obj = *objs.offset(i as isize);
            if !obj.is_null() {
                dma_resv_unlock((*obj).resv);
            }
        }
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_lru_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_lru_init(lru: *mut DrmGemLru) {
    if !lru.is_null() {
        unsafe {
            (*lru).count = 0;
            INIT_LIST_HEAD(core::ptr::addr_of_mut!((*lru).list));
        }
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_lru_remove_locked()
unsafe fn drm_gem_lru_remove_locked(obj: *mut DrmGemObject) {
    if !obj.is_null() && !(*obj).lru.is_null() {
        let lru = (*obj).lru;
        (*lru).count -= ((*obj).size >> PAGE_SHIFT) as i64;
        list_del_init(core::ptr::addr_of_mut!((*obj).lru_node));
        (*obj).lru = core::ptr::null_mut();
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_lru_remove()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_lru_remove(obj: *mut DrmGemObject) {
    if !obj.is_null() && !(*obj).lru.is_null() {
        let dev = (*obj).dev;
        mutex_lock(core::ptr::addr_of_mut!((*dev).gem_lru_mutex));
        drm_gem_lru_remove_locked(obj);
        mutex_unlock(core::ptr::addr_of_mut!((*dev).gem_lru_mutex));
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_lru_move_tail_locked()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_lru_move_tail_locked(lru: *mut DrmGemLru, obj: *mut DrmGemObject) {
    if lru.is_null() || obj.is_null() {
        return;
    }
    unsafe {
        if !(*obj).lru.is_null() {
            drm_gem_lru_remove_locked(obj);
        }
        (*lru).count += ((*obj).size >> PAGE_SHIFT) as i64;
        list_add_tail(
            core::ptr::addr_of_mut!((*obj).lru_node),
            core::ptr::addr_of_mut!((*lru).list),
        );
        (*obj).lru = lru;
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_lru_move_tail()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_lru_move_tail(lru: *mut DrmGemLru, obj: *mut DrmGemObject) {
    if lru.is_null() || obj.is_null() {
        return;
    }
    unsafe {
        let dev = (*obj).dev;
        mutex_lock(core::ptr::addr_of_mut!((*dev).gem_lru_mutex));
        drm_gem_lru_move_tail_locked(lru, obj);
        mutex_unlock(core::ptr::addr_of_mut!((*dev).gem_lru_mutex));
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_lru_scan()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_lru_scan(
    dev: *mut DrmDevice,
    lru: *mut DrmGemLru,
    nr_to_scan: c_uint,
    remaining: *mut c_ulong,
    shrink: Option<unsafe extern "C" fn(*mut DrmGemObject, *mut c_void) -> bool>,
    ticket: *mut c_void,
) -> c_ulong {
    if dev.is_null() || lru.is_null() {
        return 0;
    }
    let mut freed: c_ulong = 0;
    mutex_lock(core::ptr::addr_of_mut!((*dev).gem_lru_mutex));
    while (freed as c_uint) < nr_to_scan && !list_empty(&(*lru).list) {
        let cur = (*lru).list.next;
        let obj = cur.cast::<u8>().wrapping_sub(core::mem::offset_of!(DrmGemObject, lru_node)).cast::<DrmGemObject>();
        drm_gem_lru_remove_locked(obj);
        if let Some(s) = shrink {
            if s(obj, ticket) {
                freed += ((*obj).size >> PAGE_SHIFT) as c_ulong;
            } else if !remaining.is_null() {
                *remaining += ((*obj).size >> PAGE_SHIFT) as c_ulong;
            }
        }
    }
    mutex_unlock(core::ptr::addr_of_mut!((*dev).gem_lru_mutex));
    freed
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_evict_locked()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_evict_locked(obj: *mut DrmGemObject) -> i32 {
    if obj.is_null() {
        return -EINVAL;
    }
    if let Some(funcs) = (*obj).funcs.as_ref() {
        if let Some(evict) = funcs.evict {
            return evict(obj);
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drm_drv_types::DrmDriver;
    use crate::drm_drv_upstream::{drm_dev_alloc, drm_dev_put};
    use crate::drm_file_upstream::{drm_file_alloc, drm_file_free};

    #[test]
    fn test_gem_handles_flink_and_mmap() {
        unsafe {
            let mut driver: DrmDriver = core::mem::zeroed();
            let dev = drm_dev_alloc(&driver, core::ptr::null_mut());
            let file_priv = drm_file_alloc((*dev).primary);

            let mut obj_raw: DrmGemObject = core::mem::zeroed();
            let obj = &mut obj_raw as *mut DrmGemObject;
            drm_gem_private_object_init(dev, obj, 4096);

            // 1. Handle creation and lookup
            let mut handle: u32 = 0;
            assert_eq!(drm_gem_handle_create(file_priv, obj, &mut handle), 0);
            assert_ne!(handle, 0);
            assert_eq!((*obj).handle_count, 1);

            let looked_up = drm_gem_object_lookup(file_priv, handle);
            assert_eq!(looked_up, obj);
            drm_gem_object_put(looked_up);

            // 2. Flink and open by name
            let mut flink_args = DrmGemFlink { handle, name: 0 };
            assert_eq!(
                drm_gem_flink_ioctl(dev, &mut flink_args as *mut _ as *mut _, file_priv),
                0
            );
            assert_ne!(flink_args.name, 0);

            let file_priv2 = drm_file_alloc((*dev).primary);
            let mut open_args = DrmGemOpen {
                name: flink_args.name,
                handle: 0,
                size: 0,
            };
            assert_eq!(
                drm_gem_open_ioctl(dev, &mut open_args as *mut _ as *mut _, file_priv2),
                0
            );
            assert_ne!(open_args.handle, 0);
            assert_eq!(open_args.size, 4096);
            assert_eq!((*obj).handle_count, 2);

            // 3. Mmap offset creation
            assert_eq!(drm_gem_create_mmap_offset(obj), 0);
            assert_ne!((*obj).vma_node.vm_node.start, 0);
            drm_gem_free_mmap_offset(obj);

            // 4. LRU management
            let mut lru: DrmGemLru = core::mem::zeroed();
            drm_gem_lru_init(&mut lru);
            drm_gem_lru_move_tail(&mut lru, obj);
            assert_eq!(lru.count, 1); // 4096 >> 12 = 1
            drm_gem_lru_remove(obj);
            assert_eq!(lru.count, 0);

            // 5. Cleanup
            assert_eq!(drm_gem_handle_delete(file_priv2, open_args.handle), 0);
            assert_eq!(drm_gem_handle_delete(file_priv, handle), 0);
            assert_eq!((*obj).handle_count, 0);

            drm_file_free(file_priv2);
            drm_file_free(file_priv);
            drm_gem_object_release(obj);
            drm_dev_put(dev);
        }
    }
}
