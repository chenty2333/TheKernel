// SPDX-License-Identifier: GPL-2.0-only
// Copyright 2026 TheKernel contributors.
//! LinuxKPI sync_file: Linux 7.2.3 `drivers/dma-buf/sync_file.c` and
//! `include/linux/sync_file.h`. A sync_file is an anonymous file that carries
//! one dma-fence; `SYNC_IOC_MERGE` joins two of them and `SYNC_IOC_FILE_INFO`
//! reports the fences they contain.
//!
//! The file-table operations (anon inode creation, descriptor reservation,
//! `fd_install`, and `fget`) are reached through the
//! `FileOps` provider in `kernel_services`. The kernel installs that table and
//! wraps an installed sync_file file as its own pollable descriptor, forwarding
//! poll and ioctl to the functions here.

#![allow(unsafe_code)]

use alloc::{format, vec::Vec};
use core::{
    ffi::{CStr, c_char, c_int, c_long, c_uint, c_ulong, c_void},
    mem::{offset_of, size_of},
    ptr,
    sync::atomic::{AtomicUsize, Ordering},
};

use crate::{
    config::IS_ERR,
    dma_fence::{
        dma_fence_add_callback, dma_fence_driver_name, dma_fence_get, dma_fence_get_status,
        dma_fence_is_signaled, dma_fence_put, dma_fence_remove_callback, dma_fence_set_deadline,
        dma_fence_timeline_name, dma_fence_timestamp,
    },
    dma_fence_unwrap::{
        DmaFenceUnwrap, dma_fence_dedup_array, dma_fence_unwrap_first, dma_fence_unwrap_merge,
        dma_fence_unwrap_next,
    },
    kernel_services::{anon_inode_getfile, fd_install, fget, get_unused_fd_flags, put_unused_fd},
    list::{INIT_LIST_HEAD, list_empty},
    memory::{kfree, kzalloc_obj},
    mm_native::{copy_from_user, copy_to_user},
    shmem::{File, FileOperations, Inode, fget_file, fput_file},
    types::{DmaFence, DmaFenceCb, WaitQueueHead},
    wait::{PollTable, init_waitqueue_head, poll_wait, wake_up_all},
};

const EFAULT: c_int = 14;
const ENOMEM: c_int = 12;
const ENOENT: c_int = 2;
const EINVAL: c_int = 22;
const ENOTTY: c_int = 25;
/// `EPOLLIN`: the only event a sync_file reports, as in Linux.
const EPOLLIN: c_uint = 0x001;
const O_CLOEXEC: c_int = 0o2000000;

/// `#define POLL_ENABLED 0` (sync_file.h): bit in `flags` set once the fence
/// callback has been armed for poll.
const POLL_ENABLED: u32 = 0;

/// `struct sync_file`. `file` is the first field because the ABI consumers
/// (i915, DRM syncobj) read it to hand the descriptor to `fd_install()`.
#[repr(C)]
pub struct SyncFile {
    pub file: *mut File,
    pub user_name: [c_char; 32],
    pub fence: *mut DmaFence,
    pub cb: DmaFenceCb,
    pub wq: WaitQueueHead,
    pub flags: c_ulong,
}

/// `sync_file_fops`: the identity of sync_file files. Poll and ioctl are
/// dispatched directly by this module (`sync_file_poll`, `sync_file_ioctl`);
/// `release` runs from the final `fput()`.
pub static SYNC_FILE_FOPS: FileOperations = FileOperations {
    release: Some(sync_file_fop_release),
    ..FileOperations::empty()
};

