// SPDX-License-Identifier: MIT
// Copyright © 2013-2021 Intel Corporation
// Translated from Linux v7.2.3 drivers/gpu/drm/i915/intel_pcode.c.
// The full MIT grant is retained in ../LICENSE-MIT.

#![allow(unsafe_code, non_snake_case, non_upper_case_globals, dead_code)]

use crate::{
    intel_runtime_pm_upstream::{intel_runtime_pm_get, intel_runtime_pm_put},
    intel_uncore_types_upstream::{DrmDevice, IntelUncore, intel_uncore_read_fw, intel_uncore_write_fw},
    intel_uncore_upstream::{__intel_wait_for_register_fw, to_intel_uncore},
    intel_workarounds_types_upstream::I915RegT,
    linux::{
        i915::{GRAPHICS_VER, IS_DGFX},
        mutex::{mutex_lock, mutex_unlock},
        primitives::udelay,
        wait::wait_until,
    },
    linux_config::{EAGAIN, EBUSY, EINVAL, ENODEV, ENXIO, EOVERFLOW, ETIMEDOUT},
    linux_i915_private::DrmI915Private,
};

/// `struct mutex sb_lock` inside `struct drm_i915_private` (x86_64, oracle
/// wt-dev build; `offsetof` from the i915 headers). It serialises the mailbox.
const I915_SB_LOCK_OFFSET: usize = 2504;

/// `EPROBE_DEFER`.
const EPROBE_DEFER: i32 = 517;
const EACCES: i32 = 13;

// PCODE mailbox registers and fields (intel_pcode_regs.h, i915_reg.h).
const GEN6_PCODE_MAILBOX: I915RegT = I915RegT { reg: 0x138124 };
const GEN6_PCODE_DATA: I915RegT = I915RegT { reg: 0x138128 };
const GEN6_PCODE_DATA1: I915RegT = I915RegT { reg: 0x13812C };
const GEN6_PCODE_READY: u32 = 1 << 31;
const GEN6_PCODE_MB_PARAM2_MASK: u32 = 0xff << 16;
const GEN6_PCODE_MB_PARAM1_MASK: u32 = 0xff << 8;
const GEN6_PCODE_MB_COMMAND_MASK: u32 = 0xff;
const GEN6_PCODE_ERROR_MASK: u32 = 0xFF;
const GEN6_PCODE_SUCCESS: u32 = 0x0;
const GEN6_PCODE_ILLEGAL_CMD: u32 = 0x1;
const GEN6_PCODE_MIN_FREQ_TABLE_GT_RATIO_OUT_OF_RANGE: u32 = 0x2;
const GEN6_PCODE_TIMEOUT: u32 = 0x3;
const GEN6_PCODE_UNIMPLEMENTED_CMD: u32 = 0xFF;
const GEN7_PCODE_TIMEOUT: u32 = 0x2;
const GEN7_PCODE_ILLEGAL_DATA: u32 = 0x3;
const GEN7_PCODE_MIN_FREQ_TABLE_GT_RATIO_OUT_OF_RANGE: u32 = 0x10;
const GEN11_PCODE_ILLEGAL_SUBCOMMAND: u32 = 0x4;
const GEN11_PCODE_LOCKED: u32 = 0x6;
const GEN11_PCODE_REJECTED: u32 = 0x11;
const DG1_PCODE_STATUS: u32 = 0x7E;
const DG1_UNCORE_GET_INIT_STATUS: u32 = 0x0;
const DG1_UNCORE_INIT_STATUS_COMPLETE: u32 = 0x1;

/// `REG_FIELD_PREP()` for the mailbox command/param fields.
#[inline]
const fn mb_field_prep(mask: u32, value: u32) -> u32 {
    (value << mask.trailing_zeros()) & mask
}

/// `struct intel_display_pcode_interface` (drm/intel/display_parent_interface.h).
#[repr(C)]
pub struct IntelDisplayPcodeInterface {
    pub read: unsafe extern "C" fn(drm: *mut DrmDevice, mbox: u32, val: *mut u32, val1: *mut u32) -> i32,
    pub write: unsafe extern "C" fn(drm: *mut DrmDevice, mbox: u32, val: u32, timeout_ms: i32) -> i32,
    pub request: unsafe extern "C" fn(
        drm: *mut DrmDevice,
        mbox: u32,
        request: u32,
        reply_mask: u32,
        reply: u32,
        timeout_base_ms: i32,
    ) -> i32,
}

