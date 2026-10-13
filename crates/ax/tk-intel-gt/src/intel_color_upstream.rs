// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// Source: drivers/gpu/drm/i915/display/intel_color.c (Linux v7.2.3).
//
// This target port selects the ADL-P/display-13 TGL color pipeline. Legacy
// i915 hook tables are deliberately not selected.
#![allow(non_snake_case, dead_code, unsafe_code, clippy::missing_safety_doc)]

use core::{ffi::c_void, mem::size_of};

use tk_drm_upstream::{
    drm_device_types::DrmDevice,
    drm_property_types::DrmPropertyBlob,
    uapi_drm_mode_types::{DrmColorCtm, DrmColorCtm3x4, DrmColorLut, DrmColorLut32},
};

use crate::{
    intel_color_regs_upstream::*,
    intel_de_upstream::{intel_de_read_fw, intel_de_write_dsb, intel_de_write_fw},
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_limits_types_upstream::Pipe,
    intel_display_reg_defs_upstream::{DISPLAY_INFO, DISPLAY_VER},
    intel_display_types_upstream::{
        INTEL_OUTPUT_FORMAT_RGB, IntelAtomicState, IntelCrtc, IntelCrtcState, IntelCscMatrix,
        IntelPlaneState,
    },
    intel_workarounds_types_upstream::I915RegT,
    linux_i915_private::DrmI915Private,
    skl_universal_plane_regs_upstream::*,
};

const CTM_COEFF_SIGN: u64 = 1 << 63;
const CTM_COEFF_1_0: u64 = 1 << 32;
const CTM_COEFF_2_0: u64 = CTM_COEFF_1_0 << 1;
const CTM_COEFF_4_0: u64 = CTM_COEFF_2_0 << 1;
const CTM_COEFF_0_5: u64 = CTM_COEFF_1_0 >> 1;
const CTM_COEFF_0_25: u64 = CTM_COEFF_0_5 >> 1;
const CTM_COEFF_0_125: u64 = CTM_COEFF_0_25 >> 1;
const LEGACY_LUT_LENGTH: usize = 256;
const ILK_CSC_COEFF_1_0: u16 = 0x7800;
const ILK_CSC_COEFF_LIMITED_RANGE: u16 = (235 - 16) << (12 - 8);
const ILK_CSC_POSTOFF_LIMITED_RANGE: u16 = 16 << (12 - 8);
const DISPLAY13_DEGAMMA_SIZE: usize = 131;
const DISPLAY13_GAMMA_SIZE: usize = 1024;

#[repr(C)]
struct IntelColorFuncs {
    color_check: Option<unsafe extern "C" fn(*mut IntelAtomicState, *mut IntelCrtc) -> i32>,
    color_commit_noarm: Option<unsafe extern "C" fn(*mut c_void, *const IntelCrtcState)>,
    color_commit_arm: Option<unsafe extern "C" fn(*mut c_void, *const IntelCrtcState)>,
    color_post_update: Option<unsafe extern "C" fn(*const IntelCrtcState)>,
    load_luts: Option<unsafe extern "C" fn(*const IntelCrtcState)>,
    read_luts: Option<unsafe extern "C" fn(*mut IntelCrtcState)>,
    lut_equal: Option<
        unsafe extern "C" fn(
            *const IntelCrtcState,
            *const DrmPropertyBlob,
            *const DrmPropertyBlob,
            bool,
        ) -> bool,
    >,
    read_csc: Option<unsafe extern "C" fn(*mut IntelCrtcState)>,
    get_config: Option<unsafe extern "C" fn(*mut IntelCrtcState)>,
    load_plane_csc_matrix: Option<unsafe extern "C" fn(*mut c_void, *const IntelPlaneState)>,
    load_plane_luts: Option<unsafe extern "C" fn(*mut c_void, *const IntelPlaneState)>,
}

static TGL_COLOR_FUNCS: IntelColorFuncs = IntelColorFuncs {
    color_check: Some(icl_color_check),
    color_commit_noarm: Some(icl_color_commit_noarm),
    color_commit_arm: Some(icl_color_commit_arm),
    color_post_update: None,
    load_luts: Some(icl_load_luts),
    read_luts: Some(icl_read_luts),
    lut_equal: Some(icl_lut_equal),
    read_csc: Some(icl_read_csc),
    get_config: Some(skl_get_config),
    load_plane_csc_matrix: Some(xelpd_load_plane_csc_matrix),
    load_plane_luts: Some(xelpd_plane_load_luts),
};

unsafe extern "C" {
    fn intel_atomic_get_new_crtc_state(
        state: *mut IntelAtomicState,
        crtc: *mut IntelCrtc,
    ) -> *mut IntelCrtcState;
    fn intel_atomic_get_old_crtc_state(
        state: *mut IntelAtomicState,
        crtc: *mut IntelCrtc,
    ) -> *mut IntelCrtcState;
    fn intel_dsb_prepare(
        state: *mut IntelAtomicState,
        crtc: *mut IntelCrtc,
        id: u32,
        size: u32,
    ) -> *mut c_void;
    fn intel_dsb_finish(dsb: *mut c_void);
    fn intel_dsb_gosub_finish(dsb: *mut c_void);
    fn intel_dsb_cleanup(dsb: *mut c_void);
    fn intel_dsb_wait(dsb: *mut c_void);
    fn intel_dsb_reg_write_indexed(dsb: *mut c_void, reg: I915RegT, val: u32);
    fn intel_dsb_wait_for_delayed_vblank(state: *mut IntelAtomicState, dsb: *mut c_void);
    fn intel_dsb_interrupt(dsb: *mut c_void);
    fn intel_vrr_send_push(dsb: *mut c_void, state: *const IntelCrtcState);
    fn intel_vrr_check_push_sent(dsb: *mut c_void, state: *const IntelCrtcState);
    fn icl_is_hdr_plane(display: *mut IntelDisplay, plane: u32) -> bool;
}

unsafe fn display_from_drm(drm: *mut DrmDevice) -> *mut IntelDisplay {
    if drm.is_null() || (*drm).dev_private.is_null() {
        return core::ptr::null_mut();
    }
    (*(*drm).dev_private.cast::<DrmI915Private>())
        .display
        .cast::<IntelDisplay>()
}
unsafe fn display_from_crtc(crtc: *mut IntelCrtc) -> *mut IntelDisplay {
    if crtc.is_null() {
        return core::ptr::null_mut();
    }
    display_from_drm((*crtc).base.dev)
}
unsafe fn crtc_from_state(state: *const IntelCrtcState) -> *mut IntelCrtc {
    if state.is_null() || (*state).uapi.crtc.is_null() {
        core::ptr::null_mut()
    } else {
        (*state).uapi.crtc.cast::<IntelCrtc>()
    }
}
unsafe fn display_from_state(state: *const IntelCrtcState) -> *mut IntelDisplay {
    display_from_crtc(crtc_from_state(state))
}

unsafe fn lut_len(blob: *const DrmPropertyBlob) -> Option<usize> {
    if blob.is_null() || (*blob).length % size_of::<DrmColorLut>() != 0 {
        return None;
    }
    Some((*blob).length / size_of::<DrmColorLut>())
}
unsafe fn lut_is_legacy(blob: *const DrmPropertyBlob) -> bool {
    lut_len(blob) == Some(LEGACY_LUT_LENGTH)
}
unsafe fn lut32_len(blob: *const DrmPropertyBlob) -> Option<usize> {
    if blob.is_null() || (*blob).length % size_of::<DrmColorLut32>() != 0 {
        return None;
    }
    Some((*blob).length / size_of::<DrmColorLut32>())
}
unsafe fn lut_blob_has_size(blob: *const DrmPropertyBlob, expected: usize) -> bool {
    blob.is_null() || lut_len(blob) == Some(expected)
}
#[inline]
fn field_get(mask: u32, value: u32) -> u32 {
    (value & mask) >> mask.trailing_zeros()
}
#[inline]
fn field_prep(mask: u32, value: u32) -> u32 {
    (value << mask.trailing_zeros()) & mask
}
#[inline]
fn lut_extract(value: u16, precision: u32) -> u32 {
    (value as u32) >> (16 - precision)
}
#[inline]
fn lut32_extract(value: u32, precision: u32) -> u32 {
    value >> (32 - precision)
}

fn lut_pack(value: u32, precision: u32) -> u16 {
    if precision == 0 || precision > 32 {
        return 0;
    }
    let max = (1u64 << precision) - 1;
    (((value as u64 * u16::MAX as u64) + max / 2) / max).min(u16::MAX as u64) as u16
}
#[inline]
fn ctm_abs(coeff: u64) -> u64 {
    coeff & (CTM_COEFF_SIGN - 1)
}
#[inline]
fn ctm_negative(coeff: u64) -> bool {
    coeff & CTM_COEFF_SIGN != 0
}

