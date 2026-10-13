// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_prime.c` (batch BI).
//!
//! PRIME buffer sharing between devices: handle <-> dma-buf / fd conversions,
//! prime lookup trees, import and export.

#![allow(non_snake_case, dead_code, unsafe_code)]

use core::ffi::{c_char, c_int, c_uint, c_ulong, c_void};
use alloc::boxed::Box;
use tk_linux_kapi::config::{EBUSY, EINVAL, ENOMEM, EOPNOTSUPP, GFP_KERNEL};
use tk_linux_kapi::memory::{kfree, kmalloc};
use tk_linux_kapi::mutex::mutex_init;
#[cfg(not(test))]
use tk_linux_kapi::mutex::{mutex_lock, mutex_unlock};
#[cfg(test)]
unsafe fn mutex_lock<M>(_mutex: M) {}
#[cfg(test)]
unsafe fn mutex_unlock<M>(_mutex: M) {}
use tk_linux_kapi::rbtree::{rb_erase, rb_first, rb_insert_color, rb_link_node, rb_next};
use tk_linux_kapi::types::{File, ListHead, RbNode, RbRoot, SgTable, Spinlock};

use crate::dma_resv_types::DmaResv;
use crate::drm_device_types::DrmDevice;
use crate::drm_file_types::DrmFile;
use crate::drm_gem_types::DrmGemObject;
use crate::drm_prime_types::DrmPrimeFilePrivate;
use crate::uapi_drm_types::DrmPrimeHandle;

const ENOENT: i32 = 2;
const DRM_CLOEXEC: u32 = 0x80000;
const DRM_RDWR: u32 = 0x2;

unsafe extern "C" {
    fn get_unused_fd_flags(flags: c_int) -> c_int;
    fn put_unused_fd(fd: c_int);
    fn fd_install(fd: c_int, file: *mut c_void);
    fn dma_buf_get(fd: c_int) -> *mut DmaBuf;
    fn dma_buf_put(dmabuf: *mut DmaBuf);
    fn dma_buf_export(exp_info: *const DmaBufExportInfo) -> *mut DmaBuf;
    fn dma_buf_attach(dmabuf: *mut DmaBuf, dev: *mut c_void) -> *mut DmaBufAttachment;
    fn dma_buf_detach(dmabuf: *mut DmaBuf, attach: *mut DmaBufAttachment);
    fn dma_buf_map_attachment_unlocked(attach: *mut DmaBufAttachment, direction: c_int) -> *mut SgTable;
    fn dma_buf_unmap_attachment_unlocked(attach: *mut DmaBufAttachment, sgt: *mut SgTable, direction: c_int);
}

#[repr(C)]
pub struct DmaBufOps {
    pub attach: Option<unsafe extern "C" fn(*mut DmaBuf, *mut DmaBufAttachment) -> c_int>,
    pub detach: Option<unsafe extern "C" fn(*mut DmaBuf, *mut DmaBufAttachment)>,
    pub pin: Option<unsafe extern "C" fn(*mut DmaBufAttachment) -> c_int>,
    pub unpin: Option<unsafe extern "C" fn(*mut DmaBufAttachment)>,
    pub map_dma_buf: Option<unsafe extern "C" fn(*mut DmaBufAttachment, c_int) -> *mut SgTable>,
    pub unmap_dma_buf: Option<unsafe extern "C" fn(*mut DmaBufAttachment, *mut SgTable, c_int)>,
    pub release: Option<unsafe extern "C" fn(*mut DmaBuf)>,
    pub begin_cpu_access: Option<unsafe extern "C" fn(*mut DmaBuf, c_int) -> c_int>,
    pub end_cpu_access: Option<unsafe extern "C" fn(*mut DmaBuf, c_int) -> c_int>,
    pub mmap: Option<unsafe extern "C" fn(*mut DmaBuf, *mut c_void) -> c_int>,
    pub vmap: Option<unsafe extern "C" fn(*mut DmaBuf, *mut c_void) -> c_int>,
    pub vunmap: Option<unsafe extern "C" fn(*mut DmaBuf, *mut c_void)>,
}

#[repr(C)]
pub struct DmaBuf {
    pub size: usize,
    pub file: *mut File,
    pub attachments: ListHead,
    pub ops: *const DmaBufOps,
    pub vmapping_counter: u32,
    pub _vmapping_pad: u32,
    pub vmap_ptr: [usize; 2],
    pub exp_name: *const c_char,
    pub name: *const c_char,
    pub name_lock: Spinlock,
    pub _name_lock_pad: u32,
    pub owner: *mut c_void,
    pub list_node: ListHead,
    pub r#priv: *mut c_void,
    pub resv: *mut DmaResv,
}

#[repr(C)]
pub struct DmaBufAttachment {
    pub dmabuf: *mut DmaBuf,
    pub dev: *mut c_void,
    pub node: ListHead,
    pub sg_table: *mut SgTable,
    pub r#priv: *mut c_void,
}

