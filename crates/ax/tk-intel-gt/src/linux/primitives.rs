// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Small architecture-neutral Linux primitives shared by the imported i915
//! headers. These preserve their integer/ordering semantics; they do not
//! emulate device or GEM operations.

#![allow(unsafe_code)]

use core::{
    ffi::c_void,
    sync::atomic::{Ordering, fence},
};

/// Linux `hweight32()`: population count of the low 32 bits.
#[inline]
pub const fn hweight32(value: u32) -> u32 {
    value.count_ones()
}

/// Linux `hweight8()`: population count of the low eight bits.
#[inline]
pub const fn hweight8(value: u8) -> u32 {
    value.count_ones()
}

pub trait LinuxUnsigned: Copy + Ord {
    const ZERO: Self;
    const ONE: Self;
    fn wrapping_sub(self, rhs: Self) -> Self;
    fn wrapping_add(self, rhs: Self) -> Self;
    fn bit_or(self, rhs: Self) -> Self;
    fn bit_and(self, rhs: Self) -> Self;
    fn bit_not(self) -> Self;
    fn trailing_zeros(self) -> u32;
    fn to_u64(self) -> u64;
    fn from_u64(value: u64) -> Self;
}

macro_rules! impl_linux_unsigned {
    ($($t:ty),+ $(,)?) => {$ (
        impl LinuxUnsigned for $t {
            const ZERO: Self = 0;
            const ONE: Self = 1;
            fn wrapping_sub(self, rhs: Self) -> Self { self.wrapping_sub(rhs) }
            fn wrapping_add(self, rhs: Self) -> Self { self.wrapping_add(rhs) }
            fn bit_or(self, rhs: Self) -> Self { self | rhs }
            fn bit_and(self, rhs: Self) -> Self { self & rhs }
            fn bit_not(self) -> Self { !self }
            fn trailing_zeros(self) -> u32 { self.trailing_zeros() }
            fn to_u64(self) -> u64 { self as u64 }
            fn from_u64(value: u64) -> Self { value as Self }
        }
    )+};
}

impl_linux_unsigned!(u8, u16, u32, u64, usize);

/// Linux `round_up(x, y)`: rounds to the next `y`-aligned value; `y` is
/// required to be a nonzero power of two, as in the source macro contract.
#[inline]
pub fn round_up<T: LinuxUnsigned>(value: T, alignment: T) -> T {
    assert!(alignment > T::ZERO && alignment.bit_and(alignment.wrapping_sub(T::ONE)) == T::ZERO);
    value
        .wrapping_add(alignment.wrapping_sub(T::ONE))
        .bit_and(alignment.wrapping_sub(T::ONE).bit_not())
}

#[inline]
pub fn page_align<T: LinuxUnsigned>(value: T) -> T {
    round_up(value, T::from_u64(4096))
}

/// `is_power_of_2()` from include/linux/log2.h.
#[inline]
pub fn is_power_of_2<T: LinuxUnsigned>(value: T) -> bool {
    value > T::ZERO && value.bit_and(value.wrapping_sub(T::ONE)) == T::ZERO
}

#[inline]
pub fn offset_in_page<T: LinuxUnsigned + PageMask>(value: T) -> T {
    value.bit_and(T::from_page_mask())
}

pub trait PageMask: LinuxUnsigned {
    fn from_page_mask() -> Self;
}
macro_rules! impl_page_mask {
    ($($t:ty),+ $(,)?) => {$ (
        impl PageMask for $t {
            fn from_page_mask() -> Self { !((4096 as $t) - 1) }
        }
    )+};
}
impl_page_mask!(u8, u16, u32, u64, usize);

/// Linux memory barriers used around device-visible state publication.
#[inline]
pub fn wmb() {
    fence(Ordering::Release);
}

#[inline]
pub fn rmb() {
    fence(Ordering::Acquire);
}

#[inline]
pub fn mb() {
    fence(Ordering::SeqCst);
}

#[inline]
pub fn is_power_of_2_u64(value: u64) -> bool {
    is_power_of_2(value)
}

pub const INT_MIN: i32 = i32::MIN;
pub const __GFP_NOWARN: u32 = 1 << 13;

/// Atomic `xchg(ptr, 0)` for Linux's pointer fetch-and-zero call sites.
pub trait AtomicZero: Copy {
    fn swap_zero(place: *mut Self) -> Self;
}

impl<T> AtomicZero for *mut T {
    fn swap_zero(place: *mut Self) -> Self {
        // SAFETY: caller uses naturally aligned, initialized pointer storage;
        // all concurrent access to these fields follows Linux xchg semantics.
        unsafe { core::sync::atomic::AtomicPtr::from_ptr(place) }
            .swap(core::ptr::null_mut(), Ordering::AcqRel)
    }
}

impl<T> AtomicZero for *const T {
    fn swap_zero(place: *mut Self) -> Self {
        // SAFETY: same pointer-sized storage contract as the mutable form.
        unsafe { core::sync::atomic::AtomicPtr::from_ptr(place.cast::<*mut T>()) }
            .swap(core::ptr::null_mut(), Ordering::AcqRel)
    }
}

#[inline]
pub fn fetch_and_zero<T: AtomicZero>(place: &mut T) -> T {
    T::swap_zero(place)
}

/// Linux jiffies derived from the kernel monotonic clock and configured HZ.
#[inline]
pub fn jiffies() -> u64 {
    let nanos = axhal::time::monotonic_time_nanos();
    let second = axhal::time::NANOS_PER_SEC as u64;
    let hz = crate::linux_config::CONFIG_HZ as u64;
    (nanos / second) * hz + (nanos % second) * hz / second
}

