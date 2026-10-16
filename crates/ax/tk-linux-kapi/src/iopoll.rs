// SPDX-License-Identifier: GPL-2.0-only
// Copyright 2026 TheKernel contributors.
//! Translation of Linux 7.2.3 `include/linux/iopoll.h` (GPL-2.0 upstream).
//!
//! The Linux macros are kept as exported macros with the same names and
//! argument order. `op` and `cond` are expressions evaluated in the caller's
//! scope, so `read_poll_timeout!` and friends take the destination variable
//! as an identifier, as the C macros do through their `val` argument.
//!
//! Sleep semantics: `usleep_range(min, max)` is approximated by a task sleep
//! of `min` microseconds (the HAL timer is finer than Linux's slack window),
//! falling back to a calibrated busy wait when the caller cannot block. The
//! `_relaxed` accessors are not provided: on this target every MMIO access
//! through these macros is already a volatile access.

use core::ffi::c_void;

/// Linux `usleep_range()` for the polling macros.
pub fn usleep_range(min_us: u64, _max_us: u64) {
    if min_us != 0 {
        crate::clock::sleep_us(min_us);
    }
}

/// `readb()`: volatile 8-bit MMIO read.
///
/// # Safety
///
/// `addr` must point to a readable, mapped 8-bit register.
#[allow(unsafe_code)]
pub unsafe fn readb(addr: *const c_void) -> u8 {
    unsafe { core::ptr::read_volatile(addr.cast::<u8>()) }
}

/// `readw()`: volatile 16-bit MMIO read.
///
/// # Safety
///
/// `addr` must point to a readable, 2-byte aligned, mapped register.
#[allow(unsafe_code)]
pub unsafe fn readw(addr: *const c_void) -> u16 {
    unsafe { core::ptr::read_volatile(addr.cast::<u16>()) }
}

/// `readl()`: volatile 32-bit MMIO read.
///
/// # Safety
///
/// `addr` must point to a readable, 4-byte aligned, mapped register.
#[allow(unsafe_code)]
pub unsafe fn readl(addr: *const c_void) -> u32 {
    unsafe { core::ptr::read_volatile(addr.cast::<u32>()) }
}

/// `readq()`: volatile 64-bit MMIO read.
///
/// # Safety
///
/// `addr` must point to a readable, 8-byte aligned, mapped register.
#[allow(unsafe_code)]
pub unsafe fn readq(addr: *const c_void) -> u64 {
    unsafe { core::ptr::read_volatile(addr.cast::<u64>()) }
}

/// Linux `poll_timeout_us()`: evaluate `$op` then `$cond` until `$cond` holds
/// or the timeout expires. `$timeout_us == 0` never times out. Returns 0 on
/// success and `-ETIMEDOUT` on timeout; on timeout `$op` runs once more after
/// the deadline has passed, as in the C macro.
#[macro_export]
macro_rules! poll_timeout_us {
    ($op:expr, $cond:expr, $sleep_us:expr, $timeout_us:expr, $sleep_before_op:expr $(,)?) => {{
        let __timeout_us: u64 = ($timeout_us) as u64;
        let __sleep_us: u64 = ($sleep_us) as u64;
        let __start = ($crate::clock::now_ns() as i64);
        let __timeout_ns = (__timeout_us as i64).saturating_mul(1_000);
        let __ret: i32;
        if ($sleep_before_op) && __sleep_us != 0 {
            $crate::iopoll::usleep_range((__sleep_us >> 2) + 1, __sleep_us);
        }
        loop {
            let __expired = __timeout_us != 0
                && ($crate::clock::now_ns() as i64).saturating_sub(__start) > __timeout_ns;
            $op;
            if $cond {
                __ret = 0;
                break;
            }
            if __expired {
                __ret = -$crate::config::ETIMEDOUT;
                break;
            }
            if __sleep_us != 0 {
                $crate::iopoll::usleep_range((__sleep_us >> 2) + 1, __sleep_us);
            }
            core::hint::spin_loop();
        }
        __ret
    }};
}

