// Copyright © 2017 Intel Corporation.
// Source-faithful Rust translation of Linux v7.2.3
// drivers/gpu/drm/i915/i915_syncmap.c (MIT).
#![allow(non_snake_case, non_camel_case_types, unsafe_op_in_unsafe_fn)]

use core::{ffi::c_void, mem::size_of, ptr};

use crate::{
    intel_timeline_types_upstream::I915Syncmap,
    linux::memory::{kfree, kmalloc, kzalloc},
    linux_config::GFP_KERNEL,
};

const KSYNCMAP: usize = 16;
const SHIFT: u32 = 4;
const MASK: usize = KSYNCMAP - 1;

#[repr(C)]
struct SyncmapNode {
    prefix: u64,
    height: u32,
    bitmap: u32,
    parent: *mut SyncmapNode,
    data: [u8; 0],
}

const _: [(); 24] = [(); size_of::<SyncmapNode>()];

#[inline]
unsafe fn sync_seqno(node: *mut SyncmapNode) -> *mut u32 {
    unsafe { node.cast::<u8>().add(size_of::<SyncmapNode>()).cast() }
}

#[inline]
unsafe fn sync_child(node: *mut SyncmapNode) -> *mut *mut SyncmapNode {
    unsafe { node.cast::<u8>().add(size_of::<SyncmapNode>()).cast() }
}

// upstream: i915_syncmap.c i915_syncmap_init()
pub unsafe fn i915_syncmap_init(root: *mut *mut I915Syncmap) {
    unsafe { *root = ptr::null_mut() };
}

// upstream: i915_syncmap.c __sync_seqno()
#[inline]
unsafe fn __sync_seqno(node: *mut SyncmapNode) -> *mut u32 {
    GEM_BUG_ON!(unsafe { (*node).height != 0 });
    unsafe { sync_seqno(node) }
}

// upstream: i915_syncmap.c __sync_child()
#[inline]
unsafe fn __sync_child(node: *mut SyncmapNode) -> *mut *mut SyncmapNode {
    GEM_BUG_ON!(unsafe { (*node).height == 0 });
    unsafe { sync_child(node) }
}

// upstream: i915_syncmap.c __sync_branch_idx()
#[inline]
unsafe fn __sync_branch_idx(node: *const SyncmapNode, id: u64) -> usize {
    ((id >> unsafe { (*node).height }) as usize) & MASK
}

// upstream: i915_syncmap.c __sync_leaf_idx()
#[inline]
unsafe fn __sync_leaf_idx(node: *const SyncmapNode, id: u64) -> usize {
    GEM_BUG_ON!(unsafe { (*node).height != 0 });
    (id as usize) & MASK
}

// upstream: i915_syncmap.c __sync_branch_prefix()
#[inline]
unsafe fn __sync_branch_prefix(node: *const SyncmapNode, id: u64) -> u64 {
    (id >> unsafe { (*node).height }) >> SHIFT
}

// upstream: i915_syncmap.c __sync_leaf_prefix()
#[inline]
unsafe fn __sync_leaf_prefix(node: *const SyncmapNode, id: u64) -> u64 {
    GEM_BUG_ON!(unsafe { (*node).height != 0 });
    id >> SHIFT
}

// upstream: i915_syncmap.c seqno_later()
#[inline]
fn seqno_later(a: u32, b: u32) -> bool {
    a.wrapping_sub(b) as i32 >= 0
}

// upstream: i915_syncmap.c i915_syncmap_is_later()
pub unsafe fn i915_syncmap_is_later(root: *mut *mut I915Syncmap, id: u64, seqno: u32) -> bool {
    let mut p = unsafe { *root }.cast::<SyncmapNode>();
    if p.is_null() {
        return false;
    }

    if unsafe { __sync_leaf_prefix(p, id) == (*p).prefix } {
        return unsafe { syncmap_found(p, id, seqno) };
    }

    // First climb to the closest ancestor whose prefix includes this id.
    loop {
        p = unsafe { (*p).parent };
        if p.is_null() {
            return false;
        }
        if unsafe { __sync_branch_prefix(p, id) == (*p).prefix } {
            break;
        }
    }

    // Then descend until the matching leaf is found.
    loop {
        if unsafe { (*p).height == 0 } {
            break;
        }
        let idx = unsafe { __sync_branch_idx(p, id) };
        p = unsafe { *__sync_child(p).add(idx) };
        if p.is_null() || unsafe { __sync_branch_prefix(p, id) != (*p).prefix } {
            return false;
        }
    }

    unsafe { *root = p.cast() };
    unsafe { syncmap_found(p, id, seqno) }
}

#[inline]
unsafe fn syncmap_found(node: *mut SyncmapNode, id: u64, seqno: u32) -> bool {
    let idx = unsafe { __sync_leaf_idx(node, id) };
    if unsafe { (*node).bitmap & (1u32 << idx) == 0 } {
        return false;
    }
    seqno_later(unsafe { *__sync_seqno(node).add(idx) }, seqno)
}

// upstream: i915_syncmap.c __sync_alloc_leaf()
unsafe fn __sync_alloc_leaf(parent: *mut SyncmapNode, id: u64) -> *mut SyncmapNode {
    let bytes = size_of::<SyncmapNode>() + KSYNCMAP * size_of::<u32>();
    let p = kmalloc(bytes, GFP_KERNEL).cast::<SyncmapNode>();
    if p.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        (*p).parent = parent;
        (*p).height = 0;
        (*p).bitmap = 0;
        (*p).prefix = __sync_leaf_prefix(p, id);
    }
    p
}

