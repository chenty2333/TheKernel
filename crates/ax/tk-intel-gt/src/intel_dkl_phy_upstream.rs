// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! ADL-P Dekel (DKL) PHY register access from Linux v7.2.3.
#![allow(non_snake_case, dead_code, unsafe_code)]

use crate::{
    intel_de_upstream::{intel_de_posting_read, intel_de_read, intel_de_rmw, intel_de_write},
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_limits_types_upstream::{PORT_TC1, PORT_TC4},
    intel_dkl_phy_regs_upstream::{
        DKL_REG_MMIO, DKL_REG_TC_PORT, HIP_INDEX_REG, HIP_INDEX_VAL, IntelDklPhyReg,
    },
};

#[inline]
fn dkl_reg_port_valid(reg: IntelDklPhyReg) -> bool {
    let port = DKL_REG_TC_PORT(reg) as i32;
    (0..=PORT_TC4 - PORT_TC1).contains(&port)
}

#[inline]
unsafe fn dkl_phy_set_hip_idx(display: *mut IntelDisplay, reg: IntelDklPhyReg) -> bool {
    if display.is_null() || !dkl_reg_port_valid(reg) {
        return false;
    }
    let tc_port = DKL_REG_TC_PORT(reg);
    unsafe {
        intel_de_write(
            display,
            HIP_INDEX_REG(tc_port),
            HIP_INDEX_VAL(tc_port, reg.bank_idx),
        );
    }
    true
}

/// Initialize the shared lock protecting the DKL HIP-index + MMIO pair.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dkl_phy_init(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    let lock = unsafe { core::ptr::addr_of_mut!((*display).dkl.phy_lock) };
    tk_linux_kapi::locks::spin_lock_init(unsafe { &mut *lock });
}

/// Read one banked DKL PHY register while serializing the shared HIP index.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dkl_phy_read(
    display: *mut IntelDisplay,
    reg: IntelDklPhyReg,
) -> u32 {
    if display.is_null() {
        return u32::MAX;
    }
    let lock = unsafe { core::ptr::addr_of_mut!((*display).dkl.phy_lock) };
    tk_linux_kapi::locks::spin_lock(unsafe { &mut *lock });
    let value = if unsafe { dkl_phy_set_hip_idx(display, reg) } {
        unsafe { intel_de_read(display, DKL_REG_MMIO(reg)) }
    } else {
        u32::MAX
    };
    tk_linux_kapi::locks::spin_unlock(unsafe { &mut *lock });
    value
}

/// Write one banked DKL PHY register while serializing the shared HIP index.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dkl_phy_write(
    display: *mut IntelDisplay,
    reg: IntelDklPhyReg,
    value: u32,
) {
    if display.is_null() {
        return;
    }
    let lock = unsafe { core::ptr::addr_of_mut!((*display).dkl.phy_lock) };
    tk_linux_kapi::locks::spin_lock(unsafe { &mut *lock });
    if unsafe { dkl_phy_set_hip_idx(display, reg) } {
        unsafe { intel_de_write(display, DKL_REG_MMIO(reg), value) };
    }
    tk_linux_kapi::locks::spin_unlock(unsafe { &mut *lock });
}

/// Read/modify/write one banked DKL PHY register under the same lock.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dkl_phy_rmw(
    display: *mut IntelDisplay,
    reg: IntelDklPhyReg,
    clear: u32,
    set: u32,
) {
    if display.is_null() {
        return;
    }
    let lock = unsafe { core::ptr::addr_of_mut!((*display).dkl.phy_lock) };
    tk_linux_kapi::locks::spin_lock(unsafe { &mut *lock });
    if unsafe { dkl_phy_set_hip_idx(display, reg) } {
        unsafe { intel_de_rmw(display, DKL_REG_MMIO(reg), clear, set) };
    }
    tk_linux_kapi::locks::spin_unlock(unsafe { &mut *lock });
}

/// Complete a posting read from one banked DKL PHY register.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dkl_phy_posting_read(
    display: *mut IntelDisplay,
    reg: IntelDklPhyReg,
) {
    if display.is_null() {
        return;
    }
    let lock = unsafe { core::ptr::addr_of_mut!((*display).dkl.phy_lock) };
    tk_linux_kapi::locks::spin_lock(unsafe { &mut *lock });
    if unsafe { dkl_phy_set_hip_idx(display, reg) } {
        unsafe { intel_de_posting_read(display, DKL_REG_MMIO(reg)) };
    }
    tk_linux_kapi::locks::spin_unlock(unsafe { &mut *lock });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dkl_access_rejects_registers_outside_adlp_tc_phy_window() {
        let valid = IntelDklPhyReg {
            reg: crate::intel_dkl_phy_regs_upstream::_DKL_PHY1_BASE,
            bank_idx: 0,
        };
        let invalid = IntelDklPhyReg {
            reg: crate::intel_dkl_phy_regs_upstream::_DKL_PHY1_BASE + 4 * 0x1000,
            bank_idx: 0,
        };
        assert!(dkl_reg_port_valid(valid));
        assert!(!dkl_reg_port_valid(invalid));
    }
}