#[inline]
unsafe fn sb_lock(i915: *mut DrmI915Private) -> *mut crate::intel_engine_cs_upstream::Mutex {
    unsafe { i915.cast::<u8>().add(I915_SB_LOCK_OFFSET).cast() }
}

#[inline]
unsafe fn uncore_i915(uncore: *const IntelUncore) -> *mut DrmI915Private {
    unsafe { (*uncore).i915 }
}

#[inline]
unsafe fn uncore_drm(uncore: *const IntelUncore) -> *mut DrmDevice {
    unsafe { uncore_i915(uncore).cast::<u8>().cast::<DrmDevice>() }
}

/// `gen6_check_mailbox_status()`.
fn gen6_check_mailbox_status(mbox: u32) -> i32 {
    match mbox & GEN6_PCODE_ERROR_MASK {
        GEN6_PCODE_SUCCESS => 0,
        GEN6_PCODE_UNIMPLEMENTED_CMD => -ENODEV,
        GEN6_PCODE_ILLEGAL_CMD => -ENXIO,
        GEN6_PCODE_MIN_FREQ_TABLE_GT_RATIO_OUT_OF_RANGE | GEN7_PCODE_MIN_FREQ_TABLE_GT_RATIO_OUT_OF_RANGE => {
            -EOVERFLOW
        }
        GEN6_PCODE_TIMEOUT => -ETIMEDOUT,
        other => {
            MISSING_CASE!(other);
            0
        }
    }
}

/// `gen7_check_mailbox_status()`.
fn gen7_check_mailbox_status(mbox: u32) -> i32 {
    match mbox & GEN6_PCODE_ERROR_MASK {
        GEN6_PCODE_SUCCESS => 0,
        GEN6_PCODE_ILLEGAL_CMD => -ENXIO,
        GEN7_PCODE_TIMEOUT => -ETIMEDOUT,
        GEN7_PCODE_ILLEGAL_DATA => -EINVAL,
        GEN11_PCODE_ILLEGAL_SUBCOMMAND => -ENXIO,
        GEN11_PCODE_LOCKED => -EBUSY,
        GEN11_PCODE_REJECTED => -EACCES,
        GEN7_PCODE_MIN_FREQ_TABLE_GT_RATIO_OUT_OF_RANGE => -EOVERFLOW,
        other => {
            MISSING_CASE!(other);
            0
        }
    }
}

/// `__snb_pcode_rw()`. Caller holds `i915->sb_lock`.
unsafe fn __snb_pcode_rw(
    uncore: *mut IntelUncore,
    mbox: u32,
    val: *mut u32,
    val1: *mut u32,
    fast_timeout_us: i32,
    slow_timeout_ms: i32,
    is_read: bool,
) -> i32 {
    // GEN6_PCODE_* are outside the forcewake domain, so the _fw accessors apply.
    if unsafe { intel_uncore_read_fw(uncore, GEN6_PCODE_MAILBOX) } & GEN6_PCODE_READY != 0 {
        return -EAGAIN;
    }

    unsafe { intel_uncore_write_fw(uncore, GEN6_PCODE_DATA, *val) };
    let data1 = if val1.is_null() { 0 } else { unsafe { *val1 } };
    unsafe { intel_uncore_write_fw(uncore, GEN6_PCODE_DATA1, data1) };
    unsafe { intel_uncore_write_fw(uncore, GEN6_PCODE_MAILBOX, GEN6_PCODE_READY | mbox) };

    let mut mbox_out = mbox;
    if unsafe {
        __intel_wait_for_register_fw(
            uncore,
            GEN6_PCODE_MAILBOX,
            GEN6_PCODE_READY,
            0,
            fast_timeout_us as u32,
            slow_timeout_ms as u32,
            &mut mbox_out,
        )
    } != 0
    {
        return -ETIMEDOUT;
    }

    if is_read {
        unsafe { *val = intel_uncore_read_fw(uncore, GEN6_PCODE_DATA) };
        if !val1.is_null() {
            unsafe { *val1 = intel_uncore_read_fw(uncore, GEN6_PCODE_DATA1) };
        }
    }

    if unsafe { GRAPHICS_VER(uncore_i915(uncore) as *const DrmI915Private) } > 6 {
        gen7_check_mailbox_status(mbox_out)
    } else {
        gen6_check_mailbox_status(mbox_out)
    }
}