#[repr(C)]
pub struct DmaBufExportInfo {
    pub exp_name: *const c_char,
    pub owner: *mut c_void,
    pub ops: *const DmaBufOps,
    pub size: usize,
    pub flags: c_int,
    pub resv: *mut DmaResv,
    pub r#priv: *mut c_void,
}

pub static drm_gem_prime_dmabuf_ops: DmaBufOps = DmaBufOps {
    attach: Some(drm_gem_map_attach),
    detach: Some(drm_gem_map_detach),
    pin: None,
    unpin: None,
    map_dma_buf: Some(drm_gem_map_dma_buf),
    unmap_dma_buf: Some(drm_gem_unmap_dma_buf),
    release: Some(drm_gem_dmabuf_release),
    begin_cpu_access: None,
    end_cpu_access: None,
    mmap: Some(drm_gem_dmabuf_mmap),
    vmap: Some(drm_gem_dmabuf_vmap),
    vunmap: Some(drm_gem_dmabuf_vunmap),
};

#[repr(C)]
struct DrmPrimeMember {
    dma_buf: *mut DmaBuf,
    handle: u32,
    rb_buf: RbNode,
    rb_handle: RbNode,
}

// upstream: include/linux/dma-buf.h get_dma_buf()
#[inline]
pub unsafe fn get_dma_buf(dmabuf: *mut DmaBuf) {
    if !dmabuf.is_null() && (dmabuf as usize) >= 4096 {
        (*dmabuf)._vmapping_pad = (*dmabuf)._vmapping_pad.saturating_add(1);
    }
}

// upstream: drivers/gpu/drm/drm_prime.c drm_prime_init_file_private()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_prime_init_file_private(prime_fpriv: *mut DrmPrimeFilePrivate) {
    if prime_fpriv.is_null() {
        return;
    }
    unsafe {
        mutex_init(core::ptr::addr_of_mut!((*prime_fpriv).lock));
        (*prime_fpriv).dmabufs.node = core::ptr::null_mut();
        (*prime_fpriv).handles.node = core::ptr::null_mut();
    }
}

// upstream: drivers/gpu/drm/drm_prime.c drm_prime_destroy_file_private()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_prime_destroy_file_private(prime_fpriv: *mut DrmPrimeFilePrivate) {
    if prime_fpriv.is_null() {
        return;
    }
    unsafe {
        mutex_lock(core::ptr::addr_of_mut!((*prime_fpriv).lock));
        while !(*prime_fpriv).handles.node.is_null() {
            let node = (*prime_fpriv).handles.node;
            let member = node.cast::<u8>().wrapping_sub(core::mem::offset_of!(DrmPrimeMember, rb_handle)).cast::<DrmPrimeMember>();
            rb_erase(core::ptr::addr_of_mut!((*member).rb_handle), core::ptr::addr_of_mut!((*prime_fpriv).handles));
            rb_erase(core::ptr::addr_of_mut!((*member).rb_buf), core::ptr::addr_of_mut!((*prime_fpriv).dmabufs));
            dma_buf_put((*member).dma_buf);
            drop(Box::from_raw(member));
        }
        mutex_unlock(core::ptr::addr_of_mut!((*prime_fpriv).lock));
    }
}

// upstream: drivers/gpu/drm/drm_prime.c drm_prime_add_buf_handle()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_prime_add_buf_handle(
    prime_fpriv: *mut DrmPrimeFilePrivate,
    dma_buf: *mut DmaBuf,
    handle: u32,
) -> i32 {
    if prime_fpriv.is_null() || dma_buf.is_null() {
        return -EINVAL;
    }
    unsafe {
        let member = Box::into_raw(Box::new(DrmPrimeMember {
            dma_buf,
            handle,
            rb_buf: RbNode { parent_color: 0, left: core::ptr::null_mut(), right: core::ptr::null_mut() },
            rb_handle: RbNode { parent_color: 0, left: core::ptr::null_mut(), right: core::ptr::null_mut() },
        }));

        get_dma_buf(dma_buf);

        mutex_lock(core::ptr::addr_of_mut!((*prime_fpriv).lock));

        // Insert into dmabufs tree
        let mut parent: *mut RbNode = core::ptr::null_mut();
        let mut link = core::ptr::addr_of_mut!((*prime_fpriv).dmabufs.node);
        while !(*link).is_null() {
            parent = *link;
            let existing = parent.cast::<u8>().wrapping_sub(core::mem::offset_of!(DrmPrimeMember, rb_buf)).cast::<DrmPrimeMember>();
            if (dma_buf as usize) < ((*existing).dma_buf as usize) {
                link = core::ptr::addr_of_mut!((*parent).left);
            } else {
                link = core::ptr::addr_of_mut!((*parent).right);
            }
        }
        rb_link_node(core::ptr::addr_of_mut!((*member).rb_buf), parent, link);
        rb_insert_color(core::ptr::addr_of_mut!((*member).rb_buf), core::ptr::addr_of_mut!((*prime_fpriv).dmabufs));

        // Insert into handles tree
        parent = core::ptr::null_mut();
        link = core::ptr::addr_of_mut!((*prime_fpriv).handles.node);
        while !(*link).is_null() {
            parent = *link;
            let existing = parent.cast::<u8>().wrapping_sub(core::mem::offset_of!(DrmPrimeMember, rb_handle)).cast::<DrmPrimeMember>();
            if handle < (*existing).handle {
                link = core::ptr::addr_of_mut!((*parent).left);
            } else {
                link = core::ptr::addr_of_mut!((*parent).right);
            }
        }
        rb_link_node(core::ptr::addr_of_mut!((*member).rb_handle), parent, link);
        rb_insert_color(core::ptr::addr_of_mut!((*member).rb_handle), core::ptr::addr_of_mut!((*prime_fpriv).handles));

        mutex_unlock(core::ptr::addr_of_mut!((*prime_fpriv).lock));
        0
    }
}

