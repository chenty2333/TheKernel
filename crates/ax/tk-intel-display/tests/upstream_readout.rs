// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! The C body and masks are loaded from local Linux, never a second Rust model.
mod support;
use std::{
    cell::RefCell,
    io::Write,
    process::{Command, Stdio},
};

use tk_intel_display::{
    Error, RegisterIo,
    display::{Pipe, ReadoutIo, intel_get_transcoder_timings},
};
struct Model {
    pipe: Pipe,
    words: [u32; 7],
    reads: RefCell<Vec<u32>>,
}
impl RegisterIo for Model {
    fn read32(&self, offset: u32) -> Result<u32, Error> {
        self.reads.borrow_mut().push(offset);
        [
            0x60000, 0x60004, 0x60008, 0x6000c, 0x60010, 0x60014, 0x6007c,
        ]
        .into_iter()
        .position(|r| self.pipe.transcoder_register(r) == offset)
        .map(|i| self.words[i])
        .ok_or(Error::Unavailable(offset))
    }
    fn write32(&self, _: u32, _: u32) -> Result<(), Error> {
        panic!("readout wrote")
    }
}
impl ReadoutIo for Model {
    fn pipe_powered(&self, p: Pipe) -> bool {
        p == self.pipe
    }
}
#[test]
#[ignore = "requires local Linux 7.2.3 and GCC; run explicitly"]
fn translated_timings_and_read_order_match_compiled_i915() {
    let reference = support::reference();
    let regs = support::read(&reference, "intel_display_regs.h");
    let masks = support::defines(
        &regs,
        &[
            "_TRANS_HTOTAL_A",
            "TRANS_HTOTAL",
            "HTOTAL_MASK",
            "HACTIVE_MASK",
            "_TRANS_HBLANK_A",
            "TRANS_HBLANK",
            "HBLANK_START_MASK",
            "HBLANK_END_MASK",
            "_TRANS_HSYNC_A",
            "TRANS_HSYNC",
            "HSYNC_START_MASK",
            "HSYNC_END_MASK",
            "_TRANS_VTOTAL_A",
            "TRANS_VTOTAL",
            "VACTIVE_MASK",
            "VTOTAL_MASK",
            "_TRANS_VBLANK_A",
            "TRANS_VBLANK",
            "VBLANK_START_MASK",
            "VBLANK_END_MASK",
            "_TRANS_VSYNC_A",
            "TRANS_VSYNC",
            "VSYNC_START_MASK",
            "VSYNC_END_MASK",
            "_TRANS_A_SET_CONTEXT_LATENCY",
            "TRANS_SET_CONTEXT_LATENCY",
        ],
    );
    let function = support::function(
        &support::read(&reference, "intel_display.c"),
        "static void intel_get_transcoder_timings(",
    );
    let prefix = r#"
#include <stdint.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
typedef uint32_t u32;
#define REG_GENMASK(h,l) ((uint32_t)((UINT32_MAX >> (31-(h))) & (UINT32_MAX << (l))))
#define REG_FIELD_GET(mask,v) (((v) & (mask)) >> __builtin_ctz(mask))
#define _MMIO_TRANS2(dev,t,r) ((r) + (t)*0x1000)
#define DISPLAY_VER(d) 13
#define DRM_MODE_FLAG_INTERLACE 1
#define transcoder_is_dsi(t) false
#define DP_MIN_HBLANK_CTL(t) 0
struct drm_display_mode { u32 crtc_hdisplay,crtc_htotal,crtc_hblank_start,crtc_hblank_end,crtc_hsync_start,crtc_hsync_end,crtc_vdisplay,crtc_vtotal,crtc_vblank_start,crtc_vblank_end,crtc_vsync_start,crtc_vsync_end,flags; };
struct intel_display { int unused; };
struct intel_crtc { struct intel_display *display; };
struct intel_crtc_state { int cpu_transcoder; struct { struct drm_display_mode adjusted_mode; } hw; bool interlaced; u32 set_context_latency,min_hblank; };
enum transcoder { TRANSCODER_A,TRANSCODER_B,TRANSCODER_C,TRANSCODER_D };
#define to_intel_display(c) ((c)->display)
#define intel_pipe_is_interlaced(c) ((c)->interlaced)
static u32 words[7], reads[7]; static int nreads, pipe;
static u32 intel_de_read(struct intel_display *d, u32 reg) {
    const u32 offsets[] = {0x60000,0x60004,0x60008,0x6000c,0x60010,0x60014,0x6007c};
    if (nreads >= 7) abort(); reads[nreads++] = reg;
    for (int i=0;i<7;i++) if (reg == offsets[i]+pipe*0x1000) return words[i];
    abort();
}
"#;
    let main = r#"
int main(void) {
    int interlaced;
    while (scanf("%d %d", &pipe, &interlaced)==2) {
        for(int i=0;i<7;i++) if (scanf("%u",&words[i])!=1) return 2;
        struct intel_display display={0}; struct intel_crtc crtc={.display=&display};
        struct intel_crtc_state state={.cpu_transcoder=pipe,.interlaced=interlaced}; nreads=0;
        intel_get_transcoder_timings(&crtc,&state);
        struct drm_display_mode *m=&state.hw.adjusted_mode;
        printf("%u %u %u %u %u %u %u %u %u %u %u %u %u",m->crtc_hdisplay,m->crtc_htotal,m->crtc_hblank_start,m->crtc_hblank_end,m->crtc_hsync_start,m->crtc_hsync_end,m->crtc_vdisplay,m->crtc_vtotal,m->crtc_vblank_start,m->crtc_vblank_end,m->crtc_vsync_start,m->crtc_vsync_end,state.set_context_latency);
        for(int i=0;i<nreads;i++) printf(" %u",reads[i]); puts("");
    }
}
"#;
    let oracle = support::compile(&format!("{prefix}\n{masks}\n{function}\n{main}"), "readout");
    let mut rows = Vec::new();
    let mut input = String::new();
    for pipe in [Pipe::A, Pipe::B, Pipe::C, Pipe::D] {
        for interlaced in [false, true] {
            for seed in 0..16u32 {
                let mut words = [0; 7];
                for (i, v) in words[..6].iter_mut().enumerate() {
                    *v = seed.wrapping_mul(0x10001).wrapping_mul(i as u32 + 1) ^ 0x0438077f;
                }
                words[6] = seed;
                input.push_str(&format!("{} {}", pipe.index(), u32::from(interlaced)));
                for word in words {
                    input.push_str(&format!(" {word}"));
                }
                input.push('\n');
                rows.push((pipe, interlaced, words));
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
    assert_eq!(output.lines().count(), rows.len());
    for (line, (pipe, interlaced, words)) in output.lines().zip(rows) {
        let model = Model {
            pipe,
            words,
            reads: RefCell::new(Vec::new()),
        };
        let t = intel_get_transcoder_timings(&model, pipe, interlaced).unwrap();
        let mut expected = vec![
            t.hdisplay,
            t.htotal,
            t.hblank_start,
            t.hblank_end,
            t.hsync_start,
            t.hsync_end,
            t.vdisplay,
            t.vtotal,
            t.vblank_start,
            t.vblank_end,
            t.vsync_start,
            t.vsync_end,
            t.set_context_latency,
        ];
        expected.extend(model.reads.borrow().iter());
        let actual: Vec<u32> = line
            .split_whitespace()
            .map(|n| n.parse().unwrap())
            .collect();
        assert_eq!(
            actual, expected,
            "pipe={pipe:?} interlace={interlaced} words={words:?}"
        );
    }
    println!("128 raw timing cases: all decoded fields and seven reads match compiled i915");
}
