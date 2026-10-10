// SPDX-License-Identifier: MIT
// Copyright © 2014-2018 Intel Corporation.
// Source-order translation of Linux 7.2.3 drivers/gpu/drm/i915/gt/uc/intel_guc_fw.c.
#![allow(non_snake_case, non_upper_case_globals, unsafe_op_in_unsafe_fn)]

use core::{
    ffi::{c_int, c_void},
    mem::offset_of,
};

use crate::{
    intel_gt_api_upstream::guc_to_gt,
    intel_gt_types_upstream::IntelGt,
    intel_guc_types_upstream::IntelGuc,
    intel_guc_upstream::intel_guc_ggtt_offset,
    intel_rps_upstream::{intel_rps_get_requested_frequency, intel_rps_read_actual_frequency},
    intel_uc_fw_types_upstream::{INTEL_UC_FIRMWARE_LOAD_FAIL, INTEL_UC_FIRMWARE_RUNNING, IntelUcFw},
    intel_uc_fw_upstream::{
        intel_uc_fw_change_status, intel_uc_fw_copy_rsa, intel_uc_fw_upload,
    },
    intel_uncore_types_upstream::{IntelUncore, intel_uncore_read, intel_uncore_rmw, intel_uncore_write},
    intel_workarounds_types_upstream::I915RegT,
    linux::{
        i915::{GRAPHICS_VER, GRAPHICS_VER_FULL, IP_VER, IS_GEN9_LP},
        primitives::ktime_get,
        registers::REG_FIELD_GET,
    },
    linux_config::{EINVAL, ENOEXEC, ENOMEM, ENXIO, EPERM, ETIMEDOUT},
    linux_wait::wait_until,
};

// intel_guc_reg.h
const GUC_SHIM_CONTROL: I915RegT = I915RegT { reg: 0xc064 };
const GUC_SHIM_CONTROL2: I915RegT = I915RegT { reg: 0xc068 };
const GUC_DISABLE_SRAM_INIT_TO_ZEROES: u32 = 1 << 0;
const GUC_ENABLE_READ_CACHE_LOGIC: u32 = 1 << 1;
const GUC_ENABLE_MIA_CACHING: u32 = 1 << 2;
const GUC_ENABLE_READ_CACHE_FOR_SRAM_DATA: u32 = 1 << 9;
const GUC_ENABLE_READ_CACHE_FOR_WOPCM_DATA: u32 = 1 << 10;
const GUC_ENABLE_DEBUG_REG: u32 = 1 << 11;
const GUC_ENABLE_MIA_CLOCK_GATING: u32 = 1 << 15;
const GEN9LP_GT_PM_CONFIG: I915RegT = I915RegT { reg: 0x138140 };
const GEN9_GT_PM_CONFIG: I915RegT = I915RegT { reg: 0x13816c };
const GT_DOORBELL_ENABLE: u32 = 1 << 0;
const GUC_ARAT_C6DIS: I915RegT = I915RegT { reg: 0xa178 };
const GUC_STATUS: I915RegT = I915RegT { reg: 0xc000 };
const GUC_HEADER_INFO: I915RegT = I915RegT { reg: 0xc014 };
const UOS_RSA_SCRATCH_COUNT: usize = 64;
const SOFT_SCRATCH_BASE: u32 = 0xc180;
const UOS_RSA_SCRATCH_BASE: u32 = 0xc200;
const GS_MIA_IN_RESET: u32 = 0x01 << 0;
const GS_BOOTROM_SHIFT: u32 = 1;
const GS_BOOTROM_MASK: u32 = 0x7f << GS_BOOTROM_SHIFT;
const GS_UKERNEL_SHIFT: u32 = 8;
const GS_UKERNEL_MASK: u32 = 0xff << GS_UKERNEL_SHIFT;
const GS_MIA_SHIFT: u32 = 16;
const GS_MIA_MASK: u32 = 0x07 << GS_MIA_SHIFT;
const GS_AUTH_STATUS_SHIFT: u32 = 30;
const GS_AUTH_STATUS_MASK: u32 = 0x03 << GS_AUTH_STATUS_SHIFT;

