// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! LinuxKPI DRM VMA offset manager (Linux 7.2.3 `drm_vma_manager.c` semantics).
//!
//! Offsets are allocated from the manager's `drm_mm` range. Each node keeps an
//! rb-tree of `drm_vma_offset_file` entries keyed by the file (tag) pointer,
//! which decides which open files may map the node.

#![allow(unsafe_code)]

use core::{
    ffi::{c_int, c_ulong, c_void},
    sync::atomic::AtomicU32,
};

use alloc::boxed::Box;

use crate::{
    intel_context_upstream::DrmVmaOffsetNode,
    intel_engine_cs_upstream::RbNode,
    linux::{
        drm_mm::{__drm_mm_interval_first, drm_mm_insert_node_in_range, drm_mm_remove_node},
        i915_private::DrmVmaOffsetManager,
        locks::{write_lock, write_unlock},
        rbtree::{rb_erase, rb_insert_color, rb_link_node},
    },
};

/// `DRM_MM_INSERT_BEST`, the mode of `drm_mm_insert_node()`.
const DRM_MM_INSERT_BEST: u32 = 0;

/// Linux `struct drm_vma_offset_file`: one open file allowed to map a node.
#[repr(C)]
struct DrmVmaOffsetFile {
    vm_rb: RbNode,
    vm_tag: *mut c_void,
    vm_count: u32,
}

#[inline]
unsafe fn node_lock(node: *mut DrmVmaOffsetNode) -> *mut AtomicU32 {
    // `vm_lock` is the rwlock at offset zero of `struct drm_vma_offset_node`.
    node.cast::<AtomicU32>()
}

/// Linux `drm_vma_offset_add()`: allocate `pages` of offset space for `node`.
/// Nodes that already have an offset keep it and return 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_vma_offset_add(
    mgr: *mut DrmVmaOffsetManager,
    node: *mut DrmVmaOffsetNode,
    pages: c_ulong,
) -> c_int {
    assert!(!mgr.is_null() && !node.is_null());
    unsafe {
        write_lock(core::ptr::addr_of_mut!((*mgr).vm_lock));
        let vm_node = core::ptr::addr_of_mut!((*node).vm_node);
        let mut ret = 0;
        if !crate::linux::gem_memory::drm_mm_node_allocated(vm_node) {
            ret = drm_mm_insert_node_in_range(
                core::ptr::addr_of_mut!((*mgr).vm_addr_space),
                vm_node,
                pages as u64,
                0,
                0,
                0,
                u64::MAX,
                DRM_MM_INSERT_BEST,
            );
        }
        write_unlock(core::ptr::addr_of_mut!((*mgr).vm_lock));
        ret
    }
}

/// Linux `drm_vma_offset_remove()`: release the offset space of `node`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_vma_offset_remove(
    mgr: *mut DrmVmaOffsetManager,
    node: *mut DrmVmaOffsetNode,
) {
    assert!(!mgr.is_null() && !node.is_null());
    unsafe {
        write_lock(core::ptr::addr_of_mut!((*mgr).vm_lock));
        let vm_node = core::ptr::addr_of_mut!((*node).vm_node);
        if crate::linux::gem_memory::drm_mm_node_allocated(vm_node) {
            drm_mm_remove_node(vm_node);
            core::ptr::write_bytes(vm_node, 0, 1);
        }
        write_unlock(core::ptr::addr_of_mut!((*mgr).vm_lock));
    }
}

/// Linux `drm_vma_offset_lookup_locked()`: the node whose offset range covers
/// `[start, start + pages)`, or NULL. The caller holds the manager lock.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_vma_offset_lookup_locked(
    mgr: *mut DrmVmaOffsetManager,
    start: c_ulong,
    pages: c_ulong,
) -> *mut DrmVmaOffsetNode {
    assert!(!mgr.is_null());
    let last = (start as u64).wrapping_add(pages as u64).wrapping_sub(1);
    let vm_node = unsafe {
        __drm_mm_interval_first(
            core::ptr::addr_of!((*mgr).vm_addr_space),
            start as u64,
            last,
        )
    };
    if vm_node.is_null() {
        return core::ptr::null_mut();
    }
    let spans = unsafe {
        (*vm_node).start <= start as u64
            && (*vm_node).start.wrapping_add((*vm_node).size) >= (start as u64).wrapping_add(pages as u64)
    };
    if spans {
        vm_node.cast::<u8>().wrapping_sub(core::mem::offset_of!(DrmVmaOffsetNode, vm_node)).cast()
    } else {
        core::ptr::null_mut()
    }
}

