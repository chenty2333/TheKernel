// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Compile initial-plane reconstruction, format, stride and tile helpers from i915.
mod support;
use std::{
    cell::RefCell,
    io::Write,
    process::{Command, Stdio},
};

use tk_intel_display::{
    Error, RegisterIo,
    display::{Pipe, ReadoutIo},
    universal_plane::*,
};
struct Model {
    pipe: Pipe,
    plane: Plane,
    words: [u32; 6],
    reads: RefCell<Vec<u32>>,
}
impl RegisterIo for Model {
    fn read32(&self, r: u32) -> Result<u32, Error> {
        self.reads.borrow_mut().push(r);
        [0x70180, 0x701cc, 0x7019c, 0x701a4, 0x70190, 0x70188]
            .iter()
            .position(|v| self.plane.register(self.pipe, *v) == r)
            .map(|i| self.words[i])
            .ok_or(Error::Unavailable(r))
    }
    fn write32(&self, _: u32, _: u32) -> Result<(), Error> {
        panic!("readout wrote")
    }
}
impl ReadoutIo for Model {
    fn pipe_powered(&self, p: Pipe) -> bool {
        self.pipe == p
    }
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
fn initial_plane_fields_and_read_order_match_compiled_i915() {
    let root = support::reference();
    let regs = all_defines(&support::read(&root, "skl_universal_plane_regs.h"));
    let fourcc =
        all_defines(&std::fs::read_to_string(root.join("include/uapi/drm/drm_fourcc.h")).unwrap());
    let src = support::read(&root, "skl_universal_plane.c");
    let format = support::function(&src, "int skl_format_to_fourcc(");
    let stride = support::function(&src, "static unsigned int skl_plane_stride_mult(");
    let initial = support::function(&src, "void\nskl_get_initial_plane_config(");
    let fb = support::read(&root, "intel_fb.c");
    let tiles = [
        "unsigned int intel_tile_size(",
        "unsigned int\nintel_tile_width_bytes(",
        "unsigned int intel_tile_height(",
        "unsigned int\nintel_fb_align_height(",
    ]
    .into_iter()
    .map(|s| support::function(&fb, s))
    .collect::<Vec<_>>()
    .join("\n");
    let prefix = r#"
#include <stdint.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
typedef uint32_t u32; typedef uint32_t __u32; typedef uint64_t u64; typedef uint64_t __u64;
#define REG_BIT(b) (1U<<(b))
#define REG_GENMASK(h,l) ((uint32_t)((UINT32_MAX>>(31-(h))) & (UINT32_MAX<<(l))))
#define REG_FIELD_GET(mask,v) (((v)&(mask))>>__builtin_ctz(mask))
#define REG_FIELD_PREP(mask,v) (((v)<<__builtin_ctz(mask))&(mask))
#define _PIPE(p,a,b) ((a)+(p)*((b)-(a)))
#define _PLANE(p,a,b) _PIPE(p,a,b)
#define _MMIO(r) (r)
#define DISPLAY_VER(d) 13
#define HAS_4TILE(d) false
#define HAS_128B_Y_TILING(d) true
#define DRM_MODE_ROTATE_0 (1U<<0)
#define DRM_MODE_ROTATE_90 (1U<<1)
#define DRM_MODE_ROTATE_180 (1U<<2)
#define DRM_MODE_ROTATE_270 (1U<<3)
#define DRM_MODE_REFLECT_X (1U<<4)
#define drm_rotation_90_or_270(r) ((r)&(DRM_MODE_ROTATE_90|DRM_MODE_ROTATE_270))
#define ALIGN(v,a) (((v)+(a)-1)&~((a)-1))
#define static_assert(e) _Static_assert(e, "")
#define drm_WARN_ON(d,e) (e)
#define drm_dbg_kms(...) do {} while(0)
#define MISSING_CASE(...) abort()
#define kfree(p) free(p)
#define fallthrough ((void)0)
enum plane_id {PLANE_1,PLANE_2,PLANE_3,PLANE_4,PLANE_5};
enum pipe {PIPE_A,PIPE_B,PIPE_C,PIPE_D};
struct intel_display {int drm;};
static struct intel_display model_display;
#define to_intel_display(p) (&model_display)
struct drm_format_info {unsigned int cpp[4],num_planes;};
struct drm_framebuffer {int dev;u64 modifier;struct drm_format_info *format;u32 width,height,pitches[4];};
struct intel_framebuffer {struct drm_framebuffer base;};
struct intel_crtc_state {int joiner_pipes;};
struct intel_plane {enum plane_id id;bool(*get_hw_state)(struct intel_plane*,enum pipe*);};
struct intel_crtc {enum pipe pipe;struct {struct intel_crtc_state *state;struct intel_plane *primary;struct {int id;} base;char *name;} base;};
#define to_intel_crtc_state(s) (s)
#define to_intel_plane(p) (p)
struct intel_initial_plane_config {struct drm_framebuffer *fb;u32 rotation,base,size;};
static struct intel_framebuffer *intel_framebuffer_alloc(void) {return calloc(1,sizeof(struct intel_framebuffer));}
static bool intel_fb_is_ccs_aux_plane(const struct drm_framebuffer *fb,int p) {return false;}
static bool is_gen12_ccs_cc_plane(const struct drm_framebuffer *fb,int p) {return false;}
static bool is_surface_linear(const struct drm_framebuffer *fb,int p) {return fb->modifier==0;}
static struct drm_format_info format_info;
static struct drm_format_info *drm_get_format_info(int dev,int format,u64 modifier);
static u32 words[6],reads[8];static int nreads,pipe_idx,plane_idx;
static u32 intel_de_read(struct intel_display *d,u32 reg) {
    const u32 regs[]={0x70180,0x701cc,0x7019c,0x701a4,0x70190,0x70188};
    if(nreads>=8) abort(); reads[nreads++]=reg;
    for(int i=0;i<6;i++) if(reg==regs[i]+pipe_idx*0x1000+plane_idx*0x100) return words[i];
    abort();
}
static bool get_hw_state(struct intel_plane *p,enum pipe *pipe) {
    *pipe=pipe_idx;return intel_de_read(&model_display,0x70180+pipe_idx*0x1000+plane_idx*0x100)&0x80000000;
}
"#;
    let main = r#"
static struct drm_format_info *drm_get_format_info(int dev,int format,u64 modifier) {
    format_info.cpp[0]=4; format_info.num_planes=1;
    switch(format) {
    case DRM_FORMAT_RGB565:format_info.cpp[0]=2;break;
    case DRM_FORMAT_NV12:format_info.cpp[0]=1;format_info.num_planes=2;break;
    case DRM_FORMAT_P010:case DRM_FORMAT_P012:case DRM_FORMAT_P016:format_info.cpp[0]=2;format_info.num_planes=2;break;
    case DRM_FORMAT_XRGB16161616F:case DRM_FORMAT_XBGR16161616F:case DRM_FORMAT_ARGB16161616F:case DRM_FORMAT_ABGR16161616F:
    case DRM_FORMAT_XVYU12_16161616:case DRM_FORMAT_XVYU16161616:format_info.cpp[0]=8;break;
    }
    return &format_info;
}
int main(void) {
    while(scanf("%d %d",&pipe_idx,&plane_idx)==2) {
        for(int i=0;i<6;i++) if(scanf("%u",&words[i])!=1) return 2;
        struct intel_crtc_state state={0};struct intel_plane plane={.id=plane_idx,.get_hw_state=get_hw_state};
        struct intel_crtc crtc={.pipe=pipe_idx,.base={.state=&state,.primary=&plane}};
        struct intel_initial_plane_config config={0}; nreads=0;
        skl_get_initial_plane_config(&crtc,&config);
        if(!config.fb) {puts("refused");continue;}
        struct drm_framebuffer *fb=config.fb;
        printf("%u %llu %u %u %u %u %u %u %u %u",fb->format ? words[0]:0,(unsigned long long)fb->modifier,
            skl_format_to_fourcc(words[0]&PLANE_CTL_FORMAT_MASK_ICL,words[0]&PLANE_CTL_ORDER_RGBX,
                REG_FIELD_GET(PLANE_COLOR_ALPHA_MASK,words[1])),
            fb->format->cpp[0],fb->format->num_planes,config.rotation,config.base,fb->width,fb->height,fb->pitches[0]);
        printf(" %u",config.size);for(int i=0;i<nreads;i++) printf(" %u",reads[i]);puts("");free(fb);
    }
}
"#;
    let code = [
        prefix, &regs, &fourcc, &format, &tiles, &stride, &initial, main,
    ]
    .join("\n");
    let oracle = support::compile(&code, "plane");
    let mut input = String::new();
    let mut cases = Vec::new();
    for format in 0..32u32 {
        for alpha in 0..4u32 {
            for rgb in 0..2u32 {
                for (tile, compress) in [
                    (0, 0),
                    (1, 0),
                    (4, 0),
                    (5, 0),
                    (4, 1 << 15),
                    (4, 1 << 4),
                    (5, 1 << 15),
                ] {
                    let pipe = [Pipe::A, Pipe::B, Pipe::C, Pipe::D][(format % 4) as usize];
                    let plane = Plane::new((format % 5) as u8).unwrap();
                    let rotation = if alpha % 2 != 0 { 2 | (1 << 8) } else { 0 };
                    let words = [
                        (1 << 31)
                            | (format << 23)
                            | (rgb << 20)
                            | (tile << 10)
                            | compress
                            | rotation,
                        (1 << 13) | (alpha << 4),
                        0x12345004,
                        0x12345678,
                        (1079 << 16) | 1919,
                        120,
                    ];
                    input.push_str(&format!("{} {}", pipe.index(), format % 5));
                    for v in words {
                        input.push_str(&format!(" {v}"));
                    }
                    input.push('\n');
                    cases.push((pipe, plane, words));
                }
            }
        }
    }
    let mut child = Command::new(&oracle.executable)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    // The larger trace input/output requires concurrent pipe drainage, not a
    // write-all-before-read deadlock once both pipe buffers fill.
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()).unwrap());
    let output = child.wait_with_output().unwrap();
    writer.join().unwrap();
    assert!(output.status.success());
    let output = String::from_utf8(output.stdout).unwrap();
    assert_eq!(output.lines().count(), cases.len());
    for (line, (pipe, plane, words)) in output.lines().zip(cases) {
        let model = Model {
            pipe,
            plane,
            words,
            reads: RefCell::new(Vec::new()),
        };
        let s = skl_get_initial_plane_config(&model, pipe, plane)
            .unwrap()
            .unwrap();
        let rotation = match s.rotation_degrees_ccw {
            0 => 1,
            180 => 4,
            _ => panic!(),
        } | if s.reflect_x { 16 } else { 0 };
        let mut expected = vec![
            u64::from(s.ctl),
            s.modifier.drm(),
            u64::from(s.fourcc),
            u64::from(s.cpp),
            u64::from(s.format_planes),
            rotation,
            u64::from(s.surface()),
            u64::from(s.width),
            u64::from(s.height),
            u64::from(s.pitch),
            s.main_size,
        ];
        expected.extend(model.reads.borrow().iter().map(|v| u64::from(*v)));
        let actual: Vec<u64> = line
            .split_whitespace()
            .map(|n| n.parse().unwrap())
            .collect();
        assert_eq!(
            actual, expected,
            "pipe={pipe:?} plane={plane:?} words={words:x?}"
        );
    }
    println!(
        "1792 display-13 plane format/modifier/rotation/layout/read-order cases match compiled \
         i915"
    );
}
