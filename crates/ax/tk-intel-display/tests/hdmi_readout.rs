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
    hdmi::*,
    hdmi_packet::*,
};
struct Model {
    pipe: Pipe,
    control: u32,
    powered: bool,
    reads: RefCell<Vec<u32>>,
    missing: Option<u32>,
}
impl RegisterIo for Model {
    fn read32(&self, r: u32) -> Result<u32, Error> {
        self.reads.borrow_mut().push(r);
        if self.missing == Some(r) {
            return Err(Error::Unavailable(r));
        }
        if r == self.pipe.transcoder_register(0x60200) {
            Ok(self.control)
        } else {
            Ok(r.wrapping_mul(0x10001) ^ 0x52913478)
        }
    }
    fn write32(&self, _: u32, _: u32) -> Result<(), Error> {
        panic!("HDMI readout wrote")
    }
}
impl ReadoutIo for Model {
    fn pipe_powered(&self, p: Pipe) -> bool {
        self.pipe == p && self.powered
    }
}
fn model(pipe: Pipe, control: u32) -> Model {
    Model {
        pipe,
        control,
        powered: true,
        reads: RefCell::new(Vec::new()),
        missing: None,
    }
}
#[test]
fn inactive_frames_and_gcp_are_not_read_and_full_raw_control_is_retained() {
    let m = model(Pipe::C, (1 << 24) | (1 << 25));
    let s = read_hdmi_state(&m, Pipe::C).unwrap();
    assert_eq!(s.control, (1 << 24) | (1 << 25));
    assert_eq!(s.enable, 0);
    assert_eq!(s.enabled_packets, 0);
    assert_eq!(s.gcp, None);
    assert!(s.frames.iter().all(Option::is_none));
    assert_eq!(*m.reads.borrow(), [0x62200]);
    let s = read_hdmi_state(&model(Pipe::A, u32::MAX), Pipe::A).unwrap();
    assert_eq!(s.enabled_packets, 255);
    assert!(s.frames.iter().all(Option::is_some));
}
#[test]
fn dip_byte3_is_the_ecc_hole_not_the_checksum() {
    let mut raw = [0; 32];
    raw[..5].copy_from_slice(&[0x82, 2, 13, 0xab, 0]);
    raw[5 + 1] = 0x28;
    raw[5 + 3] = 16;
    let mut f = RawInfoframe {
        raw,
        kind: FrameType::Avi,
    };
    f.raw[4] = hdmi_infoframe_checksum(&f.packet()[..17]);
    let p = f.packet();
    assert_eq!(&p[..3], &[0x82, 2, 13]);
    assert_ne!(p[3], 0xab);
    assert_eq!(hdmi_infoframe_checksum(&p[..17]), 0);
    let Infoframe::Avi(a) = f.unpack().unwrap() else {
        panic!()
    };
    assert_eq!(a.video_code, 16);
    assert_eq!(a.picture_aspect, 2);
    f.raw[3] ^= 0xff;
    assert!(f.unpack().is_ok());
    f.raw[4] ^= 1;
    assert_eq!(f.unpack(), Err(Error::InvalidBlock));
    f.kind = FrameType::Spd;
    assert_eq!(f.unpack(), Err(Error::InvalidHeader));
}
#[test]
fn dark_and_missing_registers_do_not_create_empty_valid_packets() {
    let mut m = model(Pipe::A, u32::MAX);
    m.powered = false;
    assert_eq!(read_hdmi_state(&m, Pipe::A), Err(Error::Refused));
    assert!(m.reads.borrow().is_empty());
    m.powered = true;
    let good = model(Pipe::A, u32::MAX);
    read_hdmi_state(&good, Pipe::A).unwrap();
    for r in good.reads.take() {
        m.missing = Some(r);
        assert_eq!(read_hdmi_state(&m, Pipe::A), Err(Error::Unavailable(r)));
    }
}
#[test]
#[ignore = "requires local Linux 7.2.3 and GCC; run explicitly"]
fn hdmi_enabled_packets_and_mmio_read_traces_match_compiled_i915() {
    let root = support::reference();
    let src = support::read(&root, "intel_hdmi.c");
    let header = support::read(&root, "intel_display_regs.h");
    let names: Vec<_> = header
        .lines()
        .filter_map(|l| l.trim_start().strip_prefix("#define"))
        .map(|l| l.trim_start().split([' ', '\t', '(']).next().unwrap())
        .collect();
    let regs = support::defines(&header, &names);
    let table = format!(
        "{};",
        support::function(&src, "static const u8 infoframe_type_to_idx[] =")
    );
    let funcs = [
        "static u32 hsw_infoframe_enable(",
        "static intel_reg_t\nhsw_dip_data_reg(",
        "void hsw_read_infoframe(",
        "static u32 hsw_infoframes_enabled(",
    ]
    .into_iter()
    .map(|s| support::function(&src, s))
    .collect::<Vec<_>>()
    .join("\n");
    let enables = support::function(&src, "u32 intel_hdmi_infoframe_enable(");
    let map = support::function(&src, "u32 intel_hdmi_infoframes_enabled(");
    let gcp = support::function(&src, "void intel_hdmi_read_gcp_infoframe(");
    let prefix = r#"
#include <stdint.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <stddef.h>
#include <sys/types.h>
typedef uint32_t u32;typedef uint8_t u8;typedef uint32_t intel_reg_t;
#define REG_BIT(b) (1U<<(b))
#define BIT(b) REG_BIT(b)
#define REG_GENMASK(h,l) ((uint32_t)((UINT32_MAX>>(31-(h))) & (UINT32_MAX<<(l))))
#define _MMIO(r) (r)
#define _MMIO_PIPE(p,a,b) ((a)+(p)*((b)-(a)))
#define _MMIO_TRANS2(d,p,r) ((r)+(p)*0x1000)
#define _MMIO_TRANS(p,a,b) _MMIO_PIPE(p,a,b)
#define VLV_DISPLAY_BASE 0
#define _MMIO_BASE_PIPE3(...) 0
#define ARRAY_SIZE(v) (sizeof(v)/sizeof(v[0]))
#define DISPLAY_VER(d) 13
#define HAS_AS_SDP(d) true
#define HAS_DDI(d) true
#define HAS_PCH_SPLIT(d) false
#define HDMI_PACKET_TYPE_GENERAL_CONTROL 3
#define HDMI_PACKET_TYPE_GAMUT_METADATA 10
#define DP_SDP_VSC 7
#define DP_SDP_ADAPTIVE_SYNC 34
#define DP_SDP_PPS 16
#define HDMI_INFOFRAME_TYPE_AVI 130
#define HDMI_INFOFRAME_TYPE_SPD 131
#define HDMI_INFOFRAME_TYPE_VENDOR 129
#define HDMI_INFOFRAME_TYPE_DRM 135
#define INVALID_MMIO_REG 0
#define MISSING_CASE(...) abort()
struct intel_display {struct {bool valleyview,cherryview;} platform;};static struct intel_display model_display;
#define to_intel_display(p) (&model_display)
enum transcoder {TRANSCODER_A,TRANSCODER_B,TRANSCODER_C,TRANSCODER_D};
struct intel_crtc {int pipe;};
#define to_intel_crtc(p) (p)
struct intel_crtc_state {enum transcoder cpu_transcoder;struct {u32 enable,gcp;} infoframes;struct {struct intel_crtc *crtc;} uapi;};
struct intel_encoder {int unused;};
struct intel_digital_port {u32 (*infoframes_enabled)(struct intel_encoder*,const struct intel_crtc_state*);};static struct intel_digital_port model_dig_port;
#define enc_to_dig_port(e) (&model_dig_port)
static u32 g4x_infoframe_enable(unsigned int t) {return 0;}
static u32 control,reads[40];static int count,pipe_idx;
static u32 intel_de_read(struct intel_display *d,u32 r) {
    if(count>=40) abort();reads[count++]=r;
    if(r==0x60200+pipe_idx*0x1000) return control;return r*0x10001U^0x52913478;
}
"#;
    let main = r#"
int main(void) {
    while(scanf("%d %u",&pipe_idx,&control)==2) {
        struct intel_encoder encoder={0};struct intel_crtc crtc={.pipe=pipe_idx};struct intel_crtc_state state={.cpu_transcoder=pipe_idx,.uapi={.crtc=&crtc}};count=0;
        model_dig_port.infoframes_enabled=hsw_infoframes_enabled;u32 enabled=intel_hdmi_infoframes_enabled(&encoder,&state);state.infoframes.enable=enabled;
        intel_hdmi_read_gcp_infoframe(&encoder,&state);
        printf("%u %u ",enabled,state.infoframes.gcp);
        unsigned int types[]={HDMI_INFOFRAME_TYPE_AVI,HDMI_INFOFRAME_TYPE_SPD,HDMI_INFOFRAME_TYPE_VENDOR,HDMI_INFOFRAME_TYPE_DRM};
        for(int i=0;i<4;i++) if(enabled&intel_hdmi_infoframe_enable(types[i])) {
            u32 frame[8];hsw_read_infoframe(&encoder,&state,types[i],frame,sizeof(frame));for(int n=0;n<8;n++) printf("%u ",frame[n]);
        }
        for(int n=0;n<count;n++) printf("%u ",reads[n]);puts("");
    }
}
"#;
    let code = [prefix, &regs, &funcs, &table, &enables, &map, &gcp, main].join("\n");
    let oracle = support::compile(&code, "hdmi-readout");
    let mut input = String::new();
    let mut cases = Vec::new();
    for p in [Pipe::A, Pipe::B, Pipe::C, Pipe::D] {
        for flags in 0..64u32 {
            let ctl = ((flags & 1) << 12)
                | ((flags & 2) >> 1)
                | ((flags & 4) << 6)
                | ((flags & 8) << 25)
                | ((flags & 16) << 12)
                | ((flags & 32) << 18)
                | (1 << 24)
                | (1 << 25);
            cases.push((p, ctl));
            input.push_str(&format!("{} {ctl}\n", p.index()));
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
    for (line, (p, ctl)) in output.lines().zip(cases) {
        let m = model(p, ctl);
        let s = read_hdmi_state(&m, p).unwrap();
        let mut expected = vec![s.enabled_packets, s.gcp.unwrap_or(0)];
        for f in s.frames.into_iter().flatten() {
            for word in f.raw.as_chunks::<4>().0.iter() {
                expected.push(u32::from_le_bytes(*word));
            }
        }
        expected.extend(m.reads.borrow().iter());
        let actual: Vec<u32> = line
            .split_whitespace()
            .map(|n| n.parse().unwrap())
            .collect();
        assert_eq!(actual, expected, "{p:?} ctl={ctl:x}");
    }
    println!(
        "256 HDMI packet enable/GCP/DIP data states and exact read traces match compiled i915"
    );
}
