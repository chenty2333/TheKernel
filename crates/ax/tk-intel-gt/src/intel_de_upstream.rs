// SPDX-License-Identifier: MIT
// Copyright © 2026 Intel Corporation
// Source: drivers/gpu/drm/i915/display/intel_de.c and intel_de.h

#![allow(non_snake_case, dead_code, unsafe_code)]

use core::ffi::c_void;

use crate::intel_display_core_types_upstream::IntelDisplay;
use crate::intel_uncore_types_upstream::{
    intel_uncore_posting_read, intel_uncore_read, intel_uncore_read16, intel_uncore_read64_2x32,
    intel_uncore_read8, intel_uncore_read_fw, intel_uncore_read_notrace, intel_uncore_rmw,
    intel_uncore_write, intel_uncore_write8, intel_uncore_write_fw, intel_uncore_write_notrace,
    IntelUncore,
};
use crate::intel_uncore_upstream::to_intel_uncore;
use crate::intel_workarounds_types_upstream::I915RegT;

// upstream: intel_de.h __to_uncore()
#[inline]
pub unsafe fn __to_uncore(display: *mut IntelDisplay) -> *mut IntelUncore {
    if display.is_null() || (*display).drm.is_null() {
        core::ptr::null_mut()
    } else {
        to_intel_uncore((*display).drm)
    }
}

// upstream: intel_de.c intel_de_read8()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_de_read8(display: *mut IntelDisplay, reg: I915RegT) -> u8 {
    let uncore = __to_uncore(display);
    if uncore.is_null() {
        return 0;
    }
    intel_uncore_read8(uncore, reg)
}

// upstream: intel_de.c intel_de_write8()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_de_write8(display: *mut IntelDisplay, reg: I915RegT, val: u8) {
    let uncore = __to_uncore(display);
    if !uncore.is_null() {
        intel_uncore_write8(uncore, reg, val);
    }
}

// upstream: intel_de.c intel_de_read16()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_de_read16(display: *mut IntelDisplay, reg: I915RegT) -> u16 {
    let uncore = __to_uncore(display);
    if uncore.is_null() {
        return 0;
    }
    intel_uncore_read16(uncore, reg)
}

// upstream: intel_de.h intel_de_read()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_de_read(display: *mut IntelDisplay, reg: I915RegT) -> u32 {
    crate::display_pending_stubs::b3c::intel_dmc_wl_get(display, reg);
    let val = {
        let uncore = __to_uncore(display);
        if uncore.is_null() {
            0
        } else {
            intel_uncore_read(uncore, reg)
        }
    };
    crate::display_pending_stubs::b3c::intel_dmc_wl_put(display, reg);
    val
}

// upstream: intel_de.h intel_de_read64_2x32_volatile()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_de_read64_2x32_volatile(
    display: *mut IntelDisplay,
    lower_reg: I915RegT,
    upper_reg: I915RegT,
) -> u64 {
    crate::display_pending_stubs::b3c::intel_dmc_wl_get(display, lower_reg);
    crate::display_pending_stubs::b3c::intel_dmc_wl_get(display, upper_reg);
    let val = {
        let uncore = __to_uncore(display);
        if uncore.is_null() {
            0
        } else {
            intel_uncore_read64_2x32(uncore, lower_reg, upper_reg)
        }
    };
    crate::display_pending_stubs::b3c::intel_dmc_wl_put(display, upper_reg);
    crate::display_pending_stubs::b3c::intel_dmc_wl_put(display, lower_reg);
    val
}

// upstream: intel_de.h intel_de_read64_2x32()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_de_read64_2x32(display: *mut IntelDisplay, reg: I915RegT) -> u64 {
    let upper_reg = I915RegT { reg: reg.reg + 4 };
    let lower = intel_de_read(display, reg);
    let upper = intel_de_read(display, upper_reg);
    ((upper as u64) << 32) | (lower as u64)
}

// upstream: intel_de.h intel_de_posting_read()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_de_posting_read(display: *mut IntelDisplay, reg: I915RegT) {
    crate::display_pending_stubs::b3c::intel_dmc_wl_get(display, reg);
    let uncore = __to_uncore(display);
    if !uncore.is_null() {
        intel_uncore_posting_read(uncore, reg);
    }
    crate::display_pending_stubs::b3c::intel_dmc_wl_put(display, reg);
}

// upstream: intel_de.h intel_de_write()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_de_write(display: *mut IntelDisplay, reg: I915RegT, val: u32) {
    crate::display_pending_stubs::b3c::intel_dmc_wl_get(display, reg);
    let uncore = __to_uncore(display);
    if !uncore.is_null() {
        intel_uncore_write(uncore, reg, val);
    }
    crate::display_pending_stubs::b3c::intel_dmc_wl_put(display, reg);
}