fn ilk_csc_coeff_fp(abs_coeff: u64, fractional_bits: u32) -> u16 {
    let shift = 32 - fractional_bits - 3;
    ((((abs_coeff >> shift).saturating_add(4)).min(0xfff)) as u16) & 0xff8
}
fn ctm_to_ilk_coeff(coeff: u64) -> u16 {
    let abs_coeff = ctm_abs(coeff).min(CTM_COEFF_4_0 - 1);
    let (exponent, fraction) = if abs_coeff < CTM_COEFF_0_125 {
        (3, ilk_csc_coeff_fp(abs_coeff, 12))
    } else if abs_coeff < CTM_COEFF_0_25 {
        (2, ilk_csc_coeff_fp(abs_coeff, 11))
    } else if abs_coeff < CTM_COEFF_0_5 {
        (1, ilk_csc_coeff_fp(abs_coeff, 10))
    } else if abs_coeff < CTM_COEFF_1_0 {
        (0, ilk_csc_coeff_fp(abs_coeff, 9))
    } else if abs_coeff < CTM_COEFF_2_0 {
        (7, ilk_csc_coeff_fp(abs_coeff, 8))
    } else {
        (6, ilk_csc_coeff_fp(abs_coeff, 7))
    };
    (if ctm_negative(coeff) { 1 << 15 } else { 0 }) | (exponent << 12) | fraction
}
fn ctm_to_twos_complement(coeff: u64, integer_bits: u32, fractional_bits: u32) -> u16 {
    let mut value = (ctm_abs(coeff) >> (32 - fractional_bits - 1)) as i64;
    value = (value + 1) >> 1;
    if ctm_negative(coeff) {
        value = -value;
    }
    let signed_bits = integer_bits.max(1) + fractional_bits;
    let min = -(1i64 << (signed_bits - 1));
    let max = (1i64 << (signed_bits - 1)) - 1;
    (value.clamp(min, max) as u64 & ((1u64 << signed_bits) - 1)) as u16
}

fn ilk_lut_10(color: &DrmColorLut) -> u32 {
    field_prep(PREC_PALETTE_10_RED_MASK, lut_extract(color.red, 10))
        | field_prep(PREC_PALETTE_10_GREEN_MASK, lut_extract(color.green, 10))
        | field_prep(PREC_PALETTE_10_BLUE_MASK, lut_extract(color.blue, 10))
}
fn ilk_lut_10_pack(entry: &mut DrmColorLut, value: u32) {
    entry.red = lut_pack(field_get(PREC_PALETTE_10_RED_MASK, value), 10);
    entry.green = lut_pack(field_get(PREC_PALETTE_10_GREEN_MASK, value), 10);
    entry.blue = lut_pack(field_get(PREC_PALETTE_10_BLUE_MASK, value), 10);
    entry.reserved = 0;
}
fn ilk_lut_12p4_ldw(color: &DrmColorLut) -> u32 {
    field_prep(PREC_PALETTE_12P4_RED_LDW_MASK, (color.red & 0x3f) as u32)
        | field_prep(
            PREC_PALETTE_12P4_GREEN_LDW_MASK,
            (color.green & 0x3f) as u32,
        )
        | field_prep(PREC_PALETTE_12P4_BLUE_LDW_MASK, (color.blue & 0x3f) as u32)
}
fn ilk_lut_12p4_udw(color: &DrmColorLut) -> u32 {
    field_prep(PREC_PALETTE_12P4_RED_UDW_MASK, (color.red >> 6) as u32)
        | field_prep(PREC_PALETTE_12P4_GREEN_UDW_MASK, (color.green >> 6) as u32)
        | field_prep(PREC_PALETTE_12P4_BLUE_UDW_MASK, (color.blue >> 6) as u32)
}
fn ilk_lut_12p4_pack(entry: &mut DrmColorLut, low: u32, high: u32) {
    entry.red = ((field_get(PREC_PALETTE_12P4_RED_UDW_MASK, high) << 6)
        | field_get(PREC_PALETTE_12P4_RED_LDW_MASK, low)) as u16;
    entry.green = ((field_get(PREC_PALETTE_12P4_GREEN_UDW_MASK, high) << 6)
        | field_get(PREC_PALETTE_12P4_GREEN_LDW_MASK, low)) as u16;
    entry.blue = ((field_get(PREC_PALETTE_12P4_BLUE_UDW_MASK, high) << 6)
        | field_get(PREC_PALETTE_12P4_BLUE_LDW_MASK, low)) as u16;
    entry.reserved = 0;
}
fn lut_entries_equal(a: &DrmColorLut, b: &DrmColorLut, precision: u32) -> bool {
    let mask = if precision >= 16 {
        0
    } else if precision == 0 {
        u16::MAX
    } else {
        (1u16 << (16 - precision)) - 1
    };
    a.red & !mask == b.red & !mask
        && a.green & !mask == b.green & !mask
        && a.blue & !mask == b.blue & !mask
}
unsafe fn intel_lut_equal(
    a: *const DrmPropertyBlob,
    b: *const DrmPropertyBlob,
    limit: usize,
    precision: u32,
) -> bool {
    if a.is_null() != b.is_null() {
        return false;
    }
    if a.is_null() {
        return true;
    }
    if precision == 0 {
        return false;
    }
    if a == b {
        return true;
    }
    let (Some(alen), Some(blen)) = (lut_len(a), lut_len(b)) else {
        return false;
    };
    if alen != blen || (limit != 0 && alen < limit) {
        return false;
    }
    let ap = (*a).data.cast::<DrmColorLut>();
    let bp = (*b).data.cast::<DrmColorLut>();
    (0..if limit == 0 { alen } else { limit })
        .all(|i| lut_entries_equal(&*ap.add(i), &*bp.add(i), precision))
}

fn csc_clear(csc: &mut IntelCscMatrix) {
    csc.coeff = [0; 9];
    csc.preoff = [0; 3];
    csc.postoff = [0; 3];
}
fn csc_limited_range() -> IntelCscMatrix {
    IntelCscMatrix {
        coeff: [
            ILK_CSC_COEFF_LIMITED_RANGE,
            0,
            0,
            0,
            ILK_CSC_COEFF_LIMITED_RANGE,
            0,
            0,
            0,
            ILK_CSC_COEFF_LIMITED_RANGE,
        ],
        preoff: [0; 3],
        postoff: [ILK_CSC_POSTOFF_LIMITED_RANGE; 3],
    }
}
fn csc_rgb_to_ycbcr() -> IntelCscMatrix {
    IntelCscMatrix {
        coeff: [
            0x1e08, 0x9cc0, 0xb528, 0x2ba8, 0x09d8, 0x37e8, 0xbce8, 0x9ad8, 0x1e08,
        ],
        preoff: [0; 3],
        postoff: [0x0800, 0x0100, 0x0800],
    }
}
unsafe fn csc_copy_for_display(
    display: *mut IntelDisplay,
    dst: *mut IntelCscMatrix,
    src: IntelCscMatrix,
) {
    *dst = src;
    if DISPLAY_VER(display) < 7 {
        (*dst).postoff = [0; 3];
    }
}
unsafe fn csc_from_ctm(state: *const IntelCrtcState, csc: *mut IntelCscMatrix) {
    let matrix = (*(*state).hw.ctm).data.cast::<DrmColorCtm>();
    for i in 0..9 {
        (*csc).coeff[i] = ctm_to_ilk_coeff((*matrix).matrix[i]);
    }
    (*csc).preoff = [0; 3];
    (*csc).postoff = [0; 3];
}
unsafe fn icl_assign_csc(state: *mut IntelCrtcState) {
    let display = display_from_state(state);
    if !(*state).hw.ctm.is_null() {
        csc_from_ctm(state, core::ptr::addr_of_mut!((*state).csc));
    } else {
        csc_clear(&mut (*state).csc);
    }
    if (*state).output_format != INTEL_OUTPUT_FORMAT_RGB {
        csc_copy_for_display(
            display,
            core::ptr::addr_of_mut!((*state).output_csc),
            csc_rgb_to_ycbcr(),
        );
    } else if (*state).limited_color_range {
        csc_copy_for_display(
            display,
            core::ptr::addr_of_mut!((*state).output_csc),
            csc_limited_range(),
        );
    } else {
        csc_clear(&mut (*state).output_csc);
    }
}

unsafe fn check_luts(state: *const IntelCrtcState) -> i32 {
    let display = display_from_state(state);
    if display.is_null() {
        return -crate::linux_config::EINVAL;
    }
    let info = DISPLAY_INFO(display);
    let gamma = (*state).hw.gamma_lut;
    let degamma = (*state).hw.degamma_lut;
    let gamma_len = if lut_is_legacy(gamma) {
        LEGACY_LUT_LENGTH
    } else {
        (*info).color.gamma_lut_size as usize
    };
    let degamma_len = (*info).color.degamma_lut_size as usize;
    if (*state).c8_planes != 0 && !lut_is_legacy(gamma) {
        return -crate::linux_config::EINVAL;
    }
    if !lut_blob_has_size(gamma, gamma_len) || !lut_blob_has_size(degamma, degamma_len) {
        return -crate::linux_config::EINVAL;
    }
    if !gamma.is_null() {
        let tests = if lut_is_legacy(gamma) {
            0
        } else {
            (*info).color.gamma_lut_tests
        };
        if tk_drm_upstream::drm_color_mgmt_upstream::drm_color_lut_check(gamma, tests) != 0 {
            return -crate::linux_config::EINVAL;
        }
    }
    if !degamma.is_null()
        && tk_drm_upstream::drm_color_mgmt_upstream::drm_color_lut_check(
            degamma,
            (*info).color.degamma_lut_tests,
        ) != 0
    {
        return -crate::linux_config::EINVAL;
    }
    0
}

