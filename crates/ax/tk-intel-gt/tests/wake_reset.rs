// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
};

use tk_intel_gt::{Error, GtIo, reset, uncore};
struct Model {
    words: RefCell<BTreeMap<u32, u32>>,
    writes: RefCell<Vec<(u32, u32)>>,
    time: Cell<u64>,
    fail: Cell<Option<usize>>,
    fallback: Cell<bool>,
    never_ack: Cell<bool>,
    reset_stall: Cell<bool>,
    hidden: Cell<Option<u32>>,
}
impl Model {
    fn new() -> Self {
        Self {
            words: RefCell::new(BTreeMap::from([
                (uncore::GT_REQUEST, 1 << 12),
                (uncore::GT_ACK, 0),
                (0x2209c, 1 << 9),
                (0x2229c, 0),
                (0x220d0, 0),
                (0x941c, 0),
                (0x800c, 0),
                (0xa2a0, 0),
                (0x22034, 0),
                (0x22030, 0),
            ])),
            writes: RefCell::new(Vec::new()),
            time: Cell::new(0),
            fail: Cell::new(None),
            fallback: Cell::new(false),
            never_ack: Cell::new(false),
            reset_stall: Cell::new(false),
            hidden: Cell::new(None),
        }
    }
}
impl GtIo for Model {
    fn read(&self, r: u32) -> Result<u32, Error> {
        self.time.set(self.time.get() + 1000);
        if self.hidden.get() == Some(r) {
            return Err(Error::Unavailable(r));
        }
        self.words
            .borrow()
            .get(&r)
            .copied()
            .ok_or(Error::Unavailable(r))
    }
    fn write(&self, r: u32, v: u32) -> Result<(), Error> {
        self.writes.borrow_mut().push((r, v));
        let failed = self.fail.get() == Some(self.writes.borrow().len());
        let mut w = self.words.borrow_mut();
        if [uncore::GT_REQUEST, 0x2209c, 0x2229c, 0x220d0].contains(&r) {
            let old = w.get(&r).copied().ok_or(Error::Unavailable(r))?;
            let value = (old & !(v >> 16)) | (v & (v >> 16));
            w.insert(r, value);
            if r == uncore::GT_REQUEST {
                if value & (1 << 15) != 0 {
                    self.fallback.set(false);
                }
                let kernel = if self.fallback.get() || self.never_ack.get() {
                    0
                } else {
                    value & 1
                };
                w.insert(uncore::GT_ACK, kernel | (value & (1 << 15)));
            }
            if r == 0x220d0 {
                w.insert(
                    r,
                    if value & 1 != 0 {
                        value | 2
                    } else {
                        value & !4
                    },
                );
            }
        } else {
            w.insert(
                r,
                if r == 0x941c && !self.reset_stall.get() {
                    0
                } else {
                    v
                },
            );
        }
        if failed {
            Err(Error::Unavailable(r))
        } else {
            Ok(())
        }
    }
    fn now_us(&self) -> u64 {
        self.time.get()
    }
    fn delay_us(&self, n: u32) {
        self.time.set(self.time.get() + u64::from(n));
    }
}
#[test]
fn fresh_forcewake_preserves_debug_requests_and_release_is_acknowledged() {
    let m = Model::new();
    uncore::acquire_gt(&m).unwrap();
    assert_eq!(m.words.borrow()[&uncore::GT_REQUEST], (1 << 12) | 1);
    uncore::release_gt(&m).unwrap();
    assert_eq!(m.words.borrow()[&uncore::GT_REQUEST], 1 << 12);
    assert_eq!(m.writes.borrow()[0], (uncore::GT_REQUEST, 0xefff0000));
}
#[test]
fn fallback_forcewake_workaround_delivers_kernel_ack_without_leaking_fallback() {
    let m = Model::new();
    m.fallback.set(true);
    uncore::acquire_gt(&m).unwrap();
    assert!(
        m.writes
            .borrow()
            .contains(&(uncore::GT_REQUEST, 0x80008000))
    );
    assert_eq!(m.words.borrow()[&uncore::GT_REQUEST] & 0x8000, 0);
    uncore::release_gt(&m).unwrap();
}
#[test]
fn missing_ack_and_landed_acquire_failure_are_not_ownership() {
    let m = Model::new();
    m.hidden.set(Some(uncore::GT_ACK));
    assert_eq!(uncore::acquire_gt(&m), Err(Error::Quarantined));
    let m = Model::new();
    m.fail.set(Some(2));
    assert!(uncore::acquire_gt(&m).is_err());
    assert_eq!(m.words.borrow()[&uncore::GT_ACK] & 1, 0);
}
#[test]
fn bcs_reset_uses_gen11_domain_not_old_blt_bit_and_keeps_stop_until_resume() {
    let m = Model::new();
    uncore::acquire_gt(&m).unwrap();
    reset::stop_and_reset_bcs(&m).unwrap();
    let reset: Vec<_> = m
        .writes
        .borrow()
        .iter()
        .filter(|(r, _)| *r == 0x941c)
        .copied()
        .collect();
    assert_eq!(reset, [(0x941c, 4), (0x941c, 4)]);
    assert_eq!(m.words.borrow()[&0x2209c] & (1 << 8), 1 << 8);
    assert_eq!(m.words.borrow()[&0x2229c] & (1 << 10), 1 << 10);
    assert_eq!(m.words.borrow()[&0x220d0] & 1, 0);
    assert!(
        !m.writes
            .borrow()
            .iter()
            .any(|(r, v)| *r == 0x941c && *v != 4)
    );
}
#[test]
fn pending_mi_forcewake_is_completed_before_reset_and_failure_cancels_request() {
    let m = Model::new();
    m.words.borrow_mut().insert(0x800c, (3 << 9) | (3 << 25));
    assert_eq!(reset::stop_and_reset_bcs(&m), Err(Error::Timeout(0xa2a0)));
    assert!(!m.writes.borrow().iter().any(|(r, _)| *r == 0x941c));
    assert_eq!(m.writes.borrow().last(), Some(&(0x220d0, 1 << 16)));
    let m = Model::new();
    m.words.borrow_mut().insert(0x800c, (3 << 9) | (3 << 25));
    m.words.borrow_mut().insert(0xa2a0, 3);
    reset::stop_and_reset_bcs(&m).unwrap();
}
#[test]
fn every_landed_reset_write_fault_attempts_cancel_without_resuming_dma() {
    let baseline = Model::new();
    reset::stop_and_reset_bcs(&baseline).unwrap();
    let count = baseline.writes.borrow().len();
    for prefix in 1..=count {
        let m = Model::new();
        m.fail.set(Some(prefix));
        assert!(reset::stop_and_reset_bcs(&m).is_err());
        assert_eq!(m.writes.borrow().last(), Some(&(0x220d0, 1 << 16)));
        assert!(
            !m.writes
                .borrow()
                .iter()
                .any(|(r, v)| *r == 0x2209c && *v == 1 << 24)
        );
    }
    let m = Model::new();
    m.reset_stall.set(true);
    assert_eq!(reset::stop_and_reset_bcs(&m), Err(Error::Timeout(0x941c)));
}
