// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
mod support;
use std::process::Command;

use tk_intel_gt::bcs;
#[test]
#[ignore = "requires local Linux 7.2.3 and GCC; run explicitly"]
fn linear_batch_and_polling_ring_match_compiled_i915() {
    let root = support::reference();
    let blit = support::read(&root, "gem/selftests/i915_gem_client_blt.c");
    let cs = support::read(&root, "gt/gen8_engine_cs.c");
    let helpers = support::read(&root, "gt/gen8_engine_cs.h");
    let mut definitions = String::new();
    for file in [
        "gt/intel_gpu_commands.h",
        "gt/intel_engine_regs.h",
        "gt/intel_gt_regs.h",
        "gt/intel_lrc.h",
    ] {
        let text = support::read(&root, file);
        let names: Vec<_> = text
            .lines()
            .filter_map(|l| l.trim_start().strip_prefix("#define"))
            .map(|l| l.trim_start().split([' ', '\t', '(']).next().unwrap())
            .collect();
        definitions.push_str(&support::defines(&text, &names));
    }
    let prefix = r#"
#include <stdint.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
typedef uint32_t u32;typedef uint64_t u64;typedef struct{u32 reg;}i915_reg_t;
#define _MMIO(n) ((i915_reg_t){n})
#define INVALID_MMIO_REG _MMIO(0)
#define BIT(n) (1U<<(n))
#define REG_BIT(n) BIT(n)
#define GENMASK(h,l) ((UINT32_MAX>>(31-(h)))&(UINT32_MAX<<(l)))
#define REG_GENMASK(h,l) GENMASK(h,l)
#define REG_FIELD_PREP(mask,v) (((v)<<__builtin_ctz(mask))&(mask))
#define GEM_BUG_ON(n) do{if(n)abort();}while(0)
#define GRAPHICS_VER(i) 12
#define GRAPHICS_VER_FULL(i) 1200
#define IP_VER(a,b) ((a)*100+(b))
#define HAS_FLAT_CCS(i) 0
#define lower_32_bits(v) ((u32)(v))
#define upper_32_bits(v) ((u32)((v)>>32))
#define IS_ALIGNED(n,a) (((n)&((a)-1))==0)
#define I915_MAP_WC 0
#define I915_DISPATCH_SECURE 1
#define IS_ERR(p) false
#define PTR_ERR(p) -1
#define EMIT_INVALIDATE 1
#define EMIT_FLUSH 2
#define COPY_ENGINE_CLASS 1
#define VIDEO_DECODE_CLASS 2
#define I915_GTT_PAGE_SIZE 4096
struct intel_uncore{u32 gsi_offset;};struct intel_gt{struct intel_uncore*uncore;struct{int uc_index;}mocs;struct{int unused;}uc;};
enum engine_id{RCS0,BCS0,VCS0,VCS2,VECS0,CCS0};
struct intel_engine_cs{u32 mmio_base;struct intel_gt*gt;int class,id;void*i915;};
struct intel_context{struct intel_engine_cs*engine;};
struct tiled_blits{struct intel_context*ce;int width,height;};
struct i915_vma{u64 address;};struct blit_buffer{struct i915_vma*vma;int tiling;};
#define CLIENT_TILING_Y 2
#define CLIENT_TILING_X 1
struct drm_i915_gem_object{struct{void*dev;}base;u32 data[64];};
struct intel_ring{int unused;};struct i915_request{struct intel_engine_cs*engine;struct intel_ring*ring;int tail,wa_tail;struct{u32 seqno;}fence;};
static bool fast_blit_ok(struct blit_buffer*b){return b->tiling==0;}
static void*i915_gem_object_pin_map_unlocked(struct drm_i915_gem_object*b,int type){return b->data;}
static void i915_gem_object_flush_map(struct drm_i915_gem_object*b){}
static void i915_gem_object_unpin_map(struct drm_i915_gem_object*b){}
static u64 i915_vma_offset(struct i915_vma*v){return v->address;}
static u32 i915_mmio_reg_offset(i915_reg_t r){return r.reg;}
static bool i915_mmio_reg_valid(i915_reg_t r){return r.reg!=0;}
static u32 ring_data[128];static int used;
static u32*intel_ring_begin(struct i915_request*r,int n){return ring_data+used;}
static void intel_ring_advance(struct i915_request*r,u32*p){used=p-ring_data;}
static int intel_ring_offset(struct i915_request*r,u32*p){return (p-ring_data)*4;}
static void assert_ring_tail_valid(struct intel_ring*r,int tail){if(tail%8)abort();}
static void assert_request_valid(struct i915_request*r){}
static bool intel_engine_has_semaphores(struct intel_engine_cs*e){return false;}
static bool intel_uc_uses_guc_submission(void*u){return false;}
static bool intel_engine_uses_wa_hold_switchout(struct intel_engine_cs*e){return false;}
static u32*gen12_emit_preempt_busywait(struct i915_request*r,u32*p){abort();}
static u32*hold_switchout_emit_wa_busywait(struct i915_request*r,u32*p){abort();}
static u32 hwsp_offset(struct i915_request*r){return 0x2000d0;}
"#;
    let functions = [
        support::function(&blit, "static int prepare_blit("),
        support::function(&cs, "static u32 preparser_disable("),
        support::function(&cs, "static i915_reg_t gen12_get_aux_inv_reg("),
        support::function(&cs, "static bool gen12_needs_ccs_aux_inv("),
        support::function(&cs, "u32 *gen12_emit_aux_table_inv("),
        support::function(&cs, "int gen12_emit_flush_xcs("),
        support::function(&cs, "int gen8_emit_bb_start_noarb("),
        support::function(&helpers, "static inline u32 *\n__gen8_emit_flush_dw("),
        support::function(&helpers, "static inline u32 *\ngen8_emit_ggtt_write("),
        support::function(&cs, "static u32 *gen8_emit_wa_tail("),
        support::function(&cs, "static u32 *emit_xcs_breadcrumb("),
        support::function(
            &cs,
            "static __always_inline u32*\ngen12_emit_fini_breadcrumb_tail(",
        ),
        support::function(&cs, "u32 *gen12_emit_fini_breadcrumb_xcs("),
    ]
    .join("\n");
    let main = r#"
int main(void){struct intel_uncore uncore={0};struct intel_gt gt={.uncore=&uncore,.mocs={.uc_index=3}};
struct intel_engine_cs engine={.gt=&gt,.mmio_base=0x22000,.class=COPY_ENGINE_CLASS,.id=BCS0};
struct intel_context ce={.engine=&engine};struct tiled_blits t={.ce=&ce,.width=64,.height=64};
struct i915_vma sv={.address=0x11000},dv={.address=0x21000};struct blit_buffer s={.vma=&sv},d={.vma=&dv};
struct drm_i915_gem_object object={0};prepare_blit(&t,&d,&s,&object);
for(int i=0;i<14;i++)printf("%u ",object.data[i]);puts("");
struct intel_ring ring={0};struct i915_request rq={.engine=&engine,.ring=&ring,.fence={.seqno=1}};
gen12_emit_flush_xcs(&rq,EMIT_INVALIDATE);gen8_emit_bb_start_noarb(&rq,0x30000,4096,0);
u32*p=gen12_emit_fini_breadcrumb_xcs(&rq,ring_data+used);for(int i=0;i<p-ring_data;i++)printf("%u ",ring_data[i]);puts("");}
"#;
    let oracle = support::compile(
        &[prefix, &definitions, &functions, main].join("\n"),
        "gt-bcs",
    );
    let out = Command::new(&oracle.executable).output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines.len(), 2);
    let batch = bcs::batch(bcs::Copy {
        source: 0x11000,
        destination: 0x21000,
        source_bytes: 16384,
        destination_bytes: 16384,
        width: 64,
        height: 64,
        pitch: 256,
    })
    .unwrap();
    let actual: Vec<u32> = lines[0]
        .split_whitespace()
        .map(|s| s.parse().unwrap())
        .collect();
    assert_eq!(actual, batch);
    let mut ring = [0; 32];
    let count = bcs::ring(&mut ring, 0x30000, 0x200000, 1).unwrap();
    let actual: Vec<u32> = lines[1]
        .split_whitespace()
        .map(|s| s.parse().unwrap())
        .collect();
    assert_eq!(actual.len(), count);
    for (i, (a, e)) in actual.iter().zip(ring).enumerate() {
        assert_eq!(*a, e, "ring word{i}");
    }
    println!(
        "kernel-owned linear fast-copy batch and no-preempt flush/breadcrumb/tail match compiled \
         i915; not GPU output"
    );
}
