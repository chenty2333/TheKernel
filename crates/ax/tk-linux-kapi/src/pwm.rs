// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! PWM subsystem fallbacks for disabled subsystems.
//! Upstream reference: `include/linux/pwm.h`.

#![allow(unsafe_code)]

use core::ffi::{c_char, c_int, c_uint, c_void};

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PwmState {
    pub period: u64,
    pub duty_cycle: u64,
    pub polarity: c_uint,
    pub enabled: bool,
    pub usage_power: bool,
}

#[inline(always)]
fn err_ptr_nodev() -> *mut c_void {
    (-(crate::config::ENODEV as isize)) as *mut c_void
}

// upstream: include/linux/pwm.h !CONFIG_PWM pwm_get()
// !CONFIG_PWM fallback returns ERR_PTR(-ENODEV).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pwm_get(_dev: *mut c_void, _consumer: *const c_char) -> *mut c_void {
    err_ptr_nodev()
}

// upstream: include/linux/pwm.h !CONFIG_PWM devm_pwm_get()
// !CONFIG_PWM fallback returns ERR_PTR(-ENODEV).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn devm_pwm_get(_dev: *mut c_void, _consumer: *const c_char) -> *mut c_void {
    err_ptr_nodev()
}

// upstream: include/linux/pwm.h !CONFIG_PWM pwm_put()
// !CONFIG_PWM fallback is a no-op.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pwm_put(_pwm: *mut c_void) {}

// upstream: include/linux/pwm.h !CONFIG_PWM pwm_apply_might_sleep()
// !CONFIG_PWM fallback returns -ENODEV when PWM hardware is unavailable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pwm_apply_might_sleep(
    _pwm: *mut c_void,
    _state: *const PwmState,
) -> c_int {
    -crate::config::ENODEV
}

// upstream: include/linux/pwm.h pwm_get_state()
// Returns the current state (zeros when no PWM device exists).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pwm_get_state(_pwm: *const c_void, state: *mut PwmState) {
    if !state.is_null() {
        unsafe { *state = PwmState::default() };
    }
}

// upstream: include/linux/pwm.h pwm_is_enabled()
// Returns false when PWM is not enabled / absent.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pwm_is_enabled(_pwm: *const c_void) -> bool {
    false
}

// upstream: include/linux/pwm.h pwm_get_relative_duty_cycle()
// Calculates relative duty cycle scaled to `scale`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pwm_get_relative_duty_cycle(
    state: *const PwmState,
    scale: c_uint,
) -> c_uint {
    if state.is_null() || scale == 0 {
        return 0;
    }
    let s = unsafe { &*state };
    if s.period == 0 {
        return 0;
    }
    let num = (s.duty_cycle as u128) * (scale as u128);
    let den = s.period as u128;
    ((num + den / 2) / den) as c_uint
}

// upstream: include/linux/pwm.h pwm_set_relative_duty_cycle()
// Sets absolute duty_cycle from relative `duty_cycle` and `scale`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pwm_set_relative_duty_cycle(
    state: *mut PwmState,
    duty_cycle: c_uint,
    scale: c_uint,
) -> c_int {
    if state.is_null() || scale == 0 || duty_cycle > scale {
        return -crate::config::EINVAL;
    }
    let s = unsafe { &mut *state };
    let num = (duty_cycle as u128) * (s.period as u128);
    let den = scale as u128;
    s.duty_cycle = ((num + den / 2) / den) as u64;
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pwm_fallbacks() {
        let err_expected = (-(crate::config::ENODEV as isize)) as *mut c_void;
        assert_eq!(unsafe { pwm_get(core::ptr::null_mut(), core::ptr::null()) }, err_expected);
        assert_eq!(
            unsafe { devm_pwm_get(core::ptr::null_mut(), core::ptr::null()) },
            err_expected
        );
        unsafe { pwm_put(core::ptr::null_mut()) };

        let mut st = PwmState {
            period: 1000,
            duty_cycle: 500,
            polarity: 0,
            enabled: true,
            usage_power: false,
        };
        assert_eq!(
            unsafe { pwm_apply_might_sleep(core::ptr::null_mut(), &st) },
            -crate::config::ENODEV
        );

        let mut out_state = PwmState {
            period: 999,
            duty_cycle: 999,
            polarity: 1,
            enabled: true,
            usage_power: true,
        };
        unsafe { pwm_get_state(core::ptr::null(), &mut out_state) };
        assert_eq!(out_state, PwmState::default());
        assert!(!unsafe { pwm_is_enabled(core::ptr::null()) });

        // Test relative duty cycle
        assert_eq!(unsafe { pwm_get_relative_duty_cycle(&st, 100) }, 50);

        st.period = 2000;
        let ret = unsafe { pwm_set_relative_duty_cycle(&mut st, 25, 100) };
        assert_eq!(ret, 0);
        assert_eq!(st.duty_cycle, 500);

        // Invalid scale
        assert_eq!(unsafe { pwm_set_relative_duty_cycle(&mut st, 25, 0) }, -crate::config::EINVAL);
        assert_eq!(
            unsafe { pwm_set_relative_duty_cycle(&mut st, 101, 100) },
            -crate::config::EINVAL
        );
    }
}