/// `uapi/linux/sync_file.h`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SyncMergeData {
    pub name: [u8; 32],
    pub fd2: i32,
    pub fence: i32,
    pub flags: u32,
    pub pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SyncFenceInfo {
    pub obj_name: [u8; 32],
    pub driver_name: [u8; 32],
    pub status: i32,
    pub flags: u32,
    pub timestamp_ns: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SyncFileInfo {
    pub name: [u8; 32],
    pub status: i32,
    pub flags: u32,
    pub num_fences: u32,
    pub pad: u32,
    pub sync_fence_info: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SyncSetDeadline {
    pub deadline_ns: u64,
    pub pad: u64,
}

const SYNC_IOC_MAGIC: u32 = b'>' as u32;
const fn sync_ioc(dir: u32, nr: u32, size: usize) -> u32 {
    (dir << 30) | ((size as u32) << 16) | (SYNC_IOC_MAGIC << 8) | nr
}
/// `_IOWR`, `_IOWR` and `_IOW` as the uapi header defines them.
pub const SYNC_IOC_MERGE: u32 = sync_ioc(3, 3, size_of::<SyncMergeData>());
pub const SYNC_IOC_FILE_INFO: u32 = sync_ioc(3, 4, size_of::<SyncFileInfo>());
pub const SYNC_IOC_SET_DEADLINE: u32 = sync_ioc(1, 5, size_of::<SyncSetDeadline>());

const _: () = {
    assert!(size_of::<SyncMergeData>() == 48);
    assert!(size_of::<SyncFenceInfo>() == 80);
    assert!(size_of::<SyncFileInfo>() == 56);
    assert!(size_of::<SyncSetDeadline>() == 16);
    assert!(SYNC_IOC_MERGE == 0xc030_3e03);
    assert!(SYNC_IOC_FILE_INFO == 0xc038_3e04);
    assert!(SYNC_IOC_SET_DEADLINE == 0x4010_3e05);
};

/// `is_sync_file_file()`: the file was created by `sync_file_create()`.
///
/// # Safety
/// `file` must be null or a live LinuxKPI file.
pub unsafe fn is_sync_file_file(file: *const File) -> bool {
    !file.is_null() && ptr::eq(unsafe { (*file).f_op }, ptr::addr_of!(SYNC_FILE_FOPS))
}

/// The sync_file behind a sync_file file (`file->private_data`).
///
/// # Safety
/// `file` must be a live sync_file file.
pub unsafe fn sync_file_of(file: *mut File) -> *mut SyncFile {
    unsafe { (*file)._private_data.cast() }
}

/// Wait queue that `sync_file_poll` and the fence callback wake. A poll
/// provider adds its entry here.
///
/// # Safety
/// `file` must be a live sync_file file.
pub unsafe fn sync_file_wait_queue(file: *mut File) -> *mut WaitQueueHead {
    unsafe { ptr::addr_of_mut!((*sync_file_of(file)).wq) }
}

/// Linux `sync_file_alloc()`: a zeroed sync_file with its anonymous file.
unsafe fn sync_file_alloc() -> *mut SyncFile {
    let sync_file = kzalloc_obj::<SyncFile>();
    if sync_file.is_null() {
        return ptr::null_mut();
    }
    let file = unsafe {
        anon_inode_getfile(
            c"sync_file".as_ptr(),
            ptr::addr_of!(SYNC_FILE_FOPS).cast(),
            sync_file.cast(),
            0,
        )
    };
    if IS_ERR(file) {
        unsafe { kfree(sync_file) };
        return ptr::null_mut();
    }
    unsafe {
        (*sync_file).file = file.cast();
        init_waitqueue_head(&mut (*sync_file).wq);
        INIT_LIST_HEAD(ptr::addr_of_mut!((*sync_file).cb.node));
    }
    sync_file
}

/// Linux `fence_check_cb_func()`: the fence signaled, wake the pollers.
unsafe extern "C" fn fence_check_cb_func(_fence: *mut DmaFence, cb: *mut DmaFenceCb) {
    let sync_file = unsafe { (cb as *mut u8).sub(offset_of!(SyncFile, cb)) }.cast::<SyncFile>();
    unsafe { wake_up_all(&mut (*sync_file).wq) };
}

/// Linux `sync_file_create()`: a sync_file holding one new reference to
/// `fence`. Release with `fput(sync_file->file)`. Null on failure.
///
/// # Safety
/// `fence` must be a live fence.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sync_file_create(fence: *mut DmaFence) -> *mut SyncFile {
    let sync_file = unsafe { sync_file_alloc() };
    if sync_file.is_null() {
        return ptr::null_mut();
    }
    unsafe { (*sync_file).fence = dma_fence_get(fence) };
    sync_file
}

/// `sync_file_fdget()`: an owned reference to the file behind `fd`, or null
/// when `fd` is not a sync_file.
unsafe fn sync_file_fdget(fd: c_int) -> *mut File {
    let file = unsafe { fget(fd) };
    if file.is_null() {
        return ptr::null_mut();
    }
    if !unsafe { is_sync_file_file(file) } {
        unsafe { fput_file(file) };
        return ptr::null_mut();
    }
    file
}

/// Linux `sync_file_get_fence()`: one owned reference to the fence of the
/// sync_file behind `fd`, or null.
///
/// # Safety
/// The caller releases the result with `dma_fence_put`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sync_file_get_fence(fd: c_int) -> *mut DmaFence {
    let file = unsafe { sync_file_fdget(fd) };
    if file.is_null() {
        return ptr::null_mut();
    }
    let fence = unsafe { dma_fence_get((*sync_file_of(file)).fence) };
    unsafe { fput_file(file) };
    fence
}