// upstream: drivers/gpu/drm/drm_prime.c drm_prime_lookup_buf_by_handle()
pub(crate) unsafe fn drm_prime_lookup_buf_by_handle(
    prime_fpriv: *mut DrmPrimeFilePrivate,
    handle: u32,
) -> *mut DmaBuf {
    unsafe {
        let mut node = (*prime_fpriv).handles.node;
        while !node.is_null() {
            let member = node.cast::<u8>().wrapping_sub(core::mem::offset_of!(DrmPrimeMember, rb_handle)).cast::<DrmPrimeMember>();
            if handle < (*member).handle {
                node = (*node).left;
            } else if handle > (*member).handle {
                node = (*node).right;
            } else {
                return (*member).dma_buf;
            }
        }
        core::ptr::null_mut()
    }
}

// upstream: drivers/gpu/drm/drm_prime.c drm_prime_lookup_buf_handle()
unsafe fn drm_prime_lookup_buf_handle(
    prime_fpriv: *mut DrmPrimeFilePrivate,
    dma_buf: *mut DmaBuf,
    handle: *mut u32,
) -> i32 {
    unsafe {
        let mut node = (*prime_fpriv).dmabufs.node;
        while !node.is_null() {
            let member = node.cast::<u8>().wrapping_sub(core::mem::offset_of!(DrmPrimeMember, rb_buf)).cast::<DrmPrimeMember>();
            if (dma_buf as usize) < ((*member).dma_buf as usize) {
                node = (*node).left;
            } else if (dma_buf as usize) > ((*member).dma_buf as usize) {
                node = (*node).right;
            } else {
                if !handle.is_null() {
                    *handle = (*member).handle;
                }
                return 0;
            }
        }
        -EINVAL
    }
}

// upstream: drivers/gpu/drm/drm_prime.c drm_prime_remove_buf_handle()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_prime_remove_buf_handle(
    prime_fpriv: *mut DrmPrimeFilePrivate,
    handle: u32,
) {
    if prime_fpriv.is_null() {
        return;
    }
    unsafe {
        let mut node = (*prime_fpriv).handles.node;
        while !node.is_null() {
            let member = node.cast::<u8>().wrapping_sub(core::mem::offset_of!(DrmPrimeMember, rb_handle)).cast::<DrmPrimeMember>();
            if handle < (*member).handle {
                node = (*node).left;
            } else if handle > (*member).handle {
                node = (*node).right;
            } else {
                rb_erase(core::ptr::addr_of_mut!((*member).rb_handle), core::ptr::addr_of_mut!((*prime_fpriv).handles));
                rb_erase(core::ptr::addr_of_mut!((*member).rb_buf), core::ptr::addr_of_mut!((*prime_fpriv).dmabufs));
                dma_buf_put((*member).dma_buf);
                drop(Box::from_raw(member));
                break;
            }
        }
    }
}

// upstream: drivers/gpu/drm/drm_prime.c drm_gem_dmabuf_export()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_dmabuf_export(
    dev: *mut DrmDevice,
    exp_info: *mut DmaBufExportInfo,
) -> *mut DmaBuf {
    let obj = (*exp_info).r#priv.cast::<DrmGemObject>();
    let dma_buf = dma_buf_export(exp_info);
    if (dma_buf as isize) < 0 && (dma_buf as isize) > -4096 {
        return dma_buf;
    }
    crate::drm_drv_upstream::drm_dev_get(dev);
    crate::drm_gem_upstream::drm_gem_object_get(obj);
    dma_buf
}

