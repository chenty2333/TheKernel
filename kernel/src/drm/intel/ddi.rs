// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See the repository MIT license.
//! Source-shaped DPCLKA DDI clock mux RMW adapter for ICL/TGL/RKL/ADL.

use intel_display::ddi::{
    DdiClockPlan, DdiClockPlatform, ddi_clock_disable_write, ddi_clock_enable_writes,
    ddi_combo_clock_plan, ddi_combo_clock_pll_id,
};

use super::regs::{self, Meaning, Register, Registers};

static DPLL_MUX_LOCK: spin::Mutex<()> = spin::Mutex::new(());

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DdiClockError {
    Unreadable,
    WriteRefused,
    Unsupported,
}

fn register(plan: DdiClockPlan) -> Register {
    match plan.register {
        0x164280 => regs::dpll::ICL_DPCLKA_CFGCR0,
        _ => Register::read_write("DPCLKA_CFGCR", plan.register, Meaning::BringUp, None),
    }
}

fn update<R: Registers>(
    regs: &R,
    register: Register,
    clear: u32,
    set: u32,
) -> Result<(), DdiClockError> {
    let value = regs.read(register).ok_or(DdiClockError::Unreadable)?;
    regs.write(register, (value & !clear) | set)
        .then_some(())
        .ok_or(DdiClockError::WriteRefused)
}

/// Select the PLL then ungate the DDI clock in two separate locked RMW writes.
// upstream: intel_ddi.c _icl_ddi_enable_clock()
pub(crate) fn enable_combo_clock<R: Registers>(
    regs: &R,
    plan: DdiClockPlan,
) -> Result<(), DdiClockError> {
    let register = register(plan);
    let _guard = DPLL_MUX_LOCK.lock();
    for (offset, clear, set) in ddi_clock_enable_writes(plan) {
        debug_assert_eq!(offset, plan.register);
        update(regs, register, clear, set)?;
    }
    Ok(())
}

/// Gate the DDI clock while retaining its PLL selector, matching i915.
// upstream: intel_ddi.c _icl_ddi_disable_clock()
pub(crate) fn disable_combo_clock<R: Registers>(
    regs: &R,
    plan: DdiClockPlan,
) -> Result<(), DdiClockError> {
    let register = register(plan);
    let (offset, clear, set) = ddi_clock_disable_write(plan);
    debug_assert_eq!(offset, plan.register);
    let _guard = DPLL_MUX_LOCK.lock();
    update(regs, register, clear, set)
}

/// Read whether hardware has ungated the DDI clock.
// upstream: intel_ddi.c _icl_ddi_is_clock_enabled()
pub(crate) fn combo_clock_is_enabled<R: Registers>(
    regs: &R,
    plan: DdiClockPlan,
) -> Result<bool, DdiClockError> {
    let value = regs.read(register(plan)).ok_or(DdiClockError::Unreadable)?;
    Ok(value & plan.clock_off_mask == 0)
}

/// Read the DPLL selector field using the same per-platform DPCLKA layout.
// upstream: intel_ddi.c _icl_ddi_get_pll()
pub(crate) fn combo_clock_pll_id<R: Registers>(
    regs: &R,
    platform: DdiClockPlatform,
    phy: u8,
) -> Result<u8, DdiClockError> {
    let plan = ddi_combo_clock_plan(platform, phy, 0).map_err(|_| DdiClockError::Unsupported)?;
    let value = regs.read(register(plan)).ok_or(DdiClockError::Unreadable)?;
    ddi_combo_clock_pll_id(platform, phy, value).map_err(|_| DdiClockError::Unsupported)
}
