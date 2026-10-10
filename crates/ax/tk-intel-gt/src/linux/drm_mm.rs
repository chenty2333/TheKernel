// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Translation of Linux 7.2.3 `drivers/gpu/drm/drm_mm.c` (MIT). The hole
//! trees (`holes_size`, `holes_addr`), the interval tree over allocated nodes,
//! the `hole_stack` used by the eviction order and the scanner follow the
//! upstream structure function by function. Upstream `DRM_MM_BUG_ON()` checks
//! and the `CONFIG_DRM_DEBUG_MM` leak tracking are compiled out, matching the
//! oracle configuration.

#![allow(unsafe_code)]
#![allow(non_snake_case)]

use core::{
    ffi::{c_int, c_ulong},
    mem::offset_of,
};

use crate::{
    intel_context_upstream::DrmMmNode,
    intel_engine_cs_upstream::{ListHead, RbNode, RbRoot, RbRootCached},
    linux::{
        gem_memory::DrmMm,
        rbtree::{
            RbAugment, rb_erase_augmented, rb_erase_augmented_cached, rb_erase_cached,
            rb_first_cached, rb_insert_augmented, rb_insert_augmented_cached,
            rb_insert_color_cached, rb_link_node, rb_parent, rb_prev, RB_EMPTY_NODE,
        },
    },
    linux_config::ENOSPC,
    linux_list::{list_add, list_del},
};

const DRM_MM_NODE_ALLOCATED_BIT: u32 = 0;
const DRM_MM_NODE_SCANNED_BIT: u32 = 1;

/// `enum drm_mm_insert_mode` and `DRM_MM_INSERT_ONCE` from include/drm/drm_mm.h.
const DRM_MM_INSERT_BEST: u32 = 0;
const DRM_MM_INSERT_LOW: u32 = 1;
const DRM_MM_INSERT_HIGH: u32 = 2;
const DRM_MM_INSERT_EVICT: u32 = 3;
const DRM_MM_INSERT_ONCE: u32 = 1 << 31;

/// `struct drm_mm_scan` from include/drm/drm_mm.h (80 bytes on x86_64).
#[repr(C)]
pub struct DrmMmScan {
    mm: *mut DrmMm,
    size: u64,
    alignment: u64,
    remainder_mask: u64,
    range_start: u64,
    range_end: u64,
    hit_start: u64,
    hit_end: u64,
    color: c_ulong,
    mode: u32,
}
const _: () = assert!(core::mem::size_of::<DrmMmScan>() == 80);

// ---------------------------------------------------------------------------
// Container helpers (`rb_entry`, `list_entry`) and the node accessors used
// throughout drm_mm.c.
// ---------------------------------------------------------------------------

#[inline]
unsafe fn node_of_rb(rb: *mut RbNode) -> *mut DrmMmNode {
    unsafe { rb.cast::<u8>().sub(offset_of!(DrmMmNode, rb)).cast() }
}
#[inline]
unsafe fn node_of_hole_size(rb: *mut RbNode) -> *mut DrmMmNode {
    unsafe { rb.cast::<u8>().sub(offset_of!(DrmMmNode, rb_hole_size)).cast() }
}
#[inline]
unsafe fn node_of_hole_addr(rb: *mut RbNode) -> *mut DrmMmNode {
    unsafe { rb.cast::<u8>().sub(offset_of!(DrmMmNode, rb_hole_addr)).cast() }
}
#[inline]
unsafe fn node_of_node_list(list: *mut ListHead) -> *mut DrmMmNode {
    unsafe { list.cast::<u8>().sub(offset_of!(DrmMmNode, node_list)).cast() }
}
#[inline]
unsafe fn node_of_hole_stack(list: *mut ListHead) -> *mut DrmMmNode {
    unsafe { list.cast::<u8>().sub(offset_of!(DrmMmNode, hole_stack)).cast() }
}

/// `rb_hole_addr_to_node()` / `rb_hole_size_to_node()`: NULL-safe `rb_entry`.
#[inline]
unsafe fn hole_addr_node_or_null(rb: *mut RbNode) -> *mut DrmMmNode {
    if rb.is_null() { core::ptr::null_mut() } else { unsafe { node_of_hole_addr(rb) } }
}
#[inline]
unsafe fn hole_size_node_or_null(rb: *mut RbNode) -> *mut DrmMmNode {
    if rb.is_null() { core::ptr::null_mut() } else { unsafe { node_of_hole_size(rb) } }
}

