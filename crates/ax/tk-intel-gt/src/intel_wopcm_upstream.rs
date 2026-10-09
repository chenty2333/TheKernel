// SPDX-License-Identifier: MIT
// Copyright © 2017-2019 Intel Corporation.
//! Source-order translation of Linux v7.2.3
//! `drivers/gpu/drm/i915/gt/intel_wopcm.c`.

#![allow(unsafe_code, non_snake_case)]

use core::{ffi::c_void, mem::size_of, ptr};

use crate::{
    intel_gt_types_upstream::IntelGt,
    intel_uc_fw_types_upstream::{INTEL_UC_FW_TYPE_GUC, INTEL_UC_FW_TYPE_HUC},
    intel_uc_types_upstream::intel_uc_supports_huc,
    intel_uncore_types_upstream::{intel_uncore_read, IntelUncore},
    intel_wopcm_types_upstream::{IntelWopcm, IntelWopcmGuc},
    intel_workarounds_types_upstream::I915RegT,
    linux::{
        i915::{
            GRAPHICS_VER, HAS_GUC_DEPRIVILEGE, HAS_GT_UC, IS_GEN9_LP,
        },
        i915_private::DrmI915Private,
    },
    wopcm::{
        GUC_WOPCM_OFFSET_ALIGNMENT, GUC_WOPCM_OFFSET_MASK, GUC_WOPCM_OFFSET_VALID,
        GUC_WOPCM_RESERVED, GUC_WOPCM_SIZE, GUC_WOPCM_SIZE_LOCKED, GUC_WOPCM_SIZE_MASK,
        GUC_WOPCM_STACK_RESERVED, MAX_WOPCM_SIZE, WOPCM_RESERVED_SIZE, GEN11_WOPCM_SIZE,
    },
};

const GEN9_WOPCM_SIZE: u32 = 1024 * 1024;
const BXT_WOPCM_RC6_CTX_RESERVED: u32 = 16 * 1024 + 8 * 1024;
const ICL_WOPCM_HW_CTX_RESERVED: u32 = 32 * 1024 + 4 * 1024;
const GEN9_GUC_FW_RESERVED: u32 = 128 * 1024;
const GEN9_GUC_WOPCM_OFFSET: u32 = GUC_WOPCM_RESERVED + GEN9_GUC_FW_RESERVED;
const DMA_GUC_WOPCM_OFFSET: u32 = crate::wopcm::DMA_GUC_WOPCM_OFFSET;
const GUC_SHIM_CONTROL2: I915RegT = I915RegT { reg: 0xc068 };
const GUC_IS_PRIVILEGED: u32 = 1 << 29;

fn uc_fw_type_repr(ty: crate::intel_uc_fw_types_upstream::IntelUcFwType) -> &'static str {
    match ty {
        INTEL_UC_FW_TYPE_GUC => "GuC",
        INTEL_UC_FW_TYPE_HUC => "HuC",
        _ => "unknown uC",
    }
}

#[inline]
const fn reg(offset: u32) -> I915RegT {
    I915RegT { reg: offset }
}

// upstream: intel_wopcm.c wopcm_to_gt()
#[inline]
unsafe fn wopcm_to_gt(wopcm: *mut IntelWopcm) -> *mut IntelGt {
    container_of!(wopcm, IntelGt, wopcm)
}

// upstream: intel_wopcm.c intel_wopcm_init_early()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_wopcm_init_early(wopcm: *mut IntelWopcm) {
    let gt = unsafe { wopcm_to_gt(wopcm) };
    let i915 = unsafe { (*gt).i915 };
    if !unsafe { HAS_GT_UC(i915) } {
        return;
    }
    unsafe {
        (*wopcm).size = if GRAPHICS_VER(i915) >= 11 {
            GEN11_WOPCM_SIZE
        } else {
            GEN9_WOPCM_SIZE
        };
        drm_dbg!(
            ptr::addr_of_mut!((*i915).drm),
            "WOPCM: %uK\n",
            (*wopcm).size / 1024
        );
    }
}

// upstream: intel_wopcm.c context_reserved_size()
unsafe fn context_reserved_size(i915: *mut DrmI915Private) -> u32 {
    if unsafe { IS_GEN9_LP(i915) } {
        BXT_WOPCM_RC6_CTX_RESERVED
    } else if unsafe { GRAPHICS_VER(i915) >= 11 } {
        ICL_WOPCM_HW_CTX_RESERVED
    } else {
        0
    }
}