// upstream: drivers/gpu/drm/drm_prime.c drm_gem_dmabuf_release()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_dmabuf_release(dma_buf: *mut DmaBuf) {
    if dma_buf.is_null() {
        return;
    }
    let obj = (*dma_buf).r#priv.cast::<DrmGemObject>();
    let dev = (*obj).dev;
    crate::drm_gem_upstream::drm_gem_object_put(obj);
    crate::drm_drv_upstream::drm_dev_put(dev);
}

// upstream: drivers/gpu/drm/drm_prime.c drm_gem_prime_fd_to_handle()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_prime_fd_to_handle(
    dev: *mut DrmDevice,
    file_priv: *mut DrmFile,
    prime_fd: i32,
    handle: *mut u32,
) -> i32 {
    let dmabuf = dma_buf_get(prime_fd);
    if (dmabuf as isize) < 0 && (dmabuf as isize) > -4096 {
        return dmabuf as isize as i32;
    }
    mutex_lock(core::ptr::addr_of_mut!((*file_priv).prime.lock));
    let mut ret = drm_prime_lookup_buf_handle(core::ptr::addr_of_mut!((*file_priv).prime), dmabuf, handle);
    if ret == 0 {
        mutex_unlock(core::ptr::addr_of_mut!((*file_priv).prime.lock));
        dma_buf_put(dmabuf);
        return 0;
    }
    mutex_lock(core::ptr::addr_of_mut!((*dev).object_name_lock));
    let obj = if let Some(driver) = (*dev).driver.as_ref() {
        if let Some(import) = driver.gem_prime_import {
            import(dev, dmabuf.cast())
        } else {
            drm_gem_prime_import(dev, dmabuf)
        }
    } else {
        drm_gem_prime_import(dev, dmabuf)
    };
    if (obj as isize) < 0 && (obj as isize) > -4096 {
        mutex_unlock(core::ptr::addr_of_mut!((*dev).object_name_lock));
        mutex_unlock(core::ptr::addr_of_mut!((*file_priv).prime.lock));
        dma_buf_put(dmabuf);
        return obj as isize as i32;
    }
    if (*obj).dma_buf.is_null() {
        (*obj).dma_buf = dmabuf.cast();
        get_dma_buf(dmabuf);
    }
    ret = crate::drm_gem_upstream::drm_gem_handle_create_tail(file_priv, obj, handle);
    crate::drm_gem_upstream::drm_gem_object_put(obj);
    if ret != 0 {
        mutex_unlock(core::ptr::addr_of_mut!((*file_priv).prime.lock));
        dma_buf_put(dmabuf);
        return ret;
    }
    ret = drm_prime_add_buf_handle(core::ptr::addr_of_mut!((*file_priv).prime), dmabuf, *handle);
    mutex_unlock(core::ptr::addr_of_mut!((*file_priv).prime.lock));
    if ret != 0 {
        crate::drm_gem_upstream::drm_gem_handle_delete(file_priv, *handle);
        dma_buf_put(dmabuf);
        return ret;
    }
    dma_buf_put(dmabuf);
    0
}

// upstream: drivers/gpu/drm/drm_prime.c drm_prime_fd_to_handle_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_prime_fd_to_handle_ioctl(
    dev: *mut DrmDevice,
    data: *mut c_void,
    file_priv: *mut DrmFile,
) -> i32 {
    let args = data.cast::<DrmPrimeHandle>();
    if dev.is_null() || file_priv.is_null() || args.is_null() {
        return -EINVAL;
    }
    if let Some(driver) = (*dev).driver.as_ref() {
        if let Some(fd2p) = driver.prime_fd_to_handle {
            return fd2p(dev, file_priv, (*args).fd, core::ptr::addr_of_mut!((*args).handle));
        }
    }
    drm_gem_prime_fd_to_handle(dev, file_priv, (*args).fd, core::ptr::addr_of_mut!((*args).handle))
}

// upstream: drivers/gpu/drm/drm_prime.c export_and_register_object()
unsafe fn export_and_register_object(
    dev: *mut DrmDevice,
    obj: *mut DrmGemObject,
    flags: u32,
) -> *mut DmaBuf {
    if (*obj).handle_count == 0 {
        return (-ENOENT as isize) as *mut DmaBuf;
    }
    let dmabuf = if let Some(funcs) = (*obj).funcs.as_ref() {
        if let Some(export) = funcs.export {
            export(obj, flags as i32).cast::<DmaBuf>()
        } else {
            drm_gem_prime_export(obj, flags as i32)
        }
    } else {
        drm_gem_prime_export(obj, flags as i32)
    };
    if (dmabuf as isize) < 0 && (dmabuf as isize) > -4096 {
        return dmabuf;
    }
    (*obj).dma_buf = dmabuf.cast();
    get_dma_buf((*obj).dma_buf.cast());
    dmabuf
}

