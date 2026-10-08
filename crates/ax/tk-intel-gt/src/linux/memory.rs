// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Linux slab/allocation and reference-counting primitives used by the
//! source-ordered i915 GT bindings.  Allocation storage comes from TheKernel's
//! Rust global allocator; every returned pointer carries enough private
//! metadata for `kfree` to pass the exact original `Layout` back to it.
//!
//! This is deliberately not a Linux VM/reclaim emulator.  The global Rust
//! allocator has no GFP-aware or non-blocking entry point, so allocations that
//! may not sleep (including `GFP_ATOMIC` and `GFP_NOWAIT`) fail closed rather
//! than silently blocking in an IRQ/atomic context.  Sleepable allocations
//! (`GFP_KERNEL` and modifiers) use the kernel global allocator.

#![allow(unsafe_code)]

use alloc::alloc::{Layout, alloc, alloc_zeroed, dealloc};
use core::{
    ffi::c_void,
    mem::{align_of, size_of},
    ptr,
    sync::atomic::{AtomicBool, AtomicI32, Ordering, fence},
};

use crate::{
    intel_context_upstream::{Kref, RefcountT},
    intel_engine_cs_upstream::AtomicT,
    linux_config::{GFP_ATOMIC, GFP_KERNEL},
};

#[allow(non_camel_case_types)]
pub type kref = Kref;
#[allow(non_camel_case_types)]
pub type refcount_t = RefcountT;

// Linux 7.2.3 include/linux/gfp_types.h: ___GFP_DIRECT_RECLAIM_BIT follows
// ___GFP_UNUSED_BIT, hence bit 10 in the wt-dev configuration used here.
const __GFP_DIRECT_RECLAIM: u32 = 1 << 10;
/// Linux 7.2.3 `___GFP_RETRY_MAYFAIL_BIT`.
pub const __GFP_RETRY_MAYFAIL: u32 = 1 << 14;
const __GFP_ZERO: u32 = 1 << 8;
const ZERO_SIZE_PTR: usize = 16;

#[repr(C)]
struct AllocationHeader {
    base: *mut u8,
    layout_size: usize,
    layout_align: usize,
}

/// Allocate raw bytes like Linux `kmalloc`.
///
/// `flags` must describe a sleepable allocation: this implementation rejects
/// requests lacking `__GFP_DIRECT_RECLAIM`, because the Rust global allocator
/// does not expose a GFP_ATOMIC/non-blocking allocator path.  Linux's
/// `__GFP_ZERO` modifier is honored in addition to the separate `kzalloc`
/// entry point.
pub fn kmalloc(size: usize, flags: u32) -> *mut c_void {
    allocate(size, align_of::<usize>(), flags, flags & __GFP_ZERO != 0).cast()
}

/// Allocate and zero raw bytes like Linux `kzalloc`.
pub fn kzalloc(size: usize, flags: u32) -> *mut c_void {
    allocate(size, align_of::<usize>(), flags, true).cast()
}

/// Linux `memset()` for translated fixed-layout records.
///
/// # Safety
/// `destination` must be writable for `count` bytes.
pub unsafe fn memset<T>(destination: *mut T, value: i32, count: usize) -> *mut T {
    if count != 0 {
        unsafe { core::ptr::write_bytes(destination.cast::<u8>(), value as u8, count) };
    }
    destination
}

/// Free a non-null pointer returned by [`kmalloc`], [`kzalloc`], or one of
/// their object/array/flexible-array helpers.  Null and Linux's zero-size
/// sentinel are accepted, matching `kfree`.
///
/// # Safety
/// `object` must be null, the zero-size sentinel, or the exact pointer
/// returned by one of this module's allocation helpers, and must not have
/// already been freed.  As in Linux, freeing a foreign/interior/double-freed
/// pointer is invalid.
pub unsafe fn kfree<T>(object: *mut T) {
    let address = object as usize;
    if object.is_null() || address == ZERO_SIZE_PTR {
        return;
    }

    // The caller guarantees that the private header immediately preceding the
    // object is readable and belongs to this allocator.
    let header = unsafe {
        (object.cast::<u8>().sub(size_of::<AllocationHeader>())).cast::<AllocationHeader>()
    };
    let metadata = unsafe { ptr::read(header) };
    let layout = Layout::from_size_align(metadata.layout_size, metadata.layout_align)
        .expect("kfree received corrupt allocation metadata");
    unsafe { dealloc(metadata.base, layout) };
}

