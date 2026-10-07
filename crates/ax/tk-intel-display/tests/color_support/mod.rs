// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
#![allow(dead_code)]
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
};

use tk_intel_display::{
    Error, RegisterIo,
    color::ColorIo,
    display::{Pipe, ReadoutIo},
};
pub struct Model {
    pub pipe: Pipe,
    pub mode: u32,
    pub csc: u32,
    pub bottom: u32,
    pub seed: u32,
    pub powered: Cell<bool>,
    pub locked: Cell<bool>,
    pub fault: Cell<Option<usize>>,
    pub accesses: Cell<usize>,
    pub data_ticks: Cell<u32>,
    pub selectors: RefCell<BTreeMap<u32, u32>>,
    pub trace: RefCell<Vec<(u32, u32, u32)>>,
}
impl Model {
    pub fn new(pipe: Pipe, mode: u32, csc: u32, seed: u32) -> Self {
        let shift = pipe.index() * 0x800;
        Self {
            pipe,
            mode,
            csc,
            bottom: 0x31234567,
            seed,
            powered: Cell::new(true),
            locked: Cell::new(false),
            fault: Cell::new(None),
            accesses: Cell::new(0),
            data_ticks: Cell::new(0),
            selectors: RefCell::new(
                [
                    (0x4a484 + shift, 0x25),
                    (0x4a400 + shift, 0x8002),
                    (0x4a408 + shift, 0x8004),
                ]
                .into(),
            ),
            trace: RefCell::new(Vec::new()),
        }
    }
    fn fail(&self, r: u32) -> Result<(), Error> {
        let n = self.accesses.get();
        self.accesses.set(n + 1);
        if self.fault.get() == Some(n) {
            self.fault.set(None);
            Err(Error::Unavailable(r))
        } else {
            Ok(())
        }
    }
}
impl RegisterIo for Model {
    fn read32(&self, r: u32) -> Result<u32, Error> {
        assert!(self.locked.get());
        self.fail(r)?;
        let shift = self.pipe.index() * 0x800;
        let v = if r == 0x4a480 + shift {
            self.mode
        } else if r == 0x49028 + self.pipe.index() * 0x100 {
            self.csc
        } else if r == 0x70034 + self.pipe.index() * 0x1000 {
            self.bottom
        } else if let Some(v) = self.selectors.borrow().get(&r) {
            *v
        } else {
            let tick = self.data_ticks.get();
            self.data_ticks.set(tick + 1);
            r ^ self.seed ^ tick.wrapping_mul(0x01020304)
        };
        self.trace.borrow_mut().push((0, r, v));
        Ok(v)
    }
    fn write32(&self, r: u32, v: u32) -> Result<(), Error> {
        assert!(self.locked.get());
        assert!(self.selectors.borrow().contains_key(&r), "color data write");
        self.selectors.borrow_mut().insert(r, v);
        self.trace.borrow_mut().push((1, r, v));
        self.fail(r)
    }
}
impl ReadoutIo for Model {
    fn pipe_powered(&self, p: Pipe) -> bool {
        p == self.pipe && self.powered.get()
    }
}
impl ColorIo for Model {
    fn with_color_lock<T>(&self, f: impl FnOnce() -> Result<T, Error>) -> Result<T, Error> {
        assert!(!self.locked.replace(true));
        let r = f();
        self.locked.set(false);
        r
    }
}
