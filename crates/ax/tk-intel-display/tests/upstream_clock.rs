// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Arithmetic oracles are compiled from unmodified local MIT i915 functions.
mod support;
use std::{
    io::Write,
    process::{Command, Stdio},
};

use tk_intel_display::{
    cdclk,
    device::Step,
    dpll_mgr::{icl_calc_mg_pll_state, icl_ddi_mg_pll_get_freq},
};
#[test]
#[ignore = "requires local Linux 7.2.3 and GCC; run explicitly"]
fn dkl_hdmi_pll_and_cdclk_table_match_compiled_i915() {
    let root = support::reference();
    let source = support::read(&root, "intel_dpll_mgr.c");
    let functions = [
        "static int icl_mg_pll_find_divisors(",
        "static int icl_calc_mg_pll_state(",
        "static int icl_ddi_mg_pll_get_freq(",
    ]
    .map(|s| support::function(&source, s))
    .join("\n");
    let headers = ["intel_mg_phy_regs.h", "intel_dkl_phy_regs.h"]
        .map(|f| {
            support::read(&root, f)
                .lines()
                .filter(|l| !l.trim_start().starts_with("#include"))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .join("\n");
    let cdclk_source = support::read(&root, "intel_cdclk.c");
    let table = support::function(
        &cdclk_source,
        "static const struct intel_cdclk_vals adlp_cdclk_table[]",
    );
    let selector = support::function(&cdclk_source, "static int bxt_calc_cdclk(");
    // Compile at least one ordinary source macro via the shared extractor, to
    // check the register header is actually this reference and not a stub.
    let display_mask = support::defines(
        &support::read(&root, "intel_display_regs.h"),
        &["HACTIVE_MASK"],
    );
    let prefix = r#"
#include <stdint.h>
#include <stdbool.h>
#include <stdio.h>
#include <errno.h>
typedef uint8_t u8; typedef uint32_t u32; typedef uint64_t u64;
#define REG_GENMASK(h,l) ((uint32_t)((UINT32_MAX >> (31-(h))) & (UINT32_MAX << (l))))
#define REG_FIELD_PREP(m,v) ((v)<<__builtin_ctz(m))
#define REG_BIT(n) (1u<<(n))
#define ARRAY_SIZE(a) (sizeof(a)/sizeof((a)[0]))
#define MISSING_CASE(...) ((void)0)
#define fallthrough __attribute__((fallthrough))
#define DISPLAY_VER(d) 13
#define to_intel_display(s) ((s)->display)
#define INTEL_OUTPUT_HDMI 1
#define intel_crtc_has_type(s,t) ((s)->hdmi)
#define do_div(n,d) ((n)/=(d))
#define mul_u32_u32(a,b) ((u64)(a)*(b))
#define div_u64(n,d) ((n)/(d))
#define DIV_ROUND_UP_ULL(n,d) (((n)+(d)-1)/(d))
#define drm_WARN(...) ((void)0)
struct intel_cdclk_vals { int refclk,cdclk,ratio,waveform; };
struct intel_display { struct {struct {int nssc;} ref_clks;} dpll; struct {bool override_afc_startup; u8 override_afc_startup_val;} vbt; struct {const struct intel_cdclk_vals *table; struct {int ref;} hw; int max_cdclk_freq;} cdclk; };
struct intel_crtc_state { struct intel_display *display; int port_clock; bool hdmi; };
struct icl_dpll_hw_state { u32 mg_refclkin_ctl,mg_clktop2_coreclkctl1,mg_clktop2_hsclkctl,mg_pll_div0,mg_pll_div1,mg_pll_ssc,mg_pll_bias,mg_pll_tdc_coldst_bias,mg_pll_lf,mg_pll_frac_lock,mg_pll_tdc_coldst_bias_mask,mg_pll_bias_mask; };
struct intel_dpll_hw_state { struct icl_dpll_hw_state icl; };
struct intel_dpll { int unused; };
"#;
    let main = r#"
int main(void) {
    char kind; int clock,ref,afc;
    while(scanf(" %c %d %d %d",&kind,&clock,&ref,&afc)==4) {
        struct intel_display d={0};
        if(kind=='p') {
            d.dpll.ref_clks.nssc=ref; d.vbt.override_afc_startup=afc>=0; d.vbt.override_afc_startup_val=afc;
            struct intel_crtc_state c={.display=&d,.port_clock=clock,.hdmi=true};
            struct intel_dpll_hw_state s={0}; int err=icl_calc_mg_pll_state(&c,&s);
            if(err) { puts("ERR"); continue; }
            u32 dco=0; struct icl_dpll_hw_state scratch={0};
            icl_mg_pll_find_divisors(clock,false,false,&dco,&scratch,true);
            struct icl_dpll_hw_state *h=&s.icl;
            printf("%u %u %u %u %u %u %u %u %u %u\n",dco,h->mg_refclkin_ctl,h->mg_clktop2_coreclkctl1,h->mg_clktop2_hsclkctl,h->mg_pll_div0,h->mg_pll_div1,h->mg_pll_ssc,h->mg_pll_bias,h->mg_pll_tdc_coldst_bias,icl_ddi_mg_pll_get_freq(&d,0,&s));
        } else {
            d.cdclk.table=adlp_cdclk_table; d.cdclk.hw.ref=ref; d.cdclk.max_cdclk_freq=afc;
            printf("%d\n",bxt_calc_cdclk(&d,clock));
        }
    }
}
"#;
    let oracle = support::compile(
        &format!("{prefix}\n{display_mask}\n{headers}\n{functions}\n{table};\n{selector}\n{main}"),
        "clock",
    );
    let clocks = [
        10000, 25175, 27000, 40000, 65000, 74250, 74176, 108000, 119000, 148500, 148352, 162000,
        297000, 296703, 340000, 594000,
    ];
    let mut expected = Vec::<Option<Vec<u32>>>::new();
    let mut input = String::new();
    for refclk in [19200, 24000, 38400] {
        for clock in clocks {
            for afc in [None, Some(0), Some(3), Some(7)] {
                input.push_str(&format!(
                    "p {clock} {refclk} {}\n",
                    afc.map(i32::from).unwrap_or(-1)
                ));
                expected.push(icl_calc_mg_pll_state(clock, refclk, afc).ok().map(|s| {
                    vec![
                        s.dco_khz,
                        s.refclkin_ctl,
                        s.coreclkctl1,
                        s.hsclkctl,
                        s.div0,
                        s.div1,
                        s.ssc,
                        s.bias,
                        s.tdc_coldst_bias,
                        icl_ddi_mg_pll_get_freq(&s, refclk).unwrap(),
                    ]
                }));
            }
        }
    }
    let pll_cases = expected.len();
    for refclk in [19200, 24000, 38400] {
        let max = if refclk == 24000 { 648000 } else { 652800 };
        for min in [
            1, 74250, 148500, 172800, 176000, 179200, 192000, 192001, 307200, 312000, 500000,
            552000, 556800, 648000,
        ] {
            input.push_str(&format!("c {min} {refclk} {max}\n"));
            expected.push(Some(vec![
                cdclk::bxt_calc_cdclk(Step::D0, refclk, min, max)
                    .unwrap()
                    .cdclk_khz,
            ]));
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
    assert_eq!(output.lines().count(), expected.len());
    for (i, (line, expected)) in output.lines().zip(expected.iter()).enumerate() {
        let actual = if line == "ERR" {
            None
        } else {
            Some(
                line.split_whitespace()
                    .map(|n| n.parse::<u32>().unwrap())
                    .collect::<Vec<_>>(),
            )
        };
        assert_eq!(&actual, expected, "upstream clock case {i}");
    }
    println!(
        "{pll_cases} TC HDMI PLL/refclk/AFC cases, {} CDCLK selections match compiled i915",
        expected.len() - pll_cases
    );
}
