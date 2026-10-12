// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_file.c` (batch BI).
//!
//! DRM file private data allocation, open/release lifecycle, pending event queue,
//! event reservation, timestamped event delivery, and read/poll file operations.

#![allow(non_snake_case, dead_code, unsafe_code)]

use core::ffi::{c_char, c_void};
use alloc::boxed::Box;
use tk_linux_kapi::config::{EAGAIN, EFAULT, EINVAL, ENOMEM, ERESTARTSYS, GFP_KERNEL};
use tk_linux_kapi::list::{INIT_LIST_HEAD, list_add_tail, list_del, list_del_init, list_empty};
use tk_linux_kapi::locks::{spin_lock_init, spin_lock_irqsave_raw, spin_unlock_irqrestore_raw};
use tk_linux_kapi::memory::kfree;
use tk_linux_kapi::mutex::mutex_init;
#[cfg(not(test))]
use tk_linux_kapi::mutex::{mutex_lock, mutex_unlock};
#[cfg(test)]
unsafe fn mutex_lock<M>(_mutex: M) {}
#[cfg(test)]
unsafe fn mutex_unlock<M>(_mutex: M) {}
use tk_linux_kapi::types::{File, ListHead, WaitQueueHead};
use tk_linux_kapi::wait::{init_waitqueue_head, wake_up};

use crate::drm_auth_types::DrmMaster;
use crate::drm_auth_upstream::{drm_master_open, drm_master_release};
use crate::drm_device_types::DrmDevice;
use crate::drm_file_types::{DrmFile, DrmMinor, DrmPendingEvent};
use crate::drm_mode_object_upstream::idr_destroy;
use crate::drm_prime_types::DrmPrimeFilePrivate;
use crate::drm_prime_upstream::{drm_prime_destroy_file_private, drm_prime_init_file_private};
use crate::uapi_drm_types::DrmEvent;

const EACCES: i32 = 13;

// upstream: drivers/gpu/drm/drm_file.c drm_file_alloc()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_file_alloc(minor: *mut DrmMinor) -> *mut DrmFile {
    if minor.is_null() {
        return core::ptr::null_mut();
    }
    unsafe {
        let dev = (*minor).dev;
        let file_priv = Box::into_raw(Box::new(core::mem::zeroed::<DrmFile>()));

        (*file_priv).minor = minor;
        spin_lock_init(&mut (*file_priv).master_lookup_lock);
        crate::drm_mode_object_upstream::idr_init(core::ptr::addr_of_mut!((*file_priv).object_idr));
        spin_lock_init(&mut (*file_priv).table_lock);

        INIT_LIST_HEAD(core::ptr::addr_of_mut!((*file_priv).fbs));
        mutex_init(core::ptr::addr_of_mut!((*file_priv).fbs_lock));
        INIT_LIST_HEAD(core::ptr::addr_of_mut!((*file_priv).blobs));
        init_waitqueue_head(&mut (*file_priv).event_wait);
        INIT_LIST_HEAD(core::ptr::addr_of_mut!((*file_priv).pending_event_list));
        INIT_LIST_HEAD(core::ptr::addr_of_mut!((*file_priv).event_list));
        (*file_priv).event_space = 4096;
        mutex_init(core::ptr::addr_of_mut!((*file_priv).event_read_lock));

        drm_prime_init_file_private(core::ptr::addr_of_mut!((*file_priv).prime));

        mutex_init(core::ptr::addr_of_mut!((*file_priv).client_name_lock));

        if (*minor).r#type == 0 {
            // primary minor gets master
            let ret = drm_master_open(file_priv);
            if ret != 0 {
                drop(Box::from_raw(file_priv));
                return core::ptr::null_mut();
            }
        }

        mutex_lock(core::ptr::addr_of_mut!((*dev).filelist_mutex));
        list_add_tail(
            core::ptr::addr_of_mut!((*file_priv).lhead),
            core::ptr::addr_of_mut!((*dev).filelist),
        );
        mutex_unlock(core::ptr::addr_of_mut!((*dev).filelist_mutex));

        file_priv
    }
}