unsafe fn icl_gamma_mode(state: *const IntelCrtcState) -> u32 {
    let display = display_from_state(state);
    let gamma = (*state).hw.gamma_lut;
    let mut mode = 0;
    if !(*state).hw.degamma_lut.is_null() {
        mode |= PRE_CSC_GAMMA_ENABLE;
    }
    if !gamma.is_null() && (*state).c8_planes == 0 {
        mode |= POST_CSC_GAMMA_ENABLE;
    }
    if gamma.is_null() || lut_is_legacy(gamma) {
        mode |= GAMMA_MODE_MODE_8BIT;
    } else if DISPLAY_VER(display) >= 13 {
        mode |= GAMMA_MODE_MODE_10BIT;
    } else {
        mode |= GAMMA_MODE_MODE_12BIT_MULTI_SEG;
    }
    mode
}

unsafe fn icl_csc_mode(state: *const IntelCrtcState) -> u32 {
    (if !(*state).hw.ctm.is_null() {
        ICL_CSC_ENABLE
    } else {
        0
    }) | if (*state).output_format != INTEL_OUTPUT_FORMAT_RGB || (*state).limited_color_range {
        ICL_OUTPUT_CSC_ENABLE
    } else {
        0
    }
}

unsafe extern "C" fn icl_color_check(state: *mut IntelAtomicState, crtc: *mut IntelCrtc) -> i32 {
    if state.is_null() || crtc.is_null() {
        return -crate::linux_config::EINVAL;
    }
    let new = intel_atomic_get_new_crtc_state(state, crtc);
    if new.is_null() || crate::linux_config::IS_ERR(new) {
        return if crate::linux_config::IS_ERR(new) {
            crate::linux_config::PTR_ERR(new)
        } else {
            -crate::linux_config::EINVAL
        };
    }
    let ret = check_luts(new);
    if ret != 0 {
        return ret;
    }
    (*new).gamma_mode = icl_gamma_mode(new);
    (*new)._anon_3940_83.csc_mode = icl_csc_mode(new);
    (*new).gamma_enable = ((*new).gamma_mode & POST_CSC_GAMMA_ENABLE) != 0;
    (*new).csc_enable = ((*new)._anon_3940_83.csc_mode & ICL_CSC_ENABLE) != 0;
    (*new).preload_luts = false;
    let _ = tk_drm_upstream::drm_property_upstream::drm_property_replace_blob(
        core::ptr::addr_of_mut!((*new).pre_csc_lut),
        (*new).hw.degamma_lut,
    );
    let _ = tk_drm_upstream::drm_property_upstream::drm_property_replace_blob(
        core::ptr::addr_of_mut!((*new).post_csc_lut),
        (*new).hw.gamma_lut,
    );
    icl_assign_csc(new);
    0
}

unsafe fn write_pipe_csc(
    display: *mut IntelDisplay,
    dsb: *mut c_void,
    pipe: u32,
    csc: &IntelCscMatrix,
) {
    intel_de_write_dsb(display, dsb, PIPE_CSC_PREOFF_HI(pipe), csc.preoff[0] as u32);
    intel_de_write_dsb(display, dsb, PIPE_CSC_PREOFF_ME(pipe), csc.preoff[1] as u32);
    intel_de_write_dsb(display, dsb, PIPE_CSC_PREOFF_LO(pipe), csc.preoff[2] as u32);
    intel_de_write_dsb(
        display,
        dsb,
        PIPE_CSC_COEFF_RY_GY(pipe),
        (csc.coeff[0] as u32) << 16 | csc.coeff[1] as u32,
    );
    intel_de_write_dsb(
        display,
        dsb,
        PIPE_CSC_COEFF_BY(pipe),
        (csc.coeff[2] as u32) << 16,
    );
    intel_de_write_dsb(
        display,
        dsb,
        PIPE_CSC_COEFF_RU_GU(pipe),
        (csc.coeff[3] as u32) << 16 | csc.coeff[4] as u32,
    );
    intel_de_write_dsb(
        display,
        dsb,
        PIPE_CSC_COEFF_BU(pipe),
        (csc.coeff[5] as u32) << 16,
    );
    intel_de_write_dsb(
        display,
        dsb,
        PIPE_CSC_COEFF_RV_GV(pipe),
        (csc.coeff[6] as u32) << 16 | csc.coeff[7] as u32,
    );
    intel_de_write_dsb(
        display,
        dsb,
        PIPE_CSC_COEFF_BV(pipe),
        (csc.coeff[8] as u32) << 16,
    );
    intel_de_write_dsb(
        display,
        dsb,
        PIPE_CSC_POSTOFF_HI(pipe),
        csc.postoff[0] as u32,
    );
    intel_de_write_dsb(
        display,
        dsb,
        PIPE_CSC_POSTOFF_ME(pipe),
        csc.postoff[1] as u32,
    );
    intel_de_write_dsb(
        display,
        dsb,
        PIPE_CSC_POSTOFF_LO(pipe),
        csc.postoff[2] as u32,
    );
}

unsafe fn write_output_csc(
    display: *mut IntelDisplay,
    dsb: *mut c_void,
    pipe: u32,
    csc: &IntelCscMatrix,
) {
    intel_de_write_dsb(
        display,
        dsb,
        PIPE_CSC_OUTPUT_PREOFF_HI(pipe),
        csc.preoff[0] as u32,
    );
    intel_de_write_dsb(
        display,
        dsb,
        PIPE_CSC_OUTPUT_PREOFF_ME(pipe),
        csc.preoff[1] as u32,
    );
    intel_de_write_dsb(
        display,
        dsb,
        PIPE_CSC_OUTPUT_PREOFF_LO(pipe),
        csc.preoff[2] as u32,
    );
    intel_de_write_dsb(
        display,
        dsb,
        PIPE_CSC_OUTPUT_COEFF_RY_GY(pipe),
        (csc.coeff[0] as u32) << 16 | csc.coeff[1] as u32,
    );
    intel_de_write_dsb(
        display,
        dsb,
        PIPE_CSC_OUTPUT_COEFF_BY(pipe),
        (csc.coeff[2] as u32) << 16,
    );
    intel_de_write_dsb(
        display,
        dsb,
        PIPE_CSC_OUTPUT_COEFF_RU_GU(pipe),
        (csc.coeff[3] as u32) << 16 | csc.coeff[4] as u32,
    );
    intel_de_write_dsb(
        display,
        dsb,
        PIPE_CSC_OUTPUT_COEFF_BU(pipe),
        (csc.coeff[5] as u32) << 16,
    );
    intel_de_write_dsb(
        display,
        dsb,
        PIPE_CSC_OUTPUT_COEFF_RV_GV(pipe),
        (csc.coeff[6] as u32) << 16 | csc.coeff[7] as u32,
    );
    intel_de_write_dsb(
        display,
        dsb,
        PIPE_CSC_OUTPUT_COEFF_BV(pipe),
        (csc.coeff[8] as u32) << 16,
    );
    intel_de_write_dsb(
        display,
        dsb,
        PIPE_CSC_OUTPUT_POSTOFF_HI(pipe),
        csc.postoff[0] as u32,
    );
    intel_de_write_dsb(
        display,
        dsb,
        PIPE_CSC_OUTPUT_POSTOFF_ME(pipe),
        csc.postoff[1] as u32,
    );
    intel_de_write_dsb(
        display,
        dsb,
        PIPE_CSC_OUTPUT_POSTOFF_LO(pipe),
        csc.postoff[2] as u32,
    );
}

unsafe fn icl_load_csc_matrix(dsb: *mut c_void, state: *const IntelCrtcState) {
    let crtc = crtc_from_state(state);
    let display = display_from_crtc(crtc);
    if crtc.is_null() || display.is_null() {
        return;
    }
    let pipe = (*crtc).pipe as u32;
    let mode = (*state)._anon_3940_83.csc_mode;
    if mode & ICL_CSC_ENABLE != 0 {
        write_pipe_csc(display, dsb, pipe, &(*state).csc);
    }
    if mode & ICL_OUTPUT_CSC_ENABLE != 0 {
        write_output_csc(display, dsb, pipe, &(*state).output_csc);
    }
}
unsafe extern "C" fn icl_color_commit_noarm(dsb: *mut c_void, state: *const IntelCrtcState) {
    icl_load_csc_matrix(dsb, state);
}
unsafe extern "C" fn icl_color_commit_arm(dsb: *mut c_void, state: *const IntelCrtcState) {
    let crtc = crtc_from_state(state);
    let display = display_from_crtc(crtc);
    if crtc.is_null() || display.is_null() {
        return;
    }
    let pipe = (*crtc).pipe as u32;
    intel_de_write_dsb(
        display,
        dsb,
        SKL_BOTTOM_COLOR(pipe),
        (*state).hw.background_color,
    );
    intel_de_write_dsb(display, dsb, GAMMA_MODE(pipe), (*state).gamma_mode);
    intel_de_write_dsb(
        display,
        dsb,
        PIPE_CSC_MODE(pipe),
        (*state)._anon_3940_83.csc_mode,
    );
}

