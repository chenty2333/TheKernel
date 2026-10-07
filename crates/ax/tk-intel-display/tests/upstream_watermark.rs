// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Compare the admitted display13 4K30 -> 1080p60 linear-XRGB WM profile to
//! unmodified Linux 7.2.3 `skl_compute_wm_params()` / `skl_compute_plane_wm()`.
mod support;

use std::{
    io::Write,
    process::{Command, Stdio},
};

use tk_intel_display::watermark::adlp_linear_xrgb_4k30_watermark_profile_no_worse;

fn fixed_helpers(source: &str) -> String {
    [
        "static inline uint_fixed_16_16_t clamp_u64_to_fixed16(",
        "static inline uint_fixed_16_16_t u32_to_fixed16(",
        "static inline u32 fixed16_to_u32_round_up(",
        "static inline uint_fixed_16_16_t min_fixed16(",
        "static inline uint_fixed_16_16_t max_fixed16(",
        "static inline u32 div_round_up_fixed16(",
        "static inline u32 mul_round_up_u32_fixed16(",
        "static inline uint_fixed_16_16_t div_fixed16(",
        "static inline uint_fixed_16_16_t mul_u32_fixed16(",
        "static inline uint_fixed_16_16_t add_fixed16_u32(",
    ]
    .map(|signature| support::function(source, signature))
    .join("\n")
}

fn source_oracle(root: &std::path::Path) -> support::Oracle {
    let source = support::read(root, "skl_watermark.c");
    let fixed_source =
        std::fs::read_to_string(root.join("drivers/gpu/drm/i915/display/intel_fixed.h")).unwrap();
    let fixed = fixed_helpers(&fixed_source);
    let functions = [
        "static int skl_wm_linetime_us(",
        "static int\nskl_compute_wm_params(",
        "static bool skl_wm_has_lines(",
        "static int skl_wm_max_lines(",
        "static bool xe3_auto_min_alloc_capable(",
        "static uint_fixed_16_16_t\nskl_wm_method1(",
        "static uint_fixed_16_16_t\nskl_wm_method2(",
        "static void skl_compute_plane_wm(",
    ];
    let functions = functions
        .into_iter()
        .map(|signature| {
            // The source has forward declarations for these two functions;
            // select the final occurrence containing the implementation.
            let start = source
                .rfind(signature)
                .expect("upstream implementation changed");
            support::function(&source[start..], signature)
        })
        .collect::<Vec<_>>();
    let functions = functions.join("\n");
    let prefix = r#"
#include <stdint.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <limits.h>
#include <errno.h>
typedef uint8_t u8; typedef uint16_t u16; typedef uint32_t u32; typedef uint64_t u64;
typedef struct { u32 val; } uint_fixed_16_16_t;
#define FP_16_16_MAX ((uint_fixed_16_16_t){ .val = UINT_MAX })
#define U16_MAX UINT16_MAX
#define DISPLAY_VER(d) 13
#define HAS_SAGV_WM(d) true
#define DIV_ROUND_UP(n,d) (((n) + (d) - 1) / (d))
#define DIV_ROUND_UP_ULL(n,d) (((n) + (d) - 1) / (d))
#define max(a,b) ((a) > (b) ? (a) : (b))
#define min(a,b) ((a) < (b) ? (a) : (b))
#define max_t(t,a,b) ((t)max((a),(b)))
#define mul_u32_u32(a,b) ((u64)(a) * (u64)(b))
#define WARN_ON(x) (0)
#define MISSING_CASE(x) abort()
#define drm_dbg_kms(...) ((void)0)
#define to_intel_display(p) ((p)->display)
#define drm_rotation_90_or_270(r) (((r) == 90) || ((r) == 270))
#define PLANE_CURSOR 5
#define I915_FORMAT_MOD_X_TILED 1ULL
#define I915_FORMAT_MOD_Yf_TILED 2ULL
struct intel_display { struct { u32 block_time_us; } sagv; };
struct intel_crtc_state {
    struct intel_display *display;
    struct { struct { u32 crtc_htotal; } pipe_mode; } hw;
};
struct intel_plane { struct intel_display *display; int id; };
struct drm_format_info { u8 cpp[4]; };
struct skl_wm_params {
    bool x_tiled, y_tiled, rc_surface;
    u32 width; u8 cpp; u32 plane_pixel_rate;
    u32 y_min_scanlines, plane_bytes_per_line;
    uint_fixed_16_16_t plane_blocks_per_line, y_tile_minimum;
    u32 linetime_us, dbuf_block_size;
};
struct skl_wm_level {
    u32 blocks, lines, min_ddb_alloc;
    bool enable, auto_min_alloc_wm_enable, can_sagv;
};
static bool intel_format_info_is_yuv_semiplanar(const struct drm_format_info *f, u64 m) { return false; }
static bool intel_fb_is_tiled_modifier(u64 m) { return m != 0; }
static bool intel_fb_is_ccs_modifier(u64 m) { return false; }
static bool skl_needs_memory_bw_wa(struct intel_display *d) { return false; }
static bool use_minimal_wm0_only(const struct intel_crtc_state *s, struct intel_plane *p) { return false; }
"#;
    let extras = r#"
int main(void) {
    unsigned int latency;
    while (scanf("%u", &latency) == 1) {
        struct intel_display display = {0};
        struct intel_crtc_state state = {
            .display = &display,
            .hw.pipe_mode.crtc_htotal = 4400,
        };
        struct intel_plane plane = {.display = &display, .id = 0};
        struct drm_format_info format = {.cpp = {4, 0, 0, 0}};
        struct skl_wm_params base = {0}, target = {0};
        struct skl_wm_level zero = {0}, base_wm = {0}, target_wm = {0};
        if (skl_compute_wm_params(&state, 3840, &format, 0, 0, 297000,
                                 &base, 0, 0))
            return 2;
        state.hw.pipe_mode.crtc_htotal = 2200;
        if (skl_compute_wm_params(&state, 1920, &format, 0, 0, 148500,
                                 &target, 0, 0))
            return 2;
        state.hw.pipe_mode.crtc_htotal = 4400;
        skl_compute_plane_wm(&state, &plane, 0, latency, &base, &zero, &base_wm);
        state.hw.pipe_mode.crtc_htotal = 2200;
        skl_compute_plane_wm(&state, &plane, 0, latency, &target, &zero, &target_wm);
        printf("%u %u %u %u %u %u %u %u %u %u\n",
               base.linetime_us, base.plane_blocks_per_line.val >> 16,
               base_wm.blocks, base_wm.lines, base_wm.min_ddb_alloc,
               target.linetime_us, target.plane_blocks_per_line.val >> 16,
               target_wm.blocks, target_wm.lines, target_wm.min_ddb_alloc);
    }
    return 0;
}
"#;
    support::compile(
        &[prefix, &fixed, &functions, extras].concat(),
        "skl-watermark-profile",
    )
}

