// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! HDMI audio data/sequence oracles compile unmodified Linux 7.2.3 C helpers.
mod support;

use std::{
    cell::RefCell,
    collections::BTreeMap,
    io::Write,
    process::{Command, Stdio},
};

use tk_intel_display::{
    Error, RegisterIo,
    audio::{AudioIo, Session, build_eld},
    display::Pipe,
};

const AUD_PIN_BUF_CTL: u32 = 0x48414;
const HSW_AUD_CFG_A: u32 = 0x65000;
const HSW_AUD_M_CTS_ENABLE_A: u32 = 0x65028;
const HSW_AUD_PIN_ELD_CP_VLD: u32 = 0x650c0;
const AUDIO_OUTPUT_ENABLE_A: u32 = 1 << 2;
const AUDIO_ELD_VALID_A: u32 = 1;
const AUD_CONFIG_PIXEL_CLOCK_HDMI_MASK: u32 = 0x000f_0000;
const AUD_CONFIG_N_MASK: u32 = 0x0ff0_fff0;
const AUD_CONFIG_N_PROG_ENABLE: u32 = 1 << 28;
const AUD_CONFIG_N_VALUE_INDEX: u32 = 1 << 29;
const AUD_M_CTS_OWNED_MASK: u32 = (1 << 21) | (1 << 20);

#[derive(Clone, Copy)]
enum Event {
    Pin(u32),
    Vblank(u8),
}

struct AudioModel {
    regs: RefCell<BTreeMap<u32, u32>>,
    events: RefCell<Vec<Event>>,
}

impl AudioModel {
    fn new(config: u32, m_cts: u32, pin: u32) -> Self {
        Self {
            regs: RefCell::new(BTreeMap::from([
                (AUD_PIN_BUF_CTL, 0),
                (HSW_AUD_CFG_A, config),
                (HSW_AUD_M_CTS_ENABLE_A, m_cts),
                (HSW_AUD_PIN_ELD_CP_VLD, pin),
            ])),
            events: RefCell::new(Vec::new()),
        }
    }

    fn bytes(&self, offset: u32) -> u32 {
        self.regs.borrow().get(&offset).copied().unwrap_or(0)
    }
}

impl RegisterIo for AudioModel {
    fn read32(&self, offset: u32) -> Result<u32, Error> {
        self.regs
            .borrow()
            .get(&offset)
            .copied()
            .ok_or(Error::Unavailable(offset))
    }

    fn write32(&self, offset: u32, value: u32) -> Result<(), Error> {
        self.regs.borrow_mut().insert(offset, value);
        if offset == HSW_AUD_PIN_ELD_CP_VLD {
            self.events.borrow_mut().push(Event::Pin(value));
        }
        Ok(())
    }
}

impl AudioIo for AudioModel {
    fn audio_power_held(&self, pipe: Pipe) -> Result<(), Error> {
        (pipe == Pipe::A).then_some(()).ok_or(Error::Refused)
    }

    fn wait_vblanks(&self, pipe: Pipe, count: u8) -> Result<(), Error> {
        if pipe != Pipe::A || count == 0 {
            return Err(Error::Refused);
        }
        self.events.borrow_mut().push(Event::Vblank(count));
        Ok(())
    }
}