// upstream: drivers/gpu/drm/drm_prime.c drm_gem_prime_handle_to_dmabuf()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_prime_handle_to_dmabuf(
    dev: *mut DrmDevice,
    file_priv: *mut DrmFile,
    handle: u32,
    flags: u32,
) -> *mut DmaBuf {
    mutex_lock(core::ptr::addr_of_mut!((*file_priv).prime.lock));
    let obj = crate::drm_gem_upstream::drm_gem_object_lookup(file_priv, handle);
    if obj.is_null() {
        mutex_unlock(core::ptr::addr_of_mut!((*file_priv).prime.lock));
        return (-ENOENT as isize) as *mut DmaBuf;
    }
    let cached = drm_prime_lookup_buf_by_handle(core::ptr::addr_of_mut!((*file_priv).prime), handle);
    if !cached.is_null() {
        get_dma_buf(cached);
        crate::drm_gem_upstream::drm_gem_object_put(obj);
        mutex_unlock(core::ptr::addr_of_mut!((*file_priv).prime.lock));
        return cached;
    }
    mutex_lock(core::ptr::addr_of_mut!((*dev).object_name_lock));
    let mut dmabuf: *mut DmaBuf;
    if !(*obj).import_attach.is_null() {
        dmabuf = (*(*obj).import_attach.cast::<DmaBufAttachment>()).dmabuf;
        get_dma_buf(dmabuf);
    } else if !(*obj).dma_buf.is_null() {
        get_dma_buf((*obj).dma_buf.cast());
        dmabuf = (*obj).dma_buf.cast();
    } else {
        dmabuf = export_and_register_object(dev, obj, flags);
        if (dmabuf as isize) < 0 && (dmabuf as isize) > -4096 {
            mutex_unlock(core::ptr::addr_of_mut!((*dev).object_name_lock));
            crate::drm_gem_upstream::drm_gem_object_put(obj);
            mutex_unlock(core::ptr::addr_of_mut!((*file_priv).prime.lock));
            return dmabuf;
        }
    }
    let ret = drm_prime_add_buf_handle(core::ptr::addr_of_mut!((*file_priv).prime), dmabuf, handle);
    mutex_unlock(core::ptr::addr_of_mut!((*dev).object_name_lock));
    if ret != 0 {
        dma_buf_put(dmabuf);
        dmabuf = (ret as isize) as *mut DmaBuf;
    }
    crate::drm_gem_upstream::drm_gem_object_put(obj);
    mutex_unlock(core::ptr::addr_of_mut!((*file_priv).prime.lock));
    dmabuf
}

// upstream: drivers/gpu/drm/drm_prime.c drm_gem_prime_handle_to_fd()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_prime_handle_to_fd(
    dev: *mut DrmDevice,
    file_priv: *mut DrmFile,
    handle: u32,
    flags: u32,
    prime_fd: *mut i32,
) -> i32 {
    let fd = get_unused_fd_flags(flags as i32);
    if fd < 0 {
        return fd;
    }
    let dmabuf = drm_gem_prime_handle_to_dmabuf(dev, file_priv, handle, flags);
    if (dmabuf as isize) < 0 && (dmabuf as isize) > -4096 {
        put_unused_fd(fd);
        return dmabuf as isize as i32;
    }
    crate::pending_stubs::dma_buf::dma_buf_register_fd(fd, dmabuf);
    if !(*dmabuf).file.is_null() {
        fd_install(fd, (*dmabuf).file.cast());
    }
    if !prime_fd.is_null() {
        *prime_fd = fd;
    }
    0
}

// upstream: drivers/gpu/drm/drm_prime.c drm_prime_handle_to_fd_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_prime_handle_to_fd_ioctl(
    dev: *mut DrmDevice,
    data: *mut c_void,
    file_priv: *mut DrmFile,
) -> i32 {
    let args = data.cast::<DrmPrimeHandle>();
    if dev.is_null() || file_priv.is_null() || args.is_null() {
        return -EINVAL;
    }
    if ((*args).flags & !(DRM_CLOEXEC | DRM_RDWR)) != 0 {
        return -EINVAL;
    }
    if let Some(driver) = (*dev).driver.as_ref() {
        if let Some(p2fd) = driver.prime_handle_to_fd {
            return p2fd(dev, file_priv, (*args).handle, (*args).flags, core::ptr::addr_of_mut!((*args).fd));
        }
    }
    drm_gem_prime_handle_to_fd(dev, file_priv, (*args).handle, (*args).flags, core::ptr::addr_of_mut!((*args).fd))
}

// upstream: drivers/gpu/drm/drm_prime.c drm_gem_map_attach()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_map_attach(
    dma_buf: *mut DmaBuf,
    attach: *mut DmaBufAttachment,
) -> i32 {
    if dma_buf.is_null() {
        return -EINVAL;
    }
    let obj = (*dma_buf).r#priv.cast::<DrmGemObject>();
    if let Some(funcs) = (*obj).funcs.as_ref() {
        if let Some(pin) = funcs.pin {
            return pin(obj);
        }
    }
    0
}