#[inline]
unsafe fn node_allocated(node: *const DrmMmNode) -> bool {
    unsafe { (*node).flags & (1 << DRM_MM_NODE_ALLOCATED_BIT) != 0 }
}
#[inline]
unsafe fn node_scanned_block(node: *const DrmMmNode) -> bool {
    unsafe { (*node).flags & (1 << DRM_MM_NODE_SCANNED_BIT) != 0 }
}
#[inline]
unsafe fn hole_follows(node: *const DrmMmNode) -> bool {
    unsafe { (*node).hole_size != 0 }
}
#[inline]
unsafe fn hole_node_start(node: *const DrmMmNode) -> u64 {
    unsafe { (*node).start.wrapping_add((*node).size) }
}
#[inline]
unsafe fn hole_node_end(node: *const DrmMmNode) -> u64 {
    unsafe { (*node_of_node_list((*node).node_list.next)).start }
}
#[inline]
unsafe fn node_last(node: *const DrmMmNode) -> u64 {
    unsafe { (*node).start.wrapping_add((*node).size).wrapping_sub(1) }
}

#[inline]
unsafe fn head_node(mm: *mut DrmMm) -> *mut DrmMmNode {
    unsafe { core::ptr::addr_of_mut!((*mm).head_node) }
}

// ---------------------------------------------------------------------------
// Augmented red-black callbacks: RB_DECLARE_CALLBACKS_MAX over the hole
// addresses, and the interval tree's `subtree_last` (INTERVAL_TREE_DEFINE).
// Propagation walks to `stop` (or the root) without an early exit, so the
// summaries stay exact after structural changes.
// ---------------------------------------------------------------------------

unsafe fn hole_addr_compute(node: *mut DrmMmNode) -> u64 {
    unsafe {
        let mut max = (*node).hole_size;
        let left = hole_addr_node_or_null((*node).rb_hole_addr.left);
        if !left.is_null() && (*left).subtree_max_hole > max {
            max = (*left).subtree_max_hole;
        }
        let right = hole_addr_node_or_null((*node).rb_hole_addr.right);
        if !right.is_null() && (*right).subtree_max_hole > max {
            max = (*right).subtree_max_hole;
        }
        max
    }
}

unsafe fn hole_addr_propagate(rb: *mut RbNode, stop: *mut RbNode) {
    let mut rb = rb;
    while !rb.is_null() && rb != stop {
        let node = unsafe { node_of_hole_addr(rb) };
        unsafe { (*node).subtree_max_hole = hole_addr_compute(node) };
        rb = unsafe { rb_parent(rb) };
    }
}

unsafe fn hole_addr_copy(old: *mut RbNode, new: *mut RbNode) {
    unsafe {
        (*node_of_hole_addr(new)).subtree_max_hole = (*node_of_hole_addr(old)).subtree_max_hole;
    }
}

unsafe fn hole_addr_rotate(old: *mut RbNode, new: *mut RbNode) {
    unsafe {
        hole_addr_copy(old, new);
        let old_node = node_of_hole_addr(old);
        (*old_node).subtree_max_hole = hole_addr_compute(old_node);
    }
}

static AUGMENT_CALLBACKS: RbAugment = RbAugment {
    propagate: hole_addr_propagate,
    copy: hole_addr_copy,
    rotate: hole_addr_rotate,
};

unsafe fn interval_compute_last(node: *mut DrmMmNode) -> u64 {
    unsafe {
        let mut max = node_last(node);
        let left = if (*node).rb.left.is_null() { core::ptr::null_mut() } else { node_of_rb((*node).rb.left) };
        if !left.is_null() && (*left).subtree_last > max {
            max = (*left).subtree_last;
        }
        let right = if (*node).rb.right.is_null() { core::ptr::null_mut() } else { node_of_rb((*node).rb.right) };
        if !right.is_null() && (*right).subtree_last > max {
            max = (*right).subtree_last;
        }
        max
    }
}

unsafe fn interval_propagate(rb: *mut RbNode, stop: *mut RbNode) {
    let mut rb = rb;
    while !rb.is_null() && rb != stop {
        let node = unsafe { node_of_rb(rb) };
        unsafe { (*node).subtree_last = interval_compute_last(node) };
        rb = unsafe { rb_parent(rb) };
    }
}

unsafe fn interval_copy(old: *mut RbNode, new: *mut RbNode) {
    unsafe { (*node_of_rb(new)).subtree_last = (*node_of_rb(old)).subtree_last };
}

unsafe fn interval_rotate(old: *mut RbNode, new: *mut RbNode) {
    unsafe {
        interval_copy(old, new);
        let old_node = node_of_rb(old);
        (*old_node).subtree_last = interval_compute_last(old_node);
    }
}

static INTERVAL_AUGMENT: RbAugment = RbAugment {
    propagate: interval_propagate,
    copy: interval_copy,
    rotate: interval_rotate,
};

