// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Linux XArray surface used by the i915 uC lookup tables.
//!
//! The outer `struct xarray` storage follows include/linux/xarray.h; this Rust
//! backend stores the indexed entries in an ordered map behind `xa_head` and
//! preserves the public load/store/erase/allocation and lock contracts.

use alloc::{boxed::Box, collections::BTreeMap, vec::Vec};
use core::{ffi::c_void, mem::offset_of};

use crate::{
    intel_engine_cs_upstream::Spinlock,
    linux_config::{GFP_ATOMIC, GFP_KERNEL},
    linux_locks::{
        spin_lock, spin_lock_irq, spin_lock_irqsave, spin_unlock, spin_unlock_irq,
        spin_unlock_irqrestore,
    },
};

#[repr(C)]
#[derive(Clone, Copy)]
pub struct XArray {
    pub(crate) xa_lock: Spinlock,
    pub(crate) xa_flags: u32,
    xa_head: *mut XaEntries,
}

type XArrayHeader = XArray;

#[derive(Default)]
struct XaEntries {
    entries: BTreeMap<u32, *mut c_void>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct XaLimit {
    pub min: u32,
    pub max: u32,
}

pub const XA_FLAGS_ALLOC: u32 = 1 << 1;
pub const XA_FLAGS_LOCK_IRQ: u32 = 1 << 2;
pub const xa_limit_32b: XaLimit = XaLimit {
    min: 0,
    max: u32::MAX,
};

#[inline]
unsafe fn header<T>(array: *mut T) -> *mut XArrayHeader {
    array.cast()
}

unsafe fn entries<T>(array: *mut T, allocate: bool) -> *mut XaEntries {
    let head = core::ptr::addr_of_mut!((*header(array)).xa_head);
    if (*head).is_null() && allocate {
        *head = Box::into_raw(Box::new(XaEntries::default()));
    }
    *head
}

pub fn xa_init_flags<T>(array: &mut T, flags: u32) {
    unsafe {
        (*header(array)).xa_flags = flags;
        let _ = entries(array, true);
    }
}

pub fn xa_destroy<T>(array: &mut T) {
    unsafe {
        let head = core::ptr::addr_of_mut!((*header(array)).xa_head);
        if !(*head).is_null() {
            drop(Box::from_raw(*head));
            *head = core::ptr::null_mut();
        }
    }
}

pub fn xa_lock<T>(array: &mut T) {
    unsafe { spin_lock(&mut (*header(array)).xa_lock) }
}

pub fn xa_unlock<T>(array: &mut T) {
    unsafe { spin_unlock(&mut (*header(array)).xa_lock) }
}

pub fn xa_lock_irq<T>(array: &mut T) {
    unsafe { spin_lock_irq(&mut (*header(array)).xa_lock) }
}

pub fn xa_unlock_irq<T>(array: &mut T) {
    unsafe { spin_unlock_irq(&mut (*header(array)).xa_lock) }
}

pub fn xa_lock_irqsave<T>(array: &mut T, flags: &mut u64) {
    unsafe { spin_lock_irqsave(&mut (*header(array)).xa_lock, flags) }
}

pub fn xa_unlock_irqrestore<T>(array: &mut T, flags: u64) {
    unsafe { spin_unlock_irqrestore(&mut (*header(array)).xa_lock, flags) }
}

pub fn xa_load<T, V>(array: &mut T, index: u32) -> *mut V {
    unsafe {
        let head = entries(array, false);
        if head.is_null() {
            return core::ptr::null_mut();
        }
        (*head)
            .entries
            .get(&index)
            .copied()
            .unwrap_or(core::ptr::null_mut())
            .cast()
    }
}

pub fn __xa_store<T, V>(array: &mut T, index: u32, value: *mut V, _flags: u32) -> *mut V {
    unsafe {
        let head = entries(array, true);
        (*head)
            .entries
            .insert(index, value.cast())
            .unwrap_or(core::ptr::null_mut())
            .cast()
    }
}

pub fn __xa_erase<T, V>(array: &mut T, index: u32) -> *mut V {
    unsafe {
        let head = entries(array, false);
        if head.is_null() {
            return core::ptr::null_mut();
        }
        (*head)
            .entries
            .remove(&index)
            .unwrap_or(core::ptr::null_mut())
            .cast()
    }
}

pub fn xa_erase_irq<T, V>(array: &mut T, index: u32) -> *mut V {
    let mut flags = 0u64;
    xa_lock_irqsave(array, &mut flags);
    let entry = __xa_erase(array, index);
    xa_unlock_irqrestore(array, flags);
    entry
}

pub fn xa_alloc_cyclic_irq<T, V>(
    array: &mut T,
    index: &mut u32,
    value: *mut V,
    limit: XaLimit,
    next: &mut u32,
    gfp: u32,
) -> i32 {
    if gfp & (1 << 10) == 0 {
        // The Rust heap has no nonblocking allocator API, so preserve Linux's
        // failure contract rather than allocating while IRQs may be disabled.
        return -12;
    }
    if limit.min > limit.max {
        return -22;
    }
    let mut flags = 0u64;
    xa_lock_irqsave(array, &mut flags);
    unsafe {
        let head = entries(array, true);
        let span = u64::from(limit.max) - u64::from(limit.min) + 1;
        let start = (*next).clamp(limit.min, limit.max);
        let mut candidate = start;
        let mut found = false;
        for _ in 0..span {
            if !(*head).entries.contains_key(&candidate) {
                found = true;
                break;
            }
            candidate = if candidate == limit.max {
                limit.min
            } else {
                candidate + 1
            };
        }
        if !found {
            xa_unlock_irqrestore(array, flags);
            return -12;
        }
        (*head).entries.insert(candidate, value.cast());
        *index = candidate;
        *next = if candidate == limit.max {
            limit.min
        } else {
            candidate + 1
        };
    }
    xa_unlock_irqrestore(array, flags);
    0
}

pub(crate) fn xa_snapshot<T>(array: &mut T) -> Vec<(u32, *mut c_void)> {
    unsafe {
        let head = entries(array, false);
        if head.is_null() {
            return Vec::new();
        }
        (*head)
            .entries
            .iter()
            .map(|(index, pointer)| (*index, *pointer))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[repr(C)]
    struct TestXArray {
        lock: Spinlock,
        flags: u32,
        head: *mut XaEntries,
    }

    #[test]
    fn indexed_lookup_and_cyclic_allocation_match_xarray_contract() {
        let mut xa = TestXArray {
            lock: unsafe { core::mem::zeroed() },
            flags: 0,
            head: core::ptr::null_mut(),
        };
        xa_init_flags(&mut xa, XA_FLAGS_ALLOC);
        let a = 1u32;
        let b = 2u32;
        let mut index = 0;
        let mut next = 0;
        assert_eq!(
            xa_alloc_cyclic_irq(
                &mut xa,
                &mut index,
                (&a as *const u32).cast_mut(),
                xa_limit_32b,
                &mut next,
                GFP_KERNEL
            ),
            0
        );
        assert_eq!(index, 0);
        assert_eq!(xa_load::<_, u32>(&mut xa, 0), &a as *const u32 as *mut u32);
        assert_eq!(
            xa_alloc_cyclic_irq(
                &mut xa,
                &mut index,
                (&b as *const u32).cast_mut(),
                xa_limit_32b,
                &mut next,
                GFP_KERNEL
            ),
            0
        );
        assert_eq!(index, 1);
        assert_eq!(
            xa_erase_irq::<_, u32>(&mut xa, 0),
            &a as *const u32 as *mut u32
        );
        xa_destroy(&mut xa);
    }

    #[test]
    fn irq_allocator_fails_closed_for_nonblocking_gfp() {
        let mut xa = TestXArray {
            lock: unsafe { core::mem::zeroed() },
            flags: 0,
            head: core::ptr::null_mut(),
        };
        let mut index = 0;
        let mut next = 0;
        assert_eq!(
            xa_alloc_cyclic_irq(
                &mut xa,
                &mut index,
                core::ptr::null_mut::<u8>(),
                xa_limit_32b,
                &mut next,
                GFP_ATOMIC
            ),
            -12
        );
    }
}

const _: [(); 0] = [(); offset_of!(XArrayHeader, xa_lock)];

macro_rules! xa_for_each {
    ($array:expr, $index:ident, $entry:ident, $body:block) => {{
        let __xa_entries = $crate::linux_xarray::xa_snapshot($array);
        for (__xa_index, __xa_entry) in __xa_entries {
            let mut $index: u32 = __xa_index;
            let mut $entry: *mut _ = __xa_entry.cast();
            $body
        }
    }};
}
