// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// Source: drivers/video/fbdev/core/fb_io_fops.c (Linux v7.2.3).
//! Fbdev read/write helpers for framebuffers in I/O memory (`fb_io_read()`,
//! `fb_io_write()`). The mmap helper is not used by any caller of this port.
#![allow(unsafe_code, non_snake_case)]

use core::{
    ffi::{c_char, c_long, c_void},
    sync::atomic::{AtomicBool, Ordering},
};

use tk_linux_kapi::{
    config::{EFAULT, ENODEV, ENOMEM, ENOSPC, GFP_KERNEL, PAGE_SIZE},
    memory::{kfree, kmalloc},
    mm_native::{copy_from_user, copy_to_user},
};

use crate::fb_types::{FBINFO_VIRTFB, FbInfo};

const EFBIG: i32 = 27;

/// `fb_warn_once()` state for `FBINFO_VIRTFB` framebuffers (one message per boot).
static VIRTFB_WARNED: AtomicBool = AtomicBool::new(false);

#[inline]
unsafe fn warn_virtfb(info: *mut FbInfo) {
    if unsafe { (*info).flags } as u32 & FBINFO_VIRTFB != 0
        && !VIRTFB_WARNED.swap(true, Ordering::Relaxed)
    {
        crate::drm_print_upstream::drm_dev_printk(
            core::ptr::null(),
            tk_linux_kapi::print::DrmLogLevel::Warn,
            "Framebuffer is not in I/O address space.",
        );
    }
}

/// Linux `total_size` selection: `screen_size`, else `fix.smem_len`.
#[inline]
unsafe fn total_size(info: *mut FbInfo) -> usize {
    let screen = unsafe { (*info).screen_size } as usize;
    if screen == 0 {
        unsafe { (*info).fix.smem_len as usize }
    } else {
        screen
    }
}

/// `fb_io_read()`: copy framebuffer I/O memory out to a user buffer.
///
/// # Safety
/// `info` must be a live framebuffer, `ppos` valid, and `buf` a user pointer
/// valid for `count` bytes (or faulting, which is reported as `-EFAULT`).
// upstream: fb_io_fops.c fb_io_read()
#[unsafe(export_name = "fb_io_read")]
pub unsafe extern "C" fn fb_io_read(
    info: *mut FbInfo,
    buf: *mut c_char,
    count: usize,
    ppos: *mut c_long,
) -> isize {
    unsafe {
        warn_virtfb(info);
        if (*info).screen_base.is_null() {
            return -(ENODEV as isize);
        }

        let total = total_size(info);
        let p = *ppos as usize;
        if p >= total {
            return 0;
        }
        let mut count = count;
        if count >= total {
            count = total;
        }
        if count + p > total {
            count = total - p;
        }

        let buffer = kmalloc(
            if count > PAGE_SIZE { PAGE_SIZE } else { count },
            GFP_KERNEL,
        )
        .cast::<u8>();
        if buffer.is_null() {
            return -(ENOMEM as isize);
        }

        let mut buf = buf;
        let mut src = (*info).screen_base.cast::<u8>().add(p);
        if let Some(fb_sync) = (*(*info).fbops).fb_sync {
            fb_sync(info);
        }

        let mut cnt: usize = 0;
        let mut err: isize = 0;
        while count != 0 {
            let c = if count > PAGE_SIZE { PAGE_SIZE } else { count };
            // fb_memcpy_fromio(): plain copy on this x86_64 target.
            core::ptr::copy_nonoverlapping(src, buffer, c);
            src = src.add(c);

            let trailing = copy_to_user(buf.cast::<c_void>(), buffer.cast::<c_void>(), c);
            if trailing == c {
                err = -(EFAULT as isize);
                break;
            }
            let c = c - trailing;
            *ppos = (*ppos).wrapping_add(c as c_long);
            buf = buf.add(c);
            cnt += c;
            count -= c;

            // A partial copy means the user buffer is faulty: stop before the
            // source pointer over-advances on the next iteration.
            if trailing != 0 {
                break;
            }
        }

        kfree(buffer.cast::<c_void>());
        if cnt != 0 { cnt as isize } else { err }
    }
}

/// `fb_io_write()`: copy a user buffer into framebuffer I/O memory.
///
/// # Safety
/// Same contract as [`fb_io_read`], with `buf` a user pointer readable for `count`.
// upstream: fb_io_fops.c fb_io_write()
#[unsafe(export_name = "fb_io_write")]
pub unsafe extern "C" fn fb_io_write(
    info: *mut FbInfo,
    buf: *const c_char,
    count: usize,
    ppos: *mut c_long,
) -> isize {
    unsafe {
        warn_virtfb(info);
        if (*info).screen_base.is_null() {
            return -(ENODEV as isize);
        }

        let total = total_size(info);
        let p = *ppos as usize;
        if p > total {
            return -(EFBIG as isize);
        }
        let mut err: isize = 0;
        let mut count = count;
        if count > total {
            err = -(EFBIG as isize);
            count = total;
        }
        if count + p > total {
            if err == 0 {
                err = -(ENOSPC as isize);
            }
            count = total - p;
        }

        let buffer = kmalloc(
            if count > PAGE_SIZE { PAGE_SIZE } else { count },
            GFP_KERNEL,
        )
        .cast::<u8>();
        if buffer.is_null() {
            return -(ENOMEM as isize);
        }

        let mut buf = buf;
        let mut dst = (*info).screen_base.cast::<u8>().add(p);
        if let Some(fb_sync) = (*(*info).fbops).fb_sync {
            fb_sync(info);
        }

        let mut cnt: usize = 0;
        while count != 0 {
            let c = if count > PAGE_SIZE { PAGE_SIZE } else { count };
            let trailing = copy_from_user(buffer.cast::<c_void>(), buf.cast::<c_void>(), c);
            if trailing == c {
                err = -(EFAULT as isize);
                break;
            }
            let c = c - trailing;
            // fb_memcpy_toio(): plain copy on this x86_64 target.
            core::ptr::copy_nonoverlapping(buffer, dst, c);
            dst = dst.add(c);
            *ppos = (*ppos).wrapping_add(c as c_long);
            buf = buf.add(c);
            cnt += c;
            count -= c;
        }

        kfree(buffer.cast::<c_void>());
        if cnt != 0 { cnt as isize } else { err }
    }
}
