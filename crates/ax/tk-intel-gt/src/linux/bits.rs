// SPDX-License-Identifier: MIT
// Copyright © 2026 Intel Corporation and TheKernel contributors.
// Linux bitops/bitfield operations used by the v7.2.3 i915 headers.

use core::{
    ffi::c_ulong,
    sync::atomic::{AtomicI32, AtomicI64, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering},
};

static BIT_WAITERS: axtask::WaitQueue = axtask::WaitQueue::new();

pub trait BitWord: Copy {
    fn to_u64(self) -> u64;
    fn from_u64(value: u64) -> Self;
    fn load(ptr: *const Self) -> u64;
    fn fetch_or(ptr: *mut Self, mask: u64) -> u64;
    fn fetch_and(ptr: *mut Self, mask: u64) -> u64;
    fn store(ptr: *mut Self, value: u64);
}

macro_rules! impl_word {
    ($ty:ty, $atomic:ty, $bits:ty) => {
        impl BitWord for $ty {
            fn to_u64(self) -> u64 {
                self as u64
            }
            fn from_u64(value: u64) -> Self {
                value as Self
            }
            fn load(ptr: *const Self) -> u64 {
                unsafe { &*ptr.cast::<$atomic>() }.load(Ordering::Acquire) as u64
            }
            fn fetch_or(ptr: *mut Self, mask: u64) -> u64 {
                unsafe { &*ptr.cast::<$atomic>() }.fetch_or(mask as $bits, Ordering::AcqRel) as u64
            }
            fn fetch_and(ptr: *mut Self, mask: u64) -> u64 {
                unsafe { &*ptr.cast::<$atomic>() }.fetch_and(mask as $bits, Ordering::AcqRel) as u64
            }
            fn store(ptr: *mut Self, value: u64) {
                unsafe { &*ptr.cast::<$atomic>() }.store(value as $bits, Ordering::Release)
            }
        }
    };
}

impl_word!(u32, AtomicU32, u32);
impl_word!(i32, AtomicU32, u32);
impl_word!(u64, AtomicU64, u64);
impl_word!(i64, AtomicU64, u64);
impl_word!(usize, AtomicU64, u64);

pub trait BitIndex: Copy {
    fn index(self) -> u32;
}
impl BitIndex for u32 {
    fn index(self) -> u32 {
        self
    }
}
impl BitIndex for i32 {
    fn index(self) -> u32 {
        self as u32
    }
}
impl BitIndex for usize {
    fn index(self) -> u32 {
        self as u32
    }
}

#[inline]
pub fn test_bit<I: BitIndex, W: BitWord>(bit: I, word: &W) -> bool {
    W::load(word) & (1u64 << (bit.index() % (core::mem::size_of::<W>() as u32 * 8))) != 0
}

#[inline]
pub fn set_bit<I: BitIndex, W: BitWord>(bit: I, word: &mut W) {
    let bit = bit.index() % (core::mem::size_of::<W>() as u32 * 8);
    let _ = W::fetch_or(word, 1u64 << bit);
}

#[inline]
pub fn clear_bit<I: BitIndex, W: BitWord>(bit: I, word: &mut W) {
    let bit = bit.index() % (core::mem::size_of::<W>() as u32 * 8);
    let _ = W::fetch_and(word, !(1u64 << bit));
}

/// Release-ordered Linux `clear_bit_unlock()` primitive.
#[inline]
pub fn clear_bit_unlock<I: BitIndex, W: BitWord>(bit: I, word: &mut W) {
    let bit = bit.index() % (core::mem::size_of::<W>() as u32 * 8);
    let _ = W::fetch_and(word, !(1u64 << bit));
}

/// Linux `clear_and_wake_up_bit()` with a shared waiter queue. Waking waiters
/// on unrelated bits is permitted; every waiter rechecks its own bit predicate.
pub fn clear_and_wake_up_bit_inner<I: BitIndex, W: BitWord>(bit: I, word: &mut W) {
    clear_bit_unlock(bit, word);
    BIT_WAITERS.notify_all(false);
}

/// Linux `clear_and_wake_up_bit()` C ABI. The wait-queue key is shared across
/// bit addresses; collisions only wake waiters to recheck their own predicate.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clear_and_wake_up_bit(bit: i32, word: *mut c_ulong) {
    assert!(bit >= 0 && !word.is_null());
    clear_and_wake_up_bit_inner(bit as u32, unsafe { &mut *word });
}