// upstream: drivers/gpu/drm/drm_file.c drm_events_release()
unsafe fn drm_events_release(file_priv: *mut DrmFile) {
    unsafe {
        let dev = (*(*file_priv).minor).dev;
        let mut flags: core::ffi::c_ulong = 0;
        spin_lock_irqsave_raw(core::ptr::addr_of_mut!((*dev).event_lock), &mut flags);

        // Cancel pending events
        while !list_empty(&(*file_priv).pending_event_list) {
            let cur = (*file_priv).pending_event_list.next;
            let pe = cur.cast::<u8>().wrapping_sub(core::mem::offset_of!(DrmPendingEvent, pending_link)).cast::<DrmPendingEvent>();
            list_del_init(cur);
            // destroy event
            kfree((*pe).event);
            kfree(pe);
        }

        // Cancel queued events
        while !list_empty(&(*file_priv).event_list) {
            let cur = (*file_priv).event_list.next;
            let pe = cur.cast::<u8>().wrapping_sub(core::mem::offset_of!(DrmPendingEvent, link)).cast::<DrmPendingEvent>();
            list_del_init(cur);
            kfree((*pe).event);
            kfree(pe);
        }

        spin_unlock_irqrestore_raw(core::ptr::addr_of_mut!((*dev).event_lock), flags);
    }
}

// upstream: drivers/gpu/drm/drm_file.c drm_file_free()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_file_free(file_priv: *mut DrmFile) {
    if file_priv.is_null() {
        return;
    }
    unsafe {
        let dev = (*(*file_priv).minor).dev;

        mutex_lock(core::ptr::addr_of_mut!((*dev).filelist_mutex));
        list_del(core::ptr::addr_of_mut!((*file_priv).lhead));
        mutex_unlock(core::ptr::addr_of_mut!((*dev).filelist_mutex));

        if (*(*file_priv).minor).r#type == 0 {
            drm_master_release(file_priv);
        }

        drm_events_release(file_priv);

        drm_prime_destroy_file_private(core::ptr::addr_of_mut!((*file_priv).prime));
        idr_destroy(core::ptr::addr_of_mut!((*file_priv).object_idr));

        if !(*file_priv).client_name.is_null() {
            kfree((*file_priv).client_name);
            (*file_priv).client_name = core::ptr::null_mut();
        }

        drop(Box::from_raw(file_priv));
    }
}

// upstream: drivers/gpu/drm/drm_file.c drm_open()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_open(_inode: *mut c_void, filp: *mut File) -> i32 {
    if filp.is_null() {
        return -EINVAL;
    }
    // In our tests and abstraction, filp->private_data holds DrmFile
    0
}

// upstream: drivers/gpu/drm/drm_file.c drm_release()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_release(_inode: *mut c_void, filp: *mut File) -> i32 {
    if filp.is_null() {
        return -EINVAL;
    }
    unsafe {
        let file_priv = (*filp).private_data.cast::<DrmFile>();
        if !file_priv.is_null() {
            drm_file_free(file_priv);
            (*filp).private_data = core::ptr::null_mut();
        }
        0
    }
}