/// `strscpy()`: copy up to `dst.len() - 1` bytes of `src`, stopping at NUL,
/// and always NUL-terminate a non-empty `dst`.
fn strscpy(dst: &mut [u8], src: &[u8]) {
    let Some(last) = dst.len().checked_sub(1) else {
        return;
    };
    let src = src.split(|byte| *byte == 0).next().unwrap_or_default();
    let count = src.len().min(last);
    dst[..count].copy_from_slice(&src[..count]);
    dst[count] = 0;
}

/// `snprintf(buf, len, "%s...")` for the parts of a generated name.
fn snprintf_into(dst: &mut [u8], text: &str) {
    strscpy(dst, text.as_bytes());
}

/// Read a NUL-terminated name from a fence driver or timeline callback.
///
/// # Safety
/// `name` must be the NUL-terminated string a fence op returned.
unsafe fn fence_name<'a>(name: *const c_char) -> &'a str {
    unsafe { CStr::from_ptr(name) }.to_str().unwrap_or("")
}

/// Linux `sync_file_get_name()`: the name the merge gave the sync_file, or
/// the fence's driver, timeline, context and seqno.
///
/// # Safety
/// `sync_file` must be live; `buf` must be writable for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sync_file_get_name(
    sync_file: *mut SyncFile,
    buf: *mut c_char,
    len: c_int,
) -> *mut c_char {
    let Ok(len) = usize::try_from(len) else {
        return buf;
    };
    if len == 0 {
        return buf;
    }
    let dst = unsafe { core::slice::from_raw_parts_mut(buf.cast::<u8>(), len) };
    let name = unsafe { &(*sync_file).user_name };
    if name[0] != 0 {
        let user_name: &[u8] = unsafe { core::slice::from_raw_parts(name.as_ptr().cast(), 32) };
        strscpy(dst, user_name);
    } else {
        let fence = unsafe { (*sync_file).fence };
        let driver = unsafe { fence_name(dma_fence_driver_name(fence)) };
        let timeline = unsafe { fence_name(dma_fence_timeline_name(fence)) };
        let (context, seqno) = unsafe { ((*fence).context, (*fence).seqno as i64) };
        snprintf_into(dst, &format!("{driver}-{timeline}{context}-{seqno}"));
    }
    buf
}

