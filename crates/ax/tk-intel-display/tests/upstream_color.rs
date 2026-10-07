// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
mod color_support;
mod support;
use std::{
    io::Write,
    process::{Command, Stdio},
};

use color_support::Model;
use tk_intel_display::{color::*, display::Pipe};
fn all_defines(source: &str) -> String {
    let names: Vec<_> = source
        .lines()
        .filter_map(|l| l.trim_start().strip_prefix("#define"))
        .map(|l| l.trim_start().split([' ', '\t', '(']).next().unwrap())
        .collect();
    support::defines(source, &names)
}
#[test]
#[ignore = "requires local Linux 7.2.3 and GCC; run explicitly"]
fn color_csc_luts_and_traces_match_compiled_i915() {
    let root = support::reference();
    let regs = all_defines(&support::read(&root, "intel_color_regs.h"));
    let source = support::read(&root, "intel_color.c");
    let signatures = [
        "static u32 intel_color_lut_pack(",
        "static void i9xx_lut_8_pack(",
        "static void ilk_lut_10_pack(",
        "static void ilk_lut_12p4_pack(",
        "static void glk_degamma_lut_pack(",
        "static void ilk_read_pipe_csc(",
        "static void icl_read_output_csc(",
        "static void icl_read_csc(",
        "static u32 hsw_read_gamma_mode(",
        "static u32 ilk_read_csc_mode(",
        "static void skl_get_config(",
        "static bool icl_has_post_csc_lut(",
        "static bool icl_has_pre_csc_lut(",
        "static int ivb_lut_10_size(",
        "static struct drm_property_blob *ilk_read_lut_8(",
        "static struct drm_property_blob *bdw_read_lut_10(",
        "static struct drm_property_blob *glk_read_degamma_lut(",
        "static struct drm_property_blob *\nicl_read_lut_multi_segment(",
        "static void icl_read_luts(",
    ];
    let functions = signatures
        .into_iter()
        .map(|s| support::function(&source, s))
        .collect::<Vec<_>>()
        .join("\n");
    let prefix = r#"
#include <stdint.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
typedef uint32_t u32;typedef uint64_t u64;typedef uint16_t u16;
#define REG_BIT(b) (1U<<(b))
#define REG_GENMASK(h,l) ((uint32_t)((UINT32_MAX>>(31-(h))) & (UINT32_MAX<<(l))))
#define GENMASK(h,l) REG_GENMASK(h,l)
#define REG_FIELD_GET(mask,v) (((v)&(mask))>>__builtin_ctz(mask))
#define REG_FIELD_PREP(mask,v) (((v)<<__builtin_ctz(mask))&(mask))
#define _PIPE(p,a,b) ((a)+(p)*((b)-(a)))
#define _MMIO(r) (r)
#define _MMIO_PIPE(p,a,b) _PIPE(p,a,b)
#define DISPLAY_VER(d) 13
#define DIV_ROUND_CLOSEST(v,d) (((v)+(d)/2)/(d))
#define DIV_ROUND_CLOSEST_ULL(v,d) DIV_ROUND_CLOSEST(v,d)
#define mul_u32_u32(a,b) ((u64)(a)*(b))
#define min(a,b) ((a)<(b)?(a):(b))
#define IS_ERR(p) (!(p))
#define LEGACY_LUT_LENGTH 256
#define MISSING_CASE(...) abort()
struct drm_color_lut {u16 red,green,blue,reserved;};
struct drm_property_blob {void *data;};
struct intel_csc_matrix {u16 coeff[9],preoff[3],postoff[3];};
struct intel_display {int drm;};
static struct intel_display model_display;
#define to_intel_display(p) (&model_display)
enum pipe {PIPE_A,PIPE_B,PIPE_C,PIPE_D};
struct intel_crtc {enum pipe pipe;};
#define to_intel_crtc(p) (p)
struct intel_crtc_state {
    struct {struct intel_crtc *crtc;} uapi;
    struct {u32 background_color;} hw;
    u32 gamma_mode,csc_mode,c8_planes;bool gamma_enable,csc_enable;
    struct intel_csc_matrix csc,output_csc;
    struct drm_property_blob *pre_csc_lut,*post_csc_lut;
};
struct display_info {struct {int degamma_lut_size,gamma_lut_size;} color;};
static struct display_info model_info={.color={.degamma_lut_size=129,.gamma_lut_size=1024}};
#define DISPLAY_INFO(d) (&model_info)
static struct drm_property_blob *drm_property_create_blob(int dev,size_t size,void *unused) {
    struct drm_property_blob *b=malloc(sizeof(*b)); if(!b) abort();b->data=calloc(1,size);if(!b->data) abort();return b;
}
static void mtl_degamma_lut_pack(struct drm_color_lut *entry,u32 val) {abort();}
static u32 seed,mode,csc,bottom,ticks,selector[3],trace[1400][3];static int pipe_idx,count;
static int select_id(u32 r) {
    u32 s=pipe_idx*0x800;
    if(r==0x4a484+s) return 0;if(r==0x4a400+s) return 1;if(r==0x4a408+s) return 2;return -1;
}
static void record(u32 op,u32 r,u32 v) {
    if(count>=1400) abort();trace[count][0]=op;trace[count][1]=r;trace[count++][2]=v;
}
static u32 intel_de_read(struct intel_display *d,u32 r) {
    u32 v;int id=select_id(r);
    if(id>=0) v=selector[id];
    else if(r==0x4a480+pipe_idx*0x800) v=mode;
    else if(r==0x49028+pipe_idx*0x100) v=csc;
    else if(r==0x70034+pipe_idx*0x1000) v=bottom;
    else v=r^seed^(ticks++*0x01020304U);
    record(0,r,v);return v;
}
static void intel_de_write(struct intel_display *d,u32 r,u32 v) {
    int id=select_id(r);if(id<0) abort();selector[id]=v;record(1,r,v);
}
#define intel_de_read_fw intel_de_read
#define intel_de_write_fw intel_de_write
"#;
    let main = r#"
static void matrix(struct intel_csc_matrix *m) {
    for(int i=0;i<3;i++) printf("%u ",m->preoff[i]);
    for(int i=0;i<9;i++) printf("%u ",m->coeff[i]);
    for(int i=0;i<3;i++) printf("%u ",m->postoff[i]);
}
static void lut(struct drm_property_blob *b,int count) {
    if(!b) return;struct drm_color_lut *v=b->data;
    for(int i=0;i<count;i++) printf("%u %u %u ",v[i].red,v[i].green,v[i].blue);
    free(b->data);free(b);
}
int main(void) {
    int c8;
    while(scanf("%d %u %u %u %d",&pipe_idx,&mode,&csc,&seed,&c8)==5) {
        bottom=0x31234567;ticks=0;count=0;selector[0]=0x25;selector[1]=0x8002;selector[2]=0x8004;
        struct intel_crtc crtc={.pipe=pipe_idx};struct intel_crtc_state state={.uapi={.crtc=&crtc},.c8_planes=c8};
        skl_get_config(&state);icl_read_luts(&state);icl_read_csc(&state);
        printf("%u %u %u %u %u ",state.gamma_mode,state.csc_mode,state.gamma_enable,state.csc_enable,state.hw.background_color);
        if(csc&0x80000000) matrix(&state.csc);if(csc&0x40000000) matrix(&state.output_csc);
        lut(state.pre_csc_lut,129);
        int n=(mode&0x40000000)||c8 ? ((mode&3)==0?256:((mode&3)==1?1024:9)):0;
        lut(state.post_csc_lut,n);
        for(int i=0;i<count;i++) printf("%u %u %u ",trace[i][0],trace[i][1],trace[i][2]);puts("");
    }
}
"#;
    let code = [prefix, &regs, &functions, main].join("\n");
    let oracle = support::compile(&code, "color");
    let mut cases = Vec::new();
    let mut input = String::new();
    for pipe in [Pipe::A, Pipe::B, Pipe::C, Pipe::D] {
        for flags in 0..4u32 {
            for mode in [0, 1, 3] {
                for csc in [0, 1 << 31, 1 << 30, (1 << 31) | (1 << 30)] {
                    let mode = mode | (flags << 30);
                    let c8 = flags == 0;
                    let seed = 0x52437890;
                    input.push_str(&format!(
                        "{} {mode} {csc} {seed} {}\n",
                        pipe.index(),
                        u8::from(c8)
                    ));
                    cases.push((pipe, mode, csc, seed, c8));
                }
            }
        }
    }
    let mut child = Command::new(&oracle.executable)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()).unwrap());
    let output = child.wait_with_output().unwrap();
    writer.join().unwrap();
    assert!(output.status.success());
    let output = String::from_utf8(output.stdout).unwrap();
    assert_eq!(output.lines().count(), cases.len());
    for (line, (pipe, mode, csc, seed, c8)) in output.lines().zip(cases) {
        let io = Model::new(pipe, mode, csc, seed);
        let before = io.selectors.borrow().clone();
        let mut pre = vec![LutEntry::default(); 129];
        let mut post = vec![LutEntry::default(); 1024];
        let s = intel_color_get_config(&io, pipe, c8, &mut pre, &mut post).unwrap();
        let mut expected = vec![
            s.gamma_mode,
            s.csc_mode,
            u32::from(s.bottom_color & (1 << 31) != 0),
            u32::from(s.bottom_color & (1 << 30) != 0),
            s.bottom_color & 0x3fffffff,
        ];
        for m in [s.pipe_csc, s.output_csc].into_iter().flatten() {
            expected.extend(m.preoff.into_iter().map(u32::from));
            expected.extend(m.coeff.into_iter().map(u32::from));
            expected.extend(m.postoff.into_iter().map(u32::from));
        }
        for v in pre
            .iter()
            .take(s.degamma_entries)
            .chain(post.iter().take(s.post_lut.entries()))
        {
            expected.extend([u32::from(v.red), u32::from(v.green), u32::from(v.blue)]);
        }
        for &(op, r, v) in io.trace.borrow().iter() {
            // Remove only before-image selector reads and verified restoration.
            // Upstream's initial/reset-to-zero writes remain in the comparison.
            if before.get(&r).is_some_and(|old| op == 0 || v == *old) {
                continue;
            }
            expected.extend([op, r, v]);
        }
        assert_eq!(*io.selectors.borrow(), before);
        let actual: Vec<u32> = line
            .split_whitespace()
            .map(|n| n.parse().unwrap())
            .collect();
        assert_eq!(
            actual, expected,
            "pipe={pipe:?} mode={mode:x} csc={csc:x} c8={c8}"
        );
    }
    println!("192 raw color/CSC/LUT states, decoded entries and MMIO traces match compiled i915");
}
