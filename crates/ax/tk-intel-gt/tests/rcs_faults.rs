// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
};

use tk_intel_gt::{Error, GtIo, rcs};
struct Model {
    words: RefCell<BTreeMap<u32, u32>>,
    step: Cell<usize>,
    fail: usize,
}
impl Model {
    fn new(fail: usize) -> Self {
        Self {
            words: RefCell::new(BTreeMap::from([
                (0xfdc, 0x82000000),
                (0x9138, 1),
                (0x913c, 4),
                (0x5584, 0x1234),
                (0xb004, 0xffff),
                (0x20a0, 0),
            ])),
            step: Cell::new(0),
            fail,
        }
    }
    fn operation(&self, r: u32) -> Result<(), Error> {
        self.step.set(self.step.get() + 1);
        if self.step.get() == self.fail {
            Err(Error::Unavailable(r))
        } else {
            Ok(())
        }
    }
}
impl GtIo for Model {
    fn read(&self, r: u32) -> Result<u32, Error> {
        self.operation(r)?;
        self.words
            .borrow()
            .get(&r)
            .copied()
            .ok_or(Error::Unavailable(r))
    }
    fn write(&self, r: u32, v: u32) -> Result<(), Error> {
        self.words.borrow_mut().insert(r, v);
        self.operation(r)
    } // may land before error
    fn now_us(&self) -> u64 {
        0
    }
    fn delay_us(&self, _: u32) {}
}
#[test]
fn every_engine_wa_fault_restores_selector_or_reports_quarantine() {
    let baseline = Model::new(0);
    rcs::prepare(&baseline).unwrap();
    for step in 1..=baseline.step.get() {
        let m = Model::new(step);
        assert!(rcs::prepare(&m).is_err(), "fault {step}");
        assert_eq!(m.words.borrow()[&0xfdc], 0x82000000, "fault {step}");
    }
}
#[test]
fn every_context_wa_fault_restores_selector_and_never_publishes_partial_commands() {
    let baseline = Model::new(0);
    let mut words = [0; 14];
    rcs::context_wa(&baseline, &mut words).unwrap();
    for step in 1..=baseline.step.get() {
        let m = Model::new(step);
        let mut words = [0xdeadbeef; 14];
        assert!(rcs::context_wa(&m, &mut words).is_err(), "fault {step}");
        assert_eq!(m.words.borrow()[&0xfdc], 0x82000000, "fault {step}");
        assert_eq!(words, [0xdeadbeef; 14]);
    }
}
