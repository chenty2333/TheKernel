// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
mod support;
use std::{
    io::Write,
    process::{Command, Stdio},
};
#[test]
#[ignore = "requires local Linux 7.2.3 and GCC; run explicitly"]
fn bcs_domain_prepare_and_double_reset_match_compiled_i915() {
    let root = support::reference();
    let engine = support::read(&root, "gt/intel_engine_cs.c");
    let reset = support::read(&root, "gt/intel_reset.c");
    let regs = support::read(&root, "gt/intel_gt_regs.h");
    let mut names: Vec<_> = regs
        .lines()
        .filter_map(|l| l.trim_start().strip_prefix("#define"))
        .map(|l| l.trim_start().split([' ', '\t', '(']).next().unwrap())
        .filter(|n| n.contains("GRDOM") || *n == "GEN6_GDRST")
        .collect();
    names.sort_unstable();
    names.dedup();
    let definitions = support::defines(&regs, &names);
    let engine_regs = support::read(&root, "gt/intel_engine_regs.h");
    let fields = support::defines(
        &engine_regs,
        &[
            "RING_RESET_CTL",
            "RESET_CTL_CAT_ERROR",
            "RESET_CTL_READY_TO_RESET",
            "RESET_CTL_REQUEST_RESET",
        ],
    );
    let prefix = r#"
#include <stdint.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <errno.h>
typedef uint32_t u32;typedef uint8_t u8;
typedef struct {u32 reg;} i915_reg_t;
#define _MMIO(n) ((i915_reg_t){n})
#define REG_BIT(n) (1U<<(n))
#define REG_MASKED_FIELD_ENABLE(n) ((n)|((n)<<16))
#define REG_MASKED_FIELD_DISABLE(n) ((n)<<16)
#define ARRAY_SIZE(a) (sizeof(a)/sizeof((a)[0]))
#define GEM_BUG_ON(c) do {if(c)abort();}while(0)
#define I915_SELFTEST_ONLY(c) 0
#define GRAPHICS_VER_FULL(i) 1200
#define IP_VER(a,b) ((a)*100+(b))
#define GT_TRACE(...) ((void)0)
#define gt_err(...) ((void)0)
enum intel_engine_id {RCS0,BCS0,BCS1,BCS2,BCS3,BCS4,BCS5,BCS6,BCS7,BCS8,VCS0,VCS1,VCS2,VCS3,VCS4,VCS5,VCS6,VCS7,VECS0,VECS1,VECS2,VECS3,CCS0,CCS1,CCS2,CCS3,GSC0,I915_NUM_ENGINES};
struct intel_uncore {int unused;};struct intel_gt {struct intel_uncore *uncore;void*i915;};
struct intel_engine_cs {struct intel_gt *gt;struct intel_uncore *uncore;u32 mmio_base;const char*name;};
static u32 ctl;static unsigned nreset;static int fail_reset;
static u32 intel_uncore_read_fw(struct intel_uncore*u,i915_reg_t r){if(r.reg!=0x220d0&&r.reg!=0x941c)abort();return r.reg==0x220d0?ctl:0;}
static void intel_uncore_write_fw(struct intel_uncore*u,i915_reg_t r,u32 v){
 printf("W %u %u ",r.reg,v);
 if(r.reg==0x220d0){ctl=(ctl&~(v>>16))|(v&(v>>16));if(ctl&4)ctl&=~4;if(ctl&1)ctl|=2;}
 else if(r.reg==0x941c){nreset++;if(v!=4)abort();}else abort();}
static int __intel_wait_for_register_fw(struct intel_uncore*u,i915_reg_t r,u32 mask,u32 value,unsigned us,unsigned ms,void*last){
 printf("P %u %u %u %u ",r.reg,mask,value,us);return r.reg==0x941c&&fail_reset?-1:0;}
static void udelay(unsigned us){printf("D %u ",us);}
"#;
    let main = r#"
int main(void){while(scanf("%u %d",&ctl,&fail_reset)==2){
 struct intel_uncore u={0};struct intel_gt gt={.uncore=&u};struct intel_engine_cs e={.gt=&gt,.uncore=&u,.mmio_base=0x22000,.name="bcs0"};
 nreset=0;printf("M %u ",get_reset_domain(12,BCS0));
 int result=gen8_engine_reset_prepare(&e);if(!result)result=gen6_hw_domain_reset(&gt,get_reset_domain(12,BCS0));
 gen8_engine_reset_cancel(&e);printf("R %d %u %u\n",result,nreset,ctl&1);}}
"#;
    let code = [
        prefix,
        &definitions,
        &fields,
        &support::function(&engine, "static u32 get_reset_domain("),
        &support::function(&reset, "static int gen8_engine_reset_prepare("),
        &support::function(&reset, "static void gen8_engine_reset_cancel("),
        &support::function(&reset, "static int gen6_hw_domain_reset("),
        main,
    ]
    .join("\n");
    let oracle = support::compile(&code, "gt-reset");
    let mut child = Command::new(&oracle.executable)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"0 0\n2 0\n4 0\n0 1\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let out = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.lines().count(), 4);
    for (i, line) in out.lines().enumerate() {
        assert!(line.starts_with("M 4 "));
        assert!(line.contains("W 139472 65536 R "));
        let resets = if i == 3 { 1 } else { 2 };
        assert_eq!(line.matches("W 37916 4 ").count(), resets);
        assert!(line.contains("P 37916 4 0 2000 "));
        assert!(line.contains("D 50 "));
        if i == 0 || i == 3 {
            assert!(line.contains("W 139472 65537 P 139472 2 2 700 "));
        }
        if i == 2 {
            assert!(line.contains("W 139472 262148 P 139472 4 0 700 "));
        }
        assert!(line.ends_with(&format!("R {} {resets} 0", if i == 3 { -1 } else { 0 })));
    }
    println!(
        "compiled i915 Gen12 BCS domain/prepare/cancel/double-reset traces agree; not GPU \
         execution"
    );
}
