// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! LinuxKPI `drm_mm` range allocator for the i915 GGTT, PPGTT and stolen
//! memory managers (Linux 7.2.3 `drivers/gpu/drm/drm_mm.c` semantics).
//!
//! Free space is derived from the address-ordered `node_list`, which is the
//! same circular list Linux threads through `head_node`. Each hole is the gap
//! between a node and its successor (the head sentinel supplies both ends), so
//! hole discovery, the insertion placement rules and the eviction scanner
//! match Linux's search results. Linux additionally keeps size-, address- and
//! free-order rb-trees for its hole searches; this backend walks the list
//! instead and orders the `DRM_MM_INSERT_EVICT` search best-fit.

#![allow(unsafe_code)]

use core::{
    ffi::{c_int, c_ulong},
    mem::offset_of,
};

use crate::{
    intel_context_upstream::DrmMmNode,
    intel_engine_cs_upstream::ListHead,
    linux::gem_memory::DrmMm,
};

/// `DRM_MM_NODE_ALLOCATED_BIT` and `DRM_MM_NODE_SCANNED_BIT` from drm_mm.h.
const DRM_MM_NODE_ALLOCATED_BIT: u32 = 0;
const DRM_MM_NODE_SCANNED_BIT: u32 = 1;

/// `enum drm_mm_insert_mode` values from include/drm/drm_mm.h.
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

#[inline]
unsafe fn node_from_list(list: *mut ListHead) -> *mut DrmMmNode {
    unsafe { list.cast::<u8>().sub(offset_of!(DrmMmNode, node_list)).cast::<DrmMmNode>() }
}

#[inline]
unsafe fn head_of(mm: *mut DrmMm) -> *mut DrmMmNode {
    unsafe { core::ptr::addr_of_mut!((*mm).head_node) }
}

#[inline]
unsafe fn next_node(node: *mut DrmMmNode) -> *mut DrmMmNode {
    unsafe { node_from_list((*node).node_list.next) }
}

#[inline]
unsafe fn prev_node(node: *mut DrmMmNode) -> *mut DrmMmNode {
    unsafe { node_from_list((*node).node_list.prev) }
}

/// Start of the hole that follows `node` (`__drm_mm_hole_node_start`).
#[inline]
unsafe fn hole_start(node: *mut DrmMmNode) -> u64 {
    unsafe { (*node).start.wrapping_add((*node).size) }
}

/// End of the hole that follows `node` (`__drm_mm_hole_node_end`).
#[inline]
unsafe fn hole_end(node: *mut DrmMmNode) -> u64 {
    unsafe { (*next_node(node)).start }
}

#[inline]
unsafe fn node_allocated(node: *const DrmMmNode) -> bool {
    unsafe { (*node).flags & (1 << DRM_MM_NODE_ALLOCATED_BIT) != 0 }
}

#[inline]
unsafe fn node_scanned(node: *const DrmMmNode) -> bool {
    unsafe { (*node).flags & (1 << DRM_MM_NODE_SCANNED_BIT) != 0 }
}

/// Link `node` into the list directly after `after`.
unsafe fn list_insert_after(node: *mut DrmMmNode, after: *mut DrmMmNode) {
    unsafe {
        let new = core::ptr::addr_of_mut!((*node).node_list);
        let at = core::ptr::addr_of_mut!((*after).node_list);
        let next = (*at).next;
        (*new).next = next;
        (*new).prev = at;
        (*next).prev = new;
        (*at).next = new;
    }
}

/// Unlink `node` from the list while keeping its own pointers, as Linux's
/// scanner does, so the node can be restored at the same place.
unsafe fn list_unlink_keep(node: *mut DrmMmNode) {
    unsafe {
        let entry = core::ptr::addr_of_mut!((*node).node_list);
        (*(*entry).prev).next = (*entry).next;
        (*(*entry).next).prev = (*entry).prev;
    }
}

