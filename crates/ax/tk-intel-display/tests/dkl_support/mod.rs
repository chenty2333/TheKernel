// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
#![allow(dead_code)]
use std::cell::{Cell, RefCell};

use tk_intel_display::{Error, RegisterIo, dkl_phy::DklIo, dpll_mgr::PllReadoutIo};

pub struct Model {
    pub trace: RefCell<Vec<(u32, u32, u32)>>,
    pub selector: Cell<u32>,
    pub seed: u32,
    pub fault: Cell<Option<usize>>,
    pub restore_fault: Cell<bool>,
    pub powered: Cell<bool>,
    pub enabled: Cell<bool>,
    pub locked: Cell<bool>,
    pub power_ref: Cell<bool>,
    pub accesses: Cell<usize>,
}
impl Model {
    pub fn new(seed: u32) -> Self {
        Self {
            trace: RefCell::new(Vec::new()),
            selector: Cell::new(0x03020100),
            seed,
            fault: Cell::new(None),
            restore_fault: Cell::new(false),
            powered: Cell::new(true),
            enabled: Cell::new(true),
            locked: Cell::new(false),
            power_ref: Cell::new(false),
            accesses: Cell::new(0),
        }
    }
    fn fail(&self, reg: u32) -> Result<(), Error> {
        let n = self.accesses.get();
        self.accesses.set(n + 1);
        if self.fault.get() == Some(n) {
            self.fault.set(None);
            Err(Error::Unavailable(reg))
        } else {
            Ok(())
        }
    }
}
impl RegisterIo for Model {
    fn read32(&self, reg: u32) -> Result<u32, Error> {
        self.fail(reg)?;
        let value = if reg == 0x1010a0 {
            self.selector.get()
        } else if (0x46038..=0x46050).contains(&reg) {
            assert!(self.power_ref.get());
            if self.enabled.get() { 0xc8000000 } else { 0 }
        } else {
            assert!(self.locked.get());
            let port = (reg - 0x168000) / 0x1000;
            let bank = (self.selector.get() >> (8 * port)) & 15;
            (reg.wrapping_mul(0x10001) ^ (bank << 24) ^ self.seed) | 0x00000201
        };
        self.trace.borrow_mut().push((0, reg, value));
        Ok(value)
    }
    fn write32(&self, reg: u32, value: u32) -> Result<(), Error> {
        assert!(self.locked.get());
        // Model a store which landed before the backend reported a failure.
        self.trace.borrow_mut().push((1, reg, value));
        if reg == 0x1010a0 {
            self.selector.set(value);
        }
        self.fail(reg)?;
        if reg == 0x1010a0 && value == 0x03020100 && self.restore_fault.get() {
            return Err(Error::Unavailable(reg));
        }
        Ok(())
    }
}
impl DklIo for Model {
    fn with_dkl_lock<T>(&self, f: impl FnOnce() -> Result<T, Error>) -> Result<T, Error> {
        assert!(!self.locked.replace(true));
        let result = f();
        self.locked.set(false);
        result
    }
}
impl PllReadoutIo for Model {
    fn with_display_core_if_enabled<T>(
        &self,
        f: impl FnOnce() -> Result<T, Error>,
    ) -> Result<Option<T>, Error> {
        if !self.powered.get() {
            return Ok(None);
        }
        assert!(!self.power_ref.replace(true));
        let result = f().map(Some);
        self.power_ref.set(false);
        result
    }
}
