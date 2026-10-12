// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_gem.c` (batch BI).
//!
//! Graphics Execution Manager (GEM): object lifecycle (init/release/free),
//! handles, names/flink, mmap offset allocation through `drm_vma_manager`,
//! LRU management, and dma-resv locking helpers.

#![allow(non_snake_case, dead_code, unsafe_code)]

use core::ffi::c_int;
use alloc::boxed::Box;
use tk_linux_kapi::config::{EEXIST, EINVAL, ENOMEM, EOPNOTSUPP, GFP_KERNEL};
use tk_linux_kapi::list::{INIT_LIST_HEAD, list_add_tail, list_del, list_del_init, list_empty};
use tk_linux_kapi::locks::{spin_lock_raw, spin_unlock_raw};
use tk_linux_kapi::memory::{kfree, kref_init, kref_put};
use tk_linux_kapi::mutex::mutex_init;
#[cfg(not(test))]
use tk_linux_kapi::mutex::{mutex_lock, mutex_unlock};
#[cfg(test)]
unsafe fn mutex_lock<M>(_mutex: M) {}
#[cfg(test)]
unsafe fn mutex_unlock<M>(_mutex: M) {}
use tk_linux_kapi::types::{Kref, WwAcquireCtx};
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
use crate::drm_gem_types::{DrmGemLru, DrmGemObject};
use crate::drm_mode_object_upstream::{idr_alloc, idr_destroy, idr_find, idr_remove};
use crate::drm_print_types::DrmPrinter;
use crate::drm_vma_manager_upstream::{drm_vma_offset_add, drm_vma_offset_remove};
use crate::uapi_drm_types::{DrmGemClose, DrmGemFlink, DrmGemOpen};

const ENOENT: i32 = 2;

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
    let obj = kref.cast::<DrmGemObject>();
    unsafe {
        if let Some(funcs) = (*obj).funcs.as_ref() {
            if let Some(free_fn) = funcs.free {
                free_fn(obj);
                return;
            }
        }
        drm_gem_object_release(obj);
        drop(Box::from_raw(obj));
    }
}

// upstream: include/drm/drm_gem.h drm_gem_object_get()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_object_get(obj: *mut DrmGemObject) {
    if !obj.is_null() {
        unsafe {
            tk_linux_kapi::memory::kref_get(core::ptr::addr_of_mut!((*obj).refcount));
        }
    }
}