/// INTERVAL_TREE_DEFINE: the first node overlapping `[start, last]`, or NULL.
unsafe fn interval_iter_first(root: *mut RbRootCached, start: u64, last: u64) -> *mut DrmMmNode {
    unsafe {
        if (*root).root.node.is_null() {
            return core::ptr::null_mut();
        }
        let node = node_of_rb((*root).root.node);
        if (*node).subtree_last < start {
            return core::ptr::null_mut();
        }
        let leftmost = node_of_rb((*root).leftmost);
        if (*leftmost).start > last {
            return core::ptr::null_mut();
        }
        let mut node = node;
        loop {
            if !(*node).rb.left.is_null() {
                let left = node_of_rb((*node).rb.left);
                if (*left).subtree_last >= start {
                    node = left;
                    continue;
                }
            }
            if (*node).start <= last {
                if node_last(node) >= start {
                    return node;
                }
                if !(*node).rb.right.is_null() {
                    node = node_of_rb((*node).rb.right);
                    if (*node).subtree_last >= start {
                        continue;
                    }
                }
            }
            return core::ptr::null_mut();
        }
    }
}

/// `interval_tree_remove()` from INTERVAL_TREE_DEFINE.
unsafe fn interval_tree_remove(node: *mut DrmMmNode, root: *mut RbRootCached) {
    unsafe { rb_erase_augmented_cached(core::ptr::addr_of_mut!((*node).rb), root, &INTERVAL_AUGMENT) };
}

/// `drm_mm_interval_tree_add_node()`.
unsafe fn drm_mm_interval_tree_add_node(hole_node: *mut DrmMmNode, node: *mut DrmMmNode) {
    unsafe {
        let mm = (*hole_node).mm;
        (*node).subtree_last = node_last(node);
        let mut rb: *mut RbNode;
        let mut link: *mut *mut RbNode;
        let mut leftmost: bool;
        if node_allocated(hole_node) {
            rb = core::ptr::addr_of_mut!((*hole_node).rb);
            while !rb.is_null() {
                let parent = node_of_rb(rb);
                if (*parent).subtree_last >= (*node).subtree_last {
                    break;
                }
                (*parent).subtree_last = (*node).subtree_last;
                rb = rb_parent(rb);
            }
            rb = core::ptr::addr_of_mut!((*hole_node).rb);
            link = core::ptr::addr_of_mut!((*hole_node).rb.right);
            leftmost = false;
        } else {
            rb = core::ptr::null_mut();
            link = core::ptr::addr_of_mut!((*mm).interval_tree.root.node);
            leftmost = true;
        }
        while !(*link).is_null() {
            rb = *link;
            let parent = node_of_rb(rb);
            if (*parent).subtree_last < (*node).subtree_last {
                (*parent).subtree_last = (*node).subtree_last;
            }
            if (*node).start < (*parent).start {
                link = core::ptr::addr_of_mut!((*rb).left);
            } else {
                link = core::ptr::addr_of_mut!((*rb).right);
                leftmost = false;
            }
        }
        rb_link_node(core::ptr::addr_of_mut!((*node).rb), rb, link);
        rb_insert_augmented_cached(
            core::ptr::addr_of_mut!((*node).rb),
            core::ptr::addr_of_mut!((*mm).interval_tree),
            leftmost,
            &INTERVAL_AUGMENT,
        );
    }
}

/// `drm_mm_interval_tree_remove()`.
unsafe fn drm_mm_interval_tree_remove(node: *mut DrmMmNode, root: *mut RbRootCached) {
    unsafe { interval_tree_remove(node, root) };
}

// ---------------------------------------------------------------------------
// Hole trees.
// ---------------------------------------------------------------------------

unsafe fn insert_hole_size(root: *mut RbRootCached, node: *mut DrmMmNode) {
    unsafe {
        let mut link: *mut *mut RbNode = core::ptr::addr_of_mut!((*root).root.node);
        let mut rb: *mut RbNode = core::ptr::null_mut();
        let x = (*node).hole_size;
        let mut first = true;
        while !(*link).is_null() {
            rb = *link;
            if x > (*node_of_hole_size(rb)).hole_size {
                link = core::ptr::addr_of_mut!((*rb).left);
            } else {
                link = core::ptr::addr_of_mut!((*rb).right);
                first = false;
            }
        }
        rb_link_node(core::ptr::addr_of_mut!((*node).rb_hole_size), rb, link);
        rb_insert_color_cached(core::ptr::addr_of_mut!((*node).rb_hole_size), root, first);
    }
}

