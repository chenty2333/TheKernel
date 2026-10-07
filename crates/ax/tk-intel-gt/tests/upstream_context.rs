// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
mod support;
use std::process::Command;

use tk_intel_gt::{lrc, ppgtt};
#[test]
#[ignore = "requires local Linux 7.2.3 and GCC; run explicitly"]
fn private_pte_lrc_and_indirect_workarounds_match_compiled_i915() {
    let root = support::reference();
    let src = support::read(&root, "gt/intel_lrc.c");
    let vm = support::read(&root, "gt/gen8_ppgtt.c");
    let flush = support::read(&root, "gt/gen8_engine_cs.c");
    let mut definitions = String::new();
    for file in [
        "gt/intel_gpu_commands.h",
        "gt/intel_lrc_reg.h",
        "gt/intel_lrc.h",
        "gt/intel_engine_regs.h",
        "gt/intel_gt_regs.h",
        "gt/intel_gtt.h",
    ] {
        let text = support::read(&root, file);
        let names: Vec<_> = text
            .lines()
            .filter_map(|l| l.trim_start().strip_prefix("#define"))
            .map(|l| l.trim_start().split([' ', '\t', '(']).next().unwrap())
            .filter(|n| *n != "GTT_TRACE")
            .collect();
        definitions.push_str(&support::defines(&text, &names));
    }
    let prefix = r#"
#include <stdint.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
typedef uint32_t u32;typedef uint64_t u64;typedef uint8_t u8;typedef uint64_t dma_addr_t;typedef uint64_t gen8_pte_t;
typedef struct{u32 reg;} i915_reg_t;
#define BIT(n) (1U<<(n))
#define BIT_ULL(n) (1ULL<<(n))
#define REG_BIT(n) BIT(n)
#define REG_GENMASK(h,l) ((UINT32_MAX>>(31-(h)))&(UINT32_MAX<<(l)))
#define REG_MASKED_FIELD_ENABLE(n) ((n)|((n)<<16))
#define REG_MASKED_FIELD_DISABLE(n) ((n)<<16)
#define BUILD_BUG_ON_ZERO(n) 0
#define GEM_BUG_ON(n) do{if(n)abort();}while(0)
#define GRAPHICS_VER(i) 12
#define GRAPHICS_VER_FULL(i) 1200
#define IP_VER(a,b) ((a)*100+(b))
#define _MMIO(n) ((i915_reg_t){n})
#define INVALID_MMIO_REG _MMIO(0)
#define offset_in_page(p) ((uintptr_t)(p)&4095)
#define HAS_FLAT_CCS(i) 0
#define upper_32_bits(n) ((u32)((n)>>32))
#define lower_32_bits(n) ((u32)(n))
#define IS_ALIGNED(n,a) (((n)&((a)-1))==0)
#define PAGE_SIZE 4096
#define CACHELINE_BYTES 64
#define _PAGE_PWT (1ULL<<3)
#define _PAGE_PCD (1ULL<<4)
#define _PAGE_PAT (1ULL<<7)
#define unlikely(n) (n)
enum i915_cache_level{I915_CACHE_NONE};
enum intel_engine_id{RCS0,BCS0,VCS0,VCS2,VECS0,CCS0};
#define RENDER_CLASS 0
#define COPY_ENGINE_CLASS 1
struct intel_uncore{u32 gsi_offset;};struct intel_gt{struct intel_uncore*uncore;};
struct intel_engine_cs{void*i915;struct intel_gt*gt;u32 mmio_base;int class,id;};
struct intel_context{struct intel_engine_cs*engine;u32 state;struct{struct{u32 last;}runtime;}stats;};
static bool ctx_needs_runalone(const struct intel_context*c){return false;}
static bool i915_mmio_reg_valid(i915_reg_t r){return r.reg!=0;}
static u32 i915_mmio_reg_offset(i915_reg_t r){return r.reg;}
static u32 i915_ggtt_offset(u32 state){return state;}
static u32 lrc_indirect_bb(const struct intel_context*c){return c->state+8192;}
"#;
    let functions = [
        support::function(&src, "static void set_offsets("),
        support::function(&src, "static const u8 gen12_xcs_offsets[]") + ";",
        support::function(&src, "static int lrc_ring_mi_mode("),
        support::function(&src, "static int lrc_ring_bb_offset("),
        support::function(&src, "static int lrc_ring_gpr0("),
        support::function(&src, "static int lrc_ring_wa_bb_per_ctx("),
        support::function(&src, "static int lrc_ring_indirect_ptr("),
        support::function(&src, "static int lrc_ring_indirect_offset("),
        support::function(&src, "static u32\nlrc_ring_indirect_offset_default("),
        support::function(&src, "static void\nlrc_setup_bb_per_ctx("),
        support::function(&src, "static void\nlrc_setup_indirect_ctx("),
        support::function(&src, "static void init_common_regs("),
        support::function(&src, "static void __reset_stop_ring("),
        support::function(&src, "static u32 *\ngen12_emit_timestamp_wa("),
        support::function(&src, "static u32 *\ngen12_emit_restore_scratch("),
        support::function(&src, "static u32 *setup_predicate_disable_wa("),
        support::function(&flush, "static i915_reg_t gen12_get_aux_inv_reg("),
        support::function(&flush, "static bool gen12_needs_ccs_aux_inv("),
        support::function(&flush, "u32 *gen12_emit_aux_table_inv("),
        support::function(&vm, "static u64 gen8_pde_encode("),
        support::function(&vm, "static u64 gen12_pte_encode("),
    ]
    .join("\n");
    let main = r#"
int main(void){u32 regs[1024]={0};_Alignas(4096) u32 wa[1024]={0};struct intel_uncore u={0};struct intel_gt gt={.uncore=&u};
struct intel_engine_cs engine={.gt=&gt,.mmio_base=0x22000,.class=COPY_ENGINE_CLASS,.id=BCS0};
struct intel_context ce={.engine=&engine,.state=0x200000};
set_offsets(regs,gen12_xcs_offsets,&engine,true);init_common_regs(regs,&ce,&engine,true);__reset_stop_ring(regs,&engine);
regs[CTX_RING_HEAD]=0;regs[CTX_RING_TAIL]=128;regs[CTX_RING_START]=0x300000;regs[CTX_RING_CTL]=1;
regs[CTX_PDP0_UDW]=1;regs[CTX_PDP0_LDW]=0x23456000;
u32*cs=gen12_emit_timestamp_wa(&ce,wa);cs=gen12_emit_restore_scratch(&ce,cs);cs=gen12_emit_aux_table_inv(&engine,cs);
while((cs-wa)%16)*cs++=0;lrc_setup_indirect_ctx(regs,&engine,0x202000,(cs-wa)*4);
lrc_setup_bb_per_ctx(regs,&engine,0x203000);setup_predicate_disable_wa(&ce,wa+512);
for(int i=0;i<1024;i++)printf("%u ",regs[i]);puts("");for(int i=0;i<1024;i++)printf("%u ",wa[i]);puts("");
for(int pat=0;pat<8;pat++)for(int ro=0;ro<2;ro++)printf("%llu ",(unsigned long long)gen12_pte_encode(0x800000,pat,ro?PTE_READ_ONLY:0));
printf("%llu\n",(unsigned long long)gen8_pde_encode(0x800000,I915_CACHE_NONE));}
"#;
    let oracle = support::compile(
        &[prefix, &definitions, &functions, main].join("\n"),
        "gt-context",
    );
    let output = Command::new(&oracle.executable).output().unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines.len(), 3);
    let (mut regs, mut wa, mut per) = ([0; 1024], [0; 1024], [0; 1024]);
    lrc::build(
        &mut regs,
        &mut wa,
        &mut per,
        0x200000,
        0x300000,
        128,
        0x123456000,
    )
    .unwrap();
    for (line, expected) in lines[..2].iter().zip([regs, wa]) {
        let actual: Vec<u32> = line
            .split_whitespace()
            .map(|n| n.parse().unwrap())
            .collect();
        assert_eq!(actual.len(), expected.len());
        for (i, (a, e)) in actual.iter().zip(expected).enumerate() {
            assert_eq!(*a, e, "image word {i}");
        }
    }
    let actual: Vec<u64> = lines[2]
        .split_whitespace()
        .map(|n| n.parse().unwrap())
        .collect();
    let mut expected = Vec::new();
    for pat in 0..8 {
        for ro in [false, true] {
            expected.push(ppgtt::pte(0x800000, pat, !ro).unwrap());
        }
    }
    expected.push(ppgtt::pde(0x800000).unwrap());
    assert_eq!(actual, expected);
    println!(
        "Gen12 BCS full register/indirect images and system PTE/PDE fields match compiled i915; \
         no GPU execution"
    );
}
