// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// Source: drivers/gpu/drm/i915/display/intel_dram.c (Linux v7.2.3)

#![allow(non_snake_case, dead_code, unsafe_code)]

use core::{
    ffi::{c_char, c_void},
    mem::size_of,
};

use crate::{
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_wa_upstream::{DISPLAY_VER, platform_dg2},
    intel_dram_types_upstream::{
        DramInfo, INTEL_DRAM_DDR3, INTEL_DRAM_DDR4, INTEL_DRAM_DDR5, INTEL_DRAM_LPDDR3,
        INTEL_DRAM_LPDDR4, INTEL_DRAM_LPDDR5,
    },
};

const GFP_KERNEL: u32 = 0x0cc0;
const ICL_PCODE_MEM_SUBSYSTEM_INFO: u32 = 0x0d;
const ICL_PCODE_MEM_SS_READ_GLOBAL_INFO: u32 = 0x000;
const ENOMEM: i32 = 12;
const EINVAL: i32 = 22;

unsafe extern "C" {
    fn drmm_kzalloc(drm: *mut c_void, size: usize, flags: u32) -> *mut c_void;
    fn intel_parent_pcode_read(
        display: *mut IntelDisplay,
        mbox: u32,
        val: *mut u32,
        val1: *mut u32,
    ) -> i32;
}

fn decode_global_info(value: u32, dram: &mut DramInfo) -> Result<(), i32> {
    dram.r#type = match value & 0x0f {
        0 => INTEL_DRAM_DDR4,
        1 => INTEL_DRAM_DDR5,
        2 => INTEL_DRAM_LPDDR5,
        3 => INTEL_DRAM_LPDDR4,
        4 => INTEL_DRAM_DDR3,
        5 => INTEL_DRAM_LPDDR3,
        _ => return Err(-EINVAL),
    };
    dram.num_channels = ((value & 0x00f0) >> 4) as u8;
    dram.num_qgv_points = ((value & 0x0f00) >> 8) as u8;
    dram.num_psf_gv_points = ((value & 0x3000) >> 12) as u8;
    Ok(())
}

// upstream: intel_dram.c intel_dram_type_str()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dram_type_str(dram_type: u32) -> *const c_char {
    let types = [
        c"UNKNOWN".as_ptr(),
        c"DDR2".as_ptr(),
        c"DDR3".as_ptr(),
        c"DDR4".as_ptr(),
        c"LPDDR3".as_ptr(),
        c"LPDDR4".as_ptr(),
        c"DDR5".as_ptr(),
        c"LPDDR5".as_ptr(),
        c"GDDR".as_ptr(),
        c"GDDR_ECC".as_ptr(),
    ];
    types.get(dram_type as usize).copied().unwrap_or(types[0])
}

// upstream: intel_dram.c intel_dram_detect()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dram_detect(display: *mut IntelDisplay) -> i32 {
    if display.is_null()
        || unsafe { platform_dg2(display) }
        || unsafe { (*display).info.__runtime_info.pipe_mask == 0 }
    {
        return 0;
    }

    let drm = unsafe { (*display).drm };
    let dram_info =
        unsafe { drmm_kzalloc(drm.cast(), size_of::<DramInfo>(), GFP_KERNEL) }.cast::<DramInfo>();
    if dram_info.is_null() {
        return -ENOMEM;
    }
    unsafe { (*display).dram.info = dram_info };

    let version = unsafe { DISPLAY_VER(display) };
    let ret = if version >= 12 && version < 14 {
        let mut value = 0u32;
        let ret = unsafe {
            intel_parent_pcode_read(
                display,
                ICL_PCODE_MEM_SUBSYSTEM_INFO | ICL_PCODE_MEM_SS_READ_GLOBAL_INFO,
                &mut value,
                core::ptr::null_mut(),
            )
        };
        if ret != 0 {
            ret
        } else {
            decode_global_info(value, unsafe { &mut *dram_info })
                .err()
                .unwrap_or(0)
        }
    } else {
        // Other display generations use the legacy MCHBAR, PCODE-channel,
        // or Xe-LPDP register paths from intel_dram.c. They are not part of
        // the ADL-P bring-up target; preserve the unavailable result instead
        // of filling in guessed memory data.
        -EINVAL
    };

    // Linux currently treats DRAM detection failures as diagnostic only and
    // does not abort probe (intel_dram.c TODO at intel_dram_detect()).
    let _ = ret;
    0
}

// upstream: intel_dram.c intel_dram_info()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dram_info(display: *mut IntelDisplay) -> *const DramInfo {
    if display.is_null() {
        core::ptr::null()
    } else {
        unsafe { (*display).dram.info.cast_const() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intel_dram_types_upstream::INTEL_DRAM_UNKNOWN;

    fn empty_info() -> DramInfo {
        DramInfo {
            r#type: INTEL_DRAM_UNKNOWN,
            fsb_freq: 0,
            mem_freq: 0,
            num_channels: 0,
            num_qgv_points: 0,
            num_psf_gv_points: 0,
            ecc_impacting_de_bw: false,
            symmetric_memory: false,
            has_16gb_dimms: false,
            _pad_18: [0; 2],
        }
    }

    #[test]
    fn gen12_pcode_memory_fields_are_decoded_without_guessing() {
        let mut info = empty_info();
        decode_global_info(5 | (2 << 4) | (3 << 8) | (1 << 12), &mut info).unwrap();
        assert_eq!(info.r#type, INTEL_DRAM_LPDDR3);
        assert_eq!(info.num_channels, 2);
        assert_eq!(info.num_qgv_points, 3);
        assert_eq!(info.num_psf_gv_points, 1);

        assert_eq!(decode_global_info(6, &mut info), Err(-EINVAL));
    }

    #[test]
    fn dram_type_names_match_linux_enum_order() {
        let names = [
            c"UNKNOWN".as_ptr(),
            c"DDR2".as_ptr(),
            c"DDR3".as_ptr(),
            c"DDR4".as_ptr(),
            c"LPDDR3".as_ptr(),
            c"LPDDR4".as_ptr(),
            c"DDR5".as_ptr(),
            c"LPDDR5".as_ptr(),
            c"GDDR".as_ptr(),
            c"GDDR_ECC".as_ptr(),
        ];
        for (index, expected) in names.iter().enumerate() {
            assert_eq!(unsafe { intel_dram_type_str(index as u32) }, *expected);
        }
        assert_eq!(unsafe { intel_dram_type_str(u32::MAX) }, names[0]);
    }
}
