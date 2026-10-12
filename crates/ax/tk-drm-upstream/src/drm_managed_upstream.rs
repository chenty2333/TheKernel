// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_managed.c` (batch B1b).
//!
//! Source-faithful transcription of Linux 7.2.3. The `struct drmres` header
//! is private to the C file and is mirrored here with the same field order
//! (`node` followed by the 8-byte aligned `data[]` payload).

use core::ffi::{c_char, c_void};
use core::mem::{offset_of, size_of};

use tk_linux_kapi::assert::warn_on;
use tk_linux_kapi::list::{list_add, list_del, list_del_init};
use tk_linux_kapi::locks::{spin_lock_irqsave_raw, spin_unlock_irqrestore_raw};
use tk_linux_kapi::memory::{kfree, kmalloc, kzalloc};
use tk_linux_kapi::mutex::mutex_destroy;
use tk_linux_kapi::print::{drm_log_at, DrmLogLevel};
use tk_linux_kapi::types::{ListHead, Mutex};
use tk_linux_kapi::workqueue::destroy_workqueue;

use crate::drm_device_types::DrmDevice;

// upstream: include/drm/drm_managed.h
pub type DrmresReleaseT = Option<unsafe extern "C" fn(*mut DrmDevice, *mut c_void)>;

// upstream: drm_managed.c struct drmres_node
#[repr(C)]
struct DrmresNode {
    entry: ListHead,
    release: DrmresReleaseT,
    name: *const c_char,
    size: usize,
}

// upstream: drm_managed.c struct drmres
#[repr(C)]
struct Drmres {
    node: DrmresNode,
    data: [u64; 0],
}

const DRMRES_DATA_OFFSET: usize = offset_of!(Drmres, data);

// Kernel `kstrdup_const()` for the TheKernel allocator: always a heap copy,
// so `free_dr()` can unconditionally `kfree()` it.
unsafe fn kstrdup_const(s: *const c_char, _gfp: u32) -> *const c_char {
    if s.is_null() {
        return core::ptr::null();
    }
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let len = unsafe { c_strlen(s) } + 1;
    let buf = kmalloc(len, tk_linux_kapi::config::GFP_KERNEL) as *mut u8;
    if buf.is_null() {
        return core::ptr::null();
    }
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { core::ptr::copy_nonoverlapping(s as *const u8, buf, len) };
    buf as *const c_char
}

unsafe fn c_strlen(s: *const c_char) -> usize {
    let mut n = 0usize;
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    while unsafe { *s.add(n) } != 0 {
        n += 1;
    }
    n
}

// upstream: drm_managed.c drmres debug output (drm_dbg_drmres)
fn drm_dbg_drmres(msg: &str) {
    drm_log_at(DrmLogLevel::Debug, "drm_managed", file!(), line!(), msg);
}

fn name_str<'a>(name: *const c_char) -> &'a str {
    if name.is_null() {
        return "";
    }
    // SAFETY: the pointer or reference used here was validated or constructed in the lines above.
    unsafe { core::ffi::CStr::from_ptr(name) }.to_str().unwrap_or("")
}

// upstream: drm_managed.c free_dr()
unsafe fn free_dr(dr: *mut Drmres) {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe {
        kfree((*dr).node.name as *mut c_char);
        kfree(dr);
    }
}

// upstream: drm_managed.c drm_managed_release()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_managed_release(dev: *mut DrmDevice) {
    drm_dbg_drmres("drmres release begin");

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let head = unsafe { core::ptr::addr_of_mut!((*dev).managed.resources) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let mut link = unsafe { (*head).next };
    while link != head {
        // list_for_each_entry_safe(): fetch the successor before releasing.
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let next = unsafe { (*link).next };
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let dr = unsafe { link.cast::<u8>().sub(offset_of!(Drmres, node) + offset_of!(DrmresNode, entry)) as *mut Drmres };
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let size = unsafe { (*dr).node.size };
        drm_dbg_drmres(&format_rel(dr, size));
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        if let Some(release) = unsafe { (*dr).node.release } {
            let data = if size != 0 {
                // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
                unsafe { *((dr as *mut u8).add(DRMRES_DATA_OFFSET) as *mut *mut c_void) }
            } else {
                core::ptr::null_mut()
            };
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            unsafe { release(dev, data) };
        }
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe { list_del(core::ptr::addr_of_mut!((*dr).node.entry)) };
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe { free_dr(dr) };
        link = next;
    }

    drm_dbg_drmres("drmres release end");
}

