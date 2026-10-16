// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See the repository MIT license.

//! Userspace `mmap()` of i915 GEM objects through the upstream DRM file.
//!
//! This is the VFS-facing half of Linux `i915_gem_mman.c`. `i915_gem_mmap()`
//! resolves the file offset through `dev->vma_offset_manager` to an
//! `i915_mmap_offset` (mmo), requires `drm_vma_node_is_allowed()` for the
//! calling file, and `i915_gem_object_mmap()` chooses the CPU cache type from
//! the mmo. The Rust VFS cannot fault pages into a `vm_area_struct`, so the
//! object's pages are pinned up front (`i915_object_page_set`) and published as
//! fixed shared pages. The cache type is applied to that backing through
//! `PreparedFileMmap::with_cpu_cache_flags`, the same mechanism the DRM cache
//! mappings use.

use alloc::{sync::Arc, vec::Vec};
use core::{ffi::c_ulong, mem::offset_of, ptr};

use axerrno::{AxError, AxResult};
use axhal::paging::MappingFlags;
use intel_gt::{
    i915_gem_mman_upstream::i915_object_page_set,
    i915_gem_object_header_upstream::{
        i915_gem_object_get_rcu, i915_gem_object_is_readonly, i915_gem_object_put,
    },
    i915_gem_object_types_upstream::{DrmI915GemObject, I915MmapOffset, I915MmapType},
    i915_gem_object_upstream::i915_gem_object_has_struct_page,
};
use memory_addr::PhysAddr;
use tk_drm_upstream::{
    drm_device_types::DrmDevice,
    drm_file_types::DrmFile,
    drm_vma_manager_upstream::{drm_vma_node_is_allowed, drm_vma_offset_lookup_locked},
};
use tk_linux_kapi::locks::{read_lock, read_unlock};

use crate::{
    file::{FileMmapProtection, FileMmapRequest, FixedSharedMmapRegion, PreparedFileMmap},
    mm::SharedPages,
};

const PAGE_SIZE: usize = 4096;

/// A GEM object named by an mmo, with one GEM reference owned by this value.
struct MmoTarget {
    object: ObjectRef,
    mmap_type: I915MmapType,
    node_start: u64,
}

/// One `i915_gem_object_get_rcu()` reference, released on drop.
struct ObjectRef(*mut DrmI915GemObject);

impl Drop for ObjectRef {
    fn drop(&mut self) {
        // SAFETY: `self.0` holds the reference taken by `lookup_mmo`; releasing it
        // is the matching `i915_gem_object_put()`.
        unsafe { i915_gem_object_put(self.0) };
    }
}

/// Linux `i915_gem_object_mmap()` cache selection for an mmo type.
///
/// `FIXED` is `GEM_WARN_ON` in Linux and falls through to WB, which is the
/// placement it takes for the system-memory objects this path accepts. GTT
/// mmos need GGTT aperture faulting and `i915_gem_object_release_mmap_gtt()`
/// revocation, which a fixed shared-page backing cannot provide, so they are
/// declined with `EOPNOTSUPP`.
fn cpu_cache_flags(mmap_type: I915MmapType) -> AxResult<MappingFlags> {
    match mmap_type {
        I915MmapType::I915_MMAP_TYPE_WB | I915MmapType::I915_MMAP_TYPE_FIXED => {
            Ok(MappingFlags::empty())
        }
        I915MmapType::I915_MMAP_TYPE_WC => Ok(MappingFlags::WRITE_COMBINING),
        I915MmapType::I915_MMAP_TYPE_UC => Ok(MappingFlags::UNCACHED),
        I915MmapType::I915_MMAP_TYPE_GTT => Err(AxError::OperationNotSupported),
    }
}

/// Maps a negative-or-positive Linux errno from the i915 translation to `AxError`.
fn errno_to_ax(err: i32) -> AxError {
    match err.checked_abs().unwrap_or(i32::MAX) {
        12 => AxError::NoMemory,
        13 => AxError::PermissionDenied,
        19 => AxError::NoSuchDevice,
        22 => AxError::InvalidInput,
        95 => AxError::OperationNotSupported,
        _ => AxError::Io,
    }
}