/// Visit holes in address order. A hole is the node whose successor bounds it.
unsafe fn holes_in_order(mm: *mut DrmMm, mut visit: impl FnMut(*mut DrmMmNode) -> bool) {
    unsafe {
        let head = head_of(mm);
        let mut node = head;
        loop {
            if hole_start(node) < hole_end(node) && !visit(node) {
                return;
            }
            node = next_node(node);
            if node == head {
                return;
            }
        }
    }
}

/// Linux `drm_mm_init()`: an empty allocator over `[start, start + size)`.
/// The head sentinel stores the range end in its start and `-size` in its
/// size, so both range boundaries are holes' edges.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mm_init(mm: *mut DrmMm, start: u64, size: u64) {
    assert!(!mm.is_null());
    assert!(start.checked_add(size).is_some(), "drm_mm range wraps");
    unsafe {
        core::ptr::write_bytes(mm.cast::<u8>(), 0, core::mem::size_of::<DrmMm>());
        let head = head_of(mm);
        (*head).node_list.next = core::ptr::addr_of_mut!((*head).node_list);
        (*head).node_list.prev = core::ptr::addr_of_mut!((*head).node_list);
        (*head).hole_stack.next = core::ptr::addr_of_mut!((*head).hole_stack);
        (*head).hole_stack.prev = core::ptr::addr_of_mut!((*head).hole_stack);
        (*head).flags = 0;
        (*head).mm = mm;
        (*head).start = start.wrapping_add(size);
        (*head).size = size.wrapping_neg();
        (*mm).hole_stack.next = core::ptr::addr_of_mut!((*mm).hole_stack);
        (*mm).hole_stack.prev = core::ptr::addr_of_mut!((*mm).hole_stack);
    }
}

/// Linux `drm_mm_takedown()`: the allocator must be empty.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mm_takedown(mm: *mut DrmMm) {
    assert!(!mm.is_null());
    let head = unsafe { head_of(mm) };
    assert!(
        unsafe { next_node(head) } == head,
        "drm_mm_takedown() with nodes still allocated"
    );
}

/// Linux `drm_mm_reserve_node()`: allocate the exact range of `node`.
/// Returns -ENOSPC when the range is not wholly free.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mm_reserve_node(mm: *mut DrmMm, node: *mut DrmMmNode) -> c_int {
    assert!(!mm.is_null() && !node.is_null());
    let start = unsafe { (*node).start };
    let size = unsafe { (*node).size };
    if size == 0 || start.checked_add(size).is_none() {
        return -crate::linux_config::EINVAL;
    }
    let end = start + size;
    let mut placed_after = None;
    unsafe {
        holes_in_order(mm, |hole| {
            if hole_start(hole) <= start && end <= hole_end(hole) {
                placed_after = Some(hole);
                false
            } else {
                true
            }
        });
    }
    let Some(after) = placed_after else {
        return -crate::linux_config::ENOSPC;
    };
    unsafe {
        (*node).mm = mm;
        (*node).color = 0;
        (*node).flags |= 1 << DRM_MM_NODE_ALLOCATED_BIT;
        list_insert_after(node, after);
    }
    0
}

/// Linux `drm_mm_remove_node()`: release an allocated node.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mm_remove_node(node: *mut DrmMmNode) {
    assert!(!node.is_null());
    assert!(
        unsafe { node_allocated(node) },
        "drm_mm_remove_node() on a free node"
    );
    unsafe {
        list_unlink_keep(node);
        (*node).node_list.next = core::ptr::null_mut();
        (*node).node_list.prev = core::ptr::null_mut();
        (*node).flags &= !(1 << DRM_MM_NODE_ALLOCATED_BIT);
    }
}