/// Linux `wait_on_bit()` over the same predicate-checked LinuxKPI wait queue.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wait_on_bit(word: *mut c_ulong, bit: i32, state: i32) -> i32 {
    if word.is_null() || bit < 0 {
        return -crate::linux_config::EINVAL;
    }
    let word = unsafe { &*word };
    let condition = || !test_bit(bit as u32, word);
    if state as u32 & crate::linux::wait::TASK_INTERRUPTIBLE != 0 {
        match BIT_WAITERS.wait_until_interruptible(condition) {
            Ok(()) => 0,
            Err(axtask::WaitError::Interrupted) => -crate::linux_config::EINTR,
            Err(_) => -crate::linux_config::EIO,
        }
    } else {
        BIT_WAITERS
            .wait_until(condition)
            .map_or(-crate::linux_config::EIO, |_| 0)
    }
}

#[inline]
pub fn test_and_set_bit<I: BitIndex, W: BitWord>(bit: I, word: &mut W) -> bool {
    let bit = bit.index() % (core::mem::size_of::<W>() as u32 * 8);
    W::fetch_or(word, 1u64 << bit) & (1u64 << bit) != 0
}

#[inline]
pub fn test_and_clear_bit<I: BitIndex, W: BitWord>(bit: I, word: &mut W) -> bool {
    let bit = bit.index() % (core::mem::size_of::<W>() as u32 * 8);
    W::fetch_and(word, !(1u64 << bit)) & (1u64 << bit) != 0
}

#[inline]
pub fn __test_and_set_bit<I: BitIndex, W: BitWord>(bit: I, word: &mut W) -> bool {
    let bit = bit.index() % (core::mem::size_of::<W>() as u32 * 8);
    let old = W::load(word);
    W::store(word, old | (1u64 << bit));
    old & (1u64 << bit) != 0
}

#[inline]
pub fn __set_bit<I: BitIndex, W: BitWord>(bit: I, word: &mut W) {
    let bit = bit.index() % (core::mem::size_of::<W>() as u32 * 8);
    let old = W::load(word);
    W::store(word, old | (1u64 << bit));
}

#[inline]
pub fn __clear_bit<I: BitIndex, W: BitWord>(bit: I, word: &mut W) {
    let bit = bit.index() % (core::mem::size_of::<W>() as u32 * 8);
    let old = W::load(word);
    W::store(word, old & !(1u64 << bit));
}

#[inline]
pub fn lower_32_bits<W: BitWord>(value: W) -> u32 {
    value.to_u64() as u32
}

#[inline]
pub fn upper_32_bits<W: BitWord>(value: W) -> u32 {
    (value.to_u64() >> 32) as u32
}

#[inline]
pub fn field_prep(mask: u32, value: u32) -> u32 {
    if mask == 0 {
        0
    } else {
        (value << mask.trailing_zeros()) & mask
    }
}

#[inline]
pub const fn field_fit(mask: u32, value: u32) -> bool {
    value <= (mask >> mask.trailing_zeros())
}

#[inline]
pub const fn order_base_2(n: u32) -> u32 {
    if n <= 1 {
        0
    } else {
        u32::BITS - (n - 1).leading_zeros()
    }
}

#[inline]
pub const fn bit(n: u32) -> u32 {
    1u32.wrapping_shl(n)
}

#[inline]
pub const fn circ_space(tail: u32, head: u32, size: u32) -> u32 {
    tail.wrapping_sub(head.wrapping_add(1)) & (size - 1)
}

#[inline]
pub fn for_each_set_bit(word: u64, limit: u32) -> impl Iterator<Item = u32> {
    (0..limit).filter(move |bit| word & (1u64 << bit) != 0)
}

#[inline]
#[allow(non_snake_case)]
pub fn IS_ALIGNED<V: BitWord, A: BitWord>(value: V, alignment: A) -> bool {
    let alignment = alignment.to_u64();
    alignment != 0 && value.to_u64() & (alignment - 1) == 0
}

/// Exact-value Linux `cmpxchg()` operation. x86 LOCK CMPXCHG provides full ordering.
pub unsafe trait CmpxchgValue: Copy {
    unsafe fn cmpxchg(pointer: *mut Self, old: Self, new: Self) -> Self;
}

