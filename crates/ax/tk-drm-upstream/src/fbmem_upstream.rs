// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//
// Focused Linux v7.2.3 fbmem/fb_info/fbcmap service port. Device-class,
// fbcon, notifier, and user-facing VFS registration are kernel integration
// work and are intentionally not represented as successful operations here.
//! Framebuffer allocation, registry, lifetime, suspend state, and colormap core.
#![allow(unsafe_code, non_snake_case)]

use core::{
    ffi::{c_int, c_ulong},
    mem::{align_of, offset_of, size_of},
    ptr,
};

use tk_linux_kapi::{
    config::{E2BIG, EINVAL, ENODEV, ENOMEM, ENXIO, EOPNOTSUPP, GFP_KERNEL},
    memory::{kfree, kmalloc, kzalloc},
    mutex::{mutex_lock, mutex_unlock},
    types::{DelayedWork, ListHead, Mutex, Page, RefcountT, WorkStruct},
};

use crate::fb_types::{
    FbCmap, FbFixScreenInfo, FbInfo, FbModelist, FbVideoMode, FB_BLANK_POWERDOWN,
    FBINFO_STATE_RUNNING, FBINFO_STATE_SUSPENDED, FBINFO_VIRTFB, FB_MAX, FB_PIXMAP_DEFAULT,
    FbDeferredIoPageRef,
};

use tk_linux_kapi::mm::{
    VM_DONTDUMP, VM_DONTEXPAND, VM_FAULT_OOM, VM_FAULT_SIGBUS, VM_IO, VmAreaStruct,
    VmFault, VmOperationsStruct,
};

const FB_PIXMAP_SIZE: usize = 4096;
const FBINFO_FOREIGN_ENDIAN: i32 = 0x0010_0000;
const FBINFO_BE_MATH: i32 = 0x0010_0000;
const FBINFO_BIG_ENDIAN_SUPPORTED: bool = false;
const ENOSYS: c_int = 38;
const VM_FAULT_LOCKED: u32 = 0x000200;

#[repr(C)]
struct FbDefioState {
    refs: RefcountT,
    open_count: c_int,
    mapping: *mut core::ffi::c_void,
    lock: Mutex,
    info: *mut FbInfo,
    pagereflist: ListHead,
    npagerefs: c_ulong,
    pagerefs: *mut FbDeferredIoPageRef,
}

// Linux fb_defio.c calls the core rmap helper before the dirty callback to
// re-protect each alias and make the next store fault visible.
unsafe extern "C" {
    fn mapping_wrprotect_range_ready() -> c_int;
    fn mapping_wrprotect_range(
        mapping: *mut core::ffi::c_void,
        pgoff: c_ulong,
        pfn: c_ulong,
        nr_pages: c_ulong,
    ) -> c_int;
}

static mut REGISTERED_FB: [*mut FbInfo; FB_MAX] = [ptr::null_mut(); FB_MAX];
static mut NUM_REGISTERED_FB: usize = 0;
static mut REGISTRATION_LOCK: Mutex = unsafe { core::mem::zeroed() };

unsafe fn registered_slot(node: usize) -> *mut *mut FbInfo {
    unsafe { ptr::addr_of_mut!(REGISTERED_FB).cast::<*mut FbInfo>().add(node) }
}

unsafe fn lock_registration() {
    // The zeroed static mutex has Linux's unlocked x86_64 `struct mutex`
    // representation. Its C-layout owner word is mutable, so the storage must
    // stay in writable data rather than an immutable static in .rodata.
    unsafe { mutex_lock(ptr::addr_of_mut!(REGISTRATION_LOCK)) };
}

unsafe fn unlock_registration() {
    unsafe { mutex_unlock(ptr::addr_of_mut!(REGISTRATION_LOCK)) };
}

/// Linux `framebuffer_alloc(size, dev)` for the selected fb_info layout.
/// `par` is aligned to `sizeof(long)` and resides in the same allocation.
///
/// # Safety
/// `device` is either null or a live parent device pointer. The returned
/// allocation must eventually be passed to [`fb_release_info`].
#[unsafe(export_name = "framebuffer_alloc")]
pub unsafe extern "C" fn framebuffer_alloc(
    size: usize,
    device: *mut core::ffi::c_void,
) -> *mut FbInfo {
    let padding = if size == 0 {
        0
    } else {
        align_of::<c_ulong>() - size_of::<FbInfo>() % align_of::<c_ulong>()
    };
    let Some(info_size) = size_of::<FbInfo>().checked_add(padding) else {
        return ptr::null_mut();
    };
    let Some(allocation_size) = info_size.checked_add(size) else {
        return ptr::null_mut();
    };
    let info = kzalloc(allocation_size, GFP_KERNEL).cast::<FbInfo>();
    if info.is_null() {
        return ptr::null_mut();
    }

    unsafe {
        (*info).device = device;
        (*info).fbcon_rotate_hint = -1;
        (*info).blank = 0; // FB_BLANK_UNBLANK
        if size != 0 {
            (*info).par = info.cast::<u8>().add(info_size).cast();
        }
    }
    info
}

/// Convenience allocation used by DRM fb-helper, equivalent to Linux's
/// `framebuffer_alloc(0, NULL)`; later device wiring may assign `device`.
#[unsafe(export_name = "fb_alloc_info")]
pub unsafe extern "C" fn fb_alloc_info() -> *mut FbInfo {
    unsafe { framebuffer_alloc(0, ptr::null_mut()) }
}

