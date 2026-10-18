// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! File-backed Linux DMA-BUF core operations.
//!
//! This is the small lifetime and attachment core needed by DRM PRIME.  The
//! `struct dma_buf` ABI record remains in `drm_prime_upstream`; its file is an
//! actual LinuxKPI anonymous file, so fd lookup and object lifetime use the
//! kernel's file table and file reference count rather than a parallel fd map.

#![allow(non_snake_case)]

use core::{
    ffi::{c_int, c_long, c_void},
    mem::size_of,
    ptr,
};

use tk_linux_kapi::{
    config::{EFAULT, EINVAL, ENOMEM, EOPNOTSUPP, ERR_PTR, GFP_KERNEL, IS_ERR, PTR_ERR},
    dma_resv::{dma_resv_fini, dma_resv_init, dma_resv_wait_timeout},
    kernel_services::{anon_inode_getfile, fget},
    list::{INIT_LIST_HEAD, list_add_tail, list_del_init, list_empty},
    locks::spin_lock_init,
    memory::{kfree, kmalloc},
    mm_native::copy_from_user,
    shmem::{File as ShmemFile, FileOperations, Inode, fput_file, get_file_file},
    types::{DmaResv, SgTable},
    ww_mutex::{ww_mutex_lock, ww_mutex_unlock},
};

use crate::drm_prime_upstream::{DmaBuf, DmaBufAttachment, DmaBufExportInfo};

const EOVERFLOW: i32 = 75;
const ETIME: i32 = 62;
const EBADF: i32 = 9;
const ENOTTY: i32 = 25;
const DMA_BIDIRECTIONAL: c_int = 0;
const DMA_TO_DEVICE: c_int = 1;
const DMA_FROM_DEVICE: c_int = 2;
const DMA_RESV_USAGE_WRITE: c_int = 1;
const DMA_RESV_USAGE_READ: c_int = 2;
const DMA_BUF_SYNC_READ: u64 = 1;
const DMA_BUF_SYNC_WRITE: u64 = 2;
const DMA_BUF_SYNC_RW: u64 = DMA_BUF_SYNC_READ | DMA_BUF_SYNC_WRITE;
const DMA_BUF_SYNC_END: u64 = 4;
const DMA_BUF_SYNC_VALID_FLAGS: u64 = DMA_BUF_SYNC_RW | DMA_BUF_SYNC_END;

const fn ioc(dir: u32, kind: u32, nr: u32, size: usize) -> u32 {
    (dir << 30) | ((size as u32) << 16) | (kind << 8) | nr
}

/// `uapi/linux/dma-buf.h` DMA-BUF sync ioctl payload.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DmaBufSync {
    pub flags: u64,
}

/// Linux `_IOW(DMA_BUF_BASE, 0, struct dma_buf_sync)`.
pub const DMA_BUF_IOCTL_SYNC: u32 = ioc(1, b'b' as u32, 0, size_of::<DmaBufSync>());

const _: () = {
    assert!(DMA_BUF_IOCTL_SYNC == 0x4008_6200);
};

/// File identity used by `dma_buf_get()`; unlike an fd registry this survives
/// dup/close semantics through the actual LinuxKPI file reference count.
static DMA_BUF_FOPS: FileOperations = FileOperations {
    release: Some(dma_buf_file_release),
    ..FileOperations::empty()
};

/// Resolve a file reference to its DMA-BUF object only when it is owned by the
/// DMA-BUF anonymous-file implementation. Other anon-inode users (notably
/// sync_file) store unrelated data in `private_data` and must not be cast.
///
/// # Safety
/// `file` must be null, an error pointer, or a live `shmem::File` reference.
/// The caller must retain that reference for as long as it uses the returned
/// DMA-BUF pointer; this helper does not acquire an additional reference.
pub unsafe fn drm_dma_buf_from_file(file: *mut ShmemFile) -> Option<*mut DmaBuf> {
    if file.is_null() || IS_ERR(file) {
        return None;
    }

    // SAFETY: `file` is confirmed non-null and not an error pointer; the caller holds an open
    // reference to the ShmemFile across this call, making immutable shared dereference sound.
    let file_ref = unsafe { file.as_ref() }?;
    if file_ref.f_op != ptr::addr_of!(DMA_BUF_FOPS) {
        return None;
    }

    let dmabuf = file_ref._private_data.cast::<DmaBuf>();
    if dmabuf.is_null() || IS_ERR(dmabuf) {
        return None;
    }
    // SAFETY: `file.f_op` matched `DMA_BUF_FOPS`, so `_private_data` points to the `DmaBuf`
    // allocated during export whose lifetime is tied to `file`.
    let dmabuf_ref = unsafe { &*dmabuf };
    if dmabuf_ref.file.cast::<ShmemFile>() != file {
        return None;
    }

    Some(dmabuf)
}

/// Test whether a live LinuxKPI file belongs to this DMA-BUF core.
///
/// # Safety
/// `file` must be null or a live `ShmemFile` reference.
pub unsafe fn is_dma_buf_file(file: *mut ShmemFile) -> bool {
    // SAFETY: forwarded caller contract; this helper only inspects the file's fops identity.
    unsafe { drm_dma_buf_from_file(file) }.is_some()
}

/// Return the exported buffer's size for `fstat()` on a DMA-BUF descriptor.
///
/// # Safety
/// `file` must be null or a live `ShmemFile` reference.
pub unsafe fn dma_buf_file_size(file: *mut ShmemFile) -> Option<u64> {
    // SAFETY: forwarded caller contract; returned buffer remains valid while `file` is live.
    let dmabuf = unsafe { drm_dma_buf_from_file(file) }?;
    // SAFETY: `dmabuf` came from the live DMA-BUF file checked above.
    Some(unsafe { (*dmabuf).size as u64 })
}

