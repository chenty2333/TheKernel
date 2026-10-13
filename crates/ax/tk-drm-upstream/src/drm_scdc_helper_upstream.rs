// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/display/drm_scdc_helper.c` (batch B2e).
//!
//! Implements Status and Control Data Channel (SCDC) operations over I2C:
//! reading/writing SCDC registers, enabling/disabling scrambling, setting
//! the TMDS 1/40 vs 1/10 bit clock ratio, and reading scrambling status.

#![allow(non_snake_case, dead_code, unsafe_code)]
#![allow(clippy::missing_safety_doc)]

use core::ffi::c_void;

use tk_linux_kapi::config::{ENOMEM, EOPNOTSUPP, EPROTO, GFP_KERNEL};
use tk_linux_kapi::i2c::{I2cAdapter, I2cMsg, I2C_M_RD};
use tk_linux_kapi::memory::{kfree, kmalloc};

use crate::drm_connector_types::DrmConnector;

#[inline]
fn usleep_range(min_us: u64, max_us: u64) {
    #[cfg(not(test))]
    tk_linux_kapi::iopoll::usleep_range(min_us, max_us);
    #[cfg(test)]
    let _ = (min_us, max_us);
}

// SCDC slave address and register definitions from Linux drm_scdc.h / drm_scdc_helper.h
pub const SCDC_I2C_SLAVE_ADDRESS: u16 = 0x54;

pub const SCDC_SINK_VERSION: u8 = 0x01;
pub const SCDC_SOURCE_VERSION: u8 = 0x02;

pub const SCDC_UPDATE_0: u8 = 0x10;
pub const SCDC_READ_REQUEST_TEST: u8 = 1 << 2;
pub const SCDC_CED_UPDATE: u8 = 1 << 1;
pub const SCDC_STATUS_UPDATE: u8 = 1 << 0;

pub const SCDC_UPDATE_1: u8 = 0x11;

pub const SCDC_TMDS_CONFIG: u8 = 0x20;
pub const SCDC_TMDS_BIT_CLOCK_RATIO_BY_40: u8 = 1 << 1;
pub const SCDC_TMDS_BIT_CLOCK_RATIO_BY_10: u8 = 0 << 1;
pub const SCDC_SCRAMBLING_ENABLE: u8 = 1 << 0;

pub const SCDC_SCRAMBLER_STATUS: u8 = 0x21;
pub const SCDC_SCRAMBLING_STATUS: u8 = 1 << 0;

pub const SCDC_CONFIG_0: u8 = 0x30;
pub const SCDC_READ_REQUEST_ENABLE: u8 = 1 << 0;

pub const SCDC_STATUS_FLAGS_0: u8 = 0x40;
pub const SCDC_CH2_LOCK: u8 = 1 << 3;
pub const SCDC_CH1_LOCK: u8 = 1 << 2;
pub const SCDC_CH0_LOCK: u8 = 1 << 1;
pub const SCDC_CH_LOCK_MASK: u8 = SCDC_CH2_LOCK | SCDC_CH1_LOCK | SCDC_CH0_LOCK;
pub const SCDC_CLOCK_DETECT: u8 = 1 << 0;

pub const SCDC_STATUS_FLAGS_1: u8 = 0x41;

macro_rules! drm_dbg_kms {
    ($device:expr, $format:expr $(, $argument:expr)* $(,)?) => {
        tk_linux_kapi::drm_dbg!($device, $format $(, $argument)*)
    };
}

/// Helper calling I2cAdapter transfer.
unsafe fn i2c_transfer(adapter: *mut c_void, msgs: &mut [I2cMsg<'_>]) -> i32 {
    if adapter.is_null() {
        return -EOPNOTSUPP;
    }
    let adap = unsafe { &*(adapter as *const I2cAdapter<'static>) };
    adap.transfer(msgs)
}

// upstream: drivers/gpu/drm/display/drm_scdc_helper.c drm_scdc_read()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_scdc_read(
    adapter: *mut c_void,
    offset: u8,
    buffer: *mut c_void,
    size: usize,
) -> isize {
    if adapter.is_null() || buffer.is_null() {
        return -tk_linux_kapi::config::EINVAL as isize;
    }

    let mut off_buf = [offset];
    let rd_buf = unsafe { core::slice::from_raw_parts_mut(buffer as *mut u8, size) };
    let mut msgs = [
        I2cMsg::new(SCDC_I2C_SLAVE_ADDRESS, 0, &mut off_buf),
        I2cMsg::new(SCDC_I2C_SLAVE_ADDRESS, I2C_M_RD, rd_buf),
    ];

    let ret = unsafe { i2c_transfer(adapter, &mut msgs) };
    if ret < 0 {
        return ret as isize;
    }
    if ret != 2 {
        return -EPROTO as isize;
    }

    0
}

// upstream: drivers/gpu/drm/display/drm_scdc_helper.c drm_scdc_write()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_scdc_write(
    adapter: *mut c_void,
    offset: u8,
    buffer: *const c_void,
    size: usize,
) -> isize {
    if adapter.is_null() || (size > 0 && buffer.is_null()) {
        return -tk_linux_kapi::config::EINVAL as isize;
    }

    let total_len = 1 + size;
    let data = unsafe { kmalloc(total_len, GFP_KERNEL) };
    if data.is_null() {
        return -ENOMEM as isize;
    }

    unsafe {
        *(data as *mut u8) = offset;
        if size > 0 {
            core::ptr::copy_nonoverlapping(buffer as *const u8, (data as *mut u8).add(1), size);
        }
    }

    let data_slice = unsafe { core::slice::from_raw_parts_mut(data as *mut u8, total_len) };
    let mut msgs = [I2cMsg::new(SCDC_I2C_SLAVE_ADDRESS, 0, data_slice)];

    let err = unsafe { i2c_transfer(adapter, &mut msgs) };

    unsafe { kfree(data) };

    if err < 0 {
        return err as isize;
    }
    if err != 1 {
        return -EPROTO as isize;
    }

    0
}

// upstream: include/drm/display/drm_scdc_helper.h drm_scdc_readb()
pub unsafe extern "C" fn drm_scdc_readb(
    adapter: *mut c_void,
    offset: u8,
    value: *mut u8,
) -> i32 {
    unsafe { drm_scdc_read(adapter, offset, value as *mut c_void, 1) as i32 }
}

// upstream: include/drm/display/drm_scdc_helper.h drm_scdc_writeb()
pub unsafe extern "C" fn drm_scdc_writeb(
    adapter: *mut c_void,
    offset: u8,
    value: u8,
) -> i32 {
    let val_buf = value;
    unsafe {
        drm_scdc_write(
            adapter,
            offset,
            &val_buf as *const u8 as *const c_void,
            1,
        ) as i32
    }
}

// upstream: drivers/gpu/drm/display/drm_scdc_helper.c drm_scdc_get_scrambling_status()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_scdc_get_scrambling_status(connector: *mut DrmConnector) -> bool {
    if connector.is_null() {
        return false;
    }
    let mut status: u8 = 0;
    let ret = unsafe { drm_scdc_readb((*connector).ddc, SCDC_SCRAMBLER_STATUS, &mut status) };
    if ret < 0 {
        unsafe {
            let dev = (*connector).dev;
            let id = (*connector).base.id;
            let name_str = if (*connector).name.is_null() {
                "unknown"
            } else {
                core::ffi::CStr::from_ptr((*connector).name).to_str().unwrap_or("unknown")
            };
            drm_dbg_kms!(
                dev as *const c_void,
                "[CONNECTOR:{}:{}] Failed to read scrambling status: {}\n",
                id,
                name_str,
                ret
            );
        }
        return false;
    }

    (status & SCDC_SCRAMBLING_STATUS) != 0
}