// upstream: i915_syncmap.c __sync_set_seqno()
#[inline]
unsafe fn __sync_set_seqno(node: *mut SyncmapNode, id: u64, seqno: u32) {
    let idx = unsafe { __sync_leaf_idx(node, id) };
    unsafe {
        (*node).bitmap |= 1u32 << idx;
        *__sync_seqno(node).add(idx) = seqno;
    }
}

// upstream: i915_syncmap.c __sync_set_child()
#[inline]
unsafe fn __sync_set_child(node: *mut SyncmapNode, idx: usize, child: *mut SyncmapNode) {
    unsafe {
        (*node).bitmap |= 1u32 << idx;
        *__sync_child(node).add(idx) = child;
    }
}

// upstream: i915_syncmap.c __sync_set()
unsafe fn __sync_set(root: *mut *mut I915Syncmap, id: u64, seqno: u32) -> i32 {
    let mut p = unsafe { *root }.cast::<SyncmapNode>();
    let mut idx: usize;

    if p.is_null() {
        p = unsafe { __sync_alloc_leaf(ptr::null_mut(), id) };
        if p.is_null() {
            return -crate::linux_config::ENOMEM;
        }
        unsafe { *root = p.cast() };
        unsafe { __sync_set_seqno(p, id, seqno) };
        return 0;
    }

    GEM_BUG_ON!(unsafe { __sync_leaf_prefix(p, id) == (*p).prefix });

    loop {
        if unsafe { (*p).parent.is_null() } {
            break;
        }
        p = unsafe { (*p).parent };
        if unsafe { __sync_branch_prefix(p, id) == (*p).prefix } {
            break;
        }
    }

    loop {
        let mut next: *mut SyncmapNode;
        if unsafe { __sync_branch_prefix(p, id) != (*p).prefix } {
            // kzalloc_flex(*next, child, KSYNCMAP): a branch node and pointer array.
            let bytes = size_of::<SyncmapNode>() + KSYNCMAP * size_of::<*mut SyncmapNode>();
            next = kzalloc(bytes, GFP_KERNEL).cast::<SyncmapNode>();
            if next.is_null() {
                return -crate::linux_config::ENOMEM;
            }

            let diff = unsafe { __sync_branch_prefix(p, id) ^ (*p).prefix };
            let mut above = 64u32 - diff.leading_zeros();
            above = (above + SHIFT - 1) & !(SHIFT - 1);
            unsafe {
                (*next).height = above + (*p).height;
                (*next).prefix = __sync_branch_prefix(next, id);
            }

            if unsafe { !(*p).parent.is_null() } {
                let parent = unsafe { (*p).parent };
                idx = unsafe { __sync_branch_idx(parent, id) };
                unsafe { *__sync_child(parent).add(idx) = next };
                GEM_BUG_ON!(unsafe { (*parent).bitmap & (1u32 << idx) == 0 });
            }
            unsafe { (*next).parent = (*p).parent };

            idx = unsafe { ((*p).prefix >> (above - SHIFT)) as usize & MASK };
            unsafe { __sync_set_child(next, idx, p) };
            unsafe { (*p).parent = next };
            p = next;
        } else if unsafe { (*p).height == 0 } {
            break;
        }

        GEM_BUG_ON!(unsafe { (*p).height == 0 });
        idx = unsafe { __sync_branch_idx(p, id) };
        next = unsafe { *__sync_child(p).add(idx) };
        if next.is_null() {
            next = unsafe { __sync_alloc_leaf(p, id) };
            if next.is_null() {
                return -crate::linux_config::ENOMEM;
            }
            unsafe { __sync_set_child(p, idx, next) };
            p = next;
            break;
        }
        p = next;
    }

    GEM_BUG_ON!(unsafe { (*p).prefix != __sync_leaf_prefix(p, id) });
    unsafe {
        __sync_set_seqno(p, id, seqno);
        *root = p.cast();
    }
    0
}

// upstream: i915_syncmap.c i915_syncmap_set()
pub unsafe fn i915_syncmap_set(root: *mut *mut I915Syncmap, id: u64, seqno: u32) -> i32 {
    let p = unsafe { *root }.cast::<SyncmapNode>();
    if !p.is_null() && unsafe { __sync_leaf_prefix(p, id) == (*p).prefix } {
        unsafe { __sync_set_seqno(p, id, seqno) };
        return 0;
    }
    unsafe { __sync_set(root, id, seqno) }
}

// upstream: i915_syncmap.c __sync_free()
unsafe fn __sync_free(node: *mut SyncmapNode) {
    if unsafe { (*node).height != 0 } {
        let mut bitmap = unsafe { (*node).bitmap };
        while bitmap != 0 {
            let bit = bitmap.trailing_zeros() as usize;
            bitmap &= !(1u32 << bit);
            let child = unsafe { *__sync_child(node).add(bit) };
            unsafe { __sync_free(child) };
        }
    }
    unsafe { kfree(node) };
}

// upstream: i915_syncmap.c i915_syncmap_free()
pub unsafe fn i915_syncmap_free(root: *mut *mut I915Syncmap) {
    let mut p = unsafe { *root }.cast::<SyncmapNode>();
    if p.is_null() {
        return;
    }
    while unsafe { !(*p).parent.is_null() } {
        p = unsafe { (*p).parent };
    }
    unsafe {
        __sync_free(p);
        *root = ptr::null_mut();
    }
}