/// Allocate one uninitialized object with the requested GFP policy.
pub fn kmalloc_obj<T>(flags: u32) -> *mut T {
    allocate(
        size_of::<T>(),
        align_of::<T>(),
        flags,
        flags & __GFP_ZERO != 0,
    )
    .cast()
}

/// Allocate one zero-initialized object with `GFP_KERNEL`, as the common
/// source `kzalloc_obj` call form does.
pub fn kzalloc_obj<T>() -> *mut T {
    kzalloc_obj_flags::<T>(GFP_KERNEL)
}

/// Allocate one zero-initialized object with explicit GFP flags.
pub fn kzalloc_obj_flags<T>(flags: u32) -> *mut T {
    allocate(size_of::<T>(), align_of::<T>(), flags, true).cast()
}

/// Allocate an uninitialized array with `GFP_KERNEL`.
pub fn kmalloc_objs<T, N>(count: N) -> *mut T
where
    N: TryInto<usize>,
{
    kmalloc_objs_flags::<T, N>(count, GFP_KERNEL)
}

/// Allocate a zero-initialized array with `GFP_KERNEL`.
pub fn kzalloc_objs<T, N>(count: N) -> *mut T
where
    N: TryInto<usize>,
{
    kzalloc_objs_flags::<T, N>(count, GFP_KERNEL)
}

/// Allocate an uninitialized array with explicit GFP flags.
pub fn kmalloc_objs_flags<T, N>(count: N, flags: u32) -> *mut T
where
    N: TryInto<usize>,
{
    let Ok(count) = count.try_into() else {
        return ptr::null_mut();
    };
    let Some(size) = size_of::<T>().checked_mul(count) else {
        return ptr::null_mut();
    };
    allocate(size, align_of::<T>(), flags, flags & __GFP_ZERO != 0).cast()
}

/// Allocate a zero-initialized array with explicit GFP flags.
pub fn kzalloc_objs_flags<T, N>(count: N, flags: u32) -> *mut T
where
    N: TryInto<usize>,
{
    let Ok(count) = count.try_into() else {
        return ptr::null_mut();
    };
    let Some(size) = size_of::<T>().checked_mul(count) else {
        return ptr::null_mut();
    };
    allocate(size, align_of::<T>(), flags, true).cast()
}

/// Flexible-array field element sizing used by the `kzalloc_flex!` binding.
#[doc(hidden)]
pub trait FlexibleArray {
    const ELEMENT_SIZE: usize;
}

impl<T, const N: usize> FlexibleArray for [T; N] {
    const ELEMENT_SIZE: usize = size_of::<T>();
}

/// Allocate and zero a C-style structure followed by `count` trailing array
/// elements.  `member` is a type-inference closure for the flexible member;
/// `member_offset` is its source-layout offset.
#[doc(hidden)]
pub fn kzalloc_flex_impl<T, A, N, F>(count: N, member_offset: usize, member: F) -> *mut T
where
    A: FlexibleArray,
    N: TryInto<usize>,
    F: FnOnce(*mut T) -> *mut A,
{
    let Ok(count) = count.try_into() else {
        return ptr::null_mut();
    };
    // Constructing a dangling base is sufficient: the closure only forms a
    // raw field address and must not read the object.
    let base = ptr::NonNull::<T>::dangling().as_ptr();
    let _member = member(base);
    let Some(tail) = A::ELEMENT_SIZE.checked_mul(count) else {
        return ptr::null_mut();
    };
    let Some(size) = size_of::<T>().max(member_offset).checked_add(tail) else {
        return ptr::null_mut();
    };
    allocate(size, align_of::<T>(), GFP_KERNEL, true).cast()
}

/// Atomic read (`atomic_read`, relaxed ordering).
#[inline]
pub fn atomic_read(value: &AtomicT) -> i32 {
    atomic(value).load(Ordering::Relaxed)
}

/// Atomic set (`atomic_set`, relaxed ordering).
#[inline]
pub fn atomic_set(value: &mut AtomicT, new_value: i32) {
    atomic(value).store(new_value, Ordering::Relaxed);
}