#[test]
#[ignore = "requires local Linux 7.2.3 and GCC; run explicitly"]
fn upstream_adlp_wm_fields_are_no_worse_for_the_only_admitted_transition() {
    let root = support::reference();
    let oracle = source_oracle(&root);
    let mut child = Command::new(&oracle.executable)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut input = child.stdin.take().unwrap();
        for latency in 1..=258 {
            writeln!(input, "{latency}").unwrap();
        }
    }
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    let rows = text.lines().collect::<Vec<_>>();
    assert_eq!(rows.len(), 258);

    for (i, row) in rows.into_iter().enumerate() {
        let latency = i + 1;
        let values = row
            .split_whitespace()
            .map(|word| word.parse::<u32>().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(values.len(), 10);
        let [
            base_linetime,
            base_pbpl,
            base_blocks,
            base_lines,
            base_min_ddb,
            target_linetime,
            target_pbpl,
            target_blocks,
            target_lines,
            target_min_ddb,
        ] = values[..]
        else {
            unreachable!()
        };
        assert_eq!(
            (base_linetime, target_linetime),
            (15, 15),
            "latency={latency}"
        );
        assert_eq!((base_pbpl, target_pbpl), (31, 16), "latency={latency}");
        if [1, 14, 15, 258].contains(&latency) {
            eprintln!(
                "latency={latency}: base=({base_blocks} blocks,{base_lines} \
                 lines,min_ddb={base_min_ddb}) target=({target_blocks} blocks,{target_lines} \
                 lines,min_ddb={target_min_ddb})"
            );
        }
        assert!(
            target_blocks <= base_blocks
                && target_lines <= base_lines
                && target_min_ddb <= base_min_ddb,
            "upstream WM worsened at latency={latency}: \
             base=({base_blocks},{base_lines},{base_min_ddb}), \
             target=({target_blocks},{target_lines},{target_min_ddb})"
        );
        assert!(adlp_linear_xrgb_4k30_watermark_profile_no_worse(
            297_000, 3_840, 4_400, 148_500, 1_920, 2_200
        ));
    }
}