/// Linux `sync_file_merge()`: a new sync_file holding the unwrapped fences of
/// `a` and `b`, named `name`. `a` and `b` stay valid. Null on failure.
///
/// # Safety
/// `a` and `b` must be live sync_files.
pub unsafe fn sync_file_merge(name: &[u8], a: *mut SyncFile, b: *mut SyncFile) -> *mut SyncFile {
    let sync_file = unsafe { sync_file_alloc() };
    if sync_file.is_null() {
        return ptr::null_mut();
    }
    let fences = unsafe { [(*a).fence, (*b).fence] };
    let fence = unsafe { dma_fence_unwrap_merge(&fences) };
    if fence.is_null() {
        unsafe { fput_file((*sync_file).file) };
        return ptr::null_mut();
    }
    unsafe {
        (*sync_file).fence = fence;
        strscpy(
            core::slice::from_raw_parts_mut((*sync_file).user_name.as_mut_ptr().cast(), 32),
            name,
        );
    }
    sync_file
}

/// Linux `sync_file_release()`: the `f_op->release` of a sync_file, run by the
/// last `fput()`.
///
/// # Safety
/// `file` must be a sync_file file whose last reference is being dropped.
pub unsafe fn sync_file_release(file: *mut File) {
    let sync_file = unsafe { sync_file_of(file) };
    if unsafe { (*sync_file).flags } & (1 << POLL_ENABLED) != 0 {
        unsafe { dma_fence_remove_callback((*sync_file).fence, &mut (*sync_file).cb) };
    }
    unsafe {
        dma_fence_put((*sync_file).fence);
        kfree(sync_file);
    }
}

/// `sync_file_fops.release` (`sync_file_release()` in Linux's file table).
unsafe extern "C" fn sync_file_fop_release(_inode: *mut Inode, file: *mut File) -> c_int {
    unsafe { sync_file_release(file) };
    0
}

/// Linux `sync_file_poll()`. `pt` is the caller's poll table (null when
/// polling without registration). Returns `EPOLLIN` once the fence signaled.
///
/// # Safety
/// `file` must be a live sync_file file; `pt` must be null or live.
pub unsafe fn sync_file_poll(file: *mut File, pt: *mut PollTable) -> c_uint {
    let sync_file = unsafe { sync_file_of(file) };
    unsafe { poll_wait(file.cast(), ptr::addr_of_mut!((*sync_file).wq), pt) };
    if unsafe { list_empty(&(*sync_file).cb.node) } {
        let bit = 1 << POLL_ENABLED;
        let flags = unsafe { AtomicUsize::from_ptr(ptr::addr_of_mut!((*sync_file).flags).cast()) };
        if flags.fetch_or(bit, Ordering::AcqRel) & bit == 0
            && unsafe {
                dma_fence_add_callback(
                    (*sync_file).fence,
                    ptr::addr_of_mut!((*sync_file).cb),
                    fence_check_cb_func,
                )
            } < 0
        {
            unsafe { wake_up_all(&mut (*sync_file).wq) };
        }
    }
    if unsafe { dma_fence_is_signaled((*sync_file).fence) } {
        EPOLLIN
    } else {
        0
    }
}

/// `SYNC_IOC_MERGE` (`sync_file_ioctl_merge()`).
unsafe fn sync_file_ioctl_merge(sync_file: *mut SyncFile, arg: usize) -> c_long {
    let fd = unsafe { get_unused_fd_flags(O_CLOEXEC) };
    if fd < 0 {
        return fd as c_long;
    }
    let mut data = SyncMergeData::default();
    if unsafe {
        copy_from_user(
            ptr::addr_of_mut!(data).cast(),
            arg as *const c_void,
            size_of::<SyncMergeData>(),
        )
    } != 0
    {
        unsafe { put_unused_fd(fd) };
        return -EFAULT as c_long;
    }
    if data.flags != 0 || data.pad != 0 {
        unsafe { put_unused_fd(fd) };
        return -EINVAL as c_long;
    }
    let file2 = unsafe { sync_file_fdget(data.fd2) };
    if file2.is_null() {
        unsafe { put_unused_fd(fd) };
        return -ENOENT as c_long;
    }
    data.name[data.name.len() - 1] = 0;
    let file3 = unsafe { sync_file_merge(&data.name, sync_file, sync_file_of(file2)) };
    if file3.is_null() {
        unsafe {
            fput_file(file2);
            put_unused_fd(fd);
        }
        return -ENOMEM as c_long;
    }
    data.fence = fd;
    if unsafe {
        copy_to_user(
            arg as *mut c_void,
            ptr::addr_of!(data).cast(),
            size_of::<SyncMergeData>(),
        )
    } != 0
    {
        unsafe {
            fput_file((*file3).file);
            fput_file(file2);
            put_unused_fd(fd);
        }
        return -EFAULT as c_long;
    }
    unsafe {
        fd_install(fd, (*file3).file.cast());
        fput_file(file2);
    }
    0
}