unsafe impl<T> CmpxchgValue for *mut T {
    unsafe fn cmpxchg(pointer: *mut Self, old: Self, new: Self) -> Self {
        unsafe { AtomicPtr::from_ptr(pointer) }
            .compare_exchange(old, new, Ordering::SeqCst, Ordering::SeqCst)
            .unwrap_or_else(|actual| actual)
    }
}

unsafe impl<T> CmpxchgValue for *const T {
    unsafe fn cmpxchg(pointer: *mut Self, old: Self, new: Self) -> Self {
        unsafe { AtomicPtr::from_ptr(pointer.cast::<*mut T>()) }
            .compare_exchange(
                old.cast_mut(),
                new.cast_mut(),
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .unwrap_or_else(|actual| actual)
            .cast_const()
    }
}

unsafe impl CmpxchgValue for u32 {
    unsafe fn cmpxchg(pointer: *mut Self, old: Self, new: Self) -> Self {
        unsafe { AtomicU32::from_ptr(pointer) }
            .compare_exchange(old, new, Ordering::SeqCst, Ordering::SeqCst)
            .unwrap_or_else(|actual| actual)
    }
}
unsafe impl CmpxchgValue for u64 {
    unsafe fn cmpxchg(pointer: *mut Self, old: Self, new: Self) -> Self {
        unsafe { AtomicU64::from_ptr(pointer) }
            .compare_exchange(old, new, Ordering::SeqCst, Ordering::SeqCst)
            .unwrap_or_else(|actual| actual)
    }
}
unsafe impl CmpxchgValue for i32 {
    unsafe fn cmpxchg(pointer: *mut Self, old: Self, new: Self) -> Self {
        unsafe { AtomicI32::from_ptr(pointer) }
            .compare_exchange(old, new, Ordering::SeqCst, Ordering::SeqCst)
            .unwrap_or_else(|actual| actual)
    }
}
unsafe impl CmpxchgValue for i64 {
    unsafe fn cmpxchg(pointer: *mut Self, old: Self, new: Self) -> Self {
        unsafe { AtomicI64::from_ptr(pointer) }
            .compare_exchange(old, new, Ordering::SeqCst, Ordering::SeqCst)
            .unwrap_or_else(|actual| actual)
    }
}
unsafe impl CmpxchgValue for usize {
    unsafe fn cmpxchg(pointer: *mut Self, old: Self, new: Self) -> Self {
        unsafe { AtomicUsize::from_ptr(pointer) }
            .compare_exchange(old, new, Ordering::SeqCst, Ordering::SeqCst)
            .unwrap_or_else(|actual| actual)
    }
}

#[inline]
pub unsafe fn cmpxchg<T: CmpxchgValue>(pointer: *mut T, old: T, new: T) -> T {
    unsafe { T::cmpxchg(pointer, old, new) }
}

#[inline]
pub unsafe fn cmpxchg64(pointer: *mut u64, old: u64, new: u64) -> u64 {
    unsafe { <u64 as CmpxchgValue>::cmpxchg(pointer, old, new) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_bitops_match_linux_flag_semantics() {
        let mut flags = 0u64;
        assert!(!test_and_set_bit(35u32, &mut flags));
        assert!(test_bit(35u32, &flags));
        assert!(test_and_clear_bit(35u32, &mut flags));
        assert!(!test_bit(35u32, &flags));
    }

    #[test]
    fn bitfield_and_half_word_helpers_match_linux_macros() {
        assert_eq!(field_prep(0x0000_ff00, 0x42), 0x4200);
        assert!(field_fit(0x0000_ff00, 0xff));
        assert!(!field_fit(0x0000_ff00, 0x100));
        assert_eq!(order_base_2(1), 0);
        assert_eq!(order_base_2(17), 5);
        assert_eq!(circ_space(0, 0, 8), 7);
        assert_eq!(bit(8), 0x100);
        assert_eq!(lower_32_bits(0x1234_5678_9abc_def0u64), 0x9abc_def0);
        assert_eq!(upper_32_bits(0x1234_5678_9abc_def0u64), 0x1234_5678);
        assert!(IS_ALIGNED(0x4000usize, 0x1000usize));
    }
}
