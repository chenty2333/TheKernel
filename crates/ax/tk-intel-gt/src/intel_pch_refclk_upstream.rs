// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// Source: drivers/gpu/drm/i915/display/intel_pch_refclk.c (Linux v7.2.3),
// the boot-time PCH reference-clock initialisation (`intel_init_pch_refclk()`)
// and the helpers it reaches. `drm_dbg_kms()` diagnostics are not carried over.
#![allow(non_snake_case, dead_code, unsafe_code)]

use tk_drm_upstream::drm_encoder_types::DrmEncoder;
use tk_linux_kapi::primitives::udelay;

use crate::{
    intel_de_upstream::{
        intel_de_posting_read, intel_de_read, intel_de_rmw, intel_de_wait_for_clear_us,
        intel_de_wait_for_set_us, intel_de_write,
    },
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_limits_types_upstream::PORT_A,
    intel_display_regs_upstream::*,
    intel_display_types_upstream::{
        INTEL_OUTPUT_ANALOG, INTEL_OUTPUT_EDP, INTEL_OUTPUT_LVDS, IntelEncoder,
    },
    intel_dpll_mgr_types_upstream::{DPLL_ID_SPLL, DPLL_ID_WRPLL1, DPLL_ID_WRPLL2},
    intel_panel_upstream::intel_panel_use_ssc,
    intel_pch_types_upstream::{PCH_CPT, PCH_IBX, PCH_LPT_H, PCH_LPT_LP},
    intel_sbi_upstream::{
        SBI_ICLK, SBI_MPHY, intel_sbi_lock, intel_sbi_read, intel_sbi_unlock, intel_sbi_write,
    },
};

// intel_sbi_regs.h
const SBI_SSCDIVINTPHASE: u16 = 0x0200;
const SBI_SSCDITHPHASE: u16 = 0x0204;
const SBI_SSCCTL: u16 = 0x020c;
const SBI_SSCCTL_PATHALT: u32 = 1 << 3;
const SBI_SSCCTL_DISABLE: u32 = 1 << 0;
const SBI_DBUFF0: u16 = 0x2a00;
const SBI_GEN0: u16 = 0x1f00;
const SBI_GEN0_CFG_BUFFENABLE_DISABLE: u32 = 1 << 0;

/// `INTEL_PCH_TYPE(display)`.
#[inline]
unsafe fn pch_type(display: *const IntelDisplay) -> i32 {
    unsafe { (*display).pch_type }
}

/// `display->platform.<bit>` on the platform bitmap (declaration order of the
/// platform list): broadwell = 24, haswell_ult = 22.
#[inline]
unsafe fn platform_has(display: *const IntelDisplay, bit: u32) -> bool {
    unsafe { (*display).platform._anon_0_0.bitmap[(bit / 64) as usize] & (1u64 << (bit % 64)) != 0 }
}

const PLATFORM_HASWELL_ULT: u32 = 22;
const PLATFORM_BROADWELL: u32 = 24;

/// `VBT display_clock_mode` (bit 4 of the VBT bitfield word).
#[inline]
unsafe fn vbt_display_clock_mode(display: *const IntelDisplay) -> bool {
    unsafe { (*display).vbt._bitfields_2[0] & (1 << 4) != 0 }
}

/// `BEND_IDX(steps)` (intel_pch_refclk.c).
#[inline]
const fn BEND_IDX(steps: i32) -> i32 {
    (50 + steps) / 5
}

/// `sscdivintphase[]`, indexed by `BEND_IDX(steps)` for `steps` in -50..=50.
const SSCDIVINTPHASE: [u16; 21] = [
    // BEND_IDX(-50)
    0x0525, // BEND_IDX(-45)
    0x0425, // BEND_IDX(-40)
    0x0425, // BEND_IDX(-35)
    0x0325, // BEND_IDX(-30)
    0x0325, // BEND_IDX(-25)
    0x0225, // BEND_IDX(-20)
    0x0225, // BEND_IDX(-15)
    0x0125, // BEND_IDX(-10)
    0x0125, // BEND_IDX( -5)
    0x0025, // BEND_IDX(  0)
    0x0025, // BEND_IDX(  5)
    0x3F23, // BEND_IDX( 10)
    0x3F23, // BEND_IDX( 15)
    0x3E23, // BEND_IDX( 20)
    0x3E23, // BEND_IDX( 25)
    0x3D23, // BEND_IDX( 30)
    0x3D23, // BEND_IDX( 35)
    0x3C23, // BEND_IDX( 40)
    0x3C23, // BEND_IDX( 45)
    0x3B23, // BEND_IDX( 50)
    0x3B23,
];

