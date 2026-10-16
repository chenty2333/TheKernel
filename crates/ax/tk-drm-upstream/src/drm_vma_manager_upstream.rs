// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_vma_manager.c` (batch BI).
//!
//! VMA offset manager for GPU memory mappings into user-space linear VMs.
//! Uses `drm_mm` to allocate page-aligned offsets and an rb-tree of
//! `drm_vma_offset_file` entries keyed by the file pointer to track mapping permissions.

#![allow(non_snake_case, dead_code, unsafe_code)]

use core::ffi::{c_int, c_ulong};
use alloc::boxed::Box;
use tk_linux_kapi::drm_mm::{drm_mm_init, drm_mm_insert_node_in_range, drm_mm_remove_node, drm_mm_takedown};
use tk_linux_kapi::gem_memory::drm_mm_node_allocated;
use tk_linux_kapi::locks::{read_lock, read_unlock, write_lock, write_unlock};
use tk_linux_kapi::rbtree::{rb_erase, rb_insert_color, rb_link_node};
use tk_linux_kapi::types::{RbNode, Rwlock};

use crate::drm_file_types::DrmFile;
use crate::drm_vma_manager_types::{DrmVmaOffsetFile, DrmVmaOffsetManager, DrmVmaOffsetNode};

/// `DRM_FILE_PAGE_OFFSET_START` (include/drm/drm_vma_manager.h, 64-bit): the
/// first fake page offset GEM mmap offsets are handed out from.
pub const DRM_FILE_PAGE_OFFSET_START: c_ulong = (0xFFFF_FFFFu64 >> 12) as c_ulong + 1;
/// `DRM_FILE_PAGE_OFFSET_SIZE` (include/drm/drm_vma_manager.h, 64-bit).
pub const DRM_FILE_PAGE_OFFSET_SIZE: c_ulong = (0xFFFF_FFFFu64 >> 12) as c_ulong * 256;

// upstream: drivers/gpu/drm/drm_vma_manager.c drm_vma_offset_manager_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_vma_offset_manager_init(
    mgr: *mut DrmVmaOffsetManager,
    page_offset: c_ulong,
    size: c_ulong,
) {
    if mgr.is_null() {
        return;
    }
    unsafe {
        core::ptr::write_bytes(core::ptr::addr_of_mut!((*mgr).vm_lock), 0, 1);
        drm_mm_init(core::ptr::addr_of_mut!((*mgr).vm_addr_space_mm), page_offset as u64, size as u64);
    }
}

// upstream: drivers/gpu/drm/drm_vma_manager.c drm_vma_offset_manager_destroy()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_vma_offset_manager_destroy(mgr: *mut DrmVmaOffsetManager) {
    if mgr.is_null() {
        return;
    }
    unsafe {
        drm_mm_takedown(core::ptr::addr_of_mut!((*mgr).vm_addr_space_mm));
    }
}

// upstream: drivers/gpu/drm/drm_vma_manager.c drm_vma_offset_lookup_locked()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_vma_offset_lookup_locked(
    mgr: *mut DrmVmaOffsetManager,
    start: c_ulong,
    pages: c_ulong,
) -> *mut DrmVmaOffsetNode {
    if mgr.is_null() {
        return core::ptr::null_mut();
    }
    unsafe {
        let mut iter = (*mgr).vm_addr_space_mm.interval_tree.root.node;
        let mut best: *mut tk_linux_kapi::types::DrmMmNode = core::ptr::null_mut();

        while !iter.is_null() {
            let node = iter.cast::<u8>().wrapping_sub(core::mem::offset_of!(tk_linux_kapi::types::DrmMmNode, rb)).cast::<tk_linux_kapi::types::DrmMmNode>();
            let offset = (*node).start;
            if (start as u64) >= offset {
                iter = (*iter).right;
                best = node;
                if (start as u64) == offset {
                    break;
                }
            } else {
                iter = (*iter).left;
            }
        }

        if !best.is_null() {
            let offset = (*best).start + (*best).size;
            if offset < (start as u64) + (pages as u64) {
                best = core::ptr::null_mut();
            }
        }

        if best.is_null() {
            core::ptr::null_mut()
        } else {
            best.cast::<u8>().wrapping_sub(core::mem::offset_of!(DrmVmaOffsetNode, vm_node)).cast()
        }
    }
}