fn c_audio_oracle(root: &std::path::Path) -> support::Oracle {
    let source = support::read(root, "intel_audio.c");
    let registers = support::read(root, "intel_audio_regs.h");
    let selected = support::defines(
        &registers,
        &[
            "_HSW_AUD_CONFIG_A",
            "_HSW_AUD_CONFIG_B",
            "HSW_AUD_CFG",
            "AUD_CONFIG_N_VALUE_INDEX",
            "AUD_CONFIG_N_PROG_ENABLE",
            "AUD_CONFIG_UPPER_N_MASK",
            "AUD_CONFIG_LOWER_N_MASK",
            "AUD_CONFIG_N_MASK",
            "AUD_CONFIG_N",
            "AUD_CONFIG_PIXEL_CLOCK_HDMI_MASK",
            "AUD_CONFIG_PIXEL_CLOCK_HDMI_25175",
            "AUD_CONFIG_PIXEL_CLOCK_HDMI_25200",
            "AUD_CONFIG_PIXEL_CLOCK_HDMI_27000",
            "AUD_CONFIG_PIXEL_CLOCK_HDMI_27027",
            "AUD_CONFIG_PIXEL_CLOCK_HDMI_54000",
            "AUD_CONFIG_PIXEL_CLOCK_HDMI_54054",
            "AUD_CONFIG_PIXEL_CLOCK_HDMI_74176",
            "AUD_CONFIG_PIXEL_CLOCK_HDMI_74250",
            "AUD_CONFIG_PIXEL_CLOCK_HDMI_148352",
            "AUD_CONFIG_PIXEL_CLOCK_HDMI_148500",
            "AUD_CONFIG_PIXEL_CLOCK_HDMI_296703",
            "AUD_CONFIG_PIXEL_CLOCK_HDMI_297000",
            "AUD_CONFIG_PIXEL_CLOCK_HDMI_593407",
            "AUD_CONFIG_PIXEL_CLOCK_HDMI_594000",
            "_HSW_AUD_M_CTS_ENABLE_A",
            "_HSW_AUD_M_CTS_ENABLE_B",
            "HSW_AUD_M_CTS_ENABLE",
            "AUD_M_CTS_M_VALUE_INDEX",
            "AUD_M_CTS_M_PROG_ENABLE",
            "HSW_AUD_PIN_ELD_CP_VLD",
            "AUDIO_OUTPUT_ENABLE",
            "AUDIO_ELD_VALID",
            "AUD_CHICKENBIT_REG3",
            "DACBE_DISABLE_MIN_HBLANK_FIX",
        ],
    );
    let declarations = [
        "struct hdmi_aud_ncts",
        "static const struct {\n\tint clock;\n\tu32 config;\n} hdmi_audio_clock[]",
        "static const struct hdmi_aud_ncts hdmi_aud_ncts_24bpp[]",
        "static const struct hdmi_aud_ncts hdmi_aud_ncts_30bpp[]",
        "static const struct hdmi_aud_ncts hdmi_aud_ncts_36bpp[]",
    ]
    .map(|signature| declaration(&source, signature))
    .join("\n");
    let hdmi_ncts_constants = support::defines(
        &source,
        &[
            "TMDS_297M",
            "TMDS_296M",
            "TMDS_594M",
            "TMDS_593M",
            "TMDS_371M",
            "TMDS_370M",
            "TMDS_445_5M",
            "TMDS_445M",
        ],
    );
    let functions = [
        "static u32 audio_config_hdmi_pixel_clock(",
        "static int audio_config_hdmi_get_n(",
        "static void\nhsw_hdmi_audio_config_update(",
        "static void\nhsw_audio_config_update(",
        "static void hsw_audio_codec_disable(",
        "static void hsw_audio_codec_enable(",
    ]
    .map(|signature| support::function(&source, signature))
    .join("\n");
    let prefix = r#"
#include <stdint.h>
#include <stdbool.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>
typedef uint8_t u8; typedef uint32_t u32;
#define ARRAY_SIZE(a) (sizeof(a) / sizeof((a)[0]))
#define REG_BIT(n) (1u << (n))
#define REG_GENMASK(h,l) ((uint32_t)((UINT32_MAX >> (31-(h))) & (UINT32_MAX << (l))))
#define REG_FIELD_PREP(m,v) ((((u32)(v)) << __builtin_ctz(m)) & (m))
#define _MMIO(r) (r)
#define _MMIO_TRANS(t,a,b) ((t) ? (b) : (a))
#define DISPLAY_VER(d) 13
#define INTEL_OUTPUT_DP 1
#define INTEL_DISPLAY_WA_14020863754 1
#define to_intel_display(c) ((c)->display)
#define to_intel_crtc(c) ((struct intel_crtc *)(c))
#define intel_crtc_has_type(s,t) false
#define intel_crtc_has_dp_encoder(s) false
#define drm_dbg_kms(...) ((void)0)
#define mutex_lock(p) ((void)0)
#define mutex_unlock(p) ((void)0)
#define intel_display_wa(d,w) false
struct drm_display_mode { int crtc_clock; };
struct i915_audio_component { int aud_sample_rate[8]; };
struct intel_audio_state { struct i915_audio_component *component; int mutex; };
struct intel_display { struct intel_audio_state audio; int drm; };
struct intel_crtc_state {
    struct intel_display *display;
    struct { struct drm_display_mode adjusted_mode; } hw;
    int port_clock, pipe_bpp, cpu_transcoder;
    struct { void *crtc; } uapi;
};
struct intel_encoder { struct intel_display *display; int port; };
struct drm_connector_state { int unused; };
struct intel_crtc { int unused; };
enum transcoder { TRANSCODER_A, TRANSCODER_B };
enum port { PORT_A, PORT_B, PORT_C, PORT_D };
struct intel_audio_event { unsigned kind, value; };
"#;
    let helpers = r#"
static u32 cfg, m_cts, pin;
static struct intel_audio_event events[8];
static unsigned event_count;
static void record(unsigned kind, u32 value) {
    if (event_count >= ARRAY_SIZE(events)) abort();
    events[event_count++] = (struct intel_audio_event){kind, value};
}
static u32 intel_de_read(struct intel_display *d, u32 reg) {
    if (reg == HSW_AUD_CFG(0)) return cfg;
    if (reg == HSW_AUD_M_CTS_ENABLE(0)) return m_cts;
    if (reg == HSW_AUD_PIN_ELD_CP_VLD) return pin;
    abort();
}
static void intel_de_write(struct intel_display *d, u32 reg, u32 value) {
    if (reg == HSW_AUD_CFG(0)) cfg = value;
    else if (reg == HSW_AUD_M_CTS_ENABLE(0)) m_cts = value;
    else if (reg == HSW_AUD_PIN_ELD_CP_VLD) { pin = value; record(1, value); }
    else abort();
}
static void intel_de_rmw(struct intel_display *d, u32 reg, u32 clear, u32 set) {
    intel_de_write(d, reg, (intel_de_read(d, reg) & ~clear) | set);
}
static void intel_crtc_wait_for_next_vblank(struct intel_crtc *crtc) { record(2, 1); }
static void intel_audio_sdp_split_update(const struct intel_crtc_state *s, bool enable) { }
static void enable_audio_dsc_wa(struct intel_encoder *e, const struct intel_crtc_state *s) { }
static void hsw_dp_audio_config_update(struct intel_encoder *e, const struct intel_crtc_state *s) { }
"#;
    let main = r#"
static void run_case(int disable, int clock, int rate, u32 c, u32 m, u32 p) {
    struct intel_display display = {0};
    struct i915_audio_component component = {0};
    component.aud_sample_rate[3] = rate;
    display.audio.component = &component;
    struct intel_crtc crtc = {0};
    struct intel_crtc_state state = {0};
    state.display = &display; state.hw.adjusted_mode.crtc_clock = clock;
    state.port_clock = clock; state.pipe_bpp = 24; state.cpu_transcoder = 0;
    state.uapi.crtc = &crtc;
    struct intel_encoder encoder = {.display=&display, .port=3};
    struct drm_connector_state conn = {0};
    cfg=c; m_cts=m; pin=p; event_count=0;
    if (disable) hsw_audio_codec_disable(&encoder, &state, &conn);
    else hsw_audio_codec_enable(&encoder, &state, &conn);
    printf("%u %u %u %u", cfg, m_cts, pin, event_count);
    for (unsigned i=0; i<event_count; i++) printf(" %u %u",events[i].kind,events[i].value);
    puts("");
}
int main(void) {
    int disable,clock,rate; u32 c,m,p;
    while (scanf("%d %d %d %u %u %u",&disable,&clock,&rate,&c,&m,&p)==6)
        run_case(disable,clock,rate,c,m,p);
    return 0;
}
"#;
    let code = [
        prefix,
        &selected,
        &hdmi_ncts_constants,
        &declarations,
        helpers,
        &functions,
        main,
    ]
    .join("\n");
    support::compile(&code, "audio")
}