/// `lpt_fdi_reset_mphy()`.
// upstream: intel_pch_refclk.c lpt_fdi_reset_mphy()
unsafe fn lpt_fdi_reset_mphy(display: *mut IntelDisplay) {
    unsafe {
        intel_de_rmw(display, SOUTH_CHICKEN2, 0, FDI_MPHY_IOSFSB_RESET_CTL);
        if intel_de_wait_for_set_us(display, SOUTH_CHICKEN2, FDI_MPHY_IOSFSB_RESET_STATUS, 100) != 0
        {
            axlog::error!("i915 display: FDI mPHY reset assert timeout");
        }
        intel_de_rmw(display, SOUTH_CHICKEN2, FDI_MPHY_IOSFSB_RESET_CTL, 0);
        if intel_de_wait_for_clear_us(display, SOUTH_CHICKEN2, FDI_MPHY_IOSFSB_RESET_STATUS, 100)
            != 0
        {
            axlog::error!("i915 display: FDI mPHY reset de-assert timeout");
        }
    }
}

/// Read-modify-write one mPHY SBI register.
#[inline]
unsafe fn sbi_mphy_rmw(display: *mut IntelDisplay, reg: u16, clear: u32, set: u32) {
    unsafe {
        let mut tmp = intel_sbi_read(display, reg, SBI_MPHY);
        tmp &= !clear;
        tmp |= set;
        intel_sbi_write(display, reg, tmp, SBI_MPHY);
    }
}

/// `lpt_fdi_program_mphy()`.
// upstream: intel_pch_refclk.c lpt_fdi_program_mphy()
unsafe fn lpt_fdi_program_mphy(display: *mut IntelDisplay) {
    unsafe {
        lpt_fdi_reset_mphy(display);

        sbi_mphy_rmw(display, 0x8008, 0xFF << 24, 0x12 << 24);
        sbi_mphy_rmw(display, 0x2008, 0, 1 << 11);
        sbi_mphy_rmw(display, 0x2108, 0, 1 << 11);
        sbi_mphy_rmw(display, 0x206C, 0, (1 << 24) | (1 << 21) | (1 << 18));
        sbi_mphy_rmw(display, 0x216C, 0, (1 << 24) | (1 << 21) | (1 << 18));
        sbi_mphy_rmw(display, 0x2080, 7 << 13, 5 << 13);
        sbi_mphy_rmw(display, 0x2180, 7 << 13, 5 << 13);
        sbi_mphy_rmw(display, 0x208C, 0xFF, 0x1C);
        sbi_mphy_rmw(display, 0x218C, 0xFF, 0x1C);
        sbi_mphy_rmw(display, 0x2098, 0xFF << 16, 0x1C << 16);
        sbi_mphy_rmw(display, 0x2198, 0xFF << 16, 0x1C << 16);
        sbi_mphy_rmw(display, 0x20C4, 0, 1 << 27);
        sbi_mphy_rmw(display, 0x21C4, 0, 1 << 27);
        sbi_mphy_rmw(display, 0x20EC, 0xF << 28, 4 << 28);
        sbi_mphy_rmw(display, 0x21EC, 0xF << 28, 4 << 28);
    }
}

/// `lpt_disable_clkout_dp()`.
// upstream: intel_pch_refclk.c lpt_disable_clkout_dp()
pub unsafe fn lpt_disable_clkout_dp(display: *mut IntelDisplay) {
    unsafe {
        intel_sbi_lock(display);

        let reg = if pch_type(display) == PCH_LPT_LP {
            SBI_GEN0
        } else {
            SBI_DBUFF0
        };
        let mut tmp = intel_sbi_read(display, reg, SBI_ICLK);
        tmp &= !SBI_GEN0_CFG_BUFFENABLE_DISABLE;
        intel_sbi_write(display, reg, tmp, SBI_ICLK);

        tmp = intel_sbi_read(display, SBI_SSCCTL, SBI_ICLK);
        if tmp & SBI_SSCCTL_DISABLE == 0 {
            if tmp & SBI_SSCCTL_PATHALT == 0 {
                tmp |= SBI_SSCCTL_PATHALT;
                intel_sbi_write(display, SBI_SSCCTL, tmp, SBI_ICLK);
                udelay(32);
            }
            tmp |= SBI_SSCCTL_DISABLE;
            intel_sbi_write(display, SBI_SSCCTL, tmp, SBI_ICLK);
        }

        intel_sbi_unlock(display);
    }
}