unsafe fn insert_hole_addr(root: *mut RbRoot, node: *mut DrmMmNode) {
    unsafe {
        let mut link: *mut *mut RbNode = core::ptr::addr_of_mut!((*root).node);
        let mut rb_parent_node: *mut RbNode = core::ptr::null_mut();
        let start = hole_node_start(node);
        let subtree_max_hole = (*node).subtree_max_hole;
        while !(*link).is_null() {
            rb_parent_node = *link;
            let parent = node_of_hole_addr(rb_parent_node);
            if (*parent).subtree_max_hole < subtree_max_hole {
                (*parent).subtree_max_hole = subtree_max_hole;
            }
            if start < hole_node_start(parent) {
                link = core::ptr::addr_of_mut!((*rb_parent_node).left);
            } else {
                link = core::ptr::addr_of_mut!((*rb_parent_node).right);
            }
        }
        rb_link_node(core::ptr::addr_of_mut!((*node).rb_hole_addr), rb_parent_node, link);
        rb_insert_augmented(core::ptr::addr_of_mut!((*node).rb_hole_addr), root, &AUGMENT_CALLBACKS);
    }
}

unsafe fn add_hole(node: *mut DrmMmNode) {
    unsafe {
        let mm = (*node).mm;
        (*node).hole_size = hole_node_end(node).wrapping_sub(hole_node_start(node));
        (*node).subtree_max_hole = (*node).hole_size;
        insert_hole_size(core::ptr::addr_of_mut!((*mm).holes_size), node);
        insert_hole_addr(core::ptr::addr_of_mut!((*mm).holes_addr), node);
        list_add(
            core::ptr::addr_of_mut!((*node).hole_stack),
            core::ptr::addr_of_mut!((*mm).hole_stack),
        );
    }
}

unsafe fn rm_hole(node: *mut DrmMmNode) {
    unsafe {
        list_del(core::ptr::addr_of_mut!((*node).hole_stack));
        let mm = (*node).mm;
        rb_erase_cached(
            core::ptr::addr_of_mut!((*node).rb_hole_size),
            &mut *core::ptr::addr_of_mut!((*mm).holes_size),
        );
        rb_erase_augmented(
            core::ptr::addr_of_mut!((*node).rb_hole_addr),
            core::ptr::addr_of_mut!((*mm).holes_addr),
            &AUGMENT_CALLBACKS,
        );
        (*node).hole_size = 0;
        (*node).subtree_max_hole = 0;
    }
}

unsafe fn best_hole(mm: *mut DrmMm, size: u64) -> *mut DrmMmNode {
    unsafe {
        let mut rb = (*mm).holes_size.root.node;
        let mut best: *mut DrmMmNode = core::ptr::null_mut();
        while !rb.is_null() {
            let node = node_of_hole_size(rb);
            if size <= (*node).hole_size {
                best = node;
                rb = (*rb).right;
            } else {
                rb = (*rb).left;
            }
        }
        best
    }
}

#[inline]
unsafe fn usable_hole_addr(rb: *mut RbNode, size: u64) -> bool {
    unsafe { !rb.is_null() && (*node_of_hole_addr(rb)).subtree_max_hole >= size }
}

unsafe fn find_hole_addr(mm: *mut DrmMm, addr: u64, size: u64) -> *mut DrmMmNode {
    unsafe {
        let mut rb = (*mm).holes_addr.node;
        let mut node: *mut DrmMmNode = core::ptr::null_mut();
        while !rb.is_null() {
            if !usable_hole_addr(rb, size) {
                break;
            }
            node = node_of_hole_addr(rb);
            let hole_start = hole_node_start(node);
            if addr < hole_start {
                rb = (*node).rb_hole_addr.left;
            } else if addr > hole_start.wrapping_add((*node).hole_size) {
                rb = (*node).rb_hole_addr.right;
            } else {
                break;
            }
        }
        node
    }
}

unsafe fn first_hole(mm: *mut DrmMm, start: u64, end: u64, size: u64, mode: u32) -> *mut DrmMmNode {
    unsafe {
        match mode {
            DRM_MM_INSERT_LOW => find_hole_addr(mm, start, size),
            DRM_MM_INSERT_HIGH => find_hole_addr(mm, end, size),
            DRM_MM_INSERT_EVICT => {
                let first = (*mm).hole_stack.next;
                if first == core::ptr::addr_of_mut!((*mm).hole_stack) {
                    core::ptr::null_mut()
                } else {
                    node_of_hole_stack(first)
                }
            }
            _ => best_hole(mm, size),
        }
    }
}