/// Linux `framebuffer_release()`. Registered/live framebuffer allocations are
/// deliberately retained, matching the source core's refcount guard.
///
/// # Safety
/// `info` is null or is an allocation returned by [`framebuffer_alloc`], and
/// is not accessed concurrently after a successful release.
#[unsafe(export_name = "fb_release_info")]
pub unsafe extern "C" fn fb_release_info(info: *mut FbInfo) {
    if info.is_null() {
        return;
    }
    if unsafe { tk_linux_kapi::memory::refcount_read(&(*info).count) } != 0 {
        return;
    }
    unsafe { kfree(info) };
}

/// Alias with the source kernel spelling.
#[unsafe(export_name = "framebuffer_release")]
pub unsafe extern "C" fn framebuffer_release(info: *mut FbInfo) {
    unsafe { fb_release_info(info) };
}

fn mode_from_var(var: &crate::fb_types::FbVarScreenInfo) -> FbVideoMode {
    FbVideoMode {
        name: ptr::null(),
        refresh: 0,
        xres: var.xres,
        yres: var.yres,
        pixclock: var.pixclock,
        left_margin: var.left_margin,
        right_margin: var.right_margin,
        upper_margin: var.upper_margin,
        lower_margin: var.lower_margin,
        hsync_len: var.hsync_len,
        vsync_len: var.vsync_len,
        sync: var.sync,
        vmode: var.vmode,
        flag: 0,
    }
}

fn same_mode(a: &FbVideoMode, b: &FbVideoMode) -> bool {
    a.xres == b.xres
        && a.yres == b.yres
        && a.pixclock == b.pixclock
        && a.left_margin == b.left_margin
        && a.right_margin == b.right_margin
        && a.upper_margin == b.upper_margin
        && a.lower_margin == b.lower_margin
        && a.hsync_len == b.hsync_len
        && a.vsync_len == b.vsync_len
        && a.sync == b.sync
        && a.vmode == b.vmode
}

unsafe fn list_init(head: *mut tk_linux_kapi::types::ListHead) {
    unsafe {
        (*head).next = head;
        (*head).prev = head;
    }
}

unsafe fn list_is_empty(head: *mut tk_linux_kapi::types::ListHead) -> bool {
    unsafe { (*head).next == head }
}

unsafe fn add_initial_mode(info: *mut FbInfo) -> c_int {
    let head = unsafe { ptr::addr_of_mut!((*info).modelist) };
    if unsafe { (*head).next.is_null() || (*head).prev.is_null() } {
        unsafe { list_init(head) };
    }

    let mode = mode_from_var(unsafe { &(*info).var });
    let mut pos = unsafe { (*head).next };
    while pos != head {
        let item = pos.cast::<FbModelist>(); // list is the first field
        if same_mode(&unsafe { (*item).mode }, &mode) {
            return 0;
        }
        pos = unsafe { (*pos).next };
    }

    let item = kzalloc(size_of::<FbModelist>(), GFP_KERNEL).cast::<FbModelist>();
    if item.is_null() {
        return -ENOMEM;
    }
    unsafe {
        (*item).mode = mode;
        let node = ptr::addr_of_mut!((*item).list);
        (*node).next = head;
        (*node).prev = (*head).prev;
        (*(*head).prev).next = node;
        (*head).prev = node;
    }
    0
}

unsafe fn release_modelist(head: *mut tk_linux_kapi::types::ListHead) {
    if unsafe { (*head).next.is_null() || (*head).prev.is_null() } {
        return;
    }
    while !unsafe { list_is_empty(head) } {
        let node = unsafe { (*head).next };
        unsafe {
            (*(*node).next).prev = head;
            (*head).next = (*node).next;
            (*node).next = node;
            (*node).prev = node;
            kfree(node.cast::<FbModelist>());
        }
    }
    unsafe { list_init(head) };
}

fn check_endianness(info: *mut FbInfo) -> c_int {
    // Linux's source `fb_check_foreignness()` converts FOREIGN_ENDIAN into
    // BE_MATH and rejects it when the selected kernel was not built with the
    // corresponding fb byte-order support.
    unsafe {
        let foreign = (*info).flags & FBINFO_FOREIGN_ENDIAN != 0;
        (*info).flags &= !FBINFO_FOREIGN_ENDIAN;
        if foreign {
            (*info).flags |= FBINFO_BE_MATH;
        } else {
            (*info).flags &= !FBINFO_BE_MATH;
        }
        if (*info).flags & FBINFO_BE_MATH != 0 && !FBINFO_BIG_ENDIAN_SUPPORTED {
            -ENOSYS
        } else {
            0
        }
    }
}