/// `lpt_enable_clkout_dp()`.
// upstream: intel_pch_refclk.c lpt_enable_clkout_dp()
unsafe fn lpt_enable_clkout_dp(
    display: *mut IntelDisplay,
    mut with_spread: bool,
    mut with_fdi: bool,
) {
    unsafe {
        if with_fdi && !with_spread {
            // drm_WARN(..., "FDI requires downspread")
            with_spread = true;
        }
        if pch_type(display) == PCH_LPT_LP && with_fdi {
            // drm_WARN(..., "LP PCH doesn't have FDI")
            with_fdi = false;
        }

        intel_sbi_lock(display);

        let mut tmp = intel_sbi_read(display, SBI_SSCCTL, SBI_ICLK);
        tmp &= !SBI_SSCCTL_DISABLE;
        tmp |= SBI_SSCCTL_PATHALT;
        intel_sbi_write(display, SBI_SSCCTL, tmp, SBI_ICLK);
        udelay(24);

        if with_spread {
            tmp = intel_sbi_read(display, SBI_SSCCTL, SBI_ICLK);
            tmp &= !SBI_SSCCTL_PATHALT;
            intel_sbi_write(display, SBI_SSCCTL, tmp, SBI_ICLK);

            if with_fdi {
                lpt_fdi_program_mphy(display);
            }
        }

        let reg = if pch_type(display) == PCH_LPT_LP {
            SBI_GEN0
        } else {
            SBI_DBUFF0
        };
        tmp = intel_sbi_read(display, reg, SBI_ICLK);
        tmp |= SBI_GEN0_CFG_BUFFENABLE_DISABLE;
        intel_sbi_write(display, reg, tmp, SBI_ICLK);

        intel_sbi_unlock(display);
    }
}

/// `lpt_bend_clkout_dp()`.
// upstream: intel_pch_refclk.c lpt_bend_clkout_dp()
unsafe fn lpt_bend_clkout_dp(display: *mut IntelDisplay, steps: i32) {
    unsafe {
        let idx = BEND_IDX(steps);

        // drm_WARN_ON(display->drm, steps % 5 != 0) and the table bounds.
        if steps % 5 != 0 || idx < 0 || idx as usize >= SSCDIVINTPHASE.len() {
            return;
        }

        intel_sbi_lock(display);

        let tmp = if steps % 10 != 0 {
            0xAAAA_AAAB
        } else {
            0x0000_0000
        };
        intel_sbi_write(display, SBI_SSCDITHPHASE, tmp, SBI_ICLK);

        let mut tmp = intel_sbi_read(display, SBI_SSCDIVINTPHASE, SBI_ICLK);
        tmp &= 0xffff_0000;
        tmp |= SSCDIVINTPHASE[idx as usize] as u32;
        intel_sbi_write(display, SBI_SSCDIVINTPHASE, tmp, SBI_ICLK);

        intel_sbi_unlock(display);
    }
}

/// `spll_uses_pch_ssc()`.
// upstream: intel_pch_refclk.c spll_uses_pch_ssc()
unsafe fn spll_uses_pch_ssc(display: *mut IntelDisplay) -> bool {
    unsafe {
        let fuse_strap = intel_de_read(display, FUSE_STRAP);
        let ctl = intel_de_read(display, SPLL_CTL);

        if ctl & SPLL_PLL_ENABLE == 0 {
            return false;
        }
        if ctl & SPLL_REF_MASK == SPLL_REF_MUXED_SSC && fuse_strap & HSW_CPU_SSC_ENABLE == 0 {
            return true;
        }
        if platform_has(display, PLATFORM_BROADWELL) && ctl & SPLL_REF_MASK == SPLL_REF_PCH_SSC_BDW
        {
            return true;
        }
        false
    }
}