// upstream: drivers/gpu/drm/drm_vma_manager.c drm_vma_offset_add()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_vma_offset_add(
    mgr: *mut DrmVmaOffsetManager,
    node: *mut DrmVmaOffsetNode,
    pages: c_ulong,
) -> c_int {
    if mgr.is_null() || node.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    unsafe {
        let mut ret = 0;
        write_lock(core::ptr::addr_of_mut!((*mgr).vm_lock).cast());
        if !drm_mm_node_allocated(core::ptr::addr_of_mut!((*node).vm_node)) {
            ret = drm_mm_insert_node_in_range(
                core::ptr::addr_of_mut!((*mgr).vm_addr_space_mm),
                core::ptr::addr_of_mut!((*node).vm_node),
                pages as u64,
                0,
                0,
                0,
                u64::MAX,
                0,
            );
        }
        write_unlock(core::ptr::addr_of_mut!((*mgr).vm_lock).cast());
        ret
    }
}

// upstream: drivers/gpu/drm/drm_vma_manager.c drm_vma_offset_remove()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_vma_offset_remove(
    mgr: *mut DrmVmaOffsetManager,
    node: *mut DrmVmaOffsetNode,
) {
    if mgr.is_null() || node.is_null() {
        return;
    }
    unsafe {
        write_lock(core::ptr::addr_of_mut!((*mgr).vm_lock).cast());
        if drm_mm_node_allocated(core::ptr::addr_of_mut!((*node).vm_node)) {
            drm_mm_remove_node(core::ptr::addr_of_mut!((*node).vm_node));
            core::ptr::write_bytes(core::ptr::addr_of_mut!((*node).vm_node), 0, 1);
        }
        write_unlock(core::ptr::addr_of_mut!((*mgr).vm_lock).cast());
    }
}

// upstream: drivers/gpu/drm/drm_vma_manager.c vma_node_allow()
unsafe fn vma_node_allow(
    node: *mut DrmVmaOffsetNode,
    tag: *mut DrmFile,
    ref_counted: bool,
) -> c_int {
    unsafe {
        let mut ret = 0;
        let mut parent: *mut RbNode = core::ptr::null_mut();
        let mut found = false;

        write_lock(core::ptr::addr_of_mut!((*node).vm_lock).cast());
        let mut link = core::ptr::addr_of_mut!((*node).vm_files.node);

        while !(*link).is_null() {
            parent = *link;
            let entry = parent.cast::<u8>().wrapping_sub(core::mem::offset_of!(DrmVmaOffsetFile, vm_rb)).cast::<DrmVmaOffsetFile>();
            if tag == (*entry).vm_tag {
                if ref_counted {
                    (*entry).vm_count += 1;
                }
                found = true;
                break;
            } else if (tag as usize) > ((*entry).vm_tag as usize) {
                link = core::ptr::addr_of_mut!((*parent).right);
            } else {
                link = core::ptr::addr_of_mut!((*parent).left);
            }
        }

        if !found {
            let new_entry = Box::into_raw(Box::new(DrmVmaOffsetFile {
                vm_rb: RbNode {
                    parent_color: 0,
                    right: core::ptr::null_mut(),
                    left: core::ptr::null_mut(),
                },
                vm_tag: tag,
                vm_count: 1,
            }));
            rb_link_node(core::ptr::addr_of_mut!((*new_entry).vm_rb), parent, link);
            rb_insert_color(core::ptr::addr_of_mut!((*new_entry).vm_rb), core::ptr::addr_of_mut!((*node).vm_files));
        }

        write_unlock(core::ptr::addr_of_mut!((*node).vm_lock).cast());
        ret
    }
}

// upstream: drivers/gpu/drm/drm_vma_manager.c drm_vma_node_allow()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_vma_node_allow(node: *mut DrmVmaOffsetNode, tag: *mut DrmFile) -> c_int {
    if node.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    unsafe { vma_node_allow(node, tag, true) }
}

// upstream: drivers/gpu/drm/drm_vma_manager.c drm_vma_node_allow_once()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_vma_node_allow_once(node: *mut DrmVmaOffsetNode, tag: *mut DrmFile) -> c_int {
    if node.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    unsafe { vma_node_allow(node, tag, false) }
}