// intel_gt_regs.h
const GEN7_MISCCPCTL: I915RegT = I915RegT { reg: 0x9424 };
const GEN8_DOP_CLOCK_GATE_GUC_ENABLE: u32 = 1 << 4;

// abi/guc_errors_abi.h
const INTEL_GUC_LOAD_STATUS_READY: u32 = 0xf0;
const INTEL_GUC_LOAD_STATUS_ERROR_DEVID_BUILD_MISMATCH: u32 = 0x02;
const INTEL_GUC_LOAD_STATUS_GUC_PREPROD_BUILD_MISMATCH: u32 = 0x03;
const INTEL_GUC_LOAD_STATUS_ERROR_DEVID_INVALID_GUCTYPE: u32 = 0x04;
const INTEL_GUC_LOAD_STATUS_HWCONFIG_START: u32 = 0x05;
const INTEL_GUC_LOAD_STATUS_HWCONFIG_ERROR: u32 = 0x07;
const INTEL_GUC_LOAD_STATUS_DPC_ERROR: u32 = 0x60;
const INTEL_GUC_LOAD_STATUS_EXCEPTION: u32 = 0x70;
const INTEL_GUC_LOAD_STATUS_INIT_DATA_INVALID: u32 = 0x71;
const INTEL_GUC_LOAD_STATUS_MPU_DATA_INVALID: u32 = 0x73;
const INTEL_GUC_LOAD_STATUS_INIT_MMIO_SAVE_RESTORE_INVALID: u32 = 0x74;
const INTEL_GUC_LOAD_STATUS_KLV_WORKAROUND_INIT_ERROR: u32 = 0x75;
const INTEL_BOOTROM_STATUS_NO_KEY_FOUND: u32 = 0x13;
const INTEL_BOOTROM_STATUS_RSA_FAILED: u32 = 0x50;
const INTEL_BOOTROM_STATUS_PAVPC_FAILED: u32 = 0x73;
const INTEL_BOOTROM_STATUS_WOPCM_FAILED: u32 = 0x74;
const INTEL_BOOTROM_STATUS_LOADLOC_FAILED: u32 = 0x75;
const INTEL_BOOTROM_STATUS_JUMP_FAILED: u32 = 0x77;
const INTEL_BOOTROM_STATUS_RC6CTXCONFIG_FAILED: u32 = 0x79;
const INTEL_BOOTROM_STATUS_MPUMAP_INCORRECT: u32 = 0x7a;
const INTEL_BOOTROM_STATUS_EXCEPTION: u32 = 0x7e;
const INTEL_BOOTROM_STATUS_PROD_KEY_CHECK_FAILURE: u32 = 0x2b;

const UOS_MOVE: u32 = 1 << 4;
const GUC_LOAD_RETRY_LIMIT: u32 = 3; // CONFIG_DRM_I915_DEBUG_GEM is off.

// upstream: intel_guc_fw.c guc_prepare_xfer()
unsafe fn guc_prepare_xfer(gt: *mut IntelGt) {
    let uncore = unsafe { (*gt).uncore };
    let i915 = unsafe { (*uncore).i915 };
    let mut shim_flags = GUC_ENABLE_READ_CACHE_LOGIC
        | GUC_ENABLE_READ_CACHE_FOR_SRAM_DATA
        | GUC_ENABLE_READ_CACHE_FOR_WOPCM_DATA
        | GUC_ENABLE_MIA_CLOCK_GATING;

    if unsafe { GRAPHICS_VER_FULL(i915) } < IP_VER(12, 55) {
        shim_flags |= GUC_DISABLE_SRAM_INIT_TO_ZEROES | GUC_ENABLE_MIA_CACHING;
    }

    unsafe { intel_uncore_write(uncore, GUC_SHIM_CONTROL, shim_flags) };

    if unsafe { IS_GEN9_LP(i915) } {
        unsafe { intel_uncore_write(uncore, GEN9LP_GT_PM_CONFIG, GT_DOORBELL_ENABLE) };
    } else {
        unsafe { intel_uncore_write(uncore, GEN9_GT_PM_CONFIG, GT_DOORBELL_ENABLE) };
    }

    if unsafe { GRAPHICS_VER(i915) } == 9 {
        unsafe {
            intel_uncore_rmw(uncore, GEN7_MISCCPCTL, 0, GEN8_DOP_CLOCK_GATE_GUC_ENABLE);
            intel_uncore_write(uncore, GUC_ARAT_C6DIS, 0x1ff);
        }
    }

    if unsafe { GRAPHICS_VER_FULL(i915) } >= IP_VER(12, 50) {
        unsafe { intel_uncore_rmw(uncore, GUC_SHIM_CONTROL2, 0, GUC_ENABLE_DEBUG_REG) };
    }
}