/// Atomic increment (`atomic_inc`, relaxed ordering).
#[inline]
pub fn atomic_inc(value: &mut AtomicT) {
    atomic(value).fetch_add(1, Ordering::Relaxed);
}

/// Atomic decrement (`atomic_dec`, relaxed ordering).
#[inline]
pub fn atomic_dec(value: &mut AtomicT) {
    atomic(value).fetch_sub(1, Ordering::Relaxed);
}

/// Atomic addition (`atomic_add`, relaxed ordering).
#[inline]
pub fn atomic_add(addend: i32, value: &mut AtomicT) {
    atomic(value).fetch_add(addend, Ordering::Relaxed);
}

/// Atomic add-unless (`atomic_add_unless`, full ordering).
#[inline]
pub fn atomic_add_unless(value: &mut AtomicT, addend: i32, unless: i32) -> bool {
    let counter = atomic(value);
    let mut old = counter.load(Ordering::Relaxed);
    loop {
        if old == unless {
            return false;
        }
        match counter.compare_exchange_weak(
            old,
            old.wrapping_add(addend),
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => return true,
            Err(observed) => old = observed,
        }
    }
}

/// Atomic decrement-and-test (`atomic_dec_and_test`, full ordering).
#[inline]
pub fn atomic_dec_and_test(value: &mut AtomicT) -> bool {
    atomic(value).fetch_sub(1, Ordering::SeqCst).wrapping_sub(1) == 0
}

/// Atomic subtract-and-test (`atomic_sub_and_test`, full ordering).
#[inline]
pub fn atomic_sub_and_test(subtrahend: i32, value: &mut AtomicT) -> bool {
    atomic(value)
        .fetch_sub(subtrahend, Ordering::SeqCst)
        .wrapping_sub(subtrahend)
        == 0
}

/// Atomic fetch-increment (`atomic_fetch_inc`, full ordering), returning the
/// value before incrementing.
#[inline]
pub fn atomic_fetch_inc(value: &mut AtomicT) -> i32 {
    atomic(value).fetch_add(1, Ordering::SeqCst)
}

/// Initialize a Linux `kref` to one reference.
#[inline]
pub unsafe fn kref_init(value: *mut Kref) {
    // SAFETY: the caller provides a live `kref`, matching Linux's pointer API.
    let reference = unsafe { &mut (*value).refcount };
    refcount_set(reference, 1);
}

/// Read a reference count.  The generic view accepts the embedded `refcount_t`
/// used by dma-fence as well as the enclosing `kref` used by i915 objects.
#[inline]
pub fn kref_read<T: RefcountView + ?Sized>(value: &T) -> u32 {
    value.counter().load(Ordering::Relaxed) as u32
}

/// Increment a `kref` unless it has reached zero (`kref_get_unless_zero`).
#[inline]
pub fn kref_get_unless_zero(value: &mut Kref) -> bool {
    refcount_inc_not_zero(&mut value.refcount)
}

/// Set a Linux `refcount_t` using its relaxed initialization ordering.
#[inline]
pub fn refcount_set(value: &mut RefcountT, new_value: i32) {
    atomic(&mut value.refs).store(new_value, Ordering::Relaxed);
}

/// Read a Linux `refcount_t` (`refcount_read`, relaxed ordering).
#[inline]
pub fn refcount_read(value: &RefcountT) -> u32 {
    atomic(&value.refs).load(Ordering::Relaxed) as u32
}

/// Increment a `refcount_t` unless it is zero.  Invalid overflows saturate at
/// Linux's `REFCOUNT_SATURATED` value instead of wrapping back to zero.
pub fn refcount_inc_not_zero(value: &mut RefcountT) -> bool {
    let counter = atomic(&value.refs);
    let mut old = counter.load(Ordering::Relaxed);
    loop {
        if old == 0 {
            return false;
        }
        if old < 0 || old == i32::MAX {
            refcount_saturate(counter, "increment overflow");
            return true;
        }
        match counter.compare_exchange_weak(old, old + 1, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return true,
            Err(observed) => old = observed,
        }
    }
}

/// Decrement a `refcount_t`, returning true only for the valid 1-to-0
/// transition (`refcount_dec_and_test`).
pub fn refcount_dec_and_test(value: &mut RefcountT) -> bool {
    let counter = atomic(&value.refs);
    let old = counter.fetch_sub(1, Ordering::Release);
    if old == 1 {
        fence(Ordering::Acquire);
        return true;
    }
    if old <= 0 {
        refcount_saturate(counter, "decrement underflow");
    }
    false
}