fn run_modeset_color_program(load: impl FnOnce(), noarm: impl FnOnce(), arm: impl FnOnce()) {
    load();
    noarm();
    arm();
}

unsafe fn make_blob(display: *mut IntelDisplay, length: usize) -> *mut DrmPropertyBlob {
    let blob = tk_drm_upstream::drm_property_upstream::drm_property_create_blob(
        (*display).drm,
        length,
        core::ptr::null(),
    );
    if crate::linux_config::IS_ERR(blob) {
        core::ptr::null_mut()
    } else {
        blob
    }
}
unsafe fn replace_readback_blob(dst: *mut *mut DrmPropertyBlob, blob: *mut DrmPropertyBlob) {
    if blob.is_null() {
        return;
    }
    let _ = tk_drm_upstream::drm_property_upstream::drm_property_replace_blob(dst, blob);
    tk_drm_upstream::drm_property_upstream::drm_property_blob_put(blob);
}

unsafe fn lut_write(state: *const IntelCrtcState, reg: I915RegT, value: u32) {
    let display = display_from_state(state);
    if display.is_null() {
        return;
    }
    if !(*state).dsb_color.is_null() {
        intel_de_write_dsb(display, (*state).dsb_color, reg, value);
    } else {
        intel_de_write_fw(display, reg, value);
    }
}
unsafe fn lut_write_indexed(state: *const IntelCrtcState, reg: I915RegT, value: u32) {
    let display = display_from_state(state);
    if display.is_null() {
        return;
    }
    if !(*state).dsb_color.is_null() {
        intel_dsb_reg_write_indexed((*state).dsb_color, reg, value);
    } else {
        intel_de_write_fw(display, reg, value);
    }
}
unsafe fn ilk_load_lut_8(state: *const IntelCrtcState, blob: *const DrmPropertyBlob) {
    if blob.is_null() {
        return;
    }
    let crtc = crtc_from_state(state);
    if crtc.is_null() {
        return;
    }
    let pipe = (*crtc).pipe as u32;
    let lut = (*blob).data.cast::<DrmColorLut>();
    for i in 0..LEGACY_LUT_LENGTH {
        let c = &*lut.add(i);
        let value = field_prep(PALETTE_RED_MASK, lut_extract(c.red, 8))
            | field_prep(PALETTE_GREEN_MASK, lut_extract(c.green, 8))
            | field_prep(PALETTE_BLUE_MASK, lut_extract(c.blue, 8));
        lut_write(state, LGC_PALETTE(pipe, i as u32), value);
        // The DSB command stream must use non-posted writes or duplicate each
        // write to avoid palette anti-collision dropping an entry.
        if !(*state).dsb_color.is_null() {
            lut_write(state, LGC_PALETTE(pipe, i as u32), value);
        }
    }
}
unsafe fn bdw_load_lut_10(state: *const IntelCrtcState, blob: *const DrmPropertyBlob, index: u32) {
    if blob.is_null() {
        return;
    }
    let crtc = crtc_from_state(state);
    if crtc.is_null() {
        return;
    }
    let pipe = (*crtc).pipe as u32;
    let lut = (*blob).data.cast::<DrmColorLut>();
    lut_write(state, PREC_PAL_INDEX(pipe), index);
    lut_write(state, PREC_PAL_INDEX(pipe), PAL_PREC_AUTO_INCREMENT | index);
    let Some(len) = lut_len(blob) else {
        return;
    };
    for i in 0..len {
        lut_write_indexed(state, PREC_PAL_DATA(pipe), ilk_lut_10(&*lut.add(i)));
    }
    lut_write(state, PREC_PAL_INDEX(pipe), PAL_PREC_INDEX_VALUE(0));
}
unsafe fn load_degamma_lut(state: *const IntelCrtcState, blob: *const DrmPropertyBlob) {
    let display = display_from_state(state);
    let crtc = crtc_from_state(state);
    if display.is_null() || crtc.is_null() || blob.is_null() {
        return;
    }
    let pipe = (*crtc).pipe as u32;
    let lut = (*blob).data.cast::<DrmColorLut>();
    let Some(len) = lut_len(blob) else {
        return;
    };
    // AUTO_INCREMENT ignores the index on GLK+; program index zero twice.
    lut_write(state, PRE_CSC_GAMC_INDEX(pipe), PRE_CSC_GAMC_INDEX_VALUE(0));
    lut_write(
        state,
        PRE_CSC_GAMC_INDEX(pipe),
        PRE_CSC_GAMC_AUTO_INCREMENT | PRE_CSC_GAMC_INDEX_VALUE(0),
    );
    for i in 0..len {
        // Display 13 PRE_CSC_GAMC_DATA has 16-bit precision and one shared
        // value for all channels; Linux UAPI requires equal channels here.
        lut_write_indexed(state, PRE_CSC_GAMC_DATA(pipe), (*lut.add(i)).green as u32);
    }
    let total = DISPLAY13_DEGAMMA_SIZE;
    for _ in len..total {
        lut_write_indexed(state, PRE_CSC_GAMC_DATA(pipe), 1 << 16);
    }
    lut_write(state, PRE_CSC_GAMC_INDEX(pipe), 0);
}
unsafe fn load_ext_gamma_max(state: *const IntelCrtcState) {
    let crtc = crtc_from_state(state);
    if crtc.is_null() {
        return;
    }
    let pipe = (*crtc).pipe as u32;
    for i in 0..3 {
        lut_write(state, PREC_PAL_EXT_GC_MAX(pipe, i), 1 << 16);
        lut_write(state, PREC_PAL_EXT2_GC_MAX(pipe, i), 1 << 16);
    }
}
unsafe extern "C" fn icl_load_luts(state: *const IntelCrtcState) {
    let pre = (*state).pre_csc_lut;
    let post = (*state).post_csc_lut;
    if !pre.is_null() {
        load_degamma_lut(state, pre);
    }
    match (*state).gamma_mode & GAMMA_MODE_MODE_MASK {
        GAMMA_MODE_MODE_8BIT => ilk_load_lut_8(state, post),
        GAMMA_MODE_MODE_10BIT => {
            bdw_load_lut_10(state, post, PAL_PREC_INDEX_VALUE(0));
            load_ext_gamma_max(state);
        }
        GAMMA_MODE_MODE_12BIT_MULTI_SEG => {
            // The 12-bit multi-segment path is retained for ICL/TGL shared
            // helpers; display 13 selects the 10-bit mode above.
            icl_program_gamma_superfine(state);
            icl_program_gamma_multisegment(state);
            load_ext_gamma_max(state);
        }
        _ => {}
    }
}
unsafe fn icl_program_gamma_superfine(state: *const IntelCrtcState) {
    let crtc = crtc_from_state(state);
    let blob = (*state).post_csc_lut;
    if crtc.is_null() || blob.is_null() {
        return;
    }
    let pipe = (*crtc).pipe as u32;
    let lut = (*blob).data.cast::<DrmColorLut>();
    lut_write(
        state,
        PREC_PAL_MULTI_SEG_INDEX(pipe),
        PAL_PREC_MULTI_SEG_INDEX_VALUE(0),
    );
    lut_write(
        state,
        PREC_PAL_MULTI_SEG_INDEX(pipe),
        PAL_PREC_MULTI_SEG_AUTO_INCREMENT | PAL_PREC_MULTI_SEG_INDEX_VALUE(0),
    );
    for i in 0..9 {
        let entry = &*lut.add(i);
        lut_write_indexed(
            state,
            PREC_PAL_MULTI_SEG_DATA(pipe),
            ilk_lut_12p4_ldw(entry),
        );
        lut_write_indexed(
            state,
            PREC_PAL_MULTI_SEG_DATA(pipe),
            ilk_lut_12p4_udw(entry),
        );
    }
    lut_write(
        state,
        PREC_PAL_MULTI_SEG_INDEX(pipe),
        PAL_PREC_MULTI_SEG_INDEX_VALUE(0),
    );
}
unsafe fn icl_program_gamma_multisegment(state: *const IntelCrtcState) {
    let crtc = crtc_from_state(state);
    let blob = (*state).post_csc_lut;
    if crtc.is_null() || blob.is_null() {
        return;
    }
    let pipe = (*crtc).pipe as u32;
    let lut = (*blob).data.cast::<DrmColorLut>();
    lut_write(state, PREC_PAL_INDEX(pipe), PAL_PREC_INDEX_VALUE(0));
    lut_write(
        state,
        PREC_PAL_INDEX(pipe),
        PAL_PREC_AUTO_INCREMENT | PAL_PREC_INDEX_VALUE(0),
    );
    for i in 1..257 {
        let entry = &*lut.add(i * 8);
        lut_write_indexed(state, PREC_PAL_DATA(pipe), ilk_lut_12p4_ldw(entry));
        lut_write_indexed(state, PREC_PAL_DATA(pipe), ilk_lut_12p4_udw(entry));
    }
    for i in 0..256 {
        let entry = &*lut.add(i * 8 * 128);
        lut_write_indexed(state, PREC_PAL_DATA(pipe), ilk_lut_12p4_ldw(entry));
        lut_write_indexed(state, PREC_PAL_DATA(pipe), ilk_lut_12p4_udw(entry));
    }
    lut_write(state, PREC_PAL_INDEX(pipe), PAL_PREC_INDEX_VALUE(0));
    let last = &*lut.add(256 * 8 * 128);
    for (i, value) in [last.red, last.green, last.blue].into_iter().enumerate() {
        lut_write(state, PREC_PAL_GC_MAX(pipe, i as u32), value as u32);
    }
}
unsafe fn create_readback_blob(display: *mut IntelDisplay, len: usize) -> *mut DrmPropertyBlob {
    make_blob(display, len * size_of::<DrmColorLut>())
}
unsafe fn read_lut_8(crtc: *mut IntelCrtc) -> *mut DrmPropertyBlob {
    let display = display_from_crtc(crtc);
    if display.is_null() {
        return core::ptr::null_mut();
    }
    let blob = create_readback_blob(display, LEGACY_LUT_LENGTH);
    if blob.is_null() {
        return blob;
    }
    let lut = (*blob).data.cast::<DrmColorLut>();
    let pipe = (*crtc).pipe as u32;
    for i in 0..LEGACY_LUT_LENGTH {
        let value = intel_de_read_fw(display, LGC_PALETTE(pipe, i as u32));
        (*lut.add(i)).red = lut_pack(field_get(PALETTE_RED_MASK, value), 8);
        (*lut.add(i)).green = lut_pack(field_get(PALETTE_GREEN_MASK, value), 8);
        (*lut.add(i)).blue = lut_pack(field_get(PALETTE_BLUE_MASK, value), 8);
        (*lut.add(i)).reserved = 0;
    }
    blob
}
unsafe fn read_lut_10(crtc: *mut IntelCrtc) -> *mut DrmPropertyBlob {
    let display = display_from_crtc(crtc);
    if display.is_null() {
        return core::ptr::null_mut();
    }
    let len = (*DISPLAY_INFO(display)).color.gamma_lut_size as usize;
    let blob = create_readback_blob(display, len);
    if blob.is_null() {
        return blob;
    }
    let lut = (*blob).data.cast::<DrmColorLut>();
    let pipe = (*crtc).pipe as u32;
    intel_de_write_fw(display, PREC_PAL_INDEX(pipe), PAL_PREC_INDEX_VALUE(0));
    intel_de_write_fw(
        display,
        PREC_PAL_INDEX(pipe),
        PAL_PREC_AUTO_INCREMENT | PAL_PREC_INDEX_VALUE(0),
    );
    for i in 0..len {
        ilk_lut_10_pack(
            &mut *lut.add(i),
            intel_de_read_fw(display, PREC_PAL_DATA(pipe)),
        );
    }
    intel_de_write_fw(display, PREC_PAL_INDEX(pipe), PAL_PREC_INDEX_VALUE(0));
    blob
}
unsafe fn read_degamma_lut(crtc: *mut IntelCrtc) -> *mut DrmPropertyBlob {
    let display = display_from_crtc(crtc);
    if display.is_null() {
        return core::ptr::null_mut();
    }
    let len = (*DISPLAY_INFO(display)).color.degamma_lut_size as usize;
    let blob = create_readback_blob(display, len);
    if blob.is_null() {
        return blob;
    }
    let lut = (*blob).data.cast::<DrmColorLut>();
    let pipe = (*crtc).pipe as u32;
    intel_de_write_fw(
        display,
        PRE_CSC_GAMC_INDEX(pipe),
        PRE_CSC_GAMC_INDEX_VALUE(0),
    );
    intel_de_write_fw(
        display,
        PRE_CSC_GAMC_INDEX(pipe),
        PRE_CSC_GAMC_AUTO_INCREMENT | PRE_CSC_GAMC_INDEX_VALUE(0),
    );
    for i in 0..len {
        let value = intel_de_read_fw(display, PRE_CSC_GAMC_DATA(pipe));
        let value = value.min(0xffff) as u16;
        (*lut.add(i)).red = value;
        (*lut.add(i)).green = value;
        (*lut.add(i)).blue = value;
        (*lut.add(i)).reserved = 0;
    }
    intel_de_write_fw(display, PRE_CSC_GAMC_INDEX(pipe), 0);
    blob
}
unsafe extern "C" fn icl_read_luts(state: *mut IntelCrtcState) {
    let crtc = crtc_from_state(state);
    if crtc.is_null() {
        return;
    }
    let mode = (*state).gamma_mode;
    if mode & PRE_CSC_GAMMA_ENABLE != 0 {
        replace_readback_blob(
            core::ptr::addr_of_mut!((*state).pre_csc_lut),
            read_degamma_lut(crtc),
        );
    }
    if (*state).c8_planes != 0 || mode & POST_CSC_GAMMA_ENABLE != 0 {
        let lut = match mode & GAMMA_MODE_MODE_MASK {
            GAMMA_MODE_MODE_8BIT => read_lut_8(crtc),
            GAMMA_MODE_MODE_10BIT => read_lut_10(crtc),
            _ => core::ptr::null_mut(),
        };
        replace_readback_blob(core::ptr::addr_of_mut!((*state).post_csc_lut), lut);
    }
}
unsafe fn read_pipe_csc(display: *mut IntelDisplay, pipe: u32, csc: &mut IntelCscMatrix) {
    csc.preoff[0] = intel_de_read_fw(display, PIPE_CSC_PREOFF_HI(pipe)) as u16;
    csc.preoff[1] = intel_de_read_fw(display, PIPE_CSC_PREOFF_ME(pipe)) as u16;
    csc.preoff[2] = intel_de_read_fw(display, PIPE_CSC_PREOFF_LO(pipe)) as u16;
    let mut v = intel_de_read_fw(display, PIPE_CSC_COEFF_RY_GY(pipe));
    csc.coeff[0] = (v >> 16) as u16;
    csc.coeff[1] = v as u16;
    v = intel_de_read_fw(display, PIPE_CSC_COEFF_BY(pipe));
    csc.coeff[2] = (v >> 16) as u16;
    v = intel_de_read_fw(display, PIPE_CSC_COEFF_RU_GU(pipe));
    csc.coeff[3] = (v >> 16) as u16;
    csc.coeff[4] = v as u16;
    v = intel_de_read_fw(display, PIPE_CSC_COEFF_BU(pipe));
    csc.coeff[5] = (v >> 16) as u16;
    v = intel_de_read_fw(display, PIPE_CSC_COEFF_RV_GV(pipe));
    csc.coeff[6] = (v >> 16) as u16;
    csc.coeff[7] = v as u16;
    v = intel_de_read_fw(display, PIPE_CSC_COEFF_BV(pipe));
    csc.coeff[8] = (v >> 16) as u16;
    csc.postoff[0] = intel_de_read_fw(display, PIPE_CSC_POSTOFF_HI(pipe)) as u16;
    csc.postoff[1] = intel_de_read_fw(display, PIPE_CSC_POSTOFF_ME(pipe)) as u16;
    csc.postoff[2] = intel_de_read_fw(display, PIPE_CSC_POSTOFF_LO(pipe)) as u16;
}
unsafe fn read_output_csc(display: *mut IntelDisplay, pipe: u32, csc: &mut IntelCscMatrix) {
    csc.preoff[0] = intel_de_read_fw(display, PIPE_CSC_OUTPUT_PREOFF_HI(pipe)) as u16;
    csc.preoff[1] = intel_de_read_fw(display, PIPE_CSC_OUTPUT_PREOFF_ME(pipe)) as u16;
    csc.preoff[2] = intel_de_read_fw(display, PIPE_CSC_OUTPUT_PREOFF_LO(pipe)) as u16;
    let mut v = intel_de_read_fw(display, PIPE_CSC_OUTPUT_COEFF_RY_GY(pipe));
    csc.coeff[0] = (v >> 16) as u16;
    csc.coeff[1] = v as u16;
    v = intel_de_read_fw(display, PIPE_CSC_OUTPUT_COEFF_BY(pipe));
    csc.coeff[2] = (v >> 16) as u16;
    v = intel_de_read_fw(display, PIPE_CSC_OUTPUT_COEFF_RU_GU(pipe));
    csc.coeff[3] = (v >> 16) as u16;
    csc.coeff[4] = v as u16;
    v = intel_de_read_fw(display, PIPE_CSC_OUTPUT_COEFF_BU(pipe));
    csc.coeff[5] = (v >> 16) as u16;
    v = intel_de_read_fw(display, PIPE_CSC_OUTPUT_COEFF_RV_GV(pipe));
    csc.coeff[6] = (v >> 16) as u16;
    csc.coeff[7] = v as u16;
    v = intel_de_read_fw(display, PIPE_CSC_OUTPUT_COEFF_BV(pipe));
    csc.coeff[8] = (v >> 16) as u16;
    csc.postoff[0] = intel_de_read_fw(display, PIPE_CSC_OUTPUT_POSTOFF_HI(pipe)) as u16;
    csc.postoff[1] = intel_de_read_fw(display, PIPE_CSC_OUTPUT_POSTOFF_ME(pipe)) as u16;
    csc.postoff[2] = intel_de_read_fw(display, PIPE_CSC_OUTPUT_POSTOFF_LO(pipe)) as u16;
}
unsafe extern "C" fn icl_read_csc(state: *mut IntelCrtcState) {
    let crtc = crtc_from_state(state);
    let display = display_from_crtc(crtc);
    if crtc.is_null() || display.is_null() {
        return;
    }
    let pipe = (*crtc).pipe as u32;
    let mode = (*state)._anon_3940_83.csc_mode;
    if mode & ICL_CSC_ENABLE != 0 {
        read_pipe_csc(display, pipe, &mut (*state).csc);
    }
    if mode & ICL_OUTPUT_CSC_ENABLE != 0 {
        read_output_csc(display, pipe, &mut (*state).output_csc);
    }
}
unsafe extern "C" fn skl_get_config(state: *mut IntelCrtcState) {
    let crtc = crtc_from_state(state);
    let display = display_from_crtc(crtc);
    if crtc.is_null() || display.is_null() {
        return;
    }
    let pipe = (*crtc).pipe as u32;
    (*state).gamma_mode = intel_de_read_fw(display, GAMMA_MODE(pipe));
    (*state)._anon_3940_83.csc_mode = intel_de_read_fw(display, PIPE_CSC_MODE(pipe));
    let color = intel_de_read_fw(display, SKL_BOTTOM_COLOR(pipe));
    if DISPLAY_VER(display) < 35 {
        (*state).gamma_enable = color & SKL_BOTTOM_COLOR_GAMMA_ENABLE != 0;
        (*state).csc_enable = color & SKL_BOTTOM_COLOR_CSC_ENABLE != 0;
    }
    (*state).hw.background_color = color & ((1 << 30) - 1);
}
unsafe extern "C" fn icl_lut_equal(
    state: *const IntelCrtcState,
    a: *const DrmPropertyBlob,
    b: *const DrmPropertyBlob,
    pre: bool,
) -> bool {
    if !pre && (*state).c8_planes != 0 {
        return true;
    }
    let mode = (*state).gamma_mode & GAMMA_MODE_MODE_MASK;
    if pre {
        let precision = if (*state).gamma_mode & PRE_CSC_GAMMA_ENABLE != 0 {
            16
        } else {
            0
        };
        return intel_lut_equal(a, b, 0, precision);
    }
    if (*state).gamma_mode & POST_CSC_GAMMA_ENABLE == 0 {
        return intel_lut_equal(a, b, 0, 0);
    }
    let precision = match mode {
        GAMMA_MODE_MODE_8BIT => 8,
        GAMMA_MODE_MODE_10BIT => 10,
        GAMMA_MODE_MODE_12BIT_MULTI_SEG => 16,
        _ => 0,
    };
    let check_len = if mode == GAMMA_MODE_MODE_12BIT_MULTI_SEG {
        9
    } else {
        0
    };
    intel_lut_equal(a, b, check_len, precision)
}

