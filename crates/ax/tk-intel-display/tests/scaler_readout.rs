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
    scaler::*,
};
struct Model {
    pipe: Pipe,
    ctls: [u32; 2],
    powered: bool,
    scalers: bool,
    reads: RefCell<Vec<u32>>,
    missing: Option<u32>,
}
impl RegisterIo for Model {
    fn read32(&self, r: u32) -> Result<u32, Error> {
        self.reads.borrow_mut().push(r);
        if self.missing == Some(r) {
            return Err(Error::Unavailable(r));
        }
        let base = 0x68170 + self.pipe.index() * 0x800;
        match r - base {
            0 => Ok(0x0078002a),
            4 => Ok(0x07800438),
            16 => Ok(self.ctls[0]),
            0x100 => Ok(0x000b0011),
            0x104 => Ok(0x050002d0),
            0x110 => Ok(self.ctls[1]),
            _ => Err(Error::Unavailable(r)),
        }
    }
    fn write32(&self, _: u32, _: u32) -> Result<(), Error> {
        panic!("readout wrote")
    }
}
impl ReadoutIo for Model {
    fn pipe_powered(&self, p: Pipe) -> bool {
        p == self.pipe && self.powered
    }
}
impl ScalerIo for Model {
    fn scalers_powered(&self, p: Pipe) -> bool {
        p == self.pipe && self.scalers
    }
}
fn model(pipe: Pipe, ctls: [u32; 2]) -> Model {
    Model {
        pipe,
        ctls,
        powered: true,
        scalers: true,
        reads: RefCell::new(Vec::new()),
        missing: None,
    }
}
#[test]
fn independent_dark_domain_performs_no_reads() {
    let mut m = model(Pipe::A, [1 << 31; 2]);
    m.scalers = false;
    assert_eq!(skl_scaler_get_config(&m, Pipe::A), Ok(None));
    assert_eq!(read_all_scalers(&m, Pipe::A), Ok(None));
    assert!(m.reads.borrow().is_empty());
    m.scalers = true;
    m.powered = false;
    assert_eq!(read_all_scalers(&m, Pipe::A), Err(Error::Refused));
    assert!(m.reads.borrow().is_empty());
}
#[test]
fn ownership_readout_does_not_miss_plane_or_reserved_bindings() {
    let m = model(Pipe::C, [(1 << 31) | (1 << 25), (1 << 31) | (7 << 25)]);
    assert_eq!(skl_scaler_get_config(&m, Pipe::C), Ok(None));
    let s = read_all_scalers(&m, Pipe::C).unwrap().unwrap();
    assert!(s.iter().all(|s| s.enabled()));
    assert_eq!((s[0].binding(), s[1].binding()), (1, 7));
    assert_eq!(s[0].position, Some(0x0078002a));
    assert_eq!(s[1].size, Some(0x050002d0));
}
#[test]
fn first_pipe_binding_wins_and_dimensions_have_no_plus_one() {
    let m = model(Pipe::D, [1 << 31; 2]);
    let s = skl_scaler_get_config(&m, Pipe::D).unwrap().unwrap();
    assert_eq!(
        s,
        PipeScalerConfig {
            id: 0,
            x: 120,
            y: 42,
            width: 1920,
            height: 1080
        }
    );
    assert_eq!(*m.reads.borrow(), [0x69980, 0x69970, 0x69974]);
}
#[test]
fn unavailable_control_or_window_is_not_a_zero_config() {
    for r in [0x68180, 0x68280, 0x68270, 0x68274] {
        let mut m = model(Pipe::A, [0, 1 << 31]);
        m.missing = Some(r);
        assert_eq!(
            skl_scaler_get_config(&m, Pipe::A),
            Err(Error::Unavailable(r))
        );
    }
}
#[test]
#[ignore = "requires local Linux 7.2.3 and GCC; run explicitly"]
fn scaler_configuration_and_read_order_match_compiled_i915() {
    let root = support::reference();
    let src = support::read(&root, "skl_scaler.c");
    let regs_src = support::read(&root, "intel_display_regs.h");
    let names: Vec<_> = regs_src
        .lines()
        .filter_map(|l| l.trim_start().strip_prefix("#define"))
        .map(|l| l.trim_start().split([' ', '\t', '(']).next().unwrap())
        .collect();
    let regs = support::defines(&regs_src, &names);
    let get = support::function(&src, "static int skl_pipe_scaler_get_hw_state(");
    let config = support::function(&src, "void skl_scaler_get_config(");
    let prefix = r#"
#include <stdint.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
typedef uint32_t u32;
#define REG_BIT(b) (1U<<(b))
#define REG_GENMASK(h,l) ((uint32_t)((UINT32_MAX>>(31-(h))) & (UINT32_MAX<<(l))))
#define REG_FIELD_GET(mask,v) (((v)&(mask))>>__builtin_ctz(mask))
#define REG_FIELD_PREP(mask,v) (((v)<<__builtin_ctz(mask))&(mask))
#define _PICK_EVEN(p,a,b) ((a)+(p)*((b)-(a)))
#define _MMIO_PIPE(p,a,b) _PICK_EVEN(p,a,b)
struct intel_display {int unused;};static struct intel_display model_display;
#define to_intel_display(s) (&model_display)
struct intel_crtc {int pipe,num_scalers;};
#define to_intel_crtc(c) (c)
struct drm_rect {u32 x,y,width,height;};
struct intel_crtc_scaler_state {struct {bool in_use;} scalers[2];int scaler_id;u32 scaler_users;};
struct intel_crtc_state {struct {struct intel_crtc *crtc;} uapi;struct intel_crtc_scaler_state scaler_state;struct {bool enabled;struct drm_rect dst;} pch_pfit;};
#define SKL_CRTC_INDEX 0
static bool scaler_has_casf(struct intel_display *d,int id) {return false;}
static void intel_casf_sharpness_get_config(struct intel_crtc_state *s) {abort();}
static void drm_rect_init(struct drm_rect *r,u32 x,u32 y,u32 w,u32 h) {*r=(struct drm_rect){x,y,w,h};}
static u32 ctls[2],reads[4];static int pipe,nreads;
static u32 intel_de_read(struct intel_display *d,u32 reg) {
    if(nreads>=4) abort();reads[nreads++]=reg;
    u32 base=0x68170+pipe*0x800;
    switch(reg-base) {case 0:return 0x0078002a;case 4:return 0x07800438;case 16:return ctls[0];
        case 0x100:return 0x000b0011;case 0x104:return 0x050002d0;case 0x110:return ctls[1];default:abort();}
}
"#;
    let main = r#"
int main(void) {
    while(scanf("%d %u %u",&pipe,&ctls[0],&ctls[1])==3) {
        struct intel_crtc crtc={.pipe=pipe,.num_scalers=2};struct intel_crtc_state s={.uapi={.crtc=&crtc}};nreads=0;
        skl_scaler_get_config(&s);printf("%u ",s.pch_pfit.enabled);
        if(s.pch_pfit.enabled) {struct drm_rect *r=&s.pch_pfit.dst;printf("%u %u %u %u %u ",s.scaler_state.scaler_id,r->x,r->y,r->width,r->height);}
        for(int i=0;i<nreads;i++) printf("%u ",reads[i]);puts("");
    }
}
"#;
    let code = [prefix, &regs, &get, &config, main].join("\n");
    let oracle = support::compile(&code, "scaler");
    let mut input = String::new();
    let mut cases = Vec::new();
    for pipe in [Pipe::A, Pipe::B, Pipe::C, Pipe::D] {
        for a in [0, 1 << 31, (1 << 31) | (1 << 25), (1 << 31) | (7 << 25)] {
            for b in [0, 1 << 31, (1 << 31) | (2 << 25)] {
                input.push_str(&format!("{} {a} {b}\n", pipe.index()));
                cases.push((pipe, [a, b]));
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
    for (line, (pipe, ctls)) in output.lines().zip(cases) {
        let m = model(pipe, ctls);
        let s = skl_scaler_get_config(&m, pipe).unwrap();
        let mut expected = vec![u32::from(s.is_some())];
        if let Some(s) = s {
            expected.extend([s.id, s.x, s.y, s.width, s.height]);
        }
        expected.extend(m.reads.borrow().iter());
        let actual: Vec<u32> = line
            .split_whitespace()
            .map(|n| n.parse().unwrap())
            .collect();
        assert_eq!(actual, expected);
    }
    println!("48 scaler configurations and read traces match compiled i915");
}