/// Dispatch `DMA_BUF_IOCTL_SYNC` for a live DMA-BUF anonymous file.
///
/// This is intentionally a VFS-facing helper: LinuxKPI's file-operation
/// table is an ABI prefix, so the kernel routes anonymous-file ioctl dispatch
/// by fops identity instead of adding a parallel descriptor implementation.
///
/// # Safety
/// `file` must be a live `ShmemFile` reference. For `DMA_BUF_IOCTL_SYNC`,
/// `arg` must be a userspace address readable for one [`DmaBufSync`].
pub unsafe fn dma_buf_ioctl(file: *mut ShmemFile, cmd: u32, arg: usize) -> c_long {
    // SAFETY: forwarded caller contract; the returned object is kept alive by the file ref.
    let Some(dmabuf) = (unsafe { drm_dma_buf_from_file(file) }) else {
        return -ENOTTY as c_long;
    };
    if cmd != DMA_BUF_IOCTL_SYNC {
        return -ENOTTY as c_long;
    }

    let mut sync = DmaBufSync::default();
    // SAFETY: this ioctl's ABI defines `arg` as a userspace pointer to DmaBufSync.
    if unsafe {
        copy_from_user(
            ptr::addr_of_mut!(sync).cast(),
            arg as *const c_void,
            size_of::<DmaBufSync>(),
        )
    } != 0
    {
        return -EFAULT as c_long;
    }
    if sync.flags & !DMA_BUF_SYNC_VALID_FLAGS != 0 {
        return -EINVAL as c_long;
    }
    let direction = match sync.flags & DMA_BUF_SYNC_RW {
        DMA_BUF_SYNC_READ => DMA_TO_DEVICE,
        DMA_BUF_SYNC_WRITE => DMA_FROM_DEVICE,
        DMA_BUF_SYNC_RW => DMA_BIDIRECTIONAL,
        _ => return -EINVAL as c_long,
    };
    let result = if sync.flags & DMA_BUF_SYNC_END != 0 {
        // SAFETY: `dmabuf` is owned by the live file reference and direction was validated.
        unsafe { dma_buf_end_cpu_access(dmabuf, direction) }
    } else {
        // SAFETY: `dmabuf` is owned by the live file reference and direction was validated.
        unsafe { dma_buf_begin_cpu_access(dmabuf, direction) }
    };
    result as c_long
}

/// Acquire an additional DMA-BUF reference using its backing file, matching
/// Linux's inline `get_dma_buf()` helper.
///
/// # Safety
/// `dmabuf` must be null/ERR_PTR or a live DMA-BUF pointer whose backing file reference
/// is retained by the caller.
pub unsafe fn get_dma_buf(dmabuf: *mut DmaBuf) {
    if dmabuf.is_null() || IS_ERR(dmabuf) {
        return;
    }
    // SAFETY: `dmabuf` is verified non-null and not ERR_PTR; the caller holds an active
    // reference to the DMA-BUF, so `(*dmabuf).file` is valid and safely readable.
    let file = unsafe { (*dmabuf).file.cast::<ShmemFile>() };
    if !file.is_null() && !IS_ERR(file) {
        // SAFETY: `file` was read from the live DMA-BUF; incrementing its reference count via get_file_file.
        unsafe { get_file_file(file) };
    }
}

/// Free the allocation made by `dma_buf_export()` after the backing anon file
/// reaches its final reference.
///
/// # Safety
///
/// `file` must be null or point to a live `ShmemFile` entering release whose reference count
/// has dropped to zero.
unsafe extern "C" fn dma_buf_file_release(_inode: *mut Inode, file: *mut ShmemFile) -> c_int {
    // SAFETY: `file` is checked for null before dereferencing `(*file).f_op`.
    if file.is_null() || unsafe { (*file).f_op } != ptr::addr_of!(DMA_BUF_FOPS) {
        return -EINVAL;
    }
    // SAFETY: `file` is confirmed non-null and backed by DMA_BUF_FOPS, so `_private_data`
    // contains the exported DmaBuf pointer.
    let dmabuf = unsafe { (*file)._private_data.cast::<DmaBuf>() };
    if dmabuf.is_null() {
        return -EINVAL;
    }

    // The exporter callback releases its private object before the public
    // wrapper and an optional in-place reservation object are freed.
    // SAFETY: `dmabuf` belongs to this file whose refcount reached zero; no concurrent
    // thread can access `dmabuf`. Calling the exporter release callback, finalizing any
    // embedded reservation object, and freeing via `kfree` is sound.
    unsafe {
        if let Some(release) = (*dmabuf).ops.as_ref().and_then(|ops| ops.release) {
            release(dmabuf);
        }
        if (*dmabuf).resv == dmabuf.add(1).cast::<DmaResv>() {
            dma_resv_fini((*dmabuf).resv);
        }
        kfree(dmabuf);
    }
    0
}

