// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Independent local C oracle for DKL accesses and PLL readout, including traces.
mod dkl_support;
mod support;
use std::{
    io::Write,
    process::{Command, Stdio},
};

use dkl_support::Model;
use tk_intel_display::{dkl_phy::*, dpll_mgr::dkl_pll_get_hw_state};

#[test]
#[ignore = "requires local Linux 7.2.3 and GCC; run explicitly"]
fn dkl_operations_and_pll_readout_match_compiled_i915() {
    let root = support::reference();
    let header = support::read(&root, "intel_dkl_phy_regs.h");
    let names: Vec<_> = header
        .lines()
        .filter_map(|l| l.trim_start().strip_prefix("#define "))
        .map(|l| l.trim_start().split([' ', '\t', '(']).next().unwrap())
        .collect();
    let dkl_defines = support::defines(&header, &names);
    let mg_defines = support::defines(
        &support::read(&root, "intel_mg_phy_regs.h"),
        &[
            "MG_REFCLKIN_CTL_OD_2_MUX_MASK",
            "MG_CLKTOP2_HSCLKCTL_TLINEDRV_CLKSEL_MASK",
            "MG_CLKTOP2_HSCLKCTL_CORE_INPUTSEL_MASK",
            "MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_MASK",
            "MG_CLKTOP2_HSCLKCTL_DSDIV_RATIO_MASK",
            "MG_CLKTOP2_CORECLKCTL1_A_DIVRATIO_MASK",
        ],
    );
    let enable_defines = support::defines(
        &support::read(&root, "intel_display_regs.h"),
        &[
            "PORTTC1_PLL_ENABLE",
            "PORTTC2_PLL_ENABLE",
            "ADLP_PORTTC_PLL_ENABLE",
            "PLL_ENABLE",
        ],
    );
    let dkl_source = support::read(&root, "intel_dkl_phy.c");
    let functions = [
        "static void\ndkl_phy_set_hip_idx(",
        "u32\nintel_dkl_phy_read(",
        "void\nintel_dkl_phy_write(",
        "void\nintel_dkl_phy_rmw(",
        "void\nintel_dkl_phy_posting_read(",
    ]
    .into_iter()
    .map(|s| support::function(&dkl_source, s))
    .collect::<Vec<_>>()
    .join("\n");
    let pll = support::function(
        &support::read(&root, "intel_dpll_mgr.c"),
        "static bool dkl_pll_get_hw_state(",
    );
    let prefix = r#"
#include <stdint.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
typedef uint32_t u32;
#define REG_BIT(b) (1U<<(b))
#define REG_GENMASK(h,l) ((uint32_t)((UINT32_MAX>>(31-(h))) & (UINT32_MAX<<(l))))
#define _PORT(p,a,b) ((a)+(p)*((b)-(a)))
#define _MMIO(r) (r)
#define _MMIO_PORT(p,a,b) _PORT(p,a,b)
enum tc_port {TC_PORT_1,TC_PORT_2,TC_PORT_3,TC_PORT_4};
#define I915_MAX_TC_PORTS 4
#define drm_WARN_ON(d,b) (b)
struct intel_dkl_phy_reg { u32 reg:24; u32 bank_idx:4; };
struct intel_display { int drm; struct {int phy_lock;} dkl; struct {bool override_afc_startup;} vbt; };
struct icl_dpll_hw_state { u32 mg_refclkin_ctl,mg_clktop2_hsclkctl,mg_clktop2_coreclkctl1,mg_pll_div0,mg_pll_div1,mg_pll_ssc,mg_pll_bias,mg_pll_tdc_coldst_bias; };
struct intel_dpll_hw_state {struct icl_dpll_hw_state icl;};
enum intel_dpll_id {DPLL_ID_TC1,DPLL_ID_TC2,DPLL_ID_TC3,DPLL_ID_TC4};
struct intel_dpll_info {enum intel_dpll_id id;};
struct intel_dpll {struct intel_dpll_info *info;};
struct ref_tracker {int unused;};
#define POWER_DOMAIN_DISPLAY_CORE 0
static u32 seed,selector; static bool locked,power; static u32 trace[128][3]; static int count;
static void spin_lock(int *l) {if(locked) abort(); locked=true;}
static void spin_unlock(int *l) {if(!locked) abort(); locked=false;}
static void record(u32 op,u32 reg,u32 val) {if(count>=128) abort(); trace[count][0]=op;trace[count][1]=reg;trace[count++][2]=val;}
static u32 intel_de_read(struct intel_display *d,u32 reg) {
    u32 val;
    if(reg==0x1010a0) val=selector;
    else if(reg>=0x46038 && reg<=0x46050) {if(!power) abort(); val=0xc8000000;}
    else {if(!locked) abort(); u32 p=(reg-0x168000)/0x1000; u32 bank=(selector>>(8*p))&15;
        val=((reg*0x10001U) ^ (bank<<24) ^ seed) | 0x201;}
    record(0,reg,val); return val;
}
static void intel_de_write(struct intel_display *d,u32 reg,u32 val) {
    if(!locked) abort(); if(reg==0x1010a0) selector=val;record(1,reg,val);
}
static void intel_de_rmw(struct intel_display *d,u32 reg,u32 clear,u32 set) {
    u32 old=intel_de_read(d,reg); intel_de_write(d,reg,(old&~clear)|set);
}
static void intel_de_posting_read(struct intel_display *d,u32 reg) {intel_de_read(d,reg);}
static struct ref_tracker ref;
static struct ref_tracker *intel_display_power_get_if_enabled(struct intel_display *d,int domain) {power=true;return &ref;}
static void intel_display_power_put(struct intel_display *d,int domain,struct ref_tracker *r) {power=false;}
static enum tc_port icl_pll_id_to_tc_port(enum intel_dpll_id id) {return (enum tc_port)id;}
"#;
    let main = r#"
int main(void) {
    int port,bank,afc;
    while(scanf("%u %d %d %d",&seed,&port,&bank,&afc)==4) {
        struct intel_display d={.vbt={.override_afc_startup=afc}};
        struct intel_dpll_info info={.id=port};struct intel_dpll pll={.info=&info};
        struct intel_dpll_hw_state state={0}; count=0; selector=0x03020100;
        if(bank>=0) {
            struct intel_dkl_phy_reg reg=_DKL_REG(port,bank*0x1000+0x2c0);
            intel_dkl_phy_read(&d,reg);intel_dkl_phy_write(&d,reg,42);
            intel_dkl_phy_rmw(&d,reg,0,0);intel_dkl_phy_posting_read(&d,reg);
        } else {
            if(!dkl_pll_get_hw_state(&d,&pll,&state)) abort();
            struct icl_dpll_hw_state *s=&state.icl;
            printf("%u %u %u %u %u %u %u %u ",s->mg_refclkin_ctl,s->mg_clktop2_hsclkctl,s->mg_clktop2_coreclkctl1,s->mg_pll_div0,s->mg_pll_div1,s->mg_pll_ssc,s->mg_pll_bias,s->mg_pll_tdc_coldst_bias);
        }
        for(int i=0;i<count;i++) printf("%u %u %u ",trace[i][0],trace[i][1],trace[i][2]); puts("");
        if(locked || power) abort();
    }
}
"#;
    let routing = "static u32 intel_tc_pll_enable_reg(struct intel_display *d, struct intel_dpll \
                   *pll) {return ADLP_PORTTC_PLL_ENABLE(pll->info->id);}";
    let code = [
        prefix,
        &dkl_defines,
        &mg_defines,
        &enable_defines,
        routing,
        &functions,
        &pll,
        main,
    ]
    .join("\n");
    let oracle = support::compile(&code, "dkl");
    let mut cases = Vec::new();
    let mut input = String::new();
    for port in [TcPort::Tc1, TcPort::Tc2, TcPort::Tc3, TcPort::Tc4] {
        for bank in -1..16 {
            for (seed, afc) in [(0x52345678, false), (0xabcdef12, true), (0xffffffff, false)] {
                cases.push((port, bank, seed, afc));
                input.push_str(&format!(
                    "{seed} {} {bank} {}\n",
                    port.index(),
                    u8::from(afc)
                ));
            }
        }
    }
    let mut child = Command::new(&oracle.executable)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let output = String::from_utf8(output.stdout).unwrap();
    assert_eq!(output.lines().count(), cases.len());
    for (line, (port, bank, seed, afc)) in output.lines().zip(cases) {
        let io = Model::new(seed);
        let mut expected = Vec::new();
        if bank >= 0 {
            let reg = DklRegister::new(port, bank as u32 * 0x1000 + 0x2c0).unwrap();
            intel_dkl_phy_read(&io, reg).unwrap();
            intel_dkl_phy_write(&io, reg, 42).unwrap();
            intel_dkl_phy_rmw(&io, reg, 0, 0).unwrap();
            intel_dkl_phy_posting_read(&io, reg).unwrap();
            for &(op, r, v) in io.trace.borrow().iter() {
                expected.extend([op, r, v]);
            }
        } else {
            let s = dkl_pll_get_hw_state(&io, port, 24000, afc)
                .unwrap()
                .unwrap()
                .state;
            expected.extend([
                s.refclkin_ctl,
                s.hsclkctl,
                s.coreclkctl1,
                s.div0,
                s.div1,
                s.ssc,
                s.bias,
                s.tdc_coldst_bias,
            ]);
            let t = io.trace.borrow();
            // Only deliberate divergence: one locked before-image read and
            // verified selector restore enclosing the unchanged upstream reads.
            for (i, &(op, r, v)) in t.iter().enumerate() {
                if i != 1 && i < t.len() - 2 {
                    expected.extend([op, r, v]);
                }
            }
            assert_eq!(io.selector.get(), 0x03020100);
        }
        let actual: Vec<u32> = line
            .split_whitespace()
            .map(|n| n.parse().unwrap())
            .collect();
        assert_eq!(
            actual, expected,
            "port={port:?} bank={bank} seed={seed:x} afc={afc}"
        );
    }
    println!("192 four-operation DKL traces and 12 PLL readouts match local compiled i915");
}