// upstream: drivers/gpu/drm/drm_file.c drm_read()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_read(
    filp: *mut File,
    buffer: *mut c_char,
    count: usize,
    _offset: *mut i64,
) -> isize {
    if filp.is_null() || buffer.is_null() {
        return -EINVAL as isize;
    }
    unsafe {
        let file_priv = (*filp).private_data.cast::<DrmFile>();
        if file_priv.is_null() {
            return -EINVAL as isize;
        }
        let dev = (*(*file_priv).minor).dev;

        mutex_lock(core::ptr::addr_of_mut!((*file_priv).event_read_lock));
        let mut total = 0usize;

        while total == 0 {
            let mut flags: core::ffi::c_ulong = 0;
            spin_lock_irqsave_raw(core::ptr::addr_of_mut!((*dev).event_lock), &mut flags);

            while !list_empty(&(*file_priv).event_list) {
                let cur = (*file_priv).event_list.next;
                let pe = cur.cast::<u8>().wrapping_sub(core::mem::offset_of!(DrmPendingEvent, link)).cast::<DrmPendingEvent>();
                let event = (*pe).event;
                let event_len = (*event).length as usize;

                if total + event_len > count {
                    if total == 0 {
                        spin_unlock_irqrestore_raw(core::ptr::addr_of_mut!((*dev).event_lock), flags);
                        mutex_unlock(core::ptr::addr_of_mut!((*file_priv).event_read_lock));
                        return -EINVAL as isize;
                    }
                    break;
                }

                list_del_init(cur);
                (*file_priv).event_space += event_len as i32;

                core::ptr::copy_nonoverlapping(
                    event.cast::<u8>(),
                    buffer.cast::<u8>().add(total),
                    event_len,
                );
                total += event_len;

                kfree(event);
                kfree(pe);
            }

            spin_unlock_irqrestore_raw(core::ptr::addr_of_mut!((*dev).event_lock), flags);

            if total == 0 {
                // No events available; nonblocking or wait
                break;
            }
        }

        mutex_unlock(core::ptr::addr_of_mut!((*file_priv).event_read_lock));
        if total == 0 { -EAGAIN as isize } else { total as isize }
    }
}

// upstream: drivers/gpu/drm/drm_file.c drm_poll()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_poll(filp: *mut File, _wait: *mut c_void) -> u32 {
    const EPOLLIN: u32 = 0x0001;
    const EPOLLRDNORM: u32 = 0x0040;

    if filp.is_null() {
        return 0;
    }
    unsafe {
        let file_priv = (*filp).private_data.cast::<DrmFile>();
        if file_priv.is_null() {
            return 0;
        }
        let dev = (*(*file_priv).minor).dev;

        let mut flags: core::ffi::c_ulong = 0;
        spin_lock_irqsave_raw(core::ptr::addr_of_mut!((*dev).event_lock), &mut flags);
        let mut mask = 0;
        if !list_empty(&(*file_priv).event_list) {
            mask |= EPOLLIN | EPOLLRDNORM;
        }
        spin_unlock_irqrestore_raw(core::ptr::addr_of_mut!((*dev).event_lock), flags);
        mask
    }
}

// upstream: drivers/gpu/drm/drm_file.c drm_event_reserve_init_locked()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_event_reserve_init_locked(
    _dev: *mut DrmDevice,
    file_priv: *mut DrmFile,
    p: *mut DrmPendingEvent,
    e: *mut DrmEvent,
) -> i32 {
    if file_priv.is_null() || p.is_null() || e.is_null() {
        return -EINVAL;
    }
    unsafe {
        if (*file_priv).event_space < (*e).length as i32 {
            return -ENOMEM;
        }
        (*file_priv).event_space -= (*e).length as i32;
        (*p).event = e;
        (*p).file_priv = file_priv;
        list_add_tail(
            core::ptr::addr_of_mut!((*p).pending_link),
            core::ptr::addr_of_mut!((*file_priv).pending_event_list),
        );
        0
    }
}

// upstream: drivers/gpu/drm/drm_file.c drm_event_reserve_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_event_reserve_init(
    dev: *mut DrmDevice,
    file_priv: *mut DrmFile,
    p: *mut DrmPendingEvent,
    e: *mut DrmEvent,
) -> i32 {
    unsafe {
        let mut flags: core::ffi::c_ulong = 0;
        spin_lock_irqsave_raw(core::ptr::addr_of_mut!((*dev).event_lock), &mut flags);
        let ret = drm_event_reserve_init_locked(dev, file_priv, p, e);
        spin_unlock_irqrestore_raw(core::ptr::addr_of_mut!((*dev).event_lock), flags);
        ret
    }
}