// upstream: intel_de.h intel_de_rmw()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_de_rmw(
    display: *mut IntelDisplay,
    reg: I915RegT,
    clear: u32,
    set: u32,
) -> u32 {
    crate::display_pending_stubs::b3c::intel_dmc_wl_get(display, reg);
    let val = {
        let uncore = __to_uncore(display);
        if uncore.is_null() {
            0
        } else {
            intel_uncore_rmw(uncore, reg, clear, set)
        }
    };
    crate::display_pending_stubs::b3c::intel_dmc_wl_put(display, reg);
    val
}

// upstream: intel_de.h intel_de_read_fw()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_de_read_fw(display: *mut IntelDisplay, reg: I915RegT) -> u32 {
    let uncore = __to_uncore(display);
    if uncore.is_null() {
        0
    } else {
        intel_uncore_read_fw(uncore, reg)
    }
}

// upstream: intel_de.h intel_de_write_fw()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_de_write_fw(display: *mut IntelDisplay, reg: I915RegT, val: u32) {
    let uncore = __to_uncore(display);
    if !uncore.is_null() {
        intel_uncore_write_fw(uncore, reg, val);
    }
}

// upstream: intel_de.h intel_de_rmw_fw()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_de_rmw_fw(
    display: *mut IntelDisplay,
    reg: I915RegT,
    clear: u32,
    set: u32,
) -> u32 {
    let old = intel_de_read_fw(display, reg);
    let val = (old & !clear) | set;
    intel_de_write_fw(display, reg, val);
    old
}

// upstream: intel_de.h intel_de_read_notrace()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_de_read_notrace(display: *mut IntelDisplay, reg: I915RegT) -> u32 {
    let uncore = __to_uncore(display);
    if uncore.is_null() {
        0
    } else {
        intel_uncore_read_notrace(uncore, reg)
    }
}

// upstream: intel_de.h intel_de_write_notrace(display, reg, val)
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_de_write_notrace(display: *mut IntelDisplay, reg: I915RegT, val: u32) {
    let uncore = __to_uncore(display);
    if !uncore.is_null() {
        intel_uncore_write_notrace(uncore, reg, val);
    }
}

// upstream: intel_de.h intel_de_write_dsb()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_de_write_dsb(
    display: *mut IntelDisplay,
    dsb: *mut c_void,
    reg: I915RegT,
    val: u32,
) {
    if !dsb.is_null() {
        crate::display_pending_stubs::b4::intel_dsb_reg_write(dsb, reg, val);
    } else {
        intel_de_write_fw(display, reg, val);
    }
}

// upstream: intel_de.c __intel_de_wait_for_register()
unsafe fn __intel_de_wait_for_register(
    display: *mut IntelDisplay,
    reg: I915RegT,
    mask: u32,
    value: u32,
    timeout_us: u32,
    read: unsafe extern "C" fn(*mut IntelDisplay, I915RegT) -> u32,
    out_val: *mut u32,
    mut is_atomic: bool,
) -> i32 {
    let start = tk_linux_kapi::clock::now_ns();
    let timeout_ns = (timeout_us as u64).saturating_mul(1000);
    let end = start.saturating_add(timeout_ns);

    let wait_max = 1000;
    let mut wait = 10;
    let mut reg_value = 0u32;
    let ret;

    if timeout_us <= 10 {
        is_atomic = true;
        wait = 1;
    }

    #[cfg(test)]
    let mut test_loop_count = 0;

    loop {
        #[cfg(test)]
        {
            test_loop_count += 1;
            if test_loop_count > 5 {
                ret = -crate::linux_config::ETIMEDOUT;
                break;
            }
        }

        let expired = tk_linux_kapi::clock::now_ns() >= end;
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);

        reg_value = read(display, reg);
        if (reg_value & mask) == value {
            ret = 0;
            break;
        }

        if expired {
            ret = -crate::linux_config::ETIMEDOUT;
            break;
        }

        #[cfg(not(test))]
        if is_atomic {
            tk_linux_kapi::primitives::udelay(wait as u32);
        } else {
            tk_linux_kapi::iopoll::usleep_range(wait as u64, (wait << 1) as u64);
        }

        if wait < wait_max {
            wait <<= 1;
        }
    }

    if !out_val.is_null() {
        *out_val = reg_value;
    }

    ret
}