/// `DECLARE_NEXT_HOLE_ADDR(name, first, last)`.
unsafe fn next_hole_addr(entry: *mut DrmMmNode, size: u64, high: bool) -> *mut DrmMmNode {
    unsafe {
        if entry.is_null() || RB_EMPTY_NODE(core::ptr::addr_of_mut!((*entry).rb_hole_addr)) {
            return core::ptr::null_mut();
        }
        let first = |n: *mut RbNode| if high { (*n).left } else { (*n).right };
        let last = |n: *mut RbNode| if high { (*n).right } else { (*n).left };
        let mut node = core::ptr::addr_of_mut!((*entry).rb_hole_addr);
        if usable_hole_addr(first(node), size) {
            node = first(node);
            while usable_hole_addr(last(node), size) {
                node = last(node);
            }
            return node_of_hole_addr(node);
        }
        let mut parent;
        loop {
            parent = rb_parent(node);
            if parent.is_null() {
                break;
            }
            let parent_first = if high { (*parent).left } else { (*parent).right };
            if node != parent_first {
                break;
            }
            node = parent;
        }
        hole_addr_node_or_null(parent)
    }
}

unsafe fn next_hole(mm: *mut DrmMm, node: *mut DrmMmNode, size: u64, mode: u32) -> *mut DrmMmNode {
    unsafe {
        match mode {
            DRM_MM_INSERT_LOW => next_hole_addr(node, size, false),
            DRM_MM_INSERT_HIGH => next_hole_addr(node, size, true),
            DRM_MM_INSERT_EVICT => {
                let next = (*node).hole_stack.next;
                if next == core::ptr::addr_of_mut!((*mm).hole_stack) {
                    core::ptr::null_mut()
                } else {
                    node_of_hole_stack(next)
                }
            }
            _ => hole_size_node_or_null(rb_prev(core::ptr::addr_of_mut!((*node).rb_hole_size))),
        }
    }
}

// ---------------------------------------------------------------------------
// Public API.
// ---------------------------------------------------------------------------

/// Linux `__drm_mm_interval_first()`: the first allocated node overlapping
/// `[start, last]`, or the head node when there is none.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __drm_mm_interval_first(mm: *const DrmMm, start: u64, last: u64) -> *mut DrmMmNode {
    let mm = mm.cast_mut();
    let node = unsafe { interval_iter_first(core::ptr::addr_of_mut!((*mm).interval_tree), start, last) };
    if node.is_null() { unsafe { head_node(mm) } } else { node }
}

/// Linux `drm_mm_init()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mm_init(mm: *mut DrmMm, start: u64, size: u64) {
    assert!(!mm.is_null());
    assert!(start.checked_add(size).is_some(), "drm_mm range wraps");
    unsafe {
        core::ptr::write_bytes(mm.cast::<u8>(), 0, core::mem::size_of::<DrmMm>());
        (*mm).color_adjust = None;
        let stack = core::ptr::addr_of_mut!((*mm).hole_stack);
        (*stack).next = stack;
        (*stack).prev = stack;
        let head = head_node(mm);
        let head_list = core::ptr::addr_of_mut!((*head).node_list);
        (*head_list).next = head_list;
        (*head_list).prev = head_list;
        (*head).flags = 0;
        (*head).mm = mm;
        (*head).start = start.wrapping_add(size);
        (*head).size = size.wrapping_neg();
        add_hole(head);
        (*mm).scan_active = 0;
    }
}

/// Linux `drm_mm_takedown()`: warn when nodes remain.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mm_takedown(mm: *mut DrmMm) {
    assert!(!mm.is_null());
    let head = unsafe { core::ptr::addr_of_mut!((*head_node(mm)).node_list) };
    if unsafe { (*head).next } != head {
        axlog::warn!("Memory manager not clean during takedown.");
    }
}

/// Linux `drm_mm_reserve_node()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mm_reserve_node(mm: *mut DrmMm, node: *mut DrmMmNode) -> c_int {
    unsafe {
        let end = (*node).start.wrapping_add((*node).size);
        if end <= (*node).start {
            return -ENOSPC;
        }
        let hole = find_hole_addr(mm, (*node).start, 0);
        if hole.is_null() {
            return -ENOSPC;
        }
        let hole_start = hole_node_start(hole);
        let hole_end = hole_start.wrapping_add((*hole).hole_size);
        let mut adj_start = hole_start;
        let mut adj_end = hole_end;
        if let Some(adjust) = (*mm).color_adjust {
            adjust(hole, (*node).color, &mut adj_start, &mut adj_end);
        }
        if adj_start > (*node).start || adj_end < end {
            return -ENOSPC;
        }
        (*node).mm = mm;
        (*node).flags |= 1 << DRM_MM_NODE_ALLOCATED_BIT;
        list_add(core::ptr::addr_of_mut!((*node).node_list), core::ptr::addr_of_mut!((*hole).node_list));
        drm_mm_interval_tree_add_node(hole, node);
        (*node).hole_size = 0;
        rm_hole(hole);
        if (*node).start > hole_start {
            add_hole(hole);
        }
        if end < hole_end {
            add_hole(node);
        }
        0
    }
}