/// Linux `i915_gem_mmap()` lookup, run under the VMA-offset manager read lock.
///
/// # Safety
/// `dev` must be the live DRM device with a non-null `vma_offset_manager`, and
/// `private` must be the calling file's `drm_file` (or null).
unsafe fn lookup_mmo(
    dev: *mut DrmDevice,
    private: *mut DrmFile,
    first_page: u64,
    map_pages: usize,
) -> AxResult<MmoTarget> {
    // SAFETY: `dev` is live and its manager is non-null per this function's contract.
    let manager = unsafe { (*dev).vma_offset_manager };
    // SAFETY: `manager` is a valid `drm_vma_offset_manager` with an initialized rwlock.
    let manager_lock = unsafe { ptr::addr_of_mut!((*manager).vm_lock).cast() };
    // SAFETY: `manager_lock` is the valid rwlock of the manager; it is released below.
    unsafe { read_lock(manager_lock) };
    // SAFETY: the manager lock is held, so the node cannot be removed during the lookup.
    let node = unsafe {
        drm_vma_offset_lookup_locked(manager, first_page as c_ulong, map_pages as c_ulong)
    };
    let found = if node.is_null() {
        Err(AxError::InvalidInput)
    } else if private.is_null()
        // SAFETY: `node` is the non-null node just looked up under the manager lock, and
        // `private` is the caller's live drm_file.
        || !unsafe { drm_vma_node_is_allowed(node, private) }
    {
        Err(AxError::PermissionDenied)
    } else {
        // SAFETY: `node` is non-null and still under the manager lock.
        if unsafe { !(*node).driver_private.is_null() } {
            // Linux's `mmap_ops` nodes live in `DrmGemObject`; no i915 object in this
            // port installs them, so such a node is not a GEM mmap offset here.
            Err(AxError::OperationNotSupported)
        } else {
            // SAFETY: a node without `driver_private` is the `vma_node` of an
            // `i915_mmap_offset`, so the containing-struct arithmetic is in bounds.
            let mmo = unsafe {
                node.cast::<u8>()
                    .wrapping_sub(offset_of!(I915MmapOffset, vma_node))
                    .cast::<I915MmapOffset>()
            };
            // SAFETY: `mmo` is the embedded mmo; its `obj` and `mmap_type` are set at
            // attach time and immutable afterwards. The object is retained below
            // under the same lock, so it cannot be freed before `get_rcu`.
            let (obj, mmap_type, node_start) =
                unsafe { ((*mmo).obj, (*mmo).mmap_type, (*node).vm_node.start) };
            // SAFETY: `obj` is a non-null object of this device (checked here); taking a
            // reference while the manager lock is held prevents concurrent destruction.
            let owned = if obj.is_null()
                || unsafe { (&(*obj).base.base).dev.cast::<DrmDevice>() != dev }
            {
                ptr::null_mut()
            } else {
                unsafe { i915_gem_object_get_rcu(obj) }
            };
            if owned.is_null() {
                Err(AxError::PermissionDenied)
            } else {
                Ok(MmoTarget {
                    object: ObjectRef(owned),
                    mmap_type,
                    node_start,
                })
            }
        }
    };
    // SAFETY: `manager_lock` was acquired above and is released exactly once here.
    unsafe { read_unlock(manager_lock) };
    found
}