/// Linux `dma_buf_export()`: create an anonymous file whose final put owns the
/// exporter release callback and DMA-BUF allocation.
#[unsafe(no_mangle)]
/// # Safety
/// - `exp_info` must be null or point to a readable, properly aligned `DmaBufExportInfo` struct.
/// - If non-null, `(*exp_info).ops` must point to a valid `DmaBufOps` table with required callbacks
///   (`map_dma_buf`, `unmap_dma_buf`, `release`, and matched `pin`/`unpin`).
/// - `(*exp_info).r#priv`, if non-null, must point to exporter-managed private state that outlives this buffer.
/// - `(*exp_info).resv`, if non-null, must point to an initialized `DmaResv` object outliving the buffer.
/// - `(*exp_info).exp_name`, if non-null, must point to a valid null-terminated C string.
pub unsafe extern "C" fn dma_buf_export(exp_info: *const DmaBufExportInfo) -> *mut DmaBuf {
    if exp_info.is_null() {
        return ERR_PTR(-EINVAL);
    }
    // SAFETY: `exp_info` was checked non-null and properly aligned; it points to an initialized
    // `DmaBufExportInfo` value whose memory remains valid and unmodified for the duration of export.
    let info = unsafe { &*exp_info };
    if info.ops.is_null() || IS_ERR(info.ops) {
        return ERR_PTR(-EINVAL);
    }
    // SAFETY: `info.ops` was checked non-null and non-error; it points to a static or exporter-owned
    // `DmaBufOps` operations table that remains immutable and valid across buffer lifetime.
    let ops = unsafe { info.ops.as_ref() };
    let Some(ops) = ops else {
        return ERR_PTR(-EINVAL);
    };

    // Linux requires these exporter operations and requires pin/unpin to be
    // supplied as a pair. Optional CPU access callbacks are intentionally
    // optional: coherent exporters use the Linux-defined successful no-op.
    if ops.map_dma_buf.is_none()
        || ops.unmap_dma_buf.is_none()
        || ops.release.is_none()
        || ops.pin.is_some() != ops.unpin.is_some()
    {
        return ERR_PTR(-EINVAL);
    }

    let Some(alloc_size) = size_of::<DmaBuf>().checked_add(size_of::<DmaResv>()) else {
        return ERR_PTR(-EOVERFLOW);
    };
    let dmabuf = kmalloc(alloc_size, GFP_KERNEL).cast::<DmaBuf>();
    if dmabuf.is_null() {
        return ERR_PTR(-ENOMEM);
    }
    // SAFETY: `dmabuf` was just allocated with `alloc_size` bytes; initializing fields and optional in-place DmaResv.
    unsafe {
        ptr::write_bytes(dmabuf.cast::<u8>(), 0, alloc_size);
        (*dmabuf).size = info.size;
        (*dmabuf).ops = info.ops;
        (*dmabuf).exp_name = info.exp_name;
        (*dmabuf).owner = info.owner;
        (*dmabuf).r#priv = info.r#priv;
        (*dmabuf).resv = if info.resv.is_null() {
            let resv = dmabuf.add(1).cast::<DmaResv>();
            dma_resv_init(resv);
            resv
        } else {
            info.resv
        };
        INIT_LIST_HEAD(ptr::addr_of_mut!((*dmabuf).attachments));
        INIT_LIST_HEAD(ptr::addr_of_mut!((*dmabuf).list_node));
        spin_lock_init(&mut (*dmabuf).name_lock);
    }

    let name = if info.exp_name.is_null() {
        c"dma_buf".as_ptr()
    } else {
        info.exp_name
    };
    // SAFETY: `dmabuf` is initialized and `DMA_BUF_FOPS` is a static operations table.
    let file = unsafe {
        anon_inode_getfile(
            name,
            ptr::addr_of!(DMA_BUF_FOPS).cast(),
            dmabuf.cast(),
            info.flags,
        )
    };
    if IS_ERR(file) {
        let error = PTR_ERR(file);
        // SAFETY: `file` creation failed, clean up the allocated `dmabuf`.
        unsafe { kfree(dmabuf) };
        return ERR_PTR(error);
    }
    // SAFETY: `dmabuf` points to the newly allocated DmaBuf and `file` is the valid ShmemFile
    // returned by anon_inode_getfile; no other thread has access to `dmabuf` yet.
    unsafe { (*dmabuf).file = file.cast() };
    dmabuf
}

/// Linux `dma_buf_get()`: returns an owned file reference or an `ERR_PTR`.
#[unsafe(no_mangle)]
/// # Safety
/// - `fd` must be an integer file descriptor within the current process table.
/// - On success, returns the `DmaBuf` pointer stored in the file's private data. The caller
///   acquires an owned file reference and must eventually call [`dma_buf_put`] to release it.
pub unsafe extern "C" fn dma_buf_get(fd: c_int) -> *mut DmaBuf {
    // SAFETY: `fget` safely performs table bounds-checking and reference acquisition on `fd`.
    let file = unsafe { fget(fd) };
    if file.is_null() {
        return ERR_PTR(-EBADF);
    }
    // SAFETY: `file` was acquired via `fget`; reading `(*file).f_op` is safe while holding that reference.
    if unsafe { (*file).f_op } != ptr::addr_of!(DMA_BUF_FOPS) {
        // SAFETY: `file` is a non-null ShmemFile reference acquired above; releasing it upon type mismatch.
        unsafe { fput_file(file) };
        return ERR_PTR(-EINVAL);
    }
    // SAFETY: `file.f_op` matched `DMA_BUF_FOPS`, so `_private_data` contains the DmaBuf pointer set during export.
    let dmabuf = unsafe { (*file)._private_data.cast::<DmaBuf>() };
    if dmabuf.is_null() {
        // SAFETY: `file` is an owned reference acquired above; releasing it before returning error.
        unsafe { fput_file(file) };
        return ERR_PTR(-EINVAL);
    }
    dmabuf
}

/// Linux `dma_buf_put()`: drop the file reference returned by export/get.
#[unsafe(no_mangle)]
/// # Safety
/// - `dmabuf` can be null or an `ERR_PTR` (in which case this function is a no-op).
/// - If non-null and non-error, `dmabuf` must point to a live `DmaBuf` previously obtained from
///   [`dma_buf_export`] or [`dma_buf_get`].
/// - The caller relinquishes one file reference count corresponding to `dmabuf`. If this drops
///   the refcount to zero, the buffer is freed and `dmabuf` must not be dereferenced again.
pub unsafe extern "C" fn dma_buf_put(dmabuf: *mut DmaBuf) {
    if dmabuf.is_null() || IS_ERR(dmabuf) {
        return;
    }
    // SAFETY: `dmabuf` is verified non-null and not ERR_PTR; the caller holds an active
    // reference to the buffer, ensuring `(*dmabuf).file` can be safely read.
    let file = unsafe { (*dmabuf).file.cast::<ShmemFile>() };
    if !file.is_null() && !IS_ERR(file) {
        // SAFETY: `file` is the backing anon file of `dmabuf`; dropping one acquired reference.
        unsafe { fput_file(file) };
    }
}

