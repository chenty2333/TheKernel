// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
mod support;
use std::{collections::BTreeMap, process::Command};
#[test]
#[ignore = "requires local Linux7.2.3 and GCC; run explicitly"]
fn selected_n305_engine_and_context_workarounds_match_compiled_i915() {
    let root = support::reference();
    let source = support::read(&root, "gt/intel_workarounds.c");
    let mut defs = String::new();
    for file in ["gt/intel_gt_regs.h", "gt/intel_engine_regs.h", "i915_reg.h"] {
        let s = support::read(&root, file);
        let names: Vec<_> = s
            .lines()
            .filter_map(|l| l.trim_start().strip_prefix("#define"))
            .map(|l| l.trim_start().split([' ', '\t', '(']).next().unwrap())
            .collect();
        defs.push_str(&support::defines(&s, &names));
    }
    let funcs = [
        "static void gen12_ctx_workarounds_init(",
        "static void\nengine_fake_wa_init(",
        "static void\nrcs_engine_wa_init(",
        "static void\nadd_render_compute_tuning_settings(",
        "static void\ngeneral_render_compute_wa_init(",
    ];
    let functions = funcs.map(|n| support::function(&source, n)).join("\n");
    let mut checks = String::new();
    let mut names = std::collections::BTreeSet::new();
    for token in functions.split(|c: char| !c.is_alphanumeric() && c != '_') {
        if token.starts_with("IS_") {
            names.insert(token);
        }
    }
    for name in names {
        checks.push_str(&format!(
            "#define {name}(...) {}\n",
            if name == "IS_ALDERLAKE_P" { "1" } else { "0" }
        ));
    }
    let prefix = r#"
#include <stdint.h>
#include <stdio.h>
#include <stdbool.h>
#include <stdlib.h>
typedef uint32_t u32;typedef uint8_t u8;typedef struct{u32 reg;}i915_reg_t;
#define _MMIO(n) ((i915_reg_t){n})
#define MCR_REG(n) _MMIO(n)
#define _MMIO_BASE(base,n) _MMIO((base)+(n))
#define _MMIO_PIPE(pipe,a,b) _MMIO(a)
#define BIT(n) (1U<<(n))
#define REG_BIT(n) BIT(n)
#define REG_GENMASK(h,l) ((UINT32_MAX>>(31-(h)))&(UINT32_MAX<<(l)))
#define REG_FIELD_PREP(m,v) (((v)<<__builtin_ctz(m))&(m))
#define REG_MASKED_FIELD_ENABLE(m) ((m)|((m)<<16))
#define GRAPHICS_VER(i) 12
#define GRAPHICS_VER_FULL(i) 1200
#define IP_VER(a,b) ((a)*100+(b))
#define HAS_L3_CCS_READ(i) false
#define STEP_A0 0
#define STEP_B0 1
#define STEP_FOREVER 99
#define COMPUTE_CLASS 4
#define drm_WARN_ON(a,b) ((void)0)
struct info{int gt;bool tuning_thread_rr_after_dep;};static const struct info info={.gt=1};
#define INTEL_INFO(i) (&info)
struct drm_i915_private{int drm;};struct intel_gt{struct drm_i915_private*i915;struct{int uc_index,wb_index;}mocs;};
struct intel_engine_cs{struct drm_i915_private*i915;struct intel_gt*gt;u32 mmio_base;int class;};
struct i915_wa_list{int count;};
static void record(struct i915_wa_list*w,i915_reg_t r,u32 clr,u32 set,bool masked,bool mcr){printf("%x %x %x %u %u\n",r.reg,clr,set,masked,mcr);w->count++;}
#define wa_add(w,r,c,s,v,m) record(w,r,c,s,m,false)
#define wa_mcr_add(w,r,c,s,v,m) record(w,r,c,s,m,true)
#define wa_masked_en(w,r,b) record(w,r,b,REG_MASKED_FIELD_ENABLE(b),true,false)
#define wa_mcr_masked_en(w,r,b) record(w,r,b,REG_MASKED_FIELD_ENABLE(b),true,true)
#define wa_masked_dis(w,r,b) record(w,r,b,(b)<<16,true,false)
#define wa_mcr_masked_dis(w,r,b) record(w,r,b,(b)<<16,true,true)
#define wa_masked_field_set(w,r,m,v) record(w,r,m,((m)<<16)|(v),true,false)
#define wa_mcr_masked_field_set(w,r,m,v) record(w,r,m,((m)<<16)|(v),true,true)
#define wa_write_or(w,r,b) record(w,r,0,b,false,false)
#define wa_mcr_write_or(w,r,b) record(w,r,0,b,false,true)
#define wa_write_clr(w,r,b) record(w,r,b,0,false,false)
#define wa_mcr_write_clr(w,r,b) record(w,r,b,0,false,true)
#define wa_write_clr_set(w,r,c,s) record(w,r,c,s,false,false)
#define wa_mcr_write_clr_set(w,r,c,s) record(w,r,c,s,false,true)
"#;
    let main = r#"
int main(void){struct drm_i915_private i={0};struct intel_gt g={.i915=&i,.mocs={.uc_index=3}};struct intel_engine_cs e={.i915=&i,.gt=&g,.mmio_base=0x2000};struct i915_wa_list w={0};
engine_fake_wa_init(&e,&w);general_render_compute_wa_init(&e,&w);rcs_engine_wa_init(&e,&w);puts("CTX");gen12_ctx_workarounds_init(&e,&w);}
"#;
    let oracle = support::compile(
        &[prefix, &defs, &checks, &functions, main].join("\n"),
        "rcs-wa",
    );
    let out = Command::new(&oracle.executable).output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    println!("{text}");
    let (engine, context) = text.split_once("CTX\n").unwrap();
    let parse = |part: &str| {
        let mut entries = BTreeMap::<u32, (u32, u32)>::new();
        for line in part.lines() {
            let w: Vec<_> = line.split_whitespace().collect();
            let r = u32::from_str_radix(w[0], 16).unwrap();
            let clr = u32::from_str_radix(w[1], 16).unwrap();
            let value = u32::from_str_radix(w[2], 16).unwrap();
            let e = entries.entry(r).or_default();
            e.0 |= clr;
            e.1 |= value;
        }
        entries
    };
    let mut actual = parse(engine);
    for (r, v) in tk_intel_gt::rcs::ENGINE_MASKED
        .into_iter()
        .chain([(0x20e0, 0x40004000)])
    {
        assert_eq!(actual.remove(&r).unwrap().1, v, "engine register{r:x}");
    }
    assert_eq!(
        actual,
        BTreeMap::from([(0xb004, (0x80, 0)), (0x20a0, (0, 0x80000))])
    );
    let mut actual = parse(context);
    for (r, v) in tk_intel_gt::rcs::CONTEXT_MASKED {
        assert_eq!(actual.remove(&r).unwrap().1, v, "context register{r:x}");
    }
    assert_eq!(
        actual,
        BTreeMap::from([(0x6604, (u32::MAX, 0xe0040000)), (0x5584, (0, 0x20))])
    );
}
