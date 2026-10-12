// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_displayid.c` (batch B1b).
//!
//! Source-faithful transcription of Linux 7.2.3. EDID extension lookup and
//! panel matching are owned by the EDID batch and are declared `extern "C"`
//! here, resolved through `crate::pending_stubs` until they land.

use core::ffi::{c_char, c_void};
use core::mem::size_of;

use tk_linux_kapi::assert::warn_on;
use tk_linux_kapi::config::{EINVAL, ERR_PTR, IS_ERR};
use tk_linux_kapi::print::{drm_log_at, DrmLogLevel};

use crate::drm_displayid_internal_types::{DisplayidBlock, DisplayidHeader, DisplayidIter};
use crate::drm_edid_types::DrmEdidIdent;

// upstream: include/drm/drm_edid.h
const EDID_LENGTH: i32 = 128;
const DISPLAYID_EXT: i32 = 0x70;

// upstream: include/drm/drm_edid.h drm_edid_encode_panel_id()
const fn drm_edid_encode_panel_id(c0: u8, c1: u8, c2: u8, product_id: u32) -> u32 {
    ((((c0 as u32).wrapping_sub(b'@' as u32)) & 0x1f) << 26)
        | ((((c1 as u32).wrapping_sub(b'@' as u32)) & 0x1f) << 21)
        | ((((c2 as u32).wrapping_sub(b'@' as u32)) & 0x1f) << 16)
        | (product_id & 0xffff)
}

// upstream: drm_displayid.c enum QUIRK_IGNORE_CHECKSUM
const QUIRK_IGNORE_CHECKSUM: u8 = 0;

// upstream: drm_displayid.c struct displayid_quirk
#[repr(C)]
struct DisplayidQuirk {
    ident: DrmEdidIdent,
    quirks: u8,
}

// upstream: drm_displayid.c quirks[]
const QUIRKS: [DisplayidQuirk; 1] = [DisplayidQuirk {
    ident: DrmEdidIdent {
        panel_id: drm_edid_encode_panel_id(b'C', b'S', b'O', 5142),
        name: c"MNE007ZA1-5".as_ptr() as *mut c_char,
    },
    quirks: 1 << QUIRK_IGNORE_CHECKSUM,
}];

// EDID helpers owned by the EDID batch (see pending_stubs/).
unsafe extern "C" {
    fn drm_edid_match(drm_edid: *const c_void, ident: *const DrmEdidIdent) -> bool;
    fn drm_edid_find_extension(drm_edid: *const c_void, ext_id: i32, ext_index: *mut i32) -> *const u8;
}

// upstream: drm_displayid.c get_quirks()
unsafe fn get_quirks(drm_edid: *const c_void) -> u8 {
    for q in QUIRKS.iter() {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        if unsafe { drm_edid_match(drm_edid, &q.ident) } {
            return q.quirks;
        }
    }
    0
}

// upstream: drm_displayid.c displayid_get_header()
unsafe fn displayid_get_header(displayid: *const u8, length: i32, index: i32) -> *const DisplayidHeader {
    if (size_of::<DisplayidHeader>() as i64) > (length - index) as i64 {
        return ERR_PTR::<DisplayidHeader>(-EINVAL) as *const DisplayidHeader;
    }
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { displayid.add(index as usize) as *const DisplayidHeader }
}