/// Linux `drm_mm_insert_node_in_range()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mm_insert_node_in_range(
    mm: *mut DrmMm,
    node: *mut DrmMmNode,
    size: u64,
    alignment: u64,
    color: c_ulong,
    range_start: u64,
    range_end: u64,
    mode: u32,
) -> c_int {
    unsafe {
        if size == 0 || range_end.wrapping_sub(range_start) < size {
            return -ENOSPC;
        }
        let largest = hole_size_node_or_null(rb_first_cached(&*core::ptr::addr_of!((*mm).holes_size)));
        if largest.is_null() || (*largest).hole_size < size {
            return -ENOSPC;
        }
        let alignment = if alignment <= 1 { 0 } else { alignment };
        let once = mode & DRM_MM_INSERT_ONCE != 0;
        let mode = mode & !DRM_MM_INSERT_ONCE;
        let remainder_mask = if alignment.is_power_of_two() { alignment - 1 } else { 0 };

        let mut hole = first_hole(mm, range_start, range_end, size, mode);
        while !hole.is_null() {
            let hole_start = hole_node_start(hole);
            let hole_end = hole_start.wrapping_add((*hole).hole_size);
            if mode == DRM_MM_INSERT_LOW && hole_start >= range_end {
                break;
            }
            if mode == DRM_MM_INSERT_HIGH && hole_end <= range_start {
                break;
            }
            let mut col_start = hole_start;
            let mut col_end = hole_end;
            if let Some(adjust) = (*mm).color_adjust {
                adjust(hole, color, &mut col_start, &mut col_end);
            }
            let mut adj_start = col_start.max(range_start);
            let adj_end = col_end.min(range_end);
            let placeable = !(adj_end <= adj_start || adj_end - adj_start < size);
            if placeable {
                if mode == DRM_MM_INSERT_HIGH {
                    adj_start = adj_end - size;
                }
                let mut ok = true;
                if alignment != 0 {
                    let rem = if remainder_mask != 0 {
                        adj_start & remainder_mask
                    } else {
                        adj_start % alignment
                    };
                    if rem != 0 {
                        adj_start -= rem;
                        if mode != DRM_MM_INSERT_HIGH {
                            adj_start += alignment;
                        }
                        if adj_start < col_start.max(range_start)
                            || col_end.min(range_end) - adj_start < size
                        {
                            ok = false;
                        }
                        if ok && (adj_end <= adj_start || adj_end - adj_start < size) {
                            ok = false;
                        }
                    }
                }
                if ok {
                    (*node).mm = mm;
                    (*node).size = size;
                    (*node).start = adj_start;
                    (*node).color = color;
                    (*node).hole_size = 0;
                    (*node).flags |= 1 << DRM_MM_NODE_ALLOCATED_BIT;
                    list_add(core::ptr::addr_of_mut!((*node).node_list), core::ptr::addr_of_mut!((*hole).node_list));
                    drm_mm_interval_tree_add_node(hole, node);
                    rm_hole(hole);
                    if adj_start > hole_start {
                        add_hole(hole);
                    }
                    if adj_start.wrapping_add(size) < hole_end {
                        add_hole(node);
                    }
                    return 0;
                }
            }
            hole = if once { core::ptr::null_mut() } else { next_hole(mm, hole, size, mode) };
        }
        -ENOSPC
    }
}

/// Linux `drm_mm_remove_node()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mm_remove_node(node: *mut DrmMmNode) {
    unsafe {
        let mm = (*node).mm;
        let prev_node = node_of_node_list((*node).node_list.prev);
        if hole_follows(node) {
            rm_hole(node);
        }
        drm_mm_interval_tree_remove(node, core::ptr::addr_of_mut!((*mm).interval_tree));
        list_del(core::ptr::addr_of_mut!((*node).node_list));
        if hole_follows(prev_node) {
            rm_hole(prev_node);
        }
        add_hole(prev_node);
        (*node).flags &= !(1 << DRM_MM_NODE_ALLOCATED_BIT);
    }
}

/// Linux `drm_mm_scan_init_with_range()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mm_scan_init_with_range(
    scan: *mut DrmMmScan,
    mm: *mut DrmMm,
    size: u64,
    alignment: u64,
    color: c_ulong,
    start: u64,
    end: u64,
    mode: c_int,
) {
    unsafe {
        let alignment = if alignment <= 1 { 0 } else { alignment };
        (*scan).color = color;
        (*scan).alignment = alignment;
        (*scan).remainder_mask = if alignment.is_power_of_two() { alignment - 1 } else { 0 };
        (*scan).size = size;
        (*scan).mode = mode as u32;
        (*scan).mm = mm;
        (*scan).range_start = start;
        (*scan).range_end = end;
        (*scan).hit_start = u64::MAX;
        (*scan).hit_end = 0;
    }
}