/// Linux `register_framebuffer()` core registry path. This establishes a real
/// bounded registry entry, mode-list item, reference, locks, and pixmap state.
/// It intentionally does not claim to create `/dev/fbN`, sysfs, fbcon, or
/// notifier registrations; those require TheKernel's V1/V2 system-device glue.
///
/// # Safety
/// `info` must point to a live fb_info allocation and its callbacks/embedded
/// data must remain valid until unregister and final release.
#[unsafe(export_name = "register_framebuffer")]
pub unsafe extern "C" fn register_framebuffer(info: *mut FbInfo) -> c_int {
    if info.is_null() || unsafe { (*info).fbops.is_null() } {
        return -EINVAL;
    }
    let endian_error = check_endianness(info);
    if endian_error != 0 {
        return endian_error;
    }

    unsafe { lock_registration() };
    let result = unsafe {
        if ptr::addr_of!(NUM_REGISTERED_FB).read() >= FB_MAX {
            -ENXIO
        } else {
            let mut node = None;
            let mut duplicate = false;
            for index in 0..FB_MAX {
                let current = registered_slot(index).read();
                if current == info {
                    duplicate = true;
                    break;
                }
                if current.is_null() && node.is_none() {
                    node = Some(index);
                }
            }
            if duplicate {
                -EINVAL
            } else if let Some(node) = node {
                let mode_ret = add_initial_mode(info);
                if mode_ret != 0 {
                    mode_ret
                } else {
                    (*info).node = node as c_int;
                    tk_linux_kapi::memory::refcount_set(&mut (*info).count, 1);
                    tk_linux_kapi::mutex::mutex_init(ptr::addr_of_mut!((*info).lock));
                    tk_linux_kapi::mutex::mutex_init(ptr::addr_of_mut!((*info).mm_lock));
                    if (*(*info).fbops).fb_blank.is_some() {
                        (*info).blank = FB_BLANK_POWERDOWN;
                    }
                    if (*info).pixmap.addr.is_null() {
                        let pixmap = kmalloc(FB_PIXMAP_SIZE, GFP_KERNEL).cast::<u8>();
                        if !pixmap.is_null() {
                            (*info).pixmap.addr = pixmap;
                            (*info).pixmap.size = FB_PIXMAP_SIZE as u32;
                            (*info).pixmap.buf_align = 1;
                            (*info).pixmap.scan_align = 1;
                            (*info).pixmap.access_align = 32;
                            (*info).pixmap.flags = FB_PIXMAP_DEFAULT;
                        }
                    }
                    (*info).pixmap.offset = 0;
                    if (*info).pixmap.blit_x == [0] {
                        (*info).pixmap.blit_x = [!0];
                    }
                    if (*info).pixmap.blit_y == [0, 0] {
                        (*info).pixmap.blit_y = [!0, !0];
                    }
                    registered_slot(node).write(info);
                    ptr::addr_of_mut!(NUM_REGISTERED_FB).write(
                        ptr::addr_of!(NUM_REGISTERED_FB).read() + 1,
                    );
                    0
                }
            } else {
                -ENXIO
            }
        }
    };
    unsafe { unlock_registration() };
    result
}

/// Linux `unregister_framebuffer()` registry/lifetime core. The consuming
/// registration reference is dropped and an optional driver destroy callback
/// runs at zero; the allocation remains owned until `fb_release_info()`.
///
/// # Safety
/// `info` is live and, if registered, is not concurrently being unregistered.
#[unsafe(export_name = "unregister_framebuffer")]
pub unsafe extern "C" fn unregister_framebuffer(info: *mut FbInfo) {
    if info.is_null() {
        return;
    }
    unsafe { lock_registration() };
    let node = unsafe { (*info).node };
    if node < 0 || node as usize >= FB_MAX || unsafe { registered_slot(node as usize).read() != info } {
        unsafe { unlock_registration() };
        return;
    }

    unsafe {
        registered_slot(node as usize).write(ptr::null_mut());
        ptr::addr_of_mut!(NUM_REGISTERED_FB).write(
            ptr::addr_of!(NUM_REGISTERED_FB).read() - 1,
        );
        if !(*info).pixmap.addr.is_null() && (*info).pixmap.flags & FB_PIXMAP_DEFAULT != 0 {
            kfree((*info).pixmap.addr);
            (*info).pixmap.addr = ptr::null_mut();
            (*info).pixmap.size = 0;
            (*info).pixmap.flags = 0;
        }
        release_modelist(ptr::addr_of_mut!((*info).modelist));
        (*info).node = -1;
        if tk_linux_kapi::memory::refcount_dec_and_test(&mut (*info).count) {
            if let Some(destroy) = (*(*info).fbops).fb_destroy {
                destroy(info);
            }
        }
    }
    unsafe { unlock_registration() };
}

/// Take a registered fb_info reference by Linux framebuffer node index.
/// Returns null for an invalid or absent node.
#[unsafe(export_name = "get_fb_info")]
pub unsafe extern "C" fn get_fb_info(node: u32) -> *mut FbInfo {
    if node as usize >= FB_MAX {
        return ptr::null_mut();
    }
    unsafe { lock_registration() };
    let info = unsafe { registered_slot(node as usize).read() };
    if !info.is_null() {
        unsafe { tk_linux_kapi::memory::refcount_inc_not_zero(&mut (*info).count) };
    }
    unsafe { unlock_registration() };
    info
}

/// Drop a reference obtained from the registry.
#[unsafe(export_name = "put_fb_info")]
pub unsafe extern "C" fn put_fb_info(info: *mut FbInfo) {
    if info.is_null() {
        return;
    }
    let last = unsafe { tk_linux_kapi::memory::refcount_dec_and_test(&mut (*info).count) };
    if last {
        if let Some(destroy) = unsafe { (*(*info).fbops).fb_destroy } {
            unsafe { destroy(info) };
        }
    }
}

/// `fb_set_suspend()` state transition. Console notifications and VT policy
/// are deferred until the fbcon/system-device layer is wired into TheKernel.
#[unsafe(export_name = "fb_set_suspend")]
pub unsafe extern "C" fn fb_set_suspend(info: *mut FbInfo, state: c_int) {
    if info.is_null() {
        return;
    }
    unsafe {
        if state != 0 {
            (*info).state = FBINFO_STATE_SUSPENDED;
        } else {
            (*info).state = FBINFO_STATE_RUNNING;
        }
    }
}

