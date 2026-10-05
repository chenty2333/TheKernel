// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
mod support;
use std::{
    cell::{Cell, RefCell},
    io::Write,
    process::{Command, Stdio},
};

use tk_intel_display::{
    Error, RegisterIo,
    dkl_phy::{DklIo, TcPort},
    tc::*,
};
struct Model {
    core: bool,
    power: bool,
    cold: bool,
    status: u32,
    buf: u32,
    selector: Cell<u32>,
    locked: Cell<bool>,
    fault: Cell<Option<usize>>,
    accesses: Cell<usize>,
    reads: RefCell<Vec<u32>>,
    writes: RefCell<Vec<(u32, u32)>>,
}
fn model(status: u32, buf: u32) -> Model {
    Model {
        core: true,
        power: true,
        cold: true,
        status,
        buf,
        selector: Cell::new(0x03020100),
        locked: Cell::new(false),
        fault: Cell::new(None),
        accesses: Cell::new(0),
        reads: RefCell::new(Vec::new()),
        writes: RefCell::new(Vec::new()),
    }
}
impl Model {
    fn fail(&self, r: u32) -> Result<(), Error> {
        let n = self.accesses.get();
        self.accesses.set(n + 1);
        if self.fault.get() == Some(n) {
            self.fault.set(None);
            Err(Error::Unavailable(r))
        } else {
            Ok(())
        }
    }
}
impl RegisterIo for Model {
    fn read32(&self, r: u32) -> Result<u32, Error> {
        self.fail(r)?;
        self.reads.borrow_mut().push(r);
        if (0x161500..=0x16150c).contains(&r) {
            return Ok(self.status);
        }
        if (0x64300..=0x64600).contains(&r) {
            return Ok(self.buf);
        }
        if r == 0x1010a0 {
            assert!(self.locked.get());
            return Ok(self.selector.get());
        }
        if r == 0x163880 || r == 0x16e880 {
            return Ok(0x65);
        }
        if r == 0x1638a0 || r == 0x16e8a0 {
            return Ok(0x0f03);
        }
        assert!(self.locked.get());
        let p = (r - 0x168000) / 0x1000;
        let bank = (self.selector.get() >> (8 * p)) & 15;
        Ok(r ^ bank << 24)
    }
    fn write32(&self, r: u32, v: u32) -> Result<(), Error> {
        assert!(self.locked.get());
        assert_eq!(r, 0x1010a0);
        self.selector.set(v);
        self.writes.borrow_mut().push((r, v));
        self.fail(r)
    }
}
impl DklIo for Model {
    fn with_dkl_lock<T>(&self, f: impl FnOnce() -> Result<T, Error>) -> Result<T, Error> {
        assert!(!self.locked.replace(true));
        let result = f();
        self.locked.set(false);
        result
    }
}
impl TcIo for Model {
    fn display_core_powered(&self) -> bool {
        self.core
    }
    fn tc_port_powered(&self, _: TcPort) -> bool {
        self.power
    }
    fn tc_cold_blocked(&self, _: TcPort) -> bool {
        self.cold
    }
}
#[test]
fn cold_unowned_and_unpowered_ports_never_access_dkl() {
    for (status, buf) in [(0, 1 << 6), (u32::MAX, u32::MAX), (1 << 2, 0)] {
        let m = model(status, buf);
        assert_eq!(read_dkl_phy_state(&m, TcPort::Tc1), Err(Error::Refused));
        assert!(m.writes.borrow().is_empty());
    }
    for n in 0..3 {
        let mut m = model(4, 64);
        match n {
            0 => m.core = false,
            1 => m.power = false,
            _ => m.cold = false,
        };
        assert_eq!(read_dkl_phy_state(&m, TcPort::Tc1), Err(Error::Refused));
        assert!(m.reads.borrow().is_empty());
    }
}
#[test]
fn modular_fia_is_two_ports_per_instance_and_not_tcss_pin_bits() {
    for (p, n) in [TcPort::Tc1, TcPort::Tc2, TcPort::Tc3, TcPort::Tc4]
        .into_iter()
        .zip(0..4u32)
    {
        let m = model(0x1e000004, 64);
        let f = read_fia_state(&m, p).unwrap();
        let idx = n % 2;
        assert_eq!(f.pin_assignment, if idx == 0 { 5 } else { 6 });
        assert_eq!(f.lane_mask, if idx == 0 { 3 } else { 15 });
        let base = if n < 2 { 0x163000 } else { 0x16e000 };
        assert_eq!(*m.reads.borrow(), [base + 0x880, base + 0x8a0]);
        assert_eq!(status_register(p), 0x161500 + n * 4);
        assert_eq!(buffer_register(p), 0x64300 + n * 0x100);
    }
}
#[test]
fn phy_readout_banks_restore_at_every_fallible_prefix() {
    for p in [TcPort::Tc1, TcPort::Tc2, TcPort::Tc3, TcPort::Tc4] {
        let good = model(4, 64);
        let s = read_dkl_phy_state(&good, p).unwrap();
        let total = good.accesses.get();
        assert_eq!(total, 43);
        assert_eq!(s.uc_dw27, (0x16836c + p.index() * 0x1000) ^ (2 << 24));
        assert_eq!(
            s.lanes[1].lane_suspend,
            (0x168d00 + p.index() * 0x1000) ^ (1 << 24)
        );
        assert_eq!(good.selector.get(), 0x03020100);
        for fault in 0..total {
            let m = model(4, 64);
            m.fault.set(Some(fault));
            let result = read_dkl_phy_state(&m, p);
            assert!(result.is_err(), "{p:?} fault={fault}");
            assert_eq!(m.selector.get(), 0x03020100);
            assert!(!m.locked.get());
            if fault >= 41 {
                assert_eq!(result, Err(Error::RestoreFailed(0x1010a0)));
            }
        }
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
fn tc_ownership_fia_helpers_and_read_offsets_match_compiled_i915() {
    let root = support::reference();
    let src = support::read(&root, "intel_tc.c");
    let regs = ["intel_display_regs.h", "intel_mg_phy_regs.h"]
        .into_iter()
        .map(|f| all_defines(&support::read(&root, f)))
        .collect::<Vec<_>>()
        .join("\n");
    let fns = [
        "static void tc_phy_load_fia_params(",
        "static bool adlp_tc_phy_is_ready(",
        "static bool adlp_tc_phy_is_owned(",
        "static bool tc_phy_owned_by_display(",
        "static enum intel_tc_pin_assignment\nget_pin_assignment(",
        "static u32 get_lane_mask(",
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
typedef uint32_t u32;typedef uint8_t u8;typedef uint32_t intel_reg_t;
#define REG_BIT(b) (1U<<(b))
#define REG_GENMASK(h,l) ((uint32_t)((UINT32_MAX>>(31-(h))) & (UINT32_MAX<<(l))))
#define _MMIO(r) (r)
#define _PICK_EVEN(p,a,b) ((a)+(p)*((b)-(a)))
#define _PICK_EVEN_2RANGES(p,b,a0,a1,c0,c1) ((p)<(b)?_PICK_EVEN(p,a0,a1):_PICK_EVEN((p)-(b),c0,c1))
#define _MMIO_PORT(p,a,b) _PICK_EVEN(p,a,b)
#define DISPLAY_VER(d) 13
#define drm_WARN_ON(...) 0
#define drm_dbg_kms(...) do {} while(0)
#define MISSING_CASE(...) do {} while(0)
#define POWER_DOMAIN_DISPLAY_CORE 0
#define with_intel_display_power(d,p) for(int once=1;once;once=0)
#define ffs(v) __builtin_ffs(v)
enum tc_port {TC_PORT_1,TC_PORT_2,TC_PORT_3,TC_PORT_4};
enum port {PORT_A,PORT_B,PORT_C,PORT_D,PORT_E,PORT_F,PORT_G};
enum tc_port_mode {TC_PORT_DISCONNECTED,TC_PORT_TBT_ALT,TC_PORT_DP_ALT,TC_PORT_LEGACY};
enum intel_tc_pin_assignment {INTEL_TC_PIN_ASSIGNMENT_NONE,INTEL_TC_PIN_ASSIGNMENT_A,INTEL_TC_PIN_ASSIGNMENT_B,INTEL_TC_PIN_ASSIGNMENT_C,INTEL_TC_PIN_ASSIGNMENT_D,INTEL_TC_PIN_ASSIGNMENT_E,INTEL_TC_PIN_ASSIGNMENT_F};
#define FIA1 0
struct intel_display {int drm;};static struct intel_display model_display;
#define to_intel_display(p) (&model_display)
struct intel_encoder {enum port port;enum tc_port tc;};
struct intel_digital_port {struct intel_encoder base;};
struct intel_tc_port {struct intel_digital_port *dig_port;int phy_fia,phy_fia_idx;char *port_name;enum tc_port_mode mode;};
static enum tc_port intel_encoder_to_tc(struct intel_encoder *e) {return e->tc;}
static void assert_display_core_power_enabled(struct intel_tc_port *tc) {}
static void assert_tc_port_power_enabled(struct intel_tc_port *tc) {}
static void assert_tc_cold_blocked(struct intel_tc_port *tc) {}
static u32 status,buf,reads[4];static int count;
static u32 intel_de_read(struct intel_display *d,u32 r) {
    if(count>=4) abort();reads[count++]=r;
    if(r>=0x161500 && r<=0x16150c) return status;if(r>=0x64300 && r<=0x64600) return buf;
    if(r==0x163880 || r==0x16e880) return 0x65;if(r==0x1638a0 || r==0x16e8a0) return 0x0f03;abort();
}
"#;
    let main = r#"
int main(void) {
    int p;
    while(scanf("%d %u %u",&p,&status,&buf)==3) {
        struct intel_digital_port dp={.base={.port=PORT_D+p,.tc=p}};struct intel_tc_port tc={.dig_port=&dp,.mode=TC_PORT_LEGACY};count=0;
        tc_phy_load_fia_params(&tc,true);bool ready=adlp_tc_phy_is_ready(&tc),owned=adlp_tc_phy_is_owned(&tc);
        unsigned pin=get_pin_assignment(&tc),lanes=get_lane_mask(&tc);
        printf("%u %u %u %u %u ",ready,owned,tc_phy_owned_by_display(&tc,ready,owned),pin,lanes);
        for(int i=0;i<count;i++) printf("%u ",reads[i]);puts("");
    }
}
"#;
    let code = [prefix, &regs, &fns, main].join("\n");
    let oracle = support::compile(&code, "tc");
    let mut input = String::new();
    let mut cases = Vec::new();
    for p in [TcPort::Tc1, TcPort::Tc2, TcPort::Tc3, TcPort::Tc4] {
        for status in [0, 4, 0x1e000004, u32::MAX] {
            for buf in [0, 64, u32::MAX] {
                input.push_str(&format!("{} {status} {buf}\n", p.index()));
                cases.push((p, status, buf));
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
    for (line, (p, status, buf)) in output.lines().zip(cases) {
        let m = model(status, buf);
        let ready = adlp_tc_phy_is_ready(&m, p).unwrap();
        let owned = adlp_tc_phy_is_owned(&m, p).unwrap();
        let fia = read_fia_state(&m, p).unwrap();
        let mut expected = vec![
            u32::from(ready),
            u32::from(owned),
            u32::from(tc_phy_owned_by_display(ready, owned)),
            u32::from(fia.pin_assignment),
            u32::from(fia.lane_mask),
        ];
        expected.extend(m.reads.borrow().iter());
        let actual: Vec<u32> = line
            .split_whitespace()
            .map(|n| n.parse().unwrap())
            .collect();
        assert_eq!(actual, expected, "{p:?} status={status:x} buf={buf:x}");
    }
    println!("48 TC readiness/ownership/modular-FIA cases and read offsets match compiled i915");
}
