// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../../LICENSE-MIT.
//! Linux IDA surface backed by the LinuxKPI XArray indexed storage.

use alloc::{boxed::Box, collections::BTreeMap};
use core::ffi::{c_int, c_ulong, c_void};

use crate::{
    types::RadixTreeRoot,
    {
        config::{GFP_ATOMIC, GFP_KERNEL},
        xarray::{self, XArray},
    },
};

// The source DRM `idr` is accessed by i915 under RCU while handles are added
// and removed by other threads. This LinuxKPI implementation stores stable
// slots in a BTreeMap rather than a lockless radix tree, so serialize slot
// lookup/mutation and let the caller acquire the GEM reference before leaving
// its surrounding RCU read-side section.
static RADIX_SLOTS_LOCK: spin::Mutex<()> = spin::Mutex::new(());

/// `struct idr` (include/linux/idr.h): an xarray plus the allocation base and
/// next-free cursor. Oracle DWARF size 24, align 8.
#[repr(C)]
pub struct Idr {
    pub idr_rt: RadixTreeRoot,
    pub idr_base: u32,
    pub idr_next: u32,
}
const _: [(); 24] = [(); core::mem::size_of::<Idr>()];
const _: [(); 16] = [(); core::mem::offset_of!(Idr, idr_base)];

/// Linux `struct ida` embeds one `struct xarray` as its complete ABI.
#[repr(C)]
pub struct Ida {
    pub xa: XArray,
}

const _: [(); 16] = [(); core::mem::size_of::<Ida>()];
const _: [(); 0] = [(); core::mem::offset_of!(Ida, xa)];

pub fn ida_init(ida: &mut Ida) {
    xarray::xa_init_flags(&mut ida.xa, 0);
}

pub fn ida_destroy(ida: &mut Ida) {
    xarray::xa_destroy(&mut ida.xa);
}

/// Allocate the lowest unused ID in the inclusive range.
///
/// `IDA` stores an allocated bitmap entry per ID in the XArray. This binding
/// uses a non-null sentinel entry; the ID uniqueness and locking contract are
/// preserved, and the allocator's GFP_ATOMIC limitation is inherited from
/// the XArray backend.
pub fn ida_alloc_range(ida: &mut Ida, min: u32, max: u32, gfp: u32) -> i32 {
    if min > max {
        return -22;
    }
    if gfp & GFP_ATOMIC != 0 && gfp & GFP_KERNEL == 0 {
        return -12;
    }
    let mut flags = 0u64;
    xarray::xa_lock_irqsave(&mut ida.xa, &mut flags);
    let result = (min..=max).find(|id| xarray::xa_load::<_, u8>(&mut ida.xa, *id).is_null());
    let status = if let Some(id) = result {
        let marker = core::ptr::NonNull::<u8>::dangling().as_ptr();
        xarray::__xa_store(&mut ida.xa, id, marker, 0);
        id as i32
    } else {
        -28
    };
    xarray::xa_unlock_irqrestore(&mut ida.xa, flags);
    status
}

pub fn ida_free(ida: &mut Ida, id: u32) {
    let _ = xarray::xa_erase_irq::<_, u8>(&mut ida.xa, id);
}

// ---------------------------------------------------------------------------
// Radix tree and IDR.
//
// `struct radix_tree_root::rnode` points at a heap-allocated ordered map from
// index to a boxed slot. Each slot is a stable `*mut c_void` cell, so the
// slot pointers returned by the iterator stay valid while the entry is kept.
// Every chunk the iterator returns covers one slot, and no tags are kept.
// ---------------------------------------------------------------------------

/// Storage behind `radix_tree_root::rnode`.
struct RadixSlots {
    slots: BTreeMap<u64, Box<*mut c_void>>,
}

/// `struct radix_tree_iter` from include/linux/radix-tree.h.
#[repr(C)]
struct RadixIter {
    index: c_ulong,
    next_index: c_ulong,
    tags: c_ulong,
    node: *mut c_void,
}

const RADIX_TREE_ITER_TAGGED: u32 = 0x10;
const RADIX_TREE_ITER_CONTIG: u32 = 0x20;
const EEXIST_ERRNO: c_int = crate::config::EEXIST;
const EINVAL_ERRNO: c_int = crate::config::EINVAL;

