// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// Source: drivers/gpu/drm/i915/display/intel_sbi.c and intel_sbi_regs.h (Linux v7.2.3).
//! Sideband interface (SBI) access to the PCH ICLK and mPHY registers.
#![allow(non_snake_case, dead_code, unsafe_code)]

use tk_linux_kapi::{
    config::{EBUSY, ENXIO, ETIMEDOUT},
    mutex::{mutex_lock, mutex_unlock},
};

use crate::{
    intel_de_upstream::{intel_de_read_fw, intel_de_wait_fw_ms, intel_de_write_fw},
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_reg_defs_upstream::mmio,
    intel_workarounds_types_upstream::I915RegT,
};

/// `enum intel_sbi_destination`.
pub const SBI_ICLK: u32 = 0;
pub const SBI_MPHY: u32 = 1;

// intel_sbi_regs.h
const SBI_ADDR: I915RegT = mmio(0xC6000);
const SBI_DATA: I915RegT = mmio(0xC6004);
const SBI_CTL_STAT: I915RegT = mmio(0xC6008);
const SBI_CTL_DEST_ICLK: u32 = 0 << 16;
const SBI_CTL_DEST_MPHY: u32 = 1 << 16;
const SBI_CTL_OP_IORD: u32 = 2 << 8;
const SBI_CTL_OP_IOWR: u32 = 3 << 8;
const SBI_CTL_OP_CRRD: u32 = 6 << 8;
const SBI_CTL_OP_WR: u32 = 1 << 8;
const SBI_RESPONSE_FAIL: u32 = 1 << 1;
const SBI_STATUS_MASK: u32 = 1 << 0;
const SBI_STATUS_BUSY: u32 = 1 << 0;
const SBI_STATUS_READY: u32 = 0 << 0;

/// `SBI_ADDR_VALUE(addr)`: the register address in bits 31:16.
#[inline]
const fn SBI_ADDR_VALUE(addr: u32) -> u32 {
    addr << 16
}

/// `intel_sbi_rw()`: one SBI transaction; returns a negative errno on failure.
// upstream: intel_sbi.c intel_sbi_rw()
unsafe fn intel_sbi_rw(
    display: *mut IntelDisplay,
    reg: u16,
    destination: u32,
    val: *mut u32,
    is_read: bool,
) -> i32 {
    unsafe {
        let mut cmd: u32;

        if intel_de_wait_fw_ms(
            display,
            SBI_CTL_STAT,
            SBI_STATUS_MASK,
            SBI_STATUS_READY,
            100,
            core::ptr::null_mut(),
        ) != 0
        {
            axlog::error!("i915 display: timeout waiting for SBI to become ready");
            return -EBUSY;
        }

        intel_de_write_fw(display, SBI_ADDR, SBI_ADDR_VALUE(reg as u32));
        intel_de_write_fw(display, SBI_DATA, if is_read { 0 } else { *val });

        if destination == SBI_ICLK {
            cmd = SBI_CTL_DEST_ICLK | SBI_CTL_OP_CRRD;
        } else {
            cmd = SBI_CTL_DEST_MPHY | SBI_CTL_OP_IORD;
        }
        if !is_read {
            cmd |= SBI_CTL_OP_WR;
        }
        intel_de_write_fw(display, SBI_CTL_STAT, cmd | SBI_STATUS_BUSY);

        if intel_de_wait_fw_ms(
            display,
            SBI_CTL_STAT,
            SBI_STATUS_MASK,
            SBI_STATUS_READY,
            100,
            &mut cmd,
        ) != 0
        {
            axlog::error!("i915 display: timeout waiting for SBI to complete read");
            return -ETIMEDOUT;
        }

        if cmd & SBI_RESPONSE_FAIL != 0 {
            axlog::error!("i915 display: error during SBI read of reg {:x}", reg);
            return -ENXIO;
        }

        if is_read {
            *val = intel_de_read_fw(display, SBI_DATA);
        }
        0
    }
}

/// `intel_sbi_read()`.
// upstream: intel_sbi.c intel_sbi_read()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_sbi_read(
    display: *mut IntelDisplay,
    reg: u16,
    destination: u32,
) -> u32 {
    let mut result: u32 = 0;
    unsafe { intel_sbi_rw(display, reg, destination, &mut result, true) };
    result
}

/// `intel_sbi_write()`.
// upstream: intel_sbi.c intel_sbi_write()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_sbi_write(
    display: *mut IntelDisplay,
    reg: u16,
    value: u32,
    destination: u32,
) {
    let mut value = value;
    unsafe { intel_sbi_rw(display, reg, destination, &mut value, false) };
}

/// `intel_sbi_lock()`.
// upstream: intel_sbi.c intel_sbi_lock()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_sbi_lock(display: *mut IntelDisplay) {
    unsafe { mutex_lock(core::ptr::addr_of_mut!((*display).sbi.lock)) };
}

/// `intel_sbi_unlock()`.
// upstream: intel_sbi.c intel_sbi_unlock()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_sbi_unlock(display: *mut IntelDisplay) {
    unsafe { mutex_unlock(core::ptr::addr_of_mut!((*display).sbi.lock)) };
}