fn format_rel(dr: *mut Drmres, size: usize) -> alloc::string::String {
    // SAFETY: the pointer or reference used here was validated or constructed in the lines above.
    alloc::format!("REL {:p} {} ({} bytes)", dr, name_str(unsafe { (*dr).node.name }), size)
}

// upstream: drm_managed.c alloc_dr()
unsafe fn alloc_dr(release: DrmresReleaseT, size: usize, zero: bool) -> *mut Drmres {
    let tot_size = match size_of::<Drmres>().checked_add(size) {
        Some(t) => t,
        None => return core::ptr::null_mut(),
    };
    // The C code allocates `tot_size` bytes and zeroes only the header;
    // __GFP_ZERO callers get the whole block zeroed.
    let dr = if zero { kzalloc(tot_size, tk_linux_kapi::config::GFP_KERNEL) } else { kmalloc(tot_size, tk_linux_kapi::config::GFP_KERNEL) } as *mut Drmres;
    if dr.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe {
        core::ptr::write_bytes(dr as *mut u8, 0, DRMRES_DATA_OFFSET);
        let entry = core::ptr::addr_of_mut!((*dr).node.entry);
        (*entry).next = entry;
        (*entry).prev = entry;
        (*dr).node.release = release;
        (*dr).node.size = size;
    }
    dr
}

// upstream: drm_managed.c del_dr()
unsafe fn del_dr(dev: *mut DrmDevice, dr: *mut Drmres) {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { list_del_init(core::ptr::addr_of_mut!((*dr).node.entry)) };
    let _ = dev;
    drm_dbg_drmres(&alloc::format!(
        "DEL {:p} {} ({} bytes)",
        dr,
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        name_str(unsafe { (*dr).node.name }),
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe { (*dr).node.size }
    ));
}