const RED_2: [u16; 2] = [0x0000, 0xaaaa];
const GREEN_2: [u16; 2] = [0x0000, 0xaaaa];
const BLUE_2: [u16; 2] = [0x0000, 0xaaaa];
const RED_4: [u16; 4] = [0x0000, 0xaaaa, 0x5555, 0xffff];
const GREEN_4: [u16; 4] = [0x0000, 0xaaaa, 0x5555, 0xffff];
const BLUE_4: [u16; 4] = [0x0000, 0xaaaa, 0x5555, 0xffff];
const RED_8: [u16; 8] = [0, 0, 0, 0, 0xaaaa, 0xaaaa, 0xaaaa, 0xaaaa];
const GREEN_8: [u16; 8] = [0, 0, 0xaaaa, 0xaaaa, 0, 0, 0x5555, 0xaaaa];
const BLUE_8: [u16; 8] = [0, 0xaaaa, 0, 0xaaaa, 0, 0xaaaa, 0, 0xaaaa];
const RED_16: [u16; 16] = [0, 0, 0, 0, 0xaaaa, 0xaaaa, 0xaaaa, 0xaaaa, 0x5555, 0x5555, 0x5555, 0x5555, 0xffff, 0xffff, 0xffff, 0xffff];
const GREEN_16: [u16; 16] = [0, 0, 0xaaaa, 0xaaaa, 0, 0, 0x5555, 0xaaaa, 0x5555, 0x5555, 0xffff, 0xffff, 0x5555, 0x5555, 0xffff, 0xffff];
const BLUE_16: [u16; 16] = [0, 0xaaaa, 0, 0xaaaa, 0, 0xaaaa, 0, 0xaaaa, 0x5555, 0xffff, 0x5555, 0xffff, 0x5555, 0xffff, 0x5555, 0xffff];

fn default_palette(len: usize) -> (&'static [u16], &'static [u16], &'static [u16]) {
    if len <= 2 {
        (&RED_2, &GREEN_2, &BLUE_2)
    } else if len <= 4 {
        (&RED_4, &GREEN_4, &BLUE_4)
    } else if len <= 8 {
        (&RED_8, &GREEN_8, &BLUE_8)
    } else {
        (&RED_16, &GREEN_16, &BLUE_16)
    }
}

unsafe fn free_cmap_planes(cmap: *mut FbCmap) {
    unsafe {
        kfree((*cmap).red);
        kfree((*cmap).green);
        kfree((*cmap).blue);
        kfree((*cmap).transp);
        (*cmap).red = ptr::null_mut();
        (*cmap).green = ptr::null_mut();
        (*cmap).blue = ptr::null_mut();
        (*cmap).transp = ptr::null_mut();
        (*cmap).len = 0;
    }
}

/// Linux `fb_alloc_cmap()` with the default fbdev palette. The generic KAPI
/// allocator only supports sleepable allocation, so this path uses GFP_KERNEL
/// rather than pretending the unsupported GFP_ATOMIC allocation succeeded.
#[unsafe(export_name = "fb_alloc_cmap")]
pub unsafe extern "C" fn fb_alloc_cmap(cmap: *mut FbCmap, len: c_int, transp: c_int) -> c_int {
    if cmap.is_null() || len < 0 {
        return -EINVAL;
    }
    if len == 0 {
        unsafe { free_cmap_planes(cmap) };
        return 0;
    }
    let len = len as usize;
    let Some(bytes) = len.checked_mul(size_of::<u16>()) else {
        return -E2BIG;
    };

    unsafe {
        if (*cmap).len as usize != len {
            free_cmap_planes(cmap);
            let red = kzalloc(bytes, GFP_KERNEL).cast::<u16>();
            if red.is_null() {
                return -ENOMEM;
            }
            let green = kzalloc(bytes, GFP_KERNEL).cast::<u16>();
            if green.is_null() {
                kfree(red);
                return -ENOMEM;
            }
            let blue = kzalloc(bytes, GFP_KERNEL).cast::<u16>();
            if blue.is_null() {
                kfree(green);
                kfree(red);
                return -ENOMEM;
            }
            let alpha = if transp != 0 {
                let alpha = kzalloc(bytes, GFP_KERNEL).cast::<u16>();
                if alpha.is_null() {
                    kfree(blue);
                    kfree(green);
                    kfree(red);
                    return -ENOMEM;
                }
                alpha
            } else {
                ptr::null_mut()
            };
            (*cmap).red = red;
            (*cmap).green = green;
            (*cmap).blue = blue;
            (*cmap).transp = alpha;
        }

        let (red, green, blue) = default_palette(len);
        let entries = len.min(red.len());
        ptr::copy_nonoverlapping(red.as_ptr(), (*cmap).red, entries);
        ptr::copy_nonoverlapping(green.as_ptr(), (*cmap).green, entries);
        ptr::copy_nonoverlapping(blue.as_ptr(), (*cmap).blue, entries);
        (*cmap).start = 0;
        (*cmap).len = len as u32;
    }
    0
}

#[unsafe(export_name = "fb_dealloc_cmap")]
pub unsafe extern "C" fn fb_dealloc_cmap(cmap: *mut FbCmap) {
    if !cmap.is_null() {
        unsafe { free_cmap_planes(cmap) };
    }
}

/// Linux `fb_copy_cmap()` for kernel-owned cmap records.
#[unsafe(export_name = "fb_copy_cmap")]
pub unsafe extern "C" fn fb_copy_cmap(from: *const FbCmap, to: *mut FbCmap) -> c_int {
    if from.is_null() || to.is_null() {
        return -EINVAL;
    }
    let (from_start, from_len, to_start, to_len) = unsafe {
        ((*from).start, (*from).len, (*to).start, (*to).len)
    };
    let (fromoff, tooff) = if to_start > from_start {
        (to_start - from_start, 0)
    } else {
        (0, from_start - to_start)
    };
    if fromoff >= from_len || tooff >= to_len {
        return -EINVAL;
    }
    let entries = (to_len - tooff).min(from_len - fromoff) as usize;
    if entries == 0 {
        return -EINVAL;
    }
    unsafe {
        if (*from).red.is_null() || (*from).green.is_null() || (*from).blue.is_null()
            || (*to).red.is_null() || (*to).green.is_null() || (*to).blue.is_null()
        {
            return -EINVAL;
        }
        ptr::copy_nonoverlapping((*from).red.add(fromoff as usize), (*to).red.add(tooff as usize), entries);
        ptr::copy_nonoverlapping((*from).green.add(fromoff as usize), (*to).green.add(tooff as usize), entries);
        ptr::copy_nonoverlapping((*from).blue.add(fromoff as usize), (*to).blue.add(tooff as usize), entries);
        if !(*from).transp.is_null() && !(*to).transp.is_null() {
            ptr::copy_nonoverlapping((*from).transp.add(fromoff as usize), (*to).transp.add(tooff as usize), entries);
        }
    }
    0
}

