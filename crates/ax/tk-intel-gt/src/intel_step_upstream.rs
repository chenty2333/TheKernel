// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation
// Source: Linux v7.2.3 drivers/gpu/drm/i915/intel_step.c (MIT).
#![allow(non_snake_case, unsafe_op_in_unsafe_fn)]

use core::ffi::c_void;

use crate::{
    intel_device_info_types_upstream::{
        INTEL_ALDERLAKE_P, INTEL_ALDERLAKE_S, INTEL_DG2, INTEL_SUBPLATFORM_D,
        INTEL_SUBPLATFORM_G10, INTEL_SUBPLATFORM_G11, INTEL_SUBPLATFORM_G12,
        INTEL_SUBPLATFORM_N, INTEL_SUBPLATFORM_RPL, INTEL_SUBPLATFORM_RPLU,
        INTEL_SUBPLATFORM_UY, INTEL_TIGERLAKE,
    },
    intel_device_info_types_upstream::{DEV_INFO_FLAG_HAS_GMD_ID, IntelDeviceInfo},
    i915_probe_provider_upstream::pci_revision,
    linux::i915::{
        IS_ALDERLAKE_P, IS_ALDERLAKE_S, IS_BROXTON, IS_DG1, IS_ELKHARTLAKE, IS_GEMINILAKE,
        IS_ICELAKE, IS_JASPERLAKE, IS_KABYLAKE, IS_PLATFORM, IS_ROCKETLAKE, IS_SKYLAKE,
        IS_SUBPLATFORM,
    },
    linux_i915_private::DrmI915Private,
};

unsafe extern "C" {
    fn to_pci_dev(dev: *mut c_void) -> *mut c_void;
}

// INTEL_REVID(i915): to_pci_dev(i915->drm.dev)->revision
unsafe fn pci_revision_of(i915: *mut DrmI915Private) -> u8 {
    unsafe { pci_revision(to_pci_dev((*i915).drm.dev)) }
}

// HAS_GMD_ID(i915): INTEL_INFO(i915)->has_gmd_id
unsafe fn has_gmd_id(i915: *mut DrmI915Private) -> bool {
    unsafe {
        let info = &*((*i915).info as *const IntelDeviceInfo);
        info.flag(DEV_INFO_FLAG_HAS_GMD_ID)
    }
}

// `enum intel_step` from include/drm/intel/step.h (STEP_NAME_LIST order).
const STEP_NONE: u8 = 0;
const STEP_A0: u8 = 1;
const STEP_A1: u8 = 2;
const STEP_B0: u8 = 5;
const STEP_B1: u8 = 6;
const STEP_C0: u8 = 9;
const STEP_D0: u8 = 13;
const STEP_D1: u8 = 14;
const STEP_E0: u8 = 17;
const STEP_F0: u8 = 21;
const STEP_G0: u8 = 25;
const STEP_H0: u8 = 29;
const STEP_I1: u8 = 34;
const STEP_J0: u8 = 37;
const STEP_FUTURE: u8 = 41;

// `struct intel_step_info` from intel_step.h: COMMON_STEP() sets both the
// graphics and media steps, so each table entry is one u8 used for both.
// Tables keep the upstream designated-initializer indexing: a 0 entry is
// STEP_NONE, and the array length is the C ARRAY_SIZE (highest index + 1).

// upstream: intel_step.c skl_revids[]
const SKL_REVIDS: [u8; 11] = [0, 0, 0, 0, 0, 0, STEP_G0, STEP_H0, 0, STEP_J0, STEP_I1];
// upstream: intel_step.c kbl_revids[]
const KBL_REVIDS: [u8; 8] = [0, STEP_B0, STEP_C0, STEP_D0, STEP_F0, STEP_C0, STEP_D1, STEP_G0];
// upstream: intel_step.c bxt_revids[]
const BXT_REVIDS: [u8; 14] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, STEP_C0, STEP_C0, STEP_D0, STEP_E0,
];
// upstream: intel_step.c glk_revids[]
const GLK_REVIDS: [u8; 4] = [0, 0, 0, STEP_B0];
// upstream: intel_step.c icl_revids[]
const ICL_REVIDS: [u8; 8] = [0, 0, 0, 0, 0, 0, 0, STEP_D0];
// upstream: intel_step.c jsl_ehl_revids[]
const JSL_EHL_REVIDS: [u8; 2] = [STEP_A0, STEP_B0];
// upstream: intel_step.c tgl_uy_revids[]
const TGL_UY_REVIDS: [u8; 4] = [STEP_A0, STEP_B0, STEP_B1, STEP_C0];
// upstream: intel_step.c tgl_revids[] (same GT stepping as tgl_uy does not mean the same HW)
const TGL_REVIDS: [u8; 2] = [STEP_A0, STEP_B0];
// upstream: intel_step.c rkl_revids[]
const RKL_REVIDS: [u8; 5] = [STEP_A0, STEP_B0, 0, 0, STEP_C0];
// upstream: intel_step.c dg1_revids[]
const DG1_REVIDS: [u8; 2] = [STEP_A0, STEP_B0];
// upstream: intel_step.c adls_revids[]
const ADLS_REVIDS: [u8; 13] = [
    STEP_A0, STEP_A0, 0, 0, STEP_B0, 0, 0, 0, STEP_C0, 0, 0, 0, STEP_D0,
];
// upstream: intel_step.c adlp_revids[]
const ADLP_REVIDS: [u8; 13] = [STEP_A0, 0, 0, 0, STEP_B0, 0, 0, 0, STEP_C0, 0, 0, 0, STEP_C0];
// upstream: intel_step.c dg2_g10_revid_step_tbl[]
const DG2_G10_REVID_STEP_TBL: [u8; 9] = [STEP_A0, STEP_A1, 0, 0, STEP_B0, 0, 0, 0, STEP_C0];
// upstream: intel_step.c dg2_g11_revid_step_tbl[]
const DG2_G11_REVID_STEP_TBL: [u8; 6] = [STEP_A0, 0, 0, 0, STEP_B0, STEP_B1];
// upstream: intel_step.c dg2_g12_revid_step_tbl[]
const DG2_G12_REVID_STEP_TBL: [u8; 2] = [STEP_A0, STEP_A1];
// upstream: intel_step.c adls_rpls_revids[]
const ADLS_RPLS_REVIDS: [u8; 13] = [0, 0, 0, 0, STEP_D0, 0, 0, 0, 0, 0, 0, 0, STEP_D0];
// upstream: intel_step.c adlp_rplp_revids[]
const ADLP_RPLP_REVIDS: [u8; 5] = [0, 0, 0, 0, STEP_C0];
// upstream: intel_step.c adlp_n_revids[]
const ADLP_N_REVIDS: [u8; 1] = [STEP_A0];