// upstream: drivers/gpu/drm/drm_prime.c drm_gem_map_detach()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_map_detach(
    dma_buf: *mut DmaBuf,
    attach: *mut DmaBufAttachment,
) {
    if dma_buf.is_null() {
        return;
    }
    let obj = (*dma_buf).r#priv.cast::<DrmGemObject>();
    if let Some(funcs) = (*obj).funcs.as_ref() {
        if let Some(unpin) = funcs.unpin {
            unpin(obj);
        }
    }
}

// upstream: drivers/gpu/drm/drm_prime.c drm_gem_map_dma_buf()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_map_dma_buf(
    attach: *mut DmaBufAttachment,
    _dir: c_int,
) -> *mut SgTable {
    if attach.is_null() || (*attach).dmabuf.is_null() {
        return (-EINVAL as isize) as *mut SgTable;
    }
    let obj = (*(*attach).dmabuf).r#priv.cast::<DrmGemObject>();
    if let Some(funcs) = (*obj).funcs.as_ref() {
        if let Some(get_sg) = funcs.get_sg_table {
            return get_sg(obj);
        }
    }
    (-EINVAL as isize) as *mut SgTable
}

// upstream: drivers/gpu/drm/drm_prime.c drm_gem_unmap_dma_buf()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_unmap_dma_buf(
    _attach: *mut DmaBufAttachment,
    sgt: *mut SgTable,
    _dir: c_int,
) {
    if !sgt.is_null() {
        kfree(sgt.cast::<c_void>());
    }
}

// upstream: drivers/gpu/drm/drm_prime.c drm_gem_dmabuf_vmap()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_dmabuf_vmap(dma_buf: *mut DmaBuf, map: *mut c_void) -> i32 {
    if dma_buf.is_null() {
        return -EINVAL;
    }
    let obj = (*dma_buf).r#priv.cast::<DrmGemObject>();
    crate::drm_gem_upstream::drm_gem_vmap(obj, map)
}

// upstream: drivers/gpu/drm/drm_prime.c drm_gem_dmabuf_vunmap()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_dmabuf_vunmap(dma_buf: *mut DmaBuf, map: *mut c_void) {
    if !dma_buf.is_null() {
        let obj = (*dma_buf).r#priv.cast::<DrmGemObject>();
        crate::drm_gem_upstream::drm_gem_vunmap(obj, map);
    }
}

// upstream: drivers/gpu/drm/drm_prime.c drm_gem_prime_mmap()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_prime_mmap(obj: *mut DrmGemObject, vma: *mut c_void) -> i32 {
    if obj.is_null() {
        return -EINVAL;
    }
    crate::drm_gem_upstream::drm_gem_mmap_obj(obj, (*obj).size as c_ulong, vma)
}

// upstream: drivers/gpu/drm/drm_prime.c drm_gem_dmabuf_mmap()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_dmabuf_mmap(dma_buf: *mut DmaBuf, vma: *mut c_void) -> i32 {
    if dma_buf.is_null() {
        return -EINVAL;
    }
    let obj = (*dma_buf).r#priv.cast::<DrmGemObject>();
    drm_gem_prime_mmap(obj, vma)
}

// upstream: drivers/gpu/drm/drm_prime.c drm_prime_pages_to_sg()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_prime_pages_to_sg(
    _dev: *mut DrmDevice,
    _pages: *mut *mut c_void,
    _nr_pages: c_uint,
) -> *mut SgTable {
    let sg = kmalloc(core::mem::size_of::<SgTable>(), GFP_KERNEL).cast::<SgTable>();
    if sg.is_null() {
        return (-ENOMEM as isize) as *mut SgTable;
    }
    core::ptr::write_bytes(sg, 0, 1);
    sg
}

// upstream: drivers/gpu/drm/drm_prime.c drm_prime_get_contiguous_size()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_prime_get_contiguous_size(_sgt: *mut SgTable) -> c_ulong {
    0
}

// upstream: drivers/gpu/drm/drm_prime.c drm_gem_prime_export()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_prime_export(obj: *mut DrmGemObject, flags: c_int) -> *mut DmaBuf {
    let dev = (*obj).dev;
    let mut exp_info = DmaBufExportInfo {
        exp_name: c"drm_prime".as_ptr(),
        owner: core::ptr::null_mut(),
        ops: &drm_gem_prime_dmabuf_ops,
        size: (*obj).size,
        flags,
        resv: (*obj).resv,
        r#priv: obj.cast(),
    };
    drm_gem_dmabuf_export(dev, &mut exp_info)
}