/// Linux `drm_mm_scan_add_block()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mm_scan_add_block(scan: *mut DrmMmScan, node: *mut DrmMmNode) -> bool {
    unsafe {
        let mm = (*scan).mm;
        (*node).flags |= 1 << DRM_MM_NODE_SCANNED_BIT;
        (*mm).scan_active += 1;
        let hole = node_of_node_list((*node).node_list.prev);
        // __list_del_entry(): unlink without poisoning the node's own links.
        let entry = core::ptr::addr_of_mut!((*node).node_list);
        (*(*entry).prev).next = (*entry).next;
        (*(*entry).next).prev = (*entry).prev;

        let hole_start = hole_node_start(hole);
        let hole_end = hole_node_end(hole);
        let mut col_start = hole_start;
        let mut col_end = hole_end;
        if let Some(adjust) = (*mm).color_adjust {
            adjust(hole, (*scan).color, &mut col_start, &mut col_end);
        }
        let mut adj_start = col_start.max((*scan).range_start);
        let adj_end = col_end.min((*scan).range_end);
        if adj_end <= adj_start || adj_end - adj_start < (*scan).size {
            return false;
        }
        if (*scan).mode == DRM_MM_INSERT_HIGH {
            adj_start = adj_end - (*scan).size;
        }
        if (*scan).alignment != 0 {
            let rem = if (*scan).remainder_mask != 0 {
                adj_start & (*scan).remainder_mask
            } else {
                adj_start % (*scan).alignment
            };
            if rem != 0 {
                adj_start -= rem;
                if (*scan).mode != DRM_MM_INSERT_HIGH {
                    adj_start += (*scan).alignment;
                }
                if adj_start < col_start.max((*scan).range_start)
                    || col_end.min((*scan).range_end) - adj_start < (*scan).size
                {
                    return false;
                }
                if adj_end <= adj_start || adj_end - adj_start < (*scan).size {
                    return false;
                }
            }
        }
        (*scan).hit_start = adj_start;
        (*scan).hit_end = adj_start + (*scan).size;
        true
    }
}

/// Linux `drm_mm_scan_remove_block()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mm_scan_remove_block(scan: *mut DrmMmScan, node: *mut DrmMmNode) -> bool {
    unsafe {
        (*node).flags &= !(1 << DRM_MM_NODE_SCANNED_BIT);
        let mm = (*node).mm;
        (*mm).scan_active -= 1;
        let prev_node = node_of_node_list((*node).node_list.prev);
        list_add(core::ptr::addr_of_mut!((*node).node_list), core::ptr::addr_of_mut!((*prev_node).node_list));
        (*node).start.wrapping_add((*node).size) > (*scan).hit_start && (*node).start < (*scan).hit_end
    }
}

/// Linux `drm_mm_scan_color_evict()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mm_scan_color_evict(scan: *mut DrmMmScan) -> *mut DrmMmNode {
    unsafe {
        let mm = (*scan).mm;
        if (*mm).color_adjust.is_none() {
            return core::ptr::null_mut();
        }
        let stack = core::ptr::addr_of_mut!((*mm).hole_stack);
        let mut link = (*stack).next;
        let mut hole: *mut DrmMmNode = core::ptr::null_mut();
        while link != stack {
            let candidate = node_of_hole_stack(link);
            let hole_start = hole_node_start(candidate);
            let hole_end = hole_start.wrapping_add((*candidate).hole_size);
            if hole_start <= (*scan).hit_start && hole_end >= (*scan).hit_end {
                hole = candidate;
                break;
            }
            link = (*link).next;
        }
        if hole.is_null() {
            return core::ptr::null_mut();
        }
        let mut hole_start = hole_node_start(hole);
        let mut hole_end = hole_start.wrapping_add((*hole).hole_size);
        if let Some(adjust) = (*mm).color_adjust {
            adjust(hole, (*scan).color, &mut hole_start, &mut hole_end);
        }
        if hole_start > (*scan).hit_start {
            return hole;
        }
        if hole_end < (*scan).hit_end {
            return node_of_node_list((*hole).node_list.next);
        }
        core::ptr::null_mut()
    }
}


#[cfg(test)]
mod tests {
    use core::mem::MaybeUninit;

    use super::*;

    fn new_node() -> DrmMmNode {
        unsafe { core::mem::zeroed() }
    }