// upstream: intel_step.c gmd_to_intel_step()
unsafe fn gmd_to_intel_step(i915: *mut DrmI915Private, gmd_step: u8) -> u8 {
    // C: u8 step = gmd->step + STEP_A0; (wraps in u8)
    let step = gmd_step.wrapping_add(STEP_A0);
    if step >= STEP_FUTURE {
        drm_dbg!(&(*i915).drm, "Using future steppings\n");
        return STEP_FUTURE;
    }
    step
}

// upstream: intel_step.c intel_step_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_step_init(i915: *mut DrmI915Private) {
    unsafe {
        let revids: &[u8];
        let mut step = (STEP_NONE, STEP_NONE);

        let revid = u32::from(pci_revision_of(i915));

        if has_gmd_id(i915) {
            let graphics = gmd_to_intel_step(i915, (*i915).runtime.graphics.ip.step);
            let media = gmd_to_intel_step(i915, (*i915).runtime.media.ip.step);
            (*i915).runtime.step.graphics_step = graphics;
            (*i915).runtime.step.media_step = media;
            return;
        }

        if IS_SUBPLATFORM(i915, INTEL_DG2, INTEL_SUBPLATFORM_G10) {
            revids = &DG2_G10_REVID_STEP_TBL;
        } else if IS_SUBPLATFORM(i915, INTEL_DG2, INTEL_SUBPLATFORM_G11) {
            revids = &DG2_G11_REVID_STEP_TBL;
        } else if IS_SUBPLATFORM(i915, INTEL_DG2, INTEL_SUBPLATFORM_G12) {
            revids = &DG2_G12_REVID_STEP_TBL;
        } else if IS_SUBPLATFORM(i915, INTEL_ALDERLAKE_P, INTEL_SUBPLATFORM_N) {
            revids = &ADLP_N_REVIDS;
        } else if IS_SUBPLATFORM(i915, INTEL_ALDERLAKE_P, INTEL_SUBPLATFORM_RPL) {
            revids = &ADLP_RPLP_REVIDS;
        } else if IS_ALDERLAKE_P(i915) {
            revids = &ADLP_REVIDS;
        } else if IS_SUBPLATFORM(i915, INTEL_ALDERLAKE_S, INTEL_SUBPLATFORM_RPL) {
            revids = &ADLS_RPLS_REVIDS;
        } else if IS_ALDERLAKE_S(i915) {
            revids = &ADLS_REVIDS;
        } else if IS_DG1(i915) {
            revids = &DG1_REVIDS;
        } else if IS_ROCKETLAKE(i915) {
            revids = &RKL_REVIDS;
        } else if IS_SUBPLATFORM(i915, INTEL_TIGERLAKE, INTEL_SUBPLATFORM_UY) {
            revids = &TGL_UY_REVIDS;
        } else if IS_PLATFORM(i915, INTEL_TIGERLAKE) {
            revids = &TGL_REVIDS;
        } else if IS_JASPERLAKE(i915) || IS_ELKHARTLAKE(i915) {
            revids = &JSL_EHL_REVIDS;
        } else if IS_ICELAKE(i915) {
            revids = &ICL_REVIDS;
        } else if IS_GEMINILAKE(i915) {
            revids = &GLK_REVIDS;
        } else if IS_BROXTON(i915) {
            revids = &BXT_REVIDS;
        } else if IS_KABYLAKE(i915) {
            revids = &KBL_REVIDS;
        } else if IS_SKYLAKE(i915) {
            revids = &SKL_REVIDS;
        } else {
            // Not using the stepping scheme for the platform yet.
            return;
        }

        let size = revids.len();
        let idx = revid as usize;
        if idx < size && revids[idx] != STEP_NONE {
            step = (revids[idx], revids[idx]);
        } else {
            drm_warn!(&(*i915).drm, "Unknown revid 0x%02x\n", revid);

            // If we hit a gap in the revid array, use the information for the
            // next revid. This may be wrong in all sorts of ways, especially if
            // the steppings in the array are not monotonically increasing, but
            // it's better than defaulting to 0.
            let mut next = idx;
            while next < size && revids[next] == STEP_NONE {
                next += 1;
            }

            if next < size {
                drm_dbg!(&(*i915).drm, "Using steppings for revid 0x%02x\n", next as u32);
                step = (revids[next], revids[next]);
            } else {
                drm_dbg!(&(*i915).drm, "Using future steppings\n");
                step = (STEP_FUTURE, 0);
            }
        }

        if drm_WARN_ON!(&(*i915).drm, step.0 == STEP_NONE) {
            return;
        }

        (*i915).runtime.step.graphics_step = step.0;
        (*i915).runtime.step.media_step = step.1;
    }
}