/// Linux `dma_buf_attach()`: allocate and link a static device attachment.
#[unsafe(no_mangle)]
/// # Safety
/// - `dmabuf` must be null/ERR_PTR or point to a live, initialized `DmaBuf` whose reference is held by the caller.
/// - `dev` must be a non-null pointer to a device struct (e.g. `struct device`) that remains valid until
///   [`dma_buf_detach`] is called.
/// - On success, the caller acquires ownership of the returned `DmaBufAttachment` and must free it via [`dma_buf_detach`].
pub unsafe extern "C" fn dma_buf_attach(
    dmabuf: *mut DmaBuf,
    dev: *mut c_void,
) -> *mut DmaBufAttachment {
    if dmabuf.is_null() || IS_ERR(dmabuf) || dev.is_null() || unsafe { (*dmabuf).resv.is_null() } {
        return ERR_PTR(-EINVAL);
    }
    // SAFETY: `dmabuf` is verified non-null and not ERR_PTR; the caller-held reference guarantees
    // `(*dmabuf).ops` is valid and immutable.
    let Some(ops) = (unsafe { (*dmabuf).ops.as_ref() }) else {
        return ERR_PTR(-EINVAL);
    };
    let attach = tk_linux_kapi::memory::kzalloc_obj::<DmaBufAttachment>();
    if attach.is_null() {
        return ERR_PTR(-ENOMEM);
    }
    // SAFETY: `attach` was freshly allocated via kzalloc_obj and is uniquely owned.
    // Initializing fields and calling the exporter callback before linking the
    // attachment; Linux protects the attachment list with the shared resv lock.
    unsafe {
        (*attach).dmabuf = dmabuf;
        (*attach).dev = dev;
        INIT_LIST_HEAD(ptr::addr_of_mut!((*attach).node));
        if let Some(attach_op) = ops.attach {
            let ret = attach_op(dmabuf, attach);
            if ret != 0 {
                kfree(attach);
                return ERR_PTR(ret);
            }
        }
    }
    // SAFETY: the export contract guarantees a live initialized reservation
    // object; acquire it only for the attachment-list mutation, not callbacks.
    if let Err(error) = unsafe { lock_reservation(dmabuf) } {
        // The exporter attach callback succeeded, so undo its private setup on
        // this later failure before freeing the unpublished attachment.
        unsafe {
            if let Some(detach) = (*dmabuf).ops.as_ref().and_then(|ops| ops.detach) {
                detach(dmabuf, attach);
            }
            kfree(attach);
        }
        return ERR_PTR(error);
    }
    // SAFETY: the DMA-BUF reservation lock is held for this list mutation.
    unsafe {
        list_add_tail(
            ptr::addr_of_mut!((*attach).node),
            ptr::addr_of_mut!((*dmabuf).attachments),
        );
    }
    // SAFETY: matching unlock for lock_reservation above.
    unsafe { unlock_reservation(dmabuf) };
    attach
}

/// Linux `dma_buf_detach()`: unlink the attachment, notify the exporter, free
/// the attachment record.
#[unsafe(no_mangle)]
/// # Safety
/// - `dmabuf` can be null or ERR_PTR (no-op).
/// - `attach` can be null or ERR_PTR (no-op).
/// - If non-null, `attach` must point to a live `DmaBufAttachment` created by [`dma_buf_attach`] on `dmabuf`.
/// - All scatter-gather mappings created via [`dma_buf_map_attachment_unlocked`] for `attach`
///   must have been unmapped via [`dma_buf_unmap_attachment_unlocked`] prior to detachment.
/// - `attach` is deallocated by this function and must not be accessed after this call.
pub unsafe extern "C" fn dma_buf_detach(dmabuf: *mut DmaBuf, attach: *mut DmaBufAttachment) {
    if dmabuf.is_null() || IS_ERR(dmabuf) || attach.is_null() || IS_ERR(attach) {
        return;
    }
    // SAFETY: `attach` is verified non-null and not ERR_PTR; reading `(*attach).dmabuf` is safe
    // while the caller retains ownership of the attachment.
    if unsafe { (*attach).dmabuf != dmabuf } {
        return;
    }
    // SAFETY: `attach` is confirmed attached to `dmabuf`; lock the shared
    // reservation around the same attachment-list mutation used by map paths.
    if unsafe { lock_reservation(dmabuf) }.is_err() {
        return;
    }
    unsafe {
        if list_empty(&*ptr::addr_of!((*attach).node)) {
            unlock_reservation(dmabuf);
            return;
        }
        list_del_init(ptr::addr_of_mut!((*attach).node));
        unlock_reservation(dmabuf);
    }
    // Exporter callbacks run outside the reservation lock, matching the
    // attachment API's callback contract.
    unsafe {
        if let Some(detach) = (*dmabuf).ops.as_ref().and_then(|ops| ops.detach) {
            detach(dmabuf, attach);
        }
        kfree(attach);
    }
}

/// Internal reservation-lock wrapper used by the unlocked map/unmap exports.
///
/// # Safety
///
/// `dmabuf` must be null, ERR_PTR, or a valid `DmaBuf` pointer whose `resv` points to an initialized `DmaResv`.
unsafe fn lock_reservation(dmabuf: *mut DmaBuf) -> Result<(), c_int> {
    // SAFETY: `dmabuf` is checked for null/ERR_PTR before reading its `resv` field.
    if dmabuf.is_null() || IS_ERR(dmabuf) || unsafe { (*dmabuf).resv.is_null() } {
        return Err(-EINVAL);
    }
    // SAFETY: `dmabuf` has a non-null `resv` containing an initialized ww_mutex lock.
    let ret = unsafe { ww_mutex_lock(ptr::addr_of_mut!((*(*dmabuf).resv).lock), ptr::null_mut()) };
    if ret != 0 { Err(ret) } else { Ok(()) }
}