/// `wrpll_uses_pch_ssc()`.
// upstream: intel_pch_refclk.c wrpll_uses_pch_ssc()
unsafe fn wrpll_uses_pch_ssc(display: *mut IntelDisplay, id: u32) -> bool {
    unsafe {
        let fuse_strap = intel_de_read(display, FUSE_STRAP);
        let ctl = intel_de_read(display, WRPLL_CTL(id));

        if ctl & WRPLL_PLL_ENABLE == 0 {
            return false;
        }
        if ctl & WRPLL_REF_MASK == WRPLL_REF_PCH_SSC {
            return true;
        }
        if (platform_has(display, PLATFORM_BROADWELL)
            || platform_has(display, PLATFORM_HASWELL_ULT))
            && ctl & WRPLL_REF_MASK == WRPLL_REF_MUXED_SSC_BDW
            && fuse_strap & HSW_CPU_SSC_ENABLE == 0
        {
            return true;
        }
        false
    }
}

/// `for_each_intel_encoder()` over `display->drm`.
unsafe fn any_encoder_of(display: *mut IntelDisplay, mut f: impl FnMut(*mut IntelEncoder)) {
    unsafe {
        let head = core::ptr::addr_of_mut!((*(*display).drm).mode_config.encoder_list);
        let mut node = (*head).next;
        let head_offset = core::mem::offset_of!(DrmEncoder, head);
        while !node.is_null() && node != head {
            let drm_encoder = node.cast::<u8>().sub(head_offset).cast::<DrmEncoder>();
            f(drm_encoder.cast::<IntelEncoder>());
            node = (*node).next;
        }
    }
}

/// `lpt_init_pch_refclk()`.
// upstream: intel_pch_refclk.c lpt_init_pch_refclk()
unsafe fn lpt_init_pch_refclk(display: *mut IntelDisplay) {
    unsafe {
        let mut has_fdi = false;
        any_encoder_of(display, |encoder| {
            if (*encoder).r#type == INTEL_OUTPUT_ANALOG {
                has_fdi = true;
            }
        });

        // The BIOS may have decided to use the PCH SSC reference: leave it
        // enabled while any active PLL relies on it.
        (*display).dpll.pch_ssc_use = 0;

        if spll_uses_pch_ssc(display) {
            (*display).dpll.pch_ssc_use |= 1 << DPLL_ID_SPLL;
        }
        if wrpll_uses_pch_ssc(display, DPLL_ID_WRPLL1 as u32) {
            (*display).dpll.pch_ssc_use |= 1 << DPLL_ID_WRPLL1;
        }
        if wrpll_uses_pch_ssc(display, DPLL_ID_WRPLL2 as u32) {
            (*display).dpll.pch_ssc_use |= 1 << DPLL_ID_WRPLL2;
        }

        if (*display).dpll.pch_ssc_use != 0 {
            return;
        }

        if has_fdi {
            lpt_bend_clkout_dp(display, 0);
            lpt_enable_clkout_dp(display, true, true);
        } else {
            lpt_disable_clkout_dp(display);
        }
    }
}