// upstream: drm_displayid.c validate_displayid()
pub(crate) unsafe fn validate_displayid(
    displayid: *const u8,
    length: i32,
    idx: i32,
    ignore_checksum: bool,
) -> *const DisplayidHeader {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let base = unsafe { displayid_get_header(displayid, length, idx) };
    if IS_ERR(base) {
        return base;
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let base_ref = unsafe { &*base };
    let dispid_length = size_of::<DisplayidHeader>() as i32 + base_ref.bytes as i32 + 1;
    if dispid_length > length - idx {
        return ERR_PTR::<DisplayidHeader>(-EINVAL) as *const DisplayidHeader;
    }

    let mut csum: u8 = 0;
    for i in 0..dispid_length {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        csum = csum.wrapping_add(unsafe { *displayid.add((idx + i) as usize) });
    }
    if csum != 0 {
        drm_log_at(
            DrmLogLevel::Info,
            "drm_displayid",
            file!(),
            line!(),
            &alloc::format!(
                "DisplayID checksum invalid, remainder is {}{}",
                csum,
                if ignore_checksum { " (ignoring)" } else { "" }
            ),
        );
        if !ignore_checksum {
            return ERR_PTR::<DisplayidHeader>(-EINVAL) as *const DisplayidHeader;
        }
    }

    base
}

// upstream: drm_displayid.c find_next_displayid_extension()
unsafe fn find_next_displayid_extension(iter: *mut DisplayidIter) -> *const u8 {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let iter = unsafe { &mut *iter };
    let ignore_checksum = iter.quirks & (1 << QUIRK_IGNORE_CHECKSUM) != 0;

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let displayid = unsafe { drm_edid_find_extension(iter.drm_edid, DISPLAYID_EXT, &mut iter.ext_index) };
    if displayid.is_null() {
        return core::ptr::null();
    }

    iter.length = EDID_LENGTH - 1;
    iter.idx = 1;

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let base = unsafe { validate_displayid(displayid, iter.length, iter.idx, ignore_checksum) };
    if IS_ERR(base) {
        return core::ptr::null();
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    iter.length = iter.idx + size_of::<DisplayidHeader>() as i32 + unsafe { (*base).bytes } as i32;
    displayid
}

// upstream: drm_displayid.c displayid_iter_edid_begin()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn displayid_iter_edid_begin(drm_edid: *const c_void, iter: *mut DisplayidIter) {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { core::ptr::write_bytes(iter, 0, 1) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let iter = unsafe { &mut *iter };
    iter.drm_edid = drm_edid as *mut c_void;
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    iter.quirks = unsafe { get_quirks(drm_edid) };
}

// upstream: drm_displayid.c displayid_iter_block()
unsafe fn displayid_iter_block(iter: *const DisplayidIter) -> *const DisplayidBlock {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let iter = unsafe { &*iter };
    if iter.section.is_null() {
        return core::ptr::null();
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let block = unsafe { iter.section.add(iter.idx as usize) as *const DisplayidBlock };
    let bsz = size_of::<DisplayidBlock>() as i32;
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let num_bytes = unsafe { (*block).num_bytes } as i32;
    if iter.idx + bsz <= iter.length && iter.idx + bsz + num_bytes <= iter.length {
        return block;
    }
    core::ptr::null()
}

// upstream: drm_displayid.c __displayid_iter_next()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __displayid_iter_next(iter: *mut DisplayidIter) -> *const DisplayidBlock {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    if unsafe { (*iter).drm_edid.is_null() } {
        return core::ptr::null();
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    if !unsafe { (*iter).section.is_null() } {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let mut block = unsafe { displayid_iter_block(iter) };
        if warn_on(block.is_null()) {
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            unsafe {
                (*iter).section = core::ptr::null_mut();
                (*iter).drm_edid = core::ptr::null_mut();
            }
            return core::ptr::null();
        }

        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe {
            (*iter).idx += size_of::<DisplayidBlock>() as i32 + (*block).num_bytes as i32;
        }
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        block = unsafe { displayid_iter_block(iter) };
        if !block.is_null() {
            return block;
        }
    }

    loop {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let base_section = unsafe { (*iter).section.is_null() };
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let section = unsafe { find_next_displayid_extension(iter) };
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe { (*iter).section = section as *mut u8 };
        if section.is_null() {
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            unsafe { (*iter).drm_edid = core::ptr::null_mut() };
            return core::ptr::null();
        }

        if base_section {
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            let (length, idx) = unsafe { ((*iter).length, (*iter).idx) };
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            let base = unsafe { displayid_get_header(section, length, idx) };
            if !IS_ERR(base) {
                // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
                unsafe {
                    (*iter).version = (*base).rev;
                    (*iter).primary_use = (*base).prod_id;
                }
            }
        }

        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe { (*iter).idx += size_of::<DisplayidHeader>() as i32 };
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let block = unsafe { displayid_iter_block(iter) };
        if !block.is_null() {
            return block;
        }
    }
}

// upstream: drm_displayid.c displayid_iter_end()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn displayid_iter_end(iter: *mut DisplayidIter) {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { core::ptr::write_bytes(iter, 0, 1) };
}

// upstream: drm_displayid.c displayid_version()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn displayid_version(iter: *const DisplayidIter) -> u8 {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { (*iter).version }
}

// upstream: drm_displayid.c displayid_primary_use()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn displayid_primary_use(iter: *const DisplayidIter) -> u8 {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { (*iter).primary_use }
}