#[inline]
pub fn jiffies_to_msecs<T: LinuxUnsigned>(ticks: T) -> u64 {
    ticks.to_u64().saturating_mul(1000) / crate::linux_config::CONFIG_HZ as u64
}

#[inline]
pub fn jiffies_to_nsecs<T: LinuxUnsigned>(ticks: T) -> u64 {
    ticks
        .to_u64()
        .saturating_mul(axhal::time::NANOS_PER_SEC as u64)
        / crate::linux_config::CONFIG_HZ as u64
}

#[inline]
pub fn msecs_to_jiffies<T: LinuxUnsigned>(milliseconds: T) -> u64 {
    let hz = crate::linux_config::CONFIG_HZ as u64;
    milliseconds.to_u64().saturating_mul(hz).saturating_add(999) / 1000
}

#[inline]
pub fn overflows_u32<T: LinuxUnsigned>(value: T) -> bool {
    value.to_u64() > u32::MAX as u64
}

/// Linux 7.2.3 CTB bound: `(SZ_4K * HZ) / SZ_2K`.
#[inline]
pub const fn intel_guc_ct_max_queue_time_jiffies() -> i64 {
    (4096 * crate::linux_config::CONFIG_HZ as i64) / 2048
}

pub trait LinuxBitScan: Copy {
    fn scan_word(self) -> u64;
}
impl LinuxBitScan for u32 {
    fn scan_word(self) -> u64 {
        self as u64
    }
}
impl LinuxBitScan for u64 {
    fn scan_word(self) -> u64 {
        self
    }
}
impl LinuxBitScan for usize {
    fn scan_word(self) -> u64 {
        self as u64
    }
}
impl LinuxBitScan for i32 {
    fn scan_word(self) -> u64 {
        self as u32 as u64
    }
}

/// Linux `__ffs`: zero-based index of the first set bit (input must be nonzero).
#[inline]
pub fn __ffs<T: LinuxBitScan>(value: T) -> u32 {
    let value = value.scan_word();
    assert_ne!(value, 0, "Linux __ffs requires a nonzero word");
    value.trailing_zeros()
}

/// Linux `ffs`: one-based index, with zero mapping to zero.
#[inline]
pub fn ffs<T: LinuxBitScan>(value: T) -> i32 {
    let value = value.scan_word();
    if value == 0 {
        0
    } else {
        value.trailing_zeros() as i32 + 1
    }
}

#[inline]
pub fn ilog2<T: LinuxBitScan>(value: T) -> u32 {
    let value = value.scan_word();
    assert_ne!(value, 0, "Linux ilog2 requires a nonzero input");
    u64::BITS - 1 - value.leading_zeros()
}

#[inline]
pub fn max<T: Ord>(left: T, right: T) -> T {
    core::cmp::max(left, right)
}

#[inline]
pub fn ktime_get() -> i64 {
    axhal::time::monotonic_time_nanos() as i64
}

/// Linux `ktime_get_raw_fast_ns()` binding. The configured TheKernel time
/// source is a hardware-backed monotonic counter with no wall-clock/NTP
/// adjustment, matching the raw monotonic domain required by i915's LRC stats.
#[inline]
pub fn ktime_get_raw_fast_ns() -> u64 {
    axhal::time::monotonic_time_nanos()
}

/// `ARRAY_SIZE(array)` for fixed C-layout Rust arrays.
#[allow(non_snake_case)]
#[inline]
pub fn ARRAY_SIZE<T: Copy, const N: usize>(_array: [T; N]) -> usize {
    N
}

#[inline]
pub const fn str_yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

pub trait AsConstBytePointer {
    fn as_const_byte_pointer(self) -> *const u8;
}
pub trait AsMutBytePointer {
    fn as_mut_byte_pointer(self) -> *mut u8;
}
impl<T> AsConstBytePointer for *const T {
    fn as_const_byte_pointer(self) -> *const u8 {
        self.cast()
    }
}
impl<T> AsConstBytePointer for *mut T {
    fn as_const_byte_pointer(self) -> *const u8 {
        self.cast()
    }
}
impl<T> AsMutBytePointer for *mut T {
    fn as_mut_byte_pointer(self) -> *mut u8 {
        self.cast()
    }
}

/// Linux `memcpy` for the imported raw-pointer call sites.
pub unsafe fn memcpy<D: AsMutBytePointer, S: AsConstBytePointer>(
    destination: D,
    source: S,
    size: usize,
) -> *mut c_void {
    let destination = destination.as_mut_byte_pointer();
    let source = source.as_const_byte_pointer();
    if size != 0 {
        assert!(!destination.is_null() && !source.is_null());
        // SAFETY: Linux memcpy call sites provide valid source/destination
        // ranges of `size` bytes and require non-overlapping storage.
        unsafe { core::ptr::copy_nonoverlapping(source, destination, size) };
    }
    destination.cast()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_round_up_and_power_of_two_follow_linux_rules() {
        assert_eq!(round_up(4097u32, 4096), 8192);
        assert_eq!(round_up(0usize, 4096), 0);
        assert!(is_power_of_2(1u32));
        assert!(is_power_of_2(4096usize));
        assert!(!is_power_of_2(0u32));
        assert!(!is_power_of_2(3u64));
        assert_eq!(offset_in_page(0x1234usize), 0x234);
    }
}
