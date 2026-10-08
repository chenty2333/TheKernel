// SPDX-License-Identifier: MIT
// Copyright (c) 2016 Intel Corporation.
//
//! Source-ordered Rust translation of Linux 7.2.3
//! `drivers/gpu/drm/i915/display/intel_color.c`.
//!
//! DRM object/property ownership, atomic-state lookup, register access, and
//! DSB/MMIO execution are framework boundaries. The color conversion, fixed
//! point quantization, LUT selection, and programming sequences remain here.
#![allow(
    dead_code,
    non_camel_case_types,
    non_snake_case,
    clippy::too_many_arguments
)]

extern crate alloc;
use alloc::{vec, vec::Vec};

pub type u8_t = u8;
pub type u16_t = u16;
pub type u32_t = u32;
pub type u64_t = u64;

pub const LEGACY_LUT_LENGTH: usize = 256;
pub const CTM_COEFF_SIGN: u64 = 1 << 63;
pub const CTM_COEFF_1_0: u64 = 1 << 32;
pub const CTM_COEFF_2_0: u64 = CTM_COEFF_1_0 << 1;
pub const CTM_COEFF_4_0: u64 = CTM_COEFF_2_0 << 1;
pub const CTM_COEFF_0_5: u64 = CTM_COEFF_1_0 >> 1;
pub const CTM_COEFF_0_25: u64 = CTM_COEFF_0_5 >> 1;
pub const CTM_COEFF_0_125: u64 = CTM_COEFF_0_25 >> 1;
pub const CTM_COEFF_LIMITED_RANGE: u64 = (235 - 16) * CTM_COEFF_1_0 / 255;
pub const ILK_CSC_COEFF_1_0: u16 = 0x7800;
pub const ILK_CSC_COEFF_LIMITED_RANGE: u16 = (235 - 16) << (12 - 8);
pub const ILK_CSC_POSTOFF_LIMITED_RANGE: u16 = 16 << (12 - 8);
pub const ILK_CSC_COEFF_FP_ONE: u16 = 0x7800;
pub const ILK_CSC_COEFF_FP_LIMITED: u16 = (235 - 16) << (12 - 8);