    #[test]
    fn low_high_and_reserve_placement() {
        let mut mm = MaybeUninit::<DrmMm>::zeroed();
        let mm = mm.as_mut_ptr();
        unsafe { drm_mm_init(mm, 0x1000, 0x10000) };
        let mut a = new_node();
        let mut b = new_node();
        assert_eq!(unsafe { drm_mm_insert_node_in_range(mm, &mut a, 0x1000, 0x1000, 0, 0, u64::MAX, 1) }, 0);
        assert_eq!(a.start, 0x1000);
        assert_eq!(unsafe { drm_mm_insert_node_in_range(mm, &mut b, 0x1000, 0, 0, 0, u64::MAX, 2) }, 0);
        assert_eq!(b.start, 0x1000 + 0x10000 - 0x1000);
        let mut c = new_node();
        c.start = 0x1800;
        c.size = 0x1000;
        assert_eq!(unsafe { drm_mm_reserve_node(mm, &mut c) }, -ENOSPC);
        let mut d = new_node();
        d.start = 0x8000;
        d.size = 0x1000;
        assert_eq!(unsafe { drm_mm_reserve_node(mm, &mut d) }, 0);
        unsafe { drm_mm_remove_node(&mut a) };
        unsafe { drm_mm_remove_node(&mut b) };
        unsafe { drm_mm_remove_node(&mut d) };
        unsafe { drm_mm_takedown(mm) };
    }

    #[test]
    fn scan_finds_the_evictable_hole() {
        let mut mm = MaybeUninit::<DrmMm>::zeroed();
        let mm = mm.as_mut_ptr();
        unsafe { drm_mm_init(mm, 0, 0x4000) };
        let mut nodes = [new_node(), new_node(), new_node(), new_node()];
        for node in nodes.iter_mut() {
            assert_eq!(unsafe { drm_mm_insert_node_in_range(mm, node, 0x1000, 0, 0, 0, u64::MAX, 1) }, 0);
        }
        let mut scan = MaybeUninit::<DrmMmScan>::zeroed();
        let scan = scan.as_mut_ptr();
        unsafe { drm_mm_scan_init_with_range(scan, mm, 0x2000, 0, 0, 0, u64::MAX, 0) };
        assert!(!unsafe { drm_mm_scan_add_block(scan, &mut nodes[0]) });
        assert!(unsafe { drm_mm_scan_add_block(scan, &mut nodes[1]) });
        assert_eq!(unsafe { (*scan).hit_start }, 0);
        assert_eq!(unsafe { (*scan).hit_end }, 0x2000);
        for node in [1usize, 0] {
            let _ = unsafe { drm_mm_scan_remove_block(scan, &mut nodes[node]) };
        }
        assert_eq!(unsafe { (*mm).scan_active }, 0);
        for node in nodes.iter_mut() {
            unsafe { drm_mm_remove_node(node) };
        }
        unsafe { drm_mm_takedown(mm) };
    }

    #[test]
    fn hole_trees_track_free_space_after_churn() {
        let mut mm = MaybeUninit::<DrmMm>::zeroed();
        let mm = mm.as_mut_ptr();
        unsafe { drm_mm_init(mm, 0, 0x100000) };
        let mut nodes: alloc::vec::Vec<DrmMmNode> = (0..64).map(|_| new_node()).collect();
        for (i, node) in nodes.iter_mut().enumerate() {
            let size = 0x1000 * (1 + (i as u64 % 5));
            assert_eq!(unsafe { drm_mm_insert_node_in_range(mm, node, size, 0x1000, 0, 0, u64::MAX, DRM_MM_INSERT_BEST) }, 0);
        }
        for i in (0..64).step_by(2) {
            unsafe { drm_mm_remove_node(&mut nodes[i]) };
        }
        // Allocated nodes must never overlap.
        let mut sorted: alloc::vec::Vec<(u64, u64)> = nodes
            .iter()
            .enumerate()
            .filter(|(i, _)| i % 2 == 1)
            .map(|(_, n)| (n.start, n.size))
            .collect();
        sorted.sort();
        for pair in sorted.windows(2) {
            assert!(pair[0].0 + pair[0].1 <= pair[1].0);
        }
        for i in (1..64).step_by(2) {
            unsafe { drm_mm_remove_node(&mut nodes[i]) };
        }
        assert_eq!(unsafe { drm_mm_insert_node_in_range(mm, &mut nodes[0], 0x100000, 0, 0, 0, u64::MAX, DRM_MM_INSERT_BEST) }, 0);
        unsafe { drm_mm_remove_node(&mut nodes[0]) };
        unsafe { drm_mm_takedown(mm) };
    }
}
