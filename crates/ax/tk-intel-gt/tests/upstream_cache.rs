// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
mod support;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    process::Command,
};

use tk_intel_gt::{Error, GtIo, cache};
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
        self.step.set(self.step.get() + 1);
        self.log.borrow_mut().push((r, v));
        self.words.borrow_mut().insert(r, v);
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
fn source_adln_mocs_every_slot_l3_pair_and_pat_match() {
    let root = support::reference();
    let source = support::read(&root, "gt/intel_mocs.c");
    let defs = &source[source.find("#define _LE_CACHEABILITY").unwrap()
        ..source.find("/*\n * MOCS tables").unwrap()];
    let entries = support::defines(&source, &["GEN11_MOCS_ENTRIES"]);
    let table = support::function(
        &source,
        "static const struct drm_i915_mocs_entry gen12_mocs_table[]",
    );
    let control = support::function(&source, "static u32 get_entry_control(");
    let l3 = support::function(&source, "static u16 get_entry_l3cc(");
    let pat_source = support::read(&root, "gt/intel_gtt.c");
    let pat = support::function(&pat_source, "static void tgl_setup_private_ppat(");
    let registers = support::read(&root, "gt/intel_gtt.h");
    let pat_defs = support::defines(
        &registers,
        &[
            "GEN8_PPAT_WB",
            "GEN8_PPAT_WC",
            "GEN8_PPAT_WT",
            "GEN8_PPAT_UC",
        ],
    );
    let prefix = r#"
#include <stdint.h>
#include <stdio.h>
typedef uint32_t u32;typedef uint16_t u16;
struct drm_i915_mocs_entry{u32 control_value;u16 l3cc_value;int used;};
struct drm_i915_mocs_table{unsigned size,n_entries,unused_entries_index;const struct drm_i915_mocs_entry*table;};
struct intel_uncore{int unused;};
#define GEN12_PAT_INDEX(n) (0x4800+(n)*4)
static void intel_uncore_write(struct intel_uncore*u,u32 r,u32 v){(void)u;printf("%u %u\n",r,v);}
"#;
    let main = r#"
int main(void){struct drm_i915_mocs_table t={.size=64,.n_entries=64,.unused_entries_index=2,.table=gen12_mocs_table};
for(unsigned i=0;i<64;i++)printf("%u %u\n",0x4000+4*i,get_entry_control(&t,i));
for(unsigned i=0;i<32;i++)printf("%u %u\n",0xb020+4*i,(u32)get_entry_l3cc(&t,i*2)|((u32)get_entry_l3cc(&t,i*2+1)<<16));
tgl_setup_private_ppat(0);return 0;}
"#;
    let oracle = support::compile(
        &[
            prefix, defs, &entries, &table, ";", &control, &l3, &pat_defs, &pat, main,
        ]
        .join("\n"),
        "cache",
    );
    let output = Command::new(&oracle.executable).output().unwrap();
    assert!(output.status.success());
    let expected: Vec<(u32, u32)> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|s| {
            let v: Vec<u32> = s.split_whitespace().map(|v| v.parse().unwrap()).collect();
            (v[0], v[1])
        })
        .collect();
    let io = io(0);
    cache::prepare(&io).unwrap();
    assert_eq!(*io.log.borrow(), expected);
    assert_eq!(expected.len(), 104);
}
#[test]
fn every_shared_cache_store_and_readback_error_stops_before_more_writes() {
    for fail in 1..=208 {
        let io = io(fail);
        assert!(cache::prepare(&io).is_err());
        assert_eq!(io.step.get(), fail);
    }
}