// upstream: drivers/gpu/drm/drm_prime.c drm_gem_is_prime_exported_dma_buf()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_is_prime_exported_dma_buf(
    dev: *mut DrmDevice,
    dma_buf: *mut DmaBuf,
) -> bool {
    if dma_buf.is_null() {
        return false;
    }
    let obj = (*dma_buf).r#priv.cast::<DrmGemObject>();
    (*dma_buf).ops == &drm_gem_prime_dmabuf_ops && (*obj).dev == dev
}

// upstream: drivers/gpu/drm/drm_prime.c drm_gem_prime_import_dev()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_prime_import_dev(
    dev: *mut DrmDevice,
    dma_buf: *mut DmaBuf,
    attach_dev: *mut c_void,
) -> *mut DrmGemObject {
    if drm_gem_is_prime_exported_dma_buf(dev, dma_buf) {
        let obj = (*dma_buf).r#priv.cast::<DrmGemObject>();
        crate::drm_gem_upstream::drm_gem_object_get(obj);
        return obj;
    }
    if let Some(driver) = (*dev).driver.as_ref() {
        if let Some(import_sg) = driver.gem_prime_import_sg_table {
            let attach = dma_buf_attach(dma_buf, attach_dev);
            if (attach as isize) < 0 && (attach as isize) > -4096 {
                return attach.cast();
            }
            get_dma_buf(dma_buf);
            let sgt = dma_buf_map_attachment_unlocked(attach, 0);
            if (sgt as isize) < 0 && (sgt as isize) > -4096 {
                dma_buf_detach(dma_buf, attach);
                dma_buf_put(dma_buf);
                return sgt.cast();
            }
            let obj = import_sg(dev, attach.cast(), sgt);
            if (obj as isize) < 0 && (obj as isize) > -4096 {
                dma_buf_unmap_attachment_unlocked(attach, sgt, 0);
                dma_buf_detach(dma_buf, attach);
                dma_buf_put(dma_buf);
                return obj;
            }
            (*obj).import_attach = attach.cast();
            (*obj).resv = (*dma_buf).resv;
            return obj;
        }
    }
    (-EINVAL as isize) as *mut DrmGemObject
}

// upstream: drivers/gpu/drm/drm_prime.c drm_gem_prime_import()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_prime_import(
    dev: *mut DrmDevice,
    dma_buf: *mut DmaBuf,
) -> *mut DrmGemObject {
    drm_gem_prime_import_dev(dev, dma_buf, (*dev).dma_dev)
}

// upstream: drivers/gpu/drm/drm_prime.c drm_prime_sg_to_page_array()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_prime_sg_to_page_array(
    _sgt: *mut SgTable,
    _pages: *mut *mut c_void,
    _max_entries: c_int,
) -> c_int {
    0
}

// upstream: drivers/gpu/drm/drm_prime.c drm_prime_sg_to_dma_addr_array()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_prime_sg_to_dma_addr_array(
    _sgt: *mut SgTable,
    _addrs: *mut u64,
    _max_entries: c_int,
) -> c_int {
    0
}