unsafe fn state_from_info(info: *mut FbInfo) -> *mut FbDefioState {
    if info.is_null() {
        ptr::null_mut()
    } else {
        unsafe { (*info).fbdefio_state.cast::<FbDefioState>() }
    }
}

unsafe fn state_ref_get(state: *mut FbDefioState) -> bool {
    !state.is_null()
        && unsafe { tk_linux_kapi::memory::refcount_inc_not_zero(&mut (*state).refs) }
}

unsafe fn state_ref_put(state: *mut FbDefioState) {
    if state.is_null() {
        return;
    }
    if unsafe { tk_linux_kapi::memory::refcount_dec_and_test(&mut (*state).refs) } {
        unsafe {
            kfree((*state).pagerefs);
            kfree(state);
        }
    }
}

unsafe fn state_lock(state: *mut FbDefioState) {
    unsafe { mutex_lock(ptr::addr_of_mut!((*state).lock)) };
}

unsafe fn state_unlock(state: *mut FbDefioState) {
    unsafe { mutex_unlock(ptr::addr_of_mut!((*state).lock)) };
}

unsafe fn init_list(head: *mut ListHead) {
    unsafe {
        (*head).next = head;
        (*head).prev = head;
    }
}

unsafe fn list_empty(head: *mut ListHead) -> bool {
    unsafe { (*head).next == head }
}

unsafe fn list_add_tail(entry: *mut ListHead, head: *mut ListHead) {
    unsafe {
        (*entry).next = head;
        (*entry).prev = (*head).prev;
        (*(*head).prev).next = entry;
        (*head).prev = entry;
    }
}

unsafe fn list_del_init(entry: *mut ListHead) {
    unsafe {
        (*(*entry).next).prev = (*entry).prev;
        (*(*entry).prev).next = (*entry).next;
        (*entry).next = entry;
        (*entry).prev = entry;
    }
}

unsafe fn page_ref_from_list(node: *mut ListHead) -> *mut FbDeferredIoPageRef {
    unsafe {
        node.cast::<u8>()
            .sub(offset_of!(FbDeferredIoPageRef, list))
            .cast::<FbDeferredIoPageRef>()
    }
}

/// Hold one fb_defio state reference for a VM mapping sidecar. The core VM
/// mapping owner must pair this with [`fb_deferred_io_state_put`].
#[unsafe(export_name = "fb_deferred_io_state_get")]
pub unsafe extern "C" fn fb_deferred_io_state_get(
    info: *mut FbInfo,
) -> *mut crate::fb_types::FbDeferredIoState {
    let state = unsafe { state_from_info(info) };
    if unsafe { state_ref_get(state) } {
        state.cast()
    } else {
        ptr::null_mut()
    }
}

/// Drop a VM mapping's state reference; the state frees itself after cleanup
/// has released the base ref and the final mapping has closed.
#[unsafe(export_name = "fb_deferred_io_state_put")]
pub unsafe extern "C" fn fb_deferred_io_state_put(state: *mut crate::fb_types::FbDeferredIoState) {
    unsafe { state_ref_put(state.cast()) };
}

/// Publish the mapping cookie used by the generic rmap write-protect hook.
/// All VMAs for one fbdev instance must use the same address-space mapping.
#[unsafe(export_name = "fb_deferred_io_set_mapping")]
pub unsafe extern "C" fn fb_deferred_io_set_mapping(
    info: *mut FbInfo,
    mapping: *mut core::ffi::c_void,
) -> c_int {
    if info.is_null() || mapping.is_null() {
        return -EINVAL;
    }
    let state = unsafe { state_from_info(info) };
    if state.is_null() {
        return -EINVAL;
    }
    unsafe { state_lock(state) };
    let ret = unsafe {
        if (*state).info.is_null() {
            -ENODEV
        } else if !(*state).mapping.is_null() && (*state).mapping != mapping {
            -EINVAL
        } else {
            (*state).mapping = mapping;
            0
        }
    };
    unsafe { state_unlock(state) };
    ret
}

/// Resolve a framebuffer offset to its page and return the referenced page
/// required by the VM fault path. A driver callback owns the returned ref, as
/// specified by Linux `fb_deferred_io.get_page`.
#[unsafe(export_name = "fb_deferred_io_get_page")]
pub unsafe extern "C" fn fb_deferred_io_get_page(info: *mut FbInfo, offset: c_ulong) -> *mut Page {
    if info.is_null() {
        return ptr::null_mut();
    }
    let state = unsafe { state_from_info(info) };
    if state.is_null() {
        return ptr::null_mut();
    }
    unsafe { state_lock(state) };
    let info_live = unsafe { (*state).info == info };
    let fbdefio = unsafe { (*info).fbdefio };
    let smem_len = unsafe { (*info).fix.smem_len as c_ulong };
    if !info_live || offset >= smem_len || fbdefio.is_null() {
        unsafe { state_unlock(state) };
        return ptr::null_mut();
    }
    let page = if let Some(get_page) = unsafe { (*fbdefio).get_page } {
        unsafe { get_page(info, offset) }
    } else {
        let physical = unsafe { (*info).fix.smem_start }.checked_add(offset);
        match physical {
            Some(physical) if physical != 0 => {
                let page = tk_linux_kapi::shmem::pfn_to_page(
                    (physical >> tk_linux_kapi::config::PAGE_SHIFT) as c_ulong,
                );
                if !page.is_null() {
                    unsafe { tk_linux_kapi::shmem::get_page(page) };
                }
                page
            }
            _ => ptr::null_mut(),
        }
    };
    unsafe { state_unlock(state) };
    page
}