/// Linux `drm_mm_insert_node_in_range()`.
///
/// Searches holes that intersect `[range_start, range_end)` for room for
/// `size` bytes at `alignment`, with `color_adjust` applied to each hole.
/// `DRM_MM_INSERT_LOW` and `DRM_MM_INSERT_HIGH` take the lowest or highest
/// fitting hole; `DRM_MM_INSERT_BEST` and `DRM_MM_INSERT_EVICT` take the
/// smallest fitting hole. `DRM_MM_INSERT_ONCE` tries only the first hole
/// the mode selects.
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
    assert!(!mm.is_null() && !node.is_null());
    assert!(range_start <= range_end);
    if size == 0 || range_end - range_start < size {
        return -crate::linux_config::ENOSPC;
    }
    let alignment = if alignment <= 1 { 0 } else { alignment };
    let once = mode & DRM_MM_INSERT_ONCE != 0;
    let mode = mode & !DRM_MM_INSERT_ONCE;
    let remainder_mask = if alignment.is_power_of_two() {
        alignment - 1
    } else {
        0
    };

    // Candidate holes in the order the mode prefers, collected first so the
    // list can be walked without mutating it.
    let mut holes: alloc::vec::Vec<*mut DrmMmNode> = alloc::vec::Vec::new();
    unsafe {
        holes_in_order(mm, |hole| {
            holes.push(hole);
            true
        });
    }
    if once {
        holes.truncate(1);
    }
    match mode {
        DRM_MM_INSERT_LOW => {}
        DRM_MM_INSERT_HIGH => holes.reverse(),
        _ => unsafe {
            holes.sort_by_key(|&hole| (hole_end(hole).wrapping_sub(hole_start(hole)), hole_start(hole)));
        },
    }

    for hole in holes {
        let hole_start_addr = unsafe { hole_start(hole) };
        let hole_end_addr = unsafe { hole_end(hole) };
        if mode == DRM_MM_INSERT_LOW && hole_start_addr >= range_end {
            break;
        }
        if mode == DRM_MM_INSERT_HIGH && hole_end_addr <= range_start {
            break;
        }
        let mut col_start = hole_start_addr;
        let mut col_end = hole_end_addr;
        if let Some(adjust) = unsafe { (*mm).color_adjust } {
            unsafe { adjust(hole, color, &mut col_start, &mut col_end) };
        }
        let adj_start0 = col_start.max(range_start);
        let adj_end = col_end.min(range_end);
        if adj_end <= adj_start0 || adj_end - adj_start0 < size {
            continue;
        }
        let mut adj_start = adj_start0;
        if mode == DRM_MM_INSERT_HIGH {
            adj_start = adj_end - size;
        }
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
                    continue;
                }
                if adj_end <= adj_start || adj_end - adj_start < size {
                    continue;
                }
            }
        }
        unsafe {
            (*node).mm = mm;
            (*node).size = size;
            (*node).start = adj_start;
            (*node).color = color;
            (*node).flags |= 1 << DRM_MM_NODE_ALLOCATED_BIT;
            list_insert_after(node, hole);
        }
        return 0;
    }
    -crate::linux_config::ENOSPC
}

/// Linux `__drm_mm_interval_first()`: the lowest-addressed allocated node that
/// overlaps `[start, last]`, or NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __drm_mm_interval_first(
    mm: *const DrmMm,
    start: u64,
    last: u64,
) -> *mut DrmMmNode {
    let mm = mm.cast_mut();
    let head = unsafe { head_of(mm) };
    let mut node = unsafe { next_node(head) };
    while node != head {
        let node_last = unsafe { (*node).start.wrapping_add((*node).size).wrapping_sub(1) };
        if unsafe { (*node).start } <= last && node_last >= start {
            return node;
        }
        node = unsafe { next_node(node) };
    }
    core::ptr::null_mut()
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
    assert!(!scan.is_null() && !mm.is_null());
    assert!(start < end && size != 0 && size <= end - start);
    unsafe {
        (*scan).mm = mm;
        (*scan).range_start = start;
        (*scan).range_end = end;
        (*scan).size = size;
        (*scan).alignment = alignment;
        (*scan).remainder_mask = if alignment.is_power_of_two() {
            alignment - 1
        } else {
            0
        };
        (*scan).hit_start = u64::MAX;
        (*scan).hit_end = 0;
        (*scan).color = color;
        (*scan).mode = mode as u32;
    }
}