// upstream: intel_guc_fw.c guc_xfer_rsa_mmio()
unsafe fn guc_xfer_rsa_mmio(guc_fw: *mut IntelUcFw, uncore: *mut IntelUncore) -> c_int {
    let mut rsa = [0u32; UOS_RSA_SCRATCH_COUNT];
    let copied = unsafe { intel_uc_fw_copy_rsa(guc_fw, rsa.as_mut_ptr().cast::<c_void>(), core::mem::size_of_val(&rsa) as u32) };
    if copied < core::mem::size_of_val(&rsa) {
        return -ENOMEM;
    }
    for (i, value) in rsa.iter().enumerate() {
        unsafe { intel_uncore_write(uncore, I915RegT { reg: UOS_RSA_SCRATCH_BASE + i as u32 * 4 }, *value) };
    }
    0
}

// upstream: intel_guc_fw.c guc_xfer_rsa_vma()
unsafe fn guc_xfer_rsa_vma(guc_fw: *mut IntelUcFw, uncore: *mut IntelUncore) -> c_int {
    let guc = unsafe { guc_fw.cast::<u8>().sub(offset_of!(IntelGuc, fw)).cast::<IntelGuc>() };
    let offset = unsafe { intel_guc_ggtt_offset(guc, (*guc_fw).rsa_data) };
    unsafe { intel_uncore_write(uncore, I915RegT { reg: UOS_RSA_SCRATCH_BASE }, offset) };
    0
}

// upstream: intel_guc_fw.c guc_xfer_rsa()
unsafe fn guc_xfer_rsa(guc_fw: *mut IntelUcFw, uncore: *mut IntelUncore) -> c_int {
    if unsafe { !(*guc_fw).rsa_data.is_null() } {
        unsafe { guc_xfer_rsa_vma(guc_fw, uncore) }
    } else {
        unsafe { guc_xfer_rsa_mmio(guc_fw, uncore) }
    }
}

// upstream: intel_guc_fw.c guc_load_done()
unsafe fn guc_load_done(uncore: *mut IntelUncore, status: *mut u32, success: *mut bool) -> bool {
    let val = unsafe { intel_uncore_read(uncore, GUC_STATUS) };
    let uk_val = REG_FIELD_GET(GS_UKERNEL_MASK, val);
    let br_val = REG_FIELD_GET(GS_BOOTROM_MASK, val);

    unsafe { *status = val };

    match uk_val {
        INTEL_GUC_LOAD_STATUS_READY => {
            unsafe { *success = true };
            return true;
        }
        INTEL_GUC_LOAD_STATUS_ERROR_DEVID_BUILD_MISMATCH
        | INTEL_GUC_LOAD_STATUS_GUC_PREPROD_BUILD_MISMATCH
        | INTEL_GUC_LOAD_STATUS_ERROR_DEVID_INVALID_GUCTYPE
        | INTEL_GUC_LOAD_STATUS_HWCONFIG_ERROR
        | INTEL_GUC_LOAD_STATUS_DPC_ERROR
        | INTEL_GUC_LOAD_STATUS_EXCEPTION
        | INTEL_GUC_LOAD_STATUS_INIT_DATA_INVALID
        | INTEL_GUC_LOAD_STATUS_MPU_DATA_INVALID
        | INTEL_GUC_LOAD_STATUS_INIT_MMIO_SAVE_RESTORE_INVALID
        | INTEL_GUC_LOAD_STATUS_KLV_WORKAROUND_INIT_ERROR => {
            unsafe { *success = false };
            return true;
        }
        _ => {}
    }

    match br_val {
        INTEL_BOOTROM_STATUS_NO_KEY_FOUND
        | INTEL_BOOTROM_STATUS_RSA_FAILED
        | INTEL_BOOTROM_STATUS_PAVPC_FAILED
        | INTEL_BOOTROM_STATUS_WOPCM_FAILED
        | INTEL_BOOTROM_STATUS_LOADLOC_FAILED
        | INTEL_BOOTROM_STATUS_JUMP_FAILED
        | INTEL_BOOTROM_STATUS_RC6CTXCONFIG_FAILED
        | INTEL_BOOTROM_STATUS_MPUMAP_INCORRECT
        | INTEL_BOOTROM_STATUS_EXCEPTION
        | INTEL_BOOTROM_STATUS_PROD_KEY_CHECK_FAILURE => {
            unsafe { *success = false };
            return true;
        }
        _ => {}
    }

    false
}

