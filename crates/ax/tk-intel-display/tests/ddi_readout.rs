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
    ddi::*,
    device::Port,
    display::{Pipe, ReadoutIo},
    dkl_phy::{DklIo, TcPort},
    tc::TcIo,
};
struct Model {
    powered: bool,
    raw: u32,
    select: u32,
    gate: u32,
    reads: RefCell<Vec<u32>>,
    missing: Option<u32>,
}
impl RegisterIo for Model {
    fn read32(&self, r: u32) -> Result<u32, Error> {
        self.reads.borrow_mut().push(r);
        if self.missing == Some(r) {
            return Err(Error::Unavailable(r));
        }
        if [0x60400, 0x61400, 0x62400, 0x63400].contains(&r) {
            return Ok(self.raw);
        }
        if [0x4610c, 0x46110, 0x46114, 0x46118].contains(&r) {
            return Ok(self.select);
        }
        if r == 0x164280 {
            return Ok(self.gate);
        }
        Err(Error::Unavailable(r))
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
impl DklIo for Model {
    fn with_dkl_lock<T>(&self, _: impl FnOnce() -> Result<T, Error>) -> Result<T, Error> {
        panic!("clock readout touched HIP")
    }
}
impl TcIo for Model {
    fn display_core_powered(&self) -> bool {
        self.powered
    }
    fn tc_port_powered(&self, _: TcPort) -> bool {
        self.powered
    }
    fn tc_cold_blocked(&self, _: TcPort) -> bool {
        self.powered
    }
}
fn model(raw: u32, select: u32, gate: u32) -> Model {
    Model {
        powered: true,
        raw,
        select,
        gate,
        reads: RefCell::new(Vec::new()),
        missing: None,
    }
}
#[test]
fn port_field_uses_tgl_encoding_and_hdmi_does_not_use_dp_lane_width() {
    for (code, port) in [
        (1, Port::A),
        (2, Port::B),
        (4, Port::Tc1),
        (5, Port::Tc2),
        (6, Port::Tc3),
        (7, Port::Tc4),
    ] {
        let s = decode_function_control((1 << 31) | (code << 27) | (7 << 1));
        assert_eq!(s.port, Some(port));
        assert_eq!(s.hdmi_lanes, 4);
        assert_eq!(s.bpp, Some(24));
        assert_eq!(s.mode, DdiMode::Hdmi);
        assert!(s.enabled);
    }
    for code in [0, 3, 8, 15] {
        assert_eq!(decode_function_control(code << 27).port, None);
    }
    for bpc in 4..8 {
        assert_eq!(decode_function_control(bpc << 20).bpp, None);
    }
    let dvi = decode_function_control((1 << 24) | 17);
    assert!(!dvi.hdmi_scrambling && !dvi.high_tmds_ratio);
    assert_eq!(dvi.hdmi_lanes, 4);
}
#[test]
fn tc4_clock_gate_is_bit21_and_unknown_route_is_not_a_dkl_pll() {
    for p in [TcPort::Tc1, TcPort::Tc2, TcPort::Tc3, TcPort::Tc4] {
        assert_eq!(tc_clock_select_register(p), 0x4610c + p.index() * 4);
        assert_eq!(
            tc_clock_off_mask(p),
            1 << if p.index() < 3 { 12 + p.index() } else { 21 }
        );
        let m = model(0, 8 << 28, tc_clock_off_mask(p));
        assert!(!icl_ddi_tc_is_clock_enabled(&m, p).unwrap());
        assert_eq!(icl_ddi_tc_get_pll(&m, p).unwrap(), Some(TcPllKind::Dkl));
        let s = read_tc_clock_state(&m, p).unwrap();
        assert_eq!(s.dpclka, tc_clock_off_mask(p));
        assert!(!s.enabled);
    }
    for sel in 0..16 {
        let m = model(0, sel << 28, 0);
        let pll = icl_ddi_tc_get_pll(&m, TcPort::Tc1).unwrap();
        assert_eq!(
            pll,
            match sel {
                8 => Some(TcPllKind::Dkl),
                12..=15 => Some(TcPllKind::Tbt),
                _ => None,
            }
        );
    }
    let m = model(0, 0, 0);
    assert!(!icl_ddi_tc_is_clock_enabled(&m, TcPort::Tc1).unwrap());
    assert_eq!(m.reads.borrow().len(), 1);
}
#[test]
fn missing_and_dark_domains_never_return_a_valid_control_or_route() {
    let mut m = model(0, 0, 0);
    m.powered = false;
    assert_eq!(read_function_control(&m, Pipe::A), Err(Error::Refused));
    assert_eq!(read_tc_clock_state(&m, TcPort::Tc1), Err(Error::Refused));
    assert!(m.reads.borrow().is_empty());
    m.powered = true;
    m.select = 8 << 28;
    for r in [0x4610c, 0x164280] {
        m.missing = Some(r);
        assert_eq!(
            read_tc_clock_state(&m, TcPort::Tc1),
            Err(Error::Unavailable(r))
        );
    }
    m.missing = Some(0x60400);
    assert_eq!(
        read_function_control(&m, Pipe::A),
        Err(Error::Unavailable(0x60400))
    );
}
#[test]
#[ignore = "requires local Linux 7.2.3 and GCC; run explicitly"]
fn hdmi_function_fields_and_tc_routes_match_compiled_i915() {
    let root = support::reference();
    let src = support::read(&root, "intel_ddi.c");
    let header = support::read(&root, "intel_display_regs.h");
    let names: Vec<_> = header
        .lines()
        .filter_map(|l| l.trim_start().strip_prefix("#define"))
        .map(|l| l.trim_start().split([' ', '\t', '(']).next().unwrap())
        .collect();
    let regs = support::defines(&header, &names);
    let fns = [
        "static void intel_ddi_read_func_ctl_dvi(",
        "static void intel_ddi_read_func_ctl_hdmi(",
        "static void intel_ddi_read_func_ctl(",
        "static bool icl_ddi_tc_is_clock_enabled(",
        "static struct intel_dpll *icl_ddi_tc_get_pll(",
    ]
    .into_iter()
    .map(|s| support::function(&src, s))
    .collect::<Vec<_>>()
    .join("\n");
    let prefix = r#"
#include <stdint.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
typedef uint32_t u32;typedef uint32_t intel_reg_t;
#define REG_BIT(b) (1U<<(b))
#define REG_GENMASK(h,l) ((uint32_t)((UINT32_MAX>>(31-(h))) & (UINT32_MAX<<(l))))
#define REG_FIELD_GET(mask,v) (((v)&(mask))>>__builtin_ctz(mask))
#define REG_FIELD_PREP(mask,v) (((v)<<__builtin_ctz(mask))&(mask))
#define _MMIO(r) (r)
#define _MMIO_PORT(p,a,b) ((a)+(p)*((b)-(a)))
#define _MMIO_TRANS2(d,p,r) ((r)+(p)*0x1000)
#define DISPLAY_VER(d) 13
#define HAS_DP20(d) false
#define BIT(b) (1U<<(b))
#define MISSING_CASE(...) do {} while(0)
#define fallthrough ((void)0)
#define DRM_MODE_FLAG_PHSYNC 1
#define DRM_MODE_FLAG_NHSYNC 2
#define DRM_MODE_FLAG_PVSYNC 4
#define DRM_MODE_FLAG_NVSYNC 8
#define INTEL_OUTPUT_HDMI 0
struct intel_display {int unused;};static struct intel_display model_display;
#define to_intel_display(p) (&model_display)
enum port {PORT_A,PORT_B,PORT_C,PORT_D,PORT_E,PORT_F,PORT_G};
enum tc_port {TC_PORT_1,TC_PORT_2,TC_PORT_3,TC_PORT_4};
enum transcoder {TRANSCODER_A,TRANSCODER_B,TRANSCODER_C,TRANSCODER_D};
struct intel_encoder {enum port port;enum tc_port tc;};
struct intel_crtc_state {enum transcoder cpu_transcoder;struct {struct {u32 flags;} adjusted_mode;} hw;
    u32 pipe_bpp,lane_count,output_types;bool has_hdmi_sink,has_infoframe,hdmi_scrambling,hdmi_high_tmds_clock_ratio;struct {u32 enable;} infoframes;};
struct intel_dp {int unused;};
static struct intel_dp *enc_to_intel_dp(struct intel_encoder *e) {abort();}
static bool intel_dp_mst_active_streams(struct intel_dp *dp) {abort();}
static u32 intel_hdmi_infoframes_enabled(struct intel_encoder *e,struct intel_crtc_state *s) {return 0;}
static void intel_ddi_read_func_ctl_fdi(struct intel_encoder *e,struct intel_crtc_state *s,u32 val) {abort();}
static void intel_ddi_read_func_ctl_dp_sst(struct intel_encoder *e,struct intel_crtc_state *s,u32 val) {abort();}
static void intel_ddi_read_func_ctl_dp_mst(struct intel_encoder *e,struct intel_crtc_state *s,u32 val) {abort();}
enum intel_dpll_id {DPLL_TC1,DPLL_TC2,DPLL_TC3,DPLL_TC4,DPLL_ID_ICL_TBTPLL};
struct intel_dpll {enum intel_dpll_id id;};static struct intel_dpll plls[5]={{DPLL_TC1},{DPLL_TC2},{DPLL_TC3},{DPLL_TC4},{DPLL_ID_ICL_TBTPLL}};
static struct intel_dpll *intel_get_dpll_by_id(struct intel_display *d,enum intel_dpll_id id) {return &plls[id];}
static enum tc_port intel_encoder_to_tc(struct intel_encoder *e) {return e->tc;}
static enum intel_dpll_id icl_tc_port_to_pll_id(enum tc_port p) {return (enum intel_dpll_id)p;}
static u32 raw,select_word,gate,reads[4];static int count;
static u32 intel_de_read(struct intel_display *d,u32 r) {
    if(count>=4) abort();reads[count++]=r;
    if(r>=0x60400 && r<=0x63400) return raw;if(r==0x164280) return gate;
    if(r>=0x4610c && r<=0x46118) return select_word;abort();
}
"#;
    let main = r#"
int main(void) {
    int pipe,p;
    while(scanf("%d %d %u %u %u",&pipe,&p,&raw,&select_word,&gate)==5) {
        struct intel_encoder e={.port=PORT_D+p,.tc=p};struct intel_crtc_state s={.cpu_transcoder=pipe};count=0;
        intel_ddi_read_func_ctl(&e,&s);bool enabled=icl_ddi_tc_is_clock_enabled(&e);struct intel_dpll *pll=icl_ddi_tc_get_pll(&e);
        printf("%u %u %u %u %u %u ",s.hw.adjusted_mode.flags,s.pipe_bpp,s.lane_count,s.hdmi_scrambling,s.hdmi_high_tmds_clock_ratio,enabled);
        printf("%u ",pll ? (pll->id==DPLL_ID_ICL_TBTPLL?2:1) : 0);for(int i=0;i<count;i++) printf("%u ",reads[i]);puts("");
    }
}
"#;
    let code = [prefix, &regs, &fns, main].join("\n");
    let oracle = support::compile(&code, "ddi");
    let mut cases = Vec::new();
    let mut input = String::new();
    for p in [TcPort::Tc1, TcPort::Tc2, TcPort::Tc3, TcPort::Tc4] {
        for bpc in 0..8u32 {
            for mode in 0..2u32 {
                for flags in 0..4u32 {
                    for sel in 0..16u32 {
                        let pipe = [Pipe::A, Pipe::B, Pipe::C, Pipe::D][p.index() as usize];
                        let raw = (1 << 31)
                            | ((p.index() + 4) << 27)
                            | (mode << 24)
                            | (bpc << 20)
                            | (flags << 16)
                            | 17;
                        let gate = if flags & 1 != 0 {
                            tc_clock_off_mask(p)
                        } else {
                            0
                        };
                        let select = sel << 28;
                        cases.push((p, pipe, raw, select, gate));
                        input.push_str(&format!(
                            "{} {} {raw} {select} {gate}\n",
                            pipe.index(),
                            p.index()
                        ));
                    }
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
    for (line, (p, pipe, raw, select, gate)) in output.lines().zip(cases) {
        let m = model(raw, select, gate);
        let s = read_function_control(&m, pipe).unwrap();
        let enabled = icl_ddi_tc_is_clock_enabled(&m, p).unwrap();
        let pll = icl_ddi_tc_get_pll(&m, p).unwrap();
        let flags = if s.positive_hsync { 1 } else { 2 } | if s.positive_vsync { 4 } else { 8 };
        let mut expected = vec![
            flags,
            s.bpp.unwrap_or(0),
            s.hdmi_lanes,
            u32::from(s.hdmi_scrambling),
            u32::from(s.high_tmds_ratio),
            u32::from(enabled),
            match pll {
                Some(TcPllKind::Dkl) => 1,
                Some(TcPllKind::Tbt) => 2,
                _ => 0,
            },
        ];
        expected.extend(m.reads.borrow().iter());
        let actual: Vec<u32> = line
            .split_whitespace()
            .map(|n| n.parse().unwrap())
            .collect();
        assert_eq!(
            actual, expected,
            "{p:?} raw={raw:x} sel={select:x} gate={gate:x}"
        );
    }
    println!("4096 HDMI/DVI control and TC clock-route states/read traces match compiled i915");
}