/// Count the unwrapped fences under `head`.
unsafe fn unwrap_count(head: *mut DmaFence) -> u32 {
    let mut cursor = DmaFenceUnwrap::empty();
    let mut count = 0;
    let mut fence = unsafe { dma_fence_unwrap_first(head, &mut cursor) };
    while !fence.is_null() {
        count += 1;
        fence = unsafe { dma_fence_unwrap_next(&mut cursor) };
    }
    count
}

/// Linux `sync_fill_fence_info()`: the status of one unwrapped fence.
unsafe fn sync_fill_fence_info(fence: *mut DmaFence, info: &mut SyncFenceInfo) -> i32 {
    let timeline = unsafe { fence_name(dma_fence_timeline_name(fence)) };
    let driver = unsafe { fence_name(dma_fence_driver_name(fence)) };
    strscpy(&mut info.obj_name, timeline.as_bytes());
    strscpy(&mut info.driver_name, driver.as_bytes());
    info.status = unsafe { dma_fence_get_status(fence) };
    info.timestamp_ns = if unsafe { dma_fence_is_signaled(fence) } {
        unsafe { dma_fence_timestamp(fence) as u64 }
    } else {
        0
    };
    info.status
}

/// Result of `SYNC_IOC_FILE_INFO` before anything is copied to userspace.
pub struct FileInfo {
    pub status: i32,
    pub num_fences: u32,
    pub name: [u8; 32],
    /// Filled only when the caller asked for members (`num_fences != 0`).
    pub fences: Vec<SyncFenceInfo>,
}

/// Body of `sync_file_ioctl_fence_info()` between its copies. `requested` is
/// the caller's `info.num_fences`.
///
/// # Safety
/// `sync_file` must be live.
pub unsafe fn sync_file_fence_info(sync_file: *mut SyncFile, requested: u32) -> Result<FileInfo, c_long> {
    let fence = unsafe { (*sync_file).fence };
    let num_fences = unsafe { unwrap_count(fence) };
    let mut name = [0u8; 32];
    unsafe {
        sync_file_get_name(sync_file, name.as_mut_ptr().cast(), name.len() as c_int);
    }
    if requested == 0 {
        return Ok(FileInfo {
            status: unsafe { dma_fence_get_status(fence) },
            num_fences,
            name,
            fences: Vec::new(),
        });
    }
    if requested < num_fences {
        return Err(-EINVAL as c_long);
    }
    let mut fences: Vec<SyncFenceInfo> = Vec::new();
    if fences.try_reserve_exact(num_fences as usize).is_err() {
        return Err(-ENOMEM as c_long);
    }
    fences.resize(num_fences as usize, SyncFenceInfo::default());
    let mut status = 1;
    let mut index = 0usize;
    let mut cursor = DmaFenceUnwrap::empty();
    let mut member = unsafe { dma_fence_unwrap_first(fence, &mut cursor) };
    while !member.is_null() {
        let member_status = unsafe { sync_fill_fence_info(member, &mut fences[index]) };
        index += 1;
        status = if status <= 0 { status } else { member_status };
        member = unsafe { dma_fence_unwrap_next(&mut cursor) };
    }
    Ok(FileInfo {
        status,
        num_fences,
        name,
        fences,
    })
}