// upstream: intel_guc_fw.c guc_wait_ucode()
unsafe fn guc_wait_ucode(guc: *mut IntelGuc) -> c_int {
    let gt = unsafe { guc_to_gt(guc) };
    let uncore = unsafe { (*gt).uncore };
    let rps = unsafe { core::ptr::addr_of_mut!((*gt).rps) };

    let mut status: u32 = 0;
    let mut success = false;
    let mut ret: c_int = 0;
    let mut count: u32 = 0;

    let before_freq = unsafe { intel_rps_read_actual_frequency(rps) };
    let before = ktime_get();

    while count < GUC_LOAD_RETRY_LIMIT {
        // wait_for(guc_load_done(...), 1000): 0 on success, -ETIMEDOUT on timeout.
        let done = wait_until(
            1000u64 * 1_000_000,
            || unsafe { guc_load_done(uncore, &mut status, &mut success) },
            true,
        );
        ret = if done { 0 } else { -ETIMEDOUT };
        if ret == 0 || !success {
            break;
        }
        guc_dbg!(
            guc,
            "load still in progress, count = %d, freq = %dMHz, status = 0x%08X [0x%02X/%02X]\n",
            count as i32,
            unsafe { intel_rps_read_actual_frequency(rps) } as i32,
            status,
            REG_FIELD_GET(GS_BOOTROM_MASK, status),
            REG_FIELD_GET(GS_UKERNEL_MASK, status)
        );
        count += 1;
    }

    let after = ktime_get();
    let delta_ms = (after - before) / 1_000_000;

    if ret != 0 || !success {
        let ukernel = REG_FIELD_GET(GS_UKERNEL_MASK, status);
        let bootrom = REG_FIELD_GET(GS_BOOTROM_MASK, status);

        guc_info!(
            guc,
            "load failed: status = 0x%08X, time = %lldms, freq = %dMHz, ret = %d\n",
            status,
            delta_ms,
            unsafe { intel_rps_read_actual_frequency(rps) } as i32,
            ret
        );
        guc_info!(
            guc,
            "load failed: status: Reset = %d, BootROM = 0x%02X, UKernel = 0x%02X, MIA = 0x%02X, Auth = 0x%02X\n",
            REG_FIELD_GET(GS_MIA_IN_RESET, status) as i32,
            bootrom,
            ukernel,
            REG_FIELD_GET(GS_MIA_MASK, status),
            REG_FIELD_GET(GS_AUTH_STATUS_MASK, status)
        );

        match bootrom {
            INTEL_BOOTROM_STATUS_NO_KEY_FOUND => {
                guc_info!(
                    guc,
                    "invalid key requested, header = 0x%08X\n",
                    unsafe { intel_uncore_read(uncore, GUC_HEADER_INFO) }
                );
                ret = -ENOEXEC;
            }
            INTEL_BOOTROM_STATUS_RSA_FAILED => {
                guc_info!(guc, "firmware signature verification failed\n");
                ret = -ENOEXEC;
            }
            INTEL_BOOTROM_STATUS_PROD_KEY_CHECK_FAILURE => {
                guc_info!(guc, "firmware production part check failure\n");
                ret = -ENOEXEC;
            }
            _ => {}
        }

        match ukernel {
            INTEL_GUC_LOAD_STATUS_EXCEPTION => {
                guc_info!(
                    guc,
                    "firmware exception. EIP: %#x\n",
                    unsafe { intel_uncore_read(uncore, I915RegT { reg: SOFT_SCRATCH_BASE + 13 * 4 }) }
                );
                ret = -ENXIO;
            }
            INTEL_GUC_LOAD_STATUS_INIT_MMIO_SAVE_RESTORE_INVALID => {
                guc_info!(guc, "illegal register in save/restore workaround list\n");
                ret = -EPERM;
            }
            INTEL_GUC_LOAD_STATUS_KLV_WORKAROUND_INIT_ERROR => {
                guc_info!(guc, "invalid w/a KLV entry\n");
                ret = -EINVAL;
            }
            INTEL_GUC_LOAD_STATUS_HWCONFIG_START => {
                guc_info!(guc, "still extracting hwconfig table.\n");
                ret = -ETIMEDOUT;
            }
            _ => {}
        }

        if ret == 0 {
            ret = -ENXIO;
        }
    } else if delta_ms > 200 {
        guc_warn!(
            guc,
            "excessive init time: %lldms! [status = 0x%08X, count = %d, ret = %d]\n",
            delta_ms,
            status,
            count as i32,
            ret
        );
        guc_warn!(
            guc,
            "excessive init time: [freq = %dMHz -> %dMHz vs %dMHz, perf_limit_reasons = 0x%08X]\n",
            before_freq as i32,
            unsafe { intel_rps_read_actual_frequency(rps) } as i32,
            unsafe { intel_rps_get_requested_frequency(rps) } as i32,
            unsafe {
                intel_uncore_read(
                    uncore,
                    crate::intel_gt_upstream::intel_gt_perf_limit_reasons_reg(gt),
                )
            }
        );
    } else {
        guc_dbg!(
            guc,
            "init took %lldms, freq = %dMHz -> %dMHz vs %dMHz, status = 0x%08X, count = %d, ret = %d\n",
            delta_ms,
            before_freq as i32,
            unsafe { intel_rps_read_actual_frequency(rps) } as i32,
            unsafe { intel_rps_get_requested_frequency(rps) } as i32,
            status,
            count as i32,
            ret
        );
    }

    ret
}