/// Record one dirty VM page and queue the source delayed callback. This is
/// called only from the VM's lock-out fault phase; it may sleep on fb_defio's
/// state mutex but is never called while holding an address-space lock.
#[unsafe(export_name = "fb_deferred_io_page_mkwrite")]
pub unsafe extern "C" fn fb_deferred_io_page_mkwrite(
    info: *mut FbInfo,
    offset: c_ulong,
    page: *mut Page,
) -> u32 {
    if info.is_null() || page.is_null() {
        return VM_FAULT_SIGBUS;
    }
    let state = unsafe { state_from_info(info) };
    if state.is_null() {
        return VM_FAULT_SIGBUS;
    }
    unsafe { state_lock(state) };
    let state_info = unsafe { (*state).info };
    let mapping = unsafe { (*state).mapping };
    let smem_len = unsafe { (*info).fix.smem_len as c_ulong };
    let count = unsafe { (*state).npagerefs as usize };
    let index = (offset / tk_linux_kapi::config::PAGE_SIZE as c_ulong) as usize;
    if state_info != info || mapping.is_null() || offset >= smem_len || index >= count {
        unsafe { state_unlock(state) };
        return VM_FAULT_SIGBUS;
    }

    let pageref = unsafe { (*state).pagerefs.add(index) };
    let old_page = unsafe { (*pageref).page };
    if !old_page.is_null() && old_page != page {
        unsafe { state_unlock(state) };
        return VM_FAULT_OOM;
    }
    let new_page = old_page.is_null();
    if new_page {
        // Pages outside the LinuxKPI page registry cannot safely be retained
        // or passed to mapping_wrprotect_range.
        if unsafe { tk_linux_kapi::shmem::try_page_to_phys(page) }.is_none() {
            unsafe { state_unlock(state) };
            return VM_FAULT_OOM;
        }
        unsafe {
            tk_linux_kapi::shmem::get_page(page);
            (*pageref).page = page;
            (*pageref).offset = (index * tk_linux_kapi::config::PAGE_SIZE) as c_ulong;
        }
    }

    let list = unsafe { ptr::addr_of_mut!((*pageref).list) };
    let added_to_list = unsafe { (*list).next == list };
    if added_to_list {
        unsafe { list_add_tail(list, ptr::addr_of_mut!((*state).pagereflist)) };
    }
    if !unsafe { tk_linux_kapi::shmem::trylock_page(page) } {
        if new_page {
            unsafe {
                if added_to_list {
                    list_del_init(list);
                }
                (*pageref).page = ptr::null_mut();
                (*pageref).offset = 0;
                tk_linux_kapi::shmem::put_page(page);
            }
        }
        unsafe { state_unlock(state) };
        return VM_FAULT_OOM;
    }

    let delay = unsafe { (*(*info).fbdefio).delay as u64 };
    unsafe {
        tk_linux_kapi::workqueue::queue_delayed_work(
            tk_linux_kapi::workqueue::system_dfl_wq,
            ptr::addr_of_mut!((*info).deferred_work),
            delay,
        );
        state_unlock(state);
    }
    VM_FAULT_LOCKED
}

fn fb_deferred_io_work_trampoline(work: &mut WorkStruct) {
    unsafe { fb_deferred_io_work(work as *mut WorkStruct) };
}

unsafe fn fb_deferred_io_work(work: *mut WorkStruct) {
    let info = unsafe {
        work.cast::<u8>()
            .sub(offset_of!(FbInfo, deferred_work) + offset_of!(DelayedWork, work))
            .cast::<FbInfo>()
    };
    if info.is_null() {
        return;
    }
    let state = unsafe { state_from_info(info) };
    if state.is_null() {
        return;
    }
    unsafe { state_lock(state) };
    let state_info = unsafe { (*state).info };
    let mapping = unsafe { (*state).mapping };
    let mut rearm_ok = state_info == info && !mapping.is_null();
    let head = unsafe { ptr::addr_of_mut!((*state).pagereflist) };
    let mut node = unsafe { (*head).next };
    while node != head {
        let pageref = unsafe { page_ref_from_list(node) };
        let page = unsafe { (*pageref).page };
        let physical = unsafe { tk_linux_kapi::shmem::try_page_to_phys(page) };
        if let Some(physical) = physical {
            let ret = unsafe {
                mapping_wrprotect_range(
                    mapping,
                    (*pageref).offset / tk_linux_kapi::config::PAGE_SIZE as c_ulong,
                    (physical >> tk_linux_kapi::config::PAGE_SHIFT) as c_ulong,
                    1,
                )
            };
            if ret != 0 {
                rearm_ok = false;
            }
        } else {
            rearm_ok = false;
        }
        node = unsafe { (*node).next };
    }

    if rearm_ok {
        if let Some(callback) = unsafe { (*(*info).fbdefio).deferred_io } {
            unsafe { callback(info, ptr::addr_of_mut!((*state).pagereflist)) };
        }
    }

    while !unsafe { list_empty(ptr::addr_of_mut!((*state).pagereflist)) } {
        let list = unsafe { (*state).pagereflist.next };
        let pageref = unsafe { page_ref_from_list(list) };
        let page = unsafe { (*pageref).page };
        unsafe {
            list_del_init(list);
            (*pageref).page = ptr::null_mut();
            (*pageref).offset = 0;
            tk_linux_kapi::shmem::put_page(page);
        }
    }
    unsafe { state_unlock(state) };
}