/// `SYNC_IOC_FILE_INFO` (`sync_file_ioctl_fence_info()`).
unsafe fn sync_file_ioctl_fence_info(sync_file: *mut SyncFile, arg: usize) -> c_long {
    let mut info = SyncFileInfo::default();
    if unsafe {
        copy_from_user(
            ptr::addr_of_mut!(info).cast(),
            arg as *const c_void,
            size_of::<SyncFileInfo>(),
        )
    } != 0
    {
        return -EFAULT as c_long;
    }
    if info.flags != 0 || info.pad != 0 {
        return -EINVAL as c_long;
    }
    let result = match unsafe { sync_file_fence_info(sync_file, info.num_fences) } {
        Ok(result) => result,
        Err(error) => return error,
    };
    if !result.fences.is_empty()
        && unsafe {
            copy_to_user(
                info.sync_fence_info as *mut c_void,
                result.fences.as_ptr().cast(),
                result.fences.len() * size_of::<SyncFenceInfo>(),
            )
        } != 0
    {
        return -EFAULT as c_long;
    }
    info.status = result.status;
    info.name = result.name;
    info.num_fences = result.num_fences;
    if unsafe {
        copy_to_user(
            arg as *mut c_void,
            ptr::addr_of!(info).cast(),
            size_of::<SyncFileInfo>(),
        )
    } != 0
    {
        return -EFAULT as c_long;
    }
    0
}

/// `SYNC_IOC_SET_DEADLINE` (`sync_file_ioctl_set_deadline()`).
unsafe fn sync_file_ioctl_set_deadline(sync_file: *mut SyncFile, arg: usize) -> c_long {
    let mut ts = SyncSetDeadline::default();
    if unsafe {
        copy_from_user(
            ptr::addr_of_mut!(ts).cast(),
            arg as *const c_void,
            size_of::<SyncSetDeadline>(),
        )
    } != 0
    {
        return -EFAULT as c_long;
    }
    if ts.pad != 0 {
        return -EINVAL as c_long;
    }
    unsafe { dma_fence_set_deadline((*sync_file).fence, ts.deadline_ns as i64) };
    0
}

/// Linux `sync_file_ioctl()`. Returns the ioctl result or `-errno`.
///
/// # Safety
/// `file` must be a live sync_file file; `arg` is a userspace address for the
/// commands that take one.
pub unsafe fn sync_file_ioctl(file: *mut File, cmd: u32, arg: usize) -> c_long {
    let sync_file = unsafe { sync_file_of(file) };
    match cmd {
        SYNC_IOC_MERGE => unsafe { sync_file_ioctl_merge(sync_file, arg) },
        SYNC_IOC_FILE_INFO => unsafe { sync_file_ioctl_fence_info(sync_file, arg) },
        SYNC_IOC_SET_DEADLINE => unsafe { sync_file_ioctl_set_deadline(sync_file, arg) },
        _ => -ENOTTY as c_long,
    }
}

#[cfg(test)]
mod tests {
    // Host tests keep the fences they create: the final `dma_fence_put` runs
    // the RCU release, which needs the task scheduler these tests do not start.
    use core::{ffi::c_void, ptr};

    use super::*;
    use crate::{
        dma_fence::{dma_fence_init, dma_fence_signal},
        kernel_services::{FileOps, install_file_ops},
        shmem::shmem_file_setup,
        types::{DmaFenceOps, Spinlock},
    };

    const FD_BASE: c_int = 300;

    /// Fake descriptor table: `fd_install` takes the file's reference.
    static FD_TABLE: spin::Mutex<Vec<(c_int, usize)>> = spin::Mutex::new(Vec::new());
    static NEXT_FD: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn test_anon_inode_getfile(
        name: *const c_char,
        fops: *const c_void,
        private_data: *mut c_void,
        flags: c_int,
    ) -> *mut c_void {
        let file = unsafe { shmem_file_setup(name, 0, 0) };
        unsafe {
            (*file).f_op = fops.cast();
            (*file)._private_data = private_data;
            (*file).f_flags = flags as u32;
        }
        file.cast()
    }

    unsafe extern "C" fn test_get_unused_fd_flags(_flags: c_int) -> c_int {
        FD_BASE + NEXT_FD.fetch_add(1, Ordering::Relaxed) as c_int
    }