/// `ilk_init_pch_refclk()`.
// upstream: intel_pch_refclk.c ilk_init_pch_refclk()
unsafe fn ilk_init_pch_refclk(display: *mut IntelDisplay) {
    unsafe {
        let mut has_lvds = false;
        let mut has_cpu_edp = false;
        let mut has_panel = false;
        let has_ck505: bool;
        let can_ssc: bool;
        let mut using_ssc_source = false;

        any_encoder_of(display, |encoder| match (*encoder).r#type {
            INTEL_OUTPUT_LVDS => {
                has_panel = true;
                has_lvds = true;
            }
            INTEL_OUTPUT_EDP => {
                has_panel = true;
                if (*encoder).port == PORT_A {
                    has_cpu_edp = true;
                }
            }
            _ => {}
        });
        let _ = has_lvds;

        if pch_type(display) == PCH_IBX {
            has_ck505 = vbt_display_clock_mode(display);
            can_ssc = has_ck505;
        } else {
            has_ck505 = false;
            can_ssc = true;
        }

        let num_dpll = (*display).dpll.num_dpll.max(0) as usize;
        for i in 0..num_dpll {
            let pll = core::ptr::addr_of_mut!((*display).dpll.dplls[i]);
            let id = (*(*pll).info).id;
            let temp = intel_de_read(display, PCH_DPLL(id as u32));
            if temp & DPLL_VCO_ENABLE == 0 {
                continue;
            }
            if temp & PLL_REF_INPUT_MASK == PLLB_REF_INPUT_SPREADSPECTRUMIN {
                using_ssc_source = true;
                break;
            }
        }

        let val_initial = intel_de_read(display, PCH_DREF_CONTROL);
        let mut val = val_initial;
        let mut final_ = val;
        final_ &= !DREF_NONSPREAD_SOURCE_MASK;
        if has_ck505 {
            final_ |= DREF_NONSPREAD_CK505_ENABLE;
        } else {
            final_ |= DREF_NONSPREAD_SOURCE_ENABLE;
        }
        final_ &= !DREF_SSC_SOURCE_MASK;
        final_ &= !DREF_CPU_SOURCE_OUTPUT_MASK;
        final_ &= !DREF_SSC1_ENABLE;

        if has_panel {
            final_ |= DREF_SSC_SOURCE_ENABLE;
            if intel_panel_use_ssc(display) && can_ssc {
                final_ |= DREF_SSC1_ENABLE;
            }
            if has_cpu_edp {
                if intel_panel_use_ssc(display) && can_ssc {
                    final_ |= DREF_CPU_SOURCE_OUTPUT_DOWNSPREAD;
                } else {
                    final_ |= DREF_CPU_SOURCE_OUTPUT_NONSPREAD;
                }
            } else {
                final_ |= DREF_CPU_SOURCE_OUTPUT_DISABLE;
            }
        } else if using_ssc_source {
            final_ |= DREF_SSC_SOURCE_ENABLE;
            final_ |= DREF_SSC1_ENABLE;
        }

        if final_ == val_initial {
            return;
        }

        val &= !DREF_NONSPREAD_SOURCE_MASK;
        if has_ck505 {
            val |= DREF_NONSPREAD_CK505_ENABLE;
        } else {
            val |= DREF_NONSPREAD_SOURCE_ENABLE;
        }

        if has_panel {
            val &= !DREF_SSC_SOURCE_MASK;
            val |= DREF_SSC_SOURCE_ENABLE;

            if intel_panel_use_ssc(display) && can_ssc {
                val |= DREF_SSC1_ENABLE;
            } else {
                val &= !DREF_SSC1_ENABLE;
            }

            intel_de_write(display, PCH_DREF_CONTROL, val);
            intel_de_posting_read(display, PCH_DREF_CONTROL);
            udelay(200);

            val &= !DREF_CPU_SOURCE_OUTPUT_MASK;
            if has_cpu_edp {
                if intel_panel_use_ssc(display) && can_ssc {
                    val |= DREF_CPU_SOURCE_OUTPUT_DOWNSPREAD;
                } else {
                    val |= DREF_CPU_SOURCE_OUTPUT_NONSPREAD;
                }
            } else {
                val |= DREF_CPU_SOURCE_OUTPUT_DISABLE;
            }
            intel_de_write(display, PCH_DREF_CONTROL, val);
            intel_de_posting_read(display, PCH_DREF_CONTROL);
            udelay(200);
        } else {
            val &= !DREF_CPU_SOURCE_OUTPUT_MASK;
            val |= DREF_CPU_SOURCE_OUTPUT_DISABLE;
            intel_de_write(display, PCH_DREF_CONTROL, val);
            intel_de_posting_read(display, PCH_DREF_CONTROL);
            udelay(200);

            if !using_ssc_source {
                val &= !DREF_SSC_SOURCE_MASK;
                val |= DREF_SSC_SOURCE_DISABLE;
                val &= !DREF_SSC1_ENABLE;
                intel_de_write(display, PCH_DREF_CONTROL, val);
                intel_de_posting_read(display, PCH_DREF_CONTROL);
                udelay(200);
            }
        }
        // drm_WARN_ON(display->drm, val != final)
        let _ = val != final_;
    }
}

/// `intel_init_pch_refclk()`: called once at driver load.
// upstream: intel_pch_refclk.c intel_init_pch_refclk()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_init_pch_refclk(display: *mut IntelDisplay) {
    unsafe {
        let pch = pch_type(display);
        if pch == PCH_IBX || pch == PCH_CPT {
            ilk_init_pch_refclk(display);
        } else if pch == PCH_LPT_H || pch == PCH_LPT_LP {
            lpt_init_pch_refclk(display);
        }
    }
}