/// # Safety
///
/// `dmabuf` must point to a valid `DmaBuf` with a non-null reservation object whose ww_mutex lock
/// was locked by the calling thread.
unsafe fn unlock_reservation(dmabuf: *mut DmaBuf) {
    // SAFETY: `dmabuf` and its `resv` are valid, and the reservation ww_mutex was acquired by
    // the calling context and is now unlocked.
    unsafe { ww_mutex_unlock(ptr::addr_of_mut!((*(*dmabuf).resv).lock)) };
}

/// Return the live DMA-BUF held by an attachment.
unsafe fn attachment_dmabuf(attach: *mut DmaBufAttachment) -> Option<*mut DmaBuf> {
    if attach.is_null() || IS_ERR(attach) {
        return None;
    }
    // SAFETY: `attach` is verified non-null and non-error before reading its owner pointer.
    let dmabuf = unsafe { (*attach).dmabuf };
    if dmabuf.is_null() || IS_ERR(dmabuf) {
        None
    } else {
        Some(dmabuf)
    }
}

/// Map an attachment while its DMA-BUF reservation lock is already held.
///
/// # Safety
/// `attach` must be a live attachment and the caller must hold its DMA-BUF's
/// reservation lock for the complete exporter pin/map operation.
unsafe fn map_attachment_locked(attach: *mut DmaBufAttachment, direction: c_int) -> *mut SgTable {
    // SAFETY: the caller guarantees a live attachment; the helper still
    // handles null/error pointers defensively at the C boundary.
    let Some(dmabuf) = (unsafe { attachment_dmabuf(attach) }) else {
        return ERR_PTR(-EINVAL);
    };
    // SAFETY: `dmabuf` is a valid pointer whose ops table is guaranteed valid by the exporter.
    let Some(ops) = (unsafe { (*dmabuf).ops.as_ref() }) else {
        return ERR_PTR(-EINVAL);
    };
    let Some(map) = ops.map_dma_buf else {
        return ERR_PTR(-EOPNOTSUPP);
    };
    if ops.unmap_dma_buf.is_none() {
        return ERR_PTR(-EOPNOTSUPP);
    }

    let mut pinned = false;
    let mut result = 0;
    if let Some(pin) = ops.pin {
        // SAFETY: `attach` is valid and the caller holds the buffer reservation
        // lock as required by exporter pin.
        result = unsafe { pin(attach) };
        pinned = result == 0;
    }
    let sgt = if result == 0 {
        // SAFETY: `attach` is valid and the caller holds the buffer reservation
        // lock as required by exporter map.
        unsafe { map(attach, direction) }
    } else {
        ERR_PTR(result)
    };
    if IS_ERR(sgt) || sgt.is_null() {
        if pinned && let Some(unpin) = ops.unpin {
            // SAFETY: `attach` is valid and the caller holds the buffer reservation
            // lock as required by exporter unpin.
            unsafe { unpin(attach) };
        }
        return if sgt.is_null() { ERR_PTR(-EINVAL) } else { sgt };
    }
    sgt
}

/// Unmap an attachment while its DMA-BUF reservation lock is already held.
///
/// # Safety
/// The caller must hold the live attachment's DMA-BUF reservation lock and
/// pass the scatterlist returned by the matching map operation.
unsafe fn unmap_attachment_locked(
    attach: *mut DmaBufAttachment,
    sgt: *mut SgTable,
    direction: c_int,
) {
    if attach.is_null() || IS_ERR(attach) || sgt.is_null() || IS_ERR(sgt) {
        return;
    }
    // SAFETY: `attach` is checked before reading its owning DMA-BUF pointer.
    let Some(dmabuf) = (unsafe { attachment_dmabuf(attach) }) else {
        return;
    };
    // SAFETY: `dmabuf` is verified non-null and not an error pointer; reading `(*dmabuf).ops`.
    let Some(ops) = (unsafe { (*dmabuf).ops.as_ref() }) else {
        return;
    };
    let Some(unmap) = ops.unmap_dma_buf else {
        return;
    };
    // SAFETY: caller holds the reservation lock required by exporter unmap/unpin.
    unsafe {
        unmap(attach, sgt, direction);
        if let Some(unpin) = ops.unpin {
            unpin(attach);
        }
    }
}

/// Linux `dma_buf_map_attachment_unlocked()`: acquire the reservation lock and
/// dispatch the exporter map operation.
#[unsafe(no_mangle)]
/// # Safety
/// Same attachment and direction contract as [`dma_buf_map_attachment`].
pub unsafe extern "C" fn dma_buf_map_attachment_unlocked(
    attach: *mut DmaBufAttachment,
    direction: c_int,
) -> *mut SgTable {
    // SAFETY: the helper validates the attachment before reading its DMA-BUF.
    let Some(dmabuf) = (unsafe { attachment_dmabuf(attach) }) else {
        return ERR_PTR(-EINVAL);
    };
    // SAFETY: lock the owning DMA-BUF reservation around exporter operations.
    if let Err(error) = unsafe { lock_reservation(dmabuf) } {
        return ERR_PTR(error);
    }
    // SAFETY: reservation lock is held for the complete callback sequence.
    let sgt = unsafe { map_attachment_locked(attach, direction) };
    // SAFETY: matching unlock for the lock_reservation call above.
    unsafe { unlock_reservation(dmabuf) };
    sgt
}

/// Linux `dma_buf_map_attachment()`: the importer-held-reservation API used by
/// i915's GEM get-pages path.
#[unsafe(no_mangle)]
/// # Safety
/// - `attach` must be a live attachment whose DMA-BUF reservation lock is
///   already held by the caller.
/// - `direction` must be a valid DMA data direction.
/// - The returned scatterlist must be passed to [`dma_buf_unmap_attachment`]
///   with the same attachment and direction while that lock remains held.
pub unsafe extern "C" fn dma_buf_map_attachment(
    attach: *mut DmaBufAttachment,
    direction: c_int,
) -> *mut SgTable {
    // SAFETY: this public entry point preserves Linux's caller-held lock contract.
    unsafe { map_attachment_locked(attach, direction) }
}