    unsafe extern "C" fn test_put_unused_fd(_fd: c_int) {}

    unsafe extern "C" fn test_fd_install(fd: c_int, file: *mut c_void) {
        FD_TABLE.lock().push((fd, file as usize));
    }

    unsafe extern "C" fn test_fget(fd: c_int) -> *mut c_void {
        let table = FD_TABLE.lock();
        match table.iter().find(|(entry, _)| *entry == fd) {
            // Take the reference the caller will drop with `fput_file`.
            Some((_, file)) => unsafe { fget_file(*file as *mut File) }.cast(),
            None => ptr::null_mut(),
        }
    }

    static TEST_FILE_OPS: FileOps = FileOps {
        anon_inode_getfile: test_anon_inode_getfile,
        get_unused_fd_flags: test_get_unused_fd_flags,
        put_unused_fd: test_put_unused_fd,
        fd_install: test_fd_install,
        fget: test_fget,
    };

    fn install_test_files() {
        // Idempotent: a repeated install of the same table is accepted.
        let _ = install_file_ops(&TEST_FILE_OPS);
    }

    /// Close a test descriptor: drop the table's reference.
    fn close_fd(fd: c_int) {
        let mut table = FD_TABLE.lock();
        if let Some(index) = table.iter().position(|(entry, _)| *entry == fd) {
            let (_, file) = table.remove(index);
            unsafe { fput_file(file as *mut File) };
        }
    }

    unsafe extern "C" fn test_fence_driver(_: *mut DmaFence) -> *const c_char {
        c"test-driver".as_ptr()
    }

    static TEST_FENCE_OPS: DmaFenceOps = DmaFenceOps {
        get_driver_name: Some(test_fence_driver),
        get_timeline_name: Some(test_fence_driver),
        enable_signaling: None,
        signaled: None,
        wait: None,
        release: None,
        set_deadline: None,
    };

    /// A pending fence in its own context, released with `dma_fence_put`.
    unsafe fn pending_fence(context: u64, seqno: u64) -> *mut DmaFence {
        let fence = kzalloc_obj::<DmaFence>();
        let lock: &'static mut Spinlock =
            alloc::boxed::Box::leak(alloc::boxed::Box::new(Spinlock::new()));
        unsafe {
            dma_fence_init(fence, ptr::addr_of!(TEST_FENCE_OPS), lock, context, seqno);
        }
        fence
    }

    /// Stub fence signaled at `timestamp` in the unbound context.
    unsafe fn signaled_fence(timestamp: i64) -> *mut DmaFence {
        unsafe { crate::dma_fence::dma_fence_allocate_private_stub(timestamp) }
    }

    #[test]
    fn sync_file_ioctl_numbers_match_the_uapi_header() {
        assert_eq!(SYNC_IOC_MERGE, 0xc030_3e03);
        assert_eq!(SYNC_IOC_FILE_INFO, 0xc038_3e04);
        assert_eq!(SYNC_IOC_SET_DEADLINE, 0x4010_3e05);
    }

    #[test]
    fn sync_file_create_fd_install_and_get_fence_round_trip() {
        install_test_files();
        unsafe {
            let fence = signaled_fence(7);
            let sync_file = sync_file_create(fence);
            assert!(!sync_file.is_null());
            let fd = get_unused_fd_flags(O_CLOEXEC);
            fd_install(fd, (*sync_file).file.cast());
            assert!(is_sync_file_file((*sync_file).file));
            // The fence the caller gets back is the one the sync_file holds.
            let got = sync_file_get_fence(fd);
            assert_eq!(got, fence);
            dma_fence_put(got);
            // A descriptor that is not a sync_file has no fence.
            assert!(sync_file_get_fence(FD_BASE + 99).is_null());
            close_fd(fd);
        }
    }