/// Linux `drm_mm_scan_add_block()`: temporarily remove `node` from the list so
/// its hole grows, then report whether the enlarged hole fits the request.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mm_scan_add_block(scan: *mut DrmMmScan, node: *mut DrmMmNode) -> bool {
    assert!(!scan.is_null() && !node.is_null());
    let mm = unsafe { (*scan).mm };
    unsafe {
        assert!(node_allocated(node) && !node_scanned(node));
        (*node).flags |= 1 << DRM_MM_NODE_SCANNED_BIT;
        (*mm).scan_active += 1;
        let hole = prev_node(node);
        list_unlink_keep(node);

        let hs = hole_start(hole);
        let he = hole_end(hole);
        let mut col_start = hs;
        let mut col_end = he;
        if let Some(adjust) = (*mm).color_adjust {
            adjust(hole, (*scan).color, &mut col_start, &mut col_end);
        }
        let range_start = (*scan).range_start;
        let range_end = (*scan).range_end;
        let size = (*scan).size;
        let alignment = (*scan).alignment;
        let mut adj_start = col_start.max(range_start);
        let adj_end = col_end.min(range_end);
        if adj_end <= adj_start || adj_end - adj_start < size {
            return false;
        }
        if (*scan).mode == DRM_MM_INSERT_HIGH {
            adj_start = adj_end - size;
        }
        if alignment != 0 {
            let rem = if (*scan).remainder_mask != 0 {
                adj_start & (*scan).remainder_mask
            } else {
                adj_start % alignment
            };
            if rem != 0 {
                adj_start -= rem;
                if (*scan).mode != DRM_MM_INSERT_HIGH {
                    adj_start += alignment;
                }
                if adj_start < col_start.max(range_start)
                    || col_end.min(range_end) - adj_start < size
                {
                    return false;
                }
                if adj_end <= adj_start || adj_end - adj_start < size {
                    return false;
                }
            }
        }
        (*scan).hit_start = adj_start;
        (*scan).hit_end = adj_start + size;
    }
    true
}

/// Linux `drm_mm_scan_remove_block()`: restore `node` into the list and report
/// whether it overlaps the hole found by the scan.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mm_scan_remove_block(scan: *mut DrmMmScan, node: *mut DrmMmNode) -> bool {
    assert!(!scan.is_null() && !node.is_null());
    unsafe {
        assert!(node_scanned(node));
        (*node).flags &= !(1 << DRM_MM_NODE_SCANNED_BIT);
        let mm = (*scan).mm;
        assert!((*mm).scan_active != 0);
        (*mm).scan_active -= 1;
        let prev = prev_node(node);
        list_insert_after(node, prev);
        (*node).start.wrapping_add((*node).size) > (*scan).hit_start
            && (*node).start < (*scan).hit_end
    }
}

/// Linux `drm_mm_scan_color_evict()`: when `color_adjust` leaves nodes
/// overlapping the found hole, return one of them for eviction.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mm_scan_color_evict(scan: *mut DrmMmScan) -> *mut DrmMmNode {
    assert!(!scan.is_null());
    let mm = unsafe { (*scan).mm };
    let Some(adjust) = (unsafe { (*mm).color_adjust }) else {
        return core::ptr::null_mut();
    };
    let (hit_start, hit_end) = unsafe { ((*scan).hit_start, (*scan).hit_end) };
    let mut found: *mut DrmMmNode = core::ptr::null_mut();
    unsafe {
        holes_in_order(mm, |hole| {
            if hole_start(hole) <= hit_start && hole_end(hole) >= hit_end {
                found = hole;
                false
            } else {
                true
            }
        });
    }
    if found.is_null() {
        return core::ptr::null_mut();
    }
    let mut start = unsafe { hole_start(found) };
    let mut end = unsafe { hole_end(found) };
    unsafe { adjust(found, (*scan).color, &mut start, &mut end) };
    if start > hit_start {
        return found;
    }
    if end < hit_end {
        return unsafe { next_node(found) };
    }
    core::ptr::null_mut()
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
        assert_eq!(unsafe { drm_mm_reserve_node(mm, &mut c) }, -crate::linux_config::ENOSPC);
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
        // One 0x1000 block frees 0x1000, which is too small; the second
        // adjacent block makes a 0x2000 hole.
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
}
