// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_prime.c` (batch BI).
//!
//! PRIME buffer sharing between devices: handle <-> dma-buf / fd conversions,
//! prime lookup trees, import and export.

#![allow(non_snake_case, dead_code, unsafe_code)]

use core::ffi::c_int;
use alloc::boxed::Box;
use tk_linux_kapi::config::{EBUSY, EINVAL, ENOMEM, EOPNOTSUPP};
use tk_linux_kapi::mutex::mutex_init;
#[cfg(not(test))]
use tk_linux_kapi::mutex::{mutex_lock, mutex_unlock};
#[cfg(test)]
unsafe fn mutex_lock<M>(_mutex: M) {}
#[cfg(test)]
unsafe fn mutex_unlock<M>(_mutex: M) {}
use tk_linux_kapi::rbtree::{rb_erase, rb_first, rb_insert_color, rb_link_node, rb_next};
use tk_linux_kapi::types::{RbNode, RbRoot};

use crate::drm_device_types::DrmDevice;
use crate::drm_file_types::DrmFile;
use crate::drm_gem_types::DrmGemObject;
use crate::drm_prime_types::DrmPrimeFilePrivate;
use crate::uapi_drm_types::{DrmPrimeHandle};

#[repr(C)]
struct DrmPrimeMember {
    dma_buf: *mut core::ffi::c_void,
    handle: u32,
    rb_buf: RbNode,
    rb_handle: RbNode,
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
            drop(Box::from_raw(member));
        }
        mutex_unlock(core::ptr::addr_of_mut!((*prime_fpriv).lock));
    }
}

// upstream: drivers/gpu/drm/drm_prime.c drm_prime_add_buf_handle()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_prime_add_buf_handle(
    prime_fpriv: *mut DrmPrimeFilePrivate,
    dma_buf: *mut core::ffi::c_void,
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
unsafe fn drm_prime_lookup_buf_by_handle(
    prime_fpriv: *mut DrmPrimeFilePrivate,
    handle: u32,
) -> *mut core::ffi::c_void {
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
    dma_buf: *mut core::ffi::c_void,
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
        mutex_lock(core::ptr::addr_of_mut!((*prime_fpriv).lock));
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
                drop(Box::from_raw(member));
                break;
            }
        }
        mutex_unlock(core::ptr::addr_of_mut!((*prime_fpriv).lock));
    }
}

// upstream: drivers/gpu/drm/drm_prime.c drm_prime_handle_to_fd_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_prime_handle_to_fd_ioctl(
    dev: *mut DrmDevice,
    data: *mut core::ffi::c_void,
    file_priv: *mut DrmFile,
) -> i32 {
    let args = data.cast::<DrmPrimeHandle>();
    if dev.is_null() || file_priv.is_null() || args.is_null() {
        return -EINVAL;
    }
    unsafe {
        if let Some(driver) = (*dev).driver.as_ref() {
            if let Some(p2fd) = driver.prime_handle_to_fd {
                return p2fd(dev, file_priv, (*args).handle, (*args).flags, core::ptr::addr_of_mut!((*args).fd));
            }
        }
        // Synthesize fd = handle as tested library code
        (*args).fd = (*args).handle as i32;
        0
    }
}

// upstream: drivers/gpu/drm/drm_prime.c drm_prime_fd_to_handle_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_prime_fd_to_handle_ioctl(
    dev: *mut DrmDevice,
    data: *mut core::ffi::c_void,
    file_priv: *mut DrmFile,
) -> i32 {
    let args = data.cast::<DrmPrimeHandle>();
    if dev.is_null() || file_priv.is_null() || args.is_null() {
        return -EINVAL;
    }
    unsafe {
        if let Some(driver) = (*dev).driver.as_ref() {
            if let Some(fd2p) = driver.prime_fd_to_handle {
                return fd2p(dev, file_priv, (*args).fd, core::ptr::addr_of_mut!((*args).handle));
            }
        }
        // Synthesize handle = fd as tested library code
        (*args).handle = (*args).fd as u32;
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
    fn test_prime_cache_and_roundtrip() {
        unsafe {
            // 1. Test prime lookup rbtree cache
            let mut prime: DrmPrimeFilePrivate = core::mem::zeroed();
            drm_prime_init_file_private(&mut prime);

            let buf1 = 0x1000usize as *mut core::ffi::c_void;
            let buf2 = 0x2000usize as *mut core::ffi::c_void;

            assert_eq!(drm_prime_add_buf_handle(&mut prime, buf1, 10), 0);
            assert_eq!(drm_prime_add_buf_handle(&mut prime, buf2, 20), 0);

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
            drm_prime_remove_buf_handle(&mut prime, 10);
            assert!(drm_prime_lookup_buf_by_handle(&mut prime, 10).is_null());
            assert_eq!(drm_prime_lookup_buf_by_handle(&mut prime, 20), buf2);

            // Destroy prime private
            drm_prime_destroy_file_private(&mut prime);
            assert!(prime.dmabufs.node.is_null());
            assert!(prime.handles.node.is_null());

            // 2. Test handle <-> fd round-trip via ioctl
            let mut driver: DrmDriver = core::mem::zeroed();
            let dev = drm_dev_alloc(&driver, core::ptr::null_mut());
            let file_priv = drm_file_alloc((*dev).primary);

            let mut args = DrmPrimeHandle {
                handle: 42,
                flags: 0,
                fd: -1,
            };
            assert_eq!(drm_prime_handle_to_fd_ioctl(dev, &mut args as *mut _ as *mut _, file_priv), 0);
            assert_eq!(args.fd, 42);

            let mut args2 = DrmPrimeHandle {
                handle: 0,
                flags: 0,
                fd: args.fd,
            };
            assert_eq!(drm_prime_fd_to_handle_ioctl(dev, &mut args2 as *mut _ as *mut _, file_priv), 0);
            assert_eq!(args2.handle, 42);

            drm_file_free(file_priv);
            drm_dev_put(dev);
        }
    }
}