    #[test]
    fn sync_file_get_name_uses_user_name_or_the_fence_identity() {
        install_test_files();
        unsafe {
            let fence = pending_fence(42, 9);
            let sync_file = sync_file_create(fence);
            let mut buf = [0 as c_char; 64];
            sync_file_get_name(sync_file, buf.as_mut_ptr(), buf.len() as c_int);
            assert_eq!(CStr::from_ptr(buf.as_ptr()).to_bytes(), b"test-driver-test-driver42-9");

            let mut short = [0 as c_char; 8];
            sync_file_get_name(sync_file, short.as_mut_ptr(), short.len() as c_int);
            assert_eq!(CStr::from_ptr(short.as_ptr()).to_bytes(), b"test-dr");

            let merged = sync_file_merge(b"merged-name", sync_file, sync_file);
            assert!(!merged.is_null());
            sync_file_get_name(merged, buf.as_mut_ptr(), buf.len() as c_int);
            assert_eq!(CStr::from_ptr(buf.as_ptr()).to_bytes(), b"merged-name");
            fput_file((*merged).file);
            fput_file((*sync_file).file);
        }
    }

    #[test]
    fn sync_file_merge_keeps_pending_fence_and_drops_signaled_members() {
        unsafe {
            let pending = pending_fence(5, 1);
            let done = signaled_fence(1);
            let a = sync_file_create(pending);
            let b = sync_file_create(done);
            // One pending member: the merge returns that fence itself.
            let merged = sync_file_merge(b"", a, b);
            assert!(!merged.is_null());
            assert_eq!((*merged).fence, pending);
            assert!(!dma_fence_is_signaled((*merged).fence));
            fput_file((*merged).file);
            fput_file((*b).file);
            fput_file((*a).file);
        }
    }

    #[test]
    fn sync_file_merge_dedup_keeps_the_most_recent_fence_per_context() {
        unsafe {
            // Each array slot owns the reference the test created.
            let older = pending_fence(5, 1);
            let newer = pending_fence(5, 3);
            let mut fences = [dma_fence_get(older), dma_fence_get(newer)];
            assert_eq!(dma_fence_dedup_array(&mut fences), 1);
            assert_eq!(fences[0], newer);
        }
    }

    #[test]
    fn sync_file_poll_reports_readable_only_after_the_fence_signals() {
        install_test_files();
        unsafe {
            let fence = pending_fence(77, 1);
            let sync_file = sync_file_create(fence);
            let file = (*sync_file).file;
            assert_eq!(sync_file_poll(file, ptr::null_mut()), 0);
            dma_fence_signal(fence);
            assert_eq!(sync_file_poll(file, ptr::null_mut()), EPOLLIN);
            fput_file(file);
        }
    }

    #[test]
    fn sync_file_file_info_reports_status_and_members() {
        unsafe {
            let pending = pending_fence(11, 2);
            let done = signaled_fence(5);
            let a = sync_file_create(pending);
            let b = sync_file_create(done);
            let merged = sync_file_merge(b"pair", a, b);
            assert!(!merged.is_null());

            // Count-only query: no members copied, pending status.
            let count = sync_file_fence_info(merged, 0).ok().unwrap();
            assert_eq!(count.status, 0);
            assert_eq!(count.num_fences, 1);
            assert!(count.fences.is_empty());
            assert_eq!(&count.name[..5], b"pair\0");

            // Member query: the one pending member, reporting its driver.
            let info = sync_file_fence_info(merged, 1).ok().unwrap();
            assert_eq!(info.num_fences, 1);
            assert_eq!(info.fences.len(), 1);
            assert_eq!(&info.fences[0].driver_name[..11], b"test-driver");
            assert_eq!(info.fences[0].status, 0);
            assert_eq!(info.fences[0].timestamp_ns, 0);

            // A signaled sync_file reports its status and timestamp.
            let finished = sync_file_create(done);
            let finished_info = sync_file_fence_info(finished, 1).ok().unwrap();
            assert_eq!(finished_info.status, 1);
            assert_eq!(finished_info.fences[0].timestamp_ns, 5);

            fput_file((*finished).file);
            fput_file((*merged).file);
            fput_file((*b).file);
            fput_file((*a).file);
        }
    }
}
