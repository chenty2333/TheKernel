// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
mod support;
use std::{
    cell::RefCell,
    io::Write,
    process::{Command, Stdio},
};

use tk_intel_display::{
    Error, RegisterIo,
    display::{Pipe, ReadoutIo},
    watermark::*,
};
struct Model {
    powered: bool,
    seed: u32,
    reads: RefCell<Vec<u32>>,
    missing: Option<u32>,
}
impl RegisterIo for Model {
    fn read32(&self, r: u32) -> Result<u32, Error> {
        assert!(self.powered);
        self.reads.borrow_mut().push(r);
        if self.missing == Some(r) {
            return Err(Error::Unavailable(r));
        }
        Ok(r.wrapping_mul(0x10001) ^ self.seed)
    }
    fn write32(&self, _: u32, _: u32) -> Result<(), Error> {
        panic!("readout wrote")
    }
}
impl ReadoutIo for Model {
    fn pipe_powered(&self, _: Pipe) -> bool {
        self.powered
    }
}
fn model(seed: u32) -> Model {
    Model {
        powered: true,
        seed,
        reads: RefCell::new(Vec::new()),
        missing: None,
    }
}
#[test]
fn display13_has_six_latency_levels_and_separate_sagv_and_cursor_registers() {
    let m = model(0);
    let wm = skl_pipe_wm_get_hw_state(&m, Pipe::D).unwrap();
    let r = m.reads.borrow();
    assert_eq!(r.len(), 54);
    assert_eq!(
        &r[..9],
        &[
            0x73240, 0x73244, 0x73248, 0x7324c, 0x73250, 0x73254, 0x73268, 0x73258, 0x7325c
        ]
    );
    assert_eq!(
        &r[45..54],
        &[
            0x73140, 0x73144, 0x73148, 0x7314c, 0x73150, 0x73154, 0x73168, 0x73158, 0x7315c
        ]
    );
    assert_eq!(wm[0].sagv.raw, 0x73258u32.wrapping_mul(0x10001));
}
#[test]
fn disabled_ddb_does_not_become_block_zero_and_unbounded_decode_is_not_admission() {
    let z = skl_ddb_entry_init_from_hw(0);
    assert_eq!(z.end, 0);
    assert_eq!(z.blocks(), 0);
    assert!(z.validate(4096).is_ok());
    let e = skl_ddb_entry_init_from_hw((63 << 16) | 4);
    assert_eq!((e.start, e.end, e.blocks()), (4, 64, 60));
    assert!(e.validate(4096).is_ok());
    for raw in [1, 2 << 16 | 9, u32::MAX] {
        assert!(skl_ddb_entry_init_from_hw(raw).validate(4096).is_err());
    }
    let l = skl_wm_level_from_reg_val(u32::MAX);
    assert_eq!((l.blocks, l.lines), (8191, 8191));
    assert!(l.enable && l.ignore_lines);
}
#[test]
fn dark_or_missing_domains_are_not_zero_watermarks() {
    let mut m = model(0);
    m.powered = false;
    assert_eq!(skl_pipe_wm_get_hw_state(&m, Pipe::A), Err(Error::Refused));
    assert_eq!(skl_pipe_ddb_get_hw_state(&m, Pipe::A), Err(Error::Refused));
    assert!(m.reads.borrow().is_empty());
    let mut good = model(1);
    skl_pipe_wm_get_hw_state(&good, Pipe::A).unwrap();
    let regs = good.reads.take();
    for r in regs {
        good.missing = Some(r);
        assert_eq!(
            skl_pipe_wm_get_hw_state(&good, Pipe::A),
            Err(Error::Unavailable(r))
        );
    }
    let mut m = model(1);
    m.missing = Some(0x7017c);
    assert_eq!(
        skl_pipe_ddb_get_hw_state(&m, Pipe::A),
        Err(Error::Unavailable(0x7017c))
    );
}
#[test]
fn dbuf_uses_irregular_slice_offsets_and_preserves_global_mbus_state() {
    let m = model(0xa0000000);
    let s = read_dbuf_state(&m).unwrap();
    assert_eq!(
        *m.reads.borrow(),
        [0x45008, 0x44fe8, 0x44300, 0x44304, 0x4438c]
    );
    let mut mask = 0;
    for (n, ctl) in s.ctl.into_iter().enumerate() {
        if ctl & (1 << 30) != 0 {
            mask |= 1 << n;
        }
    }
    assert_eq!(s.enabled_slices, mask);
    assert_eq!(s.mbus_ctl, 0x4438cu32.wrapping_mul(0x10001) ^ 0xa0000000);
}
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
fn watermark_ddb_dbuf_states_and_traces_match_compiled_i915() {
    let root = support::reference();
    let source = support::read(&root, "skl_watermark.c");
    let regs = [
        "skl_universal_plane_regs.h",
        "intel_cursor_regs.h",
        "skl_watermark_regs.h",
    ]
    .into_iter()
    .map(|p| all_defines(&support::read(&root, p)))
    .collect::<Vec<_>>()
    .join("\n");
    let functions = [
        "u8 intel_enabled_dbuf_slices_mask(",
        "static u16 skl_ddb_entry_init(",
        "static void skl_ddb_entry_init_from_hw(",
        "static void\nskl_ddb_get_hw_plane_state(",
        "static void skl_pipe_ddb_get_hw_state(",
        "static void skl_wm_level_from_reg_val(",
        "static void skl_pipe_wm_get_hw_state(",
    ]
    .into_iter()
    .map(|s| support::function(&source, s))
    .collect::<Vec<_>>()
    .join("\n");
    let prefix = r#"
#include <stdint.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
typedef uint32_t u32;typedef uint16_t u16;typedef uint8_t u8;
#define REG_BIT(b) (1U<<(b))
#define BIT(b) REG_BIT(b)
#define REG_GENMASK(h,l) ((uint32_t)((UINT32_MAX>>(31-(h))) & (UINT32_MAX<<(l))))
#define REG_FIELD_GET(mask,v) (((v)&(mask))>>__builtin_ctz(mask))
#define REG_FIELD_PREP(mask,v) (((v)<<__builtin_ctz(mask))&(mask))
#define _PIPE(p,a,b) ((a)+(p)*((b)-(a)))
#define _PLANE(p,a,b) _PIPE(p,a,b)
#define _MMIO(r) (r)
#define _MMIO_PIPE(p,a,b) _PIPE(p,a,b)
#define _PICK(p,a,b,c,d) ((u32[]){a,b,c,d}[p])
#define DISPLAY_VER(d) 13
#define HAS_HW_SAGV_WM(d) true
#define HAS_SAGV_WM(d) true
#define POWER_DOMAIN_PIPE(p) (p)
enum dbuf_slice {S0,S1,S2,S3};
enum intel_display_power_domain {DOMAIN_PIPE};
enum pipe {PIPE_A,PIPE_B,PIPE_C,PIPE_D};
enum plane_id {PLANE_1,PLANE_2,PLANE_3,PLANE_4,PLANE_5,PLANE_6,PLANE_7,PLANE_CURSOR};
#define for_each_dbuf_slice(d,s) for(s=S0;s<=S3;s++)
#define for_each_plane_id_on_crtc(c,p) for(p=PLANE_1;p<=PLANE_CURSOR;p=(p==PLANE_5 ? PLANE_CURSOR : p+1))
struct intel_display {struct {int num_levels;} wm;};static struct intel_display model_display={.wm={.num_levels=6}};
#define to_intel_display(p) (&model_display)
struct intel_crtc {enum pipe pipe;};
struct skl_ddb_entry {u16 start,end;};
struct skl_wm_level {bool enable,ignore_lines,auto_min_alloc_wm_enable;u32 blocks,lines;};
struct skl_plane_wm {struct skl_wm_level wm[6],trans_wm;struct {struct skl_wm_level wm0,trans_wm;} sagv;};
struct skl_pipe_wm {struct skl_plane_wm planes[8];};
struct ref_tracker {int unused;};static struct ref_tracker power_ref;
static struct ref_tracker *intel_display_power_get_if_enabled(struct intel_display *d,int domain) {return &power_ref;}
static void intel_display_power_put(struct intel_display *d,int domain,struct ref_tracker *r) {}
static u32 seed,reads[70];static int nreads;
static u32 intel_de_read(struct intel_display *d,u32 r) {if(nreads>=70) abort();reads[nreads++]=r;return r*0x10001U^seed;}
"#;
    let main = r#"
static void level(struct skl_wm_level *l) {printf("%u %u %u %u ",l->enable,l->ignore_lines,l->blocks,l->lines);}
int main(void) {
    int pipe;
    while(scanf("%d %u",&pipe,&seed)==2) {
        struct intel_crtc crtc={.pipe=pipe};struct skl_pipe_wm wm={0};struct skl_ddb_entry ddb[8]={0},ddb_y[8]={0};u16 min[8]={0},interim[8]={0};nreads=0;
        skl_pipe_wm_get_hw_state(&crtc,&wm);skl_pipe_ddb_get_hw_state(&crtc,ddb,ddb_y,min,interim);
        u8 enabled=intel_enabled_dbuf_slices_mask(&model_display);u32 mbus=intel_de_read(&model_display,MBUS_CTL);
        enum plane_id p;for_each_plane_id_on_crtc(&crtc,p) {
            struct skl_plane_wm *w=&wm.planes[p];for(int i=0;i<6;i++) level(&w->wm[i]);level(&w->trans_wm);level(&w->sagv.wm0);level(&w->sagv.trans_wm);
        }
        for_each_plane_id_on_crtc(&crtc,p) printf("%u %u ",ddb[p].start,ddb[p].end);
        printf("%u %u ",enabled,mbus);for(int i=0;i<nreads;i++) printf("%u ",reads[i]);puts("");
    }
}
"#;
    let code = [prefix, &regs, &functions, main].join("\n");
    let oracle = support::compile(&code, "wm");
    let mut cases = Vec::new();
    let mut input = String::new();
    for pipe in [Pipe::A, Pipe::B, Pipe::C, Pipe::D] {
        for n in 0..16u32 {
            let seed = n.wrapping_mul(0x12345678);
            cases.push((pipe, seed));
            input.push_str(&format!("{} {seed}\n", pipe.index()));
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
    for (line, (pipe, seed)) in output.lines().zip(cases) {
        let m = model(seed);
        let wm = skl_pipe_wm_get_hw_state(&m, pipe).unwrap();
        let ddb = skl_pipe_ddb_get_hw_state(&m, pipe).unwrap();
        let dbuf = read_dbuf_state(&m).unwrap();
        let mut expected = Vec::new();
        for w in wm {
            for l in w
                .levels
                .into_iter()
                .chain([w.transition, w.sagv, w.sagv_transition])
            {
                expected.extend([
                    u32::from(l.enable),
                    u32::from(l.ignore_lines),
                    l.blocks,
                    l.lines,
                ]);
            }
        }
        for e in ddb {
            expected.extend([u32::from(e.start), u32::from(e.end)]);
        }
        expected.extend([u32::from(dbuf.enabled_slices), dbuf.mbus_ctl]);
        expected.extend(m.reads.borrow().iter());
        let actual: Vec<u32> = line
            .split_whitespace()
            .map(|n| n.parse().unwrap())
            .collect();
        assert_eq!(actual, expected, "pipe={pipe:?} seed={seed:x}");
    }
    println!("64 display-13 WM/DDB/DBUF states and 65-register traces match compiled i915");
}