/// Linux `poll_timeout_us_atomic()`: the same loop with `udelay()` between
/// operations, so it does not rely on the clock and may run in atomic context.
#[macro_export]
macro_rules! poll_timeout_us_atomic {
    ($op:expr, $cond:expr, $delay_us:expr, $timeout_us:expr, $delay_before_op:expr $(,)?) => {{
        let __timeout_us: u64 = ($timeout_us) as u64;
        let __delay_us: u64 = ($delay_us) as u64;
        let mut __left_ns: i64 = (__timeout_us as i64).saturating_mul(1_000);
        let __delay_ns: i64 = (__delay_us as i64).saturating_mul(1_000);
        let __ret: i32;
        if ($delay_before_op) && __delay_us != 0 {
            $crate::clock::delay_us(__delay_us as u32);
            if __timeout_us != 0 {
                __left_ns -= __delay_ns;
            }
        }
        loop {
            let __expired = __timeout_us != 0 && __left_ns < 0;
            $op;
            if $cond {
                __ret = 0;
                break;
            }
            if __expired {
                __ret = -$crate::config::ETIMEDOUT;
                break;
            }
            if __delay_us != 0 {
                $crate::clock::delay_us(__delay_us as u32);
                if __timeout_us != 0 {
                    __left_ns -= __delay_ns;
                }
            }
            core::hint::spin_loop();
            if __timeout_us != 0 {
                __left_ns -= 1;
            }
        }
        __ret
    }};
}

/// Linux `read_poll_timeout()`: poll `$op($args...)` into `$val`.
#[macro_export]
macro_rules! read_poll_timeout {
    ($op:path, $val:ident, $cond:expr, $sleep_us:expr, $timeout_us:expr, $sleep_before_read:expr $(, $arg:expr)* $(,)?) => {
        $crate::poll_timeout_us!(
            { $val = $op($($arg),*) },
            $cond,
            $sleep_us,
            $timeout_us,
            $sleep_before_read
        )
    };
}

/// Linux `read_poll_timeout_atomic()`.
#[macro_export]
macro_rules! read_poll_timeout_atomic {
    ($op:path, $val:ident, $cond:expr, $delay_us:expr, $timeout_us:expr, $delay_before_read:expr $(, $arg:expr)* $(,)?) => {
        $crate::poll_timeout_us_atomic!(
            { $val = $op($($arg),*) },
            $cond,
            $delay_us,
            $timeout_us,
            $delay_before_read
        )
    };
}

/// Linux `readx_poll_timeout(op, addr, val, cond, sleep_us, timeout_us)`.
#[macro_export]
macro_rules! readx_poll_timeout {
    ($op:path, $addr:expr, $val:ident, $cond:expr, $sleep_us:expr, $timeout_us:expr $(,)?) => {
        // The accessor takes a raw MMIO address; the caller owns that contract.
        unsafe { $crate::read_poll_timeout!($op, $val, $cond, $sleep_us, $timeout_us, false, $addr) }
    };
}

/// Linux `readx_poll_timeout_atomic(op, addr, val, cond, delay_us, timeout_us)`.
#[macro_export]
macro_rules! readx_poll_timeout_atomic {
    ($op:path, $addr:expr, $val:ident, $cond:expr, $delay_us:expr, $timeout_us:expr $(,)?) => {
        // The accessor takes a raw MMIO address; the caller owns that contract.
        unsafe { $crate::read_poll_timeout_atomic!($op, $val, $cond, $delay_us, $timeout_us, false, $addr) }
    };
}

/// Linux `readb_poll_timeout(addr, val, cond, delay_us, timeout_us)`.
#[macro_export]
macro_rules! readb_poll_timeout {
    ($addr:expr, $val:ident, $cond:expr, $sleep_us:expr, $timeout_us:expr $(,)?) => {
        $crate::readx_poll_timeout!($crate::iopoll::readb, $addr, $val, $cond, $sleep_us, $timeout_us)
    };
}

/// Linux `readw_poll_timeout(addr, val, cond, delay_us, timeout_us)`.
#[macro_export]
macro_rules! readw_poll_timeout {
    ($addr:expr, $val:ident, $cond:expr, $sleep_us:expr, $timeout_us:expr $(,)?) => {
        $crate::readx_poll_timeout!($crate::iopoll::readw, $addr, $val, $cond, $sleep_us, $timeout_us)
    };
}

