// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
mod dkl_support;
use dkl_support::Model;
use tk_intel_display::{Error, dkl_phy::*, dpll_mgr::dkl_pll_get_hw_state};
#[test]
fn aperture_index_layout_and_reject_before_access() {
    for (p, n) in [TcPort::Tc1, TcPort::Tc2, TcPort::Tc3, TcPort::Tc4]
        .into_iter()
        .zip(0..4)
    {
        for bank in 0..16 {
            let r = DklRegister::new(p, bank * 0x1000 + 0x2c0).unwrap();
            assert_eq!(r.aperture(), 0x1682c0 + n * 0x1000);
            assert_eq!(r.index_value(), bank << (8 * n));
            assert_eq!(r.selector(), 0x1010a0);
        }
        assert!(DklRegister::new(p, 0x10000).is_err());
        assert!(DklRegister::new(p, 0x2201).is_err());
        assert_eq!(p.pll_enable(), 0x46038 + n * 8);
    }
}
#[test]
fn each_operation_selects_bank_and_rmw_always_stores() {
    let io = Model::new(4);
    let r = DklRegister::new(TcPort::Tc2, 0x12c0).unwrap();
    intel_dkl_phy_read(&io, r).unwrap();
    intel_dkl_phy_write(&io, r, 42).unwrap();
    intel_dkl_phy_rmw(&io, r, 0, 0).unwrap();
    intel_dkl_phy_posting_read(&io, r).unwrap();
    let t = io.trace.borrow();
    assert_eq!(t.len(), 9);
    for i in [0, 2, 4, 7] {
        assert_eq!(t[i], (1, 0x1010a0, 0x100));
    }
    assert_eq!(t[5].1, 0x1692c0);
    assert_eq!(t[6], (1, t[5].1, t[5].2));
}
#[test]
fn pll_dark_domain_and_disabled_pll_do_not_touch_phy() {
    let io = Model::new(4);
    io.powered.set(false);
    assert_eq!(
        dkl_pll_get_hw_state(&io, TcPort::Tc1, 24000, false),
        Ok(None)
    );
    assert!(io.trace.borrow().is_empty());
    io.powered.set(true);
    io.enabled.set(false);
    assert_eq!(
        dkl_pll_get_hw_state(&io, TcPort::Tc1, 24000, false),
        Ok(None)
    );
    assert_eq!(io.trace.borrow().len(), 1);
    assert!(!io.power_ref.get());
}
#[test]
fn pll_readout_restores_selector_and_releases_locks_at_every_fault_prefix() {
    for fault in 0..18 {
        let io = Model::new(0x52345678);
        io.fault.set(Some(fault));
        assert!(
            dkl_pll_get_hw_state(&io, TcPort::Tc2, 24000, true).is_err(),
            "{fault}"
        );
        assert_eq!(io.selector.get(), 0x03020100, "fault {fault}");
        assert!(!io.locked.get());
        assert!(!io.power_ref.get());
        assert!(
            io.trace
                .borrow()
                .iter()
                .all(|&(op, r, _)| op == 0 || r == 0x1010a0)
        );
    }
}
#[test]
fn failed_or_unverifiable_selector_restore_requires_quarantine() {
    for fault in [Some(18), Some(19), None] {
        let io = Model::new(0x12345678);
        io.fault.set(fault);
        if fault.is_none() {
            io.restore_fault.set(true);
        }
        assert_eq!(
            dkl_pll_get_hw_state(&io, TcPort::Tc1, 24000, false),
            Err(Error::RestoreFailed(0x1010a0))
        );
        assert!(!io.locked.get());
        assert!(!io.power_ref.get());
    }
}