// upstream: drivers/gpu/drm/display/drm_scdc_helper.c drm_scdc_set_scrambling()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_scdc_set_scrambling(
    connector: *mut DrmConnector,
    enable: bool,
) -> bool {
    if connector.is_null() {
        return false;
    }
    let mut config: u8 = 0;
    let ret = unsafe { drm_scdc_readb((*connector).ddc, SCDC_TMDS_CONFIG, &mut config) };
    if ret < 0 {
        unsafe {
            let dev = (*connector).dev;
            let id = (*connector).base.id;
            let name_str = if (*connector).name.is_null() {
                "unknown"
            } else {
                core::ffi::CStr::from_ptr((*connector).name).to_str().unwrap_or("unknown")
            };
            drm_dbg_kms!(
                dev as *const c_void,
                "[CONNECTOR:{}:{}] Failed to read TMDS config: {}\n",
                id,
                name_str,
                ret
            );
        }
        return false;
    }

    if enable {
        config |= SCDC_SCRAMBLING_ENABLE;
    } else {
        config &= !SCDC_SCRAMBLING_ENABLE;
    }

    let ret = unsafe { drm_scdc_writeb((*connector).ddc, SCDC_TMDS_CONFIG, config) };
    if ret < 0 {
        unsafe {
            let dev = (*connector).dev;
            let id = (*connector).base.id;
            let name_str = if (*connector).name.is_null() {
                "unknown"
            } else {
                core::ffi::CStr::from_ptr((*connector).name).to_str().unwrap_or("unknown")
            };
            drm_dbg_kms!(
                dev as *const c_void,
                "[CONNECTOR:{}:{}] Failed to enable scrambling: {}\n",
                id,
                name_str,
                ret
            );
        }
        return false;
    }

    true
}

// upstream: drivers/gpu/drm/display/drm_scdc_helper.c drm_scdc_set_high_tmds_clock_ratio()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_scdc_set_high_tmds_clock_ratio(
    connector: *mut DrmConnector,
    set: bool,
) -> bool {
    if connector.is_null() {
        return false;
    }
    let mut config: u8 = 0;
    let ret = unsafe { drm_scdc_readb((*connector).ddc, SCDC_TMDS_CONFIG, &mut config) };
    if ret < 0 {
        unsafe {
            let dev = (*connector).dev;
            let id = (*connector).base.id;
            let name_str = if (*connector).name.is_null() {
                "unknown"
            } else {
                core::ffi::CStr::from_ptr((*connector).name).to_str().unwrap_or("unknown")
            };
            drm_dbg_kms!(
                dev as *const c_void,
                "[CONNECTOR:{}:{}] Failed to read TMDS config: {}\n",
                id,
                name_str,
                ret
            );
        }
        return false;
    }

    if set {
        config |= SCDC_TMDS_BIT_CLOCK_RATIO_BY_40;
    } else {
        config &= !SCDC_TMDS_BIT_CLOCK_RATIO_BY_40;
    }

    let ret = unsafe { drm_scdc_writeb((*connector).ddc, SCDC_TMDS_CONFIG, config) };
    if ret < 0 {
        unsafe {
            let dev = (*connector).dev;
            let id = (*connector).base.id;
            let name_str = if (*connector).name.is_null() {
                "unknown"
            } else {
                core::ffi::CStr::from_ptr((*connector).name).to_str().unwrap_or("unknown")
            };
            drm_dbg_kms!(
                dev as *const c_void,
                "[CONNECTOR:{}:{}] Failed to set TMDS clock ratio: {}\n",
                id,
                name_str,
                ret
            );
        }
        return false;
    }

    usleep_range(1000, 2000);
    true
}