unsafe extern "C" fn fb_deferred_io_vm_open(vma: *mut VmAreaStruct) {
    if vma.is_null() {
        return;
    }
    let state = unsafe { (*vma).vm_private_data.cast::<FbDefioState>() };
    let _ = unsafe { state_ref_get(state) };
}

unsafe extern "C" fn fb_deferred_io_vm_close(vma: *mut VmAreaStruct) {
    if vma.is_null() {
        return;
    }
    let state = unsafe { (*vma).vm_private_data.cast::<FbDefioState>() };
    unsafe { state_ref_put(state) };
}

unsafe extern "C" fn fb_deferred_io_vm_fault(vmf: *mut VmFault) -> u32 {
    if vmf.is_null() || unsafe { (*vmf).vma.is_null() } {
        return VM_FAULT_SIGBUS;
    }
    let vma = unsafe { (*vmf).vma };
    let state = unsafe { (*vma).vm_private_data.cast::<FbDefioState>() };
    if state.is_null() {
        return VM_FAULT_SIGBUS;
    }
    let info = unsafe { (*state).info };
    if info.is_null() {
        return VM_FAULT_SIGBUS;
    }
    let Some(offset) = unsafe { (*vmf).pgoff }.checked_mul(tk_linux_kapi::config::PAGE_SIZE as c_ulong) else {
        return VM_FAULT_SIGBUS;
    };
    let page = unsafe { fb_deferred_io_get_page(info, offset) };
    if page.is_null() {
        return VM_FAULT_SIGBUS;
    }
    // Linux 7.2.3 x86_64 VmFault.page occupies byte offset 80 in the selected
    // KAPI layout. The VM adapter owns this ABI bridge until it exposes a typed
    // setter; page is returned referenced, just as fb_defio.c requires.
    unsafe { vmf.cast::<u8>().add(80).cast::<*mut Page>().write(page) };
    0
}

unsafe extern "C" fn fb_deferred_io_vm_mkwrite(vmf: *mut VmFault) -> u32 {
    if vmf.is_null() || unsafe { (*vmf).vma.is_null() } {
        return VM_FAULT_SIGBUS;
    }
    let vma = unsafe { (*vmf).vma };
    let state = unsafe { (*vma).vm_private_data.cast::<FbDefioState>() };
    if state.is_null() {
        return VM_FAULT_SIGBUS;
    }
    let info = unsafe { (*state).info };
    if info.is_null() {
        return VM_FAULT_SIGBUS;
    }
    let Some(offset) = unsafe { (*vmf).pgoff }.checked_mul(tk_linux_kapi::config::PAGE_SIZE as c_ulong) else {
        return VM_FAULT_SIGBUS;
    };
    let page = unsafe { vmf.cast::<u8>().add(80).cast::<*mut Page>().read() };
    unsafe { fb_deferred_io_page_mkwrite(info, offset, page) }
}

static FB_DEFERRED_IO_VM_OPS: VmOperationsStruct = VmOperationsStruct {
    open: Some(fb_deferred_io_vm_open),
    close: Some(fb_deferred_io_vm_close),
    fault: Some(fb_deferred_io_vm_fault),
    page_mkwrite: Some(fb_deferred_io_vm_mkwrite),
    ..VmOperationsStruct::EMPTY
};

/// Linux `fb_deferred_io_mmap()`: associate the file's mapping, require the
/// real core write-protect provider, install VM callbacks, and retain state for
/// the VMA. It refuses mmap when alias rearming is not available.
#[unsafe(export_name = "fb_deferred_io_mmap")]
pub unsafe extern "C" fn fb_deferred_io_mmap(info: *mut FbInfo, vma: *mut VmAreaStruct) -> c_int {
    if info.is_null() || vma.is_null() || unsafe { (*vma).vm_file.is_null() } {
        return -EINVAL;
    }
    let mapping = unsafe { (*(*vma).vm_file).f_mapping };
    if mapping.is_null() {
        return -EINVAL;
    }
    let ret = unsafe { fb_deferred_io_set_mapping(info, mapping) };
    if ret != 0 {
        return ret;
    }
    // No VMA is published unless the core can re-protect every alias and
    // synchronously acknowledge the required TLB mutation.
    if unsafe { mapping_wrprotect_range_ready() } != 1 {
        return -EOPNOTSUPP;
    }
    let state = unsafe { state_from_info(info) };
    if state.is_null() || !unsafe { state_ref_get(state) } {
        return -ENODEV;
    }
    unsafe {
        (*vma).vm_page_prot = tk_linux_kapi::gem_memory::pgprot_decrypted((*vma).vm_page_prot);
        (*vma).vm_ops = &FB_DEFERRED_IO_VM_OPS;
        (*vma).vm_private_data = state.cast();
        let mut flags = VM_DONTEXPAND | VM_DONTDUMP;
        if (*info).flags & FBINFO_VIRTFB as c_int == 0 {
            flags |= VM_IO;
        }
        tk_linux_kapi::mm::vm_flags_set(vma, flags);
    }
    0
}