// upstream: intel_de.c intel_de_wait_for_register()
unsafe fn intel_de_wait_for_register(
    display: *mut IntelDisplay,
    reg: I915RegT,
    mask: u32,
    value: u32,
    fast_timeout_us: u32,
    slow_timeout_us: u32,
    read: unsafe extern "C" fn(*mut IntelDisplay, I915RegT) -> u32,
    out_value: *mut u32,
    is_atomic: bool,
) -> i32 {
    let mut ret = -crate::linux_config::EINVAL;

    if fast_timeout_us != 0 {
        ret = __intel_de_wait_for_register(
            display,
            reg,
            mask,
            value,
            fast_timeout_us,
            read,
            out_value,
            is_atomic,
        );
    }

    if ret != 0 && slow_timeout_us != 0 {
        ret = __intel_de_wait_for_register(
            display,
            reg,
            mask,
            value,
            slow_timeout_us,
            read,
            out_value,
            is_atomic,
        );
    }

    ret
}

// upstream: intel_de.c intel_de_wait_us()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_de_wait_us(
    display: *mut IntelDisplay,
    reg: I915RegT,
    mask: u32,
    value: u32,
    timeout_us: u32,
    out_value: *mut u32,
) -> i32 {
    crate::display_pending_stubs::b3c::intel_dmc_wl_get(display, reg);
    let ret = intel_de_wait_for_register(
        display,
        reg,
        mask,
        value,
        timeout_us,
        0,
        intel_de_read,
        out_value,
        false,
    );
    crate::display_pending_stubs::b3c::intel_dmc_wl_put(display, reg);
    ret
}

// upstream: intel_de.c intel_de_wait_ms()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_de_wait_ms(
    display: *mut IntelDisplay,
    reg: I915RegT,
    mask: u32,
    value: u32,
    timeout_ms: u32,
    out_value: *mut u32,
) -> i32 {
    crate::display_pending_stubs::b3c::intel_dmc_wl_get(display, reg);
    let ret = intel_de_wait_for_register(
        display,
        reg,
        mask,
        value,
        2,
        timeout_ms.saturating_mul(1000),
        intel_de_read,
        out_value,
        false,
    );
    crate::display_pending_stubs::b3c::intel_dmc_wl_put(display, reg);
    ret
}

// upstream: intel_de.c intel_de_wait_fw_ms()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_de_wait_fw_ms(
    display: *mut IntelDisplay,
    reg: I915RegT,
    mask: u32,
    value: u32,
    timeout_ms: u32,
    out_value: *mut u32,
) -> i32 {
    intel_de_wait_for_register(
        display,
        reg,
        mask,
        value,
        2,
        timeout_ms.saturating_mul(1000),
        intel_de_read_fw,
        out_value,
        false,
    )
}

// upstream: intel_de.c intel_de_wait_fw_us_atomic()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_de_wait_fw_us_atomic(
    display: *mut IntelDisplay,
    reg: I915RegT,
    mask: u32,
    value: u32,
    timeout_us: u32,
    out_value: *mut u32,
) -> i32 {
    intel_de_wait_for_register(
        display,
        reg,
        mask,
        value,
        timeout_us,
        0,
        intel_de_read_fw,
        out_value,
        true,
    )
}

// upstream: intel_de.c intel_de_wait_for_set_us()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_de_wait_for_set_us(
    display: *mut IntelDisplay,
    reg: I915RegT,
    mask: u32,
    timeout_us: u32,
) -> i32 {
    intel_de_wait_us(display, reg, mask, mask, timeout_us, core::ptr::null_mut())
}

// upstream: intel_de.c intel_de_wait_for_clear_us()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_de_wait_for_clear_us(
    display: *mut IntelDisplay,
    reg: I915RegT,
    mask: u32,
    timeout_us: u32,
) -> i32 {
    intel_de_wait_us(display, reg, mask, 0, timeout_us, core::ptr::null_mut())
}

// upstream: intel_de.c intel_de_wait_for_set_ms()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_de_wait_for_set_ms(
    display: *mut IntelDisplay,
    reg: I915RegT,
    mask: u32,
    timeout_ms: u32,
) -> i32 {
    intel_de_wait_ms(display, reg, mask, mask, timeout_ms, core::ptr::null_mut())
}

// upstream: intel_de.c intel_de_wait_for_clear_ms()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_de_wait_for_clear_ms(
    display: *mut IntelDisplay,
    reg: I915RegT,
    mask: u32,
    timeout_ms: u32,
) -> i32 {
    intel_de_wait_ms(display, reg, mask, 0, timeout_ms, core::ptr::null_mut())
}