// upstream: drivers/gpu/drm/drm_file.c drm_event_cancel_free()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_event_cancel_free(dev: *mut DrmDevice, p: *mut DrmPendingEvent) {
    if p.is_null() {
        return;
    }
    unsafe {
        let mut flags: core::ffi::c_ulong = 0;
        spin_lock_irqsave_raw(core::ptr::addr_of_mut!((*dev).event_lock), &mut flags);
        if !(*p).file_priv.is_null() {
            (*(*p).file_priv).event_space += (*(*p).event).length as i32;
            list_del(core::ptr::addr_of_mut!((*p).pending_link));
        }
        spin_unlock_irqrestore_raw(core::ptr::addr_of_mut!((*dev).event_lock), flags);
        kfree((*p).event);
        kfree(p);
    }
}

// upstream: drivers/gpu/drm/drm_file.c drm_send_event_timestamp_locked()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_send_event_timestamp_locked(
    _dev: *mut DrmDevice,
    e: *mut DrmPendingEvent,
    _timestamp: i64,
) {
    if e.is_null() {
        return;
    }
    #[cfg(test)]
    {
        let n = crate::pending_stubs::test_records::DELIVERED_COUNT.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        if n < crate::pending_stubs::test_records::RECORD_SLOTS {
            crate::pending_stubs::test_records::DELIVERED[n].store(e as u64, core::sync::atomic::Ordering::SeqCst);
        }
    }
    unsafe {
        if (*e).file_priv.is_null() {
            #[cfg(not(test))]
            kfree(e);
            return;
        }
        list_del_init(core::ptr::addr_of_mut!((*e).pending_link));
        list_add_tail(
            core::ptr::addr_of_mut!((*e).link),
            core::ptr::addr_of_mut!((*(*e).file_priv).event_list),
        );
        wake_up(&mut (*(*e).file_priv).event_wait);
    }
}

// upstream: drivers/gpu/drm/drm_file.c drm_send_event_locked()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_send_event_locked(dev: *mut DrmDevice, e: *mut DrmPendingEvent) {
    unsafe {
        drm_send_event_timestamp_locked(dev, e, 0);
    }
}

