// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Compare ADL-P DKL HDMI signal programming with extracted Linux 7.2.3 C.
mod support;

use std::{
    cell::RefCell,
    io::Write,
    process::{Command, Stdio},
};

use tk_intel_display::{
    Error, RegisterIo,
    dkl_phy::{DklIo, TcPort},
    tc::{TcIo, Wa16011342517, adlp_tc_dkl_hdmi_set_signal_levels},
};

#[derive(Clone, Copy, Debug)]
struct Case {
    clock: u32,
    level: u8,
    wa: bool,
    seed: u32,
}

type Event = (u32, u32, u32);

#[test]
#[ignore = "requires local Linux 7.2.3 and GCC; run explicitly"]
fn dkl_hdmi_signal_sequence_and_failure_prefixes_match_compiled_i915() {
    let root = support::reference();
    let ddi = support::read(&root, "intel_ddi.c");
    let dkl = support::read(&root, "intel_dkl_phy.c");
    let regs = support::read(&root, "intel_dkl_phy_regs.h");
    let trans_source = support::read(&root, "intel_ddi_buf_trans.c");
    let macros = support::defines(
        &regs,
        &[
            "_DKL_PHY1_BASE",
            "_DKL_PHY2_BASE",
            "DKL_REG_TC_PORT",
            "DKL_REG_MMIO",
            "_DKL_REG_PHY_BASE",
            "_DKL_BANK_SHIFT",
            "_DKL_REG_BANK_OFFSET",
            "_DKL_REG_BANK_IDX",
            "_DKL_REG",
            "_DKL_REG_LN",
            "_DKL_TX_DPCNTL0_LN0",
            "_DKL_TX_DPCNTL0_LN1",
            "DKL_TX_DPCNTL0",
            "DKL_TX_PRESHOOT_COEFF",
            "DKL_TX_PRESHOOT_COEFF_MASK",
            "DKL_TX_DE_EMPHASIS_COEFF",
            "DKL_TX_DE_EMPAHSIS_COEFF_MASK",
            "DKL_TX_VSWING_CONTROL",
            "DKL_TX_VSWING_CONTROL_MASK",
            "_DKL_TX_DPCNTL1_LN0",
            "_DKL_TX_DPCNTL1_LN1",
            "DKL_TX_DPCNTL1",
            "_DKL_TX_DPCNTL2_LN0",
            "_DKL_TX_DPCNTL2_LN1",
            "DKL_TX_DPCNTL2",
            "DKL_TX_DP20BITMODE",
            "DKL_TX_DPCNTL2_CFG_LOADGENSELECT_TX1_MASK",
            "DKL_TX_DPCNTL2_CFG_LOADGENSELECT_TX1",
            "DKL_TX_DPCNTL2_CFG_LOADGENSELECT_TX2_MASK",
            "DKL_TX_DPCNTL2_CFG_LOADGENSELECT_TX2",
            "LOADGEN_SHARING_PMD_DISABLE",
            "_DKL_TX_PMD_LANE_SUS_LN0",
            "_DKL_TX_PMD_LANE_SUS_LN1",
            "DKL_TX_PMD_LANE_SUS",
            "_HIP_INDEX_REG0",
            "_HIP_INDEX_REG1",
            "HIP_INDEX_REG",
            "_HIP_INDEX_SHIFT",
            "HIP_INDEX_VAL",
        ],
    );

    let table = initializer(
        &trans_source,
        "static const union intel_ddi_buf_trans_entry _tgl_dkl_phy_trans_hdmi[]",
    );
    let functions = [
        support::function(&dkl, "static void\ndkl_phy_set_hip_idx("),
        support::function(&dkl, "void\nintel_dkl_phy_write("),
        support::function(&dkl, "void\nintel_dkl_phy_rmw("),
        support::function(&ddi, "static int intel_ddi_hdmi_level("),
        support::function(&ddi, "int intel_ddi_level("),
        support::function(&ddi, "static void tgl_dkl_phy_set_signal_levels("),
    ]
    .join("\n");

    let prefix = r#"
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
typedef uint8_t u8; typedef uint32_t u32;
#define REG_BIT(b) (1U << (b))
#define REG_GENMASK(h,l) ((u32)((UINT32_MAX >> (31-(h))) & (UINT32_MAX << (l))))
#define REG_FIELD_PREP(mask,val) (((val) << __builtin_ctz(mask)) & (mask))
#define _PORT(p,a,b) ((a) + (p) * ((b) - (a)))
#define _MMIO(r) (r)
#define I915_MAX_TC_PORTS 4
#define INTEL_OUTPUT_HDMI 1
#define INTEL_DISPLAY_WA_16011342517 0
#define enc_to_intel_dp(encoder) (encoder)
#define enc_to_dig_port(encoder) (encoder)
#define drm_WARN_ON_ONCE(d,b) (b)
#define drm_WARN_ON(d,b) (b)
enum tc_port { TC_PORT_1, TC_PORT_2, TC_PORT_3, TC_PORT_4 };
struct intel_dkl_phy_reg { u32 reg:24; u32 bank_idx:4; };
struct intel_display { int drm; struct { int phy_lock; } dkl; bool wa; struct { bool alderlake_p; } platform; };
struct intel_crtc_state { int port_clock; bool hdmi; };
struct intel_encoder { struct intel_display *display; void *devdata; enum tc_port port; };
struct intel_ddi_buf_trans_dkl { u8 vswing, preshoot, de_emphasis; };
union intel_ddi_buf_trans_entry { struct intel_ddi_buf_trans_dkl dkl; };
struct intel_ddi_buf_trans { const union intel_ddi_buf_trans_entry *entries; int num_entries, hdmi_default_entry; };
static u32 selector, seed, chosen_level; static int event_count; static u32 events[64][3];
static u32 values[4][16][1024]; static bool valid[4][16][1024];
static bool locked;
static void spin_lock(int *lock) { (void)lock; if (locked) abort(); locked=true; }
static void spin_unlock(int *lock) { (void)lock; if (!locked) abort(); locked=false; }
static void record(u32 op,u32 reg,u32 val) { if(event_count >= 64) abort(); events[event_count][0]=op;events[event_count][1]=reg;events[event_count++][2]=val; }
static u32 read_dkl(u32 reg) {
    u32 port=(reg-0x168000)/0x1000, low=(reg-0x168000)&0xfff;
    if (port >= 4) abort();
    u32 bank=(selector>>(8*port))&15;
    u32 val=valid[port][bank][low/4] ? values[port][bank][low/4] : ((reg*0x10001U) ^ (bank<<24) ^ seed) | 0x201U;
    record(0,reg,val); return val;
}
static void intel_de_write(struct intel_display *display,u32 reg,u32 val) {
    (void)display;
    if(!locked) abort();
    if(reg==0x1010a0) { selector=val; return; }
    u32 port=(reg-0x168000)/0x1000, low=(reg-0x168000)&0xfff, bank=(selector>>(8*port))&15;
    if(port>=4) abort();
    valid[port][bank][low/4]=true; values[port][bank][low/4]=val; record(1,reg,val);
}
static u32 intel_de_read(struct intel_display *display,u32 reg) { (void)display; if(reg==0x1010a0) return selector; if(!locked) abort(); return read_dkl(reg); }
static void intel_de_rmw(struct intel_display *display,u32 reg,u32 clear,u32 set) { u32 old=intel_de_read(display,reg); intel_de_write(display,reg,(old&~clear)|set); }
static struct intel_display *to_intel_display(struct intel_encoder *encoder) { return encoder->display; }
static enum tc_port intel_encoder_to_tc(struct intel_encoder *encoder) { return encoder->port; }
static bool intel_tc_port_in_tbt_alt_mode(struct intel_encoder *encoder) { (void)encoder; return false; }
static bool intel_display_wa(struct intel_display *display,int wa) { (void)wa; return display->wa; }
static bool intel_encoder_is_hdmi(struct intel_encoder *encoder) { (void)encoder; return true; }
static bool intel_encoder_is_dp(struct intel_encoder *encoder) { (void)encoder; return false; }
static bool intel_crtc_has_type(const struct intel_crtc_state *state,int type) { return type==INTEL_OUTPUT_HDMI && state->hdmi; }
static int intel_bios_hdmi_level_shift(void *devdata) { (void)devdata; return (int)chosen_level; }
static int intel_ddi_dp_level(void *encoder,const struct intel_crtc_state *state,int lane) { (void)encoder;(void)state;(void)lane;abort(); }
static struct intel_ddi_buf_trans trans;
static const struct intel_ddi_buf_trans *intel_ddi_buf_trans_get(struct intel_encoder *encoder,const struct intel_crtc_state *state,int *n) { (void)encoder;(void)state;*n=trans.num_entries;return &trans; }
static u32 read_dkl_for_output(u32 internal) {
    u32 bank=(internal>>12)&15, low=internal&0xfff, reg=0x168000+low;
    return valid[0][bank][low/4] ? values[0][bank][low/4] : ((reg*0x10001U)^(bank<<24)^seed)|0x201U;
}
"#;

    let main = r#"
int main(void) {
    u32 clock, level, wa, initial_seed;
    while (scanf("%u %u %u %u", &clock, &level, &wa, &initial_seed)==4) {
        seed=initial_seed; chosen_level=level; selector=0x03020100; event_count=0; locked=false;
        for(int p=0;p<4;p++) for(int b=0;b<16;b++) for(int i=0;i<1024;i++) valid[p][b][i]=false;
        struct intel_display display={.wa=wa != 0,.platform={.alderlake_p=true}};
        struct intel_encoder encoder={.display=&display,.port=TC_PORT_1};
        struct intel_crtc_state crtc={.port_clock=(int)clock,.hdmi=true};
        trans=(struct intel_ddi_buf_trans){.entries=_tgl_dkl_phy_trans_hdmi,.num_entries=(int)(sizeof(_tgl_dkl_phy_trans_hdmi)/sizeof(_tgl_dkl_phy_trans_hdmi[0])),.hdmi_default_entry=9};
        tgl_dkl_phy_set_signal_levels(&encoder,&crtc);
        printf("%d",event_count);
        for(int i=0;i<event_count;i++) printf(" %u %u %u",events[i][0],events[i][1],events[i][2]);
        u32 words[8]={read_dkl_for_output(0x0d00),read_dkl_for_output(0x2c0),read_dkl_for_output(0x2c4),read_dkl_for_output(0x2c8),read_dkl_for_output(0x1d00),read_dkl_for_output(0x12c0),read_dkl_for_output(0x12c4),read_dkl_for_output(0x12c8)};
        for(int i=0;i<8;i++) printf(" %u",words[i]);
        puts("");
        if(locked) abort();
    }
}
"#;

    let code = [prefix, &macros, &table, &functions, main].join("\n");
    let oracle = support::compile(&code, "tc-signal");

    let cases = [
        Case {
            clock: 148_500,
            level: 5,
            wa: true,
            seed: 0x51a7_c0de,
        },
        Case {
            clock: 297_000,
            level: 5,
            wa: true,
            seed: 0x2345_6789,
        },
        Case {
            clock: 148_500,
            level: 5,
            wa: false,
            seed: 0x7654_3210,
        },
        // Preserve Linux's surprising `set=1` at 594 MHz literally.
        Case {
            clock: 594_000,
            level: 5,
            wa: true,
            seed: 0x89ab_cdef,
        },
    ];
    let mut input = String::new();
    for case in cases {
        input.push_str(&format!(
            "{} {} {} {}\n",
            case.clock,
            case.level,
            u8::from(case.wa),
            case.seed
        ));
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
    let lines: Vec<_> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(lines.len(), cases.len());

    for (case, line) in cases.into_iter().zip(lines) {
        let actual: Vec<u32> = line
            .split_whitespace()
            .map(|value| value.parse().unwrap())
            .collect();
        let (c_trace, c_words) = split_output(&actual);
        let model = SignalModel::new(case.seed, None);
        adlp_tc_dkl_hdmi_set_signal_levels(
            &model,
            TcPort::Tc1,
            case.clock,
            case.level,
            if case.wa {
                Wa16011342517::Active
            } else {
                Wa16011342517::Inactive
            },
        )
        .unwrap();
        assert_eq!(model.events(), c_trace, "C DKL event trace: {case:?}");
        assert_eq!(model.words(), c_words, "C final DKL words: {case:?}");
        assert_eq!(model.selector(), 0x0302_0100);

        // The compiled upstream C sequence is the successful reference. A
        // Rust backend error at each successive DKL read/write must report an
        // exact attempted-event prefix and still restore the shared selector.
        for fail_at in 0..c_trace.len() {
            let failed = SignalModel::new(case.seed, Some(fail_at));
            assert!(
                adlp_tc_dkl_hdmi_set_signal_levels(
                    &failed,
                    TcPort::Tc1,
                    case.clock,
                    case.level,
                    if case.wa {
                        Wa16011342517::Active
                    } else {
                        Wa16011342517::Inactive
                    },
                )
                .is_err(),
                "fault index {fail_at}: {case:?}"
            );
            assert_eq!(
                failed.events(),
                c_trace[..=fail_at],
                "fault prefix {fail_at}: {case:?}"
            );
            assert_eq!(
                failed.selector(),
                0x0302_0100,
                "selector restore at {fail_at}: {case:?}"
            );
        }
    }
    println!("4 upstream HDMI cases and each injected DKL access-failure prefix match Linux 7.2.3");
}

fn initializer(source: &str, signature: &str) -> String {
    let start = source
        .find(signature)
        .expect("upstream initializer changed");
    let end = start + source[start..].find("};").expect("initializer terminator") + 2;
    source[start..end].to_owned()
}

fn split_output(values: &[u32]) -> (Vec<Event>, Vec<u32>) {
    let count = values[0] as usize;
    assert_eq!(values.len(), 1 + count * 3 + 8);
    let trace = (0..count)
        .map(|index| {
            let start = 1 + index * 3;
            (values[start], values[start + 1], values[start + 2])
        })
        .collect();
    (trace, values[1 + count * 3..].to_vec())
}

struct ModelState {
    mmio: std::collections::BTreeMap<u32, u32>,
    dkl: std::collections::BTreeMap<(u32, u32), u32>,
    events: Vec<Event>,
    dkl_event_index: usize,
    fail_at: Option<usize>,
    seed: u32,
}

struct SignalModel {
    state: RefCell<ModelState>,
}

impl SignalModel {
    fn new(seed: u32, fail_at: Option<usize>) -> Self {
        Self {
            state: RefCell::new(ModelState {
                mmio: [
                    (0x1010a0, 0x0302_0100),
                    (0x161500, 1 << 2),
                    (0x64300, 1 << 6),
                ]
                .into_iter()
                .collect(),
                dkl: std::collections::BTreeMap::new(),
                events: Vec::new(),
                dkl_event_index: 0,
                fail_at,
                seed,
            }),
        }
    }

    fn decode(offset: u32) -> Option<(u32, u32)> {
        let relative = offset.checked_sub(0x168000)?;
        let port = relative / 0x1000;
        (port < 4).then_some((port, relative & 0xfff))
    }

    fn default_value(offset: u32, bank: u32, seed: u32) -> u32 {
        offset.wrapping_mul(0x10001) ^ (bank << 24) ^ seed | 0x201
    }

    fn events(&self) -> Vec<Event> {
        self.state.borrow().events.clone()
    }

    fn selector(&self) -> u32 {
        *self.state.borrow().mmio.get(&0x1010a0).unwrap()
    }

    fn words(&self) -> Vec<u32> {
        let state = self.state.borrow();
        [0x0d00, 0x2c0, 0x2c4, 0x2c8, 0x1d00, 0x12c0, 0x12c4, 0x12c8]
            .into_iter()
            .map(|internal| {
                let bank = internal >> 12;
                let low = internal & 0xfff;
                *state
                    .dkl
                    .get(&(0, internal))
                    .unwrap_or(&Self::default_value(0x168000 + low, bank, state.seed))
            })
            .collect()
    }
}

impl RegisterIo for SignalModel {
    fn read32(&self, offset: u32) -> Result<u32, Error> {
        let Some((port, low)) = Self::decode(offset) else {
            return self
                .state
                .borrow()
                .mmio
                .get(&offset)
                .copied()
                .ok_or(Error::Unavailable(offset));
        };
        let mut state = self.state.borrow_mut();
        let selector = state.mmio[&0x1010a0];
        let bank = (selector >> (8 * port)) & 15;
        let seed = state.seed;
        let internal = (bank << 12) | low;
        let value = *state
            .dkl
            .entry((port, internal))
            .or_insert_with(|| Self::default_value(offset, bank, seed));
        let index = state.dkl_event_index;
        state.dkl_event_index += 1;
        state.events.push((0, offset, value));
        if state.fail_at == Some(index) {
            return Err(Error::Unavailable(offset));
        }
        Ok(value)
    }

    fn write32(&self, offset: u32, value: u32) -> Result<(), Error> {
        let Some((port, low)) = Self::decode(offset) else {
            self.state.borrow_mut().mmio.insert(offset, value);
            return Ok(());
        };
        let mut state = self.state.borrow_mut();
        let selector = state.mmio[&0x1010a0];
        let bank = (selector >> (8 * port)) & 15;
        let internal = (bank << 12) | low;
        let index = state.dkl_event_index;
        state.dkl_event_index += 1;
        state.events.push((1, offset, value));
        if state.fail_at == Some(index) {
            return Err(Error::Unavailable(offset));
        }
        state.dkl.insert((port, internal), value);
        Ok(())
    }
}

impl DklIo for SignalModel {
    fn with_dkl_lock<T>(&self, operation: impl FnOnce() -> Result<T, Error>) -> Result<T, Error> {
        operation()
    }
}

impl TcIo for SignalModel {
    fn display_core_powered(&self) -> bool {
        true
    }
    fn tc_port_powered(&self, _port: TcPort) -> bool {
        true
    }
    fn tc_cold_blocked(&self, _port: TcPort) -> bool {
        true
    }
}
