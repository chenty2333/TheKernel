// SPDX-License-Identifier: MIT
// Copyright © 2006-2017 Intel Corporation
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL
// THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.
//
//! Ordered translation of Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_cdclk.c`.
//! Kernel register, PCI, PCODE, power-domain, DRM logging and atomic-framework
//! services are required backend hooks; this file supplies no successful or
//! no-op hardware implementation.
#![allow(
    dead_code,
    non_camel_case_types,
    non_snake_case,
    clippy::too_many_arguments
)]

use core::cmp::{max, min};

pub const I915_MAX_PIPES: usize = 4;
pub const INVALID_PIPE: i32 = -1;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IntelCdclkConfig {
    pub cdclk: i32,
    pub vco: i32,
    pub refclk: i32,
    pub bypass: i32,
    pub voltage_level: u8,
    pub waveform: u16,
    pub joined_mbus: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IntelCdclkAtomicState {
    pub logical: IntelCdclkConfig,
    pub actual: IntelCdclkConfig,
    pub dbuf_bw_min_cdclk: i32,
    pub min_cdclk: [i32; I915_MAX_PIPES],
    pub min_voltage_level: [u8; I915_MAX_PIPES],
    pub pipe: i32,
    pub force_min_cdclk: i32,
    pub enabled_pipes: u8,
    pub active_pipes: u8,
    pub disable_pipes: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CdclkPlatform {
    pub alderlake_p: bool,
    pub alderlake_p_raptorlake_u: bool,
    pub broadwell: bool,
    pub broadwell_ult: bool,
    pub broadwell_ulx: bool,
    pub broxton: bool,
    pub cherryview: bool,
    pub dg2: bool,
    pub elkhartlake: bool,
    pub g33: bool,
    pub g45: bool,
    pub geminilake: bool,
    pub gm45: bool,
    pub haswell: bool,
    pub haswell_ult: bool,
    pub i830: bool,
    pub i845g: bool,
    pub i85x: bool,
    pub i865g: bool,
    pub i915g: bool,
    pub i915gm: bool,
    pub i945g: bool,
    pub i945gm: bool,
    pub i965g: bool,
    pub i965gm: bool,
    pub ironlake: bool,
    pub ivybridge: bool,
    pub jasperlake: bool,
    pub mobile: bool,
    pub pineview: bool,
    pub rocketlake: bool,
    pub sandybridge: bool,
    pub skylake: bool,
    pub valleyview: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CdclkDisplayState {
    pub hw: IntelCdclkConfig,
    pub max_cdclk_freq: i32,
    pub max_dotclk_freq: i32,
    pub skl_preferred_vco_freq: i32,
    pub table: u32,
    pub funcs: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IntelDisplay {
    pub platform: CdclkPlatform,
    pub display_ver: u8,
    pub display_ver_full: u16,
    pub cdclk: CdclkDisplayState,
    pub atomic_cdclk: IntelCdclkAtomicState,
    pub rawclk_freq: u32,
    pub cdclk_ref: u32,
    pub cdclk_max: u32,
    pub cdclk_min: u32,
    pub pch_type: u32,
    pub has_dram: bool,
    pub has_psr: bool,
    pub joined_mbus_supported: bool,
    pub audio_enabled: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IntelCrtcState {
    pub pipe: i32,
    pub pixel_rate: i32,
    pub port_clock: i32,
    pub double_wide: bool,
    pub min_cdclk: i32,
    pub min_voltage_level: u8,
    pub plane_min_cdclk: i32,
    pub active: bool,
    pub enable: bool,
    pub joined_pipe_mask: u8,
    pub joiner_pipes: u8,
    pub pch_pipes: u8,
    pub needs_modeset: bool,
    pub mode_changed: bool,
    pub has_audio: bool,
    pub has_dsc: bool,
    pub has_drrs: bool,
    pub has_psr: bool,
    pub has_dsc_mst: bool,
    pub has_bigjoiner: bool,
    pub hw_active: bool,
    pub uapi_active: bool,
    pub min_voltage: u8,
    pub scaler_users: u8,
    pub data_rate: u32,
    pub plane_data_rate: u32,
    pub pipe_src_w: u32,
    pub pipe_src_h: u32,
    pub mode_clock: i32,
    pub pixel_multiplier: u8,
    pub pipe_bpp: u8,
    pub cdclk: i32,
    pub pch_pixel_rate: i32,
    pub mbus_joined: bool,
    pub flags: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IntelAtomicState {
    pub display: IntelDisplay,
    pub cdclk: IntelCdclkAtomicState,
    pub old_cdclk: IntelCdclkAtomicState,
    pub crtc: [IntelCrtcState; I915_MAX_PIPES],
    pub old_crtc: [IntelCrtcState; I915_MAX_PIPES],
    pub crtc_mask: u8,
    pub modeset_pipes: u8,
    pub active_pipes: u8,
    pub enabled_pipes: u8,
    pub active_crtcs: u8,
    pub new_crtcs: u8,
    pub disable_pipes: bool,
    pub global_update: bool,
    pub clear_color_changed: bool,
    pub mbus_changed: bool,
    pub error: i32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IntelCrtc {
    pub pipe: i32,
    pub state: IntelCrtcState,
}

/// The callback surface for operations owned by DRM/i915 rather than CDCLK
/// arithmetic. Arguments use the exact source operation name and scalar
/// operands, and the return value is the kernel helper's integer/bool result.
/// Implementations must supply real semantics; there is no default backend.
pub trait IntelCdclkIo {
    fn platform_get_cdclk(
        &mut self,
        family: u32,
        display: &mut IntelDisplay,
        config: &mut IntelCdclkConfig,
    );
    fn platform_set_cdclk(
        &mut self,
        family: u32,
        display: &mut IntelDisplay,
        config: &IntelCdclkConfig,
        pipe: i32,
    );
    fn platform_modeset_calc_cdclk(&mut self, family: u32, state: &mut IntelAtomicState) -> i32;
    fn platform_calc_voltage_level(
        &mut self,
        family: u32,
        display: &IntelDisplay,
        cdclk: i32,
    ) -> u8;
    fn constant(&self, name: &'static str) -> u32;
    fn read_mmio(&mut self, register: &'static str) -> u32;
    fn write_mmio(&mut self, register: &'static str, value: u32);
    fn pci_read16(&mut self, register: &'static str) -> u16;
    fn pci_bus_read16(&mut self, devfn: u16, register: &'static str) -> u16;
    fn mchbar_read8(&mut self, register: &'static str) -> u8;
    fn call(&mut self, operation: &'static str, args: &[i64]) -> i64;
    fn wait(&mut self, register: &'static str, mask: u32, value: u32, timeout: i32) -> i32;
    fn log(&mut self, level: &'static str, message: &'static str, args: &[i64]);
    fn log_cdclk_config(&mut self, context: &'static str, config: &IntelCdclkConfig);
}

#[inline]
fn c<I: IntelCdclkIo + ?Sized>(io: &I, name: &'static str) -> u32 {
    io.constant(name)
}
#[inline]
fn rd<I: IntelCdclkIo + ?Sized>(io: &mut I, reg: &'static str) -> u32 {
    io.read_mmio(reg)
}
#[inline]
fn wr<I: IntelCdclkIo + ?Sized>(io: &mut I, reg: &'static str, value: u32) {
    io.write_mmio(reg, value);
}
#[inline]
fn rmw<I: IntelCdclkIo + ?Sized>(io: &mut I, reg: &'static str, clear: u32, set: u32) {
    let value = rd(io, reg);
    wr(io, reg, (value & !clear) | set);
}
#[inline]
fn round_closest(n: i64, d: i64) -> i64 {
    (n + d / 2) / d
}
#[inline]
fn div_round_up(n: i64, d: i64) -> i64 {
    (n + d - 1) / d
}
#[inline]
fn hweight16(v: u16) -> i32 {
    v.count_ones() as i32
}
#[inline]
fn hook<I: IntelCdclkIo + ?Sized>(io: &mut I, name: &'static str, args: &[i64]) -> i64 {
    io.call(name, args)
}

// upstream: intel_cdclk.c intel_cdclk_get_cdclk()
pub fn intel_cdclk_get_cdclk<I: IntelCdclkIo>(
    io: &mut I,
    display: &mut IntelDisplay,
    config: &mut IntelCdclkConfig,
) {
    io.platform_get_cdclk(display.cdclk.funcs, display, config);
}

// upstream: intel_cdclk.c intel_cdclk_set_cdclk()
pub fn intel_cdclk_set_cdclk<I: IntelCdclkIo>(
    io: &mut I,
    display: &mut IntelDisplay,
    config: &IntelCdclkConfig,
    pipe: i32,
) {
    io.platform_set_cdclk(display.cdclk.funcs, display, config, pipe);
}

// upstream: intel_cdclk.c intel_cdclk_modeset_calc_cdclk()
pub fn intel_cdclk_modeset_calc_cdclk<I: IntelCdclkIo>(
    io: &mut I,
    state: &mut IntelAtomicState,
) -> i32 {
    io.platform_modeset_calc_cdclk(state.display.cdclk.funcs, state)
}

// upstream: intel_cdclk.c intel_cdclk_calc_voltage_level()
pub fn intel_cdclk_calc_voltage_level<I: IntelCdclkIo>(
    io: &mut I,
    display: &mut IntelDisplay,
    cdclk: i32,
) -> u8 {
    io.platform_calc_voltage_level(display.cdclk.funcs, display, cdclk)
}

// upstream: intel_cdclk.c fixed_133mhz_get_cdclk()
pub fn fixed_133mhz_get_cdclk(
    _io: &mut impl IntelCdclkIo,
    _display: &mut IntelDisplay,
    config: &mut IntelCdclkConfig,
) {
    config.cdclk = 133333;
}

// upstream: intel_cdclk.c fixed_200mhz_get_cdclk()
pub fn fixed_200mhz_get_cdclk(
    _io: &mut impl IntelCdclkIo,
    _display: &mut IntelDisplay,
    config: &mut IntelCdclkConfig,
) {
    config.cdclk = 200000;
}

// upstream: intel_cdclk.c fixed_266mhz_get_cdclk()
pub fn fixed_266mhz_get_cdclk(
    _io: &mut impl IntelCdclkIo,
    _display: &mut IntelDisplay,
    config: &mut IntelCdclkConfig,
) {
    config.cdclk = 266667;
}

// upstream: intel_cdclk.c fixed_333mhz_get_cdclk()
pub fn fixed_333mhz_get_cdclk(
    _io: &mut impl IntelCdclkIo,
    _display: &mut IntelDisplay,
    config: &mut IntelCdclkConfig,
) {
    config.cdclk = 333333;
}

// upstream: intel_cdclk.c fixed_400mhz_get_cdclk()
pub fn fixed_400mhz_get_cdclk(
    _io: &mut impl IntelCdclkIo,
    _display: &mut IntelDisplay,
    config: &mut IntelCdclkConfig,
) {
    config.cdclk = 400000;
}

// upstream: intel_cdclk.c fixed_450mhz_get_cdclk()
pub fn fixed_450mhz_get_cdclk(
    _io: &mut impl IntelCdclkIo,
    _display: &mut IntelDisplay,
    config: &mut IntelCdclkConfig,
) {
    config.cdclk = 450000;
}

// upstream: intel_cdclk.c i85x_get_cdclk()
pub fn i85x_get_cdclk<I: IntelCdclkIo>(
    io: &mut I,
    _display: &mut IntelDisplay,
    config: &mut IntelCdclkConfig,
) {
    let revision = io.pci_read16("PCI_REVISION_ID");
    if revision == 0x1 {
        config.cdclk = 133333;
        return;
    }
    let hpllcc = io.pci_bus_read16(3, "HPLLCC");
    match (hpllcc as u32) & c(io, "GC_CLOCK_CONTROL_MASK") {
        x if x == c(io, "GC_CLOCK_133_200")
            || x == c(io, "GC_CLOCK_133_200_2")
            || x == c(io, "GC_CLOCK_100_200") =>
        {
            config.cdclk = 200000
        }
        x if x == c(io, "GC_CLOCK_166_250") => config.cdclk = 250000,
        x if x == c(io, "GC_CLOCK_100_133") => config.cdclk = 133333,
        x if x == c(io, "GC_CLOCK_133_266")
            || x == c(io, "GC_CLOCK_133_266_2")
            || x == c(io, "GC_CLOCK_166_266") =>
        {
            config.cdclk = 266667
        }
        _ => {}
    }
}

// upstream: intel_cdclk.c i915gm_get_cdclk()
pub fn i915gm_get_cdclk<I: IntelCdclkIo>(
    io: &mut I,
    _display: &mut IntelDisplay,
    config: &mut IntelCdclkConfig,
) {
    let gcfgc = io.pci_read16("GCFGC");
    if (gcfgc as u32) & c(io, "GC_LOW_FREQUENCY_ENABLE") != 0 {
        config.cdclk = 133333;
        return;
    }
    config.cdclk = if (gcfgc as u32) & c(io, "GC_DISPLAY_CLOCK_MASK")
        == c(io, "GC_DISPLAY_CLOCK_333_320_MHZ")
    {
        333333
    } else {
        190000
    };
}

// upstream: intel_cdclk.c i945gm_get_cdclk()
pub fn i945gm_get_cdclk<I: IntelCdclkIo>(
    io: &mut I,
    _display: &mut IntelDisplay,
    config: &mut IntelCdclkConfig,
) {
    let gcfgc = io.pci_read16("GCFGC");
    if (gcfgc as u32) & c(io, "GC_LOW_FREQUENCY_ENABLE") != 0 {
        config.cdclk = 133333;
        return;
    }
    config.cdclk = if (gcfgc as u32) & c(io, "GC_DISPLAY_CLOCK_MASK")
        == c(io, "GC_DISPLAY_CLOCK_333_320_MHZ")
    {
        320000
    } else {
        200000
    };
}

// upstream: intel_cdclk.c intel_hpll_vco()
pub fn intel_hpll_vco<I: IntelCdclkIo>(io: &mut I, display: &mut IntelDisplay) -> u32 {
    const BLB: [u32; 8] = [3200000, 4000000, 5333333, 4800000, 6400000, 0, 0, 0];
    const PNV: [u32; 8] = [3200000, 4000000, 5333333, 4800000, 2666667, 0, 0, 0];
    const CL: [u32; 8] = [
        3200000, 4000000, 5333333, 6400000, 3333333, 3566667, 4266667, 0,
    ];
    const ELK: [u32; 8] = [3200000, 4000000, 5333333, 4800000, 0, 0, 0, 0];
    const CTG: [u32; 8] = [3200000, 4000000, 5333333, 6400000, 2666667, 4266667, 0, 0];
    let table = if display.platform.gm45 {
        &CTG
    } else if display.platform.g45 {
        &ELK
    } else if display.platform.i965gm {
        &CL
    } else if display.platform.pineview {
        &PNV
    } else if display.platform.g33 {
        &BLB
    } else {
        return 0;
    };
    let register = if display.platform.pineview || display.platform.mobile {
        "HPLLVCO_MOBILE"
    } else {
        "HPLLVCO"
    };
    let tmp = io.mchbar_read8(register);
    let vco = table[(tmp & 7) as usize];
    if vco == 0 {
        io.log("error", "Bad HPLL VCO (HPLLVCO=0x%02x)\n", &[tmp as i64]);
    } else {
        io.log("debug", "HPLL VCO %u kHz\n", &[vco as i64]);
    }
    vco
}

// upstream: intel_cdclk.c g33_get_cdclk()
pub fn g33_get_cdclk<I: IntelCdclkIo>(
    io: &mut I,
    display: &mut IntelDisplay,
    config: &mut IntelCdclkConfig,
) {
    const DIV_3200: [u8; 6] = [12, 10, 8, 7, 5, 16];
    const DIV_4000: [u8; 6] = [14, 12, 10, 8, 6, 20];
    const DIV_4800: [u8; 6] = [20, 14, 12, 10, 8, 24];
    const DIV_5333: [u8; 6] = [20, 16, 12, 12, 8, 28];
    config.vco = intel_hpll_vco(io, display) as i32;
    let tmp = io.pci_read16("GCFGC");
    let cdclk_sel = ((tmp >> 4) & 7) as usize;
    if cdclk_sel >= DIV_3200.len() {
        config.cdclk = 190476;
        io.log(
            "error",
            "Unable to determine CDCLK. HPLL VCO=%u kHz, CFGC=0x%08x\n",
            &[config.vco as i64, tmp as i64],
        );
        return;
    }
    let table = match config.vco {
        3200000 => &DIV_3200,
        4000000 => &DIV_4000,
        4800000 => &DIV_4800,
        5333333 => &DIV_5333,
        _ => {
            config.cdclk = 190476;
            io.log(
                "error",
                "Unable to determine CDCLK. HPLL VCO=%u kHz, CFGC=0x%08x\n",
                &[config.vco as i64, tmp as i64],
            );
            return;
        }
    };
    config.cdclk = round_closest(config.vco as i64, table[cdclk_sel] as i64) as i32;
}

// upstream: intel_cdclk.c pnv_get_cdclk()
pub fn pnv_get_cdclk<I: IntelCdclkIo>(
    io: &mut I,
    _display: &mut IntelDisplay,
    config: &mut IntelCdclkConfig,
) {
    let gcfgc = io.pci_read16("GCFGC");
    match (gcfgc as u32) & c(io, "GC_DISPLAY_CLOCK_MASK") {
        x if x == c(io, "GC_DISPLAY_CLOCK_267_MHZ_PNV") => config.cdclk = 266667,
        x if x == c(io, "GC_DISPLAY_CLOCK_333_MHZ_PNV") => config.cdclk = 333333,
        x if x == c(io, "GC_DISPLAY_CLOCK_444_MHZ_PNV") => config.cdclk = 444444,
        x if x == c(io, "GC_DISPLAY_CLOCK_200_MHZ_PNV") => config.cdclk = 200000,
        x if x == c(io, "GC_DISPLAY_CLOCK_133_MHZ_PNV") => config.cdclk = 133333,
        x if x == c(io, "GC_DISPLAY_CLOCK_167_MHZ_PNV") => config.cdclk = 166667,
        _ => {
            io.log(
                "error",
                "Unknown pnv display core clock 0x%04x\n",
                &[gcfgc as i64],
            );
            config.cdclk = 133333;
        }
    }
}

// upstream: intel_cdclk.c i965gm_get_cdclk()
pub fn i965gm_get_cdclk<I: IntelCdclkIo>(
    io: &mut I,
    display: &mut IntelDisplay,
    config: &mut IntelCdclkConfig,
) {
    const DIV_3200: [u8; 3] = [16, 10, 8];
    const DIV_4000: [u8; 3] = [20, 12, 10];
    const DIV_5333: [u8; 3] = [24, 16, 14];
    config.vco = intel_hpll_vco(io, display) as i32;
    let tmp = io.pci_read16("GCFGC");
    let cdclk_sel = (((tmp >> 8) & 0x1f) as i32 - 1) as usize;
    if cdclk_sel >= DIV_3200.len() {
        config.cdclk = 200000;
        io.log(
            "error",
            "Unable to determine CDCLK. HPLL VCO=%u kHz, CFGC=0x%04x\n",
            &[config.vco as i64, tmp as i64],
        );
        return;
    }
    let table = match config.vco {
        3200000 => &DIV_3200,
        4000000 => &DIV_4000,
        5333333 => &DIV_5333,
        _ => {
            config.cdclk = 200000;
            io.log(
                "error",
                "Unable to determine CDCLK. HPLL VCO=%u kHz, CFGC=0x%04x\n",
                &[config.vco as i64, tmp as i64],
            );
            return;
        }
    };
    config.cdclk = round_closest(config.vco as i64, table[cdclk_sel] as i64) as i32;
}

// upstream: intel_cdclk.c gm45_get_cdclk()
pub fn gm45_get_cdclk<I: IntelCdclkIo>(
    io: &mut I,
    display: &mut IntelDisplay,
    config: &mut IntelCdclkConfig,
) {
    config.vco = intel_hpll_vco(io, display) as i32;
    let tmp = io.pci_read16("GCFGC");
    let cdclk_sel = (tmp >> 12) & 1;
    config.cdclk = match config.vco {
        2666667 | 4000000 | 5333333 => {
            if cdclk_sel != 0 {
                333333
            } else {
                222222
            }
        }
        3200000 => {
            if cdclk_sel != 0 {
                320000
            } else {
                228571
            }
        }
        _ => {
            io.log(
                "error",
                "Unable to determine CDCLK. HPLL VCO=%u, CFGC=0x%04x\n",
                &[config.vco as i64, tmp as i64],
            );
            222222
        }
    };
}

// upstream: intel_cdclk.c hsw_get_cdclk()
pub fn hsw_get_cdclk<I: IntelCdclkIo>(
    io: &mut I,
    display: &mut IntelDisplay,
    config: &mut IntelCdclkConfig,
) {
    let lcpll = rd(io, "LCPLL_CTL");
    let freq = lcpll & c(io, "LCPLL_CLK_FREQ_MASK");
    config.cdclk = if lcpll & c(io, "LCPLL_CD_SOURCE_FCLK") != 0 {
        800000
    } else if rd(io, "FUSE_STRAP") & c(io, "HSW_CDCLK_LIMIT") != 0 {
        450000
    } else if freq == c(io, "LCPLL_CLK_FREQ_450") {
        450000
    } else if display.platform.haswell_ult {
        337500
    } else {
        540000
    };
}

// upstream: intel_cdclk.c vlv_calc_cdclk()
pub fn vlv_calc_cdclk<I: IntelCdclkIo>(io: &mut I, display: &IntelDisplay, min_cdclk: i32) -> i32 {
    let hpll = hook(io, "vlv_clock_get_hpll_vco", &[]) as i32;
    let freq_320 = if ((hpll << 1) % 320000) != 0 {
        333333
    } else {
        320000
    };
    if display.platform.valleyview && min_cdclk > freq_320 {
        400000
    } else if min_cdclk > 266667 {
        freq_320
    } else if min_cdclk > 0 {
        266667
    } else {
        200000
    }
}

// upstream: intel_cdclk.c vlv_calc_voltage_level()
pub fn vlv_calc_voltage_level<I: IntelCdclkIo>(
    io: &mut I,
    display: &IntelDisplay,
    cdclk: i32,
) -> u8 {
    if display.platform.valleyview {
        if cdclk >= 320000 {
            2
        } else if cdclk >= 266667 {
            1
        } else {
            0
        }
    } else {
        let hpll = hook(io, "vlv_clock_get_hpll_vco", &[]) as i64;
        (round_closest(hpll << 1, cdclk as i64) - 1) as u8
    }
}

// upstream: intel_cdclk.c vlv_get_cdclk()
pub fn vlv_get_cdclk<I: IntelCdclkIo>(
    io: &mut I,
    display: &mut IntelDisplay,
    config: &mut IntelCdclkConfig,
) {
    config.vco = hook(io, "vlv_clock_get_hpll_vco", &[]) as i32;
    config.cdclk = hook(io, "vlv_clock_get_cdclk", &[]) as i32;
    hook(io, "vlv_punit_get", &[]);
    let val = hook(io, "vlv_punit_read:PUNIT_REG_DSPSSPM", &[]) as u32;
    hook(io, "vlv_punit_put", &[]);
    config.voltage_level = if display.platform.valleyview {
        ((val & c(io, "DSPFREQGUAR_MASK")) >> c(io, "DSPFREQGUAR_SHIFT")) as u8
    } else {
        ((val & c(io, "DSPFREQGUAR_MASK_CHV")) >> c(io, "DSPFREQGUAR_SHIFT_CHV")) as u8
    };
}

// upstream: intel_cdclk.c vlv_program_pfi_credits()
pub fn vlv_program_pfi_credits<I: IntelCdclkIo>(io: &mut I, display: &mut IntelDisplay) {
    let default_credits = if display.platform.cherryview {
        c(io, "PFI_CREDIT(12)")
    } else {
        c(io, "PFI_CREDIT(8)")
    };
    let credits = if display.cdclk.hw.cdclk >= hook(io, "vlv_clock_get_czclk", &[]) as i32 {
        if display.platform.cherryview {
            c(io, "PFI_CREDIT_63")
        } else {
            c(io, "PFI_CREDIT(15)")
        }
    } else {
        default_credits
    };
    wr(
        io,
        "GCI_CONTROL",
        c(io, "VGA_FAST_MODE_DISABLE") | default_credits,
    );
    wr(
        io,
        "GCI_CONTROL",
        c(io, "VGA_FAST_MODE_DISABLE") | credits | c(io, "PFI_CREDIT_RESEND"),
    );
    if rd(io, "GCI_CONTROL") & c(io, "PFI_CREDIT_RESEND") != 0 {
        hook(io, "drm_WARN_ON", &[1]);
    }
}

// upstream: intel_cdclk.c vlv_set_cdclk()
pub fn vlv_set_cdclk<I: IntelCdclkIo>(
    io: &mut I,
    display: &mut IntelDisplay,
    config: &IntelCdclkConfig,
    _pipe: i32,
) {
    let cdclk = config.cdclk;
    let cmd = config.voltage_level as u32;
    if !matches!(cdclk, 400000 | 333333 | 320000 | 266667 | 200000) {
        hook(io, "MISSING_CASE", &[cdclk as i64]);
        return;
    }
    let wakeref = hook(io, "intel_display_power_get:POWER_DOMAIN_DISPLAY_CORE", &[]);
    let sb_mask = c(io, "BIT(VLV_IOSF_SB_CCK)")
        | c(io, "BIT(VLV_IOSF_SB_BUNIT)")
        | c(io, "BIT(VLV_IOSF_SB_PUNIT)");
    hook(io, "intel_parent_vlv_iosf_get", &[sb_mask as i64]);
    let mut val = hook(io, "vlv_punit_read:PUNIT_REG_DSPSSPM", &[]) as u32;
    val &= !c(io, "DSPFREQGUAR_MASK");
    val |= cmd << c(io, "DSPFREQGUAR_SHIFT");
    hook(io, "vlv_punit_write:PUNIT_REG_DSPSSPM", &[val as i64]);
    let ret = hook(
        io,
        "poll_timeout_us:PUNIT_REG_DSPSSPM",
        &[500, 50_000, cmd as i64],
    );
    if ret != 0 {
        io.log("error", "timed out waiting for CDCLK change\n", &[]);
    }
    if cdclk == 400000 {
        let hpll = hook(io, "vlv_clock_get_hpll_vco", &[]) as i64;
        let divider = (round_closest(hpll << 1, cdclk as i64) - 1) as u32;
        val = hook(io, "vlv_cck_read:CCK_DISPLAY_CLOCK_CONTROL", &[]) as u32;
        val &= !c(io, "CCK_FREQUENCY_VALUES");
        val |= divider;
        hook(io, "vlv_cck_write:CCK_DISPLAY_CLOCK_CONTROL", &[val as i64]);
        let ret = hook(
            io,
            "poll_timeout_us:CCK_DISPLAY_CLOCK_CONTROL",
            &[500, 50_000, divider as i64],
        );
        if ret != 0 {
            io.log("error", "timed out waiting for CDCLK change\n", &[]);
        }
    }
    val = hook(io, "vlv_bunit_read:BUNIT_REG_BISOC", &[]) as u32;
    val &= !0x7f;
    if cdclk == 400000 {
        val |= 4500 / 250;
    } else {
        val |= 3000 / 250;
    }
    hook(io, "vlv_bunit_write:BUNIT_REG_BISOC", &[val as i64]);
    hook(io, "intel_parent_vlv_iosf_put", &[sb_mask as i64]);
    hook(io, "intel_update_cdclk", &[]);
    vlv_program_pfi_credits(io, display);
    hook(
        io,
        "intel_display_power_put:POWER_DOMAIN_DISPLAY_CORE",
        &[wakeref],
    );
}

// upstream: intel_cdclk.c chv_set_cdclk()
pub fn chv_set_cdclk<I: IntelCdclkIo>(
    io: &mut I,
    display: &mut IntelDisplay,
    config: &IntelCdclkConfig,
    _pipe: i32,
) {
    let cdclk = config.cdclk;
    let cmd = config.voltage_level as u32;
    if !matches!(cdclk, 333333 | 320000 | 266667 | 200000) {
        hook(io, "MISSING_CASE", &[cdclk as i64]);
        return;
    }
    let wakeref = hook(io, "intel_display_power_get:POWER_DOMAIN_DISPLAY_CORE", &[]);
    hook(io, "vlv_punit_get", &[]);
    let mut val = hook(io, "vlv_punit_read:PUNIT_REG_DSPSSPM", &[]) as u32;
    val &= !c(io, "DSPFREQGUAR_MASK_CHV");
    val |= cmd << c(io, "DSPFREQGUAR_SHIFT_CHV");
    hook(io, "vlv_punit_write:PUNIT_REG_DSPSSPM", &[val as i64]);
    let ret = hook(
        io,
        "poll_timeout_us:PUNIT_REG_DSPSSPM",
        &[500, 50_000, cmd as i64],
    );
    if ret != 0 {
        io.log("error", "timed out waiting for CDCLK change\n", &[]);
    }
    hook(io, "vlv_punit_put", &[]);
    hook(io, "intel_update_cdclk", &[]);
    vlv_program_pfi_credits(io, display);
    hook(
        io,
        "intel_display_power_put:POWER_DOMAIN_DISPLAY_CORE",
        &[wakeref],
    );
}

// upstream: intel_cdclk.c bdw_calc_cdclk()
pub fn bdw_calc_cdclk(min_cdclk: i32) -> i32 {
    if min_cdclk > 540000 {
        675000
    } else if min_cdclk > 450000 {
        540000
    } else if min_cdclk > 337500 {
        450000
    } else {
        337500
    }
}

// upstream: intel_cdclk.c bdw_calc_voltage_level()
pub fn bdw_calc_voltage_level(cdclk: i32) -> u8 {
    match cdclk {
        450000 => 0,
        540000 => 1,
        675000 => 3,
        _ => 2,
    }
}

// upstream: intel_cdclk.c bdw_get_cdclk()
pub fn bdw_get_cdclk<I: IntelCdclkIo>(
    io: &mut I,
    display: &mut IntelDisplay,
    config: &mut IntelCdclkConfig,
) {
    let lcpll = rd(io, "LCPLL_CTL");
    let freq = lcpll & c(io, "LCPLL_CLK_FREQ_MASK");
    config.cdclk = if lcpll & c(io, "LCPLL_CD_SOURCE_FCLK") != 0 {
        800000
    } else if rd(io, "FUSE_STRAP") & c(io, "HSW_CDCLK_LIMIT") != 0 {
        450000
    } else if freq == c(io, "LCPLL_CLK_FREQ_450") {
        450000
    } else if freq == c(io, "LCPLL_CLK_FREQ_54O_BDW") {
        540000
    } else if freq == c(io, "LCPLL_CLK_FREQ_337_5_BDW") {
        337500
    } else {
        675000
    };
    config.voltage_level = bdw_calc_voltage_level(config.cdclk);
    let _ = display;
}

// upstream: intel_cdclk.c bdw_cdclk_freq_sel()
pub fn bdw_cdclk_freq_sel<I: IntelCdclkIo>(io: &mut I, cdclk: i32) -> u32 {
    match cdclk {
        450000 => c(io, "LCPLL_CLK_FREQ_450"),
        540000 => c(io, "LCPLL_CLK_FREQ_54O_BDW"),
        675000 => c(io, "LCPLL_CLK_FREQ_675_BDW"),
        _ => {
            if cdclk != 337500 {
                hook(io, "MISSING_CASE", &[cdclk as i64]);
            }
            c(io, "LCPLL_CLK_FREQ_337_5_BDW")
        }
    }
}

// upstream: intel_cdclk.c bdw_set_cdclk()
pub fn bdw_set_cdclk<I: IntelCdclkIo>(
    io: &mut I,
    display: &mut IntelDisplay,
    config: &IntelCdclkConfig,
    _pipe: i32,
) {
    let cdclk = config.cdclk;
    let lcpll = rd(io, "LCPLL_CTL");
    let invalid = (lcpll
        & (c(io, "LCPLL_PLL_DISABLE")
            | c(io, "LCPLL_PLL_LOCK")
            | c(io, "LCPLL_CD_CLOCK_DISABLE")
            | c(io, "LCPLL_ROOT_CD_CLOCK_DISABLE")
            | c(io, "LCPLL_CD2X_CLOCK_DISABLE")
            | c(io, "LCPLL_POWER_DOWN_ALLOW")
            | c(io, "LCPLL_CD_SOURCE_FCLK")))
        != c(io, "LCPLL_PLL_LOCK");
    if invalid {
        hook(io, "drm_WARN:cdclk_not_enabled", &[]);
        return;
    }
    let ret = hook(
        io,
        "intel_parent_pcode_write:BDW_PCODE_DISPLAY_FREQ_CHANGE_REQ",
        &[0],
    );
    if ret != 0 {
        io.log("error", "failed to inform pcode about cdclk change\n", &[]);
        return;
    }
    rmw(io, "LCPLL_CTL", 0, c(io, "LCPLL_CD_SOURCE_FCLK"));
    if io.wait(
        "LCPLL_CTL",
        c(io, "LCPLL_CD_SOURCE_FCLK_DONE"),
        c(io, "LCPLL_CD_SOURCE_FCLK_DONE"),
        100,
    ) != 0
    {
        io.log("error", "Switching to FCLK failed\n", &[]);
    }
    let frequency_mask = c(io, "LCPLL_CLK_FREQ_MASK");
    let frequency_select = bdw_cdclk_freq_sel(io, cdclk);
    rmw(io, "LCPLL_CTL", frequency_mask, frequency_select);
    rmw(io, "LCPLL_CTL", c(io, "LCPLL_CD_SOURCE_FCLK"), 0);
    if io.wait("LCPLL_CTL", c(io, "LCPLL_CD_SOURCE_FCLK_DONE"), 0, 1) != 0 {
        io.log("error", "Switching back to LCPLL failed\n", &[]);
    }
    hook(
        io,
        "intel_parent_pcode_write:HSW_PCODE_DE_WRITE_FREQ_REQ",
        &[config.voltage_level as i64],
    );
    wr(
        io,
        "CDCLK_FREQ",
        (round_closest(cdclk as i64, 1000) - 1) as u32,
    );
    hook(io, "intel_update_cdclk", &[]);
    let _ = display;
}

// upstream: intel_cdclk.c skl_calc_cdclk()
pub fn skl_calc_cdclk(min_cdclk: i32, vco: i32) -> i32 {
    if vco == 8640000 {
        if min_cdclk > 540000 {
            617143
        } else if min_cdclk > 432000 {
            540000
        } else if min_cdclk > 308571 {
            432000
        } else {
            308571
        }
    } else if min_cdclk > 540000 {
        675000
    } else if min_cdclk > 450000 {
        540000
    } else if min_cdclk > 337500 {
        450000
    } else {
        337500
    }
}

// upstream: intel_cdclk.c skl_calc_voltage_level()
pub fn skl_calc_voltage_level(cdclk: i32) -> u8 {
    if cdclk > 540000 {
        3
    } else if cdclk > 450000 {
        2
    } else if cdclk > 337500 {
        1
    } else {
        0
    }
}

// upstream: intel_cdclk.c skl_dpll0_update()
pub fn skl_dpll0_update<I: IntelCdclkIo>(io: &mut I, config: &mut IntelCdclkConfig) {
    config.refclk = 24000;
    config.vco = 0;
    let mut val = rd(io, "LCPLL1_CTL");
    if val & c(io, "LCPLL_PLL_ENABLE") == 0 {
        return;
    }
    if val & c(io, "LCPLL_PLL_LOCK") == 0 {
        hook(io, "drm_WARN", &[1]);
        return;
    }
    val = rd(io, "DPLL_CTRL1");
    let required = c(io, "DPLL_CTRL1_OVERRIDE(SKL_DPLL0)");
    let mask =
        c(io, "DPLL_CTRL1_HDMI_MODE(SKL_DPLL0)") | c(io, "DPLL_CTRL1_SSC(SKL_DPLL0)") | required;
    if val & mask != required {
        hook(io, "drm_WARN", &[1]);
        return;
    }
    match val & c(io, "DPLL_CTRL1_LINK_RATE_MASK(SKL_DPLL0)") {
        x if [
            "DPLL_CTRL1_LINK_RATE(DPLL_CTRL1_LINK_RATE_810, SKL_DPLL0)",
            "DPLL_CTRL1_LINK_RATE(DPLL_CTRL1_LINK_RATE_1350, SKL_DPLL0)",
            "DPLL_CTRL1_LINK_RATE(DPLL_CTRL1_LINK_RATE_1620, SKL_DPLL0)",
            "DPLL_CTRL1_LINK_RATE(DPLL_CTRL1_LINK_RATE_2700, SKL_DPLL0)",
        ]
        .iter()
        .any(|n| x == c(io, n)) =>
        {
            config.vco = 8100000
        }
        x if [
            "DPLL_CTRL1_LINK_RATE(DPLL_CTRL1_LINK_RATE_1080, SKL_DPLL0)",
            "DPLL_CTRL1_LINK_RATE(DPLL_CTRL1_LINK_RATE_2160, SKL_DPLL0)",
        ]
        .iter()
        .any(|n| x == c(io, n)) =>
        {
            config.vco = 8640000
        }
        x => {
            hook(io, "MISSING_CASE:DPLL_CTRL1_LINK_RATE_MASK", &[x as i64]);
        }
    }
}

// upstream: intel_cdclk.c skl_get_cdclk()
pub fn skl_get_cdclk<I: IntelCdclkIo>(io: &mut I, config: &mut IntelCdclkConfig) {
    skl_dpll0_update(io, config);
    config.cdclk = config.refclk;
    config.bypass = config.refclk;
    if config.vco != 0 {
        let cdctl = rd(io, "CDCLK_CTL");
        let freq = cdctl & c(io, "CDCLK_FREQ_SEL_MASK");
        config.cdclk = if config.vco == 8640000 {
            match freq {
                x if x == c(io, "CDCLK_FREQ_450_432") => 432000,
                x if x == c(io, "CDCLK_FREQ_337_308") => 308571,
                x if x == c(io, "CDCLK_FREQ_540") => 540000,
                x if x == c(io, "CDCLK_FREQ_675_617") => 617143,
                _ => {
                    hook(io, "MISSING_CASE:CDCLK_FREQ_SEL_MASK", &[freq as i64]);
                    config.cdclk
                }
            }
        } else {
            match freq {
                x if x == c(io, "CDCLK_FREQ_450_432") => 450000,
                x if x == c(io, "CDCLK_FREQ_337_308") => 337500,
                x if x == c(io, "CDCLK_FREQ_540") => 540000,
                x if x == c(io, "CDCLK_FREQ_675_617") => 675000,
                _ => {
                    hook(io, "MISSING_CASE:CDCLK_FREQ_SEL_MASK", &[freq as i64]);
                    config.cdclk
                }
            }
        };
    }
    config.voltage_level = skl_calc_voltage_level(config.cdclk);
}

// upstream: intel_cdclk.c skl_cdclk_decimal()
pub fn skl_cdclk_decimal(cdclk: i32) -> i32 {
    round_closest((cdclk - 1000) as i64, 500) as i32
}

// upstream: intel_cdclk.c skl_set_preferred_cdclk_vco()
pub fn skl_set_preferred_cdclk_vco<I: IntelCdclkIo>(
    io: &mut I,
    display: &mut IntelDisplay,
    vco: i32,
) {
    let changed = display.cdclk.skl_preferred_vco_freq != vco;
    display.cdclk.skl_preferred_vco_freq = vco;
    if changed {
        hook(io, "intel_update_max_cdclk", &[]);
    }
}

// upstream: intel_cdclk.c skl_dpll0_link_rate()
pub fn skl_dpll0_link_rate<I: IntelCdclkIo>(io: &mut I, vco: i32) -> u32 {
    if vco != 8100000 && vco != 8640000 {
        hook(io, "drm_WARN", &[vco as i64]);
    }
    if vco == 8640000 {
        c(
            io,
            "DPLL_CTRL1_LINK_RATE(DPLL_CTRL1_LINK_RATE_1080, SKL_DPLL0)",
        )
    } else {
        c(
            io,
            "DPLL_CTRL1_LINK_RATE(DPLL_CTRL1_LINK_RATE_810, SKL_DPLL0)",
        )
    }
}

// upstream: intel_cdclk.c skl_dpll0_enable()
pub fn skl_dpll0_enable<I: IntelCdclkIo>(io: &mut I, display: &mut IntelDisplay, vco: i32) {
    let clear = c(io, "DPLL_CTRL1_HDMI_MODE(SKL_DPLL0)")
        | c(io, "DPLL_CTRL1_SSC(SKL_DPLL0)")
        | c(io, "DPLL_CTRL1_LINK_RATE_MASK(SKL_DPLL0)");
    let set = c(io, "DPLL_CTRL1_OVERRIDE(SKL_DPLL0)") | skl_dpll0_link_rate(io, vco);
    rmw(io, "DPLL_CTRL1", clear, set);
    let _ = rd(io, "DPLL_CTRL1");
    rmw(io, "LCPLL1_CTL", 0, c(io, "LCPLL_PLL_ENABLE"));
    if io.wait(
        "LCPLL1_CTL",
        c(io, "LCPLL_PLL_LOCK"),
        c(io, "LCPLL_PLL_LOCK"),
        5,
    ) != 0
    {
        io.log("error", "DPLL0 not locked\n", &[]);
    }
    display.cdclk.hw.vco = vco;
    skl_set_preferred_cdclk_vco(io, display, vco);
}

// upstream: intel_cdclk.c skl_dpll0_disable()
pub fn skl_dpll0_disable<I: IntelCdclkIo>(io: &mut I, display: &mut IntelDisplay) {
    rmw(io, "LCPLL1_CTL", c(io, "LCPLL_PLL_ENABLE"), 0);
    if io.wait("LCPLL1_CTL", c(io, "LCPLL_PLL_LOCK"), 0, 1) != 0 {
        io.log("error", "Couldn't disable DPLL0\n", &[]);
    }
    display.cdclk.hw.vco = 0;
}

// upstream: intel_cdclk.c skl_cdclk_freq_sel()
pub fn skl_cdclk_freq_sel<I: IntelCdclkIo>(
    io: &mut I,
    display: &IntelDisplay,
    cdclk: i32,
    vco: i32,
) -> u32 {
    match cdclk {
        308571 | 337500 => c(io, "CDCLK_FREQ_337_308"),
        450000 | 432000 => c(io, "CDCLK_FREQ_450_432"),
        540000 => c(io, "CDCLK_FREQ_540"),
        617143 | 675000 => c(io, "CDCLK_FREQ_675_617"),
        _ => {
            if cdclk != display.cdclk.hw.bypass {
                hook(io, "drm_WARN:cdclk_bypass", &[cdclk as i64]);
            }
            if vco != 0 {
                hook(io, "drm_WARN:vco_zero", &[vco as i64]);
            }
            c(io, "CDCLK_FREQ_337_308")
        }
    }
}

// upstream: intel_cdclk.c skl_set_cdclk()
pub fn skl_set_cdclk<I: IntelCdclkIo>(
    io: &mut I,
    display: &mut IntelDisplay,
    config: &IntelCdclkConfig,
    _pipe: i32,
) {
    let cdclk = config.cdclk;
    let vco = config.vco;
    if display.platform.skylake && vco == 8640000 {
        hook(io, "drm_WARN_ON_ONCE:WA1183", &[]);
    }
    let ret = hook(
        io,
        "intel_parent_pcode_request:SKL_PCODE_CDCLK_CONTROL",
        &[
            c(io, "SKL_CDCLK_PREPARE_FOR_CHANGE") as i64,
            c(io, "SKL_CDCLK_READY_FOR_CHANGE") as i64,
            c(io, "SKL_CDCLK_READY_FOR_CHANGE") as i64,
            3,
        ],
    );
    if ret != 0 {
        io.log(
            "error",
            "Failed to inform PCU about cdclk change (%d)\n",
            &[ret],
        );
        return;
    }
    let freq_select = skl_cdclk_freq_sel(io, display, cdclk, vco);
    if display.cdclk.hw.vco != 0 && display.cdclk.hw.vco != vco {
        skl_dpll0_disable(io, display);
    }
    let mut cdclk_ctl = rd(io, "CDCLK_CTL");
    if display.cdclk.hw.vco != vco {
        cdclk_ctl &= !(c(io, "CDCLK_FREQ_SEL_MASK") | c(io, "CDCLK_FREQ_DECIMAL_MASK"));
        cdclk_ctl |= freq_select | skl_cdclk_decimal(cdclk) as u32;
        wr(io, "CDCLK_CTL", cdclk_ctl);
    }
    cdclk_ctl |= c(io, "CDCLK_DIVMUX_CD_OVERRIDE");
    wr(io, "CDCLK_CTL", cdclk_ctl);
    let _ = rd(io, "CDCLK_CTL");
    if display.cdclk.hw.vco != vco {
        skl_dpll0_enable(io, display, vco);
    }
    cdclk_ctl &= !(c(io, "CDCLK_FREQ_SEL_MASK") | c(io, "CDCLK_FREQ_DECIMAL_MASK"));
    wr(io, "CDCLK_CTL", cdclk_ctl);
    cdclk_ctl |= freq_select | skl_cdclk_decimal(cdclk) as u32;
    wr(io, "CDCLK_CTL", cdclk_ctl);
    cdclk_ctl &= !c(io, "CDCLK_DIVMUX_CD_OVERRIDE");
    wr(io, "CDCLK_CTL", cdclk_ctl);
    let _ = rd(io, "CDCLK_CTL");
    hook(
        io,
        "intel_parent_pcode_write:SKL_PCODE_CDCLK_CONTROL",
        &[config.voltage_level as i64],
    );
    hook(io, "intel_update_cdclk", &[]);
}

// upstream: intel_cdclk.c skl_sanitize_cdclk()
pub fn skl_sanitize_cdclk<I: IntelCdclkIo>(io: &mut I, display: &mut IntelDisplay) {
    if rd(io, "SWF_ILK(0x18)") & 0x00ff_ffff == 0 {
        goto_sanitize_skl(display);
        return;
    }
    hook(io, "intel_update_cdclk", &[]);
    hook(
        io,
        "intel_cdclk_dump_config",
        &[display.cdclk.hw.cdclk as i64, display.cdclk.hw.vco as i64],
    );
    if display.cdclk.hw.vco == 0 || display.cdclk.hw.cdclk == display.cdclk.hw.bypass {
        goto_sanitize_skl(display);
        return;
    }
    let cdctl = rd(io, "CDCLK_CTL");
    let expected =
        (cdctl & c(io, "CDCLK_FREQ_SEL_MASK")) | skl_cdclk_decimal(display.cdclk.hw.cdclk) as u32;
    if cdctl != expected {
        let corrected = (cdctl & !c(io, "CDCLK_FREQ_DECIMAL_MASK"))
            | (expected & c(io, "CDCLK_FREQ_DECIMAL_MASK"));
        if corrected != expected {
            goto_sanitize_skl(display);
            return;
        }
        let observed = rd(io, "CDCLK_CTL");
        io.log(
            "debug",
            "Sanitizing CDCLK decimal divider (CDCLK_CTL 0x%x, expected 0x%x)\n",
            &[observed as i64, expected as i64],
        );
        wr(io, "CDCLK_CTL", expected);
    }
}
#[inline]
fn goto_sanitize_skl(display: &mut IntelDisplay) {
    display.cdclk.hw.cdclk = 0;
    display.cdclk.hw.vco = !0;
}

// upstream: intel_cdclk.c skl_cdclk_init_hw()
pub fn skl_cdclk_init_hw<I: IntelCdclkIo>(io: &mut I, display: &mut IntelDisplay) {
    skl_sanitize_cdclk(io, display);
    if display.cdclk.hw.cdclk != 0 && display.cdclk.hw.vco != 0 {
        if display.cdclk.skl_preferred_vco_freq == 0 {
            skl_set_preferred_cdclk_vco(io, display, display.cdclk.hw.vco);
        }
        return;
    }
    let mut config = display.cdclk.hw;
    config.vco = display.cdclk.skl_preferred_vco_freq;
    if config.vco == 0 {
        config.vco = 8100000;
    }
    config.cdclk = skl_calc_cdclk(0, config.vco);
    config.voltage_level = skl_calc_voltage_level(config.cdclk);
    skl_set_cdclk(io, display, &config, INVALID_PIPE);
}

// upstream: intel_cdclk.c skl_cdclk_uninit_hw()
pub fn skl_cdclk_uninit_hw<I: IntelCdclkIo>(io: &mut I, display: &mut IntelDisplay) {
    let mut config = display.cdclk.hw;
    config.cdclk = config.bypass;
    config.vco = 0;
    config.voltage_level = skl_calc_voltage_level(config.cdclk);
    skl_set_cdclk(io, display, &config, INVALID_PIPE);
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IntelCdclkVals {
    pub cdclk: u32,
    pub refclk: u16,
    pub waveform: u16,
    pub ratio: u8,
}

pub const BXT_CDCLK_TABLE: &[IntelCdclkVals] = &[
    IntelCdclkVals {
        refclk: 19200,
        cdclk: 144000,
        ratio: 60,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 19200,
        cdclk: 288000,
        ratio: 60,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 19200,
        cdclk: 384000,
        ratio: 60,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 19200,
        cdclk: 576000,
        ratio: 60,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 19200,
        cdclk: 624000,
        ratio: 65,
        waveform: 0,
    },
];
pub const GLK_CDCLK_TABLE: &[IntelCdclkVals] = &[
    IntelCdclkVals {
        refclk: 19200,
        cdclk: 79200,
        ratio: 33,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 19200,
        cdclk: 158400,
        ratio: 33,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 19200,
        cdclk: 316800,
        ratio: 33,
        waveform: 0,
    },
];
pub const ICL_CDCLK_TABLE: &[IntelCdclkVals] = &[
    IntelCdclkVals {
        refclk: 19200,
        cdclk: 172800,
        ratio: 18,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 19200,
        cdclk: 192000,
        ratio: 20,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 19200,
        cdclk: 307200,
        ratio: 32,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 19200,
        cdclk: 326400,
        ratio: 68,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 19200,
        cdclk: 556800,
        ratio: 58,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 19200,
        cdclk: 652800,
        ratio: 68,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 24000,
        cdclk: 180000,
        ratio: 15,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 24000,
        cdclk: 192000,
        ratio: 16,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 24000,
        cdclk: 312000,
        ratio: 26,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 24000,
        cdclk: 324000,
        ratio: 54,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 24000,
        cdclk: 552000,
        ratio: 46,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 24000,
        cdclk: 648000,
        ratio: 54,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 172800,
        ratio: 9,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 192000,
        ratio: 10,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 307200,
        ratio: 16,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 326400,
        ratio: 34,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 556800,
        ratio: 29,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 652800,
        ratio: 34,
        waveform: 0,
    },
];
pub const RKL_CDCLK_TABLE: &[IntelCdclkVals] = &[
    IntelCdclkVals {
        refclk: 19200,
        cdclk: 172800,
        ratio: 36,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 19200,
        cdclk: 192000,
        ratio: 40,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 19200,
        cdclk: 307200,
        ratio: 64,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 19200,
        cdclk: 326400,
        ratio: 136,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 19200,
        cdclk: 556800,
        ratio: 116,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 19200,
        cdclk: 652800,
        ratio: 136,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 24000,
        cdclk: 180000,
        ratio: 30,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 24000,
        cdclk: 192000,
        ratio: 32,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 24000,
        cdclk: 312000,
        ratio: 52,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 24000,
        cdclk: 324000,
        ratio: 108,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 24000,
        cdclk: 552000,
        ratio: 92,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 24000,
        cdclk: 648000,
        ratio: 108,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 172800,
        ratio: 18,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 192000,
        ratio: 20,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 307200,
        ratio: 32,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 326400,
        ratio: 68,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 556800,
        ratio: 58,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 652800,
        ratio: 68,
        waveform: 0,
    },
];
pub const ADLP_A_STEP_CDCLK_TABLE: &[IntelCdclkVals] = &[
    IntelCdclkVals {
        refclk: 19200,
        cdclk: 307200,
        ratio: 32,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 19200,
        cdclk: 556800,
        ratio: 58,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 19200,
        cdclk: 652800,
        ratio: 68,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 24000,
        cdclk: 312000,
        ratio: 26,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 24000,
        cdclk: 552000,
        ratio: 46,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 24400,
        cdclk: 648000,
        ratio: 54,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 307200,
        ratio: 16,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 556800,
        ratio: 29,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 652800,
        ratio: 34,
        waveform: 0,
    },
];
pub const ADLP_CDCLK_TABLE: &[IntelCdclkVals] = &[
    IntelCdclkVals {
        refclk: 19200,
        cdclk: 172800,
        ratio: 27,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 19200,
        cdclk: 192000,
        ratio: 20,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 19200,
        cdclk: 307200,
        ratio: 32,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 19200,
        cdclk: 556800,
        ratio: 58,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 19200,
        cdclk: 652800,
        ratio: 68,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 24000,
        cdclk: 176000,
        ratio: 22,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 24000,
        cdclk: 192000,
        ratio: 16,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 24000,
        cdclk: 312000,
        ratio: 26,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 24000,
        cdclk: 552000,
        ratio: 46,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 24000,
        cdclk: 648000,
        ratio: 54,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 179200,
        ratio: 14,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 192000,
        ratio: 10,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 307200,
        ratio: 16,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 556800,
        ratio: 29,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 652800,
        ratio: 34,
        waveform: 0,
    },
];
pub const RPLU_CDCLK_TABLE: &[IntelCdclkVals] = &[
    IntelCdclkVals {
        refclk: 19200,
        cdclk: 172800,
        ratio: 27,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 19200,
        cdclk: 192000,
        ratio: 20,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 19200,
        cdclk: 307200,
        ratio: 32,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 19200,
        cdclk: 480000,
        ratio: 50,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 19200,
        cdclk: 556800,
        ratio: 58,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 19200,
        cdclk: 652800,
        ratio: 68,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 24000,
        cdclk: 176000,
        ratio: 22,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 24000,
        cdclk: 192000,
        ratio: 16,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 24000,
        cdclk: 312000,
        ratio: 26,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 24000,
        cdclk: 480000,
        ratio: 40,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 24000,
        cdclk: 552000,
        ratio: 46,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 24000,
        cdclk: 648000,
        ratio: 54,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 179200,
        ratio: 14,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 192000,
        ratio: 10,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 307200,
        ratio: 16,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 480000,
        ratio: 25,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 556800,
        ratio: 29,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 652800,
        ratio: 34,
        waveform: 0,
    },
];
pub const DG2_CDCLK_TABLE: &[IntelCdclkVals] = &[
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 163200,
        ratio: 34,
        waveform: 0x8888,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 204000,
        ratio: 34,
        waveform: 0x9248,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 244800,
        ratio: 34,
        waveform: 0xa4a4,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 285600,
        ratio: 34,
        waveform: 0xa54a,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 326400,
        ratio: 34,
        waveform: 0xaaaa,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 367200,
        ratio: 34,
        waveform: 0xad5a,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 408000,
        ratio: 34,
        waveform: 0xb6b6,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 448800,
        ratio: 34,
        waveform: 0xdbb6,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 489600,
        ratio: 34,
        waveform: 0xeeee,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 530400,
        ratio: 34,
        waveform: 0xf7de,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 571200,
        ratio: 34,
        waveform: 0xfefe,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 612000,
        ratio: 34,
        waveform: 0xfffe,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 652800,
        ratio: 34,
        waveform: 0xffff,
    },
];
pub const MTL_CDCLK_TABLE: &[IntelCdclkVals] = &[
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 172800,
        ratio: 16,
        waveform: 0xad5a,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 192000,
        ratio: 16,
        waveform: 0xb6b6,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 307200,
        ratio: 16,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 480000,
        ratio: 25,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 556800,
        ratio: 29,
        waveform: 0,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 652800,
        ratio: 34,
        waveform: 0,
    },
];
pub const XE2LPD_CDCLK_TABLE: &[IntelCdclkVals] = &[
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 153600,
        ratio: 16,
        waveform: 0xaaaa,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 172800,
        ratio: 16,
        waveform: 0xad5a,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 192000,
        ratio: 16,
        waveform: 0xb6b6,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 211200,
        ratio: 16,
        waveform: 0xdbb6,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 230400,
        ratio: 16,
        waveform: 0xeeee,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 249600,
        ratio: 16,
        waveform: 0xf7de,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 268800,
        ratio: 16,
        waveform: 0xfefe,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 288000,
        ratio: 16,
        waveform: 0xfffe,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 307200,
        ratio: 16,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 330000,
        ratio: 25,
        waveform: 0xdbb6,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 360000,
        ratio: 25,
        waveform: 0xeeee,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 390000,
        ratio: 25,
        waveform: 0xf7de,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 420000,
        ratio: 25,
        waveform: 0xfefe,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 450000,
        ratio: 25,
        waveform: 0xfffe,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 480000,
        ratio: 25,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 487200,
        ratio: 29,
        waveform: 0xfefe,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 522000,
        ratio: 29,
        waveform: 0xfffe,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 556800,
        ratio: 29,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 571200,
        ratio: 34,
        waveform: 0xfefe,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 612000,
        ratio: 34,
        waveform: 0xfffe,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 652800,
        ratio: 34,
        waveform: 0xffff,
    },
];
pub const XE2HPD_CDCLK_TABLE: &[IntelCdclkVals] = &[IntelCdclkVals {
    refclk: 38400,
    cdclk: 652800,
    ratio: 34,
    waveform: 0xffff,
}];
pub const XE3LPD_CDCLK_TABLE: &[IntelCdclkVals] = &[
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 153600,
        ratio: 16,
        waveform: 0xaaaa,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 172800,
        ratio: 16,
        waveform: 0xad5a,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 192000,
        ratio: 16,
        waveform: 0xb6b6,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 211200,
        ratio: 16,
        waveform: 0xdbb6,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 230400,
        ratio: 16,
        waveform: 0xeeee,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 249600,
        ratio: 16,
        waveform: 0xf7de,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 268800,
        ratio: 16,
        waveform: 0xfefe,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 288000,
        ratio: 16,
        waveform: 0xfffe,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 307200,
        ratio: 16,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 326400,
        ratio: 17,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 345600,
        ratio: 18,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 364800,
        ratio: 19,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 384000,
        ratio: 20,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 403200,
        ratio: 21,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 422400,
        ratio: 22,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 441600,
        ratio: 23,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 460800,
        ratio: 24,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 480000,
        ratio: 25,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 499200,
        ratio: 26,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 518400,
        ratio: 27,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 537600,
        ratio: 28,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 556800,
        ratio: 29,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 576000,
        ratio: 30,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 595200,
        ratio: 31,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 614400,
        ratio: 32,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 633600,
        ratio: 33,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 652800,
        ratio: 34,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 672000,
        ratio: 35,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 691200,
        ratio: 36,
        waveform: 0xffff,
    },
];
pub const XE3P_LPD_CDCLK_TABLE: &[IntelCdclkVals] = &[
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 151200,
        ratio: 21,
        waveform: 0xa4a4,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 176400,
        ratio: 21,
        waveform: 0xaa54,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 201600,
        ratio: 21,
        waveform: 0xaaaa,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 226800,
        ratio: 21,
        waveform: 0xad5a,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 252000,
        ratio: 21,
        waveform: 0xb6b6,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 277200,
        ratio: 21,
        waveform: 0xdbb6,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 302400,
        ratio: 21,
        waveform: 0xeeee,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 327600,
        ratio: 21,
        waveform: 0xf7de,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 352800,
        ratio: 21,
        waveform: 0xfefe,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 378000,
        ratio: 21,
        waveform: 0xfffe,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 403200,
        ratio: 21,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 422400,
        ratio: 22,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 441600,
        ratio: 23,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 460800,
        ratio: 24,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 480000,
        ratio: 25,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 499200,
        ratio: 26,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 518400,
        ratio: 27,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 537600,
        ratio: 28,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 556800,
        ratio: 29,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 576000,
        ratio: 30,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 595200,
        ratio: 31,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 614400,
        ratio: 32,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 633600,
        ratio: 33,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 652800,
        ratio: 34,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 672000,
        ratio: 35,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 691200,
        ratio: 36,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 710400,
        ratio: 37,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 729600,
        ratio: 38,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 748800,
        ratio: 39,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 768000,
        ratio: 40,
        waveform: 0xffff,
    },
    IntelCdclkVals {
        refclk: 38400,
        cdclk: 787200,
        ratio: 41,
        waveform: 0xffff,
    },
];

pub const CDCLK_SQUASH_LEN: i32 = 16;

// upstream: intel_cdclk.c cdclk_squash_divider()
pub fn cdclk_squash_divider(waveform: u16) -> i32 {
    hweight16(if waveform == 0 { 0xffff } else { waveform })
}

// upstream: intel_cdclk.c cdclk_divider()
pub fn cdclk_divider(cdclk: i32, vco: i32, waveform: u16) -> i32 {
    round_closest(
        (vco * cdclk_squash_divider(waveform)) as i64,
        (cdclk * CDCLK_SQUASH_LEN) as i64,
    ) as i32
}

fn table_for(display: &IntelDisplay) -> &'static [IntelCdclkVals] {
    match display.cdclk.table {
        0 => BXT_CDCLK_TABLE,
        1 => GLK_CDCLK_TABLE,
        2 => ICL_CDCLK_TABLE,
        3 => RKL_CDCLK_TABLE,
        4 => ADLP_A_STEP_CDCLK_TABLE,
        5 => ADLP_CDCLK_TABLE,
        6 => RPLU_CDCLK_TABLE,
        7 => DG2_CDCLK_TABLE,
        8 => MTL_CDCLK_TABLE,
        9 => XE2LPD_CDCLK_TABLE,
        10 => XE2HPD_CDCLK_TABLE,
        11 => XE3LPD_CDCLK_TABLE,
        _ => XE3P_LPD_CDCLK_TABLE,
    }
}

// upstream: intel_cdclk.c bxt_calc_cdclk()
pub fn bxt_calc_cdclk<I: IntelCdclkIo>(io: &mut I, display: &IntelDisplay, min_cdclk: i32) -> i32 {
    for entry in table_for(display) {
        if entry.refclk as i32 == display.cdclk.hw.refclk && entry.cdclk as i32 >= min_cdclk {
            return entry.cdclk as i32;
        }
    }
    hook(
        io,
        "drm_WARN:Cannot satisfy minimum cdclk",
        &[min_cdclk as i64, display.cdclk.hw.refclk as i64],
    );
    display.cdclk.max_cdclk_freq
}

// upstream: intel_cdclk.c bxt_calc_cdclk_pll_vco()
pub fn bxt_calc_cdclk_pll_vco<I: IntelCdclkIo>(
    io: &mut I,
    display: &IntelDisplay,
    cdclk: i32,
) -> i32 {
    if cdclk == display.cdclk.hw.bypass {
        return 0;
    }
    for entry in table_for(display) {
        if entry.refclk as i32 == display.cdclk.hw.refclk && entry.cdclk as i32 == cdclk {
            return display.cdclk.hw.refclk * entry.ratio as i32;
        }
    }
    hook(
        io,
        "drm_WARN:cdclk_not_valid_for_refclk",
        &[cdclk as i64, display.cdclk.hw.refclk as i64],
    );
    0
}

// upstream: intel_cdclk.c bxt_calc_voltage_level()
pub fn bxt_calc_voltage_level(cdclk: i32) -> u8 {
    div_round_up(cdclk as i64, 25000) as u8
}

// upstream: intel_cdclk.c calc_voltage_level()
pub fn calc_voltage_level<I: IntelCdclkIo>(io: &mut I, cdclk: i32, limits: &[i32]) -> u8 {
    for (level, max_cdclk) in limits.iter().enumerate() {
        if cdclk <= *max_cdclk {
            return level as u8;
        }
    }
    hook(io, "MISSING_CASE:cdclk_voltage", &[cdclk as i64]);
    limits.len().saturating_sub(1) as u8
}

// upstream: intel_cdclk.c icl_calc_voltage_level()
pub fn icl_calc_voltage_level<I: IntelCdclkIo>(io: &mut I, cdclk: i32) -> u8 {
    calc_voltage_level(io, cdclk, &[312000, 556800, 652800])
}

// upstream: intel_cdclk.c ehl_calc_voltage_level()
pub fn ehl_calc_voltage_level<I: IntelCdclkIo>(io: &mut I, cdclk: i32) -> u8 {
    calc_voltage_level(io, cdclk, &[180000, 312000, 326400, 652800])
}

// upstream: intel_cdclk.c tgl_calc_voltage_level()
pub fn tgl_calc_voltage_level<I: IntelCdclkIo>(io: &mut I, cdclk: i32) -> u8 {
    calc_voltage_level(io, cdclk, &[312000, 326400, 556800, 652800])
}

// upstream: intel_cdclk.c rplu_calc_voltage_level()
pub fn rplu_calc_voltage_level<I: IntelCdclkIo>(io: &mut I, cdclk: i32) -> u8 {
    calc_voltage_level(io, cdclk, &[312000, 480000, 556800, 652800])
}

// upstream: intel_cdclk.c xe3lpd_calc_voltage_level()
pub fn xe3lpd_calc_voltage_level(_cdclk: i32) -> u8 {
    0
}

// upstream: intel_cdclk.c icl_readout_refclk()
pub fn icl_readout_refclk<I: IntelCdclkIo>(io: &mut I, config: &mut IntelCdclkConfig) {
    let dssm = rd(io, "SKL_DSSM") & c(io, "ICL_DSSM_CDCLK_PLL_REFCLK_MASK");
    config.refclk = match dssm {
        x if x == c(io, "ICL_DSSM_CDCLK_PLL_REFCLK_19_2MHz") => 19200,
        x if x == c(io, "ICL_DSSM_CDCLK_PLL_REFCLK_38_4MHz") => 38400,
        _ => {
            if dssm != c(io, "ICL_DSSM_CDCLK_PLL_REFCLK_24MHz") {
                hook(io, "MISSING_CASE:ICL_DSSM_REFCLK", &[dssm as i64]);
            }
            24000
        }
    };
}

// upstream: intel_cdclk.c bxt_de_pll_readout()
pub fn bxt_de_pll_readout<I: IntelCdclkIo>(
    io: &mut I,
    display: &IntelDisplay,
    config: &mut IntelCdclkConfig,
) {
    config.refclk = if display.platform.dg2 {
        38400
    } else if display.display_ver >= 11 {
        icl_readout_refclk(io, config);
        config.refclk
    } else {
        19200
    };
    let val = rd(io, "BXT_DE_PLL_ENABLE");
    if val & c(io, "BXT_DE_PLL_PLL_ENABLE") == 0 || val & c(io, "BXT_DE_PLL_LOCK") == 0 {
        config.vco = 0;
        return;
    }
    let ratio = if display.display_ver >= 11 {
        val & c(io, "ICL_CDCLK_PLL_RATIO_MASK")
    } else {
        rd(io, "BXT_DE_PLL_CTL") & c(io, "BXT_DE_PLL_RATIO_MASK")
    };
    config.vco = ratio as i32 * config.refclk;
}

// upstream: intel_cdclk.c bxt_get_cdclk()
pub fn bxt_get_cdclk<I: IntelCdclkIo>(
    io: &mut I,
    display: &IntelDisplay,
    config: &mut IntelCdclkConfig,
) {
    bxt_de_pll_readout(io, display, config);
    config.bypass = if display.display_ver >= 12 {
        config.refclk / 2
    } else if display.display_ver >= 11 {
        50000
    } else {
        config.refclk
    };
    if config.vco == 0 {
        config.cdclk = config.bypass;
    } else {
        let divider = rd(io, "CDCLK_CTL") & c(io, "BXT_CDCLK_CD2X_DIV_SEL_MASK");
        let div = if divider == c(io, "BXT_CDCLK_CD2X_DIV_SEL_1") {
            2
        } else if divider == c(io, "BXT_CDCLK_CD2X_DIV_SEL_1_5") {
            3
        } else if divider == c(io, "BXT_CDCLK_CD2X_DIV_SEL_2") {
            4
        } else if divider == c(io, "BXT_CDCLK_CD2X_DIV_SEL_4") {
            8
        } else {
            hook(io, "MISSING_CASE:BXT_CD2X_DIV", &[divider as i64]);
            return;
        };
        let mut squash_ctl = 0;
        if hook(io, "HAS_CDCLK_SQUASH", &[]) != 0 {
            squash_ctl = rd(io, "CDCLK_SQUASH_CTL");
        }
        if squash_ctl & c(io, "CDCLK_SQUASH_ENABLE") != 0 {
            let size = (hook(
                io,
                "REG_FIELD_GET:CDCLK_SQUASH_WINDOW_SIZE_MASK",
                &[squash_ctl as i64],
            ) as u32
                + 1) as i64;
            let waveform = (hook(
                io,
                "REG_FIELD_GET:CDCLK_SQUASH_WAVEFORM_MASK",
                &[squash_ctl as i64],
            ) as u32
                >> (16 - size as u32)) as u16;
            config.cdclk = round_closest(
                (hweight16(waveform) as i64) * config.vco as i64,
                size * div as i64,
            ) as i32;
        } else {
            config.cdclk = round_closest(config.vco as i64, div as i64) as i32;
        }
    }
    if display.display_ver_full >= 2000 {
        config.joined_mbus = rd(io, "MBUS_CTL") & c(io, "MBUS_JOIN") != 0;
    }
    config.voltage_level =
        io.platform_calc_voltage_level(display.cdclk.funcs, display, config.cdclk);
}

// upstream: intel_cdclk.c bxt_de_pll_disable()
pub fn bxt_de_pll_disable<I: IntelCdclkIo>(io: &mut I, display: &mut IntelDisplay) {
    wr(io, "BXT_DE_PLL_ENABLE", 0);
    if io.wait("BXT_DE_PLL_ENABLE", c(io, "BXT_DE_PLL_LOCK"), 0, 1) != 0 {
        io.log("error", "timeout waiting for DE PLL unlock\n", &[]);
    }
    display.cdclk.hw.vco = 0;
}

// upstream: intel_cdclk.c bxt_de_pll_enable()
pub fn bxt_de_pll_enable<I: IntelCdclkIo>(io: &mut I, display: &mut IntelDisplay, vco: i32) {
    let ratio = round_closest(vco as i64, display.cdclk.hw.refclk as i64) as u32;
    rmw(io, "BXT_DE_PLL_CTL", c(io, "BXT_DE_PLL_RATIO_MASK"), ratio);
    wr(io, "BXT_DE_PLL_ENABLE", c(io, "BXT_DE_PLL_PLL_ENABLE"));
    if io.wait(
        "BXT_DE_PLL_ENABLE",
        c(io, "BXT_DE_PLL_LOCK"),
        c(io, "BXT_DE_PLL_LOCK"),
        1,
    ) != 0
    {
        io.log("error", "timeout waiting for DE PLL lock\n", &[]);
    }
    display.cdclk.hw.vco = vco;
}

// upstream: intel_cdclk.c icl_cdclk_pll_disable()
pub fn icl_cdclk_pll_disable<I: IntelCdclkIo>(io: &mut I, display: &mut IntelDisplay) {
    if hook(io, "intel_display_wa:INTEL_DISPLAY_WA_13012396614", &[]) != 0 {
        rmw(
            io,
            "CDCLK_CTL",
            c(io, "MDCLK_SOURCE_SEL_MASK"),
            c(io, "MDCLK_SOURCE_SEL_CD2XCLK"),
        );
    }
    rmw(io, "BXT_DE_PLL_ENABLE", c(io, "BXT_DE_PLL_PLL_ENABLE"), 0);
    if io.wait("BXT_DE_PLL_ENABLE", c(io, "BXT_DE_PLL_LOCK"), 0, 1) != 0 {
        io.log("error", "timeout waiting for CDCLK PLL unlock\n", &[]);
    }
    display.cdclk.hw.vco = 0;
}

// upstream: intel_cdclk.c icl_cdclk_pll_enable()
pub fn icl_cdclk_pll_enable<I: IntelCdclkIo>(io: &mut I, display: &mut IntelDisplay, vco: i32) {
    let ratio = round_closest(vco as i64, display.cdclk.hw.refclk as i64) as u32;
    let mut val = ratio;
    wr(io, "BXT_DE_PLL_ENABLE", val);
    val |= c(io, "BXT_DE_PLL_PLL_ENABLE");
    wr(io, "BXT_DE_PLL_ENABLE", val);
    if io.wait(
        "BXT_DE_PLL_ENABLE",
        c(io, "BXT_DE_PLL_LOCK"),
        c(io, "BXT_DE_PLL_LOCK"),
        1,
    ) != 0
    {
        io.log("error", "timeout waiting for CDCLK PLL lock\n", &[]);
    }
    display.cdclk.hw.vco = vco;
}

// upstream: intel_cdclk.c adlp_cdclk_pll_crawl()
pub fn adlp_cdclk_pll_crawl<I: IntelCdclkIo>(io: &mut I, display: &mut IntelDisplay, vco: i32) {
    let ratio = round_closest(vco as i64, display.cdclk.hw.refclk as i64) as u32;
    let mut val = ratio | c(io, "BXT_DE_PLL_PLL_ENABLE");
    wr(io, "BXT_DE_PLL_ENABLE", val);
    val |= c(io, "BXT_DE_PLL_FREQ_REQ");
    wr(io, "BXT_DE_PLL_ENABLE", val);
    let mask = c(io, "BXT_DE_PLL_LOCK") | c(io, "BXT_DE_PLL_FREQ_REQ_ACK");
    if io.wait("BXT_DE_PLL_ENABLE", mask, mask, 1) != 0 {
        io.log(
            "error",
            "timeout waiting for FREQ change request ack\n",
            &[],
        );
    }
    val &= !c(io, "BXT_DE_PLL_FREQ_REQ");
    wr(io, "BXT_DE_PLL_ENABLE", val);
    display.cdclk.hw.vco = vco;
}

// upstream: intel_cdclk.c bxt_cdclk_cd2x_pipe()
pub fn bxt_cdclk_cd2x_pipe<I: IntelCdclkIo>(io: &mut I, display: &IntelDisplay, pipe: i32) -> u32 {
    let (none, value) = if display.display_ver >= 12 {
        ("TGL_CDCLK_CD2X_PIPE_NONE", "TGL_CDCLK_CD2X_PIPE(pipe)")
    } else if display.display_ver >= 11 {
        ("ICL_CDCLK_CD2X_PIPE_NONE", "ICL_CDCLK_CD2X_PIPE(pipe)")
    } else {
        ("BXT_CDCLK_CD2X_PIPE_NONE", "BXT_CDCLK_CD2X_PIPE(pipe)")
    };
    if pipe == INVALID_PIPE {
        c(io, none)
    } else {
        hook(io, value, &[pipe as i64]) as u32
    }
}

// upstream: intel_cdclk.c bxt_cdclk_cd2x_div_sel()
pub fn bxt_cdclk_cd2x_div_sel<I: IntelCdclkIo>(
    io: &mut I,
    display: &IntelDisplay,
    cdclk: i32,
    vco: i32,
    waveform: u16,
) -> u32 {
    let divider = cdclk_divider(cdclk, vco, waveform);
    let ret = match divider {
        2 => c(io, "BXT_CDCLK_CD2X_DIV_SEL_1"),
        3 => c(io, "BXT_CDCLK_CD2X_DIV_SEL_1_5"),
        4 => c(io, "BXT_CDCLK_CD2X_DIV_SEL_2"),
        8 => c(io, "BXT_CDCLK_CD2X_DIV_SEL_4"),
        _ => {
            if cdclk != display.cdclk.hw.bypass {
                hook(io, "drm_WARN:cdclk_bypass", &[cdclk as i64]);
            }
            if vco != 0 {
                hook(io, "drm_WARN:vco_nonzero", &[vco as i64]);
            }
            c(io, "BXT_CDCLK_CD2X_DIV_SEL_1")
        }
    };
    if display.display_ver >= 30 && ret != c(io, "BXT_CDCLK_CD2X_DIV_SEL_1") {
        hook(io, "drm_WARN:xe3_cd2x_divider", &[ret as i64]);
    }
    ret
}

// upstream: intel_cdclk.c cdclk_squash_waveform()
pub fn cdclk_squash_waveform<I: IntelCdclkIo>(
    io: &mut I,
    display: &IntelDisplay,
    cdclk: i32,
) -> u16 {
    if cdclk == display.cdclk.hw.bypass {
        return 0;
    }
    for entry in table_for(display) {
        if entry.refclk as i32 == display.cdclk.hw.refclk && entry.cdclk as i32 == cdclk {
            return entry.waveform;
        }
    }
    hook(
        io,
        "drm_WARN:cdclk_not_valid_for_refclk",
        &[cdclk as i64, display.cdclk.hw.refclk as i64],
    );
    0xffff
}

// upstream: intel_cdclk.c icl_cdclk_pll_update()
pub fn icl_cdclk_pll_update<I: IntelCdclkIo>(io: &mut I, display: &mut IntelDisplay, vco: i32) {
    if display.cdclk.hw.vco != 0 && display.cdclk.hw.vco != vco {
        icl_cdclk_pll_disable(io, display);
    }
    if display.cdclk.hw.vco != vco {
        icl_cdclk_pll_enable(io, display, vco);
    }
}

// upstream: intel_cdclk.c bxt_cdclk_pll_update()
pub fn bxt_cdclk_pll_update<I: IntelCdclkIo>(io: &mut I, display: &mut IntelDisplay, vco: i32) {
    if display.cdclk.hw.vco != 0 && display.cdclk.hw.vco != vco {
        bxt_de_pll_disable(io, display);
    }
    if display.cdclk.hw.vco != vco {
        bxt_de_pll_enable(io, display, vco);
    }
}

// upstream: intel_cdclk.c dg2_cdclk_squash_program()
pub fn dg2_cdclk_squash_program<I: IntelCdclkIo>(io: &mut I, waveform: u16) {
    let squash_ctl = if waveform != 0 {
        c(io, "CDCLK_SQUASH_ENABLE") | c(io, "CDCLK_SQUASH_WINDOW_SIZE(0xf)") | waveform as u32
    } else {
        0
    };
    wr(io, "CDCLK_SQUASH_CTL", squash_ctl);
}

// upstream: intel_cdclk.c cdclk_pll_is_unknown()
pub fn cdclk_pll_is_unknown(vco: u32) -> bool {
    vco == !0u32
}

// upstream: intel_cdclk.c mdclk_source_is_cdclk_pll()
pub fn mdclk_source_is_cdclk_pll(display: &IntelDisplay) -> bool {
    display.display_ver_full >= 2000
}

// upstream: intel_cdclk.c xe2lpd_mdclk_source_sel()
pub fn xe2lpd_mdclk_source_sel<I: IntelCdclkIo>(io: &I, display: &IntelDisplay) -> u32 {
    if mdclk_source_is_cdclk_pll(display) {
        c(io, "MDCLK_SOURCE_SEL_CDCLK_PLL")
    } else {
        c(io, "MDCLK_SOURCE_SEL_CD2XCLK")
    }
}

// upstream: intel_cdclk.c intel_mdclk_cdclk_ratio()
pub fn intel_mdclk_cdclk_ratio(display: &IntelDisplay, config: &IntelCdclkConfig) -> i32 {
    if mdclk_source_is_cdclk_pll(display) {
        div_round_up(config.vco as i64, config.cdclk as i64) as i32
    } else {
        2
    }
}

// upstream: intel_cdclk.c xe2lpd_mdclk_cdclk_ratio_program()
pub fn xe2lpd_mdclk_cdclk_ratio_program<I: IntelCdclkIo>(
    io: &mut I,
    display: &IntelDisplay,
    config: &IntelCdclkConfig,
) {
    hook(
        io,
        "intel_dbuf_mdclk_cdclk_ratio_update",
        &[
            intel_mdclk_cdclk_ratio(display, config) as i64,
            config.joined_mbus as i64,
        ],
    );
}

// upstream: intel_cdclk.c cdclk_compute_crawl_and_squash_midpoint()
pub fn cdclk_compute_crawl_and_squash_midpoint<I: IntelCdclkIo>(
    io: &mut I,
    display: &IntelDisplay,
    old: &IntelCdclkConfig,
    new: &IntelCdclkConfig,
    mid: &mut IntelCdclkConfig,
) -> bool {
    if cdclk_pll_is_unknown(old.vco as u32) {
        return false;
    }
    if hook(io, "HAS_CDCLK_CRAWL", &[]) == 0 || hook(io, "HAS_CDCLK_SQUASH", &[]) == 0 {
        return false;
    }
    let old_waveform = cdclk_squash_waveform(io, display, old.cdclk);
    let new_waveform = cdclk_squash_waveform(io, display, new.cdclk);
    if old.vco == 0 || new.vco == 0 || old.vco == new.vco || old_waveform == new_waveform {
        return false;
    }
    let old_div = cdclk_divider(old.cdclk, old.vco, old_waveform);
    let new_div = cdclk_divider(new.cdclk, new.vco, new_waveform);
    if old_div != new_div {
        hook(
            io,
            "drm_WARN:crawl_squash_divider",
            &[old_div as i64, new_div as i64],
        );
        return false;
    }
    *mid = *new;
    let (mid_waveform, mid_div) =
        if cdclk_squash_divider(new_waveform) > cdclk_squash_divider(old_waveform) {
            mid.vco = old.vco;
            (new_waveform, old_div)
        } else {
            mid.vco = new.vco;
            (old_waveform, new_div)
        };
    mid.cdclk = round_closest(
        (cdclk_squash_divider(mid_waveform) as i64) * mid.vco as i64,
        (CDCLK_SQUASH_LEN * mid_div) as i64,
    ) as i32;
    if mid.cdclk < min(old.cdclk, new.cdclk) {
        hook(
            io,
            "drm_WARN:mid_cdclk_below_endpoints",
            &[mid.cdclk as i64],
        );
    }
    if mid.cdclk > display.cdclk.max_cdclk_freq {
        hook(io, "drm_WARN:mid_cdclk_above_max", &[mid.cdclk as i64]);
    }
    if cdclk_squash_waveform(io, display, mid.cdclk) != mid_waveform {
        hook(io, "drm_WARN:mid_waveform", &[mid.cdclk as i64]);
    }
    true
}

// upstream: intel_cdclk.c pll_enable_wa_needed()
pub fn pll_enable_wa_needed(display: &IntelDisplay) -> bool {
    (display.display_ver_full == 2000 || display.display_ver_full == 1400 || display.platform.dg2)
        && display.cdclk.hw.vco > 0
}

// upstream: intel_cdclk.c bxt_cdclk_ctl()
pub fn bxt_cdclk_ctl<I: IntelCdclkIo>(
    io: &mut I,
    display: &IntelDisplay,
    config: &IntelCdclkConfig,
    pipe: i32,
) -> u32 {
    let waveform = cdclk_squash_waveform(io, display, config.cdclk);
    let mut val = bxt_cdclk_cd2x_div_sel(io, display, config.cdclk, config.vco, waveform);
    if display.display_ver < 30 {
        val |= bxt_cdclk_cd2x_pipe(io, display, pipe);
    }
    if (display.platform.geminilake || display.platform.broxton) && config.cdclk >= 500000 {
        val |= c(io, "BXT_CDCLK_SSA_PRECHARGE_ENABLE");
    }
    if display.display_ver_full >= 2000 {
        if hook(io, "intel_display_wa:INTEL_DISPLAY_WA_13012396614", &[]) != 0 && config.vco == 0 {
            val |= c(io, "MDCLK_SOURCE_SEL_CD2XCLK");
        } else {
            val |= xe2lpd_mdclk_source_sel(io, display);
        }
    } else {
        val |= skl_cdclk_decimal(config.cdclk) as u32;
    }
    val
}

// upstream: intel_cdclk.c _bxt_set_cdclk()
pub fn _bxt_set_cdclk<I: IntelCdclkIo>(
    io: &mut I,
    display: &mut IntelDisplay,
    config: &IntelCdclkConfig,
    pipe: i32,
) {
    let cdclk = config.cdclk;
    let vco = config.vco;
    if hook(io, "HAS_CDCLK_CRAWL", &[]) != 0
        && display.cdclk.hw.vco > 0
        && vco > 0
        && !cdclk_pll_is_unknown(display.cdclk.hw.vco as u32)
    {
        if display.cdclk.hw.vco != vco {
            adlp_cdclk_pll_crawl(io, display, vco);
        }
    } else if display.display_ver >= 11 {
        if pll_enable_wa_needed(display) {
            dg2_cdclk_squash_program(io, 0);
        }
        icl_cdclk_pll_update(io, display, vco);
    } else {
        bxt_cdclk_pll_update(io, display, vco);
    }
    if hook(io, "HAS_CDCLK_SQUASH", &[]) != 0 {
        let waveform = cdclk_squash_waveform(io, display, cdclk);
        dg2_cdclk_squash_program(io, waveform);
    }
    let cdclk_ctl = bxt_cdclk_ctl(io, display, config, pipe);
    wr(io, "CDCLK_CTL", cdclk_ctl);
    if pipe != INVALID_PIPE {
        hook(io, "intel_crtc_wait_for_next_vblank", &[pipe as i64]);
    }
}

// upstream: intel_cdclk.c bxt_set_cdclk()
pub fn bxt_set_cdclk<I: IntelCdclkIo>(
    io: &mut I,
    display: &mut IntelDisplay,
    config: &IntelCdclkConfig,
    pipe: i32,
) {
    let cdclk = config.cdclk;
    let mut ret = 0i64;
    if display.display_ver >= 14 || display.platform.dg2 {
    } else if display.display_ver >= 11 {
        ret = hook(
            io,
            "intel_parent_pcode_request:SKL_PCODE_CDCLK_CONTROL",
            &[
                c(io, "SKL_CDCLK_PREPARE_FOR_CHANGE") as i64,
                c(io, "SKL_CDCLK_READY_FOR_CHANGE") as i64,
                c(io, "SKL_CDCLK_READY_FOR_CHANGE") as i64,
                3,
            ],
        );
    } else {
        ret = hook(
            io,
            "intel_parent_pcode_write_timeout:HSW_PCODE_DE_WRITE_FREQ_REQ",
            &[0x80000000, 2],
        );
    }
    if ret != 0 {
        io.log(
            "error",
            "Failed to inform PCU about cdclk change (err %d, freq %d)\n",
            &[ret, cdclk as i64],
        );
        return;
    }
    if display.display_ver_full >= 2000 && cdclk < display.cdclk.hw.cdclk {
        xe2lpd_mdclk_cdclk_ratio_program(io, display, config);
    }
    let mut mid = IntelCdclkConfig::default();
    if cdclk_compute_crawl_and_squash_midpoint(io, display, &display.cdclk.hw, config, &mut mid) {
        _bxt_set_cdclk(io, display, &mid, pipe);
        _bxt_set_cdclk(io, display, config, pipe);
    } else {
        _bxt_set_cdclk(io, display, config, pipe);
    }
    if display.display_ver_full >= 2000 && cdclk > display.cdclk.hw.cdclk {
        xe2lpd_mdclk_cdclk_ratio_program(io, display, config);
    }
    if display.display_ver >= 14 {
    } else if display.display_ver >= 11 && !display.platform.dg2 {
        ret = hook(
            io,
            "intel_parent_pcode_write:SKL_PCODE_CDCLK_CONTROL",
            &[config.voltage_level as i64],
        );
    }
    if display.display_ver < 11 {
        ret = hook(
            io,
            "intel_parent_pcode_write_timeout:HSW_PCODE_DE_WRITE_FREQ_REQ",
            &[config.voltage_level as i64, 2],
        );
    }
    if ret != 0 {
        io.log(
            "error",
            "PCode CDCLK freq set failed, (err %d, freq %d)\n",
            &[ret, cdclk as i64],
        );
        return;
    }
    hook(io, "intel_update_cdclk", &[]);
    if display.display_ver >= 11 {
        display.cdclk.hw.voltage_level = config.voltage_level;
    }
}

// upstream: intel_cdclk.c bxt_sanitize_cdclk()
pub fn bxt_sanitize_cdclk<I: IntelCdclkIo>(io: &mut I, display: &mut IntelDisplay) {
    hook(io, "intel_update_cdclk", &[]);
    hook(
        io,
        "intel_cdclk_dump_config",
        &[display.cdclk.hw.cdclk as i64, display.cdclk.hw.vco as i64],
    );
    if display.cdclk.hw.vco == 0 || display.cdclk.hw.cdclk == display.cdclk.hw.bypass {
        goto_sanitize_skl(display);
        return;
    }
    let cdclk = bxt_calc_cdclk(io, display, display.cdclk.hw.cdclk);
    if cdclk != display.cdclk.hw.cdclk {
        goto_sanitize_skl(display);
        return;
    }
    let vco = bxt_calc_cdclk_pll_vco(io, display, cdclk);
    if vco != display.cdclk.hw.vco {
        goto_sanitize_skl(display);
        return;
    }
    let mut cdctl = rd(io, "CDCLK_CTL");
    let expected = bxt_cdclk_ctl(io, display, &display.cdclk.hw, INVALID_PIPE);
    let pipe_mask = bxt_cdclk_cd2x_pipe(io, display, INVALID_PIPE);
    cdctl &= !pipe_mask;
    cdctl |= pipe_mask;
    if cdctl != expected {
        if display.display_ver < 20 {
            cdctl = (cdctl & !c(io, "CDCLK_FREQ_DECIMAL_MASK"))
                | (expected & c(io, "CDCLK_FREQ_DECIMAL_MASK"));
        }
        if cdctl != expected {
            goto_sanitize_skl(display);
            return;
        }
        let observed = rd(io, "CDCLK_CTL");
        io.log(
            "debug",
            "Sanitizing CDCLK decimal divider (CDCLK_CTL 0x%x, expected 0x%x)\n",
            &[observed as i64, expected as i64],
        );
        wr(io, "CDCLK_CTL", expected);
    }
}

// upstream: intel_cdclk.c bxt_cdclk_init_hw()
pub fn bxt_cdclk_init_hw<I: IntelCdclkIo>(io: &mut I, display: &mut IntelDisplay) {
    bxt_sanitize_cdclk(io, display);
    if display.cdclk.hw.cdclk != 0 && display.cdclk.hw.vco != 0 {
        return;
    }
    let mut config = display.cdclk.hw;
    config.cdclk = bxt_calc_cdclk(io, display, 0);
    config.vco = bxt_calc_cdclk_pll_vco(io, display, config.cdclk);
    config.voltage_level =
        io.platform_calc_voltage_level(display.cdclk.funcs, display, config.cdclk);
    bxt_set_cdclk(io, display, &config, INVALID_PIPE);
}

// upstream: intel_cdclk.c bxt_cdclk_uninit_hw()
pub fn bxt_cdclk_uninit_hw<I: IntelCdclkIo>(io: &mut I, display: &mut IntelDisplay) {
    let mut config = display.cdclk.hw;
    config.cdclk = config.bypass;
    config.vco = 0;
    config.voltage_level =
        io.platform_calc_voltage_level(display.cdclk.funcs, display, config.cdclk);
    bxt_set_cdclk(io, display, &config, INVALID_PIPE);
}

// upstream: intel_cdclk.c intel_cdclk_init_hw()
pub fn intel_cdclk_init_hw<I: IntelCdclkIo>(io: &mut I, display: &mut IntelDisplay) {
    if display.display_ver >= 10 || display.platform.broxton {
        bxt_cdclk_init_hw(io, display);
    } else if display.display_ver == 9 {
        skl_cdclk_init_hw(io, display);
    }
}

// upstream: intel_cdclk.c intel_cdclk_uninit_hw()
pub fn intel_cdclk_uninit_hw<I: IntelCdclkIo>(io: &mut I, display: &mut IntelDisplay) {
    if display.display_ver >= 10 || display.platform.broxton {
        bxt_cdclk_uninit_hw(io, display);
    } else if display.display_ver == 9 {
        skl_cdclk_uninit_hw(io, display);
    }
}

// upstream: intel_cdclk.c intel_cdclk_can_crawl_and_squash()
pub fn intel_cdclk_can_crawl_and_squash<I: IntelCdclkIo>(
    io: &mut I,
    display: &IntelDisplay,
    a: &IntelCdclkConfig,
    b: &IntelCdclkConfig,
) -> bool {
    if cdclk_pll_is_unknown(a.vco as u32) {
        hook(io, "drm_WARN:unknown_cdclk_vco", &[a.vco as i64]);
    }
    if a.vco == 0 || b.vco == 0 {
        return false;
    }
    if hook(io, "HAS_CDCLK_CRAWL", &[]) == 0 || hook(io, "HAS_CDCLK_SQUASH", &[]) == 0 {
        return false;
    }
    let old_waveform = cdclk_squash_waveform(io, display, a.cdclk);
    let new_waveform = cdclk_squash_waveform(io, display, b.cdclk);
    a.vco != b.vco && old_waveform != new_waveform
}

// upstream: intel_cdclk.c intel_cdclk_can_crawl()
pub fn intel_cdclk_can_crawl<I: IntelCdclkIo>(
    io: &mut I,
    _display: &IntelDisplay,
    a: &IntelCdclkConfig,
    b: &IntelCdclkConfig,
) -> bool {
    if hook(io, "HAS_CDCLK_CRAWL", &[]) == 0 {
        return false;
    }
    let a_div = round_closest(a.vco as i64, a.cdclk as i64);
    let b_div = round_closest(b.vco as i64, b.cdclk as i64);
    a.vco != 0 && b.vco != 0 && a.vco != b.vco && a_div == b_div && a.refclk == b.refclk
}

// upstream: intel_cdclk.c intel_cdclk_can_squash()
pub fn intel_cdclk_can_squash<I: IntelCdclkIo>(
    io: &mut I,
    _display: &IntelDisplay,
    a: &IntelCdclkConfig,
    b: &IntelCdclkConfig,
) -> bool {
    if hook(io, "HAS_CDCLK_SQUASH", &[]) == 0 {
        return false;
    }
    a.cdclk != b.cdclk && a.vco != 0 && a.vco == b.vco && a.refclk == b.refclk
}

// upstream: intel_cdclk.c intel_cdclk_clock_changed()
pub fn intel_cdclk_clock_changed(a: &IntelCdclkConfig, b: &IntelCdclkConfig) -> bool {
    a.cdclk != b.cdclk || a.vco != b.vco || a.refclk != b.refclk
}

// upstream: intel_cdclk.c intel_cdclk_can_cd2x_update()
pub fn intel_cdclk_can_cd2x_update<I: IntelCdclkIo>(
    io: &mut I,
    display: &IntelDisplay,
    a: &IntelCdclkConfig,
    b: &IntelCdclkConfig,
) -> bool {
    if display.display_ver < 10 && !display.platform.broxton {
        return false;
    }
    if hook(io, "HAS_CDCLK_SQUASH", &[]) != 0 {
        return false;
    }
    a.cdclk != b.cdclk && a.vco != 0 && a.vco == b.vco && a.refclk == b.refclk
}

// upstream: intel_cdclk.c intel_cdclk_changed()
pub fn intel_cdclk_changed(a: &IntelCdclkConfig, b: &IntelCdclkConfig) -> bool {
    intel_cdclk_clock_changed(a, b) || a.voltage_level != b.voltage_level
}

// upstream: intel_cdclk.c intel_cdclk_dump_config()
pub fn intel_cdclk_dump_config<I: IntelCdclkIo>(
    io: &mut I,
    config: &IntelCdclkConfig,
    context: &'static str,
) {
    io.log_cdclk_config(context, config);
}

// upstream: intel_cdclk.c intel_pcode_notify()
pub fn intel_pcode_notify<I: IntelCdclkIo>(
    io: &mut I,
    display: &IntelDisplay,
    voltage_level: u8,
    active_pipe_count: u8,
    cdclk: u16,
    cdclk_update_valid: bool,
    pipe_count_update_valid: bool,
) {
    if !display.platform.dg2 {
        return;
    }
    let mut update_mask = (((cdclk as u32) << 16) & 0x03ff_0000)
        | (((active_pipe_count as u32) << 28) & 0x7000_0000)
        | (voltage_level as u32 & 0x3);
    if cdclk_update_valid {
        update_mask |= 1 << 27;
    }
    if pipe_count_update_valid {
        update_mask |= 1 << 31;
    }
    let prepare = c(io, "SKL_CDCLK_PREPARE_FOR_CHANGE") | update_mask;
    let ret = hook(
        io,
        "intel_parent_pcode_request:SKL_PCODE_CDCLK_CONTROL",
        &[
            prepare as i64,
            c(io, "SKL_CDCLK_READY_FOR_CHANGE") as i64,
            c(io, "SKL_CDCLK_READY_FOR_CHANGE") as i64,
            3,
        ],
    );
    if ret != 0 {
        io.log(
            "error",
            "Failed to inform PCU about display config (err %d)\n",
            &[ret],
        );
    }
}

// upstream: intel_cdclk.c intel_set_cdclk()
pub fn intel_set_cdclk<I: IntelCdclkIo>(
    io: &mut I,
    display: &mut IntelDisplay,
    config: &IntelCdclkConfig,
    pipe: i32,
    context: &'static str,
) {
    if !intel_cdclk_changed(&display.cdclk.hw, config) {
        return;
    }
    if display.cdclk.funcs == 0 {
        hook(io, "drm_WARN_ON_ONCE:no_set_cdclk", &[]);
        return;
    }
    intel_cdclk_dump_config(io, config, context);
    hook(io, "for_each_intel_encoder_with_psr:intel_psr_pause", &[]);
    hook(io, "intel_audio_cdclk_change_pre", &[]);
    hook(io, "mutex_lock:gmbus.mutex", &[]);
    hook(io, "for_each_intel_dp:mutex_lock_nest_lock_aux", &[]);
    intel_cdclk_set_cdclk(io, display, config, pipe);
    hook(io, "for_each_intel_dp:mutex_unlock_aux", &[]);
    hook(io, "mutex_unlock:gmbus.mutex", &[]);
    hook(io, "for_each_intel_encoder_with_psr:intel_psr_resume", &[]);
    hook(io, "intel_audio_cdclk_change_post", &[]);
    if intel_cdclk_changed(&display.cdclk.hw, config) {
        hook(io, "drm_WARN:cdclk_state_mismatch", &[]);
        intel_cdclk_dump_config(io, &display.cdclk.hw, "[hw state]");
        intel_cdclk_dump_config(io, config, "[sw state]");
    }
}

// upstream: intel_cdclk.c dg2_power_well_count()
pub fn dg2_power_well_count(display: &IntelDisplay, state: &IntelCdclkAtomicState) -> u32 {
    if display.platform.dg2 {
        state.active_pipes.count_ones()
    } else {
        0
    }
}

// upstream: intel_cdclk.c intel_cdclk_pcode_pre_notify()
pub fn intel_cdclk_pcode_pre_notify<I: IntelCdclkIo>(io: &mut I, state: &IntelAtomicState) {
    let display = &state.display;
    let old = &state.old_cdclk;
    let new = &state.cdclk;
    if !intel_cdclk_changed(&old.actual, &new.actual)
        && dg2_power_well_count(display, old) == dg2_power_well_count(display, new)
    {
        return;
    }
    let voltage_level = 3u8;
    let change_cdclk = new.actual.cdclk != old.actual.cdclk;
    let update_pipe_count = dg2_power_well_count(display, new) > dg2_power_well_count(display, old);
    let cdclk = if change_cdclk {
        max(new.actual.cdclk, old.actual.cdclk) as u16
    } else {
        0
    };
    let num_active_pipes = if update_pipe_count {
        dg2_power_well_count(display, new) as u8
    } else {
        0
    };
    intel_pcode_notify(
        io,
        display,
        voltage_level,
        num_active_pipes,
        cdclk,
        change_cdclk,
        update_pipe_count,
    );
}

// upstream: intel_cdclk.c intel_cdclk_pcode_post_notify()
pub fn intel_cdclk_pcode_post_notify<I: IntelCdclkIo>(io: &mut I, state: &IntelAtomicState) {
    let display = &state.display;
    let old = &state.old_cdclk;
    let new = &state.cdclk;
    let voltage_level = new.actual.voltage_level;
    let update_cdclk = new.actual.cdclk != old.actual.cdclk;
    let update_pipe_count = dg2_power_well_count(display, new) < dg2_power_well_count(display, old);
    let cdclk = if update_cdclk {
        new.actual.cdclk as u16
    } else {
        0
    };
    let num_active_pipes = if update_pipe_count {
        dg2_power_well_count(display, new) as u8
    } else {
        0
    };
    intel_pcode_notify(
        io,
        display,
        voltage_level,
        num_active_pipes,
        cdclk,
        update_cdclk,
        update_pipe_count,
    );
}

// upstream: intel_cdclk.c intel_cdclk_is_decreasing_later()
pub fn intel_cdclk_is_decreasing_later(state: &IntelAtomicState) -> bool {
    !state.cdclk.disable_pipes && state.cdclk.actual.cdclk < state.old_cdclk.actual.cdclk
}

// upstream: intel_cdclk.c intel_set_cdclk_pre_plane_update()
pub fn intel_set_cdclk_pre_plane_update<I: IntelCdclkIo>(io: &mut I, state: &mut IntelAtomicState) {
    let old = state.old_cdclk;
    let new = state.cdclk;
    if !intel_cdclk_changed(&old.actual, &new.actual) {
        return;
    }
    if state.display.platform.dg2 {
        intel_cdclk_pcode_pre_notify(io, state);
    }
    let display = &mut state.display;
    let (mut config, pipe) = if new.disable_pipes {
        (new.actual, INVALID_PIPE)
    } else if new.actual.cdclk >= old.actual.cdclk {
        (new.actual, new.pipe)
    } else {
        (old.actual, INVALID_PIPE)
    };
    if !new.disable_pipes {
        config.voltage_level = max(new.actual.voltage_level, old.actual.voltage_level);
    }
    config.joined_mbus = old.actual.joined_mbus;
    if hook(io, "new_cdclk_state.base.changed", &[]) == 0 {
        hook(io, "drm_WARN:cdclk_state_unchanged", &[]);
    }
    intel_set_cdclk(io, display, &config, pipe, "Pre changing CDCLK to");
}

// upstream: intel_cdclk.c intel_set_cdclk_post_plane_update()
pub fn intel_set_cdclk_post_plane_update<I: IntelCdclkIo>(
    io: &mut I,
    state: &mut IntelAtomicState,
) {
    let old = state.old_cdclk;
    let new = state.cdclk;
    if !intel_cdclk_changed(&old.actual, &new.actual) {
        return;
    }
    if state.display.platform.dg2 {
        intel_cdclk_pcode_post_notify(io, state);
    }
    let display = &mut state.display;
    let pipe = if !new.disable_pipes && new.actual.cdclk < old.actual.cdclk {
        new.pipe
    } else {
        INVALID_PIPE
    };
    if hook(io, "new_cdclk_state.base.changed", &[]) == 0 {
        hook(io, "drm_WARN:cdclk_state_unchanged", &[]);
    }
    intel_set_cdclk(io, display, &new.actual, pipe, "Post changing CDCLK to");
}

// upstream: intel_cdclk.c intel_cdclk_ppc()
pub fn intel_cdclk_ppc<I: IntelCdclkIo>(_io: &I, display: &IntelDisplay, double_wide: bool) -> i32 {
    if display.display_ver >= 10 || double_wide {
        2
    } else {
        1
    }
}

// upstream: intel_cdclk.c intel_cdclk_guardband()
pub fn intel_cdclk_guardband(display: &IntelDisplay) -> i32 {
    if display.display_ver >= 9 || display.platform.broadwell || display.platform.haswell {
        100
    } else if display.platform.cherryview {
        95
    } else {
        90
    }
}

// upstream: intel_cdclk.c _intel_pixel_rate_to_cdclk()
pub fn _intel_pixel_rate_to_cdclk<I: IntelCdclkIo>(
    io: &I,
    display: &IntelDisplay,
    crtc_state: &IntelCrtcState,
    pixel_rate: i32,
) -> i32 {
    let ppc = intel_cdclk_ppc(io, display, crtc_state.double_wide);
    let guardband = intel_cdclk_guardband(display);
    div_round_up((pixel_rate * 100) as i64, (guardband * ppc) as i64) as i32
}

// upstream: intel_cdclk.c intel_pixel_rate_to_cdclk()
pub fn intel_pixel_rate_to_cdclk<I: IntelCdclkIo>(
    io: &I,
    display: &IntelDisplay,
    crtc_state: &IntelCrtcState,
) -> i32 {
    _intel_pixel_rate_to_cdclk(io, display, crtc_state, crtc_state.pixel_rate)
}

// upstream: intel_cdclk.c intel_planes_min_cdclk()
pub fn intel_planes_min_cdclk<I: IntelCdclkIo>(io: &mut I, crtc_state: &IntelCrtcState) -> i32 {
    hook(
        io,
        "for_each_intel_plane_on_crtc:max_plane_min_cdclk",
        &[crtc_state.plane_min_cdclk as i64],
    ) as i32
}

// upstream: intel_cdclk.c intel_crtc_min_cdclk()
pub fn intel_crtc_min_cdclk<I: IntelCdclkIo>(
    io: &mut I,
    display: &IntelDisplay,
    crtc_state: &IntelCrtcState,
) -> i32 {
    if !crtc_state.enable {
        return 0;
    }
    let mut min_cdclk = intel_pixel_rate_to_cdclk(io, display, crtc_state);
    for (name, arg) in [
        ("intel_crtc_bw_min_cdclk", 0),
        ("intel_fbc_min_cdclk", 0),
        ("hsw_ips_min_cdclk", 0),
        ("intel_audio_min_cdclk", 0),
        ("vlv_dsi_min_cdclk", 0),
        ("intel_planes_min_cdclk", 0),
        ("intel_vdsc_min_cdclk", 0),
    ] {
        let val = if name == "intel_planes_min_cdclk" {
            intel_planes_min_cdclk(io, crtc_state)
        } else {
            hook(io, name, &[arg]) as i32
        };
        min_cdclk = max(min_cdclk, val);
    }
    min_cdclk
}

// upstream: intel_cdclk.c intel_cdclk_update_crtc_min_cdclk()
pub fn intel_cdclk_update_crtc_min_cdclk<I: IntelCdclkIo>(
    io: &mut I,
    state: &mut IntelAtomicState,
    pipe: usize,
    old_min: i32,
    new_min: i32,
    need_calc: &mut bool,
) -> i32 {
    let allow_decrease = hook(io, "intel_any_crtc_needs_modeset", &[]) != 0;
    if new_min == old_min || (!allow_decrease && new_min < old_min) {
        return 0;
    }
    let old_min = state.cdclk.min_cdclk[pipe];
    if new_min == old_min || (!allow_decrease && new_min < old_min) {
        return 0;
    }
    state.cdclk.min_cdclk[pipe] = new_min;
    let ret = hook(io, "intel_atomic_lock_global_state", &[]);
    if ret != 0 {
        return ret as i32;
    }
    *need_calc = true;
    hook(
        io,
        "drm_dbg_kms:crtc_min_cdclk",
        &[pipe as i64, old_min as i64, new_min as i64],
    );
    0
}

// upstream: intel_cdclk.c intel_cdclk_update_crtc_min_voltage_level()
pub fn intel_cdclk_update_crtc_min_voltage_level<I: IntelCdclkIo>(
    io: &mut I,
    state: &mut IntelAtomicState,
    pipe: usize,
    old_min: u8,
    new_min: u8,
    need_calc: &mut bool,
) -> i32 {
    let allow_decrease = hook(io, "intel_any_crtc_needs_modeset", &[]) != 0;
    if new_min == old_min || (!allow_decrease && new_min < old_min) {
        return 0;
    }
    let old_min = state.cdclk.min_voltage_level[pipe];
    if new_min == old_min || (!allow_decrease && new_min < old_min) {
        return 0;
    }
    state.cdclk.min_voltage_level[pipe] = new_min;
    let ret = hook(io, "intel_atomic_lock_global_state", &[]);
    if ret != 0 {
        return ret as i32;
    }
    *need_calc = true;
    hook(
        io,
        "drm_dbg_kms:crtc_min_voltage_level",
        &[pipe as i64, old_min as i64, new_min as i64],
    );
    0
}

// upstream: intel_cdclk.c intel_cdclk_update_dbuf_bw_min_cdclk()
pub fn intel_cdclk_update_dbuf_bw_min_cdclk<I: IntelCdclkIo>(
    io: &mut I,
    state: &mut IntelAtomicState,
    old_min: i32,
    new_min: i32,
    need_calc: &mut bool,
) -> i32 {
    let allow_decrease = hook(io, "intel_any_crtc_needs_modeset", &[]) != 0;
    if new_min == old_min || (!allow_decrease && new_min < old_min) {
        return 0;
    }
    let old_min = state.cdclk.dbuf_bw_min_cdclk;
    if new_min == old_min || (!allow_decrease && new_min < old_min) {
        return 0;
    }
    state.cdclk.dbuf_bw_min_cdclk = new_min;
    let ret = hook(io, "intel_atomic_lock_global_state", &[]);
    if ret != 0 {
        return ret as i32;
    }
    *need_calc = true;
    hook(
        io,
        "drm_dbg_kms:dbuf_bw_min_cdclk",
        &[old_min as i64, new_min as i64],
    );
    0
}

// upstream: intel_cdclk.c glk_cdclk_audio_wa_needed()
pub fn glk_cdclk_audio_wa_needed(display: &IntelDisplay, state: &IntelCdclkAtomicState) -> bool {
    display.platform.geminilake
        && state.enabled_pipes != 0
        && !state.enabled_pipes.is_power_of_two()
}

// upstream: intel_cdclk.c intel_compute_min_cdclk()
pub fn intel_compute_min_cdclk<I: IntelCdclkIo>(io: &mut I, state: &IntelAtomicState) -> i32 {
    let display = &state.display;
    let cdclk = &state.cdclk;
    let mut min_cdclk = max(cdclk.force_min_cdclk, cdclk.dbuf_bw_min_cdclk);
    for pipe in 0..I915_MAX_PIPES {
        min_cdclk = max(min_cdclk, cdclk.min_cdclk[pipe]);
    }
    if glk_cdclk_audio_wa_needed(display, cdclk) {
        min_cdclk = max(min_cdclk, 2 * 96000);
    }
    if min_cdclk > display.cdclk.max_cdclk_freq {
        io.log(
            "debug",
            "required cdclk (%d kHz) exceeds max (%d kHz)\n",
            &[min_cdclk as i64, display.cdclk.max_cdclk_freq as i64],
        );
        return -22;
    }
    min_cdclk
}

// upstream: intel_cdclk.c bxt_compute_min_voltage_level()
pub fn bxt_compute_min_voltage_level<I: IntelCdclkIo>(
    io: &mut I,
    state: &mut IntelAtomicState,
) -> i32 {
    for pipe in 0..I915_MAX_PIPES {
        if state.crtc_mask & (1 << pipe) == 0 {
            continue;
        }
        let min_voltage = if state.crtc[pipe].enable {
            state.crtc[pipe].min_voltage_level
        } else {
            0
        };
        if state.cdclk.min_voltage_level[pipe] == min_voltage {
            continue;
        }
        state.cdclk.min_voltage_level[pipe] = min_voltage;
        let ret = hook(io, "intel_atomic_lock_global_state", &[]);
        if ret != 0 {
            return ret as i32;
        }
    }
    state
        .cdclk
        .min_voltage_level
        .iter()
        .copied()
        .max()
        .unwrap_or(0) as i32
}

// upstream: intel_cdclk.c vlv_modeset_calc_cdclk()
pub fn vlv_modeset_calc_cdclk<I: IntelCdclkIo>(io: &mut I, state: &mut IntelAtomicState) -> i32 {
    let min_cdclk = intel_compute_min_cdclk(io, state);
    if min_cdclk < 0 {
        return min_cdclk;
    }
    let cdclk = vlv_calc_cdclk(io, &mut state.display, min_cdclk);
    state.cdclk.logical.cdclk = cdclk;
    state.cdclk.logical.voltage_level = vlv_calc_voltage_level(io, &mut state.display, cdclk);
    if state.cdclk.active_pipes == 0 {
        let actual = vlv_calc_cdclk(io, &mut state.display, state.cdclk.force_min_cdclk);
        state.cdclk.actual.cdclk = actual;
        state.cdclk.actual.voltage_level = vlv_calc_voltage_level(io, &mut state.display, actual);
    } else {
        state.cdclk.actual = state.cdclk.logical;
    }
    0
}

// upstream: intel_cdclk.c bdw_modeset_calc_cdclk()
pub fn bdw_modeset_calc_cdclk<I: IntelCdclkIo>(io: &mut I, state: &mut IntelAtomicState) -> i32 {
    let min_cdclk = intel_compute_min_cdclk(io, state);
    if min_cdclk < 0 {
        return min_cdclk;
    }
    let cdclk = bdw_calc_cdclk(min_cdclk);
    state.cdclk.logical.cdclk = cdclk;
    state.cdclk.logical.voltage_level = bdw_calc_voltage_level(cdclk);
    if state.cdclk.active_pipes == 0 {
        let actual = bdw_calc_cdclk(state.cdclk.force_min_cdclk);
        state.cdclk.actual.cdclk = actual;
        state.cdclk.actual.voltage_level = bdw_calc_voltage_level(actual);
    } else {
        state.cdclk.actual = state.cdclk.logical;
    }
    0
}

// upstream: intel_cdclk.c skl_dpll0_vco()
pub fn skl_dpll0_vco<I: IntelCdclkIo>(io: &mut I, state: &IntelAtomicState) -> i32 {
    let mut vco = if state.cdclk.logical.vco != 0 {
        state.cdclk.logical.vco
    } else {
        state.display.cdclk.skl_preferred_vco_freq
    };
    for pipe in 0..I915_MAX_PIPES {
        let crtc = &state.crtc[pipe];
        if state.crtc_mask & (1 << pipe) == 0 || !crtc.enable {
            continue;
        }
        if hook(io, "intel_crtc_has_type:INTEL_OUTPUT_EDP", &[pipe as i64]) == 0 {
            continue;
        }
        vco = match crtc.port_clock / 2 {
            108000 | 216000 => 8640000,
            _ => 8100000,
        };
    }
    vco
}

// upstream: intel_cdclk.c skl_modeset_calc_cdclk()
pub fn skl_modeset_calc_cdclk<I: IntelCdclkIo>(io: &mut I, state: &mut IntelAtomicState) -> i32 {
    let min_cdclk = intel_compute_min_cdclk(io, state);
    if min_cdclk < 0 {
        return min_cdclk;
    }
    let vco = skl_dpll0_vco(io, state);
    let cdclk = skl_calc_cdclk(min_cdclk, vco);
    state.cdclk.logical.vco = vco;
    state.cdclk.logical.cdclk = cdclk;
    state.cdclk.logical.voltage_level = skl_calc_voltage_level(cdclk);
    if state.cdclk.active_pipes == 0 {
        let actual = skl_calc_cdclk(state.cdclk.force_min_cdclk, vco);
        state.cdclk.actual.vco = vco;
        state.cdclk.actual.cdclk = actual;
        state.cdclk.actual.voltage_level = skl_calc_voltage_level(actual);
    } else {
        state.cdclk.actual = state.cdclk.logical;
    }
    0
}

// upstream: intel_cdclk.c bxt_modeset_calc_cdclk()
pub fn bxt_modeset_calc_cdclk<I: IntelCdclkIo>(io: &mut I, state: &mut IntelAtomicState) -> i32 {
    let min_cdclk = intel_compute_min_cdclk(io, state);
    if min_cdclk < 0 {
        return min_cdclk;
    }
    let min_voltage_level = bxt_compute_min_voltage_level(io, state);
    if min_voltage_level < 0 {
        return min_voltage_level;
    }
    let cdclk = bxt_calc_cdclk(io, &state.display, min_cdclk);
    let vco = bxt_calc_cdclk_pll_vco(io, &state.display, cdclk);
    state.cdclk.logical.vco = vco;
    state.cdclk.logical.cdclk = cdclk;
    state.cdclk.logical.voltage_level = max(
        min_voltage_level as u8,
        io.platform_calc_voltage_level(state.display.cdclk.funcs, &state.display, cdclk),
    );
    if state.cdclk.active_pipes == 0 {
        let actual = bxt_calc_cdclk(io, &state.display, state.cdclk.force_min_cdclk);
        let actual_vco = bxt_calc_cdclk_pll_vco(io, &state.display, actual);
        state.cdclk.actual.vco = actual_vco;
        state.cdclk.actual.cdclk = actual;
        state.cdclk.actual.voltage_level =
            io.platform_calc_voltage_level(state.display.cdclk.funcs, &state.display, actual);
    } else {
        state.cdclk.actual = state.cdclk.logical;
    }
    0
}

// upstream: intel_cdclk.c fixed_modeset_calc_cdclk()
pub fn fixed_modeset_calc_cdclk<I: IntelCdclkIo>(io: &mut I, state: &IntelAtomicState) -> i32 {
    let min_cdclk = intel_compute_min_cdclk(io, state);
    if min_cdclk < 0 {
        return min_cdclk;
    }
    0
}

// upstream: intel_cdclk.c intel_cdclk_duplicate_state()
pub fn intel_cdclk_duplicate_state(state: &IntelCdclkAtomicState) -> IntelCdclkAtomicState {
    let mut cdclk_state = *state;
    cdclk_state.pipe = INVALID_PIPE;
    cdclk_state.disable_pipes = false;
    cdclk_state
}

// upstream: intel_cdclk.c intel_cdclk_destroy_state()
pub fn intel_cdclk_destroy_state<I: IntelCdclkIo>(io: &mut I, _state: IntelCdclkAtomicState) {
    hook(io, "kfree:intel_cdclk_state", &[]);
}

// upstream: intel_cdclk.c intel_atomic_get_cdclk_state()
pub fn intel_atomic_get_cdclk_state(state: &mut IntelAtomicState) -> &mut IntelCdclkAtomicState {
    &mut state.cdclk
}

// upstream: intel_cdclk.c intel_cdclk_modeset_checks()
pub fn intel_cdclk_modeset_checks<I: IntelCdclkIo>(
    io: &mut I,
    state: &mut IntelAtomicState,
    need_calc: &mut bool,
) -> i32 {
    if hook(io, "intel_any_crtc_enable_changed_or_active_changed", &[]) == 0 {
        return 0;
    }
    let display = state.display;
    let old = state.old_cdclk;
    let new_enabled = hook(io, "intel_calc_enabled_pipes", &[old.enabled_pipes as i64]) as u8;
    let new_active = hook(io, "intel_calc_active_pipes", &[old.active_pipes as i64]) as u8;
    state.cdclk.enabled_pipes = new_enabled;
    state.cdclk.active_pipes = new_active;
    let ret = hook(io, "intel_atomic_lock_global_state", &[]);
    if ret != 0 {
        return ret as i32;
    }
    if (old.active_pipes == 0) != (new_active == 0) {
        *need_calc = true;
    }
    if glk_cdclk_audio_wa_needed(&display, &old)
        != glk_cdclk_audio_wa_needed(&display, &state.cdclk)
    {
        *need_calc = true;
    }
    if dg2_power_well_count(&display, &old) != dg2_power_well_count(&display, &state.cdclk) {
        *need_calc = true;
    }
    0
}

// upstream: intel_cdclk.c intel_crtcs_calc_min_cdclk()
pub fn intel_crtcs_calc_min_cdclk<I: IntelCdclkIo>(
    io: &mut I,
    state: &mut IntelAtomicState,
    need_calc: &mut bool,
) -> i32 {
    for pipe in 0..I915_MAX_PIPES {
        if state.crtc_mask & (1 << pipe) == 0 {
            continue;
        }
        let ret = intel_cdclk_update_crtc_min_cdclk(
            io,
            state,
            pipe,
            state.old_crtc[pipe].min_cdclk,
            state.crtc[pipe].min_cdclk,
            need_calc,
        );
        if ret != 0 {
            return ret;
        }
        let ret = intel_cdclk_update_crtc_min_voltage_level(
            io,
            state,
            pipe,
            state.old_crtc[pipe].min_voltage_level,
            state.crtc[pipe].min_voltage_level,
            need_calc,
        );
        if ret != 0 {
            return ret;
        }
    }
    0
}

// upstream: intel_cdclk.c intel_cdclk_state_set_joined_mbus()
pub fn intel_cdclk_state_set_joined_mbus<I: IntelCdclkIo>(
    io: &mut I,
    state: &mut IntelAtomicState,
    joined_mbus: bool,
) -> i32 {
    state.cdclk.actual.joined_mbus = joined_mbus;
    state.cdclk.logical.joined_mbus = joined_mbus;
    hook(io, "intel_atomic_lock_global_state", &[]) as i32
}

// upstream: intel_cdclk.c intel_cdclk_init()
pub fn intel_cdclk_init<I: IntelCdclkIo>(io: &mut I, _display: &mut IntelDisplay) -> i32 {
    let ret = hook(io, "kzalloc_obj:intel_cdclk_state", &[]);
    if ret == 0 {
        return -12;
    }
    hook(io, "intel_atomic_global_obj_init", &[]);
    0
}

// upstream: intel_cdclk.c intel_cdclk_need_serialize()
pub fn intel_cdclk_need_serialize(
    display: &IntelDisplay,
    old: &IntelCdclkAtomicState,
    new: &IntelCdclkAtomicState,
) -> bool {
    intel_cdclk_changed(&old.actual, &new.actual)
        || dg2_power_well_count(display, old) != dg2_power_well_count(display, new)
}

// upstream: intel_cdclk.c intel_modeset_calc_cdclk()
pub fn intel_modeset_calc_cdclk<I: IntelCdclkIo>(io: &mut I, state: &mut IntelAtomicState) -> i32 {
    let ret = intel_cdclk_modeset_calc_cdclk(io, state);
    if ret != 0 {
        return ret;
    }
    let display = state.display;
    let old = state.old_cdclk;
    if intel_cdclk_need_serialize(&display, &old, &state.cdclk) {
        let ret = hook(io, "intel_atomic_serialize_global_state", &[]);
        if ret != 0 {
            return ret as i32;
        }
    } else if intel_cdclk_changed(&old.logical, &state.cdclk.logical) {
        let ret = hook(io, "intel_atomic_lock_global_state", &[]);
        if ret != 0 {
            return ret as i32;
        }
    } else {
        return 0;
    }
    let mut pipe = INVALID_PIPE;
    if state.cdclk.active_pipes.is_power_of_two()
        && intel_cdclk_can_cd2x_update(io, &display, &old.actual, &state.cdclk.actual)
    {
        pipe = state.cdclk.active_pipes.trailing_zeros() as i32;
        if hook(io, "intel_crtc_needs_modeset", &[pipe as i64]) != 0 {
            pipe = INVALID_PIPE;
        }
    }
    if intel_cdclk_can_crawl_and_squash(io, &display, &old.actual, &state.cdclk.actual) {
        io.log(
            "debug",
            "Can change cdclk via crawling and squashing\n",
            &[],
        );
    } else if intel_cdclk_can_squash(io, &display, &old.actual, &state.cdclk.actual) {
        io.log("debug", "Can change cdclk via squashing\n", &[]);
    } else if intel_cdclk_can_crawl(io, &display, &old.actual, &state.cdclk.actual) {
        io.log("debug", "Can change cdclk via crawling\n", &[]);
    } else if pipe != INVALID_PIPE {
        state.cdclk.pipe = pipe;
        hook(io, "drm_dbg_kms:cd2x_pipe", &[pipe as i64]);
    } else if intel_cdclk_clock_changed(&old.actual, &state.cdclk.actual) {
        let ret = hook(io, "intel_modeset_all_pipes_late:CDCLK change", &[]);
        if ret != 0 {
            return ret as i32;
        }
        state.cdclk.disable_pipes = true;
        io.log("debug", "Modeset required for cdclk change\n", &[]);
    }
    if intel_mdclk_cdclk_ratio(&display, &old.actual)
        != intel_mdclk_cdclk_ratio(&display, &state.cdclk.actual)
    {
        let ratio = intel_mdclk_cdclk_ratio(&display, &state.cdclk.actual);
        let ret = hook(
            io,
            "intel_dbuf_state_set_mdclk_cdclk_ratio",
            &[ratio as i64],
        );
        if ret != 0 {
            return ret as i32;
        }
    }
    io.log(
        "debug",
        "New cdclk calculated to be logical %u kHz, actual %u kHz\n",
        &[
            state.cdclk.logical.cdclk as i64,
            state.cdclk.actual.cdclk as i64,
        ],
    );
    io.log(
        "debug",
        "New voltage level calculated to be logical %u, actual %u\n",
        &[
            state.cdclk.logical.voltage_level as i64,
            state.cdclk.actual.voltage_level as i64,
        ],
    );
    0
}

// upstream: intel_cdclk.c intel_cdclk_atomic_check()
pub fn intel_cdclk_atomic_check<I: IntelCdclkIo>(io: &mut I, state: &mut IntelAtomicState) -> i32 {
    let mut need_calc = false;
    let mut ret = intel_cdclk_modeset_checks(io, state, &mut need_calc);
    if ret != 0 {
        return ret;
    }
    ret = intel_crtcs_calc_min_cdclk(io, state, &mut need_calc);
    if ret != 0 {
        return ret;
    }
    ret = hook(io, "intel_dbuf_bw_calc_min_cdclk", &[]) as i32;
    if ret != 0 {
        return ret as i32;
    }
    if state.old_cdclk.force_min_cdclk != state.cdclk.force_min_cdclk {
        ret = hook(io, "intel_atomic_lock_global_state", &[]) as i32;
        if ret != 0 {
            return ret as i32;
        }
        need_calc = true;
    }
    if need_calc {
        return intel_modeset_calc_cdclk(io, state);
    }
    0
}

// upstream: intel_cdclk.c intel_cdclk_update_hw_state()
pub fn intel_cdclk_update_hw_state<I: IntelCdclkIo>(io: &mut I, display: &mut IntelDisplay) {
    let state = &mut display.atomic_cdclk;
    state.enabled_pipes = 0;
    state.active_pipes = 0;
    for pipe in 0..I915_MAX_PIPES {
        let crtc_state_present = hook(io, "for_each_intel_crtc:present", &[pipe as i64]) != 0;
        if !crtc_state_present {
            continue;
        }
        let enabled = hook(io, "crtc.hw.enable", &[pipe as i64]) != 0;
        let active = hook(io, "crtc.hw.active", &[pipe as i64]) != 0;
        if enabled {
            state.enabled_pipes |= 1 << pipe;
        }
        if active {
            state.active_pipes |= 1 << pipe;
        }
        state.min_cdclk[pipe] = hook(io, "crtc.min_cdclk", &[pipe as i64]) as i32;
        state.min_voltage_level[pipe] = hook(io, "crtc.min_voltage_level", &[pipe as i64]) as u8;
    }
    state.dbuf_bw_min_cdclk = hook(io, "intel_dbuf_bw_min_cdclk", &[]) as i32;
}

#[cfg(test)]
mod source_field_tests {
    use super::*;

    #[test]
    fn cnp_rawclk_macro_fields_keep_integer_fraction_and_icp_numerator() {
        assert_eq!(cnp_rawclk_fields(24_000, 0, false), 24 << 16);
        assert_eq!(
            cnp_rawclk_fields(19_000, 200, false),
            (19 << 16) | (4 << 26)
        );
        assert_eq!(
            cnp_rawclk_fields(19_000, 200, true),
            (19 << 16) | (4 << 26) | (1 << 11)
        );
    }
}

// upstream: intel_cdclk.c intel_cdclk_crtc_disable_noatomic()
pub fn intel_cdclk_crtc_disable_noatomic<I: IntelCdclkIo>(
    io: &mut I,
    _crtc: &IntelCrtc,
    display: &mut IntelDisplay,
) {
    intel_cdclk_update_hw_state(io, display);
}

// upstream: intel_cdclk.c intel_compute_max_dotclk()
pub fn intel_compute_max_dotclk<I: IntelCdclkIo>(io: &mut I, display: &IntelDisplay) -> i32 {
    let double_wide = hook(io, "HAS_DOUBLE_WIDE", &[]) != 0;
    let ppc = intel_cdclk_ppc(io, display, double_wide);
    let guardband = intel_cdclk_guardband(display);
    ppc * display.cdclk.max_cdclk_freq * guardband / 100
}

// upstream: intel_cdclk.c intel_update_max_cdclk()
pub fn intel_update_max_cdclk<I: IntelCdclkIo>(io: &mut I, display: &mut IntelDisplay) {
    if display.display_ver >= 35 {
        display.cdclk.max_cdclk_freq = 787200;
    } else if display.display_ver_full >= 3002 {
        display.cdclk.max_cdclk_freq = 480000;
    } else if display.display_ver >= 30 {
        display.cdclk.max_cdclk_freq = 691200;
    } else if display.platform.jasperlake || display.platform.elkhartlake {
        display.cdclk.max_cdclk_freq = if display.cdclk.hw.refclk == 24000 {
            552000
        } else {
            556800
        };
    } else if display.display_ver >= 11 {
        display.cdclk.max_cdclk_freq = if display.cdclk.hw.refclk == 24000 {
            648000
        } else {
            652800
        };
    } else if display.platform.geminilake {
        display.cdclk.max_cdclk_freq = 316800;
    } else if display.platform.broxton {
        display.cdclk.max_cdclk_freq = 624000;
    } else if display.display_ver == 9 {
        let limit = rd(io, "SKL_DFSM") & c(io, "SKL_DFSM_CDCLK_LIMIT_MASK");
        let vco = display.cdclk.skl_preferred_vco_freq;
        if vco != 8100000 && vco != 8640000 {
            hook(io, "drm_WARN:preferred_vco", &[vco as i64]);
        }
        let max_cdclk = if limit == c(io, "SKL_DFSM_CDCLK_LIMIT_675") {
            617143
        } else if limit == c(io, "SKL_DFSM_CDCLK_LIMIT_540") {
            540000
        } else if limit == c(io, "SKL_DFSM_CDCLK_LIMIT_450") {
            432000
        } else {
            308571
        };
        display.cdclk.max_cdclk_freq = skl_calc_cdclk(max_cdclk, vco);
    } else if display.platform.broadwell {
        if rd(io, "FUSE_STRAP") & c(io, "HSW_CDCLK_LIMIT") != 0 {
            display.cdclk.max_cdclk_freq = 450000;
        } else if display.platform.broadwell_ulx {
            display.cdclk.max_cdclk_freq = 450000;
        } else if display.platform.broadwell_ult {
            display.cdclk.max_cdclk_freq = 540000;
        } else {
            display.cdclk.max_cdclk_freq = 675000;
        }
    } else if display.platform.cherryview {
        display.cdclk.max_cdclk_freq = 320000;
    } else if display.platform.valleyview {
        display.cdclk.max_cdclk_freq = 400000;
    } else {
        display.cdclk.max_cdclk_freq = display.cdclk.hw.cdclk;
    }
    display.cdclk.max_dotclk_freq = intel_compute_max_dotclk(io, display);
    io.log(
        "debug",
        "Max CD clock rate: %d kHz\n",
        &[display.cdclk.max_cdclk_freq as i64],
    );
    io.log(
        "debug",
        "Max dotclock rate: %d kHz\n",
        &[display.cdclk.max_dotclk_freq as i64],
    );
}

// upstream: intel_cdclk.c intel_update_cdclk()
pub fn intel_update_cdclk<I: IntelCdclkIo>(io: &mut I, display: &mut IntelDisplay) {
    let mut hw = display.cdclk.hw;
    intel_cdclk_get_cdclk(io, display, &mut hw);
    display.cdclk.hw = hw;
    if display.platform.valleyview || display.platform.cherryview {
        wr(
            io,
            "GMBUSFREQ_VLV",
            div_round_up(display.cdclk.hw.cdclk as i64, 1000) as u32,
        );
    }
}

// upstream: intel_cdclk.c dg1_rawclk()
pub fn dg1_rawclk<I: IntelCdclkIo>(io: &mut I, _display: &IntelDisplay) -> i32 {
    wr(
        io,
        "PCH_RAWCLK_FREQ",
        c(io, "CNP_RAWCLK_DEN(4)") | c(io, "CNP_RAWCLK_DIV(37)") | c(io, "ICP_RAWCLK_NUM(2)"),
    );
    38400
}

// upstream: intel_cdclk.c cnp_rawclk()
pub fn cnp_rawclk<I: IntelCdclkIo>(io: &mut I, display: &IntelDisplay) -> i32 {
    let (divider, fraction) = if rd(io, "SFUSE_STRAP") & c(io, "SFUSE_STRAP_RAW_FREQUENCY") != 0 {
        (24000, 0)
    } else {
        (19000, 200)
    };
    let rawclk = cnp_rawclk_fields(divider, fraction, display.pch_type >= c(io, "PCH_ICP"));
    wr(io, "PCH_RAWCLK_FREQ", rawclk);
    divider + fraction
}

fn cnp_rawclk_fields(divider: i32, fraction: i32, has_icp: bool) -> u32 {
    // intel_display_regs.h defines CNP_RAWCLK_DIV(x) as x << 16.
    let mut rawclk = (divider as u32 / 1000) << 16;
    if fraction != 0 {
        let numerator = 1;
        // CNP_RAWCLK_DEN(DIV_ROUND_CLOSEST(numerator * 1000, fraction) - 1).
        let denominator = round_closest(numerator * 1000, fraction as i64) - 1;
        rawclk |= (denominator as u32) << 26;
        if has_icp {
            // ICP_RAWCLK_NUM(numerator) is numerator << 11.
            rawclk |= (numerator as u32) << 11;
        }
    }
    rawclk
}

// upstream: intel_cdclk.c pch_rawclk()
pub fn pch_rawclk<I: IntelCdclkIo>(io: &mut I) -> i32 {
    ((rd(io, "PCH_RAWCLK_FREQ") & c(io, "RAWCLK_FREQ_MASK")) * 1000) as i32
}

// upstream: intel_cdclk.c i9xx_hrawclk()
pub fn i9xx_hrawclk<I: IntelCdclkIo>(io: &mut I) -> i32 {
    round_closest(hook(io, "intel_fsb_freq", &[]) as i64, 4) as i32
}

// upstream: intel_cdclk.c intel_read_rawclk()
pub fn intel_read_rawclk<I: IntelCdclkIo>(io: &mut I, display: &mut IntelDisplay) -> u32 {
    if display.pch_type >= c(io, "PCH_MTL") {
        38400
    } else if display.pch_type >= c(io, "PCH_DG1") {
        dg1_rawclk(io, display) as u32
    } else if display.pch_type >= c(io, "PCH_CNP") {
        cnp_rawclk(io, display) as u32
    } else if hook(io, "HAS_PCH_SPLIT", &[]) != 0 {
        pch_rawclk(io) as u32
    } else if display.platform.valleyview || display.platform.cherryview {
        hook(io, "vlv_clock_get_hrawclk", &[]) as u32
    } else if display.display_ver >= 3 {
        i9xx_hrawclk(io) as u32
    } else {
        0
    }
}

// upstream: intel_cdclk.c i915_cdclk_info_show()
pub fn i915_cdclk_info_show<I: IntelCdclkIo>(io: &mut I, display: &IntelDisplay) -> i32 {
    hook(
        io,
        "seq_printf:Current CD clock frequency",
        &[display.cdclk.hw.cdclk as i64],
    );
    hook(
        io,
        "seq_printf:Max CD clock frequency",
        &[display.cdclk.max_cdclk_freq as i64],
    );
    hook(
        io,
        "seq_printf:Max pixel clock frequency",
        &[display.cdclk.max_dotclk_freq as i64],
    );
    0
}

// upstream: intel_cdclk.c intel_cdclk_debugfs_register()
pub fn intel_cdclk_debugfs_register<I: IntelCdclkIo>(io: &mut I, display: &IntelDisplay) {
    hook(
        io,
        "debugfs_create_file:i915_cdclk_info",
        &[display.cdclk.hw.cdclk as i64],
    );
}

pub const CDCLK_FAMILY_I830: u32 = 1;
pub const CDCLK_FAMILY_I845G: u32 = 2;
pub const CDCLK_FAMILY_I85X: u32 = 3;
pub const CDCLK_FAMILY_I865G: u32 = 4;
pub const CDCLK_FAMILY_I915G: u32 = 5;
pub const CDCLK_FAMILY_I915GM: u32 = 6;
pub const CDCLK_FAMILY_I945GM: u32 = 7;
pub const CDCLK_FAMILY_G33: u32 = 8;
pub const CDCLK_FAMILY_PNV: u32 = 9;
pub const CDCLK_FAMILY_I965GM: u32 = 10;
pub const CDCLK_FAMILY_GM45: u32 = 11;
pub const CDCLK_FAMILY_ILK: u32 = 12;
pub const CDCLK_FAMILY_FIXED_400: u32 = 13;
pub const CDCLK_FAMILY_HSW: u32 = 14;
pub const CDCLK_FAMILY_VLV: u32 = 15;
pub const CDCLK_FAMILY_CHV: u32 = 16;
pub const CDCLK_FAMILY_BDW: u32 = 17;
pub const CDCLK_FAMILY_SKL: u32 = 18;
pub const CDCLK_FAMILY_BXT: u32 = 19;
pub const CDCLK_FAMILY_ICL: u32 = 20;
pub const CDCLK_FAMILY_EHL: u32 = 21;
pub const CDCLK_FAMILY_TGL: u32 = 22;
pub const CDCLK_FAMILY_RPLU: u32 = 23;
pub const CDCLK_FAMILY_XE3LPD: u32 = 24;

// upstream: intel_cdclk.c intel_init_cdclk_hooks()
pub fn intel_init_cdclk_hooks<I: IntelCdclkIo>(io: &mut I, display: &mut IntelDisplay) {
    let (family, table) = if display.display_ver >= 35 {
        (CDCLK_FAMILY_XE3LPD, 12)
    } else if display.display_ver >= 30 {
        (CDCLK_FAMILY_XE3LPD, 11)
    } else if display.display_ver >= 20 {
        (CDCLK_FAMILY_RPLU, 9)
    } else if display.display_ver_full >= 1401 {
        (CDCLK_FAMILY_RPLU, 10)
    } else if display.display_ver >= 14 {
        (CDCLK_FAMILY_RPLU, 8)
    } else if display.platform.dg2 {
        (CDCLK_FAMILY_TGL, 7)
    } else if display.platform.alderlake_p {
        if hook(io, "intel_display_wa:INTEL_DISPLAY_WA_22011320316", &[]) != 0 {
            (CDCLK_FAMILY_TGL, 4)
        } else if display.platform.alderlake_p_raptorlake_u {
            (CDCLK_FAMILY_RPLU, 6)
        } else {
            (CDCLK_FAMILY_TGL, 5)
        }
    } else if display.platform.rocketlake {
        (CDCLK_FAMILY_TGL, 3)
    } else if display.display_ver >= 12 {
        (CDCLK_FAMILY_TGL, 2)
    } else if display.platform.jasperlake || display.platform.elkhartlake {
        (CDCLK_FAMILY_EHL, 2)
    } else if display.display_ver >= 11 {
        (CDCLK_FAMILY_ICL, 2)
    } else if display.platform.geminilake || display.platform.broxton {
        (
            CDCLK_FAMILY_BXT,
            if display.platform.geminilake { 1 } else { 0 },
        )
    } else if display.display_ver == 9 {
        (CDCLK_FAMILY_SKL, u32::MAX)
    } else if display.platform.broadwell {
        (CDCLK_FAMILY_BDW, u32::MAX)
    } else if display.platform.haswell {
        (CDCLK_FAMILY_HSW, u32::MAX)
    } else if display.platform.cherryview {
        (CDCLK_FAMILY_CHV, u32::MAX)
    } else if display.platform.valleyview {
        (CDCLK_FAMILY_VLV, u32::MAX)
    } else if display.platform.sandybridge || display.platform.ivybridge {
        (CDCLK_FAMILY_FIXED_400, u32::MAX)
    } else if display.platform.ironlake {
        (CDCLK_FAMILY_ILK, u32::MAX)
    } else if display.platform.gm45 {
        (CDCLK_FAMILY_GM45, u32::MAX)
    } else if display.platform.g45 {
        (CDCLK_FAMILY_G33, u32::MAX)
    } else if display.platform.i965gm {
        (CDCLK_FAMILY_I965GM, u32::MAX)
    } else if display.platform.i965g {
        (CDCLK_FAMILY_FIXED_400, u32::MAX)
    } else if display.platform.pineview {
        (CDCLK_FAMILY_PNV, u32::MAX)
    } else if display.platform.g33 {
        (CDCLK_FAMILY_G33, u32::MAX)
    } else if display.platform.i945gm {
        (CDCLK_FAMILY_I945GM, u32::MAX)
    } else if display.platform.i945g {
        (CDCLK_FAMILY_FIXED_400, u32::MAX)
    } else if display.platform.i915gm {
        (CDCLK_FAMILY_I915GM, u32::MAX)
    } else if display.platform.i915g {
        (CDCLK_FAMILY_I915G, u32::MAX)
    } else if display.platform.i865g {
        (CDCLK_FAMILY_I865G, u32::MAX)
    } else if display.platform.i85x {
        (CDCLK_FAMILY_I85X, u32::MAX)
    } else if display.platform.i845g {
        (CDCLK_FAMILY_I845G, u32::MAX)
    } else if display.platform.i830 {
        (CDCLK_FAMILY_I830, u32::MAX)
    } else {
        hook(io, "drm_WARN:unknown_platform_assume_i830", &[]);
        (CDCLK_FAMILY_I830, u32::MAX)
    };
    display.cdclk.funcs = family;
    if table != u32::MAX {
        display.cdclk.table = table;
    }
}

// upstream: intel_cdclk.c intel_cdclk_logical()
pub fn intel_cdclk_logical(state: &IntelCdclkAtomicState) -> i32 {
    state.logical.cdclk
}

// upstream: intel_cdclk.c intel_cdclk_actual()
pub fn intel_cdclk_actual(state: &IntelCdclkAtomicState) -> i32 {
    state.actual.cdclk
}

// upstream: intel_cdclk.c intel_cdclk_actual_voltage_level()
pub fn intel_cdclk_actual_voltage_level(state: &IntelCdclkAtomicState) -> i32 {
    state.actual.voltage_level as i32
}

// upstream: intel_cdclk.c intel_cdclk_min_cdclk()
pub fn intel_cdclk_min_cdclk(state: &IntelCdclkAtomicState, pipe: usize) -> i32 {
    state.min_cdclk[pipe]
}

// upstream: intel_cdclk.c intel_cdclk_pmdemand_needs_update()
pub fn intel_cdclk_pmdemand_needs_update(state: &IntelAtomicState) -> bool {
    state.cdclk.actual.cdclk != state.old_cdclk.actual.cdclk
        || state.cdclk.actual.voltage_level != state.old_cdclk.actual.voltage_level
}

// upstream: intel_cdclk.c intel_cdclk_force_min_cdclk()
pub fn intel_cdclk_force_min_cdclk(state: &mut IntelCdclkAtomicState, force_min_cdclk: i32) {
    state.force_min_cdclk = force_min_cdclk;
}

// upstream: intel_cdclk.c intel_cdclk_read_hw()
pub fn intel_cdclk_read_hw<I: IntelCdclkIo>(io: &mut I, display: &mut IntelDisplay) {
    intel_update_cdclk(io, display);
    intel_cdclk_dump_config(io, &display.cdclk.hw, "Current CDCLK");
    display.atomic_cdclk.actual = display.cdclk.hw;
    display.atomic_cdclk.logical = display.cdclk.hw;
}

// upstream: intel_cdclk.c calc_cdclk()
pub fn calc_cdclk<I: IntelCdclkIo>(
    io: &mut I,
    display: &IntelDisplay,
    crtc_state: &IntelCrtcState,
    min_cdclk: i32,
) -> i32 {
    if display.display_ver >= 10 || display.platform.broxton {
        bxt_calc_cdclk(io, display, min_cdclk)
    } else if display.display_ver == 9 {
        let vco = if display.cdclk.skl_preferred_vco_freq == 0 {
            8100000
        } else {
            display.cdclk.skl_preferred_vco_freq
        };
        skl_calc_cdclk(min_cdclk, vco)
    } else if display.platform.broadwell {
        bdw_calc_cdclk(min_cdclk)
    } else if display.platform.cherryview || display.platform.valleyview {
        vlv_calc_cdclk(io, display, min_cdclk)
    } else {
        let _ = crtc_state;
        display.cdclk.max_cdclk_freq
    }
}

// upstream: intel_cdclk.c _intel_cdclk_prefill_adj()
pub fn _intel_cdclk_prefill_adj<I: IntelCdclkIo>(
    io: &mut I,
    display: &IntelDisplay,
    crtc_state: &IntelCrtcState,
    clock: i32,
    min_cdclk: i32,
) -> u32 {
    let ppc = intel_cdclk_ppc(io, display, crtc_state.double_wide);
    let cdclk = calc_cdclk(io, display, crtc_state, min_cdclk);
    min(
        0x10000,
        div_round_up((clock as i64) << 16, (ppc * cdclk) as i64) as u32,
    )
}

// upstream: intel_cdclk.c intel_cdclk_prefill_adjustment()
pub fn intel_cdclk_prefill_adjustment<I: IntelCdclkIo>(
    io: &mut I,
    display: &IntelDisplay,
    crtc_state: &IntelCrtcState,
) -> u32 {
    intel_cdclk_prefill_adjustment_worst(io, display, crtc_state)
}

// upstream: intel_cdclk.c intel_cdclk_prefill_adjustment_worst()
pub fn intel_cdclk_prefill_adjustment_worst<I: IntelCdclkIo>(
    io: &mut I,
    display: &IntelDisplay,
    crtc_state: &IntelCrtcState,
) -> u32 {
    let clock = crtc_state.mode_clock;
    let min_cdclk = _intel_pixel_rate_to_cdclk(io, display, crtc_state, clock);
    _intel_cdclk_prefill_adj(io, display, crtc_state, clock, min_cdclk)
}

// upstream: intel_cdclk.c intel_cdclk_min_cdclk_for_prefill()
pub fn intel_cdclk_min_cdclk_for_prefill<I: IntelCdclkIo>(
    io: &I,
    display: &IntelDisplay,
    crtc_state: &IntelCrtcState,
    prefill_lines_unadjusted: u32,
    prefill_lines_available: u32,
) -> i32 {
    let ppc = intel_cdclk_ppc(io, display, crtc_state.double_wide);
    div_round_up(
        (crtc_state.mode_clock as i64) * prefill_lines_unadjusted as i64,
        (ppc as u32 * prefill_lines_available) as i64,
    ) as i32
}