fn declaration(source: &str, signature: &str) -> String {
    let start = source
        .find(signature)
        .expect("upstream declaration changed");
    let mut braces = 0usize;
    for (offset, byte) in source[start..].char_indices() {
        match byte {
            '{' => braces += 1,
            '}' => braces -= 1,
            ';' if braces == 0 => return source[start..=start + offset].to_owned(),
            _ => {}
        }
    }
    panic!("unterminated upstream declaration")
}

fn normalize_events(events: impl Iterator<Item = Event>) -> Vec<(u8, u32)> {
    let mut normalized = Vec::new();
    for event in events {
        match event {
            Event::Pin(value) => normalized.push((1, value)),
            Event::Vblank(count) => {
                if normalized.last().is_some_and(|(kind, _)| *kind == 2) {
                    normalized.last_mut().unwrap().1 += u32::from(count);
                } else {
                    normalized.push((2, u32::from(count)));
                }
            }
        }
    }
    normalized
}

fn parse_audio_line(line: &str) -> (u32, u32, u32, Vec<(u8, u32)>) {
    let mut fields = line
        .split_whitespace()
        .map(|field| field.parse::<u32>().unwrap());
    let config = fields.next().unwrap();
    let m_cts = fields.next().unwrap();
    let pin = fields.next().unwrap();
    let event_count = fields.next().unwrap() as usize;
    let raw_events = (0..event_count)
        .map(|_| (fields.next().unwrap() as u8, fields.next().unwrap()))
        .collect::<Vec<_>>();
    assert!(fields.next().is_none());
    let mut events = Vec::<(u8, u32)>::new();
    for (kind, value) in raw_events {
        if kind == 2 && events.last().is_some_and(|(previous, _)| *previous == 2) {
            events.last_mut().unwrap().1 += value;
        } else {
            events.push((kind, value));
        }
    }
    (config, m_cts, pin, events)
}