unsafe fn write_plane_csc(dsb: *mut c_void, state: *const IntelPlaneState) {
    let drm_plane = (*state).uapi.plane;
    if drm_plane.is_null() {
        return;
    }
    let display = display_from_drm((*drm_plane).dev);
    let plane = drm_plane.cast::<crate::intel_display_types_upstream::IntelPlane>();
    let blob = (*state).hw.ctm;
    if display.is_null() || blob.is_null() {
        return;
    }
    let plane_id = (*plane).id;
    if !icl_is_hdr_plane(display, plane_id) {
        return;
    }
    let pipe = (*plane).pipe as u32;
    let input = (*(*blob).data.cast::<DrmColorCtm3x4>()).matrix;
    let mut coeffs = [0u16; 9];
    let mut j = 0usize;
    for coeff in &mut coeffs {
        *coeff = ctm_to_ilk_coeff(input[j]);
        j += if (j + 2) % 4 == 0 { 2 } else { 1 };
    }
    let packed = [
        (coeffs[0] as u32) << 16 | coeffs[1] as u32,
        (coeffs[2] as u32) << 16,
        (coeffs[3] as u32) << 16 | coeffs[4] as u32,
        (coeffs[5] as u32) << 16,
        (coeffs[6] as u32) << 16 | coeffs[7] as u32,
        (coeffs[8] as u32) << 16,
    ];
    for (i, value) in packed.into_iter().enumerate() {
        intel_de_write_dsb(
            display,
            dsb,
            PLANE_CSC_COEFF(pipe, plane_id, i as u32),
            value,
        );
    }
    for i in 0..3 {
        intel_de_write_dsb(display, dsb, PLANE_CSC_PREOFF(pipe, plane_id, i), 0);
        intel_de_write_dsb(
            display,
            dsb,
            PLANE_CSC_POSTOFF(pipe, plane_id, i),
            ctm_to_twos_complement(input[3 + (i as usize) * 4], 0, 12) as u32,
        );
    }
}