/// `snb_pcode_read()`.
// upstream: intel_pcode.c snb_pcode_read()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn snb_pcode_read(
    uncore: *mut IntelUncore,
    mbox: u32,
    val: *mut u32,
    val1: *mut u32,
) -> i32 {
    let i915 = unsafe { uncore_i915(uncore) };
    unsafe { mutex_lock(sb_lock(i915)) };
    let err = unsafe { __snb_pcode_rw(uncore, mbox, val, val1, 500, 20, true) };
    unsafe { mutex_unlock(sb_lock(i915)) };

    if err != 0 {
        drm_dbg!(
            uncore_drm(uncore),
            "warning: pcode (read from mbox %x) mailbox access failed: %d\n",
            mbox,
            err
        );
    }
    err
}

/// `snb_pcode_write_timeout()`.
// upstream: intel_pcode.c snb_pcode_write_timeout()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn snb_pcode_write_timeout(
    uncore: *mut IntelUncore,
    mbox: u32,
    mut val: u32,
    timeout_ms: i32,
) -> i32 {
    let i915 = unsafe { uncore_i915(uncore) };
    unsafe { mutex_lock(sb_lock(i915)) };
    let err = unsafe {
        __snb_pcode_rw(uncore, mbox, &mut val, core::ptr::null_mut(), 250, timeout_ms, false)
    };
    unsafe { mutex_unlock(sb_lock(i915)) };

    if err != 0 {
        drm_dbg!(
            uncore_drm(uncore),
            "warning: pcode (write of 0x%08x to mbox %x) mailbox access failed: %d\n",
            val,
            mbox,
            err
        );
    }
    err
}

/// `snb_pcode_write()` (intel_pcode.h): a one-millisecond write.
#[inline]
pub unsafe fn snb_pcode_write(uncore: *mut IntelUncore, mbox: u32, val: u32) -> i32 {
    unsafe { snb_pcode_write_timeout(uncore, mbox, val, 1) }
}

/// `skl_pcode_try_request()`.
unsafe fn skl_pcode_try_request(
    uncore: *mut IntelUncore,
    mbox: u32,
    request: &mut u32,
    reply_mask: u32,
    reply: u32,
    status: &mut i32,
) -> bool {
    *status = unsafe { __snb_pcode_rw(uncore, mbox, request, core::ptr::null_mut(), 500, 0, true) };
    *status == 0 && (*request & reply_mask) == reply
}

/// `skl_pcode_request()`.
// upstream: intel_pcode.c skl_pcode_request()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_pcode_request(
    uncore: *mut IntelUncore,
    mbox: u32,
    mut request: u32,
    reply_mask: u32,
    reply: u32,
    timeout_base_ms: i32,
) -> i32 {
    let i915 = unsafe { uncore_i915(uncore) };
    let mut status: i32 = 0;
    let ret: i32;

    unsafe { mutex_lock(sb_lock(i915)) };

    // Prime the PCODE by doing a request first: a subsequent request at most
    // `timeout_base_ms` later normally succeeds. `_wait_for()` does not guarantee
    // when its condition is first evaluated, so send the first request explicitly.
    if unsafe { skl_pcode_try_request(uncore, mbox, &mut request, reply_mask, reply, &mut status) } {
        ret = 0;
    } else {
        let wait_ret = wait_until(
            timeout_base_ms as u64 * 1_000_000,
            || unsafe { skl_pcode_try_request(uncore, mbox, &mut request, reply_mask, reply, &mut status) },
            true,
        );
        if !wait_ret {
            ret = 0;
        } else {
            // The poll can time out if few requests were sent while PCODE was busy.
            // Retry with the timeout widened to 50ms, as the C workaround does.
            drm_dbg!(
                uncore_drm(uncore),
                "PCODE timeout, retrying with preemption disabled\n"
            );
            drm_WARN_ON!(uncore_drm(uncore), timeout_base_ms > 3);
            ret = if wait_until(
                50 * 1_000_000,
                || unsafe { skl_pcode_try_request(uncore, mbox, &mut request, reply_mask, reply, &mut status) },
                false,
            ) {
                -ETIMEDOUT
            } else {
                0
            };
        }
    }

    unsafe { mutex_unlock(sb_lock(i915)) };
    if status != 0 { status } else { ret }
}

/// `pcode_init_wait()`.
unsafe fn pcode_init_wait(uncore: *mut IntelUncore, timeout_ms: i32) -> i32 {
    if unsafe {
        __intel_wait_for_register_fw(
            uncore,
            GEN6_PCODE_MAILBOX,
            GEN6_PCODE_READY,
            0,
            500,
            timeout_ms as u32,
            core::ptr::null_mut(),
        )
    } != 0
    {
        return -EPROBE_DEFER;
    }

    unsafe {
        skl_pcode_request(
            uncore,
            DG1_PCODE_STATUS,
            DG1_UNCORE_GET_INIT_STATUS,
            DG1_UNCORE_INIT_STATUS_COMPLETE,
            DG1_UNCORE_INIT_STATUS_COMPLETE,
            timeout_ms,
        )
    }
}