// upstream: intel_wopcm.c gen9_check_dword_gap()
unsafe fn gen9_check_dword_gap(
    i915: *mut DrmI915Private,
    guc_wopcm_base: u32,
    guc_wopcm_size: u32,
) -> bool {
    let offset = guc_wopcm_base + GEN9_GUC_WOPCM_OFFSET;
    if offset > guc_wopcm_size || guc_wopcm_size - offset < size_of::<u32>() as u32 {
        drm_err!(
            ptr::addr_of_mut!((*i915).drm),
            "WOPCM: invalid GuC region size: %uK < %uK\n",
            guc_wopcm_size / 1024,
            (offset + size_of::<u32>() as u32) / 1024
        );
        return false;
    }
    true
}

// upstream: intel_wopcm.c gen9_check_huc_fw_fits()
unsafe fn gen9_check_huc_fw_fits(
    i915: *mut DrmI915Private,
    guc_wopcm_size: u32,
    huc_fw_size: u32,
) -> bool {
    if huc_fw_size > guc_wopcm_size - GUC_WOPCM_RESERVED {
        drm_err!(
            ptr::addr_of_mut!((*i915).drm),
            "WOPCM: no space for %s: %uK < %uK\n",
            uc_fw_type_repr(INTEL_UC_FW_TYPE_HUC),
            (guc_wopcm_size - GUC_WOPCM_RESERVED) / 1024,
            huc_fw_size / 1024
        );
        return false;
    }
    true
}

// upstream: intel_wopcm.c check_hw_restrictions()
unsafe fn check_hw_restrictions(
    i915: *mut DrmI915Private,
    guc_wopcm_base: u32,
    guc_wopcm_size: u32,
    huc_fw_size: u32,
) -> bool {
    if unsafe { GRAPHICS_VER(i915) == 9 }
        && !unsafe { gen9_check_dword_gap(i915, guc_wopcm_base, guc_wopcm_size) }
    {
        return false;
    }
    if unsafe { GRAPHICS_VER(i915) == 9 }
        && !unsafe { gen9_check_huc_fw_fits(i915, guc_wopcm_size, huc_fw_size) }
    {
        return false;
    }
    true
}

// upstream: intel_wopcm.c __check_layout()
unsafe fn __check_layout(
    gt: *mut IntelGt,
    wopcm_size: u32,
    guc_wopcm_base: u32,
    guc_wopcm_size: u32,
    guc_fw_size: u32,
    huc_fw_size: u32,
) -> bool {
    let i915 = unsafe { (*gt).i915 };
    let ctx_rsvd = unsafe { context_reserved_size(i915) };
    let usable_size = wopcm_size - ctx_rsvd;
    if guc_wopcm_base
        .checked_add(guc_wopcm_size)
        .is_none_or(|end| end > usable_size)
    {
        drm_err!(
            ptr::addr_of_mut!((*i915).drm),
            "WOPCM: invalid GuC region layout: %uK + %uK > %uK\n",
            guc_wopcm_base / 1024,
            guc_wopcm_size / 1024,
            usable_size / 1024
        );
        return false;
    }

    let required_guc = guc_fw_size + GUC_WOPCM_RESERVED + GUC_WOPCM_STACK_RESERVED;
    if guc_wopcm_size < required_guc {
        drm_err!(
            ptr::addr_of_mut!((*i915).drm),
            "WOPCM: no space for %s: %uK < %uK\n",
            uc_fw_type_repr(INTEL_UC_FW_TYPE_GUC),
            guc_wopcm_size / 1024,
            required_guc / 1024
        );
        return false;
    }

    if unsafe { intel_uc_supports_huc(core::ptr::addr_of_mut!((*gt).uc)) } {
        let required_huc = huc_fw_size + WOPCM_RESERVED_SIZE;
        if guc_wopcm_base < required_huc {
            drm_err!(
                ptr::addr_of_mut!((*i915).drm),
            "WOPCM: no space for %s: %uK < %uK\n",
            uc_fw_type_repr(INTEL_UC_FW_TYPE_HUC),
                guc_wopcm_base / 1024,
                required_huc / 1024
            );
            return false;
        }
    }

    unsafe { check_hw_restrictions(i915, guc_wopcm_base, guc_wopcm_size, huc_fw_size) }
}