unsafe fn write_plane_lut(dsb: *mut c_void, display: *mut IntelDisplay, reg: I915RegT, value: u32) {
    intel_de_write_dsb(display, dsb, reg, value);
}
unsafe fn xelpd_program_plane_pre_csc_lut(dsb: *mut c_void, state: *const IntelPlaneState) {
    let drm_plane = (*state).uapi.plane;
    if drm_plane.is_null() {
        return;
    }
    let display = display_from_drm((*drm_plane).dev);
    let plane = drm_plane.cast::<crate::intel_display_types_upstream::IntelPlane>();
    if display.is_null() || !icl_is_hdr_plane(display, (*plane).id) {
        return;
    }
    let pipe = (*plane).pipe as u32;
    let plane_id = (*plane).id;
    let blob = (*state).hw.degamma_lut;
    let lut = if blob.is_null() {
        core::ptr::null()
    } else {
        (*blob).data.cast::<DrmColorLut32>()
    };
    if !blob.is_null() && lut32_len(blob) != Some(128) {
        return;
    }
    write_plane_lut(
        dsb,
        display,
        PLANE_PRE_CSC_GAMC_INDEX_ENH(pipe, plane_id, 0),
        PLANE_PAL_PREC_AUTO_INCREMENT,
    );
    if !lut.is_null() {
        for i in 0..128 {
            write_plane_lut(
                dsb,
                display,
                PLANE_PRE_CSC_GAMC_DATA_ENH(pipe, plane_id, 0),
                lut32_extract((*lut.add(i)).green, 24),
            );
        }
    } else {
        for i in 0..128 {
            let value = (i as u64 * ((1u64 << 24) - 1) / 127) as u32;
            write_plane_lut(
                dsb,
                display,
                PLANE_PRE_CSC_GAMC_DATA_ENH(pipe, plane_id, 0),
                value,
            );
        }
    }
    // The final three entries are the max/clamp values for the extended range.
    for _ in 0..3 {
        write_plane_lut(
            dsb,
            display,
            PLANE_PRE_CSC_GAMC_DATA_ENH(pipe, plane_id, 0),
            1 << 24,
        );
    }
    write_plane_lut(
        dsb,
        display,
        PLANE_PRE_CSC_GAMC_INDEX_ENH(pipe, plane_id, 0),
        0,
    );
}
unsafe fn xelpd_program_plane_post_csc_lut(dsb: *mut c_void, state: *const IntelPlaneState) {
    let drm_plane = (*state).uapi.plane;
    if drm_plane.is_null() {
        return;
    }
    let display = display_from_drm((*drm_plane).dev);
    let plane = drm_plane.cast::<crate::intel_display_types_upstream::IntelPlane>();
    if display.is_null() || !icl_is_hdr_plane(display, (*plane).id) {
        return;
    }
    let pipe = (*plane).pipe as u32;
    let plane_id = (*plane).id;
    let blob = (*state).hw.gamma_lut;
    let lut = if blob.is_null() {
        core::ptr::null()
    } else {
        (*blob).data.cast::<DrmColorLut32>()
    };
    if !blob.is_null() && lut32_len(blob) != Some(32) {
        return;
    }
    write_plane_lut(
        dsb,
        display,
        PLANE_POST_CSC_GAMC_INDEX_ENH(pipe, plane_id, 0),
        PLANE_PAL_PREC_AUTO_INCREMENT,
    );
    write_plane_lut(
        dsb,
        display,
        PLANE_POST_CSC_GAMC_SEG0_INDEX_ENH(pipe, plane_id, 0),
        PLANE_PAL_PREC_AUTO_INCREMENT,
    );
    if !lut.is_null() {
        for i in 0..32 {
            write_plane_lut(
                dsb,
                display,
                PLANE_POST_CSC_GAMC_DATA_ENH(pipe, plane_id, 0),
                lut32_extract((*lut.add(i)).green, 24),
            );
        }
    } else {
        for i in 0..32 {
            let value = (i as u64 * ((1u64 << 24) - 1) / 31) as u32;
            write_plane_lut(
                dsb,
                display,
                PLANE_POST_CSC_GAMC_DATA_ENH(pipe, plane_id, 0),
                value,
            );
        }
    }
    for _ in 0..3 {
        write_plane_lut(
            dsb,
            display,
            PLANE_POST_CSC_GAMC_DATA_ENH(pipe, plane_id, 0),
            1 << 24,
        );
    }
    write_plane_lut(
        dsb,
        display,
        PLANE_POST_CSC_GAMC_INDEX_ENH(pipe, plane_id, 0),
        0,
    );
    write_plane_lut(
        dsb,
        display,
        PLANE_POST_CSC_GAMC_SEG0_INDEX_ENH(pipe, plane_id, 0),
        0,
    );
}
unsafe extern "C" fn xelpd_load_plane_csc_matrix(dsb: *mut c_void, state: *const IntelPlaneState) {
    if !state.is_null() && !(*state).hw.ctm.is_null() {
        write_plane_csc(dsb, state);
    }
}
unsafe extern "C" fn xelpd_plane_load_luts(dsb: *mut c_void, state: *const IntelPlaneState) {
    if (*state).hw.degamma_lut.is_null() && (*state).hw.gamma_lut.is_null() {
        return;
    }
    if !(*state).hw.degamma_lut.is_null() {
        xelpd_program_plane_pre_csc_lut(dsb, state);
    }
    if !(*state).hw.gamma_lut.is_null() {
        xelpd_program_plane_post_csc_lut(dsb, state);
    }
}