/// Allocate fb_defio state and one tracking entry per page of framebuffer
/// memory, then initialize the genuine delayed work callback.
#[unsafe(export_name = "fb_deferred_io_init")]
pub unsafe extern "C" fn fb_deferred_io_init(info: *mut FbInfo) -> c_int {
    if info.is_null() || unsafe { (*info).fbdefio.is_null() } || unsafe { (*info).fix.smem_len == 0 } {
        return -EINVAL;
    }
    if !unsafe { (*info).fbdefio_state.is_null() } {
        return -EINVAL;
    }
    if tk_linux_kapi::workqueue::init_workqueue_executor().is_err() {
        return -ENOMEM;
    }
    let size = unsafe { (*info).fix.smem_len as usize };
    let npagerefs = size.div_ceil(tk_linux_kapi::config::PAGE_SIZE);
    let Some(pageref_bytes) = npagerefs.checked_mul(size_of::<FbDeferredIoPageRef>()) else {
        return -ENOMEM;
    };
    let state = kzalloc(size_of::<FbDefioState>(), GFP_KERNEL).cast::<FbDefioState>();
    if state.is_null() {
        return -ENOMEM;
    }
    let pagerefs = kzalloc(pageref_bytes, GFP_KERNEL).cast::<FbDeferredIoPageRef>();
    if pagerefs.is_null() {
        unsafe { kfree(state) };
        return -ENOMEM;
    }
    unsafe {
        tk_linux_kapi::memory::refcount_set(&mut (*state).refs, 1);
        (*state).info = info;
        (*state).npagerefs = npagerefs as c_ulong;
        (*state).pagerefs = pagerefs;
        init_list(ptr::addr_of_mut!((*state).pagereflist));
        for index in 0..npagerefs {
            let item = pagerefs.add(index);
            (*item).offset = (index * tk_linux_kapi::config::PAGE_SIZE) as c_ulong;
            init_list(ptr::addr_of_mut!((*item).list));
        }
        tk_linux_kapi::mutex::mutex_init(ptr::addr_of_mut!((*state).lock));
        if (*(*info).fbdefio).delay == 0 {
            (*(*info).fbdefio).delay = tk_linux_kapi::config::HZ as c_ulong;
        }
        tk_linux_kapi::workqueue::INIT_DELAYED_WORK(
            &mut (*info).deferred_work,
            fb_deferred_io_work_trampoline,
        );
        (*info).fbdefio_state = state.cast();
    }
    0
}

/// Drain pending dirty pages, mark the state dead, and release the fb_info's
/// base state reference. Existing VMAs keep the state storage alive until close.
#[unsafe(export_name = "fb_deferred_io_cleanup")]
pub unsafe extern "C" fn fb_deferred_io_cleanup(info: *mut FbInfo) {
    if info.is_null() {
        return;
    }
    let state = unsafe { state_from_info(info) };
    if state.is_null() {
        return;
    }
    unsafe {
        tk_linux_kapi::workqueue::flush_delayed_work(ptr::addr_of_mut!((*info).deferred_work));
        (*info).fbdefio_state = ptr::null_mut();
        state_lock(state);
        (*state).info = ptr::null_mut();
        state_unlock(state);
        state_ref_put(state);
    }
}

// Keep the source record names/layout referenced and compile-checked here.
const _: () = assert!(offset_of!(FbInfo, count) == 0);
const _: () = assert!(size_of::<FbFixScreenInfo>() == 80);
const _: () = assert!(size_of::<RefcountT>() == 4);
const _: () = assert!(size_of::<FbDeferredIoPageRef>() == 32);
const _: () = assert!(size_of::<FbDefioState>() == 80);
const _: () = assert!(offset_of!(FbDefioState, mapping) == 8);
const _: () = assert!(offset_of!(FbDefioState, lock) == 16);
const _: () = assert!(offset_of!(FbDefioState, info) == 40);
const _: () = assert!(offset_of!(FbDefioState, pagereflist) == 48);
const _: () = assert!(offset_of!(FbDefioState, npagerefs) == 64);
const _: () = assert!(offset_of!(FbDefioState, pagerefs) == 72);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmap_default_palette_and_cleanup_are_sized() {
        let mut cmap = FbCmap {
            start: 0,
            len: 0,
            red: ptr::null_mut(),
            green: ptr::null_mut(),
            blue: ptr::null_mut(),
            transp: ptr::null_mut(),
        };
        // This tests only the pure default palette selector, not kernel heap
        // initialization or framebuffer system-device registration.
        assert_eq!(default_palette(2).0, &RED_2);
        assert_eq!(default_palette(4).1, &GREEN_4);
        assert_eq!(default_palette(8).2, &BLUE_8);
        assert_eq!(default_palette(256).0, &RED_16);
        assert_eq!(unsafe { fb_alloc_cmap(ptr::null_mut(), 1, 0) }, -EINVAL);
        assert_eq!(unsafe { fb_alloc_cmap(&mut cmap, -1, 0) }, -EINVAL);
    }

    #[test]
    fn fb_info_abi_fields_match_selected_linux_config() {
        assert_eq!(offset_of!(FbInfo, var), 64);
        assert_eq!(offset_of!(FbInfo, deferred_work), 664);
        assert_eq!(offset_of!(FbInfo, fbops), 768);
        assert_eq!(offset_of!(FbInfo, screen_base), 792);
        assert_eq!(size_of::<FbInfo>(), 848);
    }

    #[test]
    fn defio_rejects_missing_info_or_vma() {
        assert_eq!(unsafe { fb_deferred_io_init(ptr::null_mut()) }, -EINVAL);
        assert_eq!(
            unsafe { fb_deferred_io_mmap(ptr::null_mut(), ptr::null_mut()) },
            -EINVAL
        );
        assert_eq!(
            unsafe { fb_deferred_io_page_mkwrite(ptr::null_mut(), 0, ptr::null_mut()) },
            VM_FAULT_SIGBUS
        );
    }
}