/// Prepare a `MAP_SHARED` mapping of an i915 GEM object named by a VMA offset.
///
/// `dev` is the device that owns the file, `private` the file's `drm_file`, and
/// `writable` the file's `allows_write()`. The caller has already rejected
/// execute, private and non-4 KiB requests.
///
/// The returned plan keeps the pinned pages (and so the GEM object) alive for
/// each VMA fragment; the last fragment's unmap releases the pin.
pub(crate) fn prepare_i915_gem_mmap(
    dev: *mut DrmDevice,
    private: *mut DrmFile,
    writable: bool,
    request: FileMmapRequest,
) -> AxResult<PreparedFileMmap> {
    let first_page = request.offset() / PAGE_SIZE as u64;
    let map_pages = request.length() / PAGE_SIZE;
    // SAFETY: the caller passes the live device and file it was dispatched for; the
    // manager is non-null because `upstream_gem_mmap` checked it before dispatch.
    let target = unsafe { lookup_mmo(dev, private, first_page, map_pages) }?;
    let object = target.object.0;

    // SAFETY: `object` is held by `target.object` for this scope.
    let (size, readonly) = unsafe {
        (
            (&(*object).base.base).size as u64,
            i915_gem_object_is_readonly(object),
        )
    };
    if size == 0 || !size.is_multiple_of(PAGE_SIZE as u64) {
        return Err(AxError::InvalidInput);
    }
    let region_offset = target
        .node_start
        .checked_mul(PAGE_SIZE as u64)
        .ok_or(AxError::InvalidInput)?;
    let relative_offset = request
        .offset()
        .checked_sub(region_offset)
        .ok_or(AxError::InvalidInput)?;
    if relative_offset
        .checked_add(request.length() as u64)
        .is_none_or(|end| end > size)
    {
        return Err(AxError::InvalidInput);
    }

    let wants_write = request.protection().contains(FileMmapProtection::WRITE);
    if wants_write && !writable {
        return Err(AxError::PermissionDenied);
    }
    // Linux `i915_gem_object_mmap()`: VM_WRITE on a readonly object is EINVAL, and
    // readonly objects never gain VM_MAYWRITE.
    if wants_write && readonly {
        return Err(AxError::InvalidInput);
    }
    let cache = cpu_cache_flags(target.mmap_type)?;
    // Device-local (lmem) placements need `remap_io_sg()` onto iomem; this path
    // only publishes RAM pages.
    // SAFETY: `object` is held by `target.object`.
    if !unsafe { i915_gem_object_has_struct_page(object) } {
        return Err(AxError::OperationNotSupported);
    }

    // SAFETY: `object` is live and held; `i915_object_page_set` takes its own GEM
    // reference and pin, and the returned owner releases both on drop.
    let pages_set = unsafe { i915_object_page_set(object) }.map_err(errno_to_ax)?;
    let page_count = size as usize / PAGE_SIZE;
    if pages_set.physical_pages().len() != page_count {
        return Err(AxError::InvalidInput);
    }
    let mut physical = Vec::new();
    physical
        .try_reserve_exact(page_count)
        .map_err(|_| AxError::NoMemory)?;
    physical.extend(
        pages_set
            .physical_pages()
            .iter()
            .map(|&page| PhysAddr::from_usize(page)),
    );
    let owner = Arc::try_new(pages_set).map_err(|_| AxError::NoMemory)?;
    let owner: Arc<dyn Send + Sync> = owner;
    // SAFETY: every entry is a pinned 4 KiB cacheable RAM page of the i915 object
    // (`has_struct_page` above), and `owner` holds the GEM reference and pin until
    // the final `SharedPages` clone is dropped.
    let shared = unsafe { SharedPages::new_page_cache_4k(physical, owner) }?;
    let pages = Arc::try_new(shared).map_err(|_| AxError::NoMemory)?;
    // A readonly object or read-only file can never be made writable by mprotect.
    let may_protect = if writable && !readonly {
        FileMmapProtection::READ | FileMmapProtection::WRITE
    } else {
        FileMmapProtection::READ
    };
    let region = FixedSharedMmapRegion::try_new(region_offset, pages, may_protect)?;
    let plan = region.prepare(request)?.ok_or(AxError::InvalidInput)?;
    plan.with_cpu_cache_flags(cache)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mmo_types_select_linux_cache_attributes() {
        assert_eq!(
            cpu_cache_flags(I915MmapType::I915_MMAP_TYPE_WB).unwrap(),
            MappingFlags::empty()
        );
        assert_eq!(
            cpu_cache_flags(I915MmapType::I915_MMAP_TYPE_FIXED).unwrap(),
            MappingFlags::empty()
        );
        assert_eq!(
            cpu_cache_flags(I915MmapType::I915_MMAP_TYPE_WC).unwrap(),
            MappingFlags::WRITE_COMBINING
        );
        assert_eq!(
            cpu_cache_flags(I915MmapType::I915_MMAP_TYPE_UC).unwrap(),
            MappingFlags::UNCACHED
        );
        assert!(matches!(
            cpu_cache_flags(I915MmapType::I915_MMAP_TYPE_GTT),
            Err(AxError::OperationNotSupported)
        ));
    }

    #[test]
    fn negative_intel_errnos_map_to_vfs_errors() {
        assert_eq!(errno_to_ax(-12), AxError::NoMemory);
        assert_eq!(errno_to_ax(-13), AxError::PermissionDenied);
        assert_eq!(errno_to_ax(-19), AxError::NoSuchDevice);
        assert_eq!(errno_to_ax(-22), AxError::InvalidInput);
        assert_eq!(errno_to_ax(-95), AxError::OperationNotSupported);
        assert_eq!(errno_to_ax(-5), AxError::Io);
    }
}