/// Linux `readl_poll_timeout(addr, val, cond, delay_us, timeout_us)`.
#[macro_export]
macro_rules! readl_poll_timeout {
    ($addr:expr, $val:ident, $cond:expr, $sleep_us:expr, $timeout_us:expr $(,)?) => {
        $crate::readx_poll_timeout!($crate::iopoll::readl, $addr, $val, $cond, $sleep_us, $timeout_us)
    };
}

/// Linux `readq_poll_timeout(addr, val, cond, delay_us, timeout_us)`.
#[macro_export]
macro_rules! readq_poll_timeout {
    ($addr:expr, $val:ident, $cond:expr, $sleep_us:expr, $timeout_us:expr $(,)?) => {
        $crate::readx_poll_timeout!($crate::iopoll::readq, $addr, $val, $cond, $sleep_us, $timeout_us)
    };
}

/// Linux `readl_poll_timeout_atomic(addr, val, cond, delay_us, timeout_us)`.
#[macro_export]
macro_rules! readl_poll_timeout_atomic {
    ($addr:expr, $val:ident, $cond:expr, $delay_us:expr, $timeout_us:expr $(,)?) => {
        $crate::readx_poll_timeout_atomic!($crate::iopoll::readl, $addr, $val, $cond, $delay_us, $timeout_us)
    };
}

#[cfg(test)]
mod tests {
    use core::ffi::c_void;

    use crate::config::ETIMEDOUT;

    #[test]
    fn returns_zero_on_first_satisfied_condition_with_one_operation() {
        let mut calls = 0u32;
        let mut value = 0u32;
        let ret = crate::poll_timeout_us!(
            {
                calls += 1;
                value = 7;
            },
            value == 7,
            0,
            1_000,
            false
        );
        assert_eq!(ret, 0);
        assert_eq!(calls, 1);
    }

    #[test]
    fn times_out_with_etimedout_after_at_least_one_post_deadline_operation() {
        let mut calls = 0u32;
        let ret = crate::poll_timeout_us!(
            {
                calls += 1;
            },
            false,
            0,
            2_000,
            false
        );
        assert_eq!(ret, -ETIMEDOUT);
        assert!(calls >= 2);
    }

    #[test]
    fn zero_timeout_never_expires_until_condition_holds() {
        let mut counter = 0u32;
        let ret = crate::poll_timeout_us!(
            {
                counter += 1;
            },
            counter >= 5,
            0,
            0,
            false
        );
        assert_eq!(ret, 0);
        assert_eq!(counter, 5);
    }

    #[test]
    fn read_poll_timeout_stores_the_last_value_read() {
        let mut next = 0u32;
        fn bump(counter: &mut u32) -> u32 {
            *counter += 1;
            *counter
        }
        let mut val = 0u32;
        let ret = crate::read_poll_timeout!(bump, val, val >= 3, 0, 1_000, false, &mut next);
        assert_eq!(ret, 0);
        assert_eq!(val, 3);
        assert_eq!(next, 3);
    }

    #[test]
    fn atomic_variant_polls_with_udelay_and_times_out() {
        let mut calls = 0u32;
        let mut val = 0u32;
        let ret = crate::poll_timeout_us_atomic!(
            {
                calls += 1;
                val = 0;
            },
            val == 1,
            1,
            50,
            false
        );
        assert_eq!(ret, -ETIMEDOUT);
        assert!(calls >= 2);
    }

    #[test]
    fn readl_poll_timeout_reads_the_register_until_the_mask_matches() {
        let mut register: u32 = 0x0000_0010;
        let addr = core::ptr::addr_of!(register).cast::<c_void>();
        let mut val = 0u32;
        let ret = crate::readl_poll_timeout!(addr, val, val & 0x10 != 0, 0, 1_000);
        assert_eq!(ret, 0);
        assert_eq!(val, 0x10);
        register = 0;
        let ret = crate::readl_poll_timeout_atomic!(
            core::ptr::addr_of!(register).cast::<c_void>(),
            val,
            val & 0x10 != 0,
            1,
            20
        );
        assert_eq!(ret, -ETIMEDOUT);
        assert_eq!(val, 0);
    }
}