// upstream: intel_wopcm.c __wopcm_regs_locked()
unsafe fn __wopcm_regs_locked(
    uncore: *mut IntelUncore,
    guc_wopcm_base: *mut u32,
    guc_wopcm_size: *mut u32,
) -> bool {
    let reg_base = unsafe { intel_uncore_read(uncore, reg(DMA_GUC_WOPCM_OFFSET)) };
    let reg_size = unsafe { intel_uncore_read(uncore, reg(GUC_WOPCM_SIZE)) };
    if reg_size & GUC_WOPCM_SIZE_LOCKED == 0 || reg_base & GUC_WOPCM_OFFSET_VALID == 0 {
        return false;
    }
    unsafe {
        *guc_wopcm_base = reg_base & GUC_WOPCM_OFFSET_MASK;
        *guc_wopcm_size = reg_size & GUC_WOPCM_SIZE_MASK;
    }
    true
}

// upstream: intel_wopcm.c __wopcm_regs_writable()
unsafe fn __wopcm_regs_writable(uncore: *mut IntelUncore) -> bool {
    let i915 = unsafe { (*uncore).i915 };
    if !unsafe { HAS_GUC_DEPRIVILEGE(i915) } {
        return true;
    }
    unsafe { intel_uncore_read(uncore, GUC_SHIM_CONTROL2) & GUC_IS_PRIVILEGED != 0 }
}

// upstream: intel_wopcm.c intel_wopcm_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_wopcm_init(wopcm: *mut IntelWopcm) {
    let gt = unsafe { wopcm_to_gt(wopcm) };
    let i915 = unsafe { (*gt).i915 };
    let guc_fw_size = unsafe {
        crate::intel_uc_fw_upstream::intel_uc_fw_get_upload_size(&(*gt).uc.guc.fw)
    };
    let huc_fw_size = unsafe {
        crate::intel_uc_fw_upstream::intel_uc_fw_get_upload_size(&(*gt).uc.huc.fw)
    };
    let ctx_rsvd = unsafe { context_reserved_size(i915) };
    let mut wopcm_size = unsafe { (*wopcm).size };
    let mut guc_wopcm_base = 0u32;
    let mut guc_wopcm_size = 0u32;

    if guc_fw_size == 0 {
        return;
    }
    GEM_BUG_ON!(wopcm_size == 0);
    GEM_BUG_ON!(unsafe { (*wopcm).guc.base != 0 });
    GEM_BUG_ON!(unsafe { (*wopcm).guc.size != 0 });
    GEM_BUG_ON!(guc_fw_size >= wopcm_size);
    GEM_BUG_ON!(huc_fw_size >= wopcm_size);
    GEM_BUG_ON!(ctx_rsvd + WOPCM_RESERVED_SIZE >= wopcm_size);

    if unsafe {
        __wopcm_regs_locked(
            (*gt).uncore,
            &mut guc_wopcm_base,
            &mut guc_wopcm_size,
        )
    } {
        drm_dbg!(
            ptr::addr_of_mut!((*i915).drm),
            "GuC WOPCM is already locked [%uK, %uK)\n",
            guc_wopcm_base / 1024,
            guc_wopcm_size / 1024
        );
        if !unsafe { __wopcm_regs_writable((*gt).uncore) } {
            wopcm_size = MAX_WOPCM_SIZE;
        }
    } else {
        if unsafe { !(*i915).media_gt.is_null() } {
            drm_err!(ptr::addr_of_mut!((*i915).drm), "Unlocked WOPCM regs with media GT\n");
            return;
        }

        guc_wopcm_base = huc_fw_size + WOPCM_RESERVED_SIZE;
        guc_wopcm_base = guc_wopcm_base
            .checked_add(GUC_WOPCM_OFFSET_ALIGNMENT - 1)
            .expect("WOPCM base overflow")
            & !(GUC_WOPCM_OFFSET_ALIGNMENT - 1);
        guc_wopcm_base = guc_wopcm_base.min(wopcm_size - ctx_rsvd);
        guc_wopcm_size = (wopcm_size - ctx_rsvd - guc_wopcm_base) & GUC_WOPCM_SIZE_MASK;
        drm_dbg!(
            ptr::addr_of_mut!((*i915).drm),
            "Calculated GuC WOPCM [%uK, %uK)\n",
            guc_wopcm_base / 1024,
            guc_wopcm_size / 1024
        );
    }

    if unsafe {
        __check_layout(
            gt,
            wopcm_size,
            guc_wopcm_base,
            guc_wopcm_size,
            guc_fw_size,
            huc_fw_size,
        )
    } {
        unsafe {
            (*wopcm).guc.base = guc_wopcm_base;
            (*wopcm).guc.size = guc_wopcm_size;
        }
        GEM_BUG_ON!(unsafe { (*wopcm).guc.base == 0 });
        GEM_BUG_ON!(unsafe { (*wopcm).guc.size == 0 });
    }
}