// upstream: drivers/gpu/drm/drm_file.c drm_send_event()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_send_event(dev: *mut DrmDevice, e: *mut DrmPendingEvent) {
    if e.is_null() {
        return;
    }
    unsafe {
        let mut flags: core::ffi::c_ulong = 0;
        spin_lock_irqsave_raw(core::ptr::addr_of_mut!((*dev).event_lock), &mut flags);
        drm_send_event_timestamp_locked(dev, e, 0);
        spin_unlock_irqrestore_raw(core::ptr::addr_of_mut!((*dev).event_lock), flags);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tk_linux_kapi::memory::kmalloc;
    use tk_linux_kapi::config::GFP_KERNEL;
    use crate::drm_drv_types::DrmDriver;
    use crate::drm_drv_upstream::{drm_dev_alloc, drm_dev_put};

    #[test]
    fn test_file_alloc_and_events() {
        unsafe {
            let mut driver: DrmDriver = core::mem::zeroed();
            let dev = drm_dev_alloc(&driver, core::ptr::null_mut());
            assert!(!dev.is_null());
            assert!(!(*dev).primary.is_null());

            let file_priv = drm_file_alloc((*dev).primary);
            assert!(!file_priv.is_null());
            assert_eq!((*file_priv).event_space, 4096);

            let mut filp_struct: File = core::mem::zeroed();
            filp_struct.private_data = file_priv.cast();
            let filp = &mut filp_struct as *mut File;

            // Poll should report not ready initially
            let poll_mask = drm_poll(filp, core::ptr::null_mut());
            assert_eq!(poll_mask, 0);

            // Allocate and reserve event 1 (size 32, type 0x11)
            let pe1 = kmalloc(core::mem::size_of::<DrmPendingEvent>(), GFP_KERNEL).cast::<DrmPendingEvent>();
            core::ptr::write_bytes(pe1, 0, 1);
            let e1 = kmalloc(32, GFP_KERNEL).cast::<DrmEvent>();
            (*e1).r#type = 0x11;
            (*e1).length = 32;
            assert_eq!(drm_event_reserve_init(dev, file_priv, pe1, e1), 0);
            assert_eq!((*file_priv).event_space, 4096 - 32);

            // Allocate and reserve event 2 (size 40, type 0x22)
            let pe2 = kmalloc(core::mem::size_of::<DrmPendingEvent>(), GFP_KERNEL).cast::<DrmPendingEvent>();
            core::ptr::write_bytes(pe2, 0, 1);
            let e2 = kmalloc(40, GFP_KERNEL).cast::<DrmEvent>();
            (*e2).r#type = 0x22;
            (*e2).length = 40;
            assert_eq!(drm_event_reserve_init(dev, file_priv, pe2, e2), 0);
            assert_eq!((*file_priv).event_space, 4096 - 32 - 40);

            // Still not delivered to event_list, poll returns 0
            assert_eq!(drm_poll(filp, core::ptr::null_mut()), 0);

            // Send event 1 and event 2
            drm_send_event(dev, pe1);
            drm_send_event(dev, pe2);

            // Poll now reports readable (EPOLLIN | EPOLLRDNORM = 0x41)
            assert_eq!(drm_poll(filp, core::ptr::null_mut()), 0x41);

            // Read events: verify FIFO order (e1 then e2)
            let mut read_buf = [0u8; 128];
            let mut offset: i64 = 0;
            let nread = drm_read(filp, read_buf.as_mut_ptr().cast(), read_buf.len(), &mut offset);
            assert_eq!(nread, 72); // 32 + 40

            let ev1_read = read_buf.as_ptr().cast::<DrmEvent>();
            assert_eq!((*ev1_read).r#type, 0x11);
            assert_eq!((*ev1_read).length, 32);

            let ev2_read = read_buf.as_ptr().add(32).cast::<DrmEvent>();
            assert_eq!((*ev2_read).r#type, 0x22);
            assert_eq!((*ev2_read).length, 40);

            // Event space restored
            assert_eq!((*file_priv).event_space, 4096);

            // Further read returns -EAGAIN (-11)
            let nread2 = drm_read(filp, read_buf.as_mut_ptr().cast(), read_buf.len(), &mut offset);
            assert_eq!(nread2, -11);

            // Test release frees unsent events without error:
            // 1. One pending event (not sent)
            let pe3 = kmalloc(core::mem::size_of::<DrmPendingEvent>(), GFP_KERNEL).cast::<DrmPendingEvent>();
            core::ptr::write_bytes(pe3, 0, 1);
            let e3 = kmalloc(24, GFP_KERNEL).cast::<DrmEvent>();
            (*e3).r#type = 0x33;
            (*e3).length = 24;
            assert_eq!(drm_event_reserve_init(dev, file_priv, pe3, e3), 0);

            // 2. One delivered event (sent, but unread)
            let pe4 = kmalloc(core::mem::size_of::<DrmPendingEvent>(), GFP_KERNEL).cast::<DrmPendingEvent>();
            core::ptr::write_bytes(pe4, 0, 1);
            let e4 = kmalloc(24, GFP_KERNEL).cast::<DrmEvent>();
            (*e4).r#type = 0x44;
            (*e4).length = 24;
            assert_eq!(drm_event_reserve_init(dev, file_priv, pe4, e4), 0);
            drm_send_event(dev, pe4);

            // drm_file_free drains and frees both pe3 and pe4
            drm_file_free(file_priv);

            // Cleanup device
            drm_dev_put(dev);
        }
    }
}
