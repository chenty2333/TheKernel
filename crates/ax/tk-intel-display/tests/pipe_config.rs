// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
mod support;
use std::{
    cell::RefCell,
    collections::BTreeMap,
    io::Write,
    process::{Command, Stdio},
};

use tk_intel_display::{
    Error, RegisterIo,
    display::{Pipe, ReadoutIo},
    pipe_config::*,
};
struct Model {
    pipe: Pipe,
    powered: bool,
    regs: BTreeMap<u32, u32>,
    reads: RefCell<Vec<u32>>,
}
impl RegisterIo for Model {
    fn read32(&self, r: u32) -> Result<u32, Error> {
        self.reads.borrow_mut().push(r);
        self.regs.get(&r).copied().ok_or(Error::Unavailable(r))
    }
    fn write32(&self, _: u32, _: u32) -> Result<(), Error> {
        panic!("readout wrote")
    }
}
impl ReadoutIo for Model {
    fn pipe_powered(&self, p: Pipe) -> bool {
        self.powered && p == self.pipe
    }
}
fn model(pipe: Pipe, ctl: u32, misc: u32) -> Model {
    let trans = |r| pipe.transcoder_register(r);
    let mut regs = BTreeMap::from([
        (trans(0x60078), 0x0440043c),
        (trans(0x60420), ctl),
        (trans(0x60438), 1123),
        (trans(0x60424), 1300),
        (trans(0x60434), 1124),
        (0x70030 + pipe.index() * 0x1000, misc),
        (trans(0x70008), 3 << 30),
        (0x78000 + pipe.index() * 0x200, 0),
        (0x78004 + pipe.index() * 0x200, 0),
        (chicken_trans_register(pipe), 3 << 27),
        (0x45270 + pipe.index() * 4, 0x1234),
        (trans(0x6002c), 0),
        (trans(0x6001c), (1919 << 16) | 1079),
        (trans(0x6007c), 0),
    ]);
    for (r, a, b) in [
        (0x60000, 1919, 2199),
        (0x60004, 1919, 2199),
        (0x60008, 2007, 2051),
        (0x6000c, 1079, 1124),
        (0x60010, 1079, 1124),
        (0x60014, 1083, 1088),
    ] {
        regs.insert(trans(r), (b << 16) | a);
    }
    Model {
        pipe,
        powered: true,
        regs,
        reads: RefCell::new(Vec::new()),
    }
}
#[test]
fn misc_all_encodings_and_yuv420_priority_are_not_native_admission() {
    for (code, bpc) in [
        (0, Some(8)),
        (1, Some(10)),
        (2, Some(6)),
        (3, None),
        (4, Some(12)),
        (5, None),
        (6, None),
        (7, None),
    ] {
        let m = decode_pipe_misc((code << 5) | (1 << 27) | (1 << 11));
        assert_eq!(m.bpc, bpc);
        assert_eq!(m.output, OutputFormat::Ycbcr420);
        assert!(!m.full_blend());
        assert!(decode_pipe_misc(m.raw | (1 << 26)).full_blend());
    }
}
#[test]
fn dark_and_disabled_do_not_read_active_state() {
    let mut m = model(Pipe::B, 0, 0);
    m.powered = false;
    assert_eq!(read_pipe_config(&m, Pipe::B), Ok(None));
    assert!(m.reads.borrow().is_empty());
    m.powered = true;
    m.regs.insert(0x71008, 0);
    assert_eq!(read_pipe_config(&m, Pipe::B), Ok(None));
    assert_eq!(*m.reads.borrow(), [0x71008]);
}
#[test]
fn pipe_d_uses_irregular_chicken_and_dss_stride_and_scl() {
    let m = model(Pipe::D, 0, 0);
    let s = read_pipe_config(&m, Pipe::D).unwrap().unwrap();
    s.timings.validate().unwrap();
    assert_eq!(s.frame_start_delay, 4);
    assert_eq!(s.source, (1920, 1080));
    assert_eq!(s.pixel_multiplier, 1);
    assert_eq!(s.linetime, 0x34);
    assert_eq!(&m.reads.borrow()[..4], [0x73008, 0x78600, 0x78604, 0x420d8]);
    assert!(s.dss.uncompressed());
    assert!(
        !DssConfig {
            control1: 1 << 29,
            control2: 0
        }
        .uncompressed()
    );
    assert!(
        !DssConfig {
            control1: 0,
            control2: 1 << 31
        }
        .uncompressed()
    );
}
#[test]
fn every_read_failure_and_overflow_is_refused_not_zero_filled() {
    let baseline = model(Pipe::A, (1 << 31) | (1 << 29), 0);
    read_pipe_config(&baseline, Pipe::A).unwrap();
    for &r in baseline.reads.borrow().iter() {
        let mut m = model(Pipe::A, (1 << 31) | (1 << 29), 0);
        m.regs.remove(&r);
        assert_eq!(read_pipe_config(&m, Pipe::A), Err(Error::Unavailable(r)));
    }
    for r in [0x60438, 0x60424, 0x60434, 0x6002c] {
        let mut m = model(Pipe::A, 1 << 29, 0);
        m.regs.insert(r, u32::MAX);
        assert_eq!(read_pipe_config(&m, Pipe::A), Err(Error::Refused));
    }
}
#[test]
#[ignore = "requires local Linux 7.2.3 and GCC; run explicitly"]
fn pipe_misc_vrr_fields_and_read_traces_match_compiled_i915() {
    let root = support::reference();
    let display = support::read(&root, "intel_display.c");
    let vrr = support::read(&root, "intel_vrr.c");
    let mut regs = String::new();
    for name in ["intel_display_regs.h", "intel_vrr_regs.h"] {
        let src = support::read(&root, name);
        let names: Vec<_> = src
            .lines()
            .filter_map(|s| s.trim_start().strip_prefix("#define"))
            .map(|s| s.trim_start().split([' ', '\t', '(']).next().unwrap())
            .collect();
        regs.push_str(&support::defines(&src, &names));
    }
    let prefix = r#"
#include <stdint.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
typedef uint32_t u32; typedef uint64_t u64;
#define REG_BIT(b) (1U<<(b))
#define REG_GENMASK(h,l) ((uint32_t)((UINT32_MAX>>(31-(h))) & (UINT32_MAX<<(l))))
#define REG_FIELD_GET(mask,v) (((v)&(mask))>>__builtin_ctz(mask))
#define REG_FIELD_PREP(mask,v) (((v)<<__builtin_ctz(mask))&(mask))
#define _MMIO_PIPE(p,a,b) ((a)+(p)*((b)-(a)))
#define _MMIO_TRANS2(d,p,a) ((a)+(p)*0x1000)
#define DISPLAY_VER(d) 13
#define HAS_CMRR(d) 0
#define HAS_AS_SDP(d) 1
#define I915_MODE_FLAG_VRR 1
#define drm_WARN_ON(d,v) ((void)(v))
enum intel_output_format {INTEL_OUTPUT_FORMAT_RGB,INTEL_OUTPUT_FORMAT_YCBCR444,INTEL_OUTPUT_FORMAT_YCBCR420};
enum transcoder {TRANSCODER_A,TRANSCODER_B,TRANSCODER_C,TRANSCODER_D};
struct intel_display{void *drm;};static struct intel_display model_display;
#define to_intel_display(s) (&model_display)
struct intel_crtc{int pipe;};
struct intel_crtc_state {int cpu_transcoder,set_context_latency,mode_flags;
 struct {bool enable;u64 cmrr_n,cmrr_m;} cmrr;
 struct {bool enable;int guardband,pipeline_full,flipline,vmax,vmin,vsync_start,vsync_end;} vrr;
 struct {struct {int crtc_vtotal,crtc_vblank_start;} adjusted_mode;} hw;};
static u32 ctl,misc,reads[6];static int pipe,nreads;
static u32 intel_de_read(struct intel_display *d,u32 r) {
 if(nreads>=6) abort(); reads[nreads++]=r;
 switch(r-pipe*0x1000) {case 0x70030:return misc;case 0x60420:return ctl;
 case 0x60078:return 0x0440043c;case 0x60438:return 1123;case 0x60424:return 1300;case 0x60434:return 1124;default:abort();}}
static u64 intel_de_read64_2x32(struct intel_display*d,u32 r){abort();}
static int intel_vrr_pipeline_full_to_guardband(struct intel_crtc_state*s,int x){abort();}
static int intel_vrr_vmin_flipline_offset(struct intel_display*d){abort();}
static bool intel_vrr_always_use_vrr_tg(struct intel_display*d){return false;}
static int intel_vrr_vmin_vtotal(struct intel_crtc_state*s){abort();}
static bool intel_vrr_is_fixed_rr(struct intel_crtc_state*s){abort();}
static void intel_vrr_get_dc_balance_config(struct intel_crtc_state*s){}
"#;
    let main = r#"
int main(void){while(scanf("%d %u %u",&pipe,&ctl,&misc)==3){
 nreads=0;struct intel_crtc c={.pipe=pipe};struct intel_crtc_state s={.cpu_transcoder=pipe};
 int output=bdw_get_pipe_misc_output_format(&c);intel_vrr_get_config(&s);
 printf("%u %u %u %u %u %u %u %u ",output,s.vrr.enable,s.vrr.guardband,s.vrr.flipline,s.vrr.vmax,s.vrr.vmin,s.vrr.vsync_start,s.vrr.vsync_end);
 for(int i=0;i<nreads;i++)printf("%u ",reads[i]);puts("");}}
"#;
    let code = [
        prefix,
        &regs,
        &support::function(
            &display,
            "static enum intel_output_format\nbdw_get_pipe_misc_output_format(",
        ),
        &support::function(&vrr, "void intel_vrr_get_config("),
        main,
    ]
    .join("\n");
    let oracle = support::compile(&code, "pipe-config");
    let mut input = String::new();
    let mut cases = Vec::new();
    for p in [Pipe::A, Pipe::B, Pipe::C, Pipe::D] {
        for flags in 0..4 {
            for code in 0..32 {
                let ctl = ((flags & 1) << 31) | ((flags >> 1) << 29) | 0x134;
                let misc =
                    ((code & 7) << 5) | (((code >> 3) & 1) << 27) | (((code >> 4) & 1) << 11);
                input.push_str(&format!("{} {ctl} {misc}\n", p.index()));
                cases.push((p, ctl, misc));
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
    let out = child.wait_with_output().unwrap();
    writer.join().unwrap();
    assert!(out.status.success());
    let out = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.lines().count(), cases.len());
    for (line, (p, ctl, misc)) in out.lines().zip(cases) {
        let m = model(p, ctl, misc);
        let output = bdw_get_pipe_misc_output_format(&m, p).unwrap();
        let v = intel_vrr_get_config(&m, p).unwrap();
        let mut expected = vec![
            match output.output {
                OutputFormat::Rgb => 0,
                OutputFormat::Ycbcr444 => 1,
                OutputFormat::Ycbcr420 => 2,
            },
            u32::from(v.enabled),
            v.guardband,
            v.flipline.unwrap_or(0),
            v.vmax.unwrap_or(0),
            v.vmin.unwrap_or(0),
            v.vsync.unwrap_or((0, 0)).0,
            v.vsync.unwrap_or((0, 0)).1,
        ];
        expected.extend(m.reads.borrow().iter());
        let actual: Vec<u32> = line
            .split_whitespace()
            .map(|s| s.parse().unwrap())
            .collect();
        assert_eq!(actual, expected);
    }
    println!("512 pipe misc/VRR states and MMIO traces match compiled i915");
}