#[test]
#[ignore = "requires local Linux 7.2.3 and GCC; run explicitly"]
fn hdmi_audio_clock_n_cts_and_pin_sequence_match_compiled_i915() {
    let root = support::reference();
    let oracle = c_audio_oracle(&root);
    let clocks = [
        25_175, 25_200, 27_000, 27_027, 54_000, 54_054, 74_176, 74_250, 148_352, 148_500, 296_703,
        297_000, 593_407, 594_000,
    ];
    let base_config = 0x8000_0001;
    let base_m_cts = 0x0050_0000;
    let mut input = String::new();
    for clock in clocks {
        for disable in [false, true] {
            input.push_str(&format!(
                "{} {} 48000 {} {} {}\n",
                u8::from(disable),
                clock,
                base_config,
                base_m_cts,
                if disable { 0x44 } else { 0x40 }
            ));
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
    let results = output.lines().map(parse_audio_line).collect::<Vec<_>>();
    assert_eq!(results.len(), clocks.len() * 2);

    for (index, clock) in clocks.into_iter().enumerate() {
        let (
            upstream_enable_cfg,
            upstream_enable_m_cts,
            upstream_enable_pin,
            upstream_enable_events,
        ) = &results[index * 2];
        let (_, _, upstream_disable_pin, upstream_disable_events) = &results[index * 2 + 1];
        let model = AudioModel::new(base_config, base_m_cts, 0x40);
        let eld = test_eld();
        let mut session = Session::enable(&model, Pipe::A, clock, &eld).unwrap();
        let rust_enable_events = normalize_events(model.events.borrow().iter().copied());
        assert_eq!(
            model.bytes(HSW_AUD_CFG_A) & AUD_CONFIG_PIXEL_CLOCK_HDMI_MASK,
            *upstream_enable_cfg & AUD_CONFIG_PIXEL_CLOCK_HDMI_MASK,
            "pixel clock field at Linux case {index}"
        );
        assert_eq!(
            model.bytes(HSW_AUD_CFG_A)
                & (AUD_CONFIG_N_MASK | AUD_CONFIG_N_PROG_ENABLE | AUD_CONFIG_N_VALUE_INDEX),
            *upstream_enable_cfg
                & (AUD_CONFIG_N_MASK | AUD_CONFIG_N_PROG_ENABLE | AUD_CONFIG_N_VALUE_INDEX),
            "N programming at Linux case {index}"
        );
        assert_eq!(
            model.bytes(HSW_AUD_M_CTS_ENABLE_A) & AUD_M_CTS_OWNED_MASK,
            *upstream_enable_m_cts & AUD_M_CTS_OWNED_MASK,
            "M/CTS automatic mode at Linux case {index}"
        );
        assert_eq!(model.bytes(HSW_AUD_PIN_ELD_CP_VLD), *upstream_enable_pin);
        assert_eq!(&rust_enable_events, upstream_enable_events);

        model.events.borrow_mut().clear();
        session.disable(&model).unwrap();
        let rust_disable_events = normalize_events(model.events.borrow().iter().copied());
        assert_eq!(
            &rust_disable_events, upstream_disable_events,
            "disable pin/vblank trace at Linux clock case {index}"
        );
        assert_eq!(
            model.bytes(HSW_AUD_PIN_ELD_CP_VLD),
            *upstream_disable_pin,
            "disable pin readback at Linux clock case {index}"
        );
    }
    println!(
        "{} display-13 HDMI pixel clock/N/CTS and enable/disable pin-sequence pairs match \
         compiled i915",
        results.len() / 2
    );
}

fn test_eld() -> tk_intel_display::audio::Eld {
    build_eld(&edid_with_sads(
        3,
        &[0x09, 0x04, 0x01],
        0x4c,
        0x2d,
        0x34,
        0x12,
        1,
    ))
    .unwrap()
}

fn edid_with_sads(
    cta_revision: u8,
    sads: &[u8],
    mfg0: u8,
    mfg1: u8,
    product0: u8,
    product1: u8,
    speaker_allocation: u8,
) -> Vec<u8> {
    assert_eq!(sads.len() % 3, 0);
    let mut edid = vec![0; 256];
    edid[..8].copy_from_slice(&[0, 255, 255, 255, 255, 255, 255, 0]);
    edid[8..12].copy_from_slice(&[mfg0, mfg1, product0, product1]);
    edid[126] = 1;
    edid[127] = checksum(&edid[..127]);

    let cta = &mut edid[128..];
    cta[0] = 2;
    cta[1] = cta_revision;
    cta[3] = 0x40;
    let mut pos = 4usize;
    for block in sads.chunks(30) {
        cta[pos] = (1 << 5) | block.len() as u8;
        pos += 1;
        cta[pos..pos + block.len()].copy_from_slice(block);
        pos += block.len();
    }
    cta[pos] = (4 << 5) | 1;
    cta[pos + 1] = speaker_allocation;
    pos += 2;
    cta[2] = pos as u8;
    cta[127] = checksum(&cta[..127]);
    edid
}

fn checksum(bytes: &[u8]) -> u8 {
    0u8.wrapping_sub(bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)))
}

