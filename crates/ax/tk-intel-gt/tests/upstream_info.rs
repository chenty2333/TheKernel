// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
mod support;
use std::{collections::BTreeMap, process::Command};

use tk_intel_gt::{Error, GtIo, info};
struct Io(BTreeMap<u32, u32>);
impl GtIo for Io {
    fn read(&self, r: u32) -> Result<u32, Error> {
        self.0.get(&r).copied().ok_or(Error::Unavailable(r))
    }
    fn write(&self, _: u32, _: u32) -> Result<(), Error> {
        panic!("discovery must not write")
    }
    fn now_us(&self) -> u64 {
        0
    }
    fn delay_us(&self, _: u32) {}
}
#[test]
#[ignore = "requires local Linux7.2.3/GCC"]
fn n305_fuse_topology_and_clock_match_unmodified_compiled_i915() {
    let root = support::reference();
    let clock = support::read(&root, "gt/intel_gt_clock_utils.c");
    let sseu = support::read(&root, "gt/intel_sseu.c");
    let funcs = [
        support::function(&clock, "static u32 read_reference_ts_freq("),
        support::function(&clock, "static u32 gen11_get_crystal_clock_freq("),
        support::function(&clock, "static u32 gen11_read_clock_frequency("),
        support::function(&sseu, "static void gen11_compute_sseu_info("),
        support::function(&sseu, "static void gen12_sseu_info_init("),
    ]
    .join("\n");
    let regs = support::read(&root, "gt/intel_gt_regs.h");
    let reg = support::read(&root, "i915_reg.h");
    let defs = support::defines(
        &regs,
        &[
            "GEN11_GT_S_ENA_MASK",
            "GEN11_GT_SLICE_ENABLE",
            "GEN12_GT_GEOMETRY_DSS_ENABLE",
            "GEN11_EU_DISABLE",
            "GEN11_EU_DIS_MASK",
            "CTC_MODE",
            "CTC_SOURCE_PARAMETER_MASK",
            "CTC_SOURCE_DIVIDE_LOGIC",
            "RPM_CONFIG0",
            "GEN11_RPM_CONFIG0_CRYSTAL_CLOCK_FREQ_MASK",
            "GEN11_RPM_CONFIG0_CRYSTAL_CLOCK_FREQ_24_MHZ",
            "GEN11_RPM_CONFIG0_CRYSTAL_CLOCK_FREQ_19_2_MHZ",
            "GEN11_RPM_CONFIG0_CRYSTAL_CLOCK_FREQ_38_4_MHZ",
            "GEN11_RPM_CONFIG0_CRYSTAL_CLOCK_FREQ_25_MHZ",
            "GEN10_RPM_CONFIG0_CTC_SHIFT_PARAMETER_MASK",
        ],
    ) + &support::defines(
        &reg,
        &[
            "GEN9_TIMESTAMP_OVERRIDE",
            "GEN9_TIMESTAMP_OVERRIDE_US_COUNTER_DIVIDER_MASK",
            "GEN9_TIMESTAMP_OVERRIDE_US_COUNTER_DIVIDER_SHIFT",
            "GEN9_TIMESTAMP_OVERRIDE_US_COUNTER_DENOMINATOR_MASK",
            "GEN9_TIMESTAMP_OVERRIDE_US_COUNTER_DENOMINATOR_SHIFT",
        ],
    );
    let prefix = r#"
#include <stdint.h>
#include <stdio.h>
typedef uint32_t u32;typedef uint16_t u16;typedef uint8_t u8;
#define _MMIO(r) (r)
#define BIT(n) (1U<<(n))
#define REG_BIT(n) BIT(n)
#define GENMASK(h,l) ((UINT32_MAX>>(31-(h)))&(UINT32_MAX<<(l)))
#define REG_GENMASK(h,l) GENMASK(h,l)
#define REG_FIELD_GET(m,v) (((v)&(m))>>__builtin_ctz(m))
#define REG_FIELD_PREP(m,v) (((v)<<__builtin_ctz(m))&(m))
#define hweight16(v) __builtin_popcount(v)
#define drm_WARN_ON(i,b) ((void)0)
#define MISSING_CASE(v) ((void)0)
struct intel_uncore {u32 ctc,config,override,dss,eu_disable;};
static u32 intel_uncore_read(struct intel_uncore*u,u32 r){switch(r){case 0xa26c:return u->ctc;case 0xd00:return u->config;case 0x44074:return u->override;case 0x9138:return 1;case 0x913c:return u->dss;case 0x9134:return u->eu_disable;default:__builtin_trap();}}
struct sseu_dev_info{u32 max_subslices,max_eus_per_subslice;u8 slice_mask;struct{u8 hsw[1];}subslice_mask;u16 eus[6];u32 eu_per_subslice,eu_total,has_slice_pg;};
struct intel_gt {struct{struct sseu_dev_info sseu;}info;struct intel_uncore*uncore;};
static void intel_sseu_set_info(struct sseu_dev_info*s,int a,int b,int c){s->max_subslices=b;s->max_eus_per_subslice=c;}
static int intel_sseu_has_subslice(struct sseu_dev_info*s,int a,int b){return(s->subslice_mask.hsw[0]>>b)&1;}
static void sseu_set_eus(struct sseu_dev_info*s,int a,int b,u16 e){s->eus[b]=e;}
static int compute_eu_total(struct sseu_dev_info*s){int n=0;for(int i=0;i<6;i++)n+=__builtin_popcount(s->eus[i]);return n;}
"#;
    let main = r#"
int main(void){struct intel_uncore u={0};
for(unsigned config=0;config<64;config++){u.ctc=0;u.config=config;printf("C %u %u\n",config,gen11_read_clock_frequency(&u));}
for(unsigned divisor=0;divisor<1024;divisor++)for(unsigned den=0;den<16;den++){u.ctc=1;u.override=divisor|(den<<12);printf("O %u %u\n",u.override,gen11_read_clock_frequency(&u));}
for(unsigned dss=1;dss<64;dss++)for(unsigned pairs=1;pairs<256;pairs++){u.dss=dss;u.eu_disable=(~pairs)&255;struct intel_gt g={.uncore=&u};gen12_sseu_info_init(&g);printf("T %u %u %u %u\n",dss,pairs,g.info.sseu.eu_per_subslice,g.info.sseu.eu_total);}}
"#;
    let oracle = support::compile(&[prefix, &defs, &funcs, main].join("\n"), "gt-info");
    let out = Command::new(&oracle.executable).output().unwrap();
    assert!(out.status.success());
    for line in String::from_utf8(out.stdout).unwrap().lines() {
        let mut fields = line.split_whitespace();
        let kind = fields.next().unwrap();
        let n: Vec<u32> = fields.map(|v| v.parse().unwrap()).collect();
        let io = Io(match kind {
            "C" => BTreeMap::from([(0xa26c, 0), (0xd00, n[0])]),
            "O" => BTreeMap::from([(0xa26c, 1), (0x44074, n[0])]),
            _ => BTreeMap::from([(0x9138, 1), (0x913c, n[0]), (0x9134, !n[1] & 255)]),
        });
        if kind == "T" {
            let topo = info::Topology::read(&io).unwrap();
            assert_eq!(topo.eus.count_ones(), n[2]);
            assert_eq!(topo.eu_total(), n[3]);
            let wire = topo.wire();
            assert_eq!(wire[16], 1);
            assert_eq!(wire[17], n[0] as u8);
            for dss in 0..6 {
                let mask = u16::from_le_bytes(wire[18 + dss * 2..20 + dss * 2].try_into().unwrap());
                assert_eq!(mask, if n[0] & (1 << dss) != 0 { topo.eus } else { 0 });
            }
        } else if n[1] == 0 {
            assert_eq!(info::clock_frequency(&io), Err(Error::Refused));
        } else {
            assert_eq!(info::clock_frequency(&io), Ok(n[1]));
        }
    }
}
#[test]
fn missing_or_unknown_fuses_are_refused_not_replaced_by_a_product_spec() {
    assert!(info::Topology::read(&Io(BTreeMap::new())).is_err());
    for (slice, dss, disabled) in [(0, 1, 0), (2, 1, 0), (1, 0, 0), (1, 64, 0), (1, 1, 255)] {
        assert!(
            info::Topology::read(&Io(BTreeMap::from([
                (0x9138, slice),
                (0x913c, dss),
                (0x9134, disabled)
            ])))
            .is_err()
        );
    }
}
