// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
mod support;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    process::Command,
};

use tk_intel_gt::{Error, GtIo, bcs};
struct Io {
    words: RefCell<BTreeMap<u32, u32>>,
    log: RefCell<Vec<(u32, u32)>>,
    step: Cell<usize>,
    fail: usize,
}
impl GtIo for Io {
    fn read(&self, r: u32) -> Result<u32, Error> {
        self.step.set(self.step.get() + 1);
        if self.step.get() == self.fail {
            return Err(Error::Unavailable(r));
        }
        self.words
            .borrow()
            .get(&r)
            .copied()
            .ok_or(Error::Unavailable(r))
    }
    fn write(&self, r: u32, v: u32) -> Result<(), Error> {
        self.log.borrow_mut().push((r, v));
        self.words.borrow_mut().insert(r, v);
        self.step.set(self.step.get() + 1);
        if self.step.get() == self.fail {
            Err(Error::Unavailable(r))
        } else {
            Ok(())
        }
    }
    fn now_us(&self) -> u64 {
        0
    }
    fn delay_us(&self, _: u32) {
        panic!("unexpected delay")
    }
}
fn io(fail: usize) -> Io {
    Io {
        words: RefCell::new(BTreeMap::new()),
        log: RefCell::new(Vec::new()),
        step: Cell::new(0),
        fail,
    }
}
#[test]
#[ignore = "requires local Linux7.2.3/GCC"]
fn source_gen12_whitelist_slots_and_unused_nop_entries_match() {
    let root = support::reference();
    let source = support::read(&root, "gt/intel_workarounds.c");
    let functions = [
        support::function(&source, "static void allow_read_ctx_timestamp("),
        support::function(&source, "static void tgl_whitelist_build("),
        support::function(&source, "void intel_engine_apply_whitelist("),
    ]
    .join("\n");
    let mut defs = String::new();
    for (file, names) in [
        (
            "gt/intel_engine_regs.h",
            vec![
                "RING_CTX_TIMESTAMP",
                "RING_FORCE_TO_NONPRIV",
                "RING_FORCE_TO_NONPRIV_ACCESS_RD",
                "RING_FORCE_TO_NONPRIV_RANGE_4",
                "RING_MAX_NONPRIV_SLOTS",
                "RING_NOPID",
            ],
        ),
        (
            "gt/intel_gt_regs.h",
            vec![
                "PS_INVOCATION_COUNT",
                "GEN7_COMMON_SLICE_CHICKEN1",
                "HIZ_CHICKEN",
                "GEN11_COMMON_SLICE_CHICKEN3",
            ],
        ),
    ] {
        defs += &support::defines(&support::read(&root, file), &names);
    }
    let prefix = r#"
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
typedef uint32_t u32;typedef struct{u32 reg;}i915_reg_t;
#define _MMIO(n) ((i915_reg_t){n})
#define REG_BIT(n) (1u<<(n))
#define RENDER_CLASS 0
struct i915_wa{i915_reg_t reg;};struct i915_wa_list{unsigned count;struct i915_wa*list;};struct intel_uncore{int unused;};
struct intel_engine_cs{struct i915_wa_list whitelist;struct intel_uncore*uncore;u32 mmio_base;unsigned class;};
static u32 i915_mmio_reg_offset(i915_reg_t r){return r.reg;}
static void whitelist_reg_ext(struct i915_wa_list*w,i915_reg_t r,u32 f){r.reg|=f;unsigned n=w->count++;while(n&&w->list[n-1].reg.reg>r.reg){w->list[n]=w->list[n-1];n--;}w->list[n].reg=r;}
static void whitelist_reg(struct i915_wa_list*w,i915_reg_t r){whitelist_reg_ext(w,r,0);}
static void intel_uncore_write(struct intel_uncore*u,i915_reg_t r,u32 v){printf("%u %u\n",r.reg,v);}
"#;
    let main = r#"
int main(void){for(unsigned kind=0;kind<2;kind++){struct i915_wa list[12]={0};struct intel_engine_cs e={.class=kind?0:3,.mmio_base=kind?0x2000:0x22000,.whitelist={.list=list}};tgl_whitelist_build(&e);intel_engine_apply_whitelist(&e);}return 0;}
"#;
    let oracle = support::compile(&[prefix, &defs, &functions, main].join("\n"), "nonpriv");
    let out = Command::new(&oracle.executable).output().unwrap();
    assert!(out.status.success());
    let expected: Vec<(u32, u32)> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|line| {
            let mut f = line.split_whitespace();
            (
                f.next().unwrap().parse().unwrap(),
                f.next().unwrap().parse().unwrap(),
            )
        })
        .collect();
    let model = io(0);
    bcs::apply_nonpriv(&model, false).unwrap();
    bcs::apply_nonpriv(&model, true).unwrap();
    assert_eq!(*model.log.borrow(), expected);
}
#[test]
fn whitelist_store_and_readback_faults_never_report_complete_programming() {
    for render in [false, true] {
        let model = io(0);
        bcs::apply_nonpriv(&model, render).unwrap();
        assert_eq!(model.step.get(), 24);
        for fail in 1..=24 {
            assert!(bcs::apply_nonpriv(&io(fail), render).is_err());
        }
    }
}