// upstream: drivers/gpu/drm/drm_vma_manager.c drm_vma_node_revoke()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_vma_node_revoke(node: *mut DrmVmaOffsetNode, tag: *mut DrmFile) {
    if node.is_null() {
        return;
    }
    unsafe {
        write_lock(core::ptr::addr_of_mut!((*node).vm_lock).cast());
        let mut iter = (*node).vm_files.node;
        while !iter.is_null() {
            let entry = iter.cast::<u8>().wrapping_sub(core::mem::offset_of!(DrmVmaOffsetFile, vm_rb)).cast::<DrmVmaOffsetFile>();
            if tag == (*entry).vm_tag {
                (*entry).vm_count -= 1;
                if (*entry).vm_count == 0 {
                    rb_erase(core::ptr::addr_of_mut!((*entry).vm_rb), core::ptr::addr_of_mut!((*node).vm_files));
                    drop(Box::from_raw(entry));
                }
                break;
            } else if (tag as usize) > ((*entry).vm_tag as usize) {
                iter = (*iter).right;
            } else {
                iter = (*iter).left;
            }
        }
        write_unlock(core::ptr::addr_of_mut!((*node).vm_lock).cast());
    }
}

// upstream: drivers/gpu/drm/drm_vma_manager.c drm_vma_node_is_allowed()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_vma_node_is_allowed(node: *mut DrmVmaOffsetNode, tag: *mut DrmFile) -> bool {
    if node.is_null() {
        return false;
    }
    unsafe {
        read_lock(core::ptr::addr_of_mut!((*node).vm_lock).cast());
        let mut iter = (*node).vm_files.node;
        while !iter.is_null() {
            let entry = iter.cast::<u8>().wrapping_sub(core::mem::offset_of!(DrmVmaOffsetFile, vm_rb)).cast::<DrmVmaOffsetFile>();
            if tag == (*entry).vm_tag {
                break;
            } else if (tag as usize) > ((*entry).vm_tag as usize) {
                iter = (*iter).right;
            } else {
                iter = (*iter).left;
            }
        }
        read_unlock(core::ptr::addr_of_mut!((*node).vm_lock).cast());
        !iter.is_null()
    }
}

// upstream: include/drm/drm_vma_manager.h drm_vma_node_has_offset()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_vma_node_has_offset(node: *const DrmVmaOffsetNode) -> bool {
    if node.is_null() {
        return false;
    }
    unsafe { drm_mm_node_allocated(core::ptr::addr_of!((*node).vm_node)) }
}

// upstream: include/drm/drm_vma_manager.h drm_vma_node_start()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_vma_node_start(node: *const DrmVmaOffsetNode) -> c_ulong {
    if node.is_null() {
        return 0;
    }
    unsafe { (*node).vm_node.start as c_ulong }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vma_manager_allocation_and_permissions() {
        unsafe {
            let mut mgr: DrmVmaOffsetManager = core::mem::zeroed();
            drm_vma_offset_manager_init(&mut mgr, 0x1000, 0x10000);

            let mut node: DrmVmaOffsetNode = core::mem::zeroed();
            assert!(!drm_vma_node_has_offset(&node));

            assert_eq!(drm_vma_offset_add(&mut mgr, &mut node, 4), 0);
            assert!(drm_vma_node_has_offset(&node));
            let start = drm_vma_node_start(&node);
            assert!(start >= 0x1000);

            // Lookup
            let found = drm_vma_offset_lookup_locked(&mut mgr, start, 4);
            assert_eq!(found, &mut node as *mut _);

            // Permissions
            let dummy_file = 0x5555usize as *mut DrmFile;
            assert!(!drm_vma_node_is_allowed(&mut node, dummy_file));
            assert_eq!(drm_vma_node_allow(&mut node, dummy_file), 0);
            assert!(drm_vma_node_is_allowed(&mut node, dummy_file));
            drm_vma_node_revoke(&mut node, dummy_file);
            assert!(!drm_vma_node_is_allowed(&mut node, dummy_file));

            // Remove
            drm_vma_offset_remove(&mut mgr, &mut node);
            assert!(!drm_vma_node_has_offset(&node));

            drm_vma_offset_manager_destroy(&mut mgr);
        }
    }
}
