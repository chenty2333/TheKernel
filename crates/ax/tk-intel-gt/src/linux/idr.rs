// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../../LICENSE-MIT.
//! Linux IDA surface backed by the LinuxKPI XArray indexed storage.

use alloc::{boxed::Box, collections::BTreeMap};
use core::ffi::{c_int, c_ulong, c_void};

use crate::{
    intel_context_upstream::RadixTreeRoot,
    linux::{
        config::{GFP_ATOMIC, GFP_KERNEL},
        xarray::{self, XArray},
    },
};

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
const EEXIST_ERRNO: c_int = crate::linux_config::EEXIST;
const EINVAL_ERRNO: c_int = crate::linux_config::EINVAL;

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
    match unsafe { radix_slots_ref(root) } {
        Some(slots) => slots.slots.get(&index).map_or(core::ptr::null_mut(), |slot| **slot),
        None => core::ptr::null_mut(),
    }
}

/// Linux `radix_tree_delete()`: remove `index` and return the old entry.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn radix_tree_delete(root: *mut RadixTreeRoot, index: u64) -> *mut c_void {
    assert!(!root.is_null());
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

/// Linux `idr_find()`: `struct idr` begins with its radix-tree root, followed
/// by `idr_base` and `idr_next`.
#[repr(C)]
struct IdrLayout {
    rt: RadixTreeRoot,
    base: u32,
    next: u32,
}
const _: () = assert!(core::mem::size_of::<IdrLayout>() == 24);

/// Linux `idr_find()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idr_find(idr: *const c_void, id: c_ulong) -> *mut c_void {
    assert!(!idr.is_null());
    let layout = idr.cast::<IdrLayout>();
    let key = id.wrapping_sub(u64::from(unsafe { (*layout).base }) as c_ulong);
    unsafe { radix_tree_lookup(core::ptr::addr_of!((*layout).rt).cast_mut(), key as u64) }
}

/// Linux `idr_get_next()`: the first entry whose id is at least `*nextid`.
/// On success `*nextid` is updated to the id of that entry.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idr_get_next(idr: *mut c_void, nextid: *mut c_int) -> *mut c_void {
    assert!(!idr.is_null() && !nextid.is_null());
    let layout = idr.cast::<IdrLayout>();
    let base = unsafe { (*layout).base } as u64;
    let id = unsafe { *nextid } as u32 as u64;
    let start = if id < base { 0 } else { id - base };
    let Some(slots) = (unsafe { radix_slots_ref(core::ptr::addr_of!((*layout).rt)) }) else {
        return core::ptr::null_mut();
    };
    let Some((&key, slot)) = slots
        .slots
        .range(start..)
        .find(|(_, slot)| !slot.is_null())
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