// upstream: include/drm/drm_gem.h drm_gem_object_put()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_object_put(obj: *mut DrmGemObject) {
    if !obj.is_null() {
        unsafe {
            kref_put(core::ptr::addr_of_mut!((*obj).refcount), drm_gem_object_free);
        }
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
        spin_lock_raw(core::ptr::addr_of_mut!((*file_priv).table_lock));
        let ret = idr_alloc(
            core::ptr::addr_of_mut!((*file_priv).object_idr),
            obj.cast(),
            1,
            0,
        );
        spin_unlock_raw(core::ptr::addr_of_mut!((*file_priv).table_lock));

        if ret < 0 {
            return ret;
        }

        *handlep = ret as u32;
        (*obj).handle_count += 1;
        drm_gem_object_get(obj);

        if let Some(funcs) = (*obj).funcs.as_ref() {
            if let Some(open) = funcs.open {
                open(obj, file_priv);
            }
        }

        0
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_handle_delete()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_handle_delete(file_priv: *mut DrmFile, handle: u32) -> i32 {
    if file_priv.is_null() || handle == 0 {
        return -EINVAL;
    }
    unsafe {
        spin_lock_raw(core::ptr::addr_of_mut!((*file_priv).table_lock));
        let obj = idr_remove(
            core::ptr::addr_of_mut!((*file_priv).object_idr),
            handle as i32,
        ).cast::<DrmGemObject>();
        spin_unlock_raw(core::ptr::addr_of_mut!((*file_priv).table_lock));

        if obj.is_null() {
            return -ENOENT;
        }

        if let Some(funcs) = (*obj).funcs.as_ref() {
            if let Some(close) = funcs.close {
                close(obj, file_priv);
            }
        }

        crate::drm_prime_upstream::drm_prime_remove_buf_handle(
            core::ptr::addr_of_mut!((*file_priv).prime),
            handle,
        );

        drm_gem_object_handle_put_unlocked(obj);
        0
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_object_lookup()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_object_lookup(
    file_priv: *mut DrmFile,
    handle: u32,
) -> *mut DrmGemObject {
    if file_priv.is_null() || handle == 0 {
        return core::ptr::null_mut();
    }
    unsafe {
        spin_lock_raw(core::ptr::addr_of_mut!((*file_priv).table_lock));
        let obj = idr_find(
            core::ptr::addr_of_mut!((*file_priv).object_idr),
            handle as i32,
        ).cast::<DrmGemObject>();
        if !obj.is_null() {
            drm_gem_object_get(obj);
        }
        spin_unlock_raw(core::ptr::addr_of_mut!((*file_priv).table_lock));
        obj
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_create_mmap_offset()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_create_mmap_offset(obj: *mut DrmGemObject) -> i32 {
    if obj.is_null() {
        return -EINVAL;
    }
    unsafe {
        let dev = (*obj).dev;
        let pages = ((*obj).size + 4095) / 4096;
        drm_vma_offset_add((*dev).vma_offset_manager, core::ptr::addr_of_mut!((*obj).vma_node), pages as core::ffi::c_ulong)
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_free_mmap_offset()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_free_mmap_offset(obj: *mut DrmGemObject) {
    if obj.is_null() {
        return;
    }
    unsafe {
        let dev = (*obj).dev;
        if !dev.is_null() && !(*dev).vma_offset_manager.is_null() {
            drm_vma_offset_remove((*dev).vma_offset_manager, core::ptr::addr_of_mut!((*obj).vma_node));
        }
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_close_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_close_ioctl(
    _dev: *mut DrmDevice,
    data: *mut core::ffi::c_void,
    file_priv: *mut DrmFile,
) -> i32 {
    let args = data.cast::<DrmGemClose>();
    if args.is_null() {
        return -EINVAL;
    }
    unsafe { drm_gem_handle_delete(file_priv, (*args).handle) }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_flink_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_flink_ioctl(
    dev: *mut DrmDevice,
    data: *mut core::ffi::c_void,
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
        let mut ret = 0;
        if (*obj).name == 0 {
            ret = idr_alloc(
                core::ptr::addr_of_mut!((*dev).object_name_idr),
                obj.cast(),
                1,
                0,
            );
            if ret > 0 {
                (*obj).name = ret;
                drm_gem_object_get(obj);
            }
        }
        (*args).name = (*obj).name as u32;
        mutex_unlock(core::ptr::addr_of_mut!((*dev).object_name_lock));

        drm_gem_object_put(obj);
        if ret < 0 { ret } else { 0 }
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_open_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_open_ioctl(
    dev: *mut DrmDevice,
    data: *mut core::ffi::c_void,
    file_priv: *mut DrmFile,
) -> i32 {
    let args = data.cast::<DrmGemOpen>();
    if dev.is_null() || data.is_null() || file_priv.is_null() {
        return -EINVAL;
    }
    unsafe {
        mutex_lock(core::ptr::addr_of_mut!((*dev).object_name_lock));
        let obj = idr_find(
            core::ptr::addr_of_mut!((*dev).object_name_idr),
            (*args).name as i32,
        ).cast::<DrmGemObject>();
        if !obj.is_null() {
            drm_gem_object_get(obj);
        }
        mutex_unlock(core::ptr::addr_of_mut!((*dev).object_name_lock));

        if obj.is_null() {
            return -ENOENT;
        }

        let mut handle = 0u32;
        let ret = drm_gem_handle_create(file_priv, obj, &mut handle);
        (*args).handle = handle;
        (*args).size = (*obj).size as u64;

        drm_gem_object_put(obj);
        ret
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_print_info()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_print_info(
    _p: *mut DrmPrinter,
    _indent: u32,
    _obj: *const core::ffi::c_void,
) {}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_lock_reservations()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_lock_reservations(
    objs: *mut *mut DrmGemObject,
    count: i32,
    acquire_ctx: *mut WwAcquireCtx,
) -> i32 {
    if count <= 0 {
        return 0;
    }
    unsafe {
        let mut i = 0;
        while i < count {
            let obj = *objs.add(i as usize);
            let ret = dma_resv_lock((*obj).resv, acquire_ctx);
            if ret != 0 {
                // unwind
                let mut j = i - 1;
                while j >= 0 {
                    let prev_obj = *objs.add(j as usize);
                    dma_resv_unlock((*prev_obj).resv);
                    j -= 1;
                }
                return ret;
            }
            i += 1;
        }
        0
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_unlock_reservations()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_unlock_reservations(
    objs: *mut *mut DrmGemObject,
    count: i32,
    _acquire_ctx: *mut WwAcquireCtx,
) {
    if count <= 0 {
        return;
    }
    unsafe {
        for i in 0..count as usize {
            let obj = *objs.add(i);
            dma_resv_unlock((*obj).resv);
        }
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_lru_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_lru_init(lru: *mut DrmGemLru) {
    if lru.is_null() {
        return;
    }
    unsafe {
        (*lru).count = 0;
        INIT_LIST_HEAD(core::ptr::addr_of_mut!((*lru).list));
    }
}

// upstream: drivers/gpu/drm/drm_gem.c drm_gem_lru_remove()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_lru_remove(obj: *mut DrmGemObject) {
    if obj.is_null() {
        return;
    }
    unsafe {
        let dev = (*obj).dev;
        mutex_lock(core::ptr::addr_of_mut!((*dev).gem_lru_mutex));
        if !(*obj).lru.is_null() {
            (*(*obj).lru).count -= 1;
            list_del_init(core::ptr::addr_of_mut!((*obj).lru_node));
            (*obj).lru = core::ptr::null_mut();
        }
        mutex_unlock(core::ptr::addr_of_mut!((*dev).gem_lru_mutex));
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
        if !(*obj).lru.is_null() {
            (*(*obj).lru).count -= 1;
            list_del(core::ptr::addr_of_mut!((*obj).lru_node));
        }
        (*obj).lru = lru;
        (*lru).count += 1;
        list_add_tail(
            core::ptr::addr_of_mut!((*obj).lru_node),
            core::ptr::addr_of_mut!((*lru).list),
        );
        mutex_unlock(core::ptr::addr_of_mut!((*dev).gem_lru_mutex));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::ffi::c_void;
    use crate::drm_drv_types::DrmDriver;
    use crate::drm_drv_upstream::{drm_dev_alloc, drm_dev_put};
    use crate::drm_file_upstream::{drm_file_alloc, drm_file_free};
    use crate::drm_vma_manager_upstream::drm_vma_node_has_offset;

    #[test]
    fn test_gem_handles_flink_and_mmap() {
        unsafe {
            let mut driver: DrmDriver = core::mem::zeroed();
            let dev = drm_dev_alloc(&driver, core::ptr::null_mut());
            assert!(!dev.is_null());

            let file1 = drm_file_alloc((*dev).primary);
            assert!(!file1.is_null());

            let mut obj: DrmGemObject = core::mem::zeroed();
            drm_gem_private_object_init(dev, &mut obj, 8192);
            assert_eq!(obj.size, 8192);

            // 1. Create handle
            let mut handle1: u32 = 0;
            assert_eq!(drm_gem_handle_create(file1, &mut obj, &mut handle1), 0);
            assert!(handle1 > 0);
            assert_eq!(obj.handle_count, 1);

            // 2. Lookup handle
            let looked_up = drm_gem_object_lookup(file1, handle1);
            assert_eq!(looked_up, &mut obj as *mut DrmGemObject);
            // drm_gem_object_lookup gets a ref; drop it
            drm_gem_object_put(looked_up);

            // 3. Flink (name) creation
            let mut flink_args = DrmGemFlink {
                handle: handle1,
                name: 0,
            };
            assert_eq!(drm_gem_flink_ioctl(dev, &mut flink_args as *mut _ as *mut c_void, file1), 0);
            assert!(flink_args.name > 0);
            assert_eq!(obj.name, flink_args.name as i32);

            // Flink again gives same name
            let mut flink_args2 = DrmGemFlink {
                handle: handle1,
                name: 0,
            };
            assert_eq!(drm_gem_flink_ioctl(dev, &mut flink_args2 as *mut _ as *mut c_void, file1), 0);
            assert_eq!(flink_args2.name, flink_args.name);

            // 4. Open name on file 2
            let file2 = drm_file_alloc((*dev).primary);
            let mut open_args = DrmGemOpen {
                name: flink_args.name,
                handle: 0,
                size: 0,
            };
            assert_eq!(drm_gem_open_ioctl(dev, &mut open_args as *mut _ as *mut c_void, file2), 0);
            assert!(open_args.handle > 0);
            assert_eq!(open_args.size, 8192);
            assert_eq!(obj.handle_count, 2);

            // 5. MMAP offset
            assert!(!drm_vma_node_has_offset(&obj.vma_node));
            assert_eq!(drm_gem_create_mmap_offset(&mut obj), 0);
            assert!(drm_vma_node_has_offset(&obj.vma_node));
            drm_gem_free_mmap_offset(&mut obj);
            assert!(!drm_vma_node_has_offset(&obj.vma_node));

            // 6. Delete handles
            assert_eq!(drm_gem_handle_delete(file2, open_args.handle), 0);
            assert_eq!(obj.handle_count, 1);
            assert_eq!(drm_gem_handle_delete(file1, handle1), 0);
            assert_eq!(obj.handle_count, 0);

            // Lookup after deletion fails
            assert!(drm_gem_object_lookup(file1, handle1).is_null());

            drm_file_free(file2);
            drm_file_free(file1);
            drm_dev_put(dev);
        }
    }
}