#[test]
#[ignore = "requires local Linux 7.2.3 and GCC; run explicitly"]
fn drm_eld_bytes_match_unmodified_linux_builder() {
    let root = support::reference();
    let source = std::fs::read_to_string(root.join("drivers/gpu/drm/drm_edid.c")).unwrap();
    let header = std::fs::read_to_string(root.join("include/drm/drm_eld.h")).unwrap();
    let functions = [
        support::function(&header, "static inline int drm_eld_mnl("),
        support::function(&header, "static inline int drm_eld_sad_count("),
        support::function(
            &header,
            "static inline int drm_eld_calc_baseline_block_size(",
        ),
        support::function(&source, "static void drm_edid_to_eld("),
    ]
    .join("\n");
    let selected = support::defines(
        &header,
        &[
            "DRM_ELD_HEADER_BLOCK_SIZE",
            "DRM_ELD_VER",
            "DRM_ELD_VER_CEA861D",
            "DRM_ELD_BASELINE_ELD_LEN",
            "DRM_ELD_CEA_EDID_VER_MNL",
            "DRM_ELD_CEA_EDID_VER_SHIFT",
            "DRM_ELD_SAD_COUNT_CONN_TYPE",
            "DRM_ELD_SAD_COUNT_SHIFT",
            "DRM_ELD_CONN_TYPE_HDMI",
            "DRM_ELD_CONN_TYPE_DP",
            "DRM_ELD_SPEAKER",
            "DRM_ELD_MANUFACTURER_NAME0",
            "DRM_ELD_MANUFACTURER_NAME1",
            "DRM_ELD_PRODUCT_CODE0",
            "DRM_ELD_PRODUCT_CODE1",
            "DRM_ELD_MONITOR_NAME_STRING",
            "DRM_ELD_CEA_SAD",
            "DRM_ELD_MNL_MASK",
            "DRM_ELD_MNL_SHIFT",
            "DRM_ELD_SAD_COUNT_MASK",
        ],
    );
    let prefix = r#"
#include <stdint.h>
#include <stdbool.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>
typedef uint8_t u8;
#define DIV_ROUND_UP(n,d) (((n)+(d)-1)/(d))
#define min(a,b) ((a)<(b)?(a):(b))
#define DRM_MODE_CONNECTOR_DisplayPort 1
#define DRM_MODE_CONNECTOR_eDP 2
#define CTA_DB_AUDIO 1
#define CTA_DB_VENDOR 3
#define CTA_DB_SPEAKER 4
#define mutex_lock(p) ((void)0)
#define mutex_unlock(p) ((void)0)
#define drm_dbg_kms(...) ((void)0)
struct drm_device { int unused; };
struct drm_display_info { u8 cea_rev; };
struct drm_connector {
    struct drm_display_info display_info;
    u8 eld[128];
    int connector_type, eld_mutex, id;
    const char *name;
    struct drm_device *dev;
};
struct edid { u8 mfg_id[2], prod_code[2]; };
struct drm_edid { struct edid *edid; };
struct cea_db { int tag, len; u8 data[45]; };
struct cea_db_iter { struct cea_db *dbs; int count; };
static struct cea_db dbs[3]; static int db_count;
static void cea_db_iter_edid_begin(const struct drm_edid *e, struct cea_db_iter *i) { i->dbs=dbs; i->count=db_count; }
#define cea_db_iter_for_each(db,iter) for (int cea_i=0; cea_i<(iter)->count && (((db)=&(iter)->dbs[cea_i]),1); cea_i++)
static void cea_db_iter_end(struct cea_db_iter *i) { }
static int cea_db_tag(const struct cea_db *db) { return db->tag; }
static int cea_db_payload_len(const struct cea_db *db) { return db->len; }
static const u8 *cea_db_data(const struct cea_db *db) { return db->data; }
static bool cea_db_is_hdmi_vsdb(const struct cea_db *db) { return false; }
static int get_monitor_name(const struct drm_edid *e, u8 *name) { return 0; }
static void drm_parse_hdmi_vsdb_audio(struct drm_connector *c, const u8 *db) { }
"#;
    let main = r#"
int main(void) {
    int revision, count, speaker; unsigned m0,m1,p0,p1; u8 sads[45];
    while (scanf("%d %d %d %u %u %u %u", &revision,&count,&speaker,&m0,&m1,&p0,&p1)==7) {
        if (count < 1 || count > 15) return 2;
        for (int i=0; i<count*3; i++) if (scanf("%hhu",&sads[i])!=1) return 3;
        db_count=0;
        int first=count>10?10:count;
        dbs[db_count].tag=CTA_DB_AUDIO; dbs[db_count].len=first*3;
        memcpy(dbs[db_count++].data,sads,(size_t)first*3);
        if (count>first) { dbs[db_count].tag=CTA_DB_AUDIO; dbs[db_count].len=(count-first)*3; memcpy(dbs[db_count++].data,sads+first*3,(size_t)(count-first)*3); }
        dbs[db_count].tag=CTA_DB_SPEAKER; dbs[db_count].len=1; dbs[db_count++].data[0]=(u8)speaker;
        struct edid base={{(u8)m0,(u8)m1},{(u8)p0,(u8)p1}};
        struct drm_edid edid={.edid=&base}; struct drm_connector connector={0};
        connector.display_info.cea_rev=(u8)revision; connector.connector_type=0; connector.name="HDMI-A-1";
        drm_edid_to_eld(&connector,&edid);
        unsigned len=DRM_ELD_HEADER_BLOCK_SIZE+connector.eld[DRM_ELD_BASELINE_ELD_LEN]*4;
        printf("%u",len); for(unsigned i=0;i<len;i++) printf(" %u",connector.eld[i]); puts("");
    }
}
"#;
    let oracle = support::compile(&format!("{prefix}\n{selected}\n{functions}\n{main}"), "eld");

    let sad_sets = [
        vec![0x09, 0x04, 0x01],
        vec![0x09, 0x04, 0x01, 0x17, 0x04, 0x00],
        (0..15)
            .flat_map(|index| {
                if index == 0 {
                    [0x09, 0x04, 0x01]
                } else {
                    [0x10 | (index as u8 & 7), 0x04, index as u8]
                }
            })
            .collect::<Vec<_>>(),
    ];
    let mut input = String::new();
    let mut edids = Vec::new();
    for (index, sads) in sad_sets.iter().enumerate() {
        let revision = [1u8, 3, 5][index];
        let speaker = [1u8, 0x0f, 0x7f][index];
        input.push_str(&format!(
            "{revision} {} {speaker} 76 45 52 18",
            sads.len() / 3
        ));
        for byte in sads {
            input.push_str(&format!(" {byte}"));
        }
        input.push('\n');
        edids.push(edid_with_sads(revision, sads, 76, 45, 52, 18, speaker));
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
    assert_eq!(output.lines().count(), edids.len());
    for (case, (line, edid)) in output.lines().zip(edids.iter()).enumerate() {
        let expected = build_eld(edid).unwrap();
        let actual = line
            .split_whitespace()
            .map(|byte| byte.parse::<u8>().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            actual[0] as usize,
            expected.as_bytes().len(),
            "ELD size case {case}"
        );
        assert_eq!(&actual[1..], expected.as_bytes(), "ELD bytes case {case}");
    }
    println!(
        "{} ELD byte strings match compiled drm_edid_to_eld",
        edids.len()
    );
}