/// Linux `dma_buf_unmap_attachment_unlocked()`: acquire the reservation lock
/// and dispatch the exporter unmap operation.
#[unsafe(no_mangle)]
/// # Safety
/// Same attachment, scatterlist, and direction contract as
/// [`dma_buf_unmap_attachment`].
pub unsafe extern "C" fn dma_buf_unmap_attachment_unlocked(
    attach: *mut DmaBufAttachment,
    sgt: *mut SgTable,
    direction: c_int,
) {
    // SAFETY: the helper validates the attachment before reading its DMA-BUF.
    let Some(dmabuf) = (unsafe { attachment_dmabuf(attach) }) else {
        return;
    };
    // SAFETY: lock the owning DMA-BUF reservation around exporter operations.
    if unsafe { lock_reservation(dmabuf) }.is_err() {
        return;
    }
    // SAFETY: reservation lock is held for the complete callback sequence.
    unsafe { unmap_attachment_locked(attach, sgt, direction) };
    // SAFETY: matching unlock for the lock_reservation call above.
    unsafe { unlock_reservation(dmabuf) };
}

/// Linux `dma_buf_unmap_attachment()`: i915's paired caller-locked unmap API.
#[unsafe(no_mangle)]
/// # Safety
/// Same caller-held reservation-lock contract as [`dma_buf_map_attachment`].
pub unsafe extern "C" fn dma_buf_unmap_attachment(
    attach: *mut DmaBufAttachment,
    sgt: *mut SgTable,
    direction: c_int,
) {
    // SAFETY: this public entry point preserves Linux's caller-held lock contract.
    unsafe { unmap_attachment_locked(attach, sgt, direction) };
}

/// Linux `dma_buf_begin_cpu_access()`: run exporter cache preparation and wait
/// for all implicit fences relevant to this access direction. An absent
/// callback is permitted for exporters that need no cache maintenance.
#[unsafe(no_mangle)]
/// # Safety
/// - `dmabuf` must be null/ERR_PTR or point to a live `DmaBuf` whose reference is retained by the caller.
/// - `direction` must be a valid DMA direction constant (`DMA_BIDIRECTIONAL`, `DMA_TO_DEVICE`, or `DMA_FROM_DEVICE`).
/// - Every successful call to `dma_buf_begin_cpu_access` must eventually be balanced by a call to
///   [`dma_buf_end_cpu_access`] with the same direction before device access resumes.
pub unsafe extern "C" fn dma_buf_begin_cpu_access(dmabuf: *mut DmaBuf, direction: c_int) -> c_int {
    if dmabuf.is_null() || IS_ERR(dmabuf) {
        return -EINVAL;
    }
    // SAFETY: `dmabuf` is verified non-null and not an error pointer; reading `(*dmabuf).ops`.
    let Some(ops) = (unsafe { (*dmabuf).ops.as_ref() }) else {
        return -EINVAL;
    };
    let ret = match ops.begin_cpu_access {
        // SAFETY: `dmabuf` is valid and the optional exporter callback is invoked with caller's direction.
        Some(begin) => unsafe { begin(dmabuf, direction) },
        None => 0,
    };
    if ret != 0 {
        return ret;
    }
    let usage = if direction == DMA_BIDIRECTIONAL || direction == DMA_TO_DEVICE {
        DMA_RESV_USAGE_READ
    } else {
        DMA_RESV_USAGE_WRITE
    };
    // SAFETY: `dmabuf` is verified non-null; reading its `resv` field.
    let resv = unsafe { (*dmabuf).resv };
    if resv.is_null() {
        return -EINVAL;
    }
    // SAFETY: `resv` is a non-null, initialized reservation object outliving the caller-retained buffer.
    let waited = unsafe {
        dma_resv_wait_timeout(
            resv.cast(),
            usage,
            true,
            tk_linux_kapi::config::MAX_SCHEDULE_TIMEOUT as i64,
        )
    };
    if waited < 0 {
        waited as c_int
    } else if waited == 0 {
        -ETIME
    } else {
        0
    }
}