/// Decrement without testing (`refcount_dec`).  Linux deliberately leaks on a
/// decrement from one or below: callers that need to release the final object
/// must use `refcount_dec_and_test` and run its destructor themselves.
pub fn refcount_dec(value: &mut RefcountT) {
    let counter = atomic(&value.refs);
    let old = counter.fetch_sub(1, Ordering::Release);
    if old <= 1 {
        refcount_saturate(counter, "decrement would release without test");
    }
}

fn allocate(size: usize, alignment: usize, flags: u32, zero: bool) -> *mut u8 {
    if size == 0 {
        return ZERO_SIZE_PTR as *mut u8;
    }
    if !supports_sleepable_allocation(flags) {
        return ptr::null_mut();
    }

    let Ok(payload) = Layout::from_size_align(size, alignment.max(1)) else {
        return ptr::null_mut();
    };
    let Ok((layout, payload_offset)) = Layout::new::<AllocationHeader>().extend(payload) else {
        return ptr::null_mut();
    };
    let layout = layout.pad_to_align();
    let base = unsafe {
        if zero {
            alloc_zeroed(layout)
        } else {
            alloc(layout)
        }
    };
    if base.is_null() {
        return ptr::null_mut();
    }

    // `Layout::extend` aligns the payload after the header.  The header itself
    // is placed immediately before the returned address; because the payload
    // alignment is at least the header alignment, this address is aligned too.
    let object = unsafe { base.add(payload_offset) };
    let header = unsafe { object.sub(size_of::<AllocationHeader>()) }.cast::<AllocationHeader>();
    unsafe {
        ptr::write(
            header,
            AllocationHeader {
                base,
                layout_size: layout.size(),
                layout_align: layout.align(),
            },
        );
    }
    object
}

fn supports_sleepable_allocation(flags: u32) -> bool {
    // GFP_ATOMIC contains __GFP_HIGH and __GFP_KSWAPD_RECLAIM, but not
    // __GFP_DIRECT_RECLAIM.  Testing the source direct-reclaim bit also rejects
    // GFP_NOWAIT and other non-sleepable combinations.  The `GFP_KERNEL` check
    // documents the only base policy enabled by this compatibility layer.
    let _known_sleepable_base = GFP_KERNEL;
    flags & __GFP_DIRECT_RECLAIM != 0 && flags & GFP_ATOMIC != GFP_ATOMIC
}

fn atomic(value: &AtomicT) -> &AtomicI32 {
    // AtomicT is the source-layout binding for Linux atomic_t: a repr(C) i32
    // counter at the same 4-byte alignment as AtomicI32.  All access to this
    // counter in the LinuxKPI layer is performed through this atomic view.
    let counter = ptr::addr_of!(value.counter).cast_mut();
    // SAFETY: AtomicT and AtomicI32 have matching size/alignment here, and the
    // referenced counter lives for at least the duration of the borrow.
    unsafe { AtomicI32::from_ptr(counter) }
}

/// Read-only view of the atomic counter embedded in either `refcount_t` or
/// `kref` layout bindings.
pub trait RefcountView {
    #[doc(hidden)]
    fn counter(&self) -> &AtomicI32;
}

impl RefcountView for RefcountT {
    fn counter(&self) -> &AtomicI32 {
        atomic(&self.refs)
    }
}

impl RefcountView for Kref {
    fn counter(&self) -> &AtomicI32 {
        atomic(&self.refcount.refs)
    }
}

fn refcount_saturate(counter: &AtomicI32, reason: &'static str) {
    static WARNED: AtomicBool = AtomicBool::new(false);
    counter.store(i32::MIN / 2, Ordering::Relaxed);
    if !WARNED.swap(true, Ordering::Relaxed) {
        axlog::warn!("LinuxKPI refcount saturated: {}", reason);
    }
}

/// Allocate one object with `GFP_KERNEL`, preserving source C helper spelling.
#[macro_export]
macro_rules! kzalloc_obj {
    ($type:ty $(,)?) => {{ $crate::linux_memory::kzalloc_obj::<$type>() }};
    ($type:ty, $flags:expr $(,)?) => {{ $crate::linux_memory::kzalloc_obj_flags::<$type>($flags) }};
}