// upstream: intel_guc_fw.c intel_guc_fw_upload()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_fw_upload(guc: *mut IntelGuc) -> c_int {
    let gt = unsafe { guc_to_gt(guc) };
    let uncore = unsafe { (*gt).uncore };

    unsafe { guc_prepare_xfer(gt) };

    let mut ret = unsafe { guc_xfer_rsa(core::ptr::addr_of_mut!((*guc).fw), uncore) };
    if ret != 0 {
        return unsafe { intel_guc_fw_upload_fail(guc, ret) };
    }

    ret = unsafe { intel_uc_fw_upload(core::ptr::addr_of_mut!((*guc).fw), 0x2000, UOS_MOVE) };
    if ret != 0 {
        return unsafe { intel_guc_fw_upload_fail(guc, ret) };
    }

    ret = unsafe { guc_wait_ucode(guc) };
    if ret != 0 {
        return unsafe { intel_guc_fw_upload_fail(guc, ret) };
    }

    unsafe { intel_uc_fw_change_status(core::ptr::addr_of_mut!((*guc).fw), INTEL_UC_FIRMWARE_RUNNING) };
    0
}

// upstream: intel_guc_fw.c intel_guc_fw_upload() `out:` label
unsafe fn intel_guc_fw_upload_fail(guc: *mut IntelGuc, ret: c_int) -> c_int {
    unsafe { intel_uc_fw_change_status(core::ptr::addr_of_mut!((*guc).fw), INTEL_UC_FIRMWARE_LOAD_FAIL) };
    ret
}