/// The slot storage of `root`, created on first use.
unsafe fn radix_slots_mut<'a>(root: *mut RadixTreeRoot) -> &'a mut RadixSlots {
    unsafe {
        if (*root).rnode.is_null() {
            let storage = Box::new(RadixSlots {
                slots: BTreeMap::new(),
            });
            (*root).rnode = Box::into_raw(storage).cast::<c_void>();
        }
        &mut *(*root).rnode.cast::<RadixSlots>()
    }
}

/// The slot storage of `root`, if any entry was ever stored.
unsafe fn radix_slots_ref<'a>(root: *const RadixTreeRoot) -> Option<&'a RadixSlots> {
    unsafe {
        let rnode = (*root).rnode;
        if rnode.is_null() {
            None
        } else {
            Some(&*rnode.cast::<RadixSlots>())
        }
    }
}

/// Free the slot storage of `root` once it holds no entries, so an emptied
/// tree does not keep its map allocated.
unsafe fn radix_release_if_empty(root: *mut RadixTreeRoot) {
    unsafe {
        let rnode = (*root).rnode;
        if !rnode.is_null() && (*rnode.cast::<RadixSlots>()).slots.is_empty() {
            drop(Box::from_raw(rnode.cast::<RadixSlots>()));
            (*root).rnode = core::ptr::null_mut();
        }
    }
}

/// Linux `radix_tree_destroy()`: free the tree's internal storage. The entries
/// are the caller's and are not released.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn radix_tree_destroy(root: *mut RadixTreeRoot) {
    assert!(!root.is_null());
    let _guard = RADIX_SLOTS_LOCK.lock();
    unsafe {
        let rnode = (*root).rnode;
        if !rnode.is_null() {
            drop(Box::from_raw(rnode.cast::<RadixSlots>()));
            (*root).rnode = core::ptr::null_mut();
        }
    }
}

/// Linux `radix_tree_insert()`: 0 on success, -EEXIST when `index` is taken.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn radix_tree_insert(
    root: *mut RadixTreeRoot,
    index: u64,
    item: *mut c_void,
) -> c_int {
    assert!(!root.is_null());
    if item.is_null() {
        return -EINVAL_ERRNO;
    }
    let _guard = RADIX_SLOTS_LOCK.lock();
    let slots = unsafe { radix_slots_mut(root) };
    if slots
        .slots
        .get(&index)
        .is_some_and(|slot| !slot.is_null())
    {
        return -EEXIST_ERRNO;
    }
    slots.slots.insert(index, Box::new(item));
    0
}

/// Linux `radix_tree_lookup()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn radix_tree_lookup(root: *mut RadixTreeRoot, index: u64) -> *mut c_void {
    assert!(!root.is_null());
    let _guard = RADIX_SLOTS_LOCK.lock();
    match unsafe { radix_slots_ref(root) } {
        Some(slots) => slots.slots.get(&index).map_or(core::ptr::null_mut(), |slot| **slot),
        None => core::ptr::null_mut(),
    }
}

/// Linux `radix_tree_delete()`: remove `index` and return the old entry.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn radix_tree_delete(root: *mut RadixTreeRoot, index: u64) -> *mut c_void {
    assert!(!root.is_null());
    let _guard = RADIX_SLOTS_LOCK.lock();
    match unsafe { radix_slots_ref(root) } {
        Some(_) => {
            let slots = unsafe { radix_slots_mut(root) };
            let removed = slots
                .slots
                .remove(&index)
                .map_or(core::ptr::null_mut(), |slot| *slot);
            unsafe { radix_release_if_empty(root) };
            removed
        }
        None => core::ptr::null_mut(),
    }
}

