// SPDX-License-Identifier: MIT
// Copyright © 2022 Intel Corporation.
// Source: Linux v7.2.3 drivers/gpu/drm/i915/gt/uc/intel_guc_hwconfig.c.
// GuC transport and temporary VMA allocation are owner boundaries; KLV table
// policy, lifecycle, allocation, and failure order follow the source.

#![allow(dead_code, non_snake_case, unsafe_code)]

use core::{ffi::c_void, ptr};

use crate::{
    i915_vma_api_upstream::i915_vma_unpin_and_release,
    i915_vma_types_upstream::I915Vma,
    intel_gt_api_upstream::gt_to_guc,
    intel_gt_types_upstream::IntelGt,
    intel_guc_actions_abi_types_upstream::intel_guc_action,
    intel_guc_types_upstream::IntelGuc,
    intel_hwconfig_types_upstream::IntelHwconfig,
    intel_uc_types_upstream::intel_uc_uses_guc,
    linux::{
        config::{EINVAL, ENOENT, ENOMEM, GFP_KERNEL},
        i915::{GRAPHICS_VER_FULL, IP_VER, IS_ALDERLAKE_P, IS_SUBPLATFORM},
        memory::{kfree, kmalloc},
        registers::I915_VMA_RELEASE_MAP,
    },
};

const INTEL_ALDERLAKE_P: u32 = 35;
const INTEL_SUBPLATFORM_N: u32 = 1;

unsafe extern "C" {
    fn intel_guc_send_mmio(
        guc: *mut IntelGuc,
        action: *const u32,
        len: u32,
        response_buf: *mut u32,
        response_buf_size: u32,
    ) -> i32;
    fn intel_guc_allocate_and_map_vma(
        guc: *mut IntelGuc,
        size: u32,
        vma: *mut *mut I915Vma,
        vaddr: *mut *mut c_void,
    ) -> i32;
}

// upstream: intel_guc_hwconfig.c __guc_action_get_hwconfig()
unsafe fn __guc_action_get_hwconfig(guc: *mut IntelGuc, ggtt_offset: u32, ggtt_size: u32) -> i32 {
    let action = [
        intel_guc_action::INTEL_GUC_ACTION_GET_HWCONFIG as u32,
        ggtt_offset,
        0, // upper_32_bits(u32 GGTT offset)
        ggtt_size,
    ];
    guc_dbg!(
        guc,
        "Querying HW config table: size = %d, offset = 0x%08X\\n",
        ggtt_size,
        ggtt_offset
    );
    let ret = unsafe {
        intel_guc_send_mmio(
            guc,
            action.as_ptr(),
            action.len() as u32,
            ptr::null_mut(),
            0,
        )
    };
    if ret == -crate::linux_config::ENXIO {
        -ENOENT
    } else {
        ret
    }
}

// upstream: intel_guc_hwconfig.c guc_hwconfig_discover_size()
unsafe fn guc_hwconfig_discover_size(guc: *mut IntelGuc, hwconfig: *mut IntelHwconfig) -> i32 {
    let ret = unsafe { __guc_action_get_hwconfig(guc, 0, 0) };
    if ret < 0 {
        return ret;
    }
    if ret == 0 {
        return -EINVAL;
    }
    unsafe {
        (*hwconfig).size = ret as u32;
    }
    0
}

// upstream: intel_guc_hwconfig.c guc_hwconfig_fill_buffer()
unsafe fn guc_hwconfig_fill_buffer(guc: *mut IntelGuc, hwconfig: *mut IntelHwconfig) -> i32 {
    let mut vma: *mut I915Vma = ptr::null_mut();
    let mut vaddr: *mut c_void = ptr::null_mut();
    GEM_BUG_ON!(unsafe { (*hwconfig).size == 0 });
    let ret =
        unsafe { intel_guc_allocate_and_map_vma(guc, (*hwconfig).size, &mut vma, &mut vaddr) };
    if ret != 0 {
        return ret;
    }
    let ggtt_offset = unsafe { crate::intel_guc_upstream::intel_guc_ggtt_offset(guc, vma) };
    let ret = unsafe { __guc_action_get_hwconfig(guc, ggtt_offset, (*hwconfig).size) };
    if ret >= 0 {
        unsafe {
            ptr::copy_nonoverlapping(
                vaddr.cast::<u8>(),
                (*hwconfig).ptr.cast::<u8>(),
                (*hwconfig).size as usize,
            );
        }
    }
    unsafe {
        i915_vma_unpin_and_release(&mut vma, I915_VMA_RELEASE_MAP);
    }
    ret
}

// upstream: intel_guc_hwconfig.c has_table()
unsafe fn has_table(i915: *mut crate::linux_i915_private::DrmI915Private) -> bool {
    if unsafe { IS_ALDERLAKE_P(i915) }
        && !unsafe { IS_SUBPLATFORM(i915, INTEL_ALDERLAKE_P, INTEL_SUBPLATFORM_N) }
    {
        return true;
    }
    unsafe { GRAPHICS_VER_FULL(i915) >= IP_VER(12, 55) }
}

// upstream: intel_guc_hwconfig.c guc_hwconfig_init()
unsafe fn guc_hwconfig_init(gt: *mut IntelGt) -> i32 {
    let hwconfig = unsafe { ptr::addr_of_mut!((*gt).info.hwconfig) };
    let guc = unsafe { gt_to_guc(gt) };
    if !unsafe { has_table((*gt).i915) } {
        return 0;
    }
    let ret = unsafe { guc_hwconfig_discover_size(guc, hwconfig) };
    if ret != 0 {
        return ret;
    }
    let ptr = kmalloc(unsafe { (*hwconfig).size as usize }, GFP_KERNEL);
    if ptr.is_null() {
        unsafe {
            (*hwconfig).size = 0;
        }
        return -ENOMEM;
    }
    unsafe {
        (*hwconfig).ptr = ptr;
    }
    let ret = unsafe { guc_hwconfig_fill_buffer(guc, hwconfig) };
    if ret < 0 {
        unsafe {
            intel_gt_fini_hwconfig(gt);
        }
        return ret;
    }
    0
}

// upstream: intel_guc_hwconfig.c intel_gt_init_hwconfig()
pub unsafe fn intel_gt_init_hwconfig(gt: *mut IntelGt) -> i32 {
    if !unsafe { intel_uc_uses_guc(ptr::addr_of_mut!((*gt).uc)) } {
        return 0;
    }
    unsafe { guc_hwconfig_init(gt) }
}

// upstream: intel_guc_hwconfig.c intel_gt_fini_hwconfig()
pub unsafe fn intel_gt_fini_hwconfig(gt: *mut IntelGt) {
    let hwconfig = unsafe { ptr::addr_of_mut!((*gt).info.hwconfig) };
    unsafe {
        kfree((*hwconfig).ptr);
        (*hwconfig).size = 0;
        (*hwconfig).ptr = ptr::null_mut();
    }
}