fn lut3d_pack10(color: &DrmColorLut32) -> u32 {
    field_prep(LUT_3D_DATA_RED_MASK, lut32_extract(color.red, 10))
        | field_prep(LUT_3D_DATA_GREEN_MASK, lut32_extract(color.green, 10))
        | field_prep(LUT_3D_DATA_BLUE_MASK, lut32_extract(color.blue, 10))
}
unsafe fn load_lut_3d(dsb: *mut c_void, crtc: *mut IntelCrtc, blob: *const DrmPropertyBlob) {
    let display = display_from_crtc(crtc);
    if display.is_null() || blob.is_null() {
        return;
    }
    let pipe = (*crtc).pipe as u32;
    let Some(len) = lut32_len(blob) else {
        return;
    };
    if dsb.is_null() && intel_de_read_fw(display, LUT_3D_CTL(pipe)) & LUT_3D_READY != 0 {
        return;
    }
    let lut = (*blob).data.cast::<DrmColorLut32>();
    intel_de_write_dsb(display, dsb, LUT_3D_INDEX(pipe), LUT_3D_AUTO_INCREMENT);
    for i in 0..len {
        intel_de_write_dsb(display, dsb, LUT_3D_DATA(pipe), lut3d_pack10(&*lut.add(i)));
    }
    intel_de_write_dsb(display, dsb, LUT_3D_INDEX(pipe), 0);
}
unsafe fn commit_lut_3d(dsb: *mut c_void, crtc: *mut IntelCrtc, enable: bool) {
    let display = display_from_crtc(crtc);
    if display.is_null() {
        return;
    }
    let pipe = (*crtc).pipe as u32;
    if dsb.is_null() && intel_de_read_fw(display, LUT_3D_CTL(pipe)) & LUT_3D_READY != 0 {
        return;
    }
    let val = if enable {
        LUT_3D_ENABLE | LUT_3D_READY | LUT_3D_BIND_PLANE_1
    } else {
        0
    };
    intel_de_write_dsb(display, dsb, LUT_3D_CTL(pipe), val);
}
unsafe fn intel_color_load_3dlut(dsb: *mut c_void, state: *const IntelPlaneState) {
    let crtc = if (*state).uapi.crtc.is_null() {
        core::ptr::null_mut()
    } else {
        (*state).uapi.crtc.cast::<IntelCrtc>()
    };
    if !crtc.is_null()
        && intel_color_crtc_has_3dlut(display_from_crtc(crtc), (*crtc).pipe)
        && !(*state).hw.lut_3d.is_null()
    {
        load_lut_3d(dsb, crtc, (*state).hw.lut_3d);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_color_crtc_has_3dlut(
    display: *mut IntelDisplay,
    pipe: Pipe,
) -> bool {
    !display.is_null() && DISPLAY_VER(display) >= 12 && (pipe == 0 || pipe == 1)
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_color_plane_program_pipeline(
    dsb: *mut c_void,
    state: *const IntelPlaneState,
) {
    if state.is_null() {
        return;
    }
    if !(*state).hw.ctm.is_null() {
        xelpd_load_plane_csc_matrix(dsb, state);
    }
    if !(*state).hw.degamma_lut.is_null() || !(*state).hw.gamma_lut.is_null() {
        xelpd_plane_load_luts(dsb, state);
    }
    if !(*state).hw.lut_3d.is_null() {
        intel_color_load_3dlut(dsb, state);
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_color_plane_commit_arm(
    dsb: *mut c_void,
    state: *const IntelPlaneState,
) {
    if state.is_null() || (*state).uapi.crtc.is_null() {
        return;
    }
    let crtc = (*state).uapi.crtc.cast::<IntelCrtc>();
    if intel_color_crtc_has_3dlut(display_from_crtc(crtc), (*crtc).pipe) {
        commit_lut_3d(dsb, crtc, !(*state).hw.lut_3d.is_null());
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_color_init_hooks(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    if DISPLAY_VER(display) == 13 {
        (*display).color.funcs = core::ptr::addr_of!(TGL_COLOR_FUNCS) as *mut c_void;
    } else {
        // Non-ADLP legacy platforms are deliberately unsupported by this port.
        (*display).color.funcs = core::ptr::null_mut();
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_color_init(display: *mut IntelDisplay) -> i32 {
    if display.is_null() || DISPLAY_VER(display) != 13 {
        -crate::linux_config::EOPNOTSUPP
    } else {
        0
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_color_crtc_init(crtc: *mut IntelCrtc) {
    if crtc.is_null() {
        return;
    }
    let display = display_from_crtc(crtc);
    if display.is_null() {
        return;
    }
    let info = DISPLAY_INFO(display);
    let gamma_size = (*info).color.gamma_lut_size;
    let degamma_size = (*info).color.degamma_lut_size;
    let _ = tk_drm_upstream::drm_color_mgmt_upstream::drm_mode_crtc_set_gamma_size(
        core::ptr::addr_of_mut!((*crtc).base),
        LEGACY_LUT_LENGTH as i32,
    );
    tk_drm_upstream::drm_color_mgmt_upstream::drm_crtc_enable_color_mgmt(
        core::ptr::addr_of_mut!((*crtc).base),
        degamma_size,
        DISPLAY_VER(display) >= 5,
        gamma_size,
    );
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_color_check(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
) -> i32 {
    if state.is_null() || crtc.is_null() {
        return -crate::linux_config::EINVAL;
    }
    let new = intel_atomic_get_new_crtc_state(state, crtc);
    if new.is_null() || crate::linux_config::IS_ERR(new) {
        return if crate::linux_config::IS_ERR(new) {
            crate::linux_config::PTR_ERR(new)
        } else {
            -crate::linux_config::EINVAL
        };
    }
    let alpha = ((*new).uapi.background_color >> 48) as u16;
    if alpha != u16::MAX {
        return -crate::linux_config::EINVAL;
    }
    let old = intel_atomic_get_old_crtc_state(state, crtc);
    if !old.is_null()
        && !crate::linux_config::IS_ERR(old)
        && (((*old).c8_planes == 0) != ((*new).c8_planes == 0)
            || (*old).hw.background_color != (*new).hw.background_color)
    {
        (*new).uapi.set_color_mgmt_changed(true);
    }
    icl_color_check(state, crtc)
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_color_prepare_commit(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
) {
    if state.is_null() || crtc.is_null() {
        return;
    }
    let new = intel_atomic_get_new_crtc_state(state, crtc);
    if new.is_null()
        || crate::linux_config::IS_ERR(new)
        || !(*new).hw.active
        || (*new).uapi.mode_changed()
        || (*new).uapi.active_changed()
        || (*new).uapi.connectors_changed()
        || (!(*new).uapi.color_mgmt_changed() && !(*new).update_pipe)
        || ((*new).pre_csc_lut.is_null() && (*new).post_csc_lut.is_null())
    {
        return;
    }
    // ADLP does not have double-buffered LUTs; color programming uses DSB 1.
    (*new).dsb_color = intel_dsb_prepare(state, crtc, 1u32, 1024);
    if (*new).dsb_color.is_null() || crate::linux_config::IS_ERR((*new).dsb_color) {
        return;
    }
    icl_load_luts(new);
    if (*new).use_dsb && intel_color_uses_chained_dsb(new) {
        intel_vrr_send_push((*new).dsb_color, new);
        intel_dsb_wait_for_delayed_vblank(state, (*new).dsb_color);
        intel_vrr_check_push_sent((*new).dsb_color, new);
        intel_dsb_interrupt((*new).dsb_color);
    }
    if intel_color_uses_gosub_dsb(new) {
        intel_dsb_gosub_finish((*new).dsb_color);
    } else {
        intel_dsb_finish((*new).dsb_color);
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_color_cleanup_commit(state: *mut IntelCrtcState) {
    if state.is_null() || (*state).dsb_color.is_null() {
        return;
    }
    intel_dsb_cleanup((*state).dsb_color);
    (*state).dsb_color = core::ptr::null_mut();
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_color_wait_commit(state: *const IntelCrtcState) {
    if !state.is_null() && !(*state).dsb_color.is_null() {
        intel_dsb_wait((*state).dsb_color);
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_color_uses_dsb(state: *const IntelCrtcState) -> bool {
    !state.is_null() && !(*state).dsb_color.is_null()
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_color_uses_chained_dsb(state: *const IntelCrtcState) -> bool {
    let display = display_from_state(state);
    !state.is_null()
        && !(*state).dsb_color.is_null()
        && !display.is_null()
        && DISPLAY_VER(display) < 30
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_color_uses_gosub_dsb(state: *const IntelCrtcState) -> bool {
    let display = display_from_state(state);
    !state.is_null()
        && !(*state).dsb_color.is_null()
        && !display.is_null()
        && DISPLAY_VER(display) >= 30
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_color_load_luts(state: *const IntelCrtcState) {
    if state.is_null() || !(*state).dsb_color.is_null() {
        return;
    }
    icl_load_luts(state);
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_color_commit_noarm(dsb: *mut c_void, state: *const IntelCrtcState) {
    icl_color_commit_noarm(dsb, state);
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_color_commit_arm(dsb: *mut c_void, state: *const IntelCrtcState) {
    icl_color_commit_arm(dsb, state);
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_color_post_update(state: *const IntelCrtcState) {
    let _ = state; // TGL self-arming CSC writes need no post-latch disarm.
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_color_modeset(state: *const IntelCrtcState) {
    if state.is_null() {
        return;
    }
    run_modeset_color_program(
        || intel_color_load_luts(state),
        || intel_color_commit_noarm(core::ptr::null_mut(), state),
        || intel_color_commit_arm(core::ptr::null_mut(), state),
    );
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_color_get_config(state: *mut IntelCrtcState) {
    if state.is_null() {
        return;
    }
    skl_get_config(state);
    icl_read_luts(state);
    icl_read_csc(state);
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_color_lut_equal(
    state: *const IntelCrtcState,
    a: *const DrmPropertyBlob,
    b: *const DrmPropertyBlob,
    is_pre_csc: bool,
) -> bool {
    if state.is_null() {
        return a == b;
    }
    icl_lut_equal(state, a, b, is_pre_csc)
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_color_assert_luts(state: *const IntelCrtcState) {
    if state.is_null() {
        return;
    }
    crate::linux_assert::warn_on((*state).pre_csc_lut != (*state).hw.degamma_lut);
    crate::linux_assert::warn_on((*state).post_csc_lut != (*state).hw.gamma_lut);
}
#[unsafe(no_mangle)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_color_background_color_drm_to_hw(color: u64) -> u32 {
    let c = |shift: u32| -> u32 {
        let component = (color >> shift) & 0xffff;
        ((component * 1023 + 0x7fff) / 0xffff) as u32
    };
    (c(32) << 20) | (c(16) << 10) | c(0)
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_color_background_color_hw_to_drm(color: u32) -> u64 {
    let c = |value: u32| -> u64 { ((value as u64 * 0xffff + 511) / 1023).min(0xffff) };
    (0xffffu64 << 48)
        | (c((color >> 20) & 0x3ff) << 32)
        | (c((color >> 10) & 0x3ff) << 16)
        | c(color & 0x3ff)
}

#[cfg(test)]
mod tests {
    use core::cell::Cell;

    use super::*;

    #[test]
    fn ctm_coefficient_boundaries_preserve_sign_and_saturate() {
        assert_eq!(ctm_to_ilk_coeff(0), 0x3000);
        assert_eq!(ctm_to_ilk_coeff(CTM_COEFF_1_0), ILK_CSC_COEFF_1_0);
        assert_eq!(ctm_to_ilk_coeff(CTM_COEFF_SIGN | CTM_COEFF_1_0), 0xf800);
        assert_eq!(
            ctm_to_ilk_coeff(CTM_COEFF_4_0),
            ctm_to_ilk_coeff(CTM_COEFF_4_0 - 1)
        );
        assert_eq!(ctm_to_twos_complement(CTM_COEFF_1_0 / 2, 0, 12), 0x0800);
        assert_eq!(
            ctm_to_twos_complement(CTM_COEFF_SIGN | CTM_COEFF_1_0 / 2, 0, 12),
            0x1800
        );
    }

    #[test]
    fn ten_bit_lut_pack_and_unpack_boundaries_are_exact() {
        let mut entry = DrmColorLut {
            red: 0,
            green: 0x8000,
            blue: u16::MAX,
            reserved: 0,
        };
        let packed = ilk_lut_10(&entry);
        assert_eq!(field_get(PREC_PALETTE_10_RED_MASK, packed), 0);
        assert_eq!(field_get(PREC_PALETTE_10_BLUE_MASK, packed), 0x3ff);
        ilk_lut_10_pack(&mut entry, packed);
        assert_eq!(entry.red, 0);
        assert_eq!(entry.blue, u16::MAX);
        assert_eq!(entry.reserved, 0);
        assert_eq!(lut_pack(0, 10), 0);
        assert_eq!(lut_pack(1023, 10), u16::MAX);
    }

    #[test]
    fn twelve_point_four_lut_halves_round_trip() {
        let entry = DrmColorLut {
            red: 0xabcd,
            green: 0x1357,
            blue: 0xfedc,
            reserved: 0,
        };
        let (low, high) = (ilk_lut_12p4_ldw(&entry), ilk_lut_12p4_udw(&entry));
        let mut decoded = DrmColorLut {
            red: 0,
            green: 0,
            blue: 0,
            reserved: 1,
        };
        ilk_lut_12p4_pack(&mut decoded, low, high);
        assert_eq!(decoded.red, entry.red);
        assert_eq!(decoded.green, entry.green);
        assert_eq!(decoded.blue, entry.blue);
        assert_eq!(decoded.reserved, 0);
    }

    #[test]
    fn malformed_lut_blob_lengths_are_rejected_before_dereference() {
        let mut blob: DrmPropertyBlob = unsafe { core::mem::zeroed() };
        blob.length = size_of::<DrmColorLut>() - 1;
        assert_eq!(unsafe { lut_len(&blob) }, None);
        assert!(!unsafe { lut_blob_has_size(&blob, 1) });
        blob.length = size_of::<DrmColorLut>();
        assert_eq!(unsafe { lut_len(&blob) }, Some(1));
        assert!(unsafe { lut_blob_has_size(&blob, 1) });
        assert!(!unsafe { lut_blob_has_size(&blob, 2) });
    }

    #[test]
    fn lut_precision_comparison_and_32_bit_input_endpoints() {
        let mut first = DrmColorLut {
            red: 0x1234,
            green: 0x5678,
            blue: 0x9abc,
            reserved: 0,
        };
        let mut second = DrmColorLut {
            red: 0x123f,
            green: 0x567f,
            blue: 0x9abf,
            reserved: 0,
        };
        let mut a: DrmPropertyBlob = unsafe { core::mem::zeroed() };
        let mut b: DrmPropertyBlob = unsafe { core::mem::zeroed() };
        a.length = size_of::<DrmColorLut>();
        b.length = size_of::<DrmColorLut>();
        a.data = core::ptr::addr_of_mut!(first).cast::<c_void>();
        b.data = core::ptr::addr_of_mut!(second).cast::<c_void>();
        assert!(unsafe { intel_lut_equal(&a, &b, 0, 10) });
        second.red ^= 0x40;
        assert!(!unsafe { intel_lut_equal(&a, &b, 0, 10) });
        assert!(!unsafe { intel_lut_equal(&a, &b, 0, 0) });

        let max = DrmColorLut32 {
            red: u32::MAX,
            green: u32::MAX,
            blue: u32::MAX,
            reserved: 0,
        };
        assert_eq!(
            lut3d_pack10(&max),
            LUT_3D_DATA_RED_MASK | LUT_3D_DATA_GREEN_MASK | LUT_3D_DATA_BLUE_MASK
        );
    }

    #[test]
    fn modeset_programming_order_is_load_then_noarm_then_arm() {
        let next = Cell::new(0usize);
        let events = [Cell::new(0u8), Cell::new(0u8), Cell::new(0u8)];
        let record = |event: u8| {
            let i = next.get();
            events[i].set(event);
            next.set(i + 1);
        };
        run_modeset_color_program(|| record(1), || record(2), || record(3));
        assert_eq!(next.get(), 3);
        assert_eq!(
            [events[0].get(), events[1].get(), events[2].get()],
            [1, 2, 3]
        );
    }
}