/// Linux `radix_tree_next_chunk()`: the next present slot at or after
/// `iter->next_index`, or NULL at the end. `RADIX_TREE_ITER_CONTIG` stops at
/// the first hole, and tagged iteration finds nothing since no tags are kept.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn radix_tree_next_chunk(
    root: *const RadixTreeRoot,
    iter: *mut RadixIter,
    flags: u32,
) -> *mut *mut c_void {
    assert!(!root.is_null() && !iter.is_null());
    let _guard = RADIX_SLOTS_LOCK.lock();
    if flags & RADIX_TREE_ITER_TAGGED != 0 {
        return core::ptr::null_mut();
    }
    let start = unsafe { (*iter).next_index };
    // A zero next_index after the first chunk marks the end of iteration.
    if start == 0 && unsafe { (*iter).index } != 0 {
        return core::ptr::null_mut();
    }
    let Some(slots) = (unsafe { radix_slots_ref(root) }) else {
        return core::ptr::null_mut();
    };
    let Some((&key, slot)) = slots
        .slots
        .range(start..)
        .find(|(_, slot)| !slot.is_null())
    else {
        return core::ptr::null_mut();
    };
    let slot_ptr: *mut *mut c_void = core::ptr::addr_of!(**slot).cast_mut();
    unsafe {
        if flags & RADIX_TREE_ITER_CONTIG != 0 && (*iter).index != 0 && key != start {
            (*iter).next_index = 0;
            return core::ptr::null_mut();
        }
        (*iter).index = c_ulong::try_from(key).unwrap_or(c_ulong::MAX);
        (*iter).next_index = (*iter).index.wrapping_add(1);
        (*iter).tags = 1;
        (*iter).node = core::ptr::null_mut();
    }
    slot_ptr
}

/// Linux `radix_tree_next_slot()`. Every chunk covers exactly one slot, so
/// the chunk is exhausted on return and the caller asks for the next chunk.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn radix_tree_next_slot(
    _slot: *mut *mut c_void,
    _iter: *mut RadixIter,
    _flags: u32,
) -> *mut *mut c_void {
    core::ptr::null_mut()
}

/// Linux `radix_tree_iter_delete()`: remove the entry at `iter->index` and
/// clear `*slot`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn radix_tree_iter_delete(
    root: *mut RadixTreeRoot,
    iter: *mut RadixIter,
    slot: *mut *mut c_void,
) {
    assert!(!root.is_null() && !iter.is_null());
    let _guard = RADIX_SLOTS_LOCK.lock();
    if unsafe { radix_slots_ref(root) }.is_none() {
        return;
    }
    let index = unsafe { (*iter).index } as u64;
    let slots = unsafe { radix_slots_mut(root) };
    if let Some(mut storage) = slots.slots.remove(&index) {
        *storage = core::ptr::null_mut();
        if !slot.is_null() {
            unsafe { *slot = core::ptr::null_mut() };
        }
    }
    unsafe { radix_release_if_empty(root) };
}

pub const IDR_RESERVED_ENTRY: *mut c_void = usize::MAX as *mut c_void;

/// Linux `idr_init_base()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idr_init_base(idr: *mut Idr, base: c_int) {
    if !idr.is_null() {
        unsafe {
            core::ptr::write_bytes(idr, 0, 1);
            (*idr).idr_base = base as u32;
            (*idr).idr_next = base as u32;
        }
    }
}

/// Linux `idr_init()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idr_init(idr: *mut Idr) {
    unsafe { idr_init_base(idr, 0) };
}

/// Linux `idr_destroy()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idr_destroy(idr: *mut Idr) {
    if !idr.is_null() {
        unsafe { radix_tree_destroy(core::ptr::addr_of_mut!((*idr).idr_rt)) };
    }
}

/// Linux `idr_is_empty()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idr_is_empty(idr: *const Idr) -> bool {
    if idr.is_null() {
        return true;
    }
    let _guard = RADIX_SLOTS_LOCK.lock();
    match unsafe { radix_slots_ref(core::ptr::addr_of!((*idr).idr_rt)) } {
        Some(slots) => slots.slots.is_empty(),
        None => true,
    }
}