pub const GAMMA_MODE_MODE_MASK: u32 = 0x7;
pub const GAMMA_MODE_MODE_8BIT: u32 = 0;
pub const GAMMA_MODE_MODE_10BIT: u32 = 1;
pub const GAMMA_MODE_MODE_12BIT_MULTI_SEG: u32 = 2;
pub const GAMMA_MODE_MODE_SPLIT: u32 = 3;
pub const PRE_CSC_GAMMA_ENABLE: u32 = 1 << 8;
pub const POST_CSC_GAMMA_ENABLE: u32 = 1 << 9;
pub const ICL_CSC_ENABLE: u32 = 1 << 0;
pub const ICL_OUTPUT_CSC_ENABLE: u32 = 1 << 1;
pub const CSC_POSITION_BEFORE_GAMMA: u32 = 1 << 0;
pub const CSC_BLACK_SCREEN_OFFSET: u32 = 1 << 1;
pub const CSC_MODE_YUV_TO_RGB: u32 = 1 << 2;
pub const CGM_PIPE_MODE_DEGAMMA: u32 = 1 << 0;
pub const CGM_PIPE_MODE_CSC: u32 = 1 << 1;
pub const CGM_PIPE_MODE_GAMMA: u32 = 1 << 2;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ColorLut {
    pub red: u16,
    pub green: u16,
    pub blue: u16,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ColorLut32 {
    pub red: u32,
    pub green: u32,
    pub blue: u32,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LutBlob<T> {
    pub entries: Vec<T>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CscMatrix {
    pub preoff: [u16; 3],
    pub coeff: [u16; 9],
    pub postoff: [u16; 3],
}
#[derive(Clone, Debug, Default)]
pub struct ColorState {
    pub display_ver: u8,
    pub ivybridge: bool,
    pub gmch: bool,
    pub gamma_lut: Option<LutBlob<ColorLut>>,
    pub degamma_lut: Option<LutBlob<ColorLut>>,
    pub ctm: Option<[u64; 9]>,
    pub gamma_mode: u32,
    pub csc_mode: u32,
    pub cgm_mode: u32,
    pub gamma_enable: bool,
    pub csc_enable: bool,
    pub wgc_enable: bool,
    pub has_psr: bool,
    pub background_color: u32,
    pub active: bool,
    pub active_planes: u64,
    pub update_planes: u64,
    pub async_flip_planes: u64,
    pub do_async_flip: bool,
    pub disable_cxsr: bool,
    pub needs_modeset: bool,
    pub needs_color_update: bool,
    pub use_dsb: bool,
    pub dsb_color: bool,
    pub gamma_lut_size: usize,
    pub degamma_lut_size: usize,
    pub gamma_lut_tests: u32,
    pub degamma_lut_tests: u32,
    pub glk_linear_degamma_lut: Option<LutBlob<ColorLut>>,
    pub preload_luts: bool,
    pub c8_planes: bool,
    pub limited_color_range: bool,
    pub output_format: OutputFormat,
    pub csc: CscMatrix,
    pub output_csc: CscMatrix,
    pub pre_csc_lut: Option<LutBlob<ColorLut>>,
    pub post_csc_lut: Option<LutBlob<ColorLut>>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputFormat {
    #[default]
    Rgb,
    Ycbcr,
}

/// Framework boundary for DRM blob allocation/validation and device access.
/// Algorithms such as coefficient conversion and LUT data generation do not
/// delegate to this trait.
pub trait ColorIo {
    type Error;
    fn dsb_write(&mut self, register: u32, value: u32);
    fn read_register(&mut self, register: u32) -> u32;
    fn color_blob_size_ok(
        &mut self,
        name: &'static str,
        actual: usize,
        expected: usize,
    ) -> Result<(), Self::Error>;
    fn wait_lut_ready(&mut self, register: u32, ready_mask: u32) -> bool;
    fn report_not_ready(&mut self, what: &'static str);
    fn drm_color_lut_check(&mut self, lut: &[ColorLut], tests: u32) -> bool;
    fn can_preload_color_luts(&mut self) -> bool;
    fn invalid_color_state(&mut self) -> Self::Error;
    fn add_affected_color_planes(&mut self) -> Result<(), Self::Error>;
}

// Simplified register identifiers are caller-provided by the display register
// layer; consecutive register addresses preserve the upstream write order.
#[derive(Clone, Copy, Debug)]
pub struct CscRegisters {
    pub coeff: [u32; 6],
    pub preoff: [u32; 3],
    pub postoff: [u32; 3],
}
#[derive(Clone, Copy, Debug)]
pub struct LutRegisters {
    pub index: u32,
    pub data: u32,
    pub auto_increment: u32,
}

// upstream: intel_color.c intel_csc_clear()
pub fn intel_csc_clear(csc: &mut CscMatrix) {
    *csc = CscMatrix::default();
}

// upstream: intel_color.c lut_is_legacy()
pub fn lut_is_legacy(lut: Option<&LutBlob<ColorLut>>) -> bool {
    lut.is_some_and(|lut| lut.entries.len() == LEGACY_LUT_LENGTH)
}

// upstream: intel_color.c ctm_mult_by_limited()
pub fn ctm_mult_by_limited(result: &mut [u64; 9], input: &[u64; 9]) {
    for i in 0..9 {
        let user_coeff = input[i];
        let abs_coeff = (user_coeff & (CTM_COEFF_SIGN - 1)).min(CTM_COEFF_4_0 - 1) >> 2;
        result[i] = (CTM_COEFF_LIMITED_RANGE * abs_coeff) >> 30;
        result[i] |= user_coeff & CTM_COEFF_SIGN;
    }
}

// upstream: intel_color.c ilk_update_pipe_csc()
pub fn ilk_update_pipe_csc<I: ColorIo>(
    io: &mut I,
    display_ver: u8,
    regs: PipeCscRegisterMap,
    csc: &CscMatrix,
) {
    for i in 0..3 {
        io.dsb_write(regs.preoff[i], csc.preoff[i] as u32);
    }
    io.dsb_write(
        regs.coeff[0],
        ((csc.coeff[0] as u32) << 16) | csc.coeff[1] as u32,
    );
    io.dsb_write(regs.coeff[1], (csc.coeff[2] as u32) << 16);
    io.dsb_write(
        regs.coeff[2],
        ((csc.coeff[3] as u32) << 16) | csc.coeff[4] as u32,
    );
    io.dsb_write(regs.coeff[3], (csc.coeff[5] as u32) << 16);
    io.dsb_write(
        regs.coeff[4],
        ((csc.coeff[6] as u32) << 16) | csc.coeff[7] as u32,
    );
    io.dsb_write(regs.coeff[5], (csc.coeff[8] as u32) << 16);
    if display_ver >= 7 {
        for i in 0..3 {
            io.dsb_write(regs.postoff[i], csc.postoff[i] as u32);
        }
    }
}

// upstream: intel_color.c ilk_read_pipe_csc()
pub fn ilk_read_pipe_csc<I: ColorIo>(
    io: &mut I,
    display_ver: u8,
    regs: PipeCscRegisterMap,
    csc: &mut CscMatrix,
) {
    for i in 0..3 {
        csc.preoff[i] = io.read_register(regs.preoff[i]) as u16;
    }
    let tmp = io.read_register(regs.coeff[0]);
    csc.coeff[0] = (tmp >> 16) as u16;
    csc.coeff[1] = tmp as u16;
    csc.coeff[2] = (io.read_register(regs.coeff[1]) >> 16) as u16;
    let tmp = io.read_register(regs.coeff[2]);
    csc.coeff[3] = (tmp >> 16) as u16;
    csc.coeff[4] = tmp as u16;
    csc.coeff[5] = (io.read_register(regs.coeff[3]) >> 16) as u16;
    let tmp = io.read_register(regs.coeff[4]);
    csc.coeff[6] = (tmp >> 16) as u16;
    csc.coeff[7] = tmp as u16;
    csc.coeff[8] = (io.read_register(regs.coeff[5]) >> 16) as u16;
    if display_ver >= 7 {
        for i in 0..3 {
            csc.postoff[i] = io.read_register(regs.postoff[i]) as u16;
        }
    }
}

// upstream: intel_color.c ilk_read_csc()
pub fn ilk_read_csc<I: ColorIo>(io: &mut I, state: &mut ColorState, regs: PipeCscRegisterMap) {
    if state.csc_enable {
        ilk_read_pipe_csc(io, state.display_ver, regs, &mut state.csc);
    }
}
// upstream: intel_color.c skl_read_csc()
pub fn skl_read_csc<I: ColorIo>(io: &mut I, state: &mut ColorState, regs: PipeCscRegisterMap) {
    // As in the source, callers must only read after the double-buffered CSC update latched.
    if state.csc_enable {
        ilk_read_pipe_csc(io, state.display_ver, regs, &mut state.csc);
    }
}
// upstream: intel_color.c icl_update_output_csc()
pub fn icl_update_output_csc<I: ColorIo>(io: &mut I, regs: PipeCscRegisterMap, csc: &CscMatrix) {
    for i in 0..3 {
        io.dsb_write(regs.preoff[i], csc.preoff[i] as u32);
    }
    io.dsb_write(
        regs.coeff[0],
        (csc.coeff[0] as u32) << 16 | csc.coeff[1] as u32,
    );
    io.dsb_write(regs.coeff[1], (csc.coeff[2] as u32) << 16);
    io.dsb_write(
        regs.coeff[2],
        (csc.coeff[3] as u32) << 16 | csc.coeff[4] as u32,
    );
    io.dsb_write(regs.coeff[3], (csc.coeff[5] as u32) << 16);
    io.dsb_write(
        regs.coeff[4],
        (csc.coeff[6] as u32) << 16 | csc.coeff[7] as u32,
    );
    io.dsb_write(regs.coeff[5], (csc.coeff[8] as u32) << 16);
    for i in 0..3 {
        io.dsb_write(regs.postoff[i], csc.postoff[i] as u32);
    }
}

/// Linux source's display-version dispatch: D12+ gets tgl_color_funcs (which
/// has the Xe-LP-D plane hooks), while display 11 gets the ICL pipe hooks.
// upstream: intel_color.c icl_read_output_csc()
pub fn icl_read_output_csc<I: ColorIo>(io: &mut I, regs: PipeCscRegisterMap, csc: &mut CscMatrix) {
    ilk_read_pipe_csc(io, 7, regs, csc);
}
// upstream: intel_color.c icl_read_csc()
pub fn icl_read_csc<I: ColorIo>(
    io: &mut I,
    state: &mut ColorState,
    pipe_regs: PipeCscRegisterMap,
    output_regs: PipeCscRegisterMap,
) {
    if state.csc_mode & ICL_CSC_ENABLE != 0 {
        ilk_read_pipe_csc(io, state.display_ver, pipe_regs, &mut state.csc);
    }
    if state.csc_mode & ICL_OUTPUT_CSC_ENABLE != 0 {
        icl_read_output_csc(io, output_regs, &mut state.output_csc);
    }
}

// upstream: intel_color.c ilk_limited_range()
pub fn ilk_limited_range(state: &ColorState) -> bool {
    if state.display_ver >= 11 || state.display_ver < 7 || state.ivybridge {
        return false;
    }
    state.limited_color_range
}

// upstream: intel_color.c ilk_lut_limited_range()
pub fn ilk_lut_limited_range(state: &ColorState) -> bool {
    if !ilk_limited_range(state) || state.c8_planes {
        return false;
    }
    if state.display_ver == 10 {
        return state.gamma_lut.is_some();
    }
    state.gamma_lut.is_some() && (state.degamma_lut.is_some() || state.ctm.is_some())
}

pub fn ilk_csc_limited_range(state: &ColorState) -> bool {
    ilk_limited_range(state) && !ilk_lut_limited_range(state)
}

// upstream: intel_color.c ilk_csc_limited_range()
pub fn ilk_csc_uses_limited_range(state: &ColorState) -> bool {
    ilk_csc_limited_range(state)
}

pub const PAL_PREC_SPLIT_MODE: u32 = 1 << 31;
pub const PAL_PREC_AUTO_INCREMENT: u32 = 1 << 15;
#[derive(Clone, Copy, Debug)]
pub struct GammaLutRegisters {
    pub legacy_palette_base: u32,
    pub precision_index: u32,
    pub precision_data: u32,
    pub multi_segment_index: u32,
    pub multi_segment_data: u32,
    pub pre_csc_index: u32,
    pub pre_csc_data: u32,
    pub ext_max: [u32; 3],
    pub ext2_max: [u32; 3],
    pub gc_max: [u32; 3],
}
fn rgb_word(entry: ColorLut) -> u32 {
    ((entry.red as u32) << 16) | ((entry.green as u32) << 8) | entry.blue as u32
}
fn ext_rgb_word(regs: GammaLutRegisters, channel: usize, index: u32) -> u32 {
    regs.legacy_palette_base
        .wrapping_add(index * 4 + channel as u32)
}

// upstream: intel_color.c ilk_csc_copy()
pub fn ilk_csc_copy(display_ver: u8, dst: &mut CscMatrix, src: &CscMatrix) {
    *dst = *src;
    if display_ver < 7 {
        dst.postoff = [0; 3];
    }
}

// upstream: intel_color.c ilk_csc_convert_ctm()
pub fn ilk_csc_convert_ctm(
    input: &[u64; 9],
    csc: &mut CscMatrix,
    limited_color_range: bool,
    display_ver: u8,
) {
    let mut temp = [0u64; 9];
    if limited_color_range {
        *csc = CscMatrix {
            preoff: [0; 3],
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
            postoff: [ILK_CSC_POSTOFF_LIMITED_RANGE; 3],
        };
    } else {
        *csc = CscMatrix {
            preoff: [0; 3],
            coeff: [
                ILK_CSC_COEFF_1_0,
                0,
                0,
                0,
                ILK_CSC_COEFF_1_0,
                0,
                0,
                0,
                ILK_CSC_COEFF_1_0,
            ],
            postoff: [0; 3],
        };
    }
    let matrix = if limited_color_range {
        ctm_mult_by_limited(&mut temp, input);
        &temp
    } else {
        input
    };
    for i in 0..9 {
        let abs_coeff = (matrix[i] & (CTM_COEFF_SIGN - 1)).min(CTM_COEFF_4_0 - 1);
        csc.coeff[i] = if matrix[i] & CTM_COEFF_SIGN != 0 {
            1 << 15
        } else {
            0
        };
        csc.coeff[i] |= if abs_coeff < CTM_COEFF_0_125 {
            (3 << 12) | ilk_csc_coeff_fp(abs_coeff, 12)
        } else if abs_coeff < CTM_COEFF_0_25 {
            (2 << 12) | ilk_csc_coeff_fp(abs_coeff, 11)
        } else if abs_coeff < CTM_COEFF_0_5 {
            (1 << 12) | ilk_csc_coeff_fp(abs_coeff, 10)
        } else if abs_coeff < CTM_COEFF_1_0 {
            ilk_csc_coeff_fp(abs_coeff, 9)
        } else if abs_coeff < CTM_COEFF_2_0 {
            (7 << 12) | ilk_csc_coeff_fp(abs_coeff, 8)
        } else {
            (6 << 12) | ilk_csc_coeff_fp(abs_coeff, 7)
        };
    }
    if display_ver < 7 {
        csc.postoff = [0; 3];
    }
}
fn ilk_csc_coeff_fp(coeff: u64, fbits: u32) -> u16 {
    (((coeff >> (32 - fbits - 3)).saturating_add(4).min(0xfff) as u16) & 0xff8)
}

// upstream: intel_color.c ilk_assign_csc()
pub fn ilk_assign_csc(state: &mut ColorState) {
    let limited = ilk_csc_limited_range(state);
    if let Some(ctm) = state.ctm {
        ilk_csc_convert_ctm(&ctm, &mut state.csc, limited, state.display_ver);
    } else if state.output_format != OutputFormat::Rgb {
        let mut matrix = CscMatrix {
            preoff: [0; 3],
            coeff: [
                0x1e08, 0x9cc0, 0xb528, 0x2ba8, 0x09d8, 0x37e8, 0xbce8, 0x9ad8, 0x1e08,
            ],
            postoff: [0x0800, 0x0100, 0x0800],
        };
        if state.display_ver < 7 {
            matrix.postoff = [0; 3];
        }
        state.csc = matrix;
    } else if limited {
        state.csc = CscMatrix {
            preoff: [0; 3],
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
            postoff: [ILK_CSC_POSTOFF_LIMITED_RANGE; 3],
        };
        if state.display_ver < 7 {
            state.csc.postoff = [0; 3];
        }
    } else if state.csc_enable {
        state.csc = CscMatrix {
            preoff: [0; 3],
            coeff: [
                ILK_CSC_COEFF_1_0,
                0,
                0,
                0,
                ILK_CSC_COEFF_1_0,
                0,
                0,
                0,
                ILK_CSC_COEFF_1_0,
            ],
            postoff: [0; 3],
        };
    } else {
        intel_csc_clear(&mut state.csc);
    }
}

// upstream: intel_color.c ilk_load_csc_matrix()
pub fn ilk_load_csc_matrix<I: ColorIo>(io: &mut I, regs: CommitRegisters, state: &ColorState) {
    if state.csc_enable {
        ilk_update_pipe_csc(io, state.display_ver, regs.pipe_csc, &state.csc);
    }
}
// upstream: intel_color.c icl_assign_csc()
pub fn icl_assign_csc(state: &mut ColorState) {
    if let Some(ctm) = state.ctm {
        ilk_csc_convert_ctm(&ctm, &mut state.csc, false, state.display_ver);
    } else {
        intel_csc_clear(&mut state.csc);
    }

    if state.output_format == OutputFormat::Ycbcr {
        state.output_csc = CscMatrix {
            preoff: [0; 3],
            coeff: [
                0x1e08, 0x9cc0, 0xb528, 0x2ba8, 0x09d8, 0x37e8, 0xbce8, 0x9ad8, 0x1e08,
            ],
            postoff: [0x0800, 0x0100, 0x0800],
        };
    } else if state.limited_color_range {
        state.output_csc = CscMatrix {
            preoff: [0; 3],
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
            postoff: [ILK_CSC_POSTOFF_LIMITED_RANGE; 3],
        };
    } else {
        intel_csc_clear(&mut state.output_csc);
    }
}

pub fn icl_read_lut_mode(mode: u32) -> LutReadPath {
    match mode & GAMMA_MODE_MODE_MASK {
        GAMMA_MODE_MODE_8BIT => LutReadPath::Legacy8,
        GAMMA_MODE_MODE_10BIT => LutReadPath::Bdw10,
        GAMMA_MODE_MODE_12BIT_MULTI_SEG => LutReadPath::IclMultiSegment,
        _ => LutReadPath::Unsupported,
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LutReadPath {
    Legacy8,
    Bdw10,
    IclMultiSegment,
    Unsupported,
    GlkDegamma,
}

// upstream: intel_color.c icl_load_csc_matrix()
pub fn icl_load_csc_matrix<I: ColorIo>(io: &mut I, regs: CommitRegisters, state: &ColorState) {
    if state.csc_mode & ICL_CSC_ENABLE != 0 {
        ilk_update_pipe_csc(io, state.display_ver, regs.pipe_csc, &state.csc);
    }
    if state.csc_mode & ICL_OUTPUT_CSC_ENABLE != 0 {
        icl_update_output_csc(io, regs.output_csc, &state.output_csc);
    }
}
// upstream: intel_color.c ctm_to_twos_complement()
pub fn ctm_to_twos_complement(coeff: u64, int_bits: u32, frac_bits: u32) -> u16 {
    let mut value = ((coeff & (CTM_COEFF_SIGN - 1)) >> (32 - frac_bits - 1)) as i64;
    value = (value + 1) >> 1;
    if coeff & CTM_COEFF_SIGN != 0 {
        value = -value;
    }
    let int_bits = int_bits.max(1);
    let range = 1i64 << (int_bits + frac_bits - 1);
    value = value.clamp(-range, range - 1);
    (value as u64 & ((1u64 << (int_bits + frac_bits)) - 1)) as u16
}

// upstream: intel_color.c vlv_wgc_csc_convert_ctm()
pub fn vlv_wgc_csc_convert_ctm(input: &[u64; 9], csc: &mut CscMatrix) {
    for i in 0..9 {
        csc.coeff[i] = ctm_to_twos_complement(input[i], 2, 10);
    }
}
// upstream: intel_color.c vlv_load_wgc_csc()
pub fn vlv_load_wgc_csc<I: ColorIo>(io: &mut I, regs: [u32; 6], csc: &CscMatrix) {
    io.dsb_write(regs[0], ((csc.coeff[1] as u32) << 16) | csc.coeff[0] as u32);
    io.dsb_write(regs[1], csc.coeff[2] as u32);
    io.dsb_write(regs[2], ((csc.coeff[4] as u32) << 16) | csc.coeff[3] as u32);
    io.dsb_write(regs[3], csc.coeff[5] as u32);
    io.dsb_write(regs[4], ((csc.coeff[7] as u32) << 16) | csc.coeff[6] as u32);
    io.dsb_write(regs[5], csc.coeff[8] as u32);
}
// upstream: intel_color.c vlv_read_wgc_csc()
pub fn vlv_read_wgc_csc<I: ColorIo>(io: &mut I, regs: [u32; 6], csc: &mut CscMatrix) {
    let t = io.read_register(regs[0]);
    csc.coeff[0] = t as u16;
    csc.coeff[1] = (t >> 16) as u16;
    csc.coeff[2] = io.read_register(regs[1]) as u16;
    let t = io.read_register(regs[2]);
    csc.coeff[3] = t as u16;
    csc.coeff[4] = (t >> 16) as u16;
    csc.coeff[5] = io.read_register(regs[3]) as u16;
    let t = io.read_register(regs[4]);
    csc.coeff[6] = t as u16;
    csc.coeff[7] = (t >> 16) as u16;
    csc.coeff[8] = io.read_register(regs[5]) as u16;
}
// upstream: intel_color.c vlv_read_csc()
pub fn vlv_read_csc<I: ColorIo>(io: &mut I, regs: [u32; 6], state: &mut ColorState) {
    if state.wgc_enable {
        vlv_read_wgc_csc(io, regs, &mut state.csc);
    }
}
// upstream: intel_color.c vlv_assign_csc()
pub fn vlv_assign_csc(state: &mut ColorState, wgc_enable: bool) {
    if let Some(input) = state.ctm {
        vlv_wgc_csc_convert_ctm(&input, &mut state.csc);
    } else {
        intel_csc_clear(&mut state.csc);
    }
    state.wgc_enable = wgc_enable;
}

pub const CHV_CGM_CSC_COEFF_1_0: u16 = 1 << 12;
// upstream: intel_color.c chv_cgm_csc_convert_ctm()
pub fn chv_cgm_csc_convert_ctm(input: &[u64; 9], csc: &mut CscMatrix) {
    for i in 0..9 {
        csc.coeff[i] = ctm_to_twos_complement(input[i], 4, 12);
    }
}
// upstream: intel_color.c chv_load_cgm_csc()
pub fn chv_load_cgm_csc<I: ColorIo>(io: &mut I, regs: [u32; 5], csc: &CscMatrix) {
    for i in 0..4 {
        io.dsb_write(
            regs[i],
            ((csc.coeff[i * 2 + 1] as u32) << 16) | csc.coeff[i * 2] as u32,
        );
    }
    io.dsb_write(regs[4], csc.coeff[8] as u32);
}
// upstream: intel_color.c chv_read_cgm_csc()
pub fn chv_read_cgm_csc<I: ColorIo>(io: &mut I, regs: [u32; 5], csc: &mut CscMatrix) {
    for i in 0..4 {
        let value = io.read_register(regs[i]);
        csc.coeff[i * 2] = value as u16;
        csc.coeff[i * 2 + 1] = (value >> 16) as u16;
    }
    csc.coeff[8] = io.read_register(regs[4]) as u16;
}
// upstream: intel_color.c chv_read_csc()
pub fn chv_read_csc<I: ColorIo>(io: &mut I, regs: [u32; 5], state: &mut ColorState) {
    if state.cgm_mode & CGM_PIPE_MODE_CSC != 0 {
        chv_read_cgm_csc(io, regs, &mut state.csc);
    }
}
// upstream: intel_color.c chv_assign_csc()
pub fn chv_assign_csc(state: &mut ColorState) {
    if let Some(input) = state.ctm {
        chv_cgm_csc_convert_ctm(&input, &mut state.csc);
    } else {
        state.csc = CscMatrix {
            preoff: [0; 3],
            coeff: [
                CHV_CGM_CSC_COEFF_1_0,
                0,
                0,
                0,
                CHV_CGM_CSC_COEFF_1_0,
                0,
                0,
                0,
                CHV_CGM_CSC_COEFF_1_0,
            ],
            postoff: [0; 3],
        };
    }
}

// upstream: intel_color.c intel_color_lut_pack()
pub fn intel_color_lut_pack(val: u32, bit_precision: u32) -> u32 {
    if bit_precision > 16 {
        let max = (1u64 << bit_precision) - 1;
        (((val as u64 * 0xffff) + max / 2) / max) as u32
    } else {
        let max = (1u64 << bit_precision) - 1;
        (((val as u64 * 0xffff) + max / 2) / max) as u32
    }
}

// upstream: intel_color.c i9xx_lut_8()
pub fn i9xx_lut_8(color: ColorLut) -> u32 {
    ((color.red as u32 >> 8) << 16) | ((color.green as u32 >> 8) << 8) | (color.blue as u32 >> 8)
}
// upstream: intel_color.c i9xx_lut_8_pack()
pub fn i9xx_lut_8_pack(entry: &mut ColorLut, val: u32) {
    entry.red = (((val >> 16) & 0xff) * 0x101) as u16;
    entry.green = (((val >> 8) & 0xff) * 0x101) as u16;
    entry.blue = ((val & 0xff) * 0x101) as u16;
}
// upstream: intel_color.c _i9xx_lut_10_ldw()
pub fn _i9xx_lut_10_ldw(a: u16) -> u32 {
    ((a as u32 >> 6) & 0x3ff) & 0xff
}
// upstream: intel_color.c i9xx_lut_10_ldw()
pub fn i9xx_lut_10_ldw(color: ColorLut) -> u32 {
    ((_i9xx_lut_10_ldw(color.red) & 0xff) << 16)
        | ((_i9xx_lut_10_ldw(color.green) & 0xff) << 8)
        | (_i9xx_lut_10_ldw(color.blue) & 0xff)
}
pub fn i9xx_lut_10_udw(a: ColorLut, b: ColorLut) -> u32 {
    ((_i9xx_lut_10_udw(a.red, b.red) & 0xff) << 16)
        | ((_i9xx_lut_10_udw(a.green, b.green) & 0xff) << 8)
        | (_i9xx_lut_10_udw(a.blue, b.blue) & 0xff)
}
// upstream: intel_color.c _i9xx_lut_10_udw()
pub fn _i9xx_lut_10_udw(a: u16, b: u16) -> u32 {
    let a = (a as u32) >> 6;
    let b = (b as u32) >> 6;
    let mut mantissa = b.wrapping_sub(a).min(0x7f);
    let mut exponent = 3u32;
    while mantissa > 0xf {
        mantissa >>= 1;
        exponent -= 1;
    }
    (exponent << 6) | (mantissa << 2) | (a >> 8)
}
// upstream: intel_color.c i9xx_lut_10_udw()
pub fn i9xx_lut_10_udw_pair(first: ColorLut, next: ColorLut) -> u32 {
    ((_i9xx_lut_10_udw(first.red, next.red) & 0xff) << 16)
        | ((_i9xx_lut_10_udw(first.green, next.green) & 0xff) << 8)
        | (_i9xx_lut_10_udw(first.blue, next.blue) & 0xff)
}

// upstream: intel_color.c i9xx_lut_10_pack()
pub fn i9xx_lut_10_pack(color: &mut ColorLut, ldw: u32, udw: u32) {
    let red = ((ldw >> 16) & 0xff) | (((udw >> 16) & 0x3) << 8);
    let green = ((ldw >> 8) & 0xff) | (((udw >> 8) & 0x3) << 8);
    let blue = (ldw & 0xff) | ((udw & 0x3) << 8);
    color.red = intel_color_lut_pack(red, 10) as u16;
    color.green = intel_color_lut_pack(green, 10) as u16;
    color.blue = intel_color_lut_pack(blue, 10) as u16;
}
// upstream: intel_color.c i9xx_lut_10_pack_slope()
pub fn i9xx_lut_10_pack_slope(color: &mut ColorLut, ldw: u32, udw: u32) {
    i9xx_lut_10_pack(color, ldw, udw);
    for (channel, shift) in [(0usize, 16u32), (1, 8), (2, 0)] {
        let byte = (udw >> shift) & 0xff;
        let exponent = (byte >> 6) & 0x3;
        let mantissa = (byte >> 2) & 0xf;
        let base = match channel {
            0 => &mut color.red,
            1 => &mut color.green,
            _ => &mut color.blue,
        };
        *base = base.saturating_add((mantissa << (3 - exponent)) as u16);
    }
}
// upstream: intel_color.c i965_lut_10p6_ldw()
pub fn i965_lut_10p6_ldw(color: ColorLut) -> u32 {
    ((color.red as u32 & 0xff) << 16)
        | ((color.green as u32 & 0xff) << 8)
        | (color.blue as u32 & 0xff)
}
// upstream: intel_color.c i965_lut_10p6_udw()
pub fn i965_lut_10p6_udw(color: ColorLut) -> u32 {
    ((color.red as u32 >> 8) << 16) | ((color.green as u32 >> 8) << 8) | (color.blue as u32 >> 8)
}
// upstream: intel_color.c i965_lut_10p6_pack()
pub fn i965_lut_10p6_pack(entry: &mut ColorLut, ldw: u32, udw: u32) {
    entry.red = (((udw >> 16) & 0xff) << 8 | ((ldw >> 16) & 0xff)) as u16;
    entry.green = (((udw >> 8) & 0xff) << 8 | ((ldw >> 8) & 0xff)) as u16;
    entry.blue = ((udw & 0xff) << 8 | (ldw & 0xff)) as u16;
}
// upstream: intel_color.c i965_lut_11p6_max_pack()
pub fn i965_lut_11p6_max_pack(value: u32) -> u32 {
    value.min(0xffff)
}
// upstream: intel_color.c ilk_lut_10()
pub fn ilk_lut_10(color: ColorLut) -> u32 {
    ((color.red as u32 >> 6) << 20) | ((color.green as u32 >> 6) << 10) | (color.blue as u32 >> 6)
}
// upstream: intel_color.c ilk_lut_10_pack()
pub fn ilk_lut_10_pack(entry: &mut ColorLut, val: u32) {
    entry.red = intel_color_lut_pack((val >> 20) & 0x3ff, 10) as u16;
    entry.green = intel_color_lut_pack((val >> 10) & 0x3ff, 10) as u16;
    entry.blue = intel_color_lut_pack(val & 0x3ff, 10) as u16;
}

// upstream: intel_color.c ilk_lut_12p4_ldw()
pub fn ilk_lut_12p4_ldw(color: ColorLut) -> u32 {
    (((color.red as u32 & 0x3f) << 24)
        | ((color.green as u32 & 0x3f) << 14)
        | ((color.blue as u32 & 0x3f) << 4))
}
// upstream: intel_color.c ilk_lut_12p4_udw()
pub fn ilk_lut_12p4_udw(color: ColorLut) -> u32 {
    (((color.red as u32 >> 6) << 20) | ((color.green as u32 >> 6) << 10) | (color.blue as u32 >> 6))
}
// upstream: intel_color.c ilk_lut_12p4_pack()
pub fn ilk_lut_12p4_pack(entry: &mut ColorLut, ldw: u32, udw: u32) {
    entry.red = (((udw >> 20) & 0x3ff) << 6 | ((ldw >> 24) & 0x3f)) as u16;
    entry.green = (((udw >> 10) & 0x3ff) << 6 | ((ldw >> 14) & 0x3f)) as u16;
    entry.blue = ((udw & 0x3ff) << 6 | ((ldw >> 4) & 0x3f)) as u16;
}

// upstream: intel_color.c icl_color_commit_noarm()
pub fn icl_color_commit_noarm<I: ColorIo>(io: &mut I, regs: CommitRegisters, state: &ColorState) {
    icl_load_csc_matrix(io, regs, state);
}
// upstream: intel_color.c skl_color_commit_noarm()
pub fn skl_color_commit_noarm<I: ColorIo>(io: &mut I, regs: CommitRegisters, state: &ColorState) {
    if !state.has_psr {
        ilk_load_csc_matrix(io, regs, state);
    }
}
// upstream: intel_color.c ilk_color_commit_noarm()
pub fn ilk_color_commit_noarm<I: ColorIo>(io: &mut I, regs: CommitRegisters, state: &ColorState) {
    ilk_load_csc_matrix(io, regs, state);
}
// upstream: intel_color.c i9xx_color_commit_arm()
pub fn i9xx_color_commit_arm<I: ColorIo>(
    io: &mut I,
    regs: CommitRegisters,
    state: &ColorState,
    pipeconf_gamma_mask: u32,
) {
    let old = io.read_register(regs.gamma_mode);
    let value = if state.gamma_enable {
        old | pipeconf_gamma_mask
    } else {
        old & !pipeconf_gamma_mask
    };
    io.dsb_write(regs.gamma_mode, value);
}
// upstream: intel_color.c ilk_color_commit_arm()
pub fn ilk_color_commit_arm<I: ColorIo>(
    io: &mut I,
    regs: CommitRegisters,
    state: &ColorState,
    pipeconf_gamma_mask: u32,
) {
    i9xx_color_commit_arm(io, regs, state, pipeconf_gamma_mask);
    io.dsb_write(regs.csc_mode, state.csc_mode);
}
// upstream: intel_color.c hsw_color_commit_arm()
pub fn hsw_color_commit_arm<I: ColorIo>(io: &mut I, regs: CommitRegisters, state: &ColorState) {
    io.dsb_write(regs.gamma_mode, state.gamma_mode);
    io.dsb_write(regs.csc_mode, state.csc_mode);
}
// upstream: intel_color.c hsw_read_gamma_mode()
pub fn hsw_read_gamma_mode<I: ColorIo>(io: &mut I, reg: u32) -> u32 {
    io.read_register(reg)
}
// upstream: intel_color.c ilk_read_csc_mode()
pub fn ilk_read_csc_mode<I: ColorIo>(io: &mut I, reg: u32) -> u32 {
    io.read_register(reg)
}
// upstream: intel_color.c i9xx_get_config()
pub fn i9xx_get_config<I: ColorIo>(
    io: &mut I,
    state: &mut ColorState,
    plane_control: u32,
    gamma_mask: u32,
    csc_mask: u32,
    gmch: bool,
) {
    let value = io.read_register(plane_control);
    if value & gamma_mask != 0 {
        state.gamma_enable = true;
    }
    if !gmch && value & csc_mask != 0 {
        state.csc_enable = true;
    }
}
// upstream: intel_color.c hsw_get_config()
pub fn hsw_get_config<I: ColorIo>(
    io: &mut I,
    state: &mut ColorState,
    regs: CommitRegisters,
    plane_control: u32,
    gamma_mask: u32,
    csc_mask: u32,
    gmch: bool,
) {
    state.gamma_mode = hsw_read_gamma_mode(io, regs.gamma_mode);
    state.csc_mode = ilk_read_csc_mode(io, regs.csc_mode);
    i9xx_get_config(io, state, plane_control, gamma_mask, csc_mask, gmch);
}
// upstream: intel_color.c skl_get_config()
pub fn skl_get_config<I: ColorIo>(
    io: &mut I,
    state: &mut ColorState,
    regs: CommitRegisters,
    plane_control: u32,
    gamma_mask: u32,
    csc_mask: u32,
    gmch: bool,
    bottom_gamma: u32,
    bottom_csc: u32,
    display_ver: u8,
) {
    state.gamma_mode = hsw_read_gamma_mode(io, regs.gamma_mode);
    state.csc_mode = ilk_read_csc_mode(io, regs.csc_mode);
    let color = io.read_register(regs.bottom_color);
    if display_ver < 35 {
        if color & bottom_gamma != 0 {
            state.gamma_enable = true;
        }
        if color & bottom_csc != 0 {
            state.csc_enable = true;
        }
    }
    state.background_color = color & 0x3fff_ffff;
    i9xx_get_config(io, state, plane_control, gamma_mask, csc_mask, gmch);
}

// upstream: intel_color.c intel_color_background_color_drm_to_hw()
pub fn intel_color_background_color_drm_to_hw(rgb16: [u16; 3]) -> u32 {
    ((rgb16[0] as u32 >> 6) << 20) | ((rgb16[1] as u32 >> 6) << 10) | (rgb16[2] as u32 >> 6)
}
// upstream: intel_color.c intel_color_background_color_hw_to_drm()
pub fn intel_color_background_color_hw_to_drm(hw: u32) -> [u16; 4] {
    let expand = |v: u32| ((v * 0xffff + 511) / 1023) as u16;
    [
        0xffff,
        expand((hw >> 20) & 0x3ff),
        expand((hw >> 10) & 0x3ff),
        expand(hw & 0x3ff),
    ]
}
// upstream: intel_color.c skl_color_commit_arm()
pub fn skl_color_commit_arm<I: ColorIo>(
    io: &mut I,
    regs: CommitRegisters,
    state: &ColorState,
    gamma_enable: u32,
    csc_enable: u32,
) {
    let mut val = state.background_color;
    if state.gamma_enable {
        val |= gamma_enable
    };
    if state.csc_enable {
        val |= csc_enable
    };
    if state.has_psr {
        ilk_load_csc_matrix(io, regs, state);
    }
    io.dsb_write(regs.bottom_color, val);
    io.dsb_write(regs.gamma_mode, state.gamma_mode);
    io.dsb_write(regs.csc_mode, state.csc_mode);
}
// upstream: intel_color.c icl_color_commit_arm()
pub fn icl_color_commit_arm<I: ColorIo>(io: &mut I, regs: CommitRegisters, state: &ColorState) {
    io.dsb_write(regs.bottom_color, state.background_color);
    io.dsb_write(regs.gamma_mode, state.gamma_mode);
    io.dsb_write(regs.csc_mode, state.csc_mode);
}
// upstream: intel_color.c icl_color_post_update()
pub fn icl_color_post_update<I: ColorIo>(io: &mut I, display_ver: u8, regs: PipeCscRegisterMap) {
    if display_ver == 11 {
        let _ = io.read_register(regs.preoff[0]);
    }
}
// upstream: intel_color.c create_linear_lut()
pub fn create_linear_lut(size: usize) -> LutBlob<ColorLut> {
    let mut entries = Vec::with_capacity(size);
    for i in 0..size {
        let val = if size <= 1 {
            0xffff
        } else {
            (0xffffusize * i / (size - 1)) as u16
        };
        entries.push(ColorLut {
            red: val,
            green: val,
            blue: val,
        });
    }
    LutBlob { entries }
}
// upstream: intel_color.c lut_limited_range()
pub fn lut_limited_range(value: u16) -> u16 {
    let min = 16u32 << 8;
    let max = 235u32 << 8;
    ((value as u32 * (max - min) / 0xffff) + min) as u16
}
// upstream: intel_color.c create_resized_lut()
pub fn create_resized_lut(
    input: &LutBlob<ColorLut>,
    out_size: usize,
    limited: bool,
) -> LutBlob<ColorLut> {
    let in_size = input.entries.len();
    let mut out = Vec::with_capacity(out_size);
    for i in 0..out_size {
        let index = if out_size <= 1 {
            0
        } else {
            i.saturating_mul(in_size.saturating_sub(1)) / (out_size - 1)
        };
        let mut entry = input.entries.get(index).copied().unwrap_or_default();
        if limited {
            entry.red = lut_limited_range(entry.red);
            entry.green = lut_limited_range(entry.green);
            entry.blue = lut_limited_range(entry.blue);
        }
        out.push(entry);
    }
    LutBlob { entries: out }
}
// upstream: intel_color.c i9xx_load_lut_8()
pub fn i9xx_load_lut_8<I: ColorIo>(io: &mut I, regs: GammaLutRegisters, lut: Option<&[ColorLut]>) {
    let Some(lut) = lut else { return };
    for i in 0..256 {
        ilk_lut_write(
            io,
            regs.legacy_palette_base + i as u32 * 4,
            i9xx_lut_8(lut[i]),
        );
    }
}
// upstream: intel_color.c i9xx_load_lut_10()
pub fn i9xx_load_lut_10<I: ColorIo>(io: &mut I, regs: GammaLutRegisters, lut: Option<&[ColorLut]>) {
    let Some(lut) = lut else { return };
    if lut.is_empty() {
        return;
    }
    for i in 0..lut.len() - 1 {
        ilk_lut_write(
            io,
            regs.legacy_palette_base + (2 * i) as u32 * 4,
            i9xx_lut_10_ldw(lut[i]),
        );
        ilk_lut_write(
            io,
            regs.legacy_palette_base + (2 * i + 1) as u32 * 4,
            i9xx_lut_10_udw(lut[i], lut[i + 1]),
        );
    }
}
// upstream: intel_color.c i9xx_load_luts()
pub fn i9xx_load_luts<I: ColorIo>(io: &mut I, regs: GammaLutRegisters, state: &ColorState) {
    match state.gamma_mode {
        GAMMA_MODE_MODE_8BIT => i9xx_load_lut_8(
            io,
            regs,
            state.post_csc_lut.as_ref().map(|b| b.entries.as_slice()),
        ),
        GAMMA_MODE_MODE_10BIT => i9xx_load_lut_10(
            io,
            regs,
            state.post_csc_lut.as_ref().map(|b| b.entries.as_slice()),
        ),
        _ => {}
    }
}
// upstream: intel_color.c i965_load_lut_10p6()
pub fn i965_load_lut_10p6<I: ColorIo>(
    io: &mut I,
    regs: GammaLutRegisters,
    lut: Option<&[ColorLut]>,
) {
    let Some(lut) = lut else { return };
    if lut.is_empty() {
        return;
    }
    for i in 0..lut.len() - 1 {
        ilk_lut_write(
            io,
            regs.legacy_palette_base + (2 * i) as u32 * 4,
            i965_lut_10p6_ldw(lut[i]),
        );
        ilk_lut_write(
            io,
            regs.legacy_palette_base + (2 * i + 1) as u32 * 4,
            i965_lut_10p6_udw(lut[i]),
        );
    }
    for c in 0..3 {
        let v = [
            lut.last().unwrap().red,
            lut.last().unwrap().green,
            lut.last().unwrap().blue,
        ][c];
        ilk_lut_write(io, regs.gc_max[c], v as u32);
    }
}
// upstream: intel_color.c i965_load_luts()
pub fn i965_load_luts<I: ColorIo>(
    io: &mut I,
    dsb: bool,
    regs: GammaLutRegisters,
    state: &ColorState,
) {
    match state.gamma_mode {
        GAMMA_MODE_MODE_8BIT => i9xx_load_lut_8(
            io,
            regs,
            state.post_csc_lut.as_ref().map(|b| b.entries.as_slice()),
        ),
        GAMMA_MODE_MODE_10BIT => i965_load_lut_10p6(
            io,
            regs,
            state.post_csc_lut.as_ref().map(|b| b.entries.as_slice()),
        ),
        _ => {}
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CommitRegisters {
    pub bottom_color: u32,
    pub gamma_mode: u32,
    pub csc_mode: u32,
    pub pipe_csc: PipeCscRegisterMap,
    pub output_csc: PipeCscRegisterMap,
}
// upstream: intel_color.c ilk_lut_write()
pub fn ilk_lut_write<I: ColorIo>(io: &mut I, register: u32, value: u32) {
    io.dsb_write(register, value);
}
// upstream: intel_color.c ilk_lut_write_indexed()
pub fn ilk_lut_write_indexed<I: ColorIo>(io: &mut I, register: u32, value: u32) {
    io.dsb_write(register, value);
}

// upstream: intel_color.c ilk_load_lut_8()
pub fn ilk_load_lut_8<I: ColorIo>(
    io: &mut I,
    dsb_color: bool,
    regs: GammaLutRegisters,
    lut: Option<&[ColorLut]>,
) {
    let Some(lut) = lut else {
        return;
    };
    for i in 0..256 {
        let addr = regs.legacy_palette_base + i as u32 * 4;
        let value = i9xx_lut_8(lut[i]);
        ilk_lut_write(io, addr, value);
        if dsb_color {
            ilk_lut_write(io, addr, value);
        }
    }
}
// upstream: intel_color.c ilk_load_lut_10()
pub fn ilk_load_lut_10<I: ColorIo>(io: &mut I, regs: GammaLutRegisters, lut: Option<&[ColorLut]>) {
    let Some(lut) = lut else {
        return;
    };
    for (i, entry) in lut.iter().enumerate() {
        ilk_lut_write(
            io,
            regs.legacy_palette_base + (i as u32) * 4,
            ilk_lut_10(*entry),
        );
    }
}
// upstream: intel_color.c ilk_load_luts()
pub fn ilk_load_luts<I: ColorIo>(
    io: &mut I,
    dsb_color: bool,
    regs: GammaLutRegisters,
    mode: u32,
    pre: Option<&[ColorLut]>,
    post: Option<&[ColorLut]>,
) {
    let lut = post.or(pre);
    match mode {
        GAMMA_MODE_MODE_8BIT => ilk_load_lut_8(io, dsb_color, regs, lut),
        GAMMA_MODE_MODE_10BIT => ilk_load_lut_10(io, regs, lut),
        _ => {}
    }
}

// upstream: intel_color.c ivb_lut_10_size()
pub fn ivb_lut_10_size(prec_index: u32) -> usize {
    if prec_index & PAL_PREC_SPLIT_MODE != 0 {
        512
    } else {
        1024
    }
}
// upstream: intel_color.c ivb_load_lut_10()
pub fn ivb_load_lut_10<I: ColorIo>(
    io: &mut I,
    regs: GammaLutRegisters,
    lut: &[ColorLut],
    prec_index: u32,
) {
    for (i, entry) in lut.iter().enumerate() {
        ilk_lut_write(io, regs.precision_index, prec_index + i as u32);
        ilk_lut_write(io, regs.precision_data, ilk_lut_10(*entry));
    }
    ilk_lut_write(io, regs.precision_index, 0);
}
// upstream: intel_color.c bdw_load_lut_10()
pub fn bdw_load_lut_10<I: ColorIo>(
    io: &mut I,
    regs: GammaLutRegisters,
    lut: &[ColorLut],
    prec_index: u32,
) {
    ilk_lut_write(io, regs.precision_index, prec_index);
    ilk_lut_write(
        io,
        regs.precision_index,
        PAL_PREC_AUTO_INCREMENT | prec_index,
    );
    for entry in lut {
        ilk_lut_write_indexed(io, regs.precision_data, ilk_lut_10(*entry));
    }
    ilk_lut_write(io, regs.precision_index, 0);
}
// upstream: intel_color.c ivb_load_lut_ext_max()
pub fn ivb_load_lut_ext_max<I: ColorIo>(io: &mut I, regs: GammaLutRegisters) {
    for address in regs.ext_max {
        ilk_lut_write(io, address, 1 << 16);
    }
}
// upstream: intel_color.c glk_load_lut_ext2_max()
pub fn glk_load_lut_ext2_max<I: ColorIo>(io: &mut I, regs: GammaLutRegisters) {
    for address in regs.ext2_max {
        ilk_lut_write(io, address, 1 << 16);
    }
}
// upstream: intel_color.c ivb_load_luts()
pub fn ivb_load_luts<I: ColorIo>(
    io: &mut I,
    dsb: bool,
    regs: GammaLutRegisters,
    mode: u32,
    pre: Option<&[ColorLut]>,
    post: Option<&[ColorLut]>,
) {
    let lut = post.or(pre);
    match mode {
        GAMMA_MODE_MODE_8BIT => ilk_load_lut_8(io, dsb, regs, lut),
        GAMMA_MODE_MODE_SPLIT => {
            if let Some(pre) = pre {
                ivb_load_lut_10(io, regs, pre, PAL_PREC_SPLIT_MODE);
            }
            ivb_load_lut_ext_max(io, regs);
            if let Some(post) = post {
                ivb_load_lut_10(io, regs, post, PAL_PREC_SPLIT_MODE | 512);
            }
        }
        GAMMA_MODE_MODE_10BIT => {
            if let Some(lut) = lut {
                ivb_load_lut_10(io, regs, lut, 0);
            }
            ivb_load_lut_ext_max(io, regs);
        }
        _ => {}
    }
}

// upstream: intel_color.c bdw_load_luts()
pub fn bdw_load_luts<I: ColorIo>(
    io: &mut I,
    dsb: bool,
    regs: GammaLutRegisters,
    mode: u32,
    pre: Option<&[ColorLut]>,
    post: Option<&[ColorLut]>,
) {
    let lut = post.or(pre);
    match mode {
        GAMMA_MODE_MODE_8BIT => ilk_load_lut_8(io, dsb, regs, lut),
        GAMMA_MODE_MODE_SPLIT => {
            if let Some(pre) = pre {
                bdw_load_lut_10(io, regs, pre, PAL_PREC_SPLIT_MODE);
            }
            ivb_load_lut_ext_max(io, regs);
            if let Some(post) = post {
                bdw_load_lut_10(io, regs, post, PAL_PREC_SPLIT_MODE | 512);
            }
        }
        GAMMA_MODE_MODE_10BIT => {
            if let Some(lut) = lut {
                bdw_load_lut_10(io, regs, lut, 0);
            }
            ivb_load_lut_ext_max(io, regs);
        }
        _ => {}
    }
}
// upstream: intel_color.c glk_degamma_lut_size()
pub fn glk_degamma_lut_size(display_ver: u8) -> usize {
    if display_ver >= 13 { 131 } else { 35 }
}

// upstream: intel_color.c glk_degamma_lut()
pub fn glk_degamma_lut(color: ColorLut) -> u32 {
    color.green as u32
}
// upstream: intel_color.c glk_degamma_lut_pack()
pub fn glk_degamma_lut_pack(entry: &mut ColorLut, val: u32) {
    let v = val.min(0xffff) as u16;
    entry.red = v;
    entry.green = v;
    entry.blue = v;
}
// upstream: intel_color.c mtl_degamma_lut()
pub fn mtl_degamma_lut(color: ColorLut) -> u32 {
    (color.green as u32 * 0xffffff + 32767) / 65535
}
// upstream: intel_color.c mtl_degamma_lut_pack()
pub fn mtl_degamma_lut_pack(entry: &mut ColorLut, val: u32) {
    let v = intel_color_lut_pack(val.min(0xffffff), 24) as u16;
    entry.red = v;
    entry.green = v;
    entry.blue = v;
}

/// Translate the source's CTM S31.32 coefficient to the Xe-LP-D signed 4.12
/// floating exponent representation. `input` and the register sequence are
/// caller-supplied hardware/framework data; conversion is algorithmic here.
pub fn xelpd_plane_csc_coefficients(input: &[u64; 12]) -> ([u16; 9], [u16; 3]) {
    let mut coeffs = [0u16; 9];
    let mut j = 0usize;
    for coeff in &mut coeffs {
        let raw = input[j];
        let abs_coeff = (raw & (CTM_COEFF_SIGN - 1)).min(CTM_COEFF_4_0 - 1);
        let (exp, fbits) = if abs_coeff < CTM_COEFF_0_125 {
            (3, 12)
        } else if abs_coeff < CTM_COEFF_0_25 {
            (2, 11)
        } else if abs_coeff < CTM_COEFF_0_5 {
            (1, 10)
        } else if abs_coeff < CTM_COEFF_1_0 {
            (0, 9)
        } else if abs_coeff < CTM_COEFF_2_0 {
            (7, 8)
        } else {
            (6, 7)
        };
        let fp = ((abs_coeff >> (32 - fbits - 3)).saturating_add(4).min(0xfff) as u16) & 0xff8;
        *coeff = ((if raw & CTM_COEFF_SIGN != 0 {
            1 << 15
        } else {
            0
        }) | (exp << 12)
            | fp) as u16;
        j += if (j + 2) % 4 == 0 { 2 } else { 1 };
    }
    let post = [input[3], input[7], input[11]].map(|v| ctm_to_s0_12_impl(v));
    (coeffs, post)
}
fn ctm_to_s0_12_impl(value: u64) -> u16 {
    // ctm_to_twos_complement(value, int_bits=0, frac_bits=12), including
    // upstream's extra-bit rounding, sign application, and signed clamp.
    let magnitude = (value & (CTM_COEFF_SIGN - 1)) >> (32 - 12 - 1);
    let rounded = ((magnitude as i64) + 1) >> 1;
    let signed = if value & CTM_COEFF_SIGN != 0 {
        -rounded
    } else {
        rounded
    };
    signed.clamp(-(1 << 11), (1 << 11) - 1) as u16 & 0x0fff
}

// upstream: intel_color.c glk_load_degamma_lut()
pub fn glk_load_degamma_lut<I: ColorIo>(
    io: &mut I,
    regs: GammaLutRegisters,
    lut: &[ColorLut],
    display_ver: u8,
) {
    ilk_lut_write(io, regs.pre_csc_index, 0);
    ilk_lut_write(io, regs.pre_csc_index, PAL_PREC_AUTO_INCREMENT);
    for entry in lut {
        let value = if display_ver >= 14 {
            mtl_degamma_lut(*entry)
        } else {
            glk_degamma_lut(*entry)
        };
        ilk_lut_write_indexed(io, regs.pre_csc_data, value);
    }
    let size = glk_degamma_lut_size(display_ver);
    let mut i = lut.len();
    while i < size {
        ilk_lut_write_indexed(
            io,
            regs.pre_csc_data,
            if display_ver >= 14 { 1 << 24 } else { 1 << 16 },
        );
        i += 1;
    }
    ilk_lut_write(io, regs.pre_csc_index, 0);
}

// upstream: intel_color.c glk_load_luts()
pub fn glk_load_luts<I: ColorIo>(
    io: &mut I,
    dsb: bool,
    regs: GammaLutRegisters,
    state: &ColorState,
) {
    if let Some(pre) = &state.pre_csc_lut {
        glk_load_degamma_lut(io, regs, &pre.entries, state.display_ver);
    }
    match state.gamma_mode {
        GAMMA_MODE_MODE_8BIT => ilk_load_lut_8(
            io,
            dsb,
            regs,
            state.post_csc_lut.as_ref().map(|b| b.entries.as_slice()),
        ),
        GAMMA_MODE_MODE_10BIT => {
            if let Some(post) = &state.post_csc_lut {
                bdw_load_lut_10(io, regs, &post.entries, 0);
            }
            ivb_load_lut_ext_max(io, regs);
            glk_load_lut_ext2_max(io, regs);
        }
        _ => {}
    }
}

// ---- LUT validation and assignment; DRM policy hooks are explicit ----
// upstream: intel_color.c ivb_load_lut_max()
pub fn ivb_load_lut_max<I: ColorIo>(io: &mut I, regs: GammaLutRegisters, color: ColorLut) {
    let values = [color.red, color.green, color.blue];
    for i in 0..3 {
        ilk_lut_write(io, regs.gc_max[i], values[i] as u32);
    }
}
// upstream: intel_color.c icl_program_gamma_superfine_segment()
pub fn icl_program_gamma_superfine_segment<I: ColorIo>(
    io: &mut I,
    regs: GammaLutRegisters,
    lut: &[ColorLut],
) {
    ilk_lut_write(io, regs.multi_segment_index, 0);
    ilk_lut_write(io, regs.multi_segment_index, PAL_PREC_AUTO_INCREMENT);
    for entry in &lut[..9] {
        ilk_lut_write_indexed(io, regs.multi_segment_data, ilk_lut_12p4_ldw(*entry));
        ilk_lut_write_indexed(io, regs.multi_segment_data, ilk_lut_12p4_udw(*entry));
    }
    ilk_lut_write(io, regs.multi_segment_index, 0);
}
// upstream: intel_color.c icl_program_gamma_multi_segment()
pub fn icl_program_gamma_multi_segment<I: ColorIo>(
    io: &mut I,
    regs: GammaLutRegisters,
    lut: &[ColorLut],
) {
    ilk_lut_write(io, regs.precision_index, 0);
    ilk_lut_write(io, regs.precision_index, PAL_PREC_AUTO_INCREMENT);
    for i in 1..257 {
        let entry = lut[i * 8];
        ilk_lut_write_indexed(io, regs.precision_data, ilk_lut_12p4_ldw(entry));
        ilk_lut_write_indexed(io, regs.precision_data, ilk_lut_12p4_udw(entry));
    }
    for i in 0..256 {
        let entry = lut[i * 8 * 128];
        ilk_lut_write_indexed(io, regs.precision_data, ilk_lut_12p4_ldw(entry));
        ilk_lut_write_indexed(io, regs.precision_data, ilk_lut_12p4_udw(entry));
    }
    ilk_lut_write(io, regs.precision_index, 0);
    ivb_load_lut_max(io, regs, lut[256 * 8 * 128]);
}

// upstream: intel_color.c icl_load_luts()
pub fn icl_load_luts<I: ColorIo>(
    io: &mut I,
    dsb: bool,
    regs: GammaLutRegisters,
    state: &ColorState,
) {
    if let Some(pre) = &state.pre_csc_lut {
        glk_load_degamma_lut(io, regs, &pre.entries, state.display_ver);
    }
    let post = state.post_csc_lut.as_ref().map(|b| b.entries.as_slice());
    match state.gamma_mode & GAMMA_MODE_MODE_MASK {
        GAMMA_MODE_MODE_8BIT => ilk_load_lut_8(io, dsb, regs, post),
        GAMMA_MODE_MODE_12BIT_MULTI_SEG => {
            if let Some(post) = post {
                icl_program_gamma_superfine_segment(io, regs, post);
                icl_program_gamma_multi_segment(io, regs, post);
            }
            ivb_load_lut_ext_max(io, regs);
            glk_load_lut_ext2_max(io, regs);
        }
        GAMMA_MODE_MODE_10BIT => {
            if let Some(post) = post {
                bdw_load_lut_10(io, regs, post, 0);
            }
            ivb_load_lut_ext_max(io, regs);
            glk_load_lut_ext2_max(io, regs);
        }
        _ => {}
    }
}
// upstream: intel_color.c vlv_load_luts()
pub fn vlv_load_luts<I: ColorIo>(
    io: &mut I,
    state: &ColorState,
    regs: GammaLutRegisters,
    wgc_regs: [u32; 6],
    dsb: bool,
) {
    if state.wgc_enable {
        vlv_load_wgc_csc(io, wgc_regs, &state.csc);
    }
    i965_load_luts(io, dsb, regs, state);
}

// upstream: intel_color.c chv_cgm_degamma_ldw()
pub fn chv_cgm_degamma_ldw(color: ColorLut) -> u32 {
    ((((color.green as u32) >> 2) & 0x3fff) << 16) | (((color.blue as u32) >> 2) & 0x3fff)
}
// upstream: intel_color.c chv_cgm_degamma_udw()
pub fn chv_cgm_degamma_udw(color: ColorLut) -> u32 {
    (color.red as u32 >> 2) & 0x3fff
}
// upstream: intel_color.c chv_cgm_degamma_pack()
pub fn chv_cgm_degamma_pack(entry: &mut ColorLut, ldw: u32, udw: u32) {
    entry.green = intel_color_lut_pack((ldw >> 16) & 0x3fff, 14) as u16;
    entry.blue = intel_color_lut_pack(ldw & 0x3fff, 14) as u16;
    entry.red = intel_color_lut_pack(udw & 0x3fff, 14) as u16;
}
// upstream: intel_color.c chv_load_cgm_degamma()
pub fn chv_load_cgm_degamma<I: ColorIo>(io: &mut I, base: u32, lut: &[ColorLut]) {
    for (i, color) in lut.iter().enumerate() {
        io.dsb_write(base + i as u32 * 8, chv_cgm_degamma_ldw(*color));
        io.dsb_write(base + i as u32 * 8 + 4, chv_cgm_degamma_udw(*color));
    }
}
// upstream: intel_color.c chv_cgm_gamma_ldw()
pub fn chv_cgm_gamma_ldw(color: ColorLut) -> u32 {
    (((color.green as u32 >> 6) & 0x3ff) << 16) | ((color.blue as u32 >> 6) & 0x3ff)
}
// upstream: intel_color.c chv_cgm_gamma_udw()
pub fn chv_cgm_gamma_udw(color: ColorLut) -> u32 {
    (color.red as u32 >> 6) & 0x3ff
}
// upstream: intel_color.c chv_cgm_gamma_pack()
pub fn chv_cgm_gamma_pack(entry: &mut ColorLut, ldw: u32, udw: u32) {
    entry.green = intel_color_lut_pack((ldw >> 16) & 0x3ff, 10) as u16;
    entry.blue = intel_color_lut_pack(ldw & 0x3ff, 10) as u16;
    entry.red = intel_color_lut_pack(udw & 0x3ff, 10) as u16;
}
// upstream: intel_color.c chv_load_cgm_gamma()
pub fn chv_load_cgm_gamma<I: ColorIo>(io: &mut I, base: u32, lut: &[ColorLut]) {
    for (i, color) in lut.iter().enumerate() {
        io.dsb_write(base + i as u32 * 8, chv_cgm_gamma_ldw(*color));
        io.dsb_write(base + i as u32 * 8 + 4, chv_cgm_gamma_udw(*color));
    }
}
// upstream: intel_color.c chv_load_luts()
pub fn chv_load_luts<I: ColorIo>(
    io: &mut I,
    state: &ColorState,
    regs: GammaLutRegisters,
    csc_regs: [u32; 5],
    degamma_base: u32,
    gamma_base: u32,
    mode_reg: u32,
    dsb: bool,
) {
    if state.cgm_mode & CGM_PIPE_MODE_CSC != 0 {
        chv_load_cgm_csc(io, csc_regs, &state.csc);
    }
    if state.cgm_mode & CGM_PIPE_MODE_DEGAMMA != 0 {
        if let Some(lut) = &state.pre_csc_lut {
            chv_load_cgm_degamma(io, degamma_base, &lut.entries);
        }
    }
    if state.cgm_mode & CGM_PIPE_MODE_GAMMA != 0 {
        if let Some(lut) = &state.post_csc_lut {
            chv_load_cgm_gamma(io, gamma_base, &lut.entries);
        }
    } else {
        i965_load_luts(io, dsb, regs, state);
    }
    io.dsb_write(mode_reg, state.cgm_mode);
}

// upstream: intel_color.c intel_color_load_luts()
pub fn intel_color_load_luts<H: ColorHooks>(hooks: &mut H, state: &H::State, dsb_color: bool) {
    if !dsb_color {
        hooks.load_luts(state);
    }
}
// upstream: intel_color.c intel_color_commit_noarm()
pub fn intel_color_commit_noarm<H: ColorHooks>(hooks: &mut H, state: &H::State) {
    hooks.commit_noarm(state);
}
// upstream: intel_color.c intel_color_commit_arm()
pub fn intel_color_commit_arm<H: ColorHooks>(hooks: &mut H, state: &H::State) {
    hooks.commit_arm(state);
}
// upstream: intel_color.c intel_color_post_update()
pub fn intel_color_post_update<H: ColorHooks>(hooks: &mut H, state: &H::State) {
    hooks.post_update(state);
}

// upstream: intel_color.c intel_color_modeset()
pub fn intel_color_modeset<H: ColorHooks>(hooks: &mut H, state: &H::State) {
    hooks.load_luts(state);
    hooks.commit_noarm(state);
    hooks.commit_arm(state);
}

// upstream: intel_color.c intel_color_uses_dsb()
pub fn intel_color_uses_dsb(state: &ColorState) -> bool {
    state.dsb_color
}
// upstream: intel_color.c intel_color_uses_chained_dsb()
pub fn intel_color_uses_chained_dsb(state: &ColorState, has_double_buffered_lut: bool) -> bool {
    state.dsb_color && !has_double_buffered_lut
}
// upstream: intel_color.c intel_color_uses_gosub_dsb()
pub fn intel_color_uses_gosub_dsb(state: &ColorState, has_double_buffered_lut: bool) -> bool {
    state.dsb_color && has_double_buffered_lut
}

pub trait ColorCommitBackend: ColorIo {
    type Dsb;
    fn prepare_dsb(&mut self, dsb_index: u8, capacity: usize) -> Option<Self::Dsb>;
    fn load_luts_into_dsb(&mut self, dsb: &mut Self::Dsb, state: &ColorState);
    fn send_vrr_push(&mut self, dsb: &mut Self::Dsb, state: &ColorState);
    fn wait_delayed_vblank(&mut self, dsb: &mut Self::Dsb);
    fn check_vrr_push_sent(&mut self, dsb: &mut Self::Dsb);
    fn interrupt_dsb(&mut self, dsb: &mut Self::Dsb);
    fn finish_dsb(&mut self, dsb: &mut Self::Dsb, gosub: bool);
    fn cleanup_dsb(&mut self, dsb: Self::Dsb);
    fn wait_dsb(&mut self, dsb: &Self::Dsb);
}
// upstream: intel_color.c intel_color_prepare_commit()
pub fn intel_color_prepare_commit<B: ColorCommitBackend>(
    backend: &mut B,
    state: &mut ColorState,
    has_double_buffered_lut: bool,
) -> Option<B::Dsb> {
    if !state.active
        || state.needs_modeset
        || !state.needs_color_update
        || state.pre_csc_lut.is_none() && state.post_csc_lut.is_none()
    {
        return None;
    }
    let index = if has_double_buffered_lut { 0 } else { 1 };
    let mut dsb = backend.prepare_dsb(index, 1024)?;
    state.dsb_color = true;
    backend.load_luts_into_dsb(&mut dsb, state);
    if state.use_dsb && intel_color_uses_chained_dsb(state, has_double_buffered_lut) {
        backend.send_vrr_push(&mut dsb, state);
        backend.wait_delayed_vblank(&mut dsb);
        backend.check_vrr_push_sent(&mut dsb);
        backend.interrupt_dsb(&mut dsb);
    }
    backend.finish_dsb(
        &mut dsb,
        intel_color_uses_gosub_dsb(state, has_double_buffered_lut),
    );
    Some(dsb)
}
// upstream: intel_color.c intel_color_cleanup_commit()
pub fn intel_color_cleanup_commit<B: ColorCommitBackend>(backend: &mut B, dsb: Option<B::Dsb>) {
    if let Some(dsb) = dsb {
        backend.cleanup_dsb(dsb);
    }
}
// upstream: intel_color.c intel_color_wait_commit()
pub fn intel_color_wait_commit<B: ColorCommitBackend>(backend: &mut B, dsb: Option<&B::Dsb>) {
    if let Some(dsb) = dsb {
        backend.wait_dsb(dsb);
    }
}
// upstream: intel_color.c intel_can_preload_luts()
pub fn intel_can_preload_luts(
    has_double_buffered_lut: bool,
    old_pre: Option<&LutBlob<ColorLut>>,
    old_post: Option<&LutBlob<ColorLut>>,
) -> bool {
    !has_double_buffered_lut && old_post.is_none() && old_pre.is_none()
}
// upstream: intel_color.c vlv_can_preload_luts()
pub fn vlv_can_preload_luts(old_wgc_enable: bool, old_post: Option<&LutBlob<ColorLut>>) -> bool {
    !old_wgc_enable && old_post.is_none()
}
// upstream: intel_color.c chv_can_preload_luts()
pub fn chv_can_preload_luts(
    old_cgm: u32,
    new_cgm: u32,
    old_wgc: bool,
    old_post: Option<&LutBlob<ColorLut>>,
) -> bool {
    if old_cgm != 0 || new_cgm != 0 {
        return false;
    }
    vlv_can_preload_luts(old_wgc, old_post)
}
// upstream: intel_color.c intel_color_check()
pub fn intel_color_check<H: ColorHooks>(
    hooks: &mut H,
    old: &ColorState,
    new: &mut H::State,
    new_c8_planes: bool,
    new_hw_background: u32,
    new_background_alpha: u16,
    needs_color_update: bool,
) -> i32 {
    if old.c8_planes != new_c8_planes || old.background_color != new_hw_background { /* atomic glue marks color_mgmt_changed */
    }
    if new_background_alpha != 0xffff {
        return -22;
    }
    if !needs_color_update {
        return 0;
    }
    hooks.color_check(new)
}
// upstream: intel_color.c intel_color_get_config()
pub fn intel_color_get_config<H: ColorHooks>(hooks: &mut H, state: &mut H::State) {
    hooks.get_config(state);
    hooks.read_luts(state);
    hooks.read_csc(state);
}
// upstream: intel_color.c intel_color_lut_equal()
pub fn intel_color_lut_equal_dispatch<H: ColorHooks>(
    hooks: &mut H,
    state: &H::State,
    a: Option<&LutBlob<ColorLut>>,
    b: Option<&LutBlob<ColorLut>>,
    pre: bool,
    c8_planes: bool,
) -> bool {
    if !pre && c8_planes {
        return true;
    }
    hooks.lut_equal(state, a, b, pre)
}
pub fn intel_color_select_path(
    gmch: bool,
    display_ver: u8,
    cherryview: bool,
    valleyview: bool,
    haswell: bool,
) -> DisplayColorPath {
    if gmch {
        if cherryview {
            DisplayColorPath::Chv
        } else if valleyview {
            DisplayColorPath::Vlv
        } else if display_ver >= 4 {
            DisplayColorPath::I965
        } else {
            DisplayColorPath::I9xx
        }
    } else if display_ver >= 12 {
        DisplayColorPath::TglD12Plus
    } else if display_ver == 11 {
        DisplayColorPath::IclD11
    } else if display_ver == 10 {
        DisplayColorPath::Glk
    } else if display_ver == 9 {
        DisplayColorPath::Skl
    } else if display_ver == 8 {
        DisplayColorPath::Bdw
    } else if haswell {
        DisplayColorPath::Hsw
    } else if display_ver == 7 {
        DisplayColorPath::Ivb
    } else {
        DisplayColorPath::Ilk
    }
}
// upstream: intel_color.c need_plane_update()
pub fn need_plane_update(display_ver: u8, plane_id: u8, active_planes: u64) -> bool {
    active_planes & (1u64 << plane_id) != 0 || display_ver < 9 && plane_id == 0
}
// Minimal framebuffer/plane iteration data at the atomic framework boundary.
#[derive(Clone, Debug, Default)]
pub struct ColorPlaneState {
    pub id: u8,
    pub pipe: u8,
    pub ctm: Option<[u64; 12]>,
    pub degamma_lut: Option<Vec<ColorLut32>>,
    pub gamma_lut: Option<Vec<ColorLut32>>,
    pub lut_3d: Option<Vec<ColorLut32>>,
}
// upstream: intel_color.c intel_color_add_affected_planes()
pub fn intel_color_add_affected_planes(
    old: &ColorState,
    new: &mut ColorState,
    plane_ids: &[u8],
    gmch: bool,
) -> u64 {
    if !new.active || new.needs_modeset {
        return 0;
    }
    if old.gamma_enable == new.gamma_enable && old.csc_enable == new.csc_enable {
        return 0;
    }
    let mut affected = 0;
    for id in plane_ids {
        if !need_plane_update(new.display_ver, *id, new.active_planes) {
            continue;
        }
        affected |= 1u64 << *id;
    }
    new.update_planes |= affected;
    new.async_flip_planes = 0;
    new.do_async_flip = false;
    if gmch && affected != 0 {
        new.disable_cxsr = true;
    }
    affected
}
// upstream: intel_color.c intel_gamma_lut_tests()
pub fn intel_gamma_lut_tests(state: &ColorState) -> u32 {
    if lut_is_legacy(state.gamma_lut.as_ref()) {
        0
    } else {
        state.gamma_lut_tests
    }
}
// upstream: intel_color.c intel_degamma_lut_tests()
pub fn intel_degamma_lut_tests(state: &ColorState) -> u32 {
    state.degamma_lut_tests
}
// upstream: intel_color.c intel_gamma_lut_size()
pub fn intel_gamma_lut_size(state: &ColorState) -> usize {
    if lut_is_legacy(state.gamma_lut.as_ref()) {
        LEGACY_LUT_LENGTH
    } else {
        state.gamma_lut_size
    }
}
// upstream: intel_color.c intel_degamma_lut_size()
pub fn intel_degamma_lut_size(state: &ColorState) -> usize {
    state.degamma_lut_size
}

// upstream: intel_color.c check_lut_size()
pub fn check_lut_size<I: ColorIo>(
    io: &mut I,
    name: &'static str,
    lut: Option<&LutBlob<ColorLut>>,
    expected: usize,
) -> bool {
    let Some(blob) = lut else {
        return true;
    };
    let framework_ok = io
        .color_blob_size_ok(name, blob.entries.len(), expected)
        .is_ok();
    framework_ok && blob.entries.len() == expected
}
// upstream: intel_color.c _check_luts()
pub fn _check_luts<I: ColorIo>(
    io: &mut I,
    state: &ColorState,
    degamma_tests: u32,
    gamma_tests: u32,
) -> bool {
    if state.c8_planes && !lut_is_legacy(state.gamma_lut.as_ref()) {
        return false;
    }
    let degamma_length = intel_degamma_lut_size(state);
    let gamma_length = intel_gamma_lut_size(state);
    if !check_lut_size(io, "degamma", state.degamma_lut.as_ref(), degamma_length)
        || !check_lut_size(io, "gamma", state.gamma_lut.as_ref(), gamma_length)
    {
        return false;
    }
    if let Some(lut) = &state.degamma_lut {
        if !io.drm_color_lut_check(&lut.entries, degamma_tests) {
            return false;
        }
    }
    if let Some(lut) = &state.gamma_lut {
        if !io.drm_color_lut_check(&lut.entries, gamma_tests) {
            return false;
        }
    }
    true
}
// upstream: intel_color.c check_luts()
pub fn check_luts<I: ColorIo>(io: &mut I, state: &ColorState) -> Result<(), I::Error> {
    if !_check_luts(
        io,
        state,
        intel_degamma_lut_tests(state),
        intel_gamma_lut_tests(state),
    ) {
        Err(io.invalid_color_state())
    } else {
        Ok(())
    }
}
// upstream: intel_color.c i9xx_gamma_mode()
pub fn i9xx_gamma_mode(state: &ColorState) -> u32 {
    if !state.gamma_enable || lut_is_legacy(state.gamma_lut.as_ref()) {
        GAMMA_MODE_MODE_8BIT
    } else {
        GAMMA_MODE_MODE_10BIT
    }
}
// upstream: intel_color.c i9xx_lut_10_diff()
pub fn i9xx_lut_10_diff(a: u16, b: u16) -> i32 {
    (((a as u32 >> 6) & 0x3ff) as i32) - (((b as u32 >> 6) & 0x3ff) as i32)
}

pub fn ctm_to_s0_12(coeff: u64) -> u16 {
    ctm_to_twos_complement(coeff, 0, 12)
}

// upstream: intel_color.c i9xx_check_lut_10()
pub fn i9xx_check_lut_10(lut: &[ColorLut]) -> bool {
    if lut.len() < 2 {
        return false;
    }
    let a = lut[lut.len() - 2];
    let b = lut[lut.len() - 1];
    i9xx_lut_10_diff(b.red, a.red) <= 0x7f
        && i9xx_lut_10_diff(b.green, a.green) <= 0x7f
        && i9xx_lut_10_diff(b.blue, a.blue) <= 0x7f
}
// upstream: intel_color.c intel_color_assert_luts()
pub fn intel_color_assert_luts(state: &ColorState) -> bool {
    let same = |a: &Option<LutBlob<ColorLut>>, b: &Option<LutBlob<ColorLut>>| {
        a.as_ref().map(|x| &x.entries) == b.as_ref().map(|x| &x.entries)
    };
    if state.display_ver >= 11 || state.gmch {
        return same(&state.pre_csc_lut, &state.degamma_lut)
            && same(&state.post_csc_lut, &state.gamma_lut);
    }
    if state.display_ver == 10 {
        let first = !(same(&state.post_csc_lut, &state.gamma_lut)
            && !same(&state.pre_csc_lut, &state.degamma_lut)
            && !same(&state.pre_csc_lut, &state.glk_linear_degamma_lut));
        let second = ilk_lut_limited_range(state)
            || state.post_csc_lut.is_none()
            || same(&state.post_csc_lut, &state.gamma_lut);
        return first && second;
    }
    if state.gamma_mode == GAMMA_MODE_MODE_SPLIT {
        return true;
    }
    let pre_ok =
        same(&state.pre_csc_lut, &state.degamma_lut) || same(&state.pre_csc_lut, &state.gamma_lut);
    let post_ok = ilk_lut_limited_range(state)
        || same(&state.post_csc_lut, &state.degamma_lut)
        || same(&state.post_csc_lut, &state.gamma_lut);
    pre_ok && post_ok
}
// upstream: intel_color.c intel_assign_luts()
pub fn intel_assign_luts(state: &mut ColorState) {
    state.pre_csc_lut = state.degamma_lut.clone();
    state.post_csc_lut = state.gamma_lut.clone();
}
// upstream: intel_color.c i9xx_color_check()
pub fn i9xx_color_check<I: ColorIo>(io: &mut I, state: &mut ColorState) -> Result<(), I::Error> {
    check_luts(io, state)?;
    state.gamma_enable = state.gamma_lut.is_some() && !state.c8_planes;
    state.gamma_mode = i9xx_gamma_mode(state);
    if state.display_ver < 4
        && state.gamma_mode == GAMMA_MODE_MODE_10BIT
        && state
            .gamma_lut
            .as_ref()
            .is_some_and(|l| !i9xx_check_lut_10(&l.entries))
    {
        return Err(io.invalid_color_state());
    }
    io.add_affected_color_planes()?;
    intel_assign_luts(state);
    state.preload_luts = io.can_preload_color_luts();
    Ok(())
}
// upstream: intel_color.c vlv_color_check()
pub fn vlv_color_check<I: ColorIo>(io: &mut I, state: &mut ColorState) -> Result<(), I::Error> {
    check_luts(io, state)?;
    state.gamma_enable = state.gamma_lut.is_some() && !state.c8_planes;
    state.gamma_mode = i9xx_gamma_mode(state);
    state.wgc_enable = state.ctm.is_some();
    io.add_affected_color_planes()?;
    intel_assign_luts(state);
    vlv_assign_csc(state, state.wgc_enable);
    state.preload_luts = io.can_preload_color_luts();
    Ok(())
}
// upstream: intel_color.c chv_cgm_mode()
pub fn chv_cgm_mode(state: &ColorState) -> u32 {
    let mut mode = 0;
    if state.degamma_lut.is_some() {
        mode |= CGM_PIPE_MODE_DEGAMMA;
    }
    if state.ctm.is_some() {
        mode |= CGM_PIPE_MODE_CSC;
    }
    if state
        .gamma_lut
        .as_ref()
        .is_some_and(|lut| lut.entries.len() != LEGACY_LUT_LENGTH)
    {
        mode |= CGM_PIPE_MODE_GAMMA;
    }
    mode | CGM_PIPE_MODE_CSC
}
// upstream: intel_color.c chv_color_check()
pub fn chv_color_check<I: ColorIo>(io: &mut I, state: &mut ColorState) -> Result<(), I::Error> {
    check_luts(io, state)?;
    state.gamma_enable = lut_is_legacy(state.gamma_lut.as_ref()) && !state.c8_planes;
    state.gamma_mode = GAMMA_MODE_MODE_8BIT;
    state.cgm_mode = chv_cgm_mode(state);
    state.wgc_enable = false;
    io.add_affected_color_planes()?;
    intel_assign_luts(state);
    chv_assign_csc(state);
    state.preload_luts = io.can_preload_color_luts();
    Ok(())
}
// upstream: intel_color.c ilk_gamma_enable()
pub fn ilk_gamma_enable(state: &ColorState) -> bool {
    (state.gamma_lut.is_some() || state.degamma_lut.is_some()) && !state.c8_planes
}
// upstream: intel_color.c ilk_csc_enable()
pub fn ilk_csc_enable(state: &ColorState) -> bool {
    state.output_format != OutputFormat::Rgb || ilk_csc_limited_range(state) || state.ctm.is_some()
}
// upstream: intel_color.c ilk_gamma_mode()
pub fn ilk_gamma_mode(state: &ColorState) -> u32 {
    if !state.gamma_enable || lut_is_legacy(state.gamma_lut.as_ref()) {
        GAMMA_MODE_MODE_8BIT
    } else {
        GAMMA_MODE_MODE_10BIT
    }
}
// upstream: intel_color.c ilk_csc_mode()
pub fn ilk_csc_mode(state: &ColorState) -> u32 {
    if state.output_format != OutputFormat::Rgb {
        CSC_BLACK_SCREEN_OFFSET
    } else if state.degamma_lut.is_some() {
        CSC_MODE_YUV_TO_RGB
    } else {
        CSC_MODE_YUV_TO_RGB | CSC_POSITION_BEFORE_GAMMA
    }
}
// upstream: intel_color.c ilk_assign_luts()
pub fn ilk_assign_luts(state: &mut ColorState) -> bool {
    if ilk_lut_limited_range(state) {
        if let Some(gamma) = state.gamma_lut.as_ref() {
            state.post_csc_lut = Some(create_resized_lut(gamma, gamma.entries.len(), true));
        }
        state.pre_csc_lut = state.degamma_lut.clone();
        return true;
    }
    if state.degamma_lut.is_some() || state.csc_mode & CSC_POSITION_BEFORE_GAMMA != 0 {
        state.pre_csc_lut = state.degamma_lut.clone();
        state.post_csc_lut = state.gamma_lut.clone();
    } else {
        state.pre_csc_lut = state.gamma_lut.clone();
        state.post_csc_lut = None;
    }
    true
}
// upstream: intel_color.c ilk_color_check()
pub fn ilk_color_check<I: ColorIo>(io: &mut I, state: &mut ColorState) -> Result<(), I::Error> {
    check_luts(io, state)?;
    if state.degamma_lut.is_some() && state.gamma_lut.is_some() {
        return Err(io.invalid_color_state());
    }
    if state.output_format != OutputFormat::Rgb && state.ctm.is_some() {
        return Err(io.invalid_color_state());
    }
    state.gamma_enable = ilk_gamma_enable(state);
    state.csc_enable = ilk_csc_enable(state);
    state.gamma_mode = ilk_gamma_mode(state);
    state.csc_mode = ilk_csc_mode(state);
    io.add_affected_color_planes()?;
    if !ilk_assign_luts(state) {
        return Err(io.invalid_color_state());
    }
    ilk_assign_csc(state);
    state.preload_luts = io.can_preload_color_luts();
    Ok(())
}
// upstream: intel_color.c ivb_gamma_mode()
pub fn ivb_gamma_mode(state: &ColorState) -> u32 {
    if state.degamma_lut.is_some() && state.gamma_lut.is_some() {
        GAMMA_MODE_MODE_SPLIT
    } else {
        ilk_gamma_mode(state)
    }
}
// upstream: intel_color.c ivb_csc_mode()
pub fn ivb_csc_mode(state: &ColorState) -> u32 {
    if state.degamma_lut.is_some()
        || state.output_format != OutputFormat::Rgb
        || ilk_csc_limited_range(state)
    {
        0
    } else {
        CSC_POSITION_BEFORE_GAMMA
    }
}

// upstream: intel_color.c ivb_assign_luts()
pub fn ivb_assign_luts(state: &mut ColorState) -> bool {
    if state.gamma_mode != GAMMA_MODE_MODE_SPLIT {
        return ilk_assign_luts(state);
    }
    let (Some(degamma), Some(gamma)) = (state.degamma_lut.as_ref(), state.gamma_lut.as_ref())
    else {
        return false;
    };
    state.pre_csc_lut = Some(create_resized_lut(degamma, 512, false));
    state.post_csc_lut = Some(create_resized_lut(gamma, 512, ilk_lut_limited_range(state)));
    true
}
// upstream: intel_color.c ivb_color_check()
pub fn ivb_color_check<I: ColorIo>(io: &mut I, state: &mut ColorState) -> Result<(), I::Error> {
    check_luts(io, state)?;
    if state.c8_planes && state.degamma_lut.is_some() {
        return Err(io.invalid_color_state());
    }
    if state.output_format != OutputFormat::Rgb && state.ctm.is_some() {
        return Err(io.invalid_color_state());
    }
    if state.output_format != OutputFormat::Rgb
        && state.degamma_lut.is_some()
        && state.gamma_lut.is_some()
    {
        return Err(io.invalid_color_state());
    }
    state.gamma_enable = ilk_gamma_enable(state);
    state.csc_enable = ilk_csc_enable(state);
    state.gamma_mode = ivb_gamma_mode(state);
    state.csc_mode = ivb_csc_mode(state);
    io.add_affected_color_planes()?;
    if !ivb_assign_luts(state) {
        return Err(io.invalid_color_state());
    }
    ilk_assign_csc(state);
    state.preload_luts = io.can_preload_color_luts();
    Ok(())
}
// upstream: intel_color.c glk_gamma_mode()
pub fn glk_gamma_mode(state: &ColorState) -> u32 {
    if !state.gamma_enable
        || state
            .gamma_lut
            .as_ref()
            .is_some_and(|lut| lut.entries.len() == LEGACY_LUT_LENGTH)
    {
        GAMMA_MODE_MODE_8BIT
    } else {
        GAMMA_MODE_MODE_10BIT
    }
}
// upstream: intel_color.c glk_use_pre_csc_lut_for_gamma()
pub fn glk_use_pre_csc_lut_for_gamma(state: &ColorState) -> bool {
    state.gamma_lut.is_some() && !state.c8_planes && state.output_format != OutputFormat::Rgb
}
// upstream: intel_color.c glk_assign_luts()
pub fn glk_assign_luts(state: &mut ColorState) -> bool {
    if glk_use_pre_csc_lut_for_gamma(state) {
        if let Some(gamma) = state.gamma_lut.as_ref() {
            state.pre_csc_lut = Some(create_resized_lut(gamma, state.degamma_lut_size, false));
        }
        state.post_csc_lut = None;
        return true;
    }
    if ilk_lut_limited_range(state) {
        if let Some(gamma) = state.gamma_lut.as_ref() {
            state.post_csc_lut = Some(create_resized_lut(gamma, gamma.entries.len(), true));
        }
    } else {
        state.post_csc_lut = state.gamma_lut.clone();
    }
    state.pre_csc_lut = state.degamma_lut.clone();
    if state.csc_enable && state.pre_csc_lut.is_none() {
        state.pre_csc_lut = state.glk_linear_degamma_lut.clone();
    }
    true
}
// upstream: intel_color.c glk_check_luts()
pub fn glk_check_luts<I: ColorIo>(io: &mut I, state: &ColorState) -> Result<(), I::Error> {
    let degamma_tests = intel_degamma_lut_tests(state);
    let mut gamma_tests = intel_gamma_lut_tests(state);
    if glk_use_pre_csc_lut_for_gamma(state) {
        gamma_tests |= degamma_tests;
    }
    if !_check_luts(io, state, degamma_tests, gamma_tests) {
        Err(io.invalid_color_state())
    } else {
        Ok(())
    }
}

// These checks retain the source's validation / mode-selection / assignment
// order. Plane-state acquisition is delegated only at its DRM atomic boundary.
// upstream: intel_color.c glk_color_check()
pub fn glk_color_check<I: ColorIo>(io: &mut I, state: &mut ColorState) -> Result<(), I::Error> {
    glk_check_luts(io, state)?;
    if state.output_format != OutputFormat::Rgb && state.ctm.is_some() {
        return Err(io.invalid_color_state());
    }
    if state.output_format != OutputFormat::Rgb
        && state.degamma_lut.is_some()
        && state.gamma_lut.is_some()
    {
        return Err(io.invalid_color_state());
    }
    state.gamma_enable =
        !glk_use_pre_csc_lut_for_gamma(state) && state.gamma_lut.is_some() && !state.c8_planes;
    state.csc_enable = glk_use_pre_csc_lut_for_gamma(state)
        || state.degamma_lut.is_some()
        || state.output_format != OutputFormat::Rgb
        || state.ctm.is_some()
        || ilk_csc_limited_range(state);
    state.gamma_mode = glk_gamma_mode(state);
    state.csc_mode = 0;
    io.add_affected_color_planes()?;
    if !glk_assign_luts(state) {
        return Err(io.invalid_color_state());
    }
    ilk_assign_csc(state);
    state.preload_luts = io.can_preload_color_luts();
    Ok(())
}

// upstream: intel_color.c icl_gamma_mode()
pub fn icl_gamma_mode(state: &ColorState) -> u32 {
    let mut mode = 0;
    if state.degamma_lut.is_some() {
        mode |= PRE_CSC_GAMMA_ENABLE;
    }
    if state.gamma_lut.is_some() && !state.c8_planes {
        mode |= POST_CSC_GAMMA_ENABLE;
    }
    if state
        .gamma_lut
        .as_ref()
        .is_none_or(|lut| lut.entries.len() == LEGACY_LUT_LENGTH)
    {
        mode |= GAMMA_MODE_MODE_8BIT;
    } else if state.display_ver >= 13 {
        mode |= GAMMA_MODE_MODE_10BIT;
    } else {
        mode |= GAMMA_MODE_MODE_12BIT_MULTI_SEG;
    }
    mode
}
// upstream: intel_color.c icl_csc_mode()
pub fn icl_csc_mode(state: &ColorState) -> u32 {
    (if state.ctm.is_some() {
        ICL_CSC_ENABLE
    } else {
        0
    }) | (if state.output_format != OutputFormat::Rgb || state.limited_color_range {
        ICL_OUTPUT_CSC_ENABLE
    } else {
        0
    })
}

// ---- Hardware readback helpers. Register reads remain a framework boundary;
// palette decoding, indexing, and state dispatch are translated here. ----
/// Exercise the algorithm-side atomic check operations in upstream order.
/// Blob-size policy is validated by the DRM framework callback; mode selection,
/// CSC assignment, and LUT-state decisions are computed locally.
// upstream: intel_color.c icl_color_check()
pub fn icl_color_check<I: ColorIo>(io: &mut I, state: &mut ColorState) -> i32 {
    if check_luts(io, state).is_err() {
        return -22;
    }
    state.gamma_mode = icl_gamma_mode(state);
    state.csc_mode = icl_csc_mode(state);
    intel_assign_luts(state);
    icl_assign_csc(state);
    state.preload_luts = io.can_preload_color_luts();
    0
}

// upstream: intel_color.c i9xx_post_csc_lut_precision()
pub fn i9xx_post_csc_lut_precision(state: &ColorState) -> u8 {
    if !state.gamma_enable && !state.c8_planes {
        return 0;
    }
    match state.gamma_mode {
        GAMMA_MODE_MODE_8BIT => 8,
        GAMMA_MODE_MODE_10BIT => 10,
        _ => 0,
    }
}
// upstream: intel_color.c i9xx_pre_csc_lut_precision()
pub fn i9xx_pre_csc_lut_precision(_: &ColorState) -> u8 {
    0
}
// upstream: intel_color.c i965_post_csc_lut_precision()
pub fn i965_post_csc_lut_precision(state: &ColorState) -> u8 {
    if !state.gamma_enable && !state.c8_planes {
        return 0;
    }
    match state.gamma_mode {
        GAMMA_MODE_MODE_8BIT => 8,
        GAMMA_MODE_MODE_10BIT => 16,
        _ => 0,
    }
}
// upstream: intel_color.c ilk_gamma_mode_precision()
pub fn ilk_gamma_mode_precision(mode: u32) -> u8 {
    match mode {
        GAMMA_MODE_MODE_8BIT => 8,
        GAMMA_MODE_MODE_10BIT => 10,
        _ => 0,
    }
}
// upstream: intel_color.c ilk_has_post_csc_lut()
pub fn ilk_has_post_csc_lut(state: &ColorState) -> bool {
    state.c8_planes || (state.gamma_enable && state.csc_mode & CSC_POSITION_BEFORE_GAMMA != 0)
}
// upstream: intel_color.c ilk_has_pre_csc_lut()
pub fn ilk_has_pre_csc_lut(state: &ColorState) -> bool {
    state.gamma_enable && state.csc_mode & CSC_POSITION_BEFORE_GAMMA == 0
}
// upstream: intel_color.c ilk_post_csc_lut_precision()
pub fn ilk_post_csc_lut_precision(state: &ColorState) -> u8 {
    if ilk_has_post_csc_lut(state) {
        ilk_gamma_mode_precision(state.gamma_mode)
    } else {
        0
    }
}
// upstream: intel_color.c ilk_pre_csc_lut_precision()
pub fn ilk_pre_csc_lut_precision(state: &ColorState) -> u8 {
    if ilk_has_pre_csc_lut(state) {
        ilk_gamma_mode_precision(state.gamma_mode)
    } else {
        0
    }
}
// upstream: intel_color.c ivb_post_csc_lut_precision()
pub fn ivb_post_csc_lut_precision(state: &ColorState) -> u8 {
    if state.gamma_enable && state.gamma_mode == GAMMA_MODE_MODE_SPLIT {
        10
    } else {
        ilk_post_csc_lut_precision(state)
    }
}
// upstream: intel_color.c ivb_pre_csc_lut_precision()
pub fn ivb_pre_csc_lut_precision(state: &ColorState) -> u8 {
    if state.gamma_enable && state.gamma_mode == GAMMA_MODE_MODE_SPLIT {
        10
    } else {
        ilk_pre_csc_lut_precision(state)
    }
}
// upstream: intel_color.c chv_post_csc_lut_precision()
pub fn chv_post_csc_lut_precision(state: &ColorState) -> u8 {
    if state.cgm_mode & CGM_PIPE_MODE_GAMMA != 0 {
        10
    } else {
        i965_post_csc_lut_precision(state)
    }
}
// upstream: intel_color.c chv_pre_csc_lut_precision()
pub fn chv_pre_csc_lut_precision(state: &ColorState) -> u8 {
    if state.cgm_mode & CGM_PIPE_MODE_DEGAMMA != 0 {
        14
    } else {
        0
    }
}
// upstream: intel_color.c glk_post_csc_lut_precision()
pub fn glk_post_csc_lut_precision(state: &ColorState) -> u8 {
    if !state.gamma_enable && !state.c8_planes {
        0
    } else {
        ilk_gamma_mode_precision(state.gamma_mode)
    }
}
// upstream: intel_color.c glk_pre_csc_lut_precision()
pub fn glk_pre_csc_lut_precision(state: &ColorState) -> u8 {
    if state.csc_enable { 16 } else { 0 }
}

// upstream: intel_color.c icl_has_post_csc_lut()
pub fn icl_has_post_csc_lut(state: &ColorState) -> bool {
    state.c8_planes || state.gamma_mode & POST_CSC_GAMMA_ENABLE != 0
}
// upstream: intel_color.c icl_has_pre_csc_lut()
pub fn icl_has_pre_csc_lut(state: &ColorState) -> bool {
    state.gamma_mode & PRE_CSC_GAMMA_ENABLE != 0
}
// upstream: intel_color.c icl_post_csc_lut_precision()
pub fn icl_post_csc_lut_precision(state: &ColorState) -> u8 {
    if !icl_has_post_csc_lut(state) {
        return 0;
    }
    match state.gamma_mode & GAMMA_MODE_MODE_MASK {
        GAMMA_MODE_MODE_8BIT => 8,
        GAMMA_MODE_MODE_10BIT => 10,
        GAMMA_MODE_MODE_12BIT_MULTI_SEG => 16,
        _ => 0,
    }
}
// upstream: intel_color.c icl_pre_csc_lut_precision()
pub fn icl_pre_csc_lut_precision(state: &ColorState) -> u8 {
    if icl_has_pre_csc_lut(state) { 16 } else { 0 }
}

// upstream: intel_color.c err_check()
pub fn err_check(a: ColorLut, b: ColorLut, error: u32) -> bool {
    (a.red.abs_diff(b.red) as u32 <= error)
        && (a.green.abs_diff(b.green) as u32 <= error)
        && (a.blue.abs_diff(b.blue) as u32 <= error)
}
// upstream: intel_color.c intel_lut_entries_equal()
pub fn intel_lut_entries_equal(
    a: &[ColorLut],
    b: &[ColorLut],
    lut_size: usize,
    error: u32,
) -> bool {
    if a.len() < lut_size || b.len() < lut_size {
        return false;
    }
    (0..lut_size).all(|i| err_check(a[i], b[i], error))
}
// upstream: intel_color.c intel_lut_equal()
pub fn intel_lut_equal(
    a: Option<&LutBlob<ColorLut>>,
    b: Option<&LutBlob<ColorLut>>,
    check_size: usize,
    precision: u8,
) -> bool {
    if a.is_some() != b.is_some() || (a.is_some() != (precision != 0)) {
        return false;
    }
    let Some(a) = a else {
        return true;
    };
    let Some(b) = b else {
        return false;
    };
    if check_size > a.entries.len() || check_size > b.entries.len() {
        return false;
    }
    let err = if precision == 0 {
        0
    } else {
        0xffffu32 >> precision
    };
    let count = if check_size == 0 {
        a.entries.len()
    } else {
        check_size
    };
    intel_lut_entries_equal(&a.entries, &b.entries, count, err)
}

// upstream: intel_color.c i9xx_lut_equal()
pub fn i9xx_lut_equal(
    state: &ColorState,
    a: Option<&LutBlob<ColorLut>>,
    b: Option<&LutBlob<ColorLut>>,
    pre: bool,
) -> bool {
    if pre {
        return intel_lut_equal(a, b, 0, i9xx_pre_csc_lut_precision(state));
    }
    let check = if state.gamma_mode == GAMMA_MODE_MODE_10BIT {
        128
    } else {
        0
    };
    intel_lut_equal(a, b, check, i9xx_post_csc_lut_precision(state))
}
// upstream: intel_color.c i965_lut_equal()
pub fn i965_lut_equal(
    state: &ColorState,
    a: Option<&LutBlob<ColorLut>>,
    b: Option<&LutBlob<ColorLut>>,
    pre: bool,
) -> bool {
    intel_lut_equal(
        a,
        b,
        0,
        if pre {
            i9xx_pre_csc_lut_precision(state)
        } else {
            i965_post_csc_lut_precision(state)
        },
    )
}
// upstream: intel_color.c chv_lut_equal()
pub fn chv_lut_equal(
    state: &ColorState,
    a: Option<&LutBlob<ColorLut>>,
    b: Option<&LutBlob<ColorLut>>,
    pre: bool,
) -> bool {
    intel_lut_equal(
        a,
        b,
        0,
        if pre {
            chv_pre_csc_lut_precision(state)
        } else {
            chv_post_csc_lut_precision(state)
        },
    )
}
// upstream: intel_color.c ilk_lut_equal()
pub fn ilk_lut_equal(
    state: &ColorState,
    a: Option<&LutBlob<ColorLut>>,
    b: Option<&LutBlob<ColorLut>>,
    pre: bool,
) -> bool {
    intel_lut_equal(
        a,
        b,
        0,
        if pre {
            ilk_pre_csc_lut_precision(state)
        } else {
            ilk_post_csc_lut_precision(state)
        },
    )
}
// upstream: intel_color.c ivb_lut_equal()
pub fn ivb_lut_equal(
    state: &ColorState,
    a: Option<&LutBlob<ColorLut>>,
    b: Option<&LutBlob<ColorLut>>,
    pre: bool,
) -> bool {
    intel_lut_equal(
        a,
        b,
        0,
        if pre {
            ivb_pre_csc_lut_precision(state)
        } else {
            ivb_post_csc_lut_precision(state)
        },
    )
}
// upstream: intel_color.c glk_lut_equal()
pub fn glk_lut_equal(
    state: &ColorState,
    a: Option<&LutBlob<ColorLut>>,
    b: Option<&LutBlob<ColorLut>>,
    pre: bool,
) -> bool {
    intel_lut_equal(
        a,
        b,
        0,
        if pre {
            glk_pre_csc_lut_precision(state)
        } else {
            glk_post_csc_lut_precision(state)
        },
    )
}
// upstream: intel_color.c icl_lut_equal()
pub fn icl_lut_equal(
    state: &ColorState,
    a: Option<&LutBlob<ColorLut>>,
    b: Option<&LutBlob<ColorLut>>,
    pre: bool,
) -> bool {
    if pre {
        return intel_lut_equal(a, b, 0, icl_pre_csc_lut_precision(state));
    }
    let check = if state.gamma_mode & GAMMA_MODE_MODE_MASK == GAMMA_MODE_MODE_12BIT_MULTI_SEG {
        9
    } else {
        0
    };
    intel_lut_equal(a, b, check, icl_post_csc_lut_precision(state))
}
pub fn intel_color_lut_equal(
    state: &ColorState,
    a: Option<&LutBlob<ColorLut>>,
    b: Option<&LutBlob<ColorLut>>,
    pre: bool,
    icl_path: bool,
) -> bool {
    if !pre && state.c8_planes {
        return true;
    }
    if icl_path {
        icl_lut_equal(state, a, b, pre)
    } else {
        glk_lut_equal(state, a, b, pre)
    }
}

// upstream: intel_color.c i9xx_read_lut_8()
pub fn i9xx_read_lut_8<I: ColorIo>(io: &mut I, regs: GammaLutRegisters) -> LutBlob<ColorLut> {
    let mut lut = Vec::with_capacity(LEGACY_LUT_LENGTH);
    for i in 0..LEGACY_LUT_LENGTH {
        let value = io.read_register(regs.legacy_palette_base + i as u32 * 4);
        let mut color = ColorLut::default();
        i9xx_lut_8_pack(&mut color, value);
        lut.push(color);
    }
    LutBlob { entries: lut }
}
// upstream: intel_color.c i9xx_read_lut_10()
pub fn i9xx_read_lut_10<I: ColorIo>(
    io: &mut I,
    regs: GammaLutRegisters,
    lut_size: usize,
) -> LutBlob<ColorLut> {
    let mut lut = vec![ColorLut::default(); lut_size];
    if lut_size == 0 {
        return LutBlob { entries: lut };
    }
    let mut ldw = 0;
    let mut udw = 0;
    for i in 0..lut_size - 1 {
        ldw = io.read_register(regs.legacy_palette_base + (2 * i) as u32 * 4);
        udw = io.read_register(regs.legacy_palette_base + (2 * i + 1) as u32 * 4);
        i9xx_lut_10_pack(&mut lut[i], ldw, udw);
    }
    i9xx_lut_10_pack_slope(&mut lut[lut_size - 1], ldw, udw);
    LutBlob { entries: lut }
}
// upstream: intel_color.c i9xx_read_luts()
pub fn i9xx_read_luts<I: ColorIo>(io: &mut I, regs: GammaLutRegisters, state: &mut ColorState) {
    if !state.gamma_enable && !state.c8_planes {
        return;
    }
    state.post_csc_lut = match state.gamma_mode {
        GAMMA_MODE_MODE_8BIT => Some(i9xx_read_lut_8(io, regs)),
        GAMMA_MODE_MODE_10BIT => Some(i9xx_read_lut_10(io, regs, state.gamma_lut_size)),
        _ => None,
    };
}
// upstream: intel_color.c i965_read_lut_10p6()
pub fn i965_read_lut_10p6<I: ColorIo>(
    io: &mut I,
    regs: GammaLutRegisters,
    lut_size: usize,
) -> LutBlob<ColorLut> {
    let mut lut = vec![ColorLut::default(); lut_size];
    if lut_size == 0 {
        return LutBlob { entries: lut };
    }
    for i in 0..lut_size - 1 {
        let ldw = io.read_register(regs.legacy_palette_base + (2 * i) as u32 * 4);
        let udw = io.read_register(regs.legacy_palette_base + (2 * i + 1) as u32 * 4);
        i965_lut_10p6_pack(&mut lut[i], ldw, udw);
    }
    lut[lut_size - 1] = ColorLut {
        red: i965_lut_11p6_max_pack(io.read_register(regs.gc_max[0])) as u16,
        green: i965_lut_11p6_max_pack(io.read_register(regs.gc_max[1])) as u16,
        blue: i965_lut_11p6_max_pack(io.read_register(regs.gc_max[2])) as u16,
    };
    LutBlob { entries: lut }
}
// upstream: intel_color.c i965_read_luts()
pub fn i965_read_luts<I: ColorIo>(io: &mut I, regs: GammaLutRegisters, state: &mut ColorState) {
    if !state.gamma_enable && !state.c8_planes {
        return;
    }
    state.post_csc_lut = match state.gamma_mode {
        GAMMA_MODE_MODE_8BIT => Some(i9xx_read_lut_8(io, regs)),
        GAMMA_MODE_MODE_10BIT => Some(i965_read_lut_10p6(io, regs, state.gamma_lut_size)),
        _ => None,
    };
}
// upstream: intel_color.c chv_read_cgm_degamma()
pub fn chv_read_cgm_degamma<I: ColorIo>(io: &mut I, base: u32, size: usize) -> LutBlob<ColorLut> {
    let mut lut = Vec::with_capacity(size);
    for i in 0..size {
        let ldw = io.read_register(base + i as u32 * 8);
        let udw = io.read_register(base + i as u32 * 8 + 4);
        let mut e = ColorLut::default();
        chv_cgm_degamma_pack(&mut e, ldw, udw);
        lut.push(e);
    }
    LutBlob { entries: lut }
}
// upstream: intel_color.c chv_read_cgm_gamma()
pub fn chv_read_cgm_gamma<I: ColorIo>(io: &mut I, base: u32, size: usize) -> LutBlob<ColorLut> {
    let mut lut = Vec::with_capacity(size);
    for i in 0..size {
        let ldw = io.read_register(base + i as u32 * 8);
        let udw = io.read_register(base + i as u32 * 8 + 4);
        let mut e = ColorLut::default();
        chv_cgm_gamma_pack(&mut e, ldw, udw);
        lut.push(e);
    }
    LutBlob { entries: lut }
}
// upstream: intel_color.c chv_get_config()
pub fn chv_get_config<I: ColorIo>(
    io: &mut I,
    state: &mut ColorState,
    cgm_mode_reg: u32,
    regs: CommitRegisters,
    plane_control: u32,
    gamma_mask: u32,
    csc_mask: u32,
    gmch: bool,
) {
    state.cgm_mode = io.read_register(cgm_mode_reg);
    i9xx_get_config(io, state, plane_control, gamma_mask, csc_mask, gmch);
}
// upstream: intel_color.c chv_read_luts()
pub fn chv_read_luts<I: ColorIo>(
    io: &mut I,
    state: &mut ColorState,
    cgm_deg_base: u32,
    cgm_gamma_base: u32,
    legacy_regs: GammaLutRegisters,
) {
    if state.cgm_mode & CGM_PIPE_MODE_DEGAMMA != 0 {
        state.pre_csc_lut = Some(chv_read_cgm_degamma(
            io,
            cgm_deg_base,
            state.degamma_lut_size,
        ));
    }
    if state.cgm_mode & CGM_PIPE_MODE_GAMMA != 0 {
        state.post_csc_lut = Some(chv_read_cgm_gamma(io, cgm_gamma_base, state.gamma_lut_size));
    } else {
        i965_read_luts(io, legacy_regs, state);
    }
}
// upstream: intel_color.c ilk_read_lut_8()
pub fn ilk_read_lut_8<I: ColorIo>(io: &mut I, regs: GammaLutRegisters) -> LutBlob<ColorLut> {
    let mut lut = Vec::with_capacity(LEGACY_LUT_LENGTH);
    for i in 0..LEGACY_LUT_LENGTH {
        let mut e = ColorLut::default();
        i9xx_lut_8_pack(
            &mut e,
            io.read_register(regs.legacy_palette_base + i as u32 * 4),
        );
        lut.push(e);
    }
    LutBlob { entries: lut }
}
// upstream: intel_color.c ilk_read_lut_10()
pub fn ilk_read_lut_10<I: ColorIo>(
    io: &mut I,
    regs: GammaLutRegisters,
    lut_size: usize,
) -> LutBlob<ColorLut> {
    let mut lut = Vec::with_capacity(lut_size);
    for i in 0..lut_size {
        let mut e = ColorLut::default();
        ilk_lut_10_pack(
            &mut e,
            io.read_register(regs.legacy_palette_base + i as u32 * 4),
        );
        lut.push(e);
    }
    LutBlob { entries: lut }
}
// upstream: intel_color.c ilk_get_config()
pub fn ilk_get_config<I: ColorIo>(
    io: &mut I,
    state: &mut ColorState,
    regs: CommitRegisters,
    plane_control: u32,
    gamma_mask: u32,
    csc_mask: u32,
    gmch: bool,
) {
    state.csc_mode = ilk_read_csc_mode(io, regs.csc_mode);
    i9xx_get_config(io, state, plane_control, gamma_mask, csc_mask, gmch);
}
// upstream: intel_color.c ilk_read_luts()
pub fn ilk_read_luts<I: ColorIo>(io: &mut I, regs: GammaLutRegisters, state: &mut ColorState) {
    if !state.gamma_enable && !state.c8_planes {
        return;
    }
    let path_post = ilk_has_post_csc_lut(state);
    let result = match state.gamma_mode {
        GAMMA_MODE_MODE_8BIT => Some(ilk_read_lut_8(io, regs)),
        GAMMA_MODE_MODE_10BIT => Some(ilk_read_lut_10(io, regs, state.gamma_lut_size)),
        _ => None,
    };
    if path_post {
        state.post_csc_lut = result;
    } else {
        state.pre_csc_lut = result;
    }
}
// upstream: intel_color.c ivb_read_lut_10()
pub fn ivb_read_lut_10<I: ColorIo>(
    io: &mut I,
    regs: GammaLutRegisters,
    prec_index: u32,
) -> LutBlob<ColorLut> {
    let lut_size = ivb_lut_10_size(prec_index);
    let mut lut = Vec::with_capacity(lut_size);
    for i in 0..lut_size {
        ilk_lut_write(io, regs.precision_index, prec_index + i as u32);
        let mut e = ColorLut::default();
        ilk_lut_10_pack(&mut e, io.read_register(regs.precision_data));
        lut.push(e);
    }
    ilk_lut_write(io, regs.precision_index, 0);
    LutBlob { entries: lut }
}
// upstream: intel_color.c ivb_read_luts()
pub fn ivb_read_luts<I: ColorIo>(io: &mut I, regs: GammaLutRegisters, state: &mut ColorState) {
    if !state.gamma_enable && !state.c8_planes {
        return;
    }
    match state.gamma_mode {
        GAMMA_MODE_MODE_8BIT => {
            let lut = ilk_read_lut_8(io, regs);
            if ilk_has_post_csc_lut(state) {
                state.post_csc_lut = Some(lut)
            } else {
                state.pre_csc_lut = Some(lut)
            }
        }
        GAMMA_MODE_MODE_SPLIT => {
            state.pre_csc_lut = Some(ivb_read_lut_10(io, regs, PAL_PREC_SPLIT_MODE));
            state.post_csc_lut = Some(ivb_read_lut_10(io, regs, PAL_PREC_SPLIT_MODE | 512));
        }
        GAMMA_MODE_MODE_10BIT => {
            let lut = ivb_read_lut_10(io, regs, 0);
            if ilk_has_post_csc_lut(state) {
                state.post_csc_lut = Some(lut)
            } else {
                state.pre_csc_lut = Some(lut)
            }
        }
        _ => {}
    }
}
// upstream: intel_color.c bdw_read_lut_10()
pub fn bdw_read_lut_10<I: ColorIo>(
    io: &mut I,
    regs: GammaLutRegisters,
    _lut_size: usize,
    prec_index: u32,
) -> LutBlob<ColorLut> {
    let lut_size = ivb_lut_10_size(prec_index);
    ilk_lut_write(io, regs.precision_index, prec_index);
    ilk_lut_write(
        io,
        regs.precision_index,
        PAL_PREC_AUTO_INCREMENT | prec_index,
    );
    let mut lut = Vec::with_capacity(lut_size);
    for _ in 0..lut_size {
        let mut e = ColorLut::default();
        ilk_lut_10_pack(&mut e, io.read_register(regs.precision_data));
        lut.push(e);
    }
    ilk_lut_write(io, regs.precision_index, 0);
    LutBlob { entries: lut }
}
// upstream: intel_color.c bdw_read_luts()
pub fn bdw_read_luts<I: ColorIo>(io: &mut I, regs: GammaLutRegisters, state: &mut ColorState) {
    if !state.gamma_enable && !state.c8_planes {
        return;
    }
    match state.gamma_mode {
        GAMMA_MODE_MODE_8BIT => {
            let x = ilk_read_lut_8(io, regs);
            if ilk_has_post_csc_lut(state) {
                state.post_csc_lut = Some(x)
            } else {
                state.pre_csc_lut = Some(x)
            }
        }
        GAMMA_MODE_MODE_SPLIT => {
            state.pre_csc_lut = Some(bdw_read_lut_10(io, regs, 512, PAL_PREC_SPLIT_MODE));
            state.post_csc_lut = Some(bdw_read_lut_10(io, regs, 512, PAL_PREC_SPLIT_MODE | 512));
        }
        GAMMA_MODE_MODE_10BIT => {
            let x = bdw_read_lut_10(io, regs, state.gamma_lut_size, 0);
            if ilk_has_post_csc_lut(state) {
                state.post_csc_lut = Some(x)
            } else {
                state.pre_csc_lut = Some(x)
            }
        }
        _ => {}
    }
}
// upstream: intel_color.c glk_read_degamma_lut()
pub fn glk_read_degamma_lut<I: ColorIo>(
    io: &mut I,
    regs: GammaLutRegisters,
    display_ver: u8,
    lut_size: usize,
) -> LutBlob<ColorLut> {
    ilk_lut_write(io, regs.pre_csc_index, 0);
    ilk_lut_write(io, regs.pre_csc_index, PAL_PREC_AUTO_INCREMENT);
    let mut lut = Vec::with_capacity(lut_size);
    for _ in 0..lut_size {
        let val = io.read_register(regs.pre_csc_data);
        let mut e = ColorLut::default();
        if display_ver >= 14 {
            mtl_degamma_lut_pack(&mut e, val);
        } else {
            glk_degamma_lut_pack(&mut e, val);
        }
        lut.push(e);
    }
    ilk_lut_write(io, regs.pre_csc_index, 0);
    LutBlob { entries: lut }
}
// upstream: intel_color.c glk_read_luts()
pub fn glk_read_luts<I: ColorIo>(io: &mut I, regs: GammaLutRegisters, state: &mut ColorState) {
    if state.csc_enable {
        state.pre_csc_lut = Some(glk_read_degamma_lut(
            io,
            regs,
            state.display_ver,
            state.degamma_lut_size,
        ));
    }
    if !state.gamma_enable && !state.c8_planes {
        return;
    }
    state.post_csc_lut = match state.gamma_mode {
        GAMMA_MODE_MODE_8BIT => Some(ilk_read_lut_8(io, regs)),
        GAMMA_MODE_MODE_10BIT => Some(bdw_read_lut_10(io, regs, state.gamma_lut_size, 0)),
        _ => None,
    };
}
// upstream: intel_color.c icl_read_lut_multi_segment()
pub fn icl_read_lut_multi_segment<I: ColorIo>(
    io: &mut I,
    regs: GammaLutRegisters,
    lut_size: usize,
) -> LutBlob<ColorLut> {
    let mut lut = vec![ColorLut::default(); lut_size];
    ilk_lut_write(io, regs.multi_segment_index, 0);
    ilk_lut_write(io, regs.multi_segment_index, PAL_PREC_AUTO_INCREMENT);
    for i in 0..9.min(lut_size) {
        let ldw = io.read_register(regs.multi_segment_data);
        let udw = io.read_register(regs.multi_segment_data);
        ilk_lut_12p4_pack(&mut lut[i], ldw, udw);
    }
    ilk_lut_write(io, regs.multi_segment_index, 0);
    // Hardware readback of fine and coarse segments is explicitly unreliable;
    // the Linux source intentionally leaves those blob entries untouched.
    LutBlob { entries: lut }
}

// upstream: intel_color.c icl_read_luts()
pub fn icl_read_luts<I: ColorIo>(io: &mut I, regs: GammaLutRegisters, state: &mut ColorState) {
    if icl_has_pre_csc_lut(state) {
        state.pre_csc_lut = Some(glk_read_degamma_lut(
            io,
            regs,
            state.display_ver,
            state.degamma_lut_size,
        ));
    }
    if !icl_has_post_csc_lut(state) {
        return;
    }
    state.post_csc_lut = match state.gamma_mode & GAMMA_MODE_MODE_MASK {
        GAMMA_MODE_MODE_8BIT => Some(ilk_read_lut_8(io, regs)),
        GAMMA_MODE_MODE_10BIT => Some(bdw_read_lut_10(io, regs, state.gamma_lut_size, 0)),
        GAMMA_MODE_MODE_12BIT_MULTI_SEG => {
            Some(icl_read_lut_multi_segment(io, regs, state.gamma_lut_size))
        }
        _ => None,
    };
}

// upstream: intel_color.c xelpd_load_plane_csc_matrix()
pub fn xelpd_load_plane_csc_matrix<I: ColorIo>(
    io: &mut I,
    hdr_plane: bool,
    ctm: Option<&[u64; 12]>,
    regs: PlaneCscRegisters,
) {
    let Some(ctm) = ctm.filter(|_| hdr_plane) else {
        return;
    };
    let (coeffs, post) = xelpd_plane_csc_coefficients(ctm);
    io.dsb_write(regs.coeff[0], (coeffs[0] as u32) << 16 | coeffs[1] as u32);
    io.dsb_write(regs.coeff[1], (coeffs[2] as u32) << 16);
    io.dsb_write(regs.coeff[2], (coeffs[3] as u32) << 16 | coeffs[4] as u32);
    io.dsb_write(regs.coeff[3], (coeffs[5] as u32) << 16);
    io.dsb_write(regs.coeff[4], (coeffs[6] as u32) << 16 | coeffs[7] as u32);
    io.dsb_write(regs.coeff[5], (coeffs[8] as u32) << 16);
    for register in regs.preoff {
        io.dsb_write(register, 0);
    }
    for i in 0..3 {
        io.dsb_write(regs.postoff[i], post[i] as u32);
    }
}

// upstream: intel_color.c xelpd_program_plane_pre_csc_lut()
pub fn program_xelpd_pre_csc<I: ColorIo>(
    io: &mut I,
    regs: LutRegisters,
    lut: Option<&[ColorLut32]>,
) {
    io.dsb_write(regs.index, regs.auto_increment);
    for v in xelpd_pre_csc_values(lut) {
        io.dsb_write(regs.data, v);
    }
    io.dsb_write(regs.index, 0);
}
// upstream: intel_color.c xelpd_program_plane_post_csc_lut()
pub fn program_xelpd_post_csc<I: ColorIo>(
    io: &mut I,
    regs: LutRegisters,
    lut: Option<&[ColorLut32]>,
    segment_index: u32,
) {
    io.dsb_write(regs.index, regs.auto_increment);
    io.dsb_write(segment_index, regs.auto_increment);
    for v in xelpd_post_csc_values(lut) {
        io.dsb_write(regs.data, v);
    }
    io.dsb_write(regs.index, 0);
    io.dsb_write(segment_index, 0);
}

// upstream: intel_color.c xelpd_plane_load_luts()
pub fn xelpd_plane_load_luts<I: ColorIo>(
    io: &mut I,
    state: &ColorState,
    pre_regs: LutRegisters,
    post_regs: LutRegisters,
    post_segment_index: u32,
    pre: Option<&[ColorLut32]>,
    post: Option<&[ColorLut32]>,
) {
    if state.degamma_lut.is_some() {
        program_xelpd_pre_csc(io, pre_regs, pre);
    }
    if state.gamma_lut.is_some() {
        program_xelpd_post_csc(io, post_regs, post, post_segment_index);
    }
}

/// Source-order write helper for a full CSC matrix. Register packing is kept
/// here (six coefficient pair/single words, then the three post offsets).
// upstream: intel_color.c glk_3dlut_10()
pub fn glk_3dlut_10(color: ColorLut32) -> u32 {
    ((color.red >> 22) & 0x3ff)
        | (((color.green >> 22) & 0x3ff) << 10)
        | (((color.blue >> 22) & 0x3ff) << 20)
}

/// Pure generation of the source's linear LUT values for D12/13 plane color.
pub fn xelpd_pre_csc_values(lut: Option<&[ColorLut32]>) -> Vec<u32> {
    let mut values = Vec::with_capacity(131);
    match lut {
        Some(entries) => {
            for entry in entries.iter().take(128) {
                values.push(entry.green >> 8);
            }
            while values.len() < 131 {
                values.push(1 << 24);
            }
        }
        None => {
            for i in 0..128 {
                values.push((i * ((1 << 24) - 1)) / 127);
            }
            while values.len() < 131 {
                values.push(1 << 24);
            }
        }
    }
    values
}
pub fn xelpd_post_csc_values(lut: Option<&[ColorLut32]>) -> Vec<u32> {
    let mut values = Vec::with_capacity(35);
    match lut {
        Some(entries) => {
            for entry in entries.iter().take(32) {
                values.push(entry.green >> 8);
            }
        }
        None => {
            for i in 0..32 {
                values.push((i * ((1 << 24) - 1)) / 31);
            }
        }
    }
    while values.len() < 35 {
        values.push(1 << 24);
    }
    values
}

/// Emit D13/D12 plane-pre CSC writes. Addresses are passed in as a framework
/// mapping; auto-increment/index sequencing matches the upstream function.
// upstream: intel_color.c glk_load_lut_3d()
pub fn glk_load_lut_3d<I: ColorIo>(
    io: &mut I,
    dsb_present: bool,
    control: u32,
    regs: LutRegisters,
    ready: u32,
    lut: &[ColorLut32],
) {
    if !dsb_present && !io.wait_lut_ready(control, ready) {
        io.report_not_ready("3D LUT not ready, not loading LUTs");
        return;
    }
    io.dsb_write(regs.index, regs.auto_increment);
    for entry in lut {
        io.dsb_write(regs.data, glk_3dlut_10(*entry));
    }
    io.dsb_write(regs.index, 0);
}

// upstream: intel_color.c glk_lut_3d_commit()
pub fn glk_lut_3d_commit<I: ColorIo>(
    io: &mut I,
    dsb_present: bool,
    control: u32,
    enable: bool,
    ready: u32,
    enable_bits: u32,
) {
    if !dsb_present && !io.wait_lut_ready(control, ready) {
        io.report_not_ready("3D LUT not ready, not committing change");
        return;
    }
    io.dsb_write(control, if enable { enable_bits | ready } else { 0 });
}

/// Hardware paths are registered as explicit callbacks, never as a substitute
/// for algorithm execution. These select only the source's D12+ function set.
pub trait ColorHooks {
    type State;
    fn color_check(&mut self, state: &mut Self::State) -> i32;
    fn get_config(&mut self, state: &mut Self::State);
    fn lut_equal(
        &mut self,
        state: &Self::State,
        a: Option<&LutBlob<ColorLut>>,
        b: Option<&LutBlob<ColorLut>>,
        pre: bool,
    ) -> bool;
    fn load_luts(&mut self, state: &Self::State);
    fn read_luts(&mut self, state: &mut Self::State);
    fn read_csc(&mut self, state: &mut Self::State);
    fn commit_noarm(&mut self, state: &Self::State);
    fn commit_arm(&mut self, state: &Self::State);
    fn post_update(&mut self, state: &Self::State);
    fn load_plane_csc_matrix(&mut self, state: &Self::State);
    fn load_plane_luts(&mut self, state: &Self::State);
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayColorPath {
    Chv,
    Vlv,
    I965,
    I9xx,
    TglD12Plus,
    IclD11,
    Glk,
    Skl,
    Bdw,
    Hsw,
    Ivb,
    Ilk,
}
pub fn intel_color_path(display_ver: u8) -> DisplayColorPath {
    if display_ver >= 12 {
        DisplayColorPath::TglD12Plus
    } else {
        DisplayColorPath::IclD11
    }
}
// upstream: intel_color.c intel_color_plane_commit_arm()
pub fn intel_color_plane_commit_arm<I: ColorIo>(
    io: &mut I,
    dsb: bool,
    display_ver: u8,
    pipe: u8,
    control: u32,
    ready: u32,
    enable_bits: u32,
    has_3dlut: bool,
) {
    if intel_color_crtc_has_3dlut(display_ver, pipe) {
        glk_lut_3d_commit(io, dsb, control, has_3dlut, ready, enable_bits);
    }
}

/// Equivalent to the source's `intel_color_funcs` vtable assignments. The
/// enum-valued entries identify exactly which generation-specific routines
/// the framework invokes; they are not implementations of those algorithms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorCheckRoutine {
    Chv,
    Vlv,
    I9xx,
    Ilk,
    Ivb,
    Glk,
    Icl,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorLutRoutine {
    Chv,
    Vlv,
    I9xx,
    I965,
    Ilk,
    Ivb,
    Bdw,
    Glk,
    Icl,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorCscRoutine {
    None,
    Ilk,
    Skl,
    Icl,
    Vlv,
    Chv,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorConfigRoutine {
    I9xx,
    Ilk,
    Hsw,
    Skl,
    Chv,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColorFunctionSet {
    pub check: ColorCheckRoutine,
    pub commit_noarm: Option<ColorCheckRoutine>,
    pub commit_arm: ColorCheckRoutine,
    pub post_update: bool,
    pub load: ColorLutRoutine,
    pub read: ColorLutRoutine,
    pub csc: ColorCscRoutine,
    pub config: ColorConfigRoutine,
    pub plane_csc: bool,
    pub plane_luts: bool,
}
// Equivalent data representation for the source static color-function tables.
pub fn intel_color_function_set(path: DisplayColorPath) -> ColorFunctionSet {
    use ColorCheckRoutine as C;
    use ColorConfigRoutine as G;
    use ColorCscRoutine as S;
    use ColorLutRoutine as L;
    match path {
        DisplayColorPath::Chv => ColorFunctionSet {
            check: C::Chv,
            commit_noarm: None,
            commit_arm: C::I9xx,
            post_update: false,
            load: L::Chv,
            read: L::Chv,
            csc: S::Chv,
            config: G::Chv,
            plane_csc: false,
            plane_luts: false,
        },
        DisplayColorPath::Vlv => ColorFunctionSet {
            check: C::Vlv,
            commit_noarm: None,
            commit_arm: C::I9xx,
            post_update: false,
            load: L::Vlv,
            read: L::I965,
            csc: S::Vlv,
            config: G::I9xx,
            plane_csc: false,
            plane_luts: false,
        },
        DisplayColorPath::I965 => ColorFunctionSet {
            check: C::I9xx,
            commit_noarm: None,
            commit_arm: C::I9xx,
            post_update: false,
            load: L::I965,
            read: L::I965,
            csc: S::None,
            config: G::I9xx,
            plane_csc: false,
            plane_luts: false,
        },
        DisplayColorPath::I9xx => ColorFunctionSet {
            check: C::I9xx,
            commit_noarm: None,
            commit_arm: C::I9xx,
            post_update: false,
            load: L::I9xx,
            read: L::I9xx,
            csc: S::None,
            config: G::I9xx,
            plane_csc: false,
            plane_luts: false,
        },
        DisplayColorPath::TglD12Plus => ColorFunctionSet {
            check: C::Icl,
            commit_noarm: Some(C::Icl),
            commit_arm: C::Icl,
            post_update: false,
            load: L::Icl,
            read: L::Icl,
            csc: S::Icl,
            config: G::Skl,
            plane_csc: true,
            plane_luts: true,
        },
        DisplayColorPath::IclD11 => ColorFunctionSet {
            check: C::Icl,
            commit_noarm: Some(C::Icl),
            commit_arm: C::Icl,
            post_update: true,
            load: L::Icl,
            read: L::Icl,
            csc: S::Icl,
            config: G::Skl,
            plane_csc: false,
            plane_luts: false,
        },
        DisplayColorPath::Glk => ColorFunctionSet {
            check: C::Glk,
            commit_noarm: Some(C::Ilk),
            commit_arm: C::Icl,
            post_update: false,
            load: L::Glk,
            read: L::Glk,
            csc: S::Skl,
            config: G::Skl,
            plane_csc: false,
            plane_luts: false,
        },
        DisplayColorPath::Skl => ColorFunctionSet {
            check: C::Ivb,
            commit_noarm: Some(C::Ilk),
            commit_arm: C::Icl,
            post_update: false,
            load: L::Bdw,
            read: L::Bdw,
            csc: S::Skl,
            config: G::Skl,
            plane_csc: false,
            plane_luts: false,
        },
        DisplayColorPath::Bdw => ColorFunctionSet {
            check: C::Ivb,
            commit_noarm: Some(C::Ilk),
            commit_arm: C::Ilk,
            post_update: false,
            load: L::Bdw,
            read: L::Bdw,
            csc: S::Ilk,
            config: G::Hsw,
            plane_csc: false,
            plane_luts: false,
        },
        DisplayColorPath::Hsw => ColorFunctionSet {
            check: C::Ivb,
            commit_noarm: Some(C::Ilk),
            commit_arm: C::Ilk,
            post_update: false,
            load: L::Ivb,
            read: L::Ivb,
            csc: S::Ilk,
            config: G::Hsw,
            plane_csc: false,
            plane_luts: false,
        },
        DisplayColorPath::Ivb => ColorFunctionSet {
            check: C::Ivb,
            commit_noarm: Some(C::Ilk),
            commit_arm: C::Ilk,
            post_update: false,
            load: L::Ivb,
            read: L::Ivb,
            csc: S::Ilk,
            config: G::Ilk,
            plane_csc: false,
            plane_luts: false,
        },
        DisplayColorPath::Ilk => ColorFunctionSet {
            check: C::Ilk,
            commit_noarm: Some(C::Ilk),
            commit_arm: C::Ilk,
            post_update: false,
            load: L::Ilk,
            read: L::Ilk,
            csc: S::Ilk,
            config: G::Ilk,
            plane_csc: false,
            plane_luts: false,
        },
    }
}
// upstream: intel_color.c intel_color_load_plane_csc_matrix()
pub fn intel_color_load_plane_csc_matrix<H: ColorHooks>(
    hooks: &mut H,
    state: &H::State,
    has_ctm: bool,
) {
    if has_ctm {
        hooks.load_plane_csc_matrix(state);
    }
}
// upstream: intel_color.c intel_color_load_plane_luts()
pub fn intel_color_load_plane_luts<H: ColorHooks>(
    hooks: &mut H,
    state: &H::State,
    has_degamma: bool,
    has_gamma: bool,
) {
    if has_degamma || has_gamma {
        hooks.load_plane_luts(state);
    }
}
// upstream: intel_color.c intel_color_crtc_has_3dlut()
pub fn intel_color_crtc_has_3dlut(display_ver: u8, pipe: u8) -> bool {
    display_ver >= 12 && pipe <= 1
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lut_precision_modes_follow_display_generation() {
        let mut state = ColorState {
            display_ver: 12,
            gamma_lut: Some(LutBlob {
                entries: vec![ColorLut::default(); 1024],
            }),
            ..Default::default()
        };
        assert_eq!(
            icl_gamma_mode(&state) & GAMMA_MODE_MODE_MASK,
            GAMMA_MODE_MODE_12BIT_MULTI_SEG
        );
        state.display_ver = 13;
        assert_eq!(
            icl_gamma_mode(&state) & GAMMA_MODE_MODE_MASK,
            GAMMA_MODE_MODE_10BIT
        );
    }
    #[test]
    fn xe_lpd_lut_programming_fills_clamp_entries() {
        let pre = xelpd_pre_csc_values(None);
        assert_eq!(pre.len(), 131);
        assert!(pre[128..].iter().all(|v| *v == 1 << 24));
        let post = xelpd_post_csc_values(None);
        assert_eq!(post.len(), 35);
        assert!(post[32..].iter().all(|v| *v == 1 << 24));
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PlaneCscRegisters {
    pub coeff: [u32; 6],
    pub preoff: [u32; 3],
    pub postoff: [u32; 3],
}

/// Address packing for plane CSC is supplied by the register framework while
/// the source's coefficient conversion and exact register payload ordering
/// remain in this module.
// upstream: intel_color.c intel_color_load_3dlut()
pub fn intel_color_load_3dlut<I: ColorIo>(
    io: &mut I,
    dsb: bool,
    display_ver: u8,
    pipe: u8,
    control: u32,
    regs: LutRegisters,
    ready: u32,
    lut: Option<&[ColorLut32]>,
) {
    if !intel_color_crtc_has_3dlut(display_ver, pipe) {
        return;
    }
    if let Some(lut) = lut {
        glk_load_lut_3d(io, dsb, control, regs, ready, lut);
    }
}
// upstream: intel_color.c intel_color_plane_program_pipeline()
pub fn intel_color_plane_pipeline(
    has_ctm: bool,
    has_degamma: bool,
    has_gamma: bool,
    has_3dlut: bool,
) -> PlaneColorOperations {
    PlaneColorOperations {
        load_csc: has_ctm,
        load_luts: has_degamma || has_gamma,
        load_3dlut: has_3dlut,
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlaneColorOperations {
    pub load_csc: bool,
    pub load_luts: bool,
    pub load_3dlut: bool,
}

#[cfg(test)]
mod d12_path_tests {
    use super::*;
    #[test]
    fn display12_and_13_select_distinct_gamma_tables() {
        let lut = Some(LutBlob {
            entries: vec![ColorLut::default(); 1024],
        });
        let state12 = ColorState {
            display_ver: 12,
            gamma_lut: lut.clone(),
            ..Default::default()
        };
        let state13 = ColorState {
            display_ver: 13,
            gamma_lut: lut,
            ..Default::default()
        };
        assert_eq!(
            icl_gamma_mode(&state12) & GAMMA_MODE_MODE_MASK,
            GAMMA_MODE_MODE_12BIT_MULTI_SEG
        );
        assert_eq!(
            icl_gamma_mode(&state13) & GAMMA_MODE_MODE_MASK,
            GAMMA_MODE_MODE_10BIT
        );
    }
    #[test]
    fn xe_lpd_plane_csc_skips_post_offsets_in_matrix_coefficients() {
        let input = [
            CTM_COEFF_1_0,
            0,
            0,
            0,
            0,
            CTM_COEFF_1_0,
            0,
            0,
            0,
            0,
            CTM_COEFF_1_0,
            0,
        ];
        let (coeffs, post) = xelpd_plane_csc_coefficients(&input);
        assert_eq!(coeffs[0], 0x7800);
        assert_eq!(coeffs[4], 0x7800);
        assert_eq!(coeffs[8], 0x7800);
        assert_eq!(post, [0; 3]);
    }
}

// ---- Pipe CSC operations in the order used by the upstream source ----
#[derive(Clone, Copy, Debug)]
pub struct PipeCscRegisterMap {
    pub preoff: [u32; 3],
    pub coeff: [u32; 6],
    pub postoff: [u32; 3],
}

// upstream: intel_color.c intel_color_crtc_init()
pub fn intel_color_crtc_init(
    display_ver: u8,
    pipe: u8,
    gamma_size: usize,
    degamma_size: usize,
) -> (usize, bool, usize) {
    let gamma = if display_ver == 3 && pipe == 0 {
        256
    } else {
        gamma_size
    };
    (degamma_size, display_ver >= 5, gamma)
}

// upstream: intel_color.c intel_color_init()
pub fn intel_color_init(display_ver: u8, degamma_size: usize) -> Option<LutBlob<ColorLut>> {
    if display_ver == 10 {
        Some(create_linear_lut(degamma_size))
    } else {
        None
    }
}
// upstream: intel_color.c intel_color_init_hooks()
pub fn intel_color_init_hooks(
    gmch: bool,
    display_ver: u8,
    cherryview: bool,
    valleyview: bool,
    haswell: bool,
) -> DisplayColorPath {
    intel_color_select_path(gmch, display_ver, cherryview, valleyview, haswell)
}