/// `intel_pcode_init()`.
// upstream: intel_pcode.c intel_pcode_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pcode_init(uncore: *mut IntelUncore) -> i32 {
    if !unsafe { IS_DGFX(uncore_i915(uncore) as *const DrmI915Private) } {
        return 0;
    }

    // Wait 10 seconds so that the punit settles and completes outstanding
    // transactions upon module load.
    let mut err = unsafe { pcode_init_wait(uncore, 10000) };
    if err != 0 {
        drm_notice!(uncore_drm(uncore), "Waiting for HW initialisation...\n");
        err = unsafe { pcode_init_wait(uncore, 180000) };
    }
    err
}

/// `snb_pcode_read_p()`.
// upstream: intel_pcode.c snb_pcode_read_p()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn snb_pcode_read_p(
    uncore: *mut IntelUncore,
    mbcmd: u32,
    p1: u32,
    p2: u32,
    val: *mut u32,
) -> i32 {
    let mbox = mb_field_prep(GEN6_PCODE_MB_COMMAND_MASK, mbcmd)
        | mb_field_prep(GEN6_PCODE_MB_PARAM1_MASK, p1)
        | mb_field_prep(GEN6_PCODE_MB_PARAM2_MASK, p2);
    let rpm = unsafe { (*uncore).rpm };
    let mut err = 0;
    let wakeref = unsafe { intel_runtime_pm_get(rpm) };
    if wakeref != core::ptr::null_mut() {
        err = unsafe { snb_pcode_read(uncore, mbox, val, core::ptr::null_mut()) };
        unsafe { intel_runtime_pm_put(rpm, wakeref) };
    }
    err
}

/// `snb_pcode_write_p()`.
// upstream: intel_pcode.c snb_pcode_write_p()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn snb_pcode_write_p(
    uncore: *mut IntelUncore,
    mbcmd: u32,
    p1: u32,
    p2: u32,
    val: u32,
) -> i32 {
    let mbox = mb_field_prep(GEN6_PCODE_MB_COMMAND_MASK, mbcmd)
        | mb_field_prep(GEN6_PCODE_MB_PARAM1_MASK, p1)
        | mb_field_prep(GEN6_PCODE_MB_PARAM2_MASK, p2);
    let rpm = unsafe { (*uncore).rpm };
    let mut err = 0;
    let wakeref = unsafe { intel_runtime_pm_get(rpm) };
    if wakeref != core::ptr::null_mut() {
        err = unsafe { snb_pcode_write(uncore, mbox, val) };
        unsafe { intel_runtime_pm_put(rpm, wakeref) };
    }
    err
}

/// `to_i915(drm)->uncore` for the display-facing wrappers.
#[inline]
unsafe fn drm_uncore(drm: *mut DrmDevice) -> *mut IntelUncore {
    unsafe { to_intel_uncore(drm) }
}

/// `intel_pcode_read()`.
unsafe extern "C" fn intel_pcode_read(drm: *mut DrmDevice, mbox: u32, val: *mut u32, val1: *mut u32) -> i32 {
    unsafe { snb_pcode_read(drm_uncore(drm), mbox, val, val1) }
}

/// `intel_pcode_write_timeout()`.
unsafe extern "C" fn intel_pcode_write_timeout(drm: *mut DrmDevice, mbox: u32, val: u32, timeout_ms: i32) -> i32 {
    unsafe { snb_pcode_write_timeout(drm_uncore(drm), mbox, val, timeout_ms) }
}

/// `intel_pcode_request()`.
unsafe extern "C" fn intel_pcode_request(
    drm: *mut DrmDevice,
    mbox: u32,
    request: u32,
    reply_mask: u32,
    reply: u32,
    timeout_base_ms: i32,
) -> i32 {
    unsafe { skl_pcode_request(drm_uncore(drm), mbox, request, reply_mask, reply, timeout_base_ms) }
}

/// `i915_display_pcode_interface`: the display owner's pcode callback table.
#[unsafe(no_mangle)]
pub static i915_display_pcode_interface: IntelDisplayPcodeInterface = IntelDisplayPcodeInterface {
    read: intel_pcode_read,
    write: intel_pcode_write_timeout,
    request: intel_pcode_request,
};