/// Linux `idr_alloc()`: allocates an ID in `[start, end)`. `end == 0` means INT_MAX.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idr_alloc(
    idr: *mut Idr,
    ptr: *mut c_void,
    start: i32,
    end: i32,
) -> i32 {
    if idr.is_null() {
        return -crate::config::EINVAL;
    }
    let _guard = RADIX_SLOTS_LOCK.lock();
    let base = unsafe { (*idr).idr_base };
    let max = if end <= 0 { i32::MAX } else { end - 1 };
    let start_id = start.max(base as i32);
    if start_id > max {
        return -crate::config::ENOSPC;
    }
    let slots = unsafe { radix_slots_mut(core::ptr::addr_of_mut!((*idr).idr_rt)) };
    let key_start = (start_id as u64).saturating_sub(base as u64);
    let key_max = (max as u64).saturating_sub(base as u64);
    let mut candidate_key = key_start;
    for (&k, _) in slots.slots.range(key_start..=key_max) {
        if k == candidate_key {
            candidate_key = candidate_key.wrapping_add(1);
        } else if k > candidate_key {
            break;
        }
    }
    if candidate_key > key_max {
        return -crate::config::ENOSPC;
    }
    let entry = if ptr.is_null() {
        IDR_RESERVED_ENTRY
    } else {
        ptr
    };
    slots.slots.insert(candidate_key, Box::new(entry));
    (candidate_key + base as u64) as i32
}

/// Linux `idr_replace()`: replaces the entry at `id` with `ptr`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idr_replace(
    idr: *mut Idr,
    ptr: *mut c_void,
    id: i32,
) -> *mut c_void {
    if idr.is_null() {
        return core::ptr::null_mut();
    }
    let _guard = RADIX_SLOTS_LOCK.lock();
    let base = unsafe { (*idr).idr_base };
    if (id as u64) < (base as u64) {
        return core::ptr::null_mut();
    }
    let key = (id as u64) - (base as u64);
    let slots = unsafe { radix_slots_mut(core::ptr::addr_of_mut!((*idr).idr_rt)) };
    if let Some(slot) = slots.slots.get_mut(&key) {
        let old = **slot;
        let new_val = if ptr.is_null() {
            IDR_RESERVED_ENTRY
        } else {
            ptr
        };
        **slot = new_val;
        if old == IDR_RESERVED_ENTRY {
            core::ptr::null_mut()
        } else {
            old
        }
    } else {
        core::ptr::null_mut()
    }
}

/// Linux `idr_remove()`: removes `id` from the idr.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idr_remove(idr: *mut Idr, id: i32) -> *mut c_void {
    if idr.is_null() {
        return core::ptr::null_mut();
    }
    let _guard = RADIX_SLOTS_LOCK.lock();
    let base = unsafe { (*idr).idr_base };
    if (id as u64) < (base as u64) {
        return core::ptr::null_mut();
    }
    let key = (id as u64) - (base as u64);
    let slots = unsafe { radix_slots_mut(core::ptr::addr_of_mut!((*idr).idr_rt)) };
    let old = slots.slots.remove(&key).map_or(core::ptr::null_mut(), |slot| *slot);
    unsafe { radix_release_if_empty(core::ptr::addr_of_mut!((*idr).idr_rt)) };
    if old == IDR_RESERVED_ENTRY {
        core::ptr::null_mut()
    } else {
        old
    }
}

/// Linux `idr_find()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idr_find(idr: *const Idr, id: c_ulong) -> *mut c_void {
    if idr.is_null() {
        return core::ptr::null_mut();
    }
    let _guard = RADIX_SLOTS_LOCK.lock();
    let base = unsafe { (*idr).idr_base } as u64;
    if (id as u64) < base {
        return core::ptr::null_mut();
    }
    let key = (id as u64) - base;
    match unsafe { radix_slots_ref(core::ptr::addr_of!((*idr).idr_rt)) } {
        Some(slots) => {
            let ptr = slots.slots.get(&key).map_or(core::ptr::null_mut(), |slot| **slot);
            if ptr == IDR_RESERVED_ENTRY {
                core::ptr::null_mut()
            } else {
                ptr
            }
        }
        None => core::ptr::null_mut(),
    }
}