/// Find the `drm_vma_offset_file` for `tag` in `node`'s tree.
unsafe fn file_entry(node: *mut DrmVmaOffsetNode, tag: *mut c_void) -> *mut DrmVmaOffsetFile {
    unsafe {
        let mut link = (*node).vm_files.node;
        while !link.is_null() {
            let entry = link.cast::<u8>().wrapping_sub(core::mem::offset_of!(DrmVmaOffsetFile, vm_rb)).cast::<DrmVmaOffsetFile>();
            if tag == (*entry).vm_tag {
                return entry;
            }
            link = if (tag as usize) > ((*entry).vm_tag as usize) {
                (*link).right
            } else {
                (*link).left
            };
        }
        core::ptr::null_mut()
    }
}

/// Linux `vma_node_allow()`: grant `tag` access; a reference-counted grant
/// increments the count of an existing entry.
unsafe fn vma_node_allow(node: *mut DrmVmaOffsetNode, tag: *mut c_void, ref_counted: bool) -> c_int {
    unsafe {
        write_lock(node_lock(node));
        let mut ret = 0;
        let mut parent: *mut RbNode = core::ptr::null_mut();
        let mut link: *mut *mut RbNode = core::ptr::addr_of_mut!((*node).vm_files.node);
        let mut found = false;
        while !(*link).is_null() {
            parent = *link;
            let entry = parent.cast::<u8>().wrapping_sub(core::mem::offset_of!(DrmVmaOffsetFile, vm_rb)).cast::<DrmVmaOffsetFile>();
            if tag == (*entry).vm_tag {
                if ref_counted {
                    (*entry).vm_count += 1;
                }
                found = true;
                break;
            }
            link = if (tag as usize) > ((*entry).vm_tag as usize) {
                core::ptr::addr_of_mut!((*parent).right)
            } else {
                core::ptr::addr_of_mut!((*parent).left)
            };
        }
        if !found {
            let entry = Box::into_raw(Box::new(DrmVmaOffsetFile {
                vm_rb: RbNode {
                    parent_color: 0,
                    right: core::ptr::null_mut(),
                    left: core::ptr::null_mut(),
                },
                vm_tag: tag,
                vm_count: 1,
            }));
            rb_link_node(core::ptr::addr_of_mut!((*entry).vm_rb), parent, link);
            rb_insert_color(core::ptr::addr_of_mut!((*entry).vm_rb), core::ptr::addr_of_mut!((*node).vm_files));
        }
        write_unlock(node_lock(node));
        ret
    }
}

/// Linux `drm_vma_node_allow_once()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_vma_node_allow_once(node: *mut DrmVmaOffsetNode, file: *mut c_void) -> c_int {
    assert!(!node.is_null());
    unsafe { vma_node_allow(node, file, false) }
}

/// Linux `drm_vma_node_is_allowed()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_vma_node_is_allowed(node: *mut DrmVmaOffsetNode, file: *mut c_void) -> bool {
    assert!(!node.is_null());
    unsafe {
        write_lock(node_lock(node));
        let allowed = !file_entry(node, file).is_null();
        write_unlock(node_lock(node));
        allowed
    }
}

/// Linux `drm_vma_node_revoke()`: drop one grant of `file`, removing its entry
/// when the last grant goes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_vma_node_revoke(node: *mut DrmVmaOffsetNode, file: *mut c_void) {
    assert!(!node.is_null());
    unsafe {
        write_lock(node_lock(node));
        let entry = file_entry(node, file);
        if !entry.is_null() {
            if (*entry).vm_count > 1 {
                (*entry).vm_count -= 1;
            } else {
                rb_erase(core::ptr::addr_of_mut!((*entry).vm_rb), core::ptr::addr_of_mut!((*node).vm_files));
                drop(Box::from_raw(entry));
            }
        }
        write_unlock(node_lock(node));
    }
}