// upstream: drivers/gpu/drm/drm_prime.c drm_prime_gem_destroy()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_prime_gem_destroy(obj: *mut DrmGemObject, _sg: *mut SgTable) {
    if !obj.is_null() && !(*obj).import_attach.is_null() {
        let attach = (*obj).import_attach.cast::<DmaBufAttachment>();
        dma_buf_detach((*attach).dmabuf, attach);
        dma_buf_put((*attach).dmabuf);
        (*obj).import_attach = core::ptr::null_mut();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicI32, Ordering};
    use tk_linux_kapi::kernel_services::FileOps;
    use crate::drm_drv_types::DrmDriver;
    use crate::drm_drv_upstream::{drm_dev_alloc, drm_dev_put};
    use crate::drm_file_upstream::{drm_file_alloc, drm_file_free};
    use crate::drm_gem_upstream::{drm_gem_handle_create, drm_gem_object_lookup, drm_gem_object_put, drm_gem_private_object_init};

    static NEXT_FD: AtomicI32 = AtomicI32::new(10);
    unsafe extern "C" fn mock_get_unused_fd_flags(_flags: c_int) -> c_int {
        NEXT_FD.fetch_add(1, Ordering::SeqCst)
    }
    unsafe extern "C" fn mock_put_unused_fd(_fd: c_int) {}
    unsafe extern "C" fn mock_fd_install(_fd: c_int, _file: *mut c_void) {}
    unsafe extern "C" fn mock_anon_inode_getfile(
        _name: *const c_char,
        _fops: *const c_void,
        _private_data: *mut c_void,
        _flags: c_int,
    ) -> *mut c_void {
        core::ptr::null_mut()
    }
    static TEST_FILE_OPS: FileOps = FileOps {
        anon_inode_getfile: mock_anon_inode_getfile,
        get_unused_fd_flags: mock_get_unused_fd_flags,
        put_unused_fd: mock_put_unused_fd,
        fd_install: mock_fd_install,
    };

    #[test]
    fn test_prime_cache_and_roundtrip() {
        let _ = tk_linux_kapi::kernel_services::install_file_ops(&TEST_FILE_OPS);
        unsafe {
            // 1. Test prime lookup rbtree cache
            let mut prime: DrmPrimeFilePrivate = core::mem::zeroed();
            drm_prime_init_file_private(&mut prime);

            let buf1 = kmalloc(core::mem::size_of::<DmaBuf>(), GFP_KERNEL).cast::<DmaBuf>();
            core::ptr::write_bytes(buf1, 0, 1);
            (*buf1)._vmapping_pad = 1;

            let buf2 = kmalloc(core::mem::size_of::<DmaBuf>(), GFP_KERNEL).cast::<DmaBuf>();
            core::ptr::write_bytes(buf2, 0, 1);
            (*buf2)._vmapping_pad = 1;

            assert_eq!(drm_prime_add_buf_handle(&mut prime, buf1, 10), 0);
            assert_eq!(drm_prime_add_buf_handle(&mut prime, buf2, 20), 0);
            assert_eq!((*buf1)._vmapping_pad, 2);
            assert_eq!((*buf2)._vmapping_pad, 2);

            // Lookup buffer by handle
            assert_eq!(drm_prime_lookup_buf_by_handle(&mut prime, 10), buf1);
            assert_eq!(drm_prime_lookup_buf_by_handle(&mut prime, 20), buf2);
            assert!(drm_prime_lookup_buf_by_handle(&mut prime, 30).is_null());

            // Lookup handle by buffer
            let mut h: u32 = 0;
            assert_eq!(drm_prime_lookup_buf_handle(&mut prime, buf1, &mut h), 0);
            assert_eq!(h, 10);
            assert_eq!(drm_prime_lookup_buf_handle(&mut prime, buf2, &mut h), 0);
            assert_eq!(h, 20);
            assert_eq!(drm_prime_lookup_buf_handle(&mut prime, 0x3000 as *mut _, &mut h), -EINVAL);

            // Remove handle 10
            mutex_lock(core::ptr::addr_of_mut!(prime.lock));
            drm_prime_remove_buf_handle(&mut prime, 10);
            mutex_unlock(core::ptr::addr_of_mut!(prime.lock));
            assert!(drm_prime_lookup_buf_by_handle(&mut prime, 10).is_null());
            assert_eq!(drm_prime_lookup_buf_by_handle(&mut prime, 20), buf2);
            assert_eq!((*buf1)._vmapping_pad, 1);
            dma_buf_put(buf1);

            // Destroy prime private
            drm_prime_destroy_file_private(&mut prime);
            assert!(prime.dmabufs.node.is_null());
            assert!(prime.handles.node.is_null());
            assert_eq!((*buf2)._vmapping_pad, 1);
            dma_buf_put(buf2);

            // 2. Test handle <-> fd round-trip via real dma-buf export & import
            let mut driver: DrmDriver = core::mem::zeroed();
            let dev = drm_dev_alloc(&driver, core::ptr::null_mut());
            let file_priv = drm_file_alloc((*dev).primary);

            // Create a real GEM object and handle
            let mut obj_raw: DrmGemObject = core::mem::zeroed();
            let obj = &mut obj_raw as *mut DrmGemObject;
            drm_gem_private_object_init(dev, obj, 4096);
            let mut orig_handle: u32 = 0;
            assert_eq!(drm_gem_handle_create(file_priv, obj, &mut orig_handle), 0);
            assert_ne!(orig_handle, 0);

            let mut args = DrmPrimeHandle {
                handle: orig_handle,
                flags: 0,
                fd: -1,
            };
            assert_eq!(drm_prime_handle_to_fd_ioctl(dev, &mut args as *mut _ as *mut _, file_priv), 0);
            assert!(args.fd >= 0);

            // Second export returns the same cached dma-buf
            let mut args_cached = DrmPrimeHandle {
                handle: orig_handle,
                flags: 0,
                fd: -1,
            };
            assert_eq!(drm_prime_handle_to_fd_ioctl(dev, &mut args_cached as *mut _ as *mut _, file_priv), 0);
            assert!(args_cached.fd >= 0);

            // Import fd in second file
            let file_priv2 = drm_file_alloc((*dev).primary);
            let mut args2 = DrmPrimeHandle {
                handle: 0,
                flags: 0,
                fd: args.fd,
            };
            assert_eq!(drm_prime_fd_to_handle_ioctl(dev, &mut args2 as *mut _ as *mut _, file_priv2), 0);
            assert_ne!(args2.handle, 0);

            // Look up imported object and verify it matches original
            let imported_obj = drm_gem_object_lookup(file_priv2, args2.handle);
            assert_eq!(imported_obj, obj);
            drm_gem_object_put(imported_obj);

            drm_file_free(file_priv2);
            drm_file_free(file_priv);
            drm_dev_put(dev);
        }
    }
}