/// Linux `idr_get_next()`: the first entry whose id is at least `*nextid`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idr_get_next(idr: *mut Idr, nextid: *mut c_int) -> *mut c_void {
    assert!(!idr.is_null() && !nextid.is_null());
    let _guard = RADIX_SLOTS_LOCK.lock();
    let base = unsafe { (*idr).idr_base } as u64;
    let id = unsafe { *nextid } as u32 as u64;
    let start = if id < base { 0 } else { id - base };
    let Some(slots) = (unsafe { radix_slots_ref(core::ptr::addr_of!((*idr).idr_rt)) }) else {
        return core::ptr::null_mut();
    };
    let Some((&key, slot)) = slots
        .slots
        .range(start..)
        .find(|(_, slot)| !(***slot).is_null() && ***slot != IDR_RESERVED_ENTRY)
    else {
        return core::ptr::null_mut();
    };
    let found = key + base;
    if found > i32::MAX as u64 {
        return core::ptr::null_mut();
    }
    unsafe { *nextid = found as c_int };
    **slot
}

/// Linux `idr_for_each()`.
pub unsafe fn idr_for_each<F>(idr: *mut Idr, mut f: F)
where
    F: FnMut(i32, *mut c_void),
{
    if idr.is_null() {
        return;
    }
    let entries: alloc::vec::Vec<(i32, *mut c_void)> = {
        let _guard = RADIX_SLOTS_LOCK.lock();
        let base = unsafe { (*idr).idr_base } as u64;
        match unsafe { radix_slots_ref(core::ptr::addr_of!((*idr).idr_rt)) } {
            Some(slots) => slots
                .slots
                .iter()
                .filter(|(_, slot)| !(***slot).is_null() && ***slot != IDR_RESERVED_ENTRY)
                .map(|(&k, slot)| ((k + base) as i32, **slot))
                .collect(),
            None => alloc::vec::Vec::new(),
        }
    };
    for (id, ptr) in entries {
        f(id, ptr);
    }
}

#[inline]
pub unsafe extern "C" fn idr_preload(_gfp: u32) {}

#[inline]
pub unsafe extern "C" fn idr_preload_end() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_idr_unified_radix_operations() {
        unsafe {
            let mut idr: Idr = core::mem::zeroed();
            idr_init_base(&mut idr, 1);
            assert!(idr_is_empty(&idr));

            let p1 = 0x1000usize as *mut c_void;
            let p2 = 0x2000usize as *mut c_void;
            let p3 = 0x3000usize as *mut c_void;

            let id1 = idr_alloc(&mut idr, p1, 1, 0);
            assert_eq!(id1, 1);
            let id2 = idr_alloc(&mut idr, p2, 1, 0);
            assert_eq!(id2, 2);
            let id3 = idr_alloc(&mut idr, p3, 5, 10);
            assert_eq!(id3, 5);

            assert!(!idr_is_empty(&idr));

            // idr_find
            assert_eq!(idr_find(&idr, 1), p1);
            assert_eq!(idr_find(&idr, 2), p2);
            assert_eq!(idr_find(&idr, 5), p3);
            assert!(idr_find(&idr, 3).is_null());

            // idr_get_next
            let mut next = 1;
            assert_eq!(idr_get_next(&mut idr, &mut next), p1);
            assert_eq!(next, 1);

            next = 2;
            assert_eq!(idr_get_next(&mut idr, &mut next), p2);
            assert_eq!(next, 2);

            next = 3;
            assert_eq!(idr_get_next(&mut idr, &mut next), p3);
            assert_eq!(next, 5);

            next = 6;
            assert!(idr_get_next(&mut idr, &mut next).is_null());

            // idr_for_each
            let mut visited = alloc::vec::Vec::new();
            idr_for_each(&mut idr, |id, ptr| {
                visited.push((id, ptr));
            });
            assert_eq!(visited, alloc::vec![(1, p1), (2, p2), (5, p3)]);

            // idr_replace
            let p1_new = 0x1111usize as *mut c_void;
            assert_eq!(idr_replace(&mut idr, p1_new, 1), p1);
            assert_eq!(idr_find(&idr, 1), p1_new);

            // idr_remove
            assert_eq!(idr_remove(&mut idr, 2), p2);
            assert!(idr_find(&idr, 2).is_null());

            idr_destroy(&mut idr);
            assert!(idr_is_empty(&idr));
        }
    }
}