// upstream: drm_managed.c add_dr()
unsafe fn add_dr(dev: *mut DrmDevice, dr: *mut Drmres) {
    let mut flags: u64 = 0;
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let lock = unsafe { core::ptr::addr_of_mut!((*dev).managed.lock) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { spin_lock_irqsave_raw(lock, &mut flags) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { list_add(core::ptr::addr_of_mut!((*dr).node.entry), core::ptr::addr_of_mut!((*dev).managed.resources)) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { spin_unlock_irqrestore_raw(lock, flags) };
    drm_dbg_drmres(&alloc::format!(
        "ADD {:p} {} ({} bytes)",
        dr,
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        name_str(unsafe { (*dr).node.name }),
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe { (*dr).node.size }
    ));
}

// upstream: drm_managed.c drmm_add_final_kfree()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drmm_add_final_kfree(dev: *mut DrmDevice, container: *mut c_void) {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    warn_on(!unsafe { (*dev).managed.final_kfree }.is_null());
    warn_on((dev as usize) < (container as usize));
    // WARN_ON(dev + 1 > container + ksize(container)) is not checked: the TheKernel
    // allocator does not expose ksize().
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { (*dev).managed.final_kfree = container };
}

// upstream: drm_managed.c __drmm_add_action()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __drmm_add_action(
    dev: *mut DrmDevice,
    action: DrmresReleaseT,
    data: *mut c_void,
    name: *const c_char,
) -> i32 {
    let size = if data.is_null() { 0 } else { size_of::<*mut c_void>() };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let dr = unsafe { alloc_dr(action, size, true) };
    if dr.is_null() {
        drm_dbg_drmres(&alloc::format!("failed to add action {} for {:p}", name_str(name), data));
        return -tk_linux_kapi::config::ENOMEM;
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { (*dr).node.name = kstrdup_const(name, tk_linux_kapi::config::GFP_KERNEL) };
    if !data.is_null() {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe { *((dr as *mut u8).add(DRMRES_DATA_OFFSET) as *mut *mut c_void) = data };
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { add_dr(dev, dr) };
    0
}

// upstream: drm_managed.c __drmm_add_action_or_reset()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __drmm_add_action_or_reset(
    dev: *mut DrmDevice,
    action: DrmresReleaseT,
    data: *mut c_void,
    name: *const c_char,
) -> i32 {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let ret = unsafe { __drmm_add_action(dev, action, data, name) };
    if ret != 0 {
        if let Some(action) = action {
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            unsafe { action(dev, data) };
        }
    }
    ret
}

// upstream: drm_managed.c drmm_release_action()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drmm_release_action(dev: *mut DrmDevice, action: DrmresReleaseT, data: *mut c_void) {
    let mut dr_match: *mut Drmres = core::ptr::null_mut();
    let mut flags: u64 = 0;

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let lock = unsafe { core::ptr::addr_of_mut!((*dev).managed.lock) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { spin_lock_irqsave_raw(lock, &mut flags) };
    // list_for_each_entry_reverse()
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let head = unsafe { core::ptr::addr_of_mut!((*dev).managed.resources) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let mut link = unsafe { (*head).prev };
    while link != head {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let dr = unsafe { link.cast::<u8>().sub(offset_of!(Drmres, node) + offset_of!(DrmresNode, entry)) as *mut Drmres };
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        if unsafe { (*dr).node.release } == action {
            let matched = data.is_null()
                // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
                || unsafe { *((dr as *mut u8).add(DRMRES_DATA_OFFSET) as *mut *mut c_void) } == data;
            if matched {
                dr_match = dr;
                // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
                unsafe { del_dr(dev, dr_match) };
                break;
            }
        }
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        link = unsafe { (*link).prev };
    }
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { spin_unlock_irqrestore_raw(lock, flags) };

    if warn_on(dr_match.is_null()) {
        return;
    }

    if let Some(action) = action {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe { action(dev, data) };
    }
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { free_dr(dr_match) };
}

// upstream: drm_managed.c drmm_kmalloc()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drmm_kmalloc(dev: *mut DrmDevice, size: usize, gfp: u32) -> *mut c_void {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let dr = unsafe { alloc_dr(None, size, false) };
    if dr.is_null() {
        drm_dbg_drmres(&alloc::format!("failed to allocate {} bytes", size));
        return core::ptr::null_mut();
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { (*dr).node.name = kstrdup_const(c"kmalloc".as_ptr(), gfp) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { add_dr(dev, dr) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { (dr as *mut u8).add(DRMRES_DATA_OFFSET) as *mut c_void }
}

// upstream: drm_managed.c drmm_kstrdup()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drmm_kstrdup(dev: *mut DrmDevice, s: *const c_char, gfp: u32) -> *mut c_char {
    if s.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let size = unsafe { c_strlen(s) } + 1;
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let buf = unsafe { drmm_kmalloc(dev, size, gfp) } as *mut u8;
    if !buf.is_null() {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe { core::ptr::copy_nonoverlapping(s as *const u8, buf, size) };
    }
    buf as *mut c_char
}

// upstream: drm_managed.c drmm_kfree()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drmm_kfree(dev: *mut DrmDevice, data: *mut c_void) {
    if data.is_null() {
        return;
    }

    let mut dr_match: *mut Drmres = core::ptr::null_mut();
    let mut flags: u64 = 0;
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let lock = unsafe { core::ptr::addr_of_mut!((*dev).managed.lock) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { spin_lock_irqsave_raw(lock, &mut flags) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let head = unsafe { core::ptr::addr_of_mut!((*dev).managed.resources) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let mut link = unsafe { (*head).next };
    while link != head {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let dr = unsafe { link.cast::<u8>().sub(offset_of!(Drmres, node) + offset_of!(DrmresNode, entry)) as *mut Drmres };
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        if unsafe { (dr as *mut u8).add(DRMRES_DATA_OFFSET) as *mut c_void } == data {
            dr_match = dr;
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            unsafe { del_dr(dev, dr_match) };
            break;
        }
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        link = unsafe { (*link).next };
    }
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { spin_unlock_irqrestore_raw(lock, flags) };

    if warn_on(dr_match.is_null()) {
        return;
    }
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { free_dr(dr_match) };
}

// upstream: drm_managed.c __drmm_mutex_release()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __drmm_mutex_release(_dev: *mut DrmDevice, res: *mut c_void) {
    let lock = res as *mut Mutex;
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    mutex_destroy(unsafe { &mut *lock });
}

// upstream: drm_managed.c __drmm_workqueue_release()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __drmm_workqueue_release(_device: *mut DrmDevice, res: *mut c_void) {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { destroy_workqueue(res) };
}