/// Allocate one uninitialized object with explicit GFP flags.
#[macro_export]
macro_rules! kmalloc_obj {
    ($type:ty $(,)?) => {{ $crate::linux_memory::kmalloc_obj::<$type>($crate::linux_config::GFP_KERNEL) }};
    ($type:ty, $flags:expr $(,)?) => {{ $crate::linux_memory::kmalloc_obj::<$type>($flags) }};
}

/// Allocate an object array with `GFP_KERNEL` or explicit flags.
#[macro_export]
macro_rules! kmalloc_objs {
    ($type:ty, $count:expr $(,)?) => {{ $crate::linux_memory::kmalloc_objs::<$type, _>($count) }};
    ($type:ty, $count:expr, $flags:expr $(,)?) => {{ $crate::linux_memory::kmalloc_objs_flags::<$type, _>($count, $flags) }};
}

/// Allocate and zero an object array with `GFP_KERNEL` or explicit flags.
#[macro_export]
macro_rules! kzalloc_objs {
    ($type:ty, $count:expr $(,)?) => {{ $crate::linux_memory::kzalloc_objs::<$type, _>($count) }};
    ($type:ty, $count:expr, $flags:expr $(,)?) => {{ $crate::linux_memory::kzalloc_objs_flags::<$type, _>($count, $flags) }};
}

/// Allocate a zeroed C-style structure with trailing flexible-array members.
#[macro_export]
macro_rules! kzalloc_flex {
    ($type:ty, $member:ident, $count:expr $(,)?) => {{
        $crate::linux_memory::kzalloc_flex_impl::<$type, _, _, _>(
            $count,
            core::mem::offset_of!($type, $member),
            |base: *mut $type| unsafe { core::ptr::addr_of_mut!((*base).$member) },
        )
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[repr(C, align(64))]
    struct CacheLine([u8; 73]);

    #[repr(C)]
    struct Flex {
        count: u32,
        values: [u64; 0],
    }

    #[test]
    fn allocation_tracks_layout_and_zeroing() {
        unsafe {
            let object = kzalloc_obj_flags::<CacheLine>(GFP_KERNEL);
            assert!(!object.is_null());
            assert_eq!(object as usize % align_of::<CacheLine>(), 0);
            assert!((*object).0.iter().all(|byte| *byte == 0));
            (*object).0[0] = 7;
            kfree(object);

            let bytes = kmalloc(17, GFP_KERNEL).cast::<u8>();
            assert!(!bytes.is_null());
            kfree(bytes);
        }
    }

    #[test]
    fn unsupported_atomic_allocation_fails_closed() {
        assert!(kmalloc(8, GFP_ATOMIC).is_null());
    }

    #[test]
    fn flexible_array_size_is_checked_and_zeroed() {
        unsafe {
            let count = 3usize;
            let value = kzalloc_flex_impl::<Flex, _, _, _>(
                count,
                core::mem::offset_of!(Flex, values),
                |base| ptr::addr_of_mut!((*base).values),
            );
            assert!(!value.is_null());
            let tail = ptr::addr_of_mut!((*value).values).cast::<u64>();
            assert_eq!(*tail.add(2), 0);
            kfree(value);
        }
        let too_many = kzalloc_objs::<u64, _>(u128::MAX);
        assert!(too_many.is_null());
    }

    #[test]
    fn atomic_and_refcount_ordered_transitions() {
        let mut atomic = AtomicT { counter: 0 };
        atomic_set(&mut atomic, 2);
        assert_eq!(atomic_read(&atomic), 2);
        assert!(atomic_add_unless(&mut atomic, 1, 0));
        assert_eq!(atomic_fetch_inc(&mut atomic), 3);
        assert!(!atomic_dec_and_test(&mut atomic));
        assert!(atomic_sub_and_test(3, &mut atomic));

        let mut reference = RefcountT {
            refs: AtomicT { counter: 1 },
        };
        assert!(refcount_inc_not_zero(&mut reference));
        assert_eq!(refcount_read(&reference), 2);
        assert!(!refcount_dec_and_test(&mut reference));
        assert!(refcount_dec_and_test(&mut reference));
        assert!(!refcount_inc_not_zero(&mut reference));
    }
}