/// Linux `dma_buf_end_cpu_access()`. An absent callback is permitted by the
/// DMA-BUF API for exporters that need no cache maintenance.
#[unsafe(no_mangle)]
/// # Safety
/// - `dmabuf` must be null/ERR_PTR or point to a live `DmaBuf` whose reference is retained by the caller.
/// - `direction` must match the direction passed to a preceding successful call to [`dma_buf_begin_cpu_access`].
pub unsafe extern "C" fn dma_buf_end_cpu_access(dmabuf: *mut DmaBuf, direction: c_int) -> c_int {
    if dmabuf.is_null() || IS_ERR(dmabuf) {
        return -EINVAL;
    }
    // SAFETY: `dmabuf` is verified non-null and not an error pointer; reading `(*dmabuf).ops`.
    let Some(ops) = (unsafe { (*dmabuf).ops.as_ref() }) else {
        return -EINVAL;
    };
    match ops.end_cpu_access {
        // SAFETY: `dmabuf` is valid and the optional exporter callback is invoked with caller's direction.
        Some(end) => unsafe { end(dmabuf, direction) },
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use core::{
        ffi::{c_int, c_void},
        sync::atomic::{AtomicUsize, Ordering},
    };

    use tk_linux_kapi::{
        config::{EINVAL, ERR_PTR, IS_ERR},
        memory::{kfree, kmalloc},
        types::{Scatterlist, SgTable},
    };

    use super::*;
    use crate::drm_prime_upstream::DmaBufOps;

    static RELEASES: AtomicUsize = AtomicUsize::new(0);
    static ATTACHES: AtomicUsize = AtomicUsize::new(0);
    static DETACHES: AtomicUsize = AtomicUsize::new(0);
    static UNMAPS: AtomicUsize = AtomicUsize::new(0);
    static BEGINS: AtomicUsize = AtomicUsize::new(0);
    static ENDS: AtomicUsize = AtomicUsize::new(0);

    /// # Safety
    ///
    /// Mock test callback; `_dmabuf` and `_attach` are passed by the test caller.
    unsafe extern "C" fn attach(_dmabuf: *mut DmaBuf, _attach: *mut DmaBufAttachment) -> c_int {
        ATTACHES.fetch_add(1, Ordering::SeqCst);
        0
    }

    /// # Safety
    ///
    /// Mock test callback; `_dmabuf` and `_attach` are passed by the test caller.
    unsafe extern "C" fn detach(_dmabuf: *mut DmaBuf, _attach: *mut DmaBufAttachment) {
        DETACHES.fetch_add(1, Ordering::SeqCst);
    }

    /// # Safety
    ///
    /// Mock test callback; `_attach` is passed by the test caller.
    unsafe extern "C" fn map(_attach: *mut DmaBufAttachment, _direction: c_int) -> *mut SgTable {
        let bytes = size_of::<SgTable>() + size_of::<Scatterlist>();
        let sgt = kmalloc(bytes, GFP_KERNEL).cast::<SgTable>();
        if sgt.is_null() {
            return ERR_PTR(-ENOMEM);
        }
        // SAFETY: pointer arithmetic within the single allocated block for SgTable + Scatterlist.
        let sgl = unsafe {
            sgt.cast::<u8>()
                .add(size_of::<SgTable>())
                .cast::<Scatterlist>()
        };
        // SAFETY: `sgt` and `sgl` point to valid allocated storage.
        unsafe {
            sgt.write(SgTable {
                sgl,
                nents: 1,
                orig_nents: 1,
            });
            sgl.write(Scatterlist {
                page_link: 0,
                offset: 0,
                length: 4096,
                dma_address: 0x1000,
                dma_length: 4096,
                _dma_flags_or_padding: 0,
            });
        }
        sgt
    }

    /// # Safety
    ///
    /// `sgt` must point to an `SgTable` previously allocated in `map`.
    unsafe extern "C" fn unmap(
        _attach: *mut DmaBufAttachment,
        sgt: *mut SgTable,
        _direction: c_int,
    ) {
        // SAFETY: `sgt` was initialized in `map` with 1 nent.
        assert_eq!(unsafe { (*sgt).nents }, 1);
        // SAFETY: `sgt.sgl` was initialized in `map`.
        assert_eq!(unsafe { (*(*sgt).sgl).dma_address }, 0x1000);
        UNMAPS.fetch_add(1, Ordering::SeqCst);
        // SAFETY: `sgt` was allocated with kmalloc in `map`.
        unsafe { kfree(sgt) };
    }

    /// # Safety
    ///
    /// Mock test callback; `_dmabuf` is passed by the test caller.
    unsafe extern "C" fn release(_dmabuf: *mut DmaBuf) {
        RELEASES.fetch_add(1, Ordering::SeqCst);
    }

    /// # Safety
    ///
    /// Mock test callback; `_dmabuf` is passed by the test caller.
    unsafe extern "C" fn begin(_dmabuf: *mut DmaBuf, _direction: c_int) -> c_int {
        BEGINS.fetch_add(1, Ordering::SeqCst);
        0
    }

    /// # Safety
    ///
    /// Mock test callback; `_dmabuf` is passed by the test caller.
    unsafe extern "C" fn end(_dmabuf: *mut DmaBuf, _direction: c_int) -> c_int {
        ENDS.fetch_add(1, Ordering::SeqCst);
        0
    }

    static TEST_OPS: DmaBufOps = DmaBufOps {
        attach: Some(attach),
        detach: Some(detach),
        pin: None,
        unpin: None,
        map_dma_buf: Some(map),
        unmap_dma_buf: Some(unmap),
        release: Some(release),
        begin_cpu_access: Some(begin),
        end_cpu_access: Some(end),
        mmap: None,
        vmap: None,
        vunmap: None,
    };

    #[test]
    fn file_refs_attachments_mapping_and_cpu_access_follow_exporter_ops() {
        let _context = crate::b2b_tests::init_test_env();
        let _serial = crate::drm_syncobj_upstream::test_file_ops::serial();
        crate::drm_syncobj_upstream::test_file_ops::install();
        RELEASES.store(0, Ordering::SeqCst);
        ATTACHES.store(0, Ordering::SeqCst);
        DETACHES.store(0, Ordering::SeqCst);
        UNMAPS.store(0, Ordering::SeqCst);
        BEGINS.store(0, Ordering::SeqCst);
        ENDS.store(0, Ordering::SeqCst);

        let info = DmaBufExportInfo {
            exp_name: c"kdrm-core-test".as_ptr(),
            owner: ptr::null_mut(),
            ops: &TEST_OPS,
            size: 4096,
            flags: 0,
            resv: ptr::null_mut(),
            r#priv: 1usize as *mut c_void,
        };
        // SAFETY: `info` is a valid test export descriptor.
        let dmabuf = unsafe { dma_buf_export(&info) };
        assert!(!IS_ERR(dmabuf));
        // SAFETY: `dmabuf` is non-null and holds a valid backing file.
        let file = unsafe { (*dmabuf).file.cast::<ShmemFile>() };
        // SAFETY: `file` is valid ShmemFile for `dmabuf`.
        assert_eq!(unsafe { drm_dma_buf_from_file(file) }, Some(dmabuf));
        // SAFETY: `file` remains live through the export reference above.
        assert!(unsafe { is_dma_buf_file(file) });
        // SAFETY: `file` remains live through the export reference above.
        assert_eq!(unsafe { dma_buf_file_size(file) }, Some(4096));

        // SAFETY: obtaining unused file descriptor in test harness.
        let fd = unsafe { tk_linux_kapi::kernel_services::get_unused_fd_flags(0) };
        assert!(fd >= 0);
        // SAFETY: installing the DMA-BUF file into the test fd table.
        unsafe { tk_linux_kapi::kernel_services::fd_install(fd, (*dmabuf).file.cast()) };

        // SAFETY: retrieving DMA-BUF from installed fd.
        let got = unsafe { dma_buf_get(fd) };
        assert_eq!(got, dmabuf);
        // SAFETY: dropping acquired reference.
        unsafe { dma_buf_put(got) };
        assert_eq!(RELEASES.load(Ordering::SeqCst), 0);

        // SAFETY: attaching mock device to `dmabuf`.
        let attach = unsafe { dma_buf_attach(dmabuf, 0x4000usize as *mut c_void) };
        assert!(!IS_ERR(attach));
        assert_eq!(ATTACHES.load(Ordering::SeqCst), 1);
        // SAFETY: testing cpu access begin.
        assert_eq!(unsafe { dma_buf_begin_cpu_access(dmabuf, 0) }, 0);
        // SAFETY: testing cpu access end.
        assert_eq!(unsafe { dma_buf_end_cpu_access(dmabuf, 0) }, 0);
        assert_eq!(BEGINS.load(Ordering::SeqCst), 1);
        assert_eq!(ENDS.load(Ordering::SeqCst), 1);
        let mut sync = DmaBufSync {
            flags: DMA_BUF_SYNC_READ,
        };
        // SAFETY: `file` is a live DMA-BUF file and `sync` is a valid local payload.
        assert_eq!(
            unsafe {
                dma_buf_ioctl(
                    file,
                    DMA_BUF_IOCTL_SYNC,
                    (&mut sync as *mut DmaBufSync) as usize,
                )
            },
            0
        );
        assert_eq!(BEGINS.load(Ordering::SeqCst), 2);
        sync.flags = DMA_BUF_SYNC_WRITE | DMA_BUF_SYNC_END;
        // SAFETY: `file` is a live DMA-BUF file and `sync` is a valid local payload.
        assert_eq!(
            unsafe {
                dma_buf_ioctl(
                    file,
                    DMA_BUF_IOCTL_SYNC,
                    (&mut sync as *mut DmaBufSync) as usize,
                )
            },
            0
        );
        assert_eq!(ENDS.load(Ordering::SeqCst), 2);
        // SAFETY: testing mapping attachment.
        let sgt = unsafe { dma_buf_map_attachment_unlocked(attach, 0) };
        assert!(!IS_ERR(sgt));
        // SAFETY: testing the matching unlocked unmap entry point.
        unsafe { dma_buf_unmap_attachment_unlocked(attach, sgt, 0) };
        assert_eq!(UNMAPS.load(Ordering::SeqCst), 1);

        // The non-unlocked API requires the importer to hold the shared
        // reservation lock; i915's GEM get/put-pages callbacks do so through
        // the imported object's aliased `resv`.
        let resv_lock = unsafe { ptr::addr_of_mut!((*(*dmabuf).resv).lock) };
        // SAFETY: the test owns the reservation and no other context holds it.
        assert_eq!(unsafe { ww_mutex_lock(resv_lock, ptr::null_mut()) }, 0);
        // SAFETY: the matching reservation lock is held across this legacy map.
        let sgt = unsafe { dma_buf_map_attachment(attach, 0) };
        assert!(!IS_ERR(sgt));
        // SAFETY: unmapping uses the same attachment, direction, and held lock.
        unsafe { dma_buf_unmap_attachment(attach, sgt, 0) };
        // SAFETY: release the reservation lock acquired above.
        unsafe { ww_mutex_unlock(resv_lock) };
        assert_eq!(UNMAPS.load(Ordering::SeqCst), 2);
        // SAFETY: testing detaching.
        unsafe { dma_buf_detach(dmabuf, attach) };
        assert_eq!(DETACHES.load(Ordering::SeqCst), 1);

        // SAFETY: getting second reference to test fd cleanup.
        let outstanding = unsafe { dma_buf_get(fd) };
        crate::drm_syncobj_upstream::test_file_ops::close_fd(fd);
        assert_eq!(RELEASES.load(Ordering::SeqCst), 0);
        // SAFETY: final put triggers release.
        unsafe { dma_buf_put(outstanding) };
        assert_eq!(RELEASES.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn export_rejects_incomplete_ops_and_get_rejects_other_file_types() {
        let _context = crate::b2b_tests::init_test_env();
        let _serial = crate::drm_syncobj_upstream::test_file_ops::serial();
        crate::drm_syncobj_upstream::test_file_ops::install();
        let bad_ops = DmaBufOps {
            attach: Some(attach),
            detach: Some(detach),
            pin: None,
            unpin: None,
            map_dma_buf: None,
            unmap_dma_buf: Some(unmap),
            release: Some(release),
            begin_cpu_access: Some(begin),
            end_cpu_access: Some(end),
            mmap: None,
            vmap: None,
            vunmap: None,
        };
        let info = DmaBufExportInfo {
            exp_name: c"invalid".as_ptr(),
            owner: ptr::null_mut(),
            ops: &bad_ops,
            size: 4096,
            flags: 0,
            resv: ptr::null_mut(),
            r#priv: 1usize as *mut c_void,
        };
        // SAFETY: testing export rejection of incomplete ops.
        let err = unsafe { dma_buf_export(&info) };
        assert!(IS_ERR(err));
        assert_eq!(tk_linux_kapi::config::PTR_ERR(err), -EINVAL);

        let fd = crate::drm_syncobj_upstream::test_file_ops::install_plain_file();
        // SAFETY: testing get on non-DMA-BUF file.
        let err = unsafe { dma_buf_get(fd) };
        assert!(IS_ERR(err));
        assert_eq!(tk_linux_kapi::config::PTR_ERR(err), -EINVAL);

        // SAFETY: testing fget on plain file.
        let file = unsafe { tk_linux_kapi::kernel_services::fget(fd) };
        assert!(!file.is_null());
        // SAFETY: testing drm_dma_buf_from_file returns None for plain file.
        assert_eq!(unsafe { drm_dma_buf_from_file(file) }, None);
        // SAFETY: dropping plain file reference.
        unsafe { fput_file(file) };
        crate::drm_syncobj_upstream::test_file_ops::close_fd(fd);
    }
}
